use monhop_core::{DisplayId, Platform, Point};
use monhop_protocol::{Capabilities, DisplayDescription, DisplayTopology, Message, SessionPurpose};
use monhop_transport::{
    crypto::{DeviceIdentity, LOCAL_TLS_SERVER_NAME, SecureQuicConfig, VerifiedPeer},
    session_handshake::{HandshakeConfig, device_id_from_fingerprint, negotiate},
    session_wire::write_frame,
};
use std::{
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

fn pin(identity: &DeviceIdentity) -> VerifiedPeer {
    VerifiedPeer::from_certificate_der(
        identity.certificate_der(),
        &identity.fingerprint().full_hex(),
    )
    .unwrap()
}
fn topology() -> DisplayTopology {
    DisplayTopology::new(vec![DisplayDescription {
        id: DisplayId(1),
        name: "fixture".into(),
        native_width: 1920,
        native_height: 1080,
        logical_origin: Point::default(),
        logical_size: Point::new(1920.0, 1080.0),
        scale_factor: 1.0,
        is_primary: true,
        monitor: None,
    }])
    .unwrap()
}

async fn exchange(source_is_dialer: bool, disagree_source: bool, disagree_purpose: bool) {
    let server_id = DeviceIdentity::generate().unwrap();
    let client_id = DeviceIdentity::generate().unwrap();
    let server_pin = pin(&server_id);
    let client_pin = pin(&client_id);
    let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let server = quinn::Endpoint::server(
        SecureQuicConfig::server(&server_id, &client_pin).unwrap(),
        loopback,
    )
    .unwrap();
    let mut client = quinn::Endpoint::client(loopback).unwrap();
    client.set_default_client_config(SecureQuicConfig::client(&client_id, &server_pin).unwrap());
    let connecting = client
        .connect(server.local_addr().unwrap(), LOCAL_TLS_SERVER_NAME)
        .unwrap();
    let (dialed, accepted) = tokio::join!(async { connecting.await.unwrap() }, async {
        server.accept().await.unwrap().await.unwrap()
    });
    let source = device_id_from_fingerprint(if source_is_dialer {
        client_id.fingerprint()
    } else {
        server_id.fingerprint()
    });
    let other_source = device_id_from_fingerprint(if source_is_dialer {
        server_id.fingerprint()
    } else {
        client_id.fingerprint()
    });
    let topology = topology();
    let capabilities =
        Capabilities::new(Capabilities::RELATIVE_MOTION | Capabilities::DISPLAY_TOPOLOGY).unwrap();
    let client_config = HandshakeConfig::new(
        &client_id,
        &server_pin,
        Platform::Windows,
        Platform::MacOs,
        capabilities,
        capabilities,
        &topology,
        source,
        SessionPurpose::Share,
    )
    .unwrap();
    let server_config = HandshakeConfig::new(
        &server_id,
        &client_pin,
        Platform::MacOs,
        Platform::Windows,
        capabilities,
        capabilities,
        &topology,
        if disagree_source {
            other_source
        } else {
            source
        },
        if disagree_purpose {
            SessionPurpose::Inspect
        } else {
            SessionPurpose::Share
        },
    )
    .unwrap();
    let (dialer, listener) = tokio::join!(
        negotiate(dialed, client_config),
        negotiate(accepted, server_config)
    );
    if disagree_source || disagree_purpose {
        assert!(dialer.is_err());
        assert!(listener.is_err());
    } else {
        let mut dialer = dialer.unwrap();
        let mut listener = listener.unwrap();
        assert_eq!(dialer.source(), source);
        assert_eq!(listener.source(), source);
        assert_eq!(dialer.local_is_source(), source_is_dialer);
        assert_eq!(listener.local_is_source(), !source_is_dialer);
        assert_eq!(dialer.purpose(), SessionPurpose::Share);
        assert_eq!(listener.purpose(), SessionPurpose::Share);
        assert_eq!(dialer.initial_epoch, listener.initial_epoch);
        assert_eq!(dialer.control.next_sequence(), 3);
        let heartbeat = dialer.control.next_heartbeat(Message::Ping(1)).unwrap();
        write_frame(
            &dialer.connection,
            &mut dialer.control_streams.send,
            &heartbeat,
        )
        .await
        .unwrap();
        let received = listener
            .control_streams
            .reader
            .read_frame(&mut listener.control_streams.recv)
            .await
            .unwrap();
        assert_eq!(received, heartbeat);
    }
    client.close(0_u32.into(), b"test complete");
    server.close(0_u32.into(), b"test complete");
}

#[tokio::test]
async fn authenticated_source_selection_is_independent_of_dial_or_listen_role() {
    tokio::time::timeout(Duration::from_secs(5), async {
        exchange(true, false, false).await;
        exchange(false, false, false).await;
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn two_authenticated_peers_cannot_start_with_conflicting_source_choices() {
    tokio::time::timeout(Duration::from_secs(5), exchange(true, true, false))
        .await
        .unwrap();
}
#[tokio::test]
async fn inspection_and_sharing_purposes_must_match_before_runtime_construction() {
    tokio::time::timeout(Duration::from_secs(5), exchange(true, false, true))
        .await
        .unwrap();
}
