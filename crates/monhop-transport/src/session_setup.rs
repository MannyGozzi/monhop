//! Explicit paired metadata inspection and fresh session checks before input starts.

use std::{future::Future, net::SocketAddrV4, sync::mpsc, thread::JoinHandle, time::Duration};

use monhop_core::{
    DeviceId, DisplayId, EdgeLink, Machine, Platform, Point, RevocationSignal, Topology,
};
use monhop_protocol::{Capabilities, DeliveryClass, Frame, Message};

pub use monhop_protocol::{
    DisplayDescription, DisplayTopology, MAX_DISPLAY_NAME_BYTES, MAX_LOGICAL_ORIGIN_ABS,
    MAX_LOGICAL_SIZE, MAX_NATIVE_DIMENSION, MAX_SCALE_FACTOR, MIN_SCALE_FACTOR,
};

use crate::{
    crypto::{CertificateFingerprint, DeviceIdentity, VerifiedPeer},
    guarded_endpoint::{GuardedEndpoint, NetworkSelection},
    identity_store::{ProtectedPeerStore, load_identity},
    native_storage::{NativeIdentityStore, NativePeerStore},
    pairing::{ConfirmedPeerRecord, PAIRING_PORT, initiates_connection, opposite_platform},
    session_handshake::{
        HandshakeConfig, HandshakeError, NegotiatedSession, SessionPurpose,
        device_id_from_fingerprint, negotiate,
    },
    session_native::current_displays,
    session_wire::write_frame,
};

const METADATA_COMPLETION_DEADLINE: Duration = Duration::from_secs(1);
/// The whole rendezvous for one explicit local action, unchanged by the dial retry below.
const CONNECT_WINDOW: Duration = Duration::from_secs(120);
/// One Windows dial attempt covers connect plus negotiate; the Mac may not be listening yet.
const DIAL_ATTEMPT_DEADLINE: Duration = Duration::from_secs(4);
const DIAL_INTERVAL: Duration = Duration::from_secs(2);
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(10);
const METADATA_COMPLETION_TOKEN: u64 = 0x4c4b_4d49_4e53_5043;
const METADATA_CLOSE_CODE: u32 = 3;
const METADATA_CLOSE_REASON: &[u8] = b"metadata inspection failed";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupFailure {
    Identity,
    PairingRequired,
    NetworkSelection,
    NetworkRoute,
    /// The pinned port is still closing from a previous session on this computer.
    PortBusy,
    Connection,
    Handshake,
    Cancelled,
    Displays,
    ChangedSinceInspection,
    Layout,
    LayoutSyncConflict,
    LayoutSyncIncomplete,
    PurposeMismatch,
    VersionMismatch,
}

/// Which computer supplies the physical keyboard and mouse, named by side rather than platform.
/// Two Macs or two Windows machines can be paired, so "macOS" no longer identifies a computer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceSide {
    Local,
    Peer,
}

#[derive(Clone)]
pub struct InspectedPeer {
    pub local_device: DeviceId,
    pub peer_device: DeviceId,
    pub source: DeviceId,
    pub local_fingerprint: CertificateFingerprint,
    pub peer_fingerprint: CertificateFingerprint,
    pub local_platform: Platform,
    pub peer_platform: Platform,
    pub local_displays: DisplayTopology,
    pub peer_displays: DisplayTopology,
    pub interface_id: String,
}

pub struct PairedSession {
    pub lease: EndpointLease,
    pub revocation: RevocationSignal,
    pub session: NegotiatedSession,
    pub inspection: InspectedPeer,
}

/// The bound endpoint behind one session, kept opaque so it can only go back to its holder.
pub struct EndpointLease {
    prepared: PreparedEndpoint,
}

impl EndpointLease {
    pub fn endpoint(&self) -> &GuardedEndpoint {
        self.prepared.endpoint()
    }
}

impl PairedSession {
    pub fn endpoint(&self) -> &GuardedEndpoint {
        self.lease.endpoint()
    }

    /// Split borrows for exchanges that drive the session while watching the endpoint.
    pub(crate) fn parts(&mut self) -> (&mut NegotiatedSession, &InspectedPeer, &GuardedEndpoint) {
        (&mut self.session, &self.inspection, self.lease.endpoint())
    }
}

/// A deliberate stop, or a native stop request, keeps the socket open this long so the session's
/// QUIC close can leave; a native revocation still closes it at once.
const STOP_GRACE: Duration = Duration::from_millis(100);

struct CancellationWatch {
    stop: mpsc::Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl CancellationWatch {
    fn start(
        cancel: RevocationSignal,
        native_revocation: RevocationSignal,
        revoke: impl FnOnce() + Send + 'static,
    ) -> Result<Self, SetupFailure> {
        let (stop, receive) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("monhop-session-cancel".into())
            .spawn(move || {
                let mut stop_requested_at: Option<std::time::Instant> = None;
                loop {
                    let grace_over = stop_requested_at.is_some_and(|at| at.elapsed() >= STOP_GRACE);
                    if native_revocation.is_revoked() || grace_over {
                        native_revocation.mark_revoked_without_wake();
                        cancel.mark_revoked_without_wake();
                        revoke();
                        return;
                    }
                    if (cancel.is_revoked() || native_revocation.is_stopping())
                        && stop_requested_at.is_none()
                    {
                        native_revocation.request_stop();
                        cancel.request_stop();
                        stop_requested_at = Some(std::time::Instant::now());
                    }
                    match receive.recv_timeout(Duration::from_millis(10)) {
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        _ => return,
                    }
                }
            })
            .map_err(|_| SetupFailure::Connection)?;
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
}

impl Drop for CancellationWatch {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn check_cancel(cancel: &RevocationSignal) -> Result<(), SetupFailure> {
    if cancel.is_revoked() {
        Err(SetupFailure::Cancelled)
    } else {
        Ok(())
    }
}

pub fn selected_network(interface_id: &str) -> Result<NetworkSelection, SetupFailure> {
    #[cfg(windows)]
    let adapters = monhop_platform_windows::network::enumerate_adapters()
        .map_err(|_| SetupFailure::NetworkSelection)?;
    #[cfg(target_os = "macos")]
    let adapters = monhop_platform_macos::network::enumerate_adapters_with_attachment()
        .map_err(|_| SetupFailure::NetworkSelection)?;
    let mut matches = adapters.into_iter().filter(|adapter| {
        format!(
            "{}:{}:{}",
            adapter.stable_id, adapter.index, adapter.address
        ) == interface_id
    });
    let adapter = matches.next().ok_or(SetupFailure::NetworkSelection)?;
    if matches.next().is_some()
        || !adapter.physical
        || !adapter.up
        || !(adapter.wifi || adapter.ethernet)
        || adapter.attachment.is_none()
    {
        return Err(SetupFailure::NetworkSelection);
    }
    Ok(NetworkSelection {
        stable_id: adapter.stable_id,
        interface_index: adapter.index,
        local: SocketAddrV4::new(adapter.address, PAIRING_PORT),
        peer: SocketAddrV4::new(adapter.address, PAIRING_PORT),
    })
}

/// Opens a share-purpose session after local authorization.
///
/// The source choice describes where the physical devices are attached, independently of dialing.
/// Both computers must name the same one, so the two sides pass opposite sides.
pub async fn connect_after_local_action(
    interface_id: &str,
    source: SourceSide,
    cancel: &RevocationSignal,
    peer: CertificateFingerprint,
) -> Result<PairedSession, SetupFailure> {
    connect_for_purpose(
        interface_id,
        Some(source),
        cancel,
        peer,
        SessionPurpose::Share,
    )
    .await
}

/// Opens the separately authenticated controlled trial after local authorization.
pub async fn connect_trial_after_local_action(
    interface_id: &str,
    source: SourceSide,
    cancel: &RevocationSignal,
    peer: CertificateFingerprint,
) -> Result<PairedSession, SetupFailure> {
    connect_for_purpose(
        interface_id,
        Some(source),
        cancel,
        peer,
        SessionPurpose::ControlledTrial,
    )
    .await
}

/// A bound, interface-pinned endpoint that can dial or accept repeatedly without rebinding.
///
/// Binding loads the stored identity and the one confirmed peer it is for. Every connection made
/// from it re-enumerates local displays so a negotiated session never carries stale geometry.
pub(crate) struct PreparedEndpoint {
    // Intentional endpoint teardown must not feed back into a successful local action.
    _cancellation: CancellationWatch,
    identity: DeviceIdentity,
    peer: VerifiedPeer,
    endpoint: GuardedEndpoint,
    revocation: RevocationSignal,
    local_device: DeviceId,
    peer_device: DeviceId,
    local_platform: Platform,
    peer_platform: Platform,
    source: DeviceId,
    dials: bool,
    interface_id: String,
}

/// Binds the one selected interface for repeated use.
///
/// `source` names the computer that supplies input. `None` is for metadata purposes that carry no
/// input authority: they name the dialing computer, so both sides agree without asking the user.
pub(crate) fn prepare_endpoint(
    interface_id: &str,
    source: Option<SourceSide>,
    cancel: &RevocationSignal,
    peer_fingerprint: CertificateFingerprint,
) -> Result<PreparedEndpoint, SetupFailure> {
    check_cancel(cancel)?;
    let identity = load_identity(&NativeIdentityStore)
        .map_err(|error| {
            log::warn!("session setup: the local identity could not be loaded: {error}");
            SetupFailure::Identity
        })?
        .ok_or(SetupFailure::PairingRequired)?;
    check_cancel(cancel)?;
    let record = confirmed_peer(&NativePeerStore, identity.fingerprint(), peer_fingerprint)?;
    let peer = record
        .peer()
        .verified_peer()
        .map_err(|_| SetupFailure::PairingRequired)?;
    let local_device = device_id_from_fingerprint(identity.fingerprint());
    let peer_device = device_id_from_fingerprint(peer.fingerprint());
    #[cfg(windows)]
    let local_platform = Platform::Windows;
    #[cfg(target_os = "macos")]
    let local_platform = Platform::MacOs;
    let peer_platform = record
        .peer()
        .platform()
        .unwrap_or_else(|| opposite_platform(local_platform));
    let dials = initiates_connection(
        local_platform,
        identity.fingerprint(),
        record.peer().platform(),
        peer.fingerprint(),
    );
    let source = match source.unwrap_or(if dials {
        SourceSide::Local
    } else {
        SourceSide::Peer
    }) {
        SourceSide::Local => local_device,
        SourceSide::Peer => peer_device,
    };
    check_cancel(cancel)?;
    let mut selection = selected_network(interface_id)?;
    selection.peer = record.peer().endpoint();
    check_cancel(cancel)?;
    let endpoint =
        GuardedEndpoint::bind_after_local_enable(selection, &identity, &peer).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AddrInUse {
                SetupFailure::PortBusy
            } else {
                SetupFailure::NetworkRoute
            }
        })?;
    check_cancel(cancel)?;
    let revoker = endpoint.revoker();
    // Cancellation must still close the socket while native startup blocks its async runtime.
    let revocation = endpoint.revocation_signal();
    let _cancellation =
        CancellationWatch::start(cancel.clone(), revocation.clone(), move || revoker.revoke())?;
    Ok(PreparedEndpoint {
        _cancellation,
        identity,
        peer,
        endpoint,
        revocation,
        local_device,
        peer_device,
        local_platform,
        peer_platform,
        source,
        dials,
        interface_id: interface_id.to_owned(),
    })
}

/// The stored record for `peer` among this computer's pairings; a record that no longer decodes
/// for the local identity is a stale pairing and never matches.
pub fn confirmed_peer(
    store: &impl ProtectedPeerStore,
    local: CertificateFingerprint,
    peer: CertificateFingerprint,
) -> Result<ConfirmedPeerRecord, SetupFailure> {
    store
        .list()
        .map_err(|error| {
            log::warn!("session setup: the paired computers could not be listed: {error}");
            SetupFailure::Identity
        })?
        .into_iter()
        .filter_map(|stored| ConfirmedPeerRecord::decode(&stored.record, local).ok())
        .find(|record| record.peer().fingerprint() == peer)
        .ok_or(SetupFailure::PairingRequired)
}

impl PreparedEndpoint {
    /// Exactly one of the two paired computers dials; the other one listens.
    pub(crate) const fn dials(&self) -> bool {
        self.dials
    }

    pub(crate) const fn endpoint(&self) -> &GuardedEndpoint {
        &self.endpoint
    }

    /// Dials the one pinned peer address. Never discovers or falls back to another address.
    pub(crate) async fn dial(
        &self,
        cancel: &RevocationSignal,
    ) -> Result<quinn::Connection, SetupFailure> {
        let connecting = self.endpoint.connect().map_err(|error| {
            log::debug!("share dial could not start: {error}");
            SetupFailure::Connection
        })?;
        self.while_active(cancel, async move {
            connecting.await.map_err(|error| {
                log::debug!("share dial failed: {error}");
                SetupFailure::Connection
            })
        })
        .await
    }

    /// Accepts one connection from the one pinned peer identity.
    pub(crate) async fn accept(
        &self,
        cancel: &RevocationSignal,
    ) -> Result<quinn::Connection, SetupFailure> {
        self.while_active(cancel, async {
            self.endpoint.accept().await.map_err(|error| {
                log::debug!("share accept failed: {error}");
                SetupFailure::Connection
            })
        })
        .await
    }

    /// Runs the bilateral handshake for one connection and reports fresh peer facts.
    pub(crate) async fn negotiate_session(
        &self,
        connection: quinn::Connection,
        purpose: SessionPurpose,
    ) -> Result<(NegotiatedSession, InspectedPeer), SetupFailure> {
        let local_displays =
            current_displays(self.local_device).map_err(|_| SetupFailure::Displays)?;
        let capabilities =
            Capabilities::new(Capabilities::RELATIVE_MOTION | Capabilities::DISPLAY_TOPOLOGY)
                .map_err(|_| SetupFailure::Handshake)?;
        let config = HandshakeConfig::new(
            &self.identity,
            &self.peer,
            self.local_platform,
            self.peer_platform,
            capabilities,
            capabilities,
            &local_displays,
            self.source,
            purpose,
        )
        .map_err(|_| SetupFailure::Handshake)?;
        let session = negotiate(connection, config)
            .await
            .map_err(handshake_failure)?;
        if cancel_or_endpoint_revoked(None, &self.endpoint) {
            return Err(SetupFailure::Cancelled);
        }
        let inspection = InspectedPeer {
            local_device: self.local_device,
            peer_device: self.peer_device,
            source: self.source,
            local_fingerprint: self.identity.fingerprint(),
            peer_fingerprint: self.peer.fingerprint(),
            local_platform: self.local_platform,
            peer_platform: self.peer_platform,
            local_displays,
            peer_displays: session.peer.topology.clone(),
            interface_id: self.interface_id.clone(),
        };
        Ok((session, inspection))
    }

    pub(crate) fn into_paired(
        self,
        session: NegotiatedSession,
        inspection: InspectedPeer,
    ) -> PairedSession {
        PairedSession {
            revocation: self.revocation.clone(),
            lease: EndpointLease { prepared: self },
            session,
            inspection,
        }
    }

    async fn while_active<T>(
        &self,
        cancel: &RevocationSignal,
        work: impl Future<Output = Result<T, SetupFailure>>,
    ) -> Result<T, SetupFailure> {
        tokio::pin!(work);
        let mut tick = tokio::time::interval(CANCEL_POLL_INTERVAL);
        loop {
            tokio::select! {
                biased;
                _ = tick.tick() => if cancel_or_endpoint_revoked(Some(cancel), &self.endpoint) {
                    return Err(SetupFailure::Cancelled);
                },
                result = &mut work => return result,
            }
        }
    }
}

fn cancel_or_endpoint_revoked(
    cancel: Option<&RevocationSignal>,
    endpoint: &GuardedEndpoint,
) -> bool {
    cancel.is_some_and(RevocationSignal::is_revoked) || endpoint.is_revoked()
}

/// Distinguishes a peer that answered but disagrees from a peer that could not be reached.
pub(crate) const fn handshake_failure(error: HandshakeError) -> SetupFailure {
    match error {
        HandshakeError::PurposeMismatch => SetupFailure::PurposeMismatch,
        // Purposes agree, so both sides opened a session and their saved records name different
        // keyboard sides. Dialing again repeats it; only a fresh agreed record clears it.
        HandshakeError::SourceMismatch => SetupFailure::ChangedSinceInspection,
        HandshakeError::PeerHelloMismatch => SetupFailure::VersionMismatch,
        _ => SetupFailure::Handshake,
    }
}

/// Only reach and timing failures are worth another dial. A disagreement repeats itself: a peer in
/// a different step (setup link versus sharing) answers the same way every time, so the caller must
/// change step rather than dial on, and `PurposeMismatch` is reported at once.
pub(crate) const fn is_transient(error: SetupFailure) -> bool {
    matches!(
        error,
        SetupFailure::Connection | SetupFailure::Handshake | SetupFailure::PortBusy
    )
}

pub(crate) async fn connect_for_purpose(
    interface_id: &str,
    source: Option<SourceSide>,
    cancel: &RevocationSignal,
    peer: CertificateFingerprint,
    purpose: SessionPurpose,
) -> Result<PairedSession, SetupFailure> {
    let prepared = prepare_endpoint(interface_id, source, cancel, peer)?;
    connect_prepared(prepared, cancel, purpose)
        .await
        .map_err(|failed| failed.1)
}

/// A failed window hands the endpoint back so the next attempt needs no new identity read.
async fn connect_prepared(
    prepared: PreparedEndpoint,
    cancel: &RevocationSignal,
    purpose: SessionPurpose,
) -> Result<PairedSession, Box<(PreparedEndpoint, SetupFailure)>> {
    // Start order must not decide a test window or a share, and only the dialing side can retry.
    let retry_dial = prepared.dials()
        && matches!(
            purpose,
            SessionPurpose::ControlledTrial | SessionPurpose::Share
        );
    let attempt = tokio::time::timeout(CONNECT_WINDOW, async {
        if retry_dial {
            return dial_until_negotiated(&prepared, cancel, purpose).await;
        }
        let connection = if prepared.dials() {
            prepared.dial(cancel).await?
        } else {
            prepared.accept(cancel).await?
        };
        prepared.negotiate_session(connection, purpose).await
    })
    .await;
    match attempt {
        Ok(Ok((session, inspection))) => Ok(prepared.into_paired(session, inspection)),
        Ok(Err(error)) => Err(Box::new((prepared, error))),
        Err(_) => {
            log::debug!("share connect window of {CONNECT_WINDOW:?} passed without a session");
            Err(Box::new((prepared, SetupFailure::Connection)))
        }
    }
}

/// A revoked endpoint drains this long before its pinned port is rebound; a drain that hangs
/// leaves the next bind to report the port as busy.
const REVOKED_ENDPOINT_DRAIN: Duration = Duration::from_secs(3);

/// One bound share endpoint kept across attempts and sessions: the protected identity is read
/// once while the switch stays on, so the OS asks for the Keychain at most once, and a dropped
/// session reconnects on the same socket without a rebind.
pub struct StandingShareEndpoint {
    interface_id: String,
    source: SourceSide,
    peer: CertificateFingerprint,
    prepared: Option<PreparedEndpoint>,
}

impl StandingShareEndpoint {
    pub fn new(interface_id: &str, source: SourceSide, peer: CertificateFingerprint) -> Self {
        Self {
            interface_id: interface_id.to_owned(),
            source,
            peer,
            prepared: None,
        }
    }

    /// A revoked endpoint (network change or cancellation) is rebound on the next attempt.
    pub async fn connect(
        &mut self,
        cancel: &RevocationSignal,
    ) -> Result<PairedSession, SetupFailure> {
        let prepared = match self.prepared.take() {
            Some(prepared) if !prepared.endpoint().is_revoked() => prepared,
            _ => prepare_endpoint(&self.interface_id, Some(self.source), cancel, self.peer)?,
        };
        match connect_prepared(prepared, cancel, SessionPurpose::Share).await {
            Ok(paired) => Ok(paired),
            Err(failed) => {
                let (prepared, error) = *failed;
                self.prepared = Some(prepared);
                Err(error)
            }
        }
    }

    /// Takes an ended session's endpoint back to serve the next connection, unless the network
    /// revoked it, in which case it drains before the rebind.
    pub async fn reclaim(&mut self, lease: EndpointLease) {
        let EndpointLease { prepared } = lease;
        if prepared.endpoint().is_revoked() {
            let _ = tokio::time::timeout(
                REVOKED_ENDPOINT_DRAIN,
                prepared.endpoint().close_and_wait_idle(),
            )
            .await;
            return;
        }
        self.prepared = Some(prepared);
    }
}

async fn dial_until_negotiated(
    prepared: &PreparedEndpoint,
    cancel: &RevocationSignal,
    purpose: SessionPurpose,
) -> Result<(NegotiatedSession, InspectedPeer), SetupFailure> {
    loop {
        let attempt = tokio::time::timeout(DIAL_ATTEMPT_DEADLINE, async {
            let connection = prepared.dial(cancel).await?;
            prepared.negotiate_session(connection, purpose).await
        })
        .await;
        match attempt {
            Ok(Ok(established)) => return Ok(established),
            Ok(Err(error)) if !is_transient(error) => return Err(error),
            Ok(Err(error)) => log::debug!("share dial attempt failed: {error:?}"),
            Err(_) => log::debug!("share dial attempt passed {DIAL_ATTEMPT_DEADLINE:?}"),
        }
        check_cancel(cancel)?;
        tokio::time::sleep(DIAL_INTERVAL).await;
        check_cancel(cancel)?;
    }
}

/// Exchanges a bilateral completion barrier for a metadata-only session.
///
/// This accepts only the two expected baseline-epoch Ping/Pong frames and then requires a clean
/// stream end. Any input frame, unexpected control message, stale sequence, or trailing byte
/// fails closed before a caller can treat the inspection as complete.
pub async fn complete_metadata_inspection(
    session: &mut NegotiatedSession,
) -> Result<(), SetupFailure> {
    let result = tokio::time::timeout(
        METADATA_COMPLETION_DEADLINE,
        complete_metadata_inspection_inner(session),
    )
    .await;
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            session
                .connection
                .close(METADATA_CLOSE_CODE.into(), METADATA_CLOSE_REASON);
            Err(error)
        }
        Err(_) => {
            session
                .connection
                .close(METADATA_CLOSE_CODE.into(), METADATA_CLOSE_REASON);
            Err(SetupFailure::Connection)
        }
    }
}

async fn complete_metadata_inspection_inner(
    session: &mut NegotiatedSession,
) -> Result<(), SetupFailure> {
    if session.purpose() != SessionPurpose::Inspect {
        return Err(SetupFailure::Handshake);
    }
    let first_sequence = session.control.next_sequence();
    let ping = session
        .control
        .next_heartbeat(Message::Ping(METADATA_COMPLETION_TOKEN))
        .map_err(|_| SetupFailure::Handshake)?;
    write_frame(
        &session.connection,
        &mut session.control_streams.send,
        &ping,
    )
    .await
    .map_err(|_| SetupFailure::Connection)?;

    let peer_ping = session
        .control_streams
        .reader
        .read_frame(&mut session.control_streams.recv)
        .await
        .map_err(|_| SetupFailure::Handshake)?;
    validate_metadata_frame(
        &peer_ping,
        session.initial_epoch,
        first_sequence,
        Message::Ping(METADATA_COMPLETION_TOKEN),
    )?;

    let pong = session
        .control
        .next_heartbeat(Message::Pong(METADATA_COMPLETION_TOKEN))
        .map_err(|_| SetupFailure::Handshake)?;
    write_frame(
        &session.connection,
        &mut session.control_streams.send,
        &pong,
    )
    .await
    .map_err(|_| SetupFailure::Connection)?;
    session
        .control_streams
        .send
        .finish()
        .map_err(|_| SetupFailure::Connection)?;

    let peer_pong = session
        .control_streams
        .reader
        .read_frame(&mut session.control_streams.recv)
        .await
        .map_err(|_| SetupFailure::Handshake)?;
    validate_metadata_frame(
        &peer_pong,
        session.initial_epoch,
        first_sequence
            .checked_add(1)
            .ok_or(SetupFailure::Handshake)?,
        Message::Pong(METADATA_COMPLETION_TOKEN),
    )?;

    session
        .control_streams
        .recv
        .read_to_end(0)
        .await
        .map_err(|_| SetupFailure::Handshake)?;
    // Endpoint teardown must not discard a locally queued FIN before the peer receives it.
    if session
        .control_streams
        .send
        .stopped()
        .await
        .map_err(|_| SetupFailure::Connection)?
        .is_some()
    {
        return Err(SetupFailure::Handshake);
    }
    Ok(())
}

fn validate_metadata_frame(
    frame: &Frame,
    epoch: monhop_protocol::SessionEpoch,
    sequence: u64,
    expected: Message,
) -> Result<(), SetupFailure> {
    if frame.epoch != epoch
        || frame.sequence != sequence
        || frame.delivery() != DeliveryClass::Reliable
        || frame.message != expected
    {
        return Err(SetupFailure::Handshake);
    }
    Ok(())
}

/// Exchanges public display metadata, then closes the connection without starting native input.
pub async fn inspect_after_local_action(
    interface_id: &str,
    source: SourceSide,
    cancel: &RevocationSignal,
    peer: CertificateFingerprint,
) -> Result<InspectedPeer, SetupFailure> {
    let mut paired =
        connect_for_purpose(interface_id, None, cancel, peer, SessionPurpose::Inspect).await?;
    complete_metadata_inspection(&mut paired.session).await?;
    check_cancel(cancel)?;
    paired.endpoint().close_and_wait_idle().await.map_err(|_| {
        if cancel.is_revoked() || paired.endpoint().is_revoked() {
            SetupFailure::Cancelled
        } else {
            SetupFailure::Connection
        }
    })?;
    if cancel.is_revoked() || paired.endpoint().is_revoked() {
        return Err(SetupFailure::Cancelled);
    }
    // The inspection exchange agrees on the dialing computer; the user's own choice is local.
    let mut inspection = paired.inspection;
    inspection.source = inspection.device_for(source);
    Ok(inspection)
}

impl InspectedPeer {
    pub const fn device_for(&self, source: SourceSide) -> DeviceId {
        match source {
            SourceSide::Local => self.local_device,
            SourceSide::Peer => self.peer_device,
        }
    }

    pub fn source_is_local(&self) -> bool {
        self.source == self.local_device
    }

    pub fn matches(&self, fresh: &Self) -> bool {
        self.local_fingerprint == fresh.local_fingerprint
            && self.peer_fingerprint == fresh.peer_fingerprint
            && self.source == fresh.source
            && self.local_device == fresh.local_device
            && self.peer_device == fresh.peer_device
            && self.local_platform == fresh.local_platform
            && self.peer_platform == fresh.peer_platform
            && self.interface_id == fresh.interface_id
            && self.local_displays.same_geometry(&fresh.local_displays)
            && self.peer_displays.same_geometry(&fresh.peer_displays)
    }

    /// `hidden` names displays marked not in use: they stay in the topology, not in use and with
    /// no adjacency, so the pointer treats them as space past the edge of the display it left.
    /// `placed` gives picture positions that replace OS origins for the computer whose cursor
    /// MonHop moves; the input computer's OS geometry always stands.
    pub fn topology(
        &self,
        links: Vec<EdgeLink>,
        hidden: &[DisplayId],
        placed: &[(DisplayId, Point)],
    ) -> Result<Topology, SetupFailure> {
        if links.len() > 64 {
            return Err(SetupFailure::Layout);
        }
        let displays: Vec<_> = [
            (&self.local_displays, self.local_device),
            (&self.peer_displays, self.peer_device),
        ]
        .into_iter()
        .flat_map(|(topology, device)| {
            topology.displays().iter().map(move |display| {
                monhop_core::Display::new(
                    display.id,
                    device,
                    display.name.clone(),
                    monhop_core::NativeSize::new(display.native_width, display.native_height),
                    monhop_core::LogicalSize::new(display.logical_size.x, display.logical_size.y),
                    display.logical_origin,
                    f64::from(display.scale_factor),
                    None,
                    display.is_primary,
                )
                .with_in_use(!hidden.contains(&display.id))
            })
        })
        .collect();
        let placed: Vec<(DisplayId, Point)> = placed
            .iter()
            .filter(|(id, _)| {
                displays
                    .iter()
                    .any(|display| display.id == *id && display.machine != self.source)
            })
            .copied()
            .collect();
        let links =
            crate::display_arrangement::inherit_display_edges(&displays, links, hidden, &placed)
                .map_err(|_| SetupFailure::Layout)?;
        if links.len() > 64 {
            return Err(SetupFailure::Layout);
        }
        Topology::new(
            vec![
                Machine::new(self.local_device, self.local_platform),
                Machine::new(self.peer_device, self.peer_platform),
            ],
            displays,
            links,
        )
        .map_err(|_| SetupFailure::Layout)
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;

    #[test]
    fn revoked_authorization_is_rejected() {
        let cancel = RevocationSignal::default();
        cancel.revoke();
        assert_eq!(check_cancel(&cancel), Err(SetupFailure::Cancelled));
    }

    #[test]
    fn cancellation_reaches_endpoint_without_polling_an_async_runtime() {
        let cancel = RevocationSignal::default();
        let (sent, received) = mpsc::channel();
        let watch =
            CancellationWatch::start(cancel.clone(), RevocationSignal::default(), move || {
                sent.send(()).unwrap();
            })
            .unwrap();
        cancel.revoke();
        received.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(watch);
    }

    #[test]
    fn a_deliberate_cancel_stops_the_session_first_and_revokes_after_the_grace() {
        let cancel = RevocationSignal::default();
        let native = RevocationSignal::default();
        let (sent, received) = mpsc::channel();
        let watch = CancellationWatch::start(cancel.clone(), native.clone(), move || {
            sent.send(std::time::Instant::now()).unwrap();
        })
        .unwrap();
        let cancelled_at = std::time::Instant::now();
        cancel.revoke();
        let revoked_at = received.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(revoked_at.duration_since(cancelled_at) >= STOP_GRACE);
        assert!(native.is_revoked());
        drop(watch);
    }

    #[test]
    fn a_native_stop_request_stops_the_session_first_and_revokes_after_the_grace() {
        let cancel = RevocationSignal::default();
        let native = RevocationSignal::default();
        let (sent, received) = mpsc::channel();
        let watch = CancellationWatch::start(cancel.clone(), native.clone(), move || {
            sent.send(std::time::Instant::now()).unwrap();
        })
        .unwrap();
        let requested_at = std::time::Instant::now();
        native.request_stop();
        let deadline = std::time::Instant::now() + Duration::from_millis(60);
        while !cancel.is_stopping() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(cancel.is_stopping());
        assert!(!cancel.is_revoked());
        let revoked_at = received.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(revoked_at.duration_since(requested_at) >= STOP_GRACE);
        assert!(native.is_revoked());
        assert!(cancel.is_revoked());
        drop(watch);
    }

    #[test]
    fn a_deliberate_cancel_requests_the_stop_before_the_grace_ends() {
        let cancel = RevocationSignal::default();
        let native = RevocationSignal::default();
        let watch = CancellationWatch::start(cancel.clone(), native.clone(), || {}).unwrap();
        cancel.revoke();
        let deadline = std::time::Instant::now() + Duration::from_millis(60);
        while !native.is_stopping() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(native.is_stopping());
        assert!(!native.is_revoked());
        drop(watch);
    }

    #[test]
    fn atomic_native_power_revocation_reaches_endpoint_and_local_cancel_off_callback() {
        let cancel = RevocationSignal::default();
        let native = RevocationSignal::default();
        let observer = native.clone();
        let (sent, received) = mpsc::channel();
        let caller = std::thread::current().id();
        let watch = CancellationWatch::start(cancel.clone(), native.clone(), move || {
            assert_ne!(std::thread::current().id(), caller);
            assert!(observer.is_revoked());
            sent.send(()).unwrap();
        })
        .unwrap();
        native.mark_revoked_without_wake();
        received.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(cancel.is_revoked());
        drop(watch);
        assert!(native.is_revoked());
    }

    #[test]
    fn completed_watch_joins_before_intentional_endpoint_revocation() {
        let (sent, received) = mpsc::channel();
        let cancel = RevocationSignal::default();
        let native = RevocationSignal::default();
        let watch = CancellationWatch::start(cancel.clone(), native.clone(), move || {
            let _ = sent.send(());
        })
        .unwrap();
        // PairedSession fields use this order after the metadata drain and final active check.
        drop(watch);
        native.mark_revoked_without_wake();
        assert!(!cancel.is_revoked());
        assert!(matches!(
            received.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn cancellation_closes_an_incomplete_authenticated_handshake() {
        use crate::crypto::{
            DeviceIdentity, LOCAL_TLS_SERVER_NAME, SecureQuicConfig, VerifiedPeer,
        };
        use monhop_core::{DisplayId, Point};
        use monhop_protocol::DisplayDescription;
        let client_identity = DeviceIdentity::generate().unwrap();
        let server_identity = DeviceIdentity::generate().unwrap();
        let pin = |identity: &DeviceIdentity| {
            VerifiedPeer::from_certificate_der(
                identity.certificate_der(),
                &identity.fingerprint().full_hex(),
            )
            .unwrap()
        };
        let server_pin = pin(&server_identity);
        let client_pin = pin(&client_identity);
        let local: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = quinn::Endpoint::server(
            SecureQuicConfig::server(&server_identity, &client_pin).unwrap(),
            local,
        )
        .unwrap();
        let mut client = quinn::Endpoint::client(local).unwrap();
        client.set_default_client_config(
            SecureQuicConfig::client(&client_identity, &server_pin).unwrap(),
        );
        let connecting = client
            .connect(server.local_addr().unwrap(), LOCAL_TLS_SERVER_NAME)
            .unwrap();
        let (client_connection, _server_connection) =
            tokio::time::timeout(Duration::from_secs(2), async {
                tokio::join!(async { connecting.await.unwrap() }, async {
                    server.accept().await.unwrap().await.unwrap()
                })
            })
            .await
            .unwrap();
        let displays = DisplayTopology::new(vec![DisplayDescription {
            id: DisplayId(1),
            name: "fixture".into(),
            native_width: 100,
            native_height: 100,
            logical_origin: Point::default(),
            logical_size: Point::new(100.0, 100.0),
            scale_factor: 1.0,
            is_primary: true,
            monitor: None,
        }])
        .unwrap();
        let capabilities =
            Capabilities::new(Capabilities::RELATIVE_MOTION | Capabilities::DISPLAY_TOPOLOGY)
                .unwrap();
        let config = HandshakeConfig::new(
            &client_identity,
            &server_pin,
            Platform::Windows,
            Platform::MacOs,
            capabilities,
            capabilities,
            &displays,
            device_id_from_fingerprint(client_identity.fingerprint()),
            SessionPurpose::Inspect,
        )
        .unwrap();
        let cancel = RevocationSignal::default();
        let endpoint = client.clone();
        let watch =
            CancellationWatch::start(cancel.clone(), RevocationSignal::default(), move || {
                endpoint.close(0_u32.into(), b"fixture cancelled")
            })
            .unwrap();
        let cancellation = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            cancel.revoke();
        });
        let result = negotiate(client_connection, config).await;
        assert!(matches!(
            result,
            Err(crate::session_handshake::HandshakeError::Stream)
        ));
        cancellation.join().unwrap();
        drop(watch);
        server.close(0_u32.into(), b"fixture complete");
    }
}

#[cfg(test)]
mod retry_policy_tests {
    use super::*;

    #[test]
    fn a_disagreement_ends_the_dial_while_reach_failures_keep_it_going() {
        assert!(!is_transient(SetupFailure::PurposeMismatch));
        assert!(!is_transient(SetupFailure::VersionMismatch));
        assert!(!is_transient(SetupFailure::ChangedSinceInspection));
        assert!(!is_transient(SetupFailure::Cancelled));
        assert!(is_transient(SetupFailure::Connection));
        assert!(is_transient(SetupFailure::Handshake));
        assert!(is_transient(SetupFailure::PortBusy));
    }

    #[test]
    fn a_purpose_disagreement_reaches_the_caller_from_the_handshake() {
        assert_eq!(
            handshake_failure(HandshakeError::PurposeMismatch),
            SetupFailure::PurposeMismatch
        );
        // Two sessions whose records name different keyboard sides: the records must be agreed
        // again, so this is the stale-record answer rather than a retryable reach failure.
        assert_eq!(
            handshake_failure(HandshakeError::SourceMismatch),
            SetupFailure::ChangedSinceInspection
        );
        // A peer whose hello names another build: the answer says so, and dialing cannot fix it.
        assert_eq!(
            handshake_failure(HandshakeError::PeerHelloMismatch),
            SetupFailure::VersionMismatch
        );
        assert!(!is_transient(SetupFailure::VersionMismatch));
    }
}
