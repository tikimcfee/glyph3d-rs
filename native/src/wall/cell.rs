//! Packing `term_cells::Cell` records for the GPU cell pass and computing
//! minimal dirty-row upload ranges.
//!
//! All glyph resolution flows through [`resolve_cell_glyph`] — the single
//! seam `crates/glyph-font` will replace when runtime fonts land.

use std::ops::Range;

use bytemuck::{Pod, Zeroable};
use term_cells::{CLUSTER_BASE, Cell, Face, Screen, attrs};

use crate::atlas::{FLAG_MISSING, TrieTable};

/// Style bits packed into `CellGpu::glyph_and_flags` at bit 16 (`cellAttrs.js` layout).
pub mod style_bits {
    pub const DIM: u32 = 1 << 0;
    pub const STRIKE: u32 = 1 << 1;
    pub const OVERLINE: u32 = 1 << 2;
    pub const UNDERLINE_SHIFT: u32 = 3;
    pub const UNDERLINE_MASK: u32 = 7 << UNDERLINE_SHIFT;
    pub const UNDERLINE_COLOR: u32 = 1 << 6;
    pub const ANY_LINE: u32 = STRIKE | OVERLINE | UNDERLINE_MASK;
}

/// Bit positions inside `CellGpu::glyph_and_flags`.
pub const GLYPH_ID_MASK: u32 = 0xFFFF;
pub const STYLE_SHIFT: u32 = 16;
pub const SPAN_COLS_SHIFT: u32 = 24;
pub const SPAN_COLS_MASK: u32 = 0x3;

/// Compact 16-byte per-cell GPU record read by `wall_cells.wgsl`.
///
/// Carries no position: column and row are derived in the vertex stage from
/// `instance_index` (`col = i % cols`, `row = i / cols`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Pod, Zeroable)]
pub struct CellGpu {
    /// Bits 0..16: atlas glyph slot id (0 = blank).
    /// Bits 16..23: packed style byte ([`style_bits`]).
    /// Bits 24..26: cell column span (`0` = wide-char trailing cell, `1` = normal, `2` = wide).
    pub glyph_and_flags: u32,
    /// Foreground text colour (sRGB RGBA8, red in low byte).
    pub fg: u32,
    /// Cell background fill colour (sRGB RGBA8; alpha 0 = transparent / panel shows).
    pub bg: u32,
    /// Underline override colour (sRGB RGBA8; read when `style_bits::UNDERLINE_COLOR` is set).
    pub ul: u32,
}

/// Pack a cell's SGR attributes into the 7-bit shader style byte (`cellAttrs.js` `cellStyle`).
pub const fn pack_cell_style(cell_attrs: u16) -> u32 {
    let underline = attrs::underline(cell_attrs) as u32;
    let dim = if cell_attrs & attrs::DIM != 0 { style_bits::DIM } else { 0 };
    let strike = if cell_attrs & attrs::STRIKE != 0 { style_bits::STRIKE } else { 0 };
    let overline = if cell_attrs & attrs::OVERLINE != 0 { style_bits::OVERLINE } else { 0 };
    let ul_color = if underline != 0 && (cell_attrs & attrs::UNDERLINE_COLOR != 0) {
        style_bits::UNDERLINE_COLOR
    } else {
        0
    };
    dim | strike | overline | ((underline & 7) << style_bits::UNDERLINE_SHIFT) | ul_color
}

/// Resolve which atlas glyph slot draws `cell`.
///
/// This is the single glyph-lookup seam for the wall cell renderer: today it
/// reads the prebaked Cousine/emoji atlas (`assets/atlas/`, where bold and
/// italic draw as regular); later `crates/glyph-font` replaces this lookup.
pub fn resolve_cell_glyph(trie: &TrieTable, screen: &Screen, cell: &Cell) -> u32 {
    if cell.content == 0 || cell.width == 0 {
        return 0;
    }

    // Face selection will route into bold/italic/bold-italic once glyph-font lands.
    let _face = Face::from_bits(attrs::face(cell.attrs));

    if cell.content < CLUSTER_BASE {
        if cell.content == 0x20 {
            return 0;
        }
        let entry = trie.lookup(cell.content);
        if entry.flags & FLAG_MISSING != 0 {
            return 0;
        }
        return entry.glyph_id;
    }

    // Multi-codepoint cluster: probe the atlas's v2 emoji sequence table
    // (dropping VS16 U+FE0F from the probe key, matching char_resolve.rs),
    // falling back to the first covered scalar in the cluster.
    let text = screen.content_str(cell);
    if text.is_empty() {
        return 0;
    }

    let mut key = [0u32; 16];
    let mut key_len = 0usize;
    for ch in text.chars() {
        let cp = ch as u32;
        if cp == 0xFE0F {
            continue;
        }
        if key_len < key.len() {
            key[key_len] = cp;
            key_len += 1;
        }
    }

    if key_len >= 2 && trie.starts_a_sequence(key[0]) {
        for len in (2..=key_len).rev() {
            if let Some(slot) = trie.sequence_lookup(&key[..len]) {
                return slot;
            }
        }
    }

    for &cp in &key[..key_len] {
        if cp == 0x20 || cp == 0x200D || (0xFE00..=0xFE0F).contains(&cp) {
            continue;
        }
        let entry = trie.lookup(cp);
        if entry.flags & FLAG_MISSING == 0 && entry.glyph_id != 0 {
            return entry.glyph_id;
        }
    }

    0
}

/// Resolve a UTF-8 label string into `(glyph_id, span_cols)` pairs using the same
/// atlas lookup seam as [`resolve_cell_glyph`].
///
/// Spaces emit `(0, 1)`; multi-codepoint emoji sequences in the atlas sequence
/// table coalesce into a single `(slot, 2)` glyph. When `crates/glyph-font` lands,
/// replacing [`resolve_cell_glyph`] and this helper moves both cells and labels over.
pub fn resolve_str_glyphs(trie: &TrieTable, text: &str, mut emit: impl FnMut(u32, u8)) {
    let chars: Vec<u32> = text.chars().map(|c| c as u32).collect();
    let wide_threshold = (trie.metrics.advance_fu as i32) * 3 / 2;
    let mut i = 0usize;
    while i < chars.len() {
        let cp = chars[i];
        if cp == 0x20 {
            emit(0, 1);
            i += 1;
            continue;
        }
        if cp == 0xFE0F || cp == 0xFE0E || cp == 0x200D {
            i += 1;
            continue;
        }

        // Probe longest matching emoji sequence starting at `chars[i]` (ignoring VS16).
        if trie.starts_a_sequence(cp) {
            let mut key = [0u32; 16];
            let mut raw_end = [0usize; 16];
            let mut klen = 0usize;
            let mut j = i;
            while j < chars.len() && klen < key.len() {
                let c = chars[j];
                j += 1;
                if c == 0xFE0F {
                    if klen > 0 {
                        raw_end[klen - 1] = j;
                    }
                    continue;
                }
                key[klen] = c;
                raw_end[klen] = j;
                klen += 1;
            }
            let mut matched = None;
            for len in (2..=klen).rev() {
                if let Some(slot) = trie.sequence_lookup(&key[..len]) {
                    matched = Some((slot, raw_end[len - 1]));
                    break;
                }
            }
            if let Some((slot, next_i)) = matched {
                emit(slot, 2);
                i = next_i;
                continue;
            }
        }

        let entry = trie.lookup(cp);
        let is_wide = entry.advance_fu > wide_threshold
            || trie
                .emoji_cell
                .get(entry.glyph_id as usize)
                .copied()
                .flatten()
                .is_some();
        let span = if is_wide { 2 } else { 1 };
        let gid = if entry.flags & FLAG_MISSING != 0 {
            0
        } else {
            entry.glyph_id
        };
        emit(gid, span);
        i += 1;
    }
}

/// Pack one `term_cells::Cell` into its 16-byte [`CellGpu`] record.
pub fn pack_cell(trie: &TrieTable, screen: &Screen, cell: &Cell) -> CellGpu {
    let span_cols = u32::from(cell.width.min(2));
    let (glyph_id, style) = if span_cols == 0 {
        // Trailing half of a wide character: its fill still renders from `bg`,
        // while the leading cell's 2-column quad owns both the glyph and lines.
        (0u32, 0u32)
    } else {
        let gid = resolve_cell_glyph(trie, screen, cell);
        debug_assert!(
            gid <= GLYPH_ID_MASK,
            "atlas glyph id {gid} exceeds 16-bit CellGpu lane"
        );
        (gid & GLYPH_ID_MASK, pack_cell_style(cell.attrs))
    };

    let fg = if cell.fg == 0 { screen.default_fg } else { cell.fg };
    let glyph_and_flags =
        glyph_id | (style << STYLE_SHIFT) | ((span_cols & SPAN_COLS_MASK) << SPAN_COLS_SHIFT);

    CellGpu {
        glyph_and_flags,
        fg,
        bg: cell.bg,
        ul: cell.ul,
    }
}

/// Coalesce rows whose `row_gen[r] > uploaded_gen` into contiguous row ranges.
///
/// Returns an empty list when nothing changed since `uploaded_gen`.
pub fn dirty_row_ranges(row_gen: &[u64], uploaded_gen: u64) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start: Option<usize> = None;
    for (row_idx, &gen) in row_gen.iter().enumerate() {
        if gen > uploaded_gen {
            if start.is_none() {
                start = Some(row_idx);
            }
        } else if let Some(run_start) = start.take() {
            ranges.push(run_start..row_idx);
        }
    }
    if let Some(run_start) = start {
        ranges.push(run_start..row_gen.len());
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;
    use term_cells::rgb;

    #[test]
    fn cell_gpu_size_is_16_bytes() {
        assert_eq!(std::mem::size_of::<CellGpu>(), 16);
    }

    #[test]
    fn pack_cell_style_maps_all_flags_and_masks_orphan_ul_color() {
        let all = attrs::BOLD
            | attrs::DIM
            | attrs::ITALIC
            | attrs::STRIKE
            | attrs::OVERLINE
            | (attrs::UNDERLINE_CURLY << attrs::UNDERLINE_SHIFT)
            | attrs::UNDERLINE_COLOR;
        let packed = pack_cell_style(all);
        assert_ne!(packed & style_bits::DIM, 0);
        assert_ne!(packed & style_bits::STRIKE, 0);
        assert_ne!(packed & style_bits::OVERLINE, 0);
        assert_eq!(
            (packed & style_bits::UNDERLINE_MASK) >> style_bits::UNDERLINE_SHIFT,
            u32::from(attrs::UNDERLINE_CURLY)
        );
        assert_ne!(packed & style_bits::UNDERLINE_COLOR, 0);

        // UNDERLINE_COLOR without an underline style must not set the shader bit.
        let orphan_ul = attrs::UNDERLINE_COLOR;
        assert_eq!(pack_cell_style(orphan_ul), 0);
    }

    #[test]
    fn pack_cell_resolves_ascii_wide_emoji_and_trailing_cell() {
        let trie = crate::atlas::default_trie();
        let mut screen = Screen::new(8, 2);

        // Plain 'A' with curly underline and custom underline colour.
        screen.row_mut(0)[0] = Cell {
            content: 'A' as u32,
            fg: rgb(0xee, 0xdd, 0xcc),
            bg: rgb(0x11, 0x22, 0x33),
            ul: rgb(0xff, 0x44, 0x55),
            attrs: (attrs::UNDERLINE_CURLY << attrs::UNDERLINE_SHIFT) | attrs::UNDERLINE_COLOR,
            width: 1,
            _pad: 0,
        };
        let gpu_a = pack_cell(&trie, &screen, &screen.row(0)[0]);
        assert_eq!(gpu_a.glyph_and_flags & GLYPH_ID_MASK, 34, "'A' is slot 34 in the atlas");
        assert_eq!((gpu_a.glyph_and_flags >> SPAN_COLS_SHIFT) & SPAN_COLS_MASK, 1);
        assert_eq!(gpu_a.fg, rgb(0xee, 0xdd, 0xcc));
        assert_eq!(gpu_a.bg, rgb(0x11, 0x22, 0x33));
        assert_eq!(gpu_a.ul, rgb(0xff, 0x44, 0x55));

        // Wide emoji rocket (U+1F680) + trailing cell (width 0, same bg).
        screen.row_mut(0)[1] = Cell {
            content: '🚀' as u32,
            fg: screen.default_fg,
            bg: rgb(0x20, 0x30, 0x40),
            ul: 0,
            attrs: 0,
            width: 2,
            _pad: 0,
        };
        screen.row_mut(0)[2] = Cell {
            content: 0,
            fg: 0,
            bg: rgb(0x20, 0x30, 0x40),
            ul: 0,
            attrs: attrs::STRIKE,
            width: 0,
            _pad: 0,
        };
        let gpu_rocket = pack_cell(&trie, &screen, &screen.row(0)[1]);
        assert_eq!(gpu_rocket.glyph_and_flags & GLYPH_ID_MASK, 4759, "🚀 is slot 4759");
        assert_eq!((gpu_rocket.glyph_and_flags >> SPAN_COLS_SHIFT) & SPAN_COLS_MASK, 2);

        let gpu_trail = pack_cell(&trie, &screen, &screen.row(0)[2]);
        assert_eq!(gpu_trail.glyph_and_flags, 0, "trailing half suppresses ink and lines");
        assert_eq!(gpu_trail.bg, rgb(0x20, 0x30, 0x40), "trailing half keeps its fill");

        // Cluster emoji (ZWJ family U+1F468 U+200D U+1F469 U+200D U+1F467 -> slot 6819).
        screen.clusters.push("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}".to_string());
        screen.row_mut(1)[0] = Cell {
            content: CLUSTER_BASE,
            fg: screen.default_fg,
            bg: 0,
            ul: 0,
            attrs: 0,
            width: 2,
            _pad: 0,
        };
        let gpu_family = pack_cell(&trie, &screen, &screen.row(1)[0]);
        assert_eq!(gpu_family.glyph_and_flags & GLYPH_ID_MASK, 6819, "ZWJ family sequence resolves");
    }

    #[test]
    fn dirty_row_ranges_coalesces_adjacent_rows_and_skips_clean_rows() {
        assert_eq!(dirty_row_ranges(&[], 0), Vec::<Range<usize>>::new());
        assert_eq!(dirty_row_ranges(&[5, 5, 5], 5), Vec::<Range<usize>>::new());
        assert_eq!(dirty_row_ranges(&[1, 6, 7, 2, 8, 2, 9, 10], 5), vec![1..3, 4..5, 6..8]);
        assert_eq!(dirty_row_ranges(&[6, 7, 8], 5), vec![0..3]);
        assert_eq!(dirty_row_ranges(&[6, 1, 1], 5), vec![0..1]);
    }
}
