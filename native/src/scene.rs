//! Scene: the 1M-instance quad field, its pipeline, camera, and frame encoding.
//!
//! This module is deliberately mode-agnostic — windowed and offscreen both build
//! one `Scene` and call `render()` into whatever target view they own.

#[cfg(feature = "wgpu")]
use bytemuck::{Pod, Zeroable};
#[cfg(feature = "wgpu")]
use glam::{Mat4, Vec3};
#[cfg(feature = "wgpu")]
use wgpu::util::DeviceExt;

#[cfg(feature = "wgpu")]
use crate::gpu::GpuContext;

#[cfg(feature = "wgpu")]
/// The views a frame draws into, plus their physical-pixel size.
/// (Stage F: the cull/LOD pass needs the height to convert world units to
/// on-screen pixels.) Bundled so `SceneLike::render` stays a 4-arg signature.
///
/// Stage L (L3): `color_texture` + `color_format` added for the composite
/// step — the copy path needs the texture handle (a TextureView can't be a
/// copy endpoint), and the copy-vs-shader split keys on the format
/// (Rgba8UnormSrgb offscreen → copy; Bgra8UnormSrgb windowed → shader).
pub struct FrameTarget<'a> {
    pub color_view: &'a wgpu::TextureView,
    pub color_texture: &'a wgpu::Texture,
    pub color_format: wgpu::TextureFormat,
    pub depth_view: &'a wgpu::TextureView,
    pub width: u32,
    pub height: u32,
}

/// The keys a scene acts on, by physical position (a US layout's names):
/// the fly camera's WASD/E R/Q F, the verb keys and the deck/page keys
/// (`GlyphScene::verb_key`). Engine-owned since 2026-10-26, so the scene
/// takes no windowing crate's key type: the host translates its own key
/// events (`windowed::app::scene_key` in the windowed driver) and drops the rest, and
/// offscreen scripts and the panel's buttons name these directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SceneKey {
    W,
    A,
    S,
    D,
    E,
    R,
    Q,
    F,
    H,
    T,
    X,
    B,
    G,
    C,
    V,
    N,
    P,
    BracketLeft,
    BracketRight,
    ArrowLeft,
    ArrowRight,
    Comma,
    Period,
    Home,
    End,
}

/// Mode-agnostic scene interface: windowed and offscreen modes, and a host
/// with its own window and renderer (the makepad port), drive any scene
/// (the quad demo, the glyph field, …) through exactly this.
///
/// Since 2026-10-31 a scene holds no device type: its input, picks and
/// verbs edit host state, and a frame is computed on the CPU
/// (`prepare_frame`). What draws it is a [`SceneRenderer`], built from the
/// scene by `make_renderer` (wgpu), or a host's own renderer reading the
/// frames.
pub trait SceneLike {
    fn instance_count(&self) -> u32;
    /// The CPU half of a frame at `viewport` (physical px) and `t`, with no
    /// wgpu type (`glyph_scene::FrameData`): what every renderer draws.
    /// None for a scene without one (the quad demo, which its own renderer
    /// draws).
    fn prepare_frame(&self, _viewport: (u32, u32), _t: f32) -> Option<crate::glyph_scene::FrameData<'_>> {
        None
    }

    // ── Stage F: interactive hooks (windowed only) ─────────────────────────
    // Default no-ops: the demo scene and all offscreen runs ignore them.
    // `tick` integrates per-frame camera motion; the input handlers feed
    // it. `dt` is wall-clock seconds since the previous frame. Every edit
    // they make lands in host state, which the next frame carries.
    fn on_key(&mut self, _key: SceneKey, _pressed: bool) {}
    fn on_mouse_look(&mut self, _dx: f32, _dy: f32) {}
    fn on_scroll(&mut self, _lines: f32) {}
    /// Cursor position in physical px (tracked even without pointer grab).
    fn on_cursor(&mut self, _x: f32, _y: f32) {}
    /// Left-click at a physical px position (ungrabbed pointer only).
    fn on_click(&mut self, _x: f32, _y: f32) {}
    fn tick(&mut self, _dt: f32) {}
    /// Scene-driven animation (2026-10-10: the library's page turns and
    /// relayouts): one step of `dt` seconds. Both drivers call it before
    /// every frame — windowed with wall-clock dt, offscreen with the fixed
    /// 1/60 s clock — so an animation is scriptable and deterministic
    /// offscreen. Default: nothing animates.
    fn animate(&mut self, _dt: f32) {}

    // ── Stage G: scripted picking & manipulation (offscreen + windowed) ────
    /// Viewport in physical px for ray unprojection (offscreen sets this
    /// before scripted picks; windowed scenes track it per render).
    fn set_viewport(&mut self, _w: u32, _h: u32) {}
    /// Pin the camera to an explicit Fly pose (scripted oblique-pick repro).
    /// Default no-op (non-camera scenes ignore it).
    fn set_cam_pose(&mut self, _eye: [f32; 3], _yaw: f32, _pitch: f32) {}
    /// The Fly pose `set_cam_pose` would reproduce: eye, yaw, pitch in
    /// RADIANS (the F2 `GLYPH_POSE_PRINT` line converts to the degrees
    /// `--cam-pose` takes). None for scenes without a fly camera.
    fn cam_pose(&self) -> Option<([f32; 3], f32, f32)> {
        None
    }
    /// The panel's "Show glyphs" threshold (px per text row), set live —
    /// what a windowed slider drag does to the cull cell, for offscreen's
    /// `GLYPH_FRAME_SCRIPT` (C32, 2026-10-11). Default no-op.
    fn set_show_glyphs_px(&mut self, _px: f32) {}
    /// Resolve a pick; returns a log line (None = scene doesn't support it).
    fn apply_pick(&mut self, _cmd: &crate::glyph_scene::PickCommand) -> Option<String> {
        None
    }
    /// Apply a manipulation verb to the current pick; returns a log line.
    fn apply_verb(&mut self, _verb: &crate::glyph_scene::Verb) -> Option<String> {
        None
    }
    /// Returns the group ID of the currently selected file or glyph, if any selection is active.
    fn selected_group_id(&self) -> Option<u32> {
        None
    }
    /// S3 spike (`experiments/zedspike`): apply a Zed-pipeline highlight
    /// sidecar to per-glyph instance colors (`--highlight`, repo scenes
    /// only); returns a log line. Default: scene doesn't support it.
    fn apply_highlight_sidecar(&self, _path: &std::path::Path) -> Option<String> {
        None
    }
    /// P1c: the seam's CONTRACT consumer — apply provider envelopes, with
    /// the version law enforced against the re-derived bytes. Default:
    /// scene doesn't support it.
    fn apply_surface_updates(&self, _updates: &[crate::seam::SurfaceUpdate]) -> Option<String> {
        None
    }

    /// Retrieve and clear any pending carrel layout options signaled by carrel navigation.
    fn take_pending_carrel_options(&mut self) -> Option<crate::spatial_scene::CarrelLayoutOptions> {
        None
    }

    #[cfg(feature = "wgpu")]
    /// The scene's draw on a wgpu device, drawing into `color_format`.
    /// Built after the scene, and again for every scene a relayout builds.
    fn make_renderer(&self, ctx: &GpuContext, color_format: wgpu::TextureFormat) -> Box<dyn SceneRenderer>;
}

#[cfg(feature = "wgpu")]
/// What draws a [`SceneLike`] on a wgpu device (2026-10-31: the GPU half
/// the scenes held until then). The windowed and offscreen drivers hold
/// one beside their scene.
pub trait SceneRenderer {
    fn depth_format(&self) -> wgpu::TextureFormat;
    /// Upload what `scene`'s frame at `t` changed and encode it into
    /// `encoder`, at `target`'s size.
    fn render(&mut self, ctx: &GpuContext, encoder: &mut wgpu::CommandEncoder, target: &FrameTarget<'_>, scene: &dyn SceneLike, t: f32);
    /// Stage G debug: read back the device's slot words at a global slot
    /// (partial-upload verification, `GLYPH_G_DUMP`). Default no-op.
    fn debug_dump_instances(&self, _ctx: &GpuContext, _slot: u64, _out: &mut [u32]) {}
}

/// Stress-test target: 1,000,000 instances, one draw call.
pub const INSTANCE_COUNT: u32 = 1_000_000;

#[cfg(feature = "wgpu")]
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

#[cfg(feature = "wgpu")]
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct CameraUniform {
    view_proj: [f32; 16],
}

#[cfg(feature = "wgpu")]
pub struct DemoRenderer {
    pub pipeline: wgpu::RenderPipeline,
    pub bind_group: wgpu::BindGroup,
    camera_buf: wgpu::Buffer,
    depth_format: wgpu::TextureFormat,
    instance_count: u32,
}

#[cfg(feature = "wgpu")]
/// Deterministic PRNG (xorshift64*) so both modes see the identical field
/// and runs are reproducible for verification screenshots.
struct Rng(u64);
#[cfg(feature = "wgpu")]
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

#[cfg(feature = "wgpu")]
impl DemoRenderer {
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
            label: Some("quad field camera uniform"),
            size: std::mem::size_of::<CameraUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // --- pipeline ---------------------------------------------------------
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("quad field bgl"),
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
            label: Some("quad field bg"),
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
            label: Some("quad field pl"),
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
                depth_compare: Some(wgpu::CompareFunction::Greater),
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
        let c = &crate::config::settings().quad_demo;
        let angle = t * c.orbit_rate;
        let eye = Vec3::new(c.orbit_radius * angle.cos(), c.orbit_height, c.orbit_radius * angle.sin());
        let view = glam::camera::rh::view::look_at_mat4(eye, Vec3::ZERO, Vec3::Y);
        let proj = glam::camera::rh::proj::directx::perspective(
            c.fov_y_deg.to_radians(),
            aspect,
            c.far,
            c.near,
        );
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
            // Stage L (L3): the demo Scene keeps direct rendering — it never
            // composites, so the texture handle and format go unused.
            ..
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
                    load: wgpu::LoadOp::Clear(crate::config::wgpu_color(
                        crate::config::settings().quad_demo.clear_color,
                    )),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(0.0),
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

/// The quad stress demo (`--demo`): a million instanced quads on a
/// slowly orbiting camera. The scene is nothing but its count; its field
/// and camera live in its renderer, which generates them.
pub struct DemoScene;

impl SceneLike for DemoScene {
    fn instance_count(&self) -> u32 {
        INSTANCE_COUNT
    }
    #[cfg(feature = "wgpu")]
    fn make_renderer(&self, ctx: &GpuContext, color_format: wgpu::TextureFormat) -> Box<dyn SceneRenderer> {
        Box::new(DemoRenderer::new(ctx, color_format))
    }
}

#[cfg(feature = "wgpu")]
impl SceneRenderer for DemoRenderer {
    fn depth_format(&self) -> wgpu::TextureFormat {
        self.depth_format
    }
    fn render(&mut self, ctx: &GpuContext, encoder: &mut wgpu::CommandEncoder, target: &FrameTarget<'_>, _scene: &dyn SceneLike, t: f32) {
        // Delegate to the inherent implementation.
        DemoRenderer::render(self, ctx, encoder, target, t);
    }
}

#[cfg(feature = "wgpu")]
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
