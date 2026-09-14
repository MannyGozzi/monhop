//! One-sided, authenticated layout metadata synchronization with no input authority.

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::Duration,
};

use monhop_core::RevocationSignal;
use monhop_protocol::{PROTOCOL_VERSION, SessionEpoch};
use sha2::{Digest, Sha256};

use crate::{
    crypto::CertificateFingerprint,
    session_handshake::{NegotiatedSession, SessionPurpose},
    session_native::current_displays,
    session_setup::{InspectedPeer, SetupFailure, connect_for_purpose},
    session_wire::WRITE_DEADLINE,
};

pub const MAX_LAYOUT_PAYLOAD_BYTES: usize = 32 * 1024;

const CONFIGURE_DEADLINE: Duration = Duration::from_secs(1);
const CONFIGURE_CLOSE_CODE: u32 = 4;
const CONFIGURE_CLOSE_REASON: &[u8] = b"layout synchronization incomplete";
const CONFIGURE_MAGIC: [u8; 4] = *b"LKLC";
const CONFIGURE_FORMAT_VERSION: u8 = 1;
const CONFIGURE_PURPOSE: u8 = 4;
const CONFIGURE_HEADER_LEN: usize = 64;
const ROLE_SEQUENCE: u64 = 3;
const PROPOSAL_SEQUENCE: u64 = 4;
const ACK_SEQUENCE: u64 = 5;
const COMPLETE_SEQUENCE: u64 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayoutKind {
    Role,
    Proposal,
    Acknowledgement,
    Complete,
}

impl LayoutKind {
    const fn wire(self) -> u8 {
        match self {
            Self::Role => 1,
            Self::Proposal => 2,
            Self::Acknowledgement => 3,
            Self::Complete => 4,
        }
    }

    fn from_wire(value: u8) -> Result<Self, SetupFailure> {
        match value {
            1 => Ok(Self::Role),
            2 => Ok(Self::Proposal),
            3 => Ok(Self::Acknowledgement),
            4 => Ok(Self::Complete),
            _ => Err(SetupFailure::LayoutSyncIncomplete),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayoutRole {
    Sender,
    Waiting,
}

impl LayoutRole {
    const fn wire(self) -> u8 {
        match self {
            Self::Sender => 1,
            Self::Waiting => 2,
        }
    }

    fn from_wire(value: u8) -> Result<Self, SetupFailure> {
        match value {
            1 => Ok(Self::Sender),
            2 => Ok(Self::Waiting),
            _ => Err(SetupFailure::LayoutSyncIncomplete),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyncDirection {
    Send,
    Receive,
}

struct LayoutFrame {
    kind: LayoutKind,
    role: Option<LayoutRole>,
    epoch: SessionEpoch,
    sequence: u64,
    digest: [u8; 32],
    payload: Vec<u8>,
}

/// Synchronizes exactly one locally reviewed layout after explicit local action.
/// The receiver validates and persists the peer proposal before it acknowledges the digest.
pub async fn synchronize_after_local_action<P, Fut>(
    interface_id: &str,
    peer: CertificateFingerprint,
    proposal: Option<Vec<u8>>,
    cancel: &RevocationSignal,
    abort_persist: impl Fn() + Send + Sync + 'static,
    persist: P,
) -> Result<(InspectedPeer, Vec<u8>), SetupFailure>
where
    P: FnOnce(InspectedPeer, Vec<u8>) -> Fut,
    Fut: Future<Output = Result<(), SetupFailure>>,
{
    validate_local_proposal(proposal.as_deref())?;
    check_cancel(cancel)?;
    let mut paired =
        connect_for_purpose(interface_id, None, cancel, peer, SessionPurpose::Configure).await?;
    let abort_persist: Arc<dyn Fn() + Send + Sync> = Arc::new(abort_persist);
    let deadline =
        ConfigureDeadline::start(paired.endpoint().revoker(), Arc::clone(&abort_persist))?;
    let (session, inspection, endpoint) = paired.parts();
    let result = tokio::time::timeout(
        CONFIGURE_DEADLINE,
        synchronize_inner(
            session,
            inspection,
            proposal,
            || check_active(cancel, endpoint),
            ensure_fresh_local_displays,
            persist,
        ),
    )
    .await;
    let payload = match result {
        Ok(Ok(payload)) if !deadline.expired() => payload,
        Ok(Ok(_)) => {
            abort_persist();
            paired
                .session
                .connection
                .close(CONFIGURE_CLOSE_CODE.into(), CONFIGURE_CLOSE_REASON);
            return Err(SetupFailure::LayoutSyncIncomplete);
        }
        Ok(Err(error)) => {
            abort_persist();
            paired
                .session
                .connection
                .close(CONFIGURE_CLOSE_CODE.into(), CONFIGURE_CLOSE_REASON);
            return Err(active_failure(
                error,
                cancel,
                paired.endpoint(),
                deadline.expired(),
            ));
        }
        Err(_) => {
            abort_persist();
            paired
                .session
                .connection
                .close(CONFIGURE_CLOSE_CODE.into(), CONFIGURE_CLOSE_REASON);
            return Err(active_failure(
                SetupFailure::LayoutSyncIncomplete,
                cancel,
                paired.endpoint(),
                deadline.expired(),
            ));
        }
    };
    if deadline.expired() {
        abort_persist();
        return Err(SetupFailure::LayoutSyncIncomplete);
    }
    check_active(cancel, paired.endpoint())?;
    paired.endpoint().close_and_wait_idle().await.map_err(|_| {
        if deadline.expired() {
            SetupFailure::LayoutSyncIncomplete
        } else if cancel.is_revoked() || paired.endpoint().is_revoked() {
            SetupFailure::Cancelled
        } else {
            SetupFailure::LayoutSyncIncomplete
        }
    })?;
    if deadline.expired() {
        abort_persist();
        return Err(SetupFailure::LayoutSyncIncomplete);
    }
    check_active(cancel, paired.endpoint())?;
    Ok((paired.inspection, payload))
}

struct ConfigureDeadline {
    stop: mpsc::Sender<()>,
    expired: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ConfigureDeadline {
    fn start(
        revoker: crate::guarded_endpoint::EndpointRevoker,
        abort_persist: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, SetupFailure> {
        Self::start_with(CONFIGURE_DEADLINE, abort_persist, move || revoker.revoke())
    }

    fn start_with(
        deadline: Duration,
        abort_persist: Arc<dyn Fn() + Send + Sync>,
        revoke_endpoint: impl FnOnce() + Send + 'static,
    ) -> Result<Self, SetupFailure> {
        let (stop, receive) = mpsc::channel();
        let expired = Arc::new(AtomicBool::new(false));
        let deadline_expired = Arc::clone(&expired);
        let worker = std::thread::Builder::new()
            .name("monhop-layout-deadline".into())
            .spawn(move || {
                if matches!(
                    receive.recv_timeout(deadline),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    deadline_expired.store(true, Ordering::Release);
                    abort_persist();
                    revoke_endpoint();
                }
            })
            .map_err(|_| SetupFailure::LayoutSyncIncomplete)?;
        Ok(Self {
            stop,
            expired,
            worker: Some(worker),
        })
    }

    fn expired(&self) -> bool {
        self.expired.load(Ordering::Acquire)
    }
}

impl Drop for ConfigureDeadline {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

async fn synchronize_inner<A, F, P, Fut>(
    session: &mut NegotiatedSession,
    inspection: &InspectedPeer,
    proposal: Option<Vec<u8>>,
    active: A,
    fresh: F,
    persist: P,
) -> Result<Vec<u8>, SetupFailure>
where
    A: Fn() -> Result<(), SetupFailure>,
    F: Fn(&InspectedPeer) -> Result<(), SetupFailure>,
    P: FnOnce(InspectedPeer, Vec<u8>) -> Fut,
    Fut: Future<Output = Result<(), SetupFailure>>,
{
    if session.purpose() != SessionPurpose::Configure {
        return Err(SetupFailure::Handshake);
    }
    if session.control.next_sequence() != ROLE_SEQUENCE {
        return Err(SetupFailure::LayoutSyncIncomplete);
    }
    active()?;
    let local_role = if proposal.is_some() {
        LayoutRole::Sender
    } else {
        LayoutRole::Waiting
    };
    let role = LayoutFrame {
        kind: LayoutKind::Role,
        role: Some(local_role),
        epoch: session.initial_epoch,
        sequence: ROLE_SEQUENCE,
        digest: [0; 32],
        payload: Vec::new(),
    };
    write_layout_frame(
        &session.connection,
        &mut session.control_streams.send,
        &role,
    )
    .await?;
    let remote_role = read_expected_frame(
        &mut session.control_streams.recv,
        session.initial_epoch,
        ROLE_SEQUENCE,
        LayoutKind::Role,
    )
    .await?
    .role
    .ok_or(SetupFailure::LayoutSyncIncomplete)?;

    match sync_direction(proposal.is_some(), remote_role)? {
        SyncDirection::Send => {
            sync_sender(
                session,
                inspection,
                proposal.ok_or(SetupFailure::LayoutSyncConflict)?,
                &active,
                &fresh,
                persist,
            )
            .await
        }
        SyncDirection::Receive => {
            sync_receiver(session, inspection, &active, &fresh, persist).await
        }
    }
}

async fn sync_sender<A, F, P, Fut>(
    session: &mut NegotiatedSession,
    inspection: &InspectedPeer,
    payload: Vec<u8>,
    active: &A,
    fresh: &F,
    persist: P,
) -> Result<Vec<u8>, SetupFailure>
where
    A: Fn() -> Result<(), SetupFailure>,
    F: Fn(&InspectedPeer) -> Result<(), SetupFailure>,
    P: FnOnce(InspectedPeer, Vec<u8>) -> Fut,
    Fut: Future<Output = Result<(), SetupFailure>>,
{
    fresh(inspection)?;
    active()?;
    let digest = digest(&payload);
    let proposal = LayoutFrame {
        kind: LayoutKind::Proposal,
        role: None,
        epoch: session.initial_epoch,
        sequence: PROPOSAL_SEQUENCE,
        digest,
        payload: payload.clone(),
    };
    write_layout_frame(
        &session.connection,
        &mut session.control_streams.send,
        &proposal,
    )
    .await?;
    let acknowledgement = read_expected_frame(
        &mut session.control_streams.recv,
        session.initial_epoch,
        ACK_SEQUENCE,
        LayoutKind::Acknowledgement,
    )
    .await?;
    acknowledgement_matches(Some(&acknowledgement), digest)?;
    active()?;
    fresh(inspection)?;
    persist(inspection.clone(), payload.clone())
        .await
        .map_err(|_| SetupFailure::LayoutSyncIncomplete)?;
    active()?;
    fresh(inspection)?;
    let complete = LayoutFrame {
        kind: LayoutKind::Complete,
        role: None,
        epoch: session.initial_epoch,
        sequence: COMPLETE_SEQUENCE,
        digest,
        payload: Vec::new(),
    };
    write_layout_frame(
        &session.connection,
        &mut session.control_streams.send,
        &complete,
    )
    .await?;
    finish_and_drain(session).await?;
    active()?;
    Ok(payload)
}

async fn sync_receiver<A, F, P, Fut>(
    session: &mut NegotiatedSession,
    inspection: &InspectedPeer,
    active: &A,
    fresh: &F,
    persist: P,
) -> Result<Vec<u8>, SetupFailure>
where
    A: Fn() -> Result<(), SetupFailure>,
    F: Fn(&InspectedPeer) -> Result<(), SetupFailure>,
    P: FnOnce(InspectedPeer, Vec<u8>) -> Fut,
    Fut: Future<Output = Result<(), SetupFailure>>,
{
    let proposal = read_expected_frame(
        &mut session.control_streams.recv,
        session.initial_epoch,
        PROPOSAL_SEQUENCE,
        LayoutKind::Proposal,
    )
    .await?;
    if proposal.digest != digest(&proposal.payload) {
        return Err(SetupFailure::LayoutSyncIncomplete);
    }
    active()?;
    fresh(inspection)?;
    persist(inspection.clone(), proposal.payload.clone()).await?;
    active()?;
    fresh(inspection)?;
    let acknowledgement = LayoutFrame {
        kind: LayoutKind::Acknowledgement,
        role: None,
        epoch: session.initial_epoch,
        sequence: ACK_SEQUENCE,
        digest: proposal.digest,
        payload: Vec::new(),
    };
    write_layout_frame(
        &session.connection,
        &mut session.control_streams.send,
        &acknowledgement,
    )
    .await?;
    let complete = read_expected_frame(
        &mut session.control_streams.recv,
        session.initial_epoch,
        COMPLETE_SEQUENCE,
        LayoutKind::Complete,
    )
    .await?;
    if complete.digest != proposal.digest || !complete.payload.is_empty() {
        return Err(SetupFailure::LayoutSyncIncomplete);
    }
    finish_and_drain(session).await?;
    active()?;
    Ok(proposal.payload)
}

async fn finish_and_drain(session: &mut NegotiatedSession) -> Result<(), SetupFailure> {
    session
        .control_streams
        .send
        .finish()
        .map_err(|_| SetupFailure::LayoutSyncIncomplete)?;
    session
        .control_streams
        .recv
        .read_to_end(0)
        .await
        .map_err(|_| SetupFailure::LayoutSyncIncomplete)?;
    if session
        .control_streams
        .send
        .stopped()
        .await
        .map_err(|_| SetupFailure::LayoutSyncIncomplete)?
        .is_some()
    {
        return Err(SetupFailure::LayoutSyncIncomplete);
    }
    Ok(())
}

async fn read_expected_frame(
    recv: &mut quinn::RecvStream,
    epoch: SessionEpoch,
    sequence: u64,
    kind: LayoutKind,
) -> Result<LayoutFrame, SetupFailure> {
    let frame = read_layout_frame(recv).await?;
    if frame.epoch != epoch || frame.sequence != sequence || frame.kind != kind {
        return Err(SetupFailure::LayoutSyncIncomplete);
    }
    Ok(frame)
}

async fn write_layout_frame(
    connection: &quinn::Connection,
    send: &mut quinn::SendStream,
    frame: &LayoutFrame,
) -> Result<(), SetupFailure> {
    let bytes = encode_layout_frame(frame)?;
    match tokio::time::timeout(WRITE_DEADLINE, send.write_all(&bytes)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) | Err(_) => {
            connection.close(CONFIGURE_CLOSE_CODE.into(), CONFIGURE_CLOSE_REASON);
            Err(SetupFailure::LayoutSyncIncomplete)
        }
    }
}

async fn read_layout_frame(recv: &mut quinn::RecvStream) -> Result<LayoutFrame, SetupFailure> {
    let mut header = [0_u8; CONFIGURE_HEADER_LEN];
    read_exact(recv, &mut header).await?;
    let payload_len = usize::try_from(u32::from_be_bytes([
        header[28], header[29], header[30], header[31],
    ]))
    .map_err(|_| SetupFailure::LayoutSyncIncomplete)?;
    if payload_len > MAX_LAYOUT_PAYLOAD_BYTES {
        return Err(SetupFailure::LayoutSyncIncomplete);
    }
    let mut payload = vec![0; payload_len];
    read_exact(recv, &mut payload).await?;
    decode_layout_frame(&header, payload)
}

async fn read_exact(recv: &mut quinn::RecvStream, bytes: &mut [u8]) -> Result<(), SetupFailure> {
    let mut filled = 0;
    while filled < bytes.len() {
        let read = recv
            .read(&mut bytes[filled..])
            .await
            .map_err(|_| SetupFailure::LayoutSyncIncomplete)?;
        let Some(read) = read else {
            return Err(SetupFailure::LayoutSyncIncomplete);
        };
        if read == 0 {
            return Err(SetupFailure::LayoutSyncIncomplete);
        }
        filled += read;
    }
    Ok(())
}

fn encode_layout_frame(frame: &LayoutFrame) -> Result<Vec<u8>, SetupFailure> {
    validate_layout_frame(frame)?;
    let payload_len = u32::try_from(frame.payload.len()).map_err(|_| SetupFailure::Layout)?;
    let mut bytes = Vec::with_capacity(CONFIGURE_HEADER_LEN + frame.payload.len());
    bytes.extend_from_slice(&CONFIGURE_MAGIC);
    bytes.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    bytes.push(CONFIGURE_FORMAT_VERSION);
    bytes.push(CONFIGURE_PURPOSE);
    bytes.push(frame.kind.wire());
    bytes.push(frame.role.map_or(0, LayoutRole::wire));
    bytes.extend_from_slice(&[0; 2]);
    bytes.extend_from_slice(&frame.epoch.get().to_be_bytes());
    bytes.extend_from_slice(&frame.sequence.to_be_bytes());
    bytes.extend_from_slice(&payload_len.to_be_bytes());
    bytes.extend_from_slice(&frame.digest);
    bytes.extend_from_slice(&frame.payload);
    Ok(bytes)
}

fn decode_layout_frame(
    header: &[u8; CONFIGURE_HEADER_LEN],
    payload: Vec<u8>,
) -> Result<LayoutFrame, SetupFailure> {
    if header[..4] != CONFIGURE_MAGIC
        || u16::from_be_bytes([header[4], header[5]]) != PROTOCOL_VERSION
        || header[6] != CONFIGURE_FORMAT_VERSION
        || header[7] != CONFIGURE_PURPOSE
        || header[10..12].iter().any(|byte| *byte != 0)
    {
        return Err(SetupFailure::LayoutSyncIncomplete);
    }
    let kind = LayoutKind::from_wire(header[8])?;
    let role = match header[9] {
        0 => None,
        value => Some(LayoutRole::from_wire(value)?),
    };
    let epoch = SessionEpoch::new(u64::from_be_bytes(
        header[12..20]
            .try_into()
            .map_err(|_| SetupFailure::LayoutSyncIncomplete)?,
    ))
    .map_err(|_| SetupFailure::LayoutSyncIncomplete)?;
    let sequence = u64::from_be_bytes(
        header[20..28]
            .try_into()
            .map_err(|_| SetupFailure::LayoutSyncIncomplete)?,
    );
    let payload_len = usize::try_from(u32::from_be_bytes(
        header[28..32]
            .try_into()
            .map_err(|_| SetupFailure::LayoutSyncIncomplete)?,
    ))
    .map_err(|_| SetupFailure::LayoutSyncIncomplete)?;
    if payload_len != payload.len() {
        return Err(SetupFailure::LayoutSyncIncomplete);
    }
    let mut digest = [0; 32];
    digest.copy_from_slice(&header[32..64]);
    let frame = LayoutFrame {
        kind,
        role,
        epoch,
        sequence,
        digest,
        payload,
    };
    validate_layout_frame(&frame)?;
    Ok(frame)
}

fn validate_layout_frame(frame: &LayoutFrame) -> Result<(), SetupFailure> {
    if frame.payload.len() > MAX_LAYOUT_PAYLOAD_BYTES {
        return Err(SetupFailure::LayoutSyncIncomplete);
    }
    match frame.kind {
        LayoutKind::Role => {
            if frame.role.is_none() || !frame.payload.is_empty() || frame.digest != [0; 32] {
                return Err(SetupFailure::LayoutSyncIncomplete);
            }
        }
        LayoutKind::Proposal => {
            if frame.role.is_some()
                || frame.payload.is_empty()
                || frame.digest != digest(&frame.payload)
            {
                return Err(SetupFailure::LayoutSyncIncomplete);
            }
        }
        LayoutKind::Acknowledgement | LayoutKind::Complete => {
            if frame.role.is_some() || !frame.payload.is_empty() {
                return Err(SetupFailure::LayoutSyncIncomplete);
            }
        }
    }
    Ok(())
}

fn validate_local_proposal(proposal: Option<&[u8]>) -> Result<(), SetupFailure> {
    if proposal
        .is_some_and(|payload| payload.is_empty() || payload.len() > MAX_LAYOUT_PAYLOAD_BYTES)
    {
        return Err(SetupFailure::Layout);
    }
    Ok(())
}

fn ensure_fresh_local_displays(inspection: &InspectedPeer) -> Result<(), SetupFailure> {
    let current = current_displays(inspection.local_device).map_err(|_| SetupFailure::Displays)?;
    if current.same_geometry(&inspection.local_displays) {
        Ok(())
    } else {
        Err(SetupFailure::ChangedSinceInspection)
    }
}

fn acknowledgement_matches(
    acknowledgement: Option<&LayoutFrame>,
    digest: [u8; 32],
) -> Result<(), SetupFailure> {
    let acknowledgement = acknowledgement.ok_or(SetupFailure::LayoutSyncIncomplete)?;
    if acknowledgement.digest == digest && acknowledgement.payload.is_empty() {
        Ok(())
    } else {
        Err(SetupFailure::LayoutSyncIncomplete)
    }
}

fn check_cancel(cancel: &RevocationSignal) -> Result<(), SetupFailure> {
    if cancel.is_revoked() {
        Err(SetupFailure::Cancelled)
    } else {
        Ok(())
    }
}

fn check_active(
    cancel: &RevocationSignal,
    endpoint: &crate::guarded_endpoint::GuardedEndpoint,
) -> Result<(), SetupFailure> {
    check_cancel(cancel)?;
    if endpoint.is_revoked() {
        Err(SetupFailure::Cancelled)
    } else {
        Ok(())
    }
}

fn active_failure(
    error: SetupFailure,
    cancel: &RevocationSignal,
    endpoint: &crate::guarded_endpoint::GuardedEndpoint,
    deadline_expired: bool,
) -> SetupFailure {
    if deadline_expired {
        SetupFailure::LayoutSyncIncomplete
    } else if cancel.is_revoked() || endpoint.is_revoked() {
        SetupFailure::Cancelled
    } else {
        error
    }
}

fn sync_direction(
    local_has_proposal: bool,
    remote_role: LayoutRole,
) -> Result<SyncDirection, SetupFailure> {
    match (local_has_proposal, remote_role) {
        (true, LayoutRole::Waiting) => Ok(SyncDirection::Send),
        (false, LayoutRole::Sender) => Ok(SyncDirection::Receive),
        _ => Err(SetupFailure::LayoutSyncConflict),
    }
}

fn digest(payload: &[u8]) -> [u8; 32] {
    Sha256::digest(payload).into()
}

#[cfg(test)]
mod tests {
    use std::{
        net::{Ipv4Addr, SocketAddr},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use super::*;
    use monhop_core::{DisplayId, Platform, Point, RevocationSignal};
    use monhop_protocol::{Capabilities, DisplayDescription, DisplayTopology};

    use crate::{
        crypto::{DeviceIdentity, LOCAL_TLS_SERVER_NAME, SecureQuicConfig, VerifiedPeer},
        session_handshake::{HandshakeConfig, device_id_from_fingerprint, negotiate},
    };

    fn frame(kind: LayoutKind, role: Option<LayoutRole>, payload: Vec<u8>) -> LayoutFrame {
        LayoutFrame {
            kind,
            role,
            epoch: SessionEpoch::new(7).unwrap(),
            sequence: ROLE_SEQUENCE,
            digest: if payload.is_empty() {
                [0; 32]
            } else {
                digest(&payload)
            },
            payload,
        }
    }

    fn topology(id: u64) -> DisplayTopology {
        DisplayTopology::new(vec![DisplayDescription {
            id: DisplayId(id),
            name: "fixture".into(),
            native_width: 100,
            native_height: 100,
            logical_origin: Point::default(),
            logical_size: Point::new(100.0, 100.0),
            scale_factor: 1.0,
            is_primary: true,
            monitor: None,
        }])
        .unwrap()
    }

    fn pin(identity: &DeviceIdentity) -> VerifiedPeer {
        VerifiedPeer::from_certificate_der(
            identity.certificate_der(),
            &identity.fingerprint().full_hex(),
        )
        .unwrap()
    }

    async fn configured_sessions() -> (
        quinn::Endpoint,
        quinn::Endpoint,
        NegotiatedSession,
        NegotiatedSession,
        InspectedPeer,
        InspectedPeer,
    ) {
        let client_identity = DeviceIdentity::generate().unwrap();
        let server_identity = DeviceIdentity::generate().unwrap();
        let server_pin = pin(&server_identity);
        let client_pin = pin(&client_identity);
        let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        let server = quinn::Endpoint::server(
            SecureQuicConfig::server(&server_identity, &client_pin).unwrap(),
            loopback,
        )
        .unwrap();
        let mut client = quinn::Endpoint::client(loopback).unwrap();
        client.set_default_client_config(
            SecureQuicConfig::client(&client_identity, &server_pin).unwrap(),
        );
        let connecting = client
            .connect(server.local_addr().unwrap(), LOCAL_TLS_SERVER_NAME)
            .unwrap();
        let (client_connection, server_connection) =
            tokio::join!(async { connecting.await.unwrap() }, async {
                server.accept().await.unwrap().await.unwrap()
            },);
        let client_topology = topology(1);
        let server_topology = topology(2);
        let client_device = device_id_from_fingerprint(client_identity.fingerprint());
        let server_device = device_id_from_fingerprint(server_identity.fingerprint());
        let capabilities =
            Capabilities::new(Capabilities::RELATIVE_MOTION | Capabilities::DISPLAY_TOPOLOGY)
                .unwrap();
        let client_config = HandshakeConfig::new(
            &client_identity,
            &server_pin,
            Platform::Windows,
            Platform::MacOs,
            capabilities,
            capabilities,
            &client_topology,
            client_device,
            SessionPurpose::Configure,
        )
        .unwrap();
        let server_config = HandshakeConfig::new(
            &server_identity,
            &client_pin,
            Platform::MacOs,
            Platform::Windows,
            capabilities,
            capabilities,
            &server_topology,
            client_device,
            SessionPurpose::Configure,
        )
        .unwrap();
        let (client_session, server_session) = tokio::join!(
            negotiate(client_connection, client_config),
            negotiate(server_connection, server_config),
        );
        let client_inspection = InspectedPeer {
            local_device: client_device,
            peer_device: server_device,
            source: client_device,
            local_fingerprint: client_identity.fingerprint(),
            peer_fingerprint: server_identity.fingerprint(),
            local_platform: Platform::Windows,
            peer_platform: Platform::MacOs,
            local_displays: client_topology,
            peer_displays: server_topology.clone(),
            interface_id: "fixture".into(),
        };
        let server_inspection = InspectedPeer {
            local_device: server_device,
            peer_device: client_device,
            source: client_device,
            local_fingerprint: server_identity.fingerprint(),
            peer_fingerprint: client_identity.fingerprint(),
            local_platform: Platform::MacOs,
            peer_platform: Platform::Windows,
            local_displays: server_topology,
            peer_displays: client_inspection.local_displays.clone(),
            interface_id: "fixture".into(),
        };
        (
            client,
            server,
            client_session.unwrap(),
            server_session.unwrap(),
            client_inspection,
            server_inspection,
        )
    }

    fn close(endpoints: (&quinn::Endpoint, &quinn::Endpoint)) {
        endpoints.0.close(0_u32.into(), b"fixture complete");
        endpoints.1.close(0_u32.into(), b"fixture complete");
    }

    #[test]
    fn configure_frame_round_trip_binds_version_purpose_epoch_and_digest() {
        let source = frame(
            LayoutKind::Proposal,
            None,
            vec![7; MAX_LAYOUT_PAYLOAD_BYTES],
        );
        let encoded = encode_layout_frame(&source).unwrap();
        let (header, payload) = encoded.split_at(CONFIGURE_HEADER_LEN);
        let header: [u8; CONFIGURE_HEADER_LEN] = header.try_into().unwrap();
        let decoded = decode_layout_frame(&header, payload.to_vec()).unwrap();
        assert_eq!(decoded.kind, LayoutKind::Proposal);
        assert_eq!(decoded.epoch, source.epoch);
        assert_eq!(decoded.payload, source.payload);
        assert_eq!(decoded.digest, source.digest);
    }

    #[test]
    fn configure_frame_rejects_wrong_version_purpose_digest_and_oversize_payload() {
        let source = frame(LayoutKind::Proposal, None, vec![1]);
        let encoded = encode_layout_frame(&source).unwrap();
        let (header, payload) = encoded.split_at(CONFIGURE_HEADER_LEN);
        let header: [u8; CONFIGURE_HEADER_LEN] = header.try_into().unwrap();
        for index in [4, 7, 32] {
            let mut invalid = header;
            invalid[index] ^= 1;
            assert!(matches!(
                decode_layout_frame(&invalid, payload.to_vec()),
                Err(SetupFailure::LayoutSyncIncomplete)
            ));
        }
        assert_eq!(
            validate_local_proposal(Some(&vec![0; MAX_LAYOUT_PAYLOAD_BYTES + 1])),
            Err(SetupFailure::Layout)
        );
    }

    #[test]
    fn configure_roles_require_exactly_one_sender() {
        assert_eq!(
            sync_direction(true, LayoutRole::Waiting),
            Ok(SyncDirection::Send)
        );
        assert_eq!(
            sync_direction(false, LayoutRole::Sender),
            Ok(SyncDirection::Receive)
        );
        assert_eq!(
            sync_direction(true, LayoutRole::Sender),
            Err(SetupFailure::LayoutSyncConflict)
        );
        assert_eq!(
            sync_direction(false, LayoutRole::Waiting),
            Err(SetupFailure::LayoutSyncConflict)
        );
    }

    async fn successful_exchange(sender_is_client: bool) {
        let (
            client_endpoint,
            server_endpoint,
            mut client_session,
            mut server_session,
            client_inspection,
            server_inspection,
        ) = configured_sessions().await;
        let client_calls = Arc::new(AtomicUsize::new(0));
        let server_calls = Arc::new(AtomicUsize::new(0));
        let payload = b"reviewed layout".to_vec();
        let client_proposal = sender_is_client.then(|| payload.clone());
        let server_proposal = (!sender_is_client).then(|| payload.clone());
        let client_persist = Arc::clone(&client_calls);
        let server_persist = Arc::clone(&server_calls);
        let (client_result, server_result) = tokio::join!(
            synchronize_inner(
                &mut client_session,
                &client_inspection,
                client_proposal,
                || Ok(()),
                |_| Ok(()),
                move |_, saved| async move {
                    assert_eq!(saved, b"reviewed layout");
                    client_persist.fetch_add(1, Ordering::AcqRel);
                    Ok(())
                },
            ),
            synchronize_inner(
                &mut server_session,
                &server_inspection,
                server_proposal,
                || Ok(()),
                |_| Ok(()),
                move |_, saved| async move {
                    assert_eq!(saved, b"reviewed layout");
                    server_persist.fetch_add(1, Ordering::AcqRel);
                    Ok(())
                },
            ),
        );
        assert_eq!(client_result, Ok(payload.clone()));
        assert_eq!(server_result, Ok(payload));
        assert_eq!(client_calls.load(Ordering::Acquire), 1);
        assert_eq!(server_calls.load(Ordering::Acquire), 1);
        close((&client_endpoint, &server_endpoint));
    }

    #[tokio::test]
    async fn configure_loopback_persists_once_on_both_peers_for_each_sender_direction() {
        tokio::time::timeout(Duration::from_secs(5), async {
            successful_exchange(true).await;
            successful_exchange(false).await;
        })
        .await
        .expect("bounded Configure loopback exchange");
    }

    #[tokio::test]
    async fn configure_loopback_missing_acknowledgement_does_not_persist_sender() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (
                client_endpoint,
                server_endpoint,
                mut client_session,
                mut server_session,
                client_inspection,
                _,
            ) = configured_sessions().await;
            let sender_calls = Arc::new(AtomicUsize::new(0));
            let sender_persist = Arc::clone(&sender_calls);
            let (client_result, ()) = tokio::join!(
                synchronize_inner(
                    &mut client_session,
                    &client_inspection,
                    Some(b"reviewed layout".to_vec()),
                    || Ok(()),
                    |_| Ok(()),
                    move |_, _| async move {
                        sender_persist.fetch_add(1, Ordering::AcqRel);
                        Ok(())
                    },
                ),
                async {
                    let role = read_expected_frame(
                        &mut server_session.control_streams.recv,
                        server_session.initial_epoch,
                        ROLE_SEQUENCE,
                        LayoutKind::Role,
                    )
                    .await
                    .unwrap();
                    assert_eq!(role.role, Some(LayoutRole::Sender));
                    write_layout_frame(
                        &server_session.connection,
                        &mut server_session.control_streams.send,
                        &LayoutFrame {
                            kind: LayoutKind::Role,
                            role: Some(LayoutRole::Waiting),
                            epoch: server_session.initial_epoch,
                            sequence: ROLE_SEQUENCE,
                            digest: [0; 32],
                            payload: Vec::new(),
                        },
                    )
                    .await
                    .unwrap();
                    let proposal = read_expected_frame(
                        &mut server_session.control_streams.recv,
                        server_session.initial_epoch,
                        PROPOSAL_SEQUENCE,
                        LayoutKind::Proposal,
                    )
                    .await
                    .unwrap();
                    assert_eq!(proposal.payload, b"reviewed layout");
                    server_session.control_streams.send.finish().unwrap();
                },
            );
            assert_eq!(client_result, Err(SetupFailure::LayoutSyncIncomplete));
            assert_eq!(sender_calls.load(Ordering::Acquire), 0);
            close((&client_endpoint, &server_endpoint));
        })
        .await
        .expect("bounded missing acknowledgement exchange");
    }

    #[tokio::test]
    async fn configure_loopback_receiver_save_failure_never_acknowledges() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (
                client_endpoint,
                server_endpoint,
                mut client_session,
                mut server_session,
                client_inspection,
                server_inspection,
            ) = configured_sessions().await;
            let sender_calls = Arc::new(AtomicUsize::new(0));
            let receiver_calls = Arc::new(AtomicUsize::new(0));
            let sender_persist = Arc::clone(&sender_calls);
            let receiver_persist = Arc::clone(&receiver_calls);
            let (client_result, server_result) = tokio::join!(
                async {
                    let result = synchronize_inner(
                        &mut client_session,
                        &client_inspection,
                        Some(b"reviewed layout".to_vec()),
                        || Ok(()),
                        |_| Ok(()),
                        move |_, _| async move {
                            sender_persist.fetch_add(1, Ordering::AcqRel);
                            Ok(())
                        },
                    )
                    .await;
                    client_session
                        .connection
                        .close(CONFIGURE_CLOSE_CODE.into(), CONFIGURE_CLOSE_REASON);
                    result
                },
                async {
                    let result = synchronize_inner(
                        &mut server_session,
                        &server_inspection,
                        None,
                        || Ok(()),
                        |_| Ok(()),
                        move |_, _| async move {
                            receiver_persist.fetch_add(1, Ordering::AcqRel);
                            Err(SetupFailure::Layout)
                        },
                    )
                    .await;
                    server_session
                        .connection
                        .close(CONFIGURE_CLOSE_CODE.into(), CONFIGURE_CLOSE_REASON);
                    result
                },
            );
            assert_eq!(client_result, Err(SetupFailure::LayoutSyncIncomplete));
            assert_eq!(server_result, Err(SetupFailure::Layout));
            assert_eq!(sender_calls.load(Ordering::Acquire), 0);
            assert_eq!(receiver_calls.load(Ordering::Acquire), 1);
            close((&client_endpoint, &server_endpoint));
        })
        .await
        .expect("bounded receiver save failure exchange");
    }

    #[tokio::test]
    async fn configure_loopback_sender_save_failure_closes_without_completion() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (
                client_endpoint,
                server_endpoint,
                mut client_session,
                mut server_session,
                client_inspection,
                server_inspection,
            ) = configured_sessions().await;
            let sender_calls = Arc::new(AtomicUsize::new(0));
            let receiver_calls = Arc::new(AtomicUsize::new(0));
            let sender_persist = Arc::clone(&sender_calls);
            let receiver_persist = Arc::clone(&receiver_calls);
            let (client_result, server_result) = tokio::join!(
                async {
                    let result = synchronize_inner(
                        &mut client_session,
                        &client_inspection,
                        Some(b"reviewed layout".to_vec()),
                        || Ok(()),
                        |_| Ok(()),
                        move |_, _| async move {
                            sender_persist.fetch_add(1, Ordering::AcqRel);
                            Err(SetupFailure::Layout)
                        },
                    )
                    .await;
                    client_session
                        .connection
                        .close(CONFIGURE_CLOSE_CODE.into(), CONFIGURE_CLOSE_REASON);
                    result
                },
                synchronize_inner(
                    &mut server_session,
                    &server_inspection,
                    None,
                    || Ok(()),
                    |_| Ok(()),
                    move |_, _| async move {
                        receiver_persist.fetch_add(1, Ordering::AcqRel);
                        Ok(())
                    },
                ),
            );
            assert_eq!(client_result, Err(SetupFailure::LayoutSyncIncomplete));
            assert_eq!(server_result, Err(SetupFailure::LayoutSyncIncomplete));
            assert_eq!(sender_calls.load(Ordering::Acquire), 1);
            assert_eq!(receiver_calls.load(Ordering::Acquire), 1);
            close((&client_endpoint, &server_endpoint));
        })
        .await
        .expect("bounded sender save failure exchange");
    }

    #[tokio::test]
    async fn configure_loopback_rejects_role_conflict_before_persistence() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (
                client_endpoint,
                server_endpoint,
                mut client_session,
                mut server_session,
                client_inspection,
                server_inspection,
            ) = configured_sessions().await;
            let client_calls = Arc::new(AtomicUsize::new(0));
            let server_calls = Arc::new(AtomicUsize::new(0));
            let client_persist = Arc::clone(&client_calls);
            let server_persist = Arc::clone(&server_calls);
            let (client_result, server_result) = tokio::join!(
                synchronize_inner(
                    &mut client_session,
                    &client_inspection,
                    Some(b"client layout".to_vec()),
                    || Ok(()),
                    |_| Ok(()),
                    move |_, _| async move {
                        client_persist.fetch_add(1, Ordering::AcqRel);
                        Ok(())
                    },
                ),
                synchronize_inner(
                    &mut server_session,
                    &server_inspection,
                    Some(b"server layout".to_vec()),
                    || Ok(()),
                    |_| Ok(()),
                    move |_, _| async move {
                        server_persist.fetch_add(1, Ordering::AcqRel);
                        Ok(())
                    },
                ),
            );
            assert_eq!(client_result, Err(SetupFailure::LayoutSyncConflict));
            assert_eq!(server_result, Err(SetupFailure::LayoutSyncConflict));
            assert_eq!(client_calls.load(Ordering::Acquire), 0);
            assert_eq!(server_calls.load(Ordering::Acquire), 0);
            close((&client_endpoint, &server_endpoint));
        })
        .await
        .expect("bounded Configure role conflict exchange");
    }

    #[tokio::test]
    async fn configure_loopback_rejects_trailing_bytes_after_sender_persists() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (
                client_endpoint,
                server_endpoint,
                mut client_session,
                mut server_session,
                client_inspection,
                _,
            ) = configured_sessions().await;
            let sender_calls = Arc::new(AtomicUsize::new(0));
            let sender_persist = Arc::clone(&sender_calls);
            let (client_result, ()) = tokio::join!(
                synchronize_inner(
                    &mut client_session,
                    &client_inspection,
                    Some(b"reviewed layout".to_vec()),
                    || Ok(()),
                    |_| Ok(()),
                    move |_, _| async move {
                        sender_persist.fetch_add(1, Ordering::AcqRel);
                        Ok(())
                    },
                ),
                async {
                    let role = read_expected_frame(
                        &mut server_session.control_streams.recv,
                        server_session.initial_epoch,
                        ROLE_SEQUENCE,
                        LayoutKind::Role,
                    )
                    .await
                    .unwrap();
                    assert_eq!(role.role, Some(LayoutRole::Sender));
                    write_layout_frame(
                        &server_session.connection,
                        &mut server_session.control_streams.send,
                        &LayoutFrame {
                            kind: LayoutKind::Role,
                            role: Some(LayoutRole::Waiting),
                            epoch: server_session.initial_epoch,
                            sequence: ROLE_SEQUENCE,
                            digest: [0; 32],
                            payload: Vec::new(),
                        },
                    )
                    .await
                    .unwrap();
                    let proposal = read_expected_frame(
                        &mut server_session.control_streams.recv,
                        server_session.initial_epoch,
                        PROPOSAL_SEQUENCE,
                        LayoutKind::Proposal,
                    )
                    .await
                    .unwrap();
                    let acknowledgement = LayoutFrame {
                        kind: LayoutKind::Acknowledgement,
                        role: None,
                        epoch: server_session.initial_epoch,
                        sequence: ACK_SEQUENCE,
                        digest: proposal.digest,
                        payload: Vec::new(),
                    };
                    write_layout_frame(
                        &server_session.connection,
                        &mut server_session.control_streams.send,
                        &acknowledgement,
                    )
                    .await
                    .unwrap();
                    let complete = read_expected_frame(
                        &mut server_session.control_streams.recv,
                        server_session.initial_epoch,
                        COMPLETE_SEQUENCE,
                        LayoutKind::Complete,
                    )
                    .await
                    .unwrap();
                    assert_eq!(complete.digest, proposal.digest);
                    server_session
                        .control_streams
                        .send
                        .write_all(&[0])
                        .await
                        .unwrap();
                    server_session.control_streams.send.finish().unwrap();
                },
            );
            assert_eq!(client_result, Err(SetupFailure::LayoutSyncIncomplete));
            assert_eq!(sender_calls.load(Ordering::Acquire), 1);
            close((&client_endpoint, &server_endpoint));
        })
        .await
        .expect("bounded trailing bytes exchange");
    }

    #[tokio::test]
    async fn configure_loopback_cancellation_after_receiver_save_stops_completion() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (
                client_endpoint,
                server_endpoint,
                mut client_session,
                mut server_session,
                client_inspection,
                server_inspection,
            ) = configured_sessions().await;
            let cancellation = RevocationSignal::default();
            let client_cancel = cancellation.clone();
            let server_cancel = cancellation.clone();
            let cancel_after_save = cancellation.clone();
            let sender_calls = Arc::new(AtomicUsize::new(0));
            let receiver_calls = Arc::new(AtomicUsize::new(0));
            let sender_persist = Arc::clone(&sender_calls);
            let receiver_persist = Arc::clone(&receiver_calls);
            let (client_result, server_result) = tokio::join!(
                async {
                    let result = synchronize_inner(
                        &mut client_session,
                        &client_inspection,
                        Some(b"reviewed layout".to_vec()),
                        move || {
                            if client_cancel.is_revoked() {
                                Err(SetupFailure::Cancelled)
                            } else {
                                Ok(())
                            }
                        },
                        |_| Ok(()),
                        move |_, _| async move {
                            sender_persist.fetch_add(1, Ordering::AcqRel);
                            Ok(())
                        },
                    )
                    .await;
                    client_session
                        .connection
                        .close(CONFIGURE_CLOSE_CODE.into(), CONFIGURE_CLOSE_REASON);
                    result
                },
                async {
                    let result = synchronize_inner(
                        &mut server_session,
                        &server_inspection,
                        None,
                        move || {
                            if server_cancel.is_revoked() {
                                Err(SetupFailure::Cancelled)
                            } else {
                                Ok(())
                            }
                        },
                        |_| Ok(()),
                        move |_, _| async move {
                            receiver_persist.fetch_add(1, Ordering::AcqRel);
                            cancel_after_save.revoke();
                            Ok(())
                        },
                    )
                    .await;
                    server_session
                        .connection
                        .close(CONFIGURE_CLOSE_CODE.into(), CONFIGURE_CLOSE_REASON);
                    result
                },
            );
            assert_eq!(client_result, Err(SetupFailure::LayoutSyncIncomplete));
            assert_eq!(server_result, Err(SetupFailure::Cancelled));
            assert_eq!(sender_calls.load(Ordering::Acquire), 0);
            assert_eq!(receiver_calls.load(Ordering::Acquire), 1);
            close((&client_endpoint, &server_endpoint));
        })
        .await
        .expect("bounded cancelled Configure exchange");
    }
    #[tokio::test]
    async fn configure_deadline_cancels_waiting_persistence_before_acknowledgement() {
        tokio::time::timeout(Duration::from_secs(2), async {
            let (
                client_endpoint,
                server_endpoint,
                mut client_session,
                mut server_session,
                client_inspection,
                server_inspection,
            ) = configured_sessions().await;
            let cancelled = RevocationSignal::default();
            let deadline_cancel = cancelled.clone();
            let client_cancel = cancelled.clone();
            let server_cancel = cancelled.clone();
            let endpoint = client_endpoint.clone();
            let (entered, wait_entered) = tokio::sync::oneshot::channel();
            let expired_cancel = cancelled.clone();
            let deadline = async move {
                wait_entered
                    .await
                    .expect("receiver persistence must begin before the deadline arms");
                let deadline = ConfigureDeadline::start_with(
                    Duration::from_millis(20),
                    Arc::new(move || deadline_cancel.revoke()),
                    move || endpoint.close(CONFIGURE_CLOSE_CODE.into(), CONFIGURE_CLOSE_REASON),
                )
                .unwrap();
                while !expired_cancel.is_revoked() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                deadline.expired()
            };
            let sender_calls = Arc::new(AtomicUsize::new(0));
            let receiver_calls = Arc::new(AtomicUsize::new(0));
            let sender_persist = Arc::clone(&sender_calls);
            let receiver_persist = Arc::clone(&receiver_calls);
            let (client_result, server_result, expired) = tokio::join!(
                synchronize_inner(
                    &mut client_session,
                    &client_inspection,
                    Some(b"reviewed layout".to_vec()),
                    move || {
                        if client_cancel.is_revoked() {
                            Err(SetupFailure::Cancelled)
                        } else {
                            Ok(())
                        }
                    },
                    |_| Ok(()),
                    move |_, _| async move {
                        sender_persist.fetch_add(1, Ordering::AcqRel);
                        Ok(())
                    },
                ),
                synchronize_inner(
                    &mut server_session,
                    &server_inspection,
                    None,
                    move || {
                        if server_cancel.is_revoked() {
                            Err(SetupFailure::Cancelled)
                        } else {
                            Ok(())
                        }
                    },
                    |_| Ok(()),
                    move |_, _| {
                        let cancelled = cancelled.clone();
                        receiver_persist.fetch_add(1, Ordering::AcqRel);
                        entered.send(()).unwrap();
                        async move {
                            while !cancelled.is_revoked() {
                                tokio::time::sleep(Duration::from_millis(1)).await;
                            }
                            Err(SetupFailure::Cancelled)
                        }
                    },
                ),
                deadline,
            );
            assert!(expired);
            assert_eq!(client_result, Err(SetupFailure::LayoutSyncIncomplete));
            assert_eq!(server_result, Err(SetupFailure::Cancelled));
            assert_eq!(sender_calls.load(Ordering::Acquire), 0);
            assert_eq!(receiver_calls.load(Ordering::Acquire), 1);
            close((&client_endpoint, &server_endpoint));
        })
        .await
        .expect("deadline must finish the waiting Configure exchange");
    }

    #[tokio::test]
    async fn configure_deadline_closes_loopback_without_runtime_progress() {
        let (client_endpoint, server_endpoint, client_session, _server_session, _, _) =
            configured_sessions().await;
        let (aborted, wait_abort) = std::sync::mpsc::channel();
        let endpoint = client_endpoint.clone();
        let deadline = ConfigureDeadline::start_with(
            Duration::from_millis(20),
            Arc::new(move || {
                let _ = aborted.send(());
            }),
            move || endpoint.close(CONFIGURE_CLOSE_CODE.into(), CONFIGURE_CLOSE_REASON),
        )
        .unwrap();
        wait_abort
            .recv_timeout(Duration::from_millis(250))
            .expect("deadline must abort persistence");
        tokio::time::timeout(
            Duration::from_millis(250),
            client_session.connection.closed(),
        )
        .await
        .expect("deadline must close the negotiated socket");
        assert!(deadline.expired());
        close((&client_endpoint, &server_endpoint));
    }
}
