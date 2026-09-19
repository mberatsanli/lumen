//! Text selection: mapping pointer positions to character carets and back
//! to highlight rectangles and plain text.
//!
//! Selection works on a flattened list of [`TextRun`]s (the text fragments
//! of the layout tree in paint order, which follows document order). A
//! caret is a `(run, character)` pair; a [`Selection`] is an anchor/focus
//! caret pair in any order.

use crate::geometry::Rect;
use crate::inline::FragmentContent;
use crate::layout::{LayoutBox, LayoutKind};
use crate::style::FontWeight;
use crate::text::{TextMeasurer, TextStyle};
use lumen_css::Color;

/// The style fields selection actually uses (text metrics for caret
/// math, the `::selection` highlight override). Carrying these instead
/// of a full `ComputedStyle` avoids a deep clone per run.
#[derive(Debug, Clone, PartialEq)]
pub struct TextRunStyle {
    pub font_size: f32,
    pub font_weight: FontWeight,
    pub families: crate::text::FontFamilies,
    pub italic: bool,
    pub letter_spacing: f32,
    pub selection_background: Option<Color>,
}

/// One selectable text fragment with its absolute geometry.
#[derive(Debug, Clone)]
pub struct TextRun {
    pub text: String,
    /// The fragment's slot: line top/height, fragment x/width.
    pub rect: Rect,
    pub style: TextRunStyle,
}

/// Flattens all text fragments in paint order.
#[must_use]
pub fn collect_text_runs(layout: &LayoutBox) -> Vec<TextRun> {
    let mut runs = Vec::new();
    collect(layout, &mut runs, 0);
    runs
}

fn collect(layout: &LayoutBox, runs: &mut Vec<TextRun>, depth: usize) {
    // Depth guard: text below the cap is not selectable (and cannot
    // overflow the stack).
    if depth >= crate::MAX_DEPTH {
        return;
    }
    if let LayoutKind::Inline { lines } = &layout.kind {
        let content = layout.content_box();
        for line in lines {
            for fragment in &line.fragments {
                match &fragment.content {
                    // `user-select: none` text is invisible to selection.
                    FragmentContent::Text { style, .. } if !style.selectable => {}
                    FragmentContent::Text { text, style } => runs.push(TextRun {
                        text: text.clone(),
                        rect: Rect {
                            x: content.x + fragment.x,
                            y: content.y + line.y,
                            width: fragment.width,
                            height: line.height,
                        },
                        style: TextRunStyle {
                            font_size: style.font_size,
                            font_weight: style.font_weight,
                            families: style.font_family.clone(),
                            italic: style.italic,
                            letter_spacing: style.letter_spacing,
                            selection_background: style.selection_background,
                        },
                    }),
                    FragmentContent::Box(laid) => collect(laid, runs, depth + 1),
                }
            }
        }
    }
    for child in &layout.children {
        collect(child, runs, depth + 1);
    }
}

/// A position between characters of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Caret {
    pub run: usize,
    pub offset: usize,
}

/// An anchor/focus caret pair (in either order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Caret,
    pub focus: Caret,
}

impl Selection {
    /// The range in document order.
    #[must_use]
    pub fn ordered(&self) -> (Caret, Caret) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.anchor == self.focus
    }
}

fn text_style(run: &TextRun) -> TextStyle {
    TextStyle {
        font_size: run.style.font_size,
        font_weight: run.style.font_weight,
        families: run.style.families.clone(),
        italic: run.style.italic,
        letter_spacing: run.style.letter_spacing,
    }
}

/// Byte length of the run's first `offset` characters (offsets are char
/// indices; slicing at a char boundary avoids building a prefix String).
fn prefix_end(run: &TextRun, offset: usize) -> usize {
    run.text
        .char_indices()
        .nth(offset)
        .map_or(run.text.len(), |(byte, _)| byte)
}

fn prefix_width(run: &TextRun, offset: usize, measurer: &dyn TextMeasurer) -> f32 {
    measurer
        .measure(&run.text[..prefix_end(run, offset)], &text_style(run))
        .width
}

/// The caret nearest to a page-coordinate point. Points between lines
/// snap to the vertically nearest run; x clamps to run edges.
#[must_use]
pub fn caret_at_point(
    runs: &[TextRun],
    x: f32,
    y: f32,
    measurer: &dyn TextMeasurer,
) -> Option<Caret> {
    // Prefer runs whose line contains the y; otherwise the nearest line.
    let mut best: Option<(f32, usize)> = None;
    for (index, run) in runs.iter().enumerate() {
        let vertical = if y < run.rect.y {
            run.rect.y - y
        } else if y >= run.rect.y + run.rect.height {
            y - (run.rect.y + run.rect.height)
        } else {
            0.0
        };
        let horizontal = if x < run.rect.x {
            run.rect.x - x
        } else if x > run.rect.x + run.rect.width {
            x - (run.rect.x + run.rect.width)
        } else {
            0.0
        };
        // Lines dominate: a run on the right line always beats one off it.
        let score = vertical * 10_000.0 + horizontal;
        if best.is_none_or(|(best_score, _)| score < best_score) {
            best = Some((score, index));
        }
    }
    let (_, index) = best?;
    let run = &runs[index];

    let target = x - run.rect.x;
    // Walk the prefix widths once, carrying the previous measurement.
    // The strings measured are exactly those the old offset loop
    // measured, but each is a slice of the run — no per-character String.
    let style = text_style(run);
    let mut offset = 0;
    let mut before = measurer.measure("", &style).width;
    for (index, character) in run.text.char_indices() {
        let after = measurer
            .measure(&run.text[..index + character.len_utf8()], &style)
            .width;
        if target < (before + after) / 2.0 {
            break;
        }
        before = after;
        offset += 1;
    }
    Some(Caret { run: index, offset })
}

/// One highlighted region: a rectangle plus the run's `::selection`
/// background override, when it has one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HighlightRegion {
    pub rect: Rect,
    pub background: Option<Color>,
}

/// The runs a selection touches, with their indices (clipped to the
/// available runs).
fn selected_runs(
    runs: &[TextRun],
    start: Caret,
    end: Caret,
) -> impl Iterator<Item = (usize, &TextRun)> {
    let last = end.run.min(runs.len().saturating_sub(1));
    runs.iter().enumerate().take(last + 1).skip(start.run)
}

/// Highlight regions (page coordinates) for a selection.
#[must_use]
pub fn highlight_rects(
    runs: &[TextRun],
    selection: &Selection,
    measurer: &dyn TextMeasurer,
) -> Vec<HighlightRegion> {
    let (start, end) = selection.ordered();
    if selection.is_empty() {
        return Vec::new();
    }
    let mut regions = Vec::new();
    for (index, run) in selected_runs(runs, start, end) {
        let from = if index == start.run {
            prefix_width(run, start.offset, measurer)
        } else {
            0.0
        };
        let to = if index == end.run {
            prefix_width(run, end.offset, measurer)
        } else {
            run.rect.width
        };
        if to > from {
            regions.push(HighlightRegion {
                rect: Rect {
                    x: run.rect.x + from,
                    y: run.rect.y,
                    width: to - from,
                    height: run.rect.height,
                },
                background: run.style.selection_background,
            });
        }
    }
    regions
}

/// The selected text: runs on one line join with spaces, line changes
/// become newlines.
#[must_use]
pub fn selected_text(runs: &[TextRun], selection: &Selection) -> String {
    let (start, end) = selection.ordered();
    if selection.is_empty() {
        return String::new();
    }
    let mut output = String::new();
    let mut previous_line_y: Option<f32> = None;
    for (index, run) in selected_runs(runs, start, end) {
        let count = run.text.chars().count();
        let from = if index == start.run { start.offset } else { 0 };
        let to = if index == end.run {
            end.offset.min(count)
        } else {
            count
        };
        if from >= to {
            continue;
        }
        if let Some(previous) = previous_line_y {
            if (run.rect.y - previous).abs() > 0.5 {
                output.push('\n');
            } else {
                output.push(' ');
            }
        }
        output.extend(run.text.chars().skip(from).take(to - from));
        previous_line_y = Some(run.rect.y);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::ImageMap;
    use crate::style::compute_styles;
    use crate::text::HeuristicMeasurer;
    use crate::{Size, extract_embedded_css, layout_document};
    use lumen_html::parse_document;

    fn runs_for(html: &str) -> Vec<TextRun> {
        let document = parse_document(html);
        let author = lumen_css::parse_stylesheet(&format!(
            "{}{}",
            extract_embedded_css(&document),
            crate::test_support::BODY_RESET_CSS
        ));
        let styles = compute_styles(&document, &author);
        let layout = layout_document(
            &document,
            &styles,
            Size {
                width: 800.0,
                height: 600.0,
            },
            &HeuristicMeasurer,
            &ImageMap::new(),
        );
        collect_text_runs(&layout)
    }

    #[test]
    fn collects_runs_in_document_order() {
        let runs = runs_for("<div>one <strong>two</strong></div><p>three</p>");
        let texts: Vec<&str> = runs.iter().map(|run| run.text.as_str()).collect();
        assert_eq!(texts, vec!["one", "two", "three"]);
    }

    #[test]
    fn caret_maps_between_characters() {
        // Default 16px font → 8px per char with the heuristic measurer.
        let runs = runs_for("<div>abcd</div>");
        let caret = caret_at_point(&runs, 0.0, 5.0, &HeuristicMeasurer).unwrap();
        assert_eq!(caret.offset, 0);
        // 19px is closest to the boundary after the 2nd char (16px).
        let caret = caret_at_point(&runs, 19.0, 5.0, &HeuristicMeasurer).unwrap();
        assert_eq!(caret.offset, 2);
        // Far right clamps to the end.
        let caret = caret_at_point(&runs, 500.0, 5.0, &HeuristicMeasurer).unwrap();
        assert_eq!(caret.offset, 4);
    }

    #[test]
    fn highlight_covers_partial_and_full_runs() {
        let runs = runs_for("<div>abcd efgh</div>");
        // One fragment "abcd efgh": select chars 2..7 ("cd ef").
        let selection = Selection {
            anchor: Caret { run: 0, offset: 2 },
            focus: Caret { run: 0, offset: 7 },
        };
        let regions = highlight_rects(&runs, &selection, &HeuristicMeasurer);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].rect.x, 16.0);
        assert_eq!(regions[0].rect.width, 40.0);
        assert_eq!(regions[0].background, None);
    }

    #[test]
    fn unselectable_text_is_skipped_and_custom_color_carried() {
        let runs = runs_for(
            "<style>.locked { user-select: none; }
                    .gold::selection { background-color: #f5c518; }</style>
             <p class='locked'>secret</p><p class='gold'>shiny</p>",
        );
        let texts: Vec<&str> = runs.iter().map(|run| run.text.as_str()).collect();
        assert_eq!(texts, vec!["shiny"]);
        let selection = Selection {
            anchor: Caret { run: 0, offset: 0 },
            focus: Caret { run: 0, offset: 5 },
        };
        let regions = highlight_rects(&runs, &selection, &HeuristicMeasurer);
        assert_eq!(regions[0].background, Some(Color::rgb(0xf5, 0xc5, 0x18)));
    }

    #[test]
    fn selected_text_spans_runs_and_lines() {
        let runs = runs_for(
            "<style>div { width: 80px; }</style><div>aaaa <strong>bb</strong> cccc dddd</div>",
        );
        // Everything selected: multiple fragments over wrapped lines.
        let selection = Selection {
            anchor: Caret { run: 0, offset: 0 },
            focus: Caret {
                run: runs.len() - 1,
                offset: runs.last().unwrap().text.chars().count(),
            },
        };
        let text = selected_text(&runs, &selection);
        assert!(text.starts_with("aaaa bb"));
        assert!(
            text.contains('\n'),
            "wrapped lines become newlines: {text:?}"
        );
        assert!(text.ends_with("dddd"));
    }

    #[test]
    fn reversed_selection_normalizes() {
        let runs = runs_for("<div>hello</div>");
        let selection = Selection {
            anchor: Caret { run: 0, offset: 4 },
            focus: Caret { run: 0, offset: 1 },
        };
        assert_eq!(selected_text(&runs, &selection), "ell");
    }
}
