//! Callback-local held state. No allocation or native effects occur here; the only wakeup is a
//! take-back running its gate's signal-only waker.

use std::time::Duration;

use crate::{EmergencyEscape, HidUsage, ModifierState, MouseButton};

use crate::capture::{
    CaptureEvent, CaptureProducer, CaptureStop, CapturedEvent, MAX_SUPPRESSION_TTL, StopReason,
};
use crate::floor::FloorState;
use crate::take_back::{TAKE_BACK_MOTION, TAKE_BACK_WINDOW, TakeBackGate, TakeBackOutcome};

#[derive(Default, Clone, Copy)]
struct Press {
    physical: bool,
    local_down: bool,
    suppressed_down: bool,
    initially_held: bool,
    queued_down: bool,
    locally_injected: bool,
    /// Pressed while withholding: never delivered or queued; its repeats and release stay withheld.
    withheld: bool,
}

/// Relative motion summed toward a take-back within one Receiving generation.
#[derive(Default, Clone, Copy)]
struct MotionWindow {
    /// 0 marks an empty window; floor generations start at 1.
    generation: u64,
    start: Duration,
    dx: f64,
    dy: f64,
}

impl MotionWindow {
    fn reaches_threshold(&mut self, generation: u64, now: Duration, dx: f64, dy: f64) -> bool {
        if self.generation != generation || now.saturating_sub(self.start) >= TAKE_BACK_WINDOW {
            *self = Self {
                generation,
                start: now,
                dx: 0.0,
                dy: 0.0,
            };
        }
        self.dx += dx;
        self.dy += dy;
        self.dx * self.dx + self.dy * self.dy >= TAKE_BACK_MOTION * TAKE_BACK_MOTION
    }
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
    take_back: Option<TakeBackGate>,
    motion: MotionWindow,
    /// Hook-measured pointer travel, apart from `motion` so one movement never counts twice.
    travel: MotionWindow,
    withholding_since: Option<Duration>,
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
            take_back: None,
            motion: MotionWindow::default(),
            travel: MotionWindow::default(),
            withholding_since: None,
        }
    }

    /// Deliberate physical input while the floor is Receiving takes this computer back.
    pub fn with_take_back(self, gate: TakeBackGate) -> Self {
        Self {
            take_back: Some(gate),
            ..self
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
            if !state.physical || state.initially_held || state.withheld || !usage.is_valid() {
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
            state.physical && !state.initially_held && !state.withheld && state.local_down == remote
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

    /// Whether a held press still owes local apps a withheld release.
    pub fn has_suppressed_presses(&self) -> bool {
        self.any_press(|press| press.suppressed_down || press.withheld)
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
        self.process_with_travel(event, None, remote, now, producer, stop)
    }

    /// [`Self::process`] for a pre-delivery hook's pointer position. `travel` is that position
    /// minus the cursor position just before it and counts toward take-back like relative motion,
    /// so the hook can withhold the movement that takes back. Only `event` is queued.
    pub fn process_with_travel(
        &mut self,
        event: CaptureEvent,
        travel: Option<(f64, f64)>,
        remote: bool,
        now: Duration,
        producer: &mut CaptureProducer,
        stop: &CaptureStop,
    ) -> bool {
        self.tick(now, stop);
        let remote = remote && self.is_ready_for_suppression() && !stop.is_stopped();
        let floor_generation = self.observe_take_back(event, travel, now, stop);
        let withholding = self.withholding(now, stop);
        let tagged = |event| CapturedEvent {
            event,
            routing_revision: self.routing_revision,
            remote,
            floor_generation,
        };
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
                if usage.0 == 0x29 && !pressed {
                    self.reserved_escape = false;
                }
                if let Some(suppress) =
                    withhold_press(&mut self.keys[index], previous, pressed, withholding)
                {
                    return suppress;
                }
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
                    && producer.try_push_tagged(tagged(canonical)).is_ok();
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
                suppress || (withholding && pressed)
            }
            CaptureEvent::Button {
                button, pressed, ..
            } => {
                let index = button.index();
                let previous = self.buttons[index];
                if let Some(suppress) =
                    withhold_press(&mut self.buttons[index], previous, pressed, withholding)
                {
                    return suppress;
                }
                let queued = !previous.initially_held
                    && (pressed || previous.queued_down)
                    && !stop.is_stopped()
                    && producer.try_push_tagged(tagged(event)).is_ok();
                let suppress = update_press(
                    &mut self.buttons[index],
                    previous,
                    pressed,
                    remote && queued,
                );
                if pressed && queued {
                    self.buttons[index].queued_down = true;
                }
                suppress || (withholding && pressed)
            }
            _ => {
                let queued = !stop.is_stopped() && producer.try_push_tagged(tagged(event)).is_ok();
                (remote && queued && !stop.is_stopped()) || withholding
            }
        }
    }

    /// Fires take-back on deliberate input while Receiving; returns the floor generation to stamp.
    fn observe_take_back(
        &mut self,
        event: CaptureEvent,
        travel: Option<(f64, f64)>,
        now: Duration,
        stop: &CaptureStop,
    ) -> u64 {
        let Some(gate) = self.take_back.as_ref() else {
            return 0;
        };
        let floor = gate.floor().snapshot();
        if floor.state != FloorState::Receiving {
            self.motion = MotionWindow::default();
            self.travel = MotionWindow::default();
            return floor.generation;
        }
        // Positions alone never take back: the injected cursor moves them. Only travel measured
        // from the cursor position just before a physical record counts.
        let deliberate = event.is_valid()
            && match event {
                CaptureEvent::Key { .. }
                | CaptureEvent::Button { .. }
                | CaptureEvent::Scroll { .. }
                | CaptureEvent::LogicalScroll { .. } => true,
                CaptureEvent::RelativeMotion { dx, dy } => self.motion.reaches_threshold(
                    floor.generation,
                    now,
                    f64::from(dx),
                    f64::from(dy),
                ),
                CaptureEvent::LogicalRelativeMotion { dx, dy } => {
                    self.motion.reaches_threshold(floor.generation, now, dx, dy)
                }
                CaptureEvent::AbsoluteMotion { .. }
                | CaptureEvent::LogicalAbsoluteMotion { .. } => travel.is_some_and(|(dx, dy)| {
                    dx.is_finite()
                        && dy.is_finite()
                        && self.travel.reaches_threshold(floor.generation, now, dx, dy)
                }),
                CaptureEvent::RouteChanged { .. } => false,
            };
        if deliberate {
            self.motion = MotionWindow::default();
            self.travel = MotionWindow::default();
            if let TakeBackOutcome::Triggered { withhold: true } = gate.trigger()
                && !stop.is_stopped()
            {
                self.withholding_since = Some(now);
            }
        }
        floor.generation
    }

    /// Withholding ends once injected input is released, the TTL passes, or capture stops.
    fn withholding(&mut self, now: Duration, stop: &CaptureStop) -> bool {
        let Some(since) = self.withholding_since else {
            return false;
        };
        let held = self
            .take_back
            .as_ref()
            .is_some_and(TakeBackGate::injected_held);
        if held && !stop.is_stopped() && now.saturating_sub(since) < MAX_SUPPRESSION_TTL {
            return true;
        }
        self.withholding_since = None;
        false
    }

    /// Withheld and initially held modifiers were never queued, so later events must not claim them.
    fn modifiers(&self) -> ModifierState {
        let mut value = 0;
        for index in 0..8 {
            let key = self.keys[0xe0 + index];
            if key.physical && !key.withheld && !key.initially_held {
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

/// Drops a press made while withholding and quarantines its repeats and release. `None` defers to
/// the normal ledger; a release local apps are owed is never withheld.
fn withhold_press(
    state: &mut Press,
    previous: Press,
    pressed: bool,
    withholding: bool,
) -> Option<bool> {
    if previous.withheld {
        if !pressed {
            *state = Press::default();
            return Some(!previous.local_down);
        }
        return Some(true);
    }
    if withholding && pressed && !previous.physical {
        state.physical = true;
        state.withheld = true;
        return Some(true);
    }
    None
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
    use crate::capture::{CaptureConsumer, MAX_SUPPRESSION_TTL, capture_channel};
    use crate::{FloorOwner, FloorSnapshot, FloorState, SharedFloor, TakeBackGate};

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
            floor_generation: 0,
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
                at: Duration::ZERO,
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

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn relative(dx: i32, dy: i32) -> CaptureEvent {
        CaptureEvent::RelativeMotion { dx, dy }
    }

    fn button(pressed: bool) -> CaptureEvent {
        CaptureEvent::Button {
            button: MouseButton::Left,
            pressed,
            at: Duration::ZERO,
        }
    }

    /// A gate whose floor is Receiving with injection admitted.
    fn receiving_gate() -> (TakeBackGate, FloorSnapshot) {
        let gate = TakeBackGate::new(SharedFloor::new());
        let receiving = gate
            .floor()
            .transition(gate.floor().snapshot(), FloorState::Receiving)
            .expect("free floor accepts an activation");
        gate.open_injection(receiving.generation);
        assert!(gate.admits_injection());
        (gate, receiving)
    }

    fn drain(rx: &mut CaptureConsumer) -> Vec<CapturedEvent> {
        std::iter::from_fn(|| rx.try_pop_tagged().unwrap()).collect()
    }

    fn assert_still_receiving(gate: &TakeBackGate, receiving: FloorSnapshot) {
        assert_eq!(gate.floor().snapshot(), receiving);
        assert!(gate.admits_injection());
        assert_eq!(gate.take_triggered(), None);
    }

    fn assert_took_back(gate: &TakeBackGate, receiving: FloorSnapshot) {
        let yielding = gate.floor().snapshot();
        assert_eq!(yielding.state, FloorState::Yielding);
        assert_eq!(yielding.generation, receiving.generation + 1);
        assert!(!gate.admits_injection());
        assert_eq!(gate.take_triggered(), Some(yielding.generation));
        assert_eq!(gate.take_triggered(), None, "a trigger is returned once");
    }

    #[test]
    fn take_back_ignores_sub_threshold_motion() {
        let stop = CaptureStop::default();
        let (mut tx, mut rx) = capture_channel(stop.clone());
        let (gate, receiving) = receiving_gate();
        let mut input = PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone());
        // Fifteen units of travel, but the vector sum never reaches six.
        for (at, dx) in [(0, 5), (20, -5), (40, 5)] {
            assert!(!input.process(relative(dx, 0), false, ms(at), &mut tx, &stop));
        }
        let logical = CaptureEvent::LogicalRelativeMotion { dx: 0.0, dy: 3.0 };
        assert!(!input.process(logical, false, ms(60), &mut tx, &stop));
        assert_still_receiving(&gate, receiving);
        assert_eq!(drain(&mut rx).len(), 4, "local motion is still queued");
    }

    #[test]
    fn take_back_fires_on_threshold_within_window() {
        let stop = CaptureStop::default();
        let (mut tx, _rx) = capture_channel(stop.clone());
        let (gate, receiving) = receiving_gate();
        let mut input = PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone());
        assert!(!input.process(relative(4, 0), false, ms(0), &mut tx, &stop));
        assert_still_receiving(&gate, receiving);
        let logical = CaptureEvent::LogicalRelativeMotion { dx: 1.0, dy: 4.0 };
        assert!(
            !input.process(logical, false, ms(99), &mut tx, &stop),
            "nothing injected is held, so the triggering motion passes through"
        );
        assert_took_back(&gate, receiving);
    }

    #[test]
    fn take_back_window_restarts_after_expiry() {
        let stop = CaptureStop::default();
        let (mut tx, _rx) = capture_channel(stop.clone());
        let (gate, receiving) = receiving_gate();
        let mut input = PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone());
        input.process(relative(4, 0), false, ms(0), &mut tx, &stop);
        // A sample exactly one window after the first starts a new window.
        input.process(relative(4, 0), false, ms(100), &mut tx, &stop);
        assert_still_receiving(&gate, receiving);
        input.process(relative(2, 0), false, ms(199), &mut tx, &stop);
        assert_took_back(&gate, receiving);
    }

    #[test]
    fn take_back_fires_once_per_generation() {
        let stop = CaptureStop::default();
        let (mut tx, _rx) = capture_channel(stop.clone());
        let (gate, receiving) = receiving_gate();
        let mut input = PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone());
        assert!(!input.process(key(4, true), false, ms(0), &mut tx, &stop));
        let yielding = gate.floor().snapshot();
        assert_took_back(&gate, receiving);

        assert!(!input.process(key(4, false), false, ms(1), &mut tx, &stop));
        input.process(
            CaptureEvent::Scroll {
                horizontal: 0,
                vertical: 120,
            },
            false,
            ms(2),
            &mut tx,
            &stop,
        );
        assert_eq!(
            gate.floor().snapshot(),
            yielding,
            "a yielding floor is not taken again"
        );
        assert_eq!(gate.take_triggered(), None);

        assert!(
            gate.floor()
                .release(FloorOwner::Inbound, yielding.generation)
        );
        let again = gate
            .floor()
            .transition(gate.floor().snapshot(), FloorState::Receiving)
            .expect("a later activation");
        gate.open_injection(again.generation);
        assert!(gate.admits_injection());
        assert!(!input.process(button(true), false, ms(3), &mut tx, &stop));
        assert_took_back(&gate, again);
    }

    #[test]
    fn absolute_motion_never_takes_back() {
        let stop = CaptureStop::default();
        let (mut tx, _rx) = capture_channel(stop.clone());
        let (gate, receiving) = receiving_gate();
        let mut input = PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone());
        for (at, x) in [(0, 0), (1, 5000), (2, -5000)] {
            let absolute = CaptureEvent::AbsoluteMotion { x, y: x };
            assert!(!input.process(absolute, false, ms(at), &mut tx, &stop));
            let logical = CaptureEvent::LogicalAbsoluteMotion {
                x: f64::from(x),
                y: f64::from(x),
            };
            assert!(!input.process(logical, false, ms(at), &mut tx, &stop));
        }
        assert_still_receiving(&gate, receiving);
    }

    #[test]
    fn key_takes_back_without_withholding_when_nothing_injected() {
        let stop = CaptureStop::default();
        let (mut tx, mut rx) = capture_channel(stop.clone());
        let (gate, receiving) = receiving_gate();
        let mut input = PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone());
        assert!(!gate.injected_held());
        assert!(!input.process(key(4, true), false, ms(0), &mut tx, &stop));
        assert_took_back(&gate, receiving);
        assert!(!input.process(key(4, true), false, ms(30), &mut tx, &stop));
        assert!(!input.process(key(4, false), false, ms(40), &mut tx, &stop));
        let records = drain(&mut rx);
        assert_eq!(records.len(), 3);
        assert!(records.iter().all(|record| !record.remote));
        assert!(!input.has_suppressed_presses());
    }

    #[test]
    fn withheld_press_is_dropped_and_its_release_quarantined() {
        let stop = CaptureStop::default();
        let (mut tx, mut rx) = capture_channel(stop.clone());
        let (gate, receiving) = receiving_gate();
        gate.note_injected_press();
        assert!(gate.injected_held());
        let mut input = PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone());

        assert!(input.process(key(4, true), false, ms(0), &mut tx, &stop));
        assert_took_back(&gate, receiving);
        assert!(input.process(key(4, true), false, ms(10), &mut tx, &stop));
        assert!(input.has_suppressed_presses());

        gate.note_injected_released();
        assert!(!input.process(relative(1, 0), false, ms(20), &mut tx, &stop));
        assert!(
            input.process(key(4, true), false, ms(30), &mut tx, &stop),
            "a withheld press's repeats stay withheld after withholding ends"
        );
        assert!(input.process(key(4, false), false, ms(40), &mut tx, &stop));
        assert!(!input.has_suppressed_presses());
        assert!(!input.process(key(4, true), false, ms(50), &mut tx, &stop));

        let keys: Vec<_> = drain(&mut rx)
            .into_iter()
            .filter(|record| matches!(record.event, CaptureEvent::Key { .. }))
            .collect();
        assert_eq!(
            keys.len(),
            1,
            "only the fresh press after the release is queued"
        );
        assert!(matches!(
            keys[0].event,
            CaptureEvent::Key {
                pressed: true,
                repeat: false,
                ..
            }
        ));
    }

    #[test]
    fn withheld_events_keep_local_route_tags() {
        let stop = CaptureStop::default();
        let (mut tx, mut rx) = capture_channel(stop.clone());
        let gate = TakeBackGate::new(SharedFloor::new());
        let mut input = PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone());
        assert!(!input.process(key(5, true), false, ms(0), &mut tx, &stop));
        drain(&mut rx);

        let receiving = gate
            .floor()
            .transition(gate.floor().snapshot(), FloorState::Receiving)
            .expect("activation");
        gate.open_injection(receiving.generation);
        gate.note_injected_press();
        let withheld = [
            relative(10, 0),
            CaptureEvent::AbsoluteMotion { x: 1, y: 1 },
            CaptureEvent::Scroll {
                horizontal: 0,
                vertical: 120,
            },
            key(5, true),
        ];
        for (at, event) in (1..).zip(withheld) {
            assert!(input.process(event, false, ms(at), &mut tx, &stop));
        }
        let records = drain(&mut rx);
        assert_eq!(records.len(), withheld.len());
        for record in &records {
            assert!(!record.remote, "withholding never borrows the remote route");
            assert_eq!(record.routing_revision, 0);
        }
        assert_eq!(records[0].floor_generation, receiving.generation);
        let yielding = gate.floor().snapshot().generation;
        assert!(
            records[1..]
                .iter()
                .all(|record| record.floor_generation == yielding)
        );

        assert!(
            !input.process(key(5, false), false, ms(10), &mut tx, &stop),
            "a release local apps are owed is never withheld"
        );
    }

    #[test]
    fn withholding_ends_on_release_or_ttl() {
        let stop = CaptureStop::default();
        let (mut tx, _rx) = capture_channel(stop.clone());
        let (gate, _) = receiving_gate();
        gate.note_injected_press();
        let mut input = PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone());
        assert!(input.process(relative(10, 0), false, ms(0), &mut tx, &stop));
        assert!(input.process(relative(1, 0), false, ms(10), &mut tx, &stop));
        gate.note_injected_released();
        assert!(!input.process(relative(1, 0), false, ms(11), &mut tx, &stop));

        let (gate, _) = receiving_gate();
        gate.note_injected_press();
        let mut input = PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone());
        let start = ms(1000);
        assert!(input.process(relative(0, 10), false, start, &mut tx, &stop));
        let last = start + MAX_SUPPRESSION_TTL - Duration::from_nanos(1);
        assert!(input.process(relative(0, 1), false, last, &mut tx, &stop));
        assert!(gate.injected_held());
        assert!(!input.process(
            relative(0, 1),
            false,
            start + MAX_SUPPRESSION_TTL,
            &mut tx,
            &stop
        ));
    }

    #[test]
    fn withheld_press_not_in_handoff_or_release_plans() {
        let stop = CaptureStop::default();
        let (mut tx, mut rx) = capture_channel(stop.clone());
        let (gate, _) = receiving_gate();
        gate.note_injected_press();
        let mut input = PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone());
        assert!(input.process(button(true), false, ms(0), &mut tx, &stop));
        assert!(input.process(key(0xe1, true), false, ms(1), &mut tx, &stop));
        assert!(input.process(key(4, true), false, ms(2), &mut tx, &stop));
        assert!(
            drain(&mut rx).is_empty(),
            "withheld presses are never queued"
        );

        assert_eq!(input.handoff_plan(true).iter().len(), 0);
        assert_eq!(input.handoff_plan(false).iter().len(), 0);
        assert_eq!(input.release_injected_plan().iter().len(), 0);
        assert!(input.is_ready_for_suppression());

        gate.note_injected_released();
        assert!(input.process(button(false), false, ms(3), &mut tx, &stop));
        assert!(input.process(key(0xe1, false), false, ms(4), &mut tx, &stop));
        assert!(input.process(key(4, false), false, ms(5), &mut tx, &stop));
        assert!(drain(&mut rx).is_empty(), "their releases are never queued");
        assert!(!input.has_suppressed_presses());
    }

    #[test]
    fn records_carry_floor_generation() {
        let stop = CaptureStop::default();
        let (mut tx, mut rx) = capture_channel(stop.clone());
        let mut ungated = PhysicalCapture::new(Duration::ZERO);
        ungated.process(relative(1, 0), false, ms(0), &mut tx, &stop);
        assert_eq!(drain(&mut rx)[0].floor_generation, 0);

        let gate = TakeBackGate::new(SharedFloor::new());
        let mut input = PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone());
        input.process(relative(1, 0), false, ms(1), &mut tx, &stop);
        let requesting = gate
            .floor()
            .transition(gate.floor().snapshot(), FloorState::Requesting)
            .expect("request");
        input.process(relative(1, 0), false, ms(2), &mut tx, &stop);
        gate.floor()
            .transition(requesting, FloorState::Sending)
            .expect("sending");
        input.process(key(4, true), false, ms(3), &mut tx, &stop);
        input.process(button(true), false, ms(4), &mut tx, &stop);
        let generations: Vec<_> = drain(&mut rx)
            .into_iter()
            .map(|record| record.floor_generation)
            .collect();
        assert_eq!(generations, [1, 2, 3, 3]);
    }

    #[test]
    fn withheld_modifier_never_reaches_a_later_modifier_mask() {
        let stop = CaptureStop::default();
        let (mut tx, mut rx) = capture_channel(stop.clone());
        let (gate, receiving) = receiving_gate();
        gate.note_injected_press();
        let mut input = PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone());
        let left_shift = 0xe1;
        assert!(input.process(key(left_shift, true), false, ms(0), &mut tx, &stop));
        assert_took_back(&gate, receiving);

        gate.note_injected_released();
        assert!(!input.process(key(4, true), false, ms(10), &mut tx, &stop));
        assert!(!input.process(key(4, false), false, ms(20), &mut tx, &stop));
        assert!(
            input.process(key(left_shift, false), false, ms(30), &mut tx, &stop),
            "the withheld modifier's physical release stays quarantined"
        );
        assert!(!input.has_suppressed_presses());

        let records = drain(&mut rx);
        assert_eq!(records.len(), 2, "only the typed key is queued");
        for record in records {
            assert!(matches!(
                record.event,
                CaptureEvent::Key {
                    usage: HidUsage(4),
                    modifiers: ModifierState(0),
                    ..
                }
            ));
        }
    }

    #[test]
    fn initially_held_modifier_never_reaches_a_later_modifier_mask() {
        let stop = CaptureStop::default();
        let (mut tx, mut rx) = capture_channel(stop.clone());
        let mut input = PhysicalCapture::new(Duration::ZERO);
        let left_shift = 0xe1;
        input.seed_locally_held_key(HidUsage(left_shift));
        assert!(!input.process(key(4, true), false, ms(0), &mut tx, &stop));
        assert!(!input.process(key(4, false), false, ms(1), &mut tx, &stop));
        assert!(!input.process(key(left_shift, false), false, ms(2), &mut tx, &stop));
        assert!(input.is_ready_for_suppression());

        let records = drain(&mut rx);
        assert_eq!(
            records.len(),
            2,
            "the seeded modifier's release is never queued"
        );
        for record in records {
            assert!(matches!(
                record.event,
                CaptureEvent::Key {
                    usage: HidUsage(4),
                    modifiers: ModifierState(0),
                    ..
                }
            ));
        }
    }

    #[test]
    fn initially_held_modifiers_still_complete_the_emergency_chord() {
        let stop = CaptureStop::default();
        let (mut tx, _rx) = capture_channel(stop.clone());
        let mut input = PhysicalCapture::new(Duration::ZERO);
        for usage in [0xe0, 0xe4] {
            input.seed_locally_held_key(HidUsage(usage));
        }
        input.process(key(0x29, true), false, ms(0), &mut tx, &stop);
        input.tick(ms(2000), &stop);
        assert_eq!(stop.reason(), Some(StopReason::EmergencyEscape));
    }

    fn located(x: i32) -> CaptureEvent {
        CaptureEvent::AbsoluteMotion { x, y: 0 }
    }

    #[test]
    fn hook_travel_takes_back_and_withholds_the_movement_that_triggers_it() {
        let stop = CaptureStop::default();
        let (mut tx, mut rx) = capture_channel(stop.clone());
        let (gate, receiving) = receiving_gate();
        // The peer holds an injected button: a delivered local movement would drag.
        gate.note_injected_press();
        let mut input = PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone());
        let mut travel = |x, dx: f64, at| {
            input.process_with_travel(located(x), Some((dx, 0.0)), false, ms(at), &mut tx, &stop)
        };
        assert!(!travel(4, 4.0, 0), "sub-threshold travel is delivered");
        assert_still_receiving(&gate, receiving);
        assert!(
            travel(6, 2.0, 10),
            "the movement that takes back is withheld"
        );
        assert_took_back(&gate, receiving);
        assert!(
            travel(7, 1.0, 20),
            "later movement is withheld until injection releases"
        );
        assert_eq!(
            drain(&mut rx).len(),
            3,
            "positions are still queued for local routing"
        );
    }

    #[test]
    fn hook_travel_and_relative_motion_never_add_up() {
        let stop = CaptureStop::default();
        let (mut tx, _rx) = capture_channel(stop.clone());
        let (gate, receiving) = receiving_gate();
        let mut input = PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone());
        // One physical movement arrives as both a raw delta and hook travel.
        assert!(!input.process(relative(4, 0), false, ms(0), &mut tx, &stop));
        let four = Some((4.0, 0.0));
        assert!(!input.process_with_travel(located(4), four, false, ms(1), &mut tx, &stop));
        assert_still_receiving(&gate, receiving);
        for travel in [None, Some((f64::NAN, 0.0)), Some((f64::INFINITY, 9.0))] {
            input.process_with_travel(located(4), travel, false, ms(2), &mut tx, &stop);
        }
        assert_still_receiving(&gate, receiving);
        assert!(
            !input.process_with_travel(located(6), Some((2.0, 0.0)), false, ms(3), &mut tx, &stop),
            "nothing injected is held, so the triggering movement passes through"
        );
        assert_took_back(&gate, receiving);
    }

    #[test]
    fn emergency_chord_still_detected_while_withholding() {
        let stop = CaptureStop::default();
        let (mut tx, _rx) = capture_channel(stop.clone());
        let (gate, receiving) = receiving_gate();
        gate.note_injected_press();
        let mut input = PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone());
        for (at, usage) in [(0, 0xe0), (1, 0xe4), (2, 0x29)] {
            assert!(input.process(key(usage, true), false, ms(at), &mut tx, &stop));
        }
        assert_took_back(&gate, receiving);
        input.tick(ms(2001), &stop);
        assert!(!stop.is_stopped());
        input.tick(ms(2002), &stop);
        assert_eq!(stop.reason(), Some(StopReason::EmergencyEscape));
    }
}
