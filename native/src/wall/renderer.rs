//! wgpu cell renderer for the terminal wall (`WallRenderer`).
//!
//! Free of `winit`: draws into whatever colour + depth target views the host
//! provides ([`WallTarget`]). Owns the shared pipelines and uploaded atlas
//! textures (`curves`, `glyphmap`, `emoji`) and caches per-terminal GPU cell
//! buffers so unchanged terminals upload zero bytes.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use bytemuck::{Pod, Zeroable};
use glam::Vec2;
use glyph_font::FamilyId;
use term_cells::{CursorShape, Screen};
use wgpu::util::DeviceExt;

use crate::config::WallSettings;
use crate::gpu::GpuContext;

use super::camera::WallCamera;
use super::cell::{
    CellGpu, CellSelection, apply_selection_to_cell, dirty_row_ranges_with_selection,
    pack_cell_resolved,
};
use super::glyphs::GlyphSource;
use super::labels::{Label, LabelGlyphGpu, LabelHeight, pack_label_glyphs};
use super::metrics::{CellMetrics, sanitize_scale};
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
    pub cursor_blink_ms: u32,
    pub greek_px: f32,
    pub underline_y: f32,
    pub strike_y: f32,
    pub line_thickness: f32,
    pub line_thickness_scale: f32,
    pub underline_raise: f32,
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
            cursor_blink_ms: s.cursor_blink_ms,
            greek_px: s.greek_px,
            underline_y: s.underline_y,
            strike_y: s.strike_y,
            line_thickness: s.line_thickness,
            line_thickness_scale: s.line_thickness_scale,
            underline_raise: s.underline_raise,
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
    /// Uniform world-space scale for this terminal (`1.0` = default cell size).
    /// Scales cells, panel padding, fills, style lines, and cursor around `position`.
    pub scale: f32,
    /// Whether this terminal holds keyboard focus (solid cursor + focused border).
    pub focused: bool,
    /// Optional per-terminal panel opacity override (`None` = [`WallStyle::panel_opacity`]).
    pub panel_opacity: Option<f32>,
    /// Optional per-terminal border colour override (packed sRGB RGBA8; `None` = focus/unfocused style).
    pub border_color: Option<u32>,
    /// Optional cell-coordinate text selection highlight (linear stream or column block).
    pub selection: Option<CellSelection>,
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

/// Per-terminal uniform block (`TerminalUniform` in `wall_cells.wgsl`, 64 B).
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
    scale: f32,
    cursor_color: u32,
    _pad1: u32,
    _pad2: u32,
}

struct TerminalGpu {
    cols: u16,
    rows: u16,
    uploaded_gen: u64,
    uploaded_selection: Option<CellSelection>,
    force_repack: bool,
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
    glyphs: GlyphSource,
    metrics: CellMetrics,
    style: WallStyle,
    color_format: wgpu::TextureFormat,
    paper_buf: wgpu::Buffer,
    paper_bg: wgpu::BindGroup,
    paper_pl: wgpu::PipelineLayout,
    paper_pipeline: wgpu::RenderPipeline,
    draw_paper: bool,
    shape_frame_buf: wgpu::Buffer,
    shapes_bgl: wgpu::BindGroupLayout,
    shapes_buf: wgpu::Buffer,
    shapes_cap: usize,
    shapes_bg: wgpu::BindGroup,
    shape_pl: wgpu::PipelineLayout,
    shape_opaque_pipeline: wgpu::RenderPipeline,
    shape_blend_pipeline: wgpu::RenderPipeline,
    frame_buf: wgpu::Buffer,
    quad_index_buf: wgpu::Buffer,
    shared_bgl: wgpu::BindGroupLayout,
    emoji_sampler: wgpu::Sampler,
    shared_bg: wgpu::BindGroup,
    term_bgl: wgpu::BindGroupLayout,
    labels_bgl: wgpu::BindGroupLayout,
    labels_buf: wgpu::Buffer,
    labels_cap: usize,
    labels_bg: wgpu::BindGroup,
    cell_pl: wgpu::PipelineLayout,
    label_pl: wgpu::PipelineLayout,
    panel_pipeline: wgpu::RenderPipeline,
    fill_pipeline: wgpu::RenderPipeline,
    ink_pipeline: wgpu::RenderPipeline,
    cursor_pipeline: wgpu::RenderPipeline,
    label_pipeline: wgpu::RenderPipeline,
    shader_watcher: Option<ShaderWatcher>,
    terminals: HashMap<u64, TerminalGpu>,
    pack_scratch: Vec<CellGpu>,
    shape_scratch: Vec<ShapeGpu>,
    label_scratch: Vec<LabelGlyphGpu>,
    draw_runs: Vec<DrawRun>,
    cursor_blink: CursorBlinkState,
}

const SHADER_POLL_INTERVAL: Duration = Duration::from_millis(150);

const PAPER_ENTRY_POINTS: &[&str] = &["vs_paper", "fs_paper"];
const SHAPE_ENTRY_POINTS: &[&str] = &["vs_shape", "fs_shape"];
const CELL_ENTRY_POINTS: &[&str] = &[
    "vs_panel",
    "fs_panel",
    "vs_fill",
    "fs_fill",
    "vs_ink",
    "fs_ink",
    "vs_cursor",
    "fs_cursor",
    "vs_label",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WatchedShaderKind {
    Paper,
    Shapes,
    Cells,
}

impl WatchedShaderKind {
    const fn file_name(self) -> &'static str {
        match self {
            Self::Paper => "wall_paper.wgsl",
            Self::Shapes => "wall_shapes.wgsl",
            Self::Cells => "wall_cells.wgsl",
        }
    }

    const fn entry_points(self) -> &'static [&'static str] {
        match self {
            Self::Paper => PAPER_ENTRY_POINTS,
            Self::Shapes => SHAPE_ENTRY_POINTS,
            Self::Cells => CELL_ENTRY_POINTS,
        }
    }
}

struct WatchedShader {
    kind: WatchedShaderKind,
    last_stamp: Option<(SystemTime, u64)>,
    last_error: Option<String>,
    notice: Option<String>,
}

struct ShaderWatcher {
    dir: PathBuf,
    poll_interval: Duration,
    next_poll: Instant,
    files: [WatchedShader; 3],
    active_notice: Option<String>,
}

impl ShaderWatcher {
    fn from_dir(dir: PathBuf, now: Instant) -> Self {
        let make = |kind: WatchedShaderKind| {
            let stamp = file_stamp(&dir.join(kind.file_name()));
            WatchedShader {
                kind,
                last_stamp: stamp,
                last_error: None,
                notice: None,
            }
        };
        Self {
            files: [
                make(WatchedShaderKind::Paper),
                make(WatchedShaderKind::Shapes),
                make(WatchedShaderKind::Cells),
            ],
            dir,
            poll_interval: SHADER_POLL_INTERVAL,
            next_poll: now + SHADER_POLL_INTERVAL,
            active_notice: None,
        }
    }

    fn from_env(now: Instant) -> Option<Self> {
        let dir = std::env::var_os("GLYPH_WALL_SHADER_DIR")?;
        if dir.is_empty() {
            return None;
        }
        Some(Self::from_dir(PathBuf::from(dir), now))
    }

    fn refresh_notice(&mut self) -> bool {
        let next = self.files.iter().find_map(|f| f.notice.clone());
        let changed = next != self.active_notice;
        self.active_notice = next;
        changed
    }
}

fn file_stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    Some((mtime, meta.len()))
}

/// Parse and validate WGSL source with `naga` and verify every required entry point exists.
fn validate_wall_wgsl(name: &str, src: &str, required_entries: &[&str]) -> Result<(), String> {
    let module = wgpu::naga::front::wgsl::parse_str(src)
        .map_err(|e| format!("{name}: {}", e.emit_to_string(src)))?;
    let mut validator = wgpu::naga::valid::Validator::new(
        wgpu::naga::valid::ValidationFlags::all(),
        wgpu::naga::valid::Capabilities::all(),
    );
    validator
        .validate(&module)
        .map_err(|e| format!("{name}: {}", e.emit_to_string(src)))?;
    for &entry in required_entries {
        if !module.entry_points.iter().any(|ep| ep.name == entry) {
            return Err(format!("{name}: missing entry point `{entry}`"));
        }
    }
    Ok(())
}

/// First non-empty line of a multi-line error, truncated for the wall's one-line notice bar.
fn compact_shader_error(name: &str, full: &str) -> String {
    let line = full
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("invalid WGSL");
    let stripped = line.strip_prefix(name).and_then(|s| s.strip_prefix(':')).map(str::trim).unwrap_or(line);
    let mut out = format!("shader {name}: {stripped}");
    if out.len() > 140 {
        let mut end = 137;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push_str("...");
    }
    out
}

fn build_paper_pipeline(
    device: &wgpu::Device,
    paper_pl: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    color_format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("wall paper pipeline"),
        layout: Some(paper_pl),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_paper"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
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
    })
}

fn build_shape_pipelines(
    device: &wgpu::Device,
    shape_pl: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    color_format: wgpu::TextureFormat,
) -> (wgpu::RenderPipeline, wgpu::RenderPipeline) {
    let make = |label: &str, depth_write: bool| {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(shape_pl),
            vertex: wgpu::VertexState {
                module: shader,
                entry_point: Some("vs_shape"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: shader,
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
    (
        make("wall shape opaque pipeline", true),
        make("wall shape blend pipeline", false),
    )
}

fn build_cell_pipelines(
    device: &wgpu::Device,
    cell_pl: &wgpu::PipelineLayout,
    label_pl: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    color_format: wgpu::TextureFormat,
) -> (
    wgpu::RenderPipeline,
    wgpu::RenderPipeline,
    wgpu::RenderPipeline,
    wgpu::RenderPipeline,
    wgpu::RenderPipeline,
) {
    let make = |label: &str, vs: &str, fs: &str, depth_write: bool| {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(cell_pl),
            vertex: wgpu::VertexState {
                module: shader,
                entry_point: Some(vs),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: shader,
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
    let panel = make("wall panel pipeline", "vs_panel", "fs_panel", true);
    let fill = make("wall fill pipeline", "vs_fill", "fs_fill", true);
    let ink = make("wall ink pipeline", "vs_ink", "fs_ink", false);
    let cursor = make("wall cursor pipeline", "vs_cursor", "fs_cursor", false);
    let label = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("wall label pipeline"),
        layout: Some(label_pl),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_label"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
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
    (panel, fill, ink, cursor, label)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BlinkTarget {
    terminal_id: u64,
    col: u16,
    row: u16,
    shape: CursorShape,
}

#[derive(Clone, Debug)]
struct CursorBlinkState {
    anchor: Option<Instant>,
    target: Option<BlinkTarget>,
    visible: bool,
}

impl Default for CursorBlinkState {
    fn default() -> Self {
        Self {
            anchor: None,
            target: None,
            visible: true,
        }
    }
}

impl CursorBlinkState {
    fn update(
        &mut self,
        terminals: &[TerminalDraw<'_>],
        half_period_ms: u32,
        now: Instant,
    ) -> Option<Instant> {
        if half_period_ms == 0 {
            self.anchor = None;
            self.target = None;
            self.visible = true;
            return None;
        }

        let target = terminals.iter().find_map(|d| {
            let cur = d.screen.cursor;
            if d.focused
                && cur.visible
                && cur.blink
                && cur.col < d.screen.cols
                && cur.row < d.screen.rows
            {
                Some(BlinkTarget {
                    terminal_id: d.id,
                    col: cur.col,
                    row: cur.row,
                    shape: cur.shape,
                })
            } else {
                None
            }
        });

        let Some(target) = target else {
            self.anchor = None;
            self.target = None;
            self.visible = true;
            return None;
        };

        if self.target != Some(target) {
            self.target = Some(target);
            self.anchor = Some(now);
        }

        let anchor = *self.anchor.get_or_insert(now);
        let period_ns = u128::from(half_period_ms) * 1_000_000u128;
        let elapsed_ns = now.saturating_duration_since(anchor).as_nanos();
        let phase = elapsed_ns / period_ns;
        self.visible = (phase & 1) == 0;
        let next_offset_ns = (phase + 1).saturating_mul(period_ns);
        let next_offset = Duration::new(
            (next_offset_ns / 1_000_000_000u128).min(u128::from(u64::MAX)) as u64,
            (next_offset_ns % 1_000_000_000u128) as u32,
        );
        anchor.checked_add(next_offset)
    }
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

const CURSOR_VISIBLE_FLAG: u32 = 1 << 0;
const CURSOR_SOLID_BLOCK_FLAG: u32 = 1 << 3;

fn fold_line_uniforms(style: &WallStyle) -> ([f32; 4], [f32; 4]) {
    // Matches `glyphMaterial.js::foldLines`: font line thickness × user scale,
    // underline shifted by `underline_raise` and clamped inside `[t/2, 1 - t/2]`.
    let t = (style.line_thickness * style.line_thickness_scale).clamp(1e-4, 1.0);
    let half_t = t * 0.5;
    let ul_y = (style.underline_y + style.underline_raise)
        .max(half_t)
        .min(1.0 - half_t);
    let amp = (t * style.curly_amplitude).max(0.0);
    let lower = (ul_y - t).max(half_t);
    let strike_y = style.strike_y.max(half_t).min(1.0 - half_t);
    let line_a = [ul_y, t, strike_y, 0.0];
    let line_b = [lower, lower + 2.0 * t, ul_y.max(amp + half_t), amp];
    (line_a, line_b)
}

fn cursor_drawn_now(draw: &TerminalDraw<'_>, blink_on: bool) -> bool {
    let cur = draw.screen.cursor;
    cur.visible
        && cur.col < draw.screen.cols
        && cur.row < draw.screen.rows
        && (!draw.focused || !cur.blink || blink_on)
}

fn cursor_is_solid_block(draw: &TerminalDraw<'_>, blink_on: bool) -> bool {
    cursor_drawn_now(draw, blink_on)
        && draw.focused
        && draw.screen.cursor.shape == CursorShape::Block
}

fn pack_cursor_flags(draw: &TerminalDraw<'_>, blink_on: bool) -> u32 {
    let screen = draw.screen;
    let cur = screen.cursor;
    let drawn = cursor_drawn_now(draw, blink_on);
    let solid_block = cursor_is_solid_block(draw, blink_on);
    let shape = match cur.shape {
        CursorShape::Block => 0u32,
        CursorShape::Bar => 1u32,
        CursorShape::Underline => 2u32,
    };
    let span_cols = if drawn {
        let w = screen.row(cur.row)[cur.col as usize].width;
        if w == 2 { 2u32 } else { 1u32 }
    } else {
        1u32
    };
    let mut flags = (shape << 1) | (span_cols << 4);
    if drawn {
        flags |= CURSOR_VISIBLE_FLAG;
    }
    if solid_block {
        flags |= CURSOR_SOLID_BLOCK_FLAG;
    }
    flags
}

/// Load a wall WGSL shader from `GLYPH_WALL_SHADER_DIR/<file>` when set (dev loop),
/// falling back to the compile-time `include_str!` source.
fn load_wall_wgsl(file: &str, embedded: &'static str) -> Cow<'static, str> {
    if let Ok(dir) = std::env::var("GLYPH_WALL_SHADER_DIR") {
        if !dir.is_empty() {
            let path = Path::new(&dir).join(file);
            match std::fs::read_to_string(&path) {
                Ok(src) => {
                    log::info!("wall shader: loaded {} from disk", path.display());
                    return Cow::Owned(src);
                }
                Err(e) => {
                    log::warn!(
                        "GLYPH_WALL_SHADER_DIR: could not read {}: {e}; using embedded shader",
                        path.display()
                    );
                }
            }
        }
    }
    Cow::Borrowed(embedded)
}

fn make_shared_bg(
    device: &wgpu::Device,
    shared_bgl: &wgpu::BindGroupLayout,
    frame_buf: &wgpu::Buffer,
    glyphs: &GlyphSource,
    emoji_sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("wall shared bg"),
        layout: shared_bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: frame_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(glyphs.glyphmap_view()),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::TextureView(glyphs.curves_view()),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(glyphs.emoji_view()),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: wgpu::BindingResource::Sampler(emoji_sampler),
            },
        ],
    })
}

impl WallRenderer {
    /// Build a `WallRenderer` on `device`/`queue` targeting `color_format` + [`WALL_DEPTH_FORMAT`],
    /// loading runtime fonts from `assets/fonts/` and lazy colour emoji from `emoji_sheet`.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        emoji_sheet: &Path,
        color_format: wgpu::TextureFormat,
        mut style: WallStyle,
    ) -> Self {
        let glyphs = GlyphSource::new(device, queue, emoji_sheet);
        let metrics = glyphs.cell_metrics();
        glyphs.apply_line_metrics(&mut style);

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
            source: wgpu::ShaderSource::Wgsl(load_wall_wgsl(
                "wall_paper.wgsl",
                include_str!("../shaders/wall_paper.wgsl"),
            )),
        });
        let paper_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("wall paper pl"),
            bind_group_layouts: &[Some(&paper_bgl)],
            immediate_size: 0,
        });
        let paper_pipeline = build_paper_pipeline(device, &paper_pl, &paper_shader, color_format);

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
            source: wgpu::ShaderSource::Wgsl(load_wall_wgsl(
                "wall_shapes.wgsl",
                include_str!("../shaders/wall_shapes.wgsl"),
            )),
        });
        let shape_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("wall shapes pl"),
            bind_group_layouts: &[Some(&shapes_bgl)],
            immediate_size: 0,
        });
        let (shape_opaque_pipeline, shape_blend_pipeline) =
            build_shape_pipelines(device, &shape_pl, &shape_shader, color_format);

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

        let shared_bg = make_shared_bg(
            device,
            &shared_bgl,
            &frame_buf,
            &glyphs,
            &emoji_sampler,
        );

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
            source: wgpu::ShaderSource::Wgsl(load_wall_wgsl(
                "wall_cells.wgsl",
                include_str!("../shaders/wall_cells.wgsl"),
            )),
        });

        let cell_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("wall cells pl"),
            bind_group_layouts: &[Some(&shared_bgl), Some(&term_bgl)],
            immediate_size: 0,
        });

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
        let (panel_pipeline, fill_pipeline, ink_pipeline, cursor_pipeline, label_pipeline) =
            build_cell_pipelines(device, &cell_pl, &label_pl, &shader, color_format);

        let shader_watcher = ShaderWatcher::from_env(Instant::now());

        Self {
            glyphs,
            metrics,
            style,
            color_format,
            paper_buf,
            paper_bg,
            paper_pl,
            paper_pipeline,
            draw_paper: false,
            shape_frame_buf,
            shapes_bgl,
            shapes_buf,
            shapes_cap,
            shapes_bg,
            shape_pl,
            shape_opaque_pipeline,
            shape_blend_pipeline,
            frame_buf,
            quad_index_buf,
            shared_bgl,
            emoji_sampler,
            shared_bg,
            term_bgl,
            labels_bgl,
            labels_buf,
            labels_cap,
            labels_bg,
            cell_pl,
            label_pl,
            panel_pipeline,
            fill_pipeline,
            ink_pipeline,
            cursor_pipeline,
            label_pipeline,
            shader_watcher,
            terminals: HashMap::new(),
            pack_scratch: Vec::new(),
            shape_scratch: Vec::new(),
            label_scratch: Vec::new(),
            draw_runs: Vec::new(),
            cursor_blink: CursorBlinkState::default(),
        }
    }

    pub fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    /// Underlying runtime [`GlyphSource`] (active font family, Slug atlas, GPU textures).
    pub fn glyphs(&self) -> &GlyphSource {
        &self.glyphs
    }

    /// Mutable access to the runtime [`GlyphSource`].
    pub fn glyphs_mut(&mut self) -> &mut GlyphSource {
        &mut self.glyphs
    }

    /// Active font family used by all terminals and canvas labels (default: Monaspace Neon).
    pub fn font_family(&self) -> FamilyId {
        self.glyphs.family()
    }

    /// Select the active font family by id (`"monaspace-neon"`, `"cousine"`, `"monaspace-argon"`).
    ///
    /// Updates [`Self::metrics`] and [`WallStyle`] line metrics to match the new family and
    /// marks every cached terminal for full re-pack on the next [`Self::prepare`].
    pub fn set_font_family(&mut self, name: &str) -> bool {
        let Some(id) = self.glyphs.family_id(name) else {
            return false;
        };
        self.set_font_family_id(id);
        true
    }

    /// Select the active font family by [`FamilyId`], updating cell/line metrics and
    /// forcing cached terminals to re-pack against the new family's slots.
    pub fn set_font_family_id(&mut self, family: FamilyId) {
        if self.glyphs.family() == family {
            return;
        }
        self.glyphs.set_family_id(family);
        self.metrics = self.glyphs.cell_metrics();
        self.glyphs.apply_line_metrics(&mut self.style);
        for gpu in self.terminals.values_mut() {
            gpu.force_repack = true;
        }
    }

    /// Measure the unclipped world width of `text` drawn at `height` under `zoom`.
    pub fn measure_label_width(&mut self, text: &str, height: LabelHeight, zoom: f32) -> f32 {
        self.glyphs.measure_label_width(text, height.resolve_world(zoom))
    }

    pub fn style(&self) -> &WallStyle {
        &self.style
    }

    pub fn style_mut(&mut self) -> &mut WallStyle {
        &mut self.style
    }

    /// Active one-line shader reload error notice, if the latest disk reload failed validation.
    pub fn shader_error_notice(&self) -> Option<&str> {
        self.shader_watcher
            .as_ref()
            .and_then(|w| w.active_notice.as_deref())
    }

    /// Override the watched shader directory (for tests; production reads `GLYPH_WALL_SHADER_DIR`).
    #[cfg(test)]
    pub(crate) fn set_shader_watch_dir(&mut self, dir: Option<PathBuf>) {
        self.shader_watcher = dir.map(|d| {
            let mut w = ShaderWatcher::from_dir(d, Instant::now());
            w.next_poll = Instant::now();
            w
        });
    }

    /// Poll `GLYPH_WALL_SHADER_DIR` for modified wall WGSL files, validate changed source with
    /// `naga`, rebuild only the affected pipeline(s), and swap them in atomically.
    ///
    /// Returns `(needs_redraw, next_poll_deadline)`. When `GLYPH_WALL_SHADER_DIR` is unset,
    /// returns `(false, None)` immediately without touching the filesystem or scheduling timers.
    pub fn poll_shaders(
        &mut self,
        device: &wgpu::Device,
        now: Instant,
    ) -> (bool, Option<Instant>) {
        let Some(watcher) = self.shader_watcher.as_mut() else {
            return (false, None);
        };
        if now < watcher.next_poll {
            return (false, Some(watcher.next_poll));
        }
        watcher.next_poll = now + watcher.poll_interval;
        let next_deadline = Some(watcher.next_poll);

        let mut dirty = false;
        let dir = watcher.dir.clone();
        let mut to_reload: Vec<(usize, WatchedShaderKind, String)> = Vec::new();

        for (idx, entry) in watcher.files.iter_mut().enumerate() {
            let path = dir.join(entry.kind.file_name());
            let stamp = file_stamp(&path);
            if stamp.is_none() || stamp == entry.last_stamp {
                continue;
            }
            entry.last_stamp = stamp;
            match std::fs::read_to_string(&path) {
                Ok(src) => to_reload.push((idx, entry.kind, src)),
                Err(e) => {
                    let msg = format!("{}: {e}", entry.kind.file_name());
                    if entry.last_error.as_deref() != Some(msg.as_str()) {
                        log::error!("wall shader reload failed: {msg}");
                        entry.last_error = Some(msg.clone());
                    }
                    entry.notice = Some(compact_shader_error(entry.kind.file_name(), &msg));
                }
            }
        }

        for (idx, kind, src) in to_reload {
            let name = kind.file_name();
            let t0 = Instant::now();
            match self.try_reload_shader(device, kind, &src) {
                Ok(()) => {
                    log::info!(
                        "wall shader reloaded: {name} ({:.1} ms)",
                        t0.elapsed().as_secs_f64() * 1e3
                    );
                    if let Some(w) = self.shader_watcher.as_mut() {
                        w.files[idx].last_error = None;
                        w.files[idx].notice = None;
                    }
                    dirty = true;
                }
                Err(err) => {
                    if let Some(w) = self.shader_watcher.as_mut() {
                        if w.files[idx].last_error.as_deref() != Some(err.as_str()) {
                            log::error!("wall shader reload failed ({name}):\n{err}");
                            w.files[idx].last_error = Some(err.clone());
                        }
                        w.files[idx].notice = Some(compact_shader_error(name, &err));
                    }
                }
            }
        }

        if let Some(w) = self.shader_watcher.as_mut() {
            if w.refresh_notice() {
                dirty = true;
            }
        }
        (dirty, next_deadline)
    }

    fn try_reload_shader(
        &mut self,
        device: &wgpu::Device,
        kind: WatchedShaderKind,
        src: &str,
    ) -> Result<(), String> {
        let name = kind.file_name();
        validate_wall_wgsl(name, src, kind.entry_points())?;

        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(name),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(src)),
        });

        match kind {
            WatchedShaderKind::Paper => {
                let paper = build_paper_pipeline(device, &self.paper_pl, &module, self.color_format);
                if let Some(err) = pollster::block_on(scope.pop()) {
                    return Err(format!("{name}: wgpu validation error: {err}"));
                }
                self.paper_pipeline = paper;
            }
            WatchedShaderKind::Shapes => {
                let (opaque, blend) =
                    build_shape_pipelines(device, &self.shape_pl, &module, self.color_format);
                if let Some(err) = pollster::block_on(scope.pop()) {
                    return Err(format!("{name}: wgpu validation error: {err}"));
                }
                self.shape_opaque_pipeline = opaque;
                self.shape_blend_pipeline = blend;
            }
            WatchedShaderKind::Cells => {
                let (panel, fill, ink, cursor, label) = build_cell_pipelines(
                    device,
                    &self.cell_pl,
                    &self.label_pl,
                    &module,
                    self.color_format,
                );
                if let Some(err) = pollster::block_on(scope.pop()) {
                    return Err(format!("{name}: wgpu validation error: {err}"));
                }
                self.panel_pipeline = panel;
                self.fill_pipeline = fill;
                self.ink_pipeline = ink;
                self.cursor_pipeline = cursor;
                self.label_pipeline = label;
            }
        }
        Ok(())
    }

    /// Advance the renderer's cursor-blink phase to `now` for `terminals` and return
    /// the instant of the next phase change, or `None` if no focused visible terminal
    /// has a blinking cursor (or `style.cursor_blink_ms == 0`).
    ///
    /// Call before [`Self::render`] / [`Self::prepare`] in an on-demand event loop and
    /// schedule a redraw at the returned deadline. Unfocused cursors stay hollow and
    /// never blink; moving the cursor, changing its shape, or switching focus resets
    /// the phase solid-on so typing never hides the cursor mid-stroke.
    pub fn next_cursor_blink(
        &mut self,
        terminals: &[TerminalDraw<'_>],
        now: Instant,
    ) -> Option<Instant> {
        self.cursor_blink
            .update(terminals, self.style.cursor_blink_ms, now)
    }

    /// Whether a blinking focused cursor is currently in its visible (lit) phase.
    pub fn cursor_blink_visible(&self) -> bool {
        self.cursor_blink.visible
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
                    uploaded_selection: None,
                    force_repack: false,
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
            cursor_flags: pack_cursor_flags(draw, self.cursor_blink.visible),
            border_override: draw.border_color.unwrap_or(0),
            scale: sanitize_scale(draw.scale),
            cursor_color: screen.cursor_color,
            _pad1: 0,
            _pad2: 0,
        };
        queue.write_buffer(&gpu.uniform_buf, 0, bytemuck::bytes_of(&term_uniform));

        let active_selection = draw.selection.filter(|s| !s.is_empty());

        if cell_count == 0 {
            gpu.uploaded_gen = screen.generation;
            gpu.uploaded_selection = active_selection;
            return;
        }

        let upload_all = needs_alloc || gpu.force_repack || screen.generation < gpu.uploaded_gen;
        if upload_all {
            self.pack_scratch.clear();
            self.pack_scratch.reserve(cell_count);
            for r in 0..usize::from(rows) {
                let mut any_bg = false;
                let sel_span = active_selection.map_or(0..0, |s| s.row_span(r as u16, cols));
                for (c, cell) in screen.row(r as u16).iter().enumerate() {
                    let mut packed =
                        pack_cell_resolved(self.glyphs.resolve_cell(screen, cell), screen, cell);
                    if let Some(sel) = active_selection {
                        let col_u16 = c as u16;
                        if col_u16 >= sel_span.start && col_u16 < sel_span.end {
                            apply_selection_to_cell(&mut packed, &sel);
                        }
                    }
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
            gpu.uploaded_selection = active_selection;
            gpu.force_repack = false;
            stats.terminals_uploaded += 1;
            stats.rows_uploaded += usize::from(rows);
            stats.write_calls += 1;
        } else if screen.generation != gpu.uploaded_gen || active_selection != gpu.uploaded_selection
        {
            let dirty = dirty_row_ranges_with_selection(
                &screen.row_gen,
                cols,
                screen.generation,
                gpu.uploaded_gen,
                gpu.uploaded_selection,
                active_selection,
            );
            let cols_usize = usize::from(cols);
            if !dirty.is_empty() {
                stats.terminals_uploaded += 1;
            }
            for range in dirty {
                self.pack_scratch.clear();
                self.pack_scratch.reserve(range.len() * cols_usize);
                for r in range.clone() {
                    let mut any_bg = false;
                    let sel_span = active_selection.map_or(0..0, |s| s.row_span(r as u16, cols));
                    for (c, cell) in screen.row(r as u16).iter().enumerate() {
                        let mut packed = pack_cell_resolved(
                            self.glyphs.resolve_cell(screen, cell),
                            screen,
                            cell,
                        );
                        if let Some(sel) = active_selection {
                            let col_u16 = c as u16;
                            if col_u16 >= sel_span.start && col_u16 < sel_span.end {
                                apply_selection_to_cell(&mut packed, &sel);
                            }
                        }
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
            gpu.uploaded_selection = active_selection;
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

        let view_proj = camera.view_proj(viewport).to_cols_array_2d();
        let viewport_f = [viewport.0.max(1) as f32, viewport.1.max(1) as f32];

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
        let label_family = self.glyphs.family();
        for label in frame.labels {
            pack_label_glyphs(self.metrics, self.glyphs.atlas_mut(), label_family, label, camera.zoom, |gpu| {
                let raw_idx = raw_labels.len();
                let z = gpu.z;
                raw_labels.push(gpu);
                pending.push((z, 3, Pending::Label(raw_idx)));
            });
        }
        // Flush any newly encoded cell, label, or sparse emoji glyphs to the GPU textures and
        // rebuild the shared bind group if any backing texture grew.
        let glyph_sync = self.glyphs.sync(device, queue);
        if glyph_sync.rebound {
            self.shared_bg = make_shared_bg(
                device,
                &self.shared_bgl,
                &self.frame_buf,
                &self.glyphs,
                &self.emoji_sampler,
            );
        }

        // Write frame uniforms AFTER `glyphs.sync` so `emoji_cols`/`emoji_rows`/`emoji_layer`
        // match the live sparse emoji texture after any growth this frame.
        let (line_a, line_b) = fold_line_uniforms(&self.style);
        let emoji = self.glyphs.emoji_layout();
        let frame_uniform = WallFrameUniform {
            view_proj,
            viewport: viewport_f,
            cell_size: [self.metrics.cell_width, self.metrics.cell_height],
            line_a,
            line_b,
            border_focused_color: self.style.border_focused_color,
            border_unfocused_color: self.style.border_unfocused_color,
            cursor_color: self.style.cursor_color,
            emoji_cell: [emoji.cell_w, emoji.cell_h],
            emoji_cols: emoji.cols,
            emoji_rows: emoji.rows,
            emoji_layer: [emoji.layer_w as f32, emoji.layer_h as f32],
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

                    let cursor_drawn = cursor_drawn_now(draw, self.cursor_blink.visible);
                    let cursor_solid = cursor_is_solid_block(draw, self.cursor_blink.visible);

                    pass.set_pipeline(&self.panel_pipeline);
                    pass.draw_indexed(0..6, 0, 0..1);

                    if cell_count > 0 && gpu.has_any_bg {
                        pass.set_pipeline(&self.fill_pipeline);
                        pass.draw_indexed(0..6, 0, 0..cell_count);
                    }

                    // Focused block cursor draws solid behind ink so the glyph renders
                    // over it in the cell's background colour without washout.
                    if cursor_solid {
                        pass.set_pipeline(&self.cursor_pipeline);
                        pass.draw_indexed(0..6, 0, 0..1);
                    }

                    if cell_count > 0 {
                        pass.set_pipeline(&self.ink_pipeline);
                        pass.draw_indexed(0..6, 0, 0..cell_count);
                    }

                    // Hollow unfocused, bar, and underline cursors draw above ink
                    // without modifying the glyph underneath.
                    if cursor_drawn && !cursor_solid {
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
    render_wall_to_png_with_overlay(ctx, renderer, camera, frame, viewport, frames, path, |_, _| {});
}

/// Render a wall frame offscreen into a PNG at `path`, invoking `overlay` after
/// the wall's render pass on the final frame (e.g. to paint `WallUi` over the scene).
#[expect(clippy::too_many_arguments, reason = "offscreen capture helper extending render_wall_to_png with an overlay hook")]
pub fn render_wall_to_png_with_overlay(
    ctx: &GpuContext,
    renderer: &mut WallRenderer,
    camera: &WallCamera,
    frame: &WallFrame<'_>,
    viewport: (u32, u32),
    frames: u32,
    path: &Path,
    mut overlay: impl FnMut(&mut wgpu::CommandEncoder, &wgpu::TextureView),
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
            overlay(&mut encoder, &color_view);
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

/// CLI driver for `--wall-test-pattern --screenshot OUT.png` (three terminals + full D3 chrome).
pub fn render_test_pattern_offscreen(
    ctx: &GpuContext,
    emoji_sheet: &Path,
    path: &Path,
    frames: u32,
    zoom_multiplier: f32,
) {
    let mut renderer = WallRenderer::new(
        &ctx.device,
        &ctx.queue,
        emoji_sheet,
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
            scale: t.scale,
            focused: t.focused,
            panel_opacity: None,
            border_color: t.border_color,
            selection: t.selection,
        })
        .collect();

    let pad = renderer.style().panel_padding;
    let metrics = renderer.metrics();
    let infos: Vec<super::test_pattern::ChromeTerminalInfo<'_>> = placed
        .iter()
        .map(|t| super::test_pattern::ChromeTerminalInfo {
            id: t.id,
            title: &t.title,
            cols: t.screen.cols,
            rows: t.screen.rows,
            position: t.position,
            z: t.z,
            scale: t.scale,
            focused: t.focused,
        })
        .collect();

    let vp = (crate::OFFSCREEN_WIDTH, crate::OFFSCREEN_HEIGHT);
    let family = renderer.font_family();
    // Two-pass framing: first estimate zoom from rough bounds, then build chrome at that
    // zoom and frame the exact union of terminals + group header + lasso + drop + map label.
    let probe = super::test_pattern::build_test_pattern_chrome(
        metrics,
        renderer.glyphs_mut().atlas_mut(),
        family,
        pad,
        20.0,
        &infos,
    );
    let mut camera = WallCamera::frame_rect(probe.chrome_bounds, vp, 1.08);
    let fitted = super::test_pattern::build_test_pattern_chrome(
        metrics,
        renderer.glyphs_mut().atlas_mut(),
        family,
        pad,
        camera.zoom,
        &infos,
    );
    camera = WallCamera::frame_rect(fitted.chrome_bounds, vp, 1.08);
    if zoom_multiplier > 0.0 && zoom_multiplier.is_finite() {
        camera.zoom *= zoom_multiplier;
    }
    let chrome = super::test_pattern::build_test_pattern_chrome(
        metrics,
        renderer.glyphs_mut().atlas_mut(),
        family,
        pad,
        camera.zoom,
        &infos,
    );
    let frame = WallFrame {
        paper: Some(&chrome.paper),
        terminals: &draws,
        shapes: &chrome.shapes,
        labels: &chrome.labels,
    };
    render_wall_to_png(ctx, &mut renderer, &camera, &frame, vp, frames, path);
}

/// CLI driver for `--wall-chrome-pattern --screenshot OUT.png` (exercises every
/// builder and state in [`super::chrome`]).
pub fn render_chrome_pattern_offscreen(
    ctx: &GpuContext,
    emoji_sheet: &Path,
    path: &Path,
    frames: u32,
    zoom_multiplier: f32,
) {
    let mut renderer = WallRenderer::new(
        &ctx.device,
        &ctx.queue,
        emoji_sheet,
        wgpu::TextureFormat::Rgba8UnormSrgb,
        WallStyle::default(),
    );
    let placed = super::chrome::build_chrome_pattern_terminals(renderer.metrics());
    let draws: Vec<TerminalDraw<'_>> = placed
        .iter()
        .map(|t| TerminalDraw {
            id: t.id,
            screen: &t.screen,
            position: t.position,
            z: t.z,
            scale: t.scale,
            focused: t.focused,
            panel_opacity: None,
            border_color: t.border_color,
            selection: t.selection,
        })
        .collect();

    let pad = renderer.style().panel_padding;
    let metrics = renderer.metrics();
    let vp = (crate::OFFSCREEN_WIDTH, crate::OFFSCREEN_HEIGHT);
    let family = renderer.font_family();

    let probe_cam = WallCamera::new(Vec2::ZERO, 20.0);
    let probe = {
        let mut fonts =
            super::chrome::ChromeFonts::new(metrics, renderer.glyphs_mut().atlas_mut(), family);
        super::chrome::build_chrome_pattern_scene(&mut fonts, pad, probe_cam, vp, 1.0, &placed)
    };
    let mut camera = WallCamera::frame_rect(probe.chrome_bounds, vp, 1.06);
    let fitted = {
        let mut fonts =
            super::chrome::ChromeFonts::new(metrics, renderer.glyphs_mut().atlas_mut(), family);
        super::chrome::build_chrome_pattern_scene(&mut fonts, pad, camera, vp, 1.0, &placed)
    };
    camera = WallCamera::frame_rect(fitted.chrome_bounds, vp, 1.06);
    if zoom_multiplier > 0.0 && zoom_multiplier.is_finite() {
        camera.zoom =
            (camera.zoom * zoom_multiplier).clamp(super::camera::MIN_ZOOM, super::camera::MAX_ZOOM);
    }
    let chrome = {
        let mut fonts =
            super::chrome::ChromeFonts::new(metrics, renderer.glyphs_mut().atlas_mut(), family);
        super::chrome::build_chrome_pattern_scene(&mut fonts, pad, camera, vp, 1.0, &placed)
    };
    let frame = WallFrame {
        paper: Some(&chrome.paper),
        terminals: &draws,
        shapes: &chrome.shapes,
        labels: &chrome.labels,
    };
    render_wall_to_png(ctx, &mut renderer, &camera, &frame, vp, frames, path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use term_cells::rgb;

    #[test]
    fn uniform_sizes_match_wgsl() {
        assert_eq!(std::mem::size_of::<WallFrameUniform>(), 224);
        assert_eq!(std::mem::size_of::<TerminalUniform>(), 64);
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
        let mut renderer = WallRenderer::new(
            &ctx.device,
            &ctx.queue,
            &crate::default_emoji_sheet(),
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
                scale: 1.0,
                focused: false,
                panel_opacity: Some(1.0),
                border_color: None,
                selection: None,
            };
            let near = TerminalDraw {
                id: 2,
                screen: &near_screen,
                position: Vec2::new(0.0, 0.0),
                z: 5.0,
                scale: 1.0,
                focused: false,
                panel_opacity: Some(1.0),
                border_color: None,
                selection: None,
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
        let mut renderer = WallRenderer::new(
            &ctx.device,
            &ctx.queue,
            &crate::default_emoji_sheet(),
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
                scale: 1.0,
                focused,
                panel_opacity: Some(1.0),
                border_color: None,
                selection: None,
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
        let left_px = cell_cam.world_to_screen(Vec2::new(cell_w * 0.02, -cell_h * 0.5), (w, h));
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
        let mut renderer = WallRenderer::new(
            &ctx.device,
            &ctx.queue,
            &crate::default_emoji_sheet(),
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
        let mut renderer = WallRenderer::new(
            &ctx.device,
            &ctx.queue,
            &crate::default_emoji_sheet(),
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
            scale: 1.0,
            focused: false,
            panel_opacity: Some(1.0),
            border_color: None,
            selection: None,
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
        let style = WallStyle {
            clear_color: [0.0, 0.0, 0.0, 1.0],
            ..WallStyle::default()
        };
        let mut renderer = WallRenderer::new(
            &ctx.device,
            &ctx.queue,
            &crate::default_emoji_sheet(),
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

    /// Verifies `TerminalDraw::scale` scales panel, fills, ink, and cursor together.
    #[test]
    fn gpu_terminal_scale_scales_panel_fills_ink_and_cursor() {
        let ctx = crate::gpu::test_device::context();
        let style = WallStyle {
            clear_color: [0.0, 0.0, 0.0, 1.0],
            ..WallStyle::default()
        };
        let mut renderer = WallRenderer::new(
            &ctx.device,
            &ctx.queue,
            &crate::default_emoji_sheet(),
            wgpu::TextureFormat::Rgba8UnormSrgb,
            style,
        );

        let mut screen = Screen::new(10, 6);
        screen.default_bg = rgb(0x18, 0x20, 0x30);
        // Bright red fill at cell (4, 2) and cursor at cell (1, 1).
        screen.row_mut(2)[4].bg = rgb(0xe0, 0x20, 0x20);
        screen.touch_row(2);
        screen.cursor = term_cells::Cursor {
            col: 1,
            row: 1,
            visible: true,
            shape: CursorShape::Block,
            blink: false,
        };

        let origin = Vec2::ZERO;
        let half = TerminalDraw {
            id: 99,
            screen: &screen,
            position: origin,
            z: 1.0,
            scale: 0.5,
            focused: true,
            panel_opacity: Some(1.0),
            border_color: None,
            selection: None,
        };

        let w = 256u32;
        let h = 256u32;
        let cam = WallCamera::new(Vec2::new(2.5, -3.0), 32.0);
        let px = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &cam,
            &WallFrame { terminals: &[half], ..Default::default() },
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

        let m_half = renderer.metrics().scaled(0.5);
        let m_full = renderer.metrics();

        // Red fill at scaled cell (4, 2) must be bright red; unscaled (8, 4) lies outside the 0.5x panel!
        let fill_half = sample(m_half.cell_rect(origin, 4, 2, 1).center());
        assert!(fill_half[0] > 200 && fill_half[2] < 50, "0.5x fill should be red, got {fill_half:?}");
        let outside_half = sample(m_full.cell_rect(origin, 8, 4, 1).center());
        assert_eq!(outside_half, [0, 0, 0], "outside 0.5x panel must stay black, got {outside_half:?}");

        // Green focused block cursor sits at scaled cell (1, 1).
        let cursor_half = sample(m_half.cell_rect(origin, 1, 1, 1).center());
        assert!(cursor_half[1] > 120, "0.5x focused block cursor should light green, got {cursor_half:?}");
    }

    /// Verifies runtime font faces (regular/bold/italic/bold-italic draw distinct
    /// pixels), cell sprites render, and switching font family updates cell/line
    /// metrics and re-packs cached terminals.
    #[test]
    fn gpu_runtime_faces_and_family_switching() {
        use glyph_font::{DEFAULT_FONT_FAMILY, MONASPACE_ARGON_FAMILY, MONASPACE_NEON_FAMILY};
        use term_cells::attrs;

        let ctx = crate::gpu::test_device::context();
        let style = WallStyle {
            clear_color: [0.0, 0.0, 0.0, 1.0],
            ..WallStyle::default()
        };
        let mut renderer = WallRenderer::new(
            &ctx.device,
            &ctx.queue,
            &crate::default_emoji_sheet(),
            wgpu::TextureFormat::Rgba8UnormSrgb,
            style,
        );

        let neon_metrics = renderer.metrics();
        assert_eq!(
            renderer.font_family(),
            renderer.glyphs().family_id(MONASPACE_NEON_FAMILY).expect("neon")
        );

        let render_char = |renderer: &mut WallRenderer, ch: char, cell_attrs: u16| -> Vec<u8> {
            let mut s = Screen::new(2, 1);
            s.default_bg = rgb(0x00, 0x00, 0x00);
            s.row_mut(0)[0] = term_cells::Cell {
                content: ch as u32,
                fg: rgb(0xff, 0xff, 0xff),
                bg: 0,
                ul: 0,
                attrs: cell_attrs,
                width: 1,
                _pad: 0,
            };
            s.touch_row(0);
            let m = renderer.metrics();
            let cam = WallCamera::new(Vec2::new(m.cell_width * 0.5, -m.cell_height * 0.5), 64.0);
            let draw = TerminalDraw {
                id: 500 + u64::from(cell_attrs),
                screen: &s,
                position: Vec2::ZERO,
                z: 1.0,
                scale: 1.0,
                focused: false,
                panel_opacity: Some(1.0),
                border_color: None,
                selection: None,
            };
            render_rgba_bytes(
                &ctx,
                renderer,
                &cam,
                &WallFrame { terminals: &[draw], ..Default::default() },
                64,
                96,
            )
        };

        let px_reg = render_char(&mut renderer, 'R', 0);
        let px_bold = render_char(&mut renderer, 'R', attrs::BOLD);
        let px_ital = render_char(&mut renderer, 'R', attrs::ITALIC);
        let px_bi = render_char(&mut renderer, 'R', attrs::BOLD | attrs::ITALIC);
        let lit = |px: &[u8]| px.as_chunks::<4>().0.iter().filter(|p| p[0] > 80).count();
        assert!(lit(&px_reg) > 50, "regular glyph should draw");
        assert!(lit(&px_bold) > lit(&px_reg), "bold should have more lit ink pixels than regular");
        assert_ne!(px_reg, px_ital, "italic face must differ from regular");
        assert_ne!(px_bold, px_bi, "bold-italic face must differ from bold");

        // Switch family to Cousine: metrics match baked Cousine and cached terminal re-packs.
        let mut static_screen = Screen::new(4, 1);
        static_screen.row_mut(0)[0] = term_cells::Cell {
            content: 'M' as u32,
            fg: rgb(0xff, 0xff, 0xff),
            bg: 0,
            ul: 0,
            attrs: 0,
            width: 1,
            _pad: 0,
        };
        let draw_cached = TerminalDraw {
            id: 777,
            screen: &static_screen,
            position: Vec2::ZERO,
            z: 1.0,
            scale: 1.0,
            focused: false,
            panel_opacity: Some(1.0),
            border_color: None,
            selection: None,
        };
        let cam = WallCamera::new(Vec2::new(1.0, -0.5), 48.0);
        let s_before = renderer.prepare(
            &ctx.device,
            &ctx.queue,
            (128, 128),
            &cam,
            &WallFrame { terminals: &[draw_cached], ..Default::default() },
        );
        assert_eq!(s_before.rows_uploaded, 1);

        assert!(renderer.set_font_family(DEFAULT_FONT_FAMILY));
        assert_eq!(
            renderer.metrics(),
            CellMetrics::from_primary(crate::atlas::default_trie().metrics)
        );
        assert_ne!(renderer.metrics().cell_width, neon_metrics.cell_width);

        // Even with static_screen.generation == 0 unchanged, family switch forces re-pack.
        let draw_cached2 = TerminalDraw {
            id: 777,
            screen: &static_screen,
            position: Vec2::ZERO,
            z: 1.0,
            scale: 1.0,
            focused: false,
            panel_opacity: Some(1.0),
            border_color: None,
            selection: None,
        };
        let s_after = renderer.prepare(
            &ctx.device,
            &ctx.queue,
            (128, 128),
            &cam,
            &WallFrame { terminals: &[draw_cached2], ..Default::default() },
        );
        assert_eq!(s_after.rows_uploaded, 1, "family switch must repack cached terminals");
        assert!(renderer.set_font_family(MONASPACE_ARGON_FAMILY));
        assert!(!renderer.set_font_family("nonexistent-family"));
    }

    #[test]
    fn fold_line_uniforms_matches_js_rules() {
        let mut style = WallStyle {
            underline_y: 0.1127,
            strike_y: 0.38,
            line_thickness: 0.0314,
            line_thickness_scale: 1.0,
            underline_raise: 0.0,
            curly_amplitude: 1.5,
            ..WallStyle::default()
        };
        let (a, b) = fold_line_uniforms(&style);
        assert!((a[0] - 0.1127).abs() < 1e-6);
        assert!((a[1] - 0.0314).abs() < 1e-6);
        assert!((a[2] - 0.38).abs() < 1e-6);
        let expected_amp = 0.0314 * 1.5;
        let expected_lower = 0.1127 - 0.0314;
        assert!((b[0] - expected_lower).abs() < 1e-6);
        assert!((b[1] - (expected_lower + 2.0 * 0.0314)).abs() < 1e-6);
        assert!((b[2] - 0.1127_f32.max(expected_amp + 0.0314 * 0.5)).abs() < 1e-6);
        assert!((b[3] - expected_amp).abs() < 1e-6);

        // User knobs scale font thickness and raise underline cleanly.
        style.line_thickness_scale = 2.0;
        style.underline_raise = 0.05;
        let (a2, b2) = fold_line_uniforms(&style);
        assert!((a2[1] - 0.0628).abs() < 1e-6);
        assert!((a2[0] - 0.1627).abs() < 1e-6);
        assert!((b2[3] - 0.0628 * 1.5).abs() < 1e-6);

        // Zero curly_amplitude flattens wave amplitude to 0.
        style.curly_amplitude = 0.0;
        let (a3, b3) = fold_line_uniforms(&style);
        assert_eq!(b3[3], 0.0);
        assert!((b3[2] - a3[0]).abs() < 1e-6);

        // Extreme scale and raises clamp safely without panicking.
        style.line_thickness_scale = 100.0;
        style.underline_raise = -10.0;
        let (a_lo, _) = fold_line_uniforms(&style);
        assert_eq!(a_lo[1], 1.0);
        assert!((a_lo[0] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn cursor_blink_state_phases_reset_and_idle() {
        let mut screen = Screen::new(10, 4);
        screen.cursor = term_cells::Cursor {
            col: 2,
            row: 1,
            visible: true,
            shape: CursorShape::Block,
            blink: false,
        };
        fn make(id: u64, s: &Screen, focused: bool) -> TerminalDraw<'_> {
            TerminalDraw {
                id,
                screen: s,
                position: Vec2::ZERO,
                z: 0.0,
                scale: 1.0,
                focused,
                panel_opacity: None,
                border_color: None,
                selection: None,
            }
        }

        let mut blink = CursorBlinkState::default();
        let t0 = Instant::now();

        // Non-blinking focused cursor needs no timer and stays visible.
        assert_eq!(blink.update(&[make(1, &screen, true)], 530, t0), None);
        assert!(blink.visible);

        // Unfocused blinking cursor stays hollow and never schedules a timer.
        screen.cursor.blink = true;
        assert_eq!(blink.update(&[make(1, &screen, false)], 530, t0), None);
        assert!(blink.visible);

        // Disabled blink (half_period_ms = 0) never schedules a timer.
        assert_eq!(blink.update(&[make(1, &screen, true)], 0, t0), None);
        assert!(blink.visible);

        // Focused blinking cursor alternates ON / OFF every 530 ms.
        let d0 = blink.update(&[make(1, &screen, true)], 530, t0);
        assert_eq!(d0, Some(t0 + Duration::from_millis(530)));
        assert!(blink.visible);

        let d1 = blink.update(&[make(1, &screen, true)], 530, t0 + Duration::from_millis(529));
        assert_eq!(d1, Some(t0 + Duration::from_millis(530)));
        assert!(blink.visible);

        let d2 = blink.update(&[make(1, &screen, true)], 530, t0 + Duration::from_millis(530));
        assert_eq!(d2, Some(t0 + Duration::from_millis(1060)));
        assert!(!blink.visible);

        let d3 = blink.update(&[make(1, &screen, true)], 530, t0 + Duration::from_millis(1060));
        assert_eq!(d3, Some(t0 + Duration::from_millis(1590)));
        assert!(blink.visible);

        // Moving the cursor during an OFF phase resets it immediately solid-on.
        let t_off = t0 + Duration::from_millis(600);
        let _ = blink.update(&[make(1, &screen, true)], 530, t_off);
        assert!(!blink.visible);
        screen.cursor.col = 3;
        let d_moved = blink.update(&[make(1, &screen, true)], 530, t_off);
        assert!(blink.visible);
        assert_eq!(d_moved, Some(t_off + Duration::from_millis(530)));

        // Hiding the cursor stops timers immediately.
        screen.cursor.visible = false;
        assert_eq!(blink.update(&[make(1, &screen, true)], 530, t_off), None);
        assert!(blink.visible);
    }

    /// Verifies solid focused block cursor fills opaque, draws glyph text over it in
    /// the cell's background colour, honours `Screen::cursor_color` (alpha 0 fallback),
    /// leaves bar/underline/hollow glyphs untouched, and suppresses cursor on blink-off.
    #[test]
    fn gpu_block_cursor_solid_text_inversion_color_and_blink() {
        let ctx = crate::gpu::test_device::context();
        let style = WallStyle {
            cursor_color: [0.431, 0.906, 0.627, 1.0], // #6ee7a0 -> ~(110, 231, 160)
            cursor_blink_ms: 500,
            ..WallStyle::default()
        };
        let mut renderer = WallRenderer::new(
            &ctx.device,
            &ctx.queue,
            &crate::default_emoji_sheet(),
            wgpu::TextureFormat::Rgba8UnormSrgb,
            style,
        );

        let m = renderer.metrics();
        let w = 96u32;
        let h = 144u32;
        let cam = WallCamera::new(Vec2::new(m.cell_width * 0.5, -m.cell_height * 0.5), 96.0);

        let mut screen = Screen::new(4, 2);
        screen.default_bg = rgb(0x0d, 0x10, 0x17);
        screen.row_mut(0)[0] = term_cells::Cell {
            content: 'M' as u32,
            fg: rgb(0xe0, 0x20, 0xe0),
            bg: 0,
            ul: 0,
            attrs: 0,
            width: 1,
            _pad: 0,
        };
        screen.touch_row(0);
        screen.cursor = term_cells::Cursor {
            col: 0,
            row: 0,
            visible: true,
            shape: CursorShape::Block,
            blink: false,
        };

        fn make(s: &Screen, focused: bool) -> TerminalDraw<'_> {
            TerminalDraw {
                id: 1,
                screen: s,
                position: Vec2::ZERO,
                z: 1.0,
                scale: 1.0,
                focused,
                panel_opacity: Some(1.0),
                border_color: None,
                selection: None,
            }
        }

        let tl = cam.world_to_screen(Vec2::ZERO, (w, h));
        let br = cam.world_to_screen(Vec2::new(m.cell_width, -m.cell_height), (w, h));
        let x0 = (tl.x.ceil() as u32 + 2).min(w - 1);
        let x1 = (br.x.floor() as u32).saturating_sub(2).min(w);
        let y0 = (tl.y.ceil() as u32 + 2).min(h - 1);
        let y1 = (br.y.floor() as u32).saturating_sub(2).min(h);
        let count_matching = |px: &[u8], pred: fn([u8; 3]) -> bool| -> usize {
            let mut n = 0usize;
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = ((y * w + x) * 4) as usize;
                    if pred([px[i], px[i + 1], px[i + 2]]) {
                        n += 1;
                    }
                }
            }
            n
        };

        // 1. Focused Block on default bg: solid green cursor fill + dark #0d1017 'M' ink, zero magenta.
        let px_solid = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &cam,
            &WallFrame { terminals: &[make(&screen, true)], ..Default::default() },
            w,
            h,
        );
        let solid_green = count_matching(&px_solid, |c| {
            c[0].abs_diff(110) <= 3 && c[1].abs_diff(231) <= 3 && c[2].abs_diff(160) <= 3
        });
        let dark_ink = count_matching(&px_solid, |c| {
            c[0].abs_diff(0x0d) <= 3 && c[1].abs_diff(0x10) <= 3 && c[2].abs_diff(0x17) <= 3
        });
        let magenta_ink = count_matching(&px_solid, |c| c[0] > 180 && c[2] > 180 && c[1] < 80);
        assert!(solid_green > 500, "focused block cursor must fill solid #6ee7a0 ({solid_green} px)");
        assert!(dark_ink > 100, "glyph over solid block cursor must draw in default_bg #0d1017 ({dark_ink} px)");
        assert_eq!(magenta_ink, 0, "glyph over solid block cursor must not draw in cell fg");

        // 2. Explicit cell bg (#1830b0): glyph draws in cell's own bg over solid green cursor.
        screen.row_mut(0)[0].bg = rgb(0x18, 0x30, 0xb0);
        screen.touch_row(0);
        let px_cell_bg = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &cam,
            &WallFrame { terminals: &[make(&screen, true)], ..Default::default() },
            w,
            h,
        );
        let blue_ink = count_matching(&px_cell_bg, |c| {
            c[0].abs_diff(0x18) <= 3 && c[1].abs_diff(0x30) <= 3 && c[2].abs_diff(0xb0) <= 3
        });
        assert!(blue_ink > 100, "glyph over solid block cursor must draw in explicit cell.bg ({blue_ink} px)");
        screen.row_mut(0)[0].bg = 0;
        screen.touch_row(0);

        // 3. Screen::cursor_color overrides style cursor_color when alpha > 0, falls back when 0.
        screen.cursor_color = rgb(0xff, 0x55, 0x00);
        let px_orange = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &cam,
            &WallFrame { terminals: &[make(&screen, true)], ..Default::default() },
            w,
            h,
        );
        let orange_cursor = count_matching(&px_orange, |c| {
            c[0].abs_diff(0xff) <= 3 && c[1].abs_diff(0x55) <= 3 && c[2] <= 3
        });
        assert!(orange_cursor > 500, "Screen::cursor_color must override style cursor colour ({orange_cursor} px)");
        screen.cursor_color = 0;

        // 4. Bar, Underline, and Unfocused Hollow leave glyph fg magenta untouched.
        for (focused, shape) in [
            (false, CursorShape::Block),
            (true, CursorShape::Bar),
            (true, CursorShape::Underline),
        ] {
            screen.cursor.shape = shape;
            let px = render_rgba_bytes(
                &ctx,
                &mut renderer,
                &cam,
                &WallFrame { terminals: &[make(&screen, focused)], ..Default::default() },
                w,
                h,
            );
            let mag = count_matching(&px, |c| {
                c[0].abs_diff(0xe0) <= 3 && c[1].abs_diff(0x20) <= 3 && c[2].abs_diff(0xe0) <= 3
            });
            assert!(
                mag > 100,
                "cursor (focused={focused}, shape={shape:?}) must leave glyph fg untouched ({mag} px)"
            );
        }

        // 5. Blink OFF phase suppresses focused block cursor and restores magenta glyph ink.
        screen.cursor.shape = CursorShape::Block;
        screen.cursor.blink = true;
        let t0 = Instant::now();
        let draw = make(&screen, true);
        let deadline = renderer.next_cursor_blink(std::slice::from_ref(&draw), t0);
        assert_eq!(deadline, Some(t0 + Duration::from_millis(500)));
        assert!(renderer.cursor_blink_visible());

        let _ = renderer.next_cursor_blink(
            std::slice::from_ref(&draw),
            t0 + Duration::from_millis(500),
        );
        assert!(!renderer.cursor_blink_visible());
        let px_off = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &cam,
            &WallFrame { terminals: &[draw], ..Default::default() },
            w,
            h,
        );
        let green_when_off = count_matching(&px_off, |c| {
            c[0].abs_diff(110) <= 5 && c[1].abs_diff(231) <= 5 && c[2].abs_diff(160) <= 5
        });
        let magenta_when_off = count_matching(&px_off, |c| {
            c[0].abs_diff(0xe0) <= 3 && c[1].abs_diff(0x20) <= 3 && c[2].abs_diff(0xe0) <= 3
        });
        assert_eq!(green_when_off, 0, "blink-off must hide the block cursor");
        assert!(magenta_when_off > 100, "blink-off must restore normal glyph fg ({magenta_when_off} px)");
    }

    /// Verifies text selection highlighting: linear and block cell ranges,
    /// JS wall default colours (`#cccccc` bg / `#0a0a1e` fg), `"keep"` foreground
    /// mode (`foreground: None`), and minimal dirty-row uploads on selection edits.
    #[test]
    fn gpu_text_selection_linear_block_keep_fg_and_dirty_uploads() {
        let ctx = crate::gpu::test_device::context();
        let mut renderer = WallRenderer::new(
            &ctx.device,
            &ctx.queue,
            &crate::default_emoji_sheet(),
            wgpu::TextureFormat::Rgba8UnormSrgb,
            WallStyle::default(),
        );

        let mut screen = Screen::new(10, 5);
        screen.default_bg = rgb(0x0d, 0x10, 0x17);
        screen.default_fg = rgb(0xe0, 0x30, 0xe0);
        for r in 0..5u16 {
            for c in 0..10u16 {
                screen.row_mut(r)[c as usize] = term_cells::Cell {
                    content: 'M' as u32,
                    fg: rgb(0xe0, 0x30, 0xe0),
                    bg: 0,
                    ul: 0,
                    attrs: 0,
                    width: 1,
                    _pad: 0,
                };
            }
            screen.touch_row(r);
        }

        fn make(s: &Screen, selection: Option<CellSelection>) -> TerminalDraw<'_> {
            TerminalDraw {
                id: 303,
                screen: s,
                position: Vec2::ZERO,
                z: 1.0,
                scale: 1.0,
                focused: false,
                panel_opacity: Some(1.0),
                border_color: None,
                selection,
            }
        }

        let m = renderer.metrics();
        let w = 480u32;
        let h = 400u32;
        let cam = WallCamera::new(
            Vec2::new(5.0 * m.cell_width, -2.5 * m.cell_height),
            64.0,
        );

        let count_cell = |px: &[u8], col: u16, row: u16, pred: fn([u8; 3]) -> bool| -> usize {
            let rect = m.cell_rect(Vec2::ZERO, col, row, 1);
            let tl = cam.world_to_screen(Vec2::new(rect.min.x, rect.max.y), (w, h));
            let br = cam.world_to_screen(Vec2::new(rect.max.x, rect.min.y), (w, h));
            let x0 = (tl.x.ceil() as u32 + 1).min(w - 1);
            let x1 = (br.x.floor() as u32).saturating_sub(1).min(w);
            let y0 = (tl.y.ceil() as u32 + 1).min(h - 1);
            let y1 = (br.y.floor() as u32).saturating_sub(1).min(h);
            let mut n = 0usize;
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = ((y * w + x) * 4) as usize;
                    if pred([px[i], px[i + 1], px[i + 2]]) {
                        n += 1;
                    }
                }
            }
            n
        };

        // 1. Initial upload with no selection: all 5 rows uploaded, zero fills active.
        let s0 = renderer.prepare(
            &ctx.device,
            &ctx.queue,
            (w, h),
            &cam,
            &WallFrame { terminals: &[make(&screen, None)], ..Default::default() },
        );
        assert_eq!(s0.rows_uploaded, 5);
        assert_eq!(s0.write_calls, 1);

        // 2. Apply linear selection across rows 1..=3: (1,3)..(3,6) -> ONLY 3 rows uploaded!
        let lin = CellSelection::linear(1, 3, 3, 6);
        let s1 = renderer.prepare(
            &ctx.device,
            &ctx.queue,
            (w, h),
            &cam,
            &WallFrame { terminals: &[make(&screen, Some(lin))], ..Default::default() },
        );
        assert_eq!(s1, UploadStats { terminals_drawn: 1, terminals_uploaded: 1, rows_uploaded: 3, write_calls: 1 });

        // Steady frame with identical selection uploads 0 rows.
        let s1_idle = renderer.prepare(
            &ctx.device,
            &ctx.queue,
            (w, h),
            &cam,
            &WallFrame { terminals: &[make(&screen, Some(lin))], ..Default::default() },
        );
        assert_eq!(s1_idle.rows_uploaded, 0);
        assert_eq!(s1_idle.write_calls, 0);

        // Render and verify linear selection pixels (#cccccc bg, #0a0a1e fg on selected cells).
        let px_lin = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &cam,
            &WallFrame { terminals: &[make(&screen, Some(lin))], ..Default::default() },
            w,
            h,
        );
        let is_sel_bg = |c: [u8; 3]| c[0].abs_diff(0xcc) <= 3 && c[1].abs_diff(0xcc) <= 3 && c[2].abs_diff(0xcc) <= 3;
        let is_sel_fg = |c: [u8; 3]| c[0].abs_diff(0x0a) <= 3 && c[1].abs_diff(0x0a) <= 3 && c[2].abs_diff(0x1e) <= 3;
        let is_orig_fg = |c: [u8; 3]| c[0].abs_diff(0xe0) <= 3 && c[1].abs_diff(0x30) <= 3 && c[2].abs_diff(0xe0) <= 3;

        // Selected cells: (1, 5), (2, 0) [interior full row], (3, 4).
        for (col, row) in [(5, 1), (0, 2), (4, 3)] {
            assert!(count_cell(&px_lin, col, row, is_sel_bg) > 100, "cell ({col},{row}) should have #cccccc selection fill");
            assert!(count_cell(&px_lin, col, row, is_sel_fg) > 40, "cell ({col},{row}) should have #0a0a1e selection fg");
            assert_eq!(count_cell(&px_lin, col, row, is_orig_fg), 0, "cell ({col},{row}) should override orig fg");
        }
        // Unselected cells: (0, 0), (2, 1) [before start_col], (6, 3) [at end_col].
        for (col, row) in [(0, 0), (2, 1), (6, 3)] {
            assert_eq!(count_cell(&px_lin, col, row, is_sel_bg), 0, "unselected cell ({col},{row}) must not have selection fill");
            assert!(count_cell(&px_lin, col, row, is_orig_fg) > 40, "unselected cell ({col},{row}) must keep original fg");
        }

        // 3. Extend drag right on row 3 -> ONLY 1 row (row 3) uploaded!
        let lin_ext = CellSelection::linear(1, 3, 3, 9);
        let s2 = renderer.prepare(
            &ctx.device,
            &ctx.queue,
            (w, h),
            &cam,
            &WallFrame { terminals: &[make(&screen, Some(lin_ext))], ..Default::default() },
        );
        assert_eq!(s2, UploadStats { terminals_drawn: 1, terminals_uploaded: 1, rows_uploaded: 1, write_calls: 1 });

        // 4. "keep" foreground mode (`foreground: None`): custom blue fill + original magenta fg preserved!
        let keep_sel = lin_ext.with_colors(rgb(0x28, 0x48, 0xa8), None);
        let s3 = renderer.prepare(
            &ctx.device,
            &ctx.queue,
            (w, h),
            &cam,
            &WallFrame { terminals: &[make(&screen, Some(keep_sel))], ..Default::default() },
        );
        assert_eq!(s3.rows_uploaded, 3);
        let px_keep = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &cam,
            &WallFrame { terminals: &[make(&screen, Some(keep_sel))], ..Default::default() },
            w,
            h,
        );
        let is_blue_bg = |c: [u8; 3]| c[0].abs_diff(0x28) <= 3 && c[1].abs_diff(0x48) <= 3 && c[2].abs_diff(0xa8) <= 3;
        assert!(count_cell(&px_keep, 5, 2, is_blue_bg) > 100, "keep mode should paint selection background");
        assert!(count_cell(&px_keep, 5, 2, is_orig_fg) > 40, "keep mode must preserve cell's original magenta fg");

        // 5. Column-block selection: rows 0..=2, cols 2..5 only.
        let blk = CellSelection::block(0, 2, 2, 5);
        let px_blk = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &cam,
            &WallFrame { terminals: &[make(&screen, Some(blk))], ..Default::default() },
            w,
            h,
        );
        assert!(count_cell(&px_blk, 3, 1, is_sel_bg) > 100, "inside block must be selected");
        assert_eq!(count_cell(&px_blk, 1, 1, is_sel_bg), 0, "left of block must stay unselected");
        assert_eq!(count_cell(&px_blk, 5, 1, is_sel_bg), 0, "right of block must stay unselected");

        // 6. Clearing selection re-uploads ONLY rows 0..=2 (3 rows) and restores original pixels.
        let s_clear = renderer.prepare(
            &ctx.device,
            &ctx.queue,
            (w, h),
            &cam,
            &WallFrame { terminals: &[make(&screen, None)], ..Default::default() },
        );
        assert_eq!(s_clear, UploadStats { terminals_drawn: 1, terminals_uploaded: 1, rows_uploaded: 3, write_calls: 1 });
        let px_clear = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &cam,
            &WallFrame { terminals: &[make(&screen, None)], ..Default::default() },
            w,
            h,
        );
        assert_eq!(count_cell(&px_clear, 3, 1, is_sel_bg), 0, "cleared selection must restore original background");
        assert!(count_cell(&px_clear, 3, 1, is_orig_fg) > 40, "cleared selection must restore original fg");
    }

    #[test]
    fn gpu_shader_hot_reload_validates_swaps_and_keeps_old_on_error() {
        let ctx = crate::gpu::test_device::context();
        let mut renderer = WallRenderer::new(
            &ctx.device,
            &ctx.queue,
            &crate::default_emoji_sheet(),
            wgpu::TextureFormat::Rgba8UnormSrgb,
            WallStyle::default(),
        );
        // With watcher disabled, poll_shaders is a zero-work no-op.
        renderer.set_shader_watch_dir(None);
        assert_eq!(renderer.poll_shaders(&ctx.device, Instant::now()), (false, None));
        assert!(renderer.shader_error_notice().is_none());

        // Stage all three shaders in a temporary watch directory.
        let tmp = std::env::temp_dir().join(format!(
            "glyph3d-shader-reload-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp shader dir");
        let cells_src = include_str!("../shaders/wall_cells.wgsl");
        std::fs::write(tmp.join("wall_paper.wgsl"), include_str!("../shaders/wall_paper.wgsl")).unwrap();
        std::fs::write(tmp.join("wall_shapes.wgsl"), include_str!("../shaders/wall_shapes.wgsl")).unwrap();
        std::fs::write(tmp.join("wall_cells.wgsl"), cells_src).unwrap();

        renderer.set_shader_watch_dir(Some(tmp.clone()));
        let mut screen = Screen::new(4, 2);
        screen.default_bg = rgb(0x10, 0x10, 0x20);
        let draws = [TerminalDraw {
            id: 1,
            screen: &screen,
            position: Vec2::new(-2.0, 2.0),
            z: 0.0,
            scale: 1.0,
            focused: false,
            panel_opacity: Some(1.0),
            border_color: None,
            selection: None,
        }];
        let cam = WallCamera::new(Vec2::ZERO, 32.0);
        let (w, h) = (128, 128);

        let center_rgb = |px: &[u8]| -> [u8; 3] {
            let i = ((h / 2) * w + (w / 2)) as usize * 4;
            [px[i], px[i + 1], px[i + 2]]
        };

        // Baseline: dark blue panel (#101020).
        let px0 = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &cam,
            &WallFrame { terminals: &draws, ..Default::default() },
            w,
            h,
        );
        let c0 = center_rgb(&px0);
        assert!(c0[2] > c0[1], "baseline should be dark blue, got {c0:?}");

        // 1. Valid edit: force panel fragment output to bright green (0.0, 0.8, 0.2).
        let green_src = cells_src.replacen(
            "let fill_rgb = srgb_to_output(in.fill_color.rgb);",
            "let fill_rgb = srgb_to_output(vec3<f32>(0.0, 0.8, 0.2));",
            1,
        );
        assert_ne!(green_src, cells_src, "expected fill_rgb line in wall_cells.wgsl");
        std::fs::write(tmp.join("wall_cells.wgsl"), &green_src).unwrap();
        let (dirty, next) = renderer.poll_shaders(&ctx.device, Instant::now() + SHADER_POLL_INTERVAL);
        assert!(dirty, "valid edit must trigger reload");
        assert!(next.is_some());
        assert!(renderer.shader_error_notice().is_none());

        let px1 = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &cam,
            &WallFrame { terminals: &draws, ..Default::default() },
            w,
            h,
        );
        let c1 = center_rgb(&px1);
        assert!(c1[1] > 180 && c1[0] < 30, "reloaded shader should draw green panel, got {c1:?}");

        // 2. Broken WGSL: keep green pipeline intact, expose error notice, and do not re-dirty on repeat poll.
        std::fs::write(tmp.join("wall_cells.wgsl"), "fn broken( { !!!").unwrap();
        let (dirty_err, _) = renderer.poll_shaders(
            &ctx.device,
            Instant::now() + SHADER_POLL_INTERVAL * 3,
        );
        assert!(dirty_err, "new shader error should dirty notice");
        let notice = renderer.shader_error_notice().expect("error notice should be set");
        assert!(notice.contains("wall_cells.wgsl"), "notice should name shader: {notice}");

        let (dirty_repeat, _) = renderer.poll_shaders(
            &ctx.device,
            Instant::now() + SHADER_POLL_INTERVAL * 5,
        );
        assert!(!dirty_repeat, "unchanged broken file must not spam redraws/errors");

        let px2 = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &cam,
            &WallFrame { terminals: &draws, ..Default::default() },
            w,
            h,
        );
        let c2 = center_rgb(&px2);
        assert!(c2[1] > 180 && c2[0] < 30, "old green pipeline must stay active on error, got {c2:?}");

        // 3. Fix back to original: clears notice and restores dark blue panel.
        std::fs::write(tmp.join("wall_cells.wgsl"), cells_src).unwrap();
        let (dirty_fix, _) = renderer.poll_shaders(
            &ctx.device,
            Instant::now() + SHADER_POLL_INTERVAL * 7,
        );
        assert!(dirty_fix, "fixed shader must reload and clear notice");
        assert!(renderer.shader_error_notice().is_none());
        let px3 = render_rgba_bytes(
            &ctx,
            &mut renderer,
            &cam,
            &WallFrame { terminals: &draws, ..Default::default() },
            w,
            h,
        );
        let c3 = center_rgb(&px3);
        assert!(c3[2] > c3[1] && c3[1] < 40, "restored shader should draw dark blue again, got {c3:?}");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
