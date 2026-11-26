//! wgpu cell renderer for the terminal wall (`WallRenderer`).
//!
//! Free of `winit`: draws into whatever colour + depth target views the host
//! provides ([`WallTarget`]). Owns the shared pipelines and uploaded atlas
//! textures (`curves`, `glyphmap`, `emoji`) and caches per-terminal GPU cell
//! buffers so unchanged terminals upload zero bytes.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use glam::Vec2;
use term_cells::{CursorShape, Screen};
use wgpu::util::DeviceExt;

use crate::atlas::{Atlas, AtlasGpu};
use crate::config::WallSettings;
use crate::gpu::GpuContext;

use super::camera::WallCamera;
use super::cell::{CellGpu, dirty_row_ranges, pack_cell};
use super::metrics::CellMetrics;

/// Depth texture format expected by [`WallRenderer`] (reverse-Z, clear 0.0).
pub const WALL_DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// Render target views and physical-pixel dimensions given to [`WallRenderer::render`].
pub struct WallTarget<'a> {
    pub color_view: &'a wgpu::TextureView,
    pub depth_view: &'a wgpu::TextureView,
    pub width: u32,
    pub height: u32,
}

/// Visual style knobs for the wall canvas and terminals (seeded from `[wall]` in `config/defaults.toml`).
#[derive(Clone, Debug, PartialEq)]
pub struct WallStyle {
    pub clear_color: [f32; 4],
    pub panel_opacity: f32,
    pub panel_padding: f32,
    pub border_width_px: f32,
    pub border_focused_scale: f32,
    pub border_focused_color: [f32; 4],
    pub border_unfocused_color: [f32; 4],
    pub cursor_color: [f32; 4],
    pub cursor_fill_opacity: f32,
    pub cursor_border_px: f32,
    pub greek_px: f32,
    pub underline_y: f32,
    pub strike_y: f32,
    pub line_thickness: f32,
    pub curly_amplitude: f32,
    pub dim_alpha: f32,
}

impl From<&WallSettings> for WallStyle {
    fn from(s: &WallSettings) -> Self {
        Self {
            clear_color: s.clear_color,
            panel_opacity: s.panel_opacity,
            panel_padding: s.panel_padding,
            border_width_px: s.border_width_px,
            border_focused_scale: s.border_focused_scale,
            border_focused_color: s.border_focused_color,
            border_unfocused_color: s.border_unfocused_color,
            cursor_color: s.cursor_color,
            cursor_fill_opacity: s.cursor_fill_opacity,
            cursor_border_px: s.cursor_border_px,
            greek_px: s.greek_px,
            underline_y: s.underline_y,
            strike_y: s.strike_y,
            line_thickness: s.line_thickness,
            curly_amplitude: s.curly_amplitude,
            dim_alpha: s.dim_alpha,
        }
    }
}

impl Default for WallStyle {
    fn default() -> Self {
        Self::from(&crate::config::settings().wall)
    }
}

/// One terminal submitted to [`WallRenderer::render`].
pub struct TerminalDraw<'a> {
    /// Stable caller-chosen key identifying this terminal across frames (for GPU buffer caching).
    pub id: u64,
    /// Current screen snapshot. Uploaded only when `screen.generation` advances.
    pub screen: &'a Screen,
    /// Top-left corner of cell `(0, 0)` in world space (+X right, +Y up).
    pub position: Vec2,
    /// World depth; larger `z` is nearer to the camera and covers smaller `z`.
    pub z: f32,
    /// Whether this terminal holds keyboard focus (solid cursor + focused border).
    pub focused: bool,
    /// Optional per-terminal panel opacity override (`None` = [`WallStyle::panel_opacity`]).
    pub panel_opacity: Option<f32>,
    /// Optional per-terminal border colour override (packed sRGB RGBA8; `None` = focus/unfocused style).
    pub border_color: Option<u32>,
}

/// Shared per-frame uniform block (`WallFrameUniform` in `wall_cells.wgsl`, 224 B).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct WallFrameUniform {
    view_proj: [[f32; 4]; 4],
    viewport: [f32; 2],
    cell_size: [f32; 2],
    line_a: [f32; 4],
    line_b: [f32; 4],
    border_focused_color: [f32; 4],
    border_unfocused_color: [f32; 4],
    cursor_color: [f32; 4],
    emoji_cell: [u32; 2],
    emoji_cols: u32,
    emoji_rows: u32,
    emoji_layer: [f32; 2],
    greek_px: f32,
    dim_alpha: f32,
    panel_padding: f32,
    panel_border_px: f32,
    border_focused_scale: f32,
    cursor_fill_alpha: f32,
    cursor_border_px: f32,
    target_is_srgb: u32,
    _pad0: u32,
    _pad1: u32,
}

/// Per-terminal uniform block (`TerminalUniform` in `wall_cells.wgsl`, 48 B).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct TerminalUniform {
    origin: [f32; 2],
    z: f32,
    focused: u32,
    cols: u32,
    rows: u32,
    default_bg: u32,
    panel_opacity: f32,
    cursor_col: u32,
    cursor_row: u32,
    cursor_flags: u32,
    border_override: u32,
}

struct TerminalGpu {
    cols: u16,
    rows: u16,
    uploaded_gen: u64,
    row_has_bg: Vec<bool>,
    has_any_bg: bool,
    uniform_buf: wgpu::Buffer,
    cells_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

/// Shared wgpu cell renderer for any number of live terminals.
pub struct WallRenderer {
    atlas: Arc<Atlas>,
    _atlas_gpu: AtlasGpu,
    metrics: CellMetrics,
    style: WallStyle,
    color_format: wgpu::TextureFormat,
    frame_buf: wgpu::Buffer,
    quad_index_buf: wgpu::Buffer,
    shared_bg: wgpu::BindGroup,
    term_bgl: wgpu::BindGroupLayout,
    panel_pipeline: wgpu::RenderPipeline,
    fill_pipeline: wgpu::RenderPipeline,
    ink_pipeline: wgpu::RenderPipeline,
    cursor_pipeline: wgpu::RenderPipeline,
    terminals: HashMap<u64, TerminalGpu>,
    pack_scratch: Vec<CellGpu>,
}

const PREMUL_BLEND: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
        operation: wgpu::BlendOperation::Add,
    },
    alpha: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
        operation: wgpu::BlendOperation::Add,
    },
};

fn srgb_channel_to_linear_f64(c: f32) -> f64 {
    let c = f64::from(c.clamp(0.0, 1.0));
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn srgb_to_wgpu_clear_color(rgba: [f32; 4], target_is_srgb: bool) -> wgpu::Color {
    if target_is_srgb {
        wgpu::Color {
            r: srgb_channel_to_linear_f64(rgba[0]),
            g: srgb_channel_to_linear_f64(rgba[1]),
            b: srgb_channel_to_linear_f64(rgba[2]),
            a: f64::from(rgba[3]),
        }
    } else {
        wgpu::Color {
            r: f64::from(rgba[0]),
            g: f64::from(rgba[1]),
            b: f64::from(rgba[2]),
            a: f64::from(rgba[3]),
        }
    }
}

fn fold_line_uniforms(style: &WallStyle) -> ([f32; 4], [f32; 4]) {
    let t = style.line_thickness.max(1e-4);
    let ul_y = style.underline_y.clamp(t * 0.5, 1.0 - t * 0.5);
    let amp = t * style.curly_amplitude;
    let lower = (ul_y - t).max(t * 0.5);
    let line_a = [ul_y, t, style.strike_y, 0.0];
    let line_b = [lower, lower + 2.0 * t, ul_y.max(amp + t * 0.5), amp];
    (line_a, line_b)
}

fn pack_cursor_flags(screen: &Screen) -> u32 {
    let cur = screen.cursor;
    let visible = u32::from(cur.visible);
    let shape = match cur.shape {
        CursorShape::Block => 0u32,
        CursorShape::Bar => 1u32,
        CursorShape::Underline => 2u32,
    };
    let span_cols = if cur.visible && cur.col < screen.cols && cur.row < screen.rows {
        let w = screen.row(cur.row)[cur.col as usize].width;
        if w == 2 { 2u32 } else { 1u32 }
    } else {
        1u32
    };
    visible | (shape << 1) | (span_cols << 4)
}

impl WallRenderer {
    /// Build a `WallRenderer` on `device`/`queue` targeting `color_format` + [`WALL_DEPTH_FORMAT`].
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        atlas: Arc<Atlas>,
        color_format: wgpu::TextureFormat,
        style: WallStyle,
    ) -> Self {
        let atlas_gpu = AtlasGpu::upload(device, queue, &atlas);
        let metrics = CellMetrics::from_primary(atlas.metrics);

        let frame_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wall frame uniform"),
            size: std::mem::size_of::<WallFrameUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let quad_index_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("wall quad index buffer"),
            contents: bytemuck::cast_slice(&[0u16, 1, 2, 0, 2, 3]),
            usage: wgpu::BufferUsages::INDEX,
        });

        let glyphmap_view = atlas_gpu.glyphmap.create_view(&Default::default());
        let curves_view = atlas_gpu.curves.create_view(&Default::default());
        let emoji_view = atlas_gpu.emoji.texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("wall emoji sheet view"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let emoji_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("wall emoji sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });

        let shared_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("wall shared bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
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
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Uint,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Uint,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let shared_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("wall shared bg"),
            layout: &shared_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: frame_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&glyphmap_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&curves_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&emoji_view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::Sampler(&emoji_sampler),
                },
            ],
        });

        let term_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("wall terminal bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
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

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("wall_cells.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/wall_cells.wgsl").into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("wall cells pl"),
            bind_group_layouts: &[Some(&shared_bgl), Some(&term_bgl)],
            immediate_size: 0,
        });

        let make_pipeline = |label: &str, vs: &str, fs: &str, depth_write: bool| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: color_format,
                        blend: Some(PREMUL_BLEND),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: WALL_DEPTH_FORMAT,
                    depth_write_enabled: Some(depth_write),
                    depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };

        let panel_pipeline = make_pipeline("wall panel pipeline", "vs_panel", "fs_panel", true);
        let fill_pipeline = make_pipeline("wall fill pipeline", "vs_fill", "fs_fill", true);
        let ink_pipeline = make_pipeline("wall ink pipeline", "vs_ink", "fs_ink", false);
        let cursor_pipeline = make_pipeline("wall cursor pipeline", "vs_cursor", "fs_cursor", false);

        Self {
            atlas,
            _atlas_gpu: atlas_gpu,
            metrics,
            style,
            color_format,
            frame_buf,
            quad_index_buf,
            shared_bg,
            term_bgl,
            panel_pipeline,
            fill_pipeline,
            ink_pipeline,
            cursor_pipeline,
            terminals: HashMap::new(),
            pack_scratch: Vec::new(),
        }
    }

    pub fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    pub fn style(&self) -> &WallStyle {
        &self.style
    }

    pub fn style_mut(&mut self) -> &mut WallStyle {
        &mut self.style
    }

    fn sync_terminal(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, draw: &TerminalDraw<'_>) {
        let screen = draw.screen;
        let cols = screen.cols;
        let rows = screen.rows;
        let cell_count = usize::from(cols) * usize::from(rows);

        let needs_alloc = match self.terminals.get(&draw.id) {
            Some(existing) => existing.cols != cols || existing.rows != rows,
            None => true,
        };

        if needs_alloc {
            let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("wall terminal uniform"),
                size: std::mem::size_of::<TerminalUniform>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let min_slots = cell_count.max(1);
            let cells_buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("wall terminal cells"),
                size: (min_slots * std::mem::size_of::<CellGpu>()) as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("wall terminal bg"),
                layout: &self.term_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: uniform_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: cells_buf.as_entire_binding(),
                    },
                ],
            });
            self.terminals.insert(
                draw.id,
                TerminalGpu {
                    cols,
                    rows,
                    uploaded_gen: 0,
                    row_has_bg: vec![false; usize::from(rows)],
                    has_any_bg: false,
                    uniform_buf,
                    cells_buf,
                    bind_group,
                },
            );
        }

        let gpu = self
            .terminals
            .get_mut(&draw.id)
            .expect("terminal GPU state just inserted");

        let term_uniform = TerminalUniform {
            origin: draw.position.to_array(),
            z: draw.z,
            focused: u32::from(draw.focused),
            cols: u32::from(cols),
            rows: u32::from(rows),
            default_bg: screen.default_bg,
            panel_opacity: draw.panel_opacity.unwrap_or(self.style.panel_opacity),
            cursor_col: u32::from(screen.cursor.col),
            cursor_row: u32::from(screen.cursor.row),
            cursor_flags: pack_cursor_flags(screen),
            border_override: draw.border_color.unwrap_or(0),
        };
        queue.write_buffer(&gpu.uniform_buf, 0, bytemuck::bytes_of(&term_uniform));

        if cell_count == 0 {
            gpu.uploaded_gen = screen.generation;
            return;
        }

        let upload_all = needs_alloc || screen.generation < gpu.uploaded_gen;
        if upload_all {
            self.pack_scratch.clear();
            self.pack_scratch.reserve(cell_count);
            for r in 0..usize::from(rows) {
                let mut any_bg = false;
                for cell in screen.row(r as u16) {
                    let packed = pack_cell(&self.atlas.trie, screen, cell);
                    if (packed.bg >> 24) != 0 {
                        any_bg = true;
                    }
                    self.pack_scratch.push(packed);
                }
                gpu.row_has_bg[r] = any_bg;
            }
            gpu.has_any_bg = gpu.row_has_bg.iter().any(|&b| b);
            queue.write_buffer(&gpu.cells_buf, 0, bytemuck::cast_slice(&self.pack_scratch));
            gpu.uploaded_gen = screen.generation;
        } else if screen.generation != gpu.uploaded_gen {
            let dirty = dirty_row_ranges(&screen.row_gen, gpu.uploaded_gen);
            let cols_usize = usize::from(cols);
            for range in dirty {
                self.pack_scratch.clear();
                self.pack_scratch.reserve(range.len() * cols_usize);
                for r in range.clone() {
                    let mut any_bg = false;
                    for cell in screen.row(r as u16) {
                        let packed = pack_cell(&self.atlas.trie, screen, cell);
                        if (packed.bg >> 24) != 0 {
                            any_bg = true;
                        }
                        self.pack_scratch.push(packed);
                    }
                    gpu.row_has_bg[r] = any_bg;
                }
                let byte_offset = (range.start * cols_usize * std::mem::size_of::<CellGpu>()) as u64;
                queue.write_buffer(
                    &gpu.cells_buf,
                    byte_offset,
                    bytemuck::cast_slice(&self.pack_scratch),
                );
            }
            gpu.has_any_bg = gpu.row_has_bg.iter().any(|&b| b);
            gpu.uploaded_gen = screen.generation;
        }
    }

    /// Upload any changed terminal rows and record the wall render pass into `encoder`.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &WallTarget<'_>,
        camera: &WallCamera,
        terminals: &[TerminalDraw<'_>],
    ) {
        let (line_a, line_b) = fold_line_uniforms(&self.style);
        let sheet = &self.atlas.emoji.sheet;
        let frame = WallFrameUniform {
            view_proj: camera.view_proj((target.width, target.height)).to_cols_array_2d(),
            viewport: [target.width.max(1) as f32, target.height.max(1) as f32],
            cell_size: [self.metrics.cell_width, self.metrics.cell_height],
            line_a,
            line_b,
            border_focused_color: self.style.border_focused_color,
            border_unfocused_color: self.style.border_unfocused_color,
            cursor_color: self.style.cursor_color,
            emoji_cell: [sheet.cell_w, sheet.cell_h],
            emoji_cols: sheet.cols,
            emoji_rows: sheet.rows_per_layer,
            emoji_layer: [sheet.layer_w as f32, sheet.layer_h as f32],
            greek_px: self.style.greek_px,
            dim_alpha: self.style.dim_alpha,
            panel_padding: self.style.panel_padding,
            panel_border_px: self.style.border_width_px,
            border_focused_scale: self.style.border_focused_scale,
            cursor_fill_alpha: self.style.cursor_fill_opacity,
            cursor_border_px: self.style.cursor_border_px,
            target_is_srgb: u32::from(self.color_format.is_srgb()),
            _pad0: 0,
            _pad1: 0,
        };
        queue.write_buffer(&self.frame_buf, 0, bytemuck::bytes_of(&frame));

        for draw in terminals {
            self.sync_terminal(device, queue, draw);
        }

        // Draw back-to-front by world z so translucent panels, antialiased edges,
        // and translucent cursors blend cleanly over farther terminals while the
        // reverse-Z depth buffer enforces hard 3D occlusion.
        let mut order: Vec<usize> = (0..terminals.len()).collect();
        order.sort_by(|&a, &b| terminals[a].z.total_cmp(&terminals[b].z));

        let clear = srgb_to_wgpu_clear_color(self.style.clear_color, self.color_format.is_srgb());
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("wall cells pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target.color_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(clear),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: target.depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(0.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            ..Default::default()
        });

        pass.set_bind_group(0, &self.shared_bg, &[]);
        pass.set_index_buffer(self.quad_index_buf.slice(..), wgpu::IndexFormat::Uint16);

        for idx in order {
            let draw = &terminals[idx];
            let Some(gpu) = self.terminals.get(&draw.id) else {
                continue;
            };
            let cell_count = u32::from(gpu.cols) * u32::from(gpu.rows);
            pass.set_bind_group(1, &gpu.bind_group, &[]);

            pass.set_pipeline(&self.panel_pipeline);
            pass.draw_indexed(0..6, 0, 0..1);

            if cell_count > 0 {
                if gpu.has_any_bg {
                    pass.set_pipeline(&self.fill_pipeline);
                    pass.draw_indexed(0..6, 0, 0..cell_count);
                }
                pass.set_pipeline(&self.ink_pipeline);
                pass.draw_indexed(0..6, 0, 0..cell_count);
            }

            if draw.screen.cursor.visible
                && draw.screen.cursor.col < draw.screen.cols
                && draw.screen.cursor.row < draw.screen.rows
            {
                pass.set_pipeline(&self.cursor_pipeline);
                pass.draw_indexed(0..6, 0, 0..1);
            }
        }
    }
}

/// Render a wall frame offscreen into a PNG at `path` (reused by `--wall-test-pattern`).
pub fn render_wall_to_png(
    ctx: &GpuContext,
    renderer: &mut WallRenderer,
    camera: &WallCamera,
    terminals: &[TerminalDraw<'_>],
    width: u32,
    height: u32,
    frames: u32,
    path: &Path,
) {
    let device = &ctx.device;
    let format = renderer.color_format;
    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("wall offscreen target"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = texture.create_view(&Default::default());
    let depth_view = crate::scene::create_depth(device, WALL_DEPTH_FORMAT, width, height);

    let unpadded_bpr = width * 4;
    let padded_bpr = unpadded_bpr.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("wall readback"),
        size: u64::from(padded_bpr * height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let frames = frames.max(1);
    let t0 = std::time::Instant::now();
    for frame_idx in 0..frames {
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("wall offscreen frame"),
        });
        let target = WallTarget {
            color_view: &color_view,
            depth_view: &depth_view,
            width,
            height,
        };
        renderer.render(device, &ctx.queue, &mut encoder, &target, camera, terminals);
        if frame_idx == frames - 1 {
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded_bpr),
                        rows_per_image: Some(height),
                    },
                },
                size,
            );
        }
        ctx.queue.submit([encoder.finish()]);
    }

    let slice = readback.slice(..);
    glyph_field::map_read(
        device,
        &slice,
        crate::config::settings().gpu.wait_limit(),
        "wall offscreen readback",
    );
    let data = slice.get_mapped_range().expect("wall readback not mapped");
    let mut pixels = Vec::with_capacity((unpadded_bpr * height) as usize);
    for row in 0..height {
        let start = (row * padded_bpr) as usize;
        pixels.extend_from_slice(&data[start..start + unpadded_bpr as usize]);
    }
    drop(data);
    readback.unmap();

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).expect("create wall screenshot directory");
        }
    }
    image::save_buffer(path, &pixels, width, height, image::ColorType::Rgba8)
        .expect("failed to write wall PNG");

    println!(
        "wall offscreen: {}x{}, {} terminal(s), {} frame(s) in {:.2?}",
        width,
        height,
        terminals.len(),
        frames,
        t0.elapsed(),
    );
    println!("wrote {}", path.display());
}

/// CLI driver for `--wall-test-pattern --screenshot OUT.png`.
pub fn render_test_pattern_offscreen(
    ctx: &GpuContext,
    emoji_sheet: &Path,
    path: &Path,
    frames: u32,
    zoom_multiplier: f32,
) {
    let atlas = Arc::new(Atlas::load(emoji_sheet));
    let mut renderer = WallRenderer::new(
        &ctx.device,
        &ctx.queue,
        atlas,
        wgpu::TextureFormat::Rgba8UnormSrgb,
        WallStyle::default(),
    );
    let screen = super::test_pattern::build_primary_screen();
    let origin = Vec2::ZERO;
    let draw = TerminalDraw {
        id: 1,
        screen: &screen,
        position: origin,
        z: 0.0,
        focused: true,
        panel_opacity: None,
        border_color: None,
    };
    let bounds = renderer
        .metrics()
        .panel_rect(origin, screen.cols, screen.rows, renderer.style().panel_padding);
    let mut camera = WallCamera::frame_rect(
        bounds,
        (crate::OFFSCREEN_WIDTH, crate::OFFSCREEN_HEIGHT),
        1.12,
    );
    if zoom_multiplier > 0.0 && zoom_multiplier.is_finite() {
        camera.zoom *= zoom_multiplier;
    }
    render_wall_to_png(
        ctx,
        &mut renderer,
        &camera,
        &[draw],
        crate::OFFSCREEN_WIDTH,
        crate::OFFSCREEN_HEIGHT,
        frames,
        path,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_sizes_match_wgsl() {
        assert_eq!(std::mem::size_of::<WallFrameUniform>(), 224);
        assert_eq!(std::mem::size_of::<TerminalUniform>(), 48);
    }
}
