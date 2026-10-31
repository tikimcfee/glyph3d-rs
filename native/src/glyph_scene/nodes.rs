//! The scene's group rows as nodes of the transform tree (2026-10-10;
//! step 1 of `out/DESIGN-VIEWS-AND-TRANSFORMS-2026-10-10.md`).
//! `crates/glyph-scene-graph` holds the tables and the resolve; this module
//! maps the renderer's group rows onto it and holds THE group table: the
//! one host copy (`rows`) that pick, cull and the HUD read, that the resolve
//! writes, and whose changed rows are uploaded to the GPU's (2026-10-26;
//! until then the scene kept a second copy, `groups_cpu`, synced to this
//! one). Everything here is keyed by GROUP ROW (`gid`), never by view:
//! which group row a view draws with is the view registry's answer
//! (`crate::views`).
//!
//! **Every group row is a node** whose parent is one identity root, and
//! each node routes its resolved world transform and appearance into its
//! own row of the group table (`set_group_row`), in the columns the draw
//! shaders already read (cols 0-3; column 0's w, clip and background are
//! the row's as it was allocated). The draw path, the slot formats and the
//! row layout are untouched. Composition under an identity parent is exact
//! (every product with 1, every sum with 0, returns its operand), so the
//! resolve rebuilds each row with the bits it had and every golden frame
//! stays byte-equal.
//!
//! **The frame's flush**, first thing in the frame, in two halves (the
//! frame's CPU half, `frame.rs`, and its drawing half): `prepare` resolves
//! the moved subtrees on the CPU (`NodeTables::resolve_frame`), rebuilds
//! the row of each resolved node with a group row in the table, and plans
//! the runs that the rows whose bytes changed since the last flush
//! (`unsent`: the flush's, and any written between frames) go up in
//! (`upload_runs`); the frame hands those runs to its consumer, and
//! `upload` writes them for wgpu. Between a write and the next flush the table is
//! therefore what the readers see and the GPU table is a frame behind by
//! exactly `unsent`. A settled frame resolves nothing and uploads nothing.
//! The resolved ranges are handed on: the library's
//! page faces and regions rebuild from the same nodes (`faces.rs`,
//! `regions.rs`). Until 2026-10-26 the dirty input rows went up instead and
//! a compute pass resolved them into the table; the renderer's default path
//! now dispatches no compute, the shape a draw-only port (makepad) can run.
//!
//! **Writers.** A group verb writes ONE table: `move-group` and the `g` drag
//! a local row (the world delta converted into the parent's frame, which is
//! the identity today), `scale-group` and the grab wheel the local scale
//! (uniform when the result is, else the per-axis post scale), `tint-group`
//! / `tint-cycle` the appearance tint, `hide-group` / `show-group` its alpha.
//! The row is then rebuilt from the tables at once (`rebuild_row`), the
//! arithmetic the flush runs. A tree move (the library's) is noted instead
//! (`defer_rows`) and its subtrees' rows rebuilt when a reader asks before
//! the flush (`sync_rows`), or by the flush itself.
//!
//! **External writers** — the bevy-backed layout controller (carrel zones,
//! decks) still flatten their own hierarchy into the table's rows
//! (`external_rows`) and call `write_group_rows`, which ADOPTS each row
//! (`adopt`): its transform columns become the node's local row every time,
//! its appearance columns only when the external writer changed them since
//! it last wrote (a `Visible` toggle, a new binding tint), and the row is
//! rebuilt from the tables. So an animation that moves a tinted or hidden
//! file does not erase the verb's edit — the library probe's E4 — while a
//! carrel that hides a card still hides it.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::time::Instant;

use glyph_scene_graph::upload::{plan_upload, UploadPlan};
use glyph_scene_graph::{Appearance, FramePlan, NodeHandle, NodeTables, Similarity};

use super::GroupRow;

/// What one host table's upload did and cost (the instruments:
/// `GLYPH_LIBRARY_TIMING`, the profiler's CPU scopes).
#[derive(Clone, Copy, Debug, Default)]
pub struct UploadCost {
    /// Rows whose bytes changed.
    pub rows: usize,
    /// Bytes the runs cover (whole runs: a run may carry clean rows).
    pub bytes: usize,
    /// Host time: rebuilding the rows, planning and queueing the writes.
    pub ms: f64,
}

/// The runs `rows` (ascending, unique) of a `len`-row table go up in, by
/// `plan_upload`'s choice: a few coalesced runs, or the whole table as one.
/// What a frame hands every consumer of a host table (`FrameData`): the
/// wgpu half writes exactly these, a host copies exactly these.
pub(crate) fn upload_runs(rows: &[u32], len: u32) -> Vec<Range<u32>> {
    match plan_upload(rows, len) {
        UploadPlan::None => Vec::new(),
        UploadPlan::Full => std::iter::once(0..len).collect(),
        UploadPlan::Runs(runs) => runs.into_iter().map(|(first, n)| first..first + n).collect(),
    }
}

/// Rows covered by `runs`.
pub(crate) fn run_rows(runs: &[Range<u32>]) -> usize {
    runs.iter().map(|r| r.len()).sum()
}

#[cfg(feature = "wgpu")]
/// Write `runs` of a host table `data` (`row_bytes` a row) into `buffer`.
pub(super) fn write_runs(queue: &wgpu::Queue, buffer: &wgpu::Buffer, data: &[u8], row_bytes: usize, runs: &[Range<u32>]) {
    for r in runs {
        let (a, b) = (r.start as usize * row_bytes, r.end as usize * row_bytes);
        queue.write_buffer(buffer, a as u64, &data[a..b]);
    }
}


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
    /// The group table, row per group id (`0..len()`).
    rows: RefCell<Vec<GroupRow>>,
    /// Rows written since the last upload: the GPU's differ from `rows`
    /// exactly here.
    unsent: RefCell<Vec<u32>>,
    root: NodeHandle,
    /// Group id → node.
    nodes: Vec<NodeHandle>,
    /// Group id → the appearance the external writer last wrote.
    ext: Vec<[u32; 5]>,
    /// The last flush's group-row upload and nodes resolved, for the
    /// instruments.
    last_flush: Cell<(UploadCost, u32)>,
    /// Bumped by every flush that changed a row (`FrameData::groups_generation`).
    generation: Cell<u64>,
    /// Moved subtree roots whose rows are not rebuilt yet (`defer_rows`;
    /// each slot once, `pending_mark`). The flush clears them: its resolve
    /// covers every moved node.
    pending: RefCell<Vec<NodeHandle>>,
    pending_mark: RefCell<Vec<bool>>,
}

impl GroupNodes {
    /// One node per row of `groups`, under an identity root. The rows
    /// start SENT: whoever holds the GPU group table writes `groups` whole
    /// when it builds it (rows of 96 B, sized for every row a verb may
    /// allocate), and then each frame's runs (`upload`).
    pub(crate) fn new(groups: &[GroupRow]) -> Self {
        let this = Self::cpu_only(groups);
        this.unsent.borrow_mut().clear();
        this
    }

    fn cpu_only(groups: &[GroupRow]) -> Self {
        let mut tables = NodeTables::default();
        let root = tables.insert(None, Similarity::IDENTITY, Appearance::IDENTITY).expect("a fresh pool accepts a root");
        let mut this = Self {
            tables: RefCell::new(tables),
            rows: RefCell::new(Vec::with_capacity(groups.len())),
            unsent: RefCell::new(Vec::new()),
            root,
            nodes: Vec::with_capacity(groups.len()),
            ext: Vec::with_capacity(groups.len()),
            last_flush: Cell::new((UploadCost::default(), 0)),
            generation: Cell::new(0),
            pending: RefCell::new(Vec::new()),
            pending_mark: RefCell::new(Vec::new()),
        };
        for row in groups {
            this.push_row(row);
        }
        this
    }

    /// A node for the next group id (a load's rows, or one the glyph verbs
    /// allocated). The whole row goes up at the next flush: its clip and
    /// background are the row's own, never the tree's.
    pub(crate) fn push_row(&mut self, row: &GroupRow) {
        let gid = self.nodes.len() as u32;
        self.rows.get_mut().push(*row);
        self.unsent.get_mut().push(gid);
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

    /// Group `gid`'s row as the table holds it.
    pub(crate) fn row(&self, gid: u32) -> Option<GroupRow> {
        self.rows.borrow().get(gid as usize).copied()
    }

    /// The whole table (pick and allocation scan it).
    pub(crate) fn rows(&self) -> std::cell::Ref<'_, Vec<GroupRow>> {
        self.rows.borrow()
    }

    /// The rows for an external writer to flatten into. Every row it writes
    /// must be handed to `adopt` before anything reads it: until then it
    /// holds the writer's columns, not the tree's.
    pub(crate) fn external_rows(&mut self) -> &mut [GroupRow] {
        self.rows.get_mut()
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
    /// the table (`external_rows`): its transform always, its appearance
    /// only if that writer changed it since its last write; then rebuild the
    /// row from the tables. The writer overwrote what the GPU holds, so the
    /// row goes up whatever it rebuilds to.
    pub(crate) fn adopt(&mut self, gid: u32) {
        let Some(&h) = self.nodes.get(gid as usize) else { return };
        let Some(row) = self.rows.get_mut().get(gid as usize).copied() else { return };
        let (local, post, app) = decompose(&row);
        let ext = ext_appearance(&row);
        let t = &mut self.tables.get_mut();
        t.set_local(h, local).expect("live");
        if t.post_scale(h).expect("live").xyz != post {
            t.set_post_scale(h, post).expect("live");
        }
        if self.ext[gid as usize] != ext {
            t.set_appearance(h, app).expect("live");
            self.ext[gid as usize] = ext;
        }
        self.rebuild_row(gid);
        self.unsent.get_mut().push(gid);
    }

    /// Group `gid`'s row with columns 0-3 composed from the tables, as the
    /// flush builds them (column 0's w, and columns 4-5, are the row's own).
    pub(crate) fn composed_row(&self, gid: u32) -> Option<GroupRow> {
        let &h = self.nodes.get(gid as usize)?;
        let mut row = self.row(gid)?;
        let t = self.tables.borrow();
        let (w, alpha) = t.world(h).expect("live");
        write_row(&mut row, &w, alpha, t.post_scale(h).expect("live").xyz, &t.appearance(h).expect("live"));
        Some(row)
    }

    /// Rebuild group `gid`'s row from the tables (`composed_row`), at once.
    pub(crate) fn rebuild_row(&mut self, gid: u32) {
        let Some(new) = self.composed_row(gid) else { return };
        let row = &mut self.rows.get_mut()[gid as usize];
        if bytemuck::bytes_of(row) != bytemuck::bytes_of(&new) {
            *row = new;
            self.unsent.get_mut().push(gid);
        }
    }

    /// The CPU half of the frame's flush, first thing in the frame: resolve
    /// the node edits, rebuild the rows they reach, and plan the runs the
    /// rows changed since the last flush go up in (every row written between
    /// frames included). Returns what resolved, which the faces and regions
    /// rebuild from, and those runs, which the frame's consumer must write
    /// (`upload` for wgpu); bumps the generation when there are any. A
    /// settled scene resolves nothing and plans nothing.
    pub(crate) fn prepare(&self) -> (FramePlan, Vec<Range<u32>>) {
        let t0 = Instant::now();
        let mut t = self.tables.borrow_mut();
        let plan = t.resolve_frame();
        let mut rows = self.rows.borrow_mut();
        let mut changed = std::mem::take(&mut *self.unsent.borrow_mut());
        let (post, app) = (t.post_rows(), t.appearance_rows());
        for slot in plan.slots(&t) {
            let Some(gid) = t.slot_group_row(slot) else { continue };
            let Some(row) = rows.get_mut(gid as usize) else { continue };
            let (w, alpha) = t.resolved_world(slot);
            if write_row(row, w, *alpha, post[slot as usize].xyz, &app[slot as usize]) {
                changed.push(gid);
            }
        }
        // The resolve covered every deferred move.
        let mut mark = self.pending_mark.borrow_mut();
        for h in self.pending.borrow_mut().drain(..) {
            mark[h.index() as usize] = false;
        }
        changed.sort_unstable();
        changed.dedup();
        let runs = upload_runs(&changed, rows.len() as u32);
        if !runs.is_empty() {
            self.generation.set(self.generation.get() + 1);
        }
        let bytes = run_rows(&runs) * std::mem::size_of::<GroupRow>();
        let cost = UploadCost { rows: changed.len(), bytes, ms: t0.elapsed().as_secs_f64() * 1e3 };
        self.last_flush.set((cost, plan.resolved));
        (plan, runs)
    }

    /// The flush generation: bumped whenever a flush planned any run.
    pub(crate) fn generation(&self) -> u64 {
        self.generation.get()
    }

    /// The last flush's group-row upload and nodes resolved — the previous
    /// frame's, when read from `animate`.
    pub(crate) fn last_flush(&self) -> (UploadCost, u32) {
        self.last_flush.get()
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

    /// Note moved nodes whose subtrees' rows a reader before the next flush
    /// will need rebuilt (`sync_rows`) — O(1) a node, nothing per row: a
    /// directory drag does not rewrite a row per file it carries every
    /// frame (2026-10-10). The frame's flush rebuilds them regardless.
    pub(crate) fn defer_rows(&mut self, moved: &[NodeHandle]) {
        let (pending, mark) = (self.pending.get_mut(), self.pending_mark.get_mut());
        for &h in moved {
            let i = h.index() as usize;
            if i >= mark.len() {
                mark.resize(i + 1, false);
            }
            if !mark[i] {
                mark[i] = true;
                pending.push(h);
            }
        }
    }

    /// Rebuild the rows of every deferred move (each moved subtree resolved
    /// once, `rebuild_subtrees`), for a reader before the flush. Returns the
    /// rows rebuilt.
    pub(crate) fn sync_rows(&mut self) -> usize {
        if self.pending.get_mut().is_empty() {
            return 0;
        }
        let pending = std::mem::take(self.pending.get_mut());
        for h in &pending {
            self.pending_mark.get_mut()[h.index() as usize] = false;
        }
        self.rebuild_subtrees(&pending).len()
    }

    /// Group `gid`'s live world offset and per-axis scale, composed from the
    /// tree on demand (O(depth); the bits the row will hold).
    pub(crate) fn world_offset_scale(&self, gid: u32) -> Option<([f32; 3], [f32; 3])> {
        let &h = self.nodes.get(gid as usize)?;
        let t = self.tables.borrow();
        let (w, _) = t.world(h).ok()?;
        let p = t.post_scale(h).ok()?.xyz;
        Some((w.translation, [p[0] * w.scale, p[1] * w.scale, p[2] * w.scale]))
    }

    /// The rows of a move, between frames: every node in the subtrees of
    /// `moved` resolved once (O(1) a node; `NodeTables::resolve_subtrees`)
    /// and each one that routes to a group row rebuilt — columns 0-3,
    /// exactly as `rebuild_row` writes them. Returns the group ids rebuilt,
    /// in depth-first order.
    pub(crate) fn rebuild_subtrees(&mut self, moved: &[NodeHandle]) -> Vec<u32> {
        let t = self.tables.get_mut();
        let (rows, unsent) = (self.rows.get_mut(), self.unsent.get_mut());
        let mut hits: Vec<(u32, Similarity, f32, u32)> = Vec::new();
        t.resolve_subtrees(moved, |slot, row, w, a| {
            if let Some(r) = row {
                hits.push((r, *w, a, slot));
            }
        });
        let mut out = Vec::with_capacity(hits.len());
        for (gid, w, alpha, _) in hits {
            let Some(&h) = self.nodes.get(gid as usize) else { continue };
            let Some(row) = rows.get_mut(gid as usize) else { continue };
            let p = t.post_scale(h).expect("live").xyz;
            let a = t.appearance(h).expect("live");
            if write_row(row, &w, alpha, p, &a) {
                unsent.push(gid);
            }
            out.push(gid);
        }
        out
    }
}

/// Columns 0-3 of a group row from a node's world, inherited alpha, post
/// scale and appearance (column 0's w, and columns 4-5, are left as they
/// are). Returns whether the row's bytes changed.
fn write_row(row: &mut GroupRow, w: &Similarity, alpha: f32, p: [f32; 3], a: &Appearance) -> bool {
    let old = *row;
    row.cols[0] = [w.translation[0], w.translation[1], w.translation[2], row.cols[0][3]];
    row.cols[1] = w.rotation;
    row.cols[2] = [a.tint[0], a.tint[1], a.tint[2], alpha];
    row.cols[3] = [p[0] * w.scale, p[1] * w.scale, p[2] * w.scale, a.blend];
    bytemuck::bytes_of(row) != bytemuck::bytes_of(&old)
}

#[cfg(feature = "wgpu")]
/// Block on a readback of `bytes` of `src` (tests only, never the frame
/// path).
#[cfg(test)]
pub(crate) fn read_buffer(ctx: &crate::gpu::GpuContext, src: &wgpu::Buffer, bytes: u64) -> Vec<u8> {
    let size = bytes.div_ceil(4) * 4;
    let staging = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test readback"),
        size: size.max(4),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("test readback") });
    encoder.copy_buffer_to_buffer(src, 0, &staging, 0, size);
    ctx.queue.submit([encoder.finish()]);
    let slice = staging.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("test readback: map"));
    ctx.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("test readback: poll");
    let out = slice.get_mapped_range().expect("test readback: range")[..bytes as usize].to_vec();
    staging.unmap();
    out
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

    /// The identity-root claim: every row composed back from its node is the
    /// same bits, uniform or per-axis scale alike — and each verb through
    /// the node path lands exactly where the old in-place edit of the row
    /// did, in the table itself. (`flush_uploads_exactly_the_rows_that_changed`
    /// holds the GPU table to these rows.)
    #[test]
    fn rows_through_their_nodes_come_back_bit_for_bit_and_verbs_match_the_old_edits() {
        let rows = rows();
        let mut n = GroupNodes::cpu_only(&rows);
        for (gid, r) in rows.iter().enumerate() {
            let back = n.composed_row(gid as u32).unwrap();
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
            n.rebuild_row(gid as u32);
            let got = n.row(gid as u32).unwrap();
            assert_eq!(bytemuck::bytes_of(&got), bytemuck::bytes_of(want), "row {gid} after the verbs");
        }
        assert!(!n.translate(9, [1.0; 3]), "an unknown group is refused");
    }

    #[cfg(feature = "wgpu")]
    /// What reaches the GPU's group table (2026-10-26, the CPU resolve as
    /// the path): a load's rows resolve to their own bits and upload
    /// nothing; a verb uploads exactly the rows it changed (as one run when
    /// they sit close); an allocated row goes up whole, clip and background
    /// included; and the GPU table read back is the host's, bit for bit. (Forty
    /// rows, so a few changed ones stay under the whole-table share.)
    #[test]
    fn flush_uploads_exactly_the_rows_that_changed() {
        let ctx = pollster::block_on(crate::gpu::init(None));
        let rows: Vec<GroupRow> = (0..40)
            .map(|k| {
                let mut r = rows()[k % 3];
                r.cols[0][0] += k as f32 * 7.25;
                r
            })
            .collect();
        let buf = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("test group table"),
            size: 48 * 96,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        ctx.queue.write_buffer(&buf, 0, bytemuck::cast_slice(&rows));
        let mut n = GroupNodes::new(&rows);
        let flush = |n: &GroupNodes| {
            let (plan, runs) = n.prepare();
            write_runs(&ctx.queue, &buf, bytemuck::cast_slice(&n.rows()), std::mem::size_of::<GroupRow>(), &runs);
            (plan.resolved, n.last_flush().0)
        };
        let (resolved, up) = flush(&n);
        assert_eq!((resolved, up.rows, up.bytes), (41, 0, 0), "the root and every row resolve to the bits they had");
        n.translate(10, [1.5, -2.0, 0.25]);
        n.set_tint(12, [0.25, 0.5, 0.75]);
        let (_, up) = flush(&n);
        assert_eq!((up.rows, up.bytes), (2, 3 * 96), "rows 10 and 12, one run across row 11");
        // A verb rebuilds its row at once (the readers see it before the
        // flush); the flush still sends it, once.
        n.translate(20, [0.0, 3.0, 0.0]);
        n.rebuild_row(20);
        assert_eq!(n.row(20).unwrap().cols[0][1], rows[20].cols[0][1] + 3.0, "the table holds the move before the flush");
        let (_, up) = flush(&n);
        assert_eq!((up.rows, up.bytes), (1, 96), "a row rebuilt between frames goes up at the flush");
        let mut extra = GroupRow::identity([4.0, 5.0, 6.0]);
        extra.cols[5] = [0.9, 0.1, 0.2, 1.0];
        n.push_row(&extra);
        let (_, up) = flush(&n);
        assert_eq!((up.rows, up.bytes), (1, 96), "an allocated row goes up whole");
        assert_eq!(flush(&n).1.bytes, 0, "a settled frame uploads nothing");
        let back: Vec<GroupRow> = bytemuck::cast_slice(&read_buffer(&ctx, &buf, 41 * 96)).to_vec();
        for (gid, got) in back.iter().enumerate() {
            let want = n.row(gid as u32).unwrap();
            assert_eq!(bytemuck::bytes_of(got), bytemuck::bytes_of(&want), "row {gid} on the GPU");
            assert_eq!(bytemuck::bytes_of(&want), bytemuck::bytes_of(&n.composed_row(gid as u32).unwrap()), "row {gid} is the tree's");
        }
        assert_eq!(back[40].cols[5], extra.cols[5], "the background is the row's own");
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
        // The layout controller's sync, into the table: a new offset, its
        // own binding tint.
        let mut synced = rows[0];
        synced.cols[0] = [40.0, 2.0, -3.0, 0.0];
        n.external_rows()[0] = synced;
        n.adopt(0);
        assert_eq!(n.row(0).unwrap().cols[0], [40.0, 2.0, -3.0, 0.0], "the move is taken");
        assert_eq!(n.row(0).unwrap().cols[2], [1.0, 0.0, 0.0, 0.0], "the verb's tint and hide survive");
        // The writer hides the group itself (its sync writes alpha 0, scale 0).
        let mut hidden = synced;
        hidden.cols[2] = [0.0; 4];
        hidden.cols[3] = [0.0; 4];
        n.external_rows()[0] = hidden;
        n.adopt(0);
        assert_eq!(n.row(0).unwrap().cols[2], [0.0; 4], "a change the writer made lands");
        assert_eq!(n.len(), 3);
    }

    /// The deferred rows of a tree move (2026-10-10, the library on the
    /// tree): a moved parent's subtree is noted, not rebuilt — the rows stay
    /// as they were until a reader syncs (or the flush) — and the sync writes
    /// exactly the rows an eager rebuild (`composed_row`, per group) gives,
    /// each once.
    #[test]
    fn deferred_rows_sync_to_the_eager_rows() {
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
        n.defer_rows(&[parent]);
        n.defer_rows(&[parent, g0]);
        assert_eq!(bytemuck::cast_slice::<_, u8>(&n.rows()), bytemuck::cast_slice::<_, u8>(&rows), "deferred: nothing written yet");
        assert_eq!(n.sync_rows(), 2, "each moved group's row once");
        let groups = n.rows().clone();
        for gid in 0..3u32 {
            let want = n.composed_row(gid).unwrap();
            assert_eq!(bytemuck::bytes_of(&groups[gid as usize]), bytemuck::bytes_of(&want), "row {gid}");
        }
        assert_ne!(groups[0].cols[0], rows[0].cols[0], "the nested row moved");
        assert_eq!(n.sync_rows(), 0, "nothing waits after a sync");
        let off = n.world_offset_scale(2).unwrap();
        assert_eq!(off.0, [groups[2].cols[0][0], groups[2].cols[0][1], groups[2].cols[0][2]]);
        assert_eq!(off.1, [groups[2].cols[3][0], groups[2].cols[3][1], groups[2].cols[3][2]]);
    }
}
