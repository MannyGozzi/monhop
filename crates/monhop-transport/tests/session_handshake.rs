use monhop_core::{DeviceId, DisplayId, Platform, Point};
use monhop_protocol::{
    Capabilities, DisplayDescription, DisplayTopology, Frame, Hello, Key, Message,
    PROTOCOL_VERSION, SessionEpoch, SessionPurpose, SessionSetup,
};
use monhop_transport::session_handshake::{
    HandshakeConfig, HandshakeError, device_id_from_fingerprint, validate_peer_handshake,
};

use monhop_transport::crypto::{DeviceIdentity, VerifiedPeer};

fn topology() -> DisplayTopology {
    DisplayTopology::new(vec![DisplayDescription {
        id: DisplayId(7),
        name: "Test display".to_owned(),
        native_width: 1_920,
        native_height: 1_080,
        logical_origin: Point::new(0.0, 0.0),
        logical_size: Point::new(1_920.0, 1_080.0),
        scale_factor: 1.0,
        is_primary: true,
        monitor: None,
    }])
    .expect("valid topology")
}

fn capabilities() -> Capabilities {
    Capabilities::new(Capabilities::DISPLAY_TOPOLOGY | Capabilities::RELATIVE_MOTION)
        .expect("known capability bits")
}

fn peer_pin(identity: &DeviceIdentity) -> VerifiedPeer {
    VerifiedPeer::from_certificate_der(
        identity.certificate_der(),
        &identity.fingerprint().full_hex(),
    )
    .expect("generated certificate has a matching fingerprint")
}

fn epoch() -> SessionEpoch {
    SessionEpoch::new(11).expect("nonzero epoch")
}

fn peer_frames(
    peer: &DeviceIdentity,
    epoch: SessionEpoch,
    topology: &DisplayTopology,
    source: DeviceId,
    platform: Platform,
    capabilities: Capabilities,
    purpose: SessionPurpose,
) -> [Frame; 3] {
    [
        Frame::new(
            epoch,
            0,
            Message::Hello(Hello {
                device_id: device_id_from_fingerprint(peer.fingerprint()),
                platform,
                protocol_version: PROTOCOL_VERSION,
                capabilities,
            }),
        ),
        Frame::new(epoch, 1, Message::DisplayTopology(topology.clone())),
        Frame::new(
            epoch,
            2,
            Message::SessionSetup(SessionSetup { source, purpose }),
        ),
    ]
}

#[test]
fn generated_certificates_bind_the_full_pin_and_stable_hello_id() {
    let local = DeviceIdentity::generate().expect("local identity");
    let peer = DeviceIdentity::generate().expect("peer identity");
    let other = DeviceIdentity::generate().expect("other identity");
    let peer_pin = peer_pin(&peer);
    let topology = topology();
    let source = device_id_from_fingerprint(local.fingerprint());
    let config = HandshakeConfig::new(
        &local,
        &peer_pin,
        Platform::Windows,
        Platform::MacOs,
        capabilities(),
        capabilities(),
        &topology,
        source,
        SessionPurpose::Share,
    )
    .expect("valid handshake configuration");
    let frames = peer_frames(
        &peer,
        epoch(),
        &topology,
        source,
        Platform::MacOs,
        capabilities(),
        SessionPurpose::Share,
    );

    let fingerprint = peer.fingerprint();
    assert_eq!(
        &device_id_from_fingerprint(fingerprint).0,
        &fingerprint.as_bytes()[..16]
    );
    assert!(validate_peer_handshake(&config, fingerprint, epoch(), &frames).is_ok());
    assert_eq!(
        validate_peer_handshake(&config, other.fingerprint(), epoch(), &frames),
        Err(HandshakeError::PeerCertificateMismatch)
    );
}

#[test]
fn model_rejects_duplicate_out_of_order_input_and_source_mismatch() {
    let local = DeviceIdentity::generate().expect("local identity");
    let peer = DeviceIdentity::generate().expect("peer identity");
    let peer_pin = peer_pin(&peer);
    let topology = topology();
    let source = device_id_from_fingerprint(peer.fingerprint());
    let config = HandshakeConfig::new(
        &local,
        &peer_pin,
        Platform::Windows,
        Platform::MacOs,
        capabilities(),
        capabilities(),
        &topology,
        source,
        SessionPurpose::Share,
    )
    .expect("valid handshake configuration");
    let frames = peer_frames(
        &peer,
        epoch(),
        &topology,
        source,
        Platform::MacOs,
        capabilities(),
        SessionPurpose::Share,
    );

    let duplicate = [
        frames[0].clone(),
        Frame::new(epoch(), 1, frames[0].message.clone()),
        frames[2].clone(),
    ];
    assert_eq!(
        validate_peer_handshake(&config, peer.fingerprint(), epoch(), &duplicate),
        Err(HandshakeError::UnexpectedFrame)
    );

    let input_first = [
        Frame::new(
            epoch(),
            0,
            Message::Key(Key {
                usage: monhop_core::HidUsage(0x04),
                is_down: true,
                repeat: false,
                modifiers: monhop_core::ModifierState::default(),
            }),
        ),
        frames[1].clone(),
        frames[2].clone(),
    ];
    assert_eq!(
        validate_peer_handshake(&config, peer.fingerprint(), epoch(), &input_first),
        Err(HandshakeError::UnexpectedFrame)
    );

    let mut out_of_order = frames.clone();
    out_of_order[1].sequence = 0;
    assert_eq!(
        validate_peer_handshake(&config, peer.fingerprint(), epoch(), &out_of_order),
        Err(HandshakeError::InvalidFrame)
    );

    let mut wrong_source = frames;
    wrong_source[2].message = Message::SessionSetup(SessionSetup {
        source: device_id_from_fingerprint(local.fingerprint()),
        purpose: SessionPurpose::Share,
    });
    assert_eq!(
        validate_peer_handshake(&config, peer.fingerprint(), epoch(), &wrong_source),
        Err(HandshakeError::SourceMismatch)
    );

    let frames = peer_frames(
        &peer,
        epoch(),
        &topology,
        source,
        Platform::MacOs,
        capabilities(),
        SessionPurpose::Share,
    );
    let mut wrong_purpose = frames;
    wrong_purpose[2].message = Message::SessionSetup(SessionSetup {
        source,
        purpose: SessionPurpose::Inspect,
    });
    assert_eq!(
        validate_peer_handshake(&config, peer.fingerprint(), epoch(), &wrong_purpose),
        Err(HandshakeError::PurposeMismatch)
    );
}

#[test]
fn model_requires_the_expected_platform_and_capabilities() {
    let local = DeviceIdentity::generate().expect("local identity");
    let peer = DeviceIdentity::generate().expect("peer identity");
    let peer_pin = peer_pin(&peer);
    let topology = topology();
    let source = device_id_from_fingerprint(local.fingerprint());
    let config = HandshakeConfig::new(
        &local,
        &peer_pin,
        Platform::Windows,
        Platform::MacOs,
        capabilities(),
        capabilities(),
        &topology,
        source,
        SessionPurpose::Share,
    )
    .expect("valid handshake configuration");

    let wrong_platform = peer_frames(
        &peer,
        epoch(),
        &topology,
        source,
        Platform::Windows,
        capabilities(),
        SessionPurpose::Share,
    );
    assert_eq!(
        validate_peer_handshake(&config, peer.fingerprint(), epoch(), &wrong_platform),
        Err(HandshakeError::PeerHelloMismatch)
    );

    let reduced_capabilities =
        Capabilities::new(Capabilities::DISPLAY_TOPOLOGY).expect("known capability bits");
    let wrong_capabilities = peer_frames(
        &peer,
        epoch(),
        &topology,
        source,
        Platform::MacOs,
        reduced_capabilities,
        SessionPurpose::Share,
    );
    assert_eq!(
        validate_peer_handshake(&config, peer.fingerprint(), epoch(), &wrong_capabilities),
        Err(HandshakeError::PeerHelloMismatch)
    );
}

#[test]
fn configuration_rejects_a_source_outside_the_authenticated_pair() {
    let local = DeviceIdentity::generate().expect("local identity");
    let peer = DeviceIdentity::generate().expect("peer identity");
    let peer_pin = peer_pin(&peer);
    let topology = topology();

    assert!(matches!(
        HandshakeConfig::new(
            &local,
            &peer_pin,
            Platform::Windows,
            Platform::MacOs,
            capabilities(),
            capabilities(),
            &topology,
            DeviceId([0; 16]),
            SessionPurpose::Share,
        ),
        Err(HandshakeError::InvalidConfiguration)
    ));
}
