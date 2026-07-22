//! Software rasterizer: display list → ARGB pixel buffer.
//!
//! The second display-list backend after SVG, meant for the desktop shell.
//! Text uses the built-in 8×8 bitmap font (`font8x8`) scaled to the font
//! size — deliberately crude but fully self-contained and deterministic.
//! Glyph cells are half an em wide to match [`crate::HeuristicMeasurer`],
//! so painted text agrees with layout's line breaking.

use crate::font::SystemFont;
use crate::geometry::{Corners, Rect};
use crate::paint::{DisplayCommand, GradientKind};
use crate::style::{Mark, Transform2D};
use font8x8::UnicodeFonts;
use lumen_css::Color;

/// A row-major 32-bit `0RGB` pixel buffer (softbuffer's native format).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
    /// Active clip in device pixels (x0, y0, x1, y1); draws outside are
    /// dropped. Maintained by PushClip/PopClip during rasterization.
    clip: Option<(u32, u32, u32, u32)>,
}

impl Framebuffer {
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![0x00ff_ffff; (width as usize) * (height as usize)],
            clip: None,
        }
    }

    /// The drawable bounds: the intersection of the buffer and the clip.
    fn bounds(&self) -> (u32, u32, u32, u32) {
        let (cx0, cy0, cx1, cy1) = self.clip.unwrap_or((0, 0, self.width, self.height));
        // Clips can lie entirely off-screen (overflow boxes past the
        // viewport); keep the bounds ordered so clamps never see min > max.
        let x1 = cx1.min(self.width);
        let y1 = cy1.min(self.height);
        (cx0.min(x1), cy0.min(y1), x1, y1)
    }

    /// Whether one device pixel is drawable under the current clip.
    fn admits(&self, x: u32, y: u32) -> bool {
        let (x0, y0, x1, y1) = self.bounds();
        x >= x0 && x < x1 && y >= y0 && y < y1
    }

    /// Intersects a device-pixel rect `[x0,x1)×[y0,y1)` with the current
    /// clip, so per-pixel loops iterate only drawable pixels instead of
    /// scanning the whole primitive and rejecting each pixel via `admits`.
    /// A primitive entirely outside the clip yields an empty (but ordered)
    /// range, never an inverted one — callers clamp against these bounds.
    fn clamp_to_clip(&self, x0: u32, y0: u32, x1: u32, y1: u32) -> (u32, u32, u32, u32) {
        let (cx0, cy0, cx1, cy1) = self.bounds();
        let x0 = x0.max(cx0);
        let y0 = y0.max(cy0);
        (x0, y0, x1.min(cx1).max(x0), y1.min(cy1).max(y0))
    }

    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> u32 {
        self.pixels[(y * self.width + x) as usize]
    }

    /// Blends `color` over a rectangle at the given alpha — used for
    /// translucent overlays like text-selection highlights.
    pub fn blend_fill(&mut self, rect: Rect, color: lumen_css::Color, alpha: u8) {
        let packed = pack(color);
        let (cx0, cy0, cx1, cy1) = self.bounds();
        let x0 = (rect.x.max(0.0) as u32).clamp(cx0, cx1);
        let y0 = (rect.y.max(0.0) as u32).clamp(cy0, cy1);
        let x1 = ((rect.x + rect.width).max(0.0) as u32).clamp(cx0, cx1);
        let y1 = ((rect.y + rect.height).max(0.0) as u32).clamp(cy0, cy1);
        for y in y0..y1 {
            for x in x0..x1 {
                let position = (y * self.width + x) as usize;
                self.pixels[position] = blend(self.pixels[position], packed, alpha);
            }
        }
    }

    fn fill(&mut self, rect: Rect, color: u32) {
        let (cx0, cy0, cx1, cy1) = self.bounds();
        let x0 = (rect.x.max(0.0) as u32).clamp(cx0, cx1);
        let y0 = (rect.y.max(0.0) as u32).clamp(cy0, cy1);
        let x1 = ((rect.x + rect.width).max(0.0) as u32).clamp(cx0, cx1);
        let y1 = ((rect.y + rect.height).max(0.0) as u32).clamp(cy0, cy1);
        for y in y0..y1 {
            let row = (y * self.width) as usize;
            for x in x0..x1 {
                self.pixels[row + x as usize] = color;
            }
        }
    }
}

const fn pack(color: Color) -> u32 {
    ((color.r as u32) << 16) | ((color.g as u32) << 8) | (color.b as u32)
}

/// Fills a rect respecting the color's alpha channel.
fn paint_rect(framebuffer: &mut Framebuffer, rect: Rect, color: Color) {
    if color.a == 255 {
        framebuffer.fill(rect, pack(color));
    } else if color.a > 0 {
        framebuffer.blend_fill(rect, color, color.a);
    }
}

/// Rasterizes paint commands into a fresh white framebuffer using the
/// built-in bitmap font for text.
///
/// `scroll_y` shifts all content upward, so the visible window shows the
/// page from that offset down.
#[must_use]
pub fn rasterize(
    commands: &[DisplayCommand],
    width: u32,
    height: u32,
    scroll_y: f32,
) -> Framebuffer {
    rasterize_with(commands, width, height, scroll_y, 1.0, None)
}

/// Like [`rasterize`], but with a device scale factor (HiDPI: CSS pixels ×
/// `scale` = physical pixels) and optionally a real scalable font (with
/// anti-aliased coverage blending).
#[must_use]
pub fn rasterize_with(
    commands: &[DisplayCommand],
    width: u32,
    height: u32,
    scroll_y: f32,
    scale: f32,
    font: Option<&SystemFont>,
) -> Framebuffer {
    let mut framebuffer = Framebuffer::new(width, height);
    rasterize_over(&mut framebuffer, commands, scroll_y, scale, font);
    framebuffer
}

/// Paints commands onto an existing framebuffer without clearing it —
/// used for UI chrome overlays (e.g. the desktop address bar).
pub fn rasterize_over(
    framebuffer: &mut Framebuffer,
    commands: &[DisplayCommand],
    scroll_y: f32,
    scale: f32,
    font: Option<&SystemFont>,
) {
    rasterize_clipped(framebuffer, commands, scroll_y, scale, font, None);
}

/// Like [`rasterize_over`], but restricted to a device-pixel region
/// (x0, y0, x1, y1): pixels outside are untouched. Used for incremental
/// repaints (e.g. the strip a scroll exposes). The region is filled white
/// first, matching the fresh-framebuffer background.
pub fn rasterize_region(
    framebuffer: &mut Framebuffer,
    commands: &[DisplayCommand],
    scroll_y: f32,
    scale: f32,
    font: Option<&SystemFont>,
    region: (u32, u32, u32, u32),
) {
    let (x0, y0, x1, y1) = region;
    framebuffer.fill(
        Rect {
            x: x0 as f32,
            y: y0 as f32,
            width: x1.saturating_sub(x0) as f32,
            height: y1.saturating_sub(y0) as f32,
        },
        0x00ff_ffff,
    );
    rasterize_clipped(framebuffer, commands, scroll_y, scale, font, Some(region));
}

fn rasterize_clipped(
    framebuffer: &mut Framebuffer,
    commands: &[DisplayCommand],
    scroll_y: f32,
    scale: f32,
    font: Option<&SystemFont>,
    region: Option<(u32, u32, u32, u32)>,
) {
    let framebuffer = &mut *framebuffer;
    // Active transform composition (page coordinates). Axis-aligned
    // transforms map exactly; rotations fall back to the bounding box
    // (the SVG backend renders rotation exactly).
    let mut transforms: Vec<Transform2D> = Vec::new();
    let map_rect = |rect: &Rect, transform: Option<Transform2D>| -> Rect {
        let Some(matrix) = transform else {
            return *rect;
        };
        let corners = [
            matrix.apply(rect.x, rect.y),
            matrix.apply(rect.x + rect.width, rect.y),
            matrix.apply(rect.x, rect.y + rect.height),
            matrix.apply(rect.x + rect.width, rect.y + rect.height),
        ];
        let min_x = corners
            .iter()
            .map(|(x, _)| *x)
            .fold(f32::INFINITY, f32::min);
        let min_y = corners
            .iter()
            .map(|(_, y)| *y)
            .fold(f32::INFINITY, f32::min);
        let max_x = corners
            .iter()
            .map(|(x, _)| *x)
            .fold(f32::NEG_INFINITY, f32::max);
        let max_y = corners
            .iter()
            .map(|(_, y)| *y)
            .fold(f32::NEG_INFINITY, f32::max);
        Rect {
            x: min_x,
            y: min_y,
            width: max_x - min_x,
            height: max_y - min_y,
        }
    };
    let device = |rect: Rect| Rect {
        x: rect.x * scale,
        y: (rect.y - scroll_y) * scale,
        width: rect.width * scale,
        height: rect.height * scale,
    };

    // Clip stack: each entry is the device-space intersection so far.
    // A region seed acts as the outermost clip.
    let mut clips: Vec<(u32, u32, u32, u32)> = region.into_iter().collect();
    framebuffer.clip = clips.last().copied();

    for command in commands {
        let current = transforms.last().copied();
        let shift = |rect: &Rect| device(map_rect(rect, current));
        match command {
            DisplayCommand::DrawMark { rect, color, mark } => {
                draw_mark(framebuffer, &shift(rect), *color, *mark);
            }
            DisplayCommand::PushTransform { matrix } => {
                let composed = match current {
                    Some(outer) => outer.multiply(*matrix),
                    None => *matrix,
                };
                transforms.push(composed);
                continue;
            }
            DisplayCommand::PopTransform => {
                transforms.pop();
                continue;
            }
            DisplayCommand::PushClip { rect } => {
                let rect = shift(rect);
                let x0 = rect.x.max(0.0) as u32;
                let y0 = rect.y.max(0.0) as u32;
                let x1 = (rect.x + rect.width).max(0.0).ceil() as u32;
                let y1 = (rect.y + rect.height).max(0.0).ceil() as u32;
                let outer =
                    clips
                        .last()
                        .copied()
                        .unwrap_or((0, 0, framebuffer.width, framebuffer.height));
                let clip = (
                    x0.max(outer.0),
                    y0.max(outer.1),
                    x1.min(outer.2),
                    y1.min(outer.3),
                );
                clips.push(clip);
                framebuffer.clip = Some(clip);
                continue;
            }
            DisplayCommand::PopClip => {
                clips.pop();
                framebuffer.clip = clips.last().copied();
                continue;
            }
            DisplayCommand::DrawShadow {
                rect,
                radius,
                blur,
                color,
                inset,
            } => {
                draw_shadow(
                    framebuffer,
                    &shift(rect),
                    &scale_radius(radius, scale),
                    blur * scale,
                    *color,
                    *inset,
                );
            }
            DisplayCommand::FillGradient {
                rect,
                radius,
                angle_degrees,
                stops,
                kind,
            } => {
                fill_gradient(
                    framebuffer,
                    &shift(rect),
                    &scale_radius(radius, scale),
                    *angle_degrees,
                    stops,
                    *kind,
                );
            }
            DisplayCommand::FillRect {
                rect,
                color,
                radius,
            } => {
                // Rotated/skewed boxes render exactly by inverse-mapping
                // pixels into the rect's local space; only axis-aligned
                // transforms use the fast path.
                if let Some(matrix) = current
                    && !(matrix.b.abs() < 1e-6 && matrix.c.abs() < 1e-6)
                {
                    let bounds = device(map_rect(rect, Some(matrix)));
                    fill_transformed_rect(
                        framebuffer,
                        rect,
                        matrix,
                        &bounds,
                        scale,
                        scroll_y,
                        radius,
                        *color,
                    );
                    continue;
                }
                let rect = shift(rect);
                if radius.is_zero() {
                    paint_rect(framebuffer, rect, *color);
                } else {
                    fill_rounded(framebuffer, &rect, &scale_radius(radius, scale), *color);
                }
            }
            DisplayCommand::StrokeRect {
                rect,
                widths,
                colors,
                styles,
                radius,
            } => {
                let rect = shift(rect);
                if !radius.is_zero() {
                    // Rounded frames render as a ring in the top edge color
                    // at the top edge width.
                    let width = (widths.top.max(widths.left) * scale).max(1.0);
                    fill_rounded_ring(
                        framebuffer,
                        &rect,
                        &scale_radius(radius, scale),
                        width,
                        colors.top,
                    );
                    continue;
                }
                // Border widths scale too, but stay at least one device
                // pixel so hairline borders never disappear.
                let width_of = |value: f32| {
                    if value > 0.0 {
                        (value * scale).max(1.0)
                    } else {
                        0.0
                    }
                };
                let strips = [
                    (
                        Rect {
                            height: width_of(widths.top),
                            ..rect
                        },
                        colors.top,
                        styles.top,
                        true,
                    ),
                    (
                        Rect {
                            x: rect.x + rect.width - width_of(widths.right),
                            width: width_of(widths.right),
                            ..rect
                        },
                        colors.right,
                        styles.right,
                        false,
                    ),
                    (
                        Rect {
                            y: rect.y + rect.height - width_of(widths.bottom),
                            height: width_of(widths.bottom),
                            ..rect
                        },
                        colors.bottom,
                        styles.bottom,
                        true,
                    ),
                    (
                        Rect {
                            width: width_of(widths.left),
                            ..rect
                        },
                        colors.left,
                        styles.left,
                        false,
                    ),
                ];
                for (strip, color, style, horizontal) in strips {
                    fill_edge(framebuffer, &strip, color, style, horizontal);
                }
            }
            DisplayCommand::DrawImage { rect, image, alpha } => {
                blit_image(framebuffer, &shift(rect), image, *alpha);
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
                letter_spacing,
                decoration_color,
                decoration_style,
            } => {
                // Rotated/skewed text: draw glyphs through the matrix.
                if let (Some(matrix), Some(font)) = (current, font)
                    && !(matrix.b.abs() < 1e-6 && matrix.c.abs() < 1e-6)
                {
                    draw_text_transformed(
                        framebuffer,
                        font,
                        matrix,
                        scale,
                        scroll_y,
                        *x,
                        *y,
                        text,
                        pack(*color),
                        color.a,
                        *font_size,
                        *font_weight,
                        *monospace,
                        *letter_spacing,
                    );
                    continue;
                }
                let (page_x, page_y) = match current {
                    Some(matrix) => matrix.apply(*x, *y),
                    None => (*x, *y),
                };
                // Row-vector lengths give the true scale factor (the old
                // |a|,|d| average shrank text under rotation).
                let text_scale = match current {
                    Some(matrix) => {
                        ((matrix.a * matrix.a + matrix.b * matrix.b).sqrt()
                            + (matrix.c * matrix.c + matrix.d * matrix.d).sqrt())
                            / 2.0
                    }
                    None => 1.0,
                };
                let (x, y, font_size) = (
                    page_x * scale,
                    (page_y - scroll_y) * scale,
                    font_size * text_scale * scale,
                );
                let letter_spacing = letter_spacing * text_scale * scale;
                let packed = pack(*color);
                let text_alpha = color.a;
                let shear = if *italic { 0.21 } else { 0.0 };
                let text_width = match font {
                    Some(font) => draw_text_scalable(
                        framebuffer,
                        font,
                        x,
                        y,
                        text,
                        packed,
                        text_alpha,
                        font_size,
                        *font_weight,
                        shear,
                        *monospace,
                        letter_spacing,
                    ),
                    None => draw_text(
                        framebuffer,
                        x,
                        y,
                        text,
                        packed,
                        text_alpha,
                        font_size,
                        *font_weight,
                        shear,
                        letter_spacing,
                    ),
                };
                if *underline {
                    fill_edge(
                        framebuffer,
                        &Rect {
                            x,
                            y: y + (2.0 * scale).max(1.0),
                            width: text_width,
                            height: scale.max(1.0),
                        },
                        *decoration_color,
                        *decoration_style,
                        true,
                    );
                }
                if *line_through {
                    fill_edge(
                        framebuffer,
                        &Rect {
                            x,
                            y: y - font_size * 0.3,
                            width: text_width,
                            height: scale.max(1.0),
                        },
                        *decoration_color,
                        *decoration_style,
                        true,
                    );
                }
            }
        }
    }
    // Never leak a clip into later overlay passes (e.g. browser chrome).
    framebuffer.clip = None;
}

/// Draws a text run with the 8×8 bitmap font. `y` is the baseline; the
/// glyph cell is `0.5 * font_size` wide (matching the heuristic measurer)
/// and `0.8 * font_size` tall above the baseline. Weights ≥ 600 are
/// emboldened by a 1px double-strike; `shear` fakes italics.
#[allow(clippy::too_many_arguments)]
fn draw_text(
    framebuffer: &mut Framebuffer,
    x: f32,
    y: f32,
    text: &str,
    color: u32,
    alpha: u8,
    font_size: f32,
    font_weight: u16,
    shear: f32,
    letter_spacing: f32,
) -> f32 {
    let advance = font_size * 0.5 + letter_spacing;
    let cell_height = font_size * 0.8;
    let top = y - cell_height;
    let bold = font_weight >= 600;

    for (index, character) in text.chars().enumerate() {
        let Some(glyph) = font8x8::BASIC_FONTS
            .get(character)
            .or_else(|| font8x8::LATIN_FONTS.get(character))
        else {
            continue;
        };
        // Synthetic italic: shift the cell right proportionally to its
        // height above the baseline (crude shear, but visibly slanted).
        let cell_x = x + index as f32 * advance + shear * cell_height * 0.5;
        draw_glyph(
            framebuffer,
            &glyph,
            cell_x,
            top,
            advance,
            cell_height,
            color,
            alpha,
        );
        if bold {
            draw_glyph(
                framebuffer,
                &glyph,
                cell_x + 1.0,
                top,
                advance,
                cell_height,
                color,
                alpha,
            );
        }
    }
    text.chars().count() as f32 * advance
}

/// Nearest-neighbor scales one 8×8 glyph into a cell.
#[allow(clippy::too_many_arguments)]
fn draw_glyph(
    framebuffer: &mut Framebuffer,
    glyph: &[u8; 8],
    cell_x: f32,
    cell_y: f32,
    cell_width: f32,
    cell_height: f32,
    color: u32,
    alpha: u8,
) {
    let x0 = cell_x.max(0.0) as u32;
    let y0 = cell_y.max(0.0) as u32;
    let x1 = ((cell_x + cell_width) as u32).min(framebuffer.width);
    let y1 = ((cell_y + cell_height) as u32).min(framebuffer.height);
    for pixel_y in y0..y1 {
        let source_row = (((pixel_y as f32 - cell_y) / cell_height) * 8.0) as usize;
        let row_bits = glyph[source_row.min(7)];
        for pixel_x in x0..x1 {
            let source_column = (((pixel_x as f32 - cell_x) / cell_width) * 8.0) as usize;
            if row_bits & (1 << source_column.min(7)) != 0 && framebuffer.admits(pixel_x, pixel_y) {
                let position = (pixel_y * framebuffer.width + pixel_x) as usize;
                framebuffer.pixels[position] = blend(framebuffer.pixels[position], color, alpha);
            }
        }
    }
}

/// Draws a text run with a scalable font; `y` is the baseline. Glyph
/// coverage is alpha-blended onto the framebuffer. Weights ≥ 600 get a 1px
/// double-strike (single-face fonts have no real bold); `shear` produces a
/// synthetic italic slant.
#[allow(clippy::too_many_arguments)]
fn draw_text_scalable(
    framebuffer: &mut Framebuffer,
    font: &SystemFont,
    x: f32,
    y: f32,
    text: &str,
    color: u32,
    alpha: u8,
    font_size: f32,
    font_weight: u16,
    shear: f32,
    monospace: bool,
    letter_spacing: f32,
) -> f32 {
    let mut pen_x = x;
    let bold = font_weight >= 600;
    for character in text.chars() {
        let glyph = font.rasterize(character, font_size, monospace);
        let glyph_x = pen_x + glyph.metrics.xmin as f32;
        let glyph_y = y - glyph.metrics.ymin as f32 - glyph.metrics.height as f32;
        blend_glyph(
            framebuffer,
            &glyph.coverage,
            glyph.metrics.width,
            glyph_x,
            glyph_y,
            color,
            alpha,
            y,
            shear,
        );
        if bold {
            blend_glyph(
                framebuffer,
                &glyph.coverage,
                glyph.metrics.width,
                glyph_x + 1.0,
                glyph_y,
                color,
                alpha,
                y,
                shear,
            );
        }
        pen_x += glyph.metrics.advance_width + letter_spacing;
    }
    pen_x - x
}

#[allow(clippy::too_many_arguments)]
fn blend_glyph(
    framebuffer: &mut Framebuffer,
    coverage: &[u8],
    glyph_width: usize,
    origin_x: f32,
    origin_y: f32,
    color: u32,
    alpha_multiplier: u8,
    baseline_y: f32,
    shear: f32,
) {
    if glyph_width == 0 {
        return;
    }
    for (index, alpha) in coverage.iter().enumerate() {
        let alpha = (u32::from(*alpha) * u32::from(alpha_multiplier) / 255) as u8;
        if alpha == 0 {
            continue;
        }
        let row_y = origin_y + (index / glyph_width) as f32;
        // Synthetic italic: rows above the baseline shift right.
        let slant = shear * (baseline_y - row_y).max(0.0);
        let pixel_x = origin_x + (index % glyph_width) as f32 + slant;
        let pixel_y = row_y;
        if pixel_x < 0.0 || pixel_y < 0.0 {
            continue;
        }
        let (pixel_x, pixel_y) = (pixel_x as u32, pixel_y as u32);
        if !framebuffer.admits(pixel_x, pixel_y) {
            continue;
        }
        let position = (pixel_y * framebuffer.width + pixel_x) as usize;
        framebuffer.pixels[position] = blend(framebuffer.pixels[position], color, alpha);
    }
}

/// Fills one border edge strip, segmenting it for dashed/dotted styles.
fn fill_edge(
    framebuffer: &mut Framebuffer,
    strip: &Rect,
    color: Color,
    style: crate::style::BorderStyle,
    horizontal: bool,
) {
    use crate::style::BorderStyle;
    let thickness = if horizontal {
        strip.height
    } else {
        strip.width
    };
    // A zero-thickness edge has a zero dash pattern; segmenting it would
    // never advance.
    if thickness <= 0.0 {
        return;
    }
    let (dash, gap) = match style {
        BorderStyle::Dashed => (3.0 * thickness, 2.0 * thickness),
        BorderStyle::Dotted => (thickness, thickness),
        _ => {
            paint_rect(framebuffer, *strip, color);
            return;
        }
    };
    let length = if horizontal {
        strip.width
    } else {
        strip.height
    };
    let mut offset = 0.0;
    while offset < length {
        let segment = dash.min(length - offset);
        let rect = if horizontal {
            Rect {
                x: strip.x + offset,
                width: segment,
                ..*strip
            }
        } else {
            Rect {
                y: strip.y + offset,
                height: segment,
                ..*strip
            }
        };
        paint_rect(framebuffer, rect, color);
        offset += dash + gap;
    }
}

/// Rasterizes a linear gradient: each pixel projects onto the gradient
/// axis (CSS angle, 0 = to top) and interpolates between the two
/// surrounding stops. Corner radii clip via coverage.
fn fill_gradient(
    framebuffer: &mut Framebuffer,
    rect: &Rect,
    radius: &Corners<f32>,
    angle_degrees: f32,
    stops: &[(Color, f32)],
    kind: GradientKind,
) {
    if stops.is_empty() || rect.width <= 0.0 || rect.height <= 0.0 {
        return;
    }
    let radians = angle_degrees.to_radians();
    let (dx, dy) = (radians.sin(), -radians.cos());
    // Length of the gradient line across the box for this angle.
    let line_length = (rect.width * dx).abs() + (rect.height * dy).abs();
    let (center_x, center_y) = (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
    let rounded = !radius.is_zero();

    let x0 = rect.x.max(0.0) as u32;
    let y0 = rect.y.max(0.0) as u32;
    let x1 = ((rect.x + rect.width).ceil().max(0.0) as u32).min(framebuffer.width);
    let y1 = ((rect.y + rect.height).ceil().max(0.0) as u32).min(framebuffer.height);
    let (x0, y0, x1, y1) = framebuffer.clamp_to_clip(x0, y0, x1, y1);
    for pixel_y in y0..y1 {
        for pixel_x in x0..x1 {
            let (px, py) = (pixel_x as f32 + 0.5, pixel_y as f32 + 0.5);
            let coverage = if rounded {
                rounded_coverage(rect, radius, px, py)
            } else if px < rect.x
                || px >= rect.x + rect.width
                || py < rect.y
                || py >= rect.y + rect.height
            {
                0.0
            } else {
                1.0
            };
            if coverage <= 0.0 {
                continue;
            }
            let progress = match kind {
                GradientKind::Radial => {
                    // Centered ellipse: normalized distance to the edge.
                    let nx = (px - center_x) / (rect.width / 2.0).max(f32::EPSILON);
                    let ny = (py - center_y) / (rect.height / 2.0).max(f32::EPSILON);
                    (nx * nx + ny * ny).sqrt().clamp(0.0, 1.0)
                }
                GradientKind::Conic => {
                    // Sweep angle, 0 at top, clockwise, normalized to a turn.
                    let angle = (px - center_x).atan2(center_y - py);
                    (angle / (2.0 * std::f32::consts::PI)).rem_euclid(1.0)
                }
                GradientKind::Linear if line_length <= 0.0 => 0.0,
                GradientKind::Linear => {
                    (((px - center_x) * dx + (py - center_y) * dy) / line_length + 0.5)
                        .clamp(0.0, 1.0)
                }
            };
            let color = gradient_color_at(stops, progress);
            let alpha = (f32::from(color.a) * coverage) as u8;
            if alpha == 0 {
                continue;
            }
            let position = (pixel_y * framebuffer.width + pixel_x) as usize;
            framebuffer.pixels[position] = blend(framebuffer.pixels[position], pack(color), alpha);
        }
    }
}

/// Fills a (possibly rounded) rect under an arbitrary transform: every
/// device pixel of the transformed bounding box inverse-maps into the
/// rect's local page space, where the ordinary coverage test applies.
#[allow(clippy::too_many_arguments)]
fn fill_transformed_rect(
    framebuffer: &mut Framebuffer,
    page_rect: &Rect,
    matrix: Transform2D,
    device_bounds: &Rect,
    scale: f32,
    scroll_y: f32,
    radius: &Corners<f32>,
    color: Color,
) {
    let Some(inverse) = matrix.inverse() else {
        return;
    };
    let rounded = !radius.is_zero();
    let x0 = (device_bounds.x - 1.0).max(0.0) as u32;
    let y0 = (device_bounds.y - 1.0).max(0.0) as u32;
    let x1 = ((device_bounds.x + device_bounds.width).ceil() + 1.0).max(0.0) as u32;
    let y1 = ((device_bounds.y + device_bounds.height).ceil() + 1.0).max(0.0) as u32;
    let x1 = x1.min(framebuffer.width);
    let y1 = y1.min(framebuffer.height);
    let (x0, y0, x1, y1) = framebuffer.clamp_to_clip(x0, y0, x1, y1);
    for pixel_y in y0..y1 {
        for pixel_x in x0..x1 {
            // Device pixel center → page space → the rect's local space.
            let page_x = (pixel_x as f32 + 0.5) / scale;
            let page_y = (pixel_y as f32 + 0.5) / scale + scroll_y;
            let (local_x, local_y) = inverse.apply(page_x, page_y);
            let coverage = if rounded {
                rounded_coverage(page_rect, radius, local_x, local_y)
            } else {
                // Plain rect with a half-pixel feather for smooth edges.
                let feather = 0.5 / scale;
                let inside_x = (local_x - page_rect.x)
                    .min(page_rect.x + page_rect.width - local_x);
                let inside_y = (local_y - page_rect.y)
                    .min(page_rect.y + page_rect.height - local_y);
                (inside_x.min(inside_y) / feather + 0.5).clamp(0.0, 1.0)
            };
            if coverage <= 0.0 {
                continue;
            }
            let alpha = (f32::from(color.a) * coverage) as u8;
            if alpha == 0 {
                continue;
            }
            let position = (pixel_y * framebuffer.width + pixel_x) as usize;
            framebuffer.pixels[position] =
                blend(framebuffer.pixels[position], pack(color), alpha);
        }
    }
}

/// Draws a text run under an arbitrary transform: every glyph bitmap is
/// inverse-sampled through the matrix, so the glyphs rotate with their
/// box. Decorations (underline/strike) are skipped in this path.
#[allow(clippy::too_many_arguments)]
fn draw_text_transformed(
    framebuffer: &mut Framebuffer,
    font: &SystemFont,
    matrix: Transform2D,
    scale: f32,
    scroll_y: f32,
    x_local: f32,
    y_local: f32,
    text: &str,
    color: u32,
    alpha_multiplier: u8,
    font_size: f32,
    font_weight: u16,
    monospace: bool,
    letter_spacing: f32,
) {
    let Some(inverse) = matrix.inverse() else {
        return;
    };
    let device_size = font_size * scale;
    let bold = font_weight >= 600;
    let mut pen = x_local; // page units along the local baseline
    for character in text.chars() {
        let glyph = font.rasterize(character, device_size, monospace);
        let width = glyph.metrics.width;
        let height = glyph.metrics.height;
        if width > 0 && height > 0 {
            // The bitmap's rect in local page units.
            let gx = pen + glyph.metrics.xmin as f32 / scale;
            let gy = y_local - (glyph.metrics.ymin + glyph.metrics.height as i32) as f32 / scale;
            let gw = width as f32 / scale;
            let gh = height as f32 / scale;
            // Device bounding box of the transformed bitmap rect.
            let corners = [
                matrix.apply(gx, gy),
                matrix.apply(gx + gw, gy),
                matrix.apply(gx, gy + gh),
                matrix.apply(gx + gw, gy + gh),
            ];
            let min_x = corners.iter().map(|(x, _)| *x).fold(f32::INFINITY, f32::min);
            let max_x = corners
                .iter()
                .map(|(x, _)| *x)
                .fold(f32::NEG_INFINITY, f32::max);
            let min_y = corners.iter().map(|(_, y)| *y).fold(f32::INFINITY, f32::min);
            let max_y = corners
                .iter()
                .map(|(_, y)| *y)
                .fold(f32::NEG_INFINITY, f32::max);
            let x0 = ((min_x * scale) - 1.0).max(0.0) as u32;
            let y0 = (((min_y - scroll_y) * scale) - 1.0).max(0.0) as u32;
            let x1 = (((max_x * scale) + 1.0).ceil().max(0.0) as u32).min(framebuffer.width);
            let y1 = ((((max_y - scroll_y) * scale) + 1.0).ceil().max(0.0) as u32)
                .min(framebuffer.height);
            for pixel_y in y0..y1 {
                for pixel_x in x0..x1 {
                    if !framebuffer.admits(pixel_x, pixel_y) {
                        continue;
                    }
                    let page_x = (pixel_x as f32 + 0.5) / scale;
                    let page_y = (pixel_y as f32 + 0.5) / scale + scroll_y;
                    let (local_x, local_y) = inverse.apply(page_x, page_y);
                    let u = (local_x - gx) * scale;
                    let v = (local_y - gy) * scale;
                    if u < 0.0 || v < 0.0 || u >= width as f32 || v >= height as f32 {
                        continue;
                    }
                    let coverage = glyph.coverage[v as usize * width + u as usize];
                    // A synthetic bold double-strike is approximated by a
                    // second sample one device pixel to the left.
                    let coverage = if bold && u >= 1.0 {
                        coverage.max(glyph.coverage[v as usize * width + (u as usize - 1)])
                    } else {
                        coverage
                    };
                    let alpha =
                        (u32::from(coverage) * u32::from(alpha_multiplier) / 255) as u8;
                    if alpha == 0 {
                        continue;
                    }
                    let position = (pixel_y * framebuffer.width + pixel_x) as usize;
                    framebuffer.pixels[position] =
                        blend(framebuffer.pixels[position], color, alpha);
                }
            }
        }
        pen += glyph.metrics.advance_width / scale + letter_spacing;
    }
}

/// Interpolates the stop list at `progress` (0..=1).
fn gradient_color_at(stops: &[(Color, f32)], progress: f32) -> Color {
    let mut previous = &stops[0];
    if progress <= previous.1 {
        return previous.0;
    }
    for stop in &stops[1..] {
        if progress <= stop.1 {
            let span = (stop.1 - previous.1).max(f32::EPSILON);
            let t = (progress - previous.1) / span;
            let lerp =
                |a: u8, b: u8| -> u8 { (f32::from(a) + (f32::from(b) - f32::from(a)) * t) as u8 };
            return Color {
                r: lerp(previous.0.r, stop.0.r),
                g: lerp(previous.0.g, stop.0.g),
                b: lerp(previous.0.b, stop.0.b),
                a: lerp(previous.0.a, stop.0.a),
            };
        }
        previous = stop;
    }
    stops[stops.len() - 1].0
}

/// Abramowitz–Stegun approximation of the error function (max error
/// ~2.5e-5) — the building block of analytic Gaussian box shadows.
#[allow(clippy::excessive_precision)]
fn erf(x: f32) -> f32 {
    let sign = x.signum();
    let x = x.abs();
    let t = 1.0 / (1.0 + 0.327_591_1 * x);
    let y = 1.0
        - (((((1.061_405_429 * t - 1.453_152_027) * t) + 1.421_413_741) * t - 0.284_496_736) * t
            + 0.254_829_592)
            * t
            * (-x * x).exp();
    sign * y
}

/// Rasterizes a box shadow with a true Gaussian falloff. CSS blur radius
/// ≈ 2σ. Outer shadows shade `coverage`, inset shadows its complement
/// clipped to the box.
fn draw_shadow(
    framebuffer: &mut Framebuffer,
    rect: &Rect,
    radius: &Corners<f32>,
    blur: f32,
    color: Color,
    inset: bool,
) {
    let sigma = (blur / 2.0).max(0.01);
    let reach = if inset { 0.0 } else { blur.max(1.0) * 1.5 };
    let x0 = ((rect.x - reach).floor().max(0.0)) as u32;
    let y0 = ((rect.y - reach).floor().max(0.0)) as u32;
    let x1 = (((rect.x + rect.width + reach).ceil()).max(0.0) as u32).min(framebuffer.width);
    let y1 = (((rect.y + rect.height + reach).ceil()).max(0.0) as u32).min(framebuffer.height);
    // Only iterate pixels inside the clip: a scroll's exposed strip must
    // not pay for the whole (expensive, Gaussian) shadow box every frame.
    let (x0, y0, x1, y1) = framebuffer.clamp_to_clip(x0, y0, x1, y1);
    let rounded = !radius.is_zero();
    let packed = pack(color);
    // The Gaussian box coverage is separable: coverage(x, y) = X(x)·Y(y).
    // Precompute the column terms once and the row term once per row, so a
    // pixel costs one multiply instead of four erf evaluations.
    let denominator = sigma * std::f32::consts::SQRT_2;
    let column_terms: Vec<f32> = if blur > 0.0 {
        (x0..x1)
            .map(|pixel_x| {
                let px = pixel_x as f32 + 0.5;
                0.5 * (erf((px - rect.x) / denominator)
                    - erf((px - (rect.x + rect.width)) / denominator))
            })
            .collect()
    } else {
        Vec::new()
    };
    // The column span where the Gaussian has already saturated: combined
    // with a saturated row it gives constant full coverage, so that stretch
    // blends at a fixed alpha instead of evaluating the falloff per pixel.
    let saturated = |terms: &[f32]| -> (u32, u32) {
        let first = terms.iter().position(|term| *term >= 0.999);
        let last = terms.iter().rposition(|term| *term >= 0.999);
        match (first, last) {
            (Some(first), Some(last)) if last > first => (x0 + first as u32, x0 + last as u32 + 1),
            _ => (x0, x0),
        }
    };
    let (solid_x0, solid_x1) = if blur > 0.0 && !inset {
        saturated(&column_terms)
    } else {
        (x0, x0)
    };
    for pixel_y in y0..y1 {
        let row_term = if blur > 0.0 {
            let py = pixel_y as f32 + 0.5;
            0.5 * (erf((py - rect.y) / denominator)
                - erf((py - (rect.y + rect.height)) / denominator))
        } else {
            0.0
        };
        // Rows the Gaussian never reaches contribute nothing at all.
        if blur > 0.0 && !inset && row_term <= 0.0 {
            continue;
        }
        // Fully saturated row: the middle stretch is a flat run.
        if row_term >= 0.999 && solid_x1 > solid_x0 {
            let row = (pixel_y * framebuffer.width) as usize;
            for position in row + solid_x0 as usize..row + solid_x1 as usize {
                framebuffer.pixels[position] = blend(framebuffer.pixels[position], packed, color.a);
            }
        }
        for pixel_x in x0..x1 {
            // Already covered by the flat run above.
            if row_term >= 0.999 && pixel_x >= solid_x0 && pixel_x < solid_x1 {
                continue;
            }
            let (px, py) = (pixel_x as f32 + 0.5, pixel_y as f32 + 0.5);
            let coverage = if blur <= 0.0 {
                // Hard shadow: plain (rounded) box coverage.
                if rounded {
                    rounded_coverage(rect, radius, px, py)
                } else if px >= rect.x
                    && px < rect.x + rect.width
                    && py >= rect.y
                    && py < rect.y + rect.height
                {
                    1.0
                } else {
                    0.0
                }
            } else {
                (column_terms[(pixel_x - x0) as usize] * row_term).clamp(0.0, 1.0)
            };
            let coverage = if inset {
                // Inset: the complement, clipped to the box itself.
                let inside = if rounded {
                    rounded_coverage(rect, radius, px, py)
                } else if px >= rect.x
                    && px < rect.x + rect.width
                    && py >= rect.y
                    && py < rect.y + rect.height
                {
                    1.0
                } else {
                    0.0
                };
                (1.0 - coverage) * inside
            } else {
                coverage
            };
            let alpha = (f32::from(color.a) * coverage) as u8;
            if alpha == 0 {
                continue;
            }
            let position = (pixel_y * framebuffer.width + pixel_x) as usize;
            framebuffer.pixels[position] = blend(framebuffer.pixels[position], packed, alpha);
        }
    }
}

/// Anti-aliased thick line segment: coverage from the distance to the
/// segment, with a ~1px soft edge.
fn draw_segment(
    framebuffer: &mut Framebuffer,
    from: (f32, f32),
    to: (f32, f32),
    thickness: f32,
    color: Color,
) {
    let radius = thickness / 2.0;
    let x0 = ((from.0.min(to.0) - radius - 1.0).floor().max(0.0)) as u32;
    let y0 = ((from.1.min(to.1) - radius - 1.0).floor().max(0.0)) as u32;
    let x1 = (((from.0.max(to.0) + radius + 1.0).ceil()).max(0.0) as u32).min(framebuffer.width);
    let y1 = (((from.1.max(to.1) + radius + 1.0).ceil()).max(0.0) as u32).min(framebuffer.height);
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let length_squared = (dx * dx + dy * dy).max(f32::EPSILON);
    let packed = pack(color);
    for pixel_y in y0..y1 {
        for pixel_x in x0..x1 {
            if !framebuffer.admits(pixel_x, pixel_y) {
                continue;
            }
            let (px, py) = (pixel_x as f32 + 0.5, pixel_y as f32 + 0.5);
            let t = (((px - from.0) * dx + (py - from.1) * dy) / length_squared).clamp(0.0, 1.0);
            let (cx, cy) = (from.0 + t * dx, from.1 + t * dy);
            let distance = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
            let coverage = (radius + 0.5 - distance).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            let alpha = (f32::from(color.a) * coverage) as u8;
            let position = (pixel_y * framebuffer.width + pixel_x) as usize;
            framebuffer.pixels[position] = blend(framebuffer.pixels[position], packed, alpha);
        }
    }
}

/// Draws a control mark: a two-segment check tick, or a centered disc.
fn draw_mark(framebuffer: &mut Framebuffer, rect: &Rect, color: Color, mark: Mark) {
    match mark {
        Mark::Check => {
            // Classic tick: 25%→45% down-stroke, 45%→78% up-stroke.
            let point = |fx: f32, fy: f32| (rect.x + rect.width * fx, rect.y + rect.height * fy);
            let thickness = (rect.width.min(rect.height) * 0.16).max(1.4);
            draw_segment(
                framebuffer,
                point(0.24, 0.55),
                point(0.43, 0.74),
                thickness,
                color,
            );
            draw_segment(
                framebuffer,
                point(0.43, 0.74),
                point(0.78, 0.3),
                thickness,
                color,
            );
        }
        Mark::Dot => {
            let inset = rect.width * 0.3;
            let disc = Rect {
                x: rect.x + inset,
                y: rect.y + inset,
                width: rect.width - 2.0 * inset,
                height: rect.height - 2.0 * inset,
            };
            let radius = Corners::uniform(disc.width / 2.0);
            fill_rounded(framebuffer, &disc, &radius, color);
        }
        Mark::Arrow => {
            // Small ∨ near the right edge.
            let cx = rect.x + rect.width - 12.0;
            let cy = rect.y + rect.height / 2.0;
            let arm = 3.5;
            let stroke = Color::rgba(0x55, 0x52, 0x5c, color.a);
            draw_segment(
                framebuffer,
                (cx - arm, cy - 1.5),
                (cx, cy + 2.5),
                1.6,
                stroke,
            );
            draw_segment(
                framebuffer,
                (cx, cy + 2.5),
                (cx + arm, cy - 1.5),
                1.6,
                stroke,
            );
        }
        Mark::Fraction(fraction) => {
            // Filled bar to the fraction; sliders add a thumb disc.
            let fill = Rect {
                width: rect.width * fraction.fraction,
                ..*rect
            };
            let radius = Corners::uniform(rect.height / 2.0);
            fill_rounded(
                framebuffer,
                &fill,
                &radius,
                Color::rgba(0x22, 0x66, 0xaa, color.a),
            );
            if fraction.thumb {
                let diameter = rect.height + 4.0;
                let disc = Rect {
                    x: (rect.x + rect.width * fraction.fraction - diameter / 2.0)
                        .clamp(rect.x - 2.0, rect.x + rect.width - diameter + 2.0),
                    y: rect.y + rect.height / 2.0 - diameter / 2.0,
                    width: diameter,
                    height: diameter,
                };
                fill_rounded(
                    framebuffer,
                    &disc,
                    &Corners::uniform(diameter / 2.0),
                    Color::rgba(0x1c, 0x52, 0x88, color.a),
                );
            }
        }
    }
}

fn scale_radius(radius: &Corners<f32>, scale: f32) -> Corners<f32> {
    Corners {
        top_left: radius.top_left * scale,
        top_right: radius.top_right * scale,
        bottom_right: radius.bottom_right * scale,
        bottom_left: radius.bottom_left * scale,
    }
}

/// Antialiased coverage of a point inside a rounded rectangle:
/// 1 inside, 0 outside, a ~1px ramp at curved corners.
fn rounded_coverage(rect: &Rect, radius: &Corners<f32>, x: f32, y: f32) -> f32 {
    if x < rect.x || x >= rect.x + rect.width || y < rect.y || y >= rect.y + rect.height {
        return 0.0;
    }
    let corners = [
        (
            rect.x + radius.top_left,
            rect.y + radius.top_left,
            radius.top_left,
        ),
        (
            rect.x + rect.width - radius.top_right,
            rect.y + radius.top_right,
            radius.top_right,
        ),
        (
            rect.x + rect.width - radius.bottom_right,
            rect.y + rect.height - radius.bottom_right,
            radius.bottom_right,
        ),
        (
            rect.x + radius.bottom_left,
            rect.y + rect.height - radius.bottom_left,
            radius.bottom_left,
        ),
    ];
    for (index, (cx, cy, r)) in corners.iter().enumerate() {
        if *r <= 0.0 {
            continue;
        }
        let in_corner_cell = match index {
            0 => x < *cx && y < *cy,
            1 => x >= *cx && y < *cy,
            2 => x >= *cx && y >= *cy,
            _ => x < *cx && y >= *cy,
        };
        if in_corner_cell {
            let distance = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
            return (r - distance + 0.5).clamp(0.0, 1.0);
        }
    }
    1.0
}

/// Fills a rounded rectangle with antialiased corners.
fn fill_rounded(framebuffer: &mut Framebuffer, rect: &Rect, radius: &Corners<f32>, color: Color) {
    let packed = pack(color);
    let color_alpha = f32::from(color.a) / 255.0;
    let radius = radius.clamped_to(rect.width, rect.height);
    let x0 = (rect.x.max(0.0) as u32).min(framebuffer.width);
    let y0 = (rect.y.max(0.0) as u32).min(framebuffer.height);
    let x1 = ((rect.x + rect.width).ceil().max(0.0) as u32).min(framebuffer.width);
    let y1 = ((rect.y + rect.height).ceil().max(0.0) as u32).min(framebuffer.height);
    let (x0, y0, x1, y1) = framebuffer.clamp_to_clip(x0, y0, x1, y1);
    // Coverage is exactly 1 away from the corners and the anti-aliased
    // edges, so only the corner bands and a one-pixel border need the
    // per-pixel distance math; the interior is a plain fill. Without this a
    // full-width rounded box costs a `rounded_coverage` per pixel.
    let solid_x0 = (rect.x.ceil() as i64 + 1).clamp(x0 as i64, x1 as i64) as u32;
    let solid_x1 = ((rect.x + rect.width).floor() as i64 - 1).clamp(x0 as i64, x1 as i64) as u32;
    let corner_top = ((rect.y + radius.top_left.max(radius.top_right)).ceil() as i64 + 1)
        .clamp(y0 as i64, y1 as i64) as u32;
    let corner_bottom = ((rect.y + rect.height
        - radius.bottom_left.max(radius.bottom_right))
    .floor() as i64
        - 1)
    .clamp(y0 as i64, y1 as i64) as u32;
    let solid_alpha = (color_alpha * 255.0) as u8;
    for pixel_y in y0..y1 {
        let shade = |framebuffer: &mut Framebuffer, from: u32, to: u32| {
            for pixel_x in from..to {
                let coverage =
                    rounded_coverage(rect, &radius, pixel_x as f32 + 0.5, pixel_y as f32 + 0.5);
                if coverage <= 0.0 {
                    continue;
                }
                let position = (pixel_y * framebuffer.width + pixel_x) as usize;
                framebuffer.pixels[position] = blend(
                    framebuffer.pixels[position],
                    packed,
                    (coverage * color_alpha * 255.0) as u8,
                );
            }
        };
        // Rows beside a corner (or the top/bottom edge) stay per-pixel.
        if pixel_y < corner_top || pixel_y >= corner_bottom || solid_x1 <= solid_x0 {
            shade(framebuffer, x0, x1);
            continue;
        }
        shade(framebuffer, x0, solid_x0);
        let row = (pixel_y * framebuffer.width) as usize;
        let span = row + solid_x0 as usize..row + solid_x1 as usize;
        if solid_alpha == 255 {
            framebuffer.pixels[span].fill(packed);
        } else {
            for position in span {
                framebuffer.pixels[position] =
                    blend(framebuffer.pixels[position], packed, solid_alpha);
            }
        }
        shade(framebuffer, solid_x1, x1);
    }
}

/// Fills the ring between a rounded rect and its inner inset.
fn fill_rounded_ring(
    framebuffer: &mut Framebuffer,
    rect: &Rect,
    radius: &Corners<f32>,
    width: f32,
    color: Color,
) {
    let packed = pack(color);
    let color_alpha = f32::from(color.a) / 255.0;
    let radius = radius.clamped_to(rect.width, rect.height);
    let inner = Rect {
        x: rect.x + width,
        y: rect.y + width,
        width: (rect.width - 2.0 * width).max(0.0),
        height: (rect.height - 2.0 * width).max(0.0),
    };
    let inner_radius = Corners {
        top_left: (radius.top_left - width).max(0.0),
        top_right: (radius.top_right - width).max(0.0),
        bottom_right: (radius.bottom_right - width).max(0.0),
        bottom_left: (radius.bottom_left - width).max(0.0),
    };
    let x0 = (rect.x.max(0.0) as u32).min(framebuffer.width);
    let y0 = (rect.y.max(0.0) as u32).min(framebuffer.height);
    let x1 = ((rect.x + rect.width).ceil().max(0.0) as u32).min(framebuffer.width);
    let y1 = ((rect.y + rect.height).ceil().max(0.0) as u32).min(framebuffer.height);
    let (x0, y0, x1, y1) = framebuffer.clamp_to_clip(x0, y0, x1, y1);
    // The ring is hollow: on rows clear of the top/bottom bands and the
    // corners, only the two side bands can have coverage, so the whole
    // interior span is skipped instead of evaluating it per pixel.
    let side_band = width.ceil() + 2.0;
    let left_band = ((rect.x + side_band).ceil() as i64).clamp(x0 as i64, x1 as i64) as u32;
    let right_band = ((rect.x + rect.width - side_band).floor() as i64)
        .clamp(x0 as i64, x1 as i64) as u32;
    let corner_top = ((rect.y + side_band + radius.top_left.max(radius.top_right)).ceil() as i64)
        .clamp(y0 as i64, y1 as i64) as u32;
    let corner_bottom = ((rect.y + rect.height
        - side_band
        - radius.bottom_left.max(radius.bottom_right))
    .floor() as i64)
        .clamp(y0 as i64, y1 as i64) as u32;
    for pixel_y in y0..y1 {
        let shade = |framebuffer: &mut Framebuffer, from: u32, to: u32| {
            for pixel_x in from..to {
                let (px, py) = (pixel_x as f32 + 0.5, pixel_y as f32 + 0.5);
                let coverage = rounded_coverage(rect, &radius, px, py)
                    - rounded_coverage(&inner, &inner_radius, px, py);
                if coverage <= 0.0 {
                    continue;
                }
                let position = (pixel_y * framebuffer.width + pixel_x) as usize;
                framebuffer.pixels[position] = blend(
                    framebuffer.pixels[position],
                    packed,
                    (coverage * color_alpha * 255.0) as u8,
                );
            }
        };
        if pixel_y < corner_top || pixel_y >= corner_bottom || right_band <= left_band {
            shade(framebuffer, x0, x1);
        } else {
            shade(framebuffer, x0, left_band);
            shade(framebuffer, right_band, x1);
        }
    }
}

/// Nearest-neighbor blit of an RGBA image into `rect`, alpha-blended.
fn blit_image(
    framebuffer: &mut Framebuffer,
    rect: &Rect,
    image: &crate::image::RasterImage,
    alpha_multiplier: u8,
) {
    if rect.width <= 0.0 || rect.height <= 0.0 || image.width == 0 || image.height == 0 {
        return;
    }
    let x0 = (rect.x.max(0.0) as u32).min(framebuffer.width);
    let y0 = (rect.y.max(0.0) as u32).min(framebuffer.height);
    let x1 = ((rect.x + rect.width).max(0.0) as u32).min(framebuffer.width);
    let y1 = ((rect.y + rect.height).max(0.0) as u32).min(framebuffer.height);
    for pixel_y in y0..y1 {
        let v = ((pixel_y as f32 - rect.y) / rect.height).clamp(0.0, 1.0);
        let source_y = ((v * image.height as f32) as u32).min(image.height - 1);
        for pixel_x in x0..x1 {
            let u = ((pixel_x as f32 - rect.x) / rect.width).clamp(0.0, 1.0);
            let source_x = ((u * image.width as f32) as u32).min(image.width - 1);
            let offset = ((source_y * image.width + source_x) * 4) as usize;
            let [r, g, b, a] = image.rgba[offset..offset + 4] else {
                continue;
            };
            if !framebuffer.admits(pixel_x, pixel_y) {
                continue;
            }
            let color = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
            let position = (pixel_y * framebuffer.width + pixel_x) as usize;
            let combined = (u32::from(a) * u32::from(alpha_multiplier) / 255) as u8;
            framebuffer.pixels[position] = blend(framebuffer.pixels[position], color, combined);
        }
    }
}

/// Linear interpolation of two 0RGB pixels by an 8-bit alpha.
fn blend(background: u32, foreground: u32, alpha: u8) -> u32 {
    let alpha = u32::from(alpha);
    let inverse = 255 - alpha;
    let channel = |shift: u32| {
        let back = (background >> shift) & 0xff;
        let front = (foreground >> shift) & 0xff;
        ((front * alpha + back * inverse) / 255) << shift
    };
    channel(16) | channel(8) | channel(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::EdgeSizes;

    const RED: Color = Color::rgb(255, 0, 0);

    #[test]
    fn fill_rect_paints_exact_pixels() {
        let commands = vec![DisplayCommand::FillRect {
            rect: Rect {
                x: 2.0,
                y: 3.0,
                width: 4.0,
                height: 2.0,
            },
            color: RED,
            radius: Corners::uniform(0.0),
        }];
        let framebuffer = rasterize(&commands, 10, 10, 0.0);
        assert_eq!(framebuffer.pixel(2, 3), 0x00ff_0000);
        assert_eq!(framebuffer.pixel(5, 4), 0x00ff_0000);
        assert_eq!(framebuffer.pixel(6, 3), 0x00ff_ffff); // right edge exclusive
        assert_eq!(framebuffer.pixel(2, 5), 0x00ff_ffff); // bottom edge exclusive
    }

    #[test]
    fn stroke_rect_leaves_interior_unpainted() {
        let commands = vec![DisplayCommand::StrokeRect {
            rect: Rect {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            },
            widths: EdgeSizes::uniform(1.0),
            colors: EdgeSizes::uniform(RED),
            styles: EdgeSizes::uniform(crate::style::BorderStyle::Solid),
            radius: Corners::uniform(0.0),
        }];
        let framebuffer = rasterize(&commands, 10, 10, 0.0);
        assert_eq!(framebuffer.pixel(0, 0), 0x00ff_0000);
        assert_eq!(framebuffer.pixel(9, 9), 0x00ff_0000);
        assert_eq!(framebuffer.pixel(5, 5), 0x00ff_ffff);
    }

    #[test]
    fn region_rasterization_touches_only_the_region() {
        let commands = vec![DisplayCommand::FillRect {
            rect: Rect {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            },
            color: Color::rgb(0xff, 0x00, 0x00),
            radius: Corners::uniform(0.0),
        }];
        // Start from a green buffer so untouched pixels are detectable.
        let mut framebuffer = Framebuffer::new(10, 10);
        framebuffer.fill(
            Rect {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            },
            0x0000_ff00,
        );
        rasterize_region(&mut framebuffer, &commands, 0.0, 1.0, None, (0, 6, 10, 10));
        assert_eq!(framebuffer.pixel(5, 7), 0x00ff_0000); // inside region
        assert_eq!(framebuffer.pixel(5, 5), 0x0000_ff00); // untouched
        // A scrolled region matches a full scrolled rasterization.
        let full = rasterize(&commands, 10, 10, 4.0);
        let mut partial = Framebuffer::new(10, 10);
        rasterize_region(&mut partial, &commands, 4.0, 1.0, None, (0, 0, 10, 10));
        assert_eq!(full.pixels, partial.pixels);
    }

    #[test]
    fn gradients_interpolate_across_the_rect() {
        let commands = vec![DisplayCommand::FillGradient {
            rect: Rect {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 4.0,
            },
            radius: Corners::uniform(0.0),
            angle_degrees: 90.0, // to right
            stops: vec![(Color::rgb(0, 0, 0), 0.0), (Color::rgb(255, 255, 255), 1.0)],
            kind: GradientKind::Linear,
        }];
        let framebuffer = rasterize(&commands, 10, 4, 0.0);
        let left = framebuffer.pixel(0, 2) & 0xff;
        let middle = framebuffer.pixel(5, 2) & 0xff;
        let right = framebuffer.pixel(9, 2) & 0xff;
        assert!(left < middle && middle < right, "{left} {middle} {right}");
        assert!(left < 40, "{left}");
        assert!(right > 215, "{right}");
    }

    #[test]
    fn off_screen_clips_paint_nothing_without_panicking() {
        // Regression: a clip entirely right of the buffer made bounds()
        // return min > max and clamp() panicked.
        let commands = vec![
            DisplayCommand::PushClip {
                rect: Rect {
                    x: 1800.0,
                    y: 0.0,
                    width: 100.0,
                    height: 100.0,
                },
            },
            DisplayCommand::FillRect {
                rect: Rect {
                    x: 1800.0,
                    y: 0.0,
                    width: 100.0,
                    height: 100.0,
                },
                color: Color::rgb(0xff, 0x00, 0x00),
                radius: Corners::uniform(0.0),
            },
            DisplayCommand::PopClip,
        ];
        let framebuffer = rasterize(&commands, 10, 10, 0.0);
        assert_eq!(framebuffer.pixel(5, 5), 0x00ff_ffff);
    }

    #[test]
    fn shadows_fall_off_smoothly() {
        let commands = vec![DisplayCommand::DrawShadow {
            rect: Rect {
                x: 20.0,
                y: 20.0,
                width: 20.0,
                height: 20.0,
            },
            radius: Corners::uniform(0.0),
            blur: 12.0,
            color: Color::rgb(0, 0, 0),
            inset: false,
        }];
        let framebuffer = rasterize(&commands, 60, 60, 0.0);
        // Sampling outward from the center: strictly darker → lighter,
        // with no repeated banding plateaus near the edge.
        let samples: Vec<u32> = (0..12)
            .map(|step| framebuffer.pixel(30 + step * 2, 30) & 0xff)
            .collect();
        for pair in samples.windows(2) {
            assert!(pair[0] <= pair[1], "{samples:?}");
        }
        assert!(samples[0] < 60, "center dark: {samples:?}");
        assert!(
            *samples.last().unwrap() > 240,
            "far edge light: {samples:?}"
        );
    }

    #[test]
    fn primitives_outside_the_clip_region_draw_nothing() {
        // A region repaint (scroll strip) whose clip misses the primitives
        // entirely: the clamped bounds must stay ordered, not invert.
        let far = Rect {
            x: 200.0,
            y: 200.0,
            width: 80.0,
            height: 60.0,
        };
        let commands = vec![
            DisplayCommand::FillRect {
                rect: far,
                color: Color::rgb(0xff, 0, 0),
                radius: Corners::uniform(8.0),
            },
            DisplayCommand::StrokeRect {
                rect: far,
                widths: EdgeSizes::uniform(2.0),
                colors: EdgeSizes::uniform(Color::rgb(0, 0xff, 0)),
                styles: EdgeSizes::uniform(crate::style::BorderStyle::Solid),
                radius: Corners::uniform(8.0),
            },
            DisplayCommand::DrawShadow {
                rect: far,
                radius: Corners::uniform(8.0),
                blur: 12.0,
                color: Color::rgb(0, 0, 0),
                inset: false,
            },
        ];
        let mut framebuffer = Framebuffer::new(300, 300);
        rasterize_region(&mut framebuffer, &commands, 0.0, 1.0, None, (0, 0, 300, 40));
        // The strip is seeded white and nothing reaches into it.
        assert_eq!(framebuffer.pixel(210, 20), 0x00ff_ffff);
    }

    #[test]
    fn conic_gradients_sweep_by_angle() {
        let commands = vec![DisplayCommand::FillGradient {
            rect: Rect {
                x: 0.0,
                y: 0.0,
                width: 20.0,
                height: 20.0,
            },
            radius: Corners::uniform(0.0),
            angle_degrees: 0.0,
            stops: vec![(Color::rgb(0, 0, 0), 0.0), (Color::rgb(255, 255, 255), 1.0)],
            kind: GradientKind::Conic,
        }];
        let framebuffer = rasterize(&commands, 20, 20, 0.0);
        // Just right of top-center: near the 0-turn start (dark).
        let start = framebuffer.pixel(11, 2) & 0xff;
        // Just left of top-center: near the full turn (light).
        let end = framebuffer.pixel(9, 2) & 0xff;
        assert!(start < 40, "{start}");
        assert!(end > 215, "{end}");
    }

    #[test]
    fn clips_drop_pixels_outside_the_clip_rect() {
        let commands = vec![
            DisplayCommand::PushClip {
                rect: Rect {
                    x: 2.0,
                    y: 2.0,
                    width: 4.0,
                    height: 4.0,
                },
            },
            DisplayCommand::FillRect {
                rect: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 10.0,
                    height: 10.0,
                },
                color: Color::rgb(0xff, 0x00, 0x00),
                radius: Corners::uniform(0.0),
            },
            DisplayCommand::PopClip,
            DisplayCommand::FillRect {
                rect: Rect {
                    x: 8.0,
                    y: 8.0,
                    width: 1.0,
                    height: 1.0,
                },
                color: Color::rgb(0x00, 0xff, 0x00),
                radius: Corners::uniform(0.0),
            },
        ];
        let framebuffer = rasterize(&commands, 10, 10, 0.0);
        assert_eq!(framebuffer.pixel(3, 3), 0x00ff_0000); // inside clip
        assert_eq!(framebuffer.pixel(1, 1), 0x00ff_ffff); // clipped away
        assert_eq!(framebuffer.pixel(8, 8), 0x0000_ff00); // after PopClip
    }

    #[test]
    fn scroll_shifts_content_up() {
        let commands = vec![DisplayCommand::FillRect {
            rect: Rect {
                x: 0.0,
                y: 100.0,
                width: 2.0,
                height: 2.0,
            },
            color: RED,
            radius: Corners::uniform(0.0),
        }];
        let unscrolled = rasterize(&commands, 10, 10, 0.0);
        assert_eq!(unscrolled.pixel(0, 0), 0x00ff_ffff);
        let scrolled = rasterize(&commands, 10, 10, 100.0);
        assert_eq!(scrolled.pixel(0, 0), 0x00ff_0000);
    }

    #[test]
    fn text_paints_some_pixels_in_its_cell() {
        let commands = vec![DisplayCommand::DrawText {
            x: 0.0,
            y: 16.0,
            text: "M".to_string(),
            color: RED,
            font_size: 16.0,
            font_weight: 400,
            underline: false,
            italic: false,
            monospace: false,
            line_through: false,
            letter_spacing: 0.0,
            decoration_color: Color::rgb(0, 0, 0),
            decoration_style: crate::style::BorderStyle::Solid,
        }];
        let framebuffer = rasterize(&commands, 20, 20, 0.0);
        let painted = framebuffer
            .pixels
            .iter()
            .filter(|pixel| **pixel == 0x00ff_0000)
            .count();
        assert!(painted > 4, "expected glyph pixels, found {painted}");
    }

    #[test]
    fn scale_factor_maps_css_to_physical_pixels() {
        let commands = vec![DisplayCommand::FillRect {
            rect: Rect {
                x: 2.0,
                y: 1.0,
                width: 3.0,
                height: 2.0,
            },
            color: RED,
            radius: Corners::uniform(0.0),
        }];
        let framebuffer = rasterize_with(&commands, 20, 20, 0.0, 2.0, None);
        assert_eq!(framebuffer.pixel(4, 2), 0x00ff_0000);
        assert_eq!(framebuffer.pixel(9, 5), 0x00ff_0000); // exclusive at 10,6
        assert_eq!(framebuffer.pixel(10, 2), 0x00ff_ffff);
        assert_eq!(framebuffer.pixel(3, 2), 0x00ff_ffff);
    }

    #[test]
    fn dotted_edges_leave_gaps() {
        let commands = vec![DisplayCommand::StrokeRect {
            rect: Rect {
                x: 0.0,
                y: 0.0,
                width: 20.0,
                height: 10.0,
            },
            widths: EdgeSizes {
                top: 2.0,
                right: 0.0,
                bottom: 0.0,
                left: 0.0,
            },
            colors: EdgeSizes::uniform(RED),
            styles: EdgeSizes::uniform(crate::style::BorderStyle::Dotted),
            radius: Corners::uniform(0.0),
        }];
        let framebuffer = rasterize(&commands, 20, 10, 0.0);
        // Dot (0..2), gap (2..4), dot (4..6): 2px on / 2px off.
        assert_eq!(framebuffer.pixel(0, 0), 0x00ff_0000);
        assert_eq!(framebuffer.pixel(2, 0), 0x00ff_ffff);
        assert_eq!(framebuffer.pixel(4, 0), 0x00ff_0000);
    }

    #[test]
    fn rounded_fill_leaves_corners_unpainted() {
        let commands = vec![DisplayCommand::FillRect {
            rect: Rect {
                x: 0.0,
                y: 0.0,
                width: 20.0,
                height: 20.0,
            },
            color: RED,
            radius: Corners::uniform(8.0),
        }];
        let framebuffer = rasterize(&commands, 20, 20, 0.0);
        // Extreme corner pixel is outside the 8px arc, center is inside.
        assert_eq!(framebuffer.pixel(0, 0), 0x00ff_ffff);
        assert_eq!(framebuffer.pixel(10, 10), 0x00ff_0000);
        // On the arc's midpoint the edge is inside.
        assert_eq!(framebuffer.pixel(3, 3), 0x00ff_0000);
    }

    #[test]
    fn out_of_bounds_geometry_is_clipped_safely() {
        let commands = vec![DisplayCommand::FillRect {
            rect: Rect {
                x: -5.0,
                y: -5.0,
                width: 100.0,
                height: 100.0,
            },
            color: RED,
            radius: Corners::uniform(0.0),
        }];
        let framebuffer = rasterize(&commands, 4, 4, 0.0);
        assert!(framebuffer.pixels.iter().all(|pixel| *pixel == 0x00ff_0000));
    }
}
