//! What Home calls a key or mouse button the user must release. Keys are named by their US
//! layout position, as HID usages are. A name only ever reaches the window, never a log.

use monhop_core::{HidUsage, MouseButton, capture_physical::HeldInput};

/// Stands in for a usage the capture never reports.
const UNNAMED_KEY: &str = "a held key";

pub(crate) fn name(held: HeldInput) -> String {
    match held {
        HeldInput::Key(usage) => key_name(usage),
        HeldInput::Button(button) => button_name(button).to_owned(),
    }
}

fn key_name(HidUsage(usage): HidUsage) -> String {
    match usage {
        0x04..=0x1D => char::from(b'A' + (usage - 0x04) as u8).to_string(),
        0x1E..=0x26 => (usage - 0x1D).to_string(),
        0x3A..=0x45 => format!("F{}", usage - 0x39),
        0x59..=0x61 => format!("Keypad {}", usage - 0x58),
        0x68..=0x73 => format!("F{}", usage - 0x5B),
        _ => named_key(usage).unwrap_or(UNNAMED_KEY).to_owned(),
    }
}

fn named_key(usage: u16) -> Option<&'static str> {
    Some(match usage {
        0x27 => "0",
        0x28 => platform("Return", "Enter"),
        0x29 => "Escape",
        0x2A => platform("Delete", "Backspace"),
        0x2B => "Tab",
        0x2C => "Space",
        0x2D => "Minus",
        0x2E => "Equals",
        0x2F => "Left Bracket",
        0x30 => "Right Bracket",
        0x31 => "Backslash",
        0x32 => "Non-US Hash",
        0x33 => "Semicolon",
        0x34 => "Quote",
        0x35 => "Backtick",
        0x36 => "Comma",
        0x37 => "Period",
        0x38 => "Slash",
        0x39 => "Caps Lock",
        0x46 => "Print Screen",
        0x47 => "Scroll Lock",
        0x48 => "Pause",
        0x49 => platform("Help", "Insert"),
        0x4A => "Home",
        0x4B => "Page Up",
        0x4C => platform("Forward Delete", "Delete"),
        0x4D => "End",
        0x4E => "Page Down",
        0x4F => "Right Arrow",
        0x50 => "Left Arrow",
        0x51 => "Down Arrow",
        0x52 => "Up Arrow",
        0x53 => platform("Clear", "Num Lock"),
        0x54 => "Keypad Slash",
        0x55 => "Keypad Asterisk",
        0x56 => "Keypad Minus",
        0x57 => "Keypad Plus",
        0x58 => "Keypad Enter",
        0x62 => "Keypad 0",
        0x63 => "Keypad Period",
        0x64 => "Non-US Backslash",
        0x65 => "Menu",
        0x66 => "Power",
        0x67 => "Keypad Equals",
        0xE0 => platform("Left Control", "Left Ctrl"),
        0xE1 => "Left Shift",
        0xE2 => platform("Left Option", "Left Alt"),
        0xE3 => platform("Left Command", "Left Windows"),
        0xE4 => platform("Right Control", "Right Ctrl"),
        0xE5 => "Right Shift",
        0xE6 => platform("Right Option", "Right Alt"),
        0xE7 => platform("Right Command", "Right Windows"),
        _ => return None,
    })
}

/// The label printed on this computer's own keyboard.
const fn platform(macos: &'static str, windows: &'static str) -> &'static str {
    if cfg!(target_os = "macos") {
        macos
    } else {
        windows
    }
}

const fn button_name(button: MouseButton) -> &'static str {
    match button {
        MouseButton::Left => "the left mouse button",
        MouseButton::Right => "the right mouse button",
        MouseButton::Middle => "the middle mouse button",
        MouseButton::Back => "the back mouse button",
        MouseButton::Forward => "the forward mouse button",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(usage: u16) -> String {
        name(HeldInput::Key(HidUsage(usage)))
    }

    #[test]
    fn every_key_the_capture_reports_has_its_own_name() {
        let usages: Vec<u16> = (0x04..=0x73).chain(0xE0..=0xE7).collect();
        let names: Vec<String> = usages.iter().map(|usage| key(*usage)).collect();
        assert!(names.iter().all(|name| name != UNNAMED_KEY));
        let mut distinct = names.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), names.len());
    }

    #[test]
    fn keys_read_as_their_labels() {
        assert_eq!(key(0x04), "A");
        assert_eq!(key(0x1D), "Z");
        assert_eq!(key(0x1E), "1");
        assert_eq!(key(0x26), "9");
        assert_eq!(key(0x27), "0");
        assert_eq!(key(0x3A), "F1");
        assert_eq!(key(0x45), "F12");
        assert_eq!(key(0x68), "F13");
        assert_eq!(key(0x73), "F24");
        assert_eq!(key(0x59), "Keypad 1");
        assert_eq!(key(0x62), "Keypad 0");
        assert_eq!(key(0x50), "Left Arrow");
        assert_eq!(key(0xE1), "Left Shift");
        assert_eq!(key(0xE5), "Right Shift");
        let (control, option, command) = if cfg!(target_os = "macos") {
            ("Left Control", "Left Option", "Left Command")
        } else {
            ("Left Ctrl", "Left Alt", "Left Windows")
        };
        assert_eq!(
            [key(0xE0), key(0xE2), key(0xE3)],
            [control, option, command]
        );
    }

    #[test]
    fn a_usage_the_capture_never_reports_still_reads_as_a_key() {
        assert_eq!(key(0x74), UNNAMED_KEY);
        assert_eq!(key(0x1000), UNNAMED_KEY);
    }

    #[test]
    fn buttons_read_as_mouse_buttons() {
        assert_eq!(
            MouseButton::ALL.map(|button| name(HeldInput::Button(button))),
            [
                "the left mouse button",
                "the right mouse button",
                "the middle mouse button",
                "the back mouse button",
                "the forward mouse button",
            ]
        );
    }
}
