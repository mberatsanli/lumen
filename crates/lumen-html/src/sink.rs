//! html5ever tree sink that builds our arena [`Document`].
//!
//! html5ever implements the full WHATWG tree construction algorithm:
//! insertion modes, the `html`/`head`/`body` skeleton, the adoption
//! agency, foster parenting, RCDATA/rawtext states and the complete
//! named-character-reference table. This module only translates its
//! [`TreeSink`] callbacks into arena operations.
//!
//! Deliberate simplifications on top of html5ever's output:
//!
//! - comments, processing instructions and the doctype are dropped —
//!   the DOM has no node kinds for them (the sink returns dummy handles
//!   and skips them when the tree builder appends them),
//! - `<template>` contents become direct children of the `<template>`
//!   element instead of a separate "template contents" fragment,
//! - namespaces are flattened: elements keep only their local tag name,
//!   attributes keep `prefix:local`, and every element reports the HTML
//!   namespace from `elem_name` (foreign SVG/MathML content keeps its
//!   local names but is not distinguished further).

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use html5ever::interface::{ElemName, ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::tendril::{ByteTendril, StrTendril, TendrilSink};
use html5ever::{Attribute, LocalName, Namespace, ParseOpts, QualName, ns};

use crate::dom::{Document, NodeId, NodeKind};

/// Parses a full HTML document into an arena [`Document`].
///
/// Never fails: malformed markup is recovered from, and the
/// `html`/`head`/`body` skeleton is synthesized even for fragments of
/// source — exactly like a browser. Recoverable errors are counted on
/// [`Document::parse_error_count`].
#[must_use]
pub fn parse_document(source: &str) -> Document {
    html5ever::parse_document(ArenaSink::new(), ParseOpts::default()).one(source)
}

/// An incremental document parser: byte chunks are fed as they arrive
/// (from a streaming network read), and the partial [`Document`] can be
/// snapshotted at any point for progressive rendering — elements not yet
/// closed by the source stay open, exactly the state a browser renders
/// mid-load. Input is decoded as lossy UTF-8; multi-byte sequences split
/// across chunk boundaries are reassembled by the decoder.
///
/// Feeding [`Self::finish`] the same bytes as [`parse_document`] receives
/// yields the identical tree (chunk boundaries are invisible to the
/// tokenizer), as long as the source was valid UTF-8.
pub struct StreamingParser {
    parser: html5ever::tendril::stream::Utf8LossyDecoder<html5ever::driver::Parser<ArenaSink>>,
    /// Shared with the sink, so snapshots can read the arena while the
    /// parser still owns the sink itself.
    document: Rc<RefCell<Document>>,
    parse_errors: Rc<Cell<usize>>,
}

impl StreamingParser {
    #[must_use]
    pub fn new() -> Self {
        let document = Rc::new(RefCell::new(Document::new()));
        let parse_errors = Rc::new(Cell::new(0));
        let sink = ArenaSink {
            document: Rc::clone(&document),
            parse_errors: Rc::clone(&parse_errors),
            dropped: RefCell::new(HashSet::new()),
        };
        Self {
            parser: html5ever::driver::parse_document(sink, ParseOpts::default()).from_utf8(),
            document,
            parse_errors,
        }
    }

    /// Feeds one chunk of the source. Infallible: malformed bytes are
    /// replaced (lossy decoding), malformed markup is recovered from.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(ByteTendril::from_slice(bytes));
    }

    /// A point-in-time copy of the document parsed so far. Cheap enough
    /// per milestone (the arena is a flat `Vec`), but not free — callers
    /// throttle snapshots instead of taking one per chunk.
    #[must_use]
    pub fn snapshot(&self) -> Document {
        let mut document = self.document.borrow().clone();
        document.set_parse_error_count(self.parse_errors.get());
        document
    }

    /// Ends the input and returns the final document.
    #[must_use]
    pub fn finish(self) -> Document {
        self.parser.finish()
    }
}

impl Default for StreamingParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Parses an HTML fragment in the context of `context_tag` (the
/// innerHTML algorithm). The parsed nodes end up under a synthetic
/// `<html>` element which is the only child of the returned document's
/// root — callers take that element's children as the fragment.
#[must_use]
pub fn parse_fragment(context_tag: &str, source: &str) -> Document {
    let context = QualName::new(None, ns!(html), LocalName::from(context_tag));
    html5ever::parse_fragment(
        ArenaSink::new(),
        ParseOpts::default(),
        context,
        Vec::new(),
        false,
    )
    .one(source)
}

/// The tree sink: every callback mutates the arena through a `RefCell`
/// because the [`TreeSink`] trait takes `&self` throughout. The arena
/// and the error count sit behind an `Rc` so a [`StreamingParser`] can
/// snapshot the partial document while the html5ever parser still owns
/// the sink (it only hands it back on `finish`).
struct ArenaSink {
    document: Rc<RefCell<Document>>,
    parse_errors: Rc<Cell<usize>>,
    /// Dummy handles handed out for comments and processing
    /// instructions; appends targeting them are skipped.
    dropped: RefCell<HashSet<NodeId>>,
}

impl ArenaSink {
    fn new() -> Self {
        Self {
            document: Rc::new(RefCell::new(Document::new())),
            parse_errors: Rc::new(Cell::new(0)),
            dropped: RefCell::new(HashSet::new()),
        }
    }

    /// Allocates a detached dummy node that stands in for a node kind
    /// the DOM does not represent (comment, PI).
    fn dummy(&self) -> NodeId {
        let id = self.document.borrow_mut().create_text("");
        self.dropped.borrow_mut().insert(id);
        id
    }
}

/// Attribute name with its namespace prefix flattened in
/// (`xlink:href`); plain attributes keep just their local name.
fn attribute_name(name: &QualName) -> String {
    match &name.prefix {
        Some(prefix) => format!("{prefix}:{}", name.local),
        None => name.local.to_string(),
    }
}

/// Owned [`ElemName`] rebuilt on demand from the arena's plain string
/// tag names (the sink does not store `QualName`s).
#[derive(Debug)]
struct SinkElemName(QualName);

impl ElemName for SinkElemName {
    fn ns(&self) -> &Namespace {
        &self.0.ns
    }

    fn local_name(&self) -> &LocalName {
        &self.0.local
    }
}

impl TreeSink for ArenaSink {
    type Handle = NodeId;
    type Output = Document;
    type ElemName<'a> = SinkElemName;

    fn finish(self) -> Document {
        let mut document = std::mem::take(&mut *self.document.borrow_mut());
        document.set_parse_error_count(self.parse_errors.get());
        document
    }

    fn parse_error(&self, _message: Cow<'static, str>) {
        self.parse_errors.set(self.parse_errors.get() + 1);
    }

    fn get_document(&self) -> NodeId {
        self.document.borrow().root()
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> SinkElemName {
        let document = self.document.borrow();
        match &document.node(*target).kind {
            NodeKind::Element(element) => SinkElemName(QualName::new(
                None,
                ns!(html),
                LocalName::from(element.tag_name.as_str()),
            )),
            _ => panic!("elem_name called on a non-element node"),
        }
    }

    fn create_element(
        &self,
        name: QualName,
        attributes: Vec<Attribute>,
        _flags: ElementFlags,
    ) -> NodeId {
        let mut document = self.document.borrow_mut();
        let id = document.create_element(&name.local);
        for attribute in attributes {
            document.add_attribute_if_missing(
                id,
                attribute_name(&attribute.name),
                attribute.value.to_string(),
            );
        }
        id
    }

    fn create_comment(&self, _text: StrTendril) -> NodeId {
        self.dummy()
    }

    fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> NodeId {
        self.dummy()
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        match child {
            NodeOrText::AppendNode(id) => {
                if self.dropped.borrow().contains(&id) {
                    return;
                }
                // The tree builder guarantees the child has no parent yet.
                self.document.borrow_mut().attach(*parent, id);
            }
            NodeOrText::AppendText(text) => {
                self.document.borrow_mut().append_text(*parent, &text);
            }
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        // Foster parenting: insert before the table when it has a parent,
        // otherwise append to the element above it on the stack.
        if self.document.borrow().parent(*element).is_some() {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        _name: StrTendril,
        _public_id: StrTendril,
        _system_id: StrTendril,
    ) {
        // Doctypes are not represented in the DOM.
    }

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        // Simplification: template contents are the element's own children.
        *target
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, _mode: QuirksMode) {
        // Rendering does not distinguish quirks modes.
    }

    fn append_before_sibling(&self, sibling: &NodeId, new_node: NodeOrText<NodeId>) {
        match new_node {
            NodeOrText::AppendNode(id) => {
                if self.dropped.borrow().contains(&id) {
                    return;
                }
                self.document.borrow_mut().insert_before(*sibling, id);
            }
            NodeOrText::AppendText(text) => {
                self.document
                    .borrow_mut()
                    .insert_text_before(*sibling, &text);
            }
        }
    }

    fn add_attrs_if_missing(&self, target: &NodeId, attributes: Vec<Attribute>) {
        let mut document = self.document.borrow_mut();
        for attribute in attributes {
            document.add_attribute_if_missing(
                *target,
                attribute_name(&attribute.name),
                attribute.value.to_string(),
            );
        }
    }

    fn remove_from_parent(&self, target: &NodeId) {
        self.document.borrow_mut().detach(*target);
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        let mut document = self.document.borrow_mut();
        for child in document.children(*node).to_vec() {
            document.append_child(*new_parent, child);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::NodeId;

    /// First element in document order with this tag name.
    fn first(document: &Document, tag: &str) -> NodeId {
        document
            .descendants(document.root())
            .find(|id| {
                document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == tag)
            })
            .unwrap_or_else(|| panic!("no <{tag}> in:\n{}", document.dump()))
    }

    fn body(document: &Document) -> NodeId {
        first(document, "body")
    }

    fn tags_in_order(document: &Document) -> Vec<String> {
        document
            .descendants(document.root())
            .filter_map(|id| document.element(id).map(|element| element.tag_name.clone()))
            .collect()
    }

    // -- The html5ever skeleton -------------------------------------------

    #[test]
    fn always_synthesizes_the_html_head_body_skeleton() {
        let document = parse_document("<p>x</p>");
        let html = document.children(document.root())[0];
        assert_eq!(document.element(html).unwrap().tag_name, "html");
        assert_eq!(tags_in_order(&document), vec!["html", "head", "body", "p"]);
        let body = body(&document);
        let p = document.children(body)[0];
        assert_eq!(document.element(p).unwrap().tag_name, "p");
        assert_eq!(document.text_content(p), "x");
    }

    #[test]
    fn clean_documents_report_no_parse_errors() {
        let document = parse_document("<!doctype html><p>ok</p>");
        assert_eq!(document.parse_error_count(), 0);
        let recovered = parse_document("<div>a</p>b</div>");
        assert!(recovered.parse_error_count() > 0);
    }

    // -- Recovery (carried over from the hand-written parser) --------------

    #[test]
    fn builds_nested_structure() {
        let document = parse_document("<div class='card'><h1>Hello</h1></div>");
        let dump = document.dump();
        assert!(dump.contains("<div class=\"card\">"));
        assert!(dump.contains("\"Hello\""));
    }

    #[test]
    fn builds_siblings_in_order() {
        let document = parse_document("<ul><li>a</li><li>b</li></ul>");
        let ul = first(&document, "ul");
        assert_eq!(document.children(ul).len(), 2);
    }

    #[test]
    fn void_elements_take_no_children() {
        let document = parse_document("<div><br>text after</div>");
        let div = first(&document, "div");
        let br = first(&document, "br");
        assert!(document.children(br).is_empty());
        assert_eq!(document.text_content(div), "text after");
    }

    #[test]
    fn mismatched_end_tag_is_recovered_from() {
        let document = parse_document("<div>a</p>b</div>");
        let div = first(&document, "div");
        assert_eq!(document.text_content(div), "ab");
    }

    #[test]
    fn end_tag_closes_intermediate_elements() {
        // </div> closes the still-open <p> as well.
        let document = parse_document("<div><p>a</div><span>b</span>");
        let body = body(&document);
        let span = *document.children(body).last().unwrap();
        assert_eq!(document.element(span).unwrap().tag_name, "span");
        assert_eq!(document.parent(span), Some(body));
    }

    #[test]
    fn comments_and_doctype_are_not_in_the_tree() {
        let document = parse_document("<!doctype html><!-- x --><p>ok</p>");
        assert_eq!(tags_in_order(&document), vec!["html", "head", "body", "p"]);
        assert_eq!(document.text_content(document.root()), "ok");
    }

    #[test]
    fn unclosed_elements_are_closed_at_end_of_input() {
        let document = parse_document("<div><p>hi");
        assert_eq!(
            tags_in_order(&document),
            vec!["html", "head", "body", "div", "p"]
        );
    }

    #[test]
    fn style_content_survives_as_text() {
        let document = parse_document("<style>p > a { color: red; }</style>");
        let style = first(&document, "style");
        assert_eq!(document.text_content(style), "p > a { color: red; }");
    }

    #[test]
    fn param_is_a_void_element() {
        let document = parse_document("<object><param name='a'>fallback</object>");
        let object = first(&document, "object");
        let param = first(&document, "param");
        assert!(document.children(param).is_empty());
        assert_eq!(document.text_content(object), "fallback");
    }

    #[test]
    fn new_li_closes_an_open_li() {
        let document = parse_document("<ul><li>a<li>b<li>c</ul>");
        let ul = first(&document, "ul");
        assert_eq!(document.children(ul).len(), 3);
        assert_eq!(document.text_content(ul), "abc");
    }

    #[test]
    fn block_start_tag_closes_an_open_p() {
        let document = parse_document("<p>one<div>two</div><p>three<p>four");
        let body = body(&document);
        let tags: Vec<String> = document
            .children(body)
            .iter()
            .filter_map(|id| {
                document
                    .element(*id)
                    .map(|element| element.tag_name.clone())
            })
            .collect();
        // The <div> and every later <p> are siblings, not nested in <p>.
        assert_eq!(tags, vec!["p", "div", "p", "p"]);
        let children = document.children(body);
        assert_eq!(document.text_content(children[2]), "three");
        assert_eq!(document.text_content(children[3]), "four");
    }

    #[test]
    fn new_option_closes_an_open_option() {
        let document = parse_document("<select><option>a<option>b</select>");
        let select = first(&document, "select");
        assert_eq!(document.children(select).len(), 2);
        assert_eq!(document.text_content(select), "ab");
    }

    #[test]
    fn dt_and_dd_close_each_other() {
        let document = parse_document("<dl><dt>t1<dd>d1<dt>t2<dd>d2</dl>");
        let dl = first(&document, "dl");
        assert_eq!(document.children(dl).len(), 4);
    }

    // -- Capabilities the hand-written parser did not have -----------------

    #[test]
    fn rcdata_elements_decode_entities_but_not_tags() {
        let document = parse_document("<title>a &amp; <b> c</title>");
        let title = first(&document, "title");
        assert_eq!(document.text_content(title), "a & <b> c");
        assert!(document.children(title).len() == 1);

        let document = parse_document("<textarea>&lt;tag&gt; &amp;</textarea>");
        let textarea = first(&document, "textarea");
        assert_eq!(document.text_content(textarea), "<tag> &");
    }

    #[test]
    fn rawtext_family_stays_literal() {
        for tag in ["xmp", "iframe", "noembed", "noframes"] {
            let document = parse_document(&format!("<{tag}><b>bold</b></{tag}>"));
            let element = first(&document, tag);
            assert_eq!(
                document.text_content(element),
                "<b>bold</b>",
                "<{tag}> contents must be raw text"
            );
        }
        // <plaintext> swallows the rest of the document as text.
        let document = parse_document("<plaintext><b>x</b>");
        let plaintext = first(&document, "plaintext");
        assert_eq!(document.text_content(plaintext), "<b>x</b>");
    }

    #[test]
    fn script_decodes_nothing_and_honors_spec_self_closing() {
        // A self-closing flag on <script> is ignored per spec: the
        // following text becomes script content until </script>.
        let document = parse_document("<div><script/>x < y;</script><p>after</p></div>");
        let script = first(&document, "script");
        assert_eq!(document.text_content(script), "x < y;");
        let p = first(&document, "p");
        assert_eq!(document.text_content(p), "after");
    }

    #[test]
    fn self_closing_flag_on_non_void_elements_is_ignored_per_spec() {
        // Spec behavior: <span/> stays open, so text nests inside it.
        let document = parse_document("<div><span />inside</div>");
        let span = first(&document, "span");
        assert_eq!(document.text_content(span), "inside");
    }

    #[test]
    fn adoption_agency_reconstructs_misnested_formatting() {
        // The spec's classic example: the misnested <b>/<i> pair is
        // split so <i> is rebuilt inside <p> after <b> closes.
        let document = parse_document("<p>1<b>2<i>3</b>4</i>5</p>");
        let p = first(&document, "p");
        let b = first(&document, "b");
        let inner_i = document.children(b)[1];
        assert_eq!(document.element(inner_i).unwrap().tag_name, "i");
        assert_eq!(document.text_content(inner_i), "3");
        // The rebuilt <i> is a sibling of <b> inside <p> and holds "4".
        let p_children = document.children(p);
        let rebuilt_i = p_children[p_children.len() - 2];
        assert_eq!(document.element(rebuilt_i).unwrap().tag_name, "i");
        assert_eq!(document.text_content(rebuilt_i), "4");
        assert_eq!(document.text_content(p), "12345");

        // <b><i>x</b></i> keeps a single b > i > "x" chain (verified
        // against the html5lib reference implementation).
        let document = parse_document("<b><i>x</b></i>");
        let b = first(&document, "b");
        let inner = document.children(b)[0];
        assert_eq!(document.element(inner).unwrap().tag_name, "i");
        assert_eq!(document.text_content(inner), "x");
        assert_eq!(
            document
                .children(body(&document))
                .iter()
                .filter(|id| {
                    document
                        .element(**id)
                        .is_some_and(|element| element.tag_name == "b")
                })
                .count(),
            1
        );
    }

    #[test]
    fn foster_parenting_moves_stray_text_before_the_table() {
        let document = parse_document("<table>stray<tr><td>c</td></tr></table>");
        let table = first(&document, "table");
        let body = body(&document);
        // The stray text is foster-parented out to just before <table>.
        let position = document
            .children(body)
            .iter()
            .position(|id| *id == table)
            .unwrap();
        assert!(matches!(
            &document.node(document.children(body)[position - 1]).kind,
            NodeKind::Text(text) if text == "stray"
        ));
        assert_eq!(document.text_content(table), "c");
    }

    #[test]
    fn table_rows_are_wrapped_in_an_implied_tbody() {
        let document = parse_document("<table><tr><td>a</td></tr></table>");
        let table = first(&document, "table");
        let tbody = document.children(table)[0];
        assert_eq!(document.element(tbody).unwrap().tag_name, "tbody");
        let tr = document.children(tbody)[0];
        assert_eq!(document.element(tr).unwrap().tag_name, "tr");
    }

    #[test]
    fn full_named_entity_table_is_supported() {
        let document =
            parse_document("<p>&copy;&nbsp;&auml;&CounterClockwiseContourIntegral;&notin;</p>");
        let p = first(&document, "p");
        assert_eq!(document.text_content(p), "©\u{a0}ä∳∉");
    }

    #[test]
    fn entities_decode_in_attributes() {
        let document = parse_document("<p title=\"5 &lt; 6 &amp; 7\">x</p>");
        let p = first(&document, "p");
        assert_eq!(
            document.element(p).unwrap().attributes.get("title"),
            Some("5 < 6 & 7")
        );
    }

    #[test]
    fn parses_multi_megabyte_document() {
        // A few MB of repetitive markup (elements, attributes, entity
        // references, void elements) plus a large raw-text script body:
        // parsing must stay correct at scale.
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
        let section = first(&document, "section");
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

    // -- Streaming (incremental) parsing -----------------------------------

    /// A document mixing structure, entities, raw text and multi-byte
    /// UTF-8, so chunk boundaries fall on every kind of tokenizer state.
    fn streaming_fixture() -> String {
        let mut html = String::from("<!doctype html><title>şık &amp; güzel</title><div class='a'>");
        for index in 0..50 {
            html.push_str(&format!(
                "<p data-i=\"{index}\">metin çğıöşü &copy; <b>kalın {index}</b></p>"
            ));
        }
        html.push_str("<script>if (x < y && y > 0) { s = '</p>'; }</script>");
        html.push_str("<table>stray<tr><td>hücre</td></tr></table><ul><li>a<li>b</ul>");
        html
    }

    #[test]
    fn streaming_matches_one_shot_at_every_split_point() {
        // Every two-chunk split of a tricky document must produce the
        // one-shot tree, byte-exact (Document: PartialEq).
        let html = streaming_fixture();
        let expected = parse_document(&html);
        for split in 0..=html.len() {
            let mut parser = StreamingParser::new();
            parser.feed(&html.as_bytes()[..split]);
            parser.feed(&html.as_bytes()[split..]);
            assert_eq!(parser.finish(), expected, "split at byte {split}");
        }
    }

    #[test]
    fn streaming_matches_one_shot_for_random_chunkings() {
        // Property-style test: pseudo-random chunk sizes (deterministic
        // LCG) over several fixtures, including a large one.
        let mut fixtures = vec![
            String::new(),
            "<p>x</p>".to_string(),
            streaming_fixture(),
            "<div>tail".to_string(),
        ];
        let row = "<div class=\"row\"><span>metin &amp; devam çğıöşü</span><br></div>";
        fixtures.push(row.repeat(2_000));
        for html in &fixtures {
            let expected = parse_document(html);
            // Several seeds → different chunk-size sequences per fixture.
            for mut state in [1u64, 42, 0xdead_beef, 9_999] {
                let mut parser = StreamingParser::new();
                let mut offset = 0;
                let bytes = html.as_bytes();
                while offset < bytes.len() {
                    // LCG; chunk sizes 1..=97 bytes split multi-byte
                    // UTF-8 sequences constantly.
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let size = (state >> 33) as usize % 97 + 1;
                    let end = (offset + size).min(bytes.len());
                    parser.feed(&bytes[offset..end]);
                    offset = end;
                }
                assert_eq!(parser.finish(), expected, "fixture of {} bytes", html.len());
            }
        }
    }

    #[test]
    fn mid_parse_snapshot_is_a_valid_partial_document() {
        let html = streaming_fixture();
        let half = html.len() / 2;
        let mut parser = StreamingParser::new();
        parser.feed(&html.as_bytes()[..half]);
        let partial = parser.snapshot();
        // The skeleton is synthesized up front; the elements fed so far
        // are present (still-open ones included) and nothing panics.
        let tags = tags_in_order(&partial);
        assert!(tags.first().is_some_and(|tag| tag == "html"));
        assert!(tags.contains(&"title".to_string()));
        assert!(tags.contains(&"p".to_string()));
        assert!(!tags.contains(&"table".to_string()));
        // Snapshots do not disturb the parser: finishing still yields
        // the one-shot tree.
        parser.feed(&html.as_bytes()[half..]);
        assert_eq!(parser.finish(), parse_document(&html));
    }

    #[test]
    fn streaming_recovers_from_lossy_utf8_like_one_shot() {
        // Invalid bytes become U+FFFD in both paths.
        let bytes = b"<p>a\xff\xfez</p>";
        let mut parser = StreamingParser::new();
        parser.feed(&bytes[..4]);
        parser.feed(&bytes[4..]);
        let mut streamed = parser.finish();
        let mut one_shot = parse_document(&String::from_utf8_lossy(bytes));
        // The lossy decoder counts decode errors per chunk, the one-shot
        // path per input — only the trees must match.
        streamed.set_parse_error_count(0);
        one_shot.set_parse_error_count(0);
        assert_eq!(streamed, one_shot);
    }
}
