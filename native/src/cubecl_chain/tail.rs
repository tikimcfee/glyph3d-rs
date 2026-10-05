use cubecl::prelude::*;

use super::monoid::{flags_at, item_search, ordered_key};
use super::{F_LEADER, LC_COL, LC_ROW, LC_STRIDE, LM_STRIDE, LM_X, LM_Y, LM_Z, RESOLVE_SLOTS};

/// The record emitter — phase 4, rung 2. One thread per LEADER ORDINAL
/// over the otb compaction, gathering the per-byte lanes into the 32 B
/// wire record stream [X Y Z ADVANCE HEIGHT][GLYPH_ID ROW COL], in byte
/// order (= item order; otb is the global byte-order compaction and items
/// tile the corpus). The engine emits one record per leader INCLUDING
/// newlines, blanks, and missing codepoints — parity means the same
/// stream, so nothing is dropped here; consumers filter.
///
/// Tier contract (the chain-check records diff below): gi/row/col exact,
/// height/advance bit-exact (their producing lanes are already
/// bit-fenced elsewhere in the check); X/Y/Z ride the position eps tiers.
/// The fold>0 X bit-tier stays in the byte-indexed lane diff — THIS diff
/// witnesses the GATHER, and the gather's own failure mode is a wrong
/// byte or order, which the ordinal/byte identity check catches exactly
/// (the reference's own ord_to_byte is the independent compaction).
#[cube(launch_unchecked)]
pub(super) fn emit_records(
    glyph_flags: &[u32],
    item_record_ordinals: &[u32],
    item_record_bounds: &[u32],
    item_record_bases: &[u32],
    layout_metrics: &[f32],
    line_columns: &[u32],
    advance_widths: &[f32],
    glyph_heights: &[f32],
    glyph_indices: &[u32],
    wire_records: &mut [u32],
    window_params: &[u32],
) {
    let rec_first = window_params[0usize] as usize;
    let record_index = ABSOLUTE_POS;
    let total_records = item_record_ordinals.len();
    let item_count = item_record_bounds.len() / 2;
    if record_index < total_records && item_count > 0 && flags_at(glyph_flags, record_index) & F_LEADER != 0 {
        let item_index = item_search(item_record_bounds, item_count, record_index);
        let global_ordinal = item_record_bases[item_index] + item_record_ordinals[record_index];
        if global_ordinal as usize >= rec_first && (global_ordinal as usize - rec_first) < wire_records.len() / 8 {
            let record_offset = (global_ordinal as usize - rec_first) * 8;
            wire_records[record_offset] = layout_metrics[record_index * LM_STRIDE + LM_X].to_bits();
            wire_records[record_offset + 1] = layout_metrics[record_index * LM_STRIDE + LM_Y].to_bits();
            wire_records[record_offset + 2] = layout_metrics[record_index * LM_STRIDE + LM_Z].to_bits();
            wire_records[record_offset + 3] = advance_widths[record_index].to_bits();
            wire_records[record_offset + 4] = glyph_heights[record_index].to_bits();
            wire_records[record_offset + 5] = glyph_indices[record_index];
            wire_records[record_offset + 6] = line_columns[record_index * LC_STRIDE + LC_ROW];
            wire_records[record_offset + 7] = line_columns[record_index * LC_STRIDE + LC_COL];
        }
    }
}

// ── rung 5b: the instance tail ───────────────────────────────────────────────
//
// The survivor pass and the pack kernel — the device replacement for
// `compact_records_into` (layout.rs). The survivor ordinals come from the
// SAME integer scan machinery the cluster counter uses (count_tile /
// count_spine on a plain byte flag — note 13's shape: a separate pass,
// never a second counter inside the proven monoid). The packer writes the
// 48 B GlyphInstance wire form and folds the extents with per-item atomics
// over `ordered_key` — min/max is order-free, so the reduction is
// deterministic however the atomics interleave, and its lanes are
// bit-identical to the host loop's by construction: `right = x + advance`
// is a bare add (nothing to contract), and the half-height lanes multiply
// by 0.5 — a power of two, exact — so even a forced fma contraction rounds
// identically to the host's two-step form.

/// Extent lanes per item in the `ext` buffer. Order matches the decode in
/// run_repo_chain's tail: page right/bottom/z_min/z_max over ALL records,
/// then ink min-xyz / max-xyz over survivors.
pub(super) const EXT_STRIDE: usize = 10;

/// The HOST twins of ordered_key / key_to_float — they seed the extent
/// lanes and decode their readback, so they must agree bit for bit with
/// the #[cube] pair (unit-tested together).
pub(super) fn ordered_key_host(v: f32) -> u32 {
    let bit_repr = v.to_bits();
    if (bit_repr & 0x8000_0000) != 0 {
        !bit_repr
    } else {
        bit_repr | 0x8000_0000
    }
}

pub(super) fn key_to_float_host(k: u32) -> f32 {
    let float_bits = if (k & 0x8000_0000) != 0 {
        k & 0x7FFF_FFFF
    } else {
        !k
    };
    f32::from_bits(float_bits)
}



/// The 32 B slot scatter — THE product tail (note 23, E2b). One thread
/// per byte, writing the 8-word slot at the global survivor ordinal
/// DIRECTLY at its final address in the buffer the renderer binds — no
/// window base, no rolling chunk, no hop, no copy. The dropped lanes
/// (row/col, flags, _pad) are proven dead readers by note 22's sweep
/// (the shader never reads them; pick/verbs ride the engine cache;
/// row/col stay fenced by the records tier). The tint stream rides
/// beside it: (glyph_id, color) per slot in slot order, the one
/// slot-derived readback — seg_tint's bit-exact fold input.
///
/// The scatter is pure streaming writes (slot + tint) — the extent fold
/// moved to `extent_fold` (2026-09-30): 960M per-leader global atomics onto
/// 52 KB of lanes cost 263ms of the scatter's 364ms at the flagship, while
/// the writes alone run at the copy floor. (E1's mask lesson stands: a
/// duplicate reducer is a MASK — exactly one folder per reduction, now
/// `extent_fold`.)
#[cube(launch_unchecked)]
pub(super) fn scatter_slots(
    glyph_flags: &[u32],
    item_record_ordinals: &[u32],
    item_record_bounds: &[u32],
    layout_metrics: &[f32],
    advance_widths: &[f32],
    glyph_heights: &[f32],
    glyph_indices: &[u32],
    per_record_semantic_colors: &[u32],
    item_paint: &[u32],
    survivor_tile_prefixes: &[u32],
    survivor_unit_prefixes: &[u32],
    instance_slots: &mut [u32],
    instance_tints: &mut [u32],
    #[comptime] units: usize,
    #[comptime] rake: usize,
) {
    let tile = CUBE_POS;
    let unit_pos = UNIT_POS as usize;
    let total_records = item_record_ordinals.len();
    let item_count = item_record_bounds.len() / 2;
    let range_start = tile * (units * rake) + unit_pos * rake;
    let range_end = if range_start + rake < total_records { range_start + rake } else { total_records };
    let mut survivor_ordinal = survivor_tile_prefixes[tile] + survivor_unit_prefixes[tile * units + unit_pos];
    if range_start < total_records && item_count > 0 {
        let mut item_index = item_search(item_record_bounds, item_count, range_start);
        let mut next_item_boundary = if item_index + 1 < item_count { item_record_bounds[(item_index + 1) * 2] as usize } else { total_records };
        let mut cur_col_base = item_paint[item_index * 4];
        let mut cur_is_pr = item_paint[item_index * 4 + 1];
        let mut cur_flat_color = item_paint[item_index * 4 + 2];
        let mut cur_group = item_paint[item_index * 4 + 3];

        let mut cur_word_idx = (range_start >> 2) + 1usize;
        let mut flag_word = 0u32;

        let mut record_index = range_start;
        while record_index < range_end {
            if next_item_boundary <= record_index {
                while next_item_boundary <= record_index {
                    item_index += 1;
                    next_item_boundary = if item_index + 1 < item_count { item_record_bounds[(item_index + 1) * 2] as usize } else { total_records };
                }
                cur_col_base = item_paint[item_index * 4];
                cur_is_pr = item_paint[item_index * 4 + 1];
                cur_flat_color = item_paint[item_index * 4 + 2];
                cur_group = item_paint[item_index * 4 + 3];
            }
            let word_idx = record_index >> 2;
            if word_idx != cur_word_idx {
                flag_word = glyph_flags[word_idx];
                cur_word_idx = word_idx;
            }
            let flag_byte = (flag_word >> (((record_index & 3) * 8) as u32)) & 0xFF;
            if (flag_byte & F_LEADER) != 0 && glyph_indices[record_index] != 0u32 {
                let slot_word_offset = survivor_ordinal as usize * 8;
                let color = if cur_is_pr != 0u32 {
                    per_record_semantic_colors[(cur_col_base + item_record_ordinals[record_index]) as usize]
                } else {
                    cur_flat_color
                };
                let tint_word_offset = survivor_ordinal as usize * 2;
                if tint_word_offset + 2 <= instance_tints.len() {
                    instance_tints[tint_word_offset] = glyph_indices[record_index];
                    instance_tints[tint_word_offset + 1] = color;
                }
                if slot_word_offset + 8 <= instance_slots.len() {
                    let metric_offset = record_index * LM_STRIDE;
                    let x = layout_metrics[metric_offset + LM_X];
                    let y = layout_metrics[metric_offset + LM_Y];
                    let z = layout_metrics[metric_offset + LM_Z];
                    let adv = advance_widths[record_index];
                    let height = glyph_heights[record_index];
                    instance_slots[slot_word_offset] = x.to_bits();
                    instance_slots[slot_word_offset + 1] = y.to_bits();
                    instance_slots[slot_word_offset + 2] = z.to_bits();
                    instance_slots[slot_word_offset + 3] = glyph_indices[record_index];
                    instance_slots[slot_word_offset + 4] = color;
                    instance_slots[slot_word_offset + 5] = cur_group;
                    instance_slots[slot_word_offset + 6] = adv.to_bits();
                    instance_slots[slot_word_offset + 7] = height.to_bits();
                }
                survivor_ordinal += 1u32;
            }
            record_index += 1usize;
        }
    }
}

/// THE SOLE EXTENT FOLDER (taken off the scatter, 2026-09-30): the scatter's
/// per-leader global atomics (~960M onto 52 KB of lanes) cost 263ms of its
/// 364ms at the flagship, while its writes alone run at the copy floor.
/// Min/max under the ordered key is EXACT and order-free, so any grouping
/// reproduces the bits — the fold runs at the tile_scan shape instead
/// (raked units, full occupancy): a thread accumulates its rake in
/// registers and flushes ONE atomic set per item-run (the item tracking
/// mirrors tile_scan's `nxt` walk). Page lanes over ALL records (the 0.0
/// seeds ride the local accumulators — folding the seed again is
/// idempotent), ink lanes over survivors (±inf), and the scatter's
/// arithmetic unchanged (a bare add, a power-of-two multiply —
/// contraction-neutral by construction).
#[cube(launch_unchecked)]
pub(super) fn extent_fold(
    glyph_flags: &[u32],
    layout_metrics: &[f32],
    advance_widths: &[f32],
    glyph_heights: &[f32],
    glyph_indices: &[u32],
    item_record_bounds: &[u32],
    item_extents_atomic: &mut [Atomic<u32>],
    #[comptime] units: usize,
    #[comptime] rake: usize,
) {
    let tile = CUBE_POS;
    let unit_pos = UNIT_POS as usize;
    let total_records = glyph_flags.len() * 4; // packed: words -> bytes
    let item_count = item_record_bounds.len() / 2;
    let tile_start = tile * (units * rake);

    let shared_item_extents = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS * EXT_STRIDE);
    let shared_item_flags = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS);
    let mut shared_item_base = Shared::<u32>::new();

    if unit_pos == 0 {
        let mut probe_item_index = 0usize;
        if item_count > 0 && total_records > 0 {
            let probe = if tile_start < total_records { tile_start } else { total_records - 1 };
            probe_item_index = item_search(item_record_bounds, item_count, probe);
        }
        *shared_item_base = probe_item_index as u32;
    }

    let zero_k = 0x8000_0000u32;
    let inf_k = 0xFF80_0000u32;
    let ninf_k = 0x007F_FFFFu32;

    let mut init_index = unit_pos;
    while init_index < RESOLVE_SLOTS {
        shared_item_flags[init_index].store(0u32);
        let e = init_index * EXT_STRIDE;
        shared_item_extents[e].store(zero_k);
        shared_item_extents[e + 1].store(zero_k);
        shared_item_extents[e + 2].store(zero_k);
        shared_item_extents[e + 3].store(zero_k);
        shared_item_extents[e + 4].store(inf_k);
        shared_item_extents[e + 5].store(inf_k);
        shared_item_extents[e + 6].store(ninf_k);
        shared_item_extents[e + 7].store(ninf_k);
        shared_item_extents[e + 8].store(inf_k);
        shared_item_extents[e + 9].store(ninf_k);
        init_index += units;
    }
    sync_cube();
    let tile_item_base = *shared_item_base as usize;

    let first_record_index = tile_start + unit_pos;
    if first_record_index < total_records && item_count > 0 {
        let mut item_index = item_search(item_record_bounds, item_count, first_record_index);
        let mut next_item_boundary = if item_index + 1 < item_count { item_record_bounds[(item_index + 1) * 2] as usize } else { total_records };
        let mut pg_rmax = f32::new(0.0f32);
        let mut pg_ymin = f32::new(0.0f32);
        let mut pg_zmin = f32::new(0.0f32);
        let mut pg_zmax = f32::new(0.0f32);
        let mut ink_xmin = f32::new(3.4028235e38f32);
        let mut ink_ymin = f32::new(3.4028235e38f32);
        let mut ink_rmax = f32::new(-3.4028235e38f32);
        let mut ink_ymax = f32::new(-3.4028235e38f32);
        let mut ink_zmin = f32::new(3.4028235e38f32);
        let mut ink_zmax = f32::new(-3.4028235e38f32);
        let mut any_leader = false;
        let mut any_survivor = false;

        let mut rake_step = 0usize;
        while rake_step < rake {
            let record_index = tile_start + rake_step * units + unit_pos;
            if record_index >= total_records {
                break;
            }
            if next_item_boundary <= record_index {
                if any_leader {
                    let slot = item_index - tile_item_base;
                    if slot < RESOLVE_SLOTS {
                        shared_item_flags[slot].fetch_or(if any_survivor { 3u32 } else { 1u32 });
                        let e = slot * EXT_STRIDE;
                        shared_item_extents[e].fetch_max(ordered_key(pg_rmax));
                        shared_item_extents[e + 1].fetch_min(ordered_key(pg_ymin));
                        shared_item_extents[e + 2].fetch_min(ordered_key(pg_zmin));
                        shared_item_extents[e + 3].fetch_max(ordered_key(pg_zmax));
                        if any_survivor {
                            shared_item_extents[e + 4].fetch_min(ordered_key(ink_xmin));
                            shared_item_extents[e + 5].fetch_min(ordered_key(ink_ymin));
                            shared_item_extents[e + 6].fetch_max(ordered_key(ink_rmax));
                            shared_item_extents[e + 7].fetch_max(ordered_key(ink_ymax));
                            shared_item_extents[e + 8].fetch_min(ordered_key(ink_zmin));
                            shared_item_extents[e + 9].fetch_max(ordered_key(ink_zmax));
                        }
                    } else {
                        let e = item_index * EXT_STRIDE;
                        item_extents_atomic[e].fetch_max(ordered_key(pg_rmax));
                        item_extents_atomic[e + 1].fetch_min(ordered_key(pg_ymin));
                        item_extents_atomic[e + 2].fetch_min(ordered_key(pg_zmin));
                        item_extents_atomic[e + 3].fetch_max(ordered_key(pg_zmax));
                        if any_survivor {
                            item_extents_atomic[e + 4].fetch_min(ordered_key(ink_xmin));
                            item_extents_atomic[e + 5].fetch_min(ordered_key(ink_ymin));
                            item_extents_atomic[e + 6].fetch_max(ordered_key(ink_rmax));
                            item_extents_atomic[e + 7].fetch_max(ordered_key(ink_ymax));
                            item_extents_atomic[e + 8].fetch_min(ordered_key(ink_zmin));
                            item_extents_atomic[e + 9].fetch_max(ordered_key(ink_zmax));
                        }
                    }
                    pg_rmax = f32::new(0.0f32);
                    pg_ymin = f32::new(0.0f32);
                    pg_zmin = f32::new(0.0f32);
                    pg_zmax = f32::new(0.0f32);
                    ink_xmin = f32::new(3.4028235e38f32);
                    ink_ymin = f32::new(3.4028235e38f32);
                    ink_rmax = f32::new(-3.4028235e38f32);
                    ink_ymax = f32::new(-3.4028235e38f32);
                    ink_zmin = f32::new(3.4028235e38f32);
                    ink_zmax = f32::new(-3.4028235e38f32);
                    any_leader = false;
                    any_survivor = false;
                }
                while next_item_boundary <= record_index {
                    item_index += 1;
                    next_item_boundary = if item_index + 1 < item_count { item_record_bounds[(item_index + 1) * 2] as usize } else { total_records };
                }
            }
            if flags_at(glyph_flags, record_index) & F_LEADER != 0 {
                any_leader = true;
                let x = layout_metrics[record_index * LM_STRIDE + LM_X];
                let y = layout_metrics[record_index * LM_STRIDE + LM_Y];
                let z = layout_metrics[record_index * LM_STRIDE + LM_Z];
                let right = x + advance_widths[record_index];
                if right > pg_rmax {
                    pg_rmax = right;
                }
                if y < pg_ymin {
                    pg_ymin = y;
                }
                if z < pg_zmin {
                    pg_zmin = z;
                }
                if z > pg_zmax {
                    pg_zmax = z;
                }
                if glyph_indices[record_index] != 0u32 {
                    any_survivor = true;
                    let half = glyph_heights[record_index] * 0.5f32;
                    let y_lo = y - half;
                    let y_hi = y + half;
                    if x < ink_xmin {
                        ink_xmin = x;
                    }
                    if y_lo < ink_ymin {
                        ink_ymin = y_lo;
                    }
                    if right > ink_rmax {
                        ink_rmax = right;
                    }
                    if y_hi > ink_ymax {
                        ink_ymax = y_hi;
                    }
                    if z < ink_zmin {
                        ink_zmin = z;
                    }
                    if z > ink_zmax {
                        ink_zmax = z;
                    }
                }
            }
            rake_step += 1;
        }

        if any_leader {
            let slot = item_index - tile_item_base;
            if slot < RESOLVE_SLOTS {
                shared_item_flags[slot].fetch_or(if any_survivor { 3u32 } else { 1u32 });
                let e = slot * EXT_STRIDE;
                shared_item_extents[e].fetch_max(ordered_key(pg_rmax));
                shared_item_extents[e + 1].fetch_min(ordered_key(pg_ymin));
                shared_item_extents[e + 2].fetch_min(ordered_key(pg_zmin));
                shared_item_extents[e + 3].fetch_max(ordered_key(pg_zmax));
                if any_survivor {
                    shared_item_extents[e + 4].fetch_min(ordered_key(ink_xmin));
                    shared_item_extents[e + 5].fetch_min(ordered_key(ink_ymin));
                    shared_item_extents[e + 6].fetch_max(ordered_key(ink_rmax));
                    shared_item_extents[e + 7].fetch_max(ordered_key(ink_ymax));
                    shared_item_extents[e + 8].fetch_min(ordered_key(ink_zmin));
                    shared_item_extents[e + 9].fetch_max(ordered_key(ink_zmax));
                }
            } else {
                let e = item_index * EXT_STRIDE;
                item_extents_atomic[e].fetch_max(ordered_key(pg_rmax));
                item_extents_atomic[e + 1].fetch_min(ordered_key(pg_ymin));
                item_extents_atomic[e + 2].fetch_min(ordered_key(pg_zmin));
                item_extents_atomic[e + 3].fetch_max(ordered_key(pg_zmax));
                if any_survivor {
                    item_extents_atomic[e + 4].fetch_min(ordered_key(ink_xmin));
                    item_extents_atomic[e + 5].fetch_min(ordered_key(ink_ymin));
                    item_extents_atomic[e + 6].fetch_max(ordered_key(ink_rmax));
                    item_extents_atomic[e + 7].fetch_max(ordered_key(ink_ymax));
                    item_extents_atomic[e + 8].fetch_min(ordered_key(ink_zmin));
                    item_extents_atomic[e + 9].fetch_max(ordered_key(ink_zmax));
                }
            }
        }
    }

    sync_cube();
    if unit_pos < RESOLVE_SLOTS {
        let item_index = tile_item_base + unit_pos;
        if item_index < item_count {
            let flags = shared_item_flags[unit_pos].load();
            if (flags & 1u32) != 0 {
                let se = unit_pos * EXT_STRIDE;
                let e = item_index * EXT_STRIDE;
                item_extents_atomic[e].fetch_max(shared_item_extents[se].load());
                item_extents_atomic[e + 1].fetch_min(shared_item_extents[se + 1].load());
                item_extents_atomic[e + 2].fetch_min(shared_item_extents[se + 2].load());
                item_extents_atomic[e + 3].fetch_max(shared_item_extents[se + 3].load());
                if (flags & 2u32) != 0 {
                    item_extents_atomic[e + 4].fetch_min(shared_item_extents[se + 4].load());
                    item_extents_atomic[e + 5].fetch_min(shared_item_extents[se + 5].load());
                    item_extents_atomic[e + 6].fetch_max(shared_item_extents[se + 6].load());
                    item_extents_atomic[e + 7].fetch_max(shared_item_extents[se + 7].load());
                    item_extents_atomic[e + 8].fetch_min(shared_item_extents[se + 8].load());
                    item_extents_atomic[e + 9].fetch_max(shared_item_extents[se + 9].load());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// The extent lanes' key encoding — the HOST half that seeds the lanes
    /// and decodes their readback. Roundtrip (the seeds decode back to the
    /// floats that made them) and MONOTONICITY (the per-item
    /// fetch_max/fetch_min reductions are only correct if the key preserves
    /// float order — that property, not the bit pattern, is what makes the
    /// order-free atomics deterministic). The #[cube] twin is the same bit
    /// logic; the fork gate's instance and placement tiers fence the
    /// device reduction end to end.
    #[test]
    fn ordered_key_host_roundtrip_and_monotonic() {
        let vals = [
            0.0f32,
            -0.0,
            1.0,
            -1.0,
            0.5,
            -0.5,
            3.178_448_2e0,
            -1.337_5e2,
            f32::MIN_POSITIVE,
            -f32::MIN_POSITIVE,
            f32::MAX,
            f32::MIN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ];
        for &v in &vals {
            assert_eq!(super::key_to_float_host(super::ordered_key_host(v)), v);
        }
        let mut sorted = vals;
        sorted.sort_by(f32::total_cmp);
        for w in sorted.windows(2) {
            assert!(super::ordered_key_host(w[0]) <= super::ordered_key_host(w[1]));
        }
    }
}
