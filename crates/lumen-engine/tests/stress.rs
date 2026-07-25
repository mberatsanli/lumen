//! Deterministic end-to-end stress harness for the rendering pipeline.
//!
//! Every seed builds a random/broken HTML+CSS document and runs the full
//! pipeline: parse → style → layout → paint → SVG, then rasterizes the
//! display list into a small framebuffer at extreme scroll offsets. The
//! pipeline is infallible by design — any panic here is a real bug. Fixed
//! seeds make runs byte-identical; a failure panics with its seed and
//! input, which doubles as the minimized repro.

use lumen_engine::{Size, build_page, rasterize, render_svg};

/// Runs the suite on a thread with a main-thread-sized stack: layout
/// recursion up to MAX_DEPTH (512 levels) needs more than the test
/// harness's small per-test thread stacks (the engine's own deep-nesting
/// test in layout.rs does the same). 64 MiB matches that test.
fn run_with_big_stack(body: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(body)
        .expect("spawn stress thread")
        .join()
        .expect("stress thread panicked");
}

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
    "em",
    "strong",
    "a",
    "ul",
    "ol",
    "li",
    "table",
    "thead",
    "tbody",
    "tr",
    "td",
    "th",
    "form",
    "input",
    "button",
    "select",
    "option",
    "textarea",
    "label",
    "h1",
    "h2",
    "h3",
    "section",
    "article",
    "header",
    "footer",
    "main",
    "img",
    "br",
    "hr",
    "pre",
    "code",
    "blockquote",
    "center",
    "font",
    "marquee",
    "details",
    "summary",
    "figure",
    "svg",
];

const ATTR_VALUES: &[&str] = &[
    "x",
    "a b",
    "1",
    "-5",
    "999999999999999999",
    "1e30",
    "checked",
    "selected",
    "text",
    "password",
    "checkbox",
    "radio",
    "submit",
    "number",
    "çğı şİı 🦀",
    "&amp;&lt;",
];

const TEXTS: &[&str] = &[
    "hello world",
    "a < b & c > d",
    "&amp;&lt;&gt;",
    "&#x110000; &#0; &bogus;",
    "\u{0}null\u{0}",
    "çğı şİı 🦀🎉",
    "a\u{202E}drowssap\u{202C}b",
    "one two three four five six seven eight nine ten eleven twelve",
    "\t\r\n  spaced  out \u{A0} text",
    "</",
    "<",
];

const CSS_PROPERTIES: &[&str] = &[
    "width",
    "height",
    "min-width",
    "max-width",
    "min-height",
    "max-height",
    "margin",
    "margin-top",
    "margin-left",
    "padding",
    "padding-left",
    "border",
    "border-width",
    "border-radius",
    "display",
    "position",
    "top",
    "left",
    "float",
    "color",
    "background",
    "background-color",
    "background-image",
    "font-size",
    "font-weight",
    "line-height",
    "text-align",
    "overflow",
    "z-index",
    "opacity",
    "flex",
    "flex-direction",
    "flex-grow",
    "justify-content",
    "align-items",
    "gap",
    "grid-template-columns",
    "grid-column",
    "transform",
    "animation",
    "content",
    "list-style",
    "white-space",
    "box-sizing",
    "visibility",
];

const CSS_VALUES: &[&str] = &[
    "0",
    "1px",
    "-5px",
    "999999999999999999px",
    "1e30px",
    "-1e30em",
    "1e-30vh",
    "0.000000001%",
    "3.4028235e38px",
    "50%",
    "100vw",
    "calc(100% - 10px)",
    "calc(1px + )",
    "auto",
    "none",
    "inherit",
    "red",
    "#fff",
    "#ff000080",
    "#gggggg",
    "rgb(300, -5, 1e10)",
    "rgba(0, 0, 0, 2)",
    "hsl(720, 200%, -50%)",
    "linear-gradient(red, blue)",
    "linear-gradient(to left, #fff 10%, rgb(1,2,3) 1e10%)",
    "radial-gradient(circle, red, blue)",
    "repeating-linear-gradient(45deg, red 0 10px, blue 10px 20px)",
    "url(x.png)",
    "url()",
    "translate(1e30px, -1e30px)",
    "rotate(1e30turn)",
    "scale(-1e10, 0)",
    "matrix(1e30, 1e-30, -1e30, 0, 999999999, -999999999)",
    "block",
    "inline",
    "inline-block",
    "flex",
    "grid",
    "table",
    "absolute",
    "relative",
    "fixed",
    "sticky",
    "1px solid red",
    "1e30px dashed #123456",
    "bold",
    "1e10",
    "hidden",
    "scroll",
    "visible",
    "border-box",
    "nowrap",
    "pre",
    "pre-wrap",
    "\"generated\"",
    "url(a) url(b)",
    "çğı şİı 🦀",
    "10px !important",
];

const SELECTOR_PARTS: &[&str] = &[
    "div",
    "p",
    "*",
    ".a",
    ".b",
    "#id",
    ":hover",
    ":first-child",
    ":last-child",
    ":nth-child(2n+1)",
    ":nth-child(1e10)",
    ":not(.a)",
    "::before",
    "::after",
    "[href]",
    "[type='text']",
    "div.a",
    "ul li",
    "div > p",
    "h1 + p",
    "input:checked",
    ":root",
];

fn generate_css(rng: &mut Rng) -> String {
    let mut css = String::new();
    for _ in 0..rng.below(6) {
        if rng.chance(15) {
            css.push_str("@media (max-width: ");
            css.push_str(rng.pick(&["600px", "1e30px", "-5px", "0px"]));
            css.push_str("){");
        }
        for _ in 0..1 + rng.below(3) {
            css.push_str(rng.pick(SELECTOR_PARTS));
            if rng.chance(40) {
                css.push_str(rng.pick(&[" ", ">", ",", "+"]));
            }
        }
        css.push('{');
        for _ in 0..1 + rng.below(6) {
            css.push_str(rng.pick(CSS_PROPERTIES));
            css.push(':');
            css.push_str(rng.pick(CSS_VALUES));
            if rng.chance(30) {
                css.push(' ');
                css.push_str(rng.pick(CSS_VALUES));
            }
            css.push(';');
        }
        css.push('}');
        if css.matches("{").count() > css.matches('}').count() && rng.chance(80) {
            css.push('}');
        }
    }
    css
}

fn generate_html(rng: &mut Rng) -> String {
    let mut html = String::new();

    // Rarely: pure deep nesting, crossing the engine's MAX_DEPTH (512).
    if rng.chance(4) {
        let depth = 100 + rng.below(1200);
        let tag = rng.pick(&["div", "span", "b", "table><tr><td"]);
        for _ in 0..depth {
            html.push('<');
            html.push_str(tag);
            html.push('>');
        }
        html.push_str("deep");
        return html;
    }

    // A stylesheet is usually embedded so style computation gets work.
    if rng.chance(80) {
        html.push_str("<style>");
        html.push_str(&generate_css(rng));
        html.push_str("</style>");
    }

    let mut open: Vec<&str> = Vec::new();
    let budget = 3 + rng.below(60);
    for _ in 0..budget {
        match rng.below(12) {
            0..=5 => {
                let tag = rng.pick(TAGS);
                html.push('<');
                html.push_str(tag);
                for _ in 0..rng.below(3) {
                    html.push(' ');
                    html.push_str(rng.pick(&[
                        "id", "class", "style", "href", "src", "type", "value", "checked",
                        "selected", "colspan", "rowspan", "width", "height", "align",
                    ]));
                    html.push_str("=\"");
                    // style attributes carry real declarations.
                    if html.ends_with("style=\"") {
                        for _ in 0..rng.below(3) {
                            html.push_str(rng.pick(CSS_PROPERTIES));
                            html.push(':');
                            html.push_str(rng.pick(CSS_VALUES));
                            html.push(';');
                        }
                    } else {
                        html.push_str(rng.pick(ATTR_VALUES));
                    }
                    html.push('"');
                }
                html.push('>');
                open.push(tag);
            }
            6..=7 => {
                if !open.is_empty() {
                    let tag = if rng.chance(70) {
                        open.remove(rng.below(open.len()))
                    } else {
                        rng.pick(TAGS)
                    };
                    html.push_str("</");
                    html.push_str(tag);
                    html.push('>');
                }
            }
            8..=9 => html.push_str(rng.pick(TEXTS)),
            10 => {
                // Broken markup: unclosed, misnested, stray closers.
                html.push_str(rng.pick(&[
                    "<div",
                    "<",
                    "</>",
                    "<b><i></b></i>",
                    "<table><td>",
                    "<li><li>",
                    "</div",
                    "<!--",
                    "<input = >",
                    "<p><p><p>",
                ]));
            }
            _ => {
                // Raw bytes near UTF-8 boundaries, lossy-decoded.
                let bytes: Vec<u8> = (0..1 + rng.below(12))
                    .map(|_| {
                        if rng.chance(50) {
                            rng.next() as u8
                        } else {
                            *b"<>/&=\"'\xC3\x28\xED\xA0\x80\xF4\x90\x80\x80\xFF\xFE"
                                .get(rng.below(18))
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

const VIEWPORTS: &[Size] = &[
    Size {
        width: 0.0,
        height: 0.0,
    },
    Size {
        width: 1.0,
        height: 1.0,
    },
    Size {
        width: 320.0,
        height: 200.0,
    },
    Size {
        width: 1024.0,
        height: 768.0,
    },
    Size {
        width: 50_000.0,
        height: 40_000.0,
    },
    Size {
        width: 0.5,
        height: 100_000.0,
    },
];

const SCROLL_OFFSETS: &[f32] = &[0.0, -50.0, 100.0, 1e9, -1e9, 3.4e38, -3.4e38];

/// Runs one seed through the whole pipeline; separated so the panic
/// reporter can point at the exact seed and input.
fn run_seed(seed: u64) -> String {
    let mut rng = Rng::new(seed);
    let html = generate_html(&mut rng);
    let viewport = VIEWPORTS[rng.below(VIEWPORTS.len())];

    let page = build_page(&html, viewport);
    let svg = render_svg(&page);
    assert!(svg.contains("<svg"), "no svg root in output");

    // Rasterize into a small framebuffer (big enough to exercise the
    // rasterizer, small enough to keep 2000 seeds fast) at an extreme
    // scroll offset.
    let width = 1 + rng.below(96) as u32;
    let height = 1 + rng.below(64) as u32;
    let scroll_y = SCROLL_OFFSETS[rng.below(SCROLL_OFFSETS.len())];
    let framebuffer = rasterize(&page.display_list, width, height, scroll_y);
    let _ = framebuffer.pixel(0, 0);

    html
}

fn stress_seeds(seeds: u64) {
    for seed in 0..seeds {
        if std::env::var_os("STRESS_TRACE").is_some() {
            eprintln!("seed {seed}");
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_seed(seed)));
        if let Err(payload) = result {
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("<non-string panic>");
            // Regenerate the input for the report (run_seed consumed it).
            let html = generate_html(&mut Rng::new(seed));
            panic!("seed {seed} panicked: {message}\ninput: {html:?}");
        }
    }
}

#[test]
fn stress_full_pipeline_never_panics() {
    run_with_big_stack(|| stress_seeds(2000));
}

/// The same generator, re-run at a second viewport for every seed:
/// layout must be as robust on relayout (resize) as on first layout.
#[test]
fn stress_relayout_at_second_viewport_never_panics() {
    run_with_big_stack(|| {
        for seed in 0..500u64 {
            if std::env::var_os("STRESS_TRACE").is_some() {
                eprintln!("seed {seed}");
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut rng = Rng::new(seed);
                let html = generate_html(&mut rng);
                let first = build_page(&html, VIEWPORTS[rng.below(VIEWPORTS.len())]);
                let second = build_page(&html, VIEWPORTS[rng.below(VIEWPORTS.len())]);
                let _ = render_svg(&first);
                let _ = render_svg(&second);
            }));
            if let Err(payload) = result {
                let message = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("<non-string panic>");
                let html = generate_html(&mut Rng::new(seed));
                panic!("seed {seed} panicked: {message}\ninput: {html:?}");
            }
        }
    });
}
