//! Paints one moment of the page-turn chevron as premultiplied BGRA pixels in top-down rows.

use monhop_core::pointer_mark::{
    BloomFrame, CHEVRON_POINTS, CHEVRON_SHADE_SOFTNESS, CHEVRON_SHADE_SPREAD, CHEVRON_STROKE,
    MARK_INK, MARK_SHADE, PageDirection, bloom_reach,
};

const BYTES_PER_PIXEL: usize = 4;
/// The sharpest edge still antialiases, in pixels of deviation.
const ANTIALIAS: f64 = 0.5;
/// Past this many deviations beyond its edge a stroke rounds to a zero byte.
const CUTOFF: f64 = 4.0;

/// A surface with the pointer's pixel at its middle that holds the whole bloom unclipped.
pub(super) struct Canvas {
    pub(super) width: usize,
    pub(super) height: usize,
    pixels_per_dip: f64,
}

impl Canvas {
    pub(super) fn new(pixels_per_dip: f64) -> Self {
        let reach = bloom_reach();
        let side = |length: f64| 2 * (length * pixels_per_dip).ceil() as usize + 1;
        Self {
            width: side(reach.x),
            height: side(reach.y),
            pixels_per_dip,
        }
    }

    pub(super) fn pointer(&self) -> (usize, usize) {
        (self.width / 2, self.height / 2)
    }

    pub(super) fn byte_len(&self) -> usize {
        self.width * self.height * BYTES_PER_PIXEL
    }

    pub(super) fn paint(&self, pixels: &mut [u8], direction: PageDirection, frame: BloomFrame) {
        pixels.fill(0);
        // The frame's scale sizes the glyph, stroke, shade and blur alike; the travel stays unscaled.
        let unit = frame.scale * self.pixels_per_dip;
        let (x, y) = self.pointer();
        let center = (
            x as f64 + 0.5 + frame.offset * self.pixels_per_dip,
            y as f64 + 0.5,
        );
        let mirror = -direction.sign();
        let corners =
            CHEVRON_POINTS.map(|(x, y)| (center.0 + x * mirror * unit, center.1 + y * unit));
        let ink = Stroke {
            half_width: CHEVRON_STROKE / 2.0 * unit,
            deviation: (frame.blur * unit).max(ANTIALIAS),
        };
        let shade = Stroke {
            half_width: ink.half_width + CHEVRON_SHADE_SPREAD * unit,
            deviation: ink.deviation.hypot(CHEVRON_SHADE_SOFTNESS * unit),
        };
        let margin = shade.half_width + CUTOFF * shade.deviation;
        let span = |values: [f64; 3], limit: usize| {
            let low = values.iter().copied().fold(f64::INFINITY, f64::min) - margin;
            let high = values.iter().copied().fold(f64::NEG_INFINITY, f64::max) + margin;
            (low.floor().max(0.0) as usize).min(limit)..(high.ceil().max(0.0) as usize).min(limit)
        };
        for row in span(corners.map(|corner| corner.1), self.height) {
            for column in span(corners.map(|corner| corner.0), self.width) {
                let point = (column as f64 + 0.5, row as f64 + 0.5);
                let distance = corners
                    .windows(2)
                    .map(|arm| segment_distance(point, arm[0], arm[1]))
                    .fold(f64::INFINITY, f64::min);
                let ink_alpha = MARK_INK.alpha * ink.coverage(distance);
                // The ink lies over the shade.
                let shade_alpha = MARK_SHADE.alpha * shade.coverage(distance) * (1.0 - ink_alpha);
                let white = ink_alpha * MARK_INK.white + shade_alpha * MARK_SHADE.white;
                let gray = byte(white * frame.opacity);
                let at = (row * self.width + column) * BYTES_PER_PIXEL;
                pixels[at..at + BYTES_PER_PIXEL].copy_from_slice(&[
                    gray,
                    gray,
                    gray,
                    byte((ink_alpha + shade_alpha) * frame.opacity),
                ]);
            }
        }
    }
}

/// A Gaussian-blurred band of the polyline, in pixels.
struct Stroke {
    half_width: f64,
    deviation: f64,
}

impl Stroke {
    fn coverage(&self, distance: f64) -> f64 {
        normal_cdf((self.half_width - distance) / self.deviation)
            - normal_cdf((-self.half_width - distance) / self.deviation)
    }
}

fn segment_distance(point: (f64, f64), from: (f64, f64), to: (f64, f64)) -> f64 {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let along =
        (((point.0 - from.0) * dx + (point.1 - from.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    (point.0 - from.0 - along * dx).hypot(point.1 - from.1 - along * dy)
}

/// Abramowitz and Stegun 7.1.26: within 1.5e-7, far below one byte step.
fn normal_cdf(z: f64) -> f64 {
    let x = z.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.327_591_1 * x);
    let tail = t
        * (0.254_829_592
            + t * (-0.284_496_736
                + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let erf = 1.0 - tail * (-x * x).exp();
    0.5 * (1.0 + erf.copysign(z))
}

fn byte(share: f64) -> u8 {
    (share.clamp(0.0, 1.0) * 255.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use monhop_core::pointer_mark::{BLOOM_DURATION, bloom_frame, still_frame};
    use std::time::Duration;

    fn painted(
        canvas: &Canvas,
        direction: PageDirection,
        frame: BloomFrame,
    ) -> Vec<[u8; BYTES_PER_PIXEL]> {
        let mut bytes = vec![0; canvas.byte_len()];
        canvas.paint(&mut bytes, direction, frame);
        bytes.as_chunks().0.to_vec()
    }

    #[test]
    fn the_sharp_chevron_is_white_on_its_stroke_and_clear_elsewhere() {
        let canvas = Canvas::new(1.0);
        let frame = still_frame(PageDirection::Back);
        let pixels = painted(&canvas, PageDirection::Back, frame);
        let (x, y) = canvas.pointer();
        let row = y * canvas.width;
        let tip = (x as f64 + 0.5 + frame.offset + CHEVRON_POINTS[1].0).floor() as usize;
        let [blue, green, red, alpha] = pixels[row + tip];
        assert!(alpha > 220 && blue > 210, "{:?}", pixels[row + tip]);
        assert!(blue == green && green == red);
        assert_eq!(pixels[row + x], [0; 4]);
        assert_eq!(pixels[row + 2 * x - tip], [0; 4]);
    }

    #[test]
    fn forward_mirrors_back() {
        let canvas = Canvas::new(1.25);
        let at = Duration::from_millis(150);
        let [back, forward] = [PageDirection::Back, PageDirection::Forward]
            .map(|direction| painted(&canvas, direction, bloom_frame(direction, at).unwrap()));
        assert!(back.iter().any(|pixel| pixel[3] > 0));
        for (back, forward) in back.chunks(canvas.width).zip(forward.chunks(canvas.width)) {
            for (original, mirrored) in back.iter().zip(forward.iter().rev()) {
                assert!(
                    original
                        .iter()
                        .zip(mirrored)
                        .all(|(a, b)| a.abs_diff(*b) <= 1)
                );
            }
        }
    }

    #[test]
    fn blur_spreads_the_alpha_and_lowers_its_peak() {
        let canvas = Canvas::new(1.0);
        let sharp = still_frame(PageDirection::Forward);
        let [(sharp_peak, sharp_area), (blurred_peak, blurred_area)] =
            [sharp, BloomFrame { blur: 3.0, ..sharp }].map(|frame| {
                let pixels = painted(&canvas, PageDirection::Forward, frame);
                let alphas = pixels.iter().map(|pixel| pixel[3]);
                (
                    alphas.clone().max().unwrap(),
                    alphas.filter(|&alpha| alpha > 0).count(),
                )
            });
        assert!(blurred_peak < sharp_peak && blurred_area > sharp_area);
    }

    #[test]
    fn every_frame_stays_premultiplied_and_clear_of_the_edges() {
        const EDGE: usize = 3;
        for pixels_per_dip in [1.0, 1.5, 2.25] {
            let canvas = Canvas::new(pixels_per_dip);
            for direction in [PageDirection::Back, PageDirection::Forward] {
                for ms in (0..BLOOM_DURATION.as_millis() as u64).step_by(10) {
                    let frame = bloom_frame(direction, Duration::from_millis(ms)).unwrap();
                    let pixels = painted(&canvas, direction, frame);
                    for (index, pixel) in pixels.iter().enumerate() {
                        assert!(pixel[..3].iter().all(|&channel| channel <= pixel[3]));
                        let (row, column) = (index / canvas.width, index % canvas.width);
                        let inside = (EDGE..canvas.width - EDGE).contains(&column)
                            && (EDGE..canvas.height - EDGE).contains(&row);
                        assert!(inside || pixel[3] == 0, "{direction:?} at {ms} ms");
                    }
                }
            }
        }
    }
}
