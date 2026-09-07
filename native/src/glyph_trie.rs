//! The trie rebuild — `GlyphTrie.js` in Rust, part of the reference port.
//!
//! SOURCE, with provenance: `packages/glyph3d-core/src/compute/GlyphTrie.js` in
//! the web repo, 201 lines, at commit 3da6542 (2026-09-01),
//! sha256 328a94a7b9c290a7cb3c8b7f2beef6c1521529bd3f8ee3eeef30a527a2bc27a4.
//! That commit is the carrier split ("the last mixed container"), which renamed
//! `trieLaneValue` to `trieWireValue` and split one `blocks` array into
//! `blocksExact` + `blocksMeasure`. The vendored `engine/fixtures/gen.mjs` still
//! imports the OLD names, so it is frozen against an earlier GlyphTrie than the
//! one ported here — and the corpus can still adjudicate this port anyway,
//! because the fixture format carries wire-order VALUES rather than a
//! container's bytes. That was the whole point of the format, and this is the
//! first time it has been cashed in.
//!
//! The shape is ICU's UTrie2, one level shallower: two dependent loads and no
//! hashing.
//!
//!     block = block_index[cp >> 8]
//!     entry = (block << 8) | (cp & 0xFF)
//!
//! A miss is a VALUE, not a failure: unmapped codepoints resolve through the
//! shared missing block (storage index 0), whose entries carry FLAG_MISSING and
//! the missing advance, so an un-encoded file still lays out at the right width.

use std::collections::HashMap;

use crate::text::{ResolveGlyph, WorldEntry};

/// Codepoints per block. 256 keeps `block_index` at 4352 entries for all of
/// 0..0x10FFFF.
pub const BLOCK_SHIFT: u32 = 8;
pub const BLOCK_SIZE: usize = 1 << BLOCK_SHIFT;
pub const BLOCK_MASK: u32 = (BLOCK_SIZE - 1) as u32;
pub const BLOCK_INDEX_LENGTH: usize = 0x11_0000 >> BLOCK_SHIFT;
pub const MAX_CODEPOINT: u32 = 0x10_FFFF;

/// Entry lanes, SPLIT BY CARRIER — two arrays, one kind each. The array a lane
/// lives in IS its kind, so there are no bitcasts and no lane-kind table.
pub const TRIE_EXACT_STRIDE: usize = 2;
pub const TE_GLYPH_ID: usize = 0;
pub const TE_FLAGS: usize = 1;

pub const TRIE_MEASURE_STRIDE: usize = 2;
pub const TM_ADVANCE: usize = 0;
pub const TM_HEIGHT: usize = 1;

/// Logical lanes per entry in FIXTURE/WIRE order: [glyphId, advance, height, flags].
pub const ENTRY_LANES: usize = 4;

/// This codepoint has no atlas entry yet — render blank, report it, keep the
/// layout right.
pub const FLAG_MISSING: u32 = 1;

/// Metrics for one codepoint, in WORLD units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlyphMetrics {
    pub glyph_id: u32,
    pub advance: f32,
    pub height: f32,
}

pub struct BuiltTrie {
    pub block_index: Vec<u32>,
    pub blocks_exact: Vec<u32>,
    pub blocks_measure: Vec<f32>,
    pub block_count: usize,
    pub mapped: usize,
}

impl BuiltTrie {
    /// The entry's `i`th lane in WIRE order, as a VALUE.
    ///
    /// Anything that serializes the trie goes through here. Writing raw words
    /// instead re-serialized the measures as BIT PATTERNS — a corpus change
    /// wearing the costume of a no-op. The wire ORDER lives in one place.
    pub fn wire_value(&self, i: usize) -> f64 {
        let e = i / ENTRY_LANES;
        match i % ENTRY_LANES {
            0 => self.blocks_exact[e * TRIE_EXACT_STRIDE + TE_GLYPH_ID] as f64,
            1 => self.blocks_measure[e * TRIE_MEASURE_STRIDE + TM_ADVANCE] as f64,
            2 => self.blocks_measure[e * TRIE_MEASURE_STRIDE + TM_HEIGHT] as f64,
            _ => self.blocks_exact[e * TRIE_EXACT_STRIDE + TE_FLAGS] as f64,
        }
    }

    /// Wire-order lane count — what the fixture header records as `blocksLen`.
    pub fn wire_len(&self) -> usize {
        (self.blocks_exact.len() / TRIE_EXACT_STRIDE) * ENTRY_LANES
    }

    /// The exact two-load sequence the shader runs. Never diverge this from the
    /// kernel: it IS the shader body.
    ///
    /// The out-of-range guard is this tree's contract (0ae7010), not the JS
    /// original's — JS read off the end of `block_index` and got `undefined`,
    /// which is a different bug in a language that does not trap.
    pub fn lookup(&self, cp: u32) -> WorldEntry {
        let block = if cp <= MAX_CODEPOINT {
            self.block_index[(cp >> BLOCK_SHIFT) as usize]
        } else {
            0
        };
        let entry = ((block << BLOCK_SHIFT) | (cp & BLOCK_MASK)) as usize;
        WorldEntry {
            glyph_id: self.blocks_exact[entry * TRIE_EXACT_STRIDE + TE_GLYPH_ID],
            advance: self.blocks_measure[entry * TRIE_MEASURE_STRIDE + TM_ADVANCE],
            height: self.blocks_measure[entry * TRIE_MEASURE_STRIDE + TM_HEIGHT],
            flags: self.blocks_exact[entry * TRIE_EXACT_STRIDE + TE_FLAGS],
        }
    }
}

impl ResolveGlyph for BuiltTrie {
    fn resolve(&self, cp: u32) -> WorldEntry {
        self.lookup(cp)
    }
}

/// Build the trie.
///
/// LANDMINE 1, and it is the reason this signature takes an ITERATOR and not a
/// set: storage indices are assigned by `built.len()` in the order blocks are
/// first encountered, so the ORDER of `codepoints` is part of the input. The
/// same codepoints in a different order produce a valid-but-different trie —
/// MEASURED: block 0x4e lands at storage index 2 in text order and 3 in sorted
/// order. A Rust `HashMap` for the grouping would have scrambled that and every
/// fixture would have mismatched with a diff reading "everything is wrong"
/// rather than "your map is unordered."
///
/// `by_block` is therefore a Vec plus an index map — insertion-ordered by
/// construction, and no new dependency in a repo whose gates are byte-exact.
/// `seen` (content dedup) is a plain `HashMap` because it is NEVER ITERATED:
/// slots are assigned from `built.len()` at insertion, so its order cannot
/// reach the output. That is the one place the plan's "use IndexMap for both"
/// is stronger than it needs to be, and the ordering test below pins the
/// distinction rather than leaving it to argument.
pub fn build_glyph_trie<I, F>(
    codepoints: I,
    resolve: F,
    missing_advance: f32,
    missing_height: f32,
) -> BuiltTrie
where
    I: IntoIterator<Item = u32>,
    F: Fn(u32) -> Option<GlyphMetrics>,
{
    // Block 0 is the shared MISSING block — every unmapped block slot points at
    // it, so almost all of Unicode costs one u32 in block_index and nothing else.
    let mut missing_exact = vec![0u32; BLOCK_SIZE * TRIE_EXACT_STRIDE];
    let mut missing_measure = vec![0f32; BLOCK_SIZE * TRIE_MEASURE_STRIDE];
    for i in 0..BLOCK_SIZE {
        missing_exact[i * TRIE_EXACT_STRIDE + TE_GLYPH_ID] = 0;
        missing_exact[i * TRIE_EXACT_STRIDE + TE_FLAGS] = FLAG_MISSING;
        missing_measure[i * TRIE_MEASURE_STRIDE + TM_ADVANCE] = missing_advance;
        missing_measure[i * TRIE_MEASURE_STRIDE + TM_HEIGHT] = missing_height;
    }

    // Group the mapped codepoints by block, in first-encounter order.
    let mut by_block: Vec<(u32, Vec<u32>)> = Vec::new();
    let mut block_slot: HashMap<u32, usize> = HashMap::new();
    for cp in codepoints {
        if cp > MAX_CODEPOINT {
            continue;
        }
        let b = cp >> BLOCK_SHIFT;
        match block_slot.get(&b) {
            Some(&i) => by_block[i].1.push(cp),
            None => {
                block_slot.insert(b, by_block.len());
                by_block.push((b, vec![cp]));
            }
        }
    }

    let mut block_index = vec![0u32; BLOCK_INDEX_LENGTH]; // all zero = the missing block
    let mut built: Vec<(Vec<u32>, Vec<f32>)> =
        vec![(missing_exact.clone(), missing_measure.clone())];
    // The dedup key spans BOTH arrays — two blocks agreeing on ids and flags but
    // differing in advance are different blocks, and a key from one array alone
    // would merge them.
    //
    // KEYED ON BIT PATTERNS, not on a formatted string. The JS key is
    // `${e.join(',')}|${m.join(',')}`, whose number formatting Rust cannot
    // reproduce and should not try. Note the bit-pattern key is STRICTLY FINER:
    // JS renders +0.0 and -0.0 both as "0" and would merge two blocks that
    // differ only in the sign of a zero measure. No corpus fixture contains a
    // signed zero measure (`metricsFor` produces none), so the two agree here —
    // but if that ever changes, this is the direction of the difference.
    //
    // A COVERAGE CEILING, measured 2026-09-03 and not a hypothetical: disabling
    // this dedup entirely leaves all 14 fixtures rebuilding value-identical. The
    // corpus cannot discriminate it, because gen.mjs's `metricsFor` derives
    // glyph_id from `cp % 4093` and advance/height from `cp % 13` / `cp % 7` —
    // functions of the WHOLE codepoint — so two distinct blocks can never agree
    // on their contents. The branch is structurally unreachable for this
    // generator, not merely unexercised. `identical_blocks_share_one_slot` below
    // is therefore the ONLY check covering it, and it does fail when this is
    // broken (verified). Do not read the corpus gate's green as evidence here.
    let mut seen: HashMap<(Vec<u32>, Vec<u32>), u32> = HashMap::new();
    let mut mapped = 0usize;

    for (b, cps) in &by_block {
        let mut e = missing_exact.clone(); // start fully missing, fill what resolves
        let mut m = missing_measure.clone();
        let mut any = false;
        for &cp in cps {
            let Some(g) = resolve(cp) else { continue };
            let eo = ((cp & BLOCK_MASK) as usize) * TRIE_EXACT_STRIDE;
            let mo = ((cp & BLOCK_MASK) as usize) * TRIE_MEASURE_STRIDE;
            e[eo + TE_GLYPH_ID] = g.glyph_id;
            e[eo + TE_FLAGS] = 0;
            m[mo + TM_ADVANCE] = g.advance;
            m[mo + TM_HEIGHT] = g.height;
            any = true;
            mapped += 1;
        }
        if !any {
            continue; // nothing resolved — leave it pointing at missing
        }
        let key = (e.clone(), m.iter().map(|v| v.to_bits()).collect::<Vec<u32>>());
        let slot = match seen.get(&key) {
            Some(&s) => s,
            None => {
                let s = built.len() as u32;
                built.push((e, m));
                seen.insert(key, s);
                s
            }
        };
        block_index[*b as usize] = slot;
    }

    let mut blocks_exact = Vec::with_capacity(built.len() * BLOCK_SIZE * TRIE_EXACT_STRIDE);
    let mut blocks_measure = Vec::with_capacity(built.len() * BLOCK_SIZE * TRIE_MEASURE_STRIDE);
    for (e, m) in &built {
        blocks_exact.extend_from_slice(e);
        blocks_measure.extend_from_slice(m);
    }

    BuiltTrie {
        block_index,
        blocks_exact,
        blocks_measure,
        block_count: built.len(),
        mapped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(cp: u32) -> Option<GlyphMetrics> {
        Some(GlyphMetrics { glyph_id: (cp & 0xFF) + 1, advance: 0.5, height: 1.0 })
    }

    /// LANDMINE 1, pinned. Storage indices come from `built.len()` in
    /// first-encounter order, so the same codepoints in a different order build
    /// a valid-but-DIFFERENT trie. This test exists so that a future "tidy-up"
    /// swapping the grouping for a HashMap or a BTreeMap fails here — loudly,
    /// with a reason — instead of failing as fourteen unreadable fixture diffs.
    #[test]
    fn block_storage_order_follows_codepoint_order() {
        let text_order = build_glyph_trie([0x4E00, 0x0041], flat, 0.61, 1.25);
        let sorted_order = build_glyph_trie([0x0041, 0x4E00], flat, 0.61, 1.25);

        // Same blocks, same contents, same count — only the slots moved.
        assert_eq!(text_order.block_count, sorted_order.block_count);
        assert_eq!(text_order.block_count, 3); // missing + two real blocks
        assert_eq!(text_order.block_index[0x4E], 1);
        assert_eq!(text_order.block_index[0x00], 2);
        assert_eq!(sorted_order.block_index[0x00], 1);
        assert_eq!(sorted_order.block_index[0x4E], 2);
        assert_ne!(
            text_order.block_index, sorted_order.block_index,
            "order-insensitive grouping would make these equal and every fixture would mismatch"
        );
    }

    /// Blocks with identical CONTENT share storage — that is what keeps the
    /// trie small when source code touches a few hundred codepoints.
    #[test]
    fn identical_blocks_share_one_slot() {
        // Same low byte, same flat metrics => byte-identical blocks.
        let t = build_glyph_trie([0x0141, 0x0241], |_| flat(0x41), 0.61, 1.25);
        assert_eq!(t.block_index[0x01], 1);
        assert_eq!(t.block_index[0x02], 1, "identical content must dedup to one slot");
        assert_eq!(t.block_count, 2, "missing + one shared block");
        assert_eq!(t.mapped, 2, "both codepoints still counted as mapped");
    }

    /// A block whose every codepoint fails to resolve stays pointing at the
    /// shared missing block rather than allocating an all-missing copy.
    #[test]
    fn a_wholly_unresolved_block_allocates_nothing() {
        let t = build_glyph_trie([0x0041, 0x4E00], |cp| if cp == 0x41 { flat(cp) } else { None }, 0.61, 1.25);
        assert_eq!(t.block_count, 2);
        assert_eq!(t.block_index[0x4E], 0, "unresolved block must point at missing");
        assert_eq!(t.mapped, 1);
    }

    #[test]
    fn a_miss_is_a_value_not_a_failure() {
        let t = build_glyph_trie([0x0041], flat, 0.61, 1.25);
        let hit = t.lookup(0x41);
        assert_eq!(hit.flags & FLAG_MISSING, 0);
        assert_eq!(hit.advance, 0.5);
        // Unmapped, in an unmapped block: the shared missing entry, which still
        // occupies its width so the layout stays right.
        let miss = t.lookup(0x4E00);
        assert_eq!(miss.flags & FLAG_MISSING, FLAG_MISSING);
        assert_eq!(miss.glyph_id, 0);
        assert_eq!(miss.advance, 0.61);
        assert_eq!(miss.height, 1.25);
    }

    /// Unmapped codepoints INSIDE a mapped block keep the missing entry: the
    /// block starts as a copy of the missing block and only resolved slots are
    /// overwritten.
    #[test]
    fn an_unmapped_codepoint_in_a_mapped_block_stays_missing() {
        let t = build_glyph_trie([0x0041, 0x0040], fixture_metrics_stub, 0.61, 1.25);
        assert_eq!(t.lookup(0x41).flags & FLAG_MISSING, 0);
        let at = t.lookup(0x40);
        assert_eq!(at.flags & FLAG_MISSING, FLAG_MISSING, "'@' is deliberately unmapped");
        assert_eq!(at.advance, 0.61);
    }

    fn fixture_metrics_stub(cp: u32) -> Option<GlyphMetrics> {
        if cp == 0x40 { None } else { flat(cp) }
    }

    /// The out-of-range contract this tree settled in 0ae7010, which the JS
    /// original does not have: past the last Unicode scalar, resolve through
    /// block 0 rather than reading off the end of the index.
    #[test]
    fn an_out_of_range_codepoint_resolves_through_the_missing_block() {
        let t = build_glyph_trie([0x0041], flat, 0.61, 1.25);
        let over = t.lookup(0x11_0000);
        assert_eq!(over.flags & FLAG_MISSING, FLAG_MISSING);
        assert_eq!(over.advance, 0.61);
        // And a codepoint past the range is never accepted as input either.
        let t2 = build_glyph_trie([0x0041, 0x11_0000], flat, 0.61, 1.25);
        assert_eq!(t2.mapped, 1, "an out-of-range codepoint must not be mapped");
    }

    #[test]
    fn wire_order_is_glyph_id_advance_height_flags() {
        let t = build_glyph_trie([0x0041], flat, 0.61, 1.25);
        let e = (t.block_index[0x00] as usize) << BLOCK_SHIFT | 0x41;
        assert_eq!(t.wire_value(e * ENTRY_LANES), 0x42 as f64); // glyph_id = 0x41 + 1
        assert_eq!(t.wire_value(e * ENTRY_LANES + 1), 0.5);
        assert_eq!(t.wire_value(e * ENTRY_LANES + 2), 1.0);
        assert_eq!(t.wire_value(e * ENTRY_LANES + 3), 0.0); // resolved: flags cleared
        assert_eq!(t.wire_len(), t.block_count * BLOCK_SIZE * ENTRY_LANES);
    }
}
