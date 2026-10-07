use cubecl::prelude::*;

use super::monoid::{
    advance_fixed, combine, fixed_pair, flags_at, identity, is_survivor, item_search_desc,
    key_to_float, leaf_from_flag, ordered_key, p_load, rows_for, s_load, s_store, wrap_row_of,
    wrap_segment_of,
};
use super::scan::fold_of;
use super::cluster::{cp_at, seq_len_at};
use super::decode::{byte_at, decode_trie};
use super::{
    F_CLUSTER_HEAD, F_CLUSTER_TRAILER, F_LEADER, F_NEWLINE,
    ITEM_DESC_BAND_STRIDE_Y, ITEM_DESC_BYTE_START, ITEM_DESC_BYTE_STOP, ITEM_DESC_CELL_ADVANCE, ITEM_DESC_COLOR_BASE, ITEM_DESC_DEPTH_PER_BAND,
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
    bytes: &[u32],
    trie_block_indices: &[u32],
    trie_block_metrics: &[f32],
    trie_block_codepoints: &[u32],
    #[comptime] trie_block_shift: u32,
    _candidate_head_positions: &[u32],
    _candidate_slots: &[u32],
    _candidate_total: &[u32],
    bitmap_advance: f32,
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
                        
                        let adv = if (flags_at(glyph_flags, prev_byte_idx) & F_CLUSTER_HEAD) != 0 {
                            bitmap_advance
                        } else if (flags_at(glyph_flags, prev_byte_idx) & F_CLUSTER_TRAILER) != 0 {
                            0.0f32
                        } else {
                            let cp_len = seq_len_at(bytes, prev_byte_idx, total_bytes);
                            let cp = cp_at(bytes, prev_byte_idx, cp_len, total_bytes);
                            let (decoded_adv, _) = decode_trie(cp, trie_block_indices, trie_block_metrics, trie_block_codepoints, trie_block_shift);
                            decoded_adv
                        };
                        x += adv;
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
                    
                    let adv = if (glyph_flags_val & F_CLUSTER_HEAD) != 0 {
                        bitmap_advance
                    } else if (glyph_flags_val & F_CLUSTER_TRAILER) != 0 {
                        0.0f32
                    } else {
                        let cp_len = seq_len_at(bytes, id, total_bytes);
                        let cp = cp_at(bytes, id, cp_len, total_bytes);
                        let (decoded_adv, _) = decode_trie(cp, trie_block_indices, trie_block_metrics, trie_block_codepoints, trie_block_shift);
                        decoded_adv
                    };
                    x += adv;
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
#[allow(clippy::manual_range_contains)]
#[cube(launch_unchecked)]
pub(super) fn apply_and_emit(
    glyph_flags: &[u32],
    bytes: &[u32],
    trie_block_indices: &[u32],
    trie_block_metrics: &[f32],
    trie_block_codepoints: &[u32],
    #[comptime] trie_block_shift: u32,
    _candidate_head_positions: &[u32],
    _candidate_slots: &[u32],
    _candidate_total: &[u32],
    bitmap_advance: f32,
    item_descriptors: &[u32],
    tile_item_base: &[u32],
    tile_spine_counts: &[u32],
    tile_spine_metrics: &[f32],
    max_row_extents: &[u32],
    item_extents: &mut [Atomic<u32>],
    per_record_semantic_colors: &[u32],
    segment_entry_advances: &[f32],
    instance_slots: &mut [u32],
    instance_tints: &mut [u32],
    #[comptime] emit_derived: bool,
    #[comptime] track_extents: bool,
    #[comptime] threads_per_cube: usize,
    #[comptime] bytes_per_thread: usize,
    #[comptime] log: usize,
) {
    let tile_idx = CUBE_POS;
    let unit_idx = UNIT_POS as usize;
    let total_bytes = bytes.len() * 4; // packed: words -> bytes
    let item_count = item_descriptors.len() / ITEM_DESC_STRIDE;
    let range_start = tile_idx * (threads_per_cube * bytes_per_thread) + unit_idx * bytes_per_thread;
    let range_end = if range_start + bytes_per_thread < total_bytes { range_start + bytes_per_thread } else { total_bytes };

    let shared_item_extents = Shared::<[Atomic<u32>]>::new_slice(if track_extents { RESOLVE_SLOTS * EXT_STRIDE } else { 1 });
    let shared_item_flags = Shared::<[Atomic<u32>]>::new_slice(if track_extents { RESOLVE_SLOTS } else { 1 });
    let mut shared_item_base = Shared::<u32>::new();
    let cube_lo = tile_idx * (threads_per_cube * bytes_per_thread);
    if track_extents && unit_idx == 0 {
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

    #[allow(clippy::len_zero)]
    let ascii_block_base = if trie_block_indices.len() > 0 {
        trie_block_indices[0] << trie_block_shift
    } else {
        0u32
    };

    let ordered_key_zero = 0x8000_0000u32;
    let ordered_key_infinity = 0xFF80_0000u32;
    let ordered_key_neg_infinity = 0x007F_FFFFu32;

    if track_extents {
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
            slot_index += threads_per_cube;
        }
    }

    let total_tile_bytes = threads_per_cube * bytes_per_thread;
    let tile_byte_start = tile_idx * total_tile_bytes;
    let tile_byte_end = tile_byte_start + total_tile_bytes;

    let mut shared_item_desc = Shared::<[u32]>::new_slice(ITEM_DESC_STRIDE);
    let tile_base_item = if item_count > 0 { tile_item_base[tile_idx] as usize } else { 0usize };
    let has_items = item_count > 0;
    if has_items && unit_idx < ITEM_DESC_STRIDE {
        let base_offset = tile_base_item * ITEM_DESC_STRIDE;
        shared_item_desc[unit_idx] = if base_offset + unit_idx < item_descriptors.len() {
            item_descriptors[base_offset + unit_idx]
        } else {
            0u32
        };
    }

    let mut shared_tile_flags = Shared::<[u32]>::new_slice((threads_per_cube * bytes_per_thread) / 4);

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
    let thread_flags_word0 = if glyph_flags.len() > 1 {
        if thread_word_idx < glyph_flags.len() {
            glyph_flags[thread_word_idx]
        } else {
            0u32
        }
    } else {
        let next_word = thread_bytes_word1;
        crate::cubecl_chain::decode::compute_flags_word(
            thread_bytes_word0,
            next_word,
            thread_word_idx,
            total_bytes,
            trie_block_indices,
            trie_block_codepoints,
            trie_block_shift,
        )
    };
    let thread_flags_word1 = if glyph_flags.len() > 1 {
        if thread_word_idx + 1 < glyph_flags.len() {
            glyph_flags[thread_word_idx + 1]
        } else {
            0u32
        }
    } else {
        let next_word = if thread_word_idx + 2 < bytes.len() {
            bytes[thread_word_idx + 2]
        } else {
            0x8080_8080u32
        };
        crate::cubecl_chain::decode::compute_flags_word(
            thread_bytes_word1,
            next_word,
            thread_word_idx + 1,
            total_bytes,
            trie_block_indices,
            trie_block_codepoints,
            trie_block_shift,
        )
    };
    shared_tile_flags[unit_idx * 2] = thread_flags_word0;
    shared_tile_flags[unit_idx * 2 + 1] = thread_flags_word1;
    sync_cube();

    let tile_item_start = shared_item_desc[ITEM_DESC_BYTE_START] as usize;
    let tile_item_stop = shared_item_desc[ITEM_DESC_BYTE_STOP] as usize;
    let is_uniform_tile = has_items && tile_byte_end <= tile_item_stop && tile_byte_start >= tile_item_start;

    // Phase 1: Serial rake of this unit's bytes into one monoid accumulator
    let mut accumulator = identity();
    let mut seed = range_start;
    if seed >= total_bytes {
        seed = total_bytes - 1;
    }
    let mut item_index = tile_base_item;
    let mut initial_item_index = tile_base_item;
    let mut start = tile_item_start;
    let mut next_item_boundary = tile_item_stop;
    let mut active_wrap_width = shared_item_desc[ITEM_DESC_WRAP_WIDTH] as i32;
    let mut active_wrap_mode = shared_item_desc[ITEM_DESC_WRAP_MODE] as i32;
    let mut active_cell_advance_bits = shared_item_desc[ITEM_DESC_CELL_ADVANCE];

    if !is_uniform_tile && has_items {
        item_index = tile_base_item;
        let mut cur_boundary = if item_index + 1 < item_count {
            item_descriptors[(item_index + 1) * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize
        } else {
            total_bytes
        };
        while item_index + 1 < item_count && cur_boundary <= seed {
            item_index += 1;
            cur_boundary = if item_index + 1 < item_count {
                item_descriptors[(item_index + 1) * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize
            } else {
                total_bytes
            };
        }
        initial_item_index = item_index;
        start = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize;
        next_item_boundary = cur_boundary;
        active_wrap_width = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_WRAP_WIDTH] as i32;
        active_wrap_mode = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_WRAP_MODE] as i32;
        active_cell_advance_bits = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_CELL_ADVANCE];
    }
    if range_start < total_bytes {
        let is_all_8_ascii = thread_flags_word0 == 0x2121_2121u32
            && thread_flags_word1 == 0x2121_2121u32
            && ((thread_bytes_word0 | thread_bytes_word1) & 0x8080_8080u32) == 0u32
            && range_start != start
            && (range_start + 8 <= next_item_boundary)
            && (range_start + 8 <= total_bytes);

        if is_all_8_ascii {
            let cell_advance = f32::from_bits(active_cell_advance_bits);
            accumulator.clean_len = 8;
            accumulator.tail_len = 8;
            accumulator.tail_adv = cell_advance * 8.0f32;
            accumulator.head_len = 8;
            accumulator.glyphs = 8;
            accumulator.survivors = 8;
            accumulator.wrap = active_wrap_width;
            accumulator.mode = active_wrap_mode;
        } else {
            let mut id = range_start;
            while id < range_end {
            while !is_uniform_tile && has_items && next_item_boundary <= id {
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
            let lane = id - range_start;
            let is_lane0_word_ascii = lane == 0
                && thread_flags_word0 == 0x2121_2121u32
                && (thread_bytes_word0 & 0x8080_8080u32) == 0u32
                && (range_start != start)
                && (range_start + 4 <= next_item_boundary)
                && (range_start + 4 <= total_bytes);

            let is_lane4_word_ascii = lane == 4
                && thread_flags_word1 == 0x2121_2121u32
                && (thread_bytes_word1 & 0x8080_8080u32) == 0u32
                && (range_start + 4 != start)
                && (range_start + 8 <= next_item_boundary)
                && (range_start + 8 <= total_bytes);

            if is_lane0_word_ascii || is_lane4_word_ascii {
                let cell_advance = f32::from_bits(active_cell_advance_bits);
                accumulator.clean_len += 4;
                accumulator.tail_len += 4;
                accumulator.tail_adv += cell_advance * 4.0f32;
                if accumulator.nl == 0 {
                    accumulator.head_len = accumulator.tail_len;
                }
                accumulator.glyphs += 4;
                accumulator.survivors += 4;
                id += 4;
            } else {
                let flag = if lane < 4 {
                    (thread_flags_word0 >> ((lane * 8) as u32)) & 0xFF
                } else {
                    (thread_flags_word1 >> (((lane - 4) * 8) as u32)) & 0xFF
                };
                let advance = if (flag & F_LEADER) != 0 {
                    if (flag & F_CLUSTER_HEAD) != 0 {
                        bitmap_advance
                    } else if (flag & F_CLUSTER_TRAILER) != 0 {
                        0.0f32
                    } else {
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
                let is_surv = is_survivor(flag);
                if (flag & F_NEWLINE) == 0 && (flag & F_LEADER) != 0 && advance.to_bits() == active_cell_advance_bits && reset == 0 {
                    accumulator.clean_len += 1;
                    accumulator.tail_len += 1;
                    accumulator.tail_adv += advance;
                    if accumulator.nl == 0 {
                        accumulator.head_len = accumulator.tail_len;
                    }
                    accumulator.glyphs += 1;
                    if is_surv {
                        accumulator.survivors += 1;
                    }
                } else {
                    let leaf = leaf_from_flag(flag, advance, active_wrap_width, active_wrap_mode, reset, active_cell_advance_bits);
                    combine(&mut accumulator, &leaf);
                }
                id += 1;
            }
        }
        }
    } else {
        accumulator.wrap = active_wrap_width;
        accumulator.mode = active_wrap_mode;
    }

    // Phase 2: 2-Level Cube Blelloch scan across units in shared memory
    let mut shared_counts = Shared::<[i32]>::new_slice(threads_per_cube * PARTIAL_COUNT_STRIDE);
    let mut shared_metrics = Shared::<[f32]>::new_slice(threads_per_cube);
    s_store(&mut shared_counts, &mut shared_metrics, unit_idx, &accumulator, threads_per_cube);

    let mut shared_warp_counts = Shared::<[i32]>::new_slice(8usize * PARTIAL_COUNT_STRIDE);
    let mut shared_warp_metrics = Shared::<[f32]>::new_slice(8usize);

    if log == 8 {
        // Level 1: Intra-warp Blelloch up-sweep for each 32-thread warp (d in 0..5)
        #[unroll]
        for d in 0..5 {
            sync_cube();
            let s = 1usize << d;
            if (unit_idx + 1) & (2 * s - 1) == 0 {
                let mut lhs = s_load(&shared_counts, &shared_metrics, unit_idx - s, threads_per_cube);
                let rhs = s_load(&shared_counts, &shared_metrics, unit_idx, threads_per_cube);
                combine(&mut lhs, &rhs);
                s_store(&mut shared_counts, &mut shared_metrics, unit_idx, &lhs, threads_per_cube);
            }
        }
        sync_cube();

        // Save warp totals and zero warp roots
        if (unit_idx + 1) & 31 == 0 {
            let warp_idx = unit_idx >> 5;
            let total = s_load(&shared_counts, &shared_metrics, unit_idx, threads_per_cube);
            s_store(&mut shared_warp_counts, &mut shared_warp_metrics, warp_idx, &total, 8usize);
            let e = identity();
            s_store(&mut shared_counts, &mut shared_metrics, unit_idx, &e, threads_per_cube);
        }

        // Level 1: Intra-warp Blelloch down-sweep for each 32-thread warp (d in 0..5)
        #[unroll]
        for d in 0..5 {
            sync_cube();
            let s = 16usize >> d;
            if (unit_idx + 1) & (2 * s - 1) == 0 {
                let temp_carried = s_load(&shared_counts, &shared_metrics, unit_idx, threads_per_cube);
                let mut lhs = s_load(&shared_counts, &shared_metrics, unit_idx, threads_per_cube);
                let rhs = s_load(&shared_counts, &shared_metrics, unit_idx - s, threads_per_cube);
                combine(&mut lhs, &rhs);
                s_store(&mut shared_counts, &mut shared_metrics, unit_idx, &lhs, threads_per_cube);
                s_store(&mut shared_counts, &mut shared_metrics, unit_idx - s, &temp_carried, threads_per_cube);
            }
        }
        sync_cube();

        // Level 2: Warp 0 computes the exact prefix scan over the 8 warp totals
        if unit_idx == 0 {
            let mut prefix = identity();
            for w in 0usize..8usize {
                let warp_total = s_load(&shared_warp_counts, &shared_warp_metrics, w, 8usize);
                s_store(&mut shared_warp_counts, &mut shared_warp_metrics, w, &prefix, 8usize);
                combine(&mut prefix, &warp_total);
            }
        }
        sync_cube();

        // Combine warp prefix into each thread's intra-warp prefix
        let warp_idx = unit_idx >> 5;
        if warp_idx > 0 {
            let mut warp_prefix = s_load(&shared_warp_counts, &shared_warp_metrics, warp_idx, 8usize);
            let local_prefix = s_load(&shared_counts, &shared_metrics, unit_idx, threads_per_cube);
            combine(&mut warp_prefix, &local_prefix);
            s_store(&mut shared_counts, &mut shared_metrics, unit_idx, &warp_prefix, threads_per_cube);
        }
        sync_cube();
    } else {
        #[unroll]
        for d in 0..log {
            sync_cube();
            let s = 1usize << d;
            if (unit_idx + 1) & (2 * s - 1) == 0 {
                let mut lhs = s_load(&shared_counts, &shared_metrics, unit_idx - s, threads_per_cube);
                let rhs = s_load(&shared_counts, &shared_metrics, unit_idx, threads_per_cube);
                combine(&mut lhs, &rhs);
                s_store(&mut shared_counts, &mut shared_metrics, unit_idx, &lhs, threads_per_cube);
            }
        }
        sync_cube();
        if unit_idx == threads_per_cube - 1 {
            let e = identity();
            s_store(&mut shared_counts, &mut shared_metrics, unit_idx, &e, threads_per_cube);
        }
        #[unroll]
        for d in 0..log {
            sync_cube();
            let s = threads_per_cube >> (d + 1);
            if (unit_idx + 1) & (2 * s - 1) == 0 {
                let temp_carried = s_load(&shared_counts, &shared_metrics, unit_idx, threads_per_cube);
                let mut lhs = s_load(&shared_counts, &shared_metrics, unit_idx, threads_per_cube);
                let rhs = s_load(&shared_counts, &shared_metrics, unit_idx - s, threads_per_cube);
                combine(&mut lhs, &rhs);
                s_store(&mut shared_counts, &mut shared_metrics, unit_idx, &lhs, threads_per_cube);
                s_store(&mut shared_counts, &mut shared_metrics, unit_idx - s, &temp_carried, threads_per_cube);
            }
        }
        sync_cube();
    }

    // Phase 3: The chase — combines global tile prefix + exclusive thread-local prefix
    let mut run = p_load(tile_spine_counts, tile_spine_metrics, tile_idx);
    let thread_local_element = s_load(&shared_counts, &shared_metrics, unit_idx, threads_per_cube);
    combine(&mut run, &thread_local_element);


    let mut cube_item_base = 0usize;
    if track_extents {
        cube_item_base = *shared_item_base as usize;
    }

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

    let mut line_height = 0.0f32;
    let mut origin_x = 0.0f32;
    let mut origin_y = 0.0f32;
    let mut origin_z = 0.0f32;
    let mut z_step = 0.0f32;
    let mut z_step_lo = 0.0f32;
    let mut band_stride_y = 0.0f32;
    let mut depth_per_band = 0.0f32;
    let mut depth_per_col = 0.0f32;
    let mut page_rows = 0i32;
    let mut page_cols = 0i32;
    let mut scroll_rows = 0i32;
    let mut pages_wide = 1i32;
    let mut stride_reach = 0.0f32;
    let mut stride_reach_tail = 0.0f32;
    let mut item_color_base = 0u32;
    let mut item_is_per_record = 0u32;
    let mut item_flat_color = 0u32;
    let mut item_group_id = 0u32;

    let mut has_page_active = false;
    let mut item_and_group_word = 0u32;
    if has_items {
        if is_uniform_tile {
            item_index = tile_base_item;
            start = tile_item_start;
            next_item_boundary = tile_item_stop;
            active_wrap_width = shared_item_desc[ITEM_DESC_WRAP_WIDTH] as i32;
            active_wrap_mode = shared_item_desc[ITEM_DESC_WRAP_MODE] as i32;
            active_cell_advance_bits = shared_item_desc[ITEM_DESC_CELL_ADVANCE];
            fold_unit = if active_wrap_width > 0 {
                active_wrap_width
            } else if shared_item_desc[ITEM_DESC_HAS_PAGE] != 0 {
                shared_item_desc[ITEM_DESC_PAGE_COLS] as i32
            } else {
                0
            };

            line_height = 0.0f32;
            origin_x = f32::from_bits(shared_item_desc[ITEM_DESC_ORIGIN_X]);
            if !emit_derived || track_extents {
                line_height = f32::from_bits(shared_item_desc[ITEM_DESC_LINE_HEIGHT]);
                origin_y = f32::from_bits(shared_item_desc[ITEM_DESC_ORIGIN_Y]);
                origin_z = f32::from_bits(shared_item_desc[ITEM_DESC_ORIGIN_Z]);
                z_step = f32::from_bits(shared_item_desc[ITEM_DESC_Z_STEP]);
                z_step_lo = f32::from_bits(shared_item_desc[ITEM_DESC_Z_STEP_LO]);
                band_stride_y = f32::from_bits(shared_item_desc[ITEM_DESC_BAND_STRIDE_Y]);
                depth_per_band = f32::from_bits(shared_item_desc[ITEM_DESC_DEPTH_PER_BAND]);
                depth_per_col = f32::from_bits(shared_item_desc[ITEM_DESC_DEPTH_PER_COL]);
            }
            let has_page = shared_item_desc[ITEM_DESC_HAS_PAGE] != 0;
            page_rows = if has_page { shared_item_desc[ITEM_DESC_PAGE_ROWS] as i32 } else { 0 };
            page_cols = if has_page { shared_item_desc[ITEM_DESC_PAGE_COLS] as i32 } else { 0 };
            scroll_rows = if has_page { shared_item_desc[ITEM_DESC_SCROLL_ROWS] as i32 } else { 0 };
            let pages_wide_raw = shared_item_desc[ITEM_DESC_PAGES_WIDE] as i32;
            pages_wide = if pages_wide_raw > 1 { pages_wide_raw } else { 1 };
            stride_reach = 0.0f32;
            stride_reach_tail = 0.0f32;
            if has_page && page_rows > 0 {
                let page_gap_x = f32::from_bits(shared_item_desc[ITEM_DESC_PAGE_GAP_X]);
                let exact = advance_fixed(key_to_float(max_row_extents[item_index * 2]))
                    + advance_fixed(page_gap_x);
                fixed_pair(exact, &mut stride_reach, &mut stride_reach_tail);
            }
            item_color_base = shared_item_desc[ITEM_DESC_COLOR_BASE];
            item_is_per_record = shared_item_desc[ITEM_DESC_IS_PER_RECORD];
            item_flat_color = shared_item_desc[ITEM_DESC_FLAT_COLOR];
            item_group_id = shared_item_desc[ITEM_DESC_GROUP];
            has_page_active = page_rows != 0 || page_cols != 0 || scroll_rows != 0;
            item_and_group_word = (item_index as u32 & 0xFFFFu32) | ((item_group_id & 0xFFFFu32) << 16u32);
        } else {
            item_index = initial_item_index;
            start = item_descriptors[item_index * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize;
            next_item_boundary = if item_index + 1 < item_count {
                item_descriptors[(item_index + 1) * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize
            } else {
                total_bytes
            };
            let descriptor_offset = item_index * ITEM_DESC_STRIDE;
            active_wrap_width = item_descriptors[descriptor_offset + ITEM_DESC_WRAP_WIDTH] as i32;
            active_wrap_mode = item_descriptors[descriptor_offset + ITEM_DESC_WRAP_MODE] as i32;
            active_cell_advance_bits = item_descriptors[descriptor_offset + ITEM_DESC_CELL_ADVANCE];
            fold_unit = fold_of(item_descriptors, item_index, active_wrap_width);

            line_height = 0.0f32;
            origin_x = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_ORIGIN_X]);
            if !emit_derived || track_extents {
                line_height = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_LINE_HEIGHT]);
                origin_y = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_ORIGIN_Y]);
                origin_z = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_ORIGIN_Z]);
                z_step = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_Z_STEP]);
                z_step_lo = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_Z_STEP_LO]);
                band_stride_y = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_BAND_STRIDE_Y]);
                depth_per_band = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_DEPTH_PER_BAND]);
                depth_per_col = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_DEPTH_PER_COL]);
            }
            let has_page = item_descriptors[descriptor_offset + ITEM_DESC_HAS_PAGE] != 0;
            page_rows = if has_page { item_descriptors[descriptor_offset + ITEM_DESC_PAGE_ROWS] as i32 } else { 0 };
            page_cols = if has_page { item_descriptors[descriptor_offset + ITEM_DESC_PAGE_COLS] as i32 } else { 0 };
            scroll_rows = if has_page { item_descriptors[descriptor_offset + ITEM_DESC_SCROLL_ROWS] as i32 } else { 0 };
            let pages_wide_raw = item_descriptors[descriptor_offset + ITEM_DESC_PAGES_WIDE] as i32;
            pages_wide = if pages_wide_raw > 1 { pages_wide_raw } else { 1 };
            stride_reach = 0.0f32;
            stride_reach_tail = 0.0f32;
            if has_page && page_rows > 0 {
                let page_gap_x = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_PAGE_GAP_X]);
                let exact = advance_fixed(key_to_float(max_row_extents[item_index * 2]))
                    + advance_fixed(page_gap_x);
                fixed_pair(exact, &mut stride_reach, &mut stride_reach_tail);
            }
            item_color_base = item_descriptors[descriptor_offset + ITEM_DESC_COLOR_BASE];
            item_is_per_record = item_descriptors[descriptor_offset + ITEM_DESC_IS_PER_RECORD];
            item_flat_color = item_descriptors[descriptor_offset + ITEM_DESC_FLAT_COLOR];
            item_group_id = item_descriptors[descriptor_offset + ITEM_DESC_GROUP];
            has_page_active = page_rows != 0 || page_cols != 0 || scroll_rows != 0;
            item_and_group_word = (item_index as u32 & 0xFFFFu32) | ((item_group_id & 0xFFFFu32) << 16u32);
        }
    }
    let mut current_advance_x = 0.0f32;
    let mut in_segment = false;
    let mut current_item_index = item_index;
    let mut prev_row = i32::new(-1);
    let mut prev_wrap_segment = i32::new(-1);
    let mut prev_x_page = i32::new(-1);
    let mut prev_final_y = f32::new(0.0f32);
    let mut prev_final_z = f32::new(0.0f32);
    let mut prev_y_lo = f32::new(0.0f32);
    let mut prev_y_hi = f32::new(0.0f32);
    let mut seen_survivor_on_line = false;

    if range_start < total_bytes {
        let mut id = range_start;
        while id < range_end {
            while !is_uniform_tile && has_items && next_item_boundary <= id {
                if track_extents && any_leader {
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
                let descriptor_offset = item_index * ITEM_DESC_STRIDE;
                active_wrap_width = item_descriptors[descriptor_offset + ITEM_DESC_WRAP_WIDTH] as i32;
                active_wrap_mode = item_descriptors[descriptor_offset + ITEM_DESC_WRAP_MODE] as i32;
                active_cell_advance_bits = item_descriptors[descriptor_offset + ITEM_DESC_CELL_ADVANCE];
                fold_unit = fold_of(item_descriptors, item_index, active_wrap_width);

                line_height = 0.0f32;
                origin_x = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_ORIGIN_X]);
                if !emit_derived || track_extents {
                    line_height = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_LINE_HEIGHT]);
                    origin_y = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_ORIGIN_Y]);
                    origin_z = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_ORIGIN_Z]);
                    z_step = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_Z_STEP]);
                    z_step_lo = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_Z_STEP_LO]);
                    band_stride_y = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_BAND_STRIDE_Y]);
                    depth_per_band = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_DEPTH_PER_BAND]);
                    depth_per_col = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_DEPTH_PER_COL]);
                }
                let has_page = item_descriptors[descriptor_offset + ITEM_DESC_HAS_PAGE] != 0;
                page_rows = if has_page { item_descriptors[descriptor_offset + ITEM_DESC_PAGE_ROWS] as i32 } else { 0 };
                page_cols = if has_page { item_descriptors[descriptor_offset + ITEM_DESC_PAGE_COLS] as i32 } else { 0 };
                scroll_rows = if has_page { item_descriptors[descriptor_offset + ITEM_DESC_SCROLL_ROWS] as i32 } else { 0 };
                let pages_wide_raw = item_descriptors[descriptor_offset + ITEM_DESC_PAGES_WIDE] as i32;
                pages_wide = if pages_wide_raw > 1 { pages_wide_raw } else { 1 };
                stride_reach = 0.0f32;
                stride_reach_tail = 0.0f32;
                if has_page && page_rows > 0 {
                    let page_gap_x = f32::from_bits(item_descriptors[descriptor_offset + ITEM_DESC_PAGE_GAP_X]);
                    let exact = advance_fixed(key_to_float(max_row_extents[item_index * 2]))
                        + advance_fixed(page_gap_x);
                    fixed_pair(exact, &mut stride_reach, &mut stride_reach_tail);
                }
                item_color_base = item_descriptors[descriptor_offset + ITEM_DESC_COLOR_BASE];
                item_is_per_record = item_descriptors[descriptor_offset + ITEM_DESC_IS_PER_RECORD];
                item_flat_color = item_descriptors[descriptor_offset + ITEM_DESC_FLAT_COLOR];
                item_group_id = item_descriptors[descriptor_offset + ITEM_DESC_GROUP];
                has_page_active = page_rows != 0 || page_cols != 0 || scroll_rows != 0;
                item_and_group_word = (item_index as u32 & 0xFFFFu32) | ((item_group_id & 0xFFFFu32) << 16u32);

                in_segment = false;
                prev_row = i32::new(-1);
                prev_wrap_segment = i32::new(-1);
                prev_x_page = i32::new(-1);
                seen_survivor_on_line = false;
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
                prev_row = i32::new(-1);
                prev_wrap_segment = i32::new(-1);
                prev_x_page = i32::new(-1);
                seen_survivor_on_line = false;
            }
            let lane = id - range_start;
            let col = run.tail_len;
            let segment_column = if fold_unit > 0 { col % fold_unit } else { col };
            let no_wrap_crossing = fold_unit == 0 || (segment_column + 4 <= fold_unit);
            let no_page_crossing = page_cols == 0 || ((col / page_cols) == ((col + 3) / page_cols));

            let is_lane0_word_ascii = lane == 0
                && thread_flags_word0 == 0x2121_2121u32
                && (thread_bytes_word0 & 0x8080_8080u32) == 0u32
                && (range_start != start)
                && (range_start + 4 <= next_item_boundary)
                && (range_start + 4 <= total_bytes);

            let is_lane4_word_ascii = lane == 4
                && thread_flags_word1 == 0x2121_2121u32
                && (thread_bytes_word1 & 0x8080_8080u32) == 0u32
                && (range_start + 4 != start)
                && (range_start + 8 <= next_item_boundary)
                && (range_start + 8 <= total_bytes);

            let no_wrap_crossing_8 = fold_unit == 0 || (segment_column + 8 <= fold_unit);
            let no_page_crossing_8 = page_cols == 0 || ((col / page_cols) == ((col + 7) / page_cols));
            let is_all_8_ascii = lane == 0
                && thread_flags_word0 == 0x2121_2121u32
                && thread_flags_word1 == 0x2121_2121u32
                && ((thread_bytes_word0 | thread_bytes_word1) & 0x8080_8080u32) == 0u32
                && (range_start != start)
                && (range_start + 8 <= next_item_boundary)
                && (range_start + 8 <= total_bytes);

            let can_take_8_byte_burst = is_all_8_ascii && no_wrap_crossing_8 && no_page_crossing_8;
            let can_take_word_fast_path = no_wrap_crossing
                && no_page_crossing
                && (is_lane0_word_ascii || is_lane4_word_ascii);

            if can_take_8_byte_burst {
                let is_segment_head = segment_column == 0;
                if is_segment_head {
                    current_advance_x = 0.0f32;
                    in_segment = true;
                } else if !in_segment {
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
                            let is_lead = if glyph_flags.len() > 1 {
                                (flags_at(glyph_flags, start_byte_index as usize) & F_LEADER) != 0
                            } else {
                                (byte_at(bytes, start_byte_index as usize, total_bytes) & 0xC0u32) != 0x80u32
                            };
                            if is_lead {
                                backward_column -= 1;
                            }
                            if backward_column >= 1 {
                                start_byte_index -= 1;
                            }
                        }

                        // Forward accumulation
                        let mut forward_index = if start_byte_index >= 0 { start_byte_index as usize } else { 0usize };
                        while forward_index < id {
                            let is_lead = if glyph_flags.len() > 1 {
                                (flags_at(glyph_flags, forward_index) & F_LEADER) != 0
                            } else {
                                (byte_at(bytes, forward_index, total_bytes) & 0xC0u32) != 0x80u32
                            };
                            if is_lead {
                                let flag = if glyph_flags.len() > 1 {
                                    flags_at(glyph_flags, forward_index)
                                } else {
                                    0u32
                                };
                                if (flag & F_CLUSTER_HEAD) != 0 {
                                    current_advance_x += bitmap_advance;
                                } else if (flag & F_CLUSTER_TRAILER) != 0 {
                                    // trailer contributes 0.0 advance
                                } else {
                                    let lead_byte = byte_at(bytes, forward_index, total_bytes);
                                    if lead_byte >= 32u32 && lead_byte <= 126u32 {
                                        current_advance_x += f32::from_bits(active_cell_advance_bits);
                                    } else if lead_byte < 128u32 {
                                        let entry_offset = (ascii_block_base | lead_byte) as usize;
                                        current_advance_x += trie_block_metrics[entry_offset * 2];
                                    } else {
                                        let cp_len = seq_len_at(bytes, forward_index, total_bytes);
                                        let cp = cp_at(bytes, forward_index, cp_len, total_bytes);
                                        let (adv, _) = decode_trie(cp, trie_block_indices, trie_block_metrics, trie_block_codepoints, trie_block_shift);
                                        current_advance_x += adv;
                                    }
                                }
                            }
                            forward_index += 1;
                        }
                    } else if fold_unit == 0 {
                        current_advance_x = run.tail_adv;
                    }
                    in_segment = true;
                }

                let mut closed = 0i32;
                if run.nl > 0 {
                    closed = rows_for(run.head_len, active_wrap_width, active_wrap_mode) + run.rows;
                }
                let wr = wrap_row_of(col, active_wrap_width, false, active_wrap_mode);
                let row = closed + wr;
                let wrap_segment = wrap_segment_of(col, active_wrap_width, false);

                let x_page = if page_cols > 0 { col / page_cols } else { 0 };
                let mut recompute_yz = false;
                if (!emit_derived || track_extents) && (row != prev_row || wrap_segment != prev_wrap_segment || x_page != prev_x_page) {
                    prev_row = row;
                    prev_wrap_segment = wrap_segment;
                    prev_x_page = x_page;
                    recompute_yz = true;
                    seen_survivor_on_line = false;

                    let mut line_final_y = fma(-(row as f32), line_height, origin_y);
                    let depth_steps = -(wrap_segment as f32);
                    let z_tail_folded = fma(
                        depth_steps,
                        z_step_lo,
                        origin_z,
                    );
                    let mut line_final_z = fma(depth_steps, z_step, z_tail_folded);

                    if page_rows != 0 || page_cols != 0 || scroll_rows != 0 {
                        let screen_row = row - scroll_rows;
                        let mut y_page = 0;
                        if page_rows > 0 && screen_row >= page_rows {
                            y_page = screen_row / page_rows;
                        }
                        let band = y_page / pages_wide;
                        let row_in_page = (screen_row - y_page * page_rows) as f32;
                        let y_row_folded = fma(-row_in_page, line_height, origin_y);
                        line_final_y = fma(-(band as f32), band_stride_y, y_row_folded);
                        let z_stepped = fma(depth_steps, z_step, z_tail_folded);
                        let z_banded = fma(band as f32, depth_per_band, z_stepped);
                        line_final_z = fma(x_page as f32, depth_per_col, z_banded);
                    }

                    prev_final_y = line_final_y;
                    prev_final_z = line_final_z;
                    prev_y_lo = line_final_y - 0.5f32;
                    prev_y_hi = line_final_y + 0.5f32;

                    if track_extents {
                        if line_final_y < page_y_min {
                            page_y_min = line_final_y;
                        }
                        if line_final_z < page_z_min {
                            page_z_min = line_final_z;
                        }
                        if line_final_z > page_z_max {
                            page_z_max = line_final_z;
                        }
                    }
                }

                let cell_advance = f32::from_bits(active_cell_advance_bits);
                let base_x0 = current_advance_x + origin_x;
                current_advance_x += cell_advance;
                let base_x1 = current_advance_x + origin_x;
                current_advance_x += cell_advance;
                let base_x2 = current_advance_x + origin_x;
                current_advance_x += cell_advance;
                let base_x3 = current_advance_x + origin_x;
                current_advance_x += cell_advance;
                let base_x4 = current_advance_x + origin_x;
                current_advance_x += cell_advance;
                let base_x5 = current_advance_x + origin_x;
                current_advance_x += cell_advance;
                let base_x6 = current_advance_x + origin_x;
                current_advance_x += cell_advance;
                let base_x7 = current_advance_x + origin_x;
                current_advance_x += cell_advance;

                let mut final_x0 = base_x0;
                let mut final_x1 = base_x1;
                let mut final_x2 = base_x2;
                let mut final_x3 = base_x3;
                let mut final_x4 = base_x4;
                let mut final_x5 = base_x5;
                let mut final_x6 = base_x6;
                let mut final_x7 = base_x7;

                if has_page_active {
                    let screen_row = row - scroll_rows;
                    let mut y_page = 0;
                    if page_rows > 0 && screen_row >= page_rows {
                        y_page = screen_row / page_rows;
                    }
                    let page_col = (y_page % pages_wide) as f32;
                    let x_with_tail0 = fma(page_col, stride_reach_tail, base_x0);
                    final_x0 = fma(page_col, stride_reach, x_with_tail0);
                    let x_with_tail1 = fma(page_col, stride_reach_tail, base_x1);
                    final_x1 = fma(page_col, stride_reach, x_with_tail1);
                    let x_with_tail2 = fma(page_col, stride_reach_tail, base_x2);
                    final_x2 = fma(page_col, stride_reach, x_with_tail2);
                    let x_with_tail3 = fma(page_col, stride_reach_tail, base_x3);
                    final_x3 = fma(page_col, stride_reach, x_with_tail3);
                    let x_with_tail4 = fma(page_col, stride_reach_tail, base_x4);
                    final_x4 = fma(page_col, stride_reach, x_with_tail4);
                    let x_with_tail5 = fma(page_col, stride_reach_tail, base_x5);
                    final_x5 = fma(page_col, stride_reach, x_with_tail5);
                    let x_with_tail6 = fma(page_col, stride_reach_tail, base_x6);
                    final_x6 = fma(page_col, stride_reach, x_with_tail6);
                    let x_with_tail7 = fma(page_col, stride_reach_tail, base_x7);
                    final_x7 = fma(page_col, stride_reach, x_with_tail7);
                }

                any_leader = true;
                any_survivor = true;
                if track_extents {
                    let right7 = final_x7 + cell_advance;
                    if right7 > page_right_max {
                        page_right_max = right7;
                    }
                    if final_x0 < ink_x_min {
                        ink_x_min = final_x0;
                    }
                    if right7 > ink_right_max {
                        ink_right_max = right7;
                    }
                    if recompute_yz || !seen_survivor_on_line {
                        seen_survivor_on_line = true;
                        if prev_y_lo < ink_y_min {
                            ink_y_min = prev_y_lo;
                        }
                        if prev_y_hi > ink_y_max {
                            ink_y_max = prev_y_hi;
                        }
                        if prev_final_z < ink_z_min {
                            ink_z_min = prev_final_z;
                        }
                        if prev_final_z > ink_z_max {
                            ink_z_max = prev_final_z;
                        }
                    }
                }

                let b0 = thread_bytes_word0 & 0xFF;
                let b1 = (thread_bytes_word0 >> 8u32) & 0xFF;
                let b2 = (thread_bytes_word0 >> 16u32) & 0xFF;
                let b3 = (thread_bytes_word0 >> 24u32) & 0xFF;
                let b4 = thread_bytes_word1 & 0xFF;
                let b5 = (thread_bytes_word1 >> 8u32) & 0xFF;
                let b6 = (thread_bytes_word1 >> 16u32) & 0xFF;
                let b7 = (thread_bytes_word1 >> 24u32) & 0xFF;
                let glyph_id0 = b0 - 31u32;
                let glyph_id1 = b1 - 31u32;
                let glyph_id2 = b2 - 31u32;
                let glyph_id3 = b3 - 31u32;
                let glyph_id4 = b4 - 31u32;
                let glyph_id5 = b5 - 31u32;
                let glyph_id6 = b6 - 31u32;
                let glyph_id7 = b7 - 31u32;

                let (color0, color1, color2, color3, color4, color5, color6, color7) = if item_is_per_record != 0u32 {
                    let color_offset = (item_color_base + run.glyphs as u32) as usize;
                    (
                        per_record_semantic_colors[color_offset],
                        per_record_semantic_colors[color_offset + 1],
                        per_record_semantic_colors[color_offset + 2],
                        per_record_semantic_colors[color_offset + 3],
                        per_record_semantic_colors[color_offset + 4],
                        per_record_semantic_colors[color_offset + 5],
                        per_record_semantic_colors[color_offset + 6],
                        per_record_semantic_colors[color_offset + 7],
                    )
                } else {
                    (
                        item_flat_color, item_flat_color, item_flat_color, item_flat_color,
                        item_flat_color, item_flat_color, item_flat_color, item_flat_color,
                    )
                };

                if emit_derived {
                    let slot_word_offset = survivor_ordinal as usize * 5;
                    if slot_word_offset + 40 <= instance_slots.len() {
                        let wrap_hi = (wrap_segment as u32) << 16u32;
                        instance_slots[slot_word_offset]      = final_x0.to_bits();
                        instance_slots[slot_word_offset + 1]  = row as u32;
                        instance_slots[slot_word_offset + 2]  = glyph_id0 | wrap_hi;
                        instance_slots[slot_word_offset + 3]  = color0;
                        instance_slots[slot_word_offset + 4]  = item_and_group_word;

                        instance_slots[slot_word_offset + 5]  = final_x1.to_bits();
                        instance_slots[slot_word_offset + 6]  = row as u32;
                        instance_slots[slot_word_offset + 7]  = glyph_id1 | wrap_hi;
                        instance_slots[slot_word_offset + 8]  = color1;
                        instance_slots[slot_word_offset + 9]  = item_and_group_word;

                        instance_slots[slot_word_offset + 10] = final_x2.to_bits();
                        instance_slots[slot_word_offset + 11] = row as u32;
                        instance_slots[slot_word_offset + 12] = glyph_id2 | wrap_hi;
                        instance_slots[slot_word_offset + 13] = color2;
                        instance_slots[slot_word_offset + 14] = item_and_group_word;

                        instance_slots[slot_word_offset + 15] = final_x3.to_bits();
                        instance_slots[slot_word_offset + 16] = row as u32;
                        instance_slots[slot_word_offset + 17] = glyph_id3 | wrap_hi;
                        instance_slots[slot_word_offset + 18] = color3;
                        instance_slots[slot_word_offset + 19] = item_and_group_word;

                        instance_slots[slot_word_offset + 20] = final_x4.to_bits();
                        instance_slots[slot_word_offset + 21] = row as u32;
                        instance_slots[slot_word_offset + 22] = glyph_id4 | wrap_hi;
                        instance_slots[slot_word_offset + 23] = color4;
                        instance_slots[slot_word_offset + 24] = item_and_group_word;

                        instance_slots[slot_word_offset + 25] = final_x5.to_bits();
                        instance_slots[slot_word_offset + 26] = row as u32;
                        instance_slots[slot_word_offset + 27] = glyph_id5 | wrap_hi;
                        instance_slots[slot_word_offset + 28] = color5;
                        instance_slots[slot_word_offset + 29] = item_and_group_word;

                        instance_slots[slot_word_offset + 30] = final_x6.to_bits();
                        instance_slots[slot_word_offset + 31] = row as u32;
                        instance_slots[slot_word_offset + 32] = glyph_id6 | wrap_hi;
                        instance_slots[slot_word_offset + 33] = color6;
                        instance_slots[slot_word_offset + 34] = item_and_group_word;

                        instance_slots[slot_word_offset + 35] = final_x7.to_bits();
                        instance_slots[slot_word_offset + 36] = row as u32;
                        instance_slots[slot_word_offset + 37] = glyph_id7 | wrap_hi;
                        instance_slots[slot_word_offset + 38] = color7;
                        instance_slots[slot_word_offset + 39] = item_and_group_word;
                    }
                } else {
                    let slot_word_offset = survivor_ordinal as usize * 8;
                    if slot_word_offset + 64 <= instance_slots.len() {
                        let one_bits = 0x3f800000u32;
                        let cell_bits = active_cell_advance_bits;
                        let y_bits = prev_final_y.to_bits();
                        let z_bits = prev_final_z.to_bits();

                        instance_slots[slot_word_offset]      = final_x0.to_bits();
                        instance_slots[slot_word_offset + 1]  = y_bits;
                        instance_slots[slot_word_offset + 2]  = z_bits;
                        instance_slots[slot_word_offset + 3]  = glyph_id0;
                        instance_slots[slot_word_offset + 4]  = color0;
                        instance_slots[slot_word_offset + 5]  = item_group_id;
                        instance_slots[slot_word_offset + 6]  = cell_bits;
                        instance_slots[slot_word_offset + 7]  = one_bits;

                        instance_slots[slot_word_offset + 8]  = final_x1.to_bits();
                        instance_slots[slot_word_offset + 9]  = y_bits;
                        instance_slots[slot_word_offset + 10] = z_bits;
                        instance_slots[slot_word_offset + 11] = glyph_id1;
                        instance_slots[slot_word_offset + 12] = color1;
                        instance_slots[slot_word_offset + 13] = item_group_id;
                        instance_slots[slot_word_offset + 14] = cell_bits;
                        instance_slots[slot_word_offset + 15] = one_bits;

                        instance_slots[slot_word_offset + 16] = final_x2.to_bits();
                        instance_slots[slot_word_offset + 17] = y_bits;
                        instance_slots[slot_word_offset + 18] = z_bits;
                        instance_slots[slot_word_offset + 19] = glyph_id2;
                        instance_slots[slot_word_offset + 20] = color2;
                        instance_slots[slot_word_offset + 21] = item_group_id;
                        instance_slots[slot_word_offset + 22] = cell_bits;
                        instance_slots[slot_word_offset + 23] = one_bits;

                        instance_slots[slot_word_offset + 24] = final_x3.to_bits();
                        instance_slots[slot_word_offset + 25] = y_bits;
                        instance_slots[slot_word_offset + 26] = z_bits;
                        instance_slots[slot_word_offset + 27] = glyph_id3;
                        instance_slots[slot_word_offset + 28] = color3;
                        instance_slots[slot_word_offset + 29] = item_group_id;
                        instance_slots[slot_word_offset + 30] = cell_bits;
                        instance_slots[slot_word_offset + 31] = one_bits;

                        instance_slots[slot_word_offset + 32] = final_x4.to_bits();
                        instance_slots[slot_word_offset + 33] = y_bits;
                        instance_slots[slot_word_offset + 34] = z_bits;
                        instance_slots[slot_word_offset + 35] = glyph_id4;
                        instance_slots[slot_word_offset + 36] = color4;
                        instance_slots[slot_word_offset + 37] = item_group_id;
                        instance_slots[slot_word_offset + 38] = cell_bits;
                        instance_slots[slot_word_offset + 39] = one_bits;

                        instance_slots[slot_word_offset + 40] = final_x5.to_bits();
                        instance_slots[slot_word_offset + 41] = y_bits;
                        instance_slots[slot_word_offset + 42] = z_bits;
                        instance_slots[slot_word_offset + 43] = glyph_id5;
                        instance_slots[slot_word_offset + 44] = color5;
                        instance_slots[slot_word_offset + 45] = item_group_id;
                        instance_slots[slot_word_offset + 46] = cell_bits;
                        instance_slots[slot_word_offset + 47] = one_bits;

                        instance_slots[slot_word_offset + 48] = final_x6.to_bits();
                        instance_slots[slot_word_offset + 49] = y_bits;
                        instance_slots[slot_word_offset + 50] = z_bits;
                        instance_slots[slot_word_offset + 51] = glyph_id6;
                        instance_slots[slot_word_offset + 52] = color6;
                        instance_slots[slot_word_offset + 53] = item_group_id;
                        instance_slots[slot_word_offset + 54] = cell_bits;
                        instance_slots[slot_word_offset + 55] = one_bits;

                        instance_slots[slot_word_offset + 56] = final_x7.to_bits();
                        instance_slots[slot_word_offset + 57] = y_bits;
                        instance_slots[slot_word_offset + 58] = z_bits;
                        instance_slots[slot_word_offset + 59] = glyph_id7;
                        instance_slots[slot_word_offset + 60] = color7;
                        instance_slots[slot_word_offset + 61] = item_group_id;
                        instance_slots[slot_word_offset + 62] = cell_bits;
                        instance_slots[slot_word_offset + 63] = one_bits;
                    }
                    let tint_word_offset = survivor_ordinal as usize * 2;
                    if tint_word_offset + 16 <= instance_tints.len() {
                        instance_tints[tint_word_offset]      = glyph_id0;
                        instance_tints[tint_word_offset + 1]  = color0;
                        instance_tints[tint_word_offset + 2]  = glyph_id1;
                        instance_tints[tint_word_offset + 3]  = color1;
                        instance_tints[tint_word_offset + 4]  = glyph_id2;
                        instance_tints[tint_word_offset + 5]  = color2;
                        instance_tints[tint_word_offset + 6]  = glyph_id3;
                        instance_tints[tint_word_offset + 7]  = color3;
                        instance_tints[tint_word_offset + 8]  = glyph_id4;
                        instance_tints[tint_word_offset + 9]  = color4;
                        instance_tints[tint_word_offset + 10] = glyph_id5;
                        instance_tints[tint_word_offset + 11] = color5;
                        instance_tints[tint_word_offset + 12] = glyph_id6;
                        instance_tints[tint_word_offset + 13] = color6;
                        instance_tints[tint_word_offset + 14] = glyph_id7;
                        instance_tints[tint_word_offset + 15] = color7;
                    }
                }

                survivor_ordinal += 8u32;
                run.clean_len += 8;
                run.tail_len += 8;
                run.tail_adv += cell_advance * 8.0f32;
                if run.nl == 0 {
                    run.head_len = run.tail_len;
                }
                run.glyphs += 8;
                run.survivors += 8;
                id = range_end;
            } else if can_take_word_fast_path {
                let is_segment_head = segment_column == 0;
                if is_segment_head {
                    current_advance_x = 0.0f32;
                    in_segment = true;
                } else if !in_segment {
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
                            let is_lead = if glyph_flags.len() > 1 {
                                (flags_at(glyph_flags, start_byte_index as usize) & F_LEADER) != 0
                            } else {
                                (byte_at(bytes, start_byte_index as usize, total_bytes) & 0xC0u32) != 0x80u32
                            };
                            if is_lead {
                                backward_column -= 1;
                            }
                            if backward_column >= 1 {
                                start_byte_index -= 1;
                            }
                        }

                        // Forward accumulation
                        let mut forward_index = if start_byte_index >= 0 { start_byte_index as usize } else { 0usize };
                        while forward_index < id {
                            let is_lead = if glyph_flags.len() > 1 {
                                (flags_at(glyph_flags, forward_index) & F_LEADER) != 0
                            } else {
                                (byte_at(bytes, forward_index, total_bytes) & 0xC0u32) != 0x80u32
                            };
                            if is_lead {
                                let flag = if glyph_flags.len() > 1 {
                                    flags_at(glyph_flags, forward_index)
                                } else {
                                    0u32
                                };
                                if (flag & F_CLUSTER_HEAD) != 0 {
                                    current_advance_x += bitmap_advance;
                                } else if (flag & F_CLUSTER_TRAILER) != 0 {
                                    // trailer contributes 0.0 advance
                                } else {
                                    let lead_byte = byte_at(bytes, forward_index, total_bytes);
                                    if lead_byte >= 32u32 && lead_byte <= 126u32 {
                                        current_advance_x += f32::from_bits(active_cell_advance_bits);
                                    } else if lead_byte < 128u32 {
                                        let entry_offset = (ascii_block_base | lead_byte) as usize;
                                        current_advance_x += trie_block_metrics[entry_offset * 2];
                                    } else {
                                        let cp_len = seq_len_at(bytes, forward_index, total_bytes);
                                        let cp = cp_at(bytes, forward_index, cp_len, total_bytes);
                                        let (adv, _) = decode_trie(cp, trie_block_indices, trie_block_metrics, trie_block_codepoints, trie_block_shift);
                                        current_advance_x += adv;
                                    }
                                }
                            }
                            forward_index += 1;
                        }
                    } else if fold_unit == 0 {
                        current_advance_x = run.tail_adv;
                    }
                    in_segment = true;
                }

                let mut closed = 0i32;
                if run.nl > 0 {
                    closed = rows_for(run.head_len, active_wrap_width, active_wrap_mode) + run.rows;
                }
                let wr = wrap_row_of(col, active_wrap_width, false, active_wrap_mode);
                let row = closed + wr;
                let wrap_segment = wrap_segment_of(col, active_wrap_width, false);

                let x_page = if page_cols > 0 { col / page_cols } else { 0 };
                let mut recompute_yz = false;
                if (!emit_derived || track_extents) && (row != prev_row || wrap_segment != prev_wrap_segment || x_page != prev_x_page) {
                    prev_row = row;
                    prev_wrap_segment = wrap_segment;
                    prev_x_page = x_page;
                    recompute_yz = true;
                    seen_survivor_on_line = false;

                    let mut line_final_y = fma(-(row as f32), line_height, origin_y);
                    let depth_steps = -(wrap_segment as f32);
                    let z_tail_folded = fma(
                        depth_steps,
                        z_step_lo,
                        origin_z,
                    );
                    let mut line_final_z = fma(depth_steps, z_step, z_tail_folded);

                    if page_rows != 0 || page_cols != 0 || scroll_rows != 0 {
                        let screen_row = row - scroll_rows;
                        let mut y_page = 0;
                        if page_rows > 0 && screen_row >= page_rows {
                            y_page = screen_row / page_rows;
                        }
                        let band = y_page / pages_wide;
                        let row_in_page = (screen_row - y_page * page_rows) as f32;
                        let y_row_folded = fma(-row_in_page, line_height, origin_y);
                        line_final_y = fma(-(band as f32), band_stride_y, y_row_folded);
                        let z_stepped = fma(depth_steps, z_step, z_tail_folded);
                        let z_banded = fma(band as f32, depth_per_band, z_stepped);
                        line_final_z = fma(x_page as f32, depth_per_col, z_banded);
                    }

                    prev_final_y = line_final_y;
                    prev_final_z = line_final_z;
                    prev_y_lo = line_final_y - 0.5f32;
                    prev_y_hi = line_final_y + 0.5f32;

                    if track_extents {
                        if line_final_y < page_y_min {
                            page_y_min = line_final_y;
                        }
                        if line_final_z < page_z_min {
                            page_z_min = line_final_z;
                        }
                        if line_final_z > page_z_max {
                            page_z_max = line_final_z;
                        }
                    }
                }

                let cell_advance = f32::from_bits(active_cell_advance_bits);
                let base_x0 = current_advance_x + origin_x;
                current_advance_x += cell_advance;
                let base_x1 = current_advance_x + origin_x;
                current_advance_x += cell_advance;
                let base_x2 = current_advance_x + origin_x;
                current_advance_x += cell_advance;
                let base_x3 = current_advance_x + origin_x;
                current_advance_x += cell_advance;

                let mut final_x0 = base_x0;
                let mut final_x1 = base_x1;
                let mut final_x2 = base_x2;
                let mut final_x3 = base_x3;

                if has_page_active {
                    let screen_row = row - scroll_rows;
                    let mut y_page = 0;
                    if page_rows > 0 && screen_row >= page_rows {
                        y_page = screen_row / page_rows;
                    }
                    let page_col = (y_page % pages_wide) as f32;
                    let x_with_tail0 = fma(page_col, stride_reach_tail, base_x0);
                    final_x0 = fma(page_col, stride_reach, x_with_tail0);
                    let x_with_tail1 = fma(page_col, stride_reach_tail, base_x1);
                    final_x1 = fma(page_col, stride_reach, x_with_tail1);
                    let x_with_tail2 = fma(page_col, stride_reach_tail, base_x2);
                    final_x2 = fma(page_col, stride_reach, x_with_tail2);
                    let x_with_tail3 = fma(page_col, stride_reach_tail, base_x3);
                    final_x3 = fma(page_col, stride_reach, x_with_tail3);
                }

                any_leader = true;
                any_survivor = true;
                if track_extents {
                    let right3 = final_x3 + cell_advance;
                    if right3 > page_right_max {
                        page_right_max = right3;
                    }
                    if final_x0 < ink_x_min {
                        ink_x_min = final_x0;
                    }
                    if right3 > ink_right_max {
                        ink_right_max = right3;
                    }
                    if recompute_yz || !seen_survivor_on_line {
                        seen_survivor_on_line = true;
                        if prev_y_lo < ink_y_min {
                            ink_y_min = prev_y_lo;
                        }
                        if prev_y_hi > ink_y_max {
                            ink_y_max = prev_y_hi;
                        }
                        if prev_final_z < ink_z_min {
                            ink_z_min = prev_final_z;
                        }
                        if prev_final_z > ink_z_max {
                            ink_z_max = prev_final_z;
                        }
                    }
                }

                let (color0, color1, color2, color3) = if item_is_per_record != 0u32 {
                    let color_offset = (item_color_base + run.glyphs as u32) as usize;
                    (
                        per_record_semantic_colors[color_offset],
                        per_record_semantic_colors[color_offset + 1],
                        per_record_semantic_colors[color_offset + 2],
                        per_record_semantic_colors[color_offset + 3],
                    )
                } else {
                    (item_flat_color, item_flat_color, item_flat_color, item_flat_color)
                };

                let bytes_word = if lane == 0 { thread_bytes_word0 } else { thread_bytes_word1 };
                let b0 = bytes_word & 0xFF;
                let b1 = (bytes_word >> 8u32) & 0xFF;
                let b2 = (bytes_word >> 16u32) & 0xFF;
                let b3 = (bytes_word >> 24u32) & 0xFF;
                let glyph_id0 = b0 - 31u32;
                let glyph_id1 = b1 - 31u32;
                let glyph_id2 = b2 - 31u32;
                let glyph_id3 = b3 - 31u32;

                if emit_derived {
                    let slot_word_offset = survivor_ordinal as usize * 5;
                    if slot_word_offset + 20 <= instance_slots.len() {
                        let glyph_and_wrap0 = glyph_id0 | ((wrap_segment as u32) << 16u32);
                        instance_slots[slot_word_offset]      = final_x0.to_bits();
                        instance_slots[slot_word_offset + 1]  = row as u32;
                        instance_slots[slot_word_offset + 2]  = glyph_and_wrap0;
                        instance_slots[slot_word_offset + 3]  = color0;
                        instance_slots[slot_word_offset + 4]  = item_and_group_word;

                        let glyph_and_wrap1 = glyph_id1 | ((wrap_segment as u32) << 16u32);
                        instance_slots[slot_word_offset + 5]  = final_x1.to_bits();
                        instance_slots[slot_word_offset + 6]  = row as u32;
                        instance_slots[slot_word_offset + 7]  = glyph_and_wrap1;
                        instance_slots[slot_word_offset + 8]  = color1;
                        instance_slots[slot_word_offset + 9]  = item_and_group_word;

                        let glyph_and_wrap2 = glyph_id2 | ((wrap_segment as u32) << 16u32);
                        instance_slots[slot_word_offset + 10] = final_x2.to_bits();
                        instance_slots[slot_word_offset + 11] = row as u32;
                        instance_slots[slot_word_offset + 12] = glyph_and_wrap2;
                        instance_slots[slot_word_offset + 13] = color2;
                        instance_slots[slot_word_offset + 14] = item_and_group_word;

                        let glyph_and_wrap3 = glyph_id3 | ((wrap_segment as u32) << 16u32);
                        instance_slots[slot_word_offset + 15] = final_x3.to_bits();
                        instance_slots[slot_word_offset + 16] = row as u32;
                        instance_slots[slot_word_offset + 17] = glyph_and_wrap3;
                        instance_slots[slot_word_offset + 18] = color3;
                        instance_slots[slot_word_offset + 19] = item_and_group_word;
                    }
                } else {
                    let slot_word_offset = survivor_ordinal as usize * 8;
                    if slot_word_offset + 32 <= instance_slots.len() {
                        let one_bits = 0x3f800000u32;
                        let cell_bits = active_cell_advance_bits;
                        let y_bits = prev_final_y.to_bits();
                        let z_bits = prev_final_z.to_bits();

                        instance_slots[slot_word_offset]      = final_x0.to_bits();
                        instance_slots[slot_word_offset + 1]  = y_bits;
                        instance_slots[slot_word_offset + 2]  = z_bits;
                        instance_slots[slot_word_offset + 3]  = glyph_id0;
                        instance_slots[slot_word_offset + 4]  = color0;
                        instance_slots[slot_word_offset + 5]  = item_group_id;
                        instance_slots[slot_word_offset + 6]  = cell_bits;
                        instance_slots[slot_word_offset + 7]  = one_bits;

                        instance_slots[slot_word_offset + 8]  = final_x1.to_bits();
                        instance_slots[slot_word_offset + 9]  = y_bits;
                        instance_slots[slot_word_offset + 10] = z_bits;
                        instance_slots[slot_word_offset + 11] = glyph_id1;
                        instance_slots[slot_word_offset + 12] = color1;
                        instance_slots[slot_word_offset + 13] = item_group_id;
                        instance_slots[slot_word_offset + 14] = cell_bits;
                        instance_slots[slot_word_offset + 15] = one_bits;

                        instance_slots[slot_word_offset + 16] = final_x2.to_bits();
                        instance_slots[slot_word_offset + 17] = y_bits;
                        instance_slots[slot_word_offset + 18] = z_bits;
                        instance_slots[slot_word_offset + 19] = glyph_id2;
                        instance_slots[slot_word_offset + 20] = color2;
                        instance_slots[slot_word_offset + 21] = item_group_id;
                        instance_slots[slot_word_offset + 22] = cell_bits;
                        instance_slots[slot_word_offset + 23] = one_bits;

                        instance_slots[slot_word_offset + 24] = final_x3.to_bits();
                        instance_slots[slot_word_offset + 25] = y_bits;
                        instance_slots[slot_word_offset + 26] = z_bits;
                        instance_slots[slot_word_offset + 27] = glyph_id3;
                        instance_slots[slot_word_offset + 28] = color3;
                        instance_slots[slot_word_offset + 29] = item_group_id;
                        instance_slots[slot_word_offset + 30] = cell_bits;
                        instance_slots[slot_word_offset + 31] = one_bits;
                    }
                    let tint_word_offset = survivor_ordinal as usize * 2;
                    if tint_word_offset + 8 <= instance_tints.len() {
                        instance_tints[tint_word_offset]     = glyph_id0;
                        instance_tints[tint_word_offset + 1] = color0;
                        instance_tints[tint_word_offset + 2] = glyph_id1;
                        instance_tints[tint_word_offset + 3] = color1;
                        instance_tints[tint_word_offset + 4] = glyph_id2;
                        instance_tints[tint_word_offset + 5] = color2;
                        instance_tints[tint_word_offset + 6] = glyph_id3;
                        instance_tints[tint_word_offset + 7] = color3;
                    }
                }

                survivor_ordinal += 4u32;
                run.clean_len += 4;
                run.tail_len += 4;
                run.tail_adv += cell_advance * 4.0f32;
                if run.nl == 0 {
                    run.head_len = run.tail_len;
                }
                run.glyphs += 4;
                run.survivors += 4;
                id += 4;
            } else {
                let glyph_flags_val = if lane < 4 {
                    (thread_flags_word0 >> ((lane * 8) as u32)) & 0xFF
                } else {
                    (thread_flags_word1 >> (((lane - 4) * 8) as u32)) & 0xFF
                };
                let glyph_advance = if (glyph_flags_val & super::F_LEADER) != 0 {
                    if (glyph_flags_val & F_CLUSTER_HEAD) != 0 {
                        bitmap_advance
                    } else if (glyph_flags_val & F_CLUSTER_TRAILER) != 0 {
                        0.0f32
                    } else {
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
                if is_segment_head {
                    current_advance_x = 0.0f32;
                    in_segment = true;
                } else if !in_segment {
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
                            let is_lead = if glyph_flags.len() > 1 {
                                (flags_at(glyph_flags, start_byte_index as usize) & F_LEADER) != 0
                            } else {
                                (byte_at(bytes, start_byte_index as usize, total_bytes) & 0xC0u32) != 0x80u32
                            };
                            if is_lead {
                                backward_column -= 1;
                            }
                            if backward_column >= 1 {
                                start_byte_index -= 1;
                            }
                        }

                        // Forward accumulation
                        let mut forward_index = if start_byte_index >= 0 { start_byte_index as usize } else { 0usize };
                        while forward_index < id {
                            let is_lead = if glyph_flags.len() > 1 {
                                (flags_at(glyph_flags, forward_index) & F_LEADER) != 0
                            } else {
                                (byte_at(bytes, forward_index, total_bytes) & 0xC0u32) != 0x80u32
                            };
                            if is_lead {
                                let flag = if glyph_flags.len() > 1 {
                                    flags_at(glyph_flags, forward_index)
                                } else {
                                    0u32
                                };
                                if (flag & F_CLUSTER_HEAD) != 0 {
                                    current_advance_x += bitmap_advance;
                                } else if (flag & F_CLUSTER_TRAILER) != 0 {
                                    // trailer contributes 0.0 advance
                                } else {
                                    let lead_byte = byte_at(bytes, forward_index, total_bytes);
                                    if lead_byte >= 32u32 && lead_byte <= 126u32 {
                                        current_advance_x += f32::from_bits(active_cell_advance_bits);
                                    } else if lead_byte < 128u32 {
                                        let entry_offset = (ascii_block_base | lead_byte) as usize;
                                        current_advance_x += trie_block_metrics[entry_offset * 2];
                                    } else {
                                        let cp_len = seq_len_at(bytes, forward_index, total_bytes);
                                        let cp = cp_at(bytes, forward_index, cp_len, total_bytes);
                                        let (adv, _) = decode_trie(cp, trie_block_indices, trie_block_metrics, trie_block_codepoints, trie_block_shift);
                                        current_advance_x += adv;
                                    }
                                }
                            }
                            forward_index += 1;
                        }
                    } else if fold_unit == 0 {
                        current_advance_x = run.tail_adv;
                    }
                    in_segment = true;
                }

                let wrap_segment = wrap_segment_of(col, active_wrap_width, (glyph_flags_val & F_NEWLINE) != 0);
                let base_x = current_advance_x + origin_x;

                let mut final_x = base_x;

                let x_page = if page_cols > 0 { col / page_cols } else { 0 };
                let mut recompute_yz = false;
                if (!emit_derived || track_extents) && (row != prev_row || wrap_segment != prev_wrap_segment || x_page != prev_x_page) {
                    prev_row = row;
                    prev_wrap_segment = wrap_segment;
                    prev_x_page = x_page;
                    recompute_yz = true;
                    seen_survivor_on_line = false;

                    let mut line_final_y = fma(-(row as f32), line_height, origin_y);
                    let depth_steps = -(wrap_segment as f32);
                    let z_tail_folded = fma(
                        depth_steps,
                        z_step_lo,
                        origin_z,
                    );
                    let mut line_final_z = fma(depth_steps, z_step, z_tail_folded);

                    if page_rows != 0 || page_cols != 0 || scroll_rows != 0 {
                        let screen_row = row - scroll_rows;
                        let mut y_page = 0;
                        if page_rows > 0 && screen_row >= page_rows {
                            y_page = screen_row / page_rows;
                        }
                        let band = y_page / pages_wide;
                        let row_in_page = (screen_row - y_page * page_rows) as f32;
                        let y_row_folded = fma(-row_in_page, line_height, origin_y);
                        line_final_y = fma(-(band as f32), band_stride_y, y_row_folded);
                        let z_stepped = fma(depth_steps, z_step, z_tail_folded);
                        let z_banded = fma(band as f32, depth_per_band, z_stepped);
                        line_final_z = fma(x_page as f32, depth_per_col, z_banded);
                    }

                    prev_final_y = line_final_y;
                    prev_final_z = line_final_z;
                    prev_y_lo = line_final_y - 0.5f32;
                    prev_y_hi = line_final_y + 0.5f32;

                    if track_extents {
                        if line_final_y < page_y_min {
                            page_y_min = line_final_y;
                        }
                        if line_final_z < page_z_min {
                            page_z_min = line_final_z;
                        }
                        if line_final_z > page_z_max {
                            page_z_max = line_final_z;
                        }
                    }
                }

                if has_page_active {
                    let screen_row = row - scroll_rows;
                    let mut y_page = 0;
                    if page_rows > 0 && screen_row >= page_rows {
                        y_page = screen_row / page_rows;
                    }
                    let page_col = (y_page % pages_wide) as f32;
                    let x_with_tail = fma(page_col, stride_reach_tail, base_x);
                    final_x = fma(page_col, stride_reach, x_with_tail);
                }

                any_leader = true;
                let right = final_x + glyph_advance;
                if track_extents && right > page_right_max {
                    page_right_max = right;
                }
                if is_survivor(glyph_flags_val) {
                    any_survivor = true;
                    if track_extents {
                        if final_x < ink_x_min {
                            ink_x_min = final_x;
                        }
                        if right > ink_right_max {
                            ink_right_max = right;
                        }
                        if recompute_yz || !seen_survivor_on_line {
                            seen_survivor_on_line = true;
                            if prev_y_lo < ink_y_min {
                                ink_y_min = prev_y_lo;
                            }
                            if prev_y_hi > ink_y_max {
                                ink_y_max = prev_y_hi;
                            }
                            if prev_final_z < ink_z_min {
                                ink_z_min = prev_final_z;
                            }
                            if prev_final_z > ink_z_max {
                                ink_z_max = prev_final_z;
                            }
                        }
                    }

                    // Direct instance emission: emit 8-word slot and 2-word tint
                    let color = if item_is_per_record != 0u32 {
                        let record_ordinal = run.glyphs as u32;
                        per_record_semantic_colors[(item_color_base + record_ordinal) as usize]
                    } else {
                        item_flat_color
                    };
                    let mut glyph_id = 0u32;
                    if (glyph_flags_val & F_CLUSTER_HEAD) != 0 {
                        let total = _candidate_total[0];
                        let mut lo = 0u32;
                        let mut hi = total;
                        while lo < hi {
                            let mid = (lo + hi) / 2u32;
                            let pos = _candidate_head_positions[mid as usize];
                            if pos < id as u32 {
                                lo = mid + 1u32;
                            } else {
                                hi = mid;
                            }
                        }
                        if lo < total && _candidate_head_positions[lo as usize] == id as u32 {
                            glyph_id = _candidate_slots[lo as usize];
                        }
                    } else {
                        let lane = id - range_start;
                        let lead_byte = if lane < 4 {
                            (thread_bytes_word0 >> ((lane * 8) as u32)) & 0xFF
                        } else {
                            (thread_bytes_word1 >> (((lane - 4) * 8) as u32)) & 0xFF
                        };
                        if lead_byte < 128u32 {
                            glyph_id = if lead_byte >= 32u32 && lead_byte <= 126u32 {
                                lead_byte - 31u32
                            } else {
                                0u32
                            };
                        } else {
                            let cp_len = seq_len_at(bytes, id, total_bytes);
                            let cp = cp_at(bytes, id, cp_len, total_bytes);
                            let (_, decoded_glyph_id) = decode_trie(cp, trie_block_indices, trie_block_metrics, trie_block_codepoints, trie_block_shift);
                            glyph_id = decoded_glyph_id;
                        }
                    }

                    if emit_derived {
                        let slot_word_offset = survivor_ordinal as usize * 5;
                        if slot_word_offset + 5 <= instance_slots.len() {
                            let glyph_and_wrap = glyph_id | ((wrap_segment as u32) << 16u32);
                            instance_slots[slot_word_offset] = final_x.to_bits();
                            instance_slots[slot_word_offset + 1] = row as u32;
                            instance_slots[slot_word_offset + 2] = glyph_and_wrap;
                            instance_slots[slot_word_offset + 3] = color;
                            instance_slots[slot_word_offset + 4] = item_and_group_word;
                        }
                    } else {
                        let slot_word_offset = survivor_ordinal as usize * 8;
                        if slot_word_offset + 8 <= instance_slots.len() {
                            instance_slots[slot_word_offset] = final_x.to_bits();
                            instance_slots[slot_word_offset + 1] = prev_final_y.to_bits();
                            instance_slots[slot_word_offset + 2] = prev_final_z.to_bits();
                            instance_slots[slot_word_offset + 3] = glyph_id;
                            instance_slots[slot_word_offset + 4] = color;
                            instance_slots[slot_word_offset + 5] = item_group_id;
                            instance_slots[slot_word_offset + 6] = glyph_advance.to_bits();
                            instance_slots[slot_word_offset + 7] = 0x3f800000u32; // 1.0f32.to_bits()
                        }
                    }
                    if !emit_derived {
                        let tint_word_offset = survivor_ordinal as usize * 2;
                        if tint_word_offset + 2 <= instance_tints.len() {
                            instance_tints[tint_word_offset] = glyph_id;
                            instance_tints[tint_word_offset + 1] = color;
                        }
                    }
                    survivor_ordinal += 1u32;
                }

                if (glyph_flags_val & F_NEWLINE) == 0 {
                    current_advance_x += glyph_advance;
                }
            }
            let advance_for_leaf = if (glyph_flags_val & F_LEADER) != 0 { glyph_advance } else { 0.0f32 };
            let reset_int = if reset { 1i32 } else { 0i32 };
            let is_surv = is_survivor(glyph_flags_val);
            if (glyph_flags_val & F_NEWLINE) == 0 && (glyph_flags_val & F_LEADER) != 0 && glyph_advance.to_bits() == active_cell_advance_bits && reset_int == 0 {
                run.clean_len += 1;
                run.tail_len += 1;
                run.tail_adv += glyph_advance;
                if run.nl == 0 {
                    run.head_len = run.tail_len;
                }
                run.glyphs += 1;
                if is_surv {
                    run.survivors += 1;
                }
            } else {
                let leaf = leaf_from_flag(glyph_flags_val, advance_for_leaf, active_wrap_width, active_wrap_mode, reset_int, active_cell_advance_bits);
                combine(&mut run, &leaf);
            }
            id += 1;
            }
        }

        if track_extents && any_leader {
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

    if track_extents {
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

