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
//!     the affected arena chunk at slot granularity (32 B RenderSlot stride,
//!     4-aligned);
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

use glam::{DVec3, Vec3};
use bytemuck::Zeroable;
use std::cell::Cell;
use wgpu::util::DeviceExt;

use crate::atlas::Atlas;
use crate::gpu::GpuContext;
use crate::scene::SceneLike;
use crate::text::StagedText;

mod camera;
pub use camera::{CameraMode, FlyCamera, FOV_Y};
use camera::CamFrame;

mod cull;
pub use cull::{SegCull, BACKDROP_GAIN, GLYPH_CELL_AREA, LOD_MIN_PX};
use cull::{cull_segments, frustum_planes, CullState, CullView, Phase, PhaseDraws};

mod pick;
pub use pick::{PickCommand, PickContext, PickFileInfo, PickHit, Verb};
use pick::{format_pick, PickCacheEntry};

mod tint;
pub use tint::{SegTintAccum, seg_tint};

mod instance;
pub use instance::{GlyphInstance, GroupRow, RenderSlot};
use instance::{FrameUniform, Params};

mod target;
use target::{CompositeState, Selection, SelectionFx, ViewTarget, MASK_FORMAT, POOL_FORMAT, SCENE_SAMPLE_COUNT, SELECTION_TINT};

mod ui_probe;
pub use ui_probe::UiProbe;
use ui_probe::UiFileDyn;

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
    /// Arena chunk buffers, kept for partial per-slot uploads (verbs): the
    /// chain's own slot buffers on the endpoint (Device) path, the upload
    /// buffers otherwise. `chunk_offsets[k]` is slot 0's byte address in
    /// `instance_bufs[k]` (0 for staged uploads; the pool slice's start for
    /// the endpoint's extracted buffers) — chunk_off adds it.
    instance_bufs: Vec<wgpu::Buffer>,
    /// Per-chunk byte offset of slot 0 inside the buffer (see above).
    chunk_offsets: Vec<u64>,
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

/// Slots per arena chunk buffer: the tighter of the storage binding and
/// whole-buffer limits, in instances — the value the draw path's chunk math
/// and the arena's chunking must agree on, computed once, here.
/// GLYPH_ARENA_CHUNK_SLOTS overrides it (the fork gate forces small chunks so
/// the split paths — copy-hop intersections, tint straddles, multi-buffer
/// bindings — run on a small corpus).
pub fn arena_chunk_slots(ctx: &GpuContext) -> usize {
    let derived = (ctx
        .device
        .limits()
        .max_storage_buffer_binding_size
        .min(ctx.profile.max_buffer_size) as usize
        / std::mem::size_of::<GlyphInstance>())
    .max(1);
    std::env::var("GLYPH_ARENA_CHUNK_SLOTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(derived)
        .max(1)
}

/// The device-resident arena for the repo load's direct path: ONE
/// shared-storage buffer the FFI writes instances into and the glyph shader
/// reads — the write IS the upload, so the load pays one allocation and no
/// copy (against the Vec arena + sharded copy it replaces: a second 4.5 GB
/// allocation, its kernel zero-fill, and the 4.5 GB memcpy). `slots` is the
/// corpus byte count — leaders ≤ bytes, the bound `GlyphArena::uninit_tail`
/// commits against. Metal only, gated by the caller on the profile: a
/// discrete adapter would trade the saved copy for slower per-frame shader
/// reads across the bus.
///
/// Chunked since rung 5e: one buffer per `chunk_slots` slots (the last sized
/// to the remainder), so a corpus whose instance mass exceeds
/// `max_buffer_size` is chunked instead of refused — the draw path binds one
/// buffer per chunk regardless.
pub fn mapped_instance_arena(
    ctx: &GpuContext,
    slots: usize,
    chunk_slots: usize,
) -> crate::layout::GlyphArena {
    use wgpu::hal::Device as HalDevice;
    let device = &ctx.device;
    let hal_dev = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }
        .expect("Metal profile behind a non-Metal device");
    let n_chunks = slots.div_ceil(chunk_slots).max(1);
    let mut parts = Vec::with_capacity(n_chunks);
    for k in 0..n_chunks {
        let cap = chunk_slots.min(slots - k * chunk_slots);
        let size = (cap * std::mem::size_of::<GlyphInstance>()) as u64;
        let label = format!("glyph arena (mapped) {k}/{n_chunks}");
        let hal_buf = unsafe {
            hal_dev.create_buffer(&wgpu::hal::BufferDescriptor {
                label: Some(&label),
                size,
                usage: wgpu::BufferUses::STORAGE_READ_ONLY
                    | wgpu::BufferUses::COPY_DST
                    | wgpu::BufferUses::COPY_SRC
                    | wgpu::BufferUses::MAP_READ,
                memory_flags: wgpu::hal::MemoryFlags::empty(),
            })
        }
        .expect("hal arena buffer");
        let mapping = unsafe { hal_dev.map_buffer(&hal_buf, 0..size) }.expect("hal arena map");
        // On Metal `unmap_buffer` is a no-op — the mapping simply lives as
        // long as the buffer, which the arena owns (see layout::MappedArena).
        let ptr = mapping.ptr.as_ptr() as *mut GlyphInstance;
        // SAFETY: same device, desc matches the hal request, the buffer is
        // kernel-zeroed (and wgpu's init tracker is born empty regardless),
        // nonzero size (cap > 0 by the div_ceil above).
        let buf = unsafe {
            device.create_buffer_from_hal::<wgpu::hal::api::Metal>(
                hal_buf,
                &wgpu::BufferDescriptor {
                    label: Some(&label),
                    size,
                    usage: wgpu::BufferUsages::STORAGE
                        | wgpu::BufferUsages::COPY_DST
                        | wgpu::BufferUsages::COPY_SRC
                        | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                },
            )
        };
        parts.push((ptr, cap, buf));
    }
    crate::layout::GlyphArena::from_mapped_chunks(parts, chunk_slots)
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
        let mut arena = staged.instances;
        if arena.is_empty() && !arena.is_mapped() && !arena.is_device() {
            arena.push(GlyphInstance {
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

        // Chunk the arena so no bound RANGE exceeds the binding limit.
        // (the cull/pick slot math keys on chunk_cap, so the renderer's
        // chunking and the arena's must agree).
        //
        // E2a (note 23): the shader binds the 32 B RenderSlot, so HOST and
        // MAPPED (48 B) arenas transcode at staging — the values the vertex
        // math reads are unchanged, so the goldens stay byte-equal.
        // E2b: a DEVICE arena (the endpoint) binds the chain's slot buffers
        // AS-IS — no upload, no transcode, no copy; each chunk's pool-slice
        // offset rides its binding.
        let binding_limit = ctx.device.limits().max_storage_buffer_binding_size as usize;
        let instances_len = arena.len();
        let (chunk_cap, chunk_counts, instance_bufs, chunk_offsets): (
            usize,
            Vec<u32>,
            Vec<wgpu::Buffer>,
            Vec<u64>,
        ) = match arena.device_slots() {
            Some(dev) => (
                dev.chunk_slots,
                dev.chunks.iter().map(|c| c.slots).collect(),
                dev.chunks.iter().map(|c| c.buffer.clone()).collect(),
                dev.chunks.iter().map(|c| c.offset).collect(),
            ),
            None => {
            let chunk_cap = (binding_limit / std::mem::size_of::<RenderSlot>()).max(1);
            let mut chunk_counts: Vec<u32> = (0..instances_len.div_ceil(chunk_cap).max(1))
                .map(|k| (instances_len.saturating_sub(k * chunk_cap)).min(chunk_cap) as u32)
                .collect();
            // The mapped-empty arena binds one zeroed slot (nothing draws, but
            // the safety-net segment reads slot 0).
            for c in &mut chunk_counts {
                if *c == 0 {
                    *c = 1;
                }
            }
        // Unified-memory upload: with MAPPABLE_PRIMARY_BUFFERS on Metal the
        // storage buffer is created mapped and written straight — wgpu's
        // default path instead zero-fills a full-size staging buffer AND then
        // memcpy's into it AND blits on the GPU timeline (measured ~2.4 s of
        // the glyph3d-js repo load; the zero-fill alone was ~0.7 s). Metal
        // only: discrete adapters advertise the feature too, but a
        // host-visible storage buffer pays the saved time back in per-frame
        // shader reads across the bus.
        let direct_upload = ctx.profile.backend == wgpu::Backend::Metal
            && ctx.profile.mappable_primary_buffers;
        // The transcode walk: render chunks (RenderSlot stride) intersect
        // the arena's own chunks (host Vec or mapped slices) — the two
        // chunkings do not in general coincide, and a straddling range must
        // transcode bit-identically to a contiguous one (same values, field
        // order fixed by From<&GlyphInstance>).
        let arena_chunks = arena.instance_chunks();
        let mut ac = 0usize;
        let mut arena_base = 0usize;
        let instance_bufs: Vec<wgpu::Buffer> = chunk_counts
            .iter()
            .enumerate()
            .map(|(i, &count)| {
            let label = if chunk_counts.len() == 1 {
                "glyph instances".to_string()
            } else {
                format!("glyph instances {i}/{}", chunk_counts.len())
            };
            // Stage G: COPY_DST for partial per-slot edit uploads;
            // COPY_SRC for the GLYPH_G_DUMP verification readback.
            let usage = wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC;
            let first = i * chunk_cap;
            let need_end = first + count as usize;
            let mut slots: Vec<RenderSlot> = Vec::with_capacity(count as usize);
            while slots.len() < count as usize && ac < arena_chunks.len() {
                let c = arena_chunks[ac];
                if arena_base + c.len() <= first {
                    arena_base += c.len();
                    ac += 1;
                    continue;
                }
                let lo = first - arena_base;
                let hi = (need_end - arena_base).min(c.len());
                slots.extend(c[lo..hi].iter().map(RenderSlot::from));
                if hi == c.len() {
                    arena_base += c.len();
                    ac += 1;
                }
            }
            // The mapped-empty arena has no slices at all — pad the one
            // zeroed slot; a real under-fill is a walk bug, not padding.
            debug_assert!(instances_len == 0 || slots.len() == count as usize);
            slots.resize(count as usize, RenderSlot::zeroed());
            let bytes: &[u8] = bytemuck::cast_slice(&slots);
                    if direct_upload {
                        // hal-created shared buffer, spiked for the mapped-arena
                        // work: MAP_READ in the usage makes wgpu-hal pick
                        // StorageModeShared with the DEFAULT cache mode (MAP_WRITE
                        // would set write-combining — streaming-friendly for the
                        // one upload write, but the CPU-side paths that read the
                        // arena back can't afford uncached reads). The hal map
                        // hands back the raw pointer wgpu's WriteOnly view
                        // deliberately withholds, so the sharded first-touch write
                        // splits on raw disjoint ranges.
                        use wgpu::hal::Device as HalDevice;
                        let hal_usage = wgpu::BufferUses::STORAGE_READ_ONLY
                            | wgpu::BufferUses::COPY_DST
                            | wgpu::BufferUses::COPY_SRC
                            | wgpu::BufferUses::MAP_READ;
                        let hal_dev = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }
                            .expect("Metal profile behind a non-Metal device");
                        let size = bytes.len() as u64;
                        let hal_buf = unsafe {
                            hal_dev.create_buffer(&wgpu::hal::BufferDescriptor {
                                label: Some(&label),
                                size,
                                usage: hal_usage,
                                memory_flags: wgpu::hal::MemoryFlags::empty(),
                            })
                        }
                        .expect("hal instance buffer");
                        let mapping = unsafe { hal_dev.map_buffer(&hal_buf, 0..size) }
                            .expect("hal instance map");
                        let base = mapping.ptr.as_ptr() as usize;
                        // The write IS the first touch of the buffer's pages —
                        // sharded so each worker faults in its own range (measured
                        // ~2 s of the repo load serial). Small buffers stay serial.
                        const PARALLEL_COPY_THRESHOLD: usize = 16 << 20;
                        if bytes.len() >= PARALLEL_COPY_THRESHOLD {
                            let workers = std::thread::available_parallelism()
                                .map(|n| n.get())
                                .unwrap_or(1)
                                .min(8);
                            let span = bytes.len().div_ceil(workers).next_multiple_of(32);
                            std::thread::scope(|s| {
                                for (i, src) in bytes.chunks(span).enumerate() {
                                    let off = i * span;
                                    s.spawn(move || unsafe {
                                        std::ptr::copy_nonoverlapping(
                                            src.as_ptr(),
                                            (base + off) as *mut u8,
                                            src.len(),
                                        );
                                    });
                                }
                            });
                        } else {
                            unsafe {
                                std::ptr::copy_nonoverlapping(
                                    bytes.as_ptr(),
                                    base as *mut u8,
                                    bytes.len(),
                                );
                            }
                        }
                        unsafe { hal_dev.unmap_buffer(&hal_buf) };
                        // SAFETY: same device, desc matches the hal request, every
                        // byte just written, nonzero size.
                        unsafe {
                            device.create_buffer_from_hal::<wgpu::hal::api::Metal>(
                                hal_buf,
                                &wgpu::BufferDescriptor {
                                    label: Some(&label),
                                    size,
                                    usage: usage | wgpu::BufferUsages::MAP_READ,
                                    mapped_at_creation: false,
                                },
                            )
                        }
                    } else {
                        device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                            label: Some(&label),
                            contents: bytes,
                            usage,
                        })
                    }
            })
            .collect();
            let zero_offsets = vec![0u64; instance_bufs.len()];
            (chunk_cap, chunk_counts, instance_bufs, zero_offsets)
            }
        };
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
            instances_len,
            (instances_len * std::mem::size_of::<RenderSlot>()) >> 20,
            chunk_counts.len(),
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
        // The chunk bindings: the chain's slot buffers on the endpoint
        // (Device) path — each with its pool-slice offset — the staged
        // uploads otherwise. Each chunk's binding starts at its own index
        // 0, ≤ the binding limit by construction.
        let chunk_bindings: Vec<wgpu::BufferBinding> = instance_bufs
            .iter()
            .zip(chunk_counts.iter())
            .zip(chunk_offsets.iter())
            .map(|((b, &count), &off)| wgpu::BufferBinding {
                buffer: b,
                // The endpoint's pool slices start mid-buffer; staged
                // uploads are offset 0. The binding's size is the chunk's
                // live slots (never the pool page's padding).
                offset: off,
                size: std::num::NonZeroU64::new(count as u64 * 32),
            })
            .collect();
        let bind_group_count = chunk_bindings.len();
        let bind_groups: Vec<wgpu::BindGroup> = chunk_bindings
            .iter()
            .enumerate()
            .map(|(i, chunk_binding)| {
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
                            resource: wgpu::BindingResource::Buffer(chunk_binding.clone()),
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
                slot_count: arena.len() as u32,
                tint: seg_tint(
                    arena.instances(),
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
            instance_count: instances_len as u32,
            center,
            half_w,
            half_h,
            fit,
            camera_mode,
            fly,
            cull,
            instance_bufs,
            chunk_offsets,
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
        let off = self.chunk_off(chunk, local * 32);
        enc.copy_buffer_to_buffer(self.chunk_buf(chunk), off, &buf, 0, size);
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
