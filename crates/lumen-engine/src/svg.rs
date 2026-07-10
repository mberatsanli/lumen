//! Deterministic SVG serialization of the display list.

use crate::Page;
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
            DisplayCommand::FillRect { rect, color } => {
                let _ = writeln!(
                    svg,
                    "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"{color}\"/>",
                    rect.x, rect.y, rect.width, rect.height,
                );
            }
            DisplayCommand::StrokeRect {
                rect,
                widths,
                color,
            } => {
                // Four edge strips drawn inward from the border box, so the
                // output stays plain rectangles (no stroke alignment issues).
                let edges = [
                    (rect.x, rect.y, rect.width, widths.top),
                    (
                        rect.x + rect.width - widths.right,
                        rect.y,
                        widths.right,
                        rect.height,
                    ),
                    (
                        rect.x,
                        rect.y + rect.height - widths.bottom,
                        rect.width,
                        widths.bottom,
                    ),
                    (rect.x, rect.y, widths.left, rect.height),
                ];
                for (x, y, width, height) in edges {
                    if width > 0.0 && height > 0.0 {
                        let _ = writeln!(
                            svg,
                            "<rect x=\"{x}\" y=\"{y}\" width=\"{width}\" height=\"{height}\" fill=\"{color}\"/>",
                        );
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
}
