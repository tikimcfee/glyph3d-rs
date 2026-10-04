//! Character and emoji cluster resolution for HyperLayout.
//!
//! Provides fast UTF-8 sequence decoding, trie table lookups, and
//! multi-codepoint emoji sequence / cluster resolution.

use crate::atlas::{AsciiFastEntry, TrieTable};
use crate::text::fu_to_world;

type ResolvedChar = AsciiFastEntry;

#[inline(always)]
fn sequence_length(lead: u8) -> usize {
    if lead & 0x80 == 0x00 {
        1
    } else if lead & 0xE0 == 0xC0 {
        2
    } else if lead & 0xF0 == 0xE0 {
        3
    } else if lead & 0xF8 == 0xF0 {
        4
    } else {
        0
    }
}

#[inline(always)]
fn decode_codepoint(bytes: &[u8], pos: usize, len: usize) -> u32 {
    let b0 = bytes[pos] as u32;
    if len == 1 {
        return b0;
    }
    let b1 = if pos + 1 < bytes.len() { bytes[pos + 1] as u32 } else { 0 };
    if len == 2 {
        return ((b0 & 0x1F) << 6) | (b1 & 0x3F);
    }
    let b2 = if pos + 2 < bytes.len() { bytes[pos + 2] as u32 } else { 0 };
    if len == 3 {
        return ((b0 & 0x0F) << 12) | ((b1 & 0x3F) << 6) | (b2 & 0x3F);
    }
    let b3 = if pos + 3 < bytes.len() { bytes[pos + 3] as u32 } else { 0 };
    ((b0 & 0x07) << 18) | ((b1 & 0x3F) << 12) | ((b2 & 0x3F) << 6) | (b3 & 0x3F)
}

#[inline(always)]
fn is_static_zero_cp(cp: u32) -> bool {
    cp == 0x200D || (0xFE00..=0xFE0F).contains(&cp) || (0xE0020..=0xE007F).contains(&cp)
}

#[inline(always)]
fn resolve_leader(
    bytes: &[u8],
    pos: usize,
    seq_len: usize,
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
    trailer_until: &mut usize,
) -> ResolvedChar {
    let cp = decode_codepoint(bytes, pos, seq_len);
    let entry = trie.lookup(cp);
    let height = fu_to_world(entry.height_fu, em_height_fu);

    if pos < *trailer_until {
        return ResolvedChar {
            glyph_id: 0,
            advance: 0.0,
            height,
            is_newline: false,
        };
    }

    if cp == 0x0A {
        return ResolvedChar {
            glyph_id: 0,
            advance: fu_to_world(entry.advance_fu, em_height_fu),
            height,
            is_newline: true,
        };
    }

    if is_static_zero_cp(cp) {
        return ResolvedChar {
            glyph_id: 0,
            advance: 0.0,
            height,
            is_newline: false,
        };
    }

    if trie.starts_a_sequence(cp) {
        let mut members = [0usize; 16];
        let mut key = [0u32; 16];
        members[0] = pos;
        key[0] = cp;
        let mut member_count = 1;
        let mut key_count = 1;

        let max_key = (trie.seq_max as usize).min(15);
        let mut p = pos + seq_len;
        while p < bytes.len() && key_count < max_key {
            let n2 = sequence_length(bytes[p]);
            if n2 == 0 {
                break;
            }
            let cp2 = decode_codepoint(bytes, p, n2);
            if cp2 == 0x0A || cp2 == 0xFE0E {
                break;
            }
            members[member_count] = p;
            member_count += 1;
            if cp2 != 0xFE0F {
                key[key_count] = cp2;
                key_count += 1;
            }
            p += n2;
        }

        for try_len in (2..=key_count).rev() {
            if let Some(slot) = trie.sequence_lookup(&key[..try_len]) {
                let mut span_members = 0;
                let mut need = try_len;
                while need > 0 && span_members < member_count {
                    let mid = members[span_members];
                    let n_mid = sequence_length(bytes[mid]);
                    if decode_codepoint(bytes, mid, n_mid) != 0xFE0F {
                        need -= 1;
                    }
                    span_members += 1;
                }
                if span_members > 1 {
                    let last_mid = members[span_members - 1];
                    let last_len = sequence_length(bytes[last_mid]);
                    *trailer_until = last_mid + last_len;
                }
                let entry = trie.lookup(cp);
                return ResolvedChar {
                    glyph_id: slot,
                    advance: bitmap_adv,
                    height: fu_to_world(entry.height_fu, em_height_fu),
                    is_newline: false,
                };
            }
        }
    }

    let entry = trie.lookup(cp);
    ResolvedChar {
        glyph_id: entry.glyph_id,
        advance: fu_to_world(entry.advance_fu, em_height_fu),
        height: fu_to_world(entry.height_fu, em_height_fu),
        is_newline: false,
    }
}

#[inline(always)]
pub(crate) fn resolve_byte_char(
    bytes: &[u8],
    pos: usize,
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
    trailer_until: &mut usize,
) -> Option<ResolvedChar> {
    let lead = bytes[pos];
    let fast = trie.fast_byte_table[lead as usize];
    if fast.glyph_id != AsciiFastEntry::SENTINEL && pos >= *trailer_until {
        Some(fast)
    } else {
        let seq_len = sequence_length(lead);
        if seq_len == 0 {
            None
        } else {
            Some(resolve_leader(
                bytes,
                pos,
                seq_len,
                trie,
                bitmap_adv,
                em_height_fu,
                trailer_until,
            ))
        }
    }
}
