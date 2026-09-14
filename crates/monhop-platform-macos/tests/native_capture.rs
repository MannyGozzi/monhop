#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use monhop_platform_macos::{MacError, preflight_permissions, run_passive_diagnostic};

// Field order and widths match CGEventTapInformation in the installed SDK.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TapInformation {
    id: u32,
    location: u32,
    options: u32,
    event_mask: u64,
    tapping_process: i32,
    target_process: i32,
    enabled: u8,
    min_latency: f32,
    average_latency: f32,
    max_latency: f32,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetEventTapList(capacity: u32, taps: *mut TapInformation, count: *mut u32) -> i32;
}

fn own_tap_count() -> usize {
    let mut taps = [TapInformation::default(); 256];
    let mut count = 0;
    // SAFETY: The writable array has the supplied capacity and matches the SDK ABI.
    let status = unsafe { CGGetEventTapList(taps.len() as u32, taps.as_mut_ptr(), &mut count) };
    assert_eq!(status, 0, "native tap enumeration failed");
    assert!(
        (count as usize) < taps.len(),
        "tap inventory may be truncated"
    );
    taps[..count as usize]
        .iter()
        .filter(|tap| tap.tapping_process == std::process::id() as i32)
        .count()
}

#[test]
#[ignore = "explicit native diagnostic: requires existing macOS input permission"]
fn passive_capture_removes_tap_before_returning_in_the_same_process() {
    assert!(preflight_permissions().unwrap().listen_events);
    assert_eq!(own_tap_count(), 0);
    let observer = std::thread::spawn(|| {
        // CGGetEventTapList resets tap latency extrema, so sample only once.
        std::thread::sleep(Duration::from_millis(500));
        own_tap_count() == 1
    });

    let started = Instant::now();
    let result = run_passive_diagnostic(Duration::from_secs(1));
    let elapsed = started.elapsed();
    let remaining_taps = own_tap_count();
    let observed_active_tap = observer.join().unwrap();

    result.unwrap();
    assert!(observed_active_tap, "observer never saw the live tap");
    assert_eq!(remaining_taps, 0, "tap survived the diagnostic return");
    assert!(elapsed >= Duration::from_millis(950));
    assert!(elapsed < Duration::from_secs(2), "native deadline exceeded");
}

#[test]
#[ignore = "run only from a launch context without macOS input permission"]
fn denied_permission_rejects_capture_without_installing_a_tap() {
    assert!(!preflight_permissions().unwrap().listen_events);
    assert_eq!(own_tap_count(), 0);
    assert_eq!(
        run_passive_diagnostic(Duration::from_secs(1)),
        Err(MacError::ListenEventPermissionRequired)
    );
    assert_eq!(own_tap_count(), 0);
}

#[test]
#[ignore = "explicit native diagnostic: requires existing macOS input permission"]
fn cancelled_capture_removes_its_tap_before_returning() {
    use monhop_platform_macos::run_passive_diagnostic_with_cancel;
    use std::sync::atomic::{AtomicBool, Ordering};

    assert!(preflight_permissions().unwrap().listen_events);
    assert_eq!(own_tap_count(), 0);
    let cancelled = AtomicBool::new(false);
    let started = Instant::now();
    let result = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(150));
            cancelled.store(true, Ordering::Release);
        });
        run_passive_diagnostic_with_cancel(Duration::from_secs(10), &cancelled)
    });
    assert_eq!(result, Err(MacError::DiagnosticCancelled));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(own_tap_count(), 0);
}
