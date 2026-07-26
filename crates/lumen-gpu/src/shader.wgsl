// Display-list rasterization on the GPU: one instanced-quad pipeline whose
// fragment shader reproduces the CPU rasterizer's coverage math (rounded
// rects, Gaussian shadows, segments, glyphs, images, gradients).

struct Uniforms {
    viewport: vec4<f32>, // target size in device pixels (xy)
};

@group(0) @binding(0) var<uniform> uni: Uniforms;

@group(1) @binding(0) var atlas_tex: texture_2d<f32>; // R8 glyph coverage
@group(1) @binding(1) var aux_tex: texture_2d<f32>;   // image RGBA / gradient stops
@group(1) @binding(2) var samp_nearest: sampler;
@group(1) @binding(3) var samp_linear: sampler;

const KIND_SOLID: f32 = 0.0;
const KIND_RING: f32 = 1.0;
const KIND_SHADOW: f32 = 2.0;
const KIND_SEGMENT: f32 = 3.0;
const KIND_GLYPH: f32 = 4.0;
const KIND_IMAGE: f32 = 5.0;
const KIND_GRADIENT: f32 = 6.0;

struct Inst {
    @location(0) rect: vec4<f32>,   // draw quad: device x, y, w, h
    @location(1) color: vec4<f32>,  // straight-alpha rgba, 0..1
    @location(2) radii: vec4<f32>,  // corner radii: tl, tr, br, bl
    @location(3) extra0: vec4<f32>, // ring: width | shadow: blur, inset | segment: x0,y0,x1,y1 | glyph: baseline, shear | gradient: dx, dy, line_length, kind
    @location(4) extra1: vec4<f32>, // shadow: box rect | segment: thickness
    @location(5) extra2: vec4<f32>, // unused
    @location(6) uv: vec4<f32>,     // u0, v0, u1, v1
    @location(7) tag: vec4<f32>,   // kind
};

struct Vary {
    @builtin(position) pos: vec4<f32>,
    @location(0) dev: vec2<f32>,
    @location(1) tuv: vec2<f32>,
    @location(2) @interpolate(flat) rect: vec4<f32>,
    @location(3) @interpolate(flat) color: vec4<f32>,
    @location(4) @interpolate(flat) radii: vec4<f32>,
    @location(5) @interpolate(flat) extra0: vec4<f32>,
    @location(6) @interpolate(flat) extra1: vec4<f32>,
    @location(7) @interpolate(flat) kind: f32,
    @location(8) @interpolate(flat) uv_bounds: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32, inst: Inst) -> Vary {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    let corner = corners[vertex_index];
    var dev = inst.rect.xy + corner * inst.rect.zw;
    // Synthetic italic: rows above the baseline shear right (linear in y,
    // so a vertex shear matches the CPU's per-row shift).
    if inst.tag.x == KIND_GLYPH {
        dev.x += inst.extra0.y * max(inst.extra0.x - dev.y, 0.0);
    }
    var out: Vary;
    out.dev = dev;
    out.tuv = inst.uv.xy + corner * (inst.uv.zw - inst.uv.xy);
    let ndc = vec2<f32>(
        dev.x / uni.viewport.x * 2.0 - 1.0,
        1.0 - dev.y / uni.viewport.y * 2.0,
    );
    out.pos = vec4<f32>(ndc, 0.0, 1.0);
    out.rect = inst.rect;
    out.color = inst.color;
    out.radii = inst.radii;
    out.extra0 = inst.extra0;
    out.extra1 = inst.extra1;
    out.kind = inst.tag.x;
    out.uv_bounds = inst.uv;
    return out;
}

// Abramowitz–Stegun erf approximation — the same formula the CPU shadow
// uses, so Gaussian falloff matches to float precision.
fn erf_approx(x: f32) -> f32 {
    let s = sign(x);
    let ax = abs(x);
    let t = 1.0 / (1.0 + 0.3275911 * ax);
    let y = 1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * exp(-ax * ax);
    return s * y;
}

fn corner_radius(p: vec2<f32>, half: vec2<f32>, radii: vec4<f32>) -> f32 {
    let r_top = select(radii.x, radii.y, p.x > 0.0); // tl : tr
    let r_bottom = select(radii.w, radii.z, p.x > 0.0); // bl : br
    return select(r_top, r_bottom, p.y > 0.0);
}

fn rounded_sdf(p: vec2<f32>, half: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half + vec2<f32>(r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - r;
}

// CPU parity: hard edges on the straight sides, an antialiased ramp only
// inside corner cells.
fn rounded_coverage(rect: vec4<f32>, radii: vec4<f32>, dev: vec2<f32>) -> f32 {
    let center = rect.xy + rect.zw * 0.5;
    let half = rect.zw * 0.5;
    let p = dev - center;
    let r = min(corner_radius(p, half, radii), min(half.x, half.y));
    let d = rounded_sdf(p, half, r);
    let in_corner = r > 0.0 && abs(p.x) > half.x - r && abs(p.y) > half.y - r;
    if in_corner {
        return clamp(0.5 - d, 0.0, 1.0);
    }
    return select(0.0, 1.0, d < 0.0);
}

fn ring_coverage(rect: vec4<f32>, radii: vec4<f32>, width: f32, dev: vec2<f32>) -> f32 {
    let outer = rounded_coverage(rect, radii, dev);
    let inner_size = rect.zw - vec2<f32>(2.0 * width);
    if inner_size.x <= 0.0 || inner_size.y <= 0.0 {
        return outer;
    }
    let inner_rect = vec4<f32>(rect.xy + vec2<f32>(width), inner_size);
    let inner_radii = max(radii - vec4<f32>(width), vec4<f32>(0.0));
    return clamp(outer - rounded_coverage(inner_rect, inner_radii, dev), 0.0, 1.0);
}

fn shadow_coverage(box: vec4<f32>, radii: vec4<f32>, blur: f32, inset: f32, dev: vec2<f32>) -> f32 {
    var cov: f32;
    if blur <= 0.0 {
        cov = rounded_coverage(box, radii, dev);
    } else {
        let sigma = max(blur * 0.5, 0.01);
        let denom = sigma * 1.4142135623730951;
        let cx = 0.5 * (erf_approx((dev.x - box.x) / denom) - erf_approx((dev.x - (box.x + box.z)) / denom));
        let cy = 0.5 * (erf_approx((dev.y - box.y) / denom) - erf_approx((dev.y - (box.y + box.w)) / denom));
        cov = clamp(cx * cy, 0.0, 1.0);
    }
    if inset > 0.5 {
        let inside = rounded_coverage(box, radii, dev);
        return (1.0 - cov) * inside;
    }
    return cov;
}

fn segment_coverage(a: vec2<f32>, b: vec2<f32>, thickness: f32, dev: vec2<f32>) -> f32 {
    let d = b - a;
    let l2 = max(dot(d, d), 1e-7);
    let t = clamp(dot(dev - a, d) / l2, 0.0, 1.0);
    let dist = distance(dev, a + t * d);
    return clamp(thickness * 0.5 + 0.5 - dist, 0.0, 1.0);
}

@fragment
fn fs_main(v: Vary) -> @location(0) vec4<f32> {
    if v.kind == KIND_GLYPH {
        let cov = textureSample(atlas_tex, samp_nearest, v.tuv).r;
        return vec4<f32>(v.color.rgb, v.color.a * cov);
    }
    if v.kind == KIND_IMAGE {
        // CPU samples by the pixel's top-left corner, not its center.
        let fu = (v.dev.x - 0.5 - v.rect.x) / v.rect.z;
        let fv = (v.dev.y - 0.5 - v.rect.y) / v.rect.w;
        let su = clamp(fu, 0.0, 1.0) * (v.uv_bounds.z - v.uv_bounds.x) + v.uv_bounds.x;
        let sv = clamp(fv, 0.0, 1.0) * (v.uv_bounds.w - v.uv_bounds.y) + v.uv_bounds.y;
        let tex = textureSample(aux_tex, samp_nearest, vec2<f32>(su, sv));
        return vec4<f32>(tex.rgb, tex.a * v.color.a);
    }
    var cov: f32;
    var color = v.color;
    if v.kind == KIND_RING {
        cov = ring_coverage(v.rect, v.radii, v.extra0.x, v.dev);
    } else if v.kind == KIND_SHADOW {
        cov = shadow_coverage(v.extra1, v.radii, v.extra0.x, v.extra0.y, v.dev);
    } else if v.kind == KIND_SEGMENT {
        cov = segment_coverage(v.extra0.xy, v.extra0.zw, v.extra1.x, v.dev);
    } else if v.kind == KIND_GRADIENT {
        cov = rounded_coverage(v.rect, v.radii, v.dev);
        let center = v.rect.xy + v.rect.zw * 0.5;
        let gkind = v.extra0.w;
        var progress: f32;
        if gkind < 0.5 {
            let line = v.extra0.z;
            if line <= 0.0 {
                progress = 0.0;
            } else {
                progress = clamp(((v.dev.x - center.x) * v.extra0.x + (v.dev.y - center.y) * v.extra0.y) / line + 0.5, 0.0, 1.0);
            }
        } else if gkind < 1.5 {
            let nx = (v.dev.x - center.x) / max(v.rect.z * 0.5, 1e-4);
            let ny = (v.dev.y - center.y) / max(v.rect.w * 0.5, 1e-4);
            progress = clamp(sqrt(nx * nx + ny * ny), 0.0, 1.0);
        } else {
            let angle = atan2(v.dev.x - center.x, center.y - v.dev.y);
            progress = fract(angle / 6.28318530718);
        }
        color = textureSample(aux_tex, samp_linear, vec2<f32>(progress, 0.5));
    } else {
        cov = rounded_coverage(v.rect, v.radii, v.dev);
    }
    if cov <= 0.0 {
        discard;
    }
    return vec4<f32>(color.rgb, color.a * cov);
}
