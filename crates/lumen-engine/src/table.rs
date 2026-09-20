//! Table layout (simplified automatic algorithm).
//!
//! Rows come from `<tr>` children (directly or through row groups), cells
//! from `<td>`/`<th>` with `colspan`/`rowspan`. Column widths follow a
//! crude auto algorithm: each column's preferred width is the widest cell
//! preference (explicit CSS width, else a shrink-to-fit probe); preferred
//! widths scale down proportionally when they overflow the table, and
//! stretch proportionally when the table has an explicit width. Cells are
//! separate-border boxes spaced by `border-spacing` (horizontal between
//! columns, vertical between rows; the legacy default is `gap`, else
//! 2px). Under `border-collapse: collapse` the spacing is zero and
//! interior borders collapse to a single line (naive: each cell drops
//! its top/left border except along the table's top/left edge); rowspan
//! reserves grid slots but does not stretch the spanning cell.

use crate::geometry::{Dimensions, Rect, Size};
use crate::image::ImageMap;
use crate::layout::{
    BoxType, LayoutBox, LayoutKind, ProbeCache, layout_isolated_with_style, natural_content_width,
};
use crate::style::{BoxSizing, ComputedStyle, Dimension, Display, StyleMap};
use crate::text::TextMeasurer;
use lumen_html::{Document, NodeId};

/// One row of the grid: the `<tr>`, the `<thead>`/`<tbody>`/`<tfoot>`
/// it sits in (when any), and its cells.
struct Row {
    node: NodeId,
    group: Option<NodeId>,
    cells: Vec<Cell>,
}

/// One placed cell before final positioning.
struct Cell {
    node: NodeId,
    column: usize,
    colspan: usize,
    rowspan: usize,
}

/// Collects the table's rows of cells, resolving colspan/rowspan into
/// grid positions. Returns the rows and the column count.
fn collect_grid(document: &Document, styles: &StyleMap, table: NodeId) -> (Vec<Row>, usize) {
    let mut rows: Vec<Row> = Vec::new();
    // Columns already occupied below earlier rowspan cells:
    // (column, remaining rows).
    let mut reserved: Vec<(usize, usize)> = Vec::new();

    // Rows carry the group they came from: the parser wraps bare rows
    // in a `<tbody>` of its own, so most tables have one.
    let mut row_nodes: Vec<(NodeId, Option<NodeId>)> = Vec::new();
    for child in document.children(table) {
        let Some(element) = document.element(*child) else {
            continue;
        };
        match element.tag_name.as_str() {
            "tr" => row_nodes.push((*child, None)),
            "thead" | "tbody" | "tfoot" => {
                for grandchild in document.children(*child) {
                    if document
                        .element(*grandchild)
                        .is_some_and(|element| element.tag_name == "tr")
                    {
                        row_nodes.push((*grandchild, Some(*child)));
                    }
                }
            }
            _ => {}
        }
    }

    let mut columns = 0;
    for (row_node, group) in row_nodes {
        let mut row: Vec<Cell> = Vec::new();
        let mut column = 0;
        let occupied: Vec<usize> = reserved.iter().map(|(column, _)| *column).collect();
        for cell_node in document.children(row_node) {
            let Some(element) = document.element(*cell_node) else {
                continue;
            };
            if element.tag_name != "td" && element.tag_name != "th" {
                continue;
            }
            if styles
                .by_node
                .get(cell_node)
                .is_some_and(|style| style.display == Display::None)
            {
                continue;
            }
            let attribute = |name: &str| -> usize {
                element
                    .attributes
                    .get(name)
                    .and_then(|value| value.parse::<usize>().ok())
                    .filter(|span| *span >= 1)
                    .unwrap_or(1)
            };
            while occupied.contains(&column) {
                column += 1;
            }
            let colspan = attribute("colspan").min(64);
            let rowspan = attribute("rowspan").min(64);
            row.push(Cell {
                node: *cell_node,
                column,
                colspan,
                rowspan,
            });
            if rowspan > 1 {
                for spanned in column..column + colspan {
                    reserved.push((spanned, rowspan - 1));
                }
            }
            column += colspan;
        }
        columns = columns.max(column.max(occupied.iter().map(|c| c + 1).max().unwrap_or(0)));
        rows.push(Row {
            node: row_node,
            group,
            cells: row,
        });
        // A reservation of N remaining rows must occupy the next N rows:
        // decrement after each row and keep entries until they hit zero.
        reserved = reserved
            .into_iter()
            .filter_map(|(column, left)| Some((column, left.checked_sub(1)?)))
            .collect();
    }
    (rows, columns)
}

/// Lays out a table's rows and cells. Returns the children (cells as
/// laid boxes) and the used content height.
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_table_children(
    document: &Document,
    styles: &StyleMap,
    table: NodeId,
    table_style: &ComputedStyle,
    content_x: f32,
    content_y: f32,
    content_width: f32,
    viewport: Size,
    measurer: &dyn TextMeasurer,
    images: &ImageMap,
    probe_cache: &ProbeCache,
    depth: usize,
) -> (Vec<LayoutBox>, f32) {
    let (rows, column_count) = collect_grid(document, styles, table);
    if column_count == 0 {
        return (Vec::new(), 0.0);
    }
    // Separated model: `border-spacing` (legacy default: `gap`, else
    // 2px). Collapsed model: zero spacing, single interior lines.
    let collapse = table_style.border_collapse;
    let (space_x, space_y) = if collapse {
        (0.0, 0.0)
    } else {
        table_style
            .border_spacing
            .unwrap_or_else(|| (table_style.gap.max(2.0), table_style.gap.max(2.0)))
    };
    let total_spacing = space_x * (column_count as f32 + 1.0);
    let available = (content_width - total_spacing).max(0.0);

    // Column preferences: the widest cell preference per column
    // (explicit width, else a shrink-to-fit probe), split over colspans.
    let mut preferred = vec![0.0f32; column_count];
    // Columns are sized in border-box terms (that is how the cells are
    // laid out below), so a cell's own edges count toward its column.
    let preference_of = |cell: &Cell| {
        let style = styles.by_node.get(&cell.node).cloned().unwrap_or_default();
        let edges = style.border_width.left
            + style.border_width.right
            + style
                .padding
                .left
                .resolve(available, viewport)
                .unwrap_or(0.0)
            + style
                .padding
                .right
                .resolve(available, viewport)
                .unwrap_or(0.0);
        let content = style
            .width
            .resolve(available, viewport)
            .map(|width| match style.box_sizing {
                BoxSizing::BorderBox => (width - edges).max(0.0),
                BoxSizing::ContentBox => width,
            })
            .unwrap_or_else(|| {
                natural_content_width(
                    document,
                    styles,
                    cell.node,
                    available,
                    viewport,
                    measurer,
                    images,
                    probe_cache,
                    depth,
                )
            });
        (content + edges).min(available)
    };
    // Single-column cells set each column's preference outright.
    let cells = || rows.iter().flat_map(|row| row.cells.iter());
    for cell in cells().filter(|cell| cell.colspan == 1) {
        preferred[cell.column] = preferred[cell.column].max(preference_of(cell));
    }
    // A spanning cell only widens its columns if they cannot already hold
    // it, and the extra goes to them in proportion to what they ask for
    // on their own — a wide column stays the wide one.
    for cell in cells().filter(|cell| cell.colspan > 1) {
        let end = (cell.column + cell.colspan).min(column_count);
        let columns = &mut preferred[cell.column..end];
        let spanned = columns.len() as f32;
        let gaps = space_x * (spanned - 1.0);
        let current: f32 = columns.iter().sum();
        let deficit = preference_of(cell) - gaps - current;
        if deficit <= 0.0 {
            continue;
        }
        let share = |width: f32| {
            if current > 0.0 {
                width / current
            } else {
                1.0 / spanned
            }
        };
        for width in columns.iter_mut() {
            *width += deficit * share(*width);
        }
    }
    for width in &mut preferred {
        *width = width.max(4.0);
    }

    // Fit: scale down on overflow; stretch when the table width is
    // explicit and columns underuse it.
    let total_preferred: f32 = preferred.iter().sum();
    let explicit_table = !matches!(table_style.width, Dimension::Auto);
    if total_preferred > available || (explicit_table && total_preferred < available) {
        let scale = if total_preferred > 0.0 {
            available / total_preferred
        } else {
            1.0
        };
        for width in &mut preferred {
            *width *= scale;
        }
    }

    // Column x offsets.
    let mut offsets = Vec::with_capacity(column_count);
    let mut x = space_x;
    for width in &preferred {
        offsets.push(x);
        x += width + space_x;
    }

    // Lay rows: each cell as an isolated block at its column width; the
    // row height is the tallest cell.
    let mut children: Vec<LayoutBox> = Vec::new();
    let mut cursor_y = content_y + space_y;
    for (row_index, row) in rows.iter().enumerate() {
        let mut row_height = 0.0f32;
        let mut laid_row: Vec<LayoutBox> = Vec::new();
        for cell in &row.cells {
            let mut style = styles.by_node.get(&cell.node).cloned().unwrap_or_default();
            let end = (cell.column + cell.colspan).min(column_count);
            let span_width: f32 = preferred[cell.column..end].iter().sum::<f32>()
                + space_x * (end - cell.column - 1) as f32;
            style.width = Dimension::Px(span_width);
            style.box_sizing = BoxSizing::BorderBox;
            if collapse {
                // Collapsed model, naive: interior borders merge by
                // dropping each cell's top/left border except along the
                // table's own top/left edge.
                if row_index > 0 {
                    style.border_width.top = 0.0;
                }
                if cell.column > 0 {
                    style.border_width.left = 0.0;
                }
            }
            let mut laid = layout_isolated_with_style(
                document,
                styles,
                cell.node,
                style,
                span_width,
                viewport,
                measurer,
                images,
                probe_cache,
                depth,
            );
            let margin_box = laid.margin_box();
            laid.translate(
                content_x + offsets[cell.column] - margin_box.x,
                cursor_y - margin_box.y,
            );
            if cell.rowspan == 1 {
                row_height = row_height.max(laid.margin_box().height);
            }
            laid_row.push(laid);
        }
        if row_height == 0.0 {
            row_height = laid_row
                .iter()
                .map(|laid| laid.margin_box().height)
                .fold(0.0, f32::max);
        }
        // The row is a box of its own, so a `<tr>` can carry a
        // background behind the cells sitting in it.
        let row_right = offsets
            .last()
            .zip(preferred.last())
            .map_or(space_x, |(offset, width)| offset + width);
        let row_box = LayoutBox {
            node_id: row.node,
            box_type: BoxType::Block,
            kind: LayoutKind::Element(
                document
                    .element(row.node)
                    .map_or_else(|| "tr".to_string(), |element| element.tag_name.clone()),
            ),
            dimensions: Dimensions {
                content: Rect {
                    x: content_x + space_x,
                    y: cursor_y,
                    width: (row_right - space_x).max(0.0),
                    height: row_height,
                },
                ..Dimensions::default()
            },
            style: styles.by_node.get(&row.node).cloned().unwrap_or_default(),
            children: laid_row,
        };
        push_row(document, styles, &mut children, row.group, row_box);
        cursor_y += row_height + space_y;
    }

    // Non-row children (e.g. <caption>) are ignored unless they are rows
    // (documented simplification).
    let used_height = cursor_y - content_y;
    (children, used_height)
}

/// Adds a laid-out row to the table, inside a box for its
/// `<thead>`/`<tbody>`/`<tfoot>` when it has one. The group box grows to
/// span the rows it holds, so it can carry a background behind them; the
/// border spacing around the group stays outside it.
fn push_row(
    document: &Document,
    styles: &StyleMap,
    children: &mut Vec<LayoutBox>,
    group: Option<NodeId>,
    row: LayoutBox,
) {
    let Some(group) = group else {
        children.push(row);
        return;
    };
    if let Some(open) = children.last_mut()
        && open.node_id == group
    {
        let row_box = row.content_box();
        open.dimensions.content.height = row_box.y + row_box.height - open.content_box().y;
        open.children.push(row);
        return;
    }
    children.push(LayoutBox {
        node_id: group,
        box_type: BoxType::Block,
        kind: LayoutKind::Element(
            document
                .element(group)
                .map_or_else(|| "tbody".to_string(), |element| element.tag_name.clone()),
        ),
        dimensions: Dimensions {
            content: row.content_box(),
            ..Dimensions::default()
        },
        style: styles.by_node.get(&group).cloned().unwrap_or_default(),
        children: vec![row],
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_html::parse_document;

    #[test]
    fn grid_resolves_col_and_row_spans() {
        let document = parse_document(
            "<table><tr><td>a</td><td colspan='2'>b</td></tr>\
             <tr><td rowspan='2'>c</td><td>d</td><td>e</td></tr>\
             <tr><td>f</td><td>g</td></tr></table>",
        );
        let table = document
            .descendants(document.root())
            .find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "table")
            })
            .unwrap();
        let styles = crate::style::compute_styles(&document, &lumen_css::Stylesheet::default());
        let (rows, columns) = collect_grid(&document, &styles, table);
        assert_eq!(columns, 3);
        assert_eq!(rows[0].cells.len(), 2);
        assert_eq!(rows[0].cells[1].colspan, 2);
        // Row 3's first cell starts at column 1 (column 0 reserved by the
        // rowspan above).
        assert_eq!(rows[2].cells[0].column, 1);
    }

    #[test]
    fn row_groups_get_a_box_spanning_their_rows() {
        let page = crate::build_page(
            &crate::test_support::with_body_reset(
                "<style>td { padding: 0; height: 10px; } table { border-spacing: 0; }</style>\
                 <table><thead><tr><td>h</td></tr></thead>\
                 <tbody><tr><td>a</td></tr><tr><td>b</td></tr></tbody></table>",
            ),
            crate::geometry::Size {
                width: 400.0,
                height: 200.0,
            },
        );
        fn groups(layout: &crate::LayoutBox, found: &mut Vec<crate::LayoutBox>) {
            if matches!(&layout.kind, crate::LayoutKind::Element(tag)
                if tag == "thead" || tag == "tbody" || tag == "tfoot")
            {
                found.push(layout.clone());
            }
            for child in &layout.children {
                groups(child, found);
            }
        }
        let mut found = Vec::new();
        groups(&page.layout, &mut found);
        let tags: Vec<String> = found
            .iter()
            .map(|group| match &group.kind {
                crate::LayoutKind::Element(tag) => tag.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(tags, vec!["thead", "tbody"]);
        assert_eq!(found[0].children.len(), 1);
        assert_eq!(found[1].children.len(), 2);
        // The group spans exactly the rows it holds.
        let body_group = found[1].content_box();
        let last = found[1].children[1].content_box();
        assert_eq!(body_group.y, found[1].children[0].content_box().y);
        assert_eq!(body_group.height, last.y + last.height - body_group.y);
    }

    /// Laid-out cell boxes (tag `td`/`th`) in paint order.
    fn cells_of(html: &str) -> Vec<crate::LayoutBox> {
        let page = crate::build_page(
            &crate::test_support::with_body_reset(html),
            crate::geometry::Size {
                width: 400.0,
                height: 200.0,
            },
        );
        fn collect(layout: &crate::LayoutBox, cells: &mut Vec<crate::LayoutBox>) {
            if matches!(&layout.kind, crate::LayoutKind::Element(tag) if tag == "td" || tag == "th")
            {
                cells.push(layout.clone());
            }
            for child in &layout.children {
                collect(child, cells);
            }
        }
        let mut cells = Vec::new();
        collect(&page.layout, &mut cells);
        cells
    }

    #[test]
    fn border_spacing_separates_cells() {
        let cells = cells_of(
            "<style>table { border-spacing: 10px 4px; }</style>\
             <table><tr><td>a</td><td>b</td></tr><tr><td>c</td><td>d</td></tr></table>",
        );
        assert_eq!(cells.len(), 4);
        // Horizontal spacing: the first cell starts 10px in, the second
        // follows another 10px gap.
        assert_eq!(cells[0].border_box().x, 10.0);
        let gap = cells[1].border_box().x - (cells[0].border_box().x + cells[0].border_box().width);
        assert_eq!(gap, 10.0);
        // Vertical spacing between rows.
        let row_gap =
            cells[2].border_box().y - (cells[0].border_box().y + cells[0].border_box().height);
        assert!((row_gap - 4.0).abs() < 0.01, "row_gap={row_gap}");
    }

    #[test]
    fn border_collapse_zeroes_spacing_and_merges_borders() {
        let cells = cells_of(
            "<style>table { border-collapse: collapse; } td { border: 2px solid #111111; }</style>\
             <table><tr><td>a</td><td>b</td></tr><tr><td>c</td><td>d</td></tr></table>",
        );
        assert_eq!(cells.len(), 4);
        // No spacing: the first cell starts at the table's content edge
        // and the next cell touches (or overlaps) it — no double line.
        assert_eq!(cells[0].border_box().x, 0.0);
        let gap = cells[1].border_box().x - (cells[0].border_box().x + cells[0].border_box().width);
        assert!(gap <= 0.0, "gap={gap}");
        // Interior borders collapse to a single line: top/left drop
        // except along the table's own top/left edge.
        assert_eq!(cells[0].style.border_width.left, 2.0);
        assert_eq!(cells[0].style.border_width.top, 2.0);
        assert_eq!(cells[1].style.border_width.left, 0.0);
        assert_eq!(cells[1].style.border_width.top, 2.0);
        assert_eq!(cells[2].style.border_width.top, 0.0);
        assert_eq!(cells[3].style.border_width.left, 0.0);
        assert_eq!(cells[3].style.border_width.top, 0.0);
    }
}
