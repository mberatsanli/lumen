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

use crate::geometry::{Corners, EdgeSizes};
use lumen_css::{
    AttributeOperation, Color, Combinator, CompoundSelector, CssValue, PseudoClass, Selector,
    Specificity, Stylesheet,
};
use lumen_html::{Document, ElementData, NodeId, NodeKind};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// Declared values keyed by property name. `Cow` keys let the fixed
/// property names (inherited copies, internal inserts) avoid per-node
/// string allocations during the cascade.
type RawStyle = HashMap<Cow<'static, str>, CssValue>;

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

/// Border line style. Deviation from CSS: the initial value behaves as
/// `solid` (so `border-width` alone shows a border, as the project brief
/// expects); `none`/`hidden` suppress the border. `dashed`/`dotted` parse
/// but render solid for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BorderStyle {
    #[default]
    Solid,
    Dashed,
    Dotted,
    None,
}

/// CSS positioning scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Position {
    #[default]
    Static,
    Relative,
    Absolute,
    Fixed,
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
    /// Size constraints; `Auto` means unconstrained.
    pub min_width: Dimension,
    pub max_width: Dimension,
    pub min_height: Dimension,
    pub max_height: Dimension,
    pub margin: EdgeSizes<Dimension>,
    pub padding: EdgeSizes<Dimension>,
    pub border_width: EdgeSizes<f32>,
    pub border_color: EdgeSizes<Color>,
    pub border_style: EdgeSizes<BorderStyle>,
    /// Corner radii in pixels, clockwise from top-left.
    pub border_radius: Corners<f32>,
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
    /// `font-family` collapsed to its generic: monospace or not.
    pub monospace: bool,
    pub white_space: WhiteSpace,
    pub box_sizing: BoxSizing,
    pub float: Float,
    pub clear: Clear,
    pub overflow: Overflow,
    pub position: Position,
    /// `top`/`right`/`bottom`/`left` offsets for positioned boxes.
    pub offsets: EdgeSizes<Dimension>,
    pub z_index: Option<i32>,
    pub flex_direction: FlexDirection,
    /// `flex-wrap: wrap` (wrap-reverse is treated as wrap).
    pub flex_wrap: bool,
    pub justify_content: JustifyContent,
    pub align_items: AlignItems,
    /// Per-item `align-items` override; `None` is `auto`.
    pub align_self: Option<AlignItems>,
    /// Resolved to pixels.
    pub gap: f32,
    pub flex_grow: f32,
    pub flex_shrink: f32,
    /// Element opacity 0..=1, multiplied into every paint command of the
    /// subtree (an approximation of real group compositing).
    pub opacity: f32,
    /// `user-select: none` makes the element's text unselectable.
    pub selectable: bool,
    /// `::selection` overrides: highlight background and (recorded, not
    /// yet painted) text color.
    pub selection_background: Option<Color>,
    pub selection_color: Option<Color>,
}

/// `overflow` subset: anything that is not `visible` clips children to
/// the padding box at paint time (no inner scrolling).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overflow {
    #[default]
    Visible,
    Clip,
}

/// `white-space` subset: `pre` preserves spaces and newlines and never
/// wraps (pre-wrap/pre-line are approximated as pre).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WhiteSpace {
    #[default]
    Normal,
    Pre,
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
            min_width: Dimension::Auto,
            max_width: Dimension::Auto,
            min_height: Dimension::Auto,
            max_height: Dimension::Auto,
            margin: EdgeSizes::uniform(Dimension::Px(0.0)),
            padding: EdgeSizes::uniform(Dimension::Px(0.0)),
            border_width: EdgeSizes::uniform(0.0),
            border_color: EdgeSizes::uniform(DEFAULT_COLOR),
            border_style: EdgeSizes::uniform(BorderStyle::Solid),
            border_radius: Corners::uniform(0.0),
            font_size: DEFAULT_FONT_SIZE,
            font_weight: FontWeight::default(),
            line_height: DEFAULT_FONT_SIZE * DEFAULT_LINE_HEIGHT_FACTOR,
            text_align: TextAlign::Left,
            underline: false,
            italic: false,
            monospace: false,
            white_space: WhiteSpace::Normal,
            box_sizing: BoxSizing::default(),
            float: Float::None,
            clear: Clear::None,
            overflow: Overflow::Visible,
            position: Position::Static,
            offsets: EdgeSizes::uniform(Dimension::Auto),
            z_index: None,
            flex_direction: FlexDirection::default(),
            flex_wrap: false,
            justify_content: JustifyContent::default(),
            align_items: AlignItems::default(),
            align_self: None,
            gap: 0.0,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            opacity: 1.0,
            selectable: true,
            selection_background: None,
            selection_color: None,
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
/// (`user-select` and `::selection` styling are treated as inherited —
/// an approximation that matches how they behave in practice.)
const INHERITED_PROPERTIES: [&str; 12] = [
    "color",
    "font-family",
    "white-space",
    "font-size",
    "font-weight",
    "line-height",
    "text-align",
    "text-decoration",
    "font-style",
    "user-select",
    "::selection-background",
    "::selection-color",
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
            h3 { font-size: 19px; font-weight: 700; margin-top: 9px; margin-bottom: 9px; }
            h4 { font-size: 16px; font-weight: 700; margin-top: 11px; margin-bottom: 11px; }
            h5 { font-size: 13px; font-weight: 700; margin-top: 11px; margin-bottom: 11px; }
            h6 { font-size: 11px; font-weight: 700; margin-top: 12px; margin-bottom: 12px; }
            p { font-size: 16px; margin-top: 8px; margin-bottom: 8px; }
            hr { border-top: 1px solid #808080; margin-top: 8px; margin-bottom: 8px; }
            a { color: #0000ee; text-decoration: underline; }
            strong, b { font-weight: 700; }
            em, i { font-style: italic; }
            pre { white-space: pre; font-family: monospace; margin-top: 8px; margin-bottom: 8px; }
            code, kbd, samp, tt { font-family: monospace; font-size: 0.875em; }
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
        DEFAULT_FONT_SIZE,
        &hover_chain,
        &mut by_node,
    );
    StyleMap { by_node }
}

fn compute_node(
    document: &Document,
    node_id: NodeId,
    author: &Stylesheet,
    parent_raw: &RawStyle,
    root_font_size: f32,
    hover_chain: &HashSet<NodeId>,
    output: &mut HashMap<NodeId, ComputedStyle>,
) {
    let mut raw = RawStyle::new();
    for property in INHERITED_PROPERTIES {
        if let Some(value) = parent_raw.get(property) {
            raw.insert(Cow::Borrowed(property), value.clone());
        }
    }

    let element = match &document.node(node_id).kind {
        NodeKind::Element(element) => Some(element),
        _ => None,
    };

    if let Some(element) = element {
        // Weakest origin first; each stronger origin overwrites per
        // property — unless a weaker origin declared it `!important`
        // (author important beats inline normal).
        let mut important: HashSet<String> = HashSet::new();
        for sheet in [user_agent_stylesheet(), author] {
            for (name, (is_important, _, _, value)) in
                winning_declarations(document, node_id, element, sheet, hover_chain)
            {
                if is_important || !important.contains(&name) {
                    if is_important {
                        important.insert(name.clone());
                    }
                    raw.insert(Cow::Owned(name), value);
                }
            }
        }
        if let Some(inline) = element.attributes.get("style") {
            for declaration in lumen_css::parse_declarations(inline) {
                if declaration.important || !important.contains(&declaration.name) {
                    raw.insert(Cow::Owned(declaration.name), declaration.value);
                }
            }
        }
    }

    // CSS-wide keywords: `inherit` pulls the parent's value (works for
    // non-inherited properties too), `initial`/`revert` reset to the
    // default, `unset` picks by inheritedness.
    let keyword_names: Vec<String> = raw
        .iter()
        .filter(|(_, value)| {
            matches!(value, CssValue::Keyword(keyword)
                if matches!(keyword.as_str(), "inherit" | "initial" | "unset" | "revert"))
        })
        .map(|(name, _)| name.clone().into_owned())
        .collect();
    for name in keyword_names {
        let CssValue::Keyword(keyword) = raw[name.as_str()].clone() else {
            continue;
        };
        let inherits = match keyword.as_str() {
            "inherit" => true,
            "unset" => INHERITED_PROPERTIES.contains(&name.as_str()),
            _ => false, // initial | revert
        };
        match parent_raw.get(name.as_str()).filter(|_| inherits) {
            Some(value) => raw.insert(Cow::Owned(name), value.clone()),
            None => raw.remove(name.as_str()),
        };
    }

    // `rem` resolves against the root font size here, so the rest of the
    // pipeline only ever sees px/em/percent.
    for value in raw.values_mut() {
        if let CssValue::Length(size, lumen_css::Unit::Rem) = value {
            *value = CssValue::Length(*size * root_font_size, lumen_css::Unit::Px);
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
        Cow::Borrowed("font-size"),
        CssValue::Length(computed.font_size, lumen_css::Unit::Px),
    );
    // The html element's resolved font size anchors `rem` for the tree.
    let root_font_size = match element {
        Some(element) if element.tag_name == "html" => computed.font_size,
        _ => root_font_size,
    };
    output.insert(node_id, computed);
    for child in document.children(node_id) {
        compute_node(
            document,
            *child,
            author,
            &raw,
            root_font_size,
            hover_chain,
            output,
        );
    }
}

/// Per-property winner within one origin: highest (importance,
/// specificity, source order) triple wins; later rules win ties.
fn winning_declarations(
    document: &Document,
    node_id: NodeId,
    element: &ElementData,
    sheet: &Stylesheet,
    hover_chain: &HashSet<NodeId>,
) -> HashMap<String, (bool, Specificity, usize, CssValue)> {
    let mut winners: HashMap<String, (bool, Specificity, usize, CssValue)> = HashMap::new();
    for rule in &sheet.rules {
        for selector in &rule.selectors {
            if selector_matches(document, node_id, element, selector, hover_chain) {
                // `::selection` rules style the highlight, not the element:
                // only their background-color/color apply, under internal
                // property names.
                let pseudo_element = selector.subject().pseudo_element.as_deref();
                for declaration in &rule.declarations {
                    let name = match pseudo_element {
                        None => declaration.name.clone(),
                        Some("selection") => match declaration.name.as_str() {
                            "background-color" => "::selection-background".to_string(),
                            "color" => "::selection-color".to_string(),
                            _ => continue,
                        },
                        Some(_) => continue,
                    };
                    let candidate = (
                        declaration.important,
                        selector.specificity(),
                        rule.source_order,
                        declaration.value.clone(),
                    );
                    let replace = winners.get(&name).is_none_or(|current| {
                        (candidate.0, candidate.1, candidate.2) >= (current.0, current.1, current.2)
                    });
                    if replace {
                        winners.insert(name, candidate);
                    }
                }
            }
        }
    }
    winners
}

/// The element siblings of a node (children of its parent that are
/// elements), plus the node's position among them.
fn element_siblings(document: &Document, node_id: NodeId) -> (Vec<NodeId>, usize) {
    let siblings: Vec<NodeId> = document.parent(node_id).map_or_else(Vec::new, |parent| {
        document
            .children(parent)
            .iter()
            .copied()
            .filter(|child| document.element(*child).is_some())
            .collect()
    });
    let position = siblings
        .iter()
        .position(|sibling| *sibling == node_id)
        .unwrap_or(0);
    (siblings, position)
}

/// Whether a 1-based index satisfies the `an+b` micro-syntax.
fn nth_matches(a: i32, b: i32, index: i32) -> bool {
    if a == 0 {
        return index == b;
    }
    let distance = index - b;
    distance % a == 0 && distance / a >= 0
}

fn compound_matches(
    document: &Document,
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
    if !compound.attributes.iter().all(|attribute| {
        let Some(value) = element.attributes.get(&attribute.name) else {
            return false;
        };
        match &attribute.operation {
            AttributeOperation::Exists => true,
            AttributeOperation::Equals(expected) => value == expected,
            AttributeOperation::StartsWith(prefix) => value.starts_with(prefix.as_str()),
            AttributeOperation::EndsWith(suffix) => value.ends_with(suffix.as_str()),
            AttributeOperation::Contains(needle) => value.contains(needle.as_str()),
        }
    }) {
        return false;
    }
    compound.pseudo_classes.iter().all(|pseudo| match pseudo {
        PseudoClass::Hover => hover_chain.contains(&node_id),
        // No visited state: both always match.
        PseudoClass::Link | PseudoClass::Visited => true,
        PseudoClass::FirstChild => element_siblings(document, node_id).1 == 0,
        PseudoClass::LastChild => {
            let (siblings, position) = element_siblings(document, node_id);
            position + 1 == siblings.len()
        }
        PseudoClass::OnlyChild => element_siblings(document, node_id).0.len() == 1,
        PseudoClass::NthChild(a, b) => {
            let (_, position) = element_siblings(document, node_id);
            nth_matches(*a, *b, position as i32 + 1)
        }
        PseudoClass::NthLastChild(a, b) => {
            let (siblings, position) = element_siblings(document, node_id);
            nth_matches(*a, *b, (siblings.len() - position) as i32)
        }
        PseudoClass::Not(inner) => {
            !compound_matches(document, node_id, element, inner, hover_chain)
        }
    })
}

/// Matches a complex selector right to left: the subject compound must
/// match the element itself, then each combinator walks to a parent,
/// sibling or (with backtracking) ancestor/earlier sibling.
pub(crate) fn selector_matches(
    document: &Document,
    node_id: NodeId,
    element: &ElementData,
    selector: &Selector,
    hover_chain: &HashSet<NodeId>,
) -> bool {
    if !compound_matches(document, node_id, element, selector.subject(), hover_chain) {
        return false;
    }
    complex_matches_from(
        document,
        selector,
        selector.compounds.len() - 1,
        node_id,
        hover_chain,
    )
}

/// Whether the selector prefix ending at `index` (which already matched
/// `node_id`) can be completed toward the left.
fn complex_matches_from(
    document: &Document,
    selector: &Selector,
    index: usize,
    node_id: NodeId,
    hover_chain: &HashSet<NodeId>,
) -> bool {
    if index == 0 {
        return true;
    }
    let needed = &selector.compounds[index - 1];
    let step = |candidate: NodeId| -> bool {
        document.element(candidate).is_some_and(|element| {
            compound_matches(document, candidate, element, needed, hover_chain)
        }) && complex_matches_from(document, selector, index - 1, candidate, hover_chain)
    };
    match selector.combinators[index - 1] {
        Combinator::Child => document.parent(node_id).is_some_and(step),
        Combinator::Descendant => document.ancestors(node_id).any(step),
        Combinator::NextSibling => {
            let (siblings, position) = element_siblings(document, node_id);
            position > 0 && step(siblings[position - 1])
        }
        Combinator::SubsequentSibling => {
            let (siblings, position) = element_siblings(document, node_id);
            siblings[..position].iter().rev().any(|prior| step(*prior))
        }
    }
}

/// Converts raw declared values into a typed [`ComputedStyle`].
/// `parent_font_size` anchors relative font sizes (`em`, `%`).
fn to_computed(
    raw: &RawStyle,
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
        Some(CssValue::Keyword(keyword)) if keyword == "currentcolor" => Some(style.color),
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
    style.min_width = dimension(raw, "min-width", style.font_size);
    style.max_width = dimension(raw, "max-width", style.font_size);
    style.min_height = dimension(raw, "min-height", style.font_size);
    style.max_height = dimension(raw, "max-height", style.font_size);
    style.margin = edge_dimensions(raw, "margin", Dimension::Px(0.0), style.font_size);
    style.padding = edge_dimensions(raw, "padding", Dimension::Px(0.0), style.font_size);

    let border_style_of = |side: &str| match raw
        .get(format!("border-{side}-style").as_str())
        .and_then(CssValue::as_keyword)
    {
        Some("none" | "hidden") => BorderStyle::None,
        Some("dashed") => BorderStyle::Dashed,
        Some("dotted") => BorderStyle::Dotted,
        _ => BorderStyle::Solid,
    };
    style.border_style = EdgeSizes {
        top: border_style_of("top"),
        right: border_style_of("right"),
        bottom: border_style_of("bottom"),
        left: border_style_of("left"),
    };

    // A side with border-style none has no border, whatever its width.
    let width_of = |side: &str, border_style: BorderStyle| {
        if border_style == BorderStyle::None {
            0.0
        } else {
            edge_px(raw, &format!("border-{side}-width"), style.font_size)
        }
    };
    style.border_width = EdgeSizes {
        top: width_of("top", style.border_style.top),
        right: width_of("right", style.border_style.right),
        bottom: width_of("bottom", style.border_style.bottom),
        left: width_of("left", style.border_style.left),
    };

    style.border_radius = Corners {
        top_left: edge_px(raw, "border-top-left-radius", style.font_size),
        top_right: edge_px(raw, "border-top-right-radius", style.font_size),
        bottom_right: edge_px(raw, "border-bottom-right-radius", style.font_size),
        bottom_left: edge_px(raw, "border-bottom-left-radius", style.font_size),
    };

    // Missing border colors (and the explicit `currentcolor` keyword)
    // fall back to the element color.
    let color_of = |side: &str| {
        raw.get(format!("border-{side}-color").as_str())
            .and_then(CssValue::as_color)
            .unwrap_or(style.color)
    };
    style.border_color = EdgeSizes {
        top: color_of("top"),
        right: color_of("right"),
        bottom: color_of("bottom"),
        left: color_of("left"),
    };

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

    style.monospace = matches!(
        raw.get("font-family").and_then(CssValue::as_keyword),
        Some("monospace")
    );

    style.overflow = match raw.get("overflow").and_then(CssValue::as_keyword) {
        Some("hidden" | "scroll" | "auto" | "clip") => Overflow::Clip,
        _ => Overflow::Visible,
    };

    style.white_space = match raw.get("white-space").and_then(CssValue::as_keyword) {
        Some("pre" | "pre-wrap" | "pre-line") => WhiteSpace::Pre,
        _ => WhiteSpace::Normal,
    };

    style.opacity = match raw.get("opacity") {
        Some(CssValue::Number(value)) => value.clamp(0.0, 1.0),
        Some(CssValue::Length(value, lumen_css::Unit::Percent)) => (value / 100.0).clamp(0.0, 1.0),
        _ => 1.0,
    };

    style.selectable = !matches!(
        raw.get("user-select").and_then(CssValue::as_keyword),
        Some("none")
    );
    style.selection_background = raw
        .get("::selection-background")
        .and_then(CssValue::as_color);
    style.selection_color = raw.get("::selection-color").and_then(CssValue::as_color);

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

    style.position = match raw.get("position").and_then(CssValue::as_keyword) {
        Some("relative") => Position::Relative,
        Some("absolute") => Position::Absolute,
        Some("fixed") => Position::Fixed,
        _ => Position::Static,
    };
    let offset = |name: &str| {
        raw.get(name)
            .and_then(|value| Dimension::from_value(value, style.font_size))
            .unwrap_or(Dimension::Auto)
    };
    style.offsets = EdgeSizes {
        top: offset("top"),
        right: offset("right"),
        bottom: offset("bottom"),
        left: offset("left"),
    };
    style.z_index = match raw.get("z-index") {
        Some(CssValue::Number(value)) => Some(*value as i32),
        _ => None,
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

    style.flex_shrink = match raw.get("flex-shrink") {
        Some(CssValue::Number(value)) => value.max(0.0),
        _ => 1.0,
    };

    style.flex_wrap = matches!(
        raw.get("flex-wrap").and_then(CssValue::as_keyword),
        Some("wrap" | "wrap-reverse")
    );

    style.align_self = match raw.get("align-self").and_then(CssValue::as_keyword) {
        Some("flex-start" | "start") => Some(AlignItems::Start),
        Some("center") => Some(AlignItems::Center),
        Some("flex-end" | "end") => Some(AlignItems::End),
        Some("stretch") => Some(AlignItems::Stretch),
        _ => None,
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

fn dimension(raw: &RawStyle, name: &str, font_size: f32) -> Dimension {
    raw.get(name)
        .and_then(|value| Dimension::from_value(value, font_size))
        .unwrap_or(Dimension::Auto)
}

fn edge_dimensions(
    raw: &RawStyle,
    prefix: &str,
    default: Dimension,
    font_size: f32,
) -> EdgeSizes<Dimension> {
    let side = |name: &str| {
        raw.get(format!("{prefix}-{name}").as_str())
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

fn edge_px(raw: &RawStyle, name: &str, font_size: f32) -> f32 {
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
        assert_eq!(div.border_color.top, Color::rgb(255, 0, 0));
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
    fn border_style_none_suppresses_the_width() {
        let (document, styles) =
            styles_for("<style>div { border-width: 4px; border-style: none; }</style><div>t</div>");
        assert_eq!(style_of(&document, &styles, "div").border_width.top, 0.0);

        let (document, styles) = styles_for(
            "<style>div { border: 2px solid red; border-bottom-style: none; }</style><div>t</div>",
        );
        let div = style_of(&document, &styles, "div");
        assert_eq!(div.border_width.top, 2.0);
        assert_eq!(div.border_width.bottom, 0.0);
    }

    #[test]
    fn per_side_border_colors_with_current_color_fallback() {
        let (document, styles) = styles_for(
            "<style>div { color: #112233; border-width: 1px;
                          border-top-color: red; }</style><div>t</div>",
        );
        let div = style_of(&document, &styles, "div");
        assert_eq!(div.border_color.top, Color::rgb(255, 0, 0));
        assert_eq!(div.border_color.left, Color::rgb(0x11, 0x22, 0x33));
    }

    #[test]
    fn hr_gets_a_default_top_border() {
        let (document, styles) = styles_for("<body><hr></body>");
        let hr = style_of(&document, &styles, "hr");
        assert_eq!(hr.border_width.top, 1.0);
        assert_eq!(hr.border_color.top, Color::rgb(0x80, 0x80, 0x80));
        assert_eq!(hr.display, Display::Block);
    }

    #[test]
    fn small_headings_get_ua_sizes_and_weight() {
        let (document, styles) =
            styles_for("<body><h3>a</h3><h4>b</h4><h5>c</h5><h6>d</h6></body>");
        assert_eq!(style_of(&document, &styles, "h3").font_size, 19.0);
        assert_eq!(style_of(&document, &styles, "h4").font_size, 16.0);
        assert_eq!(style_of(&document, &styles, "h5").font_size, 13.0);
        assert_eq!(style_of(&document, &styles, "h6").font_size, 11.0);
        assert_eq!(
            style_of(&document, &styles, "h3").font_weight,
            FontWeight(700)
        );
    }

    #[test]
    fn position_offsets_and_z_index_parse() {
        let (document, styles) = styles_for(
            "<style>div { position: absolute; top: 10px; left: 2em; z-index: 5; }</style>\
             <div>x</div>",
        );
        let div = style_of(&document, &styles, "div");
        assert_eq!(div.position, Position::Absolute);
        assert_eq!(div.offsets.top, Dimension::Px(10.0));
        assert_eq!(div.offsets.left, Dimension::Px(32.0));
        assert_eq!(div.offsets.bottom, Dimension::Auto);
        assert_eq!(div.z_index, Some(5));
    }

    #[test]
    fn child_combinator_requires_the_direct_parent() {
        let (document, styles) = styles_for(
            "<style>div > p { color: #ff0000; }</style>\
             <div><p>direct</p><section><p>nested</p></section></div>",
        );
        let direct = document
            .descendants(document.root())
            .find(|id| document.element(*id).is_some_and(|e| e.tag_name == "p"))
            .unwrap();
        assert_eq!(styles.by_node[&direct].color.to_string(), "#ff0000");
        let nested = document
            .descendants(document.root())
            .filter(|id| document.element(*id).is_some_and(|e| e.tag_name == "p"))
            .nth(1)
            .unwrap();
        assert_ne!(styles.by_node[&nested].color.to_string(), "#ff0000");
    }

    #[test]
    fn sibling_combinators_match_preceding_elements() {
        let (document, styles) = styles_for(
            "<style>h1 + p { color: #00ff00; } h1 ~ span { color: #0000ff; }</style>\
             <div><h1>t</h1><p>adjacent</p><p>second</p><span>later</span></div>",
        );
        let mut paragraphs = document
            .descendants(document.root())
            .filter(|id| document.element(*id).is_some_and(|e| e.tag_name == "p"));
        let adjacent = paragraphs.next().unwrap();
        let second = paragraphs.next().unwrap();
        assert_eq!(styles.by_node[&adjacent].color.to_string(), "#00ff00");
        assert_ne!(styles.by_node[&second].color.to_string(), "#00ff00");
        let span = document
            .descendants(document.root())
            .find(|id| document.element(*id).is_some_and(|e| e.tag_name == "span"))
            .unwrap();
        assert_eq!(styles.by_node[&span].color.to_string(), "#0000ff");
    }

    #[test]
    fn attribute_selectors_match_values_and_prefixes() {
        let (document, styles) = styles_for(
            "<style>a[href] { color: #111111; }\
                    a[href^='https'] { color: #222222; }\
                    input[type=text] { color: #333333; }</style>\
             <a href='https://x.test'>s</a><input type='text'>",
        );
        let anchor = document
            .descendants(document.root())
            .find(|id| document.element(*id).is_some_and(|e| e.tag_name == "a"))
            .unwrap();
        // Both rules match; equal specificity, later wins.
        assert_eq!(styles.by_node[&anchor].color.to_string(), "#222222");
        let input = document
            .descendants(document.root())
            .find(|id| document.element(*id).is_some_and(|e| e.tag_name == "input"))
            .unwrap();
        assert_eq!(styles.by_node[&input].color.to_string(), "#333333");
    }

    #[test]
    fn structural_pseudo_classes_use_element_positions() {
        let (document, styles) = styles_for(
            "<style>li:first-child { color: #111111; }\
                    li:last-child { color: #222222; }\
                    li:nth-child(2) { color: #333333; }</style>\
             <ul> <li>one</li> <li>two</li> <li>three</li> </ul>",
        );
        let items: Vec<_> = document
            .descendants(document.root())
            .filter(|id| document.element(*id).is_some_and(|e| e.tag_name == "li"))
            .collect();
        assert_eq!(styles.by_node[&items[0]].color.to_string(), "#111111");
        assert_eq!(styles.by_node[&items[1]].color.to_string(), "#333333");
        assert_eq!(styles.by_node[&items[2]].color.to_string(), "#222222");
    }

    #[test]
    fn nth_child_odd_and_not_exclude_elements() {
        let (document, styles) = styles_for(
            "<style>li:nth-child(odd) { color: #123456; }\
                    li:not(.keep) { font-weight: 700; }</style>\
             <ul><li>one</li><li class='keep'>two</li><li>three</li></ul>",
        );
        let items: Vec<_> = document
            .descendants(document.root())
            .filter(|id| document.element(*id).is_some_and(|e| e.tag_name == "li"))
            .collect();
        assert_eq!(styles.by_node[&items[0]].color.to_string(), "#123456");
        assert_ne!(styles.by_node[&items[1]].color.to_string(), "#123456");
        assert_eq!(styles.by_node[&items[2]].color.to_string(), "#123456");
        assert_eq!(styles.by_node[&items[0]].font_weight.0, 700);
        assert_ne!(styles.by_node[&items[1]].font_weight.0, 700);
    }

    #[test]
    fn rem_resolves_against_the_root_font_size() {
        let (document, styles) = styles_for(
            "<html><head><style>html { font-size: 20px; } p { width: 2rem; font-size: 1.5rem; }\
             </style></head><body><p>x</p></body></html>",
        );
        let p = style_of(&document, &styles, "p");
        assert_eq!(p.width, Dimension::Px(40.0));
        assert_eq!(p.font_size, 30.0);
    }

    #[test]
    fn inherit_and_initial_keywords_resolve() {
        let (document, styles) = styles_for(
            "<style>div { width: 300px; color: #ff0000; }\
                    p { width: inherit; color: initial; }</style>\
             <div><p>x</p></div>",
        );
        let p = style_of(&document, &styles, "p");
        assert_eq!(p.width, Dimension::Px(300.0));
        // color would inherit red; `initial` resets it to the default.
        assert_eq!(p.color.to_string(), "#111111");
    }

    #[test]
    fn important_beats_specificity_and_inline() {
        let (document, styles) = styles_for(
            "<style>p { color: #ff0000 !important; }\
                    #target { color: #00ff00; }</style>\
             <p id='target' style='color: #0000ff'>x</p>",
        );
        assert_eq!(
            style_of(&document, &styles, "p").color.to_string(),
            "#ff0000"
        );
    }

    #[test]
    fn inline_important_beats_author_important() {
        let (document, styles) = styles_for(
            "<style>p { color: #ff0000 !important; }</style>\
             <p style='color: #0000ff !important'>x</p>",
        );
        assert_eq!(
            style_of(&document, &styles, "p").color.to_string(),
            "#0000ff"
        );
    }

    #[test]
    fn pre_and_code_get_monospace_defaults() {
        let (document, styles) = styles_for("<pre>x</pre><p><code>y</code></p>");
        let pre = style_of(&document, &styles, "pre");
        assert!(pre.monospace);
        assert_eq!(pre.white_space, WhiteSpace::Pre);
        let code = style_of(&document, &styles, "code");
        assert!(code.monospace);
        assert_eq!(code.white_space, WhiteSpace::Normal);
    }

    #[test]
    fn font_family_normalizes_to_a_generic() {
        let (document, styles) = styles_for(
            "<style>p { font-family: Menlo, monospace; } h1 { font-family: Arial; }</style>\
             <p>m</p><h1>a</h1>",
        );
        assert!(style_of(&document, &styles, "p").monospace);
        assert!(!style_of(&document, &styles, "h1").monospace);
    }

    #[test]
    fn opacity_clamps_and_defaults() {
        let (document, styles) = styles_for(
            "<style>.a { opacity: 0.5; } .b { opacity: 3; } .c { opacity: 40%; }</style>\
             <div class='a'>x</div><div class='b'>y</div><div class='c'>z</div>",
        );
        let of = |class: &str| {
            let id = document
                .descendants(document.root())
                .find(|id| {
                    document
                        .element(*id)
                        .is_some_and(|element| element.has_class(class))
                })
                .unwrap();
            styles.by_node[&id].opacity
        };
        assert_eq!(of("a"), 0.5);
        assert_eq!(of("b"), 1.0);
        assert_eq!(of("c"), 0.4);
    }

    #[test]
    fn current_color_keyword_uses_the_element_color() {
        let (document, styles) = styles_for(
            "<style>div { color: #123456; background-color: currentcolor; }</style><div>x</div>",
        );
        let div = style_of(&document, &styles, "div");
        assert_eq!(div.background_color, Some(Color::rgb(0x12, 0x34, 0x56)));
    }

    #[test]
    fn user_select_none_inherits_down() {
        let (document, styles) = styles_for(
            "<style>.locked { user-select: none; }</style>\
             <div class='locked'><p>t</p></div><p>free</p>",
        );
        let locked = document
            .descendants(document.root())
            .find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.has_class("locked"))
            })
            .unwrap();
        let inner_p = document
            .descendants(locked)
            .find(|id| document.element(*id).is_some())
            .unwrap();
        assert!(!styles.by_node[&inner_p].selectable);
        // The sibling paragraph stays selectable.
        let free = document
            .descendants(document.root())
            .filter(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "p")
            })
            .last()
            .unwrap();
        assert!(styles.by_node[&free].selectable);
    }

    #[test]
    fn selection_pseudo_element_styles_the_highlight() {
        let (document, styles) = styles_for(
            "<style>p::selection { background-color: #f5c518; color: white; }\
             p { color: #111111; }</style><p>t</p>",
        );
        let p = style_of(&document, &styles, "p");
        assert_eq!(p.selection_background, Some(Color::rgb(0xf5, 0xc5, 0x18)));
        assert_eq!(p.selection_color, Some(Color::rgb(255, 255, 255)));
        // The rule did not leak into the element's own colors.
        assert_eq!(p.color, Color::rgb(0x11, 0x11, 0x11));
        assert_eq!(p.background_color, None);
    }

    #[test]
    fn border_radius_expands_and_resolves_em() {
        let (document, styles) = styles_for(
            "<style>div { font-size: 10px; border-radius: 4px 1em; }</style><div>t</div>",
        );
        let radius = style_of(&document, &styles, "div").border_radius;
        assert_eq!(radius.top_left, 4.0);
        assert_eq!(radius.top_right, 10.0);
        assert_eq!(radius.bottom_right, 4.0);
        assert_eq!(radius.bottom_left, 10.0);
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
