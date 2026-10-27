//! Tab stops in HyperLayout's walks (2026-10-11; the rule is
//! [`crate::fold::TabWidth`]).
//!
//! A tab spans `k = width - (line_cells % width)` cells, where `line_cells`
//! is the display cells its logical line has reached. Every walk here keeps
//! the line's f64 advance, which is exactly `line_cells × cell` (every atlas
//! advance is whole cells and the f64 sum of them is exact), so the count is
//! recovered at the tab itself by one division — nothing is added to the walk
//! for any other byte, and a chunk seeded mid-line needs no state it did not
//! carry already.
//!
//! At one cell, [`TabStops::advance`] hands back the trie's advance and its
//! widening — the `seg_adv += r.advance; line_adv += r.advance as f64` every
//! walk did before tab stops, bit for bit.

use crate::fold::TabWidth;
use crate::layout::ItemParams;

/// An item's tab stops and the cell they count in.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TabStops {
    width: TabWidth,
    cell_advance: f32,
}

impl TabStops {
    /// The stops of item `p`, counted in `cell_advance` (the primary font's
    /// cell, which is the advance the trie gives a tab).
    #[inline(always)]
    pub(crate) fn of(p: &ItemParams, cell_advance: f32) -> Self {
        Self::new(p.tab_width, cell_advance)
    }

    /// Stops of `width`, counted in `cell_advance`.
    #[inline(always)]
    pub(crate) fn new(width: TabWidth, cell_advance: f32) -> Self {
        Self { width, cell_advance }
    }

    /// Whether a tab can span more than one cell.
    #[inline(always)]
    pub(crate) fn expands(self) -> bool {
        self.width.expands()
    }

    /// The measures of the leader `byte`, which resolved to `advance`, on a
    /// line whose f64 advance has reached `line_advance`: its f32 advance (the
    /// record's ADVANCE lane, the page extent's width, the segment carrier's
    /// addend) and the f64 line advance's addend.
    #[inline(always)]
    pub(crate) fn advance(self, byte: u8, advance: f32, line_advance: f64) -> (f32, f64) {
        if byte == b'\t' && self.width.expands() {
            self.tab(line_cells(line_advance, self.cell_advance))
        } else {
            (advance, advance as f64)
        }
    }

    /// A tab on a line at `line_cells`: `f32(k) × cell`, one correctly
    /// rounded product, and the exact `k × cell` for the f64 carrier.
    #[inline(always)]
    fn tab(self, line_cells: u64) -> (f32, f64) {
        let cells = self.width.tab_cells(line_cells);
        (cells as f32 * self.cell_advance, cells as f64 * self.cell_advance as f64)
    }
}

/// The display cells a line whose f64 advance is `line_advance` has reached:
/// exact, since that advance is `cells × cell` to the bit.
#[inline(always)]
pub(crate) fn line_cells(line_advance: f64, cell_advance: f32) -> u64 {
    let cells = line_advance / cell_advance as f64;
    debug_assert_eq!(cells, cells.round(), "a line advance of {line_advance} is not whole cells of {cell_advance}");
    cells.round() as u64
}

#[cfg(test)]
mod tests {
    /// What makes a line's cell count exact (`fold::TabWidth`): every
    /// codepoint the trie answers, and the bitmap advance, is a whole number
    /// of cells. Moved here from the retired line table (2026-10-26), whose
    /// seeds carried the count; the tab stops rest on it.
    #[test]
    fn every_trie_advance_is_whole_cells() {
        let trie = crate::default_trie();
        let cell = trie.metrics.advance_fu as i32;
        assert_eq!(trie.bitmap_advance_fu, 2 * cell, "the bitmap advance is two cells");
        let mut distinct = std::collections::BTreeSet::new();
        for cp in 0u32..0x11_0000 {
            let e = trie.lookup(cp);
            assert_eq!(e.advance_fu % cell, 0, "U+{cp:04X} advance {} fu is not whole cells", e.advance_fu);
            distinct.insert(e.advance_fu / cell);
        }
        assert!(distinct.iter().all(|&c| (0..=2).contains(&c)), "advances in cells: {distinct:?}");
    }
}
