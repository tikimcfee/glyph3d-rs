//! resolve-bench: what the scene graph's per-frame resolve costs on this CPU.
//!
//! Builds a tree once, then for each case dirties it the same way a few
//! times and reports the frame resolve (`NodeTables::resolve_frame`): host
//! time, nodes resolved, and — with `--group-rows`, every leaf routed to a
//! 96 B row of a group table as the renderer routes its files — the rows the
//! renderer would then rebuild and how they would go up (`plan_upload`). A
//! ballpark instrument, not a gate: run by hand. (Until 2026-10-26 it timed
//! the GPU compute resolve this replaced.)
//!
//! Trees:
//! - `synthetic`: a binary directory tree 10 levels deep with 96 leaves under
//!   each bottom directory — 100,351 nodes, leaves at depth 11.
//! - `tree`: the TOPOLOGY of a real checkout, from a directory walk (no file
//!   is opened): one node per directory and per regular file, files as leaves
//!   under their directory, `.git` skipped. Pass the path with `--tree` or
//!   `GLYPH_BENCH_TREE`; nothing in the repo names where a checkout lives.
//!   With a Linux checkout, `--subtree drivers` is the worst realistic drag.
//!
//! ```sh
//! cargo run --release -p glyph-scene-graph --bin resolve-bench -- \
//!     [--tree <checkout>] [--subtree drivers] [--iters 5] [--group-rows]
//! ```

use std::path::{Path, PathBuf};
use std::time::Instant;

use glyph_scene_graph::upload::plan_upload;
use glyph_scene_graph::{Appearance, NodeHandle, NodeTables, Similarity};

struct Tree {
    name: String,
    root: NodeHandle,
    /// The case's subtree root ("one directory moved").
    subtree: NodeHandle,
    subtree_label: String,
    leaf: NodeHandle,
    /// A top-level directory outside `subtree` (the reparent case's target).
    other_dir: NodeHandle,
    nodes: Vec<NodeHandle>,
    leaves: u32,
    max_depth: u32,
}

fn synthetic(t: &mut NodeTables) -> Tree {
    let root = t.insert(None, Similarity::IDENTITY, Appearance::IDENTITY).unwrap();
    let mut nodes = vec![root];
    let mut level = vec![root];
    for _ in 0..10 {
        let mut next = Vec::with_capacity(level.len() * 2);
        for &p in &level {
            for _ in 0..2 {
                let h = t.insert(Some(p), Similarity::from_translation([1.0, 0.0, 0.0]), Appearance::IDENTITY).unwrap();
                next.push(h);
                nodes.push(h);
            }
        }
        level = next;
    }
    let mut leaves = 0;
    for &p in &level {
        for _ in 0..96 {
            nodes.push(t.insert(Some(p), Similarity::from_translation([0.0, 1.0, 0.0]), Appearance::IDENTITY).unwrap());
            leaves += 1;
        }
    }
    let subtree = t.children(root).unwrap()[0];
    let other_dir = t.children(t.children(root).unwrap()[1]).unwrap()[0];
    Tree {
        other_dir,
        name: "synthetic".into(),
        root,
        subtree,
        subtree_label: "one top-level half".into(),
        leaf: *nodes.last().unwrap(),
        nodes,
        leaves,
        max_depth: 11,
    }
}

fn walk(t: &mut NodeTables, path: &Path, subtree_name: &str) -> Tree {
    let root = t.insert(None, Similarity::IDENTITY, Appearance::IDENTITY).unwrap();
    let mut nodes = vec![root];
    let mut leaves = 0u32;
    let mut max_depth = 0u32;
    let mut subtree = None;
    let mut leaf = root;
    let mut stack: Vec<(PathBuf, NodeHandle, u32)> = vec![(path.to_path_buf(), root, 0)];
    while let Some((dir, node, depth)) = stack.pop() {
        let mut entries: Vec<_> = match std::fs::read_dir(&dir) {
            Ok(rd) => rd.filter_map(Result::ok).collect(),
            Err(e) => panic!("resolve-bench: cannot read {}: {e}", dir.display()),
        };
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if e.file_name() == ".git" {
                    continue;
                }
                let h = t.insert(Some(node), Similarity::from_translation([1.0, 0.0, 0.0]), Appearance::IDENTITY).unwrap();
                nodes.push(h);
                if depth == 0 && e.file_name() == subtree_name {
                    subtree = Some(h);
                }
                stack.push((e.path(), h, depth + 1));
            } else if ft.is_file() {
                leaf = t.insert(Some(node), Similarity::from_translation([0.0, 1.0, 0.0]), Appearance::IDENTITY).unwrap();
                nodes.push(leaf);
                leaves += 1;
                max_depth = max_depth.max(depth + 1);
            }
        }
    }
    let (subtree, subtree_label) = match subtree {
        Some(h) => (h, format!("{subtree_name}/")),
        None => (t.children(root).unwrap()[0], "first top-level dir".to_string()),
    };
    let other_dir = *t.children(root).unwrap().iter().find(|&&h| h != subtree && !t.children(h).unwrap().is_empty()).expect("a second directory");
    Tree { other_dir, name: path.file_name().map_or("tree".into(), |n| n.to_string_lossy().into_owned()), root, subtree, subtree_label, leaf, nodes, leaves, max_depth }
}

/// A timing over an empty or wrong resolve would read just as well: hold
/// every resolved world row to the chain walk, bit for bit.
fn verify(tree: &Tree, t: &NodeTables, case: &str) {
    for &h in &tree.nodes {
        let walked: [u32; 8] = bytemuck::cast(t.world(h).unwrap().0);
        let resolved: [u32; 8] = bytemuck::cast(t.resolved_world(h.index()).0);
        assert_eq!(walked, resolved, "resolve-bench: after {case:?} node {} resolved off its chain", h.index());
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn run(tree: &Tree, t: &mut NodeTables, iters: usize, group_rows: u32) {
    let (_, sub_len) = t.subtree_range(tree.subtree).unwrap();
    println!(
        "\n== {}: {} nodes ({} leaves, {} inner), leaf depth <= {}; {} = {} nodes",
        tree.name,
        tree.nodes.len(),
        tree.leaves,
        tree.nodes.len() as u32 - tree.leaves,
        tree.max_depth,
        tree.subtree_label,
        sub_len
    );
    let t0 = Instant::now();
    let p = t.resolve_frame();
    println!("initial resolve: {:.0} us, resolved {}", t0.elapsed().as_secs_f64() * 1e6, p.resolved);
    println!("{:<22} {:>11} {:>9} {:>9} {:>11}  upload", "case", "resolve us", "resolved", "rows", "bytes");
    type Dirty<'a> = Box<dyn Fn(&mut NodeTables, f32) + 'a>;
    let mv = |h: NodeHandle| -> Dirty<'_> {
        Box::new(move |t: &mut NodeTables, k: f32| {
            let l = t.local(h).unwrap();
            t.set_local(h, Similarity { translation: [l.translation[0] + k, l.translation[1], l.translation[2]], ..l }).unwrap();
        })
    };
    let cases: Vec<(&str, Dirty<'_>)> = vec![
        ("nothing dirty", Box::new(|_: &mut NodeTables, _| {})),
        ("one leaf moved", mv(tree.leaf)),
        ("subtree moved", mv(tree.subtree)),
        ("root moved", mv(tree.root)),
        (
            // A structural edit: the order is rebuilt inside the resolve.
            "subtree reparented",
            Box::new(|t: &mut NodeTables, k: f32| {
                let to = if (k as i32) % 2 == 0 { tree.root } else { tree.other_dir };
                t.reparent(tree.subtree, Some(to)).unwrap();
            }),
        ),
        (
            "everything moved",
            Box::new(|t: &mut NodeTables, k: f32| {
                for &h in &tree.nodes {
                    let l = t.local(h).unwrap();
                    t.set_local(h, Similarity { translation: [l.translation[0] + k * 1e-3, l.translation[1], l.translation[2]], ..l }).unwrap();
                }
            }),
        ),
    ];
    for (name, dirty) in &cases {
        let mut us = Vec::new();
        let mut last = Default::default();
        for i in 0..iters {
            dirty(t, 0.25 + i as f32);
            let t0 = Instant::now();
            last = t.resolve_frame();
            us.push(t0.elapsed().as_secs_f64() * 1e6);
        }
        // The group rows the renderer would rebuild from the resolved nodes.
        let mut rows: Vec<u32> = last.slots(t).filter_map(|s| t.slot_group_row(s)).collect();
        rows.sort_unstable();
        let up = plan_upload(&rows, group_rows);
        let label = if *name == "subtree moved" { format!("{} moved", tree.subtree_label) } else { name.to_string() };
        println!(
            "{:<22} {:>11.0} {:>9} {:>9} {:>11}  {}",
            label,
            median(us),
            last.resolved,
            rows.len(),
            up.rows(group_rows) * 96,
            up.label()
        );
        verify(tree, t, &label);
    }
    println!("verify: every case's world rows held to the chain walk, bit for bit, after its last resolve");
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut tree_path = std::env::var_os("GLYPH_BENCH_TREE").map(PathBuf::from);
    let mut subtree = "drivers".to_string();
    let mut iters = 5usize;
    let mut group_rows = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--tree" => tree_path = Some(PathBuf::from(args.next().expect("--tree <path>"))),
            "--subtree" => subtree = args.next().expect("--subtree <name>"),
            "--iters" => iters = args.next().and_then(|v| v.parse().ok()).expect("--iters <n>"),
            "--group-rows" => group_rows = true,
            other => panic!("resolve-bench: unknown argument {other:?} (see the module header)"),
        }
    }
    println!("iters {iters}, group rows {group_rows}");

    let mut trees: Vec<(Tree, NodeTables)> = Vec::new();
    let mut t = NodeTables::default();
    let tree = synthetic(&mut t);
    trees.push((tree, t));
    if let Some(p) = &tree_path {
        let mut t = NodeTables::default();
        let t0 = Instant::now();
        let tree = walk(&mut t, p, &subtree);
        println!("walked {} in {:.0} ms", tree.name, t0.elapsed().as_secs_f64() * 1e3);
        trees.push((tree, t));
    } else {
        println!("(no --tree / GLYPH_BENCH_TREE: synthetic tree only)");
    }
    for (tree, t) in &mut trees {
        let mut rows = 0u32;
        if group_rows {
            let leaves: Vec<NodeHandle> = tree.nodes.iter().copied().filter(|&h| t.children(h).unwrap().is_empty()).collect();
            for (i, &h) in leaves.iter().enumerate() {
                t.set_group_row(h, Some(i as u32)).unwrap();
            }
            rows = leaves.len() as u32;
        }
        run(tree, t, iters, rows);
    }
}
