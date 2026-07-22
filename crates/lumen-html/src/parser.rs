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
use crate::tokenizer::{HtmlToken, Tokenizer};

/// Elements that never have children and are closed immediately.
const VOID_ELEMENTS: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Block-level start tags that implicitly close an open `<p>`.
const P_CLOSERS: [&str; 18] = [
    "p",
    "div",
    "ul",
    "ol",
    "li",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "section",
    "article",
    "header",
    "footer",
    "table",
    "pre",
    "blockquote",
];

/// Open elements an incoming start tag closes implicitly (a minimal
/// subset of the HTML "implied end tag" rules).
fn implied_closers(name: &str) -> &'static [&'static str] {
    match name {
        "li" => &["li"],
        "dt" | "dd" => &["dt", "dd"],
        "option" => &["option"],
        _ if P_CLOSERS.contains(&name) => &["p"],
        _ => &[],
    }
}

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

    // Streaming: tokens are consumed as produced, so the token list never
    // materializes in memory (peak stays proportional to the DOM, not the
    // source size).
    for token in Tokenizer::new(source) {
        match token {
            HtmlToken::StartTag {
                name,
                attributes,
                self_closing,
            } => {
                // Close open elements the new start tag implies an end for
                // (`<li>` before `<li>`, a block before `<p>`, ...).
                let closers = implied_closers(&name);
                while let Some(&top) = stack.last() {
                    let implied = match &document.node(top).kind {
                        NodeKind::Element(element) => closers.contains(&element.tag_name.as_str()),
                        _ => false,
                    };
                    if !implied {
                        break;
                    }
                    stack.pop();
                }
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

    #[test]
    fn param_is_a_void_element() {
        let document = parse_document("<object><param name='a'>fallback</object>");
        let object = document.children(document.root())[0];
        let param = document.children(object)[0];
        assert!(document.children(param).is_empty());
        assert_eq!(document.text_content(object), "fallback");
    }

    #[test]
    fn new_li_closes_an_open_li() {
        let document = parse_document("<ul><li>a<li>b<li>c</ul>");
        let ul = document.children(document.root())[0];
        assert_eq!(document.children(ul).len(), 3);
        assert_eq!(document.text_content(ul), "abc");
    }

    #[test]
    fn block_start_tag_closes_an_open_p() {
        let document = parse_document("<p>one<div>two</div><p>three<p>four");
        let root_children = document.children(document.root());
        // The <div> and every later <p> are siblings, not nested in <p>.
        assert_eq!(tags_in_order(&document), vec!["p", "div", "p", "p"]);
        assert_eq!(root_children.len(), 4);
        assert_eq!(document.text_content(root_children[2]), "three");
        assert_eq!(document.text_content(root_children[3]), "four");
    }

    #[test]
    fn new_option_closes_an_open_option() {
        let document = parse_document("<select><option>a<option>b</select>");
        let select = document.children(document.root())[0];
        assert_eq!(document.children(select).len(), 2);
        assert_eq!(document.text_content(select), "ab");
    }

    #[test]
    fn dt_and_dd_close_each_other() {
        let document = parse_document("<dl><dt>t1<dd>d1<dt>t2<dd>d2</dl>");
        let dl = document.children(document.root())[0];
        assert_eq!(document.children(dl).len(), 4);
        assert_eq!(tags_in_order(&document), vec!["dl", "dt", "dd", "dt", "dd"]);
    }

    #[test]
    fn parses_multi_megabyte_document() {
        // A few MB of repetitive markup (elements, attributes, entity
        // references, void elements) plus a large raw-text script body:
        // parsing must stay correct at scale with the streaming tokenizer.
        let row = "<div class=\"row\" data-index=\"1\"><span>metin &amp; devam</span><br></div>";
        const ROWS: usize = 40_000;
        let script_body = "x < y && y > z;\n".repeat(10_000);
        let mut html = String::with_capacity(row.len() * ROWS + script_body.len() + 64);
        html.push_str("<section>");
        for _ in 0..ROWS {
            html.push_str(row);
        }
        html.push_str("<script>");
        html.push_str(&script_body);
        html.push_str("</script></section>");
        assert!(html.len() > 2_000_000, "fixture should be multi-MB");

        let document = parse_document(&html);
        let section = document.children(document.root())[0];
        assert_eq!(document.children(section).len(), ROWS + 1);
        let script = *document.children(section).last().unwrap();
        assert_eq!(
            document.element(script).unwrap().tag_name,
            "script",
            "raw-text element should survive among the rows"
        );
        assert_eq!(document.text_content(script), script_body);
        let first_row = document.children(section)[0];
        assert_eq!(document.text_content(first_row), "metin & devam");
    }
}
