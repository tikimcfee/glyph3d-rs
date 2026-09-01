//! Scene: the 1M-instance quad field, its pipeline, camera, and frame encoding.
//!
//! This module is deliberately mode-agnostic — windowed and offscreen both build
//! one `Scene` and call `render()` into whatever target view they own.

use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3};
use wgpu::util::DeviceExt;

use crate::gpu::GpuContext;

/// The views a frame draws into, plus their physical-pixel size.
/// (Stage F: the cull/LOD pass needs the height to convert world units to
/// on-screen pixels.) Bundled so `SceneLike::render` stays a 4-arg signature.
pub struct FrameTarget<'a> {
    pub color_view: &'a wgpu::TextureView,
    pub depth_view: &'a wgpu::TextureView,
    pub width: u32,
    pub height: u32,
}

/// Mode-agnostic scene interface: windowed and offscreen modes drive any scene
/// (Stage A quad field, Stage C glyph field, …) through exactly this.
pub trait SceneLike {
    fn depth_format(&self) -> wgpu::TextureFormat;
    fn instance_count(&self) -> u32;
    fn render(
        &self,
        ctx: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        target: &FrameTarget<'_>,
        t: f32,
    );

    // ── Stage F: interactive hooks (windowed only) ─────────────────────────
    // Default no-ops: the Stage A demo scene and all offscreen runs ignore
    // them. `tick` integrates per-frame camera motion; the input handlers
    // feed it. `dt` is wall-clock seconds since the previous frame.
    // Stage G: the hooks take &GpuContext so scenes can issue partial buffer
    // uploads from event handlers (pick flash, group drags, verb keys).
    fn on_key(&mut self, _ctx: &GpuContext, _key: winit::keyboard::KeyCode, _pressed: bool) {}
    fn on_mouse_look(&mut self, _ctx: &GpuContext, _dx: f32, _dy: f32) {}
    fn on_scroll(&mut self, _ctx: &GpuContext, _lines: f32) {}
    /// Cursor position in physical px (tracked even without pointer grab).
    fn on_cursor(&mut self, _ctx: &GpuContext, _x: f32, _y: f32) {}
    /// Left-click at a physical px position (ungrabbed pointer only).
    fn on_click(&mut self, _ctx: &GpuContext, _x: f32, _y: f32) {}
    fn tick(&mut self, _dt: f32) {}

    // ── Stage G: scripted picking & manipulation (offscreen + windowed) ────
    /// Viewport in physical px for ray unprojection (offscreen sets this
    /// before scripted picks; windowed scenes track it per render).
    fn set_viewport(&mut self, _w: u32, _h: u32) {}
    /// Pin the camera to an explicit Fly pose (scripted oblique-pick repro).
    /// Default no-op (non-camera scenes ignore it).
    fn set_cam_pose(&mut self, _eye: [f32; 3], _yaw: f32, _pitch: f32) {}
    /// Resolve a pick; returns a log line (None = scene doesn't support it).
    fn apply_pick(&mut self, _ctx: &GpuContext, _cmd: &crate::glyph_scene::PickCommand) -> Option<String> {
        None
    }
    /// Apply a manipulation verb to the current pick; returns a log line.
    fn apply_verb(&mut self, _ctx: &GpuContext, _verb: &crate::glyph_scene::Verb) -> Option<String> {
        None
    }
    /// Stage G debug: read back instance bytes at a global slot (partial-
    /// upload verification). Default no-op.
    fn debug_dump_instances(&self, _ctx: &GpuContext, _slot: u64, _out: &mut [u32]) {}
}

/// Stress-test target: 1,000,000 instances, one draw call.
pub const INSTANCE_COUNT: u32 = 1_000_000;

/// Per-instance GPU record. 48 bytes, mirrors `InstanceSlot` in quad_field.wgsl.
///
/// FUTURE STAGES: this becomes the glyph slot record (~60 B: 8 u32
/// count/identity lanes + 7 f32 measures). Keep it `#[repr(C)]` + `Pod` and keep
/// the storage-buffer-in-vertex-shader access pattern — only the fields change.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct InstanceSlot {
    pos: [f32; 3],
    scale: f32,
    color: [f32; 4],
    lanes: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct CameraUniform {
    view_proj: [f32; 16],
}

pub struct Scene {
    pub pipeline: wgpu::RenderPipeline,
    pub bind_group: wgpu::BindGroup,
    camera_buf: wgpu::Buffer,
    depth_format: wgpu::TextureFormat,
    instance_count: u32,
}

/// Deterministic PRNG (xorshift64*) so both modes see the identical field
/// and runs are reproducible for verification screenshots.
struct Rng(u64);
impl Rng {
    fn next_f32(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        let v = x.wrapping_mul(0x2545F4914F6CDD1D);
        // map to [0, 1)
        (v >> 11) as f32 / (1u64 << 53) as f32
    }
}

impl Scene {
    pub fn new(ctx: &GpuContext, color_format: wgpu::TextureFormat) -> Self {
        let device = &ctx.device;

        // --- instance field ------------------------------------------------
        // 1000 x 1000 grid, 1.6-unit spacing -> a ~1600-unit sprawling plane.
        // Slight y jitter + hue variation so depth and density read visually.
        let grid = (INSTANCE_COUNT as f64).sqrt() as usize; // 1000^2 = 1,000,000
        debug_assert_eq!((grid * grid) as u32, INSTANCE_COUNT);
        let spacing = 1.6f32;
        let half = (grid as f32 - 1.0) * spacing * 0.5;
        let mut rng = Rng(0x9E3779B97F4A7C15);
        let mut data = Vec::with_capacity(grid * grid);
        for gz in 0..grid {
            for gx in 0..grid {
                let x = gx as f32 * spacing - half;
                let z = gz as f32 * spacing - half;
                // cheap radial "terrain" so the plane is not perfectly flat
                let r = (x * x + z * z).sqrt();
                let y = (r * 0.012).sin() * 14.0 + rng.next_f32() * 2.0;
                let t = gx as f32 / grid as f32;
                let u = gz as f32 / grid as f32;
                // vibrant two-axis gradient, alpha 1
                let color = [
                    0.15 + 0.85 * t,
                    0.25 + 0.65 * (1.0 - (t - u).abs()),
                    0.15 + 0.85 * (1.0 - u),
                    1.0,
                ];
                data.push(InstanceSlot {
                    pos: [x, y, z],
                    scale: 0.9 + rng.next_f32() * 0.6,
                    color,
                    lanes: [0; 4],
                });
            }
        }
        let instance_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("instance field"),
            contents: bytemuck::cast_slice(&data),
            usage: wgpu::BufferUsages::STORAGE,
        });
        log::info!(
            "instance field: {} instances, {} MiB storage buffer",
            data.len(),
            (data.len() * std::mem::size_of::<InstanceSlot>()) >> 20
        );

        // --- camera uniform --------------------------------------------------
        let camera_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("camera uniform"),
            size: std::mem::size_of::<CameraUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // --- pipeline ---------------------------------------------------------
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scene bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scene bg"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: camera_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: instance_buf.as_entire_binding(),
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("quad_field.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/quad_field.wgsl").into()),
        });

        let depth_format = wgpu::TextureFormat::Depth32Float;
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("scene pl"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("quad field pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[], // geometry from vertex_index — no vertex buffers
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: color_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None, // quads face +Y; camera looks down, keep both sides safe
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: depth_format,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState::default(), // MSAA off (stage A)
            multiview_mask: None,
            cache: None,
        });

        Self {
            pipeline,
            bind_group,
            camera_buf,
            depth_format,
            instance_count: data.len() as u32,
        }
    }

    /// Camera at time `t`: slow orbit above the field, looking at its center.
    fn camera(&self, t: f32, aspect: f32) -> Mat4 {
        let radius = 950.0;
        let height = 620.0;
        let angle = t * 0.15; // rad/s — one orbit ~42 s
        let eye = Vec3::new(radius * angle.cos(), height, radius * angle.sin());
        let view = glam::camera::rh::view::look_at_mat4(eye, Vec3::ZERO, Vec3::Y);
        let proj =
            glam::camera::rh::proj::directx::perspective(60f32.to_radians(), aspect, 1.0, 5000.0);
        proj * view
    }

    /// Upload the camera for time `t` and encode one full frame (clear + one
    /// instanced draw) into `encoder`. Both modes call exactly this.
    pub fn render(
        &self,
        ctx: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        target: &FrameTarget<'_>,
        t: f32,
    ) {
        let FrameTarget {
            color_view,
            depth_view,
            width,
            height,
        } = *target;
        let aspect = width as f32 / height.max(1) as f32;
        let cam = CameraUniform {
            view_proj: self.camera(t, aspect).to_cols_array(),
        };
        ctx.queue
            .write_buffer(&self.camera_buf, 0, bytemuck::bytes_of(&cam));

        // Stage H: pass-level GPU timer (see GlyphScene::render for the scheme).
        let pass_query = ctx
            .profiler
            .as_ref()
            .map(|p| p.borrow().begin_pass_query("quad field pass", encoder));
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("quad field pass"),
            timestamp_writes: pass_query
                .as_ref()
                .and_then(|q| q.render_pass_timestamp_writes()),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: color_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.02,
                        g: 0.02,
                        b: 0.04,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        // 6 vertices per quad, all instances, ONE draw call.
        pass.draw(0..6, 0..self.instance_count);
        drop(pass);
        if let (Some(p), Some(q)) = (&ctx.profiler, pass_query) {
            p.borrow().end_query(encoder, q);
        }
    }
}

impl SceneLike for Scene {
    fn depth_format(&self) -> wgpu::TextureFormat {
        self.depth_format
    }
    fn instance_count(&self) -> u32 {
        self.instance_count
    }
    fn render(
        &self,
        ctx: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        target: &FrameTarget<'_>,
        t: f32,
    ) {
        // Delegate to the inherent implementation.
        Scene::render(self, ctx, encoder, target, t);
    }
}

/// Helper: depth texture matching a given target size. Both modes need one.
pub fn create_depth(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("depth"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
        .create_view(&Default::default())
}
