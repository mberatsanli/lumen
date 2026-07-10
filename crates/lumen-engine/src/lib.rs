use lumen_css::{CompoundSelector, CssValue, Selector, Stylesheet};
use lumen_html::{Document, ElementData, NodeId, NodeKind};
use std::collections::HashMap;
use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Edges {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ComputedStyle {
    pub properties: HashMap<String, CssValue>,
}

impl ComputedStyle {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&CssValue> {
        self.properties.get(name)
    }

    #[must_use]
    pub fn px(&self, name: &str) -> Option<f32> {
        self.get(name)?.as_px()
    }

    #[must_use]
    pub fn keyword(&self, name: &str) -> Option<&str> {
        self.get(name)?.as_keyword()
    }

    /// CSS text of the value, e.g. `#ff0000` or `bold` — for paint output.
    #[must_use]
    pub fn css_text(&self, name: &str) -> Option<String> {
        self.get(name).map(ToString::to_string)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StyleMap {
    pub by_node: HashMap<NodeId, ComputedStyle>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LayoutKind {
    Element(String),
    Text(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct LayoutBox {
    pub node_id: NodeId,
    pub kind: LayoutKind,
    pub rect: Rect,
    pub margin: Edges,
    pub padding: Edges,
    pub children: Vec<LayoutBox>,
    pub style: ComputedStyle,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DisplayCommand {
    FillRect {
        rect: Rect,
        color: String,
    },
    DrawText {
        x: f32,
        y: f32,
        text: String,
        color: String,
        font_size: f32,
        font_weight: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub document: Document,
    pub stylesheet: Stylesheet,
    pub styles: StyleMap,
    pub layout: LayoutBox,
    pub display_list: Vec<DisplayCommand>,
    pub viewport: Size,
}

#[derive(Debug)]
pub enum EngineError {
    Css(lumen_css::CssError),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Css(error) => write!(formatter, "CSS error: {error}"),
        }
    }
}

impl std::error::Error for EngineError {}

impl From<lumen_css::CssError> for EngineError {
    fn from(value: lumen_css::CssError) -> Self {
        Self::Css(value)
    }
}

pub fn build_page(html: &str, viewport: Size) -> Result<Page, EngineError> {
    let document = lumen_html::parse_document(html);
    let embedded_css = extract_embedded_css(&document);
    let stylesheet =
        lumen_css::parse_stylesheet(&format!("{}\n{}", user_agent_stylesheet(), embedded_css))?;
    let styles = compute_styles(&document, &stylesheet);
    let layout = layout_document(&document, &styles, viewport);
    let display_list = build_display_list(&layout);

    Ok(Page {
        document,
        stylesheet,
        styles,
        layout,
        display_list,
        viewport,
    })
}

#[must_use]
pub fn extract_embedded_css(document: &Document) -> String {
    let mut css = String::new();
    for (id, node) in document.nodes().iter().enumerate() {
        if matches!(
            &node.kind,
            NodeKind::Element(element) if element.tag_name == "style"
        ) {
            css.push_str(&document.text_content(id));
            css.push('\n');
        }
    }
    css
}

#[must_use]
pub fn compute_styles(document: &Document, stylesheet: &Stylesheet) -> StyleMap {
    let mut by_node = HashMap::new();
    compute_node_styles(document, document.root(), stylesheet, None, &mut by_node);
    StyleMap { by_node }
}

fn compute_node_styles(
    document: &Document,
    node_id: NodeId,
    stylesheet: &Stylesheet,
    parent_style: Option<&ComputedStyle>,
    output: &mut HashMap<NodeId, ComputedStyle>,
) {
    let mut properties = HashMap::new();
    if let Some(parent) = parent_style {
        for inherited in ["color", "font-size", "font-weight"] {
            if let Some(value) = parent.get(inherited) {
                properties.insert(inherited.to_string(), value.clone());
            }
        }
    }

    if let NodeKind::Element(element) = &document.node(node_id).kind {
        let mut winners: HashMap<String, (lumen_css::Specificity, usize, CssValue)> =
            HashMap::new();
        for rule in &stylesheet.rules {
            for selector in &rule.selectors {
                if selector_matches(document, node_id, element, selector) {
                    for declaration in &rule.declarations {
                        let candidate = (
                            selector.specificity(),
                            rule.source_order,
                            declaration.value.clone(),
                        );
                        let should_replace = winners.get(&declaration.name).is_none_or(|current| {
                            (candidate.0, candidate.1) >= (current.0, current.1)
                        });
                        if should_replace {
                            winners.insert(declaration.name.clone(), candidate);
                        }
                    }
                }
            }
        }
        for (name, (_, _, value)) in winners {
            properties.insert(name, value);
        }

        // Inline `style=` declarations beat every stylesheet rule.
        if let Some(inline) = element.attributes.get("style") {
            for declaration in lumen_css::parse_declarations(inline) {
                properties.insert(declaration.name, declaration.value);
            }
        }
    }

    let computed = ComputedStyle { properties };
    output.insert(node_id, computed.clone());
    for child in &document.node(node_id).children {
        compute_node_styles(document, *child, stylesheet, Some(&computed), output);
    }
}

fn compound_matches(element: &ElementData, compound: &CompoundSelector) -> bool {
    if let Some(tag) = &compound.tag
        && element.tag_name != *tag
    {
        return false;
    }
    if let Some(id) = &compound.id
        && element.id() != Some(id.as_str())
    {
        return false;
    }
    compound
        .classes
        .iter()
        .all(|class| element.has_class(class))
}

/// Matches a complex selector: the subject compound must match the element
/// itself, and remaining compounds must match ancestors in order (descendant
/// combinator, right to left).
fn selector_matches(
    document: &Document,
    node_id: NodeId,
    element: &ElementData,
    selector: &Selector,
) -> bool {
    if !compound_matches(element, selector.subject()) {
        return false;
    }
    let mut remaining = selector.compounds[..selector.compounds.len() - 1]
        .iter()
        .rev();
    let Some(mut needed) = remaining.next() else {
        return true;
    };
    for ancestor in document.ancestors(node_id) {
        if let Some(ancestor_element) = document.element(ancestor)
            && compound_matches(ancestor_element, needed)
        {
            match remaining.next() {
                Some(next) => needed = next,
                None => return true,
            }
        }
    }
    false
}

#[must_use]
pub fn layout_document(document: &Document, styles: &StyleMap, viewport: Size) -> LayoutBox {
    let root_style = styles
        .by_node
        .get(&document.root())
        .cloned()
        .unwrap_or_default();
    let mut root = LayoutBox {
        node_id: document.root(),
        kind: LayoutKind::Element("#document".to_string()),
        rect: Rect {
            x: 0.0,
            y: 0.0,
            width: viewport.width,
            height: viewport.height,
        },
        margin: Edges::default(),
        padding: Edges::default(),
        children: Vec::new(),
        style: root_style,
    };

    let mut cursor_y = 0.0;
    for child in &document.node(document.root()).children {
        if let Some(layout) =
            layout_node(document, styles, *child, 0.0, &mut cursor_y, viewport.width)
        {
            root.children.push(layout);
        }
    }
    root.rect.height = cursor_y.max(viewport.height);
    root
}

fn layout_node(
    document: &Document,
    styles: &StyleMap,
    node_id: NodeId,
    containing_x: f32,
    cursor_y: &mut f32,
    containing_width: f32,
) -> Option<LayoutBox> {
    let style = styles.by_node.get(&node_id)?.clone();
    if style.keyword("display") == Some("none") {
        return None;
    }

    match &document.node(node_id).kind {
        NodeKind::Document => None,
        NodeKind::Text(text) => {
            let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if normalized.is_empty() {
                return None;
            }
            let font_size = style.px("font-size").unwrap_or(16.0);
            let height = font_size * 1.4;
            let rect = Rect {
                x: containing_x,
                y: *cursor_y,
                width: containing_width,
                height,
            };
            *cursor_y += height;
            Some(LayoutBox {
                node_id,
                kind: LayoutKind::Text(normalized),
                rect,
                margin: Edges::default(),
                padding: Edges::default(),
                children: Vec::new(),
                style,
            })
        }
        NodeKind::Element(element) => {
            if matches!(
                element.tag_name.as_str(),
                "head" | "style" | "title" | "meta" | "link"
            ) {
                return None;
            }

            let margin = parse_edges(&style, "margin");
            let padding = parse_edges(&style, "padding");
            *cursor_y += margin.top;
            let x = containing_x + margin.left;
            let available_width = (containing_width - margin.left - margin.right).max(0.0);
            let width = style
                .px("width")
                .unwrap_or((available_width - padding.left - padding.right).max(0.0));
            let outer_width = width + padding.left + padding.right;
            let y = *cursor_y;
            let mut child_cursor_y = y + padding.top;
            let child_x = x + padding.left;
            let mut children = Vec::new();

            for child in &document.node(node_id).children {
                if let Some(layout) = layout_node(
                    document,
                    styles,
                    *child,
                    child_x,
                    &mut child_cursor_y,
                    width,
                ) {
                    children.push(layout);
                }
            }

            let content_height = style
                .px("height")
                .unwrap_or((child_cursor_y - (y + padding.top)).max(default_min_height(element)));
            let height = padding.top + content_height + padding.bottom;
            *cursor_y = y + height + margin.bottom;

            Some(LayoutBox {
                node_id,
                kind: LayoutKind::Element(element.tag_name.clone()),
                rect: Rect {
                    x,
                    y,
                    width: outer_width,
                    height,
                },
                margin,
                padding,
                children,
                style,
            })
        }
    }
}

fn default_min_height(element: &ElementData) -> f32 {
    match element.tag_name.as_str() {
        "body" | "html" | "div" => 0.0,
        _ => 8.0,
    }
}

fn parse_edges(style: &ComputedStyle, prefix: &str) -> Edges {
    // Shorthands are already expanded to longhands by the CSS parser.
    Edges {
        top: style.px(&format!("{prefix}-top")).unwrap_or(0.0),
        right: style.px(&format!("{prefix}-right")).unwrap_or(0.0),
        bottom: style.px(&format!("{prefix}-bottom")).unwrap_or(0.0),
        left: style.px(&format!("{prefix}-left")).unwrap_or(0.0),
    }
}

#[must_use]
pub fn build_display_list(layout: &LayoutBox) -> Vec<DisplayCommand> {
    let mut commands = Vec::new();
    paint_box(layout, &mut commands);
    commands
}

fn paint_box(layout: &LayoutBox, commands: &mut Vec<DisplayCommand>) {
    if let Some(background) = layout.style.get("background-color")
        && background.as_keyword() != Some("transparent")
    {
        commands.push(DisplayCommand::FillRect {
            rect: layout.rect,
            color: background.to_string(),
        });
    }

    if let LayoutKind::Text(text) = &layout.kind {
        commands.push(DisplayCommand::DrawText {
            x: layout.rect.x,
            y: layout.rect.y + layout.style.px("font-size").unwrap_or(16.0),
            text: text.clone(),
            color: layout
                .style
                .css_text("color")
                .unwrap_or_else(|| "#111111".to_string()),
            font_size: layout.style.px("font-size").unwrap_or(16.0),
            font_weight: layout
                .style
                .css_text("font-weight")
                .unwrap_or_else(|| "400".to_string()),
        });
    }

    for child in &layout.children {
        paint_box(child, commands);
    }
}

#[must_use]
pub fn render_svg(page: &Page) -> String {
    let height = page.layout.rect.height.max(page.viewport.height);
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
                    "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"{}\"/>",
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                    escape_xml(color)
                );
            }
            DisplayCommand::DrawText {
                x,
                y,
                text,
                color,
                font_size,
                font_weight,
            } => {
                let _ = writeln!(
                    svg,
                    "<text x=\"{x}\" y=\"{y}\" fill=\"{}\" font-family=\"system-ui, sans-serif\" font-size=\"{font_size}\" font-weight=\"{}\">{}</text>",
                    escape_xml(color),
                    escape_xml(font_weight),
                    escape_xml(text)
                );
            }
        }
    }
    svg.push_str("</svg>\n");
    svg
}

#[must_use]
pub fn dump_layout(layout: &LayoutBox) -> String {
    let mut output = String::new();
    dump_layout_box(layout, 0, &mut output);
    output
}

fn dump_layout_box(layout: &LayoutBox, depth: usize, output: &mut String) {
    let indent = "  ".repeat(depth);
    let label = match &layout.kind {
        LayoutKind::Element(tag) => format!("<{tag}>"),
        LayoutKind::Text(text) => format!("\"{text}\""),
    };
    let _ = writeln!(
        output,
        "{indent}{label} x={} y={} w={} h={}",
        layout.rect.x, layout.rect.y, layout.rect.width, layout.rect.height
    );
    for child in &layout.children {
        dump_layout_box(child, depth + 1, output);
    }
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn user_agent_stylesheet() -> &'static str {
    r#"
html, body { display: block; margin: 0px; padding: 0px; color: #111111; font-size: 16px; background-color: white; }
div, p, h1, h2, h3, ul, li { display: block; }
h1 { font-size: 32px; font-weight: 700; margin-top: 12px; margin-bottom: 12px; }
h2 { font-size: 24px; font-weight: 700; margin-top: 10px; margin-bottom: 10px; }
p { font-size: 16px; margin-top: 8px; margin-bottom: 8px; }
"#
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(html: &str) -> Page {
        build_page(
            html,
            Size {
                width: 800.0,
                height: 600.0,
            },
        )
        .unwrap()
    }

    fn text_color(page: &Page) -> Option<String> {
        page.display_list.iter().find_map(|command| match command {
            DisplayCommand::DrawText { color, .. } => Some(color.clone()),
            _ => None,
        })
    }

    #[test]
    fn applies_class_rule_and_builds_layout() {
        let page = page(
            "<style>.card { width: 300px; padding: 20px; background-color: #eee; }</style>\
             <div class='card'><p>Hello</p></div>",
        );
        assert!(page.display_list.iter().any(|command| matches!(
            command,
            DisplayCommand::FillRect { color, .. } if color == "#eeeeee"
        )));
        assert!(dump_layout(&page.layout).contains("<div>"));
    }

    #[test]
    fn id_selector_beats_class_selector() {
        let page = page(
            "<style>.x { color: red; } #main { color: blue; }</style>\
             <p id='main' class='x'>Text</p>",
        );
        assert_eq!(text_color(&page), Some("#0000ff".to_string()));
    }

    #[test]
    fn class_selector_beats_tag_selector() {
        let page =
            page("<style>.x { color: blue; } p { color: red; }</style><p class='x'>Text</p>");
        assert_eq!(text_color(&page), Some("#0000ff".to_string()));
    }

    #[test]
    fn later_rule_wins_on_equal_specificity() {
        let page = page("<style>p { color: red; } p { color: blue; }</style><p>Text</p>");
        assert_eq!(text_color(&page), Some("#0000ff".to_string()));
    }

    #[test]
    fn compound_selector_requires_all_parts() {
        let page = page(
            "<style>p.note { color: blue; } p.other { color: red; }</style>\
             <p class='note'>Text</p>",
        );
        assert_eq!(text_color(&page), Some("#0000ff".to_string()));
    }

    #[test]
    fn descendant_selector_walks_ancestors() {
        let page = page(
            "<style>p { color: red; } .card p { color: blue; }</style>\
             <div class='card'><div><p>Deep</p></div></div>",
        );
        assert_eq!(text_color(&page), Some("#0000ff".to_string()));
    }

    #[test]
    fn descendant_selector_does_not_match_outside_ancestor() {
        let page = page(
            "<style>p { color: blue; } .card p { color: red; }</style>\
             <div class='other'><p>Text</p></div>",
        );
        assert_eq!(text_color(&page), Some("#0000ff".to_string()));
    }

    #[test]
    fn inline_style_beats_id_rule() {
        let page = page(
            "<style>#main { color: red; }</style>\
             <p id='main' style='color: blue'>Text</p>",
        );
        assert_eq!(text_color(&page), Some("#0000ff".to_string()));
    }

    #[test]
    fn margin_shorthand_affects_layout() {
        let page = page(
            "<style>body { margin: 0; padding: 0; } div { margin: 10px 20px; height: 30px; }</style>\
             <body><div></div></body>",
        );
        let body = &page.layout.children[0];
        let div = &body.children[0];
        assert_eq!(div.rect.x, 20.0);
        assert_eq!(div.rect.y, 10.0);
    }

    #[test]
    fn display_none_removes_subtree() {
        let page = page(
            "<style>.hidden { display: none; background-color: red; }</style>\
             <div class='hidden'><p>Gone</p></div>",
        );
        assert!(page.display_list.is_empty());
    }
}
