//! Real font metrics and glyph rasterization behind [`TextMeasurer`].
//!
//! Wraps `fontdue` (a pure-Rust TTF/OTF rasterizer — font parsing is not
//! browser-engine core, like TLS it is deliberately a library). The engine
//! ships no font asset; callers load a font file (usually a system font via
//! [`SystemFont::load_default`]) and pass it in. Everything keeps working
//! without one: layout falls back to [`crate::HeuristicMeasurer`] and the
//! rasterizer to the built-in bitmap font.

use crate::text::{TextMeasurer, TextMetrics, TextStyle};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A loaded scalable font usable for both measurement and rasterization.
///
/// Rasterized glyphs are cached per (character, size), which makes
/// repeated frames (scrolling, resizing) cheap. The cache sits behind a
/// `Mutex`, so the font is `Sync` and can be shared via `Arc` — including
/// with background loader threads.
pub struct SystemFont {
    font: fontdue::Font,
    /// Monospace face for `font-family: monospace`; falls back to the
    /// regular face when no monospace font was found.
    mono: Option<fontdue::Font>,
    glyph_cache: Mutex<HashMap<(char, u32, bool), Arc<Glyph>>>,
}

/// A rasterized glyph: metrics plus an 8-bit coverage bitmap.
pub struct Glyph {
    pub metrics: fontdue::Metrics,
    pub coverage: Vec<u8>,
}

/// Common monospace font locations per platform, tried in order.
const MONO_CANDIDATE_PATHS: [&str; 8] = [
    // macOS
    "/System/Library/Fonts/Monaco.ttf",
    "/System/Library/Fonts/Supplemental/Courier New.ttf",
    "/System/Library/Fonts/Menlo.ttc",
    // Linux
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
    // Windows
    "C:\\Windows\\Fonts\\consola.ttf",
    "C:\\Windows\\Fonts\\cour.ttf",
];

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
            .map(|font| Self {
                font,
                mono: None,
                glyph_cache: Mutex::new(HashMap::new()),
            })
    }

    /// Tries the well-known system font paths for this platform, plus a
    /// monospace companion face when one exists.
    #[must_use]
    pub fn load_default() -> Option<Self> {
        let load = |paths: &[&str]| {
            paths
                .iter()
                .filter_map(|path| std::fs::read(path).ok())
                .find_map(|data| {
                    fontdue::Font::from_bytes(data.as_slice(), fontdue::FontSettings::default())
                        .ok()
                })
        };
        let font = load(&CANDIDATE_PATHS)?;
        Some(Self {
            font,
            mono: load(&MONO_CANDIDATE_PATHS),
            glyph_cache: Mutex::new(HashMap::new()),
        })
    }

    /// The face for a measurement/draw request.
    fn face(&self, monospace: bool) -> &fontdue::Font {
        if monospace {
            self.mono.as_ref().unwrap_or(&self.font)
        } else {
            &self.font
        }
    }

    /// Rasterizes one character at `font_size` (cached), returning metrics
    /// and an 8-bit coverage bitmap (row-major, `metrics.width` per row).
    #[must_use]
    pub fn rasterize(&self, character: char, font_size: f32, monospace: bool) -> Arc<Glyph> {
        let rasterize = || {
            let (metrics, coverage) = self.face(monospace).rasterize(character, font_size);
            Arc::new(Glyph { metrics, coverage })
        };
        match self.glyph_cache.lock() {
            Ok(mut cache) => cache
                .entry((character, font_size.to_bits(), monospace))
                .or_insert_with(rasterize)
                .clone(),
            // A poisoned cache just means uncached rasterization.
            Err(_) => rasterize(),
        }
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
        let face = self.face(style.monospace);
        let width = text
            .chars()
            .map(|character| face.metrics(character, style.font_size).advance_width)
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
            monospace: false,
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
