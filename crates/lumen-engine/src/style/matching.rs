//! Selector matching: compound and complex selectors evaluated against
//! a DOM node, including combinators and the sibling/nth machinery.

use super::interaction::InteractionState;
use lumen_css::{AttributeOperation, Combinator, CompoundSelector, PseudoClass, Selector};
use lumen_html::{Document, ElementData, NodeId};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// A cached sibling list, shared between every query for one parent.
type SiblingList = Rc<Vec<NodeId>>;

/// Shared state for one style pass: the document and interaction state
/// under match, plus sibling lists cached per parent.
///
/// Matching is read-only, so a cached list stays valid for the whole pass;
/// without the cache every pseudo-class check rebuilt the sibling `Vec`
/// (and cloned the tag name) per rule × element.
pub(crate) struct MatchContext<'a> {
    document: &'a Document,
    interaction: &'a InteractionState,
    /// Element children per parent node (for the child/nth-of-child
    /// pseudo-classes and the sibling combinators).
    element_kids: RefCell<HashMap<NodeId, SiblingList>>,
    /// Same-tag element children per parent, keyed by tag (for the
    /// of-type pseudo-class family; only queried tags are built).
    typed_kids: RefCell<HashMap<NodeId, HashMap<String, SiblingList>>>,
}

impl<'a> MatchContext<'a> {
    pub(crate) fn new(document: &'a Document, interaction: &'a InteractionState) -> Self {
        Self {
            document,
            interaction,
            element_kids: RefCell::new(HashMap::new()),
            typed_kids: RefCell::new(HashMap::new()),
        }
    }

    /// The element siblings of a node (children of its parent that are
    /// elements), plus the node's position among them.
    fn element_siblings(&self, node_id: NodeId) -> (SiblingList, usize) {
        let Some(parent) = self.document.parent(node_id) else {
            return (Rc::new(Vec::new()), 0);
        };
        let document = self.document;
        let siblings = self
            .element_kids
            .borrow_mut()
            .entry(parent)
            .or_insert_with(|| {
                Rc::new(
                    document
                        .children(parent)
                        .iter()
                        .copied()
                        .filter(|child| document.element(*child).is_some())
                        .collect(),
                )
            })
            .clone();
        let position = siblings
            .iter()
            .position(|sibling| *sibling == node_id)
            .unwrap_or(0);
        (siblings, position)
    }

    /// The same-tag element siblings of a node, plus its position among
    /// them (for the of-type pseudo-class family). `tag` is compared as a
    /// `&str` — nothing is cloned per query.
    fn typed_siblings(&self, node_id: NodeId, tag: &str) -> (SiblingList, usize) {
        let Some(parent) = self.document.parent(node_id) else {
            return (Rc::new(Vec::new()), 0);
        };
        let document = self.document;
        let mut cache = self.typed_kids.borrow_mut();
        let by_tag = cache.entry(parent).or_default();
        let siblings = match by_tag.get(tag) {
            Some(list) => list.clone(),
            None => {
                let list: SiblingList = Rc::new(
                    document
                        .children(parent)
                        .iter()
                        .copied()
                        .filter(|child| {
                            document
                                .element(*child)
                                .is_some_and(|element| element.tag_name == tag)
                        })
                        .collect(),
                );
                by_tag.insert(tag.to_string(), list.clone());
                list
            }
        };
        let position = siblings
            .iter()
            .position(|sibling| *sibling == node_id)
            .unwrap_or(0);
        (siblings, position)
    }
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
    context: &MatchContext<'_>,
    node_id: NodeId,
    element: &ElementData,
    compound: &CompoundSelector,
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
    let interaction = context.interaction;
    let document = context.document;
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
        PseudoClass::FirstChild => context.element_siblings(node_id).1 == 0,
        PseudoClass::LastChild => {
            let (siblings, position) = context.element_siblings(node_id);
            position + 1 == siblings.len()
        }
        PseudoClass::OnlyChild => context.element_siblings(node_id).0.len() == 1,
        PseudoClass::NthChild(a, b) => {
            let (_, position) = context.element_siblings(node_id);
            nth_matches(*a, *b, position as i32 + 1)
        }
        PseudoClass::NthLastChild(a, b) => {
            let (siblings, position) = context.element_siblings(node_id);
            nth_matches(*a, *b, (siblings.len() - position) as i32)
        }
        PseudoClass::Not(inner) => !compound_matches(context, node_id, element, inner),
        PseudoClass::Is(arguments) | PseudoClass::Where(arguments) => arguments
            .iter()
            .any(|inner| compound_matches(context, node_id, element, inner)),
        PseudoClass::FirstOfType => context.typed_siblings(node_id, &element.tag_name).1 == 0,
        PseudoClass::LastOfType => {
            let (siblings, position) = context.typed_siblings(node_id, &element.tag_name);
            position + 1 == siblings.len()
        }
        PseudoClass::OnlyOfType => context.typed_siblings(node_id, &element.tag_name).0.len() == 1,
        PseudoClass::NthOfType(a, b) => {
            let (_, position) = context.typed_siblings(node_id, &element.tag_name);
            nth_matches(*a, *b, position as i32 + 1)
        }
        PseudoClass::NthLastOfType(a, b) => {
            let (siblings, position) = context.typed_siblings(node_id, &element.tag_name);
            nth_matches(*a, *b, (siblings.len() - position) as i32)
        }
    })
}

/// Matches a complex selector right to left: the subject compound must
/// match the element itself, then each combinator walks to a parent,
/// sibling or (with backtracking) ancestor/earlier sibling.
pub(crate) fn selector_matches(
    context: &MatchContext<'_>,
    node_id: NodeId,
    element: &ElementData,
    selector: &Selector,
) -> bool {
    if !compound_matches(context, node_id, element, selector.subject()) {
        return false;
    }
    complex_matches_from(context, selector, selector.compounds.len() - 1, node_id)
}

/// Whether the selector prefix ending at `index` (which already matched
/// `node_id`) can be completed toward the left.
fn complex_matches_from(
    context: &MatchContext<'_>,
    selector: &Selector,
    index: usize,
    node_id: NodeId,
) -> bool {
    if index == 0 {
        return true;
    }
    let needed = &selector.compounds[index - 1];
    let step = |candidate: NodeId| -> bool {
        context
            .document
            .element(candidate)
            .is_some_and(|element| compound_matches(context, candidate, element, needed))
            && complex_matches_from(context, selector, index - 1, candidate)
    };
    match selector.combinators[index - 1] {
        Combinator::Child => context.document.parent(node_id).is_some_and(step),
        Combinator::Descendant => context.document.ancestors(node_id).any(step),
        Combinator::NextSibling => {
            let (siblings, position) = context.element_siblings(node_id);
            position > 0 && step(siblings[position - 1])
        }
        Combinator::SubsequentSibling => {
            let (siblings, position) = context.element_siblings(node_id);
            siblings[..position].iter().rev().any(|prior| step(*prior))
        }
    }
}
