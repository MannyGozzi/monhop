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

/// How the live pointer's marks look and move: the dimmed copy of an idle cursor, and the ring
/// where control lands. The app reads every value from its stylesheet.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MarkStyle {
    /// The copy's opacity once dimmed; it starts fully opaque.
    pub idle_opacity: f64,
    pub idle_fade: Duration,
    /// The `cubic-bezier` both the fade and the ring follow.
    pub ease: [f64; 4],
    pub ring_diameter: f64,
    pub ring_stroke: f64,
    /// Gamma-encoded sRGB, each 0..=1.
    pub ring_rgb: [f64; 3],
    /// The fill's opacity as a share of the stroke's.
    pub ring_fill: f64,
    /// From and to.
    pub ring_scale: (f64, f64),
    pub ring_opacity: (f64, f64),
    pub ring_duration: Duration,
}

/// One moment of the arrival ring, around its center.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RingFrame {
    pub scale: f64,
    pub opacity: f64,
}

/// The ring `elapsed` into its run, or None once it is over.
pub fn ring_frame(style: &MarkStyle, elapsed: Duration) -> Option<RingFrame> {
    if elapsed >= style.ring_duration {
        return None;
    }
    let progress = ease(
        style.ease,
        elapsed.as_secs_f64() / style.ring_duration.as_secs_f64(),
    );
    let mix = |(from, to): (f64, f64)| from + (to - from) * progress;
    Some(RingFrame {
        scale: mix(style.ring_scale),
        opacity: mix(style.ring_opacity),
    })
}

/// The copy's opacity `elapsed` after it went up: fully opaque, easing to `idle_opacity`, which
/// `still` (reduced motion) shows at once.
pub fn idle_alpha(style: &MarkStyle, elapsed: Duration, still: bool) -> f64 {
    if still || elapsed >= style.idle_fade {
        return style.idle_opacity;
    }
    let progress = ease(
        style.ease,
        elapsed.as_secs_f64() / style.idle_fade.as_secs_f64(),
    );
    1.0 + (style.idle_opacity - 1.0) * progress
}

/// Half the side of a square around the ring's center that no frame's stroke ever leaves, whether
/// the renderer scales the stroke with the ring or not.
pub fn ring_reach(style: &MarkStyle) -> f64 {
    let largest = style.ring_scale.0.max(style.ring_scale.1).max(1.0);
    largest * (style.ring_diameter + style.ring_stroke) / 2.0
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

    /// The design's numbers; the app's stylesheet test pins the same ones.
    const STYLE: MarkStyle = MarkStyle {
        idle_opacity: 0.3,
        idle_fade: Duration::from_millis(220),
        ease: [0.22, 1.0, 0.36, 1.0],
        ring_diameter: 44.0,
        ring_stroke: 2.0,
        ring_rgb: [1.0, 0.4, 0.6],
        ring_fill: 0.16,
        ring_scale: (0.35, 1.6),
        ring_opacity: (0.95, 0.0),
        ring_duration: Duration::from_millis(720),
    };

    #[test]
    fn the_ring_grows_and_fades_once_then_ends() {
        assert_eq!(
            ring_frame(&STYLE, Duration::ZERO),
            Some(RingFrame {
                scale: 0.35,
                opacity: 0.95
            })
        );
        let late = ring_frame(&STYLE, Duration::from_millis(719)).unwrap();
        assert!((late.scale - 1.6).abs() < 1e-3 && late.opacity < 1e-3);
        assert_eq!(ring_frame(&STYLE, Duration::from_millis(720)), None);
        let mut last = ring_frame(&STYLE, Duration::ZERO).unwrap();
        for ms in 1..720 {
            let frame = ring_frame(&STYLE, Duration::from_millis(ms)).unwrap();
            assert!(frame.scale >= last.scale && frame.opacity <= last.opacity);
            last = frame;
        }
        // An ease-out: most of the growth is done by the halfway mark.
        let half = ring_frame(&STYLE, Duration::from_millis(360)).unwrap();
        assert!(half.scale > 0.35 + 0.8 * (1.6 - 0.35));
    }

    #[test]
    fn the_copy_dims_from_opaque_over_the_fade_and_at_once_when_still() {
        assert_eq!(idle_alpha(&STYLE, Duration::ZERO, false), 1.0);
        assert_eq!(idle_alpha(&STYLE, Duration::ZERO, true), 0.3);
        assert_eq!(idle_alpha(&STYLE, Duration::from_millis(220), false), 0.3);
        assert_eq!(idle_alpha(&STYLE, Duration::from_secs(5), false), 0.3);
        let mut last = 1.0;
        for ms in 1..220 {
            let alpha = idle_alpha(&STYLE, Duration::from_millis(ms), false);
            assert!(alpha <= last && alpha >= 0.3);
            last = alpha;
        }
        let instant = MarkStyle {
            idle_fade: Duration::ZERO,
            ring_duration: Duration::ZERO,
            ..STYLE
        };
        assert_eq!(idle_alpha(&instant, Duration::ZERO, false), 0.3);
        assert_eq!(ring_frame(&instant, Duration::ZERO), None);
    }

    #[test]
    fn the_ring_reach_covers_every_frame_and_its_stroke() {
        let reach = ring_reach(&STYLE);
        for ms in 0..720 {
            let frame = ring_frame(&STYLE, Duration::from_millis(ms)).unwrap();
            let scaled = frame.scale * (STYLE.ring_diameter + STYLE.ring_stroke) / 2.0;
            let unscaled = frame.scale * STYLE.ring_diameter / 2.0 + STYLE.ring_stroke / 2.0;
            assert!(scaled.max(unscaled) <= reach + 1e-9);
        }
        assert!((reach - 1.6 * 23.0).abs() < 1e-9);
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
