//! Deterministic SVG serialization of the display list.

use crate::Page;
use crate::geometry::{Corners, Rect};
use crate::paint::DisplayCommand;
use std::fmt::Write as _;

/// Renders a page's display list to an SVG document string.
#[must_use]
pub fn render_svg(page: &Page) -> String {
    let height = page.layout.content_box().height.max(page.viewport.height);
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\">\n",
        page.viewport.width, height, page.viewport.width, height
    );
    svg.push_str("<rect width=\"100%\" height=\"100%\" fill=\"white\"/>\n");

    for command in &page.display_list {
        match command {
            DisplayCommand::FillRect {
                rect,
                color,
                radius,
            } => {
                if radius.is_zero() {
                    let _ = writeln!(
                        svg,
                        "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"{color}\"/>",
                        rect.x, rect.y, rect.width, rect.height,
                    );
                } else {
                    let _ = writeln!(
                        svg,
                        "<path d=\"{}\" fill=\"{color}\"/>",
                        rounded_rect_path(rect, radius)
                    );
                }
            }
            DisplayCommand::DrawImage { rect, image, alpha } => {
                let opacity = if *alpha == 255 {
                    String::new()
                } else {
                    format!(" opacity=\"{:.3}\"", f32::from(*alpha) / 255.0)
                };
                let _ = writeln!(
                    svg,
                    "<image x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" preserveAspectRatio=\"none\"{opacity} href=\"data:{};base64,{}\"/>",
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                    image.mime,
                    base64(&image.encoded)
                );
            }
            DisplayCommand::StrokeRect {
                rect,
                widths,
                colors,
                styles,
                radius,
            } => {
                if !radius.is_zero() {
                    // Rounded frame: a ring path (outer minus inner) in the
                    // top edge color at the top edge width.
                    let width = widths.top.max(widths.left);
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
                    let _ = writeln!(
                        svg,
                        "<path d=\"{} {}\" fill=\"{}\" fill-rule=\"evenodd\"/>",
                        rounded_rect_path(rect, radius),
                        rounded_rect_path(&inner, &inner_radius),
                        colors.top
                    );
                    continue;
                }
                // Four edge strips drawn inward from the border box, so the
                // output stays plain rectangles (no stroke alignment issues).
                let edges = [
                    (
                        rect.x, rect.y, rect.width, widths.top, colors.top, styles.top, true,
                    ),
                    (
                        rect.x + rect.width - widths.right,
                        rect.y,
                        widths.right,
                        rect.height,
                        colors.right,
                        styles.right,
                        false,
                    ),
                    (
                        rect.x,
                        rect.y + rect.height - widths.bottom,
                        rect.width,
                        widths.bottom,
                        colors.bottom,
                        styles.bottom,
                        true,
                    ),
                    (
                        rect.x,
                        rect.y,
                        widths.left,
                        rect.height,
                        colors.left,
                        styles.left,
                        false,
                    ),
                ];
                for (x, y, width, height, color, style, horizontal) in edges {
                    if width <= 0.0 || height <= 0.0 {
                        continue;
                    }
                    let thickness = if horizontal { height } else { width };
                    match dash_pattern(style, thickness) {
                        None => {
                            let _ = writeln!(
                                svg,
                                "<rect x=\"{x}\" y=\"{y}\" width=\"{width}\" height=\"{height}\" fill=\"{color}\"/>",
                            );
                        }
                        Some((dash, gap)) => {
                            // A centered line with a dash pattern.
                            let (x1, y1, x2, y2) = if horizontal {
                                (x, y + thickness / 2.0, x + width, y + thickness / 2.0)
                            } else {
                                (x + thickness / 2.0, y, x + thickness / 2.0, y + height)
                            };
                            let _ = writeln!(
                                svg,
                                "<line x1=\"{x1}\" y1=\"{y1}\" x2=\"{x2}\" y2=\"{y2}\" stroke=\"{color}\" stroke-width=\"{thickness}\" stroke-dasharray=\"{dash} {gap}\"/>",
                            );
                        }
                    }
                }
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
                let decoration = if *underline {
                    " text-decoration=\"underline\""
                } else {
                    ""
                };
                let slant = if *italic {
                    " font-style=\"italic\""
                } else {
                    ""
                };
                let _ = writeln!(
                    svg,
                    "<text x=\"{x}\" y=\"{y}\" fill=\"{color}\" font-family=\"system-ui, sans-serif\" font-size=\"{font_size}\" font-weight=\"{font_weight}\"{decoration}{slant}>{}</text>",
                    escape_xml(text)
                );
            }
        }
    }
    svg.push_str("</svg>\n");
    svg
}

/// Dash/gap lengths for a border style, `None` for solid.
fn dash_pattern(style: crate::style::BorderStyle, thickness: f32) -> Option<(f32, f32)> {
    match style {
        crate::style::BorderStyle::Dashed => Some((3.0 * thickness, 2.0 * thickness)),
        crate::style::BorderStyle::Dotted => Some((thickness, thickness)),
        _ => None,
    }
}

/// A rounded-rectangle path (clockwise, arcs at each corner).
fn rounded_rect_path(rect: &Rect, radius: &Corners<f32>) -> String {
    let radius = radius.clamped_to(rect.width, rect.height);
    let (x, y, w, h) = (rect.x, rect.y, rect.width, rect.height);
    let (tl, tr, br, bl) = (
        radius.top_left,
        radius.top_right,
        radius.bottom_right,
        radius.bottom_left,
    );
    format!(
        "M {} {} H {} A {tr} {tr} 0 0 1 {} {} V {} A {br} {br} 0 0 1 {} {} H {} A {bl} {bl} 0 0 1 {} {} V {} A {tl} {tl} 0 0 1 {} {} Z",
        x + tl,
        y,
        x + w - tr,
        x + w,
        y + tr,
        y + h - br,
        x + w - br,
        y + h,
        x + bl,
        x,
        y + h - bl,
        y + tl,
        x + tl,
        y,
    )
}

/// Minimal base64 (RFC 4648, with padding) — small enough that a
/// dependency is not worth it.
fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let bits = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        let encoded = [
            TABLE[(bits >> 18) as usize & 63],
            TABLE[(bits >> 12) as usize & 63],
            TABLE[(bits >> 6) as usize & 63],
            TABLE[bits as usize & 63],
        ];
        let keep = chunk.len() + 1;
        for (index, byte) in encoded.iter().enumerate() {
            output.push(if index < keep { *byte as char } else { '=' });
        }
    }
    output
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_xml_special_characters() {
        assert_eq!(escape_xml("a < b & \"c\""), "a &lt; b &amp; &quot;c&quot;");
    }

    #[test]
    fn rounded_rects_emit_paths() {
        let page = crate::build_page(
            "<style>div { background-color: #eee; border-radius: 8px; height: 40px; }</style>\
             <div></div>",
            crate::Size {
                width: 100.0,
                height: 100.0,
            },
        );
        let svg = render_svg(&page);
        assert!(
            svg.contains("<path d=\"M "),
            "expected a rounded path: {svg}"
        );
        assert!(svg.contains("A 8 8 0 0 1"));
    }

    #[test]
    fn dashed_borders_emit_dasharray_lines() {
        let page = crate::build_page(
            "<style>div { border: 4px dashed #112233; height: 20px; }</style><div></div>",
            crate::Size {
                width: 200.0,
                height: 100.0,
            },
        );
        let svg = render_svg(&page);
        assert!(
            svg.contains("stroke-dasharray=\"12 8\""),
            "expected dashed lines: {svg}"
        );
        assert!(svg.contains("stroke=\"#112233\""));
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
