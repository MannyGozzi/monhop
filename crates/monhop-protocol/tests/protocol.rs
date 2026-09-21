use monhop_core::{
    DeviceId, DisplayId, HidUsage, ModifierState, MonitorIdentity, MouseButton, Platform, Point,
};
use monhop_protocol::{
    Button, Capabilities, DatagramDecodeError, DecodeError, DeliveryClass, DisconnectCode,
    DisplayDescription, DisplayTopology, EncodeError, ErrorCode, Frame, Hello, Key,
    MAX_DISPLAY_NAME_BYTES, MAX_FRAME_LEN, MAX_LOGICAL_ORIGIN_ABS, MAX_LOGICAL_SIZE,
    MAX_NATIVE_DIMENSION, MAX_SCALE_FACTOR, Message, Motion, PROTOCOL_VERSION, RateLimitError,
    RateLimiter, Scroll, SequenceGate, SequenceGateError, SessionEpoch, SessionPurpose,
    SessionSetup, TopologyError, decode, decode_datagram,
};

fn epoch() -> SessionEpoch {
    SessionEpoch::new(7).expect("nonzero epoch")
}

fn frame(sequence: u64, message: Message) -> Frame {
    Frame::new(epoch(), sequence, message)
}

fn display_topology() -> DisplayTopology {
    DisplayTopology::new(vec![DisplayDescription {
        id: DisplayId(11),
        name: "Studio Display".to_owned(),
        native_width: 6_016,
        native_height: 3_384,
        logical_origin: Point::new(-1_920.0, 0.0),
        logical_size: Point::new(3_008.0, 1_692.0),
        scale_factor: 2.0,
        is_primary: true,
        monitor: None,
    }])
    .expect("valid topology")
}

fn all_messages() -> Vec<Message> {
    vec![
        Message::Hello(Hello {
            device_id: DeviceId([9; 16]),
            platform: Platform::MacOs,
            protocol_version: PROTOCOL_VERSION,
            capabilities: Capabilities::new(
                Capabilities::RELATIVE_MOTION | Capabilities::DISPLAY_TOPOLOGY,
            )
            .expect("known capability bits"),
        }),
        Message::SessionSetup(SessionSetup {
            source: DeviceId([8; 16]),
            purpose: SessionPurpose::Inspect,
        }),
        Message::DisplayTopology(display_topology()),
        Message::Motion(Motion::Absolute(Point::new(-50.25, 99.5))),
        Message::Motion(Motion::Relative(Point::new(1.5, -2.75))),
        Message::Button(Button {
            button: MouseButton::Forward,
            is_down: true,
        }),
        Message::Scroll(Scroll {
            horizontal: 0.25,
            vertical: -1.0,
        }),
        Message::Key(Key {
            usage: HidUsage(0x04),
            is_down: true,
            repeat: false,
            modifiers: ModifierState(ModifierState::LEFT_META | ModifierState::RIGHT_SHIFT),
        }),
        Message::Modifiers(ModifierState(ModifierState::LEFT_CONTROL)),
        Message::ActivateDisplay(DisplayId(12)),
        Message::ActivateDisplayAt {
            display_id: DisplayId(12),
            position: Point::new(-1_920.0, 1_080.0),
        },
        Message::ActivationAck(DisplayId(12)),
        Message::ReleaseAll,
        Message::ReleaseAck,
        Message::SessionReady,
        Message::Ping(44),
        Message::Pong(45),
        Message::Disconnect(DisconnectCode::TransportLost),
        Message::Error(ErrorCode::RateLimited),
    ]
}

#[test]
fn round_trips_every_message_with_one_reusable_buffer() {
    let mut buffer = Vec::with_capacity(MAX_FRAME_LEN);
    for (sequence, message) in all_messages().into_iter().enumerate() {
        let source = frame(sequence as u64, message);
        source.encode_into(&mut buffer).expect("encode valid frame");
        assert!(buffer.len() <= MAX_FRAME_LEN);
        assert_eq!(decode(&buffer).expect("decode encoded frame"), source);
    }
}

#[test]
fn only_absolute_motion_uses_the_lossy_datagram_class() {
    for message in all_messages() {
        let expected = if matches!(&message, Message::Motion(Motion::Absolute(_))) {
            DeliveryClass::MotionDatagram
        } else {
            DeliveryClass::Reliable
        };
        assert_eq!(message.delivery(), expected);
    }
}

#[test]
fn datagram_decoder_rejects_reliable_messages_and_relative_motion() {
    let mut encoded = Vec::new();
    let absolute = frame(1, Message::Motion(Motion::Absolute(Point::new(2.0, 3.0))));
    absolute
        .encode_into(&mut encoded)
        .expect("encode absolute motion");
    assert_eq!(
        decode_datagram(&encoded).expect("absolute datagram"),
        absolute
    );

    let relative = frame(2, Message::Motion(Motion::Relative(Point::new(2.0, 3.0))));
    relative
        .encode_into(&mut encoded)
        .expect("encode relative motion");
    assert_eq!(
        decode_datagram(&encoded),
        Err(DatagramDecodeError::ReliableMessage)
    );

    let release = frame(3, Message::ReleaseAll);
    release
        .encode_into(&mut encoded)
        .expect("encode release all");
    assert_eq!(
        decode_datagram(&encoded),
        Err(DatagramDecodeError::ReliableMessage)
    );
}

#[test]
fn decode_rejects_every_truncation_of_every_message() {
    let mut encoded = Vec::new();
    for (message_index, message) in all_messages().into_iter().enumerate() {
        frame(message_index as u64, message)
            .encode_into(&mut encoded)
            .expect("encode valid frame");
        for length in 0..encoded.len() {
            assert!(
                decode(&encoded[..length]).is_err(),
                "message {message_index}, length {length} must fail"
            );
        }
    }
}

#[test]
fn byte_corruption_is_deterministic_and_bounded() {
    let source = frame(8, Message::DisplayTopology(display_topology()));
    let mut encoded = Vec::new();
    source
        .encode_into(&mut encoded)
        .expect("encode valid frame");
    for index in 0..encoded.len() {
        let mut corrupted = encoded.clone();
        corrupted[index] ^= 0xFF;
        let first = decode(&corrupted);
        let second = decode(&corrupted);
        assert_eq!(
            first, second,
            "byte {index} produced a nondeterministic result"
        );
    }
}

#[test]
fn strict_header_and_body_guards_reject_malicious_values() {
    let mut encoded = Vec::new();
    frame(
        1,
        Message::Key(Key {
            usage: HidUsage(0x04),
            is_down: true,
            repeat: false,
            modifiers: ModifierState::default(),
        }),
    )
    .encode_into(&mut encoded)
    .expect("encode valid key frame");

    let mut reserved = encoded.clone();
    reserved[7] = 1;
    assert!(decode(&reserved).is_err());

    let mut invalid_boolean = encoded.clone();
    invalid_boolean[30] = 2;
    assert!(decode(&invalid_boolean).is_err());

    let mut invalid_usage = encoded.clone();
    invalid_usage[28] = 0;
    invalid_usage[29] = 0;
    assert!(decode(&invalid_usage).is_err());

    let mut invalid_modifiers = encoded;
    invalid_modifiers[32] = 1;
    assert!(decode(&invalid_modifiers).is_err());

    let mut trailing = Vec::new();
    frame(2, Message::ReleaseAll)
        .encode_into(&mut trailing)
        .expect("encode release frame");
    trailing.push(0);
    assert!(decode(&trailing).is_err());

    let oversized = vec![0; MAX_FRAME_LEN + 1];
    assert!(decode(&oversized).is_err());
}

#[test]
fn invalid_values_cannot_be_encoded() {
    let mut buffer = Vec::new();
    let invalid_key = frame(
        1,
        Message::Key(Key {
            usage: HidUsage(0),
            is_down: true,
            repeat: false,
            modifiers: ModifierState::default(),
        }),
    );
    assert_eq!(
        invalid_key.encode_into(&mut buffer),
        Err(EncodeError::InvalidKeyUsage)
    );

    let invalid_motion = frame(
        2,
        Message::Motion(Motion::Absolute(Point::new(f64::NAN, 0.0))),
    );
    assert_eq!(
        invalid_motion.encode_into(&mut buffer),
        Err(EncodeError::NonFiniteCoordinate)
    );

    let out_of_range_motion = frame(
        3,
        Message::Motion(Motion::Absolute(Point::new(
            MAX_LOGICAL_ORIGIN_ABS + 1.0,
            0.0,
        ))),
    );
    assert_eq!(
        out_of_range_motion.encode_into(&mut buffer),
        Err(EncodeError::CoordinateOutOfRange)
    );

    let invalid_topology = DisplayTopology::new(vec![DisplayDescription {
        id: DisplayId(1),
        name: "x".repeat(MAX_DISPLAY_NAME_BYTES + 1),
        native_width: 1,
        native_height: 1,
        logical_origin: Point::default(),
        logical_size: Point::new(1.0, 1.0),
        scale_factor: 1.0,
        is_primary: true,
        monitor: None,
    }]);
    assert!(invalid_topology.is_err());
}

#[test]
fn activation_protocol_v2_has_exact_bodies_and_reliable_delivery() {
    let messages = [
        (
            14_u8,
            24_u16,
            Message::SessionSetup(SessionSetup {
                source: DeviceId([3; 16]),
                purpose: SessionPurpose::Share,
            }),
        ),
        (
            15_u8,
            24_u16,
            Message::ActivateDisplayAt {
                display_id: DisplayId(12),
                position: Point::new(-1_920.0, 1_080.0),
            },
        ),
        (16_u8, 8_u16, Message::ActivationAck(DisplayId(12))),
    ];
    let mut encoded = Vec::new();
    for (sequence, (kind, body_len, message)) in messages.into_iter().enumerate() {
        let source = frame(sequence as u64, message);
        source.encode_into(&mut encoded).expect("encode v2 message");
        assert_eq!(&encoded[4..6], &PROTOCOL_VERSION.to_be_bytes());
        assert_eq!(encoded[6], kind);
        assert_eq!(u16::from_be_bytes([encoded[8], encoded[9]]), body_len,);
        assert_eq!(encoded.len(), 28 + usize::from(body_len));
        assert_eq!(source.delivery(), DeliveryClass::Reliable);
        assert_eq!(decode(&encoded), Ok(source));
    }
}

#[test]
fn session_setup_purpose_has_a_fixed_strict_body() {
    let source = frame(
        1,
        Message::SessionSetup(SessionSetup {
            source: DeviceId([3; 16]),
            purpose: SessionPurpose::Inspect,
        }),
    );
    let mut encoded = Vec::new();
    source
        .encode_into(&mut encoded)
        .expect("encode session setup");
    assert_eq!(u16::from_be_bytes([encoded[8], encoded[9]]), 24);
    assert_eq!(decode(&encoded), Ok(source));

    let trial = frame(
        2,
        Message::SessionSetup(SessionSetup {
            source: DeviceId([4; 16]),
            purpose: SessionPurpose::ControlledTrial,
        }),
    );
    trial
        .encode_into(&mut encoded)
        .expect("encode controlled trial setup");
    assert_eq!(encoded[44], 3);
    assert_eq!(decode(&encoded), Ok(trial));

    let configure = frame(
        3,
        Message::SessionSetup(SessionSetup {
            source: DeviceId([5; 16]),
            purpose: SessionPurpose::Configure,
        }),
    );
    configure
        .encode_into(&mut encoded)
        .expect("encode configure setup");
    assert_eq!(encoded[44], 4);
    assert_eq!(decode(&encoded), Ok(configure));

    let setup = frame(
        4,
        Message::SessionSetup(SessionSetup {
            source: DeviceId([6; 16]),
            purpose: SessionPurpose::Setup,
        }),
    );
    setup.encode_into(&mut encoded).expect("encode setup link");
    assert_eq!(encoded[44], 5);
    assert_eq!(decode(&encoded), Ok(setup));

    let mut invalid_purpose = encoded.clone();
    invalid_purpose[44] = 6;
    assert_eq!(decode(&invalid_purpose), Err(DecodeError::InvalidEnum));

    let mut nonzero_reserved = encoded;
    nonzero_reserved[45] = 1;
    assert_eq!(
        decode(&nonzero_reserved),
        Err(DecodeError::NonZeroReservedField)
    );
}

#[test]
fn same_geometry_ignores_only_names_and_enumeration_order() {
    let first = DisplayTopology::new(vec![
        DisplayDescription {
            id: DisplayId(1),
            name: "Left".into(),
            native_width: 1_920,
            native_height: 1_080,
            logical_origin: Point::new(-1_920.0, 0.0),
            logical_size: Point::new(1_920.0, 1_080.0),
            scale_factor: 1.0,
            is_primary: false,
            monitor: None,
        },
        DisplayDescription {
            id: DisplayId(2),
            name: "Primary".into(),
            native_width: 2_560,
            native_height: 1_440,
            logical_origin: Point::default(),
            logical_size: Point::new(2_560.0, 1_440.0),
            scale_factor: 2.0,
            is_primary: true,
            monitor: None,
        },
    ])
    .expect("valid topology");
    let renamed_reordered = DisplayTopology::new(vec![
        DisplayDescription {
            name: "Anything".into(),
            ..first.displays()[1].clone()
        },
        DisplayDescription {
            name: "Different".into(),
            ..first.displays()[0].clone()
        },
    ])
    .expect("valid topology");
    assert!(first.same_geometry(&renamed_reordered));

    for changed in [
        changed_geometry(&first, |display| display.id = DisplayId(3)),
        changed_geometry(&first, |display| display.native_width = 2_000),
        changed_geometry(&first, |display| display.native_height = 1_000),
        changed_geometry(&first, |display| display.logical_origin.x = -1_000.0),
        changed_geometry(&first, |display| display.logical_size.y = 1_000.0),
        changed_geometry(&first, |display| display.scale_factor = 1.5),
        changed_geometry(&first, |display| display.is_primary = false),
    ] {
        assert!(!first.same_geometry(&changed));
    }

    let missing = DisplayTopology::new(vec![first.displays()[1].clone()]).expect("valid topology");
    assert!(!first.same_geometry(&missing));
    let mut added = first.displays().to_vec();
    added.push(DisplayDescription {
        id: DisplayId(3),
        name: "Extra".into(),
        native_width: 1,
        native_height: 1,
        logical_origin: Point::new(3_000.0, 0.0),
        logical_size: Point::new(1.0, 1.0),
        scale_factor: 1.0,
        is_primary: false,
        monitor: None,
    });
    let added = DisplayTopology::new(added).expect("valid topology");
    assert!(!first.same_geometry(&added));
}

fn changed_geometry(
    original: &DisplayTopology,
    change: impl FnOnce(&mut DisplayDescription),
) -> DisplayTopology {
    let mut displays = original.displays().to_vec();
    change(&mut displays[1]);
    if !displays.iter().any(|display| display.is_primary) {
        displays[0].is_primary = true;
    }
    DisplayTopology::new(displays).expect("changed topology remains structurally valid")
}

#[test]
fn activation_protocol_rejects_nonfinite_out_of_range_and_malformed_frames() {
    let mut encoded = Vec::new();
    let source = frame(
        1,
        Message::ActivateDisplayAt {
            display_id: DisplayId(12),
            position: Point::new(-1_920.0, 1_080.0),
        },
    );
    source
        .encode_into(&mut encoded)
        .expect("encode activation frame");

    let nonfinite = frame(
        2,
        Message::ActivateDisplayAt {
            display_id: DisplayId(12),
            position: Point::new(f64::NAN, 0.0),
        },
    );
    assert_eq!(
        nonfinite.encode_into(&mut Vec::new()),
        Err(EncodeError::NonFiniteCoordinate)
    );

    let out_of_range = frame(
        3,
        Message::ActivateDisplayAt {
            display_id: DisplayId(12),
            position: Point::new(MAX_LOGICAL_ORIGIN_ABS + 1.0, 0.0),
        },
    );
    assert_eq!(
        out_of_range.encode_into(&mut Vec::new()),
        Err(EncodeError::CoordinateOutOfRange)
    );

    let mut malformed_position = encoded.clone();
    malformed_position[36..44].copy_from_slice(&f64::NAN.to_bits().to_be_bytes());
    assert_eq!(
        decode(&malformed_position),
        Err(DecodeError::NonFiniteCoordinate)
    );

    let mut malformed_range = encoded.clone();
    malformed_range[36..44]
        .copy_from_slice(&(MAX_LOGICAL_ORIGIN_ABS + 1.0).to_bits().to_be_bytes());
    assert_eq!(
        decode(&malformed_range),
        Err(DecodeError::CoordinateOutOfRange)
    );

    let mut wrong_length = encoded.clone();
    wrong_length[8..10].copy_from_slice(&23_u16.to_be_bytes());
    assert_eq!(decode(&wrong_length), Err(DecodeError::InvalidLength));

    let mut trailing = encoded.clone();
    trailing.push(0);
    trailing[8..10].copy_from_slice(&25_u16.to_be_bytes());
    assert_eq!(decode(&trailing), Err(DecodeError::TrailingBytes));

    let mut old_version = encoded.clone();
    old_version[4..6].copy_from_slice(&1_u16.to_be_bytes());
    assert_eq!(decode(&old_version), Err(DecodeError::UnsupportedVersion));

    let mut unknown_kind = encoded;
    unknown_kind[6] = 19;
    assert_eq!(decode(&unknown_kind), Err(DecodeError::UnknownMessageType));
}

#[test]
fn topology_rejects_overflow_and_implausible_geometry() {
    let overflow = DisplayTopology::new(vec![DisplayDescription {
        id: DisplayId(1),
        name: "overflow".to_owned(),
        native_width: 1,
        native_height: 1,
        logical_origin: Point::new(f64::MAX, 0.0),
        logical_size: Point::new(f64::MAX, 1.0),
        scale_factor: 1.0,
        is_primary: true,
        monitor: None,
    }]);
    assert_eq!(overflow, Err(TopologyError::GeometryOverflow));

    let native_size = DisplayTopology::new(vec![DisplayDescription {
        id: DisplayId(2),
        name: "oversized".to_owned(),
        native_width: MAX_NATIVE_DIMENSION + 1,
        native_height: 1,
        logical_origin: Point::default(),
        logical_size: Point::new(1.0, 1.0),
        scale_factor: 1.0,
        is_primary: true,
        monitor: None,
    }]);
    assert_eq!(native_size, Err(TopologyError::InvalidNativeSize));

    let logical_size = DisplayTopology::new(vec![DisplayDescription {
        id: DisplayId(3),
        name: "logical".to_owned(),
        native_width: 1,
        native_height: 1,
        logical_origin: Point::default(),
        logical_size: Point::new(MAX_LOGICAL_SIZE + 1.0, 1.0),
        scale_factor: 1.0,
        is_primary: true,
        monitor: None,
    }]);
    assert_eq!(logical_size, Err(TopologyError::InvalidLogicalSize));

    let scale = DisplayTopology::new(vec![DisplayDescription {
        id: DisplayId(4),
        name: "scale".to_owned(),
        native_width: 1,
        native_height: 1,
        logical_origin: Point::default(),
        logical_size: Point::new(1.0, 1.0),
        scale_factor: MAX_SCALE_FACTOR + 0.1,
        is_primary: true,
        monitor: None,
    }]);
    assert_eq!(scale, Err(TopologyError::InvalidScaleFactor));
}

#[test]
fn sequence_gate_requires_explicit_monotonic_epochs_and_drops_stale_packets() {
    let mut gate = SequenceGate::default();
    let reliable_one = frame(1, Message::ReleaseAll);
    assert_eq!(
        gate.accept(&reliable_one),
        Err(SequenceGateError::EpochNotActivated)
    );
    gate.activate_epoch(epoch()).expect("activate first epoch");
    assert!(gate.accept(&reliable_one).is_ok());
    assert_eq!(
        gate.accept(&reliable_one),
        Err(SequenceGateError::StaleOrReplay)
    );

    let motion_one = frame(1, Message::Motion(Motion::Absolute(Point::new(1.0, 0.0))));
    assert!(gate.accept(&motion_one).is_ok());
    assert_eq!(
        gate.accept(&motion_one),
        Err(SequenceGateError::StaleOrReplay)
    );

    let next_epoch = SessionEpoch::new(8).expect("nonzero epoch");
    gate.activate_epoch(next_epoch)
        .expect("activate newer epoch");
    assert_eq!(
        gate.accept(&reliable_one),
        Err(SequenceGateError::EpochMismatch)
    );
    assert_eq!(
        gate.activate_epoch(epoch()),
        Err(SequenceGateError::EpochNotNew)
    );
}

#[test]
fn rate_limiter_is_bounded_and_uses_monotonic_caller_time() {
    let mut limiter = RateLimiter::new(2).expect("valid limiter");
    assert!(limiter.allow_at(100).is_ok());
    assert!(limiter.allow_at(100).is_ok());
    assert_eq!(limiter.allow_at(100), Err(RateLimitError::Exceeded));
    assert!(limiter.allow_at(600).is_ok());
    assert_eq!(
        limiter.allow_at(599),
        Err(RateLimitError::ClockMovedBackwards)
    );
}

#[test]
fn readiness_requires_the_current_protocol_and_an_empty_reliable_body() {
    let ready = frame(3, Message::SessionReady);
    let mut bytes = Vec::new();
    ready.encode_into(&mut bytes).unwrap();
    assert_eq!(PROTOCOL_VERSION, 8);
    assert_eq!(ready.delivery(), DeliveryClass::Reliable);
    assert_eq!(decode(&bytes), Ok(ready));
    for version in [1_u16, 2, 3, 4, 5, 6, 7] {
        let mut old = bytes.clone();
        old[4..6].copy_from_slice(&version.to_be_bytes());
        assert_eq!(decode(&old), Err(DecodeError::UnsupportedVersion));
    }
    bytes.push(0);
    bytes[8..10].copy_from_slice(&1_u16.to_be_bytes());
    assert_eq!(decode(&bytes), Err(DecodeError::TrailingBytes));
}

#[test]
fn display_topology_carries_the_monitor_identity() {
    let mut displays = display_topology().into_displays();
    displays[0].monitor = MonitorIdentity::new(0x10ac, 0x0a13, 0x4c4e_3031);
    displays.push(DisplayDescription {
        id: DisplayId(12),
        name: "Built-in".to_owned(),
        native_width: 2_880,
        native_height: 1_800,
        logical_origin: Point::new(1_088.0, 0.0),
        logical_size: Point::new(1_440.0, 900.0),
        scale_factor: 2.0,
        is_primary: false,
        monitor: None,
    });
    let topology = DisplayTopology::new(displays).expect("valid topology");
    let source = frame(9, Message::DisplayTopology(topology.clone()));
    let mut encoded = Vec::new();
    source.encode_into(&mut encoded).expect("encode topology");
    let decoded = decode(&encoded).expect("decode topology");
    assert_eq!(decoded, source);
    let Message::DisplayTopology(decoded) = decoded.message else {
        panic!("topology expected");
    };
    assert_eq!(
        decoded.displays()[0].monitor.map(|monitor| (
            monitor.vendor,
            monitor.product,
            monitor.serial
        )),
        Some((0x10ac, 0x0a13, 0x4c4e_3031))
    );
    assert_eq!(decoded.displays()[1].monitor, None);
    assert_eq!(MonitorIdentity::new(0, 7, 1), None);
    assert_eq!(MonitorIdentity::new(7, 0, 1), None);
}
