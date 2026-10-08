//! Stage C — text staging: UTF-8 file → glyph instance slots on a monospace grid.
//!
//! Byte→codepoint is plain UTF-8 decoding (`str::chars` — continuation bytes
//! produce no glyph, per FORMAT.md); codepoint→slot is the codepoints.bin trie.
//! Layout uses the primary-font metrics: cell advance 1229 fu, em height
//! 2320 fu, scaled to a world cell height of CELL_HEIGHT_WORLD. Per-glyph color
//! comes from a small syntax-ish tokenizer (keywords / numbers / strings /
//! comments / punctuation).
//!
//! Missing / blank codepoints occupy their advance but emit no instance; bitmap
//! (emoji) codepoints stage a real slot from the emoji sheet like any other
//! glyph — the shader's mode-1 branch draws them (the `emoji` golden pins it).

use std::path::Path;

use crate::atlas::{Atlas, FLAG_MISSING};
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

/// Per-position working glyph before instance tiling: (col, row, slot, color).
struct Cell {
    col: u32,
    row: u32,
    slot: u32,
    color: [u8; 3],
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
    /// GPU layout parameters per item (file) for Derived render mode.
    pub item_params: Vec<glyph_field::ItemParamsGpu>,
}

/// Stage F: one cull segment covering a whole staged block (the text/engine
/// scenes don't need per-group granularity — their instance counts are small).
/// Divides into sub-blocks if instance count exceeds 512 for fine-grained culling.
pub fn cover_segment(instances: &[GlyphInstance], min: [f32; 3], max: [f32; 3], slot_ink: &[Option<[f32; 4]>]) -> SegCull {
    const SUBSEG_BLOCK_SIZE: usize = 512;
    let mut blocks = Vec::new();
    if instances.len() > SUBSEG_BLOCK_SIZE {
        for (b_idx, chunk) in instances.chunks(SUBSEG_BLOCK_SIZE).enumerate() {
            let b_start = b_idx * SUBSEG_BLOCK_SIZE;
            let mut min_x = f32::INFINITY;
            let mut min_y = f32::INFINITY;
            let mut min_z = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            let mut max_z = f32::NEG_INFINITY;
            for s in chunk {
                let qw = s.advance.max(s.height);
                min_x = min_x.min(s.pos[0]);
                max_x = max_x.max(s.pos[0] + qw);
                min_y = min_y.min(s.pos[1] - 0.5 * s.height);
                max_y = max_y.max(s.pos[1] + 0.5 * s.height);
                min_z = min_z.min(s.pos[2]);
                max_z = max_z.max(s.pos[2]);
            }
            if min_x <= max_x && min_y <= max_y {
                blocks.push(crate::glyph_scene::BlockCull {
                    min: [min_x - 0.3, min_y - 0.5, min_z],
                    max: [max_x + 0.6, max_y + 0.75, max_z],
                    slot_base: b_start as u32,
                    slot_count: chunk.len() as u32,
                });
            }
        }
    }
    SegCull {
        min,
        max,
        slot_base: 0,
        slot_count: instances.len() as u32,
        tint: seg_tint(instances, max[0] - min[0], max[1] - min[1], slot_ink),
        blocks,
    }
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

/// Stage a UTF-8 text file, tiled `copies` times (each copy its own group,
/// offset in a grid of blocks — exercises the group table and is the stress
/// path toward ≥1M instances).
///
/// `cluster_mode` selects the sequence pass on the char stream (the engine's
/// glyph_cluster.mojo is the same rule over the same table). Leader (the
/// default) stages one cell per codepoint, as this path always has.
pub fn stage_file(
    atlas: &Atlas,
    path: &Path,
    copies: u32,
    cluster_mode: crate::fold::ClusterMode,
) -> StagedText {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read text file {}: {e}", path.display()));

    let fu_per_world = atlas.metrics.em_height_fu as f32 / CELL_HEIGHT_WORLD;
    let cell_w = atlas.metrics.advance_fu as f32 / fu_per_world; // world advance
    let line_h = CELL_HEIGHT_WORLD * LINE_HEIGHT_FACTOR;
    // The cluster head's advance in cells — the same narrowing push_char
    // applies to the trie's per-codepoint advances (2 by the export rule),
    // derived from the trie so a baked-advance change moves every twin.
    let bitmap_cells = ((atlas.trie.bitmap_advance_fu.max(0) as u32
        + atlas.metrics.advance_fu / 2)
        / atlas.metrics.advance_fu)
        .max(1);

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
        // Bitmap (emoji) slots are STAGED like any other glyph since
        // 2026-09-10: they carry a real slot id and the shader's mode-1 branch
        // draws them from the sheet. Only MISSING is dropped. `advance_cells`
        // already yields 2 for them (the trie carries the doubled advance).
        if entry.flags & FLAG_MISSING != 0 {
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

    let chars: Vec<char> = text.chars().collect();
    let mut ci = 0usize;
    while ci < chars.len() {
        let ch = chars[ci];
        // THE SEQUENCE PASS, on the char stream (cluster mode only): a leader
        // that starts a table sequence resolves to its one slot — the same
        // longest-prefix rule as the engine's, over the same table (the
        // engine's is glyph_cluster.mojo; the oracle's is resolveClusters).
        if cluster_mode == crate::fold::ClusterMode::Cluster {
            let cp = ch as u32;
            if crate::fold::is_static_zero_cp(cp) {
                // ZWJ / variation selectors / tag characters never occupy a cell.
                prev = ch;
                ci += 1;
                continue;
            }
            if atlas.trie.starts_a_sequence(cp) {
                // Probe forward: up to seq_max effective members, VS16 dropped
                // from the key (the font's GSUB strips it) but riding the span.
                let mut key = vec![cp];
                let mut span = 1usize;
                while ci + span < chars.len() && key.len() < atlas.trie.seq_max as usize {
                    let c2 = chars[ci + span] as u32;
                    if c2 == 0x0A || c2 == 0xFE0E {
                        break;
                    }
                    if c2 != 0xFE0F {
                        key.push(c2);
                    }
                    span += 1;
                }
                let mut best = None;
                for len in (2..=key.len()).rev() {
                    if let Some(slot) = atlas.trie.sequence_lookup(&key[..len]) {
                        best = Some((len, slot));
                        break;
                    }
                }
                if let Some((need, slot)) = best {
                    // The span runs through the char that gave the key's last
                    // member — count key-consumers, so skipped VS16s stay inside.
                    let mut covered = 0usize;
                    let mut need = need;
                    while need > 0 {
                        if (chars[ci + covered] as u32) != 0xFE0F {
                            need -= 1;
                        }
                        covered += 1;
                    }
                    // Flush an open word exactly like the word-boundary branch.
                    if !word.is_empty() {
                        let wc = word_color(&word);
                        if wc != palette::DEFAULT {
                            for g in &mut glyphs[word_start..] {
                                g.color = wc;
                            }
                        }
                        word.clear();
                    }
                    let color = if in_comment {
                        palette::COMMENT
                    } else if in_string.is_some() {
                        palette::STRING
                    } else {
                        palette::PUNCT
                    };
                    glyphs.push(Cell { col, row, slot, color });
                    codepoints_decoded += covered;
                    col += bitmap_cells; // the head advance, derived at staging
                    prev = chars[ci + covered - 1];
                    ci += covered;
                    continue;
                }
            }
        }
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
            ci += 1;
            continue;
        }
        if ch == '\t' {
            word.clear();
            col += TAB_CELLS - (col % TAB_CELLS);
            prev = ch;
            ci += 1;
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
        ci += 1;
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
        &atlas.slot_ink,
    )];

    StagedText {
        glyphs_emitted: instances.len(),
        instances: crate::layout::GlyphArena::from_vec(instances),
        groups,
        bounds_min: [0.0, -total_h, z_lo],
        bounds_max: [total_w, line_h, z_hi],
        codepoints_decoded,
        missing_or_bitmap,
        focus_bounds: None,
        segments,
        pick: None,
        controller: None,
        item_params: vec![
            glyph_field::ItemParamsGpu {
                line_height: line_h,
                ..Default::default()
            };
            copies as usize
        ],
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

fn word_color(word: &str) -> [u8; 3] {
    let p = word_color_packed(word.as_bytes());
    if p == palette::C_KEYWORD {
        palette::KEYWORD
    } else if p == palette::C_NUMBER {
        palette::NUMBER
    } else {
        palette::DEFAULT
    }
}

// ── Stage E1 — engine-convention paths ──────────────────────────────────────
//
// `stage_file` above is the PRODUCTION staging: cell-quantized columns, tab
// stops, and blank/missing glyphs dropped from the instance stream (bitmap
// slots are staged — the shader draws them from the emoji sheet).
// The Mojo engine (glyph_pipeline.mojo) has different conventions by design:
// it emits ONE record per UTF-8 leader byte (newlines, blanks, and missing
// codepoints included), accumulates X as an f64 running sum of the per-glyph
// f32 advances (the oracle's float discipline), counts COL in leader glyphs
// per line (no tab stops, no double-width cells), and HEIGHT is the constant
// cell height. `reference_layout` below re-implements THAT fold on the CPU,
// reading the same atlas trie, so `--engine-check` can diff the FFI records
// against an independent implementation bit-for-bit.

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
    /// takes — the bits gen_real_trie.py wrote are reproduced, not re-derived.
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

/// The font-units→world conversion, shared with tools/gen_real_trie.py:
/// `fround(fu * CELL_HEIGHT_WORLD / em_height_fu)`, computed in f64 and
/// narrowed once — the same bits the generator wrote into engine-trie.bin.
pub(crate) fn fu_to_world(fu: i32, em_height_fu: u32) -> f32 {
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
pub fn stage_records(arena: GlyphArena, placement: &ItemPlacement, slot_ink: &[Option<[f32; 4]>]) -> StagedText {
    let (min, max) = if arena.is_empty() {
        ([0.0, 0.0, 0.0], [1.0, 1.0, 0.0])
    } else {
        (placement.ink.min, placement.ink.max)
    };
    let glyphs_emitted = arena.len();
    let segments = vec![cover_segment(arena.instances(), min, max, slot_ink)];

    StagedText {
        glyphs_emitted,
        groups: vec![GroupRow::identity([0.0; 3])],
        segments,
        instances: arena,
        bounds_min: min,
        bounds_max: max,
        codepoints_decoded: placement.record_count as usize,
        missing_or_bitmap: (placement.record_count - placement.slot_count) as usize,
        focus_bounds: None,
        pick: None,
        controller: None,
        item_params: vec![glyph_field::ItemParamsGpu::default()],
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
    let mut word_start_byte = usize::MAX;
    let mut word_end_byte = 0usize;
    let mut word_start_col = 0usize;
    let mut in_comment = false;
    let mut in_string: Option<char> = None;
    let mut prev = '\0';

    let byte_at = |i: usize| -> u32 {
        if i < bytes.len() { bytes[i] as u32 } else { 0 }
    };
    let mut id = 0usize;
    while id < bytes.len() {
        let b0 = bytes[id] as u32;
        let (n, ch) = if b0 < 0x80 {
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
                let wc = word_color_packed(&bytes[word_start_byte..word_end_byte]);
                if wc != palette::C_DEFAULT {
                    colors[word_start_col..].fill(wc);
                }
                word_start_byte = usize::MAX;
            }
            in_comment = false;
            in_string = None;
        } else if ch == '\t' {
            word_start_byte = usize::MAX;
        } else if in_comment {
            color = palette::C_COMMENT;
        } else if let Some(q) = in_string {
            if ch == q && prev != '\\' {
                in_string = None;
            }
            color = palette::C_STRING;
        } else if ch == '/' && prev == '/' {
            in_comment = true;
            // Recolor the preceding '/' too (it was pushed as punctuation).
            if let Some(last) = colors.last_mut() {
                *last = palette::C_COMMENT;
            }
            color = palette::C_COMMENT;
        } else if ch == '"' || ch == '\'' || ch == '`' {
            in_string = Some(ch);
            color = palette::C_STRING;
        } else if is_word_char(ch) {
            if word_start_byte == usize::MAX {
                word_start_byte = id;
                word_start_col = colors.len();
            }
            word_end_byte = id + n;
            if bytes[word_start_byte].is_ascii_digit() {
                color = palette::C_NUMBER;
            }
        } else {
            if word_start_byte != usize::MAX {
                let wc = word_color_packed(&bytes[word_start_byte..word_end_byte]);
                if wc != palette::C_DEFAULT {
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
    colors
}

/// Colorize a single line's leaders into a reusable vector.
/// Clears `colors` and pushes one packed RGBA u32 per leader in `line_bytes`.
pub fn colorize_line_into(line_bytes: &[u8], colors: &mut Vec<u32>) {
    colors.clear();
    colors.reserve(line_bytes.len());
    let mut word_start_byte = usize::MAX;
    let mut word_end_byte = 0usize;
    let mut word_start_col = 0usize;
    let mut in_comment = false;
    let mut in_string: Option<char> = None;
    let mut prev = '\n';

    let byte_at = |i: usize| -> u32 {
        if i < line_bytes.len() { line_bytes[i] as u32 } else { 0 }
    };
    let mut id = 0usize;
    while id < line_bytes.len() {
        let b0 = line_bytes[id] as u32;
        let (n, ch) = if b0 < 0x80 {
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
        } else if in_comment {
            color = palette::C_COMMENT;
        } else if let Some(q) = in_string {
            if ch == q && prev != '\\' {
                in_string = None;
            }
            color = palette::C_STRING;
        } else if ch == '/' && prev == '/' {
            in_comment = true;
            if let Some(last) = colors.last_mut() {
                *last = palette::C_COMMENT;
            }
            color = palette::C_COMMENT;
        } else if ch == '"' || ch == '\'' || ch == '`' {
            in_string = Some(ch);
            color = palette::C_STRING;
        } else if is_word_char(ch) {
            if word_start_byte == usize::MAX {
                word_start_byte = id;
                word_start_col = colors.len();
            }
            word_end_byte = id + n;
            if line_bytes[word_start_byte].is_ascii_digit() {
                color = palette::C_NUMBER;
            }
        } else {
            if word_start_byte != usize::MAX {
                let wc = word_color_packed(&line_bytes[word_start_byte..word_end_byte]);
                if wc != palette::C_DEFAULT {
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
        let wc = word_color_packed(&line_bytes[word_start_byte..word_end_byte]);
        if wc != palette::C_DEFAULT {
            colors[word_start_col..].fill(wc);
        }
    }
}

/// Fast-path syntax coloring for pure printable ASCII lines (0x20..=0x7E).
/// Avoids UTF-8 decode branches and char conversions.
#[inline]
pub fn colorize_pure_ascii_line(line: &[u8], colors: &mut Vec<u32>) {
    colors.clear();
    colors.reserve(line.len());
    let mut word_start = usize::MAX;
    let mut in_comment = false;
    let mut in_string: Option<u8> = None;
    let mut prev = b'\n';

    for (i, &b) in line.iter().enumerate() {
        let mut color = palette::C_DEFAULT;
        if in_comment {
            color = palette::C_COMMENT;
        } else if let Some(q) = in_string {
            if b == q && prev != b'\\' {
                in_string = None;
            }
            color = palette::C_STRING;
        } else if b == b'/' && prev == b'/' {
            in_comment = true;
            if let Some(last) = colors.last_mut() {
                *last = palette::C_COMMENT;
            }
            color = palette::C_COMMENT;
        } else if b == b'"' || b == b'\'' || b == b'`' {
            in_string = Some(b);
            color = palette::C_STRING;
        } else if ASCII_WORD_CHAR[b as usize] {
            if word_start == usize::MAX {
                word_start = i;
            }
            if line[word_start].is_ascii_digit() {
                color = palette::C_NUMBER;
            }
        } else {
            if word_start != usize::MAX {
                let wc = word_color_packed(&line[word_start..i]);
                if wc != palette::C_DEFAULT {
                    colors[word_start..].fill(wc);
                }
                word_start = usize::MAX;
            }
            color = palette::C_PUNCT;
        }
        colors.push(color);
        prev = b;
    }
    if word_start != usize::MAX {
        let wc = word_color_packed(&line[word_start..]);
        if wc != palette::C_DEFAULT {
            colors[word_start..].fill(wc);
        }
    }
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

    #[test]
    fn colorize_line_into_matches_colorize_leaders() {
        let sample = b"const x = 42;\nlet y = \"hello world\"; // comment\nfn test() -> bool { true }\n";
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
            b"\"unclosed string".as_slice(),
        ];
        let mut buf_a = Vec::new();
        let mut buf_b = Vec::new();
        for &line in &lines {
            colorize_line_into(line, &mut buf_a);
            colorize_pure_ascii_line(line, &mut buf_b);
            assert_eq!(buf_a, buf_b, "mismatch on line: {}", String::from_utf8_lossy(line));
        }
    }
}


