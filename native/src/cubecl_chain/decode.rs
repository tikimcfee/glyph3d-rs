use cubecl::prelude::*;

use super::cluster::{cp_at, is_static_zero, seq_len_at};
use super::monoid::item_search;
use super::{F_CLUSTER_TRAILER, F_LEADER, F_MISSING, F_NEWLINE, TRIE_FLAG_MISSING};

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
    fl: &mut [u32],
    sm: &mut [f32],
    gi: &mut [u32],
    hgt: &mut [f32],
    cslot: &mut [u32],
    block_shift: u32,
) {
    let w = ABSOLUTE_POS;
    let n = bytes.len() * 4;
    if w < fl.len() {
        let curr_word = bytes[w];
        let next_word = if w + 1 < bytes.len() {
            bytes[w + 1]
        } else {
            0u32
        };
        let mut word = 0u32;
        let mut lane = 0usize;
        while lane < 4 {
            let id = w * 4 + lane;
            if id < n {
                // The candidate-slot clear rides decode (2026-09-30): the
                // probe writes cslot only on its deep candidate path, and
                // count_tile/cand_scatter read it as a predicate over every
                // byte — so it arrived as a 388 MB upload of zeros. One
                // store per byte here, where every byte is already touched.
                cslot[id] = 0u32;
                let b = byte_from_pair(curr_word, next_word, lane, id, n);
                // sequence_length, transcribed: the lenient classifier.
                let len = if b & 0x80u32 == 0u32 {
                    1u32
                } else if b & 0xE0u32 == 0xC0u32 {
                    2u32
                } else if b & 0xF0u32 == 0xE0u32 {
                    3u32
                } else if b & 0xF8u32 == 0xF0u32 {
                    4u32
                } else {
                    0u32
                };
                if len > 0u32 {
                    // decode_codepoint_at, transcribed (reads past the end
                    // are zero, continuations never validated).
                    let b1 = byte_from_pair(curr_word, next_word, lane + 1, id + 1, n);
                    let b2 = byte_from_pair(curr_word, next_word, lane + 2, id + 2, n);
                    let b3 = byte_from_pair(curr_word, next_word, lane + 3, id + 3, n);
                    let cp = if len == 1u32 {
                        b
                    } else if len == 2u32 {
                        ((b & 0x1Fu32) << 6u32) | (b1 & 0x3Fu32)
                    } else if len == 3u32 {
                        ((b & 0x0Fu32) << 12u32) | ((b1 & 0x3Fu32) << 6u32) | (b2 & 0x3Fu32)
                    } else {
                        ((b & 0x07u32) << 18u32) | ((b1 & 0x3Fu32) << 12u32) | ((b2 & 0x3Fu32) << 6u32) | (b3 & 0x3Fu32)
                    };
                    let block = if cp <= 0x10FFFFu32 {
                        block_index[(cp >> block_shift) as usize]
                    } else {
                        0u32
                    };
                    let e = ((block << block_shift) | (cp & 0xFFu32)) as usize;
                    sm[id] = blocks_m[e * 2];
                    // gi and height ride the same two-level lookup the
                    // advance does — blocks_c's low word is the glyph id,
                    // blocks_m's high word the height (both pre-converted
                    // to world units by device_tables). The gi lane is the
                    // record emitter's GLYPH_ID (phase 4 rung 1; the module
                    // header's "no device writer" gap closes here).
                    gi[id] = blocks_c[e * 2];
                    hgt[id] = blocks_m[e * 2 + 1];
                    let flag = F_LEADER
                        | (if b == 10u32 {
                            F_NEWLINE
                        } else {
                            0u32
                        })
                        | (if blocks_c[e * 2 + 1] & TRIE_FLAG_MISSING != 0 {
                            F_MISSING
                        } else {
                            0u32
                        });
                    word |= flag << ((lane as u32) * 8u32);
                } else {
                    // decode_and_resolve zeroes the statics of a non-leader
                    // — gi and height included (the fold's sm stride-2
                    // reference zeroes both lanes).
                    sm[id] = f32::from_bits(0u32);
                    gi[id] = 0u32;
                    hgt[id] = f32::from_bits(0u32);
                }
            }
            lane += 1;
        }
        fl[w] = word;
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
#[cube(launch_unchecked)]
pub(super) fn decode_probe(
    bytes: &[u32],
    block_index: &[u32],
    blocks_m: &[f32],
    blocks_c: &[u32],
    bitmap: &[u32],
    sec_off: &[u32],
    sec_val: &[u32],
    seq: &[u32],
    ir: &[u32],
    ic: &[u32],
    fl: &mut [u32],
    sm: &mut [f32],
    gi: &mut [u32],
    hgt: &mut [f32],
    cslot: &mut [u32],
    cend: &mut [u32],
    block_shift: u32,
    #[comptime] seq_max: u32,
) {
    let w = ABSOLUTE_POS;
    let n = bytes.len() * 4;
    let item_count = ir.len() / 2;
    if w < fl.len() {
        let curr_word = bytes[w];
        let next_word = if w + 1 < bytes.len() {
            bytes[w + 1]
        } else {
            0u32
        };
        let mut word = 0u32;
        let mut lane = 0usize;
        while lane < 4 {
            let id = w * 4 + lane;
            if id < n {
                cslot[id] = 0u32;
                let b = byte_from_pair(curr_word, next_word, lane, id, n);
                // sequence_length, transcribed: the lenient classifier.
                let len = if b & 0x80u32 == 0u32 {
                    1u32
                } else if b & 0xE0u32 == 0xC0u32 {
                    2u32
                } else if b & 0xF0u32 == 0xE0u32 {
                    3u32
                } else if b & 0xF8u32 == 0xF0u32 {
                    4u32
                } else {
                    0u32
                };
                if len > 0u32 {
                    let b1 = byte_from_pair(curr_word, next_word, lane + 1, id + 1, n);
                    let b2 = byte_from_pair(curr_word, next_word, lane + 2, id + 2, n);
                    let b3 = byte_from_pair(curr_word, next_word, lane + 3, id + 3, n);
                    let cp = if len == 1u32 {
                        b
                    } else if len == 2u32 {
                        ((b & 0x1Fu32) << 6u32) | (b1 & 0x3Fu32)
                    } else if len == 3u32 {
                        ((b & 0x0Fu32) << 12u32) | ((b1 & 0x3Fu32) << 6u32) | (b2 & 0x3Fu32)
                    } else {
                        ((b & 0x07u32) << 18u32) | ((b1 & 0x3Fu32) << 12u32) | ((b2 & 0x3Fu32) << 6u32) | (b3 & 0x3Fu32)
                    };
                    let block = if cp <= 0x10FFFFu32 {
                        block_index[(cp >> block_shift) as usize]
                    } else {
                        0u32
                    };
                    let e = ((block << block_shift) | (cp & 0xFFu32)) as usize;
                    sm[id] = blocks_m[e * 2];
                    gi[id] = blocks_c[e * 2];
                    hgt[id] = blocks_m[e * 2 + 1];
                    let mut flag = F_LEADER
                        | (if b == 10u32 {
                            F_NEWLINE
                        } else {
                            0u32
                        })
                        | (if blocks_c[e * 2 + 1] & TRIE_FLAG_MISSING != 0 {
                            F_MISSING
                        } else {
                            0u32
                        });

                    let mut start = 0usize;
                    let mut stop = 0usize;
                    let mut cluster = false;
                    if item_count > 0 {
                        let it = item_search(ir, item_count, id);
                        start = ir[it * 2] as usize;
                        stop = ir[it * 2 + 1] as usize;
                        cluster = ic[it] != 0;
                    }
                    if cluster && id >= start && id < stop {
                        if is_static_zero(cp) != 0u32 {
                            sm[id] = f32::from_bits(0u32);
                            gi[id] = 0u32;
                            flag |= F_CLUSTER_TRAILER;
                        } else {
                            let mut bit = 0u32;
                            if cp <= 0x10FFFFu32 {
                                bit = (bitmap[(cp >> 5u32) as usize] >> (cp & 0x1Fu32)) & 1u32;
                            }
                            if bit != 0u32 {
                                let mut q = id + len as usize;
                                let mut second = 0u32;
                                let mut hunting = 1u32;
                                while hunting == 1u32 && q < stop {
                                    let l2 = seq_len_at(bytes, q, n);
                                    let c2 = cp_at(bytes, q, l2, n);
                                    let dead2 = if l2 == 0u32 || c2 == 0x0Au32 || c2 == 0xFE0Eu32 {
                                        1u32
                                    } else {
                                        0u32
                                    };
                                    if dead2 == 1u32 {
                                        hunting = 0u32;
                                    }
                                    if dead2 == 0u32 {
                                        if c2 != 0xFE0Fu32 {
                                            second = c2;
                                            hunting = 0u32;
                                        }
                                        q += l2 as usize;
                                    }
                                }
                                let mut pair_alive = 0u32;
                                if second != 0u32 {
                                    let plo = sec_off[cp as usize];
                                    let phi = sec_off[cp as usize + 1usize];
                                    let mut lo2 = plo;
                                    let mut hi2 = phi;
                                    while lo2 < hi2 {
                                        let mid = (lo2 + hi2) / 2u32;
                                        if sec_val[mid as usize] < second {
                                            lo2 = mid + 1u32;
                                        }
                                        if sec_val[mid as usize] >= second {
                                            hi2 = mid;
                                        }
                                    }
                                    if lo2 < phi && sec_val[lo2 as usize] == second {
                                        pair_alive = 1u32;
                                    }
                                }
                                if pair_alive == 1u32 {
                                    let mut skey = Array::<u32>::new(seq_max as usize);
                                    let mut klen = 0u32;
                                    let mut p = id;
                                    let mut alive = 1u32;
                                    while alive == 1u32 && p < stop && klen < seq_max {
                                        let len2 = seq_len_at(bytes, p, n);
                                        let cp2 = cp_at(bytes, p, len2, n);
                                        let dead = if len2 == 0u32 || cp2 == 0x0Au32 || cp2 == 0xFE0Eu32 {
                                            1u32
                                        } else {
                                            0u32
                                        };
                                        if dead == 1u32 {
                                            alive = 0u32;
                                        }
                                        if dead == 0u32 {
                                            if cp2 != 0xFE0Fu32 {
                                                skey[klen as usize] = cp2;
                                                klen += 1u32;
                                            }
                                            p += len2 as usize;
                                        }
                                    }
                                    let stride = 2u32 + seq_max;
                                    let seq_count = (seq.len() / stride as usize) as u32;
                                    let mut elen = if klen < seq_max { klen } else { seq_max };
                                    let mut slot = 0u32;
                                    let mut need = 0u32;
                                    while elen >= 2u32 && slot == 0u32 {
                                        let mut lo = 0u32;
                                        let mut hi = seq_count;
                                        while lo < hi {
                                            let mid = (lo + hi) / 2u32;
                                            let eoff = mid as usize * stride as usize;
                                            let entry_len = seq[eoff + 1];
                                            let kmax = if entry_len < elen { entry_len } else { elen };
                                            let mut ord = 0i32;
                                            let mut k = 0u32;
                                            while k < kmax && ord == 0i32 {
                                                let want = seq[eoff + 2 + k as usize];
                                                let probe = skey[k as usize];
                                                if probe < want {
                                                    ord = -1i32;
                                                }
                                                if ord == 0i32 && probe > want {
                                                    ord = 1i32;
                                                }
                                                k += 1u32;
                                            }
                                            if ord == 0i32 {
                                                if entry_len < elen {
                                                    ord = 1i32;
                                                }
                                                if entry_len > elen {
                                                    ord = -1i32;
                                                }
                                            }
                                            if ord < 0i32 {
                                                hi = mid;
                                            }
                                            if ord > 0i32 {
                                                lo = mid + 1u32;
                                            }
                                            if ord == 0i32 {
                                                slot = seq[eoff];
                                                need = elen;
                                                lo = hi;
                                            }
                                        }
                                        elen -= 1u32;
                                    }
                                    if slot != 0u32 {
                                        let mut got2 = 0u32;
                                        let mut p2 = id;
                                        let mut send = id as u32;
                                        let mut alive2 = 1u32;
                                        while alive2 == 1u32 && p2 < stop {
                                            let len3 = seq_len_at(bytes, p2, n);
                                            let cp3 = cp_at(bytes, p2, len3, n);
                                            let dead3 = if len3 == 0u32 || cp3 == 0x0Au32 || cp3 == 0xFE0Eu32 {
                                                1u32
                                            } else {
                                                0u32
                                            };
                                            if dead3 == 1u32 {
                                                alive2 = 0u32;
                                            }
                                            if dead3 == 0u32 {
                                                if cp3 != 0xFE0Fu32 {
                                                    got2 += 1u32;
                                                    if got2 == need {
                                                        send = (p2 + len3 as usize) as u32;
                                                    }
                                                }
                                                p2 += len3 as usize;
                                            }
                                        }
                                        cslot[id] = slot;
                                        cend[id] = send;
                                    }
                                }
                            }
                        }
                    }
                    word |= flag << ((lane as u32) * 8u32);
                } else {
                    sm[id] = f32::from_bits(0u32);
                    gi[id] = 0u32;
                    hgt[id] = f32::from_bits(0u32);
                }
            }
            lane += 1usize;
        }
        fl[w] = word;
    }
}

