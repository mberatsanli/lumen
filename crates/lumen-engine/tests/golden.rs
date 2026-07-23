//! Golden-file tests: the example pages must render to byte-identical SVG.
//!
//! When an intentional rendering change happens, regenerate the goldens:
//!
//! ```bash
//! cargo run -p lumen-cli -- render examples/hello.html crates/lumen-engine/tests/golden/hello.svg
//! cargo run -p lumen-cli -- render examples/card.html crates/lumen-engine/tests/golden/card.svg
//! cargo run -p lumen-cli -- render examples/nested.html crates/lumen-engine/tests/golden/nested.svg
//! cargo run -p lumen-cli -- render examples/kitchen-sink.html crates/lumen-engine/tests/golden/kitchen-sink.svg
//! cargo run -p lumen-cli -- render examples/css-test.html crates/lumen-engine/tests/golden/css-test.svg
//! ```
//! and review the diff before committing.

use lumen_engine::{Size, build_page, render_svg};

const VIEWPORT: Size = Size {
    width: 1024.0,
    height: 768.0,
};

fn assert_golden(name: &str, html: &str, golden: &str) {
    let page = build_page(html, VIEWPORT);
    let svg = render_svg(&page);
    assert_eq!(
        svg, golden,
        "{name}: SVG output differs from the golden file; \
         regenerate goldens if the change is intentional (see file header)"
    );
}

#[test]
fn hello_renders_to_golden_svg() {
    assert_golden(
        "hello",
        include_str!("../../../examples/hello.html"),
        include_str!("golden/hello.svg"),
    );
}

#[test]
fn card_renders_to_golden_svg() {
    assert_golden(
        "card",
        include_str!("../../../examples/card.html"),
        include_str!("golden/card.svg"),
    );
}

#[test]
fn nested_renders_to_golden_svg() {
    assert_golden(
        "nested",
        include_str!("../../../examples/nested.html"),
        include_str!("golden/nested.svg"),
    );
}

/// The whole CSS feature showcase: selectors, flexbox (wrap/shrink/
/// align-self), borders, radii, colors with alpha, opacity, positioning,
/// overflow clipping, pre/monospace text, floats — one byte-exact
/// regression net over everything the engine supports.
#[test]
fn kitchen_sink_renders_to_golden_svg() {
    assert_golden(
        "kitchen-sink",
        include_str!("../../../examples/kitchen-sink.html"),
        include_str!("golden/kitchen-sink.svg"),
    );
}

/// Rendering twice must produce identical output (determinism guard).
#[test]
fn rendering_is_deterministic() {
    let html = include_str!("../../../examples/card.html");
    let first = render_svg(&build_page(html, VIEWPORT));
    let second = render_svg(&build_page(html, VIEWPORT));
    assert_eq!(first, second);
}

/// The CSS feature board: every supported property family in labelled
/// sections (colors, backgrounds, box model, text, layout, flex, grid,
/// table, transforms, filters, forms, cursors, pointer-events, selectors,
/// logical props, media queries, pseudo-elements). The board is also the
/// determinism check for the full pipeline on a large page.
#[test]
fn css_test_renders_to_golden_svg() {
    assert_golden(
        "css-test",
        include_str!("../../../examples/css-test.html"),
        include_str!("golden/css-test.svg"),
    );
}

/// The feature board renders byte-identically on repeat runs.
#[test]
fn css_test_rendering_is_deterministic() {
    let html = include_str!("../../../examples/css-test.html");
    let first = render_svg(&build_page(html, VIEWPORT));
    let second = render_svg(&build_page(html, VIEWPORT));
    assert_eq!(first, second);
}
