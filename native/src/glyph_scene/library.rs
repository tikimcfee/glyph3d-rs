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
//! (`faces.rs`). What remains on the CPU per moving frame is the mirror the
//! CPU readers need — each moved file's row in `groups_cpu` (pick, the HUD)
//! and its cull segment (`sync_segment`) — resolved at O(1) a node from the
//! moved subtrees (`GroupNodes::mirror_subtrees`).
//!
//! `GLYPH_LIBRARY_TIMING=1` prints one `LIBTIME` line per animated frame:
//! what moved, what each stage cost, and the bytes the last flush queued.

use std::time::Instant;

use super::GlyphScene;
use crate::gpu::GpuContext;
use crate::library::LibraryVerb;

/// What a library move cost on the CPU, after the node writes.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct MirrorCost {
    /// Group rows rewritten in the CPU mirror.
    pub(super) groups: usize,
    /// The moved subtrees resolved on the CPU into `groups_cpu`.
    pub(super) mirror_ms: f64,
    /// `sync_segment` per moved file.
    pub(super) seg_ms: f64,
}

impl GlyphScene {
    pub(super) fn apply_library_verb(&mut self, verb: &LibraryVerb) -> String {
        let picked = self.picked.as_ref().map(|h| h.group_id as usize);
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

    /// The CPU mirror of a library move: the files under `moved` get their
    /// group rows (from the tree) and their cull segments.
    pub(super) fn mirror_library_move(&mut self, ctx: &GpuContext, moved: &[glyph_scene_graph::NodeHandle]) -> MirrorCost {
        let t0 = Instant::now();
        let gids = self.nodes.mirror_subtrees(moved, &mut self.groups_cpu);
        let mirror_ms = t0.elapsed().as_secs_f64() * 1e3;
        let t1 = Instant::now();
        for &g in &gids {
            self.sync_segment(ctx, g);
        }
        MirrorCost { groups: gids.len(), mirror_ms, seg_ms: t1.elapsed().as_secs_f64() * 1e3 }
    }

    /// A library zone's (`dir:<path>`) world translation, from the tree.
    pub(super) fn library_zone_translation(&self, zone_id: &str) -> Option<[f32; 3]> {
        let h = self.library()?.zone_node(zone_id)?;
        Some(self.nodes.tables().world(h).ok()?.0.translation)
    }

    /// Move a library zone by a WORLD delta: its directory node's local row
    /// takes the delta in its parent's frame (one row; the GPU carries the
    /// subtree). Returns the move's cost, or None for no such zone.
    pub(super) fn library_move_zone(&mut self, ctx: &GpuContext, zone_id: &str, d: [f32; 3]) -> Option<(f64, MirrorCost)> {
        let h = self.library()?.zone_node(zone_id)?;
        let t0 = Instant::now();
        let t = self.nodes.tables_mut();
        let dl = t.parent_world(h).ok()?.world_delta_to_local(d);
        let l = t.local(h).ok()?;
        let moved = [l.translation[0] + dl[0], l.translation[1] + dl[1], l.translation[2] + dl[2]];
        t.set_local(h, glyph_scene_graph::Similarity { translation: moved, ..l }).ok()?;
        let move_ms = t0.elapsed().as_secs_f64() * 1e3;
        Some((move_ms, self.mirror_library_move(ctx, &[h])))
    }

    /// Scale a library zone by `f` (its directory node's uniform scale).
    pub(super) fn library_scale_zone(&mut self, ctx: &GpuContext, zone_id: &str, f: f32) -> bool {
        let Some(h) = self.library().and_then(|l| l.zone_node(zone_id)) else { return false };
        let t = self.nodes.tables_mut();
        let Ok(l) = t.local(h) else { return false };
        if t.set_local(h, glyph_scene_graph::Similarity { scale: l.scale * f, ..l }).is_err() {
            return false;
        }
        self.mirror_library_move(ctx, &[h]);
        true
    }

    /// One animation step: nothing at all when the library is settled (or
    /// absent — every other scene returns on the first line).
    pub(crate) fn animate_library(&mut self, ctx: &GpuContext, dt: f32) {
        let Some(ctrl) = self.controller.as_mut() else { return };
        let Some(lib) = ctrl.library.as_mut() else { return };
        let Some(tick) = lib.tick(self.nodes.tables_mut(), dt) else { return };
        let cost = self.mirror_library_move(ctx, &tick.moved);
        if ctx.profiler.is_some() {
            crate::gpu::record_cpu_scope(ctx, "library ease (CPU)", tick.ease_ms);
            crate::gpu::record_cpu_scope(ctx, "library mirror+segments (CPU)", cost.mirror_ms + cost.seg_ms);
        }
        if std::env::var_os("GLYPH_LIBRARY_TIMING").is_some() {
            let (flush_bytes, flush_resolved, _) = self.nodes.last_flush();
            println!(
                "LIBTIME nodes_moved={} groups_synced={} ease_ms={:.4} mirror_ms={:.4} seg_ms={:.4} local_bytes={} \
                 last_flush_bytes={} last_flush_resolved={} animating={}",
                tick.nodes_moved,
                cost.groups,
                tick.ease_ms,
                cost.mirror_ms,
                cost.seg_ms,
                tick.nodes_moved * std::mem::size_of::<glyph_scene_graph::Similarity>(),
                flush_bytes,
                flush_resolved,
                tick.still_animating,
            );
        }
    }
}
