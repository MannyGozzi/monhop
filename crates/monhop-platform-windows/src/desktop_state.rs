//! Read-only checks for the ordinary interactive desktop; secure desktops are never opened for input.

use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
        TOKEN_QUERY, TokenIntegrityLevel,
    },
    System::{
        StationsAndDesktops::{
            CloseDesktop, DESKTOP_READOBJECTS, GetThreadDesktop, GetUserObjectInformationW,
            OpenInputDesktop, UOI_NAME,
        },
        Threading::{
            GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, OpenProcess,
            OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
    UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId},
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

/// Whether this process may read the foreground's key state: the foreground window is ours, or its
/// process's integrity level is not above ours. `GetAsyncKeyState` reads zero for a foreground it
/// may not read, such as an elevated window, which stays on the Default desktop. Anything that
/// cannot be established, a null foreground or a refused query included, reads as not readable.
pub fn foreground_is_readable() -> bool {
    // SAFETY: reads the current foreground window handle.
    let window = unsafe { GetForegroundWindow() };
    if window.is_null() {
        return false;
    }
    let mut process_id = 0;
    // SAFETY: `window` came from GetForegroundWindow and `process_id` is writable.
    unsafe { GetWindowThreadProcessId(window, &mut process_id) };
    // SAFETY: returns this process's id without side effects.
    let own_id = unsafe { GetCurrentProcessId() };
    if process_id == 0 {
        return false;
    }
    if process_id == own_id {
        return true;
    }
    // SAFETY: requests only limited query rights; the handle is closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return false;
    }
    let theirs = integrity_level(process);
    // SAFETY: closes only the handle OpenProcess returned.
    unsafe { CloseHandle(process) };
    // SAFETY: the pseudo-handle for this process needs no closing.
    let ours = integrity_level(unsafe { GetCurrentProcess() });
    matches!((theirs, ours), (Some(theirs), Some(ours)) if theirs <= ours)
}

/// The mandatory integrity level (a SECURITY_MANDATORY_*_RID) of `process`'s token.
fn integrity_level(process: HANDLE) -> Option<u32> {
    let mut token = std::ptr::null_mut();
    // SAFETY: requests query rights on a valid process handle; the token is closed below.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    // u64 elements keep the label's pointer field aligned.
    let mut buffer = [0_u64; 16];
    let mut needed = 0;
    // SAFETY: the buffer is writable for the length given and outlives the reads below.
    let read = unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            buffer.as_mut_ptr().cast(),
            std::mem::size_of_val(&buffer) as u32,
            &mut needed,
        )
    } != 0;
    let level = read
        .then(|| {
            // SAFETY: a successful TokenIntegrityLevel read fills a TOKEN_MANDATORY_LABEL whose
            // SID lives inside `buffer`.
            let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid };
            // SAFETY: `sid` is the valid integrity SID just read.
            let count = unsafe { *GetSidSubAuthorityCount(sid) };
            // SAFETY: the last sub-authority index is within the count just read.
            (count > 0).then(|| unsafe { *GetSidSubAuthority(sid, u32::from(count - 1)) })
        })
        .flatten();
    // SAFETY: closes only the token OpenProcessToken returned.
    unsafe { CloseHandle(token) };
    level
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
    #[ignore = "reads the live foreground window; expects an unelevated one"]
    fn an_unelevated_foreground_is_readable() {
        assert!(foreground_is_readable());
        // SAFETY: the pseudo-handle for this process needs no closing.
        assert!(integrity_level(unsafe { GetCurrentProcess() }).is_some());
    }
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
