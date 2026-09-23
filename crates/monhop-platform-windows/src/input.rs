//! Explicit Windows input diagnostics and injection. Nothing runs at crate load.

use std::{fmt, time::Duration};

use monhop_core::{
    HidUsage, MouseButton,
    clicks::{DOUBLE_CLICK_SLOP, SINGLE_CLICK},
};

use crate::keymap::KeyMapError;

/// Marker copied into native input metadata so MonHop can ignore its own injected events.
pub const MONHOP_INJECTED_MARKER: usize = 0x4c4b_4d31;
pub const MAX_CAPTURE_DURATION: Duration = Duration::from_secs(30);

#[cfg(windows)]
static RAW_CAPTURE_STATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

#[cfg(windows)]
const RAW_CAPTURE_IDLE: u8 = 0;
#[cfg(windows)]
const RAW_CAPTURE_CLAIMED: u8 = 1;
#[cfg(windows)]
const RAW_CAPTURE_POISONED: u8 = 2;

#[cfg(windows)]
pub(crate) struct RawCaptureOwnership;

#[cfg(windows)]
impl RawCaptureOwnership {
    pub(crate) fn claim() -> Result<Self, InputError> {
        use std::sync::atomic::Ordering;
        RAW_CAPTURE_STATE
            .compare_exchange(
                RAW_CAPTURE_IDLE,
                RAW_CAPTURE_CLAIMED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| Self)
            .map_err(|_| InputError::CaptureAlreadyActive)
    }

    /// Retains the active claim when native raw-input rollback could not restore a clean state.
    pub(crate) fn poison(&mut self) {
        RAW_CAPTURE_STATE.store(RAW_CAPTURE_POISONED, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(windows)]
impl Drop for RawCaptureOwnership {
    fn drop(&mut self) {
        let _ = RAW_CAPTURE_STATE.compare_exchange(
            RAW_CAPTURE_CLAIMED,
            RAW_CAPTURE_IDLE,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        );
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureStats {
    pub keyboard_events: u64,
    pub mouse_events: u64,
    pub mouse_motion_events: u64,
    pub button_events: u64,
    pub scroll_events: u64,
    pub filtered_injected_events: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RawMouseSummary {
    pub motion: bool,
    pub button_events: u8,
    pub scroll_events: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtualDesktop {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

impl VirtualDesktop {
    pub fn new(left: i32, top: i32, width: u32, height: u32) -> Result<Self, InputError> {
        if width == 0 || height == 0 {
            return Err(InputError::InvalidVirtualDesktop);
        }
        let desktop = Self {
            left,
            top,
            width,
            height,
        };
        desktop.max_x().ok_or(InputError::InvalidVirtualDesktop)?;
        desktop.max_y().ok_or(InputError::InvalidVirtualDesktop)?;
        Ok(desktop)
    }

    pub fn contains(self, x: i32, y: i32) -> bool {
        let Some(max_x) = self.max_x() else {
            return false;
        };
        let Some(max_y) = self.max_y() else {
            return false;
        };
        i64::from(x) >= i64::from(self.left)
            && i64::from(x) <= max_x
            && i64::from(y) >= i64::from(self.top)
            && i64::from(y) <= max_y
    }

    fn max_x(self) -> Option<i64> {
        i64::from(self.left).checked_add(i64::from(self.width) - 1)
    }

    fn max_y(self) -> Option<i64> {
        i64::from(self.top).checked_add(i64::from(self.height) - 1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InjectionOperation {
    Key {
        usage: HidUsage,
        pressed: bool,
    },
    /// `click_count` is the source's multi-click count for this press or release.
    Button {
        button: MouseButton,
        pressed: bool,
        click_count: u8,
    },
    RelativeMove {
        dx: i32,
        dy: i32,
    },
    AbsoluteMove {
        x: i32,
        y: i32,
        desktop: VirtualDesktop,
    },
    Scroll {
        vertical: i32,
        horizontal: i32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputError {
    UnsupportedPlatform,
    InvalidCaptureDuration,
    InvalidVirtualDesktop,
    PointOutsideVirtualDesktop,
    ZeroRelativeMotion,
    EmptyScroll,
    UnsupportedKey(KeyMapError),
    KeyNotHeld,
    ButtonNotHeld,
    WindowsApi {
        operation: &'static str,
        code: u32,
    },
    RawInputReadFailed,
    CaptureAlreadyActive,
    TooManyRawInputRegistrations {
        count: u32,
    },
    SendInputBlockedOrFailed {
        attempted: usize,
        code: u32,
    },
    PartialSendInput {
        sent: usize,
        attempted: usize,
        code: u32,
    },
}

impl fmt::Display for InputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                formatter.write_str("Windows input support is unavailable")
            }
            Self::InvalidCaptureDuration => formatter
                .write_str("capture duration must be between one millisecond and thirty seconds"),
            Self::InvalidVirtualDesktop => {
                formatter.write_str("virtual desktop bounds are invalid")
            }
            Self::PointOutsideVirtualDesktop => {
                formatter.write_str("absolute point is outside the virtual desktop")
            }
            Self::ZeroRelativeMotion => {
                formatter.write_str("relative motion must have a nonzero delta")
            }
            Self::EmptyScroll => formatter.write_str("scroll operation must have a nonzero delta"),
            Self::UnsupportedKey(_) => {
                formatter.write_str("HID usage has no supported Windows set-1 mapping")
            }
            Self::KeyNotHeld => {
                formatter.write_str("key release was requested for an untracked key")
            }
            Self::ButtonNotHeld => {
                formatter.write_str("button release was requested for an untracked button")
            }
            Self::WindowsApi { operation, code } => {
                write!(
                    formatter,
                    "Windows input operation {operation} failed with code {code}"
                )
            }
            Self::RawInputReadFailed => {
                formatter.write_str("Windows could not read a Raw Input record")
            }
            Self::CaptureAlreadyActive => {
                formatter.write_str("another MonHop input capture is active")
            }
            Self::TooManyRawInputRegistrations { count } => {
                write!(
                    formatter,
                    "Windows reported too many Raw Input registrations ({count})"
                )
            }
            Self::SendInputBlockedOrFailed { attempted, code } => {
                write!(
                    formatter,
                    "SendInput accepted none of {attempted} records (code {code})"
                )
            }
            Self::PartialSendInput {
                sent,
                attempted,
                code,
            } => {
                write!(
                    formatter,
                    "SendInput accepted {sent} of {attempted} records (code {code})"
                )
            }
        }
    }
}

impl std::error::Error for InputError {}

/// Converts one physical virtual-desktop pixel point to SendInput's 0..=65535 coordinates.
pub fn absolute_send_input_coordinates(
    desktop: VirtualDesktop,
    x: i32,
    y: i32,
) -> Result<(i32, i32), InputError> {
    if !desktop.contains(x, y) {
        return Err(InputError::PointOutsideVirtualDesktop);
    }
    Ok((
        normalize_absolute_axis(x, desktop.left, desktop.width),
        normalize_absolute_axis(y, desktop.top, desktop.height),
    ))
}

/// Where injection last put the cursor and each button's last press, so a forwarded multi-click
/// lands inside Windows' own double-click rectangle, which is 4 px wide by default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClickAnchors {
    cursor: Option<(i32, i32)>,
    presses: [Option<(i32, i32)>; MouseButton::ALL.len()],
    held: Option<SnapHold>,
}

/// A snapped press still down. The source never learns of the snap, so its moves stay unsnapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnapHold {
    button: MouseButton,
    snapped: (i32, i32),
    source: (i32, i32),
}

impl ClickAnchors {
    pub const fn new() -> Self {
        Self {
            cursor: None,
            presses: [None; MouseButton::ALL.len()],
            held: None,
        }
    }

    pub fn moved_to(&mut self, x: i32, y: i32) {
        self.cursor = Some((x, y));
    }

    pub fn forget_cursor(&mut self) {
        self.cursor = None;
    }

    /// The previous press to inject this one on, when the source counted a multi-click nearby.
    pub fn press_target(&self, button: MouseButton, click_count: u8) -> Option<(i32, i32)> {
        let cursor = self.cursor?;
        let previous = self.presses[button.index()]?;
        (click_count > SINGLE_CLICK && cursor != previous && within_slop(cursor, previous))
            .then_some(previous)
    }

    /// Records a press injected at `target`, or at the cursor without one.
    pub fn pressed(&mut self, button: MouseButton, target: Option<(i32, i32)>) {
        if let Some(snapped) = target {
            self.held = self.cursor.map(|source| SnapHold {
                button,
                snapped,
                source,
            });
            self.cursor = target;
        }
        self.presses[button.index()] = self.cursor;
    }

    /// Where to move right after this button's release: back to the source's own position when a
    /// snap held the cursor away from it.
    pub fn release_target(&self, button: MouseButton) -> Option<(i32, i32)> {
        self.held
            .filter(|hold| hold.button == button && hold.source != hold.snapped)
            .map(|hold| hold.source)
    }

    pub fn released(&mut self, button: MouseButton) {
        if self.held.is_some_and(|hold| hold.button == button) {
            self.held = None;
        }
    }

    /// Where a move to `(x, y)` lands: on a held snapped press while it stays within the slop, so
    /// the source's unsnapped position cannot drag it; the first move beyond resumes following.
    pub fn move_target(&mut self, x: i32, y: i32) -> (i32, i32) {
        match &mut self.held {
            Some(hold) if within_slop((x, y), hold.snapped) => {
                hold.source = (x, y);
                hold.snapped
            }
            _ => {
                self.held = None;
                (x, y)
            }
        }
    }
}

fn within_slop(from: (i32, i32), to: (i32, i32)) -> bool {
    let near = |from: i32, to: i32| {
        (i64::from(from) - i64::from(to)).abs() <= i64::from(DOUBLE_CLICK_SLOP)
    };
    near(from.0, to.0) && near(from.1, to.1)
}

/// Summarizes a Raw Input mouse event without retaining its payload.
pub fn summarize_raw_mouse(
    button_flags: u16,
    delta_x: i32,
    delta_y: i32,
    extra_information: u32,
) -> Option<RawMouseSummary> {
    if is_monhop_injected(extra_information) {
        return None;
    }

    let button_mask = RI_MOUSE_BUTTON_1_DOWN
        | RI_MOUSE_BUTTON_1_UP
        | RI_MOUSE_BUTTON_2_DOWN
        | RI_MOUSE_BUTTON_2_UP
        | RI_MOUSE_BUTTON_3_DOWN
        | RI_MOUSE_BUTTON_3_UP
        | RI_MOUSE_BUTTON_4_DOWN
        | RI_MOUSE_BUTTON_4_UP
        | RI_MOUSE_BUTTON_5_DOWN
        | RI_MOUSE_BUTTON_5_UP;
    let scroll_events = u8::from(button_flags & RI_MOUSE_WHEEL != 0)
        + u8::from(button_flags & RI_MOUSE_HWHEEL != 0);
    Some(RawMouseSummary {
        motion: delta_x != 0 || delta_y != 0,
        button_events: (button_flags & button_mask).count_ones() as u8,
        scroll_events,
    })
}

pub fn is_monhop_injected(extra_information: u32) -> bool {
    extra_information == MONHOP_INJECTED_MARKER as u32
}

/// Captures only bounded aggregate Raw Input counts. It installs no hooks or suppression.
#[cfg(windows)]
pub fn capture_counts(duration: Duration) -> Result<CaptureStats, InputError> {
    windows::capture_counts(duration)
}

#[cfg(not(windows))]
pub fn capture_counts(_duration: Duration) -> Result<CaptureStats, InputError> {
    Err(InputError::UnsupportedPlatform)
}

#[cfg(windows)]
pub use windows::Injector;

#[cfg(not(windows))]
#[derive(Default)]
pub struct Injector;

#[cfg(not(windows))]
impl Injector {
    pub const fn new() -> Self {
        Self
    }

    pub fn inject(&mut self, _operation: InjectionOperation) -> Result<(), InputError> {
        Err(InputError::UnsupportedPlatform)
    }

    pub fn release_all(&mut self) -> Result<(), InputError> {
        Err(InputError::UnsupportedPlatform)
    }

    pub const fn is_key_held(&self, _usage: HidUsage) -> bool {
        false
    }

    pub const fn is_button_held(&self, _button: MouseButton) -> bool {
        false
    }
}

fn normalize_absolute_axis(value: i32, origin: i32, extent: u32) -> i32 {
    if extent == 1 {
        return 0;
    }
    let relative = i64::from(value) - i64::from(origin);
    let normalized = relative * 65_535 / (i64::from(extent) - 1);
    normalized as i32
}

#[cfg(windows)]
impl CaptureStats {
    fn record_keyboard(&mut self, extra_information: u32) {
        if is_monhop_injected(extra_information) {
            self.filtered_injected_events = self.filtered_injected_events.saturating_add(1);
        } else {
            self.keyboard_events = self.keyboard_events.saturating_add(1);
        }
    }

    fn record_mouse(
        &mut self,
        button_flags: u16,
        delta_x: i32,
        delta_y: i32,
        extra_information: u32,
    ) {
        let Some(summary) = summarize_raw_mouse(button_flags, delta_x, delta_y, extra_information)
        else {
            self.filtered_injected_events = self.filtered_injected_events.saturating_add(1);
            return;
        };

        self.mouse_events = self.mouse_events.saturating_add(1);
        self.mouse_motion_events = self
            .mouse_motion_events
            .saturating_add(u64::from(summary.motion));
        self.button_events = self
            .button_events
            .saturating_add(u64::from(summary.button_events));
        self.scroll_events = self
            .scroll_events
            .saturating_add(u64::from(summary.scroll_events));
    }
}

const RI_MOUSE_BUTTON_1_DOWN: u16 = 0x0001;
const RI_MOUSE_BUTTON_1_UP: u16 = 0x0002;
const RI_MOUSE_BUTTON_2_DOWN: u16 = 0x0004;
const RI_MOUSE_BUTTON_2_UP: u16 = 0x0008;
const RI_MOUSE_BUTTON_3_DOWN: u16 = 0x0010;
const RI_MOUSE_BUTTON_3_UP: u16 = 0x0020;
const RI_MOUSE_BUTTON_4_DOWN: u16 = 0x0040;
const RI_MOUSE_BUTTON_4_UP: u16 = 0x0080;
const RI_MOUSE_BUTTON_5_DOWN: u16 = 0x0100;
const RI_MOUSE_BUTTON_5_UP: u16 = 0x0200;
const RI_MOUSE_WHEEL: u16 = 0x0400;
const RI_MOUSE_HWHEEL: u16 = 0x0800;

#[cfg(windows)]
mod windows {
    use std::{
        cell::UnsafeCell,
        collections::BTreeSet,
        mem::size_of,
        ptr,
        time::{Duration, Instant},
    };

    use crate::keymap::set1_from_hid_usage;
    use monhop_core::{HidUsage, MouseButton};
    use windows_sys::Win32::{
        Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{
                INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
                KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
                MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
                MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
                MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP,
                MOUSEINPUT, SendInput,
            },
            Input::{
                GetRawInputData, GetRegisteredRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE,
                RID_INPUT, RIDEV_INPUTSINK, RIDEV_REMOVE, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
                RegisterRawInputDevices,
            },
            WindowsAndMessaging::{
                CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
                GWLP_USERDATA, GetMessageW, GetWindowLongPtrW, HWND_MESSAGE, KillTimer, MSG,
                RegisterClassW, SetTimer, SetWindowLongPtrW, TranslateMessage, UnregisterClassW,
                WM_INPUT, WM_NCCREATE, WM_TIMER, WNDCLASSW, XBUTTON1, XBUTTON2,
            },
        },
    };

    use super::{
        CaptureStats, ClickAnchors, InjectionOperation, InputError, MAX_CAPTURE_DURATION,
        MONHOP_INJECTED_MARKER, VirtualDesktop, absolute_send_input_coordinates,
    };

    const DIAGNOSTIC_CLASS_NAME: &[u16] = &[
        76, 111, 99, 97, 108, 75, 77, 82, 97, 119, 73, 110, 112, 117, 116, 68, 105, 97, 103, 110,
        111, 115, 116, 105, 99, 0,
    ];
    const DIAGNOSTIC_TIMER_ID: usize = 1;
    const MAX_REGISTERED_RAW_INPUT_DEVICES: u32 = 4096;

    pub(super) fn capture_counts(duration: Duration) -> Result<CaptureStats, InputError> {
        validate_capture_duration(duration)?;
        let _ownership = super::RawCaptureOwnership::claim()?;
        let module = module_handle()?;
        let class = RegisteredClass::register(module)?;
        let state = Box::new(UnsafeCell::new(CaptureState::default()));
        let window = DiagnosticWindow::create(module, state.get())?;
        let mut registrations = RawRegistrationGuard::install(window.hwnd)?;

        let capture_result = run_message_loop(window.hwnd, duration);
        let restore_result = registrations.restore();
        drop(window);
        drop(class);

        capture_result?;
        restore_result?;
        // SAFETY: the window has been destroyed, so no callback can access this state.
        let state = unsafe { std::mem::take(&mut *state.get()) };
        if state.raw_input_failures != 0 {
            return Err(InputError::RawInputReadFailed);
        }
        Ok(state.stats)
    }

    #[derive(Default)]
    struct CaptureState {
        stats: CaptureStats,
        raw_input_failures: u32,
    }

    struct RegisteredClass {
        module: HINSTANCE,
    }

    impl RegisteredClass {
        fn register(module: HINSTANCE) -> Result<Self, InputError> {
            let class = WNDCLASSW {
                lpfnWndProc: Some(diagnostic_window_proc),
                hInstance: module,
                lpszClassName: DIAGNOSTIC_CLASS_NAME.as_ptr(),
                ..Default::default()
            };
            // SAFETY: class points to initialized data valid for the duration of this call.
            if unsafe { RegisterClassW(&class) } == 0 {
                return Err(last_error("RegisterClassW"));
            }
            Ok(Self { module })
        }
    }

    impl Drop for RegisteredClass {
        fn drop(&mut self) {
            // SAFETY: the class is unregistered only after its diagnostic window has been destroyed.
            unsafe {
                UnregisterClassW(DIAGNOSTIC_CLASS_NAME.as_ptr(), self.module);
            }
        }
    }

    struct DiagnosticWindow {
        hwnd: HWND,
    }

    impl DiagnosticWindow {
        fn create(module: HINSTANCE, state: *mut CaptureState) -> Result<Self, InputError> {
            // SAFETY: class is registered, the parent is HWND_MESSAGE, and state outlives the window.
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    DIAGNOSTIC_CLASS_NAME.as_ptr(),
                    ptr::null(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    HWND_MESSAGE,
                    ptr::null_mut(),
                    module,
                    state.cast::<core::ffi::c_void>(),
                )
            };
            if hwnd.is_null() {
                return Err(last_error("CreateWindowExW"));
            }
            Ok(Self { hwnd })
        }
    }

    impl Drop for DiagnosticWindow {
        fn drop(&mut self) {
            // SAFETY: hwnd is owned by this wrapper and is destroyed exactly once.
            unsafe {
                DestroyWindow(self.hwnd);
            }
        }
    }

    struct RawRegistrationGuard {
        previous: Vec<RAWINPUTDEVICE>,
        restored: bool,
    }

    impl RawRegistrationGuard {
        fn install(hwnd: HWND) -> Result<Self, InputError> {
            let previous = registered_raw_devices()?;
            let mut guard = Self {
                previous,
                restored: false,
            };
            let devices = [
                RAWINPUTDEVICE {
                    usUsagePage: 0x01,
                    usUsage: 0x02,
                    dwFlags: RIDEV_INPUTSINK,
                    hwndTarget: hwnd,
                },
                RAWINPUTDEVICE {
                    usUsagePage: 0x01,
                    usUsage: 0x06,
                    dwFlags: RIDEV_INPUTSINK,
                    hwndTarget: hwnd,
                },
            ];
            if let Err(error) = register_raw_devices(&devices) {
                let _ = guard.restore();
                return Err(error);
            }
            Ok(guard)
        }

        fn restore(&mut self) -> Result<(), InputError> {
            if self.restored {
                return Ok(());
            }
            let mut first_error = None;
            if !self.previous.is_empty()
                && let Err(error) = register_raw_devices(&self.previous)
            {
                first_error = Some(error);
            }
            for (usage_page, usage) in [(0x01, 0x02), (0x01, 0x06)] {
                if !self
                    .previous
                    .iter()
                    .any(|device| device.usUsagePage == usage_page && device.usUsage == usage)
                {
                    let removal = RAWINPUTDEVICE {
                        usUsagePage: usage_page,
                        usUsage: usage,
                        dwFlags: RIDEV_REMOVE,
                        hwndTarget: ptr::null_mut(),
                    };
                    if let Err(error) = register_raw_devices(std::slice::from_ref(&removal))
                        && first_error.is_none()
                    {
                        first_error = Some(error);
                    }
                }
            }
            if let Some(error) = first_error {
                return Err(error);
            }
            self.restored = true;
            Ok(())
        }
    }

    impl Drop for RawRegistrationGuard {
        fn drop(&mut self) {
            let _ = self.restore();
        }
    }

    fn registered_raw_devices() -> Result<Vec<RAWINPUTDEVICE>, InputError> {
        let mut count = 0_u32;
        // SAFETY: a null buffer asks Windows for the number of registered devices.
        if unsafe {
            GetRegisteredRawInputDevices(
                ptr::null_mut(),
                &mut count,
                size_of::<RAWINPUTDEVICE>() as u32,
            )
        } == u32::MAX
        {
            return Err(last_error("GetRegisteredRawInputDevices"));
        }
        if count == 0 {
            return Ok(Vec::new());
        }
        if count > MAX_REGISTERED_RAW_INPUT_DEVICES {
            return Err(InputError::TooManyRawInputRegistrations { count });
        }

        let mut devices = vec![RAWINPUTDEVICE::default(); count as usize];
        // SAFETY: devices has capacity for count records and count supplies its element count.
        let returned = unsafe {
            GetRegisteredRawInputDevices(
                devices.as_mut_ptr(),
                &mut count,
                size_of::<RAWINPUTDEVICE>() as u32,
            )
        };
        if returned == u32::MAX || returned > devices.len() as u32 {
            return Err(last_error("GetRegisteredRawInputDevices"));
        }
        devices.truncate(returned as usize);
        Ok(devices)
    }

    fn register_raw_devices(devices: &[RAWINPUTDEVICE]) -> Result<(), InputError> {
        // SAFETY: the slice is valid for this synchronous registration call.
        if unsafe {
            RegisterRawInputDevices(
                devices.as_ptr(),
                devices.len() as u32,
                size_of::<RAWINPUTDEVICE>() as u32,
            )
        } == 0
        {
            return Err(last_error("RegisterRawInputDevices"));
        }
        Ok(())
    }

    fn run_message_loop(hwnd: HWND, duration: Duration) -> Result<(), InputError> {
        let milliseconds = duration.as_millis() as u32;
        let deadline = Instant::now() + duration;
        // SAFETY: hwnd is our message-only window and the timer callback is null by design.
        if unsafe { SetTimer(hwnd, DIAGNOSTIC_TIMER_ID, milliseconds, None) } == 0 {
            return Err(last_error("SetTimer"));
        }

        let mut result = Ok(());
        let mut message = MSG::default();
        loop {
            if Instant::now() >= deadline {
                break;
            }
            // SAFETY: message is writable and hwnd is our valid message-only window.
            let status = unsafe { GetMessageW(&mut message, hwnd, 0, 0) };
            if status == -1 {
                result = Err(last_error("GetMessageW"));
                break;
            }
            if status == 0 || message.message == WM_TIMER {
                break;
            }
            // SAFETY: message was initialized by GetMessageW and dispatches only our window messages.
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        // SAFETY: the timer belongs to this window and id; it is removed before window destruction.
        unsafe {
            KillTimer(hwnd, DIAGNOSTIC_TIMER_ID);
        }
        result
    }

    unsafe extern "system" fn diagnostic_window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_NCCREATE {
            // SAFETY: WM_NCCREATE provides a valid CREATESTRUCTW for this CreateWindowExW call.
            let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
            // SAFETY: stores the caller-owned CaptureState pointer for this window's lifetime.
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            }
            return 1;
        }
        if message == WM_INPUT {
            // SAFETY: user data was set during WM_NCCREATE and remains valid until window destruction.
            let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut CaptureState };
            if !state.is_null() {
                // SAFETY: WM_INPUT's lParam is an HRAWINPUT valid while this message is processed.
                unsafe {
                    process_raw_input(lparam as HRAWINPUT, &mut *state);
                }
            }
            // SAFETY: Raw Input requires default handling after GetRawInputData releases this record.
            return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        }

        // SAFETY: forwards unhandled messages to Windows' default procedure with original arguments.
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    unsafe fn process_raw_input(raw_handle: HRAWINPUT, state: &mut CaptureState) {
        let mut raw = RAWINPUT::default();
        let mut size = size_of::<RAWINPUT>() as u32;
        // SAFETY: raw is large enough for registered keyboard/mouse RAWINPUT records and size is writable.
        let read = unsafe {
            GetRawInputData(
                raw_handle,
                RID_INPUT,
                (&mut raw as *mut RAWINPUT).cast::<core::ffi::c_void>(),
                &mut size,
                size_of::<windows_sys::Win32::UI::Input::RAWINPUTHEADER>() as u32,
            )
        };
        if read == u32::MAX {
            state.raw_input_failures = state.raw_input_failures.saturating_add(1);
            return;
        }

        match raw.header.dwType {
            RIM_TYPEKEYBOARD => {
                // SAFETY: header type identifies the active RAWINPUT union member as keyboard.
                let keyboard = unsafe { raw.data.keyboard };
                state.stats.record_keyboard(keyboard.ExtraInformation);
            }
            RIM_TYPEMOUSE => {
                // SAFETY: header type identifies the active RAWINPUT union member as mouse.
                let mouse = unsafe { raw.data.mouse };
                // SAFETY: RAWMOUSE's button union is initialized by Windows for mouse input records.
                let button_flags = unsafe { mouse.Anonymous.Anonymous.usButtonFlags };
                state.stats.record_mouse(
                    button_flags,
                    mouse.lLastX,
                    mouse.lLastY,
                    mouse.ulExtraInformation,
                );
            }
            _ => {}
        }
    }

    #[derive(Default)]
    pub struct Injector {
        held_keys: BTreeSet<HidUsage>,
        held_buttons: BTreeSet<MouseButton>,
        anchors: ClickAnchors,
        /// The desktop of the last absolute move, which is what gave `anchors` its cursor.
        desktop: Option<VirtualDesktop>,
    }

    impl Injector {
        pub const fn new() -> Self {
            Self {
                held_keys: BTreeSet::new(),
                held_buttons: BTreeSet::new(),
                anchors: ClickAnchors::new(),
                desktop: None,
            }
        }

        pub fn inject(&mut self, operation: InjectionOperation) -> Result<(), InputError> {
            match operation {
                InjectionOperation::Key { usage, pressed } => {
                    if !pressed && !self.held_keys.contains(&usage) {
                        return Err(InputError::KeyNotHeld);
                    }
                    dispatch_inputs(&[keyboard_input(usage, pressed)?])?;
                    if pressed {
                        self.held_keys.insert(usage);
                    } else {
                        self.held_keys.remove(&usage);
                    }
                }
                InjectionOperation::Button {
                    button,
                    pressed,
                    click_count,
                } => {
                    if !pressed && !self.held_buttons.contains(&button) {
                        return Err(InputError::ButtonNotHeld);
                    }
                    let moved = self.desktop.and_then(|desktop| {
                        let (x, y) = if pressed {
                            self.anchors.press_target(button, click_count)
                        } else {
                            self.anchors.release_target(button)
                        }?;
                        Some(((x, y), absolute_move_input(desktop, x, y).ok()?))
                    });
                    let input = button_input(button, pressed);
                    // One batch, so nothing lands between a snap and its press or a release and
                    // the move that resumes following the source.
                    let sent = match moved {
                        Some((_, movement)) if pressed => dispatch_inputs(&[movement, input]),
                        Some((_, movement)) => dispatch_inputs(&[input, movement]),
                        None => dispatch_inputs(&[input]),
                    };
                    if let Err(error) = sent {
                        self.anchors.forget_cursor();
                        return Err(error);
                    }
                    let moved = moved.map(|(point, _)| point);
                    if pressed {
                        self.anchors.pressed(button, moved);
                        self.held_buttons.insert(button);
                    } else {
                        self.anchors.released(button);
                        if let Some((x, y)) = moved {
                            self.anchors.moved_to(x, y);
                        }
                        self.held_buttons.remove(&button);
                    }
                }
                InjectionOperation::RelativeMove { dx, dy } => {
                    if dx == 0 && dy == 0 {
                        return Err(InputError::ZeroRelativeMotion);
                    }
                    self.anchors.forget_cursor();
                    dispatch_inputs(&[mouse_input(dx, dy, MOUSEEVENTF_MOVE, 0)])?;
                }
                InjectionOperation::AbsoluteMove { x, y, desktop } => {
                    let (x, y) = self.anchors.move_target(x, y);
                    let input = absolute_move_input(desktop, x, y)?;
                    self.anchors.forget_cursor();
                    dispatch_inputs(&[input])?;
                    self.anchors.moved_to(x, y);
                    self.desktop = Some(desktop);
                }
                InjectionOperation::Scroll {
                    vertical,
                    horizontal,
                } => {
                    if vertical == 0 && horizontal == 0 {
                        return Err(InputError::EmptyScroll);
                    }
                    let mut inputs = [mouse_input(0, 0, 0, 0), mouse_input(0, 0, 0, 0)];
                    let mut count = 0_usize;
                    if vertical != 0 {
                        inputs[count] = mouse_input(0, 0, MOUSEEVENTF_WHEEL, vertical as u32);
                        count += 1;
                    }
                    if horizontal != 0 {
                        inputs[count] = mouse_input(0, 0, MOUSEEVENTF_HWHEEL, horizontal as u32);
                        count += 1;
                    }
                    dispatch_inputs(&inputs[..count])?;
                }
            }
            Ok(())
        }

        pub fn release_all(&mut self) -> Result<(), InputError> {
            let mut inputs = Vec::with_capacity(self.held_keys.len() + self.held_buttons.len());
            let mut releases = Vec::with_capacity(inputs.capacity());
            for usage in &self.held_keys {
                inputs.push(keyboard_input(*usage, false)?);
                releases.push(HeldInput::Key(*usage));
            }
            for button in &self.held_buttons {
                inputs.push(button_input(*button, false));
                releases.push(HeldInput::Button(*button));
            }
            if inputs.is_empty() {
                return Ok(());
            }

            match dispatch_inputs(&inputs) {
                Ok(()) => {
                    for button in std::mem::take(&mut self.held_buttons) {
                        self.anchors.released(button);
                    }
                    self.held_keys.clear();
                    Ok(())
                }
                Err(InputError::PartialSendInput {
                    sent,
                    attempted,
                    code,
                }) => {
                    for release in releases.into_iter().take(sent) {
                        self.mark_released(release);
                    }
                    Err(InputError::PartialSendInput {
                        sent,
                        attempted,
                        code,
                    })
                }
                Err(error) => Err(error),
            }
        }

        pub fn is_key_held(&self, usage: HidUsage) -> bool {
            self.held_keys.contains(&usage)
        }

        pub fn is_button_held(&self, button: MouseButton) -> bool {
            self.held_buttons.contains(&button)
        }

        fn mark_released(&mut self, held: HeldInput) {
            match held {
                HeldInput::Key(usage) => {
                    self.held_keys.remove(&usage);
                }
                HeldInput::Button(button) => {
                    self.anchors.released(button);
                    self.held_buttons.remove(&button);
                }
            }
        }
    }

    impl Drop for Injector {
        fn drop(&mut self) {
            let _ = self.release_all();
        }
    }

    #[derive(Clone, Copy)]
    enum HeldInput {
        Key(HidUsage),
        Button(MouseButton),
    }

    fn keyboard_input(usage: HidUsage, pressed: bool) -> Result<INPUT, InputError> {
        let scan_code = set1_from_hid_usage(usage).map_err(InputError::UnsupportedKey)?;
        let mut flags = KEYEVENTF_SCANCODE;
        if scan_code.is_extended() {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !pressed {
            flags |= KEYEVENTF_KEYUP;
        }
        Ok(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: u16::from(scan_code.make_code),
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: MONHOP_INJECTED_MARKER,
                },
            },
        })
    }

    fn button_input(button: MouseButton, pressed: bool) -> INPUT {
        let (flags, mouse_data) = match (button, pressed) {
            (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON1)),
            (MouseButton::Back, false) => (MOUSEEVENTF_XUP, u32::from(XBUTTON1)),
            (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON2)),
            (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, u32::from(XBUTTON2)),
        };
        mouse_input(0, 0, flags, mouse_data)
    }

    fn absolute_move_input(desktop: VirtualDesktop, x: i32, y: i32) -> Result<INPUT, InputError> {
        let (normalized_x, normalized_y) = absolute_send_input_coordinates(desktop, x, y)?;
        Ok(mouse_input(
            normalized_x,
            normalized_y,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            0,
        ))
    }

    fn mouse_input(dx: i32, dy: i32, flags: u32, mouse_data: u32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: mouse_data,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: MONHOP_INJECTED_MARKER,
                },
            },
        }
    }

    fn dispatch_inputs(inputs: &[INPUT]) -> Result<(), InputError> {
        // SAFETY: inputs is a valid, contiguous array whose layout is exactly INPUT.
        let sent = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                size_of::<INPUT>() as i32,
            )
        };
        if sent == inputs.len() as u32 {
            return Ok(());
        }
        // SAFETY: GetLastError only reads the current thread's last-error value.
        let code = unsafe { GetLastError() };
        if sent == 0 {
            // A zero result can indicate UIPI denial; this code does not bypass it.
            return Err(InputError::SendInputBlockedOrFailed {
                attempted: inputs.len(),
                code,
            });
        }
        Err(InputError::PartialSendInput {
            sent: sent as usize,
            attempted: inputs.len(),
            code,
        })
    }

    fn validate_capture_duration(duration: Duration) -> Result<(), InputError> {
        if duration < Duration::from_millis(1) || duration > MAX_CAPTURE_DURATION {
            return Err(InputError::InvalidCaptureDuration);
        }
        Ok(())
    }

    fn module_handle() -> Result<HINSTANCE, InputError> {
        // SAFETY: null requests the current module handle and does not load a library.
        let module = unsafe { GetModuleHandleW(ptr::null()) };
        if module.is_null() {
            return Err(last_error("GetModuleHandleW"));
        }
        Ok(module)
    }

    fn last_error(operation: &'static str) -> InputError {
        // SAFETY: GetLastError has no preconditions and only reads this thread's error state.
        let code = unsafe { GetLastError() };
        InputError::WindowsApi { operation, code }
    }
}
