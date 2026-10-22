//! The island's design values, read from the window's own stylesheet so there is one home for
//! every duration, easing, size and color, plus the springs those tokens describe.

use std::{collections::BTreeMap, f64::consts::PI, fmt, sync::OnceLock};

const STYLESHEET: &str = include_str!("../../ui/styles.css");

/// Why a token could not be used; the unit tests over the real stylesheet keep this unreachable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenError {
    pub name: String,
    pub problem: &'static str,
}

impl fmt::Display for TokenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "token {}: {}", self.name, self.problem)
    }
}

fn fail<T>(name: &str, problem: &'static str) -> Result<T, TokenError> {
    Err(TokenError {
        name: name.to_owned(),
        problem,
    })
}

/// A color in OKLab with straight alpha, the space `color-mix(in oklab, …)` mixes in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub l: f64,
    pub a: f64,
    pub b: f64,
    pub alpha: f64,
}

impl Color {
    pub fn from_oklch(lightness: f64, chroma: f64, hue_degrees: f64, alpha: f64) -> Self {
        let hue = hue_degrees.to_radians();
        Self {
            l: lightness,
            a: chroma * hue.cos(),
            b: chroma * hue.sin(),
            alpha,
        }
    }

    /// From 0..=1 gamma-encoded sRGB channels.
    pub fn from_srgb(red: f64, green: f64, blue: f64, alpha: f64) -> Self {
        let [r, g, b] = [red, green, blue].map(|channel| {
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        });
        let l = (0.412_221_470_8 * r + 0.536_332_536_3 * g + 0.051_445_992_9 * b).cbrt();
        let m = (0.211_903_498_2 * r + 0.680_699_545_1 * g + 0.107_396_956_6 * b).cbrt();
        let s = (0.088_302_461_9 * r + 0.281_718_837_6 * g + 0.629_978_700_5 * b).cbrt();
        Self {
            l: 0.210_454_255_3 * l + 0.793_617_785 * m - 0.004_072_046_8 * s,
            a: 1.977_998_495_1 * l - 2.428_592_205 * m + 0.450_593_709_9 * s,
            b: 0.025_904_037_1 * l + 0.782_771_766_2 * m - 0.808_675_766 * s,
            alpha,
        }
    }

    /// Gamma-encoded sRGB and alpha, each clamped to 0..=1.
    pub fn srgb(self) -> [f64; 4] {
        let l = (self.l + 0.396_337_777_4 * self.a + 0.215_803_757_3 * self.b).powi(3);
        let m = (self.l - 0.105_561_345_8 * self.a - 0.063_854_172_8 * self.b).powi(3);
        let s = (self.l - 0.089_484_177_5 * self.a - 1.291_485_548 * self.b).powi(3);
        let linear = [
            4.076_741_662_1 * l - 3.307_711_591_3 * m + 0.230_969_929_2 * s,
            -1.268_438_004_6 * l + 2.609_757_401_1 * m - 0.341_319_396_5 * s,
            -0.004_196_086_3 * l - 0.703_418_614_7 * m + 1.707_614_701 * s,
        ];
        let [r, g, b] = linear.map(|channel| {
            let channel = channel.clamp(0.0, 1.0);
            if channel <= 0.003_130_8 {
                12.92 * channel
            } else {
                1.055 * channel.powf(1.0 / 2.4) - 0.055
            }
        });
        [r, g, b, self.alpha.clamp(0.0, 1.0)]
    }
}

/// The custom properties of the stylesheet's first `:root` block, comments stripped.
pub struct Sheet {
    values: BTreeMap<String, String>,
}

impl Sheet {
    pub fn parse(css: &str) -> Result<Self, TokenError> {
        let css = strip_comments(css);
        let Some(start) = css.find(":root {") else {
            return fail(":root", "the stylesheet has no :root block");
        };
        let body = &css[start + ":root {".len()..];
        let Some(end) = body.find('}') else {
            return fail(":root", "the :root block never closes");
        };
        let values = body[..end]
            .split(';')
            .filter_map(|declaration| declaration.split_once(':'))
            .map(|(name, value)| (name.trim(), value.trim()))
            .filter(|(name, _)| name.starts_with("--"))
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect();
        Ok(Self { values })
    }

    /// The value with any whole-value `var(--x)` resolved.
    fn raw(&self, name: &str) -> Result<&str, TokenError> {
        let mut current = name;
        for _ in 0..8 {
            let Some(value) = self.values.get(current) else {
                return fail(name, "missing from :root");
            };
            match value
                .strip_prefix("var(")
                .and_then(|rest| rest.strip_suffix(')'))
            {
                Some(next) if next.starts_with("--") => current = next,
                _ => return Ok(value),
            }
        }
        fail(name, "var() chain too deep")
    }

    pub fn px(&self, name: &str) -> Result<f64, TokenError> {
        number_with_unit(self.raw(name)?, "px").ok_or_else(|| problem(name, "not a px length"))
    }

    /// Seconds.
    pub fn ms(&self, name: &str) -> Result<f64, TokenError> {
        number_with_unit(self.raw(name)?, "ms")
            .map(|millis| millis / 1000.0)
            .ok_or_else(|| problem(name, "not a ms duration"))
    }

    pub fn number(&self, name: &str) -> Result<f64, TokenError> {
        number_with_unit(self.raw(name)?, "").ok_or_else(|| problem(name, "not a plain number"))
    }

    /// The dark value: the island is always drawn on black.
    pub fn color(&self, name: &str) -> Result<Color, TokenError> {
        color(self.raw(name)?)
            .ok_or_else(|| problem(name, "not an rgb(), oklch() or light-dark() color"))
    }

    pub fn bezier(&self, name: &str) -> Result<[f64; 4], TokenError> {
        let Some(arguments) = function_arguments(self.raw(name)?, "cubic-bezier") else {
            return fail(name, "not a cubic-bezier()");
        };
        let points: Vec<f64> = arguments
            .split(',')
            .filter_map(|part| part.trim().parse().ok())
            .collect();
        match points.as_slice() {
            [x1, y1, x2, y2] => Ok([*x1, *y1, *x2, *y2]),
            _ => fail(name, "cubic-bezier() needs four numbers"),
        }
    }

    /// The highest output of a `linear()` easing and where along it that peak sits, 0..=1.
    pub fn linear_peak(&self, name: &str) -> Result<(f64, f64), TokenError> {
        let Some(arguments) = function_arguments(self.raw(name)?, "linear") else {
            return fail(name, "not a linear() easing");
        };
        let stops: Vec<&str> = arguments.split(',').map(str::trim).collect();
        let last = stops.len().saturating_sub(1).max(1) as f64;
        let mut peak: Option<(f64, f64)> = None;
        for (index, stop) in stops.iter().enumerate() {
            let mut parts = stop.split_whitespace();
            let value: f64 = match parts.next().and_then(|value| value.parse().ok()) {
                Some(value) => value,
                None => return fail(name, "a linear() stop is not a number"),
            };
            let at = match parts.next() {
                Some(position) => match number_with_unit(position, "%") {
                    Some(percent) => percent / 100.0,
                    None => return fail(name, "a linear() position is not a percentage"),
                },
                None => index as f64 / last,
            };
            if peak.is_none_or(|(highest, _)| value > highest) {
                peak = Some((value, at));
            }
        }
        peak.ok_or_else(|| problem(name, "linear() has no stops"))
    }
}

fn problem(name: &str, problem: &'static str) -> TokenError {
    TokenError {
        name: name.to_owned(),
        problem,
    }
}

fn strip_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        rest = match rest[start + 2..].find("*/") {
            Some(end) => &rest[start + 2 + end + 2..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

fn number_with_unit(text: &str, unit: &str) -> Option<f64> {
    let number = text.trim().strip_suffix(unit)?;
    if number.is_empty() || number.ends_with(|c: char| c.is_ascii_alphabetic() || c == '%') {
        return None;
    }
    number.parse().ok().filter(|value: &f64| value.is_finite())
}

fn function_arguments<'a>(text: &'a str, function: &str) -> Option<&'a str> {
    text.trim()
        .strip_prefix(function)?
        .trim_start()
        .strip_prefix('(')?
        .strip_suffix(')')
}

/// Splits on `separator` outside parentheses.
fn split_top_level(text: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0_i32, 0);
    for (index, character) in text.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth -= 1,
            c if c == separator && depth == 0 => {
                parts.push(text[start..index].trim());
                start = index + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(text[start..].trim());
    parts
}

fn color(text: &str) -> Option<Color> {
    if let Some(arguments) = function_arguments(text, "light-dark") {
        return match split_top_level(arguments, ',').as_slice() {
            [_, dark] => color(dark),
            _ => None,
        };
    }
    if let Some(arguments) = function_arguments(text, "oklch") {
        let (channels, alpha) = channels_and_alpha(arguments)?;
        return match channels.as_slice() {
            [l, c, h] => Some(Color::from_oklch(*l, *c, *h, alpha)),
            _ => None,
        };
    }
    if let Some(arguments) = function_arguments(text, "rgb") {
        let (channels, alpha) = channels_and_alpha(arguments)?;
        return match channels.as_slice() {
            [r, g, b] => Some(Color::from_srgb(r / 255.0, g / 255.0, b / 255.0, alpha)),
            _ => None,
        };
    }
    None
}

/// Space-separated channels with an optional `/ alpha` (a share or a percentage).
fn channels_and_alpha(arguments: &str) -> Option<(Vec<f64>, f64)> {
    let (channels, alpha) = match arguments.split_once('/') {
        Some((channels, alpha)) => {
            let alpha = alpha.trim();
            let alpha = number_with_unit(alpha, "%")
                .map(|percent| percent / 100.0)
                .or_else(|| number_with_unit(alpha, ""))?;
            (channels, alpha)
        }
        None => (arguments, 1.0),
    };
    let channels = channels
        .split_whitespace()
        .map(|channel| {
            number_with_unit(channel, "%")
                .map(|percent| percent / 100.0)
                .or_else(|| number_with_unit(channel, ""))
        })
        .collect::<Option<Vec<f64>>>()?;
    Some((channels, alpha))
}

/// A damped spring of mass 1 carrying progress from 0 to 1, as `CASpringAnimation` plays it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spring {
    /// Undamped angular frequency, radians per second.
    pub omega: f64,
    /// 1 is critically damped; below 1 it overshoots.
    pub ratio: f64,
}

impl Spring {
    /// Matches a `linear()` spring easing played over `duration`: the overshoot fixes the damping
    /// ratio and the time of the peak fixes the frequency.
    pub fn from_easing(duration: f64, overshoot: f64, peak_at: f64) -> Self {
        let ratio = ratio_for_overshoot(overshoot);
        let damped = PI / (peak_at * duration);
        Self {
            omega: damped / (1.0 - ratio * ratio).sqrt(),
            ratio,
        }
    }

    /// Critically damped, with `like`'s frequency scaled so it settles over `duration` as `like`
    /// settles over `like_duration`.
    pub fn critical(duration: f64, like: Spring, like_duration: f64) -> Self {
        Self {
            omega: like.omega * like_duration / duration,
            ratio: 1.0,
        }
    }

    pub fn stiffness(self) -> f64 {
        self.omega * self.omega
    }

    pub fn damping(self) -> f64 {
        2.0 * self.ratio * self.omega
    }

    /// Progress at `time` seconds, starting at 0 with `velocity` progress per second.
    pub fn progress(self, time: f64, velocity: f64) -> f64 {
        1.0 + self.offset(time, velocity).0
    }

    /// Progress per second at `time`.
    pub fn velocity(self, time: f64, velocity: f64) -> f64 {
        self.offset(time, velocity).1
    }

    /// Distance from the target and its rate, for a start one unit short of it.
    fn offset(self, time: f64, velocity: f64) -> (f64, f64) {
        let time = time.max(0.0);
        let omega = self.omega;
        if self.ratio < 1.0 - 1e-9 {
            let decay = self.ratio * omega;
            let damped = omega * (1.0 - self.ratio * self.ratio).sqrt();
            let (a, b) = (-1.0, (velocity - decay) / damped);
            let envelope = (-decay * time).exp();
            let (sin, cos) = (damped * time).sin_cos();
            let value = envelope * (a * cos + b * sin);
            let rate = envelope * (-decay * (a * cos + b * sin) + damped * (b * cos - a * sin));
            (value, rate)
        } else {
            let (a, b) = (-1.0, velocity - omega);
            let envelope = (-omega * time).exp();
            (
                envelope * (a + b * time),
                envelope * (b - omega * (a + b * time)),
            )
        }
    }

    /// How far past the target it swings, as a share of the travel, from rest.
    #[cfg(test)]
    pub fn overshoot(self) -> f64 {
        if self.ratio >= 1.0 {
            return 0.0;
        }
        (-PI * self.ratio / (1.0 - self.ratio * self.ratio).sqrt()).exp()
    }

    /// When it last strays more than `tolerance` points from a target `travel` points away.
    pub fn settle_time(self, travel: f64, tolerance: f64, velocity: f64) -> f64 {
        const STEP: f64 = 1.0 / 480.0;
        const LIMIT: f64 = 10.0;
        if travel <= tolerance {
            return 0.0;
        }
        let mut last_outside = 0.0;
        let mut time = 0.0;
        while time < LIMIT {
            if self.offset(time, velocity).0.abs() * travel > tolerance {
                last_outside = time;
            }
            time += STEP;
        }
        last_outside + STEP
    }

    /// The lowest and highest progress it passes through before `until` seconds.
    pub fn excursion(self, velocity: f64, until: f64) -> (f64, f64) {
        const STEP: f64 = 1.0 / 480.0;
        let (mut low, mut high) = (0.0_f64, 0.0_f64);
        let mut time = 0.0;
        while time <= until {
            let progress = self.progress(time, velocity);
            low = low.min(progress);
            high = high.max(progress);
            time += STEP;
        }
        (low, high.max(1.0))
    }

    /// The fastest progress it reaches, per second, sampled densely.
    pub fn peak_speed(self, velocity: f64) -> f64 {
        let mut fastest = velocity.abs();
        let mut time = 0.0;
        while time < 2.0 {
            fastest = fastest.max(self.offset(time, velocity).1.abs());
            time += 1.0 / 2000.0;
        }
        fastest
    }
}

/// A CSS `cubic-bezier()` easing's output at input `x` (0..=1); x(t) is monotonic for valid easings.
#[cfg(test)]
pub fn ease_at(points: [f64; 4], x: f64) -> f64 {
    let [x1, y1, x2, y2] = points;
    let curve = |a: f64, b: f64, t: f64| {
        let rest = 1.0 - t;
        3.0 * rest * rest * t * a + 3.0 * rest * t * t * b + t * t * t
    };
    let x = x.clamp(0.0, 1.0);
    let (mut low, mut high) = (0.0, 1.0);
    for _ in 0..48 {
        let middle = (low + high) / 2.0;
        if curve(x1, x2, middle) < x {
            low = middle;
        } else {
            high = middle;
        }
    }
    curve(y1, y2, (low + high) / 2.0)
}

/// The damping ratio whose free swing passes the target by `overshoot` of the travel.
pub fn ratio_for_overshoot(overshoot: f64) -> f64 {
    if overshoot <= 0.0 {
        return 1.0;
    }
    let log = overshoot.ln();
    (-log / (PI * PI + log * log).sqrt()).clamp(0.05, 1.0)
}

/// Every value the island draws with, resolved once from the stylesheet. Colors are the dark
/// theme's, because the island is always black.
#[derive(Clone, Debug, PartialEq)]
pub struct IslandTokens {
    pub foreground: Color,
    pub success: Color,
    pub warning: Color,
    pub destructive: Color,
    pub island_fill: Color,

    pub gap_2: f64,
    pub gap_3: f64,
    pub text_base: f64,
    pub weight_medium: f64,
    pub icon_size: f64,
    pub dot_size: f64,
    pub stroke_bold: f64,

    pub wing_min: f64,
    pub wing_max: f64,

    /// Seconds.
    pub fast: f64,
    pub base: f64,
    pub slow: f64,
    pub spring: f64,
    pub stagger: f64,
    pub loop_spin: f64,
    pub blur: f64,
    pub blur_deep: f64,
    pub focus: f64,
    pub ease_out: [f64; 4],
    pub ease_focus: [f64; 4],
    pub spring_overshoot: f64,
    pub spring_peak_at: f64,
    pub scale_defocus: f64,
}

impl IslandTokens {
    pub fn from_sheet(sheet: &Sheet) -> Result<Self, TokenError> {
        Ok(Self {
            foreground: sheet.color("--foreground")?,
            success: sheet.color("--success")?,
            warning: sheet.color("--warning")?,
            destructive: sheet.color("--destructive")?,
            island_fill: sheet.color("--island-fill")?,
            gap_2: sheet.px("--gap-2")?,
            gap_3: sheet.px("--gap-3")?,
            text_base: sheet.px("--text-base")?,
            weight_medium: sheet.number("--weight-medium")?,
            icon_size: sheet.px("--icon-size")?,
            dot_size: sheet.px("--dot-size")?,
            stroke_bold: sheet.px("--stroke-bold")?,
            wing_min: sheet.px("--island-wing-min")?,
            wing_max: sheet.px("--island-wing-max")?,
            fast: sheet.ms("--motion-fast")?,
            base: sheet.ms("--motion-base")?,
            slow: sheet.ms("--motion-slow")?,
            spring: sheet.ms("--motion-spring")?,
            stagger: sheet.ms("--motion-stagger")?,
            loop_spin: sheet.ms("--loop-spin")?,
            blur: sheet.px("--motion-blur")?,
            blur_deep: sheet.px("--motion-blur-deep")?,
            focus: sheet.ms("--motion-focus")?,
            ease_out: sheet.bezier("--ease-out")?,
            ease_focus: sheet.bezier("--ease-focus")?,
            spring_overshoot: sheet.linear_peak("--ease-spring")?.0 - 1.0,
            spring_peak_at: sheet.linear_peak("--ease-spring")?.1,
            scale_defocus: sheet.number("--scale-defocus")?,
        })
    }

    /// The window's spring: growth swings past its target as `--ease-spring` does.
    pub fn grow_spring(&self) -> Spring {
        Spring::from_easing(self.spring, self.spring_overshoot, self.spring_peak_at)
    }

    /// Shrinking never passes its target, so a resting island never dips below the camera.
    pub fn shrink_spring(&self) -> Spring {
        Spring::critical(self.slow, self.grow_spring(), self.spring)
    }
}

/// Parsed once; an error names the token and is a build defect the unit tests catch.
pub fn tokens() -> Result<&'static IslandTokens, &'static TokenError> {
    static TOKENS: OnceLock<Result<IslandTokens, TokenError>> = OnceLock::new();
    TOKENS
        .get_or_init(|| Sheet::parse(STYLESHEET).and_then(|sheet| IslandTokens::from_sheet(&sheet)))
        .as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, tolerance: f64) -> bool {
        (a - b).abs() <= tolerance
    }

    #[test]
    fn the_real_stylesheet_has_every_island_token() {
        let tokens = tokens().expect("every island token parses from styles.css");
        assert_eq!(tokens.spring, 0.48);
        assert_eq!(tokens.slow, 0.34);
        assert_eq!(tokens.base, 0.22);
        assert_eq!(tokens.fast, 0.14);
        assert_eq!(tokens.stagger, 0.04);
        assert_eq!((tokens.blur, tokens.blur_deep), (3.0, 14.0));
        assert_eq!(tokens.focus, 0.36);
        assert_eq!(tokens.ease_out, [0.22, 1.0, 0.36, 1.0]);
        assert_eq!(tokens.ease_focus, [0.3, 0.0, 0.2, 1.0]);
        assert!(close(tokens.spring_overshoot, 0.038, 1e-9));
        assert!(close(tokens.spring_peak_at, 0.44, 1e-9));
        assert_eq!(tokens.scale_defocus, 0.94);
        assert_eq!(
            (tokens.wing_min, tokens.wing_max, tokens.stroke_bold),
            (44.0, 160.0, 2.0)
        );
        assert_eq!(tokens.island_fill.srgb(), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(tokens.weight_medium, 500.0);
    }

    #[test]
    fn dark_colors_come_from_light_dark_and_oklch_converts_to_srgb() {
        let tokens = tokens().unwrap();
        // rgb(245 245 246), the dark --foreground, round-trips through OKLab.
        let [r, g, b, a] = tokens.foreground.srgb();
        assert!(close(r * 255.0, 245.0, 0.01) && close(g * 255.0, 245.0, 0.01));
        assert!(close(b * 255.0, 246.0, 0.01) && a == 1.0);
        // The tray's hand-converted dark --destructive is rgb(255 99 103).
        let [r, g, b, _] = tokens.destructive.srgb();
        assert!(
            close(r * 255.0, 255.0, 1.5)
                && close(g * 255.0, 99.0, 1.5)
                && close(b * 255.0, 103.0, 1.5)
        );
    }

    #[test]
    fn the_fixture_grammar_covers_every_value_form() {
        let sheet = Sheet::parse(
            "a { --x: 1px; } :root { /* note; with a colon: here */ --len: 12px; --dur: 90ms; \
             --num: .97; --pct: 26%; --alias: var(--len); --deep: var(--alias); \
             --rgb: light-dark(rgb(0 0 0), rgb(255 0 0)); --ok: oklch(.7 .19 28 / 50%); \
             --ease: cubic-bezier(.22, 1, .36, 1); --spring: linear(0, 1.2 30%, 0.9 60%, 1); \
             --word: auto; }",
        )
        .unwrap();
        assert_eq!(sheet.px("--len"), Ok(12.0));
        assert_eq!(sheet.px("--deep"), Ok(12.0));
        assert_eq!(sheet.ms("--dur"), Ok(0.09));
        assert_eq!(sheet.number("--num"), Ok(0.97));
        let [r, g, b, _] = sheet.color("--rgb").unwrap().srgb();
        assert!(close(r, 1.0, 1e-6) && close(g, 0.0, 1e-6) && close(b, 0.0, 1e-6));
        assert!(close(sheet.color("--ok").unwrap().alpha, 0.5, 1e-9));
        assert_eq!(sheet.bezier("--ease"), Ok([0.22, 1.0, 0.36, 1.0]));
        assert_eq!(sheet.linear_peak("--spring"), Ok((1.2, 0.3)));
        assert!(sheet.px("--x").is_err(), "only :root counts");
        assert!(sheet.px("--missing").is_err());
        assert!(sheet.px("--dur").is_err());
        assert!(sheet.ms("--len").is_err());
        assert!(sheet.color("--word").is_err());
        assert!(sheet.bezier("--spring").is_err());
        assert!(sheet.number("--pct").is_err());
    }

    #[test]
    fn the_grow_spring_reproduces_the_css_spring() {
        let tokens = tokens().unwrap();
        let spring = tokens.grow_spring();
        assert!(close(spring.ratio, 0.72, 0.005), "ratio {}", spring.ratio);
        assert!(close(spring.overshoot(), 0.038, 1e-6));
        // The peak lands where the CSS curve puts it: 44% of 480 ms.
        let peak_time = (0..2000)
            .map(|step| f64::from(step) / 4000.0)
            .max_by(|a, b| {
                spring
                    .progress(*a, 0.0)
                    .total_cmp(&spring.progress(*b, 0.0))
            })
            .unwrap();
        assert!(close(peak_time, 0.44 * 0.48, 0.002), "peak at {peak_time}");
        assert!(close(spring.progress(peak_time, 0.0), 1.038, 1e-3));
        // Mass 1: stiffness and damping follow from frequency and ratio.
        assert!(close(spring.stiffness(), spring.omega * spring.omega, 1e-9));
        assert!(close(
            spring.damping(),
            2.0 * spring.ratio * spring.omega,
            1e-9
        ));
    }

    #[test]
    fn the_shrink_spring_never_overshoots_and_settles_by_its_duration() {
        let tokens = tokens().unwrap();
        let spring = tokens.shrink_spring();
        assert_eq!(spring.overshoot(), 0.0);
        let mut previous = 0.0;
        for step in 0..1000 {
            let progress = spring.progress(f64::from(step) / 1000.0, 0.0);
            assert!(progress <= 1.0 + 1e-12 && progress >= previous - 1e-12);
            previous = progress;
        }
        assert!(spring.settle_time(300.0, 0.5, 0.0) <= tokens.slow + 0.01);
    }

    #[test]
    fn settle_time_and_velocity_agree_with_the_curve() {
        let spring = tokens().unwrap().grow_spring();
        let travel = 240.0;
        let settle = spring.settle_time(travel, 0.5, 0.0);
        assert!(settle > 0.2 && settle < 0.6, "settle {settle}");
        for step in 0..200 {
            let time = settle + f64::from(step) / 100.0;
            assert!((spring.progress(time, 0.0) - 1.0).abs() * travel <= 0.5 + 1e-9);
        }
        // Velocity is the derivative of progress, including a launch speed.
        for velocity in [0.0, 3.0, -2.0] {
            for time in [0.01, 0.1, 0.3] {
                let numeric = (spring.progress(time + 1e-6, velocity)
                    - spring.progress(time - 1e-6, velocity))
                    / 2e-6;
                assert!(close(numeric, spring.velocity(time, velocity), 1e-3));
            }
            assert!(close(spring.velocity(0.0, velocity), velocity, 1e-9));
        }
        assert!(spring.peak_speed(0.0) > 1.0);
        // From rest it swings past by its overshoot and never below the start.
        let (low, high) = spring.excursion(0.0, settle);
        assert!(close(low, 0.0, 1e-9) && close(high, 1.038, 1e-3));
        // Launched hard backwards it first dips below the start.
        let (low, _) = spring.excursion(-20.0, settle);
        assert!(low < -0.05, "low {low}");
    }

    #[test]
    fn cubic_bezier_easings_run_from_zero_to_one() {
        let ease_out = tokens().unwrap().ease_out;
        assert!(close(ease_at(ease_out, 0.0), 0.0, 1e-9));
        assert!(close(ease_at(ease_out, 1.0), 1.0, 1e-9));
        assert!(
            close(ease_at([0.0, 0.0, 1.0, 1.0], 0.3), 0.3, 1e-9),
            "linear stays linear"
        );
        // An ease-out is past halfway at half time, and never steps back.
        assert!(ease_at(ease_out, 0.5) > 0.8);
        let mut previous = 0.0;
        for step in 0..=100 {
            let value = ease_at(ease_out, f64::from(step) / 100.0);
            assert!(value >= previous - 1e-12);
            previous = value;
        }
    }

    /// Arriving content must be seen soft before it sharpens: once its fade-in is nine tenths
    /// done, more than half the blur is still on, and the blur still clears by the end.
    #[test]
    fn the_fade_in_leads_the_focus() {
        let tokens = tokens().unwrap();
        assert!(tokens.focus > tokens.base);
        let faded_in = (0..=1000)
            .map(|step| f64::from(step) / 1000.0 * tokens.base)
            .find(|time| ease_at(tokens.ease_out, time / tokens.base) >= 0.9)
            .unwrap();
        let blur_left = 1.0 - ease_at(tokens.ease_focus, faded_in / tokens.focus);
        assert!(blur_left > 0.5, "blur left {blur_left} at {faded_in} s");
        assert!(close(ease_at(tokens.ease_focus, 1.0), 1.0, 1e-9));
    }

    #[test]
    fn overshoot_and_ratio_invert_each_other() {
        for overshoot in [0.01, 0.038, 0.2] {
            let ratio = ratio_for_overshoot(overshoot);
            let spring = Spring { omega: 10.0, ratio };
            assert!(close(spring.overshoot(), overshoot, 1e-9));
        }
        assert_eq!(ratio_for_overshoot(0.0), 1.0);
    }
}
