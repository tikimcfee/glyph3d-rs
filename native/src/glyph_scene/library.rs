//! The scene's side of `--layout-mode library` (`crate::library`): the
//! library verbs, and the per-frame animation step that carries the eased
//! transforms to the GPU through the paths every group edit already takes —
//! `sync_to_group_rows` (the changed files' flattened transforms into the
//! CPU group mirror), `write_group_rows` (one upload), and `sync_segment`
//! per moved file (the CPU cull box, and in the Visible field the item's
//! world box: `item_visible` tests a host-set box, so a group that moved
//! without it would be culled where it used to be).
//!
//! `GLYPH_LIBRARY_TIMING=1` prints one `LIBTIME` line per animated frame:
//! what moved, what each stage cost, and the bytes it queued.

use std::time::Instant;

use super::GlyphScene;
use crate::gpu::GpuContext;
use crate::library::LibraryVerb;

/// Bytes and `write_buffer` calls `write_group_rows` issues for `gids` (its
/// span-or-rows rule, mirrored for the instrument).
fn group_upload(gids: &[u32]) -> (u64, usize) {
    const ROW: u64 = std::mem::size_of::<crate::glyph_scene::GroupRow>() as u64;
    match gids.len() {
        0 => (0, 0),
        1 => (ROW, 1),
        n => {
            let lo = *gids.iter().min().expect("non-empty") as u64;
            let hi = *gids.iter().max().expect("non-empty") as u64;
            let span = hi - lo + 1;
            if span <= n as u64 * 4 || n > 8 {
                (span * ROW, 1)
            } else {
                (n as u64 * ROW, n)
            }
        }
    }
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
        match *verb {
            LibraryVerb::Page(p) => lib.page(&ctrl.scene, picked, p),
            LibraryVerb::Form(f) => lib.form(&ctrl.scene, picked, f),
            LibraryVerb::Stack(s) => lib.set_stack(&ctrl.scene, s),
            LibraryVerb::Sort(s, r) => lib.set_sort(&ctrl.scene, s, r),
        }
    }

    /// Whether this scene holds a library (the windowed keys route to it).
    pub(super) fn has_library(&self) -> bool {
        self.controller.as_ref().is_some_and(|c| c.library.is_some())
    }

    /// One animation step: nothing at all when the library is settled (or
    /// absent — every other scene returns on the first line).
    pub(crate) fn animate_library(&mut self, ctx: &GpuContext, dt: f32) {
        let Some(ctrl) = self.controller.as_mut() else { return };
        let Some(lib) = ctrl.library.as_mut() else { return };
        let Some(tick) = lib.tick(&mut ctrl.scene, dt) else { return };
        let t_sync = Instant::now();
        let gids = ctrl.scene.sync_to_group_rows(&mut self.groups_cpu);
        let quads = ctrl.scene.mesh_draws().quads.len();
        let sync_ms = t_sync.elapsed().as_secs_f64() * 1e3;
        let t_up = Instant::now();
        self.write_group_rows(ctx, &gids);
        let upload_ms = t_up.elapsed().as_secs_f64() * 1e3;
        let t_seg = Instant::now();
        for &g in &gids {
            self.sync_segment(ctx, g);
        }
        let seg_ms = t_seg.elapsed().as_secs_f64() * 1e3;
        if ctx.profiler.is_some() {
            crate::gpu::record_cpu_scope(ctx, "library ease+propagate (CPU)", tick.ease_ms + tick.propagate_ms);
            crate::gpu::record_cpu_scope(ctx, "library sync+upload+segments (CPU)", sync_ms + upload_ms + seg_ms);
        }
        if std::env::var_os("GLYPH_LIBRARY_TIMING").is_some() {
            let (group_bytes, group_writes) = group_upload(&gids);
            let visible = self.field.visible().is_some();
            let mesh_bytes = if tick.nodes_moved > 0 {
                (quads * std::mem::size_of::<crate::glyph_scene::mesh::MeshInstance>()) as u64
            } else {
                0
            };
            println!(
                "LIBTIME nodes_moved={} groups_synced={} ease_ms={:.4} propagate_ms={:.4} sync_ms={:.4} upload_ms={:.4} seg_ms={:.4} \
                 group_bytes={} group_writes={} item_box_bytes={} item_box_writes={} mesh_bytes={} animating={}",
                tick.nodes_moved,
                gids.len(),
                tick.ease_ms,
                tick.propagate_ms,
                sync_ms,
                upload_ms,
                seg_ms,
                group_bytes,
                group_writes,
                if visible { gids.len() * 24 } else { 0 },
                if visible { gids.len() } else { 0 },
                mesh_bytes,
                tick.still_animating,
            );
        }
    }
}
