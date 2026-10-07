//! A Windows keyboard on a Mac with Ctrl as Command: Ctrl acts as Command and the Windows key as
//! Control, except where a Windows shortcut needs another Mac modifier. A key the Mac sees stays down
//! while any physical key maps to it, and a release always answers the press it follows.

use crate::{
    HID_BACKSPACE, HID_DELETE_FORWARD, HID_LEFT_ARROW, HID_RIGHT_ARROW, HID_TAB, HidUsage,
    SemanticModifier, SemanticModifierKind,
    chord::{KeyPlan, MAX_CHORD_STEPS},
    dimming::DIM_TOGGLE,
    semantic_modifier,
};

/// Both Ctrl and both Alt keys follow the next key; each retarget is one release and one press, and
/// the key itself needs one more step.
const FOLLOWING_MODIFIERS: usize = 4;
const _: () = assert!(FOLLOWING_MODIFIERS * 2 < MAX_CHORD_STEPS);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Held {
    /// What the Mac holds for this physical key.
    target: HidUsage,
    /// A Ctrl or Alt pressed while translating, whose target follows the key it combines with.
    follows: bool,
    /// An Alt that opened the app switcher keeps Command until it is released.
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
pub struct ControlAsCommand {
    held: [Option<Held>; 256],
}

impl Default for ControlAsCommand {
    fn default() -> Self {
        Self { held: [None; 256] }
    }
}

impl ControlAsCommand {
    /// The steps that deliver one physical key. `translate` (the switch is on and a Windows keyboard
    /// drives this Mac) decides presses only: a release always follows the press it answers.
    pub fn key(&mut self, usage: HidUsage, pressed: bool, translate: bool) -> KeyPlan {
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
        if translate && modifier.is_none() {
            self.follow(Some(usage), &mut plan);
        }
        if let Some(held) = self.held[index] {
            plan.push(held.target, true);
            return plan;
        }
        let held = match modifier {
            Some(modifier) if translate => first(modifier),
            _ => Held::fixed(usage),
        };
        let shared = self.holds(held.target);
        self.held[index] = Some(held);
        if !shared {
            plan.push(held.target, true);
        }
        plan
    }

    /// The steps before a mouse press: a held Ctrl is Command again, so Ctrl+click is Command-click.
    pub fn click(&mut self, translate: bool) -> KeyPlan {
        let mut plan = KeyPlan::EMPTY;
        if translate {
            self.follow(None, &mut plan);
        }
        plan
    }

    /// The Mac released every key.
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

    /// Moves each following Ctrl and Alt to the target `key` wants (None: a click), releasing the
    /// old target unless another key holds it and pressing the new one unless another already does.
    fn follow(&mut self, key: Option<HidUsage>, plan: &mut KeyPlan) {
        let kinds = self.kinds();
        for usage in 0xE0..=0xE7 {
            let index = usize::from(usage);
            let Some(held) = self.held[index].filter(|held| held.follows) else {
                continue;
            };
            let Some(modifier) = semantic_modifier(HidUsage(usage)) else {
                continue;
            };
            let next = wanted(modifier, held, key, kinds);
            let already = next.target != held.target && self.holds(next.target);
            self.held[index] = Some(next);
            if next.target == held.target {
                continue;
            }
            if !self.holds(held.target) {
                plan.push(held.target, false);
            }
            if !already {
                plan.push(next.target, true);
            }
        }
    }
}

/// Ctrl starts as Command and Alt as Option, both following the next key; the Windows key is
/// Control and Shift stays Shift.
fn first(modifier: SemanticModifier) -> Held {
    let kind = match modifier.kind {
        SemanticModifierKind::Control => SemanticModifierKind::System,
        SemanticModifierKind::System => SemanticModifierKind::Control,
        other => other,
    };
    let follows = matches!(
        modifier.kind,
        SemanticModifierKind::Control | SemanticModifierKind::Alternate
    );
    Held {
        target: SemanticModifier { kind, ..modifier }.usage(),
        follows,
        switching: false,
    }
}

/// Where a Windows shortcut means another Mac modifier: Ctrl+Tab switches tabs with Control,
/// Ctrl+arrow and Ctrl+Backspace or Delete move and delete by word with Option, and the dimming
/// chord keeps its own keys. Alt+Tab switches apps with Command until Alt is released.
fn wanted(modifier: SemanticModifier, held: Held, key: Option<HidUsage>, kinds: Kinds) -> Held {
    let (kind, switching) = match modifier.kind {
        SemanticModifierKind::Control => {
            let kind = match key {
                Some(key) if dims(key, kinds) => SemanticModifierKind::Control,
                Some(HID_TAB) => SemanticModifierKind::Control,
                Some(HID_LEFT_ARROW | HID_RIGHT_ARROW | HID_BACKSPACE | HID_DELETE_FORWARD)
                    if !kinds.alt && !kinds.system =>
                {
                    SemanticModifierKind::Alternate
                }
                _ => SemanticModifierKind::System,
            };
            (kind, false)
        }
        SemanticModifierKind::Alternate if held.switching || key == Some(HID_TAB) => {
            (SemanticModifierKind::System, true)
        }
        kind => (kind, false),
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
    use std::collections::BTreeSet;

    use super::*;
    use crate::chord::KeyStep;

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

    /// Feeds physical keys with the switch on and collects what the Mac receives.
    fn on(keys: &mut ControlAsCommand, steps: &[KeyStep]) -> Vec<KeyStep> {
        steps
            .iter()
            .flat_map(|step| keys.key(step.usage, step.pressed, true).steps().to_vec())
            .collect()
    }

    #[test]
    fn every_modifier_usage_round_trips_through_its_meaning() {
        for usage in 0xE0..=0xE7 {
            let modifier = semantic_modifier(HidUsage(usage)).unwrap();
            assert_eq!(modifier.usage(), HidUsage(usage));
        }
    }

    #[test]
    fn ctrl_is_command_and_the_windows_key_is_control_on_each_side() {
        let mut keys = ControlAsCommand::default();
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
        let mut keys = ControlAsCommand::default();
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
        let mut keys = ControlAsCommand::default();
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
        assert!(keys.click(true).is_empty(), "a click picks in the switcher");
        assert_eq!(on(&mut keys, &[up(L_ALT)]), [up(L_COMMAND)]);
        assert_eq!(
            on(&mut keys, &[down(L_ALT), down(C)]),
            [down(L_OPTION), down(C)],
            "a fresh Alt is Option again"
        );
    }

    #[test]
    fn ctrl_moves_and_deletes_by_word_with_option() {
        for key in [
            HID_LEFT_ARROW,
            HID_RIGHT_ARROW,
            HID_BACKSPACE,
            HID_DELETE_FORWARD,
        ] {
            let mut keys = ControlAsCommand::default();
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
        let mut keys = ControlAsCommand::default();
        assert_eq!(
            on(&mut keys, &[down(R_CTRL), down(UP_ARROW)]),
            [down(R_COMMAND), down(UP_ARROW)],
            "up and down keep Command: top and bottom"
        );
    }

    #[test]
    fn word_moves_need_ctrl_alone() {
        for other in [L_ALT, L_WIN] {
            let mut keys = ControlAsCommand::default();
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
            let mut keys = ControlAsCommand::default();
            let plan = on(&mut keys, &[down(first), down(second), down(ZERO)]);
            let control = if first == R_CTRL || second == R_CTRL {
                R_CONTROL
            } else {
                L_CONTROL
            };
            assert_eq!(plan.last(), Some(&down(ZERO)));
            let mut mac = BTreeSet::new();
            for step in &plan {
                if step.pressed {
                    mac.insert(step.usage);
                } else {
                    mac.remove(&step.usage);
                }
            }
            let option = if control == R_CONTROL {
                R_OPTION
            } else {
                L_OPTION
            };
            assert_eq!(mac, BTreeSet::from([control, option, ZERO]));
        }
        let mut keys = ControlAsCommand::default();
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
        let mut keys = ControlAsCommand::default();
        on(
            &mut keys,
            &[down(L_CTRL), down(HID_LEFT_ARROW), up(HID_LEFT_ARROW)],
        );
        assert_eq!(keys.click(true).steps(), [up(L_OPTION), down(L_COMMAND)]);
        assert!(keys.click(true).is_empty());
        assert!(keys.click(false).is_empty());
        assert_eq!(on(&mut keys, &[up(L_CTRL)]), [up(L_COMMAND)]);
    }

    #[test]
    fn a_target_two_keys_share_is_pressed_once_and_released_by_the_last() {
        let mut keys = ControlAsCommand::default();
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
        let mut keys = ControlAsCommand::default();
        assert_eq!(keys.key(L_CTRL, true, true).steps(), [down(L_COMMAND)]);
        assert_eq!(
            keys.key(L_CTRL, false, false).steps(),
            [up(L_COMMAND)],
            "turned off while held"
        );
        assert_eq!(keys.key(L_CTRL, true, false).steps(), [down(L_CONTROL)]);
        assert_eq!(
            keys.key(HID_TAB, true, true).steps(),
            [down(HID_TAB)],
            "a Ctrl pressed while off never follows"
        );
        assert_eq!(keys.key(L_CTRL, false, true).steps(), [up(L_CONTROL)]);
    }

    #[test]
    fn a_repeat_repeats_what_the_mac_holds() {
        let mut keys = ControlAsCommand::default();
        assert_eq!(
            on(&mut keys, &[down(L_CTRL), down(L_CTRL), down(C), down(C)]),
            [down(L_COMMAND), down(L_COMMAND), down(C), down(C)]
        );
        on(&mut keys, &[down(HID_TAB)]);
        assert_eq!(on(&mut keys, &[down(L_CTRL)]), [down(L_CONTROL)]);
    }

    #[test]
    fn after_the_mac_releases_everything_a_late_release_lifts_nothing_held() {
        let mut keys = ControlAsCommand::default();
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
        let mut keys = ControlAsCommand::default();
        on(
            &mut keys,
            &[down(L_CTRL), down(R_CTRL), down(L_ALT), down(R_ALT)],
        );
        let plan = keys.key(HID_TAB, true, true);
        assert_eq!(plan.steps().len(), 9);
        assert_eq!(plan.steps().last(), Some(&down(HID_TAB)));
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

    /// Random presses, repeats, releases, clicks, switch flips and full releases. The Mac must hold
    /// exactly what the held keys map to after every step, and nothing once every key is up.
    #[test]
    fn any_sequence_leaves_nothing_stuck_and_off_is_untouched() {
        for seed in 1..=400u64 {
            let mut random = Xorshift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut keys = ControlAsCommand::default();
            let mut physical = BTreeSet::new();
            // Keys still down when the Mac released everything; their late release lifts nothing.
            let mut stale = BTreeSet::new();
            let mut mac = BTreeSet::new();
            let always_off = seed % 4 == 0;
            let mut translate = !always_off;
            for _ in 0..200 {
                let (plan, released) = match random.below(10) {
                    0 if !always_off => {
                        translate = !translate;
                        (KeyPlan::EMPTY, None)
                    }
                    1 => (keys.click(translate), None),
                    2 if random.below(8) == 0 => {
                        keys.clear();
                        mac.clear();
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
                        mac.insert(step.usage);
                    } else {
                        assert!(
                            mac.remove(&step.usage) || released.is_some(),
                            "seed {seed}: released an unheld key"
                        );
                    }
                }
                let expected: BTreeSet<_> =
                    keys.held.iter().flatten().map(|held| held.target).collect();
                assert_eq!(mac, expected, "seed {seed}");
            }
            for usage in std::mem::take(&mut physical) {
                for step in keys.key(usage, false, translate).steps() {
                    assert!(!step.pressed);
                    mac.remove(&step.usage);
                }
            }
            assert!(mac.is_empty(), "seed {seed}: {mac:?} stuck");
        }
    }
}
