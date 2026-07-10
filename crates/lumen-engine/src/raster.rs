//! Software rasterizer: display list → ARGB pixel buffer.
//!
//! The second display-list backend after SVG, meant for the desktop shell.
//! Text uses the built-in 8×8 bitmap font (`font8x8`) scaled to the font
//! size — deliberately crude but fully self-contained and deterministic.
//! Glyph cells are half an em wide to match [`crate::HeuristicMeasurer`],
//! so painted text agrees with layout's line breaking.

use crate::font::SystemFont;
use crate::geometry::Rect;
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
    let shift = |rect: &Rect| Rect {
        x: rect.x * scale,
        y: (rect.y - scroll_y) * scale,
        width: rect.width * scale,
        height: rect.height * scale,
    };

    for command in commands {
        match command {
            DisplayCommand::FillRect { rect, color } => {
                framebuffer.fill(shift(rect), pack(*color));
            }
            DisplayCommand::StrokeRect {
                rect,
                widths,
                color,
            } => {
                let rect = shift(rect);
                let color = pack(*color);
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
                    Rect {
                        height: width_of(widths.top),
                        ..rect
                    },
                    Rect {
                        x: rect.x + rect.width - width_of(widths.right),
                        width: width_of(widths.right),
                        ..rect
                    },
                    Rect {
                        y: rect.y + rect.height - width_of(widths.bottom),
                        height: width_of(widths.bottom),
                        ..rect
                    },
                    Rect {
                        width: width_of(widths.left),
                        ..rect
                    },
                ];
                for strip in strips {
                    framebuffer.fill(strip, color);
                }
            }
            DisplayCommand::DrawText {
                x,
                y,
                text,
                color,
                font_size,
                font_weight,
            } => match font {
                Some(font) => draw_text_scalable(
                    &mut framebuffer,
                    font,
                    x * scale,
                    (y - scroll_y) * scale,
                    text,
                    pack(*color),
                    font_size * scale,
                    *font_weight,
                ),
                None => draw_text(
                    &mut framebuffer,
                    x * scale,
                    (y - scroll_y) * scale,
                    text,
                    pack(*color),
                    font_size * scale,
                    *font_weight,
                ),
            },
        }
    }
    framebuffer
}

/// Draws a text run with the 8×8 bitmap font. `y` is the baseline; the
/// glyph cell is `0.5 * font_size` wide (matching the heuristic measurer)
/// and `0.8 * font_size` tall above the baseline. Weights ≥ 600 are
/// emboldened by a 1px double-strike.
fn draw_text(
    framebuffer: &mut Framebuffer,
    x: f32,
    y: f32,
    text: &str,
    color: u32,
    font_size: f32,
    font_weight: u16,
) {
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
        let cell_x = x + index as f32 * advance;
        draw_glyph(
            framebuffer,
            &glyph,
            cell_x,
            top,
            advance,
            cell_height,
            color,
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
            );
        }
    }
}

/// Nearest-neighbor scales one 8×8 glyph into a cell.
fn draw_glyph(
    framebuffer: &mut Framebuffer,
    glyph: &[u8; 8],
    cell_x: f32,
    cell_y: f32,
    cell_width: f32,
    cell_height: f32,
    color: u32,
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
                framebuffer.pixels[(pixel_y * framebuffer.width + pixel_x) as usize] = color;
            }
        }
    }
}

/// Draws a text run with a scalable font; `y` is the baseline. Glyph
/// coverage is alpha-blended onto the framebuffer. Weights ≥ 600 get a 1px
/// double-strike (single-face fonts have no real bold).
#[allow(clippy::too_many_arguments)]
fn draw_text_scalable(
    framebuffer: &mut Framebuffer,
    font: &SystemFont,
    x: f32,
    y: f32,
    text: &str,
    color: u32,
    font_size: f32,
    font_weight: u16,
) {
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
        );
        if bold {
            blend_glyph(
                framebuffer,
                &glyph.coverage,
                glyph.metrics.width,
                glyph_x + 1.0,
                glyph_y,
                color,
            );
        }
        pen_x += glyph.metrics.advance_width;
    }
}

fn blend_glyph(
    framebuffer: &mut Framebuffer,
    coverage: &[u8],
    glyph_width: usize,
    origin_x: f32,
    origin_y: f32,
    color: u32,
) {
    if glyph_width == 0 {
        return;
    }
    for (index, alpha) in coverage.iter().enumerate() {
        if *alpha == 0 {
            continue;
        }
        let pixel_x = origin_x + (index % glyph_width) as f32;
        let pixel_y = origin_y + (index / glyph_width) as f32;
        if pixel_x < 0.0 || pixel_y < 0.0 {
            continue;
        }
        let (pixel_x, pixel_y) = (pixel_x as u32, pixel_y as u32);
        if pixel_x >= framebuffer.width || pixel_y >= framebuffer.height {
            continue;
        }
        let position = (pixel_y * framebuffer.width + pixel_x) as usize;
        framebuffer.pixels[position] = blend(framebuffer.pixels[position], color, *alpha);
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
            color: RED,
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
        }];
        let framebuffer = rasterize_with(&commands, 20, 20, 0.0, 2.0, None);
        assert_eq!(framebuffer.pixel(4, 2), 0x00ff_0000);
        assert_eq!(framebuffer.pixel(9, 5), 0x00ff_0000); // exclusive at 10,6
        assert_eq!(framebuffer.pixel(10, 2), 0x00ff_ffff);
        assert_eq!(framebuffer.pixel(3, 2), 0x00ff_ffff);
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
        }];
        let framebuffer = rasterize(&commands, 4, 4, 0.0);
        assert!(framebuffer.pixels.iter().all(|pixel| *pixel == 0x00ff_0000));
    }
}
