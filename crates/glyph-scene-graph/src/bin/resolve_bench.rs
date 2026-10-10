//! resolve-bench: what the scene graph's per-frame flush costs on this GPU.
//!
//! Builds a tree once, then for each case dirties it the same way a few
//! times and reports the flush: GPU time of the pass (scatter + resolve,
//! timestamps at the pass boundaries, when the adapter has TIMESTAMP_QUERY),
//! host time to close the plan and encode, bytes queued, nodes resolved, and
//! how each table went up. A ballpark instrument, not a gate: run by hand.
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
//! `--group-rows` gives every leaf an output row in a 96 B group table, the
//! transitional form the renderer runs.

use std::path::{Path, PathBuf};
use std::time::Instant;

use glyph_scene_graph::{Appearance, FlushStats, NodeHandle, SceneGraph, Similarity};

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

fn synthetic(g: &mut SceneGraph) -> Tree {
    let root = g.tables.insert(None, Similarity::IDENTITY, Appearance::IDENTITY).unwrap();
    let mut nodes = vec![root];
    let mut level = vec![root];
    for _ in 0..10 {
        let mut next = Vec::with_capacity(level.len() * 2);
        for &p in &level {
            for _ in 0..2 {
                let h = g.tables.insert(Some(p), Similarity::from_translation([1.0, 0.0, 0.0]), Appearance::IDENTITY).unwrap();
                next.push(h);
                nodes.push(h);
            }
        }
        level = next;
    }
    let mut leaves = 0;
    for &p in &level {
        for _ in 0..96 {
            nodes.push(g.tables.insert(Some(p), Similarity::from_translation([0.0, 1.0, 0.0]), Appearance::IDENTITY).unwrap());
            leaves += 1;
        }
    }
    let subtree = g.tables.children(root).unwrap()[0];
    let other_dir = g.tables.children(g.tables.children(root).unwrap()[1]).unwrap()[0];
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

fn walk(g: &mut SceneGraph, path: &Path, subtree_name: &str) -> Tree {
    let root = g.tables.insert(None, Similarity::IDENTITY, Appearance::IDENTITY).unwrap();
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
                let h = g.tables.insert(Some(node), Similarity::from_translation([1.0, 0.0, 0.0]), Appearance::IDENTITY).unwrap();
                nodes.push(h);
                if depth == 0 && e.file_name() == subtree_name {
                    subtree = Some(h);
                }
                stack.push((e.path(), h, depth + 1));
            } else if ft.is_file() {
                leaf = g.tables.insert(Some(node), Similarity::from_translation([0.0, 1.0, 0.0]), Appearance::IDENTITY).unwrap();
                nodes.push(leaf);
                leaves += 1;
                max_depth = max_depth.max(depth + 1);
            }
        }
    }
    let (subtree, subtree_label) = match subtree {
        Some(h) => (h, format!("{subtree_name}/")),
        None => (g.tables.children(root).unwrap()[0], "first top-level dir".to_string()),
    };
    let other_dir = *g.tables.children(root).unwrap().iter().find(|&&h| h != subtree && !g.tables.children(h).unwrap().is_empty()).expect("a second directory");
    Tree { other_dir, name: path.file_name().map_or("tree".into(), |n| n.to_string_lossy().into_owned()), root, subtree, subtree_label, leaf, nodes, leaves, max_depth }
}

fn flush(g: &mut SceneGraph, device: &wgpu::Device, queue: &wgpu::Queue) -> (FlushStats, Option<f64>) {
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("bench flush") });
    let stats = g.flush(device, queue, &mut encoder);
    queue.submit([encoder.finish()]);
    let gpu = g.gpu.read_gpu_ms(device);
    device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("bench: poll");
    (stats, gpu)
}

/// A timing over an empty or wrong dispatch would read just as well: hold
/// every world row to the CPU reference.
fn verify(tree: &Tree, g: &SceneGraph, device: &wgpu::Device, queue: &wgpu::Queue, case: &str) -> f32 {
    let worlds = g.gpu.read_worlds(device, queue, g.tables.slot_count());
    let mut worst = 0.0f32;
    for &h in &tree.nodes {
        let (cpu, _) = g.tables.world(h).unwrap();
        let gpu = worlds[h.index() as usize];
        for k in 0..3 {
            worst = worst.max((gpu.translation[k] - cpu.translation[k]).abs() / cpu.translation[k].abs().max(1.0));
        }
    }
    assert!(worst < 1e-4, "resolve-bench: after {case:?} the GPU world rows disagree with the CPU reference ({worst:e})");
    worst
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn run(tree: &Tree, g: &mut SceneGraph, device: &wgpu::Device, queue: &wgpu::Queue, iters: usize) {
    let (sub_start, sub_len) = g.tables.subtree_range(tree.subtree).unwrap();
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
    let _ = sub_start;
    let t = Instant::now();
    let (s, gpu) = flush(g, device, queue);
    println!(
        "initial upload + resolve: gpu {} ms, host {:.0} us, {} B, resolved {}",
        gpu.map_or("n/a".into(), |m| format!("{m:.3}")),
        t.elapsed().as_secs_f64() * 1e6,
        s.bytes,
        s.resolved
    );
    println!("{:<22} {:>9} {:>9} {:>9} {:>11} {:>9}  uploads (local/post/appearance/topo)", "case", "gpu ms", "plan us", "enc us", "bytes", "resolved");
    type Dirty<'a> = Box<dyn Fn(&mut SceneGraph, f32) + 'a>;
    let mv = |h: NodeHandle| -> Dirty<'_> {
        Box::new(move |g: &mut SceneGraph, k: f32| {
            let l = g.tables.local(h).unwrap();
            g.tables.set_local(h, Similarity { translation: [l.translation[0] + k, l.translation[1], l.translation[2]], ..l }).unwrap();
        })
    };
    let cases: Vec<(&str, Dirty<'_>)> = vec![
        ("nothing dirty", Box::new(|_: &mut SceneGraph, _| {})),
        ("one leaf moved", mv(tree.leaf)),
        ("subtree moved", mv(tree.subtree)),
        ("root moved", mv(tree.root)),
        (
            // A structural edit: the order is rebuilt (plan us) and the
            // changed span of it re-uploaded.
            "subtree reparented",
            Box::new(|g: &mut SceneGraph, k: f32| {
                let to = if (k as i32) % 2 == 0 { tree.root } else { tree.other_dir };
                g.tables.reparent(tree.subtree, Some(to)).unwrap();
            }),
        ),
        (
            "everything moved",
            Box::new(|g: &mut SceneGraph, k: f32| {
                for &h in &tree.nodes {
                    let l = g.tables.local(h).unwrap();
                    g.tables.set_local(h, Similarity { translation: [l.translation[0] + k * 1e-3, l.translation[1], l.translation[2]], ..l }).unwrap();
                }
            }),
        ),
    ];
    let mut worst_all = 0.0f32;
    for (name, dirty) in &cases {
        let (mut gpu, mut plan, mut enc) = (Vec::new(), Vec::new(), Vec::new());
        let mut last = FlushStats::default();
        for i in 0..iters {
            dirty(g, 0.25 + i as f32);
            let (s, ms) = flush(g, device, queue);
            gpu.extend(ms);
            plan.push(s.cpu_plan_us);
            enc.push(s.cpu_encode_us);
            last = s;
        }
        let ups: Vec<String> = last.uploads.iter().map(|u| format!("{}:{}", u.1, u.2)).collect();
        let label = if *name == "subtree moved" { format!("{} moved", tree.subtree_label) } else { name.to_string() };
        println!(
            "{:<22} {:>9} {:>9.0} {:>9.0} {:>11} {:>9}  {}",
            label,
            if gpu.is_empty() { "-".into() } else { format!("{:.3}", median(gpu)) },
            median(plan),
            median(enc),
            last.bytes,
            last.resolved,
            ups.join(" ")
        );
        worst_all = worst_all.max(verify(tree, g, device, queue, &label));
    }
    println!("verify: every case's world rows held to the CPU reference after its last flush (worst relative translation error {:.2e})", worst_all);
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
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
        apply_limit_buckets: false,
    }))
    .expect("resolve-bench: no adapter");
    let mut features = wgpu::Features::empty();
    if adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
        features |= wgpu::Features::TIMESTAMP_QUERY;
    }
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("resolve-bench"),
        required_features: features,
        required_limits: adapter.limits(),
        experimental_features: Default::default(),
        memory_hints: Default::default(),
        trace: wgpu::Trace::Off,
    }))
    .expect("resolve-bench: device");
    let info = adapter.get_info();
    println!("adapter: {} ({:?}, {}), iters {iters}, group rows {group_rows}", info.name, info.backend, info.driver);

    let mut trees: Vec<(Tree, SceneGraph)> = Vec::new();
    let mut g = SceneGraph::new(&device);
    let t = synthetic(&mut g);
    trees.push((t, g));
    if let Some(p) = &tree_path {
        let mut g = SceneGraph::new(&device);
        let t0 = Instant::now();
        let t = walk(&mut g, p, &subtree);
        println!("walked {} in {:.0} ms", t.name, t0.elapsed().as_secs_f64() * 1e3);
        trees.push((t, g));
    } else {
        println!("(no --tree / GLYPH_BENCH_TREE: synthetic tree only)");
    }
    for (tree, g) in &mut trees {
        if !g.gpu.enable_timing(&device, &queue) {
            println!("(no TIMESTAMP_QUERY on this adapter: gpu ms not reported)");
        }
        if group_rows {
            let leaves: Vec<NodeHandle> = tree.nodes.iter().copied().filter(|&h| g.tables.children(h).unwrap().is_empty()).collect();
            let buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("bench group table"),
                size: (leaves.len().max(1) * 96) as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            for (i, &h) in leaves.iter().enumerate() {
                g.tables.set_group_row(h, Some(i as u32)).unwrap();
            }
            g.gpu.set_group_output(&device, &buf, leaves.len() as u32);
        }
        run(tree, g, &device, &queue, iters);
    }
}
