//! Interaction state (`:hover`/`:active`/`:focus`/`:visited`/`:checked`)
//! and the damage ladder: how cheaply the page can react to an
//! interaction change (nothing / repaint / relayout).

use super::matching;
use lumen_css::Stylesheet;
use lumen_css::selector::{PseudoClass, Selector, Selectors};
use lumen_html::{Document, NodeId};
use parcel_selectors::parser::{Combinator, Component};
use std::collections::HashSet;

/// How `:hover` rules in a stylesheet can affect the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoverImpact {
    /// No `:hover` rules at all: hover changes need no work.
    Nothing,
    /// Hover rules only touch paint-level properties: restyle + repaint,
    /// no relayout.
    PaintOnly,
    /// At least one hover rule can change geometry: full relayout.
    Layout,
}

/// Properties that can change geometry (post-shorthand-expansion names).
/// Everything else — colors, decorations, opacity, cursor, unsupported
/// properties — only affects painting.
fn affects_layout(property: &str) -> bool {
    const LAYOUT_PREFIXES: [&str; 9] = [
        "margin", "padding", "border-", "flex", "min-", "max-", "align", "justify", "grid",
    ];
    const LAYOUT_PROPERTIES: [&str; 27] = [
        "width",
        "height",
        "display",
        "position",
        "top",
        "right",
        "bottom",
        "left",
        "float",
        "clear",
        "gap",
        "font-size",
        "font-family",
        "line-height",
        "white-space",
        "text-align",
        "box-sizing",
        "content",
        "vertical-align",
        "order",
        "text-indent",
        "letter-spacing",
        "word-spacing",
        "text-transform",
        "word-break",
        "overflow-wrap",
        "aspect-ratio",
    ];
    // Border colors/styles are paint-only; border widths are not.
    if property.starts_with("border-") {
        return property.ends_with("-width");
    }
    LAYOUT_PROPERTIES.contains(&property)
        || LAYOUT_PREFIXES
            .iter()
            .any(|prefix| property.starts_with(prefix))
}

/// The pseudo-classes driven by pointer/keyboard interaction.
fn is_interactive(pseudo: &PseudoClass) -> bool {
    matches!(
        pseudo,
        PseudoClass::Hover | PseudoClass::Active | PseudoClass::Focus | PseudoClass::FocusWithin
    )
}

/// Whether a component (recursing into nested selector lists) mentions
/// an interactive pseudo-class.
fn component_uses_interactive(component: &Component<'static, Selectors>) -> bool {
    match component {
        Component::NonTSPseudoClass(pseudo) => is_interactive(pseudo),
        Component::Negation(list)
        | Component::Is(list)
        | Component::Where(list)
        | Component::Any(_, list)
        | Component::Has(list) => list.iter().any(uses_interactive),
        Component::NthOf(data) => data.selectors().iter().any(uses_interactive),
        _ => false,
    }
}

fn uses_interactive(selector: &Selector) -> bool {
    selector
        .iter_raw_match_order()
        .any(component_uses_interactive)
}

/// Classifies a stylesheet's hover rules (media conditions ignored —
/// conservative for any viewport).
#[must_use]
pub fn hover_impact(sheet: &Stylesheet) -> HoverImpact {
    let mut impact = HoverImpact::Nothing;
    for rule in sheet.rules.iter() {
        if !rule.selectors.iter().any(uses_interactive) {
            continue;
        }
        for declaration in &rule.declarations {
            if affects_layout(&declaration.name) {
                return HoverImpact::Layout;
            }
        }
        impact = HoverImpact::PaintOnly;
    }
    impact
}

/// Whether any `:hover` rule actually applies with the pointer on
/// `hovered` — i.e. whether moving hover onto/off it can change styles.
/// Used to skip relayouts for geometry-affecting hover rules that do not
/// involve the hovered element at all.
#[must_use]
pub fn hover_styles_may_change(
    document: &Document,
    sheet: &Stylesheet,
    hovered: Option<NodeId>,
) -> bool {
    interaction_styles_may_change(
        document,
        sheet,
        &InteractionState::new(document, hovered, None, None),
    )
}

/// Whether a selector can never match under `state` because an
/// interactive pseudo-class it directly requires has an empty state
/// chain (e.g. `:hover` with nothing hovered). Only top-level components
/// count: pseudos nested in `:not()` are ignored, since a negation
/// matches *more* when its state is empty.
fn blocked_by_state(selector: &Selector, state: &InteractionState) -> bool {
    selector.iter_raw_match_order().any(|component| {
        let Component::NonTSPseudoClass(pseudo) = component else {
            return false;
        };
        match pseudo {
            PseudoClass::Hover => state.hover_chain.is_empty(),
            PseudoClass::Active => state.active_chain.is_empty(),
            PseudoClass::Focus => state.focused.is_none(),
            PseudoClass::FocusWithin => state.focus_chain.is_empty(),
            _ => false,
        }
    })
}

/// The nodes worth testing against `selector`: when the subject itself
/// requires an interactive pseudo-class, only nodes in that state chain
/// can match — a tiny set compared to the whole tree. Otherwise the full
/// descendant walk (as before).
fn candidates<'a>(
    document: &'a Document,
    state: &'a InteractionState,
    selector: &Selector,
) -> Box<dyn Iterator<Item = NodeId> + 'a> {
    for component in selector.iter_raw_match_order() {
        match component {
            // The dummy combinator after a pseudo-element: not a real
            // compound boundary.
            Component::Combinator(Combinator::PseudoElement) => {}
            // The subject compound ended; no interactive pseudo in it.
            Component::Combinator(_) => break,
            Component::NonTSPseudoClass(pseudo) => {
                let chain = match pseudo {
                    PseudoClass::Hover => &state.hover_chain,
                    PseudoClass::Active => &state.active_chain,
                    PseudoClass::Focus | PseudoClass::FocusWithin => &state.focus_chain,
                    _ => continue,
                };
                return Box::new(chain.iter().copied());
            }
            _ => {}
        }
    }
    Box::new(document.descendants(document.root()))
}

/// Whether any interactive rule (:hover/:active/:focus...) actually
/// applies under `state` — i.e. whether entering/leaving this state can
/// change styles.
#[must_use]
pub fn interaction_styles_may_change(
    document: &Document,
    sheet: &Stylesheet,
    state: &InteractionState,
) -> bool {
    if state.hover_chain.is_empty() && state.active_chain.is_empty() && state.focused.is_none() {
        return false;
    }
    for rule in sheet.rules.iter() {
        for selector in &rule.selectors {
            if !uses_interactive(selector) {
                continue;
            }
            // A selector whose interactive state is empty can never
            // match: skip it without touching the tree.
            if blocked_by_state(selector, state) {
                continue;
            }
            let prepared = matching::prepare_selector(selector);
            let class_cache = matching::ClassCache::default();
            for id in candidates(document, state, selector) {
                if document.element(id).is_some()
                    && matching::selector_matches(
                        document,
                        state,
                        &class_cache,
                        id,
                        selector,
                        &prepared,
                    )
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Pointer/keyboard interaction state driving :hover/:active/:focus.
#[derive(Debug, Clone, Default)]
pub struct InteractionState {
    /// Hovered node and its ancestors.
    pub hover_chain: HashSet<NodeId>,
    /// Pressed node and its ancestors.
    pub active_chain: HashSet<NodeId>,
    pub focused: Option<NodeId>,
    /// Focused node and its ancestors (for :focus-within).
    pub focus_chain: HashSet<NodeId>,
    /// Link elements whose target was visited this session.
    pub visited_links: HashSet<NodeId>,
    /// Checked checkboxes/radios (live toggles + checked attributes).
    pub checked: HashSet<NodeId>,
}

impl InteractionState {
    #[must_use]
    pub fn new(
        document: &Document,
        hovered: Option<NodeId>,
        active: Option<NodeId>,
        focused: Option<NodeId>,
    ) -> Self {
        let chain = |node: Option<NodeId>| -> HashSet<NodeId> {
            let mut set = HashSet::new();
            if let Some(node) = node {
                set.insert(node);
                set.extend(document.ancestors(node));
            }
            set
        };
        Self {
            hover_chain: chain(hovered),
            active_chain: chain(active),
            focused,
            focus_chain: chain(focused),
            visited_links: HashSet::new(),
            checked: HashSet::new(),
        }
    }

    /// Same, with the set of checked checkables (`:checked`).
    #[must_use]
    pub fn with_checked(mut self, checked: HashSet<NodeId>) -> Self {
        self.checked = checked;
        self
    }

    /// Same, with the set of visited link elements (`:visited`).
    #[must_use]
    pub fn with_visited(mut self, visited_links: HashSet<NodeId>) -> Self {
        self.visited_links = visited_links;
        self
    }
}
