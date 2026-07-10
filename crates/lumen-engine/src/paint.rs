//! Display-list generation from the layout tree.
//!
//! Paint order per box: background, border, then children (text is painted
//! where its own box appears in the tree).

use crate::geometry::{EdgeSizes, Rect};
use crate::layout::{LayoutBox, LayoutKind};
use crate::style::TextAlign;
use lumen_css::Color;

/// A single backend-independent paint command.
#[derive(Debug, Clone, PartialEq)]
pub enum DisplayCommand {
    FillRect {
        rect: Rect,
        color: Color,
    },
    /// A border frame: `rect` is the border box, `widths` the per-edge
    /// thicknesses drawn inward from its edges.
    StrokeRect {
        rect: Rect,
        widths: EdgeSizes<f32>,
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
    let border_box = layout.border_box();

    if let Some(background) = layout.style.background_color {
        commands.push(DisplayCommand::FillRect {
            rect: border_box,
            color: background,
        });
    }

    let widths = layout.dimensions.border;
    if widths.top > 0.0 || widths.right > 0.0 || widths.bottom > 0.0 || widths.left > 0.0 {
        commands.push(DisplayCommand::StrokeRect {
            rect: border_box,
            widths,
            color: layout.style.border_color,
        });
    }

    if let LayoutKind::Text { lines } = &layout.kind {
        let content = layout.content_box();
        for (index, line) in lines.iter().enumerate() {
            let x = match layout.style.text_align {
                TextAlign::Left => content.x,
                TextAlign::Center => content.x + (content.width - line.width) / 2.0,
                TextAlign::Right => content.x + content.width - line.width,
            };
            commands.push(DisplayCommand::DrawText {
                x,
                y: content.y + index as f32 * layout.style.line_height + layout.style.font_size,
                text: line.text.clone(),
                color: layout.style.color,
                font_size: layout.style.font_size,
                font_weight: layout.style.font_weight.0,
            });
        }
    }

    for child in &layout.children {
        paint_box(child, commands);
    }
}

/// One paint command per line — for debugging and CLI inspection.
#[must_use]
pub fn dump_display_list(commands: &[DisplayCommand]) -> String {
    use std::fmt::Write as _;
    let mut output = String::new();
    for command in commands {
        match command {
            DisplayCommand::FillRect { rect, color } => {
                let _ = writeln!(
                    output,
                    "FillRect x={} y={} w={} h={} color={color}",
                    rect.x, rect.y, rect.width, rect.height
                );
            }
            DisplayCommand::StrokeRect {
                rect,
                widths,
                color,
            } => {
                let _ = writeln!(
                    output,
                    "StrokeRect x={} y={} w={} h={} widths={}/{}/{}/{} color={color}",
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                    widths.top,
                    widths.right,
                    widths.bottom,
                    widths.left
                );
            }
            DisplayCommand::DrawText {
                x,
                y,
                text,
                color,
                font_size,
                font_weight,
            } => {
                let _ = writeln!(
                    output,
                    "DrawText x={x} y={y} size={font_size} weight={font_weight} color={color} {text:?}"
                );
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_page;
    use crate::geometry::Size;

    fn commands(html: &str) -> Vec<DisplayCommand> {
        build_page(
            html,
            Size {
                width: 800.0,
                height: 600.0,
            },
        )
        .display_list
    }

    #[test]
    fn background_precedes_border_precedes_text() {
        let list = commands(
            "<style>div { background-color: #eee; border-width: 1px; }</style><div>hi</div>",
        );
        let kinds: Vec<&str> = list
            .iter()
            .map(|command| match command {
                DisplayCommand::FillRect { .. } => "fill",
                DisplayCommand::StrokeRect { .. } => "stroke",
                DisplayCommand::DrawText { .. } => "text",
            })
            .collect();
        assert_eq!(kinds, vec!["fill", "stroke", "text"]);
    }

    #[test]
    fn no_border_command_without_border_width() {
        let list = commands("<style>div { background-color: #eee; }</style><div></div>");
        assert!(
            !list
                .iter()
                .any(|command| matches!(command, DisplayCommand::StrokeRect { .. }))
        );
    }

    #[test]
    fn border_command_covers_border_box() {
        let list = commands(
            "<style>div { width: 100px; height: 10px; border-width: 2px; }</style><div></div>",
        );
        let Some(DisplayCommand::StrokeRect { rect, widths, .. }) = list
            .iter()
            .find(|command| matches!(command, DisplayCommand::StrokeRect { .. }))
        else {
            panic!("no StrokeRect in {list:?}");
        };
        assert_eq!(rect.width, 104.0);
        assert_eq!(rect.height, 14.0);
        assert_eq!(widths.top, 2.0);
    }

    #[test]
    fn parent_background_painted_before_child_background() {
        let list = commands(
            "<style>.a { background-color: #111111; } .b { background-color: #222222; }</style>\
             <div class='a'><div class='b'></div></div>",
        );
        let fills: Vec<String> = list
            .iter()
            .filter_map(|command| match command {
                DisplayCommand::FillRect { color, .. } => Some(color.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(fills, vec!["#111111", "#222222"]);
    }
}
