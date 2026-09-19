//! Text measurement and line breaking.
//!
//! Measurement hides behind [`TextMeasurer`] so tests can use exact fake
//! metrics and a real font backend can slot in later. The default
//! [`HeuristicMeasurer`] is a deliberate approximation — half an em per
//! character — not real text shaping.

use crate::style::FontWeight;
use std::sync::Arc;

/// A computed `font-family` list: lowercased names in author order,
/// comma-separated, ending in a generic family — `"helvetica neue,arial,
/// sans-serif"`. Shared by clone, so passing it around a line of text
/// costs a refcount, and it doubles as the cache key for the face it
/// resolves to.
pub type FontFamilies = Arc<str>;

/// The generic families, as they appear at the end of a [`FontFamilies`]
/// list.
pub mod families {
    use super::FontFamilies;

    pub const SERIF: &str = "serif";
    pub const SANS_SERIF: &str = "sans-serif";
    pub const MONOSPACE: &str = "monospace";

    /// The initial `font-family`: an unstyled page is serif.
    #[must_use]
    pub fn initial() -> FontFamilies {
        SERIF.into()
    }

    /// The family list for UI text a shell draws itself.
    #[must_use]
    pub fn sans_serif() -> FontFamilies {
        SANS_SERIF.into()
    }

    /// The family list for monospaced UI text (debug overlays).
    #[must_use]
    pub fn monospace() -> FontFamilies {
        MONOSPACE.into()
    }
}

/// Everything that picks one face out of the installed fonts: the family
/// list plus the weight and slant that select within it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FaceKey {
    pub families: FontFamilies,
    pub weight: u16,
    pub italic: bool,
}

impl FaceKey {
    /// Whether the list ends in (or names) the monospace generic — the
    /// one distinction measurers without real fonts can still honor.
    #[must_use]
    pub fn is_monospace(&self) -> bool {
        self.families
            .split(',')
            .any(|family| family == families::MONOSPACE)
    }
}

/// The style inputs that affect text measurement.
#[derive(Debug, Clone, PartialEq)]
pub struct TextStyle {
    pub font_size: f32,
    pub font_weight: FontWeight,
    pub families: FontFamilies,
    /// Slanted face (`font-style: italic` / `oblique`).
    pub italic: bool,
    /// Extra advance per character, px.
    pub letter_spacing: f32,
}

impl TextStyle {
    /// The face this style asks for, without its size.
    #[must_use]
    pub fn face(&self) -> FaceKey {
        FaceKey {
            families: self.families.clone(),
            weight: self.font_weight.0,
            italic: self.italic,
        }
    }
}

/// Measured extent of a text run on a single line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextMetrics {
    pub width: f32,
}

/// Measures text without laying it out.
pub trait TextMeasurer {
    fn measure(&self, text: &str, style: &TextStyle) -> TextMetrics;

    /// The used value of `line-height: normal` for text in `style`, when
    /// the measurer knows the font's vertical metrics. `None` leaves the
    /// caller on its font-size approximation.
    fn normal_line_height(&self, _style: &TextStyle) -> Option<f32> {
        None
    }
}

/// Deterministic approximation: every character advances half an em.
/// Good enough for layout structure; replaced by real font metrics later.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeuristicMeasurer;

impl TextMeasurer for HeuristicMeasurer {
    fn measure(&self, text: &str, style: &TextStyle) -> TextMetrics {
        // Monospace glyphs run a little wider than the proportional average.
        let per_char = if style.face().is_monospace() {
            0.6
        } else {
            0.5
        };
        let count = text.chars().count() as f32;
        TextMetrics {
            width: count * (style.font_size * per_char + style.letter_spacing),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style(font_size: f32, families: &str) -> TextStyle {
        TextStyle {
            font_size,
            font_weight: FontWeight(400),
            families: families.into(),
            italic: false,
            letter_spacing: 0.0,
        }
    }

    #[test]
    fn heuristic_measurer_scales_with_font_size() {
        let narrow = HeuristicMeasurer.measure("abcd", &style(16.0, families::SANS_SERIF));
        assert_eq!(narrow.width, 32.0); // 4 chars * 16px * 0.5
        let big = HeuristicMeasurer.measure("abcd", &style(32.0, families::SANS_SERIF));
        assert_eq!(big.width, 64.0);
    }

    #[test]
    fn monospace_anywhere_in_the_list_widens_the_estimate() {
        let mono = style(16.0, "menlo,monospace");
        assert!(mono.face().is_monospace());
        assert_eq!(HeuristicMeasurer.measure("abcd", &mono).width, 38.4);
        assert!(!style(16.0, "monaco,sans-serif").face().is_monospace());
    }
}
