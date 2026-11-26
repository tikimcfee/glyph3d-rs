//! Packing `term_cells::Cell` records for the GPU cell pass and computing
//! minimal dirty-row upload ranges.
//!
//! All glyph resolution flows through [`glyph_font::FontAtlas`] via
//! [`resolve_cell_glyph`] (terminal cells) and [`resolve_str_glyphs`] (canvas labels).

use std::ops::Range;

use bytemuck::{Pod, Zeroable};
use glyph_font::{Face, FamilyId, FontAtlas, GLYPH_MODE_BITMAP, ResolvedCell};
use term_cells::{Cell, Screen, attrs};

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

/// Resolve `(slot, span_cols)` for `cell` in `family` through [`FontAtlas::resolve_cell`].
///
/// Handles ASCII/Unicode scalars, bold/italic/bold-italic faces from `cell.attrs`,
/// generated cell sprites, combining-mark clusters, colour emoji, spaces, blanks,
/// and trailing halves of wide characters.
pub fn resolve_cell_glyph(
    atlas: &mut FontAtlas,
    family: FamilyId,
    screen: &Screen,
    cell: &Cell,
) -> ResolvedCell {
    atlas.resolve_cell(family, screen, cell)
}

const fn is_regional_indicator(cp: u32) -> bool {
    cp >= 0x1F1E6 && cp <= 0x1F1FF
}

const fn is_skin_tone(cp: u32) -> bool {
    cp >= 0x1F3FB && cp <= 0x1F3FF
}

const fn is_combining_mark(cp: u32) -> bool {
    matches!(
        cp,
        0x0300..=0x036F
            | 0x1AB0..=0x1AFF
            | 0x1DC0..=0x1DFF
            | 0x20D0..=0x20FF
            | 0xFE20..=0xFE2F
    )
}

/// East Asian Wide / Fullwidth and astral pictographic planes that occupy two monospace columns.
const fn is_wide_codepoint(cp: u32) -> bool {
    matches!(
        cp,
        0x1100..=0x115F
            | 0x2329..=0x232A
            | 0x2E80..=0x303E
            | 0x3041..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0xA4CF
            | 0xA960..=0xA97C
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE10..=0xFE19
            | 0xFE30..=0xFE6F
            | 0xFF01..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1FAFF
            | 0x20000..=0x3FFFD
    )
}

/// Resolve a UTF-8 label string into `(glyph_id, span_cols)` pairs using [`FontAtlas`].
///
/// Spaces emit `(0, 1)`; combining-mark clusters (e.g. `e\u{0301}`) and multi-codepoint
/// emoji sequences (ZWJ chains, flags, skin-tone modifiers, keycaps, VS16) coalesce into
/// a single glyph resolved through [`FontAtlas::cluster`]. Colour-emoji bitmap slots and
/// wide East Asian scalars span 2 columns; all other glyphs span 1 column.
pub fn resolve_str_glyphs(
    atlas: &mut FontAtlas,
    family: FamilyId,
    text: &str,
    mut emit: impl FnMut(u32, u8),
) {
    let mut chars: Vec<(usize, char)> = text.char_indices().collect();
    let end_byte = text.len();
    // Sentinel for easy slice-end lookup.
    chars.push((end_byte, '\0'));
    let n = chars.len() - 1;

    let mut i = 0usize;
    while i < n {
        let (start_byte, ch0) = chars[i];
        let cp0 = ch0 as u32;
        if cp0 == 0x20 {
            emit(0, 1);
            i += 1;
            continue;
        }
        if cp0 == 0xFE0F || cp0 == 0xFE0E || cp0 == 0x200D {
            i += 1;
            continue;
        }

        let mut next = i + 1;
        if is_regional_indicator(cp0) && next < n && is_regional_indicator(chars[next].1 as u32) {
            next += 1;
        } else {
            while next < n {
                let cp = chars[next].1 as u32;
                if cp == 0xFE0F || cp == 0xFE0E || cp == 0x20E3 || is_skin_tone(cp) || is_combining_mark(cp) {
                    next += 1;
                } else if cp == 0x200D {
                    next += 1;
                    if next < n {
                        next += 1;
                    }
                } else {
                    break;
                }
            }
        }

        let cluster = &text[start_byte..chars[next].0];
        let slot = atlas.cluster(family, Face::Regular, cluster);
        let is_bitmap = slot != 0 && atlas.map_entry(slot)[2] == GLYPH_MODE_BITMAP;
        let span = if is_bitmap || is_wide_codepoint(cp0) { 2 } else { 1 };
        emit(slot, span);
        i = next;
    }
}

/// Pack one pre-resolved cell into its 16-byte [`CellGpu`] record.
pub fn pack_cell_resolved(resolved: ResolvedCell, screen: &Screen, cell: &Cell) -> CellGpu {
    let span_cols = u32::from(resolved.span_cols.min(2));
    let (glyph_id, style) = if span_cols == 0 {
        // Trailing half of a wide character: its fill still renders from `bg`,
        // while the leading cell's 2-column quad owns both the glyph and lines.
        (0u32, 0u32)
    } else {
        let gid = resolved.slot;
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

/// Resolve and pack one `term_cells::Cell` into its 16-byte [`CellGpu`] record.
pub fn pack_cell(
    atlas: &mut FontAtlas,
    family: FamilyId,
    screen: &Screen,
    cell: &Cell,
) -> CellGpu {
    let resolved = resolve_cell_glyph(atlas, family, screen, cell);
    pack_cell_resolved(resolved, screen, cell)
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
    use std::sync::Arc;

    use glyph_font::{
        default_font_chain, GLYPH_MODE_OUTLINE, GLYPH_MODE_SPRITE, MONASPACE_NEON_FAMILY,
    };
    use term_cells::{CLUSTER_BASE, rgb};

    use super::*;
    use crate::atlas::Atlas;
    use crate::wall::glyphs::{AtlasEmojiSource, default_fonts_dir};

    fn test_font_atlas() -> (FontAtlas, FamilyId) {
        let specs = default_font_chain(&default_fonts_dir());
        let mut atlas = FontAtlas::load(&specs).expect("load default font chain");
        let baked = Arc::new(Atlas::load(&crate::default_emoji_sheet()));
        atlas.set_emoji_source(Some(Arc::new(AtlasEmojiSource::new(baked))));
        let family = atlas
            .family_id(MONASPACE_NEON_FAMILY)
            .unwrap_or_else(|| atlas.default_family());
        (atlas, family)
    }

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
    fn pack_cell_resolves_faces_sprites_clusters_emoji_and_trailing_cell() {
        let (mut atlas, family) = test_font_atlas();
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
        let gpu_a = pack_cell(&mut atlas, family, &screen, &screen.row(0)[0]);
        let slot_a = gpu_a.glyph_and_flags & GLYPH_ID_MASK;
        assert_ne!(slot_a, 0);
        assert_eq!(atlas.map_entry(slot_a)[2], GLYPH_MODE_OUTLINE);
        assert_eq!((gpu_a.glyph_and_flags >> SPAN_COLS_SHIFT) & SPAN_COLS_MASK, 1);
        assert_eq!(gpu_a.fg, rgb(0xee, 0xdd, 0xcc));
        assert_eq!(gpu_a.bg, rgb(0x11, 0x22, 0x33));
        assert_eq!(gpu_a.ul, rgb(0xff, 0x44, 0x55));

        // Bold and italic 'A' resolve to distinct outline slots from regular 'A'.
        let bold_a = Cell { attrs: attrs::BOLD, ..screen.row(0)[0] };
        let italic_a = Cell { attrs: attrs::ITALIC, ..screen.row(0)[0] };
        let slot_bold = pack_cell(&mut atlas, family, &screen, &bold_a).glyph_and_flags & GLYPH_ID_MASK;
        let slot_italic = pack_cell(&mut atlas, family, &screen, &italic_a).glyph_and_flags & GLYPH_ID_MASK;
        assert_ne!(slot_bold, slot_a);
        assert_ne!(slot_italic, slot_a);
        assert_ne!(slot_bold, slot_italic);

        // Box-drawing sprite routes to mode 3.
        let box_cell = Cell {
            content: '╭' as u32,
            fg: screen.default_fg,
            bg: 0,
            ul: 0,
            attrs: 0,
            width: 1,
            _pad: 0,
        };
        let slot_box = pack_cell(&mut atlas, family, &screen, &box_cell).glyph_and_flags & GLYPH_ID_MASK;
        assert_eq!(atlas.map_entry(slot_box)[2], GLYPH_MODE_SPRITE);

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
        let gpu_rocket = pack_cell(&mut atlas, family, &screen, &screen.row(0)[1]);
        let slot_rocket = gpu_rocket.glyph_and_flags & GLYPH_ID_MASK;
        assert_eq!(atlas.map_entry(slot_rocket)[2], GLYPH_MODE_BITMAP);
        assert_eq!((gpu_rocket.glyph_and_flags >> SPAN_COLS_SHIFT) & SPAN_COLS_MASK, 2);

        let gpu_trail = pack_cell(&mut atlas, family, &screen, &screen.row(0)[2]);
        assert_eq!(gpu_trail.glyph_and_flags, 0, "trailing half suppresses ink and lines");
        assert_eq!(gpu_trail.bg, rgb(0x20, 0x30, 0x40), "trailing half keeps its fill");

        // Cluster emoji (ZWJ family U+1F468 U+200D U+1F469 U+200D U+1F467).
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
        let gpu_family = pack_cell(&mut atlas, family, &screen, &screen.row(1)[0]);
        let slot_family = gpu_family.glyph_and_flags & GLYPH_ID_MASK;
        assert_eq!(atlas.map_entry(slot_family)[2], GLYPH_MODE_BITMAP);

        // Label string resolution coalesces clusters and ZWJ emoji.
        let mut label_glyphs = Vec::new();
        resolve_str_glyphs(&mut atlas, family, "A e\u{0301} \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}", |gid, span| {
            label_glyphs.push((gid, span));
        });
        assert_eq!(label_glyphs.len(), 5, "'A', ' ', 'é', ' ', ZWJ");
        assert_eq!(label_glyphs[0].1, 1);
        assert_eq!(label_glyphs[1], (0, 1));
        assert_eq!(label_glyphs[2].1, 1);
        assert_ne!(label_glyphs[2].0, 0);
        assert_eq!(label_glyphs[3], (0, 1));
        assert_eq!(label_glyphs[4], (slot_family, 2));
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
