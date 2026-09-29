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
    pack_instances, survivor_flags,
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
}

impl SharedDevice {
    pub(crate) fn from_ctx(ctx: &crate::gpu::GpuContext) -> Self {
        Self {
            instance: ctx.instance.clone(),
            adapter: ctx.adapter.clone(),
            device: ctx.device.clone(),
            queue: ctx.queue.clone(),
        }
    }
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
    /// 12 u32 per slot — the GlyphInstance wire form (pos, glyph_id, row,
    /// col, color, group_id, advance, height, flags, pad), walk order,
    /// survivor order within items. EMPTY unless Instances/Both.
    pub instances: Vec<u32>,
    pub total_slots: u32,
    /// Per-item placements, decoded from the pack kernel's extent lanes —
    /// the same reduction `compact_records_into` does on host (page over
    /// ALL records, ink over survivors, min/max order-free). EMPTY unless
    /// Instances/Both.
    pub placements: Vec<crate::layout::ItemPlacement>,
    /// True when the copy hop filled the caller's MAPPED ARENA directly
    /// (rung 5c): `instances` is empty by design and the arena only needs
    /// its `commit` — the slots never crossed to host.
    pub instances_on_device: bool,
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
    mapped: Option<crate::layout::MappedTarget>,
) -> ChainStream {
    let item_count = items.len();
    let n: usize = bytes.len();
    let fis = items;
    // ── the chain side ────────────────────────────────────────────────────
    // The span decomposition mirrors the Instant pairs below exactly —
    // ChainPhases stays the print contract, the spans are the programmatic
    // instrument, and reading the same boundaries is what lets the two be
    // cross-checked before anything collapses onto either.
    let _chain = tracing::info_span!(
        "chain",
        n,
        items = item_count,
        ?mode,
        mapped = mapped.is_some()
    )
    .entered();
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
    // the standing fixture: the windowed carry arithmetic of BOTH tails,
    // fenced on an ordinary corpus. Defaults keep each buffer at ~512MB:
    // 16.7M records × 32 B, 11.18M slots × 48 B.
    let chunk_env: Option<usize> = std::env::var("GLYPH_RECORD_CHUNK")
        .ok()
        .and_then(|v| v.parse().ok());
    let chunk_recs_cap = chunk_env.unwrap_or(16_777_216);
    let chunk_slots_cap = chunk_env.unwrap_or(536_870_912 / 48);
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
    let tb = client.read_one(h_ctotal.clone()).expect("candidate count");
    let c = bytemuck::cast_slice::<u8, u32>(&tb)[0] as usize;
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
        extent_pair::launch_unchecked(
            &client,
            cubes_of(item_count.max(1)),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_sm.clone(), n),
            BufferArg::from_raw_parts(h_fl.clone(), n_words),
            BufferArg::from_raw_parts(h_plan.clone(), walk_plan.len()),
            BufferArg::from_raw_parts(h_extent.clone(), item_count * 2),
        );
        derive_stride::launch_unchecked(
            &client,
            cubes_of(item_count.max(1)),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_extent.clone(), item_count * 2),
            BufferArg::from_raw_parts(h_ie.clone(), ie.len()),
            BufferArg::from_raw_parts(h_gap.clone(), item_count),
            BufferArg::from_raw_parts(h_strides.clone(), item_count * 2),
        );
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
        // ── rung 5b: the survivor pass ──────────────────────────────────
        // The proven cluster-counter machinery (count_tile/count_spine) on
        // the two byte flags, then the ordinal scatter overwrites the flags
        // with per-byte exclusive ordinals, then per-item totals from the
        // boundaries. Runs in EVERY mode — rec_base itself comes from here
        // now (the CPU leader scan is gone).
        survivor_flags::launch_unchecked(
            &client,
            cubes_of(n),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_fl.clone(), n_words),
            BufferArg::from_raw_parts(h_gi.clone(), n),
            BufferArg::from_raw_parts(h_lflag.clone(), n),
            BufferArg::from_raw_parts(h_sflag.clone(), n),
        );
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
    let tb_l = client.read_one(h_ltot.clone()).expect("leader totals");
    let ltot: Vec<u32> = bytemuck::cast_slice(&tb_l)[..item_count].to_vec();
    let tb_s = client.read_one(h_stot.clone()).expect("survivor totals");
    let stot: Vec<u32> = bytemuck::cast_slice(&tb_s)[..item_count].to_vec();
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
    let h_base = alloc_upload(bytemuck::cast_slice(&rec_base));
    let mut recs_all: Vec<u32> = Vec::new();
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
            }
            drop(sp_emit);
            let sp_rb = tracing::info_span!("tail.window.readback").entered();
            let cb = client.read_one(h_recs.clone()).expect("read chunk");
            let csv: &[u32] = bytemuck::cast_slice(&cb);
            recs_all.extend_from_slice(&csv[..take * 8]);
            drop(sp_rb);
            first += take;
        }
    }
    let mut inst_all: Vec<u32> = Vec::new();
    let mut placements: Vec<crate::layout::ItemPlacement> = Vec::new();
    let mut instances_on_device = false;
    if wants_instances {
        // The COPY HOP (rung 5c): with a mapped arena handed in, no slot
        // byte ever crosses to host — each window is flushed out of
        // cubecl's stream by `get_resource` (the flush IS the handoff),
        // then one encoder copy lands it in the arena's shared storage,
        // in queue order after the pack that filled it and before the
        // next window's pack overwrites the rolling buffer. One final
        // Wait-poll makes the bytes visible before the caller's staging
        // reads the pointer. Without a mapped arena (non-Metal,
        // GPU-less), the readback hop stands — measured the cheaper
        // product everywhere it can run.
        let copy_hop = mapped.is_some();
        instances_on_device = copy_hop;
        if !copy_hop {
            // Exact capacity: the caller takes this allocation over as the
            // Vec-arena's storage (GlyphArena::from_vec, zero copies) — the
            // readback hop must touch these pages exactly once.
            inst_all.reserve_exact(total_slots as usize * 12);
        }
        let chunk_slots = chunk_slots_cap.min(total_slots as usize).max(1);
        let h_out = alloc_empty(chunk_slots * 12 * 4);
        let mut first = 0usize;
        let tail_debug = std::env::var_os("GLYPH_CHAIN_DEBUG").is_some();
        while first < total_slots as usize {
            let take = chunk_slots.min(total_slots as usize - first);
            let t_win = std::time::Instant::now();
            let span_win = tracing::info_span!(
                "tail.window",
                tail = "instances",
                first,
                take,
                live_bytes = live.get()
            );
            let _sp_win = span_win.enter();
            let h_win = alloc_upload(bytemuck::cast_slice(&[first as u32]));
            let sp_pack = tracing::info_span!("tail.window.pack").entered();
            unsafe {
                pack_instances::launch_unchecked(
                    &client,
                    cubes_of(n),
                    CubeDim::new_1d(256),
                    BufferArg::from_raw_parts(h_fl.clone(), n_words),
                    BufferArg::from_raw_parts(h_wc.clone(), n),
                    BufferArg::from_raw_parts(h_ir.clone(), ir.len()),
                    BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
                    BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                    BufferArg::from_raw_parts(h_sm.clone(), n),
                    BufferArg::from_raw_parts(h_hgt.clone(), n),
                    BufferArg::from_raw_parts(h_gi.clone(), n),
                    BufferArg::from_raw_parts(h_pr_colors.clone(), inputs.per_record_colors.len()),
                    BufferArg::from_raw_parts(h_color_base.clone(), item_count),
                    BufferArg::from_raw_parts(h_is_pr.clone(), item_count),
                    BufferArg::from_raw_parts(h_flat_colors.clone(), item_count),
                    BufferArg::from_raw_parts(h_groups.clone(), item_count),
                    BufferArg::from_raw_parts(h_sflag.clone(), n),
                    BufferArg::from_raw_parts(h_out.clone(), take * 12),
                    BufferArg::from_raw_parts(h_ext.clone(), item_count * EXT_STRIDE),
                    BufferArg::from_raw_parts(h_win, 1),
                );
            }
            if let Some(target) = &mapped {
                let pack_wall = t_win.elapsed();
                drop(sp_pack);
                let sp_flush = tracing::info_span!("tail.window.flush").entered();
                let t_flush = std::time::Instant::now();
                let res = client
                    .get_resource::<WgpuServer<AutoCompiler>>(h_out.clone())
                    .expect("instance window resource");
                let flush_wall = t_flush.elapsed();
                drop(sp_flush);
                let sp_copy = tracing::info_span!("tail.window.copy").entered();
                let t_copy = std::time::Instant::now();
                let r = res.resource();
                let mut enc = device_ref.device.create_command_encoder(
                    &wgpu::CommandEncoderDescriptor {
                        label: Some("glyph instance window"),
                    },
                );
                // The window lands across the arena's chunk buffers — one
                // copy per chunk intersection (chunk boundaries do not in
                // general coincide with window boundaries).
                let mut s = first;
                while s < first + take {
                    let k = s / target.chunk_slots;
                    let in_chunk = s - k * target.chunk_slots;
                    let here = (target.chunk_slots - in_chunk).min(first + take - s);
                    enc.copy_buffer_to_buffer(
                        &r.buffer,
                        r.offset + ((s - first) * 48) as u64,
                        &target.buffers[k],
                        (in_chunk * 48) as u64,
                        (here * 48) as u64,
                    );
                    s += here;
                }
                device_ref.queue.submit([enc.finish()]);
                drop(sp_copy);
                if tail_debug {
                    println!(
                        "  tail window {first} (take {take}): pack+submit {pack_wall:?} | flush {flush_wall:?} | copy+submit {:?}",
                        t_copy.elapsed()
                    );
                }
            } else {
                drop(sp_pack);
                let sp_rb = tracing::info_span!("tail.window.readback").entered();
                let cb = client.read_one(h_out.clone()).expect("read instance chunk");
                let csv: &[u32] = bytemuck::cast_slice(&cb);
                inst_all.extend_from_slice(&csv[..take * 12]);
                drop(sp_rb);
            }
            first += take;
        }
        if copy_hop {
            let sp_drain = tracing::info_span!("tail.drain").entered();
            let t_wait = std::time::Instant::now();
            device_ref
                .device
                .poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: None,
                })
                .expect("device poll after instance windows");
            if tail_debug {
                println!("  tail final wait: {:?}", t_wait.elapsed());
            }
            drop(sp_drain);
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
    ChainStream {
        records: recs_all,
        rec_base,
        total_records,
        instances: inst_all,
        total_slots,
        placements,
        instances_on_device,
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
