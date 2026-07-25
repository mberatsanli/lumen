//! Display-list generation from the layout tree.
//!
//! Paint order per box: background, border, then children (text is painted
//! where its own box appears in the tree).

use crate::geometry::{Corners, EdgeSizes, Rect};
use crate::image::{ImageMap, RasterImage};
use crate::layout::{BoxType, LayoutBox, LayoutKind};
use crate::style::{
    BackgroundBox, BackgroundImage, BackgroundLayer, BackgroundSize, BorderStyle, FilterFunction,
    Mark, Position, Transform2D,
};
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
        /// Resolved decoration line color (defaults to the text color).
        decoration_color: Color,
        /// Solid/dashed/dotted decoration lines.
        decoration_style: BorderStyle,
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
        /// The gradient geometry; the angle only applies to `Linear`.
        kind: GradientKind,
    },
    /// A vector control mark (check tick / radio dot) inside `rect`.
    DrawMark {
        rect: Rect,
        color: Color,
        mark: Mark,
    },
    /// Apply a 2D affine transform to every command until the matching
    /// [`Self::PopTransform`] (composed with enclosing transforms).
    PushTransform {
        matrix: Transform2D,
    },
    PopTransform,
    /// A box shadow with a Gaussian falloff. `rect` is the shadow's own
    /// box (offset and spread already applied); `blur` is the CSS blur
    /// radius (≈ 2σ). Inset shadows shade inward from `rect` instead.
    DrawShadow {
        rect: Rect,
        radius: Corners<f32>,
        blur: f32,
        color: Color,
        inset: bool,
    },
    /// Applies the element's `filter` list to everything painted until
    /// the matching [`Self::PopFilter`]. `rect` is the border box
    /// (blur-expanded); the raster backend rewrites that pixel region,
    /// the SVG backend ignores filters (documented gap).
    PushFilter {
        rect: Rect,
        filters: Vec<FilterFunction>,
    },
    PopFilter,
    /// Everything until the matching [`Self::PopFixed`] is
    /// `position: fixed` content: the raster backend must NOT apply the
    /// page scroll offset to it (it is viewport-relative). The SVG
    /// backend renders without scroll, so it treats these as no-ops.
    PushFixed,
    PopFixed,
}

/// How a gradient sweeps its box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GradientKind {
    Linear,
    /// Centered ellipse.
    Radial,
    /// Centered sweep, 0 at top, clockwise.
    Conic,
}

/// Flattens the layout tree into an ordered list of paint commands.
#[must_use]
pub fn build_display_list(layout: &LayoutBox, images: &ImageMap) -> Vec<DisplayCommand> {
    build_display_list_scrolled(layout, images, &std::collections::HashMap::new())
}

/// [`build_display_list`] with per-element scroll offsets: children of an
/// `overflow: scroll/auto` box shift up by its offset (clipped as usual).
#[must_use]
pub fn build_display_list_scrolled(
    layout: &LayoutBox,
    images: &ImageMap,
    scroll_offsets: &std::collections::HashMap<lumen_html::NodeId, f32>,
) -> Vec<DisplayCommand> {
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
    paint_box(
        layout,
        images,
        1.0,
        (0.0, 0.0),
        scroll_offsets,
        &mut commands,
        0,
    );
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

/// Paints one child box. A `position: fixed` child escapes every
/// enclosing scroll shift (it is viewport-relative by definition), so
/// it paints with a zero shift inside a [`DisplayCommand::PushFixed`]
/// scope — the raster backend then skips the page scroll for it too.
#[allow(clippy::too_many_arguments)]
fn paint_child(
    child: &LayoutBox,
    images: &ImageMap,
    opacity: f32,
    shift: (f32, f32),
    scroll_offsets: &std::collections::HashMap<lumen_html::NodeId, f32>,
    commands: &mut Vec<DisplayCommand>,
    depth: usize,
) {
    if child.style.position == Position::Fixed {
        commands.push(DisplayCommand::PushFixed);
        paint_box(
            child,
            images,
            opacity,
            (0.0, 0.0),
            scroll_offsets,
            commands,
            depth + 1,
        );
        commands.push(DisplayCommand::PopFixed);
    } else {
        paint_box(
            child,
            images,
            opacity,
            shift,
            scroll_offsets,
            commands,
            depth + 1,
        );
    }
}

fn paint_box(
    layout: &LayoutBox,
    images: &ImageMap,
    parent_opacity: f32,
    shift: (f32, f32),
    scroll_offsets: &std::collections::HashMap<lumen_html::NodeId, f32>,
    commands: &mut Vec<DisplayCommand>,
    depth: usize,
) {
    // Depth guard: deeper subtrees are simply not painted (layout already
    // caps nesting, this is the second line of defense).
    if depth >= crate::MAX_DEPTH {
        return;
    }
    let place = |rect: Rect| Rect {
        x: rect.x + shift.0,
        y: rect.y + shift.1,
        ..rect
    };
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
    let border_box = place(layout.border_box());

    // Paint-time transform about the element's origin: the whole subtree
    // (background through children) renders inside the transform.
    // Percentage translations resolve against the border box here. The
    // individual translate/rotate/scale properties apply before
    // `transform`, per spec.
    let transformed = if anonymous {
        None
    } else {
        let base = if let Some(source) = &layout.style.transform_percent_source {
            crate::style::resolve_percent_transform(
                source,
                layout.style.font_size,
                crate::geometry::Size {
                    width: border_box.width,
                    height: border_box.height,
                },
            )
        } else {
            layout.style.transform
        };
        match (layout.style.individual_transform, base) {
            (Some(individual), Some(base)) => Some(individual.multiply(base)),
            (Some(individual), None) => Some(individual),
            (None, base) => base,
        }
    };
    if let Some(matrix) = transformed {
        let viewport = crate::geometry::Size::default();
        let origin_x = border_box.x
            + layout
                .style
                .transform_origin
                .0
                .resolve(border_box.width, viewport)
                .unwrap_or(border_box.width / 2.0);
        let origin_y = border_box.y
            + layout
                .style
                .transform_origin
                .1
                .resolve(border_box.height, viewport)
                .unwrap_or(border_box.height / 2.0);
        let about_origin = Transform2D::translate(origin_x, origin_y)
            .multiply(matrix)
            .multiply(Transform2D::translate(-origin_x, -origin_y));
        commands.push(DisplayCommand::PushTransform {
            matrix: about_origin,
        });
    }
    // `visibility: hidden` skips this box's own painting; children still
    // paint (they can set visibility: visible).
    let visible = layout.style.visible;

    // `filter`: the subtree's painted output (background through
    // children) is post-processed by the raster backend over the
    // blur-expanded border box; the SVG backend ignores it.
    let filtered = !anonymous && visible && !layout.style.filters.is_empty();
    if filtered {
        let blur: f32 = layout
            .style
            .filters
            .iter()
            .map(|function| match function {
                FilterFunction::Blur(px) => *px,
                _ => 0.0,
            })
            .sum();
        commands.push(DisplayCommand::PushFilter {
            rect: Rect {
                x: border_box.x - blur,
                y: border_box.y - blur,
                width: border_box.width + 2.0 * blur,
                height: border_box.height + 2.0 * blur,
            },
            filters: layout.style.filters.clone(),
        });
    }

    // Background clip/origin boxes (both default to the border box, so
    // undeclared pages paint exactly as before). Corner radii are not
    // shrunk for inner boxes — a documented approximation.
    let background_box_for = |which: BackgroundBox| match which {
        BackgroundBox::BorderBox => border_box,
        BackgroundBox::PaddingBox => place(layout.dimensions.padding_box()),
        BackgroundBox::ContentBox => place(layout.content_box()),
    };
    let background_clip_box = background_box_for(layout.style.background_clip);
    let background_origin_box = background_box_for(layout.style.background_origin);

    let radius = layout
        .style
        .border_radius
        .clamped_to(border_box.width, border_box.height);

    // Outer box shadows paint under everything, last shadow first (the
    // first of the list sits on top), with a real Gaussian falloff.
    if !anonymous && visible {
        for shadow in layout.style.box_shadows.iter().rev() {
            if shadow.inset {
                continue; // Painted over the background below.
            }
            commands.push(DisplayCommand::DrawShadow {
                rect: Rect {
                    x: border_box.x + shadow.offset_x - shadow.spread,
                    y: border_box.y + shadow.offset_y - shadow.spread,
                    width: border_box.width + 2.0 * shadow.spread,
                    height: border_box.height + 2.0 * shadow.spread,
                },
                radius,
                blur: shadow.blur,
                color: fade(shadow.color),
                inset: false,
            });
        }
    }

    if !anonymous
        && visible
        && let Some(background) = layout.style.background_color
    {
        commands.push(DisplayCommand::FillRect {
            rect: background_clip_box,
            color: fade(background),
            radius,
        });
    }

    // Inset shadows shade inward from the box edge, over the background,
    // with the same Gaussian falloff mirrored.
    if !anonymous && visible {
        for shadow in layout.style.box_shadows.iter().rev() {
            if !shadow.inset {
                continue;
            }
            commands.push(DisplayCommand::DrawShadow {
                rect: Rect {
                    x: border_box.x + shadow.offset_x + shadow.spread,
                    y: border_box.y + shadow.offset_y + shadow.spread,
                    width: (border_box.width - 2.0 * shadow.spread).max(0.0),
                    height: (border_box.height - 2.0 * shadow.spread).max(0.0),
                },
                radius,
                blur: shadow.blur,
                color: fade(shadow.color),
                inset: true,
            });
        }
    }

    // Background layers paint over the color, last layer first (the
    // first of the list sits on top). Gradients render directly over the
    // origin box; url() images follow their layer's position/size/repeat
    // against the origin box, clipped to the clip box (all url layers
    // share the one fetched image per element).
    if !anonymous && visible {
        for layer in layout.style.background_layers.iter().rev() {
            match &layer.image {
                BackgroundImage::LinearGradient(gradient) => {
                    commands.push(DisplayCommand::FillGradient {
                        rect: background_origin_box,
                        radius,
                        angle_degrees: gradient.angle_degrees,
                        stops: gradient
                            .stops
                            .iter()
                            .map(|(color, position)| (fade(*color), *position))
                            .collect(),
                        kind: GradientKind::Linear,
                    });
                }
                BackgroundImage::RadialGradient(stops) => {
                    commands.push(DisplayCommand::FillGradient {
                        rect: background_origin_box,
                        radius,
                        angle_degrees: 0.0,
                        stops: stops
                            .iter()
                            .map(|(color, position)| (fade(*color), *position))
                            .collect(),
                        kind: GradientKind::Radial,
                    });
                }
                BackgroundImage::ConicGradient(stops) => {
                    commands.push(DisplayCommand::FillGradient {
                        rect: background_origin_box,
                        radius,
                        angle_degrees: 0.0,
                        stops: stops
                            .iter()
                            .map(|(color, position)| (fade(*color), *position))
                            .collect(),
                        kind: GradientKind::Conic,
                    });
                }
                BackgroundImage::Url(_) if layout.box_type != BoxType::Replaced => {
                    if let Some(image) = images.get(&layout.node_id) {
                        paint_background_image(
                            layer,
                            background_origin_box,
                            background_clip_box,
                            image,
                            (opacity * 255.0) as u8,
                            commands,
                        );
                    }
                }
                BackgroundImage::Url(_) => {}
            }
        }
    }

    let widths = layout.dimensions.border;
    if !anonymous
        && visible
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

    // Control marks draw over the filled box: check/dot in white, value
    // bars in the accent color (UA blue by default).
    if !anonymous
        && visible
        && let Some(mark) = layout.style.mark
    {
        let mark_color = match mark {
            Mark::Fraction(_) => layout
                .style
                .accent_color
                .unwrap_or(Color::rgb(0x22, 0x66, 0xaa)),
            _ => Color::rgb(0xff, 0xff, 0xff),
        };
        commands.push(DisplayCommand::DrawMark {
            rect: border_box,
            color: fade(mark_color),
            mark,
        });
    }

    // Outline: a frame just outside the border box (plus
    // `outline-offset`), no layout impact.
    if !anonymous
        && visible
        && layout.style.outline_width > 0.0
        && layout.style.outline_style != BorderStyle::None
    {
        let width = layout.style.outline_width;
        let outset = width + layout.style.outline_offset;
        commands.push(DisplayCommand::StrokeRect {
            rect: Rect {
                x: border_box.x - outset,
                y: border_box.y - outset,
                width: border_box.width + 2.0 * outset,
                height: border_box.height + 2.0 * outset,
            },
            widths: EdgeSizes::uniform(width),
            colors: EdgeSizes::uniform(fade(
                layout.style.outline_color.unwrap_or(layout.style.color),
            )),
            styles: EdgeSizes::uniform(layout.style.outline_style),
            radius: Corners::uniform(0.0),
        });
    }

    if layout.box_type == BoxType::Replaced && visible {
        match images.get(&layout.node_id) {
            Some(image) => {
                // object-fit: fit the intrinsic image into the content
                // box; object-position anchors it (percent = share of the
                // leftover space, as for background-position). Fits that
                // can overflow the box (cover, none, scale-down) clip to
                // the content box.
                let content = place(layout.content_box());
                let (intrinsic_w, intrinsic_height) = (image.width as f32, image.height as f32);
                let fit = layout.style.object_fit;
                let contains = |w: f32, h: f32| {
                    let scale = (content.width / w).min(content.height / h);
                    (w * scale, h * scale)
                };
                let (draw_width, draw_height) = match fit {
                    crate::style::ObjectFit::Fill => (content.width, content.height),
                    crate::style::ObjectFit::Contain => contains(intrinsic_w, intrinsic_height),
                    crate::style::ObjectFit::Cover => {
                        let scale =
                            (content.width / intrinsic_w).max(content.height / intrinsic_height);
                        (intrinsic_w * scale, intrinsic_height * scale)
                    }
                    crate::style::ObjectFit::None => (intrinsic_w, intrinsic_height),
                    crate::style::ObjectFit::ScaleDown => {
                        let contained = contains(intrinsic_w, intrinsic_height);
                        if contained.0 < intrinsic_w {
                            contained
                        } else {
                            (intrinsic_w, intrinsic_height)
                        }
                    }
                };
                if draw_width > 0.0 && draw_height > 0.0 {
                    let viewport = crate::geometry::Size::default();
                    let anchor = |dimension: crate::style::Dimension,
                                  box_extent: f32,
                                  draw_extent: f32| match dimension
                    {
                        crate::style::Dimension::Percent(percent) => {
                            (box_extent - draw_extent) * percent / 100.0
                        }
                        other => other.resolve(box_extent, viewport).unwrap_or(0.0),
                    };
                    let rect = Rect {
                        x: content.x
                            + anchor(layout.style.object_position.0, content.width, draw_width),
                        y: content.y
                            + anchor(layout.style.object_position.1, content.height, draw_height),
                        width: draw_width,
                        height: draw_height,
                    };
                    let overflows =
                        draw_width > content.width + 0.5 || draw_height > content.height + 0.5;
                    if overflows {
                        commands.push(DisplayCommand::PushClip { rect: content });
                    }
                    commands.push(DisplayCommand::DrawImage {
                        rect,
                        image: image.clone(),
                        alpha: (opacity * 255.0) as u8,
                    });
                    if overflows {
                        commands.push(DisplayCommand::PopClip);
                    }
                }
            }
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

    // `overflow: hidden/scroll/auto/clip` on either axis: children
    // (including inline content) clip to the padding box; background and
    // border stay intact.
    let clips = !anonymous && layout.style.clips_overflow();
    if clips {
        commands.push(DisplayCommand::PushClip {
            rect: place(layout.dimensions.padding_box()),
        });
    }

    // Inner scrolling: content of a scrollable box shifts up by its
    // offset (the clip is already in place). Inline lines and block
    // children both scroll. `position: sticky` children counter-shift to
    // stay glued to the container's padding box (see
    // LayoutBox::sticky_child_shift).
    let scroll_offset = match scroll_offsets.get(&layout.node_id) {
        Some(offset) if clips => *offset,
        _ => 0.0,
    };
    let child_shift = (shift.0, shift.1 - scroll_offset);

    if let LayoutKind::Inline { lines } = &layout.kind {
        let content = {
            let content = layout.content_box();
            Rect {
                x: content.x + child_shift.0,
                y: content.y + child_shift.1,
                ..content
            }
        };
        for line in lines {
            for fragment in &line.fragments {
                match &fragment.content {
                    crate::inline::FragmentContent::Text { text, style } if style.visible => {
                        // A fragment background (e.g. from `::first-line`)
                        // paints behind the text over the line height.
                        if let Some(background) = style.background_color {
                            commands.push(DisplayCommand::FillRect {
                                rect: Rect {
                                    x: content.x + fragment.x,
                                    y: content.y + line.y,
                                    width: fragment.width,
                                    height: line.height,
                                },
                                color: fade(background),
                                radius: Corners::uniform(0.0),
                            });
                        }
                        // Text shadows: offset copies in the shadow color,
                        // blur approximated by thinning the alpha.
                        for shadow in style.text_shadows.iter().rev() {
                            let softness = 1.0 / (1.0 + shadow.blur / 3.0);
                            commands.push(DisplayCommand::DrawText {
                                x: content.x + fragment.x + shadow.offset_x,
                                y: content.y
                                    + line.y
                                    + line.baseline
                                    + fragment.dy
                                    + shadow.offset_y,
                                text: text.clone(),
                                color: fade(shadow.color.with_alpha_factor(softness)),
                                font_size: style.font_size,
                                font_weight: style.font_weight.0,
                                underline: false,
                                italic: style.italic,
                                monospace: style.monospace,
                                line_through: false,
                                letter_spacing: style.letter_spacing,
                                decoration_color: fade(shadow.color),
                                decoration_style: BorderStyle::Solid,
                            });
                        }
                        commands.push(DisplayCommand::DrawText {
                            x: content.x + fragment.x,
                            y: content.y + line.y + line.baseline + fragment.dy,
                            text: text.clone(),
                            color: fade(style.color),
                            font_size: style.font_size,
                            font_weight: style.font_weight.0,
                            underline: style.underline,
                            italic: style.italic,
                            monospace: style.monospace,
                            line_through: style.line_through,
                            letter_spacing: style.letter_spacing,
                            decoration_color: fade(
                                style.text_decoration_color.unwrap_or(style.color),
                            ),
                            decoration_style: style.text_decoration_style,
                        });
                    }
                    crate::inline::FragmentContent::Box(laid) => {
                        paint_child(
                            laid,
                            images,
                            opacity,
                            child_shift,
                            scroll_offsets,
                            commands,
                            depth,
                        );
                    }
                    crate::inline::FragmentContent::Text { .. } => {}
                }
            }
        }
    }

    // Fast path: with no z-index anywhere among the children, the stable
    // paint-order sort is the identity — iterate in tree order without
    // building (and sorting) a temporary Vec per box.
    if layout
        .children
        .iter()
        .all(|child| child.style.z_index.is_none())
    {
        for child in &layout.children {
            let sticky = layout.sticky_child_shift(child, scroll_offset);
            paint_child(
                child,
                images,
                opacity,
                (child_shift.0, child_shift.1 + sticky),
                scroll_offsets,
                commands,
                depth,
            );
        }
    } else {
        for child in layout.children_in_paint_order() {
            let sticky = layout.sticky_child_shift(child, scroll_offset);
            paint_child(
                child,
                images,
                opacity,
                (child_shift.0, child_shift.1 + sticky),
                scroll_offsets,
                commands,
                depth,
            );
        }
    }

    if clips {
        commands.push(DisplayCommand::PopClip);
    }
    if filtered {
        commands.push(DisplayCommand::PopFilter);
    }
    if transformed.is_some() {
        commands.push(DisplayCommand::PopTransform);
    }
}

/// Emits a background image with position/size/repeat semantics: the
/// tile rect comes from `background-size` (resolved against the origin
/// box), its anchor from `background-position` (percents place the image
/// per CSS), and `background-repeat` tiles it across the origin box,
/// clipped to the clip box (`background-clip`).
fn paint_background_image(
    layer: &BackgroundLayer,
    origin_box: Rect,
    clip_box: Rect,
    image: &Arc<RasterImage>,
    alpha: u8,
    commands: &mut Vec<DisplayCommand>,
) {
    if image.width == 0 || image.height == 0 {
        return;
    }
    let intrinsic = (image.width as f32, image.height as f32);
    let viewport = crate::geometry::Size::default(); // vw/vh unsupported here
    let (tile_width, tile_height) = match layer.size {
        BackgroundSize::Auto => intrinsic,
        BackgroundSize::Cover => {
            let scale = (origin_box.width / intrinsic.0).max(origin_box.height / intrinsic.1);
            (intrinsic.0 * scale, intrinsic.1 * scale)
        }
        BackgroundSize::Contain => {
            let scale = (origin_box.width / intrinsic.0).min(origin_box.height / intrinsic.1);
            (intrinsic.0 * scale, intrinsic.1 * scale)
        }
        BackgroundSize::Explicit(width, height) => {
            let width = width.resolve(origin_box.width, viewport);
            let height = height.resolve(origin_box.height, viewport);
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
    let anchor_x = origin_box.x + offset(layer.position.0, origin_box.width, tile_width);
    let anchor_y = origin_box.y + offset(layer.position.1, origin_box.height, tile_height);
    let (repeat_x, repeat_y) = layer.repeat;

    // Tile from the first tile at/before each edge to past the far edge.
    let mut tiles: Vec<(f32, f32)> = Vec::new();
    let first_x = if repeat_x {
        anchor_x - ((anchor_x - origin_box.x) / tile_width).ceil() * tile_width
    } else {
        anchor_x
    };
    let first_y = if repeat_y {
        anchor_y - ((anchor_y - origin_box.y) / tile_height).ceil() * tile_height
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
            if x >= origin_box.x + origin_box.width {
                break;
            }
        }
        if !repeat_y || tiles.len() >= 1024 {
            break;
        }
        y += tile_height;
        if y >= origin_box.y + origin_box.height {
            break;
        }
    }

    commands.push(DisplayCommand::PushClip { rect: clip_box });
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
            DisplayCommand::DrawMark { rect, mark, .. } => {
                let _ = writeln!(
                    output,
                    "DrawMark {:?} x={} y={} w={} h={}",
                    mark, rect.x, rect.y, rect.width, rect.height
                );
            }
            DisplayCommand::PushTransform { matrix } => {
                let _ = writeln!(
                    output,
                    "PushTransform matrix({}, {}, {}, {}, {}, {})",
                    matrix.a, matrix.b, matrix.c, matrix.d, matrix.e, matrix.f
                );
            }
            DisplayCommand::PopTransform => {
                let _ = writeln!(output, "PopTransform");
            }
            DisplayCommand::DrawShadow {
                rect,
                blur,
                color,
                inset,
                ..
            } => {
                let kind = if *inset { "inset" } else { "outer" };
                let _ = writeln!(
                    output,
                    "DrawShadow {kind} x={} y={} w={} h={} blur={blur} color={color}",
                    rect.x, rect.y, rect.width, rect.height
                );
            }
            DisplayCommand::PushFilter { rect, filters } => {
                let _ = writeln!(
                    output,
                    "PushFilter x={} y={} w={} h={} filters={}",
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                    filters.len()
                );
            }
            DisplayCommand::PopFilter => {
                let _ = writeln!(output, "PopFilter");
            }
            DisplayCommand::PushFixed => {
                let _ = writeln!(output, "PushFixed");
            }
            DisplayCommand::PopFixed => {
                let _ = writeln!(output, "PopFixed");
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

    /// First laid-out box with this tag, depth-first. html5ever wraps
    /// every document in html/head/body, so tests locate boxes by tag
    /// instead of fixed child indices from the root.
    fn find_box<'a>(layout: &'a crate::LayoutBox, tag: &str) -> &'a crate::LayoutBox {
        fn find<'a>(layout: &'a crate::LayoutBox, tag: &str) -> Option<&'a crate::LayoutBox> {
            if matches!(&layout.kind, crate::LayoutKind::Element(t) if t == tag) {
                return Some(layout);
            }
            layout.children.iter().find_map(|child| find(child, tag))
        }
        find(layout, tag).unwrap_or_else(|| panic!("no laid-out box for <{tag}>"))
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
                DisplayCommand::DrawShadow { .. } => "shadow",
                DisplayCommand::DrawMark { .. } => "mark",
                DisplayCommand::PushTransform { .. } => "push-transform",
                DisplayCommand::PopTransform => "pop-transform",
                DisplayCommand::PushFilter { .. } => "push-filter",
                DisplayCommand::PopFilter => "pop-filter",
                DisplayCommand::PushFixed => "push-fixed",
                DisplayCommand::PopFixed => "pop-fixed",
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
        let layer = BackgroundLayer {
            image: crate::style::BackgroundImage::Url("s.png".to_string()),
            position: (
                crate::style::Dimension::Px(-20.0),
                crate::style::Dimension::Px(-30.0),
            ),
            size: BackgroundSize::Auto,
            repeat: (false, false),
        };
        let image = Arc::new(RasterImage {
            width: 10,
            height: 10,
            rgba: vec![0; 400],
            encoded: Vec::new(),
            mime: "image/png",
        });
        let mut commands = Vec::new();
        let tile_box = Rect {
            x: 100.0,
            y: 50.0,
            width: 40.0,
            height: 20.0,
        };
        paint_background_image(&layer, tile_box, tile_box, &image, 255, &mut commands);
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
        let layer = BackgroundLayer {
            image: crate::style::BackgroundImage::Url("t.png".to_string()),
            position: (
                crate::style::Dimension::Px(0.0),
                crate::style::Dimension::Px(0.0),
            ),
            size: BackgroundSize::Auto,
            repeat: (true, true),
        };
        let image = Arc::new(RasterImage {
            width: 10,
            height: 10,
            rgba: vec![0; 400],
            encoded: Vec::new(),
            mime: "image/png",
        });
        let mut commands = Vec::new();
        let tile_box = Rect {
            x: 0.0,
            y: 0.0,
            width: 35.0,
            height: 15.0,
        };
        paint_background_image(&layer, tile_box, tile_box, &image, 255, &mut commands);
        let images = commands
            .iter()
            .filter(|command| matches!(command, DisplayCommand::DrawImage { .. }))
            .count();
        // 4 columns x 2 rows.
        assert_eq!(images, 8);
    }

    #[test]
    fn sticky_child_sticks_to_scrolled_container() {
        let page = build_page(
            "<style>.scroll { overflow-y: scroll; height: 100px; }\
                    .head { position: sticky; top: 0; height: 20px; z-index: 1; \
                            background-color: #112233; }\
                    .tall { height: 500px; }</style>\
             <div class='scroll'><div class='head'></div><div class='tall'></div></div>",
            Size {
                width: 800.0,
                height: 600.0,
            },
        );
        let scroller = find_box(&page.layout, "div");
        let head = &scroller.children[0];
        let container_top = scroller.dimensions.padding_box().y;
        let offsets = std::collections::HashMap::from([(scroller.node_id, 60.0)]);
        let list = build_display_list_scrolled(&page.layout, &page.images, &offsets);
        // The header background paints at the container top, not at its
        // scrolled-off flow position (container_top - 60).
        let head_y = list
            .iter()
            .find_map(|command| match command {
                DisplayCommand::FillRect { rect, color, .. }
                    if *color == Color::rgb(0x11, 0x22, 0x33) =>
                {
                    Some(rect.y)
                }
                _ => None,
            })
            .expect("header fill");
        assert_eq!(head_y, container_top);
        // The hit test agrees: the stuck header answers at its visual
        // position even though its flow position scrolled away.
        let hit = page
            .layout
            .hit_test_scrolled(5.0, container_top + 5.0, &offsets);
        assert_eq!(hit, Some(head.node_id));
        // Without scroll the header sits at its flow position.
        let list = build_display_list(&page.layout, &page.images);
        let head_y = list
            .iter()
            .find_map(|command| match command {
                DisplayCommand::FillRect { rect, color, .. }
                    if *color == Color::rgb(0x11, 0x22, 0x33) =>
                {
                    Some(rect.y)
                }
                _ => None,
            })
            .expect("header fill");
        assert_eq!(head_y, container_top);
    }

    #[test]
    fn sticky_bottom_sticks_to_the_container_bottom() {
        let page = build_page(
            "<style>.scroll { overflow: scroll; height: 100px; }\
                    .foot { position: sticky; bottom: 10px; height: 20px; \
                            background-color: #445566; }\
                    .tall { height: 500px; }</style>\
             <div class='scroll'><div class='tall'></div><div class='foot'></div></div>",
            Size {
                width: 800.0,
                height: 600.0,
            },
        );
        let scroller = find_box(&page.layout, "div");
        let container = scroller.dimensions.padding_box();
        let offsets = std::collections::HashMap::from([(scroller.node_id, 30.0)]);
        let list = build_display_list_scrolled(&page.layout, &page.images, &offsets);
        let foot_y = list
            .iter()
            .find_map(|command| match command {
                DisplayCommand::FillRect { rect, color, .. }
                    if *color == Color::rgb(0x44, 0x55, 0x66) =>
                {
                    Some(rect.y)
                }
                _ => None,
            })
            .expect("footer fill");
        // Flow position 500 - 30 scroll = 470 is way past the container
        // bottom; the footer sticks to bottom - 10 inset.
        assert_eq!(foot_y, container.y + container.height - 10.0 - 20.0);
    }

    #[test]
    fn overflow_x_alone_clips_and_forces_y_to_auto() {
        // overflow-x: hidden + (initial) overflow-y: visible computes y
        // to auto per CSS, so the box clips and becomes scrollable.
        let page = build_page(
            "<style>.clip { overflow-x: hidden; width: 50px; height: 40px; }\
                    .big { width: 500px; height: 500px; }</style>\
             <div class='clip'><div class='big'></div></div>",
            Size {
                width: 800.0,
                height: 600.0,
            },
        );
        let clip = find_box(&page.layout, "div");
        assert_eq!(clip.style.overflow_x, crate::style::Overflow::Hidden);
        assert_eq!(clip.style.overflow_y, crate::style::Overflow::Scroll);
        assert!(
            page.display_list
                .iter()
                .any(|command| matches!(command, DisplayCommand::PushClip { .. }))
        );
    }

    #[test]
    fn overflow_two_value_shorthand_splits_axes() {
        let page = build_page(
            "<style>.c { overflow: hidden auto; height: 40px; }</style>\
             <div class='c'><div style='height: 500px;'></div></div>",
            Size {
                width: 800.0,
                height: 600.0,
            },
        );
        let clip = find_box(&page.layout, "div");
        assert_eq!(clip.style.overflow_x, crate::style::Overflow::Hidden);
        assert_eq!(clip.style.overflow_y, crate::style::Overflow::Scroll);
        // Scrollable vertically: reports room to scroll.
        assert!(clip.max_inner_scroll() > 0.0);
    }

    /// Builds a page with one decoded image per `<img>`.
    fn page_with_image(html: &str, width: u32, height: u32) -> crate::Page {
        use crate::image::RasterImage;
        use std::sync::Arc;
        let document = lumen_html::parse_document(html);
        let sheet = lumen_css::parse_stylesheet(&crate::extract_embedded_css(&document));
        let mut images = crate::image::ImageMap::new();
        for (node, _) in crate::image::collect_image_sources(&document) {
            images.insert(
                node,
                Arc::new(RasterImage {
                    width,
                    height,
                    rgba: vec![0; (width * height * 4) as usize],
                    encoded: Vec::new(),
                    mime: "image/png",
                }),
            );
        }
        crate::page_from_document(
            document,
            std::sync::Arc::new(sheet),
            std::sync::Arc::new(images),
            Size {
                width: 800.0,
                height: 600.0,
            },
            &crate::HeuristicMeasurer,
            None,
        )
    }

    fn drawn_image(page: &crate::Page) -> Rect {
        page.display_list
            .iter()
            .find_map(|command| match command {
                DisplayCommand::DrawImage { rect, .. } => Some(*rect),
                _ => None,
            })
            .expect("a DrawImage command")
    }

    #[test]
    fn object_fit_contain_letterboxes_the_image() {
        // 200x50 intrinsic in a 100x100 box: contain draws 100x25,
        // centered by the default 50% 50% object-position.
        let page = page_with_image(
            "<style>img { width: 100px; height: 100px; object-fit: contain; }</style>\
             <img src='a.png'>",
            200,
            50,
        );
        let rect = drawn_image(&page);
        assert_eq!((rect.width, rect.height), (100.0, 25.0));
        assert_eq!(rect.y, 37.5);
    }

    #[test]
    fn object_fit_cover_overflows_and_clips() {
        let page = page_with_image(
            "<style>img { width: 100px; height: 100px; object-fit: cover; \
                          object-position: 0 0; }</style><img src='a.png'>",
            200,
            50,
        );
        let rect = drawn_image(&page);
        // Cover scale = max(100/200, 100/50) = 2 → 400x100 anchored 0 0.
        assert_eq!((rect.width, rect.height), (400.0, 100.0));
        assert_eq!((rect.x, rect.y), (0.0, 0.0));
        // The overflowing image is clipped to the content box.
        let push = page
            .display_list
            .iter()
            .position(|command| matches!(command, DisplayCommand::PushClip { .. }))
            .expect("clip");
        let image = page
            .display_list
            .iter()
            .position(|command| matches!(command, DisplayCommand::DrawImage { .. }))
            .unwrap();
        assert!(push < image);
    }

    #[test]
    fn object_fit_none_and_scale_down_keep_intrinsic_size() {
        // 20x10 intrinsic in a 100x100 box: none keeps 20x10 centered.
        let page = page_with_image(
            "<style>img { width: 100px; height: 100px; object-fit: none; }</style>\
             <img src='a.png'>",
            20,
            10,
        );
        let rect = drawn_image(&page);
        assert_eq!((rect.width, rect.height), (20.0, 10.0));
        assert_eq!((rect.x, rect.y), (40.0, 45.0));
        // scale-down shrinks like contain when the box is smaller.
        let page = page_with_image(
            "<style>img { width: 10px; height: 5px; object-fit: scale-down; }</style>\
             <img src='a.png'>",
            20,
            10,
        );
        let rect = drawn_image(&page);
        assert_eq!((rect.width, rect.height), (10.0, 5.0));
    }

    #[test]
    fn transforms_wrap_the_subtree_in_matrix_commands() {
        let list = commands(
            "<style>div { transform: translate(10px, 20px) scale(2); \
                          width: 50px; height: 20px; background-color: #ff0000; }</style>\
             <div>t</div>",
        );
        let Some(DisplayCommand::PushTransform { matrix }) = list
            .iter()
            .find(|command| matches!(command, DisplayCommand::PushTransform { .. }))
        else {
            panic!("no PushTransform in {list:?}");
        };
        // scale(2) about the border-box center, translated by (10, 20).
        assert_eq!(matrix.a, 2.0);
        assert_eq!(matrix.d, 2.0);
        assert!(
            list.iter()
                .any(|command| matches!(command, DisplayCommand::PopTransform))
        );
        let push = list
            .iter()
            .position(|command| matches!(command, DisplayCommand::PushTransform { .. }))
            .unwrap();
        let fill = list
            .iter()
            .position(|command| {
                matches!(command, DisplayCommand::FillRect { color, .. } if color.r == 255)
            })
            .unwrap();
        assert!(push < fill, "background paints inside the transform");
    }

    #[test]
    fn percent_translate_resolves_against_the_border_box() {
        // translate(50%, 25%) of a 100x40 border box is (50, 10) — the
        // old code treated the percentages as raw pixels.
        let list = commands(
            "<style>div { transform: translate(50%, 25%); \
                          width: 100px; height: 40px; background-color: #ff0000; }</style>\
             <div>t</div>",
        );
        let Some(DisplayCommand::PushTransform { matrix }) = list
            .iter()
            .find(|command| matches!(command, DisplayCommand::PushTransform { .. }))
        else {
            panic!("no PushTransform in {list:?}");
        };
        // Pure translation about the origin: e/f carry the resolved offsets.
        assert_eq!(matrix.a, 1.0);
        assert_eq!(matrix.d, 1.0);
        assert!((matrix.e - 50.0).abs() < 0.001, "{matrix:?}");
        assert!((matrix.f - 10.0).abs() < 0.001, "{matrix:?}");
    }

    #[test]
    fn text_shadows_paint_offset_copies_first() {
        let list = commands(
            "<style>p { text-shadow: 2px 3px 4px #ff0000; color: #111111; }</style>\
             <p>shadowed</p>",
        );
        let texts: Vec<(f32, f32, String)> = list
            .iter()
            .filter_map(|command| match command {
                DisplayCommand::DrawText { x, y, color, .. } => Some((*x, *y, color.to_string())),
                _ => None,
            })
            .collect();
        assert_eq!(texts.len(), 2);
        // Shadow first, offset by (2, 3), reddish and translucent.
        assert_eq!(texts[0].0, texts[1].0 + 2.0);
        assert_eq!(texts[0].1, texts[1].1 + 3.0);
        assert!(texts[0].2.starts_with("rgba(255"));
    }

    #[test]
    fn visibility_hidden_skips_painting_but_keeps_space() {
        let list = commands(
            "<style>.gone { visibility: hidden; background-color: #ff0000; height: 20px; }\
                    .after { background-color: #00ff00; height: 10px; }</style>\
             <div class='gone'>invisible text</div><div class='after'></div>",
        );
        assert!(
            !list
                .iter()
                .any(|command| matches!(command, DisplayCommand::DrawText { .. }))
        );
        let Some(DisplayCommand::FillRect { rect, color, .. }) = list.iter().find(
            |command| matches!(command, DisplayCommand::FillRect { color, .. } if color.g == 255),
        ) else {
            panic!("expected the visible sibling fill");
        };
        assert_eq!(color.to_string(), "#00ff00");
        // The hidden box still occupies its 20px.
        assert_eq!(rect.y, 20.0);
    }

    #[test]
    fn box_shadows_emit_gaussian_shadow_commands() {
        let list = commands(
            "<style>div { box-shadow: 5px 5px 8px #000000, 0 0 4px #ff0000; \
                          background-color: #ffffff; width: 50px; height: 20px; }</style>\
             <div></div>",
        );
        let shadows: Vec<(&Rect, f32)> = list
            .iter()
            .filter_map(|command| match command {
                DisplayCommand::DrawShadow { rect, blur, .. } => Some((rect, *blur)),
                _ => None,
            })
            .collect();
        assert_eq!(shadows.len(), 2);
        // Last shadow of the list paints first; the first sits on top.
        assert_eq!(shadows[0].1, 4.0);
        assert_eq!(shadows[1].1, 8.0);
        assert_eq!((shadows[1].0.x, shadows[1].0.y), (5.0, 5.0));
        // The shadow paints before the background fill.
        let shadow_at = list
            .iter()
            .position(|command| matches!(command, DisplayCommand::DrawShadow { .. }))
            .unwrap();
        let fill_at = list
            .iter()
            .position(|command| {
                matches!(command, DisplayCommand::FillRect { color, .. } if color.a == 255)
            })
            .unwrap();
        assert!(shadow_at < fill_at);
    }

    #[test]
    fn inset_shadows_paint_over_the_background() {
        let list = commands(
            "<style>div { box-shadow: inset 0 0 6px #000000; background-color: #ffffff; \
                          width: 50px; height: 30px; }</style><div></div>",
        );
        let background = list
            .iter()
            .position(|command| {
                matches!(command, DisplayCommand::FillRect { color, .. } if color.a == 255)
            })
            .unwrap();
        let shadow = list
            .iter()
            .position(|command| matches!(command, DisplayCommand::DrawShadow { inset: true, .. }))
            .unwrap();
        assert!(shadow > background, "inset shades over the fill");
    }

    #[test]
    fn outline_strokes_outside_the_border_box() {
        let list = commands(
            "<style>div { outline: 2px solid #ff0000; width: 50px; height: 20px; }</style>\
             <div></div>",
        );
        let Some(DisplayCommand::StrokeRect {
            rect,
            widths,
            colors,
            ..
        }) = list
            .iter()
            .find(|command| matches!(command, DisplayCommand::StrokeRect { .. }))
        else {
            panic!("no outline stroke");
        };
        assert_eq!(rect.x, -2.0);
        assert_eq!(rect.width, 54.0);
        assert_eq!(widths.top, 2.0);
        assert_eq!(colors.top.to_string(), "#ff0000");
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

    #[test]
    fn filter_wraps_the_box_in_push_pop_filter() {
        let list = commands(
            "<style>div { filter: blur(2px) grayscale(50%); width: 50px; height: 20px; }</style>\
             <div>x</div>",
        );
        let push = list
            .iter()
            .position(|command| matches!(command, DisplayCommand::PushFilter { .. }))
            .expect("a PushFilter");
        let pop = list
            .iter()
            .position(|command| matches!(command, DisplayCommand::PopFilter))
            .expect("a PopFilter");
        assert!(push < pop);
        let Some(DisplayCommand::PushFilter { rect, filters }) = list.get(push) else {
            unreachable!()
        };
        assert_eq!(
            filters,
            &vec![
                crate::style::FilterFunction::Blur(2.0),
                crate::style::FilterFunction::Grayscale(0.5),
            ]
        );
        // The region is blur-expanded around the border box.
        assert_eq!(rect.x, -2.0);
        assert_eq!(rect.width, 54.0);
    }

    #[test]
    fn no_filter_commands_without_filter() {
        let list = commands("<div>x</div>");
        assert!(!list.iter().any(|command| matches!(
            command,
            DisplayCommand::PushFilter { .. } | DisplayCommand::PopFilter
        )));
    }

    #[test]
    fn outline_offset_moves_the_outline_outward() {
        let list = commands(
            "<style>div { width: 100px; height: 40px; outline: 2px solid #112233; \
                          outline-offset: 3px; }</style><div></div>",
        );
        let Some(DisplayCommand::StrokeRect { rect, widths, .. }) = list
            .iter()
            .find(|command| matches!(command, DisplayCommand::StrokeRect { .. }))
        else {
            panic!("no outline in {list:?}");
        };
        // Outset = width + offset = 5px around the 100x40 border box.
        assert_eq!(widths.top, 2.0);
        assert_eq!(rect.x, -5.0);
        assert_eq!(rect.y, -5.0);
        assert_eq!(rect.width, 110.0);
        assert_eq!(rect.height, 50.0);
    }

    #[test]
    fn background_clip_content_box_shrinks_the_fill() {
        let list = commands(
            "<style>div { width: 100px; height: 40px; padding: 10px; border-width: 5px; \
                          background-color: #123456; background-clip: content-box; }</style>\
             <div></div>",
        );
        let Some(DisplayCommand::FillRect { rect, .. }) = list
            .iter()
            .find(|command| matches!(command, DisplayCommand::FillRect { .. }))
        else {
            panic!("no FillRect in {list:?}");
        };
        // Content box: 15px in from the 130x70 border box.
        assert_eq!(rect.x, 15.0);
        assert_eq!(rect.y, 15.0);
        assert_eq!(rect.width, 100.0);
        assert_eq!(rect.height, 40.0);
    }

    #[test]
    fn background_origin_padding_box_places_the_gradient() {
        let list = commands(
            "<style>div { width: 100px; height: 40px; padding: 10px; border-width: 5px; \
                          background-image: linear-gradient(to right, #000000, #ffffff); \
                          background-origin: padding-box; }</style><div></div>",
        );
        let Some(DisplayCommand::FillGradient { rect, .. }) = list
            .iter()
            .find(|command| matches!(command, DisplayCommand::FillGradient { .. }))
        else {
            panic!("no FillGradient in {list:?}");
        };
        // Padding box: 5px in from the border box.
        assert_eq!(rect.x, 5.0);
        assert_eq!(rect.y, 5.0);
        assert_eq!(rect.width, 120.0);
        assert_eq!(rect.height, 60.0);
    }

    #[test]
    fn accent_color_colors_the_value_bar() {
        let list = commands(
            "<style>input { accent-color: #010203; }</style>\
             <input type='range' value='50'>",
        );
        let Some(DisplayCommand::DrawMark { color, mark, .. }) = list
            .iter()
            .find(|command| matches!(command, DisplayCommand::DrawMark { .. }))
        else {
            panic!("no DrawMark in {list:?}");
        };
        assert!(matches!(mark, Mark::Fraction(_)));
        assert_eq!(*color, Color::rgb(1, 2, 3));
        // Default stays the UA blue.
        let list = commands("<input type='range' value='50'>");
        let Some(DisplayCommand::DrawMark { color, .. }) = list
            .iter()
            .find(|command| matches!(command, DisplayCommand::DrawMark { .. }))
        else {
            panic!("no DrawMark in {list:?}");
        };
        assert_eq!(*color, Color::rgb(0x22, 0x66, 0xaa));
    }

    #[test]
    fn individual_transform_properties_reach_the_display_list() {
        let list = commands(
            "<style>div { translate: 10px 0; scale: 2; width: 10px; height: 10px; }</style>\
             <div></div>",
        );
        let Some(DisplayCommand::PushTransform { matrix }) = list
            .iter()
            .find(|command| matches!(command, DisplayCommand::PushTransform { .. }))
        else {
            panic!("no PushTransform in {list:?}");
        };
        // Scale 2 about the box center, then translate by 10px.
        assert!((matrix.a - 2.0).abs() < 1e-4);
        assert!((matrix.d - 2.0).abs() < 1e-4);
    }

    #[test]
    fn fixed_boxes_are_wrapped_in_a_fixed_scope() {
        let list = commands(
            "<style>.badge { position: fixed; top: 10px; right: 10px; \
                    width: 50px; height: 20px; background-color: #333; }</style>\
             <div class='badge'></div><p>content</p>",
        );
        let push = list
            .iter()
            .position(|command| matches!(command, DisplayCommand::PushFixed))
            .expect("fixed badge emits PushFixed");
        let pop = list
            .iter()
            .position(|command| matches!(command, DisplayCommand::PopFixed))
            .expect("fixed badge emits PopFixed");
        assert!(push < pop);
        // The badge's own background is painted inside the scope.
        assert!(
            list[push + 1..pop]
                .iter()
                .any(|command| matches!(command, DisplayCommand::FillRect { .. }))
        );
    }
}
