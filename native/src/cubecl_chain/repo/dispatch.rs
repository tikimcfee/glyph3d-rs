//! GPU compute kernel launch dispatch for Block 1, candidate resolution, and Block 2.

use cubecl::client::{Client, ProfileWindow};
use cubecl::prelude::*;

use super::super::cluster::{
    cand_sort, cluster_mark, item_roots, jump_build, rank_step,
};
use super::super::decode::decode_probe;
use super::super::position::apply_and_emit;
use super::super::scan::{spine_scan, tile_scan};
use super::super::tail::EXT_STRIDE;
use super::super::{ITEM_DESC_STRIDE, PARTIAL_COUNT_STRIDE};
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
    #[allow(dead_code)]
    pub sync_prof: bool,
    pub rows: Vec<(String, std::time::Duration)>,
    pub missing: usize,
    pub timing: Option<String>,
    /// `GLYPH_GPU_ISOLATE_MS`: idle gap placed before and after every stage,
    /// with a `gpu-mark:` line (wall-clock ns) at each edge. Only effective
    /// with `GLYPH_CHAIN_PROF=stages`, whose `end` already blocks on the
    /// stage's GPU completion, so the gap is real GPU idle time. This is the
    /// attribution contract for timeline samplers (tools/gpu_profile).
    isolate_gap: Option<std::time::Duration>,
    t0: Vec<std::time::Instant>,
    w: Option<ProfileWindow>,
    bw: Option<ProfileWindow>,
}

/// Wall-clock nanoseconds since the Unix epoch, for `gpu-mark:` lines.
fn unix_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

impl ChainProfiler {
    pub fn new() -> Self {
        let prof_mode = std::env::var("GLYPH_CHAIN_PROF").unwrap_or_default();
        let prof = !prof_mode.is_empty();
        let prof_stage_on = matches!(prof_mode.as_str(), "1" | "stages");
        let prof_blocks = prof_mode == "blocks";
        let sync_prof = std::env::var_os("GLYPH_CHAIN_SYNC").is_some();
        let isolate_gap = std::env::var("GLYPH_GPU_ISOLATE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|ms| *ms > 0 && prof_stage_on)
            .map(std::time::Duration::from_millis);
        Self {
            prof_ok: prof,
            prof_stage_on,
            prof_blocks,
            sync_prof,
            rows: Vec::new(),
            missing: 0,
            timing: None,
            isolate_gap,
            t0: Vec::new(),
            w: None,
            bw: None,
        }
    }

    pub fn begin(&mut self, client: &Client, name: &'static str) {
        if let Some(gap) = self.isolate_gap {
            std::thread::sleep(gap);
            eprintln!("gpu-mark: begin {name} {}", unix_nanos());
        }
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
            let t_res0 = std::time::Instant::now();
            match pollster::block_on(dur.resolve()) {
                Some(ticks) => {
                    let wall = t_res0.elapsed();
                    self.rows.push((name.to_string(), ticks.duration()));
                    self.rows.push((format!("wall:{name}"), wall));
                    if self.isolate_gap.is_some() {
                        eprintln!("gpu-mark: gpu_ns {name} {}", ticks.duration().as_nanos());
                    }
                }
                None => self.missing += 1,
            }
        }
        if let Some(gap) = self.isolate_gap {
            eprintln!("gpu-mark: end {name} {}", unix_nanos());
            std::thread::sleep(gap);
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

    #[allow(dead_code)]
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

    unsafe {
        prof.block_begin(client, "block1");
        prof.begin(client, "decode_probe");
        decode_probe::launch_unchecked(
            client,
            cubes_of(n_words),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(buf.h_bytes.as_ref().unwrap().clone(), n_words),
            BufferArg::from_raw_parts(buf.h_trie_block_indices.as_ref().unwrap().clone(), buf.trie_block_indices_len),
            BufferArg::from_raw_parts(buf.h_trie_block_metrics.as_ref().unwrap().clone(), buf.trie_block_metrics_len),
            BufferArg::from_raw_parts(buf.h_trie_block_codepoints.as_ref().unwrap().clone(), buf.trie_block_codepoints_len),
            BufferArg::from_raw_parts(buf.h_cluster_bitmap.as_ref().unwrap().clone(), inputs.bitmap.len()),
            BufferArg::from_raw_parts(buf.h_cluster_secondary_offsets.as_ref().unwrap().clone(), inputs.pair_secondary_offsets.len()),
            BufferArg::from_raw_parts(buf.h_cluster_secondary_values.as_ref().unwrap().clone(), inputs.pair_secondary_values.len()),
            BufferArg::from_raw_parts(buf.h_cluster_sequence_table.as_ref().unwrap().clone(), inputs.seq.len()),
            BufferArg::from_raw_parts(buf.h_item_record_bounds.clone(), inputs.item_record_bounds.len()),
            BufferArg::from_raw_parts(buf.h_item_cluster_enabled.as_ref().unwrap().clone(), inputs.item_cluster_enabled.len()),
            BufferArg::from_raw_parts(buf.h_glyph_flags.clone(), n_words),
            BufferArg::from_raw_parts(buf.h_advance_widths.clone(), n),
            BufferArg::from_raw_parts(buf.h_glyph_indices.clone(), n),
            BufferArg::from_raw_parts(buf.h_candidate_head_positions.as_ref().unwrap().clone(), buf.candidate_stride),
            BufferArg::from_raw_parts(buf.h_candidate_slots.as_ref().unwrap().clone(), buf.candidate_stride),
            BufferArg::from_raw_parts(buf.h_candidate_end_positions.as_ref().unwrap().clone(), buf.candidate_stride),
            BufferArg::from_raw_parts(buf.h_candidate_total.as_ref().unwrap().clone(), 1),
            buf.trie_block_shift,
            inputs.seq_max,
            buf.candidate_capacity,
        );
        prof.end(client, "decode_probe");

        if buf.cluster_allocs.is_some() {
            prof.begin(client, "cand_sort");
            cand_sort::launch_unchecked(
                client,
                CubeCount::new_single(),
                CubeDim::new_1d(32),
                BufferArg::from_raw_parts(buf.h_candidate_head_positions.as_ref().unwrap().clone(), buf.candidate_stride),
                BufferArg::from_raw_parts(buf.h_candidate_slots.as_ref().unwrap().clone(), buf.candidate_stride),
                BufferArg::from_raw_parts(buf.h_candidate_end_positions.as_ref().unwrap().clone(), buf.candidate_stride),
                BufferArg::from_raw_parts(buf.h_candidate_total.as_ref().unwrap().clone(), 1),
                buf.candidate_capacity,
            );
            prof.end(client, "cand_sort");
        }
        prof.block_end(client, "block1");
    }
}

/// Executes Block 2 Part A: candidate resolution (if any) and cluster marking.
pub(crate) fn launch_block2_totals(
    client: &Client,
    n: usize,
    item_count: usize,
    inputs: &ChainHostInputs,
    buf: &ChainBuffers,
    prof: &mut ChainProfiler,
) {
    let n_words = inputs.n_words;

    unsafe {
        prof.block_begin(client, "block2_totals");
        if let Some(ref ca) = buf.cluster_allocs {
            let kmax = ca.kmax;
            let candidate_stride = ca.candidate_stride;
            let candidate_capacity = ca.candidate_capacity;

            prof.begin(client, "jump_build");
            jump_build::launch_unchecked(
                client,
                cubes_of(candidate_stride),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(buf.h_candidate_head_positions.as_ref().unwrap().clone(), candidate_stride),
                BufferArg::from_raw_parts(buf.h_candidate_end_positions.as_ref().unwrap().clone(), candidate_stride),
                BufferArg::from_raw_parts(buf.h_item_record_bounds.clone(), inputs.item_record_bounds.len()),
                BufferArg::from_raw_parts(buf.h_candidate_total.as_ref().unwrap().clone(), 1),
                BufferArg::from_raw_parts(ca.h_parent.clone(), candidate_stride),
                BufferArg::from_raw_parts(ca.h_d0.clone(), candidate_stride),
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
                    cubes_of(candidate_stride),
                    CubeDim::new_1d(256),
                    BufferArg::from_raw_parts(sp.clone(), candidate_stride),
                    BufferArg::from_raw_parts(sd.clone(), candidate_stride),
                    BufferArg::from_raw_parts(tp.clone(), candidate_stride),
                    BufferArg::from_raw_parts(td.clone(), candidate_stride),
                    BufferArg::from_raw_parts(ca.h_lvl.clone(), kmax * candidate_stride),
                    k,
                    candidate_stride,
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
                BufferArg::from_raw_parts(buf.h_candidate_head_positions.as_ref().unwrap().clone(), candidate_stride),
                BufferArg::from_raw_parts(buf.h_candidate_total.as_ref().unwrap().clone(), 1),
                BufferArg::from_raw_parts(buf.h_item_record_bounds.clone(), inputs.item_record_bounds.len()),
                BufferArg::from_raw_parts(buf.h_item_cluster_enabled.as_ref().unwrap().clone(), inputs.item_cluster_enabled.len()),
                BufferArg::from_raw_parts(ca.h_roots.clone(), item_count.max(1)),
            );
            prof.end(client, "item_roots");

            prof.begin(client, "cluster_mark");
            cluster_mark::launch_unchecked(
                client,
                cubes_of(candidate_capacity),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(buf.h_candidate_head_positions.as_ref().unwrap().clone(), candidate_stride),
                BufferArg::from_raw_parts(sd.clone(), candidate_stride),
                BufferArg::from_raw_parts(ca.h_lvl.clone(), kmax * candidate_stride),
                BufferArg::from_raw_parts(buf.h_candidate_total.as_ref().unwrap().clone(), 1),
                BufferArg::from_raw_parts(ca.h_roots.clone(), item_count.max(1)),
                BufferArg::from_raw_parts(buf.h_item_record_bounds.clone(), inputs.item_record_bounds.len()),
                BufferArg::from_raw_parts(buf.h_candidate_end_positions.as_ref().unwrap().clone(), candidate_stride),
                BufferArg::from_raw_parts(buf.h_candidate_slots.as_ref().unwrap().clone(), candidate_stride),
                BufferArg::from_raw_parts(buf.h_advance_widths.clone(), n),
                BufferArg::from_raw_parts(buf.h_glyph_indices.clone(), n),
                BufferArg::from_raw_parts(buf.h_glyph_flags.clone(), n_words),
                kmax,
                candidate_stride,
                inputs.bitmap_advance,
            );
            prof.end(client, "cluster_mark");
        }

        prof.block_end(client, "block2_totals");
    }
}

/// Executes Block 2 Part B: tile_scan, spine_scan, and apply_and_emit.
#[allow(clippy::too_many_arguments)]
pub(crate) fn launch_block2_geometry(
    client: &Client,
    n: usize,
    item_count: usize,
    inputs: &ChainHostInputs,
    buf: &ChainBuffers,
    prof: &mut ChainProfiler,
    emit_derived: bool,
) {
    let n_tiles = inputs.n_tiles;
    let n_words = inputs.n_words;
    let threads_per_cube = inputs.threads_per_cube;
    let bytes_per_thread = inputs.bytes_per_thread;
    let log = inputs.log;

    unsafe {
        prof.block_begin(client, "block2_geometry");
        prof.begin(client, "tile_scan");
        tile_scan::launch_unchecked(
            client,
            tiles_grid(n_tiles),
            CubeDim::new_1d(threads_per_cube as u32),
            BufferArg::from_raw_parts(buf.h_glyph_flags.clone(), n_words),
            BufferArg::from_raw_parts(buf.h_advance_widths.clone(), n),
            BufferArg::from_raw_parts(buf.h_item_descriptors.as_ref().unwrap().clone(), inputs.item_descriptors.len()),
            BufferArg::from_raw_parts(buf.h_tile_counts.as_ref().unwrap().clone(), n_tiles * PARTIAL_COUNT_STRIDE),
            BufferArg::from_raw_parts(buf.h_tile_metrics.as_ref().unwrap().clone(), n_tiles),
            threads_per_cube,
            bytes_per_thread,
            log,
        );
        prof.end(client, "tile_scan");

        prof.begin(client, "spine_scan");
        spine_scan::launch_unchecked(
            client,
            CubeCount::new_single(),
            CubeDim::new_1d(threads_per_cube as u32),
            BufferArg::from_raw_parts(buf.h_tile_counts.as_ref().unwrap().clone(), n_tiles * PARTIAL_COUNT_STRIDE),
            BufferArg::from_raw_parts(buf.h_tile_metrics.as_ref().unwrap().clone(), n_tiles),
            BufferArg::from_raw_parts(buf.h_spine_counts.as_ref().unwrap().clone(), n_tiles * PARTIAL_COUNT_STRIDE),
            BufferArg::from_raw_parts(buf.h_spine_metrics.as_ref().unwrap().clone(), n_tiles),
            threads_per_cube,
            log,
        );
        prof.end(client, "spine_scan");

        prof.begin(client, "apply_and_emit");
        apply_and_emit::launch_unchecked(
            client,
            tiles_grid(n_tiles),
            CubeDim::new_1d(threads_per_cube as u32),
            BufferArg::from_raw_parts(buf.h_glyph_flags.clone(), n_words),
            BufferArg::from_raw_parts(buf.h_advance_widths.clone(), n),
            BufferArg::from_raw_parts(buf.h_item_descriptors.as_ref().unwrap().clone(), inputs.item_descriptors.len()),
            BufferArg::from_raw_parts(buf.h_spine_counts.as_ref().unwrap().clone(), n_tiles * PARTIAL_COUNT_STRIDE),
            BufferArg::from_raw_parts(buf.h_spine_metrics.as_ref().unwrap().clone(), n_tiles),
            BufferArg::from_raw_parts(buf.h_max_row_extents.as_ref().unwrap().clone(), item_count * 2),
            BufferArg::from_raw_parts(buf.h_glyph_indices.clone(), n),
            BufferArg::from_raw_parts(buf.h_item_extents.clone(), item_count * EXT_STRIDE),
            BufferArg::from_raw_parts(buf.h_per_record_semantic_colors.clone(), buf.per_record_colors_words),
            BufferArg::from_raw_parts(buf.h_segment_entry_advances.clone(), inputs.segment_entry_advances.len()),
            BufferArg::from_raw_parts(buf.h_instance_slots.clone(), buf.slots_words),
            BufferArg::from_raw_parts(buf.h_instance_tints.clone(), buf.tint_words),
            emit_derived,
            threads_per_cube,
            bytes_per_thread,
            log,
        );
        prof.end(client, "apply_and_emit");
        prof.block_end(client, "block2_geometry");
    }
}

/// Pre-warms (compiles and caches) all 15 CubeCL compute shader pipelines
/// concurrently on a background thread so cold starts pay zero shader JIT latency.
pub fn prewarm_pipelines(client: &Client) {
    let zeroes = vec![0u32; 256];
    let zeroes_bytes = bytemuck::cast_slice::<u32, u8>(&zeroes);
    let dummy_buffers: Vec<_> = (0..25)
        .map(|_| client.create_from_slice(zeroes_bytes))
        .collect();

    let dummy = |slot: usize, len: usize| unsafe { BufferArg::from_raw_parts(dummy_buffers[slot].clone(), len) };

    let dim_256 = CubeDim::new_1d(256);

    let threads_per_cube = 256usize;
    let bytes_per_thread = std::env::var("GLYPH_CHAIN_RAKE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8usize);
    let log = threads_per_cube.ilog2() as usize;

    unsafe {
        // 1. decode_probe
        decode_probe::launch_unchecked(
            client,
            CubeCount::Static(1, 1, 1),
            dim_256,
            dummy(0, 1),
            dummy(1, 1),
            dummy(2, 2),
            dummy(3, 2),
            dummy(4, 1),
            dummy(5, 1),
            dummy(6, 1),
            dummy(7, 1),
            dummy(8, 2),
            dummy(9, 1),
            dummy(10, 0),
            dummy(11, 1),
            dummy(12, 1),
            dummy(13, 1),
            dummy(14, 1),
            dummy(15, 1),
            dummy(16, 1),
            8u32,
            9u32,
            16384usize,
        );

        // 2. cand_sort
        cand_sort::launch_unchecked(
            client,
            CubeCount::new_single(),
            CubeDim::new_1d(32),
            dummy(0, 1),
            dummy(1, 1),
            dummy(2, 1),
            dummy(3, 1),
            16384usize,
        );

        // 3. jump_build
        jump_build::launch_unchecked(
            client,
            CubeCount::Static(1, 1, 1),
            dim_256,
            dummy(0, 1),
            dummy(1, 1),
            dummy(2, 2),
            dummy(3, 1),
            dummy(4, 256),
            dummy(5, 256),
        );

        // 6. rank_step (prewarm all doubling rounds for standard repo capacity)
        for k in 0..15 {
            rank_step::launch_unchecked(
                client,
                CubeCount::Static(1, 1, 1),
                dim_256,
                dummy(0, 256),
                dummy(1, 256),
                dummy(2, 256),
                dummy(3, 256),
                dummy(4, 256),
                k,
                16385,
            );
        }

        // 7. item_roots
        item_roots::launch_unchecked(
            client,
            CubeCount::Static(1, 1, 1),
            dim_256,
            dummy(0, 1),
            dummy(1, 1),
            dummy(2, 2),
            dummy(3, 1),
            dummy(4, 1),
        );

        // 8. cluster_mark
        cluster_mark::launch_unchecked(
            client,
            CubeCount::Static(1, 1, 1),
            dim_256,
            dummy(0, 1),
            dummy(1, 1),
            dummy(2, 1),
            dummy(3, 1),
            dummy(4, 1),
            dummy(5, 2),
            dummy(6, 1),
            dummy(7, 1),
            dummy(8, 1),
            dummy(9, 1),
            dummy(10, 1),
            15,
            16385,
            136.0f32,
        );

        // 9. tile_scan
        tile_scan::launch_unchecked(
            client,
            CubeCount::Static(1, 1, 1),
            dim_256,
            dummy(0, 0),
            dummy(1, 1),
            dummy(2, ITEM_DESC_STRIDE),
            dummy(3, PARTIAL_COUNT_STRIDE),
            dummy(4, 1),
            threads_per_cube,
            bytes_per_thread,
            log,
        );

        // 10. spine_scan
        spine_scan::launch_unchecked(
            client,
            CubeCount::new_single(),
            dim_256,
            dummy(0, PARTIAL_COUNT_STRIDE),
            dummy(1, 1),
            dummy(2, PARTIAL_COUNT_STRIDE),
            dummy(3, 1),
            threads_per_cube,
            log,
        );

        // 11. apply_and_emit (unified scan, position, extents, and direct slot/tint emission)
        apply_and_emit::launch_unchecked(
            client,
            CubeCount::Static(1, 1, 1),
            dim_256,
            dummy(0, 1),
            dummy(1, 0),
            dummy(2, ITEM_DESC_STRIDE),
            dummy(3, PARTIAL_COUNT_STRIDE),
            dummy(4, 1),
            dummy(5, 2),
            dummy(6, 1),
            dummy(7, EXT_STRIDE),
            dummy(8, 1),
            dummy(11, 1),
            dummy(9, 8),
            dummy(10, 2),
            false,
            threads_per_cube,
            bytes_per_thread,
            log,
        );
    }
    // Wait for the background queue to execute all prewarm launches so every
    // compute shader pipeline is fully compiled by Naga and Metal/Vulkan
    // into the device pipeline cache before prewarm returns.
    let _ = pollster::block_on(client.sync());
    let _ = client.read_one(dummy_buffers[15].clone());
}

