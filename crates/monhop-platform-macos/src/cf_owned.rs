//! Shared Core Foundation ownership guard and string bridging for network and Wi-Fi lookups.

use std::ffi::{c_char, c_void};

pub(crate) type CFTypeRef = *const c_void;
pub(crate) type CFStringRef = *const c_void;

/// Releases a Core Foundation Create/Copy-owned value exactly once. Tolerates a null value so a
/// caller may store an owned reference that was never populated.
pub(crate) struct CfOwned(pub(crate) CFTypeRef);

impl Drop for CfOwned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: this guard owns a value returned from a Core Foundation Create/Copy API.
            unsafe { CFRelease(self.0) };
        }
    }
}

// SAFETY: These declarations match the documented Core Foundation ownership and string-bridging
// interfaces used by both the network adapter and the Wi-Fi attachment lookup.
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub(crate) fn CFRelease(value: CFTypeRef);
    pub(crate) fn CFEqual(left: CFTypeRef, right: CFTypeRef) -> u8;
    pub(crate) fn CFStringCreateWithCString(
        allocator: *const c_void,
        value: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    pub(crate) fn CFStringGetCString(
        value: CFStringRef,
        buffer: *mut c_char,
        capacity: isize,
        encoding: u32,
    ) -> u8;
}
