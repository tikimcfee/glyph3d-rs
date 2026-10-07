use cubecl::prelude::*;

use super::cluster::{cp_at, seq_len_at};
use super::decode::decode_trie;

use super::monoid::{
    combine, flags_at, identity, item_search_desc, leaf_from_flag, leaf_of,
    ordered_key, p_load, p_store, rows_for, s_load, s_store, wrap_row_of, wrap_segment_of,
};
use super::{
    F_CLUSTER_HEAD, F_CLUSTER_TRAILER, F_LEADER, F_NEWLINE,
    ITEM_DESC_BYTE_START, ITEM_DESC_CELL_ADVANCE, ITEM_DESC_HAS_PAGE, ITEM_DESC_LINE_HEIGHT, ITEM_DESC_ORIGIN_X,
    ITEM_DESC_ORIGIN_Y, ITEM_DESC_ORIGIN_Z, ITEM_DESC_PAGE_COLS, ITEM_DESC_STRIDE,
    ITEM_DESC_WRAP_MODE, ITEM_DESC_WRAP_WIDTH, ITEM_DESC_Z_STEP, ITEM_DESC_Z_STEP_LO,
    LC_COL, LC_ROW, LC_STRIDE, LM_STRIDE, LM_X, LM_Y, LM_Z, P_MODE, P_WRAP,
    PARTIAL_COUNT_STRIDE, RESOLVE_SLOTS,
};

// ── dispatch 1: tileScan — one cube per tile; rake + workgroup Blelloch ──────
//
// Each unit rakes its `rake` bytes into one monoid element (a serial,
// order-preserving micro-fold); the cube Blelloch-scans the `units` partials
// in shared memory into exclusive micro prefixes; unit units-1 publishes the
// tile total. The critical path per tile is rake + 2·log(units) combines —
// against the old thread-per-chunk serial fold's one-thread `chunk`-deep
// chain with units-fold less parallelism.
#[allow(clippy::manual_range_contains)]
#[cube(launch_unchecked)]
pub(super) fn tile_scan(
    glyph_flags: &[u32],
    bytes: &[u32],
    trie_block_indices: &[u32],
    trie_block_metrics: &[f32],
    trie_block_codepoints: &[u32],
    #[comptime] trie_block_shift: u32,
    bitmap_advance: f32,
    item_descriptors: &[u32],
    tile_counts: &mut [u32],
    tile_metrics: &mut [f32],
    #[comptime] threads_per_cube: usize,
    #[comptime] bytes_per_thread: usize,
    #[comptime] log: usize,
) {
    let tile_idx = CUBE_POS;
    let unit_idx = UNIT_POS as usize;
    let total_bytes = glyph_flags.len() * 4; // packed: words -> bytes
    let item_count = item_descriptors.len() / ITEM_DESC_STRIDE;
    let range_start = tile_idx * (threads_per_cube * bytes_per_thread) + unit_idx * bytes_per_thread;
    let range_end = if range_start + bytes_per_thread < total_bytes { range_start + bytes_per_thread } else { total_bytes };

    #[allow(clippy::len_zero)]
    let ascii_block_base = if trie_block_indices.len() > 0 {
        trie_block_indices[0] << trie_block_shift
    } else {
        0u32
    };

    let total_tile_bytes = threads_per_cube * bytes_per_thread;
    let tile_byte_start = tile_idx * total_tile_bytes;
    let tile_word_start = tile_byte_start >> 2;
    let total_tile_words = total_tile_bytes.div_ceil(4);

    let mut shared_tile_flags = Shared::<[u32]>::new_slice((threads_per_cube * bytes_per_thread) / 4);

    let mut preload_word_idx = unit_idx;
    while preload_word_idx < total_tile_words {
        let global_word_idx = tile_word_start + preload_word_idx;
        shared_tile_flags[preload_word_idx] = if global_word_idx < glyph_flags.len() {
            glyph_flags[global_word_idx]
        } else {
            0u32
        };
        preload_word_idx += threads_per_cube;
    }
    sync_cube();

    // Register-resident bytes for this thread's 8-byte range
    let thread_word_idx = range_start >> 2;
    let thread_bytes_word0 = if thread_word_idx < bytes.len() {
        bytes[thread_word_idx]
    } else {
        0x8080_8080u32
    };
    let thread_bytes_word1 = if thread_word_idx + 1 < bytes.len() {
        bytes[thread_word_idx + 1]
    } else {
        0x8080_8080u32
    };

    // ItemWalk seed. For pad units (range_start >= total_bytes) the seed clamps to the last byte
    // so the pad element carries the wrap/mode in force at the tile's end —
    // see the module header for why a pure-identity pad would poison the
    // tile total's wrap lanes. (total_bytes == 0 makes the clamp wrap; the seed then
    // feeds only an unused walk, and every descriptor read stays in bounds.)
    let mut seed = range_start;
    if seed >= total_bytes {
        seed = total_bytes - 1;
    }
    let mut item_index = 0usize;
    let mut start = 0usize;
    let mut next_item_boundary = total_bytes;
    let mut active_wrap_width = 0i32;
    let mut active_wrap_mode = 0i32;
    let mut active_cell_advance_bits = 0u32;
    let has_items = item_count > 0;
    if has_items {
        item_index = item_search_desc(item_descriptors, item_count, seed);
        start = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize;
        next_item_boundary = if item_index + 1 < item_count {
            item_descriptors[(item_index + 1) * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize
        } else {
            total_bytes
        };
        active_wrap_width = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_WRAP_WIDTH] as i32;
        active_wrap_mode = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_WRAP_MODE] as i32;
        active_cell_advance_bits = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_CELL_ADVANCE];
    }
    let mut accumulator = identity();
    if range_start < total_bytes {
        let mut id = range_start;
        while id < range_end {
            while has_items && next_item_boundary <= id {
                item_index += 1;
                start = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize;
                next_item_boundary = if item_index + 1 < item_count {
                    item_descriptors[(item_index + 1) * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize
                } else {
                    total_bytes
                };
                active_wrap_width = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_WRAP_WIDTH] as i32;
                active_wrap_mode = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_WRAP_MODE] as i32;
                active_cell_advance_bits = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_CELL_ADVANCE];
            }
            let reset = if has_items && id == start { 1i32 } else { 0i32 };
            let local_id = id - tile_byte_start;
            let glyph_flag = (shared_tile_flags[local_id >> 2] >> (((local_id & 3) * 8) as u32)) & 0xFF;
            let advance = if (glyph_flag & super::F_LEADER) != 0 {
                if (glyph_flag & F_CLUSTER_HEAD) != 0 {
                    bitmap_advance
                } else if (glyph_flag & F_CLUSTER_TRAILER) != 0 {
                    0.0f32
                } else {
                    let lane = id - range_start;
                    let lead_byte = if lane < 4 {
                        (thread_bytes_word0 >> ((lane * 8) as u32)) & 0xFF
                    } else {
                        (thread_bytes_word1 >> (((lane - 4) * 8) as u32)) & 0xFF
                    };
                    if lead_byte >= 32u32 && lead_byte <= 126u32 {
                        f32::from_bits(active_cell_advance_bits)
                    } else if lead_byte < 128u32 {
                        let entry_offset = (ascii_block_base | lead_byte) as usize;
                        trie_block_metrics[entry_offset * 2]
                    } else {
                        let cp_len = seq_len_at(bytes, id, total_bytes);
                        let cp = cp_at(bytes, id, cp_len, total_bytes);
                        let (adv, _) = decode_trie(cp, trie_block_indices, trie_block_metrics, trie_block_codepoints, trie_block_shift);
                        adv
                    }
                }
            } else {
                0.0f32
            };
            let leaf = leaf_from_flag(glyph_flag, advance, active_wrap_width, active_wrap_mode, reset, active_cell_advance_bits);
            combine(&mut accumulator, &leaf);
            id += 1;
        }
    } else {
        accumulator.wrap = active_wrap_width;
        accumulator.mode = active_wrap_mode;
    }

    let mut shared_counts = Shared::<[i32]>::new_slice(threads_per_cube * PARTIAL_COUNT_STRIDE);
    let mut shared_metrics = Shared::<[f32]>::new_slice(threads_per_cube);
    s_store(&mut shared_counts, &mut shared_metrics, unit_idx, &accumulator);

    // Up-sweep: x[unit_idx] = combine(x[unit_idx-s], x[unit_idx]) at the ends of 2s-blocks. The
    // LEFT operand is the lower element — the fold order.
    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = 1usize << d;
        if (unit_idx + 1) & (2 * s - 1) == 0 {
            let mut lhs = s_load(&shared_counts, &shared_metrics, unit_idx - s);
            let rhs = s_load(&shared_counts, &shared_metrics, unit_idx);
            combine(&mut lhs, &rhs);
            s_store(&mut shared_counts, &mut shared_metrics, unit_idx, &lhs);
        }
    }
    // The tile TOTAL is this dispatch's only output: the up-sweep leaves it
    // at the root. No down-sweep — exclusive prefixes would land in shared
    // memory that dies with the kernel; apply_and_emit re-rakes the tile and
    // runs its own Blelloch seeded from the spine. (A down-sweep lived here
    // until 2026-10-05: eight barriers and ~255 combines per tile whose
    // results nothing read.)
    sync_cube();
    if unit_idx == threads_per_cube - 1 {
        let total = s_load(&shared_counts, &shared_metrics, unit_idx);
        p_store(tile_counts, tile_metrics, tile_idx, &total);
    }
}

// ── dispatch 2: spineScan — ONE cube over the tile totals ────────────────────
//
// Each unit owns a CONTIGUOUS block of tile totals (order-preserving for the
// non-commutative monoid — a strided rake would interleave blocks across
// units and the workgroup scan could not recompose them), rakes them into
// one partial, the cube Blelloch-scans the units, then each unit chases its
// block writing global exclusive tile prefixes. One cube handles
// units·(tiles/units) tiles by raking deeper; the scaling path beyond that
// is a second spine level, not built yet.
#[cube(launch_unchecked)]
pub(super) fn spine_scan(
    tile_counts: &[u32],
    tile_metrics: &[f32],
    tile_spine_counts: &mut [u32],
    tile_spine_metrics: &mut [f32],
    #[comptime] threads_per_cube: usize,
    #[comptime] log: usize,
) {
    let thread_idx = UNIT_POS as usize;
    let n_tiles = tile_counts.len() / PARTIAL_COUNT_STRIDE;
    let per = n_tiles.div_ceil(threads_per_cube);
    let first = thread_idx * per;
    let last = if first + per < n_tiles { first + per } else { n_tiles };
    let mut acc = identity();
    if first < n_tiles {
        for t in first..last {
            let e = p_load(tile_counts, tile_metrics, t);
            combine(&mut acc, &e);
        }
    } else {
        // Pad: identity counts, the LAST tile's wrap/mode — see module header.
        let o = (n_tiles - 1) * PARTIAL_COUNT_STRIDE;
        acc.wrap = tile_counts[o + P_WRAP] as i32;
        acc.mode = tile_counts[o + P_MODE] as i32;
    }

    let mut shared_counts = Shared::<[i32]>::new_slice(threads_per_cube * PARTIAL_COUNT_STRIDE);
    let mut shared_metrics = Shared::<[f32]>::new_slice(threads_per_cube);
    s_store(&mut shared_counts, &mut shared_metrics, thread_idx, &acc);

    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = 1usize << d;
        if (thread_idx + 1) & (2 * s - 1) == 0 {
            let mut lhs = s_load(&shared_counts, &shared_metrics, thread_idx - s);
            let rhs = s_load(&shared_counts, &shared_metrics, thread_idx);
            combine(&mut lhs, &rhs);
            s_store(&mut shared_counts, &mut shared_metrics, thread_idx, &lhs);
        }
    }
    sync_cube();
    if thread_idx == threads_per_cube - 1 {
        let e = identity();
        s_store(&mut shared_counts, &mut shared_metrics, thread_idx, &e);
    }
    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = threads_per_cube >> (d + 1);
        if (thread_idx + 1) & (2 * s - 1) == 0 {
            let temp_carried = s_load(&shared_counts, &shared_metrics, thread_idx);
            let mut lhs = s_load(&shared_counts, &shared_metrics, thread_idx);
            let rhs = s_load(&shared_counts, &shared_metrics, thread_idx - s);
            combine(&mut lhs, &rhs);
            s_store(&mut shared_counts, &mut shared_metrics, thread_idx, &lhs);
            s_store(&mut shared_counts, &mut shared_metrics, thread_idx - s, &temp_carried);
        }
    }
    sync_cube();

    // Chase: this unit's block of tiles gets its global exclusive prefixes.
    let mut pre = s_load(&shared_counts, &shared_metrics, thread_idx);
    if first < n_tiles {
        for t in first..last {
            p_store(tile_spine_counts, tile_spine_metrics, t, &pre);
            let e = p_load(tile_counts, tile_metrics, t);
            combine(&mut pre, &e);
        }
    }
}

/// The fold width in force for item `item_index`: its wrap width, or its page
/// columns when it has no wrap. fold==0 means x IS the line-advance lane —
/// no segment re-sum exists.
#[cube]
pub(super) fn fold_of(item_descriptors: &[u32], item_index: usize, wrap_width: i32) -> i32 {
    if wrap_width > 0 {
        wrap_width
    } else if item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_HAS_PAGE] != 0 {
        item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_PAGE_COLS] as i32
    } else {
        0
    }
}

// ── dispatch 3: apply — rake + Blelloch + per-byte chase ─────────────────────
//
// Same tile decomposition as tile_scan (the rake and tree are re-derived).
// The publish-the-micro-prefixes alternative was BUILT AND MEASURED
// (2026-09-27) and LOST: apply 21.0 -> 31.4ms, chain 28.6 -> 40.2. The
// rake's "redundant" re-read of fl/sm rides L1 — the chase re-reads the
// same 8 bytes it just raked — so it is nearly free, while 36B/unit of
// published prefixes are cold global traffic, and the tree's 18 barriers
// are cheap. Reverted; do not re-derive this trade, re-measure it. Each
// unit chases its `rake` bytes seeded with combine(global tile prefix, own
// exclusive micro prefix), emitting the per-byte lanes.
//
// THE TREE ITSELF costs ~2.3-2.6ms per kernel that carries one (measured
// same day by emptying the loops: tile_scan 5.6 -> 3.3ms), so ~5ms of the
// ~25.6ms chain — the C6 plane-op restructure's ceiling. It stays parked:
// plane ops are subgroup intrinsics on the WGSL path (a pinned-naga
// behavior, absent on weaker backends), they diverge per compute target
// (the CPU runtime's PLANE_DIM differs), and they are scalar-numeric-only,
// so this monoid would need a manual shuffle tree with the non-commutative
// down-swap rebuilt lane by lane. If taken: as a #[comptime] capability-
// gated variant beside this default, the inline_resolve pattern — never as
// the foundation.
//
// FOLDLESS ITEMS RESOLVE HERE: for fold==0 the chase already holds
// everything resolve_x would recompute — x IS run.tail_adv, and the old
// kernel read ~700MB back (fl, lc, wm, wc, otb) to recover values that were
// in registers at this exact point. The chase writes their lm lanes and
// reduces their maxima directly; wm/wc/otb are written ONLY for fold>0
// items, whose segment re-sum genuinely needs the cross-unit ordinal table,
// and the resolve_x dispatch is skipped entirely when no item folds. Row
// maxima reduce here for EVERY item (rows are final in the chase); x maxima
// only where x is final (fold==0) — resolve_x owns the fold>0 x maxima.
//
// `inline_resolve` is COMPTIME (the k_decode_probe[probe,walk] pattern): the
// inline branch's live state (four item-parameter loads, the lm writes)
// inflates the whole kernel's register set, and measured on a pure-wrapped
// corpus that costs 2x on the division-heavy fold>0 path — stalls that the
// memory-bound foldless path hides. Corpora where EVERY item folds compile
// the inline branch out entirely; mixed and foldless corpora compile it in.
#[cube(launch_unchecked)]
pub(super) fn apply(
    glyph_flags: &[u32],
    bytes: &[u32],
    trie_block_indices: &[u32],
    trie_block_metrics: &[f32],
    trie_block_codepoints: &[u32],
    #[comptime] trie_block_shift: u32,
    line_columns: &mut [u32],
    layout_metrics: &mut [f32],
    item_descriptors: &[u32],
    tile_spine_counts: &[u32],
    tile_spine_metrics: &[f32],
    item_record_ordinals: &mut [u32],
    ordinal_to_byte_map: &mut [u32],
    item_row_max: &mut [Atomic<u32>],
    item_x_max: &mut [Atomic<u32>],
    #[comptime] units: usize,
    #[comptime] rake: usize,
    #[comptime] log: usize,
    #[comptime] inline_resolve: bool,
    #[comptime] skip_otb: bool,
    #[comptime] skip_wc: bool,
) {
    let tile_idx = CUBE_POS;
    let unit_idx = UNIT_POS as usize;
    let total_bytes = glyph_flags.len() * 4; // packed: words -> bytes
    let item_count = item_descriptors.len() / ITEM_DESC_STRIDE;
    let range_start = tile_idx * (units * rake) + unit_idx * rake;
    let range_end = if range_start + rake < total_bytes { range_start + rake } else { total_bytes };
    // Maxima-reduction slots, seeded before the tree so its barriers cover
    // visibility (see resolve_x's header for the slot protocol). The whole
    // apparatus is compiled out for pure-wrapped corpora: measured there,
    // the per-leader RMWs cost ~8ms inside apply's division-stalled chase,
    // while riding FREE inside resolve_x, whose re-sum already dominates —
    // so resolve_x owns both maxima for that shape (fetch_max idempotence
    // makes the mixed-corpus double reduction harmless).
    let shared_row_max = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS);
    let shared_x_max = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS);
    let mut shared_item_base = Shared::<u32>::new();
    let tile_lo = tile_idx * (units * rake);
    let mut tile_item_base = 0usize;
    if inline_resolve {
        if unit_idx == 0 {
            let probe = if tile_lo < total_bytes { tile_lo } else { total_bytes - 1 };
            let mut probe_item = 0usize;
            if item_count > 0 {
                probe_item = item_search_desc(item_descriptors, item_count, probe);
            }
            *shared_item_base = probe_item as u32;
        }
        let mut z = unit_idx;
        while z < RESOLVE_SLOTS {
            shared_row_max[z].store(0u32);
            shared_x_max[z].store(0u32);
            z += units;
        }
    }
    let mut seed = range_start;
    if seed >= total_bytes {
        seed = total_bytes - 1;
    }
    let mut item_index = 0usize;
    let mut start = 0usize;
    let mut next_item_boundary = total_bytes;
    let mut active_wrap_width = 0i32;
    let mut active_wrap_mode = 0i32;
    let mut active_cell_advance_bits = 0u32;
    let has_items = item_count > 0;
    if has_items {
        item_index = item_search_desc(item_descriptors, item_count, seed);
        start = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize;
        next_item_boundary = if item_index + 1 < item_count {
            item_descriptors[(item_index + 1) * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize
        } else {
            total_bytes
        };
        active_wrap_width = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_WRAP_WIDTH] as i32;
        active_wrap_mode = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_WRAP_MODE] as i32;
        active_cell_advance_bits = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_CELL_ADVANCE];
    }
    let mut accumulator = identity();
    if range_start < total_bytes {
        let mut id = range_start;
        while id < range_end {
            while has_items && next_item_boundary <= id {
                item_index += 1;
                start = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize;
                next_item_boundary = if item_index + 1 < item_count {
                    item_descriptors[(item_index + 1) * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize
                } else {
                    total_bytes
                };
                active_wrap_width = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_WRAP_WIDTH] as i32;
                active_wrap_mode = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_WRAP_MODE] as i32;
                active_cell_advance_bits = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_CELL_ADVANCE];
            }
            let reset = if has_items && id == start { 1i32 } else { 0i32 };
            let advance = if (flags_at(glyph_flags, id) & super::F_LEADER) != 0 {
                let cp_len = seq_len_at(bytes, id, total_bytes);
                let cp = cp_at(bytes, id, cp_len, total_bytes);
                let (adv, _) = decode_trie(cp, trie_block_indices, trie_block_metrics, trie_block_codepoints, trie_block_shift);
                adv
            } else {
                0.0f32
            };
            let leaf = leaf_of(glyph_flags, advance, active_wrap_width, active_wrap_mode, reset, active_cell_advance_bits, id);
            combine(&mut accumulator, &leaf);
            id += 1;
        }
    } else {
        accumulator.wrap = active_wrap_width;
        accumulator.mode = active_wrap_mode;
    }

    let mut shared_counts = Shared::<[i32]>::new_slice(units * PARTIAL_COUNT_STRIDE);
    let mut shared_metrics = Shared::<[f32]>::new_slice(units);
    s_store(&mut shared_counts, &mut shared_metrics, unit_idx, &accumulator);

    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = 1usize << d;
        if (unit_idx + 1) & (2 * s - 1) == 0 {
            let mut lhs = s_load(&shared_counts, &shared_metrics, unit_idx - s);
            let rhs = s_load(&shared_counts, &shared_metrics, unit_idx);
            combine(&mut lhs, &rhs);
            s_store(&mut shared_counts, &mut shared_metrics, unit_idx, &lhs);
        }
    }
    sync_cube();
    if unit_idx == units - 1 {
        let e = identity();
        s_store(&mut shared_counts, &mut shared_metrics, unit_idx, &e);
    }
    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = units >> (d + 1);
        if (unit_idx + 1) & (2 * s - 1) == 0 {
            let temp_carried = s_load(&shared_counts, &shared_metrics, unit_idx);
            let mut lhs = s_load(&shared_counts, &shared_metrics, unit_idx);
            let rhs = s_load(&shared_counts, &shared_metrics, unit_idx - s);
            combine(&mut lhs, &rhs);
            s_store(&mut shared_counts, &mut shared_metrics, unit_idx, &lhs);
            s_store(&mut shared_counts, &mut shared_metrics, unit_idx - s, &temp_carried);
        }
    }
    sync_cube();

    // The chase: the running prefix at this unit's first byte is the global
    // tile prefix combined with the exclusive micro prefix from the tree.
    // The walk is RE-SEEDED first — the rake advanced it past this unit's
    // range, and each byte's reset/wrap/mode must come from ITS item, not
    // the range's last one (the multi-item fixtures caught exactly this).
    let mut run = p_load(tile_spine_counts, tile_spine_metrics, tile_idx);
    let micro = s_load(&shared_counts, &shared_metrics, unit_idx);
    combine(&mut run, &micro);
    if inline_resolve {
        tile_item_base = *shared_item_base as usize;
    }
    item_index = 0usize;
    start = 0usize;
    next_item_boundary = total_bytes;
    active_wrap_width = 0i32;
    active_wrap_mode = 0i32;
    active_cell_advance_bits = 0u32;
    let mut active_fold_width = 0i32;
    if has_items {
        item_index = item_search_desc(item_descriptors, item_count, range_start);
        start = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize;
        next_item_boundary = if item_index + 1 < item_count {
            item_descriptors[(item_index + 1) * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize
        } else {
            total_bytes
        };
        active_wrap_width = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_WRAP_WIDTH] as i32;
        active_wrap_mode = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_WRAP_MODE] as i32;
        active_cell_advance_bits = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_CELL_ADVANCE];
        active_fold_width = fold_of(item_descriptors, item_index, active_wrap_width);
    }
    if range_start < total_bytes {
        let mut id = range_start;
        while id < range_end {
            while has_items && next_item_boundary <= id {
                item_index += 1;
                start = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize;
                next_item_boundary = if item_index + 1 < item_count {
                    item_descriptors[(item_index + 1) * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize
                } else {
                    total_bytes
                };
                active_wrap_width = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_WRAP_WIDTH] as i32;
                active_wrap_mode = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_WRAP_MODE] as i32;
                active_cell_advance_bits = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_CELL_ADVANCE];
                active_fold_width = fold_of(item_descriptors, item_index, active_wrap_width);
            }
            let reset = has_items && id == start;
            if reset {
                // run = identity(), then the item's params ride again.
                run.reset = 0;
                run.nl = 0;
                run.glyphs = 0;
                run.rows = 0;
                run.head_len = 0;
                run.tail_len = 0;
                run.tail_adv = 0.0;
                run.wrap = active_wrap_width;
                run.mode = active_wrap_mode;
            }
            let glyph_flags_val = flags_at(glyph_flags, id);
            if (glyph_flags_val & F_LEADER) != 0 {
                // lanes_from_prefix, inline.
                let col = run.tail_len;
                let mut closed = 0i32;
                if run.nl > 0 {
                    closed = rows_for(run.head_len, active_wrap_width, active_wrap_mode) + run.rows;
                }
                let wr = wrap_row_of(col, active_wrap_width, (glyph_flags_val & F_NEWLINE) != 0, active_wrap_mode);
                let row = closed + wr;
                let column_offset = id * LC_STRIDE;
                line_columns[column_offset + LC_ROW] = row as u32;
                line_columns[column_offset + LC_COL] = col as u32;
                if inline_resolve {
                    // Rows are final here for every item — reduce them all.
                    // (Compiled out for pure-wrapped corpora: resolve_x owns
                    // both maxima there — see the slot-seeding note above.)
                    let slot = item_index - tile_item_base;
                    if slot < RESOLVE_SLOTS {
                        shared_row_max[slot].fetch_max((row + 1) as u32);
                    } else {
                        item_row_max[item_index].fetch_max((row + 1) as u32);
                    }
                }
                // The forward ordinal map (wc: byte -> item-relative
                // ordinal) and the inverse table (otb) are written on
                // EVERY path — the record emitter (phase 4 rung 2)
                // gathers through wc, and the foldless inline path used
                // to leave both unwritten.
                if !skip_wc {
                    item_record_ordinals[id] = run.glyphs as u32;
                }
                if !skip_otb {
                    ordinal_to_byte_map[start + run.glyphs as usize] = id as u32;
                }
                if inline_resolve && active_fold_width == 0 {
                    // Foldless: x IS the line-advance lane — resolve_x's
                    // whole per-item computation, in registers, now.
                    let x = run.tail_adv;
                    let desc_offset = item_index * ITEM_DESC_STRIDE;
                    let wrap_segment = wrap_segment_of(col, active_wrap_width, (glyph_flags_val & F_NEWLINE) != 0);
                    let line_height = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_LINE_HEIGHT]);
                    let origin_x = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_ORIGIN_X]);
                    let origin_y = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_ORIGIN_Y]);
                    let origin_z = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_ORIGIN_Z]);
                    let z_step = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_Z_STEP]);
                    let z_step_lo = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_Z_STEP_LO]);

                    let metrics_offset = id * LM_STRIDE;
                    let base = x + origin_x;
                    layout_metrics[metrics_offset + LM_X] = base;
                    // Y/Z: one OPAQUE fma each — the engine's f64
                    // two-term expressions narrowed once, bit-exact (see
                    // paginate's fma note for why opaque); Z folds the
                    // z_step tail through the second fma.
                    layout_metrics[metrics_offset + LM_Y] = fma(-(row as f32), line_height, origin_y);
                    let depth_steps = -(wrap_segment as f32);
                    let z_tail_folded = fma(
                        depth_steps,
                        z_step_lo,
                        origin_z,
                    );
                    layout_metrics[metrics_offset + LM_Z] = fma(depth_steps, z_step, z_tail_folded);
                    let slot = item_index - tile_item_base;
                    if slot < RESOLVE_SLOTS {
                        shared_x_max[slot].fetch_max(ordered_key(x));
                    } else {
                        item_x_max[item_index].fetch_max(ordered_key(x));
                    }
                }
            }
            let advance = if (flags_at(glyph_flags, id) & super::F_LEADER) != 0 {
                let cp_len = seq_len_at(bytes, id, total_bytes);
                let cp = cp_at(bytes, id, cp_len, total_bytes);
                let (adv, _) = decode_trie(cp, trie_block_indices, trie_block_metrics, trie_block_codepoints, trie_block_shift);
                adv
            } else {
                0.0f32
            };
            let leaf = leaf_of(glyph_flags, advance, active_wrap_width, active_wrap_mode, if reset { 1i32 } else { 0i32 }, active_cell_advance_bits, id);
            combine(&mut run, &leaf);
            id += 1;
        }
    }
    if inline_resolve {
        sync_cube();
        if unit_idx < RESOLVE_SLOTS {
            let item_target = tile_item_base + unit_idx;
            if item_target < item_count {
                item_row_max[item_target].fetch_max(shared_row_max[unit_idx].load());
                item_x_max[item_target].fetch_max(shared_x_max[unit_idx].load());
            }
        }
    }
}
