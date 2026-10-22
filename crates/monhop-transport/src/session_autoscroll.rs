//! Windows-style middle-button autoscroll for a Mac driven by a Windows mouse: holding the button
//! and moving scrolls, and a click stays a click. The state machine is pure: the destination feeds
//! it every action and the clock, then posts what it routes, the scroll each tick asks for, and the
//! marker it reports.

use std::time::{Duration, Instant};

use monhop_core::{MouseButton, Point};

use crate::{
    session_native::MAC_POINTS_PER_DETENT,
    session_receiver::{DestinationAction, DestinationFailure},
};

/// Pointer travel from the origin, in points, past which a held middle button scrolls instead of
/// clicking.
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

#[derive(Clone, Copy, Debug)]
enum Phase {
    Idle,
    /// The middle button is down and held back: travel past `DRAG_THRESHOLD` scrolls, and a
    /// release before that is a click.
    Pressed {
        origin: Point,
        controller: u64,
    },
    /// Scrolls while the middle button stays down.
    Scrolling {
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
    /// A middle press kept from the app whose release must be kept from it too.
    withheld_middle: bool,
}

impl Autoscroll {
    pub(crate) const fn new() -> Self {
        Self {
            phase: Phase::Idle,
            pointer: None,
            withheld_middle: false,
        }
    }

    /// The origin while scrolling, where the marker belongs.
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
                if let Phase::Pressed { origin, controller } = self.phase
                    && (point.x - origin.x).hypot(point.y - origin.y) > DRAG_THRESHOLD
                {
                    self.phase = Phase::Scrolling {
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
                self.withheld_middle = false;
                post(action)
            }
            DestinationAction::Button {
                button: MouseButton::Middle,
                pressed: true,
                ..
            } => {
                self.withheld_middle = false;
                match (self.phase, starts, self.pointer) {
                    (Phase::Idle, Some(controller), Some(origin)) => {
                        self.phase = Phase::Pressed { origin, controller };
                        Ok(())
                    }
                    _ => post(action),
                }
            }
            DestinationAction::Button {
                button: MouseButton::Middle,
                pressed: false,
                ..
            } => {
                if std::mem::take(&mut self.withheld_middle) {
                    return Ok(());
                }
                match self.phase {
                    Phase::Pressed { .. } => {
                        self.phase = Phase::Idle;
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
                    Phase::Scrolling { .. } => {
                        self.finish(now);
                        Ok(())
                    }
                    Phase::Idle => post(action),
                }
            }
            DestinationAction::Button { .. }
            | DestinationAction::Key { .. }
            | DestinationAction::Scroll { .. }
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

    /// Ends any episode from outside the input stream; the middle button it held back keeps its
    /// release from the app.
    pub(crate) fn stop(&mut self, now: Instant) {
        if self.owner().is_some() {
            self.withheld_middle = true;
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

    fn finish(&mut self, now: Instant) {
        if let Phase::Scrolling { started, .. } = std::mem::replace(&mut self.phase, Phase::Idle) {
            log::info!(
                "autoscroll: drag ended after {} ms",
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
    use monhop_core::HidUsage;

    use super::*;

    const WINDOWS: Option<u64> = Some(7);
    const ESCAPE: HidUsage = HidUsage(0x29);
    const A: HidUsage = HidUsage(0x04);
    const LEFT: MouseButton = MouseButton::Left;
    const ORIGIN: Point = Point::new(100.0, 100.0);
    const CLICK: [DestinationAction; 2] = [middle(true), middle(false)];

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

        /// A middle press and its release at `millis`, returning what reached the app.
        fn click(&mut self, starts: Option<u64>, millis: u64) -> Vec<DestinationAction> {
            let mut posted = self.send(middle(true), starts, millis);
            posted.extend(self.send(middle(false), starts, millis + 1));
            posted
        }

        /// A Windows source's middle button held at the origin, not yet a click or a drag.
        fn pressed() -> Self {
            let mut harness = Self::new();
            harness.send(DestinationAction::MoveTo(ORIGIN), WINDOWS, 0);
            assert_eq!(
                harness.send(middle(true), WINDOWS, 1),
                [],
                "the press is held back"
            );
            harness
        }

        /// `pressed`, then dragged straight down past the threshold at 2 ms.
        fn dragging() -> Self {
            let mut harness = Self::pressed();
            harness.send(move_to(100.0, 300.0), WINDOWS, 2);
            assert_eq!(harness.machine.marker(), Some(ORIGIN));
            harness
        }
    }

    fn move_to(x: f64, y: f64) -> DestinationAction {
        DestinationAction::MoveTo(Point::new(x, y))
    }

    const fn button(button: MouseButton, pressed: bool) -> DestinationAction {
        DestinationAction::Button {
            button,
            pressed,
            click_count: 1,
        }
    }

    const fn middle(pressed: bool) -> DestinationAction {
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
    fn a_quick_middle_click_is_one_click_and_leaves_nothing_behind() {
        let mut h = Harness::pressed();
        assert_eq!(h.send(middle(false), WINDOWS, 2), CLICK);
        assert_eq!(h.machine.marker(), None, "no marker");

        let double = |pressed| DestinationAction::Button {
            button: MouseButton::Middle,
            pressed,
            click_count: 2,
        };
        assert_eq!(h.send(double(true), WINDOWS, 3), []);
        assert_eq!(
            h.send(double(false), WINDOWS, 4),
            CLICK,
            "a single click each time, so the pointer never snaps back"
        );

        assert_eq!(
            h.send(move_to(100.0, 400.0), WINDOWS, 5),
            [move_to(100.0, 400.0)]
        );
        assert_eq!(h.tick(40), None, "moving afterwards scrolls nothing");
        assert_eq!(h.machine.marker(), None);
        for action in [
            button(LEFT, true),
            button(LEFT, false),
            key(ESCAPE, true),
            key(ESCAPE, false),
        ] {
            assert_eq!(
                h.send(action, WINDOWS, 50),
                [action],
                "nothing after the click is withheld"
            );
        }
    }

    #[test]
    fn a_press_moved_no_further_than_the_threshold_is_still_a_click() {
        let mut h = Harness::pressed();
        assert_eq!(
            h.send(move_to(103.0, 104.0), WINDOWS, 2),
            [move_to(103.0, 104.0)]
        );
        assert_eq!(h.machine.marker(), None, "exactly the threshold away");
        assert_eq!(h.tick(20), None);
        assert_eq!(h.send(middle(false), WINDOWS, 30), CLICK);
        assert_eq!(h.machine.marker(), None);
        assert_eq!(h.tick(60), None);
    }

    #[test]
    fn a_held_middle_button_dragged_past_the_threshold_scrolls_until_released() {
        let mut h = Harness::pressed();
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
        assert_eq!(h.machine.marker(), Some(ORIGIN));
        let step = h.tick(46).expect("a scroll after the emit interval");
        assert_eq!(step.at, ORIGIN, "placed at the origin");
        let (horizontal, vertical) = scroll(Some(step));
        assert_eq!(horizontal, 0.0);
        assert!(vertical < 0.0, "below the origin scrolls down");
        assert_eq!(
            h.send(middle(false), WINDOWS, 50),
            [],
            "no click reaches the app"
        );
        assert_eq!(h.machine.marker(), None);
        assert_eq!(h.tick(100), None);
        assert_eq!(h.click(WINDOWS, 110), CLICK, "the next click is a click");
    }

    #[test]
    fn keys_and_other_buttons_pass_through_while_dragging() {
        let mut h = Harness::dragging();
        for action in [
            key(ESCAPE, true),
            key(ESCAPE, false),
            key(A, true),
            key(A, false),
            button(LEFT, true),
            button(LEFT, false),
        ] {
            assert_eq!(h.send(action, WINDOWS, 10), [action]);
        }
        assert_eq!(
            h.machine.marker(),
            Some(ORIGIN),
            "only the middle release ends a drag"
        );
        assert_eq!(h.send(middle(false), WINDOWS, 20), []);
        assert_eq!(h.machine.marker(), None);
    }

    #[test]
    fn a_held_back_middle_click_never_moves_the_pointer_under_another_held_button() {
        let mut h = Harness::pressed();
        h.send(move_to(104.0, 100.0), WINDOWS, 2);
        assert_eq!(h.send(button(LEFT, true), WINDOWS, 3), [button(LEFT, true)]);
        assert_eq!(
            h.send(middle(false), WINDOWS, 4),
            CLICK,
            "no move that would drag the held left button"
        );
        assert_eq!(h.machine.marker(), None);
        assert_eq!(
            h.send(button(LEFT, false), WINDOWS, 5),
            [button(LEFT, false)]
        );
    }

    #[test]
    fn release_all_while_pressed_posts_only_itself_and_the_next_click_works() {
        let release = DestinationAction::ReleaseAll;
        let mut h = Harness::pressed();
        assert_eq!(h.send(release, WINDOWS, 2), [release], "no click, no press");
        assert_eq!(h.machine.marker(), None);
        assert_eq!(h.tick(20), None);
        assert_eq!(h.click(WINDOWS, 30), CLICK);
        assert_eq!(h.machine.marker(), None);

        let mut h = Harness::pressed();
        h.send(release, WINDOWS, 2);
        assert_eq!(
            h.send(middle(false), WINDOWS, 3),
            [middle(false)],
            "a release of everything forgets what it held back"
        );
        assert_eq!(h.click(WINDOWS, 10), CLICK);
    }

    #[test]
    fn a_controller_change_or_the_switch_off_mid_press_or_drag_posts_no_middle_button() {
        let starts_from = [Harness::pressed, Harness::dragging];
        // Seen by the next input (a move) or by the tick, before the release arrives.
        let endings: [fn(&mut Harness); 5] = [
            |h| {
                assert_eq!(
                    h.send(move_to(90.0, 90.0), Some(8), 5),
                    [move_to(90.0, 90.0)]
                )
            },
            |h| assert_eq!(h.send(move_to(90.0, 90.0), None, 5), [move_to(90.0, 90.0)]),
            |h| assert_eq!(h.machine.tick(h.at(20), Some(8), true), None),
            |h| assert_eq!(h.machine.tick(h.at(20), None, true), None),
            |h| assert_eq!(h.machine.tick(h.at(20), WINDOWS, false), None),
        ];
        for start in starts_from {
            for (case, end) in endings.iter().enumerate() {
                let mut h = start();
                end(&mut h);
                assert_eq!(h.machine.marker(), None, "case {case}");
                assert_eq!(h.tick(40), None, "case {case}: never scrolls again");
                assert_eq!(
                    h.send(middle(false), WINDOWS, 50),
                    [],
                    "case {case}: its release stays held back"
                );
                assert_eq!(h.click(None, 60), CLICK, "case {case}: plain clicks pass");
                assert_eq!(h.click(WINDOWS, 70), CLICK, "case {case}: and so do ours");
            }
        }
    }

    #[test]
    fn end_gestures_or_a_stop_mid_press_or_drag_holds_back_the_release() {
        for start in [Harness::pressed, Harness::dragging] {
            let mut h = start();
            let end = DestinationAction::EndGestures;
            assert_eq!(h.send(end, WINDOWS, 10), [end]);
            assert_eq!(h.machine.marker(), None);
            assert_eq!(h.tick(40), None);
            assert_eq!(
                h.send(middle(false), WINDOWS, 50),
                [],
                "no click after the end"
            );

            let mut h = start();
            h.machine.stop(h.at(10));
            assert_eq!(h.machine.marker(), None);
            assert_eq!(h.send(middle(false), WINDOWS, 50), []);
            assert_eq!(h.click(WINDOWS, 60), CLICK);
        }
    }

    #[test]
    fn the_dead_zone_scrolls_nothing() {
        for offset in [0.0, 5.0, -DEAD_ZONE, DEAD_ZONE] {
            assert_eq!(speed(offset), 0.0, "{offset}");
        }
        let mut h = Harness::pressed();
        h.send(move_to(111.0, 89.0), WINDOWS, 2);
        assert_eq!(h.tick(20), None);
        assert_eq!(h.machine.marker(), Some(ORIGIN));
    }

    #[test]
    fn speed_grows_with_distance_on_each_axis_with_the_wire_sign() {
        assert!(speed(50.0) > 0.0 && speed(50.0) < speed(100.0));
        assert!((speed(100.0) - 501.6).abs() < 1e-9);
        assert_eq!(speed(-100.0), -speed(100.0));
        assert_eq!(speed(1.0e6), MAX_SPEED);

        let mut h = Harness::pressed();
        h.send(move_to(200.0, 200.0), WINDOWS, 2);
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
    }
}
