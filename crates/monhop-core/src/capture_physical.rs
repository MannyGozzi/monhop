//! Callback-local held state. No allocation, wakeups, or native effects occur here.

use std::time::Duration;

use crate::{EmergencyEscape, HidUsage, ModifierState, MouseButton};

use crate::capture::{CaptureEvent, CaptureProducer, CaptureStop, CapturedEvent, StopReason};

#[derive(Default, Clone, Copy)]
struct Press {
    physical: bool,
    local_down: bool,
    suppressed_down: bool,
    initially_held: bool,
    queued_down: bool,
    locally_injected: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LocalTransfer {
    Key { usage: HidUsage, pressed: bool },
    Button { button: MouseButton, pressed: bool },
}
impl std::fmt::Debug for LocalTransfer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalTransfer([redacted])")
    }
}

pub struct TransferPlan {
    entries: [Option<LocalTransfer>; 261],
    len: usize,
}
impl TransferPlan {
    fn new() -> Self {
        Self {
            entries: [None; 261],
            len: 0,
        }
    }
    fn push(&mut self, transfer: LocalTransfer) {
        self.entries[self.len] = Some(transfer);
        self.len += 1;
    }
    pub fn iter(&self) -> impl ExactSizeIterator<Item = LocalTransfer> + '_ {
        self.entries[..self.len]
            .iter()
            .map(|entry| entry.expect("initialized transfer plan"))
    }
}

pub struct PhysicalCapture {
    keys: [Press; 256],
    buttons: [Press; 5],
    last_time: Duration,
    chord_since: Option<Duration>,
    reserved_escape: bool,
    intercepts_escape: bool,
    routing_revision: u64,
}

impl PhysicalCapture {
    pub fn new(now: Duration) -> Self {
        Self {
            keys: [Press::default(); 256],
            buttons: [Press::default(); 5],
            last_time: now,
            chord_since: None,
            reserved_escape: false,
            intercepts_escape: true,
            routing_revision: 0,
        }
    }

    pub fn new_passive(now: Duration) -> Self {
        Self {
            intercepts_escape: false,
            ..Self::new(now)
        }
    }

    pub fn set_routing_revision(&mut self, revision: u64) {
        self.routing_revision = revision;
    }

    /// The native owner executes this plan and commits each successful operation before its barrier.
    pub fn handoff_plan(&self, remote: bool) -> TransferPlan {
        let mut plan = TransferPlan::new();
        for (index, state) in self.keys.iter().enumerate() {
            let usage = HidUsage(index as u16);
            if !state.physical || state.initially_held || !usage.is_valid() {
                continue;
            }
            if remote && state.local_down || !remote && usage.is_modifier() && !state.local_down {
                plan.push(LocalTransfer::Key {
                    usage,
                    pressed: !remote,
                });
            }
        }
        self.push_buttons(&mut plan, !remote, |state| {
            state.physical && !state.initially_held && state.local_down == remote
        });
        plan
    }

    pub fn release_injected_plan(&self) -> TransferPlan {
        let mut plan = TransferPlan::new();
        for (index, state) in self.keys.iter().enumerate() {
            if state.locally_injected && state.local_down {
                plan.push(LocalTransfer::Key {
                    usage: HidUsage(index as u16),
                    pressed: false,
                });
            }
        }
        self.push_buttons(&mut plan, false, |state| {
            state.locally_injected && state.local_down
        });
        plan
    }

    /// Appends every button slot the predicate selects, in canonical slot order.
    fn push_buttons(&self, plan: &mut TransferPlan, pressed: bool, select: impl Fn(Press) -> bool) {
        for button in MouseButton::ALL {
            if select(self.buttons[button.index()]) {
                plan.push(LocalTransfer::Button { button, pressed });
            }
        }
    }

    /// Called only for native operations that succeeded, on the physical ledger's owning thread.
    pub fn apply_transfer_success(&mut self, transfer: LocalTransfer) {
        let (state, pressed) = match transfer {
            LocalTransfer::Key { usage, pressed } if usage.is_valid() => {
                (&mut self.keys[usize::from(usage.0)], pressed)
            }
            LocalTransfer::Button { button, pressed } => {
                (&mut self.buttons[button.index()], pressed)
            }
            _ => return,
        };
        state.local_down = pressed;
        state.locally_injected = pressed;
        state.suppressed_down = state.physical && !pressed;
    }

    /// Startup snapshots cannot establish physical provenance. Wait for their releases.
    pub fn seed_locally_held_key(&mut self, usage: HidUsage) {
        if usage.is_valid() {
            self.keys[usize::from(usage.0)] = initial_press();
        }
    }

    pub fn seed_locally_held_button(&mut self, button: MouseButton) {
        self.buttons[button.index()] = initial_press();
    }

    fn any_press(&self, predicate: impl Fn(&Press) -> bool) -> bool {
        self.keys.iter().chain(self.buttons.iter()).any(predicate)
    }

    pub fn is_ready_for_suppression(&self) -> bool {
        !self.any_press(|press| press.initially_held)
    }

    pub fn has_suppressed_presses(&self) -> bool {
        self.any_press(|press| press.suppressed_down)
    }

    pub fn tick(&mut self, now: Duration, stop: &CaptureStop) {
        if now < self.last_time {
            stop.stop(StopReason::ClockRegression);
            return;
        }
        self.last_time = now;
        if let Some(start) = self.chord_since
            && now.saturating_sub(start) >= EmergencyEscape::HOLD_DURATION
        {
            stop.stop(StopReason::EmergencyEscape);
        }
    }

    /// Returns whether this event must be withheld from local applications.
    /// A stopped session drains only already-suppressed presses until their physical release.
    pub fn process(
        &mut self,
        event: CaptureEvent,
        remote: bool,
        now: Duration,
        producer: &mut CaptureProducer,
        stop: &CaptureStop,
    ) -> bool {
        self.tick(now, stop);
        let remote = remote && self.is_ready_for_suppression() && !stop.is_stopped();
        match event {
            CaptureEvent::Key { usage, pressed, .. } => {
                if !usage.is_valid() {
                    stop.stop(StopReason::InvalidInput);
                    return false;
                }
                let index = usize::from(usage.0);
                let previous = self.keys[index];
                self.keys[index].physical = pressed;
                let complete_chord = [0xe0, 0xe4, 0x29].iter().all(|i| self.keys[*i].physical);
                if complete_chord {
                    if self.chord_since.is_none() {
                        self.chord_since = Some(now);
                    }
                    self.reserved_escape = true;
                } else {
                    self.chord_since = None;
                }
                let reserved = self.intercepts_escape && usage.0 == 0x29 && self.reserved_escape;
                let chord_repeat = self.intercepts_escape && complete_chord && pressed;
                let initial = previous.initially_held;
                let should_queue = !initial
                    && if pressed {
                        !reserved && !chord_repeat
                    } else {
                        previous.queued_down
                    };
                let canonical = CaptureEvent::Key {
                    usage,
                    pressed,
                    repeat: pressed && previous.physical,
                    modifiers: self.modifiers(),
                };
                let queued = should_queue
                    && !stop.is_stopped()
                    && producer
                        .try_push_tagged(CapturedEvent {
                            event: canonical,
                            routing_revision: self.routing_revision,
                            remote,
                        })
                        .is_ok();
                // The local Ctrl+Ctrl+Escape chord is reserved even before its hold expires.
                let suppress = update_press(
                    &mut self.keys[index],
                    previous,
                    pressed,
                    (remote && queued) || (reserved && !stop.is_stopped()),
                );
                if pressed && queued {
                    self.keys[index].queued_down = true;
                }
                if usage.0 == 0x29 && !pressed {
                    self.reserved_escape = false;
                }
                suppress
            }
            CaptureEvent::Button { button, pressed } => {
                let index = button.index();
                let previous = self.buttons[index];
                let queued = !previous.initially_held
                    && (pressed || previous.queued_down)
                    && !stop.is_stopped()
                    && producer
                        .try_push_tagged(CapturedEvent {
                            event,
                            routing_revision: self.routing_revision,
                            remote,
                        })
                        .is_ok();
                let suppress = update_press(
                    &mut self.buttons[index],
                    previous,
                    pressed,
                    remote && queued,
                );
                if pressed && queued {
                    self.buttons[index].queued_down = true;
                }
                suppress
            }
            _ => {
                let queued = !stop.is_stopped()
                    && producer
                        .try_push_tagged(CapturedEvent {
                            event,
                            routing_revision: self.routing_revision,
                            remote,
                        })
                        .is_ok();
                remote && queued && !stop.is_stopped()
            }
        }
    }

    fn modifiers(&self) -> ModifierState {
        let mut value = 0;
        for index in 0..8 {
            if self.keys[0xe0 + index].physical {
                value |= 1 << index;
            }
        }
        ModifierState(value)
    }
}

fn initial_press() -> Press {
    Press {
        physical: true,
        local_down: true,
        initially_held: true,
        ..Press::default()
    }
}

fn update_press(state: &mut Press, previous: Press, pressed: bool, suppress: bool) -> bool {
    if !pressed {
        *state = Press::default();
        return previous.suppressed_down && !previous.local_down;
    }
    state.physical = true;
    if previous.suppressed_down {
        return true;
    }
    if suppress && !previous.initially_held {
        if !previous.local_down {
            state.suppressed_down = true;
        }
        return true;
    }
    state.local_down = true;
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::capture_channel;

    fn key(usage: u16, pressed: bool) -> CaptureEvent {
        CaptureEvent::Key {
            usage: HidUsage(usage),
            pressed,
            repeat: false,
            modifiers: ModifierState(0),
        }
    }

    fn route_remote(input: &mut PhysicalCapture, tx: &mut CaptureProducer) {
        tx.try_push_tagged(CapturedEvent {
            event: CaptureEvent::RouteChanged {
                remote: true,
                revision: 1,
            },
            routing_revision: 1,
            remote: true,
        })
        .unwrap();
        input.set_routing_revision(1);
    }

    #[test]
    fn locally_delivered_down_keeps_its_release_after_remote_transition() {
        let stop = CaptureStop::default();
        let (mut tx, _rx) = capture_channel(stop.clone());
        let mut input = PhysicalCapture::new(Duration::ZERO);
        assert!(!input.process(key(4, true), false, Duration::ZERO, &mut tx, &stop));
        route_remote(&mut input, &mut tx);
        assert!(input.process(key(4, true), true, Duration::ZERO, &mut tx, &stop));
        assert!(!input.process(key(4, false), true, Duration::ZERO, &mut tx, &stop));
    }

    #[test]
    fn suppressed_press_cannot_leak_repeats_after_failure() {
        let stop = CaptureStop::default();
        let (mut tx, _rx) = capture_channel(stop.clone());
        let mut input = PhysicalCapture::new(Duration::ZERO);
        route_remote(&mut input, &mut tx);
        assert!(input.process(key(4, true), true, Duration::ZERO, &mut tx, &stop));
        stop.stop(StopReason::Requested);
        assert!(input.process(key(4, true), false, Duration::ZERO, &mut tx, &stop));
        assert!(!input.process(key(5, true), false, Duration::ZERO, &mut tx, &stop));
        assert!(input.process(key(4, false), false, Duration::ZERO, &mut tx, &stop));
        assert!(!input.has_suppressed_presses());
        assert!(!input.process(key(4, true), false, Duration::ZERO, &mut tx, &stop));
    }

    #[test]
    fn failed_admission_passes_new_down_and_preserves_owed_local_release() {
        let stop = CaptureStop::default();
        let (mut tx, rx) = capture_channel(stop.clone());
        let mut input = PhysicalCapture::new(Duration::ZERO);
        assert!(!input.process(key(4, true), false, Duration::ZERO, &mut tx, &stop));
        drop(rx);
        assert!(!input.process(key(5, true), true, Duration::ZERO, &mut tx, &stop));
        assert!(!input.process(key(4, false), true, Duration::ZERO, &mut tx, &stop));
    }

    #[test]
    fn initial_held_state_requires_release_before_suppression() {
        let stop = CaptureStop::default();
        let (mut tx, mut rx) = capture_channel(stop.clone());
        let mut input = PhysicalCapture::new(Duration::ZERO);
        input.seed_locally_held_key(HidUsage(0xe0));
        assert!(!input.is_ready_for_suppression());
        assert!(!input.process(key(0xe0, true), true, Duration::ZERO, &mut tx, &stop));
        assert!(!input.process(key(0xe0, false), true, Duration::ZERO, &mut tx, &stop));
        assert!(input.is_ready_for_suppression());
        assert!(rx.try_pop().unwrap().is_none());
    }

    #[test]
    fn chord_is_reserved_before_hold_and_timer_triggers_without_more_input() {
        let stop = CaptureStop::default();
        let (mut tx, mut rx) = capture_channel(stop.clone());
        let mut input = PhysicalCapture::new(Duration::ZERO);
        route_remote(&mut input, &mut tx);
        assert!(matches!(
            rx.try_pop().unwrap(),
            Some(CaptureEvent::RouteChanged {
                remote: true,
                revision: 1
            })
        ));
        input.process(key(0xe0, true), true, Duration::ZERO, &mut tx, &stop);
        input.process(key(0xe4, true), true, Duration::ZERO, &mut tx, &stop);
        assert!(input.process(key(0x29, true), true, Duration::ZERO, &mut tx, &stop));
        assert!(rx.try_pop().unwrap().is_some());
        assert!(rx.try_pop().unwrap().is_some());
        assert!(rx.try_pop().unwrap().is_none());
        input.tick(Duration::from_millis(1999), &stop);
        assert!(!stop.is_stopped());
        input.tick(Duration::from_secs(2), &stop);
        assert_eq!(stop.reason(), Some(StopReason::EmergencyEscape));
    }

    #[test]
    fn clock_regression_is_terminal() {
        let stop = CaptureStop::default();
        let mut input = PhysicalCapture::new(Duration::from_secs(1));
        input.tick(Duration::ZERO, &stop);
        assert_eq!(stop.reason(), Some(StopReason::ClockRegression));
    }

    #[test]
    fn passive_capture_does_not_consume_the_recovery_chord() {
        let stop = CaptureStop::default();
        let (mut tx, _rx) = capture_channel(stop.clone());
        let mut input = PhysicalCapture::new_passive(Duration::ZERO);
        for usage in [0xe0, 0xe4, 0x29] {
            assert!(!input.process(key(usage, true), false, Duration::ZERO, &mut tx, &stop));
        }
        input.tick(Duration::from_secs(2), &stop);
        assert!(stop.is_stopped());
        assert!(!input.has_suppressed_presses());
    }

    #[test]
    fn escape_pressed_before_controls_keeps_its_queued_release() {
        let stop = CaptureStop::default();
        let (mut tx, mut rx) = capture_channel(stop.clone());
        let mut input = PhysicalCapture::new(Duration::ZERO);
        route_remote(&mut input, &mut tx);
        assert!(matches!(
            rx.try_pop().unwrap(),
            Some(CaptureEvent::RouteChanged {
                remote: true,
                revision: 1
            })
        ));
        input.process(key(0x29, true), true, Duration::ZERO, &mut tx, &stop);
        input.process(key(0xe0, true), true, Duration::ZERO, &mut tx, &stop);
        input.process(key(0xe4, true), true, Duration::ZERO, &mut tx, &stop);
        input.process(key(0x29, false), true, Duration::ZERO, &mut tx, &stop);
        assert!(matches!(
            rx.try_pop().unwrap(),
            Some(CaptureEvent::Key {
                usage: HidUsage(0x29),
                pressed: true,
                ..
            })
        ));
        assert!(matches!(
            rx.try_pop().unwrap(),
            Some(CaptureEvent::Key {
                usage: HidUsage(0xe0),
                pressed: true,
                ..
            })
        ));
        assert!(matches!(
            rx.try_pop().unwrap(),
            Some(CaptureEvent::Key {
                usage: HidUsage(0x29),
                pressed: false,
                ..
            })
        ));
        assert!(rx.try_pop().unwrap().is_none());
        input.tick(Duration::from_secs(3), &stop);
        assert!(!stop.is_stopped());
    }

    #[test]
    fn held_handoff_releases_all_local_presses_but_restores_only_modifiers_and_buttons() {
        let stop = CaptureStop::default();
        let (mut tx, _rx) = capture_channel(stop.clone());
        let mut input = PhysicalCapture::new(Duration::ZERO);
        for usage in [4, 0xe1] {
            input.process(key(usage, true), false, Duration::ZERO, &mut tx, &stop);
        }
        input.process(
            CaptureEvent::Button {
                button: MouseButton::Left,
                pressed: true,
            },
            false,
            Duration::ZERO,
            &mut tx,
            &stop,
        );
        let release = input.handoff_plan(true);
        assert_eq!(release.iter().len(), 3);
        for item in release.iter() {
            input.apply_transfer_success(item);
        }
        assert!(input.has_suppressed_presses());
        let restore = input.handoff_plan(false);
        assert_eq!(restore.iter().len(), 2);
        assert!(!restore.iter().any(|item| matches!(
            item,
            LocalTransfer::Key {
                usage: HidUsage(4),
                ..
            }
        )));
    }

    #[test]
    fn physical_release_ends_synthetic_ownership_before_a_fresh_local_press() {
        let stop = CaptureStop::default();
        let (mut tx, _rx) = capture_channel(stop.clone());
        let mut input = PhysicalCapture::new(Duration::ZERO);
        input.process(key(0xe1, true), false, Duration::ZERO, &mut tx, &stop);
        let release = input.handoff_plan(true);
        for item in release.iter() {
            input.apply_transfer_success(item);
        }
        let restore = input.handoff_plan(false);
        for item in restore.iter() {
            input.apply_transfer_success(item);
        }
        assert_eq!(input.release_injected_plan().iter().len(), 1);
        assert!(!input.process(key(0xe1, false), false, Duration::ZERO, &mut tx, &stop));
        assert!(!input.process(key(0xe1, true), false, Duration::ZERO, &mut tx, &stop));
        assert_eq!(input.release_injected_plan().iter().len(), 0);
    }
}
