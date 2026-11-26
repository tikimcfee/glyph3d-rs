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

/// Cell-coordinate text selection highlight for one terminal (wall-core-free).
///
/// Matches the JS wall's `TerminalSelection` / `TerminalGrid`:
/// - Linear stream (`block == false`): from `(start_row, start_col)` through
///   `(end_row, end_col)` in reading order (`end_col` exclusive), selecting full
///   rows `0..cols` on any interior row.
/// - Column block (`block == true`): rows `start_row..=end_row` and columns
///   `start_col..end_col` on every row.
/// - `background` replaces each selected cell's fill (opaque sRGB RGBA8; default
///   `#cccccc`). `foreground` replaces each selected cell's text colour (`#0a0a1e`
///   by default), or `None` keeps each cell's own foreground (`"keep"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellSelection {
    pub start_row: u16,
    pub start_col: u16,
    pub end_row: u16,
    pub end_col: u16,
    pub block: bool,
    /// Selection fill colour (packed sRGB RGBA8; alpha 0 is promoted to opaque 0xFF).
    pub background: u32,
    /// Optional selection text colour (packed sRGB RGBA8); `None` keeps cell fg.
    pub foreground: Option<u32>,
}

impl CellSelection {
    /// Default JS wall selection fill (`#cccccc`, `TERMINAL_SELECTION_DEFAULTS.background`).
    pub const DEFAULT_BACKGROUND: u32 = term_cells::rgb(0xcc, 0xcc, 0xcc);
    /// Default JS wall selection text (`#0a0a1e`, `TERMINAL_SELECTION_DEFAULTS.foreground`).
    pub const DEFAULT_FOREGROUND: u32 = term_cells::rgb(0x0a, 0x0a, 0x1e);

    /// Construct a linear (stream) selection from `(start_row, start_col)` to
    /// `(end_row, end_col)` (end column exclusive) with the default JS wall colours.
    pub const fn linear(start_row: u16, start_col: u16, end_row: u16, end_col: u16) -> Self {
        Self {
            start_row,
            start_col,
            end_row,
            end_col,
            block: false,
            background: Self::DEFAULT_BACKGROUND,
            foreground: Some(Self::DEFAULT_FOREGROUND),
        }
    }

    /// Construct a rectangular column-block selection over rows `start_row..=end_row`
    /// and columns `start_col..end_col` with the default JS wall colours.
    pub const fn block(start_row: u16, start_col: u16, end_row: u16, end_col: u16) -> Self {
        Self {
            start_row,
            start_col,
            end_row,
            end_col,
            block: true,
            background: Self::DEFAULT_BACKGROUND,
            foreground: Some(Self::DEFAULT_FOREGROUND),
        }
    }

    /// Override the fill and optional text colour (`None` keeps each cell's own `fg`).
    pub const fn with_colors(mut self, background: u32, foreground: Option<u32>) -> Self {
        self.background = background;
        self.foreground = foreground;
        self
    }

    /// True when this selection covers zero cells.
    pub const fn is_empty(&self) -> bool {
        if self.block {
            self.start_row > self.end_row || self.start_col >= self.end_col
        } else {
            self.start_row > self.end_row
                || (self.start_row == self.end_row && self.start_col >= self.end_col)
        }
    }

    /// Selected column span `[lo, hi)` on `row` for a grid of `cols` columns
    /// (`0..0` when `row` has no selected cells).
    pub fn row_span(&self, row: u16, cols: u16) -> Range<u16> {
        if self.is_empty() || row < self.start_row || row > self.end_row {
            return 0..0;
        }
        let lo = if self.block || row == self.start_row {
            self.start_col.min(cols)
        } else {
            0
        };
        let hi = if self.block || row == self.end_row {
            self.end_col.min(cols)
        } else {
            cols
        };
        if hi <= lo { 0..0 } else { lo..hi }
    }
}

/// Expand a non-empty column span `[lo, hi)` on `row` so neither boundary cuts
/// a 2-column wide character (`width == 2` head + `width == 0` tail) in half.
///
/// Mirrors terminal selection/underline rules (and `glyph_vt::Vt::text`): starting
/// on a wide character's trailing cell widens `lo` left by 1; ending on a wide
/// character's head cell widens `hi` right by 1 when its trailing cell is present.
pub fn snap_row_span_to_wide(row: &[Cell], span: Range<u16>) -> Range<u16> {
    let cols = row.len().min(usize::from(u16::MAX)) as u16;
    let mut lo = span.start.min(cols);
    let mut hi = span.end.min(cols);
    if hi <= lo {
        return 0..0;
    }
    if lo > 0 && row[lo as usize].width == 0 && row[lo as usize - 1].width == 2 {
        lo -= 1;
    }
    if hi < cols && row[hi as usize - 1].width == 2 && row[hi as usize].width == 0 {
        hi += 1;
    }
    lo..hi
}

/// Apply `selection`'s fill and optional text colour to a packed [`CellGpu`].
pub fn apply_selection_to_cell(cell: &mut CellGpu, selection: &CellSelection) {
    let bg = if (selection.background >> 24) == 0 {
        selection.background | 0xFF00_0000
    } else {
        selection.background
    };
    cell.bg = bg;
    if let Some(fg) = selection.foreground {
        cell.fg = if (fg >> 24) == 0 { fg | 0xFF00_0000 } else { fg };
    }
}

/// Cell-coordinate hover-link highlight (OSC 8 hyperlink or plain URL) for one terminal
/// (wall-core-free).
///
/// Covers a linear reading-order cell range from `(start_row, start_col)` through
/// `(end_row, end_col)` (`end_col` exclusive), including full interior rows `0..cols`
/// when a link soft-wraps across lines. Drawn as a single underline across the span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellLinkHover {
    pub start_row: u16,
    pub start_col: u16,
    pub end_row: u16,
    pub end_col: u16,
    /// Optional underline colour (packed sRGB RGBA8; alpha 0 promoted to opaque `0xFF`).
    /// `None` underlines in each cell's own foreground colour.
    pub color: Option<u32>,
}

impl CellLinkHover {
    /// Construct a linear hover-link range from `(start_row, start_col)` to
    /// `(end_row, end_col)` (end column exclusive), underlining in each cell's `fg`.
    pub const fn new(start_row: u16, start_col: u16, end_row: u16, end_col: u16) -> Self {
        Self {
            start_row,
            start_col,
            end_row,
            end_col,
            color: None,
        }
    }

    /// Construct a single-row hover-link range `start_col..end_col` on `row`.
    pub const fn single_row(row: u16, start_col: u16, end_col: u16) -> Self {
        Self::new(row, start_col, row, end_col)
    }

    /// Override the hover underline colour (packed sRGB RGBA8).
    pub const fn with_color(mut self, color: u32) -> Self {
        self.color = Some(color);
        self
    }

    /// True when this hover-link range covers zero cells.
    pub const fn is_empty(&self) -> bool {
        self.start_row > self.end_row
            || (self.start_row == self.end_row && self.start_col >= self.end_col)
    }

    /// Covered column span `[lo, hi)` on `row` for a grid of `cols` columns
    /// (`0..0` when `row` has no hovered link cells).
    pub fn row_span(&self, row: u16, cols: u16) -> Range<u16> {
        if self.is_empty() || row < self.start_row || row > self.end_row {
            return 0..0;
        }
        let lo = if row == self.start_row {
            self.start_col.min(cols)
        } else {
            0
        };
        let hi = if row == self.end_row {
            self.end_col.min(cols)
        } else {
            cols
        };
        if hi <= lo { 0..0 } else { lo..hi }
    }
}

/// Apply `link`'s hover underline to a packed [`CellGpu`].
///
/// Leaves wide-char trailing halves (`span_cols == 0`) untouched because the snapped
/// wide head cell owns the full 2-column underline quad. Cells without an existing
/// underline receive `UNDERLINE_SINGLE`; cells that already carry an underline style
/// keep it.
pub fn apply_hover_link_to_cell(cell: &mut CellGpu, link: &CellLinkHover) {
    let span_cols = (cell.glyph_and_flags >> SPAN_COLS_SHIFT) & SPAN_COLS_MASK;
    if span_cols == 0 {
        return;
    }
    let mut style = (cell.glyph_and_flags >> STYLE_SHIFT) & 0x7F;
    if (style & style_bits::UNDERLINE_MASK) == 0 {
        style |= u32::from(attrs::UNDERLINE_SINGLE) << style_bits::UNDERLINE_SHIFT;
    }
    if let Some(color) = link.color {
        style |= style_bits::UNDERLINE_COLOR;
        cell.ul = if (color >> 24) == 0 {
            color | 0xFF00_0000
        } else {
            color
        };
    }
    cell.glyph_and_flags =
        (cell.glyph_and_flags & !(0x7Fu32 << STYLE_SHIFT)) | (style << STYLE_SHIFT);
}

/// Coalesce rows that need GPU re-upload due to screen mutations, a changed
/// [`CellSelection`], or a changed [`CellLinkHover`].
#[expect(
    clippy::too_many_arguments,
    reason = "pure row-diff helper comparing previous and next screen generation plus two optional overlays"
)]
pub fn dirty_row_ranges_with_overlays(
    row_gen: &[u64],
    cols: u16,
    screen_gen: u64,
    uploaded_gen: u64,
    old_sel: Option<CellSelection>,
    new_sel: Option<CellSelection>,
    old_link: Option<CellLinkHover>,
    new_link: Option<CellLinkHover>,
) -> Vec<Range<usize>> {
    let old_sel = old_sel.filter(|s| !s.is_empty());
    let new_sel = new_sel.filter(|s| !s.is_empty());
    let old_link = old_link.filter(|l| !l.is_empty());
    let new_link = new_link.filter(|l| !l.is_empty());
    let screen_dirty = screen_gen != uploaded_gen;
    let sel_dirty = old_sel != new_sel;
    let link_dirty = old_link != new_link;
    if !screen_dirty && !sel_dirty && !link_dirty {
        return Vec::new();
    }
    let sel_colors_changed = old_sel.map(|s| (s.background, s.foreground))
        != new_sel.map(|s| (s.background, s.foreground));
    let link_color_changed = old_link.map(|l| l.color) != new_link.map(|l| l.color);

    let mut ranges = Vec::new();
    let mut start: Option<usize> = None;
    for (row_idx, &gen) in row_gen.iter().enumerate() {
        let r = row_idx as u16;
        let row_screen_dirty = screen_dirty && gen > uploaded_gen;
        let row_sel_dirty = if sel_dirty {
            let old_span = old_sel.map_or(0..0, |s| s.row_span(r, cols));
            let new_span = new_sel.map_or(0..0, |s| s.row_span(r, cols));
            old_span != new_span || (sel_colors_changed && !new_span.is_empty())
        } else {
            false
        };
        let row_link_dirty = if link_dirty {
            let old_span = old_link.map_or(0..0, |l| l.row_span(r, cols));
            let new_span = new_link.map_or(0..0, |l| l.row_span(r, cols));
            old_span != new_span || (link_color_changed && !new_span.is_empty())
        } else {
            false
        };
        if row_screen_dirty || row_sel_dirty || row_link_dirty {
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

/// Coalesce rows that need GPU re-upload due to either screen mutations
/// (`row_gen[r] > uploaded_gen` when `screen_gen != uploaded_gen`) or a change
/// in the terminal's active [`CellSelection`] affecting that row.
pub fn dirty_row_ranges_with_selection(
    row_gen: &[u64],
    cols: u16,
    screen_gen: u64,
    uploaded_gen: u64,
    old_sel: Option<CellSelection>,
    new_sel: Option<CellSelection>,
) -> Vec<Range<usize>> {
    dirty_row_ranges_with_overlays(
        row_gen,
        cols,
        screen_gen,
        uploaded_gen,
        old_sel,
        new_sel,
        None,
        None,
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use glyph_font::{
        default_font_chain, GLYPH_MODE_OUTLINE, GLYPH_MODE_SPRITE, MONASPACE_NEON_FAMILY,
    };
    use term_cells::{CLUSTER_BASE, rgb};

    use super::*;
    use crate::wall::glyphs::{EmojiSheetArchive, SparseEmojiSource, default_fonts_dir};

    fn test_font_atlas() -> (FontAtlas, FamilyId) {
        let specs = default_font_chain(&default_fonts_dir());
        let mut atlas = FontAtlas::load(&specs).expect("load default font chain");
        let archive = Arc::new(EmojiSheetArchive::load(&crate::default_emoji_sheet()));
        atlas.set_emoji_source(Some(Arc::new(SparseEmojiSource::new(archive))));
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

    #[test]
    fn cell_selection_row_span_linear_and_block() {
        let linear = CellSelection::linear(1, 5, 3, 10);
        assert!(!linear.is_empty());
        assert_eq!(linear.row_span(0, 80), 0..0);
        assert_eq!(linear.row_span(1, 80), 5..80);
        assert_eq!(linear.row_span(2, 80), 0..80);
        assert_eq!(linear.row_span(3, 80), 0..10);
        assert_eq!(linear.row_span(4, 80), 0..0);

        // Single-row linear selection.
        let single = CellSelection::linear(2, 4, 2, 9);
        assert_eq!(single.row_span(2, 80), 4..9);
        assert_eq!(single.row_span(1, 80), 0..0);

        // Linear ending at col 0 of last row leaves that row untouched.
        let end_zero = CellSelection::linear(1, 4, 2, 0);
        assert_eq!(end_zero.row_span(1, 20), 4..20);
        assert_eq!(end_zero.row_span(2, 20), 0..0);

        // Column block selection selects the same columns on every covered row.
        let block = CellSelection::block(1, 3, 3, 8);
        assert_eq!(block.row_span(0, 20), 0..0);
        assert_eq!(block.row_span(1, 20), 3..8);
        assert_eq!(block.row_span(2, 20), 3..8);
        assert_eq!(block.row_span(3, 20), 3..8);
        assert_eq!(block.row_span(4, 20), 0..0);

        // Empty and inverted selections yield empty spans.
        assert!(CellSelection::linear(2, 5, 2, 5).is_empty());
        assert!(CellSelection::linear(3, 0, 2, 5).is_empty());
        assert!(CellSelection::block(1, 5, 3, 5).is_empty());
        assert!(CellSelection::block(3, 1, 1, 5).is_empty());
    }

    #[test]
    fn dirty_row_ranges_with_selection_touches_only_changed_rows() {
        let row_gen = [1u64; 10];
        let cols = 40;

        // 1. Steady state (unchanged screen + unchanged selection) -> 0 rows.
        let s1 = CellSelection::linear(2, 5, 4, 12);
        assert_eq!(
            dirty_row_ranges_with_selection(&row_gen, cols, 1, 1, Some(s1), Some(s1)),
            Vec::<Range<usize>>::new()
        );

        // 2. Initial selection on rows 2..=4 -> only rows 2..5.
        assert_eq!(
            dirty_row_ranges_with_selection(&row_gen, cols, 1, 1, None, Some(s1)),
            vec![2..5]
        );

        // 3. Extending drag right on the final row -> ONLY row 4!
        let s2 = CellSelection::linear(2, 5, 4, 25);
        assert_eq!(
            dirty_row_ranges_with_selection(&row_gen, cols, 1, 1, Some(s1), Some(s2)),
            vec![4..5]
        );

        // 4. Extending drag down from row 4 to row 6 -> only rows 4..7 (rows 2 & 3 untouched!).
        let s3 = CellSelection::linear(2, 5, 6, 8);
        assert_eq!(
            dirty_row_ranges_with_selection(&row_gen, cols, 1, 1, Some(s2), Some(s3)),
            vec![4..7]
        );

        // 5. Jumping selection to row 8 -> two disjoint ranges (old 2..7 + new 8..9).
        let s4 = CellSelection::linear(8, 1, 8, 10);
        assert_eq!(
            dirty_row_ranges_with_selection(&row_gen, cols, 1, 1, Some(s3), Some(s4)),
            vec![2..7, 8..9]
        );

        // 6. Changing selection colours -> only currently selected row 8.
        let s4_keep = s4.with_colors(rgb(0x40, 0x60, 0xc0), None);
        assert_eq!(
            dirty_row_ranges_with_selection(&row_gen, cols, 1, 1, Some(s4), Some(s4_keep)),
            vec![8..9]
        );

        // 7. Clearing selection -> only previously selected row 8.
        assert_eq!(
            dirty_row_ranges_with_selection(&row_gen, cols, 1, 1, Some(s4_keep), None),
            vec![8..9]
        );

        // 8. Unrelated screen edit on row 0 while selection sits steady on row 8 -> only row 0.
        let mut edited = row_gen;
        edited[0] = 2;
        assert_eq!(
            dirty_row_ranges_with_selection(&edited, cols, 2, 1, Some(s4), Some(s4)),
            vec![0..1]
        );
    }

    #[test]
    fn snap_row_span_to_wide_widens_split_wide_boundaries_and_preserves_clean_edges() {
        // Row: [0:'a', 1:'漢'(w=2), 2:tail(w=0), 3:'b', 4:'🚀'(w=2), 5:tail(w=0), 6:'漢'(w=2, last col)]
        let mut row = vec![Cell::BLANK; 7];
        row[0] = Cell { content: 'a' as u32, width: 1, ..Cell::BLANK };
        row[1] = Cell { content: '漢' as u32, width: 2, ..Cell::BLANK };
        row[2] = Cell { content: 0, width: 0, ..Cell::BLANK };
        row[3] = Cell { content: 'b' as u32, width: 1, ..Cell::BLANK };
        row[4] = Cell { content: '🚀' as u32, width: 2, ..Cell::BLANK };
        row[5] = Cell { content: 0, width: 0, ..Cell::BLANK };
        row[6] = Cell { content: '漢' as u32, width: 2, ..Cell::BLANK };

        // Empty stays empty.
        assert_eq!(snap_row_span_to_wide(&row, 2..2), 0..0);
        // Strictly narrow stays untouched.
        assert_eq!(snap_row_span_to_wide(&row, 0..1), 0..1);
        assert_eq!(snap_row_span_to_wide(&row, 3..4), 3..4);
        // Ending on wide head (1..2) widens right to include tail (1..3).
        assert_eq!(snap_row_span_to_wide(&row, 0..2), 0..3);
        assert_eq!(snap_row_span_to_wide(&row, 1..2), 1..3);
        // Starting on wide tail (2..4) widens left to include head (1..4).
        assert_eq!(snap_row_span_to_wide(&row, 2..4), 1..4);
        // Splitting wide cells on BOTH ends (2..5) widens both (1..6).
        assert_eq!(snap_row_span_to_wide(&row, 2..5), 1..6);
        // Already whole wide cell (1..3) stays exact.
        assert_eq!(snap_row_span_to_wide(&row, 1..3), 1..3);
        // Orphan wide cell in the last column (6..7) stays clamped within row length.
        assert_eq!(snap_row_span_to_wide(&row, 6..7), 6..7);
    }

    #[test]
    fn cell_link_hover_spans_style_and_minimal_dirty_rows() {
        // Multi-row wrapped URL from (1, 10) to (3, 4).
        let link = CellLinkHover::new(1, 10, 3, 4);
        assert!(!link.is_empty());
        assert_eq!(link.row_span(0, 20), 0..0);
        assert_eq!(link.row_span(1, 20), 10..20);
        assert_eq!(link.row_span(2, 20), 0..20);
        assert_eq!(link.row_span(3, 20), 0..4);
        assert_eq!(link.row_span(4, 20), 0..0);
        assert!(CellLinkHover::single_row(2, 5, 5).is_empty());

        // Applying default-fg hover link sets UNDERLINE_SINGLE without UNDERLINE_COLOR.
        let mut cell = CellGpu {
            glyph_and_flags: 42 | (1 << SPAN_COLS_SHIFT),
            fg: rgb(0x82, 0xaa, 0xff),
            bg: 0,
            ul: 0,
        };
        apply_hover_link_to_cell(&mut cell, &link);
        let st = (cell.glyph_and_flags >> STYLE_SHIFT) & 0x7F;
        assert_eq!((st & style_bits::UNDERLINE_MASK) >> style_bits::UNDERLINE_SHIFT, u32::from(attrs::UNDERLINE_SINGLE));
        assert_eq!(st & style_bits::UNDERLINE_COLOR, 0);

        // Explicit colour sets UNDERLINE_COLOR and promotes alpha 0 -> 0xFF.
        let tinted = link.with_color(0x00_ff_b0_60);
        apply_hover_link_to_cell(&mut cell, &tinted);
        let st2 = (cell.glyph_and_flags >> STYLE_SHIFT) & 0x7F;
        assert_ne!(st2 & style_bits::UNDERLINE_COLOR, 0);
        assert_eq!(cell.ul, 0xff_ff_b0_60);

        // Existing curly underline is preserved (not downgraded to single).
        let mut curly = CellGpu {
            glyph_and_flags: 42 | ((u32::from(attrs::UNDERLINE_CURLY) << style_bits::UNDERLINE_SHIFT) << STYLE_SHIFT) | (1 << SPAN_COLS_SHIFT),
            fg: rgb(0xcc, 0xcc, 0xcc),
            bg: 0,
            ul: 0,
        };
        apply_hover_link_to_cell(&mut curly, &link);
        let st_c = (curly.glyph_and_flags >> STYLE_SHIFT) & 0x7F;
        assert_eq!((st_c & style_bits::UNDERLINE_MASK) >> style_bits::UNDERLINE_SHIFT, u32::from(attrs::UNDERLINE_CURLY));

        // Trailing wide half (`span_cols == 0`) stays untouched.
        let mut tail = CellGpu { glyph_and_flags: 0, fg: 0, bg: 0, ul: 0 };
        apply_hover_link_to_cell(&mut tail, &tinted);
        assert_eq!(tail.glyph_and_flags, 0);
        assert_eq!(tail.ul, 0);

        // Dirty-row tracking touches ONLY rows whose hover link changed.
        let row_gen = [1u64; 8];
        let a = CellLinkHover::single_row(2, 4, 18);
        let b = CellLinkHover::single_row(5, 0, 12);
        assert_eq!(
            dirty_row_ranges_with_overlays(&row_gen, 20, 1, 1, None, None, None, Some(a)),
            vec![2..3]
        );
        assert_eq!(
            dirty_row_ranges_with_overlays(&row_gen, 20, 1, 1, None, None, Some(a), Some(a)),
            Vec::<Range<usize>>::new()
        );
        assert_eq!(
            dirty_row_ranges_with_overlays(&row_gen, 20, 1, 1, None, None, Some(a), Some(b)),
            vec![2..3, 5..6]
        );
        assert_eq!(
            dirty_row_ranges_with_overlays(&row_gen, 20, 1, 1, None, None, Some(b), None),
            vec![5..6]
        );
    }
}
