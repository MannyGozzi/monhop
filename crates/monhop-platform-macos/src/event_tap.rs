//! Shared Quartz event-tap plumbing for the active capture tap and the passive diagnostic tap.
//!
//! Tap location is fixed to the session event tap; placement, options, event mask, callback and
//! callback state are the only knobs either site varies.

use std::ffi::c_void;
use std::ptr;
use std::time::Duration;

use crate::capture_decode::{
    CG_EVENT_FLAGS_CHANGED, CG_EVENT_KEY_DOWN, CG_EVENT_KEY_UP, CG_EVENT_LEFT_MOUSE_DOWN,
    CG_EVENT_LEFT_MOUSE_DRAGGED, CG_EVENT_LEFT_MOUSE_UP, CG_EVENT_MOUSE_MOVED,
    CG_EVENT_OTHER_MOUSE_DOWN, CG_EVENT_OTHER_MOUSE_DRAGGED, CG_EVENT_OTHER_MOUSE_UP,
    CG_EVENT_RIGHT_MOUSE_DOWN, CG_EVENT_RIGHT_MOUSE_DRAGGED, CG_EVENT_RIGHT_MOUSE_UP,
    CG_EVENT_SCROLL_WHEEL,
};

pub(crate) type CFTypeRef = *const c_void;
pub(crate) type CFAllocatorRef = *const c_void;
pub(crate) type CFStringRef = *const c_void;
pub(crate) type CFRunLoopRef = *const c_void;
pub(crate) type CFRunLoopSourceRef = *const c_void;
pub(crate) type CFMachPortRef = *const c_void;
pub(crate) type CGEventRef = *const c_void;
pub(crate) type CGEventTapProxy = *mut c_void;
pub(crate) type CGEventType = u32;
pub(crate) type CGEventField = u32;
pub(crate) type CGEventMask = u64;
pub(crate) type CGEventTapLocation = u32;
pub(crate) type CGEventTapPlacement = u32;
pub(crate) type CGEventTapOptions = u32;

pub(crate) const CG_HID_EVENT_TAP: CGEventTapLocation = 0;
const CG_SESSION_EVENT_TAP: CGEventTapLocation = 1;
pub(crate) const CG_HEAD_INSERT_EVENT_TAP: CGEventTapPlacement = 0;
pub(crate) const CG_EVENT_TAP_OPTION_DEFAULT: CGEventTapOptions = 0;
pub(crate) const CG_EVENT_TAP_OPTION_LISTEN_ONLY: CGEventTapOptions = 1;
const CG_EVENT_TAP_DISABLED_BY_TIMEOUT: CGEventType = u32::MAX - 1;
const CG_EVENT_TAP_DISABLED_BY_USER_INPUT: CGEventType = u32::MAX;
// kCGEventSourceUserData in the installed CoreGraphics SDK.
pub(crate) const CG_EVENT_SOURCE_USER_DATA: CGEventField = 42;

/// The callback signature `CGEventTapCreate` requires.
pub(crate) type EventTapCallback = Option<
    unsafe extern "C" fn(CGEventTapProxy, CGEventType, CGEventRef, *mut c_void) -> CGEventRef,
>;

// SAFETY: These declarations match the documented Core Graphics event-tap and event-posting
// interfaces and are only ever called from macOS builds.
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapCreate(
        tap: CGEventTapLocation,
        place: CGEventTapPlacement,
        options: CGEventTapOptions,
        events_of_interest: CGEventMask,
        callback: EventTapCallback,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    pub(crate) fn CGEventGetIntegerValueField(event: CGEventRef, field: CGEventField) -> i64;
    pub(crate) fn CGEventSetIntegerValueField(event: CGEventRef, field: CGEventField, value: i64);
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    pub(crate) fn CGEventPost(tap: CGEventTapLocation, event: CGEventRef);
}

// SAFETY: These declarations match the documented Core Foundation ownership and run-loop
// interfaces used to scope each tap to its owning thread.
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub(crate) fn CFRelease(cf: CFTypeRef);
    static kCFRunLoopDefaultMode: CFStringRef;
    fn CFMachPortCreateRunLoopSource(
        allocator: CFAllocatorRef,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    fn CFMachPortInvalidate(port: CFMachPortRef);
    pub(crate) fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(run_loop: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRemoveSource(run_loop: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRunInMode(
        mode: CFStringRef,
        seconds: f64,
        return_after_source_handled: bool,
    ) -> i32;
}

const fn event_mask_bit(event_type: CGEventType) -> CGEventMask {
    1_u64 << event_type
}

/// The fixed keyboard, pointer and scroll event set both the active capture tap and the passive
/// diagnostic tap listen for.
pub(crate) const fn full_input_event_mask() -> CGEventMask {
    event_mask_bit(CG_EVENT_LEFT_MOUSE_DOWN)
        | event_mask_bit(CG_EVENT_LEFT_MOUSE_UP)
        | event_mask_bit(CG_EVENT_RIGHT_MOUSE_DOWN)
        | event_mask_bit(CG_EVENT_RIGHT_MOUSE_UP)
        | event_mask_bit(CG_EVENT_MOUSE_MOVED)
        | event_mask_bit(CG_EVENT_LEFT_MOUSE_DRAGGED)
        | event_mask_bit(CG_EVENT_RIGHT_MOUSE_DRAGGED)
        | event_mask_bit(CG_EVENT_KEY_DOWN)
        | event_mask_bit(CG_EVENT_KEY_UP)
        | event_mask_bit(CG_EVENT_FLAGS_CHANGED)
        | event_mask_bit(CG_EVENT_SCROLL_WHEEL)
        | event_mask_bit(CG_EVENT_OTHER_MOUSE_DOWN)
        | event_mask_bit(CG_EVENT_OTHER_MOUSE_UP)
        | event_mask_bit(CG_EVENT_OTHER_MOUSE_DRAGGED)
}

/// True for the two tap callback event types that mean the tap itself was disabled.
pub(crate) const fn tap_disabled(event_type: CGEventType) -> bool {
    matches!(
        event_type,
        CG_EVENT_TAP_DISABLED_BY_TIMEOUT | CG_EVENT_TAP_DISABLED_BY_USER_INPUT
    )
}

/// Tags and hands an owned, non-null event to `post` for delivery, then releases it. Callers
/// check nullability and permission before calling this.
pub(crate) fn finish_and_post_event(
    event: CGEventRef,
    marker: i64,
    flags: u64,
    post: impl FnOnce(CGEventRef),
) {
    // SAFETY: event is non-null and owned by the caller until this function releases it below.
    unsafe {
        CGEventSetIntegerValueField(event, CG_EVENT_SOURCE_USER_DATA, marker);
        CGEventSetFlags(event, flags);
    }
    post(event);
    // SAFETY: event was owned by the caller and has now been handed to `post`; this function is
    // the single release point.
    unsafe { CFRelease(event) };
}

pub(crate) enum EventTapInstallError {
    TapUnavailable,
    SourceUnavailable,
}

/// An installed event tap, registered as a run-loop source on the thread that installs it.
pub(crate) struct EventTap {
    tap: CFMachPortRef,
    source: CFRunLoopSourceRef,
    run_loop: CFRunLoopRef,
}

impl EventTap {
    pub(crate) fn install(
        placement: CGEventTapPlacement,
        options: CGEventTapOptions,
        mask: CGEventMask,
        callback: EventTapCallback,
        user_info: *mut c_void,
    ) -> Result<Self, EventTapInstallError> {
        // SAFETY: mask, options and placement are caller-selected fixed values, callback is a
        // valid extern "C" function pointer, and user_info remains live for the tap's lifetime.
        let tap = unsafe {
            CGEventTapCreate(
                CG_SESSION_EVENT_TAP,
                placement,
                options,
                mask,
                callback,
                user_info,
            )
        };
        if tap.is_null() {
            return Err(EventTapInstallError::TapUnavailable);
        }
        // SAFETY: tap is owned here after a successful create; it is released below on failure.
        let source = unsafe { CFMachPortCreateRunLoopSource(ptr::null(), tap, 0) };
        if source.is_null() {
            // SAFETY: invalidating and releasing this owned port prevents a callback from firing
            // after this failure.
            unsafe {
                CFMachPortInvalidate(tap);
                CFRelease(tap);
            }
            return Err(EventTapInstallError::SourceUnavailable);
        }
        // SAFETY: this is the calling thread's current run loop; the source is removed by close
        // before either source or tap is released.
        let run_loop = unsafe { CFRunLoopGetCurrent() };
        // SAFETY: all arguments remain live until close runs on this same thread.
        unsafe {
            CFRunLoopAddSource(run_loop, source, kCFRunLoopDefaultMode);
        }
        Ok(Self {
            tap,
            source,
            run_loop,
        })
    }

    /// Services this tap's run loop for up to `slice`, delivering at most one queued source.
    pub(crate) fn run_once(&self, slice: Duration) {
        // SAFETY: the source remains registered for the tap's lifetime; a bounded timeout lets
        // the caller's own loop keep polling other state without a callback waker.
        unsafe {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, slice.as_secs_f64(), true);
        }
    }

    /// Stops new suppression from this tap. Callers that need this call it before `close`.
    pub(crate) fn disable(&self) {
        // SAFETY: self.tap is a live mach port owned by this instance until close releases it.
        unsafe { CGEventTapEnable(self.tap, false) };
    }

    pub(crate) fn close(&mut self) {
        // SAFETY: close runs once on the run-loop owner. Removal ends delivery before the mach
        // port and run-loop source are released.
        unsafe {
            CFRunLoopRemoveSource(self.run_loop, self.source, kCFRunLoopDefaultMode);
            CFMachPortInvalidate(self.tap);
            CFRelease(self.source);
            CFRelease(self.tap);
        }
    }
}
