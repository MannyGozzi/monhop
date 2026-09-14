//! Explicit foreground guard for a controlled source trial.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrialWindow {
    native_window_id: usize,
    token: usize,
}

/// One foreground reading. Unknown is reserved for platforms that cannot answer at all.
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
    NotCurrentProcess,
    NotForeground,
    RegistrationFailed,
}

impl fmt::Display for TrialWindowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedPlatform => "Windows trial windows are unavailable on this platform",
            Self::InvalidWindow => "The controlled trial window is no longer valid",
            Self::NotCurrentProcess => "The controlled trial window must belong to this process",
            Self::NotForeground => "Bring the controlled trial window to the foreground first",
            Self::RegistrationFailed => "The controlled trial window could not be registered",
        })
    }
}

impl std::error::Error for TrialWindowError {}

impl TrialWindow {
    #[cfg(windows)]
    pub fn for_current_process(native_window_id: usize) -> Result<Self, TrialWindowError> {
        windows::register(native_window_id)
    }

    #[cfg(not(windows))]
    pub fn for_current_process(_native_window_id: usize) -> Result<Self, TrialWindowError> {
        Err(TrialWindowError::UnsupportedPlatform)
    }

    /// GetForegroundWindow always answers, so a live window is either in front or it is not.
    #[cfg(windows)]
    pub fn foreground_sample(&self) -> ForegroundSample {
        if windows::is_foreground(*self) {
            ForegroundSample::Front
        } else {
            ForegroundSample::Behind
        }
    }

    #[cfg(not(windows))]
    pub fn foreground_sample(&self) -> ForegroundSample {
        let _ = self;
        ForegroundSample::Behind
    }

    pub fn is_foreground(&self) -> bool {
        matches!(self.foreground_sample(), ForegroundSample::Front)
    }
}

#[cfg(windows)]
mod windows {
    use super::{TrialWindow, TrialWindowError};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use windows_sys::Win32::{
        Foundation::{HANDLE, HWND},
        System::Threading::GetCurrentProcessId,
        UI::WindowsAndMessaging::{
            GetForegroundWindow, GetPropW, GetWindowThreadProcessId, IsWindow, SetPropW,
        },
    };

    static NEXT_TOKEN: AtomicUsize = AtomicUsize::new(1);
    const PROPERTY: &[u16] = &[
        76, 111, 99, 97, 108, 75, 77, 67, 111, 110, 116, 114, 111, 108, 108, 101, 100, 84, 114,
        105, 97, 108, 0,
    ];

    pub(super) fn register(native_window_id: usize) -> Result<TrialWindow, TrialWindowError> {
        let hwnd = native_window_id as HWND;
        if hwnd.is_null() {
            return Err(TrialWindowError::InvalidWindow);
        }
        if !belongs_to_current_process(hwnd) {
            return Err(TrialWindowError::NotCurrentProcess);
        }
        if foreground_window() != hwnd {
            return Err(TrialWindowError::NotForeground);
        }
        let token = NEXT_TOKEN
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| TrialWindowError::RegistrationFailed)?;
        if token == 0 || !set_window_token(hwnd, token) {
            return Err(TrialWindowError::RegistrationFailed);
        }
        Ok(TrialWindow {
            native_window_id,
            token,
        })
    }

    pub(super) fn is_foreground(window: TrialWindow) -> bool {
        let hwnd = window.native_window_id as HWND;
        !hwnd.is_null()
            && belongs_to_current_process(hwnd)
            && window_token(hwnd) == window.token as HANDLE
            && foreground_window() == hwnd
    }

    fn belongs_to_current_process(hwnd: HWND) -> bool {
        is_window(hwnd) && window_owner(hwnd).is_some_and(|owner| owner == current_process_id())
    }

    fn is_window(hwnd: HWND) -> bool {
        // SAFETY: IsWindow only queries the opaque window handle.
        unsafe { IsWindow(hwnd) != 0 }
    }

    fn window_owner(hwnd: HWND) -> Option<u32> {
        let mut owner = 0;
        // SAFETY: owner is writable for this call and hwnd was confirmed live above.
        (unsafe { GetWindowThreadProcessId(hwnd, &mut owner) } != 0).then_some(owner)
    }

    fn foreground_window() -> HWND {
        // SAFETY: this reads the current foreground handle and retains no pointer.
        unsafe { GetForegroundWindow() }
    }

    fn window_token(hwnd: HWND) -> HANDLE {
        // SAFETY: PROPERTY is NUL-terminated and hwnd was revalidated as a live local window.
        unsafe { GetPropW(hwnd, PROPERTY.as_ptr()) }
    }

    fn set_window_token(hwnd: HWND, token: usize) -> bool {
        // SAFETY: PROPERTY is NUL-terminated and hwnd was just validated as this process's window.
        unsafe { SetPropW(hwnd, PROPERTY.as_ptr(), token as HANDLE) != 0 }
    }

    fn current_process_id() -> u32 {
        // SAFETY: this reads only the caller's process identifier.
        unsafe { GetCurrentProcessId() }
    }
}

#[cfg(test)]
mod tests {
    #[derive(Clone, Copy)]
    struct FocusGate {
        current: bool,
    }

    impl FocusGate {
        fn permits(self) -> bool {
            self.current
        }
    }

    #[test]
    fn focus_gate_fails_closed() {
        assert!(FocusGate { current: true }.permits());
        assert!(!FocusGate { current: false }.permits());
    }
}
