//! The resolve pass on a real adapter, against the CPU reference
//! (`NodeTables::world`, the same composition in the same order).
//!
//! 1. A random tree (5,000 nodes, depth up to 9, rotations, scales, alphas)
//!    through every upload path — the first full upload, a few runs, a
//!    scatter batch, a past-15 % full table — plus a reparent and a free with
//!    slot reuse; after each flush EVERY world row is compared, so a row the
//!    plan wrongly skipped is caught as surely as a wrong one.
//! 2. The transitional output: group rows under an identity root come back
//!    bit for bit (columns 4 and 5 and col 0's w untouched), and a move, a
//!    tint, a hide and a per-axis scale land in exactly the columns they own.
//!
//! The GPU may fuse multiply-adds the CPU rounds separately, so (1) holds to
//! a tolerance; (2) is exact because every step under an identity parent
//! returns its operand.

use glyph_scene_graph::gpu::read_buffer_words;
use glyph_scene_graph::{Appearance, NodeHandle, SceneGraph, Similarity};

fn device() -> (wgpu::Device, wgpu::Queue) {
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
    .expect("an adapter (this test needs a GPU)");
    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("glyph-scene-graph test device"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        experimental_features: Default::default(),
        memory_hints: Default::default(),
        trace: wgpu::Trace::Off,
    }))
    .expect("device")
}

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

fn flush(g: &mut SceneGraph, device: &wgpu::Device, queue: &wgpu::Queue) -> glyph_scene_graph::FlushStats {
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("test flush") });
    let stats = g.flush(device, queue, &mut encoder);
    queue.submit([encoder.finish()]);
    stats
}

/// Every live node's GPU world against the CPU reference.
fn assert_worlds(g: &SceneGraph, nodes: &[NodeHandle], device: &wgpu::Device, queue: &wgpu::Queue, what: &str) {
    let worlds = g.gpu.read_worlds(device, queue, g.tables.slot_count());
    let mut worst = 0.0f32;
    for &h in nodes.iter().filter(|h| g.tables.is_live(**h)) {
        let (cpu, _) = g.tables.world(h).unwrap();
        let gpu = worlds[h.index() as usize];
        let mag = cpu.translation.iter().fold(1.0f32, |m, v| m.max(v.abs()));
        for k in 0..3 {
            let e = (gpu.translation[k] - cpu.translation[k]).abs() / mag;
            worst = worst.max(e);
            assert!(e < 1e-4, "{what}: node {} t{k}: gpu {:?} cpu {:?}", h.index(), gpu, cpu);
        }
        for k in 0..4 {
            assert!((gpu.rotation[k] - cpu.rotation[k]).abs() < 1e-4, "{what}: node {} q{k}: gpu {gpu:?} cpu {cpu:?}", h.index());
        }
        assert!((gpu.scale - cpu.scale).abs() <= 1e-5 * cpu.scale.abs().max(1.0), "{what}: node {} scale", h.index());
    }
    assert!(worst.is_finite(), "{what}: compared something");
}

#[test]
fn the_resolve_matches_the_cpu_reference_through_every_upload_path() {
    let (device, queue) = device();
    let mut g = SceneGraph::new(&device);
    let mut r = Lcg(0x5EED);
    let root = g.tables.insert(None, random_local(&mut r), Appearance::IDENTITY).unwrap();
    let mut nodes = vec![root];
    let mut depth = vec![0u32];
    while nodes.len() < 5_000 {
        // Prefer recent nodes as parents so chains get deep (up to 9).
        let k = nodes.len() - 1 - (r.next() as usize % nodes.len().min(40));
        let k = if depth[k] >= 9 { 0 } else { k };
        let a = Appearance { tint: [r.unit(), r.unit(), r.unit(), r.range(0.5, 1.0)], ..Appearance::IDENTITY };
        nodes.push(g.tables.insert(Some(nodes[k]), random_local(&mut r), a).unwrap());
        depth.push(depth[k] + 1);
    }
    assert!(depth.contains(&9), "the tree reaches depth 9");

    let s = flush(&mut g, &device, &queue);
    assert!(s.grew && s.dispatched && s.resolved == 5_000, "first flush resolves everything: {s:?}");
    assert_worlds(&g, &nodes, &device, &queue, "initial");

    let s = flush(&mut g, &device, &queue);
    assert!(!s.dispatched && s.bytes == 0, "nothing dirty, nothing dispatched or uploaded: {s:?}");

    // A few nodes: runs.
    for k in [17usize, 18, 400] {
        g.tables.set_local(nodes[k], random_local(&mut r)).unwrap();
    }
    let s = flush(&mut g, &device, &queue);
    assert_eq!(s.uploads[0].1, "runs", "{s:?}");
    assert_worlds(&g, &nodes, &device, &queue, "runs");

    // Scattered nodes past the run limit: scatter.
    for k in (0..nodes.len()).step_by(97) {
        g.tables.set_local(nodes[k], random_local(&mut r)).unwrap();
    }
    let s = flush(&mut g, &device, &queue);
    assert_eq!(s.uploads[0].1, "scatter", "{s:?}");
    assert_worlds(&g, &nodes, &device, &queue, "scatter");

    // Appearance alone: the local table stays clean.
    g.tables.set_appearance(nodes[3], Appearance { tint: [0.1, 0.2, 0.3, 0.25], ..Appearance::IDENTITY }).unwrap();
    let s = flush(&mut g, &device, &queue);
    assert_eq!((s.uploads[0].1, s.uploads[2].1), ("none", "runs"), "{s:?}");

    // Past 15 %: the whole table.
    for k in (0..nodes.len()).step_by(5) {
        g.tables.set_local(nodes[k], random_local(&mut r)).unwrap();
    }
    let s = flush(&mut g, &device, &queue);
    assert_eq!(s.uploads[0].1, "full", "{s:?}");
    assert_worlds(&g, &nodes, &device, &queue, "full");

    // Reparent a deep node's subtree under another branch.
    let moved = nodes[2_500];
    g.tables.reparent(moved, Some(nodes[10])).unwrap();
    g.tables.assert_invariants();
    flush(&mut g, &device, &queue);
    assert_worlds(&g, &nodes, &device, &queue, "reparent");

    // Free a subtree, let the quarantine run out, reuse its slots.
    let gone = nodes[1_000];
    let freed = g.tables.remove(gone).unwrap();
    for _ in 0..4 {
        flush(&mut g, &device, &queue);
    }
    for _ in 0..freed {
        let parent = nodes[r.next() as usize % 50];
        let parent = if g.tables.is_live(parent) { parent } else { root };
        nodes.push(g.tables.insert(Some(parent), random_local(&mut r), Appearance::IDENTITY).unwrap());
    }
    assert!(nodes[nodes.len() - freed..].iter().any(|h| h.index() == gone.index()), "a freed slot was reused");
    flush(&mut g, &device, &queue);
    assert_worlds(&g, &nodes, &device, &queue, "reuse");
}

/// The renderer's group row, as plain floats (the crate does not depend on
/// glyph-field): offset+w, quat, tint+alpha, scale+blend, clip, background.
type Row = [[f32; 4]; 6];

#[test]
fn group_rows_under_an_identity_root_come_back_bit_for_bit() {
    let (device, queue) = device();
    let mut g = SceneGraph::new(&device);
    let mut r = Lcg(0xC0FFEE);
    let rows: Vec<Row> = (0..300)
        .map(|i| {
            let l = random_local(&mut r);
            let uniform = i % 3 != 0;
            let s = if uniform { [l.scale; 3] } else { [r.range(0.1, 3.0), r.range(0.1, 3.0), 1.0] };
            [
                [l.translation[0], l.translation[1], l.translation[2], 0.0],
                l.rotation,
                [r.unit(), r.unit(), r.unit(), if i % 7 == 0 { 0.0 } else { 1.0 }],
                [s[0], s[1], s[2], if i % 5 == 0 { 1.0 } else { 0.0 }],
                [r.range(-9.0, 9.0), r.range(-9.0, 9.0), 1.0, r.unit()],
                [r.unit(), r.unit(), r.unit(), r.unit()],
            ]
        })
        .collect();
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test group table"),
        size: (rows.len() * 96) as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    queue.write_buffer(&buf, 0, bytemuck::cast_slice(&rows));
    g.gpu.set_group_output(&device, &buf, rows.len() as u32);

    let root = g.tables.insert(None, Similarity::IDENTITY, Appearance::IDENTITY).unwrap();
    let nodes: Vec<NodeHandle> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let sx = row[3];
            let uniform = sx[0].to_bits() == sx[1].to_bits() && sx[1].to_bits() == sx[2].to_bits();
            let local = Similarity { translation: [row[0][0], row[0][1], row[0][2]], scale: if uniform { sx[0] } else { 1.0 }, rotation: row[1] };
            let h = g.tables.insert(Some(root), local, Appearance { tint: row[2], blend: row[3][3], ..Appearance::IDENTITY }).unwrap();
            if !uniform {
                g.tables.set_post_scale(h, [sx[0], sx[1], sx[2]]).unwrap();
            }
            g.tables.set_group_row(h, Some(i as u32)).unwrap();
            h
        })
        .collect();
    flush(&mut g, &device, &queue);
    let back: Vec<Row> = bytemuck::cast_slice(&read_buffer_words(&device, &queue, &buf, (rows.len() * 96) as u64)).to_vec();
    for (i, (a, b)) in rows.iter().zip(&back).enumerate() {
        let (a, b): (&[u32; 24], &[u32; 24]) = (bytemuck::cast_ref(a), bytemuck::cast_ref(b));
        assert_eq!(a, b, "row {i} must round-trip bit for bit");
    }

    // Each writer lands in its own columns, exactly.
    let mut want = rows.clone();
    let l = g.tables.local(nodes[4]).unwrap();
    g.tables.set_local(nodes[4], Similarity { translation: [l.translation[0] + 12.5, l.translation[1], l.translation[2] - 3.0], ..l }).unwrap();
    want[4][0][0] += 12.5;
    want[4][0][2] -= 3.0;
    let a = g.tables.appearance(nodes[8]).unwrap();
    g.tables.set_appearance(nodes[8], Appearance { tint: [0.25, 0.5, 0.75, a.tint[3]], ..a }).unwrap();
    want[8][2] = [0.25, 0.5, 0.75, rows[8][2][3]];
    let a = g.tables.appearance(nodes[9]).unwrap();
    g.tables.set_appearance(nodes[9], Appearance { tint: [a.tint[0], a.tint[1], a.tint[2], 0.0], ..a }).unwrap();
    want[9][2][3] = 0.0;
    g.tables.set_post_scale(nodes[12], [2.0, 0.5, 1.0]).unwrap();
    let s12 = g.tables.local(nodes[12]).unwrap().scale;
    want[12][3] = [2.0 * s12, 0.5 * s12, s12, rows[12][3][3]];
    flush(&mut g, &device, &queue);
    let back: Vec<Row> = bytemuck::cast_slice(&read_buffer_words(&device, &queue, &buf, (rows.len() * 96) as u64)).to_vec();
    for (i, (a, b)) in want.iter().zip(&back).enumerate() {
        let (a, b): (&[u32; 24], &[u32; 24]) = (bytemuck::cast_ref(a), bytemuck::cast_ref(b));
        assert_eq!(a, b, "row {i} after the edits");
    }
}
