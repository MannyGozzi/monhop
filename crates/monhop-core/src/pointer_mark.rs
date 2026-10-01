//! What MonHop draws at the pointer of a computer it controls: the autoscroll origin, and the chevron
//! that blooms beside the pointer when a swipe from the other computer turns the page. Lengths are
//! points on macOS and device-independent pixels on Windows.

use std::time::Duration;

use crate::{pointer::SystemGesture, types::Point};

/// A gray with its opacity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gray {
    pub white: f64,
    pub alpha: f64,
}

/// Light ink over a translucent dark shade reads on light and dark content alike.
pub const MARK_INK: Gray = Gray {
    white: 1.0,
    alpha: 0.9,
};
pub const MARK_SHADE: Gray = Gray {
    white: 0.0,
    alpha: 0.35,
};

/// Which way a swipe turned the page; the chevron points and travels that way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageDirection {
    Back,
    Forward,
}

impl PageDirection {
    pub fn of(gesture: SystemGesture) -> Option<Self> {
        match gesture {
            SystemGesture::NavigateBack => Some(Self::Back),
            SystemGesture::NavigateForward => Some(Self::Forward),
            _ => None,
        }
    }

    /// Left for Back, right for Forward.
    pub fn sign(self) -> f64 {
        match self {
            Self::Back => -1.0,
            Self::Forward => 1.0,
        }
    }
}

/// A page turn just sent to this computer, with the pointer where it happened: top-left global points
/// on macOS, physical virtual-desktop pixels on Windows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageTurn {
    pub direction: PageDirection,
    pub at: Point,
}

/// One moment of the bloom.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BloomFrame {
    pub opacity: f64,
    /// From the pointer to the chevron's center, negative for Back.
    pub offset: f64,
    pub scale: f64,
    /// Gaussian standard deviation.
    pub blur: f64,
}

pub const BLOOM_DURATION: Duration = Duration::from_millis(640);
/// With reduced motion the sharpest frame shows this long, unmoving, then goes.
pub const BLOOM_STILL: Duration = Duration::from_millis(420);

/// Keyframes at shares of the run, for Forward: blurred and small, sharp, then drifting away.
pub const BLOOM_KEYS: [(f64, BloomFrame); 3] = [
    (
        0.0,
        BloomFrame {
            opacity: 0.0,
            offset: 22.0,
            scale: 0.6,
            blur: 8.0,
        },
    ),
    (
        0.35,
        BloomFrame {
            opacity: 1.0,
            offset: 30.0,
            scale: 1.0,
            blur: 0.0,
        },
    ),
    (
        1.0,
        BloomFrame {
            opacity: 0.0,
            offset: 52.0,
            scale: 0.92,
            blur: 5.0,
        },
    ),
];
/// The whole run's cubic-bezier easing; keyframes interpolate linearly within it.
pub const BLOOM_EASING: [f64; 4] = [0.22, 1.0, 0.36, 1.0];

/// The Back chevron's corners around its center, y down; Forward mirrors x. Round caps and joins.
pub const CHEVRON_POINTS: [(f64, f64); 3] = [(2.5, -6.25), (-3.75, 0.0), (2.5, 6.25)];
pub const CHEVRON_STROKE: f64 = 3.0;
/// The shade runs this much wider than the ink on each side, softened by this extra deviation.
pub const CHEVRON_SHADE_SPREAD: f64 = 1.5;
pub const CHEVRON_SHADE_SOFTNESS: f64 = 1.5;

/// The bloom `elapsed` into its run, or None once it is over.
pub fn bloom_frame(direction: PageDirection, elapsed: Duration) -> Option<BloomFrame> {
    if elapsed >= BLOOM_DURATION {
        return None;
    }
    let progress = ease(
        BLOOM_EASING,
        elapsed.as_secs_f64() / BLOOM_DURATION.as_secs_f64(),
    );
    let at = BLOOM_KEYS
        .windows(2)
        .position(|pair| progress <= pair[1].0)
        .unwrap_or(BLOOM_KEYS.len() - 2);
    let ((from_at, from), (to_at, to)) = (BLOOM_KEYS[at], BLOOM_KEYS[at + 1]);
    let share = (progress - from_at) / (to_at - from_at);
    let mix = |a: f64, b: f64| a + (b - a) * share;
    Some(BloomFrame {
        opacity: mix(from.opacity, to.opacity),
        offset: mix(from.offset, to.offset) * direction.sign(),
        scale: mix(from.scale, to.scale),
        blur: mix(from.blur, to.blur),
    })
}

/// The sharpest frame, shown alone under reduced motion.
pub fn still_frame(direction: PageDirection) -> BloomFrame {
    let sharpest = BLOOM_KEYS[1].1;
    BloomFrame {
        offset: sharpest.offset * direction.sign(),
        ..sharpest
    }
}

/// How far any visible ink reaches from the pointer over the whole bloom, shade and blur included:
/// x along the travel, y above or below. A renderer's surface this size never clips the chevron.
pub fn bloom_reach() -> Point {
    let (half_width, half_height) = CHEVRON_POINTS
        .iter()
        .fold((0.0_f64, 0.0_f64), |(w, h), &(x, y)| {
            (w.max(x.abs()), h.max(y.abs()))
        });
    let edge = CHEVRON_STROKE / 2.0 + CHEVRON_SHADE_SPREAD;
    // Three deviations hold all but a sliver of a Gaussian.
    let spread = |frame: &BloomFrame| 3.0 * frame.blur.hypot(CHEVRON_SHADE_SOFTNESS);
    BLOOM_KEYS
        .iter()
        .fold(Point::new(0.0, 0.0), |reach, (_, frame)| {
            Point::new(
                reach
                    .x
                    .max(frame.offset + frame.scale * (half_width + edge) + spread(frame)),
                reach
                    .y
                    .max(frame.scale * (half_height + edge) + spread(frame)),
            )
        })
}

/// CSS `cubic-bezier` easing of progress `t`.
pub fn ease([x1, y1, x2, y2]: [f64; 4], t: f64) -> f64 {
    if t <= 0.0 || t >= 1.0 {
        return t.clamp(0.0, 1.0);
    }
    let curve = |a: f64, b: f64, s: f64| {
        let rest = 1.0 - s;
        3.0 * a * s * rest * rest + 3.0 * b * s * s * rest + s * s * s
    };
    // x runs monotonically from 0 to 1 when x1 and x2 lie in 0..=1, so bisection finds its parameter.
    let (mut low, mut high) = (0.0, 1.0);
    for _ in 0..48 {
        let mid = (low + high) / 2.0;
        if curve(x1, x2, mid) < t {
            low = mid;
        } else {
            high = mid;
        }
    }
    curve(y1, y2, (low + high) / 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINEAR: [f64; 4] = [0.0, 0.0, 1.0, 1.0];

    #[test]
    fn easing_matches_css_curves() {
        for t in [0.0, 0.1, 0.5, 0.9, 1.0] {
            assert!((ease(LINEAR, t) - t).abs() < 1e-9);
        }
        assert_eq!(ease(BLOOM_EASING, -1.0), 0.0);
        assert_eq!(ease(BLOOM_EASING, 2.0), 1.0);
        // An ease-out: well past halfway at the halfway mark, and never backwards.
        assert!(ease(BLOOM_EASING, 0.5) > 0.85);
        let mut last = 0.0;
        for step in 0..=100 {
            let next = ease(BLOOM_EASING, f64::from(step) / 100.0);
            assert!(next >= last && next <= 1.0);
            last = next;
        }
    }

    #[test]
    fn the_bloom_starts_hidden_peaks_sharp_and_ends() {
        let start = bloom_frame(PageDirection::Forward, Duration::ZERO).unwrap();
        assert_eq!(start, BLOOM_KEYS[0].1);
        let peak = (0..BLOOM_DURATION.as_millis() as u64)
            .filter_map(|ms| bloom_frame(PageDirection::Forward, Duration::from_millis(ms)))
            .max_by(|a, b| a.opacity.total_cmp(&b.opacity))
            .unwrap();
        assert!(peak.opacity > 0.99 && peak.blur < 0.1);
        assert!((peak.offset - 30.0).abs() < 0.5);
        let late = bloom_frame(
            PageDirection::Forward,
            BLOOM_DURATION - Duration::from_millis(1),
        );
        assert!(late.unwrap().opacity < 0.01);
        assert_eq!(bloom_frame(PageDirection::Forward, BLOOM_DURATION), None);
    }

    #[test]
    fn the_chevron_only_ever_travels_away_from_the_pointer() {
        let mut last = 0.0;
        for ms in 0..BLOOM_DURATION.as_millis() as u64 {
            let back = bloom_frame(PageDirection::Back, Duration::from_millis(ms)).unwrap();
            let forward = bloom_frame(PageDirection::Forward, Duration::from_millis(ms)).unwrap();
            assert_eq!(back.offset, -forward.offset);
            assert!(forward.offset >= last && forward.offset > 0.0);
            last = forward.offset;
        }
        assert_eq!(still_frame(PageDirection::Back).offset, -30.0);
        assert_eq!(still_frame(PageDirection::Back).opacity, 1.0);
    }

    #[test]
    fn the_reach_covers_every_frame() {
        let reach = bloom_reach();
        let half_height = CHEVRON_POINTS[0].1.abs();
        for ms in 0..BLOOM_DURATION.as_millis() as u64 {
            let frame = bloom_frame(PageDirection::Forward, Duration::from_millis(ms)).unwrap();
            let ink = CHEVRON_STROKE / 2.0 + CHEVRON_SHADE_SPREAD;
            assert!(frame.offset + frame.scale * (CHEVRON_POINTS[1].0.abs() + ink) < reach.x);
            assert!(frame.scale * (half_height + ink) + 3.0 * frame.blur <= reach.y + 1e-9);
        }
        assert!(reach.x < 90.0 && reach.y < 50.0);
    }

    #[test]
    fn only_navigation_turns_the_page() {
        assert_eq!(
            PageDirection::of(SystemGesture::NavigateBack),
            Some(PageDirection::Back)
        );
        assert_eq!(
            PageDirection::of(SystemGesture::NavigateForward),
            Some(PageDirection::Forward)
        );
        assert_eq!(PageDirection::of(SystemGesture::Overview), None);
    }
}
