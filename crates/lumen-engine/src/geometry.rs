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

impl Rect {
    /// This rectangle grown outward by `edges`.
    #[must_use]
    pub fn expanded_by(&self, edges: Edges) -> Self {
        Self {
            x: self.x - edges.left,
            y: self.y - edges.top,
            width: self.width + edges.left + edges.right,
            height: self.height + edges.top + edges.bottom,
        }
    }
}

/// The CSS box model for one laid-out box: a content rectangle wrapped by
/// padding, border and margin.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Dimensions {
    /// Position and size of the content area, in page coordinates.
    pub content: Rect,
    pub padding: Edges,
    pub border: Edges,
    pub margin: Edges,
}

impl Dimensions {
    /// Content plus padding.
    #[must_use]
    pub fn padding_box(&self) -> Rect {
        self.content.expanded_by(self.padding)
    }

    /// Content plus padding plus border — the visually painted area.
    #[must_use]
    pub fn border_box(&self) -> Rect {
        self.padding_box().expanded_by(self.border)
    }

    /// Border box plus margin — the space the box occupies in flow.
    #[must_use]
    pub fn margin_box(&self) -> Rect {
        self.border_box().expanded_by(self.margin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_helpers_expand_exactly() {
        let dimensions = Dimensions {
            content: Rect {
                x: 100.0,
                y: 50.0,
                width: 200.0,
                height: 80.0,
            },
            padding: EdgeSizes::uniform(10.0),
            border: EdgeSizes {
                top: 1.0,
                right: 2.0,
                bottom: 3.0,
                left: 4.0,
            },
            margin: EdgeSizes::uniform(5.0),
        };

        assert_eq!(
            dimensions.padding_box(),
            Rect {
                x: 90.0,
                y: 40.0,
                width: 220.0,
                height: 100.0
            }
        );
        assert_eq!(
            dimensions.border_box(),
            Rect {
                x: 86.0,
                y: 39.0,
                width: 226.0,
                height: 104.0
            }
        );
        assert_eq!(
            dimensions.margin_box(),
            Rect {
                x: 81.0,
                y: 34.0,
                width: 236.0,
                height: 114.0
            }
        );
    }
}
