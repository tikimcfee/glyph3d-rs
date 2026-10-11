//! Text: the world metrics, the syntax palette and colourisers, the staged
//! scene form, and the CPU reference fold.
//!
//! - `CELL_HEIGHT_WORLD` / `LINE_HEIGHT_FACTOR`: the primary font's em
//!   (2320 fu) is one world unit, a line 1.25 of them (`fu_to_world`).
//! - The syntax colourisers (`colorize_leaders` over an item,
//!   `colorize_line_into` and the pure-ASCII fast path per line): one
//!   colour per UTF-8 leader from a small keyword / number / string /
//!   comment / punctuation heuristic, what `Paint::SyntaxHeuristic` paints
//!   on every HyperLayout path.
//! - [`StagedText`]: what a producer (a repo or file load, an agent
//!   session) hands `GlyphScene::new`.
//! - `reference_layout`: an independent CPU re-implementation of the
//!   layout fold, held bit-exact to the JS oracle's fixtures.
//!
//! `--render-file` staged its own text here until 2026-10-11 (`stage_file`:
//! its own tokenizer, cell-quantized columns, 4-cell tab stops). It is a
//! one-item repo load now (`repo::load_file`), laid out and painted by the
//! same HyperLayout and colourisers as `--load-repo`.

use crate::glyph_scene::{GroupRow, PickContext, SegCull};

/// World-space height of one em cell (ascender→descender). Everything scales
/// from this; only the ratio to the font units matters.
pub const CELL_HEIGHT_WORLD: f32 = 1.0;
/// Line pitch as a multiple of the em height (looser than 1.0 for readability).
pub const LINE_HEIGHT_FACTOR: f32 = 1.25;

/// sRGB display-value palette (VS Code dark+ flavored, brightened comments for contrast).
pub(crate) mod palette {
    pub const DEFAULT: [u8; 3] = [212, 212, 212]; // identifiers, plain text
    pub const KEYWORD: [u8; 3] = [197, 134, 192]; // const/let/fn/return…
    pub const NUMBER: [u8; 3] = [181, 206, 168];
    pub const STRING: [u8; 3] = [206, 145, 120];
    pub const COMMENT: [u8; 3] = [125, 200, 115]; // vibrant, legible spring green (high contrast against dark background)
    pub const PUNCT: [u8; 3] = [128, 128, 128];

    pub const C_DEFAULT: u32 = super::pack_rgba8(DEFAULT, 255);
    pub const C_KEYWORD: u32 = super::pack_rgba8(KEYWORD, 255);
    pub const C_NUMBER: u32 = super::pack_rgba8(NUMBER, 255);
    pub const C_STRING: u32 = super::pack_rgba8(STRING, 255);
    pub const C_COMMENT: u32 = super::pack_rgba8(COMMENT, 255);
    pub const C_PUNCT: u32 = super::pack_rgba8(PUNCT, 255);
}

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

const fn make_ascii_word_table() -> [bool; 128] {
    let mut table = [false; 128];
    let mut i = 0usize;
    while i < 128 {
        let b = i as u8;
        if (b >= b'a' && b <= b'z')
            || (b >= b'A' && b <= b'Z')
            || (b >= b'0' && b <= b'9')
            || b == b'_'
            || b == b'$'
        {
            table[i] = true;
        }
        i += 1;
    }
    table
}

const ASCII_WORD_CHAR: [bool; 128] = make_ascii_word_table();

#[inline(always)]
fn is_word_char(ch: char) -> bool {
    let u = ch as u32;
    if u < 128 {
        ASCII_WORD_CHAR[u as usize]
    } else {
        ch.is_alphanumeric() || ch == '_' || ch == '$'
    }
}

/// Checks whether a line starts with block comment continuation markers:
/// leading whitespace followed by `*` and then ` `, `/`, `*`, `\t`, `\n`, `\r`, or end-of-slice.
#[inline(always)]
pub(crate) fn is_block_comment_continuation_line(bytes: &[u8]) -> bool {
    let mut index = 0usize;
    while index < bytes.len() && (bytes[index] == b' ' || bytes[index] == b'\t') {
        index += 1;
    }
    if index < bytes.len() && bytes[index] == b'*' {
        let next_index = index + 1;
        next_index >= bytes.len()
            || bytes[next_index] == b' '
            || bytes[next_index] == b'/'
            || bytes[next_index] == b'*'
            || bytes[next_index] == b'\t'
            || bytes[next_index] == b'\n'
            || bytes[next_index] == b'\r'
    } else {
        false
    }
}

#[inline]
fn word_color_packed(word: &[u8]) -> u32 {
    match word.len() {
        2 => match word {
            b"fn" | b"if" | b"of" | b"in" | b"do" | b"Ok" => palette::C_KEYWORD,
            _ => if word[0].is_ascii_digit() { palette::C_NUMBER } else { palette::C_DEFAULT },
        },
        3 => match word {
            b"let" | b"var" | b"for" | b"new" | b"try" | b"get" | b"set" | b"pub" | b"mod" | b"use" | b"mut" | b"ref" | b"dyn" | b"Err" => palette::C_KEYWORD,
            _ => if word[0].is_ascii_digit() { palette::C_NUMBER } else { palette::C_DEFAULT },
        },
        4 => match word {
            b"from" | b"this" | b"case" | b"null" | b"true" | b"void" | b"enum" | b"impl" | b"loop" | b"move" | b"self" | b"Self" | b"Some" | b"None" | b"else" => palette::C_KEYWORD,
            _ => if word[0].is_ascii_digit() { palette::C_NUMBER } else { palette::C_DEFAULT },
        },
        5 => match word {
            b"const" | b"while" | b"class" | b"break" | b"catch" | b"throw" | b"async" | b"await" | b"yield" | b"false" | b"trait" | b"where" | b"match" | b"crate" | b"super" => palette::C_KEYWORD,
            _ => if word[0].is_ascii_digit() { palette::C_NUMBER } else { palette::C_DEFAULT },
        },
        6 => match word {
            b"return" | b"export" | b"import" | b"typeof" | b"switch" | b"struct" | b"unsafe" | b"static" | b"delete" => palette::C_KEYWORD,
            _ => if word[0].is_ascii_digit() { palette::C_NUMBER } else { palette::C_DEFAULT },
        },
        7 => match word {
            b"extends" | b"default" | b"finally" => palette::C_KEYWORD,
            _ => if word[0].is_ascii_digit() { palette::C_NUMBER } else { palette::C_DEFAULT },
        },
        8 => match word {
            b"function" | b"continue" => palette::C_KEYWORD,
            _ => if word[0].is_ascii_digit() { palette::C_NUMBER } else { palette::C_DEFAULT },
        },
        9 => match word {
            b"undefined" => palette::C_KEYWORD,
            _ => if word[0].is_ascii_digit() { palette::C_NUMBER } else { palette::C_DEFAULT },
        },
        10 => match word {
            b"instanceof" => palette::C_KEYWORD,
            _ => if word[0].is_ascii_digit() { palette::C_NUMBER } else { palette::C_DEFAULT },
        },
        _ => if !word.is_empty() && word[0].is_ascii_digit() { palette::C_NUMBER } else { palette::C_DEFAULT },
    }
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


/// Stage E2 — per-leader syntax colors for one file, in ENGINE record order
/// (one entry per UTF-8 leader byte, exactly the records `compact` emits, so
/// `colors[i]` paints `records[i]`). The tokenizer — keywords / numbers /
/// strings / line comments / punctuation, once `stage_file`'s too (gone
/// 2026-10-11, when `--render-file` became a repo load) — is a heuristic
/// paint layer over the engine's layout; leader classification is the same
/// byte-level logic as `reference_layout`.
///
/// It must paint exactly what the device Pass 2 paints line by line
/// ([`colorize_line_into`], and for pure printable ASCII
/// [`colorize_pure_ascii_line_slice`]) — hyper-oracle's paint tier holds the
/// device to it. So its word rule is the fast path's: a word is checked up to
/// the point that ENDS it (a word running into a string, `return"$"`, is not
/// a keyword) and only a keyword is filled back. Until 2026-10-09 the generic
/// forms checked up to the last word byte and filled back any colour, so
/// `return"$"` came out keyword, string and all (C17).
pub fn colorize_leaders(bytes: &[u8]) -> Vec<u32> {
    let mut colors: Vec<u32> = Vec::with_capacity(bytes.len());
    let mut word_start_byte = usize::MAX;
    let mut word_start_col = 0usize;
    let is_block_continuation = is_block_comment_continuation_line(bytes);
    let mut in_line_comment = false;
    let mut in_block_comment = is_block_continuation;
    let mut block_comment_start = if is_block_continuation { 0 } else { usize::MAX };
    let mut in_string: Option<char> = None;
    let mut prev = '\0';

    let byte_at = |i: usize| -> u32 {
        if i < bytes.len() { bytes[i] as u32 } else { 0 }
    };
    let mut id = 0usize;
    while id < bytes.len() {
        let b0 = bytes[id] as u32;
        let (_len, ch) = if b0 < 0x80 {
            (1usize, b0 as u8 as char)
        } else if b0 & 0xE0 == 0xC0 {
            let cp = ((b0 & 0x1F) << 6) | (byte_at(id + 1) & 0x3F);
            (2usize, char::from_u32(cp).unwrap_or('\u{FFFD}'))
        } else if b0 & 0xF0 == 0xE0 {
            let cp = ((b0 & 0x0F) << 12) | ((byte_at(id + 1) & 0x3F) << 6) | (byte_at(id + 2) & 0x3F);
            (3usize, char::from_u32(cp).unwrap_or('\u{FFFD}'))
        } else if b0 & 0xF8 == 0xF0 {
            let cp = ((b0 & 0x07) << 18)
                | ((byte_at(id + 1) & 0x3F) << 12)
                | ((byte_at(id + 2) & 0x3F) << 6)
                | (byte_at(id + 3) & 0x3F);
            (4usize, char::from_u32(cp).unwrap_or('\u{FFFD}'))
        } else {
            id += 1; // continuation/invalid byte: no record, no color
            continue;
        };

        let mut color = palette::C_DEFAULT;
        if ch == '\n' {
            if word_start_byte != usize::MAX {
                let wc = word_color_packed(&bytes[word_start_byte..id]);
                if wc == palette::C_KEYWORD {
                    colors[word_start_col..].fill(wc);
                }
                word_start_byte = usize::MAX;
            }
            in_line_comment = false;
            in_block_comment = if id + 1 < bytes.len() {
                is_block_comment_continuation_line(&bytes[id + 1..])
            } else {
                false
            };
            block_comment_start = if in_block_comment { id + 1 } else { usize::MAX };
            in_string = None;
        } else if ch == '\t' {
            word_start_byte = usize::MAX;
        } else if in_line_comment {
            color = palette::C_COMMENT;
        } else if in_block_comment {
            color = palette::C_COMMENT;
            if id > block_comment_start + 1 && prev == '*' && ch == '/' {
                in_block_comment = false;
                colors.push(color);
                prev = '\0';
                id += 1;
                continue;
            }
        } else if let Some(q) = in_string {
            if ch == q && prev != '\\' {
                in_string = None;
            }
            color = palette::C_STRING;
        } else if ch == '/' && prev == '/' {
            in_line_comment = true;
            // Recolor the preceding '/' too (it was pushed as punctuation).
            if let Some(last) = colors.last_mut() {
                *last = palette::C_COMMENT;
            }
            color = palette::C_COMMENT;
        } else if ch == '*' && prev == '/' {
            in_block_comment = true;
            block_comment_start = id;
            if let Some(last) = colors.last_mut() {
                *last = palette::C_COMMENT;
            }
            color = palette::C_COMMENT;
        } else if ch == '#' && (id + 1 >= bytes.len() || (bytes[id + 1] != b'[' && !(bytes[id + 1] == b'!' && id + 2 < bytes.len() && bytes[id + 2] == b'['))) {
            if word_start_byte != usize::MAX {
                let wc = word_color_packed(&bytes[word_start_byte..id]);
                if wc == palette::C_KEYWORD {
                    colors[word_start_col..].fill(wc);
                }
                word_start_byte = usize::MAX;
            }
            in_line_comment = true;
            color = palette::C_COMMENT;
        } else if ch == '"' || ch == '\'' || ch == '`' {
            in_string = Some(ch);
            color = palette::C_STRING;
        } else if is_word_char(ch) {
            if word_start_byte == usize::MAX {
                word_start_byte = id;
                word_start_col = colors.len();
            }
            if bytes[word_start_byte].is_ascii_digit() {
                color = palette::C_NUMBER;
            }
        } else {
            if word_start_byte != usize::MAX {
                let wc = word_color_packed(&bytes[word_start_byte..id]);
                if wc == palette::C_KEYWORD {
                    colors[word_start_col..].fill(wc);
                }
                word_start_byte = usize::MAX;
            }
            color = palette::C_PUNCT;
        }
        colors.push(color);
        prev = ch;
        id += 1;
    }
    // The item's last word, when no newline ends it: until 2026-10-09 it was
    // never coloured here while `colorize_line_into` coloured it, so a file
    // ending in a keyword painted differently on the host and device paths.
    if word_start_byte != usize::MAX {
        let wc = word_color_packed(&bytes[word_start_byte..]);
        if wc == palette::C_KEYWORD {
            colors[word_start_col..].fill(wc);
        }
    }
    colors
}

/// Whether the colourisers push a colour for byte `b`: every byte but a
/// continuation byte (0x80..=0xBF) or an invalid lead (0xF8..=0xFF) — the
/// leader rule `colorize_leaders` and [`colorize_line_into`] both decode by.
/// The device Pass 2 counts leaders with it to find where a chunk's bytes
/// start inside a whole-line colour run (C17).
#[inline(always)]
pub fn is_colorizer_leader(b: u8) -> bool {
    b < 0x80 || b & 0xE0 == 0xC0 || b & 0xF0 == 0xE0 || b & 0xF8 == 0xF0
}

/// Colorize a single line's leaders into a reusable vector.
/// Clears `colors` and pushes one packed RGBA u32 per leader in `line_bytes`.
pub fn colorize_line_into(line_bytes: &[u8], colors: &mut Vec<u32>) {
    colors.clear();
    if is_pure_printable_ascii(line_bytes) {
        colors.resize(line_bytes.len(), 0);
        colorize_pure_ascii_line_slice(line_bytes, colors.as_mut_slice());
        return;
    }
    colors.reserve(line_bytes.len());
    let mut word_start_byte = usize::MAX;
    let mut word_start_col = 0usize;
    let is_block_continuation = is_block_comment_continuation_line(line_bytes);
    let mut in_line_comment = false;
    let mut in_block_comment = is_block_continuation;
    let mut block_comment_start = if is_block_continuation { 0 } else { usize::MAX };
    let mut in_string: Option<char> = None;
    let mut prev = '\n';

    let byte_at = |i: usize| -> u32 {
        if i < line_bytes.len() { line_bytes[i] as u32 } else { 0 }
    };
    let mut id = 0usize;
    while id < line_bytes.len() {
        let b0 = line_bytes[id] as u32;
        let (_len, ch) = if b0 < 0x80 {
            (1usize, b0 as u8 as char)
        } else if b0 & 0xE0 == 0xC0 {
            let cp = ((b0 & 0x1F) << 6) | (byte_at(id + 1) & 0x3F);
            (2usize, char::from_u32(cp).unwrap_or('\u{FFFD}'))
        } else if b0 & 0xF0 == 0xE0 {
            let cp = ((b0 & 0x0F) << 12) | ((byte_at(id + 1) & 0x3F) << 6) | (byte_at(id + 2) & 0x3F);
            (3usize, char::from_u32(cp).unwrap_or('\u{FFFD}'))
        } else if b0 & 0xF8 == 0xF0 {
            let cp = ((b0 & 0x07) << 18)
                | ((byte_at(id + 1) & 0x3F) << 12)
                | ((byte_at(id + 2) & 0x3F) << 6)
                | (byte_at(id + 3) & 0x3F);
            (4usize, char::from_u32(cp).unwrap_or('\u{FFFD}'))
        } else {
            id += 1;
            continue;
        };

        let mut color = palette::C_DEFAULT;
        if ch == '\t' {
            word_start_byte = usize::MAX;
        } else if in_line_comment {
            color = palette::C_COMMENT;
        } else if in_block_comment {
            color = palette::C_COMMENT;
            if id > block_comment_start + 1 && prev == '*' && ch == '/' {
                in_block_comment = false;
                colors.push(color);
                prev = '\0';
                id += 1;
                continue;
            }
        } else if let Some(q) = in_string {
            if ch == q && prev != '\\' {
                in_string = None;
            }
            color = palette::C_STRING;
        } else if ch == '/' && prev == '/' {
            in_line_comment = true;
            if let Some(last) = colors.last_mut() {
                *last = palette::C_COMMENT;
            }
            color = palette::C_COMMENT;
        } else if ch == '*' && prev == '/' {
            in_block_comment = true;
            block_comment_start = id;
            if let Some(last) = colors.last_mut() {
                *last = palette::C_COMMENT;
            }
            color = palette::C_COMMENT;
        } else if ch == '#' && (id + 1 >= line_bytes.len() || (line_bytes[id + 1] != b'[' && !(line_bytes[id + 1] == b'!' && id + 2 < line_bytes.len() && line_bytes[id + 2] == b'['))) {
            if word_start_byte != usize::MAX {
                let wc = word_color_packed(&line_bytes[word_start_byte..id]);
                if wc == palette::C_KEYWORD {
                    colors[word_start_col..].fill(wc);
                }
                word_start_byte = usize::MAX;
            }
            in_line_comment = true;
            color = palette::C_COMMENT;
        } else if ch == '"' || ch == '\'' || ch == '`' {
            in_string = Some(ch);
            color = palette::C_STRING;
        } else if is_word_char(ch) {
            if word_start_byte == usize::MAX {
                word_start_byte = id;
                word_start_col = colors.len();
            }
            if line_bytes[word_start_byte].is_ascii_digit() {
                color = palette::C_NUMBER;
            }
        } else {
            if word_start_byte != usize::MAX {
                let wc = word_color_packed(&line_bytes[word_start_byte..id]);
                if wc == palette::C_KEYWORD {
                    colors[word_start_col..].fill(wc);
                }
                word_start_byte = usize::MAX;
            }
            color = palette::C_PUNCT;
        }
        colors.push(color);
        prev = ch;
        id += 1;
    }
    if word_start_byte != usize::MAX {
        let wc = word_color_packed(&line_bytes[word_start_byte..]);
        if wc == palette::C_KEYWORD {
            colors[word_start_col..].fill(wc);
        }
    }
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

/// Fast-path syntax coloring for pure printable ASCII lines (0x20..=0x7E) writing into a slice.
/// Avoids vector resizing and heap allocations.
/// Returns the counts of `[DEFAULT, KEYWORD, NUMBER, STRING, COMMENT, PUNCT]` in the line.
#[inline]
pub fn colorize_pure_ascii_line_slice(line: &[u8], colors: &mut [u32]) -> [u32; 6] {
    debug_assert_eq!(line.len(), colors.len());
    let mut counts = [0u32; 6];
    if line.is_empty() {
        return counts;
    }

    let mut first_non_space = 0usize;
    while first_non_space < line.len() && line[first_non_space] == b' ' {
        first_non_space += 1;
    }

    // Fast path: Pure whitespace line
    if first_non_space == line.len() {
        colors.fill(palette::C_PUNCT);
        counts[5] = line.len() as u32;
        return counts;
    }

    let is_block_continuation = is_block_comment_continuation_line(line);

    // Fast path: Block comment continuation line without closing '*/'
    if is_block_continuation && memchr::memchr(b'/', line).is_none() {
        colors.fill(palette::C_COMMENT);
        counts[4] = line.len() as u32;
        return counts;
    }

    // Fast path: Full-line '//' comment
    if line[first_non_space..].starts_with(b"//") {
        if first_non_space > 0 {
            colors[..first_non_space].fill(palette::C_PUNCT);
            counts[5] = first_non_space as u32;
        }
        colors[first_non_space..].fill(palette::C_COMMENT);
        counts[4] = (line.len() - first_non_space) as u32;
        return counts;
    }

    // Fast path: Full-line '#' comment
    if line[first_non_space] == b'#' && (first_non_space + 1 >= line.len() || (line[first_non_space + 1] != b'[' && !(line[first_non_space + 1] == b'!' && first_non_space + 2 < line.len() && line[first_non_space + 2] == b'['))) {
        if first_non_space > 0 {
            colors[..first_non_space].fill(palette::C_PUNCT);
            counts[5] = first_non_space as u32;
        }
        colors[first_non_space..].fill(palette::C_COMMENT);
        counts[4] = (line.len() - first_non_space) as u32;
        return counts;
    }

    let mut word_start = usize::MAX;
    let mut in_block_comment = is_block_continuation;
    let mut block_comment_start = if is_block_continuation { 0 } else { usize::MAX };
    let mut in_string: Option<u8> = None;
    let mut prev = b'\n';

    let mut start_idx = 0usize;
    if !is_block_continuation && first_non_space > 0 {
        colors[..first_non_space].fill(palette::C_PUNCT);
        counts[5] = first_non_space as u32;
        start_idx = first_non_space;
        prev = b' ';
    }

    for i in start_idx..line.len() {
        let b = line[i];
        let mut color = palette::C_DEFAULT;
        if in_block_comment {
            color = palette::C_COMMENT;
            counts[4] += 1;
            if i > block_comment_start + 1 && prev == b'*' && b == b'/' {
                in_block_comment = false;
                colors[i] = color;
                prev = b'\0';
                continue;
            }
        } else if let Some(q) = in_string {
            if b == q && prev != b'\\' {
                in_string = None;
            }
            color = palette::C_STRING;
            counts[3] += 1;
        } else if b == b'/' && prev == b'/' {
            if i > 0 {
                colors[i - 1..].fill(palette::C_COMMENT);
                counts[5] -= 1;
                counts[4] += (line.len() - (i - 1)) as u32;
            } else {
                colors.fill(palette::C_COMMENT);
                counts[4] += line.len() as u32;
            }
            break;
        } else if b == b'*' && prev == b'/' {
            in_block_comment = true;
            block_comment_start = i;
            if i > 0 {
                colors[i - 1] = palette::C_COMMENT;
                counts[5] -= 1;
                counts[4] += 1;
            }
            color = palette::C_COMMENT;
            counts[4] += 1;
        } else if b == b'#' && (i + 1 >= line.len() || (line[i + 1] != b'[' && !(line[i + 1] == b'!' && i + 2 < line.len() && line[i + 2] == b'['))) {
            if word_start != usize::MAX {
                let wc = word_color_packed(&line[word_start..i]);
                if wc == palette::C_KEYWORD {
                    colors[word_start..i].fill(wc);
                    let word_len = (i - word_start) as u32;
                    counts[0] -= word_len;
                    counts[1] += word_len;
                }
                word_start = usize::MAX;
            }
            colors[i..].fill(palette::C_COMMENT);
            counts[4] += (line.len() - i) as u32;
            break;
        } else if b == b'"' || b == b'\'' || b == b'`' {
            in_string = Some(b);
            color = palette::C_STRING;
            counts[3] += 1;
        } else if ASCII_WORD_CHAR[b as usize] {
            if word_start == usize::MAX {
                word_start = i;
            }
            if line[word_start].is_ascii_digit() {
                color = palette::C_NUMBER;
                counts[2] += 1;
            } else {
                counts[0] += 1;
            }
        } else {
            if word_start != usize::MAX {
                let wc = word_color_packed(&line[word_start..i]);
                if wc == palette::C_KEYWORD {
                    colors[word_start..i].fill(wc);
                    let word_len = (i - word_start) as u32;
                    counts[0] -= word_len;
                    counts[1] += word_len;
                }
                word_start = usize::MAX;
            }
            color = palette::C_PUNCT;
            counts[5] += 1;
        }
        colors[i] = color;
        prev = b;
    }
    if word_start != usize::MAX {
        let wc = word_color_packed(&line[word_start..]);
        if wc == palette::C_KEYWORD {
            colors[word_start..].fill(wc);
            let word_len = (line.len() - word_start) as u32;
            counts[0] -= word_len;
            counts[1] += word_len;
        }
    }
    counts
}

/// `[DEFAULT, KEYWORD, NUMBER, STRING, COMMENT, PUNCT]` counts of a colour
/// run — what [`colorize_pure_ascii_line_slice`] returns for the run it
/// writes. The device Pass 2 uses it for a cut line's share, whose colours
/// come from the whole line rather than from a colouriser call (C17).
pub fn palette_counts(colors: &[u32]) -> [u32; 6] {
    let mut counts = [0u32; 6];
    for &c in colors {
        let k = match c {
            palette::C_KEYWORD => 1,
            palette::C_NUMBER => 2,
            palette::C_STRING => 3,
            palette::C_COMMENT => 4,
            palette::C_PUNCT => 5,
            _ => 0,
        };
        counts[k] += 1;
    }
    counts
}

/// Fast-path syntax coloring for pure printable ASCII lines (0x20..=0x7E).
/// Avoids UTF-8 decode branches and char conversions.
/// Returns the counts of `[DEFAULT, KEYWORD, NUMBER, STRING, COMMENT, PUNCT]` in the line.
#[inline]
pub fn colorize_pure_ascii_line(line: &[u8], colors: &mut Vec<u32>) -> [u32; 6] {
    if colors.len() < line.len() {
        colors.resize(line.len(), palette::C_DEFAULT);
    } else {
        colors.truncate(line.len());
    }
    colorize_pure_ascii_line_slice(line, colors)
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

    /// One colour per byte `is_colorizer_leader` accepts, over every byte
    /// value in every position (continuations, overlong and invalid leads).
    /// A lead truncated at the end of a line or item (malformed.pipe.bin's
    /// class) panicked the per-line colouriser — every syntax-paint device
    /// load of such a file — until 2026-10-09; and the whole-item one left an
    /// unterminated last word uncoloured. Both now agree on both.
    #[test]
    fn colorizers_agree_on_truncated_leads_and_unterminated_words() {
        for text in [&b"ab\xC3"[..], b"x \xE2\x82", b"fn \xF0\x9F\x98", b"let x = return", b"12"] {
            let mut line = Vec::new();
            colorize_line_into(text, &mut line);
            assert_eq!(colorize_leaders(text), line, "{text:?}");
        }
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

    /// The histogram is the ASCII colouriser's own count, line for line.
    #[test]
    fn palette_counts_match_the_ascii_colorizer() {
        for line in [
            &b"fn main() { let x = 0x1F; // done"[..],
            b"    /* open block",
            b"   ",
            b"\"a string\" + 'c' + `t` - 12.5e3",
            b"#include <stdio.h>",
            b"return while for",
        ] {
            let mut colors = vec![0u32; line.len()];
            let counts = colorize_pure_ascii_line_slice(line, &mut colors);
            assert_eq!(palette_counts(&colors), counts, "{:?}", std::str::from_utf8(line));
        }
    }

    #[test]
    fn colorizer_leader_rule_counts_the_colours() {
        let mut bytes: Vec<u8> = (0u8..=255).collect();
        bytes.extend_from_slice("fn é 世 😀 // x".as_bytes());
        bytes.retain(|&b| b != b'\n');
        let mut colors = Vec::new();
        colorize_line_into(&bytes, &mut colors);
        assert_eq!(colors.len(), bytes.iter().filter(|&&b| is_colorizer_leader(b)).count());
        assert_eq!(colorize_leaders(&bytes).len(), colors.len());
    }

    #[test]
    fn colorize_line_into_matches_colorize_leaders() {
        let sample = b"const x = 42;\nlet y = \"hello world\"; // comment\n# python comment\n/* block */\n * star line\n */\n#[derive(Debug)]\nfn test() -> bool { true }\n";
        let full_colors = colorize_leaders(sample);

        let mut reconstructed = Vec::new();
        let mut line_buf = Vec::new();
        for line in sample.split(|&b| b == b'\n') {
            colorize_line_into(line, &mut line_buf);
            reconstructed.extend_from_slice(&line_buf);
            reconstructed.push(palette::C_DEFAULT); // newline color in colorize_leaders
        }
        reconstructed.pop(); // remove extra newline from trailing empty split

        assert_eq!(reconstructed.len(), full_colors.len());
        for (i, (&rec, &orig)) in reconstructed.iter().zip(&full_colors).enumerate() {
            assert_eq!(rec, orig, "mismatch at index {i}");
        }
    }

    #[test]
    fn colorize_pure_ascii_line_matches_colorize_line_into() {
        let lines = [
            b"const x = 42;".as_slice(),
            b"let y = \"hello world\"; // comment".as_slice(),
            b"fn test() -> bool { true }".as_slice(),
            b"for (let i = 0; i < 10; i++) { sum += i; }".as_slice(),
            b"// entire line is comment".as_slice(),
            b"# python or shell comment".as_slice(),
            b"x = 10 # trailing hash comment".as_slice(),
            b"/* block comment */".as_slice(),
            b" * continuation comment".as_slice(),
            b" */".as_slice(),
            b"#[derive(Clone)]".as_slice(),
            b"/* comment */ let x = 42;".as_slice(),
            b"\"unclosed string".as_slice(),
            // A word that runs into a string: the fast path checks the word up
            // to its flush point, so `return` here is never a keyword. The
            // generic colourisers checked it up to its last word byte and
            // painted `return"$"` keyword, string included (C17, 2026-10-09).
            b"var e={};return\"$\"+s".as_slice(),
            b"return'x'".as_slice(),
            b"if`t`abc;".as_slice(),
            b"1\"x\"2 + 3".as_slice(),
        ];
        // AGAINST `colorize_leaders`, the generic colouriser: until 2026-10-09
        // this compared against `colorize_line_into`, which hands every one
        // of these pure-ASCII lines to the fast path itself, so it compared
        // the fast path with itself and could not fail.
        let mut fast = Vec::new();
        for &line in &lines {
            colorize_pure_ascii_line(line, &mut fast);
            assert_eq!(colorize_leaders(line), fast, "mismatch on line: {}", String::from_utf8_lossy(line));
        }
    }

    #[test]
    fn colorize_pure_ascii_line_slice_counts_match_colors() {
        let lines = [
            b"const x = 42;".as_slice(),
            b"let y = \"hello world\"; // comment".as_slice(),
            b"fn test() -> bool { true }".as_slice(),
            b"for (let i = 0; i < 10; i++) { sum += i; }".as_slice(),
            b"// entire line is comment".as_slice(),
            b"# python or shell comment".as_slice(),
            b"x = 10 # trailing hash comment".as_slice(),
            b"/* block comment */".as_slice(),
            b" * continuation comment".as_slice(),
            b" */".as_slice(),
            b"#[derive(Clone)]".as_slice(),
            b"/* comment */ let x = 42;".as_slice(),
            b"\"unclosed string".as_slice(),
            b"   12345 67890 abc def return function   ".as_slice(),
        ];
        let mut colors = Vec::new();
        for &line in &lines {
            colors.resize(line.len(), 0);
            let counts = colorize_pure_ascii_line_slice(line, &mut colors);
            let mut manual_counts = [0u32; 6];
            for &c in &colors {
                match c {
                    palette::C_DEFAULT => manual_counts[0] += 1,
                    palette::C_KEYWORD => manual_counts[1] += 1,
                    palette::C_NUMBER  => manual_counts[2] += 1,
                    palette::C_STRING  => manual_counts[3] += 1,
                    palette::C_COMMENT => manual_counts[4] += 1,
                    palette::C_PUNCT   => manual_counts[5] += 1,
                    _ => panic!("unknown color: {:X}", c),
                }
            }
            assert_eq!(counts, manual_counts, "mismatch on line: {}", String::from_utf8_lossy(line));
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



