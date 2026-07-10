//! Text measurement and line breaking.
//!
//! Measurement hides behind [`TextMeasurer`] so tests can use exact fake
//! metrics and a real font backend can slot in later. The default
//! [`HeuristicMeasurer`] is a deliberate approximation — half an em per
//! character — not real text shaping.

use crate::style::FontWeight;

/// The style inputs that affect text measurement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextStyle {
    pub font_size: f32,
    pub font_weight: FontWeight,
}

/// Measured extent of a text run on a single line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextMetrics {
    pub width: f32,
}

/// Measures text without laying it out.
pub trait TextMeasurer {
    fn measure(&self, text: &str, style: &TextStyle) -> TextMetrics;
}

/// Deterministic approximation: every character advances half an em.
/// Good enough for layout structure; replaced by real font metrics later.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeuristicMeasurer;

impl TextMeasurer for HeuristicMeasurer {
    fn measure(&self, text: &str, style: &TextStyle) -> TextMetrics {
        TextMetrics {
            width: text.chars().count() as f32 * style.font_size * 0.5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STYLE: TextStyle = TextStyle {
        font_size: 16.0,
        font_weight: FontWeight(400),
    };

    #[test]
    fn heuristic_measurer_scales_with_font_size() {
        let narrow = HeuristicMeasurer.measure("abcd", &STYLE);
        assert_eq!(narrow.width, 32.0); // 4 chars * 16px * 0.5
        let big = HeuristicMeasurer.measure(
            "abcd",
            &TextStyle {
                font_size: 32.0,
                font_weight: FontWeight(400),
            },
        );
        assert_eq!(big.width, 64.0);
    }
}
