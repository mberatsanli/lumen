//! GPU vs CPU rasterizer parity tests. Every test silently passes when
//! no headless GPU adapter is available (CI machines without GPUs), so
//! the suite is green everywhere and meaningful where a GPU exists.

use lumen_engine::font::SystemFont;
use lumen_engine::paint::{DisplayCommand, GradientKind};
use lumen_engine::style::{BorderStyle, Mark, Transform2D};
use lumen_engine::{Corners, EdgeSizes, Rect, RasterImage};
use lumen_gpu::GpuRenderer;

fn gpu() -> Option<GpuRenderer> {
    GpuRenderer::new_headless()
}

fn fixture_font() -> Option<SystemFont> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../lumen-engine/tests/fixtures/lato.woff2");
    SystemFont::from_bytes(&std::fs::read(path).ok()?)
}

fn rect(x: f32, y: f32, width: f32, height: f32) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn rgb(hex: u32) -> lumen_css::Color {
    lumen_css::Color::rgb(
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
    )
}

/// Per-channel stats between a GPU RGBA frame and the CPU framebuffer.
struct Diff {
    mean: f64,
    /// Fraction of pixels whose worst channel differs by more than 24.
    bad_fraction: f64,
    worst: u32,
}

fn diff(gpu_rgba: &[u8], cpu: &lumen_engine::Framebuffer) -> Diff {
    assert_eq!(
        gpu_rgba.len(),
        (cpu.width * cpu.height * 4) as usize,
        "frame sizes disagree"
    );
    let mut total = 0u64;
    let mut bad = 0u64;
    let mut worst = 0u32;
    for (index, pixel) in cpu.pixels.iter().enumerate() {
        let channels = [
            (pixel >> 16) & 0xff,
            (pixel >> 8) & 0xff,
            pixel & 0xff,
        ];
        let mut pixel_worst = 0u32;
        for (channel, expected) in channels.iter().enumerate() {
            let actual = u32::from(gpu_rgba[index * 4 + channel]);
            let delta = actual.abs_diff(*expected);
            total += u64::from(delta);
            pixel_worst = pixel_worst.max(delta);
        }
        worst = worst.max(pixel_worst);
        if pixel_worst > 24 {
            bad += 1;
        }
    }
    let pixels = cpu.pixels.len() as f64;
    Diff {
        mean: total as f64 / (pixels * 3.0),
        bad_fraction: bad as f64 / pixels,
        worst,
    }
}

/// Renders both backends and asserts parity within tolerance.
#[allow(clippy::too_many_arguments)]
fn assert_parity(
    commands: &[DisplayCommand],
    width: u32,
    height: u32,
    scroll_y: f32,
    fixed_origin: f32,
    font: Option<&SystemFont>,
    max_mean: f64,
    max_bad_fraction: f64,
) {
    let Some(mut renderer) = gpu() else {
        return;
    };
    let Some(rgba) =
        renderer.render_offscreen(commands, width, height, scroll_y, fixed_origin, 1.0, font)
    else {
        return;
    };
    let cpu =
        lumen_engine::rasterize_with_fixed_origin(commands, width, height, scroll_y, fixed_origin, 1.0, font);
    let diff = diff(&rgba, &cpu);
    assert!(
        diff.mean <= max_mean && diff.bad_fraction <= max_bad_fraction,
        "GPU/CPU parity exceeded: mean={:.3} (max {max_mean}), bad={:.4}% (max {:.2}%), worst={}",
        diff.mean,
        diff.bad_fraction * 100.0,
        max_bad_fraction * 100.0,
        diff.worst,
    );
}

#[test]
fn fill_rect_rounded_parity() {
    let commands = vec![
        DisplayCommand::FillRect {
            rect: rect(10.0, 10.0, 200.0, 120.0),
            color: rgb(0xcf3333),
            radius: Corners::uniform(24.0),
        },
        DisplayCommand::FillRect {
            rect: rect(50.5, 60.5, 100.0, 40.0),
            color: lumen_css::Color::rgba(0x33, 0x66, 0xcf, 180),
            radius: Corners {
                top_left: 4.0,
                top_right: 12.0,
                bottom_right: 0.0,
                bottom_left: 20.0,
            },
        },
    ];
    assert_parity(&commands, 240, 160, 0.0, 0.0, None, 1.0, 0.02);
}

#[test]
fn stroke_rect_parity() {
    let commands = vec![
        DisplayCommand::StrokeRect {
            rect: rect(8.0, 8.0, 150.0, 90.0),
            widths: EdgeSizes {
                top: 3.0,
                right: 2.0,
                bottom: 1.0,
                left: 4.0,
            },
            colors: EdgeSizes {
                top: rgb(0x111111),
                right: rgb(0xcc2222),
                bottom: rgb(0x22cc22),
                left: rgb(0x2222cc),
            },
            styles: EdgeSizes::uniform(BorderStyle::Solid),
            radius: Corners::uniform(0.0),
        },
        DisplayCommand::StrokeRect {
            rect: rect(20.0, 120.0, 180.0, 30.0),
            widths: EdgeSizes::uniform(3.0),
            colors: EdgeSizes::uniform(rgb(0x663399)),
            styles: EdgeSizes::uniform(BorderStyle::Dashed),
            radius: Corners::uniform(0.0),
        },
        DisplayCommand::StrokeRect {
            rect: rect(190.0, 20.0, 60.0, 60.0),
            widths: EdgeSizes::uniform(4.0),
            colors: EdgeSizes::uniform(rgb(0xcc7700)),
            styles: EdgeSizes::uniform(BorderStyle::Solid),
            radius: Corners::uniform(12.0),
        },
    ];
    assert_parity(&commands, 280, 170, 0.0, 0.0, None, 1.0, 0.02);
}

#[test]
fn gradient_parity() {
    let stops = vec![
        (rgb(0xff0000), 0.0),
        (rgb(0x00ff00), 0.5),
        (rgb(0x0000ff), 1.0),
    ];
    let commands = vec![
        DisplayCommand::FillGradient {
            rect: rect(0.0, 0.0, 200.0, 60.0),
            radius: Corners::uniform(0.0),
            angle_degrees: 90.0,
            stops: stops.clone(),
            kind: GradientKind::Linear,
        },
        DisplayCommand::FillGradient {
            rect: rect(0.0, 70.0, 200.0, 60.0),
            radius: Corners::uniform(0.0),
            angle_degrees: 45.0,
            stops: stops.clone(),
            kind: GradientKind::Linear,
        },
        DisplayCommand::FillGradient {
            rect: rect(0.0, 140.0, 200.0, 60.0),
            radius: Corners::uniform(10.0),
            angle_degrees: 0.0,
            stops: stops.clone(),
            kind: GradientKind::Radial,
        },
        DisplayCommand::FillGradient {
            rect: rect(210.0, 0.0, 90.0, 200.0),
            radius: Corners::uniform(0.0),
            angle_degrees: 0.0,
            stops,
            kind: GradientKind::Conic,
        },
    ];
    assert_parity(&commands, 320, 220, 0.0, 0.0, None, 1.5, 0.02);
}

#[test]
fn text_parity() {
    let Some(font) = fixture_font() else {
        return;
    };
    let commands = vec![
        DisplayCommand::DrawText {
            x: 12.0,
            y: 40.0,
            text: "Merhaba, GPU! 123".to_string(),
            color: rgb(0x222222),
            font_size: 24.0,
            font_weight: 400,
            underline: true,
            italic: false,
            monospace: false,
            line_through: false,
            letter_spacing: 0.0,
            decoration_color: rgb(0xcc3333),
            decoration_style: BorderStyle::Solid,
        },
        DisplayCommand::DrawText {
            x: 12.0,
            y: 80.0,
            text: "bold weight".to_string(),
            color: lumen_css::Color::rgba(0x33, 0x33, 0x99, 220),
            font_size: 18.0,
            font_weight: 700,
            underline: false,
            italic: false,
            monospace: false,
            line_through: true,
            letter_spacing: 1.5,
            decoration_color: rgb(0x333399),
            decoration_style: BorderStyle::Solid,
        },
    ];
    assert_parity(&commands, 320, 110, 0.0, 0.0, Some(&font), 1.5, 0.03);
}

#[test]
fn image_parity() {
    // A 4×4 checkerboard scaled up, exercising nearest-neighbor sampling.
    let mut rgba = Vec::new();
    for y in 0..4 {
        for x in 0..4 {
            if (x + y) % 2 == 0 {
                rgba.extend_from_slice(&[0xe0, 0x30, 0x30, 0xff]);
            } else {
                rgba.extend_from_slice(&[0x30, 0x30, 0xe0, 0x80]);
            }
        }
    }
    let image = std::sync::Arc::new(RasterImage {
        width: 4,
        height: 4,
        rgba,
        encoded: Vec::new(),
        mime: "image/png",
    });
    let commands = vec![DisplayCommand::DrawImage {
        rect: rect(7.0, 5.0, 120.0, 90.0),
        image,
        alpha: 230,
    }];
    assert_parity(&commands, 140, 110, 0.0, 0.0, None, 1.0, 0.02);
}

#[test]
fn clip_parity() {
    let commands = vec![
        DisplayCommand::FillRect {
            rect: rect(0.0, 0.0, 200.0, 200.0),
            color: rgb(0xeeeeee),
            radius: Corners::uniform(0.0),
        },
        DisplayCommand::PushClip {
            rect: rect(20.0, 20.0, 100.0, 80.0),
        },
        DisplayCommand::FillRect {
            rect: rect(0.0, 0.0, 200.0, 200.0),
            color: rgb(0x3377cc),
            radius: Corners::uniform(0.0),
        },
        DisplayCommand::PushClip {
            rect: rect(60.0, 50.0, 120.0, 60.0),
        },
        DisplayCommand::FillRect {
            rect: rect(0.0, 0.0, 300.0, 300.0),
            color: lumen_css::Color::rgba(0xcc, 0x33, 0x33, 160),
            radius: Corners::uniform(30.0),
        },
        DisplayCommand::PopClip,
        DisplayCommand::PopClip,
        DisplayCommand::FillRect {
            rect: rect(150.0, 150.0, 40.0, 40.0),
            color: rgb(0x22aa66),
            radius: Corners::uniform(0.0),
        },
    ];
    assert_parity(&commands, 220, 220, 0.0, 0.0, None, 1.0, 0.02);
}

#[test]
fn axis_aligned_transform_parity() {
    let commands = vec![
        DisplayCommand::PushTransform {
            matrix: Transform2D::translate(30.0, 20.0).multiply(Transform2D {
                a: 1.5,
                d: 2.0,
                ..Transform2D::IDENTITY
            }),
        },
        DisplayCommand::FillRect {
            rect: rect(10.0, 10.0, 80.0, 40.0),
            color: rgb(0xcc5522),
            radius: Corners::uniform(8.0),
        },
        DisplayCommand::PopTransform,
    ];
    assert_parity(&commands, 220, 140, 0.0, 0.0, None, 1.5, 0.03);
}

#[test]
fn rotated_transform_hybrid_parity() {
    let angle = 0.5f32;
    let rotation = Transform2D {
        a: angle.cos(),
        b: angle.sin(),
        c: -angle.sin(),
        d: angle.cos(),
        e: 80.0,
        f: 60.0,
    };
    let commands = vec![
        // Non-trivial backdrop: the rotated scope must composite over it,
        // not paint a white-backed texture.
        DisplayCommand::FillRect {
            rect: rect(0.0, 0.0, 200.0, 140.0),
            color: rgb(0x2a4d3a),
            radius: Corners::uniform(0.0),
        },
        DisplayCommand::PushTransform { matrix: rotation },
        DisplayCommand::FillRect {
            rect: rect(-40.0, -20.0, 80.0, 40.0),
            color: lumen_css::Color::rgba(0x33, 0x66, 0xcc, 200),
            radius: Corners::uniform(6.0),
        },
        DisplayCommand::DrawText {
            x: -30.0,
            y: 5.0,
            text: "tilt".to_string(),
            color: rgb(0xffffff),
            font_size: 16.0,
            font_weight: 400,
            underline: false,
            italic: false,
            monospace: false,
            line_through: false,
            letter_spacing: 0.0,
            decoration_color: rgb(0xffffff),
            decoration_style: BorderStyle::Solid,
        },
        DisplayCommand::PopTransform,
    ];
    let font = fixture_font();
    // Hybrid (CPU subtree + alpha composite): near-exact, small unblend
    // rounding at soft edges.
    assert_parity(&commands, 200, 140, 0.0, 0.0, font.as_ref(), 2.0, 0.04);
}

#[test]
fn shadow_parity() {
    let commands = vec![
        DisplayCommand::DrawShadow {
            rect: rect(40.0, 40.0, 120.0, 80.0),
            radius: Corners::uniform(12.0),
            blur: 16.0,
            color: lumen_css::Color::rgba(0x20, 0x20, 0x60, 140),
            inset: false,
        },
        DisplayCommand::FillRect {
            rect: rect(40.0, 40.0, 120.0, 80.0),
            color: rgb(0xf0f0f0),
            radius: Corners::uniform(12.0),
        },
        DisplayCommand::DrawShadow {
            rect: rect(200.0, 40.0, 80.0, 80.0),
            radius: Corners::uniform(0.0),
            blur: 10.0,
            color: lumen_css::Color::rgba(0x60, 0x20, 0x20, 120),
            inset: true,
        },
    ];
    assert_parity(&commands, 320, 180, 0.0, 0.0, None, 2.0, 0.03);
}

#[test]
fn fixed_origin_parity() {
    let commands = vec![
        DisplayCommand::FillRect {
            rect: rect(0.0, 0.0, 100.0, 400.0),
            color: rgb(0xdddddd),
            radius: Corners::uniform(0.0),
        },
        DisplayCommand::PushFixed,
        DisplayCommand::FillRect {
            rect: rect(10.0, 10.0, 60.0, 30.0),
            color: rgb(0xcc3333),
            radius: Corners::uniform(0.0),
        },
        DisplayCommand::PopFixed,
        DisplayCommand::FillRect {
            rect: rect(10.0, 200.0, 60.0, 30.0),
            color: rgb(0x3333cc),
            radius: Corners::uniform(0.0),
        },
    ];
    // Scrolled 100px; fixed content pins to the fixed origin instead.
    assert_parity(&commands, 120, 150, 100.0, -20.0, None, 1.0, 0.02);
}

#[test]
fn mark_parity() {
    let commands = vec![
        DisplayCommand::DrawMark {
            rect: rect(10.0, 10.0, 20.0, 20.0),
            color: rgb(0x2266aa),
            mark: Mark::Check,
        },
        DisplayCommand::DrawMark {
            rect: rect(40.0, 10.0, 20.0, 20.0),
            color: rgb(0x2266aa),
            mark: Mark::Dot,
        },
    ];
    assert_parity(&commands, 80, 50, 0.0, 0.0, None, 1.0, 0.03);
}

#[test]
fn composite_parity() {
    let Some(font) = fixture_font() else {
        return;
    };
    // A kitchen-sink-style mix of everything in one frame.
    let commands = vec![
        DisplayCommand::FillRect {
            rect: rect(0.0, 0.0, 400.0, 300.0),
            color: rgb(0xf6f3ee),
            radius: Corners::uniform(0.0),
        },
        DisplayCommand::DrawShadow {
            rect: rect(20.0, 20.0, 160.0, 100.0),
            radius: Corners::uniform(10.0),
            blur: 12.0,
            color: lumen_css::Color::rgba(0x20, 0x1c, 0x2a, 70),
            inset: false,
        },
        DisplayCommand::FillGradient {
            rect: rect(20.0, 20.0, 160.0, 100.0),
            radius: Corners::uniform(10.0),
            angle_degrees: 135.0,
            stops: vec![(rgb(0x3388cc), 0.0), (rgb(0x88ddff), 1.0)],
            kind: GradientKind::Linear,
        },
        DisplayCommand::StrokeRect {
            rect: rect(20.0, 20.0, 160.0, 100.0),
            widths: EdgeSizes::uniform(1.0),
            colors: EdgeSizes::uniform(rgb(0xd6d1c6)),
            styles: EdgeSizes::uniform(BorderStyle::Solid),
            radius: Corners::uniform(10.0),
        },
        DisplayCommand::DrawText {
            x: 36.0,
            y: 70.0,
            text: "Lumen GPU".to_string(),
            color: rgb(0xffffff),
            font_size: 22.0,
            font_weight: 600,
            underline: false,
            italic: false,
            monospace: false,
            line_through: false,
            letter_spacing: 0.0,
            decoration_color: rgb(0xffffff),
            decoration_style: BorderStyle::Solid,
        },
        DisplayCommand::PushClip {
            rect: rect(200.0, 20.0, 180.0, 120.0),
        },
        DisplayCommand::FillRect {
            rect: rect(180.0, 0.0, 220.0, 160.0),
            color: lumen_css::Color::rgba(0xcc, 0x88, 0x33, 200),
            radius: Corners::uniform(24.0),
        },
        DisplayCommand::PopClip,
        DisplayCommand::FillRect {
            rect: rect(40.0, 160.0, 320.0, 100.0),
            color: rgb(0xeeeeee),
            radius: Corners::uniform(0.0),
        },
        DisplayCommand::DrawText {
            x: 52.0,
            y: 210.0,
            text: "The quick brown fox jumps over the lazy dog.".to_string(),
            color: rgb(0x232019),
            font_size: 15.0,
            font_weight: 400,
            underline: false,
            italic: false,
            monospace: false,
            line_through: false,
            letter_spacing: 0.0,
            decoration_color: rgb(0x232019),
            decoration_style: BorderStyle::Solid,
        },
    ];
    assert_parity(&commands, 400, 300, 0.0, 0.0, Some(&font), 2.0, 0.03);
}

#[test]
fn filter_hybrid_parity() {
    use lumen_engine::style::FilterFunction;
    let commands = vec![
        DisplayCommand::FillRect {
            rect: rect(0.0, 0.0, 240.0, 160.0),
            color: rgb(0x3a3a44),
            radius: Corners::uniform(0.0),
        },
        DisplayCommand::PushFilter {
            rect: rect(20.0, 20.0, 100.0, 60.0),
            filters: vec![FilterFunction::Grayscale(1.0)],
        },
        DisplayCommand::FillRect {
            rect: rect(20.0, 20.0, 100.0, 60.0),
            color: rgb(0xe52e71),
            radius: Corners::uniform(0.0),
        },
        DisplayCommand::PopFilter,
        DisplayCommand::PushFilter {
            rect: rect(130.0, 20.0, 90.0, 60.0),
            filters: vec![FilterFunction::Brightness(1.6)],
        },
        DisplayCommand::FillRect {
            rect: rect(130.0, 20.0, 90.0, 60.0),
            color: rgb(0x2ea043),
            radius: Corners::uniform(0.0),
        },
        DisplayCommand::PopFilter,
        DisplayCommand::PushFilter {
            rect: rect(20.0, 90.0, 100.0, 60.0),
            filters: vec![FilterFunction::Blur(4.0)],
        },
        DisplayCommand::FillRect {
            rect: rect(24.0, 94.0, 92.0, 52.0),
            color: rgb(0x2f6fdd),
            radius: Corners::uniform(0.0),
        },
        DisplayCommand::PopFilter,
    ];
    // Opaque filtered content is exact; blur edges and uncovered margin
    // pixels inside filter rects are the documented approximation.
    assert_parity(&commands, 240, 160, 0.0, 0.0, None, 3.0, 0.05);
}

#[test]
fn scroll_offsets_content() {
    let commands = vec![DisplayCommand::FillRect {
        rect: rect(0.0, 50.0, 100.0, 100.0),
        color: rgb(0x336633),
        radius: Corners::uniform(0.0),
    }];
    assert_parity(&commands, 120, 120, 40.0, 0.0, None, 0.5, 0.01);
}
