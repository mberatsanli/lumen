//! Selector model and parsing.
//!
//! Supported: universal `*`, tag `div`, class `.card`, id `#header`,
//! compound `div.card#x`, combinators (descendant, `>`, `+`, `~`),
//! attribute selectors (`[href]`, `[type="x"]`, `^=`, `$=`, `*=`),
//! structural pseudo-classes (`:first/last/only-child`, `:nth-child()`,
//! `:nth-last-child()`), `:not()` with a compound argument, the dynamic
//! `:link`/`:visited`/`:hover`, and selector lists (handled by the rule
//! parser). Unsupported selectors are rejected and the containing rule is
//! dropped, matching browser behavior.
//!
//! Parsing itself is done by the lightningcss selector parser
//! (`parcel_selectors` over `cssparser`); this module only maps the parsed
//! components onto the engine's selector model.

use cssparser::{Parser, ParserInput};
use lightningcss::selector::{
    Combinator as ParcelCombinator, Component, PseudoClass as LwcPseudoClass,
    PseudoElement as LwcPseudoElement, Selector as LwcSelector, SelectorList as LwcSelectorList,
};
use lightningcss::stylesheet::ParserOptions;
use lightningcss::traits::ParseWithOptions;
use parcel_selectors::attr::{AttrSelectorOperator, ParsedCaseSensitivity};
use parcel_selectors::parser::{NthSelectorData, NthType};

/// Cascade specificity, ordered lexicographically: ids > classes > types.
///
/// The universal selector contributes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Specificity {
    pub ids: u32,
    pub classes: u32,
    pub types: u32,
}

/// How a compound connects to the compound on its right.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Combinator {
    /// Whitespace: any ancestor.
    #[default]
    Descendant,
    /// `>`: the parent.
    Child,
    /// `+`: the immediately preceding element sibling.
    NextSibling,
    /// `~`: any preceding element sibling.
    SubsequentSibling,
}

/// One `[attr]` / `[attr=value]` constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeSelector {
    /// Lowercase attribute name.
    pub name: String,
    pub operation: AttributeOperation,
}

/// The comparison an attribute selector performs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttributeOperation {
    /// `[attr]` — present.
    Exists,
    /// `[attr=v]`
    Equals(String),
    /// `[attr^=v]`
    StartsWith(String),
    /// `[attr$=v]`
    EndsWith(String),
    /// `[attr*=v]`
    Contains(String),
    /// `[attr~=v]` — whitespace-separated word match.
    WordMatch(String),
    /// `[attr|=v]` — exact or `v-` prefix (language ranges).
    LangPrefix(String),
}

/// A parsed pseudo-class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PseudoClass {
    /// Always-true (no visited state).
    Link,
    /// Always-true (no visited state).
    Visited,
    /// Matches the engine's hover chain.
    Hover,
    /// The document's root element (html).
    Root,
    /// Checked checkbox/radio (state supplied by the shell).
    Checked,
    /// Matches while the pointer is pressed on the element (chain).
    Active,
    /// Matches the focused element (focus-within matches its chain).
    Focus,
    FocusWithin,
    FirstChild,
    LastChild,
    OnlyChild,
    /// `an+b` over the 1-based index among element siblings.
    NthChild(i32, i32),
    /// `an+b` counted from the end.
    NthLastChild(i32, i32),
    /// Negation of one compound (no combinators inside).
    Not(Box<CompoundSelector>),
    /// Matches when any listed compound matches. `:is()` takes its most
    /// specific argument's specificity; `:where()` contributes none.
    Is(Vec<CompoundSelector>),
    Where(Vec<CompoundSelector>),
    FirstOfType,
    LastOfType,
    OnlyOfType,
    NthOfType(i32, i32),
    NthLastOfType(i32, i32),
}

/// A compound selector: simple selectors that must all match one element.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompoundSelector {
    /// Lowercase tag name, if constrained.
    pub tag: Option<String>,
    pub id: Option<String>,
    pub classes: Vec<String>,
    pub attributes: Vec<AttributeSelector>,
    pub pseudo_classes: Vec<PseudoClass>,
    /// Supported pseudo-element (only `selection`). Rules with it style
    /// the selection overlay of the matched element, not the element.
    pub pseudo_element: Option<String>,
}

impl CompoundSelector {
    #[must_use]
    pub fn specificity(&self) -> Specificity {
        self.specificity_at(0)
    }

    /// Bounded recursion companion of [`CompoundSelector::specificity`]:
    /// nesting is already capped at parse time, but hand-built selectors
    /// get the same protection.
    fn specificity_at(&self, depth: usize) -> Specificity {
        let mut specificity = Specificity {
            ids: u32::from(self.id.is_some()),
            classes: (self.classes.len() + self.attributes.len()) as u32,
            types: u32::from(self.tag.is_some()) + u32::from(self.pseudo_element.is_some()),
        };
        if depth >= MAX_PSEUDO_NESTING {
            return specificity;
        }
        for pseudo in &self.pseudo_classes {
            match pseudo {
                // Per spec, :not() adds its argument's specificity.
                PseudoClass::Not(inner) => {
                    let inner = inner.specificity_at(depth + 1);
                    specificity.ids = specificity.ids.saturating_add(inner.ids);
                    specificity.classes = specificity.classes.saturating_add(inner.classes);
                    specificity.types = specificity.types.saturating_add(inner.types);
                }
                // :is() takes its most specific argument; :where() none.
                PseudoClass::Is(arguments) => {
                    if let Some(most) = arguments
                        .iter()
                        .map(|compound| compound.specificity_at(depth + 1))
                        .max()
                    {
                        specificity.ids = specificity.ids.saturating_add(most.ids);
                        specificity.classes = specificity.classes.saturating_add(most.classes);
                        specificity.types = specificity.types.saturating_add(most.types);
                    }
                }
                PseudoClass::Where(_) => {}
                _ => specificity.classes = specificity.classes.saturating_add(1),
            }
        }
        specificity
    }
}

/// A complex selector: compounds joined by combinators.
///
/// `compounds` is ordered outermost first; the last entry is the subject
/// (the element the rule applies to). `combinators[i]` joins
/// `compounds[i]` to `compounds[i + 1]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    pub compounds: Vec<CompoundSelector>,
    pub combinators: Vec<Combinator>,
}

impl Selector {
    #[must_use]
    pub fn specificity(&self) -> Specificity {
        self.compounds
            .iter()
            .map(CompoundSelector::specificity)
            .fold(Specificity::default(), |sum, next| Specificity {
                ids: sum.ids.saturating_add(next.ids),
                classes: sum.classes.saturating_add(next.classes),
                types: sum.types.saturating_add(next.types),
            })
    }

    /// The compound the matched element itself must satisfy.
    #[must_use]
    pub fn subject(&self) -> &CompoundSelector {
        // Invariant: parse_selector never produces an empty compound list.
        &self.compounds[self.compounds.len() - 1]
    }
}

/// How deep `:not()`/`:is()`/`:where()` arguments may nest; deeper
/// selectors are rejected (the containing rule is dropped).
const MAX_PSEUDO_NESTING: usize = 32;

/// Parses one complex selector (no commas). Returns `None` if any part is
/// unsupported or malformed.
#[must_use]
pub fn parse_selector(source: &str) -> Option<Selector> {
    let selectors = parse_selector_list(source)?;
    match selectors.len() {
        1 => selectors.into_iter().next(),
        _ => None,
    }
}

/// Parses a comma-separated selector list with the lightningcss selector
/// parser and maps it onto the engine's model. Returns `None` when any
/// selector in the list is malformed or unsupported — the containing rule
/// is then dropped whole, matching browser behavior for invalid lists.
pub(crate) fn parse_selector_list(source: &str) -> Option<Vec<Selector>> {
    let mut input = ParserInput::new(source);
    let mut input = Parser::new(&mut input);
    let list = LwcSelectorList::parse_with_options(&mut input, &ParserOptions::default()).ok()?;
    input.expect_exhausted().ok()?;
    list.0
        .iter()
        .map(|selector| convert_selector(selector, 0))
        .collect()
}

/// Maps one lightningcss selector onto our model. Lightningcss iterates
/// compounds subject-first, so the collected vectors are reversed at the
/// end to restore source order.
fn convert_selector(selector: &LwcSelector, depth: usize) -> Option<Selector> {
    if depth >= MAX_PSEUDO_NESTING {
        return None;
    }
    let mut compounds = Vec::new();
    let mut combinators = Vec::new();
    let mut current = CompoundSelector::default();
    for component in selector.iter_raw_match_order() {
        if let Component::Combinator(combinator) = component {
            // The dummy combinator lightningcss puts between a
            // pseudo-element and the rest of its compound: not a real
            // compound boundary.
            if matches!(combinator, ParcelCombinator::PseudoElement) {
                continue;
            }
            compounds.push(std::mem::take(&mut current));
            combinators.push(match combinator {
                ParcelCombinator::Child => Combinator::Child,
                ParcelCombinator::Descendant => Combinator::Descendant,
                ParcelCombinator::NextSibling => Combinator::NextSibling,
                ParcelCombinator::LaterSibling => Combinator::SubsequentSibling,
                // Shadow-piercing and pseudo-element combinators are unsupported.
                _ => return None,
            });
            continue;
        }
        convert_component(component, &mut current, depth)?;
    }
    compounds.push(current);
    compounds.reverse();
    combinators.reverse();
    // A pseudo-element is only valid on the subject.
    if compounds[..compounds.len() - 1]
        .iter()
        .any(|compound| compound.pseudo_element.is_some())
    {
        return None;
    }
    Some(Selector {
        compounds,
        combinators,
    })
}

/// Folds one simple selector into the compound under construction.
/// Returns `None` for anything the engine cannot match.
fn convert_component(
    component: &Component,
    compound: &mut CompoundSelector,
    depth: usize,
) -> Option<()> {
    match component {
        Component::Combinator(_) => unreachable!("handled by convert_selector"),
        Component::ExplicitUniversalType => Some(()),
        Component::LocalName(local_name) => {
            if compound.tag.is_some() {
                return None;
            }
            compound.tag = Some(local_name.lower_name.0.to_string());
            Some(())
        }
        Component::ID(identifier) => {
            if compound.id.is_some() {
                return None;
            }
            compound.id = Some(identifier.0.to_string());
            Some(())
        }
        Component::Class(identifier) => {
            compound.classes.push(identifier.0.to_string());
            Some(())
        }
        Component::AttributeInNoNamespaceExists {
            local_name_lower, ..
        } => {
            compound.attributes.push(AttributeSelector {
                name: local_name_lower.0.to_string(),
                operation: AttributeOperation::Exists,
            });
            Some(())
        }
        Component::AttributeInNoNamespace {
            local_name,
            operator,
            value,
            case_sensitivity,
            ..
        } => {
            // The engine matches case-sensitively; an explicit `i` flag
            // could not be honored, so such selectors are dropped instead
            // of matching wrongly.
            if matches!(case_sensitivity, ParsedCaseSensitivity::AsciiCaseInsensitive) {
                return None;
            }
            let value = value.0.to_string();
            let operation = match operator {
                AttrSelectorOperator::Equal => AttributeOperation::Equals(value),
                AttrSelectorOperator::Prefix => AttributeOperation::StartsWith(value),
                AttrSelectorOperator::Suffix => AttributeOperation::EndsWith(value),
                AttrSelectorOperator::Substring => AttributeOperation::Contains(value),
                AttrSelectorOperator::Includes => AttributeOperation::WordMatch(value),
                AttrSelectorOperator::DashMatch => AttributeOperation::LangPrefix(value),
            };
            compound.attributes.push(AttributeSelector {
                name: local_name.0.to_string(),
                operation,
            });
            Some(())
        }
        Component::Root => {
            compound.pseudo_classes.push(PseudoClass::Root);
            Some(())
        }
        Component::Nth(data) => {
            compound.pseudo_classes.push(convert_nth(data)?);
            Some(())
        }
        Component::NonTSPseudoClass(pseudo_class) => {
            let pseudo_class = match pseudo_class {
                LwcPseudoClass::Link => PseudoClass::Link,
                LwcPseudoClass::Visited => PseudoClass::Visited,
                LwcPseudoClass::Hover => PseudoClass::Hover,
                LwcPseudoClass::Active => PseudoClass::Active,
                LwcPseudoClass::Checked => PseudoClass::Checked,
                LwcPseudoClass::Focus | LwcPseudoClass::FocusVisible => PseudoClass::Focus,
                LwcPseudoClass::FocusWithin => PseudoClass::FocusWithin,
                _ => return None,
            };
            compound.pseudo_classes.push(pseudo_class);
            Some(())
        }
        Component::Negation(list) => {
            let [inner] = &list[..] else {
                // Our model negates exactly one compound.
                return None;
            };
            compound
                .pseudo_classes
                .push(PseudoClass::Not(Box::new(single_compound(inner, depth)?)));
            Some(())
        }
        Component::Is(list) => {
            compound
                .pseudo_classes
                .push(PseudoClass::Is(compound_list(list, depth)?));
            Some(())
        }
        Component::Where(list) => {
            compound
                .pseudo_classes
                .push(PseudoClass::Where(compound_list(list, depth)?));
            Some(())
        }
        Component::PseudoElement(pseudo_element) => {
            let name = match pseudo_element {
                LwcPseudoElement::Before => "before",
                LwcPseudoElement::After => "after",
                LwcPseudoElement::Selection(_) => "selection",
                _ => return None,
            };
            compound.pseudo_element = Some(name.to_string());
            Some(())
        }
        // Namespaces, `:has()`, `:nth-child(.. of ..)`, shadow parts,
        // `:empty`, `:scope`, the nesting selector and everything else the
        // engine cannot match.
        _ => None,
    }
}

/// `:is()`/`:where()` arguments: every alternative must be a single
/// compound with no pseudo-element.
fn compound_list(list: &[LwcSelector], depth: usize) -> Option<Vec<CompoundSelector>> {
    if list.is_empty() {
        return None;
    }
    list.iter()
        .map(|selector| single_compound(selector, depth))
        .collect()
}

/// Maps a nested selector that must consist of exactly one compound
/// (the `:not()`/`:is()`/`:where()` arguments our model supports).
fn single_compound(selector: &LwcSelector, depth: usize) -> Option<CompoundSelector> {
    let selector = convert_selector(selector, depth + 1)?;
    if !selector.combinators.is_empty() {
        return None;
    }
    let compound = selector.compounds.into_iter().next()?;
    if compound.pseudo_element.is_some() {
        return None;
    }
    Some(compound)
}

/// Maps structural pseudo-class data. Lightningcss represents the keyword
/// forms (`:first-child`, `:only-of-type`, ...) as `an+b` data too, so the
/// dedicated variants are recovered from the (type, a, b) triple.
fn convert_nth(data: &NthSelectorData) -> Option<PseudoClass> {
    let pseudo_class = match (data.ty, data.a, data.b) {
        (NthType::Child, 0, 1) => PseudoClass::FirstChild,
        (NthType::LastChild, 0, 1) => PseudoClass::LastChild,
        (NthType::OnlyChild, 0, 1) => PseudoClass::OnlyChild,
        (NthType::OfType, 0, 1) => PseudoClass::FirstOfType,
        (NthType::LastOfType, 0, 1) => PseudoClass::LastOfType,
        (NthType::OnlyOfType, 0, 1) => PseudoClass::OnlyOfType,
        (NthType::Child, a, b) => PseudoClass::NthChild(a, b),
        (NthType::LastChild, a, b) => PseudoClass::NthLastChild(a, b),
        (NthType::OfType, a, b) => PseudoClass::NthOfType(a, b),
        (NthType::LastOfType, a, b) => PseudoClass::NthLastOfType(a, b),
        // Table column selectors are unsupported.
        _ => return None,
    };
    Some(pseudo_class)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compound(tag: Option<&str>, id: Option<&str>, classes: &[&str]) -> CompoundSelector {
        CompoundSelector {
            tag: tag.map(str::to_string),
            id: id.map(str::to_string),
            classes: classes.iter().map(|class| (*class).to_string()).collect(),
            attributes: Vec::new(),
            pseudo_classes: Vec::new(),
            pseudo_element: None,
        }
    }

    #[test]
    fn parses_simple_selectors() {
        assert_eq!(
            parse_selector("div").unwrap().compounds,
            vec![compound(Some("div"), None, &[])]
        );
        assert_eq!(
            parse_selector(".card").unwrap().compounds,
            vec![compound(None, None, &["card"])]
        );
        assert_eq!(
            parse_selector("#header").unwrap().compounds,
            vec![compound(None, Some("header"), &[])]
        );
        assert_eq!(
            parse_selector("*").unwrap().compounds,
            vec![compound(None, None, &[])]
        );
    }

    #[test]
    fn parses_compound_selector() {
        assert_eq!(
            parse_selector("div.card.active#main").unwrap().compounds,
            vec![compound(Some("div"), Some("main"), &["card", "active"])]
        );
    }

    #[test]
    fn parses_descendant_selector() {
        let selector = parse_selector(".card  p").unwrap();
        assert_eq!(
            selector.compounds,
            vec![
                compound(None, None, &["card"]),
                compound(Some("p"), None, &[])
            ]
        );
        assert_eq!(selector.combinators, vec![Combinator::Descendant]);
    }

    #[test]
    fn parses_child_and_sibling_combinators() {
        let selector = parse_selector("ul > li + li ~ b").unwrap();
        assert_eq!(selector.compounds.len(), 4);
        assert_eq!(
            selector.combinators,
            vec![
                Combinator::Child,
                Combinator::NextSibling,
                Combinator::SubsequentSibling
            ]
        );
        // Whitespace around the symbol is optional.
        assert_eq!(
            parse_selector("ul>li").unwrap().combinators,
            vec![Combinator::Child]
        );
    }

    #[test]
    fn parses_attribute_selectors() {
        let selector = parse_selector("a[href]").unwrap();
        assert_eq!(
            selector.compounds[0].attributes,
            vec![AttributeSelector {
                name: "href".to_string(),
                operation: AttributeOperation::Exists,
            }]
        );
        let selector = parse_selector("input[type=\"text\"]").unwrap();
        assert_eq!(
            selector.compounds[0].attributes[0].operation,
            AttributeOperation::Equals("text".to_string())
        );
        let selector = parse_selector("a[href^='https']").unwrap();
        assert_eq!(
            selector.compounds[0].attributes[0].operation,
            AttributeOperation::StartsWith("https".to_string())
        );
        assert!(parse_selector("a[href$=\".pdf\"]").is_some());
        assert!(parse_selector("a[href*=example]").is_some());
    }

    #[test]
    fn parses_structural_pseudo_classes() {
        assert_eq!(
            parse_selector("li:first-child").unwrap().compounds[0].pseudo_classes,
            vec![PseudoClass::FirstChild]
        );
        assert_eq!(
            parse_selector("li:nth-child(2n+1)").unwrap().compounds[0].pseudo_classes,
            vec![PseudoClass::NthChild(2, 1)]
        );
        assert_eq!(
            parse_selector("li:nth-child(odd)").unwrap().compounds[0].pseudo_classes,
            vec![PseudoClass::NthChild(2, 1)]
        );
        assert_eq!(
            parse_selector("li:nth-child(3)").unwrap().compounds[0].pseudo_classes,
            vec![PseudoClass::NthChild(0, 3)]
        );
        assert_eq!(
            parse_selector("li:nth-last-child(-n+2)").unwrap().compounds[0].pseudo_classes,
            vec![PseudoClass::NthLastChild(-1, 2)]
        );
    }

    #[test]
    fn parses_is_where_and_of_type() {
        let selector = parse_selector("p:is(.a, #b)").unwrap();
        let PseudoClass::Is(arguments) = &selector.compounds[0].pseudo_classes[0] else {
            panic!("expected :is");
        };
        assert_eq!(arguments.len(), 2);
        // :is takes its most specific argument: the id.
        assert_eq!(
            selector.specificity(),
            Specificity {
                ids: 1,
                classes: 0,
                types: 1
            }
        );
        // :where contributes nothing.
        assert_eq!(
            parse_selector("p:where(.a, #b)").unwrap().specificity(),
            Specificity {
                ids: 0,
                classes: 0,
                types: 1
            }
        );
        assert!(parse_selector("li:first-of-type").is_some());
        assert!(parse_selector("li:nth-of-type(2n)").is_some());
        assert!(parse_selector("a[rel~=nofollow]").is_some());
        assert!(parse_selector("p[lang|=en]").is_some());
    }

    #[test]
    fn parses_not_with_compound_argument() {
        let selector = parse_selector("p:not(.muted)").unwrap();
        let PseudoClass::Not(inner) = &selector.compounds[0].pseudo_classes[0] else {
            panic!("expected :not");
        };
        assert_eq!(inner.classes, vec!["muted"]);
        // :not() takes its argument's specificity: type + class.
        assert_eq!(
            selector.specificity(),
            Specificity {
                ids: 0,
                classes: 1,
                types: 1
            }
        );
    }

    #[test]
    fn uppercase_tag_is_normalized() {
        assert_eq!(
            parse_selector("DIV").unwrap().compounds,
            vec![compound(Some("div"), None, &[])]
        );
    }

    #[test]
    fn supported_pseudo_classes_match_and_add_specificity() {
        let selector = parse_selector("a:link").unwrap();
        assert_eq!(selector.compounds[0].tag.as_deref(), Some("a"));
        assert_eq!(
            selector.compounds[0].pseudo_classes,
            vec![PseudoClass::Link]
        );
        assert_eq!(
            selector.specificity(),
            Specificity {
                ids: 0,
                classes: 1,
                types: 1
            }
        );
        assert!(parse_selector("a:visited").is_some());
        assert!(parse_selector("a:hover").is_some());
        assert!(parse_selector(".btn:hover").is_some());
    }

    #[test]
    fn selection_pseudo_element_parses_with_type_specificity() {
        let selector = parse_selector("p::selection").unwrap();
        assert_eq!(
            selector.compounds[0].pseudo_element.as_deref(),
            Some("selection")
        );
        assert_eq!(
            selector.specificity(),
            Specificity {
                ids: 0,
                classes: 0,
                types: 2
            }
        );
    }

    #[test]
    fn before_and_after_parse_in_both_colon_forms() {
        for source in ["p::before", "p:before", "p::after", "p:after"] {
            let selector = parse_selector(source).unwrap();
            let pseudo = selector.compounds[0].pseudo_element.as_deref().unwrap();
            assert!(matches!(pseudo, "before" | "after"), "{source}");
        }
    }

    #[test]
    fn rejects_unsupported_selectors() {
        assert!(parse_selector("").is_none());
        // Bare pseudo-classes are valid selectors now.
        assert!(parse_selector(":link").is_some());
        assert!(parse_selector(":blur").is_none()); // Unsupported pseudo.
        assert!(parse_selector(".").is_none());
        assert!(parse_selector("#").is_none());
        assert!(parse_selector("div..x").is_none());
        // Bare ::selection is the universal selector's selection.
        assert!(parse_selector("::selection").is_some());
        assert!(parse_selector("p >").is_none()); // Trailing combinator.
        assert!(parse_selector("> p").is_none()); // Leading combinator.
        assert!(parse_selector("a > > b").is_none()); // Doubled.
        assert!(parse_selector("p:nth-child(x)").is_none());
        assert!(parse_selector("p:has(a)").is_none());
        assert!(parse_selector("p::selection span").is_none()); // Non-subject pseudo-element.
    }

    #[test]
    fn specificity_is_structural() {
        assert_eq!(
            parse_selector("#a").unwrap().specificity(),
            Specificity {
                ids: 1,
                classes: 0,
                types: 0
            }
        );
        assert_eq!(
            parse_selector("div.card p").unwrap().specificity(),
            Specificity {
                ids: 0,
                classes: 1,
                types: 2
            }
        );
        assert_eq!(
            parse_selector("a[href]:first-child").unwrap().specificity(),
            Specificity {
                ids: 0,
                classes: 2,
                types: 1
            }
        );
        assert_eq!(
            parse_selector("*").unwrap().specificity(),
            Specificity::default()
        );
    }

    #[test]
    fn id_beats_any_number_of_classes() {
        let id = parse_selector("#a").unwrap().specificity();
        let classes = parse_selector(".a.b.c.d.e").unwrap().specificity();
        assert!(id > classes);
    }

    #[test]
    fn class_beats_any_number_of_types() {
        let class = parse_selector(".a").unwrap().specificity();
        let types = parse_selector("html body div p").unwrap().specificity();
        assert!(class > types);
    }

    #[test]
    fn deeply_nested_functional_pseudos_are_rejected() {
        // Nesting within the limit still parses and computes specificity.
        let ok = format!("p{}.a{}", ":not(".repeat(8), ")".repeat(8));
        let selector = parse_selector(&ok).unwrap();
        assert_eq!(
            selector.specificity(),
            Specificity {
                ids: 0,
                classes: 1,
                types: 1
            }
        );
        // Past the limit the selector (and so its rule) is dropped instead
        // of recursing without bound.
        let deep = format!(
            "p{}.a{}",
            ":not(".repeat(MAX_PSEUDO_NESTING + 8),
            ")".repeat(MAX_PSEUDO_NESTING + 8)
        );
        assert!(parse_selector(&deep).is_none());
    }

    #[test]
    fn huge_selector_specificity_does_not_overflow() {
        // Regression: specificity fields were u16 and overflowed here.
        let selector = parse_selector(&format!("p{}", ":hover".repeat(70_000))).unwrap();
        assert_eq!(
            selector.specificity(),
            Specificity {
                ids: 0,
                classes: 70_000,
                types: 1
            }
        );
    }
}
