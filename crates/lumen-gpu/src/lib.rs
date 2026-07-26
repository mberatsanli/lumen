//! GPU raster backend: executes the engine's display list with wgpu.
//!
//! One instanced-quad pipeline reproduces the CPU rasterizer's coverage
//! math in the fragment shader (rounded rects via SDF, Gaussian shadows
//! via the same `erf` approximation, segments, glyphs from an R8 atlas,
//! images, gradients from a 1D stops texture). Text uses the identical
//! `SystemFont` coverage bitmaps, so glyph quality matches the CPU path.
//! Rare features (rotated/skewed transforms, `filter` scopes, text
//! without a scalable font) take a whole-frame CPU fallback: the
//! software rasterizer paints the frame, which is then presented as a
//! texture — visual parity by construction, GPU speed for the common
//! case.

use lumen_engine::font::SystemFont;
use lumen_engine::geometry::{Corners, Rect};
use lumen_engine::paint::DisplayCommand;
use std::collections::HashMap;
use std::sync::Arc;

use lumen_css::Color;

mod walk;

const KIND_SOLID: f32 = 0.0;
const KIND_RING: f32 = 1.0;
const KIND_SHADOW: f32 = 2.0;
const KIND_SEGMENT: f32 = 3.0;
const KIND_GLYPH: f32 = 4.0;
const KIND_IMAGE: f32 = 5.0;
const KIND_GRADIENT: f32 = 6.0;

const GRADIENT_STOPS: u32 = 1024;
const MAX_CACHED_IMAGES: usize = 128;
const ATLAS_SIZE: u32 = 2048;

/// One display list painted over the previous ones within a frame.
pub struct Pass<'a> {
    pub commands: &'a [DisplayCommand],
    /// Page scroll applied outside `position: fixed` scopes.
    pub scroll_y: f32,
    /// Scroll value used inside fixed scopes (see the CPU
    /// [`lumen_engine::rasterize_with_fixed_origin`]).
    pub fixed_origin: f32,
}

impl<'a> Pass<'a> {
    #[must_use]
    pub fn new(commands: &'a [DisplayCommand], scroll_y: f32, fixed_origin: f32) -> Self {
        Self {
            commands,
            scroll_y,
            fixed_origin,
        }
    }
}

/// A GPU display-list renderer. Construction returns `None` where no
/// adapter/device is available — callers keep the CPU path then.
pub struct GpuRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    uniform_layout: wgpu::BindGroupLayout,
    tex_layout: wgpu::BindGroupLayout,
    uniform_buffer: wgpu::Buffer,
    uniform_group: wgpu::BindGroup,
    nearest: wgpu::Sampler,
    linear: wgpu::Sampler,
    pipelines: HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
    atlas: Atlas,
    white: TextureSlot,
    images: HashMap<usize, TextureSlot>,
    gradients: HashMap<u64, TextureSlot>,
    cpu_frame: Option<(u32, u32, wgpu::Texture, TextureSlot)>,
    /// Per-frame pool of CPU-rendered subtree textures (hybrid path for
    /// rotated/skewed transform scopes): (width, height, texture, slot).
    hybrid_pool: Vec<Option<(u32, u32, wgpu::Texture, TextureSlot)>>,
    /// Next pool slot to hand out this frame (reset per frame).
    hybrid_cursor: usize,
    instance_buffer: Option<(wgpu::Buffer, u64)>,
    surface: Option<SurfaceState>,
    offscreen: Option<(u32, u32, wgpu::Texture, wgpu::TextureView)>,
}

struct SurfaceState {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
}

struct TextureSlot {
    bind_group: wgpu::BindGroup,
    /// Keeps the source image alive so the pointer-keyed cache entry
    /// cannot be recycled for a different image.
    _keep: Option<Arc<lumen_engine::RasterImage>>,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    rect: [f32; 4],
    color: [f32; 4],
    radii: [f32; 4],
    extra0: [f32; 4],
    extra1: [f32; 4],
    extra2: [f32; 4],
    uv: [f32; 4],
    meta: [f32; 4],
}

impl Instance {
    fn new(kind: f32, rect: [f32; 4], color: [f32; 4]) -> Self {
        Self {
            rect,
            color,
            radii: [0.0; 4],
            extra0: [0.0; 4],
            extra1: [0.0; 4],
            extra2: [0.0; 4],
            uv: [0.0, 0.0, 1.0, 1.0],
            meta: [kind, 0.0, 0.0, 0.0],
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Aux {
    None,
    Image(usize),
    Gradient(u64),
    CpuFrame,
    /// A CPU-rendered subtree texture (rotated/skewed scopes) from the
    /// per-frame hybrid pool.
    Hybrid(usize),
}

struct Draw {
    scissor: (u32, u32, u32, u32),
    aux: Aux,
    instances: Vec<Instance>,
}

impl GpuRenderer {
    /// Headless renderer with no surface — for offscreen rendering and
    /// tests. `None` where no GPU adapter is available.
    #[must_use]
    pub fn new_headless() -> Option<Self> {
        Self::init(None).map(|(renderer, _)| renderer)
    }

    /// Renderer presenting to a window surface (`Arc<winit::window::Window>`
    /// or any other `'static` raw-window-handle target). `None` where GPU
    /// presentation is unavailable — keep the CPU path then.
    #[must_use]
    pub fn for_surface(
        target: impl Into<wgpu::SurfaceTarget<'static>>,
        width: u32,
        height: u32,
    ) -> Option<Self> {
        let (mut renderer, pair) = Self::init(Some(target.into()))?;
        let (adapter, surface) = pair?;
        let capabilities = surface.get_capabilities(&adapter);
        // Non-sRGB: blending then matches the CPU's naive byte math and
        // presentation matches what softbuffer showed.
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(|format| !format.is_srgb())
            .unwrap_or(capabilities.formats.first().copied()?);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: capabilities
                .alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: Vec::new(),
        };
        surface.configure(&renderer.device, &config);
        renderer.surface = Some(SurfaceState { surface, config });
        Some(renderer)
    }

    /// Creates device/queue (optionally with a compatible surface) and
    /// the shared rendering state. The surface and adapter travel
    /// alongside so `for_surface` can configure them afterwards.
    fn init(
        target: Option<wgpu::SurfaceTarget<'static>>,
    ) -> Option<(Self, Option<(wgpu::Adapter, wgpu::Surface<'static>)>)> {
        let instance = wgpu::Instance::default();
        let surface = match target {
            Some(target) => Some(instance.create_surface(target).ok()?),
            None => None,
        };
        let adapter = pollster::block_on(instance.request_adapter(
            &wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: surface.as_ref(),
                force_fallback_adapter: false,
            },
        ))?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
                .ok()?;

        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("lumen-uniform-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let tex_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("lumen-texture-layout"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                sampler_entry(2),
                sampler_entry(3),
            ],
        });
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lumen-uniforms"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let uniform_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("lumen-uniform-group"),
            layout: &uniform_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });
        let nearest = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("lumen-nearest"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let linear = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("lumen-linear"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let atlas = Atlas::new(&device);
        let white_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("lumen-white"),
            size: wgpu::Extent3d {
                width: 1,
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
        queue.write_texture(
            white_texture.as_image_copy(),
            &[255, 255, 255, 255],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let white_view = white_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let white_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("lumen-texture-group"),
            layout: &tex_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&atlas.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&white_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&nearest),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&linear),
                },
            ],
        });
        let renderer = Self {
            device,
            queue,
            uniform_layout,
            tex_layout,
            uniform_buffer,
            uniform_group,
            nearest,
            linear,
            pipelines: HashMap::new(),
            atlas,
            white: TextureSlot {
                bind_group: white_group,
                _keep: None,
            },
            images: HashMap::new(),
            gradients: HashMap::new(),
            cpu_frame: None,
            hybrid_pool: Vec::new(),
            hybrid_cursor: 0,
            instance_buffer: None,
            surface: None,
            offscreen: None,
        };
        Some((renderer, surface.map(|surface| (adapter, surface))))
    }

    /// Wraps a texture in a bind group paired with the glyph atlas and
    /// the two shared samplers.
    fn make_slot(
        &self,
        texture: &wgpu::Texture,
        keep: Option<Arc<lumen_engine::RasterImage>>,
    ) -> TextureSlot {
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("lumen-texture-group"),
            layout: &self.tex_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&self.atlas.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.nearest),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.linear),
                },
            ],
        });
        TextureSlot {
            bind_group,
            _keep: keep,
        }
    }

    /// The next hybrid-pool texture at least `width`×`height`, for a
    /// CPU-rendered transform subtree. Returns the pool index (for the
    /// bind group lookup) and the texture to upload into.
    pub(crate) fn hybrid_slot(&mut self, width: u32, height: u32) -> Option<(usize, wgpu::Texture)> {
        let index = self.hybrid_cursor;
        self.hybrid_cursor += 1;
        if self.hybrid_pool.len() <= index {
            self.hybrid_pool.push(None);
        }
        let recreate =
            !matches!(&self.hybrid_pool[index], Some((w, h, _, _)) if *w == width && *h == height);
        if recreate {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("lumen-hybrid"),
                size: wgpu::Extent3d {
                    width: width.max(1),
                    height: height.max(1),
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
            self.hybrid_pool[index] = Some((width, height, texture, slot));
        }
        self.hybrid_pool[index]
            .as_ref()
            .map(|(_, _, texture, _)| (index, texture.clone()))
    }

    /// Reconfigures the surface after a window resize.
    pub fn resize(&mut self, width: u32, height: u32) {
        if let Some(state) = &mut self.surface {
            let (width, height) = (width.max(1), height.max(1));
            if state.config.width == width && state.config.height == height {
                return;
            }
            state.config.width = width;
            state.config.height = height;
            state.surface.configure(&self.device, &state.config);
        }
    }

    /// Renders one display list to the window surface and presents.
    /// `None` when the surface is unusable — fall back to the CPU
    /// rasterizer then.
    pub fn render(
        &mut self,
        commands: &[DisplayCommand],
        scroll_y: f32,
        fixed_origin: f32,
        scale: f32,
        font: Option<&SystemFont>,
    ) -> Option<()> {
        self.render_passes(&[Pass::new(commands, scroll_y, fixed_origin)], scale, font)
    }

    /// Renders layered passes (page, then overlays, then chrome) to the
    /// window surface and presents.
    pub fn render_passes(
        &mut self,
        passes: &[Pass<'_>],
        scale: f32,
        font: Option<&SystemFont>,
    ) -> Option<()> {
        let (format, size) = {
            let state = self.surface.as_ref()?;
            (state.config.format, (state.config.width, state.config.height))
        };
        let frame = match self.surface.as_ref()?.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                let state = self.surface.as_ref()?;
                state.surface.configure(&self.device, &state.config);
                state.surface.get_current_texture().ok()?
            }
            Err(_) => return None,
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let buffer = self.encode_frame(&view, size, format, passes, scale, font)?;
        self.queue.submit([buffer]);
        frame.present();
        Some(())
    }

    /// Renders one display list offscreen and returns RGBA8 pixels
    /// (row-major, `width * height * 4` bytes). For tests and tooling.
    #[allow(clippy::too_many_arguments)]
    pub fn render_offscreen(
        &mut self,
        commands: &[DisplayCommand],
        width: u32,
        height: u32,
        scroll_y: f32,
        fixed_origin: f32,
        scale: f32,
        font: Option<&SystemFont>,
    ) -> Option<Vec<u8>> {
        self.render_offscreen_passes(
            &[Pass::new(commands, scroll_y, fixed_origin)],
            width,
            height,
            scale,
            font,
        )
    }

    /// [`Self::render_offscreen`] with layered passes.
    pub fn render_offscreen_passes(
        &mut self,
        passes: &[Pass<'_>],
        width: u32,
        height: u32,
        scale: f32,
        font: Option<&SystemFont>,
    ) -> Option<Vec<u8>> {
        let (width, height) = (width.max(1), height.max(1));
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let recreate =
            !matches!(&self.offscreen, Some((w, h, _, _)) if *w == width && *h == height);
        if recreate {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("lumen-offscreen"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.offscreen = Some((width, height, texture, view));
        }
        let (texture, view) = {
            let (_, _, texture, view) = self.offscreen.as_ref()?;
            (texture.clone(), view.clone())
        };
        let buffer = self.encode_frame(&view, (width, height), format, passes, scale, font)?;
        self.queue.submit([buffer]);
        self.read_texture(&texture, width, height)
    }

    /// [`Self::render_offscreen`] without the readback: renders and waits
    /// for GPU completion, returning nothing. For frame-time benchmarks.
    #[allow(clippy::too_many_arguments)]
    #[doc(hidden)]
    pub fn render_offscreen_no_readback(
        &mut self,
        commands: &[DisplayCommand],
        width: u32,
        height: u32,
        scroll_y: f32,
        fixed_origin: f32,
        scale: f32,
        font: Option<&SystemFont>,
    ) -> Option<()> {
        let (width, height) = (width.max(1), height.max(1));
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let recreate =
            !matches!(&self.offscreen, Some((w, h, _, _)) if *w == width && *h == height);
        if recreate {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("lumen-offscreen"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.offscreen = Some((width, height, texture, view));
        }
        let view = self.offscreen.as_ref()?.3.clone();
        let buffer = self.encode_frame(
            &view,
            (width, height),
            format,
            &[Pass::new(commands, scroll_y, fixed_origin)],
            scale,
            font,
        )?;
        self.queue.submit([buffer]);
        Some(())
    }

    /// Reads an RGBA8 texture back into CPU memory.
    fn read_texture(&self, texture: &wgpu::Texture, width: u32, height: u32) -> Option<Vec<u8>> {
        let row_bytes = width * 4;
        let padded =
            row_bytes.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
                * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lumen-readback"),
            size: u64::from(padded) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([encoder.finish()]);
        let slice = readback.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        let _ = self.device.poll(wgpu::Maintain::Wait);
        receiver.recv().ok()?.ok()?;
        let mapped = slice.get_mapped_range();
        let mut pixels = Vec::with_capacity((row_bytes * height) as usize);
        for row in 0..height as usize {
            let start = row * padded as usize;
            pixels.extend_from_slice(&mapped[start..start + row_bytes as usize]);
        }
        drop(mapped);
        readback.unmap();
        Some(pixels)
    }

    /// Builds the frame's command buffer: walk the passes into instanced
    /// draws, upload instances, encode with per-draw scissor/texture.
    fn encode_frame(
        &mut self,
        view: &wgpu::TextureView,
        size: (u32, u32),
        format: wgpu::TextureFormat,
        passes: &[Pass<'_>],
        scale: f32,
        font: Option<&SystemFont>,
    ) -> Option<wgpu::CommandBuffer> {
        let mut draws = Vec::new();
        let native = !passes
            .iter()
            .any(|pass| needs_cpu_fallback(pass.commands, font));
        let mut ok = native;
        if ok {
            self.hybrid_cursor = 0;
            for pass in passes {
                if !self.walk(pass, size, scale, font, &mut draws) {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            // A scope the walker could not handle (e.g. a rotated
            // transform inside `position: fixed`): whole-frame CPU render.
            draws.clear();
            self.cpu_frame_draw(passes, size, scale, font, &mut draws);
        }
        let instances: Vec<Instance> = draws
            .iter()
            .flat_map(|draw| draw.instances.iter().copied())
            .collect();
        if !instances.is_empty() {
            let bytes: &[u8] = bytemuck::cast_slice(&instances);
            let needed = bytes.len() as u64;
            let capacity = self
                .instance_buffer
                .as_ref()
                .map_or(0, |(_, capacity)| *capacity);
            if capacity < needed {
                self.instance_buffer = Some((
                    self.device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("lumen-instances"),
                        size: needed.next_power_of_two(),
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    }),
                    needed.next_power_of_two(),
                ));
            }
            self.queue
                .write_buffer(&self.instance_buffer.as_ref()?.0, 0, bytes);
        }
        self.queue.write_buffer(
            &self.uniform_buffer,
            0,
            bytemuck::cast_slice(&[size.0 as f32, size.1 as f32, 0.0, 0.0]),
        );
        let pipeline = self.pipeline(format)?;
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut render = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("lumen-frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::WHITE),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            render.set_pipeline(&pipeline);
            render.set_bind_group(0, &self.uniform_group, &[]);
            let mut offset: u32 = 0;
            for draw in &draws {
                let count = draw.instances.len() as u32;
                if count == 0 {
                    continue;
                }
                let Some(group) = self.aux_bind_group(draw.aux) else {
                    offset += count;
                    continue;
                };
                let (x, y, width, height) = draw.scissor;
                render.set_scissor_rect(x, y, width, height);
                render.set_bind_group(1, group, &[]);
                let buffer = &self.instance_buffer.as_ref()?.0;
                render.set_vertex_buffer(0, buffer.slice(..));
                render.draw(0..6, offset..offset + count);
                offset += count;
            }
        }
        Some(encoder.finish())
    }

    fn pipeline(&mut self, format: wgpu::TextureFormat) -> Option<wgpu::RenderPipeline> {
        if !self.pipelines.contains_key(&format) {
            let shader = self
                .device
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("lumen-shader"),
                    source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
                });
            let layout = self
                .device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("lumen-pipeline-layout"),
                    bind_group_layouts: &[&self.uniform_layout, &self.tex_layout],
                    push_constant_ranges: &[],
                });
            let attributes: Vec<wgpu::VertexAttribute> = (0..8u32)
                .map(|index| wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: u64::from(index) * 16,
                    shader_location: index,
                })
                .collect();
            let pipeline = self
                .device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("lumen-pipeline"),
                    layout: Some(&layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some("vs_main"),
                        compilation_options: wgpu::PipelineCompilationOptions::default(),
                        buffers: &[wgpu::VertexBufferLayout {
                            array_stride: std::mem::size_of::<Instance>() as u64,
                            step_mode: wgpu::VertexStepMode::Instance,
                            attributes: &attributes,
                        }],
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some("fs_main"),
                        compilation_options: wgpu::PipelineCompilationOptions::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format,
                            blend: Some(wgpu::BlendState {
                                color: wgpu::BlendComponent {
                                    src_factor: wgpu::BlendFactor::SrcAlpha,
                                    dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                                    operation: wgpu::BlendOperation::Add,
                                },
                                alpha: wgpu::BlendComponent {
                                    src_factor: wgpu::BlendFactor::One,
                                    dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                                    operation: wgpu::BlendOperation::Add,
                                },
                            }),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    primitive: wgpu::PrimitiveState::default(),
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    multiview: None,
                    cache: None,
                });
            self.pipelines.insert(format, pipeline);
        }
        self.pipelines.get(&format).cloned()
    }

    fn aux_bind_group(&self, aux: Aux) -> Option<&wgpu::BindGroup> {
        match aux {
            Aux::None => Some(&self.white.bind_group),
            Aux::Image(key) => self.images.get(&key).map(|slot| &slot.bind_group),
            Aux::Gradient(key) => self.gradients.get(&key).map(|slot| &slot.bind_group),
            Aux::CpuFrame => self
                .cpu_frame
                .as_ref()
                .map(|(_, _, _, slot)| &slot.bind_group),
            Aux::Hybrid(index) => self
                .hybrid_pool
                .get(index)
                .and_then(|entry| entry.as_ref())
                .map(|(_, _, _, slot)| &slot.bind_group),
        }
    }
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn sampler_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    }
}

/// One packed glyph: atlas UVs plus the metrics the pen advance needs,
/// so cached glyphs skip the font lookup entirely.
#[derive(Clone, Copy)]
pub(crate) struct GlyphSlot {
    pub uv: [f32; 4],
    pub advance: f32,
    pub xmin: i32,
    pub ymin: i32,
    pub width: u32,
    pub height: u32,
}

/// A growable R8 glyph coverage atlas with simple shelf packing.
struct Atlas {
    view: wgpu::TextureView,
    texture: wgpu::Texture,
    entries: HashMap<(char, u32, bool), GlyphSlot>,
    /// Open shelves: (y, height, next_x).
    shelves: Vec<(u32, u32, u32)>,
    next_shelf_y: u32,
}

impl Atlas {
    fn new(device: &wgpu::Device) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("lumen-glyph-atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        Self {
            view: texture.create_view(&wgpu::TextureViewDescriptor::default()),
            texture,
            entries: HashMap::new(),
            shelves: Vec::new(),
            next_shelf_y: 0,
        }
    }

    /// The atlas slot for one glyph, uploading its coverage on first
    /// use. Cached glyphs cost a single hash lookup — no font access.
    fn entry(
        &mut self,
        queue: &wgpu::Queue,
        font: &SystemFont,
        character: char,
        size: f32,
        monospace: bool,
    ) -> Option<GlyphSlot> {
        let key = (character, size.to_bits(), monospace);
        if let Some(slot) = self.entries.get(&key) {
            return Some(*slot);
        }
        let glyph = font.rasterize(character, size, monospace);
        let (width, height) = (glyph.metrics.width as u32, glyph.metrics.height as u32);
        let mut slot = GlyphSlot {
            uv: [0.0; 4],
            advance: glyph.metrics.advance_width,
            xmin: glyph.metrics.xmin,
            ymin: glyph.metrics.ymin,
            width,
            height,
        };
        if width > 0 && height > 0 {
            let (x, y) = self.allocate(width + 1, height + 1).or_else(|| {
                // Atlas full: drop everything and re-pack on demand.
                // Cached glyphs re-upload as they are requested again.
                self.entries.clear();
                self.shelves.clear();
                self.next_shelf_y = 0;
                self.allocate(width + 1, height + 1)
            })?;
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &glyph.coverage,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
            slot.uv = uv_rect(x, y, width, height);
        }
        self.entries.insert(key, slot);
        Some(slot)
    }

    fn allocate(&mut self, width: u32, height: u32) -> Option<(u32, u32)> {
        for shelf in &mut self.shelves {
            let (y, shelf_height, next_x) = *shelf;
            if height <= shelf_height && next_x + width <= ATLAS_SIZE {
                *shelf = (y, shelf_height, next_x + width);
                return Some((next_x, y));
            }
        }
        if self.next_shelf_y + height > ATLAS_SIZE {
            return None;
        }
        let y = self.next_shelf_y;
        self.shelves.push((y, height, width));
        self.next_shelf_y += height;
        Some((0, y))
    }
}

fn uv_rect(x: u32, y: u32, width: u32, height: u32) -> [f32; 4] {
    let scale = ATLAS_SIZE as f32;
    [
        x as f32 / scale,
        y as f32 / scale,
        (x + width) as f32 / scale,
        (y + height) as f32 / scale,
    ]
}

/// Frames the GPU path cannot reproduce exactly render through the CPU
/// rasterizer: text without a scalable font (the bitmap-font path is
/// CPU-only). Rotated transforms and `filter` scopes do NOT force this —
/// they take the per-subtree hybrid path in the walk.
fn needs_cpu_fallback(commands: &[DisplayCommand], font: Option<&SystemFont>) -> bool {
    font.is_none()
        && commands
            .iter()
            .any(|command| matches!(command, DisplayCommand::DrawText { .. }))
}

fn color_floats(color: Color) -> [f32; 4] {
    [
        f32::from(color.r) / 255.0,
        f32::from(color.g) / 255.0,
        f32::from(color.b) / 255.0,
        f32::from(color.a) / 255.0,
    ]
}

fn rect_floats(rect: Rect) -> [f32; 4] {
    [rect.x, rect.y, rect.width, rect.height]
}

fn radii_floats(radius: &Corners<f32>, scale: f32) -> [f32; 4] {
    [
        radius.top_left * scale,
        radius.top_right * scale,
        radius.bottom_right * scale,
        radius.bottom_left * scale,
    ]
}
