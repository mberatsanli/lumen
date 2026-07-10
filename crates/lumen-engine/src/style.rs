//! Style system: selector matching, cascade, inheritance and typed
//! computed styles.
//!
//! Cascade origins, weakest to strongest: user-agent defaults, author
//! stylesheet, inline `style=` attributes. Within one origin, conflicts are
//! resolved by (specificity, source order).
//!
//! Inheritance happens on raw declared values, so a `line-height: 1.5`
//! number re-resolves against each element's own font size, as in CSS.
//! Afterwards the raw values are converted once into a fully typed
//! [`ComputedStyle`]; layout and paint never parse strings.

use crate::geometry::EdgeSizes;
use lumen_css::{Color, CompoundSelector, CssValue, Selector, Specificity, Stylesheet};
use lumen_html::{Document, ElementData, NodeId, NodeKind};
use std::collections::HashMap;
use std::sync::OnceLock;

/// The subset of `display` the engine understands.
///
/// `Inline` elements currently still participate in block flow (inline
/// layout is a later milestone); `None` removes the subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Display {
    Block,
    Inline,
    None,
}

/// A width/height/margin/padding value before resolution against the
/// containing block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Dimension {
    Auto,
    Px(f32),
    Percent(f32),
}

impl Dimension {
    /// Resolves against the containing block size; `Auto` resolves to `None`.
    #[must_use]
    pub fn resolve(&self, containing: f32) -> Option<f32> {
        match self {
            Self::Auto => None,
            Self::Px(value) => Some(*value),
            Self::Percent(percent) => Some(containing * percent / 100.0),
        }
    }

    fn from_value(value: &CssValue) -> Option<Self> {
        match value {
            CssValue::Auto => Some(Self::Auto),
            CssValue::Length(pixels, lumen_css::Unit::Px) => Some(Self::Px(*pixels)),
            CssValue::Length(percent, lumen_css::Unit::Percent) => Some(Self::Percent(*percent)),
            _ => None,
        }
    }
}

/// Numeric font weight (400 = normal, 700 = bold).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FontWeight(pub u16);

impl Default for FontWeight {
    fn default() -> Self {
        Self(400)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// Fully resolved style for one node. All fields are typed; nothing needs
/// re-parsing during layout or paint.
#[derive(Debug, Clone, PartialEq)]
pub struct ComputedStyle {
    pub display: Display,
    pub color: Color,
    pub background_color: Option<Color>,
    pub width: Dimension,
    pub height: Dimension,
    pub margin: EdgeSizes<Dimension>,
    pub padding: EdgeSizes<Dimension>,
    pub border_width: EdgeSizes<f32>,
    pub border_color: Color,
    pub font_size: f32,
    pub font_weight: FontWeight,
    /// Resolved to pixels.
    pub line_height: f32,
    pub text_align: TextAlign,
}

pub const DEFAULT_FONT_SIZE: f32 = 16.0;
/// Used when no `line-height` is declared or inherited.
pub const DEFAULT_LINE_HEIGHT_FACTOR: f32 = 1.4;
const DEFAULT_COLOR: Color = Color::rgb(0x11, 0x11, 0x11);

impl Default for ComputedStyle {
    fn default() -> Self {
        Self {
            display: Display::Inline,
            color: DEFAULT_COLOR,
            background_color: None,
            width: Dimension::Auto,
            height: Dimension::Auto,
            margin: EdgeSizes::uniform(Dimension::Px(0.0)),
            padding: EdgeSizes::uniform(Dimension::Px(0.0)),
            border_width: EdgeSizes::uniform(0.0),
            border_color: DEFAULT_COLOR,
            font_size: DEFAULT_FONT_SIZE,
            font_weight: FontWeight::default(),
            line_height: DEFAULT_FONT_SIZE * DEFAULT_LINE_HEIGHT_FACTOR,
            text_align: TextAlign::Left,
        }
    }
}

/// Computed styles for every node, keyed by [`NodeId`].
#[derive(Debug, Clone, PartialEq)]
pub struct StyleMap {
    pub by_node: HashMap<NodeId, ComputedStyle>,
}

/// Properties whose declared values propagate to children.
const INHERITED_PROPERTIES: [&str; 5] = [
    "color",
    "font-size",
    "font-weight",
    "line-height",
    "text-align",
];

/// The built-in user-agent stylesheet (weakest cascade origin).
///
/// Display defaults are code-side (see `default_display`); this sheet only
/// carries typography and spacing defaults.
pub fn user_agent_stylesheet() -> &'static Stylesheet {
    static SHEET: OnceLock<Stylesheet> = OnceLock::new();
    SHEET.get_or_init(|| {
        let source = r"
            html, body { margin: 0; padding: 0; color: #111111; font-size: 16px;
                         background-color: white; }
            h1 { font-size: 32px; font-weight: 700; margin-top: 12px; margin-bottom: 12px; }
            h2 { font-size: 24px; font-weight: 700; margin-top: 10px; margin-bottom: 10px; }
            p { font-size: 16px; margin-top: 8px; margin-bottom: 8px; }
        ";
        // Invariant: the UA sheet is a compile-time constant kept valid by
        // the `ua_stylesheet_parses` test below.
        lumen_css::parse_stylesheet(source).expect("user-agent stylesheet is valid")
    })
}

/// Default `display` per tag, used when no declaration says otherwise.
/// Unknown tags default to inline, as in HTML.
fn default_display(tag: &str) -> Display {
    match tag {
        "html" | "body" | "div" | "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "ul" | "ol"
        | "li" | "section" | "article" | "header" | "footer" | "main" | "nav" | "aside"
        | "blockquote" | "pre" | "form" | "table" | "hr" => Display::Block,
        "head" | "style" | "script" | "title" | "meta" | "link" | "base" => Display::None,
        _ => Display::Inline,
    }
}

/// Computes styles for the whole document.
#[must_use]
pub fn compute_styles(document: &Document, author: &Stylesheet) -> StyleMap {
    let mut by_node = HashMap::new();
    let inherited = HashMap::new();
    compute_node(document, document.root(), author, &inherited, &mut by_node);
    StyleMap { by_node }
}

fn compute_node(
    document: &Document,
    node_id: NodeId,
    author: &Stylesheet,
    parent_raw: &HashMap<String, CssValue>,
    output: &mut HashMap<NodeId, ComputedStyle>,
) {
    let mut raw: HashMap<String, CssValue> = HashMap::new();
    for property in INHERITED_PROPERTIES {
        if let Some(value) = parent_raw.get(property) {
            raw.insert(property.to_string(), value.clone());
        }
    }

    let element = match &document.node(node_id).kind {
        NodeKind::Element(element) => Some(element),
        _ => None,
    };

    if let Some(element) = element {
        // Weakest origin first; each stronger origin overwrites per property.
        for sheet in [user_agent_stylesheet(), author] {
            for (name, (_, _, value)) in winning_declarations(document, node_id, element, sheet) {
                raw.insert(name, value);
            }
        }
        if let Some(inline) = element.attributes.get("style") {
            for declaration in lumen_css::parse_declarations(inline) {
                raw.insert(declaration.name, declaration.value);
            }
        }
    }

    output.insert(node_id, to_computed(&raw, element));
    for child in document.children(node_id) {
        compute_node(document, *child, author, &raw, output);
    }
}

/// Per-property winner within one origin: highest (specificity, source
/// order) pair wins; later rules win ties.
fn winning_declarations(
    document: &Document,
    node_id: NodeId,
    element: &ElementData,
    sheet: &Stylesheet,
) -> HashMap<String, (Specificity, usize, CssValue)> {
    let mut winners: HashMap<String, (Specificity, usize, CssValue)> = HashMap::new();
    for rule in &sheet.rules {
        for selector in &rule.selectors {
            if selector_matches(document, node_id, element, selector) {
                for declaration in &rule.declarations {
                    let candidate = (
                        selector.specificity(),
                        rule.source_order,
                        declaration.value.clone(),
                    );
                    let replace = winners
                        .get(&declaration.name)
                        .is_none_or(|current| (candidate.0, candidate.1) >= (current.0, current.1));
                    if replace {
                        winners.insert(declaration.name.clone(), candidate);
                    }
                }
            }
        }
    }
    winners
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
/// itself, remaining compounds must match ancestors in order (descendant
/// combinator, right to left).
pub(crate) fn selector_matches(
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

/// Converts raw declared values into a typed [`ComputedStyle`].
fn to_computed(raw: &HashMap<String, CssValue>, element: Option<&ElementData>) -> ComputedStyle {
    let mut style = ComputedStyle::default();

    style.font_size = raw
        .get("font-size")
        .and_then(CssValue::as_px)
        .unwrap_or(DEFAULT_FONT_SIZE);

    style.line_height = match raw.get("line-height") {
        Some(CssValue::Number(factor)) => factor * style.font_size,
        Some(value) => value
            .as_px()
            .unwrap_or(style.font_size * DEFAULT_LINE_HEIGHT_FACTOR),
        None => style.font_size * DEFAULT_LINE_HEIGHT_FACTOR,
    };

    if let Some(color) = raw.get("color").and_then(CssValue::as_color) {
        style.color = color;
    }

    style.background_color = match raw.get("background-color") {
        Some(CssValue::Color(color)) => Some(*color),
        _ => None, // includes `transparent` and absence
    };

    style.display = raw
        .get("display")
        .and_then(CssValue::as_keyword)
        .and_then(|keyword| match keyword {
            "block" => Some(Display::Block),
            "inline" => Some(Display::Inline),
            "none" => Some(Display::None),
            _ => None,
        })
        .unwrap_or_else(|| {
            element.map_or(Display::Inline, |element| {
                default_display(&element.tag_name)
            })
        });

    style.width = dimension(raw, "width");
    style.height = dimension(raw, "height");
    style.margin = edge_dimensions(raw, "margin", Dimension::Px(0.0));
    style.padding = edge_dimensions(raw, "padding", Dimension::Px(0.0));

    style.border_width = EdgeSizes {
        top: edge_px(raw, "border-top-width"),
        right: edge_px(raw, "border-right-width"),
        bottom: edge_px(raw, "border-bottom-width"),
        left: edge_px(raw, "border-left-width"),
    };
    // Initial border color is the element's own color (like `currentColor`).
    style.border_color = raw
        .get("border-color")
        .and_then(CssValue::as_color)
        .unwrap_or(style.color);

    style.font_weight = match raw.get("font-weight") {
        Some(CssValue::Number(weight)) => FontWeight((*weight as u16).clamp(1, 1000)),
        Some(CssValue::Keyword(keyword)) if keyword == "bold" => FontWeight(700),
        Some(CssValue::Keyword(keyword)) if keyword == "normal" => FontWeight(400),
        _ => FontWeight::default(),
    };

    style.text_align = raw
        .get("text-align")
        .and_then(CssValue::as_keyword)
        .and_then(|keyword| match keyword {
            "left" => Some(TextAlign::Left),
            "center" => Some(TextAlign::Center),
            "right" => Some(TextAlign::Right),
            _ => None,
        })
        .unwrap_or_default();

    style
}

fn dimension(raw: &HashMap<String, CssValue>, name: &str) -> Dimension {
    raw.get(name)
        .and_then(Dimension::from_value)
        .unwrap_or(Dimension::Auto)
}

fn edge_dimensions(
    raw: &HashMap<String, CssValue>,
    prefix: &str,
    default: Dimension,
) -> EdgeSizes<Dimension> {
    let side = |name: &str| {
        raw.get(&format!("{prefix}-{name}"))
            .and_then(Dimension::from_value)
            .unwrap_or(default)
    };
    EdgeSizes {
        top: side("top"),
        right: side("right"),
        bottom: side("bottom"),
        left: side("left"),
    }
}

fn edge_px(raw: &HashMap<String, CssValue>, name: &str) -> f32 {
    raw.get(name).and_then(CssValue::as_px).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_html::parse_document;

    fn styles_for(html: &str) -> (Document, StyleMap) {
        let document = parse_document(html);
        let author = lumen_css::parse_stylesheet(&crate::extract_embedded_css(&document)).unwrap();
        let styles = compute_styles(&document, &author);
        (document, styles)
    }

    fn style_of<'a>(document: &Document, styles: &'a StyleMap, tag: &str) -> &'a ComputedStyle {
        let id = document
            .descendants(document.root())
            .find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == tag)
            })
            .unwrap_or_else(|| panic!("no <{tag}> in document"));
        &styles.by_node[&id]
    }

    #[test]
    fn ua_stylesheet_parses() {
        assert!(!user_agent_stylesheet().rules.is_empty());
    }

    #[test]
    fn ua_defaults_apply() {
        let (document, styles) = styles_for("<body><h1>T</h1></body>");
        let h1 = style_of(&document, &styles, "h1");
        assert_eq!(h1.font_size, 32.0);
        assert_eq!(h1.font_weight, FontWeight(700));
        assert_eq!(h1.margin.top, Dimension::Px(12.0));
    }

    #[test]
    fn author_rule_beats_ua_rule_regardless_of_specificity() {
        // UA `h1 {font-size: 32px}` has type specificity; the author's
        // universal selector still wins because origin outranks specificity.
        let (document, styles) =
            styles_for("<style>* { font-size: 20px; }</style><body><h1>T</h1></body>");
        assert_eq!(style_of(&document, &styles, "h1").font_size, 20.0);
    }

    #[test]
    fn color_inherits_into_nested_elements() {
        let (document, styles) = styles_for(
            "<style>.card { color: #ff0000; }</style>\
             <div class='card'><div><p>deep</p></div></div>",
        );
        assert_eq!(
            style_of(&document, &styles, "p").color,
            Color::rgb(255, 0, 0)
        );
    }

    #[test]
    fn text_align_inherits() {
        let (document, styles) =
            styles_for("<style>div { text-align: center; }</style><div><p>t</p></div>");
        assert_eq!(
            style_of(&document, &styles, "p").text_align,
            TextAlign::Center
        );
    }

    #[test]
    fn numeric_line_height_resolves_against_own_font_size() {
        let (document, styles) = styles_for(
            "<style>div { line-height: 1.5; font-size: 20px; } p { font-size: 10px; }</style>\
             <div><p>t</p></div>",
        );
        assert_eq!(style_of(&document, &styles, "div").line_height, 30.0);
        // The number 1.5 inherits and re-resolves against 10px, as in CSS.
        assert_eq!(style_of(&document, &styles, "p").line_height, 15.0);
    }

    #[test]
    fn dimensions_are_not_inherited() {
        let (document, styles) =
            styles_for("<style>div { width: 100px; padding: 4px; }</style><div><p>t</p></div>");
        let p = style_of(&document, &styles, "p");
        assert_eq!(p.width, Dimension::Auto);
        assert_eq!(p.padding.top, Dimension::Px(0.0));
    }

    #[test]
    fn display_defaults_by_tag() {
        let (document, styles) =
            styles_for("<body><span>x</span><div>y</div><head></head><custom>z</custom></body>");
        assert_eq!(
            style_of(&document, &styles, "span").display,
            Display::Inline
        );
        assert_eq!(style_of(&document, &styles, "div").display, Display::Block);
        assert_eq!(style_of(&document, &styles, "head").display, Display::None);
        assert_eq!(
            style_of(&document, &styles, "custom").display,
            Display::Inline
        );
    }

    #[test]
    fn transparent_background_is_none() {
        let (document, styles) =
            styles_for("<style>div { background-color: transparent; }</style><div>t</div>");
        assert_eq!(style_of(&document, &styles, "div").background_color, None);
    }

    #[test]
    fn border_color_defaults_to_current_color() {
        let (document, styles) =
            styles_for("<style>div { color: #ff0000; border-width: 2px; }</style><div>t</div>");
        let div = style_of(&document, &styles, "div");
        assert_eq!(div.border_width.top, 2.0);
        assert_eq!(div.border_color, Color::rgb(255, 0, 0));
    }

    #[test]
    fn bold_keyword_maps_to_700() {
        let (document, styles) = styles_for("<style>p { font-weight: bold; }</style><p>t</p>");
        assert_eq!(
            style_of(&document, &styles, "p").font_weight,
            FontWeight(700)
        );
    }

    #[test]
    fn percent_width_is_kept_as_percent() {
        let (document, styles) = styles_for("<style>div { width: 50%; }</style><div>t</div>");
        assert_eq!(
            style_of(&document, &styles, "div").width,
            Dimension::Percent(50.0)
        );
        assert_eq!(Dimension::Percent(50.0).resolve(300.0), Some(150.0));
    }
}
