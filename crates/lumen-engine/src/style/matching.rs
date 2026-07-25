//! Selector matching: `parcel_selectors` evaluates parsed selectors
//! against a DOM wrapper ([`DomElement`] implements its `Element`
//! trait). The one gap upstream is `:has()` — parcel parses it but its
//! matcher leaves it `unreachable!()`, so selectors using `:has()` are
//! *prepared* here: the `:has()` components are cut from the selector
//! (via a serialize/re-parse round-trip) and evaluated as separate
//! clauses over the element's descendants/children/siblings.
//!
//! Supported `:has()` forms: top-level in the subject compound, whose
//! inner relative selectors contain no nested `:has()` and no
//! pseudo-elements. Anything richer never matches (the rule is kept but
//! inert), matching how the engine treats other unsupported selectors.

use super::interaction::InteractionState;
use lumen_css::selector::{PseudoClass, Selector, Selectors};
use lumen_html::{Document, ElementData, NodeId, NodeKind};
use parcel_selectors::attr::{AttrSelectorOperation, CaseSensitivity, NamespaceConstraint};
use parcel_selectors::context::{MatchingContext, MatchingMode, QuirksMode};
use parcel_selectors::matching::{ElementSelectorFlags, matches_selector};
use parcel_selectors::parser::{Combinator, Component, SelectorImpl, SelectorList};
use parcel_selectors::{Element, OpaqueElement};
use std::fmt;

/// A DOM element presented to parcel's matcher, carrying the document
/// and interaction state its pseudo-class answers depend on.
#[derive(Clone)]
struct DomElement<'a> {
    document: &'a Document,
    interaction: &'a InteractionState,
    node: NodeId,
}

impl fmt::Debug for DomElement<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DomElement({})", self.node)
    }
}

impl<'a> DomElement<'a> {
    fn at(&self, node: NodeId) -> Self {
        Self { node, ..*self }
    }

    /// The element data; only constructed for element nodes.
    fn data(&self) -> &'a ElementData {
        self.document
            .element(self.node)
            .expect("DomElement wraps element nodes")
    }
}

/// Form controls `:enabled`/`:disabled` apply to.
const FORM_TAGS: [&str; 7] = [
    "button", "input", "select", "textarea", "optgroup", "option", "fieldset",
];

impl<'i> Element<'i> for DomElement<'_> {
    type Impl = Selectors;

    fn opaque(&self) -> OpaqueElement {
        OpaqueElement::new(&self.node)
    }

    fn parent_element(&self) -> Option<Self> {
        self.document
            .parent(self.node)
            .filter(|parent| self.document.element(*parent).is_some())
            .map(|parent| self.at(parent))
    }

    fn parent_node_is_shadow_root(&self) -> bool {
        false
    }

    fn containing_shadow_host(&self) -> Option<Self> {
        None
    }

    fn is_pseudo_element(&self) -> bool {
        false
    }

    fn prev_sibling_element(&self) -> Option<Self> {
        let parent = self.document.parent(self.node)?;
        let siblings = self.document.children(parent);
        let position = siblings.iter().position(|sibling| *sibling == self.node)?;
        siblings[..position]
            .iter()
            .rev()
            .find(|sibling| self.document.element(**sibling).is_some())
            .map(|sibling| self.at(*sibling))
    }

    fn next_sibling_element(&self) -> Option<Self> {
        let parent = self.document.parent(self.node)?;
        let siblings = self.document.children(parent);
        let position = siblings.iter().position(|sibling| *sibling == self.node)?;
        siblings[position + 1..]
            .iter()
            .find(|sibling| self.document.element(**sibling).is_some())
            .map(|sibling| self.at(*sibling))
    }

    fn is_html_element_in_html_document(&self) -> bool {
        true
    }

    fn has_local_name(&self, local_name: &str) -> bool {
        self.data().tag_name == local_name
    }

    fn has_namespace(&self, ns: &str) -> bool {
        // The DOM has no namespaces; only the empty one matches.
        ns.is_empty()
    }

    fn is_same_type(&self, other: &Self) -> bool {
        self.data().tag_name == other.data().tag_name
    }

    fn attr_matches(
        &self,
        _ns: &NamespaceConstraint<&lumen_css::selector::Ident>,
        local_name: &lumen_css::selector::Ident,
        operation: &AttrSelectorOperation<&lumen_css::selector::Ident>,
    ) -> bool {
        let Some(value) = self.data().attributes.get(local_name.as_str()) else {
            return false;
        };
        // parcel evaluates the operator, including the `[attr=v i]`
        // ASCII case-insensitivity flag.
        match operation {
            AttrSelectorOperation::Exists => true,
            with_value => with_value.eval_str(value),
        }
    }

    fn match_non_ts_pseudo_class<F>(
        &self,
        pc: &PseudoClass,
        _context: &mut MatchingContext<'_, 'i, Selectors>,
        _flags_setter: &mut F,
    ) -> bool
    where
        F: FnMut(&Self, ElementSelectorFlags),
    {
        let interaction = self.interaction;
        let element = self.data();
        match pc {
            PseudoClass::Hover => interaction.hover_chain.contains(&self.node),
            PseudoClass::Active => interaction.active_chain.contains(&self.node),
            PseudoClass::Focus => interaction.focused == Some(self.node),
            PseudoClass::FocusWithin => interaction.focus_chain.contains(&self.node),
            PseudoClass::Checked => interaction.checked.contains(&self.node),
            PseudoClass::Visited => interaction.visited_links.contains(&self.node),
            PseudoClass::Link => self.is_link() && !interaction.visited_links.contains(&self.node),
            PseudoClass::Enabled => {
                FORM_TAGS.contains(&element.tag_name.as_str())
                    && !element.attributes.contains("disabled")
            }
            PseudoClass::Disabled => {
                FORM_TAGS.contains(&element.tag_name.as_str())
                    && element.attributes.contains("disabled")
            }
        }
    }

    fn match_pseudo_element(
        &self,
        _pe: &lumen_css::selector::PseudoElement,
        _context: &mut MatchingContext<'_, 'i, Selectors>,
    ) -> bool {
        // Pseudo-elements are generated boxes, not DOM elements; the
        // pseudo pass matches their rules in ForStatelessPseudoElement
        // mode instead.
        false
    }

    fn is_link(&self) -> bool {
        self.data().attributes.contains("href")
    }

    fn is_html_slot_element(&self) -> bool {
        false
    }

    fn has_id(&self, id: &lumen_css::selector::Ident, case_sensitivity: CaseSensitivity) -> bool {
        let Some(actual) = self.data().id() else {
            return false;
        };
        match case_sensitivity {
            CaseSensitivity::CaseSensitive => actual == id.as_str(),
            CaseSensitivity::AsciiCaseInsensitive => actual.eq_ignore_ascii_case(id.as_str()),
        }
    }

    fn has_class(
        &self,
        name: &lumen_css::selector::Ident,
        case_sensitivity: CaseSensitivity,
    ) -> bool {
        self.data().classes().any(|class| match case_sensitivity {
            CaseSensitivity::CaseSensitive => class == name.as_str(),
            CaseSensitivity::AsciiCaseInsensitive => class.eq_ignore_ascii_case(name.as_str()),
        })
    }

    fn imported_part(
        &self,
        _name: &lumen_css::selector::Ident,
    ) -> Option<lumen_css::selector::Ident> {
        None
    }

    fn is_part(&self, _name: &lumen_css::selector::Ident) -> bool {
        false
    }

    fn is_empty(&self) -> bool {
        self.document.children(self.node).iter().all(|child| {
            match &self.document.node(*child).kind {
                NodeKind::Text(text) => text.is_empty(),
                NodeKind::Element(_) => false,
                NodeKind::Document => true,
            }
        })
    }

    fn is_root(&self) -> bool {
        self.document.parent(self.node) == Some(self.document.root())
    }
}

/// Runs parcel's matcher for `selector` against `node`.
fn parcel_matches(
    document: &Document,
    interaction: &InteractionState,
    node: NodeId,
    selector: &Selector,
) -> bool {
    let element = DomElement {
        document,
        interaction,
        node,
    };
    // Rules with a pseudo-element (`::before` etc.) match the
    // originating element; the pseudo-element itself is stateless.
    let mode = if selector.has_pseudo_element() {
        MatchingMode::ForStatelessPseudoElement
    } else {
        MatchingMode::Normal
    };
    let mut context = MatchingContext::new(mode, None, None, QuirksMode::NoQuirks);
    matches_selector(selector, 0, None, &element, &mut context, &mut |_, _| {})
}

/// The relation a `:has()` clause's inner selector has to the element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClauseCombinator {
    /// `:has(img)` — any descendant.
    Descendant,
    /// `:has(> img)` — direct children.
    Child,
    /// `:has(+ img)` — the immediately following element sibling.
    NextSibling,
    /// `:has(~ img)` — any following element sibling.
    LaterSibling,
}

/// One `:has(...)` argument, reduced to a combinator plus a plain
/// selector evaluated against candidate elements.
#[derive(Debug, Clone)]
pub(crate) struct HasClause {
    combinator: ClauseCombinator,
    inner: Selector,
}

/// How to match a selector that may contain `:has()`.
#[derive(Debug, Clone)]
pub(crate) enum PreparedSelector {
    /// No `:has()` anywhere: match the selector as-is.
    Plain,
    /// Supported `:has()` use: match `selector` (with the `:has()`
    /// components removed), then every clause.
    Pruned {
        selector: Selector,
        clauses: Vec<HasClause>,
    },
    /// An unsupported `:has()` form: never matches.
    Never,
}

/// Whether any component of `selector` (recursing into nested selector
/// lists) is a `:has()`.
fn component_has_has(component: &Component<'static, Selectors>) -> bool {
    match component {
        Component::Has(_) => true,
        Component::Negation(list)
        | Component::Is(list)
        | Component::Where(list)
        | Component::Any(_, list) => any_has(list),
        Component::NthOf(data) => any_has(data.selectors()),
        _ => false,
    }
}

fn any_has(list: &[Selector]) -> bool {
    list.iter()
        .any(|selector| selector.iter_raw_match_order().any(component_has_has))
}

/// Serializes a selector back to CSS text (used by the prune
/// round-trip).
fn selector_to_css(selector: &Selector) -> String {
    let list = SelectorList::from(selector.clone());
    let mut out = String::new();
    Selectors::to_css(&list, &mut out).expect("selector serialization cannot fail");
    out
}

/// Analyzes a selector for `:has()` support; see the module docs for
/// the supported forms. Computed once per style pass, not per element.
pub(crate) fn prepare_selector(selector: &Selector) -> PreparedSelector {
    let mut in_subject = true;
    let mut subject_has: Vec<&[Selector]> = Vec::new();
    for component in selector.iter_raw_match_order() {
        match component {
            // The dummy combinator between a pseudo-element and the rest
            // of its compound: not a compound boundary.
            Component::Combinator(Combinator::PseudoElement) => {}
            Component::Combinator(_) => in_subject = false,
            Component::Has(list) if in_subject => subject_has.push(list),
            component => {
                // A `:has()` nested in :not()/:is()/... or sitting in a
                // non-subject compound is unsupported.
                if component_has_has(component) {
                    return PreparedSelector::Never;
                }
            }
        }
    }
    if subject_has.is_empty() {
        return PreparedSelector::Plain;
    }
    // Cut the subject-level `:has(...)` spans and re-parse: the pruned
    // selector is plain parcel-matched.
    let pruned_text = strip_subject_has(&selector_to_css(selector));
    let Some(pruned) = lumen_css::selector::parse_selector(&pruned_text) else {
        return PreparedSelector::Never;
    };
    let mut clauses = Vec::new();
    for list in subject_has {
        for inner in list {
            match extract_clause(inner) {
                Some(clause) => clauses.push(clause),
                None => return PreparedSelector::Never,
            }
        }
    }
    PreparedSelector::Pruned {
        selector: pruned,
        clauses,
    }
}

/// Reduces one relative selector inside `:has(...)` to a clause.
/// `:has(img)` is implicit-descendant; an explicit leading combinator is
/// stored as a trailing `[Combinator, Scope]` pair in match order.
fn extract_clause(inner: &Selector) -> Option<HasClause> {
    if inner.has_pseudo_element() || any_has(std::slice::from_ref(inner)) {
        return None;
    }
    let text = selector_to_css(inner);
    let mut rest = text.trim();
    if let Some(stripped) = rest.strip_prefix(":scope") {
        rest = stripped.trim_start();
    }
    let (combinator, rest) = match rest.as_bytes().first() {
        Some(b'>') => (ClauseCombinator::Child, rest[1..].trim_start()),
        Some(b'+') => (ClauseCombinator::NextSibling, rest[1..].trim_start()),
        Some(b'~') => (ClauseCombinator::LaterSibling, rest[1..].trim_start()),
        _ => (ClauseCombinator::Descendant, rest),
    };
    let inner = lumen_css::selector::parse_selector(rest)?;
    if inner.has_pseudo_element() || any_has(std::slice::from_ref(&inner)) {
        return None;
    }
    Some(HasClause { combinator, inner })
}

/// Removes top-level `:has(...)` spans that sit in the subject (last)
/// compound of a canonically serialized selector. Quote- and
/// bracket-aware so attribute values like `[title=":has("]` survive.
fn strip_subject_has(text: &str) -> String {
    // Find where the subject compound starts: after the last top-level
    // combinator (whitespace, `>`, `+`, `~`) outside parens/brackets.
    let mut subject_start = 0;
    let mut parens = 0i32;
    let mut brackets = 0i32;
    let mut in_string: Option<char> = None;
    let mut escaped = false;
    for (index, character) in text.char_indices() {
        if let Some(quote) = in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == quote {
                in_string = None;
            }
            continue;
        }
        match character {
            '"' | '\'' => in_string = Some(character),
            '(' => parens += 1,
            ')' => parens -= 1,
            '[' => brackets += 1,
            ']' => brackets -= 1,
            '>' | '+' | '~' if parens == 0 && brackets == 0 => subject_start = index + 1,
            character if character.is_whitespace() && parens == 0 && brackets == 0 => {
                subject_start = index + 1;
            }
            _ => {}
        }
    }
    // Cut every top-level `:has(` span from the subject onward.
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..subject_start]);
    let rest = &text[subject_start..];
    let mut depth = 0i32;
    let mut in_string: Option<char> = None;
    let mut escaped = false;
    let mut copied = 0;
    for (index, character) in rest.char_indices() {
        if let Some(quote) = in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == quote {
                in_string = None;
            }
            continue;
        }
        match character {
            '"' | '\'' => in_string = Some(character),
            '[' | ']' => {}
            '(' => {
                if depth == 0 && index >= 4 && &rest[index - 4..index] == ":has" {
                    // Skip to the matching close paren.
                    out.push_str(&rest[copied..index - 4]);
                    let mut inner_depth = 1i32;
                    let mut end = index + ":has(".len();
                    let mut inner_string: Option<char> = None;
                    let mut inner_escaped = false;
                    for (inner_index, inner_char) in rest[index + 5..].char_indices() {
                        if let Some(quote) = inner_string {
                            if inner_escaped {
                                inner_escaped = false;
                            } else if inner_char == '\\' {
                                inner_escaped = true;
                            } else if inner_char == quote {
                                inner_string = None;
                            }
                            continue;
                        }
                        match inner_char {
                            '"' | '\'' => inner_string = Some(inner_char),
                            '(' => inner_depth += 1,
                            ')' => {
                                inner_depth -= 1;
                                if inner_depth == 0 {
                                    end = index + 5 + inner_index + 1;
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    copied = end;
                    continue;
                }
                depth += 1;
            }
            ')' => depth -= 1,
            _ => {}
        }
    }
    out.push_str(&rest[copied..]);
    out
}

/// Element siblings following `node`.
fn following_element_siblings(document: &Document, node: NodeId) -> Vec<NodeId> {
    let Some(parent) = document.parent(node) else {
        return Vec::new();
    };
    let siblings = document.children(parent);
    let Some(position) = siblings.iter().position(|sibling| *sibling == node) else {
        return Vec::new();
    };
    siblings[position + 1..]
        .iter()
        .copied()
        .filter(|sibling| document.element(*sibling).is_some())
        .collect()
}

/// Whether one `:has()` clause holds for `node`.
fn clause_matches(
    document: &Document,
    interaction: &InteractionState,
    node: NodeId,
    clause: &HasClause,
) -> bool {
    let candidate_matches = |candidate: NodeId| {
        document.element(candidate).is_some()
            && parcel_matches(document, interaction, candidate, &clause.inner)
    };
    match clause.combinator {
        ClauseCombinator::Descendant => document.descendants(node).any(candidate_matches),
        ClauseCombinator::Child => document
            .children(node)
            .iter()
            .copied()
            .any(candidate_matches),
        ClauseCombinator::NextSibling => following_element_siblings(document, node)
            .first()
            .is_some_and(|first| candidate_matches(*first)),
        ClauseCombinator::LaterSibling => following_element_siblings(document, node)
            .into_iter()
            .any(candidate_matches),
    }
}

/// Whether `selector` matches `node`, honoring the prepared `:has()`
/// clauses.
pub(crate) fn selector_matches(
    document: &Document,
    interaction: &InteractionState,
    node: NodeId,
    selector: &Selector,
    prepared: &PreparedSelector,
) -> bool {
    let (selector, clauses) = match prepared {
        PreparedSelector::Plain => (selector, &[][..]),
        PreparedSelector::Pruned { selector, clauses } => (selector, &clauses[..]),
        PreparedSelector::Never => return false,
    };
    parcel_matches(document, interaction, node, selector)
        && clauses
            .iter()
            .all(|clause| clause_matches(document, interaction, node, clause))
}
