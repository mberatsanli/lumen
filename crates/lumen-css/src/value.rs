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
        Self::from_named(source)
    }

    fn parse_rgb_body(body: &str) -> Option<Self> {
        let parts: Vec<&str> = body.split(',').map(str::trim).collect();
        if parts.len() != 3 && parts.len() != 4 {
            return None;
        }
        let channel = |part: &str| part.parse::<u8>().ok();
        let color = Self::rgb(channel(parts[0])?, channel(parts[1])?, channel(parts[2])?);
        match parts.get(3) {
            None => Some(color),
            Some(alpha) => {
                let alpha: f32 = alpha.parse().ok()?;
                Some(Self {
                    a: (alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
                    ..color
                })
            }
        }
    }

    fn parse_hsl_body(body: &str) -> Option<Self> {
        let parts: Vec<&str> = body.split(',').map(str::trim).collect();
        if parts.len() != 3 && parts.len() != 4 {
            return None;
        }
        let hue: f32 = parts[0].parse().ok()?;
        let saturation: f32 = parts[1].strip_suffix('%')?.parse().ok()?;
        let lightness: f32 = parts[2].strip_suffix('%')?.parse().ok()?;
        let alpha = match parts.get(3) {
            None => 255u8,
            Some(alpha) => {
                let alpha: f32 = alpha.parse().ok()?;
                (alpha.clamp(0.0, 1.0) * 255.0).round() as u8
            }
        };
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
    /// Parses one whitespace-delimited value component.
    ///
    /// Returns `None` for components the engine cannot represent (which the
    /// caller then skips, per the "ignore unsupported declarations" rule).
    #[must_use]
    pub fn parse_component(source: &str) -> Option<Self> {
        let source = source.trim();
        if source.is_empty() {
            return None;
        }
        if source.eq_ignore_ascii_case("auto") {
            return Some(Self::Auto);
        }
        // url(...) and functional values (linear-gradient(...), ...).
        if let Some(inner) = source
            .strip_prefix("url(")
            .and_then(|rest| rest.strip_suffix(')'))
        {
            let inner = inner.trim();
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
            if !matches!(name.as_str(), "rgb" | "rgba" | "hsl" | "hsla") {
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
            || source.starts_with("rgb(")
            || source.starts_with("rgba(")
            || source.starts_with("hsl(")
            || source.starts_with("hsla(")
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
