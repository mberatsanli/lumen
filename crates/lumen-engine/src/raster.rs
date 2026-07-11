//! Software rasterizer: display list → ARGB pixel buffer.
//!
//! The second display-list backend after SVG, meant for the desktop shell.
//! Text uses the built-in 8×8 bitmap font (`font8x8`) scaled to the font
//! size — deliberately crude but fully self-contained and deterministic.
//! Glyph cells are half an em wide to match [`crate::HeuristicMeasurer`],
//! so painted text agrees with layout's line breaking.

use crate::font::SystemFont;
use crate::geometry::{Corners, Rect};
use crate::paint::DisplayCommand;
use font8x8::UnicodeFonts;
use lumen_css::Color;

/// A row-major 32-bit `0RGB` pixel buffer (softbuffer's native format).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
}

impl Framebuffer {
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![0x00ff_ffff; (width as usize) * (height as usize)],
        }
    }

    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> u32 {
        self.pixels[(y * self.width + x) as usize]
    }

    /// Blends `color` over a rectangle at the given alpha — used for
    /// translucent overlays like text-selection highlights.
    pub fn blend_fill(&mut self, rect: Rect, color: lumen_css::Color, alpha: u8) {
        let packed = pack(color);
        let x0 = (rect.x.max(0.0) as u32).min(self.width);
        let y0 = (rect.y.max(0.0) as u32).min(self.height);
        let x1 = ((rect.x + rect.width).max(0.0) as u32).min(self.width);
        let y1 = ((rect.y + rect.height).max(0.0) as u32).min(self.height);
        for y in y0..y1 {
            for x in x0..x1 {
                let position = (y * self.width + x) as usize;
                self.pixels[position] = blend(self.pixels[position], packed, alpha);
            }
        }
    }

    fn fill(&mut self, rect: Rect, color: u32) {
        let x0 = (rect.x.max(0.0) as u32).min(self.width);
        let y0 = (rect.y.max(0.0) as u32).min(self.height);
        let x1 = ((rect.x + rect.width).max(0.0) as u32).min(self.width);
        let y1 = ((rect.y + rect.height).max(0.0) as u32).min(self.height);
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
    let framebuffer = &mut *framebuffer;
    let shift = |rect: &Rect| Rect {
        x: rect.x * scale,
        y: (rect.y - scroll_y) * scale,
        width: rect.width * scale,
        height: rect.height * scale,
    };

    for command in commands {
        match command {
            DisplayCommand::FillRect {
                rect,
                color,
                radius,
            } => {
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
            } => {
                let (x, y, font_size) = (x * scale, (y - scroll_y) * scale, font_size * scale);
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
                    ),
                };
                if *underline {
                    paint_rect(
                        framebuffer,
                        Rect {
                            x,
                            y: y + (2.0 * scale).max(1.0),
                            width: text_width,
                            height: scale.max(1.0),
                        },
                        *color,
                    );
                }
            }
        }
    }
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
) -> f32 {
    let advance = font_size * 0.5;
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
            if row_bits & (1 << source_column.min(7)) != 0 {
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
) -> f32 {
    let mut pen_x = x;
    let bold = font_weight >= 600;
    for character in text.chars() {
        let glyph = font.rasterize(character, font_size);
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
        pen_x += glyph.metrics.advance_width;
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
        if pixel_x >= framebuffer.width || pixel_y >= framebuffer.height {
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
    for pixel_y in y0..y1 {
        for pixel_x in x0..x1 {
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
    for pixel_y in y0..y1 {
        for pixel_x in x0..x1 {
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
