//! Display-command walking: turns each pass into instanced draws,
//! mirroring the CPU rasterizer's command walk (`rasterize_clipped`).

use lumen_engine::font::SystemFont;
use lumen_engine::geometry::Rect;
use lumen_engine::paint::{DisplayCommand, GradientKind};
use lumen_engine::style::{BorderStyle, Mark, Transform2D};
use std::sync::Arc;

use crate::{
    Aux, Draw, GpuRenderer, Instance, KIND_GLYPH, KIND_GRADIENT, KIND_IMAGE, KIND_RING,
    KIND_SEGMENT, KIND_SHADOW, KIND_SOLID, Pass, color_floats, radii_floats, rect_floats,
};

impl GpuRenderer {
    /// Walks one pass, appending draws. Mirrors `rasterize_clipped`:
    /// transform stack, clip stack (scissor), fixed scopes, scroll.
    /// Returns false when a scope cannot be handled (the caller then
    /// re-renders the whole frame on the CPU).
    pub(crate) fn walk(
        &mut self,
        pass: &Pass<'_>,
        size: (u32, u32),
        scale: f32,
        font: Option<&SystemFont>,
        draws: &mut Vec<Draw>,
    ) -> bool {
        let mut walker = Walk {
            renderer: self,
            size,
            scale,
            font,
            scroll_y: pass.scroll_y,
            fixed_origin: pass.fixed_origin,
            transforms: Vec::new(),
            clips: Vec::new(),
            fixed_depth: 0,
            last_text_x: 0.0,
            last_text_y: 0.0,
            last_text_width: 0.0,
            last_font_size: 0.0,
        };
        walker.run(pass.commands, draws)
    }

    /// Whole-frame CPU fallback: rasterize every pass with the software
    /// backend into one framebuffer, upload it, draw it as a single quad.
    pub(crate) fn cpu_frame_draw(
        &mut self,
        passes: &[Pass<'_>],
        size: (u32, u32),
        scale: f32,
        font: Option<&SystemFont>,
        draws: &mut Vec<Draw>,
    ) {
        let (width, height) = size;
        let mut framebuffer = match passes.first() {
            Some(pass) => lumen_engine::rasterize_with_fixed_origin(
                pass.commands,
                width,
                height,
                pass.scroll_y,
                pass.fixed_origin,
                scale,
                font,
            ),
            None => lumen_engine::Framebuffer::new(width, height),
        };
        for pass in &passes[1..] {
            lumen_engine::rasterize_over(
                &mut framebuffer,
                pass.commands,
                pass.scroll_y,
                scale,
                font,
            );
        }
        let mut rgba = vec![0u8; (width * height * 4) as usize];
        for (chunk, pixel) in rgba.chunks_exact_mut(4).zip(&framebuffer.pixels) {
            chunk[0] = ((pixel >> 16) & 0xff) as u8;
            chunk[1] = ((pixel >> 8) & 0xff) as u8;
            chunk[2] = (pixel & 0xff) as u8;
            chunk[3] = 255;
        }
        let recreate =
            !matches!(&self.cpu_frame, Some((w, h, _, _)) if *w == width && *h == height);
        if recreate {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("lumen-cpu-frame"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let slot = self.make_slot(&texture, None);
            self.cpu_frame = Some((width, height, texture, slot));
        }
        let Some((_, _, texture, _)) = &self.cpu_frame else {
            return;
        };
        self.queue.write_texture(
            texture.as_image_copy(),
            &rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        let mut quad = Instance::new(
            KIND_IMAGE,
            [0.0, 0.0, width as f32, height as f32],
            [1.0, 1.0, 1.0, 1.0],
        );
        // Sample at texel centers: the shader's image path reconstructs
        // UVs from the pixel's top-left corner, so shift the UV window by
        // half a texel for a stable 1:1 mapping.
        quad.uv = [
            0.5 / width as f32,
            0.5 / height as f32,
            1.0 + 0.5 / width as f32,
            1.0 + 0.5 / height as f32,
        ];
        draws.push(Draw {
            scissor: (0, 0, width, height),
            aux: Aux::CpuFrame,
            instances: vec![quad],
        });
    }

    /// The texture for an image command, uploaded and cached by pointer
    /// identity (the [`Arc`] is kept alive inside the cache entry).
    fn image_aux(&mut self, image: &Arc<lumen_engine::RasterImage>) -> Aux {
        let key = Arc::as_ptr(image) as usize;
        if !self.images.contains_key(&key) {
            if self.images.len() >= crate::MAX_CACHED_IMAGES {
                self.images.clear();
            }
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("lumen-image"),
                size: wgpu::Extent3d {
                    width: image.width.max(1),
                    height: image.height.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            self.queue.write_texture(
                texture.as_image_copy(),
                &image.rgba,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(image.width * 4),
                    rows_per_image: Some(image.height),
                },
                wgpu::Extent3d {
                    width: image.width,
                    height: image.height,
                    depth_or_array_layers: 1,
                },
            );
            let slot = self.make_slot(&texture, Some(image.clone()));
            self.images.insert(key, slot);
        }
        Aux::Image(key)
    }

    /// The 1D stops texture for a gradient, cached by stop content.
    fn gradient_aux(&mut self, stops: &[(lumen_css::Color, f32)]) -> Aux {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for (color, position) in stops {
            (color.r, color.g, color.b, color.a).hash(&mut hasher);
            position.to_bits().hash(&mut hasher);
        }
        let key = hasher.finish();
        if !self.gradients.contains_key(&key) {
            let mut data = Vec::with_capacity((crate::GRADIENT_STOPS * 4) as usize);
            for index in 0..crate::GRADIENT_STOPS {
                let progress = index as f32 / (crate::GRADIENT_STOPS - 1) as f32;
                let color = gradient_color_at(stops, progress);
                data.extend_from_slice(&[color.r, color.g, color.b, color.a]);
            }
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("lumen-gradient"),
                size: wgpu::Extent3d {
                    width: crate::GRADIENT_STOPS,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            self.queue.write_texture(
                texture.as_image_copy(),
                &data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(crate::GRADIENT_STOPS * 4),
                    rows_per_image: Some(1),
                },
                wgpu::Extent3d {
                    width: crate::GRADIENT_STOPS,
                    height: 1,
                    depth_or_array_layers: 1,
                },
            );
            let slot = self.make_slot(&texture, None);
            self.gradients.insert(key, slot);
        }
        Aux::Gradient(key)
    }
}

/// Per-pass walk state (transform/clip/fixed stacks).
struct Walk<'a> {
    renderer: &'a mut GpuRenderer,
    size: (u32, u32),
    scale: f32,
    font: Option<&'a SystemFont>,
    scroll_y: f32,
    fixed_origin: f32,
    transforms: Vec<Transform2D>,
    clips: Vec<(u32, u32, u32, u32)>,
    fixed_depth: usize,
    /// Geometry of the last text run, for underline/strike-through.
    last_text_x: f32,
    last_text_y: f32,
    last_text_width: f32,
    last_font_size: f32,
}

impl Walk<'_> {
    /// Command loop with hybrid dispatch: rotated/skewed transform and
    /// `filter` scopes render on the CPU into an alpha-recovered texture
    /// (see [`Walk::hybrid`]); everything else walks natively.
    fn run(&mut self, commands: &[DisplayCommand], draws: &mut Vec<Draw>) -> bool {
        let (transform_pops, filter_pops) = match_scope_pops(commands);
        let mut index = 0;
        while index < commands.len() {
            match &commands[index] {
                DisplayCommand::PushTransform { matrix }
                    if !(matrix.b.abs() < 1e-6 && matrix.c.abs() < 1e-6) =>
                {
                    let matrix = *matrix;
                    let Some(&pop) = transform_pops.get(&index) else {
                        return false;
                    };
                    if !self.hybrid(&commands[index..=pop], Some(&matrix), draws) {
                        return false;
                    }
                    index = pop + 1;
                }
                DisplayCommand::PushFilter { .. } => {
                    let Some(&pop) = filter_pops.get(&index) else {
                        return false;
                    };
                    if !self.hybrid(&commands[index..=pop], None, draws) {
                        return false;
                    }
                    index = pop + 1;
                }
                command => {
                    self.command(command, draws);
                    index += 1;
                }
            }
        }
        true
    }

    /// Hybrid path for one rotated/skewed transform or `filter` scope:
    /// the CPU rasterizer paints the subtree twice — over a white and
    /// over a black backdrop — which recovers per-pixel straight alpha
    /// (`a = 1 - (white - black) / 255`, `fg = black / a`), and the
    /// region composites on the GPU as a textured quad. Paint order and
    /// coverage stay exact where the scope paints opaquely; the cost is
    /// two CPU renders of a small region instead of the whole frame.
    ///
    /// Known approximation for filters: pixels inside the filter rect
    /// that the subtree leaves uncovered show the *unfiltered* backdrop
    /// (the CPU filters the real backdrop there). Opaque filtered
    /// content — the common case — is exact; AA edges can differ by a
    /// rounding step because the CPU blends-then-filters.
    fn hybrid(
        &mut self,
        subtree: &[DisplayCommand],
        rotated: Option<&Transform2D>,
        draws: &mut Vec<Draw>,
    ) -> bool {
        // Fixed scopes change the scroll the CPU would apply mid-subtree;
        // punt those (rare) to the whole-frame fallback.
        if self.fixed_depth > 0 {
            return false;
        }
        let outer = self.transforms.last().copied();
        let matrix0 = match (outer, rotated) {
            (Some(outer), Some(rotated)) => outer.multiply(*rotated),
            (Some(outer), None) => outer,
            (None, Some(rotated)) => *rotated,
            (None, None) => Transform2D::IDENTITY,
        };
        // Transform scopes are pre-composed into `matrix0`, so their own
        // push/pop is excluded from the bounds scan; filter scopes keep
        // it (the filter rect feeds the bounds).
        let scanned = match rotated {
            Some(_) => &subtree[1..subtree.len() - 1],
            None => subtree,
        };
        let Some(bounds) = page_bounds(scanned, matrix0, self.font) else {
            return false;
        };
        let scale = self.scale;
        let scroll = self.scroll_y;
        let (width, height) = self.size;
        let x0 = ((bounds.x * scale).floor() as i32 - 2).clamp(0, width as i32) as u32;
        let y0 = (((bounds.y - scroll) * scale).floor() as i32 - 2).clamp(0, height as i32) as u32;
        let x1 = (((bounds.x + bounds.width) * scale).ceil() as i32 + 2).clamp(0, width as i32)
            as u32;
        let y1 = (((bounds.y + bounds.height - scroll) * scale).ceil() as i32 + 2)
            .clamp(0, height as i32) as u32;
        if x1 <= x0 || y1 <= y0 {
            return true; // Nothing visible: the whole scope is off-screen.
        }
        let (region_width, region_height) = (x1 - x0, y1 - y0);

        // The CPU walk needs the enclosing (axis-aligned) transform stack
        // replayed around the subtree.
        let mut wrapped = Vec::with_capacity(subtree.len() + 2);
        if let Some(outer) = outer {
            wrapped.push(DisplayCommand::PushTransform { matrix: outer });
        }
        wrapped.extend_from_slice(subtree);
        if outer.is_some() {
            wrapped.push(DisplayCommand::PopTransform);
        }
        let white = lumen_engine::rasterize_with_fixed_origin(
            &wrapped,
            width,
            height,
            scroll,
            self.fixed_origin,
            scale,
            self.font,
        );
        let mut black_commands = Vec::with_capacity(wrapped.len() + 1);
        black_commands.push(DisplayCommand::FillRect {
            rect: Rect {
                x: 0.0,
                y: scroll,
                width: width as f32 / scale,
                height: height as f32 / scale,
            },
            color: lumen_css::Color::rgb(0, 0, 0),
            radius: lumen_engine::Corners::uniform(0.0),
        });
        black_commands.extend_from_slice(&wrapped);
        let black = lumen_engine::rasterize_with_fixed_origin(
            &black_commands,
            width,
            height,
            scroll,
            self.fixed_origin,
            scale,
            self.font,
        );

        let mut rgba = vec![0u8; (region_width * region_height * 4) as usize];
        for row in 0..region_height {
            for column in 0..region_width {
                let over_white = white.pixel(x0 + column, y0 + row);
                let over_black = black.pixel(x0 + column, y0 + row);
                let split = |pixel: u32| {
                    (
                        (pixel >> 16) & 0xff,
                        (pixel >> 8) & 0xff,
                        pixel & 0xff,
                    )
                };
                let (wr, wg, wb) = split(over_white);
                let (br, bg, bb) = split(over_black);
                let diff = (wr as i32 - br as i32)
                    .max(wg as i32 - bg as i32)
                    .max(wb as i32 - bb as i32)
                    .clamp(0, 255) as u32;
                let alpha = 255 - diff;
                let at = ((row * region_width + column) * 4) as usize;
                if alpha == 0 {
                    continue; // rgba stays transparent black.
                }
                let unblend = |channel: u32| ((channel * 255 + alpha / 2) / alpha) as u8;
                rgba[at] = unblend(br);
                rgba[at + 1] = unblend(bg);
                rgba[at + 2] = unblend(bb);
                rgba[at + 3] = alpha as u8;
            }
        }
        let Some((slot, texture)) = self.renderer.hybrid_slot(region_width, region_height)
        else {
            return false;
        };
        self.renderer.queue.write_texture(
            texture.as_image_copy(),
            &rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(region_width * 4),
                rows_per_image: Some(region_height),
            },
            wgpu::Extent3d {
                width: region_width,
                height: region_height,
                depth_or_array_layers: 1,
            },
        );
        let mut instance = Instance::new(
            KIND_IMAGE,
            [
                x0 as f32,
                y0 as f32,
                region_width as f32,
                region_height as f32,
            ],
            [1.0, 1.0, 1.0, 1.0],
        );
        // Half-texel shift for a stable 1:1 texel mapping (see
        // `cpu_frame_draw`).
        instance.uv = [
            0.5 / region_width as f32,
            0.5 / region_height as f32,
            1.0 + 0.5 / region_width as f32,
            1.0 + 0.5 / region_height as f32,
        ];
        self.push(draws, Aux::Hybrid(slot), instance);
        true
    }

    fn effective_scroll(&self) -> f32 {
        if self.fixed_depth > 0 {
            self.fixed_origin
        } else {
            self.scroll_y
        }
    }

    fn current_scissor(&self) -> (u32, u32, u32, u32) {
        let (x0, y0, x1, y1) = self.clips.last().copied().unwrap_or((0, 0, self.size.0, self.size.1));
        (
            x0.min(self.size.0),
            y0.min(self.size.1),
            x1.saturating_sub(x0).min(self.size.0 - x0.min(self.size.0)),
            y1.saturating_sub(y0).min(self.size.1 - y0.min(self.size.1)),
        )
    }

    /// Appends an instance to the current draw, starting a new one when
    /// the scissor or texture binding changes.
    fn push(&mut self, draws: &mut Vec<Draw>, aux: Aux, instance: Instance) {
        let scissor = self.current_scissor();
        let fresh = match draws.last() {
            Some(draw) => draw.scissor != scissor || draw.aux != aux,
            None => true,
        };
        if fresh {
            draws.push(Draw {
                scissor,
                aux,
                instances: Vec::new(),
            });
        }
        if let Some(draw) = draws.last_mut() {
            draw.instances.push(instance);
        }
    }

    /// Page rect → device rect through the transform stack and scroll,
    /// exactly like the CPU walk's `shift`.
    fn shift(&self, rect: &Rect) -> Rect {
        let mapped = match self.transforms.last() {
            Some(matrix) => map_rect(rect, matrix),
            None => *rect,
        };
        let scroll = self.effective_scroll();
        Rect {
            x: mapped.x * self.scale,
            y: (mapped.y - scroll) * self.scale,
            width: mapped.width * self.scale,
            height: mapped.height * self.scale,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn command(&mut self, command: &DisplayCommand, draws: &mut Vec<Draw>) {
        match command {
            DisplayCommand::PushFixed => self.fixed_depth += 1,
            DisplayCommand::PopFixed => self.fixed_depth = self.fixed_depth.saturating_sub(1),
            DisplayCommand::PushTransform { matrix } => {
                let composed = match self.transforms.last() {
                    Some(outer) => outer.multiply(*matrix),
                    None => *matrix,
                };
                self.transforms.push(composed);
            }
            DisplayCommand::PopTransform => {
                self.transforms.pop();
            }
            // Filtered scopes never reach the walker: the pre-scan routes
            // the whole frame to the CPU fallback.
            DisplayCommand::PushFilter { .. } | DisplayCommand::PopFilter => {}
            DisplayCommand::PushClip { rect } => {
                let rect = self.shift(rect);
                let x0 = (rect.x.max(0.0) as u32).min(self.size.0);
                let y0 = (rect.y.max(0.0) as u32).min(self.size.1);
                let x1 = ((rect.x + rect.width).max(0.0).ceil() as u32).min(self.size.0);
                let y1 = ((rect.y + rect.height).max(0.0).ceil() as u32).min(self.size.1);
                let outer = self
                    .clips
                    .last()
                    .copied()
                    .unwrap_or((0, 0, self.size.0, self.size.1));
                self.clips.push((
                    x0.max(outer.0),
                    y0.max(outer.1),
                    x1.min(outer.2).max(x0.max(outer.0)),
                    y1.min(outer.3).max(y0.max(outer.1)),
                ));
            }
            DisplayCommand::PopClip => {
                self.clips.pop();
            }
            DisplayCommand::FillRect {
                rect,
                color,
                radius,
            } => {
                let rect = self.shift(rect);
                if radius.is_zero() {
                    self.solid(draws, rect, *color);
                } else {
                    let radii = radius.clamped_to(rect.width / self.scale, rect.height / self.scale);
                    let mut instance =
                        Instance::new(KIND_SOLID, rect_floats(rect), color_floats(*color));
                    instance.radii = radii_floats(&radii, self.scale);
                    self.push(draws, Aux::None, instance);
                }
            }
            DisplayCommand::StrokeRect {
                rect,
                widths,
                colors,
                styles,
                radius,
            } => {
                let rect = self.shift(rect);
                if !radius.is_zero() {
                    let width = (widths.top.max(widths.left) * self.scale).max(1.0);
                    let radii = radius.clamped_to(rect.width / self.scale, rect.height / self.scale);
                    let mut instance =
                        Instance::new(KIND_RING, rect_floats(rect), color_floats(colors.top));
                    instance.radii = radii_floats(&radii, self.scale);
                    instance.extra0[0] = width;
                    self.push(draws, Aux::None, instance);
                    return;
                }
                let width_of = |value: f32| {
                    if value > 0.0 {
                        (value * self.scale).max(1.0)
                    } else {
                        0.0
                    }
                };
                let strips = [
                    (
                        Rect {
                            height: width_of(widths.top),
                            ..rect
                        },
                        colors.top,
                        styles.top,
                        true,
                    ),
                    (
                        Rect {
                            x: rect.x + rect.width - width_of(widths.right),
                            width: width_of(widths.right),
                            ..rect
                        },
                        colors.right,
                        styles.right,
                        false,
                    ),
                    (
                        Rect {
                            y: rect.y + rect.height - width_of(widths.bottom),
                            height: width_of(widths.bottom),
                            ..rect
                        },
                        colors.bottom,
                        styles.bottom,
                        true,
                    ),
                    (
                        Rect {
                            width: width_of(widths.left),
                            ..rect
                        },
                        colors.left,
                        styles.left,
                        false,
                    ),
                ];
                for (strip, color, style, horizontal) in strips {
                    self.fill_edge(draws, &strip, color, style, horizontal);
                }
            }
            DisplayCommand::DrawShadow {
                rect,
                radius,
                blur,
                color,
                inset,
            } => {
                let rect = self.shift(rect);
                let blur = blur * self.scale;
                let radii = radius.clamped_to(rect.width / self.scale, rect.height / self.scale);
                let quad = if *inset {
                    rect
                } else {
                    let reach = blur.max(1.0) * 1.5;
                    let x0 = (rect.x - reach).floor();
                    let y0 = (rect.y - reach).floor();
                    Rect {
                        x: x0,
                        y: y0,
                        width: (rect.x + rect.width + reach).ceil() - x0,
                        height: (rect.y + rect.height + reach).ceil() - y0,
                    }
                };
                let mut instance =
                    Instance::new(KIND_SHADOW, rect_floats(quad), color_floats(*color));
                instance.radii = radii_floats(&radii, self.scale);
                instance.extra0 = [blur, f32::from(u8::from(*inset)), 0.0, 0.0];
                instance.extra1 = rect_floats(rect);
                self.push(draws, Aux::None, instance);
            }
            DisplayCommand::FillGradient {
                rect,
                radius,
                angle_degrees,
                stops,
                kind,
            } => {
                if stops.is_empty() {
                    return;
                }
                let rect = self.shift(rect);
                if rect.width <= 0.0 || rect.height <= 0.0 {
                    return;
                }
                let radians = angle_degrees.to_radians();
                let (dx, dy) = (radians.sin(), -radians.cos());
                let line_length = (rect.width * dx).abs() + (rect.height * dy).abs();
                let gkind = match kind {
                    GradientKind::Linear => 0.0,
                    GradientKind::Radial => 1.0,
                    GradientKind::Conic => 2.0,
                };
                let radii = radius.clamped_to(rect.width / self.scale, rect.height / self.scale);
                let mut instance =
                    Instance::new(KIND_GRADIENT, rect_floats(rect), [1.0, 1.0, 1.0, 1.0]);
                instance.radii = radii_floats(&radii, self.scale);
                instance.extra0 = [dx, dy, line_length, gkind];
                let aux = self.renderer.gradient_aux(stops);
                self.push(draws, aux, instance);
            }
            DisplayCommand::DrawImage { rect, image, alpha } => {
                let rect = self.shift(rect);
                if rect.width <= 0.0 || rect.height <= 0.0 || image.width == 0 || image.height == 0
                {
                    return;
                }
                let aux = self.renderer.image_aux(image);
                let mut instance = Instance::new(
                    KIND_IMAGE,
                    rect_floats(rect),
                    [1.0, 1.0, 1.0, f32::from(*alpha) / 255.0],
                );
                instance.uv = [0.0, 0.0, 1.0, 1.0];
                self.push(draws, aux, instance);
            }
            DisplayCommand::DrawText {
                x,
                y,
                text,
                color,
                font_size,
                font_weight,
                underline,
                italic,
                monospace,
                line_through,
                letter_spacing,
                decoration_color,
                decoration_style,
            } => {
                self.text(
                    draws,
                    *x,
                    *y,
                    text,
                    *color,
                    *font_size,
                    *font_weight,
                    *italic,
                    *monospace,
                    *letter_spacing,
                );
                let text_width = self.last_text_width;
                let scale = self.scale;
                if *underline {
                    self.fill_edge(
                        draws,
                        &Rect {
                            x: self.last_text_x,
                            y: self.last_text_y + (2.0 * scale).max(1.0),
                            width: text_width,
                            height: scale.max(1.0),
                        },
                        *decoration_color,
                        *decoration_style,
                        true,
                    );
                }
                if *line_through {
                    self.fill_edge(
                        draws,
                        &Rect {
                            x: self.last_text_x,
                            y: self.last_text_y - self.last_font_size * 0.3,
                            width: text_width,
                            height: scale.max(1.0),
                        },
                        *decoration_color,
                        *decoration_style,
                        true,
                    );
                }
            }
            DisplayCommand::DrawMark { rect, color, mark } => {
                let rect = self.shift(rect);
                self.mark(draws, &rect, *color, *mark);
            }
        }
    }

    /// A snapped solid fill, matching the CPU `paint_rect` truncation.
    fn solid(&mut self, draws: &mut Vec<Draw>, rect: Rect, color: lumen_css::Color) {
        if color.a == 0 {
            return;
        }
        let x0 = rect.x.max(0.0) as u32;
        let y0 = rect.y.max(0.0) as u32;
        let x1 = (rect.x + rect.width).max(0.0) as u32;
        let y1 = (rect.y + rect.height).max(0.0) as u32;
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let snapped = [
            x0 as f32,
            y0 as f32,
            (x1 - x0) as f32,
            (y1 - y0) as f32,
        ];
        self.push(
            draws,
            Aux::None,
            Instance::new(KIND_SOLID, snapped, color_floats(color)),
        );
    }

    /// One border/decoration edge strip, segmented for dashed/dotted
    /// styles exactly like the CPU `fill_edge`.
    fn fill_edge(
        &mut self,
        draws: &mut Vec<Draw>,
        strip: &Rect,
        color: lumen_css::Color,
        style: BorderStyle,
        horizontal: bool,
    ) {
        let thickness = if horizontal { strip.height } else { strip.width };
        if thickness <= 0.0 {
            return;
        }
        let (dash, gap) = match style {
            BorderStyle::Dashed => (3.0 * thickness, 2.0 * thickness),
            BorderStyle::Dotted => (thickness, thickness),
            _ => {
                self.solid(draws, *strip, color);
                return;
            }
        };
        let length = if horizontal { strip.width } else { strip.height };
        let mut offset = 0.0;
        while offset < length {
            let segment = dash.min(length - offset);
            let rect = if horizontal {
                Rect {
                    x: strip.x + offset,
                    width: segment,
                    ..*strip
                }
            } else {
                Rect {
                    y: strip.y + offset,
                    height: segment,
                    ..*strip
                }
            };
            self.solid(draws, rect, color);
            offset += dash + gap;
        }
    }

    /// A text run: glyph quads from the atlas, same pen math as the
    /// CPU's `draw_text_scalable`.
    #[allow(clippy::too_many_arguments)]
    fn text(
        &mut self,
        draws: &mut Vec<Draw>,
        x: f32,
        y: f32,
        text: &str,
        color: lumen_css::Color,
        font_size: f32,
        font_weight: u16,
        italic: bool,
        monospace: bool,
        letter_spacing: f32,
    ) {
        let Some(font) = self.font else {
            return;
        };
        let current = self.transforms.last().copied();
        let (page_x, page_y) = match current {
            Some(matrix) => matrix.apply(x, y),
            None => (x, y),
        };
        let text_scale = match current {
            Some(matrix) => {
                ((matrix.a * matrix.a + matrix.b * matrix.b).sqrt()
                    + (matrix.c * matrix.c + matrix.d * matrix.d).sqrt())
                    / 2.0
            }
            None => 1.0,
        };
        let scroll = self.effective_scroll();
        let (x, y, font_size) = (
            page_x * self.scale,
            (page_y - scroll) * self.scale,
            font_size * text_scale * self.scale,
        );
        let letter_spacing = letter_spacing * text_scale * self.scale;
        let shear = if italic { 0.21 } else { 0.0 };
        let bold = font_weight >= 600;
        let mut pen_x = x;
        for character in text.chars() {
            let slot = self.renderer.atlas.entry(
                &self.renderer.queue,
                font,
                character,
                font_size,
                monospace,
            );
            if let Some(slot) = slot {
                if slot.width > 0 && slot.height > 0 {
                    // The CPU truncates the fractional origin, so snap the
                    // quad the same way for a 1:1 texel mapping.
                    let glyph_x = (pen_x + slot.xmin as f32).floor();
                    let glyph_y = (y - slot.ymin as f32 - slot.height as f32).floor();
                    let mut instance = Instance::new(
                        KIND_GLYPH,
                        [glyph_x, glyph_y, slot.width as f32, slot.height as f32],
                        color_floats(color),
                    );
                    instance.uv = slot.uv;
                    instance.extra0 = [y, shear, 0.0, 0.0];
                    self.push(draws, Aux::None, instance);
                    if bold {
                        instance.rect[0] += 1.0;
                        self.push(draws, Aux::None, instance);
                    }
                }
                pen_x += slot.advance + letter_spacing;
            }
        }
        self.last_text_x = x;
        self.last_text_y = y;
        self.last_text_width = pen_x - x;
        self.last_font_size = font_size;
    }

    /// A control mark (check tick, radio dot, dropdown arrow, value bar).
    fn mark(&mut self, draws: &mut Vec<Draw>, rect: &Rect, color: lumen_css::Color, mark: Mark) {
        match mark {
            Mark::Check => {
                let point = |fx: f32, fy: f32| (rect.x + rect.width * fx, rect.y + rect.height * fy);
                let thickness = (rect.width.min(rect.height) * 0.16).max(1.4);
                self.segment(draws, point(0.24, 0.55), point(0.43, 0.74), thickness, color);
                self.segment(draws, point(0.43, 0.74), point(0.78, 0.3), thickness, color);
            }
            Mark::Dot => {
                let inset = rect.width * 0.3;
                let disc = Rect {
                    x: rect.x + inset,
                    y: rect.y + inset,
                    width: rect.width - 2.0 * inset,
                    height: rect.height - 2.0 * inset,
                };
                let mut instance =
                    Instance::new(KIND_SOLID, rect_floats(disc), color_floats(color));
                let radius = disc.width / 2.0;
                instance.radii = [radius, radius, radius, radius];
                self.push(draws, Aux::None, instance);
            }
            Mark::Arrow => {
                let cx = rect.x + rect.width - 12.0;
                let cy = rect.y + rect.height / 2.0;
                let arm = 3.5;
                let stroke = lumen_css::Color::rgba(0x55, 0x52, 0x5c, color.a);
                self.segment(draws, (cx - arm, cy - 1.5), (cx, cy + 2.5), 1.6, stroke);
                self.segment(draws, (cx, cy + 2.5), (cx + arm, cy - 1.5), 1.6, stroke);
            }
            Mark::Fraction(fraction) => {
                let fill = Rect {
                    width: rect.width * fraction.fraction,
                    ..*rect
                };
                let radius = rect.height / 2.0;
                let mut instance =
                    Instance::new(KIND_SOLID, rect_floats(fill), color_floats(color));
                instance.radii = [radius, radius, radius, radius];
                self.push(draws, Aux::None, instance);
                if fraction.thumb {
                    let diameter = rect.height + 4.0;
                    let disc = Rect {
                        x: (rect.x + rect.width * fraction.fraction - diameter / 2.0)
                            .clamp(rect.x - 2.0, rect.x + rect.width - diameter + 2.0),
                        y: rect.y + rect.height / 2.0 - diameter / 2.0,
                        width: diameter,
                        height: diameter,
                    };
                    let mut instance = Instance::new(
                        KIND_SOLID,
                        rect_floats(disc),
                        color_floats(lumen_css::Color::rgba(0x1c, 0x52, 0x88, color.a)),
                    );
                    let radius = diameter / 2.0;
                    instance.radii = [radius, radius, radius, radius];
                    self.push(draws, Aux::None, instance);
                }
            }
        }
    }

    /// An antialiased thick line segment (CPU `draw_segment` parity).
    fn segment(
        &mut self,
        draws: &mut Vec<Draw>,
        from: (f32, f32),
        to: (f32, f32),
        thickness: f32,
        color: lumen_css::Color,
    ) {
        let radius = thickness / 2.0;
        let x0 = (from.0.min(to.0) - radius - 1.0).floor();
        let y0 = (from.1.min(to.1) - radius - 1.0).floor();
        let x1 = (from.0.max(to.0) + radius + 1.0).ceil();
        let y1 = (from.1.max(to.1) + radius + 1.0).ceil();
        let mut instance = Instance::new(
            KIND_SEGMENT,
            [x0, y0, x1 - x0, y1 - y0],
            color_floats(color),
        );
        instance.extra0 = [from.0, from.1, to.0, to.1];
        instance.extra1 = [thickness, 0.0, 0.0, 0.0];
        self.push(draws, Aux::None, instance);
    }
}

/// Matches every `PushTransform`/`PushFilter` with its pop index.
fn match_scope_pops(
    commands: &[DisplayCommand],
) -> (
    std::collections::HashMap<usize, usize>,
    std::collections::HashMap<usize, usize>,
) {
    let mut transforms = Vec::new();
    let mut filters = Vec::new();
    let mut transform_pops = std::collections::HashMap::new();
    let mut filter_pops = std::collections::HashMap::new();
    for (index, command) in commands.iter().enumerate() {
        match command {
            DisplayCommand::PushTransform { .. } => transforms.push(index),
            DisplayCommand::PopTransform => {
                if let Some(start) = transforms.pop() {
                    transform_pops.insert(start, index);
                }
            }
            DisplayCommand::PushFilter { .. } => filters.push(index),
            DisplayCommand::PopFilter => {
                if let Some(start) = filters.pop() {
                    filter_pops.insert(start, index);
                }
            }
            _ => {}
        }
    }
    (transform_pops, filter_pops)
}

/// Conservative page-space bounds of a subtree's painted output under
/// `matrix` (the fully composed transform at the scope's start).
/// Over-estimation is safe (a larger texture region); `None` means
/// "cannot bound" and punts to the whole-frame CPU fallback.
fn page_bounds(
    commands: &[DisplayCommand],
    matrix: Transform2D,
    font: Option<&SystemFont>,
) -> Option<Rect> {
    let mut stack = vec![matrix];
    let mut bounds: Option<Rect> = None;
    let include = |rect: Rect, matrix: &Transform2D, bounds: &mut Option<Rect>| {
        let mapped = map_rect(&rect, matrix);
        *bounds = Some(match *bounds {
            Some(existing) => Rect {
                x: existing.x.min(mapped.x),
                y: existing.y.min(mapped.y),
                width: (existing.x + existing.width)
                    .max(mapped.x + mapped.width)
                    - existing.x.min(mapped.x),
                height: (existing.y + existing.height)
                    .max(mapped.y + mapped.height)
                    - existing.y.min(mapped.y),
            },
            None => mapped,
        });
    };
    for command in commands {
        let current = *stack.last()?;
        match command {
            DisplayCommand::FillRect { rect, .. }
            | DisplayCommand::StrokeRect { rect, .. }
            | DisplayCommand::FillGradient { rect, .. }
            | DisplayCommand::DrawImage { rect, .. }
            | DisplayCommand::DrawMark { rect, .. } => include(*rect, &current, &mut bounds),
            DisplayCommand::DrawShadow {
                rect, blur, inset, ..
            } => {
                let rect = if *inset {
                    *rect
                } else {
                    let reach = blur.max(1.0) * 1.5 + 2.0;
                    Rect {
                        x: rect.x - reach,
                        y: rect.y - reach,
                        width: rect.width + 2.0 * reach,
                        height: rect.height + 2.0 * reach,
                    }
                };
                include(rect, &current, &mut bounds);
            }
            DisplayCommand::DrawText {
                x,
                y,
                text,
                font_size,
                letter_spacing,
                monospace,
                ..
            } => {
                let font = font?;
                let advance: f32 = text
                    .chars()
                    .map(|character| {
                        font.rasterize(character, *font_size, *monospace)
                            .metrics
                            .advance_width
                            + letter_spacing
                    })
                    .sum();
                // Margins cover glyph overhang, synthetic bold/italic and
                // any transform scale.
                include(
                    Rect {
                        x: x - font_size,
                        y: y - font_size * 1.5,
                        width: advance + 2.0 * font_size,
                        height: font_size * 2.0,
                    },
                    &current,
                    &mut bounds,
                );
            }
            DisplayCommand::PushTransform { matrix } => {
                stack.push(current.multiply(*matrix));
            }
            DisplayCommand::PopTransform => {
                stack.pop();
            }
            DisplayCommand::PushFilter { rect, .. } => include(*rect, &current, &mut bounds),
            DisplayCommand::PushClip { .. }
            | DisplayCommand::PopClip
            | DisplayCommand::PopFilter
            | DisplayCommand::PushFixed
            | DisplayCommand::PopFixed => {}
        }
    }
    bounds
}

/// The bounding box of a rect under a transform (exact for the
/// axis-aligned matrices that reach the GPU path).
fn map_rect(rect: &Rect, matrix: &Transform2D) -> Rect {    let corners = [
        matrix.apply(rect.x, rect.y),
        matrix.apply(rect.x + rect.width, rect.y),
        matrix.apply(rect.x, rect.y + rect.height),
        matrix.apply(rect.x + rect.width, rect.y + rect.height),
    ];
    let min_x = corners.iter().map(|(x, _)| *x).fold(f32::INFINITY, f32::min);
    let min_y = corners.iter().map(|(_, y)| *y).fold(f32::INFINITY, f32::min);
    let max_x = corners
        .iter()
        .map(|(x, _)| *x)
        .fold(f32::NEG_INFINITY, f32::max);
    let max_y = corners
        .iter()
        .map(|(_, y)| *y)
        .fold(f32::NEG_INFINITY, f32::max);
    Rect {
        x: min_x,
        y: min_y,
        width: max_x - min_x,
        height: max_y - min_y,
    }
}

/// Interpolates the stop list at `progress` — a port of the CPU
/// rasterizer's `gradient_color_at`.
fn gradient_color_at(stops: &[(lumen_css::Color, f32)], progress: f32) -> lumen_css::Color {
    let mut previous = &stops[0];
    if progress <= previous.1 {
        return previous.0;
    }
    for stop in &stops[1..] {
        if progress <= stop.1 {
            let span = (stop.1 - previous.1).max(f32::EPSILON);
            let t = (progress - previous.1) / span;
            let lerp =
                |a: u8, b: u8| -> u8 { (f32::from(a) + (f32::from(b) - f32::from(a)) * t) as u8 };
            return lumen_css::Color {
                r: lerp(previous.0.r, stop.0.r),
                g: lerp(previous.0.g, stop.0.g),
                b: lerp(previous.0.b, stop.0.b),
                a: lerp(previous.0.a, stop.0.a),
            };
        }
        previous = stop;
    }
    stops[stops.len() - 1].0
}
