use monhop_core::{HidUsage, MouseButton, Point};
use monhop_platform_macos::{
    KeyMapError, MAC_EVENT_FLAG_COMMAND, MAC_EVENT_FLAG_CONTROL, MAC_EVENT_FLAG_OPTION,
    MAC_EVENT_FLAG_SHIFT, MAX_ABSOLUTE_POINTER_COORDINATE, MAX_RELATIVE_POINTER_DELTA, MacError,
    MacVirtualKey, drag_button_from_held_buttons, hid_to_mac_virtual_key,
    mac_modifier_flags_from_held_keys, mac_virtual_key_to_hid, validate_absolute_point,
    validate_relative_delta,
};
use std::time::Duration;

#[test]
fn cancelled_capture_returns_before_permissions_or_native_setup() {
    let cancelled = std::sync::atomic::AtomicBool::new(true);
    assert_eq!(
        monhop_platform_macos::run_passive_diagnostic_with_cancel(
            Duration::from_secs(10),
            &cancelled,
        ),
        Err(MacError::DiagnosticCancelled)
    );
}

#[test]
fn every_supported_hid_mapping_round_trips_without_changing_physical_key() {
    for raw in 0x04_u16..=0xE7 {
        let usage = HidUsage(raw);
        let Ok(virtual_key) = hid_to_mac_virtual_key(usage) else {
            continue;
        };
        assert_eq!(
            mac_virtual_key_to_hid(virtual_key),
            Ok(usage),
            "supported HID usage {raw:#x} must reverse-map"
        );
    }
}

#[test]
fn maps_separate_meta_and_option_sides_to_mac_physical_keys() {
    assert_eq!(
        hid_to_mac_virtual_key(HidUsage(0xE3)),
        Ok(MacVirtualKey(0x37))
    );
    assert_eq!(
        hid_to_mac_virtual_key(HidUsage(0xE7)),
        Ok(MacVirtualKey(0x36))
    );
    assert_eq!(
        hid_to_mac_virtual_key(HidUsage(0xE2)),
        Ok(MacVirtualKey(0x3A))
    );
    assert_eq!(
        hid_to_mac_virtual_key(HidUsage(0xE6)),
        Ok(MacVirtualKey(0x3D))
    );
}

#[test]
fn unknown_or_nonphysical_keys_are_explicitly_unsupported() {
    assert_eq!(
        hid_to_mac_virtual_key(HidUsage(0x46)),
        Err(KeyMapError::UnsupportedHidUsage)
    );
    assert_eq!(
        mac_virtual_key_to_hid(MacVirtualKey(u16::MAX)),
        Err(KeyMapError::UnsupportedMacVirtualKey)
    );
}

#[test]
fn modifier_flags_survive_one_side_releasing() {
    assert_eq!(
        mac_modifier_flags_from_held_keys([HidUsage(0xE3)]),
        MAC_EVENT_FLAG_COMMAND
    );
    assert_eq!(
        mac_modifier_flags_from_held_keys([HidUsage(0xE3), HidUsage(0xE7)]),
        MAC_EVENT_FLAG_COMMAND
    );
    assert_eq!(
        mac_modifier_flags_from_held_keys([HidUsage(0xE0), HidUsage(0xE1), HidUsage(0xE2)]),
        MAC_EVENT_FLAG_CONTROL | MAC_EVENT_FLAG_SHIFT | MAC_EVENT_FLAG_OPTION
    );
}

#[test]
fn held_button_selection_chooses_dragged_quartz_event_priority() {
    assert_eq!(drag_button_from_held_buttons([]), None);
    assert_eq!(
        drag_button_from_held_buttons([MouseButton::Forward]),
        Some(MouseButton::Forward)
    );
    assert_eq!(
        drag_button_from_held_buttons([MouseButton::Right, MouseButton::Left]),
        Some(MouseButton::Left)
    );
    assert_eq!(
        drag_button_from_held_buttons([MouseButton::Back, MouseButton::Middle]),
        Some(MouseButton::Middle)
    );
}

#[test]
fn pointer_validation_rejects_large_finite_values() {
    assert!(
        validate_absolute_point(Point::new(
            MAX_ABSOLUTE_POINTER_COORDINATE,
            -MAX_ABSOLUTE_POINTER_COORDINATE,
        ))
        .is_ok()
    );
    assert_eq!(
        validate_absolute_point(Point::new(MAX_ABSOLUTE_POINTER_COORDINATE + 1.0, 0.0)),
        Err(MacError::PointOutOfRange)
    );
    assert_eq!(
        validate_relative_delta(Point::new(MAX_RELATIVE_POINTER_DELTA + 1.0, 0.0)),
        Err(MacError::PointOutOfRange)
    );
    assert_eq!(
        validate_relative_delta(Point::new(f64::NAN, 0.0)),
        Err(MacError::InvalidPoint)
    );
}

#[test]
fn passive_diagnostic_rejects_zero_duration_before_platform_access() {
    assert_eq!(
        monhop_platform_macos::run_passive_diagnostic(Duration::ZERO),
        Err(MacError::DiagnosticDurationZero)
    );
}
