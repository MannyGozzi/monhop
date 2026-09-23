use std::{sync::Mutex, time::Duration};

use monhop_core::{
    DestinationModifier, DestinationModifierKey, DeviceId, Display, DisplayId, Edge, EdgeLink,
    EmergencyEscape, FailureReason, FloorOwner, FloorSnapshot, FloorState, HeldInput, HidUsage,
    InputAction, LogicalSize, Machine, MouseButton, NativeSessionClaim, NativeSize, NormalizedSpan,
    OwnershipCommand, OwnershipError, OwnershipState, Platform, Point, PointerOwnership,
    PointerTarget, SharedFloor, Topology, TopologyError, TransitionAcknowledgement,
    map_modifier_for_destination,
};

// The native claim is one process-wide flag; tests that touch it run one at a time.
static CLAIM_TESTS: Mutex<()> = Mutex::new(());

fn device(value: u8) -> DeviceId {
    DeviceId([value; 16])
}

fn display(
    id: u64,
    machine: DeviceId,
    origin: Point,
    logical_size: LogicalSize,
    native_size: NativeSize,
    scale_factor: f64,
) -> Display {
    Display::new(
        DisplayId(id),
        machine,
        format!("display-{id}"),
        native_size,
        logical_size,
        origin,
        scale_factor,
        Some(60.0),
        id == 1,
    )
}

fn machines() -> Vec<Machine> {
    vec![
        Machine::new(device(1), Platform::Windows),
        Machine::new(device(2), Platform::MacOs),
    ]
}

fn full_span() -> NormalizedSpan {
    NormalizedSpan::new(0.0, 1.0).expect("full span is valid")
}

#[test]
fn mixed_scale_negative_origin_crossing_maps_proportionally() {
    let source = display(
        1,
        device(1),
        Point::new(-1920.0, -540.0),
        LogicalSize::new(1920.0, 1080.0),
        NativeSize::new(3840, 2160),
        2.0,
    );
    let destination = display(
        2,
        device(2),
        Point::new(0.0, -100.0),
        LogicalSize::new(1500.0, 975.0),
        NativeSize::new(3000, 1950),
        2.0,
    );
    let link = EdgeLink::new(
        source.id,
        Edge::Right,
        NormalizedSpan::new(0.25, 0.75).expect("span is valid"),
        destination.id,
        Edge::Left,
        NormalizedSpan::new(0.1, 0.9).expect("span is valid"),
        2.0,
    )
    .expect("link is valid");
    let topology = Topology::new(machines(), vec![source, destination], vec![link])
        .expect("topology is valid");

    let transition = topology
        .transition_for_motion(DisplayId(1), Point::new(-1.0, 0.0), Point::new(3.0, 0.0))
        .expect("motion is valid")
        .expect("motion crosses the configured edge");

    assert_eq!(transition.crossing_point, Point::new(0.0, 0.0));
    assert_eq!(transition.entry_point, Point::new(2.0, 387.5));
}

#[test]
fn gradual_motion_through_hysteresis_transitions_after_the_dead_zone() {
    let source = display(
        1,
        device(1),
        Point::new(0.0, 0.0),
        LogicalSize::new(100.0, 100.0),
        NativeSize::new(100, 100),
        1.0,
    );
    let destination = display(
        2,
        device(2),
        Point::new(100.0, 0.0),
        LogicalSize::new(100.0, 100.0),
        NativeSize::new(100, 100),
        1.0,
    );
    let link = EdgeLink::new(
        source.id,
        Edge::Right,
        full_span(),
        destination.id,
        Edge::Left,
        full_span(),
        2.0,
    )
    .expect("link is valid");
    let topology = Topology::new(machines(), vec![source, destination], vec![link])
        .expect("topology is valid");

    assert_eq!(
        topology
            .transition_for_motion(
                DisplayId(1),
                Point::new(99.0, 50.0),
                Point::new(101.0, 50.0)
            )
            .expect("motion is valid"),
        None
    );
    let transition = topology
        .transition_for_motion(
            DisplayId(1),
            Point::new(101.0, 50.0),
            Point::new(103.0, 50.0),
        )
        .expect("motion is valid")
        .expect("motion exits the dead zone");
    assert_eq!(transition.crossing_point, Point::new(100.0, 50.0));
    assert_eq!(transition.entry_point, Point::new(102.0, 50.0));

    let diagonal = topology
        .transition_for_motion(
            DisplayId(1),
            Point::new(101.0, 90.0),
            Point::new(103.0, 94.0),
        )
        .expect("finite diagonal motion")
        .expect("diagonal movement exits the dead zone");
    assert_eq!(diagonal.crossing_point, Point::new(100.0, 92.0));
    assert_eq!(diagonal.entry_point, Point::new(102.0, 92.0));
}

#[test]
fn portrait_top_edge_maps_into_a_bottom_edge() {
    let source = display(
        1,
        device(1),
        Point::new(-1080.0, 0.0),
        LogicalSize::new(1080.0, 1920.0),
        NativeSize::new(1080, 1920),
        1.0,
    );
    let destination = display(
        2,
        device(2),
        Point::new(-1920.0, -1080.0),
        LogicalSize::new(1920.0, 1080.0),
        NativeSize::new(3840, 2160),
        2.0,
    );
    let link = EdgeLink::new(
        source.id,
        Edge::Top,
        full_span(),
        destination.id,
        Edge::Bottom,
        full_span(),
        4.0,
    )
    .expect("link is valid");
    let topology = Topology::new(machines(), vec![source, destination], vec![link])
        .expect("topology is valid");

    let transition = topology
        .transition_for_motion(
            DisplayId(1),
            Point::new(-540.0, 5.0),
            Point::new(-540.0, -5.0),
        )
        .expect("motion is valid")
        .expect("motion crosses the configured edge");

    assert_eq!(transition.entry_point, Point::new(-960.0, -4.0));
}

#[test]
fn topology_rejects_invalid_and_ambiguous_configuration() {
    let source = display(
        1,
        device(1),
        Point::new(0.0, 0.0),
        LogicalSize::new(100.0, 100.0),
        NativeSize::new(100, 100),
        1.0,
    );
    let destination = display(
        2,
        device(2),
        Point::new(100.0, 0.0),
        LogicalSize::new(100.0, 100.0),
        NativeSize::new(100, 100),
        1.0,
    );
    assert!(matches!(
        NormalizedSpan::new(0.5, 0.5),
        Err(TopologyError::InvalidNormalizedSpan { .. })
    ));
    assert!(matches!(
        EdgeLink::new(
            source.id,
            Edge::Right,
            full_span(),
            source.id,
            Edge::Left,
            full_span(),
            1.0
        ),
        Err(TopologyError::SameDisplayLink { .. })
    ));
    assert!(matches!(
        EdgeLink::new(
            source.id,
            Edge::Right,
            full_span(),
            destination.id,
            Edge::Left,
            full_span(),
            0.0
        ),
        Err(TopologyError::InvalidHysteresis { .. })
    ));

    let unknown_link = EdgeLink::new(
        source.id,
        Edge::Right,
        full_span(),
        DisplayId(99),
        Edge::Left,
        full_span(),
        1.0,
    )
    .expect("link shape is valid");
    assert!(matches!(
        Topology::new(
            machines(),
            vec![source.clone(), destination.clone()],
            vec![unknown_link]
        ),
        Err(TopologyError::UnknownDisplay {
            display: DisplayId(99)
        })
    ));

    let first = EdgeLink::new(
        source.id,
        Edge::Right,
        NormalizedSpan::new(0.0, 0.5).expect("span is valid"),
        destination.id,
        Edge::Left,
        full_span(),
        1.0,
    )
    .expect("link is valid");
    let second = EdgeLink::new(
        source.id,
        Edge::Right,
        NormalizedSpan::new(0.5, 1.0).expect("span is valid"),
        destination.id,
        Edge::Left,
        full_span(),
        1.0,
    )
    .expect("link is valid");
    assert!(
        Topology::new(
            machines(),
            vec![source.clone(), destination.clone()],
            vec![first, second]
        )
        .is_ok()
    );
    let mut overlapping = second;
    overlapping.from_span = NormalizedSpan::new(0.49, 1.0).expect("valid span");
    assert!(matches!(
        Topology::new(
            machines(),
            vec![source.clone(), destination.clone()],
            vec![first, overlapping]
        ),
        Err(TopologyError::OverlappingSourceLinks { .. })
    ));

    let invalid_display = display(
        3,
        device(1),
        Point::new(f64::NAN, 0.0),
        LogicalSize::new(100.0, 100.0),
        NativeSize::new(100, 100),
        1.0,
    );
    assert!(matches!(
        Topology::new(machines(), vec![invalid_display], Vec::new()),
        Err(TopologyError::InvalidOrigin { .. })
    ));

    let invalid_native_size = display(
        3,
        device(1),
        Point::new(0.0, 0.0),
        LogicalSize::new(100.0, 100.0),
        NativeSize::new(0, 100),
        1.0,
    );
    assert!(matches!(
        Topology::new(machines(), vec![invalid_native_size], Vec::new()),
        Err(TopologyError::InvalidNativeSize { .. })
    ));

    let invalid_logical_size = display(
        3,
        device(1),
        Point::new(0.0, 0.0),
        LogicalSize::new(0.0, 100.0),
        NativeSize::new(100, 100),
        1.0,
    );
    assert!(matches!(
        Topology::new(machines(), vec![invalid_logical_size], Vec::new()),
        Err(TopologyError::InvalidLogicalSize { .. })
    ));

    let bypassed_constructor = EdgeLink {
        from_display: source.id,
        from_edge: Edge::Right,
        from_span: full_span(),
        to_display: destination.id,
        to_edge: Edge::Left,
        to_span: full_span(),
        hysteresis: f64::NAN,
    };
    assert!(matches!(
        Topology::new(
            machines(),
            vec![source.clone(), destination.clone()],
            vec![bypassed_constructor]
        ),
        Err(TopologyError::InvalidHysteresis { .. })
    ));

    let bypassed_negative_hysteresis = EdgeLink {
        from_display: source.id,
        from_edge: Edge::Right,
        from_span: full_span(),
        to_display: destination.id,
        to_edge: Edge::Left,
        to_span: full_span(),
        hysteresis: -1.0,
    };
    assert!(matches!(
        Topology::new(
            machines(),
            vec![source.clone(), destination.clone()],
            vec![bypassed_negative_hysteresis]
        ),
        Err(TopologyError::InvalidHysteresis { .. })
    ));

    let bypassed_same_display = EdgeLink {
        from_display: source.id,
        from_edge: Edge::Right,
        from_span: full_span(),
        to_display: source.id,
        to_edge: Edge::Left,
        to_span: full_span(),
        hysteresis: 1.0,
    };
    assert!(matches!(
        Topology::new(
            machines(),
            vec![source.clone(), destination.clone()],
            vec![bypassed_same_display]
        ),
        Err(TopologyError::SameDisplayLink { .. })
    ));

    let bypassed_zero_hysteresis = EdgeLink {
        from_display: source.id,
        from_edge: Edge::Right,
        from_span: full_span(),
        to_display: destination.id,
        to_edge: Edge::Left,
        to_span: full_span(),
        hysteresis: 0.0,
    };
    assert!(matches!(
        Topology::new(
            machines(),
            vec![source.clone(), destination.clone()],
            vec![bypassed_zero_hysteresis]
        ),
        Err(TopologyError::InvalidHysteresis { .. })
    ));

    let topology = Topology::new(machines(), vec![source, destination], Vec::new())
        .expect("topology is valid");
    assert!(matches!(
        topology.transition_for_motion(
            DisplayId(1),
            Point::new(f64::NAN, 0.0),
            Point::new(1.0, 0.0)
        ),
        Err(TopologyError::NonFiniteMotionPoint)
    ));
}

#[test]
fn endpoint_mapping_enters_inside_destination_bounds() {
    let source = display(
        1,
        device(1),
        Point::new(0.0, 0.0),
        LogicalSize::new(100.0, 100.0),
        NativeSize::new(100, 100),
        1.0,
    );
    let destination = display(
        2,
        device(2),
        Point::new(100.0, 0.0),
        LogicalSize::new(100.0, 100.0),
        NativeSize::new(100, 100),
        1.0,
    );
    let link = EdgeLink::new(
        source.id,
        Edge::Right,
        full_span(),
        destination.id,
        Edge::Left,
        full_span(),
        1.0,
    )
    .expect("link is valid");
    let topology = Topology::new(machines(), vec![source, destination], vec![link])
        .expect("topology is valid");

    let transition = topology
        .transition_for_motion(
            DisplayId(1),
            Point::new(99.0, 100.0),
            Point::new(102.0, 100.0),
        )
        .expect("motion is valid")
        .expect("motion crosses the configured edge");
    assert!(transition.entry_point.y < 100.0);
    assert!(transition.entry_point.y >= 0.0);

    let vertical_source = display(
        3,
        device(1),
        Point::new(0.0, 0.0),
        LogicalSize::new(100.0, 100.0),
        NativeSize::new(100, 100),
        1.0,
    );
    let vertical_destination = display(
        4,
        device(2),
        Point::new(0.0, -100.0),
        LogicalSize::new(100.0, 100.0),
        NativeSize::new(100, 100),
        1.0,
    );
    let vertical_link = EdgeLink::new(
        vertical_source.id,
        Edge::Top,
        full_span(),
        vertical_destination.id,
        Edge::Bottom,
        full_span(),
        1.0,
    )
    .expect("link is valid");
    let vertical_topology = Topology::new(
        machines(),
        vec![vertical_source, vertical_destination],
        vec![vertical_link],
    )
    .expect("topology is valid");
    let vertical_transition = vertical_topology
        .transition_for_motion(
            DisplayId(3),
            Point::new(100.0, 1.0),
            Point::new(100.0, -2.0),
        )
        .expect("motion is valid")
        .expect("motion crosses the configured edge");
    assert!(vertical_transition.entry_point.x < 100.0);
    assert!(vertical_transition.entry_point.x >= 0.0);
}

#[test]
fn semantic_mapping_preserves_sides_and_uses_destination_native_names() {
    assert_eq!(
        map_modifier_for_destination(HidUsage(0xE2), Platform::MacOs),
        Some(DestinationModifier {
            side: monhop_core::ModifierSide::Left,
            key: DestinationModifierKey::Option,
        })
    );
    assert_eq!(
        map_modifier_for_destination(HidUsage(0xE3), Platform::MacOs),
        Some(DestinationModifier {
            side: monhop_core::ModifierSide::Left,
            key: DestinationModifierKey::Command,
        })
    );
    assert_eq!(
        map_modifier_for_destination(HidUsage(0xE6), Platform::Windows),
        Some(DestinationModifier {
            side: monhop_core::ModifierSide::Right,
            key: DestinationModifierKey::Alt,
        })
    );
    assert_eq!(
        map_modifier_for_destination(HidUsage(0xE7), Platform::Windows),
        Some(DestinationModifier {
            side: monhop_core::ModifierSide::Right,
            key: DestinationModifierKey::Super,
        })
    );
}

#[test]
fn transition_releases_outgoing_and_only_transfers_modifiers_and_buttons() {
    let mut held = HeldInput::default();
    let letter = HidUsage(0x04);
    let shift = HidUsage(0xE1);
    held.key_down(letter, Platform::Windows)
        .expect("letter is valid");
    held.key_down(shift, Platform::Windows)
        .expect("shift is valid");
    held.button_down(MouseButton::Left);

    let plan = held.transition(Platform::Windows, Platform::MacOs);

    assert_eq!(
        plan.release_outgoing,
        vec![
            InputAction::Key {
                usage: letter,
                pressed: false,
                repeat: false,
                modifier: None,
            },
            InputAction::Key {
                usage: shift,
                pressed: false,
                repeat: false,
                modifier: Some(DestinationModifier {
                    side: monhop_core::ModifierSide::Left,
                    key: DestinationModifierKey::Shift,
                }),
            },
            InputAction::Button {
                button: MouseButton::Left,
                pressed: false,
            },
        ]
    );
    assert_eq!(
        plan.transfer_incoming,
        vec![
            InputAction::Key {
                usage: shift,
                pressed: true,
                repeat: false,
                modifier: Some(DestinationModifier {
                    side: monhop_core::ModifierSide::Left,
                    key: DestinationModifierKey::Shift,
                }),
            },
            InputAction::Button {
                button: MouseButton::Left,
                pressed: true,
            },
        ]
    );
    assert!(!held.is_delivered_key(letter));
    assert!(held.is_delivered_key(shift));
}

#[test]
fn held_ordinary_key_release_after_transition_is_suppressed() {
    let mut held = HeldInput::default();
    let letter = HidUsage(0x04);
    held.key_down(letter, Platform::Windows)
        .expect("letter is valid");
    held.transition(Platform::Windows, Platform::MacOs);

    assert_eq!(
        held.key_up(letter, Platform::MacOs)
            .expect("letter is valid"),
        None
    );
}

#[test]
fn held_windows_key_becomes_command_and_failure_releases_held_state() {
    let mut held = HeldInput::default();
    let windows_key = HidUsage(0xE3);
    let control = HidUsage(0xE0);
    held.key_down(windows_key, Platform::Windows)
        .expect("windows key is valid");
    let transition = held.transition(Platform::Windows, Platform::MacOs);
    assert!(transition.transfer_incoming.contains(&InputAction::Key {
        usage: windows_key,
        pressed: true,
        repeat: false,
        modifier: Some(DestinationModifier {
            side: monhop_core::ModifierSide::Left,
            key: DestinationModifierKey::Command,
        }),
    }));

    held.key_up(windows_key, Platform::MacOs)
        .expect("windows key is valid");
    held.key_down(control, Platform::MacOs)
        .expect("control is valid");
    held.button_down(MouseButton::Left);
    let releases = held.release_destination(Platform::MacOs);
    assert!(releases.contains(&InputAction::Key {
        usage: control,
        pressed: false,
        repeat: false,
        modifier: Some(DestinationModifier {
            side: monhop_core::ModifierSide::Left,
            key: DestinationModifierKey::Control,
        }),
    }));
    assert!(releases.contains(&InputAction::Button {
        button: MouseButton::Left,
        pressed: false,
    }));
    assert!(held.is_key_pressed(control));
    assert!(held.is_button_pressed(MouseButton::Left));
    assert!(!held.is_delivered_key(control));
    assert!(!held.is_delivered_button(MouseButton::Left));
}

#[test]
fn emergency_escape_is_monotonic_and_one_shot() {
    let mut held = HeldInput::default();
    for usage in [HidUsage(0xE0), HidUsage(0xE4), HidUsage(0x29)] {
        held.key_down(usage, Platform::MacOs)
            .expect("emergency key is valid");
    }
    let mut escape = EmergencyEscape::default();

    assert!(!escape.evaluate(&held, Duration::ZERO));
    assert!(!escape.evaluate(&held, Duration::from_millis(1999)));
    assert!(escape.evaluate(&held, Duration::from_secs(2)));
    assert!(!escape.evaluate(&held, Duration::from_secs(3)));
    held.key_up(HidUsage(0x29), Platform::MacOs)
        .expect("escape is valid");
    assert!(!escape.evaluate(&held, Duration::from_secs(3)));
}

#[test]
fn ownership_rejects_unsolicited_and_stale_acknowledgements_then_fails_local() {
    let local = PointerTarget::new(device(1), DisplayId(1));
    let remote = PointerTarget::new(device(2), DisplayId(2));
    let mut ownership = PointerOwnership::new(device(1), local).expect("local target is valid");

    assert_eq!(
        ownership.acknowledge_transition(TransitionAcknowledgement {
            target: remote,
            epoch: 1,
        }),
        Err(OwnershipError::UnsolicitedAcknowledgement)
    );
    let begin = ownership
        .begin_transition(remote)
        .expect("transition begins");
    assert_eq!(
        begin,
        OwnershipCommand::BeginTransition {
            from: local,
            to: remote,
            epoch: 1,
        }
    );
    assert_eq!(ownership.active_destination(), local);
    assert_eq!(
        ownership.acknowledge_transition(TransitionAcknowledgement {
            target: local,
            epoch: 1,
        }),
        Err(OwnershipError::StaleAcknowledgement)
    );
    assert_eq!(
        ownership
            .acknowledge_transition(TransitionAcknowledgement {
                target: remote,
                epoch: 1,
            })
            .expect("matching acknowledgement activates destination"),
        OwnershipCommand::Activate {
            target: remote,
            epoch: 1,
        }
    );
    assert!(matches!(
        ownership.state(),
        OwnershipState::RemoteActive { target, epoch: 1 } if target == remote
    ));
    assert_eq!(
        ownership.acknowledge_transition(TransitionAcknowledgement {
            target: remote,
            epoch: 1,
        }),
        Err(OwnershipError::UnsolicitedAcknowledgement)
    );

    let return_begin = ownership
        .begin_transition(local)
        .expect("return transition begins");
    assert_eq!(
        return_begin,
        OwnershipCommand::BeginTransition {
            from: remote,
            to: local,
            epoch: 2,
        }
    );
    assert_eq!(
        ownership.acknowledge_transition(TransitionAcknowledgement {
            target: remote,
            epoch: 1,
        }),
        Err(OwnershipError::StaleAcknowledgement)
    );
    ownership
        .acknowledge_transition(TransitionAcknowledgement {
            target: local,
            epoch: 2,
        })
        .expect("current acknowledgement returns locally");

    ownership
        .begin_transition(remote)
        .expect("second remote transition begins");
    ownership
        .acknowledge_transition(TransitionAcknowledgement {
            target: remote,
            epoch: 3,
        })
        .expect("second remote transition is acknowledged");

    let plan = ownership.on_timeout().expect("timeout fails local");
    assert_eq!(plan.reason, FailureReason::Timeout);
    assert_eq!(plan.remote_to_release, Some(remote));
    assert_eq!(ownership.active_destination(), local);
    assert!(matches!(
        ownership.state(),
        OwnershipState::Recovering { local: target, .. } if target == local
    ));
}

#[test]
fn disconnect_overflow_and_recovery_do_not_leave_remote_owner() {
    let local = PointerTarget::new(device(1), DisplayId(1));
    let remote = PointerTarget::new(device(2), DisplayId(2));
    let mut ownership = PointerOwnership::new(device(1), local).expect("local target is valid");
    ownership
        .begin_transition(remote)
        .expect("transition begins");
    ownership
        .acknowledge_transition(TransitionAcknowledgement {
            target: remote,
            epoch: 1,
        })
        .expect("transition is acknowledged");

    let disconnect = ownership.on_disconnect().expect("disconnect is handled");
    assert_eq!(disconnect.reason, FailureReason::Disconnected);
    assert_eq!(disconnect.remote_to_release, Some(remote));
    assert_eq!(ownership.active_destination(), local);
    let epoch = ownership.begin_recovery().expect("recovery starts");
    assert_eq!(
        ownership.complete_recovery(epoch - 1),
        Err(OwnershipError::StaleRecovery)
    );
    ownership
        .complete_recovery(epoch)
        .expect("matching recovery completes");
    assert_eq!(
        ownership.state(),
        OwnershipState::LocalActive { target: local }
    );

    let overflow = ownership.on_input_overflow().expect("overflow fails local");
    assert_eq!(overflow.reason, FailureReason::InputOverflow);
    assert_eq!(overflow.remote_to_release, None);
    let emergency = ownership
        .on_emergency_escape()
        .expect("emergency escape remains local");
    assert_eq!(emergency.reason, FailureReason::EmergencyEscape);
    assert_eq!(ownership.active_destination(), local);
}

#[test]
fn lost_ack_disconnect_plans_release_for_pending_remote_target() {
    let local = PointerTarget::new(device(1), DisplayId(1));
    let remote = PointerTarget::new(device(2), DisplayId(2));
    let mut ownership = PointerOwnership::new(device(1), local).expect("local target is valid");
    ownership
        .begin_transition(remote)
        .expect("transition begins");

    let recovery = ownership
        .on_disconnect()
        .expect("disconnect fails closed during handoff");

    assert_eq!(recovery.reason, FailureReason::Disconnected);
    assert_eq!(recovery.remote_to_release, Some(remote));
    assert_eq!(ownership.active_destination(), local);
}

#[test]
fn claim_clears_only_after_both_permits_drop() {
    let _serial = CLAIM_TESTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(!NativeSessionClaim::is_claimed());

    let claim = NativeSessionClaim::claim().expect("first claim");
    assert!(NativeSessionClaim::is_claimed());
    drop(claim);
    assert!(
        !NativeSessionClaim::is_claimed(),
        "an unsplit claim clears on drop"
    );

    let (capture, injection) = NativeSessionClaim::claim().expect("claim").split();
    assert!(NativeSessionClaim::is_claimed());
    drop(capture);
    assert!(
        NativeSessionClaim::is_claimed(),
        "the injection permit still holds"
    );
    drop(injection);
    assert!(!NativeSessionClaim::is_claimed());

    let (capture, injection) = NativeSessionClaim::claim().expect("claim").split();
    drop(injection);
    assert!(
        NativeSessionClaim::is_claimed(),
        "the capture permit still holds"
    );
    drop(capture);
    assert!(!NativeSessionClaim::is_claimed());
}

#[test]
fn second_claim_fails_while_a_permit_lives() {
    let _serial = CLAIM_TESTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let claim = NativeSessionClaim::claim().expect("first claim");
    assert!(NativeSessionClaim::claim().is_none());

    let (capture, injection) = claim.split();
    assert!(NativeSessionClaim::claim().is_none());
    drop(injection);
    assert!(NativeSessionClaim::claim().is_none());
    drop(capture);

    let again = NativeSessionClaim::claim().expect("claim after both permits dropped");
    drop(again);
}

const FLOOR_STATES: [FloorState; 6] = [
    FloorState::Free,
    FloorState::Requesting,
    FloorState::Sending,
    FloorState::Returning,
    FloorState::Receiving,
    FloorState::Yielding,
];

/// Drives a fresh floor to `state` through legal edges only.
fn floor_at(state: FloorState) -> (SharedFloor, FloorSnapshot) {
    let path: &[FloorState] = match state {
        FloorState::Free => &[],
        FloorState::Requesting => &[FloorState::Requesting],
        FloorState::Sending => &[FloorState::Requesting, FloorState::Sending],
        FloorState::Returning => &[
            FloorState::Requesting,
            FloorState::Sending,
            FloorState::Returning,
        ],
        FloorState::Receiving => &[FloorState::Receiving],
        FloorState::Yielding => &[FloorState::Receiving, FloorState::Yielding],
    };
    let floor = SharedFloor::new();
    let mut snapshot = floor.snapshot();
    for next in path {
        snapshot = floor
            .transition(snapshot, *next)
            .expect("path uses legal edges");
    }
    (floor, snapshot)
}

#[test]
fn floor_rejects_illegal_edges() {
    use FloorState::*;
    let legal = [
        (Free, Requesting),
        (Free, Receiving),
        (Requesting, Sending),
        (Requesting, Free),
        (Requesting, Receiving),
        (Sending, Returning),
        (Returning, Free),
        (Receiving, Yielding),
        (Receiving, Free),
        (Yielding, Free),
    ];
    assert_eq!(
        SharedFloor::new().snapshot(),
        FloorSnapshot {
            state: Free,
            generation: 1
        }
    );
    for from in FLOOR_STATES {
        for to in FLOOR_STATES {
            let (floor, before) = floor_at(from);
            let result = floor.transition(before, to);
            if legal.contains(&(from, to)) {
                let after = FloorSnapshot {
                    state: to,
                    generation: before.generation + 1,
                };
                assert_eq!(result, Ok(after), "{from:?} -> {to:?} is legal");
                assert_eq!(floor.snapshot(), after);
            } else {
                assert_eq!(result, Err(before), "{from:?} -> {to:?} is illegal");
                assert_eq!(floor.snapshot(), before, "a refused edge changes nothing");
            }
        }
    }

    let (floor, current) = floor_at(Sending);
    let stale = FloorSnapshot {
        state: Sending,
        generation: current.generation - 1,
    };
    assert_eq!(floor.transition(stale, Returning), Err(current));
    assert_eq!(floor.snapshot(), current);
}

#[test]
fn floor_release_requires_owner_and_generation() {
    assert_eq!(FloorState::Free.owner(), None);
    for state in [
        FloorState::Requesting,
        FloorState::Sending,
        FloorState::Returning,
    ] {
        assert_eq!(state.owner(), Some(FloorOwner::Outbound));
    }
    for state in [FloorState::Receiving, FloorState::Yielding] {
        assert_eq!(state.owner(), Some(FloorOwner::Inbound));
    }

    for state in FLOOR_STATES {
        let (floor, held) = floor_at(state);
        let Some(owner) = state.owner() else {
            assert!(!floor.release(FloorOwner::Outbound, held.generation));
            assert!(!floor.release(FloorOwner::Inbound, held.generation));
            assert_eq!(
                floor.snapshot(),
                held,
                "a free floor has no owner to release it"
            );
            continue;
        };
        let other = match owner {
            FloorOwner::Outbound => FloorOwner::Inbound,
            FloorOwner::Inbound => FloorOwner::Outbound,
        };
        assert!(!floor.release(other, held.generation));
        assert!(!floor.release(owner, held.generation - 1));
        assert!(!floor.release(owner, held.generation + 1));
        assert_eq!(floor.snapshot(), held);
        assert!(floor.release(owner, held.generation));
        assert_eq!(
            floor.snapshot(),
            FloorSnapshot {
                state: FloorState::Free,
                generation: held.generation + 1
            }
        );
        assert!(
            !floor.release(owner, held.generation),
            "release is one-shot"
        );
    }

    let (floor, sending) = floor_at(FloorState::Sending);
    floor.reset();
    assert_eq!(
        floor.snapshot(),
        FloorSnapshot {
            state: FloorState::Free,
            generation: sending.generation + 1
        }
    );
    floor.reset();
    assert_eq!(floor.snapshot().generation, sending.generation + 2);
}

#[test]
fn idle_inbound_release_cannot_free_outbound_floor() {
    let floor = SharedFloor::new();
    let receiving = floor
        .transition(floor.snapshot(), FloorState::Receiving)
        .expect("inbound acquires the free floor");
    assert!(floor.release(FloorOwner::Inbound, receiving.generation));
    let requesting = floor
        .transition(floor.snapshot(), FloorState::Requesting)
        .expect("outbound acquires the free floor");
    let sending = floor
        .transition(requesting, FloorState::Sending)
        .expect("outbound activation completes");

    // The idle inbound half holds or cleans up with its last generation, or even the current one.
    assert!(!floor.release(FloorOwner::Inbound, receiving.generation));
    assert!(!floor.release(FloorOwner::Inbound, sending.generation));
    assert_eq!(floor.snapshot(), sending);

    let clone = floor.clone();
    assert!(clone.release(FloorOwner::Outbound, sending.generation));
    assert_eq!(
        floor.snapshot().state,
        FloorState::Free,
        "clones share one floor"
    );
}

#[test]
fn tie_break_edge_requesting_to_receiving() {
    let floor = SharedFloor::new();
    let requesting = floor
        .transition(floor.snapshot(), FloorState::Requesting)
        .expect("outbound requests");
    let receiving = floor
        .transition(requesting, FloorState::Receiving)
        .expect("the tie-break loser becomes the receiver");
    assert_eq!(receiving.state.owner(), Some(FloorOwner::Inbound));
    assert_eq!(receiving.generation, requesting.generation + 1);

    assert!(!floor.release(FloorOwner::Outbound, requesting.generation));
    assert_eq!(
        floor.transition(requesting, FloorState::Sending),
        Err(receiving),
        "the losing request can no longer complete"
    );
    assert!(floor.release(FloorOwner::Inbound, receiving.generation));
}
