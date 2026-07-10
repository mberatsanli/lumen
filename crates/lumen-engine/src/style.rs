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
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// The subset of `display` the engine understands.
///
/// `Inline` elements currently still participate in block flow (inline
/// layout is a later milestone); `None` removes the subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Display {
    Block,
    Inline,
    /// Atomic inline: flows in line boxes, lays out like a block inside.
    InlineBlock,
    /// Flex container (single-line; see layout docs for the subset).
    Flex,
    None,
}

/// A width/height/margin/padding value before resolution against the
/// containing block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Dimension {
    Auto,
    Px(f32),
    Percent(f32),
    /// Percent of the viewport width / height.
    Vw(f32),
    Vh(f32),
}

impl Dimension {
    /// Resolves against the containing block size and the viewport;
    /// `Auto` resolves to `None`.
    #[must_use]
    pub fn resolve(&self, containing: f32, viewport: crate::geometry::Size) -> Option<f32> {
        match self {
            Self::Auto => None,
            Self::Px(value) => Some(*value),
            Self::Percent(percent) => Some(containing * percent / 100.0),
            Self::Vw(percent) => Some(viewport.width * percent / 100.0),
            Self::Vh(percent) => Some(viewport.height * percent / 100.0),
        }
    }

    /// Converts a declared value; `em` resolves against `font_size` here,
    /// so layout only ever sees px, percent or auto.
    fn from_value(value: &CssValue, font_size: f32) -> Option<Self> {
        match value {
            CssValue::Auto => Some(Self::Auto),
            CssValue::Length(pixels, lumen_css::Unit::Px) => Some(Self::Px(*pixels)),
            CssValue::Length(factor, lumen_css::Unit::Em) => Some(Self::Px(factor * font_size)),
            CssValue::Length(percent, lumen_css::Unit::Percent) => Some(Self::Percent(*percent)),
            CssValue::Length(percent, lumen_css::Unit::Vw) => Some(Self::Vw(*percent)),
            CssValue::Length(percent, lumen_css::Unit::Vh) => Some(Self::Vh(*percent)),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlexDirection {
    #[default]
    Row,
    Column,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JustifyContent {
    #[default]
    Start,
    Center,
    End,
    SpaceBetween,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlignItems {
    #[default]
    Stretch,
    Start,
    Center,
    End,
}

/// `float: left | right`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Float {
    #[default]
    None,
    Left,
    Right,
}

/// `clear: left | right | both`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Clear {
    #[default]
    None,
    Left,
    Right,
    Both,
}

/// What `width`/`height` refer to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BoxSizing {
    /// The content box (CSS initial value).
    #[default]
    ContentBox,
    /// The border box: content shrinks by padding and border.
    BorderBox,
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
    /// `text-decoration: underline`. Approximation: treated as inherited
    /// so text nodes inside links pick it up.
    pub underline: bool,
    /// `font-style: italic` (rendered as a synthetic shear).
    pub italic: bool,
    pub box_sizing: BoxSizing,
    pub float: Float,
    pub clear: Clear,
    pub flex_direction: FlexDirection,
    pub justify_content: JustifyContent,
    pub align_items: AlignItems,
    /// Resolved to pixels.
    pub gap: f32,
    pub flex_grow: f32,
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
            underline: false,
            italic: false,
            box_sizing: BoxSizing::default(),
            float: Float::None,
            clear: Clear::None,
            flex_direction: FlexDirection::default(),
            justify_content: JustifyContent::default(),
            align_items: AlignItems::default(),
            gap: 0.0,
            flex_grow: 0.0,
        }
    }
}

/// Computed styles for every node, keyed by [`NodeId`].
#[derive(Debug, Clone, PartialEq)]
pub struct StyleMap {
    pub by_node: HashMap<NodeId, ComputedStyle>,
}

/// Properties whose declared values propagate to children.
/// (`text-decoration` is not inherited in CSS — it *propagates by
/// painting*; treating it as inherited approximates that.)
const INHERITED_PROPERTIES: [&str; 7] = [
    "color",
    "font-size",
    "font-weight",
    "line-height",
    "text-align",
    "text-decoration",
    "font-style",
];

/// The built-in user-agent stylesheet (weakest cascade origin).
///
/// Display defaults are code-side (see `default_display`); this sheet only
/// carries typography and spacing defaults.
pub fn user_agent_stylesheet() -> &'static Stylesheet {
    static SHEET: OnceLock<Stylesheet> = OnceLock::new();
    SHEET.get_or_init(|| {
        let source = r"
            html, body { margin: 0; padding: 0; color: #111111; font-size: 16px; }
            h1 { font-size: 32px; font-weight: 700; margin-top: 12px; margin-bottom: 12px; }
            h2 { font-size: 24px; font-weight: 700; margin-top: 10px; margin-bottom: 10px; }
            p { font-size: 16px; margin-top: 8px; margin-bottom: 8px; }
            a { color: #0000ee; text-decoration: underline; }
            strong, b { font-weight: 700; }
            em, i { font-style: italic; }
            code { font-size: 0.875em; }
        ";
        lumen_css::parse_stylesheet(source)
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

/// Computes styles for the whole document with no hover state.
#[must_use]
pub fn compute_styles(document: &Document, author: &Stylesheet) -> StyleMap {
    compute_styles_hovered(document, author, None)
}

/// Computes styles with `hovered` under the pointer. Per CSS, `:hover`
/// matches the hovered node and all of its ancestors.
#[must_use]
pub fn compute_styles_hovered(
    document: &Document,
    author: &Stylesheet,
    hovered: Option<NodeId>,
) -> StyleMap {
    let mut hover_chain = HashSet::new();
    if let Some(node) = hovered {
        hover_chain.insert(node);
        hover_chain.extend(document.ancestors(node));
    }
    let mut by_node = HashMap::new();
    let inherited = HashMap::new();
    compute_node(
        document,
        document.root(),
        author,
        &inherited,
        &hover_chain,
        &mut by_node,
    );
    StyleMap { by_node }
}

fn compute_node(
    document: &Document,
    node_id: NodeId,
    author: &Stylesheet,
    parent_raw: &HashMap<String, CssValue>,
    hover_chain: &HashSet<NodeId>,
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
            for (name, (_, _, value)) in
                winning_declarations(document, node_id, element, sheet, hover_chain)
            {
                raw.insert(name, value);
            }
        }
        if let Some(inline) = element.attributes.get("style") {
            for declaration in lumen_css::parse_declarations(inline) {
                raw.insert(declaration.name, declaration.value);
            }
        }
    }

    let parent_font_size = parent_raw
        .get("font-size")
        .and_then(CssValue::as_px)
        .unwrap_or(DEFAULT_FONT_SIZE);
    let computed = to_computed(&raw, element, parent_font_size);
    // Children inherit the *resolved* font size, so `em` chains and
    // percentages resolve against real pixels, not unresolved declarations.
    raw.insert(
        "font-size".to_string(),
        CssValue::Length(computed.font_size, lumen_css::Unit::Px),
    );
    output.insert(node_id, computed);
    for child in document.children(node_id) {
        compute_node(document, *child, author, &raw, hover_chain, output);
    }
}

/// Per-property winner within one origin: highest (specificity, source
/// order) pair wins; later rules win ties.
fn winning_declarations(
    document: &Document,
    node_id: NodeId,
    element: &ElementData,
    sheet: &Stylesheet,
    hover_chain: &HashSet<NodeId>,
) -> HashMap<String, (Specificity, usize, CssValue)> {
    let mut winners: HashMap<String, (Specificity, usize, CssValue)> = HashMap::new();
    for rule in &sheet.rules {
        for selector in &rule.selectors {
            if selector_matches(document, node_id, element, selector, hover_chain) {
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

fn compound_matches(
    node_id: NodeId,
    element: &ElementData,
    compound: &CompoundSelector,
    hover_chain: &HashSet<NodeId>,
) -> bool {
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
    if !compound
        .classes
        .iter()
        .all(|class| element.has_class(class))
    {
        return false;
    }
    compound
        .pseudo_classes
        .iter()
        .all(|pseudo| match pseudo.as_str() {
            "hover" => hover_chain.contains(&node_id),
            // `link`/`visited` are always-true (no visited state).
            _ => true,
        })
}

/// Matches a complex selector: the subject compound must match the element
/// itself, remaining compounds must match ancestors in order (descendant
/// combinator, right to left).
pub(crate) fn selector_matches(
    document: &Document,
    node_id: NodeId,
    element: &ElementData,
    selector: &Selector,
    hover_chain: &HashSet<NodeId>,
) -> bool {
    if !compound_matches(node_id, element, selector.subject(), hover_chain) {
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
            && compound_matches(ancestor, ancestor_element, needed, hover_chain)
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
/// `parent_font_size` anchors relative font sizes (`em`, `%`).
fn to_computed(
    raw: &HashMap<String, CssValue>,
    element: Option<&ElementData>,
    parent_font_size: f32,
) -> ComputedStyle {
    let mut style = ComputedStyle::default();

    style.font_size = match raw.get("font-size") {
        Some(CssValue::Length(pixels, lumen_css::Unit::Px)) => *pixels,
        Some(CssValue::Length(factor, lumen_css::Unit::Em)) => factor * parent_font_size,
        Some(CssValue::Length(percent, lumen_css::Unit::Percent)) => {
            percent / 100.0 * parent_font_size
        }
        _ => parent_font_size,
    };

    style.line_height = match raw.get("line-height") {
        Some(CssValue::Number(factor)) => factor * style.font_size,
        Some(CssValue::Length(factor, lumen_css::Unit::Em)) => factor * style.font_size,
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
            "inline-block" => Some(Display::InlineBlock),
            "flex" => Some(Display::Flex),
            "none" => Some(Display::None),
            _ => None,
        })
        .unwrap_or_else(|| {
            element.map_or(Display::Inline, |element| {
                default_display(&element.tag_name)
            })
        });

    style.width = dimension(raw, "width", style.font_size);
    style.height = dimension(raw, "height", style.font_size);
    style.margin = edge_dimensions(raw, "margin", Dimension::Px(0.0), style.font_size);
    style.padding = edge_dimensions(raw, "padding", Dimension::Px(0.0), style.font_size);

    style.border_width = EdgeSizes {
        top: edge_px(raw, "border-top-width", style.font_size),
        right: edge_px(raw, "border-right-width", style.font_size),
        bottom: edge_px(raw, "border-bottom-width", style.font_size),
        left: edge_px(raw, "border-left-width", style.font_size),
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

    style.underline = matches!(
        raw.get("text-decoration").and_then(CssValue::as_keyword),
        Some("underline")
    );

    style.italic = matches!(
        raw.get("font-style").and_then(CssValue::as_keyword),
        Some("italic" | "oblique")
    );

    style.box_sizing = match raw.get("box-sizing").and_then(CssValue::as_keyword) {
        Some("border-box") => BoxSizing::BorderBox,
        _ => BoxSizing::ContentBox,
    };

    style.float = match raw.get("float").and_then(CssValue::as_keyword) {
        Some("left") => Float::Left,
        Some("right") => Float::Right,
        _ => Float::None,
    };

    style.clear = match raw.get("clear").and_then(CssValue::as_keyword) {
        Some("left") => Clear::Left,
        Some("right") => Clear::Right,
        Some("both") => Clear::Both,
        _ => Clear::None,
    };

    style.flex_direction = match raw.get("flex-direction").and_then(CssValue::as_keyword) {
        Some("column") => FlexDirection::Column,
        _ => FlexDirection::Row,
    };

    style.justify_content = match raw.get("justify-content").and_then(CssValue::as_keyword) {
        Some("center") => JustifyContent::Center,
        Some("flex-end" | "end") => JustifyContent::End,
        Some("space-between") => JustifyContent::SpaceBetween,
        _ => JustifyContent::Start,
    };

    style.align_items = match raw.get("align-items").and_then(CssValue::as_keyword) {
        Some("flex-start" | "start") => AlignItems::Start,
        Some("center") => AlignItems::Center,
        Some("flex-end" | "end") => AlignItems::End,
        _ => AlignItems::Stretch,
    };

    style.gap = edge_px(raw, "gap", style.font_size);

    style.flex_grow = match raw.get("flex-grow") {
        Some(CssValue::Number(value)) => value.max(0.0),
        _ => 0.0,
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

fn dimension(raw: &HashMap<String, CssValue>, name: &str, font_size: f32) -> Dimension {
    raw.get(name)
        .and_then(|value| Dimension::from_value(value, font_size))
        .unwrap_or(Dimension::Auto)
}

fn edge_dimensions(
    raw: &HashMap<String, CssValue>,
    prefix: &str,
    default: Dimension,
    font_size: f32,
) -> EdgeSizes<Dimension> {
    let side = |name: &str| {
        raw.get(&format!("{prefix}-{name}"))
            .and_then(|value| Dimension::from_value(value, font_size))
            .unwrap_or(default)
    };
    EdgeSizes {
        top: side("top"),
        right: side("right"),
        bottom: side("bottom"),
        left: side("left"),
    }
}

fn edge_px(raw: &HashMap<String, CssValue>, name: &str, font_size: f32) -> f32 {
    match raw.get(name) {
        Some(CssValue::Length(factor, lumen_css::Unit::Em)) => factor * font_size,
        Some(value) => value.as_px().unwrap_or(0.0),
        None => 0.0,
    }
}

/// One line per element with its key computed values — for debugging and
/// CLI inspection.
#[must_use]
pub fn dump_styles(document: &Document, styles: &StyleMap) -> String {
    use std::fmt::Write as _;
    let mut output = String::new();
    for id in document.descendants(document.root()) {
        let Some(element) = document.element(id) else {
            continue;
        };
        let Some(style) = styles.by_node.get(&id) else {
            continue;
        };
        let mut selector = element.tag_name.clone();
        if let Some(element_id) = element.id() {
            let _ = write!(selector, "#{element_id}");
        }
        for class in element.classes() {
            let _ = write!(selector, ".{class}");
        }
        let background = style
            .background_color
            .map_or("transparent".to_string(), |color| color.to_string());
        let _ = writeln!(
            output,
            "{selector}: display={:?} color={} background={background} font-size={} \
             font-weight={} line-height={} width={:?} height={:?}",
            style.display,
            style.color,
            style.font_size,
            style.font_weight.0,
            style.line_height,
            style.width,
            style.height,
        );
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_html::parse_document;

    fn styles_for(html: &str) -> (Document, StyleMap) {
        let document = parse_document(html);
        let author = lumen_css::parse_stylesheet(&crate::extract_embedded_css(&document));
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
    fn em_font_size_resolves_against_parent_chain() {
        let (document, styles) = styles_for(
            "<style>div { font-size: 20px; } section { font-size: 1.5em; } p { font-size: 150%; }\
             </style><div><section><p>t</p></section></div>",
        );
        assert_eq!(style_of(&document, &styles, "section").font_size, 30.0);
        // 150% of the section's resolved 30px.
        assert_eq!(style_of(&document, &styles, "p").font_size, 45.0);
    }

    #[test]
    fn auto_margin_survives_to_computed_style() {
        let (document, styles) = styles_for("<style>div { margin: 0 auto; }</style><div>t</div>");
        let div = style_of(&document, &styles, "div");
        assert_eq!(div.margin.left, Dimension::Auto);
        assert_eq!(div.margin.top, Dimension::Px(0.0));
    }

    #[test]
    fn percent_width_is_kept_as_percent() {
        let (document, styles) = styles_for("<style>div { width: 50%; }</style><div>t</div>");
        assert_eq!(
            style_of(&document, &styles, "div").width,
            Dimension::Percent(50.0)
        );
        assert_eq!(
            Dimension::Percent(50.0).resolve(
                300.0,
                crate::geometry::Size {
                    width: 0.0,
                    height: 0.0
                }
            ),
            Some(150.0)
        );
    }
}
