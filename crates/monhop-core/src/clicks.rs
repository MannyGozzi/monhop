//! Multi-clicks for forwarded presses. The source numbers each press from the peer cursor it
//! tracks, never from either OS's view of a pointer pinned while control is remote, and the
//! receiver lands a multi-click on its first press.

use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};

use crate::{
    topology::LogicalRect,
    types::{MouseButton, Point},
};

/// The click count of a press that does not continue a multi-click.
pub const SINGLE_CLICK: u8 = 1;

/// Used when the source user's double-click interval cannot be read; both OSes ship with it.
pub const FALLBACK_DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);

/// Peer px/pt per axis a press may stray from its sequence's first press, where every receiver
/// lands it: looser than Windows' ±2 px for jitter, under half a 20 px row, so never a neighbour.
pub const DOUBLE_CLICK_SLOP: f64 = 8.0;

/// Now on the one monotonic clock both platforms' capture threads stamp buttons with; only the
/// difference between two readings means anything.
pub fn capture_clock() -> Duration {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN.get_or_init(Instant::now).elapsed()
}

/// The one per-axis test for whether a press continues its sequence's first press.
fn within_slop(from: Point, to: Point) -> bool {
    (from.x - to.x).abs() <= DOUBLE_CLICK_SLOP && (from.y - to.y).abs() <= DOUBLE_CLICK_SLOP
}

#[derive(Clone, Copy, Debug)]
struct Sequence {
    button: MouseButton,
    /// The first press's position: every receiver lands each later press of the sequence there.
    anchor: Point,
    at: Duration,
    count: u8,
}

/// Numbers presses: the same button again within the interval of its previous press, and within
/// [`DOUBLE_CLICK_SLOP`] of the sequence's first press, continues the sequence.
#[derive(Clone, Copy, Debug)]
pub struct ClickCounter {
    interval: Duration,
    last: Option<Sequence>,
}

impl Default for ClickCounter {
    fn default() -> Self {
        Self::new(FALLBACK_DOUBLE_CLICK_INTERVAL)
    }
}

impl ClickCounter {
    pub const fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
        }
    }

    /// The count of a press of `button` at `position`, captured at `at` on [`capture_clock`].
    pub fn press(&mut self, button: MouseButton, position: Point, at: Duration) -> u8 {
        let next = match self.last {
            Some(last)
                if last.button == button
                    && at
                        .checked_sub(last.at)
                        .is_some_and(|gap| gap <= self.interval)
                    && within_slop(position, last.anchor) =>
            {
                Sequence {
                    at,
                    count: last.count.saturating_add(1),
                    ..last
                }
            }
            _ => Sequence {
                button,
                anchor: position,
                at,
                count: SINGLE_CLICK,
            },
        };
        self.last = Some(next);
        next.count
    }

    /// The next press starts a new sequence.
    pub fn reset(&mut self) {
        self.last = None;
    }
}

/// Where a receiver posts forwarded clicks, in its own display coordinates. A press continuing a
/// multi-click lands on its sequence's first landing, and until that button's release every move
/// on that display is shifted by the same offset, so a double-click-drag follows the hand 1:1.
#[derive(Clone, Debug)]
pub struct ClickLanding {
    displays: Vec<LogicalRect>,
    /// Where the source's last posted move put its cursor, before any shift.
    source: Option<Point>,
    first: [Option<Point>; MouseButton::ALL.len()],
    shift: Option<Shift>,
}

/// A snapped press still down. The source never learns of the snap, so its moves arrive unshifted.
#[derive(Clone, Copy, Debug)]
struct Shift {
    button: MouseButton,
    /// The display the snap landed on. The source leaving it drops the offset for good.
    display: usize,
    by: Point,
}

impl ClickLanding {
    pub fn new(displays: impl IntoIterator<Item = LogicalRect>) -> Self {
        Self {
            displays: displays.into_iter().collect(),
            source: None,
            first: [None; MouseButton::ALL.len()],
            shift: None,
        }
    }

    /// Where a move of the source's cursor to `source` is posted: shifted, but never off the
    /// snap's display, and unshifted once the source is on another one.
    pub fn move_target(&self, source: Point) -> Point {
        let Some(shift) = self.shift else {
            return source;
        };
        let display = self.displays[shift.display];
        if display.contains_half_open(source) {
            display.clamp_half_open(Point::new(source.x + shift.by.x, source.y + shift.by.y))
        } else {
            source
        }
    }

    /// Records a posted move of the source's cursor to `source`.
    pub fn moved(&mut self, source: Point) {
        self.source = Some(source);
        if let Some(shift) = &mut self.shift
            && !self.displays[shift.display].contains_half_open(source)
        {
            shift.by = Point::default();
        }
    }

    /// Where to move before posting this press, when it lands away from the cursor. One snapped
    /// button shifts moves at a time; a press under it lands where the cursor is.
    pub fn press_target(&self, button: MouseButton, click_count: u8) -> Option<Point> {
        self.snap(button, click_count).map(|(target, _)| target)
    }

    /// Records a posted press.
    pub fn pressed(&mut self, button: MouseButton, click_count: u8) {
        if let Some((_, shift)) = self.snap(button, click_count) {
            self.shift = Some(shift);
        } else if self.continued(button, click_count).is_none() {
            self.first[button.index()] = self.source.map(|source| self.move_target(source));
        }
    }

    /// Where to move after posting this button's release: back to the source's own point.
    pub fn release_target(&self, button: MouseButton) -> Option<Point> {
        self.shift.filter(|shift| shift.button == button)?;
        self.source
            .filter(|source| self.move_target(*source) != *source)
    }

    /// Records a posted release.
    pub fn released(&mut self, button: MouseButton) {
        if self.shift.is_some_and(|shift| shift.button == button) {
            self.shift = None;
        }
    }

    /// Forgets the cursor and every sequence, as after releasing all input.
    pub fn clear(&mut self) {
        self.source = None;
        self.first = [None; MouseButton::ALL.len()];
        self.shift = None;
    }

    /// The first landing a press continues: a multi-click from within the slop of it.
    fn continued(&self, button: MouseButton, click_count: u8) -> Option<Point> {
        let first = self.first[button.index()]?;
        (click_count > SINGLE_CLICK && within_slop(self.source?, first)).then_some(first)
    }

    /// Where a snapped press lands, as near its first landing as the source's display allows, and
    /// the shift it starts.
    fn snap(&self, button: MouseButton, click_count: u8) -> Option<(Point, Shift)> {
        if self.shift.is_some() {
            return None;
        }
        let source = self.source?;
        let first = self.continued(button, click_count)?;
        let display = self
            .displays
            .iter()
            .position(|display| display.contains_half_open(source))?;
        let target = self.displays[display].clamp_half_open(first);
        let by = Point::new(target.x - source.x, target.y - source.y);
        (target != source).then_some((
            target,
            Shift {
                button,
                display,
                by,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::LogicalSize;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    const LEFT: MouseButton = MouseButton::Left;
    const SLOP: f64 = DOUBLE_CLICK_SLOP;

    #[test]
    fn quick_presses_in_place_count_up() {
        let mut clicks = ClickCounter::default();
        let at = Point::new(40.0, 40.0);
        assert_eq!(clicks.press(LEFT, at, ms(0)), 1);
        assert_eq!(clicks.press(LEFT, at, ms(120)), 2);
        assert_eq!(clicks.press(LEFT, at, ms(240)), 3);
    }

    #[test]
    fn jitter_within_the_slop_is_still_a_multi_click() {
        let mut clicks = ClickCounter::default();
        clicks.press(LEFT, Point::new(100.0, 100.0), ms(0));
        assert_eq!(
            clicks.press(LEFT, Point::new(100.0 + SLOP, 100.0 - SLOP), ms(90)),
            2
        );
        assert_eq!(
            clicks.press(LEFT, Point::new(100.0 - SLOP, 100.0 + SLOP), ms(180)),
            3,
            "each press is measured from the first, where the peer received it"
        );
    }

    #[test]
    fn a_press_past_the_slop_or_far_away_starts_over() {
        for position in [
            Point::new(SLOP + 0.5, 0.0),
            Point::new(0.0, -SLOP - 0.5),
            Point::new(200.0, 0.0),
        ] {
            let mut clicks = ClickCounter::default();
            clicks.press(LEFT, Point::new(0.0, 0.0), ms(0));
            assert_eq!(clicks.press(LEFT, position, ms(50)), 1);
        }
        let mut creeping = ClickCounter::default();
        creeping.press(LEFT, Point::new(0.0, 0.0), ms(0));
        creeping.press(LEFT, Point::new(SLOP, 0.0), ms(50));
        assert_eq!(
            creeping.press(LEFT, Point::new(2.0 * SLOP, 0.0), ms(100)),
            1,
            "drift adds up against the first press"
        );
    }

    #[test]
    fn the_source_users_interval_decides_the_window() {
        let interval = ms(180);
        let at = Point::new(5.0, 5.0);
        let mut clicks = ClickCounter::new(interval);
        clicks.press(LEFT, at, ms(0));
        assert_eq!(clicks.press(LEFT, at, interval), 2);
        assert_eq!(
            clicks.press(LEFT, at, interval + interval + ms(1)),
            1,
            "just past the interval, though inside the 500 ms fallback"
        );
        let mut generous = ClickCounter::new(ms(900));
        generous.press(LEFT, at, ms(0));
        assert_eq!(generous.press(LEFT, at, ms(900)), 2);
    }

    #[test]
    fn another_button_a_reset_or_an_earlier_time_starts_over() {
        let at = Point::new(0.0, 0.0);
        let mut clicks = ClickCounter::default();
        clicks.press(LEFT, at, ms(0));
        assert_eq!(clicks.press(MouseButton::Right, at, ms(50)), 1);
        assert_eq!(
            clicks.press(LEFT, at, ms(100)),
            1,
            "the other button broke the sequence"
        );
        clicks.reset();
        assert_eq!(clicks.press(LEFT, at, ms(150)), 1);
        assert_eq!(clicks.press(LEFT, at, ms(100)), 1);
    }

    #[test]
    fn a_long_run_saturates() {
        let mut clicks = ClickCounter::default();
        let at = Point::new(0.0, 0.0);
        for _ in 0..300 {
            clicks.press(LEFT, at, ms(0));
        }
        assert_eq!(clicks.press(LEFT, at, ms(0)), u8::MAX);
    }

    #[test]
    fn the_capture_clock_only_moves_forward() {
        let earlier = capture_clock();
        assert!(capture_clock() >= earlier);
    }

    fn display(x: f64, y: f64, width: f64, height: f64) -> LogicalRect {
        LogicalRect {
            origin: Point::new(x, y),
            size: LogicalSize::new(width, height),
        }
    }

    /// Two 1920x1080 displays side by side.
    fn landing() -> ClickLanding {
        ClickLanding::new([
            display(0.0, 0.0, 1920.0, 1080.0),
            display(1920.0, 0.0, 1920.0, 1080.0),
        ])
    }

    /// Posts a move and a press as a receiver would, returning where the press landed.
    fn press_at(
        landing: &mut ClickLanding,
        source: Point,
        button: MouseButton,
        count: u8,
    ) -> Point {
        let cursor = landing.move_target(source);
        landing.moved(source);
        let landed = landing.press_target(button, count).unwrap_or(cursor);
        landing.pressed(button, count);
        landed
    }

    fn click_at(landing: &mut ClickLanding, source: Point, count: u8) -> Point {
        let landed = press_at(landing, source, LEFT, count);
        landing.released(LEFT);
        landed
    }

    #[test]
    fn a_multi_click_within_the_slop_lands_on_its_first_press() {
        let first = Point::new(100.0, 200.0);
        let mut clicks = landing();
        assert_eq!(click_at(&mut clicks, first, 1), first);
        let jittered = Point::new(100.0 + SLOP, 200.0 - SLOP);
        assert_eq!(click_at(&mut clicks, jittered, 2), first);
        assert_eq!(
            click_at(&mut clicks, Point::new(97.0, 204.0), 3),
            first,
            "a triple click keeps the first press's spot"
        );
    }

    #[test]
    fn a_single_another_button_or_a_press_past_the_slop_lands_where_the_source_put_it() {
        let first = Point::new(100.0, 200.0);
        let near = Point::new(104.0, 198.0);

        let mut single = landing();
        click_at(&mut single, first, 1);
        assert_eq!(click_at(&mut single, near, 1), near);

        let mut other = landing();
        click_at(&mut other, first, 1);
        assert_eq!(
            press_at(&mut other, near, MouseButton::Right, 2),
            near,
            "the right button has no first landing"
        );

        for far in [
            Point::new(100.0 + SLOP + 0.5, 200.0),
            Point::new(100.0, 200.0 - SLOP - 0.5),
        ] {
            let mut clicks = landing();
            click_at(&mut clicks, first, 1);
            assert_eq!(click_at(&mut clicks, far, 2), far);
            let between = Point::new((first.x + far.x) / 2.0, (first.y + far.y) / 2.0);
            assert_eq!(
                click_at(&mut clicks, between, 3),
                far,
                "the far press began a new sequence, measured from it"
            );
        }

        let mut unknown = ClickLanding::new([display(0.0, 0.0, 1920.0, 1080.0)]);
        unknown.pressed(LEFT, 1);
        unknown.released(LEFT);
        assert_eq!(
            click_at(&mut unknown, near, 2),
            near,
            "a press before any move anchors nothing"
        );
    }

    #[test]
    fn a_press_already_on_its_first_landing_or_unsnapped_moves_and_shifts_nothing() {
        let first = Point::new(100.0, 200.0);
        let mut in_place = landing();
        click_at(&mut in_place, first, 1);
        assert_eq!(in_place.press_target(LEFT, 2), None);
        in_place.pressed(LEFT, 2);
        assert_eq!(
            in_place.move_target(Point::new(102.0, 202.0)),
            Point::new(102.0, 202.0)
        );
        assert_eq!(in_place.release_target(LEFT), None);

        let mut single = landing();
        press_at(&mut single, first, LEFT, 1);
        assert_eq!(
            single.move_target(Point::new(102.0, 202.0)),
            Point::new(102.0, 202.0),
            "only a snap shifts"
        );
        assert_eq!(single.release_target(LEFT), None);
    }

    #[test]
    fn moves_under_a_snapped_press_follow_the_hand_one_to_one() {
        let mut clicks = landing();
        click_at(&mut clicks, Point::new(100.0, 200.0), 1);
        assert_eq!(
            press_at(&mut clicks, Point::new(106.0, 195.0), LEFT, 2),
            Point::new(100.0, 200.0)
        );
        assert_eq!(
            clicks.move_target(Point::new(109.0, 195.0)),
            Point::new(103.0, 200.0),
            "a 3-unit move moves the cursor 3 units, not 0 and not a jump"
        );
        clicks.moved(Point::new(109.0, 195.0));
        assert_eq!(clicks.release_target(MouseButton::Right), None);
        assert_eq!(
            clicks.release_target(LEFT),
            Some(Point::new(109.0, 195.0)),
            "released where the cursor is, then back to the source's own point"
        );
        clicks.released(LEFT);
        assert_eq!(clicks.release_target(LEFT), None);
        assert_eq!(
            clicks.move_target(Point::new(110.0, 195.0)),
            Point::new(110.0, 195.0)
        );
    }

    #[test]
    fn one_snapped_button_shifts_at_a_time() {
        let mut clicks = landing();
        click_at(&mut clicks, Point::new(100.0, 200.0), 1);
        press_at(&mut clicks, Point::new(103.0, 200.0), MouseButton::Right, 1);
        clicks.released(MouseButton::Right);
        press_at(&mut clicks, Point::new(104.0, 200.0), LEFT, 2);
        assert_eq!(
            press_at(&mut clicks, Point::new(105.0, 200.0), MouseButton::Right, 2),
            Point::new(101.0, 200.0),
            "lands where the left snap's shift put the cursor"
        );
        clicks.released(MouseButton::Right);
        assert_eq!(
            clicks.move_target(Point::new(106.0, 200.0)),
            Point::new(102.0, 200.0),
            "the right release left the left snap's shift in place"
        );
        assert_eq!(clicks.release_target(LEFT), Some(Point::new(105.0, 200.0)));
    }

    #[test]
    fn a_shift_toward_a_far_edge_stops_there_and_resumes_1_to_1_across_it() {
        let mut edge = landing();
        click_at(&mut edge, Point::new(1915.0, 500.0), 1);
        press_at(&mut edge, Point::new(1911.0, 500.0), LEFT, 2);
        let pinned = edge.move_target(Point::new(1917.0, 500.0));
        assert_eq!(pinned.x, 1920.0_f64.next_down(), "the far edge is excluded");
        edge.moved(Point::new(1917.0, 500.0));
        for x in [1920.0, 1925.0] {
            assert_eq!(
                edge.move_target(Point::new(x, 500.0)),
                Point::new(x, 500.0),
                "on the next display the source's own point, with no jump"
            );
            edge.moved(Point::new(x, 500.0));
        }
        assert_eq!(edge.release_target(LEFT), None);
    }

    #[test]
    fn a_shift_away_from_a_seam_drops_there_with_one_jump_and_no_dead_zone() {
        let mut hop = landing();
        click_at(&mut hop, Point::new(1912.0, 500.0), 1);
        press_at(&mut hop, Point::new(1919.0, 500.0), LEFT, 2);
        assert_eq!(
            hop.move_target(Point::new(1919.5, 500.0)),
            Point::new(1912.5, 500.0)
        );
        for x in [1921.0, 1923.0, 1915.0] {
            assert_eq!(
                hop.move_target(Point::new(x, 500.0)),
                Point::new(x, 500.0),
                "the source left the snap's display, so the offset is gone for good"
            );
            hop.moved(Point::new(x, 500.0));
        }
        assert_eq!(hop.release_target(LEFT), None, "nothing to move back from");
        assert_eq!(
            press_at(&mut hop, Point::new(1916.0, 500.0), MouseButton::Right, 1),
            Point::new(1916.0, 500.0),
            "the dropped shift still holds the one snap until its release"
        );
    }

    #[test]
    fn a_snap_across_a_seam_lands_on_the_sources_display() {
        let mut seam = landing();
        click_at(&mut seam, Point::new(1918.0, 500.0), 1);
        assert_eq!(
            click_at(&mut seam, Point::new(1921.0, 500.0), 2),
            Point::new(1920.0, 500.0),
            "as near the first press as the source's display allows"
        );
    }

    #[test]
    fn clearing_forgets_every_sequence_and_shift() {
        let mut clicks = landing();
        click_at(&mut clicks, Point::new(100.0, 200.0), 1);
        press_at(&mut clicks, Point::new(104.0, 200.0), LEFT, 2);
        clicks.clear();
        assert_eq!(
            clicks.move_target(Point::new(104.0, 200.0)),
            Point::new(104.0, 200.0)
        );
        assert_eq!(clicks.release_target(LEFT), None);
        assert_eq!(
            click_at(&mut clicks, Point::new(104.0, 200.0), 2),
            Point::new(104.0, 200.0)
        );
    }
}
