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

/// One laid-out line of text with its measured width.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub text: String,
    pub width: f32,
}

/// Greedy word wrap. `text` must already have collapsed whitespace.
/// A word wider than `max_width` gets its own overflowing line rather than
/// being split mid-word.
#[must_use]
pub fn break_into_lines(
    text: &str,
    style: &TextStyle,
    max_width: f32,
    measurer: &dyn TextMeasurer,
) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut current = String::new();

    for word in text.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{current} {word}")
        };
        if current.is_empty() || measurer.measure(&candidate, style).width <= max_width {
            current = candidate;
        } else {
            lines.push(finish_line(current, style, measurer));
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(finish_line(current, style, measurer));
    }
    lines
}

fn finish_line(text: String, style: &TextStyle, measurer: &dyn TextMeasurer) -> Line {
    let width = measurer.measure(&text, style).width;
    Line { text, width }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exact fake: one pixel per character.
    struct CharWidth;
    impl TextMeasurer for CharWidth {
        fn measure(&self, text: &str, _style: &TextStyle) -> TextMetrics {
            TextMetrics {
                width: text.chars().count() as f32,
            }
        }
    }

    const STYLE: TextStyle = TextStyle {
        font_size: 16.0,
        font_weight: FontWeight(400),
    };

    fn texts(lines: &[Line]) -> Vec<&str> {
        lines.iter().map(|line| line.text.as_str()).collect()
    }

    #[test]
    fn short_text_stays_on_one_line() {
        let lines = break_into_lines("hello world", &STYLE, 100.0, &CharWidth);
        assert_eq!(texts(&lines), vec!["hello world"]);
        assert_eq!(lines[0].width, 11.0);
    }

    #[test]
    fn wraps_at_max_width() {
        // "aa bb cc dd" with max 5: "aa bb" fits, "cc dd" fits.
        let lines = break_into_lines("aa bb cc dd", &STYLE, 5.0, &CharWidth);
        assert_eq!(texts(&lines), vec!["aa bb", "cc dd"]);
    }

    #[test]
    fn word_wider_than_line_overflows_alone() {
        let lines = break_into_lines("a extraordinarily b", &STYLE, 4.0, &CharWidth);
        assert_eq!(texts(&lines), vec!["a", "extraordinarily", "b"]);
        assert!(lines[1].width > 4.0);
    }

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
