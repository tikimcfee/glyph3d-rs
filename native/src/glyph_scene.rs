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
use cull::CullState;

mod pick;
pub use pick::{PickCommand, PickContext, PickFileInfo, PickHit, Verb};
use pick::PickCacheEntry;

mod tint;
pub use tint::{SegTintAccum, seg_tint, srgb_to_linear_table};

mod instance;
pub use instance::{GlyphInstance, GroupRow, RenderSlot};
use instance::{FrameUniform, Params};

mod target;
use target::{CompositeState, Selection, ViewTarget, POOL_FORMAT, SCENE_SAMPLE_COUNT};

mod ui_probe;
pub use ui_probe::UiProbe;

mod buffers;

mod pipelines;
mod render;

pub struct GlyphScene {
    pub pipeline: wgpu::RenderPipeline,
    /// The colour-emoji sheet's view, held so the texture outlives the bind
    /// groups that sample it (binding 6 of every chunk's bind group).
    pub(crate) _emoji_view: wgpu::TextureView,
    /// Stage E2: one bind group per instance-buffer CHUNK. A repo-scale field
    /// can exceed `max_storage_buffer_binding_size` (48 B × tens of millions
    /// of glyphs), so the arena is split into buffers that each fit the
    /// binding limit; render() issues one draw per chunk. instance_index is
    /// chunk-local, which is exactly right — each chunk buffer starts at 0.
    pub(crate) bind_groups: Vec<wgpu::BindGroup>,
    pub(crate) chunk_counts: Vec<u32>,
    /// Stage F: instances per chunk (the uniform the cull pass uses to split
    /// a segment's slot range across chunk draws).
    pub(crate) chunk_cap: u32,
    pub(crate) camera_buf: wgpu::Buffer,
    pub(crate) depth_format: wgpu::TextureFormat,
    pub(crate) instance_count: u32,
    pub(crate) center: Vec3,
    pub(crate) half_w: f32,
    pub(crate) half_h: f32,
    /// Fit distance of the Front camera — scales the Fly camera's near/far
    /// and initial speed.
    pub(crate) fit: f32,
    pub(crate) camera_mode: CameraMode,
    pub(crate) fly: FlyCamera,
    /// Stage F: present unless culling is disabled (`--no-cull`); None keeps
    /// the legacy per-chunk draws.
    pub(in crate::glyph_scene) cull: Option<CullState>,
    // ── Stage G: picking & live manipulation ────────────────────────────
    /// Arena chunk buffers, kept for partial per-slot uploads (verbs): the
    /// chain's own slot buffers on the endpoint (Device) path, the upload
    /// buffers otherwise. `chunk_offsets[k]` is slot 0's byte address in
    /// `instance_bufs[k]` (0 for staged uploads; the pool slice's start for
    /// the endpoint's extracted buffers) — chunk_off adds it.
    pub(crate) instance_bufs: Vec<wgpu::Buffer>,
    /// Per-chunk byte offset of slot 0 inside the buffer (see above).
    pub(crate) chunk_offsets: Vec<u64>,
    /// When mapped in host-visible memory, base pointer to the RenderSlot slice as usize.
    pub(crate) mapped_slots: Option<usize>,
    /// Group table buffer, kept for partial per-row uploads (80 B/row).
    pub(crate) group_buf: wgpu::Buffer,
    /// CPU mirror of the group table — the pick path reads the LIVE TRS from
    /// here and every group verb writes it back (then uploads just that row).
    pub(crate) groups_cpu: Vec<GroupRow>,
    /// Repo-mode pick context (None for text/engine scenes).
    pub(crate) pick: Option<PickContext>,
    /// The panel's cluster-toggle seed for scenes WITHOUT a pick context
    /// (text): the staging choice carries the mode and nothing else on the
    /// scene remembers it. Repo scenes leave this None — their probe seeds
    /// from the pick context's uniform ItemParams (the two agree there).
    pub(crate) probe_cluster_mode: Option<bool>,
    /// The last resolved pick (verbs operate on it).
    pub(crate) picked: Option<PickHit>,
    /// Stage L (L4): the current selection (drives the mask pass). Replaces
    /// the Stage G click-flash hack (instance-byte write + restore) — no
    /// buffer writes, nothing to restore; the tint lives entirely in the
    /// windowed composite path.
    pub(in crate::glyph_scene) selection: Option<Selection>,
    /// Geometry overrides from nudge/scale-glyph verbs (slot → pos/advance/
    /// height), so a later recolor-line rebuild preserves them.
    pub(crate) geom_overrides: std::collections::HashMap<u32, ([f32; 3], f32, f32)>,
    /// One-entry cache of the last pick's re-derived file data.
    pub(in crate::glyph_scene) cache: Option<PickCacheEntry>,
    /// Windowed grab verb: the group being dragged with the mouse.
    pub(crate) grabbed_group: Option<u32>,
    /// Last known cursor position, physical px (click pick + grab drag).
    pub(crate) cursor: (f32, f32),
    /// Viewport in physical px, refreshed every render() (ray unprojection).
    pub(crate) viewport: Cell<(u32, u32)>,
    /// Per-group position in the DIR_TINTS cycle (t verb).
    pub(crate) tint_step: Vec<u32>,
    /// Stage K: windowed debug-UI probe (None offscreen / under --no-ui).
    pub(crate) ui_probe: Option<UiProbe>,
    /// Stage L (L3): device handle for pool (re)creation in set_viewport
    /// (which has no ctx param — the trait shape is fenced).
    pub(crate) device: wgpu::Device,
    /// Stage L (L3): composite machinery — the ONLY release path (the
    /// --no-composite A/B escape hatch proved neutrality and was removed at
    /// stage end; see out/STAGE_L_REPORT.md).
    pub(in crate::glyph_scene) composite: CompositeState,
    pub(in crate::glyph_scene) params_buf: wgpu::Buffer,
    pub(in crate::glyph_scene) params: Cell<Params>,
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
        if arena.is_empty() && !arena.is_device() {
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
        let t_scene_start = std::time::Instant::now();
        let binding_limit = ctx.device.limits().max_storage_buffer_binding_size as usize;
        let instances_len = arena.len();
        let mapped_slots = arena.device_slots().and_then(|d| d.mapped_slots);
        let (chunk_cap, chunk_counts, instance_bufs, chunk_offsets) =
            buffers::build_instance_buffers(ctx, &arena, instances_len);
        let upload_dur = t_scene_start.elapsed();
        let t_pipe_start = std::time::Instant::now();
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
            greek_mode: 1,
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
            greek_onset_px: 10.0,
            _pad3: 0,
        };
        let params_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("glyph params"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let bgl = pipelines::build_glyph_bgl(device);
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

        let depth_format = wgpu::TextureFormat::Depth32Float;
        let (shader, layout, pipeline) =
            pipelines::build_glyph_pipeline(device, &bgl, depth_format);
        let mask_pipeline =
            pipelines::build_mask_pipeline(device, &shader, &layout, color_format);

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
        let composite = pipelines::build_composite_state(device, color_format, mask_pipeline);

        let pick = staged.pick;
        let groups_cpu = groups.clone();
        let tint_step = vec![0u32; groups.len()];
        let pipe_dur = t_pipe_start.elapsed();
        log::info!(
            "scene timings: buffer upload {:.3}s | pipeline/cull init {:.3}s | total scene {:.3}s",
            upload_dur.as_secs_f64(),
            pipe_dur.as_secs_f64(),
            (upload_dur + pipe_dur).as_secs_f64(),
        );

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
            mapped_slots,
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
            params_buf,
            params: Cell::new(params),
        }
    }

    /// Configure whether Greeking (anti-Moiré subpixel bars) is enabled.
    pub fn set_greeking(&self, queue: &wgpu::Queue, on: bool) {
        let mut p = self.params.get();
        let mode = if on { 1 } else { 0 };
        if p.greek_mode != mode {
            p.greek_mode = mode;
            self.params.set(p);
            queue.write_buffer(&self.params_buf, 0, bytemuck::bytes_of(&p));
        }
    }

    /// Configure the on-screen glyph height in px/em where Greeking begins (default: 10.0).
    pub fn set_greek_onset_px(&self, queue: &wgpu::Queue, onset: f32) {
        let mut p = self.params.get();
        if (p.greek_onset_px - onset).abs() > 1e-3 {
            p.greek_onset_px = onset;
            self.params.set(p);
            queue.write_buffer(&self.params_buf, 0, bytemuck::bytes_of(&p));
        }
    }

    /// Configure whether file background bounding quads are emitted behind glyphs when near.
    pub fn set_file_backgrounds(&mut self, on: bool) {
        if let Some(cull) = &self.cull {
            cull.file_backgrounds.set(on);
        }
    }

    /// Set the RGBA color for near file background bounding quads.
    pub fn set_file_bg_color(&mut self, rgba: [f32; 4]) {
        if let Some(cull) = &self.cull {
            cull.file_bg_color.set(rgba);
        }
    }

    /// Set the LOD minimum pixel threshold.
    pub fn set_lod_min_px(&mut self, lod: f32) {
        if let Some(cull) = &self.cull {
            cull.lod_min_px.set(lod);
        }
    }

    /// Update slot colors in-place on the GPU.
    /// If direct-mapped GPU memory is available (e.g. Apple Silicon Metal),
    /// writes directly into host-visible mapped slots without queue uploads.
    /// Otherwise, dispatches wgpu queue write_buffer commands.
    pub fn write_slot_colors(&self, ctx: &GpuContext, slot_base: u32, colors: &[u32]) {
        if colors.is_empty() {
            return;
        }
        if let Some(addr) = self.mapped_slots {
            let ptr = addr as *mut RenderSlot;
            let total = self.instance_count as usize;
            let base = slot_base as usize;
            let count = colors.len().min(total.saturating_sub(base));
            unsafe {
                for (i, &c) in colors.iter().take(count).enumerate() {
                    (*ptr.add(base + i)).color = c;
                }
            }
        } else {
            for (i, &color) in colors.iter().enumerate() {
                let slot = slot_base + i as u32;
                if slot >= self.instance_count {
                    break;
                }
                let chunk = (slot / self.chunk_cap) as usize;
                let local = (slot % self.chunk_cap) as u64;
                let off = self.chunk_offsets[chunk] + local * 32 + 16;
                ctx.queue.write_buffer(&self.instance_bufs[chunk], off, bytemuck::bytes_of(&color));
            }
        }
    }

    /// Recolors a file group in-place given its source bytes and AST/LSP byte spans.
    /// Resolves survivor glyph slots using the provided engine trie and writes colors
    /// directly to the GPU instance buffers.
    /// Returns the number of slots recolored.
    pub fn apply_file_spans(
        &self,
        ctx: &GpuContext,
        group_id: u32,
        file_bytes: &[u8],
        spans: &[crate::layout::ByteSpan],
        trie: &crate::atlas::TrieTable,
    ) -> usize {
        let (slot_base, slot_count) = if let Some(pctx) = &self.pick {
            if let Some(f) = pctx.files.iter().find(|f| f.group_id == group_id) {
                (f.slot_base, f.slot_count)
            } else {
                return 0;
            }
        } else if group_id == 0 {
            (0, self.instance_count)
        } else {
            return 0;
        };

        let colors = crate::layout_hyper::resolve_spans_to_slot_colors(file_bytes, spans, trie);
        let to_write = colors.len().min(slot_count as usize);
        self.write_slot_colors(ctx, slot_base, &colors[..to_write]);
        to_write
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

    pub(in crate::glyph_scene) fn camera_frame(&self, t: f32, aspect: f32) -> CamFrame {
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
                // Fly depth conditioning: keep near at 0.05 for single-glyph
                // closeups, while conditioning far to the scene bounds and distance
                // from the field center to prevent f32 depth precision collapse and Z-fighting.
                let d_center = (self.fly.eye - self.center).length();
                let far = (d_center + self.fit * 4.0).clamp(20_000.0, 100_000.0);
                (0.05, far)
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
        let proj = glam::camera::rh::proj::directx::perspective(fov, aspect, far, near);
        CamFrame {
            view_proj: proj * view,
            eye,
        }
    }

    // ── Stage G: picking & live manipulation ───────────────────────────────

    /// Group TRS from the CPU mirror: (offset, scale, rgb, alpha).
    pub(crate) fn group_trs(&self, gid: u32) -> Option<(Vec3, Vec3, [f32; 3], f32)> {
        let g = self.groups_cpu.get(gid as usize)?;
        Some((
            Vec3::new(g.cols[0][0], g.cols[0][1], g.cols[0][2]),
            Vec3::new(g.cols[3][0], g.cols[3][1], g.cols[3][2]),
            [g.cols[2][0], g.cols[2][1], g.cols[2][2]],
            g.cols[2][3],
        ))
    }

    pub(crate) fn group_hidden(&self, gid: u32) -> bool {
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
                    (i.aabb_min[2] + i.aabb_max[2]) * 0.5,
                ]
            });
        let Some(cl) = center_local else {
            self.grabbed_group = None;
            return;
        };
        let c = DVec3::new(
            cl[0] as f64 * sc.x as f64 + off.x as f64,
            cl[1] as f64 * sc.y as f64 + off.y as f64,
            cl[2] as f64 * sc.z as f64 + off.z as f64,
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
        render::render_scene(self, ctx, encoder, target, t);
    }
}

// ── Stage H (Phase 5) — encase layout assertions ────────────────────────────
// The WGSL lane maps (glyph_field.wgsl header: 12×4 B lanes; GROUP_STRIDE=5
