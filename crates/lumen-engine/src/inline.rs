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
use std::borrow::Cow;

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
    /// How far the fragment's own content area reaches above and below
    /// the line's baseline: the font's ascent and descent for text, the
    /// margin box for an atomic inline. A line is often taller than
    /// this, so anything painted behind the fragment — a background, a
    /// highlight — covers the content area, not the whole line.
    pub extent: (f32, f32),
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

enum InlineItem<'a> {
    Word {
        node_id: NodeId,
        /// Borrows the DOM text unless a transform/tab expansion forced
        /// an owned copy — most words never allocate.
        text: Cow<'a, str>,
        space_before: bool,
        /// The background of the nearest inline ancestor that has one.
        /// Text carries no background of its own, so this is how a
        /// `<mark>` or a styled `<span>` reaches the fragment it paints.
        background: Option<lumen_css::Color>,
    },
    Atomic {
        node_id: NodeId,
        space_before: bool,
    },
    HardBreak,
}

/// Collects the word/atomic/break stream of an inline run in document order.
/// Applies `text-transform` to one word.
fn transform_word(word: &str, transform: TextTransform) -> Cow<'_, str> {
    match transform {
        TextTransform::None => Cow::Borrowed(word),
        TextTransform::Uppercase => Cow::Owned(word.to_uppercase()),
        TextTransform::Lowercase => Cow::Owned(word.to_lowercase()),
        TextTransform::Capitalize => {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    Cow::Owned(first.to_uppercase().collect::<String>() + chars.as_str())
                }
                None => Cow::Borrowed(word),
            }
        }
    }
}

fn collect_items<'a>(
    document: &'a Document,
    styles: &StyleMap,
    node_id: NodeId,
    pending_space: &mut bool,
    items: &mut Vec<InlineItem<'a>>,
    background: Option<lumen_css::Color>,
) {
    match &document.node(node_id).kind {
        NodeKind::Text(text) => {
            let style = styles.by_node.get(&node_id);
            let pre = style.is_some_and(|style| style.white_space == WhiteSpace::Pre);
            if pre {
                // Preserved whitespace: each newline forces a line break and
                // spaces survive verbatim. The newlines hugging the element
                // tags are dropped (as HTML does for `<pre>`), tabs become
                // `tab-size` spaces (naive: a fixed count, not tab stops).
                let tab = " ".repeat(style.map_or(4, |style| style.tab_size) as usize);
                let text = text.strip_prefix('\n').unwrap_or(text);
                let text = text.strip_suffix('\n').unwrap_or(text);
                for (index, segment) in text.split('\n').enumerate() {
                    if index > 0 {
                        items.push(InlineItem::HardBreak);
                    }
                    if !segment.is_empty() {
                        let text = if segment.contains('\t') {
                            Cow::Owned(segment.replace('\t', &tab))
                        } else {
                            Cow::Borrowed(segment)
                        };
                        items.push(InlineItem::Word {
                            node_id,
                            text,
                            space_before: false,
                            background,
                        });
                    }
                }
                *pending_space = false;
                return;
            }
            let leading_space = text.chars().next().is_some_and(char::is_whitespace);
            let trailing_space = text.chars().last().is_some_and(char::is_whitespace);
            let mut first = true;
            let transform = style.map_or(TextTransform::None, |style| style.text_transform);
            for word in text.split_whitespace() {
                items.push(InlineItem::Word {
                    node_id,
                    text: transform_word(word, transform),
                    space_before: if first {
                        *pending_space || leading_space
                    } else {
                        true
                    },
                    background,
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
            // An inline box paints its background behind the text
            // inside it; a nested one covers the same run, so the
            // innermost background is the one that shows.
            let inside = styles
                .by_node
                .get(&node_id)
                .and_then(|style| style.background_color)
                .or(background);
            for child in document.children(node_id) {
                collect_items(document, styles, *child, pending_space, items, inside);
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
/// containing content box. `owner` is the run's block container, whose
/// `::first-line`/`::first-letter` styles apply to the first formatted
/// line. Returns the lines and their total height.
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_inline_run(
    document: &Document,
    styles: &StyleMap,
    run: &[NodeId],
    container: &ComputedStyle,
    owner: Option<NodeId>,
    origin: (f32, f32),
    bounds: &LineBounds<'_>,
    measurer: &dyn TextMeasurer,
    layout_atomic: &mut AtomicLayout<'_>,
) -> (Vec<LineBox>, f32) {
    let mut items = Vec::new();
    let mut pending_space = false;
    for node in run {
        collect_items(
            document,
            styles,
            *node,
            &mut pending_space,
            &mut items,
            None,
        );
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
        space_widths: std::collections::HashMap::new(),
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
                background,
            } => builder.place_word(node_id, &text, space_before, background),
            InlineItem::Atomic {
                node_id,
                space_before,
            } => builder.place_atomic(node_id, space_before, layout_atomic),
        }
    }
    builder.flush_line(false, false);
    let total = builder.cursor_y;
    let mut lines = builder.lines;
    if let Some(owner) = owner {
        apply_pseudo_line_styles(&mut lines, styles, owner, measurer);
    }
    (lines, total)
}

/// `::first-line` / `::first-letter` post-processing: the first-line
/// pseudo style overrides the text fields of every fragment on the
/// first formatted line, and the first letter of the first text
/// fragment splits into its own fragment carrying the first-letter
/// style on top. Cascade detail is simplified: pseudo declarations
/// simply override (their undeclared values equal the inherited ones).
fn apply_pseudo_line_styles(
    lines: &mut [LineBox],
    styles: &StyleMap,
    owner: NodeId,
    measurer: &dyn TextMeasurer,
) {
    let first_line = styles.first_line.get(&owner);
    let first_letter = styles.first_letter.get(&owner);
    if first_line.is_none() && first_letter.is_none() {
        return;
    }
    let Some(line) = lines.first_mut() else {
        return;
    };
    if let Some(pseudo) = first_line {
        for fragment in &mut line.fragments {
            if let FragmentContent::Text { style, .. } = &mut fragment.content {
                let style = style.as_mut();
                style.color = pseudo.color;
                style.background_color = pseudo.background_color;
                style.font_size = pseudo.font_size;
                style.font_weight = pseudo.font_weight;
                style.italic = pseudo.italic;
            }
        }
    }
    let Some(pseudo) = first_letter else {
        return;
    };
    for index in 0..line.fragments.len() {
        let FragmentContent::Text { text, style } = &line.fragments[index].content else {
            continue; // Atomic boxes are not letters; keep looking.
        };
        let Some(first) = text.chars().next() else {
            continue;
        };
        let first_end = first.len_utf8();
        let mut letter_style = style.as_ref().clone();
        letter_style.color = pseudo.color;
        letter_style.font_size = pseudo.font_size;
        letter_style.font_weight = pseudo.font_weight;
        letter_style.italic = pseudo.italic;
        let text_style = |style: &ComputedStyle| TextStyle {
            font_size: style.font_size,
            font_weight: style.font_weight,
            families: style.font_family.clone(),
            italic: style.italic,
            letter_spacing: style.letter_spacing,
        };
        let letter_text = text[..first_end].to_string();
        let letter_width = measurer
            .measure(&letter_text, &text_style(&letter_style))
            .width;
        let fragment = &line.fragments[index];
        let letter_extent = measurer
            .content_extent(&text_style(&letter_style))
            .unwrap_or(fragment.extent);
        let letter = Fragment {
            node_id: fragment.node_id,
            x: fragment.x,
            width: letter_width,
            dy: fragment.dy,
            extent: letter_extent,
            content: FragmentContent::Text {
                text: letter_text,
                style: Box::new(letter_style),
            },
        };
        let rest_text = text[first_end..].to_string();
        if rest_text.is_empty() {
            line.fragments[index] = letter;
        } else {
            let rest_width = measurer.measure(&rest_text, &text_style(style)).width;
            let rest = Fragment {
                node_id: fragment.node_id,
                x: fragment.x + letter_width,
                width: rest_width,
                dy: fragment.dy,
                extent: fragment.extent,
                content: FragmentContent::Text {
                    text: rest_text,
                    style: style.clone(),
                },
            };
            line.fragments[index] = letter;
            line.fragments.insert(index + 1, rest);
        }
        break;
    }
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
    /// Width of a single space per text style — one measure per style
    /// instead of one per inter-word gap.
    space_widths: std::collections::HashMap<SpaceKey, f32>,
}

/// The baseline offset of `vertical-align: sub` / `super` (positive
/// lowers the text). The shift is a third of the *surrounding* text's
/// size — the baseline a superscript is raised from belongs to its
/// parent, not to the smaller type riding on it.
fn sub_super_shift(style: &ComputedStyle, parent_font_size: f32) -> f32 {
    match style.vertical_align {
        VerticalAlign::Sub => parent_font_size / 3.0,
        VerticalAlign::Super => -parent_font_size / 3.0,
        _ => 0.0,
    }
}

/// Form controls keep aligning on the text inside them even though they
/// clip what overflows; a scroll container does not.
///
/// A list box arguably belongs on the scroll-container side — its rows
/// scroll and there is no single line to align on — but making that
/// exception costs more than it gains: measured against real pages it
/// moved a handful of list boxes into place and knocked every form row
/// around them out of it. The right fix needs the alignment of the
/// rows themselves, not just the container's baseline.
fn keeps_baseline_while_clipping(layout: &LayoutBox) -> bool {
    match &layout.kind {
        crate::LayoutKind::Element(tag) => {
            matches!(tag.as_str(), "input" | "button" | "select")
        }
        crate::LayoutKind::Inline { .. } => false,
    }
}

/// Where an atomic inline's baseline sits, measured from its margin-box
/// top: the baseline of its own last line box. `None` when it has no line
/// box to align on (it is empty, or it clips its overflow), and the box
/// then sits with its bottom margin edge on the baseline.
fn atomic_ascent(laid: &LayoutBox) -> Option<f32> {
    fn last_baseline(layout: &LayoutBox) -> Option<f32> {
        if layout.style.overflow_y != crate::style::Overflow::Visible
            && !keeps_baseline_while_clipping(layout)
        {
            return None;
        }
        let own = match &layout.kind {
            crate::LayoutKind::Inline { lines } => lines
                .last()
                .map(|line| layout.content_box().y + line.y + line.baseline),
            crate::LayoutKind::Element(_) => None,
        };
        layout
            .children
            .iter()
            .filter_map(last_baseline)
            .chain(own)
            .reduce(f32::max)
    }
    last_baseline(laid).map(|baseline| baseline - laid.margin_box().y)
}

/// Hashable identity of the [`TextStyle`] inputs (f32s compared bitwise;
/// layout never produces -0.0/NaN font sizes).
type SpaceKey = (u32, crate::text::FaceKey, u32);

fn space_key(style: &TextStyle) -> SpaceKey {
    (
        style.font_size.to_bits(),
        style.face(),
        style.letter_spacing.to_bits(),
    )
}

impl<'a> LineBuilder<'a> {
    fn style_of(&self, node_id: NodeId) -> &'a ComputedStyle {
        self.styles.by_node.get(&node_id).unwrap_or(self.container)
    }

    /// The content area of an inline box in `style`: how far its font
    /// reaches above and below the baseline, leading excluded.
    fn content_extent(&self, style: &ComputedStyle) -> (f32, f32) {
        self.measurer
            .content_extent(&self.text_style_of(style))
            // Without real metrics, split the em 0.8 above the baseline.
            .unwrap_or((style.font_size * 0.8, style.font_size * 0.2))
    }

    /// How far an inline box in `style` reaches above and below the
    /// baseline: its content area plus half of its leading on each side.
    fn leaded_extent(&self, style: &ComputedStyle) -> (f32, f32) {
        let (ascent, descent) = self.content_extent(style);
        let half_leading = (self.line_height_of(style) - (ascent + descent)) / 2.0;
        (ascent + half_leading, descent + half_leading)
    }

    /// How far an atomic inline reaches above and below the baseline it
    /// aligns on.
    fn atomic_extent(&self, laid: &LayoutBox) -> (f32, f32) {
        let height = laid.margin_box().height;
        if laid.style.vertical_align == VerticalAlign::Middle {
            // `middle` centers the box on the baseline raised by half an
            // x-height, so it reaches further up than down.
            let half_x = self.x_height() / 2.0;
            return (height / 2.0 + half_x, height / 2.0 - half_x);
        }
        let ascent = self.atomic_ascent_of(laid);
        (ascent, height - ascent)
    }

    /// Where a baseline-aligned atomic inline's baseline sits, measured
    /// from its margin-box top. Sizing the line and placing the box on it
    /// must agree, or the box lands off its own line.
    fn atomic_ascent_of(&self, laid: &LayoutBox) -> f32 {
        atomic_ascent(laid)
            .or_else(|| self.empty_control_ascent(laid))
            .unwrap_or_else(|| laid.margin_box().height)
    }

    /// An empty single-line control still aligns on where its text would
    /// sit: its own font's ascent below its top edge.
    fn empty_control_ascent(&self, laid: &LayoutBox) -> Option<f32> {
        if !keeps_baseline_while_clipping(laid) {
            return None;
        }
        let (ascent, _) = self
            .measurer
            .content_extent(&self.text_style_of(&laid.style))?;
        Some(laid.content_box().y - laid.margin_box().y + ascent)
    }

    /// Half of what `vertical-align: middle` centers against, in the
    /// block's own font.
    fn x_height(&self) -> f32 {
        self.measurer
            .x_height(&self.text_style_of(self.container))
            // Without real metrics, half an em is the usual stand-in.
            .unwrap_or(self.container.font_size * 0.5)
    }

    /// The baseline offset a fragment's `vertical-align` applies before
    /// the line is sized (positive lowers it).
    fn baseline_shift(&self, content: &FragmentContent) -> f32 {
        match content {
            FragmentContent::Text { style, .. } => sub_super_shift(style, self.container.font_size),
            FragmentContent::Box(_) => 0.0,
        }
    }

    fn text_style_of(&self, style: &ComputedStyle) -> TextStyle {
        TextStyle {
            font_size: style.font_size,
            font_weight: style.font_weight,
            families: style.font_family.clone(),
            italic: style.italic,
            letter_spacing: style.letter_spacing,
        }
    }

    /// The used line height of text in `style`: `normal` comes from the
    /// font when the measurer has one, anything else from the cascade.
    fn line_height_of(&self, style: &ComputedStyle) -> f32 {
        if !style.line_height_normal {
            return style.line_height;
        }
        self.measurer
            .normal_line_height(&self.text_style_of(style))
            .unwrap_or(style.line_height)
    }

    /// The advance of one space in `style`, cached per style.
    fn space_width(&mut self, style: &TextStyle) -> f32 {
        *self
            .space_widths
            .entry(space_key(style))
            .or_insert_with(|| self.measurer.measure(" ", style).width)
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

    fn place_word(
        &mut self,
        node_id: NodeId,
        word: &str,
        space_before: bool,
        background: Option<lumen_css::Color>,
    ) {
        self.start_line_if_needed();
        // `&'a` borrows the style map, not the builder, so it can live
        // across `&mut self` calls; the style is cloned only when a new
        // fragment actually needs to own it.
        let style = self.style_of(node_id);
        let text_style = TextStyle {
            font_size: style.font_size,
            font_weight: style.font_weight,
            families: style.font_family.clone(),
            italic: style.italic,
            letter_spacing: style.letter_spacing,
        };
        let word_width = self.measurer.measure(word, &text_style).width;
        let space_width = if space_before && !self.current.is_empty() {
            self.space_width(&text_style) + style.word_spacing
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
                self.append_text(node_id, chunk, kept_width, space, style, background);
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
            self.append_text(node_id, word, word_width, 0.0, style, background);
        } else {
            self.append_text(node_id, word, word_width, space_width, style, background);
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
                families: self.container.font_family.clone(),
                italic: self.container.italic,
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
        let extent = self.atomic_extent(&laid);
        self.current.push(Fragment {
            node_id,
            x: self.pen_x + space_width,
            width,
            dy: 0.0,
            extent,
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
        style: &ComputedStyle,
        background: Option<lumen_css::Color>,
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
            // Text has no background of its own; the fragment carries
            // whichever inline box around it does, so paint can fill
            // behind the run without walking back up the tree.
            let mut style = style.clone();
            style.background_color = background;
            self.current.push(Fragment {
                node_id,
                x: self.pen_x + space_width,
                width: word_width,
                dy: 0.0,
                extent: self.content_extent(&style),
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
                families: self.container.font_family.clone(),
                italic: self.container.italic,
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
                        families: style.font_family.clone(),
                        italic: style.italic,
                        letter_spacing: style.letter_spacing,
                    };
                    // Binary search the longest prefix that fits, instead
                    // of re-measuring a cloned candidate per character
                    // (O(n²) → O(n log n)). Assumes prefix widths are
                    // non-decreasing, which holds for any real measurer.
                    let boundaries: Vec<usize> = text
                        .char_indices()
                        .map(|(index, _)| index)
                        .chain(std::iter::once(text.len()))
                        .collect();
                    let fits = |end: usize| {
                        fragment.x + self.measurer.measure(&text[..end], &text_style).width
                            <= budget
                    };
                    let mut low = 0; // the empty prefix always "fits"
                    let mut high = boundaries.len();
                    while low + 1 < high {
                        let mid = (low + high) / 2;
                        if fits(boundaries[mid]) {
                            low = mid;
                        } else {
                            high = mid;
                        }
                    }
                    let mut cut = text[..boundaries[low]].to_string();
                    cut.push('…');
                    let width = self.measurer.measure(&cut, &text_style).width;
                    kept.push(Fragment {
                        node_id: fragment.node_id,
                        x: fragment.x,
                        width,
                        dy: 0.0,
                        extent: fragment.extent,
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

        // Every inline box on the line contributes its content area (the
        // font's ascent and descent) plus half of its leading — the
        // difference between its line-height and that content area — above
        // and below the baseline. The line box spans the tallest
        // contribution on each side. The block's own font takes part as
        // the strut, so an empty line still has the block's line height.
        let (mut above, mut below) = self.leaded_extent(self.container);
        for fragment in &fragments {
            let (fragment_above, fragment_below) = match &fragment.content {
                FragmentContent::Text { style, .. } => self.leaded_extent(style),
                FragmentContent::Box(laid) => self.atomic_extent(laid),
            };
            // A box raised or lowered off the baseline carries its extent
            // with it (`vertical-align: sub` / `super`).
            let shift = self.baseline_shift(&fragment.content);
            above = above.max(fragment_above - shift);
            below = below.max(fragment_below + shift);
        }
        let height = above + below;
        let baseline = above;

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
                    let default_top = baseline - self.atomic_ascent_of(laid);
                    let align = laid.style.vertical_align;
                    let top = match align {
                        VerticalAlign::Top => 0.0,
                        VerticalAlign::Middle => {
                            baseline - (box_height / 2.0 + self.x_height() / 2.0)
                        }
                        VerticalAlign::Bottom => height - box_height,
                        _ => default_top,
                    };
                    let dx = self.origin.0 + fragment.x - margin_box.x;
                    let dy = self.origin.1 + self.cursor_y + top - margin_box.y;
                    laid.translate(dx, dy);
                }
                // Text fragments carry a baseline offset for painting.
                FragmentContent::Text { style, .. } => {
                    let (ascent, _) = self.leaded_extent(style);
                    fragment.dy = match style.vertical_align {
                        VerticalAlign::Top => -(baseline - ascent),
                        VerticalAlign::Middle => (height - ascent) / 2.0 - (baseline - ascent),
                        VerticalAlign::Bottom => height - baseline,
                        VerticalAlign::Baseline | VerticalAlign::Sub | VerticalAlign::Super => {
                            sub_super_shift(style, self.container.font_size)
                        }
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

#[cfg(test)]
mod tests {
    use crate::geometry::Size;
    use crate::layout::LayoutKind;
    use lumen_css::Color;

    /// All line boxes of the first inline container in the page.
    fn lines_of(html: &str, width: f32) -> Vec<crate::LineBox> {
        let page = crate::build_page(
            &crate::test_support::with_body_reset(html),
            Size {
                width,
                height: 600.0,
            },
        );
        fn find(layout: &crate::LayoutBox) -> Option<Vec<crate::LineBox>> {
            if let LayoutKind::Inline { lines } = &layout.kind
                && !lines.is_empty()
            {
                return Some(lines.clone());
            }
            layout.children.iter().find_map(find)
        }
        find(&page.layout).expect("an inline line box")
    }

    #[test]
    fn first_letter_splits_and_styles_the_first_character() {
        let lines = lines_of(
            "<style>p::first-letter { color: rgb(1, 2, 3); font-size: 30px; font-weight: 700; }\
             </style><body><p>hello</p></body>",
            400.0,
        );
        let fragments = &lines[0].fragments;
        assert_eq!(fragments.len(), 2);
        assert_eq!(fragments[0].text(), Some("h"));
        let style = fragments[0].style().unwrap();
        assert_eq!(style.color, Color::rgb(1, 2, 3));
        assert_eq!(style.font_size, 30.0);
        assert_eq!(style.font_weight, crate::FontWeight(700));
        assert_eq!(fragments[1].text(), Some("ello"));
        assert_eq!(fragments[1].style().unwrap().color, Color::rgb(0, 0, 0));
        // The split accounts for the letter's own advance.
        assert!(fragments[1].x > fragments[0].x);
    }

    #[test]
    fn without_first_letter_rule_the_run_stays_whole() {
        let lines = lines_of("<body><p>hello</p></body>", 400.0);
        assert_eq!(lines[0].fragments.len(), 1);
        assert_eq!(lines[0].fragments[0].text(), Some("hello"));
    }

    #[test]
    fn first_line_styles_only_the_first_line() {
        let lines = lines_of(
            "<style>p::first-line { color: rgb(4, 5, 6); background-color: rgb(7, 8, 9); }\
             </style><body><p>one two three four five six seven eight nine ten</p></body>",
            120.0,
        );
        assert!(
            lines.len() > 1,
            "expected wrapping: {} line(s)",
            lines.len()
        );
        for fragment in &lines[0].fragments {
            let style = fragment.style().unwrap();
            assert_eq!(style.color, Color::rgb(4, 5, 6));
            assert_eq!(style.background_color, Some(Color::rgb(7, 8, 9)));
        }
        for fragment in &lines[1].fragments {
            let style = fragment.style().unwrap();
            assert_eq!(style.color, Color::rgb(0, 0, 0));
            assert_eq!(style.background_color, None);
        }
    }

    #[test]
    fn tab_size_controls_tab_expansion_in_pre() {
        let lines = lines_of(
            "<style>pre { tab-size: 2; }</style><body><pre>a\tb</pre></body>",
            400.0,
        );
        assert_eq!(lines[0].fragments[0].text(), Some("a  b"));
        // The engine's historical default stays four spaces.
        let lines = lines_of("<body><pre>a\tb</pre></body>", 400.0);
        assert_eq!(lines[0].fragments[0].text(), Some("a    b"));
    }
}
