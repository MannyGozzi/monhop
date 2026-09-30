//! Windows-style middle-button autoscroll for a Mac driven by a Windows mouse. The state machine is
//! pure: the destination feeds it every action and the clock, then posts what it routes, the scroll
//! each tick asks for, and the marker it reports.

use std::time::{Duration, Instant};

use monhop_core::{HidUsage, MouseButton, Point};

use crate::{
    session_native::MAC_POINTS_PER_DETENT,
    session_receiver::{DestinationAction, DestinationFailure},
};

/// Pointer travel from the origin, in points, that turns a held middle button into a drag.
const DRAG_THRESHOLD: f64 = 5.0;
/// Offset from the origin on each axis, in points, that scrolls nothing.
const DEAD_ZONE: f64 = 12.0;
/// Speed gained per point past the dead zone, in points per second: about 500 pt/s at 100 pt.
const GAIN: f64 = 5.7;
/// The fastest scroll on each axis, in points per second.
const MAX_SPEED: f64 = 4000.0;
/// Scroll posts at most this often, each covering the time since the last.
const EMIT_INTERVAL: Duration = Duration::from_millis(8);
/// The most time one post covers, so a stalled injection thread never jumps the page.
const MAX_STEP: Duration = Duration::from_millis(50);
const ESCAPE: HidUsage = HidUsage(0x29);
const MIDDLE: usize = MouseButton::Middle.index();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Scrolls while the middle button stays down.
    Drag,
    /// Entered by a middle click; the next button or non-modifier key press ends it.
    Toggle,
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Idle,
    /// The middle button is down and held back; its release decides between a click and a drag.
    Pressed {
        origin: Point,
        controller: u64,
    },
    Scrolling {
        mode: Mode,
        origin: Point,
        controller: u64,
        started: Instant,
        emitted: Instant,
    },
}

/// One scroll step: wire detents, and the origin its event is placed at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Step {
    pub(crate) scroll: DestinationAction,
    pub(crate) at: Point,
}

pub(crate) struct Autoscroll {
    phase: Phase,
    pointer: Option<Point>,
    /// Presses kept from the app whose releases must be kept from it too.
    withheld_buttons: [bool; 5],
    withheld_escape: bool,
    /// An Escape press reached the app, so a repeat of it is not a new press to swallow.
    escape_delivered: bool,
}

impl Autoscroll {
    pub(crate) const fn new() -> Self {
        Self {
            phase: Phase::Idle,
            pointer: None,
            withheld_buttons: [false; 5],
            withheld_escape: false,
            escape_delivered: false,
        }
    }

    /// The origin while either mode scrolls, where the marker belongs.
    pub(crate) fn marker(&self) -> Option<Point> {
        match self.phase {
            Phase::Scrolling { origin, .. } => Some(origin),
            Phase::Idle | Phase::Pressed { .. } => None,
        }
    }

    /// Routes one action, posting through `post` whatever reaches the app. `starts` names the
    /// current controller when a middle press may start autoscroll: a Windows source, switch on.
    pub(crate) fn apply(
        &mut self,
        action: DestinationAction,
        starts: Option<u64>,
        now: Instant,
        mut post: impl FnMut(DestinationAction) -> Result<(), DestinationFailure>,
    ) -> Result<(), DestinationFailure> {
        // The switch or the controller can change between ticks; input must not outrun that.
        if self.owner().is_some_and(|owner| starts != Some(owner)) {
            self.stop(now);
        }
        match action {
            DestinationAction::MoveTo(point) => {
                self.pointer = Some(point);
                if let Phase::Pressed {
                    origin, controller, ..
                } = self.phase
                    && (point.x - origin.x).hypot(point.y - origin.y) > DRAG_THRESHOLD
                {
                    self.phase = Phase::Scrolling {
                        mode: Mode::Drag,
                        origin,
                        controller,
                        started: now,
                        emitted: now,
                    };
                }
                post(action)
            }
            DestinationAction::EndGestures => {
                self.stop(now);
                post(action)
            }
            DestinationAction::ReleaseAll => {
                self.stop(now);
                self.withheld_buttons = [false; 5];
                self.withheld_escape = false;
                self.escape_delivered = false;
                post(action)
            }
            DestinationAction::Button {
                button,
                pressed: true,
                ..
            } => {
                let index = button.index();
                if self.mode() == Some(Mode::Toggle) {
                    self.finish(now);
                    self.withheld_buttons[index] = true;
                    return Ok(());
                }
                self.withheld_buttons[index] = false;
                match (button, self.phase, starts, self.pointer) {
                    (MouseButton::Middle, Phase::Idle, Some(controller), Some(origin)) => {
                        self.phase = Phase::Pressed { origin, controller };
                        Ok(())
                    }
                    _ => post(action),
                }
            }
            DestinationAction::Button {
                button,
                pressed: false,
                ..
            } => {
                if std::mem::take(&mut self.withheld_buttons[button.index()]) {
                    return Ok(());
                }
                match (button, self.phase) {
                    (MouseButton::Middle, Phase::Pressed { origin, controller }) => {
                        self.phase = Phase::Scrolling {
                            mode: Mode::Toggle,
                            origin,
                            controller,
                            started: now,
                            emitted: now,
                        };
                        // A single click where the pointer is: moving it, or a multi-click count that
                        // snaps it back, would drag any other held button.
                        for pressed in [true, false] {
                            post(DestinationAction::Button {
                                button: MouseButton::Middle,
                                pressed,
                                click_count: 1,
                            })?;
                        }
                        Ok(())
                    }
                    (
                        MouseButton::Middle,
                        Phase::Scrolling {
                            mode: Mode::Drag, ..
                        },
                    ) => {
                        self.finish(now);
                        Ok(())
                    }
                    _ => post(action),
                }
            }
            DestinationAction::Key {
                usage,
                pressed: true,
            } => {
                // Escape's auto-repeat follows its swallowed press.
                if usage == ESCAPE && self.withheld_escape {
                    return Ok(());
                }
                if self.mode() == Some(Mode::Toggle) && !usage.is_modifier() {
                    self.finish(now);
                    if usage == ESCAPE && !self.escape_delivered {
                        self.withheld_escape = true;
                        return Ok(());
                    }
                }
                post(action)?;
                self.escape_delivered |= usage == ESCAPE;
                Ok(())
            }
            DestinationAction::Key {
                usage,
                pressed: false,
            } => {
                if usage == ESCAPE && std::mem::take(&mut self.withheld_escape) {
                    return Ok(());
                }
                if usage == ESCAPE {
                    self.escape_delivered = false;
                }
                post(action)
            }
            DestinationAction::Scroll { .. }
            | DestinationAction::Gesture(_)
            | DestinationAction::System(_) => post(action),
        }
    }

    /// The scroll owed since the last step, at most every `EMIT_INTERVAL`. Ends the episode once
    /// `controller` (None: control ended) is not the one it started under or the switch is off.
    pub(crate) fn tick(
        &mut self,
        now: Instant,
        controller: Option<u64>,
        enabled: bool,
    ) -> Option<Step> {
        let owner = self.owner()?;
        if !enabled || controller != Some(owner) {
            self.stop(now);
            return None;
        }
        let Phase::Scrolling {
            origin,
            ref mut emitted,
            ..
        } = self.phase
        else {
            return None;
        };
        let elapsed = now.saturating_duration_since(*emitted);
        if elapsed < EMIT_INTERVAL {
            return None;
        }
        *emitted = now;
        let seconds = elapsed.min(MAX_STEP).as_secs_f64();
        let pointer = self.pointer.unwrap_or(origin);
        let right = speed(pointer.x - origin.x) * seconds;
        let down = speed(pointer.y - origin.y) * seconds;
        if right == 0.0 && down == 0.0 {
            return None;
        }
        // Wire detents are positive right and up; below the origin is a wheel turned toward you.
        Some(Step {
            scroll: DestinationAction::Scroll {
                horizontal: right / MAC_POINTS_PER_DETENT,
                vertical: -down / MAC_POINTS_PER_DETENT,
            },
            at: origin,
        })
    }

    /// Ends any episode from outside the input stream; a middle button still held back keeps its
    /// release from the app.
    pub(crate) fn stop(&mut self, now: Instant) {
        if matches!(
            self.phase,
            Phase::Pressed { .. }
                | Phase::Scrolling {
                    mode: Mode::Drag,
                    ..
                }
        ) {
            self.withheld_buttons[MIDDLE] = true;
        }
        self.finish(now);
    }

    /// The controller the running episode started under.
    fn owner(&self) -> Option<u64> {
        match self.phase {
            Phase::Idle => None,
            Phase::Pressed { controller, .. } | Phase::Scrolling { controller, .. } => {
                Some(controller)
            }
        }
    }

    fn mode(&self) -> Option<Mode> {
        match self.phase {
            Phase::Scrolling { mode, .. } => Some(mode),
            Phase::Idle | Phase::Pressed { .. } => None,
        }
    }

    fn finish(&mut self, now: Instant) {
        if let Phase::Scrolling { mode, started, .. } =
            std::mem::replace(&mut self.phase, Phase::Idle)
        {
            log::info!(
                "autoscroll: {} mode ended after {} ms",
                match mode {
                    Mode::Drag => "drag",
                    Mode::Toggle => "toggle",
                },
                now.saturating_duration_since(started).as_millis()
            );
        }
    }
}

/// Signed speed, in points per second, for an offset from the origin on one axis.
fn speed(offset: f64) -> f64 {
    let beyond = offset.abs() - DEAD_ZONE;
    if beyond <= 0.0 {
        return 0.0;
    }
    (beyond * GAIN).min(MAX_SPEED).copysign(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOWS: Option<u64> = Some(7);
    const SHIFT: HidUsage = HidUsage(0xe1);
    const A: HidUsage = HidUsage(0x04);

    struct Harness {
        machine: Autoscroll,
        start: Instant,
        posted: Vec<DestinationAction>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                machine: Autoscroll::new(),
                start: Instant::now(),
                posted: Vec::new(),
            }
        }

        fn at(&self, millis: u64) -> Instant {
            self.start + Duration::from_millis(millis)
        }

        /// Applies `action` at `millis` and returns what reached the app.
        fn send(
            &mut self,
            action: DestinationAction,
            starts: Option<u64>,
            millis: u64,
        ) -> Vec<DestinationAction> {
            let now = self.at(millis);
            let posted = &mut self.posted;
            let before = posted.len();
            self.machine
                .apply(action, starts, now, |post| {
                    posted.push(post);
                    Ok(())
                })
                .unwrap();
            posted.split_off(before)
        }

        fn tick(&mut self, millis: u64) -> Option<Step> {
            let now = self.at(millis);
            self.machine.tick(now, WINDOWS, true)
        }

        /// A Windows source's middle click at (100, 100), entering toggle autoscroll.
        fn toggled() -> Self {
            let mut harness = Self::new();
            harness.send(move_to(100.0, 100.0), WINDOWS, 0);
            harness.send(middle(true), WINDOWS, 1);
            harness.send(middle(false), WINDOWS, 2);
            assert_eq!(harness.machine.marker(), Some(Point::new(100.0, 100.0)));
            harness
        }
    }

    fn move_to(x: f64, y: f64) -> DestinationAction {
        DestinationAction::MoveTo(Point::new(x, y))
    }

    fn button(button: MouseButton, pressed: bool) -> DestinationAction {
        DestinationAction::Button {
            button,
            pressed,
            click_count: 1,
        }
    }

    fn middle(pressed: bool) -> DestinationAction {
        button(MouseButton::Middle, pressed)
    }

    fn key(usage: HidUsage, pressed: bool) -> DestinationAction {
        DestinationAction::Key { usage, pressed }
    }

    fn scroll(step: Option<Step>) -> (f64, f64) {
        match step {
            Some(Step {
                scroll:
                    DestinationAction::Scroll {
                        horizontal,
                        vertical,
                    },
                ..
            }) => (horizontal, vertical),
            other => panic!("expected a scroll, got {other:?}"),
        }
    }

    #[test]
    fn a_held_middle_button_dragged_past_the_threshold_scrolls_until_released() {
        let mut h = Harness::new();
        h.send(move_to(100.0, 100.0), WINDOWS, 0);
        assert_eq!(
            h.send(middle(true), WINDOWS, 1),
            [],
            "the press is held back"
        );
        assert_eq!(
            h.send(move_to(103.0, 101.0), WINDOWS, 2),
            [move_to(103.0, 101.0)]
        );
        assert_eq!(
            h.machine.marker(),
            None,
            "within the threshold nothing scrolls"
        );
        assert_eq!(h.tick(20), None);
        assert_eq!(
            h.send(move_to(100.0, 200.0), WINDOWS, 30),
            [move_to(100.0, 200.0)],
            "the pointer keeps moving"
        );
        assert_eq!(h.machine.marker(), Some(Point::new(100.0, 100.0)));
        let step = h.tick(46).expect("a scroll after the emit interval");
        assert_eq!(step.at, Point::new(100.0, 100.0), "placed at the origin");
        let (horizontal, vertical) = scroll(Some(step));
        assert_eq!(horizontal, 0.0);
        assert!(vertical < 0.0, "below the origin scrolls down");
        assert_eq!(
            h.send(middle(false), WINDOWS, 50),
            [],
            "no middle button reaches the app"
        );
        assert_eq!(h.machine.marker(), None);
        assert_eq!(h.tick(100), None);
        assert_eq!(h.send(middle(true), None, 110), [middle(true)]);
        assert_eq!(h.send(middle(false), None, 120), [middle(false)]);
    }

    #[test]
    fn a_middle_click_is_delivered_at_the_pointer_then_toggles_autoscroll() {
        let mut h = Harness::new();
        h.send(move_to(100.0, 100.0), WINDOWS, 0);
        h.send(middle(true), WINDOWS, 1);
        h.send(move_to(102.0, 101.0), WINDOWS, 2);
        assert_eq!(
            h.send(middle(false), WINDOWS, 3),
            [middle(true), middle(false)]
        );
        assert_eq!(h.machine.marker(), Some(Point::new(100.0, 100.0)));
    }

    #[test]
    fn a_held_back_middle_click_never_moves_the_pointer_under_another_held_button() {
        let mut h = Harness::new();
        h.send(move_to(100.0, 100.0), WINDOWS, 0);
        h.send(middle(true), WINDOWS, 1);
        h.send(move_to(104.0, 100.0), WINDOWS, 2);
        assert_eq!(
            h.send(button(MouseButton::Left, true), WINDOWS, 3),
            [button(MouseButton::Left, true)]
        );
        assert_eq!(
            h.send(middle(false), WINDOWS, 4),
            [middle(true), middle(false)],
            "no move that would drag the held left button"
        );
        assert_eq!(
            h.send(button(MouseButton::Left, false), WINDOWS, 5),
            [button(MouseButton::Left, false)]
        );
    }

    #[test]
    fn input_after_the_switch_turns_off_never_waits_for_a_tick() {
        let mut pressed = Harness::new();
        pressed.send(move_to(100.0, 100.0), WINDOWS, 0);
        pressed.send(middle(true), WINDOWS, 1);
        assert_eq!(
            pressed.send(middle(false), None, 2),
            [],
            "no click and no toggle once off"
        );
        assert_eq!(pressed.machine.marker(), None);

        let mut toggled = Harness::toggled();
        let left = MouseButton::Left;
        assert_eq!(
            toggled.send(button(left, true), None, 10),
            [button(left, true)]
        );
        assert_eq!(toggled.machine.marker(), None);
        assert_eq!(
            toggled.send(button(left, false), None, 11),
            [button(left, false)]
        );
    }

    #[test]
    fn a_button_press_ends_toggle_autoscroll_and_it_and_its_release_are_swallowed() {
        for ending in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            let mut h = Harness::toggled();
            assert_eq!(h.send(button(ending, true), WINDOWS, 10), []);
            assert_eq!(h.machine.marker(), None, "{ending:?}");
            assert_eq!(h.tick(40), None);
            assert_eq!(h.send(button(ending, false), WINDOWS, 20), []);
            assert_eq!(
                h.send(button(ending, true), None, 30),
                [button(ending, true)],
                "the next press reaches the app"
            );
            assert_eq!(
                h.send(button(ending, false), None, 40),
                [button(ending, false)]
            );
        }
    }

    #[test]
    fn escape_is_swallowed_another_key_ends_and_is_delivered_and_a_modifier_does_neither() {
        let mut h = Harness::toggled();
        assert_eq!(h.send(key(SHIFT, true), WINDOWS, 10), [key(SHIFT, true)]);
        assert_eq!(h.send(key(SHIFT, false), WINDOWS, 11), [key(SHIFT, false)]);
        assert!(h.machine.marker().is_some(), "a modifier keeps the mode");
        assert_eq!(h.send(key(ESCAPE, true), WINDOWS, 12), []);
        assert_eq!(h.machine.marker(), None);
        assert_eq!(h.send(key(ESCAPE, true), WINDOWS, 13), [], "its repeat");
        assert_eq!(h.send(key(ESCAPE, false), WINDOWS, 14), []);
        assert_eq!(h.send(key(ESCAPE, true), WINDOWS, 15), [key(ESCAPE, true)]);
        assert_eq!(
            h.send(key(ESCAPE, false), WINDOWS, 16),
            [key(ESCAPE, false)]
        );

        let mut h = Harness::toggled();
        assert_eq!(h.send(key(A, true), WINDOWS, 10), [key(A, true)]);
        assert_eq!(h.machine.marker(), None);
        assert_eq!(h.send(key(A, false), WINDOWS, 11), [key(A, false)]);
    }

    #[test]
    fn an_escape_already_down_ends_toggle_autoscroll_and_its_release_still_arrives() {
        let mut h = Harness::new();
        h.send(move_to(100.0, 100.0), WINDOWS, 0);
        assert_eq!(h.send(key(ESCAPE, true), WINDOWS, 1), [key(ESCAPE, true)]);
        h.send(middle(true), WINDOWS, 2);
        h.send(middle(false), WINDOWS, 3);
        assert!(h.machine.marker().is_some());
        assert_eq!(
            h.send(key(ESCAPE, true), WINDOWS, 4),
            [key(ESCAPE, true)],
            "a repeat of a delivered press"
        );
        assert_eq!(h.machine.marker(), None);
        assert_eq!(
            h.send(key(ESCAPE, false), WINDOWS, 5),
            [key(ESCAPE, false)],
            "so the app never keeps Escape down"
        );
    }

    #[test]
    fn the_dead_zone_scrolls_nothing() {
        for offset in [0.0, 5.0, -DEAD_ZONE, DEAD_ZONE] {
            assert_eq!(speed(offset), 0.0, "{offset}");
        }
        let mut h = Harness::toggled();
        h.send(move_to(111.0, 89.0), WINDOWS, 5);
        assert_eq!(h.tick(20), None);
        assert!(h.machine.marker().is_some());
    }

    #[test]
    fn speed_grows_with_distance_on_each_axis_with_the_wire_sign() {
        assert!(speed(50.0) > 0.0 && speed(50.0) < speed(100.0));
        assert!((speed(100.0) - 501.6).abs() < 1e-9);
        assert_eq!(speed(-100.0), -speed(100.0));
        assert_eq!(speed(1.0e6), MAX_SPEED);

        let mut h = Harness::toggled();
        h.send(move_to(200.0, 200.0), WINDOWS, 3);
        assert_eq!(h.tick(5), None, "paced by the emit interval");
        let (horizontal, vertical) = scroll(h.tick(18));
        assert!(horizontal > 0.0, "right of the origin scrolls right");
        assert!(vertical < 0.0, "below the origin scrolls down");
        assert!((horizontal * MAC_POINTS_PER_DETENT - 501.6 * 0.016).abs() < 1e-9);
        assert_eq!(horizontal, -vertical);

        h.send(move_to(50.0, 90.0), WINDOWS, 19);
        let (horizontal, vertical) = scroll(h.tick(1_018));
        assert!(
            horizontal < 0.0 && vertical == 0.0,
            "left scrolls left, y in the dead zone"
        );
        assert!(
            (horizontal * MAC_POINTS_PER_DETENT + speed(50.0) * 0.05).abs() < 1e-9,
            "a stall covers at most one step"
        );
    }

    #[test]
    fn release_yield_or_the_switch_turning_off_ends_the_mode_and_hides_the_marker() {
        for release in [
            DestinationAction::EndGestures,
            DestinationAction::ReleaseAll,
        ] {
            let mut h = Harness::toggled();
            assert_eq!(h.send(release, WINDOWS, 10), [release]);
            assert_eq!(h.machine.marker(), None);
            assert_eq!(h.tick(40), None);
        }

        let yielded = [(Some(8), true), (None, true), (WINDOWS, false)];
        for (controller, enabled) in yielded {
            let mut h = Harness::toggled();
            assert_eq!(h.machine.tick(h.at(20), controller, enabled), None);
            assert_eq!(h.machine.marker(), None);
            assert_eq!(h.tick(40), None, "and never scrolls again");
        }

        let mut dragging = Harness::new();
        dragging.send(move_to(100.0, 100.0), WINDOWS, 0);
        dragging.send(middle(true), WINDOWS, 1);
        dragging.send(move_to(100.0, 300.0), WINDOWS, 2);
        assert_eq!(dragging.machine.tick(dragging.at(20), WINDOWS, false), None);
        assert_eq!(dragging.machine.marker(), None);
        assert_eq!(
            dragging.send(middle(false), WINDOWS, 30),
            [],
            "its release stays held back"
        );

        let mut pressed = Harness::new();
        pressed.send(move_to(100.0, 100.0), WINDOWS, 0);
        pressed.send(middle(true), WINDOWS, 1);
        pressed.machine.stop(pressed.at(2));
        assert_eq!(
            pressed.send(middle(false), WINDOWS, 3),
            [],
            "no click after the end"
        );
        assert_eq!(pressed.machine.marker(), None);

        let mut released = Harness::new();
        released.send(move_to(100.0, 100.0), WINDOWS, 0);
        released.send(middle(true), WINDOWS, 1);
        released.send(DestinationAction::ReleaseAll, WINDOWS, 2);
        assert_eq!(
            released.send(middle(false), WINDOWS, 3),
            [middle(false)],
            "a release of everything forgets what it held back"
        );
    }

    #[test]
    fn a_mac_source_or_the_switch_off_passes_the_middle_button_through() {
        let mut h = Harness::new();
        h.send(move_to(100.0, 100.0), None, 0);
        assert_eq!(h.send(middle(true), None, 1), [middle(true)]);
        assert_eq!(
            h.send(move_to(100.0, 300.0), None, 2),
            [move_to(100.0, 300.0)]
        );
        assert_eq!(h.machine.marker(), None);
        assert_eq!(h.tick(40), None);
        assert_eq!(h.send(middle(false), None, 50), [middle(false)]);
        assert_eq!(h.send(key(ESCAPE, true), None, 60), [key(ESCAPE, true)]);
    }
}
