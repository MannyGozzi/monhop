use monhop_core::{
    HidUsage, LogicalRect, LogicalSize, ModifierState, MouseButton, Point, capture::CaptureEvent,
    capture_physical::LocalTransfer,
};
use monhop_platform_macos::{
    MAC_EVENT_FLAG_CONTROL, MAC_EVENT_FLAG_SHIFT, SYNTHETIC_EVENT_MARKER,
    capture_decode::{
        ActiveDisplayBounds, CG_EVENT_FLAGS_CHANGED, CG_EVENT_KEY_DOWN, CG_EVENT_KEY_UP,
        CG_EVENT_LEFT_MOUSE_DOWN, CG_EVENT_LEFT_MOUSE_UP, CG_EVENT_MOUSE_MOVED,
        CG_EVENT_OTHER_MOUSE_DOWN, CG_EVENT_SOURCE_STATE_HID_SYSTEM, DecodedInput,
        EventSourceMetadata, HidKeyState, LocalModifierState, PhysicalModifierLedger,
        PointerFields, QUARTZ_POINTS_PER_DISCRETE_SCROLL_LINE, decode_keyboard, decode_pointer,
        decode_scroll, should_keep_quarantine_tap,
    },
};

fn physical_source() -> EventSourceMetadata {
    EventSourceMetadata {
        user_data: 0,
        state_id: CG_EVENT_SOURCE_STATE_HID_SYSTEM,
    }
}

fn pointer(location: Point, delta_x: f64, delta_y: f64, button_number: i64) -> PointerFields {
    PointerFields {
        location,
        delta_x,
        delta_y,
        button_number,
        click_state: 1,
    }
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> LogicalRect {
    LogicalRect {
        origin: Point::new(x, y),
        size: LogicalSize::new(width, height),
    }
}

#[test]
fn keyboard_decode_keeps_side_specific_hid_and_defers_repeat_and_modifiers_to_ledger() {
    let source = physical_source();
    let mut modifiers = PhysicalModifierLedger::default();
    match decode_keyboard(
        CG_EVENT_KEY_DOWN,
        0x3b,
        &mut modifiers,
        HidKeyState::default(),
        source,
        SYNTHETIC_EVENT_MARKER,
    ) {
        DecodedInput::Event(CaptureEvent::Key {
            usage,
            pressed,
            repeat,
            modifiers,
        }) => {
            assert_eq!(usage, HidUsage(0xe0));
            assert!(pressed);
            assert!(!repeat);
            assert_eq!(modifiers, ModifierState::default());
        }
        _ => panic!("expected left-control key press"),
    }
    modifiers.seed_held(HidUsage(0xe4));
    match decode_keyboard(
        CG_EVENT_FLAGS_CHANGED,
        0x3e,
        &mut modifiers,
        HidKeyState {
            down: true,
            injection_held: true,
        },
        source,
        SYNTHETIC_EVENT_MARKER,
    ) {
        DecodedInput::Event(CaptureEvent::Key { usage, pressed, .. }) => {
            assert_eq!(usage, HidUsage(0xe4));
            assert!(!pressed);
        }
        _ => panic!("expected right-control key release"),
    }
    assert!(matches!(
        decode_keyboard(
            CG_EVENT_KEY_UP,
            0xffff,
            &mut modifiers,
            HidKeyState::default(),
            source,
            SYNTHETIC_EVENT_MARKER,
        ),
        DecodedInput::Unsupported
    ));
}

#[test]
fn marked_or_non_hid_events_never_reach_the_physical_ledger() {
    let source = physical_source();
    assert!(matches!(
        decode_keyboard(
            CG_EVENT_KEY_DOWN,
            0,
            &mut PhysicalModifierLedger::default(),
            HidKeyState::default(),
            EventSourceMetadata {
                user_data: SYNTHETIC_EVENT_MARKER,
                ..source
            },
            SYNTHETIC_EVENT_MARKER,
        ),
        DecodedInput::Ignored
    ));
    assert!(matches!(
        decode_scroll(
            0.5,
            -0.25,
            true,
            EventSourceMetadata {
                state_id: 0,
                ..source
            },
            SYNTHETIC_EVENT_MARKER,
        ),
        DecodedInput::Ignored
    ));
}

#[test]
fn quarantine_waits_for_a_suppressed_release_unless_interception_failed() {
    assert!(should_keep_quarantine_tap(true, false));
    assert!(!should_keep_quarantine_tap(false, false));
    assert!(!should_keep_quarantine_tap(true, true));
}

#[test]
fn local_anchor_precedes_outward_delta_at_a_clamped_edge() {
    let source = physical_source();
    let decoded = decode_pointer(
        CG_EVENT_MOUSE_MOVED,
        pointer(Point::new(-1440.0, 11.5), 8.0, -3.0, 0),
        Some(Point::new(-1440.0, 11.5)),
        source,
        SYNTHETIC_EVENT_MARKER,
    );
    assert_eq!(decoded.position, Some(Point::new(-1440.0, 11.5)));
    match decoded.local_absolute {
        Some(CaptureEvent::LogicalAbsoluteMotion { x, y }) => {
            assert_eq!(x.to_bits(), (-1440.0_f64).to_bits());
            assert_eq!(y.to_bits(), 11.5_f64.to_bits());
        }
        _ => panic!("expected local cursor anchor"),
    }
    match decoded.input {
        DecodedInput::Event(CaptureEvent::LogicalRelativeMotion { dx, dy }) => {
            assert_eq!(dx.to_bits(), 8.0_f64.to_bits());
            assert_eq!(dy.to_bits(), (-3.0_f64).to_bits());
        }
        _ => panic!("expected independent outward delta"),
    }
}

#[test]
fn remote_motion_keeps_nonzero_delta_when_the_quartz_location_is_constant() {
    let source = physical_source();
    let decoded = decode_pointer(
        CG_EVENT_MOUSE_MOVED,
        pointer(Point::new(1919.999, 100.0), 6.0, 0.0, 0),
        Some(Point::new(1919.999, 100.0)),
        source,
        SYNTHETIC_EVENT_MARKER,
    );
    assert!(matches!(
        decoded.local_absolute,
        Some(CaptureEvent::LogicalAbsoluteMotion { .. })
    ));
    assert!(matches!(
        decoded.input,
        DecodedInput::Event(CaptureEvent::LogicalRelativeMotion { dx, dy })
            if dx == 6.0 && dy == 0.0
    ));
}

#[test]
fn sub_point_trackpad_deltas_reach_capture_unrounded() {
    let decoded = decode_pointer(
        CG_EVENT_MOUSE_MOVED,
        pointer(Point::new(10.0, 10.0), 0.375, -0.125, 0),
        None,
        physical_source(),
        SYNTHETIC_EVENT_MARKER,
    );
    match decoded.input {
        DecodedInput::Event(CaptureEvent::LogicalRelativeMotion { dx, dy }) => {
            assert_eq!(dx.to_bits(), 0.375_f64.to_bits());
            assert_eq!(dy.to_bits(), (-0.125_f64).to_bits());
        }
        _ => panic!("a sub-point move is motion, not an empty record"),
    }
    for (dx, dy) in [(f64::NAN, 0.0), (0.0, f64::INFINITY)] {
        let malformed = decode_pointer(
            CG_EVENT_MOUSE_MOVED,
            pointer(Point::new(10.0, 10.0), dx, dy, 0),
            Some(Point::new(1.0, 1.0)),
            physical_source(),
            SYNTHETIC_EVENT_MARKER,
        );
        assert!(matches!(malformed.input, DecodedInput::Malformed));
        assert_eq!(malformed.position, Some(Point::new(1.0, 1.0)));
    }
}

#[test]
fn a_press_and_its_release_carry_the_quartz_click_state() {
    for (click_state, click_count) in [(2, 2), (3, 3), (0, 1), (-4, 1), (300, u8::MAX)] {
        for (event_type, pressed) in [
            (CG_EVENT_LEFT_MOUSE_DOWN, true),
            (CG_EVENT_LEFT_MOUSE_UP, false),
        ] {
            let decoded = decode_pointer(
                event_type,
                PointerFields {
                    click_state,
                    ..pointer(Point::new(4.0, 5.0), 0.0, 0.0, 0)
                },
                None,
                physical_source(),
                SYNTHETIC_EVENT_MARKER,
            );
            assert!(matches!(
                decoded.input,
                DecodedInput::Event(CaptureEvent::Button {
                    button: MouseButton::Left,
                    pressed: actual,
                    click_count: actual_count,
                }) if actual == pressed && actual_count == click_count
            ));
        }
    }
}

#[test]
fn restore_bounds_and_modifier_post_flags_are_exact() {
    let bounds = ActiveDisplayBounds::from_rectangles([
        rect(-1440.0, 0.0, 1440.0, 900.0),
        rect(0.0, 0.0, 1920.0, 1080.0),
    ])
    .expect("valid active displays");
    assert!(bounds.contains(Point::new(-1440.0, 0.0)));
    assert!(bounds.contains(Point::new(1919.999, 1079.999)));
    assert!(!bounds.contains(Point::new(1920.0, 10.0)));
    assert!(!bounds.contains(Point::new(-1440.001, 10.0)));
    assert!(ActiveDisplayBounds::from_rectangles([rect(0.0, 0.0, 0.0, 1.0)]).is_none());

    let mut modifiers = LocalModifierState::default();
    modifiers.record_local_key(HidUsage(0xe0), true);
    modifiers.record_local_key(HidUsage(0xe4), true);
    let release_left = LocalTransfer::Key {
        usage: HidUsage(0xe0),
        pressed: false,
    };
    assert_eq!(
        modifiers.flags_after_transfer(release_left),
        MAC_EVENT_FLAG_CONTROL
    );
    modifiers.apply_transfer_success(release_left);
    let release_right = LocalTransfer::Key {
        usage: HidUsage(0xe4),
        pressed: false,
    };
    assert_eq!(modifiers.flags_after_transfer(release_right), 0);
    modifiers.apply_transfer_success(release_right);
    modifiers.record_local_key(HidUsage(0xe1), true);
    assert_eq!(modifiers.flags(), MAC_EVENT_FLAG_SHIFT);
}

#[test]
fn pointer_buttons_and_scroll_preserve_fractions_in_logical_points() {
    let source = physical_source();
    let button = decode_pointer(
        CG_EVENT_OTHER_MOUSE_DOWN,
        pointer(Point::new(4.0, 5.0), 0.0, 0.0, 4),
        None,
        source,
        SYNTHETIC_EVENT_MARKER,
    );
    assert!(matches!(
        button.input,
        DecodedInput::Event(CaptureEvent::Button {
            button: MouseButton::Forward,
            pressed: true,
            click_count: 1,
        })
    ));
    let left = decode_pointer(
        CG_EVENT_LEFT_MOUSE_DOWN,
        pointer(Point::new(4.0, 5.0), 0.0, 0.0, 0),
        None,
        source,
        SYNTHETIC_EVENT_MARKER,
    );
    assert!(matches!(
        left.input,
        DecodedInput::Event(CaptureEvent::Button {
            button: MouseButton::Left,
            pressed: true,
            click_count: 1,
        })
    ));
    match decode_scroll(-0.015_625, 2.75, true, source, SYNTHETIC_EVENT_MARKER) {
        DecodedInput::Event(CaptureEvent::LogicalScroll {
            horizontal,
            vertical,
        }) => {
            assert_eq!(horizontal.to_bits(), (-0.015_625_f64).to_bits());
            assert_eq!(vertical.to_bits(), 2.75_f64.to_bits());
        }
        _ => panic!("expected fractional logical scroll"),
    }
    match decode_scroll(-0.015_625, 2.75, false, source, SYNTHETIC_EVENT_MARKER) {
        DecodedInput::Event(CaptureEvent::LogicalScroll {
            horizontal,
            vertical,
        }) => {
            assert_eq!(
                horizontal.to_bits(),
                (-0.015_625_f64 * QUARTZ_POINTS_PER_DISCRETE_SCROLL_LINE).to_bits()
            );
            assert_eq!(
                vertical.to_bits(),
                (2.75_f64 * QUARTZ_POINTS_PER_DISCRETE_SCROLL_LINE).to_bits()
            );
        }
        _ => panic!("expected discrete scroll normalized to logical points"),
    }
    assert!(matches!(
        decode_scroll(f64::NAN, 0.0, true, source, SYNTHETIC_EVENT_MARKER),
        DecodedInput::Malformed
    ));
}
