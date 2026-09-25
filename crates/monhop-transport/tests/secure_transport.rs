use std::{sync::Arc, time::Duration};

use monhop_transport::crypto::{
    CertificateFingerprint, CryptoError, DeviceIdentity, LOCAL_ALPN, LOCAL_TLS_SERVER_NAME,
    SecureQuicConfig, VerifiedPeer,
};
use quinn::Endpoint;
use rcgen::{
    CertificateParams, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256,
};
use rustls::{RootCertStore, client::WebPkiServerVerifier, pki_types::CertificateDer};
use tokio::{sync::oneshot, time::timeout};
use zeroize::{Zeroize, Zeroizing};

const NETWORK_TIMEOUT: Duration = Duration::from_secs(5);
const PING: &[u8] = b"monhop transport ping";
const PONG: &[u8] = b"monhop transport pong";
/// Many times what the datagram send buffer holds, so the oldest queued datagrams are dropped.
const STALLED_DATAGRAMS: usize = 1024;

#[tokio::test]
async fn exact_mutual_pins_negotiate_tls13_alpn_and_transport_fixed_ping() {
    timeout(NETWORK_TIMEOUT, async {
        let server_identity = DeviceIdentity::generate().expect("server identity");
        let client_identity = DeviceIdentity::generate().expect("client identity");
        let server_peer = paired_peer(&server_identity);
        let client_peer = paired_peer(&client_identity);

        let server = Endpoint::server(
            SecureQuicConfig::server(&server_identity, &client_peer).expect("server config"),
            loopback_unspecified_port(),
        )
        .expect("loopback server endpoint");
        let server_address = server.local_addr().expect("server address");
        let (complete_ping, ping_complete) = oneshot::channel();
        let server_task = tokio::spawn(serve_fixed_ping(server.clone(), ping_complete));

        let mut client =
            Endpoint::client(loopback_unspecified_port()).expect("loopback client endpoint");
        client.set_default_client_config(
            SecureQuicConfig::client(&client_identity, &server_peer).expect("client config"),
        );
        let connection = client
            .connect(server_address, LOCAL_TLS_SERVER_NAME)
            .expect("valid TLS name")
            .await
            .expect("mutually pinned connection");

        let handshake = connection
            .handshake_data()
            .expect("completed handshake data")
            .downcast::<quinn::crypto::rustls::HandshakeData>()
            .expect("rustls handshake data");
        assert_eq!(handshake.protocol.as_deref(), Some(LOCAL_ALPN));

        let (mut send, mut receive) = connection.open_bi().await.expect("open stream");
        send.write_all(PING).await.expect("write fixed ping");
        send.finish().expect("finish ping");
        assert_eq!(
            receive.read_to_end(128).await.expect("read fixed pong"),
            PONG
        );

        complete_ping
            .send(())
            .expect("keep server connection alive for pong");
        server_task.await.expect("server task");
        connection.close(quinn::VarInt::from_u32(0), b"test complete");
        client.close(quinn::VarInt::from_u32(0), b"test complete");
        server.close(quinn::VarInt::from_u32(0), b"test complete");
    })
    .await
    .expect("loopback test exceeded five seconds");
}

#[tokio::test]
async fn a_stalled_datagram_queue_drops_its_oldest_datagrams_without_panicking() {
    timeout(NETWORK_TIMEOUT, async {
        let server_identity = DeviceIdentity::generate().expect("server identity");
        let client_identity = DeviceIdentity::generate().expect("client identity");
        let server_peer = paired_peer(&server_identity);
        let client_peer = paired_peer(&client_identity);

        let server = Endpoint::server(
            SecureQuicConfig::server(&server_identity, &client_peer).expect("server config"),
            loopback_unspecified_port(),
        )
        .expect("loopback server endpoint");
        let server_address = server.local_addr().expect("server address");
        let server_task = tokio::spawn(async move {
            let incoming = server.accept().await.expect("incoming connection");
            let connection = incoming.await.expect("authenticated connection");
            connection.closed().await;
            server.close(quinn::VarInt::from_u32(0), b"test complete");
        });

        let mut client =
            Endpoint::client(loopback_unspecified_port()).expect("loopback client endpoint");
        client.set_default_client_config(
            SecureQuicConfig::client(&client_identity, &server_peer).expect("client config"),
        );
        let connection = client
            .connect(server_address, LOCAL_TLS_SERVER_NAME)
            .expect("valid TLS name")
            .await
            .expect("mutually pinned connection");

        // Nothing yields, so the connection driver cannot drain the queue, as in a Wi-Fi stall.
        let datagram = bytes::Bytes::from_static(&[0; 64]);
        for _ in 0..STALLED_DATAGRAMS {
            connection
                .send_datagram(datagram.clone())
                .expect("a full queue drops its oldest datagram");
        }

        connection.close(quinn::VarInt::from_u32(0), b"test complete");
        server_task.await.expect("server task");
        client.close(quinn::VarInt::from_u32(0), b"test complete");
    })
    .await
    .expect("loopback test exceeded five seconds");
}

#[tokio::test]
async fn wrong_server_pin_fails_handshake() {
    timeout(NETWORK_TIMEOUT, async {
        let server_identity = DeviceIdentity::generate().expect("server identity");
        let client_identity = DeviceIdentity::generate().expect("client identity");
        let unpaired_identity = DeviceIdentity::generate().expect("unpaired identity");
        let client_peer = paired_peer(&client_identity);
        let wrong_server_peer = paired_peer(&unpaired_identity);

        let server = Endpoint::server(
            SecureQuicConfig::server(&server_identity, &client_peer).expect("server config"),
            loopback_unspecified_port(),
        )
        .expect("loopback server endpoint");
        let address = server.local_addr().expect("server address");
        let server_task = tokio::spawn(observe_rejected_connection(server.clone()));
        let mut client =
            Endpoint::client(loopback_unspecified_port()).expect("loopback client endpoint");
        client.set_default_client_config(
            SecureQuicConfig::client(&client_identity, &wrong_server_peer).expect("client config"),
        );

        assert!(
            client
                .connect(address, LOCAL_TLS_SERVER_NAME)
                .expect("valid TLS name")
                .await
                .is_err()
        );

        server_task.await.expect("server rejection task");
        client.close(quinn::VarInt::from_u32(0), b"test complete");
        server.close(quinn::VarInt::from_u32(0), b"test complete");
    })
    .await
    .expect("loopback test exceeded five seconds");
}

#[tokio::test]
async fn wrong_client_pin_fails_handshake() {
    timeout(NETWORK_TIMEOUT, async {
        let server_identity = DeviceIdentity::generate().expect("server identity");
        let client_identity = DeviceIdentity::generate().expect("client identity");
        let unpaired_identity = DeviceIdentity::generate().expect("unpaired identity");
        let server_peer = paired_peer(&server_identity);
        let wrong_client_peer = paired_peer(&unpaired_identity);

        let server = Endpoint::server(
            SecureQuicConfig::server(&server_identity, &wrong_client_peer).expect("server config"),
            loopback_unspecified_port(),
        )
        .expect("loopback server endpoint");
        let address = server.local_addr().expect("server address");
        let server_task = tokio::spawn(observe_rejected_connection(server.clone()));
        let mut client =
            Endpoint::client(loopback_unspecified_port()).expect("loopback client endpoint");
        client.set_default_client_config(
            SecureQuicConfig::client(&client_identity, &server_peer).expect("client config"),
        );

        let _ = client
            .connect(address, LOCAL_TLS_SERVER_NAME)
            .expect("valid TLS name")
            .await;

        server_task.await.expect("server rejection task");
        client.close(quinn::VarInt::from_u32(0), b"test complete");
        server.close(quinn::VarInt::from_u32(0), b"test complete");
    })
    .await
    .expect("loopback test exceeded five seconds");
}

#[tokio::test]
async fn server_rejects_missing_client_certificate() {
    timeout(NETWORK_TIMEOUT, async {
        let server_identity = DeviceIdentity::generate().expect("server identity");
        let client_identity = DeviceIdentity::generate().expect("client identity");
        let client_peer = paired_peer(&client_identity);

        let server = Endpoint::server(
            SecureQuicConfig::server(&server_identity, &client_peer).expect("server config"),
            loopback_unspecified_port(),
        )
        .expect("loopback server endpoint");
        let address = server.local_addr().expect("server address");
        let server_task = tokio::spawn(observe_rejected_connection(server.clone()));
        let mut client =
            Endpoint::client(loopback_unspecified_port()).expect("loopback client endpoint");
        client.set_default_client_config(no_client_certificate_config(&server_identity));

        let _ = client
            .connect(address, LOCAL_TLS_SERVER_NAME)
            .expect("valid TLS name")
            .await;

        server_task.await.expect("server rejection task");
        client.close(quinn::VarInt::from_u32(0), b"test complete");
        server.close(quinn::VarInt::from_u32(0), b"test complete");
    })
    .await
    .expect("loopback test exceeded five seconds");
}

#[test]
fn malformed_fingerprints_are_rejected() {
    let identity = DeviceIdentity::generate().expect("identity");
    let certificate = identity.certificate_der();
    for malformed in [
        "",
        "00",
        "z000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000:",
    ] {
        assert!(matches!(
            CertificateFingerprint::parse_full(malformed),
            Err(CryptoError::InvalidFingerprint)
        ));
        assert!(VerifiedPeer::from_certificate_der(certificate, malformed).is_err());
    }
}

#[test]
fn key_and_certificate_mismatch_is_rejected_on_import() {
    let first_key = TestKeyPair::generate();
    let second_key = TestKeyPair::generate();
    let mut params = CertificateParams::new(vec![LOCAL_TLS_SERVER_NAME.to_owned()])
        .expect("certificate parameters");
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    let certificate = params.self_signed(first_key.as_ref()).expect("certificate");

    let mut second_key_der = Zeroizing::new(second_key.serialize_der());
    assert!(matches!(
        DeviceIdentity::from_pkcs8_and_certificate(
            second_key_der.as_slice(),
            certificate.der().as_ref()
        ),
        Err(CryptoError::KeyCertificateMismatch)
    ));
    second_key_der.zeroize();
}

async fn serve_fixed_ping(endpoint: Endpoint, ping_complete: oneshot::Receiver<()>) {
    let incoming = endpoint.accept().await.expect("incoming connection");
    let connection = incoming.await.expect("authenticated connection");
    let (mut send, mut receive) = connection.accept_bi().await.expect("incoming stream");
    assert_eq!(
        receive.read_to_end(128).await.expect("read fixed ping"),
        PING
    );
    send.write_all(PONG).await.expect("write fixed pong");
    send.finish().expect("finish pong");
    ping_complete.await.expect("client read pong");
    connection.close(quinn::VarInt::from_u32(0), b"test complete");
}

async fn observe_rejected_connection(endpoint: Endpoint) {
    let incoming = endpoint.accept().await.expect("incoming connection");
    assert!(incoming.await.is_err());
}

fn paired_peer(identity: &DeviceIdentity) -> VerifiedPeer {
    VerifiedPeer::from_certificate_der(
        identity.certificate_der(),
        &identity.fingerprint().full_hex(),
    )
    .expect("complete out-of-band fingerprint matches peer DER")
}

fn no_client_certificate_config(server_identity: &DeviceIdentity) -> quinn::ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(
            server_identity.certificate_der().to_vec(),
        ))
        .expect("server self-signed trust anchor");
    let verifier = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .expect("server verifier");
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3")
        .with_webpki_verifier(verifier)
        .with_no_client_auth();
    tls.alpn_protocols = vec![LOCAL_ALPN.to_vec()];
    tls.enable_early_data = false;
    tls.resumption = rustls::client::Resumption::disabled();
    let quic_tls =
        quinn::crypto::rustls::QuicClientConfig::try_from(Arc::new(tls)).expect("QUIC TLS config");
    quinn::ClientConfig::new(Arc::new(quic_tls))
}

fn loopback_unspecified_port() -> std::net::SocketAddr {
    "127.0.0.1:0"
        .parse()
        .expect("explicit IPv4 loopback address")
}

struct TestKeyPair(KeyPair);

impl TestKeyPair {
    fn generate() -> Self {
        Self(KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("P-256 key"))
    }

    fn as_ref(&self) -> &KeyPair {
        &self.0
    }

    fn serialize_der(&self) -> Vec<u8> {
        self.0.serialize_der()
    }
}

impl Drop for TestKeyPair {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}
