//! Core Animation and AppKit building blocks for the island: colors, outlines as paths, text,
//! symbols, animations and timers. Main thread only.

use std::{cell::Cell, ffi::c_void, ops::Deref, ptr::NonNull};

use block2::RcBlock;
use objc2::{AnyThread, Message, rc::Retained, runtime::AnyObject};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSFont, NSFontAttributeName, NSFontWeight,
    NSFontWeightMedium, NSFontWeightRegular, NSFontWeightSemibold, NSImage,
    NSImageSymbolConfiguration,
};
use objc2_core_graphics::{CGColor, CGMutablePath, CGPath, CGPathElement, CGPathElementType};
use objc2_core_image::CIFilter;
use objc2_foundation::{
    NSArray, NSAttributedString, NSDictionary, NSNumber, NSObjectNSKeyValueCoding,
    NSOperationQueue, NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize, NSString, NSTimer,
    ns_string,
};
use objc2_quartz_core::{
    CAAnimation, CABasicAnimation, CACurrentMediaTime, CALayer, CAMediaTiming,
    CAMediaTimingFunction, CASpringAnimation, CATextLayer, CATransaction, kCAAlignmentLeft,
    kCAAlignmentRight, kCAFillModeBackwards, kCATruncationEnd,
};

use super::super::{
    geometry::{Outline, Point, Rect},
    tokens::{Color, Spring},
};

/// The blur filter's name, and the key path of its radius.
pub const BLUR: &str = "blur";
pub const BLUR_RADIUS: &str = "filters.blur.inputRadius";
/// A layer's own key (CALayer stores any key) for when its blur has cleared and may be dropped.
const SHARP_AT: &str = "monhopSharpAt";
/// Timers may fire a hair before the media clock reads the time they were set for.
const CLOCK_SLACK: f64 = 1e-3;

pub fn cg(color: Color) -> impl Deref<Target = CGColor> {
    let [r, g, b, a] = color.srgb();
    CGColor::new_srgb(r, g, b, a)
}

pub fn rect(rect: Rect) -> NSRect {
    NSRect::new(
        NSPoint::new(rect.x, rect.y),
        NSSize::new(rect.width, rect.height),
    )
}

/// Gives `layer` the bounds `area` in its superlayer's own coordinates, so both share one space.
pub fn share_space(layer: &CALayer, area: Rect) {
    layer.setAnchorPoint(NSPoint::new(0.5, 0.5));
    layer.setBounds(rect(area));
    layer.setPosition(NSPoint::new(area.mid_x(), area.y + area.height / 2.0));
}

/// A content group spanning `area` in island coordinates that scales about the island's top
/// center, its (0, 0).
pub fn group(area: Rect) -> Retained<CALayer> {
    let layer = CALayer::new();
    span(&layer, area);
    layer.setPosition(NSPoint::new(0.0, 0.0));
    layer
}

/// Gives a group new bounds without moving its content or the point it scales about.
pub fn span(layer: &CALayer, area: Rect) {
    let (width, height) = (area.width.max(1.0), area.height.max(1.0));
    layer.setBounds(NSRect::new(
        NSPoint::new(area.x, area.y),
        NSSize::new(width, height),
    ));
    layer.setAnchorPoint(NSPoint::new(-area.x / width, -area.y / height));
}

/// Takes a layer out of the tree once its exit has played.
pub fn remove_later(layer: &CALayer, seconds: f64) {
    let leaving = layer.retain();
    let _ = after(seconds, move || quietly(|| leaving.removeFromSuperlayer()));
}

/// The outline as a path: a move, then four line-and-curve pairs, closed.
pub fn path(outline: &Outline) -> impl Deref<Target = CGMutablePath> {
    let path = CGMutablePath::new();
    let p = &outline.points;
    // SAFETY: plain path construction with null transforms, so points are added as given.
    unsafe {
        CGMutablePath::move_to_point(Some(&path), std::ptr::null(), p[0].x, p[0].y);
        for pair in 0..4 {
            let at = 1 + pair * 4;
            CGMutablePath::add_line_to_point(Some(&path), std::ptr::null(), p[at].x, p[at].y);
            CGMutablePath::add_curve_to_point(
                Some(&path),
                std::ptr::null(),
                p[at + 1].x,
                p[at + 1].y,
                p[at + 2].x,
                p[at + 2].y,
                p[at + 3].x,
                p[at + 3].y,
            );
        }
    }
    CGMutablePath::close_subpath(Some(&path));
    path
}

unsafe extern "C-unwind" fn collect_element(info: *mut c_void, element: NonNull<CGPathElement>) {
    // SAFETY: `info` is the Vec handed to CGPathApply below, alive for the whole call.
    let elements = unsafe { &mut *info.cast::<Vec<Vec<Point>>>() };
    // SAFETY: CoreGraphics passes a valid element for the duration of the callback.
    let element = unsafe { element.as_ref() };
    let count = match element.r#type {
        CGPathElementType::MoveToPoint | CGPathElementType::AddLineToPoint => 1,
        CGPathElementType::AddQuadCurveToPoint => 2,
        CGPathElementType::AddCurveToPoint => 3,
        _ => return,
    };
    let points = (0..count)
        .map(|index| {
            // SAFETY: the element carries `count` points of its type.
            let point = unsafe { *element.points.as_ptr().add(index) };
            Point {
                x: point.x,
                y: point.y,
            }
        })
        .collect();
    elements.push(points);
}

/// Reads a path built by [`path`] back into an outline, as a presentation layer draws it now.
/// An interpolated path may carry a line as a curve, so each segment is read by its end points.
pub fn read_outline(path: &CGPath) -> Option<Outline> {
    let mut elements: Vec<Vec<Point>> = Vec::with_capacity(9);
    // SAFETY: the callback only appends to `elements`, which outlives the synchronous apply.
    unsafe {
        CGPath::apply(
            Some(path),
            (&raw mut elements).cast(),
            Some(collect_element),
        );
    }
    if elements.len() != 9 {
        return None;
    }
    let mut points = [Point { x: 0.0, y: 0.0 }; 17];
    points[0] = *elements[0].last()?;
    for pair in 0..4 {
        let at = 1 + pair * 4;
        let start = *elements[1 + pair * 2].last()?;
        points[at] = start;
        let curve = &elements[2 + pair * 2];
        let end = *curve.last()?;
        let (first, second) = match curve.as_slice() {
            [first, second, _] => (*first, *second),
            // A quadratic's one control point, raised to the cubic it equals.
            [control, _] => (
                Point {
                    x: start.x + (control.x - start.x) * 2.0 / 3.0,
                    y: start.y + (control.y - start.y) * 2.0 / 3.0,
                },
                Point {
                    x: end.x + (control.x - end.x) * 2.0 / 3.0,
                    y: end.y + (control.y - end.y) * 2.0 / 3.0,
                },
            ),
            _ => (start, end),
        };
        points[at + 1] = first;
        points[at + 2] = second;
        points[at + 3] = end;
    }
    Some(Outline { points })
}

/// A color as the `id` Core Animation values take.
pub fn color_value(color: &CGColor) -> &AnyObject {
    // SAFETY: every CoreFoundation object is an Objective-C object (toll-free bridged).
    unsafe { &*(std::ptr::from_ref(color).cast::<AnyObject>()) }
}

/// A path as the `id` Core Animation values take.
pub fn path_value(path: &CGPath) -> &AnyObject {
    // SAFETY: every CoreFoundation object is an Objective-C object (toll-free bridged).
    unsafe { &*(std::ptr::from_ref(path).cast::<AnyObject>()) }
}

pub fn now() -> f64 {
    CACurrentMediaTime()
}

pub fn ease(points: [f64; 4]) -> Retained<CAMediaTimingFunction> {
    let [x1, y1, x2, y2] = points.map(|point| point as f32);
    CAMediaTimingFunction::functionWithControlPoints(x1, y1, x2, y2)
}

/// A spring on `key` from `from` to `to`, as long as it takes to settle.
pub fn spring(
    key: &str,
    from: &AnyObject,
    to: &AnyObject,
    spring: Spring,
    velocity: f64,
    duration: f64,
) -> Retained<CASpringAnimation> {
    let animation = CASpringAnimation::new();
    animation.setKeyPath(Some(&NSString::from_str(key)));
    // SAFETY: the values are of the type `key` animates.
    unsafe {
        animation.setFromValue(Some(from));
        animation.setToValue(Some(to));
    }
    animation.setMass(1.0);
    animation.setStiffness(spring.stiffness());
    animation.setDamping(spring.damping());
    animation.setInitialVelocity(velocity);
    animation.setDuration(duration.max(1.0 / 120.0));
    animation
}

/// An eased change on `key`, starting `delay` seconds from now and holding `from` until then.
pub fn tween(
    key: &str,
    from: &AnyObject,
    to: &AnyObject,
    duration: f64,
    easing: [f64; 4],
    delay: f64,
) -> Retained<CABasicAnimation> {
    let animation = CABasicAnimation::new();
    animation.setKeyPath(Some(&NSString::from_str(key)));
    // SAFETY: the values are of the type `key` animates.
    unsafe {
        animation.setFromValue(Some(from));
        animation.setToValue(Some(to));
    }
    animation.setDuration(duration);
    animation.setTimingFunction(Some(&ease(easing)));
    delayed(&animation, delay);
    animation
}

/// Starts `animation` `delay` seconds from now, holding its first value until then.
pub fn delayed(animation: &CAAnimation, delay: f64) {
    if delay > 0.0 {
        animation.setBeginTime(now() + delay);
        // SAFETY: a constant QuartzCore exports.
        animation.setFillMode(unsafe { kCAFillModeBackwards });
    }
}

pub fn number(value: f64) -> Retained<NSNumber> {
    NSNumber::new_f64(value)
}

/// Sets a numeric key path's model value, so the layer rests there once an animation ends.
pub fn set_number(layer: &CALayer, key: &str, value: f64) {
    // SAFETY: every key path set through here takes a number.
    unsafe { layer.setValue_forKeyPath(Some(&number(value)), &NSString::from_str(key)) };
}

/// The presentation value of a numeric key path, or `fallback` when none is on screen yet.
pub fn shown_number(layer: &CALayer, key: &str, fallback: f64) -> f64 {
    // SAFETY: the presentation layer is a read-only copy of this one.
    let Some(presentation) = (unsafe { layer.presentationLayer() }) else {
        return fallback;
    };
    presentation
        .valueForKeyPath(&NSString::from_str(key))
        .and_then(|value| value.downcast::<NSNumber>().ok())
        .map_or(fallback, |value| value.as_f64())
}

pub fn add(layer: &CALayer, key: &str, animation: &objc2_quartz_core::CAAnimation) {
    layer.addAnimation_forKey(animation, Some(&NSString::from_str(key)));
}

pub fn remove(layer: &CALayer, key: &str) {
    layer.removeAnimationForKey(&NSString::from_str(key));
}

/// Runs `change` with implicit animations off, so only explicit ones play.
pub fn quietly<R>(change: impl FnOnce() -> R) -> R {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    let result = change();
    CATransaction::commit();
    result
}

/// A `filters` array holding one Gaussian blur named [`BLUR`].
pub fn blur_filters(radius: f64) -> Option<Retained<NSArray>> {
    // SAFETY: CIGaussianBlur is a built-in filter.
    let filter = unsafe { CIFilter::filterWithName(ns_string!("CIGaussianBlur")) }?;
    // SAFETY: plain setters on a filter nothing else holds yet; the radius is a number.
    unsafe {
        filter.setDefaults();
        filter.setName(&NSString::from_str(BLUR));
        filter.setValue_forKey(Some(&number(radius)), ns_string!("inputRadius"));
    }
    // SAFETY: every CIFilter is an object.
    Some(unsafe { Retained::cast_unchecked(NSArray::from_retained_slice(&[filter])) })
}

pub fn set_blur(layer: &CALayer, radius: Option<f64>) {
    // SAFETY: an array of CIFilter, the type macOS layer filters take, or none.
    unsafe { layer.setFilters(radius.and_then(blur_filters).as_deref()) };
}

/// The blur radius on screen now; zero without a filter.
pub fn shown_blur(layer: &CALayer) -> f64 {
    shown_number(layer, BLUR_RADIUS, 0.0)
}

fn own_number(layer: &CALayer, key: &str) -> Option<f64> {
    layer
        .valueForKey(&NSString::from_str(key))
        .and_then(|value| value.downcast::<NSNumber>().ok())
        .map(|value| value.as_f64())
}

/// Clears a blur from `from` to sharp after `delay`, then drops the filter unless a later blur
/// change took the layer over, so nothing filters a still island.
pub fn focus(layer: &CALayer, from: f64, duration: f64, easing: [f64; 4], delay: f64) {
    set_blur(layer, Some(0.0));
    let clear = tween(
        BLUR_RADIUS,
        &number(from),
        &number(0.0),
        duration,
        easing,
        delay,
    );
    add(layer, BLUR_RADIUS, &clear);
    let sharp_at = now() + delay + duration;
    set_number(layer, SHARP_AT, sharp_at);
    let layer = layer.retain();
    let _ = after(delay + duration, move || {
        if own_number(&layer, SHARP_AT).is_some_and(|at| at <= now() + CLOCK_SLACK) {
            quietly(|| set_blur(&layer, None));
        }
    });
}

/// Blurs a layer from `from` to `to` and holds it there; the layer is on its way out.
pub fn blur_to(layer: &CALayer, from: f64, to: f64, duration: f64, easing: [f64; 4]) {
    set_blur(layer, Some(to));
    add(
        layer,
        BLUR_RADIUS,
        &tween(
            BLUR_RADIUS,
            &number(from),
            &number(to),
            duration,
            easing,
            0.0,
        ),
    );
    set_number(layer, SHARP_AT, f64::INFINITY);
}

/// Where a layer is drawn now, which an animated move may have taken from its model position.
fn shown_position(layer: &CALayer) -> NSPoint {
    // SAFETY: the presentation layer is a read-only copy of this one.
    unsafe { layer.presentationLayer() }.map_or(layer.position(), |shown| shown.position())
}

/// A copy of a line of text as it shows now, to carry its old words out while it takes new ones.
pub fn text_ghost(layer: &CATextLayer) -> Retained<CATextLayer> {
    let ghost = CATextLayer::new();
    // SAFETY: the font, string and color come from a layer of the same class.
    unsafe {
        ghost.setFont(layer.font().as_deref());
        ghost.setString(layer.string().as_deref());
    }
    ghost.setFontSize(layer.fontSize());
    ghost.setForegroundColor(layer.foregroundColor().as_deref());
    ghost.setAlignmentMode(&layer.alignmentMode());
    ghost.setTruncationMode(&layer.truncationMode());
    ghost.setContentsScale(layer.contentsScale());
    ghost.setBounds(layer.bounds());
    ghost.setAnchorPoint(layer.anchorPoint());
    ghost.setPosition(shown_position(layer));
    ghost.setOpacity(shown_number(layer, "opacity", f64::from(layer.opacity())) as f32);
    ghost
}

/// A copy of a plain layer as it shows now: its size, place, fill, corners and opacity.
pub fn ghost(layer: &CALayer) -> Retained<CALayer> {
    let ghost = CALayer::new();
    ghost.setBounds(layer.bounds());
    ghost.setAnchorPoint(layer.anchorPoint());
    ghost.setPosition(shown_position(layer));
    ghost.setCornerRadius(layer.cornerRadius());
    // SAFETY: the presentation layer is a read-only copy of this one.
    let color = unsafe { layer.presentationLayer() }
        .and_then(|shown| shown.backgroundColor())
        .or_else(|| layer.backgroundColor());
    ghost.setBackgroundColor(color.as_deref());
    ghost.setOpacity(shown_number(layer, "opacity", f64::from(layer.opacity())) as f32);
    ghost
}

/// Gives a glyph the symbol `image` and returns a copy of it as it showed, keeping the old symbol.
pub fn hand_over_glyph(layer: &CALayer, image: &NSImage, scale: f64) -> Retained<CALayer> {
    let ghost = ghost(layer);
    let old = layer.mask();
    let size = image.size();
    layer.setBounds(NSRect::new(NSPoint::new(0.0, 0.0), size));
    // SAFETY: each mask is a plain layer only its own glyph uses; the old one moves to the ghost.
    unsafe {
        layer.setMask(Some(&glyph_mask(image, scale)));
        ghost.setMask(old.as_deref());
    }
    ghost
}

/// Recolors a line of text from the color it shows now, over `duration`.
pub fn retint(layer: &CATextLayer, to: Color, duration: f64, easing: [f64; 4]) {
    // SAFETY: the presentation layer is a read-only copy of this one.
    let from = unsafe { layer.presentationLayer() }
        .and_then(|shown| shown.downcast::<CATextLayer>().ok())
        .and_then(|shown| shown.foregroundColor())
        .or_else(|| layer.foregroundColor());
    let target = cg(to);
    layer.setForegroundColor(Some(&target));
    if let Some(from) = from {
        let change = tween(
            "foregroundColor",
            color_value(&from),
            color_value(&target),
            duration,
            easing,
            0.0,
        );
        add(layer, "foregroundColor", &change);
    }
}

/// A system font at a CSS weight (400, 500 or 600).
pub fn font(size: f64, weight: f64) -> Retained<NSFont> {
    // SAFETY: AppKit's exported weight constants.
    let weight: NSFontWeight = unsafe {
        if weight >= 600.0 {
            NSFontWeightSemibold
        } else if weight >= 500.0 {
            NSFontWeightMedium
        } else {
            NSFontWeightRegular
        }
    };
    NSFont::systemFontOfSize_weight(size, weight)
}

/// How wide `text` sets in `font`, in points.
pub fn measure(text: &str, font: &NSFont) -> f64 {
    let font_object: &AnyObject = font;
    // SAFETY: AppKit's exported attribute name.
    let key = unsafe { NSFontAttributeName };
    let attributes = NSDictionary::from_slices(&[key], &[font_object]);
    // SAFETY: a font under the font attribute, as NSAttributedString expects.
    let string = unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(text),
            Some(&attributes),
        )
    };
    string.size().width.ceil()
}

/// A line of text in `font`: its height fits the font's line, so centering the layer centers the
/// text.
pub fn text_layer(
    text: &str,
    font: &NSFont,
    color: Color,
    right: bool,
    scale: f64,
) -> Retained<CATextLayer> {
    let layer = CATextLayer::new();
    // SAFETY: the font is the CTFont `setFont` takes, and the string an NSString.
    unsafe {
        layer.setFont(Some(as_ct_font(font)));
        layer.setString(Some(&NSString::from_str(text)));
        layer.setTruncationMode(kCATruncationEnd);
        layer.setAlignmentMode(if right {
            kCAAlignmentRight
        } else {
            kCAAlignmentLeft
        });
    }
    layer.setFontSize(font.pointSize());
    layer.setForegroundColor(Some(&cg(color)));
    layer.setContentsScale(scale);
    layer
}

/// An NSFont as the CoreFoundation type `CATextLayer.setFont` takes; `T` is only ever that type.
fn as_ct_font<T>(font: &NSFont) -> &T {
    // SAFETY: NSFont is toll-free bridged with CTFont, a CoreFoundation object.
    unsafe { &*std::ptr::from_ref(font).cast::<T>() }
}

pub fn line_height(font: &NSFont) -> f64 {
    (font.ascender() - font.descender() + font.leading()).ceil()
}

pub fn set_string(layer: &CATextLayer, text: &str) {
    // SAFETY: CATextLayer takes an NSString.
    unsafe { layer.setString(Some(&NSString::from_str(text))) };
}

/// An SF Symbol at `size` points, as a template whose alpha masks a colored layer.
pub fn symbol(name: &str, size: f64, weight: f64) -> Option<Retained<NSImage>> {
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(name),
        None,
    )?;
    // SAFETY: AppKit's exported weight constants.
    let weight: NSFontWeight = unsafe {
        if weight >= 600.0 {
            NSFontWeightSemibold
        } else if weight >= 500.0 {
            NSFontWeightMedium
        } else {
            NSFontWeightRegular
        }
    };
    let configuration = NSImageSymbolConfiguration::configurationWithPointSize_weight(size, weight);
    image.imageWithSymbolConfiguration(&configuration)
}

/// A glyph tinted `color`: a colored square masked by the symbol, sized to the symbol.
pub fn glyph(image: &NSImage, color: Color, scale: f64) -> Retained<CALayer> {
    let size = image.size();
    let layer = CALayer::new();
    layer.setBounds(NSRect::new(NSPoint::new(0.0, 0.0), size));
    layer.setBackgroundColor(Some(&cg(color)));
    // SAFETY: the mask is a plain layer only this glyph uses.
    unsafe { layer.setMask(Some(&glyph_mask(image, scale))) };
    layer
}

pub fn glyph_mask(image: &NSImage, scale: f64) -> Retained<CALayer> {
    let size = image.size();
    let mask = CALayer::new();
    mask.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), size));
    mask.setContentsScale(image.recommendedLayerContentsScale(scale));
    // SAFETY: an image's layer contents for this scale is what `contents` takes.
    unsafe { mask.setContents(Some(&image.layerContentsForContentsScale(scale))) };
    mask
}

/// Swaps a glyph layer's symbol at once.
pub fn set_glyph(layer: &CALayer, image: &NSImage, scale: f64) {
    layer.setBounds(NSRect::new(NSPoint::new(0.0, 0.0), image.size()));
    // SAFETY: the mask is a plain layer this glyph owns.
    unsafe { layer.setMask(Some(&glyph_mask(image, scale))) };
}

/// A filled circle `diameter` wide.
pub fn dot(diameter: f64, color: Color) -> Retained<CALayer> {
    let layer = CALayer::new();
    layer.setBounds(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(diameter, diameter),
    ));
    layer.setCornerRadius(diameter / 2.0);
    layer.setBackgroundColor(Some(&cg(color)));
    layer
}

/// Fades `layer` from what shows now to `to` over `duration`, after `delay`.
pub fn fade_to(layer: &CALayer, to: f64, duration: f64, easing: [f64; 4], delay: f64) {
    let from = shown_number(layer, "opacity", f64::from(layer.opacity()));
    layer.setOpacity(to as f32);
    add(
        layer,
        "opacity",
        &tween(
            "opacity",
            &number(from),
            &number(to),
            duration,
            easing,
            delay,
        ),
    );
}

/// Recolors a layer's background from what shows now, over `duration`.
pub fn recolor(layer: &CALayer, to: Color, duration: f64, easing: [f64; 4]) {
    // SAFETY: the presentation layer is a read-only copy of this one.
    let from = unsafe { layer.presentationLayer() }
        .and_then(|presentation| presentation.backgroundColor())
        .or_else(|| layer.backgroundColor());
    let target = cg(to);
    layer.setBackgroundColor(Some(&target));
    if let Some(from) = from {
        add(
            layer,
            "backgroundColor",
            &tween(
                "backgroundColor",
                color_value(&from),
                color_value(&target),
                duration,
                easing,
                0.0,
            ),
        );
    }
}

/// Moves a layer from where it shows now to `to` along `spring`.
pub fn spring_position(layer: &CALayer, to: NSPoint, curve: Spring) {
    let from = shown_position(layer);
    layer.setPosition(to);
    let travel = (to.x - from.x).hypot(to.y - from.y);
    if travel < 0.01 {
        return;
    }
    // SAFETY: plain value boxes of two points.
    let (from, target) = unsafe {
        (
            objc2_foundation::NSValue::valueWithPoint(from),
            objc2_foundation::NSValue::valueWithPoint(to),
        )
    };
    let duration = curve.settle_time(travel, 0.1, 0.0);
    add(
        layer,
        "position",
        &spring("position", &from, &target, curve, 0.0, duration),
    );
}

/// Calls `fire` once, `seconds` from now, on the main run loop in every mode.
pub fn after(seconds: f64, fire: impl Fn() + 'static) -> Retained<NSTimer> {
    timer(seconds, false, fire)
}

pub fn every(seconds: f64, fire: impl Fn() + 'static) -> Retained<NSTimer> {
    timer(seconds, true, fire)
}

fn timer(seconds: f64, repeats: bool, fire: impl Fn() + 'static) -> Retained<NSTimer> {
    let block = RcBlock::new(move |_: NonNull<NSTimer>| fire());
    // SAFETY: the timer is added to the main run loop below and fires only on this, the main,
    // thread, so the block never crosses threads.
    let timer = unsafe { NSTimer::timerWithTimeInterval_repeats_block(seconds, repeats, &block) };
    // SAFETY: the common modes keep it firing while a menu or a drag is tracking.
    unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
    timer
}

/// Runs `work` on the main queue after the current event, once.
pub fn later(work: impl FnOnce() + 'static) {
    let slot = Cell::new(Some(work));
    let block = RcBlock::new(move || {
        if let Some(work) = slot.take() {
            work();
        }
    });
    // SAFETY: the main queue runs the block on this, the main, thread.
    unsafe { NSOperationQueue::mainQueue().addOperationWithBlock(&block) };
}
