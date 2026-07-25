//! Deterministic stress harness for the CSS parser.
//!
//! Same approach as the HTML stress suite: a seeded, dependency-free
//! generator beats on `parse_stylesheet` / `parse_declarations` /
//! `parse_selector` with malformed, adversarial, and numerically extreme
//! input. Fixed seeds make every run reproducible; a failure panics with
//! its seed and input, which doubles as the minimized repro.

use lumen_css::{Color, CssValue, parse_declarations, parse_selector, parse_stylesheet};

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

const PROPERTIES: &[&str] = &[
    "width",
    "height",
    "min-width",
    "max-height",
    "margin",
    "margin-left",
    "padding",
    "padding-top",
    "border",
    "border-width",
    "border-radius",
    "display",
    "position",
    "top",
    "left",
    "right",
    "bottom",
    "float",
    "clear",
    "color",
    "background",
    "background-color",
    "background-image",
    "font",
    "font-size",
    "font-weight",
    "font-family",
    "line-height",
    "text-align",
    "text-decoration",
    "vertical-align",
    "overflow",
    "z-index",
    "opacity",
    "flex",
    "flex-direction",
    "flex-wrap",
    "flex-grow",
    "justify-content",
    "align-items",
    "gap",
    "grid-template-columns",
    "grid-column",
    "transform",
    "transition",
    "animation",
    "content",
    "list-style",
    "white-space",
    "box-shadow",
    "text-shadow",
    "visibility",
    "cursor",
    "box-sizing",
    "unknown-property",
    "--custom-prop",
    "-webkit-flex",
];

const VALUES: &[&str] = &[
    "0",
    "1px",
    "-5px",
    "999999999999999999px",
    "1e30px",
    "-1e30em",
    "1e-30vh",
    "0.000000001%",
    "3.4028235e38px",
    "1e309px",
    "50%",
    "100vw",
    "calc(100% - 10px)",
    "calc(1px + )",
    "calc()",
    "calc((((((1px))))))",
    "auto",
    "none",
    "inherit",
    "initial",
    "unset",
    "red",
    "#fff",
    "#ffff",
    "#ff000080",
    "#12345",
    "#1234567",
    "#gggggg",
    "#",
    "rgb(255, 0, 0)",
    "rgb(300, -5, 1e10)",
    "rgba(0, 0, 0, 2)",
    "rgba(0,0,0,)",
    "hsl(120, 50%, 50%)",
    "hsla(720deg, 200%, -50%, 5)",
    "url(http://example.com/x.png)",
    "url()",
    "url(unclosed",
    "linear-gradient(red, blue)",
    "linear-gradient(to left, #fff 10%, rgb(1,2,3) 1e10%)",
    "radial-gradient(circle at 10% 20%, red, blue)",
    "repeating-linear-gradient(45deg, red 0 10px, blue 10px 20px)",
    "translate(10px, 20px)",
    "rotate(45deg)",
    "rotate(1e30turn)",
    "scale(2)",
    "scale(-1e10, 0)",
    "matrix(1, 0, 0, 1, 0, 0)",
    "matrix(1e30, 1e-30, -1e30, 0, 999999999, -999999999)",
    "skew(89deg, -89deg)",
    "1px 2px 3px 4px 5px",
    "solid",
    "1px solid red",
    "1e30px dashed #123456",
    "bold",
    "900",
    "1e10",
    "16px/1.5 serif",
    "Arial, 'Times New Roman', sans-serif",
    "\"unterminated",
    "'quo\\'te'",
    "attr(data-x)",
    "var(--x)",
    "var(--x, 10px)",
    "var(--undefined, var(--also-undefined, red))",
    "counter(item)",
    "url(a) url(b) url(c)",
    "\u{0}\u{1}\u{8}",
    "çğı şİı 🦀",
    "\\61 bc",
    "10px !important",
    "!important",
    "10px ! important",
    "10px !IMPORTANT",
    "10px !bogus",
    "1px;",
    "1px}",
    "1px{",
];

const SELECTOR_PARTS: &[&str] = &[
    "div",
    "p",
    "*",
    ".a",
    ".b",
    "#id",
    "#main",
    ":hover",
    ":active",
    ":focus",
    ":checked",
    ":disabled",
    ":first-child",
    ":last-child",
    ":nth-child(2n+1)",
    ":nth-child(1e10)",
    ":nth-child(odd)",
    ":nth-child(-n+3)",
    ":not(.a)",
    ":not(:not(div))",
    ":root",
    ":empty",
    "::before",
    "::after",
    "::marker",
    "::first-line",
    ":link",
    ":visited",
    "input[type='text']",
    "[href]",
    "[href='x']",
    "[class~='a b']",
    "[lang|='en']",
    "[src^='http']",
    "[alt$='.png']",
    "[title*='x']",
    "div.a#b",
    "ul li",
    "div > p",
    "h1 + p",
    "h2 ~ p",
    "a:b",
    "#",
    ".",
    ":",
    "::",
    ":",
    ":nth-child(",
    "[",
    "]",
    "[=",
    "..a",
    "##b",
    ":bogus-pseudo",
    ":nth-child(xyz)",
];

const AT_RULES: &[&str] = &[
    "@media (max-width: 600px)",
    "@media (min-width: 1e30px)",
    "@media (max-width: -5px)",
    "@media screen and (min-width: 100px)",
    "@media (max-width: 600px",
    "@media",
    "@media (",
    "@keyframes spin",
    "@keyframes",
    "@font-face",
    "@import url(x.css)",
    "@import",
    "@charset \"utf-8\"",
    "@supports (display: grid)",
    "@bogus-rule foo bar",
    "@",
];

/// Broken fragments spliced in raw to hit error-recovery paths.
const BROKEN: &[&str] = &[
    "{",
    "}",
    "}",
    "{",
    "{{{",
    "}}}",
    ";",
    ";;",
    ":",
    "::",
    "(",
    ")",
    "(()",
    "))",
    "[",
    "]",
    "/*",
    "*/",
    "/* unterminated",
    "<!--",
    "-->",
    "\"",
    "'",
    "\\",
    "\\61",
    "@",
    "#",
    ".",
    ",",
    ",,,",
    "!",
    "!important",
    "url(",
    "calc(",
    "rgba(",
    "var(",
    "{ color: red",
];

fn generate_value(rng: &mut Rng) -> String {
    let mut value = String::new();
    for _ in 0..1 + rng.below(3) {
        if !value.is_empty() && rng.chance(70) {
            value.push(' ');
        }
        value.push_str(rng.pick(VALUES));
    }
    value
}

fn generate_declaration(rng: &mut Rng) -> String {
    let mut declaration = String::new();
    declaration.push_str(rng.pick(PROPERTIES));
    if rng.chance(90) {
        declaration.push(':');
        declaration.push_str(&generate_value(rng));
    }
    declaration
}

fn generate_selector(rng: &mut Rng) -> String {
    let mut selector = String::new();
    for _ in 0..1 + rng.below(4) {
        if !selector.is_empty() {
            selector.push_str(rng.pick(&[" ", ">", "+", "~", ",", " ", "  "]));
        }
        selector.push_str(rng.pick(SELECTOR_PARTS));
    }
    selector
}

fn generate_stylesheet(rng: &mut Rng) -> String {
    let mut css = String::new();
    let budget = 3 + rng.below(14);
    for _ in 0..budget {
        match rng.below(8) {
            0..=4 => {
                css.push_str(&generate_selector(rng));
                css.push('{');
                for _ in 0..rng.below(5) {
                    css.push_str(&generate_declaration(rng));
                    if rng.chance(80) {
                        css.push(';');
                    }
                }
                if rng.chance(90) {
                    css.push('}');
                }
            }
            5 => {
                // At-rule, possibly with a nested block.
                css.push_str(rng.pick(AT_RULES));
                match rng.below(3) {
                    0 => {
                        css.push('{');
                        css.push_str(rng.pick(&["div", "from", "to", "50%", "0%"]));
                        css.push('{');
                        css.push_str(&generate_declaration(rng));
                        css.push_str("}}");
                    }
                    1 => css.push(';'),
                    _ => {}
                }
            }
            6 => css.push_str(rng.pick(BROKEN)),
            _ => {
                // Raw bytes near UTF-8 boundaries, lossy-decoded.
                let bytes: Vec<u8> = (0..1 + rng.below(16))
                    .map(|_| {
                        if rng.chance(50) {
                            rng.next() as u8
                        } else {
                            *b"{}:;()@#.\"'\xC3\x28\xED\xA0\x80\xF4\x90\x80\x80\xFF\xFE"
                                .get(rng.below(22))
                                .unwrap_or(&b'x')
                        }
                    })
                    .collect();
                css.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
    }
    css
}

/// Runs `check` over `seeds`, reporting the failing seed and its input.
fn for_each_seed(seeds: u64, check: impl Fn(u64, &str)) {
    for seed in 0..seeds {
        let mut rng = Rng::new(seed);
        let css = generate_stylesheet(&mut rng);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            check(seed, &css);
        }));
        if let Err(payload) = result {
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("<non-string panic>");
            panic!("seed {seed} panicked: {message}\ninput: {css:?}");
        }
    }
}

#[test]
fn stress_parse_stylesheet_never_panics() {
    for_each_seed(2000, |seed, css| {
        let sheet = parse_stylesheet(css);
        // Exercise the follow-up APIs: media filtering at extreme
        // viewport widths and keyframes lookup.
        let widths = [0.0, -1.0, 600.0, 1e30, f32::MAX, f32::MIN_POSITIVE];
        let _ = sheet.for_width(widths[seed as usize % widths.len()]);
        let _ = sheet.keyframes("spin");
    });
}

#[test]
fn stress_parse_declarations_never_panics() {
    for seed in 0..2000u64 {
        let mut rng = Rng::new(seed ^ 0xDEC1_A3A7);
        let mut source = String::new();
        for _ in 0..rng.below(8) {
            source.push_str(&generate_declaration(&mut rng));
            source.push(';');
            if rng.chance(20) {
                source.push_str(rng.pick(BROKEN));
            }
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = parse_declarations(&source);
        }));
        if let Err(payload) = result {
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("<non-string panic>");
            panic!("seed {seed} panicked: {message}\ninput: {source:?}");
        }
    }
}

#[test]
fn stress_parse_selector_never_panics() {
    for seed in 0..2000u64 {
        let mut rng = Rng::new(seed ^ 0x5E1E_C70A);
        let source = generate_selector(&mut rng);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = parse_selector(&source);
        }));
        if let Err(payload) = result {
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("<non-string panic>");
            panic!("seed {seed} panicked: {message}\ninput: {source:?}");
        }
    }
}

#[test]
fn stress_value_and_color_parsing_never_panics() {
    for seed in 0..1000u64 {
        let mut rng = Rng::new(seed ^ 0xC010_5EED);
        let value = generate_value(&mut rng);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = CssValue::parse_component(&value);
            let _ = Color::parse(&value);
        }));
        if let Err(payload) = result {
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("<non-string panic>");
            panic!("seed {seed} panicked: {message}\ninput: {value:?}");
        }
    }
}

/// Huge declaration blocks: the parser must stay linear-ish and never
/// recurse past the stack.
#[test]
fn stress_huge_inputs_stay_bounded() {
    // 20k declarations in one block.
    let mut big = String::from("div{");
    for index in 0..20_000 {
        big.push_str(&format!("margin: {index}px; color: #{index:06x};"));
    }
    big.push('}');
    let sheet = parse_stylesheet(&big);
    assert!(!sheet.rules.is_empty());

    // 5k selectors in one rule.
    let selectors = (0..5_000)
        .map(|index| format!(".c{index} > :nth-child({index}n+1)"))
        .collect::<Vec<_>>()
        .join(",");
    let sheet = parse_stylesheet(&format!("{selectors}{{ color: red }}"));
    assert!(!sheet.rules.is_empty());

    // Deeply nested calc() and var().
    let deep_calc = format!("width: {}1px{}", "calc(".repeat(500), ")".repeat(500));
    let deep_var = format!("color: var(--a{})", ", var(--b".repeat(500));
    let _ = parse_declarations(&deep_calc);
    let _ = parse_declarations(&deep_var);
}
