//! wgpu cell renderer for the terminal wall (`WallRenderer`).
//!
//! Free of `winit`: draws into whatever colour + depth target views the host
//! provides ([`WallTarget`]). Owns the shared pipelines and uploaded atlas
//! textures (`curves`, `glyphmap`, `emoji`) and caches per-terminal GPU cell
//! buffers so unchanged terminals upload zero bytes.

use std::collections::HashMap;
use std::ops::Range;
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
use super::labels::{Label, LabelGlyphGpu, LabelHeight, measure_label_width, pack_label_glyphs};
use super::metrics::CellMetrics;
use super::paper::{PaperStyle, WallPaperUniform, build_paper_uniform};
use super::shapes::{Shape, ShapeFrameUniform, ShapeGpu, expand_shape};

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

/// Complete description of one wall frame submitted to [`WallRenderer::render`].
#[derive(Default)]
pub struct WallFrame<'a> {
    /// Optional drafting paper (canvas clear colour + discrete minor/major grid).
    /// When `None`, `render` clears to [`WallStyle::clear_color`] without grid lines.
    pub paper: Option<&'a PaperStyle>,
    /// Live or static terminals to draw (sorted internally back-to-front by `z`).
    pub terminals: &'a [TerminalDraw<'a>],
    /// World-space 2D chrome shapes (group frames, title bar strips, selection outlines,
    /// lassos, drop outlines, map label pills and leaders), interleaved by `z`.
    pub shapes: &'a [Shape],
    /// World-space or screen-pixel text labels (title bar names, group headers, map labels),
    /// rendered with the cell pass's Slug shader and interleaved by `z`.
    pub labels: &'a [Label<'a>],
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

/// Per-frame GPU upload counters returned by [`WallRenderer::prepare`] / [`WallRenderer::render`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UploadStats {
    /// Terminals included in the draw list.
    pub terminals_drawn: usize,
    /// Terminals whose cell buffer received at least one row upload this frame.
    pub terminals_uploaded: usize,
    /// Total cell rows uploaded across all terminals this frame.
    pub rows_uploaded: usize,
    /// Total `queue.write_buffer` calls issued for cell buffers (coalesced dirty runs).
    pub write_calls: usize,
}

enum DrawRun {
    ShapesOpaque(Range<u32>),
    ShapesBlend(Range<u32>),
    Terminal(usize),
    Labels(Range<u32>),
}

/// Shared wgpu cell renderer for any number of live terminals.
pub struct WallRenderer {
    atlas: Arc<Atlas>,
    _atlas_gpu: AtlasGpu,
    metrics: CellMetrics,
    style: WallStyle,
    color_format: wgpu::TextureFormat,
    paper_buf: wgpu::Buffer,
    paper_bg: wgpu::BindGroup,
    paper_pipeline: wgpu::RenderPipeline,
    draw_paper: bool,
    shape_frame_buf: wgpu::Buffer,
    shapes_bgl: wgpu::BindGroupLayout,
    shapes_buf: wgpu::Buffer,
    shapes_cap: usize,
    shapes_bg: wgpu::BindGroup,
    shape_opaque_pipeline: wgpu::RenderPipeline,
    shape_blend_pipeline: wgpu::RenderPipeline,
    frame_buf: wgpu::Buffer,
    quad_index_buf: wgpu::Buffer,
    shared_bg: wgpu::BindGroup,
    term_bgl: wgpu::BindGroupLayout,
    labels_bgl: wgpu::BindGroupLayout,
    labels_buf: wgpu::Buffer,
    labels_cap: usize,
    labels_bg: wgpu::BindGroup,
    panel_pipeline: wgpu::RenderPipeline,
    fill_pipeline: wgpu::RenderPipeline,
    ink_pipeline: wgpu::RenderPipeline,
    cursor_pipeline: wgpu::RenderPipeline,
    label_pipeline: wgpu::RenderPipeline,
    terminals: HashMap<u64, TerminalGpu>,
    pack_scratch: Vec<CellGpu>,
    shape_scratch: Vec<ShapeGpu>,
    label_scratch: Vec<LabelGlyphGpu>,
    draw_runs: Vec<DrawRun>,
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

        let paper_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wall paper uniform"),
            size: std::mem::size_of::<WallPaperUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let paper_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("wall paper bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let paper_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("wall paper bg"),
            layout: &paper_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: paper_buf.as_entire_binding(),
            }],
        });
        let paper_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("wall_paper.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/wall_paper.wgsl").into()),
        });
        let paper_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("wall paper pl"),
            bind_group_layouts: &[Some(&paper_bgl)],
            immediate_size: 0,
        });
        let paper_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("wall paper pipeline"),
            layout: Some(&paper_pl),
            vertex: wgpu::VertexState {
                module: &paper_shader,
                entry_point: Some("vs_paper"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &paper_shader,
                entry_point: Some("fs_paper"),
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
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let shape_frame_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wall shape frame uniform"),
            size: std::mem::size_of::<ShapeFrameUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let shapes_cap = 64usize;
        let shapes_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wall shapes instances"),
            size: (shapes_cap * std::mem::size_of::<ShapeGpu>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let shapes_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("wall shapes bgl"),
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
        let shapes_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("wall shapes bg"),
            layout: &shapes_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: shape_frame_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: shapes_buf.as_entire_binding(),
                },
            ],
        });
        let shape_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("wall_shapes.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/wall_shapes.wgsl").into()),
        });
        let shape_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("wall shapes pl"),
            bind_group_layouts: &[Some(&shapes_bgl)],
            immediate_size: 0,
        });
        let make_shape_pipeline = |label: &str, depth_write: bool| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&shape_pl),
                vertex: wgpu::VertexState {
                    module: &shape_shader,
                    entry_point: Some("vs_shape"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shape_shader,
                    entry_point: Some("fs_shape"),
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
        let shape_opaque_pipeline = make_shape_pipeline("wall shape opaque pipeline", true);
        let shape_blend_pipeline = make_shape_pipeline("wall shape blend pipeline", false);

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

        let labels_cap = 256usize;
        let labels_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wall label glyphs"),
            size: (labels_cap * std::mem::size_of::<LabelGlyphGpu>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let labels_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("wall labels bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let labels_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("wall labels bg"),
            layout: &labels_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 2,
                resource: labels_buf.as_entire_binding(),
            }],
        });
        let label_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("wall labels pl"),
            bind_group_layouts: &[Some(&shared_bgl), Some(&labels_bgl)],
            immediate_size: 0,
        });
        let label_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("wall label pipeline"),
            layout: Some(&label_pl),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_label"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_ink"),
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
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        Self {
            atlas,
            _atlas_gpu: atlas_gpu,
            metrics,
            style,
            color_format,
            paper_buf,
            paper_bg,
            paper_pipeline,
            draw_paper: false,
            shape_frame_buf,
            shapes_bgl,
            shapes_buf,
            shapes_cap,
            shapes_bg,
            shape_opaque_pipeline,
            shape_blend_pipeline,
            frame_buf,
            quad_index_buf,
            shared_bg,
            term_bgl,
            labels_bgl,
            labels_buf,
            labels_cap,
            labels_bg,
            panel_pipeline,
            fill_pipeline,
            ink_pipeline,
            cursor_pipeline,
            label_pipeline,
            terminals: HashMap::new(),
            pack_scratch: Vec::new(),
            shape_scratch: Vec::new(),
            label_scratch: Vec::new(),
            draw_runs: Vec::new(),
        }
    }

    pub fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    /// Measure the unclipped world width of `text` drawn at `height` under `zoom`.
    pub fn measure_label_width(&self, text: &str, height: LabelHeight, zoom: f32) -> f32 {
        measure_label_width(self.metrics, &self.atlas.trie, text, height.resolve_world(zoom))
    }

    pub fn style(&self) -> &WallStyle {
        &self.style
    }

    pub fn style_mut(&mut self) -> &mut WallStyle {
        &mut self.style
    }

    /// Release cached GPU buffers for a closed terminal.
    pub fn remove_terminal(&mut self, id: u64) -> bool {
        self.terminals.remove(&id).is_some()
    }

    /// Retain only cached terminal GPU entries matching `keep`, freeing all others.
    pub fn retain_terminals(&mut self, mut keep: impl FnMut(u64) -> bool) {
        self.terminals.retain(|&id, _| keep(id));
    }

    /// Number of terminals currently cached on the GPU.
    pub fn cached_terminal_count(&self) -> usize {
        self.terminals.len()
    }

    fn sync_terminal(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        draw: &TerminalDraw<'_>,
        stats: &mut UploadStats,
    ) {
        let screen = draw.screen;
        let cols = screen.cols;
        let rows = screen.rows;
        let cell_count = usize::from(cols) * usize::from(rows);
        assert_eq!(
            screen.cells.len(),
            cell_count,
            "terminal {}: screen.cells.len() ({}) != cols*rows ({cell_count})",
            draw.id,
            screen.cells.len()
        );
        assert_eq!(
            screen.row_gen.len(),
            usize::from(rows),
            "terminal {}: screen.row_gen.len() ({}) != rows ({rows})",
            draw.id,
            screen.row_gen.len()
        );

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
            stats.terminals_uploaded += 1;
            stats.rows_uploaded += usize::from(rows);
            stats.write_calls += 1;
        } else if screen.generation != gpu.uploaded_gen {
            let dirty = dirty_row_ranges(&screen.row_gen, gpu.uploaded_gen);
            let cols_usize = usize::from(cols);
            if !dirty.is_empty() {
                stats.terminals_uploaded += 1;
            }
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
                stats.rows_uploaded += range.len();
                stats.write_calls += 1;
            }
            gpu.has_any_bg = gpu.row_has_bg.iter().any(|&b| b);
            gpu.uploaded_gen = screen.generation;
        }
    }

    /// Upload frame uniforms, paper parameters, and any dirty terminal rows,
    /// and sort `frame.terminals` back-to-front by `z`. Call before [`Self::record_into_pass`].
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        viewport: (u32, u32),
        camera: &WallCamera,
        frame: &WallFrame<'_>,
    ) -> UploadStats {
        let target_is_srgb = self.color_format.is_srgb();
        if let Some(paper_uniform) = frame
            .paper
            .and_then(|p| build_paper_uniform(p, camera, viewport, target_is_srgb))
        {
            queue.write_buffer(&self.paper_buf, 0, bytemuck::bytes_of(&paper_uniform));
            self.draw_paper = true;
        } else {
            self.draw_paper = false;
        }

        let (line_a, line_b) = fold_line_uniforms(&self.style);
        let sheet = &self.atlas.emoji.sheet;
        let view_proj = camera.view_proj(viewport).to_cols_array_2d();
        let viewport_f = [viewport.0.max(1) as f32, viewport.1.max(1) as f32];
        let frame_uniform = WallFrameUniform {
            view_proj,
            viewport: viewport_f,
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
            target_is_srgb: u32::from(target_is_srgb),
            _pad0: 0,
            _pad1: 0,
        };
        queue.write_buffer(&self.frame_buf, 0, bytemuck::bytes_of(&frame_uniform));

        let terminals = frame.terminals;
        let mut stats = UploadStats {
            terminals_drawn: terminals.len(),
            ..Default::default()
        };
        for draw in terminals {
            self.sync_terminal(device, queue, draw, &mut stats);
        }

        // Expand shapes and labels and interleave with terminals back-to-front by world z.
        // Equal-z tier tie-break: opaque shape (0) -> terminal (1) -> blend shape (2) -> label (3).
        #[derive(Clone, Copy)]
        enum Pending {
            Shape { opaque: bool, raw_idx: usize },
            Terminal(usize),
            Label(usize),
        }
        let mut raw_shapes: Vec<ShapeGpu> = Vec::new();
        let mut raw_labels: Vec<LabelGlyphGpu> = Vec::new();
        let mut pending: Vec<(f32, u8, Pending)> =
            Vec::with_capacity(terminals.len() + frame.shapes.len() + frame.labels.len());
        for shape in frame.shapes {
            expand_shape(shape, camera.zoom, |opaque, gpu| {
                let raw_idx = raw_shapes.len();
                let z = gpu.z;
                raw_shapes.push(gpu);
                pending.push((z, if opaque { 0 } else { 2 }, Pending::Shape { opaque, raw_idx }));
            });
        }
        for (i, draw) in terminals.iter().enumerate() {
            pending.push((draw.z, 1, Pending::Terminal(i)));
        }
        for label in frame.labels {
            pack_label_glyphs(self.metrics, &self.atlas.trie, label, camera.zoom, |gpu| {
                let raw_idx = raw_labels.len();
                let z = gpu.z;
                raw_labels.push(gpu);
                pending.push((z, 3, Pending::Label(raw_idx)));
            });
        }
        pending.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

        self.shape_scratch.clear();
        self.shape_scratch.reserve(raw_shapes.len());
        self.label_scratch.clear();
        self.label_scratch.reserve(raw_labels.len());
        self.draw_runs.clear();
        for (_, _, item) in pending {
            match item {
                Pending::Shape { opaque, raw_idx } => {
                    let gpu_idx = self.shape_scratch.len() as u32;
                    self.shape_scratch.push(raw_shapes[raw_idx]);
                    if opaque {
                        if let Some(DrawRun::ShapesOpaque(r)) = self.draw_runs.last_mut() {
                            if r.end == gpu_idx {
                                r.end = gpu_idx + 1;
                                continue;
                            }
                        }
                        self.draw_runs.push(DrawRun::ShapesOpaque(gpu_idx..gpu_idx + 1));
                    } else {
                        if let Some(DrawRun::ShapesBlend(r)) = self.draw_runs.last_mut() {
                            if r.end == gpu_idx {
                                r.end = gpu_idx + 1;
                                continue;
                            }
                        }
                        self.draw_runs.push(DrawRun::ShapesBlend(gpu_idx..gpu_idx + 1));
                    }
                }
                Pending::Terminal(idx) => {
                    self.draw_runs.push(DrawRun::Terminal(idx));
                }
                Pending::Label(raw_idx) => {
                    let gpu_idx = self.label_scratch.len() as u32;
                    self.label_scratch.push(raw_labels[raw_idx]);
                    if let Some(DrawRun::Labels(r)) = self.draw_runs.last_mut() {
                        if r.end == gpu_idx {
                            r.end = gpu_idx + 1;
                            continue;
                        }
                    }
                    self.draw_runs.push(DrawRun::Labels(gpu_idx..gpu_idx + 1));
                }
            }
        }

        if !self.shape_scratch.is_empty() {
            let shape_frame = ShapeFrameUniform {
                view_proj,
                viewport: viewport_f,
                target_is_srgb: u32::from(target_is_srgb),
                _pad0: 0,
            };
            queue.write_buffer(&self.shape_frame_buf, 0, bytemuck::bytes_of(&shape_frame));
            if self.shape_scratch.len() > self.shapes_cap {
                let new_cap = self.shape_scratch.len().next_power_of_two().max(64);
                self.shapes_buf = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("wall shapes instances"),
                    size: (new_cap * std::mem::size_of::<ShapeGpu>()) as u64,
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                self.shapes_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("wall shapes bg"),
                    layout: &self.shapes_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: self.shape_frame_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: self.shapes_buf.as_entire_binding(),
                        },
                    ],
                });
                self.shapes_cap = new_cap;
            }
            queue.write_buffer(
                &self.shapes_buf,
                0,
                bytemuck::cast_slice(&self.shape_scratch),
            );
        }

        if !self.label_scratch.is_empty() {
            if self.label_scratch.len() > self.labels_cap {
                let new_cap = self.label_scratch.len().next_power_of_two().max(256);
                self.labels_buf = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("wall label glyphs"),
                    size: (new_cap * std::mem::size_of::<LabelGlyphGpu>()) as u64,
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                self.labels_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("wall labels bg"),
                    layout: &self.labels_bgl,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 2,
                        resource: self.labels_buf.as_entire_binding(),
                    }],
                });
                self.labels_cap = new_cap;
            }
            queue.write_buffer(
                &self.labels_buf,
                0,
                bytemuck::cast_slice(&self.label_scratch),
            );
        }
        stats
    }

    /// Record draw calls for the prepared `frame` into an existing render pass.
    /// The pass's colour attachment must match `color_format` and its depth
    /// attachment must be [`WALL_DEPTH_FORMAT`] (reverse-Z, cleared to 0.0).
    pub fn record_into_pass<'a>(
        &'a self,
        pass: &mut wgpu::RenderPass<'a>,
        frame: &WallFrame<'_>,
    ) {
        if self.draw_paper {
            pass.set_pipeline(&self.paper_pipeline);
            pass.set_bind_group(0, &self.paper_bg, &[]);
            pass.draw(0..3, 0..1);
        }

        if self.draw_runs.is_empty() {
            return;
        }

        pass.set_index_buffer(self.quad_index_buf.slice(..), wgpu::IndexFormat::Uint16);

        let terminals = frame.terminals;
        for run in &self.draw_runs {
            match run {
                DrawRun::ShapesOpaque(range) => {
                    pass.set_pipeline(&self.shape_opaque_pipeline);
                    pass.set_bind_group(0, &self.shapes_bg, &[]);
                    pass.draw_indexed(0..6, 0, range.clone());
                }
                DrawRun::ShapesBlend(range) => {
                    pass.set_pipeline(&self.shape_blend_pipeline);
                    pass.set_bind_group(0, &self.shapes_bg, &[]);
                    pass.draw_indexed(0..6, 0, range.clone());
                }
                DrawRun::Terminal(idx) => {
                    let draw = &terminals[*idx];
                    let Some(gpu) = self.terminals.get(&draw.id) else {
                        continue;
                    };
                    let cell_count = u32::from(gpu.cols) * u32::from(gpu.rows);
                    pass.set_bind_group(0, &self.shared_bg, &[]);
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
                DrawRun::Labels(range) => {
                    pass.set_pipeline(&self.label_pipeline);
                    pass.set_bind_group(0, &self.shared_bg, &[]);
                    pass.set_bind_group(1, &self.labels_bg, &[]);
                    pass.draw_indexed(0..6, 0, range.clone());
                }
            }
        }
    }

    /// Upload any changed terminal rows, clear `target` to `paper.background`
    /// (or `style.clear_color` when `frame.paper` is `None`) and depth 0.0,
    /// then draw `frame`.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &WallTarget<'_>,
        camera: &WallCamera,
        frame: &WallFrame<'_>,
    ) -> UploadStats {
        let clear_rgba = frame
            .paper
            .map(|p| p.background)
            .unwrap_or(self.style.clear_color);
        let clear = srgb_to_wgpu_clear_color(clear_rgba, self.color_format.is_srgb());
        self.render_pass_impl(device, queue, encoder, target, camera, frame, Some(clear))
    }

    /// Upload any changed terminal rows and draw `frame` over existing
    /// colour and depth contents (`LoadOp::Load`).
    pub fn render_over(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &WallTarget<'_>,
        camera: &WallCamera,
        frame: &WallFrame<'_>,
    ) -> UploadStats {
        self.render_pass_impl(device, queue, encoder, target, camera, frame, None)
    }

    #[expect(clippy::too_many_arguments, reason = "internal pass helper shared by render and render_over")]
    fn render_pass_impl(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &WallTarget<'_>,
        camera: &WallCamera,
        frame: &WallFrame<'_>,
        clear: Option<wgpu::Color>,
    ) -> UploadStats {
        let stats = self.prepare(device, queue, (target.width, target.height), camera, frame);
        let (color_load, depth_load) = match clear {
            Some(c) => (wgpu::LoadOp::Clear(c), wgpu::LoadOp::Clear(0.0)),
            None => (wgpu::LoadOp::Load, wgpu::LoadOp::Load),
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("wall cells pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target.color_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: color_load,
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: target.depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: depth_load,
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        self.record_into_pass(&mut pass, frame);
        stats
    }
}

/// Render a wall frame offscreen into a PNG at `path` (reused by `--wall-test-pattern`).
pub fn render_wall_to_png(
    ctx: &GpuContext,
    renderer: &mut WallRenderer,
    camera: &WallCamera,
    frame: &WallFrame<'_>,
    viewport: (u32, u32),
    frames: u32,
    path: &Path,
) {
    let (width, height) = viewport;
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
        renderer.render(device, &ctx.queue, &mut encoder, &target, camera, frame);
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
        frame.terminals.len(),
        frames,
        t0.elapsed(),
    );
    println!("wrote {}", path.display());
}

/// CLI driver for `--wall-test-pattern --screenshot OUT.png` (three overlapping terminals).
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
    let placed = super::test_pattern::build_overlapping_test_pattern(renderer.metrics());
    let draws: Vec<TerminalDraw<'_>> = placed
        .iter()
        .map(|t| TerminalDraw {
            id: t.id,
            screen: &t.screen,
            position: t.position,
            z: t.z,
            focused: t.focused,
            panel_opacity: None,
            border_color: t.border_color,
        })
        .collect();

    let pad = renderer.style().panel_padding;
    let metrics = renderer.metrics();
    let bounds = placed
        .iter()
        .map(|t| metrics.panel_rect(t.position, t.screen.cols, t.screen.rows, pad))
        .reduce(|a, b| a.union(b))
        .expect("test pattern has at least one terminal");

    let mut camera = WallCamera::frame_rect(
        bounds,
        (crate::OFFSCREEN_WIDTH, crate::OFFSCREEN_HEIGHT),
        1.08,
    );
    if zoom_multiplier > 0.0 && zoom_multiplier.is_finite() {
        camera.zoom *= zoom_multiplier;
    }
    let paper = PaperStyle::default();
    let frame = WallFrame {
        paper: Some(&paper),
        terminals: &draws,
        ..Default::default()
    };
    render_wall_to_png(
        ctx,
        &mut renderer,
        &camera,
        &frame,
        (crate::OFFSCREEN_WIDTH, crate::OFFSCREEN_HEIGHT),
        frames,
        path,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use term_cells::rgb;

    #[test]
    fn uniform_sizes_match_wgsl() {
        assert_eq!(std::mem::size_of::<WallFrameUniform>(), 224);
        assert_eq!(std::mem::size_of::<TerminalUniform>(), 48);
    }

    fn render_rgba_bytes(
        ctx: &GpuContext,
        renderer: &mut WallRenderer,
        camera: &WallCamera,
        frame: &WallFrame<'_>,
        width: u32,
        height: u32,
    ) -> Vec<u8> {
        let device = &ctx.device;
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("wall test target"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: renderer.color_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let color_view = texture.create_view(&Default::default());
        let depth_view = crate::scene::create_depth(device, WALL_DEPTH_FORMAT, width, height);

        let unpadded_bpr = width * 4;
        let padded_bpr = unpadded_bpr.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wall test readback"),
            size: u64::from(padded_bpr * height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("wall test frame"),
        });
        let target = WallTarget {
            color_view: &color_view,
            depth_view: &depth_view,
            width,
            height,
        };
        let _stats = renderer.render(device, &ctx.queue, &mut encoder, &target, camera, frame);
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
        ctx.queue.submit([encoder.finish()]);

        let slice = readback.slice(..);
        glyph_field::map_read(
            device,
            &slice,
            crate::config::settings().gpu.wait_limit(),
            "wall test readback",
        );
        let data = slice.get_mapped_range().expect("wall test readback mapped");
        let mut pixels = Vec::with_capacity((unpadded_bpr * height) as usize);
        for row in 0..height {
            let start = (row * padded_bpr) as usize;
            pixels.extend_from_slice(&data[start..start + unpadded_bpr as usize]);
        }
        drop(data);
        readback.unmap();
        pixels
    }

    /// Verifies both Risk 4 (exact IEC sRGB round-trip on dark #1a1b26 without
    /// pow(2.2) crush) and reverse-Z depth occlusion (nearer terminal covers
    /// farther terminal regardless of input slice order).
    #[test]
    fn gpu_dark_srgb_round_trip_and_depth_occlusion() {
        let ctx = crate::gpu::test_device::context();
        let atlas = Arc::new(Atlas::load(&crate::default_emoji_sheet()));
        let mut renderer = WallRenderer::new(
            &ctx.device,
            &ctx.queue,
            atlas,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            WallStyle::default(),
        );

        let mut far_screen = Screen::new(10, 4);
        far_screen.default_bg = rgb(0xc8, 0x20, 0x20);
        let mut near_screen = Screen::new(10, 4);
        near_screen.default_bg = rgb(0x1a, 0x1b, 0x26);

        let w = 256u32;
        let h = 128u32;
        let cam = WallCamera::new(Vec2::new(2.5, -2.0), 24.0);

        // Pass near BEFORE far AND far BEFORE near: in both orders, near (z=5) must cover far (z=0).
        for reverse_input in [false, true] {
            let far = TerminalDraw {
                id: 1,
                screen: &far_screen,
                position: Vec2::new(0.0, 0.0),
                z: 0.0,
                focused: false,
                panel_opacity: Some(1.0),
                border_color: None,
            };
            let near = TerminalDraw {
                id: 2,
                screen: &near_screen,
                position: Vec2::new(0.0, 0.0),
                z: 5.0,
                focused: false,
                panel_opacity: Some(1.0),
                border_color: None,
            };
            let list = if reverse_input { [near, far] } else { [far, near] };
            let frame = WallFrame { terminals: &list, ..Default::default() };
            let px = render_rgba_bytes(&ctx, &mut renderer, &cam, &frame, w, h);
            let center_idx = ((h / 2) * w + (w / 2)) as usize * 4;
            let got = [px[center_idx], px[center_idx + 1], px[center_idx + 2]];
            assert!(
                got[0].abs_diff(0x1a) <= 1
                    && got[1].abs_diff(0x1b) <= 1
                    && got[2].abs_diff(0x26) <= 1,
                "expected uncrushed #1a1b26 (26,27,38) from nearer terminal (reverse_input={reverse_input}), got {got:?}"
            );
        }
    }

    /// Verifies generation-gated dirty-row uploads: first frame uploads all
    /// rows, unchanged second frame uploads 0 bytes, editing rows {1,2} and {5}
    /// uploads 3 rows in 2 coalesced writes, and GPU pixels reflect the edit.
    #[test]
    fn gpu_dirty_row_uploads_and_cursor_shapes() {
        let ctx = crate::gpu::test_device::context();
        let atlas = Arc::new(Atlas::load(&crate::default_emoji_sheet()));
        let mut renderer = WallRenderer::new(
            &ctx.device,
            &ctx.queue,
            atlas,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            WallStyle::default(),
        );

        let mut screen = Screen::new(12, 8);
        screen.default_bg = rgb(0x10, 0x10, 0x10);
        fn make_draw(screen: &Screen, focused: bool) -> TerminalDraw<'_> {
            TerminalDraw {
                id: 42,
                screen,
                position: Vec2::ZERO,
                z: 0.0,
                focused,
                panel_opacity: Some(1.0),
                border_color: None,
            }
        }
        let cam = WallCamera::new(Vec2::new(3.0, -4.0), 32.0);
        let vp = (320, 320);

        // Frame 1: initial upload of all 8 rows in 1 write.
        let s1 = renderer.prepare(
            &ctx.device,
            &ctx.queue,
            vp,
            &cam,
            &WallFrame { terminals: &[make_draw(&screen, true)], ..Default::default() },
        );
        assert_eq!(s1, UploadStats { terminals_drawn: 1, terminals_uploaded: 1, rows_uploaded: 8, write_calls: 1 });
        assert_eq!(renderer.cached_terminal_count(), 1);

        // Frame 2: unchanged generation -> zero row uploads.
        let s2 = renderer.prepare(
            &ctx.device,
            &ctx.queue,
            vp,
            &cam,
            &WallFrame { terminals: &[make_draw(&screen, true)], ..Default::default() },
        );
        assert_eq!(s2, UploadStats { terminals_drawn: 1, terminals_uploaded: 0, rows_uploaded: 0, write_calls: 0 });

        // Frame 3: edit rows 1, 2 (contiguous) and row 5 (disjoint) -> 3 rows, 2 writes.
        screen.row_mut(1)[0].bg = rgb(0x00, 0xd0, 0x40);
        screen.touch_row(1);
        screen.row_mut(2)[0].bg = rgb(0x00, 0xd0, 0x40);
        screen.touch_row(2);
        screen.row_mut(5)[0].bg = rgb(0xe0, 0x30, 0x30);
        screen.touch_row(5);
        let s3 = renderer.prepare(
            &ctx.device,
            &ctx.queue,
            vp,
            &cam,
            &WallFrame { terminals: &[make_draw(&screen, true)], ..Default::default() },
        );
        assert_eq!(s3, UploadStats { terminals_drawn: 1, terminals_uploaded: 1, rows_uploaded: 3, write_calls: 2 });

        // Verify all 4 cursor states render into expected cell regions (Block, Bar, Underline, Hollow).
        let cell_w = renderer.metrics().cell_width;
        let cell_h = renderer.metrics().cell_height;
        let cell_cam = WallCamera::new(Vec2::new(cell_w * 0.5, -cell_h * 0.5), 64.0);
        let mut cursor_screen = Screen::new(4, 2);
        cursor_screen.default_bg = rgb(0x00, 0x00, 0x00);
        cursor_screen.cursor = term_cells::Cursor {
            col: 0,
            row: 0,
            visible: true,
            shape: CursorShape::Block,
            blink: false,
        };
        let w = 128u32;
        let h = 128u32;
        let sample_g = |px: &[u8], sx: f32, sy: f32| -> u8 {
            let x = (sx.round() as u32).min(w - 1);
            let y = (sy.round() as u32).min(h - 1);
            px[((y * w + x) * 4 + 1) as usize]
        };
        let center_px = cell_cam.world_to_screen(Vec2::new(cell_w * 0.5, -cell_h * 0.5), (w, h));
        let left_px = cell_cam.world_to_screen(Vec2::new(cell_w * 0.04, -cell_h * 0.5), (w, h));
        let right_px = cell_cam.world_to_screen(Vec2::new(cell_w * 0.85, -cell_h * 0.5), (w, h));
        let bottom_px = cell_cam.world_to_screen(Vec2::new(cell_w * 0.5, -cell_h * 0.96), (w, h));
        let top_mid_px = cell_cam.world_to_screen(Vec2::new(cell_w * 0.5, -cell_h * 0.2), (w, h));

        // 1. Focused Block: interior is lit green.
        let px_block = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &cell_cam,
            &WallFrame { terminals: &[make_draw(&cursor_screen, true)], ..Default::default() },
            w,
            h,
        );
        assert!(sample_g(&px_block, center_px.x, center_px.y) > 80, "focused block interior should be lit");

        // 2. Unfocused Hollow: border is lit green, interior center stays dark.
        let px_hollow = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &cell_cam,
            &WallFrame { terminals: &[make_draw(&cursor_screen, false)], ..Default::default() },
            w,
            h,
        );
        assert!(sample_g(&px_hollow, left_px.x, left_px.y) > 80, "hollow cursor edge should be lit");
        assert!(sample_g(&px_hollow, center_px.x, center_px.y) < 10, "hollow cursor centre must stay dark");

        // 3. Focused Bar: left edge lit, right side dark.
        cursor_screen.cursor.shape = CursorShape::Bar;
        let px_bar = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &cell_cam,
            &WallFrame { terminals: &[make_draw(&cursor_screen, true)], ..Default::default() },
            w,
            h,
        );
        assert!(sample_g(&px_bar, left_px.x, left_px.y) > 100, "bar cursor left edge should be lit");
        assert!(sample_g(&px_bar, right_px.x, right_px.y) < 10, "bar cursor right side must stay dark");

        // 4. Focused Underline: bottom edge lit, upper interior dark.
        cursor_screen.cursor.shape = CursorShape::Underline;
        let px_ul = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &cell_cam,
            &WallFrame { terminals: &[make_draw(&cursor_screen, true)], ..Default::default() },
            w,
            h,
        );
        assert!(sample_g(&px_ul, bottom_px.x, bottom_px.y) > 100, "underline cursor bottom should be lit");
        assert!(sample_g(&px_ul, top_mid_px.x, top_mid_px.y) < 10, "underline cursor top must stay dark");

        assert!(renderer.remove_terminal(42));
        assert_eq!(renderer.cached_terminal_count(), 0);
    }

    /// Verifies drafting paper renders exact sRGB minor/major colours at world
    /// grid crossings and hard-switches minor lines off below `min_px`.
    #[test]
    fn gpu_paper_draws_exact_minor_and_major_lines_and_hard_switches() {
        let ctx = crate::gpu::test_device::context();
        let atlas = Arc::new(Atlas::load(&crate::default_emoji_sheet()));
        let mut renderer = WallRenderer::new(
            &ctx.device,
            &ctx.queue,
            atlas,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            WallStyle::default(),
        );

        let paper = PaperStyle {
            show: true,
            background: crate::wall::srgb_hex(0x07080b, 1.0),
            minor_color: crate::wall::srgb_hex(0x204080, 1.0),
            major_color: crate::wall::srgb_hex(0x90b0d0, 1.0),
            minor_spacing: 4.0,
            major_every: 5,
            line_px: 2.0,
            min_px: 6.0,
        };
        let w = 256u32;
        let h = 256u32;
        let sample = |px: &[u8], cam: &WallCamera, world: Vec2| -> [u8; 3] {
            let s = cam.world_to_screen(world, (w, h));
            let x = (s.x.floor() as u32).min(w - 1);
            let y = (s.y.floor() as u32).min(h - 1);
            let i = ((y * w + x) * 4) as usize;
            [px[i], px[i + 1], px[i + 2]]
        };

        // Zoom 4.0: minor_px = 16 >= 6, major_px = 80 >= 6 -> both levels draw.
        let cam_both = WallCamera::new(Vec2::new(2.0, 2.0), 4.0);
        let px_both = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &cam_both,
            &WallFrame { paper: Some(&paper), ..Default::default() },
            w,
            h,
        );
        let on_major = sample(&px_both, &cam_both, Vec2::new(0.0, 2.0));
        let on_minor = sample(&px_both, &cam_both, Vec2::new(4.0, 2.0));
        let on_bg = sample(&px_both, &cam_both, Vec2::new(2.0, 2.0));
        assert!(
            on_major[0].abs_diff(0x90) <= 1 && on_major[1].abs_diff(0xb0) <= 1 && on_major[2].abs_diff(0xd0) <= 1,
            "expected major line #90b0d0, got {on_major:?}"
        );
        assert!(
            on_minor[0].abs_diff(0x20) <= 1 && on_minor[1].abs_diff(0x40) <= 1 && on_minor[2].abs_diff(0x80) <= 1,
            "expected minor line #204080, got {on_minor:?}"
        );
        assert!(
            on_bg[0].abs_diff(0x07) <= 1 && on_bg[1].abs_diff(0x08) <= 1 && on_bg[2].abs_diff(0x0b) <= 1,
            "expected paper background #07080b, got {on_bg:?}"
        );

        // Zoom 1.0: minor_px = 4 < 6 (hard-switched off!), major_px = 20 >= 6 (still on!).
        let cam_major = WallCamera::new(Vec2::new(2.0, 2.0), 1.0);
        let px_major = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &cam_major,
            &WallFrame { paper: Some(&paper), ..Default::default() },
            w,
            h,
        );
        let on_major2 = sample(&px_major, &cam_major, Vec2::new(0.0, 2.0));
        let on_hidden_minor = sample(&px_major, &cam_major, Vec2::new(4.0, 2.0));
        assert!(
            on_major2[0].abs_diff(0x90) <= 1 && on_major2[1].abs_diff(0xb0) <= 1 && on_major2[2].abs_diff(0xd0) <= 1,
            "expected major line still #90b0d0 at zoom 1.0, got {on_major2:?}"
        );
        assert!(
            on_hidden_minor[0].abs_diff(0x07) <= 1 && on_hidden_minor[1].abs_diff(0x08) <= 1 && on_hidden_minor[2].abs_diff(0x0b) <= 1,
            "minor line must hard-switch off to #07080b (never fade), got {on_hidden_minor:?}"
        );
    }

    /// Verifies world-space shapes: opaque rect behind a terminal is occluded,
    /// dashed rect in front alternates dash/gap over the terminal, polygon fills
    /// and strokes cleanly, and leader draws both its line and anchor dot.
    #[test]
    fn gpu_shapes_rect_dashed_polygon_leader_and_z_order() {
        use super::super::metrics::WorldRect;
        use super::super::shapes::{DashStyle, LeaderShape, PolygonShape, RectShape};

        let ctx = crate::gpu::test_device::context();
        let atlas = Arc::new(Atlas::load(&crate::default_emoji_sheet()));
        let mut renderer = WallRenderer::new(
            &ctx.device,
            &ctx.queue,
            atlas,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            WallStyle::default(),
        );

        let mut term = Screen::new(6, 4);
        term.default_bg = rgb(0x10, 0x30, 0x80);
        let draw = TerminalDraw {
            id: 7,
            screen: &term,
            position: Vec2::new(-1.5, 2.0),
            z: 1.0,
            focused: false,
            panel_opacity: Some(1.0),
            border_color: None,
        };

        let shapes = vec![
            // Opaque yellow backdrop BEHIND terminal (z = 0.0) -> hidden behind terminal, visible outside it.
            Shape::Rect(RectShape {
                rect: WorldRect::new(Vec2::new(-5.0, -4.0), Vec2::new(5.0, 4.0)),
                z: 0.0,
                fill: Some([0.9, 0.8, 0.1, 1.0]),
                stroke: None,
                stroke_px: 0.0,
                dash: DashStyle::SOLID,
                corner_radius_px: 0.0,
            }),
            // Translucent dashed red rect IN FRONT of terminal (z = 5.0).
            Shape::Rect(RectShape {
                rect: WorldRect::new(Vec2::new(-2.0, -2.0), Vec2::new(2.0, 2.0)),
                z: 5.0,
                fill: Some([0.9, 0.1, 0.1, 0.2]),
                stroke: Some([1.0, 0.1, 0.1, 1.0]),
                stroke_px: 4.0,
                dash: DashStyle::new(16.0, 16.0),
                corner_radius_px: 0.0,
            }),
            // Leader line + dot on the right side (z = 5.0).
            Shape::Leader(LeaderShape {
                from: Vec2::new(6.0, 0.0),
                to: Vec2::new(9.0, 0.0),
                z: 5.0,
                color: [0.1, 0.95, 0.4, 1.0],
                width_px: 3.0,
                dot_radius_px: 5.0,
                dash: DashStyle::SOLID,
            }),
            // Closed polygon on the left side (z = 5.0).
            Shape::Polygon(PolygonShape {
                points: vec![
                    Vec2::new(-9.0, -2.0),
                    Vec2::new(-6.0, -2.0),
                    Vec2::new(-6.0, 2.0),
                    Vec2::new(-9.0, 2.0),
                ],
                z: 5.0,
                fill: Some([0.2, 0.6, 1.0, 0.8]),
                stroke: [1.0, 1.0, 1.0, 1.0],
                stroke_px: 2.0,
                dash: DashStyle::SOLID,
            }),
        ];

        let w = 400u32;
        let h = 200u32;
        let cam = WallCamera::new(Vec2::ZERO, 18.0);
        let px = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &cam,
            &WallFrame {
                paper: None,
                terminals: &[draw],
                shapes: &shapes,
                labels: &[],
            },
            w,
            h,
        );
        let sample = |world: Vec2| -> [u8; 3] {
            let s = cam.world_to_screen(world, (w, h));
            let x = (s.x.round() as u32).min(w - 1);
            let y = (s.y.round() as u32).min(h - 1);
            let i = ((y * w + x) * 4) as usize;
            [px[i], px[i + 1], px[i + 2]]
        };

        // Backdrop outside terminal is bright yellow.
        let outside_backdrop = sample(Vec2::new(4.0, 3.0));
        assert!(outside_backdrop[0] > 200 && outside_backdrop[1] > 180 && outside_backdrop[2] < 50,
            "backdrop outside terminal should be yellow, got {outside_backdrop:?}");
        // Inside terminal + translucent red overlay: blue terminal shows through (NOT yellow backdrop!).
        let inside_term = sample(Vec2::new(0.0, 0.0));
        assert!(inside_term[2] > 90 && inside_term[0] > 60 && inside_term[1] < 60,
            "translucent red tint should blend over blue terminal, got {inside_term:?}");

        // Along the top border of the dashed red rect (y = 2.0, 1px inside), we must see BOTH
        // bright red dash pixels AND non-red gap pixels!
        let top_left = cam.world_to_screen(Vec2::new(-2.0, 2.0), (w, h));
        let top_right = cam.world_to_screen(Vec2::new(2.0, 2.0), (w, h));
        let y_row = (top_left.y.round() as u32 + 1).min(h - 1);
        let (mut dashes, mut gaps) = (0usize, 0usize);
        for x in (top_left.x.round() as u32 + 4)..(top_right.x.round() as u32 - 4) {
            let i = ((y_row * w + x) * 4) as usize;
            if px[i] > 230 && px[i + 2] < 45 {
                dashes += 1;
            } else {
                gaps += 1;
            }
        }
        assert!(dashes > 10 && gaps > 10, "dashed border must show both dashes ({dashes}) and gaps ({gaps})");

        // Leader midpoint and anchor dot are bright green.
        let leader_mid = sample(Vec2::new(7.5, 0.0));
        let leader_dot = sample(Vec2::new(9.0, 0.0));
        assert!(leader_mid[1] > 200, "leader line should be green, got {leader_mid:?}");
        assert!(leader_dot[1] > 200, "leader dot should be green, got {leader_dot:?}");

        // Polygon interior is blue.
        let poly_mid = sample(Vec2::new(-7.5, 0.0));
        assert!(poly_mid[2] > 180, "polygon fill should be blue, got {poly_mid:?}");
    }

    /// Verifies non-terminal Slug labels render at world height, stay constant
    /// pixel height under ScreenPx across zoom changes, and clip cleanly to a world rect.
    #[test]
    fn gpu_labels_world_screen_px_and_clip_rect() {
        use super::super::metrics::WorldRect;

        let ctx = crate::gpu::test_device::context();
        let atlas = Arc::new(Atlas::load(&crate::default_emoji_sheet()));
        let mut style = WallStyle::default();
        style.clear_color = [0.0, 0.0, 0.0, 1.0];
        let mut renderer = WallRenderer::new(
            &ctx.device,
            &ctx.queue,
            atlas,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            style,
        );

        let w = 256u32;
        let h = 128u32;
        let count_green_px = |px: &[u8], x_range: Range<u32>| -> usize {
            let mut n = 0usize;
            for y in 0..h {
                for x in x_range.clone() {
                    let i = ((y * w + x) * 4) as usize;
                    if px[i + 1] > 80 {
                        n += 1;
                    }
                }
            }
            n
        };

        // 1. Clipped world label: "MMMMMM" starting at x = 0 clipped to x <= 2.0 (right half stays dark).
        let cam = WallCamera::new(Vec2::new(2.0, -1.0), 32.0);
        let clip_x_px = cam.world_to_screen(Vec2::new(2.0, 0.0), (w, h)).x.round() as u32;
        let clipped = Label::new(
            "MMMMMM",
            Vec2::new(0.0, 0.0),
            LabelHeight::World(2.0),
            [0.2, 1.0, 0.4, 1.0],
            1.0,
        )
        .with_clip(WorldRect::new(Vec2::new(0.0, -2.0), Vec2::new(2.0, 0.0)));
        let px_clip = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &cam,
            &WallFrame { labels: &[clipped], ..Default::default() },
            w,
            h,
        );
        let left_lit = count_green_px(&px_clip, 0..clip_x_px);
        let right_lit = count_green_px(&px_clip, (clip_x_px + 1)..w);
        assert!(left_lit > 100, "clipped label should draw inside clip rect ({left_lit} px)");
        assert_eq!(right_lit, 0, "clipped label must draw zero pixels past clip.max.x");

        // 2. ScreenPx label: same pixel count at zoom 12.0 and zoom 48.0 (constant pixel size).
        let px_label = Label::new(
            "HELLO",
            Vec2::new(0.0, 0.0),
            LabelHeight::ScreenPx(24.0),
            [0.2, 1.0, 0.4, 1.0],
            1.0,
        );
        let px_z12 = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &WallCamera::new(Vec2::new(3.0, -1.0), 12.0),
            &WallFrame { labels: std::slice::from_ref(&px_label), ..Default::default() },
            w,
            h,
        );
        let px_z48 = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &WallCamera::new(Vec2::new(0.75, -0.25), 48.0),
            &WallFrame { labels: &[px_label], ..Default::default() },
            w,
            h,
        );
        let n12 = count_green_px(&px_z12, 0..w);
        let n48 = count_green_px(&px_z48, 0..w);
        assert!(n12 > 100 && n48 > 100, "ScreenPx label should draw at both zooms ({n12}, {n48})");
        let ratio = n12 as f32 / n48 as f32;
        assert!(
            (ratio - 1.0).abs() < 0.08,
            "ScreenPx label size should stay constant across 4x zoom change (n12={n12}, n48={n48})"
        );
    }
}
