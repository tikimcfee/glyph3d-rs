use std::cell::Cell;
use std::sync::Arc;

use glam::Vec3;

use crate::atlas::Atlas;
use crate::layout::EmittedSlots;
use crate::text::StagedText;
use glyph_field::{GlyphField, GlyphFieldMode, SlotSource};
use glyph_field_derived::DerivedField;
use super::{
    CameraMode, CullState, FlyCamera, GlyphScene,
    cull::SegCull,
    instance::{Params, GlyphInstance, GroupRow},
    camera::fit_distance,
    tint::seg_tint,
};

impl GlyphScene {
    /// The scene over `staged`, with no device type (2026-10-31): a
    /// renderer is built from it afterwards (`GlyphRenderer::new` for wgpu;
    /// the makepad host draws its frames itself). It keeps `atlas` for that.
    pub fn new(
        atlas: Arc<Atlas>,
        mut staged: StagedText,
        camera_mode: CameraMode,
        cull_enabled: bool,
        field_mode: GlyphFieldMode,
    ) -> Self {

        // --- instance + group buffers --------------------------------------
        let mut arena = staged.instances;
        if arena.is_empty() && !arena.is_emitted() {
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

        let t_scene_start = std::time::Instant::now();
        let instances_len = arena.len();

        let max_groups = groups.len().max(65536);
        // The rows as nodes under an identity root: the first frame's resolve
        // rebuilds columns 0-3 of every row with the bits a renderer uploads
        // when it is built, so only a row a library load moved goes up again.
        let mut nodes = super::nodes::GroupNodes::new(&groups);
        // Each view's node is its group row's (`crate::views`).
        let mut views = staged.views;
        views.attach_nodes(|group_row| nodes.node(group_row));
        // A library load moves its hierarchy onto those nodes (2026-10-10):
        // file i's node — view i's — goes under its page's mount, and the
        // library eases its directories and sheets by writing their local
        // rows. Its page faces are drawn from the sheets' resolved worlds.
        let library_faces = staged.controller.as_mut().and_then(|c| c.library.as_mut()).and_then(|lib| {
            assert_eq!(lib.file_count(), views.len(), "a library load registers one view per file");
            let files = views.nodes().to_vec();
            let root = nodes.root();
            lib.attach(nodes.tables_mut(), Some(root), &files);
            let spec = lib.face?;
            let slots: Vec<u32> = lib.nodes.as_ref().expect("just attached").sheet.iter().map(|h| h.index()).collect();
            Some(super::faces::LibraryFaces::new(&spec, &slots))
        });


        let sheet = &atlas.emoji.sheet;
        let params = Params {
            max_groups: max_groups as u32,
            greek_mode: 2,
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
            greek_onset_px: crate::config::settings().lod.text_detail_px,
            _pad3: 0,
        };
        // The item table is the registry's (one row per view, its group lane
        // the view's group row), and so is the map a host record's group
        // row is read through to find its item.
        let item_table = views.item_table();
        let item_of_group = views.item_of_group_rows(groups.len());
        // The arena's forms map onto the field's sources. An EMITTED arena is
        // already the field's own slots: 20 B DerivedSlots in host blocks the
        // Derived field keeps. A HOST arena is the engine's neutral records,
        // which the field transcodes into its blocks. Either way the field
        // keeps its slots on the host and its draw uploads them
        // (`DerivedFieldGpu`). (Until 2026-10-31 a load past the Derived
        // lanes fell back to the Instanced field, and the field built here
        // followed the arena; a file past a lane is now laid out empty.)
        let source = match arena.take_emitted() {
            Some(EmittedSlots { slots, .. }) => SlotSource::Blocks(slots),
            None => SlotSource::Host { slices: arena.instance_chunks(), glyph_count: instances_len },
        };
        let block_slots = crate::config::settings().glyph_scene.derived_block_slots as usize;
        let field: Box<dyn GlyphField> = match field_mode {
            GlyphFieldMode::Derived => Box::new(DerivedField::new(source, &item_table, &item_of_group, block_slots)),
        };
        let field_dur = t_scene_start.elapsed();
        let t_pipe_start = std::time::Instant::now();
        log::info!(
            "glyph field ({}): {} instances ({} MiB) in {} chunk(s) of ≤{}, {} groups",
            field.mode(),
            instances_len,
            (instances_len * field.slot_bytes() as usize) >> 20,
            field.chunk_count(),
            field.chunk_capacity(),
            groups.len(),
        );

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
            fit_distance(half_h_needed)
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
                slot_count: instances_len as u32,
                tint: seg_tint(
                    arena.instances(),
                    staged.bounds_max[0] - staged.bounds_min[0],
                    staged.bounds_max[1] - staged.bounds_min[1],
                    &atlas.slot_ink,
                ),
                blocks: Vec::new(),
            });
        }
        // multi_draw_indirect is core in wgpu 30 — but its Metal backend
        // silently drops any indirect draw with first_instance != 0 (see the
        // module header), so Stage F culls on the CPU instead; --no-cull
        // keeps the legacy per-chunk full draws for A/B.
        let mut library_regions = None;
        // Each segment's group row, as the registry answers it: the one its
        // views share, or none (a cover segment over several groups, a
        // segment no view uses).
        let segment_group_rows: Vec<Option<u32>> = (0..segments.len() as u32).map(|s| views.group_row_of_segment(s)).collect();
        let cull = if cull_enabled {
            let mut cull = CullState::new(&segments, &groups, &segment_group_rows);
            let library_root = staged.controller.as_ref().and_then(|c| c.library.as_ref()).and_then(|l| l.nodes.as_ref()).map(|n| n.root);
            cull.backdrop_at_front = library_root.is_some();
            if let Some(root) = library_root {
                // Segment i's node and staged colour, through its group row
                // (a library load: file i's view).
                let seg_nodes: Vec<_> = segment_group_rows.iter().map_while(|g| g.and_then(|g| nodes.node(g))).collect();
                let rgb: Vec<[f32; 4]> =
                    segment_group_rows.iter().map(|g| g.and_then(|g| groups.get(g as usize)).map_or([1.0; 4], |row| row.cols[2])).collect();
                let mut tree = super::tree_cull::TreeCull::new(nodes.tables_mut(), root, &seg_nodes, &cull, &rgb);
                // Bounding regions (2026-10-10, regions.rs): each directory's
                // box is its tree-cull box padded, so the cull logs its
                // refits for them, and — so a box encloses its pages — each
                // sheet's box takes its page face.
                let lib = staged.controller.as_ref().and_then(|c| c.library.as_ref()).expect("a library root has a library");
                if let Some(spec) = super::regions::RegionSpec::from_settings(&crate::config::settings().library) {
                    let lnodes = lib.nodes.as_ref().expect("attached above");
                    if let Some(f) = lib.face {
                        let face = [[-f.size[0] / 2.0, -f.size[1] / 2.0, -f.gap], [f.size[0] / 2.0, f.size[1] / 2.0, -f.gap]];
                        tree = tree.with_own_box(&nodes.tables(), &lnodes.sheet, face, &cull);
                    }
                    tree.log_refits();
                    let table = super::regions::RegionTable::new(&lib.plan, lnodes, &tree, spec);
                    library_regions = Some(super::regions::LibraryRegions::new(table));
                }
                cull.tree = Some(std::cell::RefCell::new(tree));
            }
            Some(cull)
        } else {
            log::info!("culling disabled (--no-cull) — legacy per-chunk draws");
            None
        };


        let pick = staged.pick;
        if let Some(pctx) = &pick {
            assert_eq!(pctx.files.len(), views.len(), "a pick record per view: PickContext::files is indexed by view");
        }
        let controller = staged.controller;
        let tint_step = vec![0u32; groups.len()];
        let pipe_dur = t_pipe_start.elapsed();
        log::info!(
            "scene timings: field build {:.3}s | cull init {:.3}s | total scene {:.3}s",
            field_dur.as_secs_f64(),
            pipe_dur.as_secs_f64(),
            (field_dur + pipe_dur).as_secs_f64(),
        );

        Self {
            atlas,
            field,
            center,
            half_w,
            half_h,
            fit,
            camera_mode,
            fly,
            cull,
            nodes,
            views,
            pick,
            probe_cluster_mode: None,
            probe_layout_mode: None,
            probe_strategy: None,
            picked: None,
            selection: None,
            geom_overrides: std::collections::HashMap::new(),
            cache: None,
            grabbed_group: None,
            controller,
            grabbed_zone: None,
            cursor: (0.0, 0.0),
            drag_pending: None,
            viewport: Cell::new((1600, 1000)), // refreshed every render()
            tint_step,
            ui_probe: None,
            params: Cell::new(params),
            params_generation: Cell::new(0),
            library_faces,
            library_regions,
            picked_region: None,
            scene_min_y: staged.bounds_min[1],
            environment: super::environment::Environment::new(),
            item_table,
            library_tick_ms: Cell::new((0.0, 0.0)),
        }
    }

    /// Configure Greeking mode: 0 = disabled, 1 = smooth fade, 2 = pure hard
    /// bypass. Like every Params setter it changes the host's Params only;
    /// the next frame carries it (`FrameData::params`).
    pub fn set_greek_mode(&self, mode: u32) {
        let mut p = self.params.get();
        if p.greek_mode != mode {
            p.greek_mode = mode;
            self.set_params(p);
        }
    }

    /// The Params uniform's host copy, and a new generation for the frame.
    fn set_params(&self, p: Params) {
        self.params.set(p);
        self.params_generation.set(self.params_generation.get() + 1);
    }

    /// Configure whether Greeking (anti-Moiré subpixel bars) is enabled.
    pub fn set_greeking(&self, on: bool) {
        let cur = self.params.get().greek_mode;
        let mode = if on {
            if cur == 1 { 1 } else { 2 }
        } else {
            0
        };
        self.set_greek_mode(mode);
    }

    /// Configure the on-screen glyph height in px/em where Greeking begins (default: 10.0).
    pub fn set_greek_onset_px(&self, onset: f32) {
        let mut p = self.params.get();
        if (p.greek_onset_px - onset).abs() > 1e-3 {
            p.greek_onset_px = onset;
            self.set_params(p);
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

    /// Set the environment mode and an explicit ground height (None = the
    /// scene's lowest point minus `[environment] ground_gap`).
    pub fn set_environment(&mut self, mode: crate::config::EnvironmentMode, ground_y: Option<f32>) {
        self.environment.mode.set(mode);
        self.environment.ground_y_override.set(ground_y);
    }

    /// Set the LOD minimum pixel threshold.
    pub fn set_lod_min_px(&mut self, lod: f32) {
        if let Some(cull) = &self.cull {
            cull.lod_min_px.set(lod);
        }
    }
}

#[cfg(all(test, feature = "wgpu"))]
mod tests {
    use std::sync::Arc;

    use crate::glyph_scene::{CameraMode, GlyphRenderer, GlyphScene, PickCommand};
    use crate::repo::{self, RepoParams, Strategy};
    use crate::scene::SceneLike;
    use glyph_field::GlyphFieldMode;

    /// g-pick-repo loaded as a Derived scene with its wgpu renderer on
    /// `ctx` (built before the edits, so the device holds the load), then
    /// edited by the verbs that write slots (a glyph's colour, x and group,
    /// a line, a whole file through the highlight sidecar), with the host
    /// blocks as they were before the edits.
    fn edited_derived_scene(ctx: &crate::gpu::GpuContext) -> (GlyphScene, GlyphRenderer, Vec<Arc<[u32]>>) {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/g-pick-repo");
        let params = RepoParams { field_mode: GlyphFieldMode::Derived, ..Default::default() };
        let prefetched = repo::prefetch_repo(&dir, params, Strategy::Hyper);
        let home = crate::budget::SlotHome::of(ctx);
        let load = repo::load_repo_from_prefetched(&dir, prefetched, &params, Strategy::Hyper, Some(&home), crate::layout::GlyphArena::new(), None);
        let atlas = crate::atlas::Atlas::load(&crate::default_emoji_sheet());
        let staged = load.into_staged(None, &atlas.slot_ink);
        let mut scene = GlyphScene::new(Arc::new(atlas), staged, CameraMode::Front { zoom: 1.0 }, true, GlyphFieldMode::Derived);
        scene.set_viewport(crate::OFFSCREEN_WIDTH, crate::OFFSCREEN_HEIGHT);
        let renderer = GlyphRenderer::new(ctx, wgpu::TextureFormat::Rgba8UnormSrgb, &scene);

        let held: Vec<Arc<[u32]>> = scene.field.host_slots().blocks().to_vec();
        assert!(held.len() >= 2, "g-pick-repo spans more than one block");

        let pick = SceneLike::apply_pick(&mut scene, &PickCommand::RowCol { file: "alpha.rs".into(), row: 1, col: 4 });
        assert!(pick.is_some_and(|l| l.contains("alpha.rs")), "the pick resolves in alpha.rs");
        for verb in ["recolor-glyph ff0000", "nudge-glyph 0.5 0", "set-glyph-transform 1 2 0", "recolor-line 00ff00", "reset-glyph-group"] {
            let verb = crate::parse_verb(verb).expect("a verb literal parses");
            SceneLike::apply_verb(&mut scene, &verb).expect("the scene applies the verb");
        }
        let sidecar = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/highlight-alpha.tsv");
        SceneLike::apply_highlight_sidecar(&scene, &sidecar).expect("the sidecar applies");
        (scene, renderer, held)
    }

    /// The Derived field keeps its slots in host blocks (2026-10-26), and
    /// since 2026-10-31 its edits land there alone: the wgpu draw's device
    /// copy follows at the frame's sync, block by block. A load of
    /// g-pick-repo (two blocks at the default 262144 slots) is edited
    /// ([`edited_derived_scene`]); before the sync the edited block's buffer
    /// still holds the load and the other's equals its block, and after it
    /// every buffer equals its block.
    #[test]
    fn derived_host_blocks_follow_every_slot_write() {
        let ctx = pollster::block_on(crate::gpu::init(None));
        let (scene, renderer, _held) = edited_derived_scene(&ctx);
        let device_copy = |k: usize| {
            let host = scene.field.host_slots();
            let mut gpu = vec![0u32; host.blocks()[k].len()];
            renderer.field.read_slot_words(&ctx.device, &ctx.queue, (k * host.block_slots()) as u32, &mut gpu, crate::config::settings().gpu.wait_limit());
            gpu
        };
        let blocks = scene.field.host_slots().blocks().len();
        let stale: Vec<usize> = (0..blocks).filter(|&k| device_copy(k)[..] != scene.field.host_slots().blocks()[k][..]).collect();
        assert_eq!(stale, [0], "before the sync only the edited block (alpha.rs's, block 0) differs on the device");
        let field = &scene.field;
        renderer.field.sync(&ctx.queue, &field.host_slots(), &field.group_overrides(), field.group_overrides_version());
        for k in 0..blocks {
            assert!(device_copy(k)[..] == scene.field.host_slots().blocks()[k][..], "block {k}: the host block and its buffer differ after the sync");
        }
    }

    /// The edits touch one block (all are in alpha.rs), and only it moves:
    /// its version, and, held as a makepad host holds the blocks, its
    /// identity (a fresh allocation, copy-on-write). An untouched block
    /// republished or re-uploaded would go up for nothing.
    #[test]
    fn edits_move_only_the_block_they_touch() {
        let ctx = pollster::block_on(crate::gpu::init(None));
        let (scene, _renderer, held) = edited_derived_scene(&ctx);
        let host = scene.field.host_slots();
        let moved: Vec<usize> = (0..host.blocks().len()).filter(|&k| host.version(k) != 0).collect();
        assert_eq!(moved, [0], "only alpha.rs's block has a new version");
        for (k, block) in host.blocks().iter().enumerate() {
            let fresh = !Arc::ptr_eq(block, &held[k]);
            assert_eq!(fresh, k == 0, "block {k}: identity moved {fresh}");
            if fresh {
                assert!(block[..] != held[k][..], "block {k} changed identity with no change of content");
            }
        }
    }
}
