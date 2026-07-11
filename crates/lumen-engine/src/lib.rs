//! The Lumen rendering pipeline: style → layout → paint → SVG.
//!
//! [`build_page`] runs the full pipeline over an HTML string; the
//! intermediate results are all inspectable on the returned [`Page`].

mod flex;
mod float;
pub mod font;
pub mod geometry;
pub mod image;
pub mod inline;
pub mod layout;
pub mod paint;
pub mod raster;
pub mod selection;
pub mod style;
pub mod svg;
pub mod text;

pub use font::SystemFont;
pub use geometry::{Corners, Dimensions, EdgeSizes, Edges, Rect, Size};
pub use image::{ImageMap, RasterImage, collect_image_sources};
pub use inline::{Fragment, LineBox};
pub use layout::{BoxType, LayoutBox, LayoutKind, dump_layout, layout_document};
pub use paint::{DisplayCommand, build_display_list};
pub use raster::{Framebuffer, rasterize, rasterize_over, rasterize_with};
pub use selection::{
    Caret, HighlightRegion, Selection, TextRun, caret_at_point, collect_text_runs, highlight_rects,
    selected_text,
};
pub use style::{
    BackgroundImage, ComputedStyle, Dimension, Display, FontWeight, LinearGradient, StyleMap,
    TextAlign, compute_styles, compute_styles_hovered,
};
pub use svg::render_svg;
pub use text::{HeuristicMeasurer, TextMeasurer, TextMetrics, TextStyle};

use lumen_html::{Document, NodeKind};

/// A fully processed page with every pipeline stage retained.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub document: Document,
    /// The author stylesheet (embedded `<style>` contents). Shared so
    /// relayouts (hover, resize) never copy the parsed rules.
    pub stylesheet: std::sync::Arc<lumen_css::Stylesheet>,
    pub styles: StyleMap,
    pub layout: LayoutBox,
    pub display_list: Vec<DisplayCommand>,
    pub viewport: Size,
    /// Decoded images per `<img>` node. Shared like the stylesheet.
    pub images: std::sync::Arc<ImageMap>,
}

/// Runs the full pipeline: parse HTML, extract embedded CSS, cascade,
/// layout, and build the display list.
///
/// Infallible: both parsers recover from malformed input the way browsers
/// do, so every input produces a page.
#[must_use]
pub fn build_page(html: &str, viewport: Size) -> Page {
    build_page_with_measurer(html, viewport, &HeuristicMeasurer)
}

/// [`build_page`] with an explicit text measurer (e.g. a real font via
/// [`SystemFont`]) so layout wraps text with true glyph widths.
#[must_use]
pub fn build_page_with_measurer(html: &str, viewport: Size, measurer: &dyn TextMeasurer) -> Page {
    build_page_full(html, viewport, measurer, None)
}

/// The full-control pipeline entry: explicit measurer plus interaction
/// state (`hovered` enables `:hover` rules for that node and its
/// ancestors). Node ids are stable across rebuilds of the same source.
#[must_use]
pub fn build_page_full(
    html: &str,
    viewport: Size,
    measurer: &dyn TextMeasurer,
    hovered: Option<lumen_html::NodeId>,
) -> Page {
    let document = lumen_html::parse_document(html);
    let stylesheet = lumen_css::parse_stylesheet(&collect_author_css(&document, |_| None));
    page_from_document(
        document,
        std::sync::Arc::new(stylesheet),
        std::sync::Arc::new(ImageMap::new()),
        viewport,
        measurer,
        hovered,
    )
}

/// Builds a page from an already-parsed document and author stylesheet.
/// This is what navigation code uses so external stylesheets are fetched
/// once and reused across hover/viewport relayouts.
#[must_use]
pub fn page_from_document(
    document: Document,
    stylesheet: std::sync::Arc<lumen_css::Stylesheet>,
    images: std::sync::Arc<ImageMap>,
    viewport: Size,
    measurer: &dyn TextMeasurer,
    hovered: Option<lumen_html::NodeId>,
) -> Page {
    let mut document = document;
    // Media queries resolve against the viewport width here, so resizes
    // (which rebuild the page) restyle automatically.
    let effective = stylesheet.for_width(viewport.width);
    let mut styles = compute_styles_hovered(&document, &effective, hovered);
    apply_generated_content(&mut document, &mut styles);
    let layout = layout_document(&document, &styles, viewport, measurer, &images);
    let display_list = build_display_list(&layout, &images);

    Page {
        document,
        stylesheet,
        styles,
        layout,
        display_list,
        viewport,
        images,
    }
}

/// Materializes `::before`/`::after` content: each pseudo text becomes a
/// generated text node (inserted or updated in place) whose computed
/// style is registered in the style map.
fn apply_generated_content(document: &mut Document, styles: &mut StyleMap) {
    for pseudo in std::mem::take(&mut styles.pseudo_texts) {
        let node = document.upsert_generated_text(pseudo.element, pseudo.leading, &pseudo.text);
        styles.by_node.insert(node, pseudo.style);
    }
}

/// Collects author CSS in document order: `<style>` contents inline, and
/// `<link rel="stylesheet" href>` contents through `load_external` (which
/// returns `None` on failure — the sheet is then skipped, page intact).
/// The engine stays network-free; navigation code supplies the closure.
pub fn collect_author_css(
    document: &Document,
    mut load_external: impl FnMut(&str) -> Option<String>,
) -> String {
    let mut css = String::new();
    for id in document.descendants(document.root()) {
        let Some(element) = document.element(id) else {
            continue;
        };
        match element.tag_name.as_str() {
            "style" => {
                css.push_str(&document.text_content(id));
                css.push('\n');
            }
            "link" => {
                let is_stylesheet = element.attributes.get("rel").is_some_and(|rel| {
                    rel.split_whitespace()
                        .any(|word| word.eq_ignore_ascii_case("stylesheet"))
                });
                if is_stylesheet
                    && let Some(href) = element.attributes.get("href")
                    && let Some(external) = load_external(href)
                {
                    css.push_str(&external);
                    css.push('\n');
                }
            }
            _ => {}
        }
    }
    css
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
    #[test]
    fn before_and_after_generate_inline_text() {
        let page = crate::build_page(
            "<style>.badge::before { content: \"[pre] \"; color: #ff0000; }\
                    .badge::after { content: \" [post]\"; }</style>\
             <p class='badge'>middle</p>",
            crate::Size {
                width: 800.0,
                height: 600.0,
            },
        );
        let texts: Vec<(String, String)> = page
            .display_list
            .iter()
            .filter_map(|command| match command {
                crate::DisplayCommand::DrawText { text, color, .. } => {
                    Some((text.clone(), color.to_string()))
                }
                _ => None,
            })
            .collect();
        let joined: String = texts
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<Vec<_>>()
            .join("|");
        assert!(joined.contains("[pre]"), "display list text: {joined}");
        assert!(joined.contains("[post]"), "display list text: {joined}");
        // ::before comes first and carries its own color.
        assert!(texts[0].0.contains("[pre]"));
        assert_eq!(texts[0].1, "#ff0000");
    }

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
    fn ua_defaults_for_strong_and_em() {
        let page = page("<p>x <strong>bold</strong> <em>slant</em></p>");
        let find = |needle: &str| {
            page.display_list.iter().find_map(|command| match command {
                DisplayCommand::DrawText {
                    text,
                    font_weight,
                    italic,
                    ..
                } if text == needle => Some((*font_weight, *italic)),
                _ => None,
            })
        };
        assert_eq!(find("bold"), Some((700, false)));
        assert_eq!(find("slant"), Some((400, true)));
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
