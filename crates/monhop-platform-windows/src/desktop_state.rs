//! Read-only checks for the ordinary interactive desktop; secure desktops are never opened for input.

use windows_sys::Win32::{
    Foundation::HANDLE,
    System::{
        StationsAndDesktops::{
            CloseDesktop, DESKTOP_READOBJECTS, GetThreadDesktop, GetUserObjectInformationW,
            OpenInputDesktop, UOI_NAME,
        },
        Threading::GetCurrentThreadId,
    },
};

pub fn ordinary_desktop_is_active() -> bool {
    // SAFETY: these calls query the calling thread and request only desktop metadata access.
    let thread_desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
    if thread_desktop.is_null() || !is_default(thread_desktop) {
        return false;
    }
    // SAFETY: no switching or input rights are requested, and this handle is always closed below.
    let input_desktop = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) };
    if input_desktop.is_null() {
        return false;
    }
    let ordinary = is_default(input_desktop);
    // SAFETY: only the owned OpenInputDesktop handle is closed; the thread handle is borrowed.
    let closed = unsafe { CloseDesktop(input_desktop) } != 0;
    ordinary && closed
}

fn is_default(desktop: HANDLE) -> bool {
    let mut name = [0_u16; 256];
    let mut needed = 0;
    // SAFETY: the fixed writable buffer size is supplied in bytes for the Unicode desktop name.
    let success = unsafe {
        GetUserObjectInformationW(
            desktop,
            UOI_NAME,
            name.as_mut_ptr().cast(),
            std::mem::size_of_val(&name) as u32,
            &mut needed,
        )
    } != 0;
    success && is_default_name(&name, needed as usize)
}

fn is_default_name(name: &[u16], bytes: usize) -> bool {
    bytes == 16 && name.get(..8) == Some(&[68, 101, 102, 97, 117, 108, 116, 0])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_complete_ordinary_desktop_name_is_accepted() {
        let ordinary = [68, 101, 102, 97, 117, 108, 116, 0];
        assert!(is_default_name(&ordinary, 16));
        assert!(!is_default_name(&ordinary, 14));
        assert!(!is_default_name(
            &[87, 105, 110, 108, 111, 103, 111, 110, 0],
            18
        ));
    }
}
