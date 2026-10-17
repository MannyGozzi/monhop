//! What the island shows inside its outline: a compact activity's wings, and the open island's
//! header, rows and controls. Layout is in island coordinates (origin at the top center, y up);
//! every value comes from the stylesheet's tokens.

use std::{iter, time::Instant};

use objc2::{MainThreadMarker, Message, rc::Retained};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityButtonRole, NSAccessibilityCheckBoxRole,
    NSAccessibilitySliderRole, NSAccessibilitySwitchSubrole, NSFont, NSImage,
};
use objc2_foundation::{NSNumber, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{
    CAGradientLayer, CALayer, CAMediaTiming, CAShapeLayer, CATextLayer,
    kCAMediaTimingFunctionLinear,
};

use super::{
    super::{
        geometry::{Placement, Rect, Size, shoulder},
        model::{Activity, Expanded, Leading, Row, Symbol, Tone, Trailing, status_words},
        tokens::{Color, IslandTokens},
    },
    classes::ControlElement,
    draw::{self, cg},
};

const SCALE: &str = "transform.scale";
/// A Gaussian blur shows out to about this many radii; a band leaves that room around its content.
const BLUR_REACH: f64 = 3.0;

/// Everything layout reads: tokens, where the island sits, the backing scale, and the fonts.
pub struct Look {
    pub tokens: &'static IslandTokens,
    pub place: Placement,
    pub reduce: bool,
    title: Retained<NSFont>,
    brand: Retained<NSFont>,
    detail: Retained<NSFont>,
    status: Retained<NSFont>,
    /// The widest header status, so a new status never resizes the open island.
    status_room: f64,
}

impl Look {
    pub fn new(tokens: &'static IslandTokens, place: Placement, reduce: bool) -> Self {
        let status = draw::font(tokens.text_sm, tokens.weight_medium);
        let status_room = status_words()
            .map(|word| draw::measure(word, &status))
            .fold(0.0, f64::max);
        Self {
            tokens,
            place,
            reduce,
            title: draw::font(tokens.text_base, tokens.weight_medium),
            brand: draw::font(tokens.text_base, tokens.weight_semibold),
            detail: draw::font(tokens.text_sm, tokens.weight_regular),
            status,
            status_room,
        }
    }

    pub fn scale(&self) -> f64 {
        self.place.scale
    }

    pub fn tone(&self, tone: Tone) -> Color {
        let tokens = self.tokens;
        match tone {
            Tone::Foreground => tokens.foreground,
            Tone::Subtle => tokens.subtle,
            Tone::Success => tokens.success,
            Tone::Warning => tokens.warning,
            Tone::Destructive => tokens.destructive,
        }
    }

    fn symbol(&self, symbol: Symbol, size: f64, weight: f64) -> Option<Retained<NSImage>> {
        draw::symbol(symbol.system_name(), size, weight)
    }

    /// A tinted symbol layer, or an empty one if the system lacks the symbol.
    fn glyph(&self, symbol: Symbol, size: f64, color: Color) -> Retained<CALayer> {
        match self.symbol(symbol, size, self.tokens.weight_medium) {
            Some(image) => draw::glyph(&image, color, self.scale()),
            None => CALayer::new(),
        }
    }

    /// The vertical middle of the notch's band, where a compact island and the open header sit.
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

/// Gives `live` its new look through a light blur while `ghost`, showing the old one, leaves;
/// `blur` is what the old look shows now.
fn crossblur(look: &Look, live: &CALayer, ghost: &CALayer, blur: f64) {
    let tokens = look.tokens;
    draw::blur_to(ghost, blur, tokens.blur, tokens.fast, tokens.ease_out);
    draw::fade_to(ghost, 0.0, tokens.fast, tokens.ease_out, 0.0);
    draw::remove_later(ghost, tokens.fast);
    live.setOpacity(1.0);
    let fade = draw::tween(
        "opacity",
        &draw::number(0.0),
        &draw::number(1.0),
        tokens.base,
        tokens.ease_out,
        0.0,
    );
    draw::add(live, "opacity", &fade);
    draw::focus(live, tokens.blur, tokens.base, tokens.ease_focus, 0.0);
}

/// New words through a light blur, or at once under reduced motion.
fn swap_text(look: &Look, layer: &CATextLayer, text: &str) {
    let Some(parent) = layer.superlayer().filter(|_| !look.reduce) else {
        draw::set_string(layer, text);
        return;
    };
    let blur = draw::shown_blur(layer);
    let ghost = draw::text_ghost(layer);
    parent.insertSublayer_below(&ghost, Some(&**layer));
    draw::set_string(layer, text);
    crossblur(look, layer, &ghost, blur);
}

/// A new symbol through a light blur, or at once under reduced motion.
fn swap_glyph(look: &Look, layer: &CALayer, image: &NSImage) {
    let Some(parent) = layer.superlayer().filter(|_| !look.reduce) else {
        draw::set_glyph(layer, image, look.scale());
        return;
    };
    let blur = draw::shown_blur(layer);
    let ghost = draw::hand_over_glyph(layer, image, look.scale());
    parent.insertSublayer_below(&ghost, Some(layer));
    crossblur(look, layer, &ghost, blur);
}

/// Moves a dot through a light blur, leaving where it was and arriving at `center`, so it never
/// slides across the words beside it.
fn swap_dot(look: &Look, dot: &CALayer, center: NSPoint) {
    let Some(parent) = dot
        .superlayer()
        .filter(|_| !look.reduce && dot.position() != center)
    else {
        dot.setPosition(center);
        return;
    };
    let blur = draw::shown_blur(dot);
    let ghost = draw::ghost(dot);
    parent.insertSublayer_below(&ghost, Some(dot));
    draw::remove(dot, "position");
    dot.setPosition(center);
    crossblur(look, dot, &ghost, blur);
}

/// Puts `next` where `old` was, crossing through a light blur.
fn swap_in(look: &Look, holder: &CALayer, old: &CALayer, next: &CALayer) {
    holder.addSublayer(next);
    if look.reduce {
        old.removeFromSuperlayer();
        return;
    }
    crossblur(look, next, old, draw::shown_blur(old));
}

/// A layer's fill takes `color` over the base time, or at once under reduced motion.
fn recolor(look: &Look, layer: &CALayer, color: Color) {
    if look.reduce {
        layer.setBackgroundColor(Some(&cg(color)));
    } else {
        draw::recolor(layer, color, look.tokens.base, look.tokens.ease_out);
    }
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

/// A layer, in island coordinates, for content that materializes as one.
fn band(look: &Look, area: Rect) -> Retained<CALayer> {
    draw::group(padded(look, area))
}

fn frame_centered(layer: &CALayer, center_x: f64, center_y: f64, width: f64, height: f64) {
    layer.setFrame(NSRect::new(
        NSPoint::new(center_x - width / 2.0, center_y - height / 2.0),
        NSSize::new(width, height),
    ));
}

fn layer_size(layer: &CALayer) -> (f64, f64) {
    let bounds = layer.bounds();
    (bounds.size.width, bounds.size.height)
}

/// The two-dot link mark, centered on (0, 0) of its own layer.
fn link_mark(look: &Look, local: Color, peer: Color) -> Retained<CALayer> {
    let tokens = look.tokens;
    let mark = CALayer::new();
    let size = tokens.sharing_mark_size;
    mark.setBounds(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(size, size)));
    let dot = tokens.sharing_dot_size;
    let reach = tokens.orbit_rest;
    for (offset, color) in [(-reach, local), (reach, peer)] {
        let piece = draw::dot(dot, color);
        frame_centered(&piece, size / 2.0 + offset, size / 2.0, dot, dot);
        mark.addSublayer(&piece);
    }
    mark
}

enum TrailingParts {
    Text(Retained<CATextLayer>),
    Level {
        track: Retained<CALayer>,
        fill: Retained<CALayer>,
    },
    Labeled {
        glyph: Retained<CALayer>,
        text: Retained<CATextLayer>,
    },
}

/// A compact activity's layers, kept so the same activity can change in place.
pub struct CompactParts {
    pub group: Retained<CALayer>,
    /// Holds both wings, which materialize together.
    band: Retained<CALayer>,
    leading: Retained<CALayer>,
    leading_now: Leading,
    trailing: TrailingParts,
    trailing_now: Trailing,
}

impl CompactParts {
    pub fn bands(&self) -> Vec<&CALayer> {
        vec![&self.band]
    }
}

fn leading_width(look: &Look, leading: &Leading) -> f64 {
    let tokens = look.tokens;
    match leading {
        Leading::Symbol(symbol, _) => look
            .symbol(*symbol, tokens.text_base, tokens.weight_medium)
            .map_or(tokens.icon_size, |image| image.size().width),
        Leading::Dot(_) => tokens.dot_size,
        Leading::Spinner => tokens.icon_size,
    }
}

fn trailing_width(look: &Look, trailing: &Trailing) -> f64 {
    let tokens = look.tokens;
    match trailing {
        Trailing::Text(text) => draw::measure(text, &look.title),
        Trailing::Level(_) => tokens.level_width,
        Trailing::Labeled(symbol, text) => {
            leading_width(look, &Leading::Symbol(*symbol, Tone::Foreground))
                + tokens.gap_1
                + draw::measure(text, &look.title)
        }
    }
}

/// Both wings take the wider side's width, so the island stays centered on the camera.
pub fn compact_wing(look: &Look, activity: &Activity) -> f64 {
    let tokens = look.tokens;
    let content =
        leading_width(look, &activity.leading).max(trailing_width(look, &activity.trailing));
    (content + look.compact_outer() + tokens.gap_2).clamp(tokens.wing_min, tokens.wing_max)
}

fn make_leading(look: &Look, leading: &Leading) -> Retained<CALayer> {
    let tokens = look.tokens;
    match leading {
        Leading::Symbol(symbol, tone) => look.glyph(*symbol, tokens.text_base, look.tone(*tone)),
        Leading::Dot(tone) => draw::dot(tokens.dot_size, look.tone(*tone)),
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
fn pinned_right(layer: Retained<CATextLayer>) -> Retained<CATextLayer> {
    layer.setAnchorPoint(NSPoint::new(1.0, 0.5));
    layer
}

fn make_trailing(look: &Look, trailing: &Trailing) -> TrailingParts {
    let tokens = look.tokens;
    match trailing {
        Trailing::Text(text) => TrailingParts::Text(pinned_right(draw::text_layer(
            text,
            &look.title,
            tokens.foreground,
            true,
            look.scale(),
        ))),
        Trailing::Level(level) => {
            let track = CALayer::new();
            track.setBackgroundColor(Some(&cg(tokens.hairline_strong)));
            track.setCornerRadius(tokens.slider_track / 2.0);
            let fill = CALayer::new();
            fill.setBackgroundColor(Some(&cg(tokens.foreground)));
            fill.setCornerRadius(tokens.slider_track / 2.0);
            fill.setAnchorPoint(NSPoint::new(0.0, 0.5));
            track.addSublayer(&fill);
            let parts = TrailingParts::Level { track, fill };
            set_level_fill(look, &parts, *level, false);
            parts
        }
        Trailing::Labeled(symbol, text) => TrailingParts::Labeled {
            glyph: look.glyph(*symbol, tokens.text_base, tokens.foreground),
            text: pinned_right(draw::text_layer(
                text,
                &look.title,
                tokens.foreground,
                true,
                look.scale(),
            )),
        },
    }
}

fn set_level_fill(look: &Look, parts: &TrailingParts, level: f64, animated: bool) {
    let TrailingParts::Level { fill, .. } = parts else {
        return;
    };
    let tokens = look.tokens;
    let width = tokens.level_width * level.clamp(0.0, 1.0);
    let from = draw::shown_number(fill, "bounds.size.width", fill.bounds().size.width);
    fill.setBounds(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(width, tokens.slider_track),
    ));
    fill.setPosition(NSPoint::new(0.0, tokens.slider_track / 2.0));
    if animated && !look.reduce && (from - width).abs() > 0.01 {
        let curve = if width > from {
            tokens.grow_spring()
        } else {
            tokens.shrink_spring()
        };
        let duration = curve.settle_time((width - from).abs(), 0.1, 0.0);
        let animation = draw::spring(
            "bounds.size.width",
            &draw::number(from),
            &draw::number(width),
            curve,
            0.0,
            duration,
        );
        draw::add(fill, "bounds.size.width", &animation);
    }
}

fn trailing_layers(parts: &TrailingParts) -> Vec<&CALayer> {
    match parts {
        TrailingParts::Text(text) => vec![text],
        TrailingParts::Level { track, .. } => vec![track],
        TrailingParts::Labeled { glyph, text } => vec![glyph, text],
    }
}

/// Places the wings' content for an island whose wings are `wing` wide. `animated` springs a
/// changed position instead of jumping, except for the `fresh` layers, which start in place.
fn lay_out_compact(
    look: &Look,
    parts: &CompactParts,
    wing: f64,
    animated: bool,
    fresh: &[Retained<CALayer>],
) {
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
        let new = fresh.iter().any(|made| std::ptr::eq(&**made, layer));
        if animated && !look.reduce && !new && layer.position() != position {
            layer.setBounds(NSRect::new(NSPoint::new(0.0, 0.0), frame.size));
            draw::spring_position(layer, position, curve);
        } else {
            layer.setFrame(frame);
        }
    };
    let (leading_w, leading_h) = layer_size(&parts.leading);
    let leading_w = match parts.leading_now {
        Leading::Dot(_) => tokens.dot_size,
        Leading::Spinner => tokens.icon_size,
        Leading::Symbol(..) => leading_w,
    };
    let leading_h = leading_h.max(leading_w.min(tokens.icon_size));
    place(
        &parts.leading,
        -half + outer + leading_w / 2.0,
        leading_w,
        leading_h,
    );
    let right = half - outer;
    match &parts.trailing {
        TrailingParts::Text(text) => {
            let width = trailing_width(look, &parts.trailing_now).min(room);
            let height = draw::line_height(&look.title);
            place(text, right - width / 2.0, width, height);
        }
        TrailingParts::Level { track, .. } => {
            let width = tokens.level_width.min(room);
            place(track, right - width / 2.0, width, tokens.slider_track);
        }
        TrailingParts::Labeled { glyph, text } => {
            let (glyph_w, glyph_h) = layer_size(glyph);
            let text_w = (trailing_width(look, &parts.trailing_now) - glyph_w - tokens.gap_1)
                .min(room - glyph_w - tokens.gap_1)
                .max(0.0);
            let height = draw::line_height(&look.title);
            place(text, right - text_w / 2.0, text_w, height);
            place(
                glyph,
                right - text_w - tokens.gap_1 - glyph_w / 2.0,
                glyph_w,
                glyph_h,
            );
        }
    }
}

pub fn build_compact(look: &Look, activity: &Activity, wing: f64) -> CompactParts {
    let place = look.place;
    let area = place.outline(place.compact(wing)).bounds();
    let group = draw::group(area);
    let band = band(look, area);
    group.addSublayer(&band);
    let leading = make_leading(look, &activity.leading);
    let trailing = make_trailing(look, &activity.trailing);
    band.addSublayer(&leading);
    for layer in trailing_layers(&trailing) {
        band.addSublayer(layer);
    }
    let parts = CompactParts {
        group,
        band,
        leading,
        leading_now: activity.leading,
        trailing,
        trailing_now: activity.trailing.clone(),
    };
    draw::quietly(|| lay_out_compact(look, &parts, wing, false, &[]));
    parts
}

/// The same activity with new content: only what changed moves or crosses through a blur.
pub fn update_compact(look: &Look, parts: &mut CompactParts, activity: &Activity, wing: f64) {
    let tokens = look.tokens;
    let mut fresh: Vec<Retained<CALayer>> = Vec::new();
    if parts.leading_now != activity.leading {
        match (parts.leading_now, activity.leading) {
            (Leading::Symbol(shown, _), Leading::Symbol(symbol, tone)) => {
                let color = look.tone(tone);
                let image = (shown != symbol)
                    .then(|| look.symbol(symbol, tokens.text_base, tokens.weight_medium))
                    .flatten();
                match image {
                    Some(image) => {
                        swap_glyph(look, &parts.leading, &image);
                        parts.leading.setBackgroundColor(Some(&cg(color)));
                    }
                    None => recolor(look, &parts.leading, color),
                }
            }
            _ => {
                let next = make_leading(look, &activity.leading);
                swap_in(look, &parts.band, &parts.leading, &next);
                fresh.push(next.clone());
                parts.leading = next;
            }
        }
        parts.leading_now = activity.leading;
    }
    if parts.trailing_now != activity.trailing {
        match (&parts.trailing, &activity.trailing) {
            (TrailingParts::Text(layer), Trailing::Text(text)) => swap_text(look, layer, text),
            (TrailingParts::Level { .. }, Trailing::Level(level)) => {
                set_level_fill(look, &parts.trailing, *level, true);
            }
            (TrailingParts::Labeled { glyph, text: layer }, Trailing::Labeled(symbol, text)) => {
                if !matches!(&parts.trailing_now, Trailing::Labeled(shown, _) if shown == symbol)
                    && let Some(image) =
                        look.symbol(*symbol, tokens.text_base, tokens.weight_medium)
                {
                    swap_glyph(look, glyph, &image);
                }
                if !matches!(&parts.trailing_now, Trailing::Labeled(_, shown) if shown == text) {
                    swap_text(look, layer, text);
                }
            }
            _ => {
                let next = make_trailing(look, &activity.trailing);
                let old = std::mem::replace(&mut parts.trailing, next);
                for (old, new) in trailing_layers(&old)
                    .into_iter()
                    .zip(trailing_layers(&parts.trailing))
                {
                    swap_in(look, &parts.band, old, new);
                }
                for extra in trailing_layers(&old)
                    .into_iter()
                    .skip(trailing_layers(&parts.trailing).len())
                {
                    extra.removeFromSuperlayer();
                }
                for extra in trailing_layers(&parts.trailing)
                    .into_iter()
                    .skip(trailing_layers(&old).len())
                {
                    parts.band.addSublayer(extra);
                }
                fresh.extend(
                    trailing_layers(&parts.trailing)
                        .into_iter()
                        .map(CALayer::retain),
                );
            }
        }
        parts.trailing_now = activity.trailing.clone();
    }
    let area = look.place.outline(look.place.compact(wing)).bounds();
    draw::span(&parts.group, area);
    draw::span(&parts.band, padded(look, area));
    lay_out_compact(look, parts, wing, true, &fresh);
}

/// The open island's controls, by what they change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlId {
    Computer(String),
    Dimming,
    Level,
    Clipboard,
    OpenWindow,
    SetUp,
}

pub enum Widget {
    Switch {
        track: Retained<CALayer>,
        thumb: Retained<CALayer>,
        on: bool,
    },
    Slider {
        fill: Retained<CALayer>,
        thumb: Retained<CALayer>,
        width: f64,
        level: u8,
        min: u8,
        max: u8,
    },
    Button {
        plate: Retained<CALayer>,
    },
}

/// A value the user just set, kept on screen until the app reports it or it times out.
pub struct Pending {
    pub value: u8,
    pub until: Instant,
}

pub struct Control {
    pub id: ControlId,
    /// The layer that scales when pressed.
    pub root: Retained<CALayer>,
    pub widget: Widget,
    /// Where a press counts, in island coordinates.
    pub hit: Rect,
    pub label: String,
    pub pending: Option<Pending>,
    pub element: Option<Retained<ControlElement>>,
    /// The row it sits in; none in the header.
    row: Option<RowKey>,
}

impl Control {
    pub fn value(&self) -> u8 {
        match &self.widget {
            Widget::Switch { on, .. } => u8::from(*on),
            Widget::Slider { level, .. } => *level,
            Widget::Button { .. } => 0,
        }
    }
}

fn switch(look: &Look, on: bool) -> (Retained<CALayer>, Widget) {
    let tokens = look.tokens;
    let track = CALayer::new();
    track.setBounds(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(tokens.switch_width, tokens.switch_height),
    ));
    track.setCornerRadius(tokens.switch_height / 2.0);
    track.setBackgroundColor(Some(&cg(if on {
        tokens.primary_fill
    } else {
        tokens.switch_track_off
    })));
    let thumb = draw::dot(tokens.switch_thumb, tokens.thumb);
    shadow(look, &thumb);
    track.addSublayer(&thumb);
    thumb.setPosition(thumb_position(look, on));
    (track.clone(), Widget::Switch { track, thumb, on })
}

fn thumb_position(look: &Look, on: bool) -> NSPoint {
    let tokens = look.tokens;
    let inset = tokens.gap_half + tokens.switch_thumb / 2.0;
    NSPoint::new(
        if on {
            tokens.switch_width - inset
        } else {
            inset
        },
        tokens.switch_height / 2.0,
    )
}

fn shadow(look: &Look, layer: &CALayer) {
    let shadow = look.tokens.thumb_shadow;
    layer.setShadowColor(Some(&cg(shadow.color.with_alpha(1.0))));
    layer.setShadowOpacity(shadow.color.alpha as f32);
    layer.setShadowRadius(shadow.blur / 2.0);
    layer.setShadowOffset(NSSize::new(shadow.x, -shadow.y));
}

/// Moves a switch to `on`: the thumb on the window's spring, the track's color over the base time.
pub fn set_switch(look: &Look, widget: &mut Widget, on: bool) {
    let Widget::Switch {
        track,
        thumb,
        on: shown,
    } = widget
    else {
        return;
    };
    if *shown == on {
        return;
    }
    *shown = on;
    let tokens = look.tokens;
    let color = if on {
        tokens.primary_fill
    } else {
        tokens.switch_track_off
    };
    recolor(look, track, color);
    if look.reduce {
        thumb.setPosition(thumb_position(look, on));
    } else {
        draw::spring_position(thumb, thumb_position(look, on), tokens.grow_spring());
    }
}

fn slider(look: &Look, width: f64, level: u8, min: u8, max: u8) -> (Retained<CALayer>, Widget) {
    let tokens = look.tokens;
    let root = CALayer::new();
    root.setBounds(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(width, tokens.slider_thumb),
    ));
    let track = CALayer::new();
    track.setFrame(NSRect::new(
        NSPoint::new(0.0, (tokens.slider_thumb - tokens.slider_track) / 2.0),
        NSSize::new(width, tokens.slider_track),
    ));
    track.setCornerRadius(tokens.slider_track / 2.0);
    track.setBackgroundColor(Some(&cg(tokens.hairline_strong)));
    let fill = CALayer::new();
    fill.setAnchorPoint(NSPoint::new(0.0, 0.5));
    fill.setCornerRadius(tokens.slider_track / 2.0);
    fill.setBackgroundColor(Some(&cg(tokens.primary)));
    track.addSublayer(&fill);
    let thumb = draw::dot(tokens.slider_thumb, tokens.thumb);
    shadow(look, &thumb);
    root.addSublayer(&track);
    root.addSublayer(&thumb);
    let mut widget = Widget::Slider {
        fill,
        thumb,
        width,
        level,
        min,
        max,
    };
    draw::quietly(|| place_slider(look, &widget, false));
    if let Widget::Slider { level: shown, .. } = &mut widget {
        *shown = level;
    }
    (root, widget)
}

fn slider_share(level: u8, min: u8, max: u8) -> f64 {
    if max <= min {
        return 0.0;
    }
    (f64::from(level.clamp(min, max)) - f64::from(min)) / f64::from(max - min)
}

/// The level whose thumb center sits at `x` along the slider.
pub fn slider_level(look: &Look, widget: &Widget, x: f64) -> Option<u8> {
    let Widget::Slider {
        width, min, max, ..
    } = widget
    else {
        return None;
    };
    let thumb = look.tokens.slider_thumb;
    let share = ((x - thumb / 2.0) / (width - thumb).max(1.0)).clamp(0.0, 1.0);
    Some((f64::from(*min) + share * f64::from(max.saturating_sub(*min))).round() as u8)
}

fn place_slider(look: &Look, widget: &Widget, animated: bool) {
    let Widget::Slider {
        fill,
        thumb,
        width,
        level,
        min,
        max,
    } = widget
    else {
        return;
    };
    let tokens = look.tokens;
    let share = slider_share(*level, *min, *max);
    let travel = width - tokens.slider_thumb;
    let center = NSPoint::new(
        tokens.slider_thumb / 2.0 + travel * share,
        tokens.slider_thumb / 2.0,
    );
    let fill_width = center.x;
    if animated && !look.reduce {
        let curve = tokens.grow_spring();
        draw::spring_position(thumb, center, curve);
        let from = draw::shown_number(fill, "bounds.size.width", fill.bounds().size.width);
        fill.setBounds(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(fill_width, tokens.slider_track),
        ));
        let duration = curve.settle_time((fill_width - from).abs(), 0.1, 0.0);
        draw::add(
            fill,
            "bounds.size.width",
            &draw::spring(
                "bounds.size.width",
                &draw::number(from),
                &draw::number(fill_width),
                curve,
                0.0,
                duration,
            ),
        );
    } else {
        thumb.setPosition(center);
        fill.setBounds(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(fill_width, tokens.slider_track),
        ));
    }
    fill.setPosition(NSPoint::new(0.0, tokens.slider_track / 2.0));
}

/// Moves the slider to `level`; a drag follows the pointer at once, a reported change springs.
pub fn set_slider(look: &Look, widget: &mut Widget, level: u8, dragging: bool) {
    let Widget::Slider { level: shown, .. } = widget else {
        return;
    };
    if *shown == level {
        return;
    }
    *shown = level;
    if dragging {
        draw::quietly(|| place_slider(look, widget, false));
    } else {
        place_slider(look, widget, true);
    }
}

/// The thumb grows under the pointer and more while dragged, as the window's slider does.
pub fn set_thumb_scale(look: &Look, widget: &Widget, scale: f64) {
    let Widget::Slider { thumb, .. } = widget else {
        return;
    };
    scale_to(look, thumb, scale, look.tokens.spring);
}

/// Scales a layer from its current scale to `to`.
pub fn scale_to(look: &Look, layer: &CALayer, to: f64, duration: f64) {
    let from = draw::shown_number(layer, SCALE, 1.0);
    draw::set_number(layer, SCALE, to);
    if look.reduce || (from - to).abs() < 1e-6 {
        return;
    }
    let tokens = look.tokens;
    draw::add(
        layer,
        SCALE,
        &draw::tween(
            SCALE,
            &draw::number(from),
            &draw::number(to),
            duration,
            tokens.ease_out,
            0.0,
        ),
    );
}

fn button(look: &Look, symbol: Symbol) -> (Retained<CALayer>, Widget) {
    let tokens = look.tokens;
    let size = tokens.icon_button_size;
    let plate = CALayer::new();
    plate.setBounds(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(size, size)));
    plate.setCornerRadius(tokens.radius);
    plate.setBackgroundColor(Some(&cg(tokens.control_fill)));
    let glyph = look.glyph(symbol, tokens.icon_size, tokens.foreground);
    let (width, height) = layer_size(&glyph);
    frame_centered(&glyph, size / 2.0, size / 2.0, width, height);
    plate.addSublayer(&glyph);
    (plate.clone(), Widget::Button { plate })
}

pub fn set_button_hover(look: &Look, widget: &Widget, hovered: bool) {
    let Widget::Button { plate } = widget else {
        return;
    };
    let tokens = look.tokens;
    let color = if hovered {
        tokens.control_fill_hover
    } else {
        tokens.control_fill
    };
    if look.reduce {
        draw::quietly(|| plate.setBackgroundColor(Some(&cg(color))));
    } else {
        draw::recolor(plate, color, tokens.fast, tokens.ease_out);
    }
}

/// Which row is which across updates: a row whose key changes leaves and a new one arrives.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RowKey {
    Computer(String),
    SetUp,
    Dimming,
    Clipboard { detail: bool },
}

fn row_key(row: &Row) -> RowKey {
    match row {
        Row::Computer(computer) => RowKey::Computer(computer.fingerprint.clone()),
        Row::SetUp => RowKey::SetUp,
        Row::Dimming(_) => RowKey::Dimming,
        Row::Clipboard { detail, .. } => RowKey::Clipboard {
            detail: detail.is_some(),
        },
    }
}

/// For each new row, the old row it continues: the first unclaimed one with its key.
fn match_rows(old: &[RowKey], new: &[RowKey]) -> Vec<Option<usize>> {
    let mut claimed = vec![false; old.len()];
    new.iter()
        .map(|key| {
            let found = (0..old.len()).find(|&index| !claimed[index] && old[index] == *key);
            if let Some(index) = found {
                claimed[index] = true;
            }
            found
        })
        .collect()
}

struct RowTexts {
    title: Retained<CATextLayer>,
    detail: Option<Retained<CATextLayer>>,
    title_now: String,
    detail_now: Option<String>,
}

/// One row of the open island, in a band of its own.
struct RowParts {
    key: RowKey,
    band: Retained<CALayer>,
    /// Where the row was built and where it stands now; its band is moved by the difference.
    built_middle: f64,
    middle: f64,
    texts: RowTexts,
}

struct Header {
    band: Retained<CALayer>,
    status_dot: Retained<CALayer>,
    status_text: Retained<CATextLayer>,
    status_now: (String, Tone),
    /// Where the status word ends; the dot sits just before the word.
    status_right: f64,
}

/// The open island's layers.
pub struct OpenParts {
    pub group: Retained<CALayer>,
    pub size: Size,
    /// The inset the header and rows were laid out with; a different one needs a rebuild.
    pad: f64,
    header: Header,
    rows: Vec<RowParts>,
    pub controls: Vec<Control>,
}

impl OpenParts {
    /// Top to bottom, the order the content materializes in.
    pub fn bands(&self) -> Vec<&CALayer> {
        iter::once(&*self.header.band)
            .chain(self.rows.iter().map(|row| &*row.band))
            .collect()
    }
}

/// Measurements both the size and the build read, so they always agree.
struct OpenLayout {
    size: Size,
    pad: f64,
    /// Vertical middle of the header.
    header_middle: f64,
    /// Top of the first row.
    rows_top: f64,
    row_height: f64,
}

impl OpenLayout {
    fn half(&self) -> f64 {
        self.size.width / 2.0
    }

    /// The content's left and right edges, clear of the outline's shoulders.
    fn edges(&self) -> (f64, f64) {
        (-self.half() + self.pad, self.half() - self.pad)
    }

    fn row_middle(&self, index: usize) -> f64 {
        self.rows_top - self.row_height * (index as f64 + 0.5)
    }
}

fn header_widths(look: &Look) -> (f64, f64) {
    let tokens = look.tokens;
    let left = tokens.sharing_mark_size + tokens.gap_2 + draw::measure("MonHop", &look.brand);
    let right =
        tokens.dot_size + tokens.gap_2 + look.status_room + tokens.gap_3 + tokens.icon_button_size;
    (left, right)
}

fn status_dot_x(look: &Look, status_right: f64, status: &str) -> f64 {
    let tokens = look.tokens;
    status_right - draw::measure(status, &look.status) - tokens.gap_2 - tokens.dot_size / 2.0
}

fn open_layout(look: &Look, expanded: &Expanded) -> OpenLayout {
    let tokens = look.tokens;
    let place = look.place;
    let row_height = tokens.avatar_size + tokens.gap_3;
    let rows = expanded.rows.len() as f64;
    let (left, right) = header_widths(look);
    let header_middle = look.band_middle();
    let rows_top = -place.idle.height - tokens.gap_2;
    let height = -(rows_top - rows * row_height - tokens.gap_3);
    // The outline curves in at the top; content keeps clear of its shoulders.
    let pad = shoulder(height) + tokens.gap_4;
    // The header wraps the camera: each side keeps its own half beside it.
    let side = left.max(right) + tokens.gap_3;
    let width = place.core + 2.0 * (side + pad);
    let size = place.open(width.max(tokens.island_width), height);
    OpenLayout {
        size,
        pad,
        header_middle,
        rows_top,
        row_height,
    }
}

pub fn open_size(look: &Look, expanded: &Expanded) -> Size {
    open_layout(look, expanded).size
}

fn tile(look: &Look, row: &Row) -> Retained<CALayer> {
    let tokens = look.tokens;
    let size = tokens.avatar_size;
    let plate: Retained<CALayer>;
    let glyph_color: Color;
    let symbol = match row {
        Row::Computer(computer) => {
            let tone = tokens.display_peers[computer.tone.min(tokens.display_peers.len() - 1)];
            let gradient = CAGradientLayer::new();
            let strong = cg(tone.mix(tokens.card, tokens.mix_icon));
            let faint = cg(tone.mix(tokens.card, tokens.mix_faint));
            let colors = objc2_foundation::NSArray::from_slice(&[
                draw::color_value(&strong),
                draw::color_value(&faint),
            ]);
            // SAFETY: an array of CGColor, the type gradient colors take.
            unsafe { gradient.setColors(Some(&colors)) };
            gradient.setStartPoint(NSPoint::new(0.0, 1.0));
            gradient.setEndPoint(NSPoint::new(1.0, 0.0));
            plate = gradient.into_super();
            glyph_color = tone;
            Symbol::for_platform(computer.platform)
        }
        Row::SetUp | Row::Dimming(_) | Row::Clipboard { .. } => {
            plate = CALayer::new();
            plate.setBackgroundColor(Some(&cg(tokens.primary.with_alpha(tokens.mix_tint))));
            glyph_color = tokens.primary;
            match row {
                Row::SetUp => Symbol::Plus,
                Row::Dimming(_) => Symbol::MoonFill,
                _ => Symbol::DocOnClipboard,
            }
        }
    };
    plate.setBounds(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(size, size)));
    plate.setCornerRadius(tokens.radius_group);
    plate.setMasksToBounds(true);
    let glyph = look.glyph(symbol, tokens.avatar_glyph, glyph_color);
    let (width, height) = layer_size(&glyph);
    frame_centered(&glyph, size / 2.0, size / 2.0, width, height);
    plate.addSublayer(&glyph);
    plate
}

fn row_texts(row: &Row) -> (String, Option<String>) {
    match row {
        Row::Computer(computer) => (computer.name.clone(), Some(computer.status.clone())),
        Row::SetUp => ("Set up a computer".to_owned(), None),
        Row::Dimming(_) => ("Dim screens".to_owned(), None),
        Row::Clipboard { detail, .. } => ("Share clipboard".to_owned(), detail.clone()),
    }
}

fn build_header(look: &Look, layout: &OpenLayout, expanded: &Expanded) -> (Header, Control) {
    let tokens = look.tokens;
    let (left_edge, right_edge) = layout.edges();
    let band = band(
        look,
        Rect::new(
            -layout.half(),
            layout.rows_top,
            layout.size.width,
            -layout.rows_top,
        ),
    );

    let mark = link_mark(look, tokens.orbit_local, tokens.orbit_peer);
    let mark_size = tokens.sharing_mark_size;
    frame_centered(
        &mark,
        left_edge + mark_size / 2.0,
        layout.header_middle,
        mark_size,
        mark_size,
    );
    band.addSublayer(&mark);
    let brand = draw::text_layer(
        "MonHop",
        &look.brand,
        tokens.foreground,
        false,
        look.scale(),
    );
    let brand_width = draw::measure("MonHop", &look.brand);
    frame_centered(
        &brand,
        left_edge + mark_size + tokens.gap_2 + brand_width / 2.0,
        layout.header_middle,
        brand_width,
        draw::line_height(&look.brand),
    );
    band.addSublayer(&brand);

    let button_size = tokens.icon_button_size;
    let (open_root, open_widget) = button(look, Symbol::OpenApp);
    let open_x = right_edge - button_size / 2.0;
    frame_centered(
        &open_root,
        open_x,
        layout.header_middle,
        button_size,
        button_size,
    );
    band.addSublayer(&open_root);
    let open = Control {
        id: ControlId::OpenWindow,
        root: open_root,
        widget: open_widget,
        hit: Rect::new(
            open_x - button_size / 2.0,
            layout.header_middle - button_size / 2.0,
            button_size,
            button_size,
        ),
        label: "Open MonHop".to_owned(),
        pending: None,
        element: None,
        row: None,
    };
    let status_color = status_text_color(look, expanded.status_tone);
    let status_text = draw::text_layer(
        &expanded.status,
        &look.status,
        status_color,
        true,
        look.scale(),
    );
    let status_right = right_edge - button_size - tokens.gap_3;
    frame_centered(
        &status_text,
        status_right - look.status_room / 2.0,
        layout.header_middle,
        look.status_room,
        draw::line_height(&look.status),
    );
    band.addSublayer(&status_text);
    let status_dot = draw::dot(tokens.dot_size, look.tone(expanded.status_tone));
    frame_centered(
        &status_dot,
        status_dot_x(look, status_right, &expanded.status),
        layout.header_middle,
        tokens.dot_size,
        tokens.dot_size,
    );
    band.addSublayer(&status_dot);
    let header = Header {
        band,
        status_dot,
        status_text,
        status_now: (expanded.status.clone(), expanded.status_tone),
        status_right,
    };
    (header, open)
}

fn build_row(
    look: &Look,
    layout: &OpenLayout,
    index: usize,
    row: &Row,
) -> (RowParts, Vec<Control>) {
    let tokens = look.tokens;
    let (left_edge, right_edge) = layout.edges();
    let half = layout.half();
    let middle = layout.row_middle(index);
    let key = row_key(row);
    let band = band(
        look,
        Rect::new(
            -half,
            middle - layout.row_height / 2.0,
            layout.size.width,
            layout.row_height,
        ),
    );
    let button_size = tokens.icon_button_size;
    let mut controls = Vec::new();

    let plate = tile(look, row);
    let tile_size = tokens.avatar_size;
    frame_centered(
        &plate,
        left_edge + tile_size / 2.0,
        middle,
        tile_size,
        tile_size,
    );
    band.addSublayer(&plate);
    let text_left = left_edge + tile_size + tokens.gap_3;

    let (control_root, widget, id, label, hit_whole_row) = match row {
        Row::Computer(computer) => {
            let (root, widget) = switch(look, computer.enabled);
            (
                root,
                widget,
                ControlId::Computer(computer.fingerprint.clone()),
                computer.name.clone(),
                true,
            )
        }
        Row::SetUp => {
            let (root, widget) = button(look, Symbol::OpenApp);
            (
                root,
                widget,
                ControlId::SetUp,
                "Set up a computer".to_owned(),
                true,
            )
        }
        Row::Dimming(dimming) => {
            let (root, widget) = switch(look, dimming.dimmed);
            (
                root,
                widget,
                ControlId::Dimming,
                "Dim screens".to_owned(),
                false,
            )
        }
        Row::Clipboard { enabled, .. } => {
            let (root, widget) = switch(look, *enabled);
            (
                root,
                widget,
                ControlId::Clipboard,
                "Share clipboard".to_owned(),
                true,
            )
        }
    };
    let (control_w, control_h) = layer_size(&control_root);
    let control_x = right_edge - control_w / 2.0;
    frame_centered(&control_root, control_x, middle, control_w, control_h);
    band.addSublayer(&control_root);
    let control_left = right_edge - control_w;
    let text_width = (control_left - tokens.gap_3 - text_left).max(0.0);
    let hit = if hit_whole_row {
        Rect::new(
            -half,
            middle - layout.row_height / 2.0,
            layout.size.width,
            layout.row_height,
        )
    } else {
        let height = control_h.max(button_size);
        Rect::new(control_left, middle - height / 2.0, control_w, height)
    };
    controls.push(Control {
        id,
        root: control_root,
        widget,
        hit,
        label,
        pending: None,
        element: None,
        row: Some(key.clone()),
    });

    let (title_text, detail_text) = row_texts(row);
    let title = draw::text_layer(
        &title_text,
        &look.title,
        tokens.foreground,
        false,
        look.scale(),
    );
    let title_h = draw::line_height(&look.title);
    let second_h = match row {
        Row::Dimming(_) => tokens.slider_thumb,
        _ if detail_text.is_some() => draw::line_height(&look.detail),
        _ => 0.0,
    };
    let block = if second_h > 0.0 {
        title_h + tokens.gap_half + second_h
    } else {
        title_h
    };
    let top = middle + block / 2.0;
    title.setFrame(NSRect::new(
        NSPoint::new(text_left, top - title_h),
        NSSize::new(text_width, title_h),
    ));
    band.addSublayer(&title);
    let second_middle = top - title_h - tokens.gap_half - second_h / 2.0;
    let detail = detail_text.as_ref().map(|text| {
        let layer = draw::text_layer(text, &look.detail, tokens.muted, false, look.scale());
        layer.setFrame(NSRect::new(
            NSPoint::new(text_left, second_middle - second_h / 2.0),
            NSSize::new(text_width, second_h),
        ));
        band.addSublayer(&layer);
        layer
    });
    if let Row::Dimming(dimming) = row {
        let (root, widget) = slider(
            look,
            text_width,
            dimming.level,
            dimming.min_level,
            dimming.max_level,
        );
        root.setFrame(NSRect::new(
            NSPoint::new(text_left, second_middle - tokens.slider_thumb / 2.0),
            NSSize::new(text_width, tokens.slider_thumb),
        ));
        band.addSublayer(&root);
        let height = tokens.slider_thumb.max(button_size);
        controls.push(Control {
            id: ControlId::Level,
            root,
            widget,
            hit: Rect::new(text_left, second_middle - height / 2.0, text_width, height),
            label: "Darkness".to_owned(),
            pending: None,
            element: None,
            row: Some(key.clone()),
        });
    }
    let parts = RowParts {
        key,
        band,
        built_middle: middle,
        middle,
        texts: RowTexts {
            title,
            detail,
            title_now: title_text,
            detail_now: detail_text,
        },
    };
    (parts, controls)
}

pub fn build_open(look: &Look, expanded: &Expanded, mtm: MainThreadMarker) -> OpenParts {
    let layout = open_layout(look, expanded);
    let group = draw::group(look.place.outline(layout.size).bounds());
    let (header, open) = build_header(look, &layout, expanded);
    group.addSublayer(&header.band);
    let mut controls = vec![open];
    let mut rows = Vec::new();
    for (index, row) in expanded.rows.iter().enumerate() {
        let (parts, row_controls) = build_row(look, &layout, index, row);
        group.addSublayer(&parts.band);
        controls.extend(row_controls);
        rows.push(parts);
    }
    number_controls(&mut controls, mtm);
    OpenParts {
        group,
        size: layout.size,
        pad: layout.pad,
        header,
        rows,
        controls,
    }
}

/// Every control's accessibility element answers for its place among the controls.
fn number_controls(controls: &mut [Control], mtm: MainThreadMarker) {
    for (index, control) in controls.iter_mut().enumerate() {
        match &control.element {
            Some(element) => element.set_control(index),
            None => control.element = Some(element(mtm, index, control)),
        }
    }
}

/// How far the status dot is drawn from its place beside the current word; zero at rest.
pub fn status_drift(look: &Look, parts: &OpenParts) -> f64 {
    let header = &parts.header;
    let dot = &header.status_dot;
    // SAFETY: the presentation layer is a read-only copy of this one.
    let drawn = unsafe { dot.presentationLayer() }.map_or(dot.position(), |shown| shown.position());
    drawn.x - status_dot_x(look, header.status_right, &header.status_now.0)
}

fn status_text_color(look: &Look, tone: Tone) -> Color {
    if tone == Tone::Success {
        look.tokens.foreground
    } else {
        look.tokens.muted
    }
}

fn element(mtm: MainThreadMarker, index: usize, control: &Control) -> Retained<ControlElement> {
    let element = ControlElement::new(mtm, index);
    // SAFETY: AppKit's exported role constants.
    unsafe {
        match control.widget {
            Widget::Switch { .. } => {
                element.setAccessibilityRole(Some(NSAccessibilityCheckBoxRole));
                element.setAccessibilitySubrole(Some(NSAccessibilitySwitchSubrole));
            }
            Widget::Slider { .. } => element.setAccessibilityRole(Some(NSAccessibilitySliderRole)),
            Widget::Button { .. } => element.setAccessibilityRole(Some(NSAccessibilityButtonRole)),
        }
    }
    element.setAccessibilityLabel(Some(&NSString::from_str(&control.label)));
    refresh_element_value(control, &element);
    element
}

pub fn refresh_element_value(control: &Control, element: &ControlElement) {
    let value = match control.widget {
        Widget::Button { .. } => None,
        _ => Some(NSNumber::new_u8(control.value())),
    };
    // SAFETY: switches and sliders report a number; buttons have no value.
    unsafe {
        element.setAccessibilityValue(
            value
                .as_deref()
                .map(|number| -> &objc2::runtime::AnyObject { number }),
        )
    };
}

/// A row that changed place glides there: down on the growing spring, up on the closing one.
fn move_row(look: &Look, row: &mut RowParts, middle: f64) {
    let shift = middle - row.middle;
    if shift == 0.0 {
        return;
    }
    row.middle = middle;
    let place = NSPoint::new(0.0, middle - row.built_middle);
    let tokens = look.tokens;
    if look.reduce {
        row.band.setPosition(place);
    } else if shift < 0.0 {
        draw::spring_position(&row.band, place, tokens.grow_spring());
    } else {
        draw::spring_position(&row.band, place, tokens.shrink_spring());
    }
}

/// New rows materialize in turn, rows that stay glide to their places, and rows that went
/// away dissolve where they were. Nothing happens while the rows are the same.
fn restack(
    look: &Look,
    parts: &mut OpenParts,
    expanded: &Expanded,
    layout: &OpenLayout,
    mtm: MainThreadMarker,
) {
    let keys: Vec<RowKey> = expanded.rows.iter().map(row_key).collect();
    if parts.rows.iter().map(|row| &row.key).eq(keys.iter()) {
        return;
    }
    let tokens = look.tokens;
    let old_keys: Vec<RowKey> = parts.rows.iter().map(|row| row.key.clone()).collect();
    let mut old_rows: Vec<Option<RowParts>> = std::mem::take(&mut parts.rows)
        .into_iter()
        .map(Some)
        .collect();
    let (mut controls, mut row_controls): (Vec<Control>, Vec<Control>) =
        std::mem::take(&mut parts.controls)
            .into_iter()
            .partition(|control| control.row.is_none());
    let mut arriving = 0.0;
    for (index, (row, from)) in expanded
        .rows
        .iter()
        .zip(match_rows(&old_keys, &keys))
        .enumerate()
    {
        let middle = layout.row_middle(index);
        match from.and_then(|old| old_rows[old].take()) {
            Some(mut kept) => {
                let shift = middle - kept.middle;
                move_row(look, &mut kept, middle);
                for mut control in
                    row_controls.extract_if(.., |control| control.row.as_ref() == Some(&kept.key))
                {
                    control.hit.y += shift;
                    controls.push(control);
                }
                parts.rows.push(kept);
            }
            None => {
                let (built, built_controls) = build_row(look, layout, index, row);
                parts.group.addSublayer(&built.band);
                if !look.reduce {
                    arriving += 1.0;
                    materialize(look, &built.band, tokens.stagger * arriving);
                }
                controls.extend(built_controls);
                parts.rows.push(built);
            }
        }
    }
    for gone in old_rows.into_iter().flatten() {
        if look.reduce {
            gone.band.removeFromSuperlayer();
        } else {
            dissolve(look, &gone.band);
            draw::remove_later(&gone.band, tokens.fast);
        }
    }
    number_controls(&mut controls, mtm);
    parts.controls = controls;
    draw::span(&parts.group, look.place.outline(layout.size).bounds());
}

/// New values in place: rows come, go or move, texts cross through a light blur, switches and
/// the slider move. False when the island's width or inset changed and it must be rebuilt.
pub fn update_open(
    look: &Look,
    parts: &mut OpenParts,
    expanded: &Expanded,
    dragging: bool,
    now: Instant,
    mtm: MainThreadMarker,
) -> bool {
    let layout = open_layout(look, expanded);
    if layout.size.width != parts.size.width || layout.pad != parts.pad {
        return false;
    }
    restack(look, parts, expanded, &layout, mtm);
    parts.size = layout.size;
    let tokens = look.tokens;
    let header = &mut parts.header;
    let words_changed = header.status_now.0 != expanded.status;
    if words_changed {
        swap_text(look, &header.status_text, &expanded.status);
        let dot = &header.status_dot;
        let center = NSPoint::new(
            status_dot_x(look, header.status_right, &expanded.status),
            dot.position().y,
        );
        swap_dot(look, dot, center);
    }
    if header.status_now.1 != expanded.status_tone {
        let (dot, text) = (
            look.tone(expanded.status_tone),
            status_text_color(look, expanded.status_tone),
        );
        // New words arrive from nothing in their new color; standing ones change color.
        if words_changed || look.reduce {
            header.status_dot.setBackgroundColor(Some(&cg(dot)));
            header.status_text.setForegroundColor(Some(&cg(text)));
        } else {
            recolor(look, &header.status_dot, dot);
            draw::retint(&header.status_text, text, tokens.base, tokens.ease_out);
        }
    }
    header.status_now = (expanded.status.clone(), expanded.status_tone);
    for (row_parts, row) in parts.rows.iter_mut().zip(expanded.rows.iter()) {
        let texts = &mut row_parts.texts;
        let (title, detail) = row_texts(row);
        if texts.title_now != title {
            swap_text(look, &texts.title, &title);
            texts.title_now = title;
        }
        if let (Some(layer), Some(text)) = (&texts.detail, &detail)
            && texts.detail_now.as_ref() != Some(text)
        {
            swap_text(look, layer, text);
        }
        texts.detail_now = detail;
    }
    for control in &mut parts.controls {
        let reported = match (
            &control.id,
            expanded
                .rows
                .iter()
                .find_map(|row| control_value(&control.id, row)),
        ) {
            (_, Some(value)) => value,
            _ => continue,
        };
        if let Some(pending) = &control.pending {
            if pending.value == reported || pending.until <= now {
                control.pending = None;
            } else {
                continue;
            }
        }
        let is_level = control.id == ControlId::Level;
        match &mut control.widget {
            widget @ Widget::Switch { .. } => set_switch(look, widget, reported == 1),
            widget @ Widget::Slider { .. } if !(is_level && dragging) => {
                set_slider(look, widget, reported, false);
            }
            _ => {}
        }
        if let Some(element) = &control.element {
            refresh_element_value(control, element);
        }
    }
    true
}

/// What the app reports for a control, from the row that holds it.
fn control_value(id: &ControlId, row: &Row) -> Option<u8> {
    match (id, row) {
        (ControlId::Computer(fingerprint), Row::Computer(computer))
            if &computer.fingerprint == fingerprint =>
        {
            Some(u8::from(computer.enabled))
        }
        (ControlId::Dimming, Row::Dimming(dimming)) => Some(u8::from(dimming.dimmed)),
        (ControlId::Level, Row::Dimming(dimming)) => Some(dimming.level),
        (ControlId::Clipboard, Row::Clipboard { enabled, .. }) => Some(u8::from(*enabled)),
        _ => None,
    }
}

/// Positions each control's accessibility element in the view, `origin` being the island's top
/// center there.
pub fn place_elements(parts: &OpenParts, origin: NSPoint, view: &objc2::runtime::AnyObject) {
    for control in &parts.controls {
        let Some(element) = &control.element else {
            continue;
        };
        let hit = control.hit;
        element.setAccessibilityFrameInParentSpace(NSRect::new(
            NSPoint::new(origin.x + hit.x, origin.y + hit.y),
            NSSize::new(hit.width, hit.height),
        ));
        // SAFETY: the view outlives its elements, which it lists as its children.
        unsafe { element.setAccessibilityParent(Some(view)) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn computer(name: &str) -> RowKey {
        RowKey::Computer(name.to_owned())
    }

    #[test]
    fn rows_that_stay_are_found_wherever_they_moved() {
        let clipboard = RowKey::Clipboard { detail: false };
        let old = [computer("a"), RowKey::Dimming, clipboard.clone()];
        // A computer arrives first: everything else stays and moves down one place.
        let new = [
            computer("b"),
            computer("a"),
            RowKey::Dimming,
            clipboard.clone(),
        ];
        assert_eq!(match_rows(&old, &new), [None, Some(0), Some(1), Some(2)]);
        // The middle row leaves: the last one moves up into its place.
        let new = [computer("a"), clipboard];
        assert_eq!(match_rows(&old, &new), [Some(0), Some(2)]);
    }

    #[test]
    fn a_row_whose_shape_changed_is_a_new_row() {
        let old = [
            RowKey::Dimming,
            RowKey::Clipboard { detail: false },
            RowKey::SetUp,
        ];
        let new = [
            RowKey::Dimming,
            RowKey::Clipboard { detail: true },
            RowKey::SetUp,
        ];
        assert_eq!(match_rows(&old, &new), [Some(0), None, Some(2)]);
    }

    #[test]
    fn each_old_row_continues_at_most_one_new_row() {
        let old = [RowKey::SetUp];
        let new = [RowKey::SetUp, RowKey::SetUp];
        assert_eq!(match_rows(&old, &new), [Some(0), None]);
        assert_eq!(match_rows(&[], &new), [None, None]);
        assert!(match_rows(&old, &[]).is_empty());
    }
}
