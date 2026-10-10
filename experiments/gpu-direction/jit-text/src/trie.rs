//! The codepoint tables: `codepoints.bin` parsed as the renderer parses it
//! (`native/src/atlas.rs`, `TrieTable::load`), the two GPU table layouts
//! built from it, and the CPU twin of the kernel's cluster-mode resolver
//! (a port of `native/src/layout_hyper/char_resolve.rs`).
//!
//! A resolved leader is a PACKED u32 everywhere in this crate:
//!   bits 0..16   glyph id (atlas slot; 0 = no slot)
//!   bits 16..18  advance in CELLS (0, 1 or 2 — every trie advance is a
//!                multiple of the primary cell, asserted at load)
//!   bit 18       this codepoint is the first member of some sequence
//!   bit 19       the entry is MISSING (informational)
//!
//! The advance in cells is what makes the no-wrap x bit-equal to HyperLayout
//! without f64 on the GPU: HyperLayout sums f32 advances in f64, and a sum of
//! k copies of one f32 is exact in f64, so its x is `f32(k * adv)` rounded
//! once — which is exactly what `f32(k) * adv` is in f32.

use std::collections::{HashMap, HashSet};
use std::path::Path;

pub const PK_GLYPH_MASK: u32 = 0xFFFF;
pub const PK_K_SHIFT: u32 = 16;
pub const PK_SEQ_FIRST: u32 = 1 << 18;
pub const PK_MISSING: u32 = 1 << 19;
/// The ASCII table's entry for a byte that is not a leader (continuation / invalid lead).
pub const PK_SENTINEL: u32 = 0xFFFF_FFFF;
pub const HASH_EMPTY: u32 = 0xFFFF_FFFF;
/// The probe key's capacity (char_resolve.rs MAX_SEQ_KEY).
pub const MAX_SEQ_KEY: usize = 16;

/// `text::fu_to_world` with CELL_HEIGHT_WORLD = 1.0 (the renderer's constant).
pub fn fu_to_world(fu: i32, em_height_fu: u32) -> f32 {
    (fu as f64 * 1.0f64 / em_height_fu as f64) as f32
}

pub fn pk_glyph(p: u32) -> u32 { p & PK_GLYPH_MASK }
pub fn pk_cells(p: u32) -> u32 { (p >> PK_K_SHIFT) & 3 }

pub struct Trie {
    pub block_shift: u32,
    pub block_index: Vec<u32>,
    /// Entries as the file has them: stride-4 words [glyph, advance_fu, height_fu, flags].
    pub blocks: Vec<u32>,
    pub entry_stride: u32,
    /// The v2 sequence section verbatim: `seq_count` × (2 + seq_max) words of [slot, len, cps..].
    pub sequences: Vec<u32>,
    pub seq_max: u32,
    pub seq_count: u32,
    seq_first: HashSet<u32>,
    pub upem: u32,
    pub advance_fu: u32,
    pub em_height_fu: u32,
    pub bitmap_advance_fu: u32,
    pub slot_count: u32,
    /// Per slot: the emoji sheet cell of a bitmap slot (glyphs.bin), None otherwise.
    pub emoji_cell: Vec<Option<u32>>,
    /// The primary cell advance in world units (what one `k` is worth).
    pub cell_adv: f32,
    /// Packed entries for the 128 ASCII bytes (PK_SENTINEL above 0x7F).
    pub ascii: [u32; 256],
    pub file_bytes: u64,
}

fn read_words(path: &Path) -> Vec<u32> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    assert!(bytes.len() % 4 == 0, "{}: not a u32 array", path.display());
    bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

fn check_magic(words: &[u32], expected: &str, path: &Path) {
    let exp = u32::from_le_bytes(expected.as_bytes().try_into().unwrap());
    assert_eq!(words.first().copied(), Some(exp), "{}: bad magic, expected {expected}", path.display());
}

pub fn is_static_zero_cp(cp: u32) -> bool {
    cp == 0x200D || (0xFE00..=0xFE0F).contains(&cp) || (0xE0020..=0xE007F).contains(&cp)
}

pub fn sequence_length(lead: u8) -> usize {
    if lead & 0x80 == 0 { 1 } else if lead & 0xE0 == 0xC0 { 2 } else if lead & 0xF0 == 0xE0 { 3 } else if lead & 0xF8 == 0xF0 { 4 } else { 0 }
}

impl Trie {
    pub fn load(dir: &Path) -> Trie {
        let cp_path = dir.join("codepoints.bin");
        let gl_path = dir.join("glyphs.bin");
        let cp = read_words(&cp_path);
        check_magic(&cp, "G3CP", &cp_path);
        let gl = read_words(&gl_path);
        check_magic(&gl, "G3GL", &gl_path);
        let (upem, advance_fu, em_height_fu) = (gl[5], gl[6], gl[7]);
        let slot_count = gl[4];
        let (font_count, font_rec_words, slot_rec_words) = (gl[3] as usize, gl[8] as usize / 4, gl[9] as usize / 4);
        let slots_at = 11 + font_count * font_rec_words;
        let emoji_cell: Vec<Option<u32>> = (0..slot_count as usize)
            .map(|s| {
                let r = &gl[slots_at + s * slot_rec_words..][..slot_rec_words];
                (r[2] & 1 != 0 && r[3] != 0xFFFF_FFFF).then_some(r[3])
            })
            .collect();

        let version = cp[1];
        assert_eq!(version, 2, "codepoints.bin: version {version}; the sequence pass needs v2");
        let header_words = 17;
        let block_shift = cp[3];
        assert_eq!(block_shift, 8, "the kernel assumes 256-codepoint blocks");
        let block_index_len = cp[4] as usize;
        let block_count = cp[5] as usize;
        let entry_stride = cp[6];
        assert_eq!(entry_stride, 4);
        let bitmap_advance_fu = cp[11];
        let seq_count = cp[12];
        let seq_max = cp[13];
        let seq_off = cp[14] as usize;
        let seq_words = seq_count as usize * (2 + seq_max as usize);
        assert_eq!(seq_off, header_words + block_index_len + block_count * 256 * entry_stride as usize);
        let sequences = cp[seq_off..seq_off + seq_words].to_vec();
        let block_index = cp[header_words..header_words + block_index_len].to_vec();
        let blocks = cp[header_words + block_index_len..seq_off].to_vec();
        let seq_first: HashSet<u32> = sequences.chunks_exact(2 + seq_max as usize).map(|e| e[2]).collect();
        assert!(seq_max as usize <= MAX_SEQ_KEY);
        // The invariants HyperLayout's ASCII fast paths and the chunk cuts rest on (atlas.rs).
        for e in sequences.chunks_exact(2 + seq_max as usize) {
            let cps = &e[2..2 + e[1] as usize];
            assert!(cps.len() >= 2, "a one-codepoint sequence");
            assert!(!(cps[0] < 0x80 && cps[1] < 0x80), "sequence {cps:X?} opens with two ASCII members");
            assert!(cps.iter().skip(1).all(|&c| c >= 0x80), "sequence {cps:X?} has an ASCII member after its first");
            assert!(cps.iter().all(|&c| c != 0), "sequence with a zero member");
        }
        // Every advance is a whole number of primary cells, 0..=2.
        for e in blocks.chunks_exact(4) {
            assert!(e[1] % advance_fu == 0 && e[1] / advance_fu <= 2, "trie advance {} fu is not 0, 1 or 2 cells", e[1]);
        }
        assert_eq!(bitmap_advance_fu, 2 * advance_fu);

        let mut t = Trie {
            block_shift, block_index, blocks, entry_stride, sequences, seq_max, seq_count, seq_first,
            upem, advance_fu, em_height_fu, bitmap_advance_fu, slot_count, emoji_cell,
            cell_adv: fu_to_world(advance_fu as i32, em_height_fu),
            ascii: [PK_SENTINEL; 256],
            file_bytes: (cp.len() * 4) as u64,
        };
        for b in 0..128u32 {
            // The newline's entry is forced to glyph 0 by the fast table; it never
            // appears inside a line here (lines exclude their '\n').
            t.ascii[b as usize] = if b == 0x0A { t.packed(b) & !PK_GLYPH_MASK } else { t.packed(b) };
        }
        let a = t.lookup(0x41);
        assert_eq!((a.0, a.1), (34, 1229), "trie sanity check failed for 'A'");
        t
    }

    /// Codepoint → (glyph, advance_fu, flags): the two dependent loads of FORMAT.md.
    pub fn lookup(&self, cp: u32) -> (u32, u32, u32) {
        let block = if cp <= 0x10FFFF { self.block_index[(cp >> self.block_shift) as usize] } else { 0 };
        let e = (((block << self.block_shift) | (cp & 0xFF)) * self.entry_stride) as usize;
        (self.blocks[e], self.blocks[e + 1], self.blocks[e + 3])
    }

    /// The packed form of `lookup`, seq-first bit included.
    pub fn packed(&self, cp: u32) -> u32 {
        let (g, adv, flags) = self.lookup(cp);
        let mut p = g | ((adv / self.advance_fu) << PK_K_SHIFT);
        if self.seq_first.contains(&cp) { p |= PK_SEQ_FIRST }
        if flags & 1 != 0 { p |= PK_MISSING }
        p
    }

    pub fn starts_a_sequence(&self, cp: u32) -> bool { self.seq_first.contains(&cp) }

    fn seq_entry(&self, i: usize) -> (u32, &[u32]) {
        let stride = 2 + self.seq_max as usize;
        let o = i * stride;
        (self.sequences[o], &self.sequences[o + 2..o + 2 + self.sequences[o + 1] as usize])
    }

    /// Binary search over the sequence section (TrieTable::sequence_lookup).
    pub fn sequence_lookup(&self, cps: &[u32]) -> Option<u32> {
        let cmp = |probe: &[u32], entry: &[u32]| {
            for k in 0..probe.len().min(entry.len()) {
                match probe[k].cmp(&entry[k]) { std::cmp::Ordering::Equal => {}, ord => return ord }
            }
            probe.len().cmp(&entry.len())
        };
        let (mut lo, mut hi) = (0usize, self.seq_count as usize);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if cmp(cps, self.seq_entry(mid).1) == std::cmp::Ordering::Greater { lo = mid + 1 } else { hi = mid }
        }
        (lo < self.seq_count as usize && cmp(cps, self.seq_entry(lo).1) == std::cmp::Ordering::Equal).then(|| self.seq_entry(lo).0)
    }

    /// Byte `i` of an item whose true end is `lim`; 0 past it (the reference's
    /// bounds-checked decode reads 0 past the slice).
    #[inline(always)]
    fn byte_at(bytes: &[u8], i: usize, lim: usize) -> u32 { if i < lim { bytes[i] as u32 } else { 0 } }

    fn decode(bytes: &[u8], i: usize, len: usize, lim: usize) -> u32 {
        let b0 = Self::byte_at(bytes, i, lim);
        match len {
            1 => b0,
            2 => ((b0 & 0x1F) << 6) | (Self::byte_at(bytes, i + 1, lim) & 0x3F),
            3 => ((b0 & 0x0F) << 12) | ((Self::byte_at(bytes, i + 1, lim) & 0x3F) << 6) | (Self::byte_at(bytes, i + 2, lim) & 0x3F),
            _ => ((b0 & 0x07) << 18) | ((Self::byte_at(bytes, i + 1, lim) & 0x3F) << 12) | ((Self::byte_at(bytes, i + 2, lim) & 0x3F) << 6) | (Self::byte_at(bytes, i + 3, lim) & 0x3F),
        }
    }

    /// The slow path (char_resolve.rs `resolve_leader`), cluster mode: a leader at
    /// `i` of `len` bytes. `end` bounds the probe (the segment's end: a cut
    /// before an ASCII byte or the line's '\n', either of which ends every
    /// candidate the reference would have built); `lim` bounds the byte reads.
    #[allow(clippy::too_many_arguments)]
    fn resolve_leader(&self, bytes: &[u8], i: usize, len: usize, end: usize, lim: usize, trailer_until: &mut usize) -> u32 {
        let cp = Self::decode(bytes, i, len, lim);
        let entry = self.packed(cp);
        if i < *trailer_until || is_static_zero_cp(cp) { return 0 }
        if entry & PK_SEQ_FIRST != 0 {
            let mut key = [0u32; MAX_SEQ_KEY];
            let mut key_end = [0usize; MAX_SEQ_KEY];
            key[0] = cp;
            key_end[0] = i + len;
            let mut n = 1usize;
            let max_key = (self.seq_max as usize).min(MAX_SEQ_KEY);
            let mut p = i + len;
            while p < end && n < max_key {
                let n2 = sequence_length(bytes[p]);
                if n2 == 0 { break }
                let cp2 = Self::decode(bytes, p, n2, lim);
                if cp2 == 0x0A || cp2 == 0xFE0E { break }
                if cp2 != 0xFE0F { key[n] = cp2; key_end[n] = p + n2; n += 1 }
                p += n2;
            }
            for try_len in (2..=n).rev() {
                if let Some(slot) = self.sequence_lookup(&key[..try_len]) {
                    *trailer_until = key_end[try_len - 1];
                    return slot | (2 << PK_K_SHIFT);
                }
            }
        }
        entry & (PK_GLYPH_MASK | (3 << PK_K_SHIFT))
    }

    /// Resolve the byte at `i` (char_resolve.rs `resolve_byte_char_cluster`, cluster
    /// mode): `None` for a non-leader, else `(packed, byte length)`. Packed carries
    /// glyph (0 = no slot) and advance in cells only.
    #[inline(always)]
    pub fn resolve(&self, bytes: &[u8], i: usize, end: usize, lim: usize, trailer_until: &mut usize) -> Option<(u32, usize)> {
        let b = bytes[i];
        if b < 0x80 {
            let e = self.ascii[b as usize];
            let next_non_ascii = i + 1 < end && bytes[i + 1] >= 0x80;
            if i >= *trailer_until && !(e & PK_SEQ_FIRST != 0 && next_non_ascii) {
                return Some((e & (PK_GLYPH_MASK | (3 << PK_K_SHIFT)), 1));
            }
            return Some((self.resolve_leader(bytes, i, 1, end, lim, trailer_until), 1));
        }
        let len = sequence_length(b);
        if len == 0 { return None }
        Some((self.resolve_leader(bytes, i, len, end, lim, trailer_until), len))
    }
}

// ---------------------------------------------------------------- GPU table layouts

/// Everything the kernel reads for a lookup, as one u32 array with the
/// offsets below; both variants' tables live in it so one bind group serves
/// both pipelines. Variant A is the trie as the file has it (index, then a
/// block), plus the sequence-head bitmap `compute_cluster_tables` builds and
/// the sequence section for the binary search. Variant B is a direct 65,536-
/// entry table for the BMP, an open-addressing hash for cp >= 0x10000, and a
/// hash of every sequence prefix key (verified against the section).
pub struct GpuTables {
    pub words: Vec<u32>,
    pub off_ascii: u32,
    pub off_index: u32,
    pub off_blocks: u32,
    pub off_bitmap: u32,
    pub off_bmp: u32,
    pub off_cp_hash: u32,
    pub cp_hash_mask: u32,
    pub off_seq_hash: u32,
    pub seq_hash_mask: u32,
    pub off_seq: u32,
    pub seq_count: u32,
    pub seq_stride: u32,
    /// Bytes resident per variant (what each one's lookups can touch).
    pub bytes_variant_a: u64,
    pub bytes_variant_b: u64,
    pub bytes_shared_ascii: u64,
}

/// u32 mixing shared by the CPU builder and the WGSL (wrapping arithmetic).
pub fn hash_cp(cp: u32) -> u32 {
    let mut h = cp.wrapping_mul(0x9E37_79B1);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h
}

/// Incremental hash of a sequence key: `h = fold(hash_seq_step, 0x811C9DC5, cps)`, then `hash_seq_final`.
pub fn hash_seq_step(h: u32, cp: u32) -> u32 { (h ^ cp).wrapping_mul(0x0100_0193) }
pub fn hash_seq_final(h: u32) -> u32 { let mut h = h ^ (h >> 15); h = h.wrapping_mul(0x2C1B_3C6D); h ^ (h >> 12) }
pub const HASH_SEQ_SEED: u32 = 0x811C_9DC5;

impl GpuTables {
    pub fn build(t: &Trie) -> GpuTables {
        let mut words: Vec<u32> = Vec::new();
        let off_ascii = 0u32;
        words.extend_from_slice(&t.ascii);

        // Variant A: index + packed blocks (one u32 per entry, same packing as the BMP table).
        let off_index = words.len() as u32;
        words.extend_from_slice(&t.block_index);
        let off_blocks = words.len() as u32;
        for e in t.blocks.chunks_exact(4) {
            let mut p = e[0] | ((e[1] / t.advance_fu) << PK_K_SHIFT);
            if e[3] & 1 != 0 { p |= PK_MISSING }
            words.push(p);
        }
        let off_bitmap = words.len() as u32;
        let bitmap_words = 0x110000 / 32 + 1;
        let bitmap_at = words.len();
        words.resize(bitmap_at + bitmap_words, 0);
        for e in t.sequences.chunks_exact(2 + t.seq_max as usize) {
            let cp = e[2] as usize;
            words[bitmap_at + (cp >> 5)] |= 1 << (cp & 31);
        }

        // Variant B: the BMP direct table, the non-BMP hash, the sequence-key hash.
        let off_bmp = words.len() as u32;
        for cp in 0..0x10000u32 { words.push(t.packed(cp)) }
        let non_bmp: Vec<(u32, u32)> = (0x10000u32..0x110000)
            .filter_map(|cp| { let p = t.packed(cp); (p & PK_MISSING == 0 || p & PK_SEQ_FIRST != 0).then_some((cp, p)) })
            .collect();
        let cp_cap = (non_bmp.len() * 2).next_power_of_two().max(16);
        let cp_hash_mask = (cp_cap - 1) as u32;
        let off_cp_hash = words.len() as u32;
        let cp_at = words.len();
        words.resize(cp_at + cp_cap * 2, HASH_EMPTY);
        for &(cp, p) in &non_bmp {
            let mut idx = (hash_cp(cp) & cp_hash_mask) as usize;
            while words[cp_at + idx * 2] != HASH_EMPTY { idx = (idx + 1) & cp_hash_mask as usize }
            words[cp_at + idx * 2] = cp;
            words[cp_at + idx * 2 + 1] = p;
        }
        let seq_cap = (t.seq_count as usize * 2).next_power_of_two().max(16);
        let seq_hash_mask = (seq_cap - 1) as u32;
        let off_seq_hash = words.len() as u32;
        let sh_at = words.len();
        words.resize(sh_at + seq_cap * 2, HASH_EMPTY);
        let stride = 2 + t.seq_max as usize;
        for (si, e) in t.sequences.chunks_exact(stride).enumerate() {
            let cps = &e[2..2 + e[1] as usize];
            let h = hash_seq_final(cps.iter().fold(HASH_SEQ_SEED, |h, &c| hash_seq_step(h, c)));
            let mut idx = (h & seq_hash_mask) as usize;
            while words[sh_at + idx * 2 + 1] != HASH_EMPTY { idx = (idx + 1) & seq_hash_mask as usize }
            words[sh_at + idx * 2] = h;
            words[sh_at + idx * 2 + 1] = si as u32;
        }
        let off_seq = words.len() as u32;
        words.extend_from_slice(&t.sequences);

        let seq_bytes = (t.sequences.len() * 4) as u64;
        let g = GpuTables {
            words, off_ascii, off_index, off_blocks, off_bitmap, off_bmp, off_cp_hash, cp_hash_mask, off_seq_hash, seq_hash_mask,
            off_seq, seq_count: t.seq_count, seq_stride: stride as u32,
            bytes_shared_ascii: 256 * 4,
            bytes_variant_a: (t.block_index.len() * 4 + t.blocks.len() + bitmap_words * 4) as u64 + seq_bytes,
            bytes_variant_b: (0x10000 * 4 + cp_cap * 8 + seq_cap * 8) as u64 + seq_bytes,
        };
        g.self_check(t);
        g
    }

    /// Variant B answers every codepoint and every sequence as the trie does.
    fn self_check(&self, t: &Trie) {
        for cp in 0..0x110000u32 {
            let want = t.packed(cp);
            let got = if cp < 0x10000 { self.words[(self.off_bmp + cp) as usize] } else { self.cp_hash_lookup(cp) };
            assert_eq!(got, want, "variant B disagrees with the trie at U+{cp:X}");
        }
        let stride = self.seq_stride as usize;
        let mut seen: HashMap<Vec<u32>, u32> = HashMap::new();
        for e in t.sequences.chunks_exact(stride) {
            let cps = e[2..2 + e[1] as usize].to_vec();
            assert_eq!(self.seq_hash_lookup(&cps), Some(e[0]));
            seen.insert(cps, e[0]);
        }
        assert_eq!(seen.len(), t.seq_count as usize, "duplicate sequence keys");
    }

    pub fn cp_hash_lookup(&self, cp: u32) -> u32 {
        let base = self.off_cp_hash as usize;
        let mut idx = (hash_cp(cp) & self.cp_hash_mask) as usize;
        loop {
            let k = self.words[base + idx * 2];
            if k == HASH_EMPTY { return PK_MISSING | (1 << PK_K_SHIFT) }
            if k == cp { return self.words[base + idx * 2 + 1] }
            idx = (idx + 1) & self.cp_hash_mask as usize;
        }
    }

    pub fn seq_hash_lookup(&self, cps: &[u32]) -> Option<u32> {
        let h = hash_seq_final(cps.iter().fold(HASH_SEQ_SEED, |h, &c| hash_seq_step(h, c)));
        let base = self.off_seq_hash as usize;
        let mut idx = (h & self.seq_hash_mask) as usize;
        loop {
            let si = self.words[base + idx * 2 + 1];
            if si == HASH_EMPTY { return None }
            if self.words[base + idx * 2] == h {
                let o = (self.off_seq + si * self.seq_stride) as usize;
                let len = self.words[o + 1] as usize;
                if len == cps.len() && self.words[o + 2..o + 2 + len] == *cps { return Some(self.words[o]) }
            }
            idx = (idx + 1) & self.seq_hash_mask as usize;
        }
    }
}
