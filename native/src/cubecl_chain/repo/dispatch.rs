//! GPU compute kernel launch dispatch for Block 1, candidate resolution, and Block 2.

use cubecl::client::{Client, ProfileWindow};
use cubecl::prelude::*;
use cubecl::server::Handle;

use super::super::cluster::{
    cand_scatter, cluster_mark, cluster_probe, count_spine, count_tile, item_roots, jump_build,
    rank_step,
};
use super::super::decode::decode;
use super::super::position::{derive_stride, extent_pair, resolve_x};
use super::super::scan::{apply, spine_scan, tile_scan};
use super::super::tail::{
    extent_fold, item_totals, ordinal_scatter, sv_count_spine, sv_count_tile,
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

pub(crate) struct ClusterCandidateAllocs {
    pub h_lvl: Handle,
    pub h_parent: Handle,
    pub h_parent_b: Handle,
    pub h_d0: Handle,
    pub h_d_a: Handle,
    pub h_d_b: Handle,
    pub h_roots: Handle,
    pub kmax: usize,
    pub cstride: usize,
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
        prof.begin(client, "decode");
        decode::launch_unchecked(
            client,
            cubes_of(n_words),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(buf.h_bytes.as_ref().unwrap().clone(), n_words),
            BufferArg::from_raw_parts(buf.h_bi.as_ref().unwrap().clone(), buf.bi_len),
            BufferArg::from_raw_parts(buf.h_bm.as_ref().unwrap().clone(), buf.bm_len),
            BufferArg::from_raw_parts(buf.h_bc.as_ref().unwrap().clone(), buf.bc_len),
            BufferArg::from_raw_parts(buf.h_fl.clone(), n_words),
            BufferArg::from_raw_parts(buf.h_sm.clone(), n),
            BufferArg::from_raw_parts(buf.h_gi.clone(), n),
            BufferArg::from_raw_parts(buf.h_hgt.clone(), n),
            BufferArg::from_raw_parts(buf.h_cslot.as_ref().unwrap().clone(), n),
            buf.bshift,
        );
        prof.end(client, "decode");

        prof.begin(client, "cluster_probe");
        cluster_probe::launch_unchecked(
            client,
            cubes_of(n_words),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(buf.h_bytes.as_ref().unwrap().clone(), n_words),
            BufferArg::from_raw_parts(buf.h_bmap.as_ref().unwrap().clone(), inputs.bitmap.len()),
            BufferArg::from_raw_parts(buf.h_poff.as_ref().unwrap().clone(), inputs.poff.len()),
            BufferArg::from_raw_parts(buf.h_pval.as_ref().unwrap().clone(), inputs.pval.len()),
            BufferArg::from_raw_parts(buf.h_seq.as_ref().unwrap().clone(), inputs.seq.len()),
            BufferArg::from_raw_parts(buf.h_ir.clone(), inputs.ir.len()),
            BufferArg::from_raw_parts(buf.h_ic.as_ref().unwrap().clone(), inputs.ic.len()),
            BufferArg::from_raw_parts(buf.h_fl.clone(), n_words),
            BufferArg::from_raw_parts(buf.h_sm.clone(), n),
            BufferArg::from_raw_parts(buf.h_gi.clone(), n),
            BufferArg::from_raw_parts(buf.h_cslot.as_ref().unwrap().clone(), n),
            BufferArg::from_raw_parts(buf.h_cend.as_ref().unwrap().clone(), n),
            inputs.seq_max,
        );
        prof.end(client, "cluster_probe");

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

/// Reads back candidate count `c` and allocates cluster structures when `c > 0`.
pub(crate) fn resolve_candidates(
    client: &Client,
    buf: &ChainBuffers,
    item_count: usize,
    prof: &mut ChainProfiler,
) -> (usize, Option<ClusterCandidateAllocs>) {
    let t_csync = std::time::Instant::now();
    let tb = client.read_one(buf.h_ctotal.as_ref().unwrap().clone()).expect("candidate count");
    let c = bytemuck::cast_slice::<u8, u32>(&tb)[0] as usize;
    prof.record_sync("sync:cand_readback", t_csync.elapsed());

    let allocs = if c > 0 {
        let kmax = ((c as u32 + 1).next_power_of_two().trailing_zeros()) as usize;
        let cstride = c + 1;
        let h_lvl = client.empty((kmax * (c + 1)).max(1) * 4);
        let h_parent = client.empty((c + 1) * 4);
        let h_parent_b = client.empty((c + 1) * 4);
        let mut d0 = vec![1u32; c + 1];
        d0[c] = 0;
        let h_d0 = client.create_from_slice(bytemuck::cast_slice(&d0));
        let h_d_a = client.empty((c + 1) * 4);
        let h_d_b = client.empty((c + 1) * 4);
        let h_roots = client.create_from_slice(bytemuck::cast_slice(&vec![c as u32; item_count]));
        Some(ClusterCandidateAllocs {
            h_lvl,
            h_parent,
            h_parent_b,
            h_d0,
            h_d_a,
            h_d_b,
            h_roots,
            kmax,
            cstride,
        })
    } else {
        None
    };

    (c, allocs)
}

/// Executes Block 2: scan, resolve_x, extent_fold, survivor dual-Blelloch, and item_totals.
#[allow(clippy::too_many_arguments)]
pub(crate) fn launch_block2(
    client: &Client,
    n: usize,
    item_count: usize,
    inputs: &ChainHostInputs,
    buf: &ChainBuffers,
    c: usize,
    cluster_allocs: Option<&ClusterCandidateAllocs>,
    prof: &mut ChainProfiler,
) {
    let n_tiles = inputs.n_tiles;
    let n_words = inputs.n_words;
    let units = inputs.units;
    let rake = inputs.rake;
    let log = inputs.log;
    let rspan = inputs.rspan;

    unsafe {
        prof.block_begin(client, "block2");
        if let Some(ca) = cluster_allocs {
            let kmax = ca.kmax;
            let cstride = ca.cstride;
            prof.begin(client, "cand_scatter");
            cand_scatter::launch_unchecked(
                client,
                tiles_grid(n_tiles),
                CubeDim::new_1d(units as u32),
                BufferArg::from_raw_parts(buf.h_cslot.as_ref().unwrap().clone(), n),
                BufferArg::from_raw_parts(buf.h_cxc.as_ref().unwrap().clone(), n_tiles),
                BufferArg::from_raw_parts(buf.h_cup.as_ref().unwrap().clone(), n_tiles * units),
                BufferArg::from_raw_parts(buf.h_hp.as_ref().unwrap().clone(), c),
                units,
                rake,
            );
            prof.end(client, "cand_scatter");

            prof.begin(client, "jump_build");
            jump_build::launch_unchecked(
                client,
                cubes_of(c + 1),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(buf.h_hp.as_ref().unwrap().clone(), c),
                BufferArg::from_raw_parts(buf.h_cend.as_ref().unwrap().clone(), n),
                BufferArg::from_raw_parts(buf.h_ir.clone(), inputs.ir.len()),
                BufferArg::from_raw_parts(buf.h_ctotal.as_ref().unwrap().clone(), 1),
                BufferArg::from_raw_parts(ca.h_parent.clone(), c + 1),
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
                    cubes_of(c + 1),
                    CubeDim::new_1d(256),
                    BufferArg::from_raw_parts(sp.clone(), c + 1),
                    BufferArg::from_raw_parts(sd.clone(), c + 1),
                    BufferArg::from_raw_parts(tp.clone(), c + 1),
                    BufferArg::from_raw_parts(td.clone(), c + 1),
                    BufferArg::from_raw_parts(ca.h_lvl.clone(), kmax * (c + 1)),
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
                BufferArg::from_raw_parts(buf.h_hp.as_ref().unwrap().clone(), c),
                BufferArg::from_raw_parts(buf.h_ctotal.as_ref().unwrap().clone(), 1),
                BufferArg::from_raw_parts(buf.h_ir.clone(), inputs.ir.len()),
                BufferArg::from_raw_parts(buf.h_ic.as_ref().unwrap().clone(), inputs.ic.len()),
                BufferArg::from_raw_parts(ca.h_roots.clone(), item_count),
            );
            prof.end(client, "item_roots");

            prof.begin(client, "cluster_mark");
            cluster_mark::launch_unchecked(
                client,
                cubes_of(c.max(1)),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(buf.h_hp.as_ref().unwrap().clone(), c),
                BufferArg::from_raw_parts(sd.clone(), c + 1),
                BufferArg::from_raw_parts(ca.h_lvl.clone(), kmax * (c + 1)),
                BufferArg::from_raw_parts(buf.h_ctotal.as_ref().unwrap().clone(), 1),
                BufferArg::from_raw_parts(ca.h_roots.clone(), item_count),
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
            BufferArg::from_raw_parts(buf.h_wm.as_ref().unwrap().clone(), n),
            BufferArg::from_raw_parts(buf.h_wc.clone(), n),
            BufferArg::from_raw_parts(buf.h_otb.as_ref().unwrap().clone(), n),
            BufferArg::from_raw_parts(buf.h_rmax.as_ref().unwrap().clone(), item_count),
            BufferArg::from_raw_parts(buf.h_xmax.as_ref().unwrap().clone(), item_count),
            units,
            rake,
            log,
            false,
        );
        prof.end(client, "apply");

        prof.begin(client, "extent_pair");
        extent_pair::launch_unchecked(
            client,
            cubes_of(n),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(buf.h_sm.clone(), n),
            BufferArg::from_raw_parts(buf.h_fl.clone(), n_words),
            BufferArg::from_raw_parts(buf.h_lc.as_ref().unwrap().clone(), n * LC_STRIDE),
            BufferArg::from_raw_parts(buf.h_plan.as_ref().unwrap().clone(), inputs.walk_plan.len()),
            BufferArg::from_raw_parts(buf.h_extent.as_ref().unwrap().clone(), item_count * 2),
        );
        prof.end(client, "extent_pair");

        prof.begin(client, "derive_stride");
        derive_stride::launch_unchecked(
            client,
            cubes_of(item_count.max(1)),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(buf.h_extent.as_ref().unwrap().clone(), item_count * 2),
            BufferArg::from_raw_parts(buf.h_ie.as_ref().unwrap().clone(), inputs.ie.len()),
            BufferArg::from_raw_parts(buf.h_gap.as_ref().unwrap().clone(), item_count),
            BufferArg::from_raw_parts(buf.h_strides.as_ref().unwrap().clone(), item_count * 2),
        );
        prof.end(client, "derive_stride");

        prof.begin(client, "resolve_x");
        resolve_x::launch_unchecked(
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
            BufferArg::from_raw_parts(buf.h_wm.as_ref().unwrap().clone(), n),
            BufferArg::from_raw_parts(buf.h_rmax.as_ref().unwrap().clone(), item_count),
            BufferArg::from_raw_parts(buf.h_xmax.as_ref().unwrap().clone(), item_count),
            BufferArg::from_raw_parts(buf.h_strides.as_ref().unwrap().clone(), item_count * 2),
            256,
            rspan,
        );
        prof.end(client, "resolve_x");

        let rake_e = 32usize;
        prof.begin(client, "extent_fold");
        extent_fold::launch_unchecked(
            client,
            tiles_grid(n.div_ceil(units * rake_e).max(1)),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(buf.h_fl.clone(), n_words),
            BufferArg::from_raw_parts(buf.h_lm.clone(), n * LM_STRIDE),
            BufferArg::from_raw_parts(buf.h_sm.clone(), n),
            BufferArg::from_raw_parts(buf.h_hgt.clone(), n),
            BufferArg::from_raw_parts(buf.h_gi.clone(), n),
            BufferArg::from_raw_parts(buf.h_ir.clone(), inputs.ir.len()),
            BufferArg::from_raw_parts(buf.h_ext.clone(), item_count * EXT_STRIDE),
            units,
            rake_e,
        );
        prof.end(client, "extent_fold");

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

        prof.begin(client, "ordinal_scatter");
        ordinal_scatter::launch_unchecked(
            client,
            tiles_grid(n_tiles),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(buf.h_fl.clone(), n_words),
            BufferArg::from_raw_parts(buf.h_gi.clone(), n),
            BufferArg::from_raw_parts(buf.h_sxc.as_ref().unwrap().clone(), n_tiles),
            BufferArg::from_raw_parts(buf.h_sup.as_ref().unwrap().clone(), n_tiles * units),
            BufferArg::from_raw_parts(buf.h_sv.clone(), n),
            units,
            rake,
        );
        prof.end(client, "ordinal_scatter");

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
        prof.block_end(client, "block2");
    }
}
