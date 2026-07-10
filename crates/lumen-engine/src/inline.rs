//! Inline formatting: lays a run of inline-level content (text nodes and
//! inline elements) into shared line boxes.
//!
//! Deliberate simplifications, documented here rather than hidden:
//! inline elements contribute no box edges (their margins, paddings,
//! borders and backgrounds are ignored); `vertical-align` is fixed to a
//! shared baseline approximated as the tallest font size on the line.

use crate::style::{ComputedStyle, Display, StyleMap, TextAlign};
use crate::text::{TextMeasurer, TextStyle};
use lumen_html::{Document, NodeId, NodeKind};

/// A run of same-styled text positioned inside a line box.
#[derive(Debug, Clone, PartialEq)]
pub struct Fragment {
    /// The DOM text node this run came from (link/hover hit testing walks
    /// its ancestors).
    pub node_id: NodeId,
    pub text: String,
    /// X offset relative to the containing content box.
    pub x: f32,
    pub width: f32,
    pub style: ComputedStyle,
}

/// One horizontal line of fragments.
#[derive(Debug, Clone, PartialEq)]
pub struct LineBox {
    /// Y offset of the line's top relative to the containing content box.
    pub y: f32,
    pub height: f32,
    /// Baseline offset from the line's top.
    pub baseline: f32,
    pub fragments: Vec<Fragment>,
}

enum InlineItem {
    Word {
        node_id: NodeId,
        text: String,
        space_before: bool,
    },
    HardBreak,
}

/// Collects the word/break stream of an inline run in document order.
fn collect_items(
    document: &Document,
    styles: &StyleMap,
    node_id: NodeId,
    pending_space: &mut bool,
    items: &mut Vec<InlineItem>,
) {
    match &document.node(node_id).kind {
        NodeKind::Text(text) => {
            let leading_space = text.chars().next().is_some_and(char::is_whitespace);
            let trailing_space = text.chars().last().is_some_and(char::is_whitespace);
            let mut first = true;
            for word in text.split_whitespace() {
                items.push(InlineItem::Word {
                    node_id,
                    text: word.to_string(),
                    space_before: if first {
                        *pending_space || leading_space
                    } else {
                        true
                    },
                });
                first = false;
            }
            if first {
                // Whitespace-only text acts as a separator.
                *pending_space = *pending_space || !text.is_empty();
            } else {
                *pending_space = trailing_space;
            }
        }
        NodeKind::Element(element) => {
            let display = styles
                .by_node
                .get(&node_id)
                .map_or(Display::Inline, |style| style.display);
            if display == Display::None {
                return;
            }
            if element.tag_name == "br" {
                items.push(InlineItem::HardBreak);
                *pending_space = false;
                return;
            }
            for child in document.children(node_id) {
                collect_items(document, styles, *child, pending_space, items);
            }
        }
        NodeKind::Document => {}
    }
}

/// Lays out `run` (sibling inline-level nodes) into line boxes of at most
/// `max_width`. Returns the lines and their total height.
pub(crate) fn layout_inline_run(
    document: &Document,
    styles: &StyleMap,
    run: &[NodeId],
    container: &ComputedStyle,
    max_width: f32,
    measurer: &dyn TextMeasurer,
) -> (Vec<LineBox>, f32) {
    let mut items = Vec::new();
    let mut pending_space = false;
    for node in run {
        collect_items(document, styles, *node, &mut pending_space, &mut items);
    }

    let mut builder = LineBuilder {
        styles,
        container,
        max_width,
        measurer,
        lines: Vec::new(),
        current: Vec::new(),
        pen_x: 0.0,
        cursor_y: 0.0,
    };

    for item in items {
        match item {
            InlineItem::HardBreak => builder.flush_line(true),
            InlineItem::Word {
                node_id,
                text,
                space_before,
            } => builder.place_word(node_id, &text, space_before),
        }
    }
    builder.flush_line(false);
    let total = builder.cursor_y;
    (builder.lines, total)
}

struct LineBuilder<'a> {
    styles: &'a StyleMap,
    container: &'a ComputedStyle,
    max_width: f32,
    measurer: &'a dyn TextMeasurer,
    lines: Vec<LineBox>,
    current: Vec<Fragment>,
    pen_x: f32,
    cursor_y: f32,
}

impl LineBuilder<'_> {
    fn style_of(&self, node_id: NodeId) -> &ComputedStyle {
        self.styles.by_node.get(&node_id).unwrap_or(self.container)
    }

    fn place_word(&mut self, node_id: NodeId, word: &str, space_before: bool) {
        let style = self.style_of(node_id).clone();
        let text_style = TextStyle {
            font_size: style.font_size,
            font_weight: style.font_weight,
        };
        let word_width = self.measurer.measure(word, &text_style).width;
        let space_width = if space_before && !self.current.is_empty() {
            self.measurer.measure(" ", &text_style).width
        } else {
            0.0
        };

        if !self.current.is_empty() && self.pen_x + space_width + word_width > self.max_width {
            self.flush_line(false);
            self.append(node_id, word, word_width, 0.0, style);
        } else {
            self.append(node_id, word, word_width, space_width, style);
        }
    }

    fn append(
        &mut self,
        node_id: NodeId,
        word: &str,
        word_width: f32,
        space_width: f32,
        style: ComputedStyle,
    ) {
        if let Some(last) = self.current.last_mut()
            && last.node_id == node_id
        {
            if space_width > 0.0 {
                last.text.push(' ');
            }
            last.text.push_str(word);
            last.width += space_width + word_width;
        } else {
            self.current.push(Fragment {
                node_id,
                text: word.to_string(),
                x: self.pen_x + space_width,
                width: word_width,
                style,
            });
        }
        self.pen_x += space_width + word_width;
    }

    /// Ends the current line. `forced` lines (from `<br>`) are emitted even
    /// when empty, producing a blank line.
    fn flush_line(&mut self, forced: bool) {
        if self.current.is_empty() && !forced {
            return;
        }
        let fragments = std::mem::take(&mut self.current);
        let height = fragments
            .iter()
            .map(|fragment| fragment.style.line_height)
            .fold(self.container.line_height, f32::max);
        let baseline = fragments
            .iter()
            .map(|fragment| fragment.style.font_size)
            .fold(self.container.font_size, f32::max);

        let mut fragments = fragments;
        let leftover = (self.max_width - self.pen_x).max(0.0);
        let shift = match self.container.text_align {
            TextAlign::Left => 0.0,
            TextAlign::Center => leftover / 2.0,
            TextAlign::Right => leftover,
        };
        if shift > 0.0 {
            for fragment in &mut fragments {
                fragment.x += shift;
            }
        }

        self.lines.push(LineBox {
            y: self.cursor_y,
            height,
            baseline,
            fragments,
        });
        self.cursor_y += height;
        self.pen_x = 0.0;
    }
}
