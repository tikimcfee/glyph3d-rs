//! CubeCL scan chain — dev-only (`--cubecl-chain-check`), the standard
//! parallel structure.
//!
//!   tileScan (rake + workgroup Blelloch) -> spineScan (one cube)
//!                -> apply (rake + Blelloch + chase) -> resolveX
//!                -> deriveStride -> paginate
//!
//! The note-16 phase-2 skeleton transcribed the Mojo device chain
//! line-for-line — thread-per-chunk serial 64-byte folds, a single-thread
//! spine — which proved the monoid but measured transcription quality, not
//! the algorithm. This is the textbook hierarchical scan instead: each cube
//! owns a `units x rake`-byte tile; every unit rakes its `rake` bytes into
//! one monoid element; a workgroup Blelloch scan over the per-unit partials
//! (shared memory, `sync_cube` between rounds) produces exclusive prefixes;
//! the spine is one cube doing the same over tile totals; `apply` re-rakes
//! and chases its bytes seeded with (global tile prefix + own micro prefix).
//! The CPU reference stays `scan.rs::run_scan_pipeline` — a DIFFERENT tree
//! shape at a different tuning, so agreement is associativity checked in
//! situ, the same evidence pattern as scan.rs's own chunk-sweep tests.
//!
//! Precision contract, deliberately restated for this structure: integer
//! lanes (lc/wc/otb) diff BIT-EXACT — the monoid's count lanes are exact and
//! any order-preserving association agrees. `tail_adv` is f32-per-add and the
//! Blelloch tree REASSOCIATES those adds within a tile, so line_advance
//! leaves bit-parity with scan.rs and lands in the ORACLE's existing 1e-4
//! eps tier (scan-vs-serial-fold was already eps there — only this
//! instrument's same-tuning bit check is loosened, max deviation reported).
//! The fold>0 X lanes stay bit-exact: resolve_x still re-sums each wrap
//! segment serially, in the same left-fold order as the serial recurrence.
//!
//! Two tree-specific invariants, both load-bearing:
//! - PAD elements (tail units of the last tile, empty spine blocks) carry the
//!   wrap/mode in force at their position, NOT pure identity — combine copies
//!   wrap/mode off its RIGHT operand unconditionally, and a zero-wrap pad at
//!   the end of a tile would clobber the tile total's wrap and mis-junction
//!   every later combine that reads it.
//! - The exclusive prefix at the tree root is seeded with pure identity; that
//!   is safe because a prefix element's OWN wrap/mode lanes are never read —
//!   combine only reads them off the right operand, which is always a real
//!   leaf or a real-derived element on every path that matters.

use std::path::Path;

use cubecl::prelude::*;
use cubecl::wgpu::{AutoCompiler, AutoGraphicsApi, GraphicsApi, WgpuServer, WgpuSetup};

use crate::fold::WrapMode;
use crate::text::ResolveGlyph;
use crate::gpu::GpuContext;

mod bench;
mod checks;
mod cluster;
mod decode;
mod monoid;
mod position;
mod scan;
mod tail;

pub use bench::bench;
pub use checks::{cluster_check, decode_check, run};

use self::cluster::{
    cand_scatter, cluster_host_inputs, cluster_mark, cluster_pair_filter, cluster_probe,
    count_spine, count_tile, item_roots, jump_build, rank_step,
};
use self::position::{derive_stride, extent_pair, paginate, resolve_x};
use self::scan::{apply, spine_scan, tile_scan};
use self::tail::{
    EXT_STRIDE, emit_records, item_totals, key_to_float_host, ordinal_scatter, ordered_key_host,
    pack_instances, survivor_flags,
};

// ── lane layout (glyph-identity.json, hash-pinned) ──────────────────────────
const PARTIAL_COUNT_STRIDE: usize = 8;
const P_RESET: usize = 0;
const P_NL: usize = 1;
const P_GLYPHS: usize = 2;
const P_ROWS: usize = 3;
const P_HEAD_LEN: usize = 4;
const P_TAIL_LEN: usize = 5;
const P_WRAP: usize = 6;
const P_MODE: usize = 7;
/// The measure static is ADVANCE ONLY. Height is renderer statics the
/// scan never reads; carrying it here doubled the per-byte measure traffic.
const SM_STRIDE: usize = 1;
const SM_ADVANCE: usize = 0;
/// resolveX's per-cube shared reduction slots, item-relative from the item
/// at the cube's first byte. A 2 KB tile spanning more than this many items
/// (never in practice) takes the global-atomic overflow path instead.
const RESOLVE_SLOTS: usize = 16;
/// The trie's missing flag (fold::TRIE_FLAG_MISSING) and the decode's
/// passthrough lane (fixture::F_MISSING), both in the packed low byte.
const TRIE_FLAG_MISSING: u32 = 1;
const F_MISSING: u32 = 8;
/// The cluster trailer lane (fold::F_CLUSTER_TRAILER) — bit 16, inside the
/// packed byte lane. Nothing in the chain reads it; the corpus FLAGS
/// comparison is its only witness, so it rides anyway.
const F_CLUSTER_TRAILER: u32 = 16;

const LM_STRIDE: usize = 4;
const LM_X: usize = 0;
const LM_Y: usize = 1;
const LM_Z: usize = 2;
const LM_BASE_X: usize = 3;
const LC_STRIDE: usize = 2;
const LC_ROW: usize = 0;
const LC_COL: usize = 1;
const IM_STRIDE: usize = 10;
const IM_ORIGIN_Y: usize = 0;
const IM_ORIGIN_Z: usize = 1;
const IM_LINE_HEIGHT: usize = 2;
const IM_Z_STEP: usize = 3;
const IM_BAND_STRIDE_Y: usize = 4;
const IM_DEPTH_PER_BAND: usize = 5;
const IM_DEPTH_PER_COL: usize = 6;
const IM_ORIGIN_X: usize = 8;
/// The z_step's f64 tail as a second f32 — the engine multiplies the FULL
/// f64 param and the correctly-rounded lane alone measurably diverges at
/// seg >= 3 (the wide-repo Z class: fl(3·0.15000000596) vs the engine's
/// fl(3·0.1499999999999999944), one ulp apart). The outer fma folds this
/// tail back in; see paginate's fma note.
const IM_Z_STEP_LO: usize = 9;
const IE_STRIDE: usize = 8;
const IE_PAGE_ROWS: usize = 0;
const IE_PAGE_COLS: usize = 1;
const IE_SCROLL_ROWS: usize = 2;
const IE_PAGES_WIDE: usize = 3;
const IE_WRAP_WIDTH: usize = 4;
const IE_HAS_PAGE: usize = 5;
const IE_WRAP_MODE: usize = 6;

const F_LEADER: u32 = 1;
const F_NEWLINE: u32 = 4;
const WRAP_BACK: i32 = 1;

// ── the driver ────────────────────────────────────────────────────────────────

/// The corpus packed four bytes per u32 word for the device decode, tail
/// lanes of the final word filled with 0x80 — a CONTINUATION lead, which the
/// lenient classifier reads as a non-leader. Zero pads instead classify as
/// phantom 1-byte NUL leaders with a resolved advance: every byte-indexed
/// kernel bounds itself by the rounded-UP word count, so scan totals inflate
/// and phantom statics writes land past buffers sized by the real n —
/// discarded by WGSL's robustness on Metal (why every gate stayed green
/// through the bug), real out-of-bounds writes on non-robust backends.
/// Found by the rung-4 grounding review.
///
/// The fill is a SEPARATE pass because the first landing guarded it with
/// `if i < n` inside the loop over the real bytes — a branch that can never
/// fire — and every gate stayed green through the no-op: trailing phantom
/// records self-truncate past `total_records`, so no device gate can see the
/// class (the fork gate's attempted mutation was dropped for exactly this).
/// The reddening witness is the unit test in this file; the classifier's
/// continuation rule itself is fenced by the real-byte flags diffs.
fn pack_words(bytes: &[u8]) -> Vec<u32> {
    let n_words = bytes.len().div_ceil(4);
    let mut packed = vec![0u32; n_words];
    for (i, &b) in bytes.iter().enumerate() {
        packed[i >> 2] |= (b as u32) << ((i & 3) * 8);
    }
    for i in bytes.len()..(n_words * 4) {
        packed[i >> 2] |= 0x80u32 << ((i & 3) * 8);
    }
    packed
}

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

pub fn repo_check(ctx: &GpuContext, dir: &Path) -> ! {
    use crate::layout::{LayoutGlyphs as _, VerifyLayout as _};
    let t_all = std::time::Instant::now();
    // The renderer's default shape: wrap BACK, cluster on, the tuned grid
    // pagination from RepoParams::default().
    let params = crate::repo::RepoParams {
        wrap_mode: WrapMode::Back,
        cluster_mode: crate::fold::ClusterMode::Cluster,
        ..Default::default()
    };
    let walk = crate::repo::walk_repo(dir);
    let file_params: Vec<crate::layout::ItemParams> = walk
        .files
        .iter()
        .map(|f| {
            let newlines = f.bytes.iter().filter(|&&b| b == b'\n').count();
            crate::repo::file_item_params(&params, f.bytes.len(), newlines)
        })
        .collect();
    let item_count = walk.files.len();
    let n: usize = walk.files.iter().map(|f| f.bytes.len()).sum();
    let mut bytes = Vec::with_capacity(n);
    let mut fis = Vec::with_capacity(item_count);
    let mut off = 0usize;
    for (i, f) in walk.files.iter().enumerate() {
        let p = &file_params[i];
        bytes.extend_from_slice(&f.bytes);
        fis.push(crate::fold::Item {
            byte_start: off as i64,
            byte_count: f.bytes.len() as i64,
            origin_x: p.origin_x,
            origin_y: p.origin_y,
            origin_z: p.origin_z,
            wrap_width: p.wrap_width as i64,
            wrap_mode: p.wrap_mode,
            cluster_mode: p.cluster_mode,
            z_step: p.z_step,
            line_height: p.line_height,
            has_page: p.has_page,
            page_rows: p.page_rows as i64,
            page_cols: p.page_cols as i64,
            scroll_rows: p.scroll_rows as i64,
            pages_wide: p.pages_wide as i64,
            page_gap_x: p.page_gap_x,
            band_stride_y: p.band_stride_y,
            depth_per_band: p.depth_per_band,
            depth_per_col: p.depth_per_col,
            page_line_height: p.page_line_height,
        });
        off += f.bytes.len();
    }

    // ── the chain side — THE load path, shared with CubeclLayout ────────
    // BOTH tails: the fence sees the product's instance/placement output
    // AND the record tier against the same dispatches. The paint tables are
    // the SAME colorize_leaders output the engine side paints with, so the
    // instance tier compares like against like. (Peak-memory note: Both
    // holds four streams at the big-corpus shape — records and instances,
    // engine and chain. The fork gate's standing fixture is small by
    // design; a manual big-corpus run that brushes the machine ceiling can
    // set GLYPH_REPO_CHECK_TAIL=records to drop the instance tier.)
    let colors: Vec<Vec<u32>> = walk
        .files
        .iter()
        .map(|f| crate::text::colorize_leaders(&f.bytes))
        .collect();
    let mut per_record_colors: Vec<u32> = Vec::new();
    let mut color_base = vec![0u32; item_count];
    let mut groups = Vec::with_capacity(item_count);
    for (index, c) in colors.iter().enumerate() {
        color_base[index] = per_record_colors.len() as u32;
        per_record_colors.extend_from_slice(c);
        groups.push(index as u32);
    }
    let inputs = InstanceInputs {
        per_record_colors,
        color_base,
        is_per_record: vec![1u32; item_count],
        flat_colors: vec![0u32; item_count],
        groups,
    };
    let mode = if std::env::var("GLYPH_REPO_CHECK_TAIL").as_deref() == Ok("records") {
        ChainMode::Records
    } else {
        ChainMode::Both
    };
    let device = SharedDevice::from_ctx(ctx);
    // The copy hop's own tier: the check's CHAIN side runs the SAME hop the
    // renderer runs — the fence follows the path. Chunked like the renderer
    // (one buffer per `arena_chunk_slots`); GLYPH_ARENA_CHUNK_SLOTS forces
    // small chunks so the gate crosses buffer boundaries on the standing
    // fixture. Other hosts fence the readback hop instead: each host fences
    // what it runs.
    let mut chain_arena = if ctx.profile.backend == wgpu::Backend::Metal
        && ctx.profile.mappable_primary_buffers
        && n > 0
    {
        crate::glyph_scene::mapped_instance_arena(ctx, n, crate::glyph_scene::arena_chunk_slots(ctx))
    } else {
        crate::layout::GlyphArena::new()
    };
    let stream = run_repo_chain(
        Some(&device),
        &bytes,
        &fis,
        &inputs,
        mode,
        chain_arena.mapped_target(),
    );
    crate::cubecl_layout::hand_off(
        stream.instances,
        stream.total_slots as usize,
        stream.instances_on_device,
        &mut chain_arena,
    );
    let recs_all = stream.records;
    let total_records = stream.total_records;
    let c = stream.candidates;
    let chain_dt = stream.chain_dur;
    let readback_dt = stream.readback_dur;
    let phases = stream.phases;
    if std::env::var_os("GLYPH_CHAIN_DEBUG").is_some() {
        println!("  dbg recs[0..16] = {:?}", &recs_all[..16]);
    }
    let recs: &[u32] = &recs_all;

    // ── the engine side, SECOND — records for the same items, run after the
    // GPU work so its host-side record stream never overlaps the chain's
    // dispatches (see the note above the leader scan). The arena and the
    // placements stay LIVE past the diff now: the instance and placement
    // tiers compare against them (the arena IS the engine's instance
    // output; the records tier only needs `engine_records` and runs after
    // the instance tiers have dropped the arena).
    let t_eng = std::time::Instant::now();
    let mut arena = crate::layout::GlyphArena::new();
    let mut backend = crate::layout_mojo::MojoLayout::new(crate::layout_mojo::Strategy::Batched);
    backend
        .load_trie_file(&crate::default_engine_trie())
        .expect("engine trie");
    let eng_items: Vec<crate::layout::LayoutItem<'_>> = walk
        .files
        .iter()
        .enumerate()
        .map(|(index, f)| crate::layout::LayoutItem {
            bytes: &f.bytes,
            params: file_params[index],
            group_id: index as u32,
            paint: crate::layout::Paint::PerRecord(&colors[index]),
        })
        .collect();
    let (placements, engine_records) = backend
        .layout_items_recording(&eng_items, &mut arena)
        .expect("engine layout failed");
    let eng_dt = t_eng.elapsed();
    let engine_total: u32 = placements.iter().map(|p| p.record_count).sum();
    // The owning item of every record, from the ENGINE's own placement
    // counts — the fork census below needs each record's page geometry to
    // attribute a deviation to the arithmetic that produced it.
    let mut item_of: Vec<u32> = Vec::with_capacity(engine_total as usize);
    for (idx, p) in placements.iter().enumerate() {
        item_of.extend(std::iter::repeat_n(idx as u32, p.record_count as usize));
    }
    drop(eng_items);

    // Empty-corpus refusal, always on — the repo-verify-direct lesson:
    // a PASS over zero items compared nothing.
    if item_count == 0 || total_records == 0 {
        eprintln!(
            "cubecl-repo-check FAIL: refusing to verify an empty corpus ({item_count} items, {total_records} records)"
        );
        std::process::exit(1);
    }

    // ── the instance and placement tiers (rung 5b) ────────────────────────
    // The PRODUCT tail's own claims: the packed slots byte-equal against
    // the engine-batched arena (the same comparison repo-verify makes of
    // the host backends), and the placements bit-equal — which fences the
    // pack kernel's extent folds (order-free atomics, but the seeds and
    // the lane arithmetic must match compact_records_into exactly).
    let mut inst_bad = 0usize;
    let mut place_bad = 0usize;
    if mode != ChainMode::Records {
        // Arena against arena: the chain side's slots live in
        // `chain_arena` whichever hop filled it (device copy or readback
        // hand-off), possibly across SEVERAL chunk buffers — the compare
        // walks them in order with a running word index, so a window/chunk
        // offset slip lands on a named slot either way.
        let eng_words: &[u32] = bytemuck::cast_slice(arena.instances());
        let chain_chunks = chain_arena.instance_chunks();
        let chain_slots_total: usize = chain_chunks.iter().map(|c| c.len()).sum();
        if eng_words.len() != chain_slots_total * 12 {
            inst_bad += 1;
            println!(
                "  INSTANCE LENGTH MISMATCH: engine {} slots, chain {} slots",
                eng_words.len() / 12,
                chain_slots_total
            );
        } else {
            let mut idx = 0usize;
            for chunk in &chain_chunks {
                let cw: &[u32] = bytemuck::cast_slice(chunk);
                for &b in cw {
                    let a = eng_words[idx];
                    if a != b {
                        inst_bad += 1;
                        if inst_bad <= 4 {
                            println!(
                                "  INSTANCE MISMATCH slot {} word {}: chain {:#x} engine {:#x}",
                                idx / 12,
                                idx % 12,
                                b,
                                a
                            );
                        }
                    }
                    idx += 1;
                }
            }
        }
        for (idx, (gp, cp)) in placements.iter().zip(stream.placements.iter()).enumerate() {
            if !gp.bit_eq(cp) {
                place_bad += 1;
                if place_bad <= 4 {
                    println!(
                        "  PLACEMENT MISMATCH item {}: engine (base {} cnt {} rec {} right {:e} bottom {:e}) chain (base {} cnt {} rec {} right {:e} bottom {:e})",
                        idx,
                        gp.slot_base, gp.slot_count, gp.record_count, gp.page.right, gp.page.bottom,
                        cp.slot_base, cp.slot_count, cp.record_count, cp.page.right, cp.page.bottom
                    );
                }
            }
        }
        drop(arena);
        drop(chain_arena);
        drop(stream.placements);
    }

    // ── the diff ──────────────────────────────────────────────────────────
    // The FORK CENSUS: bit-deviations bucketed BY LANE and by the integer
    // context that produced them — X's page multiplier m (the paginate
    // stride product), Z's wrap segment (the base-Z product), Y's row
    // magnitude (the base-Y product), and X at m == 0 (the line_adv scan
    // tree, which paginate never touches). The one aggregate number that
    // stood here could not tell those classes apart, and the rung-4
    // arithmetic-fork decision turns on exactly this decomposition: each
    // class has a different fix, and one of them (the scan tree) is not
    // fixable in paginate at all.
    let mut bad = 0usize;
    let mut bit_devs = 0usize;
    let mut max_dev = 0.0f64;
    let mut lane_devs = [0usize; 5];
    let mut lane_far = [0usize; 5]; // deviations farther than 1 ulp
    let mut lane_max = [0.0f64; 5];
    let mut x_m = [0usize; 4]; // X deviations at m == 0, 1, 2, >= 3
    let mut z_seg = [0usize; 4]; // Z deviations at segment 0, 1, 2, >= 3
    let mut y_big_row = 0usize; // Y deviations at row > 2048
    let total = total_records as usize;
    // STRICT mode (the fork gate): the claim is BIT-exactness, not the
    // eps tier — any measure-word deviation fails — and the census's
    // m>=3 / seg>=3 buckets must be proven EXERCISED by this corpus, so
    // the gate cannot quietly hollow the way /tmp scratch corpora would.
    let strict = std::env::var_os("GLYPH_REPO_CHECK_STRICT").is_some();
    let mut m3_records = 0usize;
    let mut seg3_records = 0usize;
    let mut shown = 0usize;
    for (o, want) in engine_records.iter().take(total).enumerate() {
        let w = o * 8;
        let got_gi = recs[w + 5];
        let got_row = recs[w + 6];
        let got_col = recs[w + 7];
        let mut ok = got_gi == want.counts[0] && got_row == want.counts[1] && got_col == want.counts[2];
        // The record's paginate context, off the engine-side item params.
        // Computed lazily for the census, unconditionally for the strict
        // denominators (the exercise proof).
        let mut ctx: Option<(i64, i64)> = None;
        if strict && o < item_of.len() {
            let prm = &file_params[item_of[o] as usize];
            let rows_s = if prm.has_page { prm.page_rows as i64 } else { 0 };
            let scroll_s = if prm.has_page { prm.scroll_rows as i64 } else { 0 };
            let wide_s = prm.pages_wide.max(1) as i64;
            let screen_row_s = got_row as i64 - scroll_s;
            let y_page_s = if rows_s > 0 && screen_row_s >= rows_s {
                screen_row_s / rows_s
            } else {
                0
            };
            if y_page_s % wide_s >= 3 {
                m3_records += 1;
            }
            if prm.wrap_width > 0 && (got_col as i64 / prm.wrap_width as i64) >= 3 {
                seg3_records += 1;
            }
        }
        for k in 0..5 {
            let got = f32::from_bits(recs[w + k]);
            let wantm = want.measures[k];
            if got.to_bits() == wantm.to_bits() {
                continue;
            }
            bit_devs += 1;
            lane_devs[k] += 1;
            if shown < 4 && std::env::var_os("GLYPH_CHAIN_DEBUG").is_some() {
                println!(
                    "  dbg dev record {o} lane {k}: chain {:e} ({:#x}) engine {:e} ({:#x}) row {} col {}",
                    got,
                    recs[w + k],
                    wantm,
                    wantm.to_bits(),
                    got_row,
                    got_col
                );
                shown += 1;
            }
            let rel = (got as f64 - wantm as f64).abs() / (wantm as f64).abs().max(1.0);
            if rel > max_dev {
                max_dev = rel;
            }
            if rel > lane_max[k] {
                lane_max[k] = rel;
            }
            // Same-sign f32s order by bit pattern, so the bit distance IS
            // the ulp distance; a cross-sign pair lands absurdly far and
            // counts as far, which is the right verdict for a position.
            let bd = (recs[w + k] as i64)
                .wrapping_sub(wantm.to_bits() as i64)
                .abs();
            if bd > 1 {
                lane_far[k] += 1;
            }
            if k < 3 && ctx.is_none() && o < item_of.len() {
                let prm = &file_params[item_of[o] as usize];
                let rows = if prm.has_page { prm.page_rows as i64 } else { 0 };
                let scroll = if prm.has_page { prm.scroll_rows as i64 } else { 0 };
                let wide = prm.pages_wide.max(1) as i64;
                let screen_row = got_row as i64 - scroll;
                let y_page = if rows > 0 && screen_row >= rows {
                    screen_row / rows
                } else {
                    0
                };
                let wrap_segment = if prm.wrap_width > 0 {
                    got_col as i64 / prm.wrap_width as i64
                } else {
                    0
                };
                ctx = Some((y_page % wide, wrap_segment));
            }
            match k {
                0 => x_m[ctx.map(|(page_col, _)| page_col).unwrap_or(0).min(3) as usize] += 1,
                1 => {
                    if got_row > 2048 {
                        y_big_row += 1;
                    }
                }
                2 => z_seg[ctx.map(|(_, seg_idx)| seg_idx).unwrap_or(0).min(3) as usize] += 1,
                _ => {}
            }
            if rel > 1e-4 {
                ok = false;
            }
        }
        if !ok {
            if bad < 8 {
                println!(
                    "  MISMATCH record {o}: gi {} vs {} row {} vs {} col {} vs {} | x {:e} vs {:e} y {:e} vs {:e} z {:e} vs {:e} adv {:e} vs {:e} hgt {:e} vs {:e}",
                    got_gi, want.counts[0], got_row, want.counts[1], got_col, want.counts[2],
                    f32::from_bits(recs[w]), want.measures[0],
                    f32::from_bits(recs[w + 1]), want.measures[1],
                    f32::from_bits(recs[w + 2]), want.measures[2],
                    f32::from_bits(recs[w + 3]), want.measures[3],
                    f32::from_bits(recs[w + 4]), want.measures[4]
                );
            }
            bad += 1;
        }
    }
    let count_ok = engine_records.len() == total_records as usize
        && engine_total == total_records;
    println!(
        "cubecl-repo-check: {} ({} files, {} B, {} records, {} candidates) — engine {:?} | chain+readback {:?} (readback {:?}) | counts {} — {} record mismatches, {} measure bit-deviations, max {:.2e} (total {:?})",
        dir.display(),
        item_count,
        n,
        total,
        c,
        eng_dt,
        chain_dt,
        readback_dt,
        if count_ok { "MATCH" } else { "DIFFER" },
        bad,
        bit_devs,
        max_dev,
        t_all.elapsed()
    );
    println!(
        "  chain spans: prep {:?} | tables {:?} | init {:?} | pack+upload {:?} | dispatch {:?} (wall; cold-process JIT hides in dispatch)",
        phases.prep, phases.tables, phases.init, phases.upload, phases.dispatch
    );
    if inst_bad > 0 || place_bad > 0 {
        eprintln!(
            "cubecl-repo-check FAIL: {inst_bad} instance mismatch words, {place_bad} placement mismatches"
        );
        std::process::exit(1);
    }
    if bad > 0 || !count_ok || max_dev > 1e-4 {
        eprintln!(
            "cubecl-repo-check FAIL: {bad} record mismatches, counts {}, max deviation {max_dev:.2e}",
            if count_ok { "MATCH" } else { "DIFFER" }
        );
        std::process::exit(1);
    }
    if strict {
        if bit_devs > 0 {
            eprintln!(
                "cubecl-repo-check FAIL (strict): {bit_devs} measure bit-deviations — the fork gate claims bit-exactness"
            );
            std::process::exit(1);
        }
        if m3_records == 0 || seg3_records == 0 {
            eprintln!(
                "cubecl-repo-check FAIL (strict): census not exercised — {m3_records} records at m >= 3, {seg3_records} at segment >= 3; the corpus cannot see the fork classes it exists to fence"
            );
            std::process::exit(1);
        }
        println!(
            "strict: bit-exact across all lanes; {m3_records} records at m >= 3, {seg3_records} at segment >= 3 (exercised)"
        );
    }
    // The census line prints on PASS too — it is the instrument that prices
    // the rung-4 fork, and a zero-deviation run is its most important datum.
    println!(
        "cubecl-repo-check census: \
         X {} (max {:.2e}, {} >1ulp; m0 {} m1 {} m2 {} m3+ {}) | \
         Y {} (max {:.2e}, {} >1ulp, {} at row>2048) | \
         Z {} (max {:.2e}, {} >1ulp; seg0 {} seg1 {} seg2 {} seg3+ {}) | \
         adv {} | hgt {}",
        lane_devs[0],
        lane_max[0],
        lane_far[0],
        x_m[0],
        x_m[1],
        x_m[2],
        x_m[3],
        lane_devs[1],
        lane_max[1],
        lane_far[1],
        y_big_row,
        lane_devs[2],
        lane_max[2],
        lane_far[2],
        z_seg[0],
        z_seg[1],
        z_seg[2],
        z_seg[3],
        lane_devs[3],
        lane_devs[4],
    );
    println!(
        "cubecl-repo-check PASS: glyph_id/row/col exact, measures inside 1e-4 ({} of {} measure words carry a last-bit f32 deviation — the documented reassociation tier)",
        bit_devs,
        total * 5
    );
    if mode != ChainMode::Records {
        println!(
            "instance tier: {} slots byte-equal, {} placements bit-equal (the pack kernel vs the engine-batched arena)",
            stream.total_slots, item_count
        );
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    /// The phantom-tail class (see pack_words): a non-word-aligned corpus
    /// must fill the final word's tail lanes with 0x80 continuation leads —
    /// zero pads decode on device as phantom NUL leaders. This test is the
    /// reddening witness for the `tail-pads-zero` mutation: the device gates
    /// cannot carry it (trailing phantom records self-truncate past the
    /// record count, so the cubecl gates stay green through the bug — the
    /// fork gate's attempted mutation was dropped for exactly that), while
    /// the classifier's continuation rule they DO fence is only half the
    /// class. Three of the five cubecl-chain gate fixtures are
    /// non-word-aligned; the standing fork corpus's 278,470 bytes are too.
    #[test]
    fn pack_words_fills_tail_lanes() {
        assert_eq!(super::pack_words(&[0x41]), vec![0x8080_8041u32]);
        assert_eq!(super::pack_words(&[0x41, 0x42]), vec![0x8080_4241u32]);
        // Word-aligned input: no fill, the words carry exactly the bytes.
        assert_eq!(
            super::pack_words(&[0x41, 0x42, 0x43, 0x44]),
            vec![0x4443_4241u32]
        );
        // Empty input: no words at all (the pre-helper behavior).
        assert!(super::pack_words(&[]).is_empty());
    }
}
