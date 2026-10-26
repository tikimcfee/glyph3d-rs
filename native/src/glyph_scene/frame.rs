//! One frame's CPU half (2026-10-26): `GlyphScene::prepare_frame` computes
//! everything a frame draws — the camera, the environment and glyph
//! uniforms, the transform tree's resolve and the group rows, page faces
//! and regions it changed, the CPU cull's backdrops and glyph ranges, the
//! selection, the scene meshes — and the probe sync, with no wgpu type in
//! sight, into a [`FrameData`]. A frame's consumer uploads and draws it:
//! the wgpu renderer (`render.rs`, `encode_frame`) is one, a host drawing
//! the same passes with its own renderer (the makepad port's g3_passes) is
//! the other. Before this split, `render_scene` interleaved the two.
//!
//! **Generations.** Each slow group (the Params uniform, the group rows,
//! the page faces, the scene meshes) carries a generation that moves only
//! when its data changed, and the table groups carry the runs that changed
//! THIS frame (the runs the wgpu half writes, coalesced by
//! `nodes::upload_runs`): a consumer that skips a frame must accumulate
//! the runs until it catches up. The changes are handed over once: the
//! host tables forget them when the frame is prepared.
//!
//! **Borrows.** The tables are borrowed from the scene (`Ref` guards), not
//! copied: drop the frame before anything mutates the scene again (input,
//! verbs, animation), as both drivers do by preparing, drawing and
//! dropping it in one call.

use std::cell::Ref;
use std::ops::Range;

use super::camera::fov_y_deg;
use super::cull::{cull_segments, frustum_planes, BackdropInst, CullView, PhaseDraws};
use super::environment::{EnvCamera, EnvUniform};
use super::instance::{FrameUniform, Params};
use super::mesh::MeshInstance;
use super::nodes::UploadCost;
use super::pick::format_pick;
use super::regions::{RegionCost, RegionInstance, RegionParams};
use super::target::Selection;
use super::ui_probe::UiFileDyn;
use super::{GlyphScene, GroupRow};

/// Slot ranges per chunk, chunk-major and ascending within a chunk (the
/// order the glyph pass records, so the within-pixel blend order is fixed):
/// one flat list and each chunk's start in it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChunkRanges {
    ranges: Vec<Range<u32>>,
    /// `starts[k]..starts[k + 1]` are chunk k's ranges (`chunk_count() + 1`
    /// entries).
    starts: Vec<usize>,
}

impl ChunkRanges {
    /// From (chunk, range) pairs in chunk-major order over `chunk_count`
    /// chunks (a pair past the last chunk is refused).
    pub(super) fn from_pairs(pairs: &[(u32, Range<u32>)], chunk_count: u32) -> Self {
        let mut starts = Vec::with_capacity(chunk_count as usize + 1);
        let mut ranges = Vec::with_capacity(pairs.len());
        starts.push(0);
        for k in 0..chunk_count {
            ranges.extend(pairs.iter().filter(|(c, _)| *c == k).map(|(_, r)| r.clone()));
            starts.push(ranges.len());
        }
        assert_eq!(ranges.len(), pairs.len(), "every glyph range names a chunk below {chunk_count}");
        Self { ranges, starts }
    }

    pub fn chunk_count(&self) -> usize {
        self.starts.len().saturating_sub(1)
    }

    /// Chunk `k`'s ranges (empty past the last chunk).
    pub fn chunk(&self, k: usize) -> &[Range<u32>] {
        match (self.starts.get(k), self.starts.get(k + 1)) {
            (Some(&a), Some(&b)) => &self.ranges[a..b],
            _ => &[],
        }
    }

    /// Every chunk's ranges, in chunk order (`G3FrameInputs::chunk_ranges`).
    pub fn per_chunk(&self) -> Vec<&[Range<u32>]> {
        (0..self.chunk_count()).map(|k| self.chunk(k)).collect()
    }

    /// (chunk, range) pairs in record order (the wgpu glyph pass's form).
    pub fn pairs(&self) -> Vec<(u32, Range<u32>)> {
        (0..self.chunk_count()).flat_map(|k| self.chunk(k).iter().map(move |r| (k as u32, r.clone()))).collect()
    }

    /// Ranges over every chunk.
    pub fn range_count(&self) -> usize {
        self.ranges.len()
    }

    /// Slots over every range.
    pub fn slot_count(&self) -> u64 {
        self.ranges.iter().map(|r| u64::from(r.end - r.start)).sum()
    }
}

/// The selection a frame highlights: the selected glyphs' slot ranges per
/// chunk (drawn into the mask) and the tint the composite adds where the
/// mask covers. The Visible field's selection is a byte range it lays out
/// itself (`byte_range`, wgpu only); its `ranges` are empty.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameSelection {
    pub ranges: ChunkRanges,
    /// (item, start byte, end byte) for the Visible field.
    pub byte_range: Option<(u32, u32, u32)>,
    /// rgb (linear) and strength: `[glyph_scene] selection_tint`.
    pub tint: [f32; 4],
}

/// What the CPU half cost, for the instruments the drawing half reports
/// (`GLYPH_PROFILE`'s CPU scopes, `REGIONTIME`).
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameCosts {
    pub rows: UploadCost,
    pub faces: UploadCost,
    pub regions: RegionCost,
    /// The CPU cull, when the scene culls (0 under --no-cull).
    pub cull_ms: f32,
}

/// One frame, prepared: see the module header.
pub struct FrameData<'a> {
    // ── every frame ──
    pub viewport: [u32; 2],
    pub time: f32,
    /// proj × view, column-major, reverse-Z (depth clears to 0.0 and tests
    /// GREATER_OR_EQUAL).
    pub view_proj: [f32; 16],
    pub eye: [f32; 3],
    /// Pixels per world unit at distance 1 (height / 2·tan(fov/2)).
    pub px_scale: f32,
    /// The pass's clear colour (`[glyph_scene] clear_color`, linear).
    pub clear_color: [f64; 4],
    /// The environment's uniform (environment.wgsl `Env`); None: off.
    pub env: Option<EnvUniform>,
    /// The CPU cull's backdrop survivors.
    pub backdrops: Vec<BackdropInst>,
    /// The glyph slot ranges to draw, per chunk (empty for a field that
    /// culls itself).
    pub glyph_ranges: ChunkRanges,
    pub selection: Option<FrameSelection>,
    /// The library's region instances (empty without a library or with
    /// `[library] regions` off), the runs that changed this frame, the
    /// draw's dials and the picked region.
    pub regions: Option<Ref<'a, Vec<RegionInstance>>>,
    pub region_runs: Vec<Range<u32>>,
    pub region_params: Option<RegionParams>,
    pub picked_region: Option<usize>,
    // ── slow groups ──
    pub params_generation: u64,
    pub params: Params,
    /// The group table (96 B rows) and the runs changed this frame.
    pub groups_generation: u64,
    pub group_rows: Ref<'a, Vec<GroupRow>>,
    pub group_runs: Vec<Range<u32>>,
    /// The library's page faces and the runs changed this frame (None
    /// without a library or with `[library] page_faces` off).
    pub faces_generation: u64,
    pub faces: Option<Ref<'a, Vec<MeshInstance>>>,
    pub face_runs: Vec<Range<u32>>,
    /// The layout controller's scene meshes (quads, then cubes).
    pub meshes_generation: u64,
    pub quads: &'a [MeshInstance],
    pub cubes: &'a [MeshInstance],
    // ── what a host renderer draws the glyphs from (2026-10-26; the wgpu
    // half binds the field's own buffers) ──
    /// The field's slots on the host, in blocks that are its chunks
    /// (Derived; None for a field without them). A block's `Arc` identity
    /// is its generation: a write gives the block it touched a new one.
    pub slots: Option<Ref<'a, glyph_field::HostSlots>>,
    /// The Derived field's group overrides table (index 0 unused).
    pub group_overrides: Option<Ref<'a, [u32]>>,
    /// The item table the Derived shader reads, fixed for the scene's life.
    pub item_table: &'a [glyph_field::ItemParamsGpu],
    // ── for the wgpu half only ──
    /// The self-culling (Visible) field's per-frame inputs.
    pub field_prepare: Option<glyph_field::FramePrepare>,
    pub costs: FrameCosts,
    /// The frame uniform the stored fields' shaders read.
    pub(super) uniform: FrameUniform,
}

impl GlyphScene {
    /// The CPU half of a frame at `viewport` (physical px) and time `t`:
    /// see the module header. Also refreshes the windowed probe.
    pub fn prepare_frame(&self, viewport: (u32, u32), t: f32) -> FrameData<'_> {
        let (width, height) = viewport;
        // The transform tree first: the moved subtrees resolve and the group
        // rows they changed are planned before anything reads a row (the
        // cull below, the Visible field's prepare, the glyph pass).
        let (plan, group_runs) = self.nodes.prepare();
        // The library's page faces follow their sheets' resolved worlds: the
        // faces of the sheets the resolve reached (faces.rs).
        let face_runs = self.library_faces.as_ref().map(|f| f.prepare(&self.nodes.tables(), &plan)).unwrap_or_default();
        let aspect = width as f32 / height.max(1) as f32;
        self.viewport.set((width, height));
        let frame = self.camera_frame(t, aspect);

        // Stage K (K4): apply live UI controls BEFORE culling so a slider
        // drag takes effect this frame. This is the SINGLE write site of
        // CullState::lod_min_px, and it runs only when a windowed probe is
        // installed — offscreen never installs one, so offscreen culls with
        // the `[lod] show_glyphs_px` setting by construction (gate 6's
        // byte-equal PNGs are the proof).
        if let (Some(probe), Some(cull)) = (&self.ui_probe, &self.cull) {
            let p = probe.borrow();
            cull.lod_min_px.set(p.lod_min_px);
            cull.lod_backdrop_px.set(p.lod_backdrop_px);
            cull.file_backgrounds.set(p.file_backgrounds);
            cull.file_bg_color.set(p.file_bg_color);
        }
        if let Some(probe) = &self.ui_probe {
            let p = probe.borrow();
            let mode = if !p.greeking {
                0
            } else if p.greek_pure {
                2
            } else {
                1
            };
            self.set_greek_mode(mode);
            self.set_greek_onset_px(p.greek_onset_px);
            self.set_debug_tint(p.debug_tint);
        }
        // A field that culls and draws itself (Visible): the scene's
        // per-file glyph ranges are not built for it, its slot math is
        // never asked, and the CPU cull keeps only two jobs — the BACKDROP
        // quads (under the field's backdrop threshold, not the glyph one,
        // so a washed item is not also a backdrop) and the hidden flags.
        let self_drawing = self.field.draws_itself();

        // Stage L (L1): every lane of the frame uniform. px_scale is shared
        // with the cull below (the uniform needs it even under --no-cull).
        // flags bit 0 (deterministic_rendering) stays 0 — reserved.
        let px_scale = height as f32 / (2.0 * (fov_y_deg().to_radians() * 0.5).tan());
        let uniform = FrameUniform {
            view_proj: frame.view_proj.to_cols_array(),
            eye: frame.eye.to_array(),
            _pad0: 0.0,
            viewport: [width as f32, height as f32],
            px_scale,
            time: t,
            flags: 0,
            _pad1: 0,
        };
        // The self-culling field's per-frame inputs. The thresholds are the
        // cull cells (live from the panel in windowed runs, the `[lod]`
        // settings offscreen): the glyph tier is the existing
        // `show_glyphs_px`, the backdrop tier the visible mode's own.
        let field_prepare = self_drawing.then(|| {
            let p = self.params.get();
            let (lod_glyph_px, lod_backdrop_px) =
                self.cull.as_ref().map(|c| (c.lod_min_px.get(), c.lod_backdrop_px.get())).unwrap_or_else(|| {
                    let s = &crate::config::settings().lod;
                    (s.show_glyphs_px, s.visible_backdrop_px)
                });
            glyph_field::FramePrepare {
                view_proj: frame.view_proj.to_cols_array_2d(),
                eye: frame.eye.to_array(),
                viewport: [width as f32, height as f32],
                px_scale,
                lod_glyph_px,
                lod_backdrop_px,
                greek_mode: p.greek_mode,
                debug_tint: p.debug_tint,
                time: t,
            }
        });
        let env = self.environment.is_on().then(|| {
            let env_cam = EnvCamera { eye: frame.eye.as_dvec3(), vp_rel: frame.view_proj_rel, far: frame.far, fit: self.fit };
            self.environment.frame_uniform(&env_cam, self.environment.ground_y(self.scene_min_y))
        });

        // --- Stage F: CPU segment cull (frustum + LOD) --------------------
        // Stage L (L2): the cull output IS the phase lists. The legacy
        // --no-cull path builds the glyph ranges straight from the chunk
        // counts (one full range per chunk — identical draws to the pre-L2
        // per-chunk loop) and has no backdrops.
        let mut cull_ms = 0.0f32;
        let chunk_count = if self_drawing { 0 } else { self.field.chunk_count() };
        let phase_draws: PhaseDraws = if let Some(cull) = &self.cull {
            // The profiler's and the HUD's cull figure read the same clock.
            let cull_t0 = std::time::Instant::now();
            let view = CullView {
                planes: frustum_planes(&frame.view_proj),
                eye: frame.eye,
                px_scale,
                lod_min_px: if self_drawing { cull.lod_backdrop_px.get() } else { cull.lod_min_px.get() },
                file_backgrounds: cull.file_backgrounds.get(),
                file_bg_color: cull.file_bg_color.get(),
                backdrop_at_front: cull.backdrop_at_front,
            };
            // No chunks for a self-drawing field: zero chunks means the cull
            // builds no glyph range (and never asks the field for its slot
            // math).
            let chunk_cap = if self_drawing { 1 } else { self.field.chunk_capacity() };
            let phase_draws = match &cull.tree {
                // A library load: top-down over the transform tree, the flat
                // cull's draws without a per-file box write per move.
                Some(tree) => tree.borrow_mut().cull(&self.nodes.tables(), cull, &view, chunk_cap, chunk_count),
                None => cull_segments(&cull.segments, &cull.hidden, &cull.em_scale, &view, chunk_cap, chunk_count),
            };
            cull_ms = cull_t0.elapsed().as_secs_f32() * 1000.0;
            if std::env::var_os("GLYPH_CULL_DEBUG").is_some() && t == 0.0 {
                let insts: u64 = phase_draws.glyph_ranges.iter().map(|(_, r)| (r.end - r.start) as u64).sum();
                println!(
                    "CULLDBG glyph draws={} instances={} | backdrops={}",
                    phase_draws.glyph_ranges.len(),
                    insts,
                    phase_draws.backdrops.len(),
                );
            }
            phase_draws
        } else if self_drawing {
            // --no-cull on a self-drawing field: it still culls itself; the
            // scene simply has no backdrops to offer.
            PhaseDraws { backdrops: Vec::new(), glyph_ranges: Vec::new() }
        } else {
            PhaseDraws {
                backdrops: Vec::new(),
                glyph_ranges: self.field.chunk_glyph_counts().iter().enumerate().map(|(c, &n)| (c as u32, 0..n)).collect(),
            }
        };
        let glyph_ranges = ChunkRanges::from_pairs(&phase_draws.glyph_ranges, chunk_count);

        // The library's regions: the cull's refits re-pad their boxes, and
        // the regions whose box changed or whose directory resolved are
        // composed with their directories' worlds.
        let (region_cost, region_runs) = match (&self.library_regions, self.cull.as_ref().and_then(|c| c.tree.as_ref())) {
            (Some(regions), Some(tree)) => regions.prepare(&mut tree.borrow_mut(), &self.nodes.tables(), Some(&plan)),
            _ => (RegionCost::default(), Vec::new()),
        };

        let selection = self.selection.as_ref().map(|sel| FrameSelection {
            ranges: match sel {
                Selection::Glyph { chunk, local } => ChunkRanges::from_pairs(&[(*chunk, *local..*local + 1)], self.field.chunk_count()),
                // Per-chunk split — the same math cull_segments uses.
                Selection::Segment { slot_base, slot_count } => {
                    let pairs: Vec<(u32, Range<u32>)> = glyph_field::split_at_chunks(*slot_base, *slot_count, self.field.chunk_capacity())
                        .filter(|(chunk, _, _)| *chunk < self.field.chunk_count())
                        .map(|(chunk, local, _)| (chunk, local))
                        .collect();
                    ChunkRanges::from_pairs(&pairs, self.field.chunk_count())
                }
                Selection::ByteRange { .. } => ChunkRanges::default(),
            },
            byte_range: match sel {
                Selection::ByteRange { item, start, end } => Some((*item, *start, *end)),
                _ => None,
            },
            tint: crate::config::settings().glyph_scene.selection_tint,
        });

        self.sync_probe(&frame.eye, &glyph_ranges, phase_draws.backdrops.len(), cull_ms);

        let meshes = self.controller.as_ref().map(|ctrl| ctrl.scene.mesh_draws());
        let costs = FrameCosts {
            rows: self.nodes.last_flush().0,
            faces: if face_runs.is_empty() { UploadCost::default() } else { self.library_faces.as_ref().map(|f| f.last_update()).unwrap_or_default() },
            regions: region_cost,
            cull_ms,
        };
        FrameData {
            viewport: [width, height],
            time: t,
            view_proj: uniform.view_proj,
            eye: uniform.eye,
            px_scale,
            clear_color: crate::config::settings().glyph_scene.clear_color,
            env,
            backdrops: phase_draws.backdrops,
            glyph_ranges,
            selection,
            regions: self.library_regions.as_ref().map(|r| r.image()),
            region_runs,
            region_params: self.library_regions.as_ref().map(|r| r.params()),
            picked_region: self.library_regions.as_ref().and_then(|r| r.picked()).map(|r| r as usize),
            params_generation: self.params_generation.get(),
            params: self.params.get(),
            groups_generation: self.nodes.generation(),
            group_rows: self.nodes.rows(),
            group_runs,
            faces_generation: self.library_faces.as_ref().map_or(0, |f| f.generation()),
            faces: self.library_faces.as_ref().map(|f| f.image()),
            face_runs,
            meshes_generation: meshes.map_or(0, |m| m.revision),
            quads: meshes.map_or(&[], |m| &m.quads),
            cubes: meshes.map_or(&[], |m| &m.cubes),
            slots: self.field.host_slots(),
            group_overrides: self.field.group_overrides(),
            item_table: &self.item_table,
            field_prepare,
            costs,
            uniform,
        }
    }

    /// Stage K: refresh the windowed debug-UI probe (installed only by
    /// windowed runs; offscreen skips this entirely). Camera fields are
    /// this frame's ACTUAL products; the pick line is the same string
    /// format_pick produces for the stdout log; the K4 cull counters are
    /// the same sums GLYPH_CULL_DEBUG prints (zeros under --no-cull).
    fn sync_probe(&self, eye: &glam::Vec3, glyph_ranges: &ChunkRanges, backdrops: usize, cull_ms: f32) {
        let Some(probe) = &self.ui_probe else { return };
        let mut p = probe.borrow_mut();
        p.camera_mode = Some(self.camera_mode);
        p.eye = eye.to_array();
        p.yaw = self.fly.yaw;
        p.pitch = self.fly.pitch;
        p.environment = self.environment.mode.get();
        p.last_pick = self.picked.as_ref().map(|h| format_pick(h, self.views.group_row(h.view)));
        p.selection = self.selection.as_ref().map(Selection::describe);
        p.pick_key = self.picked.as_ref().map(|h| match &h.glyph {
            Some(g) => format!("{} byte {}", h.rel_path, g.byte_off),
            None => format!("{} (file)", h.rel_path),
        });
        p.grabbed_group = self.grabbed_group;
        p.grabbed_zone = self.grabbed_zone.clone();
        p.active_zone = self.picked.as_ref().map(|h| {
            if let Some(ctrl) = &self.controller {
                let dir = h.rel_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                ctrl.zone_for_file(&h.rel_path, dir)
            } else {
                format!("group:{}", self.views.group_row(h.view))
            }
        });
        p.carrel = self.controller.as_ref().and_then(|ctrl| {
            let carrel_e = ctrl.active_carrel?;
            let carrel_comp = ctrl.scene.world.get::<crate::spatial_scene::AgentCarrel>(carrel_e)?;
            let session = ctrl.session.as_ref()?;
            let prompt = session.turn_prompt(carrel_comp.active_turn).unwrap_or("—");
            let touched = if let Some(desk) = ctrl.scene.world.get::<crate::spatial_scene::workdesk::Workdesk>(carrel_comp.workdesk_entity) {
                let mut files: Vec<(String, usize, usize)> = desk.file_stacks.iter().map(|(path, &stack_e)| {
                    let (act, cnt) = if let Some(st) = ctrl.scene.world.get::<crate::spatial_scene::workdesk::FileRevisionStack>(stack_e) {
                        (st.active_revision, st.revision_count)
                    } else {
                        (0, 0)
                    };
                    (path.clone(), act, cnt)
                }).collect();
                files.sort_by(|a, b| a.0.cmp(&b.0));
                files
            } else {
                Vec::new()
            };
            let events = session.linearize_events(ctrl.revision_engine.as_ref());
            let beat_summary = events.get(carrel_comp.active_beat).map(|e| e.summary()).unwrap_or_default();

            let total_items = carrel_comp.beat_count;
            let limit = carrel_comp.layout_options.deck_window_limit.max(1);
            let max_deck_scroll = total_items.saturating_sub(limit);
            let k = carrel_comp.layout_options.deck_scroll_offset.min(max_deck_scroll);
            let m = limit.min(total_items.saturating_sub(k));
            let window_item_range = if total_items > 0 && m > 0 {
                let newest = total_items - k;
                let oldest = total_items - k - m + 1;
                (oldest, newest)
            } else {
                (0, 0)
            };

            let max_desk_scroll = ctrl.revision_engine.as_ref().map(|re| {
                re.all_histories().values().map(|h| h.revisions.len().saturating_sub(carrel_comp.layout_options.desk_revision_limit.max(1))).max().unwrap_or(0)
            }).unwrap_or(0);

            Some(crate::glyph_scene::UiCarrelState {
                session_id: carrel_comp.session_id.clone(),
                active_turn: carrel_comp.active_turn,
                turn_count: carrel_comp.turn_count,
                active_beat: carrel_comp.active_beat,
                beat_count: carrel_comp.beat_count,
                prompt_summary: prompt.to_string(),
                beat_summary,
                touched_files: touched,
                layout_options: carrel_comp.layout_options,
                window_item_range,
                max_deck_scroll,
                max_desk_scroll,
            })
        });
        if self.cull.is_some() {
            p.cull_ranges = glyph_ranges.range_count();
            p.cull_instances = glyph_ranges.slot_count();
            p.cull_backdrops = backdrops;
        } else {
            p.cull_ranges = 0;
            p.cull_instances = 0;
            p.cull_backdrops = 0;
        }
        // The HUD's readout: what the field is, what the CPU cull cost, and
        // the self-culling field's own counters (its readback lags a frame
        // or two; nothing here waits for it).
        p.field_mode = Some(self.field.mode());
        p.cull_cpu_ms = cull_ms;
        p.segments = self.cull.as_ref().map_or(0, |c| c.segments.len());
        p.hidden_segments = self.cull.as_ref().map_or(0, |c| c.hidden.iter().filter(|h| **h).count());
        p.visible_stats = self.field.visible().map(|v| v.stats());
        // The layout dial's readout: the field's depth extent over the
        // segment table (per frame, so group z-moves show too). None
        // under --no-cull — no segment table to measure.
        p.z_extent = self.cull.as_ref().map(|c| {
            c.segments.iter().fold([f32::INFINITY, f32::NEG_INFINITY], |[lo, hi], s| [lo.min(s.min[2]), hi.max(s.max[2])])
        });
        // K5: refresh the browser's dynamic row state (world pose under
        // the live group TRS, hidden, tint) for the rows the browser drew
        // last frame (`browser_rows`, a scrolled window of a dozen or so):
        // the pose from the transform tree (O(depth) each — a library
        // move's rows are rebuilt at the flush or a reader's sync, 2026-10-10),
        // the appearance from the group table (verbs rebuild their row at
        // once). Until 2026-10-10 every row was refreshed every frame,
        // O(files). Skipped entirely offscreen (no probe installed).
        if !p.files.is_empty() {
            let st = &mut *p;
            if st.file_dyn.len() != st.files.len() {
                st.file_dyn = vec![UiFileDyn::default(); st.files.len()];
            }
            for &i in &st.browser_rows {
                let (Some(r), Some(slot)) = (st.files.get(i), st.file_dyn.get_mut(i)) else { continue };
                let (Some(g), Some((o, s))) = (self.nodes.row(r.group_id), self.nodes.world_offset_scale(r.group_id)) else {
                    *slot = UiFileDyn::default();
                    continue;
                };
                let (ox, oy) = (o[0], o[1]);
                let (sx, sy) = (s[0].max(0.0), s[1].max(0.0));
                let wmin = [r.aabb_min[0] * sx + ox, r.aabb_min[1] * sy + oy];
                let wmax = [r.aabb_max[0] * sx + ox, r.aabb_max[1] * sy + oy];
                *slot = UiFileDyn {
                    center: [(wmin[0] + wmax[0]) * 0.5, (wmin[1] + wmax[1]) * 0.5],
                    half: [(wmax[0] - wmin[0]) * 0.5, (wmax[1] - wmin[1]) * 0.5],
                    hidden: g.cols[2][3] == 0.0,
                    tint: [
                        (g.cols[2][0].clamp(0.0, 1.0) * 255.0) as u8,
                        (g.cols[2][1].clamp(0.0, 1.0) * 255.0) as u8,
                        (g.cols[2][2].clamp(0.0, 1.0) * 255.0) as u8,
                    ],
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ChunkRanges;

    #[test]
    fn chunk_ranges_keep_record_order_and_hand_out_per_chunk_slices() {
        let pairs = vec![(0, 0..4), (0, 9..12), (2, 1..2)];
        let c = ChunkRanges::from_pairs(&pairs, 3);
        assert_eq!(c.chunk_count(), 3);
        assert_eq!(c.chunk(0), &[0..4, 9..12]);
        assert!(c.chunk(1).is_empty());
        assert_eq!(c.chunk(2), std::slice::from_ref(&(1..2)));
        assert!(c.chunk(7).is_empty());
        assert_eq!(c.pairs(), pairs);
        assert_eq!(c.per_chunk().len(), 3);
        assert_eq!((c.range_count(), c.slot_count()), (3, 8));
        assert_eq!(ChunkRanges::from_pairs(&[], 0).chunk_count(), 0);
    }
}
