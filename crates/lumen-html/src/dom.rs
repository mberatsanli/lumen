//! Arena-based DOM tree.
//!
//! Nodes live in a `Vec` inside [`Document`] and reference each other through
//! [`NodeId`] indices (see ADR 0001). The document root is always node `0`.

use std::fmt::Write as _;

/// Index of a node inside a [`Document`] arena.
pub type NodeId = usize;

/// The kind of a DOM node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    /// The document root. Exactly one per [`Document`], always node `0`.
    Document,
    Element(ElementData),
    Text(String),
}

/// Element attributes in source (insertion) order.
///
/// Duplicate attribute names keep the first value, matching browser
/// behavior. Elements carry only a handful of attributes, so a flat
/// vector with linear lookup beats a map on both memory and speed while
/// keeping the original order for serialization.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AttributeMap {
    entries: Vec<(String, String)>,
}

impl AttributeMap {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts an attribute unless one with the same name already exists.
    pub fn insert(&mut self, name: String, value: String) {
        if !self.entries.iter().any(|(existing, _)| *existing == name) {
            self.entries.push((name, value));
        }
    }

    /// Sets (or overwrites) an attribute, keeping its original position.
    pub fn set(&mut self, name: &str, value: &str) {
        if let Some(entry) = self.entries.iter_mut().find(|(n, _)| n == name) {
            entry.1.clear();
            entry.1.push_str(value);
        } else {
            self.entries.push((name.to_string(), value.to_string()));
        }
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, value)| value.as_str())
    }

    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.entries.iter().any(|(n, _)| n == name)
    }

    /// Iterates attributes in insertion (source) order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl FromIterator<(String, String)> for AttributeMap {
    fn from_iter<I: IntoIterator<Item = (String, String)>>(iter: I) -> Self {
        let mut map = Self::new();
        for (name, value) in iter {
            map.insert(name, value);
        }
        map
    }
}

/// Data owned by an element node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementData {
    /// Lowercase tag name.
    pub tag_name: String,
    pub attributes: AttributeMap,
}

impl ElementData {
    /// The `id` attribute, if present.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.attributes.get("id")
    }

    /// Whitespace-separated entries of the `class` attribute.
    pub fn classes(&self) -> impl Iterator<Item = &str> {
        self.attributes
            .get("class")
            .unwrap_or("")
            .split_whitespace()
    }

    #[must_use]
    pub fn has_class(&self, class: &str) -> bool {
        self.classes().any(|candidate| candidate == class)
    }
}

/// A single DOM node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub kind: NodeKind,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
}

/// An HTML document holding all nodes in an arena.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    nodes: Vec<Node>,
    root: NodeId,
    /// CSS-generated (`::before`/`::after`) text nodes by
    /// (parent element, leading?) — lets regeneration update in place.
    generated: std::collections::BTreeMap<(NodeId, bool), NodeId>,
}

impl Document {
    /// Creates a document containing only the root node.
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: vec![Node {
                kind: NodeKind::Document,
                parent: None,
                children: Vec::new(),
            }],
            root: 0,
            generated: std::collections::BTreeMap::new(),
        }
    }

    /// Overwrites one attribute on an element (live form state).
    pub fn set_attribute(&mut self, node: NodeId, name: &str, value: &str) {
        if let NodeKind::Element(element) = &mut self.nodes[node].kind {
            element.attributes.set(name, value);
        }
    }

    /// Replaces the text of a node's first text child (creating one when
    /// none exists) — used for live textarea values.
    pub fn set_text_content(&mut self, parent: NodeId, text: &str) {
        let child = self.nodes[parent]
            .children
            .iter()
            .copied()
            .find(|child| matches!(self.nodes[*child].kind, NodeKind::Text(_)));
        match child {
            Some(child) => {
                if let NodeKind::Text(current) = &mut self.nodes[child].kind {
                    *current = text.to_string();
                }
            }
            None => {
                self.append(parent, NodeKind::Text(text.to_string()));
            }
        }
    }

    /// Inserts (or updates in place) a CSS-generated text node as the
    /// first (`leading`) or last child of `parent`. Returns its id.
    /// The generated text node for `(parent, leading)`, if one exists.
    #[must_use]
    pub fn generated_text(&self, parent: NodeId, leading: bool) -> Option<NodeId> {
        self.generated.get(&(parent, leading)).copied()
    }

    pub fn upsert_generated_text(&mut self, parent: NodeId, leading: bool, text: &str) -> NodeId {
        if let Some(existing) = self.generated.get(&(parent, leading)).copied() {
            if let NodeKind::Text(current) = &mut self.nodes[existing].kind {
                *current = text.to_string();
            }
            return existing;
        }
        let id = self.nodes.len();
        self.nodes.push(Node {
            kind: NodeKind::Text(text.to_string()),
            parent: Some(parent),
            children: Vec::new(),
        });
        if leading {
            self.nodes[parent].children.insert(0, id);
        } else {
            self.nodes[parent].children.push(id);
        }
        self.generated.insert((parent, leading), id);
        id
    }

    #[must_use]
    pub const fn root(&self) -> NodeId {
        self.root
    }

    /// Returns the node for `id`.
    ///
    /// # Panics
    ///
    /// Panics if `id` was not produced by this document.
    #[must_use]
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    #[must_use]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// Appends a new node as the last child of `parent` and returns its id.
    pub fn append(&mut self, parent: NodeId, kind: NodeKind) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node {
            kind,
            parent: Some(parent),
            children: Vec::new(),
        });
        self.nodes[parent].children.push(id);
        id
    }

    /// Creates a detached element (no parent until appended).
    pub fn create_element(&mut self, tag: &str) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node {
            kind: NodeKind::Element(ElementData {
                tag_name: tag.to_ascii_lowercase(),
                attributes: AttributeMap::new(),
            }),
            parent: None,
            children: Vec::new(),
        });
        id
    }

    /// Creates a detached text node.
    pub fn create_text(&mut self, text: &str) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node {
            kind: NodeKind::Text(text.to_string()),
            parent: None,
            children: Vec::new(),
        });
        id
    }

    /// Attaches `child` as the last child of `parent`, detaching it from
    /// any current parent first. Refuses appends that would create a
    /// cycle (a node into itself or its own descendant).
    pub fn append_child(&mut self, parent: NodeId, child: NodeId) {
        if parent == child
            || child == self.root
            || std::iter::once(parent)
                .chain(self.ancestors(parent))
                .any(|ancestor| ancestor == child)
        {
            return;
        }
        self.detach(child);
        self.nodes[child].parent = Some(parent);
        self.nodes[parent].children.push(child);
    }

    /// Detaches a node from its parent. The node stays in the arena (and
    /// can be re-appended); detached subtrees simply never render.
    pub fn detach(&mut self, node: NodeId) {
        if let Some(parent) = self.nodes[node].parent.take() {
            self.nodes[parent].children.retain(|child| *child != node);
        }
    }

    /// Replaces a node's children with the parse of an HTML fragment
    /// (the innerHTML setter). Scripts inside the fragment become inert
    /// nodes, as in real browsers.
    pub fn set_inner_html(&mut self, parent: NodeId, html: &str) {
        for child in std::mem::take(&mut self.nodes[parent].children) {
            self.nodes[child].parent = None;
        }
        // Generated value/pseudo text under the old children is stale now.
        self.generated.retain(|(host, _), _| *host != parent);
        let fragment = crate::parse_document(html);
        let mut stack: Vec<(NodeId, NodeId)> = fragment
            .children(fragment.root())
            .iter()
            .rev()
            .map(|child| (*child, parent))
            .collect();
        while let Some((source, target_parent)) = stack.pop() {
            let copy = self.append(target_parent, fragment.node(source).kind.clone());
            for child in fragment.children(source).iter().rev() {
                stack.push((*child, copy));
            }
        }
    }

    /// Serializes a node's children back to HTML (the innerHTML getter).
    pub fn inner_html(&self, parent: NodeId) -> String {
        let mut output = String::new();
        for child in self.children(parent) {
            self.serialize_node(*child, &mut output);
        }
        output
    }

    // Iterative (explicit stack) so deeply nested DOMs cannot overflow
    // the call stack; children are pushed in reverse to keep order.
    fn serialize_node(&self, node: NodeId, output: &mut String) {
        enum Frame {
            Enter(NodeId),
            Exit(NodeId),
        }
        let mut stack = vec![Frame::Enter(node)];
        while let Some(frame) = stack.pop() {
            match frame {
                Frame::Exit(node) => {
                    if let NodeKind::Element(element) = &self.node(node).kind {
                        let _ = write!(output, "</{}>", element.tag_name);
                    }
                }
                Frame::Enter(node) => match &self.node(node).kind {
                    NodeKind::Document => {}
                    NodeKind::Text(text) => {
                        output.push_str(&text.replace('&', "&amp;").replace('<', "&lt;"));
                    }
                    NodeKind::Element(element) => {
                        let _ = write!(output, "<{}", element.tag_name);
                        for (name, value) in element.attributes.iter() {
                            let escaped = value.replace('&', "&amp;").replace('"', "&quot;");
                            let _ = write!(output, " {name}=\"{escaped}\"");
                        }
                        output.push('>');
                        stack.push(Frame::Exit(node));
                        for child in self.children(node).iter().rev() {
                            stack.push(Frame::Enter(*child));
                        }
                    }
                },
            }
        }
    }

    #[must_use]
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.node(id).parent
    }

    #[must_use]
    pub fn children(&self, id: NodeId) -> &[NodeId] {
        &self.node(id).children
    }

    /// Element data for `id`, if the node is an element.
    #[must_use]
    pub fn element(&self, id: NodeId) -> Option<&ElementData> {
        match &self.node(id).kind {
            NodeKind::Element(element) => Some(element),
            _ => None,
        }
    }

    /// Preorder traversal of the subtree below `id`, excluding `id` itself.
    pub fn descendants(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        let mut stack: Vec<NodeId> = self.children(id).iter().rev().copied().collect();
        std::iter::from_fn(move || {
            let next = stack.pop()?;
            stack.extend(self.children(next).iter().rev());
            Some(next)
        })
    }

    /// Ancestors of `id` from its parent up to the root.
    pub fn ancestors(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        let mut current = self.parent(id);
        std::iter::from_fn(move || {
            let next = current?;
            current = self.parent(next);
            Some(next)
        })
    }

    /// First element in document order whose `id` attribute equals `id_value`.
    #[must_use]
    pub fn get_element_by_id(&self, id_value: &str) -> Option<NodeId> {
        self.descendants(self.root)
            .find(|node| self.element(*node).and_then(ElementData::id) == Some(id_value))
    }

    /// Concatenated text of all text nodes in the subtree of `id`.
    #[must_use]
    pub fn text_content(&self, id: NodeId) -> String {
        let mut output = String::new();
        self.collect_text(id, &mut output);
        output
    }

    fn collect_text(&self, id: NodeId, output: &mut String) {
        let mut stack = vec![id];
        while let Some(node) = stack.pop() {
            match &self.node(node).kind {
                NodeKind::Text(text) => output.push_str(text),
                _ => stack.extend(self.node(node).children.iter().rev()),
            }
        }
    }

    /// Human-readable tree dump for debugging and tests.
    #[must_use]
    pub fn dump(&self) -> String {
        let mut output = String::new();
        self.dump_node(self.root, 0, &mut output);
        output
    }

    fn dump_node(&self, id: NodeId, depth: usize, output: &mut String) {
        let mut stack = vec![(id, depth)];
        while let Some((id, depth)) = stack.pop() {
            let indent = "  ".repeat(depth);
            match &self.node(id).kind {
                NodeKind::Document => {
                    let _ = writeln!(output, "{indent}#document");
                }
                NodeKind::Text(text) => {
                    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !normalized.is_empty() {
                        let _ = writeln!(output, "{indent}\"{normalized}\"");
                    }
                }
                NodeKind::Element(element) => {
                    let mut attributes = String::new();
                    for (name, value) in element.attributes.iter() {
                        let _ = write!(attributes, " {name}=\"{value}\"");
                    }
                    let _ = writeln!(output, "{indent}<{}{}>", element.tag_name, attributes);
                }
            }

            for child in self.node(id).children.iter().rev() {
                stack.push((*child, depth + 1));
            }
        }
    }
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(tag: &str, attributes: &[(&str, &str)]) -> NodeKind {
        NodeKind::Element(ElementData {
            tag_name: tag.to_string(),
            attributes: attributes
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        })
    }

    #[test]
    fn new_document_has_only_root() {
        let document = Document::new();
        assert_eq!(document.root(), 0);
        assert!(matches!(document.node(0).kind, NodeKind::Document));
        assert!(document.children(document.root()).is_empty());
    }

    #[test]
    fn append_builds_nested_elements_with_valid_parents() {
        let mut document = Document::new();
        let outer = document.append(document.root(), element("div", &[]));
        let inner = document.append(outer, element("p", &[]));
        let text = document.append(inner, NodeKind::Text("hi".to_string()));

        assert_eq!(document.parent(outer), Some(document.root()));
        assert_eq!(document.parent(inner), Some(outer));
        assert_eq!(document.parent(text), Some(inner));
        assert_eq!(document.children(outer), &[inner]);
    }

    #[test]
    fn siblings_keep_insertion_order() {
        let mut document = Document::new();
        let parent = document.append(document.root(), element("ul", &[]));
        let first = document.append(parent, element("li", &[]));
        let second = document.append(parent, element("li", &[]));
        assert_eq!(document.children(parent), &[first, second]);
    }

    #[test]
    fn text_content_concatenates_subtree_text() {
        let mut document = Document::new();
        let div = document.append(document.root(), element("div", &[]));
        document.append(div, NodeKind::Text("a".to_string()));
        let span = document.append(div, element("span", &[]));
        document.append(span, NodeKind::Text("b".to_string()));
        assert_eq!(document.text_content(div), "ab");
    }

    #[test]
    fn attribute_lookup_id_and_classes() {
        let mut document = Document::new();
        let id = document.append(
            document.root(),
            element("div", &[("id", "main"), ("class", "card active")]),
        );
        let data = document.element(id).unwrap();
        assert_eq!(data.id(), Some("main"));
        assert_eq!(data.classes().collect::<Vec<_>>(), vec!["card", "active"]);
        assert!(data.has_class("card"));
        assert!(!data.has_class("car"));
        assert_eq!(data.attributes.get("missing"), None);
    }

    #[test]
    fn duplicate_attribute_keeps_first_value() {
        let mut attributes = AttributeMap::new();
        attributes.insert("class".to_string(), "first".to_string());
        attributes.insert("class".to_string(), "second".to_string());
        assert_eq!(attributes.get("class"), Some("first"));
        assert_eq!(attributes.len(), 1);
    }

    #[test]
    fn attributes_keep_source_order() {
        let document = crate::parse_document("<div z='1' id='x' class='c'>");
        let div = document.children(document.root())[0];
        let names: Vec<&str> = document
            .element(div)
            .unwrap()
            .attributes
            .iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, vec!["z", "id", "class"]);
        // inner_html serializes in that same order.
        assert_eq!(
            document.inner_html(document.root()),
            "<div z=\"1\" id=\"x\" class=\"c\"></div>"
        );
    }

    #[test]
    fn set_overwrites_in_place_keeping_position() {
        let mut attributes = AttributeMap::new();
        attributes.insert("b".to_string(), "1".to_string());
        attributes.insert("a".to_string(), "2".to_string());
        attributes.set("b", "3");
        let entries: Vec<(&str, &str)> = attributes.iter().collect();
        assert_eq!(entries, vec![("b", "3"), ("a", "2")]);
    }

    #[test]
    fn descendants_traverse_in_preorder() {
        let mut document = Document::new();
        let a = document.append(document.root(), element("a", &[]));
        let b = document.append(a, element("b", &[]));
        let c = document.append(b, element("c", &[]));
        let d = document.append(a, element("d", &[]));
        let order: Vec<NodeId> = document.descendants(document.root()).collect();
        assert_eq!(order, vec![a, b, c, d]);
    }

    #[test]
    fn ancestors_walk_to_root() {
        let mut document = Document::new();
        let a = document.append(document.root(), element("a", &[]));
        let b = document.append(a, element("b", &[]));
        let chain: Vec<NodeId> = document.ancestors(b).collect();
        assert_eq!(chain, vec![a, document.root()]);
    }

    #[test]
    fn get_element_by_id_finds_first_match() {
        let mut document = Document::new();
        let first = document.append(document.root(), element("div", &[("id", "x")]));
        document.append(document.root(), element("p", &[("id", "x")]));
        assert_eq!(document.get_element_by_id("x"), Some(first));
        assert_eq!(document.get_element_by_id("missing"), None);
    }
}

#[cfg(test)]
mod inner_html_tests {
    #[test]
    fn set_inner_html_replaces_children_and_serializes_back() {
        let mut document = crate::parse_document("<ul id='l'><li>eski</li></ul>");
        let list = document.get_element_by_id("l").unwrap();
        document.set_inner_html(list, "<li class='a'>bir</li><li>iki &amp; buçuk</li>");
        assert_eq!(document.children(list).len(), 2);
        assert_eq!(document.text_content(list), "biriki & buçuk");
        assert_eq!(
            document.inner_html(list),
            "<li class=\"a\">bir</li><li>iki &amp; buçuk</li>"
        );
        // Old children are detached, and nested fragments nest.
        document.set_inner_html(list, "<li><b>kalın</b></li>");
        assert_eq!(document.text_content(list), "kalın");
    }

    #[test]
    fn inner_html_escapes_ampersand_and_quote_in_attributes() {
        let mut document = crate::parse_document("<p id='x'>hi</p>");
        let paragraph = document.get_element_by_id("x").unwrap();
        document.set_attribute(paragraph, "title", "a & b \"c\"");
        assert_eq!(
            document.inner_html(document.root()),
            "<p id=\"x\" title=\"a &amp; b &quot;c&quot;\">hi</p>"
        );
        // Round-trip: parsing the serialized form restores the value.
        let reparsed = crate::parse_document(&document.inner_html(document.root()));
        let paragraph = reparsed.get_element_by_id("x").unwrap();
        assert_eq!(
            reparsed.element(paragraph).unwrap().attributes.get("title"),
            Some("a & b \"c\"")
        );
    }

    #[test]
    fn deeply_nested_tree_does_not_overflow_the_stack() {
        // serialize/text_content/dump were recursive; a deep chain must work.
        let html = format!("{}dip", "<div>".repeat(10_000));
        let document = crate::parse_document(&html);
        assert!(document.text_content(document.root()).ends_with("dip"));
        let serialized = document.inner_html(document.root());
        assert_eq!(serialized.matches("<div>").count(), 10_000);
        assert_eq!(serialized.matches("</div>").count(), 10_000);
        assert!(document.dump().ends_with("dip\"\n"));
    }
}
