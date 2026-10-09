//! Character and emoji cluster resolution for HyperLayout.
//!
//! Provides fast UTF-8 sequence decoding, trie table lookups, and
//! multi-codepoint emoji sequence / cluster resolution.
//!
//! THE REFERENCE is `fold.rs`: `decode_and_resolve` for every leader, then,
//! for an item in [`ClusterMode::Cluster`] only, `resolve_clusters` (the
//! oracle's `resolveClusters`). The hyper-oracle gate holds this file to it.
//!
//! THE MODE is an item parameter. Callers read it once per item (or chunk)
//! with [`clusters`] and pass the bool down; leader mode never looks ahead,
//! never zeroes an invisible-by-design codepoint, and never builds a probe.
//!
//! THE ASCII FAST PATH answers a byte from `TrieTable::fast_byte_table` with
//! one load. The only ASCII bytes that can start a sequence carry `seq_lead`
//! in that same entry (derived from the table at load; `#`, `*`, `0`-`9`
//! today), and since no sequence opens with two ASCII members (asserted at
//! load), such a byte can only head a sequence when the NEXT byte is
//! non-ASCII. So the lookahead runs only for a `seq_lead` byte in cluster
//! mode, and the slow path only when that next byte is >= 0x80. Every other
//! ASCII byte costs what it cost before: a load and a compare. The same
//! invariant is why the pure-ASCII LINE shortcuts (Pass 1's counts, the host
//! and device Pass 2 line and burst paths) need no change: a pure-ASCII line
//! cannot hold a sequence.

use crate::atlas::{AsciiFastEntry, TrieTable};
use crate::fold::ClusterMode;
use crate::layout::ItemParams;
use crate::text::fu_to_world;

type ResolvedChar = AsciiFastEntry;

/// The probe key's capacity: the longest sequence (in FE0F-normalized
/// codepoints) HyperLayout can match. `TrieTable::load` refuses an atlas
/// whose `seq_max` exceeds it, so the probe never truncates a key the fold
/// would have built.
pub(crate) const MAX_SEQ_KEY: usize = 16;

/// Whether an item's layout runs the sequence pass. Read once per item.
#[inline(always)]
pub(crate) fn clusters(p: &ItemParams) -> bool {
    p.cluster_mode == ClusterMode::Cluster
}

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

/// Whether a byte is a LEADER — a byte [`resolve_byte_char`] answers with
/// `Some`, one record each (a sequence's trailers included): every byte but
/// a continuation byte or an invalid lead.
#[inline(always)]
pub(crate) fn is_leader_byte(b: u8) -> bool {
    sequence_length(b) != 0
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
fn resolved(glyph_id: u32, advance: f32, height: f32, is_newline: bool) -> ResolvedChar {
    ResolvedChar { glyph_id, advance, height, is_newline, seq_lead: false }
}

/// The slow path: one leader the fast table could not answer (a non-ASCII
/// leader, a byte inside a trailer span, or a `seq_lead` byte with a
/// non-ASCII successor in cluster mode).
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn resolve_leader(
    bytes: &[u8],
    pos: usize,
    seq_len: usize,
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
    cluster: bool,
    trailer_until: &mut usize,
    has_cluster: &mut bool,
) -> ResolvedChar {
    let cp = decode_codepoint(bytes, pos, seq_len);
    let entry = trie.lookup(cp);
    let height = fu_to_world(entry.height_fu, em_height_fu);
    let advance = fu_to_world(entry.advance_fu, em_height_fu);

    if cp == 0x0A {
        // A newline ends every candidate, so it is never inside a span.
        return resolved(0, advance, height, true);
    }

    // LEADER MODE: the fold's decode_and_resolve and nothing more. The
    // trie's own entry for every leader, the invisible-by-design codepoints
    // included: they occupy a cell, as the corpus pins.
    if !cluster {
        return resolved(entry.glyph_id, advance, height, false);
    }

    // CLUSTER MODE: fold::resolve_clusters, one leader at a time.
    if pos < *trailer_until {
        *has_cluster = true;
        return resolved(0, 0.0, height, false);
    }

    if is_static_zero_cp(cp) {
        *has_cluster = true;
        return resolved(0, 0.0, height, false);
    }

    if trie.starts_a_sequence(cp) {
        if cp >= 0x80 {
            *has_cluster = true;
        }
        // The probe: up to seq_max EFFECTIVE codepoints, VS16 dropped from
        // the key but riding the span; a newline, a continuation byte or VS15
        // ends the candidate. `key_end[k]` is the byte just past the leader
        // that contributed key member k, so a match of length L spans through
        // `key_end[L - 1]`: the fold's "count key-consumers, not members",
        // which keeps the skipped VS16s inside the trailer span.
        let mut key = [0u32; MAX_SEQ_KEY];
        let mut key_end = [0usize; MAX_SEQ_KEY];
        key[0] = cp;
        key_end[0] = pos + seq_len;
        let mut key_count = 1usize;
        let max_key = (trie.seq_max as usize).min(MAX_SEQ_KEY);
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
            if cp2 != 0xFE0F {
                key[key_count] = cp2;
                key_end[key_count] = p + n2;
                key_count += 1;
            }
            p += n2;
        }

        // The longest table prefix of the key wins.
        for try_len in (2..=key_count).rev() {
            if let Some(slot) = trie.sequence_lookup(&key[..try_len]) {
                *trailer_until = key_end[try_len - 1];
                *has_cluster = true;
                return resolved(slot, bitmap_adv, height, false);
            }
        }
    }

    resolved(entry.glyph_id, advance, height, false)
}

/// [`resolve_byte_char_cluster`] for callers that do not track whether the
/// item held a cluster.
#[inline(always)]
pub(crate) fn resolve_byte_char(
    bytes: &[u8],
    pos: usize,
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
    cluster: bool,
    trailer_until: &mut usize,
) -> Option<ResolvedChar> {
    resolve_byte_char_cluster(
        bytes,
        pos,
        trie,
        bitmap_adv,
        em_height_fu,
        cluster,
        trailer_until,
        &mut false,
    )
}

/// Resolve the byte at `pos`: `None` for a non-leader (a continuation or
/// invalid lead byte), else its glyph, advance and height. `cluster` is the
/// item's mode ([`clusters`]); `trailer_until` carries a matched sequence's
/// span across calls and starts at 0 for each item (or chunk).
#[allow(clippy::too_many_arguments)]
#[inline(always)]
pub(crate) fn resolve_byte_char_cluster(
    bytes: &[u8],
    pos: usize,
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
    cluster: bool,
    trailer_until: &mut usize,
    has_cluster: &mut bool,
) -> Option<ResolvedChar> {
    let lead = bytes[pos];
    let fast = trie.fast_byte_table[lead as usize];
    // The fast answer, unless this ASCII byte may head a sequence: a
    // `seq_lead` byte, in cluster mode, followed by a non-ASCII byte. The
    // `seq_lead` test reads the entry already loaded and is false for every
    // byte but the table's ASCII first members.
    if fast.glyph_id != AsciiFastEntry::SENTINEL
        && pos >= *trailer_until
        && !(fast.seq_lead && cluster && bytes.get(pos + 1).is_some_and(|&b| b >= 0x80))
    {
        return Some(fast);
    }
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
            cluster,
            trailer_until,
            has_cluster,
        ))
    }
}
