//! Keyboard chords a receiver injects for system gestures, planned so a chord never presses or
//! releases a key the wire holds and so never leaves a modifier stuck.

use crate::{HID_LEFT_CONTROL, HidUsage, Platform, SystemGesture};

/// Modifier kinds a chord needs. Either side of a held modifier satisfies its kind; a chord that
/// must press one presses the left key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ChordModifiers(u8);

impl ChordModifiers {
    pub const NONE: Self = Self(0);
    pub const CONTROL: Self = Self(1);
    pub const SHIFT: Self = Self(1 << 1);
    pub const ALTERNATE: Self = Self(1 << 2);
    /// Windows key on Windows, Command on macOS.
    pub const SYSTEM: Self = Self(1 << 3);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

impl std::ops::BitOr for ChordModifiers {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        self.union(other)
    }
}

/// HID modifier order: each side's usages run in it, the left side from 0xE0, the right from 0xE4.
const MODIFIER_KINDS: [ChordModifiers; 4] = [
    ChordModifiers::CONTROL,
    ChordModifiers::SHIFT,
    ChordModifiers::ALTERNATE,
    ChordModifiers::SYSTEM,
];

/// `key` may itself be a modifier: a Windows-key tap is the key 0xE3 with no modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    pub modifiers: ChordModifiers,
    pub key: HidUsage,
}

impl Chord {
    pub const fn new(modifiers: ChordModifiers, key: HidUsage) -> Self {
        Self { modifiers, key }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyStep {
    pub usage: HidUsage,
    pub pressed: bool,
}

/// Four modifier presses, the key down and up, four modifier releases.
pub const MAX_CHORD_STEPS: usize = 10;

/// An ordered press/release sequence; injectors post it in order, as one unit where they can.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyPlan {
    steps: [KeyStep; MAX_CHORD_STEPS],
    len: usize,
}

impl KeyPlan {
    const EMPTY: Self = Self {
        steps: [KeyStep {
            usage: HidUsage(0),
            pressed: false,
        }; MAX_CHORD_STEPS],
        len: 0,
    };

    fn push(&mut self, usage: HidUsage, pressed: bool) {
        self.steps[self.len] = KeyStep { usage, pressed };
        self.len += 1;
    }

    pub fn steps(&self) -> &[KeyStep] {
        &self.steps[..self.len]
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Presses only the chord modifiers `held` lacks, taps the key, then releases only what it pressed,
/// in reverse. Empty when `held` holds `chord.key` itself, which the chord must not touch.
pub fn chord_plan(held: impl IntoIterator<Item = HidUsage>, chord: Chord) -> KeyPlan {
    let mut held_kinds = ChordModifiers::NONE;
    for usage in held {
        if usage == chord.key {
            return KeyPlan::EMPTY;
        }
        if usage.is_modifier() {
            held_kinds = held_kinds | MODIFIER_KINDS[usize::from(usage.0 - HID_LEFT_CONTROL.0) % 4];
        }
    }
    let missing = chord.modifiers.without(held_kinds);
    let left = |index: usize| HidUsage(HID_LEFT_CONTROL.0 + index as u16);
    let mut plan = KeyPlan::EMPTY;
    for (index, kind) in MODIFIER_KINDS.iter().enumerate() {
        if missing.contains(*kind) {
            plan.push(left(index), true);
        }
    }
    plan.push(chord.key, true);
    plan.push(chord.key, false);
    for (index, kind) in MODIFIER_KINDS.iter().enumerate().rev() {
        if missing.contains(*kind) {
            plan.push(left(index), false);
        }
    }
    plan
}

const KEY_D: HidUsage = HidUsage(0x07);
const KEY_N: HidUsage = HidUsage(0x11);
const KEY_S: HidUsage = HidUsage(0x16);
const KEY_TAB: HidUsage = HidUsage(0x2B);
const KEY_SPACE: HidUsage = HidUsage(0x2C);
const KEY_MINUS: HidUsage = HidUsage(0x2D);
const KEY_EQUAL: HidUsage = HidUsage(0x2E);
const KEY_LEFT_BRACKET: HidUsage = HidUsage(0x2F);
const KEY_RIGHT_BRACKET: HidUsage = HidUsage(0x30);
const KEY_F11: HidUsage = HidUsage(0x44);
const KEY_RIGHT_ARROW: HidUsage = HidUsage(0x4F);
const KEY_LEFT_ARROW: HidUsage = HidUsage(0x50);
const KEY_DOWN_ARROW: HidUsage = HidUsage(0x51);
const KEY_UP_ARROW: HidUsage = HidUsage(0x52);
const KEY_LEFT_SYSTEM: HidUsage = HidUsage(0xE3);

/// macOS Look Up, the default binding a force click maps to.
pub const MAC_LOOK_UP: Chord =
    Chord::new(ChordModifiers::CONTROL.union(ChordModifiers::SYSTEM), KEY_D);
/// macOS zoom steps, the nearest meaning of a pinch there.
pub const MAC_ZOOM_IN: Chord = Chord::new(ChordModifiers::SYSTEM, KEY_EQUAL);
pub const MAC_ZOOM_OUT: Chord = Chord::new(ChordModifiers::SYSTEM, KEY_MINUS);

/// The default chord `destination` binds to `gesture`; macOS prefers the user's live
/// symbolic-hotkey binding over it.
pub const fn system_chord(destination: Platform, gesture: SystemGesture) -> Option<Chord> {
    match destination {
        Platform::Windows => windows_chord(gesture),
        Platform::MacOs => mac_chord(gesture),
    }
}

/// None for back and forward, which Windows injects as mouse-button clicks.
const fn windows_chord(gesture: SystemGesture) -> Option<Chord> {
    let system = ChordModifiers::SYSTEM;
    let (modifiers, key) = match gesture {
        SystemGesture::Overview | SystemGesture::AppWindows => (system, KEY_TAB),
        SystemGesture::DesktopPrevious => (ChordModifiers::CONTROL.union(system), KEY_LEFT_ARROW),
        SystemGesture::DesktopNext => (ChordModifiers::CONTROL.union(system), KEY_RIGHT_ARROW),
        SystemGesture::ShowDesktop => (system, KEY_D),
        SystemGesture::Launcher => (ChordModifiers::NONE, KEY_LEFT_SYSTEM),
        SystemGesture::NotificationCenter => (system, KEY_N),
        SystemGesture::Search => (system, KEY_S),
        SystemGesture::SwitchAppNext => (ChordModifiers::ALTERNATE, KEY_TAB),
        SystemGesture::SwitchAppPrevious => (
            ChordModifiers::ALTERNATE.union(ChordModifiers::SHIFT),
            KEY_TAB,
        ),
        SystemGesture::NavigateBack | SystemGesture::NavigateForward => return None,
    };
    Some(Chord::new(modifiers, key))
}

/// None for Launcher and Notification Center, which macOS binds to no key by default.
const fn mac_chord(gesture: SystemGesture) -> Option<Chord> {
    let control = ChordModifiers::CONTROL;
    let command = ChordModifiers::SYSTEM;
    let (modifiers, key) = match gesture {
        SystemGesture::Overview => (control, KEY_UP_ARROW),
        SystemGesture::AppWindows => (control, KEY_DOWN_ARROW),
        SystemGesture::DesktopPrevious => (control, KEY_LEFT_ARROW),
        SystemGesture::DesktopNext => (control, KEY_RIGHT_ARROW),
        SystemGesture::ShowDesktop => (ChordModifiers::NONE, KEY_F11),
        SystemGesture::NavigateBack => (command, KEY_LEFT_BRACKET),
        SystemGesture::NavigateForward => (command, KEY_RIGHT_BRACKET),
        SystemGesture::Search => (command, KEY_SPACE),
        SystemGesture::SwitchAppNext => (command, KEY_TAB),
        SystemGesture::SwitchAppPrevious => (command.union(ChordModifiers::SHIFT), KEY_TAB),
        SystemGesture::Launcher | SystemGesture::NotificationCenter => return None,
    };
    Some(Chord::new(modifiers, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEFT_CONTROL: u16 = 0xE0;
    const LEFT_SHIFT: u16 = 0xE1;
    const LEFT_ALT: u16 = 0xE2;
    const LEFT_SYSTEM: u16 = 0xE3;
    const RIGHT_CONTROL: u16 = 0xE4;
    const RIGHT_SYSTEM: u16 = 0xE7;

    /// The plan as (usage, pressed) pairs, whose failures print the usages.
    fn plan(held: &[u16], chord: Chord) -> Vec<(u16, bool)> {
        chord_plan(held.iter().map(|usage| HidUsage(*usage)), chord)
            .steps()
            .iter()
            .map(|step| (step.usage.0, step.pressed))
            .collect()
    }

    /// A tap of `key` inside presses of `modifiers` and their releases in reverse.
    fn wrapped(modifiers: &[u16], key: u16) -> Vec<(u16, bool)> {
        let mut steps: Vec<_> = modifiers.iter().map(|usage| (*usage, true)).collect();
        steps.extend([(key, true), (key, false)]);
        steps.extend(modifiers.iter().rev().map(|usage| (*usage, false)));
        steps
    }

    fn windows(gesture: SystemGesture) -> Vec<(u16, bool)> {
        plan(&[], system_chord(Platform::Windows, gesture).unwrap())
    }

    fn mac(gesture: SystemGesture) -> Vec<(u16, bool)> {
        plan(&[], system_chord(Platform::MacOs, gesture).unwrap())
    }

    #[test]
    fn with_nothing_held_a_chord_presses_every_modifier_and_releases_them_in_reverse() {
        let chord = Chord::new(
            ChordModifiers::CONTROL
                | ChordModifiers::SHIFT
                | ChordModifiers::ALTERNATE
                | ChordModifiers::SYSTEM,
            KEY_TAB,
        );
        let steps = plan(&[], chord);
        assert_eq!(steps.len(), MAX_CHORD_STEPS);
        assert_eq!(
            steps,
            wrapped(&[LEFT_CONTROL, LEFT_SHIFT, LEFT_ALT, LEFT_SYSTEM], 0x2B)
        );
    }

    #[test]
    fn a_held_modifier_is_neither_pressed_again_nor_released() {
        let chord = Chord::new(
            ChordModifiers::CONTROL | ChordModifiers::SYSTEM,
            KEY_LEFT_ARROW,
        );
        assert_eq!(plan(&[LEFT_CONTROL], chord), wrapped(&[LEFT_SYSTEM], 0x50));
        assert_eq!(
            plan(&[RIGHT_CONTROL, 0x04], chord),
            wrapped(&[LEFT_SYSTEM], 0x50),
            "either side satisfies the kind; ordinary held keys change nothing"
        );
        assert_eq!(
            plan(&[RIGHT_CONTROL, RIGHT_SYSTEM], chord),
            wrapped(&[], 0x50)
        );
    }

    #[test]
    fn a_chord_whose_key_the_wire_holds_is_skipped() {
        assert!(plan(&[0x2B], Chord::new(ChordModifiers::ALTERNATE, KEY_TAB)).is_empty());
        let launcher = system_chord(Platform::Windows, SystemGesture::Launcher).unwrap();
        assert!(plan(&[LEFT_SYSTEM], launcher).is_empty());
    }

    #[test]
    fn windows_task_view_is_win_tab_for_overview_and_app_windows() {
        for gesture in [SystemGesture::Overview, SystemGesture::AppWindows] {
            assert_eq!(windows(gesture), wrapped(&[LEFT_SYSTEM], 0x2B));
        }
    }

    #[test]
    fn windows_desktop_previous_is_ctrl_win_left() {
        assert_eq!(
            windows(SystemGesture::DesktopPrevious),
            wrapped(&[LEFT_CONTROL, LEFT_SYSTEM], 0x50)
        );
    }

    #[test]
    fn windows_desktop_next_is_ctrl_win_right() {
        assert_eq!(
            windows(SystemGesture::DesktopNext),
            wrapped(&[LEFT_CONTROL, LEFT_SYSTEM], 0x4F)
        );
    }

    #[test]
    fn windows_show_desktop_is_win_d() {
        assert_eq!(
            windows(SystemGesture::ShowDesktop),
            wrapped(&[LEFT_SYSTEM], 0x07)
        );
    }

    #[test]
    fn windows_launcher_is_a_win_tap() {
        assert_eq!(windows(SystemGesture::Launcher), wrapped(&[], LEFT_SYSTEM));
    }

    #[test]
    fn windows_back_and_forward_are_mouse_buttons_not_chords() {
        for gesture in [SystemGesture::NavigateBack, SystemGesture::NavigateForward] {
            assert_eq!(system_chord(Platform::Windows, gesture), None);
        }
    }

    #[test]
    fn windows_notification_center_is_win_n() {
        assert_eq!(
            windows(SystemGesture::NotificationCenter),
            wrapped(&[LEFT_SYSTEM], 0x11)
        );
    }

    #[test]
    fn windows_search_is_win_s() {
        assert_eq!(
            windows(SystemGesture::Search),
            wrapped(&[LEFT_SYSTEM], 0x16)
        );
    }

    #[test]
    fn windows_switch_app_next_is_alt_tab() {
        assert_eq!(
            windows(SystemGesture::SwitchAppNext),
            wrapped(&[LEFT_ALT], 0x2B)
        );
    }

    #[test]
    fn windows_switch_app_previous_is_alt_shift_tab() {
        assert_eq!(
            windows(SystemGesture::SwitchAppPrevious),
            wrapped(&[LEFT_SHIFT, LEFT_ALT], 0x2B)
        );
    }

    #[test]
    fn mac_overview_is_ctrl_up() {
        assert_eq!(mac(SystemGesture::Overview), wrapped(&[LEFT_CONTROL], 0x52));
    }

    #[test]
    fn mac_app_windows_is_ctrl_down() {
        assert_eq!(
            mac(SystemGesture::AppWindows),
            wrapped(&[LEFT_CONTROL], 0x51)
        );
    }

    #[test]
    fn mac_desktop_previous_is_ctrl_left() {
        assert_eq!(
            mac(SystemGesture::DesktopPrevious),
            wrapped(&[LEFT_CONTROL], 0x50)
        );
    }

    #[test]
    fn mac_desktop_next_is_ctrl_right() {
        assert_eq!(
            mac(SystemGesture::DesktopNext),
            wrapped(&[LEFT_CONTROL], 0x4F)
        );
    }

    #[test]
    fn mac_show_desktop_is_f11() {
        assert_eq!(mac(SystemGesture::ShowDesktop), wrapped(&[], 0x44));
    }

    #[test]
    fn mac_launcher_and_notification_center_have_no_default_chord() {
        for gesture in [SystemGesture::Launcher, SystemGesture::NotificationCenter] {
            assert_eq!(system_chord(Platform::MacOs, gesture), None);
        }
    }

    #[test]
    fn mac_navigate_back_is_cmd_left_bracket() {
        assert_eq!(
            mac(SystemGesture::NavigateBack),
            wrapped(&[LEFT_SYSTEM], 0x2F)
        );
    }

    #[test]
    fn mac_navigate_forward_is_cmd_right_bracket() {
        assert_eq!(
            mac(SystemGesture::NavigateForward),
            wrapped(&[LEFT_SYSTEM], 0x30)
        );
    }

    #[test]
    fn mac_search_is_cmd_space() {
        assert_eq!(mac(SystemGesture::Search), wrapped(&[LEFT_SYSTEM], 0x2C));
    }

    #[test]
    fn mac_switch_app_next_is_cmd_tab() {
        assert_eq!(
            mac(SystemGesture::SwitchAppNext),
            wrapped(&[LEFT_SYSTEM], 0x2B)
        );
    }

    #[test]
    fn mac_switch_app_previous_is_cmd_shift_tab() {
        assert_eq!(
            mac(SystemGesture::SwitchAppPrevious),
            wrapped(&[LEFT_SHIFT, LEFT_SYSTEM], 0x2B)
        );
    }

    #[test]
    fn mac_look_up_is_ctrl_cmd_d() {
        assert_eq!(
            plan(&[], MAC_LOOK_UP),
            wrapped(&[LEFT_CONTROL, LEFT_SYSTEM], 0x07)
        );
    }

    #[test]
    fn mac_zoom_steps_are_cmd_equal_and_cmd_minus() {
        assert_eq!(plan(&[], MAC_ZOOM_IN), wrapped(&[LEFT_SYSTEM], 0x2E));
        assert_eq!(plan(&[], MAC_ZOOM_OUT), wrapped(&[LEFT_SYSTEM], 0x2D));
    }
}
