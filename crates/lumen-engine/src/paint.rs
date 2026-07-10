//! Display-list generation from the layout tree.
//!
//! Paint order per box: background, border, then children (text is painted
//! where its own box appears in the tree).

use crate::geometry::{EdgeSizes, Rect};
use crate::image::{ImageMap, RasterImage};
use crate::layout::{BoxType, LayoutBox, LayoutKind};
use lumen_css::Color;
use std::sync::Arc;

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
        /// Baseline position.
        y: f32,
        text: String,
        color: Color,
        font_size: f32,
        font_weight: u16,
        underline: bool,
        italic: bool,
    },
    /// A decoded image scaled into `rect`.
    DrawImage {
        rect: Rect,
        image: Arc<RasterImage>,
    },
}

/// Flattens the layout tree into an ordered list of paint commands.
#[must_use]
pub fn build_display_list(layout: &LayoutBox, images: &ImageMap) -> Vec<DisplayCommand> {
    let mut commands = Vec::new();
    // Per CSS, the root element's background (or the body's, when the root
    // is transparent) paints the whole canvas, not just its own box.
    if let Some(color) = canvas_background(layout) {
        commands.push(DisplayCommand::FillRect {
            rect: layout.content_box(),
            color,
        });
    }
    paint_box(layout, images, &mut commands);
    commands
}

fn canvas_background(root: &LayoutBox) -> Option<Color> {
    let is_element = |layout: &&LayoutBox, name: &str| matches!(&layout.kind, LayoutKind::Element(tag) if tag == name);
    let html = root
        .children
        .iter()
        .find(|child| is_element(child, "html"))?;
    html.style.background_color.or_else(|| {
        html.children
            .iter()
            .find(|child| is_element(child, "body"))
            .and_then(|body| body.style.background_color)
    })
}

fn paint_box(layout: &LayoutBox, images: &ImageMap, commands: &mut Vec<DisplayCommand>) {
    let border_box = layout.border_box();

    // Anonymous blocks carry a clone of their container's style for text
    // defaults; the container already painted its own background/border.
    let anonymous = layout.box_type == BoxType::AnonymousBlock;

    if !anonymous && let Some(background) = layout.style.background_color {
        commands.push(DisplayCommand::FillRect {
            rect: border_box,
            color: background,
        });
    }

    let widths = layout.dimensions.border;
    if !anonymous
        && (widths.top > 0.0 || widths.right > 0.0 || widths.bottom > 0.0 || widths.left > 0.0)
    {
        commands.push(DisplayCommand::StrokeRect {
            rect: border_box,
            widths,
            color: layout.style.border_color,
        });
    }

    if layout.box_type == BoxType::Replaced {
        match images.get(&layout.node_id) {
            Some(image) => commands.push(DisplayCommand::DrawImage {
                rect: layout.content_box(),
                image: image.clone(),
            }),
            // Broken image: a thin gray placeholder frame.
            None => commands.push(DisplayCommand::StrokeRect {
                rect: border_box,
                widths: EdgeSizes::uniform(1.0),
                color: Color::rgb(0x80, 0x80, 0x80),
            }),
        }
    }

    if let LayoutKind::Inline { lines } = &layout.kind {
        let content = layout.content_box();
        for line in lines {
            for fragment in &line.fragments {
                match &fragment.content {
                    crate::inline::FragmentContent::Text { text, style } => {
                        commands.push(DisplayCommand::DrawText {
                            x: content.x + fragment.x,
                            y: content.y + line.y + line.baseline,
                            text: text.clone(),
                            color: style.color,
                            font_size: style.font_size,
                            font_weight: style.font_weight.0,
                            underline: style.underline,
                            italic: style.italic,
                        });
                    }
                    crate::inline::FragmentContent::Box(laid) => {
                        paint_box(laid, images, commands);
                    }
                }
            }
        }
    }

    for child in &layout.children {
        paint_box(child, images, commands);
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
                underline,
                italic,
            } => {
                let decoration = if *underline { " underline" } else { "" };
                let slant = if *italic { " italic" } else { "" };
                let _ = writeln!(
                    output,
                    "DrawText x={x} y={y} size={font_size} weight={font_weight} color={color}{decoration}{slant} {text:?}"
                );
            }
            DisplayCommand::DrawImage { rect, image } => {
                let _ = writeln!(
                    output,
                    "DrawImage x={} y={} w={} h={} intrinsic={}x{} {}",
                    rect.x, rect.y, rect.width, rect.height, image.width, image.height, image.mime
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
    fn body_background_propagates_to_the_canvas() {
        let list = commands(
            "<html><head><style>body { background-color: #eee; }</style></head>\
             <body><p>t</p></body></html>",
        );
        let Some(DisplayCommand::FillRect { rect, color }) = list.first() else {
            panic!("expected canvas fill first, got {list:?}");
        };
        assert_eq!(color.to_string(), "#eeeeee");
        // Covers the whole viewport, not just the body's box.
        assert_eq!(rect.width, 800.0);
        assert_eq!(rect.height, 600.0);
    }

    #[test]
    fn fragment_without_html_element_has_no_canvas_fill() {
        let list =
            commands("<style>div { background-color: #222; height: 5px; }</style><div></div>");
        assert_eq!(
            list.iter()
                .filter(|command| matches!(command, DisplayCommand::FillRect { .. }))
                .count(),
            1
        );
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
                DisplayCommand::DrawImage { .. } => "image",
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
