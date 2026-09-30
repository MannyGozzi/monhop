//! Explicit, physical-interface-bound QUIC. Pairing never authorizes input.

mod native;
mod runtime;
mod socket;

use std::{
    fmt, io,
    marker::PhantomData,
    net::{SocketAddr, SocketAddrV4},
    rc::Rc,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use monhop_core::revocation::RevocationSignal;
use quinn::AsyncUdpSocket;
use tokio::{sync::watch, task::JoinSet, time::Instant};

use crate::{
    crypto::{
        CertificateFingerprint, DeviceIdentity, LOCAL_TLS_SERVER_NAME, RefusedCertificate,
        SecureQuicConfig, VerifiedPeer,
    },
    policy::{MAX_PINNED_PEERS, is_private_or_link_local},
};
use runtime::HandleRuntime;
use socket::{DatagramIo, GuardedSocket, Reachability};

/// Incoming handshakes one member may have running at once; more are refused until one ends.
const MAX_MEMBER_HANDSHAKES: usize = 2;
/// Incoming handshakes one member may start back to back before its budget runs dry.
const MEMBER_HANDSHAKE_BURST: u32 = 4;
/// How often a member's budget regains one incoming handshake.
const MEMBER_HANDSHAKE_REFILL: Duration = Duration::from_secs(1);
/// An incoming handshake still unfinished after this is closed, freeing its member's slot.
const INCOMING_HANDSHAKE_DEADLINE: Duration = Duration::from_secs(3);

/// Identifies a failed native route check without exposing platform error details.
#[derive(Debug)]
pub struct RouteCheckFailure;

impl std::fmt::Display for RouteCheckFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the selected physical route could not be verified")
    }
}

impl std::error::Error for RouteCheckFailure {}

/// A paired computer's incoming handshake failed, or finished without binding to that computer.
/// Either way its connection is closed; other members are unaffected.
///
/// Neither `member` nor `cause` is authenticated: `member` names whichever member's recorded
/// address the attempt came from, and any cause but [`Self::certificate_refused`] may be a close
/// the remote sent before the handshake proved who it was.
pub struct MemberHandshakeFailure {
    pub member: CertificateFingerprint,
    /// None when the handshake finished but did not come from the member's address and pin.
    pub cause: Option<quinn::ConnectionError>,
    /// The certificate this computer's pin refused on this attempt, when it refused one.
    pub refused: Option<CertificateFingerprint>,
}

impl MemberHandshakeFailure {
    /// Whether this computer's pin refused the certificate presented on this attempt: the only
    /// handshake failure that says the member's identity changed.
    pub fn certificate_refused(&self) -> bool {
        self.cause.as_ref().is_some_and(refused_by_this_computer)
    }

    fn into_io_error(self) -> io::Error {
        let kind = if self.cause.is_some() {
            io::ErrorKind::Other
        } else {
            io::ErrorKind::PermissionDenied
        };
        io::Error::new(kind, self)
    }
}

impl fmt::Debug for MemberHandshakeFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemberHandshakeFailure")
            .field("member", &self.member.short_hex())
            .field("cause", &self.cause)
            .field(
                "refused",
                &self.refused.map(CertificateFingerprint::short_hex),
            )
            .finish()
    }
}

impl fmt::Display for MemberHandshakeFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let member = self.member.short_hex();
        match &self.cause {
            Some(cause) => write!(formatter, "the handshake with {member} failed: {cause}"),
            None => write!(formatter, "the connection did not bind to {member}"),
        }
    }
}

impl std::error::Error for MemberHandshakeFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause
            .as_ref()
            .map(|cause| cause as &(dyn std::error::Error + 'static))
    }
}

/// This computer's pin refused the certificate presented on this connection. Quinn reports an
/// alert this computer raised as a transport error and a remote's close as `ConnectionClosed`; the
/// pin's refusal is raised as `AccessDenied`.
pub(crate) fn refused_by_this_computer(error: &quinn::ConnectionError) -> bool {
    matches!(
        error,
        quinn::ConnectionError::TransportError(local)
            if local.code
                == quinn::TransportErrorCode::crypto(
                    rustls::AlertDescription::AccessDenied.into()
                )
    )
}

/// A user-selected adapter identity and exact numeric endpoints. No discovery or port fallback.
#[derive(Clone, Debug)]
pub struct NetworkSelection {
    pub stable_id: String,
    pub interface_index: u32,
    pub local: SocketAddrV4,
    pub peer: SocketAddrV4,
}

impl NetworkSelection {
    fn validate(&self) -> io::Result<()> {
        if self.stable_id.is_empty()
            || self.stable_id.len() > 256
            || self.interface_index == 0
            || self.local.port() == 0
            || self.peer.port() == 0
            || !is_private_or_link_local(*self.local.ip())
            || !is_private_or_link_local(*self.peer.ip())
            || self.local.ip() == self.peer.ip()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "select an exact physical adapter, distinct private IPv4 addresses and nonzero ports",
            ));
        }
        Ok(())
    }
}

/// One paired computer a group endpoint admits: its recorded address and pinned certificate.
#[derive(Clone)]
pub struct GroupMember {
    pub address: SocketAddrV4,
    pub pin: VerifiedPeer,
}

/// A user-selected adapter and the fixed set of paired computers one endpoint admits. The set
/// never grows for the endpoint's life; a different set needs a new bind.
#[derive(Clone)]
pub struct GroupSelection {
    pub stable_id: String,
    pub interface_index: u32,
    pub local: SocketAddrV4,
    pub members: Vec<GroupMember>,
}

impl GroupSelection {
    /// Every member needs its own address and its own certificate, never this computer's.
    fn validate(&self, identity: &DeviceIdentity) -> io::Result<PinnedNetwork> {
        let addresses: Vec<_> = self.members.iter().map(|member| member.address).collect();
        let pinned = PinnedNetwork::new(
            &self.stable_id,
            self.interface_index,
            self.local,
            &addresses,
        )?;
        validate_pins(identity, &self.members)?;
        Ok(pinned)
    }
}

fn validate_pins(identity: &DeviceIdentity, members: &[GroupMember]) -> io::Result<()> {
    let local = identity.fingerprint();
    for (index, member) in members.iter().enumerate() {
        let pin = member.pin.fingerprint();
        if pin == local
            || members[..index]
                .iter()
                .any(|earlier| earlier.pin.fingerprint() == pin)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "each paired computer needs its own certificate",
            ));
        }
    }
    Ok(())
}

/// The adapter, local address and paired peer addresses one socket is pinned to.
#[derive(Clone, Debug)]
struct PinnedNetwork {
    stable_id: String,
    interface_index: u32,
    local: SocketAddrV4,
    peers: Box<[SocketAddrV4]>,
}

impl PinnedNetwork {
    fn new(
        stable_id: &str,
        interface_index: u32,
        local: SocketAddrV4,
        peers: &[SocketAddrV4],
    ) -> io::Result<Self> {
        let valid_peer = |index: usize, peer: &SocketAddrV4| {
            peer.port() != 0
                && is_private_or_link_local(*peer.ip())
                && peer.ip() != local.ip()
                && !peers[..index]
                    .iter()
                    .any(|earlier| earlier.ip() == peer.ip())
        };
        if stable_id.is_empty()
            || stable_id.len() > 256
            || interface_index == 0
            || local.port() == 0
            || !is_private_or_link_local(*local.ip())
            || !(1..=MAX_PINNED_PEERS).contains(&peers.len())
            || !peers
                .iter()
                .enumerate()
                .all(|(index, peer)| valid_peer(index, peer))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "select an exact physical adapter, 1 to 7 distinct private IPv4 peers and nonzero ports",
            ));
        }
        Ok(Self {
            stable_id: stable_id.to_owned(),
            interface_index,
            local,
            peers: peers.into(),
        })
    }
}

/// One member's configurations. Its verifiers pin exactly its certificate, and the accept path
/// picks them only for its exact recorded address.
struct Member {
    address: SocketAddrV4,
    fingerprint: CertificateFingerprint,
    client: quinn::ClientConfig,
    server: Arc<quinn::ServerConfig>,
    /// What this member's two pins last refused.
    refused: RefusedCertificate,
    /// Set once the member is forgotten, and never cleared for the endpoint's life.
    forgotten: AtomicBool,
}

impl Member {
    fn admitted(&self) -> bool {
        !self.forgotten.load(Ordering::SeqCst)
    }
}

fn member_configs(identity: &DeviceIdentity, members: &[GroupMember]) -> io::Result<Box<[Member]>> {
    members
        .iter()
        .map(|member| {
            let refused = RefusedCertificate::default();
            Ok(Member {
                address: member.address,
                fingerprint: member.pin.fingerprint(),
                client: SecureQuicConfig::client_recording(identity, &member.pin, refused.clone())
                    .map_err(io::Error::other)?,
                server: Arc::new(
                    SecureQuicConfig::server_recording(identity, &member.pin, refused.clone())
                        .map_err(io::Error::other)?,
                ),
                refused,
                forgotten: AtomicBool::new(false),
            })
        })
        .collect()
}

/// What the endpoint's cross-thread handles share. The slot empties when the owner drops, so a
/// stray handle never keeps the pinned socket open.
struct Shared {
    members: Box<[Member]>,
    /// The socket's route verdict for each member, in member order.
    reachability: Arc<Reachability>,
    signal: RevocationSignal,
    endpoint: Mutex<Option<quinn::Endpoint>>,
}

impl Shared {
    /// Only a member still admitted: a forgotten one is no longer paired on this endpoint.
    fn member(&self, fingerprint: CertificateFingerprint) -> io::Result<&Member> {
        self.admitted_member(fingerprint).map(|(_, member)| member)
    }

    fn admitted_member(&self, fingerprint: CertificateFingerprint) -> io::Result<(usize, &Member)> {
        let (index, member) = self
            .members
            .iter()
            .enumerate()
            .find(|(_, member)| member.fingerprint == fingerprint)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "that computer is not paired on this endpoint",
                )
            })?;
        if !member.admitted() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "that computer is no longer paired on this endpoint",
            ));
        }
        Ok((index, member))
    }

    /// The admitted member recorded at exactly `remote`, IP and port.
    fn admitted_index(&self, remote: SocketAddr) -> Option<usize> {
        let SocketAddr::V4(remote) = remote else {
            return None;
        };
        self.members
            .iter()
            .position(|member| member.address == remote)
            .filter(|&index| self.members[index].admitted())
    }

    fn admits(&self, fingerprint: CertificateFingerprint) -> bool {
        self.member(fingerprint).is_ok()
    }

    fn forget(&self, fingerprint: CertificateFingerprint) {
        if let Some(member) = self
            .members
            .iter()
            .find(|member| member.fingerprint == fingerprint)
        {
            member.forgotten.store(true, Ordering::SeqCst);
        }
    }

    fn take_refused(&self, fingerprint: CertificateFingerprint) -> Option<CertificateFingerprint> {
        self.members
            .iter()
            .find(|member| member.fingerprint == fingerprint)
            .and_then(|member| member.refused.take())
    }

    fn endpoint(&self) -> Option<quinn::Endpoint> {
        self.endpoint
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn check_active(&self) -> io::Result<()> {
        if self.signal.is_revoked() {
            Err(socket::revoked_error())
        } else {
            Ok(())
        }
    }

    #[track_caller]
    fn revoke(&self) {
        self.signal.revoke();
        if let Some(endpoint) = self.endpoint() {
            endpoint.close(0_u32.into(), b"network revoked");
        }
    }

    /// Quinn never reopens a stopped endpoint: revoking it makes the next attempt rebind instead
    /// of failing on this socket forever.
    #[track_caller]
    fn stopped(&self, action: &str) -> io::Error {
        if !self.signal.is_revoked() {
            log::warn!("the endpoint can no longer {action}; revoked so the next attempt rebinds");
        }
        self.revoke();
        socket::revoked_error()
    }

    fn connect_member(&self, fingerprint: CertificateFingerprint) -> io::Result<quinn::Connecting> {
        self.check_active()?;
        let (index, member) = self.admitted_member(fingerprint)?;
        // Its datagrams would only be dropped; the caller retries once a route check finds it.
        if !self.reachability.reaches(index) {
            return Err(io::Error::new(
                io::ErrorKind::HostUnreachable,
                "that computer has no usable route on the selected network",
            ));
        }
        let Some(endpoint) = self.endpoint() else {
            return Err(socket::revoked_error());
        };
        let connecting = match endpoint.connect_with(
            member.client.clone(),
            member.address.into(),
            LOCAL_TLS_SERVER_NAME,
        ) {
            Ok(connecting) => connecting,
            Err(quinn::ConnectError::EndpointStopping) => return Err(self.stopped("dial")),
            Err(error) => return Err(io::Error::other(error)),
        };
        self.check_active()?;
        Ok(connecting)
    }

    fn confirm_member(
        &self,
        fingerprint: CertificateFingerprint,
        connection: &quinn::Connection,
    ) -> io::Result<()> {
        let member = self.member(fingerprint).inspect_err(|_| {
            connection.close(0_u32.into(), b"not a paired computer");
        })?;
        if let Err(error) = self.check_active() {
            connection.close(0_u32.into(), b"network revoked");
            return Err(error);
        }
        if bind_to_member(member, connection) {
            Ok(())
        } else {
            Err(unbound(member).into_io_error())
        }
    }
}

/// A finished connection must come from the member's recorded address and present exactly its
/// pinned certificate; anything else is closed.
fn bind_to_member(member: &Member, connection: &quinn::Connection) -> bool {
    if connection.remote_address() == SocketAddr::V4(member.address)
        && observed_fingerprint(connection) == Some(member.fingerprint)
    {
        return true;
    }
    log::warn!(
        "guarded endpoint: a connection did not bind to paired computer {}; closed",
        member.fingerprint.short_hex()
    );
    connection.close(0_u32.into(), b"not the paired computer");
    false
}

fn unbound(member: &Member) -> MemberHandshakeFailure {
    MemberHandshakeFailure {
        member: member.fingerprint,
        cause: None,
        refused: None,
    }
}

fn observed_fingerprint(connection: &quinn::Connection) -> Option<CertificateFingerprint> {
    let certificates = connection
        .peer_identity()?
        .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
        .ok()?;
    let [certificate] = certificates.as_slice() else {
        return None;
    };
    Some(CertificateFingerprint::from_certificate_der(
        certificate.as_ref(),
    ))
}

/// Owns the native watcher on its creating thread and never exposes endpoint rebinding.
/// This is a transport boundary, not proof of human pairing or permission to forward input.
pub struct GuardedEndpoint {
    endpoint: quinn::Endpoint,
    shared: Arc<Shared>,
    /// Empty once an accept loop elsewhere took it; incoming connections never have two owners.
    acceptor: tokio::sync::Mutex<Option<Acceptor>>,
    socket_lifetime: watch::Receiver<()>,
    _readmission: Option<native::Readmission>,
    _watch: Option<native::Watch>,
    _owner_thread: PhantomData<Rc<()>>,
}

/// Every incoming handshake and its admission. One exists per endpoint, so two accept loops never
/// split incoming connections between them.
pub(crate) struct Acceptor {
    shared: Arc<Shared>,
    network: tokio::runtime::Handle,
    running: JoinSet<Handshake>,
    pending: Arc<[AtomicUsize]>,
    budgets: Box<[TokenBucket]>,
}

/// What one accept produced.
pub(crate) enum Accepted {
    /// A finished handshake, already bound to this member's address and pin.
    Member(CertificateFingerprint, quinn::Connection),
    /// One member's attempt failed; its connection is closed.
    Failed(MemberHandshakeFailure),
}

struct Handshake {
    member: usize,
    result: Result<quinn::Connection, quinn::ConnectionError>,
    _slot: PendingSlot,
}

/// Counts one handshake against its member until its result is taken or its task dropped.
struct PendingSlot {
    pending: Arc<[AtomicUsize]>,
    member: usize,
}

impl Drop for PendingSlot {
    fn drop(&mut self) {
        self.pending[self.member].fetch_sub(1, Ordering::Relaxed);
    }
}

/// A token bucket over one member's attempts, so a flood from one recorded address never takes
/// another member's turn or the network thread: `burst` back to back, then one per `refill`.
struct TokenBucket {
    burst: u32,
    refill: Duration,
    tokens: u32,
    refilled: Instant,
}

impl TokenBucket {
    fn new(burst: u32, refill: Duration, now: Instant) -> Self {
        Self {
            burst,
            refill,
            tokens: burst,
            refilled: now,
        }
    }

    fn take(&mut self, now: Instant) -> bool {
        if self.tokens < self.burst {
            let earned =
                now.saturating_duration_since(self.refilled).as_nanos() / self.refill.as_nanos();
            let earned = u32::try_from(earned).unwrap_or(u32::MAX);
            if earned > 0 {
                self.tokens = self.tokens.saturating_add(earned).min(self.burst);
                // Below the cap, earned < burst, so the product cannot overflow.
                self.refilled = if self.tokens == self.burst {
                    now
                } else {
                    self.refilled + self.refill * earned
                };
            }
        }
        if self.tokens == 0 {
            return false;
        }
        // A full bucket earns nothing, so its refill clock starts at the first take.
        if self.tokens == self.burst {
            self.refilled = now;
        }
        self.tokens -= 1;
        true
    }
}

/// Incoming handshakes one member may start.
fn handshake_budget(now: Instant) -> TokenBucket {
    TokenBucket::new(MEMBER_HANDSHAKE_BURST, MEMBER_HANDSHAKE_REFILL, now)
}

impl Acceptor {
    fn new(shared: Arc<Shared>, network: tokio::runtime::Handle) -> Self {
        let now = Instant::now();
        let members = shared.members.len();
        Self {
            pending: (0..members).map(|_| AtomicUsize::new(0)).collect(),
            budgets: (0..members).map(|_| handshake_budget(now)).collect(),
            running: JoinSet::new(),
            shared,
            network,
        }
    }

    /// The next finished handshake from an admitted member, or one member's failed attempt.
    /// Incoming from a member `claimed` rejects is refused before any TLS runs, and so is every
    /// attempt past the member's concurrency and rate budget. Handshakes run concurrently and
    /// outlive a dropped call for the next one to return. An error means the endpoint is revoked.
    pub(crate) async fn accept_any(
        &mut self,
        claimed: &impl Fn(CertificateFingerprint) -> bool,
    ) -> io::Result<Accepted> {
        self.shared.check_active()?;
        let Some(endpoint) = self.shared.endpoint() else {
            return Err(socket::revoked_error());
        };
        loop {
            tokio::select! {
                biased;
                Some(finished) = self.running.join_next(), if !self.running.is_empty() => {
                    match finished {
                        Ok(handshake) => {
                            if let Some(accepted) = self.finish_handshake(handshake)? {
                                return Ok(accepted);
                            }
                        }
                        Err(error) => {
                            log::warn!("guarded endpoint: an incoming handshake stopped: {error}");
                        }
                    }
                }
                incoming = endpoint.accept() => {
                    let Some(incoming) = incoming else {
                        return Err(self.shared.stopped("accept"));
                    };
                    self.shared.check_active()?;
                    self.start_handshake(&endpoint, incoming, claimed)?;
                }
            }
        }
    }

    fn start_handshake(
        &mut self,
        endpoint: &quinn::Endpoint,
        incoming: quinn::Incoming,
        claimed: &impl Fn(CertificateFingerprint) -> bool,
    ) -> io::Result<()> {
        let Some(index) = self.shared.admitted_index(incoming.remote_address()) else {
            incoming.ignore();
            return Ok(());
        };
        if !claimed(self.shared.members[index].fingerprint) {
            incoming.refuse();
            return Ok(());
        }
        // A stateless retry proves the sender receives at the member's address before any TLS
        // runs or budget is spent, so a blind spoofer can neither start a handshake nor close one.
        if !incoming.remote_address_validated() {
            if let Err(error) = incoming.retry() {
                error.into_incoming().ignore();
            }
            return Ok(());
        }
        if self.pending[index].load(Ordering::Relaxed) >= MAX_MEMBER_HANDSHAKES
            || !self.budgets[index].take(Instant::now())
        {
            incoming.refuse();
            return Ok(());
        }
        let server = self.shared.members[index].server.clone();
        let connecting = match register_incoming(endpoint, &self.shared.signal, incoming, server) {
            Ok(connecting) => connecting,
            Err(error) => {
                self.shared.check_active()?;
                log::debug!("guarded endpoint: an incoming handshake could not start: {error}");
                return Ok(());
            }
        };
        self.pending[index].fetch_add(1, Ordering::Relaxed);
        let slot = PendingSlot {
            pending: self.pending.clone(),
            member: index,
        };
        // Dropping an unfinished handshake at its deadline closes it.
        self.running.spawn_on(
            async move {
                let result = tokio::time::timeout(INCOMING_HANDSHAKE_DEADLINE, connecting)
                    .await
                    .unwrap_or(Err(quinn::ConnectionError::TimedOut));
                Handshake {
                    member: index,
                    result,
                    _slot: slot,
                }
            },
            &self.network,
        );
        Ok(())
    }

    /// None for a member forgotten while its handshake ran: its connection is closed unreported.
    fn finish_handshake(&self, handshake: Handshake) -> io::Result<Option<Accepted>> {
        let member = &self.shared.members[handshake.member];
        let connection = match handshake.result {
            Ok(connection) => connection,
            Err(_) if !member.admitted() => return Ok(None),
            Err(cause) => {
                let refused = if refused_by_this_computer(&cause) {
                    member.refused.take()
                } else {
                    None
                };
                return Ok(Some(Accepted::Failed(MemberHandshakeFailure {
                    member: member.fingerprint,
                    cause: Some(cause),
                    refused,
                })));
            }
        };
        if let Err(error) = self.shared.check_active() {
            connection.close(0_u32.into(), b"network revoked");
            return Err(error);
        }
        if !member.admitted() {
            connection.close(0_u32.into(), b"not a paired computer");
            return Ok(None);
        }
        Ok(Some(if bind_to_member(member, &connection) {
            Accepted::Member(member.fingerprint, connection)
        } else {
            Accepted::Failed(unbound(member))
        }))
    }
}

/// Cross-thread cancellation only. It cannot connect, accept, send, or rebind.
#[derive(Clone)]
pub struct EndpointRevoker {
    shared: Arc<Shared>,
}

impl EndpointRevoker {
    #[track_caller]
    pub fn revoke(&self) {
        self.shared.revoke();
    }

    pub fn is_revoked(&self) -> bool {
        self.shared.signal.is_revoked()
    }
}

/// Dials members and revokes from any thread; accepting stays with the owning thread. Connection
/// drivers run on the runtime that bound the endpoint, whichever thread dials.
#[derive(Clone)]
pub struct EndpointHandle {
    shared: Arc<Shared>,
}

impl EndpointHandle {
    /// Dials `member`'s recorded address with only its pinned configuration. Once the handshake
    /// finishes, `confirm_member` must bind the connection before anything else uses it.
    pub fn connect_member(&self, member: CertificateFingerprint) -> io::Result<quinn::Connecting> {
        self.shared.connect_member(member)
    }

    /// Checks that `connection` came from `member`'s recorded address with exactly its pinned
    /// certificate on an endpoint still active. Anything else closes the connection.
    pub fn confirm_member(
        &self,
        member: CertificateFingerprint,
        connection: &quinn::Connection,
    ) -> io::Result<()> {
        self.shared.confirm_member(member, connection)
    }

    /// Whether `member` is paired on this endpoint and not forgotten.
    pub fn admits(&self, member: CertificateFingerprint) -> bool {
        self.shared.admits(member)
    }

    /// The certificate `member`'s pins refused since the last call, for one diagnostic line.
    pub(crate) fn take_refused_certificate(
        &self,
        member: CertificateFingerprint,
    ) -> Option<CertificateFingerprint> {
        self.shared.take_refused(member)
    }

    pub fn is_revoked(&self) -> bool {
        self.shared.signal.is_revoked()
    }

    pub fn revoker(&self) -> EndpointRevoker {
        EndpointRevoker {
            shared: self.shared.clone(),
        }
    }
}

/// What carries QUIC for one endpoint: the socket, its revocation, its lifetime and its members'
/// reachability.
struct Carrier {
    socket: Arc<dyn AsyncUdpSocket>,
    signal: RevocationSignal,
    lifetime: watch::Receiver<()>,
    reachability: Arc<Reachability>,
}

impl Carrier {
    fn guarded<I: DatagramIo>(socket: Arc<GuardedSocket<I>>) -> Self {
        Self {
            signal: socket.revocation(),
            lifetime: socket.lifetime(),
            reachability: socket.reachability(),
            socket,
        }
    }
}

/// What the readmission timer runs besides the endpoint's own state: the network watch's check,
/// its cadence, and the socket's resolution probe.
struct Readmitting {
    check: native::SharedCheck,
    cadence: native::Cadence,
    prober: socket::Prober,
}

/// Probes a member slot at its recorded address while that computer is still paired here, never
/// once it is forgotten.
fn member_probe(shared: &Arc<Shared>, prober: socket::Prober) -> native::Probe {
    let shared = Arc::downgrade(shared);
    Box::new(move |slot| {
        let address = shared.upgrade().and_then(|shared| {
            shared
                .members
                .get(slot)
                .filter(|member| member.admitted())
                .map(|member| member.address)
        });
        if let Some(address) = address {
            prober(address);
        }
    })
}

impl GuardedEndpoint {
    /// Attempts the system local-network request after an explicit local action without sending data.
    #[cfg(target_os = "macos")]
    pub fn request_local_network_access_after_local_action(
        selection: NetworkSelection,
        cancel: &RevocationSignal,
    ) -> io::Result<()> {
        selection.validate()?;
        native::request_local_network_access_after_local_action(&selection, cancel)
    }

    /// Opens a listener only after an explicit local action and fresh native network checks.
    /// The caller must separately establish human confirmation of `peer_identity` on both machines.
    pub fn bind_after_local_enable(
        selection: NetworkSelection,
        identity: &DeviceIdentity,
        peer_identity: &VerifiedPeer,
    ) -> io::Result<Self> {
        selection.validate()?;
        Self::bind_group_after_local_enable(
            GroupSelection {
                stable_id: selection.stable_id,
                interface_index: selection.interface_index,
                local: selection.local,
                members: vec![GroupMember {
                    address: selection.peer,
                    pin: peer_identity.clone(),
                }],
            },
            identity,
        )
    }

    /// Opens one listener for a fixed set of paired computers only after an explicit local action
    /// and fresh native checks of every member's route. Must run on the runtime that will drive
    /// the endpoint. The caller must separately establish human confirmation of every pin.
    pub fn bind_group_after_local_enable(
        selection: GroupSelection,
        identity: &DeviceIdentity,
    ) -> io::Result<Self> {
        let pinned = selection.validate(identity)?;
        let runtime = HandleRuntime::current()?;
        let members = member_configs(identity, &selection.members)?;
        let refusing = SecureQuicConfig::server_refusing_all().map_err(io::Error::other)?;
        let prepared = native::prepare(&pinned)?;
        let socket = Arc::new(GuardedSocket::new(
            prepared.socket,
            &prepared.lock,
            pinned.local,
            &pinned.peers,
            prepared.reachability,
            prepared.signal,
        )?);
        let readmitting = Readmitting {
            check: prepared.check,
            cadence: native::CADENCE,
            prober: socket.prober(),
        };
        Self::assemble(
            Carrier::guarded(socket),
            members,
            refusing,
            runtime,
            Some(prepared.watch),
            Some(readmitting),
        )
    }

    /// No default client config: every dial names one member's pinned configuration. The
    /// readmission timer runs on `runtime`.
    fn assemble(
        carrier: Carrier,
        members: Box<[Member]>,
        refusing: quinn::ServerConfig,
        runtime: HandleRuntime,
        watch: Option<native::Watch>,
        readmitting: Option<Readmitting>,
    ) -> io::Result<Self> {
        let Carrier {
            socket,
            signal,
            lifetime,
            reachability,
        } = carrier;
        if reachability.len() != members.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the socket's members differ from the endpoint's",
            ));
        }
        let network = runtime.handle().clone();
        let endpoint = quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            Some(refusing),
            socket,
            Arc::new(runtime),
        )?;
        if signal.is_revoked() {
            endpoint.close(0_u32.into(), b"network revoked");
            return Err(socket::revoked_error());
        }
        let shared = Arc::new(Shared {
            members,
            reachability,
            signal,
            endpoint: Mutex::new(Some(endpoint.clone())),
        });
        let readmission = readmitting.and_then(|readmitting| {
            native::Readmission::for_group(
                &network,
                readmitting.check,
                member_probe(&shared, readmitting.prober),
                &shared.signal,
                &shared.reachability,
                readmitting.cadence,
            )
        });
        Ok(Self {
            acceptor: tokio::sync::Mutex::new(Some(Acceptor::new(shared.clone(), network))),
            shared,
            endpoint,
            socket_lifetime: lifetime,
            _readmission: readmission,
            _watch: watch,
            _owner_thread: PhantomData,
        })
    }

    /// Connects only to the selected numeric peer using the fixed mutual TLS configuration.
    pub fn connect(&self) -> io::Result<quinn::Connecting> {
        self.shared.connect_member(self.sole_member()?.fingerprint)
    }

    /// Accepts only the configured peer identity. No alternate TLS configuration is exposed.
    pub async fn accept(&self) -> io::Result<quinn::Connection> {
        self.sole_member()?;
        self.accept_any().await.map(|(_, connection)| connection)
    }

    /// Dials `member`'s recorded address with only its pinned configuration. Once the handshake
    /// finishes, `confirm_member` must bind the connection before anything else uses it.
    pub fn connect_member(&self, member: CertificateFingerprint) -> io::Result<quinn::Connecting> {
        self.shared.connect_member(member)
    }

    /// Checks that `connection` came from `member`'s recorded address with exactly its pinned
    /// certificate on an endpoint still active. Anything else closes the connection.
    pub fn confirm_member(
        &self,
        member: CertificateFingerprint,
        connection: &quinn::Connection,
    ) -> io::Result<()> {
        self.shared.confirm_member(member, connection)
    }

    /// The next finished handshake from any member, already bound to that member. Each attempt
    /// uses only the pinned configuration of the member at its exact address; any other address is
    /// ignored without a reply, and an unvalidated address is answered only with a retry.
    /// Handshakes run concurrently, so a stalled one never holds up another member, and they
    /// outlive a dropped call for the next one to return. A `MemberHandshakeFailure` ends one
    /// attempt only; any other error means the endpoint is revoked or its incoming connections
    /// belong to another accept loop.
    pub async fn accept_any(&self) -> io::Result<(CertificateFingerprint, quinn::Connection)> {
        let mut acceptor = self.acceptor.lock().await;
        let acceptor = acceptor.as_mut().ok_or_else(accepted_elsewhere)?;
        match acceptor.accept_any(&|_| true).await? {
            Accepted::Member(member, connection) => Ok((member, connection)),
            Accepted::Failed(failure) => Err(failure.into_io_error()),
        }
    }

    /// Hands every incoming connection to one accept loop elsewhere on the network runtime; this
    /// endpoint's own accepts fail from then on.
    pub(crate) fn take_acceptor(&self) -> io::Result<Acceptor> {
        self.acceptor
            .try_lock()
            .ok()
            .and_then(|mut slot| slot.take())
            .ok_or_else(accepted_elsewhere)
    }

    /// Stops admitting `member` without a rebind: its incoming is ignored, and dialing or
    /// confirming it fails. The socket still passes its packets until the next bind.
    pub(crate) fn forget_member(&self, member: CertificateFingerprint) {
        self.shared.forget(member);
    }

    /// The certificate `member`'s pins refused since the last call, for one diagnostic line.
    pub(crate) fn take_refused_certificate(
        &self,
        member: CertificateFingerprint,
    ) -> Option<CertificateFingerprint> {
        self.shared.take_refused(member)
    }

    /// A handle other threads can hold to dial members or revoke.
    pub fn handle(&self) -> EndpointHandle {
        EndpointHandle {
            shared: self.shared.clone(),
        }
    }

    pub fn is_revoked(&self) -> bool {
        self.shared.signal.is_revoked()
    }

    pub(crate) fn revocation_signal(&self) -> RevocationSignal {
        self.shared.signal.clone()
    }

    pub fn revoker(&self) -> EndpointRevoker {
        EndpointRevoker {
            shared: self.shared.clone(),
        }
    }

    /// Lets Quinn deliver a closed connection's final packets while the endpoint stays open for
    /// the next connection. Only `close_and_wait_idle` retires it.
    pub async fn wait_idle(&self) -> io::Result<()> {
        wait_idle_or_revoked(self.endpoint.wait_idle(), &self.shared.signal).await
    }

    /// Let Quinn deliver final acknowledgements before the socket's hard revocation.
    pub async fn close_and_wait_idle(&self) -> io::Result<()> {
        self.endpoint.close(0_u32.into(), b"exchange complete");
        wait_idle_or_revoked(self.endpoint.wait_idle(), &self.shared.signal).await
    }

    /// Resolves once the OS socket is closed. Quinn holds it until its endpoint and connection
    /// tasks have run to completion, which is after the last handle drops, not at that drop.
    pub fn socket_closed(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut lifetime = self.socket_lifetime.clone();
        async move { while lifetime.changed().await.is_ok() {} }
    }

    /// Stops locally without waiting for a peer. This instance can never reconnect.
    #[track_caller]
    pub fn revoke(&self) {
        self.shared.signal.revoke();
        self.endpoint.close(0_u32.into(), b"network revoked");
    }

    /// The single-peer calls name no member, so they refuse an endpoint admitting several.
    fn sole_member(&self) -> io::Result<&Member> {
        match &*self.shared.members {
            [member] => Ok(member),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "this endpoint admits several paired computers; name one",
            )),
        }
    }
}

fn accepted_elsewhere() -> io::Error {
    io::Error::other("another accept loop owns this endpoint's incoming connections")
}

#[cfg(test)]
impl GuardedEndpoint {
    /// A group endpoint over a test socket that is already pinned, checked as a bind checks it.
    fn over_socket<I: DatagramIo>(
        socket: GuardedSocket<I>,
        selection: &GroupSelection,
        identity: &DeviceIdentity,
    ) -> io::Result<Self> {
        Self::over_socket_rechecking(socket, selection, identity, None)
    }

    /// As `over_socket`, and while a member is set aside the timer runs `recheck`'s check and
    /// probes on its cadence, as a bind runs its network watch's check.
    fn over_socket_rechecking<I: DatagramIo>(
        socket: GuardedSocket<I>,
        selection: &GroupSelection,
        identity: &DeviceIdentity,
        recheck: Option<(native::SharedCheck, native::Cadence)>,
    ) -> io::Result<Self> {
        selection.validate(identity)?;
        let socket = Arc::new(socket);
        let readmitting = recheck.map(|(check, cadence)| Readmitting {
            check,
            cadence,
            prober: socket.prober(),
        });
        Self::assemble(
            Carrier::guarded(socket),
            member_configs(identity, &selection.members)?,
            SecureQuicConfig::server_refusing_all().map_err(io::Error::other)?,
            HandleRuntime::current()?,
            None,
            readmitting,
        )
    }

    /// Sets each member's reachability, in member order, as a route check would.
    pub(crate) fn set_reachable(&self, reachable: &[bool]) {
        self.shared.reachability.set(reachable);
    }
}

async fn wait_idle_or_revoked(
    idle: impl std::future::Future<Output = ()>,
    signal: &RevocationSignal,
) -> io::Result<()> {
    tokio::pin!(idle);
    let mut check = tokio::time::interval(std::time::Duration::from_millis(20));
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        // Quinn's socket-error shutdown does not wake wait_idle. Do not steal the socket's waker.
        tokio::select! {
            biased;
            _ = check.tick() => {
                if signal.is_revoked() { return Err(socket::revoked_error()); }
            }
            _ = &mut idle => {
                return if signal.is_revoked() { Err(socket::revoked_error()) } else { Ok(()) };
            }
        }
    }
}

fn register_incoming(
    endpoint: &quinn::Endpoint,
    signal: &RevocationSignal,
    incoming: quinn::Incoming,
    server: Arc<quinn::ServerConfig>,
) -> io::Result<quinn::Connecting> {
    let connecting = incoming.accept_with(server).map_err(io::Error::other)?;
    // Quinn can register an Incoming after its endpoint driver has already dropped.
    if signal.is_revoked() {
        endpoint.close(0_u32.into(), b"network revoked");
        return Err(socket::revoked_error());
    }
    Ok(connecting)
}

impl Drop for GuardedEndpoint {
    fn drop(&mut self) {
        self.revoke();
        self.shared
            .endpoint
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}

/// Plain loopback UDP for endpoint tests. Admission, pins, retry, budgets and binding run as on
/// the pinned socket; that socket's interface and address filters do not.
#[cfg(test)]
pub(crate) mod loopback {
    use std::{
        io::IoSliceMut,
        net::{Ipv4Addr, UdpSocket},
        pin::Pin,
        task::{Context, Poll},
    };

    use quinn::{
        Runtime, UdpPoller,
        udp::{RecvMeta, Transmit},
    };

    use super::*;

    pub(crate) fn bind() -> UdpSocket {
        UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap()
    }

    pub(crate) fn address(socket: &UdpSocket) -> SocketAddrV4 {
        match socket.local_addr().unwrap() {
            SocketAddr::V4(address) => address,
            SocketAddr::V6(_) => unreachable!("bound to IPv4 loopback"),
        }
    }

    pub(crate) fn pin(identity: &DeviceIdentity) -> VerifiedPeer {
        VerifiedPeer::from_certificate_der(
            identity.certificate_der(),
            &identity.fingerprint().full_hex(),
        )
        .unwrap()
    }

    /// Stops like the pinned socket: nothing leaves once revoked, and the receive driver ends.
    struct Loopback {
        inner: Arc<dyn AsyncUdpSocket>,
        signal: RevocationSignal,
        _lifetime: watch::Sender<()>,
    }

    impl fmt::Debug for Loopback {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("Loopback")
        }
    }

    impl AsyncUdpSocket for Loopback {
        fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
            self.inner.clone().create_io_poller()
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
            if self.signal.poll_revoked(cx).is_ready() {
                return Poll::Ready(Err(socket::revoked_error()));
            }
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

    impl GuardedEndpoint {
        /// A group endpoint on `socket` admitting exactly `members`, each pinned as a bind pins it.
        pub(crate) fn over_loopback(
            socket: UdpSocket,
            identity: &DeviceIdentity,
            members: &[GroupMember],
        ) -> io::Result<Self> {
            validate_pins(identity, members)?;
            let lifetime = watch::Sender::new(());
            let signal = RevocationSignal::default();
            let carrier = Carrier {
                lifetime: lifetime.subscribe(),
                socket: Arc::new(Loopback {
                    inner: quinn::TokioRuntime.wrap_udp_socket(socket)?,
                    signal: signal.clone(),
                    _lifetime: lifetime,
                }),
                signal,
                reachability: Reachability::new(members.len()),
            };
            Self::assemble(
                carrier,
                member_configs(identity, members)?,
                SecureQuicConfig::server_refusing_all().map_err(io::Error::other)?,
                HandleRuntime::current()?,
                None,
                None,
            )
        }
    }

    /// A plain peer's socket that hears only its first `heard` receives and keeps the first byte
    /// of every datagram it drops.
    pub(crate) struct Hearing {
        inner: Arc<dyn AsyncUdpSocket>,
        heard: AtomicUsize,
        dropped: Mutex<Vec<u8>>,
    }

    impl Hearing {
        /// The first byte of every datagram dropped so far.
        pub(crate) fn dropped(&self) -> Vec<u8> {
            self.dropped.lock().unwrap().clone()
        }
    }

    impl fmt::Debug for Hearing {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("Hearing")
        }
    }

    impl AsyncUdpSocket for Hearing {
        fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
            self.inner.clone().create_io_poller()
        }

        fn try_send(&self, transmit: &Transmit<'_>) -> io::Result<()> {
            self.inner.try_send(transmit)
        }

        fn poll_recv(
            &self,
            cx: &mut Context<'_>,
            bufs: &mut [IoSliceMut<'_>],
            meta: &mut [RecvMeta],
        ) -> Poll<io::Result<usize>> {
            loop {
                let count = match self.inner.poll_recv(cx, bufs, meta) {
                    Poll::Ready(Ok(count)) => count,
                    other => return other,
                };
                let hears = self
                    .heard
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                        left.checked_sub(1)
                    })
                    .is_ok();
                if hears {
                    return Poll::Ready(Ok(count));
                }
                let mut dropped = self.dropped.lock().unwrap();
                for (buffer, meta) in bufs.iter().zip(meta.iter()).take(count) {
                    let stride = meta.stride.max(1);
                    dropped.extend((0..meta.len).step_by(stride).map(|start| buffer[start]));
                }
            }
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

    /// A plain, unguarded peer on `socket` that dials with `identity`, pinning `server`, and hears
    /// only its first `heard` receives.
    pub(crate) fn peer(
        socket: UdpSocket,
        identity: &DeviceIdentity,
        server: &VerifiedPeer,
        heard: usize,
    ) -> (quinn::Endpoint, Arc<Hearing>) {
        let hearing = Arc::new(Hearing {
            inner: quinn::TokioRuntime.wrap_udp_socket(socket).unwrap(),
            heard: AtomicUsize::new(heard),
            dropped: Mutex::default(),
        });
        let mut endpoint = quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            None,
            hearing.clone(),
            Arc::new(quinn::TokioRuntime),
        )
        .unwrap();
        endpoint.set_default_client_config(SecureQuicConfig::client(identity, server).unwrap());
        (endpoint, hearing)
    }

    /// A plain peer that hears everything.
    pub(crate) fn dialer(
        socket: UdpSocket,
        identity: &DeviceIdentity,
        server: &VerifiedPeer,
    ) -> quinn::Endpoint {
        peer(socket, identity, server, usize::MAX).0
    }

    /// A plain peer that accepts only `client`, answering without address validation.
    pub(crate) fn listener(
        socket: UdpSocket,
        identity: &DeviceIdentity,
        client: &VerifiedPeer,
    ) -> quinn::Endpoint {
        quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            Some(SecureQuicConfig::server(identity, client).unwrap()),
            socket,
            Arc::new(quinn::TokioRuntime),
        )
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn revocation_releases_drain_without_a_quinn_idle_notification() {
        let signal = RevocationSignal::default();
        let observer = signal.clone();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let waiting = tokio::spawn(async move {
            let idle = async {
                let _ = entered.send(());
                std::future::pending::<()>().await
            };
            wait_idle_or_revoked(idle, &observer).await
        });
        ready.await.unwrap();
        signal.revoke();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn selection_rejects_wildcards_ports_and_nonlocal_addresses_without_native_work() {
        let valid = NetworkSelection {
            stable_id: "physical-adapter".into(),
            interface_index: 7,
            local: "192.168.50.10:24800".parse().unwrap(),
            peer: "192.168.50.12:24800".parse().unwrap(),
        };
        assert!(valid.validate().is_ok());
        for address in [
            "0.0.0.0:24800",
            "127.0.0.1:24800",
            "8.8.8.8:24800",
            "192.168.50.10:0",
        ] {
            let selection = NetworkSelection {
                local: address.parse().unwrap(),
                ..valid.clone()
            };
            assert!(selection.validate().is_err());
        }
        for address in [
            "0.0.0.0:24800",
            "127.0.0.1:24800",
            "8.8.8.8:24800",
            "192.168.50.12:0",
            "192.168.50.10:24801",
        ] {
            let selection = NetworkSelection {
                peer: address.parse().unwrap(),
                ..valid.clone()
            };
            assert!(selection.validate().is_err());
        }
        assert!(
            NetworkSelection {
                interface_index: 0,
                ..valid.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            NetworkSelection {
                stable_id: String::new(),
                ..valid.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            NetworkSelection {
                stable_id: "x".repeat(257),
                ..valid
            }
            .validate()
            .is_err()
        );
    }

    fn pin(identity: &DeviceIdentity) -> VerifiedPeer {
        VerifiedPeer::from_certificate_der(
            identity.certificate_der(),
            &identity.fingerprint().full_hex(),
        )
        .unwrap()
    }

    #[test]
    fn a_group_needs_one_to_seven_members_with_their_own_addresses_and_certificates() {
        let local = DeviceIdentity::generate().unwrap();
        let identities: Vec<_> = (0..8)
            .map(|_| DeviceIdentity::generate().unwrap())
            .collect();
        let member = |host: u8, identity: &DeviceIdentity| GroupMember {
            address: SocketAddrV4::new(std::net::Ipv4Addr::new(192, 168, 50, host), 24800),
            pin: pin(identity),
        };
        let group = |members: Vec<GroupMember>| GroupSelection {
            stable_id: "physical-adapter".into(),
            interface_index: 7,
            local: "192.168.50.10:24800".parse().unwrap(),
            members,
        };
        let three: Vec<_> = (0..3)
            .map(|index| member(11 + index as u8, &identities[index]))
            .collect();
        let pinned = group(three.clone()).validate(&local).unwrap();
        assert_eq!(pinned.peers.len(), 3);
        assert!(
            group(
                (0..7)
                    .map(|index| member(11 + index as u8, &identities[index]))
                    .collect()
            )
            .validate(&local)
            .is_ok()
        );

        let mut same_certificate = three.clone();
        same_certificate[2].pin = pin(&identities[0]);
        let mut same_address = three.clone();
        same_address[1].address = three[0].address;
        let mut same_host = three.clone();
        same_host[1].address = SocketAddrV4::new(*three[0].address.ip(), 24801);
        let mut this_computer = three.clone();
        this_computer[0].pin = pin(&local);
        let mut local_address = three.clone();
        local_address[2].address = "192.168.50.10:24801".parse().unwrap();
        let mut public = three.clone();
        public[1].address = "8.8.8.8:24800".parse().unwrap();
        let mut no_port = three;
        no_port[0].address = "192.168.50.11:0".parse().unwrap();
        for members in [
            Vec::new(),
            (0..8)
                .map(|index| member(11 + index as u8, &identities[index]))
                .collect(),
            same_certificate,
            same_address,
            same_host,
            this_computer,
            local_address,
            public,
            no_port,
        ] {
            assert_eq!(
                group(members).validate(&local).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn a_member_handshake_budget_refills_one_per_interval_up_to_its_burst() {
        let start = Instant::now();
        let mut budget = handshake_budget(start);
        for _ in 0..MEMBER_HANDSHAKE_BURST {
            assert!(budget.take(start));
        }
        assert!(!budget.take(start));
        assert!(!budget.take(start + MEMBER_HANDSHAKE_REFILL / 2));
        assert!(budget.take(start + MEMBER_HANDSHAKE_REFILL));
        assert!(!budget.take(start + MEMBER_HANDSHAKE_REFILL));
        let idle = start + MEMBER_HANDSHAKE_REFILL * 100;
        for _ in 0..MEMBER_HANDSHAKE_BURST {
            assert!(budget.take(idle));
        }
        assert!(!budget.take(idle));
    }
}

#[cfg(test)]
mod accept_tests {
    use std::net::UdpSocket;

    use super::{
        loopback::{self, pin},
        *,
    };

    const DEADLINE: Duration = Duration::from_secs(2);

    /// A hub endpoint on loopback admitting one member per identity, and each member's socket.
    fn hub(
        hub_id: &DeviceIdentity,
        members: &[&DeviceIdentity],
    ) -> (GuardedEndpoint, Vec<(UdpSocket, SocketAddrV4)>) {
        let sockets: Vec<_> = members
            .iter()
            .map(|_| {
                let socket = loopback::bind();
                let address = loopback::address(&socket);
                (socket, address)
            })
            .collect();
        let group: Vec<_> = members
            .iter()
            .zip(&sockets)
            .map(|(identity, (_, address))| GroupMember {
                address: *address,
                pin: pin(identity),
            })
            .collect();
        let hub = GuardedEndpoint::over_loopback(loopback::bind(), hub_id, &group).unwrap();
        (hub, sockets)
    }

    fn local(hub: &GuardedEndpoint) -> SocketAddr {
        hub.endpoint.local_addr().unwrap()
    }

    /// A spoofer that can send from a member's recorded address but never receive there gets
    /// only retries: no TLS flight is sent and no connection state is created for it.
    #[tokio::test]
    async fn unvalidated_incoming_is_retried_before_tls() {
        let hub_id = DeviceIdentity::generate().unwrap();
        let spoofed_id = DeviceIdentity::generate().unwrap();
        let member_id = DeviceIdentity::generate().unwrap();
        let (hub, mut sockets) = hub(&hub_id, &[&spoofed_id, &member_id]);
        let (member_socket, _) = sockets.pop().unwrap();
        let (spoofed_socket, _) = sockets.pop().unwrap();
        let (spoofer, deaf) = loopback::peer(spoofed_socket, &spoofed_id, &pin(&hub_id), 0);
        let blind = spoofer.connect(local(&hub), LOCAL_TLS_SERVER_NAME).unwrap();

        tokio::select! {
            _ = hub.accept_any() => panic!("a spoofed attempt finished a handshake"),
            () = async {
                while deaf.dropped().len() < 2 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            } => {}
            () = tokio::time::sleep(DEADLINE) => panic!("the spoofed address got no answer"),
        }
        let answers = deaf.dropped();
        assert!(
            answers.iter().all(|first| first & 0xF0 == 0xF0),
            "only retry packets answer an unvalidated address: {answers:02X?}"
        );
        assert_eq!(hub.endpoint.stats().accepted_handshakes, 0);
        assert_eq!(hub.endpoint.open_connections(), 0);
        drop(blind);

        // A member that receives at its address passes the retry and connects.
        let member = loopback::dialer(member_socket, &member_id, &pin(&hub_id));
        let (dialed, accepted) = tokio::time::timeout(DEADLINE, async {
            tokio::join!(
                async {
                    member
                        .connect(local(&hub), LOCAL_TLS_SERVER_NAME)
                        .unwrap()
                        .await
                },
                hub.accept_any()
            )
        })
        .await
        .expect("a validated member connects");
        dialed.unwrap();
        let (accepted, _connection) = accepted.unwrap();
        assert!(accepted == member_id.fingerprint());
        assert_eq!(hub.endpoint.stats().accepted_handshakes, 1);
        hub.revoke();
    }

    /// Once a member passes the retry and then stops answering, its handshake is closed at the
    /// deadline instead of holding a slot until the idle timeout.
    #[tokio::test]
    async fn a_stalled_incoming_handshake_ends_at_its_deadline() {
        let hub_id = DeviceIdentity::generate().unwrap();
        let member_id = DeviceIdentity::generate().unwrap();
        let (hub, mut sockets) = hub(&hub_id, &[&member_id]);
        let (socket, _) = sockets.pop().unwrap();
        // Hears the retry, then nothing: the server's TLS flight never arrives.
        let (stalled, _) = loopback::peer(socket, &member_id, &pin(&hub_id), 1);
        let _attempt = stalled.connect(local(&hub), LOCAL_TLS_SERVER_NAME).unwrap();
        let started = std::time::Instant::now();
        let failed = tokio::time::timeout(INCOMING_HANDSHAKE_DEADLINE + DEADLINE, hub.accept_any())
            .await
            .expect("the stalled handshake ended at its deadline")
            .err()
            .expect("the stalled handshake failed");
        assert!(started.elapsed() >= INCOMING_HANDSHAKE_DEADLINE - Duration::from_millis(100));
        let failure = failed
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<MemberHandshakeFailure>())
            .expect("one member's failure");
        assert!(failure.member == member_id.fingerprint());
        assert!(matches!(
            failure.cause,
            Some(quinn::ConnectionError::TimedOut)
        ));
        assert!(!failure.certificate_refused());
        hub.revoke();
    }

    #[tokio::test]
    async fn handshake_rate_limit_is_per_member() {
        let hub_id = DeviceIdentity::generate().unwrap();
        let flooding_id = DeviceIdentity::generate().unwrap();
        let other_id = DeviceIdentity::generate().unwrap();
        let (hub, mut sockets) = hub(&hub_id, &[&flooding_id, &other_id]);
        let (other_socket, _) = sockets.pop().unwrap();
        let (flooding_socket, _) = sockets.pop().unwrap();
        let flooding = loopback::dialer(flooding_socket, &flooding_id, &pin(&hub_id));
        let other = loopback::dialer(other_socket, &other_id, &pin(&hub_id));

        // One at a time, so only the rate budget, never the concurrency limit, can refuse.
        let mut refused = None;
        for attempt in 1..=MEMBER_HANDSHAKE_BURST * 5 {
            let connecting = flooding
                .connect(local(&hub), LOCAL_TLS_SERVER_NAME)
                .unwrap();
            let (dialed, accepted) = tokio::join!(
                tokio::time::timeout(DEADLINE, connecting),
                tokio::time::timeout(DEADLINE, hub.accept_any())
            );
            match dialed.expect("each attempt is answered") {
                Ok(connection) => {
                    let (member, _accepted) = accepted.unwrap().unwrap();
                    assert!(member == flooding_id.fingerprint());
                    connection.close(0_u32.into(), b"next attempt");
                }
                Err(error) => {
                    assert!(accepted.is_err(), "a refused attempt reaches no accept");
                    refused = Some((attempt, error));
                    break;
                }
            }
        }
        let (attempt, error) = refused.expect("the flooding member ran out of budget");
        assert!(attempt > MEMBER_HANDSHAKE_BURST, "refused after {attempt}");
        assert!(
            matches!(
                &error,
                quinn::ConnectionError::ConnectionClosed(close)
                    if close.error_code == quinn::TransportErrorCode::CONNECTION_REFUSED
            ),
            "{error}"
        );

        // The other member's budget is untouched.
        let (dialed, accepted) = tokio::time::timeout(DEADLINE, async {
            tokio::join!(
                async {
                    other
                        .connect(local(&hub), LOCAL_TLS_SERVER_NAME)
                        .unwrap()
                        .await
                },
                hub.accept_any()
            )
        })
        .await
        .expect("the other member connects at once");
        dialed.unwrap();
        assert!(accepted.unwrap().0 == other_id.fingerprint());
        hub.revoke();
    }

    #[tokio::test]
    async fn confirming_on_a_revoked_endpoint_closes_the_connection() {
        let hub_id = DeviceIdentity::generate().unwrap();
        let member_id = DeviceIdentity::generate().unwrap();
        let (hub, mut sockets) = hub(&hub_id, &[&member_id]);
        let (socket, member) = sockets.pop().unwrap();
        let listener = loopback::listener(socket, &member_id, &pin(&hub_id));
        let (dialed, _answered) = tokio::time::timeout(DEADLINE, async {
            tokio::join!(
                async {
                    hub.connect_member(member_id.fingerprint())
                        .unwrap()
                        .await
                        .unwrap()
                },
                async { listener.accept().await.unwrap().await.unwrap() }
            )
        })
        .await
        .expect("the member answers");
        assert_eq!(dialed.remote_address(), SocketAddr::V4(member));
        // Only the signal: the connection itself still binds to the member's address and pin.
        hub.shared.signal.revoke();
        assert_eq!(
            hub.confirm_member(member_id.fingerprint(), &dialed)
                .unwrap_err()
                .kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert!(dialed.close_reason().is_some());
        hub.revoke();
    }

    #[tokio::test]
    async fn a_forgotten_member_is_ignored_and_can_no_longer_be_dialed_or_confirmed() {
        let hub_id = DeviceIdentity::generate().unwrap();
        let forgotten_id = DeviceIdentity::generate().unwrap();
        let kept_id = DeviceIdentity::generate().unwrap();
        let (hub, mut sockets) = hub(&hub_id, &[&forgotten_id, &kept_id]);
        let (kept_socket, _) = sockets.pop().unwrap();
        let (forgotten_socket, _) = sockets.pop().unwrap();
        let forgotten = loopback::dialer(forgotten_socket, &forgotten_id, &pin(&hub_id));
        let kept = loopback::dialer(kept_socket, &kept_id, &pin(&hub_id));

        let (dialed, accepted) = tokio::time::timeout(DEADLINE, async {
            tokio::join!(
                async {
                    forgotten
                        .connect(local(&hub), LOCAL_TLS_SERVER_NAME)
                        .unwrap()
                        .await
                        .unwrap()
                },
                hub.accept_any()
            )
        })
        .await
        .expect("the member connects before it is forgotten");
        let (_, connection) = accepted.unwrap();
        hub.forget_member(forgotten_id.fingerprint());
        assert!(!hub.handle().admits(forgotten_id.fingerprint()));
        assert!(hub.handle().admits(kept_id.fingerprint()));
        assert_eq!(
            hub.connect_member(forgotten_id.fingerprint())
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(
            hub.confirm_member(forgotten_id.fingerprint(), &connection)
                .is_err()
        );
        tokio::time::timeout(DEADLINE, dialed.closed())
            .await
            .expect("confirming a forgotten member closes its connection");

        let ignored = forgotten
            .connect(local(&hub), LOCAL_TLS_SERVER_NAME)
            .unwrap();
        let kept_dial = kept.connect(local(&hub), LOCAL_TLS_SERVER_NAME).unwrap();
        let (ignored, kept_dialed, (first, second)) = tokio::join!(
            tokio::time::timeout(Duration::from_millis(500), ignored),
            kept_dial,
            async {
                let first = hub.accept_any().await;
                let second = tokio::time::timeout(Duration::from_millis(600), hub.accept_any());
                (first, second.await)
            }
        );
        assert!(ignored.is_err(), "a forgotten member gets no answer");
        kept_dialed.unwrap();
        assert!(first.unwrap().0 == kept_id.fingerprint());
        assert!(second.is_err(), "nothing but the kept member was accepted");
        let stats = hub.endpoint.stats();
        assert_eq!(stats.accepted_handshakes, 2);
        assert!(stats.ignored_handshakes > 0);
        hub.revoke();
    }
}
