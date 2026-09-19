//! Stylesheet parsing: rules, declarations and shorthand expansion.
//!
//! Tokenization and block structure come from the lightningcss parse stack:
//! `cssparser` walks rule lists and declaration blocks (so strings, `url()`
//! arguments and comments can no longer desynchronize the parser), and
//! lightningcss proper parses media query preludes. The results are mapped
//! onto the engine's own model types — consumers never see lightningcss
//! types, and declaration values keep their raw source text (data URIs and
//! quoted strings survive byte-for-byte).

use crate::selector::{Selector, parse_selector_list};
use crate::value::{CssValue, split_components};
use cssparser::{
    AtRuleParser, CowRcStr, DeclarationParser, ParseError, Parser, ParserInput, ParserState,
    QualifiedRuleParser, RuleBodyItemParser, RuleBodyParser, StyleSheetParser,
};
use lightningcss::media_query::{
    MediaCondition, MediaFeatureComparison, MediaFeatureId, MediaFeatureName, MediaFeatureValue,
    MediaList, MediaType, Operator, Qualifier, QueryFeature,
};
use lightningcss::stylesheet::ParserOptions;
use lightningcss::values::length::{Length, LengthValue};

/// A single longhand `name: value` pair. Shorthands are expanded at parse
/// time, so consumers never see `margin`, only `margin-top` etc.
#[derive(Debug, Clone, PartialEq)]
pub struct Declaration {
    /// Lowercase property name.
    pub name: String,
    pub value: CssValue,
    /// Declared with `!important`.
    pub important: bool,
}

/// One rule: a selector list and its declarations.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub selectors: Vec<Selector>,
    pub declarations: Vec<Declaration>,
    /// Position of the rule in the stylesheet, for cascade tie-breaking.
    pub source_order: usize,
    /// The enclosing `@media` condition, when any.
    pub media: Option<MediaQuery>,
}

/// A supported media query: width bounds in px, `and`-combined.
/// Unsupported queries never reach here — their blocks are skipped.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MediaQuery {
    pub min_width: Option<f32>,
    pub max_width: Option<f32>,
    /// Strict range bounds (`width > 600px`): the bound itself is
    /// excluded, unlike the inclusive `min-width` form.
    pub min_exclusive: bool,
    pub max_exclusive: bool,
}

impl MediaQuery {
    #[must_use]
    pub fn matches(&self, viewport_width: f32) -> bool {
        let above_min = match self.min_width {
            Some(min) if self.min_exclusive => viewport_width > min,
            Some(min) => viewport_width >= min,
            None => true,
        };
        let below_max = match self.max_width {
            Some(max) if self.max_exclusive => viewport_width < max,
            Some(max) => viewport_width <= max,
            None => true,
        };
        above_min && below_max
    }
}

/// Maps the lightningcss media query list of one `@media` prelude onto our
/// model: a single query of type `screen`/`all` (or none) whose `and`-
/// combined conditions only constrain width (`em`/`rem` count as 16px).
/// Anything richer — comma lists, `not`/`or`, print, equality — is
/// unsupported: `None`, skip the block.
fn parse_media_query(input: &mut Parser) -> Option<MediaQuery> {
    let list = MediaList::parse(input, &ParserOptions::default()).ok()?;
    let [query] = &list.media_queries[..] else {
        return None;
    };
    if matches!(query.qualifier, Some(Qualifier::Not)) {
        return None;
    }
    match &query.media_type {
        MediaType::All | MediaType::Screen => {}
        _ => return None,
    }
    let mut result = MediaQuery {
        min_width: None,
        max_width: None,
        min_exclusive: false,
        max_exclusive: false,
    };
    if let Some(condition) = &query.condition {
        apply_media_condition(condition, &mut result)?;
    }
    Some(result)
}

fn apply_media_condition(condition: &MediaCondition, query: &mut MediaQuery) -> Option<()> {
    match condition {
        MediaCondition::Feature(feature) => apply_media_feature(feature, query),
        MediaCondition::Operation {
            operator: Operator::And,
            conditions,
        } => {
            for condition in conditions {
                apply_media_condition(condition, query)?;
            }
            Some(())
        }
        // `not`, `or` and unknown conditions are unsupported.
        _ => None,
    }
}

fn apply_media_feature(
    feature: &QueryFeature<MediaFeatureId>,
    query: &mut MediaQuery,
) -> Option<()> {
    match feature {
        // `(min-width: N)` arrives as a range with a legacy operator;
        // strict forms (`width > N`) keep their exclusive bound.
        QueryFeature::Range {
            name: MediaFeatureName::Standard(MediaFeatureId::Width),
            operator,
            value,
        } => {
            let pixels = media_value_px(value)?;
            match operator {
                MediaFeatureComparison::GreaterThanEqual => tighten_min(query, pixels, false),
                MediaFeatureComparison::GreaterThan => tighten_min(query, pixels, true),
                MediaFeatureComparison::LessThanEqual => tighten_max(query, pixels, false),
                MediaFeatureComparison::LessThan => tighten_max(query, pixels, true),
                // Equality doesn't fit the bound model.
                _ => return None,
            }
            Some(())
        }
        // `(400px <= width <= 900px)`, bounds inclusive or strict.
        QueryFeature::Interval {
            name: MediaFeatureName::Standard(MediaFeatureId::Width),
            start,
            start_operator,
            end,
            end_operator,
        } => {
            let min_exclusive = match start_operator {
                MediaFeatureComparison::LessThan => true,
                MediaFeatureComparison::LessThanEqual => false,
                _ => return None,
            };
            let max_exclusive = match end_operator {
                MediaFeatureComparison::LessThan => true,
                MediaFeatureComparison::LessThanEqual => false,
                _ => return None,
            };
            let min = media_value_px(start)?;
            let max = media_value_px(end)?;
            tighten_min(query, min, min_exclusive);
            tighten_max(query, max, max_exclusive);
            Some(())
        }
        _ => None,
    }
}

/// Narrows the min bound to the larger candidate; on ties the exclusive
/// (strict) form wins, as it is the tighter constraint.
fn tighten_min(query: &mut MediaQuery, pixels: f32, exclusive: bool) {
    match query.min_width {
        Some(current) if current > pixels => {}
        Some(current) if current == pixels => query.min_exclusive |= exclusive,
        _ => {
            query.min_width = Some(pixels);
            query.min_exclusive = exclusive;
        }
    }
}

/// Narrows the max bound to the smaller candidate; on ties the exclusive
/// (strict) form wins, as it is the tighter constraint.
fn tighten_max(query: &mut MediaQuery, pixels: f32, exclusive: bool) {
    match query.max_width {
        Some(current) if current < pixels => {}
        Some(current) if current == pixels => query.max_exclusive |= exclusive,
        _ => {
            query.max_width = Some(pixels);
            query.max_exclusive = exclusive;
        }
    }
}

/// A media feature length in px; `em`/`rem` count as 16px, like before.
fn media_value_px(value: &MediaFeatureValue) -> Option<f32> {
    let MediaFeatureValue::Length(length) = value else {
        return None;
    };
    if let Some(px) = length.to_px() {
        return Some(px);
    }
    match length {
        Length::Value(LengthValue::Em(value) | LengthValue::Rem(value)) => Some(value * 16.0),
        _ => None,
    }
}

/// One `@font-face` block: a family name and its source URLs in order.
#[derive(Debug, Clone, PartialEq)]
pub struct FontFace {
    pub family: String,
    /// (url, format hint if any) pairs in source order.
    pub sources: Vec<(String, Option<String>)>,
}

/// One `@keyframes` block: a name plus (offset 0..=1, declarations)
/// frames sorted by offset.
#[derive(Debug, Clone, PartialEq)]
pub struct Keyframes {
    pub name: String,
    pub frames: Vec<(f32, Vec<Declaration>)>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Stylesheet {
    /// Arc-shared so `for_width` can hand out a viewport-filtered sheet
    /// without deep-cloning every rule when nothing (or only a little)
    /// is filtered out.
    pub rules: std::sync::Arc<Vec<Rule>>,
    pub font_faces: std::sync::Arc<Vec<FontFace>>,
    pub keyframes: std::sync::Arc<Vec<Keyframes>>,
}

impl Stylesheet {
    /// The rules that apply at a viewport width: everything outside
    /// `@media`, plus matching media blocks.
    ///
    /// When every rule applies (the common case: no media queries, or a
    /// viewport that matches them all) this is three `Arc` bumps instead
    /// of a full deep clone; otherwise only the surviving rules clone.
    #[must_use]
    pub fn for_width(&self, viewport_width: f32) -> Stylesheet {
        let applies = |rule: &Rule| {
            rule.media
                .as_ref()
                .is_none_or(|media| media.matches(viewport_width))
        };
        if self.rules.iter().all(applies) {
            return self.clone();
        }
        Stylesheet {
            rules: std::sync::Arc::new(
                self.rules
                    .iter()
                    .filter(|rule| applies(rule))
                    .cloned()
                    .collect(),
            ),
            font_faces: self.font_faces.clone(),
            keyframes: self.keyframes.clone(),
        }
    }

    /// The `@keyframes` block for an animation name, if declared.
    #[must_use]
    pub fn keyframes(&self, name: &str) -> Option<&Keyframes> {
        self.keyframes.iter().rev().find(|block| block.name == name)
    }
}

/// Parses a stylesheet.
///
/// Lenient where browsers are lenient, and therefore infallible: comments
/// are skipped, malformed declarations and unsupported selectors are
/// dropped (a rule whose selector list contains any invalid selector is
/// dropped entirely), and an unterminated `{` block is closed at end of
/// input with its declarations kept.
#[must_use]
pub fn parse_stylesheet(source: &str) -> Stylesheet {
    let mut sheet = Stylesheet::default();
    let mut source_order = 0;
    let mut input = ParserInput::new(source);
    let mut input = Parser::new(&mut input);
    {
        let mut parser = SheetParser {
            sheet: &mut sheet,
            media: None,
            source_order: &mut source_order,
            depth: 0,
        };
        for _ in StyleSheetParser::new(&mut input, &mut parser) {}
    }
    sheet
}

/// How deep `@media` blocks may nest; deeper blocks are dropped.
const MAX_MEDIA_NESTING: usize = 32;

/// Parser state for one rule list: the top level, or the body of one
/// `@media` block (nested media conditions intersect).
struct SheetParser<'a> {
    sheet: &'a mut Stylesheet,
    media: Option<MediaQuery>,
    source_order: &'a mut usize,
    depth: usize,
}

/// What a supported at-rule prelude resolves to; anything else is skipped
/// (cssparser then skips its block with correct, string-aware brace
/// matching, so nested rules cannot desynchronize the parse).
enum AtRulePrelude {
    /// `@media` with its condition already mapped (`None`: skip the block).
    Media(Option<MediaQuery>),
    FontFace,
    Keyframes(String),
}

impl<'i> AtRuleParser<'i> for SheetParser<'_> {
    type Prelude = AtRulePrelude;
    type AtRule = ();
    type Error = ();

    fn parse_prelude<'t>(
        &mut self,
        name: CowRcStr<'i>,
        input: &mut Parser<'i, 't>,
    ) -> Result<AtRulePrelude, ParseError<'i, ()>> {
        if name.eq_ignore_ascii_case("media") {
            return Ok(AtRulePrelude::Media(parse_media_query(input)));
        }
        if name.eq_ignore_ascii_case("font-face") {
            return Ok(AtRulePrelude::FontFace);
        }
        if name.eq_ignore_ascii_case("keyframes") || name.eq_ignore_ascii_case("-webkit-keyframes")
        {
            let start = input.position();
            while input.next().is_ok() {}
            let name = input.slice_from(start).trim();
            if name.is_empty() {
                return Err(input.new_error_for_next_token());
            }
            return Ok(AtRulePrelude::Keyframes(name.to_string()));
        }
        Err(input.new_error_for_next_token())
    }

    fn parse_block<'t>(
        &mut self,
        prelude: AtRulePrelude,
        _start: &ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<(), ParseError<'i, ()>> {
        match prelude {
            AtRulePrelude::Media(Some(query)) if self.depth < MAX_MEDIA_NESTING => {
                let mut combined = MediaQuery {
                    min_width: None,
                    max_width: None,
                    min_exclusive: false,
                    max_exclusive: false,
                };
                for source in [self.media, Some(query)].into_iter().flatten() {
                    if let Some(min) = source.min_width {
                        tighten_min(&mut combined, min, source.min_exclusive);
                    }
                    if let Some(max) = source.max_width {
                        tighten_max(&mut combined, max, source.max_exclusive);
                    }
                }
                let mut nested = SheetParser {
                    sheet: self.sheet,
                    media: Some(combined),
                    source_order: self.source_order,
                    depth: self.depth + 1,
                };
                for _ in RuleBodyParser::new(input, &mut nested) {}
            }
            // Unsupported condition or too-deep nesting: block skipped.
            AtRulePrelude::Media(_) => {}
            AtRulePrelude::FontFace => {
                if let Some(face) = parse_font_face(input) {
                    std::sync::Arc::make_mut(&mut self.sheet.font_faces).push(face);
                }
            }
            AtRulePrelude::Keyframes(name) => {
                let block = parse_keyframes(name, input);
                if !block.frames.is_empty() {
                    std::sync::Arc::make_mut(&mut self.sheet.keyframes).push(block);
                }
            }
        }
        Ok(())
    }
}

impl<'i> DeclarationParser<'i> for SheetParser<'_> {
    type Declaration = ();
    type Error = ();
    // Declarations directly in a rule list (not inside a style rule) are
    // invalid; the default `parse_value` rejects them.
}

impl<'i> QualifiedRuleParser<'i> for SheetParser<'_> {
    type Prelude = Vec<Selector>;
    type QualifiedRule = ();
    type Error = ();

    fn parse_prelude<'t>(
        &mut self,
        input: &mut Parser<'i, 't>,
    ) -> Result<Vec<Selector>, ParseError<'i, ()>> {
        let start = input.position();
        while input.next().is_ok() {}
        let source = input.slice_from(start);
        let selectors = parse_selector_list(source.trim())
            .filter(|selectors| !selectors.is_empty())
            .ok_or_else(|| input.new_error_for_next_token())?;
        Ok(selectors)
    }

    fn parse_block<'t>(
        &mut self,
        selectors: Vec<Selector>,
        _start: &ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<(), ParseError<'i, ()>> {
        let declarations = collect_declarations(input);
        std::sync::Arc::make_mut(&mut self.sheet.rules).push(Rule {
            selectors,
            declarations,
            source_order: *self.source_order,
            media: self.media,
        });
        *self.source_order += 1;
        Ok(())
    }
}

impl<'i> RuleBodyItemParser<'i, (), ()> for SheetParser<'_> {
    fn parse_declarations(&self) -> bool {
        false
    }
    fn parse_qualified(&self) -> bool {
        true
    }
}

/// Collects the declarations of one block (a style rule body, an inline
/// `style=` attribute, a `@keyframes` frame), running the full value
/// pipeline per declaration.
struct DeclarationCollector {
    declarations: Vec<Declaration>,
}

impl<'i> DeclarationParser<'i> for DeclarationCollector {
    type Declaration = ();
    type Error = ();

    fn parse_value<'t>(
        &mut self,
        name: CowRcStr<'i>,
        input: &mut Parser<'i, 't>,
        _declaration_start: &ParserState,
    ) -> Result<(), ParseError<'i, ()>> {
        // The value keeps its raw source text: the tokenizer has already
        // handled strings, url() and comments, so the slice boundaries are
        // correct and the bytes in between are untouched.
        let start = input.position();
        while input.next().is_ok() {}
        let value = input.slice_from(start);
        push_declaration(
            &name.to_ascii_lowercase(),
            value.trim(),
            &mut self.declarations,
        );
        Ok(())
    }
}

impl<'i> AtRuleParser<'i> for DeclarationCollector {
    type Prelude = ();
    type AtRule = ();
    type Error = ();
}

impl<'i> QualifiedRuleParser<'i> for DeclarationCollector {
    type Prelude = ();
    type QualifiedRule = ();
    type Error = ();
}

impl<'i> RuleBodyItemParser<'i, (), ()> for DeclarationCollector {
    fn parse_declarations(&self) -> bool {
        true
    }
    fn parse_qualified(&self) -> bool {
        false
    }
}

/// Runs the declaration pipeline over one block of raw CSS text.
fn collect_declarations(input: &mut Parser) -> Vec<Declaration> {
    let mut collector = DeclarationCollector {
        declarations: Vec::new(),
    };
    for _ in RuleBodyParser::new(input, &mut collector) {}
    collector.declarations
}

/// Parses the body of a `@keyframes` block: `from`/`to`/percent frame
/// selectors (comma lists share declarations), sorted by offset.
fn parse_keyframes(name: String, input: &mut Parser) -> Keyframes {
    let mut parser = KeyframesParser { frames: Vec::new() };
    for _ in RuleBodyParser::new(input, &mut parser) {}
    parser.frames.sort_by(|a, b| a.0.total_cmp(&b.0));
    Keyframes {
        name,
        frames: parser.frames,
    }
}

struct KeyframesParser {
    frames: Vec<(f32, Vec<Declaration>)>,
}

impl<'i> QualifiedRuleParser<'i> for KeyframesParser {
    type Prelude = Vec<f32>;
    type QualifiedRule = ();
    type Error = ();

    fn parse_prelude<'t>(
        &mut self,
        input: &mut Parser<'i, 't>,
    ) -> Result<Vec<f32>, ParseError<'i, ()>> {
        let start = input.position();
        while input.next().is_ok() {}
        let offsets: Vec<f32> = input
            .slice_from(start)
            .split(',')
            .filter_map(|frame_selector| {
                match frame_selector.trim() {
                    "from" => Some(0.0),
                    "to" => Some(1.0),
                    other => other
                        .strip_suffix('%')
                        .and_then(|percent| percent.trim().parse::<f32>().ok())
                        .map(|percent| percent / 100.0),
                }
                .filter(|offset| offset.is_finite())
                .map(|offset| offset.clamp(0.0, 1.0))
            })
            .collect();
        if offsets.is_empty() {
            return Err(input.new_error_for_next_token());
        }
        Ok(offsets)
    }

    fn parse_block<'t>(
        &mut self,
        offsets: Vec<f32>,
        _start: &ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<(), ParseError<'i, ()>> {
        let declarations = collect_declarations(input);
        for offset in offsets {
            self.frames.push((offset, declarations.clone()));
        }
        Ok(())
    }
}

impl<'i> DeclarationParser<'i> for KeyframesParser {
    type Declaration = ();
    type Error = ();
}

impl<'i> AtRuleParser<'i> for KeyframesParser {
    type Prelude = ();
    type AtRule = ();
    type Error = ();
}

impl<'i> RuleBodyItemParser<'i, (), ()> for KeyframesParser {
    fn parse_declarations(&self) -> bool {
        false
    }
    fn parse_qualified(&self) -> bool {
        true
    }
}

/// Collects raw `name: value` pairs (for `@font-face`, whose `src` list
/// does not go through the component pipeline).
struct RawDeclarationCollector {
    declarations: Vec<(String, String)>,
}

impl<'i> DeclarationParser<'i> for RawDeclarationCollector {
    type Declaration = ();
    type Error = ();

    fn parse_value<'t>(
        &mut self,
        name: CowRcStr<'i>,
        input: &mut Parser<'i, 't>,
        _declaration_start: &ParserState,
    ) -> Result<(), ParseError<'i, ()>> {
        let start = input.position();
        while input.next().is_ok() {}
        self.declarations.push((
            name.to_ascii_lowercase(),
            input.slice_from(start).trim().to_string(),
        ));
        Ok(())
    }
}

impl<'i> AtRuleParser<'i> for RawDeclarationCollector {
    type Prelude = ();
    type AtRule = ();
    type Error = ();
}

impl<'i> QualifiedRuleParser<'i> for RawDeclarationCollector {
    type Prelude = ();
    type QualifiedRule = ();
    type Error = ();
}

impl<'i> RuleBodyItemParser<'i, (), ()> for RawDeclarationCollector {
    fn parse_declarations(&self) -> bool {
        true
    }
    fn parse_qualified(&self) -> bool {
        false
    }
}

/// Parses an `@font-face` block: the family name and its `src` list
/// ("url(x) format('woff2'), url(y.ttf)").
fn parse_font_face(input: &mut Parser) -> Option<FontFace> {
    let mut collector = RawDeclarationCollector {
        declarations: Vec::new(),
    };
    for _ in RuleBodyParser::new(input, &mut collector) {}
    let mut family = None;
    let mut sources: Vec<(String, Option<String>)> = Vec::new();
    for (name, value) in &collector.declarations {
        match name.as_str() {
            "font-family" => {
                let value = value
                    .strip_prefix(['"', '\''])
                    .and_then(|rest| rest.strip_suffix(['"', '\'']))
                    .unwrap_or(value);
                family = Some(value.to_string());
            }
            "src" => {
                for part in split_top_level_commas(value) {
                    // ASCII-lowercasing preserves byte offsets.
                    let Some(url_start) = part.to_ascii_lowercase().find("url(") else {
                        continue;
                    };
                    let after = &part[url_start + 4..];
                    let Some(close) = after.find(')') else {
                        continue;
                    };
                    let url = after[..close]
                        .trim()
                        .trim_matches(|character| character == '"' || character == '\'')
                        .to_string();
                    let format = part.find("format(").and_then(|at| {
                        let inner = &part[at + 7..];
                        let close = inner.find(')')?;
                        Some(
                            inner[..close]
                                .trim()
                                .trim_matches(|character: char| {
                                    character == '"' || character == '\''
                                })
                                .to_ascii_lowercase(),
                        )
                    });
                    sources.push((url, format));
                }
            }
            _ => {}
        }
    }
    Some(FontFace {
        family: family?,
        sources,
    })
}

/// Splits at top-level commas only (parenthesized content stays together).
fn split_top_level_commas(source: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (index, character) in source.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&source[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&source[start..]);
    parts
}

/// Expands the `background` shorthand at raw level, layer by layer
/// (top-level commas). Each layer contributes its image, repeat and
/// position; the color may appear on any layer (CSS allows it only on the
/// last). Unsupported parts (attachment, origin/clip keywords) are
/// ignored. An explicit `none` emits `background-image: none` so the
/// declaration resets an earlier image in the cascade instead of
/// vanishing.
fn expand_background_shorthand(source: &str, output: &mut Vec<Declaration>, important: bool) {
    let mut images: Vec<String> = Vec::new();
    let mut repeats: Vec<String> = Vec::new();
    let mut positions: Vec<String> = Vec::new();
    let mut color: Option<CssValue> = None;
    let mut explicit_none = false;
    let layers = split_top_level_commas(source);

    for layer in &layers {
        let mut image = String::from("none");
        let mut repeat = String::from("repeat");
        let mut position_parts: Vec<String> = Vec::new();
        for component in split_components(layer) {
            match CssValue::parse_component(&component) {
                Some(CssValue::Url(_) | CssValue::Function(..)) => image = component.clone(),
                Some(CssValue::Color(parsed)) => color = Some(CssValue::Color(parsed)),
                Some(CssValue::Keyword(keyword)) => match keyword.as_str() {
                    "repeat" | "no-repeat" | "repeat-x" | "repeat-y" => repeat = keyword,
                    "left" | "right" | "top" | "bottom" | "center" if position_parts.len() < 2 => {
                        position_parts.push(keyword);
                    }
                    "transparent" => color = Some(CssValue::Keyword(keyword)),
                    "none" => explicit_none = true,
                    _ => {}
                },
                Some(CssValue::Length(..)) if position_parts.len() < 2 => {
                    position_parts.push(component.clone());
                }
                _ => {}
            }
        }
        images.push(image);
        repeats.push(repeat);
        positions.push(if position_parts.is_empty() {
            "0px 0px".to_string()
        } else {
            position_parts.join(" ")
        });
    }

    let mut push = |name: &str, value: CssValue| {
        output.push(Declaration {
            name: name.to_string(),
            value,
            important,
        });
    };
    if let Some(color) = color {
        push("background-color", color);
    }
    if images.iter().any(|image| image != "none") {
        push("background-image", CssValue::String(images.join(", ")));
        push("background-repeat", CssValue::String(repeats.join(", ")));
        push(
            "background-position",
            CssValue::String(positions.join(", ")),
        );
    } else if explicit_none {
        push("background-image", CssValue::Keyword("none".to_string()));
    }
}

/// Expands the `font` shorthand: [style] [weight] size[/line-height]
/// family... Unsupported system-font keywords drop the declaration.
fn expand_font_shorthand(source: &str, output: &mut Vec<Declaration>, important: bool) {
    let mut push = |name: &str, value: CssValue| {
        output.push(Declaration {
            name: name.to_string(),
            value,
            important,
        });
    };
    let mut family_parts: Vec<String> = Vec::new();
    let mut saw_size = false;
    for component in split_components(source) {
        if saw_size {
            family_parts.push(component);
            continue;
        }
        match component.as_str() {
            "normal" => {}
            "italic" | "oblique" => {
                push("font-style", CssValue::Keyword("italic".to_string()));
            }
            "bold" | "bolder" => push("font-weight", CssValue::Keyword("bold".to_string())),
            "small-caps" => {}
            _ => {
                // size[/line-height] or a numeric weight.
                let (size_text, line_height) = match component.split_once('/') {
                    Some((size, line_height)) => (size, Some(line_height.to_string())),
                    None => (component.as_str(), None),
                };
                match CssValue::parse_component(size_text) {
                    Some(CssValue::Number(weight)) if weight >= 100.0 => {
                        push("font-weight", CssValue::Number(weight));
                    }
                    Some(size @ CssValue::Length(..)) => {
                        push("font-size", size);
                        if let Some(value) =
                            line_height.and_then(|text| CssValue::parse_component(&text))
                        {
                            push("line-height", value);
                        }
                        saw_size = true;
                    }
                    _ => {}
                }
            }
        }
    }
    if let Some(families) = normalize_font_families(&family_parts.join(" ")) {
        output.push(Declaration {
            name: "font-family".to_string(),
            value: CssValue::Keyword(families),
            important,
        });
    }
}

/// Parses a `;`-separated declaration list (also used for inline `style=`
/// attributes). Malformed entries are skipped; shorthands are expanded.
#[must_use]
pub fn parse_declarations(source: &str) -> Vec<Declaration> {
    let mut input = ParserInput::new(source);
    let mut input = Parser::new(&mut input);
    collect_declarations(&mut input)
}

/// Runs one declaration through the value pipeline: `!important` peeling,
/// shorthand expansion, raw-kept and `var()`-carrying properties, then
/// component parsing with longhand expansion.
fn push_declaration(name: &str, value: &str, declarations: &mut Vec<Declaration>) {
    if name.is_empty() {
        return;
    }
    // `!important` peels off the end of the value (case-insensitive,
    // whitespace tolerated).
    let trimmed = value.trim_end();
    let (value, important) = match trimmed
        .to_ascii_lowercase()
        .strip_suffix("important")
        .map(|rest| rest.trim_end())
        .and_then(|rest| rest.strip_suffix('!').map(str::len))
    {
        Some(prefix_length) => (&trimmed[..prefix_length], true),
        None => (value, false),
    };
    // The font shorthand needs raw handling: "14px/1.4" does not parse
    // as one component.
    if name == "font" {
        expand_font_shorthand(value, declarations, important);
        return;
    }
    // aspect-ratio and box-shadow keep their raw text ("16 / 9" and
    // shadow commas would not survive component parsing).
    if name == "background" {
        expand_background_shorthand(value, declarations, important);
        return;
    }
    if crate::properties::keeps_raw(name) {
        declarations.push(Declaration {
            name: name.to_string(),
            value: CssValue::Keyword(value.trim().to_string()),
            important,
        });
        return;
    }
    // Custom properties keep their raw text (substituted into var()
    // uses later); values using var() defer parsing entirely.
    if name.starts_with("--") {
        declarations.push(Declaration {
            name: name.to_string(),
            value: CssValue::String(value.trim().to_string()),
            important,
        });
        return;
    }
    if value.contains("var(") {
        declarations.push(Declaration {
            name: name.to_string(),
            value: CssValue::Unresolved(value.trim().to_string()),
            important,
        });
        return;
    }
    if name == "font-family" {
        if let Some(families) = normalize_font_families(value) {
            declarations.push(Declaration {
                name: name.to_string(),
                value: CssValue::Keyword(families),
                important,
            });
        }
        return;
    }
    let components: Vec<CssValue> = split_components(value)
        .iter()
        .filter_map(|component| CssValue::parse_component(component))
        .collect();
    if components.is_empty() {
        return;
    }
    let start = declarations.len();
    expand_declaration(name, components, declarations);
    if important {
        for declaration in &mut declarations[start..] {
            declaration.important = true;
        }
    }
}

/// A `font-family` list in author order, lowercased, quotes dropped and
/// inner whitespace collapsed: `"Helvetica Neue", Arial , sans-serif`
/// becomes `helvetica neue,arial,sans-serif`. `None` when the list holds
/// no usable name.
fn normalize_font_families(value: &str) -> Option<String> {
    let families: Vec<String> = split_top_level_commas(value)
        .into_iter()
        .map(|family| {
            family
                .trim()
                .trim_matches(['"', '\''])
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_ascii_lowercase()
        })
        .filter(|family| !family.is_empty())
        .collect();
    (!families.is_empty()).then(|| families.join(","))
}

/// Expands `margin`/`padding`/`border-width` shorthands into longhands and
/// `background` to `background-color`; every other property keeps its first
/// component (multi-value forms of other properties are unsupported).
fn expand_declaration(name: &str, mut components: Vec<CssValue>, output: &mut Vec<Declaration>) {
    let expand_edges = |suffix_for: &dyn Fn(&str) -> String,
                        components: &[CssValue],
                        output: &mut Vec<Declaration>| {
        let Some(edges) = edge_values(components) else {
            return;
        };
        for (side, value) in SIDES.iter().zip(edges) {
            output.push(Declaration {
                important: false,
                name: suffix_for(side),
                value,
            });
        }
    };

    // One border side: width/style/color in any order; missing width is
    // 3px (CSS medium), missing style is solid, missing color falls back
    // to the element color at computed-style time.
    let expand_border_side =
        |side: &str, components: &[CssValue], output: &mut Vec<Declaration>| {
            let mut width = CssValue::Length(3.0, crate::value::Unit::Px);
            let mut style = CssValue::Keyword("none".to_string());
            let mut color = None;
            for component in components {
                match component {
                    CssValue::Length(..) | CssValue::Number(_) => width = component.clone(),
                    CssValue::Color(_) => color = Some(component.clone()),
                    CssValue::Keyword(keyword) if BORDER_STYLES.contains(&keyword.as_str()) => {
                        style = component.clone();
                    }
                    _ => {}
                }
            }
            output.push(Declaration {
                important: false,
                name: format!("border-{side}-width"),
                value: width,
            });
            output.push(Declaration {
                important: false,
                name: format!("border-{side}-style"),
                value: style,
            });
            if let Some(color) = color {
                output.push(Declaration {
                    important: false,
                    name: format!("border-{side}-color"),
                    value: color,
                });
            }
        };

    // One value to both sides of an axis, two values to start/end
    // (logical-property expansion; LTR: inline-start = left,
    // block-start = top).
    let expand_axis = |components: &[CssValue],
                       start_name: &str,
                       end_name: &str,
                       output: &mut Vec<Declaration>| {
        let start = match components.first() {
            Some(value) => value.clone(),
            None => return,
        };
        let end = components.get(1).unwrap_or(&start).clone();
        for (name, value) in [(start_name, start), (end_name, end)] {
            output.push(Declaration {
                important: false,
                name: name.to_string(),
                value,
            });
        }
    };
    // Renames a single logical longhand to its physical equivalent.
    let rename = |components: &[CssValue], physical: &str, output: &mut Vec<Declaration>| {
        if let Some(value) = components.first() {
            output.push(Declaration {
                important: false,
                name: physical.to_string(),
                value: value.clone(),
            });
        }
    };

    match name {
        "margin" | "padding" => {
            expand_edges(&|side| format!("{name}-{side}"), &components, output);
        }
        // Logical box properties (LTR mapping: inline → left/right,
        // block → top/bottom).
        "margin-inline" => expand_axis(&components, "margin-left", "margin-right", output),
        "margin-block" => expand_axis(&components, "margin-top", "margin-bottom", output),
        "padding-inline" => expand_axis(&components, "padding-left", "padding-right", output),
        "padding-block" => expand_axis(&components, "padding-top", "padding-bottom", output),
        "margin-inline-start" => rename(&components, "margin-left", output),
        "margin-inline-end" => rename(&components, "margin-right", output),
        "margin-block-start" => rename(&components, "margin-top", output),
        "margin-block-end" => rename(&components, "margin-bottom", output),
        "padding-inline-start" => rename(&components, "padding-left", output),
        "padding-inline-end" => rename(&components, "padding-right", output),
        "padding-block-start" => rename(&components, "padding-top", output),
        "padding-block-end" => rename(&components, "padding-bottom", output),
        "inline-size" => rename(&components, "width", output),
        "block-size" => rename(&components, "height", output),
        // Logical border shorthands: same shape as their physical twins.
        "border-inline" | "border-block" => {
            let sides: [&str; 2] = if name == "border-inline" {
                ["left", "right"]
            } else {
                ["top", "bottom"]
            };
            for side in sides {
                expand_border_side(side, &components, output);
            }
        }
        "border-inline-start" => expand_border_side("left", &components, output),
        "border-inline-end" => expand_border_side("right", &components, output),
        "border-block-start" => expand_border_side("top", &components, output),
        "border-block-end" => expand_border_side("bottom", &components, output),
        "border-inline-width" => {
            expand_axis(
                &components,
                "border-left-width",
                "border-right-width",
                output,
            );
        }
        "border-inline-style" => {
            expand_axis(
                &components,
                "border-left-style",
                "border-right-style",
                output,
            );
        }
        "border-inline-color" => {
            expand_axis(
                &components,
                "border-left-color",
                "border-right-color",
                output,
            );
        }
        "border-block-width" => {
            expand_axis(
                &components,
                "border-top-width",
                "border-bottom-width",
                output,
            );
        }
        "border-block-style" => {
            expand_axis(
                &components,
                "border-top-style",
                "border-bottom-style",
                output,
            );
        }
        "border-block-color" => {
            expand_axis(
                &components,
                "border-top-color",
                "border-bottom-color",
                output,
            );
        }
        "border-inline-start-width" => rename(&components, "border-left-width", output),
        "border-inline-start-style" => rename(&components, "border-left-style", output),
        "border-inline-start-color" => rename(&components, "border-left-color", output),
        "border-inline-end-width" => rename(&components, "border-right-width", output),
        "border-inline-end-style" => rename(&components, "border-right-style", output),
        "border-inline-end-color" => rename(&components, "border-right-color", output),
        "border-block-start-width" => rename(&components, "border-top-width", output),
        "border-block-start-style" => rename(&components, "border-top-style", output),
        "border-block-start-color" => rename(&components, "border-top-color", output),
        "border-block-end-width" => rename(&components, "border-bottom-width", output),
        "border-block-end-style" => rename(&components, "border-bottom-style", output),
        "border-block-end-color" => rename(&components, "border-bottom-color", output),
        // inset shorthand (top right bottom left, margin logic) and its
        // logical longhands.
        "inset" => {
            expand_edges(&|side| side.to_string(), &components, output);
        }
        "inset-inline" => expand_axis(&components, "left", "right", output),
        "inset-block" => expand_axis(&components, "top", "bottom", output),
        "inset-inline-start" => rename(&components, "left", output),
        "inset-inline-end" => rename(&components, "right", output),
        "inset-block-start" => rename(&components, "top", output),
        "inset-block-end" => rename(&components, "bottom", output),
        "border-width" => {
            expand_edges(&|side| format!("border-{side}-width"), &components, output);
        }
        "border-style" => {
            expand_edges(&|side| format!("border-{side}-style"), &components, output);
        }
        "border-color" => {
            expand_edges(&|side| format!("border-{side}-color"), &components, output);
        }
        "border-radius" => {
            // 1-4 values, clockwise from top-left.
            let corners = ["top-left", "top-right", "bottom-right", "bottom-left"];
            let Some(values) = edge_values(&components) else {
                return;
            };
            for (corner, value) in corners.iter().zip(values) {
                output.push(Declaration {
                    important: false,
                    name: format!("border-{corner}-radius"),
                    value,
                });
            }
        }
        "border" => {
            for side in SIDES {
                expand_border_side(side, &components, output);
            }
        }
        "border-top" | "border-right" | "border-bottom" | "border-left" => {
            let side = &name["border-".len()..];
            expand_border_side(side, &components, output);
        }
        // Multi-value properties whose components must survive together;
        // the engine re-splits the joined text.
        "outline" => {
            // width/style/color in any order, like one border side.
            for component in &components {
                match component {
                    CssValue::Length(..) | CssValue::Number(_) => {
                        output.push(Declaration {
                            name: "outline-width".to_string(),
                            value: component.clone(),
                            important: false,
                        });
                    }
                    CssValue::Color(_) => output.push(Declaration {
                        name: "outline-color".to_string(),
                        value: component.clone(),
                        important: false,
                    }),
                    CssValue::Keyword(keyword) if BORDER_STYLES.contains(&keyword.as_str()) => {
                        output.push(Declaration {
                            name: "outline-style".to_string(),
                            value: component.clone(),
                            important: false,
                        });
                    }
                    _ => {}
                }
            }
        }
        "text-decoration"
        | "background-position"
        | "background-size"
        | "overflow"
        | "object-position" => {
            let text = components
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" ");
            output.push(Declaration {
                name: name.to_string(),
                value: CssValue::Keyword(text),
                important: false,
            });
        }
        "flex" => {
            // grow [shrink] [basis]; `none` = 0 0, `auto` = 1 1,
            // `initial` = 0 1. A non-numeric trailing component is the
            // basis (per spec the basis may also appear without shrink).
            let (grow, shrink, basis) = match components.first() {
                Some(CssValue::Keyword(keyword)) if keyword == "none" => (0.0, 0.0, None),
                Some(CssValue::Keyword(keyword)) if keyword == "auto" => (1.0, 1.0, None),
                Some(CssValue::Keyword(keyword)) if keyword == "initial" => (0.0, 1.0, None),
                Some(CssValue::Number(grow)) => {
                    // Bare `0` parses as a zero length, not a number;
                    // both are valid shrink factors here.
                    let as_factor = |value: &CssValue| match value {
                        CssValue::Number(number) => Some(*number),
                        CssValue::Length(0.0, _) => Some(0.0),
                        _ => None,
                    };
                    let (shrink, basis_at) = match components.get(1).and_then(as_factor) {
                        Some(shrink) => (shrink, 2),
                        // `flex: 1 200px`: the second component is the
                        // basis, not the shrink factor.
                        None => (1.0, 1),
                    };
                    let basis = components
                        .get(basis_at)
                        .filter(|value| as_factor(value).is_none())
                        .cloned();
                    (*grow, shrink, basis)
                }
                _ => return,
            };
            output.push(Declaration {
                important: false,
                name: "flex-grow".to_string(),
                value: CssValue::Number(grow),
            });
            output.push(Declaration {
                important: false,
                name: "flex-shrink".to_string(),
                value: CssValue::Number(shrink),
            });
            if let Some(basis) = basis {
                output.push(Declaration {
                    important: false,
                    name: "flex-basis".to_string(),
                    value: basis,
                });
            }
        }
        _ => output.push(Declaration {
            name: name.to_string(),
            value: components.swap_remove(0),
            important: false,
        }),
    }
}

const SIDES: [&str; 4] = ["top", "right", "bottom", "left"];
const BORDER_STYLES: [&str; 5] = ["none", "hidden", "solid", "dashed", "dotted"];

/// CSS 1-to-4 value expansion: top, right, bottom, left.
fn edge_values(components: &[CssValue]) -> Option<[CssValue; 4]> {
    let get = |index: usize| components[index].clone();
    match components.len() {
        1 => Some([get(0), get(0), get(0), get(0)]),
        2 => Some([get(0), get(1), get(0), get(1)]),
        3 => Some([get(0), get(1), get(2), get(1)]),
        4 => Some([get(0), get(1), get(2), get(3)]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{Color, Unit};

    fn px(value: f32) -> CssValue {
        CssValue::Length(value, Unit::Px)
    }

    #[test]
    fn parses_selector_list_and_declarations() {
        let sheet = parse_stylesheet(".card, #main { width: 400px; color: #222; }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].selectors.len(), 2);
        assert_eq!(
            sheet.rules[0].declarations,
            vec![
                Declaration {
                    name: "width".to_string(),
                    value: px(400.0),
                    important: false,
                },
                Declaration {
                    name: "color".to_string(),
                    value: CssValue::Color(Color::rgb(0x22, 0x22, 0x22)),
                    important: false,
                },
            ]
        );
    }

    #[test]
    fn expands_margin_shorthand() {
        let cases: &[(&str, [f32; 4])] = &[
            ("margin: 8px", [8.0, 8.0, 8.0, 8.0]),
            ("margin: 8px 16px", [8.0, 16.0, 8.0, 16.0]),
            ("margin: 1px 2px 3px", [1.0, 2.0, 3.0, 2.0]),
            ("margin: 1px 2px 3px 4px", [1.0, 2.0, 3.0, 4.0]),
        ];
        for (source, [top, right, bottom, left]) in cases {
            let declarations = parse_declarations(source);
            let expect = |name: &str, value: f32| {
                assert!(
                    declarations
                        .iter()
                        .any(|declaration| declaration.name == name
                            && declaration.value == px(value)),
                    "{source}: expected {name}={value}px in {declarations:?}"
                );
            };
            expect("margin-top", *top);
            expect("margin-right", *right);
            expect("margin-bottom", *bottom);
            expect("margin-left", *left);
        }
    }

    #[test]
    fn expands_flex_shorthand() {
        let sheet = parse_stylesheet("a { flex: 2; } b { flex: none; } c { flex: 1 3; }");
        let decls = |index: usize| -> Vec<(String, String)> {
            sheet.rules[index]
                .declarations
                .iter()
                .map(|d| (d.name.clone(), format!("{:?}", d.value)))
                .collect()
        };
        assert_eq!(
            decls(0),
            vec![
                ("flex-grow".to_string(), "Number(2.0)".to_string()),
                ("flex-shrink".to_string(), "Number(1.0)".to_string()),
            ]
        );
        assert_eq!(
            decls(1),
            vec![
                ("flex-grow".to_string(), "Number(0.0)".to_string()),
                ("flex-shrink".to_string(), "Number(0.0)".to_string()),
            ]
        );
        assert_eq!(
            decls(2),
            vec![
                ("flex-grow".to_string(), "Number(1.0)".to_string()),
                ("flex-shrink".to_string(), "Number(3.0)".to_string()),
            ]
        );
    }

    #[test]
    fn expands_border_width_shorthand() {
        let declarations = parse_declarations("border-width: 1px 2px");
        let names: Vec<&str> = declarations
            .iter()
            .map(|declaration| declaration.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "border-top-width",
                "border-right-width",
                "border-bottom-width",
                "border-left-width"
            ]
        );
        assert_eq!(declarations[1].value, px(2.0));
    }

    #[test]
    fn border_shorthand_expands_width_style_color() {
        let declarations = parse_declarations("border: 1px solid #cccccc");
        assert_eq!(declarations.len(), 12);
        let find = |name: &str| {
            declarations
                .iter()
                .find(|declaration| declaration.name == name)
                .map(|declaration| declaration.value.clone())
        };
        assert_eq!(find("border-top-width"), Some(px(1.0)));
        assert_eq!(
            find("border-left-style"),
            Some(CssValue::Keyword("solid".to_string()))
        );
        assert_eq!(
            find("border-bottom-color"),
            Some(CssValue::Color(Color::rgb(0xcc, 0xcc, 0xcc)))
        );
        // Order-independent, defaults for missing parts (medium = 3px).
        let declarations = parse_declarations("border: red");
        let find = |name: &str| {
            declarations
                .iter()
                .find(|declaration| declaration.name == name)
                .map(|declaration| declaration.value.clone())
        };
        assert_eq!(find("border-top-width"), Some(px(3.0)));
        assert_eq!(
            find("border-top-color"),
            Some(CssValue::Color(Color::rgb(255, 0, 0)))
        );
    }

    #[test]
    fn border_radius_expands_clockwise_from_top_left() {
        let declarations = parse_declarations("border-radius: 1px 2px");
        let names: Vec<&str> = declarations
            .iter()
            .map(|declaration| declaration.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "border-top-left-radius",
                "border-top-right-radius",
                "border-bottom-right-radius",
                "border-bottom-left-radius"
            ]
        );
        assert_eq!(declarations[2].value, px(1.0));
        assert_eq!(declarations[3].value, px(2.0));
    }

    #[test]
    fn logical_properties_map_to_physical_ltr() {
        let declarations = parse_declarations(
            "margin-inline: 1px 2px; margin-block-start: 3px; padding-inline-start: 4px; \
             padding-block: 5px 6px; inline-size: 7px; block-size: 8px",
        );
        let find = |name: &str| {
            declarations
                .iter()
                .find(|declaration| declaration.name == name)
                .map(|declaration| declaration.value.clone())
        };
        assert_eq!(find("margin-left"), Some(px(1.0)));
        assert_eq!(find("margin-right"), Some(px(2.0)));
        assert_eq!(find("margin-top"), Some(px(3.0)));
        assert_eq!(find("padding-left"), Some(px(4.0)));
        assert_eq!(find("padding-top"), Some(px(5.0)));
        assert_eq!(find("padding-bottom"), Some(px(6.0)));
        assert_eq!(find("width"), Some(px(7.0)));
        assert_eq!(find("height"), Some(px(8.0)));
    }

    #[test]
    fn logical_border_shorthands_expand_to_physical_sides() {
        let declarations =
            parse_declarations("border-inline: 2px dashed red; border-block-start-width: 5px");
        let find = |name: &str| {
            declarations
                .iter()
                .find(|declaration| declaration.name == name)
                .map(|declaration| declaration.value.clone())
        };
        assert_eq!(find("border-left-width"), Some(px(2.0)));
        assert_eq!(find("border-right-width"), Some(px(2.0)));
        assert_eq!(
            find("border-left-style"),
            Some(CssValue::Keyword("dashed".to_string()))
        );
        assert_eq!(
            find("border-right-color"),
            Some(CssValue::Color(Color::rgb(255, 0, 0)))
        );
        assert_eq!(find("border-top-width"), Some(px(5.0)));
    }

    #[test]
    fn inset_shorthand_and_logical_longhands_expand() {
        let declarations = parse_declarations("inset: 1px 2px; inset-inline-end: 9px");
        // Last expansion wins, mirroring source-order cascade.
        let find = |name: &str| {
            declarations
                .iter()
                .rev()
                .find(|declaration| declaration.name == name)
                .map(|declaration| declaration.value.clone())
        };
        assert_eq!(find("top"), Some(px(1.0)));
        assert_eq!(find("bottom"), Some(px(1.0)));
        assert_eq!(find("left"), Some(px(2.0)));
        // inset-inline-end later in source order wins for `right`.
        assert_eq!(find("right"), Some(px(9.0)));
        let declarations = parse_declarations("inset: 1px 2px 3px 4px");
        let find = |name: &str| {
            declarations
                .iter()
                .find(|declaration| declaration.name == name)
                .map(|declaration| declaration.value.clone())
        };
        assert_eq!(find("top"), Some(px(1.0)));
        assert_eq!(find("right"), Some(px(2.0)));
        assert_eq!(find("bottom"), Some(px(3.0)));
        assert_eq!(find("left"), Some(px(4.0)));
    }

    #[test]
    fn overflow_keeps_both_axis_values() {
        let declarations = parse_declarations("overflow: hidden auto");
        assert_eq!(declarations.len(), 1);
        assert_eq!(
            declarations[0].value,
            CssValue::Keyword("hidden auto".to_string())
        );
    }

    #[test]
    fn flex_shorthand_emits_flex_basis() {
        let declarations = parse_declarations("flex: 1 0 200px");
        let find = |name: &str| {
            declarations
                .iter()
                .find(|declaration| declaration.name == name)
                .map(|declaration| declaration.value.clone())
        };
        assert_eq!(find("flex-grow"), Some(CssValue::Number(1.0)));
        assert_eq!(find("flex-shrink"), Some(CssValue::Number(0.0)));
        assert_eq!(find("flex-basis"), Some(px(200.0)));
        // Basis without an explicit shrink: `flex: 2 50%`.
        let declarations = parse_declarations("flex: 2 50%");
        let find = |name: &str| {
            declarations
                .iter()
                .find(|declaration| declaration.name == name)
                .map(|declaration| declaration.value.clone())
        };
        assert_eq!(find("flex-shrink"), Some(CssValue::Number(1.0)));
        assert_eq!(
            find("flex-basis"),
            Some(CssValue::Length(50.0, Unit::Percent))
        );
    }

    #[test]
    fn border_side_shorthand_targets_one_side() {
        let declarations = parse_declarations("border-top: 2px dashed #112233");
        let names: Vec<&str> = declarations
            .iter()
            .map(|declaration| declaration.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["border-top-width", "border-top-style", "border-top-color"]
        );
    }

    #[test]
    fn border_color_and_style_expand_per_side() {
        let declarations = parse_declarations("border-color: red blue; border-style: solid none");
        let find = |name: &str| {
            declarations
                .iter()
                .find(|declaration| declaration.name == name)
                .map(|declaration| declaration.value.clone())
        };
        assert_eq!(
            find("border-right-color"),
            Some(CssValue::Color(Color::rgb(0, 0, 255)))
        );
        assert_eq!(
            find("border-bottom-color"),
            Some(CssValue::Color(Color::rgb(255, 0, 0)))
        );
        assert_eq!(
            find("border-left-style"),
            Some(CssValue::Keyword("none".to_string()))
        );
    }

    #[test]
    fn longhand_overrides_survive_in_source_order() {
        let declarations = parse_declarations("margin: 8px; margin-left: 2px");
        let names: Vec<&str> = declarations
            .iter()
            .map(|declaration| declaration.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "margin-top",
                "margin-right",
                "margin-bottom",
                "margin-left",
                "margin-left"
            ]
        );
    }

    #[test]
    fn skips_malformed_declarations_and_extra_semicolons() {
        let declarations = parse_declarations(";; color: red; oops; width: ; height: 10px; x: !!;");
        assert_eq!(declarations.len(), 2);
        assert_eq!(declarations[0].name, "color");
        assert_eq!(declarations[1].name, "height");
    }

    #[test]
    fn font_face_blocks_collect_family_and_sources() {
        let sheet = parse_stylesheet(
            "@font-face { font-family: 'My Font'; \
                          src: url(a.woff2) format('woff2'), url('b.ttf') format(\"truetype\"); } \
             p { color: red; }",
        );
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.font_faces.len(), 1);
        let face = &sheet.font_faces[0];
        assert_eq!(face.family, "My Font");
        assert_eq!(
            face.sources,
            vec![
                ("a.woff2".to_string(), Some("woff2".to_string())),
                ("b.ttf".to_string(), Some("truetype".to_string())),
            ]
        );
    }

    #[test]
    fn media_rules_carry_conditions_and_filter_by_width() {
        let sheet = parse_stylesheet(
            "p { color: red; }
             @media screen and (max-width: 700px) {
                 div { margin: 0; }
             }
             @media (min-width: 400px) and (max-width: 900px) {
                 b { color: blue; }
             }
             @media print { i { color: green; } }
             h1 { color: blue; }",
        );
        // print block is skipped entirely; the rest parse.
        assert_eq!(sheet.rules.len(), 4);
        let media = sheet.rules[1].media.unwrap();
        assert_eq!(media.max_width, Some(700.0));
        assert!(media.matches(500.0));
        assert!(!media.matches(800.0));

        let narrow = sheet.for_width(300.0);
        assert_eq!(narrow.rules.len(), 3); // p, div, h1
        let middle = sheet.for_width(600.0);
        assert_eq!(middle.rules.len(), 4);
        let wide = sheet.for_width(1200.0);
        assert_eq!(wide.rules.len(), 2); // p, h1
    }

    #[test]
    fn media_range_syntax_maps_inclusive_bounds() {
        // Media Queries 4 range syntax: inclusive forms map onto the model.
        let sheet = parse_stylesheet(
            "@media (400px <= width <= 900px) { p { color: red; } }
             @media (width >= 600px) { div { color: blue; } }",
        );
        assert_eq!(sheet.rules.len(), 2);
        let interval = sheet.rules[0].media.unwrap();
        assert_eq!(interval.min_width, Some(400.0));
        assert_eq!(interval.max_width, Some(900.0));
        let range = sheet.rules[1].media.unwrap();
        assert_eq!(range.min_width, Some(600.0));
        assert_eq!(range.max_width, None);
    }

    #[test]
    fn media_strict_range_maps_exclusive_bounds() {
        // Strict bounds keep their exclusive semantics: the bound itself
        // does not match.
        let sheet = parse_stylesheet(
            "@media (width > 600px) { p { color: red; } }
             @media (400px < width <= 900px) { div { color: blue; } }
             h1 { color: green; }",
        );
        assert_eq!(sheet.rules.len(), 3);
        let strict = sheet.rules[0].media.unwrap();
        assert_eq!(strict.min_width, Some(600.0));
        assert!(strict.min_exclusive);
        assert!(!strict.matches(600.0));
        assert!(strict.matches(601.0));
        let interval = sheet.rules[1].media.unwrap();
        assert_eq!(interval.min_width, Some(400.0));
        assert_eq!(interval.max_width, Some(900.0));
        assert!(interval.min_exclusive);
        assert!(!interval.max_exclusive);
        assert!(!interval.matches(400.0));
        assert!(interval.matches(900.0));
        assert!(!interval.matches(901.0));
        // Strict max bound as well.
        let sheet = parse_stylesheet("@media (width < 500px) { p { color: red; } }");
        let strict_max = sheet.rules[0].media.unwrap();
        assert_eq!(strict_max.max_width, Some(500.0));
        assert!(strict_max.max_exclusive);
        assert!(strict_max.matches(499.0));
        assert!(!strict_max.matches(500.0));
    }

    #[test]
    fn background_none_emits_background_image_none() {
        // `background: none` must survive as background-image: none so it
        // resets an earlier image in the cascade (it used to vanish).
        let declarations = parse_declarations("background: none");
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].name, "background-image");
        assert_eq!(declarations[0].value, CssValue::Keyword("none".to_string()));
        // A color alongside still expands.
        let declarations = parse_declarations("background: none red");
        assert_eq!(declarations.len(), 2);
        assert_eq!(declarations[0].name, "background-color");
        assert_eq!(declarations[1].name, "background-image");
    }

    #[test]
    fn keyframes_with_non_finite_offsets_are_dropped() {
        // "NaN%" parses as f32::NAN; the offset must not reach the engine.
        let sheet = parse_stylesheet(
            "@keyframes k { NaN% { opacity: 0; } 50% { opacity: 1; } 1e999% { opacity: 0; } }",
        );
        let block = sheet.keyframes("k").expect("keyframes block");
        assert_eq!(block.frames.len(), 1);
        assert_eq!(block.frames[0].0, 0.5);
    }

    #[test]
    fn for_width_shares_instead_of_cloning() {
        // No media queries: every rule applies, so the filtered sheet is
        // the same storage, not a deep clone.
        let sheet = parse_stylesheet("p { color: red; } div { color: blue; }");
        let filtered = sheet.for_width(800.0);
        assert!(std::sync::Arc::ptr_eq(&sheet.rules, &filtered.rules));
        assert_eq!(filtered, sheet);

        // With media queries, only the surviving rules are cloned;
        // font faces and keyframes stay shared.
        let sheet = parse_stylesheet(
            "@font-face { font-family: x; src: url(x.ttf); }
             @keyframes spin { from { opacity: 0; } to { opacity: 1; } }
             p { color: red; }
             @media (max-width: 500px) { div { color: blue; } }",
        );
        // 600px: the media rule drops out, so rules are re-collected.
        let narrow = sheet.for_width(600.0);
        assert_eq!(narrow.rules.len(), 1);
        assert!(!std::sync::Arc::ptr_eq(&sheet.rules, &narrow.rules));
        assert!(std::sync::Arc::ptr_eq(
            &sheet.font_faces,
            &narrow.font_faces
        ));
        assert!(std::sync::Arc::ptr_eq(&sheet.keyframes, &narrow.keyframes));
        // The surviving rule itself is untouched.
        assert_eq!(narrow.rules[0].source_order, sheet.rules[0].source_order);
        assert_eq!(narrow.rules[0], sheet.rules[0]);
    }

    #[test]
    fn nested_media_conditions_intersect() {
        let sheet = parse_stylesheet(
            "@media (min-width: 400px) { @media (max-width: 800px) { p { color: red; } } }",
        );
        assert_eq!(sheet.rules.len(), 1);
        let media = sheet.rules[0].media.unwrap();
        assert_eq!(media.min_width, Some(400.0));
        assert_eq!(media.max_width, Some(800.0));
    }

    #[test]
    fn too_deeply_nested_media_is_dropped_without_recursing() {
        // Nesting within the limit keeps working.
        let ok = format!(
            "{}p {{ color: red; }}{}",
            "@media (min-width: 1px) { ".repeat(8),
            "}".repeat(8)
        );
        assert_eq!(parse_stylesheet(&ok).rules.len(), 1);
        // Past the limit the innermost rules are dropped, outer ones kept.
        let deep = format!(
            "a {{ color: blue; }} {}p {{ color: red; }}{}",
            "@media (min-width: 1px) { ".repeat(MAX_MEDIA_NESTING + 16),
            "}".repeat(MAX_MEDIA_NESTING + 16)
        );
        let sheet = parse_stylesheet(&deep);
        assert_eq!(sheet.rules.len(), 1);
        assert!(sheet.rules[0].media.is_none());
    }

    #[test]
    fn drops_rule_with_invalid_selector() {
        let sheet =
            parse_stylesheet("p ! a { color: red; } h1 { color: blue; } a:blur { color: red; }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].source_order, 0);
    }

    #[test]
    fn skips_comments() {
        let sheet = parse_stylesheet("/* x */ p { /* y */ color: red; }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].declarations.len(), 1);
    }

    #[test]
    fn unterminated_rule_recovers_at_end_of_input() {
        let sheet = parse_stylesheet("p { color: red;");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].declarations.len(), 1);
    }

    #[test]
    fn background_shorthand_keeps_color_and_image() {
        // Color-only: placement without an image is dropped.
        let declarations = parse_declarations("background: #fdfcff left top no-repeat");
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].name, "background-color");
        assert_eq!(
            declarations[0].value,
            CssValue::Color(Color::rgb(0xfd, 0xfc, 0xff))
        );
        // Layered: images/repeats/positions expand as comma lists.
        let layered = parse_declarations("background: url(a.png) no-repeat left top, url(b.png)");
        let names: Vec<&str> = layered
            .iter()
            .map(|declaration| declaration.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "background-image",
                "background-repeat",
                "background-position"
            ]
        );
        assert_eq!(
            layered[0].value,
            CssValue::String("url(a.png), url(b.png)".to_string())
        );
        assert_eq!(
            layered[1].value,
            CssValue::String("no-repeat, repeat".to_string())
        );
    }

    #[test]
    fn data_uri_semicolon_does_not_split_declaration() {
        // Regression: the declaration splitter used to break on the `;`
        // inside an unquoted data URI, losing the URL and corrupting the
        // declarations that followed.
        let declarations =
            parse_declarations("background: url(data:image/png;base64,iVBORw0KGgo=) ; color: red");
        let image = declarations
            .iter()
            .find(|declaration| declaration.name == "background-image")
            .expect("background-image survives");
        assert_eq!(
            image.value,
            CssValue::String("url(data:image/png;base64,iVBORw0KGgo=)".to_string())
        );
        let color = declarations
            .iter()
            .find(|declaration| declaration.name == "color")
            .expect("color survives");
        assert_eq!(color.value, CssValue::Color(Color::rgb(255, 0, 0)));
    }

    #[test]
    fn quoted_brace_does_not_close_rule_early() {
        // Regression: a `}` inside a string used to close the rule block,
        // dropping the declarations after it.
        let sheet = parse_stylesheet("p::before { content: \"}\"; color: red; }");
        assert_eq!(sheet.rules.len(), 1);
        let declarations = &sheet.rules[0].declarations;
        assert_eq!(declarations[0].value, CssValue::String("}".to_string()));
        assert_eq!(declarations[1].name, "color");
        assert_eq!(
            declarations[1].value,
            CssValue::Color(Color::rgb(255, 0, 0))
        );
    }

    #[test]
    fn comment_opener_inside_string_is_not_a_comment() {
        // Regression: comment stripping was not string-aware, so
        // `content: "/*"` swallowed the rest of the stylesheet.
        let sheet = parse_stylesheet("p { content: \"/*\"; color: red; } div { color: blue; }");
        assert_eq!(sheet.rules.len(), 2);
        assert_eq!(
            sheet.rules[0].declarations[0].value,
            CssValue::String("/*".to_string())
        );
        assert_eq!(sheet.rules[0].declarations[1].name, "color");
    }

    #[test]
    fn modern_rgb_forms_parse_as_colors() {
        // Space-separated channels and slash alpha are valid CSS Color 4.
        assert_eq!(
            Color::parse("rgb(255 0 0 / 50%)"),
            Some(Color::rgba(255, 0, 0, 128))
        );
        assert_eq!(Color::parse("rgb(100% 0% 0%)"), Some(Color::rgb(255, 0, 0)));
    }

    #[test]
    fn media_block_is_skipped_without_desync() {
        // Regression: the parser used to close unsupported at-blocks at
        // the first `}`, swallowing every rule that followed.
        let sheet = parse_stylesheet(
            "p { color: red; }
             @supports (display: grid) {
                 div { margin: 0; }
                 body { background-color: white; }
             }
             h1 { color: blue; }",
        );
        assert_eq!(sheet.rules.len(), 2);
        assert!(
            sheet.rules[1].selectors[0]
                .iter_raw_match_order()
                .any(|component| matches!(
                    component,
                    parcel_selectors::parser::Component::LocalName(name)
                        if name.lower_name.as_str() == "h1"
                ))
        );
    }

    #[test]
    fn statement_at_rules_are_skipped() {
        let sheet = parse_stylesheet("@charset \"utf-8\"; @import url(x.css); p { color: red; }");
        assert_eq!(sheet.rules.len(), 1);
    }

    #[test]
    fn unterminated_at_rule_consumes_rest() {
        let sheet = parse_stylesheet("p { color: red; } @media (x) { div { color: blue; }");
        assert_eq!(sheet.rules.len(), 1);
    }

    #[test]
    fn preserves_source_order() {
        let sheet = parse_stylesheet("p { color: red; } div { color: blue; }");
        assert_eq!(sheet.rules[0].source_order, 0);
        assert_eq!(sheet.rules[1].source_order, 1);
    }
}
