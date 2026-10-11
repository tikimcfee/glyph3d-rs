//! The library's CPU cull, top-down over the transform tree (2026-10-10;
//! the design note's bounds table, `out/DESIGN-VIEWS-AND-TRANSFORMS-
//! 2026-10-10.md` § Groups).
//!
//! The flat cull (`cull::cull_segments`) tests every segment's stored WORLD
//! box each frame, so every move had to rewrite the world boxes of every
//! file it carried (`sync_segment` per moved file: O(descendants) per frame
//! of a directory drag, a page turn or a form change). Here a move costs
//! nothing per file:
//!
//! - **Bounds table.** Every library node keeps its subtree's segment box in
//!   its OWN frame (a file: its segment's local box; any other node: the
//!   union of its children's boxes carried through their local rows). A
//!   move marks the moved node; before the next cull the boxes along its
//!   ancestors are refit (O(children) per ancestor, not O(descendants)).
//! - **Top-down cull.** The walk composes world transforms down the
//!   depth-first order (root first, the resolve's order, so the bits are
//!   the GPU's), skips a subtree whose box is outside the frustum, and
//!   collects the candidate segments under it — then runs exactly the flat
//!   cull's per-segment test (`CullAcc::segment`) on them in segment order.
//!   The subtree test is only a conservative prefilter (its boxes are
//!   padded), so the output is the flat cull's, draw for draw: a segment it
//!   drops would have failed the exact test.
//!
//! **Byte-equal with the synced boxes.** A file that never moved is tested
//! with its stored (staged) world box, as before; a file under a node that
//! moved since load — or whose segment a verb synced — gets the box
//! `sync_segment` would have written (local × scale + offset, term for
//! term, from the same world the mirror holds), its em scale and blocks the
//! same way, and the backdrop tint `sync_segment` computes at its colour.
//! The library tests hold this cull to the flat one over synced boxes
//! (`tree_cull::tests`).

use glyph_scene_graph::{NodeHandle, NodeTables, Similarity, NONE};

use super::cull::{BlockCull, Blocks, CullAcc, CullState, CullView, PhaseDraws};

type Aabb = [[f32; 3]; 2];
const EMPTY: Aabb = [[f32::INFINITY; 3], [f32::NEG_INFINITY; 3]];

fn is_empty(b: &Aabb) -> bool {
    !(b[0][0] <= b[1][0] && b[0][1] <= b[1][1] && b[0][2] <= b[1][2])
}

fn union(a: &mut Aabb, b: &Aabb) {
    for k in 0..3 {
        a[0][k] = a[0][k].min(b[0][k]);
        a[1][k] = a[1][k].max(b[1][k]);
    }
}

/// The absolute rotation matrix's rows times `e`: the extent of a box of
/// half-size `e` after rotation by `q` (glam's `from_quat` entries).
fn rotated_extent(q: [f32; 4], e: [f32; 3]) -> [f32; 3] {
    let (x2, y2, z2) = (q[0] + q[0], q[1] + q[1], q[2] + q[2]);
    let (xx, xy, xz) = (q[0] * x2, q[0] * y2, q[0] * z2);
    let (yy, yz, zz) = (q[1] * y2, q[1] * z2, q[2] * z2);
    let (wx, wy, wz) = (q[3] * x2, q[3] * y2, q[3] * z2);
    let m = [
        [1.0 - (yy + zz), xy - wz, xz + wy],
        [xy + wz, 1.0 - (xx + zz), yz - wx],
        [xz - wy, yz + wx, 1.0 - (xx + yy)],
    ];
    [0, 1, 2].map(|r| m[r][0].abs() * e[0] + m[r][1].abs() * e[1] + m[r][2].abs() * e[2])
}

/// `b` (in a child's frame) carried through the similarity `s` into the
/// parent's frame, as a box (centre and extent).
fn map_box(s: &Similarity, b: &Aabb) -> Aabb {
    if s.rotation == [0.0, 0.0, 0.0, 1.0] {
        // No rotation (every library node): scale about the origin, then
        // translate. Prefilter boxes only, padded where they are tested.
        let (lo, hi) = if s.scale >= 0.0 { (b[0], b[1]) } else { (b[1], b[0]) };
        let t = s.translation;
        return [
            [lo[0] * s.scale + t[0], lo[1] * s.scale + t[1], lo[2] * s.scale + t[2]],
            [hi[0] * s.scale + t[0], hi[1] * s.scale + t[1], hi[2] * s.scale + t[2]],
        ];
    }
    let c = [0, 1, 2].map(|k| (b[0][k] + b[1][k]) * 0.5 * s.scale);
    let e = [0, 1, 2].map(|k| (b[1][k] - b[0][k]) * 0.5 * s.scale.abs());
    let rc = glyph_scene_graph::transform::rotate(s.rotation, c);
    let re = rotated_extent(s.rotation, e);
    let wc = [0, 1, 2].map(|k| s.translation[k] + rc[k]);
    [[wc[0] - re[0], wc[1] - re[1], wc[2] - re[2]], [wc[0] + re[0], wc[1] + re[1], wc[2] + re[2]]]
}

/// Grow a box by a margin that dwarfs every rounding a subtree box and a
/// leaf box can differ by (each is a few ulps of its coordinates).
fn padded(b: &Aabb) -> Aabb {
    let mag = (0..3).fold(0.0f32, |m, k| m.max(b[0][k].abs()).max(b[1][k].abs()));
    let pad = mag * 1e-4 + 1e-3;
    [[b[0][0] - pad, b[0][1] - pad, b[0][2] - pad], [b[1][0] + pad, b[1][1] + pad, b[1][2] + pad]]
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Outside,
    Straddles,
    Inside,
}

fn classify(b: &Aabb, planes: &[[f32; 4]; 6]) -> Side {
    let mut inside = true;
    for pl in planes {
        let p = [0, 1, 2].map(|k| if pl[k] >= 0.0 { b[1][k] } else { b[0][k] });
        if pl[0] * p[0] + pl[1] * p[1] + pl[2] * p[2] + pl[3] < 0.0 {
            return Side::Outside;
        }
        let n = [0, 1, 2].map(|k| if pl[k] >= 0.0 { b[0][k] } else { b[1][k] });
        if pl[0] * n[0] + pl[1] * n[1] + pl[2] * n[2] + pl[3] < 0.0 {
            inside = false;
        }
    }
    if inside {
        Side::Inside
    } else {
        Side::Straddles
    }
}

/// `sync_segment`'s backdrop tint for a group colour `rgb`: the staged tint
/// scaled by the colour's change since staging (linear light). One copy of
/// the arithmetic, so a computed tint has a synced one's bits.
pub(super) fn backdrop_tint(base: [f32; 4], orig: [f32; 3], rgb: [f32; 4]) -> [f32; 4] {
    let mut t = base;
    for (c, tc) in t.iter_mut().enumerate().take(3) {
        let newc = rgb[c].max(0.0).powf(2.2);
        let oldc = orig[c].max(1e-6).powf(2.2);
        *tc = (base[c] * newc / oldc).min(1.0);
    }
    t
}

/// A computed segment's world offset and per-axis scale (its box is
/// local × scale + offset); None: the stored world box.
type Geom = Option<([f32; 3], [f32; 3])>;

/// Per library node, everything the walk reads, kept in DEPTH-FIRST order
/// (index q = the node's position − the library root's), so the walk reads
/// one array front to back, skipping forward over culled subtrees. The
/// library's topology never changes after load, so q is fixed. Measured
/// (2026-10-10, a 20,002-file synthetic library, near pose): 1,925 nodes
/// visited in ~0.11 ms whether the worlds come from the cache or are
/// composed — the cost is the visit, not the arithmetic.
#[derive(Clone, Copy)]
struct Node {
    /// Pool slot (the node tables' index).
    slot: u32,
    /// Subtree size (this node and its descendants).
    size: u32,
    /// Segment index (NONE: no segment).
    seg: u32,
    /// The subtree's segments: `dfs_segs[seg_lo..seg_hi]`.
    seg_lo: u32,
    seg_hi: u32,
    /// Its own local row changed since load (a move, a drag, a verb).
    moved: bool,
    /// It or a descendant moved since load.
    subtree_moved: bool,
    /// Refit pending (dedupe within one refit).
    mark: bool,
    /// The move that last wrote its local row; the refit that last wrote
    /// its box; the epoch its cached world was computed at.
    moved_epoch: u32,
    bounds_epoch: u32,
    cache_epoch: u32,
    /// Its subtree's segment box in its own frame.
    bounds: Aabb,
    /// The cached world and padded world box (valid while nothing on its
    /// chain moved and its box was not refit since `cache_epoch`).
    cache_w: Similarity,
    cache_wb: Aabb,
}

pub(super) struct TreeCull {
    root: NodeHandle,
    nodes: Vec<Node>,
    /// Pool slot → q (NONE: not a library node).
    q_of_slot: Vec<u32>,
    /// Segment → q (NONE: not a library node).
    q_of_seg: Vec<u32>,
    /// The library's segments in depth-first order.
    dfs_segs: Vec<u32>,
    /// Segment → `sync_segment` ran for it (its stored tint is current).
    synced: Vec<bool>,
    /// Segment → `sync_segment`'s tint at the colour it was staged with.
    moved_tint: Vec<[f32; 4]>,
    /// q of the nodes moved since the last refit, and whether the node's
    /// own box changed too (a verb's edit) or only its place in its parent.
    pending: Vec<(u32, bool)>,
    /// Segments not under the root: always candidates, stored boxes.
    loose: Vec<u32>,
    /// This frame's candidates: their segment indices, and per segment the
    /// frame they were last collected in and their geometry (ascending
    /// order comes from a scan of the stamps when many, a sort when few).
    cands: Vec<u32>,
    stamp: Vec<u32>,
    geom: Vec<Geom>,
    frame: u32,
    /// The move counter behind the world cache.
    epoch: u32,
    stack: Vec<(u32, Similarity, bool, bool, u32)>,
    /// The last cull's walk: (nodes visited, candidates), and its CPU time
    /// (`LIBTIME`).
    pub(super) last_walk: (usize, usize),
    pub(super) last_ms: f64,
}

impl TreeCull {
    /// The tree cull for the library rooted at `root`, segment i being
    /// `seg_nodes[i]`'s (a repo load: file i, group i).
    pub(super) fn new(t: &mut NodeTables, root: NodeHandle, seg_nodes: &[NodeHandle], cull: &CullState, group_rgb: &[[f32; 4]]) -> Self {
        t.ensure_order();
        let n_seg = cull.segments.len();
        let (r0, rn) = t.subtree_range(root).expect("the library root is live");
        let (order, pos, size) = (t.order_rows(), t.positions(), t.subtree_sizes());
        let mut q_of_slot = vec![NONE; t.slot_count() as usize];
        let blank = Node {
            slot: NONE,
            size: 1,
            seg: NONE,
            seg_lo: 0,
            seg_hi: 0,
            moved: false,
            subtree_moved: false,
            mark: false,
            moved_epoch: 0,
            bounds_epoch: 0,
            cache_epoch: 0,
            bounds: EMPTY,
            cache_w: Similarity::IDENTITY,
            cache_wb: EMPTY,
        };
        let mut nodes = vec![blank; rn as usize];
        for q in 0..rn {
            let s = order[(r0 + q) as usize];
            q_of_slot[s as usize] = q;
            nodes[q as usize].slot = s;
            nodes[q as usize].size = size[s as usize];
        }
        let mut q_of_seg = vec![NONE; n_seg];
        let mut loose = Vec::new();
        for (i, qs) in q_of_seg.iter_mut().enumerate() {
            match seg_nodes.get(i).filter(|h| t.is_live(**h)).map(|h| q_of_slot[h.index() as usize]) {
                Some(q) if q != NONE => {
                    *qs = q;
                    nodes[q as usize].seg = i as u32;
                }
                _ => loose.push(i as u32),
            }
        }
        // The segments in depth-first order, and each subtree's range of them.
        let mut dfs_segs = Vec::new();
        let mut first = vec![0u32; rn as usize + 1];
        for q in 0..rn as usize {
            first[q] = dfs_segs.len() as u32;
            if nodes[q].seg != NONE {
                dfs_segs.push(nodes[q].seg);
            }
        }
        first[rn as usize] = dfs_segs.len() as u32;
        for q in 0..rn as usize {
            let end = q + nodes[q].size as usize;
            nodes[q].seg_lo = first[q];
            nodes[q].seg_hi = first[end];
        }
        debug_assert!(nodes.iter().enumerate().all(|(q, n)| pos[n.slot as usize] == r0 + q as u32));
        let moved_tint = (0..n_seg)
            .map(|i| backdrop_tint(cull.base_tint[i], cull.orig_group_rgb[i], group_rgb.get(i).copied().unwrap_or([1.0; 4])))
            .collect();
        let mut this = Self {
            root,
            nodes,
            q_of_slot,
            q_of_seg,
            dfs_segs,
            synced: vec![false; n_seg],
            moved_tint,
            pending: Vec::new(),
            loose,
            cands: Vec::new(),
            stamp: vec![0; n_seg],
            geom: vec![None; n_seg],
            frame: 0,
            epoch: 1,
            stack: Vec::new(),
            last_walk: (0, 0),
            last_ms: 0.0,
        };
        // Every box, children before parents (reverse pre-order).
        for q in (0..rn).rev() {
            this.nodes[q as usize].bounds = this.fit(t, q, cull);
        }
        this
    }

    /// Node q's box from its own segment and its children's boxes.
    fn fit(&self, t: &NodeTables, q: u32, cull: &CullState) -> Aabb {
        let node = &self.nodes[q as usize];
        let mut b = EMPTY;
        if node.seg != NONE {
            let i = node.seg as usize;
            let post = t.post_rows()[node.slot as usize].xyz;
            let (lo, hi) = (cull.local_min[i], cull.local_max[i]);
            let a = [0, 1, 2].map(|k| lo[k] * post[k]);
            let z = [0, 1, 2].map(|k| hi[k] * post[k]);
            b = [[a[0].min(z[0]), a[1].min(z[1]), a[2].min(z[2])], [a[0].max(z[0]), a[1].max(z[1]), a[2].max(z[2])]];
        }
        let local = t.local_rows();
        for c in t.child_slots(node.slot) {
            let cq = self.q_of_slot.get(c as usize).copied().unwrap_or(NONE);
            if cq == NONE {
                continue;
            }
            let cb = self.nodes[cq as usize].bounds;
            if !is_empty(&cb) {
                union(&mut b, &map_box(&local[c as usize], &cb));
            }
        }
        b
    }

    fn mark_moved(&mut self, q: u32, self_too: bool) {
        self.epoch += 1;
        let n = &mut self.nodes[q as usize];
        n.moved = true;
        n.moved_epoch = self.epoch;
        self.pending.push((q, self_too));
    }

    /// A node's local row was written (a library move, a drag): its world,
    /// and every world below it, now differ from what the stored boxes say.
    pub(super) fn note_moved(&mut self, slot: u32) {
        if let Some(&q) = self.q_of_slot.get(slot as usize) {
            if q != NONE {
                self.mark_moved(q, false);
            }
        }
    }

    /// `sync_segment` ran for segment `i` (a verb edited its group): its
    /// stored tint is current, and its box is now computed, as a synced
    /// box was. A verb may have scaled the group (its post scale is in its
    /// own box): the node itself is refit as well.
    pub(super) fn segment_synced(&mut self, i: usize) {
        if let Some(s) = self.synced.get_mut(i) {
            *s = true;
            let q = self.q_of_seg[i];
            if q != NONE {
                self.mark_moved(q, true);
            }
        }
    }

    /// Refit the boxes the moves since the last cull invalidated: each moved
    /// node's ancestors up to the root (a node's own box is in its own
    /// frame, which its move does not change — unless a verb changed its
    /// content), children before parents.
    fn refit(&mut self, t: &NodeTables, cull: &CullState) {
        if self.pending.is_empty() {
            return;
        }
        let mut dirty: Vec<u32> = Vec::new();
        for k in 0..self.pending.len() {
            let (moved, self_too) = self.pending[k];
            self.nodes[moved as usize].subtree_moved = true;
            let mut q = if self_too {
                moved
            } else {
                let p = t.parent_slot(self.nodes[moved as usize].slot);
                if p == NONE {
                    NONE
                } else {
                    self.q_of_slot.get(p as usize).copied().unwrap_or(NONE)
                }
            };
            while q != NONE && !self.nodes[q as usize].mark {
                let n = &mut self.nodes[q as usize];
                n.mark = true;
                n.subtree_moved = true;
                dirty.push(q);
                let p = t.parent_slot(n.slot);
                q = if p == NONE { NONE } else { self.q_of_slot.get(p as usize).copied().unwrap_or(NONE) };
            }
        }
        self.pending.clear();
        // q is the depth-first position: descending refits children first.
        dirty.sort_unstable_by(|a, b| b.cmp(a));
        self.epoch += 1;
        for &q in &dirty {
            let b = self.fit(t, q, cull);
            let n = &mut self.nodes[q as usize];
            n.bounds = b;
            n.bounds_epoch = self.epoch;
            n.mark = false;
        }
    }

    /// This frame's cull: the flat cull's draws, from a walk that visits
    /// only the subtrees the frustum reaches.
    pub(super) fn cull(&mut self, t: &NodeTables, cull: &CullState, view: &CullView, chunk_cap: u32, chunk_count: u32) -> PhaseDraws {
        let t0 = std::time::Instant::now();
        self.refit(t, cull);
        self.cands.clear();
        self.frame = self.frame.wrapping_add(1).max(1);
        let frame = self.frame;
        for &i in &self.loose {
            self.stamp[i as usize] = frame;
            self.geom[i as usize] = None;
            self.cands.push(i);
        }
        let (local, post) = (t.local_rows(), t.post_rows());
        let base = t.parent_world(self.root).expect("the library root is live");
        let end = self.nodes.len() as u32;
        let mut q = 0u32;
        let mut visited = 0usize;
        self.stack.clear();
        while q < end {
            visited += 1;
            while self.stack.last().is_some_and(|s| s.0 <= q) {
                self.stack.pop();
            }
            let (pw, pm, pin, pe) = self.stack.last().map_or((base, false, false, 0), |s| (s.1, s.2, s.3, s.4));
            let node = &mut self.nodes[q as usize];
            if is_empty(&node.bounds) {
                q += node.size;
                continue;
            }
            // The newest move on the chain down to here; a cache older than
            // it (or than the node's box) is recomputed — the same bits.
            let chain = pe.max(node.moved_epoch);
            let ce = node.cache_epoch;
            let (w, wb) = if ce > 0 && ce >= chain && ce >= node.bounds_epoch {
                (node.cache_w, node.cache_wb)
            } else {
                let w = Similarity::compose(&pw, &local[node.slot as usize]);
                let wb = padded(&map_box(&w, &node.bounds));
                node.cache_epoch = self.epoch;
                node.cache_w = w;
                node.cache_wb = wb;
                (w, wb)
            };
            let m = pm || node.moved;
            let side = if pin { Side::Inside } else { classify(&wb, &view.planes) };
            let (size, seg, slot, subtree_moved, lo, hi) = (node.size, node.seg, node.slot, node.subtree_moved, node.seg_lo, node.seg_hi);
            if side == Side::Outside {
                q += size;
                continue;
            }
            if side == Side::Inside && !m && !subtree_moved {
                // Nothing under here ever moved: every segment keeps its
                // stored box, and none needs a composed world.
                for &si in &self.dfs_segs[lo as usize..hi as usize] {
                    self.stamp[si as usize] = frame;
                    self.geom[si as usize] = None;
                }
                self.cands.extend_from_slice(&self.dfs_segs[lo as usize..hi as usize]);
                q += size;
                continue;
            }
            if seg != NONE {
                let geom = m.then(|| {
                    let ps = post[slot as usize].xyz;
                    (w.translation, [0, 1, 2].map(|k| (ps[k] * w.scale).max(0.0)))
                });
                self.stamp[seg as usize] = frame;
                self.geom[seg as usize] = geom;
                self.cands.push(seg);
            }
            self.stack.push((q + size, w, m, side == Side::Inside, chain));
            q += 1;
        }
        // Segment order (the blend order): a sort when few, a scan of the
        // stamps when many (a scan of a few bytes per segment).
        let n_seg = self.stamp.len();
        if self.cands.len() * 8 > n_seg {
            self.cands.clear();
            self.cands.extend((0..n_seg as u32).filter(|&i| self.stamp[i as usize] == frame));
        } else {
            self.cands.sort_unstable();
        }
        self.last_walk = (visited, self.cands.len());
        let mut acc = CullAcc::new(chunk_cap, chunk_count);
        for &si in &self.cands {
            let i = si as usize;
            let geom = self.geom[i];
            if cull.hidden.get(i).copied().unwrap_or(false) {
                continue;
            }
            let seg = &cull.segments[i];
            match geom {
                None => acc.segment(view, seg.min, seg.max, seg.tint, seg.slot_base, seg.slot_count, Blocks::World(&seg.blocks), cull.em_scale.get(i).copied().unwrap_or(1.0)),
                Some((o, s)) => {
                    let (lo, hi) = (cull.local_min[i], cull.local_max[i]);
                    let min = [lo[0] * s[0] + o[0], lo[1] * s[1] + o[1], lo[2] * s[2] + o[2]];
                    let max = [hi[0] * s[0] + o[0], hi[1] * s[1] + o[1], hi[2] * s[2] + o[2]];
                    let tint = if self.synced[i] { seg.tint } else { self.moved_tint[i] };
                    let blocks: &[BlockCull] = cull.local_blocks.get(i).map_or(&[], |b| b.as_slice());
                    acc.segment(view, min, max, tint, seg.slot_base, seg.slot_count, Blocks::Local { blocks, offset: o, scale: s }, s[1]);
                }
            }
        }
        let out = acc.finish();
        self.last_ms = t0.elapsed().as_secs_f64() * 1e3;
        out
    }
}

#[cfg(test)]
mod tests {
    use glam::{Mat4, Vec3};
    use glyph_scene_graph::{Appearance, NodeHandle, NodeTables, Similarity};

    use super::{backdrop_tint, TreeCull};
    use crate::glyph_scene::cull::{cull_segments, frustum_planes, BlockCull, CullState, CullView, PhaseDraws, SegCull};
    use crate::library::plan::LibFile;
    use crate::library::{Form, FormCmd, Library, Page};

    /// A small deterministic generator (no rand dependency).
    struct Lcg(u64);
    impl Lcg {
        fn unit(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 40) as f32 / (1u64 << 24) as f32
        }
        fn range(&mut self, lo: f32, hi: f32) -> f32 {
            lo + (hi - lo) * self.unit()
        }
    }

    fn same(a: &PhaseDraws, b: &PhaseDraws) -> bool {
        a.glyph_ranges == b.glyph_ranges && bytemuck::cast_slice::<_, u8>(&a.backdrops) == bytemuck::cast_slice::<_, u8>(&b.backdrops)
    }

    /// The library's tree cull against the flat cull it replaces (2026-10-10):
    /// over a 120-file library, the flat cull runs on segment boxes kept the
    /// way the bevy-era code kept them — every file under a moved node
    /// re-synced (`sync_segment`'s arithmetic) each frame it moved — and the
    /// tree cull gets only the moved NODES. Seated, mid form change, after a
    /// directory dragged far off and a file dragged, mid page turn: over 400
    /// random cameras (near and far, so both LOD tiers and every kind of
    /// partial cull) the two must produce the same draws and backdrops, bit
    /// for bit — the tree's subtree test is a prefilter, never a decision.
    #[test]
    fn the_tree_cull_draws_what_the_flat_cull_draws_over_synced_boxes() {
        let ctx = pollster::block_on(crate::gpu::init(None));
        let s = &crate::config::settings().library;
        let mut r = Lcg(0x7EE_C011);
        let dirs = ["", "src/", "src/core/", "src/core/deep/", "docs/", "tools/gen/", "tools/", "a/b/c/"];
        let files: Vec<LibFile> = (0..120)
            .map(|i| {
                let (w, h, d) = (r.range(2.0, 400.0), r.range(1.0, 900.0), r.range(0.0, 60.0));
                LibFile { rel_path: format!("{}f{i:03}.rs", dirs[i % dirs.len()]), ink_min: [0.0, -h, -d], ink_max: [w, 0.0, 0.0] }
            })
            .collect();
        let mut lib = Library::new(files.clone(), s);
        let mut t = NodeTables::default();
        let root = t.insert(None, Similarity::IDENTITY, Appearance::IDENTITY).unwrap();
        // The load rows from a scratch tree, as apply_library makes them.
        let mut scratch = NodeTables::default();
        let sn = lib.insert_nodes(&mut scratch, None, None);
        let tints: Vec<[f32; 3]> = (0..files.len()).map(|i| [0.2 + 0.005 * i as f32, 0.6, 0.9]).collect();
        let groups = crate::library::seated_rows(&scratch, &sn, &tints);
        let file_nodes: Vec<NodeHandle> = (0..files.len())
            .map(|i| {
                let h = t.insert(Some(root), Similarity::IDENTITY, Appearance::IDENTITY).unwrap();
                t.set_group_row(h, Some(i as u32)).unwrap();
                h
            })
            .collect();
        lib.attach(&mut t, Some(root), &file_nodes);
        // Segments staged as into_staged stages them: offset + local x scale,
        // with sub-file blocks over each file's rows.
        let mut slot = 0u32;
        let segments: Vec<SegCull> = files
            .iter()
            .zip(&groups)
            .enumerate()
            .map(|(i, (f, g))| {
                let (o, sc) = ([g.cols[0][0], g.cols[0][1], g.cols[0][2]], g.cols[3][0]);
                let w = |p: [f32; 3]| [o[0] + p[0] * sc, o[1] + p[1] * sc, o[2] + p[2] * sc];
                let count = if i % 9 == 4 { 0 } else { 600 + (i as u32 % 5) * 400 };
                let nb = count / 512;
                let blocks = (0..nb)
                    .map(|b| {
                        let (y0, y1) = (f.ink_min[1] * b as f32 / nb as f32, f.ink_min[1] * (b + 1) as f32 / nb as f32);
                        BlockCull {
                            min: w([f.ink_min[0] - 0.5, y1 - 0.5, f.ink_min[2] - 0.2]),
                            max: w([f.ink_max[0] + 0.8, y0 + 0.8, f.ink_max[2] + 0.2]),
                            slot_base: slot + b * 512,
                            slot_count: 512.min(count - b * 512),
                        }
                    })
                    .collect();
                let seg = SegCull {
                    min: w([f.ink_min[0] - 0.3, f.ink_min[1] - 0.5, f.ink_min[2]]),
                    max: w([f.ink_max[0] + 0.6, f.ink_max[1] + 0.75, f.ink_max[2]]),
                    slot_base: slot,
                    slot_count: count,
                    tint: [0.3 + 0.004 * i as f32, 0.4, 0.5, 0.6],
                    blocks,
                };
                slot += count;
                seg
            })
            .collect();
        let camera_buf = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("test camera"),
            size: 256,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut cull = CullState::new(&ctx, crate::glyph_scene::target::POOL_FORMAT, wgpu::TextureFormat::Depth32Float, &camera_buf, &segments, &groups);
        cull.backdrop_at_front = true;
        let rgb: Vec<[f32; 4]> = groups.iter().map(|g| g.cols[2]).collect();
        let mut tree = TreeCull::new(&mut t, lib.nodes.as_ref().unwrap().root, &file_nodes, &cull, &rgb);
        // The flat reference: the bevy-era segment table, synced per moved file.
        let mut flat = cull.segments.clone();
        let mut flat_em = cull.em_scale.clone();
        let sync = |t: &mut NodeTables, moved: &[NodeHandle], flat: &mut Vec<SegCull>, flat_em: &mut Vec<f32>, cull: &CullState| {
            let mut hit = Vec::new();
            t.resolve_subtrees(moved, |_, row, w, _| {
                if let Some(g) = row {
                    hit.push((g as usize, *w));
                }
            });
            for (i, w) in hit {
                let o = w.translation;
                let s = [w.scale.max(0.0); 3];
                let seg = &mut flat[i];
                seg.min = [0, 1, 2].map(|k| cull.local_min[i][k] * s[k] + o[k]);
                seg.max = [0, 1, 2].map(|k| cull.local_max[i][k] * s[k] + o[k]);
                for (b, lb) in seg.blocks.iter_mut().zip(&cull.local_blocks[i]) {
                    b.min = [0, 1, 2].map(|k| lb.min[k] * s[k] + o[k]);
                    b.max = [0, 1, 2].map(|k| lb.max[k] * s[k] + o[k]);
                }
                flat_em[i] = s[1];
                seg.tint = backdrop_tint(cull.base_tint[i], cull.orig_group_rgb[i], groups[i].cols[2]);
            }
        };
        let mut checked = 0;
        let mut culled_some = 0;
        let mut check = |t: &mut NodeTables, tree: &mut TreeCull, flat: &[SegCull], flat_em: &[f32], r: &mut Lcg, what: &str| {
            t.ensure_order();
            for k in 0..100 {
                let far = k % 3 == 0;
                let dist = if far { r.range(2000.0, 12000.0) } else { r.range(20.0, 900.0) };
                let target = Vec3::new(r.range(-300.0, 300.0), r.range(-900.0, 50.0), r.range(-200.0, 20.0));
                let dir = Vec3::new(r.range(-0.6, 0.6), r.range(-0.5, 0.5), 1.0).normalize();
                let eye = target + dir * dist;
                let view_m = glam::camera::rh::view::look_at_mat4(eye, target, Vec3::Y);
                let fov = 45f32.to_radians();
                let proj = glam::camera::rh::proj::directx::perspective(fov, 1.6, 20_000.0, 0.05);
                let vp: Mat4 = proj * view_m;
                let view = CullView {
                    planes: frustum_planes(&vp),
                    eye,
                    px_scale: 1000.0 / (2.0 * (fov * 0.5).tan()),
                    lod_min_px: if k % 4 == 0 { 0.2 } else { 6.0 },
                    file_backgrounds: k % 5 == 0,
                    file_bg_color: [0.1, 0.1, 0.13, 0.85],
                    backdrop_at_front: true,
                };
                let want = cull_segments(flat, &cull.hidden, flat_em, &view, 1 << 20, 4);
                let got = tree.cull(t, &cull, &view, 1 << 20, 4);
                assert!(same(&want, &got), "{what}, camera {k}: flat {} ranges / {} backdrops, tree {} / {}", want.glyph_ranges.len(), want.backdrops.len(), got.glyph_ranges.len(), got.backdrops.len());
                checked += 1;
                culled_some += usize::from(tree.last_walk.1 < flat.len());
            }
        };
        check(&mut t, &mut tree, &flat, &flat_em, &mut r, "seated");
        // A whole-library form change, sampled mid-ease.
        lib.form(&t, None, FormCmd::Set(Form::Splay));
        for _ in 0..6 {
            let tick = lib.tick(&mut t, 1.0 / 60.0).unwrap();
            for h in &tick.moved {
                tree.note_moved(h.index());
            }
            sync(&mut t, &tick.moved, &mut flat, &mut flat_em, &cull);
        }
        check(&mut t, &mut tree, &flat, &flat_em, &mut r, "mid splay");
        // A directory dragged far off (its old bounds no longer cover it),
        // and a file dragged under its scaled mount.
        let zone = lib.zone_node("dir:src/core").unwrap();
        let l = t.local(zone).unwrap();
        t.set_local(zone, Similarity { translation: [l.translation[0] + 700.0, l.translation[1] - 250.0, l.translation[2] + 15.0], ..l }).unwrap();
        tree.note_moved(zone.index());
        sync(&mut t, &[zone], &mut flat, &mut flat_em, &cull);
        let f = lib.nodes.as_ref().unwrap().file[7];
        let d = t.parent_world(f).unwrap().world_delta_to_local([-120.0, 33.0, 4.0]);
        t.set_local(f, Similarity::from_translation(d)).unwrap();
        tree.note_moved(f.index());
        sync(&mut t, &[f], &mut flat, &mut flat_em, &cull);
        check(&mut t, &mut tree, &flat, &flat_em, &mut r, "after the drags");
        lib.page(&t, None, Page::Next);
        for _ in 0..4 {
            let tick = lib.tick(&mut t, 1.0 / 60.0).unwrap();
            for h in &tick.moved {
                tree.note_moved(h.index());
            }
            sync(&mut t, &tick.moved, &mut flat, &mut flat_em, &cull);
        }
        check(&mut t, &mut tree, &flat, &flat_em, &mut r, "mid page turn");
        assert_eq!(checked, 400);
        assert!(culled_some > 100, "the cameras must exercise the subtree cull ({culled_some} of 400 dropped segments)");
    }
}
