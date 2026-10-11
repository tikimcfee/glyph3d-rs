//! The scene's group rows as nodes of the transform tree (2026-10-10;
//! step 1 of `out/DESIGN-VIEWS-AND-TRANSFORMS-2026-10-10.md`, transitional
//! form). `crates/glyph-scene-graph` holds the tables and the resolve pass;
//! this module maps the renderer's group ids onto it.
//!
//! **Every group row is a node** whose parent is one identity root, and
//! each node routes its resolved world transform and appearance into its
//! own row of the group table (`set_group_row`), in the columns the draw
//! shaders already read (cols 0-3; clip and background stay the host's). The
//! draw path, the slot formats and the row layout are untouched. Composition
//! under an identity parent is exact (every product with 1, every sum with
//! 0, returns its operand), so the resolve rewrites each row with the bits it
//! had and every golden frame stays byte-equal.
//!
//! **Writers.** A group verb writes ONE table: `move-group` and the `g` drag
//! a local row (the world delta converted into the parent's frame, which is
//! the identity today), `scale-group` and the grab wheel the local scale
//! (uniform when the result is, else the per-axis post scale), `tint-group`
//! / `tint-cycle` the appearance tint, `hide-group` / `show-group` its alpha.
//! The CPU mirror (`groups_cpu`, which pick, cull and the HUD read) is then
//! rewritten from the tables (`mirror`) — the same arithmetic the GPU runs.
//!
//! **External writers** — the bevy-backed layout controller (carrel zones,
//! decks, the library's animation) still flatten their own hierarchy into
//! `groups_cpu` and call `write_group_rows`, which now ADOPTS those rows
//! (`adopt`): their transform columns become the node's local row every
//! time, but their appearance columns only when the external writer changed
//! them since it last wrote (a `Visible` toggle, a new binding tint). So an
//! animation that moves a tinted or hidden file no longer erases the verb's
//! edit — the library probe's E4 — while a carrel that hides a card still
//! hides it. The library's own hierarchy moves onto these nodes next.

use std::cell::RefCell;

use glyph_scene_graph::{Appearance, NodeHandle, NodeTables, SceneGraphGpu, Similarity};

use super::GroupRow;
use crate::gpu::GpuContext;

/// A row's columns as (local, post scale, appearance). A uniform scale (all
/// three bit-equal) is the inherited uniform scale with post 1; anything
/// else is post scale under a unit uniform scale. Either way the resolve's
/// `post * scale` gives back the row's three values exactly.
fn decompose(row: &GroupRow) -> (Similarity, [f32; 3], Appearance) {
    let c = &row.cols;
    let s = [c[3][0], c[3][1], c[3][2]];
    let uniform = s[0].to_bits() == s[1].to_bits() && s[1].to_bits() == s[2].to_bits();
    let local = Similarity { translation: [c[0][0], c[0][1], c[0][2]], scale: if uniform { s[0] } else { 1.0 }, rotation: c[1] };
    let post = if uniform { [1.0; 3] } else { s };
    (local, post, Appearance { tint: c[2], blend: c[3][3], ..Appearance::IDENTITY })
}

/// The appearance columns an external writer put in a row (bit patterns,
/// so the change test is exact).
fn ext_appearance(row: &GroupRow) -> [u32; 5] {
    let c = &row.cols;
    [c[2][0].to_bits(), c[2][1].to_bits(), c[2][2].to_bits(), c[2][3].to_bits(), c[3][3].to_bits()]
}

pub(crate) struct GroupNodes {
    /// Interior mutability for the flush, which runs from `render(&self)`.
    tables: RefCell<NodeTables>,
    /// None only in the device-less unit tests.
    gpu: Option<RefCell<SceneGraphGpu>>,
    root: NodeHandle,
    /// Group id → node.
    nodes: Vec<NodeHandle>,
    /// Group id → the appearance the external writer last wrote.
    ext: Vec<[u32; 5]>,
    /// The last flush's (bytes, resolved, dispatched), for the instruments.
    last_flush: std::cell::Cell<(u64, u32, bool)>,
    /// Moved subtree roots whose group rows the CPU mirror has not taken
    /// yet (`defer_mirror`; each slot once, `pending_mark`).
    pending: Vec<NodeHandle>,
    pending_mark: Vec<bool>,
}

impl GroupNodes {
    /// One node per row of `groups`, under an identity root, each writing
    /// its row of `group_buf` (`max_rows` rows of 96 B).
    pub(crate) fn new(device: &wgpu::Device, group_buf: &wgpu::Buffer, max_rows: u32, groups: &[GroupRow]) -> Self {
        let mut gpu = SceneGraphGpu::new(device);
        gpu.set_group_output(device, group_buf, max_rows);
        Self { gpu: Some(RefCell::new(gpu)), ..Self::cpu_only(groups) }
    }

    fn cpu_only(groups: &[GroupRow]) -> Self {
        let mut tables = NodeTables::default();
        let root = tables.insert(None, Similarity::IDENTITY, Appearance::IDENTITY).expect("a fresh pool accepts a root");
        let mut this = Self {
            tables: RefCell::new(tables),
            gpu: None,
            root,
            nodes: Vec::with_capacity(groups.len()),
            ext: Vec::with_capacity(groups.len()),
            last_flush: std::cell::Cell::new((0, 0, false)),
            pending: Vec::new(),
            pending_mark: Vec::new(),
        };
        for row in groups {
            this.push_row(row);
        }
        this
    }

    /// A node for the next group id (a load's rows, or one the glyph verbs
    /// allocated).
    pub(crate) fn push_row(&mut self, row: &GroupRow) {
        let gid = self.nodes.len() as u32;
        let (local, post, app) = decompose(row);
        let t = &mut self.tables.get_mut();
        let h = t.insert(Some(self.root), local, app).expect("the root is live");
        if post != [1.0; 3] {
            t.set_post_scale(h, post).expect("just inserted");
        }
        t.set_group_row(h, Some(gid)).expect("just inserted");
        self.nodes.push(h);
        self.ext.push(ext_appearance(row));
    }

    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Move group `gid` by a WORLD-space delta: converted into its parent's
    /// frame (the identity root today, where the conversion is exact).
    pub(crate) fn translate(&mut self, gid: u32, d: [f32; 3]) -> bool {
        let Some(&h) = self.nodes.get(gid as usize) else { return false };
        let t = &mut self.tables.get_mut();
        let dl = t.parent_world(h).expect("group nodes stay live").world_delta_to_local(d);
        let l = t.local(h).expect("group nodes stay live");
        let moved = [l.translation[0] + dl[0], l.translation[1] + dl[1], l.translation[2] + dl[2]];
        t.set_local(h, Similarity { translation: moved, ..l }).expect("live");
        true
    }

    /// Scale group `gid` by `f` per axis (clamped as the verbs always have),
    /// in its own frame. Returns the new x scale.
    pub(crate) fn scale_by(&mut self, gid: u32, f: f32) -> Option<f32> {
        let &h = self.nodes.get(gid as usize)?;
        let t = &mut self.tables.get_mut();
        let l = t.local(h).expect("live");
        let p = t.post_scale(h).expect("live").xyz;
        let s = [0, 1, 2].map(|k| (p[k] * l.scale * f).clamp(0.001, 100.0));
        let uniform = s[0].to_bits() == s[1].to_bits() && s[1].to_bits() == s[2].to_bits();
        let (scale, post) = if uniform { (s[0], [1.0; 3]) } else { (1.0, s) };
        t.set_local(h, Similarity { scale, ..l }).expect("live");
        if post != p {
            t.set_post_scale(h, post).expect("live");
        }
        Some(s[0])
    }

    /// Tint group `gid` (the appearance table only).
    pub(crate) fn set_tint(&mut self, gid: u32, rgb: [f32; 3]) -> bool {
        let Some(&h) = self.nodes.get(gid as usize) else { return false };
        let t = &mut self.tables.get_mut();
        let a = t.appearance(h).expect("live");
        t.set_appearance(h, Appearance { tint: [rgb[0], rgb[1], rgb[2], a.tint[3]], ..a }).expect("live");
        true
    }

    /// Set group `gid`'s own alpha (hide = 0, show = 1).
    pub(crate) fn set_alpha(&mut self, gid: u32, alpha: f32) -> bool {
        let Some(&h) = self.nodes.get(gid as usize) else { return false };
        let t = &mut self.tables.get_mut();
        let a = t.appearance(h).expect("live");
        t.set_appearance(h, Appearance { tint: [a.tint[0], a.tint[1], a.tint[2], alpha], ..a }).expect("live");
        true
    }

    /// Take a row an external writer (the layout controller's sync) left in
    /// the CPU mirror: its transform always, its appearance only if that
    /// writer changed it since its last write; then rewrite the row from the
    /// tables, so the mirror shows what the GPU will.
    pub(crate) fn adopt(&mut self, gid: u32, row: &mut GroupRow) {
        let Some(&h) = self.nodes.get(gid as usize) else { return };
        let (local, post, app) = decompose(row);
        let ext = ext_appearance(row);
        let t = &mut self.tables.get_mut();
        t.set_local(h, local).expect("live");
        if t.post_scale(h).expect("live").xyz != post {
            t.set_post_scale(h, post).expect("live");
        }
        if self.ext[gid as usize] != ext {
            t.set_appearance(h, app).expect("live");
            self.ext[gid as usize] = ext;
        }
        self.mirror(gid, row);
    }

    /// Rewrite `row`'s columns 0-3 from the tables, in the resolve pass's
    /// arithmetic (column 0's w, and columns 4-5, are left as they are).
    pub(crate) fn mirror(&self, gid: u32, row: &mut GroupRow) {
        let Some(&h) = self.nodes.get(gid as usize) else { return };
        let t = self.tables.borrow();
        let (w, alpha) = t.world(h).expect("live");
        let p = t.post_scale(h).expect("live").xyz;
        let a = t.appearance(h).expect("live");
        write_row(row, &w, alpha, p, &a);
    }

    /// Upload the frame's node edits and resolve the moved subtrees into the
    /// group table — first thing in the frame's encoder, before any pass
    /// reads a group row. A settled scene dispatches nothing. Returns what
    /// the flush did (None without a device); the last one is also kept
    /// for the instruments (`last_flush`).
    pub(crate) fn flush(&self, ctx: &GpuContext, encoder: &mut wgpu::CommandEncoder) -> Option<glyph_scene_graph::FlushStats> {
        let gpu = self.gpu.as_ref()?;
        let stats = gpu.borrow_mut().flush(&ctx.device, &ctx.queue, encoder, &mut self.tables.borrow_mut());
        if stats.dispatched && ctx.profiler.is_some() {
            crate::gpu::record_cpu_scope(ctx, "group nodes flush (CPU)", (stats.cpu_plan_us + stats.cpu_encode_us) / 1e3);
        }
        self.last_flush.set((stats.bytes, stats.resolved, stats.dispatched));
        Some(stats)
    }

    /// The last flush's (bytes queued, nodes resolved, dispatched) — the
    /// previous frame's, when read from `animate`.
    pub(crate) fn last_flush(&self) -> (u64, u32, bool) {
        self.last_flush.get()
    }

    /// The resolve pass's world table (one 32 B `Similarity` per pool slot;
    /// replaced whenever the pool outgrows it — `FlushStats::grew`).
    pub(crate) fn world_buffer(&self) -> Option<wgpu::Buffer> {
        Some(self.gpu.as_ref()?.borrow().world_buffer().clone())
    }

    pub(crate) fn tables_mut(&mut self) -> &mut NodeTables {
        self.tables.get_mut()
    }

    pub(crate) fn tables(&self) -> std::cell::Ref<'_, NodeTables> {
        self.tables.borrow()
    }

    /// The identity root every group hangs under.
    pub(crate) fn root(&self) -> NodeHandle {
        self.root
    }

    /// Group `gid`'s node.
    pub(crate) fn node(&self, gid: u32) -> Option<NodeHandle> {
        self.nodes.get(gid as usize).copied()
    }

    /// Group ids `0..n` — a repo load's files — as nodes, in group order.
    pub(crate) fn file_nodes(&self, n: usize) -> &[NodeHandle] {
        &self.nodes[..n]
    }

    /// Note moved nodes whose subtrees' group rows the CPU mirror will take
    /// when a reader asks (`sync_mirror`) — O(1) a node, nothing per row: a
    /// directory drag no longer rewrites a row per file it carries every
    /// frame (2026-10-10). The GPU resolve draws the move regardless.
    pub(crate) fn defer_mirror(&mut self, moved: &[NodeHandle]) {
        for &h in moved {
            let i = h.index() as usize;
            if i >= self.pending_mark.len() {
                self.pending_mark.resize(i + 1, false);
            }
            if !self.pending_mark[i] {
                self.pending_mark[i] = true;
                self.pending.push(h);
            }
        }
    }

    /// Bring `groups` up to date with every deferred move (each moved
    /// subtree resolved once, `mirror_subtrees`). Returns the rows written.
    pub(crate) fn sync_mirror(&mut self, groups: &mut [GroupRow]) -> usize {
        if self.pending.is_empty() {
            return 0;
        }
        let pending = std::mem::take(&mut self.pending);
        for h in &pending {
            self.pending_mark[h.index() as usize] = false;
        }
        self.mirror_subtrees(&pending, groups).len()
    }

    /// Group `gid`'s live world offset and per-axis scale, composed from the
    /// tree on demand (O(depth); the bits the mirror would hold).
    pub(crate) fn world_offset_scale(&self, gid: u32) -> Option<([f32; 3], [f32; 3])> {
        let &h = self.nodes.get(gid as usize)?;
        let t = self.tables.borrow();
        let (w, _) = t.world(h).ok()?;
        let p = t.post_scale(h).ok()?.xyz;
        Some((w.translation, [p[0] * w.scale, p[1] * w.scale, p[2] * w.scale]))
    }

    /// The CPU mirror of a move: every node in the subtrees of `moved`
    /// resolved once on the CPU (O(1) a node; `NodeTables::resolve_subtrees`)
    /// and each one that routes to a group row rewritten into `groups` —
    /// columns 0-3, exactly as `mirror` writes them. Returns the group ids
    /// rewritten, ascending in depth-first order.
    pub(crate) fn mirror_subtrees(&mut self, moved: &[NodeHandle], groups: &mut [GroupRow]) -> Vec<u32> {
        let t = self.tables.get_mut();
        let mut hits: Vec<(u32, Similarity, f32, u32)> = Vec::new();
        t.resolve_subtrees(moved, |slot, row, w, a| {
            if let Some(r) = row {
                hits.push((r, *w, a, slot));
            }
        });
        let mut out = Vec::with_capacity(hits.len());
        for (gid, w, alpha, _) in hits {
            let Some(&h) = self.nodes.get(gid as usize) else { continue };
            let Some(row) = groups.get_mut(gid as usize) else { continue };
            let p = t.post_scale(h).expect("live").xyz;
            let a = t.appearance(h).expect("live");
            write_row(row, &w, alpha, p, &a);
            out.push(gid);
        }
        out
    }
}

/// Columns 0-3 of a group row from a node's world, inherited alpha, post
/// scale and appearance — the resolve pass's output, on the CPU (column 0's
/// w, and columns 4-5, are left as they are).
fn write_row(row: &mut GroupRow, w: &Similarity, alpha: f32, p: [f32; 3], a: &Appearance) {
    row.cols[0] = [w.translation[0], w.translation[1], w.translation[2], row.cols[0][3]];
    row.cols[1] = w.rotation;
    row.cols[2] = [a.tint[0], a.tint[1], a.tint[2], alpha];
    row.cols[3] = [p[0] * w.scale, p[1] * w.scale, p[2] * w.scale, a.blend];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(t: [f32; 3], q: [f32; 4], tint: [f32; 4], s: [f32; 3], blend: f32) -> GroupRow {
        GroupRow { cols: [[t[0], t[1], t[2], 0.0], q, tint, [s[0], s[1], s[2], blend], [1.0, -1.0, 1.0, 0.0], [0.1, 0.2, 0.3, 0.4]] }
    }

    fn rows() -> Vec<GroupRow> {
        vec![
            row([12.5, -167.8, 3.0], [0.0, 0.0, 0.0, 1.0], [0.82, 0.71, 0.55, 1.0], [1.0; 3], 0.0),
            row([0.0, 0.1, -20.0], [0.1, -0.2, 0.3, 0.927_361_85], [1.0, 1.0, 1.0, 0.0], [0.0086; 3], 1.0),
            row([7.0, 7.0, 7.0], [0.0, 0.0, 0.0, 1.0], [0.3, 0.6, 0.9, 1.0], [2.0, 0.5, 1.0], 0.0),
        ]
    }

    /// The CPU half of the identity-root claim (the GPU half is
    /// glyph-scene-graph's `group_rows_under_an_identity_root_...`): every
    /// row mirrored back from its node is the same bits, uniform or per-axis
    /// scale alike — and each verb through the node path lands exactly
    /// where the old in-place edit of the row did.
    #[test]
    fn rows_through_their_nodes_come_back_bit_for_bit_and_verbs_match_the_old_edits() {
        let rows = rows();
        let mut n = GroupNodes::cpu_only(&rows);
        for (gid, r) in rows.iter().enumerate() {
            let mut back = *r;
            n.mirror(gid as u32, &mut back);
            assert_eq!(bytemuck::bytes_of(&back), bytemuck::bytes_of(r), "row {gid}");
        }
        let mut old = rows.clone();
        old[0].cols[0][0] += -12.7;
        old[0].cols[0][1] += -167.8;
        old[0].cols[0][2] += -20.0;
        assert!(n.translate(0, [-12.7, -167.8, -20.0]));
        for c in 0..3 {
            old[2].cols[3][c] = (old[2].cols[3][c] * 1.5).clamp(0.001, 100.0);
            old[1].cols[3][c] = (old[1].cols[3][c] * 0.5).clamp(0.001, 100.0);
        }
        assert_eq!(n.scale_by(2, 1.5), Some(3.0));
        assert!(n.scale_by(1, 0.5).is_some());
        old[1].cols[2] = [1.0, 0.188_235_3, 0.188_235_3, old[1].cols[2][3]];
        assert!(n.set_tint(1, [1.0, 0.188_235_3, 0.188_235_3]));
        old[2].cols[2][3] = 0.0;
        assert!(n.set_alpha(2, 0.0));
        for (gid, want) in old.iter().enumerate() {
            let mut got = rows[gid];
            n.mirror(gid as u32, &mut got);
            assert_eq!(bytemuck::bytes_of(&got), bytemuck::bytes_of(want), "row {gid} after the verbs");
        }
        assert!(!n.translate(9, [1.0; 3]), "an unknown group is refused");
    }

    /// E4: an external writer that moves a group carries its transform in,
    /// but not the appearance it did not change — a verb's tint and hide
    /// survive the move; a change the writer DID make (a carrel's `Visible`
    /// toggle) still lands.
    #[test]
    fn an_external_move_keeps_a_verbs_tint_and_hide() {
        let rows = rows();
        let mut n = GroupNodes::cpu_only(&rows);
        n.set_tint(0, [1.0, 0.0, 0.0]);
        n.set_alpha(0, 0.0);
        // The layout controller's sync: a new offset, its own binding tint.
        let mut synced = rows[0];
        synced.cols[0] = [40.0, 2.0, -3.0, 0.0];
        let mut mirror = synced;
        n.adopt(0, &mut mirror);
        assert_eq!(mirror.cols[0], [40.0, 2.0, -3.0, 0.0], "the move is taken");
        assert_eq!(mirror.cols[2], [1.0, 0.0, 0.0, 0.0], "the verb's tint and hide survive");
        // The writer hides the group itself (its sync writes alpha 0, scale 0).
        let mut hidden = synced;
        hidden.cols[2] = [0.0; 4];
        hidden.cols[3] = [0.0; 4];
        let mut mirror = hidden;
        n.adopt(0, &mut mirror);
        assert_eq!(mirror.cols[2], [0.0; 4], "a change the writer made lands");
        assert_eq!(n.len(), 3);
    }

    /// The deferred CPU mirror (2026-10-10, the library on the tree): a
    /// moved parent's subtree is noted, not mirrored — the rows stay as
    /// they were until a reader syncs — and the sync writes exactly the
    /// rows an eager mirror (`mirror`, per group) writes, each once.
    #[test]
    fn a_deferred_mirror_syncs_to_the_eager_rows() {
        let rows = rows();
        let mut n = GroupNodes::cpu_only(&rows);
        // Nest groups 0 and 2 under a scaled, offset parent, as the library
        // nests a file under its mount.
        let root = n.root();
        let (g0, g2) = (n.node(0).unwrap(), n.node(2).unwrap());
        let t = n.tables_mut();
        let parent = t.insert(Some(root), Similarity { scale: 0.25, ..Similarity::from_translation([3.0, -4.0, 5.0]) }, Appearance::IDENTITY).unwrap();
        t.reparent(g0, Some(parent)).unwrap();
        t.reparent(g2, Some(parent)).unwrap();
        let mut groups = rows.clone();
        n.defer_mirror(&[parent]);
        n.defer_mirror(&[parent, g0]);
        assert_eq!(bytemuck::cast_slice::<_, u8>(&groups), bytemuck::cast_slice::<_, u8>(&rows), "deferred: nothing written yet");
        assert_eq!(n.sync_mirror(&mut groups), 2, "each moved group's row once");
        for gid in 0..3u32 {
            let mut want = rows[gid as usize];
            n.mirror(gid, &mut want);
            assert_eq!(bytemuck::bytes_of(&groups[gid as usize]), bytemuck::bytes_of(&want), "row {gid}");
        }
        assert_ne!(groups[0].cols[0], rows[0].cols[0], "the nested row moved");
        assert_eq!(n.sync_mirror(&mut groups), 0, "nothing waits after a sync");
        let off = n.world_offset_scale(2).unwrap();
        assert_eq!(off.0, [groups[2].cols[0][0], groups[2].cols[0][1], groups[2].cols[0][2]]);
        assert_eq!(off.1, [groups[2].cols[3][0], groups[2].cols[3][1], groups[2].cols[3][2]]);
    }
}
