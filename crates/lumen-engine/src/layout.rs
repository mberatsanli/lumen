//! Vertical block layout.
//!
//! Layout runs in the classic phases per box: resolve width, position
//! horizontally, position vertically, lay out children, resolve height.
//! Every visible element becomes a block-level box stacked top to bottom;
//! inline flow and line wrapping are later milestones (`Display::Inline`
//! elements currently stack like blocks). Margin collapsing is intentionally
//! not implemented.

use crate::geometry::{Dimensions, EdgeSizes, Edges, Rect, Size};
use crate::style::{ComputedStyle, Dimension, Display, StyleMap};
use crate::text::{Line, TextMeasurer, TextStyle, break_into_lines};
use lumen_html::{Document, ElementData, NodeId, NodeKind};
use std::fmt::Write as _;

/// The formatting role of a layout box.
///
/// `AnonymousBlock` and `Replaced` are not generated yet; they arrive with
/// inline layout and images respectively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxType {
    Block,
    Inline,
    AnonymousBlock,
    Replaced,
}

/// What the box renders: an element or a wrapped text run.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutKind {
    Element(String),
    Text { lines: Vec<Line> },
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
}

/// Lays out the whole document against a viewport. Pure function of its
/// inputs: relayout after a viewport change is just calling it again.
#[must_use]
pub fn layout_document(
    document: &Document,
    styles: &StyleMap,
    viewport: Size,
    measurer: &dyn TextMeasurer,
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
            measurer,
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
    measurer: &dyn TextMeasurer,
) -> Option<LayoutBox> {
    let style = styles.by_node.get(&node_id)?.clone();
    if style.display == Display::None {
        return None;
    }

    match &document.node(node_id).kind {
        NodeKind::Document => None,
        NodeKind::Text(text) => {
            // Whitespace collapsing: runs of whitespace become one space.
            let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if normalized.is_empty() {
                return None;
            }
            let text_style = TextStyle {
                font_size: style.font_size,
                font_weight: style.font_weight,
            };
            let lines = break_into_lines(&normalized, &text_style, containing_width, measurer);
            let height = lines.len() as f32 * style.line_height;
            let content = Rect {
                x: containing_x,
                y: *cursor_y,
                width: containing_width,
                height,
            };
            *cursor_y += height;
            Some(LayoutBox {
                node_id,
                box_type: BoxType::Inline,
                kind: LayoutKind::Text { lines },
                dimensions: Dimensions {
                    content,
                    ..Dimensions::default()
                },
                style,
                children: Vec::new(),
            })
        }
        NodeKind::Element(element) => Some(layout_element(
            document,
            styles,
            node_id,
            element,
            style,
            containing_x,
            cursor_y,
            containing_width,
            measurer,
        )),
    }
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
    measurer: &dyn TextMeasurer,
) -> LayoutBox {
    let mut margin = resolve_edges(&style.margin, containing_width);
    let border = style.border_width;
    let padding = resolve_edges(&style.padding, containing_width);

    // Phase 1: width. Explicit widths set the content box; auto fills the
    // containing block minus margins, borders and paddings.
    let content_width = style.width.resolve(containing_width).unwrap_or_else(|| {
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

    // Phase 4: children, laid out against this box's content box.
    let mut child_cursor_y = content_y;
    let mut children = Vec::new();
    for child in document.children(node_id) {
        if let Some(layout) = layout_node(
            document,
            styles,
            *child,
            content_x,
            &mut child_cursor_y,
            content_width,
            measurer,
        ) {
            children.push(layout);
        }
    }

    // Phase 5: height. Explicit heights win; auto grows from the children.
    let content_height = match style.height {
        Dimension::Px(height) => height,
        // Percent heights are unsupported; treated as auto.
        Dimension::Auto | Dimension::Percent(_) => (child_cursor_y - content_y).max(0.0),
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

/// Percentages resolve against the containing block width; `auto` margins
/// and paddings resolve to zero (no centering yet).
fn resolve_edges(edges: &EdgeSizes<Dimension>, containing_width: f32) -> Edges {
    let resolve = |dimension: &Dimension| dimension.resolve(containing_width).unwrap_or(0.0);
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
        LayoutKind::Text { lines } => {
            let joined = lines
                .iter()
                .map(|line| line.text.as_str())
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
        );
        let wide = layout_document(
            &document,
            &styles,
            Size {
                width: 900.0,
                height: 100.0,
            },
            &crate::text::HeuristicMeasurer,
        );
        assert_eq!(narrow.children[0].content_box().width, 400.0);
        assert_eq!(wide.children[0].content_box().width, 900.0);
    }

    #[test]
    fn text_box_is_inline_elements_are_blocks() {
        let layout = layout_of("<div>hi</div>");
        let div = &layout.children[0];
        assert_eq!(div.box_type, BoxType::Block);
        assert_eq!(div.children[0].box_type, BoxType::Inline);
    }
}
