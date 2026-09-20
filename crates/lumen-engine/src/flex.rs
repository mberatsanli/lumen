//! Flexbox layout (wrapping subset) (see `layout.rs` for the block algorithm).

use crate::geometry::{Dimensions, Rect, Size};
use crate::image::ImageMap;
use crate::inline::layout_inline_run;
use crate::layout::{
    BoxType, LayoutBox, LayoutKind, ProbeCache, layout_atomic_box, layout_isolated_with_style,
};
use crate::style::{
    AlignItems, BoxSizing, ComputedStyle, Dimension, Display, FlexDirection, Float, JustifyContent,
    Overflow, Position, StyleMap,
};
use crate::text::TextMeasurer;
use lumen_html::{Document, NodeId, NodeKind};

/// Flexbox subset: `flex-wrap: wrap` (greedy line filling), `flex-grow`,
/// `flex-shrink` (weighted by base size), `flex-basis` (base main size),
/// `order` (stable ascending), `align-items`/`align-self`,
/// `align-content` (line distribution when wrapped), `justify-content`
/// and `gap` (used on both axes). No `wrap-reverse`; `width`/`height`
/// act as the base size when `flex-basis` is auto. Bare text children
/// become anonymous items via inline layout.
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
    probe_cache: &ProbeCache,
    depth: usize,
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
    // `order`: items lay out ascending, stable within one order value
    // (anonymous runs keep order 0).
    items.sort_by_key(|item| match item {
        Item::Element(child) => styles.by_node.get(child).map_or(0, |style| style.order),
        Item::Run(_) => 0,
    });
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
                probe_cache,
                depth,
            )
        };
        let (lines, height) = layout_inline_run(
            document,
            styles,
            nodes,
            style,
            None,
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
                probe_cache,
                depth,
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
                probe_cache,
                depth,
            )
        }
    };

    // Base layout plus flex factors per item. `flex-basis` (when not
    // auto) overrides the item's base main size; percent bases resolve
    // against the container's main size.
    let container_main_for_basis = if row {
        Some(content_width)
    } else {
        explicit_height
    };
    let main_size = |laid: &LayoutBox| {
        let margin_box = laid.margin_box();
        if row {
            margin_box.width
        } else {
            margin_box.height
        }
    };

    struct FlexItem {
        laid: LayoutBox,
        grow: f32,
        shrink: f32,
        align_self: Option<AlignItems>,
        /// Smallest main size the item may shrink to, margin box.
        min_main: f32,
    }

    // The floor an item may shrink to. An explicit `min-width`/
    // `min-height` sets it outright; the initial `auto` means the
    // content-based minimum, which stops a row item at the width of its
    // longest unbreakable content and a column item at its own content
    // height. Without it, shrinking would squeeze items to nothing and
    // their text would spill out.
    let minimum_main = |child: NodeId, laid: &LayoutBox| -> f32 {
        let item_style = styles.by_node.get(&child).cloned().unwrap_or_default();
        let dimensions = &laid.dimensions;
        let (declared, edges_main) = if row {
            (
                item_style.min_width,
                dimensions.margin.left
                    + dimensions.margin.right
                    + dimensions.border.left
                    + dimensions.border.right
                    + dimensions.padding.left
                    + dimensions.padding.right,
            )
        } else {
            (
                item_style.min_height,
                dimensions.margin.top
                    + dimensions.margin.bottom
                    + dimensions.border.top
                    + dimensions.border.bottom
                    + dimensions.padding.top
                    + dimensions.padding.bottom,
            )
        };
        if !matches!(declared, Dimension::Auto)
            && let Some(resolved) = declared.resolve(content_width, viewport)
        {
            return resolved + edges_main;
        }
        if !row {
            // A column item's content height is what it already laid out
            // to: shrinking the main axis never reflows it narrower.
            return main_size(laid);
        }
        let content_minimum = crate::layout::min_content_width(
            document,
            styles,
            child,
            viewport,
            measurer,
            images,
            probe_cache,
            depth,
        );
        // An item narrower than its content stays at its own width: the
        // declared size is a suggestion the minimum never exceeds.
        let suggestion = item_style
            .width
            .resolve(content_width, viewport)
            .unwrap_or(f32::INFINITY);
        content_minimum.min(suggestion) + edges_main
    };
    let mut flex_items: Vec<FlexItem> = items
        .iter()
        .map(|item| {
            let laid = match item {
                Item::Element(child) => {
                    let basis = styles.by_node.get(child).and_then(|item_style| {
                        match item_style.flex_basis {
                            Dimension::Auto => None,
                            other => container_main_for_basis
                                .and_then(|main| other.resolve(main, viewport)),
                        }
                    });
                    lay_element(*child, basis)
                }
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
            let min_main = match item {
                Item::Element(child) => minimum_main(*child, &laid),
                // An anonymous run reflows freely across a row, but
                // down a column its height is already its content's.
                Item::Run(_) => {
                    if row {
                        0.0
                    } else {
                        main_size(&laid)
                    }
                }
            };
            FlexItem {
                laid,
                grow,
                shrink,
                align_self,
                min_main,
            }
        })
        .collect();

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
        if free.abs() < 0.5 {
            continue;
        }
        let weight_of = |item: &FlexItem| {
            if free > 0.0 {
                item.grow
            } else {
                item.shrink * main_size(&item.laid)
            }
        };
        // Fast path: with no grow demand (or no shrink capacity) no item
        // changes size, so the second layout pass is skipped entirely —
        // without materializing a weights vector.
        let total: f32 = line
            .iter()
            .map(|&index| weight_of(&flex_items[index]))
            .sum();
        if total <= 0.0 {
            continue;
        }
        for &index in line {
            let weight = weight_of(&flex_items[index]);
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
            let floor = (flex_items[index].min_main - edges_main).max(0.0);
            let target = (main_size(laid) + delta - edges_main).max(floor);
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
            // Fast path: when the stretch would not actually change the
            // box, skip the third layout pass (see the helper for the
            // exact safety conditions).
            if stretch_relayout_is_noop(
                document,
                styles,
                *child,
                &item_style,
                &flex_items[index].laid,
                target,
                row,
            ) {
                continue;
            }
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
                probe_cache,
                depth,
            );
        }
    }

    // Position lines along the cross axis and items along the main axis.
    let line_crosses: Vec<f32> = lines
        .iter()
        .map(|line| line_cross_size(line, &flex_items))
        .collect();
    // `align-content` distributes leftover cross space between the lines
    // of a wrapping container (no effect on a single line).
    let container_cross = if row {
        explicit_height
    } else {
        Some(content_width)
    };
    let (mut cross_cursor, cross_between) = match container_cross {
        Some(container_cross) if !single_line => {
            let total: f32 = line_crosses.iter().sum::<f32>()
                + style.gap * (line_crosses.len() as f32 - 1.0).max(0.0);
            let free = (container_cross - total).max(0.0);
            match style.align_content {
                crate::style::AlignContent::Start => (0.0, 0.0),
                crate::style::AlignContent::Center => (free / 2.0, 0.0),
                crate::style::AlignContent::End => (free, 0.0),
                crate::style::AlignContent::SpaceBetween => {
                    if line_crosses.len() > 1 {
                        (0.0, free / (line_crosses.len() - 1) as f32)
                    } else {
                        (free / 2.0, 0.0)
                    }
                }
            }
        }
        _ => (0.0, 0.0),
    };
    let mut item_iter = flex_items.into_iter();
    let mut children = Vec::new();
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
        cross_cursor += line_cross + style.gap + cross_between;
    }

    let total_cross = (cross_cursor - style.gap - cross_between).max(0.0);
    let used_height = if row { total_cross } else { main_extent };
    (children, used_height)
}

/// Whether the stretch re-layout of `child` would reproduce its base
/// layout bit-for-bit, making that layout pass skippable. Conservative:
/// the target cross size must equal what auto layout already produced,
/// and every feature whose behavior flips when an auto size becomes
/// explicit must be absent (percent heights below, absolute descendants,
/// bottom margin collapsing, empty-block collapse-through, auto margins
/// on the switched axis, min-/max-clamps that read `box-sizing`).
fn stretch_relayout_is_noop(
    document: &Document,
    styles: &StyleMap,
    child: NodeId,
    item_style: &ComputedStyle,
    laid: &LayoutBox,
    target: f32,
    row: bool,
) -> bool {
    // The explicit cross size must reproduce the auto-laid cross size
    // exactly; anything less than bit equality is real re-layout work.
    let current_cross = if row {
        laid.content_box().height
    } else {
        laid.content_box().width
    };
    if target != current_cross {
        return false;
    }
    // The re-layout flips `box-sizing` to content-box. That is neutral
    // only when no min-/max-constraint clamps differently under the two
    // sizings (percent height constraints resolve to nothing here, as
    // the isolated layout has no containing height).
    let clamp_neutral = item_style.box_sizing == BoxSizing::ContentBox
        || (matches!(item_style.min_width, Dimension::Auto)
            && matches!(item_style.max_width, Dimension::Auto)
            && matches!(
                item_style.min_height,
                Dimension::Auto | Dimension::Percent(_)
            )
            && matches!(
                item_style.max_height,
                Dimension::Auto | Dimension::Percent(_)
            ));
    if !clamp_neutral {
        return false;
    }
    // Column stretch switches the width from auto to explicit, which
    // would let auto margins absorb the leftover space.
    if !row
        && (matches!(item_style.margin.left, Dimension::Auto)
            || matches!(item_style.margin.right, Dimension::Auto))
    {
        return false;
    }
    // The re-layout also makes the height explicit (to preserve the
    // flex-adjusted main size). When the base height was auto, several
    // behaviors flip together with that switch.
    if matches!(item_style.height, Dimension::Auto) {
        // Percent heights below the item resolve only once an ancestor
        // height is explicit; absolute descendants resolve `bottom` and
        // percent offsets against the nearest positioned ancestor's
        // explicit height.
        let has_sensitive_descendant = document.descendants(child).any(|descendant| {
            styles.by_node.get(&descendant).is_some_and(|style| {
                matches!(style.height, Dimension::Percent(_))
                    || matches!(style.min_height, Dimension::Percent(_))
                    || matches!(style.max_height, Dimension::Percent(_))
                    || style.position == Position::Absolute
            })
        });
        if has_sensitive_descendant {
            return false;
        }
        // Bottom parent-child margin collapsing applies only under an
        // auto height; it is a no-op when it cannot apply or when the
        // last in-flow child's bottom margin is zero.
        if laid.dimensions.border.bottom == 0.0
            && laid.dimensions.padding.bottom == 0.0
            && item_style.overflow_x == Overflow::Visible
            && item_style.overflow_y == Overflow::Visible
        {
            let last_in_flow_bottom = laid
                .children
                .iter()
                .rev()
                .find(|child| {
                    child.style.float == Float::None
                        && matches!(child.style.position, Position::Static | Position::Relative)
                })
                .map(|child| child.dimensions.margin.bottom);
            // A trailing anonymous (inline) block carries no margin, so
            // both arms collapse to "no-op" only at exactly zero.
            if !matches!(last_in_flow_bottom, None | Some(0.0)) {
                return false;
            }
        }
        // Empty edgeless blocks collapse through themselves only under
        // an auto height.
        let vertical_edges = laid.dimensions.border.top
            + laid.dimensions.border.bottom
            + laid.dimensions.padding.top
            + laid.dimensions.padding.bottom;
        if laid.children.is_empty()
            && !matches!(laid.kind, LayoutKind::Inline { .. })
            && laid.dimensions.content.height == 0.0
            && vertical_edges == 0.0
            && item_style.position == Position::Static
        {
            return false;
        }
    }
    true
}
