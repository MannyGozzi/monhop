//! Window-server foreground guard for an explicit self-process trial receiver.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrialWindow {
    native_window_id: u32,
    process_id: i32,
}

/// One window-server reading. Unknown means the list could not be read, not that we are behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForegroundSample {
    Front,
    Behind,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrialWindowError {
    UnsupportedPlatform,
    InvalidWindow,
    NotForeground,
    WindowServerUnavailable,
}

impl fmt::Display for TrialWindowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedPlatform => "macOS trial windows are unavailable on this platform",
            Self::InvalidWindow => "The controlled trial window ID is invalid",
            Self::NotForeground => "Bring the controlled trial window to the foreground first",
            Self::WindowServerUnavailable => "The macOS window server is unavailable",
        })
    }
}

impl std::error::Error for TrialWindowError {}

impl TrialWindow {
    #[cfg(target_os = "macos")]
    pub fn for_current_process(native_window_id: usize) -> Result<Self, TrialWindowError> {
        let native_window_id = u32::try_from(native_window_id)
            .ok()
            .filter(|id| *id != 0)
            .ok_or(TrialWindowError::InvalidWindow)?;
        let process_id =
            i32::try_from(std::process::id()).map_err(|_| TrialWindowError::InvalidWindow)?;
        let window = Self {
            native_window_id,
            process_id,
        };
        match macos::sample(window) {
            ForegroundSample::Front => Ok(window),
            ForegroundSample::Behind => Err(TrialWindowError::NotForeground),
            ForegroundSample::Unknown => Err(TrialWindowError::WindowServerUnavailable),
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub fn for_current_process(_native_window_id: usize) -> Result<Self, TrialWindowError> {
        Err(TrialWindowError::UnsupportedPlatform)
    }

    #[cfg(target_os = "macos")]
    pub fn foreground_sample(&self) -> ForegroundSample {
        macos::sample(*self)
    }

    #[cfg(not(target_os = "macos"))]
    pub fn foreground_sample(&self) -> ForegroundSample {
        let _ = self;
        ForegroundSample::Behind
    }

    pub fn is_foreground(&self) -> bool {
        matches!(self.foreground_sample(), ForegroundSample::Front)
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{ForegroundSample, TrialWindow};
    use std::{ffi::c_void, ptr};

    type CFArrayRef = *const c_void;
    type CFDictionaryRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFTypeRef = *const c_void;
    type CFTypeID = usize;
    type CGWindowID = u32;
    type CFIndex = isize;

    const ON_SCREEN: u32 = 1 << 0;
    const EXCLUDE_DESKTOP: u32 = 1 << 4;
    const MAX_WINDOW_SCAN: CFIndex = 4_096;
    const SINT32: isize = 3;
    const FLOAT64: isize = 6;

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: CGWindowID) -> CFArrayRef;
        static kCGWindowAlpha: CFStringRef;
        static kCGWindowIsOnscreen: CFStringRef;
        static kCGWindowLayer: CFStringRef;
        static kCGWindowNumber: CFStringRef;
        static kCGWindowOwnerPID: CFStringRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFArrayGetCount(array: CFArrayRef) -> CFIndex;
        fn CFArrayGetTypeID() -> CFTypeID;
        fn CFArrayGetValueAtIndex(array: CFArrayRef, index: CFIndex) -> *const c_void;
        fn CFBooleanGetValue(boolean: CFTypeRef) -> u8;
        fn CFBooleanGetTypeID() -> CFTypeID;
        fn CFDictionaryGetTypeID() -> CFTypeID;
        fn CFDictionaryGetValueIfPresent(
            dictionary: CFDictionaryRef,
            key: *const c_void,
            value: *mut *const c_void,
        ) -> u8;
        fn CFGetTypeID(value: CFTypeRef) -> CFTypeID;
        fn CFNumberGetValue(number: CFTypeRef, number_type: isize, value: *mut c_void) -> u8;
        fn CFNumberGetTypeID() -> CFTypeID;
        fn CFRelease(value: CFTypeRef);
    }

    struct WindowList(CFArrayRef);

    impl Drop for WindowList {
        fn drop(&mut self) {
            // SAFETY: the list is the retained Core Foundation result returned by this instance's query.
            unsafe { CFRelease(self.0) };
        }
    }

    pub(super) fn sample(target: TrialWindow) -> ForegroundSample {
        // SAFETY: this only copies WindowServer metadata for the active GUI session.
        let list = unsafe { CGWindowListCopyWindowInfo(ON_SCREEN | EXCLUDE_DESKTOP, 0) };
        if list.is_null() {
            return ForegroundSample::Unknown;
        }
        let list = WindowList(list);
        if !has_type(list.0, array_type()) {
            return ForegroundSample::Unknown;
        }
        // SAFETY: list passed Core Foundation's CFArray type check above.
        let count = unsafe { CFArrayGetCount(list.0) };
        if !(0..=MAX_WINDOW_SCAN).contains(&count) {
            return ForegroundSample::Unknown;
        }
        let mut skipped = false;
        let mut front = None;
        for index in 0..count {
            // SAFETY: index is bounded by the checked CFArray count.
            let dictionary = unsafe { CFArrayGetValueAtIndex(list.0, index) };
            let entry = (!dictionary.is_null())
                .then(|| window_info(dictionary))
                .flatten();
            let Some(window) = entry else {
                skipped = true;
                continue;
            };
            if window.layer == 0 && window.onscreen && window.alpha > 0.0 {
                front =
                    Some(window.id == target.native_window_id && window.owner == target.process_id);
                break;
            }
        }
        scan_result(front, skipped)
    }

    /// An entry that could not be read may have been any window, ours or the one above it.
    fn scan_result(front: Option<bool>, skipped: bool) -> ForegroundSample {
        if skipped {
            return ForegroundSample::Unknown;
        }
        match front {
            Some(true) => ForegroundSample::Front,
            _ => ForegroundSample::Behind,
        }
    }

    struct WindowInfo {
        id: u32,
        owner: i32,
        layer: i32,
        alpha: f64,
        onscreen: bool,
    }

    fn window_info(dictionary: *const c_void) -> Option<WindowInfo> {
        if !has_type(dictionary, dictionary_type()) {
            return None;
        }
        let keys = window_keys()?;
        Some(WindowInfo {
            id: u32::try_from(signed_number(dictionary, keys.number)?).ok()?,
            owner: signed_number(dictionary, keys.owner)?,
            layer: signed_number(dictionary, keys.layer)?,
            alpha: float_number(dictionary, keys.alpha)?,
            // kCGWindowIsOnscreen is optional, and this list was already queried on-screen only.
            onscreen: boolean(dictionary, keys.onscreen).unwrap_or(true),
        })
    }

    fn signed_number(dictionary: *const c_void, key: CFStringRef) -> Option<i32> {
        let value = value(dictionary, key)?;
        if !has_type(value, number_type()) {
            return None;
        }
        let mut result = 0_i32;
        // SAFETY: value passed CFNumber validation and result is a writable i32 buffer.
        (unsafe { CFNumberGetValue(value, SINT32, (&mut result as *mut i32).cast()) } != 0)
            .then_some(result)
    }

    fn float_number(dictionary: *const c_void, key: CFStringRef) -> Option<f64> {
        let value = value(dictionary, key)?;
        if !has_type(value, number_type()) {
            return None;
        }
        let mut result = 0_f64;
        // SAFETY: value passed CFNumber validation and result is a writable f64 buffer.
        (unsafe { CFNumberGetValue(value, FLOAT64, (&mut result as *mut f64).cast()) } != 0)
            .then_some(result)
    }

    fn boolean(dictionary: *const c_void, key: CFStringRef) -> Option<bool> {
        let value = value(dictionary, key)?;
        if !has_type(value, boolean_type()) {
            return None;
        }
        // SAFETY: value passed Core Foundation's CFBoolean type check.
        Some(unsafe { CFBooleanGetValue(value) } != 0)
    }

    fn value(dictionary: *const c_void, key: CFStringRef) -> Option<CFTypeRef> {
        let mut result: CFTypeRef = ptr::null();
        // SAFETY: dictionary passed CFDictionary validation and result is writable for this synchronous query.
        (unsafe { CFDictionaryGetValueIfPresent(dictionary, key, &mut result) } != 0
            && !result.is_null())
        .then_some(result)
    }

    struct WindowKeys {
        alpha: CFStringRef,
        onscreen: CFStringRef,
        layer: CFStringRef,
        number: CFStringRef,
        owner: CFStringRef,
    }

    fn window_keys() -> Option<WindowKeys> {
        // SAFETY: these are immutable Core Graphics CFString constants with process-long lifetimes.
        let keys = unsafe {
            WindowKeys {
                alpha: kCGWindowAlpha,
                onscreen: kCGWindowIsOnscreen,
                layer: kCGWindowLayer,
                number: kCGWindowNumber,
                owner: kCGWindowOwnerPID,
            }
        };
        (!keys.alpha.is_null()
            && !keys.onscreen.is_null()
            && !keys.layer.is_null()
            && !keys.number.is_null()
            && !keys.owner.is_null())
        .then_some(keys)
    }

    fn has_type(value: CFTypeRef, expected: CFTypeID) -> bool {
        if value.is_null() {
            return false;
        }
        // SAFETY: values come from retained Core Foundation collections or framework constants.
        unsafe { CFGetTypeID(value) == expected }
    }

    fn array_type() -> CFTypeID {
        // SAFETY: Core Foundation returns this static type identifier without dereferencing caller memory.
        unsafe { CFArrayGetTypeID() }
    }

    fn boolean_type() -> CFTypeID {
        // SAFETY: Core Foundation returns this static type identifier without dereferencing caller memory.
        unsafe { CFBooleanGetTypeID() }
    }

    fn dictionary_type() -> CFTypeID {
        // SAFETY: Core Foundation returns this static type identifier without dereferencing caller memory.
        unsafe { CFDictionaryGetTypeID() }
    }

    fn number_type() -> CFTypeID {
        // SAFETY: Core Foundation returns this static type identifier without dereferencing caller memory.
        unsafe { CFNumberGetTypeID() }
    }

    #[cfg(test)]
    mod tests {
        use super::{ForegroundSample, scan_result};

        #[test]
        fn an_unreadable_entry_makes_the_sample_unknown_rather_than_a_miss() {
            assert_eq!(scan_result(None, true), ForegroundSample::Unknown);
            assert_eq!(scan_result(Some(false), true), ForegroundSample::Unknown);
            assert_eq!(scan_result(Some(true), true), ForegroundSample::Unknown);
        }

        #[test]
        fn a_complete_scan_answers_front_or_behind() {
            assert_eq!(scan_result(Some(true), false), ForegroundSample::Front);
            assert_eq!(scan_result(Some(false), false), ForegroundSample::Behind);
            assert_eq!(scan_result(None, false), ForegroundSample::Behind);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn invalid_ids_are_rejected_before_window_server_lookup() {
        assert_eq!(u32::try_from(usize::MAX).ok().filter(|id| *id != 0), None);
    }
}
