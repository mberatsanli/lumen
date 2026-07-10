//! Block layout with inline formatting.
//!
//! Layout runs in the classic phases per box: resolve width, position
//! horizontally, position vertically, lay out children, resolve height.
//! Block-level children stack vertically; consecutive inline-level
//! children (text and inline elements) form runs laid into shared line
//! boxes inside anonymous blocks (see `inline.rs`). Margin collapsing is
//! intentionally not implemented.

use crate::geometry::{Dimensions, EdgeSizes, Edges, Rect, Size};
use crate::image::ImageMap;
use crate::inline::{LineBox, layout_inline_run};
use crate::style::{ComputedStyle, Dimension, Display, StyleMap};
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
        // Replaced elements are promoted to block level (no inline images yet).
        NodeKind::Element(element) if element.tag_name == "img" => false,
        NodeKind::Element(_) => {
            styles
                .by_node
                .get(&node_id)
                .is_some_and(|style| style.display == Display::Inline)
                && !has_block_descendant(document, styles, node_id)
        }
        NodeKind::Document => false,
    }
}

fn has_block_descendant(document: &Document, styles: &StyleMap, node_id: NodeId) -> bool {
    document.descendants(node_id).any(|descendant| {
        styles
            .by_node
            .get(&descendant)
            .is_some_and(|style| style.display == Display::Block)
    })
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

    // Phase 1: width. Explicit widths set the content box; auto fills the
    // containing block minus margins, borders and paddings.
    let content_width = style
        .width
        .resolve(containing_width, viewport)
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

    let flush_run = |run: &mut Vec<NodeId>, cursor_y: &mut f32, children: &mut Vec<LayoutBox>| {
        if run.is_empty() {
            return;
        }
        let (lines, height) =
            layout_inline_run(document, styles, run, &style, content_width, measurer);
        run.clear();
        if lines.is_empty() {
            return;
        }
        children.push(LayoutBox {
            node_id,
            box_type: BoxType::AnonymousBlock,
            kind: LayoutKind::Inline { lines },
            dimensions: Dimensions {
                content: Rect {
                    x: content_x,
                    y: *cursor_y,
                    width: content_width,
                    height,
                },
                ..Dimensions::default()
            },
            style: style.clone(),
            children: Vec::new(),
        });
        *cursor_y += height;
    };

    for child in document.children(node_id) {
        let child_display = styles.by_node.get(child).map(|style| style.display);
        if child_display == Some(Display::None) {
            continue;
        }
        if is_inline_level(document, styles, *child) {
            run.push(*child);
        } else {
            flush_run(&mut run, &mut child_cursor_y, &mut children);
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
                children.push(layout);
            }
        }
    }
    flush_run(&mut run, &mut child_cursor_y, &mut children);

    // Phase 5: height. Explicit heights win; auto grows from the children.
    // Percent heights are unsupported and treated as auto; vh works.
    let content_height = match style.height {
        Dimension::Auto | Dimension::Percent(_) => (child_cursor_y - content_y).max(0.0),
        explicit => explicit
            .resolve(containing_width, viewport)
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
                .map(|fragment| fragment.text.as_str())
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
            .map(|fragment| fragment.text.as_str())
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
        assert_eq!(lines[0].fragments[0].text, "a");
        assert_eq!(lines[1].fragments[0].text, "b");
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
