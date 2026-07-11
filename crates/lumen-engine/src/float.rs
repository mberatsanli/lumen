//! Simplified float model: per-container float tracking that narrows
//! inline line boxes (see `layout.rs` for placement and `clear`).

use crate::geometry::Rect;
use crate::style::Clear;

/// Active floats of one block container (margin boxes, page coordinates).
#[derive(Debug, Default)]
pub(crate) struct FloatContext {
    pub(crate) left: Vec<Rect>,
    pub(crate) right: Vec<Rect>,
}

impl FloatContext {
    /// Usable `(indent, width)` inside `[content_x, content_x+width)` for a
    /// line starting at absolute `y`.
    pub(crate) fn bounds_at(&self, content_x: f32, content_width: f32, y: f32) -> (f32, f32) {
        let intersects = |rect: &&Rect| y >= rect.y && y < rect.y + rect.height;
        let left_edge = self
            .left
            .iter()
            .filter(intersects)
            .map(|rect| rect.x + rect.width)
            .fold(content_x, f32::max);
        let right_edge = self
            .right
            .iter()
            .filter(intersects)
            .map(|rect| rect.x)
            .fold(content_x + content_width, f32::min);
        let indent = left_edge - content_x;
        (indent, (right_edge - left_edge).max(0.0))
    }

    /// The lowest bottom edge of the given side(s); `y` when none.
    pub(crate) fn clearance(&self, clear: Clear, y: f32) -> f32 {
        let bottom = |rects: &[Rect]| {
            rects
                .iter()
                .map(|rect| rect.y + rect.height)
                .fold(y, f32::max)
        };
        match clear {
            Clear::None => y,
            Clear::Left => bottom(&self.left),
            Clear::Right => bottom(&self.right),
            Clear::Both => bottom(&self.left).max(bottom(&self.right)),
        }
    }

    pub(crate) fn lowest_bottom(&self) -> f32 {
        self.left
            .iter()
            .chain(&self.right)
            .map(|rect| rect.y + rect.height)
            .fold(0.0, f32::max)
    }
}
