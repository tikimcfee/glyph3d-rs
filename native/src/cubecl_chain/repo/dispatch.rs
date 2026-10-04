//! GPU compute kernel launch dispatch for Block 1, candidate resolution, and Block 2.

use cubecl::client::{Client, ProfileWindow};
use cubecl::prelude::*;

use super::super::cluster::{
    cand_scatter, cluster_mark, count_spine, count_tile, item_roots, jump_build,
    rank_step,
};
use super::super::decode::decode_probe;
use super::super::position::{extent_pair, resolve_x_fused};
use super::super::scan::{apply, spine_scan, tile_scan};
use super::super::tail::{
    item_totals, sv_count_spine, sv_count_tile,
    EXT_STRIDE,
};
use super::super::{IM_STRIDE, LC_STRIDE, LM_STRIDE, PARTIAL_COUNT_STRIDE};
use super::buffers::ChainBuffers;
use super::prep::ChainHostInputs;

#[inline]
pub(crate) fn cubes_of(threads: usize) -> CubeCount {
    let cubes = threads.div_ceil(256);
    CubeCount::Static(cubes.min(65535) as u32, cubes.div_ceil(65535) as u32, 1)
}

#[inline]
pub(crate) fn tiles_grid(tiles: usize) -> CubeCount {
    CubeCount::Static(tiles.min(65535) as u32, tiles.div_ceil(65535) as u32, 1)
}

/// Profiler instrument for per-stage GPU timing windows.
pub(crate) struct ChainProfiler {
    pub prof_ok: bool,
    pub prof_stage_on: bool,
    pub prof_blocks: bool,
    pub sync_prof: bool,
    pub rows: Vec<(String, std::time::Duration)>,
    pub missing: usize,
    pub timing: Option<String>,
    t0: Vec<std::time::Instant>,
    w: Option<ProfileWindow>,
    bw: Option<ProfileWindow>,
}

impl ChainProfiler {
    pub fn new() -> Self {
        let prof_mode = std::env::var("GLYPH_CHAIN_PROF").unwrap_or_default();
        let prof = !prof_mode.is_empty();
        let prof_stage_on = matches!(prof_mode.as_str(), "1" | "stages");
        let prof_blocks = prof_mode == "blocks";
        let sync_prof = std::env::var_os("GLYPH_CHAIN_SYNC").is_some();
        Self {
            prof_ok: prof,
            prof_stage_on,
            prof_blocks,
            sync_prof,
            rows: Vec::new(),
            missing: 0,
            timing: None,
            t0: Vec::new(),
            w: None,
            bw: None,
        }
    }

    pub fn begin(&mut self, client: &Client, name: &'static str) {
        let _ = name;
        self.w = if self.prof_ok && self.prof_stage_on {
            match client.profile_start() {
                Ok(w) => Some(w),
                Err(e) => {
                    eprintln!("chain-prof: profile_start failed ({e}); stages run unwindowed");
                    self.prof_ok = false;
                    None
                }
            }
        } else {
            None
        };
        self.t0.push(std::time::Instant::now());
    }

    pub fn end(&mut self, client: &Client, name: &'static str) {
        let enq = self.t0.pop().expect("prof end without begin").elapsed();
        if self.prof_ok && self.prof_stage_on {
            self.rows.push((format!("enq:{name}"), enq));
        }
        if let Some(w) = self.w.take() {
            let dur = client.profile_end(w).expect("profile_end");
            if self.timing.is_none() {
                self.timing = Some(format!("{}", dur.timing_method()));
            }
            match pollster::block_on(dur.resolve()) {
                Some(ticks) => self.rows.push((name.to_string(), ticks.duration())),
                None => self.missing += 1,
            }
        }
    }

    pub fn block_begin(&mut self, client: &Client, name: &'static str) {
        let _ = name;
        self.bw = if self.prof_ok && self.prof_blocks {
            match client.profile_start() {
                Ok(w) => Some(w),
                Err(e) => {
                    eprintln!("chain-prof: profile_start failed ({e}); blocks run unwindowed");
                    self.prof_ok = false;
                    None
                }
            }
        } else {
            None
        };
    }

    pub fn block_end(&mut self, client: &Client, name: &'static str) {
        if let Some(w) = self.bw.take() {
            let dur = client.profile_end(w).expect("profile_end");
            if self.timing.is_none() {
                self.timing = Some(format!("{}", dur.timing_method()));
            }
            match pollster::block_on(dur.resolve()) {
                Some(ticks) => self.rows.push((name.to_string(), ticks.duration())),
                None => self.missing += 1,
            }
        }
    }

    pub fn record_sync(&mut self, name: &'static str, dur: std::time::Duration) {
        if self.prof_ok {
            self.rows.push((name.to_string(), dur));
        }
        if self.sync_prof {
            eprintln!("chain-sync: {name} wall {dur:?}");
        }
    }

    pub fn print_summary(&self) {
        if !self.prof_ok && self.rows.is_empty() {
            return;
        }
        if !self.prof_ok {
            eprintln!("chain-prof: GPU windows were unavailable — only the sync rows carry timings");
        }
        let sum: std::time::Duration = self.rows.iter().map(|(_, d)| *d).sum();
        eprintln!(
            "chain-prof: timing={} — {} rows, {} missing windows (fenced per stage; the SUM is the price table, the spans keep the unfused walls)",
            self.timing.as_deref().unwrap_or("none"),
            self.rows.len(),
            self.missing
        );
        for (name, d) in &self.rows {
            eprintln!("  {name:<24} {:>9.3}ms", d.as_secs_f64() * 1e3);
        }
        eprintln!("  {:<24} {:>9.3}ms", "SUM", sum.as_secs_f64() * 1e3);
    }
}

/// Executes Block 1: decode, cluster_probe, cand_count_tile, and cand_count_spine.
pub(crate) fn launch_block1(
    client: &Client,
    n: usize,
    inputs: &ChainHostInputs,
    buf: &ChainBuffers,
    prof: &mut ChainProfiler,
) {
    let n_words = inputs.n_words;
    let n_tiles = inputs.n_tiles;
    let units = inputs.units;
    let rake = inputs.rake;
    let log = inputs.log;

    unsafe {
        prof.block_begin(client, "block1");
        prof.begin(client, "decode_probe");
        decode_probe::launch_unchecked(
            client,
            cubes_of(n_words),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(buf.h_bytes.as_ref().unwrap().clone(), n_words),
            BufferArg::from_raw_parts(buf.h_bi.as_ref().unwrap().clone(), buf.bi_len),
            BufferArg::from_raw_parts(buf.h_bm.as_ref().unwrap().clone(), buf.bm_len),
            BufferArg::from_raw_parts(buf.h_bc.as_ref().unwrap().clone(), buf.bc_len),
            BufferArg::from_raw_parts(buf.h_bmap.as_ref().unwrap().clone(), inputs.bitmap.len()),
            BufferArg::from_raw_parts(buf.h_poff.as_ref().unwrap().clone(), inputs.poff.len()),
            BufferArg::from_raw_parts(buf.h_pval.as_ref().unwrap().clone(), inputs.pval.len()),
            BufferArg::from_raw_parts(buf.h_seq.as_ref().unwrap().clone(), inputs.seq.len()),
            BufferArg::from_raw_parts(buf.h_ir.clone(), inputs.ir.len()),
            BufferArg::from_raw_parts(buf.h_ic.as_ref().unwrap().clone(), inputs.ic.len()),
            BufferArg::from_raw_parts(buf.h_fl.clone(), n_words),
            BufferArg::from_raw_parts(buf.h_sm.clone(), n),
            BufferArg::from_raw_parts(buf.h_gi.clone(), n),
            BufferArg::from_raw_parts(buf.h_hgt.clone(), n),
            BufferArg::from_raw_parts(buf.h_cslot.as_ref().unwrap().clone(), n),
            BufferArg::from_raw_parts(buf.h_cend.as_ref().unwrap().clone(), n),
            buf.bshift,
            inputs.seq_max,
        );
        prof.end(client, "decode_probe");

        prof.begin(client, "cand_count_tile");
        count_tile::launch_unchecked(
            client,
            tiles_grid(n_tiles),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(buf.h_cslot.as_ref().unwrap().clone(), n),
            BufferArg::from_raw_parts(buf.h_ctc.as_ref().unwrap().clone(), n_tiles),
            BufferArg::from_raw_parts(buf.h_cup.as_ref().unwrap().clone(), n_tiles * units),
            units,
            rake,
            log,
        );
        prof.end(client, "cand_count_tile");

        prof.begin(client, "cand_count_spine");
        count_spine::launch_unchecked(
            client,
            CubeCount::new_single(),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(buf.h_ctc.as_ref().unwrap().clone(), n_tiles),
            BufferArg::from_raw_parts(buf.h_cxc.as_ref().unwrap().clone(), n_tiles),
            BufferArg::from_raw_parts(buf.h_ctotal.as_ref().unwrap().clone(), 1),
            units,
            log,
        );
        prof.end(client, "cand_count_spine");
        prof.block_end(client, "block1");
    }
}

/// Executes Block 2 Part A: candidate resolution (if any), survivor counting (sv_count_tile, sv_count_spine), and item_totals.
pub(crate) fn launch_block2_totals(
    client: &Client,
    n: usize,
    item_count: usize,
    inputs: &ChainHostInputs,
    buf: &ChainBuffers,
    prof: &mut ChainProfiler,
) {
    let n_tiles = inputs.n_tiles;
    let n_words = inputs.n_words;
    let units = inputs.units;
    let rake = inputs.rake;
    let log = inputs.log;

    unsafe {
        prof.block_begin(client, "block2_totals");
        if let Some(ref ca) = buf.cluster_allocs {
            let kmax = ca.kmax;
            let cstride = ca.cstride;
            let c_cap = ca.c_cap;

            prof.begin(client, "cand_scatter");
            cand_scatter::launch_unchecked(
                client,
                tiles_grid(n_tiles),
                CubeDim::new_1d(units as u32),
                BufferArg::from_raw_parts(buf.h_cslot.as_ref().unwrap().clone(), n),
                BufferArg::from_raw_parts(buf.h_cxc.as_ref().unwrap().clone(), n_tiles),
                BufferArg::from_raw_parts(buf.h_cup.as_ref().unwrap().clone(), n_tiles * units),
                BufferArg::from_raw_parts(buf.h_hp.as_ref().unwrap().clone(), n),
                units,
                rake,
            );
            prof.end(client, "cand_scatter");

            prof.begin(client, "jump_build");
            jump_build::launch_unchecked(
                client,
                cubes_of(cstride),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(buf.h_hp.as_ref().unwrap().clone(), n),
                BufferArg::from_raw_parts(buf.h_cend.as_ref().unwrap().clone(), n),
                BufferArg::from_raw_parts(buf.h_ir.clone(), inputs.ir.len()),
                BufferArg::from_raw_parts(buf.h_ctotal.as_ref().unwrap().clone(), 1),
                BufferArg::from_raw_parts(ca.h_parent.clone(), cstride),
                BufferArg::from_raw_parts(ca.h_d0.clone(), cstride),
            );
            prof.end(client, "jump_build");

            prof.begin(client, "cluster_rank");
            let mut sp = ca.h_parent.clone();
            let mut sd = ca.h_d0.clone();
            for k in 0..kmax {
                let tp = if k % 2 == 0 { ca.h_parent_b.clone() } else { ca.h_parent.clone() };
                let td = if k % 2 == 0 { ca.h_d_a.clone() } else { ca.h_d_b.clone() };
                rank_step::launch_unchecked(
                    client,
                    cubes_of(cstride),
                    CubeDim::new_1d(256),
                    BufferArg::from_raw_parts(sp.clone(), cstride),
                    BufferArg::from_raw_parts(sd.clone(), cstride),
                    BufferArg::from_raw_parts(tp.clone(), cstride),
                    BufferArg::from_raw_parts(td.clone(), cstride),
                    BufferArg::from_raw_parts(ca.h_lvl.clone(), kmax * cstride),
                    k,
                    cstride,
                );
                sp = tp;
                sd = td;
            }
            prof.end(client, "cluster_rank");

            prof.begin(client, "item_roots");
            item_roots::launch_unchecked(
                client,
                cubes_of(item_count.max(1)),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(buf.h_hp.as_ref().unwrap().clone(), n),
                BufferArg::from_raw_parts(buf.h_ctotal.as_ref().unwrap().clone(), 1),
                BufferArg::from_raw_parts(buf.h_ir.clone(), inputs.ir.len()),
                BufferArg::from_raw_parts(buf.h_ic.as_ref().unwrap().clone(), inputs.ic.len()),
                BufferArg::from_raw_parts(ca.h_roots.clone(), item_count.max(1)),
            );
            prof.end(client, "item_roots");

            prof.begin(client, "cluster_mark");
            cluster_mark::launch_unchecked(
                client,
                cubes_of(c_cap),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(buf.h_hp.as_ref().unwrap().clone(), n),
                BufferArg::from_raw_parts(sd.clone(), cstride),
                BufferArg::from_raw_parts(ca.h_lvl.clone(), kmax * cstride),
                BufferArg::from_raw_parts(buf.h_ctotal.as_ref().unwrap().clone(), 1),
                BufferArg::from_raw_parts(ca.h_roots.clone(), item_count.max(1)),
                BufferArg::from_raw_parts(buf.h_ir.clone(), inputs.ir.len()),
                BufferArg::from_raw_parts(buf.h_cend.as_ref().unwrap().clone(), n),
                BufferArg::from_raw_parts(buf.h_cslot.as_ref().unwrap().clone(), n),
                BufferArg::from_raw_parts(buf.h_sm.clone(), n),
                BufferArg::from_raw_parts(buf.h_gi.clone(), n),
                BufferArg::from_raw_parts(buf.h_fl.clone(), n_words),
                kmax,
                cstride,
                inputs.bitmap_advance,
            );
            prof.end(client, "cluster_mark");
        }

        prof.begin(client, "sv_count_tile");
        sv_count_tile::launch_unchecked(
            client,
            tiles_grid(n_tiles),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(buf.h_fl.clone(), n_words),
            BufferArg::from_raw_parts(buf.h_gi.clone(), n),
            BufferArg::from_raw_parts(buf.h_ltc.as_ref().unwrap().clone(), n_tiles),
            BufferArg::from_raw_parts(buf.h_stc.as_ref().unwrap().clone(), n_tiles),
            BufferArg::from_raw_parts(buf.h_lup.as_ref().unwrap().clone(), n_tiles * units),
            BufferArg::from_raw_parts(buf.h_sup.as_ref().unwrap().clone(), n_tiles * units),
            units,
            rake,
            log,
        );
        prof.end(client, "sv_count_tile");

        prof.begin(client, "sv_count_spine");
        sv_count_spine::launch_unchecked(
            client,
            CubeCount::new_single(),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(buf.h_ltc.as_ref().unwrap().clone(), n_tiles),
            BufferArg::from_raw_parts(buf.h_stc.as_ref().unwrap().clone(), n_tiles),
            BufferArg::from_raw_parts(buf.h_lxc.as_ref().unwrap().clone(), n_tiles),
            BufferArg::from_raw_parts(buf.h_sxc.as_ref().unwrap().clone(), n_tiles),
            BufferArg::from_raw_parts(buf.h_lgrand.as_ref().unwrap().clone(), 1),
            BufferArg::from_raw_parts(buf.h_sgrand.as_ref().unwrap().clone(), 1),
            units,
            log,
        );
        prof.end(client, "sv_count_spine");

        prof.begin(client, "item_totals");
        item_totals::launch_unchecked(
            client,
            cubes_of(item_count.max(1)),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(buf.h_ir.clone(), inputs.ir.len()),
            BufferArg::from_raw_parts(buf.h_fl.clone(), n_words),
            BufferArg::from_raw_parts(buf.h_gi.clone(), n),
            BufferArg::from_raw_parts(buf.h_lxc.as_ref().unwrap().clone(), n_tiles),
            BufferArg::from_raw_parts(buf.h_sxc.as_ref().unwrap().clone(), n_tiles),
            BufferArg::from_raw_parts(buf.h_lup.as_ref().unwrap().clone(), n_tiles * units),
            BufferArg::from_raw_parts(buf.h_sup.as_ref().unwrap().clone(), n_tiles * units),
            BufferArg::from_raw_parts(buf.h_lgrand.as_ref().unwrap().clone(), 1),
            BufferArg::from_raw_parts(buf.h_sgrand.as_ref().unwrap().clone(), 1),
            BufferArg::from_raw_parts(buf.h_totals.as_ref().unwrap().clone(), item_count * 2),
            units,
            rake,
        );
        prof.end(client, "item_totals");
        prof.block_end(client, "block2_totals");
    }
}

/// Executes Block 2 Part B: scan, apply, extent_pair, derive_stride, resolve_x, extent_fold, and ordinal_scatter.
#[allow(clippy::too_many_arguments)]
pub(crate) fn launch_block2_geometry(
    client: &Client,
    n: usize,
    item_count: usize,
    inputs: &ChainHostInputs,
    buf: &ChainBuffers,
    prof: &mut ChainProfiler,
) {
    let n_tiles = inputs.n_tiles;
    let n_words = inputs.n_words;
    let units = inputs.units;
    let rake = inputs.rake;
    let log = inputs.log;
    let rspan = inputs.rspan;

    unsafe {
        prof.block_begin(client, "block2_geometry");
        prof.begin(client, "tile_scan");
        tile_scan::launch_unchecked(
            client,
            tiles_grid(n_tiles),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(buf.h_fl.clone(), n_words),
            BufferArg::from_raw_parts(buf.h_sm.clone(), n),
            BufferArg::from_raw_parts(buf.h_ir.clone(), inputs.ir.len()),
            BufferArg::from_raw_parts(buf.h_ie.as_ref().unwrap().clone(), inputs.ie.len()),
            BufferArg::from_raw_parts(buf.h_tc.as_ref().unwrap().clone(), n_tiles * PARTIAL_COUNT_STRIDE),
            BufferArg::from_raw_parts(buf.h_tm.as_ref().unwrap().clone(), n_tiles),
            units,
            rake,
            log,
        );
        prof.end(client, "tile_scan");

        prof.begin(client, "spine_scan");
        spine_scan::launch_unchecked(
            client,
            CubeCount::new_single(),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(buf.h_tc.as_ref().unwrap().clone(), n_tiles * PARTIAL_COUNT_STRIDE),
            BufferArg::from_raw_parts(buf.h_tm.as_ref().unwrap().clone(), n_tiles),
            BufferArg::from_raw_parts(buf.h_xc.as_ref().unwrap().clone(), n_tiles * PARTIAL_COUNT_STRIDE),
            BufferArg::from_raw_parts(buf.h_xm.as_ref().unwrap().clone(), n_tiles),
            units,
            log,
        );
        prof.end(client, "spine_scan");

        prof.begin(client, "apply");
        apply::launch_unchecked(
            client,
            tiles_grid(n_tiles),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(buf.h_fl.clone(), n_words),
            BufferArg::from_raw_parts(buf.h_sm.clone(), n),
            BufferArg::from_raw_parts(buf.h_lc.as_ref().unwrap().clone(), n * LC_STRIDE),
            BufferArg::from_raw_parts(buf.h_lm.clone(), n * LM_STRIDE),
            BufferArg::from_raw_parts(buf.h_ir.clone(), inputs.ir.len()),
            BufferArg::from_raw_parts(buf.h_ie.as_ref().unwrap().clone(), inputs.ie.len()),
            BufferArg::from_raw_parts(buf.h_im.as_ref().unwrap().clone(), item_count * IM_STRIDE),
            BufferArg::from_raw_parts(buf.h_xc.as_ref().unwrap().clone(), n_tiles * PARTIAL_COUNT_STRIDE),
            BufferArg::from_raw_parts(buf.h_xm.as_ref().unwrap().clone(), n_tiles),
            BufferArg::from_raw_parts(buf.h_wm.as_ref().unwrap().clone(), 1),
            BufferArg::from_raw_parts(buf.h_wc.clone(), n),
            BufferArg::from_raw_parts(buf.h_otb.as_ref().unwrap().clone(), n),
            BufferArg::from_raw_parts(buf.h_rmax.as_ref().unwrap().clone(), item_count),
            BufferArg::from_raw_parts(buf.h_xmax.as_ref().unwrap().clone(), item_count),
            units,
            rake,
            log,
            false,
            true,
        );
        prof.end(client, "apply");

        let skip_extent_pair = std::env::var_os("GLYPH_EXTENT_PAIR_GPU").is_none();
        if !skip_extent_pair {
            let rake_ep = 32usize;
            prof.begin(client, "extent_pair");
            extent_pair::launch_unchecked(
                client,
                tiles_grid(n.div_ceil(units * rake_ep).max(1)),
                CubeDim::new_1d(units as u32),
                BufferArg::from_raw_parts(buf.h_sm.clone(), n),
                BufferArg::from_raw_parts(buf.h_fl.clone(), n_words),
                BufferArg::from_raw_parts(buf.h_lc.as_ref().unwrap().clone(), n * LC_STRIDE),
                BufferArg::from_raw_parts(buf.h_plan.as_ref().unwrap().clone(), inputs.walk_plan.len()),
                BufferArg::from_raw_parts(buf.h_extent.as_ref().unwrap().clone(), item_count * 2),
                inputs.min_sw,
                inputs.uniform_sw,
                units,
                rake_ep,
            );
            prof.end(client, "extent_pair");
        }

        prof.begin(client, "resolve_x");
        resolve_x_fused::launch_unchecked(
            client,
            cubes_of(n.div_ceil(rspan)),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(buf.h_sm.clone(), n),
            BufferArg::from_raw_parts(buf.h_fl.clone(), n_words),
            BufferArg::from_raw_parts(buf.h_lm.clone(), n * LM_STRIDE),
            BufferArg::from_raw_parts(buf.h_lc.as_ref().unwrap().clone(), n * LC_STRIDE),
            BufferArg::from_raw_parts(buf.h_im.as_ref().unwrap().clone(), item_count * IM_STRIDE),
            BufferArg::from_raw_parts(buf.h_ie.as_ref().unwrap().clone(), inputs.ie.len()),
            BufferArg::from_raw_parts(buf.h_ir.clone(), inputs.ir.len()),
            BufferArg::from_raw_parts(buf.h_wc.clone(), n),
            BufferArg::from_raw_parts(buf.h_otb.as_ref().unwrap().clone(), n),
            BufferArg::from_raw_parts(buf.h_wm.as_ref().unwrap().clone(), 1),
            BufferArg::from_raw_parts(buf.h_extent.as_ref().unwrap().clone(), item_count * 2),
            BufferArg::from_raw_parts(buf.h_gap.as_ref().unwrap().clone(), item_count),
            BufferArg::from_raw_parts(buf.h_gi.clone(), n),
            BufferArg::from_raw_parts(buf.h_hgt.clone(), n),
            BufferArg::from_raw_parts(buf.h_ext.clone(), item_count * EXT_STRIDE),
            256,
            rspan,
        );
        prof.end(client, "resolve_x");
        prof.block_end(client, "block2_geometry");
    }
}

/// Pre-warms (compiles and caches) all 16 CubeCL compute shader pipelines
/// concurrently on a background thread so cold starts pay zero shader JIT latency.
pub fn prewarm_pipelines(client: &Client) {
    use super::super::tail::scatter_slots;

    let zeroes = vec![0u32; 256];
    let zeroes_bytes = bytemuck::cast_slice::<u32, u8>(&zeroes);
    let b: Vec<_> = (0..25)
        .map(|_| client.create_from_slice(zeroes_bytes))
        .collect();

    let dim_256 = CubeDim::new_1d(256);

    let units = 256usize;
    let rake = 8usize;
    let log = 8usize;
    let rspan = std::env::var("GLYPH_CHAIN_SPAN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32usize);

    unsafe {
        // 1. decode_probe
        decode_probe::launch_unchecked(
            client,
            CubeCount::Static(1, 1, 1),
            dim_256,
            BufferArg::from_raw_parts(b[0].clone(), 1),
            BufferArg::from_raw_parts(b[1].clone(), 1),
            BufferArg::from_raw_parts(b[2].clone(), 2),
            BufferArg::from_raw_parts(b[3].clone(), 2),
            BufferArg::from_raw_parts(b[4].clone(), 1),
            BufferArg::from_raw_parts(b[5].clone(), 1),
            BufferArg::from_raw_parts(b[6].clone(), 1),
            BufferArg::from_raw_parts(b[7].clone(), 1),
            BufferArg::from_raw_parts(b[8].clone(), 2),
            BufferArg::from_raw_parts(b[9].clone(), 1),
            BufferArg::from_raw_parts(b[10].clone(), 0),
            BufferArg::from_raw_parts(b[11].clone(), 1),
            BufferArg::from_raw_parts(b[12].clone(), 1),
            BufferArg::from_raw_parts(b[13].clone(), 1),
            BufferArg::from_raw_parts(b[14].clone(), 1),
            BufferArg::from_raw_parts(b[15].clone(), 1),
            8u32,
            4u32,
        );

        // 2. count_tile
        count_tile::launch_unchecked(
            client,
            CubeCount::Static(1, 1, 1),
            dim_256,
            BufferArg::from_raw_parts(b[0].clone(), 0),
            BufferArg::from_raw_parts(b[1].clone(), 1),
            BufferArg::from_raw_parts(b[2].clone(), units),
            units,
            rake,
            log,
        );

        // 3. count_spine
        count_spine::launch_unchecked(
            client,
            CubeCount::new_single(),
            dim_256,
            BufferArg::from_raw_parts(b[0].clone(), 1),
            BufferArg::from_raw_parts(b[1].clone(), 1),
            BufferArg::from_raw_parts(b[2].clone(), 1),
            units,
            log,
        );

        // 4. cand_scatter
        cand_scatter::launch_unchecked(
            client,
            CubeCount::Static(1, 1, 1),
            dim_256,
            BufferArg::from_raw_parts(b[0].clone(), 0),
            BufferArg::from_raw_parts(b[1].clone(), 1),
            BufferArg::from_raw_parts(b[2].clone(), units),
            BufferArg::from_raw_parts(b[3].clone(), 1),
            units,
            rake,
        );

        // 5. jump_build
        jump_build::launch_unchecked(
            client,
            CubeCount::Static(1, 1, 1),
            dim_256,
            BufferArg::from_raw_parts(b[0].clone(), 1),
            BufferArg::from_raw_parts(b[1].clone(), 1),
            BufferArg::from_raw_parts(b[2].clone(), 2),
            BufferArg::from_raw_parts(b[3].clone(), 1),
            BufferArg::from_raw_parts(b[4].clone(), 256),
            BufferArg::from_raw_parts(b[5].clone(), 256),
        );

        // 6. rank_step
        rank_step::launch_unchecked(
            client,
            CubeCount::Static(1, 1, 1),
            dim_256,
            BufferArg::from_raw_parts(b[0].clone(), 256),
            BufferArg::from_raw_parts(b[1].clone(), 256),
            BufferArg::from_raw_parts(b[2].clone(), 256),
            BufferArg::from_raw_parts(b[3].clone(), 256),
            BufferArg::from_raw_parts(b[4].clone(), 256),
            0,
            256,
        );

        // 7. item_roots
        item_roots::launch_unchecked(
            client,
            CubeCount::Static(1, 1, 1),
            dim_256,
            BufferArg::from_raw_parts(b[0].clone(), 1),
            BufferArg::from_raw_parts(b[1].clone(), 1),
            BufferArg::from_raw_parts(b[2].clone(), 2),
            BufferArg::from_raw_parts(b[3].clone(), 1),
            BufferArg::from_raw_parts(b[4].clone(), 1),
        );

        // 8. cluster_mark
        cluster_mark::launch_unchecked(
            client,
            CubeCount::Static(1, 1, 1),
            dim_256,
            BufferArg::from_raw_parts(b[0].clone(), 1),
            BufferArg::from_raw_parts(b[1].clone(), 1),
            BufferArg::from_raw_parts(b[2].clone(), 1),
            BufferArg::from_raw_parts(b[3].clone(), 1),
            BufferArg::from_raw_parts(b[4].clone(), 1),
            BufferArg::from_raw_parts(b[5].clone(), 2),
            BufferArg::from_raw_parts(b[6].clone(), 1),
            BufferArg::from_raw_parts(b[7].clone(), 1),
            BufferArg::from_raw_parts(b[8].clone(), 1),
            BufferArg::from_raw_parts(b[9].clone(), 1),
            BufferArg::from_raw_parts(b[10].clone(), 1),
            1,
            256,
            0.0f32,
        );

        // 9. sv_count_tile
        sv_count_tile::launch_unchecked(
            client,
            CubeCount::Static(1, 1, 1),
            dim_256,
            BufferArg::from_raw_parts(b[0].clone(), 1),
            BufferArg::from_raw_parts(b[1].clone(), 0),
            BufferArg::from_raw_parts(b[2].clone(), 1),
            BufferArg::from_raw_parts(b[3].clone(), 1),
            BufferArg::from_raw_parts(b[4].clone(), units),
            BufferArg::from_raw_parts(b[5].clone(), units),
            units,
            rake,
            log,
        );

        // 10. sv_count_spine
        sv_count_spine::launch_unchecked(
            client,
            CubeCount::new_single(),
            dim_256,
            BufferArg::from_raw_parts(b[0].clone(), 1),
            BufferArg::from_raw_parts(b[1].clone(), 1),
            BufferArg::from_raw_parts(b[2].clone(), 1),
            BufferArg::from_raw_parts(b[3].clone(), 1),
            BufferArg::from_raw_parts(b[4].clone(), 1),
            BufferArg::from_raw_parts(b[5].clone(), 1),
            units,
            log,
        );

        // 11. item_totals
        item_totals::launch_unchecked(
            client,
            CubeCount::Static(1, 1, 1),
            dim_256,
            BufferArg::from_raw_parts(b[0].clone(), 2),
            BufferArg::from_raw_parts(b[1].clone(), 1),
            BufferArg::from_raw_parts(b[2].clone(), 1),
            BufferArg::from_raw_parts(b[3].clone(), 1),
            BufferArg::from_raw_parts(b[4].clone(), 1),
            BufferArg::from_raw_parts(b[5].clone(), units),
            BufferArg::from_raw_parts(b[6].clone(), units),
            BufferArg::from_raw_parts(b[7].clone(), 1),
            BufferArg::from_raw_parts(b[8].clone(), 1),
            BufferArg::from_raw_parts(b[9].clone(), 2),
            units,
            rake,
        );

        // 12. tile_scan
        tile_scan::launch_unchecked(
            client,
            CubeCount::Static(1, 1, 1),
            dim_256,
            BufferArg::from_raw_parts(b[0].clone(), 0),
            BufferArg::from_raw_parts(b[1].clone(), 1),
            BufferArg::from_raw_parts(b[2].clone(), 2),
            BufferArg::from_raw_parts(b[3].clone(), 1),
            BufferArg::from_raw_parts(b[4].clone(), PARTIAL_COUNT_STRIDE),
            BufferArg::from_raw_parts(b[5].clone(), 1),
            units,
            rake,
            log,
        );

        // 13. spine_scan
        spine_scan::launch_unchecked(
            client,
            CubeCount::new_single(),
            dim_256,
            BufferArg::from_raw_parts(b[0].clone(), PARTIAL_COUNT_STRIDE),
            BufferArg::from_raw_parts(b[1].clone(), 1),
            BufferArg::from_raw_parts(b[2].clone(), PARTIAL_COUNT_STRIDE),
            BufferArg::from_raw_parts(b[3].clone(), 1),
            units,
            log,
        );

        // 14. apply
        apply::launch_unchecked(
            client,
            CubeCount::Static(1, 1, 1),
            dim_256,
            BufferArg::from_raw_parts(b[0].clone(), 1),
            BufferArg::from_raw_parts(b[1].clone(), 0),
            BufferArg::from_raw_parts(b[2].clone(), 2),
            BufferArg::from_raw_parts(b[3].clone(), 4),
            BufferArg::from_raw_parts(b[4].clone(), 2),
            BufferArg::from_raw_parts(b[5].clone(), 1),
            BufferArg::from_raw_parts(b[6].clone(), IM_STRIDE),
            BufferArg::from_raw_parts(b[7].clone(), PARTIAL_COUNT_STRIDE),
            BufferArg::from_raw_parts(b[8].clone(), 1),
            BufferArg::from_raw_parts(b[9].clone(), 1),
            BufferArg::from_raw_parts(b[10].clone(), 1),
            BufferArg::from_raw_parts(b[11].clone(), 1),
            BufferArg::from_raw_parts(b[12].clone(), 1),
            BufferArg::from_raw_parts(b[13].clone(), 1),
            units,
            rake,
            log,
            false,
            true,
        );

        // 15. resolve_x_fused
        resolve_x_fused::launch_unchecked(
            client,
            CubeCount::Static(1, 1, 1),
            dim_256,
            BufferArg::from_raw_parts(b[0].clone(), 1),
            BufferArg::from_raw_parts(b[1].clone(), 0),
            BufferArg::from_raw_parts(b[2].clone(), LM_STRIDE),
            BufferArg::from_raw_parts(b[3].clone(), LC_STRIDE),
            BufferArg::from_raw_parts(b[4].clone(), IM_STRIDE),
            BufferArg::from_raw_parts(b[5].clone(), 1),
            BufferArg::from_raw_parts(b[6].clone(), 2),
            BufferArg::from_raw_parts(b[7].clone(), 1),
            BufferArg::from_raw_parts(b[8].clone(), 1),
            BufferArg::from_raw_parts(b[9].clone(), 1),
            BufferArg::from_raw_parts(b[10].clone(), 2),
            BufferArg::from_raw_parts(b[11].clone(), 1),
            BufferArg::from_raw_parts(b[12].clone(), 1),
            BufferArg::from_raw_parts(b[13].clone(), 1),
            BufferArg::from_raw_parts(b[14].clone(), EXT_STRIDE),
            units,
            rspan,
        );

        // 16. scatter_slots
        scatter_slots::launch_unchecked(
            client,
            CubeCount::Static(1, 1, 1),
            dim_256,
            BufferArg::from_raw_parts(b[0].clone(), 1),
            BufferArg::from_raw_parts(b[1].clone(), 0),
            BufferArg::from_raw_parts(b[2].clone(), 2),
            BufferArg::from_raw_parts(b[3].clone(), LM_STRIDE),
            BufferArg::from_raw_parts(b[4].clone(), 1),
            BufferArg::from_raw_parts(b[5].clone(), 1),
            BufferArg::from_raw_parts(b[6].clone(), 1),
            BufferArg::from_raw_parts(b[7].clone(), 1),
            BufferArg::from_raw_parts(b[8].clone(), 1),
            BufferArg::from_raw_parts(b[9].clone(), 1),
            BufferArg::from_raw_parts(b[10].clone(), 1),
            BufferArg::from_raw_parts(b[11].clone(), 1),
            BufferArg::from_raw_parts(b[12].clone(), 1),
            BufferArg::from_raw_parts(b[13].clone(), units),
            BufferArg::from_raw_parts(b[14].clone(), 1),
            BufferArg::from_raw_parts(b[15].clone(), 1),
            units,
            rake,
        );
    }
}

