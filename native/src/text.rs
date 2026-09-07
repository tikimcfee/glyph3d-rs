//! Stage C — text staging: UTF-8 file → glyph instance slots on a monospace grid.
//!
//! Byte→codepoint is plain UTF-8 decoding (`str::chars` — continuation bytes
//! produce no glyph, per FORMAT.md); codepoint→slot is the codepoints.bin trie.
//! Layout uses the primary-font metrics: cell advance 1229 fu, em height
//! 2320 fu, scaled to a world cell height of CELL_HEIGHT_WORLD. Per-glyph color
//! comes from a small syntax-ish tokenizer (keywords / numbers / strings /
//! comments / punctuation).
//!
//! Missing / blank / bitmap (emoji) codepoints occupy their advance but emit no
//! instance — emoji bitmaps were not exported, so emoji render as blank space.

use std::path::Path;

use crate::atlas::{Atlas, FLAG_BITMAP, FLAG_MISSING};
use crate::glyph_scene::{seg_tint, GlyphInstance, GroupRow, PickContext, SegCull};
use crate::layout::{GlyphArena, ItemPlacement};

/// World-space height of one em cell (ascender→descender). Everything scales
/// from this; only the ratio to the font units matters.
pub const CELL_HEIGHT_WORLD: f32 = 1.0;
/// Line pitch as a multiple of the em height (looser than 1.0 for readability).
pub const LINE_HEIGHT_FACTOR: f32 = 1.25;
/// Tab stop width in cells.
pub const TAB_CELLS: u32 = 4;

/// sRGB display-value palette (VS Code dark+ flavored).
mod palette {
    pub const DEFAULT: [u8; 3] = [212, 212, 212]; // identifiers, plain text
    pub const KEYWORD: [u8; 3] = [197, 134, 192]; // const/let/fn/return…
    pub const NUMBER: [u8; 3] = [181, 206, 168];
    pub const STRING: [u8; 3] = [206, 145, 120];
    pub const COMMENT: [u8; 3] = [106, 153, 85];
    pub const PUNCT: [u8; 3] = [128, 128, 128];
}

const KEYWORDS: &[&str] = &[
    // JS/TS
    "const", "let", "var", "function", "return", "if", "else", "for", "while", "import",
    "export", "from", "class", "extends", "new", "this", "typeof", "instanceof", "switch",
    "case", "break", "continue", "default", "try", "catch", "finally", "throw", "async",
    "await", "yield", "of", "in", "do", "null", "undefined", "true", "false", "static",
    "get", "set", "delete", "void",
    // Rust
    "fn", "pub", "mod", "use", "struct", "enum", "impl", "trait", "where", "match",
    "loop", "move", "mut", "ref", "self", "Self", "crate", "super", "unsafe", "dyn",
    "Some", "None", "Ok", "Err",
];

fn pack_rgba8(rgb: [u8; 3], a: u8) -> u32 {
    rgb[0] as u32 | (rgb[1] as u32) << 8 | (rgb[2] as u32) << 16 | (a as u32) << 24
}

/// Per-position working glyph before instance tiling: (col, row, slot, color).
struct Cell {
    col: u32,
    row: u32,
    slot: u32,
    color: [u8; 3],
}

/// The staged result: instances + group rows + world bounds (for camera fit).
pub struct StagedText {
    pub instances: Vec<GlyphInstance>,
    pub groups: Vec<GroupRow>,
    /// (min, max) of the laid-out text block(s) in world units, including
    /// DEPTH — WrapBack spends wraps in z, so a block's extent is not planar.
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
    pub codepoints_decoded: usize,
    pub glyphs_emitted: usize,
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
}

/// Stage F: one cull segment covering a whole staged block (the text/engine
/// scenes don't need per-group granularity — their instance counts are small).
fn cover_segment(instances: &[GlyphInstance], min: [f32; 3], max: [f32; 3]) -> SegCull {
    SegCull {
        min,
        max,
        slot_base: 0,
        slot_count: instances.len() as u32,
        tint: seg_tint(instances, max[0] - min[0], max[1] - min[1]),
    }
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '$'
}

/// Stage a UTF-8 text file, tiled `copies` times (each copy its own group,
/// offset in a grid of blocks — exercises the group table and is the stress
/// path toward ≥1M instances).
pub fn stage_file(atlas: &Atlas, path: &Path, copies: u32) -> StagedText {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read text file {}: {e}", path.display()));

    let fu_per_world = atlas.metrics.em_height_fu as f32 / CELL_HEIGHT_WORLD;
    let cell_w = atlas.metrics.advance_fu as f32 / fu_per_world; // world advance
    let line_h = CELL_HEIGHT_WORLD * LINE_HEIGHT_FACTOR;

    // --- lay out one copy on a monospace grid ------------------------------
    let mut glyphs: Vec<Cell> = Vec::new();
    let mut codepoints_decoded = 0usize;
    let mut missing_or_bitmap = 0usize;

    let mut col: u32 = 0;
    let mut row: u32 = 0;
    let mut max_col: u32 = 0;

    // Tokenizer state. `word_start` indexes `glyphs` where the current
    // identifier/number run began, so a keyword can recolor its span at flush.
    let mut word = String::new();
    let mut word_start: usize = 0;
    let mut in_comment = false;
    let mut in_string: Option<char> = None;
    let mut prev = '\0';

    let push_char = |glyphs: &mut Vec<Cell>,
                         ch: char,
                         col: u32,
                         row: u32,
                         color: [u8; 3],
                         atlas: &Atlas,
                         codepoints_decoded: &mut usize,
                         missing_or_bitmap: &mut usize|
     -> u32 {
        *codepoints_decoded += 1;
        let entry = atlas.lookup(ch as u32);
        let advance_cells = (entry.advance_fu.max(0) as u32 + atlas.metrics.advance_fu / 2)
            / atlas.metrics.advance_fu;
        if entry.flags & (FLAG_MISSING | FLAG_BITMAP) != 0 {
            *missing_or_bitmap += 1;
        } else if entry.glyph_id != 0 {
            glyphs.push(Cell {
                col,
                row,
                slot: entry.glyph_id,
                color,
            });
        }
        advance_cells.max(1)
    };

    for ch in text.chars() {
        if ch == '\n' {
            // Flush any open word as keyword/number/plain.
            if !word.is_empty() {
                let color = word_color(&word);
                if color != palette::DEFAULT {
                    for g in &mut glyphs[word_start..] {
                        g.color = color;
                    }
                }
                word.clear();
            }
            in_comment = false;
            in_string = None;
            max_col = max_col.max(col);
            col = 0;
            row += 1;
            prev = ch;
            continue;
        }
        if ch == '\t' {
            word.clear();
            col += TAB_CELLS - (col % TAB_CELLS);
            prev = ch;
            continue;
        }

        let color = if in_comment {
            palette::COMMENT
        } else if let Some(q) = in_string {
            if ch == q && prev != '\\' {
                in_string = None;
            }
            palette::STRING
        } else if ch == '/' && prev == '/' {
            in_comment = true;
            // Recolor the preceding '/' too.
            if let Some(g) = glyphs.last_mut() {
                if g.col + 1 == col && g.row == row {
                    g.color = palette::COMMENT;
                }
            }
            palette::COMMENT
        } else if ch == '"' || ch == '\'' || ch == '`' {
            in_string = Some(ch);
            palette::STRING
        } else if is_word_char(ch) {
            if word.is_empty() {
                word_start = glyphs.len();
            }
            word.push(ch);
            if word.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                palette::NUMBER
            } else {
                palette::DEFAULT
            }
        } else {
            // Word boundary: flush as keyword/number/plain.
            if !word.is_empty() {
                let wc = word_color(&word);
                if wc != palette::DEFAULT {
                    for g in &mut glyphs[word_start..] {
                        g.color = wc;
                    }
                }
                word.clear();
            }
            palette::PUNCT
        };

        col += push_char(
            &mut glyphs,
            ch,
            col,
            row,
            color,
            atlas,
            &mut codepoints_decoded,
            &mut missing_or_bitmap,
        );
        prev = ch;
    }
    max_col = max_col.max(col);

    let block_w = max_col as f32 * cell_w;
    let block_h = (row as f32 + 1.0) * line_h;

    // --- tile copies into groups -------------------------------------------
    let copies = copies.max(1);
    let grid_cols = (copies as f32).sqrt().ceil() as u32;
    let gap = cell_w * 4.0;

    let mut instances = Vec::with_capacity(glyphs.len() * copies as usize);
    let mut groups = Vec::with_capacity(copies as usize);

    for copy in 0..copies {
        let gx = copy % grid_cols;
        let gy = copy / grid_cols;
        groups.push(GroupRow::identity([
            gx as f32 * (block_w + gap),
            -(gy as f32 * (block_h + gap)),
            0.0,
        ]));
        for g in &glyphs {
            instances.push(GlyphInstance {
                pos: [g.col as f32 * cell_w, -(g.row as f32) * line_h, 0.0],
                glyph_id: g.slot,
                row: g.row,
                col: g.col,
                color: pack_rgba8(g.color, 255),
                group_id: copy,
                advance: cell_w,
                height: CELL_HEIGHT_WORLD,
                flags: 0,
                _pad: 0,
            });
        }
    }

    let total_w = grid_cols as f32 * (block_w + gap) - gap;
    let grid_rows = copies.div_ceil(grid_cols);
    let total_h = grid_rows as f32 * (block_h + gap) - gap;

    // Depth measured from the instances rather than assumed flat: this path
    // stages copies of a block, and whether those copies carry z is a property
    // of how they were laid out, not something to take on faith here.
    let (z_lo, z_hi) = instances.iter().fold((0.0f32, 0.0f32), |(lo, hi), i| {
        (lo.min(i.pos[2]), hi.max(i.pos[2]))
    });
    let segments = vec![cover_segment(
        &instances,
        [0.0, -total_h, z_lo],
        [total_w, line_h, z_hi],
    )];

    StagedText {
        glyphs_emitted: instances.len(),
        instances,
        groups,
        bounds_min: [0.0, -total_h, z_lo],
        bounds_max: [total_w, line_h, z_hi],
        codepoints_decoded,
        missing_or_bitmap,
        focus_bounds: None,
        segments,
        pick: None,
    }
}

fn word_color(word: &str) -> [u8; 3] {
    if KEYWORDS.contains(&word) {
        palette::KEYWORD
    } else if word.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        palette::NUMBER
    } else {
        palette::DEFAULT
    }
}

// ── Stage E1 — engine-convention paths ──────────────────────────────────────
//
// `stage_file` above is the PRODUCTION staging: cell-quantized columns, tab
// stops, and blank/missing/bitmap glyphs dropped from the instance stream.
// The Mojo engine (glyph_pipeline.mojo) has different conventions by design:
// it emits ONE record per UTF-8 leader byte (newlines, blanks, and missing
// codepoints included), accumulates X as an f64 running sum of the per-glyph
// f32 advances (the oracle's float discipline), counts COL in leader glyphs
// per line (no tab stops, no double-width cells), and HEIGHT is the constant
// cell height. `reference_layout` below re-implements THAT fold on the CPU,
// reading the same atlas trie, so `--engine-check` can diff the FFI records
// against an independent implementation bit-for-bit.

use crate::atlas::TrieTable;
use crate::layout::GlyphRecord;

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

/// The font-units→world conversion, shared with tools/gen_real_trie.py:
/// `fround(fu * CELL_HEIGHT_WORLD / em_height_fu)`, computed in f64 and
/// narrowed once — the same bits the generator wrote into engine-trie.bin.
fn fu_to_world(fu: i32, em_height_fu: u32) -> f32 {
    (fu as f64 * CELL_HEIGHT_WORLD as f64 / em_height_fu as f64) as f32
}

/// Replicate the engine's decode + fold (wrap 0, no pages) for one item.
///
/// Byte-level UTF-8, exactly as `decode_and_resolve`: the lead byte picks the
/// sequence length, continuation/invalid bytes are non-leaders (no record),
/// and the codepoint assembles from masked payload bits WITHOUT validating
/// the continuation bytes (bounds-checked reads return 0 past the end).
/// DO NOT MERGE THIS INTO `fold::run_pipeline`, even though that function now
/// computes a strict superset of it. They look like a dual code path and are
/// not: their VALUE is that they have different lineage.
///
/// `reference_layout` was written independently against the TSL kernel
/// (Stage E1). `fold.rs` was ported from `engine/glyph_pipeline.mojo`. So
/// `--engine-check`, which runs the Mojo engine over real source with the real
/// atlas and diffs it against this, is comparing two implementations that do
/// NOT share a parent. Point it at `fold.rs` instead and it becomes the Mojo
/// checked against a port of the Mojo — an oracle sharing a fault with its
/// port, which is the one failure mode a diff cannot see.
///
/// The fixtures cannot cover that gap either: they gate both implementations,
/// so anything they miss, both miss together. `--engine-check`'s input (40 KB of
/// this crate's own source, the real atlas trie) is outside the corpus entirely,
/// and independence is the whole reason it is worth running there.
///
/// Same rule, same reason as `to_world` / `fu_to_world`: a formula written twice
/// on purpose stops being a check the moment it is written once.
pub fn reference_layout<T: ResolveGlyph + ?Sized>(
    trie: &T,
    bytes: &[u8],
    origin: [f64; 3],
    line_height: f64,
) -> Vec<RefGlyph> {
    let mut out = Vec::new();
    let mut line_adv: f64 = 0.0; // f64 chain — the oracle's truth-layer prefix
    let mut row: u32 = 0;
    let mut col: u32 = 0;
    let mut id = 0usize;
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
        let advance = e.advance;
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
            line_adv += advance as f64;
        }
        id += 1;
    }
    out
}

/// fold_leaders output: per-engine-record (byte offset, codepoint), folded
/// ROW, folded COL, source line — four parallel vectors in record order.
pub type FoldTables = (Vec<(usize, u32)>, Vec<u32>, Vec<u32>, Vec<u32>);

/// Stage G — decode the UTF-8 leaders of `bytes` and fold them with the
/// engine's exact conventions (glyph_pipeline.mojo, THE FOLD): COL is the raw
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

/// Diff engine records against the CPU reference. Returns Ok(()) on a
/// bit-exact match (counts AND measures, compared as bits — the repo's
/// discipline), or a human-readable first-mismatch report.
pub fn diff_records(records: &[GlyphRecord], expected: &[RefGlyph]) -> Result<(), String> {
    if records.len() != expected.len() {
        return Err(format!(
            "record count: engine {} vs reference {}",
            records.len(),
            expected.len()
        ));
    }
    let mut bad = 0usize;
    let mut report = String::new();
    for (i, (r, e)) in records.iter().zip(expected.iter()).enumerate() {
        let measures_match = r.measures.iter()
            .zip([e.x, e.y, e.z, e.advance, e.height].iter())
            .all(|(a, b)| a.to_bits() == b.to_bits());
        let counts_match = r.counts == [e.glyph_id, e.row, e.col];
        if !measures_match || !counts_match {
            bad += 1;
            if bad <= 10 {
                report.push_str(&format!(
                    "  rec[{i}]: engine [X={} Y={} Z={} ADV={} H={} | GID={} ROW={} COL={}]\n\
                     \x20          expect [X={} Y={} Z={} ADV={} H={} | GID={} ROW={} COL={}]\n",
                    r.x(), r.y(), r.z(), r.advance(), r.height(),
                    r.glyph_id(), r.row(), r.col(),
                    e.x, e.y, e.z, e.advance, e.height, e.glyph_id, e.row, e.col,
                ));
            }
        }
    }
    if bad > 0 {
        Err(format!("{bad}/{} records differ:\n{report}", records.len()))
    } else {
        Ok(())
    }
}

/// Present one item's staged arena as a renderable scene.
///
/// Since the layout seam landed, compaction and the extents happen behind it,
/// so this does no arithmetic at all: it names which extent the camera frames
/// and packages the scene. The INK extent is the right one — the quads of the
/// glyphs that survived, not the page rectangle they were laid out on — and
/// choosing between them is exactly the kind of policy the seam declines to
/// have on the caller's behalf.
///
/// An item with no ink has no meaningful frame; a unit square is this caller's
/// answer, unchanged from when the loop lived here.
pub fn stage_records(arena: GlyphArena, placement: &ItemPlacement) -> StagedText {
    let (min, max) = if arena.is_empty() {
        ([0.0, 0.0, 0.0], [1.0, 1.0, 0.0])
    } else {
        (placement.ink.min, placement.ink.max)
    };
    let instances = arena.into_instances();

    StagedText {
        glyphs_emitted: instances.len(),
        groups: vec![GroupRow::identity([0.0; 3])],
        segments: vec![cover_segment(&instances, min, max)],
        instances,
        bounds_min: min,
        bounds_max: max,
        codepoints_decoded: placement.record_count as usize,
        missing_or_bitmap: (placement.record_count - placement.slot_count) as usize,
        focus_bounds: None,
        pick: None,
    }
}

/// Stage E2 — per-leader syntax colors for one file, in ENGINE record order
/// (one entry per UTF-8 leader byte, exactly the records `compact` emits, so
/// `colors[i]` paints `records[i]`). The tokenizer is `stage_file`'s —
/// keywords / numbers / strings / line comments / punctuation — a heuristic
/// paint layer over the engine's layout; leader classification is the same
/// byte-level logic as `reference_layout`.
pub fn colorize_leaders(bytes: &[u8]) -> Vec<u32> {
    let mut colors: Vec<u32> = Vec::with_capacity(bytes.len());
    let mut word = String::new();
    let mut word_start = 0usize;
    let mut in_comment = false;
    let mut in_string: Option<char> = None;
    let mut prev = '\0';

    let byte_at = |i: usize| -> u32 {
        if i < bytes.len() { bytes[i] as u32 } else { 0 }
    };
    let mut id = 0usize;
    while id < bytes.len() {
        let b0 = bytes[id] as u32;
        let n = if b0 & 0x80 == 0x00 {
            1usize
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
            id += 1; // continuation/invalid byte: no record, no color
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
        let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');

        let mut color = palette::DEFAULT;
        if ch == '\n' {
            if !word.is_empty() {
                let wc = word_color(&word);
                if wc != palette::DEFAULT {
                    for c in &mut colors[word_start..] {
                        *c = pack_rgba8(wc, 255);
                    }
                }
                word.clear();
            }
            in_comment = false;
            in_string = None;
        } else if ch == '\t' {
            word.clear();
        } else if in_comment {
            color = palette::COMMENT;
        } else if let Some(q) = in_string {
            if ch == q && prev != '\\' {
                in_string = None;
            }
            color = palette::STRING;
        } else if ch == '/' && prev == '/' {
            in_comment = true;
            // Recolor the preceding '/' too (it was pushed as punctuation).
            if let Some(last) = colors.last_mut() {
                *last = pack_rgba8(palette::COMMENT, 255);
            }
            color = palette::COMMENT;
        } else if ch == '"' || ch == '\'' || ch == '`' {
            in_string = Some(ch);
            color = palette::STRING;
        } else if is_word_char(ch) {
            if word.is_empty() {
                word_start = colors.len();
            }
            word.push(ch);
            if word.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                color = palette::NUMBER;
            }
        } else {
            if !word.is_empty() {
                let wc = word_color(&word);
                if wc != palette::DEFAULT {
                    for c in &mut colors[word_start..] {
                        *c = pack_rgba8(wc, 255);
                    }
                }
                word.clear();
            }
            color = palette::PUNCT;
        }
        colors.push(pack_rgba8(color, 255));
        prev = ch;
        id += 1;
    }
    colors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold::{run_pipeline, Item, F_LEADER};
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
}
