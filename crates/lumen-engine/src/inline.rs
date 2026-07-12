//! Inline formatting: lays a run of inline-level content (text nodes,
//! inline elements and atomic inline-blocks) into shared line boxes.
//!
//! Deliberate simplifications, documented here rather than hidden:
//! non-atomic inline elements contribute no box edges (their margins,
//! paddings, borders and backgrounds are ignored); `vertical-align` is
//! fixed to a shared baseline approximated as the tallest font size, with
//! atomic boxes sitting bottom-on-baseline.

use crate::layout::LayoutBox;
use crate::style::{
    ComputedStyle, Display, StyleMap, TextAlign, TextTransform, VerticalAlign, WhiteSpace,
};
use crate::text::{TextMeasurer, TextStyle};
use lumen_html::{Document, NodeId, NodeKind};

/// What a fragment holds.
#[derive(Debug, Clone, PartialEq)]
pub enum FragmentContent {
    /// A run of same-styled text.
    Text {
        text: String,
        style: Box<ComputedStyle>,
    },
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
    /// Vertical offset from the default (baseline) position, from
    /// `vertical-align`.
    pub dy: f32,
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
            FragmentContent::Text { style, .. } => Some(style.as_ref()),
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
/// Applies `text-transform` to one word.
fn transform_word(word: &str, transform: TextTransform) -> String {
    match transform {
        TextTransform::None => word.to_string(),
        TextTransform::Uppercase => word.to_uppercase(),
        TextTransform::Lowercase => word.to_lowercase(),
        TextTransform::Capitalize => {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

fn collect_items(
    document: &Document,
    styles: &StyleMap,
    node_id: NodeId,
    pending_space: &mut bool,
    items: &mut Vec<InlineItem>,
) {
    match &document.node(node_id).kind {
        NodeKind::Text(text) => {
            let pre = styles
                .by_node
                .get(&node_id)
                .is_some_and(|style| style.white_space == WhiteSpace::Pre);
            if pre {
                // Preserved whitespace: each newline forces a line break and
                // spaces survive verbatim. The newlines hugging the element
                // tags are dropped (as HTML does for `<pre>`), tabs become
                // four spaces.
                let text = text.strip_prefix('\n').unwrap_or(text);
                let text = text.strip_suffix('\n').unwrap_or(text);
                for (index, segment) in text.split('\n').enumerate() {
                    if index > 0 {
                        items.push(InlineItem::HardBreak);
                    }
                    if !segment.is_empty() {
                        items.push(InlineItem::Word {
                            node_id,
                            text: segment.replace('\t', "    "),
                            space_before: false,
                        });
                    }
                }
                *pending_space = false;
                return;
            }
            let leading_space = text.chars().next().is_some_and(char::is_whitespace);
            let trailing_space = text.chars().last().is_some_and(char::is_whitespace);
            let mut first = true;
            let transform = styles
                .by_node
                .get(&node_id)
                .map_or(TextTransform::None, |style| style.text_transform);
            for word in text.split_whitespace() {
                items.push(InlineItem::Word {
                    node_id,
                    text: transform_word(word, transform),
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
            if display == Display::InlineBlock || element.tag_name == "img" {
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
                builder.flush_line(true, false);
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
    builder.flush_line(false, false);
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
            // `text-indent` shifts the first line of the run.
            let extra = if self.lines.is_empty() {
                self.container.text_indent
            } else {
                0.0
            };
            self.line_indent = indent + extra;
            self.line_width = (width - extra).max(0.0);
            self.started = true;
        }
    }

    fn place_word(&mut self, node_id: NodeId, word: &str, space_before: bool) {
        self.start_line_if_needed();
        let style = self.style_of(node_id).clone();
        let text_style = TextStyle {
            font_size: style.font_size,
            font_weight: style.font_weight,
            monospace: style.monospace,
            letter_spacing: style.letter_spacing,
        };
        let word_width = self.measurer.measure(word, &text_style).width;
        let space_width = if space_before && !self.current.is_empty() {
            self.measurer.measure(" ", &text_style).width + style.word_spacing
        } else {
            0.0
        };

        // Preserved-whitespace and nowrap text never wrap.
        let wraps = style.white_space == WhiteSpace::Normal;
        // word-break: an over-wide word splits at character boundaries,
        // filling each line before breaking to the next.
        if wraps && style.break_words && word_width > self.line_width {
            let mut rest: &str = word;
            let mut space = space_width;
            while !rest.is_empty() {
                if !self.current.is_empty() && self.pen_x + space >= self.line_width {
                    self.flush_line(false, true);
                    self.start_line_if_needed();
                    space = 0.0;
                }
                let available = (self.line_width - self.pen_x - space).max(0.0);
                let mut end = 0;
                let mut kept_width = 0.0;
                for (index, character) in rest.char_indices() {
                    let next_end = index + character.len_utf8();
                    let width = self.measurer.measure(&rest[..next_end], &text_style).width;
                    if width > available && end > 0 {
                        break;
                    }
                    // The first char always fits (guarantees progress).
                    end = next_end;
                    kept_width = width;
                    if width > available {
                        break;
                    }
                }
                if end == 0 {
                    break;
                }
                let chunk = &rest[..end];
                self.append_text(node_id, chunk, kept_width, space, style.clone());
                space = 0.0;
                rest = &rest[end..];
                if !rest.is_empty() {
                    self.flush_line(false, true);
                    self.start_line_if_needed();
                }
            }
            return;
        }
        if wraps
            && !self.current.is_empty()
            && self.pen_x + space_width + word_width > self.line_width
        {
            self.flush_line(false, true);
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
                monospace: self.container.monospace,
                letter_spacing: self.container.letter_spacing,
            };
            self.measurer.measure(" ", &text_style).width
        } else {
            0.0
        };
        let mut space_width = space_width;
        if !self.current.is_empty() && self.pen_x + space_width + width > self.line_width {
            self.flush_line(false, true);
            self.start_line_if_needed();
            space_width = 0.0;
        }
        self.current.push(Fragment {
            node_id,
            x: self.pen_x + space_width,
            width,
            dy: 0.0,
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
        // Justified text keeps per-word fragments so gaps can stretch.
        if self.container.text_align != TextAlign::Justify
            && let Some(last) = self.current.last_mut()
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
                dy: 0.0,
                content: FragmentContent::Text {
                    text: word.to_string(),
                    style: Box::new(style),
                },
            });
        }
        self.pen_x += space_width + word_width;
    }

    /// Ends the current line. `forced` lines (from `<br>`) are emitted even
    /// when empty, producing a blank line.
    /// Ends the current line. `forced` lines (from `<br>`) are emitted
    /// even when empty; `fill` marks a wrapped (non-final) line eligible
    /// for justification.
    fn flush_line(&mut self, forced: bool, fill: bool) {
        if self.current.is_empty() && !forced {
            return;
        }
        let mut fragments = std::mem::take(&mut self.current);

        // text-overflow: ellipsis — when the line overflows its box, drop
        // trailing content and end the last surviving text fragment in an
        // ellipsis that fits the available width.
        if self.container.text_overflow_ellipsis && self.pen_x > self.line_width {
            let ellipsis_style = TextStyle {
                font_size: self.container.font_size,
                font_weight: self.container.font_weight,
                monospace: self.container.monospace,
                letter_spacing: self.container.letter_spacing,
            };
            let ellipsis_width = self.measurer.measure("…", &ellipsis_style).width;
            let budget = (self.line_width - ellipsis_width).max(0.0);
            let mut kept: Vec<Fragment> = Vec::new();
            for fragment in fragments {
                if fragment.x + fragment.width <= budget {
                    kept.push(fragment);
                    continue;
                }
                // Boundary fragment: truncate its text to the budget.
                if let FragmentContent::Text { text, style } = &fragment.content {
                    let text_style = TextStyle {
                        font_size: style.font_size,
                        font_weight: style.font_weight,
                        monospace: style.monospace,
                        letter_spacing: style.letter_spacing,
                    };
                    let mut cut = String::new();
                    for character in text.chars() {
                        let mut candidate = cut.clone();
                        candidate.push(character);
                        if fragment.x + self.measurer.measure(&candidate, &text_style).width
                            > budget
                        {
                            break;
                        }
                        cut = candidate;
                    }
                    cut.push('…');
                    let width = self.measurer.measure(&cut, &text_style).width;
                    kept.push(Fragment {
                        node_id: fragment.node_id,
                        x: fragment.x,
                        width,
                        dy: 0.0,
                        content: FragmentContent::Text {
                            text: cut,
                            style: style.clone(),
                        },
                    });
                }
                break; // Everything after the boundary is dropped.
            }
            self.pen_x = kept
                .last()
                .map_or(0.0, |fragment| fragment.x + fragment.width);
            fragments = kept;
        }

        // Content extent around the baseline: text contributes an
        // 0.8/0.2 ascent/descent split of its font size (a practical
        // approximation of real font metrics), atomic boxes sit with
        // their full height above the baseline.
        let mut height = self.container.line_height;
        let mut ascent = self.container.font_size * 0.8;
        let mut descent = self.container.font_size * 0.2;
        for fragment in &fragments {
            match &fragment.content {
                FragmentContent::Text { style, .. } => {
                    height = height.max(style.line_height);
                    ascent = ascent.max(style.font_size * 0.8);
                    descent = descent.max(style.font_size * 0.2);
                }
                FragmentContent::Box(laid) => {
                    ascent = ascent.max(laid.margin_box().height);
                }
            }
        }
        let content = ascent + descent;
        height = height.max(content);
        // Half-leading: CSS splits the extra line-height evenly above and
        // below the content, so a tall line-height centers its text (and
        // baseline-aligned atomic boxes) vertically.
        let baseline = (height - content) / 2.0 + ascent;

        let leftover = (self.line_width - self.pen_x).max(0.0);
        // Justify: wrapped lines stretch, spreading the leftover across
        // the gaps between fragments; final/forced lines stay left.
        if self.container.text_align == TextAlign::Justify && fill && fragments.len() > 1 {
            let per_gap = leftover / (fragments.len() - 1) as f32;
            for (index, fragment) in fragments.iter_mut().enumerate() {
                fragment.x += per_gap * index as f32;
            }
            self.pen_x = self.line_width;
        }
        let shift = self.line_indent
            + match self.container.text_align {
                TextAlign::Left | TextAlign::Justify => 0.0,
                TextAlign::Center => leftover / 2.0,
                TextAlign::Right => leftover,
            };
        for fragment in &mut fragments {
            fragment.x += shift;
            match &mut fragment.content {
                // Atomic boxes get their final absolute position now:
                // baseline aligned by default, shifted by vertical-align.
                FragmentContent::Box(laid) => {
                    let margin_box = laid.margin_box();
                    let box_height = margin_box.height;
                    let default_top = baseline - box_height;
                    let align = laid.style.vertical_align;
                    let top = match align {
                        VerticalAlign::Top => 0.0,
                        VerticalAlign::Middle => (height - box_height) / 2.0,
                        VerticalAlign::Bottom => height - box_height,
                        _ => default_top,
                    };
                    let dx = self.origin.0 + fragment.x - margin_box.x;
                    let dy = self.origin.1 + self.cursor_y + top - margin_box.y;
                    laid.translate(dx, dy);
                }
                // Text fragments carry a baseline offset for painting.
                FragmentContent::Text { style, .. } => {
                    let ascent = style.font_size * 0.8;
                    fragment.dy = match style.vertical_align {
                        VerticalAlign::Baseline => 0.0,
                        VerticalAlign::Top => -(baseline - ascent),
                        VerticalAlign::Middle => (height - ascent) / 2.0 - (baseline - ascent),
                        VerticalAlign::Bottom => height - baseline,
                        VerticalAlign::Sub => 0.25 * style.font_size,
                        VerticalAlign::Super => -0.4 * style.font_size,
                    };
                }
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
