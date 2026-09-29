use cubecl::prelude::*;

use super::{F_LEADER, F_MISSING, F_NEWLINE, TRIE_FLAG_MISSING};

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
    block_shift: u32,
) {
    let w = ABSOLUTE_POS;
    let n = bytes.len() * 4;
    if w < fl.len() {
        let mut word = 0u32;
        let mut lane = 0usize;
        while lane < 4 {
            let id = w * 4 + lane;
            if id < n {
                let b = byte_at(bytes, id, n);
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
                    let b1 = byte_at(bytes, id + 1, n);
                    let b2 = byte_at(bytes, id + 2, n);
                    let b3 = byte_at(bytes, id + 3, n);
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
