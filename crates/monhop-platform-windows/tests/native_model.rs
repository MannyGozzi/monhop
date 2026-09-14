use std::time::Duration;

use monhop_core::{DeviceId, Display, HidUsage};
use monhop_platform_windows::{
    CaptureStats, DisplayError, InputError, KeyMapError, MONHOP_INJECTED_MARKER, Set1Prefix,
    Set1ScanCode, VirtualDesktop, absolute_send_input_coordinates, capture_counts,
    display_id_from_device_name, enumerate_displays, hid_usage_from_set1, is_monhop_injected,
    set1_from_hid_usage, summarize_raw_mouse,
};

#[test]
fn standard_set1_keys_round_trip_to_hid() {
    let cases = [
        (Set1ScanCode::new(0x1e, Set1Prefix::None), HidUsage(0x04)),
        (Set1ScanCode::new(0x01, Set1Prefix::None), HidUsage(0x29)),
        (Set1ScanCode::new(0x1c, Set1Prefix::None), HidUsage(0x28)),
        (Set1ScanCode::new(0x52, Set1Prefix::None), HidUsage(0x62)),
        (Set1ScanCode::new(0x76, Set1Prefix::None), HidUsage(0x73)),
    ];

    for (scan_code, usage) in cases {
        assert_eq!(hid_usage_from_set1(scan_code), Ok(usage));
        assert_eq!(set1_from_hid_usage(usage), Ok(scan_code));
    }
}

#[test]
fn e0_prefix_preserves_modifier_side_and_navigation_identity() {
    let cases = [
        (Set1ScanCode::new(0x1d, Set1Prefix::None), HidUsage(0xe0)),
        (Set1ScanCode::new(0x1d, Set1Prefix::E0), HidUsage(0xe4)),
        (Set1ScanCode::new(0x38, Set1Prefix::None), HidUsage(0xe2)),
        (Set1ScanCode::new(0x38, Set1Prefix::E0), HidUsage(0xe6)),
        (Set1ScanCode::new(0x5b, Set1Prefix::E0), HidUsage(0xe3)),
        (Set1ScanCode::new(0x5c, Set1Prefix::E0), HidUsage(0xe7)),
        (Set1ScanCode::new(0x47, Set1Prefix::None), HidUsage(0x5f)),
        (Set1ScanCode::new(0x47, Set1Prefix::E0), HidUsage(0x4a)),
    ];

    for (scan_code, usage) in cases {
        assert_eq!(hid_usage_from_set1(scan_code), Ok(usage));
        assert_eq!(set1_from_hid_usage(usage), Ok(scan_code));
    }
}

#[test]
fn pause_e1_and_unknown_scancodes_are_explicitly_unsupported() {
    assert_eq!(
        hid_usage_from_set1(Set1ScanCode::new(0x1d, Set1Prefix::E1)),
        Err(KeyMapError::UnsupportedE1Sequence { make_code: 0x1d })
    );
    assert_eq!(
        hid_usage_from_set1(Set1ScanCode::new(0x7f, Set1Prefix::E0)),
        Err(KeyMapError::UnsupportedSet1ScanCode {
            make_code: 0x7f,
            prefix: Set1Prefix::E0,
        })
    );
    assert_eq!(
        set1_from_hid_usage(HidUsage(0x82)),
        Err(KeyMapError::UnsupportedHidUsage)
    );
}

#[test]
fn absolute_coordinates_use_full_negative_origin_virtual_desktop() {
    let desktop = VirtualDesktop::new(-1920, -1080, 3840, 2160).expect("valid desktop");

    assert_eq!(
        absolute_send_input_coordinates(desktop, -1920, -1080),
        Ok((0, 0))
    );
    assert_eq!(
        absolute_send_input_coordinates(desktop, 1919, 1079),
        Ok((65_535, 65_535))
    );
    assert_eq!(
        absolute_send_input_coordinates(desktop, 1920, 1080),
        Err(InputError::PointOutsideVirtualDesktop)
    );
}

#[test]
fn raw_mouse_summary_counts_buttons_scroll_and_ignores_our_marker() {
    let summary = summarize_raw_mouse(0x0001 | 0x0008 | 0x0400 | 0x0800, 4, -2, 0)
        .expect("non-injected input is summarized");
    assert!(summary.motion);
    assert_eq!(summary.button_events, 2);
    assert_eq!(summary.scroll_events, 2);
    assert!(is_monhop_injected(MONHOP_INJECTED_MARKER as u32));
    assert_eq!(
        summarize_raw_mouse(0x0001, 1, 1, MONHOP_INJECTED_MARKER as u32),
        None
    );
}

#[test]
fn display_identifiers_are_stable_for_windows_device_names() {
    let first = display_id_from_device_name(r"\\.\DISPLAY1");
    assert_eq!(first, display_id_from_device_name(r"\\.\DISPLAY1"));
    assert_ne!(first, display_id_from_device_name(r"\\.\DISPLAY2"));
}

#[test]
fn cli_facing_native_diagnostic_signatures_are_exported() {
    let _: fn(DeviceId) -> Result<Vec<Display>, DisplayError> = enumerate_displays;
    let _: fn(Duration) -> Result<CaptureStats, InputError> = capture_counts;
}
