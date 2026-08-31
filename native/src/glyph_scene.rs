//! Stage C — glyph field scene: Slug pipeline, group table, text camera.
//! Stage F — fly camera + two-level GPU culling with LOD backdrops.
//! Stage G — CPU picking + live instance/group manipulation.
//!
//! Mode-agnostic like `scene.rs`: windowed and offscreen both build one
//! `GlyphScene` and call `render()` into their own target.
//!
//! ## Stage G pick/manipulation contract
//!
//! PICKING IS CPU-SIDE, deliberately: every glyph's geometry is derivable
//! deterministically (a picked file's records are re-produced by re-running
//! the engine with the SAME ItemParams it was staged with — bit-identical by
//! the Stage E2 verify discipline), the segment table already holds per-file
//! world AABBs, and ~1.3k ray-AABB tests cost microseconds. A GPU ID pass
//! would buy nothing at this scale and would re-enter the wgpu/Metal
//! indirect-draw morass documented below. Resolution order:
//!   screen px → ray (inverse view-proj) → nearest visible file AABB →
//!   ray ∩ file plane (z = group offset.z) → local point (undo group TRS) →
//!   nearest record cell → (row, col) → source line/byte/char via
//!   text::fold_leaders, cross-checked against the engine's ROW/COL lanes.
//!
//! EDITS write through to the GPU with PARTIAL uploads only:
//!   - instance fields (color/pos/advance/height): queue.write_buffer into
//!     the affected arena chunk at slot granularity (48 B stride, 4-aligned);
//!   - group TRS/color/alpha: one 80 B GroupRow write per edit, plus a CPU
//!     segment-table sync (AABB follows the group; backdrop tint follows the
//!     group color; hidden groups are skipped by the cull entirely).
//!
//! ## Stage F cull/LOD contract (see shaders/cull.wgsl header for the backdrop side)
//!
//! The staged instances are partitioned into SEGMENTS (one per file in repo
//! mode; a single cover segment for text scenes). Each frame, `cull_segments`
//! (CPU, ~1.3k AABBs ≈ microseconds) tests each segment's world AABB against
//! the frustum and its on-screen glyph size against LOD_MIN_PX, producing:
//!   - per-chunk vertex-range draw lists for visible segments
//!     (`pass.draw(0..6, base..base+count)` — instance_index stays
//!     chunk-local, exactly like the legacy per-chunk draws), and
//!   - a compacted backdrop list (flat quads: mean ink color × ink coverage)
//!     for segments whose whole em is below one screen pixel.
//! Culling is visually lossless: a culled segment is entirely outside the
//! frustum, and the LOD tier only substitutes subpixel glyphs.
//!
//! WHY CPU + DIRECT DRAWS: the original Stage F design was the standard
//! WebGPU pattern (compute cull pass → indirect multi-draws). It is
//! empirically BROKEN in wgpu 30.0.1's Metal backend: any indirect draw with
//! `first_instance != 0` rasterizes nothing (verified via GLYPH_CULL_DEBUG
//! readbacks — args correct — plus per-chunk/per-offset A/Bs; draws with
//! first_instance = 0 work, nonzero never do, through the indirect-validation
//! batcher). 1,305 tiny CPU AABB tests + direct range draws are ~free, fully
//! deterministic, and need no indirect machinery. The GPU compute path can
//! be revisited after a wgpu upgrade.

use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3, Vec4};
use std::cell::Cell;
use std::path::PathBuf;
use wgpu::util::DeviceExt;

use crate::atlas::Atlas;
use crate::engine::{GlyphRecord, ItemParams};
use crate::gpu::GpuContext;
use crate::scene::SceneLike;
use crate::text::StagedText;

/// Vertical field of view shared by every glyph-scene camera mode.
const FOV_Y: f32 = 40f32;

/// Stage F: LOD threshold in on-screen pixels per em cell (cell height = 1.0
/// world unit). Below 1 px/em individual glyphs are raster-lottery subpixel
/// blobs; the segment's backdrop quad (mean ink color × ink coverage) is the
/// visually equivalent representation. Never substitutes at ≥1 px/em, so
/// legible text is always drawn glyph-by-glyph.
pub const LOD_MIN_PX: f32 = 1.0;

/// Stage F: backdrop coverage gain — fitted against the Stage E2 full-field
/// render (out/f-field-before.png): with E = ink_frac × GAIN the post-LOD
/// wide shot's mean linear pixel value matches the pre-LOD one within ~3%
/// (GAIN 1.9 overshot +45%, 1.3 overshot +29%, 1.0 +17%, 0.7 lands even; the residual
/// difference is the flat-per-file haze vs real per-page texture — the
/// backdrop fills a file's intra-page gaps). See out/STAGE_F_REPORT.md.
pub const BACKDROP_GAIN: f32 = 0.7;

/// Per-instance glyph slot — 48 B, mirrors `InstanceSlot` in glyph_field.wgsl.
/// (Layout rationale is documented in the shader header.)
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct GlyphInstance {
    pub pos: [f32; 3],
    pub glyph_id: u32,
    pub row: u32,
    pub col: u32,
    pub color: u32, // packed RGBA8 (sRGB display values)
    pub group_id: u32,
    pub advance: f32,
    pub height: f32,
    pub flags: u32,
    pub _pad: u32,
}

/// Group table row — 5 vec4s, 80 B, the web's GROUP_STRIDE=5 schema
/// (glyphVertex.js): offset / quat / color+alpha / scale+colorBlend / clip.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct GroupRow {
    pub cols: [[f32; 4]; 5],
}

impl GroupRow {
    /// Identity pose at `offset`: unit quat, white opaque color, unit scale,
    /// colorBlend 0 (multiply), clip disabled.
    pub fn identity(offset: [f32; 3]) -> Self {
        Self {
            cols: [
                [offset[0], offset[1], offset[2], 0.0],
                [0.0, 0.0, 0.0, 1.0],
                [1.0, 1.0, 1.0, 1.0],
                [1.0, 1.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 0.0],
            ],
        }
    }

    /// Stage E2: identity pose with a color tint (multiplied with per-instance
    /// colors; colorBlend 0 = multiply, alpha 1).
    pub fn tinted(offset: [f32; 3], rgb: [f32; 3]) -> Self {
        let mut g = Self::identity(offset);
        g.cols[2] = [rgb[0], rgb[1], rgb[2], 1.0];
        g
    }
}

// ── Stage G — picking & manipulation types ─────────────────────────────────

/// Per-file pick record: everything needed to (a) hit-test the file's AABB
/// under the LIVE group TRS and (b) re-derive its glyph geometry via a
/// deterministic engine re-run (`repo::rederive_records` with `item`).
pub struct PickFileInfo {
    pub rel_path: String,
    pub group_id: u32,
    pub slot_base: u32,
    pub slot_count: u32,
    /// The exact engine params this file was laid out with.
    pub item: ItemParams,
    /// Local-space (pre-TRS) xy AABB, same margins as the cull segment.
    pub aabb_min: [f32; 2],
    pub aabb_max: [f32; 2],
}

/// Repo-mode pick context (one per staged scene).
pub struct PickContext {
    pub root: PathBuf,
    pub trie: PathBuf,
    pub files: Vec<PickFileInfo>,
}

/// A pick request — scripted (CLI) or interactive (click).
#[derive(Clone)]
pub enum PickCommand {
    /// Group-level pick: first file whose rel path contains the substring.
    File(String),
    /// Deterministic glyph pick: exact folded (row, col) within that file.
    RowCol { file: String, row: u32, col: u32 },
    /// Ray pick through a physical pixel of the current viewport.
    Pixel { x: f32, y: f32 },
}

/// A manipulation verb, applied to the current pick. Instance verbs need a
/// glyph pick; group verbs need at least a file pick.
#[derive(Clone)]
pub enum Verb {
    /// Recolor the picked glyph (packed sRGB rgb, alpha kept 255).
    RecolorGlyph([u8; 3]),
    /// Recolor every glyph on the picked glyph's folded row ("highlight line").
    RecolorLine([u8; 3]),
    /// Offset the picked glyph's local position (plumbing demo).
    NudgeGlyph([f32; 3]),
    /// Scale the picked glyph's quad (advance & height; plumbing demo).
    ScaleGlyph(f32),
    /// Move the picked file's group offset (world units).
    MoveGroup([f32; 3]),
    /// Multiply the picked file's group scale (uniform xyz).
    ScaleGroup(f32),
    /// Set the picked file's group color tint (sRGB floats).
    TintGroup([f32; 3]),
    /// Cycle the picked file through the per-directory tint palette.
    TintCycle,
    SetHidden(bool),
    ToggleHidden,
}

/// A resolved glyph within a file.
pub struct PickGlyph {
    /// Record index within the file (== UTF-8 leader index).
    pub record: usize,
    /// Arena slot (global), None for blank/missing records (no instance).
    pub slot: Option<u32>,
    pub row: u32,
    pub col: u32,
    /// Source line (0-based) — differs from `row` under wrap/pagination.
    pub line: u32,
    pub byte_off: usize,
    pub ch: char,
    /// Local (pre-TRS) layout position/metrics — the verb write-back basis.
    pub pos: [f32; 3],
    pub advance: f32,
    pub height: f32,
    /// The staged packed color (for flash restore).
    pub color: u32,
}

/// A resolved pick: always the file; the glyph when one is close enough.
pub struct PickHit {
    pub group_id: u32,
    pub rel_path: String,
    pub glyph: Option<PickGlyph>,
}

/// Per-file pick cache: the re-derived records plus the CPU-side walks over
/// the file bytes, all indexed by record (== leader) index.
struct PickCacheEntry {
    group_id: u32,
    records: Vec<GlyphRecord>,
    /// (byte offset, codepoint) per record.
    leaders: Vec<(usize, u32)>,
    /// Source line per record.
    lines: Vec<u32>,
    /// Staged per-record packed colors (flash restore / recolor basis).
    colors: Vec<u32>,
    /// Global arena slot per record; u32::MAX for blank/missing (no instance).
    slot_of: Vec<u32>,
}

/// Stage F — per-segment cull record, 48 B, mirrors `SegCull` in cull.wgsl
/// (vec4 alignment: the `_pad` lane keeps `tint` at offset 32 on both sides).
/// One segment per FILE in repo mode; text scenes stage a single segment
/// covering the whole block. Bounds are WORLD-space xy with the group offset
/// already applied; all instances live in the z=0 plane (the cull pass tests
/// z ∈ [-1, 1] as a margin).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct SegCull {
    pub min: [f32; 2],
    pub max: [f32; 2],
    pub slot_base: u32,
    pub slot_count: u32,
    pub _pad: [f32; 2],
    /// rgb = mean LINEAR ink color (sRGB bytes pow-2.2 decoded at staging);
    /// w = effective per-pixel ink coverage E at deep minification
    /// (`ink_frac × BACKDROP_GAIN`, clamped to 1) — the backdrop alpha.
    pub tint: [f32; 4],
}

/// Stage F — world cell footprint used for ink-density estimates: em advance
/// 1229/2320 ≈ 0.53 world units × 1.25 line pitch. Approximation only; it
/// feeds the backdrop haze alpha, never any layout decision.
pub const GLYPH_CELL_AREA: f32 = (1229.0 / 2320.0) * 1.25;

/// Stage F — mean linear ink color + backdrop coverage for a slice of
/// instances occupying a `width × height` world rect. Shared by the repo
/// per-file segments and the single-segment text scenes.
pub fn seg_tint(instances: &[GlyphInstance], width: f32, height: f32) -> [f32; 4] {
    let mut sum = [0f64; 3];
    for g in instances {
        // Match the shader's decode: sRGB display bytes → linear via pow 2.2.
        for (i, s) in sum.iter_mut().enumerate() {
            let byte = ((g.color >> (8 * i)) & 0xFF) as f64 / 255.0;
            *s += byte.powf(2.2);
        }
    }
    let n = instances.len().max(1) as f64;
    let area = (width as f64 * height as f64).max(1e-3);
    let ink_frac = (instances.len() as f64 * GLYPH_CELL_AREA as f64 / area).min(1.0);
    let e = (ink_frac * BACKDROP_GAIN as f64).min(1.0);
    [
        (sum[0] / n) as f32,
        (sum[1] / n) as f32,
        (sum[2] / n) as f32,
        e as f32,
    ]
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct CameraUniform {
    view_proj: [f32; 16],
}

/// GlyphField.js GLYPH_LOD_DEFAULTS + group count, one uniform block.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Params {
    max_groups: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
    dilate_px: f32,
    soften: f32,
    min_lo: f32,
    min_hi: f32,
}

/// Stage F — one far-LOD backdrop quad, 32 B, mirrors `BackdropInst` in
/// cull.wgsl: world rect + premultiplied-ready color (rgb linear, a = E).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BackdropInst {
    min: [f32; 2],
    max: [f32; 2],
    rgba: [f32; 4],
}

/// Stage F — CPU cull: frustum + LOD over the segment table. Returns the
/// glyph draw list (chunk, chunk_local_base, count — in arena order per
/// chunk, so blending matches the legacy full draws exactly) and the
/// compacted backdrop list. See the module header for the contract and for
/// why this runs on the CPU. Stage G: `hidden` (parallel to `segments`,
/// empty = nothing hidden) skips user-hidden groups entirely — no glyph
/// draws AND no backdrop.
fn cull_segments(
    segments: &[SegCull],
    hidden: &[bool],
    planes: &[[f32; 4]; 6],
    eye: Vec3,
    px_scale: f32,
    chunk_cap: u32,
    chunk_count: u32,
) -> (Vec<Vec<std::ops::Range<u32>>>, Vec<BackdropInst>) {
    let mut draws: Vec<Vec<std::ops::Range<u32>>> =
        (0..chunk_count).map(|_| Vec::new()).collect();
    let mut backdrops = Vec::new();
    for (si, seg) in segments.iter().enumerate() {
        if hidden.get(si).copied().unwrap_or(false) {
            continue;
        }
        // Frustum: positive-vertex test per plane; z margin ±1 (z=0 plane).
        let mut visible = true;
        for pl in planes {
            let px = if pl[0] >= 0.0 { seg.max[0] } else { seg.min[0] };
            let py = if pl[1] >= 0.0 { seg.max[1] } else { seg.min[1] };
            let pz = if pl[2] >= 0.0 { 1.0 } else { -1.0 };
            if pl[0] * px + pl[1] * py + pl[2] * pz + pl[3] < 0.0 {
                visible = false;
                break;
            }
        }
        if !visible {
            continue;
        }
        // LOD: em-cell pixel height at the AABB's nearest point to the eye
        // (conservative — a segment only drops to backdrop when even its
        // CLOSEST glyphs are subpixel).
        let nx = eye.x.clamp(seg.min[0], seg.max[0]);
        let ny = eye.y.clamp(seg.min[1], seg.max[1]);
        let nz = eye.z.clamp(-1.0, 1.0);
        let dist = ((eye.x - nx).powi(2) + (eye.y - ny).powi(2) + (eye.z - nz).powi(2))
            .sqrt()
            .max(0.001);
        let glyph_px = px_scale / dist;
        if glyph_px < LOD_MIN_PX {
            if seg.slot_count > 0 {
                backdrops.push(BackdropInst {
                    min: seg.min,
                    max: seg.max,
                    rgba: seg.tint,
                });
            }
            continue;
        }
        // Glyph stream: split the slot range across arena chunks.
        let slot_end = seg.slot_base + seg.slot_count;
        for c in 0..chunk_count {
            let c_lo = c * chunk_cap;
            let lo = seg.slot_base.max(c_lo);
            let hi = slot_end.min(c_lo + chunk_cap);
            if hi > lo {
                draws[c as usize].push((lo - c_lo)..(hi - c_lo));
            }
        }
    }
    (draws, backdrops)
}

/// Extract the 6 frustum planes from a view-proj matrix (Gribb-Hartmann;
/// wgpu clip space has z ∈ [0,w], so the near plane is row2, not row3+row2).
/// glam's to_cols_array is column-major: row r = (m[r], m[4+r], m[8+r], m[12+r]).
fn frustum_planes(vp: &Mat4) -> [[f32; 4]; 6] {
    let m = vp.to_cols_array();
    let row = |r: usize| [m[r], m[4 + r], m[8 + r], m[12 + r]];
    let (r0, r1, r2, r3) = (row(0), row(1), row(2), row(3));
    let add = |a: [f32; 4], b: [f32; 4]| [a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3]];
    let sub = |a: [f32; 4], b: [f32; 4]| [a[0] - b[0], a[1] - b[1], a[2] - b[2], a[3] - b[3]];
    let mut planes = [
        add(r3, r0), // left
        sub(r3, r0), // right
        add(r3, r1), // bottom
        sub(r3, r1), // top
        r2,          // near (z ≥ 0)
        sub(r3, r2), // far
    ];
    for p in &mut planes {
        let len = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt().max(1e-12);
        for c in p.iter_mut() {
            *c /= len;
        }
    }
    planes
}

/// Camera behavior. `Front` faces the text plane dead-on at a fit distance
/// (offscreen verification); `Orbit` slowly circles the block (legacy
/// windowed demo); `Fly` is the Stage F free camera driven by windowed input.
/// `zoom` multiplies magnification (2.0 = twice as close).
#[derive(Clone, Copy)]
pub enum CameraMode {
    Front { zoom: f32 },
    Orbit,
    Fly,
}

/// Stage F — fly camera state: WASD strafe/forward, E|R up, Q|F down,
/// mouse-look (yaw/pitch), scroll = persistent speed multiplier, exponential
/// velocity damping. yaw = 0 looks down −Z (the text plane faces +Z).
#[derive(Clone, Copy)]
pub struct FlyCamera {
    pub eye: Vec3,
    yaw: f32,
    pitch: f32,
    speed: f32,
    speed_min: f32,
    speed_max: f32,
    vel: Vec3,
    keys: u8, // FWD|BACK|LEFT|RIGHT|UP|DOWN
}

const FLY_FWD: u8 = 1;
const FLY_BACK: u8 = 2;
const FLY_LEFT: u8 = 4;
const FLY_RIGHT: u8 = 8;
const FLY_UP: u8 = 16;
const FLY_DOWN: u8 = 32;

impl FlyCamera {
    fn new(eye: Vec3, fit: f32) -> Self {
        Self {
            eye,
            yaw: 0.0,
            pitch: 0.0,
            speed: fit * 0.4,
            speed_min: fit * 0.005,
            speed_max: fit * 8.0,
            vel: Vec3::ZERO,
            keys: 0,
        }
    }

    /// View direction from yaw/pitch: yaw 0 = −Z, right-handed, Y up.
    fn forward(&self) -> Vec3 {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        Vec3::new(sy * cp, sp, -cy * cp)
    }

    fn on_key(&mut self, code: winit::keyboard::KeyCode, pressed: bool) {
        use winit::keyboard::KeyCode as K;
        let bit = match code {
            K::KeyW => FLY_FWD,
            K::KeyS => FLY_BACK,
            K::KeyA => FLY_LEFT,
            K::KeyD => FLY_RIGHT,
            K::KeyE | K::KeyR => FLY_UP,
            K::KeyQ | K::KeyF => FLY_DOWN,
            _ => return,
        };
        if pressed {
            self.keys |= bit;
        } else {
            self.keys &= !bit;
        }
    }

    fn on_look(&mut self, dx: f32, dy: f32) {
        const SENS: f32 = 0.0022;
        self.yaw -= dx * SENS;
        self.pitch = (self.pitch - dy * SENS).clamp(-1.55, 1.55);
    }

    fn on_scroll(&mut self, lines: f32) {
        self.speed = (self.speed * 1.15f32.powf(lines)).clamp(self.speed_min, self.speed_max);
    }

    fn tick(&mut self, dt: f32) {
        let fwd = self.forward();
        let right = Vec3::new(self.yaw.cos(), 0.0, self.yaw.sin());
        let mut dir = Vec3::ZERO;
        if self.keys & FLY_FWD != 0 {
            dir += fwd;
        }
        if self.keys & FLY_BACK != 0 {
            dir -= fwd;
        }
        if self.keys & FLY_RIGHT != 0 {
            dir += right;
        }
        if self.keys & FLY_LEFT != 0 {
            dir -= right;
        }
        if self.keys & FLY_UP != 0 {
            dir += Vec3::Y;
        }
        if self.keys & FLY_DOWN != 0 {
            dir -= Vec3::Y;
        }
        let target = if dir.length_squared() > 0.0 {
            dir.normalize() * self.speed
        } else {
            Vec3::ZERO
        };
        // Exponential approach: ~63% of the way to target every 100 ms.
        let k = 1.0 - (-10.0 * dt).exp();
        self.vel += (target - self.vel) * k;
        self.eye += self.vel * dt;
    }
}

/// One frame's camera products: the view-proj (written to the camera uniform)
/// plus the eye position (consumed by the cull pass for the LOD metric).
struct CamFrame {
    view_proj: Mat4,
    eye: Vec3,
}

/// Stage F — the cull/LOD subsystem. The per-frame segment cull itself runs
/// on the CPU (`cull_segments`, ~1.3k AABB tests ≈ microseconds — see the
/// module header for why the GPU-indirect design was abandoned); what remains
/// here is the far-LOD BACKDROP stream: a CPU-compacted quad list uploaded
/// per frame plus the flat-tint pipeline drawing it.
struct CullState {
    /// CPU copy of the segment table (the per-frame cull input).
    segments: Vec<SegCull>,
    /// Stage G: per-segment LOCAL (pre-TRS) AABBs + the as-staged backdrop
    /// tints + hidden flags — group edits re-sync `segments` from these
    /// (`GlyphScene::sync_segment`).
    local_min: Vec<[f32; 2]>,
    local_max: Vec<[f32; 2]>,
    base_tint: Vec<[f32; 4]>,
    /// Group color rgb at staging time — tint edits scale the backdrop by
    /// pow(new)/pow(orig) so an untouched segment keeps its Stage F tint.
    orig_group_rgb: Vec<[f32; 3]>,
    hidden: Vec<bool>,
    /// seg_count × 32 B staging target for the per-frame backdrop list.
    backdrop_insts_buf: wgpu::Buffer,
    backdrop_pipeline: wgpu::RenderPipeline,
    backdrop_bind_group: wgpu::BindGroup,
}

impl CullState {
    fn new(
        ctx: &GpuContext,
        color_format: wgpu::TextureFormat,
        depth_format: wgpu::TextureFormat,
        camera_buf: &wgpu::Buffer,
        segments: &[SegCull],
        groups: &[GroupRow],
    ) -> Self {
        let device = &ctx.device;
        let seg_count = segments.len() as u32;

        // Stage G: derive the local (pre-TRS) segment AABBs — at staging time
        // every group is at scale 1, so local = world − group offset.
        let mut local_min = Vec::with_capacity(segments.len());
        let mut local_max = Vec::with_capacity(segments.len());
        let mut base_tint = Vec::with_capacity(segments.len());
        let mut orig_group_rgb = Vec::with_capacity(segments.len());
        for (i, seg) in segments.iter().enumerate() {
            let off = groups
                .get(i)
                .map(|g| [g.cols[0][0], g.cols[0][1]])
                .unwrap_or([0.0, 0.0]);
            local_min.push([seg.min[0] - off[0], seg.min[1] - off[1]]);
            local_max.push([seg.max[0] - off[0], seg.max[1] - off[1]]);
            base_tint.push(seg.tint);
            orig_group_rgb.push(
                groups
                    .get(i)
                    .map(|g| [g.cols[2][0], g.cols[2][1], g.cols[2][2]])
                    .unwrap_or([1.0, 1.0, 1.0]),
            );
        }

        let backdrop_insts_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("backdrop instances"),
            size: (seg_count.max(1) * 32) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        log::info!(
            "cull: {} segments (CPU frustum+LOD) | backdrop buffer {} B (KB-scale, no instance-sized buffers added)",
            seg_count,
            seg_count * 32,
        );

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cull.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/cull.wgsl").into()),
        });

        // --- backdrop render pipeline ----------------------------------------
        // Flat tinted quads for far (subpixel-glyph) segments. Same camera
        // uniform and same premultiplied blend as the glyph pass; depth test
        // without write (all glyph-plane content lives at z=0, segments do
        // not overlap, so draw order between streams is immaterial).
        let ro_storage = |binding: u32, visibility: wgpu::ShaderStages| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let backdrop_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("backdrop bgl"),
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
                ro_storage(1, wgpu::ShaderStages::VERTEX),
            ],
        });
        let backdrop_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("backdrop bg"),
            layout: &backdrop_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: camera_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: backdrop_insts_buf.as_entire_binding(),
                },
            ],
        });
        let backdrop_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("backdrop pl"),
            bind_group_layouts: &[Some(&backdrop_bgl)],
            immediate_size: 0,
        });
        let backdrop_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("backdrop pipeline"),
            layout: Some(&backdrop_pl),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_backdrop"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_backdrop"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: color_format,
                    blend: Some(wgpu::BlendState {
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
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: depth_format,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        Self {
            segments: segments.to_vec(),
            local_min,
            local_max,
            base_tint,
            orig_group_rgb,
            hidden: vec![false; segments.len()],
            backdrop_insts_buf,
            backdrop_pipeline,
            backdrop_bind_group,
        }
    }
}

pub struct GlyphScene {
    pub pipeline: wgpu::RenderPipeline,
    /// Stage E2: one bind group per instance-buffer CHUNK. A repo-scale field
    /// can exceed `max_storage_buffer_binding_size` (48 B × tens of millions
    /// of glyphs), so the arena is split into buffers that each fit the
    /// binding limit; render() issues one draw per chunk. instance_index is
    /// chunk-local, which is exactly right — each chunk buffer starts at 0.
    bind_groups: Vec<wgpu::BindGroup>,
    chunk_counts: Vec<u32>,
    /// Stage F: instances per chunk (the uniform the cull pass uses to split
    /// a segment's slot range across chunk draws).
    chunk_cap: u32,
    camera_buf: wgpu::Buffer,
    depth_format: wgpu::TextureFormat,
    instance_count: u32,
    center: Vec3,
    half_w: f32,
    half_h: f32,
    /// Fit distance of the Front camera — scales the Fly camera's near/far
    /// and initial speed.
    fit: f32,
    camera_mode: CameraMode,
    fly: FlyCamera,
    /// Stage F: present unless culling is disabled (`--no-cull`); None keeps
    /// the legacy per-chunk draws.
    cull: Option<CullState>,
    // ── Stage G: picking & live manipulation ────────────────────────────
    /// Arena chunk buffers, kept for partial per-slot uploads (verbs).
    instance_bufs: Vec<wgpu::Buffer>,
    /// Group table buffer, kept for partial per-row uploads (80 B/row).
    group_buf: wgpu::Buffer,
    /// CPU mirror of the group table — the pick path reads the LIVE TRS from
    /// here and every group verb writes it back (then uploads just that row).
    groups_cpu: Vec<GroupRow>,
    /// Repo-mode pick context (None for text/engine scenes).
    pick: Option<PickContext>,
    /// The last resolved pick (verbs operate on it).
    picked: Option<PickHit>,
    /// Flash-highlighted glyph: (arena slot, original packed color); the next
    /// click restores it before flashing the new pick.
    flash: Option<(u32, u32)>,
    /// One-entry cache of the last pick's re-derived file data.
    cache: Option<PickCacheEntry>,
    /// Windowed grab verb: the group being dragged with the mouse.
    grabbed_group: Option<u32>,
    /// Last known cursor position, physical px (click pick + grab drag).
    cursor: (f32, f32),
    /// Viewport in physical px, refreshed every render() (ray unprojection).
    viewport: Cell<(u32, u32)>,
    /// Per-group position in the DIR_TINTS cycle (t verb).
    tint_step: Vec<u32>,
}

impl GlyphScene {
    pub fn new(
        ctx: &GpuContext,
        color_format: wgpu::TextureFormat,
        atlas: &Atlas,
        staged: StagedText,
        camera_mode: CameraMode,
        cull_enabled: bool,
    ) -> Self {
        let device = &ctx.device;

        // --- instance + group buffers --------------------------------------
        let mut instances = staged.instances;
        if instances.is_empty() {
            instances.push(GlyphInstance {
                pos: [0.0; 3],
                glyph_id: 0,
                row: 0,
                col: 0,
                color: 0,
                group_id: 0,
                advance: 0.0,
                height: 0.0,
                flags: 0,
                _pad: 0,
            });
        }
        let mut groups = staged.groups;
        if groups.is_empty() {
            groups.push(GroupRow::identity([0.0; 3]));
        }

        // Chunk the arena so no storage BUFFER exceeds the binding limit.
        let binding_limit = ctx.device.limits().max_storage_buffer_binding_size as usize;
        let chunk_cap = (binding_limit / std::mem::size_of::<GlyphInstance>()).max(1);
        let chunks: Vec<&[GlyphInstance]> = instances.chunks(chunk_cap).collect();
        let chunk_counts: Vec<u32> = chunks.iter().map(|c| c.len() as u32).collect();
        let instance_bufs: Vec<wgpu::Buffer> = chunks
            .iter()
            .enumerate()
            .map(|(i, chunk)| {
                let label = if chunks.len() == 1 {
                    "glyph instances".to_string()
                } else {
                    format!("glyph instances {i}/{}", chunks.len())
                };
                device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some(&label),
                    contents: bytemuck::cast_slice(chunk),
                    usage: wgpu::BufferUsages::STORAGE,
                })
            })
            .collect();
        let group_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("group table"),
            contents: bytemuck::cast_slice(&groups),
            usage: wgpu::BufferUsages::STORAGE,
        });
        log::info!(
            "glyph field: {} instances ({} MiB) in {} chunk(s) of ≤{} ({} MiB binding limit), {} groups",
            instances.len(),
            (instances.len() * std::mem::size_of::<GlyphInstance>()) >> 20,
            chunks.len(),
            chunk_cap,
            binding_limit >> 20,
            groups.len(),
        );

        let camera_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("camera uniform"),
            size: std::mem::size_of::<CameraUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let params = Params {
            max_groups: groups.len() as u32,
            _pad0: 0,
            _pad1: 0,
            _pad2: 0,
            // GLYPH_LOD_DEFAULTS (GlyphField.js)
            dilate_px: 0.75,
            soften: 0.45,
            min_lo: 0.06,
            min_hi: 0.20,
        };
        let params_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("glyph params"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });

        // --- bind group ------------------------------------------------------
        let uint_tex = |binding: u32, visibility: wgpu::ShaderStages| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Uint,
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("glyph bgl"),
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
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                uint_tex(3, wgpu::ShaderStages::VERTEX),   // glyphmap
                uint_tex(4, wgpu::ShaderStages::FRAGMENT), // curves
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let bind_groups: Vec<wgpu::BindGroup> = instance_bufs
            .iter()
            .map(|buf| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("glyph bg"),
                    layout: &bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: camera_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: group_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: wgpu::BindingResource::TextureView(
                                &atlas.glyphmap.create_view(&Default::default()),
                            ),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: wgpu::BindingResource::TextureView(
                                &atlas.curves.create_view(&Default::default()),
                            ),
                        },
                        wgpu::BindGroupEntry {
                            binding: 5,
                            resource: params_buf.as_entire_binding(),
                        },
                    ],
                })
            })
            .collect();

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("glyph_field.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/glyph_field.wgsl").into()),
        });

        let depth_format = wgpu::TextureFormat::Depth32Float;
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("glyph pl"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("glyph field pipeline"),
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
                    // Premultiplied-alpha compositing: fragment outputs
                    // rgb·alpha and alpha; ONE / 1−SrcAlpha is the correct
                    // coverage composite (see shader header note).
                    blend: Some(wgpu::BlendState {
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
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: depth_format,
                // Blended coverage pass: test but don't write depth.
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState::default(), // AA is analytic
            multiview_mask: None,
            cache: None,
        });

        // Camera fit from staged bounds (or the Stage E2 focus-file override).
        let (center, half_w, half_h) = match staged.focus_bounds {
            Some((c, half)) => (
                Vec3::new(c[0], c[1], 0.0),
                half[0].max(1.0),
                half[1].max(1.0),
            ),
            None => {
                let min = staged.bounds_min;
                let max = staged.bounds_max;
                (
                    Vec3::new((min[0] + max[0]) * 0.5, (min[1] + max[1]) * 0.5, 0.0),
                    ((max[0] - min[0]) * 0.5).max(1.0),
                    ((max[1] - min[1]) * 0.5).max(1.0),
                )
            }
        };
        let fit = {
            // aspect is unknown at build time; the Front/Fly fit distance is
            // dominated by the vertical half-extent, and render() recomputes
            // the exact value per frame — this is for Fly's near/far/speed.
            let half_h_needed = half_h.max(half_w / 1.6);
            half_h_needed / (FOV_Y.to_radians() * 0.5).tan() * 1.08 + 2.0
        };
        let fly = FlyCamera::new(center + Vec3::new(0.0, 0.0, fit), fit);

        // --- Stage F: cull/LOD subsystem --------------------------------------
        let mut segments = staged.segments;
        if segments.is_empty() {
            // Safety net: one cover segment over the whole arena (the text
            // staging paths always provide one; never ship an empty table to
            // the cull pass).
            segments.push(SegCull {
                min: [staged.bounds_min[0], staged.bounds_min[1]],
                max: [staged.bounds_max[0], staged.bounds_max[1]],
                slot_base: 0,
                slot_count: instances.len() as u32,
                _pad: [0.0; 2],
                tint: seg_tint(
                    &instances,
                    staged.bounds_max[0] - staged.bounds_min[0],
                    staged.bounds_max[1] - staged.bounds_min[1],
                ),
            });
        }
        // multi_draw_indirect is core in wgpu 30 — but its Metal backend
        // silently drops any indirect draw with first_instance != 0 (see the
        // module header), so Stage F culls on the CPU instead; --no-cull
        // keeps the legacy per-chunk full draws for A/B.
        let cull = if cull_enabled {
            Some(CullState::new(
                ctx,
                color_format,
                depth_format,
                &camera_buf,
                &segments,
            ))
        } else {
            log::info!("culling disabled (--no-cull) — legacy per-chunk draws");
            None
        };

        Self {
            pipeline,
            bind_groups,
            chunk_counts,
            chunk_cap: chunk_cap as u32,
            camera_buf,
            depth_format,
            instance_count: instances.len() as u32,
            center,
            half_w,
            half_h,
            fit,
            camera_mode,
            fly,
            cull,
        }
    }

    fn camera_frame(&self, t: f32, aspect: f32) -> CamFrame {
        let fov = FOV_Y.to_radians();
        let half_h_needed = (self.half_h).max(self.half_w / aspect);
        let fit = half_h_needed / (fov * 0.5).tan() * 1.08 + 2.0;
        let (eye, target, near, far) = match self.camera_mode {
            CameraMode::Front { zoom } => {
                let d = fit / zoom.max(0.01);
                let e = self.center + Vec3::new(0.0, 0.0, d);
                (e, self.center, d * 0.01, d * 20.0)
            }
            CameraMode::Orbit => {
                let r = fit * 1.05;
                let a = t * 0.12; // slow orbit
                let e = self.center + Vec3::new(r * a.cos(), r * 0.18, r * a.sin());
                (e, self.center, r * 0.01, r * 20.0)
            }
            CameraMode::Fly => {
                let e = self.fly.eye;
                // Depth is test-only (nothing writes it), so a wide range is
                // safe; near stays small enough for single-glyph closeups.
                (e, e + self.fly.forward(), 0.05, (self.fit * 50.0).max(20_000.0))
            }
        };
        let view = Mat4::look_at_rh(eye, target, Vec3::Y);
        let proj = Mat4::perspective_rh(fov, aspect, near, far);
        CamFrame {
            view_proj: proj * view,
            eye,
        }
    }
}

impl SceneLike for GlyphScene {
    fn depth_format(&self) -> wgpu::TextureFormat {
        self.depth_format
    }

    fn instance_count(&self) -> u32 {
        self.instance_count
    }

    fn on_key(&mut self, key: winit::keyboard::KeyCode, pressed: bool) {
        if matches!(self.camera_mode, CameraMode::Fly) {
            self.fly.on_key(key, pressed);
        }
    }

    fn on_mouse_look(&mut self, dx: f32, dy: f32) {
        if matches!(self.camera_mode, CameraMode::Fly) {
            self.fly.on_look(dx, dy);
        }
    }

    fn on_scroll(&mut self, lines: f32) {
        if matches!(self.camera_mode, CameraMode::Fly) {
            self.fly.on_scroll(lines);
        }
    }

    fn tick(&mut self, dt: f32) {
        if matches!(self.camera_mode, CameraMode::Fly) {
            self.fly.tick(dt);
        }
    }

    fn render(
        &self,
        ctx: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        color_view: &wgpu::TextureView,
        depth_view: &wgpu::TextureView,
        width: u32,
        height: u32,
        t: f32,
    ) {
        let aspect = width as f32 / height.max(1) as f32;
        let frame = self.camera_frame(t, aspect);
        let cam = CameraUniform {
            view_proj: frame.view_proj.to_cols_array(),
        };
        ctx.queue
            .write_buffer(&self.camera_buf, 0, bytemuck::bytes_of(&cam));

        // --- Stage F: CPU segment cull (frustum + LOD), then range draws ----
        let mut culled_draws: Option<(Vec<Vec<std::ops::Range<u32>>>, Vec<BackdropInst>)> = None;
        if let Some(cull) = &self.cull {
            let px_scale = height as f32 / (2.0 * (FOV_Y.to_radians() * 0.5).tan());
            let planes = frustum_planes(&frame.view_proj);
            let (draws, backdrops) = cull_segments(
                &cull.segments,
                &planes,
                frame.eye,
                px_scale,
                self.chunk_cap,
                self.bind_groups.len() as u32,
            );
            if !backdrops.is_empty() {
                ctx.queue.write_buffer(
                    &cull.backdrop_insts_buf,
                    0,
                    bytemuck::cast_slice(&backdrops),
                );
            }
            if std::env::var_os("GLYPH_CULL_DEBUG").is_some() && t == 0.0 {
                let insts: u64 = draws
                    .iter()
                    .flat_map(|c| c.iter())
                    .map(|r| (r.end - r.start) as u64)
                    .sum();
                println!(
                    "CULLDBG glyph draws={} instances={} | backdrops={}",
                    draws.iter().map(|c| c.len()).sum::<usize>(),
                    insts,
                    backdrops.len(),
                );
            }
            culled_draws = Some((draws, backdrops));
        }


        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("glyph field pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: color_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.07,
                        g: 0.07,
                        b: 0.09,
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
        if let (Some(cull), Some((draws, backdrops))) = (&self.cull, &culled_draws) {
            // Far LOD stream first: one instanced draw over the compacted
            // backdrop quads (plain draw — no indirect machinery, see the
            // module header).
            if !backdrops.is_empty() {
                pass.set_pipeline(&cull.backdrop_pipeline);
                pass.set_bind_group(0, &cull.backdrop_bind_group, &[]);
                pass.draw(0..6, 0..backdrops.len() as u32);
            }
            // Glyph stream: one range draw per visible segment per chunk.
            // Ranges ascend in arena order per chunk, so within-pixel blend
            // order matches the legacy full draws exactly.
            pass.set_pipeline(&self.pipeline);
            for (bg, ranges) in self.bind_groups.iter().zip(draws.iter()) {
                if ranges.is_empty() {
                    continue;
                }
                pass.set_bind_group(0, bg, &[]);
                for r in ranges {
                    pass.draw(0..6, r.clone());
                }
            }
        } else {
            pass.set_pipeline(&self.pipeline);
            // Legacy: one instanced draw per arena chunk (chunk-local
            // instance_index). Kept as the no-feature / --no-cull fallback.
            for (bg, count) in self.bind_groups.iter().zip(self.chunk_counts.iter()) {
                pass.set_bind_group(0, bg, &[]);
                pass.draw(0..6, 0..*count);
            }
        }
    }
}
