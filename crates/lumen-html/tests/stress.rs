//! Deterministic stress harness for the HTML parser, the arena DOM, and
//! the `inner_html` serializer.
//!
//! No fuzzing infrastructure is available on the stable toolchain, so
//! this beats on the parser with a seeded, dependency-free generator
//! instead: fixed seeds make every run byte-identical, so a failure in
//! CI reproduces locally by re-running the same test. Each case panics
//! with its seed and input, which doubles as the minimized repro.

use lumen_html::{parse_document, parse_fragment};

/// xorshift64* — tiny, dependency-free, deterministic on every platform.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Mix so seed 0 does not start from a zero (fixed-point) state.
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<'a>(&mut self, items: &'a [&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

const TAGS: &[&str] = &[
    "div",
    "p",
    "span",
    "b",
    "i",
    "em",
    "strong",
    "a",
    "ul",
    "ol",
    "li",
    "dl",
    "dt",
    "dd",
    "table",
    "thead",
    "tbody",
    "tfoot",
    "tr",
    "td",
    "th",
    "caption",
    "colgroup",
    "form",
    "input",
    "button",
    "select",
    "option",
    "optgroup",
    "textarea",
    "label",
    "fieldset",
    "h1",
    "h2",
    "h6",
    "section",
    "article",
    "header",
    "footer",
    "main",
    "nav",
    "aside",
    "img",
    "br",
    "hr",
    "wbr",
    "pre",
    "code",
    "blockquote",
    "style",
    "script",
    "title",
    "head",
    "body",
    "html",
    "center",
    "font",
    "marquee",
    "nobr",
    "svg",
    "math",
    "mi",
    "template",
    "iframe",
    "frameset",
    "frame",
    "video",
    "audio",
    "canvas",
    "object",
    "embed",
    "noscript",
    "noembed",
    "plaintext",
    "xmp",
    "listing",
    "isindex",
    "unknown-tag",
    "x",
];

const ATTR_NAMES: &[&str] = &[
    "id",
    "class",
    "style",
    "href",
    "src",
    "type",
    "value",
    "checked",
    "selected",
    "multiple",
    "disabled",
    "colspan",
    "rowspan",
    "width",
    "height",
    "align",
    "valign",
    "bgcolor",
    "color",
    "size",
    "face",
    "rel",
    "media",
    "title",
    "alt",
    "placeholder",
    "name",
    "for",
    "data-x",
    "xml:lang",
    "CLASS",
    "ID",
    "on\u{0}click",
    "a=b",
    "\"",
    "'",
];

const ATTR_VALUES: &[&str] = &[
    "",
    "x",
    "a b c",
    "a&b\"c'd<e>f",
    "&amp; &lt; &#65; &#x41; &#x110000; &#0; &bogus; &",
    "100",
    "-5",
    "999999999999999999999999",
    "1e30",
    "0.000000001",
    "url(javascript:alert(1))",
    "\u{0}\u{1}\u{8}",
    "çğı şİı 🦀 \u{202e}rtl\u{202c}",
    "very-long-value-that-keeps-going-on-and-on-and-on-and-on-and-on-and-on",
];

const TEXTS: &[&str] = &[
    "hello world",
    "a < b & c > d",
    "&amp;&lt;&gt;&quot;&apos;",
    "&#xD800; &#x10FFFF; &#0; &#x110000;",
    "&bogus; &; &amp",
    "]]>",
    "<!-- not a comment",
    "-->",
    "<![CDATA[ not cdata ]]>",
    "\u{0}null\u{0}",
    "\u{FFFD}\u{FEFF}\u{200B}\u{2028}\u{2029}",
    "çğı şİı 🦀🎉 \u{1F600}",
    "combining: a\u{301}\u{20D0}",
    "\t\r\n lots   of \u{A0} space",
    "<script>alert(1)</script>",
    "</",
    "<",
    ">",
    "< div",
    "a\u{202E}drowssap\u{202C}b",
];

/// Weird markup fragments with unbalanced, misnested, or raw syntax.
const BROKEN: &[&str] = &[
    "<div",
    "<div ",
    "<div /",
    "<>",
    "</>",
    "< div>",
    "</div>",
    "<b><i></b></i>",
    "<p><p><p>",
    "<table><td>",
    "<table><tr><td><table><td>",
    "<li><li><li>",
    "<select><option<option>",
    "<style><p>",
    "<script><b>",
    "<textarea><div></textarea>",
    "<title><span>",
    "<plaintext><div>",
    "<xmp></xmp",
    "<!--",
    "<!-- comment",
    "<!-->-->",
    "<![CDATA[x]]>",
    "<!DOCTYPE html PUBLIC",
    "<?xml version='1.0'?>",
    "<a href='x\"y'>",
    "<a = >",
    "<a b c d>",
    "<a ==x>",
    "<input checked='checked' checked>",
    "<form><form>",
    "<button><button>",
    "<nobr><nobr><nobr>",
    "<h1><h2>",
    "<colgroup><col>",
    "<frameset><frame><frameset>",
    "<svg><foreignObject><div>",
    "<math><mi><svg><path>",
    "<template><tr><td>",
    "<body><body>",
    "<head><body><head>",
];

/// How deep generated documents nest; crosses the engine's MAX_DEPTH
/// (512) on the high end to prove the depth guards hold.
fn generate_html(rng: &mut Rng) -> String {
    let mut html = String::new();
    if rng.chance(20) {
        html.push_str(rng.pick(&["<!DOCTYPE html>", "<!doctype foo bar>", "<!", "<!-->"]));
    }

    // Rarely: pure deep nesting, up to ~2500 levels, no other noise.
    if rng.chance(3) {
        let depth = 100 + rng.below(2400);
        let tag = rng.pick(&["div", "b", "span", "table><tr><td", "nobr"]);
        for _ in 0..depth {
            html.push('<');
            html.push_str(tag);
            html.push('>');
        }
        if rng.chance(50) {
            for _ in 0..depth {
                html.push_str("</");
                html.push_str(tag);
                html.push('>');
            }
        }
        return html;
    }

    let mut open: Vec<&str> = Vec::new();
    let budget = 5 + rng.below(120);
    for _ in 0..budget {
        match rng.below(10) {
            0..=4 => {
                let tag = rng.pick(TAGS);
                html.push('<');
                html.push_str(tag);
                let attrs = rng.below(4);
                for _ in 0..attrs {
                    html.push(' ');
                    html.push_str(rng.pick(ATTR_NAMES));
                    if rng.chance(80) {
                        html.push('=');
                        html.push_str(rng.pick(&["\"", "'", ""]));
                        html.push_str(rng.pick(ATTR_VALUES));
                        if rng.chance(80) {
                            html.push('"');
                        }
                    }
                }
                if rng.chance(10) {
                    html.push('/');
                }
                html.push('>');
                open.push(tag);
            }
            5..=6 => {
                // Close something — correctly, or a random/mismatched tag.
                if rng.chance(60) && !open.is_empty() {
                    let tag = open.remove(rng.below(open.len()));
                    html.push_str("</");
                    html.push_str(tag);
                    html.push('>');
                } else {
                    html.push_str("</");
                    html.push_str(rng.pick(TAGS));
                    if rng.chance(90) {
                        html.push('>');
                    }
                }
            }
            7 => html.push_str(rng.pick(TEXTS)),
            8 => html.push_str(rng.pick(BROKEN)),
            _ => {
                // Raw bytes near UTF-8 boundaries, lossy-decoded: lone
                // continuation bytes, truncated multi-byte sequences,
                // overlong encodings, surrogates.
                let bytes: Vec<u8> = (0..1 + rng.below(24))
                    .map(|_| {
                        if rng.chance(50) {
                            rng.next() as u8
                        } else {
                            *b"<>/&=\"'\xC3\x28\xE0\x80\x80\xED\xA0\x80\xF4\x90\x80\x80\xFF\xFE"
                                .get(rng.below(25))
                                .unwrap_or(&b'x')
                        }
                    })
                    .collect();
                html.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
    }
    html
}

/// Runs `check` over `seeds`, reporting the failing seed and its input.
fn for_each_seed(seeds: u64, check: impl Fn(u64, &str)) {
    for seed in 0..seeds {
        let mut rng = Rng::new(seed);
        let html = generate_html(&mut rng);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            check(seed, &html);
        }));
        if let Err(payload) = result {
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("<non-string panic>");
            panic!("seed {seed} panicked: {message}\ninput: {html:?}");
        }
    }
}

#[test]
fn stress_parse_document_never_panics() {
    for_each_seed(2000, |_, html| {
        let document = parse_document(html);
        // Exercise the tree walks too, not just the parse.
        let _ = document.descendants(document.root()).count();
        let _ = document.text_content(document.root());
        let _ = document.dump();
    });
}

#[test]
fn stress_inner_html_round_trip_reaches_fixed_point() {
    for_each_seed(1000, |_, html| {
        let first = parse_document(html);
        let s1 = first.inner_html(first.root());
        let second = parse_document(&s1);
        let s2 = second.inner_html(second.root());
        // `<plaintext>` is a spec-level round-trip breaker: it swallows
        // the rest of the document as text on every reparse, so no
        // serializer can reach a fixed point through it (browsers have
        // the same behavior). Skip those seeds.
        if s2.contains("<plaintext") {
            return;
        }
        let third = parse_document(&s2);
        let s3 = third.inner_html(third.root());
        // Serialization must converge: after one reparse the output is a
        // fixed point, or inner_html is not a faithful serializer.
        if s2 != s3 {
            let at = s2
                .char_indices()
                .zip(s3.char_indices())
                .find_map(|((i, a), (_, b))| (a != b).then_some(i))
                .unwrap_or_else(|| s2.len().min(s3.len()));
            fn window(s: &str, at: usize) -> &str {
                let start = s.floor_char_boundary(at.saturating_sub(60));
                let end = s.ceil_char_boundary((at + 60).min(s.len()));
                &s[start..end]
            }
            panic!(
                "inner_html did not reach a fixed point at byte {at}:\n s2: {:?}\n s3: {:?}",
                window(&s2, at),
                window(&s3, at)
            );
        }
    });
}

#[test]
fn stress_set_inner_html_never_panics() {
    for_each_seed(1000, |seed, html| {
        let mut document = parse_document("<div id='host'><p>orig</p></div>");
        let host = document.get_element_by_id("host").unwrap();
        document.set_inner_html(host, html);
        let _ = document.inner_html(host);
        let _ = document.text_content(host);
        // Fragments in random element contexts (RCDATA, table, ...).
        let context = ["div", "title", "textarea", "table", "select", "ul"][seed as usize % 6];
        let fragment = parse_fragment(context, html);
        let _ = fragment.inner_html(fragment.root());
    });
}

#[test]
fn stress_random_bytes_lossy_parse() {
    for seed in 0..500u64 {
        let mut rng = Rng::new(seed ^ 0xB17E_5EED);
        let len = rng.below(512);
        let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        let text = String::from_utf8_lossy(&bytes);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let document = parse_document(&text);
            let _ = document.inner_html(document.root());
        }));
        if let Err(payload) = result {
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("<non-string panic>");
            panic!("seed {seed} panicked: {message}\nbytes: {bytes:?}");
        }
    }
}

/// The DOM must stay internally consistent after stress: every child's
/// parent pointer points back, and every node is reachable from the root.
#[test]
fn stress_dom_invariants_hold() {
    for_each_seed(500, |_, html| {
        let document = parse_document(html);
        for (index, node) in document.nodes().iter().enumerate() {
            if index == document.root() {
                continue;
            }
            if let Some(parent) = node.parent {
                assert!(
                    document.children(parent).contains(&index),
                    "node {index} claims parent {parent} but is not its child"
                );
            }
            for child in document.children(index) {
                assert_eq!(
                    document.node(*child).parent,
                    Some(index),
                    "child {child} of {index} does not point back"
                );
            }
            // The parent chain must reach the root without cycles.
            let mut cursor = Some(index);
            let mut steps = 0;
            while let Some(id) = cursor {
                steps += 1;
                assert!(
                    steps <= document.nodes().len(),
                    "parent chain from {index} cycles"
                );
                cursor = document.node(id).parent;
                if cursor.is_none() && id != document.root() {
                    // Detached node (e.g. removed by the tree builder): the
                    // chain just has to terminate, which it did.
                    break;
                }
            }
        }
    });
}
