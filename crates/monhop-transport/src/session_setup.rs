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
        let local_displays = read_native_displays(self.local_device).await?;
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
    /// This link's own cancel, to pass to the session run on it: stopping it ends only that
    /// session, never the endpoint. The endpoint revokes it once the member is forgotten and
    /// requests a stop once the link is released or replaced; revoking the endpoint closes the
    /// connection instead.
    pub cancel: RevocationSignal,
}

/// The connection a group endpoint last handed out for one member, with that link's cancel.
struct LiveLink {
    connection: quinn::Connection,
    cancel: RevocationSignal,
}

/// Close reasons for member connections the group endpoint ends itself.
const MEMBER_FORGOTTEN_REASON: &[u8] = b"pairing removed";
pub(crate) const MEMBER_SUPERSEDED_REASON: &[u8] = b"replaced by a newer connection";
pub(crate) const MEMBER_RELEASED_REASON: &[u8] = b"connection released";
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

/// Enumeration can stall, and the runtime awaiting it may be the network thread that serves the
/// hub and every connection, so `reader` runs on the blocking pool.
async fn read_displays(
    reader: DisplayReader,
    device: DeviceId,
) -> Result<DisplayTopology, SetupFailure> {
    match tokio::task::spawn_blocking(move || reader(device)).await {
        Ok(displays) => displays,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(_) => Err(SetupFailure::Displays),
    }
}

/// This computer's displays, read off the calling runtime's thread.
pub(crate) async fn read_native_displays(
    device: DeviceId,
) -> Result<DisplayTopology, SetupFailure> {
    read_displays(native_displays, device).await
}

/// Why a bind left a paired computer out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Exclusion {
    /// Its recorded address is off the selected subnet.
    OffSubnet,
    /// A computer admitted before it holds its recorded address.
    AddressTaken,
    /// The network already admits `MAX_PINNED_PEERS`.
    Full,
}

/// Who a bind admits, and each other paired computer with why it was left out.
struct Admission {
    admitted: Vec<(GroupPeer, SocketAddrV4)>,
    excluded: Vec<(CertificateFingerprint, Exclusion)>,
}

/// O-1: every paired computer the selected network reaches directly, whatever group it is in.
/// Group members come first, so they win an address and the cap over any other paired record;
/// one computer per address, and no more than one socket pins.
fn admitted_peers(
    selected: &InterfaceSnapshot,
    local: CertificateFingerprint,
    local_platform: Platform,
    records: &[ConfirmedPeerRecord],
    members: &[GroupMemberRecord],
) -> Admission {
    let (grouped, others): (Vec<_>, Vec<_>) = records.iter().partition(|record| {
        members
            .iter()
            .any(|member| member.fingerprint == record.peer().fingerprint())
    });
    let mut admission = Admission {
        admitted: Vec::new(),
        excluded: Vec::new(),
    };
    for record in grouped.into_iter().chain(others) {
        let offer = record.peer();
        let fingerprint = offer.fingerprint();
        let address = offer.endpoint();
        let admitted = &admission.admitted;
        if fingerprint == local
            || admitted
                .iter()
                .any(|(peer, _)| peer.fingerprint == fingerprint)
        {
            continue;
        }
        let Ok(pin) = offer.verified_peer() else {
            log::info!(
                "group endpoint: paired computer {} has an unreadable certificate; not admitted",
                fingerprint.short_hex()
            );
            continue;
        };
        let exclusion = if validate_peer(selected, *address.ip()).is_err() {
            Some(Exclusion::OffSubnet)
        } else if admitted
            .iter()
            .any(|(_, earlier)| earlier.ip() == address.ip())
        {
            Some(Exclusion::AddressTaken)
        } else if admitted.len() == MAX_PINNED_PEERS {
            Some(Exclusion::Full)
        } else {
            None
        };
        if let Some(exclusion) = exclusion {
            let short = fingerprint.short_hex();
            match exclusion {
                Exclusion::OffSubnet => log::info!(
                    "group endpoint: paired computer {short} is off the selected subnet; not admitted"
                ),
                Exclusion::AddressTaken => log::info!(
                    "group endpoint: paired computer {short} is at an address already admitted; \
                     not admitted"
                ),
                Exclusion::Full => log::warn!(
                    "group endpoint: paired computer {short} not admitted: one network admits at \
                     most {MAX_PINNED_PEERS}"
                ),
            }
            admission.excluded.push((fingerprint, exclusion));
            continue;
        }
        let platform = offer.platform();
        admission.admitted.push((
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
    admission
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
    live: RefCell<Box<[Option<LiveLink>]>>,
    controls: RefCell<Vec<GroupMemberRecord>>,
    peers: Box<[GroupPeer]>,
    /// Paired computers the bind left out and not forgotten since.
    excluded: RefCell<Vec<(CertificateFingerprint, Exclusion)>>,
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
    /// admitted computer without one can open only setup links. A paired computer left out, off
    /// the subnet, at an address a group member or an earlier record holds, or past the cap,
    /// fails every connect as `NetworkRoute`; only one with no pairing is `PairingRequired`.
    /// Must run on the network runtime, which then hosts the one accept router.
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
        let Admission { admitted, excluded } = admitted_peers(
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
            excluded,
            members,
            interface_id,
            cancel,
            native_displays,
        )
    }

    /// `peers` must be in the endpoint's member order.
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        endpoint: GuardedEndpoint,
        local: LocalFacts,
        peers: Box<[GroupPeer]>,
        excluded: Vec<(CertificateFingerprint, Exclusion)>,
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
            excluded: RefCell::new(excluded),
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

    /// The whole endpoint's revocation: stopping or revoking it ends every member's connection
    /// and the endpoint itself. Never use it to cancel one link; each `PairedMember` carries its
    /// own `cancel`.
    pub fn revocation(&self) -> RevocationSignal {
        self.revocation.clone()
    }

    /// Dials `member` or waits for it, as the pair's dial rule says, then negotiates `purpose`.
    /// A share session carries the member's control and `agreement`, and a different agreement on
    /// the other computer is `ChangedSinceInspection`. A member forgotten, or `cancel` revoked,
    /// before or during negotiation gets no session, and no Hello when that came first. Handing
    /// the session out closes the member's previous connection.
    pub async fn connect(
        &self,
        member: CertificateFingerprint,
        purpose: SessionPurpose,
        agreement: [u8; 32],
        cancel: &RevocationSignal,
    ) -> Result<PairedMember, SetupFailure> {
        let index = self.admitted(member)?;
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
            let connection = self.connection(index, cancel).await?;
            self.negotiate_while_admitted(index, connection, control, purpose, agreement, cancel)
                .await
        })
        .await;
        let Ok(established) = attempt else {
            log::debug!("share connect window of {CONNECT_WINDOW:?} passed without a session");
            return Err(SetupFailure::Connection);
        };
        let (session, inspection) = established?;
        self.hand_out(index, session, inspection, cancel)
    }

    /// Whether this computer dials `member` under the pair's dial rule.
    pub(crate) fn dials(&self, member: CertificateFingerprint) -> Result<bool, SetupFailure> {
        self.admitted(member).map(|index| self.peers[index].dials)
    }

    /// Why no connection to `member` may start now: it was forgotten, or the endpoint revoked.
    pub(crate) fn check_member(&self, member: CertificateFingerprint) -> Result<(), SetupFailure> {
        let index = self.admitted(member)?;
        self.check(index, None)
    }

    /// The connection step of a setup `connect` alone: a dial to the member's recorded address or
    /// its next incoming connection, as the pair's dial rule says. A setup link bounds and retries
    /// it as on an endpoint of its own, then hands the connection to `negotiate_setup`.
    pub(crate) async fn setup_connection(
        &self,
        member: CertificateFingerprint,
        cancel: &RevocationSignal,
    ) -> Result<quinn::Connection, SetupFailure> {
        let index = self.admitted(member)?;
        self.check(index, Some(cancel))?;
        self.connection(index, cancel).await
    }

    /// Negotiates a setup link on a connection from `setup_connection` and hands it out as
    /// `connect` does, closing the member's previous connection.
    pub(crate) async fn negotiate_setup(
        &self,
        member: CertificateFingerprint,
        connection: quinn::Connection,
        cancel: &RevocationSignal,
    ) -> Result<PairedMember, SetupFailure> {
        let index = self.admitted(member)?;
        let (session, inspection) = self
            .negotiate_while_admitted(
                index,
                connection,
                ControlPermissions::BOTH,
                SessionPurpose::Setup,
                [0; 32],
                cancel,
            )
            .await?;
        self.hand_out(index, session, inspection, cancel)
    }

    /// This computer's displays as a connect reads them.
    pub(crate) async fn local_displays(&self) -> Result<DisplayTopology, SetupFailure> {
        read_displays(self.displays, self.local.device).await
    }

    async fn connection(
        &self,
        index: usize,
        cancel: &RevocationSignal,
    ) -> Result<quinn::Connection, SetupFailure> {
        if self.peers[index].dials {
            self.dial(index, cancel).await
        } else {
            self.wait_for(index, cancel).await
        }
    }

    /// Records a negotiated session as the member's live connection with a fresh link cancel,
    /// stopping and closing the one it replaces.
    fn hand_out(
        &self,
        index: usize,
        session: NegotiatedSession,
        inspection: InspectedPeer,
        cancel: &RevocationSignal,
    ) -> Result<PairedMember, SetupFailure> {
        // Forget runs on this thread, so nothing is handed out for a member it already removed.
        if let Err(error) = self.check(index, Some(cancel)) {
            session
                .connection
                .close(0_u32.into(), self.refusal_reason(error));
            return Err(error);
        }
        let link_cancel = RevocationSignal::default();
        let previous = self.live.borrow_mut()[index].replace(LiveLink {
            connection: session.connection.clone(),
            cancel: link_cancel.clone(),
        });
        if let Some(previous) = previous {
            previous.cancel.request_stop();
            previous
                .connection
                .close(0_u32.into(), MEMBER_SUPERSEDED_REASON);
        }
        Ok(PairedMember {
            session,
            inspection,
            cancel: link_cancel,
        })
    }

    /// Closes `member`'s connection, leaving every other member's untouched.
    pub fn close_member(&self, member: CertificateFingerprint) {
        let link = self
            .index(member)
            .and_then(|index| self.live.borrow_mut()[index].take());
        if let Some(link) = link {
            link.cancel.request_stop();
            link.connection.close(0_u32.into(), MEMBER_RELEASED_REASON);
        }
    }

    /// Stops admitting `member` without a rebind: its waiting connect ends, its incoming is
    /// ignored and its connection closed. Only a new bind admits it again.
    pub fn forget(&self, member: CertificateFingerprint) {
        self.excluded
            .borrow_mut()
            .retain(|(excluded, _)| *excluded != member);
        let Some(index) = self.index(member) else {
            return;
        };
        self.endpoint.forget_member(member);
        routes(&self.routes).waiters[index] = None;
        let link = self.live.borrow_mut()[index].take();
        if let Some(link) = link {
            link.cancel.revoke();
            link.connection.close(0_u32.into(), MEMBER_FORGOTTEN_REASON);
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

    /// `member`'s index. A paired computer the bind left out is a route failure, as an endpoint
    /// of its own reports a recorded address off the selected network; only a computer with no
    /// pairing needs pairing.
    fn admitted(&self, member: CertificateFingerprint) -> Result<usize, SetupFailure> {
        if let Some(index) = self.index(member) {
            return Ok(index);
        }
        let excluded = self.excluded.borrow();
        let Some((_, exclusion)) = excluded.iter().find(|(excluded, _)| *excluded == member) else {
            return Err(SetupFailure::PairingRequired);
        };
        note_attempt_failure(format!(
            "group endpoint: paired computer {} is not admitted on this network ({exclusion:?})",
            member.short_hex()
        ));
        Err(SetupFailure::NetworkRoute)
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

    /// Why this endpoint closes a member's connection instead of handing it out.
    fn refusal_reason(&self, error: SetupFailure) -> &'static [u8] {
        if error == SetupFailure::PairingRequired {
            MEMBER_FORGOTTEN_REASON
        } else if self.endpoint.is_revoked() {
            NETWORK_REVOKED_REASON
        } else {
            MEMBER_RELEASED_REASON
        }
    }

    /// Negotiates only while the member stays admitted and nothing cancels. The check runs
    /// before the Hello goes out, and a stop found midway closes the connection with its reason
    /// before the unfinished handshake is dropped.
    async fn negotiate_while_admitted(
        &self,
        index: usize,
        connection: quinn::Connection,
        control: ControlPermissions,
        purpose: SessionPurpose,
        agreement: [u8; 32],
        cancel: &RevocationSignal,
    ) -> Result<(NegotiatedSession, InspectedPeer), SetupFailure> {
        let closing = connection.clone();
        let negotiation = self.negotiate(index, connection, control, purpose, agreement);
        tokio::pin!(negotiation);
        let result = self
            .while_member_active(index, cancel, &mut negotiation)
            .await;
        if let Err(error @ (SetupFailure::PairingRequired | SetupFailure::Cancelled)) = result {
            closing.close(0_u32.into(), self.refusal_reason(error));
        }
        result
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
        let local_displays = self.local_displays().await?;
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
                self.negotiate_while_admitted(
                    index, connection, control, purpose, agreement, cancel,
                )
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
    pub(crate) fn waiting(&self, member: CertificateFingerprint) -> bool {
        routes(&self.routes).claimed(member)
    }

    /// A group endpoint on a loopback `socket` for `identity` on `platform`, admitting each
    /// `(identity, address, this computer dials it)` member in order, on the other platform.
    #[cfg(test)]
    pub(crate) fn over_loopback(
        socket: std::net::UdpSocket,
        identity: DeviceIdentity,
        platform: Platform,
        members: &[(&DeviceIdentity, SocketAddrV4, bool)],
        controls: &[GroupMemberRecord],
        cancel: &RevocationSignal,
        displays: DisplayReader,
    ) -> Rc<Self> {
        use crate::guarded_endpoint::loopback::pin;
        let group: Vec<_> = members
            .iter()
            .map(|(member, address, _)| GroupMember {
                address: *address,
                pin: pin(member),
            })
            .collect();
        let endpoint = GuardedEndpoint::over_loopback(socket, &identity, &group).unwrap();
        let peers = members
            .iter()
            .map(|(member, _, dials)| GroupPeer {
                fingerprint: member.fingerprint(),
                pin: pin(member),
                device: device_id_from_fingerprint(member.fingerprint()),
                platform: opposite_platform(platform),
                dials: *dials,
            })
            .collect();
        Self::assemble(
            endpoint,
            LocalFacts::new(identity, platform),
            peers,
            Vec::new(),
            controls,
            "loopback-test",
            cancel,
            displays,
        )
        .unwrap()
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
    use std::{
        net::{SocketAddr, UdpSocket},
        sync::atomic::{AtomicUsize, Ordering},
    };

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
        hub_reading(members, dials, controls, fixture_displays)
    }

    /// `hub`, reading this computer's displays with `displays`.
    fn hub_reading(
        members: &[&Computer],
        dials: bool,
        controls: &[GroupMemberRecord],
        displays: DisplayReader,
    ) -> Hub {
        let identity = DeviceIdentity::generate().unwrap();
        let socket = loopback::bind();
        let address = socket.local_addr().unwrap();
        let members: Vec<_> = members
            .iter()
            .map(|member| (&member.identity, member.address, dials))
            .collect();
        let hub_pin = pin(&identity);
        let cancel = RevocationSignal::default();
        let group = GroupEndpoint::over_loopback(
            socket, identity, HUB, &members, controls, &cancel, displays,
        );
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

    /// Connects `remote` once the hub waits for it, reads the start of the hub's Hello, then
    /// runs `interrupt` without ever answering, and reports how the hub closed the connection.
    async fn interrupted_negotiation(
        remote: &Remote,
        hub: &Hub,
        interrupt: impl FnOnce(),
    ) -> (quinn::ConnectionError, Duration) {
        until(|| hub.group.waiting(remote.fingerprint())).await;
        let connection = remote
            .endpoint
            .connect(hub.address, LOCAL_TLS_SERVER_NAME)
            .unwrap()
            .await
            .unwrap();
        let (_, mut hello) = connection.accept_bi().await.unwrap();
        let mut first_byte = [0_u8; 1];
        hello.read_exact(&mut first_byte).await.unwrap();
        let interrupted = Instant::now();
        interrupt();
        let closed = connection.closed().await;
        (closed, interrupted.elapsed())
    }

    fn closed_with(error: &quinn::ConnectionError, reason: &[u8]) -> bool {
        matches!(
            error,
            quinn::ConnectionError::ApplicationClosed(close) if close.reason.as_ref() == reason
        )
    }

    #[tokio::test]
    async fn a_member_forgotten_mid_negotiation_gets_no_session() {
        use crate::session_handshake::HANDSHAKE_DEADLINE;
        let computers = [(); 2].map(|()| Computer::new());
        let hub = hub(&computers.each_ref(), false, &[]);
        let [forgotten, kept] = computers.map(|computer| computer.dialing(&hub));
        let setup = SessionPurpose::Setup;

        let fingerprint = forgotten.fingerprint();
        let (ours, (closed, took)) = tokio::time::timeout(DEADLINE, async {
            tokio::join!(
                hub.group
                    .connect(fingerprint, setup, NO_AGREEMENT, &hub.cancel),
                interrupted_negotiation(&forgotten, &hub, || hub.group.forget(fingerprint))
            )
        })
        .await
        .expect("both sides settle the forget");
        assert_eq!(ours.err(), Some(SetupFailure::PairingRequired));
        assert!(closed_with(&closed, MEMBER_FORGOTTEN_REASON), "{closed}");
        assert!(took < HANDSHAKE_DEADLINE, "ended after {took:?}");

        // A connect cancelled midway gets no session either; the endpoint and the other member
        // carry on.
        let cancel = RevocationSignal::default();
        let fingerprint = kept.fingerprint();
        let (ours, (closed, took)) = tokio::time::timeout(DEADLINE, async {
            tokio::join!(
                hub.group.connect(fingerprint, setup, NO_AGREEMENT, &cancel),
                interrupted_negotiation(&kept, &hub, || cancel.revoke())
            )
        })
        .await
        .expect("both sides settle the cancel");
        assert_eq!(ours.err(), Some(SetupFailure::Cancelled));
        assert!(closed_with(&closed, MEMBER_RELEASED_REASON), "{closed}");
        assert!(took < HANDSHAKE_DEADLINE, "ended after {took:?}");
        assert!(!hub.group.revocation().is_stopping());
        let (paired, _dialed) = tokio::time::timeout(DEADLINE, async {
            tokio::join!(
                hub.group
                    .connect(fingerprint, setup, NO_AGREEMENT, &hub.cancel),
                async {
                    until(|| hub.group.waiting(fingerprint)).await;
                    remote_dials(&kept, &hub).await
                }
            )
        })
        .await
        .expect("the kept computer still connects");
        assert!(paired.unwrap().inspection.peer_fingerprint == fingerprint);
    }

    /// The hub dials `remote` for a setup link while it answers.
    async fn dial_setup(
        hub: &Hub,
        remote: &Remote,
    ) -> (
        Result<PairedMember, SetupFailure>,
        Result<NegotiatedSession, HandshakeError>,
    ) {
        tokio::time::timeout(DEADLINE, async {
            tokio::join!(
                hub.group.connect(
                    remote.fingerprint(),
                    SessionPurpose::Setup,
                    NO_AGREEMENT,
                    &hub.cancel
                ),
                async {
                    let connection = remote.endpoint.accept().await.unwrap().await.unwrap();
                    remote_negotiates(remote, hub, connection, SessionPurpose::Setup, NO_AGREEMENT)
                        .await
                }
            )
        })
        .await
        .expect("the member answers")
    }

    #[tokio::test]
    async fn a_dial_to_an_unreachable_member_is_a_transient_connection_failure() {
        let computers = [(); 2].map(|()| Computer::new());
        let hub = hub(&computers.each_ref(), true, &[]);
        let [down, up] = computers.map(|computer| computer.listening(&hub));

        hub.group.endpoint.set_reachable(&[false, true]);
        let failure = tokio::time::timeout(
            DEADLINE,
            hub.group.connect(
                down.fingerprint(),
                SessionPurpose::Setup,
                NO_AGREEMENT,
                &hub.cancel,
            ),
        )
        .await
        .expect("the dial fails at once")
        .err()
        .expect("no session without a route");
        assert_eq!(failure, SetupFailure::Connection);
        assert!(is_transient(failure));
        let (paired, answered) = dial_setup(&hub, &up).await;
        assert!(paired.unwrap().inspection.peer_fingerprint == up.fingerprint());
        answered.unwrap();

        // The route check that finds it again admits the next dial.
        hub.group.endpoint.set_reachable(&[true, true]);
        let (paired, answered) = dial_setup(&hub, &down).await;
        assert!(paired.unwrap().inspection.peer_fingerprint == down.fingerprint());
        answered.unwrap();
    }

    #[tokio::test]
    async fn each_link_has_its_own_cancel_apart_from_the_endpoints() {
        let computer = Computer::new();
        let hub = hub(&[&computer], false, &[]);
        let remote = computer.dialing(&hub);
        let fingerprint = remote.fingerprint();
        let link = || async {
            let (paired, dialed) = tokio::time::timeout(DEADLINE, async {
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
            .expect("the computer connects");
            (paired.unwrap(), dialed)
        };

        // A newer link stops the one it replaces, and only that one.
        let (first, _first_dialed) = link().await;
        let (second, _second_dialed) = link().await;
        assert!(first.cancel.is_stopping() && !first.cancel.is_revoked());
        assert!(!second.cancel.is_stopping());

        // A session stopping its own link leaves the endpoint and the connection to the caller.
        second.cancel.request_stop();
        assert!(!hub.group.revocation().is_stopping());
        assert!(second.session.connection.close_reason().is_none());

        let (third, _third_dialed) = link().await;
        hub.group.close_member(fingerprint);
        assert!(third.cancel.is_stopping() && !third.cancel.is_revoked());

        let (fourth, _fourth_dialed) = link().await;
        hub.group.forget(fingerprint);
        assert!(fourth.cancel.is_revoked());
        assert!(!hub.group.revocation().is_stopping());
    }

    /// The selected network: 192.168.50.0/24.
    fn subnet() -> InterfaceSnapshot {
        InterfaceSnapshot {
            stable_id: "adapter".into(),
            name: "ethernet".into(),
            index: 7,
            address: Ipv4Addr::new(192, 168, 50, 10),
            prefix_len: 24,
            kind: InterfaceKind::Ethernet,
            is_hardware: true,
            is_up: true,
            network_signature: vec![1; 32],
        }
    }

    /// `local`'s pairing with `identity`, recorded at `host`.
    fn record(
        local: &DeviceIdentity,
        host: [u8; 4],
        identity: &DeviceIdentity,
    ) -> ConfirmedPeerRecord {
        let offer = PairingOffer::new(
            SocketAddrV4::new(host.into(), PAIRING_PORT),
            identity.certificate_der(),
        )
        .unwrap()
        .with_platform(MEMBER);
        ConfirmedPeerRecord::new(local.fingerprint(), offer).unwrap()
    }

    fn in_group(identity: &DeviceIdentity) -> GroupMemberRecord {
        GroupMemberRecord {
            fingerprint: identity.fingerprint(),
            control: ControlPermissions::BOTH,
        }
    }

    fn admission(
        local: &DeviceIdentity,
        records: &[ConfirmedPeerRecord],
        members: &[GroupMemberRecord],
    ) -> Admission {
        admitted_peers(&subnet(), local.fingerprint(), HUB, records, members)
    }

    fn admitted_fingerprints(admission: &Admission) -> Vec<CertificateFingerprint> {
        admission
            .admitted
            .iter()
            .map(|(peer, _)| peer.fingerprint)
            .collect()
    }

    #[test]
    fn the_admitted_set_is_every_paired_computer_on_the_selected_subnet() {
        let local = DeviceIdentity::generate().unwrap();
        let ids: Vec<_> = (0..9)
            .map(|_| DeviceIdentity::generate().unwrap())
            .collect();
        let records = [
            record(&local, [192, 168, 50, 20], &ids[0]),
            record(&local, [192, 168, 50, 21], &ids[1]),
            record(&local, [10, 0, 0, 5], &ids[2]),
            record(&local, [192, 168, 50, 20], &ids[3]),
        ];
        let Admission { admitted, excluded } = admission(&local, &records, &[in_group(&ids[1])]);
        let admitted: Vec<_> = admitted
            .iter()
            .map(|(peer, address)| (peer.fingerprint, *address, peer.dials))
            .collect();
        // The group member first, then any paired computer on the subnet; another subnet and a
        // second computer at a taken address are left out, and remembered with why.
        assert!(
            admitted
                == [
                    (ids[1].fingerprint(), records[1].peer().endpoint(), true),
                    (ids[0].fingerprint(), records[0].peer().endpoint(), true),
                ]
        );
        assert!(
            excluded
                == [
                    (ids[2].fingerprint(), Exclusion::OffSubnet),
                    (ids[3].fingerprint(), Exclusion::AddressTaken),
                ]
        );

        let crowded: Vec<_> = (0..9_u8)
            .map(|host| record(&local, [192, 168, 50, 30 + host], &ids[usize::from(host)]))
            .collect();
        let crowded = admission(&local, &crowded, &[in_group(&ids[8])]);
        assert_eq!(crowded.admitted.len(), MAX_PINNED_PEERS);
        assert!(crowded.admitted[0].0.fingerprint == ids[8].fingerprint());
        assert!(
            crowded.excluded
                == [
                    (ids[6].fingerprint(), Exclusion::Full),
                    (ids[7].fingerprint(), Exclusion::Full),
                ]
        );
    }

    /// A loopback hub admitting one computer, which bind left `excluded` beside.
    fn hub_excluding(excluded: Vec<(CertificateFingerprint, Exclusion)>) -> (Hub, Computer) {
        let computer = Computer::new();
        let hub = hub(&[&computer], false, &[]);
        *hub.group.excluded.borrow_mut() = excluded;
        (hub, computer)
    }

    async fn setup_connect(hub: &Hub, member: CertificateFingerprint) -> Option<SetupFailure> {
        tokio::time::timeout(
            DEADLINE,
            hub.group
                .connect(member, SessionPurpose::Setup, NO_AGREEMENT, &hub.cancel),
        )
        .await
        .expect("the connect settles at once")
        .err()
    }

    #[tokio::test]
    async fn a_paired_computer_off_the_subnet_is_a_network_route_failure() {
        let local = DeviceIdentity::generate().unwrap();
        let away = DeviceIdentity::generate().unwrap();
        let records = [record(&local, [10, 0, 0, 5], &away)];
        let admission = admission(&local, &records, &[in_group(&away)]);
        assert!(admission.admitted.is_empty());
        let (hub, _computer) = hub_excluding(admission.excluded);
        let member = away.fingerprint();

        // As an endpoint of its own reports it, and retried as a route failure, never as pairing.
        assert_eq!(
            setup_connect(&hub, member).await,
            Some(SetupFailure::NetworkRoute)
        );
        let share = hub
            .group
            .connect(member, SessionPurpose::Share, NO_AGREEMENT, &hub.cancel)
            .await;
        assert_eq!(share.err(), Some(SetupFailure::NetworkRoute));
        assert_eq!(hub.group.dials(member), Err(SetupFailure::NetworkRoute));
        assert_eq!(
            hub.group.check_member(member),
            Err(SetupFailure::NetworkRoute)
        );
        assert!(!hub.group.admits(member));

        // Removing the pairing leaves no trust record at all.
        hub.group.forget(member);
        assert_eq!(
            setup_connect(&hub, member).await,
            Some(SetupFailure::PairingRequired)
        );
    }

    #[tokio::test]
    async fn a_stale_record_sharing_an_address_never_displaces_an_enabled_member() {
        let local = DeviceIdentity::generate().unwrap();
        let [stale, enabled] = [(); 2].map(|()| DeviceIdentity::generate().unwrap());
        let at_stale = record(&local, [192, 168, 50, 20], &stale);
        let at_enabled = record(&local, [192, 168, 50, 20], &enabled);
        for records in [
            [at_stale.clone(), at_enabled.clone()],
            [at_enabled.clone(), at_stale.clone()],
        ] {
            let admission = admission(&local, &records, &[in_group(&enabled)]);
            assert!(admitted_fingerprints(&admission) == [enabled.fingerprint()]);
            assert!(admission.excluded == [(stale.fingerprint(), Exclusion::AddressTaken)]);
        }

        // The stale record's connects are a route failure, not a pairing to redo.
        let (hub, _computer) = hub_excluding(
            admission(&local, &[at_stale, at_enabled], &[in_group(&enabled)]).excluded,
        );
        assert_eq!(
            setup_connect(&hub, stale.fingerprint()).await,
            Some(SetupFailure::NetworkRoute)
        );
    }

    #[tokio::test]
    async fn an_unpaired_computer_is_still_pairing_required() {
        let local = DeviceIdentity::generate().unwrap();
        let [away, stranger] = [(); 2].map(|()| DeviceIdentity::generate().unwrap());
        let records = [record(&local, [10, 0, 0, 5], &away)];
        let (hub, computer) = hub_excluding(admission(&local, &records, &[]).excluded);
        let member = stranger.fingerprint();
        assert_eq!(
            setup_connect(&hub, member).await,
            Some(SetupFailure::PairingRequired)
        );
        assert_eq!(hub.group.dials(member), Err(SetupFailure::PairingRequired));
        assert_eq!(
            hub.group.check_member(member),
            Err(SetupFailure::PairingRequired)
        );
        // The admitted computer is untouched by either.
        assert!(hub.group.admits(computer.identity.fingerprint()));
        assert_eq!(
            hub.group.check_member(computer.identity.fingerprint()),
            Ok(())
        );
    }

    /// Ticks of a task running beside the display reads.
    static TICKS: AtomicUsize = AtomicUsize::new(0);
    /// How many of them landed during each read.
    static TICKS_DURING_READS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

    /// Well inside the other computer's handshake deadline, which runs while the hub reads.
    fn slow_displays(_: DeviceId) -> Result<DisplayTopology, SetupFailure> {
        let before = TICKS.load(Ordering::Acquire);
        std::thread::sleep(Duration::from_millis(200));
        let during = TICKS.load(Ordering::Acquire) - before;
        TICKS_DURING_READS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(during);
        Ok(topology())
    }

    /// The test runtime has one thread, as the network thread does.
    #[tokio::test]
    async fn a_slow_display_read_does_not_block_the_network_runtime() {
        let ticker = tokio::spawn(async {
            loop {
                tokio::time::sleep(Duration::from_millis(1)).await;
                TICKS.fetch_add(1, Ordering::AcqRel);
            }
        });
        let computer = Computer::new();
        let hub = hub_reading(&[&computer], false, &[], slow_displays);
        let remote = computer.dialing(&hub);
        let fingerprint = remote.fingerprint();

        // A setup link's topology poll, then a negotiation.
        let polled = hub.group.local_displays().await.unwrap();
        assert!(polled.same_geometry(&topology()));
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
        .expect("the computer connects");
        assert!(paired.unwrap().inspection.peer_fingerprint == fingerprint);
        ticker.abort();

        let reads = TICKS_DURING_READS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        assert_eq!(reads.len(), 2);
        assert!(
            reads.iter().all(|&ticks| ticks >= 3),
            "the runtime stalled while displays were read: {reads:?}"
        );
    }
}
