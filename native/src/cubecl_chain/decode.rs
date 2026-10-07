use cubecl::prelude::*;

use super::cluster::{cp_at, is_static_zero, seq_len_at};
use super::monoid::item_search;
use super::{F_CLUSTER_TRAILER, F_LEADER, F_MISSING, F_NEWLINE, F_SURVIVOR, TRIE_FLAG_MISSING};

// ── dispatch 0: decode — thread per 4-byte word ──────────────────────────────
//
// The phase-3a port of fold::decode_all's leader-mode half. Every byte is
// INDEPENDENT: the lenient classifier reads only the byte's own bits
// (continuations self-identify, leads declare their length, continuation
// bytes are never validated — transcribed exactly), the codepoint reads at
// most three bytes forward, and the resolve is the same two dependent loads
// as TrieTable::lookup (block_index[cp >> shift], then entry
// (block<<shift)|(cp & 0xFF)). One thread per WORD so the packed flag word
// is written whole — four byte-lane writers to one u32 would race.
//
// Scope, deliberately: this produces the chain's inputs (packed fl, advance
// f32) only. gi/height are renderer statics the CPU still owns; the miss
// list is a CPU product concern; and cluster resolution is the separate
// phase-3b pass. The tables arrive PRE-CONVERTED to world units (fixtures
// store world values; the atlas path converts once at upload) so no
// device-side division — and no fast-math question — ever touches an
// advance bit.
#[cube(launch_unchecked)]
pub(super) fn decode(
    bytes: &[u32],
    block_index: &[u32],
    blocks_m: &[f32],
    blocks_c: &[u32],
    glyph_flags: &mut [u32],
    
    glyph_heights: &mut [f32],
    candidate_slots: &mut [u32],
    block_shift: u32,
) {
    let word_index = ABSOLUTE_POS;
    let total_bytes = bytes.len() * 4;
    if word_index < glyph_flags.len() {
        let curr_word = bytes[word_index];
        let next_word = if word_index + 1 < bytes.len() {
            bytes[word_index + 1]
        } else {
            0u32
        };
        let mut packed_word = 0u32;
        let mut lane = 0usize;
        while lane < 4 {
            let byte_index = word_index * 4 + lane;
            if byte_index < total_bytes {
                // The candidate-slot clear rides decode (2026-09-30): the
                // probe writes candidate_slots only on its deep candidate path, and
                // count_tile/cand_scatter read it as a predicate over every
                // byte — so it arrived as a 388 MB upload of zeros. One
                // store per byte here, where every byte is already touched.
                candidate_slots[byte_index] = 0u32;
                let lead_byte = byte_from_pair(curr_word, next_word, lane, byte_index, total_bytes);
                // sequence_length, transcribed: the lenient classifier.
                let len = if lead_byte & 0x80u32 == 0u32 {
                    1u32
                } else if lead_byte & 0xE0u32 == 0xC0u32 {
                    2u32
                } else if lead_byte & 0xF0u32 == 0xE0u32 {
                    3u32
                } else if lead_byte & 0xF8u32 == 0xF0u32 {
                    4u32
                } else {
                    0u32
                };
                if len > 0u32 {
                    // decode_codepoint_at, transcribed (reads past the end
                    // are zero, continuations never validated).
                    let codepoint = if len == 1u32 {
                        lead_byte
                    } else {
                        let byte1 = byte_from_pair(curr_word, next_word, lane + 1, byte_index + 1, total_bytes);
                        if len == 2u32 {
                            ((lead_byte & 0x1Fu32) << 6u32) | (byte1 & 0x3Fu32)
                        } else {
                            let byte2 = byte_from_pair(curr_word, next_word, lane + 2, byte_index + 2, total_bytes);
                            if len == 3u32 {
                                ((lead_byte & 0x0Fu32) << 12u32) | ((byte1 & 0x3Fu32) << 6u32) | (byte2 & 0x3Fu32)
                            } else {
                                let byte3 = byte_from_pair(curr_word, next_word, lane + 3, byte_index + 3, total_bytes);
                                ((lead_byte & 0x07u32) << 18u32) | ((byte1 & 0x3Fu32) << 12u32) | ((byte2 & 0x3Fu32) << 6u32) | (byte3 & 0x3Fu32)
                            }
                        }
                    };
                    let block = if codepoint <= 0x10FFFFu32 {
                        block_index[(codepoint >> block_shift) as usize]
                    } else {
                        0u32
                    };
                    let entry_offset = ((block << block_shift) | (codepoint & 0xFFu32)) as usize;
                    
                    // glyph_indices and height ride the same two-level lookup the
                    // advance does — blocks_c's low word is the glyph id,
                    // blocks_m's high word the height (both pre-converted
                    // to world units by device_tables). The glyph_indices lane is the
                    // record emitter's GLYPH_ID (phase 4 rung 1; the module
                    // header's "no device writer" gap closes here).
                    let glyph_id = blocks_c[entry_offset * 2];
                    
                    glyph_heights[byte_index] = blocks_m[entry_offset * 2 + 1];
                    let flag = F_LEADER
                        | (if lead_byte == 10u32 {
                            F_NEWLINE
                        } else {
                            0u32
                        })
                        | (if blocks_c[entry_offset * 2 + 1] & TRIE_FLAG_MISSING != 0 {
                            F_MISSING
                        } else {
                            0u32
                        })
                        | (if glyph_id != 0u32 {
                            F_SURVIVOR
                        } else {
                            0u32
                        });
                    packed_word |= flag << ((lane as u32) * 8u32);
                } else {
                    // decode_and_resolve zeroes the statics of a non-leader
                    // — glyph_indices and height included (the fold's sm stride-2
                    // reference zeroes both lanes).
                    
                    
                    glyph_heights[byte_index] = f32::from_bits(0u32);
                }
            }
            lane += 1;
        }
        glyph_flags[word_index] = packed_word;
    }
}

/// Byte i of the packed corpus, zero past the end (the reference's
/// bounds-checked read).
#[cube]
pub(super) fn byte_at(bytes: &[u32], i: usize, n: usize) -> u32 {
    if i < n {
        (bytes[i >> 2] >> (((i & 3) * 8) as u32)) & 0xFFu32
    } else {
        0u32
    }
}

/// Reads a byte from a register pair of consecutive u32 words without global memory access.
#[cube]
pub(super) fn byte_from_pair(curr: u32, next: u32, offset: usize, id: usize, n: usize) -> u32 {
    if id < n {
        if offset < 4 {
            (curr >> ((offset as u32) * 8u32)) & 0xFFu32
        } else {
            (next >> (((offset - 4) as u32) * 8u32)) & 0xFFu32
        }
    } else {
        0u32
    }
}

/// Fused decode and cluster probe — thread per 4-byte word.
///
/// Combines phase 3a (UTF-8 lenient decode, trie metrics lookup) and phase 3b
/// (cluster item classification, static-zero trailer marking, pair filtering,
/// and descending-length binary search) into a single pass over `bytes`.
///
/// Codepoints are decoded once in registers, eliminating the 97 MB VRAM
/// round-trip of `fl`, redundant `seq_len_at`/`cp_at` re-evaluations, and
/// an entire 95-million-thread dispatch from the critical path.
#[allow(clippy::manual_range_contains)]
#[cube(launch_unchecked)]
pub(super) fn decode_probe(
    bytes: &[u32],
    block_index: &[u32],
    _blocks_m: &[f32],
    blocks_c: &[u32],
    bitmap: &[u32],
    secondary_offsets: &[u32],
    secondary_values: &[u32],
    sequence_table: &[u32],
    item_record_bounds: &[u32],
    item_cluster_enabled: &[u32],
    glyph_flags: &mut [u32],
    
    candidate_head_positions: &mut [u32],
    candidate_slots: &mut [u32],
    candidate_end_positions: &mut [u32],
    candidate_total_atomic: &mut [Atomic<u32>],
    block_shift: u32,
    #[comptime] seq_max: u32,
    #[comptime] candidate_capacity: usize,
) {
    let word_index = ABSOLUTE_POS;
    let total_bytes = bytes.len() * 4;
    let item_count = item_record_bounds.len() / 2;
    if word_index < glyph_flags.len() {
        let curr_word = bytes[word_index];
        let next_word = if word_index + 1 < bytes.len() {
            bytes[word_index + 1]
        } else {
            0u32
        };
        let mut packed_word = 0u32;
        let mut lane = 0usize;
        while lane < 4 {
            let byte_index = word_index * 4 + lane;
            if byte_index < total_bytes {
                let lead_byte = byte_from_pair(curr_word, next_word, lane, byte_index, total_bytes);
                let is_ascii = lead_byte < 128u32;
                let len = if is_ascii {
                    1u32
                } else if lead_byte & 0xE0u32 == 0xC0u32 {
                    2u32
                } else if lead_byte & 0xF0u32 == 0xE0u32 {
                    3u32
                } else if lead_byte & 0xF8u32 == 0xF0u32 {
                    4u32
                } else {
                    0u32
                };
                if len > 0u32 {
                    let codepoint = if is_ascii {
                        lead_byte
                    } else {
                        let byte1 = byte_from_pair(curr_word, next_word, lane + 1, byte_index + 1, total_bytes);
                        if len == 2u32 {
                            ((lead_byte & 0x1Fu32) << 6u32) | (byte1 & 0x3Fu32)
                        } else {
                            let byte2 = byte_from_pair(curr_word, next_word, lane + 2, byte_index + 2, total_bytes);
                            if len == 3u32 {
                                ((lead_byte & 0x0Fu32) << 12u32) | ((byte1 & 0x3Fu32) << 6u32) | (byte2 & 0x3Fu32)
                            } else {
                                let byte3 = byte_from_pair(curr_word, next_word, lane + 3, byte_index + 3, total_bytes);
                                ((lead_byte & 0x07u32) << 18u32) | ((byte1 & 0x3Fu32) << 12u32) | ((byte2 & 0x3Fu32) << 6u32) | (byte3 & 0x3Fu32)
                            }
                        }
                    };
                    let mut flag = if is_ascii {
                        F_LEADER
                            | (if lead_byte == 10u32 {
                                F_NEWLINE
                            } else {
                                0u32
                            })
                            | (if lead_byte >= 32u32 && lead_byte <= 126u32 {
                                F_SURVIVOR
                            } else {
                                0u32
                            })
                    } else {
                        let block = if codepoint <= 0x10FFFFu32 {
                            block_index[(codepoint >> block_shift) as usize]
                        } else {
                            0u32
                        };
                        let entry_offset = ((block << block_shift) | (codepoint & 0xFFu32)) as usize;
                        let glyph_id = blocks_c[entry_offset * 2];
                        F_LEADER
                            | (if lead_byte == 10u32 {
                                F_NEWLINE
                            } else {
                                0u32
                            })
                            | (if blocks_c[entry_offset * 2 + 1] & TRIE_FLAG_MISSING != 0 {
                                F_MISSING
                            } else {
                                0u32
                            })
                            | (if glyph_id != 0u32 {
                                F_SURVIVOR
                            } else {
                                0u32
                            })
                    };

                    let is_sz = if is_ascii { 0u32 } else { is_static_zero(codepoint) };
                    let mut is_candidate_head = 0u32;
                    if is_sz == 0u32 && codepoint <= 0x10FFFFu32 {
                        is_candidate_head = (bitmap[(codepoint >> 5u32) as usize] >> (codepoint & 0x1Fu32)) & 1u32;
                    }

                    if (is_sz != 0u32 || is_candidate_head != 0u32) && item_count > 0 {
                        let item_index = item_search(item_record_bounds, item_count, byte_index);
                        let item_start_byte = item_record_bounds[item_index * 2] as usize;
                        let item_end_byte = item_record_bounds[item_index * 2 + 1] as usize;
                        let cluster_enabled = item_cluster_enabled[item_index] != 0;

                        if cluster_enabled && byte_index >= item_start_byte && byte_index < item_end_byte {
                            if is_sz != 0u32 {
                                flag = (flag & !F_SURVIVOR) | F_CLUSTER_TRAILER;
                            } else {
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
                                        let candidate_index = candidate_total_atomic[0].fetch_add(1);
                                        if (candidate_index as usize) < candidate_capacity {
                                            candidate_head_positions[candidate_index as usize] = byte_index as u32;
                                            candidate_slots[candidate_index as usize] = matched_slot;
                                            candidate_end_positions[candidate_index as usize] = matched_end_byte;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    packed_word |= flag << ((lane as u32) * 8u32);
                }
            }
            lane += 1usize;
        }
        glyph_flags[word_index] = packed_word;
    }
}


#[cube]
pub(super) fn decode_trie(
    codepoint: u32,
    block_index: &[u32],
    blocks_m: &[f32],
    blocks_c: &[u32],
    block_shift: u32,
) -> (f32, u32) {
    let block = if codepoint <= 0x10FFFFu32 {
        block_index[(codepoint >> block_shift) as usize]
    } else {
        0u32
    };
    let entry_offset = ((block << block_shift) | (codepoint & 0xFFu32)) as usize;
    let advance = blocks_m[entry_offset * 2];
    let glyph_id = blocks_c[entry_offset * 2];
    (advance, glyph_id)
}
