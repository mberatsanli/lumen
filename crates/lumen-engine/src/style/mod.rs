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
use crate::ua::{default_display, user_agent_stylesheet};
use lumen_css::{Color, CssValue, Specificity, Stylesheet};
use lumen_html::{Document, ElementData, NodeId, NodeKind};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

mod interaction;
mod matching;
mod model;

pub use interaction::{
    HoverImpact, InteractionState, hover_impact, hover_styles_may_change,
    interaction_styles_may_change,
};
pub(crate) use matching::MatchContext;
pub use model::*;

/// Declared values keyed by property name. `Cow` keys let the fixed
/// property names (inherited copies, internal inserts) avoid per-node
/// string allocations during the cascade.
type RawStyle = HashMap<Cow<'static, str>, CssValue>;

/// Computed styles for every node, keyed by [`NodeId`].
#[derive(Debug, Clone, PartialEq)]
pub struct StyleMap {
    pub by_node: HashMap<NodeId, ComputedStyle>,
    /// `::before`/`::after` generated content, ready to materialize as
    /// text nodes (see `apply_generated_content`).
    pub pseudo_texts: Vec<PseudoText>,
}

/// One piece of CSS-generated content.
#[derive(Debug, Clone, PartialEq)]
pub struct PseudoText {
    pub element: NodeId,
    /// `::before` inserts leading, `::after` trailing.
    pub leading: bool,
    pub text: String,
    pub style: ComputedStyle,
}

/// Parses a `transform` value list into a matrix (public for the
/// browser's keyframe interpolation).
#[must_use]
pub fn parse_transform_value(source: &str, font_size: f32) -> Option<Transform2D> {
    parse_transform(source, font_size)
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
    compute_styles_interactive(
        document,
        author,
        &InteractionState::new(document, hovered, None, None),
    )
}

/// [`compute_styles`] with full interaction state
/// (:hover/:active/:focus).
#[must_use]
pub fn compute_styles_interactive(
    document: &Document,
    author: &Stylesheet,
    interaction: &InteractionState,
) -> StyleMap {
    let mut by_node = HashMap::new();
    let mut pseudo_texts = Vec::new();
    let inherited = HashMap::new();
    let context = StyleContext::new(document, author, interaction);
    compute_node(
        document,
        document.root(),
        &context,
        &inherited,
        DEFAULT_FONT_SIZE,
        &mut by_node,
        &mut pseudo_texts,
        0,
    );
    StyleMap {
        by_node,
        pseudo_texts,
    }
}

/// One stylesheet in the cascade with data precomputed once per style
/// pass instead of per element: selector specificities, and whether any
/// selector targets `::before`/`::after` (so pseudo passes on sheets
/// without such rules — the common case — are skipped entirely).
struct CascadeSheet<'a> {
    sheet: &'a Stylesheet,
    /// `specificities[rule][selector]`, aligned with `sheet.rules`.
    specificities: Vec<Vec<Specificity>>,
    has_before: bool,
    has_after: bool,
}

impl<'a> CascadeSheet<'a> {
    fn new(sheet: &'a Stylesheet) -> Self {
        let mut has_before = false;
        let mut has_after = false;
        let specificities = sheet
            .rules
            .iter()
            .map(|rule| {
                rule.selectors
                    .iter()
                    .map(|selector| {
                        match selector.subject().pseudo_element.as_deref() {
                            Some("before") => has_before = true,
                            Some("after") => has_after = true,
                            _ => {}
                        }
                        selector.specificity()
                    })
                    .collect()
            })
            .collect();
        Self {
            sheet,
            specificities,
            has_before,
            has_after,
        }
    }

    /// Whether any rule in the sheet targets this pseudo-element.
    fn has_pseudo(&self, kind: &str) -> bool {
        match kind {
            "before" => self.has_before,
            "after" => self.has_after,
            _ => false,
        }
    }
}

/// Per-style-pass shared state: the sibling/matching cache and the
/// cascade sheets, weakest origin (UA) first.
struct StyleContext<'a> {
    matcher: MatchContext<'a>,
    sheets: [CascadeSheet<'a>; 2],
}

impl<'a> StyleContext<'a> {
    fn new(
        document: &'a Document,
        author: &'a Stylesheet,
        interaction: &'a InteractionState,
    ) -> Self {
        Self {
            matcher: MatchContext::new(document, interaction),
            sheets: [
                CascadeSheet::new(user_agent_stylesheet()),
                CascadeSheet::new(author),
            ],
        }
    }
}

/// Substitutes `var(--name[, fallback])` occurrences from the raw map
/// (custom properties carry their text in `CssValue::String`). Depth-caps
/// self-referential chains. `None` when a variable has no value and no
/// fallback.
fn substitute_vars(text: &str, raw: &RawStyle, depth: usize) -> Option<String> {
    if depth > 8 {
        return None;
    }
    let Some(start) = text.find("var(") else {
        return Some(text.to_string());
    };
    let after = &text[start + 4..];
    let mut nesting = 1usize;
    let mut close = None;
    for (index, character) in after.char_indices() {
        match character {
            '(' => nesting += 1,
            ')' => {
                nesting -= 1;
                if nesting == 0 {
                    close = Some(index);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    let arguments = &after[..close];
    let (variable, fallback) = match arguments.find(',') {
        Some(comma) => (
            arguments[..comma].trim(),
            Some(arguments[comma + 1..].trim()),
        ),
        None => (arguments.trim(), None),
    };
    let replacement = match raw.get(variable) {
        Some(CssValue::String(value)) => substitute_vars(value, raw, depth + 1)?,
        _ => substitute_vars(fallback?, raw, depth + 1)?,
    };
    let rest = substitute_vars(&after[close + 1..], raw, depth)?;
    Some(format!("{}{replacement}{rest}", &text[..start]))
}

/// A calc() term family: absolute pixels, percent, or a bare number.
#[derive(Clone, Copy)]
enum CalcValue {
    Px(f32),
    Percent(f32),
    Number(f32),
}

/// Evaluates a calc() expression. Units: px/em/rem/% and numbers; `em`
/// resolves against `font_size`. Additive mixing of px and % is
/// unsupported and returns `None`.
fn evaluate_calc(expression: &str, font_size: f32, root_font_size: f32) -> Option<CssValue> {
    struct Parser<'a> {
        tokens: Vec<&'a str>,
        position: usize,
    }
    // Tokenize: parens and operators split; everything else is a term.
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    for character in expression.chars() {
        match character {
            '(' | ')' | '+' | '*' | '/' | ',' => {
                if !current.trim().is_empty() {
                    tokens.push(current.trim().to_string());
                }
                current.clear();
                tokens.push(character.to_string());
            }
            // Minus only splits when surrounded by whitespace (CSS
            // requires it); `-n` stays part of a number.
            ' ' | '\t' => {
                if !current.trim().is_empty() {
                    tokens.push(current.trim().to_string());
                }
                current.clear();
            }
            _ => current.push(character),
        }
    }
    if !current.trim().is_empty() {
        tokens.push(current.trim().to_string());
    }
    let tokens: Vec<&str> = tokens.iter().map(String::as_str).collect();

    fn parse_term(
        parser: &mut Parser<'_>,
        font_size: f32,
        root_font_size: f32,
    ) -> Option<CalcValue> {
        let token = *parser.tokens.get(parser.position)?;
        parser.position += 1;
        if token == "(" {
            let value = parse_sum(parser, font_size, root_font_size)?;
            if parser.tokens.get(parser.position) == Some(&")") {
                parser.position += 1;
                return Some(value);
            }
            return None;
        }
        // min()/max()/clamp() nest inside expressions; arguments must
        // share a family (all px-like or all %).
        if matches!(token, "min" | "max" | "clamp")
            && parser.tokens.get(parser.position) == Some(&"(")
        {
            parser.position += 1;
            let mut arguments: Vec<CalcValue> = Vec::new();
            loop {
                arguments.push(parse_sum(parser, font_size, root_font_size)?);
                match parser.tokens.get(parser.position) {
                    Some(&",") => parser.position += 1,
                    Some(&")") => {
                        parser.position += 1;
                        break;
                    }
                    _ => return None,
                }
            }
            let numbers: Option<Vec<f32>> = match arguments.first()? {
                CalcValue::Px(_) => arguments
                    .iter()
                    .map(|value| match value {
                        CalcValue::Px(number) => Some(*number),
                        _ => None,
                    })
                    .collect(),
                CalcValue::Percent(_) => arguments
                    .iter()
                    .map(|value| match value {
                        CalcValue::Percent(number) => Some(*number),
                        _ => None,
                    })
                    .collect(),
                CalcValue::Number(_) => arguments
                    .iter()
                    .map(|value| match value {
                        CalcValue::Number(number) => Some(*number),
                        _ => None,
                    })
                    .collect(),
            };
            let numbers = numbers?;
            let combined = match token {
                "min" => numbers.iter().copied().fold(f32::INFINITY, f32::min),
                "max" => numbers.iter().copied().fold(f32::NEG_INFINITY, f32::max),
                _ => {
                    if numbers.len() != 3 {
                        return None;
                    }
                    numbers[1].clamp(numbers[0], numbers[2])
                }
            };
            return Some(match arguments[0] {
                CalcValue::Px(_) => CalcValue::Px(combined),
                CalcValue::Percent(_) => CalcValue::Percent(combined),
                CalcValue::Number(_) => CalcValue::Number(combined),
            });
        }
        let value = lumen_css::CssValue::parse_component(token)?;
        match value {
            CssValue::Length(size, lumen_css::Unit::Px) => Some(CalcValue::Px(size)),
            CssValue::Length(size, lumen_css::Unit::Em) => Some(CalcValue::Px(size * font_size)),
            CssValue::Length(size, lumen_css::Unit::Rem) => {
                Some(CalcValue::Px(size * root_font_size))
            }
            CssValue::Length(size, lumen_css::Unit::Percent) => Some(CalcValue::Percent(size)),
            CssValue::Number(number) => Some(CalcValue::Number(number)),
            _ => None,
        }
    }

    fn parse_product(
        parser: &mut Parser<'_>,
        font_size: f32,
        root_font_size: f32,
    ) -> Option<CalcValue> {
        let mut left = parse_term(parser, font_size, root_font_size)?;
        while let Some(operator) = parser.tokens.get(parser.position).copied() {
            if operator != "*" && operator != "/" {
                break;
            }
            parser.position += 1;
            let right = parse_term(parser, font_size, root_font_size)?;
            left = match (left, right, operator) {
                (value, CalcValue::Number(number), "*")
                | (CalcValue::Number(number), value, "*") => scale(value, number),
                (value, CalcValue::Number(number), "/") if number != 0.0 => {
                    scale(value, 1.0 / number)
                }
                _ => return None,
            };
        }
        Some(left)
    }

    fn scale(value: CalcValue, factor: f32) -> CalcValue {
        match value {
            CalcValue::Px(size) => CalcValue::Px(size * factor),
            CalcValue::Percent(size) => CalcValue::Percent(size * factor),
            CalcValue::Number(number) => CalcValue::Number(number * factor),
        }
    }

    fn parse_sum(
        parser: &mut Parser<'_>,
        font_size: f32,
        root_font_size: f32,
    ) -> Option<CalcValue> {
        let mut left = parse_product(parser, font_size, root_font_size)?;
        while let Some(operator) = parser.tokens.get(parser.position).copied() {
            let sign = match operator {
                "+" => 1.0,
                "-" => -1.0,
                _ => break,
            };
            parser.position += 1;
            let right = parse_product(parser, font_size, root_font_size)?;
            left = match (left, right) {
                (CalcValue::Px(a), CalcValue::Px(b)) => CalcValue::Px(a + sign * b),
                (CalcValue::Percent(a), CalcValue::Percent(b)) => CalcValue::Percent(a + sign * b),
                (CalcValue::Number(a), CalcValue::Number(b)) => CalcValue::Number(a + sign * b),
                _ => return None, // px + % needs layout-time resolution.
            };
        }
        Some(left)
    }

    let mut parser = Parser {
        tokens,
        position: 0,
    };
    let value = parse_sum(&mut parser, font_size, root_font_size)?;
    if parser.position != parser.tokens.len() {
        return None;
    }
    Some(match value {
        CalcValue::Px(size) => CssValue::Length(size, lumen_css::Unit::Px),
        CalcValue::Percent(size) => CssValue::Length(size, lumen_css::Unit::Percent),
        CalcValue::Number(number) => CssValue::Number(number),
    })
}

#[allow(clippy::too_many_arguments)]
fn compute_node(
    document: &Document,
    node_id: NodeId,
    context: &StyleContext<'_>,
    parent_raw: &RawStyle,
    root_font_size: f32,
    output: &mut HashMap<NodeId, ComputedStyle>,
    pseudo_texts: &mut Vec<PseudoText>,
    depth: usize,
) {
    // Depth guard: absurdly nested documents stop here; the skipped
    // subtree keeps no style entry, so layout treats it as unstyled.
    if depth >= crate::MAX_DEPTH {
        return;
    }
    let mut raw = RawStyle::new();
    for property in lumen_css::properties::inherited() {
        if let Some(value) = parent_raw.get(property) {
            raw.insert(Cow::Borrowed(property), value.clone());
        }
    }
    // Custom properties (`--x`) inherit wholesale.
    for (name, value) in parent_raw {
        if name.starts_with("--") {
            raw.insert(name.clone(), value.clone());
        }
    }

    let element = match &document.node(node_id).kind {
        NodeKind::Element(element) => Some(element),
        _ => None,
    };

    // Text runs are anonymous inline content: CSS does not inherit
    // `vertical-align` or `transition`, but both apply to the inline box
    // the text belongs to — and this engine flattens inline boxes into
    // per-text-node runs, so the parent's values must reach text nodes
    // for `sup`/`sub` shifts and hover color fades to work on text.
    if element.is_none() {
        for property in ["vertical-align", "transition"] {
            if let Some(value) = parent_raw.get(property) {
                raw.insert(Cow::Borrowed(property), value.clone());
            }
        }
    }

    if let Some(element) = element {
        // Weakest origin first; each stronger origin overwrites per
        // property — unless a weaker origin declared it `!important`
        // (author important beats inline normal).
        let mut important: HashSet<String> = HashSet::new();
        for cascade_sheet in &context.sheets {
            for (name, (is_important, _, _, value)) in
                winning_declarations(node_id, element, cascade_sheet, &context.matcher, None)
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

    // var() substitution: unresolved values substitute custom properties
    // (with fallbacks), then re-parse as a normal declaration so
    // shorthands still expand.
    let pending: Vec<(String, String)> = raw
        .iter()
        .filter_map(|(name, value)| match value {
            CssValue::Unresolved(text) => Some((name.clone().into_owned(), text.clone())),
            _ => None,
        })
        .collect();
    for (name, text) in pending {
        raw.remove(name.as_str());
        let Some(substituted) = substitute_vars(&text, &raw, 0) else {
            continue; // Unknown variable without fallback: declaration dies.
        };
        for declaration in lumen_css::parse_declarations(&format!("{name}: {substituted}")) {
            raw.insert(Cow::Owned(declaration.name), declaration.value);
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
            "unset" => lumen_css::properties::is_inherited(name.as_str()),
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

    // calc(): evaluated when all terms share a family (px-likes or %).
    // `em` resolves against the parent font size (exact for font-size,
    // an approximation elsewhere); mixed px/% expressions are dropped.
    let calc_names: Vec<String> = raw
        .iter()
        .filter_map(|(name, value)| match value {
            CssValue::Function(function, _)
                if matches!(function.as_str(), "calc" | "min" | "max" | "clamp") =>
            {
                Some(name.clone().into_owned())
            }
            _ => None,
        })
        .collect();
    for name in calc_names {
        let CssValue::Function(function, expression) = raw[name.as_str()].clone() else {
            continue;
        };
        // Bare min()/max()/clamp() evaluate through the calc grammar.
        let expression = if function == "calc" {
            expression
        } else {
            format!("{function}({expression})")
        };
        match evaluate_calc(&expression, parent_font_size, root_font_size) {
            Some(value) => raw.insert(Cow::Owned(name), value),
            None => raw.remove(name.as_str()),
        };
    }
    let computed = to_computed(&raw, element, parent_font_size);
    // Children inherit the *resolved* font size, so `em` chains and
    // percentages resolve against real pixels, not unresolved declarations.
    raw.insert(
        Cow::Borrowed("font-size"),
        CssValue::Length(computed.font_size, lumen_css::Unit::Px),
    );

    // `::before`/`::after`: a pseudo style inherits from the element like
    // a child and needs a string `content` to generate anything.
    if let Some(element) = element {
        for (kind, leading) in [("before", true), ("after", false)] {
            // No rule in either sheet targets this pseudo-element (the
            // common case): the pass below would produce nothing.
            if !context.sheets.iter().any(|sheet| sheet.has_pseudo(kind)) {
                continue;
            }
            let mut pseudo_raw = RawStyle::new();
            for property in lumen_css::properties::inherited() {
                if let Some(value) = raw.get(property) {
                    pseudo_raw.insert(Cow::Borrowed(property), value.clone());
                }
            }
            pseudo_raw.insert(
                Cow::Borrowed("font-size"),
                CssValue::Length(computed.font_size, lumen_css::Unit::Px),
            );
            let mut any = false;
            for cascade_sheet in &context.sheets {
                for (name, (_, _, _, value)) in winning_declarations(
                    node_id,
                    element,
                    cascade_sheet,
                    &context.matcher,
                    Some(kind),
                ) {
                    pseudo_raw.insert(Cow::Owned(name), value);
                    any = true;
                }
            }
            if !any {
                continue;
            }
            let Some(CssValue::String(text)) = pseudo_raw.get("content") else {
                continue;
            };
            if matches!(pseudo_raw.get("display"), Some(CssValue::Keyword(keyword)) if keyword == "none")
            {
                continue;
            }
            let text = text.clone();
            for value in pseudo_raw.values_mut() {
                if let CssValue::Length(size, lumen_css::Unit::Rem) = value {
                    *value = CssValue::Length(*size * root_font_size, lumen_css::Unit::Px);
                }
            }
            let mut style = to_computed(&pseudo_raw, None, computed.font_size);
            // Generated content is not selectable (as in browsers).
            style.selectable = false;
            pseudo_texts.push(PseudoText {
                element: node_id,
                leading,
                text,
                style,
            });
        }
    }
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
            context,
            &raw,
            root_font_size,
            output,
            pseudo_texts,
            depth + 1,
        );
    }
}

/// Per-property winner within one origin: highest (importance,
/// specificity, source order) triple wins; later rules win ties.
/// Specificities come precomputed from the [`CascadeSheet`]; matching
/// reuses the shared sibling cache in the [`MatchContext`].
fn winning_declarations(
    node_id: NodeId,
    element: &ElementData,
    cascade_sheet: &CascadeSheet<'_>,
    matcher: &MatchContext<'_>,
    pseudo: Option<&str>,
) -> HashMap<String, (bool, Specificity, usize, CssValue)> {
    let mut winners: HashMap<String, (bool, Specificity, usize, CssValue)> = HashMap::new();
    for (rule, specificities) in cascade_sheet
        .sheet
        .rules
        .iter()
        .zip(&cascade_sheet.specificities)
    {
        for (selector, specificity) in rule.selectors.iter().zip(specificities) {
            if matching::selector_matches(matcher, node_id, element, selector) {
                // `::selection` rules style the highlight, not the element:
                // only their background-color/color apply, under internal
                // property names.
                let pseudo_element = selector.subject().pseudo_element.as_deref();
                for declaration in &rule.declarations {
                    let name = match (pseudo, pseudo_element) {
                        // Element pass: plain rules apply; `::selection`
                        // rules route under internal property names.
                        (None, None) => declaration.name.clone(),
                        (None, Some("selection")) => match declaration.name.as_str() {
                            "background-color" => "::selection-background".to_string(),
                            "color" => "::selection-color".to_string(),
                            _ => continue,
                        },
                        // Pseudo pass: only rules for that pseudo-element.
                        (Some(wanted), Some(actual)) if wanted == actual => {
                            declaration.name.clone()
                        }
                        _ => continue,
                    };
                    let candidate = (
                        declaration.important,
                        *specificity,
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

/// Upper bound for a computed `font-size`: bigger values only occur in
/// pathological pages and would explode glyph caches and layouts.
const MAX_FONT_SIZE: f32 = 512.0;

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
    // Keep the resolved size sane: negatives/NaN/inf (huge `em` chains,
    // hostile calc()) fall back to the inherited size or clamp.
    style.font_size = if style.font_size.is_finite() {
        style.font_size.clamp(0.0, MAX_FONT_SIZE)
    } else {
        parent_font_size
    };

    style.line_height = match raw.get("line-height") {
        Some(CssValue::Number(factor)) => factor * style.font_size,
        Some(CssValue::Length(factor, lumen_css::Unit::Em)) => factor * style.font_size,
        Some(CssValue::Length(percent, lumen_css::Unit::Percent)) => {
            percent / 100.0 * style.font_size
        }
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
            "grid" => Some(Display::Grid),
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

    // text-decoration: joined multi-value (lines, style, color mix).
    if let Some(text) = raw.get("text-decoration").map(CssValue::raw_text) {
        for piece in text.split_whitespace() {
            match piece {
                "underline" => style.underline = true,
                "line-through" => style.line_through = true,
                "overline" => {}
                "none" => {
                    style.underline = false;
                    style.line_through = false;
                }
                "solid" | "double" | "wavy" => {
                    style.text_decoration_style = BorderStyle::Solid;
                }
                "dashed" => style.text_decoration_style = BorderStyle::Dashed,
                "dotted" => style.text_decoration_style = BorderStyle::Dotted,
                other => {
                    if let Some(color) = Color::parse(other) {
                        style.text_decoration_color = Some(color);
                    }
                }
            }
        }
    }
    if let Some(CssValue::Color(color)) = raw.get("text-decoration-color") {
        style.text_decoration_color = Some(*color);
    }
    match raw
        .get("text-decoration-style")
        .and_then(CssValue::as_keyword)
    {
        Some("dashed") => style.text_decoration_style = BorderStyle::Dashed,
        Some("dotted") => style.text_decoration_style = BorderStyle::Dotted,
        Some(_) => style.text_decoration_style = BorderStyle::Solid,
        None => {}
    }

    style.italic = matches!(
        raw.get("font-style").and_then(CssValue::as_keyword),
        Some("italic" | "oblique")
    );

    // Background layers: the image list defines the layer count; the
    // position/size/repeat lists cycle when shorter. Gradients keep their
    // internal commas (top-level splitting is paren-aware).
    {
        let list = |name: &str| -> Vec<String> {
            raw.get(name)
                .map(|value| {
                    split_top_level_commas(&value.raw_text())
                        .iter()
                        .map(|part| part.trim().to_string())
                        .collect()
                })
                .unwrap_or_default()
        };
        let images = list("background-image");
        let positions = list("background-position");
        let sizes = list("background-size");
        let repeats = list("background-repeat");

        let position_component = |value: &str| -> Option<Dimension> {
            match value {
                "left" | "top" => Some(Dimension::Px(0.0)),
                "center" => Some(Dimension::Percent(50.0)),
                "right" | "bottom" => Some(Dimension::Percent(100.0)),
                other => Dimension::from_value(&CssValue::parse_component(other)?, style.font_size),
            }
        };
        let parse_position = |text: &str| -> (Dimension, Dimension) {
            let mut pieces = text.split_whitespace();
            let x = pieces.next().and_then(position_component);
            let y = pieces.next().and_then(position_component);
            (
                x.unwrap_or(Dimension::Px(0.0)),
                y.or(x.map(|_| Dimension::Percent(50.0)))
                    .unwrap_or(Dimension::Px(0.0)),
            )
        };
        let parse_size = |text: &str| -> BackgroundSize {
            match text {
                "cover" => BackgroundSize::Cover,
                "contain" => BackgroundSize::Contain,
                other => {
                    let mut pieces = other.split_whitespace();
                    match pieces
                        .next()
                        .and_then(CssValue::parse_component)
                        .and_then(|piece| Dimension::from_value(&piece, style.font_size))
                    {
                        Some(width) => {
                            let height = pieces
                                .next()
                                .and_then(CssValue::parse_component)
                                .and_then(|piece| Dimension::from_value(&piece, style.font_size))
                                .unwrap_or(Dimension::Auto);
                            BackgroundSize::Explicit(width, height)
                        }
                        None => BackgroundSize::Auto,
                    }
                }
            }
        };
        let parse_repeat = |text: &str| -> (bool, bool) {
            match text {
                "no-repeat" => (false, false),
                "repeat-x" => (true, false),
                "repeat-y" => (false, true),
                _ => (true, true),
            }
        };
        let parse_image = |text: &str| -> Option<BackgroundImage> {
            match CssValue::parse_component(text)? {
                CssValue::Url(url) => Some(BackgroundImage::Url(url)),
                CssValue::Function(name, arguments) if name == "linear-gradient" => {
                    parse_linear_gradient(&arguments).map(BackgroundImage::LinearGradient)
                }
                CssValue::Function(name, arguments)
                    if name == "radial-gradient" || name == "conic-gradient" =>
                {
                    let parts = split_top_level_commas(&arguments);
                    let start = usize::from(
                        Color::parse(parts[0].split_whitespace().next().unwrap_or("")).is_none(),
                    );
                    let stops = parse_gradient_stops(&parts[start..]);
                    if name == "radial-gradient" {
                        stops.map(BackgroundImage::RadialGradient)
                    } else {
                        stops.map(BackgroundImage::ConicGradient)
                    }
                }
                _ => None,
            }
        };

        style.background_layers = images
            .iter()
            .enumerate()
            .filter_map(|(index, text)| {
                let image = parse_image(text)?;
                let pick = |values: &[String], fallback: &str| -> String {
                    if values.is_empty() {
                        fallback.to_string()
                    } else {
                        values[index % values.len()].clone()
                    }
                };
                Some(BackgroundLayer {
                    image,
                    position: parse_position(&pick(&positions, "0px 0px")),
                    size: parse_size(&pick(&sizes, "auto")),
                    repeat: parse_repeat(&pick(&repeats, "repeat")),
                })
            })
            .collect();
    }

    style.aspect_ratio = raw
        .get("aspect-ratio")
        .map(CssValue::raw_text)
        .as_deref()
        .and_then(|text| {
            let mut pieces = text.split('/').map(str::trim);
            let width: f32 = pieces.next()?.parse().ok()?;
            let height: f32 = match pieces.next() {
                Some(piece) => piece.parse().ok()?,
                None => 1.0,
            };
            (height > 0.0 && width > 0.0).then_some(width / height)
        });

    // grid-template-columns: px/%/fr/auto tracks with repeat(N, ...).
    style.grid_columns = raw
        .get("grid-template-columns")
        .map(|value| parse_grid_tracks(&value.raw_text(), style.font_size))
        .unwrap_or_default();

    style.grid_span = raw
        .get("grid-column")
        .map(CssValue::raw_text)
        .as_deref()
        .and_then(|text| {
            let text = text.trim();
            text.strip_prefix("span")
                .and_then(|rest| rest.trim().parse::<usize>().ok())
        })
        .filter(|span| *span >= 1)
        .unwrap_or(1);

    style.transform = raw
        .get("transform")
        .map(CssValue::raw_text)
        .as_deref()
        .and_then(|text| parse_transform(text, style.font_size))
        .filter(|matrix| !matrix.is_identity());

    if let Some(text) = raw.get("transform-origin").map(CssValue::raw_text) {
        let component = |value: &str| -> Option<Dimension> {
            match value {
                "left" | "top" => Some(Dimension::Percent(0.0)),
                "center" => Some(Dimension::Percent(50.0)),
                "right" | "bottom" => Some(Dimension::Percent(100.0)),
                other => Dimension::from_value(&CssValue::parse_component(other)?, style.font_size),
            }
        };
        let mut pieces = text.split_whitespace();
        if let Some(x) = pieces.next().and_then(component) {
            let y = pieces
                .next()
                .and_then(component)
                .unwrap_or(Dimension::Percent(50.0));
            style.transform_origin = (x, y);
        }
    }

    // transition: property duration [timing] [delay], comma-separated.
    style.transitions = raw
        .get("transition")
        .map(CssValue::raw_text)
        .map(|text| {
            text.split(',')
                .filter_map(|entry| {
                    let mut property = String::from("all");
                    let mut times: Vec<f32> = Vec::new();
                    let mut ease = true;
                    for piece in entry.split_whitespace() {
                        if let Some(seconds) = piece.strip_suffix("ms") {
                            if let Ok(value) = seconds.parse::<f32>() {
                                times.push(value / 1000.0);
                            }
                        } else if let Some(seconds) = piece.strip_suffix('s') {
                            if let Ok(value) = seconds.parse::<f32>() {
                                times.push(value);
                            }
                        } else if piece == "linear" {
                            ease = false;
                        } else if matches!(piece, "ease" | "ease-in" | "ease-out" | "ease-in-out")
                            || piece.starts_with("cubic-bezier")
                        {
                            ease = true;
                        } else {
                            property = piece.to_string();
                        }
                    }
                    let duration = *times.first()?;
                    (duration > 0.0).then_some(TransitionSpec {
                        property,
                        duration,
                        delay: times.get(1).copied().unwrap_or(0.0),
                        ease,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    // animation: name duration [delay] [iterations] [timing] (first
    // comma-separated entry only).
    style.animation = raw
        .get("animation")
        .map(CssValue::raw_text)
        .and_then(|text| {
            let entry = text.split(',').next()?;
            let mut name = String::new();
            let mut times: Vec<f32> = Vec::new();
            let mut iterations = 1.0f32;
            let mut ease = true;
            for piece in entry.split_whitespace() {
                if let Some(millis) = piece.strip_suffix("ms") {
                    if let Ok(value) = millis.parse::<f32>() {
                        times.push(value / 1000.0);
                    }
                } else if let Some(seconds) = piece.strip_suffix('s')
                    && let Ok(value) = seconds.parse::<f32>()
                {
                    times.push(value);
                } else if piece == "infinite" {
                    iterations = f32::INFINITY;
                } else if let Ok(count) = piece.parse::<f32>() {
                    iterations = count;
                } else if piece == "linear" {
                    ease = false;
                } else if matches!(piece, "ease" | "ease-in" | "ease-out" | "ease-in-out")
                    || piece.starts_with("cubic-bezier")
                    || matches!(
                        piece,
                        "normal" | "forwards" | "backwards" | "both" | "alternate"
                    )
                {
                    // Timing keywords keep the default; fill/direction modes
                    // are accepted but not modeled.
                } else {
                    name = piece.to_string();
                }
            }
            let duration = times.first().copied()?;
            (duration > 0.0 && !name.is_empty()).then_some(AnimationSpec {
                name,
                duration,
                delay: times.get(1).copied().unwrap_or(0.0),
                iterations,
                ease,
            })
        });

    style.list_style_none = matches!(
        raw.get("list-style-type")
            .or_else(|| raw.get("list-style"))
            .and_then(CssValue::as_keyword),
        Some("none")
    );
    style.mark = match raw.get("--lumen-mark").map(CssValue::raw_text).as_deref() {
        Some("check") => Some(Mark::Check),
        Some("dot") => Some(Mark::Dot),
        Some("arrow") => Some(Mark::Arrow),
        _ => None,
    };
    // Value bars read their fraction straight from the element.
    if let Some(element) = element {
        let attr = |name: &str| -> Option<f32> {
            element
                .attributes
                .get(name)
                .and_then(|value| value.parse().ok())
        };
        let fraction = match element.tag_name.as_str() {
            "progress" => {
                Some(attr("value").unwrap_or(0.0) / attr("max").unwrap_or(1.0).max(f32::EPSILON))
            }
            "meter" => {
                Some(attr("value").unwrap_or(0.0) / attr("max").unwrap_or(1.0).max(f32::EPSILON))
            }
            "input" if element.attributes.get("type") == Some("range") => {
                let min = attr("min").unwrap_or(0.0);
                let max = attr("max").unwrap_or(100.0);
                Some(
                    (attr("value").unwrap_or((min + max) / 2.0) - min)
                        / (max - min).max(f32::EPSILON),
                )
            }
            _ => None,
        };
        if let Some(fraction) = fraction {
            style.mark = Some(Mark::Fraction(FractionMark {
                fraction: fraction.clamp(0.0, 1.0),
                thumb: element.tag_name == "input",
            }));
        }
        // Color inputs show their value as the swatch background.
        if element.tag_name == "input"
            && element.attributes.get("type") == Some("color")
            && let Some(color) = element.attributes.get("value").and_then(Color::parse)
        {
            style.background_color = Some(color);
        }
    }

    style.visible = !matches!(
        raw.get("visibility").and_then(CssValue::as_keyword),
        Some("hidden" | "collapse")
    );

    // box-shadow: comma-separated "[inset] x y [blur] [spread] color".
    style.box_shadows = raw
        .get("box-shadow")
        .map(|value| {
            let text = value.raw_text();
            text.split(',')
                .filter_map(|shadow| {
                    let mut lengths: Vec<f32> = Vec::new();
                    let mut color = None;
                    let mut inset = false;
                    for piece in shadow.split_whitespace() {
                        if piece == "inset" {
                            inset = true;
                            continue;
                        }
                        match CssValue::parse_component(piece)? {
                            CssValue::Length(px, lumen_css::Unit::Px) => lengths.push(px),
                            CssValue::Length(em, lumen_css::Unit::Em) => {
                                lengths.push(em * style.font_size);
                            }
                            CssValue::Number(number) => lengths.push(number),
                            CssValue::Color(parsed) => color = Some(parsed),
                            CssValue::Keyword(keyword) => color = Color::parse(&keyword),
                            _ => return None,
                        }
                    }
                    (lengths.len() >= 2).then(|| BoxShadow {
                        offset_x: lengths[0],
                        offset_y: lengths[1],
                        blur: lengths.get(2).copied().unwrap_or(0.0).max(0.0),
                        spread: lengths.get(3).copied().unwrap_or(0.0),
                        color: color.unwrap_or(Color::rgba(0, 0, 0, 100)),
                        inset,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    // text-shadow: comma-separated "x y [blur] color".
    style.text_shadows = raw
        .get("text-shadow")
        .map(|value| {
            let text = value.raw_text();
            text.split(',')
                .filter_map(|shadow| {
                    let mut lengths: Vec<f32> = Vec::new();
                    let mut color = None;
                    for piece in shadow.split_whitespace() {
                        match CssValue::parse_component(piece)? {
                            CssValue::Length(px, lumen_css::Unit::Px) => lengths.push(px),
                            CssValue::Length(em, lumen_css::Unit::Em) => {
                                lengths.push(em * style.font_size);
                            }
                            CssValue::Number(number) => lengths.push(number),
                            CssValue::Color(parsed) => color = Some(parsed),
                            CssValue::Keyword(keyword) => color = Color::parse(&keyword),
                            _ => return None,
                        }
                    }
                    (lengths.len() >= 2).then(|| TextShadow {
                        offset_x: lengths[0],
                        offset_y: lengths[1],
                        blur: lengths.get(2).copied().unwrap_or(0.0).max(0.0),
                        color: color.unwrap_or(Color::rgba(0, 0, 0, 128)),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    style.outline_width = raw
        .get("outline-width")
        .and_then(|value| Dimension::from_value(value, style.font_size))
        .and_then(|dimension| dimension.resolve(0.0, crate::geometry::Size::default()))
        .unwrap_or(0.0);
    style.outline_color = match raw.get("outline-color") {
        Some(CssValue::Color(color)) => Some(*color),
        _ => None,
    };
    style.outline_style = match raw.get("outline-style").and_then(CssValue::as_keyword) {
        Some("solid" | "auto") => BorderStyle::Solid,
        Some("dashed") => BorderStyle::Dashed,
        Some("dotted") => BorderStyle::Dotted,
        _ => {
            if style.outline_width > 0.0 && raw.contains_key("outline-width") {
                BorderStyle::Solid
            } else {
                BorderStyle::None
            }
        }
    };

    style.monospace = matches!(
        raw.get("font-family").and_then(CssValue::as_keyword),
        Some("monospace")
    );

    // `auto` parses as CssValue::Auto (not a keyword), so match raw text.
    style.overflow = match raw.get("overflow").map(CssValue::raw_text).as_deref() {
        Some("hidden" | "clip") => Overflow::Hidden,
        Some("scroll" | "auto") => Overflow::Scroll,
        _ => Overflow::Visible,
    };

    style.white_space = match raw.get("white-space").and_then(CssValue::as_keyword) {
        Some("pre" | "pre-wrap" | "pre-line") => WhiteSpace::Pre,
        Some("nowrap") => WhiteSpace::Nowrap,
        _ => WhiteSpace::Normal,
    };

    style.vertical_align = match raw.get("vertical-align").and_then(CssValue::as_keyword) {
        Some("top" | "text-top") => VerticalAlign::Top,
        Some("middle") => VerticalAlign::Middle,
        Some("bottom" | "text-bottom") => VerticalAlign::Bottom,
        Some("sub") => VerticalAlign::Sub,
        Some("super") => VerticalAlign::Super,
        _ => VerticalAlign::Baseline,
    };

    style.text_transform = match raw.get("text-transform").and_then(CssValue::as_keyword) {
        Some("uppercase") => TextTransform::Uppercase,
        Some("lowercase") => TextTransform::Lowercase,
        Some("capitalize") => TextTransform::Capitalize,
        _ => TextTransform::None,
    };

    style.text_overflow_ellipsis = matches!(
        raw.get("text-overflow").and_then(CssValue::as_keyword),
        Some("ellipsis")
    );
    style.break_words = matches!(
        raw.get("word-break").and_then(CssValue::as_keyword),
        Some("break-all" | "break-word")
    ) || matches!(
        raw.get("overflow-wrap")
            .or_else(|| raw.get("word-wrap"))
            .and_then(CssValue::as_keyword),
        Some("break-word" | "anywhere")
    );

    style.letter_spacing = raw
        .get("letter-spacing")
        .and_then(|value| Dimension::from_value(value, style.font_size))
        .and_then(|dimension| dimension.resolve(0.0, crate::geometry::Size::default()))
        .unwrap_or(0.0);
    style.word_spacing = raw
        .get("word-spacing")
        .and_then(|value| Dimension::from_value(value, style.font_size))
        .and_then(|dimension| dimension.resolve(0.0, crate::geometry::Size::default()))
        .unwrap_or(0.0);
    style.text_indent = raw
        .get("text-indent")
        .and_then(|value| Dimension::from_value(value, style.font_size))
        .and_then(|dimension| dimension.resolve(0.0, crate::geometry::Size::default()))
        .unwrap_or(0.0);

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
            "justify" => Some(TextAlign::Justify),
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
        .map(|dimension| match dimension {
            // Negative sizes are invalid per CSS; clamp them to zero.
            Dimension::Px(value) => Dimension::Px(value.max(0.0)),
            other => other,
        })
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
    fn overflow_auto_clips_and_scrolls() {
        // `auto` parses as CssValue::Auto, not a keyword — the overflow
        // match must still see it.
        let (document, styles) =
            styles_for("<style>div { overflow: auto; }</style><body><div>x</div></body>");
        let div = style_of(&document, &styles, "div");
        assert_eq!(div.overflow, Overflow::Scroll);
        assert!(div.overflow.clips());
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
    fn of_type_and_is_and_word_attributes_match() {
        let (document, styles) = styles_for(
            "<style>p:first-of-type { color: #ff0000; }\
                    span:nth-of-type(2) { color: #00ff00; }\
                    :is(.x, .y) { font-weight: 700; }\
                    a[rel~=nofollow] { color: #0000ff; }\
                    div[lang|=en] { color: #ff00ff; }</style>\
             <div lang='en-US'><h1>t</h1><p>first p</p><span>s1</span>\
             <span class='y'>s2</span><a rel='external nofollow'>l</a></div>",
        );
        let find = |tag: &str, nth: usize| {
            document
                .descendants(document.root())
                .filter(|id| document.element(*id).is_some_and(|e| e.tag_name == tag))
                .nth(nth)
                .unwrap()
        };
        // p is not the first element child, but IS the first p.
        assert_eq!(styles.by_node[&find("p", 0)].color.to_string(), "#ff0000");
        assert_eq!(
            styles.by_node[&find("span", 1)].color.to_string(),
            "#00ff00"
        );
        assert_eq!(styles.by_node[&find("span", 1)].font_weight.0, 700);
        assert_eq!(styles.by_node[&find("a", 0)].color.to_string(), "#0000ff");
        assert_eq!(styles.by_node[&find("div", 0)].color.to_string(), "#ff00ff");
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
    fn linear_gradient_parses_directions_and_stops() {
        let (document, styles) = styles_for(
            "<style>div { background-image: linear-gradient(to right, #ff0000, #0000ff); }\
                    p { background: linear-gradient(45deg, #000000 20%, #ffffff 80%); }</style>\
             <div>x</div><p>y</p>",
        );
        let BackgroundImage::LinearGradient(gradient) =
            &style_of(&document, &styles, "div").background_layers[0].image
        else {
            panic!("expected a gradient");
        };
        assert_eq!(gradient.angle_degrees, 90.0);
        assert_eq!(gradient.stops[0], (Color::rgb(0xff, 0, 0), 0.0));
        assert_eq!(gradient.stops[1], (Color::rgb(0, 0, 0xff), 1.0));
        let BackgroundImage::LinearGradient(gradient) =
            &style_of(&document, &styles, "p").background_layers[0].image
        else {
            panic!("expected a gradient from the shorthand");
        };
        assert_eq!(gradient.angle_degrees, 45.0);
        assert_eq!(gradient.stops[0].1, 0.2);
        assert_eq!(gradient.stops[1].1, 0.8);
    }

    #[test]
    fn background_placement_properties_parse() {
        let (document, styles) = styles_for(
            "<style>div { background-image: url(a.png); background-position: right center; \
                          background-size: cover; background-repeat: no-repeat; }\
                    p { background-image: url(b.png); background-position: 10px 20px; \
                        background-size: 50px auto; }\
             </style><div>x</div><p>y</p>",
        );
        let div = &style_of(&document, &styles, "div").background_layers[0];
        assert_eq!(
            div.position,
            (Dimension::Percent(100.0), Dimension::Percent(50.0))
        );
        assert_eq!(div.size, BackgroundSize::Cover);
        assert_eq!(div.repeat, (false, false));
        let p = &style_of(&document, &styles, "p").background_layers[0];
        assert_eq!(p.position, (Dimension::Px(10.0), Dimension::Px(20.0)));
        assert_eq!(
            p.size,
            BackgroundSize::Explicit(Dimension::Px(50.0), Dimension::Auto)
        );
    }

    #[test]
    fn background_layers_split_and_cycle() {
        let (document, styles) = styles_for(
            "<style>div { background-image: linear-gradient(red, blue), url(tile.png); \
                          background-repeat: no-repeat, repeat-x; \
                          background-position: center; }</style><div>x</div>",
        );
        let layers = &style_of(&document, &styles, "div").background_layers;
        assert_eq!(layers.len(), 2);
        assert!(matches!(
            layers[0].image,
            BackgroundImage::LinearGradient(_)
        ));
        assert_eq!(layers[0].repeat, (false, false));
        assert_eq!(
            layers[1].image,
            BackgroundImage::Url("tile.png".to_string())
        );
        assert_eq!(layers[1].repeat, (true, false));
        // The single position cycles onto both layers.
        assert_eq!(layers[1].position.0, Dimension::Percent(50.0));
    }

    #[test]
    fn radial_gradient_parses_with_prelude() {
        let (document, styles) = styles_for(
            "<style>div { background-image: radial-gradient(circle at center, #ff0000, #0000ff); }\
             </style><div>x</div>",
        );
        let BackgroundImage::RadialGradient(stops) =
            &style_of(&document, &styles, "div").background_layers[0].image
        else {
            panic!("expected radial gradient");
        };
        assert_eq!(stops.len(), 2);
        assert_eq!(stops[0].0, Color::rgb(0xff, 0, 0));
    }

    #[test]
    fn background_image_url_survives_to_computed_style() {
        let (document, styles) =
            styles_for("<style>div { background-image: url('bg.png'); }</style><div>x</div>");
        assert_eq!(
            style_of(&document, &styles, "div").background_layers[0].image,
            BackgroundImage::Url("bg.png".to_string())
        );
    }

    #[test]
    fn custom_properties_substitute_and_inherit() {
        let (document, styles) = styles_for(
            "<style>:root { --brand: #ff0000; --pad: 4px 8px; }\
                    div { color: var(--brand); padding: var(--pad); }\
                    p { color: var(--missing, #0000ff); }</style>\
             <div><p>x</p></div>",
        );
        let div = style_of(&document, &styles, "div");
        assert_eq!(div.color.to_string(), "#ff0000");
        // The shorthand expands after substitution.
        assert_eq!(div.padding.top, Dimension::Px(4.0));
        assert_eq!(div.padding.right, Dimension::Px(8.0));
        // Fallback used when the variable is missing.
        assert_eq!(
            style_of(&document, &styles, "p").color.to_string(),
            "#0000ff"
        );
    }

    #[test]
    fn calc_evaluates_homogeneous_expressions() {
        let (document, styles) = styles_for(
            "<html><head><style>html { font-size: 10px; }\
                    div { width: calc(200px + 2 * 50px); height: calc(10rem - 2rem); \
                          margin-top: calc(100% / 4); padding-top: calc(100% - 20px); }\
             </style></head><body><div>x</div></body></html>",
        );
        let div = style_of(&document, &styles, "div");
        assert_eq!(div.width, Dimension::Px(300.0));
        assert_eq!(div.height, Dimension::Px(80.0));
        assert_eq!(div.margin.top, Dimension::Percent(25.0));
        // Mixed % and px cannot evaluate: the declaration drops.
        assert_eq!(div.padding.top, Dimension::Px(0.0));
    }

    #[test]
    fn min_max_clamp_evaluate() {
        let (document, styles) = styles_for(
            "<style>div { width: min(300px, 12.5em); height: max(40px, 60px); \
                          margin-top: clamp(10px, 25px, 20px); \
                          padding-top: calc(min(100px, 200px) * 2); \
                          margin-bottom: min(50%, 100px); }</style><div>x</div>",
        );
        let div = style_of(&document, &styles, "div");
        assert_eq!(div.width, Dimension::Px(200.0)); // 12.5em = 200px
        assert_eq!(div.height, Dimension::Px(60.0));
        assert_eq!(div.margin.top, Dimension::Px(20.0)); // clamped to max
        assert_eq!(div.padding.top, Dimension::Px(200.0));
        // Mixed % and px families cannot evaluate: declaration drops.
        assert_eq!(div.margin.bottom, Dimension::Px(0.0));
    }

    #[test]
    fn var_inside_calc_resolves() {
        let (document, styles) = styles_for(
            "<style>:root { --base: 100px; } div { width: calc(var(--base) * 3); }</style>\
             <div>x</div>",
        );
        assert_eq!(
            style_of(&document, &styles, "div").width,
            Dimension::Px(300.0)
        );
    }

    #[test]
    fn hover_impact_classifies_stylesheets() {
        let none = lumen_css::parse_stylesheet("p { color: red; } .x { padding: 4px; }");
        assert_eq!(hover_impact(&none), HoverImpact::Nothing);
        let paint = lumen_css::parse_stylesheet(
            "a:hover { color: red; background-color: #eee; border-color: blue; \
                       text-decoration: underline; opacity: 0.8; }",
        );
        assert_eq!(hover_impact(&paint), HoverImpact::PaintOnly);
        let layout = lumen_css::parse_stylesheet("a:hover { padding: 2px; }");
        assert_eq!(hover_impact(&layout), HoverImpact::Layout);
        let hidden_hover = lumen_css::parse_stylesheet(".x:not(:hover) { font-size: 2em; }");
        assert_eq!(hover_impact(&hidden_hover), HoverImpact::Layout);
        let border_width = lumen_css::parse_stylesheet("a:hover { border-top-width: 3px; }");
        assert_eq!(hover_impact(&border_width), HoverImpact::Layout);
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
    fn typography_properties_parse_and_inherit() {
        let (document, styles) = styles_for(
            "<style>div { text-transform: uppercase; letter-spacing: 2px; \
                          word-spacing: 4px; text-indent: 24px; white-space: nowrap; }\
                    s { text-decoration: line-through; }</style>\
             <div><p>t</p></div><p><s>struck</s></p>",
        );
        let inner = style_of(&document, &styles, "p");
        assert_eq!(inner.text_transform, TextTransform::Uppercase);
        assert_eq!(inner.letter_spacing, 2.0);
        assert_eq!(inner.word_spacing, 4.0);
        assert_eq!(inner.text_indent, 24.0);
        assert_eq!(inner.white_space, WhiteSpace::Nowrap);
        assert!(style_of(&document, &styles, "s").line_through);
    }

    #[test]
    fn decoration_shorthand_carries_style_and_color() {
        let (document, styles) = styles_for(
            "<style>p { text-decoration: underline dotted #ff0000; }\
                    s { text-decoration-color: #00ff00; text-decoration-style: dashed; \
                        text-decoration: line-through; }</style><p>u</p><p><s>s</s></p>",
        );
        let p = style_of(&document, &styles, "p");
        assert!(p.underline);
        assert_eq!(p.text_decoration_style, BorderStyle::Dotted);
        assert_eq!(p.text_decoration_color, Some(Color::rgb(0xff, 0, 0)));
        let strike = style_of(&document, &styles, "s");
        assert!(strike.line_through);
        assert_eq!(strike.text_decoration_style, BorderStyle::Dashed);
        assert_eq!(strike.text_decoration_color, Some(Color::rgb(0, 0xff, 0)));
    }

    #[test]
    fn font_shorthand_expands() {
        let (document, styles) =
            styles_for("<style>p { font: italic bold 20px/2 Menlo, monospace; }</style><p>x</p>");
        let p = style_of(&document, &styles, "p");
        assert!(p.italic);
        assert_eq!(p.font_weight.0, 700);
        assert_eq!(p.font_size, 20.0);
        assert_eq!(p.line_height, 40.0);
        assert!(p.monospace);
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

    #[test]
    fn font_size_is_clamped_to_a_sane_range() {
        let (document, styles) =
            styles_for("<style>div { font-size: 99999px; }</style><div>t</div>");
        assert_eq!(style_of(&document, &styles, "div").font_size, 512.0);
        let (document, styles) = styles_for("<style>div { font-size: -5px; }</style><div>t</div>");
        assert_eq!(style_of(&document, &styles, "div").font_size, 0.0);
    }

    #[test]
    fn line_height_percent_resolves_against_font_size() {
        let (document, styles) =
            styles_for("<style>div { font-size: 20px; line-height: 150%; }</style><div>t</div>");
        assert_eq!(style_of(&document, &styles, "div").line_height, 30.0);
    }

    #[test]
    fn negative_dimensions_clamp_to_zero() {
        let (document, styles) =
            styles_for("<style>div { width: -50px; height: -10px; }</style><div>t</div>");
        let style = style_of(&document, &styles, "div");
        assert_eq!(style.width, Dimension::Px(0.0));
        assert_eq!(style.height, Dimension::Px(0.0));
    }

    #[test]
    fn absurdly_deep_nesting_stops_at_the_depth_cap() {
        // Big-stack thread: see the layout depth test for why.
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(|| {
                let mut html = String::new();
                for _ in 0..crate::MAX_DEPTH * 4 {
                    html.push_str("<div>");
                }
                let (document, styles) = styles_for(&html);
                // The walk completed without overflowing the stack; nodes
                // past the cap carry no computed style.
                let styled = document
                    .descendants(document.root())
                    .filter(|id| styles.by_node.contains_key(id))
                    .count();
                assert!(styled <= crate::MAX_DEPTH + 2, "styled {styled} nodes");
            })
            .expect("spawn")
            .join()
            .expect("no stack overflow");
    }

    /// Builds the wide-DOM fixture: 100 sections × 20 items (2000
    /// elements) and a 50-rule stylesheet mixing tag/class selectors,
    /// child and descendant combinators, and the sibling/nth/of-type
    /// pseudo-class families — the selectors whose matching used to
    /// rebuild sibling vectors and recompute specificity per element.
    fn wide_fixture() -> (Document, StyleMap) {
        use std::fmt::Write as _;
        let mut css = String::new();
        // Tag + class rules for every item class.
        for i in 0..5 {
            let _ = writeln!(css, ".c{i} {{ color: #11111{i}; }}");
            let _ = writeln!(css, "div.c{i} {{ margin-top: {i}px; }}");
        }
        // Sibling and nth machinery, exercised per rule × element.
        css.push_str(
            "section > div:first-child { color: #010203; }
             section > div:last-child { color: #030201; }
             div:nth-child(2n) { font-weight: 700; }
             div:nth-child(3n+1) { text-align: right; }
             div:nth-last-child(1) { font-style: italic; }
             div:first-of-type { line-height: 21px; }
             div:last-of-type { line-height: 22px; }
             div:nth-of-type(2n+1) { letter-spacing: 1px; }
             div:only-child { display: block; }
             section div + div { border-top-width: 1px; }
             section div ~ div { padding-top: 2px; }
             section > .c0 { width: 10px; }
             section .c1 { width: 11px; }
             div:not(.c4) { min-height: 1px; }
             div:is(.c0, .c2) { max-width: 50px; }
             div:where(.c3) { max-height: 51px; }",
        );
        // Pad out to 50 rules with descendant-combinator rules.
        for i in 0..24 {
            let _ = writeln!(
                css,
                "section div.c{} {{ border-left-width: {}px; }}",
                i % 5,
                i % 3
            );
        }
        let mut body = String::from("<body>");
        for section in 0..100 {
            let _ = write!(body, "<section id='s{section}'>");
            for item in 0..20 {
                let _ = write!(body, "<div class='c{}'>x</div>", (section + item) % 5);
            }
            body.push_str("</section>");
        }
        body.push_str("</body>");
        let html = format!("<style>{css}</style>{body}");
        let document = parse_document(&html);
        let author = lumen_css::parse_stylesheet(&css);
        // Sanity: the fixture really is 50 rules × 2000 item elements.
        assert_eq!(author.rules.len(), 50);
        let styles = compute_styles(&document, &author);
        (document, styles)
    }

    fn item_style<'a>(
        document: &Document,
        styles: &'a StyleMap,
        section: usize,
        item: usize,
    ) -> &'a ComputedStyle {
        let section_id = document
            .descendants(document.root())
            .find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.id() == Some(format!("s{section}").as_str()))
            })
            .unwrap_or_else(|| panic!("no section s{section}"));
        let items: Vec<NodeId> = document
            .children(section_id)
            .iter()
            .copied()
            .filter(|id| document.element(*id).is_some())
            .collect();
        assert_eq!(items.len(), 20);
        &styles.by_node[&items[item]]
    }

    #[test]
    fn wide_dom_matching_stays_correct() {
        // Correctness of the shared sibling cache and the precomputed
        // specificity table on a 2000-element × 50-rule tree: nth/of-type
        // and combinator results must match the plain cascade semantics.
        let (document, styles) = wide_fixture();

        // :first-child / :last-child colors.
        assert_eq!(
            item_style(&document, &styles, 7, 0).color,
            Color::rgb(0x01, 0x02, 0x03)
        );
        assert_eq!(
            item_style(&document, &styles, 7, 19).color,
            Color::rgb(0x03, 0x02, 0x01)
        );
        // :nth-child(2n) → bold on even 1-based positions (odd 0-based).
        assert_eq!(
            item_style(&document, &styles, 3, 1).font_weight,
            FontWeight(700)
        );
        assert_eq!(
            item_style(&document, &styles, 3, 2).font_weight,
            FontWeight(400)
        );
        // :nth-of-type(2n+1) → letter spacing on odd 1-based positions.
        assert_eq!(item_style(&document, &styles, 5, 0).letter_spacing, 1.0);
        assert_eq!(item_style(&document, &styles, 5, 1).letter_spacing, 0.0);
        // `div + div` skips the first child; `div ~ div` covers the rest.
        assert_eq!(item_style(&document, &styles, 9, 0).border_width.top, 0.0);
        assert_eq!(item_style(&document, &styles, 9, 4).border_width.top, 1.0);
        assert_eq!(
            item_style(&document, &styles, 9, 0).padding.top,
            Dimension::Px(0.0)
        );
        assert_eq!(
            item_style(&document, &styles, 9, 8).padding.top,
            Dimension::Px(2.0)
        );
        // :not() and :is() specificity/matching.
        let c4_item = item_style(&document, &styles, 0, 4); // class c4 at (0,4)
        assert_eq!(c4_item.min_height, Dimension::Auto);
        assert_eq!(
            item_style(&document, &styles, 0, 0).min_height,
            Dimension::Px(1.0)
        );
        assert_eq!(
            item_style(&document, &styles, 0, 0).max_width,
            Dimension::Px(50.0)
        );
    }

    #[test]
    fn wide_flat_sibling_lists_do_not_rebuild_per_check() {
        // One parent with 2000 element children, several nth/of-type
        // rules: the old matcher rebuilt the 2000-entry sibling Vec per
        // pseudo-class per rule (O(n²) allocations); with the shared
        // cache this completes with the list built once. Correctness is
        // asserted per position.
        let css = "div:nth-child(2n) { font-weight: 700; }
                   div:first-child { color: #010203; }
                   div:last-child { color: #030201; }
                   div:nth-of-type(4n) { letter-spacing: 2px; }
                   div + div { margin-top: 3px; }";
        let mut body = String::from("<body><section>");
        for _ in 0..2000 {
            body.push_str("<div>x</div>");
        }
        body.push_str("</section></body>");
        let html = format!("<style>{css}</style>{body}");
        let document = parse_document(&html);
        let author = lumen_css::parse_stylesheet(css);
        let styles = compute_styles(&document, &author);
        let items: Vec<NodeId> = document
            .descendants(document.root())
            .filter(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "div")
            })
            .collect();
        assert_eq!(items.len(), 2000);
        assert_eq!(
            styles.by_node[&items[0]].color,
            Color::rgb(0x01, 0x02, 0x03)
        );
        assert_eq!(
            styles.by_node[&items[1999]].color,
            Color::rgb(0x03, 0x02, 0x01)
        );
        assert_eq!(styles.by_node[&items[1]].font_weight, FontWeight(700));
        assert_eq!(styles.by_node[&items[2]].font_weight, FontWeight(400));
        assert_eq!(styles.by_node[&items[3]].letter_spacing, 2.0);
        assert_eq!(styles.by_node[&items[0]].margin.top, Dimension::Px(0.0));
        assert_eq!(styles.by_node[&items[1]].margin.top, Dimension::Px(3.0));
    }
}
