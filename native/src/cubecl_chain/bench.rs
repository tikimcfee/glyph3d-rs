use std::path::Path;

use cubecl::prelude::*;
use cubecl::wgpu::{AutoGraphicsApi, GraphicsApi, WgpuSetup};

use crate::fold::WrapMode;
use crate::gpu::GpuContext;
use crate::scan::{DEFAULT_CHUNK_SIZE, DEFAULT_GROUP_SIZE, run_scan_pipeline};
use crate::text::ResolveGlyph;

use super::cluster::{
    cand_scatter, cluster_host_inputs, cluster_mark, cluster_pair_filter, cluster_probe,
    count_spine, count_tile, item_roots, jump_build, rank_step,
};
use super::decode::decode;
use super::position::{derive_stride, extent_pair, paginate, resolve_x};
use super::scan::{apply, spine_scan, tile_scan};
use super::{
    F_LEADER, IE_STRIDE, IM_STRIDE, LC_COL, LC_ROW, LC_STRIDE, LM_STRIDE, PARTIAL_COUNT_STRIDE,
    pack_words,
};

// ── the bench driver ──────────────────────────────────────────────────────────

/// `--cubecl-chain-bench <corpus>`: the chain over a raw file as ONE item.
///
/// Timing is PER-DISPATCH GPU WINDOWS (`profile_start`/`profile_end` — device
/// timestamps when the shared device carries TIMESTAMP_QUERY, which gpu.rs
/// requests unconditionally where supported), `GLYPH_CHAIN_LOOP` samples per
/// dispatch with the MINIMUM kept (the "run it a few times" rule, automated).
/// Each window flushes, so stages cannot overlap: these are per-dispatch
/// latencies in the same posture as the Mojo bench's `mark()` table, and the
/// sum of minima is the chain estimate — the chain is dependency-serialized,
/// so nothing is lost to that. The old batched wall-clock loop mode is
/// retired: it measured repeat-overlap, not the chain.
///
/// `GLYPH_CHAIN_WRAP=<width>` swaps the single item to wrap_width>0 /
/// WRAP_DOWN so the segment re-sum in resolve_x actually executes — the plain
/// shape leaves that path dead at fold==0 and measures only atomic
/// throughput.
///
/// `GLYPH_CHAIN_CLUSTER=1` (implies `GLYPH_CHAIN_DECODE=1`): the bench item
/// flips to cluster mode and the ranked chain runs as stages 1-4 between
/// decode and tile_scan — probe, compact, rank (jump graph + pointer
/// doubling), mark; the pass order of `--cubecl-cluster-check`. A 4 B
/// setup-time readback of the candidate count sizes the level tables,
/// outside every timing window. Flags + advance are diffed bit-exact
/// against the cluster-resolved reference whenever the mark stage ran.
pub fn bench(ctx: &GpuContext, corpus_path: &Path) -> ! {
    let bytes = std::fs::read(corpus_path).unwrap_or_else(|e| {
        eprintln!("cubecl-chain-bench: {e}");
        std::process::exit(1);
    });
    let n = bytes.len();
    // The tile shape (the same dials as the check instrument, so a sweep
    // measures exactly what the fixtures verify).
    let units: usize = std::env::var("GLYPH_CHAIN_TILE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256);
    let rake: usize = std::env::var("GLYPH_CHAIN_RAKE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    assert!(units.is_power_of_two(), "GLYPH_CHAIN_TILE must be a power of two");
    let log = units.ilog2() as usize;
    let n_tiles = n.div_ceil(units * rake).max(1);
    // resolve_x worker span (bytes per worker).
    let rspan: usize = std::env::var("GLYPH_CHAIN_SPAN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    // The wrapped shape: fold>0 makes resolve_x take the re-sum path.
    let wrap_width: i64 = std::env::var("GLYPH_CHAIN_WRAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // GLYPH_CHAIN_CLUSTER=1: cluster mode on top of decode (it implies
    // GLYPH_CHAIN_DECODE — the cluster stages rewrite the fl/sm the decode
    // produces). The bench item flips to Cluster so the CPU reference resolves
    // clusters too, and its lanes witness the device pass at speed.
    let cluster_mode = std::env::var_os("GLYPH_CHAIN_CLUSTER").is_some();
    let item = crate::fold::Item {
        byte_start: 0,
        byte_count: n as i64,
        origin_x: 0.0,
        origin_y: 0.0,
        origin_z: 0.0,
        wrap_width,
        wrap_mode: WrapMode::Down,
        cluster_mode: if cluster_mode {
            crate::fold::ClusterMode::Cluster
        } else {
            crate::fold::ClusterMode::Leader
        },
        z_step: 2.0,
        line_height: 1.25,
        has_page: false,
        page_rows: 0,
        page_cols: 0,
        scroll_rows: 0,
        pages_wide: 1,
        page_gap_x: 0.0,
        band_stride_y: 0.0,
        depth_per_band: 0.0,
        depth_per_col: 0.0,
        page_line_height: 1.25,
    };
    let items = [item];
    let trie = crate::atlas::TrieTable::load(&crate::atlas_dir());
    // The bench item folds iff GLYPH_CHAIN_WRAP is set — foldless runs skip
    // the resolve_x dispatch entirely (apply resolves them), and pure-wrapped
    // runs compile apply's inline branch out.
    let needs_resolve = wrap_width > 0;
    let inline_resolve = wrap_width == 0;
    // GLYPH_CHAIN_DECODE=1: the chain starts from raw BYTES — the device
    // decode produces fl/sm (phase 3a) and the CPU statics upload dies; the
    // CPU reference still runs for verification. Decode is stage 1 then, and
    // GLYPH_CHAIN_STAGES counts it. GLYPH_CHAIN_CLUSTER implies it.
    let decode_mode = cluster_mode || std::env::var_os("GLYPH_CHAIN_DECODE").is_some();

    let t_decode = std::time::Instant::now();
    let r = run_scan_pipeline(&bytes, &trie, &items, DEFAULT_CHUNK_SIZE, DEFAULT_GROUP_SIZE, 1);
    let decode_dt = t_decode.elapsed();

    let ir: Vec<u32> = vec![0, n as u32];
    let walk_plan: Vec<u32> = vec![0, n as u32, wrap_width.max(0) as u32];
    let ie: Vec<u32> = vec![0, 0, 0, 1, wrap_width as u32, 0, 0, 0];
    let im: Vec<f32> = vec![0.0, 0.0, 1.25, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let page_gap_x: Vec<f32> = vec![0.0];

    let setup = WgpuSetup {
        instance: ctx.instance.clone(),
        adapter: ctx.adapter.clone(),
        device: ctx.device.clone(),
        queue: ctx.queue.clone(),
        backend: AutoGraphicsApi::backend(),
    };
    let cdev = cubecl::wgpu::init_device(setup, Default::default());
    let client = cubecl::Device::Wgpu(cdev).client();
    let n_words = n.div_ceil(4);
    let (h_fl, h_sm, h_gi, h_hgt) = if decode_mode {
        (
            client.empty(n_words * 4),
            client.empty(n * 4),
            client.empty(n * 4),
            client.empty(n * 4),
        )
    } else {
        // Statics: advance (f32/byte) + PACKED flags (u8/byte, four per word —
        // the chain reads fl three-to-four passes and consumes only the low
        // byte; the full flags stay CPU-side for the renderer).
        let mut fl = Vec::with_capacity(n_words);
        let mut sm = Vec::with_capacity(n);
        for w in 0..n_words {
            let mut word = 0u32;
            for b in 0..4 {
                let i = w * 4 + b;
                if i < n {
                    word |= (r.slots.flags(i) & 0xFF) << (b * 8);
                }
            }
            fl.push(word);
        }
        for i in 0..n {
            sm.push(r.slots.advance(i));
        }
        // gi/height ride only the decode path (rung 1); the CPU-statics
        // upload mode never reads them.
        let mut giv = vec![0u32; n];
        let mut hgv = vec![0f32; n];
        for i in 0..n {
            giv[i] = r.slots.gi[i];
            hgv[i] = r.slots.height(i);
        }
        (
            client.create_from_slice(bytemuck::cast_slice(&fl)),
            client.create_from_slice(bytemuck::cast_slice(&sm)),
            client.create_from_slice(bytemuck::cast_slice(&giv)),
            client.create_from_slice(bytemuck::cast_slice(&hgv)),
        )
    };
    // The decode's inputs: the corpus packed four bytes per word (tail lanes
    // 0x80 — see pack_words), and the atlas trie's tables pre-converted to
    // world units.
    let packed = pack_words(&bytes);
    let (bi, bm, bc, bshift) = trie.device_tables();
    let h_bytes = client.create_from_slice(bytemuck::cast_slice(&packed));
    let h_bi = client.create_from_slice(bytemuck::cast_slice(&bi));
    let h_bm = client.create_from_slice(bytemuck::cast_slice(&bm));
    let h_bc = client.create_from_slice(bytemuck::cast_slice(&bc));
    // The cluster stages' inputs: the sequence section, the host-built
    // candidacy bitmap + per-item mode flags (cluster_host_inputs), and the
    // probe's scratch. The buffers exist in every mode — the cluster launches
    // are their only readers — but cslot's zero-fill is gated: that one is a
    // full-corpus upload the non-cluster runs must not pay.
    let (seq, seq_max, bitmap_advance) = match trie.cluster_table() {
        Some((s, m, a)) => (s.to_vec(), m, a),
        None if cluster_mode => {
            eprintln!(
                "cubecl-chain-bench: this atlas carries no sequence section; GLYPH_CHAIN_CLUSTER needs the v2 trie"
            );
            std::process::exit(1);
        }
        None => (Vec::new(), 2u32, f32::NAN),
    };
    assert!(
        seq_max <= 64,
        "seq_max {seq_max} would exceed the local key-scratch budget"
    );
    let (bitmap, ic) = cluster_host_inputs(&seq, seq_max, &items);
    let h_seq = client.create_from_slice(bytemuck::cast_slice(&seq));
    let h_bmap = client.create_from_slice(bytemuck::cast_slice(&bitmap));
    // The pair filter computes unconditionally (host-side, trivial); only
    // its ~4.4MB upload is gated on cluster mode.
    let (poff, pval) = cluster_pair_filter(&seq, seq_max);
    let (h_poff, h_pval) = if cluster_mode {
        (
            client.create_from_slice(bytemuck::cast_slice(&poff)),
            client.create_from_slice(bytemuck::cast_slice(&pval)),
        )
    } else {
        (client.empty(0), client.empty(0))
    };
    let h_ic = client.create_from_slice(bytemuck::cast_slice(&ic));
    // The probe writes cslot only on its candidate path and count_tile reads
    // it as a per-byte predicate — but the zero-fill rides decode's per-byte
    // store now (2026-09-30), so this is a plain allocation in every mode.
    let h_cslot = client.empty(n * 4);
    let h_cend = client.empty(n * 4);
    let h_ir = client.create_from_slice(bytemuck::cast_slice(&ir));
    let h_ie = client.create_from_slice(bytemuck::cast_slice(&ie));
    let h_im = client.create_from_slice(bytemuck::cast_slice(&im));
    let h_gap = client.create_from_slice(bytemuck::cast_slice(&page_gap_x));
    let h_tc = client.empty(n_tiles * PARTIAL_COUNT_STRIDE * 4);
    let h_tm = client.empty(n_tiles * 4);
    let h_xc = client.empty(n_tiles * PARTIAL_COUNT_STRIDE * 4);
    let h_xm = client.empty(n_tiles * 4);
    let h_lc = client.empty(n * LC_STRIDE * 4);
    let h_wm = client.empty(n * 4);
    let h_wc = client.empty(n * 4);
    let h_otb = client.empty(n * 4);
    let h_lm = client.empty(n * LM_STRIDE * 4);
    let h_strides = client.empty(8);
    let h_rmax = client.create_from_slice(bytemuck::cast_slice(&[0u32]));
    let h_xmax = client.create_from_slice(bytemuck::cast_slice(&[0u32]));
    // The extent pair + walk plan for the single bench item.
    let h_extent =
        client.create_from_slice(bytemuck::cast_slice(&[0x8000_0000u32, 0x8000_0000u32]));
    let h_plan = client.create_from_slice(bytemuck::cast_slice(&walk_plan));

    // This M2's ADAPTER caps workgroups per grid dimension at 65535 (verified
    // live: a 94075-cube dispatch was rejected) — not a wgpu default to lift.
    // Spill into Y; ABSOLUTE_POS is the flattened id across axes, so the
    // kernels need no index change. gpu.rs still requests the adapter's value,
    // so an adapter with a higher cap takes the plain grid automatically.
    let cubes_of = |threads: usize| {
        let cubes = threads.div_ceil(256);
        CubeCount::Static(cubes.min(65535) as u32, cubes.div_ceil(65535) as u32, 1)
    };
    // GLYPH_CHAIN_STAGES is an ABSOLUTE dispatch count (the check driver's
    // semantics): with GLYPH_CHAIN_DECODE=1 it counts the decode stage, and
    // with GLYPH_CHAIN_CLUSTER=1 (decode + the four ranked-chain stages) it
    // counts all five prefixes.
    let pre = decode_mode as usize + 4 * cluster_mode as usize;
    let stages: usize = match std::env::var("GLYPH_CHAIN_STAGES").ok().and_then(|v| v.parse().ok()) {
        Some(v) => v,
        None => 6 + pre,
    };
    // One cube per tile (dim = units); the byte-wide kernels stay 256-unit.
    let tiles_grid = |tiles: usize| {
        CubeCount::Static(tiles.min(65535) as u32, tiles.div_ceil(65535) as u32, 1)
    };
    // The ranked chain's C-dependent state: run decode + probe + the
    // compaction counts ONCE here — outside every timing window — and read
    // back the candidate count (4 B) to size the level tables. The timed
    // loop re-derives C on device each sample; the setup value only sizes
    // buffers and the host-side rank-round count. C is deterministic in
    // the corpus, so setup's C equals every timed sample's C.
    let h_ctc = client.empty(n_tiles * 4);
    let h_cup = client.empty(n_tiles * units * 4);
    let h_cxc = client.empty(n_tiles * 4);
    let h_ctotal = client.empty(4);
    let (h_hp, h_lvl, h_parent, h_parent_b, h_d0, h_d_a, h_d_b, h_roots, mut rank_out, c_host) =
        if cluster_mode {
            unsafe {
                decode::launch_unchecked(
                    &client,
                    cubes_of(n_words),
                    CubeDim::new_1d(256),
                    BufferArg::from_raw_parts(h_bytes.clone(), n_words),
                    BufferArg::from_raw_parts(h_bi.clone(), bi.len()),
                    BufferArg::from_raw_parts(h_bm.clone(), bm.len()),
                    BufferArg::from_raw_parts(h_bc.clone(), bc.len()),
                    BufferArg::from_raw_parts(h_fl.clone(), n_words),
                    BufferArg::from_raw_parts(h_sm.clone(), n),
                    BufferArg::from_raw_parts(h_gi.clone(), n),
                    BufferArg::from_raw_parts(h_hgt.clone(), n),
                    BufferArg::from_raw_parts(h_cslot.clone(), n),
                    bshift,
                );
                cluster_probe::launch_unchecked(
                    &client,
                    cubes_of(n_words),
                    CubeDim::new_1d(256),
                    BufferArg::from_raw_parts(h_bytes.clone(), n_words),
                    BufferArg::from_raw_parts(h_bmap.clone(), bitmap.len()),
                    BufferArg::from_raw_parts(h_poff.clone(), poff.len()),
                    BufferArg::from_raw_parts(h_pval.clone(), pval.len()),
                    BufferArg::from_raw_parts(h_seq.clone(), seq.len()),
                    BufferArg::from_raw_parts(h_ir.clone(), 2),
                    BufferArg::from_raw_parts(h_ic.clone(), ic.len()),
                    BufferArg::from_raw_parts(h_fl.clone(), n_words),
                    BufferArg::from_raw_parts(h_sm.clone(), n),
                    BufferArg::from_raw_parts(h_gi.clone(), n),
                    BufferArg::from_raw_parts(h_cslot.clone(), n),
                    BufferArg::from_raw_parts(h_cend.clone(), n),
                    seq_max,
                );
                count_tile::launch_unchecked(
                    &client,
                    tiles_grid(n_tiles),
                    CubeDim::new_1d(units as u32),
                    BufferArg::from_raw_parts(h_cslot.clone(), n),
                    BufferArg::from_raw_parts(h_ctc.clone(), n_tiles),
                    BufferArg::from_raw_parts(h_cup.clone(), n_tiles * units),
                    units,
                    rake,
                    log,
                );
                count_spine::launch_unchecked(
                    &client,
                    CubeCount::new_single(),
                    CubeDim::new_1d(units as u32),
                    BufferArg::from_raw_parts(h_ctc.clone(), n_tiles),
                    BufferArg::from_raw_parts(h_cxc.clone(), n_tiles),
                    BufferArg::from_raw_parts(h_ctotal.clone(), 1),
                    units,
                    log,
                );
            }
            let tb = client.read_one(h_ctotal.clone()).expect("setup candidate count");
            let c = bytemuck::cast_slice::<u8, u32>(&tb)[0] as usize;
            let k = ((c as u32 + 1).next_power_of_two().trailing_zeros()) as u32;
            let mut d0 = vec![1u32; c + 1];
            d0[c] = 0;
            (
                client.empty(n * 4),
                client.empty((k as usize * (c + 1)).max(1) * 4),
                client.empty((c + 1) * 4),
                client.empty((c + 1) * 4),
                client.create_from_slice(bytemuck::cast_slice(&d0)),
                client.empty((c + 1) * 4),
                client.empty((c + 1) * 4),
                client.create_from_slice(bytemuck::cast_slice(&[c as u32])),
                client.empty((c + 1) * 4),
                c,
            )
        } else {
            let z = client.empty(0);
            (
                z.clone(),
                z.clone(),
                z.clone(),
                z.clone(),
                z.clone(),
                z.clone(),
                z.clone(),
                z.clone(),
                z.clone(),
                0usize,
            )
        };
    let kmax = (c_host as u32 + 1).next_power_of_two().trailing_zeros();
    let cstride = c_host + 1;
    // Timed samples per dispatch; the minimum is reported.
    let samples: usize = std::env::var("GLYPH_CHAIN_LOOP").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
    // FnMut: the rank arm parks the final depth buffer in `rank_out` for
    // the mark arm's next call.
    let mut launch = |s: usize| {
        if decode_mode && s == 0 {
            unsafe {
                decode::launch_unchecked(
                    &client,
                    cubes_of(n_words),
                    CubeDim::new_1d(256),
                    BufferArg::from_raw_parts(h_bytes.clone(), n_words),
                    BufferArg::from_raw_parts(h_bi.clone(), bi.len()),
                    BufferArg::from_raw_parts(h_bm.clone(), bm.len()),
                    BufferArg::from_raw_parts(h_bc.clone(), bc.len()),
                    BufferArg::from_raw_parts(h_fl.clone(), n_words),
                    BufferArg::from_raw_parts(h_sm.clone(), n),
                    BufferArg::from_raw_parts(h_gi.clone(), n),
                    BufferArg::from_raw_parts(h_hgt.clone(), n),
                    BufferArg::from_raw_parts(h_cslot.clone(), n),
                    bshift,
                );
            }
            return;
        }
        if cluster_mode && s == 1 {
            unsafe {
                cluster_probe::launch_unchecked(
                    &client,
                    cubes_of(n_words),
                    CubeDim::new_1d(256),
                    BufferArg::from_raw_parts(h_bytes.clone(), n_words),
                    BufferArg::from_raw_parts(h_bmap.clone(), bitmap.len()),
                    BufferArg::from_raw_parts(h_poff.clone(), poff.len()),
                    BufferArg::from_raw_parts(h_pval.clone(), pval.len()),
                    BufferArg::from_raw_parts(h_seq.clone(), seq.len()),
                    BufferArg::from_raw_parts(h_ir.clone(), 2),
                    BufferArg::from_raw_parts(h_ic.clone(), ic.len()),
                    BufferArg::from_raw_parts(h_fl.clone(), n_words),
                    BufferArg::from_raw_parts(h_sm.clone(), n),
                    BufferArg::from_raw_parts(h_gi.clone(), n),
                    BufferArg::from_raw_parts(h_cslot.clone(), n),
                    BufferArg::from_raw_parts(h_cend.clone(), n),
                    seq_max,
                );
            }
            return;
        }
        if cluster_mode && s == 2 {
            unsafe {
                count_tile::launch_unchecked(
                    &client,
                    tiles_grid(n_tiles),
                    CubeDim::new_1d(units as u32),
                    BufferArg::from_raw_parts(h_cslot.clone(), n),
                    BufferArg::from_raw_parts(h_ctc.clone(), n_tiles),
                    BufferArg::from_raw_parts(h_cup.clone(), n_tiles * units),
                    units,
                    rake,
                    log,
                );
                count_spine::launch_unchecked(
                    &client,
                    CubeCount::new_single(),
                    CubeDim::new_1d(units as u32),
                    BufferArg::from_raw_parts(h_ctc.clone(), n_tiles),
                    BufferArg::from_raw_parts(h_cxc.clone(), n_tiles),
                    BufferArg::from_raw_parts(h_ctotal.clone(), 1),
                    units,
                    log,
                );
                cand_scatter::launch_unchecked(
                    &client,
                    tiles_grid(n_tiles),
                    CubeDim::new_1d(units as u32),
                    BufferArg::from_raw_parts(h_cslot.clone(), n),
                    BufferArg::from_raw_parts(h_cxc.clone(), n_tiles),
                    BufferArg::from_raw_parts(h_cup.clone(), n_tiles * units),
                    BufferArg::from_raw_parts(h_hp.clone(), c_host),
                    units,
                    rake,
                );
            }
            return;
        }
        if cluster_mode && s == 3 {
            unsafe {
                jump_build::launch_unchecked(
                    &client,
                    cubes_of(c_host + 1),
                    CubeDim::new_1d(256),
                    BufferArg::from_raw_parts(h_hp.clone(), c_host),
                    BufferArg::from_raw_parts(h_cend.clone(), n),
                    BufferArg::from_raw_parts(h_ir.clone(), 2),
                    BufferArg::from_raw_parts(h_ctotal.clone(), 1),
                    BufferArg::from_raw_parts(h_parent.clone(), c_host + 1),
                );
                // Fresh level-0 sources each sample — and the rotation
                // never writes h_d0 (round 1 of a naive ping-pong would,
                // seeding the next sample with stale depths).
                let mut sp = h_parent.clone();
                let mut sd = h_d0.clone();
                for k in 0..kmax {
                    let tp = if k % 2 == 0 { h_parent_b.clone() } else { h_parent.clone() };
                    let td = if k % 2 == 0 { h_d_a.clone() } else { h_d_b.clone() };
                    rank_step::launch_unchecked(
                        &client,
                        cubes_of(c_host + 1),
                        CubeDim::new_1d(256),
                        BufferArg::from_raw_parts(sp.clone(), c_host + 1),
                        BufferArg::from_raw_parts(sd.clone(), c_host + 1),
                        BufferArg::from_raw_parts(tp.clone(), c_host + 1),
                        BufferArg::from_raw_parts(td.clone(), c_host + 1),
                        BufferArg::from_raw_parts(h_lvl.clone(), kmax as usize * (c_host + 1)),
                        k as usize,
                        cstride,
                    );
                    sp = tp;
                    sd = td;
                }
                rank_out = sd.clone();
                item_roots::launch_unchecked(
                    &client,
                    cubes_of(items.len().max(1)),
                    CubeDim::new_1d(256),
                    BufferArg::from_raw_parts(h_hp.clone(), c_host),
                    BufferArg::from_raw_parts(h_ctotal.clone(), 1),
                    BufferArg::from_raw_parts(h_ir.clone(), 2),
                    BufferArg::from_raw_parts(h_ic.clone(), ic.len()),
                    BufferArg::from_raw_parts(h_roots.clone(), 1),
                );
            }
            return;
        }
        if cluster_mode && s == 4 {
            unsafe {
                cluster_mark::launch_unchecked(
                    &client,
                    cubes_of(c_host.max(1)),
                    CubeDim::new_1d(256),
                    BufferArg::from_raw_parts(h_hp.clone(), c_host),
                    BufferArg::from_raw_parts(rank_out.clone(), c_host + 1),
                    BufferArg::from_raw_parts(h_lvl.clone(), kmax as usize * (c_host + 1)),
                    BufferArg::from_raw_parts(h_ctotal.clone(), 1),
                    BufferArg::from_raw_parts(h_roots.clone(), 1),
                    BufferArg::from_raw_parts(h_ir.clone(), 2),
                    BufferArg::from_raw_parts(h_cend.clone(), n),
                    BufferArg::from_raw_parts(h_cslot.clone(), n),
                    BufferArg::from_raw_parts(h_sm.clone(), n),
                    BufferArg::from_raw_parts(h_gi.clone(), n),
                    BufferArg::from_raw_parts(h_fl.clone(), n_words),
                    kmax as usize,
                    cstride,
                    bitmap_advance,
                );
            }
            return;
        }
        let t = s - pre;
        unsafe {
            match t {
                0 => {
                    tile_scan::launch_unchecked(
                        &client,
                        tiles_grid(n_tiles),
                        CubeDim::new_1d(units as u32),
                        BufferArg::from_raw_parts(h_fl.clone(), n_words),
                        BufferArg::from_raw_parts(h_sm.clone(), n),
                        BufferArg::from_raw_parts(h_ir.clone(), 2),
                        BufferArg::from_raw_parts(h_ie.clone(), IE_STRIDE),
                        BufferArg::from_raw_parts(h_tc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
                        BufferArg::from_raw_parts(h_tm.clone(), n_tiles),
                        units,
                        rake,
                        log,
                    );
                }
                1 => {
                    spine_scan::launch_unchecked(
                        &client,
                        CubeCount::new_single(),
                        CubeDim::new_1d(units as u32),
                        BufferArg::from_raw_parts(h_tc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
                        BufferArg::from_raw_parts(h_tm.clone(), n_tiles),
                        BufferArg::from_raw_parts(h_xc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
                        BufferArg::from_raw_parts(h_xm.clone(), n_tiles),
                        units,
                        log,
                    );
                }
                2 => {
                    apply::launch_unchecked(
                        &client,
                        tiles_grid(n_tiles),
                        CubeDim::new_1d(units as u32),
                        BufferArg::from_raw_parts(h_fl.clone(), n_words),
                        BufferArg::from_raw_parts(h_sm.clone(), n),
                        BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                        BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
                        BufferArg::from_raw_parts(h_ir.clone(), 2),
                        BufferArg::from_raw_parts(h_ie.clone(), IE_STRIDE),
                        BufferArg::from_raw_parts(h_im.clone(), IM_STRIDE),
                        BufferArg::from_raw_parts(h_xc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
                        BufferArg::from_raw_parts(h_xm.clone(), n_tiles),
                        BufferArg::from_raw_parts(h_wm.clone(), n),
                        BufferArg::from_raw_parts(h_wc.clone(), n),
                        BufferArg::from_raw_parts(h_otb.clone(), n),
                        BufferArg::from_raw_parts(h_rmax.clone(), 1),
                        BufferArg::from_raw_parts(h_xmax.clone(), 1),
                        units,
                        rake,
                        log,
                        inline_resolve,
                    );
                }
                3 => {
                    if needs_resolve {
                        resolve_x::launch_unchecked(
                            &client,
                            cubes_of(n.div_ceil(rspan)),
                            CubeDim::new_1d(256),
                            BufferArg::from_raw_parts(h_sm.clone(), n),
                            BufferArg::from_raw_parts(h_fl.clone(), n_words),
                            BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
                            BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                            BufferArg::from_raw_parts(h_im.clone(), IM_STRIDE),
                            BufferArg::from_raw_parts(h_ie.clone(), IE_STRIDE),
                            BufferArg::from_raw_parts(h_ir.clone(), 2),
                            BufferArg::from_raw_parts(h_wc.clone(), n),
                            BufferArg::from_raw_parts(h_otb.clone(), n),
                            BufferArg::from_raw_parts(h_rmax.clone(), 1),
                            BufferArg::from_raw_parts(h_xmax.clone(), 1),
                            256,
                            rspan,
                        );
                    }
                }
                4 => {
                    // extent_pair + derive_stride share this window (the rank
                    // stages set the precedent for merged dispatches).
                    extent_pair::launch_unchecked(
                        &client,
                        cubes_of(n),
                        CubeDim::new_1d(256),
                        BufferArg::from_raw_parts(h_sm.clone(), n),
                        BufferArg::from_raw_parts(h_fl.clone(), n_words),
                        BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                        BufferArg::from_raw_parts(h_plan.clone(), 3),
                        BufferArg::from_raw_parts(h_extent.clone(), 2),
                    );
                    derive_stride::launch_unchecked(
                        &client,
                        CubeCount::new_single(),
                        CubeDim::new_1d(1),
                        BufferArg::from_raw_parts(h_extent.clone(), 2),
                        BufferArg::from_raw_parts(h_ie.clone(), IE_STRIDE),
                        BufferArg::from_raw_parts(h_gap.clone(), 1),
                        BufferArg::from_raw_parts(h_strides.clone(), 2),
                    );
                }
                _ => {
                    paginate::launch_unchecked(
                        &client,
                        cubes_of(n),
                        CubeDim::new_1d(256),
                        BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
                        BufferArg::from_raw_parts(h_fl.clone(), n_words),
                        BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                        BufferArg::from_raw_parts(h_im.clone(), IM_STRIDE),
                        BufferArg::from_raw_parts(h_ie.clone(), IE_STRIDE),
                        BufferArg::from_raw_parts(h_ir.clone(), 2),
                        BufferArg::from_raw_parts(h_strides.clone(), 2),
                    );
                }
            }
        }
    };
    // The per-dispatch GPU windows. Every stage runs `samples` times; the
    // minimum survives. A window that resolved to no measurement counts as a
    // missing sample, never as a zero.
    let mut stage_names: Vec<&'static str> = Vec::new();
    if decode_mode {
        stage_names.push("decode");
    }
    if cluster_mode {
        stage_names.push("cluster_probe");
        stage_names.push("cluster_compact");
        stage_names.push("cluster_rank");
        stage_names.push("cluster_mark");
    }
    stage_names.extend([
        "tile_scan",
        "spine_scan",
        "apply",
        "resolve_x",
        "derive_stride",
        "paginate",
    ]);
    // The STAGES knob is a bisection count, not a hint — reject past the
    // dispatched table instead of panicking in the report loops below.
    assert!(
        stages <= stage_names.len(),
        "GLYPH_CHAIN_STAGES={stages} exceeds the {} stages this configuration dispatches",
        stage_names.len()
    );
    let stage_meta = |s: usize| -> (&'static str, usize, u32) {
        if decode_mode && s == 0 {
            return ("decode", n_words.div_ceil(256), 256);
        }
        if cluster_mode && s == 1 {
            return ("cluster_probe", n_words.div_ceil(256), 256);
        }
        if cluster_mode && s == 2 {
            return ("cluster_compact", n_tiles, units as u32);
        }
        if cluster_mode && s == 3 {
            // 1 + K + 1 dispatches (jump, rank steps, roots) under one
            // window; the count is the dominant per-dispatch shape.
            return ("cluster_rank", (c_host + 1).div_ceil(256), 256);
        }
        if cluster_mode && s == 4 {
            return ("cluster_mark", c_host.max(1).div_ceil(256), 256);
        }
        let t = s - pre;
        let (cubes, dim) = match t {
            0 | 2 => (n_tiles, units as u32),
            1 => (1, units as u32),
            3 => (n.div_ceil(rspan).div_ceil(256), 256),
            4 => (1, 1),
            _ => (n.div_ceil(256), 256),
        };
        (stage_names[s], cubes, dim)
    };
    let mut mins: Vec<Option<std::time::Duration>> = vec![None; stages];
    let mut missing_windows = 0usize;
    let mut timing_method = String::new();
    for _ in 0..samples {
        for (s, slot) in mins.iter_mut().enumerate() {
            if s == 3 + pre && !needs_resolve {
                // The resolve_x slot; foldless corpora skip the dispatch
                // (and its window) entirely.
                continue;
            }
            let window = client.profile_start().expect("profile_start");
            launch(s);
            let dur = client.profile_end(window).expect("profile_end");
            if timing_method.is_empty() {
                timing_method = format!("{}", dur.timing_method());
            }
            match pollster::block_on(dur.resolve()) {
                Some(ticks) => {
                    let d = ticks.duration();
                    *slot = Some(slot.map_or(d, |cur| cur.min(d)));
                }
                None => missing_windows += 1,
            }
        }
    }

    // Readbacks: host-side, wall clock (the product flow binds instead).
    let t1 = std::time::Instant::now();
    let lc_bytes = client.read_one(h_lc.clone()).expect("read lc");
    let _wc = client.read_one(h_wc.clone()).expect("read wc");
    let _wm = client.read_one(h_wm.clone()).expect("read wm");
    let _lm = client.read_one(h_lm.clone()).expect("read lm");
    let readback_dt = t1.elapsed();
    // The cluster lanes' own witness, whenever the mark stage ran (it is
    // absolute stage 4, so stages >= 5 — cluster implies decode): packed
    // flags (low byte, trailer bit included) and advance, bit-exact against
    // decode_all + resolve_clusters — the same PRE-SCAN reference
    // --cubecl-cluster-check diffs against. Not run_scan_pipeline's slots:
    // those carry scan-derived bits (F_RENDERED) the device pass never
    // writes. The stages after cluster only READ fl/sm, so the end-of-run
    // readback still sees the pass's output untouched.
    if cluster_mode && stages >= 5 {
        let mut cslots = crate::fold::Slots::new(n);
        let _ = crate::fold::decode_all(&bytes, &mut cslots, &trie);
        crate::fold::resolve_clusters(&bytes, &mut cslots, &trie, &items[0]);
        let fl_bytes = client.read_one(h_fl.clone()).expect("read fl");
        let sm_bytes = client.read_one(h_sm.clone()).expect("read sm");
        let flw: &[u32] = bytemuck::cast_slice(&fl_bytes);
        let smv: &[f32] = bytemuck::cast_slice(&sm_bytes);
        let mut bad = 0usize;
        for id in 0..n {
            let want_f = cslots.flags(id) & 0xFF;
            let got_f = (flw[id >> 2] >> (((id & 3) * 8) as u32)) & 0xFF;
            if want_f != got_f || cslots.advance(id).to_bits() != smv[id].to_bits() {
                if bad < 8 {
                    println!(
                        "  MISMATCH byte {id} flags: cpu {want_f:#04x} gpu {got_f:#04x} advance: cpu {:e} gpu {:e}",
                        cslots.advance(id),
                        smv[id]
                    );
                }
                bad += 1;
            }
        }
        assert_eq!(bad, 0, "bench cluster verification failed: {bad} lane mismatches");
    }
    // Correctness at speed: the bench's whole number is worthless if the fast
    // path is wrong — diff the leader row/col lanes against the CPU reference.
    let lc: &[u32] = bytemuck::cast_slice(&lc_bytes);
    if stages < 3 + pre {
        println!(
            "cubecl-chain-bench: {} ({} B, {} tiles @ {}x{}, wrap {}) stages {} — pre-apply stages only, no verification",
            corpus_path.display(),
            n,
            n_tiles,
            units,
            rake,
            wrap_width,
            stages
        );
        for (s, m) in mins.iter().enumerate() {
            let (name, cubes, dim) = stage_meta(s);
            println!("  {name:<14} cubes={cubes} units={dim} min={m:?}");
        }
        std::process::exit(0);
    }
    let mut bad = 0usize;
    for id in 0..n {
        if r.slots.flags(id) & F_LEADER == 0 {
            continue;
        }
        if r.slots.row(id) != lc[id * LC_STRIDE + LC_ROW] as i64
            || r.slots.col(id) != lc[id * LC_STRIDE + LC_COL] as i64
        {
            bad += 1;
        }
    }
    assert_eq!(bad, 0, "bench verification failed: {bad} leader lane mismatches");

    let chain: std::time::Duration = mins.iter().filter_map(|d| *d).sum();
    let total = chain + readback_dt;
    println!(
        "cubecl-chain-bench: {} ({} B, {} tiles @ {}x{}, wrap {}, cluster {}, samples {}) timing={} missing_windows={} — \
         cpu decode+scan {:?} | chain (sum of per-dispatch minima) {:?} readbacks {:?} total {:?} ({:.1} MB/s)",
        corpus_path.display(),
        n,
        n_tiles,
        units,
        rake,
        wrap_width,
        cluster_mode,
        samples,
        timing_method,
        missing_windows,
        decode_dt,
        chain,
        readback_dt,
        total,
        n as f64 / 1e6 / total.as_secs_f64()
    );
    for (s, m) in mins.iter().enumerate() {
        let (name, cubes, dim) = stage_meta(s);
        println!("  {name:<14} cubes={cubes:>7} units={dim} min={m:?}");
    }
    std::process::exit(0);
}
