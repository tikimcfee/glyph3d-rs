//! Tab stops in HyperLayout's walks (2026-10-11; the rule is
//! [`crate::fold::TabWidth`]).
//!
//! A tab spans `k = width - (line_cells % width)` cells, where `line_cells`
//! is the display cells its logical line has reached. Every walk here keeps
//! the line's f64 advance, which is exactly `line_cells × cell` (every atlas
//! advance is whole cells and the f64 sum of them is exact), so the count is
//! recovered at the tab itself by one division — nothing is added to the walk
//! for any other byte, and a chunk or segment seeded mid-line needs no state
//! it did not carry already. A walk that counts cells as an integer instead
//! (the line table's replays) asks [`TabStops::advance_at_cells`].
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

    /// [`Self::advance`] for a walk that counts the line's cells itself.
    #[inline(always)]
    pub(crate) fn advance_at_cells(self, byte: u8, advance: f32, line_cells: u64) -> (f32, f64) {
        if byte == b'\t' && self.width.expands() {
            self.tab(line_cells)
        } else {
            (advance, advance as f64)
        }
    }

    /// A tab on a line at `line_cells`: `f32(k) × cell`, one correctly
    /// rounded product (what the Visible kernel's `f32(kk) * cell_adv` is),
    /// and the exact `k × cell` for the f64 carrier.
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
