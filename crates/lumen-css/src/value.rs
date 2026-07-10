//! Typed CSS values.

use std::fmt;

/// Length units understood by the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Px,
    /// Relative to the element's font size (parent's, for `font-size`).
    Em,
    /// Percent of the viewport width / height.
    Vw,
    Vh,
    Percent,
}

/// An opaque sRGB color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parses `#rgb`, `#rrggbb`, `rgb(r, g, b)` or a named color.
    #[must_use]
    pub fn parse(source: &str) -> Option<Self> {
        let source = source.trim();
        if let Some(hex) = source.strip_prefix('#') {
            return Self::parse_hex(hex);
        }
        if let Some(body) = source
            .strip_prefix("rgb(")
            .and_then(|rest| rest.strip_suffix(')'))
        {
            let mut channels = body.split(',').map(|part| part.trim().parse::<u8>().ok());
            let r = channels.next()??;
            let g = channels.next()??;
            let b = channels.next()??;
            if channels.next().is_some() {
                return None;
            }
            return Some(Self::rgb(r, g, b));
        }
        Self::from_named(source)
    }

    fn parse_hex(hex: &str) -> Option<Self> {
        let value = u32::from_str_radix(hex, 16).ok()?;
        match hex.len() {
            3 => {
                let r = ((value >> 8) & 0xf) as u8;
                let g = ((value >> 4) & 0xf) as u8;
                let b = (value & 0xf) as u8;
                Some(Self::rgb(r << 4 | r, g << 4 | g, b << 4 | b))
            }
            6 => Some(Self::rgb(
                ((value >> 16) & 0xff) as u8,
                ((value >> 8) & 0xff) as u8,
                (value & 0xff) as u8,
            )),
            _ => None,
        }
    }

    /// A small predefined set of named colors.
    #[must_use]
    pub fn from_named(name: &str) -> Option<Self> {
        let color = match name.to_ascii_lowercase().as_str() {
            "black" => Self::rgb(0, 0, 0),
            "white" => Self::rgb(255, 255, 255),
            "red" => Self::rgb(255, 0, 0),
            "green" => Self::rgb(0, 128, 0),
            "blue" => Self::rgb(0, 0, 255),
            "yellow" => Self::rgb(255, 255, 0),
            "orange" => Self::rgb(255, 165, 0),
            "purple" => Self::rgb(128, 0, 128),
            "gray" | "grey" => Self::rgb(128, 128, 128),
            "silver" => Self::rgb(192, 192, 192),
            _ => return None,
        };
        Some(color)
    }
}

impl fmt::Display for Color {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// A single parsed component of a declaration value.
#[derive(Debug, Clone, PartialEq)]
pub enum CssValue {
    /// An identifier the engine interprets later (`block`, `bold`, `transparent`, ...).
    Keyword(String),
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
        if source.starts_with('#') || source.starts_with("rgb(") {
            return Color::parse(source).map(Self::Color);
        }
        if let Some(number) = source.strip_suffix("px") {
            return number
                .trim()
                .parse()
                .ok()
                .map(|v| Self::Length(v, Unit::Px));
        }
        if let Some(number) = source.strip_suffix("em") {
            return number
                .trim()
                .parse()
                .ok()
                .map(|v| Self::Length(v, Unit::Em));
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
            Self::Keyword(keyword) => write!(formatter, "{keyword}"),
            Self::Length(value, Unit::Px) => write!(formatter, "{value}px"),
            Self::Length(value, Unit::Em) => write!(formatter, "{value}em"),
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
    for character in source.chars() {
        match character {
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
        assert_eq!(Color::parse("mauve"), None);
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
        assert_eq!(CssValue::parse_component("url(x)"), None);
    }

    #[test]
    fn splits_components_respecting_parentheses() {
        assert_eq!(
            split_components("1px  rgb(0, 0, 0)   solid"),
            vec!["1px", "rgb(0, 0, 0)", "solid"]
        );
    }
}
