//! Flexbox layout (wrapping subset) (see `layout.rs` for the block algorithm).

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

/// Flexbox subset: `flex-wrap: wrap` (greedy line filling), `flex-grow`,
/// `flex-shrink` (weighted by base size), `align-items`/`align-self`,
/// `justify-content` and `gap` (used on both axes). No `flex-basis`,
/// `order`, `align-content` distribution or `wrap-reverse`; `width`/
/// `height` act as the base size. Bare text children become anonymous
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

    // Base layout plus flex factors per item.
    struct FlexItem {
        laid: LayoutBox,
        grow: f32,
        shrink: f32,
        align_self: Option<AlignItems>,
    }
    let mut flex_items: Vec<FlexItem> = items
        .iter()
        .map(|item| {
            let laid = match item {
                Item::Element(child) => lay_element(*child, None),
                Item::Run(nodes) => lay_run(nodes, content_width),
            };
            let (grow, shrink, align_self) = match item {
                Item::Element(child) => {
                    styles
                        .by_node
                        .get(child)
                        .map_or((0.0, 1.0, None), |item_style| {
                            (
                                item_style.flex_grow,
                                item_style.flex_shrink,
                                item_style.align_self,
                            )
                        })
                }
                Item::Run(_) => (0.0, 1.0, None),
            };
            FlexItem {
                laid,
                grow,
                shrink,
                align_self,
            }
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
    let cross_of = |laid: &LayoutBox| {
        let margin_box = laid.margin_box();
        if row {
            margin_box.height
        } else {
            margin_box.width
        }
    };
    let container_main = if row {
        content_width
    } else {
        explicit_height.unwrap_or(f32::INFINITY)
    };

    // Split items into flex lines: greedy filling when wrapping, one line
    // otherwise. Lines hold consecutive item indices.
    let mut lines: Vec<Vec<usize>> = Vec::new();
    if style.flex_wrap && container_main.is_finite() {
        let mut line: Vec<usize> = Vec::new();
        let mut used = 0.0;
        for (index, item) in flex_items.iter().enumerate() {
            let main = main_size(&item.laid);
            let extra = if line.is_empty() {
                main
            } else {
                style.gap + main
            };
            if !line.is_empty() && used + extra > container_main + 0.5 {
                lines.push(std::mem::take(&mut line));
                used = main;
            } else {
                used += extra;
            }
            line.push(index);
        }
        if !line.is_empty() {
            lines.push(line);
        }
    } else {
        lines.push((0..flex_items.len()).collect());
    }
    let single_line = lines.len() == 1;

    // Resolve flexible lengths per line: distribute positive free space by
    // grow weights, negative by shrink weights (factor times base size),
    // then re-lay adjusted items at their target main size.
    for line in &lines {
        if !container_main.is_finite() {
            continue;
        }
        let gaps = style.gap * (line.len() as f32 - 1.0).max(0.0);
        let used: f32 = line
            .iter()
            .map(|&index| main_size(&flex_items[index].laid))
            .sum::<f32>()
            + gaps;
        let free = container_main - used;
        let weights: Vec<f32> = line
            .iter()
            .map(|&index| {
                let item = &flex_items[index];
                if free > 0.0 {
                    item.grow
                } else {
                    item.shrink * main_size(&item.laid)
                }
            })
            .collect();
        let total: f32 = weights.iter().sum();
        if free.abs() < 0.5 || total <= 0.0 {
            continue;
        }
        for (&index, &weight) in line.iter().zip(&weights) {
            if weight <= 0.0 {
                continue;
            }
            let delta = free * weight / total;
            let laid = &flex_items[index].laid;
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
            let target = (main_size(laid) + delta - edges_main).max(0.0);
            flex_items[index].laid = match &items[index] {
                Item::Element(child) => lay_element(*child, Some(target)),
                Item::Run(nodes) => lay_run(nodes, target),
            };
        }
    }

    // The cross size of one line of items.
    let natural_cross = |line: &[usize], flex_items: &[FlexItem]| -> f32 {
        line.iter()
            .map(|&index| cross_of(&flex_items[index].laid))
            .fold(0.0, f32::max)
    };
    let line_cross_size = |line: &[usize], flex_items: &[FlexItem]| -> f32 {
        if row {
            if single_line {
                explicit_height.unwrap_or_else(|| natural_cross(line, flex_items))
            } else {
                natural_cross(line, flex_items)
            }
        } else if single_line {
            content_width
        } else {
            natural_cross(line, flex_items)
        }
    };

    // Stretch: items whose effective alignment is stretch and whose cross
    // size is auto re-lay to fill their line.
    for line in &lines {
        let line_cross = line_cross_size(line, &flex_items);
        for &index in line {
            let Item::Element(child) = &items[index] else {
                continue;
            };
            let align = flex_items[index].align_self.unwrap_or(style.align_items);
            if align != AlignItems::Stretch {
                continue;
            }
            let mut item_style = styles.by_node.get(child).cloned().unwrap_or_default();
            let auto_cross = if row {
                matches!(item_style.height, Dimension::Auto)
            } else {
                matches!(item_style.width, Dimension::Auto)
            };
            if !auto_cross {
                continue;
            }
            let dimensions = &flex_items[index].laid.dimensions;
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
            // Preserve any flex-adjusted main size.
            if row {
                item_style.width = Dimension::Px(flex_items[index].laid.content_box().width);
            } else {
                item_style.height = Dimension::Px(flex_items[index].laid.content_box().height);
            }
            flex_items[index].laid = layout_isolated_with_style(
                document,
                styles,
                *child,
                item_style,
                content_width,
                viewport,
                measurer,
                images,
            );
        }
    }

    // Position lines along the cross axis and items along the main axis.
    let line_crosses: Vec<f32> = lines
        .iter()
        .map(|line| line_cross_size(line, &flex_items))
        .collect();
    let mut item_iter = flex_items.into_iter();
    let mut children = Vec::new();
    let mut cross_cursor: f32 = 0.0;
    let mut main_extent: f32 = 0.0;
    for (line, line_cross) in lines.iter().zip(line_crosses) {
        let line_items: Vec<FlexItem> = item_iter.by_ref().take(line.len()).collect();
        let count = line_items.len() as f32;
        let gaps = style.gap * (count - 1.0).max(0.0);
        let used: f32 = line_items
            .iter()
            .map(|item| main_size(&item.laid))
            .sum::<f32>()
            + gaps;
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
        for item in line_items {
            let mut laid = item.laid;
            let margin_box = laid.margin_box();
            let cross = cross_of(&laid);
            let align = item.align_self.unwrap_or(style.align_items);
            let cross_offset = match align {
                AlignItems::Stretch | AlignItems::Start => 0.0,
                AlignItems::Center => (line_cross - cross) / 2.0,
                AlignItems::End => line_cross - cross,
            };
            let (dx, dy) = if row {
                (
                    content_x + main_cursor - margin_box.x,
                    content_y + cross_cursor + cross_offset - margin_box.y,
                )
            } else {
                (
                    content_x + cross_cursor + cross_offset - margin_box.x,
                    content_y + main_cursor - margin_box.y,
                )
            };
            laid.translate(dx, dy);
            main_cursor += main_size(&laid) + style.gap + between_extra;
            children.push(laid);
        }
        main_extent = main_extent.max((main_cursor - style.gap - between_extra).max(0.0));
        cross_cursor += line_cross + style.gap;
    }

    let total_cross = (cross_cursor - style.gap).max(0.0);
    let used_height = if row { total_cross } else { main_extent };
    (children, used_height)
}
