use cubecl::prelude::*;
use cubecl::wgpu::{AutoCompiler, AutoGraphicsApi, GraphicsApi, WgpuServer, WgpuSetup};

use crate::fold::WrapMode;
use crate::text::ResolveGlyph;

use super::cluster::{
    cand_scatter, cluster_host_inputs, cluster_mark, cluster_pair_filter, cluster_probe,
    count_spine, count_tile, item_roots, jump_build, rank_step,
};
use super::decode::decode;
use super::position::{derive_stride, extent_pair, paginate, resolve_x};
use super::scan::{apply, spine_scan, tile_scan};
use super::tail::{
    EXT_STRIDE, emit_records, item_totals, key_to_float_host, ordinal_scatter, ordered_key_host,
    scatter_slots, survivor_flags,
};
use super::{IE_STRIDE, IM_STRIDE, LC_STRIDE, LM_STRIDE, PARTIAL_COUNT_STRIDE, pack_words};

// ── the repo parity driver — phase 4, rung 3 ─────────────────────────────────
//
// `--cubecl-repo-check <dir>`: the full chain over a REAL repository — walk,
// per-file items carrying the engine path's OWN ItemParams (file_item_params,
// default origins), decode-from-bytes through the ranked cluster pass, the
// scan, resolve_x (the repo's wrap-back default keeps it live), paginate, and
// the record emitter — diffed against the ENGINE's batched records for the
// same items. Tier contract: glyph_id/row/col EXACT; advance/height/X/Y/Z
// reported as bit-deviation counts and max relative deviation, gated at the
// 1e-4 tier (the chain's f32 Blelloch reassociation vs the engine's f64
// running sums is the documented eps tier — scan-vs-fold was already eps
// there). The per-item record bases come from the ENGINE's own
// ItemPlacement.record_count, so the stream alignment itself is part of the
// fence: a divergent count shifts every later record and the exact lanes
// catch it wholesale.
/// The chain's wall-clock decomposition inside run_repo_chain — rung 5's
/// yardstick. `chain_dur` above is the whole span (and today INCLUDES the
/// emit/readback loop, which readback_dur reports separately); these five
/// carve the pre-readback part so a single number never has to answer for
/// the serial host prelude, the device acquisition, and the launches at
/// once. Wall clock, not GPU timestamps — the bench instrument owns the
/// per-dispatch device view; this one answers "where did the load go".
#[derive(Clone, Copy, Default)]
pub(crate) struct ChainPhases {
    /// Serial host prelude: the leader scan + rec_base prefix sums.
    pub prep: std::time::Duration,
    /// Atlas trie load + cluster host inputs + pair filter + item tables.
    pub tables: std::time::Duration,
    /// Device acquisition + the cubecl client. Only the no-caller-device
    /// fallback pays a construction here (`--repo-scan-only`, GPU-less
    /// checks); since rung 5a the render path shares the renderer's device
    /// and this span is near zero.
    pub init: std::time::Duration,
    /// pack_words + buffer allocation + uploads.
    pub upload: std::time::Duration,
    /// The launch block's wall time. First-launch kernel JIT hides here on
    /// a cold process; a cold/warm pair of runs separates it.
    pub dispatch: std::time::Duration,
}

/// What the chain's TAIL emits. `Records` is the check/verify shape (the
/// 8-word wire stream, read back chunked); `Instances` is the product
/// shape (the pack kernel writes 48 B GlyphInstance slots, extents and
/// totals come back item_count-sized); `Both` runs the two tails in one
/// driver pass — the fork gate's mode, so the fence sees the product tail
/// and the record tier against the same dispatches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChainMode {
    Records,
    Instances,
    Both,
}

/// The paint/group tables the instance tail needs — the two lanes
/// `compact_records_into` folds in on host (layout.rs's `Paint` doc:
/// compaction destroys the index that names a byte, so paint rides
/// THROUGH it). PerRecord lengths are host-known (one color per leader by
/// construction); Flat paint needs no count at all — the kernel picks
/// `flat_colors[it]` when `is_per_record[it]` is zero. The driver
/// cross-checks the PerRecord total against the device's own leader
/// totals and fails loud, the successor of compact's record/colors
/// length assert.
pub(crate) struct InstanceInputs {
    /// Concatenated per-record colors of the PerRecord items, walk order.
    pub per_record_colors: Vec<u32>,
    /// Per-item start inside `per_record_colors`.
    pub color_base: Vec<u32>,
    /// Per-item 1/0: take the jagged table (indexed by the record ordinal)
    /// or the flat one.
    pub is_per_record: Vec<u32>,
    /// Flat color per item.
    pub flat_colors: Vec<u32>,
    /// group_id per item — the renderer's per-file group row key.
    pub groups: Vec<u32>,
}

/// The renderer's own device, handed to the chain so both run on ONE
/// instance/adapter/device/queue — rung 5a's device merge. The wgpu handles
/// clone as Arcs, so this is a cheap by-value pass; `run_repo_chain` accepts
/// `None` and constructs a device of its own for callers that have none
/// (`--repo-scan-only`, GPU-less checks), which keeps that path exactly as it
/// was.
pub(crate) struct SharedDevice {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    /// The adapter's max_buffer_size — the endpoint's slot buffer asserts
    /// against it (one buffer holds ≤ 134M slots; chunking past that is the
    /// named follow-up).
    pub max_buffer_size: u64,
    /// Metal + MAPPABLE_PRIMARY_BUFFERS — the shared-memory forms (the
    /// tint stream's mapped readback) exist only there.
    pub host_visible_storage: bool,
}

impl SharedDevice {
    pub(crate) fn from_ctx(ctx: &crate::gpu::GpuContext) -> Self {
        Self {
            instance: ctx.instance.clone(),
            adapter: ctx.adapter.clone(),
            device: ctx.device.clone(),
            queue: ctx.queue.clone(),
            max_buffer_size: ctx.profile.max_buffer_size,
            host_visible_storage: ctx.profile.backend == wgpu::Backend::Metal
                && ctx.profile.mappable_primary_buffers,
        }
    }
}

/// The endpoint's render-bound output (note 23, E2b): the 32 B slots on
/// device, extracted from the chain's allocator, ready to bind as-is. The
/// guard keeps the pool slice from being handed to a later allocation —
/// the renderer holds it for the scene's lifetime.
pub(crate) struct SlotDevice {
    pub chunk: crate::layout::DeviceSlotChunk,
    pub keep_alive: Box<dyn std::any::Any + Send>,
}

/// THE DEVICE LOAD PATH — bytes and per-file items in, the record stream
/// out. Both `repo_check` (the fence) and `CubeclLayout` (the product)
/// call THIS function: a second copy of the driver would mean the gate no
/// longer fences the path the renderer runs, which is the entire point of
/// the cubecl-fork gate.
pub(crate) struct ChainStream {
    /// 8 u32 per record (x, y, z, advance, height, gi, row, col), items in
    /// walk order, ordinal order within items. EMPTY unless Records/Both.
    pub records: Vec<u32>,
    /// Per-item first-record index (prefix sums over the DEVICE leader
    /// totals since rung 5b — the serial CPU scan is gone from every mode).
    pub rec_base: Vec<u32>,
    pub total_records: u32,
    pub total_slots: u32,
    /// Per-item placements, decoded from the scatter's extent lanes —
    /// the same reduction `compact_records_into` does on host (page over
    /// ALL records, ink over survivors, min/max order-free). EMPTY unless
    /// Instances/Both.
    pub placements: Vec<crate::layout::ItemPlacement>,
    /// The 32 B slots on device — the endpoint form the renderer binds.
    /// Some whenever Instances/Both ran and the survivor total is nonzero.
    pub slot_device: Option<SlotDevice>,
    /// The tint stream — (glyph_id, color) per slot, slot order.
    /// seg_tint's fold input now that no host arena exists. Instances/Both.
    pub tint: crate::layout::TintStore,
    /// 8 u32 per slot — the 32 B form READ BACK to host. EMPTY unless Both
    /// (the fork gate's lane tier reads it; the product never wants it).
    pub slots: Vec<u32>,
    /// The ranked chain's candidate count (diagnostics).
    pub candidates: usize,
    pub chain_dur: std::time::Duration,
    pub readback_dur: std::time::Duration,
    pub phases: ChainPhases,
}

#[allow(clippy::too_many_lines)]
pub(crate) fn run_repo_chain(
    device: Option<&SharedDevice>,
    bytes: &[u8],
    items: &[crate::fold::Item],
    inputs: &InstanceInputs,
    mode: ChainMode,
) -> ChainStream {
    let item_count = items.len();
    let n: usize = bytes.len();
    let fis = items;
    // ── the chain side ────────────────────────────────────────────────────
    // The span decomposition mirrors the Instant pairs below exactly —
    // ChainPhases stays the print contract, the spans are the programmatic
    // instrument, and reading the same boundaries is what lets the two be
    // cross-checked before anything collapses onto either.
    let _chain = tracing::info_span!("chain", n, items = item_count, ?mode).entered();
    let t_chain0 = std::time::Instant::now();
    let sp_prep = tracing::info_span!("chain.prep").entered();
    // Rung 5b: the serial CPU leader scan is GONE from every mode — the
    // survivor pass publishes per-item leader AND survivor totals on
    // device (an item_count-sized readback; prefix sums on host), which
    // is what feeds rec_base/slot_base and the loop bounds below. The
    // ENGINE's own per-item counts still fence the alignment in the fork
    // gate's counts tier. (The ordering lesson that comment used to carry
    // stands: chain first, engine second, diff last — the product path
    // holds none of the engine's host memory, and at the 97MB repo shape
    // gigabytes held across dispatches brushed the machine's ceiling with
    // deterministically-dead dispatches as the failure mode.)
    let t_tables = std::time::Instant::now();
    drop(sp_prep);
    let sp_tables = tracing::info_span!("chain.tables").entered();

    let (units, rake) = (256usize, 8usize);
    let log = units.ilog2() as usize;
    let n_tiles = n.div_ceil(units * rake).max(1);
    let n_words = n.div_ceil(4);
    let rspan = 8usize;
    let trie = crate::atlas::TrieTable::load(&crate::atlas_dir());
    let (seq, seq_max, bitmap_advance) = match trie.cluster_table() {
        Some((s, m, a)) => (s.to_vec(), m, a),
        None => {
            eprintln!("cubecl-repo-check: atlas carries no sequence section");
            std::process::exit(1);
        }
    };
    let (bitmap, ic) = cluster_host_inputs(&seq, seq_max, fis);
    let (poff, pval) = cluster_pair_filter(&seq, seq_max);
    let mut ir = Vec::with_capacity(item_count * 2);
    let mut ie = Vec::with_capacity(item_count * IE_STRIDE);
    let mut im = Vec::with_capacity(item_count * IM_STRIDE);
    let mut page_gap_x = Vec::with_capacity(item_count);
    for item in fis {
        // Note 24 Q3: apply is compiled inline_resolve=false unconditionally
        // and resolve_x writes lm only for fold > 0 leaders — a foldless
        // item would render uninitialized lm with every gate green. No
        // caller produces one today (repo wrap_cols is fixed at 100, and no
        // CLI flag reaches it); this keeps a future caller honest.
        assert!(
            item.wrap_width > 0,
            "run_repo_chain requires folded items (wrap_width > 0)"
        );
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
        im.push(0.0f32);
        im.push(item.origin_x as f32);
        im.push((item.z_step - item.z_step as f32 as f64) as f32);
        page_gap_x.push(item.page_gap_x as f32);
    }

    let t_init = std::time::Instant::now();
    drop(sp_tables);
    let sp_init = tracing::info_span!("chain.init").entered();
    // One device when the caller has one (the renderer's, shared since rung
    // 5a — the rung-4 second device is gone from the render path); callers
    // with no GPU of their own still construct one here.
    let owned_ctx;
    let owned_dev;
    let device_ref = match device {
        Some(d) => d,
        None => {
            owned_ctx = pollster::block_on(crate::gpu::init(None));
            owned_dev = SharedDevice::from_ctx(&owned_ctx);
            &owned_dev
        }
    };
    let setup = WgpuSetup {
        instance: device_ref.instance.clone(),
        adapter: device_ref.adapter.clone(),
        device: device_ref.device.clone(),
        queue: device_ref.queue.clone(),
        backend: AutoGraphicsApi::backend(),
    };
    let cdev = cubecl::wgpu::init_device(setup, Default::default());
    let client = cubecl::Device::Wgpu(cdev).client();
    let t_upload = std::time::Instant::now();
    drop(sp_init);
    let span_upload = tracing::info_span!("chain.upload", live_bytes = tracing::field::Empty);
    let sp_upload = span_upload.enter();
    // The device-memory ledger: EVERY allocation this function makes passes
    // through these two doors, so the figure the spans report is the chain's
    // true semantic live set (cubecl's pool may hold more on top). Cell
    // because the two closures share it; this function is single-threaded.
    let live = std::cell::Cell::new(0u64);
    let alloc_empty = |size: usize| {
        live.set(live.get() + size as u64);
        client.empty(size)
    };
    let alloc_upload = |bytes: &[u8]| {
        live.set(live.get() + bytes.len() as u64);
        client.create_from_slice(bytes)
    };
    let packed = pack_words(bytes);
    let (bi, bm, bc, bshift) = trie.device_tables();
    let h_bytes = alloc_upload(bytemuck::cast_slice(&packed));
    let h_bi = alloc_upload(bytemuck::cast_slice(&bi));
    let h_bm = alloc_upload(bytemuck::cast_slice(&bm));
    let h_bc = alloc_upload(bytemuck::cast_slice(&bc));
    let h_seq = alloc_upload(bytemuck::cast_slice(&seq));
    let h_bmap = alloc_upload(bytemuck::cast_slice(&bitmap));
    let h_poff = alloc_upload(bytemuck::cast_slice(&poff));
    let h_pval = alloc_upload(bytemuck::cast_slice(&pval));
    let h_ir = alloc_upload(bytemuck::cast_slice(&ir));
    let h_ic = alloc_upload(bytemuck::cast_slice(&ic));
    let h_ie = alloc_upload(bytemuck::cast_slice(&ie));
    let h_im = alloc_upload(bytemuck::cast_slice(&im));
    let h_gap = alloc_upload(bytemuck::cast_slice(&page_gap_x));
    let h_fl = alloc_empty(n_words * 4);
    let h_sm = alloc_empty(n * 4);
    let h_gi = alloc_empty(n * 4);
    let h_hgt = alloc_empty(n * 4);
    let h_cslot = alloc_upload(bytemuck::cast_slice(&vec![0u32; n]));
    let h_cend = alloc_empty(n * 4);
    let h_tc = alloc_empty(n_tiles * PARTIAL_COUNT_STRIDE * 4);
    let h_tm = alloc_empty(n_tiles * 4);
    let h_xc = alloc_empty(n_tiles * PARTIAL_COUNT_STRIDE * 4);
    let h_xm = alloc_empty(n_tiles * 4);
    let h_lc = alloc_empty(n * LC_STRIDE * 4);
    let h_wm = alloc_empty(n * 4);
    let h_wc = alloc_empty(n * 4);
    let h_otb = alloc_empty(n * 4);
    let h_lm = alloc_empty(n * LM_STRIDE * 4);
    let h_strides = alloc_empty(item_count * 8);
    let h_rmax = alloc_upload(bytemuck::cast_slice(&vec![0u32; item_count]));
    let h_xmax = alloc_upload(bytemuck::cast_slice(&vec![0u32; item_count]));
    // The extent pair + walk plan (the width pre-resolved by fold.rs's
    // rule — see extent_pair's header).
    let h_extent =
        alloc_upload(bytemuck::cast_slice(&vec![0x8000_0000u32; item_count * 2]));
    let mut walk_plan: Vec<u32> = Vec::with_capacity(item_count * 3);
    for (i, item) in fis.iter().enumerate() {
        let width = if item.wrap_width > 0 {
            item.wrap_width
        } else if item.has_page {
            item.page_cols
        } else {
            0
        };
        walk_plan.push(ir[i * 2]);
        walk_plan.push(ir[i * 2 + 1]);
        walk_plan.push(width as u32);
    }
    let h_plan = alloc_upload(bytemuck::cast_slice(&walk_plan));
    let h_ctc = alloc_empty(n_tiles * 4);
    let h_cup = alloc_empty(n_tiles * units * 4);
    let h_cxc = alloc_empty(n_tiles * 4);
    let h_ctotal = alloc_empty(4);
    let h_hp = alloc_empty(n * 4);
    // ── rung 5b: the survivor pass's buffers ─────────────────────────────
    // Two byte flags (leader, survivor) that the PROVEN count_tile /
    // count_spine machinery scans; the ordinal scatter then overwrites
    // them in place with the per-byte exclusive ordinals (lv/sv — see its
    // header), so the flags cost no extra resident memory after the pass.
    let h_lflag = alloc_empty(n * 4);
    let h_sflag = alloc_empty(n * 4);
    let h_ltc = alloc_empty(n_tiles * 4);
    let h_stc = alloc_empty(n_tiles * 4);
    let h_lup = alloc_empty(n_tiles * units * 4);
    let h_sup = alloc_empty(n_tiles * units * 4);
    let h_lxc = alloc_empty(n_tiles * 4);
    let h_sxc = alloc_empty(n_tiles * 4);
    let h_lgrand = alloc_empty(4);
    let h_sgrand = alloc_empty(4);
    let h_ltot = alloc_empty(item_count.max(1) * 4);
    let h_stot = alloc_empty(item_count.max(1) * 4);
    // The extent lanes, SEEDED in key space exactly like the host loop
    // seeds its accumulators: page at 0.0 (over ALL records), ink at
    // ±inf (over survivors — an item with none keeps the empty extent).
    let wants_instances = matches!(mode, ChainMode::Instances | ChainMode::Both);
    let mut ext_seed = vec![0u32; item_count * EXT_STRIDE];
    if wants_instances {
        let zero_k = ordered_key_host(0.0f32);
        let inf_k = ordered_key_host(f32::INFINITY);
        let ninf_k = ordered_key_host(f32::NEG_INFINITY);
        for it in 0..item_count {
            let e = it * EXT_STRIDE;
            ext_seed[e] = zero_k;
            ext_seed[e + 1] = zero_k;
            ext_seed[e + 2] = zero_k;
            ext_seed[e + 3] = zero_k;
            ext_seed[e + 4] = inf_k;
            ext_seed[e + 5] = inf_k;
            ext_seed[e + 6] = ninf_k;
            ext_seed[e + 7] = ninf_k;
            ext_seed[e + 8] = inf_k;
            ext_seed[e + 9] = ninf_k;
        }
    }
    let h_ext = alloc_upload(bytemuck::cast_slice(&ext_seed));
    let h_pr_colors = if wants_instances {
        alloc_upload(bytemuck::cast_slice(&inputs.per_record_colors))
    } else {
        alloc_empty(4)
    };
    let h_color_base =
        alloc_upload(bytemuck::cast_slice(&inputs.color_base));
    let h_is_pr = alloc_upload(bytemuck::cast_slice(&inputs.is_per_record));
    let h_flat_colors = alloc_upload(bytemuck::cast_slice(&inputs.flat_colors));
    let h_groups = alloc_upload(bytemuck::cast_slice(&inputs.groups));
    // The record/instance buffers are fixed rolling CHUNKS, not
    // whole-corpus allocations — see the emitter's rec_first note. Their
    // sizes bind to the device totals now, so they are allocated in the
    // tail, after the survivor readback. GLYPH_RECORD_CHUNK shrinks the
    // windows (units = elements) so the fork gate crosses boundaries on
    // the standing fixture: the emitter's windowed carry arithmetic,
    // fenced on an ordinary corpus. Default keeps the buffer at ~512MB:
    // 16.7M records × 32 B. (The instance tail has NO windows since E2b —
    // the scatter writes the renderer-bound buffer directly.)
    let chunk_env: Option<usize> = std::env::var("GLYPH_RECORD_CHUNK")
        .ok()
        .and_then(|v| v.parse().ok());
    let chunk_recs_cap = chunk_env.unwrap_or(16_777_216);
    let cubes_of = |threads: usize| {
        let cubes = threads.div_ceil(256);
        CubeCount::Static(cubes.min(65535) as u32, cubes.div_ceil(65535) as u32, 1)
    };
    let tiles_grid = |tiles: usize| {
        CubeCount::Static(tiles.min(65535) as u32, tiles.div_ceil(65535) as u32, 1)
    };
    let t_dispatch = std::time::Instant::now();
    span_upload.record("live_bytes", live.get());
    drop(sp_upload);
    // The guard's drop ends the ENTER; the Span handle must drop too or the
    // span only CLOSEs at function end (spans close at refcount zero), which
    // would print this phase's close line after the whole tail.
    drop(span_upload);
    let sp_dispatch = tracing::info_span!("chain.dispatch").entered();
    // GLYPH_CHAIN_PROF=1: the bench's per-stage GPU windows, ported into the
    // PRODUCT chain (note 24 §A4.2) — every stage fenced and timestamped in
    // place, so tail.totals' absorbed execution decomposes by name. Fences
    // serialize the chain: the printed SUM is the pricing table, NOT a load
    // time (the spans keep the unfused walls). The two internal syncs ride
    // as wall-clock rows; the tail's ladder/tint/placements stay
    // span-covered. Single-shot at repo scale; replicate by re-running.
    let prof = std::env::var_os("GLYPH_CHAIN_PROF").is_some();
    let mut prof_ok = prof;
    let mut prof_rows: Vec<(&'static str, std::time::Duration)> = Vec::new();
    let mut prof_missing = 0usize;
    let mut prof_timing: Option<String> = None;
    let mut prof_w;
    macro_rules! prof {
        (begin $name:literal) => {
            prof_w = if prof_ok {
                match client.profile_start() {
                    Ok(w) => Some(w),
                    Err(e) => {
                        eprintln!(
                            "chain-prof: profile_start failed ({e}); stages run unwindowed"
                        );
                        prof_ok = false;
                        None
                    }
                }
            } else {
                None
            };
        };
        (end $name:literal) => {
            if let Some(w) = prof_w.take() {
                let dur = client.profile_end(w).expect("profile_end");
                if prof_timing.is_none() {
                    prof_timing = Some(format!("{}", dur.timing_method()));
                }
                match pollster::block_on(dur.resolve()) {
                    Some(ticks) => prof_rows.push(($name, ticks.duration())),
                    None => prof_missing += 1,
                }
            }
        };
    }
    unsafe {
        prof!(begin "decode");
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
            bshift,
        );
        prof!(end "decode");
        prof!(begin "cluster_probe");
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
        prof!(end "cluster_probe");
        prof!(begin "cand_count_tile");
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
        prof!(end "cand_count_tile");
        prof!(begin "cand_count_spine");
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
        prof!(end "cand_count_spine");
    }
    let t_csync = std::time::Instant::now();
    let tb = client.read_one(h_ctotal.clone()).expect("candidate count");
    let c = bytemuck::cast_slice::<u8, u32>(&tb)[0] as usize;
    if prof_ok {
        prof_rows.push(("sync:cand_readback", t_csync.elapsed()));
    }
    let kmax = ((c as u32 + 1).next_power_of_two().trailing_zeros()) as usize;
    let cstride = c + 1;
    let h_lvl = alloc_empty((kmax * (c + 1)).max(1) * 4);
    let h_parent = alloc_empty((c + 1) * 4);
    let h_parent_b = alloc_empty((c + 1) * 4);
    let mut d0 = vec![1u32; c + 1];
    d0[c] = 0;
    let h_d0 = alloc_upload(bytemuck::cast_slice(&d0));
    let h_d_a = alloc_empty((c + 1) * 4);
    let h_d_b = alloc_empty((c + 1) * 4);
    let h_roots = alloc_upload(bytemuck::cast_slice(&vec![c as u32; item_count]));
    unsafe {
        prof!(begin "cand_scatter");
        cand_scatter::launch_unchecked(
            &client,
            tiles_grid(n_tiles),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(h_cslot.clone(), n),
            BufferArg::from_raw_parts(h_cxc.clone(), n_tiles),
            BufferArg::from_raw_parts(h_cup.clone(), n_tiles * units),
            BufferArg::from_raw_parts(h_hp.clone(), c),
            units,
            rake,
        );
        prof!(end "cand_scatter");
        prof!(begin "jump_build");
        jump_build::launch_unchecked(
            &client,
            cubes_of(c + 1),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_hp.clone(), c),
            BufferArg::from_raw_parts(h_cend.clone(), n),
            BufferArg::from_raw_parts(h_ir.clone(), ir.len()),
            BufferArg::from_raw_parts(h_ctotal.clone(), 1),
            BufferArg::from_raw_parts(h_parent.clone(), c + 1),
        );
        prof!(end "jump_build");
        // The K rank steps ride ONE window (the bench's cluster_rank
        // grouping): same kernel, ping-ponged buffers, K = ceil(log2(c+1)).
        prof!(begin "cluster_rank");
        let mut sp = h_parent.clone();
        let mut sd = h_d0.clone();
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
                BufferArg::from_raw_parts(h_lvl.clone(), kmax * (c + 1)),
                k,
                cstride,
            );
            sp = tp;
            sd = td;
        }
        prof!(end "cluster_rank");
        prof!(begin "item_roots");
        item_roots::launch_unchecked(
            &client,
            cubes_of(item_count.max(1)),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_hp.clone(), c),
            BufferArg::from_raw_parts(h_ctotal.clone(), 1),
            BufferArg::from_raw_parts(h_ir.clone(), ir.len()),
            BufferArg::from_raw_parts(h_ic.clone(), ic.len()),
            BufferArg::from_raw_parts(h_roots.clone(), item_count),
        );
        prof!(end "item_roots");
        prof!(begin "cluster_mark");
        cluster_mark::launch_unchecked(
            &client,
            cubes_of(c.max(1)),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_hp.clone(), c),
            BufferArg::from_raw_parts(sd.clone(), c + 1),
            BufferArg::from_raw_parts(h_lvl.clone(), kmax * (c + 1)),
            BufferArg::from_raw_parts(h_ctotal.clone(), 1),
            BufferArg::from_raw_parts(h_roots.clone(), item_count),
            BufferArg::from_raw_parts(h_ir.clone(), ir.len()),
            BufferArg::from_raw_parts(h_cend.clone(), n),
            BufferArg::from_raw_parts(h_cslot.clone(), n),
            BufferArg::from_raw_parts(h_sm.clone(), n),
            BufferArg::from_raw_parts(h_gi.clone(), n),
            BufferArg::from_raw_parts(h_fl.clone(), n_words),
            kmax,
            cstride,
            bitmap_advance,
        );
        prof!(end "cluster_mark");
        prof!(begin "tile_scan");
        tile_scan::launch_unchecked(
            &client,
            tiles_grid(n_tiles),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(h_fl.clone(), n_words),
            BufferArg::from_raw_parts(h_sm.clone(), n),
            BufferArg::from_raw_parts(h_ir.clone(), ir.len()),
            BufferArg::from_raw_parts(h_ie.clone(), ie.len()),
            BufferArg::from_raw_parts(h_tc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
            BufferArg::from_raw_parts(h_tm.clone(), n_tiles),
            units,
            rake,
            log,
        );
        prof!(end "tile_scan");
        prof!(begin "spine_scan");
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
        prof!(end "spine_scan");
        prof!(begin "apply");
        apply::launch_unchecked(
            &client,
            tiles_grid(n_tiles),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(h_fl.clone(), n_words),
            BufferArg::from_raw_parts(h_sm.clone(), n),
            BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
            BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
            BufferArg::from_raw_parts(h_ir.clone(), ir.len()),
            BufferArg::from_raw_parts(h_ie.clone(), ie.len()),
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
            false,
        );
        prof!(end "apply");
        prof!(begin "resolve_x");
        resolve_x::launch_unchecked(
            &client,
            cubes_of(n.div_ceil(rspan)),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_sm.clone(), n),
            BufferArg::from_raw_parts(h_fl.clone(), n_words),
            BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
            BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
            BufferArg::from_raw_parts(h_im.clone(), item_count * IM_STRIDE),
            BufferArg::from_raw_parts(h_ie.clone(), ie.len()),
            BufferArg::from_raw_parts(h_ir.clone(), ir.len()),
            BufferArg::from_raw_parts(h_wc.clone(), n),
            BufferArg::from_raw_parts(h_otb.clone(), n),
            BufferArg::from_raw_parts(h_rmax.clone(), item_count),
            BufferArg::from_raw_parts(h_xmax.clone(), item_count),
            256,
            rspan,
        );
        prof!(end "resolve_x");
        prof!(begin "extent_pair");
        extent_pair::launch_unchecked(
            &client,
            cubes_of(n),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_sm.clone(), n),
            BufferArg::from_raw_parts(h_fl.clone(), n_words),
            BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
            BufferArg::from_raw_parts(h_plan.clone(), walk_plan.len()),
            BufferArg::from_raw_parts(h_extent.clone(), item_count * 2),
        );
        prof!(end "extent_pair");
        prof!(begin "derive_stride");
        derive_stride::launch_unchecked(
            &client,
            cubes_of(item_count.max(1)),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_extent.clone(), item_count * 2),
            BufferArg::from_raw_parts(h_ie.clone(), ie.len()),
            BufferArg::from_raw_parts(h_gap.clone(), item_count),
            BufferArg::from_raw_parts(h_strides.clone(), item_count * 2),
        );
        prof!(end "derive_stride");
        prof!(begin "paginate");
        paginate::launch_unchecked(
            &client,
            cubes_of(n),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
            BufferArg::from_raw_parts(h_fl.clone(), n_words),
            BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
            BufferArg::from_raw_parts(h_im.clone(), item_count * IM_STRIDE),
            BufferArg::from_raw_parts(h_ie.clone(), ie.len()),
            BufferArg::from_raw_parts(h_ir.clone(), ir.len()),
            BufferArg::from_raw_parts(h_strides.clone(), item_count * 2),
        );
        prof!(end "paginate");
        // ── rung 5b: the survivor pass ──────────────────────────────────
        // The proven cluster-counter machinery (count_tile/count_spine) on
        // the two byte flags, then the ordinal scatter overwrites the flags
        // with per-byte exclusive ordinals, then per-item totals from the
        // boundaries. Runs in EVERY mode — rec_base itself comes from here
        // now (the CPU leader scan is gone).
        prof!(begin "survivor_flags");
        survivor_flags::launch_unchecked(
            &client,
            cubes_of(n),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_fl.clone(), n_words),
            BufferArg::from_raw_parts(h_gi.clone(), n),
            BufferArg::from_raw_parts(h_lflag.clone(), n),
            BufferArg::from_raw_parts(h_sflag.clone(), n),
        );
        prof!(end "survivor_flags");
        prof!(begin "sv_count_tile_l");
        count_tile::launch_unchecked(
            &client,
            tiles_grid(n_tiles),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(h_lflag.clone(), n),
            BufferArg::from_raw_parts(h_ltc.clone(), n_tiles),
            BufferArg::from_raw_parts(h_lup.clone(), n_tiles * units),
            units,
            rake,
            log,
        );
        prof!(end "sv_count_tile_l");
        prof!(begin "sv_count_spine_l");
        count_spine::launch_unchecked(
            &client,
            CubeCount::new_single(),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(h_ltc.clone(), n_tiles),
            BufferArg::from_raw_parts(h_lxc.clone(), n_tiles),
            BufferArg::from_raw_parts(h_lgrand.clone(), 1),
            units,
            log,
        );
        prof!(end "sv_count_spine_l");
        prof!(begin "sv_count_tile_s");
        count_tile::launch_unchecked(
            &client,
            tiles_grid(n_tiles),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(h_sflag.clone(), n),
            BufferArg::from_raw_parts(h_stc.clone(), n_tiles),
            BufferArg::from_raw_parts(h_sup.clone(), n_tiles * units),
            units,
            rake,
            log,
        );
        prof!(end "sv_count_tile_s");
        prof!(begin "sv_count_spine_s");
        count_spine::launch_unchecked(
            &client,
            CubeCount::new_single(),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(h_stc.clone(), n_tiles),
            BufferArg::from_raw_parts(h_sxc.clone(), n_tiles),
            BufferArg::from_raw_parts(h_sgrand.clone(), 1),
            units,
            log,
        );
        prof!(end "sv_count_spine_s");
        prof!(begin "ordinal_scatter");
        ordinal_scatter::launch_unchecked(
            &client,
            tiles_grid(n_tiles),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(h_lflag.clone(), n),
            BufferArg::from_raw_parts(h_sflag.clone(), n),
            BufferArg::from_raw_parts(h_lxc.clone(), n_tiles),
            BufferArg::from_raw_parts(h_sxc.clone(), n_tiles),
            BufferArg::from_raw_parts(h_lup.clone(), n_tiles * units),
            BufferArg::from_raw_parts(h_sup.clone(), n_tiles * units),
            BufferArg::from_raw_parts(h_lflag.clone(), n),
            BufferArg::from_raw_parts(h_sflag.clone(), n),
            units,
            rake,
        );
        prof!(end "ordinal_scatter");
        prof!(begin "item_totals");
        item_totals::launch_unchecked(
            &client,
            cubes_of(item_count.max(1)),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_ir.clone(), ir.len()),
            BufferArg::from_raw_parts(h_lflag.clone(), n),
            BufferArg::from_raw_parts(h_sflag.clone(), n),
            BufferArg::from_raw_parts(h_ltot.clone(), item_count),
            BufferArg::from_raw_parts(h_stot.clone(), item_count),
            BufferArg::from_raw_parts(h_lgrand.clone(), 1),
            BufferArg::from_raw_parts(h_sgrand.clone(), 1),
        );
        prof!(end "item_totals");
    }
    // ── the tail: totals, then the mode's emission loops ─────────────────
    let t_rb = std::time::Instant::now();
    drop(sp_dispatch);
    let span_tail = tracing::info_span!(
        "tail",
        live_bytes = live.get(),
        total_records = tracing::field::Empty,
        total_slots = tracing::field::Empty,
    );
    let _sp_tail = span_tail.enter();
    let sp_totals = tracing::info_span!("tail.totals").entered();
    // The survivor pass's tiny readbacks — per-item leader/survivor totals,
    // prefix-summed on host into rec_base/slot_base and the loop bounds.
    // This is the CPU leader scan's replacement (its 0.195s at the 97MB
    // shape is what `prep` used to report).
    let t_tsync = std::time::Instant::now();
    let tb_l = client.read_one(h_ltot.clone()).expect("leader totals");
    let ltot: Vec<u32> = bytemuck::cast_slice(&tb_l)[..item_count].to_vec();
    let tb_s = client.read_one(h_stot.clone()).expect("survivor totals");
    let stot: Vec<u32> = bytemuck::cast_slice(&tb_s)[..item_count].to_vec();
    if prof_ok {
        prof_rows.push(("sync:totals_readback", t_tsync.elapsed()));
    }
    let mut rec_base = vec![0u32; item_count];
    let mut slot_base = vec![0u32; item_count];
    let mut total_records = 0u32;
    let mut total_slots = 0u32;
    for i in 0..item_count {
        rec_base[i] = total_records;
        slot_base[i] = total_slots;
        total_records += ltot[i];
        total_slots += stot[i];
    }
    // The paint/record alignment cross-check — compact's loud assert's
    // successor: the HOST colorize counts must line up with the DEVICE
    // leader totals, item by item, or the paint table would silently paint
    // the wrong records.
    if wants_instances {
        let mut expect = 0u32;
        for (it, &is_pr) in inputs.is_per_record.iter().enumerate() {
            if is_pr != 0 {
                assert_eq!(
                    inputs.color_base[it], expect,
                    "paint/record misalignment at item {it}: colors start at {} but the records say {expect}",
                    inputs.color_base[it],
                );
                expect += ltot[it];
            }
        }
        assert_eq!(
            inputs.per_record_colors.len(),
            expect as usize,
            "paint is indexed by record: {} colors for {} records",
            inputs.per_record_colors.len(),
            expect,
        );
    }
    drop(sp_totals);
    span_tail.record("total_records", total_records);
    span_tail.record("total_slots", total_slots);
    // THE LADDER (E3, note 23): every lane whose last reader ran before the
    // survivor pass drops HERE — the totals readback above is a queue sync,
    // so nothing in flight reads them when the cleanup reclaims. ~28 B per
    // corpus byte at the flagship (~2.7 GB), shrinking the tail's live set
    // to slots + tint + the scatter's reads, back under the working-set
    // cliff by construction. The records tail keeps lc (its row/col lanes).
    {
        let _sp_ladder = tracing::info_span!("tail.ladder").entered();
        drop((
            h_bytes, h_bi, h_bm, h_bc, h_seq, h_bmap, h_poff, h_pval, h_ic, h_ie, h_im, h_gap,
            h_plan, h_strides, h_rmax, h_xmax, h_extent, h_cslot, h_cend, h_tc, h_tm, h_xc, h_xm,
            h_wm, h_otb, h_hp, h_lvl, h_parent, h_parent_b, h_d0, h_d_a, h_d_b, h_roots, h_lflag,
            h_ltc, h_stc, h_lup, h_sup, h_lxc, h_sxc, h_lgrand, h_sgrand, h_ltot, h_stot,
            h_ctotal,
        ));
        client.memory_cleanup();
        let usage = client.memory_usage();
        tracing::info!(
            live_bytes = live.get(),
            bytes_in_use = usage.bytes_in_use,
            number_allocs = usage.number_allocs,
            "tail.ladder: post-cleanup pool state"
        );
    }
    let h_base = alloc_upload(bytemuck::cast_slice(&rec_base));
    let mut recs_all: Vec<u32> = Vec::new();
    // h_lc's end splits by mode: the records tail reads it (row/col), the
    // product tail never does — and Rust's path-sensitivity means the
    // drop rides each arm of ONE if/else, not two correlated conditions.
    if matches!(mode, ChainMode::Records | ChainMode::Both) {
        recs_all.reserve(total_records as usize * 8);
        let chunk_recs = chunk_recs_cap.min(total_records as usize).max(1);
        let h_recs = alloc_empty(chunk_recs * 8 * 4);
        let mut first = 0usize;
        while first < total_records as usize {
            let take = chunk_recs.min(total_records as usize - first);
            let span_win = tracing::info_span!(
                "tail.window",
                tail = "records",
                first,
                take,
                live_bytes = live.get()
            );
            let _sp_win = span_win.enter();
            let h_win = alloc_upload(bytemuck::cast_slice(&[first as u32]));
            let sp_emit = tracing::info_span!("tail.window.emit").entered();
            unsafe {
                prof!(begin "emit_records");
                emit_records::launch_unchecked(
                    &client,
                    cubes_of(n),
                    CubeDim::new_1d(256),
                    BufferArg::from_raw_parts(h_fl.clone(), n_words),
                    BufferArg::from_raw_parts(h_wc.clone(), n),
                    BufferArg::from_raw_parts(h_ir.clone(), ir.len()),
                    BufferArg::from_raw_parts(h_base.clone(), item_count),
                    BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
                    BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                    BufferArg::from_raw_parts(h_sm.clone(), n),
                    BufferArg::from_raw_parts(h_hgt.clone(), n),
                    BufferArg::from_raw_parts(h_gi.clone(), n),
                    BufferArg::from_raw_parts(h_recs.clone(), take * 8),
                    BufferArg::from_raw_parts(h_win, 1),
                );
                prof!(end "emit_records");
            }
            drop(sp_emit);
            let sp_rb = tracing::info_span!("tail.window.readback").entered();
            let cb = client.read_one(h_recs.clone()).expect("read chunk");
            let csv: &[u32] = bytemuck::cast_slice(&cb);
            recs_all.extend_from_slice(&csv[..take * 8]);
            drop(sp_rb);
            first += take;
        }
        drop(h_lc); // the records tail is lc's last reader
    } else {
        drop(h_lc); // the product path never reads row/col — it rides the ladder
    }
    let mut placements: Vec<crate::layout::ItemPlacement> = Vec::new();
    let mut slots_all: Vec<u32> = Vec::new();
    let mut tint_store = crate::layout::TintStore::Host(Vec::new());
    let mut slot_device: Option<SlotDevice> = None;
    if wants_instances {
        // THE ENDPOINT (note 23, E2b): ONE scatter pass writes the 32 B
        // slots directly into the buffer the renderer will bind — no
        // windows, no rolling chunk, no hop, no copy; the slots never
        // cross to host. The pack windows, both hops, and the footprint
        // gate that chose between them are all gone (the gate's lesson —
        // the ledger and the cliff numbers — stays in note 22). The
        // scatter is also the sole extent folder (E1's mask lesson: a
        // duplicate reducer is a mask, so there is exactly one).
        let sp_scatter = tracing::info_span!("tail.scatter").entered();
        assert!(
            (total_slots as u64) * 32 <= device_ref.max_buffer_size,
            "the endpoint needs one {} B slot buffer — over this device's \
             max_buffer_size ({}); chunked slot buffers are the named \
             follow-up",
            (total_slots as u64) * 32,
            device_ref.max_buffer_size,
        );
        let h_slots = alloc_empty(total_slots.max(1) as usize * 8 * 4);
        let h_tint = alloc_empty(total_slots.max(1) as usize * 2 * 4);
        unsafe {
            prof!(begin "scatter_slots");
            scatter_slots::launch_unchecked(
                &client,
                cubes_of(n),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(h_fl.clone(), n_words),
                BufferArg::from_raw_parts(h_wc.clone(), n),
                BufferArg::from_raw_parts(h_ir.clone(), ir.len()),
                BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
                BufferArg::from_raw_parts(h_sm.clone(), n),
                BufferArg::from_raw_parts(h_hgt.clone(), n),
                BufferArg::from_raw_parts(h_gi.clone(), n),
                BufferArg::from_raw_parts(h_pr_colors.clone(), inputs.per_record_colors.len()),
                BufferArg::from_raw_parts(h_color_base.clone(), item_count),
                BufferArg::from_raw_parts(h_is_pr.clone(), item_count),
                BufferArg::from_raw_parts(h_flat_colors.clone(), item_count),
                BufferArg::from_raw_parts(h_groups.clone(), item_count),
                BufferArg::from_raw_parts(h_sflag.clone(), n),
                BufferArg::from_raw_parts(h_slots.clone(), total_slots.max(1) as usize * 8),
                BufferArg::from_raw_parts(h_tint.clone(), total_slots.max(1) as usize * 2),
                BufferArg::from_raw_parts(h_ext.clone(), item_count * EXT_STRIDE),
            );
            prof!(end "scatter_slots");
        }
        drop(sp_scatter);
        // The one slot-derived readback: the tint stream, seg_tint's
        // fold input. TWO forms (E3b): the gate's Both mode and
        // non-shared hosts take cubecl's staging pipe; the product on
        // Metal takes ONE device copy into a hal-mapped shared buffer and
        // the host reads the pointer — no staging alloc, no Bytes, no
        // to_vec. (The read_one cascade cost 4.16s of the 12.1s flagship
        // backend — three 762 MB fault-and-copy passes for one stream.)
        let sp_tint = tracing::info_span!("tail.tint").entered();
        tint_store = if matches!(mode, ChainMode::Both)
            || !device_ref.host_visible_storage
            || total_slots == 0
        {
            let tb = client.read_one(h_tint.clone()).expect("read tint stream");
            crate::layout::TintStore::Host(
                bytemuck::cast_slice::<u8, u32>(&tb)[..total_slots as usize * 2].to_vec(),
            )
        } else {
            let res = client
                .get_resource::<WgpuServer<AutoCompiler>>(h_tint.clone())
                .expect("tint stream resource");
            let (src, src_off) = {
                let r = res.resource();
                (r.buffer.clone(), r.offset)
            };
            let bytes = (total_slots as usize * 8) as u64;
            let (buf, ptr) = mapped_read_buffer(&device_ref.device, bytes.max(4), "tint stream");
            let mut enc = device_ref
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("tint stream copy"),
                });
            enc.copy_buffer_to_buffer(&src, src_off, &buf, 0, bytes.max(4));
            device_ref.queue.submit([enc.finish()]);
            device_ref
                .device
                .poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: None,
                })
                .expect("tint copy poll");
            // `res` drops here — the cubecl tint slice returns to the pool.
            // Queue order already moved the bytes; nothing pending reads it.
            crate::layout::TintStore::Mapped(crate::layout::TintMapped {
                buffer: buf,
                ptr,
                words: total_slots as usize * 2,
            })
        };
        drop(sp_tint);
        // The fork gate's lane tier reads the slot stream host-side.
        if matches!(mode, ChainMode::Both) {
            let sb = client.read_one(h_slots.clone()).expect("read slot scatter");
            slots_all = bytemuck::cast_slice::<u8, u32>(&sb)[..total_slots as usize * 8].to_vec();
        }
        // The extraction: the slot buffer becomes the renderer's storage.
        // get_resource CONSUMES the handle into a ManagedResource whose
        // binding keeps the pool slice from being re-allocated — so the
        // guard rides the stream (type-erased) for the scene's lifetime.
        if total_slots > 0 {
            let res = client
                .get_resource::<WgpuServer<AutoCompiler>>(h_slots.clone())
                .expect("slot buffer resource");
            let (buffer, offset) = {
                let r = res.resource();
                (r.buffer.clone(), r.offset)
            };
            assert!(
                offset % 16 == 0,
                "slot buffer's pool offset {offset} breaks the storage binding alignment"
            );
            slot_device = Some(SlotDevice {
                chunk: crate::layout::DeviceSlotChunk {
                    buffer,
                    offset,
                    slots: total_slots,
                },
                keep_alive: Box::new(res),
            });
        }
        // Placements from the extent lanes — the same reduction the host
        // compaction performs (page over ALL records, ink over survivors),
        // decoded from key space.
        let _sp_placements = tracing::info_span!("tail.placements").entered();
        let tb_e = client.read_one(h_ext.clone()).expect("extent lanes");
        let ev: &[u32] = bytemuck::cast_slice(&tb_e);
        placements.reserve(item_count);
        for it in 0..item_count {
            let e = it * EXT_STRIDE;
            placements.push(crate::layout::ItemPlacement {
                slot_base: slot_base[it],
                slot_count: stot[it],
                record_count: ltot[it],
                page: crate::layout::PageExtent {
                    right: key_to_float_host(ev[e]),
                    bottom: key_to_float_host(ev[e + 1]),
                    z_min: key_to_float_host(ev[e + 2]),
                    z_max: key_to_float_host(ev[e + 3]),
                },
                ink: crate::layout::InkExtent {
                    min: [
                        key_to_float_host(ev[e + 4]),
                        key_to_float_host(ev[e + 5]),
                        key_to_float_host(ev[e + 8]),
                    ],
                    max: [
                        key_to_float_host(ev[e + 6]),
                        key_to_float_host(ev[e + 7]),
                        key_to_float_host(ev[e + 9]),
                    ],
                },
            });
        }
    }
    if prof {
        if !prof_ok {
            eprintln!("chain-prof: GPU windows were unavailable — only the sync rows carry timings");
        }
        let sum: std::time::Duration = prof_rows.iter().map(|(_, d)| *d).sum();
        eprintln!(
            "chain-prof: timing={} — {} rows, {} missing windows (fenced per stage; the SUM is the price table, the spans keep the unfused walls)",
            prof_timing.as_deref().unwrap_or("none"),
            prof_rows.len(),
            prof_missing
        );
        for (name, d) in &prof_rows {
            eprintln!("  {name:<24} {:>9.3}ms", d.as_secs_f64() * 1e3);
        }
        eprintln!("  {:<24} {:>9.3}ms", "SUM", sum.as_secs_f64() * 1e3);
    }
    ChainStream {
        records: recs_all,
        rec_base,
        total_records,
        total_slots,
        placements,
        slot_device,
        tint: tint_store,
        slots: slots_all,
        candidates: c,
        chain_dur: t_chain0.elapsed(),
        readback_dur: t_rb.elapsed(),
        phases: ChainPhases {
            prep: t_tables.duration_since(t_chain0),
            tables: t_init.duration_since(t_tables),
            init: t_upload.duration_since(t_init),
            upload: t_dispatch.duration_since(t_upload),
            dispatch: t_rb.duration_since(t_dispatch),
        },
    }
}

/// A hal-mapped shared-storage readback target (Metal hosts only — the
/// caller gates on the profile). MAP_READ makes wgpu-hal pick
/// StorageModeShared with the DEFAULT cache mode (write-combining is
/// MAP_WRITE-only), so host reads of the GPU-written bytes stay cached.
/// The mapping lives as long as the buffer (Metal's unmap is a no-op) —
/// the same lifecycle as the mapped arena's chunks.
fn mapped_read_buffer(device: &wgpu::Device, bytes: u64, label: &str) -> (wgpu::Buffer, *const u32) {
    use wgpu::hal::Device as HalDevice;
    let hal_dev = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }
        .expect("host-visible storage behind a non-Metal device");
    let hal_buf = unsafe {
        hal_dev.create_buffer(&wgpu::hal::BufferDescriptor {
            label: Some(label),
            size: bytes,
            usage: wgpu::BufferUses::MAP_READ | wgpu::BufferUses::COPY_DST,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
        })
    }
    .expect("hal readback buffer");
    let mapping = unsafe { hal_dev.map_buffer(&hal_buf, 0..bytes) }.expect("hal readback map");
    let ptr = mapping.ptr.as_ptr() as *const u32;
    // SAFETY: same device, desc matches the hal request, nonzero size, and
    // every byte is written by the caller's copy before the poll publishes.
    let buf = unsafe {
        device.create_buffer_from_hal::<wgpu::hal::api::Metal>(
            hal_buf,
            &wgpu::BufferDescriptor {
                label: Some(label),
                size: bytes,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            },
        )
    };
    (buf, ptr)
}
