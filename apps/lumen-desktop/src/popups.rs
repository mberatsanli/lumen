//! Shell-drawn popup state: the `<select>` dropdown card and the color
//! input's swatch palette, plus their shared geometry helpers.

use lumen_engine::Rect;

/// Row height of the shell-drawn select dropdown, CSS px.
pub(crate) const SELECT_ROW_HEIGHT: f32 = 22.0;

/// An open `<select>` dropdown drawn by the shell.
pub(crate) struct SelectPopup {
    pub(crate) node: usize,
    pub(crate) options: Vec<(String, String)>,
    /// Page coordinates of the option list.
    pub(crate) rect: Rect,
    pub(crate) hovered: usize,
    pub(crate) selected: usize,
}

/// An open color-input palette drawn by the shell.
pub(crate) struct ColorPopup {
    pub(crate) node: usize,
    /// Page coordinates of the palette card.
    pub(crate) rect: Rect,
    pub(crate) hovered: Option<usize>,
}

/// The preset swatches a color input offers.
pub(crate) const COLOR_SWATCHES: [&str; 24] = [
    "#000000", "#444444", "#888888", "#cccccc", "#ffffff", "#7a1712", "#b3261e", "#e8590c",
    "#f2b705", "#f7e26b", "#2e7d32", "#1a936f", "#7fc8a9", "#1c5288", "#2266aa", "#6aa5d8",
    "#5e35b1", "#9b6bd3", "#d63384", "#f2a6c8", "#8d6e63", "#b9a08c", "#55524c", "#8a8272",
];
pub(crate) const SWATCH_SIZE: f32 = 22.0;
pub(crate) const SWATCH_GAP: f32 = 6.0;
pub(crate) const SWATCH_COLUMNS: usize = 6;

/// Where swatch `index` sits inside a palette card.
pub(crate) fn swatch_rect(card: Rect, index: usize) -> Rect {
    let column = index % SWATCH_COLUMNS;
    let row = index / SWATCH_COLUMNS;
    Rect {
        x: card.x + 8.0 + (SWATCH_SIZE + SWATCH_GAP) * column as f32,
        y: card.y + 8.0 + (SWATCH_SIZE + SWATCH_GAP) * row as f32,
        width: SWATCH_SIZE,
        height: SWATCH_SIZE,
    }
}

/// Whether a point falls inside a rect.
pub(crate) fn rect_contains(rect: Rect, x: f32, y: f32) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}
