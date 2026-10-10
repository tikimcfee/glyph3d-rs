//! Stage K — the windowed debug-UI probe: the read-only state snapshot the
//! egui panel displays, the group-browser rows, and the probe's install
//! method. Extracted from `glyph_scene.rs` in the 2026-09 code-shape
//! refactor — a pure move.

use super::{CameraMode, GlyphScene};

/// Which LOD handle the Debug panel just moved (C26, 2026-10-10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LodHandle {
    /// "Text detail" (`greek_onset_px`).
    TextDetail,
    /// "Show glyphs" (`lod_min_px`).
    ShowGlyphs,
    /// "File rectangle", the Visible field's tier under Show glyphs
    /// (`lod_backdrop_px`).
    FileRectangle,
}

/// Keep the three LOD tiers ordered after `moved` changed: text detail >=
/// show glyphs >= file rectangle (all px per text row). The handle that
/// moved keeps its value and the others yield — a clamp, never an error —
/// so glyphs always appear fuzzed first and sharpen as the viewer
/// approaches. Only the panel calls this, on a slider change: values that
/// arrive from the CLI or a config file are applied as given.
pub fn keep_lod_order(moved: LodHandle, text_detail: &mut f32, show_glyphs: &mut f32, file_rectangle: &mut f32) {
    match moved {
        LodHandle::TextDetail => {
            *show_glyphs = show_glyphs.min(*text_detail);
            *file_rectangle = file_rectangle.min(*show_glyphs);
        }
        LodHandle::ShowGlyphs => {
            *text_detail = text_detail.max(*show_glyphs);
            *file_rectangle = file_rectangle.min(*show_glyphs);
        }
        LodHandle::FileRectangle => {
            *show_glyphs = show_glyphs.max(*file_rectangle);
            *text_detail = text_detail.max(*show_glyphs);
        }
    }
}

impl UiProbeState {
    /// `keep_lod_order` over this state's three live thresholds.
    pub fn keep_lod_order(&mut self, moved: LodHandle) {
        keep_lod_order(moved, &mut self.greek_onset_px, &mut self.lod_min_px, &mut self.lod_backdrop_px);
    }
}

#[cfg(test)]
mod lod_order_tests {
    use super::{keep_lod_order, LodHandle};

    fn after(moved: LodHandle, detail: f32, show: f32, rect: f32) -> (f32, f32, f32) {
        let (mut d, mut s, mut r) = (detail, show, rect);
        keep_lod_order(moved, &mut d, &mut s, &mut r);
        (d, s, r)
    }

    #[test]
    fn the_moved_handle_wins_and_the_others_yield() {
        // Show glyphs dragged above text detail: detail follows it up.
        assert_eq!(after(LodHandle::ShowGlyphs, 10.0, 12.0, 0.25), (12.0, 12.0, 0.25));
        // Text detail dragged below show glyphs: show glyphs follows it down,
        // and the file rectangle under it too.
        assert_eq!(after(LodHandle::TextDetail, 0.5, 1.0, 0.25), (0.5, 0.5, 0.25));
        assert_eq!(after(LodHandle::TextDetail, 0.1, 1.0, 0.25), (0.1, 0.1, 0.1));
        // File rectangle dragged above both: both follow it up.
        assert_eq!(after(LodHandle::FileRectangle, 2.0, 1.0, 3.0), (3.0, 3.0, 3.0));
        // Show glyphs dragged below the file rectangle: the rectangle follows.
        assert_eq!(after(LodHandle::ShowGlyphs, 10.0, 0.1, 0.25), (10.0, 0.1, 0.1));
    }

    #[test]
    fn an_ordered_set_is_untouched_whichever_handle_moved() {
        for moved in [LodHandle::TextDetail, LodHandle::ShowGlyphs, LodHandle::FileRectangle] {
            assert_eq!(after(moved, 10.0, 1.0, 0.25), (10.0, 1.0, 0.25), "{moved:?}");
            assert_eq!(after(moved, 1.0, 1.0, 1.0), (1.0, 1.0, 1.0), "{moved:?} (equal is ordered)");
        }
    }
}

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
    /// The environment mode actually drawn this frame (the B toggle lands
    /// here), so a relayout rebuild keeps it.
    pub environment: crate::config::EnvironmentMode,
    /// The last resolved pick, formatted by the same `format_pick` as the
    /// stdout pick log line.
    pub last_pick: Option<String>,
    /// The current selection in the field's own key (`Selection::describe`):
    /// `item:start..end` for the Visible field, the slot or slot range for
    /// the stored modes. None when nothing is selected.
    pub selection: Option<String>,
    /// The last pick's key, compact: `file byte N` for a glyph, `file (file)`
    /// for a file-level pick — what the Visible field's verbs address.
    pub pick_key: Option<String>,
    // ── K4: UI → scene controls. Written by the Debug-panel sliders;
    // applied by render() before culling. Seeded from the compile-time
    // consts at install; offscreen never installs a probe, so the consts
    // rule there. ──
    /// Live "Show glyphs" threshold, px per text row (default: `[lod]
    /// show_glyphs_px`): at or above it a segment is glyphs, below it a
    /// backdrop rectangle (visible mode: a line wash).
    pub lod_min_px: f32,
    /// Live "File rectangle" threshold of the Visible field, px per text row
    /// (default: `[lod] visible_backdrop_px`); lines between it and
    /// `lod_min_px` are washes. Unread by the other modes.
    pub lod_backdrop_px: f32,
    /// Live debug tint of the Visible field (`--debug-tint`): 0 off, 1 by
    /// LOD tier, 2 by cull state. Written to the Params uniform's spare
    /// lane; the other modes' shaders never read it.
    pub debug_tint: u32,
    /// Live file background cards toggle.
    pub file_backgrounds: bool,
    /// Live file background cards color.
    pub file_bg_color: [f32; 4],
    /// Live Greeking (anti-moiré) toggle.
    pub greeking: bool,
    /// Live Greeking pure bypass (hard cutoff, max FPS) toggle.
    pub greek_pure: bool,
    /// Live "Text detail" threshold, px per text row (default: `[lod]
    /// text_detail_px`): at or above it glyphs render with full curve
    /// detail, below it greeking fuzzes them progressively.
    pub greek_onset_px: f32,
    // ── K4: live cull readouts (scene → UI; the same sums GLYPH_CULL_DEBUG
    // prints). Zero when culling is disabled (--no-cull). ──
    pub cull_ranges: usize,
    pub cull_instances: u64,
    pub cull_backdrops: usize,
    // ── The field HUD (F8): per-frame readouts beside the K4 counters. ──
    /// The CPU segment cull's wall time this frame, ms.
    pub cull_cpu_ms: f32,
    /// Cull segments (one per file) and how many are user-hidden.
    pub segments: usize,
    pub hidden_segments: usize,
    /// The Visible field's own counters (None for the stored modes).
    pub visible_stats: Option<glyph_field_visible::VisibleStats>,
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
    /// Canvas layout arrangement mode (repo scenes): shelf vs carrel. None for non-repo scenes.
    pub layout_mode: Option<crate::repo::RepoLayoutMode>,
    /// Layout engine strategy (repo scenes): Hyper vs Direct vs Batch vs Naive.
    pub strategy: Option<crate::repo::Strategy>,
    /// Glyph field render mode: Instanced (32B) vs Derived (20B).
    pub field_mode: Option<glyph_field::GlyphFieldMode>,
    /// Currently grabbed group ID (file grab via `KeyG`).
    pub grabbed_group: Option<u32>,
    /// Currently grabbed zone / carrel ID (carrel grab via `KeyC`).
    pub grabbed_zone: Option<String>,
    /// The carrel / zone ID corresponding to the currently picked file.
    pub active_zone: Option<String>,
    /// Live snapshot of the active Agent Carrel (if any).
    pub carrel: Option<UiCarrelState>,
    // ── K5: group-browser data. `files` is STATIC (built once at install;
    // Rc-shared so the panel's per-frame snapshot clones a refcount, not the
    // rows). `file_dyn` is refreshed per frame (world pose under the live
    // group TRS, hidden flag, tint) — parallel to `files`. ──
    pub files: std::rc::Rc<Vec<UiFileRow>>,
    pub file_dyn: Vec<UiFileDyn>,
}

/// Read-only snapshot of an active Agent Carrel for the egui HUD.
#[derive(Clone, Debug, Default)]
pub struct UiCarrelState {
    pub session_id: String,
    pub active_turn: usize,
    pub turn_count: usize,
    pub active_beat: usize,
    pub beat_count: usize,
    pub prompt_summary: String,
    pub beat_summary: String,
    pub touched_files: Vec<(String, usize, usize)>, // (path, active_revision, revision_count)
    pub layout_options: crate::spatial_scene::CarrelLayoutOptions,
    pub window_item_range: (usize, usize), // (oldest_index_in_window + 1, newest_index_in_window + 1)
    pub max_deck_scroll: usize,
    pub max_desk_scroll: usize,
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

impl GlyphScene {
    /// Seed for the panel's cluster toggle on scenes without a pick context
    /// (text). Called by the scene builder between `new` and `init_ui_probe`.
    pub fn set_probe_cluster_mode(&mut self, on: bool) {
        self.probe_cluster_mode = Some(on);
    }

    /// Seed for the panel's layout mode toggle on repo scenes.
    /// Called by the scene builder between `new` and `init_ui_probe`.
    pub fn set_probe_layout_mode(&mut self, mode: crate::repo::RepoLayoutMode) {
        self.probe_layout_mode = Some(mode);
    }

    /// Seed for the panel's layout engine strategy on repo scenes.
    /// Called by the scene builder between `new` and `init_ui_probe`.
    pub fn set_probe_strategy(&mut self, strategy: crate::repo::Strategy) {
        self.probe_strategy = Some(strategy);
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
                        aabb_min: [f.aabb_min[0], f.aabb_min[1]],
                        aabb_max: [f.aabb_max[0], f.aabb_max[1]],
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
        let layout_mode = self.probe_layout_mode;
        let strategy = self.probe_strategy;
        let field_mode = Some(self.field.mode());
        let (file_backgrounds, file_bg_color, lod_min_px, lod_backdrop_px) = self
            .cull
            .as_ref()
            .map(|c| (c.file_backgrounds.get(), c.file_bg_color.get(), c.lod_min_px.get(), c.lod_backdrop_px.get()))
            .unwrap_or_else(|| {
                let s = crate::config::settings();
                (false, s.glyph_scene.file_bg_color, s.lod.show_glyphs_px, s.lod.visible_backdrop_px)
            });
        let mode = self.params.get().greek_mode;
        let greeking = mode != 0;
        let greek_pure = mode == 2;
        let greek_onset_px = self.params.get().greek_onset_px;
        let debug_tint = self.params.get().debug_tint;
        let probe = UiProbe::new(std::cell::RefCell::new(UiProbeState {
            lod_min_px,
            lod_backdrop_px,
            debug_tint,
            file_backgrounds,
            file_bg_color,
            greeking,
            greek_pure,
            greek_onset_px,
            files: std::rc::Rc::new(files),
            file_dyn,
            z_wrap_spacing,
            cluster_mode,
            layout_mode,
            strategy,
            field_mode,
            ..Default::default()
        }));
        self.ui_probe = Some(probe.clone());
        probe
    }
}
