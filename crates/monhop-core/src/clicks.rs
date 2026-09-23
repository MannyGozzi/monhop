//! Multi-click numbering for forwarded presses. The source decides each count from the peer
//! cursor it tracks, never from either OS's view of a pointer pinned while control is remote.

use std::time::Duration;

use crate::types::{MouseButton, Point};

/// The click count of a press that does not continue a multi-click.
pub const SINGLE_CLICK: u8 = 1;

/// Used when the source user's double-click interval cannot be read; both OSes ship with it.
pub const FALLBACK_DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);

/// Peer px/pt, on each axis, a multi-click press may land from its first press: looser than
/// Windows' own ±2 px so hand jitter counts, under half a 20 px list row so a neighbour never does.
pub const DOUBLE_CLICK_SLOP: i32 = 8;

#[derive(Clone, Copy, Debug)]
struct Sequence {
    button: MouseButton,
    /// The first press's position: a Windows peer lands every later press of the sequence there.
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

    /// The count of a press of `button` at `position` and time `at`.
    pub fn press(&mut self, button: MouseButton, position: Point, at: Duration) -> u8 {
        let slop = f64::from(DOUBLE_CLICK_SLOP);
        let next = match self.last {
            Some(last)
                if last.button == button
                    && at
                        .checked_sub(last.at)
                        .is_some_and(|gap| gap <= self.interval)
                    && (position.x - last.anchor.x).abs() <= slop
                    && (position.y - last.anchor.y).abs() <= slop =>
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    const LEFT: MouseButton = MouseButton::Left;

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
        let slop = f64::from(DOUBLE_CLICK_SLOP);
        let mut clicks = ClickCounter::default();
        clicks.press(LEFT, Point::new(100.0, 100.0), ms(0));
        assert_eq!(
            clicks.press(LEFT, Point::new(100.0 + slop, 100.0 - slop), ms(90)),
            2
        );
        assert_eq!(
            clicks.press(LEFT, Point::new(100.0 - slop, 100.0 + slop), ms(180)),
            3,
            "each press is measured from the first, where the peer received it"
        );
    }

    #[test]
    fn a_press_past_the_slop_or_far_away_starts_over() {
        let slop = f64::from(DOUBLE_CLICK_SLOP);
        for position in [
            Point::new(slop + 0.5, 0.0),
            Point::new(0.0, -slop - 0.5),
            Point::new(200.0, 0.0),
        ] {
            let mut clicks = ClickCounter::default();
            clicks.press(LEFT, Point::new(0.0, 0.0), ms(0));
            assert_eq!(clicks.press(LEFT, position, ms(50)), 1);
        }
        let mut creeping = ClickCounter::default();
        creeping.press(LEFT, Point::new(0.0, 0.0), ms(0));
        creeping.press(LEFT, Point::new(slop, 0.0), ms(50));
        assert_eq!(
            creeping.press(LEFT, Point::new(2.0 * slop, 0.0), ms(100)),
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
}
