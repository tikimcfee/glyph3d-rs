use cubecl::prelude::*;

use super::decode::byte_at;
use super::monoid::item_search;
use super::{F_CLUSTER_HEAD, F_CLUSTER_TRAILER, F_LEADER};

// ── the cluster pass, phase 3b: probe (parallel) + chain (per item) ───────────
//
// Re-derived scan-shaped per the design session: the PROBE is pure per byte
// (its outcome depends only on the byte's own decode, forward bytes, the
// tables, and the item end — verified against resolve_clusters), so it runs
// one thread per WORD like decode. The CONSUMPTION is serial per item with
// a single integer of state (the resume pointer) — the brief's counterexample
// killed the naive max-scan: with W=[0,10), Y=[5,15), X=[12,14) the prefix-
// max sees Y's 15 and suppresses X, which greedy commits. v1 runs the chain
// one thread per ITEM (the product corpus is 1,306 items — item-parallel);
// the chunked function-composition form (the Mojo zone tables) is the
// follow-up only if a dense-single-item measurement demands it.
//
// Lookup is DESCENDING-LENGTH BINARY SEARCH over the sorted sequence
// section (the section order is asserted at bake; the longest exact prefix
// is unique) — semantics-neutral vs the Mojo st_probe hash by that same
// argument, and it reuses the item_search pattern already proven here. The
// comparator re-walks the probe's effective codepoints from the head byte
// instead of storing a key array: the walk is deterministic, so no local
// storage exists to spill.
//
// GAP-BYTE GUARD: the CPU walks only [start, stop); item_search attributes
// bytes to the largest start <= id WITHOUT an end check, so every per-byte
// test here carries its own id < stop guard — no fixture pins this (items
// tile every fixture's blob), the landmine list does.

/// sequence_length at i: the lenient classifier over the packed corpus.
#[cube]
pub(super) fn seq_len_at(bytes: &[u32], i: usize, n: usize) -> u32 {
    let lead_byte = byte_at(bytes, i, n);
    if lead_byte & 0x80u32 == 0u32 {
        1u32
    } else if lead_byte & 0xE0u32 == 0xC0u32 {
        2u32
    } else if lead_byte & 0xF0u32 == 0xE0u32 {
        3u32
    } else if lead_byte & 0xF8u32 == 0xF0u32 {
        4u32
    } else {
        0u32
    }
}

/// decode_codepoint_at at i for a known length.
#[cube]
pub(super) fn cp_at(bytes: &[u32], i: usize, len: u32, n: usize) -> u32 {
    let lead_byte = byte_at(bytes, i, n);
    let trail_byte1 = byte_at(bytes, i + 1, n);
    let trail_byte2 = byte_at(bytes, i + 2, n);
    let trail_byte3 = byte_at(bytes, i + 3, n);
    if len == 1u32 {
        lead_byte
    } else if len == 2u32 {
        ((lead_byte & 0x1Fu32) << 6u32) | (trail_byte1 & 0x3Fu32)
    } else if len == 3u32 {
        ((lead_byte & 0x0Fu32) << 12u32) | ((trail_byte1 & 0x3Fu32) << 6u32) | (trail_byte2 & 0x3Fu32)
    } else {
        ((lead_byte & 0x07u32) << 18u32) | ((trail_byte1 & 0x3Fu32) << 12u32) | ((trail_byte2 & 0x3Fu32) << 6u32) | (trail_byte3 & 0x3Fu32)
    }
}

/// is_static_zero_cp: ZWJ, the variation selectors, the tag characters.
#[cube]
// clippy:manual-range-contains allowed here — the cube macro has no
// RangeInclusive::contains expansion, and this is the spelled-out form it
// takes on device.
#[allow(clippy::manual_range_contains)]
pub(super) fn is_static_zero(cp: u32) -> u32 {
    if cp == 0x200Du32 || (cp >= 0xFE00u32 && cp <= 0xFE0Fu32) || (cp >= 0xE0020u32 && cp <= 0xE007Fu32) {
        1u32
    } else {
        0u32
    }
}


/// The cluster PROBE: thread per word. Static-zero bytes of cluster items
/// are marked here (unconditionally — match-independent, fold.rs:543-549);
/// candidates build their EFFECTIVE key into a per-thread LOCAL array (the
/// head's own codepoint first, FE0F skipped but riding, newline/VS15/
/// continuation/item-end breaking) and run a descending-length binary
/// search over the sorted sequence section — the longest exact prefix is
/// unique, so this is answer-identical to the CPU's linear scan and the
/// Mojo's hash probe alike. The span end re-walks counting CONSUMED key
/// elements, so trailing FE0Fs past the last consumer stay outside.
///
/// The key scratch is a LOCAL `Array`, not `Shared`: each unit only ever
/// touches its own row, so workgroup memory was never doing inter-thread
/// work — but its allocation (WGSL zero-initializes `var<workgroup>`) cost
/// every cube 8KB of memset whether or not any candidate ran. Measured
/// 2026-09-27: 63.9ms of probe time on a 24MB text corpus whose bytes all
/// bitmap-reject — 2.7µs/cube of pure scratch init.
///
/// All of this lives INLINE in the kernel because the walk/search shapes
/// only compile in kernel context — loops in HELPERS break the macro's
/// assign typing (recorded landmine; the deleted helper drafts are in the
/// commit history).
#[cube(launch_unchecked)]
pub(super) fn cluster_probe(
    bytes: &[u32],
    bitmap: &[u32],
    secondary_offsets: &[u32],
    secondary_values: &[u32],
    sequence_table: &[u32],
    item_record_bounds: &[u32],
    item_cluster_enabled: &[u32],
    glyph_flags: &mut [u32],
    
    candidate_slots: &mut [u32],
    candidate_end_positions: &mut [u32],
    #[comptime] seq_max: u32,
) {
    let word_index = ABSOLUTE_POS;
    let total_bytes = bytes.len() * 4;
    let item_count = item_record_bounds.len() / 2;
    if word_index < glyph_flags.len() {
        let mut packed_word = glyph_flags[word_index];
        let mut lane = 0usize;
        while lane < 4 {
            let byte_index = word_index * 4 + lane;
            if byte_index < total_bytes {
                let len = seq_len_at(bytes, byte_index, total_bytes);
                if len > 0u32 {
                    let mut item_start_byte = 0usize;
                    let mut item_end_byte = 0usize;
                    let mut cluster_enabled = false;
                    if item_count > 0 {
                        let item_index = item_search(item_record_bounds, item_count, byte_index);
                        item_start_byte = item_record_bounds[item_index * 2] as usize;
                        item_end_byte = item_record_bounds[item_index * 2 + 1] as usize;
                        cluster_enabled = item_cluster_enabled[item_index] != 0;
                    }
                    // The gap-byte guard: only bytes INSIDE the item range.
                    // BOTH edges are load-bearing — item_search clamps to
                    // item 0 for bytes BEFORE the first item (a leading gap
                    // would otherwise take item 0's static-zero marking,
                    // which the CPU never applies there).
                    if cluster_enabled && byte_index >= item_start_byte && byte_index < item_end_byte {
                        let codepoint = cp_at(bytes, byte_index, len, total_bytes);
                        if is_static_zero(codepoint) != 0u32 {
                            // fold.rs:543-548 zeroes ALL THREE static lanes
                            // for a static-zero byte in a cluster item — gi
                            // included. The probe wrote two of them until
                            // the repo parity driver caught the third.
                            packed_word |= F_CLUSTER_TRAILER << ((lane as u32) * 8u32);
                        } else {
                            // cp above 0x10FFFF is malformed decode — the
                            // bitmap covers real codepoints only; reject
                            // here rather than lean on the backend's
                            // OOB-read-is-zero (decode guards this class
                            // itself).
                            let mut bit = 0u32;
                            if codepoint <= 0x10FFFFu32 {
                                bit = (bitmap[(codepoint >> 5u32) as usize] >> (codepoint & 0x1Fu32)) & 1u32;
                            }
                            if bit != 0u32 {
                                // Pair filter: every reachable table entry
                                // has effective length >= 2 (the matcher's
                                // own guard), so a candidate whose SECOND
                                // effective element cannot follow its first
                                // in ANY entry can never match. Walk just
                                // far enough for that second element —
                                // skipping FE0F riders, stopping at the same
                                // breakers as the key walk — then one small
                                // binary search over this first's seconds.
                                // On source text this is where the keycap
                                // digits/#/* die: a few loads instead of a
                                // full key walk plus up to seven table
                                // searches (measured 42.6ms of the 24MB
                                // text probe before it).
                                let mut search_byte_pos = byte_index + len as usize;
                                let mut second_codepoint = 0u32;
                                let mut is_hunting = 1u32;
                                while is_hunting == 1u32 && search_byte_pos < item_end_byte {
                                    let seq2_len = seq_len_at(bytes, search_byte_pos, total_bytes);
                                    let seq2_cp = cp_at(bytes, search_byte_pos, seq2_len, total_bytes);
                                    let seq2_dead = if seq2_len == 0u32 || seq2_cp == 0x0Au32 || seq2_cp == 0xFE0Eu32 {
                                        1u32
                                    } else {
                                        0u32
                                    };
                                    if seq2_dead == 1u32 {
                                        is_hunting = 0u32;
                                    }
                                    if seq2_dead == 0u32 {
                                        if seq2_cp != 0xFE0Fu32 {
                                            second_codepoint = seq2_cp;
                                            is_hunting = 0u32;
                                        }
                                        search_byte_pos += seq2_len as usize;
                                    }
                                }
                                let mut is_pair_alive = 0u32;
                                if second_codepoint != 0u32 {
                                    let sec_start = secondary_offsets[codepoint as usize];
                                    let sec_end = secondary_offsets[codepoint as usize + 1usize];
                                    let mut bin_lo = sec_start;
                                    let mut bin_hi = sec_end;
                                    while bin_lo < bin_hi {
                                        let bin_mid = (bin_lo + bin_hi) / 2u32;
                                        if secondary_values[bin_mid as usize] < second_codepoint {
                                            bin_lo = bin_mid + 1u32;
                                        }
                                        if secondary_values[bin_mid as usize] >= second_codepoint {
                                            bin_hi = bin_mid;
                                        }
                                    }
                                    if bin_lo < sec_end && secondary_values[bin_lo as usize] == second_codepoint {
                                        is_pair_alive = 1u32;
                                    }
                                }
                                if is_pair_alive == 1u32 {
                                    // Key build: the head's own cp is element 0.
                                    // The scratch is LOCAL to this thread —
                                    // declared here so only candidates pay for it.
                                    let mut sequence_key = Array::<u32>::new(seq_max as usize);
                                    let mut key_length = 0u32;
                                    let mut scan_byte_pos = byte_index;
                                    let mut is_alive = 1u32;
                                    while is_alive == 1u32 && scan_byte_pos < item_end_byte && key_length < seq_max {
                                        let scan_len = seq_len_at(bytes, scan_byte_pos, total_bytes);
                                        let scan_cp = cp_at(bytes, scan_byte_pos, scan_len, total_bytes);
                                        let is_dead = if scan_len == 0u32 || scan_cp == 0x0Au32 || scan_cp == 0xFE0Eu32 {
                                            1u32
                                        } else {
                                            0u32
                                        };
                                        if is_dead == 1u32 {
                                            is_alive = 0u32;
                                        }
                                        if is_dead == 0u32 {
                                            if scan_cp != 0xFE0Fu32 {
                                                sequence_key[key_length as usize] = scan_cp;
                                                key_length += 1u32;
                                            }
                                            scan_byte_pos += scan_len as usize;
                                        }
                                    }
                                    // Descending-length binary search.
                                    let table_stride = 2u32 + seq_max;
                                    let total_sequences = (sequence_table.len() / table_stride as usize) as u32;
                                    let mut current_match_len = if key_length < seq_max { key_length } else { seq_max };
                                    let mut matched_slot = 0u32;
                                    let mut matched_len = 0u32;
                                    while current_match_len >= 2u32 && matched_slot == 0u32 {
                                        let mut seq_search_lo = 0u32;
                                        let mut seq_search_hi = total_sequences;
                                        while seq_search_lo < seq_search_hi {
                                            let seq_search_mid = (seq_search_lo + seq_search_hi) / 2u32;
                                            let entry_offset = seq_search_mid as usize * table_stride as usize;
                                            let entry_sequence_len = sequence_table[entry_offset + 1];
                                            let probe_compare_len = if entry_sequence_len < current_match_len { entry_sequence_len } else { current_match_len };
                                            let mut comparison_order = 0i32;
                                            let mut compare_step = 0u32;
                                            while compare_step < probe_compare_len && comparison_order == 0i32 {
                                                let target_codepoint = sequence_table[entry_offset + 2 + compare_step as usize];
                                                let probe_codepoint = sequence_key[compare_step as usize];
                                                if probe_codepoint < target_codepoint {
                                                    comparison_order = -1i32;
                                                }
                                                if comparison_order == 0i32 && probe_codepoint > target_codepoint {
                                                    comparison_order = 1i32;
                                                }
                                                compare_step += 1u32;
                                            }
                                            if comparison_order == 0i32 {
                                                // Shorter-prefix-first order: with equal
                                                // elements the SHORTER sequence sorts first,
                                                // so a longer entry is GREATER than the probe.
                                                if entry_sequence_len < current_match_len {
                                                    comparison_order = 1i32;
                                                }
                                                if entry_sequence_len > current_match_len {
                                                    comparison_order = -1i32;
                                                }
                                            }
                                            if comparison_order < 0i32 {
                                                seq_search_hi = seq_search_mid;
                                            }
                                            if comparison_order > 0i32 {
                                                seq_search_lo = seq_search_mid + 1u32;
                                            }
                                            if comparison_order == 0i32 {
                                                matched_slot = sequence_table[entry_offset];
                                                matched_len = current_match_len;
                                                seq_search_lo = seq_search_hi;
                                            }
                                        }
                                        current_match_len -= 1u32;
                                    }
                                    if matched_slot != 0u32 {
                                        // Span end: re-walk, counting consumers.
                                        let mut collected_codepoint_count = 0u32;
                                        let mut end_scan_byte_pos = byte_index;
                                        let mut matched_end_byte = byte_index as u32;
                                        let mut is_end_scan_alive = 1u32;
                                        while is_end_scan_alive == 1u32 && end_scan_byte_pos < item_end_byte {
                                            let end_scan_len = seq_len_at(bytes, end_scan_byte_pos, total_bytes);
                                            let end_scan_cp = cp_at(bytes, end_scan_byte_pos, end_scan_len, total_bytes);
                                            let is_end_scan_dead = if end_scan_len == 0u32 || end_scan_cp == 0x0Au32 || end_scan_cp == 0xFE0Eu32 {
                                                1u32
                                            } else {
                                                0u32
                                            };
                                            if is_end_scan_dead == 1u32 {
                                                is_end_scan_alive = 0u32;
                                            }
                                            if is_end_scan_dead == 0u32 {
                                                if end_scan_cp != 0xFE0Fu32 {
                                                    collected_codepoint_count += 1u32;
                                                    if collected_codepoint_count == matched_len {
                                                        matched_end_byte = (end_scan_byte_pos + end_scan_len as usize) as u32;
                                                    }
                                                }
                                                end_scan_byte_pos += end_scan_len as usize;
                                            }
                                        }
                                        candidate_slots[byte_index] = matched_slot;
                                        candidate_end_positions[byte_index] = matched_end_byte;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            lane += 1usize;
        }
        glyph_flags[word_index] = packed_word;
    }
}

/// The cluster CHAIN, scan-shaped: LIST RANKING over the candidate jump
/// graph. The serial walk's state is MEMORYLESS — the entire state is the
/// current position — so its committed set is exactly greedy-by-start
/// interval scheduling: `c1` = first candidate at/after the item start,
/// `c_{k+1}` = first candidate at/after `cend[c_k]` (plain stepping visits
/// every codepoint in order, so "first candidate ≥ x" is well-defined).
/// That makes the commits the ORBIT of each item's first candidate under
/// `jump`, and orbits in a functional graph are list ranking — pointer
/// doubling, the textbook primitive. This replaced thread-per-item
/// cluster_chain (measured 2026-09-27: 5.65s on 24MB text — one GPU thread
/// stepping every byte; 1.24s on 7MB emoji) with graph work proportional to
/// MATCHED candidates, which is ~0 on text and ~1/2.5 of codepoints on the
/// emoji corpus. A chunk+boundary-fixup form was considered and rejected:
/// on dense corpora the true and assumed walks stay permanently out of
/// phase (both commit at different phases), so the fixup degrades to the
/// serial walk.
///
/// The stages: compact (count-scan + scatter → sorted head positions hp),
/// jump_build (`hp[cend]` lower-bound → parent forest, terminal self-loop,
/// clamped to the candidate's ITEM so a span ending at an item edge can
/// never jump into the next item), K = ceil(log2(C+1)) rank_steps
/// (L_{k+1} = L_k∘L_k with depth sums D; level tables stored flat — the
/// orbit test needs arbitrary lifts), and cluster_mark: candidate i is
/// committed iff lifting its item's root by exactly `T[root]−T[i]` levels
/// lands on i. Merging branches (suppressed candidates can share a jump
/// target) do not fool that test: the lift follows the root's UNIQUE
/// chain, and equality against i is index-exact.
///
/// The marking body is the old chain kernel's, unchanged: head advance,
/// trailer zeroing gated on the leader bit, packed-flag fetch_or (a span
/// can straddle threads), and the trailer walk CLAMPED to the item end —
/// cend can overrun it by up to one codepoint, and the serial side only
/// ever marks members strictly inside (the Mojo chain's ownership rule).
///
/// Scope notes, deliberately recorded: the device chain writes sm/fl
/// only — the serial `gi` lane (slot / zeroed trailers) has NO device
/// writer yet, and the check's diff covers flags+advance; the renderer's
/// consumption of this path (phase 4) must grow one or the diff must
/// gain the lane. Level tables price at K·(C+1)·4B — ~40MB at the 7MB
/// emoji corpus (C ≈ 600K), ~126MB at a hypothetical 24MB emoji-dense
/// worst case; `hp`/`cslot`/`cend` add 4B/byte beside them. Never size
/// `lvl` worst-case at C=n (2.4GB) — read C once per corpus, as both
/// drivers here do.
///
/// Two instrument-era facts this build paid for, kept because they will
/// bite again: (1) `rank_step`'s step/stride MUST be `#[comptime]` — as
/// runtime u32 scalars after five same-typed slice params, the macro's
/// launch misbound the buffer args outright (sentinel-verified: writes
/// landed in the wrong buffers, values swapped across statements);
/// comptime specialization fixed it untouched otherwise. Comptime tuning
/// scalars are house style for a reason. (2) The depth ping-pong must
/// NEVER write the d0 seed buffer — a naive two-buffer alternation
/// writes round 1's depths into d0, and every replay (the bench's sample
/// loop) seeds itself with the previous run's depths. Single-run drivers
/// pass with the bug; only repeat sampling exposes it.
#[cube(launch_unchecked)]
pub(super) fn count_tile(
    candidate_slots: &[u32],
    tile_counts: &mut [u32],
    unit_prefixes: &mut [u32],
    #[comptime] threads_per_cube: usize,
    #[comptime] bytes_per_thread: usize,
    #[comptime] log: usize,
) {
    let tile = CUBE_POS;
    let unit_pos = UNIT_POS as usize;
    let total_slots = candidate_slots.len();
    let range_start = tile * (threads_per_cube * bytes_per_thread) + unit_pos * bytes_per_thread;
    let range_end = if range_start + bytes_per_thread < total_slots { range_start + bytes_per_thread } else { total_slots };
    let mut candidate_count = 0u32;
    if range_start < total_slots {
        let mut slot_index = range_start;
        while slot_index < range_end {
            if candidate_slots[slot_index] != 0u32 {
                candidate_count += 1u32;
            }
            slot_index += 1usize;
        }
    }
    // The additive little sibling of tile_scan's monoid Blelloch. The load
    // phase writes EVERY shared slot before any read — naga only inserts
    // workgroup zero-init when initialization-before-read is unprovable,
    // and conditional writes (the probe's key scratch, formerly) force it.
    let mut shared_counts = Shared::<[u32]>::new_slice(threads_per_cube);
    shared_counts[unit_pos] = candidate_count;
    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = 1usize << d;
        if (unit_pos + 1) & (2 * s - 1) == 0 {
            shared_counts[unit_pos] += shared_counts[unit_pos - s];
        }
    }
    sync_cube();
    if unit_pos == threads_per_cube - 1 {
        tile_counts[tile] = shared_counts[unit_pos];
        shared_counts[unit_pos] = 0u32;
    }
    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = threads_per_cube >> (d + 1);
        if (unit_pos + 1) & (2 * s - 1) == 0 {
            let t = shared_counts[unit_pos];
            shared_counts[unit_pos] += shared_counts[unit_pos - s];
            shared_counts[unit_pos - s] = t;
        }
    }
    sync_cube();
    unit_prefixes[tile * threads_per_cube + unit_pos] = shared_counts[unit_pos];
}

/// The compaction spine: one cube Blelloch-scans the tile totals into
/// exclusive tile prefixes, chasing like spine_scan (contiguous blocks keep
/// the order for the chase writes). The unit whose block owns the LAST tile
/// publishes the grand total C — the candidate count the graph stages and
/// the host both key on (the host reads it once, in setup, to size the
/// level tables; the timed loop re-derives it on device).
#[cube(launch_unchecked)]
pub(super) fn count_spine(
    tile_counts: &[u32],
    tile_spine_counts: &mut [u32],
    grand_total: &mut [u32],
    #[comptime] threads_per_cube: usize,
    #[comptime] log: usize,
) {
    let unit_pos = UNIT_POS as usize;
    let n_tiles = tile_counts.len();
    let per = n_tiles.div_ceil(threads_per_cube);
    let first = unit_pos * per;
    let last = if first + per < n_tiles { first + per } else { n_tiles };
    let mut accumulated_count = 0u32;
    if first < n_tiles {
        let mut t = first;
        while t < last {
            accumulated_count += tile_counts[t];
            t += 1usize;
        }
    }
    let mut shared_counts = Shared::<[u32]>::new_slice(threads_per_cube);
    shared_counts[unit_pos] = accumulated_count;
    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = 1usize << d;
        if (unit_pos + 1) & (2 * s - 1) == 0 {
            shared_counts[unit_pos] += shared_counts[unit_pos - s];
        }
    }
    sync_cube();
    if unit_pos == threads_per_cube - 1 {
        shared_counts[unit_pos] = 0u32;
    }
    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = threads_per_cube >> (d + 1);
        if (unit_pos + 1) & (2 * s - 1) == 0 {
            let t = shared_counts[unit_pos];
            shared_counts[unit_pos] += shared_counts[unit_pos - s];
            shared_counts[unit_pos - s] = t;
        }
    }
    sync_cube();
    let mut prefix_count = shared_counts[unit_pos];
    if first < n_tiles {
        for t in first..last {
            tile_spine_counts[t] = prefix_count;
            prefix_count += tile_counts[t];
        }
        if last == n_tiles {
            grand_total[0] = prefix_count;
        }
    }
}

/// Scatter: unit re-walks its rake seeded with (tile prefix + unit prefix)
/// and appends candidate head positions — compaction into the sorted hp
/// array the binary searches ride on.
#[cube(launch_unchecked)]
pub(super) fn cand_scatter(
    candidate_slots: &[u32],
    tile_spine_counts: &[u32],
    unit_prefixes: &[u32],
    candidate_head_positions: &mut [u32],
    #[comptime] threads_per_cube: usize,
    #[comptime] bytes_per_thread: usize,
) {
    let tile = CUBE_POS;
    let unit_pos = UNIT_POS as usize;
    let total_slots = candidate_slots.len();
    let range_start = tile * (threads_per_cube * bytes_per_thread) + unit_pos * bytes_per_thread;
    let range_end = if range_start + bytes_per_thread < total_slots { range_start + bytes_per_thread } else { total_slots };
    let mut scatter_index = tile_spine_counts[tile] + unit_prefixes[tile * threads_per_cube + unit_pos];
    if range_start < total_slots {
        let mut slot_index = range_start;
        while slot_index < range_end {
            if candidate_slots[slot_index] != 0u32 {
                if (scatter_index as usize) < candidate_head_positions.len() {
                    candidate_head_positions[scatter_index as usize] = slot_index as u32;
                }
                scatter_index += 1u32;
            }
            slot_index += 1usize;
        }
    }
}

/// In-place comb sort of candidate positions in `candidate_head_positions` and associated `candidate_slots` / `candidate_end_positions`.
/// Typically ≤200 candidates in real corpora, executing on thread 0 in <1 microsecond.
#[allow(clippy::manual_swap)]
#[cube(launch_unchecked)]
pub(super) fn cand_sort(
    candidate_head_positions: &mut [u32],
    candidate_slots: &mut [u32],
    candidate_end_positions: &mut [u32],
    candidate_total_atomic: &mut [Atomic<u32>],
    #[comptime] candidate_capacity: usize,
) {
    if ABSOLUTE_POS == 0 {
        let raw_c = candidate_total_atomic[0].load();
        let candidate_count = if (raw_c as usize) > candidate_capacity { candidate_capacity } else { raw_c as usize };
        if (raw_c as usize) > candidate_capacity {
            candidate_total_atomic[0].store(candidate_capacity as u32);
        }
        if candidate_count > 1 {
            let mut gap = candidate_count;
            let mut swapped = true;
            while gap > 1 || swapped {
                gap = (gap * 10) / 13;
                if gap < 1 {
                    gap = 1;
                }
                swapped = false;
                let mut i = 0usize;
                while i + gap < candidate_count {
                    let j = i + gap;
                    if candidate_head_positions[i] > candidate_head_positions[j] {
                        let temp_head_pos = candidate_head_positions[i];
                        candidate_head_positions[i] = candidate_head_positions[j];
                        candidate_head_positions[j] = temp_head_pos;

                        let temp_slot = candidate_slots[i];
                        candidate_slots[i] = candidate_slots[j];
                        candidate_slots[j] = temp_slot;

                        let temp_end_pos = candidate_end_positions[i];
                        candidate_end_positions[i] = candidate_end_positions[j];
                        candidate_end_positions[j] = temp_end_pos;

                        swapped = true;
                    }
                    i += 1usize;
                }
            }
        }
    }
}

/// The jump graph: `parent[i]` = first candidate at/after `cend[i]`,
/// CLAMPED to `hp[i]`'s item (a span ending exactly at an item edge must
/// never jump into the next item — the serial walk restarts there). Index C
/// is the terminal: self-loop, written by thread C itself.
#[cube(launch_unchecked)]
pub(super) fn jump_build(
    candidate_head_positions: &[u32],
    candidate_end_positions: &[u32],
    item_record_bounds: &[u32],
    candidate_count_buffer: &[u32],
    parent_pointers: &mut [u32],
    depth_step_zero: &mut [u32],
) {
    let candidate_index = ABSOLUTE_POS;
    let candidate_count = candidate_count_buffer[0] as usize;
    if candidate_index < parent_pointers.len() {
        if candidate_index == candidate_count {
            parent_pointers[candidate_index] = candidate_index as u32;
            depth_step_zero[candidate_index] = 0u32;
        }
        if candidate_index < candidate_count {
            depth_step_zero[candidate_index] = 1u32;
            let head_pos = candidate_head_positions[candidate_index] as usize;
            let end_pos = candidate_end_positions[candidate_index] as usize;
            let item_count = item_record_bounds.len() / 2;
            let item_end_boundary = if item_count > 0 {
                let item_index = item_search(item_record_bounds, item_count, head_pos);
                item_record_bounds[item_index * 2 + 1] as usize
            } else {
                end_pos
            };
            // Lower bound over candidate_head_positions (guarded-if form — the landmine-safe binary
            // search shape the probe uses).
            let mut search_lo = 0u32;
            let mut search_hi = candidate_count as u32;
            while search_lo < search_hi {
                let search_mid = (search_lo + search_hi) / 2u32;
                if (candidate_head_positions[search_mid as usize] as usize) < end_pos {
                    search_lo = search_mid + 1u32;
                }
                if (candidate_head_positions[search_mid as usize] as usize) >= end_pos {
                    search_hi = search_mid;
                }
            }
            let target_candidate_index = search_lo as usize;
            if target_candidate_index < candidate_count && (candidate_head_positions[target_candidate_index] as usize) < item_end_boundary {
                parent_pointers[candidate_index] = target_candidate_index as u32;
            } else {
                parent_pointers[candidate_index] = candidate_count as u32;
            }
        }
        if candidate_index > candidate_count {
            parent_pointers[candidate_index] = candidate_count as u32;
            depth_step_zero[candidate_index] = 0u32;
        }
    }
}

/// One pointer-doubling round: L_{k+1} = L_k∘L_k with depth sums
/// D_{k+1} = D_k + D_k∘L_k (terminal carries 0, so sums saturate at the
/// true chain length). Also archives level k's parent table into the flat
/// lvl store — the orbit test lifts by ARBITRARY distances and needs every
/// level, not just the saturated end state.
#[cube(launch_unchecked)]
pub(super) fn rank_step(
    current_parents: &[u32],
    current_depths: &[u32],
    next_parents: &mut [u32],
    next_depths: &mut [u32],
    level_tables: &mut [u32],
    #[comptime] step: usize,
    #[comptime] stride: usize,
) {
    let candidate_index = ABSOLUTE_POS;
    if candidate_index < current_parents.len() {
        let parent_index = current_parents[candidate_index] as usize;
        next_parents[candidate_index] = current_parents[parent_index];
        next_depths[candidate_index] = current_depths[candidate_index] + current_depths[parent_index];
        level_tables[step * stride + candidate_index] = current_parents[candidate_index];
    }
}

/// Per item (cluster items only): the orbit ROOT — first candidate at/after
/// the item start, or C when the item carries none.
#[cube(launch_unchecked)]
pub(super) fn item_roots(
    candidate_head_positions: &[u32],
    candidate_count_buffer: &[u32],
    item_record_bounds: &[u32],
    item_cluster_enabled: &[u32],
    item_root_candidates: &mut [u32],
) {
    let item_index = ABSOLUTE_POS;
    let item_count = item_record_bounds.len() / 2;
    let candidate_count = candidate_count_buffer[0];
    if item_index < item_count {
        if item_cluster_enabled[item_index] != 0u32 {
            let item_start_byte = item_record_bounds[item_index * 2] as usize;
            let mut search_lo = 0u32;
            let mut search_hi = candidate_count;
            while search_lo < search_hi {
                let search_mid = (search_lo + search_hi) / 2u32;
                if (candidate_head_positions[search_mid as usize] as usize) < item_start_byte {
                    search_lo = search_mid + 1u32;
                }
                if (candidate_head_positions[search_mid as usize] as usize) >= item_start_byte {
                    search_hi = search_mid;
                }
            }
            let item_end_byte = item_record_bounds[item_index * 2 + 1] as usize;
            if search_lo < candidate_count && (candidate_head_positions[search_lo as usize] as usize) < item_end_byte {
                item_root_candidates[item_index] = search_lo;
            } else {
                item_root_candidates[item_index] = candidate_count;
            }
        } else {
            item_root_candidates[item_index] = candidate_count;
        }
    }
}

/// The commit: candidate i is committed iff it lies on its item's root
/// chain — lift the root by exactly `T[root]−T[i]` levels (binary
/// decomposition over the archived level tables) and compare. The marking
/// body is the retired serial kernel's, byte for byte: head advance,
/// trailer zeroing gated on the leader bit, packed-flag fetch_or (spans
/// straddle threads).
#[cube(launch_unchecked)]
pub(super) fn cluster_mark(
    candidate_head_positions: &[u32],
    total_depths: &[u32],
    level_tables: &[u32],
    candidate_count_buffer: &[u32],
    item_root_candidates: &[u32],
    item_record_bounds: &[u32],
    candidate_end_positions: &[u32],
    _candidate_slots: &[u32],
    
    glyph_flags_atomic: &mut [Atomic<u32>],
    #[comptime] kmax: usize,
    #[comptime] stride: usize,
    _bitmap_advance: f32,
) {
    // Comptime on purpose — landmine #7's shape (runtime scalars after
    // several same-typed slices) misbound rank_step outright before these
    // were comptime; there is no reason to keep a second instance of the
    // shape to find out how narrow the trigger is.
    let candidate_index = ABSOLUTE_POS;
    let candidate_count = candidate_count_buffer[0] as usize;
    let item_count = item_record_bounds.len() / 2;
    if candidate_index < candidate_count && item_count > 0 {
        let head_pos = candidate_head_positions[candidate_index] as usize;
        let item_index = item_search(item_record_bounds, item_count, head_pos);
        let root_index = item_root_candidates[item_index] as usize;
        if root_index < candidate_count {
            let root_depth = total_depths[root_index];
            let candidate_depth = total_depths[candidate_index];
            // candidate_depth == root_depth is the ROOT itself — distance 0, committed by
            // definition (the lift loop below then never runs).
            if candidate_depth <= root_depth {
                let mut x = root_index as u32;
                let mut rem = root_depth - candidate_depth;
                let mut k = 0usize;
                while k < kmax && rem > 0u32 {
                    if rem & 1u32 == 1u32 {
                        x = level_tables[k * stride + x as usize];
                    }
                    rem >>= 1u32;
                    k += 1usize;
                }
                if x as usize == candidate_index {
                    // The trailer walk clamps to the ITEM END — cend can
                    // overrun it by up to one codepoint (the span-end walk
                    // starts a member before stop), and the serial side only
                    // ever marks members strictly inside the item. The Mojo
                    // chain's ownership rule, verbatim.
                    let item_end_boundary = item_record_bounds[item_index * 2 + 1] as usize;
                    let candidate_end = candidate_end_positions[candidate_index] as usize;
                    let marking_limit = if candidate_end < item_end_boundary { candidate_end } else { item_end_boundary };
                    
                    // A committed head's glyph IS the sequence's slot —
                    // the record emitter's GLYPH_ID for cluster heads
                    // (fold.rs:607's slots.gi[id] = best_slot). The
                    // consumer is the repo parity driver / phase 4.
                    glyph_flags_atomic[head_pos >> 2].fetch_or(F_CLUSTER_HEAD << (((head_pos & 3) as u32) * 8u32));
                    let mut trailer_pos = head_pos + 1usize;
                    while trailer_pos < marking_limit {
                        if flags_at_from_atomic(glyph_flags_atomic, trailer_pos) & F_LEADER != 0 {
                            
                            // fold.rs:610-612: a trailer member's gi zeroes
                            // with its advance — the engine's records carry
                            // gi 0 for cluster trailers, and the parity
                            // driver catches exactly this.
                            
                            glyph_flags_atomic[trailer_pos >> 2].fetch_or(F_CLUSTER_TRAILER << (((trailer_pos & 3) as u32) * 8u32));
                        }
                        trailer_pos += 1usize;
                    }
                }
            }
        }
    }
}

/// flags_at over the atomic view of the packed flag buffer (the chain reads
/// leader bits while OR-ing trailer bits into the same words — the fetch_or
/// never touches bit 0, so reads stay consistent).
#[cube]
fn flags_at_from_atomic(glyph_flags_atomic: &mut [Atomic<u32>], byte_index: usize) -> u32 {
    glyph_flags_atomic[byte_index >> 2].load() >> (((byte_index & 3) as u32) * 8u32) & 0xFFu32
}

/// Host-side cluster inputs from a flat sequence table: the candidacy
/// bitmap (one bit per codepoint, set for every sequence's first member)
/// and the per-item cluster flags.
pub(crate) fn cluster_host_inputs(
    seq: &[u32],
    seq_max: u32,
    items: &[crate::fold::Item],
) -> (Vec<u32>, Vec<u32>) {
    let stride = 2 + seq_max as usize;
    let mut bitmap = vec![0u32; 0x110000 / 32 + 1];
    for i in (0..seq.len()).step_by(stride) {
        let cp = seq[i + 2] as usize;
        bitmap[cp >> 5] |= 1 << (cp & 31);
    }
    let ic = items
        .iter()
        .map(|it| u32::from(it.cluster_mode == crate::fold::ClusterMode::Cluster))
        .collect();
    (bitmap, ic)
}

/// The probe's second-level filter: for every first codepoint that starts
/// sequences, the SORTED list of second elements that actually occur. The
/// table stores EFFECTIVE keys (FE0F already stripped — the keycap entry
/// is `[49, 8419]`), so the recorded seconds are the effective seconds; a
/// pathological raw-FE0F entry would only yield a dead, over-accept-only
/// pair. `sec_off` is indexed by cp (0x110002 entries — the +1 read gives
/// each first's end), `sec_val` the flat seconds grouped by first. Entries
/// shorter than 2 are skipped: the matcher's own `elen >= 2` guard makes
/// them unreachable on both sides, so rejecting before the search is
/// behavior-identical.
pub(crate) fn cluster_pair_filter(seq: &[u32], seq_max: u32) -> (Vec<u32>, Vec<u32>) {
    let stride = 2 + seq_max as usize;
    // The kernel's "no second found" sentinel is codepoint 0 — pin that no
    // entry carries a zero ELEMENT at all (a NUL second would be
    // indistinguishable from "none" and silently diverge from the CPU,
    // which keys it). Trie data, checked once, fails loudly.
    assert!(
        (0..seq.len())
            .step_by(stride)
            .all(|e| (0..seq[e + 1] as usize).all(|k| seq[e + 2 + k] != 0)),
        "sequence entry with a zero element would break the pair filter's sentinel"
    );
    let mut pairs: Vec<(u32, u32)> = Vec::new();
    for e in (0..seq.len()).step_by(stride) {
        if seq[e + 1] >= 2 {
            pairs.push((seq[e + 2], seq[e + 3]));
        }
    }
    pairs.sort_unstable();
    pairs.dedup();
    let mut off = vec![0u32; 0x110000 + 2];
    for &(first, _) in &pairs {
        off[first as usize + 1] += 1;
    }
    for i in 1..off.len() {
        off[i] += off[i - 1];
    }
    let mut cursor = off.clone();
    let mut val = vec![0u32; pairs.len()];
    for &(first, second) in &pairs {
        let s = cursor[first as usize] as usize;
        val[s] = second;
        cursor[first as usize] += 1;
    }
    (off, val)
}
