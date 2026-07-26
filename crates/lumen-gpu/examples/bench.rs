//! CPU vs GPU frame-time comparison for the example pages.
//!
//! ```bash
//! cargo run -p lumen-gpu --example bench --release
//! ```

use lumen_engine::{Size, build_page_with_measurer};
use lumen_gpu::GpuRenderer;
use std::time::Instant;

const VIEWPORT: Size = Size {
    width: 1024.0,
    height: 768.0,
};
const ITERATIONS: u32 = 100;

fn bench_page(name: &str, html: &str, font: Option<&lumen_engine::SystemFont>) {
    bench_at(name, html, font, 1.0, 1024, 768);
    bench_at(name, html, font, 2.0, 2048, 1536);
}

fn bench_at(
    name: &str,
    html: &str,
    font: Option<&lumen_engine::SystemFont>,
    scale: f32,
    width: u32,
    height: u32,
) {
    let page = build_page_with_measurer(
        html,
        VIEWPORT,
        font.map(|f| f as &dyn lumen_engine::TextMeasurer)
            .unwrap_or(&lumen_engine::HeuristicMeasurer),
    );
    let commands = &page.display_list;
    println!("{name} @{scale}x ({width}x{height}): {} display commands", commands.len());

    // CPU rasterizer.
    for _ in 0..5 {
        let _ = lumen_engine::rasterize_with_fixed_origin(
            commands,
            width,
            height,
            0.0,
            0.0,
            scale,
            font,
        );
    }
    let started = Instant::now();
    for _ in 0..ITERATIONS {
        let _ = lumen_engine::rasterize_with_fixed_origin(
            commands,
            width,
            height,
            0.0,
            0.0,
            scale,
            font,
        );
    }
    let cpu = started.elapsed().as_secs_f64() * 1000.0 / f64::from(ITERATIONS);

    let Some(mut renderer) = GpuRenderer::new_headless() else {
        println!("  CPU: {cpu:.2} ms/frame; no GPU adapter available");
        return;
    };
    // Warm-up: pipeline compilation, atlas/texture uploads.
    for _ in 0..5 {
        let _ = renderer.render_offscreen_no_readback(commands, width, height, 0.0, 0.0, scale, font);
    }
    let started = Instant::now();
    for _ in 0..ITERATIONS {
        let _ = renderer.render_offscreen_no_readback(commands, width, height, 0.0, 0.0, scale, font);
    }
    let gpu = started.elapsed().as_secs_f64() * 1000.0 / f64::from(ITERATIONS);
    let started = Instant::now();
    for _ in 0..ITERATIONS {
        let _ = renderer.render_offscreen(commands, width, height, 0.0, 0.0, scale, font);
    }
    let gpu_readback = started.elapsed().as_secs_f64() * 1000.0 / f64::from(ITERATIONS);

    println!(
        "  CPU: {cpu:.2} ms | GPU: {gpu:.2} ms (+readback {gpu_readback:.2} ms) | speedup {:.1}x",
        cpu / gpu
    );
}

fn main() {
    let font = lumen_engine::SystemFont::load_default();
    bench_page(
        "kitchen-sink",
        include_str!("../../../examples/kitchen-sink.html"),
        font.as_ref(),
    );
    bench_page(
        "css-test",
        include_str!("../../../examples/css-test.html"),
        font.as_ref(),
    );
}
