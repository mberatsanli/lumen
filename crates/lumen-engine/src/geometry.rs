//! Shared geometry primitives.

/// Viewport or content size in CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Per-edge values (margins, paddings, border widths, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EdgeSizes<T> {
    pub top: T,
    pub right: T,
    pub bottom: T,
    pub left: T,
}

impl<T: Copy> EdgeSizes<T> {
    pub const fn uniform(value: T) -> Self {
        Self {
            top: value,
            right: value,
            bottom: value,
            left: value,
        }
    }
}

/// Resolved per-edge pixel values.
pub type Edges = EdgeSizes<f32>;
