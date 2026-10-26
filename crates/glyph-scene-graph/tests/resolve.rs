//! The frame resolve against the chain walk (`NodeTables::world`, every
//! node composed from its whole chain of local rows).
//!
//! 1. A random tree (5,000 nodes, depth up to 9, rotations, scales, alphas)
//!    through every kind of edit — a few moves, scattered moves, an
//!    appearance alone, a fifth of the tree, a reparent, a free with slot
//!    reuse; after each resolve EVERY live node's world row is compared bit
//!    for bit, so a node the plan wrongly skipped is caught as surely as a
//!    wrong one, and a frame with no edit resolves nothing.
//! 2. The library's shape: chains of pure translations under a scaled mount
//!    resolve to the ROOT-FIRST sum (bevy's order, which the library's rows
//!    were drawn with before it moved onto these tables).

use glyph_scene_graph::{Appearance, NodeHandle, NodeTables, Similarity};

/// A small deterministic generator (no rand dependency).
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }
    fn unit(&mut self) -> f32 {
        self.next() as f32 / (1u64 << 31) as f32
    }
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit()
    }
}

fn random_local(r: &mut Lcg) -> Similarity {
    let axis = [r.range(-1.0, 1.0), r.range(-1.0, 1.0), r.range(-1.0, 1.0)];
    let n = (axis[0] * axis[0] + axis[1] * axis[1] + axis[2] * axis[2]).sqrt().max(1e-3);
    let half = r.range(-0.6, 0.6);
    let (s, c) = half.sin_cos();
    Similarity {
        translation: [r.range(-50.0, 50.0), r.range(-50.0, 50.0), r.range(-20.0, 20.0)],
        scale: r.range(0.5, 1.6),
        rotation: [axis[0] / n * s, axis[1] / n * s, axis[2] / n * s, c],
    }
}

/// Every live node's resolved world and alpha against the chain walk.
fn assert_worlds(t: &NodeTables, nodes: &[NodeHandle], what: &str) {
    let mut seen = 0;
    for &h in nodes.iter().filter(|h| t.is_live(**h)) {
        let walked = t.world(h).unwrap();
        let resolved = *t.resolved_world(h.index());
        let (a, b): ([u32; 8], [u32; 8]) = (bytemuck::cast(walked.0), bytemuck::cast(resolved.0));
        assert_eq!(a, b, "{what}: node {} world: resolved {:?} walked {:?}", h.index(), resolved.0, walked.0);
        assert_eq!(walked.1.to_bits(), resolved.1.to_bits(), "{what}: node {} alpha", h.index());
        seen += 1;
    }
    assert!(seen > 0, "{what}: compared something");
}

#[test]
fn the_frame_resolve_matches_the_chain_walk_through_every_kind_of_edit() {
    let mut t = NodeTables::default();
    let mut r = Lcg(0x5EED);
    let root = t.insert(None, random_local(&mut r), Appearance::IDENTITY).unwrap();
    let mut nodes = vec![root];
    let mut depth = vec![0u32];
    while nodes.len() < 5_000 {
        // Prefer recent nodes as parents so chains get deep (up to 9).
        let k = nodes.len() - 1 - (r.next() as usize % nodes.len().min(40));
        let k = if depth[k] >= 9 { 0 } else { k };
        let a = Appearance { tint: [r.unit(), r.unit(), r.unit(), r.range(0.5, 1.0)], ..Appearance::IDENTITY };
        nodes.push(t.insert(Some(nodes[k]), random_local(&mut r), a).unwrap());
        depth.push(depth[k] + 1);
    }
    assert!(depth.contains(&9), "the tree reaches depth 9");

    let p = t.resolve_frame();
    assert_eq!(p.resolved, 5_000, "the first frame resolves everything");
    assert_worlds(&t, &nodes, "initial");
    assert!(t.resolve_frame().is_empty(), "nothing marked, nothing resolved");

    // A few nodes, then scattered ones.
    for k in [17usize, 18, 400] {
        t.set_local(nodes[k], random_local(&mut r)).unwrap();
    }
    assert!(!t.resolve_frame().is_empty());
    assert_worlds(&t, &nodes, "a few");
    for k in (0..nodes.len()).step_by(97) {
        t.set_local(nodes[k], random_local(&mut r)).unwrap();
    }
    t.resolve_frame();
    assert_worlds(&t, &nodes, "scattered");

    // Appearance alone: alpha inherits, so the subtree resolves.
    t.set_appearance(nodes[3], Appearance { tint: [0.1, 0.2, 0.3, 0.25], ..Appearance::IDENTITY }).unwrap();
    let (_, n3) = t.subtree_range(nodes[3]).unwrap();
    assert_eq!(t.resolve_frame().resolved, n3);
    assert_worlds(&t, &nodes, "appearance");

    // A fifth of the tree (past the sorted path's 4,096 marks: the sweep).
    for k in (0..nodes.len()).step_by(5) {
        t.set_local(nodes[k], random_local(&mut r)).unwrap();
    }
    for &h in nodes.iter().skip(1).step_by(7) {
        t.set_post_scale(h, [r.range(0.5, 2.0), 1.0, r.range(0.5, 2.0)]).unwrap();
    }
    t.resolve_frame();
    assert_worlds(&t, &nodes, "a fifth");

    // Reparent a deep node's subtree under another branch.
    let moved = nodes[2_500];
    t.reparent(moved, Some(nodes[10])).unwrap();
    t.assert_invariants();
    t.resolve_frame();
    assert_worlds(&t, &nodes, "reparent");

    // Free a subtree and reuse its slots.
    let gone = nodes[1_000];
    let freed = t.remove(gone).unwrap();
    t.resolve_frame();
    for _ in 0..freed {
        let parent = nodes[r.next() as usize % 50];
        let parent = if t.is_live(parent) { parent } else { root };
        nodes.push(t.insert(Some(parent), random_local(&mut r), Appearance::IDENTITY).unwrap());
    }
    assert!(nodes[nodes.len() - freed..].iter().any(|h| h.index() == gone.index()), "a freed slot was reused");
    t.resolve_frame();
    assert_worlds(&t, &nodes, "reuse");
}

/// The library's shape (2026-10-10): chains of pure translations — root,
/// directories, volume, sheet — then a mount carrying a uniform scale and a
/// file at the identity. The resolve must give the ROOT-FIRST sum; values
/// are picked so a leaf-first sum differs in the last bit for most chains.
#[test]
fn translation_chains_resolve_root_first() {
    let mut t = NodeTables::default();
    let mut r = Lcg(0x11B2A2);
    let root = t.insert(None, Similarity::IDENTITY, Appearance::IDENTITY).unwrap();
    let mut leaves = Vec::new();
    let mut order_tells = 0;
    for _ in 0..500 {
        let depth = 2 + (r.next() % 8) as usize;
        let mut p = root;
        let mut sum = 0.0f32;
        let mut rev: Vec<f32> = Vec::new();
        for _ in 0..depth {
            let x = r.range(-300.0, 300.0);
            sum += x;
            rev.push(x);
            p = t.insert(Some(p), Similarity::from_translation([x, r.range(-200.0, 0.0), -32.0]), Appearance::IDENTITY).unwrap();
        }
        let leaf_first = rev.iter().rev().fold(0.0f32, |acc, x| x + acc);
        order_tells += usize::from(leaf_first.to_bits() != sum.to_bits());
        let s = r.range(0.0086, 4.0);
        let mount = t.insert(Some(p), Similarity { scale: s, ..Similarity::from_translation([r.range(-30.0, 30.0), r.range(-40.0, 40.0), 0.0]) }, Appearance::IDENTITY).unwrap();
        let file = t.insert(Some(mount), Similarity::IDENTITY, Appearance::IDENTITY).unwrap();
        leaves.push((file, sum));
    }
    assert!(order_tells > 50, "the corpus must tell the summation orders apart ({order_tells} chains do)");
    t.resolve_frame();
    // The mount's offset is the last term, added onto the directories' sum.
    for (file, sum) in leaves {
        let mount = t.parent(file).unwrap().unwrap();
        let m = t.local(mount).unwrap().translation[0];
        assert_eq!(t.resolved_world(file.index()).0.translation[0].to_bits(), (sum + m).to_bits());
    }
}
