//! Typed CSS values.

use std::fmt;

/// Length units understood by the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Px,
    /// Relative to the element's font size (parent's, for `font-size`).
    Em,
    /// Relative to the root (html) font size.
    Rem,
    /// Percent of the viewport width / height.
    Vw,
    Vh,
    /// Percent of the smaller / larger viewport dimension.
    Vmin,
    Vmax,
    Percent,
}

/// An sRGB color with alpha (255 = opaque).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    /// An opaque color.
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    #[must_use]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    #[must_use]
    pub const fn is_opaque(&self) -> bool {
        self.a == 255
    }

    /// This color with its alpha scaled by `factor` (for `opacity`).
    #[must_use]
    pub fn with_alpha_factor(&self, factor: f32) -> Self {
        Self {
            a: (f32::from(self.a) * factor.clamp(0.0, 1.0)) as u8,
            ..*self
        }
    }

    /// Parses `#rgb[a]`, `#rrggbb[aa]`, `rgb()`/`rgba()`, `hsl()`/`hsla()`
    /// or a CSS named color.
    #[must_use]
    pub fn parse(source: &str) -> Option<Self> {
        let source = source.trim();
        if let Some(hex) = source.strip_prefix('#') {
            return Self::parse_hex(hex);
        }
        for prefix in ["rgba(", "rgb("] {
            if let Some(body) = source
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix(')'))
            {
                return Self::parse_rgb_body(body);
            }
        }
        for prefix in ["hsla(", "hsl("] {
            if let Some(body) = source
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix(')'))
            {
                return Self::parse_hsl_body(body);
            }
        }
        for prefix in ["hwb("] {
            if let Some(body) = source
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix(')'))
            {
                return Self::parse_hwb_body(body);
            }
        }
        // The perceptual spaces share a shape: three components and an
        // optional alpha, differing only in how the components map onto
        // their axes.
        for (prefix, space) in [
            ("oklch(", Space::Oklch),
            ("oklab(", Space::Oklab),
            ("lch(", Space::Lch),
            ("lab(", Space::Lab),
        ] {
            if let Some(body) = source
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix(')'))
            {
                return Self::parse_perceptual_body(body, space);
            }
        }
        if let Some(body) = source
            .strip_prefix("color-mix(")
            .and_then(|rest| rest.strip_suffix(')'))
        {
            return Self::parse_mix_body(body);
        }
        Self::from_named(source)
    }

    /// `hwb(H W B[ / A])`: a hue washed with white and blackened.
    fn parse_hwb_body(body: &str) -> Option<Self> {
        let (parts, alpha) = split_color_components(body)?;
        let [hue, white, black] = parts[..] else {
            return None;
        };
        let hue = parse_hue(hue)?;
        let white = parse_percent(white)?;
        let black = parse_percent(black)?;
        // Once white and black together fill the color, nothing of the
        // hue is left and their ratio decides the gray.
        let (r, g, b) = if white + black >= 1.0 {
            let gray = white / (white + black);
            (gray, gray, gray)
        } else {
            let (r, g, b) = hsl_to_rgb(hue, 1.0, 0.5);
            let wash = |channel: u8| f32::from(channel) / 255.0 * (1.0 - white - black) + white;
            (wash(r), wash(g), wash(b))
        };
        Some(Self::from_unit_rgb(r, g, b, alpha?))
    }

    /// The Lab-family functions, which all describe a color by lightness
    /// plus two axes — either rectangular (`a`/`b`) or polar (chroma and
    /// hue) — and differ only in scale.
    fn parse_perceptual_body(body: &str, space: Space) -> Option<Self> {
        let (parts, alpha) = split_color_components(body)?;
        let [first, second, third] = parts[..] else {
            return None;
        };
        let lightness = parse_number_or_percent(first, space.lightness_scale())?;
        let (a, b) = if space.is_polar() {
            let chroma = parse_number_or_percent(second, space.chroma_scale())?;
            let hue = parse_hue(third)?.to_radians();
            (chroma * hue.cos(), chroma * hue.sin())
        } else {
            (
                parse_number_or_percent(second, space.axis_scale())?,
                parse_number_or_percent(third, space.axis_scale())?,
            )
        };
        let (red, green, blue) = if space.is_ok() {
            oklab_to_linear_rgb(lightness, a, b)
        } else {
            lab_to_linear_rgb(lightness, a, b)
        };
        Some(Self::from_unit_rgb(
            gamma_encode(red),
            gamma_encode(green),
            gamma_encode(blue),
            alpha?,
        ))
    }

    /// `color-mix(in <space>, A [p%], B [q%])`.
    fn parse_mix_body(body: &str) -> Option<Self> {
        let mut parts = split_top_level(body, ',');
        if parts.len() != 3 {
            return None;
        }
        let tail = parts.split_off(1);
        let space = parts[0]
            .trim()
            .strip_prefix("in ")?
            .trim()
            .to_ascii_lowercase();
        let (first, first_weight) = parse_mix_operand(tail[0])?;
        let (second, second_weight) = parse_mix_operand(tail[1])?;
        // Unstated weights split what the stated ones leave.
        let (first_weight, second_weight) = match (first_weight, second_weight) {
            (Some(a), Some(b)) if a + b > 0.0 => (a / (a + b), b / (a + b)),
            (Some(a), None) => (a.clamp(0.0, 1.0), 1.0 - a.clamp(0.0, 1.0)),
            (None, Some(b)) => (1.0 - b.clamp(0.0, 1.0), b.clamp(0.0, 1.0)),
            _ => (0.5, 0.5),
        };
        // Mixing in a perceptual space keeps the midpoint looking
        // halfway; mixing in sRGB does not, so the space is honored.
        let perceptual = matches!(space.as_str(), "oklab" | "oklch");
        let coordinates = |color: Self| -> [f32; 3] {
            let channels = [
                f32::from(color.r) / 255.0,
                f32::from(color.g) / 255.0,
                f32::from(color.b) / 255.0,
            ];
            if perceptual {
                linear_rgb_to_oklab(
                    gamma_decode(channels[0]),
                    gamma_decode(channels[1]),
                    gamma_decode(channels[2]),
                )
            } else {
                channels
            }
        };
        let (left, right) = (coordinates(first), coordinates(second));
        let blend = |index: usize| left[index] * first_weight + right[index] * second_weight;
        let mixed = [blend(0), blend(1), blend(2)];
        let (r, g, b) = if perceptual {
            let (r, g, b) = oklab_to_linear_rgb(mixed[0], mixed[1], mixed[2]);
            (gamma_encode(r), gamma_encode(g), gamma_encode(b))
        } else {
            (mixed[0], mixed[1], mixed[2])
        };
        let alpha =
            (f32::from(first.a) * first_weight + f32::from(second.a) * second_weight) / 255.0;
        Some(Self::from_unit_rgb(r, g, b, alpha))
    }

    /// Builds a color from 0..=1 channels, clamping what falls outside
    /// the sRGB gamut onto its edge.
    fn from_unit_rgb(r: f32, g: f32, b: f32, alpha: f32) -> Self {
        let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
        Self {
            r: channel(r),
            g: channel(g),
            b: channel(b),
            a: channel(alpha),
        }
    }

    fn parse_rgb_body(body: &str) -> Option<Self> {
        // Legacy form is comma-separated; the modern form is
        // space-separated with the alpha after a `/`.
        let (body, slash_alpha) = match body.split_once('/') {
            Some((channels, alpha)) => (channels, Some(alpha.trim())),
            None => (body, None),
        };
        let parts: Vec<&str> = if body.contains(',') {
            body.split(',').map(str::trim).collect()
        } else {
            body.split_whitespace().collect()
        };
        let alpha = match (parts.len(), slash_alpha) {
            (3, None) => None,
            (4, None) => Some(parts[3]),
            (3, Some(alpha)) => Some(alpha),
            _ => return None,
        };
        // Channels accept integers (0-255) or percentages of 255.
        let channel = |part: &str| {
            if let Some(percent) = part.strip_suffix('%') {
                let percent: f32 = percent.parse().ok()?;
                Some((percent.clamp(0.0, 100.0) / 100.0 * 255.0).round() as u8)
            } else {
                part.parse::<u8>().ok()
            }
        };
        let color = Self::rgb(channel(parts[0])?, channel(parts[1])?, channel(parts[2])?);
        match alpha {
            None => Some(color),
            Some(alpha) => {
                // Alpha accepts a fraction (0-1) or a percentage.
                let alpha: f32 = if let Some(percent) = alpha.strip_suffix('%') {
                    percent.parse::<f32>().ok()? / 100.0
                } else {
                    alpha.parse().ok()?
                };
                Some(Self {
                    a: (alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
                    ..color
                })
            }
        }
    }

    fn parse_hsl_body(body: &str) -> Option<Self> {
        // Legacy form is comma-separated; the modern form is
        // space-separated with the alpha after a `/`.
        let (body, slash_alpha) = match body.split_once('/') {
            Some((channels, alpha)) => (channels, Some(alpha.trim())),
            None => (body, None),
        };
        let parts: Vec<&str> = if body.contains(',') {
            body.split(',').map(str::trim).collect()
        } else {
            body.split_whitespace().collect()
        };
        let alpha = match (parts.len(), slash_alpha) {
            (3, None) => None,
            (4, None) => Some(parts[3]),
            (3, Some(alpha)) => Some(alpha),
            _ => return None,
        };
        let alpha = match alpha {
            None => 255u8,
            // Alpha accepts a fraction (0-1) or a percentage.
            Some(alpha) => {
                let alpha: f32 = match alpha.strip_suffix('%') {
                    Some(percent) => percent.parse::<f32>().ok()? / 100.0,
                    None => alpha.parse().ok()?,
                };
                (alpha.clamp(0.0, 1.0) * 255.0).round() as u8
            }
        };
        let hue: f32 = parts[0]
            .strip_suffix("deg")
            .unwrap_or(parts[0])
            .parse()
            .ok()?;
        let saturation: f32 = parts[1].strip_suffix('%')?.parse().ok()?;
        let lightness: f32 = parts[2].strip_suffix('%')?.parse().ok()?;
        let (r, g, b) = hsl_to_rgb(
            hue.rem_euclid(360.0),
            (saturation / 100.0).clamp(0.0, 1.0),
            (lightness / 100.0).clamp(0.0, 1.0),
        );
        Some(Self { r, g, b, a: alpha })
    }

    fn parse_hex(hex: &str) -> Option<Self> {
        let value = u32::from_str_radix(hex, 16).ok()?;
        let nib = |shift: u32| {
            let n = ((value >> shift) & 0xf) as u8;
            n << 4 | n
        };
        match hex.len() {
            3 => Some(Self::rgb(nib(8), nib(4), nib(0))),
            4 => Some(Self::rgba(nib(12), nib(8), nib(4), nib(0))),
            6 => Some(Self::rgb(
                ((value >> 16) & 0xff) as u8,
                ((value >> 8) & 0xff) as u8,
                (value & 0xff) as u8,
            )),
            8 => Some(Self::rgba(
                ((value >> 24) & 0xff) as u8,
                ((value >> 16) & 0xff) as u8,
                ((value >> 8) & 0xff) as u8,
                (value & 0xff) as u8,
            )),
            _ => None,
        }
    }

    /// The full CSS named color table.
    #[must_use]
    pub fn from_named(name: &str) -> Option<Self> {
        NAMED_COLORS
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, [r, g, b])| Self::rgb(*r, *g, *b))
    }
}

/// Which Lab-family function a value came from. They share a parser and
/// differ only in the scale of their components.
#[derive(Clone, Copy)]
enum Space {
    Lab,
    Lch,
    Oklab,
    Oklch,
}

impl Space {
    const fn is_ok(self) -> bool {
        matches!(self, Self::Oklab | Self::Oklch)
    }

    const fn is_polar(self) -> bool {
        matches!(self, Self::Lch | Self::Oklch)
    }

    /// What `100%` means for the lightness component.
    const fn lightness_scale(self) -> f32 {
        if self.is_ok() { 1.0 } else { 100.0 }
    }

    /// What `100%` means on the `a`/`b` axes.
    const fn axis_scale(self) -> f32 {
        if self.is_ok() { 0.4 } else { 125.0 }
    }

    /// What `100%` means for chroma.
    const fn chroma_scale(self) -> f32 {
        if self.is_ok() { 0.4 } else { 150.0 }
    }
}

/// Splits a modern color body into its three components and an alpha,
/// which follows a `/`. A missing alpha is opaque.
fn split_color_components(body: &str) -> Option<(Vec<&str>, Option<f32>)> {
    let (channels, alpha) = match body.split_once('/') {
        Some((channels, alpha)) => (channels, Some(parse_alpha(alpha)?)),
        None => (body, Some(1.0)),
    };
    Some((channels.split_whitespace().collect(), alpha))
}

/// Splits on a separator that is not inside parentheses, so a nested
/// function keeps its own commas.
fn split_top_level(source: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (index, character) in source.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if character == separator && depth == 0 => {
                parts.push(&source[start..index]);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&source[start..]);
    parts
}

/// One side of a `color-mix()`: a color and, when stated, its share.
fn parse_mix_operand(source: &str) -> Option<(Color, Option<f32>)> {
    let source = source.trim();
    // The percentage may lead or trail the color.
    if let Some((color, percent)) = source.rsplit_once(' ')
        && let Some(percent) = percent.trim().strip_suffix('%')
        && let Ok(percent) = percent.parse::<f32>()
    {
        return Some((Color::parse(color.trim())?, Some(percent / 100.0)));
    }
    if let Some((percent, color)) = source.split_once(' ')
        && let Some(percent) = percent.trim().strip_suffix('%')
        && let Ok(percent) = percent.parse::<f32>()
    {
        return Some((Color::parse(color.trim())?, Some(percent / 100.0)));
    }
    Some((Color::parse(source)?, None))
}

/// An alpha as a fraction or a percentage; `none` counts as opaque.
fn parse_alpha(source: &str) -> Option<f32> {
    let source = source.trim();
    if source.eq_ignore_ascii_case("none") {
        return Some(1.0);
    }
    let value = match source.strip_suffix('%') {
        Some(percent) => percent.trim().parse::<f32>().ok()? / 100.0,
        None => source.parse::<f32>().ok()?,
    };
    Some(value.clamp(0.0, 1.0))
}

/// A `0%`..`100%` component as a 0..1 fraction.
fn parse_percent(source: &str) -> Option<f32> {
    if source.eq_ignore_ascii_case("none") {
        return Some(0.0);
    }
    let percent: f32 = source.strip_suffix('%')?.trim().parse().ok()?;
    Some((percent / 100.0).clamp(0.0, 1.0))
}

/// A component written either as a number on its own scale or as a
/// percentage of `full`.
fn parse_number_or_percent(source: &str, full: f32) -> Option<f32> {
    if source.eq_ignore_ascii_case("none") {
        return Some(0.0);
    }
    match source.strip_suffix('%') {
        Some(percent) => Some(percent.trim().parse::<f32>().ok()? / 100.0 * full),
        None => source.parse().ok(),
    }
}

/// A hue in degrees, wrapped into one turn. Other angle units are read
/// as the turns they stand for.
fn parse_hue(source: &str) -> Option<f32> {
    if source.eq_ignore_ascii_case("none") {
        return Some(0.0);
    }
    let (number, per_turn) = if let Some(number) = source.strip_suffix("deg") {
        (number, 1.0)
    } else if let Some(number) = source.strip_suffix("grad") {
        (number, 360.0 / 400.0)
    } else if let Some(number) = source.strip_suffix("rad") {
        (number, 360.0 / std::f32::consts::TAU)
    } else if let Some(number) = source.strip_suffix("turn") {
        (number, 360.0)
    } else {
        (source, 1.0)
    };
    let degrees: f32 = number.trim().parse().ok()?;
    Some((degrees * per_turn).rem_euclid(360.0))
}

/// sRGB transfer function, linear light to the encoded channel.
fn gamma_encode(channel: f32) -> f32 {
    if channel <= 0.003_130_8 {
        channel * 12.92
    } else {
        1.055 * channel.powf(1.0 / 2.4) - 0.055
    }
}

/// The inverse: an encoded channel back to linear light.
fn gamma_decode(channel: f32) -> f32 {
    if channel <= 0.040_45 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

/// Oklab to linear sRGB (Ottosson's matrices).
fn oklab_to_linear_rgb(lightness: f32, a: f32, b: f32) -> (f32, f32, f32) {
    let l = (lightness + 0.396_337_78 * a + 0.215_803_76 * b).powi(3);
    let m = (lightness - 0.105_561_346 * a - 0.063_854_17 * b).powi(3);
    let s = (lightness - 0.089_484_18 * a - 1.291_485_5 * b).powi(3);
    (
        4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
        -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
        -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
    )
}

/// Linear sRGB to Oklab, for mixing in a perceptual space.
fn linear_rgb_to_oklab(red: f32, green: f32, blue: f32) -> [f32; 3] {
    let l = (0.412_221_46 * red + 0.536_332_55 * green + 0.051_445_995 * blue).cbrt();
    let m = (0.211_903_5 * red + 0.680_699_5 * green + 0.107_396_96 * blue).cbrt();
    let s = (0.088_302_46 * red + 0.281_718_85 * green + 0.629_978_5 * blue).cbrt();
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

/// CIE Lab (D50, as CSS specifies) to linear sRGB.
fn lab_to_linear_rgb(lightness: f32, a: f32, b: f32) -> (f32, f32, f32) {
    const EPSILON: f32 = 216.0 / 24389.0;
    const KAPPA: f32 = 24389.0 / 27.0;
    // The D50 white point CSS measures Lab against.
    const WHITE: [f32; 3] = [0.964_22, 1.0, 0.825_21];

    let fy = (lightness + 16.0) / 116.0;
    let fx = fy + a / 500.0;
    let fz = fy - b / 200.0;
    let invert = |f: f32| {
        let cubed = f * f * f;
        if cubed > EPSILON {
            cubed
        } else {
            (116.0 * f - 16.0) / KAPPA
        }
    };
    let (x, y, z) = (
        invert(fx) * WHITE[0],
        invert(fy) * WHITE[1],
        invert(fz) * WHITE[2],
    );
    // D50 XYZ straight to linear sRGB (Bradford-adapted).
    (
        3.134_136 * x - 1.617_386_3 * y - 0.490_661_95 * z,
        -0.978_768_45 * x + 1.916_141_6 * y + 0.033_454_068 * z,
        0.071_945_4 * x - 0.228_999_4 * y + 1.405_356_3 * z,
    )
}

fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> (u8, u8, u8) {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let hue_prime = hue / 60.0;
    let x = chroma * (1.0 - (hue_prime % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hue_prime as u32 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let m = lightness - chroma / 2.0;
    let channel = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    (channel(r1), channel(g1), channel(b1))
}

/// CSS Color Module named colors (sRGB).
const NAMED_COLORS: &[(&str, [u8; 3])] = &[
    ("aliceblue", [240, 248, 255]),
    ("antiquewhite", [250, 235, 215]),
    ("aqua", [0, 255, 255]),
    ("aquamarine", [127, 255, 212]),
    ("azure", [240, 255, 255]),
    ("beige", [245, 245, 220]),
    ("bisque", [255, 228, 196]),
    ("black", [0, 0, 0]),
    ("blanchedalmond", [255, 235, 205]),
    ("blue", [0, 0, 255]),
    ("blueviolet", [138, 43, 226]),
    ("brown", [165, 42, 42]),
    ("burlywood", [222, 184, 135]),
    ("cadetblue", [95, 158, 160]),
    ("chartreuse", [127, 255, 0]),
    ("chocolate", [210, 105, 30]),
    ("coral", [255, 127, 80]),
    ("cornflowerblue", [100, 149, 237]),
    ("cornsilk", [255, 248, 220]),
    ("crimson", [220, 20, 60]),
    ("cyan", [0, 255, 255]),
    ("darkblue", [0, 0, 139]),
    ("darkcyan", [0, 139, 139]),
    ("darkgoldenrod", [184, 134, 11]),
    ("darkgray", [169, 169, 169]),
    ("darkgreen", [0, 100, 0]),
    ("darkgrey", [169, 169, 169]),
    ("darkkhaki", [189, 183, 107]),
    ("darkmagenta", [139, 0, 139]),
    ("darkolivegreen", [85, 107, 47]),
    ("darkorange", [255, 140, 0]),
    ("darkorchid", [153, 50, 204]),
    ("darkred", [139, 0, 0]),
    ("darksalmon", [233, 150, 122]),
    ("darkseagreen", [143, 188, 143]),
    ("darkslateblue", [72, 61, 139]),
    ("darkslategray", [47, 79, 79]),
    ("darkslategrey", [47, 79, 79]),
    ("darkturquoise", [0, 206, 209]),
    ("darkviolet", [148, 0, 211]),
    ("deeppink", [255, 20, 147]),
    ("deepskyblue", [0, 191, 255]),
    ("dimgray", [105, 105, 105]),
    ("dimgrey", [105, 105, 105]),
    ("dodgerblue", [30, 144, 255]),
    ("firebrick", [178, 34, 34]),
    ("floralwhite", [255, 250, 240]),
    ("forestgreen", [34, 139, 34]),
    ("fuchsia", [255, 0, 255]),
    ("gainsboro", [220, 220, 220]),
    ("ghostwhite", [248, 248, 255]),
    ("gold", [255, 215, 0]),
    ("goldenrod", [218, 165, 32]),
    ("gray", [128, 128, 128]),
    ("green", [0, 128, 0]),
    ("greenyellow", [173, 255, 47]),
    ("grey", [128, 128, 128]),
    ("honeydew", [240, 255, 240]),
    ("hotpink", [255, 105, 180]),
    ("indianred", [205, 92, 92]),
    ("indigo", [75, 0, 130]),
    ("ivory", [255, 255, 240]),
    ("khaki", [240, 230, 140]),
    ("lavender", [230, 230, 250]),
    ("lavenderblush", [255, 240, 245]),
    ("lawngreen", [124, 252, 0]),
    ("lemonchiffon", [255, 250, 205]),
    ("lightblue", [173, 216, 230]),
    ("lightcoral", [240, 128, 128]),
    ("lightcyan", [224, 255, 255]),
    ("lightgoldenrodyellow", [250, 250, 210]),
    ("lightgray", [211, 211, 211]),
    ("lightgreen", [144, 238, 144]),
    ("lightgrey", [211, 211, 211]),
    ("lightpink", [255, 182, 193]),
    ("lightsalmon", [255, 160, 122]),
    ("lightseagreen", [32, 178, 170]),
    ("lightskyblue", [135, 206, 250]),
    ("lightslategray", [119, 136, 153]),
    ("lightslategrey", [119, 136, 153]),
    ("lightsteelblue", [176, 196, 222]),
    ("lightyellow", [255, 255, 224]),
    ("lime", [0, 255, 0]),
    ("limegreen", [50, 205, 50]),
    ("linen", [250, 240, 230]),
    ("magenta", [255, 0, 255]),
    ("maroon", [128, 0, 0]),
    ("mediumaquamarine", [102, 205, 170]),
    ("mediumblue", [0, 0, 205]),
    ("mediumorchid", [186, 85, 211]),
    ("mediumpurple", [147, 112, 219]),
    ("mediumseagreen", [60, 179, 113]),
    ("mediumslateblue", [123, 104, 238]),
    ("mediumspringgreen", [0, 250, 154]),
    ("mediumturquoise", [72, 209, 204]),
    ("mediumvioletred", [199, 21, 133]),
    ("midnightblue", [25, 25, 112]),
    ("mintcream", [245, 255, 250]),
    ("mistyrose", [255, 228, 225]),
    ("moccasin", [255, 228, 181]),
    ("navajowhite", [255, 222, 173]),
    ("navy", [0, 0, 128]),
    ("oldlace", [253, 245, 230]),
    ("olive", [128, 128, 0]),
    ("olivedrab", [107, 142, 35]),
    ("orange", [255, 165, 0]),
    ("orangered", [255, 69, 0]),
    ("orchid", [218, 112, 214]),
    ("palegoldenrod", [238, 232, 170]),
    ("palegreen", [152, 251, 152]),
    ("paleturquoise", [175, 238, 238]),
    ("palevioletred", [219, 112, 147]),
    ("papayawhip", [255, 239, 213]),
    ("peachpuff", [255, 218, 185]),
    ("peru", [205, 133, 63]),
    ("pink", [255, 192, 203]),
    ("plum", [221, 160, 221]),
    ("powderblue", [176, 224, 230]),
    ("purple", [128, 0, 128]),
    ("rebeccapurple", [102, 51, 153]),
    ("red", [255, 0, 0]),
    ("rosybrown", [188, 143, 143]),
    ("royalblue", [65, 105, 225]),
    ("saddlebrown", [139, 69, 19]),
    ("salmon", [250, 128, 114]),
    ("sandybrown", [244, 164, 96]),
    ("seagreen", [46, 139, 87]),
    ("seashell", [255, 245, 238]),
    ("sienna", [160, 82, 45]),
    ("silver", [192, 192, 192]),
    ("skyblue", [135, 206, 235]),
    ("slateblue", [106, 90, 205]),
    ("slategray", [112, 128, 144]),
    ("slategrey", [112, 128, 144]),
    ("snow", [255, 250, 250]),
    ("springgreen", [0, 255, 127]),
    ("steelblue", [70, 130, 180]),
    ("tan", [210, 180, 140]),
    ("teal", [0, 128, 128]),
    ("thistle", [216, 191, 216]),
    ("tomato", [255, 99, 71]),
    ("turquoise", [64, 224, 208]),
    ("violet", [238, 130, 238]),
    ("wheat", [245, 222, 179]),
    ("white", [255, 255, 255]),
    ("whitesmoke", [245, 245, 245]),
    ("yellow", [255, 255, 0]),
    ("yellowgreen", [154, 205, 50]),
];

impl fmt::Display for Color {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_opaque() {
            write!(formatter, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            write!(
                formatter,
                "rgba({},{},{},{:.3})",
                self.r,
                self.g,
                self.b,
                f32::from(self.a) / 255.0
            )
        }
    }
}

/// A single parsed component of a declaration value.
#[derive(Debug, Clone, PartialEq)]
pub enum CssValue {
    /// An identifier the engine interprets later (`block`, `bold`, `transparent`, ...).
    Keyword(String),
    /// A quoted string (`content: "..."`).
    String(String),
    /// `url(...)` with the URL unquoted.
    Url(String),
    /// A raw declaration value containing `var()`, substituted (and then
    /// re-parsed) at style-computation time.
    Unresolved(String),
    /// An unparsed functional value: (lowercase name, raw arguments).
    Function(String, String),
    Length(f32, Unit),
    Color(Color),
    Number(f32),
    Auto,
}

impl CssValue {
    /// The raw text of string-ish values (String/Keyword without quotes);
    /// other values through their Display form.
    #[must_use]
    pub fn raw_text(&self) -> String {
        match self {
            Self::String(text) | Self::Keyword(text) | Self::Unresolved(text) => text.clone(),
            other => other.to_string(),
        }
    }

    /// Parses one whitespace-delimited value component.
    ///
    /// Returns `None` for components the engine cannot represent (which the
    /// caller then skips, per the "ignore unsupported declarations" rule).
    #[must_use]
    pub fn parse_component(source: &str) -> Option<Self> {
        // `f32` parsing accepts "NaN"/"inf" spellings and overflowing
        // literals: reject non-finite numerics here so they can never
        // leak into layout/paint math downstream.
        match Self::parse_component_inner(source) {
            Some(Self::Length(value, _) | Self::Number(value)) if !value.is_finite() => None,
            parsed => parsed,
        }
    }

    fn parse_component_inner(source: &str) -> Option<Self> {
        let source = source.trim();
        if source.is_empty() {
            return None;
        }
        if source.eq_ignore_ascii_case("auto") {
            return Some(Self::Auto);
        }
        // url(...) and functional values (linear-gradient(...), ...).
        if source.len() >= "url()".len()
            && source
                .get(..4)
                .is_some_and(|head| head.eq_ignore_ascii_case("url("))
            && source.ends_with(')')
        {
            let inner = source[4..source.len() - 1].trim();
            let inner = inner
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
                .or_else(|| {
                    inner
                        .strip_prefix('\'')
                        .and_then(|rest| rest.strip_suffix('\''))
                })
                .unwrap_or(inner);
            return Some(Self::Url(inner.to_string()));
        }
        if let Some(open) = source.find('(')
            && source.ends_with(')')
            && source[..open]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !source[..open].is_empty()
        {
            let name = source[..open].to_ascii_lowercase();
            // Colors and shape functions parsed elsewhere keep their path.
            if !matches!(
                name.as_str(),
                "rgb"
                    | "rgba"
                    | "hsl"
                    | "hsla"
                    | "hwb"
                    | "lab"
                    | "lch"
                    | "oklab"
                    | "oklch"
                    | "color-mix"
            ) {
                let arguments = source[open + 1..source.len() - 1].to_string();
                return Some(Self::Function(name, arguments));
            }
        }
        // Quoted strings (kept whole by split_components).
        if let Some(inner) = source
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .or_else(|| {
                source
                    .strip_prefix('\'')
                    .and_then(|rest| rest.strip_suffix('\''))
            })
        {
            return Some(Self::String(inner.to_string()));
        }
        if source.starts_with('#')
            || [
                "rgb(",
                "rgba(",
                "hsl(",
                "hsla(",
                "hwb(",
                "lab(",
                "lch(",
                "oklab(",
                "oklch(",
                "color-mix(",
            ]
            .iter()
            .any(|prefix| source.starts_with(prefix))
        {
            return Color::parse(source).map(Self::Color);
        }
        if let Some(number) = source.strip_suffix("px") {
            return number
                .trim()
                .parse()
                .ok()
                .map(|v| Self::Length(v, Unit::Px));
        }
        if let Some(number) = source.strip_suffix("rem") {
            return number
                .parse()
                .ok()
                .map(|value| Self::Length(value, Unit::Rem));
        }
        if let Some(number) = source.strip_suffix("em") {
            return number
                .trim()
                .parse()
                .ok()
                .map(|v| Self::Length(v, Unit::Em));
        }
        // Absolute units fold straight to px (96 px per inch); ch/ex are
        // approximated as half an em, matching the heuristic measurer.
        // These suffixes collide with keywords ("flex" ends in ex), so a
        // failed numeric parse falls through instead of rejecting.
        for (suffix, factor) in [
            ("pt", 96.0 / 72.0),
            ("pc", 16.0),
            ("cm", 96.0 / 2.54),
            ("mm", 96.0 / 25.4),
            ("in", 96.0),
            ("q", 96.0 / 101.6),
        ] {
            if let Some(number) = source.strip_suffix(suffix)
                && let Ok(value) = number.trim().parse::<f32>()
            {
                return Some(Self::Length(value * factor, Unit::Px));
            }
        }
        for suffix in ["ch", "ex"] {
            if let Some(number) = source.strip_suffix(suffix)
                && let Ok(value) = number.trim().parse::<f32>()
            {
                return Some(Self::Length(value * 0.5, Unit::Em));
            }
        }
        if let Some(number) = source.strip_suffix("vmin")
            && let Ok(value) = number.trim().parse::<f32>()
        {
            return Some(Self::Length(value, Unit::Vmin));
        }
        if let Some(number) = source.strip_suffix("vmax")
            && let Ok(value) = number.trim().parse::<f32>()
        {
            return Some(Self::Length(value, Unit::Vmax));
        }
        if let Some(number) = source.strip_suffix("vw") {
            return number
                .trim()
                .parse()
                .ok()
                .map(|v| Self::Length(v, Unit::Vw));
        }
        if let Some(number) = source.strip_suffix("vh") {
            return number
                .trim()
                .parse()
                .ok()
                .map(|v| Self::Length(v, Unit::Vh));
        }
        if let Some(number) = source.strip_suffix('%') {
            return number
                .trim()
                .parse()
                .ok()
                .map(|v| Self::Length(v, Unit::Percent));
        }
        if let Ok(number) = source.parse::<f32>() {
            // Unitless zero is a valid length; other bare numbers stay numbers
            // (e.g. font-weight: 700, line-height: 1.5).
            if number == 0.0 {
                return Some(Self::Length(0.0, Unit::Px));
            }
            return Some(Self::Number(number));
        }
        if let Some(color) = Color::from_named(source) {
            // `transparent` intentionally stays a keyword.
            return Some(Self::Color(color));
        }
        if source
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
        {
            return Some(Self::Keyword(source.to_ascii_lowercase()));
        }
        None
    }

    /// The pixel value, if this is a `px` length (or unitless zero).
    #[must_use]
    pub fn as_px(&self) -> Option<f32> {
        match self {
            Self::Length(value, Unit::Px) => Some(*value),
            _ => None,
        }
    }

    /// The plain number this value carries. A bare `0` parses as a
    /// length (so `margin: 0` works), so the number-typed properties —
    /// `opacity`, `z-index`, `flex-grow`, `order` — have to accept a zero
    /// length back as the number it was written as.
    #[must_use]
    pub fn as_number(&self) -> Option<f32> {
        match self {
            Self::Number(value) => Some(*value),
            Self::Length(value, _) if *value == 0.0 => Some(0.0),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_color(&self) -> Option<Color> {
        match self {
            Self::Color(color) => Some(*color),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_keyword(&self) -> Option<&str> {
        match self {
            Self::Keyword(keyword) => Some(keyword),
            _ => None,
        }
    }
}

impl fmt::Display for CssValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(value) => write!(formatter, "\"{value}\""),
            Self::Url(value) => write!(formatter, "url({value})"),
            Self::Unresolved(value) => write!(formatter, "{value}"),
            Self::Function(name, arguments) => write!(formatter, "{name}({arguments})"),
            Self::Keyword(keyword) => write!(formatter, "{keyword}"),
            Self::Length(value, Unit::Px) => write!(formatter, "{value}px"),
            Self::Length(value, Unit::Em) => write!(formatter, "{value}em"),
            Self::Length(value, Unit::Rem) => write!(formatter, "{value}rem"),
            Self::Length(value, Unit::Vmin) => write!(formatter, "{value}vmin"),
            Self::Length(value, Unit::Vmax) => write!(formatter, "{value}vmax"),
            Self::Length(value, Unit::Vw) => write!(formatter, "{value}vw"),
            Self::Length(value, Unit::Vh) => write!(formatter, "{value}vh"),
            Self::Length(value, Unit::Percent) => write!(formatter, "{value}%"),
            Self::Color(color) => write!(formatter, "{color}"),
            Self::Number(value) => write!(formatter, "{value}"),
            Self::Auto => write!(formatter, "auto"),
        }
    }
}

/// Splits a declaration value into components, keeping `rgb(...)` together.
#[must_use]
pub fn split_components(source: &str) -> Vec<String> {
    let mut components = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    for character in source.chars() {
        if let Some(open) = quote {
            current.push(character);
            if character == open {
                quote = None;
            }
            continue;
        }
        match character {
            '"' | '\'' => {
                quote = Some(character);
                current.push(character);
            }
            '(' => {
                depth += 1;
                current.push(character);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(character);
            }
            _ if character.is_whitespace() && depth == 0 => {
                if !current.is_empty() {
                    components.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(character),
        }
    }
    if !current.is_empty() {
        components.push(current);
    }
    components
}

#[cfg(test)]
mod tests {

    /// Colors the CSS Color 4 spec pins to known sRGB values.
    #[test]
    fn modern_color_spaces_land_on_their_srgb_equivalents() {
        let parse = |source: &str| Color::parse(source).unwrap().to_string();
        // White and black survive every round trip.
        assert_eq!(parse("oklch(1 0 0)"), "#ffffff");
        assert_eq!(parse("oklab(0 0 0)"), "#000000");
        assert_eq!(parse("lab(100% 0 0)"), "#ffffff");
        assert_eq!(parse("lch(0% 0 0)"), "#000000");
        // hwb with no wash is the pure hue; equal wash is mid gray.
        assert_eq!(parse("hwb(0 0% 0%)"), "#ff0000");
        assert_eq!(parse("hwb(120deg 0% 0%)"), "#00ff00");
        assert_eq!(parse("hwb(0 50% 50%)"), "#808080");
        // A known sRGB primary through the Lab path, within rounding.
        let red = Color::parse("lab(54.29% 80.8 69.89)").unwrap();
        assert!(red.r > 250 && red.g < 6 && red.b < 6, "lab red: {red}");
        let blue = Color::parse("oklch(0.452 0.313 264.05)").unwrap();
        assert!(
            blue.b > 248 && blue.r < 8 && blue.g < 8,
            "oklch blue: {blue}"
        );
    }

    #[test]
    fn a_hue_reads_in_any_angle_unit() {
        let green = Color::parse("hwb(120deg 0% 0%)").unwrap();
        for same in [
            "hwb(120 0% 0%)",
            "hwb(0.3333turn 0% 0%)",
            "hwb(2.0944rad 0% 0%)",
        ] {
            assert_eq!(Color::parse(same).unwrap(), green, "{same}");
        }
    }

    #[test]
    fn color_mix_weighs_its_two_sides() {
        let parse = |source: &str| Color::parse(source).unwrap();
        // Half and half in sRGB is the arithmetic midpoint.
        assert_eq!(
            parse("color-mix(in srgb, #000000, #ffffff)").to_string(),
            "#808080"
        );
        // A stated share moves the midpoint, and the other side takes
        // what is left.
        assert_eq!(
            parse("color-mix(in srgb, #000000 25%, #ffffff)").to_string(),
            "#bfbfbf"
        );
        // Mixing in a perceptual space lands somewhere else entirely —
        // that is the point of naming one. Halfway up Oklab's lightness
        // is the gray that looks mid, which is darker than sRGB's
        // arithmetic middle.
        assert_eq!(
            parse("color-mix(in oklab, #000000, #ffffff)").to_string(),
            "#636363"
        );
        // Alpha mixes with the color.
        assert_eq!(parse("color-mix(in srgb, #ff000000, #ff0000)").a, 128);
    }

    #[test]
    fn a_modern_color_survives_as_a_declaration_value() {
        assert_eq!(
            CssValue::parse_component("oklch(1 0 0)"),
            Some(CssValue::Color(Color::rgb(0xff, 0xff, 0xff)))
        );
        assert_eq!(
            CssValue::parse_component("color-mix(in srgb, red, red)"),
            Some(CssValue::Color(Color::rgb(0xff, 0, 0)))
        );
    }
    use super::*;

    #[test]
    fn absolute_and_font_relative_units_fold() {
        assert_eq!(
            CssValue::parse_component("72pt"),
            Some(CssValue::Length(96.0, Unit::Px))
        );
        assert_eq!(
            CssValue::parse_component("1in"),
            Some(CssValue::Length(96.0, Unit::Px))
        );
        assert_eq!(
            CssValue::parse_component("2.54cm"),
            Some(CssValue::Length(96.0, Unit::Px))
        );
        assert_eq!(
            CssValue::parse_component("1pc"),
            Some(CssValue::Length(16.0, Unit::Px))
        );
        assert_eq!(
            CssValue::parse_component("2ch"),
            Some(CssValue::Length(1.0, Unit::Em))
        );
        assert_eq!(
            CssValue::parse_component("10vmin"),
            Some(CssValue::Length(10.0, Unit::Vmin))
        );
        assert_eq!(
            CssValue::parse_component("10vmax"),
            Some(CssValue::Length(10.0, Unit::Vmax))
        );
    }

    #[test]
    fn quoted_strings_survive_splitting_and_parse() {
        let components = split_components("\"hello world\" 4px");
        assert_eq!(components, vec!["\"hello world\"", "4px"]);
        assert_eq!(
            CssValue::parse_component("\"hello world\""),
            Some(CssValue::String("hello world".to_string()))
        );
        assert_eq!(
            CssValue::parse_component("'x'"),
            Some(CssValue::String("x".to_string()))
        );
    }

    #[test]
    fn parses_hex_colors() {
        assert_eq!(Color::parse("#fff"), Some(Color::rgb(255, 255, 255)));
        assert_eq!(Color::parse("#1f2937"), Some(Color::rgb(31, 41, 55)));
        assert_eq!(Color::parse("#12"), None);
        assert_eq!(Color::parse("#zzz"), None);
    }

    #[test]
    fn parses_rgb_function() {
        assert_eq!(
            Color::parse("rgb(255, 0, 10)"),
            Some(Color::rgb(255, 0, 10))
        );
        assert_eq!(Color::parse("rgb(300, 0, 0)"), None);
        assert_eq!(Color::parse("rgb(1, 2)"), None);
    }

    #[test]
    fn parses_rgb_percentage_channels_and_alpha() {
        assert_eq!(
            Color::parse("rgb(100%, 0%, 0%)"),
            Some(Color::rgb(255, 0, 0))
        );
        assert_eq!(
            Color::parse("rgb(50%, 50%, 50%)"),
            Some(Color::rgb(128, 128, 128))
        );
        assert_eq!(
            Color::parse("rgba(0, 0, 0, 50%)"),
            Some(Color::rgba(0, 0, 0, 128))
        );
        assert_eq!(Color::parse("rgb(150%, 0, 0)"), Some(Color::rgb(255, 0, 0)));
    }

    #[test]
    fn parses_named_colors() {
        assert_eq!(Color::parse("RED"), Some(Color::rgb(255, 0, 0)));
        assert_eq!(
            Color::parse("rebeccapurple"),
            Some(Color::rgb(102, 51, 153))
        );
        assert_eq!(Color::parse("dodgerblue"), Some(Color::rgb(30, 144, 255)));
        assert_eq!(Color::parse("mauve"), None);
    }

    #[test]
    fn parses_alpha_forms() {
        assert_eq!(
            Color::parse("#ff000080"),
            Some(Color::rgba(255, 0, 0, 0x80))
        );
        assert_eq!(Color::parse("#f008"), Some(Color::rgba(255, 0, 0, 0x88)));
        assert_eq!(
            Color::parse("rgba(10, 20, 30, 0.5)"),
            Some(Color::rgba(10, 20, 30, 128))
        );
        assert_eq!(
            Color::parse("rgba(10, 20, 30, 2.0)"),
            Some(Color::rgba(10, 20, 30, 255))
        );
    }

    #[test]
    fn parses_hsl() {
        assert_eq!(
            Color::parse("hsl(0, 100%, 50%)"),
            Some(Color::rgb(255, 0, 0))
        );
        assert_eq!(
            Color::parse("hsl(120, 100%, 25%)"),
            Some(Color::rgb(0, 128, 0))
        );
        assert_eq!(
            Color::parse("hsla(240, 100%, 50%, 0.5)"),
            Some(Color::rgba(0, 0, 255, 128))
        );
    }

    #[test]
    fn parses_modern_hsl_forms() {
        // Space-separated channels and slash alpha are valid CSS Color 4.
        assert_eq!(
            Color::parse("hsl(0 100% 50% / .5)"),
            Some(Color::rgba(255, 0, 0, 128))
        );
        assert_eq!(
            Color::parse("hsl(120 100% 25%)"),
            Some(Color::rgb(0, 128, 0))
        );
        assert_eq!(
            Color::parse("hsl(240 100% 50% / 50%)"),
            Some(Color::rgba(0, 0, 255, 128))
        );
        // An explicit deg unit on the hue is accepted.
        assert_eq!(
            Color::parse("hsl(120deg 100% 25%)"),
            Some(Color::rgb(0, 128, 0))
        );
    }

    #[test]
    fn url_keyword_is_case_insensitive() {
        assert_eq!(
            CssValue::parse_component("URL(bg.png)"),
            Some(CssValue::Url("bg.png".to_string()))
        );
        assert_eq!(
            CssValue::parse_component("Url('a b.png')"),
            Some(CssValue::Url("a b.png".to_string()))
        );
    }

    #[test]
    fn non_finite_numbers_are_rejected() {
        // f32 parsing accepts these spellings; they must not become values.
        assert_eq!(CssValue::parse_component("NaN"), None);
        assert_eq!(CssValue::parse_component("NaNpx"), None);
        assert_eq!(CssValue::parse_component("inf%"), None);
        assert_eq!(CssValue::parse_component("1e999px"), None);
        // Finite values are untouched.
        assert_eq!(
            CssValue::parse_component("1e3px"),
            Some(CssValue::Length(1000.0, Unit::Px))
        );
    }

    #[test]
    fn alpha_colors_display_as_rgba() {
        assert_eq!(
            Color::rgba(255, 0, 0, 128).to_string(),
            "rgba(255,0,0,0.502)"
        );
        assert_eq!(Color::rgb(255, 0, 0).to_string(), "#ff0000");
    }

    #[test]
    fn color_displays_as_rrggbb() {
        assert_eq!(Color::rgb(255, 0, 10).to_string(), "#ff000a");
    }

    #[test]
    fn parses_lengths_numbers_and_keywords() {
        assert_eq!(
            CssValue::parse_component("16px"),
            Some(CssValue::Length(16.0, Unit::Px))
        );
        assert_eq!(
            CssValue::parse_component("50%"),
            Some(CssValue::Length(50.0, Unit::Percent))
        );
        assert_eq!(
            CssValue::parse_component("1.5em"),
            Some(CssValue::Length(1.5, Unit::Em))
        );
        assert_eq!(
            CssValue::parse_component("0"),
            Some(CssValue::Length(0.0, Unit::Px))
        );
        assert_eq!(
            CssValue::parse_component("700"),
            Some(CssValue::Number(700.0))
        );
        assert_eq!(CssValue::parse_component("Auto"), Some(CssValue::Auto));
        assert_eq!(
            CssValue::parse_component("Block"),
            Some(CssValue::Keyword("block".to_string()))
        );
        assert_eq!(
            CssValue::parse_component("transparent"),
            Some(CssValue::Keyword("transparent".to_string()))
        );
        assert_eq!(
            CssValue::parse_component("url(x)"),
            Some(CssValue::Url("x".to_string()))
        );
        assert_eq!(
            CssValue::parse_component("linear-gradient(to right, #000, #fff)"),
            Some(CssValue::Function(
                "linear-gradient".to_string(),
                "to right, #000, #fff".to_string()
            ))
        );
    }

    #[test]
    fn splits_components_respecting_parentheses() {
        assert_eq!(
            split_components("1px  rgb(0, 0, 0)   solid"),
            vec!["1px", "rgb(0, 0, 0)", "solid"]
        );
    }
}
