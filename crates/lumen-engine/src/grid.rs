//! Grid layout (auto-flow row subset).
//!
//! `grid-template-columns` defines the tracks (px/%/fr/auto with
//! `repeat()`; fr shares the leftover after fixed tracks and gaps, auto
//! behaves like 1fr). Items place in source order, row by row, spanning
//! `grid-column: span N` tracks (clamped to the row); rows are as tall as
//! their tallest item and items stretch to their cell width. No named
//! lines/areas, explicit placement, dense packing or `grid-template-rows`
//! sizing.

use crate::geometry::Size;
use crate::image::ImageMap;
use crate::layout::{LayoutBox, layout_isolated_with_style};
use crate::style::{BoxSizing, ComputedStyle, Dimension, Display, GridTrack, StyleMap};
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
    viewport: Size,
    measurer: &dyn TextMeasurer,
    images: &ImageMap,
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

    // Auto placement, row by row.
    let mut children: Vec<LayoutBox> = Vec::new();
    let mut cursor_y = content_y;
    let mut column = 0usize;
    let mut row: Vec<LayoutBox> = Vec::new();
    let mut row_height = 0.0f32;

    let flush_row = |row: &mut Vec<LayoutBox>,
                     row_height: &mut f32,
                     cursor_y: &mut f32,
                     children: &mut Vec<LayoutBox>| {
        if row.is_empty() {
            return;
        }
        children.append(row);
        *cursor_y += *row_height + gap;
        *row_height = 0.0;
    };

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
        let span = child_style.grid_span.clamp(1, count);
        if column + span > count {
            flush_row(&mut row, &mut row_height, &mut cursor_y, &mut children);
            column = 0;
        }
        let cell_width: f32 =
            widths[column..column + span].iter().sum::<f32>() + gap * (span as f32 - 1.0);

        let mut item_style = child_style.clone();
        if matches!(item_style.width, Dimension::Auto) {
            // Items stretch to their cell by default.
            item_style.width = Dimension::Px(cell_width);
            item_style.box_sizing = BoxSizing::BorderBox;
        }
        let mut laid = layout_isolated_with_style(
            document, styles, *child, item_style, cell_width, viewport, measurer, images,
        );
        let margin_box = laid.margin_box();
        laid.translate(
            content_x + offsets[column] - margin_box.x,
            cursor_y - margin_box.y,
        );
        row_height = row_height.max(laid.margin_box().height);
        row.push(laid);
        column += span;
        if column >= count {
            flush_row(&mut row, &mut row_height, &mut cursor_y, &mut children);
            column = 0;
        }
    }
    flush_row(&mut row, &mut row_height, &mut cursor_y, &mut children);

    let used_height = (cursor_y - gap - content_y).max(0.0);
    (children, used_height)
}
