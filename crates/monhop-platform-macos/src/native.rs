use std::ffi::c_void;
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use monhop_core::{
    DisplayId, LogicalRect, LogicalSize, MAX_DISPLAYS, MouseButton, NativeSize, Point,
};

use crate::capture_decode::{
    CG_EVENT_FLAGS_CHANGED, CG_EVENT_KEY_DOWN, CG_EVENT_KEY_UP, CG_EVENT_LEFT_MOUSE_DOWN,
    CG_EVENT_LEFT_MOUSE_DRAGGED, CG_EVENT_LEFT_MOUSE_UP, CG_EVENT_MOUSE_MOVED,
    CG_EVENT_OTHER_MOUSE_DOWN, CG_EVENT_OTHER_MOUSE_DRAGGED, CG_EVENT_OTHER_MOUSE_UP,
    CG_EVENT_RIGHT_MOUSE_DOWN, CG_EVENT_RIGHT_MOUSE_DRAGGED, CG_EVENT_RIGHT_MOUSE_UP,
    CG_EVENT_SCROLL_WHEEL,
};
use crate::event_tap::{
    CFAllocatorRef, CFRelease, CFRunLoopGetCurrent, CFRunLoopRef, CFStringRef,
    CG_EVENT_SOURCE_USER_DATA, CG_EVENT_TAP_OPTION_LISTEN_ONLY, CG_HEAD_INSERT_EVENT_TAP,
    CG_HID_EVENT_TAP, CGEventField, CGEventGetIntegerValueField, CGEventPost, CGEventRef,
    CGEventSetIntegerValueField, CGEventTapProxy, CGEventType, EventTap, EventTapInstallError,
    finish_and_post_event, full_input_event_mask, tap_disabled,
};

use crate::{
    MacDisplay, MacError, MacVirtualKey, PassiveDiagnosticCounts, PermissionState,
    validate_absolute_point,
};

type CFBooleanRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFMutableDictionaryRef = *mut c_void;
type CGDisplayModeRef = *const c_void;
type CGEventSourceRef = *const c_void;
type CGDirectDisplayID = u32;
type CGError = i32;
type CGKeyCode = u16;
type CGMouseButton = u32;
type Pid = i32;
type CGScrollEventUnit = u32;
// Mach boolean_t is unsigned in Intel user space and signed on Apple Silicon.
#[cfg(target_arch = "x86_64")]
type MachBoolean = u32;
#[cfg(not(target_arch = "x86_64"))]
type MachBoolean = i32;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

const CG_ERROR_SUCCESS: CGError = 0;
const CG_SCROLL_EVENT_UNIT_PIXEL: CGScrollEventUnit = 0;
const CG_MOUSE_EVENT_BUTTON_NUMBER: CGEventField = 3;
/// Core Graphics `kCGMouseEventClickState`, the count macOS apps read to recognize a multi-click.
const CG_MOUSE_EVENT_CLICK_STATE: CGEventField = 1;

// SAFETY: These declarations match the documented Core Graphics and
// ApplicationServices C interfaces and are called only from macOS builds.
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGGetActiveDisplayList(
        max_displays: u32,
        active_displays: *mut CGDirectDisplayID,
        display_count: *mut u32,
    ) -> CGError;
    fn CGDisplayBounds(display: CGDirectDisplayID) -> CGRect;
    fn CGDisplayIsMain(display: CGDirectDisplayID) -> MachBoolean;
    fn CGDisplayVendorNumber(display: CGDirectDisplayID) -> u32;
    fn CGDisplayModelNumber(display: CGDirectDisplayID) -> u32;
    fn CGDisplaySerialNumber(display: CGDirectDisplayID) -> u32;
    fn CGDisplayCopyDisplayMode(display: CGDirectDisplayID) -> CGDisplayModeRef;
    fn CGDisplayModeGetRefreshRate(mode: CGDisplayModeRef) -> f64;
    fn CGDisplayModeGetPixelWidth(mode: CGDisplayModeRef) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: CGDisplayModeRef) -> usize;

    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> u8;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
    fn CGPreflightListenEventAccess() -> bool;
    fn CGRequestListenEventAccess() -> bool;

    fn CGEventCreateKeyboardEvent(
        source: CGEventSourceRef,
        virtual_key: CGKeyCode,
        key_down: bool,
    ) -> CGEventRef;
    fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        mouse_type: CGEventType,
        mouse_cursor_position: CGPoint,
        mouse_button: CGMouseButton,
    ) -> CGEventRef;
    fn CGEventCreateScrollWheelEvent2(
        source: CGEventSourceRef,
        units: CGScrollEventUnit,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CGEventRef;
    fn CGEventSetLocation(event: CGEventRef, location: CGPoint);
    fn CGEventPostToPid(pid: Pid, event: CGEventRef);
}

// SAFETY: These declarations match the documented Core Foundation ownership
// APIs used to scope the explicit diagnostic operation.
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDictionaryCreateMutable(
        allocator: CFAllocatorRef,
        capacity: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFMutableDictionaryRef;
    fn CFDictionarySetValue(
        dictionary: CFMutableDictionaryRef,
        key: *const c_void,
        value: *const c_void,
    );
    static kCFBooleanTrue: CFBooleanRef;
    fn CFRunLoopStop(run_loop: CFRunLoopRef);
}

#[derive(Clone, Copy, Debug)]
pub enum PostingDestination {
    System,
    CurrentProcess,
}

pub const fn system_destination() -> PostingDestination {
    PostingDestination::System
}

pub fn current_process_destination() -> Result<PostingDestination, MacError> {
    Ok(PostingDestination::CurrentProcess)
}

pub fn enumerate_active_displays() -> Result<Vec<MacDisplay>, MacError> {
    let mut total = 0_u32;
    // SAFETY: Core Graphics permits a null list when querying the active count.
    let status = unsafe { CGGetActiveDisplayList(0, ptr::null_mut(), &mut total) };
    if status != CG_ERROR_SUCCESS {
        return Err(MacError::DisplayEnumerationFailed);
    }
    if usize::try_from(total).map_err(|_| MacError::DisplayLimitExceeded)? > MAX_DISPLAYS {
        return Err(MacError::DisplayLimitExceeded);
    }

    let mut display_ids = [0_u32; MAX_DISPLAYS];
    let mut actual = total;
    // SAFETY: The fixed array has capacity MAX_DISPLAYS, which is passed as
    // max_displays, and actual points to initialized writable storage.
    let status = unsafe {
        CGGetActiveDisplayList(MAX_DISPLAYS as u32, display_ids.as_mut_ptr(), &mut actual)
    };
    if status != CG_ERROR_SUCCESS {
        return Err(MacError::DisplayEnumerationFailed);
    }
    let actual = usize::try_from(actual).map_err(|_| MacError::DisplayLimitExceeded)?;
    if actual > MAX_DISPLAYS {
        return Err(MacError::DisplayLimitExceeded);
    }

    display_ids[..actual]
        .iter()
        .copied()
        .map(display_metadata)
        .collect()
}

/// Caches a probed value behind a monotonic TTL so a hot loop can avoid re-probing on every call.
struct TtlCache<T> {
    ttl: Duration,
    cached: Mutex<Option<(Instant, T)>>,
}

impl<T: Copy> TtlCache<T> {
    const fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            cached: Mutex::new(None),
        }
    }

    /// Returns the cached value if it is still within its TTL; otherwise calls `probe` and caches
    /// its result. A probe error is never cached, so the next call probes again.
    fn get(&self, probe: impl FnOnce() -> Result<T, MacError>) -> Result<T, MacError> {
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some((fetched_at, value)) = *cached
            && fetched_at.elapsed() < self.ttl
        {
            return Ok(value);
        }
        let value = probe()?;
        *cached = Some((Instant::now(), value));
        Ok(value)
    }
}

const PERMISSION_CACHE_TTL: Duration = Duration::from_millis(250);
static PERMISSION_CACHE: TtlCache<PermissionState> = TtlCache::new(PERMISSION_CACHE_TTL);

pub fn preflight_permissions() -> Result<PermissionState, MacError> {
    PERMISSION_CACHE.get(raw_preflight_permissions)
}

fn raw_preflight_permissions() -> Result<PermissionState, MacError> {
    // SAFETY: Both functions are side-effect-free permission queries.
    let state = unsafe {
        PermissionState {
            accessibility: AXIsProcessTrusted() != 0,
            listen_events: CGPreflightListenEventAccess(),
        }
    };
    Ok(state)
}

pub fn request_permissions() -> Result<PermissionState, MacError> {
    let before = preflight_permissions()?;
    let accessibility = if before.accessibility {
        true
    } else {
        // SAFETY: A null allocator selects Core Foundation's default allocator.
        let options =
            unsafe { CFDictionaryCreateMutable(ptr::null(), 1, ptr::null(), ptr::null()) };
        if options.is_null() {
            return Err(MacError::NativeEventCreationFailed);
        }
        // SAFETY: Both values are process-lifetime Core Foundation constants and
        // options remains valid until the matching CFRelease below.
        unsafe {
            CFDictionarySetValue(options, kAXTrustedCheckOptionPrompt, kCFBooleanTrue);
        }
        // SAFETY: The explicitly created dictionary requests only Apple's normal
        // Accessibility prompt and is valid for this synchronous call.
        let trusted = unsafe { AXIsProcessTrustedWithOptions(options) != 0 };
        // SAFETY: options was returned owned by CFDictionaryCreateMutable.
        unsafe {
            CFRelease(options);
        }
        trusted
    };
    // SAFETY: This explicit public function is the only call site that asks
    // macOS to display the listen-event permission prompt.
    let listen_events = if before.listen_events {
        true
    } else {
        // SAFETY: This explicit public function is the only call site that asks
        // macOS to display the listen-event permission prompt.
        unsafe { CGRequestListenEventAccess() }
    };
    Ok(PermissionState {
        accessibility,
        listen_events,
    })
}

pub fn run_passive_diagnostic(
    duration: Duration,
    synthetic_marker: i64,
    cancelled: &AtomicBool,
) -> Result<PassiveDiagnosticCounts, MacError> {
    if !preflight_permissions()?.listen_events {
        return Err(MacError::ListenEventPermissionRequired);
    }

    let mut state = DiagnosticState {
        counts: PassiveDiagnosticCounts::default(),
        synthetic_marker,
        tap_disabled: false,
    };
    // SAFETY: state outlives the tap, which is closed before this function returns.
    let mut tap = EventTap::install(
        CG_HEAD_INSERT_EVENT_TAP,
        CG_EVENT_TAP_OPTION_LISTEN_ONLY,
        full_input_event_mask(),
        Some(diagnostic_callback),
        (&mut state as *mut DiagnosticState).cast(),
    )
    .map_err(|error| match error {
        EventTapInstallError::TapUnavailable => MacError::EventTapUnavailable,
        EventTapInstallError::SourceUnavailable => MacError::EventTapSourceUnavailable,
    })?;

    let deadline = Instant::now() + duration;
    while !cancelled.load(Ordering::Acquire) && !state.tap_disabled {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        tap.run_once(remaining.min(Duration::from_millis(20)));
    }
    tap.close();

    if state.tap_disabled {
        return Err(MacError::EventTapDisabled);
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(MacError::DiagnosticCancelled);
    }
    Ok(state.counts)
}

pub fn ensure_injection_permission() -> Result<(), MacError> {
    if !preflight_permissions()?.accessibility {
        return Err(MacError::AccessibilityPermissionRequired);
    }
    Ok(())
}

pub fn post_key(
    destination: PostingDestination,
    key: MacVirtualKey,
    is_down: bool,
    marker: i64,
    flags: u64,
) -> Result<(), MacError> {
    ensure_injection_permission()?;
    // SAFETY: A null event source asks Core Graphics for its default source;
    // key is a validated macOS virtual key code from the mapping table.
    let event = unsafe { CGEventCreateKeyboardEvent(ptr::null(), key.0, is_down) };
    post_marked_event(destination, event, marker, flags)
}

pub fn post_button(
    destination: PostingDestination,
    button: MouseButton,
    is_down: bool,
    click_count: u8,
    marker: i64,
    flags: u64,
    point: Point,
) -> Result<(), MacError> {
    ensure_injection_permission()?;
    let event = button_event(button, is_down, click_count, point_to_cg(point)?)?;
    post_marked_event(destination, event, marker, flags)
}

/// Returns an owned event that the caller posts or releases.
fn button_event(
    button: MouseButton,
    is_down: bool,
    click_count: u8,
    location: CGPoint,
) -> Result<CGEventRef, MacError> {
    let (event_type, button_number) = mouse_button_event(button, is_down);
    let creation_button = button_number.min(2);
    // SAFETY: location is a checked MonHop cursor point and the event type and
    // button pair are selected from the fixed MouseButton enum.
    let event =
        unsafe { CGEventCreateMouseEvent(ptr::null(), event_type, location, creation_button) };
    if event.is_null() {
        return Err(MacError::NativeEventCreationFailed);
    }
    // SAFETY: event is a non-null Core Graphics event owned by the caller.
    unsafe {
        CGEventSetIntegerValueField(
            event,
            CG_MOUSE_EVENT_BUTTON_NUMBER,
            i64::from(button_number),
        );
        CGEventSetIntegerValueField(event, CG_MOUSE_EVENT_CLICK_STATE, i64::from(click_count));
    }
    Ok(event)
}

pub fn post_scroll(
    destination: PostingDestination,
    horizontal: i32,
    vertical: i32,
    marker: i64,
    flags: u64,
    point: Point,
) -> Result<(), MacError> {
    ensure_injection_permission()?;
    let location = point_to_cg(point)?;
    // SAFETY: The pixel-unit, two-axis event uses bounded i32 components and a
    // null source requests the default Core Graphics source.
    let event = unsafe {
        CGEventCreateScrollWheelEvent2(
            ptr::null(),
            CG_SCROLL_EVENT_UNIT_PIXEL,
            2,
            vertical,
            horizontal,
            0,
        )
    };
    if event.is_null() {
        return Err(MacError::NativeEventCreationFailed);
    }
    // SAFETY: event is non-null and owned here; location is the validated virtual cursor.
    unsafe { CGEventSetLocation(event, location) };
    post_marked_event(destination, event, marker, flags)
}

pub fn post_motion(
    destination: PostingDestination,
    point: Point,
    marker: i64,
    flags: u64,
    drag_button: Option<MouseButton>,
) -> Result<(), MacError> {
    ensure_injection_permission()?;
    let location = point_to_cg(point)?;
    let (event_type, button_number) = drag_button
        .map(mouse_drag_event)
        .unwrap_or((CG_EVENT_MOUSE_MOVED, 0));
    // SAFETY: location is finite and the mouse-moved event uses a documented
    // button selected from the held MouseButton state.
    let event =
        unsafe { CGEventCreateMouseEvent(ptr::null(), event_type, location, button_number.min(2)) };
    if event.is_null() {
        return Err(MacError::NativeEventCreationFailed);
    }
    if drag_button.is_some() {
        // SAFETY: event is non-null and owned until post_marked_event releases it.
        unsafe {
            CGEventSetIntegerValueField(
                event,
                CG_MOUSE_EVENT_BUTTON_NUMBER,
                i64::from(button_number),
            );
        }
    }
    post_marked_event(destination, event, marker, flags)
}

fn display_metadata(display: CGDirectDisplayID) -> Result<MacDisplay, MacError> {
    // SAFETY: display IDs came directly from CGGetActiveDisplayList.
    let bounds = unsafe { CGDisplayBounds(display) };
    if !bounds.origin.x.is_finite()
        || !bounds.origin.y.is_finite()
        || !bounds.size.width.is_finite()
        || !bounds.size.height.is_finite()
        || bounds.size.width <= 0.0
        || bounds.size.height <= 0.0
    {
        return Err(MacError::InvalidDisplayData);
    }
    let (width, height, refresh_rate_hz) = display_mode_details(display)?;
    let scale_factor = f64::from(width) / bounds.size.width;
    if !scale_factor.is_finite() || scale_factor <= 0.0 {
        return Err(MacError::InvalidDisplayData);
    }
    // SAFETY: display IDs came directly from CGGetActiveDisplayList.
    let primary = unsafe { CGDisplayIsMain(display) } != 0;
    // SAFETY: display IDs came directly from CGGetActiveDisplayList.
    let (vendor, model, serial) = unsafe {
        (
            CGDisplayVendorNumber(display),
            CGDisplayModelNumber(display),
            CGDisplaySerialNumber(display),
        )
    };
    let monitor = monitor_identity(vendor, model, serial);

    let mut result = MacDisplay {
        id: DisplayId(u64::from(display)),
        name: None,
        native_size: NativeSize::new(width, height),
        logical_bounds: LogicalRect {
            origin: Point::new(bounds.origin.x, bounds.origin.y),
            size: LogicalSize::new(bounds.size.width, bounds.size.height),
        },
        scale_factor,
        refresh_rate_hz,
        primary,
        monitor,
    };
    result.name = crate::display_names::name_for(&result);
    Ok(result)
}

// Core Graphics placeholders for a display without an EDID vendor or product id.
const DISPLAY_VENDOR_UNKNOWN: u32 = 0x756e_6b6e;
const DISPLAY_PRODUCT_GENERIC: u32 = 0x717;

/// The EDID numbers as Core Graphics reports them; a placeholder or an oversized value means none.
fn monitor_identity(vendor: u32, model: u32, serial: u32) -> Option<monhop_core::MonitorIdentity> {
    if vendor == DISPLAY_VENDOR_UNKNOWN || model == DISPLAY_PRODUCT_GENERIC {
        return None;
    }
    monhop_core::MonitorIdentity::new(
        u16::try_from(vendor).ok()?,
        u16::try_from(model).ok()?,
        serial,
    )
}

fn display_mode_details(display: CGDirectDisplayID) -> Result<(u32, u32, Option<f64>), MacError> {
    // SAFETY: display IDs came directly from CGGetActiveDisplayList.
    let mode = unsafe { CGDisplayCopyDisplayMode(display) };
    if mode.is_null() {
        return Err(MacError::InvalidDisplayData);
    }
    // Pixel-specific mode APIs distinguish Retina backing pixels from desktop points.
    // SAFETY: mode is non-null and remains owned until the CFRelease below.
    let (width, height, refresh_rate_hz) = unsafe {
        (
            CGDisplayModeGetPixelWidth(mode),
            CGDisplayModeGetPixelHeight(mode),
            CGDisplayModeGetRefreshRate(mode),
        )
    };
    // SAFETY: CGDisplayCopyDisplayMode returned an owned Core Foundation object.
    unsafe {
        CFRelease(mode);
    }
    let width = u32::try_from(width).map_err(|_| MacError::InvalidDisplayData)?;
    let height = u32::try_from(height).map_err(|_| MacError::InvalidDisplayData)?;
    if width == 0 || height == 0 {
        return Err(MacError::InvalidDisplayData);
    }
    Ok((
        width,
        height,
        (refresh_rate_hz.is_finite() && refresh_rate_hz > 0.0).then_some(refresh_rate_hz),
    ))
}

fn point_to_cg(point: Point) -> Result<CGPoint, MacError> {
    validate_absolute_point(point)?;
    Ok(CGPoint {
        x: point.x,
        y: point.y,
    })
}

fn mouse_button_event(button: MouseButton, is_down: bool) -> (CGEventType, CGMouseButton) {
    match (button, is_down) {
        (MouseButton::Left, true) => (CG_EVENT_LEFT_MOUSE_DOWN, 0),
        (MouseButton::Left, false) => (CG_EVENT_LEFT_MOUSE_UP, 0),
        (MouseButton::Right, true) => (CG_EVENT_RIGHT_MOUSE_DOWN, 1),
        (MouseButton::Right, false) => (CG_EVENT_RIGHT_MOUSE_UP, 1),
        (MouseButton::Middle, true) => (CG_EVENT_OTHER_MOUSE_DOWN, 2),
        (MouseButton::Middle, false) => (CG_EVENT_OTHER_MOUSE_UP, 2),
        (MouseButton::Back, true) => (CG_EVENT_OTHER_MOUSE_DOWN, 3),
        (MouseButton::Back, false) => (CG_EVENT_OTHER_MOUSE_UP, 3),
        (MouseButton::Forward, true) => (CG_EVENT_OTHER_MOUSE_DOWN, 4),
        (MouseButton::Forward, false) => (CG_EVENT_OTHER_MOUSE_UP, 4),
    }
}

fn mouse_drag_event(button: MouseButton) -> (CGEventType, CGMouseButton) {
    match button {
        MouseButton::Left => (CG_EVENT_LEFT_MOUSE_DRAGGED, 0),
        MouseButton::Right => (CG_EVENT_RIGHT_MOUSE_DRAGGED, 1),
        MouseButton::Middle => (CG_EVENT_OTHER_MOUSE_DRAGGED, 2),
        MouseButton::Back => (CG_EVENT_OTHER_MOUSE_DRAGGED, 3),
        MouseButton::Forward => (CG_EVENT_OTHER_MOUSE_DRAGGED, 4),
    }
}

fn post_marked_event(
    destination: PostingDestination,
    event: CGEventRef,
    marker: i64,
    flags: u64,
) -> Result<(), MacError> {
    if event.is_null() {
        return Err(MacError::NativeEventCreationFailed);
    }
    finish_and_post_event(event, marker, flags, |event| {
        // SAFETY: event remains owned until finish_and_post_event releases it after this call.
        // CurrentProcess resolves this process's PID immediately before posting.
        unsafe {
            match destination {
                PostingDestination::System => CGEventPost(CG_HID_EVENT_TAP, event),
                PostingDestination::CurrentProcess => {
                    CGEventPostToPid(std::process::id() as Pid, event)
                }
            }
        }
    });
    Ok(())
}

struct DiagnosticState {
    counts: PassiveDiagnosticCounts,
    synthetic_marker: i64,
    tap_disabled: bool,
}

unsafe extern "C" fn diagnostic_callback(
    _: CGEventTapProxy,
    event_type: CGEventType,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    if user_info.is_null() {
        return event;
    }
    // SAFETY: user_info originates from a mutable DiagnosticState that remains
    // alive until the run-loop source and tap have both been released.
    let state = unsafe { &mut *user_info.cast::<DiagnosticState>() };
    if tap_disabled(event_type) {
        record_diagnostic_event(&mut state.counts, event_type, None, state.synthetic_marker);
        state.tap_disabled = true;
        // SAFETY: The callback runs on the diagnostic's current run loop, which
        // is stopped only to finish cleanup and return the terminal error.
        unsafe {
            CFRunLoopStop(CFRunLoopGetCurrent());
        }
        return event;
    }
    if event.is_null() {
        return event;
    }
    // SAFETY: event is supplied by Core Graphics for this callback invocation.
    let source_marker = unsafe { CGEventGetIntegerValueField(event, CG_EVENT_SOURCE_USER_DATA) };
    record_diagnostic_event(
        &mut state.counts,
        event_type,
        Some(source_marker),
        state.synthetic_marker,
    );
    event
}

fn record_diagnostic_event(
    counts: &mut PassiveDiagnosticCounts,
    event_type: CGEventType,
    source_marker: Option<i64>,
    synthetic_marker: i64,
) {
    if tap_disabled(event_type) {
        counts.tap_disabled_events = counts.tap_disabled_events.saturating_add(1);
    } else if source_marker == Some(synthetic_marker) {
        counts.synthetic_events_filtered = counts.synthetic_events_filtered.saturating_add(1);
    } else {
        match event_type {
            CG_EVENT_KEY_DOWN | CG_EVENT_KEY_UP | CG_EVENT_FLAGS_CHANGED => {
                counts.keyboard_events = counts.keyboard_events.saturating_add(1);
            }
            CG_EVENT_SCROLL_WHEEL => {
                counts.scroll_events = counts.scroll_events.saturating_add(1);
            }
            CG_EVENT_LEFT_MOUSE_DOWN
            | CG_EVENT_LEFT_MOUSE_UP
            | CG_EVENT_RIGHT_MOUSE_DOWN
            | CG_EVENT_RIGHT_MOUSE_UP
            | CG_EVENT_MOUSE_MOVED
            | CG_EVENT_LEFT_MOUSE_DRAGGED
            | CG_EVENT_RIGHT_MOUSE_DRAGGED
            | CG_EVENT_OTHER_MOUSE_DOWN
            | CG_EVENT_OTHER_MOUSE_UP
            | CG_EVENT_OTHER_MOUSE_DRAGGED => {
                counts.pointer_events = counts.pointer_events.saturating_add(1);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::{
        CFRelease, CG_EVENT_KEY_DOWN, CG_EVENT_MOUSE_MOVED, CG_EVENT_SCROLL_WHEEL,
        CG_EVENT_SOURCE_USER_DATA, CG_MOUSE_EVENT_BUTTON_NUMBER, CG_MOUSE_EVENT_CLICK_STATE,
        CGEventCreateKeyboardEvent, CGEventGetIntegerValueField, CGEventSetIntegerValueField,
        CGPoint, DiagnosticState, MouseButton, PassiveDiagnosticCounts, button_event,
        diagnostic_callback, record_diagnostic_event,
    };

    const SYNTHETIC_MARKER: i64 = 42;

    #[test]
    fn a_posted_press_and_release_carry_the_wire_click_count() {
        for (button, number) in [(MouseButton::Left, 0), (MouseButton::Back, 3)] {
            for click_count in [1, 2, 3, u8::MAX] {
                for is_down in [true, false] {
                    let event =
                        button_event(button, is_down, click_count, CGPoint { x: 1.0, y: 2.0 })
                            .expect("a fixed button event is created");
                    // SAFETY: event is owned here, read, then released; it is never posted.
                    let (state, read_number) = unsafe {
                        let fields = (
                            CGEventGetIntegerValueField(event, CG_MOUSE_EVENT_CLICK_STATE),
                            CGEventGetIntegerValueField(event, CG_MOUSE_EVENT_BUTTON_NUMBER),
                        );
                        CFRelease(event);
                        fields
                    };
                    assert_eq!(state, i64::from(click_count));
                    assert_eq!(read_number, number);
                }
            }
        }
    }

    #[test]
    #[ignore = "desktop Core Graphics callback check"]
    fn callback_filters_a_marked_typed_native_event_without_posting_it() {
        // SAFETY: The test creates a typed keyboard event, owns it, and never posts it.
        let event = unsafe { CGEventCreateKeyboardEvent(ptr::null(), 0, true) };
        assert!(!event.is_null());
        // SAFETY: event is non-null and remains owned through the callback.
        unsafe {
            CGEventSetIntegerValueField(event, CG_EVENT_SOURCE_USER_DATA, SYNTHETIC_MARKER);
        }
        let mut state = DiagnosticState {
            counts: PassiveDiagnosticCounts::default(),
            synthetic_marker: SYNTHETIC_MARKER,
            tap_disabled: false,
        };

        // SAFETY: the callback receives the exact non-null event and live state
        // pointer shape it receives from Core Graphics during a diagnostic.
        let returned = unsafe {
            diagnostic_callback(
                ptr::null_mut(),
                CG_EVENT_KEY_DOWN,
                event,
                (&mut state as *mut DiagnosticState).cast(),
            )
        };

        assert_eq!(returned, event);
        assert_eq!(state.counts.synthetic_events_filtered, 1);
        assert_eq!(state.counts.keyboard_events, 0);
        assert_eq!(state.counts.pointer_events, 0);
        assert_eq!(state.counts.scroll_events, 0);
        // SAFETY: event was created by this test and was not transferred.
        unsafe {
            CFRelease(event);
        }
    }

    #[test]
    fn synthetic_marker_is_filtered_before_category_counts() {
        let mut counts = PassiveDiagnosticCounts::default();
        record_diagnostic_event(
            &mut counts,
            CG_EVENT_KEY_DOWN,
            Some(SYNTHETIC_MARKER),
            SYNTHETIC_MARKER,
        );

        assert_eq!(counts.synthetic_events_filtered, 1);
        assert_eq!(counts.keyboard_events, 0);
        assert_eq!(counts.pointer_events, 0);
        assert_eq!(counts.scroll_events, 0);
    }

    #[test]
    fn diagnostic_counts_only_event_categories() {
        let mut counts = PassiveDiagnosticCounts::default();
        for event_type in [
            CG_EVENT_KEY_DOWN,
            CG_EVENT_MOUSE_MOVED,
            CG_EVENT_SCROLL_WHEEL,
        ] {
            record_diagnostic_event(&mut counts, event_type, Some(0), SYNTHETIC_MARKER);
        }

        assert_eq!(counts.keyboard_events, 1);
        assert_eq!(counts.pointer_events, 1);
        assert_eq!(counts.scroll_events, 1);
        assert_eq!(counts.synthetic_events_filtered, 0);
    }
}

#[cfg(test)]
mod monitor_identity_tests {
    use super::*;

    #[test]
    fn placeholders_and_oversized_numbers_mean_no_identity() {
        assert_eq!(monitor_identity(DISPLAY_VENDOR_UNKNOWN, 0x0a13, 7), None);
        assert_eq!(monitor_identity(0x10ac, DISPLAY_PRODUCT_GENERIC, 7), None);
        assert_eq!(monitor_identity(0x1_0000, 0x0a13, 7), None);
        assert_eq!(monitor_identity(0, 0x0a13, 7), None);
        let identity = monitor_identity(0x10ac, 0x0a13, 0x4c4e_3031).unwrap();
        assert_eq!(
            (identity.vendor, identity.product, identity.serial),
            (0x10ac, 0x0a13, 0x4c4e_3031)
        );
        assert_eq!(
            monitor_identity(0x10ac, 0x0a13, 0).map(|identity| identity.serial),
            Some(0)
        );
    }
}

#[cfg(test)]
mod permission_cache_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    use super::{MacError, PermissionState, TtlCache};

    fn state() -> PermissionState {
        PermissionState {
            accessibility: true,
            listen_events: true,
        }
    }

    #[test]
    fn a_second_query_within_the_ttl_skips_the_probe_and_a_later_one_calls_it_again() {
        let cache = TtlCache::new(Duration::from_millis(20));
        let calls = AtomicUsize::new(0);
        let probe = || -> Result<PermissionState, MacError> {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(state())
        };

        cache.get(probe).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        cache.get(probe).unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a query inside the TTL must not call the probe"
        );

        thread::sleep(Duration::from_millis(40));
        cache.get(probe).unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a query after the TTL elapses must call the probe again"
        );
    }
}
