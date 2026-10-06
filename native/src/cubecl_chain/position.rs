use cubecl::prelude::*;

use super::monoid::{
    advance_fixed, combine, fixed_pair, flags_at, identity, is_survivor, item_search_desc,
    key_to_float, leaf_of, ordered_key, p_load, rows_for, s_load, s_store, wrap_row_of,
    wrap_segment_of,
};
use super::scan::fold_of;
use super::{
    F_LEADER, F_NEWLINE,
    ITEM_DESC_BAND_STRIDE_Y, ITEM_DESC_BYTE_START, ITEM_DESC_CELL_ADVANCE, ITEM_DESC_COLOR_BASE, ITEM_DESC_DEPTH_PER_BAND,
    ITEM_DESC_DEPTH_PER_COL, ITEM_DESC_FLAT_COLOR, ITEM_DESC_GROUP, ITEM_DESC_HAS_PAGE,
    ITEM_DESC_IS_PER_RECORD, ITEM_DESC_LINE_HEIGHT, ITEM_DESC_ORIGIN_X, ITEM_DESC_ORIGIN_Y,
    ITEM_DESC_ORIGIN_Z, ITEM_DESC_PAGE_COLS, ITEM_DESC_PAGE_GAP_X, ITEM_DESC_PAGE_ROWS,
    ITEM_DESC_PAGES_WIDE, ITEM_DESC_SCROLL_ROWS, ITEM_DESC_STRIDE, ITEM_DESC_WRAP_MODE,
    ITEM_DESC_WRAP_WIDTH, ITEM_DESC_Z_STEP, ITEM_DESC_Z_STEP_LO,
    LC_COL, LC_ROW, LC_STRIDE, LM_STRIDE, LM_X, LM_Y, LM_Z, PARTIAL_COUNT_STRIDE, RESOLVE_SLOTS,
};
use super::tail::EXT_STRIDE;

// ── dispatch 4: resolveX — the WRAPPED items' x, range workers ────────────────
//
// SKIPPED ENTIRELY when no item folds (the driver knows): foldless items
// resolve inside apply's chase, which holds x in a register.
//
// Each worker owns a `span`-byte range (apply's unit decomposition without
// the tree). At the first leader of a segment it enters, the worker walks
// BACKWARD once — at most fold dependent loads — to compute the entry x;
// from there it sweeps FORWARD through its range: every leader's x is the
// running sum, the same additions in the same left-fold order as the old
// per-leader backward re-sum, so the fold>0 X lanes stay BIT-exact (the
// check instrument witnesses that lane at bit level). Heads (col % fold ==
// 0; line and item starts are col==0) re-zero for free. Total backward
// work drops fold-fold (one entry walk per range, not per leader) and
// every worker stays busy — the first draft of this kernel gave each
// SEGMENT to its head and measured 4x WORSE (129ms vs 32): ~2% of threads
// active on long dependent chains is latency-bound with the machine idle.
// Both maxima reduce here (apply's are compiled out for this shape). An
// untouched slot flushes 0, which cannot beat a real value: rows count
// from 1 and every x >= 0 has an ordered key above 0.
#[cube(launch_unchecked)]
pub(super) fn resolve_x(
    advance_widths: &[f32],
    glyph_flags: &[u32],
    layout_metrics: &mut [f32],
    line_columns: &[u32],
    item_descriptors: &[u32],
    item_record_ordinals: &[u32],
    ordinal_to_byte_map: &[u32],
    item_row_max: &mut [Atomic<u32>],
    item_x_max: &mut [Atomic<u32>],
    max_row_extents: &[u32],
    #[comptime] units: usize,
    #[comptime] span: usize,
) {
    let thread_pos = ABSOLUTE_POS;
    let total_bytes = glyph_flags.len() * 4; // packed: words -> bytes
    let item_count = item_descriptors.len() / ITEM_DESC_STRIDE;
    let unit_idx = UNIT_POS as usize;
    let shared_row_max = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS);
    let shared_x_max = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS);
    let mut shared_item_base = Shared::<u32>::new();
    let cube_lo = CUBE_POS * units * span;
    if unit_idx == 0 {
        // The item at this cube's first byte anchors the slot numbering.
        let probe = if cube_lo < total_bytes { cube_lo } else { total_bytes - 1 };
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
    sync_cube();
    let cube_item_base = *shared_item_base as usize;

    let range_start = thread_pos * span;
    if range_start < total_bytes {
        let range_end = if range_start + span < total_bytes { range_start + span } else { total_bytes };
        // The item walk, seeded at the range start.
        let mut item_index = 0usize;
        let mut start = 0usize;
        let mut next_item_boundary = total_bytes;
        let mut wrap = 0i32;
        let mut fold = 0i32;
        let mut line_height = 0.0f32;
        let mut origin_x = 0.0f32;
        let mut origin_y = 0.0f32;
        let mut origin_z = 0.0f32;
        let mut z_step = 0.0f32;
        let mut z_step_lo = 0.0f32;
        let mut band_stride_y = 0.0f32;
        let mut depth_per_band = 0.0f32;
        let mut depth_per_col = 0.0f32;
        let mut rows = 0i32;
        let mut cols = 0i32;
        let mut scroll = 0i32;
        let mut pages_wide = 1i32;
        let mut stride_reach = 0.0f32;
        let mut stride_reach_tail = 0.0f32;
        let has_items = item_count > 0;
        if has_items {
            item_index = item_search_desc(item_descriptors, item_count, range_start);
            let desc_offset = item_index * ITEM_DESC_STRIDE;
            start = item_descriptors[desc_offset + ITEM_DESC_BYTE_START] as usize;
            next_item_boundary = if item_index + 1 < item_count {
                item_descriptors[(item_index + 1) * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize
            } else {
                total_bytes
            };
            wrap = item_descriptors[desc_offset + ITEM_DESC_WRAP_WIDTH] as i32;
            fold = fold_of(item_descriptors, item_index, wrap);
            line_height = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_LINE_HEIGHT]);
            origin_x = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_ORIGIN_X]);
            origin_y = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_ORIGIN_Y]);
            origin_z = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_ORIGIN_Z]);
            z_step = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_Z_STEP]);
            z_step_lo = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_Z_STEP_LO]);
            band_stride_y = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_BAND_STRIDE_Y]);
            depth_per_band = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_DEPTH_PER_BAND]);
            depth_per_col = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_DEPTH_PER_COL]);
            let has_page = item_descriptors[desc_offset + ITEM_DESC_HAS_PAGE] != 0;
            rows = if has_page { item_descriptors[desc_offset + ITEM_DESC_PAGE_ROWS] as i32 } else { 0 };
            cols = if has_page { item_descriptors[desc_offset + ITEM_DESC_PAGE_COLS] as i32 } else { 0 };
            scroll = if has_page { item_descriptors[desc_offset + ITEM_DESC_SCROLL_ROWS] as i32 } else { 0 };
            let pages_wide_raw = item_descriptors[desc_offset + ITEM_DESC_PAGES_WIDE] as i32;
            pages_wide = if pages_wide_raw > 1 { pages_wide_raw } else { 1 };
            if has_page && rows > 0 {
                let page_gap_x = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_PAGE_GAP_X]);
                let exact = advance_fixed(key_to_float(max_row_extents[item_index * 2]))
                    + advance_fixed(page_gap_x);
                fixed_pair(exact, &mut stride_reach, &mut stride_reach_tail);
            }
        }
        let mut x = 0.0f32;
        let mut in_seg = false;
        let mut current_item_index = item_index;
        let mut loc_row_max = 0u32;
        let mut loc_x_max = 0u32;
        let mut id = range_start;
        while id < range_end {
            while has_items && next_item_boundary <= id {
                if loc_row_max > 0 {
                    let slot = current_item_index - cube_item_base;
                    if slot < RESOLVE_SLOTS {
                        shared_row_max[slot].fetch_max(loc_row_max);
                        shared_x_max[slot].fetch_max(loc_x_max);
                    } else {
                        item_row_max[current_item_index].fetch_max(loc_row_max);
                        item_x_max[current_item_index].fetch_max(loc_x_max);
                    }
                    loc_row_max = 0u32;
                    loc_x_max = 0u32;
                }
                item_index += 1;
                current_item_index = item_index;
                let desc_offset = item_index * ITEM_DESC_STRIDE;
                start = item_descriptors[desc_offset + ITEM_DESC_BYTE_START] as usize;
                next_item_boundary = if item_index + 1 < item_count {
                    item_descriptors[(item_index + 1) * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize
                } else {
                    total_bytes
                };
                wrap = item_descriptors[desc_offset + ITEM_DESC_WRAP_WIDTH] as i32;
                fold = fold_of(item_descriptors, item_index, wrap);
                line_height = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_LINE_HEIGHT]);
                origin_x = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_ORIGIN_X]);
                origin_y = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_ORIGIN_Y]);
                origin_z = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_ORIGIN_Z]);
                z_step = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_Z_STEP]);
                z_step_lo = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_Z_STEP_LO]);
                band_stride_y = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_BAND_STRIDE_Y]);
                depth_per_band = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_DEPTH_PER_BAND]);
                depth_per_col = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_DEPTH_PER_COL]);
                let has_page = item_descriptors[desc_offset + ITEM_DESC_HAS_PAGE] != 0;
                rows = if has_page { item_descriptors[desc_offset + ITEM_DESC_PAGE_ROWS] as i32 } else { 0 };
                cols = if has_page { item_descriptors[desc_offset + ITEM_DESC_PAGE_COLS] as i32 } else { 0 };
                scroll = if has_page { item_descriptors[desc_offset + ITEM_DESC_SCROLL_ROWS] as i32 } else { 0 };
                let pages_wide_raw = item_descriptors[desc_offset + ITEM_DESC_PAGES_WIDE] as i32;
                pages_wide = if pages_wide_raw > 1 { pages_wide_raw } else { 1 };
                if has_page && rows > 0 {
                    let page_gap_x = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_PAGE_GAP_X]);
                    let exact = advance_fixed(key_to_float(max_row_extents[item_index * 2]))
                        + advance_fixed(page_gap_x);
                    fixed_pair(exact, &mut stride_reach, &mut stride_reach_tail);
                } else {
                    stride_reach = 0.0f32;
                    stride_reach_tail = 0.0f32;
                }
            }
            let glyph_flags_val = flags_at(glyph_flags, id);
            if (glyph_flags_val & F_LEADER) != 0 {
                let col = line_columns[id * LC_STRIDE + LC_COL] as i32;
                let seg_col = if fold > 0 { col % fold } else { col };
                let is_segment_head = seg_col == 0;
                if !in_seg || is_segment_head {
                    // Entry walk (backward, once per segment entry; free at
                    // a head where seg_col == 0 empties the loop).
                    x = 0.0f32;
                    let item_ordinal = item_record_ordinals[id] as i32;
                    let mut back_col = seg_col;
                    while back_col >= 1 {
                        let prev_byte_idx = ordinal_to_byte_map[start + (item_ordinal - back_col) as usize] as usize;
                        x += advance_widths[prev_byte_idx];
                        back_col -= 1;
                    }
                    in_seg = true;
                }
                let row = line_columns[id * LC_STRIDE + LC_ROW] as i32;
                let wrap_segment = wrap_segment_of(col, wrap, (glyph_flags_val & F_NEWLINE) != 0);
                let metrics_offset = id * LM_STRIDE;
                let base = x + origin_x;

                let mut final_x = base;
                let mut final_y = fma(-(row as f32), line_height, origin_y);
                let depth_steps = -(wrap_segment as f32);
                let z_tail_folded = fma(depth_steps, z_step_lo, origin_z);
                let mut final_z = fma(depth_steps, z_step, z_tail_folded);

                if rows != 0 || cols != 0 || scroll != 0 {
                    let screen_row = row - scroll;
                    let mut y_page = 0;
                    if rows > 0 && screen_row >= rows {
                        y_page = screen_row / rows;
                    }
                    let mut x_page = 0;
                    if cols > 0 {
                        x_page = col / cols;
                    }
                    let band = y_page / pages_wide;
                    let page_col = (y_page % pages_wide) as f32;
                    let x_with_tail = fma(page_col, stride_reach_tail, base);
                    final_x = fma(page_col, stride_reach, x_with_tail);
                    let row_in_page = (screen_row - y_page * rows) as f32;
                    let y_row_folded = fma(-row_in_page, line_height, origin_y);
                    final_y = fma(-(band as f32), band_stride_y, y_row_folded);
                    let z_stepped = fma(depth_steps, z_step, z_tail_folded);
                    let z_banded = fma(band as f32, depth_per_band, z_stepped);
                    final_z = fma(x_page as f32, depth_per_col, z_banded);
                }

                layout_metrics[metrics_offset + LM_X] = final_x;
                layout_metrics[metrics_offset + LM_Y] = final_y;
                layout_metrics[metrics_offset + LM_Z] = final_z;

                let r = (row + 1) as u32;
                if r > loc_row_max {
                    loc_row_max = r;
                }
                let k = ordered_key(x);
                if k > loc_x_max {
                    loc_x_max = k;
                }
                if (glyph_flags_val & F_NEWLINE) == 0 {
                    // This leader's advance feeds the next x — the same add
                    // the backward re-sum performed, one step forward.
                    x += advance_widths[id];
                }
            }
            id += 1;
        }
        if loc_row_max > 0 {
            let slot = current_item_index - cube_item_base;
            if slot < RESOLVE_SLOTS {
                shared_row_max[slot].fetch_max(loc_row_max);
                shared_x_max[slot].fetch_max(loc_x_max);
            } else {
                item_row_max[current_item_index].fetch_max(loc_row_max);
                item_x_max[current_item_index].fetch_max(loc_x_max);
            }
        }
    }
    sync_cube();
    if unit_idx < RESOLVE_SLOTS {
        let item_idx = cube_item_base + unit_idx;
        if item_idx < item_count {
            item_row_max[item_idx].fetch_max(shared_row_max[unit_idx].load());
            item_x_max[item_idx].fetch_max(shared_x_max[unit_idx].load());
        }
    }
}

/// Fused scan chase, spatial positioning, extent reduction, and direct slot/tint emission:
/// Unifies `apply` with `resolve_x_fused`, completely eliminating intermediate VRAM roundtrips
/// for `line_columns`, `item_record_ordinals`, and `ordinal_to_byte_map` (~1.55 GB on flagship corpus).
/// Preserves bit-exact layout and pagination parity with left-to-right advance accumulation.
#[cube(launch_unchecked)]
pub(super) fn apply_and_emit(
    glyph_flags: &[u32],
    advance_widths: &[f32],
    item_descriptors: &[u32],
    tile_spine_counts: &[u32],
    tile_spine_metrics: &[f32],
    max_row_extents: &[u32],
    glyph_indices: &[u32],
    item_extents: &mut [Atomic<u32>],
    per_record_semantic_colors: &[u32],
    segment_entry_advances: &[f32],
    instance_slots: &mut [u32],
    instance_tints: &mut [u32],
    #[comptime] emit_derived: bool,
    #[comptime] units: usize,
    #[comptime] rake: usize,
    #[comptime] log: usize,
) {
    let tile_idx = CUBE_POS;
    let unit_idx = UNIT_POS as usize;
    let total_bytes = glyph_flags.len() * 4; // packed: words -> bytes
    let item_count = item_descriptors.len() / ITEM_DESC_STRIDE;
    let range_start = tile_idx * (units * rake) + unit_idx * rake;
    let range_end = if range_start + rake < total_bytes { range_start + rake } else { total_bytes };

    let shared_item_extents = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS * EXT_STRIDE);
    let shared_item_flags = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS);
    let mut shared_item_base = Shared::<u32>::new();
    let cube_lo = tile_idx * (units * rake);
    if unit_idx == 0 {
        let probe = if total_bytes > 0 {
            if cube_lo < total_bytes { cube_lo } else { total_bytes - 1 }
        } else {
            0usize
        };
        let mut probe_item = 0usize;
        if item_count > 0 {
            probe_item = item_search_desc(item_descriptors, item_count, probe);
        }
        *shared_item_base = probe_item as u32;
    }

    let ordered_key_zero = 0x8000_0000u32;
    let ordered_key_infinity = 0xFF80_0000u32;
    let ordered_key_neg_infinity = 0x007F_FFFFu32;

    let mut slot_index = unit_idx;
    while slot_index < RESOLVE_SLOTS {
        shared_item_flags[slot_index].store(0u32);
        let extent_offset = slot_index * EXT_STRIDE;
        shared_item_extents[extent_offset].store(ordered_key_zero);
        shared_item_extents[extent_offset + 1].store(ordered_key_zero);
        shared_item_extents[extent_offset + 2].store(ordered_key_zero);
        shared_item_extents[extent_offset + 3].store(ordered_key_zero);
        shared_item_extents[extent_offset + 4].store(ordered_key_infinity);
        shared_item_extents[extent_offset + 5].store(ordered_key_infinity);
        shared_item_extents[extent_offset + 6].store(ordered_key_neg_infinity);
        shared_item_extents[extent_offset + 7].store(ordered_key_neg_infinity);
        shared_item_extents[extent_offset + 8].store(ordered_key_infinity);
        shared_item_extents[extent_offset + 9].store(ordered_key_neg_infinity);
        slot_index += units;
    }

    let total_tile_bytes = units * rake;
    let tile_byte_start = tile_idx * total_tile_bytes;
    let tile_word_start = tile_byte_start >> 2;
    let total_tile_words = total_tile_bytes.div_ceil(4);

    let mut shared_tile_flags = Shared::<[u32]>::new_slice((units * rake) / 4);

    let mut preload_word_idx = unit_idx;
    while preload_word_idx < total_tile_words {
        let global_word_idx = tile_word_start + preload_word_idx;
        shared_tile_flags[preload_word_idx] = if global_word_idx < glyph_flags.len() {
            glyph_flags[global_word_idx]
        } else {
            0u32
        };
        preload_word_idx += units;
    }
    sync_cube();

    // Phase 1: Serial rake of this unit's bytes into one monoid accumulator
    let mut accumulator = identity();
    let mut item_index = 0usize;
    let mut start = 0usize;
    let mut next_item_boundary = total_bytes;
    let mut active_wrap_width = 0i32;
    let mut active_wrap_mode = 0i32;
    let mut active_cell_advance_bits = 0u32;
    let has_items = item_count > 0;
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
            }
            let reset = if has_items && id == start { 1i32 } else { 0i32 };
            let leaf = leaf_of(glyph_flags, advance_widths, active_wrap_width, active_wrap_mode, reset, active_cell_advance_bits, id);
            combine(&mut accumulator, &leaf);
            id += 1;
        }
    } else {
        accumulator.wrap = active_wrap_width;
        accumulator.mode = active_wrap_mode;
    }

    // Phase 2: Cube Blelloch scan across units in shared memory
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
    // Down-sweep — NON-COMMUTATIVE form, derived and unit-tested by hand on
    // n=8: t = x[unit_idx]; x[unit_idx] = combine(x[unit_idx], x[unit_idx-s]); x[unit_idx-s] = t. The carried
    // prefix is the LEFT operand and the left child's TOTAL the right; the
    // commutative textbook form (combine(x[unit_idx-s], x[unit_idx])) silently scrambles
    // reset/head/tail lanes.
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

    // Phase 3: The chase — combines global tile prefix + exclusive micro prefix
    let mut run = p_load(tile_spine_counts, tile_spine_metrics, tile_idx);
    let micro = s_load(&shared_counts, &shared_metrics, unit_idx);
    combine(&mut run, &micro);

    // Repurpose shared_counts (2304 slots, dead after micro load) to cache the tile's 2048 advance floats
    sync_cube();
    let mut preload_byte_idx = unit_idx;
    while preload_byte_idx < total_tile_bytes {
        let global_byte_idx = tile_byte_start + preload_byte_idx;
        let adv_val = if global_byte_idx < total_bytes {
            advance_widths[global_byte_idx]
        } else {
            0.0f32
        };
        shared_counts[preload_byte_idx] = adv_val.to_bits() as i32;
        preload_byte_idx += units;
    }
    sync_cube();

    let cube_item_base = *shared_item_base as usize;

    let mut survivor_ordinal = run.survivors as u32;

    let mut page_right_max = f32::new(0.0f32);
    let mut page_y_min = f32::new(0.0f32);
    let mut page_z_min = f32::new(0.0f32);
    let mut page_z_max = f32::new(0.0f32);
    let mut ink_x_min = f32::new(3.4028235e38f32);
    let mut ink_y_min = f32::new(3.4028235e38f32);
    let mut ink_right_max = f32::new(-3.4028235e38f32);
    let mut ink_y_max = f32::new(-3.4028235e38f32);
    let mut ink_z_min = f32::new(3.4028235e38f32);
    let mut ink_z_max = f32::new(-3.4028235e38f32);
    let mut any_leader = false;
    let mut any_survivor = false;

    item_index = 0usize;
    start = 0usize;
    next_item_boundary = total_bytes;
    active_wrap_width = 0i32;
    active_wrap_mode = 0i32;
    active_cell_advance_bits = 0u32;
    let mut fold_unit = 0i32;
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
        fold_unit = fold_of(item_descriptors, item_index, active_wrap_width);
    }
    let mut current_advance_x = 0.0f32;
    let mut in_segment = false;
    let mut current_item_index = item_index;

    if range_start < total_bytes {
        let mut id = range_start;
        while id < range_end {
            while has_items && next_item_boundary <= id {
                if any_leader {
                    let slot = current_item_index - cube_item_base;
                    if slot < RESOLVE_SLOTS {
                        shared_item_flags[slot].fetch_or(if any_survivor { 3u32 } else { 1u32 });
                        let extent_offset = slot * EXT_STRIDE;
                        shared_item_extents[extent_offset].fetch_max(ordered_key(page_right_max));
                        shared_item_extents[extent_offset + 1].fetch_min(ordered_key(page_y_min));
                        shared_item_extents[extent_offset + 2].fetch_min(ordered_key(page_z_min));
                        shared_item_extents[extent_offset + 3].fetch_max(ordered_key(page_z_max));
                        if any_survivor {
                            shared_item_extents[extent_offset + 4].fetch_min(ordered_key(ink_x_min));
                            shared_item_extents[extent_offset + 5].fetch_min(ordered_key(ink_y_min));
                            shared_item_extents[extent_offset + 6].fetch_max(ordered_key(ink_right_max));
                            shared_item_extents[extent_offset + 7].fetch_max(ordered_key(ink_y_max));
                            shared_item_extents[extent_offset + 8].fetch_min(ordered_key(ink_z_min));
                            shared_item_extents[extent_offset + 9].fetch_max(ordered_key(ink_z_max));
                        }
                    } else {
                        let extent_offset = current_item_index * EXT_STRIDE;
                        item_extents[extent_offset].fetch_max(ordered_key(page_right_max));
                        item_extents[extent_offset + 1].fetch_min(ordered_key(page_y_min));
                        item_extents[extent_offset + 2].fetch_min(ordered_key(page_z_min));
                        item_extents[extent_offset + 3].fetch_max(ordered_key(page_z_max));
                        if any_survivor {
                            item_extents[extent_offset + 4].fetch_min(ordered_key(ink_x_min));
                            item_extents[extent_offset + 5].fetch_min(ordered_key(ink_y_min));
                            item_extents[extent_offset + 6].fetch_max(ordered_key(ink_right_max));
                            item_extents[extent_offset + 7].fetch_max(ordered_key(ink_y_max));
                            item_extents[extent_offset + 8].fetch_min(ordered_key(ink_z_min));
                            item_extents[extent_offset + 9].fetch_max(ordered_key(ink_z_max));
                        }
                    }
                    page_right_max = f32::new(0.0f32);
                    page_y_min = f32::new(0.0f32);
                    page_z_min = f32::new(0.0f32);
                    page_z_max = f32::new(0.0f32);
                    ink_x_min = f32::new(3.4028235e38f32);
                    ink_y_min = f32::new(3.4028235e38f32);
                    ink_right_max = f32::new(-3.4028235e38f32);
                    ink_y_max = f32::new(-3.4028235e38f32);
                    ink_z_min = f32::new(3.4028235e38f32);
                    ink_z_max = f32::new(-3.4028235e38f32);
                    any_leader = false;
                    any_survivor = false;
                }
                item_index += 1;
                current_item_index = item_index;
                start = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize;
                next_item_boundary = if item_index + 1 < item_count {
                    item_descriptors[(item_index + 1) * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize
                } else {
                    total_bytes
                };
                active_wrap_width = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_WRAP_WIDTH] as i32;
                active_wrap_mode = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_WRAP_MODE] as i32;
                active_cell_advance_bits = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_CELL_ADVANCE];
                fold_unit = fold_of(item_descriptors, item_index, active_wrap_width);
                in_segment = false;
            }
            let reset = has_items && id == start;
            if reset {
                run.reset = 0;
                run.nl = 0;
                run.glyphs = 0;
                run.rows = 0;
                run.head_len = 0;
                run.tail_len = 0;
                run.tail_adv = 0.0;
                run.clean_len = 0;
                run.clean_break = 1;
                run.wrap = active_wrap_width;
                run.mode = active_wrap_mode;
                in_segment = false;
            }
            let local_id = id - tile_byte_start;
            let glyph_flags_val = (shared_tile_flags[local_id >> 2] >> (((local_id & 3) * 8) as u32)) & 0xFF;
            if (glyph_flags_val & F_LEADER) != 0 {
                let col = run.tail_len;
                let mut closed = 0i32;
                if run.nl > 0 {
                    closed = rows_for(run.head_len, active_wrap_width, active_wrap_mode) + run.rows;
                }
                let wr = wrap_row_of(col, active_wrap_width, (glyph_flags_val & F_NEWLINE) != 0, active_wrap_mode);
                let row = closed + wr;

                let segment_column = if fold_unit > 0 { col % fold_unit } else { col };
                let is_segment_head = segment_column == 0;
                if !in_segment || is_segment_head {
                    current_advance_x = 0.0f32;
                    // The segment's `segment_column` leaders behind this one are
                    // exactly the `segment_column` leaders immediately before it
                    // (a newline or item reset would have zeroed col). When the
                    // clean run covers them all, each is one cell and the walk's
                    // left-to-right re-sum is the table entry, bit for bit.
                    let entry_is_clean = fold_unit > 0
                        && segment_column > 0
                        && run.clean_len >= segment_column
                        && (segment_column as usize) < segment_entry_advances.len();
                    if entry_is_clean {
                        current_advance_x = segment_entry_advances[segment_column as usize];
                    } else if fold_unit > 0 && segment_column > 0 {
                        let mut backward_column = segment_column;
                        let mut start_byte_index = id as i32 - 1;
                        let tile_byte_start_i32 = tile_byte_start as i32;

                        // Fast branch-free walk in on-chip SRAM while inside the current tile
                        while backward_column >= 1 && start_byte_index >= tile_byte_start_i32 {
                            let local_idx = start_byte_index as usize - tile_byte_start;
                            let flag = (shared_tile_flags[local_idx >> 2] >> (((local_idx & 3) * 8) as u32)) & 0xFF;
                            if (flag & F_LEADER) != 0 {
                                backward_column -= 1;
                            }
                            if backward_column >= 1 {
                                start_byte_index -= 1;
                            }
                        }

                        // Rare fallback: only if the segment crossed before the tile boundary
                        while backward_column >= 1 && start_byte_index >= 0 {
                            if (flags_at(glyph_flags, start_byte_index as usize) & F_LEADER) != 0 {
                                backward_column -= 1;
                            }
                            if backward_column >= 1 {
                                start_byte_index -= 1;
                            }
                        }

                        // Fast forward accumulation
                        if start_byte_index >= tile_byte_start_i32 {
                            let local_start = start_byte_index as usize - tile_byte_start;
                            let local_end = id - tile_byte_start;
                            let mut local_idx = local_start;
                            while local_idx < local_end {
                                let flag = (shared_tile_flags[local_idx >> 2] >> (((local_idx & 3) * 8) as u32)) & 0xFF;
                                if (flag & F_LEADER) != 0 {
                                    current_advance_x += f32::from_bits(shared_counts[local_idx] as u32);
                                }
                                local_idx += 1;
                            }
                        } else {
                            let mut forward_index = start_byte_index as usize;
                            while forward_index < id {
                                if (flags_at(glyph_flags, forward_index) & F_LEADER) != 0 {
                                    current_advance_x += advance_widths[forward_index];
                                }
                                forward_index += 1;
                            }
                        }
                    } else if fold_unit == 0 {
                        current_advance_x = run.tail_adv;
                    }
                    in_segment = true;
                }

                let descriptor_offset = item_index * ITEM_DESC_STRIDE;
                let wrap_segment = wrap_segment_of(col, active_wrap_width, (glyph_flags_val & F_NEWLINE) != 0);
                let line_height = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_LINE_HEIGHT]);
                let origin_x = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_ORIGIN_X]);
                let origin_y = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_ORIGIN_Y]);
                let origin_z = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_ORIGIN_Z]);
                let z_step = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_Z_STEP]);
                let z_step_lo = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_Z_STEP_LO]);
                let band_stride_y = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_BAND_STRIDE_Y]);
                let depth_per_band = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_DEPTH_PER_BAND]);
                let depth_per_col = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_DEPTH_PER_COL]);
                let base_x = current_advance_x + origin_x;

                let mut final_x = base_x;
                let mut final_y = fma(-(row as f32), line_height, origin_y);
                let depth_steps = -(wrap_segment as f32);
                let z_tail_folded = fma(
                    depth_steps,
                    z_step_lo,
                    origin_z,
                );
                let mut final_z = fma(depth_steps, z_step, z_tail_folded);

                let has_page = item_descriptors[descriptor_offset + ITEM_DESC_HAS_PAGE] != 0;
                let page_rows = if has_page { item_descriptors[descriptor_offset + ITEM_DESC_PAGE_ROWS] as i32 } else { 0 };
                let page_cols = if has_page { item_descriptors[descriptor_offset + ITEM_DESC_PAGE_COLS] as i32 } else { 0 };
                let scroll_rows = if has_page { item_descriptors[descriptor_offset + ITEM_DESC_SCROLL_ROWS] as i32 } else { 0 };
                if page_rows != 0 || page_cols != 0 || scroll_rows != 0 {
                    let screen_row = row - scroll_rows;
                    let mut y_page = 0;
                    if page_rows > 0 && screen_row >= page_rows {
                        y_page = screen_row / page_rows;
                    }
                    let mut x_page = 0;
                    if page_cols > 0 {
                        x_page = col / page_cols;
                    }
                    let pages_wide_raw = item_descriptors[descriptor_offset + ITEM_DESC_PAGES_WIDE] as i32;
                    let pages_wide = if pages_wide_raw > 1 { pages_wide_raw } else { 1 };
                    let band = y_page / pages_wide;
                    let page_col = (y_page % pages_wide) as f32;
                    let mut stride_reach = 0.0f32;
                    let mut stride_reach_tail = 0.0f32;
                    if has_page && page_rows > 0 {
                        let page_gap_x = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_PAGE_GAP_X]);
                        let exact = advance_fixed(key_to_float(max_row_extents[item_index * 2]))
                            + advance_fixed(page_gap_x);
                        fixed_pair(exact, &mut stride_reach, &mut stride_reach_tail);
                    }
                    let x_with_tail = fma(page_col, stride_reach_tail, base_x);
                    final_x = fma(page_col, stride_reach, x_with_tail);
                    let row_in_page = (screen_row - y_page * page_rows) as f32;
                    let y_row_folded = fma(-row_in_page, line_height, origin_y);
                    final_y = fma(-(band as f32), band_stride_y, y_row_folded);
                    let z_stepped = fma(depth_steps, z_step, z_tail_folded);
                    let z_banded = fma(band as f32, depth_per_band, z_stepped);
                    final_z = fma(x_page as f32, depth_per_col, z_banded);
                }

                any_leader = true;
                let glyph_advance = f32::from_bits(shared_counts[local_id] as u32);
                let right = final_x + glyph_advance;
                if right > page_right_max {
                    page_right_max = right;
                }
                if final_y < page_y_min {
                    page_y_min = final_y;
                }
                if final_z < page_z_min {
                    page_z_min = final_z;
                }
                if final_z > page_z_max {
                    page_z_max = final_z;
                }
                if is_survivor(glyph_flags_val) {
                    any_survivor = true;
                    let half = 0.5f32;
                    let y_lo = final_y - half;
                    let y_hi = final_y + half;
                    if final_x < ink_x_min {
                        ink_x_min = final_x;
                    }
                    if y_lo < ink_y_min {
                        ink_y_min = y_lo;
                    }
                    if right > ink_right_max {
                        ink_right_max = right;
                    }
                    if y_hi > ink_y_max {
                        ink_y_max = y_hi;
                    }
                    if final_z < ink_z_min {
                        ink_z_min = final_z;
                    }
                    if final_z > ink_z_max {
                        ink_z_max = final_z;
                    }

                    // Direct instance emission: emit 8-word slot and 2-word tint
                    let item_color_base = item_descriptors[descriptor_offset + ITEM_DESC_COLOR_BASE];
                    let item_is_per_record = item_descriptors[descriptor_offset + ITEM_DESC_IS_PER_RECORD];
                    let item_flat_color = item_descriptors[descriptor_offset + ITEM_DESC_FLAT_COLOR];
                    let item_group_id = item_descriptors[descriptor_offset + ITEM_DESC_GROUP];
                    let color = if item_is_per_record != 0u32 {
                        let record_ordinal = run.glyphs as u32;
                        per_record_semantic_colors[(item_color_base + record_ordinal) as usize]
                    } else {
                        item_flat_color
                    };
                    if emit_derived {
                        let slot_word_offset = survivor_ordinal as usize * 5;
                        if slot_word_offset + 5 <= instance_slots.len() {
                            let glyph_and_wrap = glyph_indices[id] | ((wrap_segment as u32) << 16u32);
                            instance_slots[slot_word_offset] = final_x.to_bits();
                            instance_slots[slot_word_offset + 1] = row as u32;
                            instance_slots[slot_word_offset + 2] = glyph_and_wrap;
                            instance_slots[slot_word_offset + 3] = color;
                            instance_slots[slot_word_offset + 4] = (item_index as u32 & 0xFFFFu32) | ((item_group_id & 0xFFFFu32) << 16u32);
                        }
                    } else {
                        let slot_word_offset = survivor_ordinal as usize * 8;
                        if slot_word_offset + 8 <= instance_slots.len() {
                            instance_slots[slot_word_offset] = final_x.to_bits();
                            instance_slots[slot_word_offset + 1] = final_y.to_bits();
                            instance_slots[slot_word_offset + 2] = final_z.to_bits();
                            instance_slots[slot_word_offset + 3] = glyph_indices[id];
                            instance_slots[slot_word_offset + 4] = color;
                            instance_slots[slot_word_offset + 5] = item_group_id;
                            instance_slots[slot_word_offset + 6] = glyph_advance.to_bits();
                            instance_slots[slot_word_offset + 7] = 0x3f800000u32; // 1.0f32.to_bits()
                        }
                    }
                    let tint_word_offset = survivor_ordinal as usize * 2;
                    if tint_word_offset + 2 <= instance_tints.len() {
                        instance_tints[tint_word_offset] = glyph_indices[id];
                        instance_tints[tint_word_offset + 1] = color;
                    }
                    survivor_ordinal += 1u32;
                }

                if (glyph_flags_val & F_NEWLINE) == 0 {
                    current_advance_x += glyph_advance;
                }
            }
            let leaf = leaf_of(glyph_flags, advance_widths, active_wrap_width, active_wrap_mode, if reset { 1i32 } else { 0i32 }, active_cell_advance_bits, id);
            combine(&mut run, &leaf);
            id += 1;
        }

        if any_leader {
            let slot = current_item_index - cube_item_base;
            if slot < RESOLVE_SLOTS {
                shared_item_flags[slot].fetch_or(if any_survivor { 3u32 } else { 1u32 });
                let extent_offset = slot * EXT_STRIDE;
                shared_item_extents[extent_offset].fetch_max(ordered_key(page_right_max));
                shared_item_extents[extent_offset + 1].fetch_min(ordered_key(page_y_min));
                shared_item_extents[extent_offset + 2].fetch_min(ordered_key(page_z_min));
                shared_item_extents[extent_offset + 3].fetch_max(ordered_key(page_z_max));
                if any_survivor {
                    shared_item_extents[extent_offset + 4].fetch_min(ordered_key(ink_x_min));
                    shared_item_extents[extent_offset + 5].fetch_min(ordered_key(ink_y_min));
                    shared_item_extents[extent_offset + 6].fetch_max(ordered_key(ink_right_max));
                    shared_item_extents[extent_offset + 7].fetch_max(ordered_key(ink_y_max));
                    shared_item_extents[extent_offset + 8].fetch_min(ordered_key(ink_z_min));
                    shared_item_extents[extent_offset + 9].fetch_max(ordered_key(ink_z_max));
                }
            } else {
                let extent_offset = current_item_index * EXT_STRIDE;
                item_extents[extent_offset].fetch_max(ordered_key(page_right_max));
                item_extents[extent_offset + 1].fetch_min(ordered_key(page_y_min));
                item_extents[extent_offset + 2].fetch_min(ordered_key(page_z_min));
                item_extents[extent_offset + 3].fetch_max(ordered_key(page_z_max));
                if any_survivor {
                    item_extents[extent_offset + 4].fetch_min(ordered_key(ink_x_min));
                    item_extents[extent_offset + 5].fetch_min(ordered_key(ink_y_min));
                    item_extents[extent_offset + 6].fetch_max(ordered_key(ink_right_max));
                    item_extents[extent_offset + 7].fetch_max(ordered_key(ink_y_max));
                    item_extents[extent_offset + 8].fetch_min(ordered_key(ink_z_min));
                    item_extents[extent_offset + 9].fetch_max(ordered_key(ink_z_max));
                }
            }
        }
    }
    sync_cube();
    if unit_idx < RESOLVE_SLOTS {
        let item_index = cube_item_base + unit_idx;
        if item_index < item_count {
            let flags = shared_item_flags[unit_idx].load();
            if (flags & 1u32) != 0 {
                let shared_extent_offset = unit_idx * EXT_STRIDE;
                let extent_offset = item_index * EXT_STRIDE;
                item_extents[extent_offset].fetch_max(shared_item_extents[shared_extent_offset].load());
                item_extents[extent_offset + 1].fetch_min(shared_item_extents[shared_extent_offset + 1].load());
                item_extents[extent_offset + 2].fetch_min(shared_item_extents[shared_extent_offset + 2].load());
                item_extents[extent_offset + 3].fetch_max(shared_item_extents[shared_extent_offset + 3].load());
                if (flags & 2u32) != 0 {
                    item_extents[extent_offset + 4].fetch_min(shared_item_extents[shared_extent_offset + 4].load());
                    item_extents[extent_offset + 5].fetch_min(shared_item_extents[shared_extent_offset + 5].load());
                    item_extents[extent_offset + 6].fetch_max(shared_item_extents[shared_extent_offset + 6].load());
                    item_extents[extent_offset + 7].fetch_max(shared_item_extents[shared_extent_offset + 7].load());
                    item_extents[extent_offset + 8].fetch_min(shared_item_extents[shared_extent_offset + 8].load());
                    item_extents[extent_offset + 9].fetch_max(shared_item_extents[shared_extent_offset + 9].load());
                }
            }
        }
    }
}

// ── the extent walk ─────────────────────────────────────────────────────
// The page stride's input: the WIDEST wrap segment's exact advance sum.
// The engine reads the unrounded f64 prefix (fold.rs scalars[7]); the scan
// tree's keyed value is per-step-rounded and 1-2 ulp short — the census's
// X-at-m2 class (deviations at exact doublings, which only a wrong stride
// value produces). A glyph's stored x is the segment sum BEFORE its own
// advance; the segment resets when the incremented column fills it, and
// the filling glyph's advance never joins the segment it closes
// (fold.rs:758); terminators reset both.
//
// THE SEGMENT-PARALLEL FORM (2026-09-30, repriced by the chain profiler):
// the one-thread-per-item serial walk was 54% of the chain's GPU time at
// the flagship (827ms — the wall is the LARGEST item's latency chain, not
// bandwidth). The bit-exactness constraint binds only WITHIN a segment
// (the engine's f32 serial segment sums), and segments are bounded by the
// wrap width, so: one thread per SEGMENT START, the serial add order
// preserved byte-for-byte inside the walk, and the cross-segment max via
// an order-free ordered-key atomic (the seeded lanes pattern — the buffer
// arrives pre-seeded with ordered_key(0.0), which is also the right value
// for an item with no leaders). Segment starts read off lc's col — the
// fold's own fenced lane: col == 0 (line start) or col % width == 0 (the
// fill boundary); the serial kernel rediscovered the same boundaries by
// counting. 827ms -> ~15ms at the flagship shape.
//
// The segment width arrives pre-resolved in the walk plan (host-packed
// [start, stop, width] per item — fold.rs's rule: wrap if wrapped, else
// page columns if paged, else 0 = whole-line sums). It does NOT ride the
// ie buffer: a six-slice kernel shape misbinds that read (landmine 7's
// cousin, 2026-09-28 — the width came back zeroed, the reset never
// fired). Four input slices, one mutable output.
#[cube(launch_unchecked)]
pub(super) fn extent_pair(
    advance_widths: &[f32],
    glyph_flags: &[u32],
    line_columns: &[u32],
    walk_plan: &[u32],
    max_row_extents: &mut [Atomic<u32>],
    min_sw: u32,
    uniform_sw: u32,
    #[comptime] units: usize,
    #[comptime] rake: usize,
) {
    let tile = CUBE_POS;
    let u = UNIT_POS as usize;
    let n = line_columns.len() / LC_STRIDE;
    let tile_start = tile * (units * rake);
    let item_count = walk_plan.len() / 3;

    let shared_max_extents = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS);
    let mut shared_item_base = Shared::<u32>::new();

    if u == 0 {
        let mut probe_item = 0usize;
        if item_count > 0 && n > 0 {
            let probe = if tile_start < n { tile_start } else { n - 1 };
            let mut search_lo = 0usize;
            let mut search_hi = item_count;
            while search_lo + 1 < search_hi {
                let mid = (search_lo + search_hi) / 2;
                if (walk_plan[mid * 3] as usize) <= probe {
                    search_lo = mid;
                } else {
                    search_hi = mid;
                }
            }
            probe_item = search_lo;
        }
        *shared_item_base = probe_item as u32;
    }

    let zero_k = 0x8000_0000u32;
    if u < RESOLVE_SLOTS {
        shared_max_extents[u].store(zero_k);
    }
    sync_cube();
    let tile_item_base = *shared_item_base as usize;
    let nxt_start = if tile_item_base + 1 < item_count {
        walk_plan[(tile_item_base + 1) * 3] as usize
    } else {
        n
    };

    let mut k = 0usize;
    while k < rake {
        let byte_index = tile_start + k * units + u;
        if byte_index < n && (flags_at(glyph_flags, byte_index) & F_LEADER) != 0 {
            let col = line_columns[byte_index * LC_STRIDE + LC_COL] as usize;
            let candidate = col == 0
                || (col >= (min_sw as usize)
                    && (uniform_sw == 0 || col.is_multiple_of(uniform_sw as usize)));
            if candidate {
                let mut matched_item = tile_item_base;
                if byte_index >= nxt_start {
                    let mut search_lo = tile_item_base;
                    let mut search_hi = item_count;
                    while search_lo + 1 < search_hi {
                        let mid = (search_lo + search_hi) / 2;
                        if (walk_plan[mid * 3] as usize) <= byte_index {
                            search_lo = mid;
                        } else {
                            search_hi = mid;
                        }
                    }
                    matched_item = search_lo;
                }
                let sw = walk_plan[matched_item * 3 + 2] as usize;
                if col == 0 || (sw != 0 && col.is_multiple_of(sw)) {
                    let stop = walk_plan[matched_item * 3 + 1] as usize;
                    let mut sum = 0.0f32;
                    let mut widest = 0.0f32;
                    let mut count = 0usize;
                    let mut id = byte_index;
                    let mut cur_word_idx = id >> 2;
                    let mut fl_word = glyph_flags[cur_word_idx];
                    while id < stop {
                        let word_idx = id >> 2;
                        if word_idx != cur_word_idx {
                            fl_word = glyph_flags[word_idx];
                            cur_word_idx = word_idx;
                        }
                        let f = (fl_word >> (((id & 3) * 8) as u32)) & 0xFF;
                        if (f & F_LEADER) != 0 {
                            // The compare set is the running sum BEFORE this
                            // glyph's own advance — the stored-x rule.
                            if sum > widest {
                                widest = sum;
                            }
                            if (f & F_NEWLINE) != 0 {
                                break;
                            }
                            count += 1;
                            if sw != 0 && count >= sw {
                                // The fill closes the segment; its advance never
                                // joins (fold.rs:758).
                                break;
                            }
                            sum += advance_widths[id];
                        }
                        id += 1;
                    }
                    if widest > 0.0f32 {
                        let key = ordered_key(widest);
                        let slot = matched_item - tile_item_base;
                        if slot < RESOLVE_SLOTS {
                            shared_max_extents[slot].fetch_max(key);
                        } else {
                            max_row_extents[matched_item * 2].fetch_max(key);
                        }
                    }
                }
            }
        }
        k += 1;
    }
    sync_cube();
    if u < RESOLVE_SLOTS {
        let item_idx = tile_item_base + u;
        if item_idx < item_count {
            let val = shared_max_extents[u].load();
            if val > zero_k {
                max_row_extents[item_idx * 2].fetch_max(val);
            }
        }
    }
}

#[allow(dead_code)]
#[cube(launch_unchecked)]
pub(super) fn derive_stride(
    max_row_extents: &[u32],
    item_descriptors: &[u32],
    strides: &mut [f32],
) {
    let item_idx = ABSOLUTE_POS;
    let item_count = item_descriptors.len() / ITEM_DESC_STRIDE;
    if item_idx < item_count {
        let desc_offset = item_idx * ITEM_DESC_STRIDE;
        let has_page = item_descriptors[desc_offset + ITEM_DESC_HAS_PAGE] != 0;
        let rows = item_descriptors[desc_offset + ITEM_DESC_PAGE_ROWS] as i32;
        if has_page && rows > 0 {
            // The gap folds EXACTLY in fixed-point — the engine holds
            // extent+gap as an f64 (an f32 value plus a small constant,
            // exact in f64), and the f32 add would round it whenever the
            // sum crosses a binade. The decode splits the exact value
            // into (fl(stride), sub-ulp tail) for paginate's fmas.
            let exact = advance_fixed(key_to_float(max_row_extents[item_idx * 2]))
                + advance_fixed(f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_PAGE_GAP_X]));
            let mut stride_sum = 0.0f32;
            let mut stride_tail = 0.0f32;
            fixed_pair(exact, &mut stride_sum, &mut stride_tail);
            strides[item_idx * 2] = stride_sum;
            strides[item_idx * 2 + 1] = stride_tail;
        } else {
            strides[item_idx * 2] = 0.0;
            strides[item_idx * 2 + 1] = 0.0;
        }
    }
}

// ── legacy dispatch: paginate (now fused directly into apply_and_emit) ────────
#[allow(dead_code)]
#[cube(launch_unchecked)]
pub(super) fn paginate(
    layout_metrics: &mut [f32],
    glyph_flags: &[u32],
    line_columns: &[u32],
    item_descriptors: &[u32],
    strides: &[f32],
) {
    let byte_index = ABSOLUTE_POS;
    let total_bytes = glyph_flags.len() * 4; // packed: words -> bytes
    let item_count = item_descriptors.len() / ITEM_DESC_STRIDE;
    if byte_index < total_bytes && (flags_at(glyph_flags, byte_index) & F_LEADER) != 0 && item_count > 0 {
        let item_idx = item_search_desc(item_descriptors, item_count, byte_index);
        let desc_offset = item_idx * ITEM_DESC_STRIDE;
        let has_page = item_descriptors[desc_offset + ITEM_DESC_HAS_PAGE] != 0;
        let rows = if has_page { item_descriptors[desc_offset + ITEM_DESC_PAGE_ROWS] as i32 } else { 0 };
        let cols = if has_page { item_descriptors[desc_offset + ITEM_DESC_PAGE_COLS] as i32 } else { 0 };
        let scroll = if has_page { item_descriptors[desc_offset + ITEM_DESC_SCROLL_ROWS] as i32 } else { 0 };
        if rows != 0 || cols != 0 || scroll != 0 {
            let row = line_columns[byte_index * LC_STRIDE + LC_ROW] as i32;
            let col = line_columns[byte_index * LC_STRIDE + LC_COL] as i32;
            let screen_row = row - scroll;
            let mut y_page = 0;
            if rows > 0 && screen_row >= rows {
                y_page = screen_row / rows;
            }
            let mut x_page = 0;
            if cols > 0 {
                x_page = col / cols;
            }
            let pages_wide_raw = item_descriptors[desc_offset + ITEM_DESC_PAGES_WIDE] as i32;
            let pages_wide = if pages_wide_raw > 1 { pages_wide_raw } else { 1 };
            let band = y_page / pages_wide;
            let wrap = item_descriptors[desc_offset + ITEM_DESC_WRAP_WIDTH] as i32;
            let wrap_segment = wrap_segment_of(col, wrap, (flags_at(glyph_flags, byte_index) & F_NEWLINE) != 0);
            let line_height = f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_LINE_HEIGHT]);
            let metrics_offset = byte_index * LM_STRIDE;
            // The three position formulas, one nested OPAQUE fma per
            // term, folded TAIL-FIRST — the tiny correction words ride
            // INSIDE the dominant term's single rounding, which is the
            // engine's f64-store discipline reproduced in f32. Why fma
            // and not plain arithmetic: Metal's optimizer collapses
            // correction identities (`s - (s - a)` → a) on large kernels
            // and cubecl contracts loose mul+add into fma anyway
            // (landmines 8-9) — the builtin is the only shape that
            // survives with its rounding structure intact.
            //
            // X = column position + page column × page stride:
            //   which page column of the band this row lands on, times
            //   how far one page column reaches (the stride pair).
            let page_col = (y_page % pages_wide) as f32;
            let stride_reach_tail = strides[item_idx * 2 + 1];
            let stride_reach = strides[item_idx * 2];
            let x_with_tail = fma(page_col, stride_reach_tail, layout_metrics[metrics_offset + LM_X]);
            layout_metrics[metrics_offset + LM_X] = fma(page_col, stride_reach, x_with_tail);
            // Y = page top − row-in-page × line height − band × band stride.
            let row_in_page = (screen_row - y_page * rows) as f32;
            let y_row_folded = fma(-row_in_page, line_height, f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_ORIGIN_Y]));
            layout_metrics[metrics_offset + LM_Y] = fma(-(band as f32), f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_BAND_STRIDE_Y]), y_row_folded);
            // Z = depth origin − wrap segment × depth step
            //       + band × band depth + page column × column depth.
            // The last two terms are zero in repo mode; the depth step
            // carries an f64 tail lane (ITEM_DESC_Z_STEP_LO) because the engine
            // multiplies the full f64 param.
            let depth_steps = -(wrap_segment as f32);
            let z_tail_folded = fma(depth_steps, f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_Z_STEP_LO]), f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_ORIGIN_Z]));
            let z_stepped = fma(depth_steps, f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_Z_STEP]), z_tail_folded);
            let z_banded = fma(band as f32, f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_DEPTH_PER_BAND]), z_stepped);
            layout_metrics[metrics_offset + LM_Z] = fma(x_page as f32, f32::from_bits(item_descriptors[desc_offset + ITEM_DESC_DEPTH_PER_COL]), z_banded);
        }
    }
}

