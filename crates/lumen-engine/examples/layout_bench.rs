//! Times layout + paint over a real page with a real font measurer:
//! parse HTML, collect author CSS, compute styles, then measure
//! `layout_document` and `build_display_list` separately. Usage:
//! `cargo run --release --example layout_bench -- path/to/page.html [width]`

use lumen_engine::{
    Size, SystemFont, TextMetrics, TextStyle, build_display_list, compute_styles, layout_document,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Counts measure calls and total measured chars, to gauge text-measure
/// redundancy inside a single layout pass.
struct CountingMeasurer<'a> {
    inner: &'a SystemFont,
    calls: AtomicU64,
    chars: AtomicU64,
}

impl lumen_engine::TextMeasurer for CountingMeasurer<'_> {
    fn measure(&self, text: &str, style: &TextStyle) -> TextMetrics {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.chars
            .fetch_add(text.chars().count() as u64, Ordering::Relaxed);
        self.inner.measure(text, style)
    }

    fn normal_line_height(&self, style: &TextStyle) -> Option<f32> {
        self.inner.normal_line_height(style)
    }

    fn content_extent(&self, style: &TextStyle) -> Option<(f32, f32)> {
        self.inner.content_extent(style)
    }

    fn x_height(&self, style: &TextStyle) -> Option<f32> {
        self.inner.x_height(style)
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: layout_bench <html> [width]");
    let width: f32 = std::env::args()
        .nth(2)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(1280.0);
    let html = std::fs::read_to_string(path).expect("read html");

    let document = lumen_html::parse_document(&html);
    let css = lumen_engine::collect_author_css(&document, |_| None);
    let stylesheet = lumen_css::parse_stylesheet(&css);
    let styles = compute_styles(&document, &stylesheet);
    let images = lumen_engine::ImageMap::new();
    let viewport = Size {
        width,
        height: 800.0,
    };
    let font = SystemFont::load_default().expect("a system font");
    let counting = CountingMeasurer {
        inner: &font,
        calls: AtomicU64::new(0),
        chars: AtomicU64::new(0),
    };

    let nodes = document
        .descendants(document.root())
        .filter(|node| document.element(*node).is_some())
        .count();
    println!("elements: {nodes}, viewport: {width}x800");

    let runs: u32 = std::env::var("LAYOUT_BENCH_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    for run in 0..runs {
        let start = Instant::now();
        let layout = if std::env::var_os("HEURISTIC").is_some() {
            layout_document(
                &document,
                &styles,
                viewport,
                &lumen_engine::HeuristicMeasurer,
                &images,
            )
        } else {
            layout_document(&document, &styles, viewport, &counting, &images)
        };
        let layout_time = start.elapsed();

        let start = Instant::now();
        let list = build_display_list(&layout, &images);
        let paint_time = start.elapsed();

        println!(
            "run #{run}: layout {layout_time:?}, paint {paint_time:?}, total {:?} ({} commands), \
             measures: {} calls / {} chars",
            layout_time + paint_time,
            list.len(),
            counting.calls.swap(0, Ordering::Relaxed),
            counting.chars.swap(0, Ordering::Relaxed)
        );
    }
}
