//! Unidirectional stream credit on a negotiated Share connection with MonHop's transport config.

use std::{
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

use monhop_core::{DisplayId, Platform, Point};
use monhop_protocol::{
    Capabilities, ControlPermissions, DisplayDescription, DisplayTopology, Frame, Message,
    SessionPurpose,
};
use monhop_transport::{
    crypto::{DeviceIdentity, LOCAL_TLS_SERVER_NAME, SecureQuicConfig, VerifiedPeer},
    session_handshake::{HandshakeConfig, NegotiatedSession, negotiate},
    session_wire::{FrameWriter, decode_datagram, write_frame},
};
use tokio::time::timeout;

const NETWORK_TIMEOUT: Duration = Duration::from_secs(5);
/// Far longer than a loopback round trip: whatever has not happened by then is being refused.
const QUIET: Duration = Duration::from_millis(200);
const FRAMES: u64 = 16;

/// A compliant peer cannot create a stream the receiver never credited, so nothing is ever
/// accepted and the connection stays up; quinn-proto closes a peer that sends one anyway with
/// STREAM_LIMIT_ERROR. Only an explicit credit grant lets a stream through.
#[tokio::test]
async fn without_credit_a_unidirectional_stream_is_never_opened_or_accepted() {
    timeout(NETWORK_TIMEOUT, async {
        let pair = negotiated_pair().await;
        let client = pair.client.connection.clone();
        let server = pair.server.connection.clone();

        assert!(
            timeout(QUIET, client.open_uni()).await.is_err(),
            "the default transport config grants no unidirectional stream"
        );
        assert!(
            timeout(QUIET, server.accept_uni()).await.is_err(),
            "nothing reaches the acceptor"
        );
        assert!(client.close_reason().is_none());
        assert!(server.close_reason().is_none());

        server.set_max_concurrent_uni_streams(1_u32.into());
        let mut stream = timeout(QUIET, client.open_uni())
            .await
            .expect("an explicit grant reaches the peer")
            .expect("the granted stream opens");
        stream
            .write_all(b"granted")
            .await
            .expect("write the stream");
        stream.finish().expect("finish the stream");
        let mut accepted = server.accept_uni().await.expect("accept the stream");
        assert_eq!(
            accepted.read_to_end(64).await.expect("read the stream"),
            b"granted"
        );
    })
    .await
    .expect("loopback test exceeded five seconds");
}

/// Ordered control frames and heartbeat datagrams keep flowing both ways while the peer's
/// unidirectional open waits for credit that never comes.
#[tokio::test]
async fn control_frames_keep_flowing_without_unidirectional_streams() {
    timeout(NETWORK_TIMEOUT, async {
        let mut pair = negotiated_pair().await;
        let uni = pair.client.connection.clone();
        let exchange = async {
            exchange(&mut pair.client, &mut pair.server).await;
            exchange(&mut pair.server, &mut pair.client).await;
        };
        tokio::select! {
            biased;
            _ = uni.open_uni() => panic!("a unidirectional stream opened without credit"),
            () = exchange => {}
        }
        assert!(pair.client.connection.close_reason().is_none());
        assert!(pair.server.connection.close_reason().is_none());
    })
    .await
    .expect("loopback test exceeded five seconds");
}

async fn exchange(from: &mut NegotiatedSession, to: &mut NegotiatedSession) {
    let scope = from.outbound_scope();
    let epoch = from.initial_epoch;
    let first = from.control.next_sequence();
    for sequence in first..first + FRAMES {
        let frame = Frame::new(epoch, sequence, Message::SessionReady).with_scope(scope);
        write_frame(&from.connection, &mut from.control_streams.send, &frame)
            .await
            .expect("write a control frame");
    }
    let streams = &mut to.control_streams;
    for sequence in first..first + FRAMES {
        let frame = streams
            .reader
            .read_frame(&mut streams.recv)
            .await
            .expect("read a control frame");
        assert_eq!(
            (frame.sequence, frame.scope, frame.message),
            (sequence, scope, Message::SessionReady)
        );
    }

    let heartbeat = Frame::new(epoch, first + FRAMES, Message::Ping(first)).with_scope(scope);
    FrameWriter::default()
        .send_datagram(&from.connection, &heartbeat)
        .expect("send a heartbeat");
    let datagram = to
        .connection
        .read_datagram()
        .await
        .expect("receive a heartbeat");
    assert_eq!(
        decode_datagram(&datagram).expect("decode a heartbeat"),
        heartbeat
    );
}

struct Pair {
    client: NegotiatedSession,
    server: NegotiatedSession,
    _endpoints: [quinn::Endpoint; 2],
}

fn pin(identity: &DeviceIdentity) -> VerifiedPeer {
    VerifiedPeer::from_certificate_der(
        identity.certificate_der(),
        &identity.fingerprint().full_hex(),
    )
    .expect("generated identity has a full matching pin")
}

fn displays(id: u64) -> DisplayTopology {
    DisplayTopology::new(vec![DisplayDescription {
        id: DisplayId(id),
        name: "fixture".into(),
        native_width: 100,
        native_height: 100,
        logical_origin: Point::default(),
        logical_size: Point::new(100.0, 100.0),
        scale_factor: 1.0,
        is_primary: true,
        monitor: None,
    }])
    .expect("fixture topology is valid")
}

fn share_config<'a>(
    identity: &'a DeviceIdentity,
    peer: &'a VerifiedPeer,
    (local, remote): (Platform, Platform),
    topology: &'a DisplayTopology,
) -> HandshakeConfig<'a> {
    let features =
        Capabilities::new(Capabilities::RELATIVE_MOTION | Capabilities::DISPLAY_TOPOLOGY)
            .expect("fixture capabilities are known");
    HandshakeConfig::new(
        identity,
        peer,
        local,
        remote,
        features,
        features,
        topology,
        ControlPermissions::BOTH,
        SessionPurpose::Share,
    )
    .expect("share handshake configuration")
}

async fn negotiated_pair() -> Pair {
    let client_identity = DeviceIdentity::generate().expect("client identity");
    let server_identity = DeviceIdentity::generate().expect("server identity");
    let (client_pin, server_pin) = (pin(&client_identity), pin(&server_identity));
    let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let listener = quinn::Endpoint::server(
        SecureQuicConfig::server(&server_identity, &client_pin).expect("server TLS configuration"),
        loopback,
    )
    .expect("loopback server endpoint");
    let mut dialer = quinn::Endpoint::client(loopback).expect("loopback client endpoint");
    dialer.set_default_client_config(
        SecureQuicConfig::client(&client_identity, &server_pin).expect("client TLS configuration"),
    );
    let connecting = dialer
        .connect(
            listener.local_addr().expect("server address"),
            LOCAL_TLS_SERVER_NAME,
        )
        .expect("loopback connection");
    let (client_connection, server_connection) = tokio::join!(
        async { connecting.await.expect("client TLS connection") },
        async {
            listener
                .accept()
                .await
                .expect("incoming connection")
                .await
                .expect("server TLS connection")
        },
    );
    let (client_displays, server_displays) = (displays(1), displays(2));
    let (client, server) = tokio::join!(
        negotiate(
            client_connection,
            share_config(
                &client_identity,
                &server_pin,
                (Platform::Windows, Platform::MacOs),
                &client_displays,
            ),
        ),
        negotiate(
            server_connection,
            share_config(
                &server_identity,
                &client_pin,
                (Platform::MacOs, Platform::Windows),
                &server_displays,
            ),
        ),
    );
    Pair {
        client: client.expect("client share session"),
        server: server.expect("server share session"),
        _endpoints: [dialer, listener],
    }
}
