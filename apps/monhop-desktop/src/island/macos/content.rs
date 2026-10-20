//! What the island shows inside its outline: a compact activity's wings. Layout is in island
//! coordinates (origin at the top center, y up); every value comes from the stylesheet's tokens.

use objc2::rc::Retained;
use objc2_app_kit::{NSFont, NSImage};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{
    CALayer, CAMediaTiming, CAShapeLayer, CATextLayer, kCAMediaTimingFunctionLinear,
};

use super::{
    super::{
        geometry::{Placement, Rect, shoulder},
        model::{Activity, ActivityId, Leading, Symbol, Tone},
        tokens::{Color, IslandTokens},
    },
    draw::{self, cg},
};

const SCALE: &str = "transform.scale";
/// A Gaussian blur shows out to about this many radii; a band leaves that room around its content.
const BLUR_REACH: f64 = 3.0;

/// Everything layout reads: tokens, where the island sits, the backing scale, and the font.
pub struct Look {
    pub tokens: &'static IslandTokens,
    pub place: Placement,
    pub reduce: bool,
    title: Retained<NSFont>,
}

impl Look {
    pub fn new(tokens: &'static IslandTokens, place: Placement, reduce: bool) -> Self {
        Self {
            tokens,
            place,
            reduce,
            title: draw::font(tokens.text_base, tokens.weight_medium),
        }
    }

    pub fn scale(&self) -> f64 {
        self.place.scale
    }

    fn tone(&self, tone: Tone) -> Color {
        let tokens = self.tokens;
        match tone {
            Tone::Success => tokens.success,
            Tone::Warning => tokens.warning,
            Tone::Destructive => tokens.destructive,
        }
    }

    fn symbol(&self, symbol: Symbol) -> Option<Retained<NSImage>> {
        let tokens = self.tokens;
        draw::symbol(symbol.system_name(), tokens.text_base, tokens.weight_medium)
    }

    /// A symbol in the text's color, or an empty layer if the system lacks the symbol.
    fn glyph(&self, symbol: Symbol) -> Retained<CALayer> {
        match self.symbol(symbol) {
            Some(image) => draw::glyph(&image, self.tokens.foreground, self.scale()),
            None => CALayer::new(),
        }
    }

    /// The vertical middle of the notch's band, where a compact island sits.
    fn band_middle(&self) -> f64 {
        -self.place.idle.height / 2.0
    }

    /// From the outline's side, past its shoulder, to the first content of a compact island.
    fn compact_outer(&self) -> f64 {
        shoulder(self.place.idle.height) + self.tokens.gap_3
    }
}

/// Content arriving: it fades in still soft, then grows on the spring and comes into focus,
/// `delay` seconds from now.
pub fn materialize(look: &Look, layer: &CALayer, delay: f64) {
    let tokens = look.tokens;
    layer.setOpacity(1.0);
    let fade = draw::tween(
        "opacity",
        &draw::number(0.0),
        &draw::number(1.0),
        tokens.base,
        tokens.ease_out,
        delay,
    );
    draw::add(layer, "opacity", &fade);
    draw::focus(
        layer,
        tokens.blur_deep,
        tokens.focus,
        tokens.ease_focus,
        delay,
    );
    grow_to_size(look, layer, tokens.scale_defocus, delay);
}

/// Content leaving: it blurs, fades and sinks toward the notch quickly, ahead of the shape.
pub fn dissolve(look: &Look, layer: &CALayer) {
    let tokens = look.tokens;
    let blur = draw::shown_blur(layer);
    draw::blur_to(layer, blur, tokens.blur_deep, tokens.fast, tokens.ease_out);
    draw::fade_to(layer, 0.0, tokens.fast, tokens.ease_out, 0.0);
    scale_to(look, layer, tokens.scale_defocus, tokens.fast);
}

/// Leaving content called back: from wherever its exit had reached, back into focus.
pub fn refocus(look: &Look, layer: &CALayer) {
    let tokens = look.tokens;
    let blur = draw::shown_blur(layer);
    draw::focus(layer, blur, tokens.focus, tokens.ease_focus, 0.0);
    draw::fade_to(layer, 1.0, tokens.base, tokens.ease_out, 0.0);
    let scale = draw::shown_number(layer, SCALE, 1.0);
    grow_to_size(look, layer, scale, 0.0);
}

/// Ends a layer's entrance or exit where it lands: shown, sharp and full size.
pub fn rest(layer: &CALayer) {
    for key in ["opacity", SCALE, draw::BLUR_RADIUS] {
        draw::remove(layer, key);
    }
    layer.setOpacity(1.0);
    draw::set_number(layer, SCALE, 1.0);
    draw::set_blur(layer, None);
}

/// Scales a layer from `from` to full size on the window's spring.
fn grow_to_size(look: &Look, layer: &CALayer, from: f64, delay: f64) {
    let tokens = look.tokens;
    draw::set_number(layer, SCALE, 1.0);
    let grow = draw::spring(
        SCALE,
        &draw::number(from),
        &draw::number(1.0),
        tokens.grow_spring(),
        0.0,
        tokens.spring,
    );
    draw::delayed(&grow, delay);
    draw::add(layer, SCALE, &grow);
}

/// Scales a layer from its current scale to `to`.
fn scale_to(look: &Look, layer: &CALayer, to: f64, duration: f64) {
    let from = draw::shown_number(layer, SCALE, 1.0);
    draw::set_number(layer, SCALE, to);
    if look.reduce || (from - to).abs() < 1e-6 {
        return;
    }
    draw::add(
        layer,
        SCALE,
        &draw::tween(
            SCALE,
            &draw::number(from),
            &draw::number(to),
            duration,
            look.tokens.ease_out,
            0.0,
        ),
    );
}

/// Fades a layer out through a light blur from `blur`, what it shows now, then drops it.
fn blur_away(look: &Look, layer: &CALayer, blur: f64) {
    let tokens = look.tokens;
    draw::blur_to(layer, blur, tokens.blur, tokens.fast, tokens.ease_out);
    draw::fade_to(layer, 0.0, tokens.fast, tokens.ease_out, 0.0);
    draw::remove_later(layer, tokens.fast);
}

/// Fades a layer in through a light blur.
fn blur_in(look: &Look, layer: &CALayer) {
    let tokens = look.tokens;
    layer.setOpacity(1.0);
    let fade = draw::tween(
        "opacity",
        &draw::number(0.0),
        &draw::number(1.0),
        tokens.base,
        tokens.ease_out,
        0.0,
    );
    draw::add(layer, "opacity", &fade);
    draw::focus(layer, tokens.blur, tokens.base, tokens.ease_focus, 0.0);
}

/// New words through a light blur while a copy carries the old ones out, or at once under reduced
/// motion.
fn swap_text(look: &Look, layer: &CATextLayer, text: &str) {
    let Some(parent) = layer.superlayer().filter(|_| !look.reduce) else {
        draw::set_string(layer, text);
        return;
    };
    let blur = draw::shown_blur(layer);
    let ghost = draw::text_ghost(layer);
    parent.insertSublayer_below(&ghost, Some(&**layer));
    draw::set_string(layer, text);
    blur_away(look, &ghost, blur);
    blur_in(look, layer);
}

/// Room around `area` for a blur to spread inside its band.
fn padded(look: &Look, area: Rect) -> Rect {
    let room = look.tokens.blur_deep * BLUR_REACH;
    Rect::new(
        area.x - room,
        area.y - room,
        area.width + 2.0 * room,
        area.height + 2.0 * room,
    )
}

/// A compact activity's layers, kept so the same activity can take new words in place.
pub struct CompactParts {
    pub group: Retained<CALayer>,
    /// Holds both wings, which materialize together.
    pub band: Retained<CALayer>,
    id: ActivityId,
    leading: Retained<CALayer>,
    leading_now: Leading,
    text: Retained<CATextLayer>,
    text_now: String,
}

impl CompactParts {
    /// The same activity with the same left wing: only its words may change in place.
    pub fn continues(&self, activity: &Activity) -> bool {
        self.id == activity.id && self.leading_now == activity.leading
    }
}

fn leading_width(look: &Look, leading: &Leading) -> f64 {
    let tokens = look.tokens;
    match leading {
        Leading::Symbol(symbol) => look
            .symbol(*symbol)
            .map_or(tokens.icon_size, |image| image.size().width),
        Leading::Dot(_) => tokens.dot_size,
        Leading::Spinner => tokens.icon_size,
    }
}

/// Both wings take the wider side's width, so the island stays centered on the camera.
pub fn compact_wing(look: &Look, activity: &Activity) -> f64 {
    let tokens = look.tokens;
    let content =
        leading_width(look, &activity.leading).max(draw::measure(&activity.text, &look.title));
    (content + look.compact_outer() + tokens.gap_2).clamp(tokens.wing_min, tokens.wing_max)
}

fn make_leading(look: &Look, leading: &Leading) -> Retained<CALayer> {
    match leading {
        Leading::Symbol(symbol) => look.glyph(*symbol),
        Leading::Dot(tone) => draw::dot(look.tokens.dot_size, look.tone(*tone)),
        Leading::Spinner => spinner(look),
    }
}

/// An arc that turns, or holds still under reduced motion.
fn spinner(look: &Look) -> Retained<CALayer> {
    let tokens = look.tokens;
    let size = tokens.icon_size;
    let arc = CAShapeLayer::new();
    arc.setBounds(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(size, size)));
    let path = objc2_core_graphics::CGMutablePath::new();
    let radius = (size - tokens.stroke_bold) / 2.0;
    // SAFETY: a null transform adds the arc as given.
    unsafe {
        objc2_core_graphics::CGMutablePath::add_arc(
            Some(&path),
            std::ptr::null(),
            size / 2.0,
            size / 2.0,
            radius,
            0.0,
            std::f64::consts::PI * 1.5,
            false,
        );
    }
    arc.setPath(Some(&path));
    arc.setFillColor(None);
    arc.setStrokeColor(Some(&cg(tokens.foreground)));
    arc.setLineWidth(tokens.stroke_bold);
    // SAFETY: a constant QuartzCore exports.
    unsafe { arc.setLineCap(objc2_quartz_core::kCALineCapRound) };
    arc.setContentsScale(look.scale());
    if !look.reduce {
        let turn = objc2_quartz_core::CABasicAnimation::new();
        turn.setKeyPath(Some(&NSString::from_str("transform.rotation.z")));
        // SAFETY: rotation takes numbers.
        unsafe {
            turn.setFromValue(Some(&draw::number(0.0)));
            turn.setToValue(Some(&draw::number(-std::f64::consts::TAU)));
        }
        turn.setDuration(tokens.loop_spin);
        turn.setRepeatCount(f32::INFINITY);
        // SAFETY: a constant QuartzCore exports.
        turn.setTimingFunction(Some(
            &objc2_quartz_core::CAMediaTimingFunction::functionWithName(unsafe {
                kCAMediaTimingFunctionLinear
            }),
        ));
        draw::add(&arc, "spin", &turn);
    }
    arc.into_super()
}

/// A right-aligned line held by its right edge, so a new width never shifts the words.
fn make_text(look: &Look, text: &str) -> Retained<CATextLayer> {
    let layer = draw::text_layer(text, &look.title, look.tokens.foreground, look.scale());
    layer.setAnchorPoint(NSPoint::new(1.0, 0.5));
    layer
}

/// Places the wings' content for an island whose wings are `wing` wide. `animated` springs a
/// changed position instead of jumping.
fn lay_out_compact(look: &Look, parts: &CompactParts, wing: f64, animated: bool) {
    let tokens = look.tokens;
    let half = look.place.compact(wing).width / 2.0;
    let outer = look.compact_outer();
    let middle = look.band_middle();
    let room = (wing - outer - tokens.gap_2).max(0.0);
    let curve = tokens.grow_spring();
    let place = |layer: &CALayer, center_x: f64, width: f64, height: f64| {
        let frame = NSRect::new(
            NSPoint::new(center_x - width / 2.0, middle - height / 2.0),
            NSSize::new(width, height),
        );
        let anchor = layer.anchorPoint();
        let position = NSPoint::new(
            frame.origin.x + anchor.x * width,
            frame.origin.y + anchor.y * height,
        );
        if animated && !look.reduce && layer.position() != position {
            layer.setBounds(NSRect::new(NSPoint::new(0.0, 0.0), frame.size));
            draw::spring_position(layer, position, curve);
        } else {
            layer.setFrame(frame);
        }
    };
    let bounds = parts.leading.bounds();
    let leading_w = match parts.leading_now {
        Leading::Dot(_) => tokens.dot_size,
        Leading::Spinner => tokens.icon_size,
        Leading::Symbol(_) => bounds.size.width,
    };
    let leading_h = bounds.size.height.max(leading_w.min(tokens.icon_size));
    place(
        &parts.leading,
        -half + outer + leading_w / 2.0,
        leading_w,
        leading_h,
    );
    let width = draw::measure(&parts.text_now, &look.title).min(room);
    let height = draw::line_height(&look.title);
    place(&parts.text, half - outer - width / 2.0, width, height);
}

pub fn build_compact(look: &Look, activity: &Activity, wing: f64) -> CompactParts {
    let place = look.place;
    let area = place.outline(place.compact(wing)).bounds();
    let group = draw::group(area);
    let band = draw::group(padded(look, area));
    group.addSublayer(&band);
    let leading = make_leading(look, &activity.leading);
    let text = make_text(look, &activity.text);
    band.addSublayer(&leading);
    band.addSublayer(&text);
    let parts = CompactParts {
        group,
        band,
        id: activity.id,
        leading,
        leading_now: activity.leading,
        text,
        text_now: activity.text.clone(),
    };
    draw::quietly(|| lay_out_compact(look, &parts, wing, false));
    parts
}

/// The same activity with new words: they cross through a blur and the wings glide to the new width.
pub fn update_compact(look: &Look, parts: &mut CompactParts, activity: &Activity, wing: f64) {
    if parts.text_now != activity.text {
        swap_text(look, &parts.text, &activity.text);
        parts.text_now.clone_from(&activity.text);
    }
    let area = look.place.outline(look.place.compact(wing)).bounds();
    draw::span(&parts.group, area);
    draw::span(&parts.band, padded(look, area));
    lay_out_compact(look, parts, wing, true);
}

/// Whether a layer is growing or shrinking now.
pub fn scaling(layer: &CALayer) -> bool {
    draw::animating(layer, SCALE)
}

/// The compact island at rest holds the same layers a fresh build of `activity` would, with no
/// filter left on any; for the island check.
pub fn audit_compact(look: &Look, parts: &CompactParts, activity: &Activity) -> Result<(), String> {
    let fresh = build_compact(look, activity, compact_wing(look, activity));
    if draw::shape(&parts.group) != draw::shape(&fresh.group) {
        return Err("the compact island holds other layers than a fresh one".to_owned());
    }
    draw::unfiltered(&parts.group)
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard, PoisonError};

    use objc2_quartz_core::CATransaction;

    use super::{
        super::super::{geometry::Size, tokens},
        *,
    };

    /// A transaction committed on any thread ends the animations of every layer outside a
    /// window, so layer tests take turns and commit their own before handing over.
    struct Turn {
        _lock: MutexGuard<'static, ()>,
    }

    impl Drop for Turn {
        fn drop(&mut self) {
            CATransaction::flush();
        }
    }

    fn turn() -> Turn {
        static LAYERS: Mutex<()> = Mutex::new(());
        Turn {
            _lock: LAYERS.lock().unwrap_or_else(PoisonError::into_inner),
        }
    }

    /// No window is needed: off the main thread the island's timers never fire.
    fn look(reduce: bool) -> Look {
        let place = Placement {
            center_x: 0.0,
            top: 0.0,
            core: 200.0,
            idle: Size::new(200.0, 32.0),
            scale: 2.0,
        };
        Look::new(tokens::tokens().expect("the tokens parse"), place, reduce)
    }

    fn lost(name: &str) -> Activity {
        Activity {
            id: ActivityId::Dropped,
            leading: Leading::Dot(Tone::Destructive),
            text: format!("Lost {name}"),
        }
    }

    #[test]
    fn new_words_in_place_audit_as_a_fresh_build_without_scaling_the_band() {
        let _turn = turn();
        let moving = look(false);
        let (before, after) = (lost("PC"), lost("Desk PC"));
        let mut parts = build_compact(&moving, &before, compact_wing(&moving, &before));
        let wing = compact_wing(&moving, &after);
        assert!(
            wing > compact_wing(&moving, &before),
            "longer words widen the wings"
        );
        update_compact(&moving, &mut parts, &after, wing);
        assert!(
            !scaling(&parts.band),
            "words change in place, the band stays"
        );
        let still = look(true);
        let mut parts = build_compact(&still, &before, compact_wing(&still, &before));
        update_compact(&still, &mut parts, &after, wing);
        assert_eq!(audit_compact(&still, &parts, &after), Ok(()));
    }

    #[test]
    fn only_the_same_activity_with_the_same_left_wing_continues() {
        let _turn = turn();
        let look = look(true);
        let shown = lost("PC");
        let parts = build_compact(&look, &shown, compact_wing(&look, &shown));
        assert!(parts.continues(&lost("Desk PC")));
        assert!(!parts.continues(&Activity {
            leading: Leading::Spinner,
            ..lost("PC")
        }));
        assert!(!parts.continues(&Activity {
            id: ActivityId::Connected,
            ..lost("PC")
        }));
    }
}
