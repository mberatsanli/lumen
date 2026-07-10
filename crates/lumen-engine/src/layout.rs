//! Vertical block layout.
//!
//! Every visible element becomes a block-level box stacked top to bottom;
//! inline layout and line wrapping are later milestones (`Display::Inline`
//! elements currently flow like blocks). Margin collapsing is intentionally
//! not implemented.

use crate::geometry::{Edges, Rect, Size};
use crate::style::{ComputedStyle, Dimension, Display, StyleMap};
use lumen_html::{Document, ElementData, NodeId, NodeKind};
use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub enum LayoutKind {
    Element(String),
    Text(String),
}

/// A laid-out box. `rect` is the padding box (content plus padding);
/// margins are outside of it.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutBox {
    pub node_id: NodeId,
    pub kind: LayoutKind,
    pub rect: Rect,
    pub margin: Edges,
    pub padding: Edges,
    pub children: Vec<LayoutBox>,
    pub style: ComputedStyle,
}

/// Lays out the whole document against a viewport.
#[must_use]
pub fn layout_document(document: &Document, styles: &StyleMap, viewport: Size) -> LayoutBox {
    let root_style = styles
        .by_node
        .get(&document.root())
        .cloned()
        .unwrap_or_default();
    let mut root = LayoutBox {
        node_id: document.root(),
        kind: LayoutKind::Element("#document".to_string()),
        rect: Rect {
            x: 0.0,
            y: 0.0,
            width: viewport.width,
            height: viewport.height,
        },
        margin: Edges::default(),
        padding: Edges::default(),
        children: Vec::new(),
        style: root_style,
    };

    let mut cursor_y = 0.0;
    for child in document.children(document.root()) {
        if let Some(layout) =
            layout_node(document, styles, *child, 0.0, &mut cursor_y, viewport.width)
        {
            root.children.push(layout);
        }
    }
    root.rect.height = cursor_y.max(viewport.height);
    root
}

fn layout_node(
    document: &Document,
    styles: &StyleMap,
    node_id: NodeId,
    containing_x: f32,
    cursor_y: &mut f32,
    containing_width: f32,
) -> Option<LayoutBox> {
    let style = styles.by_node.get(&node_id)?.clone();
    if style.display == Display::None {
        return None;
    }

    match &document.node(node_id).kind {
        NodeKind::Document => None,
        NodeKind::Text(text) => {
            let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if normalized.is_empty() {
                return None;
            }
            let height = style.line_height;
            let rect = Rect {
                x: containing_x,
                y: *cursor_y,
                width: containing_width,
                height,
            };
            *cursor_y += height;
            Some(LayoutBox {
                node_id,
                kind: LayoutKind::Text(normalized),
                rect,
                margin: Edges::default(),
                padding: Edges::default(),
                children: Vec::new(),
                style,
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
) -> LayoutBox {
    // Percentages resolve against the containing block width; auto margins
    // and paddings resolve to zero (no centering yet).
    let resolve_edges = |edges: &crate::geometry::EdgeSizes<Dimension>| Edges {
        top: edges.top.resolve(containing_width).unwrap_or(0.0),
        right: edges.right.resolve(containing_width).unwrap_or(0.0),
        bottom: edges.bottom.resolve(containing_width).unwrap_or(0.0),
        left: edges.left.resolve(containing_width).unwrap_or(0.0),
    };
    let margin = resolve_edges(&style.margin);
    let padding = resolve_edges(&style.padding);

    *cursor_y += margin.top;
    let x = containing_x + margin.left;
    let available_width = (containing_width - margin.left - margin.right).max(0.0);
    let width = style
        .width
        .resolve(containing_width)
        .unwrap_or((available_width - padding.left - padding.right).max(0.0));
    let outer_width = width + padding.left + padding.right;
    let y = *cursor_y;
    let mut child_cursor_y = y + padding.top;
    let child_x = x + padding.left;
    let mut children = Vec::new();

    for child in document.children(node_id) {
        if let Some(layout) = layout_node(
            document,
            styles,
            *child,
            child_x,
            &mut child_cursor_y,
            width,
        ) {
            children.push(layout);
        }
    }

    let content_height = style
        .height
        .resolve(0.0) // Percent heights are unsupported; resolve as auto.
        .filter(|_| !matches!(style.height, Dimension::Percent(_)))
        .unwrap_or((child_cursor_y - (y + padding.top)).max(default_min_height(element)));
    let height = padding.top + content_height + padding.bottom;
    *cursor_y = y + height + margin.bottom;

    LayoutBox {
        node_id,
        kind: LayoutKind::Element(element.tag_name.clone()),
        rect: Rect {
            x,
            y,
            width: outer_width,
            height,
        },
        margin,
        padding,
        children,
        style,
    }
}

fn default_min_height(element: &ElementData) -> f32 {
    match element.tag_name.as_str() {
        "body" | "html" | "div" => 0.0,
        _ => 8.0,
    }
}

/// Human-readable layout dump for debugging and tests.
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
        LayoutKind::Text(text) => format!("\"{text}\""),
    };
    let _ = writeln!(
        output,
        "{indent}{label} x={} y={} w={} h={}",
        layout.rect.x, layout.rect.y, layout.rect.width, layout.rect.height
    );
    for child in &layout.children {
        dump_layout_box(child, depth + 1, output);
    }
}
