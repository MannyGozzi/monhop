//! Authoritative held input state and destination-aware modifier semantics.

use std::{collections::BTreeSet, time::Duration};

use crate::{HidUsage, ModifierState, MouseButton, Platform};

pub const HID_ESCAPE: HidUsage = HidUsage(0x29);
pub const HID_LEFT_CONTROL: HidUsage = HidUsage(0xE0);
pub const HID_RIGHT_CONTROL: HidUsage = HidUsage(0xE4);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModifierSide {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticModifierKind {
    Control,
    Shift,
    Alternate,
    System,
}

/// A physical modifier's side and platform-neutral intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemanticModifier {
    pub side: ModifierSide,
    pub kind: SemanticModifierKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestinationModifierKey {
    Control,
    Shift,
    Alt,
    Option,
    Super,
    Command,
}

/// The destination platform's native modifier identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DestinationModifier {
    pub side: ModifierSide,
    pub key: DestinationModifierKey,
}

pub fn semantic_modifier(usage: HidUsage) -> Option<SemanticModifier> {
    let (side, kind) = match usage.0 {
        0xE0 => (ModifierSide::Left, SemanticModifierKind::Control),
        0xE1 => (ModifierSide::Left, SemanticModifierKind::Shift),
        0xE2 => (ModifierSide::Left, SemanticModifierKind::Alternate),
        0xE3 => (ModifierSide::Left, SemanticModifierKind::System),
        0xE4 => (ModifierSide::Right, SemanticModifierKind::Control),
        0xE5 => (ModifierSide::Right, SemanticModifierKind::Shift),
        0xE6 => (ModifierSide::Right, SemanticModifierKind::Alternate),
        0xE7 => (ModifierSide::Right, SemanticModifierKind::System),
        _ => return None,
    };

    Some(SemanticModifier { side, kind })
}

pub fn map_modifier_for_destination(
    usage: HidUsage,
    destination: Platform,
) -> Option<DestinationModifier> {
    let modifier = semantic_modifier(usage)?;
    let key = match (modifier.kind, destination) {
        (SemanticModifierKind::Control, _) => DestinationModifierKey::Control,
        (SemanticModifierKind::Shift, _) => DestinationModifierKey::Shift,
        (SemanticModifierKind::Alternate, Platform::Windows) => DestinationModifierKey::Alt,
        (SemanticModifierKind::Alternate, Platform::MacOs) => DestinationModifierKey::Option,
        (SemanticModifierKind::System, Platform::Windows) => DestinationModifierKey::Super,
        (SemanticModifierKind::System, Platform::MacOs) => DestinationModifierKey::Command,
    };

    Some(DestinationModifier {
        side: modifier.side,
        key,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputAction {
    Key {
        usage: HidUsage,
        pressed: bool,
        repeat: bool,
        modifier: Option<DestinationModifier>,
    },
    Button {
        button: MouseButton,
        pressed: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputError {
    InvalidHidUsage,
}

#[derive(Clone, Debug, Default)]
pub struct HeldInput {
    physical_keys: BTreeSet<HidUsage>,
    physical_buttons: BTreeSet<MouseButton>,
    delivered_keys: BTreeSet<HidUsage>,
    delivered_buttons: BTreeSet<MouseButton>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransitionInputPlan {
    pub release_outgoing: Vec<InputAction>,
    pub transfer_incoming: Vec<InputAction>,
}

impl HeldInput {
    pub fn key_down(
        &mut self,
        usage: HidUsage,
        destination: Platform,
    ) -> Result<Option<InputAction>, InputError> {
        validate_usage(usage)?;

        let already_physical = !self.physical_keys.insert(usage);
        if !self.delivered_keys.insert(usage) {
            return Ok(Some(key_action(usage, true, true, destination)));
        }
        if already_physical {
            // A key held through a handoff needs a new physical press before it is sent again.
            self.delivered_keys.remove(&usage);
            return Ok(None);
        }

        Ok(Some(key_action(usage, true, false, destination)))
    }

    pub fn key_up(
        &mut self,
        usage: HidUsage,
        destination: Platform,
    ) -> Result<Option<InputAction>, InputError> {
        validate_usage(usage)?;

        if !self.physical_keys.remove(&usage) || !self.delivered_keys.remove(&usage) {
            return Ok(None);
        }

        Ok(Some(key_action(usage, false, false, destination)))
    }

    pub fn button_down(&mut self, button: MouseButton) -> Option<InputAction> {
        if !self.physical_buttons.insert(button) || !self.delivered_buttons.insert(button) {
            return None;
        }

        Some(InputAction::Button {
            button,
            pressed: true,
        })
    }

    pub fn button_up(&mut self, button: MouseButton) -> Option<InputAction> {
        if !self.physical_buttons.remove(&button) || !self.delivered_buttons.remove(&button) {
            return None;
        }

        Some(InputAction::Button {
            button,
            pressed: false,
        })
    }

    pub fn transition(
        &mut self,
        outgoing_destination: Platform,
        incoming_destination: Platform,
    ) -> TransitionInputPlan {
        let release_outgoing = self.release_destination(outgoing_destination);
        let mut transfer_incoming = Vec::new();

        for usage in self
            .physical_keys
            .iter()
            .copied()
            .filter(|usage| usage.is_modifier())
        {
            self.delivered_keys.insert(usage);
            transfer_incoming.push(key_action(usage, true, false, incoming_destination));
        }
        for button in self.physical_buttons.iter().copied() {
            self.delivered_buttons.insert(button);
            transfer_incoming.push(InputAction::Button {
                button,
                pressed: true,
            });
        }

        TransitionInputPlan {
            release_outgoing,
            transfer_incoming,
        }
    }

    pub fn release_destination(&mut self, destination: Platform) -> Vec<InputAction> {
        let mut actions =
            Vec::with_capacity(self.delivered_keys.len() + self.delivered_buttons.len());
        for usage in &self.delivered_keys {
            actions.push(key_action(*usage, false, false, destination));
        }
        for button in &self.delivered_buttons {
            actions.push(InputAction::Button {
                button: *button,
                pressed: false,
            });
        }
        self.delivered_keys.clear();
        self.delivered_buttons.clear();
        actions
    }

    pub fn is_key_pressed(&self, usage: HidUsage) -> bool {
        self.physical_keys.contains(&usage)
    }

    pub fn is_button_pressed(&self, button: MouseButton) -> bool {
        self.physical_buttons.contains(&button)
    }

    pub fn is_delivered_key(&self, usage: HidUsage) -> bool {
        self.delivered_keys.contains(&usage)
    }

    pub fn is_delivered_button(&self, button: MouseButton) -> bool {
        self.delivered_buttons.contains(&button)
    }

    pub fn modifier_state(&self) -> ModifierState {
        let mut state = 0;
        for usage in &self.physical_keys {
            state |= modifier_mask(*usage);
        }
        ModifierState(state)
    }
}

#[derive(Clone, Debug, Default)]
pub struct EmergencyEscape {
    held_since: Option<Duration>,
    triggered: bool,
}

impl EmergencyEscape {
    pub const HOLD_DURATION: Duration = Duration::from_secs(2);

    /// Returns `true` once after both Control keys and Escape have been held for two seconds.
    pub fn evaluate(&mut self, input: &HeldInput, monotonic_now: Duration) -> bool {
        let combination_held = input.is_key_pressed(HID_LEFT_CONTROL)
            && input.is_key_pressed(HID_RIGHT_CONTROL)
            && input.is_key_pressed(HID_ESCAPE);
        if !combination_held {
            self.held_since = None;
            self.triggered = false;
            return false;
        }

        let Some(held_since) = self.held_since else {
            self.held_since = Some(monotonic_now);
            return false;
        };
        let Some(held_for) = monotonic_now.checked_sub(held_since) else {
            self.held_since = Some(monotonic_now);
            self.triggered = false;
            return false;
        };
        if !self.triggered && held_for >= Self::HOLD_DURATION {
            self.triggered = true;
            return true;
        }

        false
    }
}

fn validate_usage(usage: HidUsage) -> Result<(), InputError> {
    usage
        .is_valid()
        .then_some(())
        .ok_or(InputError::InvalidHidUsage)
}

fn key_action(usage: HidUsage, pressed: bool, repeat: bool, destination: Platform) -> InputAction {
    InputAction::Key {
        usage,
        pressed,
        repeat,
        modifier: map_modifier_for_destination(usage, destination),
    }
}

fn modifier_mask(usage: HidUsage) -> u8 {
    match usage.0 {
        0xE0 => ModifierState::LEFT_CONTROL,
        0xE1 => ModifierState::LEFT_SHIFT,
        0xE2 => ModifierState::LEFT_ALT,
        0xE3 => ModifierState::LEFT_META,
        0xE4 => ModifierState::RIGHT_CONTROL,
        0xE5 => ModifierState::RIGHT_SHIFT,
        0xE6 => ModifierState::RIGHT_ALT,
        0xE7 => ModifierState::RIGHT_META,
        _ => 0,
    }
}
