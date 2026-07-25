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

use crate::flex::layout_flex_children;
use crate::float::FloatContext;
use crate::geometry::{Dimensions, EdgeSizes, Edges, Rect, Size};
use crate::image::ImageMap;
use crate::inline::{FragmentContent, LineBox, layout_inline_run};
use crate::style::{
    BoxSizing, Clear, ComputedStyle, Dimension, Display, Float, Overflow, PointerEvents, Position,
    StyleMap,
};
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

    /// Children sorted for painting: ascending `z-index` (default 0),
    /// stable so DOM order breaks ties. A simplification of CSS stacking
    /// contexts.
    #[must_use]
    pub fn children_in_paint_order(&self) -> Vec<&LayoutBox> {
        let mut ordered: Vec<&LayoutBox> = self.children.iter().collect();
        ordered.sort_by_key(|child| child.style.z_index.unwrap_or(0));
        ordered
    }

    /// The innermost `overflow: scroll/auto` box under a point (page
    /// coordinates), with its maximum scroll offset. Follows current
    /// scroll offsets while descending.
    #[must_use]
    pub fn scrollable_under(
        &self,
        x: f32,
        y: f32,
        scroll_offsets: &std::collections::HashMap<NodeId, f32>,
    ) -> Option<(NodeId, f32)> {
        let rect = self.border_box();
        let inside =
            x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height;
        let scroll_offset = match scroll_offsets.get(&self.node_id) {
            Some(offset) if self.style.clips_overflow() => *offset,
            _ => 0.0,
        };
        let (child_x, child_y) = (x, y + scroll_offset);
        // Sticky children sit at their stuck (visual) position: convert
        // the point into their flow coordinates per child.
        let child_point = |child: &LayoutBox| {
            (
                child_x,
                child_y - self.sticky_child_shift(child, scroll_offset),
            )
        };
        // Fast path: with no z-index anywhere, paint order is DOM order,
        // so neither the collecting Vec nor the sort is needed.
        let found = if self
            .children
            .iter()
            .all(|child| child.style.z_index.unwrap_or(0) == 0)
        {
            self.children.iter().rev().find_map(|child| {
                let (px, py) = child_point(child);
                child.scrollable_under(px, py, scroll_offsets)
            })
        } else {
            self.children_in_paint_order()
                .into_iter()
                .rev()
                .find_map(|child| {
                    let (px, py) = child_point(child);
                    child.scrollable_under(px, py, scroll_offsets)
                })
        };
        if found.is_some() {
            return found;
        }
        // A `pointer-events: none` box never scrolls under the pointer;
        // its children were still tested above and may scroll themselves.
        if inside
            && self.style.pointer_events != PointerEvents::None
            && self.style.overflow_y == crate::style::Overflow::Scroll
        {
            let max = self.max_inner_scroll();
            if max > 0.0 {
                return Some((self.node_id, max));
            }
        }
        None
    }

    /// The vertical paint-time shift of a `position: sticky` child when
    /// this box is its scroll container, scrolled by `offset`. Zero for
    /// non-sticky children, a zero offset, or sticky without a `top`/
    /// `bottom` constraint. The child sticks to the container's padding
    /// box inset and never leaves the padding box. (Horizontal sticking
    /// is unsupported: inner scrolling is vertical-only.)
    #[must_use]
    pub fn sticky_child_shift(&self, child: &LayoutBox, offset: f32) -> f32 {
        if child.style.position != Position::Sticky || offset == 0.0 {
            return 0.0;
        }
        let containing = self.dimensions.padding_box();
        let flow_top = child.border_box().y;
        let height = child.border_box().height;
        let viewport = Size::default();
        let resolve = |dimension: Dimension| dimension.resolve(containing.height, viewport);
        let offsets = &child.style.offsets;
        // Where the child visually sits after plain scrolling.
        let visual = flow_top - offset;
        if let Some(top) = resolve(offsets.top) {
            let min_y = containing.y + top;
            let max_y = (containing.y + containing.height - height).max(min_y);
            visual.clamp(min_y, max_y) - visual
        } else if let Some(bottom) = resolve(offsets.bottom) {
            let max_y = containing.y + containing.height - bottom - height;
            let min_y = containing.y.min(max_y);
            visual.clamp(min_y, max_y) - visual
        } else {
            0.0
        }
    }

    /// How far this box's content can scroll: the extent of its children
    /// beyond its own content height.
    #[must_use]
    pub fn max_inner_scroll(&self) -> f32 {
        let content = self.content_box();
        let mut bottom = self
            .children
            .iter()
            .map(|child| child.margin_box().y + child.margin_box().height)
            .fold(content.y, f32::max);
        // Inline content (e.g. a textarea's text lines) scrolls too.
        if let LayoutKind::Inline { lines } = &self.kind {
            for line in lines {
                bottom = bottom.max(content.y + line.y + line.height);
            }
        }
        (bottom - content.y - content.height).max(0.0)
    }

    /// Depth-first search for the layout box of a DOM node (anonymous
    /// blocks share their container's node and are skipped).
    #[must_use]
    pub fn find_by_node(&self, node_id: NodeId) -> Option<&LayoutBox> {
        if self.node_id == node_id && self.box_type != BoxType::AnonymousBlock {
            return Some(self);
        }
        // Atomic inlines (inputs, inline-blocks, images) live inside line
        // fragments, not `children`.
        if let LayoutKind::Inline { lines } = &self.kind {
            for line in lines {
                for fragment in &line.fragments {
                    if let FragmentContent::Box(laid) = &fragment.content
                        && let Some(found) = laid.find_by_node(node_id)
                    {
                        return Some(found);
                    }
                }
            }
        }
        self.children
            .iter()
            .find_map(|child| child.find_by_node(node_id))
    }

    /// The deepest box under the point (page coordinates, CSS pixels),
    /// checking topmost paint order first.
    #[must_use]
    pub fn hit_test(&self, x: f32, y: f32) -> Option<NodeId> {
        self.hit_test_scrolled(x, y, &std::collections::HashMap::new())
    }

    /// [`Self::hit_test`] under per-element scroll offsets: points inside
    /// a scrolled box test its children at the scrolled position.
    #[must_use]
    pub fn hit_test_scrolled(
        &self,
        x: f32,
        y: f32,
        scroll_offsets: &std::collections::HashMap<NodeId, f32>,
    ) -> Option<NodeId> {
        self.hit_test_impl(x, y, 0.0, true, scroll_offsets)
    }

    /// Like [`Self::hit_test_scrolled`], but the point is in page
    /// coordinates (viewport point + page scroll) and `position: fixed`
    /// boxes are tested at their viewport position: the page scroll is
    /// subtracted back out at each fixed boundary.
    #[must_use]
    pub fn hit_test_page(
        &self,
        x: f32,
        y: f32,
        page_scroll: f32,
        scroll_offsets: &std::collections::HashMap<NodeId, f32>,
    ) -> Option<NodeId> {
        self.hit_test_impl(x, y, page_scroll, false, scroll_offsets)
    }

    fn hit_test_impl(
        &self,
        x: f32,
        y: f32,
        page_scroll: f32,
        in_fixed: bool,
        scroll_offsets: &std::collections::HashMap<NodeId, f32>,
    ) -> Option<NodeId> {
        let scroll_offset = match scroll_offsets.get(&self.node_id) {
            Some(offset) if self.style.clips_overflow() => *offset,
            _ => 0.0,
        };
        let (child_x, child_y) = (x, y + scroll_offset);
        // Sticky children sit at their stuck (visual) position: convert
        // the point into their flow coordinates per child. Fixed
        // children escape both the enclosing inner scroll and the page
        // scroll — their coordinates are viewport-relative.
        let child_point = |child: &LayoutBox| {
            if !in_fixed && child.style.position == Position::Fixed {
                (child_x, child_y - scroll_offset - page_scroll)
            } else {
                (
                    child_x,
                    child_y - self.sticky_child_shift(child, scroll_offset),
                )
            }
        };
        let hit_child = |child: &LayoutBox| {
            let (px, py) = child_point(child);
            let nested_fixed = in_fixed || child.style.position == Position::Fixed;
            child.hit_test_impl(px, py, page_scroll, nested_fixed, scroll_offsets)
        };
        // Fast path: with no z-index anywhere, paint order is DOM order,
        // so neither the collecting Vec nor the sort is needed.
        let hit = if self
            .children
            .iter()
            .all(|child| child.style.z_index.unwrap_or(0) == 0)
        {
            self.children.iter().rev().find_map(hit_child)
        } else {
            self.children_in_paint_order()
                .into_iter()
                .rev()
                .find_map(hit_child)
        };
        if let Some(hit) = hit {
            return Some(hit);
        }
        let rect = self.border_box();
        let inside =
            x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height;
        // `pointer-events: none`: the box itself (and its inline text
        // fragments) is never the hit target; children were still tested
        // above, and the hit falls through to whatever paints underneath.
        if !inside || self.style.pointer_events == PointerEvents::None {
            return None;
        }
        // Inside inline content, individual fragments are the hit targets,
        // so a link is hit only on its own words.
        if let LayoutKind::Inline { lines } = &self.kind {
            let content = self.content_box();
            for line in lines {
                for fragment in &line.fragments {
                    if let FragmentContent::Box(laid) = &fragment.content {
                        let (px, py) = if !in_fixed && laid.style.position == Position::Fixed {
                            (x, y - page_scroll)
                        } else {
                            (x, y)
                        };
                        if let Some(hit) = laid.hit_test_impl(
                            px,
                            py,
                            page_scroll,
                            in_fixed || laid.style.position == Position::Fixed,
                            scroll_offsets,
                        ) {
                            return Some(hit);
                        }
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

/// Memoized shrink-to-fit probes for a single layout pass.
///
/// [`natural_content_width`] re-lays a whole subtree to measure it, and
/// nested auto-width atomic boxes (inline-blocks, floats, table cells)
/// would otherwise redo the same probes exponentially often — O(2^depth).
/// A probe is a pure function of the node, the available width and the
/// depth (the depth guard can truncate deep subtrees, so the depth is
/// part of the key), so results are cached per `layout_document` call.
#[derive(Debug, Default)]
pub(crate) struct ProbeCache(
    std::cell::RefCell<std::collections::HashMap<(NodeId, u32, usize), f32>>,
);

impl ProbeCache {
    fn get(&self, node_id: NodeId, available: f32, depth: usize) -> Option<f32> {
        self.0
            .borrow()
            .get(&(node_id, available.to_bits(), depth))
            .copied()
    }

    fn insert(&self, node_id: NodeId, available: f32, depth: usize, width: f32) {
        self.0
            .borrow_mut()
            .insert((node_id, available.to_bits(), depth), width);
    }
}

/// The containing block for absolutely positioned descendants: the
/// content box of the nearest positioned ancestor (the viewport at the
/// root). Height is known only when that ancestor's height is explicit.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AbsoluteContext {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: Option<f32>,
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

    let probe_cache = ProbeCache::default();
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
            Some(viewport.height),
            Some(AbsoluteContext {
                x: 0.0,
                y: 0.0,
                width: viewport.width,
                height: Some(viewport.height),
            }),
            viewport,
            measurer,
            images,
            &probe_cache,
            0,
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
    containing_height: Option<f32>,
    absolute_context: Option<AbsoluteContext>,
    viewport: Size,
    measurer: &dyn TextMeasurer,
    images: &ImageMap,
    probe_cache: &ProbeCache,
    depth: usize,
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
            containing_height,
            absolute_context,
            viewport,
            measurer,
            images,
            probe_cache,
            depth,
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
            // Replaced elements flow in lines as atomic inlines unless
            // explicitly made block-level.
            if element.tag_name == "img" {
                return !matches!(style.display, Display::Block | Display::Flex);
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
        let Some(style) = styles.by_node.get(child) else {
            continue;
        };
        if style.display == Display::None {
            continue;
        }
        // Floats and out-of-flow boxes never collapse margins.
        if style.float != Float::None
            || matches!(style.position, Position::Absolute | Position::Fixed)
        {
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
        styles.by_node.get(&descendant).is_some_and(|style| {
            matches!(
                style.display,
                Display::Block | Display::Flex | Display::Grid
            )
        })
    })
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

/// Shrink-to-fit probe: the natural content width of an element laid out
/// at `available` width (the widest used extent of its children).
#[allow(clippy::too_many_arguments)]
pub(crate) fn natural_content_width(
    document: &Document,
    styles: &StyleMap,
    node_id: NodeId,
    available: f32,
    viewport: Size,
    measurer: &dyn TextMeasurer,
    images: &ImageMap,
    probe_cache: &ProbeCache,
    depth: usize,
) -> f32 {
    if let Some(cached) = probe_cache.get(node_id, available, depth) {
        return cached;
    }
    let NodeKind::Element(element) = &document.node(node_id).kind else {
        return 0.0;
    };
    let style = styles.by_node.get(&node_id).cloned().unwrap_or_default();
    let probe = layout_element(
        document,
        styles,
        node_id,
        element,
        style,
        0.0,
        &mut 0.0,
        available,
        None,
        None,
        viewport,
        measurer,
        images,
        probe_cache,
        depth,
    );
    // Measure the probe's children (the target's own padding/border sit
    // outside its content width and must not be double-counted).
    let content_x = probe.content_box().x;
    let width = (probe
        .children
        .iter()
        .map(natural_right)
        .fold(content_x, f32::max)
        - content_x)
        .min(available)
        .max(0.0);
    probe_cache.insert(node_id, available, depth, width);
    width
}

/// Lays out an element as an isolated box with an explicit style (used by
/// flex items to apply grow/stretch overrides).
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_isolated_with_style(
    document: &Document,
    styles: &StyleMap,
    node_id: NodeId,
    style: ComputedStyle,
    available: f32,
    viewport: Size,
    measurer: &dyn TextMeasurer,
    images: &ImageMap,
    probe_cache: &ProbeCache,
    depth: usize,
) -> LayoutBox {
    let NodeKind::Element(element) = &document.node(node_id).kind else {
        unreachable!("isolated boxes are always elements");
    };
    layout_element(
        document,
        styles,
        node_id,
        element,
        style,
        0.0,
        &mut 0.0,
        available,
        None,
        None,
        viewport,
        measurer,
        images,
        probe_cache,
        depth,
    )
}

/// Lays out an element as an isolated box (for inline-blocks and floats):
/// auto widths shrink to fit their content, capped by `available`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_atomic_box(
    document: &Document,
    styles: &StyleMap,
    node_id: NodeId,
    available: f32,
    viewport: Size,
    measurer: &dyn TextMeasurer,
    images: &ImageMap,
    probe_cache: &ProbeCache,
    depth: usize,
) -> LayoutBox {
    let NodeKind::Element(element) = &document.node(node_id).kind else {
        unreachable!("atomic boxes are always elements");
    };
    let mut style = styles.by_node.get(&node_id).cloned().unwrap_or_default();
    if matches!(style.width, Dimension::Auto) && element.tag_name != "img" {
        let natural = natural_content_width(
            document,
            styles,
            node_id,
            available,
            viewport,
            measurer,
            images,
            probe_cache,
            depth,
        );
        style.width = Dimension::Px(natural);
        style.box_sizing = BoxSizing::ContentBox;
    }
    layout_element(
        document,
        styles,
        node_id,
        element,
        style,
        0.0,
        &mut 0.0,
        available,
        None,
        None,
        viewport,
        measurer,
        images,
        probe_cache,
        depth,
    )
}

/// Resolves a specified size to its content-box value: border-box sizes
/// shrink by the given edge total (border + padding on that axis),
/// content-box sizes pass through. Negative specified sizes clamp to 0.
fn content_size(specified: f32, box_sizing: BoxSizing, edges: f32) -> f32 {
    match box_sizing {
        BoxSizing::ContentBox => specified.max(0.0),
        BoxSizing::BorderBox => (specified - edges).max(0.0),
    }
}

/// Clamps a used content-box size by min/max constraints (`Auto` =
/// unconstrained, min wins over max). Constraints name the same box as
/// `width`/`height`, so with border-box sizing they shrink by `edges`.
fn clamp_content_size(
    size: f32,
    min: Dimension,
    max: Dimension,
    containing: f32,
    viewport: Size,
    edges: f32,
    box_sizing: BoxSizing,
) -> f32 {
    let adjust = |value: f32| content_size(value, box_sizing, edges);
    let mut clamped = size;
    if let Some(max) = max.resolve(containing, viewport) {
        clamped = clamped.min(adjust(max));
    }
    if let Some(min) = min.resolve(containing, viewport) {
        clamped = clamped.max(adjust(min));
    }
    clamped
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
    containing_height: Option<f32>,
    absolute_context: Option<AbsoluteContext>,
    viewport: Size,
    measurer: &dyn TextMeasurer,
    images: &ImageMap,
    probe_cache: &ProbeCache,
    depth: usize,
) -> LayoutBox {
    // Depth guard: absurdly nested subtrees collapse to an empty box so
    // pathological documents cannot overflow the stack.
    if depth >= crate::MAX_DEPTH {
        return LayoutBox {
            node_id,
            box_type: BoxType::Block,
            kind: LayoutKind::Element(element.tag_name.clone()),
            dimensions: Dimensions::default(),
            style,
            children: Vec::new(),
        };
    }
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
        .map(|specified| {
            content_size(
                specified,
                style.box_sizing,
                border.left + border.right + padding.left + padding.right,
            )
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

    // min-/max-width clamp the used width (min wins over max). They name
    // the same box as `width`, so border-box sizes shrink by the edges.
    let horizontal_edges = border.left + border.right + padding.left + padding.right;
    let content_width = clamp_content_size(
        content_width,
        style.min_width,
        style.max_width,
        containing_width,
        viewport,
        horizontal_edges,
        style.box_sizing,
    );
    let width_constrained =
        content_width < (containing_width - margin.left - margin.right - horizontal_edges).max(0.0);

    // With an explicit or constrained width, auto margins absorb the
    // leftover space: both auto centers the box, one auto pushes it to the
    // other side.
    if !matches!(style.width, Dimension::Auto) || width_constrained {
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

    // The resolved explicit content height (when any) is the containing
    // height for percent-height children; percent heights resolve against
    // it, and `auto` heights leave children unresolved (treated as auto).
    let vertical_edges = border.top + border.bottom + padding.top + padding.bottom;
    let explicit_content_height: Option<f32> = match style.height {
        Dimension::Auto => None,
        Dimension::Percent(percent) => containing_height.map(|containing| {
            content_size(
                containing * percent / 100.0,
                style.box_sizing,
                vertical_edges,
            )
        }),
        explicit => explicit
            .resolve(containing_width, viewport)
            .map(|specified| content_size(specified, style.box_sizing, vertical_edges)),
    };

    // Positioned elements become the containing block for their
    // absolutely positioned descendants.
    let child_absolute_context = if style.position == Position::Static {
        absolute_context
    } else {
        Some(AbsoluteContext {
            x: content_x,
            y: content_y,
            width: content_width,
            height: explicit_content_height,
        })
    };

    // Phase 4: children. Consecutive inline-level children (text, inline
    // elements) form runs laid out into shared line boxes inside an
    // anonymous block; block-level children lay out as blocks.
    // Flex containers use their own child algorithm.
    if style.display == Display::Flex {
        let explicit_height = match style.height {
            Dimension::Auto | Dimension::Percent(_) => None,
            explicit => explicit
                .resolve(containing_width, viewport)
                .map(|specified| {
                    content_size(
                        specified,
                        style.box_sizing,
                        border.top + border.bottom + padding.top + padding.bottom,
                    )
                }),
        };
        let (children, used_height) = layout_flex_children(
            document,
            styles,
            node_id,
            &style,
            content_x,
            content_y,
            content_width,
            explicit_height,
            viewport,
            measurer,
            images,
            probe_cache,
            depth + 1,
        );
        let content_height = explicit_height.unwrap_or(used_height);
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
        return LayoutBox {
            node_id,
            box_type: BoxType::Block,
            kind: LayoutKind::Element(element.tag_name.clone()),
            dimensions,
            style,
            children,
        };
    }

    // Grid containers use the grid algorithm (see grid.rs).
    if style.display == Display::Grid {
        let (children, used_height) = crate::grid::layout_grid_children(
            document,
            styles,
            node_id,
            &style,
            content_x,
            content_y,
            content_width,
            explicit_content_height,
            viewport,
            measurer,
            images,
            probe_cache,
            depth + 1,
        );
        let content_height = explicit_content_height.unwrap_or(used_height);
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
        return LayoutBox {
            node_id,
            box_type: BoxType::Block,
            kind: LayoutKind::Element(element.tag_name.clone()),
            dimensions,
            style,
            children,
        };
    }

    // Tables use the table algorithm (see table.rs).
    if element.tag_name == "table" {
        let (children, used_height) = crate::table::layout_table_children(
            document,
            styles,
            node_id,
            &style,
            content_x,
            content_y,
            content_width,
            viewport,
            measurer,
            images,
            probe_cache,
            depth + 1,
        );
        // Shrink-to-fit: an auto-width table hugs its columns.
        let used_width = children
            .iter()
            .map(|child| child.margin_box().x + child.margin_box().width - content_x)
            .fold(0.0f32, f32::max)
            + style.gap.max(2.0);
        let table_width = if matches!(style.width, Dimension::Auto) {
            used_width.min(content_width)
        } else {
            content_width
        };
        let content_height = explicit_content_height.unwrap_or(used_height);
        let dimensions = Dimensions {
            content: Rect {
                x: content_x,
                y: content_y,
                width: table_width,
                height: content_height,
            },
            padding,
            border,
            margin,
        };
        *cursor_y = dimensions.margin_box().y + dimensions.margin_box().height;
        return LayoutBox {
            node_id,
            box_type: BoxType::Block,
            kind: LayoutKind::Element(element.tag_name.clone()),
            dimensions,
            style,
            children,
        };
    }

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
        probe_cache: &ProbeCache,
        children: &mut Vec<LayoutBox>,
        depth: usize,
    ) {
        if run.is_empty() {
            return;
        }
        let run_top = *cursor_y;
        let bounds = |line_top: f32| floats.bounds_at(content_x, content_width, run_top + line_top);
        let mut layout_atomic = |node_id: NodeId, available: f32| {
            layout_atomic_box(
                document,
                styles,
                node_id,
                available,
                viewport,
                measurer,
                images,
                probe_cache,
                depth,
            )
        };
        let (lines, height) = layout_inline_run(
            document,
            styles,
            run,
            style,
            Some(owner),
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

        // Absolutely positioned children leave the flow entirely. The
        // containing block is the content box of the nearest positioned
        // ancestor (fixed uses the viewport); `bottom` resolves whenever
        // the containing height is known.
        if matches!(child_style.position, Position::Absolute | Position::Fixed)
            && matches!(&document.node(*child).kind, NodeKind::Element(_))
        {
            let (cb_x, cb_y, cb_width, cb_height) = if child_style.position == Position::Fixed {
                (0.0, 0.0, viewport.width, viewport.height)
            } else {
                match child_absolute_context {
                    Some(context) => (
                        context.x,
                        context.y,
                        context.width,
                        context.height.unwrap_or(f32::NAN),
                    ),
                    None => (content_x, content_y, content_width, f32::NAN),
                }
            };
            let laid = layout_atomic_box(
                document,
                styles,
                *child,
                cb_width,
                viewport,
                measurer,
                images,
                probe_cache,
                depth + 1,
            );
            let margin_box = laid.margin_box();
            let offsets = &child_style.offsets;
            let x = if let Some(left) = offsets.left.resolve(cb_width, viewport) {
                cb_x + left
            } else if let Some(right) = offsets.right.resolve(cb_width, viewport) {
                cb_x + cb_width - margin_box.width - right
            } else {
                content_x // static-position fallback
            };
            // Vertical offsets resolve against the containing block
            // *height*; a percent against an unknown height behaves as
            // `auto` (falls through to `bottom`, then the static spot).
            let vertical = |dimension: Dimension| match dimension {
                Dimension::Percent(_) if cb_height.is_nan() => None,
                other => other.resolve(cb_height, viewport),
            };
            let y = if let Some(top) = vertical(offsets.top) {
                cb_y + top
            } else if !cb_height.is_nan()
                && let Some(bottom) = vertical(offsets.bottom)
            {
                cb_y + cb_height - margin_box.height - bottom
            } else {
                child_cursor_y // static-position fallback
            };
            let mut laid = laid;
            laid.translate(x - margin_box.x, y - margin_box.y);
            children.push(laid);
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
                probe_cache,
                depth + 1,
            );
            let margin_box = laid.margin_box();
            let (width, height) = (margin_box.width, margin_box.height);
            // Find the highest y at or below the cursor where the float
            // fits. The probe is bounded so pathological float stacks
            // cannot loop forever; real pages need only a few steps.
            const FLOAT_PLACEMENT_ATTEMPTS: usize = 64;
            let mut spot = None;
            let mut y = child_cursor_y;
            for _ in 0..FLOAT_PLACEMENT_ATTEMPTS {
                let (indent, available) = floats.bounds_at(content_x, content_width, y);
                if width <= available || available >= content_width {
                    let x = match side {
                        Float::Right => content_x + indent + available - width,
                        _ => content_x + indent,
                    };
                    spot = Some((x, y));
                    break;
                }
                y = floats.lowest_bottom().max(y + 1.0);
            }
            // Out of probe budget: stack below the lowest float rather
            // than dropping the box entirely.
            let (x, y) = spot.unwrap_or_else(|| {
                let y = floats.lowest_bottom().max(child_cursor_y);
                let x = match side {
                    Float::Right => (content_x + content_width - width).max(content_x),
                    _ => content_x,
                };
                (x, y)
            });
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
                probe_cache,
                &mut children,
                depth + 1,
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
                explicit_content_height,
                child_absolute_context,
                viewport,
                measurer,
                images,
                probe_cache,
                depth + 1,
            ) {
                previous_bottom_margin = Some(layout.dimensions.margin.bottom);
                children.push(layout);
            }
        }
    }
    let blocks_before_trailing_run = children.len();
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
        probe_cache,
        &mut children,
        depth + 1,
    );
    let trailing_inline = children.len() > blocks_before_trailing_run;

    // Bottom parent-child collapsing: with an auto height and no bottom
    // border/padding, the last block child's bottom margin escapes the
    // parent and collapses with the parent's own bottom margin.
    if !trailing_inline
        && matches!(style.height, Dimension::Auto)
        && border.bottom == 0.0
        && padding.bottom == 0.0
        && style.overflow_x == Overflow::Visible
        && style.overflow_y == Overflow::Visible
        && let Some(last_bottom) = previous_bottom_margin
    {
        child_cursor_y -= last_bottom;
        margin.bottom = collapsed_margin(margin.bottom, last_bottom);
    }

    // A container's auto height contains its floats (BFC-root behavior).
    child_cursor_y = child_cursor_y.max(floats.lowest_bottom());

    // Phase 5: height. Explicit heights win (border-box heights shrink by
    // vertical padding and border); auto grows from the children. Percent
    // heights are unsupported and treated as auto; vh works.
    let content_height = explicit_content_height.unwrap_or_else(|| {
        // `aspect-ratio` derives an auto height from the used width.
        match style.aspect_ratio {
            Some(ratio) if ratio > 0.0 => content_width / ratio,
            _ => (child_cursor_y - content_y).max(0.0),
        }
    });
    // min-/max-height clamp like widths; percent constraints resolve
    // against the containing height when it is known, else are ignored.
    let ignore_percent = |dimension: Dimension| match dimension {
        Dimension::Percent(percent) => match containing_height {
            Some(containing) => Dimension::Px(containing * percent / 100.0),
            None => Dimension::Auto,
        },
        other => other,
    };
    let content_height = clamp_content_size(
        content_height,
        ignore_percent(style.min_height),
        ignore_percent(style.max_height),
        containing_width,
        viewport,
        border.top + border.bottom + padding.top + padding.bottom,
        style.box_sizing,
    );

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

    let mut laid = LayoutBox {
        node_id,
        box_type: match style.display {
            Display::Inline => BoxType::Inline,
            _ => BoxType::Block,
        },
        kind: LayoutKind::Element(element.tag_name.clone()),
        dimensions,
        style,
        children,
    };
    // Empty blocks collapse through themselves: an edgeless, contentless,
    // auto-height block joins its top and bottom margins into one
    // (carried on the bottom edge; the box itself is invisible anyway).
    if laid.children.is_empty()
        && !matches!(&laid.kind, LayoutKind::Inline { .. })
        && laid.dimensions.content.height == 0.0
        && vertical_edges == 0.0
        && matches!(laid.style.height, Dimension::Auto)
        && laid.style.position == Position::Static
    {
        let old_top = laid.dimensions.margin.top;
        let collapsed = collapsed_margin(old_top, laid.dimensions.margin.bottom);
        laid.dimensions.margin.top = 0.0;
        laid.dimensions.margin.bottom = collapsed;
        laid.translate(0.0, -old_top);
        *cursor_y = laid.dimensions.margin_box().y + laid.dimensions.margin_box().height;
    }

    // position: relative — a pure visual offset; flow space is unchanged
    // (the cursor above already advanced from the unshifted box).
    if laid.style.position == Position::Relative {
        let offsets = &laid.style.offsets;
        let dx = offsets
            .left
            .resolve(containing_width, viewport)
            .or_else(|| {
                offsets
                    .right
                    .resolve(containing_width, viewport)
                    .map(|right| -right)
            })
            .unwrap_or(0.0);
        let dy = offsets
            .top
            .resolve(containing_width, viewport)
            .or_else(|| {
                offsets
                    .bottom
                    .resolve(containing_width, viewport)
                    .map(|bottom| -bottom)
            })
            .unwrap_or(0.0);
        if dx != 0.0 || dy != 0.0 {
            laid.translate(dx, dy);
        }
    }
    laid
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

    /// First laid-out box with this tag, depth-first. html5ever wraps
    /// every document in html/head/body, so tests locate boxes by tag
    /// instead of fixed child indices from the root.
    fn find_box<'a>(layout: &'a LayoutBox, tag: &str) -> &'a LayoutBox {
        fn find<'a>(layout: &'a LayoutBox, tag: &str) -> Option<&'a LayoutBox> {
            if matches!(&layout.kind, LayoutKind::Element(t) if t == tag) {
                return Some(layout);
            }
            layout.children.iter().find_map(|child| find(child, tag))
        }
        find(layout, tag).unwrap_or_else(|| panic!("no laid-out box for <{tag}>"))
    }

    /// The old "top-level" boxes are now the body box's children, in
    /// the same order (head/style/title produce no boxes).
    fn body_box(layout: &LayoutBox) -> &LayoutBox {
        find_box(layout, "body")
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
        let parent = &body_box(&layout).children[0];
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
        let div = &body_box(&layout).children[0];
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
        let div = &body_box(&layout).children[0];
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
        let outer = &body_box(&layout).children[0];
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
        assert_eq!(body_box(&auto).children[0].border_box().height, 40.0);

        let fixed = layout_of(
            "<style>.parent { height: 25px; } .child { height: 40px; }</style>\
             <div class='parent'><div class='child'></div></div>",
        );
        assert_eq!(body_box(&fixed).children[0].border_box().height, 25.0);
    }

    #[test]
    fn auto_margins_center_a_fixed_width_box() {
        let layout = layout_of(
            "<style>div { width: 100px; height: 10px; margin: 0 auto; }</style><div></div>",
        );
        let div = &body_box(&layout).children[0];
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
        let div = &body_box(&layout).children[0];
        assert_eq!(div.border_box().x, 700.0);
    }

    #[test]
    fn em_lengths_resolve_against_font_size() {
        let layout = layout_of(
            "<style>div { font-size: 20px; margin-left: 2em; padding-top: 1.5em; height: 10px; }\
             </style><div></div>",
        );
        let div = &body_box(&layout).children[0];
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
        let anonymous = &body_box(&layout).children[0].children[0];
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
        let LayoutKind::Inline { lines } = &body_box(&layout).children[0].children[0].kind else {
            panic!("expected inline content");
        };
        // No space between fragments: "foo" ends at 24, "bar" starts at 24.
        assert_eq!(lines[0].fragments[1].x, 24.0);
    }

    #[test]
    fn br_forces_a_line_break() {
        let layout = layout_of("<div>a<br>b</div>");
        let LayoutKind::Inline { lines } = &body_box(&layout).children[0].children[0].kind else {
            panic!("expected inline content");
        };
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].fragments[0].text(), Some("a"));
        assert_eq!(lines[1].fragments[0].text(), Some("b"));
    }

    #[test]
    fn mixed_block_and_inline_children_get_anonymous_blocks() {
        let layout = layout_of("<div>before<p>block</p>after</div>");
        let div = &body_box(&layout).children[0];
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
        let LayoutKind::Inline { lines } = &body_box(&layout).children[0].children[0].kind else {
            panic!("expected inline content");
        };
        // Default 16px text line-height is 22.4; the span's is 32. The
        // baseline sits at the 32px span's ascent (0.8 × 32).
        assert_eq!(lines[0].height, 32.0);
        assert!((lines[0].baseline - 25.6).abs() < 0.01);
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
        // The imgs are inline content, so the body wraps them in one
        // anonymous block whose line fragments carry the laid boxes.
        let LayoutKind::Inline { lines } = &body_box(&layout).children[0].kind else {
            panic!("expected inline content");
        };
        let boxes: Vec<&LayoutBox> = lines
            .iter()
            .flat_map(|line| &line.fragments)
            .filter_map(|fragment| match &fragment.content {
                FragmentContent::Box(laid) => Some(laid.as_ref()),
                _ => None,
            })
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
        let div = &body_box(&layout).children[0];
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
        let div = &body_box(&layout).children[0];
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
        let div = &body_box(&layout).children[0];
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
        let container = &body_box(&layout).children[0];
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
        let container = &body_box(&layout).children[0];
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
        let parent = &body_box(&layout).children[0];
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
        let parent = &body_box(&layout).children[0];
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
        let anonymous = &body_box(&layout).children[0].children[0];
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
        // Bottom sits on the baseline: the chip's 20px height dominates
        // the ascent, text descent (3.2) hangs below, so the line is
        // 23.2 tall with the baseline at 20 — the chip tops out at 0.
        assert!(chip_box.border_box().y.abs() < 0.01);
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
        let LayoutKind::Inline { lines } = &body_box(&layout).children[0].children[0].kind else {
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
        let wrap = &body_box(&layout).children[0];
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
        let float_box = &body_box(&layout).children[0].children[0];
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
        let cleared = &body_box(&layout).children[0].children[1];
        assert_eq!(cleared.border_box().y, 40.0);
    }

    #[test]
    fn flex_row_places_items_side_by_side_with_gap() {
        let layout = layout_of(
            "<style>
                .row { display: flex; gap: 10px; }
                .item { width: 100px; height: 20px; }
             </style>\
             <div class='row'><div class='item'></div><div class='item'></div>\
             <div class='item'></div></div>",
        );
        let row = &body_box(&layout).children[0];
        let xs: Vec<f32> = row
            .children
            .iter()
            .map(|child| child.border_box().x)
            .collect();
        assert_eq!(xs, vec![0.0, 110.0, 220.0]);
        assert_eq!(row.border_box().height, 20.0);
    }

    #[test]
    fn flex_grow_distributes_free_space() {
        let layout = layout_of(
            "<style>
                .row { display: flex; }
                .a { width: 100px; height: 10px; }
                .b { flex-grow: 1; height: 10px; }
                .c { flex-grow: 3; height: 10px; }
             </style>\
             <div class='row'><div class='a'></div><div class='b'></div>\
             <div class='c'></div></div>",
        );
        let row = &body_box(&layout).children[0];
        // Free space 700 split 1:3 → 175 and 525.
        assert_eq!(row.children[1].border_box().width, 175.0);
        assert_eq!(row.children[2].border_box().width, 525.0);
        assert_eq!(row.children[2].border_box().x, 275.0);
    }

    #[test]
    fn flex_wrap_flows_items_onto_new_lines() {
        let layout = layout_of(
            "<style>
                .row { display: flex; flex-wrap: wrap; gap: 10px; }
                .item { width: 300px; height: 20px; }
             </style>\
             <div class='row'><div class='item'></div><div class='item'></div>\
             <div class='item'></div></div>",
        );
        let row = &body_box(&layout).children[0];
        // 300+10+300 = 610 fits in 800; the third item (needs 920) wraps.
        assert_eq!(
            row.children[0].border_box().y,
            row.children[1].border_box().y
        );
        assert_eq!(row.children[2].border_box().x, 0.0);
        assert_eq!(row.children[2].border_box().y, 30.0);
        // Container height covers both lines.
        assert_eq!(row.content_box().height, 50.0);
    }

    #[test]
    fn flex_shrink_narrows_overflowing_items() {
        let layout = layout_of(
            "<style>
                .row { display: flex; }
                .a { width: 600px; height: 10px; }
                .b { width: 600px; height: 10px; flex-shrink: 2; }
             </style>\
             <div class='row'><div class='a'></div><div class='b'></div></div>",
        );
        let row = &body_box(&layout).children[0];
        // Overflow 400 split by shrink*base 600 : 1200 → 133.3 and 266.7.
        let a = row.children[0].border_box().width;
        let b = row.children[1].border_box().width;
        assert!((a - 466.7).abs() < 0.5, "a = {a}");
        assert!((b - 333.3).abs() < 0.5, "b = {b}");
        assert!((a + b - 800.0).abs() < 0.5);
    }

    #[test]
    fn align_self_overrides_align_items() {
        let layout = layout_of(
            "<style>
                .row { display: flex; align-items: flex-start; height: 100px; }
                .a { width: 10px; height: 20px; }
                .b { width: 10px; height: 20px; align-self: flex-end; }
                .c { width: 10px; align-self: stretch; }
             </style>\
             <div class='row'><div class='a'></div><div class='b'></div>\
             <div class='c'></div></div>",
        );
        let row = &body_box(&layout).children[0];
        assert_eq!(row.children[0].border_box().y, 0.0);
        assert_eq!(row.children[1].border_box().y, 80.0);
        assert_eq!(row.children[2].border_box().height, 100.0);
    }

    #[test]
    fn justify_content_positions_the_line() {
        for (justify, expected_x) in [
            ("center", 300.0),
            ("flex-end", 600.0),
            ("space-between", 0.0),
        ] {
            let layout = layout_of(&format!(
                "<style>
                    .row {{ display: flex; justify-content: {justify}; }}
                    .item {{ width: 100px; height: 10px; }}
                 </style>\
                 <div class='row'><div class='item'></div><div class='item'></div></div>"
            ));
            let row = &body_box(&layout).children[0];
            assert_eq!(row.children[0].border_box().x, expected_x, "{justify}");
            if justify == "space-between" {
                assert_eq!(row.children[1].border_box().x, 700.0);
            }
        }
    }

    #[test]
    fn align_items_positions_and_stretches_the_cross_axis() {
        let layout = layout_of(
            "<style>
                .row { display: flex; height: 100px; align-items: center; }
                .item { width: 50px; height: 40px; }
             </style><div class='row'><div class='item'></div></div>",
        );
        assert_eq!(
            body_box(&layout).children[0].children[0].border_box().y,
            30.0
        );

        let stretch = layout_of(
            "<style>
                .row { display: flex; height: 100px; }
                .item { width: 50px; }
             </style><div class='row'><div class='item'></div></div>",
        );
        assert_eq!(
            body_box(&stretch).children[0].children[0]
                .border_box()
                .height,
            100.0
        );
    }

    #[test]
    fn flex_column_stacks_with_gap_and_grow() {
        let layout = layout_of(
            "<style>
                .col { display: flex; flex-direction: column; height: 200px; gap: 10px; }
                .a { height: 50px; }
                .b { flex-grow: 1; }
             </style><div class='col'><div class='a'></div><div class='b'></div></div>",
        );
        let col = &body_box(&layout).children[0];
        assert_eq!(col.children[0].border_box().y, 0.0);
        assert_eq!(col.children[1].border_box().y, 60.0);
        // 200 - 50 - 10 gap = 140 for the growing item.
        assert_eq!(col.children[1].border_box().height, 140.0);
        // Column items stretch to the full width by default.
        assert_eq!(col.children[1].border_box().width, 800.0);
    }

    #[test]
    fn flex_text_children_become_anonymous_items() {
        let layout = layout_of(
            "<style>.row { display: flex; gap: 8px; }</style>\
             <div class='row'>label<div style='width: 40px; height: 10px'></div></div>",
        );
        let row = &body_box(&layout).children[0];
        assert_eq!(row.children.len(), 2);
        // "label" = 5 chars * 8px wide anonymous item, then the div after the gap.
        assert_eq!(row.children[1].border_box().x, 48.0);
    }

    #[test]
    fn relative_position_offsets_without_affecting_flow() {
        let layout = layout_of(
            "<style>
                .rel { position: relative; top: 5px; left: 10px; height: 20px; }
                .after { height: 10px; }
             </style><div class='rel'></div><div class='after'></div>",
        );
        let rel = &body_box(&layout).children[0];
        let after = &body_box(&layout).children[1];
        assert_eq!(rel.border_box().x, 10.0);
        assert_eq!(rel.border_box().y, 5.0);
        // The sibling flows as if the box had not moved.
        assert_eq!(after.border_box().y, 20.0);
    }

    #[test]
    fn absolute_position_leaves_the_flow() {
        let layout = layout_of(
            "<style>
                .wrap { padding: 10px; position: relative; }
                .abs { position: absolute; top: 4px; left: 6px;
                       width: 30px; height: 8px; }
                .flow { height: 20px; }
             </style><div class='wrap'><div class='abs'></div><div class='flow'></div></div>",
        );
        let wrap = &body_box(&layout).children[0];
        let abs = &wrap.children[0];
        let flow = &wrap.children[1];
        // Positioned against the parent's content box (10,10).
        assert_eq!(abs.border_box().x, 16.0);
        assert_eq!(abs.border_box().y, 14.0);
        // The in-flow sibling starts at the content top; the parent's
        // height ignores the absolute child.
        assert_eq!(flow.border_box().y, 10.0);
        assert_eq!(wrap.border_box().height, 40.0);
    }

    #[test]
    fn absolute_right_aligns_to_the_containing_edge() {
        let layout = layout_of(
            "<style>.abs { position: absolute; right: 20px; top: 0;
                           width: 50px; height: 5px; }</style>\
             <div><div class='abs'></div></div>",
        );
        let abs = &body_box(&layout).children[0].children[0];
        assert_eq!(abs.border_box().x, 800.0 - 50.0 - 20.0);
    }

    #[test]
    fn absolute_uses_the_nearest_positioned_ancestor() {
        let layout = layout_of(
            "<style>
                .anchor { position: relative; margin-left: 100px; width: 300px;
                          height: 120px; }
                .middle { padding: 20px; }
                .abs { position: absolute; top: 10px; left: 10px;
                       width: 30px; height: 10px; }
                .btm { position: absolute; bottom: 10px; left: 0;
                       width: 30px; height: 10px; }
             </style>\
             <div class='anchor'><div class='middle'>\
             <div class='abs'></div><div class='btm'></div></div></div>",
        );
        let abs = &body_box(&layout).children[0].children[0].children[0];
        // Against the .anchor content box (x=100), not .middle's (x=120).
        assert_eq!(abs.border_box().x, 110.0);
        assert_eq!(abs.border_box().y, 10.0);
        // Absolute bottom now resolves against the explicit 120px height.
        let btm = &body_box(&layout).children[0].children[0].children[1];
        assert_eq!(btm.border_box().y, 120.0 - 10.0 - 10.0);
    }

    #[test]
    fn unpositioned_pages_use_the_viewport_as_containing_block() {
        let layout = layout_of(
            "<style>.abs { position: absolute; bottom: 0; right: 0;
                           width: 50px; height: 20px; }</style>\
             <div><div class='abs'></div></div>",
        );
        let abs = &body_box(&layout).children[0].children[0];
        assert_eq!(abs.border_box().x, 800.0 - 50.0);
        assert_eq!(abs.border_box().y, 600.0 - 20.0);
    }

    #[test]
    fn bottom_margins_collapse_out_of_edgeless_parents() {
        let layout = layout_of(
            "<style>.wrap { margin-bottom: 10px; } .inner { margin-bottom: 30px; height: 5px; }\
             </style><main><div class='wrap'><div class='inner'></div></div><p>after</p></main>",
        );
        let wrap = &body_box(&layout).children[0].children[0];
        // The parent's content stops at the child's border box...
        assert_eq!(wrap.content_box().height, 5.0);
        // ...and the collapsed 30px margin sits on the parent.
        assert_eq!(wrap.dimensions.margin.bottom, 30.0);
        // The following paragraph starts after exactly one collapsed margin.
        assert_eq!(
            body_box(&layout).children[0].children[1].border_box().y,
            35.0
        );
    }

    #[test]
    fn empty_blocks_collapse_through() {
        let layout = layout_of(
            "<style>.gap { margin-top: 20px; margin-bottom: 30px; }\
                    .after { height: 5px; }</style>\
             <main><div class='gap'></div><div class='after'></div></main>",
        );
        // One collapsed margin (30), not 20 + 30.
        assert_eq!(
            body_box(&layout).children[0].children[1].border_box().y,
            30.0
        );
    }

    #[test]
    fn fixed_positions_against_the_viewport() {
        let layout = layout_of(
            "<style>
                .wrap { padding: 50px; }
                .fix { position: fixed; bottom: 10px; right: 5px;
                       width: 40px; height: 20px; }
             </style><div class='wrap'><div class='fix'></div></div>",
        );
        let fix = &body_box(&layout).children[0].children[0];
        assert_eq!(fix.border_box().x, 800.0 - 40.0 - 5.0);
        assert_eq!(fix.border_box().y, 600.0 - 20.0 - 10.0);
    }

    #[test]
    fn z_index_orders_painting_and_hit_testing() {
        let document = parse_document(
            "<style>
                .a, .b { position: absolute; top: 0; left: 0; margin: 0;
                         width: 50px; height: 50px; }
                .a { z-index: 2; }
                .b { z-index: 1; }
             </style><div><p class='a'>top</p><p class='b'>under</p></div>",
        );
        let author = lumen_css::parse_stylesheet(&crate::extract_embedded_css(&document));
        let styles = compute_styles(&document, &author);
        let layout = layout_document(
            &document,
            &styles,
            VIEWPORT,
            &crate::text::HeuristicMeasurer,
            &crate::image::ImageMap::new(),
        );
        let container = &body_box(&layout).children[0];
        let ordered = container.children_in_paint_order();
        // .b (z=1) paints before .a (z=2) despite DOM order.
        let z_of = |layout: &LayoutBox| layout.style.z_index.unwrap_or(0);
        assert_eq!(z_of(ordered[0]), 1);
        assert_eq!(z_of(ordered[1]), 2);
        // Hit test at the overlap returns the higher z (text of .a).
        let hit = layout.hit_test(5.0, 5.0).unwrap();
        let hit_in_a = std::iter::once(hit)
            .chain(document.ancestors(hit))
            .any(|id| {
                document
                    .element(id)
                    .is_some_and(|element| element.has_class("a"))
            });
        assert!(hit_in_a);
    }

    #[test]
    fn pointer_events_none_falls_through_to_what_is_underneath() {
        let document = parse_document(
            "<style>
                .overlay, .under { position: absolute; top: 0; left: 0; margin: 0;
                                   width: 100px; height: 100px; }
                .overlay { pointer-events: none; z-index: 2; }
                .under { z-index: 1; }
             </style><div><div class='under'>under</div><div class='overlay'>cover</div></div>",
        );
        let author = lumen_css::parse_stylesheet(&crate::extract_embedded_css(&document));
        let styles = compute_styles(&document, &author);
        let layout = layout_document(
            &document,
            &styles,
            VIEWPORT,
            &crate::text::HeuristicMeasurer,
            &crate::image::ImageMap::new(),
        );
        // The overlay paints on top (z-index 2) but is pointer-transparent:
        // the hit lands on .under's text.
        let hit = layout.hit_test(5.0, 5.0).expect("hit through the overlay");
        let tag = |id: NodeId| {
            document
                .element(id)
                .map(|element| element.tag_name.as_str())
        };
        let hit_in_under = std::iter::once(hit)
            .chain(document.ancestors(hit))
            .any(|id| {
                document
                    .element(id)
                    .is_some_and(|element| element.has_class("under"))
            });
        assert!(hit_in_under, "hit node {hit:?} (tag {:?})", tag(hit));
        let hit_in_overlay = std::iter::once(hit)
            .chain(document.ancestors(hit))
            .any(|id| {
                document
                    .element(id)
                    .is_some_and(|element| element.has_class("overlay"))
            });
        assert!(!hit_in_overlay);
        // Without the declaration the overlay wins the same point.
        let document = parse_document(
            "<style>
                .overlay, .under { position: absolute; top: 0; left: 0; margin: 0;
                                   width: 100px; height: 100px; }
                .overlay { z-index: 2; }
                .under { z-index: 1; }
             </style><div><div class='under'>under</div><div class='overlay'>cover</div></div>",
        );
        let author = lumen_css::parse_stylesheet(&crate::extract_embedded_css(&document));
        let styles = compute_styles(&document, &author);
        let layout = layout_document(
            &document,
            &styles,
            VIEWPORT,
            &crate::text::HeuristicMeasurer,
            &crate::image::ImageMap::new(),
        );
        let covered = layout.hit_test(5.0, 5.0).expect("hit on the overlay");
        let covered_in_overlay = std::iter::once(covered)
            .chain(document.ancestors(covered))
            .any(|id| {
                document
                    .element(id)
                    .is_some_and(|element| element.has_class("overlay"))
            });
        assert!(covered_in_overlay);
    }

    #[test]
    fn text_transform_and_indent_shape_the_lines() {
        let layout = layout_of(
            "<style>p { text-transform: uppercase; text-indent: 40px; }</style>\
             <p>hello world</p>",
        );
        let LayoutKind::Inline { lines } = &body_box(&layout).children[0].children[0].kind else {
            panic!("expected inline content");
        };
        assert_eq!(lines[0].fragments[0].text(), Some("HELLO WORLD"));
        // First line indented by 40px.
        assert_eq!(lines[0].fragments[0].x, 40.0);
    }

    #[test]
    fn ellipsis_truncates_the_overflowing_line() {
        let layout = layout_of(
            "<style>div { width: 80px; white-space: nowrap; overflow: hidden; \
                          text-overflow: ellipsis; }</style>\
             <div>a very long sentence that cannot fit</div>",
        );
        let LayoutKind::Inline { lines } = &body_box(&layout).children[0].children[0].kind else {
            panic!("expected inline content");
        };
        assert_eq!(lines.len(), 1);
        let text = lines[0].fragments[0].text().unwrap();
        assert!(text.ends_with('…'), "{text}");
        // The kept text fits the 80px box (10 chars at 8px each).
        assert!(lines[0].fragments[0].width <= 80.0);
    }

    #[test]
    fn break_all_splits_over_wide_words() {
        let layout = layout_of(
            "<style>div { width: 40px; word-break: break-all; }</style>\
             <div>abcdefghijklmnop</div>",
        );
        let LayoutKind::Inline { lines } = &body_box(&layout).children[0].children[0].kind else {
            panic!("expected inline content");
        };
        // 16 chars at 8px = 128px over a 40px box → 4 lines of 5 chars.
        assert!(lines.len() >= 3, "got {} lines", lines.len());
        for line in lines {
            assert!(line.fragments[0].width <= 40.0 + 8.0);
        }
    }

    #[test]
    fn justify_stretches_wrapped_lines_only() {
        let layout = layout_of(
            "<style>div { width: 200px; text-align: justify; }</style>\
             <div>aaaa bbbb cccc dddd eeee ffff gggg hhhh last</div>",
        );
        let LayoutKind::Inline { lines } = &body_box(&layout).children[0].children[0].kind else {
            panic!("expected inline content");
        };
        assert!(lines.len() >= 2);
        // A wrapped line ends flush with the right edge.
        let first = &lines[0];
        let last_fragment = first.fragments.last().unwrap();
        assert!(
            (last_fragment.x + last_fragment.width - 200.0).abs() < 0.5,
            "wrapped line should fill the width"
        );
        // The final line stays left-aligned (does not fill).
        let final_line = lines.last().unwrap();
        let end = final_line
            .fragments
            .last()
            .map(|fragment| fragment.x + fragment.width)
            .unwrap();
        assert!(end < 199.0, "final line must not stretch: {end}");
    }

    #[test]
    fn vertical_align_shifts_atomic_inlines_and_text() {
        let layout = layout_of(
            "<style>.line { line-height: 60px; }\
                    .box { display: inline-block; width: 10px; height: 20px; }\
                    .top { vertical-align: top; }\
                    .mid { vertical-align: middle; }</style>\
             <p class='line'>x <span class='box top'></span>\
             <span class='box mid'></span> <sup>up</sup></p>",
        );
        let LayoutKind::Inline { lines } = &body_box(&layout).children[0].children[0].kind else {
            panic!("expected inline content");
        };
        let line = &lines[0];
        let boxes: Vec<&crate::layout::LayoutBox> = line
            .fragments
            .iter()
            .filter_map(|fragment| match &fragment.content {
                FragmentContent::Box(laid) => Some(laid.as_ref()),
                _ => None,
            })
            .collect();
        let line_top = body_box(&layout).children[0].children[0].content_box().y + line.y;
        // top-aligned box sits at the line top.
        assert_eq!(boxes[0].margin_box().y, line_top);
        // middle-aligned box is centered in the 60px line.
        assert_eq!(boxes[1].margin_box().y, line_top + 20.0);
        // The sup text fragment carries a negative baseline shift.
        let sup = line
            .fragments
            .iter()
            .find(|fragment| fragment.text() == Some("up"))
            .unwrap();
        assert!(sup.dy < 0.0);
    }

    #[test]
    fn nowrap_keeps_text_on_one_line() {
        let layout = layout_of(
            "<style>div { width: 60px; white-space: nowrap; }</style>\
             <div>many words that would surely wrap</div>",
        );
        let LayoutKind::Inline { lines } = &body_box(&layout).children[0].children[0].kind else {
            panic!("expected inline content");
        };
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn pre_preserves_spaces_and_newlines_without_wrapping() {
        let layout = layout_of(
            "<pre>a  b\nverylongline that would normally wrap far beyond any width limit set here</pre>",
        );
        let pre = &body_box(&layout).children[0];
        let LayoutKind::Inline { lines } = &pre.children[0].kind else {
            panic!("expected inline content in pre");
        };
        assert_eq!(lines.len(), 2);
        // Double space preserved: 4 chars at 0.6em (mono heuristic).
        assert_eq!(lines[0].fragments[0].text(), Some("a  b"));
        assert_eq!(lines[0].fragments[0].width, 4.0 * 16.0 * 0.6);
        // The long line stays a single fragment (no wrapping).
        assert_eq!(lines[1].fragments.len(), 1);
    }

    #[test]
    fn percent_heights_resolve_against_explicit_parents() {
        let layout = layout_of(
            "<style>.outer { height: 200px; } .half { height: 50%; }\
                    .auto-parent { } .orphan { height: 50%; }</style>\
             <div class='outer'><div class='half'></div></div>\
             <div class='auto-parent'><div class='orphan'></div></div>",
        );
        // 50% of the explicit 200px parent.
        assert_eq!(
            body_box(&layout).children[0].children[0]
                .content_box()
                .height,
            100.0
        );
        // Percent inside an auto parent stays auto (0 here).
        assert_eq!(
            body_box(&layout).children[1].children[0]
                .content_box()
                .height,
            0.0
        );
    }

    #[test]
    fn viewport_is_the_root_containing_height() {
        // The html5ever skeleton puts <html>/<body> between the viewport
        // and the content; their height is auto by default, so a bare
        // percentage height collapses to auto (spec behavior — the old
        // expectation only held when the div was a direct child of the
        // viewport-height root).
        let layout = layout_of("<style>div { height: 50%; }</style><div></div>");
        assert_eq!(body_box(&layout).children[0].content_box().height, 0.0);
        // With html/body pinned to 100%, the viewport height propagates
        // through and 50% resolves to 300px of the 600px viewport.
        let layout = layout_of(
            "<style>html, body { height: 100%; } div { height: 50%; }</style><div></div>",
        );
        assert_eq!(body_box(&layout).children[0].content_box().height, 300.0);
    }

    #[test]
    fn aspect_ratio_derives_height_from_width() {
        let layout = layout_of(
            "<style>div { width: 200px; aspect-ratio: 2 / 1; }\
                    p { width: 90px; aspect-ratio: 3; }</style><div></div><p></p>",
        );
        assert_eq!(body_box(&layout).children[0].content_box().height, 100.0);
        assert_eq!(body_box(&layout).children[1].content_box().height, 30.0);
    }

    #[test]
    fn max_width_clamps_and_auto_margins_center() {
        let layout = layout_of(
            "<style>div { max-width: 400px; margin-left: auto; margin-right: auto; \
                          height: 10px; }</style><div></div>",
        );
        let div = &body_box(&layout).children[0];
        assert_eq!(div.content_box().width, 400.0);
        // (800 - 400) / 2 on each side.
        assert_eq!(div.content_box().x, 200.0);
    }

    #[test]
    fn min_width_wins_over_max_width() {
        let layout = layout_of(
            "<style>div { width: 100px; max-width: 50px; min-width: 200px; height: 5px; }\
             </style><div></div>",
        );
        assert_eq!(body_box(&layout).children[0].content_box().width, 200.0);
    }

    #[test]
    fn min_and_max_height_clamp_the_used_height() {
        let layout = layout_of(
            "<style>.short { min-height: 50px; } .tall { max-height: 20px; }</style>\
             <div class='short'></div>\
             <div class='tall'><div style='height: 100px;'></div></div>",
        );
        assert_eq!(body_box(&layout).children[0].content_box().height, 50.0);
        assert_eq!(body_box(&layout).children[1].content_box().height, 20.0);
        // The clamped box still stacks flow at its used height.
        assert_eq!(body_box(&layout).children[1].content_box().y, 50.0);
    }

    #[test]
    fn inline_lines_count_toward_inner_scroll() {
        // A textarea-like box: inline text lines overflow the fixed
        // height, so the box must report scrollable room.
        let layout = layout_of(
            "<div style='height: 30px; overflow: scroll; white-space: pre;'>a\nb\nc\nd\ne</div>",
        );
        let scroller = &body_box(&layout).children[0];
        assert!(
            scroller.max_inner_scroll() > 0.0,
            "inline overflow should scroll, got {}",
            scroller.max_inner_scroll()
        );
    }

    #[test]
    fn images_flow_inline_with_text() {
        let layout = layout_of("<p>before <img src='x.png' width='30' height='20'> after</p>");
        let paragraph = &body_box(&layout).children[0];
        let LayoutKind::Inline { lines } = &paragraph.children[0].kind else {
            panic!("expected inline content in p");
        };
        // One line: text, image box, text.
        assert_eq!(lines.len(), 1);
        let kinds: Vec<bool> = lines[0]
            .fragments
            .iter()
            .map(|fragment| matches!(fragment.content, FragmentContent::Box(_)))
            .collect();
        assert_eq!(kinds, vec![false, true, false]);
        // The image fragment is 30 wide and lifts the line to 20 tall.
        assert_eq!(lines[0].fragments[1].width, 30.0);
        assert!(lines[0].height >= 20.0);
    }

    #[test]
    fn grid_places_items_in_tracks() {
        let layout = layout_of(
            "<style>.g { display: grid; grid-template-columns: 100px 1fr 2fr; gap: 10px; }\
                    .g div { height: 10px; }</style>\
             <div class='g'><div></div><div></div><div></div><div></div></div>",
        );
        let grid = &body_box(&layout).children[0];
        let cell = |index: usize| grid.children[index].border_box();
        // 800 - 100 - 20 gaps = 680 leftover → 1fr ≈ 226.67, 2fr ≈ 453.33.
        assert_eq!(cell(0).width, 100.0);
        assert!((cell(1).width - 680.0 / 3.0).abs() < 0.5);
        assert!((cell(2).width - 2.0 * 680.0 / 3.0).abs() < 0.5);
        assert_eq!(cell(1).x, 110.0);
        // Fourth item wraps to row 2, column 1.
        assert_eq!(cell(3).x, 0.0);
        assert_eq!(cell(3).y, 20.0);
        assert_eq!(grid.content_box().height, 30.0);
    }

    #[test]
    fn grid_span_and_repeat_work() {
        let layout = layout_of(
            "<style>.g { display: grid; grid-template-columns: repeat(3, 1fr); gap: 0px; }\
                    .wide { grid-column: span 2; height: 10px; }\
                    .one { height: 10px; }</style>\
             <div class='g'><div class='wide'></div><div class='one'></div>\
             <div class='one'></div></div>",
        );
        let grid = &body_box(&layout).children[0];
        let wide = grid.children[0].border_box();
        let single = grid.children[1].border_box();
        assert!((wide.width - 2.0 * 800.0 / 3.0).abs() < 1.0);
        assert!((single.x - 2.0 * 800.0 / 3.0).abs() < 1.0);
        // The third item wraps (no room after span 2 + 1).
        assert_eq!(grid.children[2].border_box().y, 10.0);
    }

    #[test]
    fn grid_template_rows_size_rows() {
        let layout = layout_of(
            "<style>.g { display: grid; grid-template-columns: 100px 100px; \
                            grid-template-rows: 30px 50px; gap: 10px; }\
                    .g div { width: 10px; }</style>\
             <div class='g'><div></div><div></div><div></div></div>",
        );
        let grid = &body_box(&layout).children[0];
        let cell = |index: usize| grid.children[index].border_box();
        // Auto-height items stretch to their fixed row track.
        assert_eq!(cell(0).height, 30.0);
        assert_eq!(cell(1).height, 30.0);
        // Third item sits in row 2 at track offset 30 + 10 gap.
        assert_eq!(cell(2).y, 40.0);
        assert_eq!(cell(2).height, 50.0);
        assert_eq!(grid.content_box().height, 90.0);
    }

    #[test]
    fn grid_row_span_covers_multiple_rows() {
        let layout = layout_of(
            "<style>.g { display: grid; grid-template-columns: 100px 100px; \
                            grid-template-rows: 20px 20px; gap: 10px; }\
                    .tall { grid-row: span 2; }\
                    .g div { width: 10px; }</style>\
             <div class='g'><div class='tall'></div><div></div><div></div></div>",
        );
        let grid = &body_box(&layout).children[0];
        let tall = grid.children[0].border_box();
        // 20 + 10 gap + 20.
        assert_eq!(tall.height, 50.0);
        // The third item flows into row 2, column 2 (column 1 is covered
        // by the spanning item).
        let third = grid.children[2].border_box();
        assert_eq!(third.x, 110.0);
        assert_eq!(third.y, 30.0);
    }

    #[test]
    fn flex_basis_sets_the_base_main_size() {
        let layout = layout_of(
            "<style>.row { display: flex; }\
                    .a { flex-basis: 200px; height: 10px; }\
                    .b { flex: 1 100px; height: 10px; }</style>\
             <div class='row'><div class='a'></div><div class='b'></div></div>",
        );
        let row = &body_box(&layout).children[0];
        assert_eq!(row.children[0].border_box().width, 200.0);
        // b: basis 100 + the whole free space (800 - 300).
        assert_eq!(row.children[1].border_box().width, 600.0);
        assert_eq!(row.children[1].border_box().x, 200.0);
    }

    #[test]
    fn order_reorders_flex_items_visually() {
        let layout = layout_of(
            "<style>.row { display: flex; }\
                    .a { width: 100px; height: 10px; order: 2; }\
                    .b { width: 100px; height: 10px; order: 1; }</style>\
             <div class='row'><div class='a'></div><div class='b'></div></div>",
        );
        let row = &body_box(&layout).children[0];
        // DOM order is a, b; layout order follows `order` (b first).
        assert_eq!(row.children[0].style.order, 1);
        assert_eq!(row.children[0].border_box().x, 0.0);
        assert_eq!(row.children[1].style.order, 2);
        assert_eq!(row.children[1].border_box().x, 100.0);
    }

    #[test]
    fn align_content_distributes_wrapped_lines() {
        let layout = layout_of(
            "<style>.row { display: flex; flex-wrap: wrap; height: 100px; \
                            align-content: space-between; }\
                    .item { width: 800px; height: 20px; }</style>\
             <div class='row'><div class='item'></div><div class='item'></div></div>",
        );
        let row = &body_box(&layout).children[0];
        assert_eq!(row.children[0].border_box().y, 0.0);
        // Free cross space 100 - 40 = 60 goes between the two lines.
        assert_eq!(row.children[1].border_box().y, 80.0);
    }

    #[test]
    fn logical_properties_drive_layout() {
        let layout = layout_of(
            "<style>div { margin-inline-start: 30px; margin-block-start: 10px; \
                          padding-inline: 5px 7px; border-inline-start-width: 2px; \
                          inline-size: 100px; block-size: 20px; }</style>\
             <div></div>",
        );
        let div = &body_box(&layout).children[0];
        // border box x = margin-left 30; content width 100.
        assert_eq!(div.border_box().x, 30.0);
        assert_eq!(div.border_box().y, 10.0);
        assert_eq!(div.content_box().x, 30.0 + 2.0 + 5.0);
        assert_eq!(div.content_box().width, 100.0);
        assert_eq!(div.content_box().height, 20.0);
        assert_eq!(div.dimensions.padding.right, 7.0);
    }

    #[test]
    fn inset_shorthand_offsets_relative_box() {
        let layout = layout_of(
            "<style>div { position: relative; inset: 5px 0 0 10px; height: 20px; }</style>\
             <div></div>",
        );
        let div = &body_box(&layout).children[0];
        assert_eq!(div.border_box().x, 10.0);
        assert_eq!(div.border_box().y, 5.0);
    }

    #[test]
    fn min_max_clamp_drive_layout() {
        let layout = layout_of(
            "<style>.a { width: min(200px, 300px); height: 10px; }\
                    .b { width: max(100px, 50px); height: 10px; }\
                    .c { width: clamp(150px, 500px, 250px); height: 10px; }</style>\
             <div class='a'></div><div class='b'></div><div class='c'></div>",
        );
        let body = body_box(&layout);
        assert_eq!(body.children[0].content_box().width, 200.0);
        assert_eq!(body.children[1].content_box().width, 100.0);
        // clamp picks the preferred value bounded to [min, max] → 250.
        assert_eq!(body.children[2].content_box().width, 250.0);
    }

    #[test]
    fn tables_lay_out_columns_and_rows() {
        let layout = layout_of(
            "<style>td { padding: 0; } table { gap: 0px; }</style>\
             <table>\
             <tr><td style='width: 100px; height: 10px;'>a</td>\
                 <td style='width: 50px; height: 10px;'>b</td></tr>\
             <tr><td style='height: 20px;'>c</td><td style='height: 20px;'>d</td></tr>\
             </table>",
        );
        let table = &body_box(&layout).children[0];
        assert_eq!(table.children.len(), 4);
        let cell = |index: usize| table.children[index].border_box();
        // Columns align across rows (spacing floor is 2px).
        assert_eq!(cell(0).x, cell(2).x);
        assert_eq!(cell(1).x, cell(3).x);
        assert!(cell(1).x >= cell(0).x + 100.0);
        // Second row sits below the first.
        assert!(cell(2).y > cell(0).y);
        // Rows share heights: cells 2 and 3 on one line.
        assert_eq!(cell(2).y, cell(3).y);
    }

    #[test]
    fn colspan_spans_columns() {
        let layout = layout_of(
            "<table>\
             <tr><td colspan='2' style='height: 5px;'>wide</td></tr>\
             <tr><td style='width: 60px; height: 5px;'>a</td>\
                 <td style='width: 40px; height: 5px;'>b</td></tr>\
             </table>",
        );
        let table = &body_box(&layout).children[0];
        let wide = table.children[0].border_box();
        let a = table.children[1].border_box();
        let b = table.children[2].border_box();
        // The spanning cell covers both columns.
        assert!((wide.width - (a.width + b.width + 2.0)).abs() < 1.0);
        assert_eq!(wide.x, a.x);
        assert!(b.x > a.x);
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
        let div = &body_box(&layout).children[0];
        assert_eq!(div.box_type, BoxType::Block);
        let anonymous = &div.children[0];
        assert_eq!(anonymous.box_type, BoxType::AnonymousBlock);
        assert!(matches!(&anonymous.kind, LayoutKind::Inline { lines } if lines.len() == 1));
    }

    #[test]
    fn absolute_top_percent_resolves_against_containing_height() {
        let layout = layout_of(
            "<style>.outer { position: relative; height: 100px; }\
                    .inner { position: absolute; top: 50%; height: 10px; }</style>\
             <div class='outer'><div class='inner'></div></div>",
        );
        let outer = &body_box(&layout).children[0];
        assert_eq!(outer.content_box().height, 100.0);
        let inner = outer
            .children
            .iter()
            .find(|child| child.style.position == crate::style::Position::Absolute)
            .expect("absolute child is laid out");
        // 50% of the 100px containing block height, not of its width.
        assert_eq!(inner.content_box().y, outer.content_box().y + 50.0);
    }

    #[test]
    fn parent_margin_collapses_past_a_float_first_child() {
        // The float first child is out of flow: the parent's top margin
        // collapses with the first *in-flow* block child's 20px margin.
        let layout = layout_of(
            "<style>.parent { margin-top: 10px; }\
                    .fl { float: left; width: 10px; height: 10px; }\
                    .child { margin-top: 20px; height: 5px; }</style>\
             <div class='parent'><div class='fl'></div><div class='child'></div></div>",
        );
        let parent = &body_box(&layout).children[0];
        assert_eq!(parent.dimensions.margin.top, 20.0);
        // Both children are laid out (the float was not dropped either).
        assert_eq!(parent.children.len(), 2);
    }

    #[test]
    fn negative_explicit_width_clamps_to_zero() {
        let layout = layout_of("<style>div { width: -50px; height: 10px; }</style><div>x</div>");
        assert_eq!(body_box(&layout).children[0].content_box().width, 0.0);
    }

    #[test]
    fn deeply_nested_auto_width_inline_blocks_lay_out_fast() {
        // Without probe memoization this is O(2^depth): every auto-width
        // atomic box probes (fully lays out) its subtree and then lays it
        // out again, doubling the work per nesting level.
        let mut html = String::from("<style>.ib { display: inline-block; }</style><div>");
        for _ in 0..30 {
            html.push_str("<span class='ib'>");
        }
        html.push('x');
        for _ in 0..30 {
            html.push_str("</span>");
        }
        html.push_str("</div>");
        let start = std::time::Instant::now();
        let layout = layout_of(&html);
        assert!(layout.content_box().width > 0.0);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "30 nested auto-width inline-blocks took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn deeply_nested_tables_lay_out_fast() {
        // Each auto-width cell triggers a shrink-to-fit probe of its
        // subtree; nested tables compound that exponentially without
        // probe memoization.
        let mut html = String::new();
        for _ in 0..16 {
            html.push_str("<table><tr><td>");
        }
        html.push('x');
        for _ in 0..16 {
            html.push_str("</td></tr></table>");
        }
        let start = std::time::Instant::now();
        let layout = layout_of(&html);
        assert!(layout.content_box().width > 0.0);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "16 nested tables took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn ellipsis_on_very_long_text_stays_fast() {
        // The truncation search measures a prefix per binary-search step
        // instead of cloning and re-measuring per character (O(n²)).
        let words = "word ".repeat(10_000);
        let html = format!(
            "<style>div {{ width: 80px; white-space: nowrap; overflow: hidden; \
                          text-overflow: ellipsis; }}</style><div>{words}</div>"
        );
        let start = std::time::Instant::now();
        let layout = layout_of(&html);
        let LayoutKind::Inline { lines } = &body_box(&layout).children[0].children[0].kind else {
            panic!("expected inline content");
        };
        let text = lines[0].fragments[0].text().unwrap();
        assert!(text.ends_with('…'), "{text}");
        assert!(lines[0].fragments[0].width <= 80.0);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "ellipsis over 50k chars took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn absurdly_deep_nesting_lays_out_without_overflow() {
        // Test threads have small stacks; run on a thread with a
        // main-thread-sized stack so the depth cap (not the harness) is
        // what bounds the recursion.
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(|| {
                let mut html = String::new();
                for _ in 0..crate::MAX_DEPTH * 4 {
                    html.push_str("<div>");
                }
                // Completes without a stack overflow; the subtree past
                // the cap collapses into empty boxes.
                let layout = layout_of(&html);
                assert!(layout.content_box().width > 0.0);
            })
            .expect("spawn")
            .join()
            .expect("no stack overflow");
    }

    #[test]
    fn hit_test_page_finds_fixed_boxes_after_scrolling() {
        let layout = layout_of(
            "<style>.badge { position: fixed; top: 10px; left: 10px; \
                    width: 50px; height: 20px; }\
                    .tall { height: 5000px; }</style>\
             <div class='tall'></div><div class='badge'></div>",
        );
        let offsets = std::collections::HashMap::new();
        // Same viewport point (15, 15), once unscrolled and once with
        // the page scrolled 1000px down: the fixed badge is hit both
        // times, not the tall div scrolled underneath the point.
        let unscrolled = layout.hit_test_page(15.0, 15.0, 0.0, &offsets);
        let scrolled = layout.hit_test_page(15.0, 1015.0, 1000.0, &offsets);
        assert!(unscrolled.is_some());
        assert_eq!(unscrolled, scrolled);
        let node = scrolled.expect("the badge is hit");
        let laid = layout.find_by_node(node).expect("hit node has a box");
        assert_eq!(laid.style.position, Position::Fixed);
    }
}
