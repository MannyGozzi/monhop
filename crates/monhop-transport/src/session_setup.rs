//! Explicit paired metadata inspection and fresh session checks before input starts.

use std::{
    cell::RefCell,
    future::Future,
    net::{Ipv4Addr, SocketAddrV4},
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use monhop_core::{
    DeviceId, DisplayId, EdgeLink, Machine, Platform, Point, RevocationSignal, Topology,
};
use monhop_protocol::{Capabilities, ControlPermissions};
use tokio::sync::oneshot;

pub use monhop_protocol::{
    DisplayDescription, DisplayTopology, MAX_DISPLAY_NAME_BYTES, MAX_LOGICAL_ORIGIN_ABS,
    MAX_LOGICAL_SIZE, MAX_NATIVE_DIMENSION, MAX_SCALE_FACTOR, MIN_SCALE_FACTOR,
};

use crate::{
    crypto::{CertificateFingerprint, DeviceIdentity, VerifiedPeer},
    guarded_endpoint::{
        Accepted, Acceptor, EndpointHandle, GroupMember, GroupSelection, GuardedEndpoint,
        MemberHandshakeFailure, NetworkSelection, refused_by_this_computer,
    },
    identity_store::{ProtectedPeerStore, load_identity},
    native_storage::{NativeIdentityStore, NativePeerStore},
    pairing::{ConfirmedPeerRecord, PAIRING_PORT, initiates_connection, opposite_platform},
    policy::{InterfaceKind, InterfaceSnapshot, MAX_PINNED_PEERS, validate_peer},
    session::NETWORK_REVOKED_REASON,
    session_handshake::{
        HandshakeConfig, HandshakeError, NegotiatedSession, SessionPurpose,
        device_id_from_fingerprint, negotiate,
    },
    session_native::current_displays,
};

/// The whole rendezvous for one explicit local action, unchanged by the dial retry below.
const CONNECT_WINDOW: Duration = Duration::from_secs(120);
/// One Windows dial attempt covers connect plus negotiate; the Mac may not be listening yet.
const DIAL_ATTEMPT_DEADLINE: Duration = Duration::from_secs(4);
const DIAL_INTERVAL: Duration = Duration::from_secs(2);
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupFailure {
    Identity,
    PairingRequired,
    NetworkSelection,
    NetworkRoute,
    /// The pinned port is still closing from a previous session on this computer.
    PortBusy,
    Connection,
    /// The other computer presented an identity other than the paired one; pairing again fixes it.
    PeerIdentityChanged,
    Handshake,
    Cancelled,
    Displays,
    ChangedSinceInspection,
    Layout,
    PurposeMismatch,
    VersionMismatch,
}

#[derive(Clone)]
pub struct InspectedPeer {
    pub local_device: DeviceId,
    pub peer_device: DeviceId,
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

/// A sharing connection with its reusable pinned endpoint lease.
pub type PairedShare = PairedSession;

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

    /// True once the watch has handed a revocation on and exited, which a stopping signal
    /// reaches within `STOP_GRACE`.
    fn finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
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

/// The id a chosen network is saved under.
pub fn interface_id(stable_id: &str, index: u32, address: Ipv4Addr) -> String {
    format!("{stable_id}:{index}:{address}")
}

/// Whether a saved network id names this adapter. The index is not compared: an adapter that
/// resets can come back renumbered with the same stable id and address.
pub fn names_adapter(interface_id: &str, stable_id: &str, address: Ipv4Addr) -> bool {
    interface_id
        .strip_prefix(stable_id)
        .and_then(|rest| rest.strip_prefix(':'))
        .and_then(|rest| rest.split_once(':'))
        .is_some_and(|(index, saved)| {
            index.parse::<u32>().is_ok() && saved.parse::<Ipv4Addr>() == Ok(address)
        })
}

pub fn selected_network(interface_id: &str) -> Result<NetworkSelection, SetupFailure> {
    selected_adapter(interface_id).map(|(selection, _)| selection)
}

/// The one adapter a saved network id names, fit to share over, and the facts its subnet checks
/// read.
fn selected_adapter(
    interface_id: &str,
) -> Result<(NetworkSelection, InterfaceSnapshot), SetupFailure> {
    #[cfg(windows)]
    let adapters = monhop_platform_windows::network::enumerate_adapters()
        .map_err(|_| SetupFailure::NetworkSelection)?;
    #[cfg(target_os = "macos")]
    let adapters = monhop_platform_macos::network::enumerate_adapters_with_attachment()
        .map_err(|_| SetupFailure::NetworkSelection)?;
    let mut matches = adapters
        .into_iter()
        .filter(|adapter| names_adapter(interface_id, &adapter.stable_id, adapter.address));
    let adapter = matches.next().ok_or(SetupFailure::NetworkSelection)?;
    if matches.next().is_some()
        || !adapter.physical
        || !adapter.up
        || !(adapter.wifi || adapter.ethernet)
        || adapter.attachment.is_none()
    {
        return Err(SetupFailure::NetworkSelection);
    }
    let snapshot = InterfaceSnapshot {
        stable_id: adapter.stable_id.clone(),
        name: adapter.name,
        index: adapter.index,
        address: adapter.address,
        prefix_len: adapter.prefix_len,
        kind: if adapter.wifi {
            InterfaceKind::WiFi
        } else {
            InterfaceKind::Ethernet
        },
        is_hardware: adapter.physical,
        is_up: adapter.up,
        network_signature: adapter.attachment.unwrap_or_default(),
    };
    Ok((
        NetworkSelection {
            stable_id: adapter.stable_id,
            interface_index: adapter.index,
            local: SocketAddrV4::new(adapter.address, PAIRING_PORT),
            peer: SocketAddrV4::new(adapter.address, PAIRING_PORT),
        },
        snapshot,
    ))
}

const fn this_platform() -> Platform {
    #[cfg(windows)]
    let platform = Platform::Windows;
    #[cfg(target_os = "macos")]
    let platform = Platform::MacOs;
    platform
}

fn stored_identity() -> Result<DeviceIdentity, SetupFailure> {
    load_identity(&NativeIdentityStore)
        .map_err(|error| {
            log::warn!("session setup: the local identity could not be loaded: {error}");
            SetupFailure::Identity
        })?
        .ok_or(SetupFailure::PairingRequired)
}

fn bind_failure(error: std::io::Error) -> SetupFailure {
    if error.kind() == std::io::ErrorKind::AddrInUse {
        SetupFailure::PortBusy
    } else {
        SetupFailure::NetworkRoute
    }
}

/// A bound, interface-pinned endpoint that can dial or accept repeatedly without rebinding.
///
/// Binding loads the stored identity and the one confirmed peer it is for. Every connection made
/// from it re-enumerates local displays so a negotiated session never carries stale geometry.
pub(crate) struct PreparedEndpoint {
    // Intentional endpoint teardown must not feed back into a successful local action.
    cancellation: CancellationWatch,
    identity: DeviceIdentity,
    peer: VerifiedPeer,
    endpoint: GuardedEndpoint,
    revocation: RevocationSignal,
    local_device: DeviceId,
    peer_device: DeviceId,
    local_platform: Platform,
    peer_platform: Platform,
    control: ControlPermissions,
    dials: bool,
    interface_id: String,
}

/// Binds the one selected interface for repeated use.
///
/// Control permissions bind every sharing connection. Setup always carries BOTH.
pub(crate) fn prepare_endpoint(
    interface_id: &str,
    control: ControlPermissions,
    cancel: &RevocationSignal,
    peer_fingerprint: CertificateFingerprint,
) -> Result<PreparedEndpoint, SetupFailure> {
    check_cancel(cancel)?;
    let identity = stored_identity()?;
    check_cancel(cancel)?;
    let record = confirmed_peer(&NativePeerStore, identity.fingerprint(), peer_fingerprint)?;
    let peer = record
        .peer()
        .verified_peer()
        .map_err(|_| SetupFailure::PairingRequired)?;
    let local_device = device_id_from_fingerprint(identity.fingerprint());
    let peer_device = device_id_from_fingerprint(peer.fingerprint());
    let local_platform = this_platform();
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
    check_cancel(cancel)?;
    let mut selection = selected_network(interface_id)?;
    selection.peer = record.peer().endpoint();
    check_cancel(cancel)?;
    let endpoint = GuardedEndpoint::bind_after_local_enable(selection, &identity, &peer)
        .map_err(bind_failure)?;
    check_cancel(cancel)?;
    let revoker = endpoint.revoker();
    // Cancellation must still close the socket while native startup blocks its async runtime.
    let revocation = endpoint.revocation_signal();
    let cancellation =
        CancellationWatch::start(cancel.clone(), revocation.clone(), move || revoker.revoke())?;
    Ok(PreparedEndpoint {
        cancellation,
        identity,
        peer,
        endpoint,
        revocation,
        local_device,
        peer_device,
        local_platform,
        peer_platform,
        control,
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
    confirmed_peers(store, local)?
        .into_iter()
        .find(|record| record.peer().fingerprint() == peer)
        .ok_or(SetupFailure::PairingRequired)
}

/// Every pairing that still decodes for the local identity.
fn confirmed_peers(
    store: &impl ProtectedPeerStore,
    local: CertificateFingerprint,
) -> Result<Vec<ConfirmedPeerRecord>, SetupFailure> {
    Ok(store
        .list()
        .map_err(|error| {
            log::warn!("session setup: the paired computers could not be listed: {error}");
            SetupFailure::Identity
        })?
        .into_iter()
        .filter_map(|stored| ConfirmedPeerRecord::decode(&stored.record, local).ok())
        .collect())
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
            note_attempt_failure(format!("share dial could not start: {error}"));
            SetupFailure::Connection
        })?;
        self.while_active(cancel, async move {
            connecting.await.map_err(|error| {
                connection_failure("share dial failed", &error, Some(&error), || {
                    self.endpoint
                        .take_refused_certificate(self.peer.fingerprint())
                })
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
            self.endpoint
                .accept()
                .await
                .map_err(|error| incoming_failure("share accept failed", &error))
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
        let pair = PairFacts {
            identity: &self.identity,
            peer: &self.peer,
            local_device: self.local_device,
            peer_device: self.peer_device,
            local_platform: self.local_platform,
            peer_platform: self.peer_platform,
            control: self.control,
            interface_id: &self.interface_id,
        };
        negotiate_pair(pair, connection, purpose, None, local_displays, || {
            cancel_or_endpoint_revoked(None, &self.endpoint)
        })
        .await
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

/// Both computers' facts for one pair's session handshake.
struct PairFacts<'a> {
    identity: &'a DeviceIdentity,
    peer: &'a VerifiedPeer,
    local_device: DeviceId,
    peer_device: DeviceId,
    local_platform: Platform,
    peer_platform: Platform,
    /// Share control; a setup link always carries BOTH.
    control: ControlPermissions,
    interface_id: &'a str,
}

/// Negotiates `purpose` on an authenticated connection and reports fresh peer facts. A share
/// session with an `agreement` fails unless the other computer's agreement is the same.
async fn negotiate_pair(
    pair: PairFacts<'_>,
    connection: quinn::Connection,
    purpose: SessionPurpose,
    agreement: Option<[u8; 32]>,
    local_displays: DisplayTopology,
    endpoint_revoked: impl Fn() -> bool,
) -> Result<(NegotiatedSession, InspectedPeer), SetupFailure> {
    let capabilities =
        Capabilities::new(Capabilities::RELATIVE_MOTION | Capabilities::DISPLAY_TOPOLOGY)
            .map_err(|_| SetupFailure::Handshake)?;
    let config = HandshakeConfig::new(
        pair.identity,
        pair.peer,
        pair.local_platform,
        pair.peer_platform,
        capabilities,
        capabilities,
        &local_displays,
        if purpose == SessionPurpose::Setup {
            ControlPermissions::BOTH
        } else {
            pair.control
        },
        purpose,
    )
    .map_err(|_| SetupFailure::Handshake)?;
    let config = match agreement {
        Some(agreement) => config.with_agreement(agreement),
        None => config,
    };
    // A pin refused after the client finished its handshake surfaces here, as the close.
    let closing = connection.clone();
    let session =
        negotiate(connection, config)
            .await
            .map_err(|error| match closing.close_reason() {
                Some(reason) if refused_identity(&reason) => {
                    let refusal = if refused_by_this_computer(&reason) {
                        Refusal::Ours(None)
                    } else {
                        Refusal::Theirs
                    };
                    identity_changed("share handshake closed", &reason, refusal)
                }
                _ => handshake_failure(error),
            })?;
    if endpoint_revoked() {
        return Err(SetupFailure::Cancelled);
    }
    let inspection = InspectedPeer {
        local_device: pair.local_device,
        peer_device: pair.peer_device,
        local_fingerprint: pair.identity.fingerprint(),
        peer_fingerprint: pair.peer.fingerprint(),
        local_platform: pair.local_platform,
        peer_platform: pair.peer_platform,
        local_displays,
        peer_displays: session.peer.topology.clone(),
        interface_id: pair.interface_id.to_owned(),
    };
    Ok((session, inspection))
}

/// A pin refusal on a connection whose handshake finished: this computer's, or the peer's close
/// refusing this computer's certificate, which the finished handshake authenticates. Before that,
/// only [`refused_by_this_computer`] counts, since anyone who sees the handshake can send a close.
pub(crate) fn refused_identity(error: &quinn::ConnectionError) -> bool {
    use rustls::AlertDescription;
    let alert =
        |description: AlertDescription| quinn::TransportErrorCode::crypto(description.into());
    refused_by_this_computer(error)
        || matches!(
            error,
            quinn::ConnectionError::ConnectionClosed(peer)
                if peer.error_code == alert(AlertDescription::AccessDenied)
                    || peer.error_code == alert(AlertDescription::BadCertificate)
        )
}

/// A dial or incoming handshake that failed before it authenticated the peer: only this
/// computer's pin refusing the presented certificate says the peer's identity changed.
fn connection_failure(
    what: &str,
    error: &dyn std::fmt::Display,
    cause: Option<&quinn::ConnectionError>,
    refused: impl FnOnce() -> Option<CertificateFingerprint>,
) -> SetupFailure {
    match cause {
        Some(cause) if refused_by_this_computer(cause) => {
            identity_changed(what, error, Refusal::Ours(refused()))
        }
        _ => {
            note_attempt_failure(format!("{what}: {error}"));
            SetupFailure::Connection
        }
    }
}

/// The endpoint wraps one member's failed incoming handshake in an I/O error.
fn incoming_failure(what: &str, error: &std::io::Error) -> SetupFailure {
    match error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<MemberHandshakeFailure>())
    {
        Some(failure) => member_failure(what, failure),
        None => connection_failure(what, error, None, || None),
    }
}

fn member_failure(what: &str, failure: &MemberHandshakeFailure) -> SetupFailure {
    connection_failure(what, failure, failure.cause.as_ref(), || failure.refused)
}

/// Which side's pin refused a certificate.
#[derive(Clone, Copy)]
enum Refusal {
    /// This computer's, with the certificate it refused when that was recorded.
    Ours(Option<CertificateFingerprint>),
    /// The authenticated peer's, refusing this computer's certificate.
    Theirs,
}

/// Warns once per refused identity with its short fingerprint; repeats go to the collapsed DEBUG
/// log.
fn identity_changed(what: &str, error: &dyn std::fmt::Display, refusal: Refusal) -> SetupFailure {
    let line = format!("{what}: {error}");
    let mut log = ATTEMPT_LOG.lock().unwrap_or_else(PoisonError::into_inner);
    let now = Instant::now();
    let refused = match refusal {
        Refusal::Ours(refused) => refused,
        Refusal::Theirs => None,
    };
    if log.first_refusal(refused) {
        match refusal {
            Refusal::Ours(Some(certificate)) => log::warn!(
                "session setup: the other computer presented certificate {}, not the paired \
                 identity; pairing the computers again fixes this",
                certificate.short_hex()
            ),
            Refusal::Ours(None) => log::warn!(
                "session setup: the other computer presented a certificate other than the paired \
                 identity; pairing the computers again fixes this"
            ),
            Refusal::Theirs => log::warn!(
                "session setup: the other computer refused this computer's identity; pairing the \
                 computers again fixes this"
            ),
        }
        // The warning stands for this line's first occurrence.
        let _ = log.repeated(line, now);
    } else if let Some(line) = log.repeated(line, now) {
        log::debug!("{line}");
    }
    SetupFailure::PeerIdentityChanged
}

fn note_attempt_failure(line: String) {
    let collapsed = ATTEMPT_LOG
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .repeated(line, Instant::now());
    if let Some(line) = collapsed {
        log::debug!("{line}");
    }
}

/// Identical attempt failures repeat every few seconds for as long as the peer stays wrong.
const REPEAT_LOG_INTERVAL: Duration = Duration::from_secs(60);

static ATTEMPT_LOG: Mutex<AttemptLog> = Mutex::new(AttemptLog::new());

/// Keeps a failing retry loop from rotating every earlier line out of the bounded log.
struct AttemptLog {
    line: Option<String>,
    logged_at: Option<Instant>,
    repeats: u64,
    /// The last refusal warned about: the refused certificate, or None when the peer refused ours.
    warned: Option<Option<CertificateFingerprint>>,
}

impl AttemptLog {
    const fn new() -> Self {
        Self {
            line: None,
            logged_at: None,
            repeats: 0,
            warned: None,
        }
    }

    /// The line to log now, if any: a run's first line, then one a minute carrying its count.
    fn repeated(&mut self, line: String, now: Instant) -> Option<String> {
        if self.line.as_deref() == Some(line.as_str()) {
            self.repeats += 1;
            if self
                .logged_at
                .is_some_and(|at| now.duration_since(at) < REPEAT_LOG_INTERVAL)
            {
                return None;
            }
            let summary = format!(
                "{line} (repeated {} times since the last report)",
                self.repeats
            );
            self.repeats = 0;
            self.logged_at = Some(now);
            return Some(summary);
        }
        let summary = match self.repeats {
            0 => line.clone(),
            unreported => format!("{line} (the previous failure repeated {unreported} more times)"),
        };
        self.line = Some(line);
        self.logged_at = Some(now);
        self.repeats = 0;
        Some(summary)
    }

    fn first_refusal(&mut self, refused: Option<CertificateFingerprint>) -> bool {
        let first = self.warned != Some(refused);
        self.warned = Some(refused);
        first
    }
}

/// Distinguishes a peer that answered but disagrees from a peer that could not be reached.
pub(crate) const fn handshake_failure(error: HandshakeError) -> SetupFailure {
    match error {
        HandshakeError::PurposeMismatch => SetupFailure::PurposeMismatch,
        // A changed saved control map, or a changed saved layout agreement, must be agreed over
        // the setup link rather than retried as a share attempt.
        HandshakeError::ControlMismatch | HandshakeError::AgreementMismatch => {
            SetupFailure::ChangedSinceInspection
        }
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

/// A failed window hands the endpoint back so the next attempt needs no new identity read.
async fn connect_prepared(
    prepared: PreparedEndpoint,
    cancel: &RevocationSignal,
    purpose: SessionPurpose,
) -> Result<PairedSession, Box<(PreparedEndpoint, SetupFailure)>> {
    // Start order must not decide a test window or a share, and only the dialing side can retry.
    let retry_dial = prepared.dials() && purpose == SessionPurpose::Share;
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

/// The longest a retiring endpoint may hold its pinned port; one that hangs leaves the next bind
/// to report the port as busy.
const RETIRED_ENDPOINT_DRAIN: Duration = Duration::from_secs(3);

/// An endpoint a standing share can keep from one session to the next.
trait Standing: Sized {
    fn signal(&self) -> &RevocationSignal;
    /// Lets final packets leave, then returns once the pinned port can be bound again.
    async fn retire(self);
}

impl Standing for PreparedEndpoint {
    fn signal(&self) -> &RevocationSignal {
        &self.revocation
    }

    async fn retire(self) {
        let closed = self.endpoint.socket_closed();
        let retired = async move {
            let _ = self.endpoint.close_and_wait_idle().await;
            // The watch turns this stop into a revoke and cancels the worker; dropping it sooner
            // would leave that to timing.
            let mut check = tokio::time::interval(Duration::from_millis(10));
            while !self.cancellation.finished() {
                check.tick().await;
            }
            drop(self);
            closed.await;
        };
        if tokio::time::timeout(RETIRED_ENDPOINT_DRAIN, retired)
            .await
            .is_err()
        {
            log::warn!(
                "session setup: a retired endpoint still held its port after {RETIRED_ENDPOINT_DRAIN:?}"
            );
        }
    }
}

/// The one endpoint a standing share keeps between sessions.
struct StandingSlot<E> {
    held: Option<E>,
}

impl<E: Standing> StandingSlot<E> {
    const fn new() -> Self {
        Self { held: None }
    }

    /// A stop request is permanent on the signal, so the next session on it would end at once.
    fn serves_again(endpoint: &E) -> bool {
        !endpoint.signal().is_stopping()
    }

    /// The pinned port has no address reuse: a retired endpoint lets go of it before `bind` runs.
    async fn take_or_bind(
        &mut self,
        bind: impl FnOnce() -> Result<E, SetupFailure>,
    ) -> Result<E, SetupFailure> {
        if let Some(held) = self.held.take() {
            if Self::serves_again(&held) {
                return Ok(held);
            }
            held.retire().await;
        }
        bind()
    }

    async fn keep(&mut self, endpoint: E) {
        if Self::serves_again(&endpoint) {
            self.held = Some(endpoint);
        } else {
            endpoint.retire().await;
        }
    }

    /// Holds without judging it; the next `take_or_bind` retires it if it cannot serve.
    fn hold(&mut self, endpoint: E) {
        self.held = Some(endpoint);
    }
}

/// One bound share endpoint kept across attempts and sessions: the protected identity is read
/// once while the switch stays on, so the OS asks for the Keychain at most once, and a dropped
/// session reconnects on the same socket without a rebind.
pub struct StandingShareEndpoint {
    interface_id: String,
    control: ControlPermissions,
    peer: CertificateFingerprint,
    slot: StandingSlot<PreparedEndpoint>,
}

impl StandingShareEndpoint {
    pub fn new(
        interface_id: &str,
        peer: CertificateFingerprint,
        control: ControlPermissions,
    ) -> Self {
        Self {
            interface_id: interface_id.to_owned(),
            control,
            peer,
            slot: StandingSlot::new(),
        }
    }

    /// An endpoint whose session was asked to stop (network change, cancellation, a native stop)
    /// is retired first. Its watch has cancelled this worker by then, so the next worker binds.
    pub async fn connect(
        &mut self,
        cancel: &RevocationSignal,
    ) -> Result<PairedShare, SetupFailure> {
        let prepared = self
            .slot
            .take_or_bind(|| prepare_endpoint(&self.interface_id, self.control, cancel, self.peer))
            .await?;
        match connect_prepared(prepared, cancel, SessionPurpose::Share).await {
            Ok(paired) => Ok(paired),
            Err(failed) => {
                let (prepared, error) = *failed;
                self.slot.hold(prepared);
                Err(error)
            }
        }
    }

    /// Takes an ended session's endpoint back to serve the next connection, unless its session was
    /// asked to stop, in which case it lets go of the port before the rebind.
    pub async fn reclaim(&mut self, lease: EndpointLease) {
        let EndpointLease { prepared } = lease;
        self.slot.keep(prepared).await;
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
            // The dial or the handshake already logged why.
            Ok(Err(_)) => {}
            Err(_) => note_attempt_failure(format!(
                "share dial attempt passed {DIAL_ATTEMPT_DEADLINE:?}"
            )),
        }
        check_cancel(cancel)?;
        tokio::time::sleep(DIAL_INTERVAL).await;
        check_cancel(cancel)?;
    }
}

/// One computer's share control under the current group record.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct GroupMemberRecord {
    pub fingerprint: CertificateFingerprint,
    pub control: ControlPermissions,
}

/// A negotiated session with one computer on a group endpoint.
pub struct PairedMember {
    pub session: NegotiatedSession,
    pub inspection: InspectedPeer,
}

/// Close reasons for member connections the group endpoint ends itself.
const MEMBER_FORGOTTEN_REASON: &[u8] = b"pairing removed";
const MEMBER_SUPERSEDED_REASON: &[u8] = b"replaced by a newer connection";
const MEMBER_RELEASED_REASON: &[u8] = b"connection released";
const MEMBER_UNCLAIMED_REASON: &[u8] = b"no session is waiting for this computer";

/// What a group endpoint fixed at bind about one admitted computer.
struct GroupPeer {
    fingerprint: CertificateFingerprint,
    pin: VerifiedPeer,
    device: DeviceId,
    platform: Platform,
    /// The per-pair rule: exactly one of the two computers dials.
    dials: bool,
}

struct LocalFacts {
    identity: DeviceIdentity,
    device: DeviceId,
    platform: Platform,
}

impl LocalFacts {
    fn new(identity: DeviceIdentity, platform: Platform) -> Self {
        Self {
            device: device_id_from_fingerprint(identity.fingerprint()),
            identity,
            platform,
        }
    }
}

type DisplayReader = fn(DeviceId) -> Result<DisplayTopology, SetupFailure>;

fn native_displays(device: DeviceId) -> Result<DisplayTopology, SetupFailure> {
    current_displays(device).map_err(|_| SetupFailure::Displays)
}

/// O-1: every paired computer the selected network reaches directly, whatever group it is in.
/// Group members come first; one computer per address, and no more than one socket pins.
fn admitted_peers(
    selected: &InterfaceSnapshot,
    local: CertificateFingerprint,
    local_platform: Platform,
    records: &[ConfirmedPeerRecord],
    members: &[GroupMemberRecord],
) -> Vec<(GroupPeer, SocketAddrV4)> {
    let (grouped, others): (Vec<_>, Vec<_>) = records.iter().partition(|record| {
        members
            .iter()
            .any(|member| member.fingerprint == record.peer().fingerprint())
    });
    let mut admitted: Vec<(GroupPeer, SocketAddrV4)> = Vec::new();
    for record in grouped.into_iter().chain(others) {
        let offer = record.peer();
        let fingerprint = offer.fingerprint();
        let address = offer.endpoint();
        let pin = match offer.verified_peer() {
            Ok(pin)
                if fingerprint != local
                    && validate_peer(selected, *address.ip()).is_ok()
                    && !admitted.iter().any(|(peer, earlier)| {
                        peer.fingerprint == fingerprint || earlier.ip() == address.ip()
                    }) =>
            {
                pin
            }
            _ => {
                log::info!(
                    "group endpoint: paired computer {} is off the selected subnet or at an \
                     address already admitted; not admitted",
                    fingerprint.short_hex()
                );
                continue;
            }
        };
        if admitted.len() == MAX_PINNED_PEERS {
            log::warn!(
                "group endpoint: paired computer {} not admitted: one network admits at most \
                 {MAX_PINNED_PEERS}",
                fingerprint.short_hex()
            );
            continue;
        }
        let platform = offer.platform();
        admitted.push((
            GroupPeer {
                fingerprint,
                pin,
                device: device_id_from_fingerprint(fingerprint),
                platform: platform.unwrap_or_else(|| opposite_platform(local_platform)),
                dials: initiates_connection(local_platform, local, platform, fingerprint),
            },
            address,
        ));
    }
    admitted
}

type Delivery = Result<quinn::Connection, MemberHandshakeFailure>;

/// Which connect waits for each member's incoming connection. Shared with the accept router,
/// which runs on the same network runtime.
struct Routes {
    members: Box<[CertificateFingerprint]>,
    waiters: Box<[Option<Waiter>]>,
    next_ticket: u64,
}

struct Waiter {
    ticket: u64,
    deliver: oneshot::Sender<Delivery>,
}

impl Routes {
    fn new(members: Box<[CertificateFingerprint]>) -> Self {
        Self {
            waiters: members.iter().map(|_| None).collect(),
            members,
            next_ticket: 0,
        }
    }

    fn index(&self, member: CertificateFingerprint) -> Option<usize> {
        self.members.iter().position(|known| *known == member)
    }

    fn claimed(&self, member: CertificateFingerprint) -> bool {
        self.index(member)
            .and_then(|index| self.waiters[index].as_ref())
            .is_some_and(|waiter| !waiter.deliver.is_closed())
    }

    /// Makes this the one connect waiting for member `index`; an older one ends as cancelled.
    fn wait(&mut self, index: usize) -> (u64, oneshot::Receiver<Delivery>) {
        let (deliver, delivered) = oneshot::channel();
        self.next_ticket += 1;
        self.waiters[index] = Some(Waiter {
            ticket: self.next_ticket,
            deliver,
        });
        (self.next_ticket, delivered)
    }

    fn stop_waiting(&mut self, index: usize, ticket: u64) {
        if self.waiters[index]
            .as_ref()
            .is_some_and(|waiter| waiter.ticket == ticket)
        {
            self.waiters[index] = None;
        }
    }

    /// Hands `delivery` to the connect waiting for `member` only; a connection nobody waits for
    /// is closed.
    fn deliver(&mut self, member: CertificateFingerprint, delivery: Delivery) {
        let waiter = self
            .index(member)
            .and_then(|index| self.waiters[index].take());
        let unclaimed = match waiter {
            Some(waiter) => waiter.deliver.send(delivery).err(),
            None => Some(delivery),
        };
        match unclaimed {
            Some(Ok(connection)) => {
                log::debug!(
                    "group endpoint: no connect waits for {}; its connection is closed",
                    member.short_hex()
                );
                connection.close(0_u32.into(), MEMBER_UNCLAIMED_REASON);
            }
            Some(Err(failure)) => log::debug!("group endpoint: unclaimed failure: {failure}"),
            None => {}
        }
    }

    fn close(&mut self) {
        self.waiters.iter_mut().for_each(|waiter| *waiter = None);
    }
}

fn routes(routes: &Mutex<Routes>) -> MutexGuard<'_, Routes> {
    routes.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Withdraws one connect's claim on its member's incoming connection, unless a newer connect
/// already replaced it.
struct Waiting<'a> {
    routes: &'a Mutex<Routes>,
    index: usize,
    ticket: u64,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        routes(self.routes).stop_waiting(self.index, self.ticket);
    }
}

/// The one loop that owns the endpoint's incoming connections. A member no connect waits for is
/// refused before any TLS runs.
async fn route_incoming(mut acceptor: Acceptor, table: Arc<Mutex<Routes>>) {
    let claimed = |member| routes(&table).claimed(member);
    loop {
        let accepted = acceptor.accept_any(&claimed).await;
        let mut routed = routes(&table);
        match accepted {
            Ok(Accepted::Member(member, connection)) => routed.deliver(member, Ok(connection)),
            Ok(Accepted::Failed(failure)) => routed.deliver(failure.member, Err(failure)),
            Err(error) => {
                log::debug!("group endpoint: incoming connections stopped: {error}");
                routed.close();
                return;
            }
        }
    }
}

/// Stops the accept router when the endpoint goes.
struct Router(tokio::task::JoinHandle<()>);

impl Drop for Router {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One interface-pinned endpoint for every paired computer the selected network reaches, and the
/// single accept router handing each member's incoming connection to the connect waiting for it.
/// It lives on the network thread; one member's failure or teardown never touches another's
/// connection.
pub struct GroupEndpoint {
    // Dropped before the endpoint, so this endpoint's own teardown never reaches `cancel`.
    _cancellation: CancellationWatch,
    router: Router,
    routes: Arc<Mutex<Routes>>,
    /// Each member's latest connection handed out, kept so forget and a newer connection can
    /// close it. Dropping a session's handle therefore does not close its connection.
    live: RefCell<Box<[Option<quinn::Connection>]>>,
    controls: RefCell<Vec<GroupMemberRecord>>,
    peers: Box<[GroupPeer]>,
    local: LocalFacts,
    displays: DisplayReader,
    interface_id: String,
    revocation: RevocationSignal,
    handle: EndpointHandle,
    endpoint: GuardedEndpoint,
}

impl GroupEndpoint {
    /// Binds the selected interface for every paired computer whose recorded address is on its
    /// subnet, in the current group or not; `members` gives the group's share control, and an
    /// admitted computer without one can open only setup links. Must run on the network runtime,
    /// which then hosts the one accept router.
    pub fn bind(
        interface_id: &str,
        members: &[GroupMemberRecord],
        cancel: &RevocationSignal,
    ) -> Result<Rc<Self>, SetupFailure> {
        check_cancel(cancel)?;
        let identity = stored_identity()?;
        check_cancel(cancel)?;
        let records = confirmed_peers(&NativePeerStore, identity.fingerprint())?;
        check_cancel(cancel)?;
        let (selection, selected) = selected_adapter(interface_id)?;
        let local_platform = this_platform();
        let admitted = admitted_peers(
            &selected,
            identity.fingerprint(),
            local_platform,
            &records,
            members,
        );
        if admitted.is_empty() {
            log::warn!("group endpoint: no paired computer is on the selected network");
            return Err(if records.is_empty() {
                SetupFailure::PairingRequired
            } else {
                SetupFailure::NetworkRoute
            });
        }
        let group = GroupSelection {
            stable_id: selection.stable_id,
            interface_index: selection.interface_index,
            local: selection.local,
            members: admitted
                .iter()
                .map(|(peer, address)| GroupMember {
                    address: *address,
                    pin: peer.pin.clone(),
                })
                .collect(),
        };
        check_cancel(cancel)?;
        let endpoint = GuardedEndpoint::bind_group_after_local_enable(group, &identity)
            .map_err(bind_failure)?;
        check_cancel(cancel)?;
        Self::assemble(
            endpoint,
            LocalFacts::new(identity, local_platform),
            admitted.into_iter().map(|(peer, _)| peer).collect(),
            members,
            interface_id,
            cancel,
            native_displays,
        )
    }

    /// `peers` must be in the endpoint's member order.
    fn assemble(
        endpoint: GuardedEndpoint,
        local: LocalFacts,
        peers: Box<[GroupPeer]>,
        members: &[GroupMemberRecord],
        interface_id: &str,
        cancel: &RevocationSignal,
        displays: DisplayReader,
    ) -> Result<Rc<Self>, SetupFailure> {
        let acceptor = endpoint
            .take_acceptor()
            .map_err(|_| SetupFailure::NetworkRoute)?;
        let table = Arc::new(Mutex::new(Routes::new(
            peers.iter().map(|peer| peer.fingerprint).collect(),
        )));
        let router = Router(tokio::spawn(route_incoming(acceptor, table.clone())));
        let revoker = endpoint.revoker();
        // Cancellation must still close the socket while native startup blocks the runtime.
        let revocation = endpoint.revocation_signal();
        let cancellation =
            CancellationWatch::start(cancel.clone(), revocation.clone(), move || revoker.revoke())?;
        Ok(Rc::new(Self {
            _cancellation: cancellation,
            router,
            routes: table,
            live: RefCell::new(peers.iter().map(|_| None).collect()),
            controls: RefCell::new(members.to_vec()),
            peers,
            local,
            displays,
            interface_id: interface_id.to_owned(),
            revocation,
            handle: endpoint.handle(),
            endpoint,
        }))
    }

    /// Whether `member` is admitted here and not forgotten.
    pub fn admits(&self, member: CertificateFingerprint) -> bool {
        self.index(member).is_some() && self.handle.admits(member)
    }

    /// Replaces the share control of every computer from a new group record without a rebind.
    /// Sessions already negotiated keep the control they agreed.
    pub fn set_members(&self, members: &[GroupMemberRecord]) {
        *self.controls.borrow_mut() = members.to_vec();
    }

    pub fn revocation(&self) -> RevocationSignal {
        self.revocation.clone()
    }

    /// Dials `member` or waits for it, as the pair's dial rule says, then negotiates `purpose`.
    /// A share session carries the member's control and `agreement`, and a different agreement on
    /// the other computer is `ChangedSinceInspection`. Handing the session out closes the
    /// member's previous connection.
    pub async fn connect(
        &self,
        member: CertificateFingerprint,
        purpose: SessionPurpose,
        agreement: [u8; 32],
        cancel: &RevocationSignal,
    ) -> Result<PairedMember, SetupFailure> {
        let index = self.index(member).ok_or(SetupFailure::PairingRequired)?;
        self.check(index, Some(cancel))?;
        let control = if purpose == SessionPurpose::Setup {
            ControlPermissions::BOTH
        } else {
            // A computer outside the group record has no agreed control to share under.
            self.controls
                .borrow()
                .iter()
                .find(|record| record.fingerprint == member)
                .map(|record| record.control)
                .ok_or(SetupFailure::ChangedSinceInspection)?
        };
        let dials = self.peers[index].dials;
        // Start order must not decide a test window or a share, and only the dialing side retries.
        let retry_dial = dials && purpose == SessionPurpose::Share;
        let attempt = tokio::time::timeout(CONNECT_WINDOW, async {
            if retry_dial {
                return self
                    .dial_until_negotiated(index, control, purpose, agreement, cancel)
                    .await;
            }
            let connection = if dials {
                self.dial(index, cancel).await?
            } else {
                self.wait_for(index, cancel).await?
            };
            self.negotiate(index, connection, control, purpose, agreement)
                .await
        })
        .await;
        let Ok(established) = attempt else {
            log::debug!("share connect window of {CONNECT_WINDOW:?} passed without a session");
            return Err(SetupFailure::Connection);
        };
        let (session, inspection) = established?;
        // Forget runs on this thread, so nothing is handed out for a member it already removed.
        if let Err(error) = self.check(index, None) {
            let reason = if error == SetupFailure::PairingRequired {
                MEMBER_FORGOTTEN_REASON
            } else {
                NETWORK_REVOKED_REASON
            };
            session.connection.close(0_u32.into(), reason);
            return Err(error);
        }
        let previous = self.live.borrow_mut()[index].replace(session.connection.clone());
        if let Some(previous) = previous {
            previous.close(0_u32.into(), MEMBER_SUPERSEDED_REASON);
        }
        Ok(PairedMember {
            session,
            inspection,
        })
    }

    /// Closes `member`'s connection, leaving every other member's untouched.
    pub fn close_member(&self, member: CertificateFingerprint) {
        let connection = self
            .index(member)
            .and_then(|index| self.live.borrow_mut()[index].take());
        if let Some(connection) = connection {
            connection.close(0_u32.into(), MEMBER_RELEASED_REASON);
        }
    }

    /// Stops admitting `member` without a rebind: its waiting connect ends, its incoming is
    /// ignored and its connection closed. Only a new bind admits it again.
    pub fn forget(&self, member: CertificateFingerprint) {
        let Some(index) = self.index(member) else {
            return;
        };
        self.endpoint.forget_member(member);
        routes(&self.routes).waiters[index] = None;
        let connection = self.live.borrow_mut()[index].take();
        if let Some(connection) = connection {
            connection.close(0_u32.into(), MEMBER_FORGOTTEN_REASON);
        }
        log::info!(
            "group endpoint: forgot paired computer {}",
            member.short_hex()
        );
    }

    /// Closes this endpoint alone and waits, bounded, until its port can be bound again. Every
    /// other handle to it must already be dropped.
    pub async fn retire(self: Rc<Self>) {
        self.router.0.abort();
        let closed = self.endpoint.socket_closed();
        let retired = async move {
            let _ = self.endpoint.close_and_wait_idle().await;
            drop(self);
            closed.await;
        };
        if tokio::time::timeout(RETIRED_ENDPOINT_DRAIN, retired)
            .await
            .is_err()
        {
            log::warn!(
                "group endpoint: a retired endpoint still held its port after {RETIRED_ENDPOINT_DRAIN:?}"
            );
        }
    }

    fn index(&self, member: CertificateFingerprint) -> Option<usize> {
        self.peers
            .iter()
            .position(|peer| peer.fingerprint == member)
    }

    fn check(&self, index: usize, cancel: Option<&RevocationSignal>) -> Result<(), SetupFailure> {
        if !self.handle.admits(self.peers[index].fingerprint) {
            return Err(SetupFailure::PairingRequired);
        }
        if cancel_or_endpoint_revoked(cancel, &self.endpoint) {
            return Err(SetupFailure::Cancelled);
        }
        Ok(())
    }

    async fn while_member_active<T>(
        &self,
        index: usize,
        cancel: &RevocationSignal,
        work: impl Future<Output = Result<T, SetupFailure>>,
    ) -> Result<T, SetupFailure> {
        tokio::pin!(work);
        let mut tick = tokio::time::interval(CANCEL_POLL_INTERVAL);
        loop {
            tokio::select! {
                biased;
                _ = tick.tick() => self.check(index, Some(cancel))?,
                result = &mut work => return result,
            }
        }
    }

    /// Dials the member's recorded address only, and binds the finished connection to it.
    async fn dial(
        &self,
        index: usize,
        cancel: &RevocationSignal,
    ) -> Result<quinn::Connection, SetupFailure> {
        let member = self.peers[index].fingerprint;
        let what = format!("share dial to {} failed", member.short_hex());
        let unreached = |error: &dyn std::fmt::Display| {
            self.check(index, None).err().unwrap_or_else(|| {
                note_attempt_failure(format!("{what}: {error}"));
                SetupFailure::Connection
            })
        };
        let connecting = self
            .handle
            .connect_member(member)
            .map_err(|error| unreached(&error))?;
        let connection = self
            .while_member_active(index, cancel, async {
                connecting.await.map_err(|error| {
                    connection_failure(&what, &error, Some(&error), || {
                        self.handle.take_refused_certificate(member)
                    })
                })
            })
            .await?;
        self.handle
            .confirm_member(member, &connection)
            .map_err(|error| unreached(&error))?;
        Ok(connection)
    }

    /// Waits for the router to hand over the member's next incoming connection.
    async fn wait_for(
        &self,
        index: usize,
        cancel: &RevocationSignal,
    ) -> Result<quinn::Connection, SetupFailure> {
        let (ticket, delivered) = routes(&self.routes).wait(index);
        let _waiting = Waiting {
            routes: &self.routes,
            index,
            ticket,
        };
        let what = format!(
            "share accept from {} failed",
            self.peers[index].fingerprint.short_hex()
        );
        self.while_member_active(index, cancel, async {
            match delivered.await {
                Ok(Ok(connection)) => Ok(connection),
                Ok(Err(failure)) => Err(member_failure(&what, &failure)),
                // Forgotten, revoked, or replaced by a newer connect for this member.
                Err(_) => Err(self
                    .check(index, Some(cancel))
                    .err()
                    .unwrap_or(SetupFailure::Cancelled)),
            }
        })
        .await
    }

    async fn negotiate(
        &self,
        index: usize,
        connection: quinn::Connection,
        control: ControlPermissions,
        purpose: SessionPurpose,
        agreement: [u8; 32],
    ) -> Result<(NegotiatedSession, InspectedPeer), SetupFailure> {
        let peer = &self.peers[index];
        let local_displays = (self.displays)(self.local.device)?;
        let pair = PairFacts {
            identity: &self.local.identity,
            peer: &peer.pin,
            local_device: self.local.device,
            peer_device: peer.device,
            local_platform: self.local.platform,
            peer_platform: peer.platform,
            control,
            interface_id: &self.interface_id,
        };
        let agreement = (purpose == SessionPurpose::Share).then_some(agreement);
        negotiate_pair(pair, connection, purpose, agreement, local_displays, || {
            self.endpoint.is_revoked()
        })
        .await
    }

    async fn dial_until_negotiated(
        &self,
        index: usize,
        control: ControlPermissions,
        purpose: SessionPurpose,
        agreement: [u8; 32],
        cancel: &RevocationSignal,
    ) -> Result<(NegotiatedSession, InspectedPeer), SetupFailure> {
        loop {
            let attempt = tokio::time::timeout(DIAL_ATTEMPT_DEADLINE, async {
                let connection = self.dial(index, cancel).await?;
                self.negotiate(index, connection, control, purpose, agreement)
                    .await
            })
            .await;
            match attempt {
                Ok(Ok(established)) => return Ok(established),
                Ok(Err(error)) if !is_transient(error) => return Err(error),
                // The dial or the handshake already logged why.
                Ok(Err(_)) => {}
                Err(_) => note_attempt_failure(format!(
                    "share dial attempt to {} passed {DIAL_ATTEMPT_DEADLINE:?}",
                    self.peers[index].fingerprint.short_hex()
                )),
            }
            self.check(index, Some(cancel))?;
            tokio::time::sleep(DIAL_INTERVAL).await;
            self.check(index, Some(cancel))?;
        }
    }

    #[cfg(test)]
    fn waiting(&self, member: CertificateFingerprint) -> bool {
        routes(&self.routes).claimed(member)
    }
}

impl InspectedPeer {
    pub fn matches(&self, fresh: &Self) -> bool {
        self.local_fingerprint == fresh.local_fingerprint
            && self.peer_fingerprint == fresh.peer_fingerprint
            && self.local_device == fresh.local_device
            && self.peer_device == fresh.peer_device
            && self.local_platform == fresh.local_platform
            && self.peer_platform == fresh.peer_platform
            && self.interface_id == fresh.interface_id
            && self.local_displays.same_geometry(&fresh.local_displays)
            && self.peer_displays.same_geometry(&fresh.peer_displays)
    }

    /// Keeps local native geometry and translates the peer's entire display block.
    pub fn outbound_topology(
        &self,
        links: Vec<EdgeLink>,
        hidden: &[DisplayId],
        peer_offset: Point,
    ) -> Result<Topology, SetupFailure> {
        if links.len() > 64 || !peer_offset.is_finite() {
            return Err(SetupFailure::Layout);
        }
        let displays: Vec<_> = [
            (&self.local_displays, self.local_device, Point::default()),
            (&self.peer_displays, self.peer_device, peer_offset),
        ]
        .into_iter()
        .flat_map(|(topology, device, offset)| {
            topology.displays().iter().map(move |display| {
                monhop_core::Display::new(
                    display.id,
                    device,
                    display.name.clone(),
                    monhop_core::NativeSize::new(display.native_width, display.native_height),
                    monhop_core::LogicalSize::new(display.logical_size.x, display.logical_size.y),
                    Point::new(
                        display.logical_origin.x + offset.x,
                        display.logical_origin.y + offset.y,
                    ),
                    f64::from(display.scale_factor),
                    None,
                    display.is_primary,
                )
                .with_in_use(!hidden.contains(&display.id))
            })
        })
        .collect();
        if hidden
            .iter()
            .any(|id| !displays.iter().any(|display| display.id == *id))
        {
            return Err(SetupFailure::Layout);
        }
        let links = crate::display_arrangement::inherit_display_edges(&displays, links, hidden)
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
mod network_id_tests {
    use super::*;

    const ADDRESS: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 4);

    #[test]
    fn a_renumbered_adapter_is_still_the_saved_network() {
        let saved = interface_id("0123456789abcdef", 19, ADDRESS);
        assert!(names_adapter(&saved, "0123456789abcdef", ADDRESS));
    }

    #[test]
    fn another_adapter_or_address_is_not_the_saved_network() {
        let saved = interface_id("mac:a0b1c2d3e4f5", 4, ADDRESS);
        assert!(names_adapter(&saved, "mac:a0b1c2d3e4f5", ADDRESS));
        assert!(!names_adapter(&saved, "mac:a0b1c2d3e4f6", ADDRESS));
        assert!(!names_adapter(&saved, "mac:a0b1c2d3e4f", ADDRESS));
        assert!(!names_adapter(
            &saved,
            "mac:a0b1c2d3e4f5",
            Ipv4Addr::new(192, 168, 1, 5)
        ));
        assert!(!names_adapter(
            "mac:a0b1c2d3e4f5:x:192.168.1.4",
            "mac:a0b1c2d3e4f5",
            ADDRESS
        ));
        assert!(!names_adapter(
            "mac:a0b1c2d3e4f5:4:192.168.1.4:9",
            "mac:a0b1c2d3e4f5",
            ADDRESS
        ));
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
            ControlPermissions::BOTH,
            SessionPurpose::Setup,
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
mod standing_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    /// The pinned port: like the real bind without address reuse, it takes one owner at a time.
    #[derive(Clone, Default)]
    struct Port(Arc<AtomicBool>);

    struct Fake {
        id: usize,
        signal: RevocationSignal,
        port: Port,
        retires: Arc<AtomicUsize>,
    }

    impl Fake {
        fn bind(port: &Port, id: usize, retires: &Arc<AtomicUsize>) -> Result<Self, SetupFailure> {
            if port.0.swap(true, Ordering::SeqCst) {
                return Err(SetupFailure::PortBusy);
            }
            Ok(Self {
                id,
                signal: RevocationSignal::default(),
                port: port.clone(),
                retires: Arc::clone(retires),
            })
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.port.0.store(false, Ordering::SeqCst);
        }
    }

    impl Standing for Fake {
        fn signal(&self) -> &RevocationSignal {
            &self.signal
        }

        async fn retire(self) {
            self.retires.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn an_untouched_endpoint_serves_the_next_session_without_a_rebind() {
        let (port, retires) = (Port::default(), Arc::default());
        let mut slot = StandingSlot::new();
        let first = slot
            .take_or_bind(|| Fake::bind(&port, 1, &retires))
            .await
            .unwrap();
        slot.keep(first).await;
        let reused = slot
            .take_or_bind(|| panic!("an untouched endpoint is never rebound"))
            .await
            .unwrap();
        assert_eq!(reused.id, 1);
        assert_eq!(retires.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_endpoint_asked_to_stop_is_retired_by_reclaim_and_connect_before_any_bind() {
        let stops: [fn(&RevocationSignal); 2] =
            [RevocationSignal::request_stop, RevocationSignal::revoke];
        for stop in stops {
            // Reclaim: an ended session's endpoint whose signal stopped retires and is let go.
            let (port, retires) = (Port::default(), Arc::default());
            let mut slot = StandingSlot::new();
            let ended = Fake::bind(&port, 1, &retires).unwrap();
            stop(&ended.signal);
            slot.keep(ended).await;
            assert!(slot.held.is_none());
            assert_eq!(retires.load(Ordering::SeqCst), 1);
            let fresh = slot
                .take_or_bind(|| Fake::bind(&port, 2, &retires))
                .await
                .unwrap();
            assert_eq!(fresh.id, 2);
            assert!(!fresh.signal.is_stopping());

            // Connect: a failed attempt handed its endpoint back; it stops before the next attempt.
            let (port, retires) = (Port::default(), Arc::default());
            let mut slot = StandingSlot::new();
            let failed = Fake::bind(&port, 1, &retires).unwrap();
            let signal = failed.signal.clone();
            slot.hold(failed);
            stop(&signal);
            let fresh = slot
                .take_or_bind(|| Fake::bind(&port, 2, &retires))
                .await
                .expect("the retired endpoint released the port before the bind");
            assert_eq!(fresh.id, 2);
            assert_eq!(retires.load(Ordering::SeqCst), 1);
        }
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
        // Different saved control maps must be agreed again over the setup link.
        assert_eq!(
            handshake_failure(HandshakeError::ControlMismatch),
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

#[cfg(test)]
mod identity_tests {
    use super::*;

    #[test]
    fn identical_attempt_failures_log_once_then_once_a_minute_with_a_count() {
        let mut log = AttemptLog::new();
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        assert_eq!(
            log.repeated("refused".into(), at(0)),
            Some("refused".into())
        );
        for second in 1..60 {
            assert_eq!(log.repeated("refused".into(), at(second)), None);
        }
        assert_eq!(
            log.repeated("refused".into(), at(60)),
            Some("refused (repeated 60 times since the last report)".into())
        );
        assert_eq!(log.repeated("refused".into(), at(61)), None);
        assert_eq!(
            log.repeated("timed out".into(), at(62)),
            Some("timed out (the previous failure repeated 1 more times)".into())
        );
    }

    #[test]
    fn each_refused_identity_is_warned_about_once() {
        let mut log = AttemptLog::new();
        let first = CertificateFingerprint::from_certificate_der(b"first");
        let second = CertificateFingerprint::from_certificate_der(b"second");
        assert!(log.first_refusal(Some(first)));
        assert!(!log.first_refusal(Some(first)));
        assert!(log.first_refusal(None));
        assert!(!log.first_refusal(None));
        assert!(log.first_refusal(Some(second)));
    }

    /// This computer's pin refusing the presented certificate is an identity change, and so is the
    /// peer's refusal of ours once this computer's handshake finished and authenticated the peer.
    /// Neither is a connection that may succeed on the next dial. A refusal the peer sends before
    /// then is only a failed connection.
    #[tokio::test]
    async fn a_refused_certificate_is_an_identity_change_only_where_it_is_authenticated() {
        use crate::crypto::{LOCAL_TLS_SERVER_NAME, RefusedCertificate, SecureQuicConfig};
        let pin = |identity: &DeviceIdentity| {
            VerifiedPeer::from_certificate_der(
                identity.certificate_der(),
                &identity.fingerprint().full_hex(),
            )
            .unwrap()
        };
        for server_refuses in [true, false] {
            let client = DeviceIdentity::generate().unwrap();
            let server = DeviceIdentity::generate().unwrap();
            let stale = DeviceIdentity::generate().unwrap();
            let (client_pin, server_pin) = if server_refuses {
                (pin(&stale), pin(&server))
            } else {
                (pin(&client), pin(&stale))
            };
            let (server_refused, client_refused) =
                (RefusedCertificate::default(), RefusedCertificate::default());
            let local: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
            let listener = quinn::Endpoint::server(
                SecureQuicConfig::server_recording(&server, &client_pin, server_refused.clone())
                    .unwrap(),
                local,
            )
            .unwrap();
            let mut dialer = quinn::Endpoint::client(local).unwrap();
            dialer.set_default_client_config(
                SecureQuicConfig::client_recording(&client, &server_pin, client_refused.clone())
                    .unwrap(),
            );
            let connecting = dialer
                .connect(listener.local_addr().unwrap(), LOCAL_TLS_SERVER_NAME)
                .unwrap();
            let (dialed, accepted) = tokio::time::timeout(Duration::from_secs(10), async {
                tokio::join!(
                    async {
                        match connecting.await {
                            // A finished handshake: its close comes from the authenticated server.
                            Ok(connection) => Ok(connection.closed().await),
                            Err(error) => Err(error),
                        }
                    },
                    async { listener.accept().await.unwrap().await.unwrap_err() }
                )
            })
            .await
            .expect("both sides settle the refused handshake");
            // The listener's handshake never finishes, so only its own pin's refusal counts.
            let recorded = server_refused.take();
            let listener_saw =
                connection_failure("share accept failed", &accepted, Some(&accepted), || {
                    recorded
                });
            if server_refuses {
                assert_eq!(
                    listener_saw,
                    SetupFailure::PeerIdentityChanged,
                    "{accepted}"
                );
                assert!(recorded == Some(client.fingerprint()));
                match dialed {
                    Ok(closed) => assert!(refused_identity(&closed), "dialer saw {closed}"),
                    Err(failed) => assert_eq!(
                        connection_failure("share dial failed", &failed, Some(&failed), || None),
                        SetupFailure::Connection
                    ),
                }
            } else {
                assert_eq!(listener_saw, SetupFailure::Connection, "{accepted}");
                assert!(recorded.is_none());
                let failed = dialed.expect_err("the client's own pin refuses during its handshake");
                assert!(client_refused.take() == Some(server.fingerprint()));
                assert_eq!(
                    connection_failure("share dial failed", &failed, Some(&failed), || None),
                    SetupFailure::PeerIdentityChanged
                );
            }
            assert!(!is_transient(SetupFailure::PeerIdentityChanged));
            listener.close(0_u32.into(), b"fixture complete");
            dialer.close(0_u32.into(), b"fixture complete");
        }
    }

    #[test]
    fn an_unreachable_peer_is_still_a_connection_failure() {
        let timed_out = quinn::ConnectionError::TimedOut;
        assert!(!refused_identity(&timed_out));
        assert_eq!(
            connection_failure("share dial failed", &timed_out, Some(&timed_out), || None),
            SetupFailure::Connection
        );
    }
}

#[cfg(test)]
mod capture_stop_close_tests {
    use super::*;
    use crate::{
        crypto::{DeviceIdentity, LOCAL_TLS_SERVER_NAME, SecureQuicConfig, VerifiedPeer},
        session::{NETWORK_REVOKED_REASON, SESSION_FAILED_REASON, close_reason, worker_end},
    };
    use monhop_core::capture::{CaptureStop, StopReason};
    use quinn::udp::{RecvMeta, Transmit};
    use std::{
        fmt,
        io::{self, IoSliceMut},
        net::SocketAddr,
        pin::Pin,
        sync::Arc,
        task::{Context, Poll},
    };

    /// Far inside `session_health::HOLD_LIMIT`, how long a peer that never hears the close holds.
    const HEARD_WITHIN: Duration = Duration::from_millis(200);

    /// Loopback UDP gated like the guarded socket: once its session signal is revoked, nothing
    /// leaves.
    struct GatedSocket {
        inner: Arc<dyn quinn::AsyncUdpSocket>,
        signal: RevocationSignal,
    }

    impl fmt::Debug for GatedSocket {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("GatedSocket")
        }
    }

    impl quinn::AsyncUdpSocket for GatedSocket {
        fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn quinn::UdpPoller>> {
            Arc::clone(&self.inner).create_io_poller()
        }

        fn try_send(&self, transmit: &Transmit<'_>) -> io::Result<()> {
            if self.signal.is_revoked() {
                return Ok(());
            }
            self.inner.try_send(transmit)
        }

        fn poll_recv(
            &self,
            cx: &mut Context<'_>,
            bufs: &mut [IoSliceMut<'_>],
            meta: &mut [RecvMeta],
        ) -> Poll<io::Result<usize>> {
            self.inner.poll_recv(cx, bufs, meta)
        }

        fn local_addr(&self) -> io::Result<SocketAddr> {
            self.inner.local_addr()
        }

        fn max_transmit_segments(&self) -> usize {
            self.inner.max_transmit_segments()
        }

        fn max_receive_segments(&self) -> usize {
            self.inner.max_receive_segments()
        }

        fn may_fragment(&self) -> bool {
            self.inner.may_fragment()
        }
    }

    /// Side A's capture stops with an expired lease and `settle` hands that to its session signal;
    /// A's runtime then ends as it would and queues its close. Returns how side B's connection
    /// closed, if it heard within `HEARD_WITHIN`.
    async fn peer_hears_capture_end(
        settle: impl FnOnce(&CaptureStop, &RevocationSignal),
    ) -> Option<quinn::ConnectionError> {
        let ours_id = DeviceIdentity::generate().unwrap();
        let theirs_id = DeviceIdentity::generate().unwrap();
        let pin = |identity: &DeviceIdentity| {
            VerifiedPeer::from_certificate_der(
                identity.certificate_der(),
                &identity.fingerprint().full_hex(),
            )
            .unwrap()
        };
        let loopback: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let theirs = quinn::Endpoint::server(
            SecureQuicConfig::server(&theirs_id, &pin(&ours_id)).unwrap(),
            loopback,
        )
        .unwrap();
        let signal = RevocationSignal::default();
        let socket = quinn::Runtime::wrap_udp_socket(
            &quinn::TokioRuntime,
            std::net::UdpSocket::bind(loopback).unwrap(),
        )
        .unwrap();
        let mut ours = quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            None,
            Arc::new(GatedSocket {
                inner: socket,
                signal: signal.clone(),
            }),
            Arc::new(quinn::TokioRuntime),
        )
        .unwrap();
        ours.set_default_client_config(
            SecureQuicConfig::client(&ours_id, &pin(&theirs_id)).unwrap(),
        );
        let connecting = ours
            .connect(theirs.local_addr().unwrap(), LOCAL_TLS_SERVER_NAME)
            .unwrap();
        let (our_connection, their_connection) =
            tokio::time::timeout(Duration::from_secs(2), async {
                tokio::join!(async { connecting.await.unwrap() }, async {
                    theirs.accept().await.unwrap().await.unwrap()
                })
            })
            .await
            .unwrap();
        // Wired as prepare_endpoint wires the endpoint's revoker.
        let revoked_endpoint = ours.clone();
        let revoked_signal = signal.clone();
        let watch =
            CancellationWatch::start(RevocationSignal::default(), signal.clone(), move || {
                revoked_signal.revoke();
                revoked_endpoint.close(0_u32.into(), NETWORK_REVOKED_REASON);
            })
            .unwrap();

        let capture = CaptureStop::new();
        capture.stop(StopReason::LeaseExpired);
        settle(&capture, &signal);
        let failure = worker_end(signal.is_stopping(), false, capture.reason(), false, None)
            .expect("a capture's own end ends the session");
        our_connection.close(
            0_u32.into(),
            close_reason(
                &Err(failure),
                false,
                signal.is_stopping_for_control_change(),
            ),
        );
        let heard = tokio::time::timeout(HEARD_WITHIN, their_connection.closed())
            .await
            .ok();
        drop(watch);
        ours.close(0_u32.into(), b"fixture complete");
        theirs.close(0_u32.into(), b"fixture complete");
        heard
    }

    #[tokio::test]
    async fn a_lease_expiry_reaches_the_peer_as_a_failure_close_long_before_its_deadline() {
        let heard = peer_hears_capture_end(|capture, session| {
            capture.request_session_stop(session);
        })
        .await;
        assert!(
            matches!(
                &heard,
                Some(quinn::ConnectionError::ApplicationClosed(close))
                    if close.reason.as_ref() == SESSION_FAILED_REASON
            ),
            "{heard:?}"
        );
    }

    /// The end this replaces: a revoked session signal swallowed the close, so the peer waited.
    #[tokio::test]
    async fn a_revoked_session_signal_leaves_the_peer_waiting_for_its_deadline() {
        let heard = peer_hears_capture_end(|_, session| session.mark_revoked_without_wake()).await;
        assert!(heard.is_none(), "{heard:?}");
    }
}

#[cfg(test)]
mod group_tests {
    use std::net::{SocketAddr, UdpSocket};

    use super::*;
    use crate::{
        crypto::LOCAL_TLS_SERVER_NAME,
        guarded_endpoint::loopback::{self, pin},
        pairing::PairingOffer,
    };

    const DEADLINE: Duration = Duration::from_secs(5);
    const HUB: Platform = Platform::Windows;
    const MEMBER: Platform = Platform::MacOs;
    const NO_AGREEMENT: [u8; 32] = [0; 32];

    fn topology() -> DisplayTopology {
        DisplayTopology::new(vec![DisplayDescription {
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
        .unwrap()
    }

    fn fixture_displays(_: DeviceId) -> Result<DisplayTopology, SetupFailure> {
        Ok(topology())
    }

    /// A paired computer with its own loopback socket, before it dials or listens.
    struct Computer {
        identity: DeviceIdentity,
        socket: UdpSocket,
        address: SocketAddrV4,
    }

    /// A paired computer on its plain, unguarded endpoint.
    struct Remote {
        identity: DeviceIdentity,
        endpoint: quinn::Endpoint,
        address: SocketAddrV4,
    }

    impl Computer {
        fn new() -> Self {
            let socket = loopback::bind();
            Self {
                identity: DeviceIdentity::generate().unwrap(),
                address: loopback::address(&socket),
                socket,
            }
        }

        fn dialing(self, hub: &Hub) -> Remote {
            Remote {
                endpoint: loopback::dialer(self.socket, &self.identity, &hub.pin),
                identity: self.identity,
                address: self.address,
            }
        }

        fn listening(self, hub: &Hub) -> Remote {
            Remote {
                endpoint: loopback::listener(self.socket, &self.identity, &hub.pin),
                identity: self.identity,
                address: self.address,
            }
        }
    }

    impl Remote {
        fn fingerprint(&self) -> CertificateFingerprint {
            self.identity.fingerprint()
        }
    }

    struct Hub {
        group: Rc<GroupEndpoint>,
        pin: VerifiedPeer,
        address: SocketAddr,
        cancel: RevocationSignal,
    }

    /// A group endpoint on loopback admitting `members`: it dials them all when `dials`, else it
    /// waits for each.
    fn hub(members: &[&Computer], dials: bool, controls: &[GroupMemberRecord]) -> Hub {
        let identity = DeviceIdentity::generate().unwrap();
        let socket = loopback::bind();
        let address = socket.local_addr().unwrap();
        let group: Vec<_> = members
            .iter()
            .map(|member| GroupMember {
                address: member.address,
                pin: pin(&member.identity),
            })
            .collect();
        let endpoint = GuardedEndpoint::over_loopback(socket, &identity, &group).unwrap();
        let peers = members
            .iter()
            .map(|member| GroupPeer {
                fingerprint: member.identity.fingerprint(),
                pin: pin(&member.identity),
                device: device_id_from_fingerprint(member.identity.fingerprint()),
                platform: MEMBER,
                dials,
            })
            .collect();
        let hub_pin = pin(&identity);
        let cancel = RevocationSignal::default();
        let group = GroupEndpoint::assemble(
            endpoint,
            LocalFacts::new(identity, HUB),
            peers,
            controls,
            "loopback-test",
            &cancel,
            fixture_displays,
        )
        .unwrap();
        Hub {
            group,
            pin: hub_pin,
            address,
            cancel,
        }
    }

    /// The paired computer's side of the session handshake with the hub.
    async fn remote_negotiates(
        remote: &Remote,
        hub: &Hub,
        connection: quinn::Connection,
        purpose: SessionPurpose,
        agreement: [u8; 32],
    ) -> Result<NegotiatedSession, HandshakeError> {
        let displays = topology();
        let capabilities =
            Capabilities::new(Capabilities::RELATIVE_MOTION | Capabilities::DISPLAY_TOPOLOGY)
                .unwrap();
        let config = HandshakeConfig::new(
            &remote.identity,
            &hub.pin,
            MEMBER,
            HUB,
            capabilities,
            capabilities,
            &displays,
            ControlPermissions::BOTH,
            purpose,
        )
        .unwrap()
        .with_agreement(agreement);
        negotiate(connection, config).await
    }

    /// Dials the hub from the computer's recorded address and opens a setup link.
    async fn remote_dials(remote: &Remote, hub: &Hub) -> NegotiatedSession {
        let connection = remote
            .endpoint
            .connect(hub.address, LOCAL_TLS_SERVER_NAME)
            .unwrap()
            .await
            .unwrap();
        remote_negotiates(remote, hub, connection, SessionPurpose::Setup, NO_AGREEMENT)
            .await
            .unwrap()
    }

    /// Answers the hub's dial with `agreement`.
    async fn remote_answers(
        remote: &Remote,
        hub: &Hub,
        agreement: [u8; 32],
    ) -> Result<NegotiatedSession, HandshakeError> {
        let connection = remote.endpoint.accept().await.unwrap().await.unwrap();
        remote_negotiates(remote, hub, connection, SessionPurpose::Share, agreement).await
    }

    async fn until(condition: impl Fn() -> bool) {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn each_accepted_connection_reaches_only_its_members_waiter() {
        let computers = [(); 3].map(|()| Computer::new());
        let hub = hub(&computers.each_ref(), false, &[]);
        let [first, second, third] = computers.map(|computer| computer.dialing(&hub));
        let setup = |remote: &Remote| {
            hub.group.connect(
                remote.fingerprint(),
                SessionPurpose::Setup,
                NO_AGREEMENT,
                &hub.cancel,
            )
        };
        let (at_first, at_second, (from_second, from_first, unclaimed)) =
            tokio::time::timeout(DEADLINE, async {
                tokio::join!(setup(&first), setup(&second), async {
                    until(|| {
                        hub.group.waiting(first.fingerprint())
                            && hub.group.waiting(second.fingerprint())
                    })
                    .await;
                    // The second computer's connection arrives while both connects wait.
                    let from_second = remote_dials(&second, &hub).await;
                    let from_first = remote_dials(&first, &hub).await;
                    let unclaimed = third
                        .endpoint
                        .connect(hub.address, LOCAL_TLS_SERVER_NAME)
                        .unwrap()
                        .await;
                    (from_second, from_first, unclaimed)
                })
            })
            .await
            .expect("both waiting computers connected");
        for (paired, remote, dialed) in [
            (at_first.unwrap(), &first, &from_first),
            (at_second.unwrap(), &second, &from_second),
        ] {
            assert!(paired.inspection.peer_fingerprint == remote.fingerprint());
            assert_eq!(
                paired.session.connection.remote_address(),
                SocketAddr::V4(remote.address)
            );
            assert_eq!(dialed.peer.device_id, paired.inspection.local_device);
        }
        assert!(
            unclaimed.is_err(),
            "no connect waits for the third computer"
        );
    }

    #[tokio::test]
    async fn an_unclaimed_member_connection_is_refused() {
        let computer = Computer::new();
        let hub = hub(&[&computer], false, &[]);
        let remote = computer.dialing(&hub);
        let refused = tokio::time::timeout(
            DEADLINE,
            remote
                .endpoint
                .connect(hub.address, LOCAL_TLS_SERVER_NAME)
                .unwrap(),
        )
        .await
        .expect("the refusal is prompt");
        assert!(
            matches!(
                &refused,
                Err(quinn::ConnectionError::ConnectionClosed(close))
                    if close.error_code == quinn::TransportErrorCode::CONNECTION_REFUSED
            ),
            "{refused:?}"
        );

        let fingerprint = remote.fingerprint();
        let (paired, _dialed) = tokio::time::timeout(DEADLINE, async {
            tokio::join!(
                hub.group.connect(
                    fingerprint,
                    SessionPurpose::Setup,
                    NO_AGREEMENT,
                    &hub.cancel
                ),
                async {
                    until(|| hub.group.waiting(fingerprint)).await;
                    remote_dials(&remote, &hub).await
                }
            )
        })
        .await
        .expect("a claimed connection is delivered");
        assert!(paired.unwrap().inspection.peer_fingerprint == fingerprint);
    }

    #[tokio::test]
    async fn forgetting_one_member_keeps_the_others_connected() {
        let computers = [(); 2].map(|()| Computer::new());
        let hub = hub(&computers.each_ref(), false, &[]);
        let [forgotten, kept] = computers.map(|computer| computer.dialing(&hub));
        let setup = |fingerprint| {
            hub.group.connect(
                fingerprint,
                SessionPurpose::Setup,
                NO_AGREEMENT,
                &hub.cancel,
            )
        };
        let (at_forgotten, at_kept, (from_forgotten, from_kept)) =
            tokio::time::timeout(DEADLINE, async {
                tokio::join!(
                    setup(forgotten.fingerprint()),
                    setup(kept.fingerprint()),
                    async {
                        until(|| {
                            hub.group.waiting(forgotten.fingerprint())
                                && hub.group.waiting(kept.fingerprint())
                        })
                        .await;
                        tokio::join!(remote_dials(&forgotten, &hub), remote_dials(&kept, &hub))
                    }
                )
            })
            .await
            .expect("both computers connected");
        let (at_forgotten, at_kept) = (at_forgotten.unwrap(), at_kept.unwrap());

        // A connect already waiting for the forgotten computer ends with it.
        let (waited, ()) = tokio::time::timeout(DEADLINE, async {
            tokio::join!(setup(forgotten.fingerprint()), async {
                until(|| hub.group.waiting(forgotten.fingerprint())).await;
                hub.group.forget(forgotten.fingerprint());
            })
        })
        .await
        .expect("forget ends the waiting connect at once");
        assert_eq!(waited.err(), Some(SetupFailure::PairingRequired));
        let closed = tokio::time::timeout(DEADLINE, from_forgotten.connection.closed())
            .await
            .expect("the forgotten computer's connection closed");
        assert!(
            matches!(
                &closed,
                quinn::ConnectionError::ApplicationClosed(close)
                    if close.reason.as_ref() == MEMBER_FORGOTTEN_REASON
            ),
            "{closed}"
        );
        assert!(at_forgotten.session.connection.close_reason().is_some());

        // The kept computer's connection still carries data both ways.
        let (ours, theirs) = (&at_kept.session.connection, &from_kept.connection);
        for (from, to) in [(ours, theirs), (theirs, ours)] {
            from.send_datagram(b"still connected".to_vec().into())
                .unwrap();
            let received = tokio::time::timeout(DEADLINE, to.read_datagram())
                .await
                .expect("the datagram crossed")
                .unwrap();
            assert_eq!(received.as_ref(), b"still connected");
        }
        assert!(ours.close_reason().is_none());

        // Only a new bind admits the forgotten computer again; its dials get no answer.
        assert!(!hub.group.admits(forgotten.fingerprint()));
        assert!(hub.group.admits(kept.fingerprint()));
        assert_eq!(
            setup(forgotten.fingerprint()).await.err(),
            Some(SetupFailure::PairingRequired)
        );
        let ignored = tokio::time::timeout(
            Duration::from_millis(500),
            forgotten
                .endpoint
                .connect(hub.address, LOCAL_TLS_SERVER_NAME)
                .unwrap(),
        )
        .await;
        assert!(ignored.is_err(), "a forgotten computer gets no answer");
        assert!(ours.close_reason().is_none());
    }

    #[tokio::test]
    async fn a_share_connect_with_a_different_agreement_is_changed_since_inspection() {
        let computer = Computer::new();
        let fingerprint = computer.identity.fingerprint();
        let controls = [GroupMemberRecord {
            fingerprint,
            control: ControlPermissions::BOTH,
        }];
        let hub = hub(&[&computer], true, &controls);
        let remote = computer.listening(&hub);
        let share = |agreement| {
            hub.group
                .connect(fingerprint, SessionPurpose::Share, agreement, &hub.cancel)
        };

        let (ours, theirs) = tokio::time::timeout(DEADLINE, async {
            tokio::join!(share([1; 32]), remote_answers(&remote, &hub, [2; 32]))
        })
        .await
        .expect("both sides settle the disagreement");
        assert_eq!(ours.err(), Some(SetupFailure::ChangedSinceInspection));
        assert_eq!(theirs.err(), Some(HandshakeError::AgreementMismatch));

        // The same agreement connects, on the dial path's member binding.
        let (ours, theirs) = tokio::time::timeout(DEADLINE, async {
            tokio::join!(share([3; 32]), remote_answers(&remote, &hub, [3; 32]))
        })
        .await
        .expect("an agreed share connects");
        let ours = ours.unwrap();
        theirs.unwrap();
        assert!(ours.inspection.peer_fingerprint == fingerprint);
        assert_eq!(
            ours.session.connection.remote_address(),
            SocketAddr::V4(remote.address)
        );

        // Outside the group record there is no control to share under.
        hub.group.set_members(&[]);
        assert_eq!(
            share([3; 32]).await.err(),
            Some(SetupFailure::ChangedSinceInspection)
        );
    }

    #[tokio::test]
    async fn a_close_before_the_handshake_is_not_an_identity_change() {
        let stale = DeviceIdentity::generate().unwrap();
        let computers = [(); 2].map(|()| Computer::new());
        let hub = hub(&computers.each_ref(), false, &[]);
        let [refusing, impostor] = computers;
        let setup = |fingerprint| {
            hub.group.connect(
                fingerprint,
                SessionPurpose::Setup,
                NO_AGREEMENT,
                &hub.cancel,
            )
        };

        // This computer pins a stale certificate for the hub, so it refuses the hub's and closes
        // before the hub's handshake finishes. Anyone who sees the handshake could send that.
        let refusing = Remote {
            endpoint: loopback::dialer(refusing.socket, &refusing.identity, &pin(&stale)),
            identity: refusing.identity,
            address: refusing.address,
        };
        let fingerprint = refusing.fingerprint();
        let (ours, theirs) = tokio::time::timeout(DEADLINE, async {
            tokio::join!(setup(fingerprint), async {
                until(|| hub.group.waiting(fingerprint)).await;
                refusing
                    .endpoint
                    .connect(hub.address, LOCAL_TLS_SERVER_NAME)
                    .unwrap()
                    .await
            })
        })
        .await
        .expect("both sides settle the refusal");
        assert!(
            theirs.as_ref().is_err_and(refused_by_this_computer),
            "the other computer's own pin refused: {theirs:?}"
        );
        assert_eq!(ours.err(), Some(SetupFailure::Connection));

        // A certificate other than the member's, from its recorded address, is refused by this
        // computer's own pin: that is an identity change.
        let fingerprint = impostor.identity.fingerprint();
        let impostor = Remote {
            endpoint: loopback::dialer(impostor.socket, &stale, &hub.pin),
            identity: stale,
            address: impostor.address,
        };
        let (ours, _theirs) = tokio::time::timeout(DEADLINE, async {
            tokio::join!(setup(fingerprint), async {
                until(|| hub.group.waiting(fingerprint)).await;
                impostor
                    .endpoint
                    .connect(hub.address, LOCAL_TLS_SERVER_NAME)
                    .unwrap()
                    .await
            })
        })
        .await
        .expect("both sides settle the refusal");
        assert_eq!(ours.err(), Some(SetupFailure::PeerIdentityChanged));
    }

    #[test]
    fn the_admitted_set_is_every_paired_computer_on_the_selected_subnet() {
        let local = DeviceIdentity::generate().unwrap();
        let ids: Vec<_> = (0..9)
            .map(|_| DeviceIdentity::generate().unwrap())
            .collect();
        let selected = InterfaceSnapshot {
            stable_id: "adapter".into(),
            name: "ethernet".into(),
            index: 7,
            address: Ipv4Addr::new(192, 168, 50, 10),
            prefix_len: 24,
            kind: InterfaceKind::Ethernet,
            is_hardware: true,
            is_up: true,
            network_signature: vec![1; 32],
        };
        let record = |host: [u8; 4], identity: &DeviceIdentity| {
            let offer = PairingOffer::new(
                SocketAddrV4::new(host.into(), PAIRING_PORT),
                identity.certificate_der(),
            )
            .unwrap()
            .with_platform(MEMBER);
            ConfirmedPeerRecord::new(local.fingerprint(), offer).unwrap()
        };
        let in_group = |identity: &DeviceIdentity| GroupMemberRecord {
            fingerprint: identity.fingerprint(),
            control: ControlPermissions::BOTH,
        };
        let records = [
            record([192, 168, 50, 20], &ids[0]),
            record([192, 168, 50, 21], &ids[1]),
            record([10, 0, 0, 5], &ids[2]),
            record([192, 168, 50, 20], &ids[3]),
        ];
        let admitted = admitted_peers(
            &selected,
            local.fingerprint(),
            HUB,
            &records,
            &[in_group(&ids[1])],
        );
        let admitted: Vec<_> = admitted
            .iter()
            .map(|(peer, address)| (peer.fingerprint, *address, peer.dials))
            .collect();
        // The group member first, then any paired computer on the subnet; another subnet and a
        // second computer at a taken address are left out.
        assert!(
            admitted
                == [
                    (ids[1].fingerprint(), records[1].peer().endpoint(), true),
                    (ids[0].fingerprint(), records[0].peer().endpoint(), true),
                ]
        );

        let crowded: Vec<_> = (0..9_u8)
            .map(|host| record([192, 168, 50, 30 + host], &ids[usize::from(host)]))
            .collect();
        let admitted = admitted_peers(
            &selected,
            local.fingerprint(),
            HUB,
            &crowded,
            &[in_group(&ids[8])],
        );
        assert_eq!(admitted.len(), MAX_PINNED_PEERS);
        assert!(admitted[0].0.fingerprint == ids[8].fingerprint());
    }
}
