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
        atomic::{AtomicUsize, Ordering},
    },
};

use monhop_core::revocation::RevocationSignal;
use tokio::{sync::watch, task::JoinSet};

use crate::{
    crypto::{
        CertificateFingerprint, DeviceIdentity, LOCAL_TLS_SERVER_NAME, SecureQuicConfig,
        VerifiedPeer,
    },
    policy::{MAX_PINNED_PEERS, is_private_or_link_local},
};
use runtime::HandleRuntime;
use socket::{DatagramIo, GuardedSocket};

/// Incoming handshakes one member may have running at once; more are refused until one ends.
const MAX_MEMBER_HANDSHAKES: usize = 2;

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
pub struct MemberHandshakeFailure {
    pub member: CertificateFingerprint,
    /// None when the handshake finished but did not come from the member's address and pin.
    pub cause: Option<quinn::ConnectionError>,
}

impl fmt::Debug for MemberHandshakeFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemberHandshakeFailure")
            .field("member", &self.member.short_hex())
            .field("cause", &self.cause)
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
/// never changes for the endpoint's life; a different set needs a new bind.
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
        let local = identity.fingerprint();
        for (index, member) in self.members.iter().enumerate() {
            let pin = member.pin.fingerprint();
            if pin == local
                || self.members[..index]
                    .iter()
                    .any(|earlier| earlier.pin.fingerprint() == pin)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "each paired computer needs its own certificate",
                ));
            }
        }
        Ok(pinned)
    }
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
}

fn member_configs(identity: &DeviceIdentity, members: &[GroupMember]) -> io::Result<Box<[Member]>> {
    members
        .iter()
        .map(|member| {
            Ok(Member {
                address: member.address,
                fingerprint: member.pin.fingerprint(),
                client: SecureQuicConfig::client(identity, &member.pin)
                    .map_err(io::Error::other)?,
                server: Arc::new(
                    SecureQuicConfig::server(identity, &member.pin).map_err(io::Error::other)?,
                ),
            })
        })
        .collect()
}

/// What the endpoint's cross-thread handles share. The slot empties when the owner drops, so a
/// stray handle never keeps the pinned socket open.
struct Shared {
    members: Box<[Member]>,
    signal: RevocationSignal,
    endpoint: Mutex<Option<quinn::Endpoint>>,
}

impl Shared {
    fn member(&self, fingerprint: CertificateFingerprint) -> io::Result<&Member> {
        self.members
            .iter()
            .find(|member| member.fingerprint == fingerprint)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "that computer is not paired on this endpoint",
                )
            })
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
        let member = self.member(fingerprint)?;
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
        bind_to_member(member, connection)
    }
}

/// A finished connection must come from the member's recorded address and present exactly its
/// pinned certificate; anything else is closed.
fn bind_to_member(member: &Member, connection: &quinn::Connection) -> io::Result<()> {
    if connection.remote_address() == SocketAddr::V4(member.address)
        && observed_fingerprint(connection) == Some(member.fingerprint)
    {
        return Ok(());
    }
    log::warn!(
        "guarded endpoint: a connection did not bind to paired computer {}; closed",
        member.fingerprint.short_hex()
    );
    connection.close(0_u32.into(), b"not the paired computer");
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        MemberHandshakeFailure {
            member: member.fingerprint,
            cause: None,
        },
    ))
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
    network: tokio::runtime::Handle,
    handshakes: tokio::sync::Mutex<Handshakes>,
    socket_lifetime: watch::Receiver<()>,
    _watch: Option<native::Watch>,
    _owner_thread: PhantomData<Rc<()>>,
}

/// Incoming handshakes still running, so one member's stalled attempt never holds up another's.
struct Handshakes {
    running: JoinSet<Handshake>,
    pending: Arc<[AtomicUsize]>,
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
    /// certificate. Anything else closes the connection.
    pub fn confirm_member(
        &self,
        member: CertificateFingerprint,
        connection: &quinn::Connection,
    ) -> io::Result<()> {
        self.shared.confirm_member(member, connection)
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
        let socket = GuardedSocket::new(
            prepared.socket,
            &prepared.lock,
            pinned.local,
            &pinned.peers,
            prepared.signal,
        )?;
        Self::assemble(socket, members, refusing, runtime, Some(prepared.watch))
    }

    /// No default client config: every dial names one member's pinned configuration.
    fn assemble<I: DatagramIo>(
        socket: GuardedSocket<I>,
        members: Box<[Member]>,
        refusing: quinn::ServerConfig,
        runtime: HandleRuntime,
        watch: Option<native::Watch>,
    ) -> io::Result<Self> {
        let signal = socket.revocation();
        let socket_lifetime = socket.lifetime();
        let network = runtime.handle().clone();
        let endpoint = quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            Some(refusing),
            Arc::new(socket),
            Arc::new(runtime),
        )?;
        if signal.is_revoked() {
            endpoint.close(0_u32.into(), b"network revoked");
            return Err(socket::revoked_error());
        }
        let pending = members.iter().map(|_| AtomicUsize::new(0)).collect();
        Ok(Self {
            shared: Arc::new(Shared {
                members,
                signal,
                endpoint: Mutex::new(Some(endpoint.clone())),
            }),
            endpoint,
            network,
            handshakes: tokio::sync::Mutex::new(Handshakes {
                running: JoinSet::new(),
                pending,
            }),
            socket_lifetime,
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
        let member = self.sole_member()?;
        self.check_active()?;
        loop {
            let Some(incoming) = self.endpoint.accept().await else {
                return Err(self.shared.stopped("accept"));
            };
            self.check_active()?;
            if incoming.remote_address() != SocketAddr::V4(member.address) {
                incoming.ignore();
                continue;
            }
            let connecting = register_incoming(
                &self.endpoint,
                &self.shared.signal,
                incoming,
                member.server.clone(),
            )?;
            let connection = connecting.await.map_err(io::Error::other)?;
            if let Err(error) = self.check_active() {
                connection.close(0_u32.into(), b"network revoked");
                return Err(error);
            }
            bind_to_member(member, &connection)?;
            return Ok(connection);
        }
    }

    /// Dials `member`'s recorded address with only its pinned configuration. Once the handshake
    /// finishes, `confirm_member` must bind the connection before anything else uses it.
    pub fn connect_member(&self, member: CertificateFingerprint) -> io::Result<quinn::Connecting> {
        self.shared.connect_member(member)
    }

    /// Checks that `connection` came from `member`'s recorded address with exactly its pinned
    /// certificate. Anything else closes the connection.
    pub fn confirm_member(
        &self,
        member: CertificateFingerprint,
        connection: &quinn::Connection,
    ) -> io::Result<()> {
        self.shared.confirm_member(member, connection)
    }

    /// The next finished handshake from any member, already bound to that member. Each attempt
    /// uses only the pinned configuration of the member at its exact address; any other address is
    /// ignored without a reply. Handshakes run concurrently, so a stalled one never holds up
    /// another member, and they outlive a dropped call for the next one to return. A
    /// `MemberHandshakeFailure` ends one attempt only; any other error means the endpoint is
    /// revoked.
    pub async fn accept_any(&self) -> io::Result<(CertificateFingerprint, quinn::Connection)> {
        self.check_active()?;
        let mut handshakes = self.handshakes.lock().await;
        loop {
            tokio::select! {
                biased;
                Some(finished) = handshakes.running.join_next(),
                    if !handshakes.running.is_empty() =>
                {
                    match finished {
                        Ok(handshake) => return self.finish_handshake(handshake),
                        Err(error) => {
                            log::warn!("guarded endpoint: an incoming handshake stopped: {error}");
                        }
                    }
                }
                incoming = self.endpoint.accept() => {
                    let Some(incoming) = incoming else {
                        return Err(self.shared.stopped("accept"));
                    };
                    self.check_active()?;
                    self.start_handshake(&mut handshakes, incoming)?;
                }
            }
        }
    }

    fn start_handshake(
        &self,
        handshakes: &mut Handshakes,
        incoming: quinn::Incoming,
    ) -> io::Result<()> {
        let found = match incoming.remote_address() {
            SocketAddr::V4(remote) => self
                .shared
                .members
                .iter()
                .position(|member| member.address == remote),
            SocketAddr::V6(_) => None,
        };
        let Some(index) = found else {
            incoming.ignore();
            return Ok(());
        };
        if handshakes.pending[index].load(Ordering::Relaxed) >= MAX_MEMBER_HANDSHAKES {
            incoming.refuse();
            return Ok(());
        }
        let server = self.shared.members[index].server.clone();
        let connecting =
            match register_incoming(&self.endpoint, &self.shared.signal, incoming, server) {
                Ok(connecting) => connecting,
                Err(error) => {
                    self.check_active()?;
                    log::debug!("guarded endpoint: an incoming handshake could not start: {error}");
                    return Ok(());
                }
            };
        handshakes.pending[index].fetch_add(1, Ordering::Relaxed);
        let slot = PendingSlot {
            pending: handshakes.pending.clone(),
            member: index,
        };
        handshakes.running.spawn_on(
            async move {
                Handshake {
                    member: index,
                    result: connecting.await,
                    _slot: slot,
                }
            },
            &self.network,
        );
        Ok(())
    }

    fn finish_handshake(
        &self,
        handshake: Handshake,
    ) -> io::Result<(CertificateFingerprint, quinn::Connection)> {
        let member = &self.shared.members[handshake.member];
        let connection = handshake.result.map_err(|cause| {
            io::Error::other(MemberHandshakeFailure {
                member: member.fingerprint,
                cause: Some(cause),
            })
        })?;
        if let Err(error) = self.check_active() {
            connection.close(0_u32.into(), b"network revoked");
            return Err(error);
        }
        bind_to_member(member, &connection)?;
        Ok((member.fingerprint, connection))
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

    fn check_active(&self) -> io::Result<()> {
        self.shared.check_active()
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

#[cfg(test)]
impl GuardedEndpoint {
    /// A group endpoint over a test socket that is already pinned, checked as a bind checks it.
    fn over_socket<I: DatagramIo>(
        socket: GuardedSocket<I>,
        selection: &GroupSelection,
        identity: &DeviceIdentity,
    ) -> io::Result<Self> {
        selection.validate(identity)?;
        Self::assemble(
            socket,
            member_configs(identity, &selection.members)?,
            SecureQuicConfig::server_refusing_all().map_err(io::Error::other)?,
            HandleRuntime::current()?,
            None,
        )
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
}
