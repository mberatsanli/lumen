//! Block layout with inline formatting.
//!
//! Layout runs in the classic phases per box: resolve width, position
//! horizontally, position vertically, lay out children, resolve height.
//! Block-level children stack vertically; consecutive inline-level
//! children (text and inline elements) form runs laid into shared line
//! boxes inside anonymous blocks (see `inline.rs`).
//!
//! Margin collapsing (simplified): vertical margins of adjacent in-flow
//! block siblings collapse (max of positives plus min of negatives), and a
//! parent with no top border/padding collapses its top margin with its
//! first block child's. Not covered: bottom parent-child collapsing and
//! empty blocks collapsing through themselves.

use crate::geometry::{Dimensions, EdgeSizes, Edges, Rect, Size};
use crate::image::ImageMap;
use crate::inline::{FragmentContent, LineBox, layout_inline_run};
use crate::style::{BoxSizing, Clear, ComputedStyle, Dimension, Display, Float, StyleMap};
use crate::text::TextMeasurer;
use lumen_html::{Document, ElementData, NodeId, NodeKind};
use std::fmt::Write as _;

/// The formatting role of a layout box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxType {
    Block,
    Inline,
    AnonymousBlock,
    Replaced,
}

/// What the box renders: an element box or flowed inline content.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutKind {
    Element(String),
    /// Line boxes produced by inline layout (text and inline elements).
    Inline {
        lines: Vec<LineBox>,
    },
}

/// A laid-out box with full box-model geometry.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutBox {
    pub node_id: NodeId,
    pub box_type: BoxType,
    pub kind: LayoutKind,
    pub dimensions: Dimensions,
    pub style: ComputedStyle,
    pub children: Vec<LayoutBox>,
}

impl LayoutBox {
    #[must_use]
    pub fn content_box(&self) -> Rect {
        self.dimensions.content
    }

    #[must_use]
    pub fn border_box(&self) -> Rect {
        self.dimensions.border_box()
    }

    #[must_use]
    pub fn margin_box(&self) -> Rect {
        self.dimensions.margin_box()
    }

    /// Shifts this box and its entire subtree (children, line fragments).
    pub(crate) fn translate(&mut self, dx: f32, dy: f32) {
        self.dimensions.content.x += dx;
        self.dimensions.content.y += dy;
        if let LayoutKind::Inline { lines } = &mut self.kind {
            for line in lines {
                for fragment in &mut line.fragments {
                    if let FragmentContent::Box(laid) = &mut fragment.content {
                        laid.translate(dx, dy);
                    }
                }
            }
        }
        for child in &mut self.children {
            child.translate(dx, dy);
        }
    }

    /// The deepest box under the point (page coordinates, CSS pixels),
    /// checking later siblings first (paint order: they are on top).
    #[must_use]
    pub fn hit_test(&self, x: f32, y: f32) -> Option<NodeId> {
        for child in self.children.iter().rev() {
            if let Some(hit) = child.hit_test(x, y) {
                return Some(hit);
            }
        }
        let rect = self.border_box();
        let inside =
            x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height;
        if !inside {
            return None;
        }
        // Inside inline content, individual fragments are the hit targets,
        // so a link is hit only on its own words.
        if let LayoutKind::Inline { lines } = &self.kind {
            let content = self.content_box();
            for line in lines {
                for fragment in &line.fragments {
                    if let FragmentContent::Box(laid) = &fragment.content
                        && let Some(hit) = laid.hit_test(x, y)
                    {
                        return Some(hit);
                    }
                    let fx = content.x + fragment.x;
                    let fy = content.y + line.y;
                    if x >= fx && x < fx + fragment.width && y >= fy && y < fy + line.height {
                        return Some(fragment.node_id);
                    }
                }
            }
        }
        // The #document root is not a hit target.
        (!matches!(&self.kind, LayoutKind::Element(tag) if tag == "#document"))
            .then_some(self.node_id)
    }
}

/// Lays out the whole document against a viewport. Pure function of its
/// inputs: relayout after a viewport change is just calling it again.
#[must_use]
pub fn layout_document(
    document: &Document,
    styles: &StyleMap,
    viewport: Size,
    measurer: &dyn TextMeasurer,
    images: &ImageMap,
) -> LayoutBox {
    let root_style = styles
        .by_node
        .get(&document.root())
        .cloned()
        .unwrap_or_default();

    let mut cursor_y = 0.0;
    let mut children = Vec::new();
    for child in document.children(document.root()) {
        if let Some(layout) = layout_node(
            document,
            styles,
            *child,
            0.0,
            &mut cursor_y,
            viewport.width,
            viewport,
            measurer,
            images,
        ) {
            children.push(layout);
        }
    }

    LayoutBox {
        node_id: document.root(),
        box_type: BoxType::Block,
        kind: LayoutKind::Element("#document".to_string()),
        dimensions: Dimensions {
            content: Rect {
                x: 0.0,
                y: 0.0,
                width: viewport.width,
                height: cursor_y.max(viewport.height),
            },
            ..Dimensions::default()
        },
        style: root_style,
        children,
    }
}

#[allow(clippy::too_many_arguments)]
fn layout_node(
    document: &Document,
    styles: &StyleMap,
    node_id: NodeId,
    containing_x: f32,
    cursor_y: &mut f32,
    containing_width: f32,
    viewport: Size,
    measurer: &dyn TextMeasurer,
    images: &ImageMap,
) -> Option<LayoutBox> {
    let style = styles.by_node.get(&node_id)?.clone();
    if style.display == Display::None {
        return None;
    }

    match &document.node(node_id).kind {
        // Text nodes are laid out by their parent's inline run, never here.
        NodeKind::Document | NodeKind::Text(_) => None,
        NodeKind::Element(element) => Some(layout_element(
            document,
            styles,
            node_id,
            element,
            style,
            containing_x,
            cursor_y,
            containing_width,
            viewport,
            measurer,
            images,
        )),
    }
}

/// Whether this child participates in inline flow (text, or an inline
/// element with no block-level descendant — blocks inside inlines get
/// promoted to block-level, a simplification of CSS's splitting rules).
fn is_inline_level(document: &Document, styles: &StyleMap, node_id: NodeId) -> bool {
    match &document.node(node_id).kind {
        NodeKind::Text(_) => true,
        NodeKind::Element(element) => {
            let Some(style) = styles.by_node.get(&node_id) else {
                return false;
            };
            // Floats leave the inline flow.
            if style.float != Float::None {
                return false;
            }
            // Atomic inlines flow in lines regardless of their contents.
            if style.display == Display::InlineBlock {
                return true;
            }
            // Replaced elements are promoted to block level (no inline images).
            if element.tag_name == "img" {
                return false;
            }
            style.display == Display::Inline && !has_block_descendant(document, styles, node_id)
        }
        NodeKind::Document => false,
    }
}

/// CSS collapsed-margin value: max of the positives plus min of the
/// negatives.
fn collapsed_margin(a: f32, b: f32) -> f32 {
    a.max(0.0).max(b.max(0.0)) + a.min(0.0).min(b.min(0.0))
}

/// The resolved top margin of the first in-flow child if it is
/// block-level; `None` when the first visible child is inline content.
fn first_block_child_top_margin(
    document: &Document,
    styles: &StyleMap,
    node_id: NodeId,
    containing_width: f32,
    viewport: Size,
) -> Option<f32> {
    for child in document.children(node_id) {
        let style = styles.by_node.get(child)?;
        if style.display == Display::None {
            continue;
        }
        if is_inline_level(document, styles, *child) {
            // Inline content separates the margins.
            match &document.node(*child).kind {
                // Whitespace-only text does not count as content.
                NodeKind::Text(text) if text.trim().is_empty() => continue,
                _ => return None,
            }
        }
        return Some(
            style
                .margin
                .top
                .resolve(containing_width, viewport)
                .unwrap_or(0.0),
        );
    }
    None
}

fn has_block_descendant(document: &Document, styles: &StyleMap, node_id: NodeId) -> bool {
    document.descendants(node_id).any(|descendant| {
        styles
            .by_node
            .get(&descendant)
            .is_some_and(|style| style.display == Display::Block)
    })
}

/// Active floats of one block container (margin boxes, page coordinates).
#[derive(Debug, Default)]
struct FloatContext {
    left: Vec<Rect>,
    right: Vec<Rect>,
}

impl FloatContext {
    /// Usable `(indent, width)` inside `[content_x, content_x+width)` for a
    /// line starting at absolute `y`.
    fn bounds_at(&self, content_x: f32, content_width: f32, y: f32) -> (f32, f32) {
        let intersects = |rect: &&Rect| y >= rect.y && y < rect.y + rect.height;
        let left_edge = self
            .left
            .iter()
            .filter(intersects)
            .map(|rect| rect.x + rect.width)
            .fold(content_x, f32::max);
        let right_edge = self
            .right
            .iter()
            .filter(intersects)
            .map(|rect| rect.x)
            .fold(content_x + content_width, f32::min);
        let indent = left_edge - content_x;
        (indent, (right_edge - left_edge).max(0.0))
    }

    /// The lowest bottom edge of the given side(s); `y` when none.
    fn clearance(&self, clear: Clear, y: f32) -> f32 {
        let bottom = |rects: &[Rect]| {
            rects
                .iter()
                .map(|rect| rect.y + rect.height)
                .fold(y, f32::max)
        };
        match clear {
            Clear::None => y,
            Clear::Left => bottom(&self.left),
            Clear::Right => bottom(&self.right),
            Clear::Both => bottom(&self.left).max(bottom(&self.right)),
        }
    }

    fn lowest_bottom(&self) -> f32 {
        self.left
            .iter()
            .chain(&self.right)
            .map(|rect| rect.y + rect.height)
            .fold(0.0, f32::max)
    }
}

/// Rightmost used extent of a laid-out subtree — the crude "preferred
/// width" behind shrink-to-fit for inline-blocks and floats. Auto-width
/// block descendants inflate to the probe width (documented limitation).
fn natural_right(layout: &LayoutBox) -> f32 {
    match &layout.kind {
        LayoutKind::Inline { lines } => {
            let content = layout.content_box();
            lines
                .iter()
                .flat_map(|line| line.fragments.iter())
                .map(|fragment| content.x + fragment.x + fragment.width)
                .fold(content.x, f32::max)
        }
        _ if layout.box_type == BoxType::Replaced
            || !matches!(layout.style.width, Dimension::Auto) =>
        {
            let border_box = layout.border_box();
            border_box.x + border_box.width
        }
        _ => {
            let inner = layout
                .children
                .iter()
                .map(natural_right)
                .fold(layout.content_box().x, f32::max);
            inner + layout.dimensions.padding.right + layout.dimensions.border.right
        }
    }
}

/// Lays out an element as an isolated box (for inline-blocks and floats):
/// auto widths shrink to fit their content, capped by `available`.
#[allow(clippy::too_many_arguments)]
fn layout_atomic_box(
    document: &Document,
    styles: &StyleMap,
    node_id: NodeId,
    available: f32,
    viewport: Size,
    measurer: &dyn TextMeasurer,
    images: &ImageMap,
) -> LayoutBox {
    let NodeKind::Element(element) = &document.node(node_id).kind else {
        unreachable!("atomic boxes are always elements");
    };
    let mut style = styles.by_node.get(&node_id).cloned().unwrap_or_default();
    if matches!(style.width, Dimension::Auto) && element.tag_name != "img" {
        let probe = layout_element(
            document,
            styles,
            node_id,
            element,
            style.clone(),
            0.0,
            &mut 0.0,
            available,
            viewport,
            measurer,
            images,
        );
        // Measure the probe's children (the target's own padding/border sit
        // outside its content width and must not be double-counted).
        let content_x = probe.content_box().x;
        let natural = (probe
            .children
            .iter()
            .map(natural_right)
            .fold(content_x, f32::max)
            - content_x)
            .min(available)
            .max(0.0);
        style.width = Dimension::Px(natural);
        style.box_sizing = BoxSizing::ContentBox;
    }
    layout_element(
        document, styles, node_id, element, style, 0.0, &mut 0.0, available, viewport, measurer,
        images,
    )
}

#[allow(clippy::too_many_arguments)]
fn layout_element(
    document: &Document,
    styles: &StyleMap,
    node_id: NodeId,
    element: &ElementData,
    style: ComputedStyle,
    containing_x: f32,
    cursor_y: &mut f32,
    containing_width: f32,
    viewport: Size,
    measurer: &dyn TextMeasurer,
    images: &ImageMap,
) -> LayoutBox {
    if element.tag_name == "img" {
        return layout_image(
            node_id,
            element,
            style,
            containing_x,
            cursor_y,
            containing_width,
            viewport,
            images,
        );
    }
    let mut margin = resolve_edges(&style.margin, containing_width, viewport);
    let border = style.border_width;
    let padding = resolve_edges(&style.padding, containing_width, viewport);

    // Phase 1: width. Explicit widths set the content box (or, with
    // box-sizing: border-box, the border box — content shrinks by padding
    // and border); auto fills the containing block minus all box edges.
    let content_width = style
        .width
        .resolve(containing_width, viewport)
        .map(|specified| match style.box_sizing {
            BoxSizing::ContentBox => specified,
            BoxSizing::BorderBox => {
                (specified - border.left - border.right - padding.left - padding.right).max(0.0)
            }
        })
        .unwrap_or_else(|| {
            (containing_width
                - margin.left
                - margin.right
                - border.left
                - border.right
                - padding.left
                - padding.right)
                .max(0.0)
        });

    // With an explicit width, auto margins absorb the leftover space:
    // both auto centers the box, one auto pushes it to the other side.
    if !matches!(style.width, Dimension::Auto) {
        let leftover = (containing_width
            - content_width
            - border.left
            - border.right
            - padding.left
            - padding.right
            - margin.left
            - margin.right)
            .max(0.0);
        match (
            matches!(style.margin.left, Dimension::Auto),
            matches!(style.margin.right, Dimension::Auto),
        ) {
            (true, true) => {
                margin.left = leftover / 2.0;
                margin.right = leftover / 2.0;
            }
            (true, false) => margin.left = leftover,
            (false, true) => margin.right = leftover,
            (false, false) => {}
        }
    }

    // Parent-child margin collapsing: with no top border/padding, the
    // parent's top margin collapses with its first block child's, and the
    // child's own top margin is suppressed inside.
    let child_top_collapse = if border.top == 0.0 && padding.top == 0.0 {
        first_block_child_top_margin(document, styles, node_id, content_width, viewport)
    } else {
        None
    };
    if let Some(child_top) = child_top_collapse {
        margin.top = collapsed_margin(margin.top, child_top);
    }

    // Phases 2 and 3: position. Block boxes stack vertically at the current
    // cursor; horizontal position comes from the containing block edge.
    let content_x = containing_x + margin.left + border.left + padding.left;
    *cursor_y += margin.top;
    let border_box_y = *cursor_y;
    let content_y = border_box_y + border.top + padding.top;

    // Phase 4: children. Consecutive inline-level children (text, inline
    // elements) form runs laid out into shared line boxes inside an
    // anonymous block; block-level children lay out as blocks.
    let mut child_cursor_y = content_y;
    let mut children = Vec::new();
    let mut run: Vec<NodeId> = Vec::new();
    let mut floats = FloatContext::default();
    // Bottom margin of the previous in-flow block sibling, for sibling
    // margin collapsing; None at the start or after inline content.
    let mut previous_bottom_margin: Option<f32> = None;
    // The first block child's top margin already collapsed into the parent.
    let mut suppress_next_top = child_top_collapse;

    fn flush_run(
        document: &Document,
        styles: &StyleMap,
        run: &mut Vec<NodeId>,
        owner: NodeId,
        style: &ComputedStyle,
        content_x: f32,
        content_width: f32,
        cursor_y: &mut f32,
        floats: &FloatContext,
        viewport: Size,
        measurer: &dyn TextMeasurer,
        images: &ImageMap,
        children: &mut Vec<LayoutBox>,
    ) {
        if run.is_empty() {
            return;
        }
        let run_top = *cursor_y;
        let bounds = |line_top: f32| floats.bounds_at(content_x, content_width, run_top + line_top);
        let mut layout_atomic = |node_id: NodeId, available: f32| {
            layout_atomic_box(
                document, styles, node_id, available, viewport, measurer, images,
            )
        };
        let (lines, height) = layout_inline_run(
            document,
            styles,
            run,
            style,
            (content_x, run_top),
            &bounds,
            measurer,
            &mut layout_atomic,
        );
        run.clear();
        if lines.is_empty() {
            return;
        }
        children.push(LayoutBox {
            node_id: owner,
            box_type: BoxType::AnonymousBlock,
            kind: LayoutKind::Inline { lines },
            dimensions: Dimensions {
                content: Rect {
                    x: content_x,
                    y: run_top,
                    width: content_width,
                    height,
                },
                ..Dimensions::default()
            },
            style: style.clone(),
            children: Vec::new(),
        });
        *cursor_y += height;
    }

    for child in document.children(node_id) {
        let Some(child_style) = styles.by_node.get(child) else {
            continue;
        };
        if child_style.display == Display::None {
            continue;
        }

        // Floats: laid out out-of-flow at the requested side, narrowing
        // subsequent inline content; the vertical cursor does not advance.
        if child_style.float != Float::None
            && matches!(&document.node(*child).kind, NodeKind::Element(_))
        {
            let side = child_style.float;
            let laid = layout_atomic_box(
                document,
                styles,
                *child,
                content_width,
                viewport,
                measurer,
                images,
            );
            let margin_box = laid.margin_box();
            let (width, height) = (margin_box.width, margin_box.height);
            // Find the highest y at or below the cursor where the float fits.
            let mut y = child_cursor_y;
            for _ in 0..64 {
                let (indent, available) = floats.bounds_at(content_x, content_width, y);
                if width <= available || available >= content_width {
                    let x = match side {
                        Float::Right => content_x + indent + available - width,
                        _ => content_x + indent,
                    };
                    let mut laid = laid;
                    laid.translate(x - margin_box.x, y - margin_box.y);
                    let rect = Rect {
                        x,
                        y,
                        width,
                        height,
                    };
                    match side {
                        Float::Right => floats.right.push(rect),
                        _ => floats.left.push(rect),
                    }
                    children.push(laid);
                    break;
                }
                y = floats.lowest_bottom().max(y + 1.0);
            }
            continue;
        }

        if is_inline_level(document, styles, *child) {
            // Whitespace-only text between blocks is not content and must
            // not interrupt margin collapsing.
            let is_blank_text = matches!(
                &document.node(*child).kind,
                NodeKind::Text(text) if text.trim().is_empty()
            );
            run.push(*child);
            if !is_blank_text {
                previous_bottom_margin = None;
                suppress_next_top = None;
            }
        } else {
            flush_run(
                document,
                styles,
                &mut run,
                node_id,
                &style,
                content_x,
                content_width,
                &mut child_cursor_y,
                &floats,
                viewport,
                measurer,
                images,
                &mut children,
            );
            // clear: drop below the floats of the given side(s).
            if child_style.clear != Clear::None {
                let cleared = floats.clearance(child_style.clear, child_cursor_y);
                if cleared > child_cursor_y {
                    child_cursor_y = cleared;
                    previous_bottom_margin = None;
                    suppress_next_top = None;
                }
            }
            // Sibling margin collapsing: undo the doubled gap so it equals
            // the collapsed value. The suppressed first top margin (already
            // collapsed into the parent) is removed entirely.
            let child_top = styles
                .by_node
                .get(child)
                .and_then(|style| style.margin.top.resolve(content_width, viewport))
                .unwrap_or(0.0);
            if let Some(previous) = previous_bottom_margin {
                let gap = collapsed_margin(previous, child_top);
                child_cursor_y -= previous + child_top - gap;
            } else if let Some(collapsed) = suppress_next_top.take() {
                debug_assert_eq!(collapsed, child_top);
                child_cursor_y -= child_top;
            }
            if let Some(layout) = layout_node(
                document,
                styles,
                *child,
                content_x,
                &mut child_cursor_y,
                content_width,
                viewport,
                measurer,
                images,
            ) {
                previous_bottom_margin = Some(layout.dimensions.margin.bottom);
                children.push(layout);
            }
        }
    }
    flush_run(
        document,
        styles,
        &mut run,
        node_id,
        &style,
        content_x,
        content_width,
        &mut child_cursor_y,
        &floats,
        viewport,
        measurer,
        images,
        &mut children,
    );
    // A container's auto height contains its floats (BFC-root behavior).
    child_cursor_y = child_cursor_y.max(floats.lowest_bottom().min(f32::MAX));
    if floats.lowest_bottom() > 0.0 {
        child_cursor_y = child_cursor_y.max(floats.lowest_bottom());
    }

    // Phase 5: height. Explicit heights win (border-box heights shrink by
    // vertical padding and border); auto grows from the children. Percent
    // heights are unsupported and treated as auto; vh works.
    let content_height = match style.height {
        Dimension::Auto | Dimension::Percent(_) => (child_cursor_y - content_y).max(0.0),
        explicit => explicit
            .resolve(containing_width, viewport)
            .map(|specified| match style.box_sizing {
                BoxSizing::ContentBox => specified,
                BoxSizing::BorderBox => {
                    (specified - border.top - border.bottom - padding.top - padding.bottom).max(0.0)
                }
            })
            .unwrap_or_else(|| (child_cursor_y - content_y).max(0.0)),
    };

    let dimensions = Dimensions {
        content: Rect {
            x: content_x,
            y: content_y,
            width: content_width,
            height: content_height,
        },
        padding,
        border,
        margin,
    };
    *cursor_y = dimensions.margin_box().y + dimensions.margin_box().height;

    LayoutBox {
        node_id,
        box_type: match style.display {
            Display::Inline => BoxType::Inline,
            _ => BoxType::Block,
        },
        kind: LayoutKind::Element(element.tag_name.clone()),
        dimensions,
        style,
        children,
    }
}

/// Lays out an `<img>` as a block-level replaced box. Size comes from CSS
/// width/height, then the width/height attributes, then the intrinsic
/// size; a single known dimension keeps the intrinsic aspect ratio.
#[allow(clippy::too_many_arguments)]
fn layout_image(
    node_id: NodeId,
    element: &ElementData,
    style: ComputedStyle,
    containing_x: f32,
    cursor_y: &mut f32,
    containing_width: f32,
    viewport: Size,
    images: &ImageMap,
) -> LayoutBox {
    let margin = resolve_edges(&style.margin, containing_width, viewport);
    let border = style.border_width;
    let padding = resolve_edges(&style.padding, containing_width, viewport);

    let attribute = |name: &str| {
        element
            .attributes
            .get(name)
            .and_then(|value| value.trim().parse::<f32>().ok())
            .filter(|value| *value >= 0.0)
    };
    let intrinsic = images
        .get(&node_id)
        .map(|image| (image.width as f32, image.height as f32));
    let specified_width = style
        .width
        .resolve(containing_width, viewport)
        .or_else(|| attribute("width"));
    let specified_height = style
        .height
        .resolve(0.0, viewport)
        .filter(|_| !matches!(style.height, Dimension::Percent(_)))
        .or_else(|| attribute("height"));

    let (content_width, content_height) = match (specified_width, specified_height) {
        (Some(width), Some(height)) => (width, height),
        (Some(width), None) => {
            let height = intrinsic.map_or(width, |(iw, ih)| width * ih / iw.max(1.0));
            (width, height)
        }
        (None, Some(height)) => {
            let width = intrinsic.map_or(height, |(iw, ih)| height * iw / ih.max(1.0));
            (width, height)
        }
        (None, None) => intrinsic.unwrap_or((0.0, 0.0)),
    };

    *cursor_y += margin.top;
    let content_x = containing_x + margin.left + border.left + padding.left;
    let content_y = *cursor_y + border.top + padding.top;
    let dimensions = Dimensions {
        content: Rect {
            x: content_x,
            y: content_y,
            width: content_width,
            height: content_height,
        },
        padding,
        border,
        margin,
    };
    *cursor_y = dimensions.margin_box().y + dimensions.margin_box().height;

    LayoutBox {
        node_id,
        box_type: BoxType::Replaced,
        kind: LayoutKind::Element("img".to_string()),
        dimensions,
        style,
        children: Vec::new(),
    }
}

/// Percentages resolve against the containing block width; `auto` margins
/// and paddings resolve to zero (no centering yet).
fn resolve_edges(edges: &EdgeSizes<Dimension>, containing_width: f32, viewport: Size) -> Edges {
    let resolve =
        |dimension: &Dimension| dimension.resolve(containing_width, viewport).unwrap_or(0.0);
    Edges {
        top: resolve(&edges.top),
        right: resolve(&edges.right),
        bottom: resolve(&edges.bottom),
        left: resolve(&edges.left),
    }
}

/// Human-readable layout dump (border-box coordinates).
#[must_use]
pub fn dump_layout(layout: &LayoutBox) -> String {
    let mut output = String::new();
    dump_layout_box(layout, 0, &mut output);
    output
}

fn dump_layout_box(layout: &LayoutBox, depth: usize, output: &mut String) {
    let indent = "  ".repeat(depth);
    let label = match &layout.kind {
        LayoutKind::Element(tag) => format!("<{tag}>"),
        LayoutKind::Inline { lines } => {
            let joined = lines
                .iter()
                .flat_map(|line| line.fragments.iter())
                .filter_map(|fragment| fragment.text())
                .collect::<Vec<_>>()
                .join(" ");
            format!(
                "\"{joined}\" ({} line{})",
                lines.len(),
                if lines.len() == 1 { "" } else { "s" }
            )
        }
    };
    let rect = layout.border_box();
    let _ = writeln!(
        output,
        "{indent}{label} x={} y={} w={} h={}",
        rect.x, rect.y, rect.width, rect.height
    );
    for child in &layout.children {
        dump_layout_box(child, depth + 1, output);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::compute_styles;
    use lumen_html::parse_document;

    const VIEWPORT: Size = Size {
        width: 800.0,
        height: 600.0,
    };

    fn layout_of(html: &str) -> LayoutBox {
        let document = parse_document(html);
        let author = lumen_css::parse_stylesheet(&crate::extract_embedded_css(&document));
        let styles = compute_styles(&document, &author);
        layout_document(
            &document,
            &styles,
            VIEWPORT,
            &crate::text::HeuristicMeasurer,
            &crate::image::ImageMap::new(),
        )
    }

    /// The brief's canonical deterministic-geometry example.
    #[test]
    fn parent_and_children_have_exact_rects() {
        let layout = layout_of(
            "<style>
                .parent { width: 300px; padding: 10px; }
                .child { height: 50px; margin-bottom: 5px; }
             </style>
             <div class='parent'><div class='child'></div><div class='child'></div></div>",
        );
        let parent = &layout.children[0];
        assert_eq!(
            parent.content_box(),
            Rect {
                x: 10.0,
                y: 10.0,
                width: 300.0,
                height: 110.0 // 50 + 5 + 50 + 5
            }
        );
        assert_eq!(
            parent.border_box(),
            Rect {
                x: 0.0,
                y: 0.0,
                width: 320.0,
                height: 130.0
            }
        );

        let first = &parent.children[0];
        let second = &parent.children[1];
        assert_eq!(
            first.border_box(),
            Rect {
                x: 10.0,
                y: 10.0,
                width: 300.0,
                height: 50.0
            }
        );
        assert_eq!(
            second.border_box(),
            Rect {
                x: 10.0,
                y: 65.0, // 10 + 50 + 5
                width: 300.0,
                height: 50.0
            }
        );
    }

    #[test]
    fn border_takes_space_in_the_box_model() {
        let layout = layout_of(
            "<style>
                div { width: 100px; height: 20px; padding: 5px;
                      border-width: 3px; margin: 2px; }
             </style>
             <div></div>",
        );
        let div = &layout.children[0];
        assert_eq!(
            div.content_box(),
            Rect {
                x: 10.0, // margin 2 + border 3 + padding 5
                y: 10.0,
                width: 100.0,
                height: 20.0
            }
        );
        assert_eq!(
            div.border_box(),
            Rect {
                x: 2.0,
                y: 2.0,
                width: 116.0,
                height: 36.0
            }
        );
        assert_eq!(div.margin_box().height, 40.0);
    }

    #[test]
    fn auto_width_fills_containing_block_minus_box_edges() {
        let layout = layout_of(
            "<style>div { border-width: 2px; padding: 8px; margin: 10px; }</style><div></div>",
        );
        let div = &layout.children[0];
        // 800 - 2*10 margin - 2*2 border - 2*8 padding
        assert_eq!(div.content_box().width, 760.0);
        assert_eq!(div.border_box().width, 780.0);
    }

    #[test]
    fn nested_blocks_offset_by_each_level() {
        let layout = layout_of(
            "<style>
                .outer { padding: 10px; }
                .inner { padding: 5px; height: 30px; }
             </style>
             <div class='outer'><div class='inner'></div></div>",
        );
        let outer = &layout.children[0];
        let inner = &outer.children[0];
        assert_eq!(inner.content_box().x, 15.0);
        assert_eq!(inner.content_box().y, 15.0);
        assert_eq!(outer.border_box().height, 60.0); // 10 + (5+30+5) + 10
    }

    #[test]
    fn auto_height_grows_from_children_explicit_height_wins() {
        let auto = layout_of(
            "<style>.child { height: 40px; }</style><div><div class='child'></div></div>",
        );
        assert_eq!(auto.children[0].border_box().height, 40.0);

        let fixed = layout_of(
            "<style>.parent { height: 25px; } .child { height: 40px; }</style>\
             <div class='parent'><div class='child'></div></div>",
        );
        assert_eq!(fixed.children[0].border_box().height, 25.0);
    }

    #[test]
    fn auto_margins_center_a_fixed_width_box() {
        let layout = layout_of(
            "<style>div { width: 100px; height: 10px; margin: 0 auto; }</style><div></div>",
        );
        let div = &layout.children[0];
        // (800 - 100) / 2 on each side.
        assert_eq!(div.border_box().x, 350.0);
        assert_eq!(div.dimensions.margin.left, 350.0);
        assert_eq!(div.dimensions.margin.right, 350.0);
    }

    #[test]
    fn single_auto_margin_takes_all_leftover() {
        let layout = layout_of(
            "<style>div { width: 100px; height: 10px; margin-left: auto; }</style><div></div>",
        );
        let div = &layout.children[0];
        assert_eq!(div.border_box().x, 700.0);
    }

    #[test]
    fn em_lengths_resolve_against_font_size() {
        let layout = layout_of(
            "<style>div { font-size: 20px; margin-left: 2em; padding-top: 1.5em; height: 10px; }\
             </style><div></div>",
        );
        let div = &layout.children[0];
        assert_eq!(div.border_box().x, 40.0);
        assert_eq!(div.dimensions.padding.top, 30.0);
    }

    #[test]
    fn inline_elements_share_a_line_with_text() {
        // 8px/char at default 16px font.
        let layout = layout_of(
            "<style>div { width: 400px; margin: 0; padding: 0; }</style>\
             <div>one <span>two</span> three</div>",
        );
        let anonymous = &layout.children[0].children[0];
        let LayoutKind::Inline { lines } = &anonymous.kind else {
            panic!("expected inline content");
        };
        assert_eq!(lines.len(), 1, "all words share one line: {lines:?}");
        let texts: Vec<&str> = lines[0]
            .fragments
            .iter()
            .filter_map(|fragment| fragment.text())
            .collect();
        assert_eq!(texts, vec!["one", "two", "three"]);
        // "one " = 4 chars * 8px, span starts after the space.
        assert_eq!(lines[0].fragments[1].x, 32.0);
    }

    #[test]
    fn words_join_without_boundary_whitespace() {
        let layout = layout_of("<div>foo<span>bar</span></div>");
        let LayoutKind::Inline { lines } = &layout.children[0].children[0].kind else {
            panic!("expected inline content");
        };
        // No space between fragments: "foo" ends at 24, "bar" starts at 24.
        assert_eq!(lines[0].fragments[1].x, 24.0);
    }

    #[test]
    fn br_forces_a_line_break() {
        let layout = layout_of("<div>a<br>b</div>");
        let LayoutKind::Inline { lines } = &layout.children[0].children[0].kind else {
            panic!("expected inline content");
        };
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].fragments[0].text(), Some("a"));
        assert_eq!(lines[1].fragments[0].text(), Some("b"));
    }

    #[test]
    fn mixed_block_and_inline_children_get_anonymous_blocks() {
        let layout = layout_of("<div>before<p>block</p>after</div>");
        let div = &layout.children[0];
        assert_eq!(div.children.len(), 3);
        assert_eq!(div.children[0].box_type, BoxType::AnonymousBlock);
        assert_eq!(div.children[1].box_type, BoxType::Block);
        assert_eq!(div.children[2].box_type, BoxType::AnonymousBlock);
        // Vertical order: run, block, run.
        assert!(div.children[0].content_box().y < div.children[1].content_box().y);
        assert!(div.children[1].content_box().y < div.children[2].content_box().y);
    }

    #[test]
    fn line_height_uses_tallest_fragment() {
        let layout = layout_of(
            "<style>span { font-size: 32px; line-height: 1; }</style>\
             <div>small <span>BIG</span></div>",
        );
        let LayoutKind::Inline { lines } = &layout.children[0].children[0].kind else {
            panic!("expected inline content");
        };
        // Default 16px text line-height is 22.4; the span's is 32.
        assert_eq!(lines[0].height, 32.0);
        assert_eq!(lines[0].baseline, 32.0);
    }

    #[test]
    fn fragment_hit_test_targets_the_text_node() {
        let document = parse_document("<div>plain <a href='x'>link</a></div>");
        let author = lumen_css::parse_stylesheet(&crate::extract_embedded_css(&document));
        let styles = compute_styles(&document, &author);
        let layout = layout_document(
            &document,
            &styles,
            VIEWPORT,
            &crate::text::HeuristicMeasurer,
            &crate::image::ImageMap::new(),
        );
        // "plain " occupies x 0..48 (6 chars * 8px); the link text follows.
        let link_hit = layout.hit_test(50.0, 5.0).expect("hit on link text");
        let plain_hit = layout.hit_test(5.0, 5.0).expect("hit on plain text");
        assert_ne!(link_hit, plain_hit);
        let link_ancestor = std::iter::once(link_hit)
            .chain(document.ancestors(link_hit))
            .find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "a")
            });
        assert!(link_ancestor.is_some());
        let plain_ancestor = std::iter::once(plain_hit)
            .chain(document.ancestors(plain_hit))
            .find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "a")
            });
        assert!(plain_ancestor.is_none());
    }

    #[test]
    fn image_sizes_from_attributes_css_and_intrinsic_ratio() {
        use crate::image::{ImageMap, RasterImage};
        use std::sync::Arc;

        let document = parse_document(
            "<style>.styled { width: 200px; }</style>\
             <img src='a' width='40' height='30'>\
             <img class='styled' src='a'>\
             <img src='a'>",
        );
        let author = lumen_css::parse_stylesheet(&crate::extract_embedded_css(&document));
        let styles = compute_styles(&document, &author);
        let mut images = ImageMap::new();
        // 100x50 intrinsic (2:1), attached to every img in the page.
        for (node, _) in crate::image::collect_image_sources(&document) {
            images.insert(
                node,
                Arc::new(RasterImage {
                    width: 100,
                    height: 50,
                    rgba: vec![0; 100 * 50 * 4],
                    encoded: Vec::new(),
                    mime: "image/png",
                }),
            );
        }
        let layout = layout_document(
            &document,
            &styles,
            VIEWPORT,
            &crate::text::HeuristicMeasurer,
            &images,
        );
        let boxes: Vec<&LayoutBox> = layout
            .children
            .iter()
            .filter(|child| child.box_type == BoxType::Replaced)
            .collect();
        assert_eq!(boxes.len(), 3);
        // Attributes win when CSS is absent.
        assert_eq!(boxes[0].content_box().width, 40.0);
        assert_eq!(boxes[0].content_box().height, 30.0);
        // CSS width 200 + 2:1 intrinsic ratio → height 100.
        assert_eq!(boxes[1].content_box().width, 200.0);
        assert_eq!(boxes[1].content_box().height, 100.0);
        // Nothing specified → intrinsic size.
        assert_eq!(boxes[2].content_box().width, 100.0);
        assert_eq!(boxes[2].content_box().height, 50.0);
    }

    #[test]
    fn border_box_sizing_shrinks_the_content_box() {
        let layout = layout_of(
            "<style>
                div { box-sizing: border-box; width: 200px; height: 100px;
                      padding: 20px; border-width: 5px; }
             </style><div></div>",
        );
        let div = &layout.children[0];
        // width/height name the border box.
        assert_eq!(div.border_box().width, 200.0);
        assert_eq!(div.border_box().height, 100.0);
        assert_eq!(div.content_box().width, 150.0);
        assert_eq!(div.content_box().height, 50.0);
    }

    #[test]
    fn content_box_sizing_adds_edges_outside() {
        let layout = layout_of(
            "<style>
                div { width: 200px; height: 100px; padding: 20px; border-width: 5px; }
             </style><div></div>",
        );
        let div = &layout.children[0];
        assert_eq!(div.content_box().width, 200.0);
        assert_eq!(div.border_box().width, 250.0);
        assert_eq!(div.border_box().height, 150.0);
    }

    #[test]
    fn border_box_never_goes_negative() {
        let layout = layout_of(
            "<style>div { box-sizing: border-box; width: 10px; padding: 20px; height: 5px; }\
             </style><div></div>",
        );
        let div = &layout.children[0];
        assert_eq!(div.content_box().width, 0.0);
        assert_eq!(div.border_box().width, 40.0); // padding only
    }

    #[test]
    fn sibling_margins_collapse_to_the_larger_one() {
        let layout = layout_of(
            "<style>
                .a { height: 10px; margin-bottom: 20px; }
                .b { height: 10px; margin-top: 12px; }
             </style><div><div class='a'></div><div class='b'></div></div>",
        );
        let container = &layout.children[0];
        let first = &container.children[0];
        let second = &container.children[1];
        // Gap is max(20, 12) = 20, not 32.
        assert_eq!(second.border_box().y - first.border_box().y, 30.0);
        assert_eq!(container.border_box().height, 40.0);
    }

    #[test]
    fn inline_content_between_blocks_prevents_collapsing() {
        let layout = layout_of(
            "<style>
                .a { height: 10px; margin-bottom: 20px; }
                .b { height: 10px; margin-top: 12px; }
             </style><div><div class='a'></div>separator<div class='b'></div></div>",
        );
        let container = &layout.children[0];
        // a (10) + margin 20 + text line (22.4) + margin 12 + b (10)
        let second = container.children.last().unwrap();
        assert_eq!(second.border_box().y, 10.0 + 20.0 + 22.4 + 12.0);
    }

    #[test]
    fn first_child_top_margin_escapes_an_edgeless_parent() {
        let layout = layout_of(
            "<style>
                .parent { margin-top: 10px; background-color: #eee; }
                .child { margin-top: 30px; height: 10px; }
             </style><div class='parent'><div class='child'></div></div>",
        );
        let parent = &layout.children[0];
        let child = &parent.children[0];
        // Parent moves down by the collapsed max(10, 30) = 30...
        assert_eq!(parent.border_box().y, 30.0);
        // ...and the child sits flush with the parent's top.
        assert_eq!(child.border_box().y, 30.0);
        assert_eq!(parent.border_box().height, 10.0);
    }

    #[test]
    fn padding_blocks_parent_child_collapsing() {
        let layout = layout_of(
            "<style>
                .parent { margin-top: 10px; padding-top: 4px; }
                .child { margin-top: 30px; height: 10px; }
             </style><div class='parent'><div class='child'></div></div>",
        );
        let parent = &layout.children[0];
        let child = &parent.children[0];
        assert_eq!(parent.border_box().y, 10.0);
        assert_eq!(child.border_box().y, 10.0 + 4.0 + 30.0);
    }

    #[test]
    fn inline_block_flows_in_the_line_and_lays_out_inside() {
        let layout = layout_of(
            "<style>
                .chip { display: inline-block; width: 60px; height: 20px;
                        background-color: #eee; }
             </style><div>before <span class='chip'></span> after</div>",
        );
        let anonymous = &layout.children[0].children[0];
        let LayoutKind::Inline { lines } = &anonymous.kind else {
            panic!("expected inline content");
        };
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].fragments.len(), 3);
        // "before " = 7 chars * 8px = 56; chip occupies the next 60px slot.
        let chip = &lines[0].fragments[1];
        assert_eq!(chip.x, 56.0);
        assert_eq!(chip.width, 60.0);
        let FragmentContent::Box(chip_box) = &chip.content else {
            panic!("expected an atomic box");
        };
        // Bottom sits on the baseline (= its own 20px height here... baseline
        // is max(text 16, box 20) = 20): top = line.y + 20 - 20 = 0.
        assert_eq!(chip_box.border_box().y, 0.0);
        assert_eq!(chip_box.border_box().x, 56.0);
        // Text after the chip continues on the same line.
        assert_eq!(lines[0].fragments[2].x, 56.0 + 60.0 + 8.0);
    }

    #[test]
    fn auto_width_inline_block_shrinks_to_content() {
        let layout = layout_of(
            "<style>.tag { display: inline-block; padding: 5px; }</style>\
             <div><span class='tag'>hi</span> rest</div>",
        );
        let LayoutKind::Inline { lines } = &layout.children[0].children[0].kind else {
            panic!("expected inline content");
        };
        // Content "hi" = 16px + 2*5 padding = 26 margin-box width.
        assert_eq!(lines[0].fragments[0].width, 26.0);
    }

    #[test]
    fn float_left_narrows_following_lines() {
        let layout = layout_of(
            "<style>
                .f { float: left; width: 100px; height: 30px; background-color: #eee; }
             </style>\
             <div class='wrap'><div class='f'></div>aaaa bbbb cccc</div>",
        );
        let wrap = &layout.children[0];
        // First child is the float, positioned at the left edge, no flow advance.
        let float_box = &wrap.children[0];
        assert_eq!(float_box.border_box().x, 0.0);
        assert_eq!(float_box.border_box().y, 0.0);
        let LayoutKind::Inline { lines } = &wrap.children[1].kind else {
            panic!("expected inline content");
        };
        // The line starts to the right of the 100px float.
        assert_eq!(lines[0].fragments[0].x, 100.0);
        // Container height includes the float.
        assert!(wrap.border_box().height >= 30.0);
    }

    #[test]
    fn float_right_hugs_the_right_edge() {
        let layout = layout_of(
            "<style>.f { float: right; width: 50px; height: 10px; }</style>\
             <div><div class='f'></div>text</div>",
        );
        let float_box = &layout.children[0].children[0];
        assert_eq!(float_box.border_box().x, 800.0 - 50.0);
    }

    #[test]
    fn clear_drops_below_floats() {
        let layout = layout_of(
            "<style>
                .f { float: left; width: 100px; height: 40px; }
                .c { clear: left; height: 10px; }
             </style><div><div class='f'></div><div class='c'></div></div>",
        );
        let cleared = &layout.children[0].children[1];
        assert_eq!(cleared.border_box().y, 40.0);
    }

    #[test]
    fn relayout_respects_new_viewport_width() {
        let document = parse_document("<div></div>");
        let styles = compute_styles(&document, &lumen_css::Stylesheet::default());
        let narrow = layout_document(
            &document,
            &styles,
            Size {
                width: 400.0,
                height: 100.0,
            },
            &crate::text::HeuristicMeasurer,
            &crate::image::ImageMap::new(),
        );
        let wide = layout_document(
            &document,
            &styles,
            Size {
                width: 900.0,
                height: 100.0,
            },
            &crate::text::HeuristicMeasurer,
            &crate::image::ImageMap::new(),
        );
        assert_eq!(narrow.children[0].content_box().width, 400.0);
        assert_eq!(wide.children[0].content_box().width, 900.0);
    }

    #[test]
    fn text_becomes_anonymous_inline_content_inside_blocks() {
        let layout = layout_of("<div>hi</div>");
        let div = &layout.children[0];
        assert_eq!(div.box_type, BoxType::Block);
        let anonymous = &div.children[0];
        assert_eq!(anonymous.box_type, BoxType::AnonymousBlock);
        assert!(matches!(&anonymous.kind, LayoutKind::Inline { lines } if lines.len() == 1));
    }
}
