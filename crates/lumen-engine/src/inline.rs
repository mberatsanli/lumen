//! Inline formatting: lays a run of inline-level content (text nodes,
//! inline elements and atomic inline-blocks) into shared line boxes.
//!
//! Deliberate simplifications, documented here rather than hidden:
//! non-atomic inline elements contribute no box edges (their margins,
//! paddings, borders and backgrounds are ignored); `vertical-align` is
//! fixed to a shared baseline approximated as the tallest font size, with
//! atomic boxes sitting bottom-on-baseline.

use crate::layout::LayoutBox;
use crate::style::{ComputedStyle, Display, StyleMap, TextAlign};
use crate::text::{TextMeasurer, TextStyle};
use lumen_html::{Document, NodeId, NodeKind};

/// What a fragment holds.
#[derive(Debug, Clone, PartialEq)]
pub enum FragmentContent {
    /// A run of same-styled text.
    Text { text: String, style: ComputedStyle },
    /// An atomic inline (inline-block or similar): a fully laid-out box.
    Box(Box<LayoutBox>),
}

/// One positioned item inside a line box.
#[derive(Debug, Clone, PartialEq)]
pub struct Fragment {
    /// For text: the DOM text node (link/hover hit testing walks its
    /// ancestors). For boxes: the element node.
    pub node_id: NodeId,
    /// X offset relative to the containing content box.
    pub x: f32,
    pub width: f32,
    pub content: FragmentContent,
}

impl Fragment {
    /// Text content, when this is a text fragment (test/dump convenience).
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match &self.content {
            FragmentContent::Text { text, .. } => Some(text),
            FragmentContent::Box(_) => None,
        }
    }

    /// The text style, when this is a text fragment.
    #[must_use]
    pub fn style(&self) -> Option<&ComputedStyle> {
        match &self.content {
            FragmentContent::Text { style, .. } => Some(style),
            FragmentContent::Box(_) => None,
        }
    }
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

/// Per-line horizontal bounds inside the containing content box, queried
/// with the line's top offset — this is how floats narrow lines.
pub(crate) type LineBounds<'a> = dyn Fn(f32) -> (f32, f32) + 'a;

/// Lays out an atomic inline (inline-block) box for the line builder.
/// Returns the box positioned at a run-relative origin; the builder
/// translates it to its final spot when the line is flushed.
pub(crate) type AtomicLayout<'a> = dyn FnMut(NodeId, f32) -> LayoutBox + 'a;

enum InlineItem {
    Word {
        node_id: NodeId,
        text: String,
        space_before: bool,
    },
    Atomic {
        node_id: NodeId,
        space_before: bool,
    },
    HardBreak,
}

/// Collects the word/atomic/break stream of an inline run in document order.
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
            if display == Display::InlineBlock {
                items.push(InlineItem::Atomic {
                    node_id,
                    space_before: *pending_space,
                });
                *pending_space = false;
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

/// Lays out `run` (sibling inline-level nodes) into line boxes.
///
/// `origin` is the absolute position of the run's content box (atomic
/// boxes are translated to absolute coordinates at flush time), and
/// `bounds(line_top)` yields each line's `(indent, width)` inside the
/// containing content box. Returns the lines and their total height.
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_inline_run(
    document: &Document,
    styles: &StyleMap,
    run: &[NodeId],
    container: &ComputedStyle,
    origin: (f32, f32),
    bounds: &LineBounds<'_>,
    measurer: &dyn TextMeasurer,
    layout_atomic: &mut AtomicLayout<'_>,
) -> (Vec<LineBox>, f32) {
    let mut items = Vec::new();
    let mut pending_space = false;
    for node in run {
        collect_items(document, styles, *node, &mut pending_space, &mut items);
    }

    let mut builder = LineBuilder {
        styles,
        container,
        origin,
        bounds,
        measurer,
        lines: Vec::new(),
        current: Vec::new(),
        line_indent: 0.0,
        line_width: 0.0,
        pen_x: 0.0,
        cursor_y: 0.0,
        started: false,
    };

    for item in items {
        match item {
            InlineItem::HardBreak => {
                builder.start_line_if_needed();
                builder.flush_line(true);
            }
            InlineItem::Word {
                node_id,
                text,
                space_before,
            } => builder.place_word(node_id, &text, space_before),
            InlineItem::Atomic {
                node_id,
                space_before,
            } => builder.place_atomic(node_id, space_before, layout_atomic),
        }
    }
    builder.flush_line(false);
    let total = builder.cursor_y;
    (builder.lines, total)
}

struct LineBuilder<'a> {
    styles: &'a StyleMap,
    container: &'a ComputedStyle,
    origin: (f32, f32),
    bounds: &'a LineBounds<'a>,
    measurer: &'a dyn TextMeasurer,
    lines: Vec<LineBox>,
    current: Vec<Fragment>,
    /// Current line's indent and usable width (set at line start).
    line_indent: f32,
    line_width: f32,
    pen_x: f32,
    cursor_y: f32,
    started: bool,
}

impl LineBuilder<'_> {
    fn style_of(&self, node_id: NodeId) -> &ComputedStyle {
        self.styles.by_node.get(&node_id).unwrap_or(self.container)
    }

    fn start_line_if_needed(&mut self) {
        if !self.started {
            let (indent, width) = (self.bounds)(self.cursor_y);
            self.line_indent = indent;
            self.line_width = width;
            self.started = true;
        }
    }

    fn place_word(&mut self, node_id: NodeId, word: &str, space_before: bool) {
        self.start_line_if_needed();
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

        if !self.current.is_empty() && self.pen_x + space_width + word_width > self.line_width {
            self.flush_line(false);
            self.start_line_if_needed();
            self.append_text(node_id, word, word_width, 0.0, style);
        } else {
            self.append_text(node_id, word, word_width, space_width, style);
        }
    }

    fn place_atomic(
        &mut self,
        node_id: NodeId,
        space_before: bool,
        layout_atomic: &mut AtomicLayout<'_>,
    ) {
        self.start_line_if_needed();
        let laid = layout_atomic(node_id, self.line_width);
        let width = laid.margin_box().width;
        let space_width = if space_before && !self.current.is_empty() {
            let text_style = TextStyle {
                font_size: self.container.font_size,
                font_weight: self.container.font_weight,
            };
            self.measurer.measure(" ", &text_style).width
        } else {
            0.0
        };
        let mut space_width = space_width;
        if !self.current.is_empty() && self.pen_x + space_width + width > self.line_width {
            self.flush_line(false);
            self.start_line_if_needed();
            space_width = 0.0;
        }
        self.current.push(Fragment {
            node_id,
            x: self.pen_x + space_width,
            width,
            content: FragmentContent::Box(Box::new(laid)),
        });
        self.pen_x += space_width + width;
    }

    fn append_text(
        &mut self,
        node_id: NodeId,
        word: &str,
        word_width: f32,
        space_width: f32,
        style: ComputedStyle,
    ) {
        if let Some(last) = self.current.last_mut()
            && last.node_id == node_id
            && matches!(last.content, FragmentContent::Text { .. })
        {
            if let FragmentContent::Text { text, .. } = &mut last.content {
                if space_width > 0.0 {
                    text.push(' ');
                }
                text.push_str(word);
            }
            last.width += space_width + word_width;
        } else {
            self.current.push(Fragment {
                node_id,
                x: self.pen_x + space_width,
                width: word_width,
                content: FragmentContent::Text {
                    text: word.to_string(),
                    style,
                },
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
        let mut fragments = std::mem::take(&mut self.current);

        let mut height = self.container.line_height;
        let mut baseline = self.container.font_size;
        for fragment in &fragments {
            match &fragment.content {
                FragmentContent::Text { style, .. } => {
                    height = height.max(style.line_height);
                    baseline = baseline.max(style.font_size);
                }
                FragmentContent::Box(laid) => {
                    let box_height = laid.margin_box().height;
                    baseline = baseline.max(box_height);
                    height = height.max(box_height);
                }
            }
        }
        height = height.max(baseline);

        let leftover = (self.line_width - self.pen_x).max(0.0);
        let shift = self.line_indent
            + match self.container.text_align {
                TextAlign::Left => 0.0,
                TextAlign::Center => leftover / 2.0,
                TextAlign::Right => leftover,
            };
        for fragment in &mut fragments {
            fragment.x += shift;
            // Atomic boxes get their final absolute position now: baseline
            // aligned, margin box flush with the fragment slot.
            if let FragmentContent::Box(laid) = &mut fragment.content {
                let margin_box = laid.margin_box();
                let dx = self.origin.0 + fragment.x - margin_box.x;
                let dy =
                    self.origin.1 + self.cursor_y + baseline - margin_box.height - margin_box.y;
                laid.translate(dx, dy);
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
        self.started = false;
    }
}
