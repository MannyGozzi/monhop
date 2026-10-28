//! Windows' pointer ballistics: how far Windows moves its pointer for a mouse's raw counts, from
//! the speed slider and, with Enhance pointer precision, the SmoothMouse acceleration curve.
//! Distances are logical units (DIPs), the same on every display scale.

/// The pointer speed slider's notches; Windows' default is the middle one.
pub const MIN_SPEED: u32 = 1;
pub const MAX_SPEED: u32 = 20;
pub const DEFAULT_SPEED: u32 = 10;

/// Most raw reports one message is taken to merge: Windows 11 coalesces a background listener's
/// messages to about 125 a second, which a 2000 Hz mouse fills with 16.
pub const MAX_MERGED_REPORTS: u32 = 16;

/// A SmoothMouse curve as the registry stores it: five little-endian 8-byte 16.16 fixed-point
/// values.
pub const CURVE_BYTES: usize = 40;
const CURVE_POINTS: usize = 5;

/// Windows' own SmoothMouseXCurve: 0, 0.43, 1.25, 3.86 and 40.
pub const DEFAULT_X_CURVE: [u8; CURVE_BYTES] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x15, 0x6e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x40, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x29, 0xdc, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x28, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Windows' own SmoothMouseYCurve: 0, 1.07, 4.14, 18.98 and 443.75.
pub const DEFAULT_Y_CURVE: [u8; CURVE_BYTES] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0xfd, 0x11, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x24, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0xfc, 0x12, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0xc0, 0xbb, 0x01, 0x00, 0x00, 0x00, 0x00,
];

/// A report's counts per unit along the curve's x axis.
const COUNTS_PER_CURVE_UNIT: f64 = 3.5;

/// The curve's y axis is pixels at the display's DPI over 150, which is 96/150 of a DIP at any DPI.
const DIPS_PER_CURVE_UNIT: f64 = 96.0 / 150.0;

/// Pixels per count without Enhance pointer precision, by slider notch.
const LINEAR_PIXELS_PER_COUNT: [f64; MAX_SPEED as usize] = [
    1.0 / 32.0,
    1.0 / 16.0,
    1.0 / 8.0,
    1.0 / 4.0,
    3.0 / 8.0,
    1.0 / 2.0,
    5.0 / 8.0,
    3.0 / 4.0,
    7.0 / 8.0,
    1.0,
    1.25,
    1.5,
    1.75,
    2.0,
    2.25,
    2.5,
    2.75,
    3.0,
    3.25,
    3.5,
];

/// One user's Windows pointer settings, as Windows applies them to each raw mouse report.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ballistics {
    enhance: bool,
    speed: u32,
    /// Points `(x, y)` with x rising strictly from 0 and y never falling, so every gain is finite
    /// and nonnegative.
    curve: [(f64, f64); CURVE_POINTS],
    /// Without Enhance pointer precision a count moves whole pixels, which the primary display
    /// scales from DIPs.
    pixels_per_dip: f64,
}

impl Default for Ballistics {
    fn default() -> Self {
        Self::new(true, DEFAULT_SPEED, None, None, 1.0)
    }
}

impl Ballistics {
    /// Windows' settings as read: a speed off the slider, a missing, malformed or non-monotonic
    /// curve, or an invalid display scale each fall back to Windows' default.
    pub fn new(
        enhance: bool,
        speed: u32,
        x_curve: Option<&[u8]>,
        y_curve: Option<&[u8]>,
        pixels_per_dip: f64,
    ) -> Self {
        let speed = if (MIN_SPEED..=MAX_SPEED).contains(&speed) {
            speed
        } else {
            DEFAULT_SPEED
        };
        let curve = curve(x_curve, y_curve)
            .or_else(|| curve(Some(&DEFAULT_X_CURVE), Some(&DEFAULT_Y_CURVE)))
            .expect("Windows' default curve is valid");
        let pixels_per_dip = if pixels_per_dip.is_finite() && pixels_per_dip > 0.0 {
            pixels_per_dip
        } else {
            1.0
        };
        Self {
            enhance,
            speed,
            curve,
            pixels_per_dip,
        }
    }

    /// DIPs per count for a record of `dx` and `dy` counts that merged `reports` reports, each
    /// taken as an equal share: one gain for both axes keeps the direction.
    pub fn gain(&self, dx: f64, dy: f64, reports: u32) -> f64 {
        if !self.enhance {
            return LINEAR_PIXELS_PER_COUNT[self.speed as usize - 1] / self.pixels_per_dip;
        }
        let reports = f64::from(reports.clamp(1, MAX_MERGED_REPORTS));
        let (x, y) = ((dx / reports).abs(), (dy / reports).abs());
        let magnitude = x.max(y) + x.min(y) / 2.0;
        let scale = DIPS_PER_CURVE_UNIT * f64::from(self.speed) / 10.0;
        let slope = |index: usize| {
            let ((x0, y0), (x1, y1)) = (self.curve[index - 1], self.curve[index]);
            (y1 - y0) / (x1 - x0)
        };
        // Vanishing and unbounded reports take the curve's limits, so no gain is NaN or infinite.
        if magnitude.is_nan() || magnitude == 0.0 {
            return scale * slope(1) / COUNTS_PER_CURVE_UNIT;
        }
        if magnitude.is_infinite() {
            return scale * slope(CURVE_POINTS - 1) / COUNTS_PER_CURVE_UNIT;
        }
        let along = magnitude / COUNTS_PER_CURVE_UNIT;
        // The last segment extends past the curve's last point.
        let index = (1..CURVE_POINTS)
            .find(|&index| along <= self.curve[index].0)
            .unwrap_or(CURVE_POINTS - 1);
        let (x0, y0) = self.curve[index - 1];
        let slope = slope(index);
        // The segment's intercept over the magnitude, so a huge report cannot overflow.
        (scale * ((y0 - slope * x0) / magnitude + slope / COUNTS_PER_CURVE_UNIT)).max(0.0)
    }

    /// The record's motion in DIPs.
    pub fn apply(&self, dx: f64, dy: f64, reports: u32) -> (f64, f64) {
        let gain = self.gain(dx, dy, reports);
        (dx * gain, dy * gain)
    }
}

fn curve(x: Option<&[u8]>, y: Option<&[u8]>) -> Option<[(f64, f64); CURVE_POINTS]> {
    let (xs, ys) = (decode(x?)?, decode(y?)?);
    let rising = xs[0] == 0.0
        && xs.windows(2).all(|pair| pair[0] < pair[1])
        && ys.windows(2).all(|pair| pair[0] <= pair[1]);
    rising.then(|| std::array::from_fn(|index| (xs[index], ys[index])))
}

/// Values past 32 bits are not 16.16 fixed point and reject the curve.
fn decode(bytes: &[u8]) -> Option<[f64; CURVE_POINTS]> {
    if bytes.len() != CURVE_BYTES {
        return None;
    }
    let mut values = [0.0; CURVE_POINTS];
    for (value, raw) in values.iter_mut().zip(bytes.as_chunks::<8>().0) {
        let raw = u64::from_le_bytes(*raw);
        *value = f64::from(u32::try_from(raw).ok()?) / 65536.0;
    }
    Some(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-4,
            "{actual} is not within 1e-4 of {expected}"
        );
    }

    #[test]
    fn the_default_curve_decodes_to_windows_documented_points() {
        let xs = decode(&DEFAULT_X_CURVE).unwrap();
        let ys = decode(&DEFAULT_Y_CURVE).unwrap();
        assert_eq!(xs, [0.0, 28181.0 / 65536.0, 1.25, 252969.0 / 65536.0, 40.0]);
        assert_eq!(ys, [0.0, 70141.0 / 65536.0, 4.140625, 18.984375, 443.75]);
        for (decoded, documented) in xs.into_iter().zip([0.0, 0.43, 1.25, 3.86, 40.0]) {
            assert!((decoded - documented).abs() < 1e-4);
        }
        for (decoded, documented) in ys.into_iter().zip([0.0, 1.07, 4.14, 18.98, 443.75]) {
            assert!((decoded - documented).abs() < 0.01);
        }
    }

    #[test]
    fn enhanced_precision_at_the_middle_notch_follows_the_default_curve() {
        let windows = Ballistics::default();
        for (counts, dips) in [
            (1.0, 0.455122),
            (2.0, 1.023871),
            (4.0, 2.393243),
            (8.0, 6.419840),
            (16.0, 17.501458),
            (32.0, 51.888367),
            (-6.0, -4.339928),
        ] {
            let (x, y) = windows.apply(counts, 0.0, 1);
            close(x, dips);
            assert_eq!(y, 0.0);
        }
        // 140 counts reach the curve's last point: 443.75 at 96/150.
        assert!((windows.apply(140.0, 0.0, 1).0 - 284.0).abs() < 1e-9);
    }

    #[test]
    fn one_gain_scales_both_axes_so_the_direction_holds() {
        let (x, y) = Ballistics::default().apply(4.0, 2.0, 1);
        close(x, 2.639978);
        close(y, 1.319989);
        assert_eq!(x / y, 2.0);
    }

    #[test]
    fn a_merged_message_moves_as_its_reports_would_one_by_one() {
        let windows = Ballistics::default();
        let merged = windows.apply(16.0, 0.0, 8).0;
        close(merged, 8.190969);
        assert!((merged - 8.0 * windows.apply(2.0, 0.0, 1).0).abs() < 1e-12);
        // Report counts outside 1 to MAX_MERGED_REPORTS clamp.
        assert_eq!(windows.gain(16.0, 0.0, 0), windows.gain(16.0, 0.0, 1));
        assert_eq!(
            windows.gain(64.0, 0.0, 1000),
            windows.gain(64.0, 0.0, MAX_MERGED_REPORTS)
        );
    }

    #[test]
    fn the_slider_scales_the_curve_and_sets_the_unaccelerated_step() {
        close(
            Ballistics::new(true, 20, None, None, 1.0)
                .apply(1.0, 0.0, 1)
                .0,
            0.910243,
        );
        for (speed, per_count) in [(1, 1.0 / 32.0), (6, 0.5), (10, 1.0), (11, 1.25), (20, 3.5)] {
            let linear = Ballistics::new(false, speed, None, None, 1.0);
            assert_eq!(linear.gain(1.0, 0.0, 1), per_count);
            assert_eq!(linear.gain(37.0, -5.0, 8), per_count);
        }
        // Unaccelerated counts move whole pixels, which a 150% display packs 1.5 to the DIP.
        assert_eq!(
            Ballistics::new(false, 10, None, None, 1.5).gain(3.0, 0.0, 1),
            1.0 / 1.5
        );
        // The curve's pixels already scale with DPI, so its DIPs do not.
        assert_eq!(
            Ballistics::new(true, 10, None, None, 1.5).gain(3.0, 0.0, 1),
            Ballistics::default().gain(3.0, 0.0, 1)
        );
    }

    #[test]
    fn invalid_settings_fall_back_to_windows_defaults() {
        let windows = Ballistics::default();
        for speed in [0, 21, u32::MAX] {
            assert_eq!(Ballistics::new(true, speed, None, None, 1.0), windows);
        }
        for scale in [0.0, -1.5, f64::NAN, f64::INFINITY] {
            assert_eq!(Ballistics::new(true, 10, None, None, scale), windows);
        }
        let mut falling = DEFAULT_X_CURVE;
        falling[24..32].copy_from_slice(&0x0001_0000u64.to_le_bytes());
        let mut sinking = DEFAULT_Y_CURVE;
        sinking[32..40].copy_from_slice(&0x0001_0000u64.to_le_bytes());
        let mut offset = DEFAULT_X_CURVE;
        offset[0] = 1;
        let mut wide = DEFAULT_Y_CURVE;
        wide[36] = 1;
        let short = &DEFAULT_X_CURVE[..32];
        for (x, y) in [
            (Some(&falling[..]), Some(&DEFAULT_Y_CURVE[..])),
            (Some(&DEFAULT_X_CURVE[..]), Some(&sinking[..])),
            (Some(&offset[..]), Some(&DEFAULT_Y_CURVE[..])),
            (Some(&DEFAULT_X_CURVE[..]), Some(&wide[..])),
            (Some(short), Some(&DEFAULT_Y_CURVE[..])),
            (Some(&DEFAULT_X_CURVE[..]), None),
            (None, Some(&DEFAULT_Y_CURVE[..])),
        ] {
            assert_eq!(Ballistics::new(true, 10, x, y, 1.0), windows);
        }
    }

    #[test]
    fn a_custom_curve_is_used_as_read() {
        // The curve y = x: 0.64/3.5 DIPs a count at every speed of the hand.
        let line = Ballistics::new(
            true,
            10,
            Some(&DEFAULT_X_CURVE),
            Some(&DEFAULT_X_CURVE),
            1.0,
        );
        for counts in [1.0, 7.0, 300.0] {
            assert!((line.gain(counts, 0.0, 1) - 0.64 / 3.5).abs() < 1e-12);
        }
    }

    #[test]
    fn no_input_yields_a_nan_or_infinite_gain() {
        let extreme = [
            0.0,
            -0.0,
            f64::MIN_POSITIVE,
            1e-300,
            1e300,
            f64::MAX,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            f64::from(i32::MIN),
            f64::from(i32::MAX),
        ];
        let mut steep_y = [0u8; CURVE_BYTES];
        for (index, value) in [0u64, 0xffff_ffff, 0xffff_ffff, 0xffff_ffff, 0xffff_ffff]
            .into_iter()
            .enumerate()
        {
            steep_y[index * 8..index * 8 + 8].copy_from_slice(&value.to_le_bytes());
        }
        let mut tiny_x = [0u8; CURVE_BYTES];
        for index in 0..CURVE_POINTS {
            tiny_x[index * 8..index * 8 + 8].copy_from_slice(&(index as u64).to_le_bytes());
        }
        for windows in [
            Ballistics::default(),
            Ballistics::new(true, MAX_SPEED, Some(&tiny_x), Some(&steep_y), 1.0),
            Ballistics::new(false, MIN_SPEED, None, None, 3.5),
        ] {
            for dx in extreme {
                for dy in extreme {
                    for reports in [0, 1, 3, MAX_MERGED_REPORTS, u32::MAX] {
                        let gain = windows.gain(dx, dy, reports);
                        assert!(gain.is_finite() && gain >= 0.0, "{dx} {dy} {reports}");
                    }
                }
            }
        }
    }
}
