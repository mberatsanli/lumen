//! Grid layout (auto-flow row subset).
//!
//! `grid-template-columns` defines the tracks (px/%/fr/auto with
//! `repeat()`; fr shares the leftover after fixed tracks and gaps, auto
//! behaves like 1fr). Items place in source order, row by row, spanning
//! `grid-column: span N` / `grid-row: span M` tracks (clamped to the
//! grid). `grid-template-rows` sizes rows (px always; %/fr only when the
//! container height is explicit); rows without a track size to their
//! tallest single-row item, and items stretch to their cell. No named
//! lines/areas, explicit placement or dense packing.

use crate::geometry::Size;
use crate::image::ImageMap;
use crate::layout::{LayoutBox, ProbeCache, layout_isolated_with_style};
use crate::style::{AlignItems, BoxSizing, ComputedStyle, Dimension, Display, GridTrack, StyleMap};
use crate::text::TextMeasurer;
use lumen_html::{Document, NodeId, NodeKind};

/// Lays out a grid container's children. Returns the laid items and the
/// used content height.
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_grid_children(
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
    let tracks = if style.grid_columns.is_empty() {
        vec![GridTrack::Auto]
    } else {
        style.grid_columns.clone()
    };
    let count = tracks.len();
    let gap = style.gap;
    let total_gaps = gap * (count as f32 - 1.0).max(0.0);

    // Resolve track widths: fixed first, fr/auto share the leftover.
    let mut widths = vec![0.0f32; count];
    let mut fr_total = 0.0f32;
    let mut fixed_total = 0.0f32;
    for (index, track) in tracks.iter().enumerate() {
        match track {
            GridTrack::Px(px) => {
                widths[index] = *px;
                fixed_total += px;
            }
            GridTrack::Percent(percent) => {
                widths[index] = content_width * percent / 100.0;
                fixed_total += widths[index];
            }
            GridTrack::Fr(fraction) => fr_total += fraction.max(0.0),
            GridTrack::Auto => fr_total += 1.0,
        }
    }
    let leftover = (content_width - total_gaps - fixed_total).max(0.0);
    for (index, track) in tracks.iter().enumerate() {
        match track {
            GridTrack::Fr(fraction) => {
                widths[index] = leftover * fraction.max(0.0) / fr_total.max(f32::EPSILON);
            }
            GridTrack::Auto => widths[index] = leftover / fr_total.max(f32::EPSILON),
            _ => {}
        }
    }
    let mut offsets = Vec::with_capacity(count);
    let mut x = 0.0;
    for width in &widths {
        offsets.push(x);
        x += width + gap;
    }

    // Collect items with their spans.
    struct GridItem<'a> {
        node: NodeId,
        style: &'a ComputedStyle,
        colspan: usize,
        rowspan: usize,
    }
    let mut items: Vec<GridItem<'_>> = Vec::new();
    for child in document.children(node_id) {
        if !matches!(&document.node(*child).kind, NodeKind::Element(_)) {
            continue;
        }
        let Some(child_style) = styles.by_node.get(child) else {
            continue;
        };
        if child_style.display == Display::None {
            continue;
        }
        items.push(GridItem {
            node: *child,
            style: child_style,
            colspan: child_style.grid_span.clamp(1, count),
            rowspan: child_style.grid_row_span.clamp(1, 64),
        });
    }

    // Auto placement, row by row (sparse): the cursor only moves forward,
    // cells covered by row spans are skipped. Bounded so pathological
    // spans cannot loop forever.
    let mut occupied: Vec<Vec<bool>> = Vec::new();
    let mut cursor = (0usize, 0usize);
    let mut placements: Vec<(usize, usize)> = Vec::with_capacity(items.len());
    for item in &items {
        let (mut row, mut column) = cursor;
        let cell = loop {
            if row > 1024 {
                break (row, 0);
            }
            if column + item.colspan > count {
                row += 1;
                column = 0;
                continue;
            }
            let fits = (row..row + item.rowspan).all(|r| {
                (column..column + item.colspan).all(|c| {
                    !occupied
                        .get(r)
                        .and_then(|line| line.get(c))
                        .copied()
                        .unwrap_or(false)
                })
            });
            if fits {
                break (row, column);
            }
            column += 1;
        };
        for r in cell.0..cell.0 + item.rowspan {
            while occupied.len() <= r {
                occupied.push(vec![false; count]);
            }
            for slot in &mut occupied[r][cell.1..cell.1 + item.colspan] {
                *slot = true;
            }
        }
        placements.push(cell);
        cursor = if cell.1 + item.colspan >= count {
            (cell.0 + 1, 0)
        } else {
            (cell.0, cell.1 + item.colspan)
        };
    }
    let row_count = occupied.len();

    // Base layout per item: stretch to the cell width, auto height.
    let mut laid_items: Vec<LayoutBox> = Vec::with_capacity(items.len());
    for (item, &(_, column)) in items.iter().zip(&placements) {
        let cell_width: f32 = widths[column..column + item.colspan].iter().sum::<f32>()
            + gap * (item.colspan as f32 - 1.0);
        let mut item_style = item.style.clone();
        if matches!(item_style.width, Dimension::Auto) {
            // Items stretch to their cell by default.
            item_style.width = Dimension::Px(cell_width);
            item_style.box_sizing = BoxSizing::BorderBox;
        }
        laid_items.push(layout_isolated_with_style(
            document,
            styles,
            item.node,
            item_style,
            cell_width,
            viewport,
            measurer,
            images,
            probe_cache,
            depth,
        ));
    }

    // Row heights: declared tracks win; px always resolves, %/fr only
    // against an explicit container height (fr shares what px/% leave).
    // `auto` rows (and undeclared rows) size to their tallest single-row
    // item.
    let row_fr_total: f32 = style
        .grid_rows
        .iter()
        .take(row_count)
        .map(|track| match track {
            GridTrack::Fr(fraction) => fraction.max(0.0),
            _ => 0.0,
        })
        .sum();
    let row_fixed: f32 = style
        .grid_rows
        .iter()
        .take(row_count)
        .map(|track| match track {
            GridTrack::Px(px) => *px,
            GridTrack::Percent(percent) => {
                explicit_height.map_or(0.0, |height| height * percent / 100.0)
            }
            _ => 0.0,
        })
        .sum();
    let row_gaps = gap * (row_count as f32 - 1.0).max(0.0);
    let row_leftover =
        explicit_height.map_or(0.0, |height| (height - row_gaps - row_fixed).max(0.0));
    // Content height of one row from its single-row items.
    let content_height = |row: usize| -> f32 {
        items
            .iter()
            .zip(&placements)
            .zip(&laid_items)
            .filter(|((item, placement), _)| placement.0 == row && item.rowspan == 1)
            .map(|(_, laid)| laid.margin_box().height)
            .fold(0.0, f32::max)
    };
    // `Some` = resolved by a track (items stretch to it); `None` = auto.
    let row_heights: Vec<Option<f32>> = (0..row_count)
        .map(|row| match style.grid_rows.get(row) {
            Some(GridTrack::Px(px)) => Some(*px),
            Some(GridTrack::Percent(percent)) => {
                explicit_height.map(|height| height * percent / 100.0)
            }
            Some(GridTrack::Fr(fraction)) => explicit_height
                .map(|_| row_leftover * fraction.max(0.0) / row_fr_total.max(f32::EPSILON)),
            Some(GridTrack::Auto) | None => None,
        })
        .collect();
    let row_height = |row: usize| row_heights[row].unwrap_or_else(|| content_height(row));

    // Stretch items whose spanned rows are all track-resolved to fill
    // them (like the width stretch), then position every item at its
    // cell.
    // Row heights, settled before the items move: an item's stretch
    // reads them, and reading them borrows the items it came from.
    let resolved_rows: Vec<f32> = (0..row_count).map(row_height).collect();
    let mut children: Vec<LayoutBox> = Vec::with_capacity(items.len());
    let mut row_tops: Vec<f32> = Vec::with_capacity(row_count);
    let mut y = content_y;
    for height in &resolved_rows {
        row_tops.push(y);
        y += height + gap;
    }
    for ((item, &(row, column)), mut laid) in
        items.iter().zip(&placements).zip(laid_items.into_iter())
    {
        // Items fill the rows they span, whether a track sized those
        // rows or their own contents did: a short card in a row with a
        // tall one grows to match it.
        let stretches = matches!(
            item.style.align_self.unwrap_or(style.align_items),
            AlignItems::Stretch
        );
        if stretches && matches!(item.style.height, Dimension::Auto) {
            let target = resolved_rows[row..(row + item.rowspan).min(row_count)]
                .iter()
                .sum::<f32>()
                + gap * (item.rowspan as f32 - 1.0);
            if (laid.margin_box().height - target).abs() > 0.5 {
                let mut item_style = item.style.clone();
                if matches!(item_style.width, Dimension::Auto) {
                    let cell_width: f32 = widths[column..column + item.colspan].iter().sum::<f32>()
                        + gap * (item.colspan as f32 - 1.0);
                    item_style.width = Dimension::Px(cell_width);
                }
                item_style.height = Dimension::Px(target);
                item_style.box_sizing = BoxSizing::BorderBox;
                laid = layout_isolated_with_style(
                    document,
                    styles,
                    item.node,
                    item_style,
                    laid.content_box().width,
                    viewport,
                    measurer,
                    images,
                    probe_cache,
                    depth,
                );
            }
        }
        let margin_box = laid.margin_box();
        laid.translate(
            content_x + offsets[column] - margin_box.x,
            row_tops[row] - margin_box.y,
        );
        children.push(laid);
    }

    let used_height = (y - gap - content_y).max(0.0);
    (children, used_height)
}
