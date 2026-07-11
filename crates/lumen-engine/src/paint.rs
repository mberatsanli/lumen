//! Display-list generation from the layout tree.
//!
//! Paint order per box: background, border, then children (text is painted
//! where its own box appears in the tree).

use crate::geometry::{Corners, EdgeSizes, Rect};
use crate::image::{ImageMap, RasterImage};
use crate::layout::{BoxType, LayoutBox, LayoutKind};
use crate::style::{BackgroundImage, BackgroundSize, BorderStyle, Overflow};
use lumen_css::Color;
use std::sync::Arc;

/// A single backend-independent paint command.
#[derive(Debug, Clone, PartialEq)]
pub enum DisplayCommand {
    FillRect {
        rect: Rect,
        color: Color,
        /// Corner radii (zero = square).
        radius: Corners<f32>,
    },
    /// A border frame: `rect` is the border box, `widths` the per-edge
    /// thicknesses drawn inward from its edges, each with its own color.
    /// With non-zero `radius` the frame renders as a rounded ring in the
    /// top edge color at the top edge width (per-edge colors and widths
    /// apply to square borders only).
    StrokeRect {
        rect: Rect,
        widths: EdgeSizes<f32>,
        colors: EdgeSizes<Color>,
        /// Per-edge line styles; dashed/dotted apply to square borders.
        styles: EdgeSizes<BorderStyle>,
        radius: Corners<f32>,
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
        monospace: bool,
        /// Struck through (`text-decoration: line-through`).
        line_through: bool,
        /// Extra advance per character, px.
        letter_spacing: f32,
    },
    /// A decoded image scaled into `rect`, with an extra alpha multiplier
    /// (255 = opaque) from `opacity`.
    DrawImage {
        rect: Rect,
        image: Arc<RasterImage>,
        alpha: u8,
    },
    /// Clip all commands until the matching [`Self::PopClip`] to `rect`
    /// (intersected with any enclosing clips). From `overflow`.
    PushClip {
        rect: Rect,
    },
    PopClip,
    /// A linear gradient over `rect`. `angle_degrees` follows the CSS
    /// convention (0 = to top, 90 = to right); stops are normalized 0..=1.
    FillGradient {
        rect: Rect,
        radius: Corners<f32>,
        angle_degrees: f32,
        stops: Vec<(Color, f32)>,
        /// Radial (centered ellipse) instead of linear; angle ignored.
        radial: bool,
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
            radius: Corners::uniform(0.0),
        });
    }
    paint_box(layout, images, 1.0, &mut commands);
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

fn paint_box(
    layout: &LayoutBox,
    images: &ImageMap,
    parent_opacity: f32,
    commands: &mut Vec<DisplayCommand>,
) {
    // Anonymous blocks carry a clone of their container's style for text
    // defaults; the container already painted its own background/border
    // and applied its own opacity.
    let anonymous = layout.box_type == BoxType::AnonymousBlock;
    let opacity = if anonymous {
        parent_opacity
    } else {
        parent_opacity * layout.style.opacity
    };
    if opacity <= 0.0 {
        return;
    }
    let fade = |color: lumen_css::Color| color.with_alpha_factor(opacity);
    let border_box = layout.border_box();

    let radius = layout
        .style
        .border_radius
        .clamped_to(border_box.width, border_box.height);

    if !anonymous && let Some(background) = layout.style.background_color {
        commands.push(DisplayCommand::FillRect {
            rect: border_box,
            color: fade(background),
            radius,
        });
    }

    // The background image layer paints over the color. Gradients render
    // directly; url() images stretch over the border box when their bytes
    // were fetched (keyed by this node in the image map).
    if !anonymous {
        match &layout.style.background_image {
            Some(BackgroundImage::LinearGradient(gradient)) => {
                commands.push(DisplayCommand::FillGradient {
                    rect: border_box,
                    radius,
                    angle_degrees: gradient.angle_degrees,
                    stops: gradient
                        .stops
                        .iter()
                        .map(|(color, position)| (fade(*color), *position))
                        .collect(),
                    radial: false,
                });
            }
            Some(BackgroundImage::RadialGradient(stops)) => {
                commands.push(DisplayCommand::FillGradient {
                    rect: border_box,
                    radius,
                    angle_degrees: 0.0,
                    stops: stops
                        .iter()
                        .map(|(color, position)| (fade(*color), *position))
                        .collect(),
                    radial: true,
                });
            }
            Some(BackgroundImage::Url(_)) if layout.box_type != BoxType::Replaced => {
                if let Some(image) = images.get(&layout.node_id) {
                    paint_background_image(
                        &layout.style,
                        border_box,
                        image,
                        (opacity * 255.0) as u8,
                        commands,
                    );
                }
            }
            _ => {}
        }
    }

    let widths = layout.dimensions.border;
    if !anonymous
        && (widths.top > 0.0 || widths.right > 0.0 || widths.bottom > 0.0 || widths.left > 0.0)
    {
        let colors = layout.style.border_color;
        commands.push(DisplayCommand::StrokeRect {
            rect: border_box,
            widths,
            colors: EdgeSizes {
                top: fade(colors.top),
                right: fade(colors.right),
                bottom: fade(colors.bottom),
                left: fade(colors.left),
            },
            styles: layout.style.border_style,
            radius,
        });
    }

    if layout.box_type == BoxType::Replaced {
        match images.get(&layout.node_id) {
            Some(image) => commands.push(DisplayCommand::DrawImage {
                rect: layout.content_box(),
                image: image.clone(),
                alpha: (opacity * 255.0) as u8,
            }),
            // Broken image: a thin gray placeholder frame.
            None => commands.push(DisplayCommand::StrokeRect {
                rect: border_box,
                widths: EdgeSizes::uniform(1.0),
                colors: EdgeSizes::uniform(Color::rgb(0x80, 0x80, 0x80)),
                styles: EdgeSizes::uniform(BorderStyle::Solid),
                radius: Corners::uniform(0.0),
            }),
        }
    }

    // `overflow: hidden/scroll/auto/clip`: children (including inline
    // content) clip to the padding box; background and border stay intact.
    let clips = !anonymous && layout.style.overflow == Overflow::Clip;
    if clips {
        commands.push(DisplayCommand::PushClip {
            rect: layout.dimensions.padding_box(),
        });
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
                            color: fade(style.color),
                            font_size: style.font_size,
                            font_weight: style.font_weight.0,
                            underline: style.underline,
                            italic: style.italic,
                            monospace: style.monospace,
                            line_through: style.line_through,
                            letter_spacing: style.letter_spacing,
                        });
                    }
                    crate::inline::FragmentContent::Box(laid) => {
                        paint_box(laid, images, opacity, commands);
                    }
                }
            }
        }
    }

    for child in layout.children_in_paint_order() {
        paint_box(child, images, opacity, commands);
    }

    if clips {
        commands.push(DisplayCommand::PopClip);
    }
}

/// Emits a background image with position/size/repeat semantics: the
/// tile rect comes from `background-size`, its anchor from
/// `background-position` (percents place the image per CSS), and
/// `background-repeat` tiles it across the clipped border box.
fn paint_background_image(
    style: &crate::style::ComputedStyle,
    border_box: Rect,
    image: &Arc<RasterImage>,
    alpha: u8,
    commands: &mut Vec<DisplayCommand>,
) {
    if image.width == 0 || image.height == 0 {
        return;
    }
    let intrinsic = (image.width as f32, image.height as f32);
    let viewport = crate::geometry::Size::default(); // vw/vh unsupported here
    let (tile_width, tile_height) = match style.background_size {
        BackgroundSize::Auto => intrinsic,
        BackgroundSize::Cover => {
            let scale = (border_box.width / intrinsic.0).max(border_box.height / intrinsic.1);
            (intrinsic.0 * scale, intrinsic.1 * scale)
        }
        BackgroundSize::Contain => {
            let scale = (border_box.width / intrinsic.0).min(border_box.height / intrinsic.1);
            (intrinsic.0 * scale, intrinsic.1 * scale)
        }
        BackgroundSize::Explicit(width, height) => {
            let width = width.resolve(border_box.width, viewport);
            let height = height.resolve(border_box.height, viewport);
            match (width, height) {
                (Some(width), Some(height)) => (width, height),
                (Some(width), None) => (width, width * intrinsic.1 / intrinsic.0),
                (None, Some(height)) => (height * intrinsic.0 / intrinsic.1, height),
                (None, None) => intrinsic,
            }
        }
    };
    if tile_width <= 0.0 || tile_height <= 0.0 {
        return;
    }
    // Percents position the image per CSS: p% of the leftover space.
    let offset =
        |dimension: crate::style::Dimension, box_extent: f32, tile_extent: f32| match dimension {
            crate::style::Dimension::Percent(percent) => {
                (box_extent - tile_extent) * percent / 100.0
            }
            other => other.resolve(box_extent, viewport).unwrap_or(0.0),
        };
    let anchor_x = border_box.x + offset(style.background_position.0, border_box.width, tile_width);
    let anchor_y =
        border_box.y + offset(style.background_position.1, border_box.height, tile_height);
    let (repeat_x, repeat_y) = style.background_repeat;

    // Tile from the first tile at/before each edge to past the far edge.
    let mut tiles: Vec<(f32, f32)> = Vec::new();
    let first_x = if repeat_x {
        anchor_x - ((anchor_x - border_box.x) / tile_width).ceil() * tile_width
    } else {
        anchor_x
    };
    let first_y = if repeat_y {
        anchor_y - ((anchor_y - border_box.y) / tile_height).ceil() * tile_height
    } else {
        anchor_y
    };
    let mut y = first_y;
    loop {
        let mut x = first_x;
        loop {
            tiles.push((x, y));
            if !repeat_x || tiles.len() >= 1024 {
                break;
            }
            x += tile_width;
            if x >= border_box.x + border_box.width {
                break;
            }
        }
        if !repeat_y || tiles.len() >= 1024 {
            break;
        }
        y += tile_height;
        if y >= border_box.y + border_box.height {
            break;
        }
    }

    commands.push(DisplayCommand::PushClip { rect: border_box });
    for (x, y) in tiles {
        commands.push(DisplayCommand::DrawImage {
            rect: Rect {
                x,
                y,
                width: tile_width,
                height: tile_height,
            },
            image: image.clone(),
            alpha,
        });
    }
    commands.push(DisplayCommand::PopClip);
}

/// One paint command per line — for debugging and CLI inspection.
#[must_use]
pub fn dump_display_list(commands: &[DisplayCommand]) -> String {
    use std::fmt::Write as _;
    let mut output = String::new();
    for command in commands {
        match command {
            DisplayCommand::FillRect {
                rect,
                color,
                radius,
            } => {
                let rounded = if radius.is_zero() {
                    String::new()
                } else {
                    format!(" radius={}", radius.top_left)
                };
                let _ = writeln!(
                    output,
                    "FillRect x={} y={} w={} h={} color={color}{rounded}",
                    rect.x, rect.y, rect.width, rect.height
                );
            }
            DisplayCommand::StrokeRect {
                rect,
                widths,
                colors,
                styles: _,
                radius: _,
            } => {
                let _ = writeln!(
                    output,
                    "StrokeRect x={} y={} w={} h={} widths={}/{}/{}/{} colors={}/{}/{}/{}",
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                    widths.top,
                    widths.right,
                    widths.bottom,
                    widths.left,
                    colors.top,
                    colors.right,
                    colors.bottom,
                    colors.left
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
                monospace,
                line_through,
                ..
            } => {
                let strike = if *line_through { " line-through" } else { "" };
                let decoration = if *underline { " underline" } else { "" };
                let slant = if *italic { " italic" } else { "" };
                let face = if *monospace { " mono" } else { "" };
                let _ = writeln!(
                    output,
                    "DrawText x={x} y={y} size={font_size} weight={font_weight} color={color}{decoration}{strike}{slant}{face} {text:?}"
                );
            }
            DisplayCommand::DrawImage { rect, image, alpha } => {
                let _ = writeln!(
                    output,
                    "DrawImage x={} y={} w={} h={} intrinsic={}x{} {} alpha={alpha}",
                    rect.x, rect.y, rect.width, rect.height, image.width, image.height, image.mime
                );
            }
            DisplayCommand::PushClip { rect } => {
                let _ = writeln!(
                    output,
                    "PushClip x={} y={} w={} h={}",
                    rect.x, rect.y, rect.width, rect.height
                );
            }
            DisplayCommand::PopClip => {
                let _ = writeln!(output, "PopClip");
            }
            DisplayCommand::FillGradient {
                rect,
                angle_degrees,
                stops,
                ..
            } => {
                let stop_list: Vec<String> = stops
                    .iter()
                    .map(|(color, position)| format!("{color}@{position}"))
                    .collect();
                let _ = writeln!(
                    output,
                    "FillGradient x={} y={} w={} h={} angle={angle_degrees} stops={}",
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                    stop_list.join(",")
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
        let Some(DisplayCommand::FillRect { rect, color, .. }) = list.first() else {
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
                DisplayCommand::PushClip { .. } => "push-clip",
                DisplayCommand::PopClip => "pop-clip",
                DisplayCommand::FillGradient { .. } => "gradient",
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
    fn overflow_hidden_clips_children_to_the_padding_box() {
        let list = commands(
            "<style>.clip { overflow: hidden; width: 100px; height: 40px; padding: 5px; }\
             </style><div class='clip'><div style='width: 500px; height: 500px;'></div></div>",
        );
        let Some(DisplayCommand::PushClip { rect }) = list
            .iter()
            .find(|command| matches!(command, DisplayCommand::PushClip { .. }))
        else {
            panic!("no PushClip in {list:?}");
        };
        // Padding box: 100 + 2*5 wide, 40 + 2*5 tall.
        assert_eq!(rect.width, 110.0);
        assert_eq!(rect.height, 50.0);
        assert!(
            list.iter()
                .any(|command| matches!(command, DisplayCommand::PopClip))
        );
        // The clip opens before the child's background paints.
        let push = list
            .iter()
            .position(|command| matches!(command, DisplayCommand::PushClip { .. }))
            .unwrap();
        let pop = list
            .iter()
            .position(|command| matches!(command, DisplayCommand::PopClip))
            .unwrap();
        assert!(push < pop);
    }

    #[test]
    fn background_position_offsets_the_clipped_image() {
        use crate::image::RasterImage;
        use std::sync::Arc;
        // A 10x10 image positioned at -20px -30px in a no-repeat box:
        // exactly one DrawImage, anchored at box - offset, clipped.
        let mut style = crate::style::ComputedStyle {
            background_image: Some(crate::style::BackgroundImage::Url("s.png".to_string())),
            background_repeat: (false, false),
            ..Default::default()
        };
        style.background_position = (
            crate::style::Dimension::Px(-20.0),
            crate::style::Dimension::Px(-30.0),
        );
        let image = Arc::new(RasterImage {
            width: 10,
            height: 10,
            rgba: vec![0; 400],
            encoded: Vec::new(),
            mime: "image/png",
        });
        let mut commands = Vec::new();
        paint_background_image(
            &style,
            Rect {
                x: 100.0,
                y: 50.0,
                width: 40.0,
                height: 20.0,
            },
            &image,
            255,
            &mut commands,
        );
        let kinds: Vec<&str> = commands
            .iter()
            .map(|command| match command {
                DisplayCommand::PushClip { .. } => "push",
                DisplayCommand::DrawImage { .. } => "image",
                DisplayCommand::PopClip => "pop",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, vec!["push", "image", "pop"]);
        let Some(DisplayCommand::DrawImage { rect, .. }) = commands.get(1) else {
            panic!();
        };
        assert_eq!((rect.x, rect.y), (80.0, 20.0));
        assert_eq!((rect.width, rect.height), (10.0, 10.0));
    }

    #[test]
    fn repeating_backgrounds_tile_across_the_box() {
        use crate::image::RasterImage;
        use std::sync::Arc;
        let style = crate::style::ComputedStyle {
            background_image: Some(crate::style::BackgroundImage::Url("t.png".to_string())),
            ..Default::default()
        };
        let image = Arc::new(RasterImage {
            width: 10,
            height: 10,
            rgba: vec![0; 400],
            encoded: Vec::new(),
            mime: "image/png",
        });
        let mut commands = Vec::new();
        paint_background_image(
            &style,
            Rect {
                x: 0.0,
                y: 0.0,
                width: 35.0,
                height: 15.0,
            },
            &image,
            255,
            &mut commands,
        );
        let images = commands
            .iter()
            .filter(|command| matches!(command, DisplayCommand::DrawImage { .. }))
            .count();
        // 4 columns x 2 rows.
        assert_eq!(images, 8);
    }

    #[test]
    fn opacity_fades_the_subtree() {
        let list = commands(
            "<style>.half { opacity: 0.5; background-color: #ff0000; height: 10px; }</style>\
             <div class='half'>hi</div>",
        );
        let Some(DisplayCommand::FillRect { color, .. }) = list.iter().find(
            |command| matches!(command, DisplayCommand::FillRect { color, .. } if color.r == 255),
        ) else {
            panic!("no faded fill in {list:?}");
        };
        assert_eq!(color.a, 127);
        let text_alpha = list.iter().find_map(|command| match command {
            DisplayCommand::DrawText { color, .. } => Some(color.a),
            _ => None,
        });
        assert_eq!(text_alpha, Some(127));
    }

    #[test]
    fn border_edges_carry_their_own_colors() {
        let list = commands(
            "<style>div { border: 2px solid #111111; border-left-color: #222222; height: 5px; }\
             </style><div></div>",
        );
        let Some(DisplayCommand::StrokeRect { colors, .. }) = list
            .iter()
            .find(|command| matches!(command, DisplayCommand::StrokeRect { .. }))
        else {
            panic!("no StrokeRect in {list:?}");
        };
        assert_eq!(colors.top.to_string(), "#111111");
        assert_eq!(colors.left.to_string(), "#222222");
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
