//! Pure decoding of low-level Windows hook metadata into capture events.

use std::time::Duration;

use monhop_core::{ModifierState, MouseButton};

use crate::{
    capture::{CaptureEvent, SINGLE_CLICK},
    input::MONHOP_INJECTED_MARKER,
    keymap::{Set1Prefix, Set1ScanCode, hid_usage_from_set1},
};

/// Only representable physical events may enter the capture ledger.
pub enum DecodedInput {
    Ignored,
    Unsupported,
    Malformed,
    Event(CaptureEvent),
}

// Windows SDK `um/winuser.h` is the primary declaration for these WM_*, LL*HF, and XBUTTON values.
const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_MOUSEWHEEL: u32 = 0x020A;
const WM_XBUTTONDOWN: u32 = 0x020B;
const WM_XBUTTONUP: u32 = 0x020C;
const WM_MOUSEHWHEEL: u32 = 0x020E;

const VK_PAUSE: u32 = 0x13;
const LLKHF_EXTENDED: u32 = 0x01;
const LLKHF_LOWER_IL_INJECTED: u32 = 0x02;
const LLKHF_INJECTED: u32 = 0x10;
const LLKHF_UP: u32 = 0x80;
const LLMHF_INJECTED: u32 = 0x01;
const LLMHF_LOWER_IL_INJECTED: u32 = 0x02;
const XBUTTON1: u16 = 0x0001;
const XBUTTON2: u16 = 0x0002;

/// Decodes one `KBDLLHOOKSTRUCT` record without retaining any physical state.
pub fn decode_keyboard(
    message: u32,
    virtual_key: u32,
    scan_code: u32,
    flags: u32,
    extra_info: usize,
) -> DecodedInput {
    if flags & (LLKHF_INJECTED | LLKHF_LOWER_IL_INJECTED) != 0
        || extra_info == MONHOP_INJECTED_MARKER
    {
        return DecodedInput::Ignored;
    }

    let Some(pressed) = keyboard_pressed(message) else {
        return DecodedInput::Ignored;
    };
    if (flags & LLKHF_UP != 0) == pressed {
        return DecodedInput::Malformed;
    }
    if virtual_key == VK_PAUSE || scan_code == 0 || scan_code > u32::from(u8::MAX) {
        return DecodedInput::Unsupported;
    }

    let prefix = if flags & LLKHF_EXTENDED != 0 {
        Set1Prefix::E0
    } else {
        Set1Prefix::None
    };
    let scan_code = Set1ScanCode::new(scan_code as u8, prefix);
    let Ok(usage) = hid_usage_from_set1(scan_code) else {
        return DecodedInput::Unsupported;
    };

    DecodedInput::Event(CaptureEvent::Key {
        usage,
        pressed,
        repeat: false,
        modifiers: ModifierState(0),
    })
}

/// The user's multi-click settings: `GetDoubleClickTime` and `SM_CXDOUBLECLK`/`SM_CYDOUBLECLK`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DoubleClickSettings {
    pub interval: Duration,
    pub width: u32,
    pub height: u32,
}

/// Windows' out-of-the-box settings, also used for a metric that reads as 0.
pub const WINDOWS_DEFAULT_DOUBLE_CLICK: DoubleClickSettings = DoubleClickSettings {
    interval: Duration::from_millis(500),
    width: 4,
    height: 4,
};

#[derive(Clone, Copy)]
struct LastPress {
    button: MouseButton,
    at: Duration,
    count: u8,
}

/// Numbers presses the way Windows pairs a double-click: the same button again within the
/// interval, having moved at most half the rectangle each way since its previous press.
///
/// Travel is summed from raw motion, because the hook's position stays clipped at the crossed
/// edge while the cursor is pinned for the peer.
#[derive(Clone, Copy)]
pub struct ClickCounter {
    last: Option<LastPress>,
    travel: (i64, i64),
    released: [u8; MouseButton::ALL.len()],
}

impl Default for ClickCounter {
    fn default() -> Self {
        Self {
            last: None,
            travel: (0, 0),
            released: [SINGLE_CLICK; MouseButton::ALL.len()],
        }
    }
}

impl ClickCounter {
    pub fn moved(&mut self, dx: i32, dy: i32) {
        self.travel = (
            self.travel.0.saturating_add(i64::from(dx)),
            self.travel.1.saturating_add(i64::from(dy)),
        );
    }

    /// The count for a press at `at`, or for a release the count of that button's last press.
    pub fn count(
        &mut self,
        button: MouseButton,
        pressed: bool,
        at: Duration,
        settings: DoubleClickSettings,
    ) -> u8 {
        if !pressed {
            return self.released[button.index()];
        }
        let count = match self.last {
            Some(last)
                if last.button == button
                    && at
                        .checked_sub(last.at)
                        .is_some_and(|gap| gap <= settings.interval)
                    && self.travel.0.unsigned_abs() <= u64::from(settings.width / 2)
                    && self.travel.1.unsigned_abs() <= u64::from(settings.height / 2) =>
            {
                last.count.saturating_add(1)
            }
            _ => SINGLE_CLICK,
        };
        self.last = Some(LastPress { button, at, count });
        self.travel = (0, 0);
        self.released[button.index()] = count;
        count
    }
}

/// Decodes one `MSLLHOOKSTRUCT` record without retaining pointer or button state. A button is a
/// single click here; the capture thread's [`ClickCounter`] numbers it.
pub fn decode_mouse(
    message: u32,
    flags: u32,
    extra_info: usize,
    mouse_data: u32,
    x: i32,
    y: i32,
) -> DecodedInput {
    if flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) != 0
        || extra_info == MONHOP_INJECTED_MARKER
    {
        return DecodedInput::Ignored;
    }

    match message {
        WM_MOUSEMOVE => DecodedInput::Event(CaptureEvent::AbsoluteMotion { x, y }),
        WM_LBUTTONDOWN => button(MouseButton::Left, true),
        WM_LBUTTONUP => button(MouseButton::Left, false),
        WM_RBUTTONDOWN => button(MouseButton::Right, true),
        WM_RBUTTONUP => button(MouseButton::Right, false),
        WM_MBUTTONDOWN => button(MouseButton::Middle, true),
        WM_MBUTTONUP => button(MouseButton::Middle, false),
        WM_XBUTTONDOWN => xbutton(mouse_data, true),
        WM_XBUTTONUP => xbutton(mouse_data, false),
        WM_MOUSEWHEEL => scroll(0, signed_high_word(mouse_data)),
        WM_MOUSEHWHEEL => scroll(signed_high_word(mouse_data), 0),
        _ => DecodedInput::Unsupported,
    }
}

fn keyboard_pressed(message: u32) -> Option<bool> {
    match message {
        WM_KEYDOWN | WM_SYSKEYDOWN => Some(true),
        WM_KEYUP | WM_SYSKEYUP => Some(false),
        _ => None,
    }
}

fn button(button: MouseButton, pressed: bool) -> DecodedInput {
    DecodedInput::Event(CaptureEvent::Button {
        button,
        pressed,
        click_count: SINGLE_CLICK,
    })
}

fn xbutton(mouse_data: u32, pressed: bool) -> DecodedInput {
    let mouse_button = match (mouse_data >> 16) as u16 {
        XBUTTON1 => MouseButton::Back,
        XBUTTON2 => MouseButton::Forward,
        _ => return DecodedInput::Unsupported,
    };
    button(mouse_button, pressed)
}

fn scroll(horizontal: i32, vertical: i32) -> DecodedInput {
    if horizontal == 0 && vertical == 0 {
        return DecodedInput::Ignored;
    }
    DecodedInput::Event(CaptureEvent::Scroll {
        horizontal,
        vertical,
    })
}

fn signed_high_word(value: u32) -> i32 {
    i32::from(((value >> 16) as u16) as i16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use monhop_core::HidUsage;

    fn high_word(value: i16) -> u32 {
        u32::from(value as u16) << 16
    }

    fn assert_key(input: DecodedInput, usage: u16, pressed: bool) {
        match input {
            DecodedInput::Event(CaptureEvent::Key {
                usage: actual_usage,
                pressed: actual_pressed,
                repeat,
                modifiers,
            }) => {
                assert_eq!(actual_usage, HidUsage(usage));
                assert_eq!(actual_pressed, pressed);
                assert!(!repeat);
                assert_eq!(modifiers, ModifierState(0));
            }
            _ => panic!("expected a key event"),
        }
    }

    fn assert_button(input: DecodedInput, button: MouseButton, pressed: bool) {
        match input {
            DecodedInput::Event(CaptureEvent::Button {
                button: actual_button,
                pressed: actual_pressed,
                ..
            }) => {
                assert_eq!(actual_button, button);
                assert_eq!(actual_pressed, pressed);
            }
            _ => panic!("expected a button event"),
        }
    }

    #[test]
    fn set1_prefix_preserves_ctrl_alt_and_navigation_sides() {
        assert_key(decode_keyboard(WM_KEYDOWN, 0, 0x1d, 0, 0), 0xe0, true);
        assert_key(
            decode_keyboard(WM_KEYDOWN, 0, 0x1d, LLKHF_EXTENDED, 0),
            0xe4,
            true,
        );
        assert_key(decode_keyboard(WM_KEYDOWN, 0, 0x38, 0, 0), 0xe2, true);
        assert_key(
            decode_keyboard(WM_KEYDOWN, 0, 0x38, LLKHF_EXTENDED, 0),
            0xe6,
            true,
        );
        assert_key(
            decode_keyboard(WM_KEYDOWN, 0, 0x47, LLKHF_EXTENDED, 0),
            0x4a,
            true,
        );
    }

    #[test]
    fn print_screen_uses_its_e0_set1_mapping() {
        assert_key(
            decode_keyboard(WM_KEYDOWN, 0x2c, 0x37, LLKHF_EXTENDED, 0),
            0x46,
            true,
        );
    }

    #[test]
    fn injected_records_are_excluded_before_other_validation() {
        assert!(matches!(
            decode_keyboard(WM_KEYUP, VK_PAUSE, 0, LLKHF_INJECTED, 0),
            DecodedInput::Ignored
        ));
        assert!(matches!(
            decode_keyboard(0, 0, 0, 0, MONHOP_INJECTED_MARKER),
            DecodedInput::Ignored
        ));
        assert!(matches!(
            decode_mouse(WM_XBUTTONDOWN, LLMHF_LOWER_IL_INJECTED, 0, 0, 0, 0),
            DecodedInput::Ignored
        ));
        assert!(matches!(
            decode_mouse(0, 0, MONHOP_INJECTED_MARKER, 0, 0, 0),
            DecodedInput::Ignored
        ));
    }

    #[test]
    fn pause_is_rejected_before_num_lock_mapping() {
        assert!(matches!(
            decode_keyboard(WM_KEYDOWN, VK_PAUSE, 0x45, 0, 0),
            DecodedInput::Unsupported
        ));
        assert_key(decode_keyboard(WM_KEYDOWN, 0x90, 0x45, 0, 0), 0x53, true);
    }

    #[test]
    fn keyboard_up_flag_must_match_the_message() {
        assert!(matches!(
            decode_keyboard(WM_KEYUP, 0, 0x1e, 0, 0),
            DecodedInput::Malformed
        ));
        assert!(matches!(
            decode_keyboard(WM_SYSKEYDOWN, 0, 0x1e, LLKHF_UP, 0),
            DecodedInput::Malformed
        ));
        assert_key(
            decode_keyboard(WM_SYSKEYUP, 0, 0x1e, LLKHF_UP, 0),
            0x04,
            false,
        );
    }

    #[test]
    fn invalid_scan_width_and_zero_scan_are_unsupported() {
        assert!(matches!(
            decode_keyboard(WM_KEYDOWN, 0, 0, 0, 0),
            DecodedInput::Unsupported
        ));
        assert!(matches!(
            decode_keyboard(WM_KEYDOWN, 0, 0x100, 0, 0),
            DecodedInput::Unsupported
        ));
    }

    #[test]
    fn wheel_deltas_stay_signed_and_in_raw_units() {
        match decode_mouse(WM_MOUSEWHEEL, 0, 0, high_word(-60), 0, 0) {
            DecodedInput::Event(CaptureEvent::Scroll {
                horizontal,
                vertical,
            }) => {
                assert_eq!(horizontal, 0);
                assert_eq!(vertical, -60);
            }
            _ => panic!("expected vertical scroll"),
        }
        match decode_mouse(WM_MOUSEHWHEEL, 0, 0, high_word(180), 0, 0) {
            DecodedInput::Event(CaptureEvent::Scroll {
                horizontal,
                vertical,
            }) => {
                assert_eq!(horizontal, 180);
                assert_eq!(vertical, 0);
            }
            _ => panic!("expected horizontal scroll"),
        }
        assert!(matches!(
            decode_mouse(WM_MOUSEWHEEL, 0, 0, 0, 0, 0),
            DecodedInput::Ignored
        ));
    }

    #[test]
    fn xbuttons_decode_to_back_and_forward() {
        assert_button(
            decode_mouse(WM_XBUTTONDOWN, 0, 0, u32::from(XBUTTON1) << 16, 0, 0),
            MouseButton::Back,
            true,
        );
        assert_button(
            decode_mouse(WM_XBUTTONUP, 0, 0, u32::from(XBUTTON2) << 16, 0, 0),
            MouseButton::Forward,
            false,
        );
        assert!(matches!(
            decode_mouse(WM_XBUTTONDOWN, 0, 0, 3_u32 << 16, 0, 0),
            DecodedInput::Unsupported
        ));
    }

    #[test]
    fn pointer_motion_keeps_negative_physical_desktop_coordinates() {
        match decode_mouse(WM_MOUSEMOVE, 0, 0, 0, -1920, -1080) {
            DecodedInput::Event(CaptureEvent::AbsoluteMotion { x, y }) => {
                assert_eq!(x, -1920);
                assert_eq!(y, -1080);
            }
            _ => panic!("expected absolute motion"),
        }
    }

    #[test]
    fn unknown_keyboard_messages_are_ignored_and_mouse_messages_are_unsupported() {
        assert!(matches!(
            decode_keyboard(0xffff, 0, 0x1e, 0, 0),
            DecodedInput::Ignored
        ));
        assert!(matches!(
            decode_mouse(0xffff, 0, 0, 0, 0, 0),
            DecodedInput::Unsupported
        ));
    }

    #[test]
    fn unsupported_keys_are_classified_consistently_on_press_and_release() {
        for (virtual_key, scan_code) in [(VK_PAUSE, 0x45), (0, 0), (0, 0x100), (0, 0xff)] {
            for (message, flags) in [(WM_KEYDOWN, 0), (WM_KEYUP, LLKHF_UP)] {
                assert!(matches!(
                    decode_keyboard(message, virtual_key, scan_code, flags, 0),
                    DecodedInput::Unsupported
                ));
            }
        }
        // Unsupported capabilities do not excuse contradictory hook metadata.
        assert!(matches!(
            decode_keyboard(WM_KEYUP, VK_PAUSE, 0, 0, 0),
            DecodedInput::Malformed
        ));
    }
}
