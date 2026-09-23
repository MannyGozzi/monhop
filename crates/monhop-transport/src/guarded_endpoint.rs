//! Explicit, physical-interface-bound QUIC. Pairing never authorizes input.

mod native;
mod socket;

use std::{io, marker::PhantomData, net::SocketAddrV4, rc::Rc, sync::Arc};

use monhop_core::revocation::RevocationSignal;
use tokio::sync::watch;

use crate::{
    crypto::{DeviceIdentity, LOCAL_TLS_SERVER_NAME, SecureQuicConfig, VerifiedPeer},
    policy::is_private_or_link_local,
};
use socket::GuardedSocket;

/// Identifies a failed native route check without exposing platform error details.
#[derive(Debug)]
pub struct RouteCheckFailure;

impl std::fmt::Display for RouteCheckFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the selected physical route could not be verified")
    }
}

impl std::error::Error for RouteCheckFailure {}

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

/// Owns the native watcher on its creating thread and never exposes endpoint rebinding.
/// This is a transport boundary, not proof of human pairing or permission to forward input.
pub struct GuardedEndpoint {
    endpoint: quinn::Endpoint,
    signal: RevocationSignal,
    peer: SocketAddrV4,
    socket_lifetime: watch::Receiver<()>,
    _watch: native::Watch,
    _owner_thread: PhantomData<Rc<()>>,
}

/// Cross-thread cancellation only. It cannot connect, accept, send, or rebind.
#[derive(Clone)]
pub struct EndpointRevoker {
    endpoint: quinn::Endpoint,
    signal: RevocationSignal,
}

impl EndpointRevoker {
    #[track_caller]
    pub fn revoke(&self) {
        self.signal.revoke();
        self.endpoint.close(0_u32.into(), b"network revoked");
    }

    pub fn is_revoked(&self) -> bool {
        self.signal.is_revoked()
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
        tokio::runtime::Handle::try_current()
            .map_err(|_| io::Error::other("an active Tokio I/O runtime is required"))?;
        let client = SecureQuicConfig::client(identity, peer_identity).map_err(io::Error::other)?;
        let server = SecureQuicConfig::server(identity, peer_identity).map_err(io::Error::other)?;
        let prepared = native::prepare(&selection)?;
        let signal = prepared.signal;
        let socket = GuardedSocket::new(
            prepared.socket,
            &prepared.lock,
            selection.local,
            selection.peer,
            signal.clone(),
        )?;
        let socket_lifetime = socket.lifetime();
        let mut endpoint = quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            Some(server),
            Arc::new(socket),
            Arc::new(quinn::TokioRuntime),
        )?;
        endpoint.set_default_client_config(client);
        if signal.is_revoked() {
            endpoint.close(0_u32.into(), b"network revoked");
            return Err(socket::revoked_error());
        }
        Ok(Self {
            endpoint,
            signal,
            peer: selection.peer,
            socket_lifetime,
            _watch: prepared.watch,
            _owner_thread: PhantomData,
        })
    }

    /// Connects only to the selected numeric peer using the fixed mutual TLS configuration.
    pub fn connect(&self) -> io::Result<quinn::Connecting> {
        self.check_active()?;
        let connecting = match self
            .endpoint
            .connect(self.peer.into(), LOCAL_TLS_SERVER_NAME)
        {
            Ok(connecting) => connecting,
            Err(quinn::ConnectError::EndpointStopping) => return Err(self.stopped("dial")),
            Err(error) => return Err(io::Error::other(error)),
        };
        self.check_active()?;
        Ok(connecting)
    }

    /// Accepts only the configured peer identity. No alternate TLS configuration is exposed.
    pub async fn accept(&self) -> io::Result<quinn::Connection> {
        self.check_active()?;
        let Some(incoming) = self.endpoint.accept().await else {
            return Err(self.stopped("accept"));
        };
        self.check_active()?;
        let connecting = register_incoming(&self.endpoint, &self.signal, incoming)?;
        let connection = connecting.await.map_err(io::Error::other)?;
        if let Err(error) = self.check_active() {
            connection.close(0_u32.into(), b"network revoked");
            return Err(error);
        }
        Ok(connection)
    }

    pub fn is_revoked(&self) -> bool {
        self.signal.is_revoked()
    }

    pub(crate) fn revocation_signal(&self) -> RevocationSignal {
        self.signal.clone()
    }

    pub fn revoker(&self) -> EndpointRevoker {
        EndpointRevoker {
            endpoint: self.endpoint.clone(),
            signal: self.signal.clone(),
        }
    }

    /// Lets Quinn deliver a closed connection's final packets while the endpoint stays open for
    /// the next connection. Only `close_and_wait_idle` retires it.
    pub async fn wait_idle(&self) -> io::Result<()> {
        wait_idle_or_revoked(self.endpoint.wait_idle(), &self.signal).await
    }

    /// Let Quinn deliver final acknowledgements before the socket's hard revocation.
    pub async fn close_and_wait_idle(&self) -> io::Result<()> {
        self.endpoint.close(0_u32.into(), b"exchange complete");
        wait_idle_or_revoked(self.endpoint.wait_idle(), &self.signal).await
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
        self.signal.revoke();
        self.endpoint.close(0_u32.into(), b"network revoked");
    }

    fn check_active(&self) -> io::Result<()> {
        if self.signal.is_revoked() {
            Err(socket::revoked_error())
        } else {
            Ok(())
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
) -> io::Result<quinn::Connecting> {
    let connecting = incoming.accept().map_err(io::Error::other)?;
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
}
