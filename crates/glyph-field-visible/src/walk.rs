//! `visible_layout.wgsl`'s `walk_segment` on the CPU, over the SAME packed
//! trie words the kernel reads ([`PackedTrie`]) — the leader classification,
//! the ASCII fast table with its seq_lead guard, the cluster-mode lookahead
//! probe and longest-prefix sequence lookup, operation for operation. It
//! exists for [`crate::VisibleField::locate`], which must count the
//! survivors of a segment exactly as the kernel did to name a transient
//! slot; the tests hold it to the kernel's own slot count on a GPU, and the
//! test crate's independent twin (written from `char_resolve.rs`, over the
//! unpacked trie) is the oracle for both.
//!
//! Only what the slot INDEX depends on is here: which bytes are leaders and
//! whether they survive (glyph != 0). X, rows and colour are the kernel's
//! business and never affect a slot's position.

use crate::tables::{PackedTrie, PK_GLYPH_MASK, PK_K_SHIFT, PK_RESOLVED_MASK, PK_SEQ_FIRST};

/// A leader the walk found: its segment-relative byte offset, the glyph it
/// resolved to (0 = no slot) and its advance in cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Leader {
    pub offset: u32,
    pub glyph: u32,
    pub cells: u32,
}

/// The kernel's 16-codepoint key bound.
const MAX_SEQ_KEY: usize = 16;

struct Walk<'a> {
    trie: &'a PackedTrie,
    /// The bytes from the segment's start; may run past `end` (the kernel
    /// reads a lead's trailing bytes and the lookahead probe's up to the
    /// item's end), zero past the slice as `byte_at` is past `lim`.
    bytes: &'a [u8],
}

impl Walk<'_> {
    fn word(&self, i: u32) -> u32 {
        self.trie.words[i as usize]
    }

    fn byte_at(&self, i: usize) -> u32 {
        self.bytes.get(i).copied().unwrap_or(0) as u32
    }

    fn decode(&self, i: usize, len: usize) -> u32 {
        let b0 = self.byte_at(i);
        if len == 1 {
            return b0;
        }
        let b1 = self.byte_at(i + 1) & 0x3F;
        if len == 2 {
            return ((b0 & 0x1F) << 6) | b1;
        }
        let b2 = self.byte_at(i + 2) & 0x3F;
        if len == 3 {
            return ((b0 & 0x0F) << 12) | (b1 << 6) | b2;
        }
        let b3 = self.byte_at(i + 3) & 0x3F;
        ((b0 & 0x07) << 18) | (b1 << 12) | (b2 << 6) | b3
    }

    /// `lookup`: the packed entry of a codepoint, block 0 past the plane.
    fn lookup(&self, cp: u32) -> u32 {
        let m = &self.trie.meta;
        let block = if cp <= 0x10FFFF { self.word(m.off_index + (cp >> m.block_shift)) } else { 0 };
        self.word(m.off_blocks + ((block << m.block_shift) | (cp & m.block_mask)))
    }

    fn starts_seq(&self, cp: u32) -> bool {
        if cp > 0x10FFFF {
            return false;
        }
        (self.word(self.trie.meta.off_bitmap + (cp >> 5)) >> (cp & 31)) & 1 != 0
    }

    fn cmp_key(&self, key: &[u32], e: u32) -> std::cmp::Ordering {
        let m = &self.trie.meta;
        let o = m.off_seq + e * m.seq_stride;
        let elen = self.word(o + 1) as usize;
        let n = key.len();
        for (k, &a) in key.iter().enumerate().take(n.min(elen)) {
            let b = self.word(o + 2 + k as u32);
            if a != b {
                return a.cmp(&b);
            }
        }
        n.cmp(&elen)
    }

    /// `seq_lookup`: the slot of the sequence `key`, if the table has it.
    fn seq_lookup(&self, key: &[u32]) -> Option<u32> {
        let m = &self.trie.meta;
        let (mut lo, mut hi) = (0u32, m.seq_count);
        while lo < hi {
            let mid = (lo + hi) >> 1;
            if self.cmp_key(key, mid) == std::cmp::Ordering::Greater {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        (lo < m.seq_count && self.cmp_key(key, lo) == std::cmp::Ordering::Equal).then(|| self.word(m.off_seq + lo * m.seq_stride))
    }

    /// `resolve_leader`: glyph | cells << 16.
    fn resolve_leader(&self, i: usize, len: usize, end: usize, cluster: bool, trailer_until: &mut usize) -> u32 {
        let cp = self.decode(i, len);
        let entry = self.lookup(cp);
        if !cluster {
            return entry & PK_RESOLVED_MASK;
        }
        if i < *trailer_until || is_static_zero(cp) {
            return 0;
        }
        if self.starts_seq(cp) {
            let mut key = Vec::with_capacity(MAX_SEQ_KEY);
            let mut key_end = Vec::with_capacity(MAX_SEQ_KEY);
            key.push(cp);
            key_end.push(i + len);
            let max_key = (self.trie.meta.seq_max as usize).min(MAX_SEQ_KEY);
            let mut p = i + len;
            while p < end && key.len() < max_key {
                let n2 = seq_len(self.byte_at(p));
                if n2 == 0 {
                    break;
                }
                let cp2 = self.decode(p, n2);
                if cp2 == 0x0A || cp2 == 0xFE0E {
                    break;
                }
                if cp2 != 0xFE0F {
                    key.push(cp2);
                    key_end.push(p + n2);
                }
                p += n2;
            }
            for try_len in (2..=key.len()).rev() {
                if let Some(hit) = self.seq_lookup(&key[..try_len]) {
                    *trailer_until = key_end[try_len - 1];
                    return (hit & PK_GLYPH_MASK) | (2 << PK_K_SHIFT);
                }
            }
        }
        entry & PK_RESOLVED_MASK
    }
}

fn seq_len(b: u32) -> usize {
    if b & 0x80 == 0 {
        1
    } else if b & 0xE0 == 0xC0 {
        2
    } else if b & 0xF0 == 0xE0 {
        3
    } else if b & 0xF8 == 0xF0 {
        4
    } else {
        0
    }
}

fn is_static_zero(cp: u32) -> bool {
    cp == 0x200D || (0xFE00..=0xFE0F).contains(&cp) || (0xE0020..=0xE007F).contains(&cp)
}

/// Every leader of the segment `bytes[..end]` in order, as `walk_segment`
/// classifies them. `bytes` starts at the segment's first byte and should
/// reach three bytes past `end` where the item does (a lead cut by the
/// segment's end is decoded over its trailing bytes); past the slice reads
/// zero, as the kernel reads zero past the item.
pub fn walk_leaders(trie: &PackedTrie, bytes: &[u8], end: usize, cluster: bool) -> Vec<Leader> {
    let w = Walk { trie, bytes };
    let off_ascii = trie.meta.off_ascii;
    let mut out = Vec::new();
    let mut trailer_until = 0usize;
    let mut i = 0usize;
    while i < end {
        let b = w.byte_at(i);
        let r = if b < 0x80 {
            let e = w.word(off_ascii + b);
            let next_non_ascii = i + 1 < end && w.byte_at(i + 1) >= 0x80;
            if i >= trailer_until && !((e & PK_SEQ_FIRST) != 0 && cluster && next_non_ascii) {
                e & PK_RESOLVED_MASK
            } else {
                w.resolve_leader(i, 1, end, cluster, &mut trailer_until)
            }
        } else {
            let len = seq_len(b);
            if len == 0 {
                i += 1;
                continue;
            }
            w.resolve_leader(i, len, end, cluster, &mut trailer_until)
        };
        out.push(Leader { offset: i as u32, glyph: r & PK_GLYPH_MASK, cells: (r >> PK_K_SHIFT) & 3 });
        // One byte, as the kernel (and the reference) advance.
        i += 1;
    }
    out
}

/// The segment-relative offsets of the leaders that take a slot (glyph !=
/// 0), in slot order: the k-th is the segment's slot `slot_base + k`.
pub fn surviving_leader_offsets(trie: &PackedTrie, bytes: &[u8], end: usize, cluster: bool) -> Vec<u32> {
    walk_leaders(trie, bytes, end, cluster).into_iter().filter(|l| l.glyph != 0).map(|l| l.offset).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::pack_trie;
    use crate::test_support::synthetic_trie;

    #[test]
    fn leaders_follow_the_kernels_rules() {
        let trie = pack_trie(&synthetic_trie());
        // ASCII, a two-byte é, a wide 中, a keycap with VS16 inside, a ZWJ
        // family, an unlisted chain (pieces), a tab (glyph 0).
        let text = "ab\u{e9}\u{4e2d} 1\u{fe0f}\u{20e3}\u{1f468}\u{200d}\u{1f469}\t\u{1f468}\u{1f469}".as_bytes();
        let cluster = walk_leaders(&trie, text, text.len(), true);
        let glyphs: Vec<(u32, u32)> = cluster.iter().map(|l| (l.glyph, l.cells)).collect();
        assert_eq!(
            glyphs,
            [
                (b'a' as u32, 1),
                (b'b' as u32, 1),
                (300, 1),
                (400, 2),
                (b' ' as u32, 1),
                (9001, 2), // the keycap head
                (0, 0),    // FE0F: static zero
                (0, 0),    // 20E3: a trailer
                (9002, 2), // the family head
                (0, 0),    // ZWJ
                (0, 0),    // the second member
                (0, 1),    // the tab: a cell, no slot
                (500, 2),
                (501, 2),
            ]
        );
        // Leader mode: every leader is the trie's own entry.
        let leader = walk_leaders(&trie, text, text.len(), false);
        assert_eq!(leader[5], Leader { offset: cluster[5].offset, glyph: b'1' as u32, cells: 1 });
        assert_eq!(leader[8].glyph, 500);
        assert_eq!(leader.len(), cluster.len(), "the leader SET is the same in both modes");
        // Survivors: everything with a glyph.
        let surv = surviving_leader_offsets(&trie, text, text.len(), true);
        assert_eq!(surv.len(), 9);
        assert_eq!(surv[0], 0);
        assert_eq!(surv[5], cluster[5].offset, "the keycap head takes the sixth slot");
        assert_eq!(surv[6], cluster[8].offset, "the family's slot follows the keycap's: its trailers took none");
    }

    #[test]
    fn a_segment_end_bounds_the_probe_and_a_cut_lead_reads_past_it() {
        let trie = pack_trie(&synthetic_trie());
        // The keycap head `1` with its combiner past the segment's end: the
        // probe stops at `end`, so the `1` is a plain leader.
        let text = "x1\u{20e3}".as_bytes();
        let cut = walk_leaders(&trie, text, 2, true);
        assert_eq!(cut.iter().map(|l| l.glyph).collect::<Vec<_>>(), [b'x' as u32, b'1' as u32]);
        // A lead whose trailing bytes lie past `end` is decoded over them.
        let text = "\u{4e2d}".as_bytes();
        let partial = walk_leaders(&trie, text, 1, true);
        assert_eq!(partial, [Leader { offset: 0, glyph: 400, cells: 2 }]);
        // And past the slice it reads zero: a lone lead becomes the missing
        // block's entry (glyph 0, one cell), not a panic.
        let lone = walk_leaders(&trie, &[0xE4], 1, true);
        assert_eq!(lone, [Leader { offset: 0, glyph: 0, cells: 1 }]);
    }
}
