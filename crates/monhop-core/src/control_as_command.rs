//! Shortcuts typed across platforms: a Windows keyboard on a Mac with Ctrl as Command, or a Mac
//! keyboard on Windows with Command as Ctrl. Either way Ctrl and Command trade places and so do the
//! Windows key and Control, except where a habit needs another modifier there. A key the destination
//! sees stays down while any physical key maps to it, and a release always answers the press it
//! follows.

use crate::{
    HID_BACKSPACE, HID_DELETE_FORWARD, HID_LEFT_ARROW, HID_RIGHT_ARROW, HID_TAB, HidUsage,
    Platform, SemanticModifier, SemanticModifierKind,
    chord::{KeyPlan, MAX_CHORD_STEPS},
    dimming::DIM_TOGGLE,
    semantic_modifier,
};

// The key itself plus: a Windows keyboard's four following keys (both Ctrl, both Alt) each retarget
// with one release and one press; a Mac keyboard's six never move one Windows key twice in a plan,
// so at most the six keys they target change. `every_reachable_plan_fits` walks both.
const _: () = assert!(4 * 2 < MAX_CHORD_STEPS);
const _: () = assert!(6 < MAX_CHORD_STEPS);

/// Whose keyboard drives which computer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Ctrl acts as Command and the Windows key as Control.
    WindowsOnMac,
    /// Command acts as Ctrl and Control as the Windows key.
    MacOnWindows,
}

impl Direction {
    /// Only a keyboard on the other platform is translated; an unknown one never is.
    pub fn between(keyboard: Option<Platform>, destination: Option<Platform>) -> Option<Self> {
        match (keyboard?, destination?) {
            (Platform::Windows, Platform::MacOs) => Some(Self::WindowsOnMac),
            (Platform::MacOs, Platform::Windows) => Some(Self::MacOnWindows),
            _ => None,
        }
    }

    /// Windows opens Start or the menu bar when the Windows key or Alt comes up alone, so there a
    /// retarget presses the new key before it lifts the old one.
    fn presses_first(self) -> bool {
        self == Self::MacOnWindows
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Held {
    /// What the destination holds for this physical key.
    target: HidUsage,
    /// A modifier pressed while translating, whose target follows the key it combines with.
    follows: bool,
    /// A modifier that opened the app switcher keeps its target until it is released.
    switching: bool,
}

impl Held {
    const fn fixed(target: HidUsage) -> Self {
        Self {
            target,
            follows: false,
            switching: false,
        }
    }
}

/// The physical modifier kinds held, either side.
#[derive(Clone, Copy, Default)]
struct Kinds {
    control: bool,
    shift: bool,
    alt: bool,
    system: bool,
}

#[derive(Clone, Debug)]
pub struct Translator {
    held: [Option<Held>; 256],
}

impl Default for Translator {
    fn default() -> Self {
        Self { held: [None; 256] }
    }
}

impl Translator {
    /// The steps that deliver one physical key. `translate` (the switch is on and the keyboard is
    /// the other platform's) decides presses only: a release always follows the press it answers.
    pub fn key(&mut self, usage: HidUsage, pressed: bool, translate: Option<Direction>) -> KeyPlan {
        let mut plan = KeyPlan::EMPTY;
        let Some(index) = self.index(usage) else {
            plan.push(usage, pressed);
            return plan;
        };
        if !pressed {
            let target = self.held[index].take().map_or(usage, |held| held.target);
            if !self.holds(target) {
                plan.push(target, false);
            }
            return plan;
        }
        let modifier = semantic_modifier(usage);
        if let Some(direction) = translate
            && modifier.is_none()
        {
            self.follow(direction, Some(usage), &mut plan);
        }
        if let Some(held) = self.held[index] {
            plan.push(held.target, true);
            return plan;
        }
        let held = match (modifier, translate) {
            (Some(modifier), Some(direction)) => first(direction, modifier),
            _ => Held::fixed(usage),
        };
        let shared = self.holds(held.target);
        self.held[index] = Some(held);
        if !shared {
            plan.push(held.target, true);
        }
        plan
    }

    /// The steps before a mouse press: each held modifier is back on its own key, so Ctrl+click on
    /// a Mac is Command-click and Command+click on Windows is Ctrl-click.
    pub fn click(&mut self, translate: Option<Direction>) -> KeyPlan {
        let mut plan = KeyPlan::EMPTY;
        if let Some(direction) = translate {
            self.follow(direction, None, &mut plan);
        }
        plan
    }

    /// The destination released every key.
    pub fn clear(&mut self) {
        self.held = [None; 256];
    }

    fn index(&self, usage: HidUsage) -> Option<usize> {
        let index = usize::from(usage.0);
        (index < self.held.len()).then_some(index)
    }

    fn holds(&self, target: HidUsage) -> bool {
        self.held.iter().flatten().any(|held| held.target == target)
    }

    fn kinds(&self) -> Kinds {
        let mut kinds = Kinds::default();
        for usage in 0xE0..=0xE7 {
            if self.held[usize::from(usage)].is_some()
                && let Some(modifier) = semantic_modifier(HidUsage(usage))
            {
                match modifier.kind {
                    SemanticModifierKind::Control => kinds.control = true,
                    SemanticModifierKind::Shift => kinds.shift = true,
                    SemanticModifierKind::Alternate => kinds.alt = true,
                    SemanticModifierKind::System => kinds.system = true,
                }
            }
        }
        kinds
    }

    /// Moves each following modifier to the target `key` wants (None: a click), releasing the old
    /// target unless another key holds it and pressing the new one unless another already does.
    fn follow(&mut self, direction: Direction, key: Option<HidUsage>, plan: &mut KeyPlan) {
        let kinds = self.kinds();
        for usage in 0xE0..=0xE7 {
            let index = usize::from(usage);
            let Some(held) = self.held[index].filter(|held| held.follows) else {
                continue;
            };
            let Some(modifier) = semantic_modifier(HidUsage(usage)) else {
                continue;
            };
            let next = wanted(direction, modifier, held, key, kinds);
            let moves = next.target != held.target;
            let press = (next.target, true, moves && !self.holds(next.target));
            self.held[index] = Some(next);
            let release = (held.target, false, moves && !self.holds(held.target));
            let steps = if direction.presses_first() {
                [press, release]
            } else {
                [release, press]
            };
            for (target, pressed, needed) in steps {
                if needed {
                    plan.push(target, pressed);
                }
            }
        }
    }
}

/// Ctrl and Command trade places, and so do the Windows key and Control; Alt and Shift stay. From a
/// Windows keyboard Ctrl and Alt follow the next key; from a Mac keyboard Command, Control and
/// Option all do.
fn first(direction: Direction, modifier: SemanticModifier) -> Held {
    use SemanticModifierKind::{Alternate, Control, Shift, System};
    let kind = match modifier.kind {
        Control => System,
        System => Control,
        other => other,
    };
    let follows = match direction {
        Direction::WindowsOnMac => matches!(modifier.kind, Control | Alternate),
        Direction::MacOnWindows => modifier.kind != Shift,
    };
    Held {
        target: SemanticModifier { kind, ..modifier }.usage(),
        follows,
        switching: false,
    }
}

/// Where a habit means another modifier on the destination, the dimming chord keeping its keys.
/// A Windows keyboard on a Mac: Ctrl+Tab switches tabs with Control, Ctrl+arrow and Ctrl+Backspace
/// or Delete act by word with Option, and Alt+Tab switches apps with Command until Alt is released.
/// A Mac keyboard on Windows: Control+Tab switches tabs with Ctrl, Option+arrow and Option+Backspace
/// or Delete act by word with Ctrl, and Command+Tab switches apps with Alt until Command is released;
/// Option+Tab keeps Alt the same way.
fn wanted(
    direction: Direction,
    modifier: SemanticModifier,
    held: Held,
    key: Option<HidUsage>,
    kinds: Kinds,
) -> Held {
    use Direction::{MacOnWindows, WindowsOnMac};
    use SemanticModifierKind::{Alternate, Control, System};
    let tab = key == Some(HID_TAB);
    let switches = held.switching || tab;
    let control_stays = tab || key.is_some_and(|key| dims(key, kinds));
    let word = matches!(
        key,
        Some(HID_LEFT_ARROW | HID_RIGHT_ARROW | HID_BACKSPACE | HID_DELETE_FORWARD)
    );
    let (kind, switching) = match (direction, modifier.kind) {
        (WindowsOnMac, Control) if control_stays => (Control, false),
        (WindowsOnMac, Control) if word && !kinds.alt && !kinds.system => (Alternate, false),
        (WindowsOnMac, Control) => (System, false),
        (WindowsOnMac, Alternate) if switches => (System, true),
        (MacOnWindows, System) if switches => (Alternate, true),
        (MacOnWindows, System) => (Control, false),
        (MacOnWindows, Control) if control_stays => (Control, false),
        (MacOnWindows, Control) => (System, false),
        (MacOnWindows, Alternate) if switches => (Alternate, true),
        (MacOnWindows, Alternate) if word && !kinds.control && !kinds.system => (Control, false),
        (_, kind) => (kind, false),
    };
    Held {
        target: SemanticModifier { kind, ..modifier }.usage(),
        switching,
        ..held
    }
}

fn dims(key: HidUsage, kinds: Kinds) -> bool {
    key == DIM_TOGGLE.key
        && kinds.control == DIM_TOGGLE.control
        && kinds.alt == DIM_TOGGLE.alt
        && kinds.shift == DIM_TOGGLE.shift
        && kinds.system == DIM_TOGGLE.command
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashSet};

    use super::*;
    use crate::chord::KeyStep;

    const WINDOWS_KEYBOARD: Option<Direction> = Some(Direction::WindowsOnMac);
    const MAC_KEYBOARD: Option<Direction> = Some(Direction::MacOnWindows);
    const L_CTRL: HidUsage = HidUsage(0xE0);
    const L_SHIFT: HidUsage = HidUsage(0xE1);
    const L_ALT: HidUsage = HidUsage(0xE2);
    const L_WIN: HidUsage = HidUsage(0xE3);
    const R_CTRL: HidUsage = HidUsage(0xE4);
    const R_ALT: HidUsage = HidUsage(0xE6);
    const R_WIN: HidUsage = HidUsage(0xE7);
    // On a Mac the same usages read as Control, Shift, Option and Command.
    const L_CONTROL: HidUsage = L_CTRL;
    const L_OPTION: HidUsage = L_ALT;
    const L_COMMAND: HidUsage = L_WIN;
    const R_CONTROL: HidUsage = R_CTRL;
    const R_OPTION: HidUsage = R_ALT;
    const R_COMMAND: HidUsage = R_WIN;
    const C: HidUsage = HidUsage(0x06);
    const W: HidUsage = HidUsage(0x1A);
    const ZERO: HidUsage = DIM_TOGGLE.key;
    const UP_ARROW: HidUsage = HidUsage(0x52);
    const WORD_KEYS: [HidUsage; 4] = [
        HID_LEFT_ARROW,
        HID_RIGHT_ARROW,
        HID_BACKSPACE,
        HID_DELETE_FORWARD,
    ];

    fn down(usage: HidUsage) -> KeyStep {
        KeyStep {
            usage,
            pressed: true,
        }
    }

    fn up(usage: HidUsage) -> KeyStep {
        KeyStep {
            usage,
            pressed: false,
        }
    }

    fn feed(
        keys: &mut Translator,
        translate: Option<Direction>,
        steps: &[KeyStep],
    ) -> Vec<KeyStep> {
        steps
            .iter()
            .flat_map(|step| {
                keys.key(step.usage, step.pressed, translate)
                    .steps()
                    .to_vec()
            })
            .collect()
    }

    /// Feeds a Windows keyboard's keys with its switch on and collects what the Mac receives.
    fn on(keys: &mut Translator, steps: &[KeyStep]) -> Vec<KeyStep> {
        feed(keys, WINDOWS_KEYBOARD, steps)
    }

    /// Feeds a Mac keyboard's keys with its switch on and collects what Windows receives.
    fn on_windows(keys: &mut Translator, steps: &[KeyStep]) -> Vec<KeyStep> {
        feed(keys, MAC_KEYBOARD, steps)
    }

    /// What the destination holds after `plan`.
    fn holding(plan: &[KeyStep]) -> BTreeSet<HidUsage> {
        let mut held = BTreeSet::new();
        for step in plan {
            if step.pressed {
                held.insert(step.usage);
            } else {
                held.remove(&step.usage);
            }
        }
        held
    }

    #[test]
    fn every_modifier_usage_round_trips_through_its_meaning() {
        for usage in 0xE0..=0xE7 {
            let modifier = semantic_modifier(HidUsage(usage)).unwrap();
            assert_eq!(modifier.usage(), HidUsage(usage));
        }
    }

    #[test]
    fn only_a_keyboard_on_the_other_platform_is_translated() {
        use Platform::{MacOs, Windows};
        assert_eq!(
            Direction::between(Some(Windows), Some(MacOs)),
            WINDOWS_KEYBOARD
        );
        assert_eq!(Direction::between(Some(MacOs), Some(Windows)), MAC_KEYBOARD);
        for (keyboard, destination) in [
            (Some(Windows), Some(Windows)),
            (Some(MacOs), Some(MacOs)),
            (None, Some(MacOs)),
            (None, Some(Windows)),
            (Some(Windows), None),
        ] {
            assert_eq!(Direction::between(keyboard, destination), None);
        }
    }

    #[test]
    fn ctrl_is_command_and_the_windows_key_is_control_on_each_side() {
        let mut keys = Translator::default();
        assert_eq!(
            on(&mut keys, &[down(L_CTRL), down(C), up(C), up(L_CTRL)]),
            [down(L_COMMAND), down(C), up(C), up(L_COMMAND)]
        );
        assert_eq!(
            on(&mut keys, &[down(R_CTRL), down(C), up(C), up(R_CTRL)]),
            [down(R_COMMAND), down(C), up(C), up(R_COMMAND)]
        );
        assert_eq!(
            on(&mut keys, &[down(L_WIN), up(L_WIN), down(R_WIN), up(R_WIN)]),
            [
                down(L_CONTROL),
                up(L_CONTROL),
                down(R_CONTROL),
                up(R_CONTROL)
            ]
        );
        assert_eq!(
            on(&mut keys, &[down(L_ALT), down(L_SHIFT), down(C)]),
            [down(L_OPTION), down(L_SHIFT), down(C)]
        );
    }

    #[test]
    fn ctrl_tab_switches_tabs_with_control_and_the_next_shortcut_is_command_again() {
        let mut keys = Translator::default();
        assert_eq!(on(&mut keys, &[down(L_CTRL)]), [down(L_COMMAND)]);
        assert_eq!(
            on(&mut keys, &[down(L_SHIFT), down(HID_TAB)]),
            [down(L_SHIFT), up(L_COMMAND), down(L_CONTROL), down(HID_TAB)]
        );
        assert_eq!(
            on(&mut keys, &[up(HID_TAB), up(L_SHIFT), down(HID_TAB)]),
            [up(HID_TAB), up(L_SHIFT), down(HID_TAB)]
        );
        assert_eq!(
            on(&mut keys, &[up(HID_TAB), down(W)]),
            [up(HID_TAB), up(L_CONTROL), down(L_COMMAND), down(W)]
        );
        assert_eq!(on(&mut keys, &[up(W), up(L_CTRL)]), [up(W), up(L_COMMAND)]);
    }

    #[test]
    fn alt_tab_switches_apps_and_keeps_command_until_alt_is_released() {
        let mut keys = Translator::default();
        assert_eq!(
            on(&mut keys, &[down(L_ALT), down(HID_TAB), up(HID_TAB)]),
            [
                down(L_OPTION),
                up(L_OPTION),
                down(L_COMMAND),
                down(HID_TAB),
                up(HID_TAB)
            ]
        );
        assert_eq!(
            on(
                &mut keys,
                &[
                    down(HID_TAB),
                    up(HID_TAB),
                    down(HID_LEFT_ARROW),
                    up(HID_LEFT_ARROW)
                ]
            ),
            [
                down(HID_TAB),
                up(HID_TAB),
                down(HID_LEFT_ARROW),
                up(HID_LEFT_ARROW)
            ],
            "arrows move through the switcher without closing it"
        );
        assert!(
            keys.click(WINDOWS_KEYBOARD).is_empty(),
            "a click picks in the switcher"
        );
        assert_eq!(on(&mut keys, &[up(L_ALT)]), [up(L_COMMAND)]);
        assert_eq!(
            on(&mut keys, &[down(L_ALT), down(C)]),
            [down(L_OPTION), down(C)],
            "a fresh Alt is Option again"
        );
    }

    #[test]
    fn ctrl_moves_and_deletes_by_word_with_option() {
        for key in WORD_KEYS {
            let mut keys = Translator::default();
            assert_eq!(
                on(
                    &mut keys,
                    &[down(L_CTRL), down(L_SHIFT), down(key), up(key)]
                ),
                [
                    down(L_COMMAND),
                    down(L_SHIFT),
                    up(L_COMMAND),
                    down(L_OPTION),
                    down(key),
                    up(key)
                ]
            );
            assert_eq!(on(&mut keys, &[up(L_CTRL)]), [up(L_OPTION)]);
        }
        let mut keys = Translator::default();
        assert_eq!(
            on(&mut keys, &[down(R_CTRL), down(UP_ARROW)]),
            [down(R_COMMAND), down(UP_ARROW)],
            "up and down keep Command: top and bottom"
        );
    }

    #[test]
    fn word_moves_need_ctrl_alone() {
        for other in [L_ALT, L_WIN] {
            let mut keys = Translator::default();
            let plan = on(
                &mut keys,
                &[down(L_CTRL), down(other), down(HID_LEFT_ARROW)],
            );
            assert!(plan.contains(&down(L_COMMAND)));
            assert!(!plan.contains(&down(L_OPTION)) || other == L_ALT);
            assert_eq!(plan.last(), Some(&down(HID_LEFT_ARROW)));
            assert!(!plan.contains(&up(L_COMMAND)));
        }
    }

    #[test]
    fn the_dimming_chord_keeps_its_own_keys() {
        let chord = std::hint::black_box(DIM_TOGGLE);
        assert!(chord.control && chord.alt && !chord.shift && !chord.command);
        for (first, second) in [(L_CTRL, L_ALT), (L_ALT, L_CTRL), (R_CTRL, R_ALT)] {
            let mut keys = Translator::default();
            let plan = on(&mut keys, &[down(first), down(second), down(ZERO)]);
            let control = if first == R_CTRL || second == R_CTRL {
                R_CONTROL
            } else {
                L_CONTROL
            };
            assert_eq!(plan.last(), Some(&down(ZERO)));
            let option = if control == R_CONTROL {
                R_OPTION
            } else {
                L_OPTION
            };
            assert_eq!(holding(&plan), BTreeSet::from([control, option, ZERO]));
        }
        let mut keys = Translator::default();
        let plan = on(
            &mut keys,
            &[down(L_CTRL), down(L_SHIFT), down(L_ALT), down(ZERO)],
        );
        assert!(
            !plan.contains(&up(L_COMMAND)),
            "with Shift it is not the chord"
        );
    }

    #[test]
    fn a_click_after_a_word_move_is_command_click() {
        let mut keys = Translator::default();
        on(
            &mut keys,
            &[down(L_CTRL), down(HID_LEFT_ARROW), up(HID_LEFT_ARROW)],
        );
        assert_eq!(
            keys.click(WINDOWS_KEYBOARD).steps(),
            [up(L_OPTION), down(L_COMMAND)]
        );
        assert!(keys.click(WINDOWS_KEYBOARD).is_empty());
        assert!(keys.click(None).is_empty());
        assert_eq!(on(&mut keys, &[up(L_CTRL)]), [up(L_COMMAND)]);
    }

    #[test]
    fn a_target_two_keys_share_is_pressed_once_and_released_by_the_last() {
        let mut keys = Translator::default();
        assert_eq!(
            on(&mut keys, &[down(L_WIN), down(L_CTRL), down(HID_TAB)]),
            [
                down(L_CONTROL),
                down(L_COMMAND),
                up(L_COMMAND),
                down(HID_TAB)
            ]
        );
        assert_eq!(on(&mut keys, &[up(L_WIN)]), [], "Ctrl still holds Control");
        assert_eq!(
            on(&mut keys, &[up(HID_TAB), up(L_CTRL)]),
            [up(HID_TAB), up(L_CONTROL)]
        );
    }

    #[test]
    fn the_switch_decides_presses_and_never_strands_a_release() {
        let mut keys = Translator::default();
        assert_eq!(
            keys.key(L_CTRL, true, WINDOWS_KEYBOARD).steps(),
            [down(L_COMMAND)]
        );
        assert_eq!(
            keys.key(L_CTRL, false, None).steps(),
            [up(L_COMMAND)],
            "turned off while held"
        );
        assert_eq!(keys.key(L_CTRL, true, None).steps(), [down(L_CONTROL)]);
        assert_eq!(
            keys.key(HID_TAB, true, WINDOWS_KEYBOARD).steps(),
            [down(HID_TAB)],
            "a Ctrl pressed while off never follows"
        );
        assert_eq!(
            keys.key(L_CTRL, false, WINDOWS_KEYBOARD).steps(),
            [up(L_CONTROL)]
        );
    }

    #[test]
    fn a_repeat_repeats_what_the_mac_holds() {
        let mut keys = Translator::default();
        assert_eq!(
            on(&mut keys, &[down(L_CTRL), down(L_CTRL), down(C), down(C)]),
            [down(L_COMMAND), down(L_COMMAND), down(C), down(C)]
        );
        on(&mut keys, &[down(HID_TAB)]);
        assert_eq!(on(&mut keys, &[down(L_CTRL)]), [down(L_CONTROL)]);
    }

    #[test]
    fn after_the_mac_releases_everything_a_late_release_lifts_nothing_held() {
        let mut keys = Translator::default();
        on(&mut keys, &[down(L_CTRL)]);
        keys.clear();
        assert_eq!(on(&mut keys, &[down(L_WIN)]), [down(L_CONTROL)]);
        assert_eq!(
            on(&mut keys, &[up(L_CTRL)]),
            [],
            "Control belongs to the Windows key"
        );
        assert_eq!(on(&mut keys, &[up(L_WIN)]), [up(L_CONTROL)]);
        assert_eq!(
            on(&mut keys, &[up(C)]),
            [up(C)],
            "an unknown release passes"
        );
    }

    #[test]
    fn the_worst_case_fits_one_plan() {
        let mut keys = Translator::default();
        on(
            &mut keys,
            &[down(L_CTRL), down(R_CTRL), down(L_ALT), down(R_ALT)],
        );
        let plan = keys.key(HID_TAB, true, WINDOWS_KEYBOARD);
        assert_eq!(plan.steps().len(), 9);
        assert_eq!(plan.steps().last(), Some(&down(HID_TAB)));
    }

    #[test]
    fn command_is_ctrl_and_control_is_the_windows_key_on_each_side() {
        let mut keys = Translator::default();
        assert_eq!(
            on_windows(&mut keys, &[down(L_COMMAND), down(C), up(C), up(L_COMMAND)]),
            [down(L_CTRL), down(C), up(C), up(L_CTRL)]
        );
        assert_eq!(
            on_windows(&mut keys, &[down(R_COMMAND), down(C), up(C), up(R_COMMAND)]),
            [down(R_CTRL), down(C), up(C), up(R_CTRL)]
        );
        assert_eq!(
            on_windows(
                &mut keys,
                &[
                    down(L_CONTROL),
                    up(L_CONTROL),
                    down(R_CONTROL),
                    up(R_CONTROL)
                ]
            ),
            [down(L_WIN), up(L_WIN), down(R_WIN), up(R_WIN)]
        );
        assert_eq!(
            on_windows(&mut keys, &[down(L_OPTION), down(L_SHIFT), down(C)]),
            [down(L_ALT), down(L_SHIFT), down(C)]
        );
    }

    #[test]
    fn control_tab_switches_tabs_with_ctrl_and_the_next_shortcut_is_the_windows_key_again() {
        let mut keys = Translator::default();
        assert_eq!(on_windows(&mut keys, &[down(L_CONTROL)]), [down(L_WIN)]);
        assert_eq!(
            on_windows(&mut keys, &[down(L_SHIFT), down(HID_TAB)]),
            [down(L_SHIFT), down(L_CTRL), up(L_WIN), down(HID_TAB)],
            "Ctrl goes down before the Windows key comes up, so Start stays closed"
        );
        assert_eq!(
            on_windows(&mut keys, &[up(HID_TAB), up(L_SHIFT), down(HID_TAB)]),
            [up(HID_TAB), up(L_SHIFT), down(HID_TAB)]
        );
        assert_eq!(
            on_windows(&mut keys, &[up(HID_TAB), down(W)]),
            [up(HID_TAB), down(L_WIN), up(L_CTRL), down(W)]
        );
        assert_eq!(
            on_windows(&mut keys, &[up(W), up(L_CONTROL)]),
            [up(W), up(L_WIN)]
        );
    }

    #[test]
    fn command_tab_switches_apps_and_keeps_alt_until_command_is_released() {
        let mut keys = Translator::default();
        assert_eq!(
            on_windows(&mut keys, &[down(L_COMMAND), down(HID_TAB), up(HID_TAB)]),
            [
                down(L_CTRL),
                down(L_ALT),
                up(L_CTRL),
                down(HID_TAB),
                up(HID_TAB)
            ]
        );
        assert_eq!(
            on_windows(
                &mut keys,
                &[
                    down(HID_TAB),
                    up(HID_TAB),
                    down(HID_LEFT_ARROW),
                    up(HID_LEFT_ARROW)
                ]
            ),
            [
                down(HID_TAB),
                up(HID_TAB),
                down(HID_LEFT_ARROW),
                up(HID_LEFT_ARROW)
            ],
            "arrows move through the switcher without closing it"
        );
        assert!(
            keys.click(MAC_KEYBOARD).is_empty(),
            "a click picks in the switcher"
        );
        assert_eq!(on_windows(&mut keys, &[up(L_COMMAND)]), [up(L_ALT)]);
        assert_eq!(
            on_windows(&mut keys, &[down(L_COMMAND), down(C)]),
            [down(L_CTRL), down(C)],
            "a fresh Command is Ctrl again"
        );
    }

    #[test]
    fn option_tab_keeps_alt_through_the_switcher() {
        let mut keys = Translator::default();
        assert_eq!(
            on_windows(
                &mut keys,
                &[down(L_OPTION), down(HID_TAB), down(HID_LEFT_ARROW)]
            ),
            [down(L_ALT), down(HID_TAB), down(HID_LEFT_ARROW)]
        );
        assert_eq!(on_windows(&mut keys, &[up(L_OPTION)]), [up(L_ALT)]);
    }

    #[test]
    fn option_moves_and_deletes_by_word_with_ctrl() {
        for key in WORD_KEYS {
            let mut keys = Translator::default();
            assert_eq!(
                on_windows(
                    &mut keys,
                    &[down(L_OPTION), down(L_SHIFT), down(key), up(key)]
                ),
                [
                    down(L_ALT),
                    down(L_SHIFT),
                    down(L_CTRL),
                    up(L_ALT),
                    down(key),
                    up(key)
                ],
                "Ctrl goes down before Alt comes up, so the menu bar stays closed"
            );
            assert_eq!(on_windows(&mut keys, &[up(L_OPTION)]), [up(L_CTRL)]);
        }
        let mut keys = Translator::default();
        assert_eq!(
            on_windows(&mut keys, &[down(R_OPTION), down(UP_ARROW)]),
            [down(R_ALT), down(UP_ARROW)],
            "up and down keep Alt"
        );
    }

    #[test]
    fn word_moves_need_option_alone() {
        for (other, target) in [(L_COMMAND, L_CTRL), (L_CONTROL, L_WIN)] {
            let mut keys = Translator::default();
            assert_eq!(
                on_windows(
                    &mut keys,
                    &[down(L_OPTION), down(other), down(HID_LEFT_ARROW)]
                ),
                [down(L_ALT), down(target), down(HID_LEFT_ARROW)]
            );
        }
    }

    #[test]
    fn the_dimming_chord_keeps_its_own_keys_on_windows() {
        for (first, second) in [
            (L_CONTROL, L_OPTION),
            (L_OPTION, L_CONTROL),
            (R_CONTROL, R_OPTION),
        ] {
            let mut keys = Translator::default();
            let plan = on_windows(&mut keys, &[down(first), down(second), down(ZERO)]);
            let (ctrl, alt) = if first == R_CONTROL {
                (R_CTRL, R_ALT)
            } else {
                (L_CTRL, L_ALT)
            };
            assert_eq!(plan.last(), Some(&down(ZERO)));
            assert_eq!(holding(&plan), BTreeSet::from([ctrl, alt, ZERO]));
        }
        let mut keys = Translator::default();
        let plan = on_windows(
            &mut keys,
            &[down(L_CONTROL), down(L_SHIFT), down(L_OPTION), down(ZERO)],
        );
        assert!(!plan.contains(&up(L_WIN)), "with Shift it is not the chord");
    }

    #[test]
    fn a_click_puts_option_back_on_alt_and_command_click_is_ctrl_click() {
        let mut keys = Translator::default();
        on_windows(
            &mut keys,
            &[down(L_OPTION), down(HID_LEFT_ARROW), up(HID_LEFT_ARROW)],
        );
        assert_eq!(keys.click(MAC_KEYBOARD).steps(), [down(L_ALT), up(L_CTRL)]);
        assert!(keys.click(MAC_KEYBOARD).is_empty());
        assert!(keys.click(None).is_empty());
        assert_eq!(on_windows(&mut keys, &[up(L_OPTION)]), [up(L_ALT)]);
        assert_eq!(on_windows(&mut keys, &[down(L_COMMAND)]), [down(L_CTRL)]);
        assert!(keys.click(MAC_KEYBOARD).is_empty());
    }

    #[test]
    fn a_target_two_keys_share_on_windows_is_pressed_once_and_released_by_the_last() {
        let mut keys = Translator::default();
        assert_eq!(
            on_windows(
                &mut keys,
                &[
                    down(L_OPTION),
                    down(HID_LEFT_ARROW),
                    up(HID_LEFT_ARROW),
                    down(L_COMMAND)
                ]
            ),
            [
                down(L_ALT),
                down(L_CTRL),
                up(L_ALT),
                down(HID_LEFT_ARROW),
                up(HID_LEFT_ARROW)
            ],
            "Option already holds Ctrl"
        );
        assert_eq!(
            on_windows(&mut keys, &[up(L_OPTION)]),
            [],
            "Command still holds Ctrl"
        );
        assert_eq!(on_windows(&mut keys, &[up(L_COMMAND)]), [up(L_CTRL)]);
    }

    #[test]
    fn the_switch_decides_presses_on_windows_and_never_strands_a_release() {
        let mut keys = Translator::default();
        assert_eq!(
            keys.key(L_COMMAND, true, MAC_KEYBOARD).steps(),
            [down(L_CTRL)]
        );
        assert_eq!(
            keys.key(L_COMMAND, false, None).steps(),
            [up(L_CTRL)],
            "turned off while held"
        );
        assert_eq!(keys.key(L_COMMAND, true, None).steps(), [down(L_WIN)]);
        assert_eq!(
            keys.key(HID_TAB, true, MAC_KEYBOARD).steps(),
            [down(HID_TAB)],
            "a Command pressed while off never follows"
        );
        assert_eq!(
            keys.key(L_COMMAND, false, MAC_KEYBOARD).steps(),
            [up(L_WIN)]
        );
    }

    #[test]
    fn a_repeat_repeats_what_windows_holds() {
        let mut keys = Translator::default();
        assert_eq!(
            on_windows(
                &mut keys,
                &[down(L_COMMAND), down(L_COMMAND), down(C), down(C)]
            ),
            [down(L_CTRL), down(L_CTRL), down(C), down(C)]
        );
        on_windows(&mut keys, &[down(HID_TAB)]);
        assert_eq!(on_windows(&mut keys, &[down(L_COMMAND)]), [down(L_ALT)]);
    }

    #[test]
    fn after_windows_releases_everything_a_late_release_lifts_nothing_held() {
        let mut keys = Translator::default();
        on_windows(&mut keys, &[down(L_COMMAND)]);
        keys.clear();
        assert_eq!(on_windows(&mut keys, &[down(L_CONTROL)]), [down(L_WIN)]);
        assert_eq!(
            on_windows(&mut keys, &[up(L_COMMAND)]),
            [],
            "the Windows key belongs to Control"
        );
        assert_eq!(on_windows(&mut keys, &[up(L_CONTROL)]), [up(L_WIN)]);
        assert_eq!(
            on_windows(&mut keys, &[up(C)]),
            [up(C)],
            "an unknown release passes"
        );
    }

    /// Every modifier state each direction can reach, and every event from each: `push` panics on a
    /// plan longer than one KeyPlan, and the longest found is pinned.
    #[test]
    fn every_reachable_plan_fits() {
        enum Event {
            Modifier(HidUsage, bool),
            Key(HidUsage),
            Click,
        }
        for (translate, worst) in [(WINDOWS_KEYBOARD, 9), (MAC_KEYBOARD, 5)] {
            let mut seen = HashSet::new();
            let mut pending = vec![Translator::default()];
            let mut longest = 0;
            while let Some(keys) = pending.pop() {
                let modifiers: [Option<Held>; 8] = std::array::from_fn(|i| keys.held[0xE0 + i]);
                if !seen.insert(modifiers) {
                    continue;
                }
                let events = (0xE0..=0xE7)
                    .flat_map(|usage| {
                        [true, false].map(|down| Event::Modifier(HidUsage(usage), down))
                    })
                    .chain([HID_TAB, HID_LEFT_ARROW, ZERO, UP_ARROW, C].map(Event::Key))
                    .chain([Event::Click]);
                for event in events {
                    let mut next = keys.clone();
                    let length = match event {
                        Event::Modifier(usage, pressed) => next.key(usage, pressed, translate),
                        Event::Key(usage) => {
                            let plan = next.key(usage, true, translate);
                            next.key(usage, false, translate);
                            plan
                        }
                        Event::Click => next.click(translate),
                    }
                    .steps()
                    .len();
                    longest = longest.max(length);
                    pending.push(next);
                }
            }
            assert_eq!(longest, worst, "{translate:?} over {} states", seen.len());
        }
    }

    /// A small deterministic generator, so the sweep needs no dependency.
    struct Xorshift(u64);

    impl Xorshift {
        fn below(&mut self, bound: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % bound as u64) as usize
        }
    }

    const SWEPT: [HidUsage; 15] = [
        L_CTRL,
        L_SHIFT,
        L_ALT,
        L_WIN,
        R_CTRL,
        R_ALT,
        R_WIN,
        C,
        W,
        ZERO,
        UP_ARROW,
        HID_TAB,
        HID_LEFT_ARROW,
        HID_BACKSPACE,
        HID_DELETE_FORWARD,
    ];

    /// Every step a Windows keyboard on a Mac produced over a fixed sweep, folded into a hash taken
    /// before the Mac keyboard direction existed: that direction changes nothing here.
    #[test]
    fn a_windows_keyboard_on_a_mac_types_exactly_what_it_did_before() {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for seed in 1..=300u64 {
            let mut random = Xorshift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut keys = Translator::default();
            let mut translate = WINDOWS_KEYBOARD;
            for _ in 0..300 {
                let plan = match random.below(12) {
                    0 => {
                        translate = translate.xor(WINDOWS_KEYBOARD);
                        continue;
                    }
                    1 => keys.click(translate),
                    2 if random.below(8) == 0 => {
                        keys.clear();
                        continue;
                    }
                    _ => {
                        let usage = SWEPT[random.below(SWEPT.len())];
                        let pressed = random.below(2) == 0;
                        keys.key(usage, pressed, translate)
                    }
                };
                for step in plan.steps() {
                    let bytes = step.usage.0.to_le_bytes();
                    for byte in bytes.into_iter().chain([u8::from(step.pressed), 0xFF]) {
                        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01B3);
                    }
                }
            }
        }
        assert_eq!(hash, 0x82ba_5562_930e_fa1a);
    }

    /// Random presses, repeats, releases, clicks, switch flips and full releases, from either
    /// platform's keyboard. The destination must hold exactly what the held keys map to after every
    /// step, and nothing once every key is up.
    #[test]
    fn any_sequence_leaves_nothing_stuck_and_off_is_untouched() {
        for direction in [WINDOWS_KEYBOARD, MAC_KEYBOARD] {
            for seed in 1..=400u64 {
                sweep(direction, seed);
            }
        }
    }

    fn sweep(direction: Option<Direction>, seed: u64) {
        let mut random = Xorshift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut keys = Translator::default();
        let mut physical = BTreeSet::new();
        // Keys still down when the destination released everything; their late release lifts
        // nothing.
        let mut stale = BTreeSet::new();
        let mut destination = BTreeSet::new();
        let always_off = seed.is_multiple_of(4);
        let mut translate = if always_off { None } else { direction };
        for _ in 0..200 {
            let (plan, released) = match random.below(10) {
                0 if !always_off => {
                    translate = translate.xor(direction);
                    (KeyPlan::EMPTY, None)
                }
                1 => (keys.click(translate), None),
                2 if random.below(8) == 0 => {
                    keys.clear();
                    destination.clear();
                    stale.clone_from(&physical);
                    (KeyPlan::EMPTY, None)
                }
                _ => {
                    let usage = SWEPT[random.below(SWEPT.len())];
                    let pressed = !physical.contains(&usage) || random.below(3) == 0;
                    let was_stale = if pressed {
                        physical.insert(usage);
                        stale.remove(&usage)
                    } else {
                        physical.remove(&usage);
                        stale.remove(&usage)
                    };
                    let plan = keys.key(usage, pressed, translate);
                    if always_off {
                        assert_eq!(plan.steps(), [KeyStep { usage, pressed }]);
                    }
                    (plan, (!pressed && was_stale).then_some(usage))
                }
            };
            for step in plan.steps() {
                if step.pressed {
                    destination.insert(step.usage);
                } else {
                    assert!(
                        destination.remove(&step.usage) || released.is_some(),
                        "{direction:?} seed {seed}: released an unheld key"
                    );
                }
            }
            let expected: BTreeSet<_> =
                keys.held.iter().flatten().map(|held| held.target).collect();
            assert_eq!(destination, expected, "{direction:?} seed {seed}");
        }
        for usage in std::mem::take(&mut physical) {
            for step in keys.key(usage, false, translate).steps() {
                assert!(!step.pressed);
                destination.remove(&step.usage);
            }
        }
        assert!(
            destination.is_empty(),
            "{direction:?} seed {seed}: {destination:?} stuck"
        );
    }
}
