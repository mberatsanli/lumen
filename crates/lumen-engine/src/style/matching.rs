//! Selector matching: compound and complex selectors evaluated against
//! a DOM node, including combinators and the sibling/nth machinery.

use super::interaction::InteractionState;
use lumen_css::{AttributeOperation, Combinator, CompoundSelector, PseudoClass, Selector};
use lumen_html::{Document, ElementData, NodeId};

/// The same-tag element siblings of a node, plus its position among them
/// (for the of-type pseudo-class family).
pub(crate) fn typed_siblings(document: &Document, node_id: NodeId) -> (Vec<NodeId>, usize) {
    let tag = document
        .element(node_id)
        .map(|element| element.tag_name.clone())
        .unwrap_or_default();
    let siblings: Vec<NodeId> = document.parent(node_id).map_or_else(Vec::new, |parent| {
        document
            .children(parent)
            .iter()
            .copied()
            .filter(|child| {
                document
                    .element(*child)
                    .is_some_and(|element| element.tag_name == tag)
            })
            .collect()
    });
    let position = siblings
        .iter()
        .position(|sibling| *sibling == node_id)
        .unwrap_or(0);
    (siblings, position)
}

/// The element siblings of a node (children of its parent that are
/// elements), plus the node's position among them.
pub(crate) fn element_siblings(document: &Document, node_id: NodeId) -> (Vec<NodeId>, usize) {
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
pub(crate) fn nth_matches(a: i32, b: i32, index: i32) -> bool {
    if a == 0 {
        return index == b;
    }
    let distance = index - b;
    distance % a == 0 && distance / a >= 0
}

pub(crate) fn compound_matches(
    document: &Document,
    node_id: NodeId,
    element: &ElementData,
    compound: &CompoundSelector,
    interaction: &InteractionState,
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
            AttributeOperation::WordMatch(word) => {
                value.split_whitespace().any(|candidate| candidate == word)
            }
            AttributeOperation::LangPrefix(prefix) => {
                value == prefix
                    || value
                        .strip_prefix(prefix.as_str())
                        .is_some_and(|rest| rest.starts_with('-'))
            }
        }
    }) {
        return false;
    }
    compound.pseudo_classes.iter().all(|pseudo| match pseudo {
        PseudoClass::Hover => interaction.hover_chain.contains(&node_id),
        PseudoClass::Active => interaction.active_chain.contains(&node_id),
        PseudoClass::Focus => interaction.focused == Some(node_id),
        PseudoClass::FocusWithin => interaction.focus_chain.contains(&node_id),
        PseudoClass::Root => document.parent(node_id) == Some(document.root()),
        PseudoClass::Checked => interaction.checked.contains(&node_id),
        PseudoClass::Visited => interaction.visited_links.contains(&node_id),
        PseudoClass::Link => {
            element.attributes.contains("href") && !interaction.visited_links.contains(&node_id)
        }
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
            !compound_matches(document, node_id, element, inner, interaction)
        }
        PseudoClass::Is(arguments) | PseudoClass::Where(arguments) => arguments
            .iter()
            .any(|inner| compound_matches(document, node_id, element, inner, interaction)),
        PseudoClass::FirstOfType => typed_siblings(document, node_id).1 == 0,
        PseudoClass::LastOfType => {
            let (siblings, position) = typed_siblings(document, node_id);
            position + 1 == siblings.len()
        }
        PseudoClass::OnlyOfType => typed_siblings(document, node_id).0.len() == 1,
        PseudoClass::NthOfType(a, b) => {
            let (_, position) = typed_siblings(document, node_id);
            nth_matches(*a, *b, position as i32 + 1)
        }
        PseudoClass::NthLastOfType(a, b) => {
            let (siblings, position) = typed_siblings(document, node_id);
            nth_matches(*a, *b, (siblings.len() - position) as i32)
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
    interaction: &InteractionState,
) -> bool {
    if !compound_matches(document, node_id, element, selector.subject(), interaction) {
        return false;
    }
    complex_matches_from(
        document,
        selector,
        selector.compounds.len() - 1,
        node_id,
        interaction,
    )
}

/// Whether the selector prefix ending at `index` (which already matched
/// `node_id`) can be completed toward the left.
pub(crate) fn complex_matches_from(
    document: &Document,
    selector: &Selector,
    index: usize,
    node_id: NodeId,
    interaction: &InteractionState,
) -> bool {
    if index == 0 {
        return true;
    }
    let needed = &selector.compounds[index - 1];
    let step = |candidate: NodeId| -> bool {
        document.element(candidate).is_some_and(|element| {
            compound_matches(document, candidate, element, needed, interaction)
        }) && complex_matches_from(document, selector, index - 1, candidate, interaction)
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
