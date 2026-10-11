//! The scene's side of `--layout-mode library` (`crate::library`): the
//! library verbs, its zone (`c`) grabs, and the per-frame animation step.
//!
//! The library lives in the scene's transform tree (`nodes.rs`; since
//! 2026-10-10 — before, a bevy hierarchy flattened on the CPU every frame
//! anything moved, ~88 % of a drag or page-turn frame, findings E16). A
//! move writes the LOCAL rows of the nodes that moved: a page turn its
//! sheets, a `c` drag its directory, a `g` drag its file. The frame's flush
//! uploads those rows and the GPU resolve composes the moved subtrees into
//! the group rows the draw shaders read; the page faces follow on the GPU
//! (`faces.rs`). Nothing on the CPU is per moved FILE either: the move is
//! noted on the tree cull (`tree_cull.rs`, which refits its bounds along
//! the moved nodes' ancestors and computes moved files' boxes as it walks)
//! and deferred for the CPU mirror of the group rows, which a reader (pick,
//! a verb, the windowed HUD) brings up to date when it asks
//! (`GroupNodes::defer_mirror` / `sync_group_mirror`).
//!
//! `GLYPH_LIBRARY_TIMING=1` prints one `LIBTIME` line per animated frame:
//! what moved, what each stage cost, and the bytes the last flush queued.

use std::time::Instant;

use super::GlyphScene;
use crate::gpu::GpuContext;
use crate::library::LibraryVerb;

/// What noting a library move cost on the CPU, after the node writes.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct MoveCost {
    /// Moved nodes noted (their subtrees are what moved).
    pub(super) nodes: usize,
    /// Deferring the mirror and marking the tree cull.
    pub(super) note_ms: f64,
}

impl GlyphScene {
    pub(super) fn apply_library_verb(&mut self, verb: &LibraryVerb) -> String {
        // The library's file index is the view's (one view per file, in
        // file order).
        let picked = self.picked.as_ref().map(|h| h.view as usize);
        let Some(ctrl) = self.controller.as_mut() else {
            return "library: not a repo scene".to_string();
        };
        let mode = ctrl.mode;
        let Some(lib) = ctrl.library.as_mut() else {
            return format!("library: this load's layout mode is {mode}, not library (--layout-mode library)");
        };
        let t = self.nodes.tables();
        match *verb {
            LibraryVerb::Page(p) => lib.page(&t, picked, p),
            LibraryVerb::Form(f) => lib.form(&t, picked, f),
            LibraryVerb::Stack(s) => lib.set_stack(&t, s),
            LibraryVerb::Sort(s, r) => lib.set_sort(&t, s, r),
        }
    }

    /// Whether this scene holds a library (the windowed keys route to it).
    pub(super) fn has_library(&self) -> bool {
        self.controller.as_ref().is_some_and(|c| c.library.is_some())
    }

    fn library(&self) -> Option<&crate::library::Library> {
        self.controller.as_ref()?.library.as_ref()
    }

    /// Note a tree move: the moved nodes' subtrees are deferred for the CPU
    /// mirror and marked on the tree cull (when the scene culls from the
    /// tree). O(moved nodes), never O(the files they carry).
    pub(super) fn note_tree_move(&mut self, moved: &[glyph_scene_graph::NodeHandle]) -> MoveCost {
        let t0 = Instant::now();
        self.nodes.defer_mirror(moved);
        if let Some(tree) = self.cull.as_ref().and_then(|c| c.tree.as_ref()) {
            let mut tree = tree.borrow_mut();
            for h in moved {
                tree.note_moved(h.index());
            }
        }
        MoveCost { nodes: moved.len(), note_ms: t0.elapsed().as_secs_f64() * 1e3 }
    }

    /// Bring the CPU mirror of the group rows (`groups_cpu`: pick, the
    /// verbs, the HUD) up to date with every deferred move. Returns the rows
    /// written (0 when nothing waited).
    pub(super) fn sync_group_mirror(&mut self) -> usize {
        self.nodes.sync_mirror(&mut self.groups_cpu)
    }

    /// A library zone's (`dir:<path>`) world translation, from the tree.
    pub(super) fn library_zone_translation(&self, zone_id: &str) -> Option<[f32; 3]> {
        let h = self.library()?.zone_node(zone_id)?;
        Some(self.nodes.tables().world(h).ok()?.0.translation)
    }

    /// Move a library zone by a WORLD delta: its directory node's local row
    /// takes the delta in its parent's frame (one row; the GPU carries the
    /// subtree), and the library keeps it through every later re-plan
    /// (`Library::move_zone`). Returns the move's cost, or None for no such
    /// zone.
    pub(super) fn library_move_zone(&mut self, zone_id: &str, d: [f32; 3]) -> Option<(f64, MoveCost)> {
        let t0 = Instant::now();
        // The library keeps the drag (its user offset): no re-plan undoes it.
        let lib = self.controller.as_mut()?.library.as_mut()?;
        let h = lib.move_zone(self.nodes.tables_mut(), zone_id, d)?;
        let move_ms = t0.elapsed().as_secs_f64() * 1e3;
        Some((move_ms, self.note_tree_move(&[h])))
    }

    /// Scale a library zone by `f` (its directory node's uniform scale).
    pub(super) fn library_scale_zone(&mut self, zone_id: &str, f: f32) -> bool {
        let Some(h) = self.library().and_then(|l| l.zone_node(zone_id)) else { return false };
        let t = self.nodes.tables_mut();
        let Ok(l) = t.local(h) else { return false };
        if t.set_local(h, glyph_scene_graph::Similarity { scale: l.scale * f, ..l }).is_err() {
            return false;
        }
        self.note_tree_move(&[h]);
        true
    }

    /// One animation step: nothing at all when the library is settled (or
    /// absent — every other scene returns on the first line).
    pub(crate) fn animate_library(&mut self, ctx: &GpuContext, dt: f32) {
        let Some(ctrl) = self.controller.as_mut() else { return };
        let Some(lib) = ctrl.library.as_mut() else { return };
        let Some(tick) = lib.tick(self.nodes.tables_mut(), dt) else { return };
        let cost = self.note_tree_move(&tick.moved);
        if ctx.profiler.is_some() {
            crate::gpu::record_cpu_scope(ctx, "library ease (CPU)", tick.ease_ms);
            crate::gpu::record_cpu_scope(ctx, "library move notes (CPU)", cost.note_ms);
        }
        if std::env::var_os("GLYPH_LIBRARY_TIMING").is_some() {
            let (flush_bytes, flush_resolved, _) = self.nodes.last_flush();
            let (cull_ms, visited, cands) = self.last_tree_cull();
            println!(
                "LIBTIME nodes_moved={} ease_ms={:.4} note_ms={:.4} local_bytes={} last_flush_bytes={} last_flush_resolved={} \
                 last_cull_ms={:.4} last_cull_visited={} last_cull_candidates={} animating={}",
                tick.nodes_moved,
                tick.ease_ms,
                cost.note_ms,
                tick.nodes_moved * std::mem::size_of::<glyph_scene_graph::Similarity>(),
                flush_bytes,
                flush_resolved,
                cull_ms,
                visited,
                cands,
                tick.still_animating,
            );
        }
    }

    /// The last tree cull's CPU time and walk (nodes visited, candidate
    /// segments), zeros without one.
    pub(super) fn last_tree_cull(&self) -> (f64, usize, usize) {
        match self.cull.as_ref().and_then(|c| c.tree.as_ref()) {
            Some(t) => {
                let t = t.borrow();
                (t.last_ms, t.last_walk.0, t.last_walk.1)
            }
            None => (0.0, 0, 0),
        }
    }
}
