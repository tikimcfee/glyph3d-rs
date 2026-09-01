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
//!   screen px → ray (ANALYTIC, f64: camera basis + fov/aspect — never the
//!   inverse of the f32 view-proj, whose near/far conditioning is fatal at
//!   Fly's near=0.05/far≈1.7e6; see tools/repro_pick_oblique.py)
//!   → nearest visible file AABB →
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
use glam::{DVec3, Mat4, Vec3};
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
#[derive(Clone)]
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
#[derive(Clone)]
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
        // yaw += : mouse-right rotates the view toward +X (camera right).
        // (was yaw -=, which swung the view left — inverted horizontal look)
        self.yaw += dx * SENS;
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
    /// Geometry overrides from nudge/scale-glyph verbs (slot → pos/advance/
    /// height), so a later recolor-line rebuild preserves them.
    geom_overrides: std::collections::HashMap<u32, ([f32; 3], f32, f32)>,
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
                    // Stage G: COPY_DST for partial per-slot edit uploads;
                    // COPY_SRC for the GLYPH_G_DUMP verification readback.
                    usage: wgpu::BufferUsages::STORAGE
                        | wgpu::BufferUsages::COPY_DST
                        | wgpu::BufferUsages::COPY_SRC,
                })
            })
            .collect();
        let group_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("group table"),
            contents: bytemuck::cast_slice(&groups),
            // Stage G: COPY_DST for partial per-row edit uploads (80 B/row).
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
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
                &groups,
            ))
        } else {
            log::info!("culling disabled (--no-cull) — legacy per-chunk draws");
            None
        };

        let pick = staged.pick;
        let groups_cpu = groups.clone();
        let tint_step = vec![0u32; groups.len()];

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
            instance_bufs,
            group_buf,
            groups_cpu,
            pick,
            picked: None,
            flash: None,
            geom_overrides: std::collections::HashMap::new(),
            cache: None,
            grabbed_group: None,
            cursor: (0.0, 0.0),
            viewport: Cell::new((1600, 1000)), // refreshed every render()
            tint_step,
        }
    }

    /// Camera eye/target for the mode at time `t` — the SINGLE source both
    /// the render frame and the pick ray derive from, so they always see the
    /// same camera.
    fn camera_eye_target(&self, t: f32, aspect: f32) -> (Vec3, Vec3) {
        let fov = FOV_Y.to_radians();
        let half_h_needed = (self.half_h).max(self.half_w / aspect);
        let fit = half_h_needed / (fov * 0.5).tan() * 1.08 + 2.0;
        match self.camera_mode {
            CameraMode::Front { zoom } => {
                let d = fit / zoom.max(0.01);
                (self.center + Vec3::new(0.0, 0.0, d), self.center)
            }
            CameraMode::Orbit => {
                let r = fit * 1.05;
                let a = t * 0.12; // slow orbit
                (
                    self.center + Vec3::new(r * a.cos(), r * 0.18, r * a.sin()),
                    self.center,
                )
            }
            CameraMode::Fly => (self.fly.eye, self.fly.eye + self.fly.forward()),
        }
    }

    fn camera_frame(&self, t: f32, aspect: f32) -> CamFrame {
        let fov = FOV_Y.to_radians();
        let half_h_needed = (self.half_h).max(self.half_w / aspect);
        let fit = half_h_needed / (fov * 0.5).tan() * 1.08 + 2.0;
        let (near, far) = match self.camera_mode {
            CameraMode::Front { zoom } => {
                let d = fit / zoom.max(0.01);
                (d * 0.01, d * 20.0)
            }
            CameraMode::Orbit => {
                let r = fit * 1.05;
                (r * 0.01, r * 20.0)
            }
            CameraMode::Fly => {
                // Depth is test-only (nothing writes it), so a wide range is
                // safe; near stays small enough for single-glyph closeups.
                (0.05, (self.fit * 50.0).max(20_000.0))
            }
        };
        let (eye, target) = self.camera_eye_target(t, aspect);
        // Fly: build the view from the DIRECTION directly. look_at(eye,
        // eye+fwd) forms `eye - (eye+fwd)` in f32; with eye ~1e4 world units
        // that cancellation rounds the forward vector to ~1e-3 rad, rotating
        // the whole render away from fly.forward() — the pick ray derives
        // from the same direction, so both paths must skip the roundtrip.
        // (Oracle screenshots are all the Front camera, where eye−target is
        // exact; this changes nothing they cover.)
        let view = match self.camera_mode {
            CameraMode::Fly => Mat4::look_to_rh(eye, self.fly.forward(), Vec3::Y),
            _ => Mat4::look_at_rh(eye, target, Vec3::Y),
        };
        let proj = Mat4::perspective_rh(fov, aspect, near, far);
        CamFrame {
            view_proj: proj * view,
            eye,
        }
    }

    // ── Stage G: picking & live manipulation ───────────────────────────────

    /// Group TRS from the CPU mirror: (offset, scale, rgb, alpha).
    fn group_trs(&self, gid: u32) -> Option<(Vec3, Vec3, [f32; 3], f32)> {
        let g = self.groups_cpu.get(gid as usize)?;
        Some((
            Vec3::new(g.cols[0][0], g.cols[0][1], g.cols[0][2]),
            Vec3::new(g.cols[3][0], g.cols[3][1], g.cols[3][2]),
            [g.cols[2][0], g.cols[2][1], g.cols[2][2]],
            g.cols[2][3],
        ))
    }

    fn group_hidden(&self, gid: u32) -> bool {
        self.group_trs(gid).is_some_and(|(_, _, _, a)| a <= 0.01)
    }

    /// Ensure the one-entry pick cache holds `gid`'s re-derived file data:
    /// records (bit-identical engine re-run) + the CPU byte walks. The fold
    /// cross-check (CPU row/col vs engine ROW/COL lanes, every record) is the
    /// standing pick-correctness gate and runs on every cache fill.
    fn ensure_pick_cache(&mut self, gid: u32) -> bool {
        if self.cache.as_ref().is_some_and(|c| c.group_id == gid) {
            return true;
        }
        let Some(pctx) = &self.pick else { return false };
        let Some(info) = pctx.files.iter().find(|f| f.group_id == gid) else {
            return false;
        };
        let t = std::time::Instant::now();
        let Ok((records, bytes)) =
            crate::repo::rederive_records(&pctx.root, &pctx.trie, &info.rel_path, &info.item)
        else {
            log::warn!("pick: failed to re-read/re-run {}", info.rel_path);
            return false;
        };
        let (leaders, rows, cols, lines) = crate::text::fold_leaders(&bytes, info.item.wrap_width);
        let mut mismatch = 0usize;
        if leaders.len() != records.len() {
            mismatch += 1;
        } else {
            for (i, r) in records.iter().enumerate() {
                if r.row() != rows[i] || r.col() != cols[i] {
                    mismatch += 1;
                }
            }
        }
        let colors = crate::text::colorize_leaders(&bytes);
        let mut slot_of = vec![u32::MAX; records.len()];
        let mut k = info.slot_base;
        for (i, r) in records.iter().enumerate() {
            if r.glyph_id() != 0 {
                slot_of[i] = k;
                k += 1;
            }
        }
        debug_assert_eq!(k, info.slot_base + info.slot_count);
        log::info!(
            "pick cache: {} — {} records re-derived in {:.1?} ({} B) | fold cross-check: {} ({} mismatch)",
            info.rel_path,
            records.len(),
            t.elapsed(),
            bytes.len(),
            if mismatch == 0 { "PASS" } else { "FAIL" },
            mismatch,
        );
        if mismatch != 0 {
            log::warn!("pick: fold/engine row-col mismatch on {} — char resolution unreliable", info.rel_path);
        }
        self.cache = Some(PickCacheEntry {
            group_id: gid,
            records,
            leaders,
            lines,
            colors,
            slot_of,
        });
        true
    }

    /// Build a PickGlyph for record `rec` of the cached file `gid`.
    fn make_glyph(&self, gid: u32, rec: usize) -> Option<PickGlyph> {
        let c = self.cache.as_ref().filter(|c| c.group_id == gid)?;
        let r = c.records.get(rec)?;
        let (byte_off, cp) = c.leaders[rec];
        Some(PickGlyph {
            record: rec,
            slot: (c.slot_of[rec] != u32::MAX).then_some(c.slot_of[rec]),
            row: r.row(),
            col: r.col(),
            line: c.lines[rec],
            byte_off,
            ch: char::from_u32(cp).unwrap_or('\u{FFFD}'),
            pos: [r.x(), r.y(), r.z()],
            advance: r.advance(),
            height: r.height(),
            color: c.colors.get(rec).copied().unwrap_or(0xFF_D4D4D4),
        })
    }

    /// Unproject a physical pixel to a world ray under the CURRENT camera.
    ///
    /// ANALYTIC, in f64 — deliberately NOT the inverse of the f32 view-proj.
    /// The Fly projection spans near=0.05 … far=(fit·50).max(20000); at the
    /// full-field fit (far≈1.7e6) the near/far ratio is past f32 epsilon, so
    /// the inverted matrix unprojects the far-plane point to w≈0 and EVERY
    /// pick returned None (windowed clicks always MISSed); even at
    /// far=20000 the inverse's angular error grows linearly with distance
    /// and crosses the 0.8-world-unit acceptance at D≈50 (measured; see
    /// tools/repro_pick_oblique.py). The ray is derived exactly from the
    /// same eye/target the view matrix is built from — right/up/back basis +
    /// fov/aspect — so there is no matrix to invert and no near/far
    /// conditioning at all. (The GPU's forward f32 projection of a world
    /// point differs from this ray by ≲1e-4 px — subpixel.)
    fn pixel_ray(&self, x: f32, y: f32) -> Option<(DVec3, DVec3)> {
        let (w, h) = self.viewport.get();
        if w == 0 || h == 0 {
            return None;
        }
        let aspect = w as f64 / h as f64;
        let (eye, target) = self.camera_eye_target(0.0, (w as f32) / (h as f32));
        // Forward direction WITHOUT the big-coordinate f32 roundtrip: for
        // Fly, `target` was formed as eye+fwd in f32, so eye−target loses
        // ~1e-3 rad to cancellation at field-scale coordinates — use the
        // camera's own forward (the same one camera_frame's look_to uses).
        let fwd = match self.camera_mode {
            CameraMode::Fly => self.fly.forward(),
            _ => (target - eye).normalize(),
        };
        let eye = eye.as_dvec3();
        let back = -fwd.as_dvec3().normalize(); // view z axis (backward)
        if back.length_squared() < 1e-24 {
            return None;
        }
        let right = DVec3::Y.cross(back).normalize(); // view x axis
        let up = back.cross(right); // view y axis
        let tan = (FOV_Y as f64 * 0.5).to_radians().tan();
        let nx = (x as f64 / w as f64) * 2.0 - 1.0;
        let ny = 1.0 - (y as f64 / h as f64) * 2.0;
        // View-space ray (nx·tan·aspect, ny·tan, −1) rotated to world.
        let dir = (right * (nx * tan * aspect) + up * (ny * tan) - back).normalize();
        Some((eye, dir))
    }

    /// Nearest non-hidden file whose live world AABB the ray pierces.
    fn ray_file(&self, ro: DVec3, rd: DVec3) -> Option<(u32, f64)> {
        let pctx = self.pick.as_ref()?;
        let mut best: Option<(u32, f64)> = None;
        for info in &pctx.files {
            if self.group_hidden(info.group_id) {
                continue;
            }
            let Some((off, sc, _, _)) = self.group_trs(info.group_id) else {
                continue;
            };
            let min = DVec3::new(
                info.aabb_min[0] as f64 * sc.x as f64 + off.x as f64,
                info.aabb_min[1] as f64 * sc.y as f64 + off.y as f64,
                off.z as f64 - 1.0,
            );
            let max = DVec3::new(
                info.aabb_max[0] as f64 * sc.x as f64 + off.x as f64,
                info.aabb_max[1] as f64 * sc.y as f64 + off.y as f64,
                off.z as f64 + 1.0,
            );
            if let Some(t) = ray_aabb(ro, rd, min, max) {
                if best.is_none_or(|(_, bt)| t < bt) {
                    best = Some((info.group_id, t));
                }
            }
        }
        best
    }

    /// Ray pick: nearest file AABB → ray ∩ file plane → nearest record cell.
    fn pick_ray(&mut self, x: f32, y: f32) -> Option<PickHit> {
        let dbg = std::env::var_os("GLYPH_PICK_DEBUG").is_some();
        let Some((ro, rd)) = self.pixel_ray(x, y) else {
            if dbg {
                println!("pickdbg: px ({x},{y}) — pixel_ray returned None (degenerate unprojection)");
            }
            return None;
        };
        let Some((gid, t_aabb)) = self.ray_file(ro, rd) else {
            if dbg {
                println!(
                    "pickdbg: px ({x},{y}) ro=({:.4},{:.4},{:.4}) rd=({:.6},{:.6},{:.6}) — no file AABB under the ray",
                    ro.x, ro.y, ro.z, rd.x, rd.y, rd.z
                );
            }
            return None;
        };
        let rel_path = self
            .pick
            .as_ref()?
            .files
            .iter()
            .find(|f| f.group_id == gid)?
            .rel_path
            .clone();
        let (off, sc, _, _) = self.group_trs(gid)?;
        // All glyphs live in the z = offset.z plane (group quats are identity).
        let t = if rd.z.abs() > 1e-12 {
            (off.z as f64 - ro.z) / rd.z
        } else {
            t_aabb
        };
        let p = ro + rd * t.max(0.0);
        let qx = ((p.x - off.x as f64) / (sc.x as f64).max(1e-6)) as f32;
        let qy = ((p.y - off.y as f64) / (sc.y as f64).max(1e-6)) as f32;
        if dbg {
            println!(
                "pickdbg: px ({x},{y}) ro=({:.4},{:.4},{:.4}) rd=({:.6},{:.6},{:.6}) t={t:.4} q=({qx:.4},{qy:.4}) file={rel_path}",
                ro.x, ro.y, ro.z, rd.x, rd.y, rd.z
            );
        }
        if !self.ensure_pick_cache(gid) {
            return Some(PickHit {
                group_id: gid,
                rel_path,
                glyph: None,
            });
        }
        let c = self.cache.as_ref().unwrap();
        // Nearest record cell: 2-D distance from the local point to each
        // glyph's rect [x, x+advance] × [y−h/2, y+h/2]. O(records of ONE
        // file) — microseconds for typical files, ~10 ms for a 10 MB monster.
        let mut best = (f32::MAX, 0usize);
        for (i, r) in c.records.iter().enumerate() {
            let x0 = r.x();
            let x1 = x0 + r.advance();
            let y0 = r.y() - r.height() * 0.5;
            let y1 = r.y() + r.height() * 0.5;
            let dx = (x0 - qx).max(0.0).max(qx - x1);
            let dy = (y0 - qy).max(0.0).max(qy - y1);
            let d = dx * dx + dy * dy;
            if d < best.0 {
                best = (d, i);
            }
        }
        let dist_world = best.0.sqrt() * (sc.x + sc.y) * 0.5;
        if dbg && !c.records.is_empty() {
            let r = c.records[best.1];
            println!(
                "pickdbg: nearest rec={} row={} col={} cell=({:.4},{:.4}) adv={:.4} h={:.4} dist_world={dist_world:.4} (accept ≤ 0.8)",
                best.1,
                r.row(),
                r.col(),
                r.x(),
                r.y(),
                r.advance(),
                r.height()
            );
        }
        // Accept within ~3/4 of a cell; further out it's file background.
        let glyph = if !c.records.is_empty() && dist_world <= 0.8 {
            self.make_glyph(gid, best.1)
        } else {
            None
        };
        Some(PickHit {
            group_id: gid,
            rel_path,
            glyph,
        })
    }

    /// Deterministic scripted pick: exact folded (row, col) within the first
    /// file whose path contains `file` (snaps to the nearest col on that row
    /// with a warning if there is no exact record).
    fn pick_row_col(&mut self, file: &str, row: u32, col: u32) -> Option<PickHit> {
        let pctx = self.pick.as_ref()?;
        let info = pctx.files.iter().find(|f| f.rel_path.contains(file))?;
        let gid = info.group_id;
        let rel_path = info.rel_path.clone();
        if !self.ensure_pick_cache(gid) {
            return Some(PickHit {
                group_id: gid,
                rel_path,
                glyph: None,
            });
        }
        let c = self.cache.as_ref().unwrap();
        let mut exact = None;
        let mut nearest: Option<(u32, usize)> = None;
        for (i, r) in c.records.iter().enumerate() {
            if r.row() == row {
                if r.col() == col {
                    exact = Some(i);
                    break;
                }
                let d = r.col().abs_diff(col);
                if nearest.is_none_or(|(bd, _)| d < bd) {
                    nearest = Some((d, i));
                }
            }
        }
        if exact.is_none() {
            match nearest {
                Some((_, i)) => {
                    let r = c.records[i];
                    log::warn!(
                        "pick: no exact record at row {row} col {col} in {rel_path}; \
                         snapped to row {} col {}",
                        r.row(),
                        r.col()
                    );
                }
                None => log::warn!("pick: no record on row {row} in {rel_path}"),
            }
        }
        let idx = exact.or(nearest.map(|(_, i)| i));
        let glyph = idx.and_then(|i| self.make_glyph(gid, i));
        if std::env::var_os("GLYPH_PICK_DEBUG").is_some() {
            if let (Some((off, sc, _, _)), Some(g)) = (self.group_trs(gid), &glyph) {
                println!(
                    "pickdbg: target {rel_path} rec={} local=({:.4},{:.4}) adv={:.4} h={:.4} \
                     off=({:.4},{:.4},{:.4}) sc=({:.4},{:.4}) — world cell center ({:.4},{:.4})",
                    g.record,
                    g.pos[0],
                    g.pos[1],
                    g.advance,
                    g.height,
                    off.x,
                    off.y,
                    off.z,
                    sc.x,
                    sc.y,
                    (g.pos[0] + g.advance * 0.5) * sc.x + off.x,
                    g.pos[1] * sc.y + off.y,
                );
            }
        }
        Some(PickHit {
            group_id: gid,
            rel_path,
            glyph,
        })
    }

    /// Resolve a pick command, store it as the current pick, return the log line.
    pub fn apply_pick(&mut self, _ctx: &GpuContext, cmd: &PickCommand) -> Option<String> {
        if self.pick.is_none() {
            return Some("pick: this scene has no pick context (repo mode only)".to_string());
        }
        let hit = match cmd {
            PickCommand::File(f) => {
                let pctx = self.pick.as_ref().unwrap();
                pctx.files
                    .iter()
                    .find(|i| i.rel_path.contains(f.as_str()))
                    .map(|i| PickHit {
                        group_id: i.group_id,
                        rel_path: i.rel_path.clone(),
                        glyph: None,
                    })
            }
            PickCommand::RowCol { file, row, col } => self.pick_row_col(file, *row, *col),
            PickCommand::Pixel { x, y } => self.pick_ray(*x, *y),
        };
        match hit {
            Some(h) => {
                let line = format_pick(&h);
                self.picked = Some(h);
                Some(line)
            }
            None => Some("pick: MISS (no file under the ray / no path match)".to_string()),
        }
    }

    /// Partial instance-field upload: `data` at byte `field_off` within a
    /// slot (48 B stride, 4-aligned offsets — write_buffer's requirement).
    fn write_instance(&self, ctx: &GpuContext, slot: u32, field_off: u64, data: &[u8]) {
        let chunk = (slot / self.chunk_cap) as usize;
        let local = (slot % self.chunk_cap) as u64;
        ctx.queue
            .write_buffer(&self.instance_bufs[chunk], local * 48 + field_off, data);
    }

    /// Upload one edited group row (80 B) — never the whole table.
    fn write_group_row(&self, ctx: &GpuContext, gid: u32) {
        if let Some(g) = self.groups_cpu.get(gid as usize) {
            ctx.queue
                .write_buffer(&self.group_buf, gid as u64 * 80, bytemuck::bytes_of(g));
        }
    }

    /// Re-derive a cull segment from the live group TRS: the world AABB
    /// follows offset/scale, and the backdrop tint follows the group color
    /// relative to its as-staged value (so untouched segments keep their
    /// Stage F-fitted tint exactly).
    fn sync_segment(&mut self, gid: u32) {
        let Some(g) = self.groups_cpu.get(gid as usize).copied() else {
            return;
        };
        let Some(cull) = &mut self.cull else { return };
        let i = gid as usize;
        if i >= cull.segments.len() {
            return;
        }
        let (ox, oy) = (g.cols[0][0], g.cols[0][1]);
        let (sx, sy) = (g.cols[3][0].max(0.0), g.cols[3][1].max(0.0));
        cull.segments[i].min = [
            cull.local_min[i][0] * sx + ox,
            cull.local_min[i][1] * sy + oy,
        ];
        cull.segments[i].max = [
            cull.local_max[i][0] * sx + ox,
            cull.local_max[i][1] * sy + oy,
        ];
        let bt = cull.base_tint[i];
        let orig = cull.orig_group_rgb[i];
        let mut t = bt;
        for (c, tc) in t.iter_mut().enumerate().take(3) {
            let newc = g.cols[2][c].max(0.0).powf(2.2);
            let oldc = orig[c].max(1e-6).powf(2.2);
            *tc = (bt[c] * newc / oldc).min(1.0);
        }
        cull.segments[i].tint = t;
    }

    /// Apply a manipulation verb to the current pick. All GPU writes are
    /// partial uploads; the return string is the audit log line.
    pub fn apply_verb(&mut self, ctx: &GpuContext, verb: &Verb) -> String {
        let Some(hit) = &self.picked else {
            return "verb: nothing picked yet — ignored".to_string();
        };
        let gid = hit.group_id;
        let rel = hit.rel_path.clone();
        let glyph = hit.glyph.clone();
        let pack = |rgb: [u8; 3]| -> u32 {
            rgb[0] as u32 | (rgb[1] as u32) << 8 | (rgb[2] as u32) << 16 | 0xFF00_0000
        };
        match verb {
            Verb::RecolorGlyph(rgb) => {
                let Some(g) = &glyph else {
                    return format!("verb recolor-glyph: {rel} pick has no glyph");
                };
                let Some(slot) = g.slot else {
                    return format!("verb recolor-glyph: {rel} '{}' is blank (no instance)", g.ch);
                };
                self.write_instance(ctx, slot, 24, &pack(*rgb).to_le_bytes());
                format!(
                    "verb recolor-glyph: {rel} row {} col {} slot {slot} -> #{:02x}{:02x}{:02x} (4 B)",
                    g.row, g.col, rgb[0], rgb[1], rgb[2]
                )
            }
            Verb::RecolorLine(rgb) => {
                let Some(g) = &glyph else {
                    return format!("verb recolor-line: {rel} pick has no glyph");
                };
                let row = g.row;
                if !self.ensure_pick_cache(gid) {
                    return format!("verb recolor-line: {rel} pick cache unavailable");
                }
                let c = self.cache.as_ref().unwrap();
                let packed = pack(*rgb);
                // Collect (slot, record) for the row, coalesce into runs of
                // CONTIGUOUS SLOTS, then rebuild the full 48 B instance
                // records for each run from the cache (the color field is
                // strided 48 B apart — a raw color-only byte range would
                // stomp neighboring fields; a rebuilt-instance range write
                // keeps it to ONE write_buffer per run).
                let mut runs: Vec<(u32, Vec<GlyphInstance>)> = Vec::new();
                let mut total = 0usize;
                for (i, r) in c.records.iter().enumerate() {
                    if r.row() != row {
                        continue;
                    }
                    let s = c.slot_of[i];
                    if s == u32::MAX {
                        continue;
                    }
                    total += 1;
                    let (mut pos, mut advance, mut height) =
                        ([r.x(), r.y(), r.z()], r.advance(), r.height());
                    // Preserve earlier nudge/scale-glyph edits on this slot.
                    if let Some((p, a, h)) = self.geom_overrides.get(&s) {
                        pos = *p;
                        advance = *a;
                        height = *h;
                    }
                    let inst = GlyphInstance {
                        pos,
                        glyph_id: r.glyph_id(),
                        row: r.row(),
                        col: r.col(),
                        color: packed,
                        group_id: gid,
                        advance,
                        height,
                        flags: 0,
                        _pad: 0,
                    };
                    match runs.last_mut() {
                        Some((start, insts))
                            if *start + insts.len() as u32 == s
                                && *start / self.chunk_cap == s / self.chunk_cap =>
                        {
                            insts.push(inst);
                        }
                        _ => runs.push((s, vec![inst])),
                    }
                }
                let mut bytes = 0u64;
                for (start, insts) in &runs {
                    let chunk = (*start / self.chunk_cap) as usize;
                    let local = (*start % self.chunk_cap) as u64;
                    ctx.queue.write_buffer(
                        &self.instance_bufs[chunk],
                        local * 48,
                        bytemuck::cast_slice(insts),
                    );
                    bytes += insts.len() as u64 * 48;
                }
                format!(
                    "verb recolor-line: {rel} row {row} — {total} glyphs in {} run(s), {bytes} B uploaded",
                    runs.len()
                )
            }
            Verb::NudgeGlyph(d) => {
                let Some(g) = &glyph else {
                    return format!("verb nudge-glyph: {rel} pick has no glyph");
                };
                let Some(slot) = g.slot else {
                    return format!("verb nudge-glyph: {rel} '{}' is blank (no instance)", g.ch);
                };
                let new = [g.pos[0] + d[0], g.pos[1] + d[1], g.pos[2] + d[2]];
                self.write_instance(ctx, slot, 0, bytemuck::cast_slice(&new));
                self.geom_overrides
                    .entry(slot)
                    .or_insert((new, g.advance, g.height))
                    .0 = new;
                if let Some(h) = &mut self.picked {
                    if let Some(pg) = &mut h.glyph {
                        pg.pos = new;
                    }
                }
                format!(
                    "verb nudge-glyph: {rel} slot {slot} pos -> ({:.2},{:.2},{:.2}) (12 B)",
                    new[0], new[1], new[2]
                )
            }
            Verb::ScaleGlyph(f) => {
                let Some(g) = &glyph else {
                    return format!("verb scale-glyph: {rel} pick has no glyph");
                };
                let Some(slot) = g.slot else {
                    return format!("verb scale-glyph: {rel} '{}' is blank (no instance)", g.ch);
                };
                let new_ah = [g.advance * f, g.height * f];
                self.write_instance(ctx, slot, 32, bytemuck::cast_slice(&new_ah));
                let ov = self
                    .geom_overrides
                    .entry(slot)
                    .or_insert((g.pos, g.advance, g.height));
                ov.1 = new_ah[0];
                ov.2 = new_ah[1];
                if let Some(h) = &mut self.picked {
                    if let Some(pg) = &mut h.glyph {
                        pg.advance = new_ah[0];
                        pg.height = new_ah[1];
                    }
                }
                format!(
                    "verb scale-glyph: {rel} slot {slot} advance/height -> ({:.2},{:.2}) x{f} (8 B)",
                    new_ah[0], new_ah[1]
                )
            }
            Verb::MoveGroup(d) => {
                let Some(g) = self.groups_cpu.get_mut(gid as usize) else {
                    return format!("verb move-group: group {gid} out of range");
                };
                g.cols[0][0] += d[0];
                g.cols[0][1] += d[1];
                g.cols[0][2] += d[2];
                let off = [g.cols[0][0], g.cols[0][1], g.cols[0][2]];
                self.write_group_row(ctx, gid);
                self.sync_segment(gid);
                format!(
                    "verb move-group: {rel} group {gid} offset -> ({:.1},{:.1},{:.1}) (80 B row)",
                    off[0], off[1], off[2]
                )
            }
            Verb::ScaleGroup(f) => {
                let Some(g) = self.groups_cpu.get_mut(gid as usize) else {
                    return format!("verb scale-group: group {gid} out of range");
                };
                for c in 0..3 {
                    g.cols[3][c] = (g.cols[3][c] * f).clamp(0.001, 100.0);
                }
                let s = g.cols[3][0];
                self.write_group_row(ctx, gid);
                self.sync_segment(gid);
                format!("verb scale-group: {rel} group {gid} scale -> {s:.3} (80 B row)")
            }
            Verb::TintGroup(rgb) => {
                let Some(g) = self.groups_cpu.get_mut(gid as usize) else {
                    return format!("verb tint-group: group {gid} out of range");
                };
                g.cols[2][0] = rgb[0];
                g.cols[2][1] = rgb[1];
                g.cols[2][2] = rgb[2];
                self.write_group_row(ctx, gid);
                self.sync_segment(gid);
                format!(
                    "verb tint-group: {rel} group {gid} tint -> ({:.2},{:.2},{:.2}) (80 B row)",
                    rgb[0], rgb[1], rgb[2]
                )
            }
            Verb::TintCycle => {
                let i = gid as usize;
                let step = self.tint_step.get(i).copied().unwrap_or(0) + 1;
                if i < self.tint_step.len() {
                    self.tint_step[i] = step;
                }
                let rgb = crate::repo::DIR_TINTS[(step as usize) % crate::repo::DIR_TINTS.len()];
                let line = self.apply_verb(ctx, &Verb::TintGroup(rgb));
                format!("{line} [palette step {step}]")
            }
            Verb::SetHidden(hide) => {
                let Some(g) = self.groups_cpu.get_mut(gid as usize) else {
                    return format!("verb hide/show: group {gid} out of range");
                };
                g.cols[2][3] = if *hide { 0.0 } else { 1.0 };
                if let Some(cull) = &mut self.cull {
                    if (gid as usize) < cull.hidden.len() {
                        cull.hidden[gid as usize] = *hide;
                    }
                }
                self.write_group_row(ctx, gid);
                format!(
                    "verb {}: {rel} group {gid} (alpha -> {}, 80 B row; cull skips the segment)",
                    if *hide { "hide-group" } else { "show-group" },
                    g_alpha(self.groups_cpu.get(gid as usize)),
                )
            }
            Verb::ToggleHidden => {
                let hidden = self.group_hidden(gid);
                self.apply_verb(ctx, &Verb::SetHidden(!hidden))
            }
        }
    }

    /// Scriptable Fly-camera pose (offscreen repro of oblique windowed
    /// picks): switches the camera to Fly and pins eye/yaw/pitch. Offscreen
    /// mode never ticks the camera, so the pose holds for every op and frame.
    pub fn set_cam_pose(&mut self, eye: [f32; 3], yaw: f32, pitch: f32) {
        self.camera_mode = CameraMode::Fly;
        self.fly.eye = Vec3::new(eye[0], eye[1], eye[2]);
        self.fly.yaw = yaw;
        self.fly.pitch = pitch;
        self.fly.vel = Vec3::ZERO;
        self.fly.keys = 0;
        log::info!(
            "cam pose: eye=({:.2},{:.2},{:.2}) yaw={yaw:.4} pitch={pitch:.4} (Fly)",
            eye[0],
            eye[1],
            eye[2]
        );
    }

    /// Windowed click: restore the previous flash, pick at the pixel, flash
    /// the new glyph (bright yellow), return the pick log line.
    pub fn click_pick(&mut self, ctx: &GpuContext, x: f32, y: f32) -> Option<String> {
        if let Some((slot, old)) = self.flash.take() {
            self.write_instance(ctx, slot, 24, &old.to_le_bytes());
        }
        let line = self.apply_pick(ctx, &PickCommand::Pixel { x, y });
        let target = self
            .picked
            .as_ref()
            .and_then(|h| h.glyph.as_ref())
            .and_then(|g| g.slot.map(|s| (s, g.color)));
        if let Some((slot, old_color)) = target {
            let flash_packed: u32 = 255 | 240 << 8 | 120 << 16 | 0xFF00_0000;
            self.write_instance(ctx, slot, 24, &flash_packed.to_le_bytes());
            self.flash = Some((slot, old_color));
        }
        line
    }

    /// Windowed cursor move: while a group is grabbed (`g`), drag it in the
    /// view plane through its AABB center.
    pub fn cursor_moved(&mut self, ctx: &GpuContext, x: f32, y: f32) {
        let prev = self.cursor;
        self.cursor = (x, y);
        let Some(gid) = self.grabbed_group else { return };
        if (x - prev.0).abs() + (y - prev.1).abs() < 1e-3 {
            return;
        }
        let (Some((o0, d0)), Some((o1, d1))) =
            (self.pixel_ray(prev.0, prev.1), self.pixel_ray(x, y))
        else {
            return;
        };
        let (w, h) = self.viewport.get();
        let Some((_, fwd)) = self.pixel_ray(w as f32 * 0.5, h as f32 * 0.5) else {
            return;
        };
        let Some((off, sc, _, _)) = self.group_trs(gid) else {
            return;
        };
        let center_local = self
            .pick
            .as_ref()
            .and_then(|p| p.files.iter().find(|f| f.group_id == gid))
            .map(|i| {
                [
                    (i.aabb_min[0] + i.aabb_max[0]) * 0.5,
                    (i.aabb_min[1] + i.aabb_max[1]) * 0.5,
                ]
            });
        let Some(cl) = center_local else {
            self.grabbed_group = None;
            return;
        };
        let c = DVec3::new(
            cl[0] as f64 * sc.x as f64 + off.x as f64,
            cl[1] as f64 * sc.y as f64 + off.y as f64,
            off.z as f64,
        );
        let hit_plane = |o: DVec3, d: DVec3| -> Option<DVec3> {
            let denom = d.dot(fwd);
            if denom.abs() < 1e-12 {
                None
            } else {
                Some(o + d * ((c - o).dot(fwd) / denom))
            }
        };
        let (Some(p0), Some(p1)) = (hit_plane(o0, d0), hit_plane(o1, d1)) else {
            return;
        };
        let delta = p1 - p0;
        if let Some(g) = self.groups_cpu.get_mut(gid as usize) {
            g.cols[0][0] += delta.x as f32;
            g.cols[0][1] += delta.y as f32;
            g.cols[0][2] += delta.z as f32;
        }
        self.write_group_row(ctx, gid);
        self.sync_segment(gid);
    }

    /// Windowed scroll: scales the grabbed group; otherwise camera speed.
    fn scroll_or_scale(&mut self, ctx: &GpuContext, lines: f32) {
        if let Some(gid) = self.grabbed_group {
            let f = 1.1f32.powf(lines);
            let mut s = 0.0;
            if let Some(g) = self.groups_cpu.get_mut(gid as usize) {
                for c in 0..3 {
                    g.cols[3][c] = (g.cols[3][c] * f).clamp(0.001, 100.0);
                }
                s = g.cols[3][0];
            }
            self.write_group_row(ctx, gid);
            self.sync_segment(gid);
            println!("grab: group {gid} scale -> {s:.3}");
        } else if matches!(self.camera_mode, CameraMode::Fly) {
            self.fly.on_scroll(lines);
        }
    }

    /// Windowed verb keys: h highlight line, g grab/release file, t cycle
    /// tint, x toggle hidden.
    fn verb_key(&mut self, ctx: &GpuContext, key: winit::keyboard::KeyCode) {
        use winit::keyboard::KeyCode as K;
        match key {
            K::KeyH => {
                let line = self.apply_verb(ctx, &Verb::RecolorLine([255, 213, 79]));
                println!("{line}");
            }
            K::KeyT => {
                let line = self.apply_verb(ctx, &Verb::TintCycle);
                println!("{line}");
            }
            K::KeyX => {
                let line = self.apply_verb(ctx, &Verb::ToggleHidden);
                println!("{line}");
            }
            K::KeyG => match self.grabbed_group {
                Some(gid) => {
                    self.grabbed_group = None;
                    println!("grab: released group {gid}");
                }
                None => match &self.picked {
                    Some(h) => {
                        self.grabbed_group = Some(h.group_id);
                        println!(
                            "grab: {} (group {}) — mouse drags it in the view plane, scroll scales, g releases",
                            h.rel_path, h.group_id
                        );
                    }
                    None => println!("grab: nothing picked (click a file first)"),
                },
            },
            _ => {}
        }
    }
}

/// Slab ray-AABB test; returns the entry t (0 when the origin is inside).
fn ray_aabb(ro: DVec3, rd: DVec3, min: DVec3, max: DVec3) -> Option<f64> {
    let mut t0 = 0.0f64;
    let mut t1 = f64::MAX;
    for ax in 0..3 {
        let (o, d, lo, hi) = (ro[ax], rd[ax], min[ax], max[ax]);
        if d.abs() < 1e-15 {
            if o < lo || o > hi {
                return None;
            }
        } else {
            let inv = 1.0 / d;
            let (mut ta, mut tb) = ((lo - o) * inv, (hi - o) * inv);
            if ta > tb {
                std::mem::swap(&mut ta, &mut tb);
            }
            t0 = t0.max(ta);
            t1 = t1.min(tb);
            if t0 > t1 {
                return None;
            }
        }
    }
    Some(t0)
}

/// One-line pick report: file, group, folded row/col, source line, byte
/// offset, the actual character, and the arena slot.
fn format_pick(h: &PickHit) -> String {
    match &h.glyph {
        Some(g) => format!(
            "pick: {} group={} rec={} row={} col={} line={} byte={} char={:?} slot={} pos=({:.2},{:.2},{:.2})",
            h.rel_path,
            h.group_id,
            g.record,
            g.row,
            g.col,
            g.line,
            g.byte_off,
            g.ch,
            g.slot.map_or("-".to_string(), |s| s.to_string()),
            g.pos[0],
            g.pos[1],
            g.pos[2],
        ),
        None => format!(
            "pick: {} group={} (file-level pick, no glyph resolved)",
            h.rel_path, h.group_id
        ),
    }
}

fn g_alpha(g: Option<&GroupRow>) -> f32 {
    g.map_or(f32::NAN, |g| g.cols[2][3])
}

impl SceneLike for GlyphScene {
    fn depth_format(&self) -> wgpu::TextureFormat {
        self.depth_format
    }

    fn instance_count(&self) -> u32 {
        self.instance_count
    }

    fn on_key(&mut self, ctx: &GpuContext, key: winit::keyboard::KeyCode, pressed: bool) {
        if matches!(self.camera_mode, CameraMode::Fly) {
            self.fly.on_key(key, pressed);
        }
        if pressed {
            self.verb_key(ctx, key);
        }
    }

    fn on_mouse_look(&mut self, _ctx: &GpuContext, dx: f32, dy: f32) {
        if matches!(self.camera_mode, CameraMode::Fly) {
            self.fly.on_look(dx, dy);
        }
    }

    fn on_scroll(&mut self, ctx: &GpuContext, lines: f32) {
        // Stage G: scroll scales a grabbed group; otherwise camera speed.
        self.scroll_or_scale(ctx, lines);
    }

    fn on_cursor(&mut self, ctx: &GpuContext, x: f32, y: f32) {
        self.cursor_moved(ctx, x, y);
    }

    fn on_click(&mut self, ctx: &GpuContext, x: f32, y: f32) {
        if let Some(line) = self.click_pick(ctx, x, y) {
            println!("{line}");
        }
    }

    fn set_viewport(&mut self, w: u32, h: u32) {
        self.viewport.set((w, h));
    }

    fn set_cam_pose(&mut self, eye: [f32; 3], yaw: f32, pitch: f32) {
        GlyphScene::set_cam_pose(self, eye, yaw, pitch);
    }

    fn apply_pick(&mut self, ctx: &GpuContext, cmd: &PickCommand) -> Option<String> {
        GlyphScene::apply_pick(self, ctx, cmd)
    }

    fn apply_verb(&mut self, ctx: &GpuContext, verb: &Verb) -> Option<String> {
        Some(GlyphScene::apply_verb(self, ctx, verb))
    }

    fn debug_dump_instances(&self, ctx: &GpuContext, slot: u64, out: &mut [u32]) {
        let chunk = (slot as u32 / self.chunk_cap) as usize;
        let local = (slot as u32 % self.chunk_cap) as u64;
        let size = (out.len() * 4) as u64;
        let buf = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("debug dump"),
            size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = ctx.device.create_command_encoder(&Default::default());
        enc.copy_buffer_to_buffer(&self.instance_bufs[chunk], local * 48, &buf, 0, size);
        ctx.queue.submit([enc.finish()]);
        let slice = buf.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        ctx.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .expect("debug dump poll failed");
        rx.recv().expect("dump cb dropped").expect("dump map failed");
        let data = slice.get_mapped_range().expect("dump range");
        out.copy_from_slice(bytemuck::cast_slice(&data[..size as usize]));
        drop(data);
        buf.unmap();
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
        self.viewport.set((width, height));
        let frame = self.camera_frame(t, aspect);
        let cam = CameraUniform {
            view_proj: frame.view_proj.to_cols_array(),
        };
        ctx.queue
            .write_buffer(&self.camera_buf, 0, bytemuck::bytes_of(&cam));

        // --- Stage F: CPU segment cull (frustum + LOD), then range draws ----
        let mut culled_draws: Option<(Vec<Vec<std::ops::Range<u32>>>, Vec<BackdropInst>)> = None;
        if let Some(cull) = &self.cull {
            // Stage H: CPU scope timing (only when GLYPH_PROFILE=1 built a profiler).
            let cull_t0 = ctx.profiler.as_ref().map(|_| std::time::Instant::now());
            let px_scale = height as f32 / (2.0 * (FOV_Y.to_radians() * 0.5).tan());
            let planes = frustum_planes(&frame.view_proj);
            let (draws, backdrops) = cull_segments(
                &cull.segments,
                &cull.hidden,
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
            if let Some(t0) = cull_t0 {
                crate::gpu::record_cpu_scope(ctx, "cull (CPU)", t0.elapsed().as_secs_f64() * 1000.0);
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

        // Stage H: pass-level GPU timer (TIMESTAMP_QUERY; pass-boundary writes,
        // so it works on Metal). Nested in-pass scopes below additionally need
        // TIMESTAMP_QUERY_INSIDE_PASSES — where unsupported they simply report
        // no time. Queries must always be closed, timing or not.
        let pass_query = ctx
            .profiler
            .as_ref()
            .map(|p| p.borrow().begin_pass_query("glyph field pass", encoder));
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("glyph field pass"),
            timestamp_writes: pass_query
                .as_ref()
                .and_then(|q| q.render_pass_timestamp_writes()),
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
                let q = ctx
                    .profiler
                    .as_ref()
                    .map(|p| p.borrow().begin_query("backdrop stream", &mut pass));
                pass.set_pipeline(&cull.backdrop_pipeline);
                pass.set_bind_group(0, &cull.backdrop_bind_group, &[]);
                pass.draw(0..6, 0..backdrops.len() as u32);
                if let (Some(p), Some(q)) = (&ctx.profiler, q) {
                    p.borrow().end_query(&mut pass, q);
                }
            }
            // Glyph stream: one range draw per visible segment per chunk.
            // Ranges ascend in arena order per chunk, so within-pixel blend
            // order matches the legacy full draws exactly.
            let q = ctx
                .profiler
                .as_ref()
                .map(|p| p.borrow().begin_query("glyph stream", &mut pass));
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
            if let (Some(p), Some(q)) = (&ctx.profiler, q) {
                p.borrow().end_query(&mut pass, q);
            }
        } else {
            let q = ctx
                .profiler
                .as_ref()
                .map(|p| p.borrow().begin_query("glyph stream", &mut pass));
            pass.set_pipeline(&self.pipeline);
            // Legacy: one instanced draw per arena chunk (chunk-local
            // instance_index). Kept as the no-feature / --no-cull fallback.
            for (bg, count) in self.bind_groups.iter().zip(self.chunk_counts.iter()) {
                pass.set_bind_group(0, bg, &[]);
                pass.draw(0..6, 0..*count);
            }
            if let (Some(p), Some(q)) = (&ctx.profiler, q) {
                p.borrow().end_query(&mut pass, q);
            }
        }
        drop(pass);
        if let (Some(p), Some(q)) = (&ctx.profiler, pass_query) {
            p.borrow().end_query(encoder, q);
        }
    }
}
