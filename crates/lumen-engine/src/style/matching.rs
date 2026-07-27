//! Selector matching: `parcel_selectors` evaluates parsed selectors
//! against a DOM wrapper ([`DomElement`] implements its `Element`
//! trait). The one gap upstream is `:has()` — parcel parses it but its
//! matcher leaves it `unreachable!()`, so selectors using `:has()` are
//! *prepared* here: the selector is decomposed into compounds, each
//! compound's `:has()` components are cut (via a serialize/re-parse
//! round-trip) and evaluated as separate clauses against the element
//! that compound matches.
//!
//! Supported `:has()` forms: top-level in any compound (so
//! `.a:has(.b) .c:has(.d)` works), with inner relative selectors that
//! may themselves contain `:has()` (handled by recursion). `:has()`
//! nested in `:not()`/`:is()`/... is unsupported and never matches,
//! as does any pseudo-element inside `:has()` — per spec that
//! invalidates the selector, so the rule is kept but inert.

use super::interaction::InteractionState;
use lumen_css::selector::{PseudoClass, Selector, Selectors};
use lumen_html::{Document, ElementData, NodeId, NodeKind};
use parcel_selectors::attr::{AttrSelectorOperation, CaseSensitivity, NamespaceConstraint};
use parcel_selectors::context::{MatchingContext, MatchingMode, QuirksMode};
use parcel_selectors::matching::{ElementSelectorFlags, matches_selector};
use parcel_selectors::parser::{Combinator, Component, SelectorImpl, SelectorList};
use parcel_selectors::{Element, OpaqueElement};
use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

/// Per-style-pass cache of pre-split class attributes. Parcel's matcher
/// asks [`DomElement::has_class`] once per candidate selector per
/// element, and re-splitting the `class` attribute on every call
/// dominated profiles on class-heavy pages.
pub(crate) type ClassCache = RefCell<super::FxHashMap<NodeId, Rc<[Box<str>]>>>;

/// The element's class list, split once per node per style pass.
pub(crate) fn cached_classes(
    document: &Document,
    cache: &ClassCache,
    node: NodeId,
) -> Rc<[Box<str>]> {
    if let Some(classes) = cache.borrow().get(&node) {
        return Rc::clone(classes);
    }
    let classes: Rc<[Box<str>]> = document
        .element(node)
        .map(|element| element.classes().map(Box::from).collect())
        .unwrap_or_default();
    cache.borrow_mut().insert(node, Rc::clone(&classes));
    classes
}

/// The bucket a selector belongs to, derived from its subject
/// (rightmost) compound's positively-required id/class/tag. Combinators
/// only constrain relatives of the subject, so the subject compound
/// alone decides which elements can ever match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BucketKey {
    Id(Box<str>),
    Class(Box<str>),
    Tag(Box<str>),
    /// No positively-required key: universal, attribute- or
    /// pseudo-class-only subjects, and keys hidden inside
    /// `:is()`/`:not()`/... (conservatively tried against everything).
    Always,
}

/// Extracts the bucket key from a selector's subject compound. Only
/// top-level `ID`/`Class`/`LocalName` components count: each of them
/// must match for the compound to match, so any one is a sound
/// pre-filter. Components nested in `:is()`/`:not()`/`:has()` are
/// alternatives, negations or separate clauses and do not qualify.
pub(crate) fn subject_key(selector: &Selector) -> BucketKey {
    let mut tag = None;
    let mut class = None;
    let mut id = None;
    for component in selector.iter_raw_match_order() {
        match component {
            // The dummy combinator between a pseudo-element and the rest
            // of its compound is not a compound boundary.
            Component::Combinator(Combinator::PseudoElement) => {}
            Component::Combinator(_) => break,
            Component::ID(name) => id = Some(name.as_str()),
            Component::Class(name) => {
                if class.is_none() {
                    class = Some(name.as_str());
                }
            }
            // HTML matching compares the lowercased name.
            Component::LocalName(name) => tag = Some(name.lower_name.as_str()),
            _ => {}
        }
    }
    if let Some(id) = id {
        BucketKey::Id(Box::from(id))
    } else if let Some(class) = class {
        BucketKey::Class(Box::from(class))
    } else if let Some(tag) = tag {
        BucketKey::Tag(Box::from(tag))
    } else {
        BucketKey::Always
    }
}

/// A DOM element presented to parcel's matcher, carrying the document
/// and interaction state its pseudo-class answers depend on.
#[derive(Clone)]
struct DomElement<'a> {
    document: &'a Document,
    interaction: &'a InteractionState,
    class_cache: &'a ClassCache,
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
        cached_classes(self.document, self.class_cache, self.node)
            .iter()
            .any(|class| match case_sensitivity {
                CaseSensitivity::CaseSensitive => class.as_ref() == name.as_str(),
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
    class_cache: &ClassCache,
    node: NodeId,
    selector: &Selector,
) -> bool {
    let element = DomElement {
        document,
        interaction,
        class_cache,
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

/// One compound of a decomposed selector.
#[derive(Debug, Clone)]
struct PreparedCompound {
    /// The combinator linking the previous compound (nearer the
    /// subject) to this one; `None` on the subject compound.
    combinator: Option<Combinator>,
    /// The compound with its `:has()` components removed, matched by
    /// parcel. `None` only for the anchor compound ending a `:has()`
    /// clause chain: it matches exactly the scoped element.
    selector: Option<Selector>,
    /// `:has()` clauses attached to this compound, evaluated against
    /// the element the compound matches.
    clauses: Vec<HasClause>,
}

/// A selector decomposed into compounds so each compound's `:has()`
/// clauses run against the element that compound matches.
#[derive(Debug, Clone)]
pub(crate) struct PreparedChain {
    /// Compounds in match order: the subject first, leftmost last.
    compounds: Vec<PreparedCompound>,
}

/// One `:has(...)` argument: an inner selector chain whose last
/// compound is the anchor (linked by the leading combinator, matching
/// only the element `:has()` is evaluated on).
#[derive(Debug, Clone)]
pub(crate) struct HasClause {
    inner: PreparedChain,
}

/// How to match a selector that may contain `:has()`.
#[derive(Debug, Clone)]
pub(crate) enum PreparedSelector {
    /// No `:has()` anywhere: match the selector as-is.
    Plain,
    /// Supported `:has()` use: walk the decomposed chain.
    Chain(PreparedChain),
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
    let mut top_level_has = false;
    for component in selector.iter_raw_match_order() {
        match component {
            Component::Has(_) => top_level_has = true,
            // A `:has()` nested in :not()/:is()/... is unsupported.
            component if component_has_has(component) => {
                return PreparedSelector::Never;
            }
            _ => {}
        }
    }
    if !top_level_has {
        return PreparedSelector::Plain;
    }
    match prepare_chain(selector) {
        Some(chain) => PreparedSelector::Chain(chain),
        None => PreparedSelector::Never,
    }
}

/// Decomposes a selector into per-compound pruned selectors plus their
/// `:has()` clauses. `None` for unsupported forms: `:has()` nested in
/// `:not()`/`:is()`/... or a pseudo-element inside a `:has()` argument.
fn prepare_chain(selector: &Selector) -> Option<PreparedChain> {
    // Split into compounds in match order, collecting each compound's
    // top-level `:has()` argument lists and the combinators between.
    let mut has_lists: Vec<Vec<&[Selector]>> = vec![Vec::new()];
    let mut combinators: Vec<Combinator> = Vec::new();
    for component in selector.iter_raw_match_order() {
        match component {
            // The dummy combinator between a pseudo-element and the rest
            // of its compound: not a compound boundary.
            Component::Combinator(Combinator::PseudoElement) => {}
            Component::Combinator(combinator) => {
                combinators.push(*combinator);
                has_lists.push(Vec::new());
            }
            Component::Has(list) => has_lists.last_mut()?.push(list),
            component if component_has_has(component) => return None,
            _ => {}
        }
    }
    // Serialize, split at top-level combinators, and strip each piece's
    // top-level `:has(...)` spans; pieces come out in source order.
    let mut pieces = split_compound_pieces(&selector_to_css(selector));
    pieces.reverse();
    if pieces.len() != has_lists.len() {
        return None;
    }
    let mut compounds = Vec::with_capacity(pieces.len());
    for (index, (piece, lists)) in pieces.into_iter().zip(has_lists).enumerate() {
        // A compound that held only `:has(...)` spans prunes to `*`.
        let pruned =
            lumen_css::selector::parse_selector(if piece.is_empty() { "*" } else { &piece })?;
        let mut clauses = Vec::new();
        for list in lists {
            for inner in list {
                clauses.push(extract_clause(inner)?);
            }
        }
        compounds.push(PreparedCompound {
            combinator: (index > 0).then(|| combinators[index - 1]),
            selector: Some(pruned),
            clauses,
        });
    }
    Some(PreparedChain { compounds })
}

/// Reduces one relative selector inside `:has(...)` to a clause: the
/// recursively prepared inner chain plus a final anchor compound linked
/// by the leading combinator (`:has(img)` is implicit-descendant).
fn extract_clause(inner: &Selector) -> Option<HasClause> {
    // Pseudo-elements inside `:has()` invalidate the selector per spec.
    if inner.has_pseudo_element() {
        return None;
    }
    let text = selector_to_css(inner);
    let mut rest = text.trim();
    if let Some(stripped) = rest.strip_prefix(":scope") {
        rest = stripped.trim_start();
    }
    let (combinator, rest) = match rest.as_bytes().first() {
        Some(b'>') => (Combinator::Child, rest[1..].trim_start()),
        Some(b'+') => (Combinator::NextSibling, rest[1..].trim_start()),
        Some(b'~') => (Combinator::LaterSibling, rest[1..].trim_start()),
        _ => (Combinator::Descendant, rest),
    };
    let inner = lumen_css::selector::parse_selector(rest)?;
    if inner.has_pseudo_element() {
        return None;
    }
    let mut chain = prepare_chain(&inner)?;
    chain.compounds.push(PreparedCompound {
        combinator: Some(combinator),
        selector: None,
        clauses: Vec::new(),
    });
    Some(HasClause { inner: chain })
}

/// Whether `character` ends a compound at the top level of a serialized
/// selector (combinator glyphs and the whitespace around them).
fn is_separator(character: char) -> bool {
    matches!(
        character,
        ' ' | '\t' | '\n' | '\x0C' | '\r' | '>' | '+' | '~'
    )
}

/// Consumes the rest of a balanced `(...)` span whose open paren was
/// already consumed, quote- and escape-aware.
fn skip_paren_span(chars: &mut std::iter::Peekable<std::str::CharIndices>) {
    let mut depth = 1i32;
    let mut in_string: Option<char> = None;
    let mut escaped = false;
    for (_, character) in chars.by_ref() {
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
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
    }
}

/// Splits canonically serialized selector text at top-level combinators
/// into compound pieces in source order, cutting every top-level
/// `:has(...)` span. Quote- and bracket-aware so attribute values like
/// `[title=":has("]` survive.
fn split_compound_pieces(text: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::with_capacity(text.len());
    let mut parens = 0i32;
    let mut brackets = 0i32;
    let mut in_string: Option<char> = None;
    let mut escaped = false;
    let mut chars = text.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        if let Some(quote) = in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == quote {
                in_string = None;
            }
            current.push(character);
            continue;
        }
        match character {
            '"' | '\'' => {
                in_string = Some(character);
                current.push(character);
            }
            '(' if parens == 0 && brackets == 0 && text[..index].ends_with(":has") => {
                // Cut the whole top-level `:has(...)` span.
                current.truncate(current.len() - ":has".len());
                skip_paren_span(&mut chars);
            }
            '(' => {
                parens += 1;
                current.push(character);
            }
            ')' => {
                parens -= 1;
                current.push(character);
            }
            '[' => {
                brackets += 1;
                current.push(character);
            }
            ']' => {
                brackets -= 1;
                current.push(character);
            }
            character if is_separator(character) && parens == 0 && brackets == 0 => {
                // A combinator run (glyph plus surrounding whitespace)
                // ends the current compound piece.
                pieces.push(std::mem::take(&mut current));
                while chars.peek().is_some_and(|&(_, next)| is_separator(next)) {
                    chars.next();
                }
            }
            _ => current.push(character),
        }
    }
    pieces.push(current);
    pieces
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

/// The element parent of `node`, skipping non-element parents.
fn parent_element(document: &Document, node: NodeId) -> Option<NodeId> {
    document
        .parent(node)
        .filter(|parent| document.element(*parent).is_some())
}

/// Element siblings preceding `node`, nearest first.
fn preceding_element_siblings(document: &Document, node: NodeId) -> Vec<NodeId> {
    let Some(parent) = document.parent(node) else {
        return Vec::new();
    };
    let siblings = document.children(parent);
    let Some(position) = siblings.iter().position(|sibling| *sibling == node) else {
        return Vec::new();
    };
    siblings[..position]
        .iter()
        .rev()
        .copied()
        .filter(|sibling| document.element(*sibling).is_some())
        .collect()
}

/// Whether `chain`'s compound at `index` and everything left of it
/// match, with `node` as the element for that compound. `scope` is the
/// element a `:has()` clause is anchored to: the chain's final
/// `selector: None` compound matches only that node, which keeps the
/// ancestor walk inside the anchor's subtree.
fn chain_matches(
    document: &Document,
    interaction: &InteractionState,
    class_cache: &ClassCache,
    node: NodeId,
    chain: &PreparedChain,
    index: usize,
    scope: Option<NodeId>,
) -> bool {
    let compound = &chain.compounds[index];
    let matches = match &compound.selector {
        Some(selector) => {
            parcel_matches(document, interaction, class_cache, node, selector)
                && compound
                    .clauses
                    .iter()
                    .all(|clause| clause_matches(document, interaction, class_cache, node, clause))
        }
        None => scope == Some(node),
    };
    if !matches {
        return false;
    }
    let Some(next) = chain.compounds.get(index + 1) else {
        return true;
    };
    match next.combinator {
        Some(Combinator::Child) => parent_element(document, node).is_some_and(|parent| {
            chain_matches(
                document,
                interaction,
                class_cache,
                parent,
                chain,
                index + 1,
                scope,
            )
        }),
        Some(Combinator::Descendant) => {
            let mut ancestor = parent_element(document, node);
            while let Some(node) = ancestor {
                if chain_matches(
                    document,
                    interaction,
                    class_cache,
                    node,
                    chain,
                    index + 1,
                    scope,
                ) {
                    return true;
                }
                ancestor = parent_element(document, node);
            }
            false
        }
        Some(Combinator::NextSibling) => preceding_element_siblings(document, node)
            .first()
            .is_some_and(|first| {
                chain_matches(
                    document,
                    interaction,
                    class_cache,
                    *first,
                    chain,
                    index + 1,
                    scope,
                )
            }),
        Some(Combinator::LaterSibling) => preceding_element_siblings(document, node)
            .into_iter()
            .any(|sibling| {
                chain_matches(
                    document,
                    interaction,
                    class_cache,
                    sibling,
                    chain,
                    index + 1,
                    scope,
                )
            }),
        _ => false,
    }
}

/// Whether one `:has()` clause holds for `node`: some element related
/// to `node` the way the leading combinator describes must satisfy the
/// inner chain anchored at `node`.
fn clause_matches(
    document: &Document,
    interaction: &InteractionState,
    class_cache: &ClassCache,
    node: NodeId,
    clause: &HasClause,
) -> bool {
    let anchor = clause
        .inner
        .compounds
        .last()
        .expect("clause chains end in an anchor compound");
    let candidate_matches = |candidate: NodeId| {
        document.element(candidate).is_some()
            && chain_matches(
                document,
                interaction,
                class_cache,
                candidate,
                &clause.inner,
                0,
                Some(node),
            )
    };
    match anchor.combinator {
        // The inner subject sits somewhere in `node`'s subtree; the
        // anchor compound pins the walk to it.
        Some(Combinator::Descendant) | Some(Combinator::Child) => {
            document.descendants(node).any(candidate_matches)
        }
        Some(Combinator::NextSibling) => match following_element_siblings(document, node).first() {
            Some(first) => std::iter::once(*first)
                .chain(document.descendants(*first))
                .any(candidate_matches),
            None => false,
        },
        Some(Combinator::LaterSibling) => following_element_siblings(document, node)
            .into_iter()
            .flat_map(|sibling| std::iter::once(sibling).chain(document.descendants(sibling)))
            .any(candidate_matches),
        _ => false,
    }
}

/// Whether `selector` matches `node`, honoring the prepared `:has()`
/// clauses.
pub(crate) fn selector_matches(
    document: &Document,
    interaction: &InteractionState,
    class_cache: &ClassCache,
    node: NodeId,
    selector: &Selector,
    prepared: &PreparedSelector,
) -> bool {
    match prepared {
        PreparedSelector::Plain => {
            parcel_matches(document, interaction, class_cache, node, selector)
        }
        PreparedSelector::Chain(chain) => {
            chain_matches(document, interaction, class_cache, node, chain, 0, None)
        }
        PreparedSelector::Never => false,
    }
}
