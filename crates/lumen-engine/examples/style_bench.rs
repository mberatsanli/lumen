//! Times the style pass over a real page: parse HTML, collect author
//! CSS, then measure `compute_styles` alone. Usage:
//! `cargo run --release --example style_bench -- path/to/page.html`

use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("usage: style_bench <html>");
    let html = std::fs::read_to_string(path).expect("read html");

    let start = Instant::now();
    let document = lumen_html::parse_document(&html);
    let parse_html = start.elapsed();

    let start = Instant::now();
    let css = if std::env::var_os("STYLE_BENCH_NO_CSS").is_some() {
        String::new()
    } else {
        lumen_engine::collect_author_css(&document, |_| None)
    };
    let stylesheet = lumen_css::parse_stylesheet(&css);
    let parse_css = start.elapsed();

    let nodes = document
        .descendants(document.root())
        .filter(|node| document.element(*node).is_some())
        .count();
    println!(
        "elements: {nodes}, rules: {}, parse_html: {parse_html:?}, parse_css: {parse_css:?}",
        stylesheet.rules.len()
    );

    for run in 0..3 {
        let start = Instant::now();
        let styles = lumen_engine::compute_styles(&document, &stylesheet);
        println!(
            "style pass #{run}: {:?} ({} nodes)",
            start.elapsed(),
            styles.by_node.len()
        );
    }
}
