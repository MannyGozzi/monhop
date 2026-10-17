//! Core Animation and AppKit building blocks for the island: colors, outlines as paths, text,
//! symbols, animations and timers. Main thread only.

use std::{cell::Cell, ffi::c_void, ops::Deref, ptr::NonNull};

use block2::RcBlock;
use objc2::{AnyThread, rc::Retained, runtime::AnyObject};
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
    CABasicAnimation, CACurrentMediaTime, CALayer, CAMediaTiming, CAMediaTimingFunction,
    CASpringAnimation, CATextLayer, CATransaction, CATransition, kCAAlignmentLeft,
    kCAAlignmentRight, kCAFillModeBackwards, kCATransitionFade, kCATruncationEnd,
};

use super::super::{
    geometry::{Outline, Point, Rect},
    tokens::{Color, Spring},
};

/// The blur filter's name, and the key path of its radius.
pub const BLUR: &str = "blur";
pub const BLUR_RADIUS: &str = "filters.blur.inputRadius";

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
    let (width, height) = (area.width.max(1.0), area.height.max(1.0));
    layer.setBounds(NSRect::new(
        NSPoint::new(area.x, area.y),
        NSSize::new(width, height),
    ));
    layer.setAnchorPoint(NSPoint::new(-area.x / width, -area.y / height));
    layer.setPosition(NSPoint::new(0.0, 0.0));
    layer
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
pub fn read_outline(path: &CGPath, like: &Outline) -> Option<Outline> {
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
    let mut points = like.points;
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
    Some(Outline {
        silhouette: like.silhouette,
        points,
    })
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
    if delay > 0.0 {
        animation.setBeginTime(now() + delay);
        // SAFETY: a constant QuartzCore exports.
        animation.setFillMode(unsafe { kCAFillModeBackwards });
    }
    animation
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

/// A crossfade of whatever `layer` draws next, over `duration`.
pub fn crossfade(layer: &CALayer, duration: f64, easing: [f64; 4]) {
    let fade = CATransition::new();
    // SAFETY: a constant QuartzCore exports.
    fade.setType(unsafe { kCATransitionFade });
    fade.setDuration(duration);
    fade.setTimingFunction(Some(&ease(easing)));
    add(layer, "contents", &fade);
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

/// Replaces a text layer's words with a crossfade, or at once under reduced motion.
pub fn set_text(layer: &CATextLayer, text: &str, fade: Option<(f64, [f64; 4])>) {
    if let Some((duration, easing)) = fade {
        crossfade(layer, duration, easing);
    }
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

/// Swaps a glyph layer's symbol, crossfading unless `fade` is None.
pub fn set_glyph(layer: &CALayer, image: &NSImage, scale: f64, fade: Option<(f64, [f64; 4])>) {
    let size = image.size();
    if let Some((duration, easing)) = fade {
        crossfade(layer, duration, easing);
    }
    layer.setBounds(NSRect::new(NSPoint::new(0.0, 0.0), size));
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
    // SAFETY: the presentation layer is a read-only copy of this one.
    let from =
        unsafe { layer.presentationLayer() }.map_or(layer.position(), |shown| shown.position());
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
