use std::{net::SocketAddrV4, sync::Arc, time::Duration};

use monhop_core::Platform;
use monhop_transport::{
    crypto::{DeviceIdentity, LOCAL_TLS_SERVER_NAME, PAIRING_ALPN, SecureQuicConfig, VerifiedPeer},
    pairing_code::PairingCode,
    pairing_exchange::{
        ConfirmedPairing, ConfirmedPeer, PairingFailure, PairingRole, confirm_pairing,
    },
};
use quinn::{Connection, Endpoint};
use rustls::{RootCertStore, client::WebPkiServerVerifier, pki_types::CertificateDer};
use tokio::time::timeout;

const NETWORK_TIMEOUT: Duration = Duration::from_secs(15);
const SHOWING_PLATFORM: Platform = Platform::MacOs;
const ENTERING_PLATFORM: Platform = Platform::Windows;

struct Side {
    identity: DeviceIdentity,
    endpoint: Endpoint,
}

impl Side {
    fn showing() -> Self {
        let identity = DeviceIdentity::generate().expect("identity");
        let endpoint = Endpoint::server(
            SecureQuicConfig::pairing_server(&identity).expect("pairing server"),
            loopback(),
        )
        .expect("loopback server");
        Self { identity, endpoint }
    }

    fn entering() -> Self {
        let identity = DeviceIdentity::generate().expect("identity");
        let mut endpoint = Endpoint::client(loopback()).expect("loopback client");
        endpoint.set_default_client_config(
            SecureQuicConfig::pairing_client(&identity).expect("pairing client"),
        );
        Self { identity, endpoint }
    }

    fn address(&self) -> SocketAddrV4 {
        match self.endpoint.local_addr().expect("bound") {
            std::net::SocketAddr::V4(address) => address,
            std::net::SocketAddr::V6(_) => unreachable!("bound to IPv4 loopback"),
        }
    }

    async fn dial(&self, to: SocketAddrV4) -> Connection {
        self.endpoint
            .connect(to.into(), LOCAL_TLS_SERVER_NAME)
            .expect("valid name")
            .await
            .expect("pairing handshake")
    }

    async fn accept(&self) -> Connection {
        self.endpoint
            .accept()
            .await
            .expect("incoming")
            .await
            .expect("pairing handshake")
    }

    async fn confirm(
        &self,
        connection: &Connection,
        role: PairingRole,
        code: &PairingCode,
    ) -> Result<ConfirmedPairing, PairingFailure> {
        let platform = match role {
            PairingRole::Showing => SHOWING_PLATFORM,
            PairingRole::Entering => ENTERING_PLATFORM,
        };
        confirm_pairing(
            connection,
            role,
            code,
            &self.identity,
            self.address(),
            platform,
        )
        .await
    }

    async fn pair(
        &self,
        connection: &Connection,
        role: PairingRole,
        code: &PairingCode,
    ) -> Result<ConfirmedPeer, PairingFailure> {
        let confirmed = self.confirm(connection, role, code).await?;
        let peer = confirmed.peer().clone();
        confirmed.finish_saved().await?;
        Ok(peer)
    }
}

fn loopback() -> std::net::SocketAddr {
    "127.0.0.1:0".parse().expect("loopback address")
}

/// The code as the entering computer's user typed it, from the showing computer's screen.
fn typed(code: &PairingCode) -> PairingCode {
    PairingCode::parse(&code.display().to_ascii_lowercase()).expect("typed as shown")
}

#[tokio::test]
async fn the_right_code_pairs_and_each_side_learns_the_others_exact_identity() {
    timeout(NETWORK_TIMEOUT, async {
        let showing = Side::showing();
        let entering = Side::entering();
        let code = PairingCode::generate(*showing.address().ip()).expect("code");
        let (at_showing, at_entering) = tokio::join!(
            async {
                let connection = showing.accept().await;
                let handshake = connection
                    .handshake_data()
                    .expect("handshake data")
                    .downcast::<quinn::crypto::rustls::HandshakeData>()
                    .expect("rustls handshake data");
                assert_eq!(handshake.protocol.as_deref(), Some(PAIRING_ALPN));
                showing.pair(&connection, PairingRole::Showing, &code).await
            },
            async {
                let connection = entering.dial(showing.address()).await;
                entering
                    .pair(&connection, PairingRole::Entering, &typed(&code))
                    .await
            }
        );
        let at_showing = at_showing.expect("showing side paired");
        let at_entering = at_entering.expect("entering side paired");
        assert_eq!(at_showing.certificate, entering.identity.certificate_der());
        assert_eq!(at_showing.endpoint, entering.address());
        assert_eq!(at_showing.platform, ENTERING_PLATFORM);
        assert!(at_showing.fingerprint() == entering.identity.fingerprint());
        assert_eq!(at_entering.certificate, showing.identity.certificate_der());
        assert_eq!(at_entering.endpoint, showing.address());
        assert_eq!(at_entering.platform, SHOWING_PLATFORM);
    })
    .await
    .expect("pairing exceeded its bound");
}

#[tokio::test]
async fn a_wrong_code_fails_both_sides() {
    timeout(NETWORK_TIMEOUT, async {
        let showing = Side::showing();
        let entering = Side::entering();
        let code = PairingCode::generate(*showing.address().ip()).expect("code");
        let wrong = PairingCode::generate(*showing.address().ip()).expect("another code");
        let (at_showing, at_entering) = tokio::join!(
            async {
                let connection = showing.accept().await;
                showing.pair(&connection, PairingRole::Showing, &code).await
            },
            async {
                let connection = entering.dial(showing.address()).await;
                entering
                    .pair(&connection, PairingRole::Entering, &wrong)
                    .await
            }
        );
        assert_eq!(at_showing.unwrap_err(), PairingFailure::NotConfirmed);
        assert_eq!(at_entering.unwrap_err(), PairingFailure::NotConfirmed);
    })
    .await
    .expect("a wrong code must fail promptly, not at a deadline");
}

/// A relay terminating TLS separately with each side, its own certificate on both, forwards every
/// pairing byte unchanged. Both sides use the right code, yet neither confirms: each binds the
/// confirmation to its own TLS session and the certificate it saw.
#[tokio::test]
async fn a_tls_terminating_relay_cannot_confirm_even_with_the_right_code() {
    timeout(NETWORK_TIMEOUT, async {
        let showing = Side::showing();
        let entering = Side::entering();
        let relay_identity = DeviceIdentity::generate().expect("relay identity");
        let mut relay = Endpoint::server(
            SecureQuicConfig::pairing_server(&relay_identity).expect("relay server"),
            loopback(),
        )
        .expect("relay endpoint");
        relay.set_default_client_config(
            SecureQuicConfig::pairing_client(&relay_identity).expect("relay client"),
        );
        let relay_address = match relay.local_addr().expect("bound") {
            std::net::SocketAddr::V4(address) => address,
            std::net::SocketAddr::V6(_) => unreachable!("bound to IPv4 loopback"),
        };
        let code = PairingCode::generate(*showing.address().ip()).expect("code");

        let (at_showing, at_entering, ()) = tokio::join!(
            async {
                let connection = showing.accept().await;
                showing.pair(&connection, PairingRole::Showing, &code).await
            },
            async {
                let connection = entering.dial(relay_address).await;
                entering
                    .pair(&connection, PairingRole::Entering, &typed(&code))
                    .await
            },
            async {
                let from_entering = relay
                    .accept()
                    .await
                    .expect("incoming")
                    .await
                    .expect("relay handshake with the entering side");
                let to_showing = relay
                    .connect(showing.address().into(), LOCAL_TLS_SERVER_NAME)
                    .expect("valid name")
                    .await
                    .expect("relay handshake with the showing side");
                forward(&from_entering, &to_showing).await;
            }
        );
        assert_eq!(at_showing.unwrap_err(), PairingFailure::NotConfirmed);
        assert_eq!(at_entering.unwrap_err(), PairingFailure::NotConfirmed);
    })
    .await
    .expect("a relayed attempt must fail promptly");
}

/// Forwards one bidirectional stream each way until either side ends, then closes both.
async fn forward(from_entering: &Connection, to_showing: &Connection) {
    let pumped = async {
        let (to_entering, from_entering_stream) = from_entering.accept_bi().await.ok()?;
        let (to_showing_stream, from_showing) = to_showing.open_bi().await.ok()?;
        tokio::select! {
            () = pump(from_entering_stream, to_showing_stream) => {}
            () = pump(from_showing, to_entering) => {}
        }
        Some(())
    };
    let _ = pumped.await;
    from_entering.close(0_u32.into(), b"relay done");
    to_showing.close(0_u32.into(), b"relay done");
}

async fn pump(mut from: quinn::RecvStream, mut to: quinn::SendStream) {
    while let Ok(Some(chunk)) = from.read_chunk(4096, true).await {
        if to.write_all(&chunk.bytes).await.is_err() {
            return;
        }
    }
    let _ = to.finish();
}

#[tokio::test]
async fn a_side_that_never_saves_fails_the_other_at_saved() {
    timeout(NETWORK_TIMEOUT, async {
        let showing = Side::showing();
        let entering = Side::entering();
        let code = PairingCode::generate(*showing.address().ip()).expect("code");
        let (at_showing, ()) = tokio::join!(
            async {
                let connection = showing.accept().await;
                showing.pair(&connection, PairingRole::Showing, &code).await
            },
            async {
                let connection = entering.dial(showing.address()).await;
                let confirmed = entering
                    .confirm(&connection, PairingRole::Entering, &typed(&code))
                    .await
                    .expect("confirmed");
                // The save failed here: the exchange ends without a Saved frame.
                drop(confirmed);
                connection.close(0_u32.into(), b"save failed");
            }
        );
        assert_eq!(at_showing.unwrap_err(), PairingFailure::Interrupted);
    })
    .await
    .expect("a missing Saved frame must fail promptly");
}

#[tokio::test]
async fn sharing_and_pairing_configurations_never_accept_each_other() {
    timeout(NETWORK_TIMEOUT, async {
        let showing = Side::showing();
        let entering = Side::entering();
        let pin = |identity: &DeviceIdentity| {
            VerifiedPeer::from_certificate_der(
                identity.certificate_der(),
                &identity.fingerprint().full_hex(),
            )
            .expect("pin")
        };

        // A sharing client pinned to the showing computer meets its pairing listener.
        let mut sharing_client = Endpoint::client(loopback()).expect("client");
        sharing_client.set_default_client_config(
            SecureQuicConfig::client(&entering.identity, &pin(&showing.identity))
                .expect("sharing client"),
        );
        let (dialed, accepted) = tokio::join!(
            async {
                sharing_client
                    .connect(showing.address().into(), LOCAL_TLS_SERVER_NAME)
                    .expect("valid name")
                    .await
            },
            async { showing.endpoint.accept().await.expect("incoming").await }
        );
        assert!(dialed.is_err());
        assert!(accepted.is_err());

        // A pairing client meets a sharing server that pins it exactly.
        let sharing_server = Endpoint::server(
            SecureQuicConfig::server(&showing.identity, &pin(&entering.identity))
                .expect("sharing server"),
            loopback(),
        )
        .expect("server");
        let (dialed, accepted) = tokio::join!(
            async {
                entering
                    .endpoint
                    .connect(
                        sharing_server.local_addr().expect("bound"),
                        LOCAL_TLS_SERVER_NAME,
                    )
                    .expect("valid name")
                    .await
            },
            async { sharing_server.accept().await.expect("incoming").await }
        );
        assert!(dialed.is_err());
        assert!(accepted.is_err());
    })
    .await
    .expect("configuration checks exceeded their bound");
}

#[tokio::test]
async fn a_pairing_listener_requires_a_client_certificate() {
    timeout(NETWORK_TIMEOUT, async {
        let showing = Side::showing();
        let mut anonymous = Endpoint::client(loopback()).expect("client");
        anonymous.set_default_client_config(no_client_certificate(&showing.identity));
        let (dialed, accepted) = tokio::join!(
            async {
                anonymous
                    .connect(showing.address().into(), LOCAL_TLS_SERVER_NAME)
                    .expect("valid name")
                    .await
            },
            async { showing.endpoint.accept().await.expect("incoming").await }
        );
        assert!(accepted.is_err());
        if let Ok(connection) = dialed {
            assert!(
                timeout(Duration::from_secs(2), connection.closed())
                    .await
                    .is_ok(),
                "the listener closed the anonymous connection"
            );
        }
    })
    .await
    .expect("the anonymous attempt exceeded its bound");
}

/// A pairing-ALPN client that trusts the showing computer but presents no certificate.
fn no_client_certificate(server: &DeviceIdentity) -> quinn::ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(server.certificate_der().to_vec()))
        .expect("root");
    let verifier = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .expect("verifier");
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3")
        .with_webpki_verifier(verifier)
        .with_no_client_auth();
    tls.alpn_protocols = vec![PAIRING_ALPN.to_vec()];
    quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(Arc::new(tls)).expect("QUIC TLS"),
    ))
}
