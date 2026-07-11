//! Single-line flexbox layout (see `layout.rs` for the block algorithm).

use crate::geometry::{Dimensions, Rect, Size};
use crate::image::ImageMap;
use crate::inline::layout_inline_run;
use crate::layout::{
    BoxType, LayoutBox, LayoutKind, layout_atomic_box, layout_isolated_with_style,
};
use crate::style::{
    AlignItems, BoxSizing, ComputedStyle, Dimension, Display, FlexDirection, JustifyContent,
    StyleMap,
};
use crate::text::TextMeasurer;
use lumen_html::{Document, NodeId, NodeKind};

/// Single-line flexbox (no wrap, no shrink, no flex-basis; `width`/
/// `height` act as the base size). Bare text children become anonymous
/// items via inline layout.
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_flex_children(
    document: &Document,
    styles: &StyleMap,
    node_id: NodeId,
    style: &ComputedStyle,
    content_x: f32,
    content_y: f32,
    content_width: f32,
    explicit_height: Option<f32>,
    viewport: Size,
    measurer: &dyn TextMeasurer,
    images: &ImageMap,
) -> (Vec<LayoutBox>, f32) {
    let row = style.flex_direction == FlexDirection::Row;

    // Collect items: element children, plus anonymous items for runs of
    // bare inline content.
    enum Item {
        Element(NodeId),
        Run(Vec<NodeId>),
    }
    let mut items: Vec<Item> = Vec::new();
    let mut run: Vec<NodeId> = Vec::new();
    for child in document.children(node_id) {
        let is_element = matches!(&document.node(*child).kind, NodeKind::Element(_));
        let display = styles.by_node.get(child).map(|s| s.display);
        if display == Some(Display::None) {
            continue;
        }
        if is_element {
            if !run.is_empty() {
                items.push(Item::Run(std::mem::take(&mut run)));
            }
            items.push(Item::Element(*child));
        } else if matches!(&document.node(*child).kind, NodeKind::Text(text) if !text.trim().is_empty())
        {
            run.push(*child);
        }
    }
    if !run.is_empty() {
        items.push(Item::Run(run));
    }
    if items.is_empty() {
        return (Vec::new(), 0.0);
    }

    // Lay out an anonymous text run as a box, wrapped at `width`.
    let lay_run = |nodes: &[NodeId], width: f32| -> LayoutBox {
        let bounds = move |_line_top: f32| (0.0, width);
        let mut atomic = |atomic_node: NodeId, available: f32| {
            layout_atomic_box(
                document,
                styles,
                atomic_node,
                available,
                viewport,
                measurer,
                images,
            )
        };
        let (lines, height) = layout_inline_run(
            document,
            styles,
            nodes,
            style,
            (0.0, 0.0),
            &bounds,
            measurer,
            &mut atomic,
        );
        let natural = lines
            .iter()
            .flat_map(|line| line.fragments.iter())
            .map(|fragment| fragment.x + fragment.width)
            .fold(0.0, f32::max);
        LayoutBox {
            node_id,
            box_type: BoxType::AnonymousBlock,
            kind: LayoutKind::Inline { lines },
            dimensions: Dimensions {
                content: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: if row { natural } else { width },
                    height,
                },
                ..Dimensions::default()
            },
            style: style.clone(),
            children: Vec::new(),
        }
    };

    // Base layout per item: rows shrink auto widths, columns fill.
    let lay_element = |child: NodeId, main_override: Option<f32>| -> LayoutBox {
        let mut item_style = styles.by_node.get(&child).cloned().unwrap_or_default();
        if let Some(target) = main_override {
            // Grow targets are content-box main sizes.
            item_style.box_sizing = BoxSizing::ContentBox;
            if row {
                item_style.width = Dimension::Px(target.max(0.0));
            } else {
                item_style.height = Dimension::Px(target.max(0.0));
            }
        }
        if row && main_override.is_none() && matches!(item_style.width, Dimension::Auto) {
            layout_atomic_box(
                document,
                styles,
                child,
                content_width,
                viewport,
                measurer,
                images,
            )
        } else {
            layout_isolated_with_style(
                document,
                styles,
                child,
                item_style,
                content_width,
                viewport,
                measurer,
                images,
            )
        }
    };

    let mut boxes: Vec<(LayoutBox, f32)> = items
        .iter()
        .map(|item| {
            let laid = match item {
                Item::Element(child) => lay_element(*child, None),
                Item::Run(nodes) => lay_run(nodes, content_width),
            };
            let grow = match item {
                Item::Element(child) => styles.by_node.get(child).map_or(0.0, |s| s.flex_grow),
                Item::Run(_) => 0.0,
            };
            (laid, grow)
        })
        .collect();

    let main_size = |laid: &LayoutBox| {
        let margin_box = laid.margin_box();
        if row {
            margin_box.width
        } else {
            margin_box.height
        }
    };
    let container_main = if row {
        content_width
    } else {
        explicit_height.unwrap_or(f32::INFINITY)
    };
    let count = boxes.len() as f32;
    let total_gap = style.gap * (count - 1.0).max(0.0);

    // flex-grow: distribute positive free space, then re-lay grown items.
    let used: f32 = boxes.iter().map(|(laid, _)| main_size(laid)).sum::<f32>() + total_gap;
    let total_grow: f32 = boxes.iter().map(|(_, grow)| grow).sum();
    if container_main.is_finite() && container_main > used && total_grow > 0.0 {
        let free = container_main - used;
        for (index, item) in items.iter().enumerate() {
            let (laid, grow) = &boxes[index];
            if *grow <= 0.0 {
                continue;
            }
            let extra = free * grow / total_grow;
            let dimensions = &laid.dimensions;
            let edges_main = if row {
                dimensions.margin.left
                    + dimensions.margin.right
                    + dimensions.border.left
                    + dimensions.border.right
                    + dimensions.padding.left
                    + dimensions.padding.right
            } else {
                dimensions.margin.top
                    + dimensions.margin.bottom
                    + dimensions.border.top
                    + dimensions.border.bottom
                    + dimensions.padding.top
                    + dimensions.padding.bottom
            };
            let target = main_size(laid) + extra - edges_main;
            let grow_value = *grow;
            let relaid = match item {
                Item::Element(child) => lay_element(*child, Some(target)),
                Item::Run(nodes) => lay_run(nodes, target.max(0.0)),
            };
            boxes[index] = (relaid, grow_value);
        }
    }

    // Cross size of the line.
    let cross_of = |laid: &LayoutBox| {
        let margin_box = laid.margin_box();
        if row {
            margin_box.height
        } else {
            margin_box.width
        }
    };
    let line_cross = if row {
        explicit_height.unwrap_or_else(|| {
            boxes
                .iter()
                .map(|(laid, _)| cross_of(laid))
                .fold(0.0, f32::max)
        })
    } else {
        content_width
    };

    // align-items: stretch re-lays auto-cross items to fill the line.
    if style.align_items == AlignItems::Stretch {
        for (index, item) in items.iter().enumerate() {
            let Item::Element(child) = item else { continue };
            let mut item_style = styles.by_node.get(child).cloned().unwrap_or_default();
            let auto_cross = if row {
                matches!(item_style.height, Dimension::Auto)
            } else {
                matches!(item_style.width, Dimension::Auto)
            };
            if !auto_cross {
                continue;
            }
            let dimensions = &boxes[index].0.dimensions;
            let (margins, pb) = if row {
                (
                    dimensions.margin.top + dimensions.margin.bottom,
                    dimensions.border.top
                        + dimensions.border.bottom
                        + dimensions.padding.top
                        + dimensions.padding.bottom,
                )
            } else {
                (
                    dimensions.margin.left + dimensions.margin.right,
                    dimensions.border.left
                        + dimensions.border.right
                        + dimensions.padding.left
                        + dimensions.padding.right,
                )
            };
            let target = (line_cross - margins - pb).max(0.0);
            if row {
                item_style.height = Dimension::Px(target);
            } else {
                item_style.width = Dimension::Px(target);
            }
            item_style.box_sizing = BoxSizing::ContentBox;
            // Preserve any grow-adjusted main size.
            if row {
                item_style.width = Dimension::Px(boxes[index].0.content_box().width);
            } else {
                item_style.height = Dimension::Px(boxes[index].0.content_box().height);
            }
            let grow_value = boxes[index].1;
            boxes[index] = (
                layout_isolated_with_style(
                    document,
                    styles,
                    *child,
                    item_style,
                    content_width,
                    viewport,
                    measurer,
                    images,
                ),
                grow_value,
            );
        }
    }

    // Main-axis positions from justify-content.
    let used: f32 = boxes.iter().map(|(laid, _)| main_size(laid)).sum::<f32>() + total_gap;
    let free = if container_main.is_finite() {
        (container_main - used).max(0.0)
    } else {
        0.0
    };
    let (mut main_cursor, between_extra) = match style.justify_content {
        JustifyContent::Start => (0.0, 0.0),
        JustifyContent::Center => (free / 2.0, 0.0),
        JustifyContent::End => (free, 0.0),
        JustifyContent::SpaceBetween => {
            if count > 1.0 {
                (0.0, free / (count - 1.0))
            } else {
                (free / 2.0, 0.0)
            }
        }
    };

    let mut children = Vec::new();
    let mut used_cross: f32 = 0.0;
    for (laid, _) in boxes {
        let margin_box = laid.margin_box();
        let cross = cross_of(&laid);
        used_cross = used_cross.max(cross);
        let cross_offset = match style.align_items {
            AlignItems::Stretch | AlignItems::Start => 0.0,
            AlignItems::Center => (line_cross - cross) / 2.0,
            AlignItems::End => line_cross - cross,
        };
        let (dx, dy) = if row {
            (
                content_x + main_cursor - margin_box.x,
                content_y + cross_offset - margin_box.y,
            )
        } else {
            (
                content_x + cross_offset - margin_box.x,
                content_y + main_cursor - margin_box.y,
            )
        };
        let mut laid = laid;
        laid.translate(dx, dy);
        main_cursor += main_size(&laid) + style.gap + between_extra;
        children.push(laid);
    }

    let used_height = if row {
        line_cross
    } else {
        (main_cursor - style.gap - between_extra).max(0.0)
    };
    (children, used_height)
}
