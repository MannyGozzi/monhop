//! Where the island sits and the outline it draws, from what a display reports. Plain points in
//! AppKit's global space (y up); no AppKit here, so every placement and outline is unit-tested.

/// The outline of a hanging island borrows the notch's proportions.
const SHOULDER_MAX: f64 = 14.0;
const SHOULDER_SHARE: f64 = 0.19;
const CORNER_MAX: f64 = 28.0;
const CORNER_SHARE: f64 = 0.34;
/// A simulated island floats this far inside the top and bottom of the menu bar.
const FLOAT_GAP: f64 = 2.0;
/// The simulated capsule is as wide, per point of bar height, as Apple's notch is.
const CAPSULE_WIDTH_PER_BAR: f64 = 180.0 / 32.0;
/// A menu bar outside this range is a misreading; the status bar's own thickness, then 24 pt, stand in.
const BAR_RANGE: std::ops::RangeInclusive<f64> = 16.0..=64.0;
const BAR_FALLBACK: f64 = 24.0;
/// The circle-to-cubic handle ratio.
const KAPPA: f64 = 0.552_284_75;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

impl Size {
    pub const fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }

    pub fn area(self) -> f64 {
        self.width * self.height
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn max_x(self) -> f64 {
        self.x + self.width
    }

    pub fn max_y(self) -> f64 {
        self.y + self.height
    }

    pub fn mid_x(self) -> f64 {
        self.x + self.width / 2.0
    }
}

/// What one display reports, in global points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisplayInfo {
    pub frame: Rect,
    pub visible: Rect,
    /// `safeAreaInsets.top`; zero without a camera housing.
    pub safe_top: f64,
    /// The menu-bar areas left and right of the camera, when the display has one.
    pub aux_left: Option<Rect>,
    pub aux_right: Option<Rect>,
    pub scale: f64,
    /// `NSStatusBar.system.thickness`, for a menu bar that is hidden now.
    pub status_bar: f64,
}

impl DisplayInfo {
    /// The camera housing's width, when this display has one.
    pub fn camera_gap(&self) -> Option<(f64, f64)> {
        let (left, right) = (self.aux_left?, self.aux_right?);
        let gap = right.x - left.max_x();
        (self.safe_top > 0.0 && gap > 0.0).then(|| (left.max_x(), gap))
    }

    pub fn notched(&self) -> bool {
        self.camera_gap().is_some()
    }
}

/// Where the island hangs and how big it is at rest.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub notched: bool,
    /// The camera's (or capsule's) center, on the half-pixel grid so symmetric edges land on pixels.
    pub center_x: f64,
    /// The top edge of the display.
    pub top: f64,
    /// The notch's height, or the menu bar's.
    pub band: f64,
    /// The width between the wings: the camera, or the capsule without its round ends.
    pub core: f64,
    pub idle: Size,
    /// How far below the top edge the outline starts.
    pub float: f64,
    pub scale: f64,
}

pub fn placement(display: &DisplayInfo) -> Placement {
    let scale = if display.scale.is_finite() && display.scale >= 1.0 {
        display.scale
    } else {
        1.0
    };
    let half_grid = |value: f64| (value * scale * 2.0).round() / (scale * 2.0);
    let top = display.frame.max_y();
    if let Some((left_edge, gap)) = display.camera_gap() {
        let center_x = half_grid(left_edge + gap / 2.0);
        let band = ceil_to_pixel(display.safe_top, scale);
        let mut placement = Placement {
            notched: true,
            center_x,
            top,
            band,
            core: gap,
            idle: Size::new(gap, band),
            float: 0.0,
            scale,
        };
        placement.core = placement.fit(gap);
        placement.idle.width = placement.core;
        return placement;
    }
    let shown = top - display.visible.max_y();
    let bar = if BAR_RANGE.contains(&shown) {
        shown
    } else if BAR_RANGE.contains(&display.status_bar) {
        display.status_bar
    } else {
        BAR_FALLBACK
    };
    let height = bar - FLOAT_GAP * 2.0;
    let mut placement = Placement {
        notched: false,
        center_x: half_grid(display.frame.mid_x()),
        top,
        band: bar,
        core: 0.0,
        idle: Size::new(0.0, height),
        float: FLOAT_GAP,
        scale,
    };
    placement.idle.width = placement.fit((CAPSULE_WIDTH_PER_BAR * bar).round());
    placement.core = placement.idle.width - height;
    placement
}

/// How far a hanging outline `height` tall curves in at the top before its sides drop.
pub fn shoulder(height: f64) -> f64 {
    SHOULDER_MAX.min(height.max(0.0) * SHOULDER_SHARE)
}

fn ceil_to_pixel(value: f64, scale: f64) -> f64 {
    (value * scale - 1e-9).ceil() / scale
}

fn floor_to_pixel(value: f64, scale: f64) -> f64 {
    (value * scale + 1e-9).floor() / scale
}

impl Placement {
    /// The narrowest width of at least `width` whose edges, centered here, land on pixels.
    pub fn fit(&self, width: f64) -> f64 {
        let left = floor_to_pixel(self.center_x - width / 2.0, self.scale);
        2.0 * (self.center_x - left)
    }

    /// A compact activity: the rest size plus a wing on each side.
    pub fn compact(&self, wing: f64) -> Size {
        Size::new(self.fit(self.core + 2.0 * wing), self.idle.height)
    }

    pub fn open(&self, width: f64, height: f64) -> Size {
        Size::new(self.fit(width), ceil_to_pixel(height, self.scale))
    }

    /// The outline at `size`; `open` rounds a floating panel like the hanging one.
    pub fn outline(&self, size: Size, open: bool) -> Outline {
        if self.notched {
            Outline::hanging(size)
        } else {
            Outline::floating(size, self.float, open)
        }
    }

    /// The window around motion from `from` to `to`: both sizes plus `overshoot` of the travel,
    /// with even whole-point margins so it stays centered and pixel-aligned.
    pub fn envelope(&self, from: Size, to: Size, overshoot: f64) -> Size {
        let swing = |a: f64, b: f64| a.max(b) + overshoot * (b - a).abs();
        let width = swing(from.width, to.width);
        let height = swing(from.height, to.height) + self.float;
        let margin = ((width - to.width.max(from.width)) / 2.0).ceil() + 1.0;
        Size::new(
            self.fit(to.width.max(from.width)) + 2.0 * margin,
            (height + 1.0).ceil(),
        )
    }

    /// The window frame for `envelope`, hanging from the top edge, centered on the island.
    pub fn frame(&self, envelope: Size) -> Rect {
        Rect::new(
            self.center_x - envelope.width / 2.0,
            self.top - envelope.height,
            envelope.width,
            envelope.height,
        )
    }
}

/// Which shape an outline is; the two never interpolate into each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Silhouette {
    Hanging,
    Floating,
}

/// Every outline is the same 17 points: a move, then four line-and-curve pairs, closed. With one
/// element sequence for every state, Core Animation interpolates any two outlines point by point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Outline {
    pub silhouette: Silhouette,
    /// Relative to the island's top center, y up (the outline lies at y <= 0).
    pub points: [Point; 17],
}

impl Outline {
    /// Flat on the top edge with concave shoulders, as the notch hangs from the bezel.
    pub fn hanging(size: Size) -> Self {
        let (w, h) = (size.width.max(0.0), size.height.max(0.0));
        let half = w / 2.0;
        let s = shoulder(h).min(half / 2.0);
        let b = CORNER_MAX
            .min(h * CORNER_SHARE)
            .min(h / 2.0)
            .min((w - 2.0 * s).max(0.0) / 2.0);
        let (sk, bk) = (s * KAPPA, b * (1.0 - KAPPA));
        let p = |x: f64, y: f64| Point { x, y };
        Self {
            silhouette: Silhouette::Hanging,
            points: [
                p(-half, 0.0),
                p(half, 0.0),
                p(half - sk, 0.0),
                p(half - s, -s * (1.0 - KAPPA)),
                p(half - s, -s),
                p(half - s, -(h - b)),
                p(half - s, -(h - bk)),
                p(half - s - bk, -h),
                p(half - s - b, -h),
                p(-half + s + b, -h),
                p(-half + s + bk, -h),
                p(-half + s, -(h - bk)),
                p(-half + s, -(h - b)),
                p(-half + s, -s),
                p(-half + s, -s * (1.0 - KAPPA)),
                p(-half + sk, 0.0),
                p(-half, 0.0),
            ],
        }
    }

    /// A capsule (or, open, a panel with the hanging corners) floating `float` below the top edge.
    pub fn floating(size: Size, float: f64, open: bool) -> Self {
        let (w, h) = (size.width.max(0.0), size.height.max(0.0));
        let half = w / 2.0;
        let radius = if open {
            CORNER_MAX.min(h * CORNER_SHARE)
        } else {
            (h / 2.0).min(CORNER_MAX)
        }
        .min(h / 2.0)
        .min(half);
        let (top, bottom) = (-float, -float - h);
        let (r, rk) = (radius, radius * (1.0 - KAPPA));
        let p = |x: f64, y: f64| Point { x, y };
        Self {
            silhouette: Silhouette::Floating,
            points: [
                p(-half + r, top),
                p(half - r, top),
                p(half - rk, top),
                p(half, top - rk),
                p(half, top - r),
                p(half, bottom + r),
                p(half, bottom + rk),
                p(half - rk, bottom),
                p(half - r, bottom),
                p(-half + r, bottom),
                p(-half + rk, bottom),
                p(-half, bottom + rk),
                p(-half, bottom + r),
                p(-half, top - r),
                p(-half, top - rk),
                p(-half + rk, top),
                p(-half + r, top),
            ],
        }
    }

    /// The smallest rectangle around every point, relative to the top center.
    pub fn bounds(&self) -> Rect {
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        for point in &self.points {
            min_x = min_x.min(point.x);
            max_x = max_x.max(point.x);
            min_y = min_y.min(point.y);
            max_y = max_y.max(point.y);
        }
        Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
    }

    /// Point by point: progress 0 is `self`, 1 is `to`, and beyond 1 extrapolates as a spring does.
    #[cfg(test)]
    pub fn lerp(&self, to: &Outline, progress: f64) -> Outline {
        let mut points = self.points;
        for (point, target) in points.iter_mut().zip(to.points.iter()) {
            point.x += (target.x - point.x) * progress;
            point.y += (target.y - point.y) * progress;
        }
        Outline {
            silhouette: to.silhouette,
            points,
        }
    }

    /// The farthest any point moves between the two outlines.
    pub fn travel(&self, to: &Outline) -> f64 {
        self.points
            .iter()
            .zip(to.points.iter())
            .map(|(a, b)| (b.x - a.x).hypot(b.y - a.y))
            .fold(0.0, f64::max)
    }

    /// `(to - from) · (target - self)` over `|target - self|²`: a motion from `from` to `to`
    /// measured along the way from here to `target`, so a retargeted spring keeps the speed it
    /// had in the new direction.
    pub fn share_along(&self, from: &Outline, to: &Outline, target: &Outline) -> f64 {
        let (mut dot, mut norm) = (0.0, 0.0);
        let pairs = from.points.iter().zip(to.points.iter());
        for ((now, goal), (start, end)) in self.points.iter().zip(target.points.iter()).zip(pairs) {
            let (motion_x, motion_y) = (end.x - start.x, end.y - start.y);
            let (left_x, left_y) = (goal.x - now.x, goal.y - now.y);
            dot += motion_x * left_x + motion_y * left_y;
            norm += left_x * left_x + left_y * left_y;
        }
        if norm < 1e-9 { 0.0 } else { dot / norm }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 16-inch MacBook Pro: 1728 x 1117 at 2x, a 200 pt camera housing 32 pt tall.
    fn notched() -> DisplayInfo {
        DisplayInfo {
            frame: Rect::new(0.0, 0.0, 1728.0, 1117.0),
            visible: Rect::new(0.0, 0.0, 1728.0, 1080.0),
            safe_top: 32.0,
            aux_left: Some(Rect::new(0.0, 1080.0, 764.0, 37.0)),
            aux_right: Some(Rect::new(964.0, 1080.0, 764.0, 37.0)),
            scale: 2.0,
            status_bar: 24.0,
        }
    }

    /// A 2560 x 1440 display at 1x with a 24 pt menu bar, right of the main one.
    fn external() -> DisplayInfo {
        DisplayInfo {
            frame: Rect::new(1728.0, 0.0, 2560.0, 1440.0),
            visible: Rect::new(1728.0, 0.0, 2560.0, 1416.0),
            safe_top: 0.0,
            aux_left: None,
            aux_right: None,
            scale: 1.0,
            status_bar: 24.0,
        }
    }

    fn on_pixels(value: f64, scale: f64) -> bool {
        ((value * scale) - (value * scale).round()).abs() < 1e-9
    }

    #[test]
    fn a_notched_display_hangs_the_island_around_its_camera() {
        let place = placement(&notched());
        assert!(place.notched);
        assert_eq!(place.center_x, 864.0);
        assert_eq!(place.top, 1117.0);
        assert_eq!(place.idle, Size::new(200.0, 32.0));
        assert_eq!(place.float, 0.0);
        assert_eq!(place.compact(60.0), Size::new(320.0, 32.0));
        let outline = place.outline(place.idle, false);
        assert_eq!(outline.silhouette, Silhouette::Hanging);
        assert_eq!(outline.bounds(), Rect::new(-100.0, -32.0, 200.0, 32.0));
    }

    #[test]
    fn a_display_without_a_camera_floats_a_capsule_in_the_menu_bar() {
        let place = placement(&external());
        assert!(!place.notched);
        assert_eq!(place.center_x, 1728.0 + 1280.0);
        assert_eq!(place.band, 24.0);
        // 180 x 24 / 32 is 135; at 1x a centered width must be even to keep its edges on pixels.
        assert_eq!(place.idle, Size::new(136.0, 20.0));
        assert_eq!(place.core, 116.0);
        let outline = place.outline(place.idle, false);
        assert_eq!(outline.silhouette, Silhouette::Floating);
        assert_eq!(outline.bounds(), Rect::new(-68.0, -22.0, 136.0, 20.0));
        // At 2x the 135 pt capsule already lands on pixels.
        let retina = placement(&DisplayInfo {
            scale: 2.0,
            ..external()
        });
        assert_eq!(retina.idle, Size::new(135.0, 20.0));
    }

    #[test]
    fn an_auto_hidden_menu_bar_falls_back_to_the_status_bar_then_24() {
        let hidden = DisplayInfo {
            visible: Rect::new(1728.0, 0.0, 2560.0, 1440.0),
            status_bar: 22.0,
            ..external()
        };
        assert_eq!(placement(&hidden).band, 22.0);
        let unknown = DisplayInfo {
            status_bar: f64::NAN,
            ..hidden
        };
        assert_eq!(placement(&unknown).band, 24.0);
        let tall = DisplayInfo {
            visible: Rect::new(1728.0, 0.0, 2560.0, 1340.0),
            ..unknown
        };
        assert_eq!(
            placement(&tall).band,
            24.0,
            "a 100 pt gap is not a menu bar"
        );
    }

    #[test]
    fn odd_widths_and_half_point_centers_keep_both_edges_on_pixels() {
        let display = DisplayInfo {
            frame: Rect::new(0.0, 0.0, 1511.0, 982.0),
            visible: Rect::new(0.0, 0.0, 1511.0, 958.0),
            ..external()
        };
        let place = placement(&display);
        assert_eq!(place.center_x, 755.5);
        for wing in [44.0, 45.5, 61.25, 160.0] {
            let size = place.compact(wing);
            assert!(size.width >= place.core + 2.0 * wing);
            assert!(on_pixels(place.center_x - size.width / 2.0, place.scale));
            assert!(on_pixels(place.center_x + size.width / 2.0, place.scale));
        }
        let notched = placement(&DisplayInfo {
            aux_right: Some(Rect::new(963.5, 1080.0, 764.5, 37.0)),
            ..notched()
        });
        assert_eq!(notched.center_x, 863.75);
        let open = notched.open(440.0, 301.3);
        assert!(on_pixels(notched.center_x - open.width / 2.0, 2.0));
        assert!(on_pixels(open.height, 2.0));
    }

    #[test]
    fn every_outline_shares_one_element_sequence_and_hangs_from_its_top() {
        let place = placement(&notched());
        for size in [place.idle, place.compact(44.0), place.open(440.0, 300.0)] {
            let outline = place.outline(size, true);
            assert_eq!(outline.points.len(), 17);
            assert_eq!(
                outline.points[0], outline.points[16],
                "the outline closes where it starts"
            );
            let bounds = outline.bounds();
            assert_eq!(
                (bounds.width, bounds.height, bounds.max_y()),
                (size.width, size.height, 0.0)
            );
        }
        let floating = placement(&external());
        let open = floating.outline(floating.open(440.0, 300.0), true);
        assert_eq!(open.bounds().max_y(), -2.0);
        let rest = floating.outline(floating.idle, false);
        // The capsule is fully round: its corner reaches half its height.
        assert_eq!(rest.points[4].y - rest.points[1].y, -10.0);
    }

    #[test]
    fn the_hanging_outline_uses_the_notch_proportions() {
        let tall = Outline::hanging(Size::new(440.0, 300.0));
        // Shoulder min(14, h * .19) and bottom corner min(28, h * .34).
        assert_eq!(
            tall.points[4],
            Point {
                x: 220.0 - 14.0,
                y: -14.0
            }
        );
        assert_eq!(
            tall.points[8],
            Point {
                x: 220.0 - 14.0 - 28.0,
                y: -300.0
            }
        );
        let short = Outline::hanging(Size::new(200.0, 32.0));
        assert!((short.points[4].x - (100.0 - 32.0 * 0.19)).abs() < 1e-9);
        let corner = 32.0 * 0.34;
        assert!((short.points[8].x - (100.0 - 32.0 * 0.19 - corner)).abs() < 1e-9);
    }

    #[test]
    fn interpolation_is_point_by_point_and_extrapolates_past_one() {
        let from = Outline::hanging(Size::new(200.0, 32.0));
        let to = Outline::hanging(Size::new(440.0, 300.0));
        assert_eq!(from.lerp(&to, 0.0), from);
        assert_eq!(from.lerp(&to, 1.0), to);
        let past = from.lerp(&to, 1.038).bounds();
        assert!(past.width > 440.0 && past.height > 300.0);
        assert!(from.travel(&to) > 268.0);
        // Halfway along, the whole motion is twice what is left, forward or back.
        let middle = from.lerp(&to, 0.5);
        assert!((middle.share_along(&from, &to, &to) - 2.0).abs() < 1e-9);
        assert!((middle.share_along(&from, &to, &from) + 2.0).abs() < 1e-9);
        assert_eq!(to.share_along(&from, &to, &to), 0.0);
    }

    #[test]
    fn the_envelope_leaves_room_for_the_swing_with_even_margins() {
        let place = placement(&notched());
        let open = place.open(440.0, 300.0);
        let envelope = place.envelope(place.idle, open, 0.038);
        assert!(envelope.width >= 440.0 + 0.038 * 240.0);
        assert!(envelope.height >= 300.0 + 0.038 * 268.0);
        assert_eq!(
            (envelope.width - open.width) % 2.0,
            0.0,
            "even margins keep it centered"
        );
        assert_eq!(envelope.height.fract(), 0.0);
        let frame = place.frame(envelope);
        assert_eq!(frame.max_y(), place.top);
        assert_eq!(frame.mid_x(), place.center_x);
        assert!(on_pixels(frame.x, 2.0));
        // Shrinking has no swing: the envelope is the larger size with its margins.
        let shrink = place.envelope(open, place.idle, 0.0);
        assert!(shrink.width >= open.width && shrink.width <= open.width + 2.0);
        let floating = placement(&external());
        let rest = floating.envelope(floating.idle, floating.idle, 0.0);
        assert!(rest.height >= floating.idle.height + floating.float);
        assert!(on_pixels(floating.frame(rest).x, 1.0));
    }
}
