//! What the Mac injects for a peer's gestures: Cmd± zoom steps for a pinch, Look Up for a force
//! click, and the user's own symbolic-hotkey chord for a system gesture.
//!
//! Pure: the native side copies the hotkey preference in and posts the planned key events.

use std::fmt;

use monhop_core::{
    GesturePhase, HidUsage, MAX_PINCH_STEPS_PER_RECORD, Platform, SystemGesture,
    chord::{Chord, ChordModifiers, chord_plan, system_chord},
};

use crate::{
    MAC_EVENT_FLAG_COMMAND, MAC_EVENT_FLAG_CONTROL, MAC_EVENT_FLAG_NUMERIC_PAD,
    MAC_EVENT_FLAG_OPTION, MAC_EVENT_FLAG_SECONDARY_FN, MAC_EVENT_FLAG_SHIFT, MacError,
    MacVirtualKey, hid_to_mac_virtual_key, mac_modifier_flags_from_held_keys,
    mac_virtual_key_to_hid,
};

/// Pinch scale per Cmd+= or Cmd+- step: one step per 25 % of accumulated scale.
pub const PINCH_SCALE_PER_ZOOM_STEP: f64 = 1.25;

const HOTKEY_MISSION_CONTROL: u32 = 32;
const HOTKEY_APPLICATION_WINDOWS: u32 = 33;
const HOTKEY_SHOW_DESKTOP: u32 = 36;
const HOTKEY_SPOTLIGHT: u32 = 64;
const HOTKEY_SPACE_LEFT: u32 = 79;
const HOTKEY_SPACE_RIGHT: u32 = 81;
const HOTKEY_LAUNCHPAD: u32 = 160;
const HOTKEY_NOTIFICATION_CENTER: u32 = 163;
/// The `AppleSymbolicHotKeys` ids behind system gestures; the reader copies only these.
pub const SYSTEM_GESTURE_HOTKEYS: [u32; 8] = [
    HOTKEY_MISSION_CONTROL,
    HOTKEY_APPLICATION_WINDOWS,
    HOTKEY_SHOW_DESKTOP,
    HOTKEY_SPOTLIGHT,
    HOTKEY_SPACE_LEFT,
    HOTKEY_SPACE_RIGHT,
    HOTKEY_LAUNCHPAD,
    HOTKEY_NOTIFICATION_CENTER,
];
/// The virtual key code a binding stores when it has no key.
const NO_KEY: i64 = 0xFFFF;

/// Accumulates a pinch into whole Cmd± zoom steps.
#[derive(Debug, Default)]
pub struct PinchSteps {
    /// Steps not yet posted, in units of one step.
    residual: f64,
}

impl PinchSteps {
    /// Signed steps one magnify record adds, positive zooming in, at most
    /// [`MAX_PINCH_STEPS_PER_RECORD`] either way; a larger change keeps only its fraction. The
    /// residual starts at Began and is dropped at Ended or Cancelled.
    pub fn steps(&mut self, phase: GesturePhase, delta: f64) -> i32 {
        if phase == GesturePhase::Began {
            self.residual = 0.0;
        }
        let change = if phase == GesturePhase::Cancelled || !(delta > -1.0 && delta.is_finite()) {
            0.0
        } else {
            (1.0 + delta).ln() / PINCH_SCALE_PER_ZOOM_STEP.ln()
        };
        let total = self.residual + change;
        let whole = total.trunc();
        self.residual = if phase.is_terminal() {
            0.0
        } else {
            total - whole
        };
        let limit = f64::from(MAX_PINCH_STEPS_PER_RECORD);
        whole.clamp(-limit, limit) as i32
    }

    pub fn reset(&mut self) {
        self.residual = 0.0;
    }
}

/// A chord plus the flags its key carries on a real keyboard, or in the user's binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MacChord {
    pub chord: Chord,
    pub key_flags: u64,
}

impl MacChord {
    pub const fn new(chord: Chord) -> Self {
        Self {
            chord,
            key_flags: intrinsic_key_flags(chord.key),
        }
    }
}

/// Flags macOS sets on a key's own events: arrows count as keypad keys, and arrow, function and
/// navigation keys carry Fn. A global hotkey on such a key may not match without them.
pub const fn intrinsic_key_flags(usage: HidUsage) -> u64 {
    match usage.0 {
        0x4F..=0x52 => MAC_EVENT_FLAG_NUMERIC_PAD | MAC_EVENT_FLAG_SECONDARY_FN,
        0x3A..=0x45 | 0x49..=0x4E | 0x68..=0x73 => MAC_EVENT_FLAG_SECONDARY_FN,
        _ => 0,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChordKeyEvent {
    pub key: MacVirtualKey,
    pub pressed: bool,
    pub flags: u64,
}

/// `chord` as ordered key events over the wire's `held` keys, per [`chord_plan`]: it presses
/// only the modifiers `held` lacks, and each event carries the flags a keyboard would.
pub fn chord_key_events(
    held: impl IntoIterator<Item = HidUsage> + Clone,
    chord: MacChord,
) -> Result<Vec<ChordKeyEvent>, MacError> {
    let mut flags = mac_modifier_flags_from_held_keys(held.clone());
    chord_plan(held, chord.chord)
        .steps()
        .iter()
        .map(|step| {
            let modifier = mac_modifier_flags_from_held_keys([step.usage]);
            let event_flags = if modifier == 0 {
                flags | chord.key_flags
            } else {
                if step.pressed {
                    flags |= modifier;
                } else {
                    flags &= !modifier;
                }
                flags
            };
            Ok(ChordKeyEvent {
                key: hid_to_mac_virtual_key(step.usage)?,
                pressed: step.pressed,
                flags: event_flags,
            })
        })
        .collect()
}

/// Posts `events` back to back. A failed post first releases every key the chord left down, in
/// the chord's own release order, then returns that error; a release that fails too joins `stuck`.
pub fn post_chord_events(
    events: &[ChordKeyEvent],
    stuck: &mut StuckChordKeys,
    mut post: impl FnMut(ChordKeyEvent) -> Result<(), MacError>,
) -> Result<(), MacError> {
    for (index, event) in events.iter().enumerate() {
        let Err(error) = post(*event) else {
            continue;
        };
        let posted = &events[..index];
        let still_down = |key| {
            posted
                .iter()
                .rev()
                .find(|posted| posted.key == key)
                .is_some_and(|posted| posted.pressed)
        };
        for release in events[index..]
            .iter()
            .filter(|release| !release.pressed && still_down(release.key))
        {
            if post(*release).is_err() {
                stuck.add(*release);
            }
        }
        return Err(error);
    }
    Ok(())
}

/// Chord keys a failed post left down. The wire never held them, so only these releases free
/// them; the injector retries them whenever it ends gestures or releases everything.
#[derive(Debug, Default)]
pub struct StuckChordKeys {
    releases: Vec<ChordKeyEvent>,
}

impl StuckChordKeys {
    fn add(&mut self, release: ChordKeyEvent) {
        if !self.releases.iter().any(|stuck| stuck.key == release.key) {
            self.releases.push(release);
        }
    }

    /// The wire's own press or release of `key` now decides it.
    pub fn forget(&mut self, key: MacVirtualKey) {
        self.releases.retain(|stuck| stuck.key != key);
    }

    /// Retries every release in order, keeping each that fails again; returns the first error.
    pub fn release(
        &mut self,
        mut post: impl FnMut(ChordKeyEvent) -> Result<(), MacError>,
    ) -> Result<(), MacError> {
        let mut first_error = None;
        self.releases.retain(|release| {
            post(*release)
                .map_err(|error| first_error.get_or_insert(error))
                .is_err()
        });
        first_error.map_or(Ok(()), Err)
    }

    pub fn is_empty(&self) -> bool {
        self.releases.is_empty()
    }
}

/// One `AppleSymbolicHotKeys` entry as the preference stores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SymbolicHotkeyEntry {
    pub enabled: bool,
    pub binding: StoredBinding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoredBinding {
    /// No `value`: the entry keeps its default binding.
    Default,
    /// `value.parameters`: character code, virtual key code and modifier mask.
    Parameters([i64; 3]),
    /// A `value` in any other shape.
    Unreadable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HotkeyBinding {
    Default,
    Disabled,
    Chord(MacChord),
}

impl HotkeyBinding {
    /// A binding MonHop cannot reproduce counts as disabled: it never guesses a chord.
    fn from_entry(entry: SymbolicHotkeyEntry) -> Self {
        if !entry.enabled {
            return Self::Disabled;
        }
        match entry.binding {
            StoredBinding::Default => Self::Default,
            StoredBinding::Parameters(parameters) => {
                bound_chord(parameters).map_or(Self::Disabled, Self::Chord)
            }
            StoredBinding::Unreadable => Self::Disabled,
        }
    }
}

fn bound_chord([_, key_code, mask]: [i64; 3]) -> Option<MacChord> {
    if key_code == NO_KEY {
        return None;
    }
    let key = MacVirtualKey(u16::try_from(key_code).ok()?);
    let usage = mac_virtual_key_to_hid(key).ok()?;
    let mask = u64::try_from(mask).ok()?;
    let mut modifiers = ChordModifiers::NONE;
    for (flag, kind) in [
        (MAC_EVENT_FLAG_CONTROL, ChordModifiers::CONTROL),
        (MAC_EVENT_FLAG_SHIFT, ChordModifiers::SHIFT),
        (MAC_EVENT_FLAG_OPTION, ChordModifiers::ALTERNATE),
        (MAC_EVENT_FLAG_COMMAND, ChordModifiers::SYSTEM),
    ] {
        if mask & flag != 0 {
            modifiers = modifiers | kind;
        }
    }
    Some(MacChord {
        chord: Chord::new(modifiers, usage),
        key_flags: intrinsic_key_flags(usage)
            | mask & (MAC_EVENT_FLAG_NUMERIC_PAD | MAC_EVENT_FLAG_SECONDARY_FN),
    })
}

/// The user's bindings for [`SYSTEM_GESTURE_HOTKEYS`]; an id absent from the preference keeps its
/// default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SymbolicHotkeys {
    bindings: [HotkeyBinding; SYSTEM_GESTURE_HOTKEYS.len()],
}

impl Default for SymbolicHotkeys {
    fn default() -> Self {
        Self {
            bindings: [HotkeyBinding::Default; SYSTEM_GESTURE_HOTKEYS.len()],
        }
    }
}

impl SymbolicHotkeys {
    /// Ids outside [`SYSTEM_GESTURE_HOTKEYS`] are ignored.
    pub fn from_entries(entries: impl IntoIterator<Item = (u32, SymbolicHotkeyEntry)>) -> Self {
        let mut hotkeys = Self::default();
        for (id, entry) in entries {
            if let Some(index) = SYSTEM_GESTURE_HOTKEYS.iter().position(|known| *known == id) {
                hotkeys.bindings[index] = HotkeyBinding::from_entry(entry);
            }
        }
        hotkeys
    }

    fn binding(&self, id: u32) -> HotkeyBinding {
        SYSTEM_GESTURE_HOTKEYS
            .iter()
            .position(|known| *known == id)
            .map_or(HotkeyBinding::Default, |index| self.bindings[index])
    }
}

/// States only, never the keys of a custom binding.
impl fmt::Display for SymbolicHotkeys {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, (id, binding)) in SYSTEM_GESTURE_HOTKEYS.iter().zip(self.bindings).enumerate() {
            let state = match binding {
                HotkeyBinding::Default => "default",
                HotkeyBinding::Disabled => "disabled",
                HotkeyBinding::Chord(_) => "custom",
            };
            let separator = if index == 0 { "" } else { ", " };
            write!(formatter, "{separator}{id} {state}")?;
        }
        Ok(())
    }
}

const fn hotkey_id(gesture: SystemGesture) -> Option<u32> {
    Some(match gesture {
        SystemGesture::Overview => HOTKEY_MISSION_CONTROL,
        SystemGesture::AppWindows => HOTKEY_APPLICATION_WINDOWS,
        SystemGesture::DesktopPrevious => HOTKEY_SPACE_LEFT,
        SystemGesture::DesktopNext => HOTKEY_SPACE_RIGHT,
        SystemGesture::ShowDesktop => HOTKEY_SHOW_DESKTOP,
        SystemGesture::Launcher => HOTKEY_LAUNCHPAD,
        SystemGesture::NotificationCenter => HOTKEY_NOTIFICATION_CENTER,
        SystemGesture::Search => HOTKEY_SPOTLIGHT,
        SystemGesture::NavigateBack
        | SystemGesture::NavigateForward
        | SystemGesture::SwitchAppPrevious
        | SystemGesture::SwitchAppNext => return None,
    })
}

/// The user's live binding for `gesture`, else its default chord; App Exposé falls back to
/// Mission Control. None means no macOS action: the gesture is dropped, never replaced by an app.
pub fn system_gesture_chord(gesture: SystemGesture, hotkeys: &SymbolicHotkeys) -> Option<MacChord> {
    let binding = hotkey_id(gesture).map_or(HotkeyBinding::Default, |id| hotkeys.binding(id));
    match binding {
        HotkeyBinding::Chord(chord) => Some(chord),
        HotkeyBinding::Default => system_chord(Platform::MacOs, gesture).map(MacChord::new),
        HotkeyBinding::Disabled if gesture == SystemGesture::AppWindows => {
            system_gesture_chord(SystemGesture::Overview, hotkeys)
        }
        HotkeyBinding::Disabled => None,
    }
}

#[cfg(test)]
mod tests {
    use monhop_core::chord::{MAC_LOOK_UP, MAC_ZOOM_IN, MAC_ZOOM_OUT};

    use super::*;

    const FN: u64 = MAC_EVENT_FLAG_SECONDARY_FN;
    const KEYPAD: u64 = MAC_EVENT_FLAG_NUMERIC_PAD;
    const CONTROL: u64 = MAC_EVENT_FLAG_CONTROL;
    const COMMAND: u64 = MAC_EVENT_FLAG_COMMAND;
    const SHIFT: u64 = MAC_EVENT_FLAG_SHIFT;
    // macOS virtual key codes.
    const VK_CONTROL: u16 = 0x3B;
    const VK_COMMAND: u16 = 0x37;
    const VK_SHIFT: u16 = 0x38;
    const VK_LEFT: u16 = 0x7B;
    const VK_UP: u16 = 0x7E;
    const VK_F11: u16 = 0x67;
    const VK_EQUAL: u16 = 0x18;
    const VK_D: u16 = 0x02;
    const VK_SPACE: u16 = 0x31;

    fn events(held: &[u16], chord: MacChord) -> Vec<(u16, bool, u64)> {
        chord_key_events(held.iter().map(|usage| HidUsage(*usage)), chord)
            .unwrap()
            .into_iter()
            .map(|event| (event.key.0, event.pressed, event.flags))
            .collect()
    }

    fn default_chord(gesture: SystemGesture) -> MacChord {
        system_gesture_chord(gesture, &SymbolicHotkeys::default())
            .unwrap_or_else(|| panic!("{gesture:?} has a default chord"))
    }

    fn entry(enabled: bool, binding: StoredBinding) -> SymbolicHotkeyEntry {
        SymbolicHotkeyEntry { enabled, binding }
    }

    /// `magnify` deltas whose logarithms are `steps` zoom steps each.
    fn delta(steps: f64) -> f64 {
        PINCH_SCALE_PER_ZOOM_STEP.powf(steps) - 1.0
    }

    #[test]
    fn an_arrow_chord_carries_the_keypad_and_fn_flags_real_arrows_carry() {
        assert_eq!(
            events(&[], default_chord(SystemGesture::DesktopPrevious)),
            [
                (VK_CONTROL, true, CONTROL),
                (VK_LEFT, true, CONTROL | KEYPAD | FN),
                (VK_LEFT, false, CONTROL | KEYPAD | FN),
                (VK_CONTROL, false, 0),
            ]
        );
        assert_eq!(
            events(&[], default_chord(SystemGesture::ShowDesktop)),
            [(VK_F11, true, FN), (VK_F11, false, FN)],
            "a function key carries Fn and no keypad flag"
        );
    }

    #[test]
    fn a_wire_held_modifier_is_neither_pressed_again_nor_released_but_stays_in_the_flags() {
        const RIGHT_CONTROL: u16 = 0xE4;
        const LETTER_A: u16 = 0x04;
        assert_eq!(
            events(
                &[RIGHT_CONTROL, LETTER_A],
                default_chord(SystemGesture::Overview)
            ),
            [
                (VK_UP, true, CONTROL | KEYPAD | FN),
                (VK_UP, false, CONTROL | KEYPAD | FN)
            ]
        );
        assert!(
            events(&[0x52], default_chord(SystemGesture::Overview)).is_empty(),
            "a chord never touches a key the wire holds"
        );
    }

    #[test]
    fn look_up_and_zoom_steps_are_plain_command_chords() {
        assert_eq!(
            events(&[], MacChord::new(MAC_LOOK_UP)),
            [
                (VK_CONTROL, true, CONTROL),
                (VK_COMMAND, true, CONTROL | COMMAND),
                (VK_D, true, CONTROL | COMMAND),
                (VK_D, false, CONTROL | COMMAND),
                (VK_COMMAND, false, CONTROL),
                (VK_CONTROL, false, 0),
            ]
        );
        assert_eq!(
            events(&[], MacChord::new(MAC_ZOOM_IN)),
            [
                (VK_COMMAND, true, COMMAND),
                (VK_EQUAL, true, COMMAND),
                (VK_EQUAL, false, COMMAND),
                (VK_COMMAND, false, 0),
            ]
        );
        assert_eq!(MacChord::new(MAC_ZOOM_OUT).key_flags, 0);
    }

    /// Posts `events`, failing the post at `fail_at`; returns every attempted post in order.
    fn post_failing_at(events: &[ChordKeyEvent], fail_at: usize) -> Vec<(u16, bool)> {
        let mut attempts = Vec::new();
        let mut stuck = StuckChordKeys::default();
        let result = post_chord_events(events, &mut stuck, |event| {
            attempts.push((event.key.0, event.pressed));
            if attempts.len() == fail_at + 1 {
                Err(MacError::NativeEventCreationFailed)
            } else {
                Ok(())
            }
        });
        assert_eq!(result, Err(MacError::NativeEventCreationFailed));
        assert!(stuck.is_empty(), "every rescue release went through");
        attempts
    }

    #[test]
    fn a_failed_post_releases_what_the_chord_left_down_in_its_own_order() {
        let look_up = chord_key_events([], MacChord::new(MAC_LOOK_UP)).unwrap();
        assert_eq!(
            post_failing_at(&look_up, 0),
            [(VK_CONTROL, true)],
            "nothing was pressed yet"
        );
        assert_eq!(
            post_failing_at(&look_up, 2),
            [
                (VK_CONTROL, true),
                (VK_COMMAND, true),
                (VK_D, true),
                (VK_COMMAND, false),
                (VK_CONTROL, false),
            ],
            "the key never went down, so only the modifiers are released"
        );
        assert_eq!(
            post_failing_at(&look_up, 3),
            [
                (VK_CONTROL, true),
                (VK_COMMAND, true),
                (VK_D, true),
                (VK_D, false),
                (VK_D, false),
                (VK_COMMAND, false),
                (VK_CONTROL, false),
            ],
            "a key whose release failed is released again first"
        );
        assert_eq!(
            post_failing_at(&look_up, 5),
            [
                (VK_CONTROL, true),
                (VK_COMMAND, true),
                (VK_D, true),
                (VK_D, false),
                (VK_COMMAND, false),
                (VK_CONTROL, false),
                (VK_CONTROL, false),
            ]
        );
        let mut posted = Vec::new();
        post_chord_events(&look_up, &mut StuckChordKeys::default(), |event| {
            posted.push(event);
            Ok(())
        })
        .unwrap();
        assert_eq!(posted, look_up);
    }

    #[test]
    fn a_release_that_fails_too_stays_tracked_until_a_retry_frees_it() {
        let look_up = chord_key_events([], MacChord::new(MAC_LOOK_UP)).unwrap();
        let mut stuck = StuckChordKeys::default();
        let mut attempts = Vec::new();
        // D fails to go down, then Command's rescue release fails as well.
        let result = post_chord_events(&look_up, &mut stuck, |event| {
            attempts.push((event.key.0, event.pressed));
            match (event.key.0, event.pressed) {
                (VK_D, true) | (VK_COMMAND, false) => Err(MacError::NativeEventCreationFailed),
                _ => Ok(()),
            }
        });
        assert_eq!(result, Err(MacError::NativeEventCreationFailed));
        assert_eq!(
            attempts,
            [
                (VK_CONTROL, true),
                (VK_COMMAND, true),
                (VK_D, true),
                (VK_COMMAND, false),
                (VK_CONTROL, false),
            ]
        );

        let mut retried = Vec::new();
        assert_eq!(
            stuck.release(|event| {
                retried.push((event.key.0, event.pressed, event.flags));
                Err(MacError::AccessibilityPermissionRequired)
            }),
            Err(MacError::AccessibilityPermissionRequired),
            "a retry that fails keeps the key"
        );
        assert!(!stuck.is_empty());
        assert_eq!(
            stuck.release(|event| {
                retried.push((event.key.0, event.pressed, event.flags));
                Ok(())
            }),
            Ok(())
        );
        assert!(stuck.is_empty());
        assert_eq!(
            retried,
            [(VK_COMMAND, false, CONTROL), (VK_COMMAND, false, CONTROL)],
            "the retry posts the chord's own release of Command"
        );
        assert_eq!(stuck.release(|_| panic!("nothing is left down")), Ok(()));
    }

    #[test]
    fn the_wire_takes_over_a_stuck_key_it_presses_or_releases() {
        let look_up = chord_key_events([], MacChord::new(MAC_LOOK_UP)).unwrap();
        let mut stuck = StuckChordKeys::default();
        let _ = post_chord_events(&look_up, &mut stuck, |event| match event.key.0 {
            VK_D => Err(MacError::NativeEventCreationFailed),
            _ if !event.pressed => Err(MacError::NativeEventCreationFailed),
            _ => Ok(()),
        });
        stuck.forget(MacVirtualKey(VK_COMMAND));
        let mut retried = Vec::new();
        stuck
            .release(|event| {
                retried.push(event.key.0);
                Ok(())
            })
            .unwrap();
        assert_eq!(retried, [VK_CONTROL]);
    }

    #[test]
    fn pinch_steps_accumulate_their_residual_and_drop_it_at_the_end() {
        let mut pinch = PinchSteps::default();
        assert_eq!(pinch.steps(GesturePhase::Began, delta(0.6)), 0);
        assert_eq!(pinch.steps(GesturePhase::Changed, delta(0.6)), 1);
        assert_eq!(pinch.steps(GesturePhase::Changed, delta(-0.3)), 0);
        assert_eq!(pinch.steps(GesturePhase::Changed, delta(-1.6)), -1);
        assert_eq!(
            pinch.steps(GesturePhase::Ended, delta(0.6)),
            0,
            "the end applies its own delta, then drops what is left"
        );
        assert_eq!(pinch.steps(GesturePhase::Changed, delta(0.6)), 0);
        assert_eq!(
            pinch.steps(GesturePhase::Began, delta(0.6)),
            0,
            "a new pinch starts from nothing"
        );
        assert_eq!(pinch.steps(GesturePhase::Cancelled, delta(3.0)), 0);
        assert_eq!(pinch.steps(GesturePhase::Changed, delta(0.6)), 0);
        pinch.reset();
        assert_eq!(pinch.steps(GesturePhase::Changed, delta(0.6)), 0);
    }

    #[test]
    fn a_huge_or_invalid_pinch_record_posts_a_bounded_number_of_steps() {
        let limit = MAX_PINCH_STEPS_PER_RECORD as i32;
        let mut pinch = PinchSteps::default();
        assert_eq!(pinch.steps(GesturePhase::Began, 4.0), limit);
        assert_eq!(pinch.steps(GesturePhase::Changed, -0.999_999), -limit);
        assert_eq!(pinch.steps(GesturePhase::Changed, delta(0.6)), 0);
        for invalid in [-1.0, -2.0, f64::NAN, f64::INFINITY] {
            assert_eq!(pinch.steps(GesturePhase::Changed, invalid), 0);
        }
    }

    #[test]
    fn default_bindings_are_the_macos_defaults() {
        let defaults = SymbolicHotkeys::default();
        for gesture in [
            SystemGesture::Overview,
            SystemGesture::AppWindows,
            SystemGesture::DesktopPrevious,
            SystemGesture::DesktopNext,
            SystemGesture::ShowDesktop,
            SystemGesture::NavigateBack,
            SystemGesture::NavigateForward,
            SystemGesture::Search,
            SystemGesture::SwitchAppPrevious,
            SystemGesture::SwitchAppNext,
        ] {
            assert_eq!(
                system_gesture_chord(gesture, &defaults),
                system_chord(Platform::MacOs, gesture).map(MacChord::new),
                "{gesture:?}"
            );
        }
        for unbound in [SystemGesture::Launcher, SystemGesture::NotificationCenter] {
            assert_eq!(
                system_gesture_chord(unbound, &defaults),
                None,
                "{unbound:?} has no default binding and no app fallback"
            );
        }
    }

    #[test]
    fn a_disabled_hotkey_does_nothing_but_app_windows_falls_back_to_overview() {
        let disabled = |ids: &[u32]| {
            SymbolicHotkeys::from_entries(
                ids.iter()
                    .map(|id| (*id, entry(false, StoredBinding::Default))),
            )
        };
        let no_mission_control = disabled(&[HOTKEY_MISSION_CONTROL]);
        assert_eq!(
            system_gesture_chord(SystemGesture::Overview, &no_mission_control),
            None
        );
        assert_eq!(
            system_gesture_chord(SystemGesture::AppWindows, &no_mission_control),
            Some(default_chord(SystemGesture::AppWindows)),
            "App Exposé keeps its own binding"
        );
        assert_eq!(
            system_gesture_chord(
                SystemGesture::AppWindows,
                &disabled(&[HOTKEY_APPLICATION_WINDOWS])
            ),
            Some(default_chord(SystemGesture::Overview)),
            "App Exposé falls back to Mission Control"
        );
        assert_eq!(
            system_gesture_chord(
                SystemGesture::AppWindows,
                &disabled(&[HOTKEY_APPLICATION_WINDOWS, HOTKEY_MISSION_CONTROL])
            ),
            None
        );
        let none = disabled(&[
            HOTKEY_SPACE_LEFT,
            HOTKEY_SPACE_RIGHT,
            HOTKEY_SHOW_DESKTOP,
            HOTKEY_SPOTLIGHT,
            HOTKEY_LAUNCHPAD,
            HOTKEY_NOTIFICATION_CENTER,
        ]);
        for gesture in [
            SystemGesture::DesktopPrevious,
            SystemGesture::DesktopNext,
            SystemGesture::ShowDesktop,
            SystemGesture::Search,
            SystemGesture::Launcher,
            SystemGesture::NotificationCenter,
        ] {
            assert_eq!(system_gesture_chord(gesture, &none), None, "{gesture:?}");
        }
    }

    #[test]
    fn a_custom_binding_replaces_the_default_with_its_own_flags() {
        let bound = |parameters| {
            SymbolicHotkeys::from_entries([(
                HOTKEY_SPOTLIGHT,
                entry(true, StoredBinding::Parameters(parameters)),
            )])
        };
        // Cmd+Shift+Space, as the preference stores it.
        let chord = system_gesture_chord(
            SystemGesture::Search,
            &bound([32, i64::from(VK_SPACE), 0x0012_0000]),
        )
        .expect("a custom binding posts its chord");
        assert_eq!(
            events(&[], chord),
            [
                (VK_SHIFT, true, SHIFT),
                (VK_COMMAND, true, SHIFT | COMMAND),
                (VK_SPACE, true, SHIFT | COMMAND),
                (VK_SPACE, false, SHIFT | COMMAND),
                (VK_COMMAND, false, SHIFT),
                (VK_SHIFT, false, 0),
            ]
        );
        // A binding the user set to Fn+F11 keeps the stored Fn on its key.
        let launcher = SymbolicHotkeys::from_entries([(
            HOTKEY_LAUNCHPAD,
            entry(
                true,
                StoredBinding::Parameters([0xFFFF, i64::from(VK_F11), 0x0080_0000]),
            ),
        )]);
        let chord = system_gesture_chord(SystemGesture::Launcher, &launcher)
            .expect("a bound launcher posts its chord");
        assert_eq!(
            events(&[], chord),
            [(VK_F11, true, FN), (VK_F11, false, FN)]
        );
    }

    #[test]
    fn a_binding_monhop_cannot_reproduce_counts_as_disabled() {
        for binding in [
            StoredBinding::Parameters([0xFFFF, NO_KEY, 0]),
            StoredBinding::Parameters([0, 0x1_0000, 0]),
            StoredBinding::Parameters([0, 0x66, 0]),
            StoredBinding::Parameters([0, i64::from(VK_SPACE), -1]),
            StoredBinding::Unreadable,
        ] {
            let hotkeys = SymbolicHotkeys::from_entries([(HOTKEY_SPOTLIGHT, entry(true, binding))]);
            assert_eq!(
                system_gesture_chord(SystemGesture::Search, &hotkeys),
                None,
                "{binding:?}"
            );
        }
    }

    #[test]
    fn an_enabled_entry_without_a_value_keeps_its_default() {
        // As this Mac stores 79 and 81.
        let hotkeys = SymbolicHotkeys::from_entries([
            (HOTKEY_SPACE_LEFT, entry(true, StoredBinding::Default)),
            (999, entry(false, StoredBinding::Default)),
        ]);
        assert_eq!(hotkeys, SymbolicHotkeys::default());
        assert_eq!(
            hotkeys.to_string(),
            "32 default, 33 default, 36 default, 64 default, 79 default, 81 default, \
             160 default, 163 default"
        );
        let mixed = SymbolicHotkeys::from_entries([
            (
                HOTKEY_SPOTLIGHT,
                entry(false, StoredBinding::Parameters([32, 49, 0x0010_0000])),
            ),
            (
                HOTKEY_SHOW_DESKTOP,
                entry(true, StoredBinding::Parameters([0xFFFF, 0x67, 0])),
            ),
        ]);
        assert_eq!(
            mixed.to_string(),
            "32 default, 33 default, 36 custom, 64 disabled, 79 default, 81 default, \
             160 default, 163 default"
        );
    }
}
