//! Text: the world metrics, the staged scene form, the CPU reference fold
//! and the pick path's leader table.
//!
//! - `CELL_HEIGHT_WORLD` / `LINE_HEIGHT_FACTOR`: the primary font's em
//!   (2320 fu) is one world unit, a line 1.25 of them (`fu_to_world`).
//! - [`StagedText`]: what a producer (a repo or file load, an agent
//!   session) hands `GlyphScene::new`.
//! - `reference_layout`: an independent CPU re-implementation of the
//!   layout fold, held bit-exact to the JS oracle's fixtures.
//! - [`is_pure_printable_ascii`]: the line test HyperLayout's ASCII fast
//!   paths take.
//!
//! `--render-file` staged its own text here until 2026-10-11 (`stage_file`:
//! its own tokenizer, cell-quantized columns, 4-cell tab stops). It is a
//! one-item repo load now (`repo::load_file`), laid out by the same
//! HyperLayout as `--load-repo`. The syntax palette and colourisers
//! (`colorize_leaders`, `colorize_line_into`, the pure-ASCII fast path) that
//! painted `Paint::SyntaxHeuristic` were retired the same day: colour
//! arrives as byte spans (`Paint::ByteSpans`, the `--highlight` sidecar).

use crate::glyph_scene::{GroupRow, PickContext, SegCull};

/// World-space height of one em cell (ascender→descender). Everything scales
/// from this; only the ratio to the font units matters.
pub const CELL_HEIGHT_WORLD: f32 = 1.0;
/// Line pitch as a multiple of the em height (looser than 1.0 for readability).
pub const LINE_HEIGHT_FACTOR: f32 = 1.25;

/// Pack an sRGB triple and an alpha into a slot's RGBA8 colour lane, red in
/// the low byte.
pub const fn pack_rgba8(rgb: [u8; 3], a: u8) -> u32 {
    rgb[0] as u32 | (rgb[1] as u32) << 8 | (rgb[2] as u32) << 16 | (a as u32) << 24
}

/// The staged result: instances + group rows + world bounds (for camera fit).
pub struct StagedText {
    pub instances: crate::layout::GlyphArena,
    pub groups: Vec<GroupRow>,
    /// (min, max) of the laid-out text block(s) in world units, including
    /// DEPTH — WrapBack spends wraps in z, so a block's extent is not planar.
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
    pub codepoints_decoded: usize,
    pub glyphs_emitted: usize,
    /// Codepoints dropped from the instance stream: MISSING in the trie (the
    /// CPU path) or blank/missing records (the engine paths). Bitmap slots
    /// stopped being counted here on 2026-09-10; the name predates that.
    pub missing_or_bitmap: usize,
    /// Stage E2: optional camera override — (center.xy, half-extents) of one
    /// file's page, so an offscreen shot can frame a single file instead of
    /// the whole field. `None` = fit the whole field.
    pub focus_bounds: Option<([f32; 2], [f32; 2])>,
    /// Stage F: cull segments — one per file in repo mode, a single cover
    /// segment here. World-space bounds + arena slot range + backdrop tint.
    pub segments: Vec<SegCull>,
    /// Stage G: picking context — repo mode fills this (one PickFileInfo per
    /// file + repo root/trie for deterministic per-file engine re-runs);
    /// text/engine scenes leave it None (picking unsupported there).
    pub pick: Option<PickContext>,
    /// Hierarchical layout controller and spatial scene graph (repo mode).
    pub controller: Option<crate::layout_stack::LayoutController>,
    /// The views the producer staged, one per item (`crate::views`): each
    /// one's buffer and bytes, recipe, group row, segment, slots and local
    /// extent. The item table is written from it (`Views::item_table`).
    pub views: crate::views::Views,
}

// ── Fold-convention paths ───────────────────────────────────────────────────
//
// The layout fold (`fold.rs`, and every backend held to it — HyperLayout,
// which since 2026-10-11 lays out `--render-file` too) emits ONE record per
// UTF-8 leader byte (newlines, blanks, and missing codepoints included),
// accumulates X as an f64 running sum of the per-glyph f32 advances (the
// oracle's float discipline), counts COL in leader glyphs per line (a
// double-width glyph and a tab are one column each; a tab's ADVANCE reaches
// the item's stop, `fold::TabWidth`, one cell by default and in every
// fixture), and HEIGHT is the constant cell height. `reference_layout` below
// re-implements THAT fold on the CPU, independently, so its records can be
// diffed against the fold's bit-for-bit. (Until 2026-10-11 `stage_file`
// staged `--render-file` with conventions of its own — cell-quantized
// columns, its own fixed 4-cell tab stops, its own tokenizer — and is gone:
// the file is a one-item repo load.)

use crate::atlas::TrieTable;

/// Resolve a codepoint to a glyph in WORLD units — the fold's only view of
/// a trie. The units live in `WorldEntry`, which is the whole point of the
/// seam: the atlas stores font units and converts, a fixture does not.
///
/// TWO SOURCES, one fold. The app atlas (`atlas::TrieTable`) stores FONT UNITS
/// and converts here; a `.pipe.bin` fixture stores world units already, as f64
/// VALUES narrowed once by its loader. Before this trait, `reference_layout`
/// could only be pointed at the atlas, so the fixture corpus — the only
/// artifacts with direct JS-oracle provenance — could not reach it at all.
pub trait ResolveGlyph {
    fn resolve(&self, cp: u32) -> WorldEntry;

    /// The sequence pass's table: the flat [slot, len, cps..] rows, the entry
    /// stride's seq_max, and the head's advance. Default None = "no sequences"
    /// — the rule never fires, which is the test trie's behavior (glyph_trie.rs
    /// does not override). FixtureTrie and the atlas's TrieTable do.
    fn cluster_table(&self) -> Option<(&[u32], u32, f32)> {
        None
    }
}

/// A resolved codepoint in world units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorldEntry {
    pub glyph_id: u32,
    pub advance: f32,
    pub height: f32,
    pub flags: u32,
}

impl ResolveGlyph for TrieTable {
    fn resolve(&self, cp: u32) -> WorldEntry {
        let e = self.lookup(cp);
        let em = self.metrics.em_height_fu;
        WorldEntry {
            glyph_id: e.glyph_id,
            advance: fu_to_world(e.advance_fu, em),
            height: fu_to_world(e.height_fu, em),
            flags: e.flags,
        }
    }

    /// The v2 sections, on the real atlas. The advance crosses as a WORLD
    /// value through the same one-narrowing conversion every measure here
    /// takes (held to exact arithmetic by `fu_to_world_is_the_nearest_f32`).
    fn cluster_table(&self) -> Option<(&[u32], u32, f32)> {
        if self.sequences.is_empty() {
            None
        } else {
            Some((
                &self.sequences,
                self.seq_max,
                fu_to_world(self.bitmap_advance_fu, self.metrics.em_height_fu),
            ))
        }
    }
}

/// One expected engine record, computed the CPU way. Field order mirrors the
/// 32 B wire record ([X Y Z ADVANCE HEIGHT][GLYPH_ID ROW COL]).
#[derive(Clone, Copy, Debug)]
pub struct RefGlyph {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub advance: f32,
    pub height: f32,
    pub glyph_id: u32,
    pub row: u32,
    pub col: u32,
}

/// The font-units→world conversion: `fu * CELL_HEIGHT_WORLD / em_height_fu`,
/// computed in f64 and narrowed once. Every layout path resolves advances
/// through this; `tests::fu_to_world_is_the_nearest_f32` holds it to exact
/// arithmetic over the whole u16 domain (the sweep the retired engine-trie
/// generator ran on its own copy of the conversion, until 2026-10-09).
pub(crate) fn fu_to_world(fu: i32, em_height_fu: u32) -> f32 {
    (fu as f64 * CELL_HEIGHT_WORLD as f64 / em_height_fu as f64) as f32
}

/// Replicate the fold's decode + fold (wrap 0, no pages) for one item.
///
/// Byte-level UTF-8, exactly as `decode_and_resolve`: the lead byte picks the
/// sequence length, continuation/invalid bytes are non-leaders (no record),
/// and the codepoint assembles from masked payload bits WITHOUT validating
/// the continuation bytes (bounds-checked reads return 0 past the end).
/// DO NOT MERGE THIS INTO `fold::run_pipeline`, even though that function now
/// computes a strict superset of it. They look like a dual code path and are
/// not: their VALUE is that they have different lineage.
///
/// `reference_layout` was written independently against the TSL kernel.
/// `fold.rs` descends from the layout pipeline itself — it is the contract
/// every backend is held to. The two do NOT share a parent
/// (`--fixture-reference` holds this one to the corpus, `--fixture-fold` that
/// one), so a diff between them can see a fault that one lineage carries and
/// the other does not. Merge this into `fold.rs` and the pipeline is left
/// checked only against itself — an oracle sharing a fault with its port,
/// which is the one failure mode a diff cannot see.
///
/// The fixtures cannot cover that gap on their own: they gate both
/// implementations, so anything they miss, both miss together. Independence
/// is what makes a diff of the two worth running on input OUTSIDE the corpus
/// (real source, the real atlas trie) — which no instrument does today.
///
/// Same rule, same reason as `to_world` / `fu_to_world`: a formula written twice
/// on purpose stops being a check the moment it is written once.
///
/// TAB STOPS (2026-10-11, `fold::TabWidth`) are written here a second time
/// for the same reason, from the rule rather than from `fold.rs`: the cell
/// count is kept from the line's own start as the f64 line advance over the
/// tab's advance (exact — every atlas advance is whole cells), the tab's
/// record advance is that cell narrowed from its exact f64 multiple, and the
/// line advance takes the multiple itself. The oracle has no tab stops, so
/// past one cell the two lineages agreeing is the only witness there is
/// (`fold::tests::tab_stops_agree_with_the_independent_reference`).
pub fn reference_layout<T: ResolveGlyph + ?Sized>(
    trie: &T,
    bytes: &[u8],
    origin: [f64; 3],
    line_height: f64,
    tab_width: crate::fold::TabWidth,
) -> Vec<RefGlyph> {
    let mut out = Vec::new();
    let mut line_adv: f64 = 0.0; // f64 chain — the oracle's truth-layer prefix
    let mut row: u32 = 0;
    let mut col: u32 = 0;
    let mut id = 0usize;
    let tab_cell = trie.resolve(0x09).advance as f64;
    let byte_at = |i: usize| -> u32 {
        if i < bytes.len() { bytes[i] as u32 } else { 0 }
    };
    while id < bytes.len() {
        let b0 = bytes[id] as u32;
        let n = if b0 & 0x80 == 0x00 {
            1
        } else if b0 & 0xE0 == 0xC0 {
            2
        } else if b0 & 0xF0 == 0xE0 {
            3
        } else if b0 & 0xF8 == 0xF0 {
            4
        } else {
            0
        };
        if n == 0 {
            id += 1; // non-leader: no record, layout untouched
            continue;
        }
        let cp = match n {
            1 => b0,
            2 => ((b0 & 0x1F) << 6) | (byte_at(id + 1) & 0x3F),
            3 => ((b0 & 0x0F) << 12) | ((byte_at(id + 1) & 0x3F) << 6) | (byte_at(id + 2) & 0x3F),
            _ => {
                ((b0 & 0x07) << 18)
                    | ((byte_at(id + 1) & 0x3F) << 12)
                    | ((byte_at(id + 2) & 0x3F) << 6)
                    | (byte_at(id + 3) & 0x3F)
            }
        };
        // OUT-OF-RANGE CODEPOINT -> the shared missing block, matching the
        // engine. This decode is a LENIENT classifier that never validates
        // continuation bytes, so lead bytes 0xF5-0xF7 (and 0xF4 with a
        // continuation above 0x8F) produce values past the last Unicode scalar.
        //
        // This used to `assert!(cp <= 0x10FFFF)`, which was wrong in an
        // interesting way: not because panicking is harsh, but because the ENGINE
        // did something ELSE — an unchecked read off the end of its block index,
        // yielding a plausible glyph. The two implementations disagreed on real
        // input and no corpus contained the bytes that would show it, so the
        // bit-exact gate between them had nothing to compare.
        //
        // Contract (2026-09-02): resolve it like any unmapped codepoint. The
        // guard lives in TrieTable::lookup so every caller gets it, not just
        // this one — block 0 is the shared missing block by construction, so it
        // comes back FLAG_MISSING with the missing advance and still occupies
        // its width.
        let e = trie.resolve(cp);
        let mut advance = e.advance;
        let mut line_adv_step = advance as f64;
        if cp == 0x09 && tab_width.expands() {
            let reached = line_adv / tab_cell;
            assert_eq!(reached, reached.round(), "tab stops count whole cells; the line reached {reached} cells");
            let width = tab_width.cells() as f64;
            let spanned = width - reached % width;
            line_adv_step = spanned * tab_cell;
            advance = line_adv_step as f32;
        }
        let height = e.height;
        out.push(RefGlyph {
            // Same narrowing points as the fold: f32(x + ox), f32(-row*lh + oy).
            x: (line_adv + origin[0]) as f32,
            y: (-(row as f64) * line_height + origin[1]) as f32,
            z: origin[2] as f32, // z_step = 0, no wrap
            advance,
            height,
            glyph_id: e.glyph_id,
            row,
            col,
        });
        if cp == 0x0A {
            row += 1; // rows_for_line(col, wrap=0) == 1
            col = 0;
            line_adv = 0.0;
        } else {
            col += 1;
            line_adv += line_adv_step;
        }
        id += 1;
    }
    out
}

/// fold_leaders output: per-engine-record (byte offset, codepoint), folded
/// ROW, folded COL, source line — four parallel vectors in record order.
pub type FoldTables = (Vec<(usize, u32)>, Vec<u32>, Vec<u32>, Vec<u32>);

/// Stage G — decode the UTF-8 leaders of `bytes` and fold them with the
/// layout's exact conventions (`fold.rs`, THE FOLD): COL is the raw
/// leader count within the source line (NOT col % wrap), ROW is
/// `base_row + wrap_row_of(col, wrap, is_newline)`, and the newline rides at
/// column == line length but on the row it CLOSES, so a line covers
/// `rows_for_line(len, wrap) = ceil(len / wrap)` rows. Returns one entry per
/// engine record, in record order: (byte offset, codepoint), folded ROW,
/// folded COL, source line. Picking uses this to resolve a record to the
/// actual character in the file bytes, and cross-checks ROW/COL against the
/// engine's records bit-for-bit.
pub fn fold_leaders(bytes: &[u8], wrap: i32, mode: crate::fold::WrapMode) -> FoldTables {
    let mut leaders: Vec<(usize, u32)> = Vec::new();
    let mut rows: Vec<u32> = Vec::new();
    let mut cols: Vec<u32> = Vec::new();
    let mut lines: Vec<u32> = Vec::new();
    let byte_at = |i: usize| -> u32 {
        if i < bytes.len() { bytes[i] as u32 } else { 0 }
    };
    let w = if wrap > 0 { wrap as u32 } else { 0 };
    let mut base_row = 0u32;
    let mut col = 0u32;
    let mut line = 0u32;
    let mut id = 0usize;
    while id < bytes.len() {
        let b0 = bytes[id] as u32;
        let n = if b0 & 0x80 == 0x00 {
            1
        } else if b0 & 0xE0 == 0xC0 {
            2
        } else if b0 & 0xF0 == 0xE0 {
            3
        } else if b0 & 0xF8 == 0xF0 {
            4
        } else {
            0
        };
        if n == 0 {
            id += 1; // non-leader: no record, layout untouched
            continue;
        }
        let cp = match n {
            1 => b0,
            2 => ((b0 & 0x1F) << 6) | (byte_at(id + 1) & 0x3F),
            3 => ((b0 & 0x0F) << 12) | ((byte_at(id + 1) & 0x3F) << 6) | (byte_at(id + 2) & 0x3F),
            _ => {
                ((b0 & 0x07) << 18)
                    | ((byte_at(id + 1) & 0x3F) << 12)
                    | ((byte_at(id + 2) & 0x3F) << 6)
                    | (byte_at(id + 3) & 0x3F)
            }
        };
        // Rows come from the ONE rule (`fold::wrap_row_of` / `rows_for_line`),
        // not from a second spelling of it here: this table is cross-checked
        // against the engine's own ROW lane bit-for-bit, so a copy of the
        // formula that drifted would make the pick oracle agree with nothing.
        let is_newline = cp == 0x0A;
        let wrap_row = crate::fold::wrap_row_of(col as i64, w as i64, is_newline, mode) as u32;
        leaders.push((id, cp));
        rows.push(base_row + wrap_row);
        cols.push(col);
        lines.push(line);
        if is_newline {
            base_row += crate::fold::rows_for_line(col as i64, w as i64, mode) as u32;
            col = 0;
            line += 1;
        } else {
            col += 1;
        }
        id += 1;
    }
    (leaders, rows, cols, lines)
}


/// Returns true if all bytes in `bytes` are printable ASCII (0x20..=0x7E).
///
/// Uses 16-byte NEON on aarch64 and portable 8-byte SWAR on 64-bit words,
/// falling back to scalar for trailing bytes.
#[inline]
pub fn is_pure_printable_ascii(bytes: &[u8]) -> bool {
    let len = bytes.len();
    let mut i = 0usize;

    #[cfg(target_arch = "aarch64")]
    unsafe {
        use std::arch::aarch64::*;
        let lower = vdupq_n_u8(0x20);
        let bound = vdupq_n_u8(0x5E); // 0x7E - 0x20
        while i + 16 <= len {
            let chunk = vld1q_u8(bytes.as_ptr().add(i));
            let sub = vsubq_u8(chunk, lower);
            let bad = vcgtq_u8(sub, bound);
            if vmaxvq_u8(bad) != 0 {
                return false;
            }
            i += 16;
        }
    }

    while i + 8 <= len {
        let w = unsafe { (bytes.as_ptr().add(i) as *const u64).read_unaligned() };
        let has_lt_20 = w.wrapping_sub(0x2020202020202020) & !w & 0x8080808080808080;
        let has_ge_7f = ((w & 0x7F7F7F7F7F7F7F7F).wrapping_add(0x0101010101010101) | w) & 0x8080808080808080;
        if (has_lt_20 | has_ge_7f) != 0 {
            return false;
        }
        i += 8;
    }

    while i < len {
        let b = bytes[i];
        if !(0x20..=0x7E).contains(&b) {
            return false;
        }
        i += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold::{run_pipeline, ClusterMode, Item, F_LEADER};
    use crate::glyph_trie::{build_glyph_trie, BuiltTrie, GlyphMetrics};

    fn trie() -> BuiltTrie {
        build_glyph_trie(
            (0x20u32..0x7Fu32).chain(std::iter::once(0x0A)),
            |cp| Some(GlyphMetrics { glyph_id: cp + 1, advance: 0.5, height: 1.0 }),
            0.61,
            1.25,
        )
    }

    /// `fold_leaders` is the PICK path's row/col table and it is cross-checked
    /// against the engine's own ROW lane bit-for-bit, so it has to fold by the
    /// same rule the fold does. It used to spell the rule out a second time
    /// (`base_row += wrap_row + 1`) and carried the phantom row with it; a pick
    /// oracle that disagreed with the fold would mis-resolve every click below
    /// an exact-multiple line.
    ///
    /// This checks it against `fold::run_pipeline` on input built to hit the
    /// defect: eight-cell lines at wrap 4, mixed with lines that are not
    /// multiples so the sweep is not one-sided.
    #[test]
    fn fold_leaders_agrees_with_the_fold_on_every_row_and_column() {
        let t = trie();
        let bytes: Vec<u8> = b"abcdefgh\nxyz\nABCDEFGHIJKL\n\nqq\nmnopqrst\n".to_vec();
        // Both MODES, not just the default: the pick oracle and the fold have to
        // agree about WrapBack too, and a sweep that never left mode A would say
        // nothing about the branch the dial can select.
        for mode in [crate::fold::WrapMode::Down, crate::fold::WrapMode::Back] {
            for wrap in [4i32, 3, 8, 1, 0] {
                let item = Item {
                    byte_start: 0,
                    byte_count: bytes.len() as i64,
                    wrap_width: wrap.max(0) as i64,
                    wrap_mode: mode,
                    cluster_mode: ClusterMode::default(),
                    line_height: 1.0,
                    ..Item::default()
                };
                let folded = run_pipeline(&bytes, &t, &[item]);
                let (leaders, rows, cols, _lines) = fold_leaders(&bytes, wrap, mode);
                assert_eq!(leaders.len(), bytes.len(), "every byte here is a leader");
                for (index, &(id, _cp)) in leaders.iter().enumerate() {
                    assert_ne!(folded.slots.flags(id) & F_LEADER, 0);
                    assert_eq!(
                        rows[index] as i64,
                        folded.slots.row(id),
                        "byte {id} ROW at wrap {wrap} mode {mode:?}"
                    );
                    assert_eq!(
                        cols[index] as i64,
                        folded.slots.col(id),
                        "byte {id} COL at wrap {wrap} mode {mode:?}"
                    );
                }
            }
        }
        // ANTI-VACUITY on the MODE: at wrap 4 the two modes must actually
        // disagree somewhere, or the loop above swept one rule twice.
        let (_l, rows_down, _c, _n) = fold_leaders(&bytes, 4, crate::fold::WrapMode::Down);
        let (_l2, rows_back, _c2, _n2) = fold_leaders(&bytes, 4, crate::fold::WrapMode::Back);
        assert_ne!(rows_down, rows_back, "the modes must differ on this input");
        // ANTI-VACUITY: at wrap 4 the input must really contain exact-multiple
        // lines, or this agrees about nothing that used to be wrong.
        let (_l, rows, _c, _lines) = fold_leaders(&bytes, 4, crate::fold::WrapMode::Down);
        assert_eq!(rows[8], 1, "the 8-cell line's newline rides its second row");
        assert_eq!(rows[9], 2, "and the next line starts immediately below");
    }

    /// The conversion is the NEAREST f32 to the exact quotient fu / em, ties to
    /// even, for every u16 font-unit value — checked in exact integer
    /// arithmetic, not by evaluating the same expression twice (a check that
    /// compares a function to a copy of itself cannot fail). It guards the
    /// f64-then-f32 double rounding and a wrong denominator. The atlas itself
    /// only exercises three values (1229, 2458, 2320).
    #[test]
    fn fu_to_world_is_the_nearest_f32() {
        assert_eq!(CELL_HEIGHT_WORLD, 1.0, "the exact check below assumes a unit cell");
        // An f32 > 0 as m * 2^k exactly.
        let parts = |bits: u32| -> (i128, i32) {
            let e = ((bits >> 23) & 0xFF) as i32;
            let frac = (bits & 0x7F_FFFF) as i128;
            if e == 0 { (frac, -149) } else { (frac | 0x80_0000, e - 150) }
        };
        for em in [2320u32, 2048, 1000] {
            for fu in 0..=u16::MAX as i32 {
                let got = fu_to_world(fu, em);
                if fu == 0 {
                    assert_eq!(got.to_bits(), 0);
                    continue;
                }
                let b = got.to_bits();
                let cands = [b - 1, b, b + 1].map(parts);
                let s = cands.iter().map(|&(_, k)| -k).max().unwrap().max(0);
                // |m * 2^k - fu/em| * em * 2^s, as an integer.
                let dist = |(m, k): (i128, i32)| ((m * em as i128) << (k + s)).abs_diff((fu as i128) << s);
                let (d_lo, d, d_hi) = (dist(cands[0]), dist(cands[1]), dist(cands[2]));
                assert!(
                    d <= d_lo && d <= d_hi,
                    "fu={fu} em={em}: {got} is not the nearest f32 to the exact quotient"
                );
                if d == d_lo || d == d_hi {
                    assert_eq!(b & 1, 0, "fu={fu} em={em}: a tie must round to even");
                }
            }
        }
    }

    #[test]
    fn test_is_pure_printable_ascii_exhaustive() {
        // Test single bytes
        for b in 0u8..=255 {
            let slice = [b];
            let expected = (0x20..=0x7E).contains(&b);
            assert_eq!(super::is_pure_printable_ascii(&slice), expected, "Byte 0x{b:02X} failed");
        }

        // Test lengths from 0 to 64 bytes with all printable
        for len in 0..=64 {
            let vec: Vec<u8> = (0..len).map(|i| 0x20 + (i % 95) as u8).collect();
            assert!(super::is_pure_printable_ascii(&vec), "Length {len} printable failed");

            // Poison each position with a non-printable byte
            for pos in 0..len {
                let mut poisoned = vec.clone();
                poisoned[pos] = 0x09; // tab
                assert!(!super::is_pure_printable_ascii(&poisoned), "Poison tab at pos {pos}/{len} failed");

                poisoned[pos] = 0x7F; // DEL
                assert!(!super::is_pure_printable_ascii(&poisoned), "Poison DEL at pos {pos}/{len} failed");

                poisoned[pos] = 0x80; // High byte
                assert!(!super::is_pure_printable_ascii(&poisoned), "Poison 0x80 at pos {pos}/{len} failed");
            }
        }
    }
}



