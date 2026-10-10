//! The node tables: a pool of rows behind generation handles, the tree's
//! topology in depth-first order, and one dirty set per table.
//!
//! **Handles.** A [`NodeHandle`] is an index plus the generation the slot had
//! when the handle was issued (slotmap's scheme; Weissflog, "Handles are the
//! better pointers", 2018). Freeing bumps the slot's generation, so every
//! handle to the old occupant is refused with [`NodeError::Stale`] — before
//! the slot is reused and after. A freed slot is also QUARANTINED for
//! `frames_in_flight` frames before it can be reused: the CPU has forgotten
//! the node, but a frame still in flight on the GPU may read its rows, and a
//! reused row would hand that frame someone else's transform. A slot whose
//! generation would wrap is retired, never reused.
//!
//! **Topology.** Parent, first/last child and sibling links per node, and a
//! depth-first (pre-order) ORDER the CPU keeps, so any node's subtree is the
//! contiguous range `pos[n] .. pos[n] + size[n]` of it. A structural edit
//! (insert, remove, reparent) marks the order stale and the next
//! [`NodeTables::prepare_frame`] rebuilds it in one O(n) walk — measured at
//! ~0.3 ms for the 102k-node Linux tree (`resolve-bench`); a later step can
//! splice instead if structure ever changes per frame. Children keep their
//! insertion order, so a rebuild moves only what the edit moved.
//!
//! **Tables.** `local` (a [`Similarity`], 32 B), `post` (the non-inherited
//! per-axis scale, 16 B), `appearance` (tint, alpha, blend, 32 B) and the
//! GPU's topology row (parent, output group row; 8 B). Each has its own
//! setter and its own dirty set, so a transform write cannot touch a node's
//! appearance and an appearance write cannot touch its transform — the
//! library probe's E4, where one writer owned the whole group row and an
//! animation erased every tint and hide it crossed.
//!
//! **Per frame.** [`NodeTables::prepare_frame`] turns the dirty sets into a
//! [`FramePlan`]: the rows each table must upload (ascending), the span of
//! the order that changed, and the depth-first RANGES the resolve pass must
//! cover — the union of every dirty node's subtree, merged, then coalesced to
//! at most [`MAX_RESOLVE_RANGES`]. Coalescing may resolve a few clean nodes
//! between two dirty subtrees; that is harmless because the resolve walks
//! every node's whole parent chain from the LOCAL rows (no node reads
//! another's world row), so resolving a clean node rewrites the bits it
//! already had, whatever order the edits arrived in.

use std::collections::VecDeque;
use std::fmt;

use crate::dirty::DirtyBits;
use crate::transform::{Appearance, PostScale, Similarity};

/// "No node" / "no group row" in a u32 lane, on both sides.
pub const NONE: u32 = u32::MAX;
/// Frames a freed slot waits before reuse: the renderer submits one frame per
/// encoder and wgpu keeps at most a few in flight.
pub const DEFAULT_FRAMES_IN_FLIGHT: u64 = 3;
/// The resolve pass reads its ranges from a uniform; past this many the
/// smallest gaps are filled (see the module header for why that is safe).
pub const MAX_RESOLVE_RANGES: usize = 64;
/// The longest parent chain the resolve walks: a corrupted parent cycle must
/// not hang the GPU. The Linux tree is 11 deep below its root.
pub const MAX_DEPTH: u32 = 64;

/// An index into the node pool plus the generation it was issued at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeHandle {
    index: u32,
    generation: u32,
}

impl NodeHandle {
    /// The row this node occupies in every table (local, world, appearance…).
    pub fn index(self) -> u32 {
        self.index
    }
    pub fn generation(self) -> u32 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeError {
    /// The handle's node was freed (its slot may since hold another node).
    Stale(NodeHandle),
    /// The reparent would make a node its own ancestor.
    Cycle { node: NodeHandle, parent: NodeHandle },
}

impl fmt::Display for NodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NodeError::Stale(h) => write!(f, "stale node handle {}v{}", h.index, h.generation),
            NodeError::Cycle { node, parent } => {
                write!(f, "reparenting node {} under {} would make a cycle", node.index, parent.index)
            }
        }
    }
}

impl std::error::Error for NodeError {}

/// What one frame must upload and resolve; produced (and the dirty sets
/// cleared) by [`NodeTables::prepare_frame`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FramePlan {
    /// Depth-first `(start, len)` ranges to resolve, ascending, disjoint.
    pub ranges: Vec<(u32, u32)>,
    /// Sum of the ranges' lengths: the resolve pass's thread count.
    pub resolve_count: u32,
    /// Positions `[start, end)` of the order that changed since last frame.
    pub order_span: Option<(u32, u32)>,
    pub local_rows: Vec<u32>,
    pub post_rows: Vec<u32>,
    pub appearance_rows: Vec<u32>,
    pub topo_rows: Vec<u32>,
    /// Pool slots in use or quarantined (every table's row count).
    pub slot_count: u32,
    /// Live nodes (the order's length).
    pub live_count: u32,
}

impl FramePlan {
    pub fn is_empty(&self) -> bool {
        self.resolve_count == 0
            && self.order_span.is_none()
            && self.local_rows.is_empty()
            && self.post_rows.is_empty()
            && self.appearance_rows.is_empty()
            && self.topo_rows.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct NodeTables {
    generation: Vec<u32>,
    alive: Vec<bool>,
    free: VecDeque<u32>,
    quarantine: VecDeque<(u32, u64)>,
    frame: u64,
    frames_in_flight: u64,
    live: u32,

    parent: Vec<u32>,
    first_child: Vec<u32>,
    last_child: Vec<u32>,
    next_sibling: Vec<u32>,
    prev_sibling: Vec<u32>,
    roots: Vec<u32>,
    group_row: Vec<u32>,

    order: Vec<u32>,
    pos: Vec<u32>,
    size: Vec<u32>,
    order_stale: bool,
    order_changed: Option<(u32, u32)>,

    local: Vec<Similarity>,
    post: Vec<PostScale>,
    appearance: Vec<Appearance>,

    dirty_local: DirtyBits,
    dirty_post: DirtyBits,
    dirty_appearance: DirtyBits,
    dirty_topo: DirtyBits,
    dirty_subtree: DirtyBits,
}

impl Default for NodeTables {
    fn default() -> Self {
        Self::new(DEFAULT_FRAMES_IN_FLIGHT)
    }
}

impl NodeTables {
    pub fn new(frames_in_flight: u64) -> Self {
        Self {
            generation: Vec::new(),
            alive: Vec::new(),
            free: VecDeque::new(),
            quarantine: VecDeque::new(),
            frame: 0,
            frames_in_flight,
            live: 0,
            parent: Vec::new(),
            first_child: Vec::new(),
            last_child: Vec::new(),
            next_sibling: Vec::new(),
            prev_sibling: Vec::new(),
            roots: Vec::new(),
            group_row: Vec::new(),
            order: Vec::new(),
            pos: Vec::new(),
            size: Vec::new(),
            order_stale: false,
            order_changed: None,
            local: Vec::new(),
            post: Vec::new(),
            appearance: Vec::new(),
            dirty_local: DirtyBits::default(),
            dirty_post: DirtyBits::default(),
            dirty_appearance: DirtyBits::default(),
            dirty_topo: DirtyBits::default(),
            dirty_subtree: DirtyBits::default(),
        }
    }

    /// Pool slots ever allocated (live, quarantined or free): every table's
    /// row count, and the GPU buffers' minimum capacity.
    pub fn slot_count(&self) -> u32 {
        self.alive.len() as u32
    }

    pub fn live_count(&self) -> u32 {
        self.live
    }

    /// The frame counter [`Self::prepare_frame`] advances (quarantine clock).
    pub fn frame(&self) -> u64 {
        self.frame
    }

    pub fn is_live(&self, h: NodeHandle) -> bool {
        let i = h.index as usize;
        i < self.alive.len() && self.alive[i] && self.generation[i] == h.generation
    }

    fn check(&self, h: NodeHandle) -> Result<usize, NodeError> {
        if self.is_live(h) {
            Ok(h.index as usize)
        } else {
            Err(NodeError::Stale(h))
        }
    }

    /// A new node under `parent` (a root when None), appended after its
    /// siblings, with the given rows and no group row.
    pub fn insert(
        &mut self,
        parent: Option<NodeHandle>,
        local: Similarity,
        appearance: Appearance,
    ) -> Result<NodeHandle, NodeError> {
        let p = match parent {
            Some(h) => self.check(h)? as u32,
            None => NONE,
        };
        let index = match self.free.pop_front() {
            Some(i) => i,
            None => {
                let i = self.alive.len() as u32;
                self.generation.push(0);
                self.alive.push(false);
                self.parent.push(NONE);
                self.first_child.push(NONE);
                self.last_child.push(NONE);
                self.next_sibling.push(NONE);
                self.prev_sibling.push(NONE);
                self.group_row.push(NONE);
                self.pos.push(NONE);
                self.size.push(0);
                self.local.push(Similarity::IDENTITY);
                self.post.push(PostScale::IDENTITY);
                self.appearance.push(Appearance::IDENTITY);
                i
            }
        };
        let i = index as usize;
        self.alive[i] = true;
        self.live += 1;
        self.group_row[i] = NONE;
        self.local[i] = local;
        self.post[i] = PostScale::IDENTITY;
        self.appearance[i] = appearance;
        self.link(index, p);
        self.order_stale = true;
        self.dirty_local.set(index);
        self.dirty_post.set(index);
        self.dirty_appearance.set(index);
        self.dirty_topo.set(index);
        self.dirty_subtree.set(index);
        Ok(NodeHandle { index, generation: self.generation[i] })
    }

    fn link(&mut self, n: u32, p: u32) {
        let i = n as usize;
        self.parent[i] = p;
        self.next_sibling[i] = NONE;
        if p == NONE {
            self.prev_sibling[i] = NONE;
            self.roots.push(n);
            return;
        }
        let pi = p as usize;
        let last = self.last_child[pi];
        self.prev_sibling[i] = last;
        if last == NONE {
            self.first_child[pi] = n;
        } else {
            self.next_sibling[last as usize] = n;
        }
        self.last_child[pi] = n;
    }

    fn unlink(&mut self, n: u32) {
        let i = n as usize;
        let p = self.parent[i];
        if p == NONE {
            let at = self.roots.iter().position(|&r| r == n).expect("a root is in the root list");
            self.roots.remove(at);
        } else {
            let (prev, next) = (self.prev_sibling[i], self.next_sibling[i]);
            let pi = p as usize;
            if prev == NONE {
                self.first_child[pi] = next;
            } else {
                self.next_sibling[prev as usize] = next;
            }
            if next == NONE {
                self.last_child[pi] = prev;
            } else {
                self.prev_sibling[next as usize] = prev;
            }
        }
        self.parent[i] = NONE;
        self.prev_sibling[i] = NONE;
        self.next_sibling[i] = NONE;
    }

    /// Free `h` and its whole subtree; returns how many nodes went. Every
    /// handle to them is stale from here on; their slots wait out the
    /// quarantine before reuse.
    pub fn remove(&mut self, h: NodeHandle) -> Result<usize, NodeError> {
        let root = self.check(h)? as u32;
        self.unlink(root);
        let mut stack = vec![root];
        let mut freed = 0;
        while let Some(n) = stack.pop() {
            let i = n as usize;
            let mut c = self.first_child[i];
            while c != NONE {
                stack.push(c);
                c = self.next_sibling[c as usize];
            }
            self.alive[i] = false;
            self.first_child[i] = NONE;
            self.last_child[i] = NONE;
            self.parent[i] = NONE;
            self.next_sibling[i] = NONE;
            self.prev_sibling[i] = NONE;
            self.pos[i] = NONE;
            self.live -= 1;
            freed += 1;
            match self.generation[i].checked_add(1) {
                Some(g) if g != u32::MAX => {
                    self.generation[i] = g;
                    self.quarantine.push_back((n, self.frame));
                }
                // Retired: a wrapped generation could match a handle from
                // four billion frees ago.
                _ => self.generation[i] = u32::MAX,
            }
        }
        self.order_stale = true;
        Ok(freed)
    }

    /// Move `h` (with its subtree) under `parent` (a root when None),
    /// keeping its LOCAL transform, so its world transform follows the new
    /// parent (bevy's `set_parent`, not `set_parent_in_place`).
    pub fn reparent(&mut self, h: NodeHandle, parent: Option<NodeHandle>) -> Result<(), NodeError> {
        let n = self.check(h)? as u32;
        let p = match parent {
            Some(ph) => {
                let p = self.check(ph)? as u32;
                // Refuse a cycle: the new parent may not be h or below it.
                let mut a = p;
                while a != NONE {
                    if a == n {
                        return Err(NodeError::Cycle { node: h, parent: ph });
                    }
                    a = self.parent[a as usize];
                }
                p
            }
            None => NONE,
        };
        self.unlink(n);
        self.link(n, p);
        self.order_stale = true;
        self.dirty_topo.set(n);
        self.dirty_subtree.set(n);
        Ok(())
    }

    pub fn parent(&self, h: NodeHandle) -> Result<Option<NodeHandle>, NodeError> {
        let i = self.check(h)?;
        let p = self.parent[i];
        Ok((p != NONE).then(|| NodeHandle { index: p, generation: self.generation[p as usize] }))
    }

    pub fn children(&self, h: NodeHandle) -> Result<Vec<NodeHandle>, NodeError> {
        let i = self.check(h)?;
        let mut out = Vec::new();
        let mut c = self.first_child[i];
        while c != NONE {
            out.push(NodeHandle { index: c, generation: self.generation[c as usize] });
            c = self.next_sibling[c as usize];
        }
        Ok(out)
    }

    pub fn local(&self, h: NodeHandle) -> Result<Similarity, NodeError> {
        Ok(self.local[self.check(h)?])
    }

    pub fn post_scale(&self, h: NodeHandle) -> Result<PostScale, NodeError> {
        Ok(self.post[self.check(h)?])
    }

    pub fn appearance(&self, h: NodeHandle) -> Result<Appearance, NodeError> {
        Ok(self.appearance[self.check(h)?])
    }

    pub fn group_row(&self, h: NodeHandle) -> Result<Option<u32>, NodeError> {
        let r = self.group_row[self.check(h)?];
        Ok((r != NONE).then_some(r))
    }

    /// Write the node's local transform: its subtree re-resolves.
    pub fn set_local(&mut self, h: NodeHandle, local: Similarity) -> Result<(), NodeError> {
        let i = self.check(h)?;
        self.local[i] = local;
        self.dirty_local.set(i as u32);
        self.dirty_subtree.set(i as u32);
        Ok(())
    }

    /// Write the node's non-inherited per-axis scale.
    pub fn set_post_scale(&mut self, h: NodeHandle, xyz: [f32; 3]) -> Result<(), NodeError> {
        let i = self.check(h)?;
        self.post[i] = PostScale { xyz, _pad: 0.0 };
        self.dirty_post.set(i as u32);
        // Only the node's own output row reads it, but that row is written
        // by the resolve, so the node must be in a range.
        self.dirty_subtree.set(i as u32);
        Ok(())
    }

    /// Write the node's appearance: alpha inherits, so its subtree re-resolves.
    pub fn set_appearance(&mut self, h: NodeHandle, a: Appearance) -> Result<(), NodeError> {
        let i = self.check(h)?;
        self.appearance[i] = a;
        self.dirty_appearance.set(i as u32);
        self.dirty_subtree.set(i as u32);
        Ok(())
    }

    /// Route the node's resolved transform and appearance into row `row` of
    /// an output group table (the transitional draw path), or stop (None).
    pub fn set_group_row(&mut self, h: NodeHandle, row: Option<u32>) -> Result<(), NodeError> {
        let i = self.check(h)?;
        self.group_row[i] = row.unwrap_or(NONE);
        self.dirty_topo.set(i as u32);
        self.dirty_subtree.set(i as u32);
        Ok(())
    }

    /// The node's world transform and inherited alpha, composed on the CPU
    /// in the order the GPU resolve composes it (bottom-up along the chain).
    pub fn world(&self, h: NodeHandle) -> Result<(Similarity, f32), NodeError> {
        let i = self.check(h)?;
        let mut acc = self.local[i];
        let mut alpha = self.appearance[i].tint[3];
        let mut p = self.parent[i];
        let mut steps = 0;
        while p != NONE && steps < MAX_DEPTH {
            let pi = p as usize;
            acc = Similarity::compose(&self.local[pi], &acc);
            alpha *= self.appearance[pi].tint[3];
            p = self.parent[pi];
            steps += 1;
        }
        Ok((acc, alpha))
    }

    /// The world transform of the node's parent (identity for a root): the
    /// frame a world-space drag must be converted into.
    pub fn parent_world(&self, h: NodeHandle) -> Result<Similarity, NodeError> {
        match self.parent(h)? {
            Some(p) => Ok(self.world(p)?.0),
            None => Ok(Similarity::IDENTITY),
        }
    }

    /// Bring the depth-first order up to date (prepare_frame does this; a
    /// caller asking for positions between frames may too).
    pub fn ensure_order(&mut self) {
        if !self.order_stale {
            return;
        }
        let old = std::mem::take(&mut self.order);
        let mut order = Vec::with_capacity(self.live as usize);
        let mut stack: Vec<u32> = Vec::with_capacity(64);
        for &r in &self.roots {
            stack.push(r);
            while let Some(n) = stack.pop() {
                let i = n as usize;
                self.pos[i] = order.len() as u32;
                self.size[i] = 1;
                order.push(n);
                let mut c = self.last_child[i];
                while c != NONE {
                    stack.push(c);
                    c = self.prev_sibling[c as usize];
                }
            }
        }
        for &n in order.iter().rev() {
            let p = self.parent[n as usize];
            if p != NONE {
                self.size[p as usize] += self.size[n as usize];
            }
        }
        debug_assert_eq!(order.len() as u32, self.live, "every live node is under a root");
        // The span a GPU copy must refresh: first to last differing position
        // (positions past the new length are never dispatched).
        let first = order.iter().zip(&old).position(|(a, b)| a != b).unwrap_or(order.len().min(old.len()));
        if first < order.len() {
            let last = if order.len() == old.len() {
                order.iter().zip(&old).rposition(|(a, b)| a != b).unwrap_or(first)
            } else {
                order.len() - 1
            };
            let span = (first as u32, last as u32 + 1);
            self.order_changed = Some(match self.order_changed {
                Some((a, b)) => (a.min(span.0), b.max(span.1).min(order.len() as u32)),
                None => span,
            });
        }
        self.order = order;
        self.order_stale = false;
    }

    /// Depth-first position → node index.
    pub fn order(&mut self) -> &[u32] {
        self.ensure_order();
        &self.order
    }

    /// The node's subtree as a depth-first `(start, len)` range.
    pub fn subtree_range(&mut self, h: NodeHandle) -> Result<(u32, u32), NodeError> {
        let i = self.check(h)?;
        self.ensure_order();
        Ok((self.pos[i], self.size[i]))
    }

    pub fn local_rows(&self) -> &[Similarity] {
        &self.local
    }
    pub fn post_rows(&self) -> &[PostScale] {
        &self.post
    }
    pub fn appearance_rows(&self) -> &[Appearance] {
        &self.appearance
    }
    /// The GPU's topology row for slot `i`: (parent, output group row).
    pub fn topo_row(&self, i: u32) -> [u32; 2] {
        [self.parent[i as usize], self.group_row[i as usize]]
    }
    pub fn order_rows(&self) -> &[u32] {
        &self.order
    }

    /// Mark every live row of every table dirty and the whole order changed
    /// (a fresh GPU mirror, e.g. after its buffers grew).
    pub fn mark_all_dirty(&mut self) {
        for i in 0..self.alive.len() as u32 {
            if self.alive[i as usize] {
                self.dirty_local.set(i);
                self.dirty_post.set(i);
                self.dirty_appearance.set(i);
                self.dirty_topo.set(i);
                self.dirty_subtree.set(i);
            }
        }
        self.ensure_order();
        if !self.order.is_empty() {
            self.order_changed = Some((0, self.order.len() as u32));
        }
    }

    /// Close the frame's edits into a plan, clear the dirty sets, and advance
    /// the quarantine clock.
    pub fn prepare_frame(&mut self) -> FramePlan {
        self.ensure_order();
        let ranges = coalesce(self.dirty_ranges(), MAX_RESOLVE_RANGES);
        let resolve_count = ranges.iter().map(|r| r.1).sum();
        let live_rows = |d: &DirtyBits, alive: &[bool]| -> Vec<u32> { d.iter().filter(|&i| alive[i as usize]).collect() };
        let plan = FramePlan {
            ranges,
            resolve_count,
            order_span: self.order_changed.take(),
            local_rows: live_rows(&self.dirty_local, &self.alive),
            post_rows: live_rows(&self.dirty_post, &self.alive),
            appearance_rows: live_rows(&self.dirty_appearance, &self.alive),
            topo_rows: live_rows(&self.dirty_topo, &self.alive),
            slot_count: self.slot_count(),
            live_count: self.live,
        };
        self.dirty_local.clear();
        self.dirty_post.clear();
        self.dirty_appearance.clear();
        self.dirty_topo.clear();
        self.dirty_subtree.clear();
        self.advance_frame();
        plan
    }

    /// One frame has been handed to the GPU: release the slots whose
    /// quarantine has run out.
    pub fn advance_frame(&mut self) {
        self.frame += 1;
        while let Some(&(i, freed)) = self.quarantine.front() {
            if freed + self.frames_in_flight > self.frame {
                break;
            }
            self.quarantine.pop_front();
            self.free.push_back(i);
        }
    }

    /// The union of every dirty node's subtree, merged, ascending. Few dirty
    /// nodes sort; many sweep the order once (an O(n) pass beats sorting
    /// tens of thousands of ranges).
    fn dirty_ranges(&self) -> Vec<(u32, u32)> {
        let n = self.order.len();
        let dirty = self.dirty_subtree.len();
        if dirty == 0 || n == 0 {
            return Vec::new();
        }
        let mut out: Vec<(u32, u32)> = Vec::new();
        if dirty <= 4096 {
            let mut spans: Vec<(u32, u32)> = self
                .dirty_subtree
                .iter()
                .filter(|&i| self.alive[i as usize])
                .map(|i| (self.pos[i as usize], self.pos[i as usize] + self.size[i as usize]))
                .collect();
            spans.sort_unstable();
            for (s, e) in spans {
                match out.last_mut() {
                    Some(last) if s <= last.1 => last.1 = last.1.max(e),
                    _ => out.push((s, e)),
                }
            }
        } else {
            let mut end_at = vec![0u32; n];
            for i in self.dirty_subtree.iter().filter(|&i| self.alive[i as usize]) {
                let (s, e) = (self.pos[i as usize] as usize, self.pos[i as usize] + self.size[i as usize]);
                end_at[s] = end_at[s].max(e);
            }
            let mut cur: Option<(u32, u32)> = None;
            for (p, &e) in end_at.iter().enumerate() {
                let p = p as u32;
                match &mut cur {
                    Some(c) if p <= c.1 => c.1 = c.1.max(e),
                    _ => {
                        if let Some(c) = cur.take() {
                            out.push(c);
                        }
                        if e > p {
                            cur = Some((p, e));
                        }
                    }
                }
            }
            out.extend(cur);
        }
        out.into_iter().map(|(s, e)| (s, e - s)).collect()
    }

    /// Test and debug aid: every structural invariant the resolve relies on.
    /// Panics with the first one that fails.
    pub fn assert_invariants(&mut self) {
        self.ensure_order();
        assert_eq!(self.order.len() as u32, self.live, "order length is the live count");
        for (p, &n) in self.order.iter().enumerate() {
            let i = n as usize;
            assert!(self.alive[i], "order holds only live nodes ({n} at {p})");
            assert_eq!(self.pos[i] as usize, p, "pos inverts order for node {n}");
            // Pre-order: the first child follows its parent, each next sibling
            // follows the previous one's subtree, and the children's sizes add
            // up to the parent's.
            let mut expect = p as u32 + 1;
            let mut c = self.first_child[i];
            while c != NONE {
                assert!(self.alive[c as usize], "child {c} of {n} is live");
                assert_eq!(self.parent[c as usize], n, "child {c} names {n} as parent");
                assert_eq!(self.pos[c as usize], expect, "child {c} of {n} sits where pre-order puts it");
                expect += self.size[c as usize];
                c = self.next_sibling[c as usize];
            }
            assert_eq!(expect, p as u32 + self.size[i], "subtree size of node {n}");
        }
    }
}

/// Fill the smallest gaps until at most `max` ranges remain.
fn coalesce(ranges: Vec<(u32, u32)>, max: usize) -> Vec<(u32, u32)> {
    if ranges.len() <= max {
        return ranges;
    }
    let mut gaps: Vec<(u32, usize)> =
        ranges.windows(2).enumerate().map(|(k, w)| (w[1].0 - (w[0].0 + w[0].1), k)).collect();
    gaps.sort_unstable();
    let mut fill = vec![false; ranges.len()];
    for &(_, k) in gaps.iter().take(ranges.len() - max) {
        fill[k] = true; // merge range k with k + 1
    }
    let mut out: Vec<(u32, u32)> = Vec::with_capacity(max);
    let mut start = ranges[0].0;
    for (k, r) in ranges.iter().enumerate() {
        if !fill[k] {
            out.push((start, r.0 + r.1 - start));
            if let Some(next) = ranges.get(k + 1) {
                start = next.0;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(t: &mut NodeTables, parent: Option<NodeHandle>) -> NodeHandle {
        t.insert(parent, Similarity::IDENTITY, Appearance::IDENTITY).unwrap()
    }

    #[test]
    fn a_stale_handle_is_refused_and_never_aliases_a_reused_slot() {
        let mut t = NodeTables::new(2);
        let root = leaf(&mut t, None);
        let a = leaf(&mut t, Some(root));
        assert_eq!(t.remove(a), Ok(1));
        // Refused straight away, by every accessor and every writer.
        assert_eq!(t.set_local(a, Similarity::from_translation([1.0; 3])), Err(NodeError::Stale(a)));
        assert_eq!(t.local(a), Err(NodeError::Stale(a)));
        assert_eq!(t.world(a).map(|_| ()), Err(NodeError::Stale(a)));
        assert_eq!(t.remove(a), Err(NodeError::Stale(a)));
        assert_eq!(t.insert(Some(a), Similarity::IDENTITY, Appearance::IDENTITY), Err(NodeError::Stale(a)));
        // Quarantined: an insert inside the frames in flight takes a new slot.
        let b = leaf(&mut t, Some(root));
        assert_ne!(b.index(), a.index(), "a freed slot is not reused while frames are in flight");
        t.prepare_frame();
        let c = leaf(&mut t, Some(root));
        assert_ne!(c.index(), a.index(), "still in quarantine after one of two frames");
        t.prepare_frame();
        // Released: the slot is reused under a new generation, and the old
        // handle still cannot reach it.
        let d = leaf(&mut t, Some(root));
        assert_eq!(d.index(), a.index(), "the quarantined slot is reused once it has run out");
        assert_ne!(d.generation(), a.generation());
        t.set_local(d, Similarity::from_translation([5.0, 0.0, 0.0])).unwrap();
        assert_eq!(t.local(a), Err(NodeError::Stale(a)), "the old handle must not read the new occupant");
        assert_eq!(t.set_local(a, Similarity::IDENTITY), Err(NodeError::Stale(a)));
        assert_eq!(t.local(d).unwrap().translation, [5.0, 0.0, 0.0]);
        t.assert_invariants();
    }

    #[test]
    fn removing_a_node_frees_its_subtree() {
        let mut t = NodeTables::default();
        let root = leaf(&mut t, None);
        let dir = leaf(&mut t, Some(root));
        let sub = leaf(&mut t, Some(dir));
        let file = leaf(&mut t, Some(sub));
        let keep = leaf(&mut t, Some(root));
        assert_eq!(t.remove(dir), Ok(3));
        for h in [dir, sub, file] {
            assert!(!t.is_live(h));
        }
        assert!(t.is_live(keep));
        assert_eq!(t.live_count(), 2);
        assert_eq!(t.children(root).unwrap(), vec![keep]);
        t.assert_invariants();
    }

    /// A small tree: root → {a → {a1, a2 → {a2x}}, b → {b1}, c}.
    fn tree(t: &mut NodeTables) -> [NodeHandle; 9] {
        let root = leaf(t, None);
        let a = leaf(t, Some(root));
        let a1 = leaf(t, Some(a));
        let a2 = leaf(t, Some(a));
        let a2x = leaf(t, Some(a2));
        let b = leaf(t, Some(root));
        let b1 = leaf(t, Some(b));
        let c = leaf(t, Some(root));
        let lone = leaf(t, None);
        [root, a, a1, a2, a2x, b, b1, c, lone]
    }

    #[test]
    fn a_dirty_subtree_is_one_contiguous_range_and_reparenting_keeps_it_so() {
        let mut t = NodeTables::default();
        let [root, a, a1, a2, a2x, b, b1, c, lone] = tree(&mut t);
        t.assert_invariants();
        let names = |t: &mut NodeTables, hs: &[NodeHandle]| -> Vec<u32> {
            let order = t.order().to_vec();
            hs.iter().map(|h| order.iter().position(|&n| n == h.index()).unwrap() as u32).collect()
        };
        assert_eq!(names(&mut t, &[root, a, a1, a2, a2x, b, b1, c, lone]), [0, 1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(t.subtree_range(a).unwrap(), (1, 4));
        t.prepare_frame();

        // One dirty node: exactly its subtree.
        t.set_local(a2, Similarity::from_translation([1.0, 0.0, 0.0])).unwrap();
        let plan = t.prepare_frame();
        assert_eq!(plan.ranges, vec![(3, 2)]);
        assert_eq!(plan.local_rows, vec![a2.index()]);
        assert!(plan.appearance_rows.is_empty(), "a transform write leaves the appearance table clean");

        // An ancestor subsumes its descendants; disjoint siblings stay apart.
        t.set_local(a2x, Similarity::IDENTITY).unwrap();
        t.set_local(a, Similarity::IDENTITY).unwrap();
        t.set_appearance(c, Appearance::IDENTITY).unwrap();
        let plan = t.prepare_frame();
        assert_eq!(plan.ranges, vec![(1, 4), (7, 1)]);
        assert_eq!(plan.resolve_count, 5);
        assert_eq!(plan.appearance_rows, vec![c.index()]);
        assert!(t.prepare_frame().is_empty(), "nothing dirty, nothing to do");

        // Reparent a2 (with a2x) under b: the order stays pre-order, the
        // moved subtree's range follows it, and its world now follows b.
        t.set_local(b, Similarity::from_translation([0.0, 10.0, 0.0])).unwrap();
        t.set_local(a2, Similarity::from_translation([1.0, 0.0, 0.0])).unwrap();
        t.prepare_frame();
        t.reparent(a2, Some(b)).unwrap();
        t.assert_invariants();
        assert_eq!(names(&mut t, &[root, a, a1, b, b1, a2, a2x, c, lone]), [0, 1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(t.subtree_range(b).unwrap(), (3, 4));
        assert_eq!(t.subtree_range(a).unwrap(), (1, 2));
        let plan = t.prepare_frame();
        assert_eq!(plan.ranges, vec![(5, 2)], "only the moved subtree re-resolves");
        assert_eq!(plan.order_span, Some((3, 7)), "the order changed from a2's old place to its new one");
        assert_eq!(plan.topo_rows, vec![a2.index()]);
        assert_eq!(t.world(a2x).unwrap().0.translation, [1.0, 10.0, 0.0]);

        // To a root and back; a cycle is refused and changes nothing.
        t.reparent(a, None).unwrap();
        t.assert_invariants();
        assert_eq!(t.reparent(b, Some(b1)), Err(NodeError::Cycle { node: b, parent: b1 }));
        assert_eq!(t.reparent(root, Some(a2x)), Err(NodeError::Cycle { node: root, parent: a2x }));
        t.reparent(a, Some(root)).unwrap();
        t.assert_invariants();
        assert_eq!(t.parent(a).unwrap(), Some(root));
        assert_eq!(t.children(root).unwrap(), vec![b, c, a]);
    }

    #[test]
    fn many_dirty_nodes_sweep_to_the_same_ranges_a_sort_gives() {
        // Past the sort threshold (4096 dirty nodes) the ranges come from the
        // O(n) sweep; both must agree with a brute-force cover.
        let mut t = NodeTables::default();
        let root = leaf(&mut t, None);
        let mut dirs = Vec::new();
        for _ in 0..200 {
            let d = leaf(&mut t, Some(root));
            for _ in 0..40 {
                leaf(&mut t, Some(d));
            }
            dirs.push(d);
        }
        t.prepare_frame();
        for (k, &d) in dirs.iter().enumerate() {
            if k % 3 != 1 {
                for (j, f) in t.children(d).unwrap().into_iter().enumerate() {
                    if j % 5 != 4 {
                        t.set_local(f, Similarity::IDENTITY).unwrap();
                    }
                }
            }
            if k % 7 == 0 {
                t.set_local(d, Similarity::IDENTITY).unwrap();
            }
        }
        // The brute-force cover: every dirty node's whole subtree.
        let mut cover = vec![false; t.live_count() as usize];
        for i in t.dirty_subtree.iter() {
            let (s, n) = (t.pos[i as usize], t.size[i as usize]);
            for p in s..s + n {
                cover[p as usize] = true;
            }
        }
        assert!(t.dirty_subtree.len() > 4096, "the test must reach the sweep");
        let ranges = t.dirty_ranges();
        let mut got = vec![false; cover.len()];
        for (s, n) in &ranges {
            for p in *s..s + n {
                got[p as usize] = true;
            }
        }
        assert_eq!(got, cover);
        for w in ranges.windows(2) {
            assert!(w[0].0 + w[0].1 < w[1].0, "merged ranges are disjoint and non-adjacent: {w:?}");
        }
    }

    #[test]
    fn coalescing_fills_the_smallest_gaps_first() {
        let r = vec![(0, 2), (3, 1), (10, 1), (12, 3), (40, 1)];
        assert_eq!(coalesce(r.clone(), 5), r);
        assert_eq!(coalesce(r.clone(), 3), vec![(0, 4), (10, 5), (40, 1)]);
        assert_eq!(coalesce(r, 1), vec![(0, 41)]);
    }

    #[test]
    fn world_composes_the_whole_chain_bottom_up_with_alpha_multiplied() {
        let mut t = NodeTables::default();
        let root = t.insert(None, Similarity { scale: 2.0, ..Similarity::from_translation([10.0, 0.0, 0.0]) }, Appearance::IDENTITY).unwrap();
        let dir = t
            .insert(Some(root), Similarity::from_translation([0.0, 5.0, 0.0]), Appearance { tint: [1.0, 1.0, 1.0, 0.5], ..Appearance::IDENTITY })
            .unwrap();
        let file = t
            .insert(Some(dir), Similarity::from_translation([1.0, 1.0, 1.0]), Appearance { tint: [0.2, 0.4, 0.6, 0.5], ..Appearance::IDENTITY })
            .unwrap();
        let (w, alpha) = t.world(file).unwrap();
        let expect = Similarity::compose(&t.local(root).unwrap(), &Similarity::compose(&t.local(dir).unwrap(), &t.local(file).unwrap()));
        assert_eq!(w, expect);
        assert_eq!(w.translation, [12.0, 12.0, 2.0]);
        assert_eq!(alpha, 0.25);
        assert_eq!(t.parent_world(file).unwrap(), t.world(dir).unwrap().0);
    }
}
