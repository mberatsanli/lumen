//! Tree builder: turns tokenizer output into a [`Document`].
//!
//! A simplified open-element stack model. It does not implement WHATWG
//! insertion modes; there is no implied `<html>`/`<body>` synthesis.
//! Recovery rules:
//!
//! - end tags with no matching open element are ignored,
//! - a matching end tag closes every element opened after it,
//! - elements left open at end of input are closed implicitly,
//! - comments and doctype tokens are dropped (not represented in the DOM).

use crate::dom::{AttributeMap, Document, ElementData, NodeId, NodeKind};
use crate::tokenizer::{HtmlToken, tokenize};

/// Elements that never have children and are closed immediately.
const VOID_ELEMENTS: [&str; 13] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track",
    "wbr",
];

/// Parses HTML source into a DOM tree.
///
/// Never fails: malformed markup is recovered from (see module docs), which
/// mirrors how browsers always produce a tree.
#[must_use]
pub fn parse_document(source: &str) -> Document {
    let mut document = Document::new();
    // Invariant: stack[0] is the document root and is never popped, because
    // end tags only match Element nodes and the root is a Document node.
    let mut stack: Vec<NodeId> = vec![document.root()];

    for token in tokenize(source) {
        match token {
            HtmlToken::StartTag {
                name,
                attributes,
                self_closing,
            } => {
                let parent = stack[stack.len() - 1];
                let attributes: AttributeMap = attributes
                    .into_iter()
                    .map(|attribute| (attribute.name, attribute.value))
                    .collect();
                let id = document.append(
                    parent,
                    NodeKind::Element(ElementData {
                        tag_name: name.clone(),
                        attributes,
                    }),
                );
                if !self_closing && !is_void_element(&name) {
                    stack.push(id);
                }
            }
            HtmlToken::EndTag { name } => {
                if let Some(position) = stack.iter().rposition(|id| {
                    matches!(
                        &document.node(*id).kind,
                        NodeKind::Element(element) if element.tag_name == name
                    )
                }) {
                    stack.truncate(position);
                }
            }
            HtmlToken::Text(text) => {
                if !text.is_empty() {
                    let parent = stack[stack.len() - 1];
                    document.append(parent, NodeKind::Text(text));
                }
            }
            HtmlToken::Doctype(_) | HtmlToken::Comment(_) => {}
        }
    }

    document
}

fn is_void_element(name: &str) -> bool {
    VOID_ELEMENTS.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags_in_order(document: &Document) -> Vec<String> {
        document
            .descendants(document.root())
            .filter_map(|id| document.element(id).map(|element| element.tag_name.clone()))
            .collect()
    }

    #[test]
    fn builds_nested_structure() {
        let document = parse_document("<div class='card'><h1>Hello</h1></div>");
        let dump = document.dump();
        assert!(dump.contains("<div class=\"card\">"));
        assert!(dump.contains("\"Hello\""));
        assert_eq!(tags_in_order(&document), vec!["div", "h1"]);
    }

    #[test]
    fn builds_siblings_in_order() {
        let document = parse_document("<ul><li>a</li><li>b</li></ul>");
        assert_eq!(tags_in_order(&document), vec!["ul", "li", "li"]);
        let ul = document.children(document.root())[0];
        assert_eq!(document.children(ul).len(), 2);
    }

    #[test]
    fn void_elements_take_no_children() {
        let document = parse_document("<div><br>text after</div>");
        let div = document.children(document.root())[0];
        let br = document.children(div)[0];
        assert!(document.children(br).is_empty());
        assert_eq!(document.text_content(div), "text after");
    }

    #[test]
    fn self_closing_tag_takes_no_children() {
        let document = parse_document("<div><span />inside</div>");
        let div = document.children(document.root())[0];
        let span = document.children(div)[0];
        assert!(document.children(span).is_empty());
        assert_eq!(document.text_content(div), "inside");
    }

    #[test]
    fn mismatched_end_tag_is_ignored() {
        let document = parse_document("<div>a</p>b</div>");
        let div = document.children(document.root())[0];
        assert_eq!(document.text_content(div), "ab");
    }

    #[test]
    fn end_tag_closes_intermediate_elements() {
        // </div> closes the still-open <p> as well.
        let document = parse_document("<div><p>a</div><span>b</span>");
        assert_eq!(tags_in_order(&document), vec!["div", "p", "span"]);
        let span = *document.children(document.root()).last().unwrap();
        assert_eq!(document.parent(span), Some(document.root()));
    }

    #[test]
    fn text_before_and_after_children() {
        let document = parse_document("<div>before<span>mid</span>after</div>");
        let div = document.children(document.root())[0];
        assert_eq!(document.children(div).len(), 3);
        assert_eq!(document.text_content(div), "beforemidafter");
    }

    #[test]
    fn comments_and_doctype_are_not_in_the_tree() {
        let document = parse_document("<!doctype html><!-- x --><p>ok</p>");
        assert_eq!(tags_in_order(&document), vec!["p"]);
        assert_eq!(document.children(document.root()).len(), 1);
    }

    #[test]
    fn unclosed_elements_are_closed_at_end_of_input() {
        let document = parse_document("<div><p>hi");
        assert_eq!(tags_in_order(&document), vec!["div", "p"]);
    }

    #[test]
    fn style_content_survives_as_text() {
        let document = parse_document("<style>p > a { color: red; }</style>");
        let style = document.children(document.root())[0];
        assert_eq!(document.text_content(style), "p > a { color: red; }");
    }
}
