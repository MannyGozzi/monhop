//! The page-turn chevron on macOS: each turn gets its own click-through window above normal windows
//! on every Space, whose Core Animation bloom the render server plays out before the window closes.
//! Everything here runs on the main thread, which is why `show` takes a `MainThreadMarker`.

use std::{cell::RefCell, ptr, time::Duration};

use block2::RcBlock;
use monhop_core::{
    Point,
    pointer_mark::{
        BLOOM_DURATION, BLOOM_EASING, BLOOM_KEYS, BLOOM_STILL, BloomFrame, CHEVRON_POINTS,
        CHEVRON_SHADE_SOFTNESS, CHEVRON_SHADE_SPREAD, CHEVRON_STROKE, Gray, MARK_INK, MARK_SHADE,
        PageDirection, PageTurn, bloom_reach, still_frame,
    },
};
use objc2::{MainThreadMarker, rc::Retained, runtime::AnyObject};
use objc2_app_kit::{NSColor, NSScreen, NSStatusWindowLevel, NSView, NSWindow, NSWorkspace};
use objc2_core_graphics::{CGColor, CGMutablePath};
use objc2_core_image::CIFilter;
use objc2_foundation::{
    NSArray, NSNumber, NSObjectNSKeyValueCoding, NSPoint, NSRect, NSSize, NSString, ns_string,
};
use objc2_quartz_core::{
    CAKeyframeAnimation, CALayer, CAMediaTiming, CAMediaTimingFunction, CAShapeLayer,
    CATransaction, kCALineCapRound, kCALineJoinRound,
};

use crate::dimming::macos::{close_all, overlay_window};

const OPACITY: &str = "opacity";
const SLIDE: &str = "position.x";
const SCALE: &str = "transform.scale";
/// The bloom's blur filter, by name and by the key path of its radius.
const BLUR: &str = "bloom";
const BLUR_RADIUS: &str = "filters.bloom.inputRadius";

thread_local! {
    /// Every window whose bloom is still playing; its completion takes it out and closes it.
    static LIVE: RefCell<Vec<Retained<NSWindow>>> = const { RefCell::new(Vec::new()) };
}

/// Opens a window centered on the pointer for this turn alone; it closes itself when its bloom ends.
pub(super) fn show(mtm: MainThreadMarker, turn: PageTurn) {
    let Some(primary) = NSScreen::screens(mtm).iter().next() else {
        return;
    };
    let frame = overlay_frame(turn.at, primary.frame().size.height);
    let window = overlay_window(mtm, frame, NSStatusWindowLevel);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setAlphaValue(1.0);
    let bounds = NSRect::new(NSPoint::new(0.0, 0.0), frame.size);
    let view = NSView::initWithFrame(mtm.alloc(), bounds);
    let root = CALayer::new();
    view.setLayer(Some(&root));
    view.setWantsLayer(true);
    view.setLayerUsesCoreImageFilters(true);
    window.setContentView(Some(&view));

    let closing = window.clone();
    let closed = RcBlock::new(move || {
        LIVE.with(|live| {
            live.borrow_mut()
                .retain(|open| !ptr::eq(&**open, &*closing))
        });
        close_all(vec![closing.clone()]);
    });
    let still = NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion();
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    // SAFETY: CATransaction copies the block and calls it on this, the main, thread.
    unsafe { CATransaction::setCompletionBlock(Some(&closed)) };
    root.addSublayer(&chevron(
        turn.direction,
        still,
        window.backingScaleFactor(),
        bounds,
    ));
    CATransaction::commit();
    window.orderFrontRegardless();
    LIVE.with(|live| live.borrow_mut().push(window));
}

/// The overlay in AppKit's bottom-left global points, centered on `at` (top-left global points)
/// and wide enough for the whole bloom on either side.
fn overlay_frame(at: Point, primary_height: f64) -> NSRect {
    let reach = bloom_reach();
    NSRect::new(
        NSPoint::new(at.x - reach.x, primary_height - at.y - reach.y),
        NSSize::new(2.0 * reach.x, 2.0 * reach.y),
    )
}

/// One animated key path of the chevron layer and its value at each of `BLOOM_KEYS`.
struct Track {
    key_path: &'static str,
    values: [f64; BLOOM_KEYS.len()],
}

/// The bloom for `direction` beside a pointer at `pointer_x`, or under reduced motion the sharpest
/// frame at every key. Core Image blurs in pixels, `pixels_per_point` to a point.
fn tracks(
    direction: PageDirection,
    still: bool,
    pixels_per_point: f64,
    pointer_x: f64,
) -> [Track; 4] {
    let frames = BLOOM_KEYS.map(|(_, frame)| {
        if still {
            still_frame(direction)
        } else {
            BloomFrame {
                offset: frame.offset * direction.sign(),
                ..frame
            }
        }
    });
    let track = |key_path, value: &dyn Fn(BloomFrame) -> f64| Track {
        key_path,
        values: frames.map(value),
    };
    [
        track(OPACITY, &|frame| frame.opacity),
        track(SLIDE, &|frame| pointer_x + frame.offset),
        track(BLUR_RADIUS, &|frame| frame.blur * pixels_per_point),
        track(SCALE, &|frame| frame.scale),
    ]
}

/// The chevron's layer, as big as `bounds` and centered on it, already animating. Its blur and
/// scale share the layer, so the blur, stroke and shade all scale together as in CSS.
fn chevron(
    direction: PageDirection,
    still: bool,
    pixels_per_point: f64,
    bounds: NSRect,
) -> Retained<CALayer> {
    let chevron = CALayer::new();
    place(&chevron, bounds, pixels_per_point);
    chevron.setAllowsGroupOpacity(true);
    // SAFETY: an array of CIFilter, the type macOS layer filters take.
    unsafe { chevron.setFilters(blur(BLUR, 0.0).as_deref()) };
    let shade = stroke(
        direction,
        bounds,
        CHEVRON_STROKE + 2.0 * CHEVRON_SHADE_SPREAD,
        MARK_SHADE,
    );
    place(&shade, bounds, pixels_per_point);
    // SAFETY: as above.
    unsafe {
        shade.setFilters(blur("shade", CHEVRON_SHADE_SOFTNESS * pixels_per_point).as_deref())
    };
    let ink = stroke(direction, bounds, CHEVRON_STROKE, MARK_INK);
    place(&ink, bounds, pixels_per_point);
    chevron.addSublayer(&shade);
    chevron.addSublayer(&ink);

    let run = if still { BLOOM_STILL } else { BLOOM_DURATION };
    let pointer_x = bounds.size.width / 2.0;
    for track in tracks(direction, still, pixels_per_point, pointer_x) {
        animate(&chevron, &track, run);
    }
    chevron
}

fn place(layer: &CALayer, bounds: NSRect, pixels_per_point: f64) {
    layer.setFrame(bounds);
    layer.setContentsScale(pixels_per_point);
}

/// The chevron's polyline centered in `bounds`, pointing the way the page went.
fn stroke(
    direction: PageDirection,
    bounds: NSRect,
    width: f64,
    gray: Gray,
) -> Retained<CAShapeLayer> {
    // Back draws the points as given; y flips because layers here run y up.
    let mirror = -direction.sign();
    let (center_x, center_y) = (bounds.size.width / 2.0, bounds.size.height / 2.0);
    let path = CGMutablePath::new();
    for (index, &(x, y)) in CHEVRON_POINTS.iter().enumerate() {
        let (x, y) = (center_x + x * mirror, center_y - y);
        // SAFETY: a null transform adds the point as given.
        unsafe {
            if index == 0 {
                CGMutablePath::move_to_point(Some(&path), ptr::null(), x, y);
            } else {
                CGMutablePath::add_line_to_point(Some(&path), ptr::null(), x, y);
            }
        }
    }
    let shape = CAShapeLayer::new();
    shape.setPath(Some(&path));
    shape.setFillColor(None);
    shape.setStrokeColor(Some(&CGColor::new_generic_gray(gray.white, gray.alpha)));
    shape.setLineWidth(width);
    // SAFETY: constant strings QuartzCore exports.
    unsafe {
        shape.setLineCap(kCALineCapRound);
        shape.setLineJoin(kCALineJoinRound);
    }
    shape
}

/// A `filters` array of one Gaussian blur called `name`, so a key path can reach its radius.
fn blur(name: &str, radius: f64) -> Option<Retained<NSArray>> {
    // SAFETY: CIGaussianBlur is a built-in filter.
    let filter = unsafe { CIFilter::filterWithName(ns_string!("CIGaussianBlur")) }?;
    // SAFETY: plain setters on a filter nothing else holds yet; the radius is a number.
    unsafe {
        filter.setDefaults();
        filter.setName(&NSString::from_str(name));
        filter.setValue_forKey(Some(&NSNumber::new_f64(radius)), ns_string!("inputRadius"));
    }
    // SAFETY: every CIFilter is an object.
    Some(unsafe { Retained::cast_unchecked(NSArray::from_retained_slice(&[filter])) })
}

/// Plays `track` on `layer` over `run`: the easing paces the whole run and keyframes interpolate
/// linearly within it.
fn animate(layer: &CALayer, track: &Track, run: Duration) {
    let key_path = NSString::from_str(track.key_path);
    let animation = CAKeyframeAnimation::animationWithKeyPath(Some(&key_path));
    let key_times = BLOOM_KEYS.map(|(at, _)| NSNumber::new_f64(at));
    animation.setKeyTimes(Some(&NSArray::from_retained_slice(&key_times)));
    let numbers = track.values.map(NSNumber::new_f64);
    let values = NSArray::from_retained_slice(&numbers);
    // SAFETY: every key path animated here takes numbers.
    unsafe { animation.setValues(Some(values.cast_unchecked::<AnyObject>())) };
    let [x1, y1, x2, y2] = BLOOM_EASING.map(|point| point as f32);
    animation.setTimingFunction(Some(&CAMediaTimingFunction::functionWithControlPoints(
        x1, y1, x2, y2,
    )));
    animation.setDuration(run.as_secs_f64());
    // The model holds the last keyframe, so nothing jumps back when the animation is removed.
    let [.., last] = &numbers;
    // SAFETY: a number for a numeric key path.
    unsafe { layer.setValue_forKeyPath(Some(last), &key_path) };
    layer.addAnimation_forKey(&animation, Some(&key_path));
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIRECTIONS: [PageDirection; 2] = [PageDirection::Back, PageDirection::Forward];
    const PIXELS_PER_POINT: f64 = 2.0;
    const POINTER_X: f64 = 60.0;

    /// What `key_path` should read at `frame`.
    fn expected(key_path: &str, frame: BloomFrame) -> f64 {
        match key_path {
            OPACITY => frame.opacity,
            SLIDE => POINTER_X + frame.offset,
            BLUR_RADIUS => frame.blur * PIXELS_PER_POINT,
            SCALE => frame.scale,
            _ => unreachable!("{key_path}"),
        }
    }

    fn key_frame(direction: PageDirection, key: usize) -> BloomFrame {
        let frame = BLOOM_KEYS[key].1;
        BloomFrame {
            offset: frame.offset * direction.sign(),
            ..frame
        }
    }

    #[test]
    fn the_chevron_starts_on_the_turned_side_of_the_pointer() {
        let at = Point::new(400.0, 300.0);
        let frame = overlay_frame(at, 1000.0);
        let pointer_x = frame.size.width / 2.0;
        assert_eq!(frame.origin.x + pointer_x, at.x);
        assert_eq!(frame.origin.y + frame.size.height / 2.0, 1000.0 - at.y);
        for direction in DIRECTIONS {
            let slide = tracks(direction, false, PIXELS_PER_POINT, pointer_x)
                .into_iter()
                .find(|track| track.key_path == SLIDE)
                .unwrap();
            let start = frame.origin.x + slide.values[0];
            assert_eq!((start - at.x).signum(), direction.sign());
        }
    }

    #[test]
    fn the_tracks_follow_the_bloom_keys_and_hold_still_under_reduced_motion() {
        for direction in DIRECTIONS {
            for track in tracks(direction, false, PIXELS_PER_POINT, POINTER_X) {
                for key in 0..BLOOM_KEYS.len() {
                    let frame = key_frame(direction, key);
                    assert_eq!(track.values[key], expected(track.key_path, frame));
                }
            }
            for track in tracks(direction, true, PIXELS_PER_POINT, POINTER_X) {
                let still = expected(track.key_path, still_frame(direction));
                assert!(track.values.iter().all(|&value| value == still));
            }
        }
    }

    #[test]
    #[ignore = "builds native Core Animation layers"]
    fn the_layer_carries_the_bloom() {
        let bounds = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(2.0 * POINTER_X, POINTER_X),
        );
        for direction in DIRECTIONS {
            // Read before committing: a commit outside any window ends every animation at once.
            CATransaction::begin();
            let chevron = chevron(direction, false, PIXELS_PER_POINT, bounds);
            for key_path in [OPACITY, SLIDE, BLUR_RADIUS, SCALE] {
                let key = NSString::from_str(key_path);
                // SAFETY: the animation `animate` added under its own key path.
                let animation = unsafe { chevron.animationForKey(&key) }
                    .unwrap()
                    .downcast::<CAKeyframeAnimation>()
                    .unwrap();
                let key_times: Vec<f64> = animation
                    .keyTimes()
                    .unwrap()
                    .iter()
                    .map(|at| at.as_f64())
                    .collect();
                assert_eq!(key_times, BLOOM_KEYS.map(|(at, _)| at));
                let values: Vec<f64> = animation
                    .values()
                    .unwrap()
                    .iter()
                    .map(|value| value.downcast_ref::<NSNumber>().unwrap().as_f64())
                    .collect();
                let wanted: Vec<f64> = (0..BLOOM_KEYS.len())
                    .map(|key| expected(key_path, key_frame(direction, key)))
                    .collect();
                assert_eq!(values, wanted);
                let model = chevron.valueForKeyPath(&key).unwrap();
                let model = model.downcast_ref::<NSNumber>().unwrap().as_f64();
                assert!((model - wanted[wanted.len() - 1]).abs() < 1e-6);
            }
            CATransaction::commit();
        }
    }
}
