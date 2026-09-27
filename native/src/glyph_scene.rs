//! Stage C — glyph field scene: Slug pipeline, group table, text camera.
//! Stage F — fly camera + two-level CPU culling with LOD backdrops (the GPU
//! indirect-draw design is documented as broken-in-wgpu-30 below).
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
//!
//! Culling is visually lossless: a culled segment is entirely outside the
//! frustum, and the LOD tier only substitutes subpixel glyphs.
//!
//! Stage L (L2): the cull output is organized as PHASE LISTS (`enum Phase`
//! with `PhaseDraws` — re_renderer's DrawPhase borrow). Recording iterates
//! phases in declaration order (Backdrop, then Glyphs — exactly the order
//! above); behavior and profiler query names are unchanged.
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
use crate::layout::{GlyphRecord, ItemParams};
use crate::gpu::GpuContext;
use crate::scene::SceneLike;
use crate::text::StagedText;

/// Vertical field of view shared by every glyph-scene camera mode.
/// (pub since Stage K (K5): the Debug panel's file browser mirrors the
/// Front-camera framing formula for click-to-fly navigation.)
pub const FOV_Y: f32 = 40f32;

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
/// Stage H: encase ShaderType derive — generated WGSL-layout size/offsets,
/// asserted against the bytemuck wire format in `layout_tests` below (the
/// Stage G strided-color bug class, caught at compile/test time).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, encase::ShaderType)]
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
#[derive(Clone, Copy, Pod, Zeroable, encase::ShaderType)]
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
    /// P1-live: envelope-owned content. `None` (the default, and every
    /// disk-loaded scene) re-derives from `root` on disk as Stage G always
    /// did. `Some(map)` re-derives from the CALLER's bytes — the ones the
    /// seam folded — which is what makes the version join meaningful for
    /// live content: the hash is of the same bytes the fold consumed.
    /// Injected by the consumer (fieldzed), not by `into_staged`.
    pub content: Option<std::collections::HashMap<String, std::sync::Arc<Vec<u8>>>>,
    /// P2a: the scene's fold set — normalized LINE ranges per rel_path, the
    /// same ones `load_items` compacted with. The style walk (and later the
    /// pick walk) must skip folded records so its slot sequence matches the
    /// COMPACTED instances; walking uncompacted records against compacted
    /// slots paints wrong glyphs and runs past the file's slot range (the
    /// white-glyph state, seen live 2026-09-26). Injected by the consumer
    /// beside `content`; empty = no folds (the default).
    pub folds: std::collections::HashMap<String, Vec<std::ops::Range<u32>>>,
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
    /// Global arena slot per record; u32::MAX for blank/missing (no instance).
    slot_of: Vec<u32>,
}

/// Per-segment cull record, 48 B. One segment per FILE in repo mode; text
/// scenes stage a single segment covering the whole block. Bounds are
/// WORLD-space with the group offset already applied.
///
/// NOT a GPU struct, despite the `Pod` derive and the comment this replaced,
/// which claimed it mirrored a `SegCull` in cull.wgsl. It does not: that shader
/// declares only `BackdropInst` and `Camera`, culling is entirely CPU-side
/// (`cull_segments`), and this type is never uploaded to any buffer — no
/// `write_buffer`, no `cast_slice`, no test pinning its layout. The derive and
/// the old `_pad` lane were vestigial.
///
/// The z lanes replace that `_pad`, so the struct is still 48 B and `tint` is
/// still at offset 32. They exist because the old code did not merely lack
/// depth — it ASSERTED a false one, testing every segment as though it spanned
/// z ∈ [-1, 1]. That was true while all instances lived in the z=0 plane, and
/// WrapBack made it false by spending wraps in depth.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct SegCull {
    pub min: [f32; 3],
    pub max: [f32; 3],
    pub slot_base: u32,
    pub slot_count: u32,
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
///
/// `slot_ink` (`Atlas::slot_ink`) says what a BITMAP slot's pixels average
/// to; an emoji contributes that instead of its instance colour — which is
/// the syntax colour, a colour it does not display — and counts as the two
/// cells its advance covers. Outline glyphs are summed exactly as before, so
/// a segment without emoji tints to the same bits (the goldens hold).
pub fn seg_tint(instances: &[GlyphInstance], width: f32, height: f32, slot_ink: &[Option<[f32; 4]>]) -> [f32; 4] {
    let mut sum = [0f64; 3];
    let mut cells = 0usize;
    for g in instances {
        if let Some(Some(ink)) = slot_ink.get(g.glyph_id as usize) {
            for (i, s) in sum.iter_mut().enumerate() {
                *s += ink[i] as f64;
            }
            cells += 2;
            continue;
        }
        // Match the shader's decode: sRGB display bytes → linear via pow 2.2.
        for (i, s) in sum.iter_mut().enumerate() {
            let byte = ((g.color >> (8 * i)) & 0xFF) as f64 / 255.0;
            *s += byte.powf(2.2);
        }
        cells += 1;
    }
    let n = instances.len().max(1) as f64;
    let area = (width as f64 * height as f64).max(1e-3);
    let ink_frac = (cells as f64 * GLYPH_CELL_AREA as f64 / area).min(1.0);
    let e = (ink_frac * BACKDROP_GAIN as f64).min(1.0);
    [
        (sum[0] / n) as f32,
        (sum[1] / n) as f32,
        (sum[2] / n) as f32,
        e as f32,
    ]
}

/// Stage L (L1): the frame uniform — was CameraUniform (just view_proj).
/// re_renderer's FrameUniformBuffer borrow: everything a frame needs behind
/// the one reserved binding. `view_proj` stays at offset 0, byte-for-byte
/// the same 64 B; the appended lanes bind to the UNCHANGED WGSL uniform
/// block (glyph_field.wgsl `struct Camera` = 64 B minimum binding size ≤
/// this 104 B buffer — no shader bytes move; the extra lanes are consumed by
/// a future stage that changes the shader anyway). `flags` bit 0 is reserved
/// `deterministic_rendering` (re_renderer's RenderMode::Deterministic idea);
/// 0 everywhere today — nothing consumes it yet.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, encase::ShaderType)]
struct FrameUniform {
    view_proj: [f32; 16],
    eye: [f32; 3],
    _pad0: f32,
    viewport: [f32; 2],
    px_scale: f32,
    time: f32,
    flags: u32,
    _pad1: u32,
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
    /// Emoji sheet geometry, mirrored from the G3ES header (atlas.rs) so the
    /// vertex stage can place a cell from its index alone. WGSL `vec2<u32>` /
    /// `vec2<f32>` are 8-byte aligned; this 32-byte tail keeps the struct at a
    /// 16-byte multiple (64 B).
    emoji_cell: [u32; 2],
    emoji_cols: u32,
    emoji_rows: u32,
    emoji_layer: [f32; 2],
    _pad3: [u32; 2],
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

/// Stage L (L2): draw phases — re_renderer's DrawPhase borrow (a flat enum
/// partitioning draw order, per-phase work lists, no render graph).
/// Recording iterates phases in declaration order: Backdrop first, Glyphs
/// second — exactly the Stage F order. Stage L (L4) adds Selection: it
/// renders selected glyph quads into a separate MASK target after the glyph
/// field pass — own target ⇒ own pass, so it is not an in-pass arm; the
/// variant marks the phase ordering (Selection renders after Glyphs) and
/// gates the mask pass. (`Overlay` arrives WITH its phase; no dead
/// variants — Stage J rule.)
enum Phase {
    /// Far-LOD backdrop quads (one instanced draw over the compacted list).
    Backdrop,
    /// The glyph instances (per-chunk range draws).
    Glyphs,
    /// Stage L (L4): the selection mask (selected segments' glyph quads).
    Selection,
}

/// Stage L (L2): one frame's draw work, partitioned by phase. Built during
/// cull (culled path) or straight from the chunk counts (legacy --no-cull
/// path).
struct PhaseDraws {
    /// Backdrop phase: the compacted far-LOD quads.
    backdrops: Vec<BackdropInst>,
    /// Glyphs phase: (chunk, chunk-local slot range), CHUNK-major —
    /// ascending chunk, arena-ascending ranges within a chunk — so the
    /// record order (and thus the within-pixel blend order) is identical to
    /// the pre-L2 per-chunk loops.
    glyph_ranges: Vec<(u32, std::ops::Range<u32>)>,
}

/// Per-frame, view-derived cull inputs (everything the segment table doesn't
/// provide). Bundled so `cull_segments` stays under the argument-count lint.
struct CullView {
    planes: [[f32; 4]; 6],
    eye: Vec3,
    /// px per world unit at distance 1 (height / 2·tan(fov/2)).
    px_scale: f32,
    /// Stage K (K4): the LOD threshold (px/em) is per-frame data (was the
    /// LOD_MIN_PX const read directly) so windowed runs can tune it live;
    /// offscreen always carries the const (see CullState::lod_min_px).
    lod_min_px: f32,
}

/// Stage F — CPU cull: frustum + LOD over the segment table. Returns the
/// frame's draw work as phase lists (Stage L, L2): the Backdrop phase's
/// compacted quad list and the Glyphs phase's (chunk, chunk-local range)
/// list — chunk-major, arena-ascending within a chunk, so blending matches
/// the legacy full draws exactly. See the module header for the contract
/// and for why this runs on the CPU. Stage G: `hidden` (parallel to
/// `segments`, empty = nothing hidden) skips user-hidden groups entirely —
/// no glyph draws AND no backdrop.
fn cull_segments(
    segments: &[SegCull],
    hidden: &[bool],
    view: &CullView,
    chunk_cap: u32,
    chunk_count: u32,
) -> PhaseDraws {
    let CullView { planes, eye, px_scale, lod_min_px } = *view;
    let mut draws: Vec<Vec<std::ops::Range<u32>>> =
        (0..chunk_count).map(|_| Vec::new()).collect();
    let mut backdrops = Vec::new();
    for (si, seg) in segments.iter().enumerate() {
        if hidden.get(si).copied().unwrap_or(false) {
            continue;
        }
        // Frustum: positive-vertex test per plane, over the segment's real AABB.
        // The z lane used to be the constant ±1 — true while every instance
        // lived in the z=0 plane, false since WrapBack began spending wraps in
        // depth, and wrong in the direction that KEEPS what it should drop.
        let mut visible = true;
        for pl in planes {
            let px = if pl[0] >= 0.0 { seg.max[0] } else { seg.min[0] };
            let py = if pl[1] >= 0.0 { seg.max[1] } else { seg.min[1] };
            let pz = if pl[2] >= 0.0 { seg.max[2] } else { seg.min[2] };
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
        // Clamped into the segment, like x and y. Clamping into the old ±1 slab
        // computed the distance to a place the segment is not, understating it
        // whenever the content had depth — so far content measured near, and
        // was drawn at full detail instead of collapsing to a backdrop.
        let nz = eye.z.clamp(seg.min[2], seg.max[2]);
        let dist = ((eye.x - nx).powi(2) + (eye.y - ny).powi(2) + (eye.z - nz).powi(2))
            .sqrt()
            .max(0.001);
        let glyph_px = px_scale / dist;
        if glyph_px < lod_min_px {
            if seg.slot_count > 0 {
                backdrops.push(BackdropInst {
                    // Backdrops are flat quads; BackdropInst IS a GPU struct
                    // and stays 2D. Only the cull arithmetic needs depth.
                    min: [seg.min[0], seg.min[1]],
                    max: [seg.max[0], seg.max[1]],
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
    // Stage L (L2): flatten chunk-major into the Glyphs phase list — the
    // per-chunk vectors are already segment/arena-ascending, so the flat
    // list's record order matches the pre-L2 loops exactly.
    let glyph_ranges = draws
        .into_iter()
        .enumerate()
        .flat_map(|(c, rs)| rs.into_iter().map(move |r| (c as u32, r)))
        .collect();
    PhaseDraws { backdrops, glyph_ranges }
}

/// Extract the 6 frustum planes from a view-proj matrix (Gribb-Hartmann;
/// wgpu clip space has z ∈ `[0,w]`, so the near plane is row2, not row3+row2).
/// glam's to_cols_array is column-major: row r = `(m[r], m[4+r], m[8+r], m[12+r])`.
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

// ── Stage K: windowed debug-UI probe ─────────────────────────────────────
// The windowed egui Debug panel (K3) needs read-only scene state, but
// windowed.rs holds the scene type-erased as `Box<dyn SceneLike>` and fence
// 4 forbids trait changes. The probe is the channel: a shared cell installed
// on the CONCRETE GlyphScene before boxing (see build_scene_probed in
// main.rs), written once per frame by render(), read by the panel.
// Offscreen never installs one, so the determinism chain never touches it.
// Single-threaded: winit's event-loop thread owns both writer and reader.

/// Read-only snapshot displayed by the windowed debug panel.
#[derive(Clone, Default)]
pub struct UiProbeState {
    /// None until the first probed frame has rendered.
    pub camera_mode: Option<CameraMode>,
    /// The eye position actually used for this frame's cull/projection.
    pub eye: [f32; 3],
    /// Fly-camera angles (windowed always runs Fly).
    pub yaw: f32,
    pub pitch: f32,
    /// The last resolved pick, formatted by the same `format_pick` as the
    /// stdout pick log line.
    pub last_pick: Option<String>,
    // ── K4: UI → scene controls. Written by the Debug-panel sliders;
    // applied by render() before culling. Seeded from the compile-time
    // consts at install; offscreen never installs a probe, so the consts
    // rule there. ──
    /// Live LOD threshold in px/em (const default: LOD_MIN_PX = 1.0).
    pub lod_min_px: f32,
    // ── K4: live cull readouts (scene → UI; the same sums GLYPH_CULL_DEBUG
    // prints). Zero when culling is disabled (--no-cull). ──
    pub cull_ranges: usize,
    pub cull_instances: u64,
    pub cull_backdrops: usize,
    // ── Layout dial (repo scenes): the wrap staircase's pitch. Unlike the
    // K4 controls this is LAYOUT, not a per-frame cull input — applying it
    // re-runs load_repo and rebuilds the scene (windowed.rs's
    // pending_relayout arm), so the panel fires on drag RELEASE, never per
    // tick (the JS system's `grid.layout` semantics: a discrete refold
    // command). Seeded at install from the files' actual z_step; written by
    // the panel's slider; read by nothing per frame. None for non-repo
    // scenes ⇒ the panel hides the section. ──
    pub z_wrap_spacing: Option<f64>,
    /// Field depth extent (world z over the cull segments), refreshed per
    /// frame — the quantified readout of what the dial did. None under
    /// --no-cull (no segment table).
    pub z_extent: Option<[f32; 2]>,
    /// The scene's cluster mode, for the panel's toggle label. Seeded at
    /// install — repo scenes from the pick context's uniform ItemParams,
    /// text scenes from the staging choice (GlyphScene::probe_cluster_mode);
    /// None where the scene carries no mode (demo, engine-text) ⇒ the panel
    /// hides the toggle.
    pub cluster_mode: Option<bool>,
    // ── K5: group-browser data. `files` is STATIC (built once at install;
    // Rc-shared so the panel's per-frame snapshot clones a refcount, not the
    // rows). `file_dyn` is refreshed per frame (world pose under the live
    // group TRS, hidden flag, tint) — parallel to `files`. ──
    pub files: std::rc::Rc<Vec<UiFileRow>>,
    pub file_dyn: Vec<UiFileDyn>,
}

/// Stage K (K5): one static group-browser row — file identity + the
/// local-space (pre-TRS) AABB (same margins as the cull segment; the world
/// pose derives from the live group TRS each frame, see UiFileDyn).
#[derive(Clone)]
pub struct UiFileRow {
    pub rel_path: String,
    pub group_id: u32,
    pub aabb_min: [f32; 2],
    pub aabb_max: [f32; 2],
}

/// Stage K (K5): per-frame dynamic row state for the group browser.
#[derive(Clone, Copy, Default)]
pub struct UiFileDyn {
    /// World-space center/half extents under the live group TRS.
    pub center: [f32; 2],
    pub half: [f32; 2],
    /// From the group row's alpha (the same place the hide/show verbs write),
    /// so it is correct even under --no-cull.
    pub hidden: bool,
    /// Group tint as sRGB bytes (`cols[2]` is display-space — TintGroup verbs
    /// store normalized sRGB there).
    pub tint: [u8; 3],
}

/// Shared probe cell: GlyphScene writes, the egui panel reads.
pub type UiProbe = std::rc::Rc<std::cell::RefCell<UiProbeState>>;

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
    local_min: Vec<[f32; 3]>,
    local_max: Vec<[f32; 3]>,
    base_tint: Vec<[f32; 4]>,
    /// Group color rgb at staging time — tint edits scale the backdrop by
    /// pow(new)/pow(orig) so an untouched segment keeps its Stage F tint.
    orig_group_rgb: Vec<[f32; 3]>,
    hidden: Vec<bool>,
    /// Stage K (K4): live LOD threshold (px/em), seeded from the LOD_MIN_PX
    /// const. Cell because render(&self) is immutable (the `viewport: Cell`
    /// precedent). The ONLY write site is the UI-controls application in
    /// render(), which runs solely when a windowed probe is installed —
    /// offscreen never writes it, so offscreen culls with the const.
    lod_min_px: Cell<f32>,
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
            local_min.push([seg.min[0] - off[0], seg.min[1] - off[1], seg.min[2]]);
            local_max.push([seg.max[0] - off[0], seg.max[1] - off[1], seg.max[2]]);
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
        // uniform, same premultiplied blend and the SAME depth state as the
        // glyph pass: test AND write, LessEqual. This used to test without
        // writing, on the premise that all glyph-plane content lives at z=0
        // and segments do not overlap, so draw order between streams was
        // immaterial. `--wrap-mode back` (the default since dde3f82) ended
        // that premise: a wrapped line steps BACK in z, so a long file's
        // column recedes behind the pages beside it, and with nothing writing
        // depth the later-drawn file simply painted over the nearer one —
        // measured 2026-09-07, wide.txt's column over long.md's pages from a
        // front-on camera pitched down. LessEqual rather than Less keeps
        // every coplanar fragment passing, so within a z=0 page the blend
        // order is exactly what it was.
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
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState {
                count: SCENE_SAMPLE_COUNT, // Stage L (L3): loud non-MSAA pin
                ..Default::default()
            },
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
            lod_min_px: Cell::new(LOD_MIN_PX),
            backdrop_insts_buf,
            backdrop_pipeline,
            backdrop_bind_group,
        }
    }
}

pub struct GlyphScene {
    pub pipeline: wgpu::RenderPipeline,
    /// The colour-emoji sheet's view, held so the texture outlives the bind
    /// groups that sample it (binding 6 of every chunk's bind group).
    _emoji_view: wgpu::TextureView,
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
    /// The panel's cluster-toggle seed for scenes WITHOUT a pick context
    /// (text): the staging choice carries the mode and nothing else on the
    /// scene remembers it. Repo scenes leave this None — their probe seeds
    /// from the pick context's uniform ItemParams (the two agree there).
    probe_cluster_mode: Option<bool>,
    /// The last resolved pick (verbs operate on it).
    picked: Option<PickHit>,
    /// Stage L (L4): the current selection (drives the mask pass). Replaces
    /// the Stage G click-flash hack (instance-byte write + restore) — no
    /// buffer writes, nothing to restore; the tint lives entirely in the
    /// windowed composite path.
    selection: Option<Selection>,
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
    /// Stage K: windowed debug-UI probe (None offscreen / under --no-ui).
    ui_probe: Option<UiProbe>,
    /// Stage L (L3): device handle for pool (re)creation in set_viewport
    /// (which has no ctx param — the trait shape is fenced).
    device: wgpu::Device,
    /// Stage L (L3): composite machinery — the ONLY release path (the
    /// --no-composite A/B escape hatch proved neutrality and was removed at
    /// stage end; see out/STAGE_L_REPORT.md).
    composite: CompositeState,
}

// ── Stage K (K4): what the live controls change, and what stays const ────
//
// LOD_MIN_PX becomes live in windowed runs via `CullState::lod_min_px`
// (a Cell seeded from the const — the `viewport: Cell` precedent for
// render(&self) immutability). The Debug-panel slider writes the shared
// probe cell; render() copies it into the Cell before culling. That copy is
// the SINGLE write site, and it runs only when a probe is installed
// (windowed) — offscreen never installs one, so offscreen culls with the
// const by construction (the byte-equal PNG gates prove it).
//
// BACKDROP_GAIN stays a compile-time const. The K4 handoff assumed it lived
// in the Params uniform — it does NOT: Params carries only the Slug
// minification dials (glyph_field.wgsl), while the gain is baked into
// SegCull.tint's alpha at STAGING time by seg_tint (text.rs/repo.rs share
// that path). A live gain would need either a cull.wgsl edit (fence 4) or
// an ink_frac plumbing redesign across staging + sync_segment. Cut from K4;
// the feasible future seam is recorded in out/STAGE_K_REPORT.md.

// ── Stage L (L3): pooled view target + composite (the ViewBuilder borrow) ──
//
// re_renderer's ViewBuilder renders each view into a pooled target, then
// composites into whatever pass the host provides. L3 lands the skeleton for
// GlyphScene only (the demo Scene stays direct — it is the minimal template
// by design): the phase lists draw into a ping-pong pair of POOL_FORMAT
// textures + one depth, then composite into the driver's view:
//   - same format (offscreen oracle, Rgba8UnormSrgb):
//     copy_texture_to_texture — 1:1, no scaling, bit-exact BY CONSTRUCTION;
//   - different format (windowed, Bgra8UnormSrgb — component-order
//     incompatible with the pool, so a copy is invalid):
//     a fullscreen shader composite through composite.wgsl.
// The split exists for the component-order copy incompatibility; a future
// scaled/sub-rect composite (minimap inset) extends the shader path only.
// egui wrinkle (recorded, not solved): register_native_texture demands
// Rgba8Unorm (NON-sRGB) — an egui-hosted view would want its own non-sRGB
// pool or a conversion pass.

/// Pooled view-target format: matches the offscreen oracle's target exactly.
const POOL_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Both scene pipelines (glyph + backdrop) are non-MSAA. The pool's copy
/// composite is invalid on multisample targets — this was a silent
/// assumption before L3; now it is loud at both ends (the pipeline
/// multisample states and ViewTarget::new's assert).
const SCENE_SAMPLE_COUNT: u32 = 1;

/// The persistent half of the composite: pipeline (targets the DRIVER's
/// color format), bind group layout, sampler. `target` is sized by
/// set_viewport; `parity` selects which pool texture the frame draws into.
struct CompositeState {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    target: Option<ViewTarget>,
    parity: Cell<u8>,
    /// Stage L (L4): selection mask/tint machinery — only on the
    /// shader-composite path (windowed). None when the driver composites by
    /// copy (offscreen), which therefore never renders selection visuals.
    selection_fx: Option<SelectionFx>,
}

/// The per-size half: the ping-pong pair of pool textures (re_renderer's
/// DynamicResourcePool is reference material, not a dependency — two
/// textures, not a pool), one depth, and the composite bind groups that
/// sample each pool texture.
struct ViewTarget {
    width: u32,
    height: u32,
    colors: [wgpu::Texture; 2],
    color_views: [wgpu::TextureView; 2],
    depth: wgpu::TextureView,
    bind_groups: [wgpu::BindGroup; 2],
    /// Stage L (L4): the selection mask target + per-pool-slot tint bind
    /// groups. Only created on the shader-composite path (windowed) — the
    /// copy path (offscreen) never renders selection visuals, so offscreen
    /// stays byte-identical by construction.
    mask: Option<MaskSet>,
}

/// Stage L (L4): the mask target for the selection pass. (No texture field:
/// a wgpu TextureView keeps its texture alive internally.)
struct MaskSet {
    view: wgpu::TextureView,
    /// Tint-pass bind groups, one per pool slot: `pool[slot]` + mask + tint
    /// uniform.
    tint_bgs: [wgpu::BindGroup; 2],
}

/// Stage L (L4): selection state — replaces the Stage G click-flash hack
/// (instance-byte write + restore). Set by apply_pick (the exact API the
/// CLI op-stream and the windowed click share): a glyph pick with a real
/// slot selects that glyph; a file-level pick (or a blank-glyph pick, which
/// has no slot) selects the whole segment; a pick MISS clears. Verbs never
/// touch it. Persistent until the next pick — this closes the "sticky
/// flash" gap.
enum Selection {
    /// One glyph slot (arena-global split into chunk + chunk-local index).
    Glyph { chunk: u32, local: u32 },
    /// A whole segment/file (arena slot range; split per chunk at draw).
    Segment { slot_base: u32, slot_count: u32 },
}

/// Stage L (L4): the selection tint (warm yellow, 45% additive) — the
/// flash's bright-yellow legacy, but as a coverage-weighted tint that keeps
/// the glyph readable underneath.
const SELECTION_TINT: [f32; 4] = [1.0, 0.85, 0.25, 0.45];

/// Stage L (L4): mask/tint pass resources (windowed shader path only).
struct SelectionFx {
    /// Glyph geometry drawn into the mask: same glyph_field.wgsl and the
    /// same per-chunk bind groups, blend disabled, Rgba8Unorm target.
    mask_pipeline: wgpu::RenderPipeline,
    /// composite.wgsl's fs_tint: pool + mask → pool (ping-pong), additive
    /// tint.
    tint_pipeline: wgpu::RenderPipeline,
    tint_bgl: wgpu::BindGroupLayout,
    /// Static tint uniform (written once at creation).
    tint_buf: wgpu::Buffer,
}

/// Stage L (L4): mask target format. Rgba8Unorm (not the sRGB pool format):
/// the mask is data (coverage in alpha), and the mask pipeline needs a
/// non-pool format to coexist with the glyph pipeline's sRGB target.
const MASK_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

impl ViewTarget {
    fn new(
        device: &wgpu::Device,
        width: u32,
        height: u32,
        bgl: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        selection_fx: Option<&SelectionFx>,
    ) -> Self {
        assert_eq!(
            SCENE_SAMPLE_COUNT, 1,
            "L3: copy composite requires non-MSAA scene pipelines/targets"
        );
        let colors: [wgpu::Texture; 2] = std::array::from_fn(|i| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(if i == 0 { "view target A" } else { "view target B" }),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: SCENE_SAMPLE_COUNT,
                dimension: wgpu::TextureDimension::D2,
                format: POOL_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
        });
        // TextureView is an owned handle (Arc inside) — no borrow ties.
        let color_views: [wgpu::TextureView; 2] =
            std::array::from_fn(|i| colors[i].create_view(&Default::default()));
        let bind_groups: [wgpu::BindGroup; 2] = std::array::from_fn(|i| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(if i == 0 { "composite bg A" } else { "composite bg B" }),
                layout: bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&color_views[i]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
            })
        });
        let depth = crate::scene::create_depth(device, wgpu::TextureFormat::Depth32Float, width, height);
        // Stage L (L4): the selection mask target + tint bind groups
        // (shader-composite path only — the caller passes the fx only then).
        let mask = selection_fx.map(|fx| {
            let mask_texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("selection mask"),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: MASK_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = mask_texture.create_view(&Default::default());
            let tint_bgs = std::array::from_fn(|i| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some(if i == 0 { "tint bg A" } else { "tint bg B" }),
                    layout: &fx.tint_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&color_views[i]),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(sampler),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::TextureView(&view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: fx.tint_buf.as_entire_binding(),
                        },
                    ],
                })
            });
            MaskSet { view, tint_bgs }
        });
        Self { width, height, colors, color_views, depth, bind_groups, mask }
    }
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

        // Stage L (L3): the glyph/backdrop pipelines render into the POOL
        // format (Rgba8UnormSrgb); the composite step maps the pool into the
        // driver's view. (The --no-composite direct path proved the pool
        // draw byte-neutral and was removed at stage end.)

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
            "emoji sheet bound: {} cells, {} mip levels, {:.1} MiB",
            atlas.emoji.sheet.cells.len(),
            atlas.emoji.mip_levels,
            atlas.emoji.texture_bytes as f64 / (1 << 20) as f64,
        );
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
            // Stage L (L1): the widened FrameUniform buffer (104 B) — binds to
            // the unchanged 64 B WGSL block via the minimum-binding-size rule.
            label: Some("frame uniform"),
            size: std::mem::size_of::<FrameUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let emoji_view = atlas.emoji.texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("emoji sheet view"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let sheet = &atlas.emoji.sheet;
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
            emoji_cell: [sheet.cell_w, sheet.cell_h],
            emoji_cols: sheet.cols,
            emoji_rows: sheet.rows_per_layer,
            emoji_layer: [sheet.layer_w as f32, sheet.layer_h as f32],
            _pad3: [0, 0],
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
            label: Some("glyph field bgl"),
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
                // The emoji sheet: a filterable sRGB 2D array + its sampler.
                wgpu::BindGroupLayoutEntry {
                    binding: 6,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 7,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        // Trilinear, clamped: the UV rect is inset half a texel so clamping
        // never engages inside a cell; it only guards the sheet's padding.
        let emoji_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("emoji sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        // Stage L (O2): enumerate so captures can tell chunk bind groups
        // apart (mirrors the "glyph instances i/N" buffer labels).
        let bind_group_count = instance_bufs.len();
        let bind_groups: Vec<wgpu::BindGroup> = instance_bufs
            .iter()
            .enumerate()
            .map(|(i, buf)| {
                let label = if bind_group_count == 1 {
                    "glyph bg".to_string()
                } else {
                    format!("glyph bg {i}/{bind_group_count}")
                };
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some(&label),
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
                        wgpu::BindGroupEntry {
                            binding: 6,
                            resource: wgpu::BindingResource::TextureView(&emoji_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 7,
                            resource: wgpu::BindingResource::Sampler(&emoji_sampler),
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
            label: Some("glyph field pl"),
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
                    // Stage L (L3): the POOL format (the composite pass
                    // targets the driver's format instead).
                    format: POOL_FORMAT,
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
                // Blended coverage pass that WRITES depth. It used to test
                // only, which made glyph-vs-glyph visibility a fact about
                // draw order — fine while every glyph sat at z=0, wrong the
                // moment `--wrap-mode back` put wrapped segments behind the
                // page plane (see the backdrop pipeline's note above). The
                // cost of writing: a glyph's ≤1 px coverage fringe also
                // writes depth, so where a NEARER glyph is drawn before a
                // farther one, the farther one's ink under that fringe is
                // rejected rather than blended — a faint halo, at edges,
                // only where two glyphs at different depths overlap on
                // screen. The wrong-order alternative was whole glyphs.
                // LessEqual: coplanar fragments still pass, so a flat page
                // blends in exactly the order it did before.
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState {
                count: SCENE_SAMPLE_COUNT, // Stage L (L3): loud non-MSAA pin
                ..Default::default()
            }, // AA is analytic
            multiview_mask: None,
            cache: None,
        });

        // Stage L (L4): the selection mask pipeline — same glyph_field.wgsl,
        // same layout (the per-chunk bind groups work unchanged), only the
        // target format (MASK_FORMAT — data, not sRGB) and blend (coverage
        // overwrite) differ. Shader path only: the copy path (offscreen)
        // never renders selection visuals. `shader`/`layout` above are the
        // glyph pipeline's — the composite block below shadows those names.
        let mask_pipeline = (color_format != POOL_FORMAT).then(|| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("selection mask pipeline"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: MASK_FORMAT,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: None, // the mask is flat 2D coverage
                multisample: wgpu::MultisampleState {
                    count: SCENE_SAMPLE_COUNT,
                    ..Default::default()
                },
                multiview_mask: None,
                cache: None,
            })
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
                min: staged.bounds_min,
                max: staged.bounds_max,
                slot_base: 0,
                slot_count: instances.len() as u32,
                tint: seg_tint(
                    &instances,
                    staged.bounds_max[0] - staged.bounds_min[0],
                    staged.bounds_max[1] - staged.bounds_min[1],
                    &atlas.slot_ink,
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
                POOL_FORMAT, // Stage L (L3): the backdrop pipeline renders into the pool
                depth_format,
                &camera_buf,
                &segments,
                &groups,
            ))
        } else {
            log::info!("culling disabled (--no-cull) — legacy per-chunk draws");
            None
        };

        // Stage L (L3): the composite machinery (persistent half). The
        // pooled target itself is sized by set_viewport — GlyphScene::new
        // doesn't know the viewport (the drivers decide it later).
        let composite = {
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("composite.wgsl"),
                source: wgpu::ShaderSource::Wgsl(include_str!("shaders/composite.wgsl").into()),
            });
            let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("composite bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            multisampled: false,
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("composite pl"),
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            });
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("composite pipeline"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[], // fullscreen triangle from vertex_index
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        // The DRIVER's format (e.g. Bgra8UnormSrgb windowed).
                        format: color_format,
                        // Exact-overwrite passthrough — NOT a blend.
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
            let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("composite sampler"),
                // 1:1 samples land on texel centers — exact; Linear is for
                // future scaled composites (minimap inset).
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            });
            // Stage L (L4): the tint half of the selection machinery
            // (mask_pipeline was built next to the glyph pipeline, before
            // this block shadowed `shader`/`layout` with the composite's).
            // Shader path only.
            let selection_fx = mask_pipeline.map(|mask_pipeline| {
                let tint_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("tint bgl"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                multisampled: false,
                                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                                view_dimension: wgpu::TextureViewDimension::D2,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 2,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                multisampled: false,
                                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                                view_dimension: wgpu::TextureViewDimension::D2,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 3,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Uniform,
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                    ],
                });
                let tint_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("tint pl"),
                    bind_group_layouts: &[Some(&tint_bgl)],
                    immediate_size: 0,
                });
                let tint_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("selection tint pipeline"),
                    layout: Some(&tint_layout),
                    vertex: wgpu::VertexState {
                        module: &shader, // composite.wgsl (this block's `shader`)
                        entry_point: Some("vs_main"),
                        compilation_options: Default::default(),
                        buffers: &[],
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some("fs_tint"),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: POOL_FORMAT, // tints pool A into pool B
                            blend: None,
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    primitive: wgpu::PrimitiveState::default(),
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    multiview_mask: None,
                    cache: None,
                });
                let tint_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("selection tint"),
                    contents: bytemuck::cast_slice(&[SELECTION_TINT]),
                    usage: wgpu::BufferUsages::UNIFORM,
                });
                SelectionFx { mask_pipeline, tint_pipeline, tint_bgl, tint_buf }
            });
            CompositeState {
                pipeline,
                bind_group_layout: bgl,
                sampler,
                target: None,
                parity: Cell::new(0),
                selection_fx,
            }
        };

        let pick = staged.pick;
        let groups_cpu = groups.clone();
        let tint_step = vec![0u32; groups.len()];

        Self {
            pipeline,
            _emoji_view: emoji_view,
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
            probe_cluster_mode: None,
            picked: None,
            selection: None,
            geom_overrides: std::collections::HashMap::new(),
            cache: None,
            grabbed_group: None,
            cursor: (0.0, 0.0),
            viewport: Cell::new((1600, 1000)), // refreshed every render()
            tint_step,
            ui_probe: None,
            device: device.clone(),
            composite,
        }
    }

    /// Seed for the panel's cluster toggle on scenes without a pick context
    /// (text). Called by the scene builder between `new` and `init_ui_probe`.
    pub fn set_probe_cluster_mode(&mut self, on: bool) {
        self.probe_cluster_mode = Some(on);
    }

    /// Stage K: install and return the windowed debug-UI probe. Windowed mode
    /// calls this on the concrete scene BEFORE boxing it as
    /// `Box<dyn SceneLike>` (build_scene_probed); offscreen never does, so
    /// the write in render() stays inert there.
    pub fn init_ui_probe(&mut self) -> UiProbe {
        // K4: the UI→scene controls are seeded from the compile-time consts,
        // so a windowed run starts bit-identical to an offscreen one.
        // K5: the browser's static rows come from the pick context (repo
        // scenes; empty for text/engine scenes → the panel hides the
        // browser). Dynamic row state is filled on the first render.
        let files: Vec<UiFileRow> = self
            .pick
            .as_ref()
            .map(|pctx| {
                pctx.files
                    .iter()
                    .map(|f| UiFileRow {
                        rel_path: f.rel_path.clone(),
                        group_id: f.group_id,
                        aabb_min: f.aabb_min,
                        aabb_max: f.aabb_max,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let file_dyn = vec![UiFileDyn::default(); files.len()];
        // The layout dial's seed: every repo file shares one z_step
        // (repo::file_item_params computes it from the same
        // RepoParams::z_wrap_spacing), so any file speaks for the field.
        // Non-repo scenes have no pick context → None → the panel hides
        // the section.
        let z_wrap_spacing = self
            .pick
            .as_ref()
            .and_then(|p| p.files.first())
            .map(|f| f.item.z_step / crate::text::CELL_HEIGHT_WORLD as f64);
        // The toggle's seed: the mode the scene was built with. Repo scenes
        // read it off the pick context's uniform-per-field params (the same
        // read as the dial's seed); text scenes carry it on
        // `probe_cluster_mode` instead (no pick context there). Demo and
        // engine-text scenes: None — the panel hides the toggle.
        let cluster_mode = self
            .pick
            .as_ref()
            .and_then(|p| p.files.first())
            .map(|f| f.item.cluster_mode == crate::fold::ClusterMode::Cluster)
            .or(self.probe_cluster_mode);
        let probe = UiProbe::new(std::cell::RefCell::new(UiProbeState {
            lod_min_px: LOD_MIN_PX,
            files: std::rc::Rc::new(files),
            file_dyn,
            z_wrap_spacing,
            cluster_mode,
            ..Default::default()
        }));
        self.ui_probe = Some(probe.clone());
        probe
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
            CameraMode::Fly => {
                glam::camera::rh::view::look_to_mat4(eye, self.fly.forward(), Vec3::Y)
            }
            _ => glam::camera::rh::view::look_at_mat4(eye, target, Vec3::Y),
        };
        let proj = glam::camera::rh::proj::directx::perspective(fov, aspect, near, far);
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

    /// S3 spike (`experiments/zedspike`): apply a highlight SIDECAR produced by
    /// Zed's headless language stack to this scene's per-glyph instance colors.
    /// One run per line — `rel_path<TAB>start<TAB>end<TAB>rrggbb`, byte offsets
    /// into the file, end exclusive. Legacy spike path: NO version law (the
    /// sidecar is positional by construction); the CONTRACT path is
    /// [`Self::apply_surface_updates`], which checks `seam::joins` against the
    /// re-derived bytes. Reached only via the offscreen `--highlight` op.
    pub fn apply_highlight_sidecar(&self, ctx: &GpuContext, path: &std::path::Path) -> String {
        let map = match parse_highlight_sidecar(path) {
            Ok(map) => map,
            Err(e) => return format!("highlight: {e}"),
        };
        let Some(pctx) = &self.pick else {
            return "highlight: no pick context (repo scenes only) — ignored".to_string();
        };
        let mut files = 0usize;
        let mut colored = 0usize;
        let mut unstyled = 0usize;
        for info in &pctx.files {
            if let Some(runs) = map.get(&info.rel_path) {
                files += 1;
                if let FileStyle::Ok { colored: c, unstyled: u } =
                    self.style_file(ctx, info, runs, None)
                {
                    colored += c;
                    unstyled += u;
                }
            }
        }
        format!(
            "highlight: {} file(s), {} glyphs colored, {} left default — {}",
            files,
            colored,
            unstyled,
            path.display()
        )
    }

    /// P1c — the seam's CONTRACT consumer: apply provider envelopes
    /// (`seam::SurfaceUpdate`) to this scene. For each update whose file is in
    /// the field, the file's bytes are re-derived and hashed; the update is
    /// applied iff `seam::joins(hash, update)` — the version law, made
    /// load-bearing in the renderer. Mismatches are DROPPED and counted, never
    /// translated. Structure/decorations planes are accepted and ignored until
    /// their stages ship (the anti-sprawl law: one walk, fields arrive later).
    pub fn apply_surface_updates(
        &self,
        ctx: &GpuContext,
        updates: &[crate::seam::SurfaceUpdate],
    ) -> String {
        let Some(pctx) = &self.pick else {
            return "seam: no pick context (repo scenes only) — ignored".to_string();
        };
        let mut files = 0usize;
        let mut colored = 0usize;
        let mut unstyled = 0usize;
        let mut dropped = 0usize;
        for update in updates {
            let Some(info) = pctx.files.iter().find(|f| f.rel_path == update.file.0) else {
                dropped += 1; // not in the field (yet) — the workspace grammar's job later
                continue;
            };
            files += 1;
            match self.style_file(ctx, info, &update.style, Some(update.version)) {
                FileStyle::Ok { colored: c, unstyled: u } => {
                    colored += c;
                    unstyled += u;
                }
                FileStyle::VersionMismatch => dropped += 1,
                FileStyle::Failed => {}
            }
        }
        format!(
            "seam: {} file(s), {} glyphs colored, {} left default, {} dropped (version/file) — {} update(s)",
            files,
            colored,
            unstyled,
            dropped,
            updates.len()
        )
    }

    /// Style one file: re-derive its records + fold leaders (ONE rederive
    /// serves both the version hash and the walk), check the version if the
    /// caller has one, then write per-glyph colors — `Verb::RecolorGlyph`'s
    /// 4 B partial-write mechanism, iterated with a merge pointer
    /// (O(runs + records); both walks are byte-ascending). Blank records take
    /// no slot, exactly as `ensure_pick_cache` skips them.
    fn style_file(
        &self,
        ctx: &GpuContext,
        info: &PickFileInfo,
        runs: &[crate::seam::StyleRun],
        expected: Option<crate::seam::BufferVersion>,
    ) -> FileStyle {
        let Some(pctx) = &self.pick else {
            return FileStyle::Failed;
        };
        // Bytes' provenance: envelope-owned content first (the bytes the seam
        // folded), disk second (Stage G's original semantics). One code path
        // either way — only the read differs.
        let bytes: std::sync::Arc<Vec<u8>> = if let Some(owned) = pctx
            .content
            .as_ref()
            .and_then(|m| m.get(&info.rel_path))
            .cloned()
        {
            owned
        } else {
            match std::fs::read(pctx.root.join(&info.rel_path)) {
                Ok(bytes) => std::sync::Arc::new(bytes),
                Err(_) => {
                    log::warn!("seam/style: failed to read {}", info.rel_path);
                    return FileStyle::Failed;
                }
            }
        };
        if let Some(expected) = expected {
            // The seam's law (seam::joins is this comparison over a whole
            // envelope): equality or drop. The folded version here is the
            // content hash of the re-derived bytes — the file-driven
            // provider convention (seam::content_hash_version).
            if crate::seam::content_hash_version(&bytes) != expected {
                log::warn!(
                    "seam/style: version mismatch on {} — update dropped, not translated",
                    info.rel_path
                );
                return FileStyle::VersionMismatch;
            }
        }
        // The engine re-run, on the THREAD-CACHED rederiver (repo::
        // rederive_cached — one Engine + trie per thread, outliving scenes:
        // the live loop rebuilds a scene per edit, so a scene-lifetime cache
        // would never amortize). The FFI resets the arena per load ("reuse
        // the arena across loads", ffi.mojo). This is the P1-live
        // measurement's named fix: uncached, this step paid ~120 ms FIXED
        // (Engine::new + trie parse) per file per apply.
        let records = match crate::repo::rederive_cached(&pctx.trie, &bytes, &info.item) {
            Ok(records) => records,
            Err(_) => {
                log::warn!("seam/style: engine re-run failed on {}", info.rel_path);
                return FileStyle::Failed;
            }
        };
        let (leaders, _, _, _) =
            crate::text::fold_leaders(&bytes, info.item.wrap_width, info.item.wrap_mode);
        if leaders.len() != records.len() {
            log::warn!(
                "seam/style: leader/record count mismatch on {} — file skipped",
                info.rel_path
            );
            return FileStyle::Failed;
        }
        // P2a: skip folded records so this walk's slot sequence matches the
        // COMPACTED instances the loader staged. Walking uncompacted records
        // against compacted slots painted wrong glyphs and ran past the
        // file's slot range (the white-glyph state, seen live 2026-09-26).
        let file_folds = pctx.folds.get(&info.rel_path);
        let starts = file_folds.map(|_| crate::repo::line_starts_of(&bytes));
        let folded = |i: usize| -> bool {
            match (&file_folds, &starts) {
                (Some(folds), Some(starts)) => {
                    let line = crate::repo::line_of_byte(leaders[i].0, starts);
                    folds.iter().any(|f| f.contains(&line))
                }
                _ => false,
            }
        };
        // The walk, COALESCED — the RecolorLine lesson: one queue.write_buffer
        // per glyph is ~15 µs of validation each, which is where the uncached
        // style plane's ~130 ms per file actually went (the cached engine was
        // necessary but not sufficient). Full 48 B instances are rebuilt per
        // CONTIGUOUS slot run — few writes per file. A record no run covers
        // is written back at the default color: a re-style after an edit must
        // not leave the previous version's colors on the glyphs between runs.
        let mut batches: Vec<(u32, Vec<GlyphInstance>)> = Vec::new();
        let push = |slot: u32, inst: GlyphInstance, batches: &mut Vec<(u32, Vec<GlyphInstance>)>| {
            match batches.last_mut() {
                Some((start, insts))
                    if *start + insts.len() as u32 == slot
                        && *start / self.chunk_cap == slot / self.chunk_cap =>
                {
                    insts.push(inst);
                }
                _ => batches.push((slot, vec![inst])),
            }
        };
        let mut run_ix = 0usize;
        let mut slot = info.slot_base;
        let mut colored = 0usize;
        let mut unstyled = 0usize;
        for (i, r) in records.iter().enumerate() {
            if r.glyph_id() == 0 || folded(i) {
                continue; // blank: no slot; folded: dropped at compaction —
                          // either way it takes no slot in THIS field
            }
            let byte = leaders[i].0;
            while run_ix < runs.len() && runs[run_ix].range.end <= byte {
                run_ix += 1;
            }
            // runs[run_ix] is the first run ending past `byte`; it covers
            // the record iff it also starts at/before it.
            let (packed, hit) = match runs.get(run_ix).filter(|run| byte >= run.range.start) {
                Some(run) => (
                    u32::from(run.rgb[0])
                        | u32::from(run.rgb[1]) << 8
                        | u32::from(run.rgb[2]) << 16
                        | 0xFF00_0000,
                    true,
                ),
                None => (crate::layout::DEFAULT_COLOR_PACKED, false),
            };
            if hit {
                colored += 1;
            } else {
                unstyled += 1;
            }
            let (mut pos, mut advance, mut height) =
                ([r.x(), r.y(), r.z()], r.advance(), r.height());
            // Preserve earlier nudge/scale-glyph edits on this slot.
            if let Some((p, a, h)) = self.geom_overrides.get(&slot) {
                pos = *p;
                advance = *a;
                height = *h;
            }
            push(
                slot,
                GlyphInstance {
                    pos,
                    glyph_id: r.glyph_id(),
                    row: r.row(),
                    col: r.col(),
                    color: packed,
                    group_id: info.group_id,
                    advance,
                    height,
                    flags: 0,
                    _pad: 0,
                },
                &mut batches,
            );
            slot += 1;
        }
        for (start, insts) in &batches {
            let chunk = (*start / self.chunk_cap) as usize;
            let local = (*start % self.chunk_cap) as u64;
            ctx.queue
                .write_buffer(&self.instance_bufs[chunk], local * 48, bytemuck::cast_slice(insts));
        }
        FileStyle::Ok { colored, unstyled }
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
        let (leaders, rows, cols, lines) =
            crate::text::fold_leaders(&bytes, info.item.wrap_width, info.item.wrap_mode);
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
        let c = self.cache.as_ref().expect("cache populated: ensure_pick_cache just returned true");
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
        let c = self.cache.as_ref().expect("cache populated: ensure_pick_cache just returned true");
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
                let pctx = self.pick.as_ref().expect("pick context checked Some at apply_pick entry");
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
                // Stage L (L4): drive the selection mask. A glyph pick with
                // a real slot selects that glyph; a file-level pick (or a
                // blank glyph, which has no slot) selects the whole segment.
                self.selection = self.selection_from_hit(&h);
                let line = format_pick(&h);
                self.picked = Some(h);
                Some(line)
            }
            None => {
                // Stage L (L4): a miss CLEARS the selection (click on empty
                // space = deselect). `picked` is untouched — verb semantics
                // (act on the most recent pick) are unchanged.
                self.selection = None;
                Some("pick: MISS (no file under the ray / no path match)".to_string())
            }
        }
    }

    /// Stage L (L4): pick hit → selection mask content.
    fn selection_from_hit(&self, h: &PickHit) -> Option<Selection> {
        if let Some(g) = &h.glyph {
            if let Some(slot) = g.slot {
                return Some(Selection::Glyph {
                    chunk: slot / self.chunk_cap,
                    local: slot % self.chunk_cap,
                });
            }
        }
        // File-level (or blank-glyph) pick: the whole segment's slot range.
        self.pick
            .as_ref()?
            .files
            .iter()
            .find(|f| f.group_id == h.group_id)
            .map(|f| Selection::Segment { slot_base: f.slot_base, slot_count: f.slot_count })
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
        // Group TRS is xy only (cols[3] carries no z scale), so depth passes
        // through untransformed — which is what the old code did implicitly by
        // having no z lane at all.
        cull.segments[i].min = [
            cull.local_min[i][0] * sx + ox,
            cull.local_min[i][1] * sy + oy,
            cull.local_min[i][2],
        ];
        cull.segments[i].max = [
            cull.local_max[i][0] * sx + ox,
            cull.local_max[i][1] * sy + oy,
            cull.local_max[i][2],
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
                let c = self.cache.as_ref().expect("cache populated: ensure_pick_cache just returned true");
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

    /// Windowed click: pick at the pixel and return the log line. Stage L
    /// (L4): the click-flash hack is gone — `apply_pick` now drives the
    /// selection mask instead (no instance-byte writes, nothing to restore).
    pub fn click_pick(&mut self, ctx: &GpuContext, x: f32, y: f32) -> Option<String> {
        self.apply_pick(ctx, &PickCommand::Pixel { x, y })
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

/// Outcome of styling one file — `style_file`'s report. `VersionMismatch`
/// is kept DISTINCT from `Failed` so the envelope consumer's audit line can
/// say how many updates the seam law dropped.
enum FileStyle {
    Ok { colored: usize, unstyled: usize },
    VersionMismatch,
    Failed,
}

/// Parse the sidecar: `rel_path<TAB>start<TAB>end<TAB>rrggbb` per line, a
/// leading `#` on the color tolerated, blank lines skipped, runs kept in file
/// order. Emits [`crate::seam::StyleRun`]s — the seam's run type — so the
/// spike path and the envelope path share one law. Hand-rolled on purpose:
/// the renderer carries no JSON dependency, and this format exists to cross
/// the zedspike → renderer seam, not to be a public contract.
fn parse_highlight_sidecar(
    path: &std::path::Path,
) -> Result<std::collections::HashMap<String, Vec<crate::seam::StyleRun>>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut map: std::collections::HashMap<String, Vec<crate::seam::StyleRun>> =
        std::collections::HashMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim_end_matches(['\r']);
        if line.is_empty() {
            continue;
        }
        let bad = |what: &str| format!("{}:{}: {what}", path.display(), n + 1);
        let mut parts = line.split('\t');
        let (rel, start, end, rgb) = (
            parts.next().ok_or_else(|| bad("missing rel_path"))?,
            parts.next().ok_or_else(|| bad("missing start"))?,
            parts.next().ok_or_else(|| bad("missing end"))?,
            parts.next().ok_or_else(|| bad("missing color"))?,
        );
        if parts.next().is_some() {
            return Err(bad("extra field"));
        }
        let start: usize = start.trim().parse().map_err(|_| bad("start not a number"))?;
        let end: usize = end.trim().parse().map_err(|_| bad("end not a number"))?;
        let hex = rgb.trim().trim_start_matches('#');
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad("color not rrggbb hex"));
        }
        let rgb = [
            u8::from_str_radix(&hex[0..2], 16).map_err(|_| bad("color not rrggbb hex"))?,
            u8::from_str_radix(&hex[2..4], 16).map_err(|_| bad("color not rrggbb hex"))?,
            u8::from_str_radix(&hex[4..6], 16).map_err(|_| bad("color not rrggbb hex"))?,
        ];
        map.entry(rel.to_string()).or_default().push(crate::seam::StyleRun {
            range: start..end,
            rgb,
        });
    }
    Ok(map)
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
        // Stage L (L3): (re)size the pooled view target to the viewport.
        // Both drivers call set_viewport before the first render (windowed:
        // on window creation and every resize; offscreen: once at startup).
        let comp = &mut self.composite;
        let stale = comp
            .target
            .as_ref()
            .is_none_or(|t| t.width != w || t.height != h);
        if stale && w > 0 && h > 0 {
            let target = ViewTarget::new(
                &self.device,
                w,
                h,
                &comp.bind_group_layout,
                &comp.sampler,
                comp.selection_fx.as_ref(), // Stage L (L4): mask + tint bind groups
            );
            comp.target = Some(target);
        }
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

    fn apply_highlight_sidecar(&self, ctx: &GpuContext, path: &std::path::Path) -> Option<String> {
        Some(GlyphScene::apply_highlight_sidecar(self, ctx, path))
    }

    fn apply_surface_updates(
        &self,
        ctx: &GpuContext,
        updates: &[crate::seam::SurfaceUpdate],
    ) -> Option<String> {
        Some(GlyphScene::apply_surface_updates(self, ctx, updates))
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
        let mut enc = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("debug dump copy"), // Stage L (O2)
        });
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
        target: &crate::scene::FrameTarget<'_>,
        t: f32,
    ) {
        let crate::scene::FrameTarget {
            color_view,
            color_texture,
            color_format,
            // Stage L (L3): unused — the pass renders into the pool's own
            // depth; the driver's depth is only for direct scenes.
            depth_view: _,
            width,
            height,
        } = *target;
        let aspect = width as f32 / height.max(1) as f32;
        self.viewport.set((width, height));
        let frame = self.camera_frame(t, aspect);

        // Stage L (L3): the phase lists draw into the pooled view target
        // (ping-pong slot selected here) and composite into the driver's
        // view at the end of render.
        let comp = &self.composite;
        let vt = comp
            .target
            .as_ref()
            .expect("L3: set_viewport must run before render (both drivers call it)");
        assert_eq!(
            (vt.width, vt.height),
            (width, height),
            "L3: pool/viewport size mismatch (set_viewport out of sync with the driver)"
        );
        let pool_slot = (comp.parity.get() % 2) as usize;
        comp.parity.set(comp.parity.get() + 1);

        // Stage K (K4): apply live UI controls BEFORE culling so a slider
        // drag takes effect this frame. This is the SINGLE write site of
        // CullState::lod_min_px, and it runs only when a windowed probe is
        // installed — offscreen never installs one, so offscreen culls with
        // the LOD_MIN_PX const by construction (gate 6's byte-equal PNGs are
        // the proof).
        if let (Some(probe), Some(cull)) = (&self.ui_probe, &self.cull) {
            cull.lod_min_px.set(probe.borrow().lod_min_px);
        }

        // Stage L (L1): fill every lane of the widened frame uniform from
        // values already computed here. px_scale is computed once and shared
        // with the cull block below (it was CullView-local before L1 — the
        // uniform needs it even under --no-cull). flags bit 0
        // (deterministic_rendering) stays 0 — reserved.
        let px_scale = height as f32 / (2.0 * (FOV_Y.to_radians() * 0.5).tan());
        let cam = FrameUniform {
            view_proj: frame.view_proj.to_cols_array(),
            eye: frame.eye.to_array(),
            _pad0: 0.0,
            viewport: [width as f32, height as f32],
            px_scale,
            time: t,
            flags: 0,
            _pad1: 0,
        };
        ctx.queue
            .write_buffer(&self.camera_buf, 0, bytemuck::bytes_of(&cam));

        // --- Stage F: CPU segment cull (frustum + LOD) ----------------------
        // Stage L (L2): the cull output IS the phase lists (PhaseDraws). The
        // legacy --no-cull path builds the Glyphs list straight from the
        // chunk counts (one full range per chunk — identical draws to the
        // pre-L2 per-chunk loop) and has no Backdrop phase content.
        let phase_draws: PhaseDraws = if let Some(cull) = &self.cull {
            // Stage H: CPU scope timing (only when GLYPH_PROFILE=1 built a profiler).
            let cull_t0 = ctx.profiler.as_ref().map(|_| std::time::Instant::now());
            let view = CullView {
                planes: frustum_planes(&frame.view_proj),
                eye: frame.eye,
                px_scale,
                lod_min_px: cull.lod_min_px.get(),
            };
            let phase_draws = cull_segments(
                &cull.segments,
                &cull.hidden,
                &view,
                self.chunk_cap,
                self.bind_groups.len() as u32,
            );
            if !phase_draws.backdrops.is_empty() {
                ctx.queue.write_buffer(
                    &cull.backdrop_insts_buf,
                    0,
                    bytemuck::cast_slice(&phase_draws.backdrops),
                );
            }
            if let Some(t0) = cull_t0 {
                crate::gpu::record_cpu_scope(ctx, "cull (CPU)", t0.elapsed().as_secs_f64() * 1000.0);
            }
            if std::env::var_os("GLYPH_CULL_DEBUG").is_some() && t == 0.0 {
                let insts: u64 = phase_draws
                    .glyph_ranges
                    .iter()
                    .map(|(_, r)| (r.end - r.start) as u64)
                    .sum();
                println!(
                    "CULLDBG glyph draws={} instances={} | backdrops={}",
                    phase_draws.glyph_ranges.len(),
                    insts,
                    phase_draws.backdrops.len(),
                );
            }
            phase_draws
        } else {
            PhaseDraws {
                backdrops: Vec::new(),
                glyph_ranges: self
                    .chunk_counts
                    .iter()
                    .enumerate()
                    .map(|(c, &n)| (c as u32, 0..n))
                    .collect(),
            }
        };

        // Stage K: refresh the windowed debug-UI probe (installed only by
        // windowed runs; offscreen skips this entirely). Camera fields are
        // this frame's ACTUAL products; the pick line is the same string
        // format_pick produces for the stdout log; the K4 cull counters are
        // the same sums GLYPH_CULL_DEBUG prints (zeros under --no-cull).
        if let Some(probe) = &self.ui_probe {
            let mut p = probe.borrow_mut();
            p.camera_mode = Some(self.camera_mode);
            p.eye = frame.eye.to_array();
            p.yaw = self.fly.yaw;
            p.pitch = self.fly.pitch;
            p.last_pick = self.picked.as_ref().map(format_pick);
            if self.cull.is_some() {
                p.cull_ranges = phase_draws.glyph_ranges.len();
                p.cull_instances = phase_draws
                    .glyph_ranges
                    .iter()
                    .map(|(_, r)| (r.end - r.start) as u64)
                    .sum();
                p.cull_backdrops = phase_draws.backdrops.len();
            } else {
                p.cull_ranges = 0;
                p.cull_instances = 0;
                p.cull_backdrops = 0;
            }
            // The layout dial's readout: the field's depth extent over the
            // segment table (per frame, so group z-moves show too). None
            // under --no-cull — no segment table to measure.
            p.z_extent = self.cull.as_ref().map(|c| {
                c.segments.iter().fold([f32::INFINITY, f32::NEG_INFINITY], |[lo, hi], s| {
                    [lo.min(s.min[2]), hi.max(s.max[2])]
                })
            });
            // K5: refresh the browser's dynamic row state (world pose under
            // the live group TRS, hidden, tint). ~1.3k cheap iterations at
            // repo scale; skipped entirely offscreen (no probe installed).
            if !p.files.is_empty() {
                // Rc bump so the map below doesn't hold `p` and `files`
                // through the same borrow awkwardly.
                let files = p.files.clone();
                p.file_dyn = files
                    .iter()
                    .map(|r| {
                        let Some(g) = self.groups_cpu.get(r.group_id as usize) else {
                            return UiFileDyn::default();
                        };
                        let (ox, oy) = (g.cols[0][0], g.cols[0][1]);
                        let (sx, sy) = (g.cols[3][0].max(0.0), g.cols[3][1].max(0.0));
                        let wmin = [r.aabb_min[0] * sx + ox, r.aabb_min[1] * sy + oy];
                        let wmax = [r.aabb_max[0] * sx + ox, r.aabb_max[1] * sy + oy];
                        UiFileDyn {
                            center: [(wmin[0] + wmax[0]) * 0.5, (wmin[1] + wmax[1]) * 0.5],
                            half: [(wmax[0] - wmin[0]) * 0.5, (wmax[1] - wmin[1]) * 0.5],
                            hidden: g.cols[2][3] == 0.0,
                            tint: [
                                (g.cols[2][0].clamp(0.0, 1.0) * 255.0) as u8,
                                (g.cols[2][1].clamp(0.0, 1.0) * 255.0) as u8,
                                (g.cols[2][2].clamp(0.0, 1.0) * 255.0) as u8,
                            ],
                        }
                    })
                    .collect();
            }
        }

        // Stage H: pass-level GPU timer (TIMESTAMP_QUERY; pass-boundary writes,
        // so it works on Metal). Nested in-pass scopes below additionally need
        // TIMESTAMP_QUERY_INSIDE_PASSES — where unsupported they simply report
        // no time. Queries must always be closed, timing or not.
        let pass_query = ctx
            .profiler
            .as_ref()
            .map(|p| p.borrow().begin_pass_query("glyph field pass", encoder));
        // Stage L (L3): the pass renders into the pooled target.
        let draw_depth_view = &vt.depth;
        let draw_color_view = &vt.color_views[pool_slot];
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("glyph field pass"),
            timestamp_writes: pass_query
                .as_ref()
                .and_then(|q| q.render_pass_timestamp_writes()),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: draw_color_view,
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
                view: draw_depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        // Stage L (L2): record the phase lists in phase order — Backdrop
        // first, Glyphs second, exactly the Stage F order. Empty phases
        // record nothing (the legacy --no-cull branch has no Backdrop
        // content; an all-near-LOD frame has none either). Profiler query
        // names unchanged ("backdrop stream", "glyph stream").
        for phase in [Phase::Backdrop, Phase::Glyphs] {
            match phase {
                Phase::Selection => unreachable!(
                    "L4: the Selection phase renders into its own mask target, \
                     never inside the glyph field pass"
                ),
                Phase::Backdrop => {
                    // Far LOD stream first: one instanced draw over the
                    // compacted backdrop quads (plain draw — no indirect
                    // machinery, see the module header). The pipeline lives
                    // in CullState; the legacy branch never reaches in here
                    // (its backdrops list is empty).
                    if let Some(cull) = &self.cull {
                        if !phase_draws.backdrops.is_empty() {
                            let q = ctx
                                .profiler
                                .as_ref()
                                .map(|p| p.borrow().begin_query("backdrop stream", &mut pass));
                            pass.set_pipeline(&cull.backdrop_pipeline);
                            pass.set_bind_group(0, &cull.backdrop_bind_group, &[]);
                            pass.draw(0..6, 0..phase_draws.backdrops.len() as u32);
                            if let (Some(p), Some(q)) = (&ctx.profiler, q) {
                                p.borrow().end_query(&mut pass, q);
                            }
                        }
                    }
                }
                Phase::Glyphs => {
                    // Glyph stream: one range draw per entry. The list is
                    // chunk-major with arena-ascending ranges within a chunk,
                    // so within-pixel blend order matches the pre-L2 loops
                    // exactly; the chunk bind group is re-set only on change
                    // (legacy: one full range per chunk, so every chunk sets
                    // its bind group exactly once, as before).
                    let q = ctx
                        .profiler
                        .as_ref()
                        .map(|p| p.borrow().begin_query("glyph stream", &mut pass));
                    pass.set_pipeline(&self.pipeline);
                    let mut cur_chunk = u32::MAX;
                    for (c, r) in &phase_draws.glyph_ranges {
                        if *c != cur_chunk {
                            cur_chunk = *c;
                            pass.set_bind_group(0, &self.bind_groups[*c as usize], &[]);
                        }
                        pass.draw(0..6, r.clone());
                    }
                    if let (Some(p), Some(q)) = (&ctx.profiler, q) {
                        p.borrow().end_query(&mut pass, q);
                    }
                }
            }
        }
        drop(pass);
        if let (Some(p), Some(q)) = (&ctx.profiler, pass_query) {
            p.borrow().end_query(encoder, q);
        }

        // Stage L (L4): the Selection phase — own mask target ⇒ own pass,
        // rendered after Glyphs. The variant is constructed here, when a
        // selection exists (no selection → the L3 command stream is
        // unchanged). Windowed shader path only: on the copy path
        // (offscreen) selection_fx/mask are None and this block is skipped,
        // so offscreen output never carries the tint.
        let selection_phase = self.selection.is_some().then_some(Phase::Selection);
        // The pool slot the composite will read: the scene slot, or the
        // tinted ping-pong partner when a selection was rendered.
        let mut final_slot = pool_slot;
        if let (Some(Phase::Selection), Some(fx), Some(mask)) =
            (selection_phase, &comp.selection_fx, &vt.mask)
        {
            let sel = self.selection.as_ref().expect("selection_phase implies selection");
            // Mask pass: the selected glyph quads into the mask target
            // (glyph coverage in alpha). Blend disabled; no depth.
            let mask_query = ctx
                .profiler
                .as_ref()
                .map(|p| p.borrow().begin_pass_query("selection mask pass", encoder));
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("selection mask pass"),
                    timestamp_writes: mask_query
                        .as_ref()
                        .and_then(|q| q.render_pass_timestamp_writes()),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &mask.view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            // TRANSPARENT, not BLACK: wgpu::Color::BLACK is
                            // (0,0,0,1) — clearing to alpha=1 blanketed the
                            // whole mask (uniform tint over the frame; caught
                            // by the K6 eyeball + a mask-dump probe).
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    ..Default::default()
                });
                pass.set_pipeline(&fx.mask_pipeline);
                match sel {
                    Selection::Glyph { chunk, local } => {
                        pass.set_bind_group(0, &self.bind_groups[*chunk as usize], &[]);
                        pass.draw(0..6, *local..*local + 1);
                    }
                    Selection::Segment { slot_base, slot_count } => {
                        // Per-chunk split — the same math cull_segments uses.
                        let slot_end = slot_base + slot_count;
                        for c in 0..self.bind_groups.len() as u32 {
                            let c_lo = c * self.chunk_cap;
                            let lo = (*slot_base).max(c_lo);
                            let hi = slot_end.min(c_lo + self.chunk_cap);
                            if hi > lo {
                                pass.set_bind_group(0, &self.bind_groups[c as usize], &[]);
                                pass.draw(0..6, (lo - c_lo)..(hi - c_lo));
                            }
                        }
                    }
                }
            }
            if let (Some(p), Some(q)) = (&ctx.profiler, mask_query) {
                p.borrow().end_query(encoder, q);
            }
            // Tint pass: pool[pool_slot] + mask → pool[1 - pool_slot]
            // (additive coverage-weighted tint); the composite reads the
            // tinted slot below.
            final_slot = 1 - pool_slot;
            let tint_query = ctx
                .profiler
                .as_ref()
                .map(|p| p.borrow().begin_pass_query("selection tint pass", encoder));
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("selection tint pass"),
                    timestamp_writes: tint_query
                        .as_ref()
                        .and_then(|q| q.render_pass_timestamp_writes()),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &vt.color_views[final_slot],
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    ..Default::default()
                });
                pass.set_pipeline(&fx.tint_pipeline);
                pass.set_bind_group(0, &mask.tint_bgs[pool_slot], &[]);
                pass.draw(0..3, 0..1);
            }
            if let (Some(p), Some(q)) = (&ctx.profiler, tint_query) {
                p.borrow().end_query(encoder, q);
            }
        }

        // Stage L (L3): composite the pooled target into the driver's view.
        if color_format == POOL_FORMAT {
            // Offscreen/oracle path: same format, 1:1, no scaling —
            // copy_texture_to_texture is bit-exact BY CONSTRUCTION (this
            // is the gate-critical path; it cannot fail a byte compare).
            assert_eq!(
                color_format, POOL_FORMAT,
                "L3: copy composite requires matching formats (deliberately loud)"
            );
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &vt.colors[final_slot],
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyTextureInfo {
                    texture: color_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            );
        } else {
            // Windowed path: the surface (Bgra8UnormSrgb) is
            // component-order-incompatible with the pool, so a copy is
            // invalid — fullscreen shader composite instead. The pass
            // clears-then-overwrites every pixel (blend disabled).
            let composite_query = ctx
                .profiler
                .as_ref()
                .map(|p| p.borrow().begin_pass_query("composite pass", encoder));
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("composite pass"),
                    timestamp_writes: composite_query
                        .as_ref()
                        .and_then(|q| q.render_pass_timestamp_writes()),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: color_view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    ..Default::default()
                });
                pass.set_pipeline(&comp.pipeline);
                pass.set_bind_group(0, &vt.bind_groups[final_slot], &[]);
                pass.draw(0..3, 0..1);
            }
            if let (Some(p), Some(q)) = (&ctx.profiler, composite_query) {
                p.borrow().end_query(encoder, q);
            }
        }
    }
}

// ── Stage H (Phase 5) — encase layout assertions ────────────────────────────
// The WGSL lane maps (glyph_field.wgsl header: 12×4 B lanes; GROUP_STRIDE=5
// vec4s) are mirrored by hand in the repr(C) structs above — the Stage G
// strided-color bug came from exactly this mirroring. encase's derive computes
// WGSL-layout size/offsets independently; these tests pin the two
// representations against each other AND against bytemuck's raw bytes, so a
// layout edit that disagrees with the shaders fails `cargo test` at compile
// time instead of corrupting a render.
#[cfg(test)]
mod layout_tests {
    use super::*;
    use encase::{ShaderSize, ShaderType};

    #[test]
    fn glyph_instance_size_and_offsets() {
        assert_eq!(<GlyphInstance as ShaderSize>::SHADER_SIZE.get(), 48, "WGSL lane map is 12 x 4 B");
        assert_eq!(std::mem::size_of::<GlyphInstance>(), 48);
        // Lane map from the glyph_field.wgsl header (offset in bytes).
        // (Metadata is by-value; METADATA is a const, so re-instantiate it.)
        let expected = [
            ("pos", 0),
            ("glyph_id", 12),
            ("row", 16),
            ("col", 20),
            ("color", 24),
            ("group_id", 28),
            ("advance", 32),
            ("height", 36),
            ("flags", 40),
            ("_pad", 44),
        ];
        for (i, (name, off)) in expected.iter().enumerate() {
            assert_eq!(GlyphInstance::METADATA.offset(i), *off as u64, "field {name} offset");
        }
    }

    #[test]
    fn group_row_size_and_offsets() {
        assert_eq!(<GroupRow as ShaderSize>::SHADER_SIZE.get(), 80, "GROUP_STRIDE=5 vec4s");
        assert_eq!(std::mem::size_of::<GroupRow>(), 80);
        assert_eq!(GroupRow::METADATA.offset(0), 0, "cols offset");
    }

    /// Stage L (L1): the widened frame uniform. `view_proj` is pinned at
    /// 0..64 — the unchanged WGSL `Camera` block binds those first 64 B, and
    /// the minimum-binding-size rule lets the larger buffer carry the
    /// appended lanes without a shader edit. NOTE: repr(C) arrays are
    /// align-4 in Rust (no GPU vec alignment), so there is NO tail padding —
    /// Rust and encase agree at 104 B.
    #[test]
    fn frame_uniform_size_and_offsets() {
        assert_eq!(std::mem::size_of::<FrameUniform>(), 104, "repr(C) size (align-4 arrays: no tail pad)");
        let expected = [
            ("view_proj", 0),
            ("eye", 64),
            ("_pad0", 76),
            ("viewport", 80),
            ("px_scale", 88),
            ("time", 92),
            ("flags", 96),
            ("_pad1", 100),
        ];
        for (i, (name, off)) in expected.iter().enumerate() {
            assert_eq!(FrameUniform::METADATA.offset(i), *off as u64, "field {name} offset");
        }
        // Encase uniform-space size agrees with repr(C); both are ≥ the WGSL
        // block's 64 B minimum binding size.
        assert_eq!(<FrameUniform as ShaderSize>::SHADER_SIZE.get(), 104);
    }

    /// Decisive check: encase's serialization must be byte-identical to the
    /// bytemuck wire format for distinctive bit patterns — if this holds, the
    /// write paths can never diverge silently.
    #[test]
    fn encase_bytes_match_bytemuck() {
        let inst = GlyphInstance {
            pos: [1.5, -2.25, 3.75],
            glyph_id: 0xAABBCCDD,
            row: 0x11223344,
            col: 0x55667788,
            color: 0xDEADBEEF,
            group_id: 7,
            advance: 0.529_741_4,
            height: 1.0,
            flags: 0x80000001,
            _pad: 0x42424242,
        };
        let mut buf = Vec::<u8>::new();
        encase::StorageBuffer::new(&mut buf).write(&inst).unwrap();
        assert_eq!(buf.len(), 48);
        assert_eq!(&buf[..], bytemuck::bytes_of(&inst), "GlyphInstance bytes");

        let row = GroupRow {
            cols: [
                [1.0, 2.0, 3.0, 4.0],
                [0.0, 0.0, 0.0, 1.0],
                [0.25, 0.5, 0.75, 1.0],
                [2.0, 2.0, 2.0, 0.0],
                [-1.0, -2.0, 1e10, f32::MIN_POSITIVE],
            ],
        };
        let mut buf = Vec::<u8>::new();
        encase::StorageBuffer::new(&mut buf).write(&row).unwrap();
        assert_eq!(buf.len(), 80);
        assert_eq!(&buf[..], bytemuck::bytes_of(&row), "GroupRow bytes");
    }
}

#[cfg(test)]
mod cull_depth_tests {
    use super::*;

    /// A segment somewhere in space, with the tint the cull path never reads.
    fn seg(min: [f32; 3], max: [f32; 3]) -> SegCull {
        SegCull { min, max, slot_base: 0, slot_count: 16, tint: [0.0; 4] }
    }

    /// A frustum that keeps everything except what is behind the near plane,
    /// which here is a plane at z = -10 facing +z. Every other plane is placed
    /// far enough away to be irrelevant, so a cull decision is attributable to
    /// depth alone.
    fn view_clipping_behind_z(near_z: f32, eye: Vec3, lod_min_px: f32) -> CullView {
        let far = |a: f32, b: f32, c: f32, d: f32| [a, b, c, d];
        CullView {
            planes: [
                far(1.0, 0.0, 0.0, 1.0e6),
                far(-1.0, 0.0, 0.0, 1.0e6),
                far(0.0, 1.0, 0.0, 1.0e6),
                far(0.0, -1.0, 0.0, 1.0e6),
                // keep z >= near_z
                [0.0, 0.0, 1.0, -near_z],
                far(0.0, 0.0, -1.0, 1.0e6),
            ],
            eye,
            px_scale: 1000.0,
            lod_min_px,
        }
    }

    fn drew_glyphs(d: &PhaseDraws) -> bool {
        d.glyph_ranges.iter().any(|(_, r)| !r.is_empty())
    }

    /// A segment BEHIND the near plane must be culled.
    ///
    /// The old arithmetic tested every segment as though it spanned z ∈ [-1, 1]
    /// regardless of where it was, so a segment at z = -50 was judged at z ≈ 0 —
    /// comfortably inside — and drawn. True while everything lived in the z=0
    /// plane; false since WrapBack started spending wraps in depth.
    #[test]
    fn a_segment_behind_the_near_plane_is_culled() {
        let s = seg([-1.0, -1.0, -50.0], [1.0, 1.0, -49.0]);
        let v = view_clipping_behind_z(-10.0, Vec3::new(0.0, 0.0, 5.0), 0.0);
        let d = cull_segments(&[s], &[false], &v, 1024, 1);
        assert!(
            !drew_glyphs(&d),
            "a segment at z=-50, behind a near plane at z=-10, was drawn: the \
             frustum test is ignoring the segment's depth"
        );
    }

    /// ...and one in front of it must survive, so the test above cannot pass by
    /// culling everything.
    #[test]
    fn a_segment_in_front_of_the_near_plane_survives() {
        let s = seg([-1.0, -1.0, -1.0], [1.0, 1.0, 1.0]);
        let v = view_clipping_behind_z(-10.0, Vec3::new(0.0, 0.0, 5.0), 0.0);
        let d = cull_segments(&[s], &[false], &v, 1024, 1);
        assert!(drew_glyphs(&d), "a segment inside the frustum was culled");
    }

    /// LOD distance must include depth.
    ///
    /// `nz` was `eye.z.clamp(-1.0, 1.0)`, so the nearest point of a segment 50
    /// units away in z was computed as if it were adjacent: the distance came
    /// out ~0 instead of ~50, the glyphs measured far larger than a pixel, and
    /// a segment that should have collapsed to a backdrop quad was drawn in
    /// full. The error is in the expensive direction — it draws what it should
    /// have skipped.
    #[test]
    fn lod_distance_accounts_for_depth() {
        let s = seg([-1.0, -1.0, -50.0], [1.0, 1.0, -49.0]);
        // px_scale/dist at dist≈50 is 20 px/em; ask for 100 so it must drop.
        let v = view_clipping_behind_z(-1.0e6, Vec3::new(0.0, 0.0, 0.0), 100.0);
        let d = cull_segments(&[s], &[false], &v, 1024, 1);
        assert!(
            !drew_glyphs(&d) && !d.backdrops.is_empty(),
            "a segment 50 units away in z was drawn at full detail: the LOD \
             distance is ignoring depth"
        );
    }
}
