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

/// An open form-validation bubble drawn by the shell.
pub(crate) struct ViolationPopup {
    pub(crate) node: usize,
    pub(crate) message: String,
    /// Page coordinates of the bubble card.
    pub(crate) rect: Rect,
}

/// Horizontal/vertical margins around the bubble card.
const VIOLATION_MARGIN: f32 = 4.0;

/// The validation bubble's card geometry: below the violating control
/// sized to its text and clamped so the card never
/// leaves the page horizontally.
pub(crate) fn violation_rect(control: Rect, text_width: f32, page_width: f32) -> Rect {
    let width = (text_width + 24.0)
        .clamp(96.0, 320.0)
        .min((page_width - 2.0 * VIOLATION_MARGIN).max(24.0));
    let x = control.x.clamp(
        VIOLATION_MARGIN,
        (page_width - width - VIOLATION_MARGIN).max(VIOLATION_MARGIN),
    );
    Rect {
        x,
        y: control.y + control.height + VIOLATION_MARGIN,
        width,
        height: 30.0,
    }
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

#[cfg(test)]
mod tests {
    use super::{Rect, violation_rect};

    fn control(x: f32) -> Rect {
        Rect {
            x,
            y: 40.0,
            width: 120.0,
            height: 22.0,
        }
    }

    #[test]
    fn violation_bubble_sits_below_the_control() {
        let rect = violation_rect(control(100.0), 200.0, 800.0);
        assert_eq!(rect.y, 40.0 + 22.0 + 4.0);
        // No clamping needed: the card follows the control's left edge.
        assert_eq!(rect.x, 100.0);
        assert_eq!(rect.width, 224.0);
    }

    #[test]
    fn violation_bubble_clamps_at_the_page_edges() {
        // A control near the right edge: the card slides back into view.
        let rect = violation_rect(control(900.0), 200.0, 800.0);
        assert_eq!(rect.x + rect.width, 800.0 - 4.0);
        // A control (partly) off the left edge: the card pins to the margin.
        let rect = violation_rect(control(-50.0), 200.0, 800.0);
        assert_eq!(rect.x, 4.0);
    }

    #[test]
    fn violation_bubble_fits_a_narrow_page() {
        // Viewport narrower than the card: the card itself shrinks to fit.
        let rect = violation_rect(control(60.0), 500.0, 150.0);
        assert_eq!(rect.x, 4.0);
        assert!(rect.x + rect.width <= 150.0 - 4.0 + f32::EPSILON);
        // Long text is capped at the card's maximum width.
        let rect = violation_rect(control(0.0), 900.0, 1200.0);
        assert_eq!(rect.width, 320.0);
    }
}
