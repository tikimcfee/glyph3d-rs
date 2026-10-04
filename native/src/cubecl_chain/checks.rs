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
use super::position::{derive_stride, extent_pair, resolve_x};
use super::scan::{apply, spine_scan, tile_scan};
use super::tail::emit_records;
use super::{
    F_LEADER, IE_STRIDE, IM_STRIDE, LC_COL, LC_ROW, LC_STRIDE, LM_STRIDE, LM_X, LM_Y, LM_Z,
    PARTIAL_COUNT_STRIDE, pack_words,
};

pub fn run(ctx: &GpuContext, fixture_path: &Path) -> ! {
    let fx = crate::fixture::load_pipe_fixture(fixture_path).unwrap_or_else(|e| {
        eprintln!("cubecl-chain-check: {e}");
        std::process::exit(1);
    });
    let n = fx.bytes.len();
    let item_count = fx.items.len();
    // The tile shape, env-overridable so a (units, rake) sweep runs through
    // the same instrument — cross-shape agreement with the CPU scan's
    // (64, 256) chunks is associativity checked in situ.
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
    // resolve_x worker span (bytes per worker; entry walks scale with
    // worker count, sweeps with span).
    let rspan: usize = std::env::var("GLYPH_CHAIN_SPAN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);

    // The CPU reference — a different tree shape at the (64, 256) tuning.
    let r = run_scan_pipeline(&fx.bytes, &fx.trie, &fx.items, DEFAULT_CHUNK_SIZE, DEFAULT_GROUP_SIZE, 1);

    // Uploads: statics from the CPU decode (the bench's mode 0 shape). The
    // measure static is ADVANCE ONLY — the scan never reads height.
    // Statics: advance (f32/byte) + PACKED flags (u8/byte, four per word —
    // the chain reads fl three-to-four passes and consumes only the low
    // byte; the full flags stay CPU-side for the renderer).
    let n_words = n.div_ceil(4);
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
    // Phase 4 rung 2: the record lanes upload with the statics — gi and
    // height per byte from the reference decode (bit-identical to the
    // device decode's lanes, which decode-check fences separately).
    let mut giv = vec![0u32; n];
    let mut hgv = vec![0f32; n];
    let mut leaders = 0usize;
    let mut item_leaders = vec![0u32; item_count];
    for (idx, item) in fx.items.iter().enumerate() {
        let mut c = 0u32;
        for i in item.byte_start as usize..(item.byte_start + item.byte_count) as usize {
            if r.slots.flags(i) & F_LEADER != 0 {
                c += 1;
            }
        }
        item_leaders[idx] = c;
        leaders += c as usize;
    }
    for i in 0..n {
        giv[i] = r.slots.gi[i];
        hgv[i] = r.slots.height(i);
    }
    let mut ir = Vec::with_capacity(item_count * 2);
    let mut ie = Vec::with_capacity(item_count * IE_STRIDE);
    let mut im = Vec::with_capacity(item_count * IM_STRIDE);
    let mut page_gap_x = Vec::with_capacity(item_count);
    for item in &fx.items {
        ir.push(item.byte_start as u32);
        ir.push((item.byte_start + item.byte_count) as u32);
        ie.push(item.page_rows as u32);
        ie.push(item.page_cols as u32);
        ie.push(item.scroll_rows as u32);
        ie.push(item.pages_wide as u32);
        ie.push(item.wrap_width as u32);
        ie.push(item.has_page as u32);
        ie.push(match item.wrap_mode {
            WrapMode::Down => 0u32,
            WrapMode::Back => 1,
        });
        ie.push(0u32);
        im.push(item.origin_y as f32);
        im.push(item.origin_z as f32);
        im.push(item.line_height as f32);
        im.push(item.z_step as f32);
        im.push(item.band_stride_y as f32);
        im.push(item.depth_per_band as f32);
        im.push(item.depth_per_col as f32);
        im.push(0.0f32); // IM_PAGE_STRIDE_X: the device chain derives it on device
        im.push(item.origin_x as f32);
        // The z_step pair's tail: the f64 param minus its f32 high word.
        im.push((item.z_step - item.z_step as f32 as f64) as f32);
        page_gap_x.push(item.page_gap_x as f32);
    }

    let setup = WgpuSetup {
        instance: ctx.instance.clone(),
        adapter: ctx.adapter.clone(),
        device: ctx.device.clone(),
        queue: ctx.queue.clone(),
        backend: AutoGraphicsApi::backend(),
    };
    let cdev = cubecl::wgpu::init_device(setup, Default::default());
    let client = cubecl::Device::Wgpu(cdev).client();

    let h_fl = client.create_from_slice(bytemuck::cast_slice(&fl));
    let h_sm = client.create_from_slice(bytemuck::cast_slice(&sm));
    let h_gi = client.create_from_slice(bytemuck::cast_slice(&giv));
    let h_hgt = client.create_from_slice(bytemuck::cast_slice(&hgv));
    let h_recs = client.empty(leaders.max(1) * 8 * 4);
    // Per-item record bases: the record stream is item order, ordinal
    // order within items — base[it] is where item it's records start.
    let mut rec_base = vec![0u32; item_count];
    for i in 1..item_count {
        rec_base[i] = rec_base[i - 1] + item_leaders[i - 1];
    }
    let h_base = client.create_from_slice(bytemuck::cast_slice(&rec_base));
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
    let h_strides = client.empty(item_count * 8);
    let zeroes = vec![0u32; item_count];
    let h_rmax = client.create_from_slice(bytemuck::cast_slice(&zeroes));
    let h_xmax = client.create_from_slice(bytemuck::cast_slice(&zeroes));
    // The extent pair (sum, tail), keyed-zero: raw 0u32 would decode as
    // NaN (key_to_float(0) — see the ordered_key pair for why).
    let zero_keys = vec![0x8000_0000u32; item_count * 2];
    let h_extent = client.create_from_slice(bytemuck::cast_slice(&zero_keys));
    // The extent walk's plan: [start, stop, segment_width] per item, the
    // width pre-resolved by fold.rs's rule (wrap if wrapped, else page
    // columns if paged, else 0) — see extent_pair's header for why this
    // rides its own buffer and not ie.
    let mut walk_plan: Vec<u32> = Vec::with_capacity(item_count * 3);
    let mut min_sw = u32::MAX;
    for item in &fx.items {
        let width = if item.wrap_width > 0 {
            item.wrap_width
        } else if item.has_page {
            item.page_cols
        } else {
            0
        };
        if width > 0 && (width as u32) < min_sw {
            min_sw = width as u32;
        }
        walk_plan.push(item.byte_start as u32);
        walk_plan.push((item.byte_start + item.byte_count) as u32);
        walk_plan.push(width as u32);
    }
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
    // GLYPH_CHAIN_STAGES=N runs only the first N dispatches (bisection aid).
    let stages: usize = std::env::var("GLYPH_CHAIN_STAGES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let inline_resolve = false;
    // One cube per tile (dim = units); the byte-wide kernels stay 256-unit.
    let tiles_grid = |tiles: usize| {
        CubeCount::Static(tiles.min(65535) as u32, tiles.div_ceil(65535) as u32, 1)
    };
    let t0 = std::time::Instant::now();
    unsafe {
        tile_scan::launch_unchecked(
            &client,
            tiles_grid(n_tiles),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(h_fl.clone(), n_words),
            BufferArg::from_raw_parts(h_sm.clone(), n),
            BufferArg::from_raw_parts(h_ir.clone(), item_count * 2),
            BufferArg::from_raw_parts(h_ie.clone(), item_count * IE_STRIDE),
            BufferArg::from_raw_parts(h_tc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
            BufferArg::from_raw_parts(h_tm.clone(), n_tiles),
            units,
            rake,
            log,
        );
        if stages >= 2 {
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
        if stages >= 3 {
            if std::env::var_os("GLYPH_CHAIN_DEBUG").is_some() {
                // Force stages 1-2 to land in their own submission, so a later
                // batch failing cannot take tile_scan's write down with it.
                let probe = client.read_one(h_tc.clone()).expect("pre-apply probe");
                let pv: &[u32] = bytemuck::cast_slice(&probe);
                println!("  dbg pre-apply tc[{} {} {} {}]", pv[0], pv[1], pv[2], pv[3]);
            }
            apply::launch_unchecked(
                &client,
                tiles_grid(n_tiles),
                CubeDim::new_1d(units as u32),
                BufferArg::from_raw_parts(h_fl.clone(), n_words),
                BufferArg::from_raw_parts(h_sm.clone(), n),
                BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
                BufferArg::from_raw_parts(h_ir.clone(), item_count * 2),
                BufferArg::from_raw_parts(h_ie.clone(), item_count * IE_STRIDE),
                BufferArg::from_raw_parts(h_im.clone(), item_count * IM_STRIDE),
                BufferArg::from_raw_parts(h_xc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
                BufferArg::from_raw_parts(h_xm.clone(), n_tiles),
                BufferArg::from_raw_parts(h_wm.clone(), n),
                BufferArg::from_raw_parts(h_wc.clone(), n),
                BufferArg::from_raw_parts(h_otb.clone(), n),
                BufferArg::from_raw_parts(h_rmax.clone(), item_count),
                BufferArg::from_raw_parts(h_xmax.clone(), item_count),
                units,
                rake,
                log,
                inline_resolve,
                false,
            );
        }
        if stages >= 4 {
            extent_pair::launch_unchecked(
                &client,
                cubes_of(n),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(h_sm.clone(), n),
                BufferArg::from_raw_parts(h_fl.clone(), n_words),
                BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                BufferArg::from_raw_parts(h_plan.clone(), item_count * 3),
                BufferArg::from_raw_parts(h_extent.clone(), item_count * 2),
                min_sw,
            );
            derive_stride::launch_unchecked(
                &client,
                cubes_of(item_count),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(h_extent.clone(), item_count * 2),
                BufferArg::from_raw_parts(h_ie.clone(), item_count * IE_STRIDE),
                BufferArg::from_raw_parts(h_gap.clone(), item_count),
                BufferArg::from_raw_parts(h_strides.clone(), item_count * 2),
            );
        }
        if stages >= 5 {
            resolve_x::launch_unchecked(
                &client,
                cubes_of(n.div_ceil(rspan)),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(h_sm.clone(), n),
                BufferArg::from_raw_parts(h_fl.clone(), n_words),
                BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
                BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                BufferArg::from_raw_parts(h_im.clone(), item_count * IM_STRIDE),
                BufferArg::from_raw_parts(h_ie.clone(), item_count * IE_STRIDE),
                BufferArg::from_raw_parts(h_ir.clone(), item_count * 2),
                BufferArg::from_raw_parts(h_wc.clone(), n),
                BufferArg::from_raw_parts(h_otb.clone(), n),
                BufferArg::from_raw_parts(h_wm.clone(), n),
                BufferArg::from_raw_parts(h_rmax.clone(), item_count),
                BufferArg::from_raw_parts(h_xmax.clone(), item_count),
                BufferArg::from_raw_parts(h_extent.clone(), item_count * 2),
                BufferArg::from_raw_parts(h_gap.clone(), item_count),
                256,
                rspan,
            );
        }
        // Phase 4 rung 2: the record emitter — only when the full chain ran
        if stages >= 5 {
            let h_win0 = client.create_from_slice(bytemuck::cast_slice(&[0u32]));
            emit_records::launch_unchecked(
                &client,
                cubes_of(n),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(h_fl.clone(), n_words),
                BufferArg::from_raw_parts(h_wc.clone(), n),
                BufferArg::from_raw_parts(h_ir.clone(), item_count * 2),
                BufferArg::from_raw_parts(h_base.clone(), item_count),
                BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
                BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                BufferArg::from_raw_parts(h_sm.clone(), n),
                BufferArg::from_raw_parts(h_hgt.clone(), n),
                BufferArg::from_raw_parts(h_gi.clone(), n),
                BufferArg::from_raw_parts(h_recs.clone(), leaders * 8),
                BufferArg::from_raw_parts(h_win0, 1),
            );
        }
    }
    let lc_bytes = client.read_one(h_lc).expect("read lc");
    let wc_bytes = client.read_one(h_wc).expect("read wc");
    let wm_bytes = client.read_one(h_wm).expect("read wm");
    let lm_bytes = client.read_one(h_lm).expect("read lm");
    let rmax_bytes = client.read_one(h_rmax).expect("read rmax");
    let xmax_bytes = client.read_one(h_xmax).expect("read xmax");
    let recs_bytes = if stages >= 5 {
        Some(client.read_one(h_recs).expect("read recs"))
    } else {
        None
    };
    let dt = t0.elapsed();
    if std::env::var_os("GLYPH_CHAIN_DEBUG").is_some() {
        let fl_bytes = client.read_one(h_fl).expect("read fl");
        let flb: &[u32] = bytemuck::cast_slice(&fl_bytes);
        println!(
            "  dbg fl readback: [{} {} {} {} {} {} {} {}]",
            flb[0], flb[1], flb[2], flb[3], flb[4], flb[5], flb[6], flb[7]
        );
        let tc_bytes = client.read_one(h_tc).expect("read tc");
        let xc_bytes = client.read_one(h_xc).expect("read xc");
        let tc: &[u32] = bytemuck::cast_slice(&tc_bytes);
        let xc: &[u32] = bytemuck::cast_slice(&xc_bytes);
        for c in 0..n_tiles.min(3) {
            let o = c * PARTIAL_COUNT_STRIDE;
            println!(
                "  dbg tile {c} tc[reset={} nl={} glyphs={} rows={} head={} tail={} wrap={} mode={}] xc[same={}]",
                tc[o], tc[o + 1], tc[o + 2], tc[o + 3], tc[o + 4], tc[o + 5], tc[o + 6], tc[o + 7],
                xc[o] == tc[o] && xc[o + 2] == tc[o + 2]
            );
        }
    }
    let lc: &[u32] = bytemuck::cast_slice(&lc_bytes);
    let wc: &[u32] = bytemuck::cast_slice(&wc_bytes);
    let wm: &[f32] = bytemuck::cast_slice(&wm_bytes);
    let lm: &[f32] = bytemuck::cast_slice(&lm_bytes);
    let rmax: &[u32] = bytemuck::cast_slice(&rmax_bytes);
    let xmax: &[u32] = bytemuck::cast_slice(&xmax_bytes);

    // The item maxima, diffed DIRECTLY against the CPU fold's item_bounds
    // lanes (TOTAL_ROWS, MAX_ROW_EXTENT) — both arrive with resolve_x (stage 5).
    let host_key_to_float = |k: u32| -> f32 {
        let b = if (k & 0x8000_0000) != 0 { k & 0x7FFF_FFFF } else { !k };
        f32::from_bits(b)
    };
    let mut bad = 0usize;
    let mut max_x_dev = 0.0f64;
    if stages >= 5 {
        for (i, got) in rmax.iter().take(item_count).enumerate() {
            let want_rows = r.item_bounds[i * 8 + 6];
            if *got as f64 != want_rows {
                if bad < 8 {
                    println!("  MISMATCH item {i} total_rows: cpu {want_rows} gpu {got}");
                }
                bad += 1;
            }
        }
        for (i, got) in xmax.iter().take(item_count).enumerate() {
            let want_x = r.item_bounds[i * 8 + 7];
            let got_x = host_key_to_float(*got) as f64;
            let x_dev = (got_x - want_x).abs() / want_x.abs().max(1.0);
            if x_dev > max_x_dev {
                max_x_dev = x_dev;
            }
        }
    }

    // Foldless items leave wm/wc/otb unwritten on purpose (apply resolves
    // them in-register) — the ord/line_adv diffs apply only to folding items.
    let fold_of_item: Vec<i64> = fx
        .items
        .iter()
        .map(|it| {
            if it.wrap_width > 0 {
                it.wrap_width
            } else if it.has_page && it.page_cols > 0 {
                it.page_cols
            } else {
                0
            }
        })
        .collect();
    let fold_at = |byte: usize| -> i64 {
        let mut f = 0i64;
        for (k, item) in fx.items.iter().enumerate() {
            if (item.byte_start as usize) <= byte
                && byte < ((item.byte_start + item.byte_count) as usize)
            {
                f = fold_of_item[k];
            }
        }
        f
    };

    // The diff: counts bit-exact; line_advance and positions reported with
    // max deviation and held to the oracle's 1e-4 eps tier (the module
    // header's contract note — the Blelloch tree reassociates tail_adv).
    let mut max_pos_dev = 0.0f64;
    let mut max_line_dev = 0.0f64;
    let mut leaders = 0usize;
    for id in 0..n {
        if r.slots.flags(id) & F_LEADER == 0 {
            continue;
        }
        leaders += 1;
        let folds = fold_at(id);
        let mut checks = vec![
            (r.slots.row(id), lc[id * LC_STRIDE + LC_ROW] as i64, "row"),
            (r.slots.col(id), lc[id * LC_STRIDE + LC_COL] as i64, "col"),
        ];
        if folds > 0 {
            checks.push((r.slots.wc[id] as i64, wc[id] as i64, "ord"));
        }
        for (want, got, name) in checks {
            if want != got {
                if bad < 8 {
                    println!("  MISMATCH byte {id} {name}: cpu {want} gpu {got}");
                }
                bad += 1;
            }
        }
        let la_cpu = r.slots.wm[id] as f64;
        if folds > 0 {
            let la_rel = (wm[id] as f64 - la_cpu).abs() / la_cpu.abs().max(1.0);
            if la_rel > max_line_dev {
                max_line_dev = la_rel;
            }
            // fold>0 X is a BIT-tier lane — the segment walk performs the
            // same re-sum adds in the same left-fold order, and this witness
            // holds it to that. (The eps-tier position diff below would hide
            // an order change; this cannot.) lm arrives with resolve_x
            // (stage 5) — partial-stage bisection skips it.
            if stages >= 5 && lm[id * LM_STRIDE + LM_X].to_bits() != r.slots.x(id).to_bits() {
                if bad < 8 {
                    println!(
                        "  MISMATCH byte {id} fold_x: cpu {:e} gpu {:e}",
                        r.slots.x(id),
                        lm[id * LM_STRIDE + LM_X]
                    );
                }
                bad += 1;
            }
        }
        // lm lanes exist from resolve_x (stage 5) on — partial-stage
        // bisection diffs the scan lanes only.
        if stages >= 5 {
            for (k, acc) in [(LM_X, r.slots.x(id)), (LM_Y, r.slots.y(id)), (LM_Z, r.slots.z(id))] {
                let dev = (lm[id * LM_STRIDE + k] as f64 - acc as f64).abs();
                let rel = dev / (acc as f64).abs().max(1.0);
                if rel > max_pos_dev {
                    max_pos_dev = rel;
                }
            }
        }
    }

    // Phase 4 rung 2 — the records witness: the emitter's gather diffed
    // against a CPU gather over the reference's OWN ordinal compaction
    // (ord_to_byte), ordinal by ordinal. Tiers: byte identity implied by
    // the gather point, gi/row/col exact, advance/height bit-exact, X/Y/Z
    // eps — the fold>0 X bit tier lives in the byte-indexed diff above;
    // this one watches the GATHER, whose failure mode is a wrong byte or
    // order.
    let mut rec_bad = 0usize;
    let mut max_rec_dev = 0.0f64;
    if let Some(rb) = &recs_bytes {
        let recs: &[u32] = bytemuck::cast_slice(rb);
        // Byte-driven mirror of the emitter: every leader's record index
        // is base[item] + the reference's own ordinal lane (wc), so the
        // diff checks the same gather the kernel performs.
        let mut leader_item: Vec<(usize, usize)> = Vec::with_capacity(leaders);
        for (idx, item) in fx.items.iter().enumerate() {
            let mut i = item.byte_start as usize;
            let stop = (item.byte_start + item.byte_count) as usize;
            while i < stop {
                if r.slots.flags(i) & F_LEADER != 0 {
                    leader_item.push((i, idx));
                }
                i += 1;
            }
        }
        for &(b, it) in &leader_item {
            let w = (rec_base[it] as usize + r.slots.wc[b] as usize) * 8;
            let xf = f32::from_bits(recs[w]);
            let yf = f32::from_bits(recs[w + 1]);
            let zf = f32::from_bits(recs[w + 2]);
            let af = f32::from_bits(recs[w + 3]);
            let hf = f32::from_bits(recs[w + 4]);
            let mut ok = recs[w + 5] == r.slots.gi[b]
                && recs[w + 6] == r.slots.lc[b * 2]
                && recs[w + 7] == r.slots.lc[b * 2 + 1]
                && af.to_bits() == r.slots.advance(b).to_bits()
                && hf.to_bits() == r.slots.height(b).to_bits();
            for (got, want) in [(xf, r.slots.x(b)), (yf, r.slots.y(b)), (zf, r.slots.z(b))] {
                let rel = (got as f64 - want as f64).abs() / (want as f64).abs().max(1.0);
                if rel > max_rec_dev {
                    max_rec_dev = rel;
                }
                if rel > 1e-4 {
                    ok = false;
                }
            }
            if !ok {
                if rec_bad < 8 {
                    println!(
                        "  MISMATCH record @byte {b}: gi {} row {} col {} x {:e} y {:e} z {:e}",
                        recs[w + 5], recs[w + 6], recs[w + 7], xf, yf, zf
                    );
                }
                rec_bad += 1;
            }
        }
    }

    println!(
        "cubecl-chain-check: {} ({} B, {} items, {} leaders, tile {}x{}) — {} count-lane mismatches, \
         {} record mismatches (max position deviation {:.2e}), \
         max line_adv deviation {:.2e}, max x-extent deviation {:.2e}, max position deviation {:.2e}; \
         chain+readbacks {:?} (smoke timing only)",
        fx.name,
        n,
        item_count,
        leaders,
        units,
        rake,
        bad,
        rec_bad,
        max_rec_dev,
        max_line_dev,
        max_x_dev,
        max_pos_dev,
        dt
    );
    if bad > 0
        || rec_bad > 0
        || max_rec_dev > 1e-4
        || max_line_dev > 1e-4
        || max_x_dev > 1e-4
        || max_pos_dev > 1e-4
    {
        eprintln!(
            "cubecl-chain-check FAIL: {bad} count mismatches, {rec_bad} record mismatches ({max_rec_dev:.2e}), {max_line_dev:.2e} line_adv, {max_x_dev:.2e} x-extent, {max_pos_dev:.2e} position deviation"
        );
        std::process::exit(1);
    }
    println!("cubecl-chain-check PASS: counts + rows exact, fold>0 X bit-exact, line_adv + foldless positions inside 1e-4, records gathered tier-correct");
    std::process::exit(0);
}

// ── the decode-check driver ──────────────────────────────────────────────────

/// `--cubecl-decode-check <fixture.pipe.bin>`: the device decode over one
/// fixture — packed flags and advance lanes diffed BIT-EXACT per byte
/// against `fold::decode_all` on a fresh Slots (leader mode; cluster
/// resolution is the separate phase-3b pass and the fixtures' cluster
/// lanes belong to it, not to decode).
pub fn decode_check(ctx: &GpuContext, fixture_path: &Path) -> ! {
    let fx = crate::fixture::load_pipe_fixture(fixture_path).unwrap_or_else(|e| {
        eprintln!("cubecl-decode-check: {e}");
        std::process::exit(1);
    });
    let n = fx.bytes.len();
    let mut slots = crate::fold::Slots::new(n);
    let _ = crate::fold::decode_all(&fx.bytes, &mut slots, &fx.trie);

    let n_words = n.div_ceil(4);
    let packed = pack_words(&fx.bytes);
    let setup = WgpuSetup {
        instance: ctx.instance.clone(),
        adapter: ctx.adapter.clone(),
        device: ctx.device.clone(),
        queue: ctx.queue.clone(),
        backend: AutoGraphicsApi::backend(),
    };
    let cdev = cubecl::wgpu::init_device(setup, Default::default());
    let client = cubecl::Device::Wgpu(cdev).client();
    let h_bytes = client.create_from_slice(bytemuck::cast_slice(&packed));
    let h_bi = client.create_from_slice(bytemuck::cast_slice(&fx.trie.block_index));
    let h_bm = client.create_from_slice(bytemuck::cast_slice(&fx.trie.blocks_m));
    let h_bc = client.create_from_slice(bytemuck::cast_slice(&fx.trie.blocks_c));
    let h_fl = client.empty(n_words * 4);
    let h_sm = client.empty(n * 4);
    let h_gi = client.empty(n * 4);
    let h_hgt = client.empty(n * 4);
    // decode clears the candidate-slot lane as of 2026-09-30; this driver
    // never reads it, so a plain allocation rides the launch.
    let h_cslot = client.empty(n * 4);
    let cubes_of = |threads: usize| {
        let cubes = threads.div_ceil(256);
        CubeCount::Static(cubes.min(65535) as u32, cubes.div_ceil(65535) as u32, 1)
    };
    let t0 = std::time::Instant::now();
    unsafe {
        decode::launch_unchecked(
            &client,
            cubes_of(n_words),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_bytes.clone(), n_words),
            BufferArg::from_raw_parts(h_bi.clone(), fx.trie.block_index.len()),
            BufferArg::from_raw_parts(h_bm.clone(), fx.trie.blocks_m.len()),
            BufferArg::from_raw_parts(h_bc.clone(), fx.trie.blocks_c.len()),
            BufferArg::from_raw_parts(h_fl.clone(), n_words),
            BufferArg::from_raw_parts(h_sm.clone(), n),
            BufferArg::from_raw_parts(h_gi.clone(), n),
            BufferArg::from_raw_parts(h_hgt.clone(), n),
            BufferArg::from_raw_parts(h_cslot.clone(), n),
            crate::glyph_trie::BLOCK_SHIFT,
        );
    }
    let fl_bytes = client.read_one(h_fl).expect("read fl");
    let sm_bytes = client.read_one(h_sm).expect("read sm");
    let gi_bytes = client.read_one(h_gi).expect("read gi");
    let hgt_bytes = client.read_one(h_hgt).expect("read hgt");
    let dt = t0.elapsed();
    let flw: &[u32] = bytemuck::cast_slice(&fl_bytes);
    let sm: &[f32] = bytemuck::cast_slice(&sm_bytes);
    let giv: &[u32] = bytemuck::cast_slice(&gi_bytes);
    let hgv: &[f32] = bytemuck::cast_slice(&hgt_bytes);

    let mut bad = 0usize;
    for id in 0..n {
        let want_f = slots.flags(id) & 0xFF;
        let got_f = (flw[id >> 2] >> (((id & 3) * 8) as u32)) & 0xFF;
        if want_f != got_f {
            if bad < 8 {
                println!("  MISMATCH byte {id} flags: cpu {want_f:#04x} gpu {got_f:#04x}");
            }
            bad += 1;
        }
        if slots.advance(id).to_bits() != sm[id].to_bits() {
            if bad < 8 {
                println!(
                    "  MISMATCH byte {id} advance: cpu {:e} gpu {:e}",
                    slots.advance(id),
                    sm[id]
                );
            }
            bad += 1;
        }
        if slots.gi[id] != giv[id] {
            if bad < 8 {
                println!("  MISMATCH byte {id} gi: cpu {} gpu {}", slots.gi[id], giv[id]);
            }
            bad += 1;
        }
        if slots.height(id).to_bits() != hgv[id].to_bits() {
            if bad < 8 {
                println!(
                    "  MISMATCH byte {id} height: cpu {:e} gpu {:e}",
                    slots.height(id),
                    hgv[id]
                );
            }
            bad += 1;
        }
    }
    println!(
        "cubecl-decode-check: {} ({} B, {} words) — {} lane mismatches; decode+readbacks {:?} (smoke timing only)",
        fx.name,
        n,
        n_words,
        bad,
        dt
    );
    if bad > 0 {
        eprintln!("cubecl-decode-check FAIL: {bad} lane mismatches vs decode_all");
        std::process::exit(1);
    }
    println!("cubecl-decode-check PASS: flags + advance bit-exact vs decode_all");
    std::process::exit(0);
}

// ── the cluster-check driver ──────────────────────────────────────────────────

/// `--cubecl-cluster-check <fixture.pipe.bin>`: decode + cluster on device
/// vs `decode_all` + `resolve_clusters` on CPU — packed flags (low byte,
/// trailer bit included) and advance diffed BIT-EXACT per byte. Non-cluster
/// fixtures verify the pass is a no-op for leader items.
pub fn cluster_check(ctx: &GpuContext, fixture_path: &Path) -> ! {
    let fx = crate::fixture::load_pipe_fixture(fixture_path).unwrap_or_else(|e| {
        eprintln!("cubecl-cluster-check: {e}");
        std::process::exit(1);
    });
    let n = fx.bytes.len();
    let mut slots = crate::fold::Slots::new(n);
    let _ = crate::fold::decode_all(&fx.bytes, &mut slots, &fx.trie);
    for item in &fx.items {
        if item.cluster_mode == crate::fold::ClusterMode::Cluster {
            crate::fold::resolve_clusters(&fx.bytes, &mut slots, &fx.trie, item);
        }
    }

    let n_words = n.div_ceil(4);
    let packed = pack_words(&fx.bytes);
    let (seq, seq_max, bitmap_advance) = match fx.trie.cluster_table() {
        Some((s, m, a)) => (s.to_vec(), m, a),
        None => (Vec::new(), 2u32, f32::NAN),
    };
    // The probe's key scratch is a LOCAL array of seq_max u32 per candidate
    // thread (it was workgroup memory until 2026-09-27 — WGSL zero-initializes
    // `var<workgroup>`, which cost every cube 8KB of memset). seq_max is TRIE
    // DATA, so a pathological table would still bloat the per-thread scratch;
    // fail here, loudly, instead of coupling kernel viability to atlas content.
    assert!(
        seq_max <= 64,
        "seq_max {seq_max} would exceed the local key-scratch budget"
    );
    let (bitmap, ic) = cluster_host_inputs(&seq, seq_max, &fx.items);
    let mut ir = Vec::with_capacity(fx.items.len() * 2);
    for item in &fx.items {
        ir.push(item.byte_start as u32);
        ir.push((item.byte_start + item.byte_count) as u32);
    }

    let setup = WgpuSetup {
        instance: ctx.instance.clone(),
        adapter: ctx.adapter.clone(),
        device: ctx.device.clone(),
        queue: ctx.queue.clone(),
        backend: AutoGraphicsApi::backend(),
    };
    let cdev = cubecl::wgpu::init_device(setup, Default::default());
    let client = cubecl::Device::Wgpu(cdev).client();
    let h_bytes = client.create_from_slice(bytemuck::cast_slice(&packed));
    let h_bi = client.create_from_slice(bytemuck::cast_slice(&fx.trie.block_index));
    let h_bm = client.create_from_slice(bytemuck::cast_slice(&fx.trie.blocks_m));
    let h_bc = client.create_from_slice(bytemuck::cast_slice(&fx.trie.blocks_c));
    let h_seq = client.create_from_slice(bytemuck::cast_slice(&seq));
    let h_bmap = client.create_from_slice(bytemuck::cast_slice(&bitmap));
    let (poff, pval) = cluster_pair_filter(&seq, seq_max);
    let h_poff = client.create_from_slice(bytemuck::cast_slice(&poff));
    let h_pval = client.create_from_slice(bytemuck::cast_slice(&pval));
    let h_ir = client.create_from_slice(bytemuck::cast_slice(&ir));
    let h_ic = client.create_from_slice(bytemuck::cast_slice(&ic));
    let h_fl = client.empty(n_words * 4);
    let h_sm = client.empty(n * 4);
    let h_gi = client.empty(n * 4);
    let h_hgt = client.empty(n * 4);
    let h_cslot = client.create_from_slice(bytemuck::cast_slice(&vec![0u32; n]));
    let h_cend = client.empty(n * 4);
    // The ranked chain's buffers. The compaction tiles mirror the scan
    // chain's 256x8 shape; the graph buffers are sized off C, read back
    // once after count_spine (fixtures are small — the bench does the same
    // readback in setup, outside its timing windows).
    let (units, rake) = (256usize, 8usize);
    let log = units.ilog2() as usize;
    let n_tiles = n.div_ceil(units * rake).max(1);
    let h_tc = client.empty(n_tiles * 4);
    let h_up = client.empty(n_tiles * units * 4);
    let h_xc = client.empty(n_tiles * 4);
    let h_total = client.empty(4);
    let h_hp = client.empty(n * 4);
    let cubes_of = |threads: usize| {
        let cubes = threads.div_ceil(256);
        CubeCount::Static(cubes.min(65535) as u32, cubes.div_ceil(65535) as u32, 1)
    };
    let tiles_grid = |tiles: usize| {
        CubeCount::Static(tiles.min(65535) as u32, tiles.div_ceil(65535) as u32, 1)
    };
    let t0 = std::time::Instant::now();
    unsafe {
        decode::launch_unchecked(
            &client,
            cubes_of(n_words),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_bytes.clone(), n_words),
            BufferArg::from_raw_parts(h_bi.clone(), fx.trie.block_index.len()),
            BufferArg::from_raw_parts(h_bm.clone(), fx.trie.blocks_m.len()),
            BufferArg::from_raw_parts(h_bc.clone(), fx.trie.blocks_c.len()),
            BufferArg::from_raw_parts(h_fl.clone(), n_words),
            BufferArg::from_raw_parts(h_sm.clone(), n),
            BufferArg::from_raw_parts(h_gi.clone(), n),
            BufferArg::from_raw_parts(h_hgt.clone(), n),
            BufferArg::from_raw_parts(h_cslot.clone(), n),
            crate::glyph_trie::BLOCK_SHIFT,
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
            BufferArg::from_raw_parts(h_ir.clone(), ir.len()),
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
            BufferArg::from_raw_parts(h_tc.clone(), n_tiles),
            BufferArg::from_raw_parts(h_up.clone(), n_tiles * units),
            units,
            rake,
            log,
        );
        count_spine::launch_unchecked(
            &client,
            CubeCount::new_single(),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(h_tc.clone(), n_tiles),
            BufferArg::from_raw_parts(h_xc.clone(), n_tiles),
            BufferArg::from_raw_parts(h_total.clone(), 1),
            units,
            log,
        );
    }
    let total_bytes = client.read_one(h_total.clone()).expect("read candidate count");
    let c = bytemuck::cast_slice::<u8, u32>(&total_bytes)[0] as usize;
    // Level tables: K = ceil(log2(C+1)) archived levels, stride C+1.
    let kmax = ((c as u32 + 1).next_power_of_two().trailing_zeros()) as u32;
    let stride = c + 1;
    let h_lvl = client.empty((kmax as usize * (c + 1)).max(1) * 4);
    let h_parent = client.empty((c + 1) * 4);
    let h_parent_b = client.empty((c + 1) * 4);
    let mut d0 = vec![1u32; c + 1];
    d0[c] = 0;
    let h_d0 = client.create_from_slice(bytemuck::cast_slice(&d0));
    let h_d_a = client.empty((c + 1) * 4);
    let h_d_b = client.empty((c + 1) * 4);
    let h_roots = client.create_from_slice(bytemuck::cast_slice(&vec![c as u32; fx.items.len()]));
    // Rotation state for the rank loop. h_d0 (the pristine [1,1,..,0] seed)
    // is an INPUT forever — the ping-pong alternates between h_d_a/h_d_b and
    // parent/parent_b, never writing d0. A naive two-buffer ping-pong writes
    // round 1's depths into d0, and every REPLAY (the bench's sample loop)
    // would then seed itself with the previous run's depths.
    let mut sp = h_parent.clone();
    let mut sd = h_d0.clone();
    unsafe {
        cand_scatter::launch_unchecked(
            &client,
            tiles_grid(n_tiles),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(h_cslot.clone(), n),
            BufferArg::from_raw_parts(h_xc.clone(), n_tiles),
            BufferArg::from_raw_parts(h_up.clone(), n_tiles * units),
            BufferArg::from_raw_parts(h_hp.clone(), c),
            units,
            rake,
        );
        jump_build::launch_unchecked(
            &client,
            cubes_of(c + 1),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_hp.clone(), c),
            BufferArg::from_raw_parts(h_cend.clone(), n),
            BufferArg::from_raw_parts(h_ir.clone(), ir.len()),
            BufferArg::from_raw_parts(h_total.clone(), 1),
            BufferArg::from_raw_parts(h_parent.clone(), c + 1),
        );
        for k in 0..kmax {
            let tp = if k % 2 == 0 { h_parent_b.clone() } else { h_parent.clone() };
            let td = if k % 2 == 0 { h_d_a.clone() } else { h_d_b.clone() };
            rank_step::launch_unchecked(
                &client,
                cubes_of(c + 1),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(sp.clone(), c + 1),
                BufferArg::from_raw_parts(sd.clone(), c + 1),
                BufferArg::from_raw_parts(tp.clone(), c + 1),
                BufferArg::from_raw_parts(td.clone(), c + 1),
                BufferArg::from_raw_parts(h_lvl.clone(), kmax as usize * (c + 1)),
                k as usize,
                stride,
            );
            sp = tp;
            sd = td;
        }
        item_roots::launch_unchecked(
            &client,
            cubes_of(fx.items.len().max(1)),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_hp.clone(), c),
            BufferArg::from_raw_parts(h_total.clone(), 1),
            BufferArg::from_raw_parts(h_ir.clone(), ir.len()),
            BufferArg::from_raw_parts(h_ic.clone(), ic.len()),
            BufferArg::from_raw_parts(h_roots.clone(), fx.items.len()),
        );
        cluster_mark::launch_unchecked(
            &client,
            cubes_of(c.max(1)),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_hp.clone(), c),
            BufferArg::from_raw_parts(sd.clone(), c + 1),
            BufferArg::from_raw_parts(h_lvl.clone(), kmax as usize * (c + 1)),
            BufferArg::from_raw_parts(h_total.clone(), 1),
            BufferArg::from_raw_parts(h_roots.clone(), fx.items.len()),
            BufferArg::from_raw_parts(h_ir.clone(), ir.len()),
            BufferArg::from_raw_parts(h_cend.clone(), n),
            BufferArg::from_raw_parts(h_cslot.clone(), n),
            BufferArg::from_raw_parts(h_sm.clone(), n),
            BufferArg::from_raw_parts(h_gi.clone(), n),
            BufferArg::from_raw_parts(h_fl.clone(), n_words),
            kmax as usize,
            stride,
            bitmap_advance,
        );
    }
    let fl_bytes = client.read_one(h_fl).expect("read fl");
    let sm_bytes = client.read_one(h_sm).expect("read sm");
    let dt = t0.elapsed();
    if std::env::var_os("GLYPH_CHAIN_DEBUG").is_some() {
        let hp_b = client.read_one(h_hp.clone()).expect("hp");
        let pa_b = client.read_one(h_parent.clone()).expect("parent");
        let lv_b = client.read_one(h_lvl.clone()).expect("lvl");
        let d_b = client.read_one(sd.clone()).expect("depth");
        let ro_b = client.read_one(h_roots.clone()).expect("roots");
        let hpv: &[u32] = bytemuck::cast_slice(&hp_b);
        let pav: &[u32] = bytemuck::cast_slice(&pa_b);
        let lv: &[u32] = bytemuck::cast_slice(&lv_b);
        let dv: &[u32] = bytemuck::cast_slice(&d_b);
        let rv: &[u32] = bytemuck::cast_slice(&ro_b);
        println!(
            "  dbg graph C={c} kmax={kmax} hp={hpv:?} parent={pav:?} T={dv:?} roots={rv:?} lvl={lv:?}"
        );
    }
    let flw: &[u32] = bytemuck::cast_slice(&fl_bytes);
    let sm: &[f32] = bytemuck::cast_slice(&sm_bytes);
    if std::env::var_os("GLYPH_CHAIN_DEBUG").is_some() {
        let cs = client.read_one(h_cslot).expect("read cslot");
        let ce = client.read_one(h_cend).expect("read cend");
        let csv: &[u32] = bytemuck::cast_slice(&cs);
        let cev: &[u32] = bytemuck::cast_slice(&ce);
        for i in 0..n {
            if csv[i] != 0 {
                println!("  dbg cslot[{i}]={} cend[{i}]={}", csv[i], cev[i]);
            }
        }
        let stride = 2 + seq_max as usize;
        println!("  dbg seq table stride {stride}, {} entries", seq.len() / stride);
        for e in 0..(seq.len() / stride).min(4) {
            let o = e * stride;
            let l = seq[o + 1] as usize;
            println!("  dbg entry {e}: slot {} len {} cps {:?}", seq[o], l, &seq[o + 2..o + 2 + l]);
        }
    }

    let mut bad = 0usize;
    for id in 0..n {
        let want_f = slots.flags(id) & 0xFF;
        let got_f = (flw[id >> 2] >> (((id & 3) * 8) as u32)) & 0xFF;
        if want_f != got_f {
            if bad < 8 {
                println!("  MISMATCH byte {id} flags: cpu {want_f:#04x} gpu {got_f:#04x}");
            }
            bad += 1;
        }
        if slots.advance(id).to_bits() != sm[id].to_bits() {
            if bad < 8 {
                println!(
                    "  MISMATCH byte {id} advance: cpu {:e} gpu {:e}",
                    slots.advance(id),
                    sm[id]
                );
            }
            bad += 1;
        }
    }
    println!(
        "cubecl-cluster-check: {} ({} B, {} items, {} seq entries) — {} lane mismatches; decode+cluster+readbacks {:?} (smoke timing only)",
        fx.name,
        n,
        fx.items.len(),
        seq.len() / (2 + seq_max as usize),
        bad,
        dt
    );
    if bad > 0 {
        eprintln!("cubecl-cluster-check FAIL: {bad} lane mismatches vs decode_all + resolve_clusters");
        std::process::exit(1);
    }
    println!("cubecl-cluster-check PASS: flags + advance bit-exact, cluster trailers and head advances included");
    std::process::exit(0);
}
