use std::{
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

use monhop_core::{DisplayId, HidUsage, ModifierState, Platform, Point};
use monhop_protocol::{Capabilities, DisplayDescription, DisplayTopology, Frame, Key, Message};
use monhop_transport::{
    crypto::{DeviceIdentity, LOCAL_TLS_SERVER_NAME, SecureQuicConfig, VerifiedPeer},
    session_handshake::{
        HandshakeConfig, HandshakeError, NegotiatedSession, SessionPurpose,
        device_id_from_fingerprint, negotiate,
    },
    session_setup::{InspectedPeer, SetupFailure, complete_metadata_inspection},
    session_wire::write_frame,
};

fn pin(identity: &DeviceIdentity) -> VerifiedPeer {
    VerifiedPeer::from_certificate_der(
        identity.certificate_der(),
        &identity.fingerprint().full_hex(),
    )
    .expect("generated certificate has a matching full fingerprint")
}

fn topology(display_id: u64) -> DisplayTopology {
    DisplayTopology::new(vec![DisplayDescription {
        id: DisplayId(display_id),
        name: "loopback display".to_owned(),
        native_width: 1_920,
        native_height: 1_080,
        logical_origin: Point::new(0.0, 0.0),
        logical_size: Point::new(1_920.0, 1_080.0),
        scale_factor: 1.0,
        is_primary: true,
        monitor: None,
    }])
    .expect("valid generated topology")
}

fn capabilities() -> Capabilities {
    Capabilities::new(Capabilities::RELATIVE_MOTION | Capabilities::DISPLAY_TOPOLOGY)
        .expect("known capabilities")
}

async fn authenticated_connections(
    client_identity: &DeviceIdentity,
    server_identity: &DeviceIdentity,
    server_pin: &VerifiedPeer,
    client_pin: &VerifiedPeer,
) -> (
    quinn::Endpoint,
    quinn::Endpoint,
    quinn::Connection,
    quinn::Connection,
) {
    let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let server = quinn::Endpoint::server(
        SecureQuicConfig::server(server_identity, client_pin).expect("server configuration"),
        loopback,
    )
    .expect("loopback server endpoint");
    let mut client = quinn::Endpoint::client(loopback).expect("loopback client endpoint");
    client.set_default_client_config(
        SecureQuicConfig::client(client_identity, server_pin).expect("client configuration"),
    );
    let connecting = client
        .connect(
            server.local_addr().expect("server address"),
            LOCAL_TLS_SERVER_NAME,
        )
        .expect("loopback connection");
    let (client_connection, server_connection) = tokio::join!(
        async { connecting.await.expect("client TLS connection") },
        async {
            server
                .accept()
                .await
                .expect("incoming connection")
                .await
                .expect("server TLS connection")
        },
    );
    (client, server, client_connection, server_connection)
}

async fn negotiated_sessions(
    source_is_dialer: bool,
    client_purpose: SessionPurpose,
    server_purpose: SessionPurpose,
) -> (
    quinn::Endpoint,
    quinn::Endpoint,
    Result<NegotiatedSession, HandshakeError>,
    Result<NegotiatedSession, HandshakeError>,
) {
    let client_identity = DeviceIdentity::generate().expect("client identity");
    let server_identity = DeviceIdentity::generate().expect("server identity");
    let server_pin = pin(&server_identity);
    let client_pin = pin(&client_identity);
    let (client_endpoint, server_endpoint, client_connection, server_connection) =
        authenticated_connections(&client_identity, &server_identity, &server_pin, &client_pin)
            .await;
    let client_topology = topology(1);
    let server_topology = topology(2);
    let source = device_id_from_fingerprint(if source_is_dialer {
        client_identity.fingerprint()
    } else {
        server_identity.fingerprint()
    });
    let client_config = HandshakeConfig::new(
        &client_identity,
        &server_pin,
        Platform::Windows,
        Platform::MacOs,
        capabilities(),
        capabilities(),
        &client_topology,
        source,
        client_purpose,
    )
    .expect("client inspection configuration");
    let server_config = HandshakeConfig::new(
        &server_identity,
        &client_pin,
        Platform::MacOs,
        Platform::Windows,
        capabilities(),
        capabilities(),
        &server_topology,
        source,
        server_purpose,
    )
    .expect("server inspection configuration");
    let (client_session, server_session) = tokio::join!(
        negotiate(client_connection, client_config),
        negotiate(server_connection, server_config),
    );
    (
        client_endpoint,
        server_endpoint,
        client_session,
        server_session,
    )
}

async fn inspected_sessions(
    source_is_dialer: bool,
) -> (
    quinn::Endpoint,
    quinn::Endpoint,
    NegotiatedSession,
    NegotiatedSession,
) {
    let (client_endpoint, server_endpoint, client_session, server_session) = negotiated_sessions(
        source_is_dialer,
        SessionPurpose::Inspect,
        SessionPurpose::Inspect,
    )
    .await;
    (
        client_endpoint,
        server_endpoint,
        client_session.expect("client inspection handshake"),
        server_session.expect("server inspection handshake"),
    )
}

#[tokio::test]
async fn metadata_completion_is_bilateral_for_both_source_and_connection_roles() {
    tokio::time::timeout(Duration::from_secs(5), async {
        for source_is_dialer in [true, false] {
            let (client_endpoint, server_endpoint, mut client_session, mut server_session) =
                inspected_sessions(source_is_dialer).await;
            assert_eq!(client_session.local_is_source(), source_is_dialer);
            assert_eq!(server_session.local_is_source(), !source_is_dialer);
            assert_eq!(client_session.purpose(), SessionPurpose::Inspect);
            assert_eq!(server_session.purpose(), SessionPurpose::Inspect);

            let (client_result, server_result) = tokio::join!(
                async {
                    let result = complete_metadata_inspection(&mut client_session).await;
                    client_endpoint.close(0_u32.into(), b"inspection complete");
                    client_endpoint.wait_idle().await;
                    result
                },
                async {
                    let result = complete_metadata_inspection(&mut server_session).await;
                    server_endpoint.close(0_u32.into(), b"inspection complete");
                    server_endpoint.wait_idle().await;
                    result
                },
            );
            assert_eq!(client_result, Ok(()));
            assert_eq!(server_result, Ok(()));

            client_endpoint.close(0_u32.into(), b"test complete");
            server_endpoint.close(0_u32.into(), b"test complete");
        }
    })
    .await
    .expect("bounded loopback metadata completion");
}

#[tokio::test]
async fn mixed_purpose_is_rejected_before_metadata_completion() {
    let (client_endpoint, server_endpoint, client_result, server_result) =
        negotiated_sessions(true, SessionPurpose::Inspect, SessionPurpose::Share).await;

    assert!(client_result.is_err());
    assert!(server_result.is_err());
    client_endpoint.close(0_u32.into(), b"test complete");
    server_endpoint.close(0_u32.into(), b"test complete");
}

#[tokio::test]
async fn metadata_completion_rejects_input_before_the_barrier() {
    let (client_endpoint, server_endpoint, mut client_session, mut server_session) =
        inspected_sessions(true).await;
    let input = Frame::new(
        client_session.initial_epoch,
        client_session.control.next_sequence(),
        Message::Key(Key {
            usage: HidUsage(0x04),
            is_down: true,
            repeat: false,
            modifiers: ModifierState::default(),
        }),
    );
    write_frame(
        &client_session.connection,
        &mut client_session.control_streams.send,
        &input,
    )
    .await
    .expect("synthetic input frame write");
    client_session
        .control_streams
        .send
        .finish()
        .expect("synthetic input stream finish");

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        complete_metadata_inspection(&mut server_session),
    )
    .await
    .expect("bounded metadata rejection");
    assert_eq!(result, Err(SetupFailure::Handshake));

    client_endpoint.close(0_u32.into(), b"test complete");
    server_endpoint.close(0_u32.into(), b"test complete");
}

#[test]
fn inspection_recheck_requires_the_same_route() {
    let local_identity = DeviceIdentity::generate().expect("local identity");
    let peer_identity = DeviceIdentity::generate().expect("peer identity");
    let local_device = device_id_from_fingerprint(local_identity.fingerprint());
    let peer_device = device_id_from_fingerprint(peer_identity.fingerprint());
    let inspection = InspectedPeer {
        local_device,
        peer_device,
        source: local_device,
        local_fingerprint: local_identity.fingerprint(),
        peer_fingerprint: peer_identity.fingerprint(),
        local_platform: Platform::Windows,
        peer_platform: Platform::MacOs,
        local_displays: topology(1),
        peer_displays: topology(2),
        interface_id: "loopback-route".to_owned(),
    };
    let mut changed_route = inspection.clone();
    changed_route.source = peer_device;

    assert!(inspection.matches(&inspection));
    assert!(!inspection.matches(&changed_route));
}
