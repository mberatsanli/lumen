//! The Lumen rendering pipeline: style → layout → paint → SVG.
//!
//! [`build_page`] runs the full pipeline over an HTML string; the
//! intermediate results are all inspectable on the returned [`Page`].

pub mod geometry;
pub mod layout;
pub mod paint;
pub mod raster;
pub mod style;
pub mod svg;
pub mod text;

pub use geometry::{Dimensions, EdgeSizes, Edges, Rect, Size};
pub use layout::{BoxType, LayoutBox, LayoutKind, dump_layout, layout_document};
pub use paint::{DisplayCommand, build_display_list};
pub use raster::{Framebuffer, rasterize};
pub use style::{
    ComputedStyle, Dimension, Display, FontWeight, StyleMap, TextAlign, compute_styles,
};
pub use svg::render_svg;
pub use text::{HeuristicMeasurer, Line, TextMeasurer, TextMetrics, TextStyle};

use lumen_html::{Document, NodeKind};

/// A fully processed page with every pipeline stage retained.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub document: Document,
    /// The author stylesheet (embedded `<style>` contents).
    pub stylesheet: lumen_css::Stylesheet,
    pub styles: StyleMap,
    pub layout: LayoutBox,
    pub display_list: Vec<DisplayCommand>,
    pub viewport: Size,
}

/// Runs the full pipeline: parse HTML, extract embedded CSS, cascade,
/// layout, and build the display list.
///
/// Infallible: both parsers recover from malformed input the way browsers
/// do, so every input produces a page.
#[must_use]
pub fn build_page(html: &str, viewport: Size) -> Page {
    let document = lumen_html::parse_document(html);
    let stylesheet = lumen_css::parse_stylesheet(&extract_embedded_css(&document));
    let styles = compute_styles(&document, &stylesheet);
    let layout = layout_document(&document, &styles, viewport, &HeuristicMeasurer);
    let display_list = build_display_list(&layout);

    Page {
        document,
        stylesheet,
        styles,
        layout,
        display_list,
        viewport,
    }
}

/// Concatenates the contents of all `<style>` elements in document order.
#[must_use]
pub fn extract_embedded_css(document: &Document) -> String {
    let mut css = String::new();
    for id in document.descendants(document.root()) {
        if matches!(
            &document.node(id).kind,
            NodeKind::Element(element) if element.tag_name == "style"
        ) {
            css.push_str(&document.text_content(id));
            css.push('\n');
        }
    }
    css
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_css::Color;

    fn page(html: &str) -> Page {
        build_page(
            html,
            Size {
                width: 800.0,
                height: 600.0,
            },
        )
    }

    fn text_color(page: &Page) -> Option<Color> {
        page.display_list.iter().find_map(|command| match command {
            DisplayCommand::DrawText { color, .. } => Some(*color),
            _ => None,
        })
    }

    const BLUE: Color = Color::rgb(0, 0, 255);

    #[test]
    fn applies_class_rule_and_builds_layout() {
        let page = page(
            "<style>.card { width: 300px; padding: 20px; background-color: #eee; }</style>\
             <div class='card'><p>Hello</p></div>",
        );
        assert!(page.display_list.iter().any(|command| matches!(
            command,
            DisplayCommand::FillRect { color, .. } if *color == Color::rgb(0xee, 0xee, 0xee)
        )));
        assert!(dump_layout(&page.layout).contains("<div>"));
    }

    #[test]
    fn id_selector_beats_class_selector() {
        let page = page(
            "<style>.x { color: red; } #main { color: blue; }</style>\
             <p id='main' class='x'>Text</p>",
        );
        assert_eq!(text_color(&page), Some(BLUE));
    }

    #[test]
    fn class_selector_beats_tag_selector() {
        let page =
            page("<style>.x { color: blue; } p { color: red; }</style><p class='x'>Text</p>");
        assert_eq!(text_color(&page), Some(BLUE));
    }

    #[test]
    fn later_rule_wins_on_equal_specificity() {
        let page = page("<style>p { color: red; } p { color: blue; }</style><p>Text</p>");
        assert_eq!(text_color(&page), Some(BLUE));
    }

    #[test]
    fn compound_selector_requires_all_parts() {
        let page = page(
            "<style>p.note { color: blue; } p.other { color: red; }</style>\
             <p class='note'>Text</p>",
        );
        assert_eq!(text_color(&page), Some(BLUE));
    }

    #[test]
    fn descendant_selector_walks_ancestors() {
        let page = page(
            "<style>p { color: red; } .card p { color: blue; }</style>\
             <div class='card'><div><p>Deep</p></div></div>",
        );
        assert_eq!(text_color(&page), Some(BLUE));
    }

    #[test]
    fn descendant_selector_does_not_match_outside_ancestor() {
        let page = page(
            "<style>p { color: blue; } .card p { color: red; }</style>\
             <div class='other'><p>Text</p></div>",
        );
        assert_eq!(text_color(&page), Some(BLUE));
    }

    #[test]
    fn inline_style_beats_id_rule() {
        let page = page(
            "<style>#main { color: red; }</style>\
             <p id='main' style='color: blue'>Text</p>",
        );
        assert_eq!(text_color(&page), Some(BLUE));
    }

    #[test]
    fn margin_shorthand_affects_layout() {
        let page = page(
            "<style>div { margin: 10px 20px; height: 30px; }</style>\
             <body><div></div></body>",
        );
        let body = &page.layout.children[0];
        let div = &body.children[0];
        assert_eq!(div.border_box().x, 20.0);
        assert_eq!(div.border_box().y, 10.0);
        assert_eq!(div.border_box().height, 30.0);
    }

    #[test]
    fn display_none_removes_subtree() {
        let page = page(
            "<style>.hidden { display: none; background-color: red; }</style>\
             <div class='hidden'><p>Gone</p></div>",
        );
        assert!(page.display_list.is_empty());
    }

    #[test]
    fn head_content_is_hidden_by_default() {
        let page = page("<html><head><title>Doc</title></head><body><p>Vis</p></body></html>");
        let texts: Vec<&str> = page
            .display_list
            .iter()
            .filter_map(|command| match command {
                DisplayCommand::DrawText { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Vis"]);
    }

    #[test]
    fn percent_width_resolves_against_containing_block() {
        let page = page(
            "<style>body { padding: 0; margin: 0; } div { width: 50%; height: 10px; }</style>\
             <body><div></div></body>",
        );
        let body = &page.layout.children[0];
        let div = &body.children[0];
        assert_eq!(div.content_box().width, 400.0);
    }

    #[test]
    fn long_text_wraps_into_multiple_lines() {
        // 30 chars/word at 16px * 0.5 = 8px/char; container 200px fits 25 chars.
        let page = page(
            "<style>div { width: 200px; margin: 0; padding: 0; }</style>\
             <div>aaaaaaaaaa bbbbbbbbbb cccccccccc dddddddddd</div>",
        );
        let texts: Vec<&str> = page
            .display_list
            .iter()
            .filter_map(|command| match command {
                DisplayCommand::DrawText { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec!["aaaaaaaaaa bbbbbbbbbb", "cccccccccc dddddddddd"]
        );
        let div = &page.layout.children[0];
        let text_box = &div.children[0];
        // Two lines at the default 1.4 * 16px line height.
        assert_eq!(
            text_box.content_box().height,
            2.0 * text_box.style.line_height
        );
    }

    #[test]
    fn inherited_font_size_affects_wrapping() {
        // Same text: 16px wraps in 200px, 8px does not.
        let big = page(
            "<style>div { width: 200px; font-size: 16px; }</style>\
             <div>aaaaaaaaaa bbbbbbbbbb cccccccccc</div>",
        );
        let small = page(
            "<style>div { width: 200px; font-size: 8px; }</style>\
             <div>aaaaaaaaaa bbbbbbbbbb cccccccccc</div>",
        );
        let count = |page: &Page| {
            page.display_list
                .iter()
                .filter(|command| matches!(command, DisplayCommand::DrawText { .. }))
                .count()
        };
        assert_eq!(count(&big), 2);
        assert_eq!(count(&small), 1);
    }

    #[test]
    fn text_align_center_and_right_position_lines() {
        // "hi" at 16px * 0.5 = 16px wide in a 200px container.
        let centered = page(
            "<style>div { width: 200px; margin: 0; padding: 0; text-align: center; }</style>\
             <div>hi</div>",
        );
        let righted = page(
            "<style>div { width: 200px; margin: 0; padding: 0; text-align: right; }</style>\
             <div>hi</div>",
        );
        let x_of = |page: &Page| {
            page.display_list.iter().find_map(|command| match command {
                DisplayCommand::DrawText { x, .. } => Some(*x),
                _ => None,
            })
        };
        assert_eq!(x_of(&centered), Some(92.0)); // (200 - 16) / 2
        assert_eq!(x_of(&righted), Some(184.0)); // 200 - 16
    }

    #[test]
    fn whitespace_collapses_across_newlines() {
        let page = page("<p>a \n\n   b\t c</p>");
        let text = page.display_list.iter().find_map(|command| match command {
            DisplayCommand::DrawText { text, .. } => Some(text.clone()),
            _ => None,
        });
        assert_eq!(text, Some("a b c".to_string()));
    }

    #[test]
    fn text_box_height_uses_line_height() {
        let page =
            page("<style>p { line-height: 2; font-size: 20px; margin: 0; }</style><p>Text</p>");
        let p = &page.layout.children[0];
        let text = &p.children[0];
        assert_eq!(text.content_box().height, 40.0);
    }
}
