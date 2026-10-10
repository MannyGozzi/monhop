//! Pairing transport on the selected interface: one listener while a code is shown, or one dial
//! to the computer showing it. Its TLS uses the pairing ALPN and no pin, so a connection proves
//! nothing until the code is confirmed over it; sharing configurations never accept it.

use std::{
    io,
    marker::PhantomData,
    net::{SocketAddr, SocketAddrV4},
    rc::Rc,
    sync::{Arc, Mutex, OnceLock},
};

use tokio::{sync::watch, time::Instant};

use super::{
    Carrier, EndpointRevoker, INCOMING_HANDSHAKE_DEADLINE, ListenSelection, NetworkSelection,
    PinnedNetwork, Shared, TokenBucket, handshake_budget, native, register_incoming,
    runtime::HandleRuntime,
    socket::{self, GuardedSocket},
    wait_idle_or_revoked,
};
use crate::crypto::{
    DeviceIdentity, LOCAL_TLS_SERVER_NAME, SecureQuicConfig, presented_certificate,
};

/// A pairing listener or dialer. Dropping it closes its socket.
pub struct PairingEndpoint {
    endpoint: quinn::Endpoint,
    shared: Arc<Shared>,
    role: Role,
    socket_lifetime: watch::Receiver<()>,
    _watch: Option<native::Watch>,
    _owner_thread: PhantomData<Rc<()>>,
}

enum Role {
    Listening(tokio::sync::Mutex<Listener>),
    Dialing {
        peer: SocketAddrV4,
        client: quinn::ClientConfig,
    },
}

/// One handshake at a time, and nothing at all once a connection is established.
struct Listener {
    server: Arc<quinn::ServerConfig>,
    /// The established peer, shared with the socket, which from then on hears only that host.
    chosen: Arc<OnceLock<SocketAddrV4>>,
    budget: TokenBucket,
    established: bool,
}

impl PairingEndpoint {
    /// Listens for one pairing connection from another private host of the selected subnet,
    /// after an explicit local action and fresh native checks of the adapter. Must run on the
    /// runtime that will drive it.
    pub fn listen_after_local_action(
        selection: ListenSelection,
        identity: &DeviceIdentity,
    ) -> io::Result<Self> {
        selection.validate()?;
        let runtime = HandleRuntime::current()?;
        let server = SecureQuicConfig::pairing_server(identity).map_err(io::Error::other)?;
        let prepared = native::prepare_listener(&selection)?;
        let chosen = Arc::new(OnceLock::new());
        let socket = GuardedSocket::listening(
            prepared.socket,
            &prepared.interface,
            selection.local,
            chosen.clone(),
            prepared.signal,
        )?;
        Self::assemble(
            Carrier::guarded(Arc::new(socket)),
            runtime,
            Some(prepared.watch),
            Role::Listening(tokio::sync::Mutex::new(Listener::new(server, chosen))),
        )
    }

    /// Dials the computer showing a code at `selection.peer`, after an explicit local action and
    /// the native route checks a sharing bind makes for that address.
    pub fn dial_after_local_action(
        selection: NetworkSelection,
        identity: &DeviceIdentity,
    ) -> io::Result<Self> {
        selection.validate()?;
        let runtime = HandleRuntime::current()?;
        let client = SecureQuicConfig::pairing_client(identity).map_err(io::Error::other)?;
        let pinned = PinnedNetwork::new(
            &selection.stable_id,
            selection.interface_index,
            selection.local,
            &[selection.peer],
        )?;
        let prepared = native::prepare(&pinned)?;
        let socket = GuardedSocket::new(
            prepared.socket,
            &prepared.lock,
            pinned.local,
            &pinned.peers,
            prepared.reachability,
            prepared.signal,
        )?;
        Self::assemble(
            Carrier::guarded(Arc::new(socket)),
            runtime,
            Some(prepared.watch),
            Role::Dialing {
                peer: selection.peer,
                client,
            },
        )
    }

    /// The default server configuration refuses everyone; a listener accepts only with its own.
    fn assemble(
        carrier: Carrier,
        runtime: HandleRuntime,
        watch: Option<native::Watch>,
        role: Role,
    ) -> io::Result<Self> {
        let Carrier {
            socket,
            signal,
            lifetime,
            reachability,
        } = carrier;
        let refusing = SecureQuicConfig::server_refusing_all().map_err(io::Error::other)?;
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
            members: Box::new([]),
            reachability,
            signal,
            endpoint: Mutex::new(Some(endpoint.clone())),
        });
        Ok(Self {
            endpoint,
            shared,
            role,
            socket_lifetime: lifetime,
            _watch: watch,
            _owner_thread: PhantomData,
        })
    }

    /// The first connection whose handshake finishes. Handshakes run one at a time, each from an
    /// address that passed a stateless retry, so a failed one from another computer locks no one
    /// out; once one is established the listener refuses everything, so a code is tried once.
    pub async fn accept(&self) -> io::Result<quinn::Connection> {
        let Role::Listening(listener) = &self.role else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "this pairing endpoint dials",
            ));
        };
        let mut listener = listener.lock().await;
        if listener.established {
            return Err(io::Error::other(
                "this pairing listener already admitted its one connection",
            ));
        }
        loop {
            self.shared.check_active()?;
            let Some(incoming) = self.endpoint.accept().await else {
                return Err(self.shared.stopped("accept"));
            };
            self.shared.check_active()?;
            let SocketAddr::V4(remote) = incoming.remote_address() else {
                incoming.ignore();
                continue;
            };
            // A blind spoofer cannot start a handshake.
            if !incoming.remote_address_validated() {
                if let Err(error) = incoming.retry() {
                    error.into_incoming().ignore();
                }
                continue;
            }
            if !listener.budget.take(Instant::now()) {
                incoming.refuse();
                continue;
            }
            let connecting = match register_incoming(
                &self.endpoint,
                &self.shared.signal,
                incoming,
                listener.server.clone(),
            ) {
                Ok(connecting) => connecting,
                Err(error) => {
                    self.shared.check_active()?;
                    log::debug!("pairing listener: a handshake could not start: {error}");
                    continue;
                }
            };
            let connection =
                match tokio::time::timeout(INCOMING_HANDSHAKE_DEADLINE, connecting).await {
                    Ok(Ok(connection)) => connection,
                    Ok(Err(error)) => {
                        log::info!("pairing listener: a handshake failed: {error}");
                        continue;
                    }
                    Err(_) => {
                        log::info!("pairing listener: a handshake did not finish in time");
                        continue;
                    }
                };
            if let Err(error) = self.shared.check_active() {
                connection.close(0_u32.into(), b"network revoked");
                return Err(error);
            }
            if connection.remote_address() != SocketAddr::V4(remote)
                || presented_certificate(&connection).is_none()
            {
                connection.close(0_u32.into(), b"not a pairing peer");
                continue;
            }
            listener.established = true;
            self.endpoint.set_server_config(None);
            if listener.chosen.set(remote).is_err() {
                connection.close(0_u32.into(), b"listener already chose");
                return Err(io::Error::other(
                    "this pairing listener already chose a host",
                ));
            }
            return Ok(connection);
        }
    }

    /// Dials the showing computer's exact address with the pairing configuration.
    pub async fn dial(&self) -> io::Result<quinn::Connection> {
        let Role::Dialing { peer, client } = &self.role else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "this pairing endpoint listens",
            ));
        };
        self.shared.check_active()?;
        let connecting =
            match self
                .endpoint
                .connect_with(client.clone(), (*peer).into(), LOCAL_TLS_SERVER_NAME)
            {
                Ok(connecting) => connecting,
                Err(quinn::ConnectError::EndpointStopping) => {
                    return Err(self.shared.stopped("dial"));
                }
                Err(error) => return Err(io::Error::other(error)),
            };
        let connection = connecting.await.map_err(io::Error::other)?;
        if let Err(error) = self.shared.check_active() {
            connection.close(0_u32.into(), b"network revoked");
            return Err(error);
        }
        if connection.remote_address() != SocketAddr::V4(*peer)
            || presented_certificate(&connection).is_none()
        {
            connection.close(0_u32.into(), b"not the showing computer");
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the connection did not come from the dialed computer",
            ));
        }
        Ok(connection)
    }

    /// The address this endpoint's socket is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddrV4> {
        match self.endpoint.local_addr()? {
            SocketAddr::V4(local) => Ok(local),
            SocketAddr::V6(_) => Err(io::Error::from(io::ErrorKind::InvalidData)),
        }
    }

    pub fn is_revoked(&self) -> bool {
        self.shared.signal.is_revoked()
    }

    /// Cross-thread cancellation only.
    pub fn revoker(&self) -> EndpointRevoker {
        EndpointRevoker {
            shared: self.shared.clone(),
        }
    }

    /// Lets Quinn deliver final acknowledgements before the socket closes.
    pub async fn close_and_wait_idle(&self) -> io::Result<()> {
        self.endpoint.close(0_u32.into(), b"exchange complete");
        wait_idle_or_revoked(self.endpoint.wait_idle(), &self.shared.signal).await
    }

    /// Resolves once the OS socket is closed.
    pub fn socket_closed(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut lifetime = self.socket_lifetime.clone();
        async move { while lifetime.changed().await.is_ok() {} }
    }

    #[track_caller]
    pub fn revoke(&self) {
        self.shared.revoke();
    }
}

impl Listener {
    fn new(server: quinn::ServerConfig, chosen: Arc<OnceLock<SocketAddrV4>>) -> Self {
        Self {
            server: Arc::new(server),
            chosen,
            budget: handshake_budget(Instant::now()),
            established: false,
        }
    }
}

impl Drop for PairingEndpoint {
    fn drop(&mut self) {
        self.revoke();
        self.shared
            .endpoint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

#[cfg(test)]
impl PairingEndpoint {
    /// A listener over a test socket built by `GuardedSocket::listening` with the same `chosen`.
    pub(super) fn listening_over<I: socket::DatagramIo>(
        socket: GuardedSocket<I>,
        chosen: Arc<OnceLock<SocketAddrV4>>,
        identity: &DeviceIdentity,
    ) -> io::Result<Self> {
        let server = SecureQuicConfig::pairing_server(identity).map_err(io::Error::other)?;
        Self::assemble(
            Carrier::guarded(Arc::new(socket)),
            HandleRuntime::current()?,
            None,
            Role::Listening(tokio::sync::Mutex::new(Listener::new(server, chosen))),
        )
    }

    /// A dialer over a test socket admitting exactly `peer`.
    pub(super) fn dialing_over<I: socket::DatagramIo>(
        socket: GuardedSocket<I>,
        peer: SocketAddrV4,
        identity: &DeviceIdentity,
    ) -> io::Result<Self> {
        Self::assemble(
            Carrier::guarded(Arc::new(socket)),
            HandleRuntime::current()?,
            None,
            Role::Dialing {
                peer,
                client: SecureQuicConfig::pairing_client(identity).map_err(io::Error::other)?,
            },
        )
    }
}
