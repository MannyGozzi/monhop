//! Explicit paired metadata inspection and fresh session checks before input starts.

use std::{
    future::Future,
    net::SocketAddrV4,
    sync::{Mutex, PoisonError, mpsc},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use monhop_core::{
    DeviceId, DisplayId, EdgeLink, Machine, Platform, Point, RevocationSignal, Topology,
};
use monhop_protocol::{Capabilities, ControlPermissions};

pub use monhop_protocol::{
    DisplayDescription, DisplayTopology, MAX_DISPLAY_NAME_BYTES, MAX_LOGICAL_ORIGIN_ABS,
    MAX_LOGICAL_SIZE, MAX_NATIVE_DIMENSION, MAX_SCALE_FACTOR, MIN_SCALE_FACTOR,
};

use crate::{
    crypto::{CertificateFingerprint, DeviceIdentity, VerifiedPeer, take_refused_certificate},
    guarded_endpoint::{GuardedEndpoint, NetworkSelection},
    identity_store::{ProtectedPeerStore, load_identity},
    native_storage::{NativeIdentityStore, NativePeerStore},
    pairing::{ConfirmedPeerRecord, PAIRING_PORT, initiates_connection, opposite_platform},
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
            note_attempt_failure(format!("share dial could not start: {error}"));
            SetupFailure::Connection
        })?;
        self.while_active(cancel, async move {
            connecting
                .await
                .map_err(|error| connection_failure("share dial failed", &error, Some(&error)))
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
                connection_failure("share accept failed", &error, accept_cause(&error))
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
            if purpose == SessionPurpose::Setup {
                ControlPermissions::BOTH
            } else {
                self.control
            },
            purpose,
        )
        .map_err(|_| SetupFailure::Handshake)?;
        // A pin refused after the client finished its handshake surfaces here, as the close.
        let closing = connection.clone();
        let session =
            negotiate(connection, config)
                .await
                .map_err(|error| match closing.close_reason() {
                    Some(reason) if refused_identity(&reason) => {
                        identity_changed("share handshake closed", &reason)
                    }
                    _ => handshake_failure(error),
                })?;
        if cancel_or_endpoint_revoked(None, &self.endpoint) {
            return Err(SetupFailure::Cancelled);
        }
        let inspection = InspectedPeer {
            local_device: self.local_device,
            peer_device: self.peer_device,
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

/// A pin refusal on either side. Locally, `verify_exact_leaf`'s ApplicationVerificationFailure
/// is sent as AccessDenied; a peer refusing this computer's certificate closes with that alert or
/// BadCertificate.
pub(crate) fn refused_identity(error: &quinn::ConnectionError) -> bool {
    use rustls::AlertDescription;
    let alert =
        |description: AlertDescription| quinn::TransportErrorCode::crypto(description.into());
    match error {
        quinn::ConnectionError::TransportError(local) => {
            local.code == alert(AlertDescription::AccessDenied)
        }
        quinn::ConnectionError::ConnectionClosed(peer) => {
            peer.error_code == alert(AlertDescription::AccessDenied)
                || peer.error_code == alert(AlertDescription::BadCertificate)
        }
        _ => false,
    }
}

/// The endpoint wraps a failed incoming handshake in an I/O error.
fn accept_cause(error: &std::io::Error) -> Option<&quinn::ConnectionError> {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<quinn::ConnectionError>())
}

fn connection_failure(
    what: &str,
    error: &dyn std::fmt::Display,
    cause: Option<&quinn::ConnectionError>,
) -> SetupFailure {
    match cause {
        Some(cause) if refused_identity(cause) => identity_changed(what, error),
        _ => {
            note_attempt_failure(format!("{what}: {error}"));
            SetupFailure::Connection
        }
    }
}

/// Warns once per refused identity with its short fingerprint; repeats go to the collapsed DEBUG
/// log.
fn identity_changed(what: &str, error: &dyn std::fmt::Display) -> SetupFailure {
    let refused = take_refused_certificate();
    let line = format!("{what}: {error}");
    let mut log = ATTEMPT_LOG.lock().unwrap_or_else(PoisonError::into_inner);
    let now = Instant::now();
    if log.first_refusal(refused) {
        match refused {
            Some(certificate) => log::warn!(
                "session setup: the other computer presented certificate {}, not the paired \
                 identity; pairing the computers again fixes this",
                certificate.short_hex()
            ),
            None => log::warn!(
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
        // A changed saved control map must be agreed over the setup link.
        HandshakeError::ControlMismatch => SetupFailure::ChangedSinceInspection,
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

    /// One pinned side sees a certificate other than the paired one; both sides must call it an
    /// identity change, never a connection that may succeed on the next dial.
    #[tokio::test]
    async fn a_certificate_other_than_the_paired_one_is_an_identity_change_on_both_sides() {
        use crate::crypto::{LOCAL_TLS_SERVER_NAME, SecureQuicConfig};
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
            let (client_pin, server_pin, refused) = if server_refuses {
                (pin(&stale), pin(&server), client.fingerprint())
            } else {
                (pin(&client), pin(&stale), server.fingerprint())
            };
            let local: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
            let listener = quinn::Endpoint::server(
                SecureQuicConfig::server(&server, &client_pin).unwrap(),
                local,
            )
            .unwrap();
            let mut dialer = quinn::Endpoint::client(local).unwrap();
            dialer
                .set_default_client_config(SecureQuicConfig::client(&client, &server_pin).unwrap());
            let connecting = dialer
                .connect(listener.local_addr().unwrap(), LOCAL_TLS_SERVER_NAME)
                .unwrap();
            let (dialed, accepted) = tokio::time::timeout(Duration::from_secs(10), async {
                tokio::join!(
                    async {
                        match connecting.await {
                            // The client can finish first; the server's refusal then closes it.
                            Ok(connection) => connection.closed().await,
                            Err(error) => error,
                        }
                    },
                    async { listener.accept().await.unwrap().await.unwrap_err() }
                )
            })
            .await
            .expect("both sides settle the refused handshake");
            assert!(take_refused_certificate() == Some(refused));
            assert!(refused_identity(&dialed), "dialer saw {dialed}");
            assert!(refused_identity(&accepted), "listener saw {accepted}");
            let wrapped = std::io::Error::other(accepted.clone());
            assert_eq!(
                connection_failure("share accept failed", &wrapped, accept_cause(&wrapped)),
                SetupFailure::PeerIdentityChanged
            );
            assert_eq!(
                connection_failure("share dial failed", &dialed, Some(&dialed)),
                SetupFailure::PeerIdentityChanged
            );
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
            connection_failure("share dial failed", &timed_out, Some(&timed_out)),
            SetupFailure::Connection
        );
    }
}
