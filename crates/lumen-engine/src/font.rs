//! Real font metrics and glyph rasterization behind [`TextMeasurer`].
//!
//! Wraps `fontdue` (a pure-Rust TTF/OTF rasterizer — font parsing is not
//! browser-engine core, like TLS it is deliberately a library). The engine
//! ships no font asset; callers load a font file (usually a system font via
//! [`SystemFont::load_default`]) and pass it in. Everything keeps working
//! without one: layout falls back to [`crate::HeuristicMeasurer`] and the
//! rasterizer to the built-in bitmap font.

use crate::text::{TextMeasurer, TextMetrics, TextStyle};

/// A loaded scalable font usable for both measurement and rasterization.
pub struct SystemFont {
    font: fontdue::Font,
}

/// Common system font locations per platform, tried in order.
const CANDIDATE_PATHS: [&str; 8] = [
    // macOS
    "/System/Library/Fonts/Supplemental/Arial.ttf",
    "/System/Library/Fonts/Supplemental/Verdana.ttf",
    "/System/Library/Fonts/Supplemental/Tahoma.ttf",
    // Linux
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    // Windows
    "C:\\Windows\\Fonts\\arial.ttf",
    "C:\\Windows\\Fonts\\segoeui.ttf",
];

impl SystemFont {
    /// Parses TTF/OTF bytes. Returns `None` when the data is not a font.
    #[must_use]
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        fontdue::Font::from_bytes(data, fontdue::FontSettings::default())
            .ok()
            .map(|font| Self { font })
    }

    /// Tries the well-known system font paths for this platform.
    #[must_use]
    pub fn load_default() -> Option<Self> {
        CANDIDATE_PATHS
            .iter()
            .filter_map(|path| std::fs::read(path).ok())
            .find_map(|data| Self::from_bytes(&data))
    }

    /// Rasterizes one character at `font_size`, returning its metrics and
    /// an 8-bit coverage bitmap (row-major, `metrics.width` per row).
    #[must_use]
    pub fn rasterize(&self, character: char, font_size: f32) -> (fontdue::Metrics, Vec<u8>) {
        self.font.rasterize(character, font_size)
    }

    /// The ascent (baseline distance from the top of the line) at
    /// `font_size`, approximated as `0.8 × font_size` if unavailable.
    #[must_use]
    pub fn ascent(&self, font_size: f32) -> f32 {
        self.font
            .horizontal_line_metrics(font_size)
            .map_or(font_size * 0.8, |metrics| metrics.ascent)
    }
}

impl TextMeasurer for SystemFont {
    fn measure(&self, text: &str, style: &TextStyle) -> TextMetrics {
        let width = text
            .chars()
            .map(|character| self.font.metrics(character, style.font_size).advance_width)
            .sum();
        TextMetrics { width }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::FontWeight;

    #[test]
    fn garbage_bytes_are_not_a_font() {
        assert!(SystemFont::from_bytes(b"definitely not a font").is_none());
    }

    /// Runs only where a system font exists (macOS/Linux/Windows dev boxes
    /// and most CI images); silently passes elsewhere.
    #[test]
    fn proportional_metrics_when_a_system_font_is_available() {
        let Some(font) = SystemFont::load_default() else {
            return;
        };
        let style = TextStyle {
            font_size: 16.0,
            font_weight: FontWeight(400),
        };
        let narrow = font.measure("iiii", &style).width;
        let wide = font.measure("MMMM", &style).width;
        assert!(
            narrow < wide,
            "expected proportional widths: {narrow} vs {wide}"
        );
        assert!(font.ascent(16.0) > 8.0);
    }
}
