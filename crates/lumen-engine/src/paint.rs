//! Display-list generation from the layout tree.
//!
//! Paint order per box: background, then children (text is painted where
//! its own box appears in the tree). Borders arrive with the box-model
//! milestone.

use crate::geometry::Rect;
use crate::layout::{LayoutBox, LayoutKind};
use lumen_css::Color;

/// A single backend-independent paint command.
#[derive(Debug, Clone, PartialEq)]
pub enum DisplayCommand {
    FillRect {
        rect: Rect,
        color: Color,
    },
    DrawText {
        x: f32,
        /// Baseline position, approximated as top + font size.
        y: f32,
        text: String,
        color: Color,
        font_size: f32,
        font_weight: u16,
    },
}

/// Flattens the layout tree into an ordered list of paint commands.
#[must_use]
pub fn build_display_list(layout: &LayoutBox) -> Vec<DisplayCommand> {
    let mut commands = Vec::new();
    paint_box(layout, &mut commands);
    commands
}

fn paint_box(layout: &LayoutBox, commands: &mut Vec<DisplayCommand>) {
    if let Some(background) = layout.style.background_color {
        commands.push(DisplayCommand::FillRect {
            rect: layout.rect,
            color: background,
        });
    }

    if let LayoutKind::Text(text) = &layout.kind {
        commands.push(DisplayCommand::DrawText {
            x: layout.rect.x,
            y: layout.rect.y + layout.style.font_size,
            text: text.clone(),
            color: layout.style.color,
            font_size: layout.style.font_size,
            font_weight: layout.style.font_weight.0,
        });
    }

    for child in &layout.children {
        paint_box(child, commands);
    }
}
