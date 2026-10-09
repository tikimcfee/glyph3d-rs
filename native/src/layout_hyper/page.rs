//! Pagination, written once for every HyperLayout emitter.
//!
//! THE REFERENCE is `fold::paginate`, and this is its arithmetic transcribed
//! operation for operation, because the emitters are held to it BIT-exact
//! (hyper-oracle): the same f64 expressions, the same terms in the same order,
//! the zero terms included (dropping `- 0.0 * band_stride_y` can flip the sign
//! of a zero). Every page decision reads the INTEGER row and column:
//!
//! - `screen_row = row - scroll_rows` — the conveyor; a row scrolled above the
//!   page keeps a negative screen row and stays in flow;
//! - `y_page = screen_row / page_rows`, only once `screen_row >= page_rows`;
//! - `x_page = col / page_cols`, whenever `page_cols > 0`, whatever
//!   `page_rows` says (the depth fan of a column-paged item);
//! - the stride is the fold's `derive_stride`: the widest item-relative x plus
//!   `page_gap_x` (Pass 1's `max_row_extent`), only for a row-paged item.
//!
//! Until 2026-10-09 (C14) the three Pass 2 paths carried their own copies,
//! each with `row + scroll_rows`, `y_page` read from the unscrolled row, and
//! `x_page` dropped when `page_rows` was 0; the recording path (`rederive.rs`)
//! had it right, so records agreed while instances did not.

use crate::layout::ItemParams;

/// One item's page geometry, read once per item (or chunk).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Pager {
    /// `fold::page_active`: whether paginate rewrites this item at all.
    pub active: bool,
    rows: i64,
    cols: i64,
    scroll: i64,
    pages_wide: i64,
    stride_x: f64,
    origin_y: f64,
    origin_z: f64,
    line_height: f64,
    band_stride_y: f64,
    depth_per_band: f64,
    depth_per_col: f64,
    z_step: f64,
}

/// Where paginate puts one cell: the x offset to add to its pre-page x (in
/// f64, as the fold adds it to the BASE_X lane), and its final y and z.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PageFrame {
    pub x_off: f64,
    pub y: f32,
    pub z: f32,
}

impl Pager {
    /// `max_row_extent` is the fold's scalar 7 (Pass 1 computes it); it
    /// becomes the stride only for a row-paged item, as `derive_stride` says.
    #[inline]
    pub fn new(p: &ItemParams, max_row_extent: f64) -> Self {
        let rows = if p.has_page { p.page_rows as i64 } else { 0 };
        let cols = if p.has_page { p.page_cols as i64 } else { 0 };
        let scroll = if p.has_page { p.scroll_rows as i64 } else { 0 };
        Self {
            active: rows != 0 || cols != 0 || scroll != 0,
            rows,
            cols,
            scroll,
            pages_wide: if p.pages_wide > 1 { p.pages_wide as i64 } else { 1 },
            stride_x: if !p.has_page || p.page_rows <= 0 { 0.0 } else { max_row_extent + p.page_gap_x },
            origin_y: p.origin_y,
            origin_z: p.origin_z,
            line_height: p.line_height,
            band_stride_y: p.band_stride_y,
            depth_per_band: p.depth_per_band,
            depth_per_col: p.depth_per_col,
            z_step: p.z_step,
        }
    }

    /// The column page a cell at `col` falls in (0 unless `page_cols > 0`).
    #[inline(always)]
    pub fn x_page(&self, col: i64) -> i64 {
        if self.cols > 0 {
            col / self.cols
        } else {
            0
        }
    }

    /// `page_cols` when the item is column-paged, else 0: the run length at
    /// which an emitter that batches cells must re-ask [`Self::frame`].
    #[inline(always)]
    pub fn cols(&self) -> i64 {
        if self.cols > 0 {
            self.cols
        } else {
            0
        }
    }

    /// `fold::paginate` for a cell at `row` / `col` in wrap segment
    /// `wrap_segment` (the DEPTH fan's index, `fold::wrap_segment_of`).
    #[inline(always)]
    pub fn frame(&self, row: i64, col: i64, wrap_segment: i64) -> PageFrame {
        let rows = self.rows;
        let screen_row = row - self.scroll;
        let mut y_page = 0i64;
        if rows > 0 && screen_row >= rows {
            y_page = screen_row / rows;
        }
        let x_page = self.x_page(col);
        let pages_wide = self.pages_wide;
        let band = y_page / pages_wide;
        PageFrame {
            x_off: (y_page % pages_wide) as f64 * self.stride_x,
            y: (self.origin_y
                - (screen_row - y_page * rows) as f64 * self.line_height
                - band as f64 * self.band_stride_y) as f32,
            z: (self.origin_z - wrap_segment as f64 * self.z_step
                + band as f64 * self.depth_per_band
                + x_page as f64 * self.depth_per_col) as f32,
        }
    }
}

/// Paginate's x: the pre-page x (the fold's BASE_X lane) plus the frame's
/// offset, narrowed once.
#[inline(always)]
pub(crate) fn paged_x(base_x: f32, frame: &PageFrame) -> f32 {
    (base_x as f64 + frame.x_off) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold;

    fn item_of(p: &ItemParams) -> fold::Item {
        fold::Item {
            byte_start: 0,
            byte_count: 0,
            origin_x: p.origin_x,
            origin_y: p.origin_y,
            origin_z: p.origin_z,
            wrap_width: p.wrap_width as i64,
            wrap_mode: p.wrap_mode,
            cluster_mode: p.cluster_mode,
            z_step: p.z_step,
            line_height: p.line_height,
            has_page: p.has_page,
            page_rows: p.page_rows as i64,
            page_cols: p.page_cols as i64,
            scroll_rows: p.scroll_rows as i64,
            pages_wide: p.pages_wide as i64,
            page_gap_x: p.page_gap_x,
            band_stride_y: p.band_stride_y,
            depth_per_band: p.depth_per_band,
            depth_per_col: p.depth_per_col,
            page_line_height: p.page_line_height,
        }
    }

    /// The pager against `fold::paginate` itself, cell by cell, over a grid
    /// of rows, columns and page shapes that reaches every branch: scroll
    /// above and below a page, column paging with and without row paging,
    /// pages_wide 1 and 3, wrap segments into depth.
    #[test]
    fn pager_is_fold_paginate_bit_for_bit() {
        let shapes: [(i32, i32, i32, i32); 7] =
            [(0, 0, 3, 1), (4, 0, 0, 1), (4, 0, 2, 3), (0, 5, 0, 1), (3, 5, 1, 2), (4, 0, 9, 3), (2, 2, 2, 2)];
        for (rows, cols, scroll, wide) in shapes {
            let p = ItemParams {
                origin_x: 0.25,
                origin_y: 1.5,
                origin_z: -0.75,
                line_height: 1.3,
                z_step: 0.6,
                wrap_width: 7,
                has_page: true,
                page_rows: rows,
                page_cols: cols,
                scroll_rows: scroll,
                pages_wide: wide,
                page_gap_x: 2.0,
                band_stride_y: 11.0,
                depth_per_band: 0.375,
                depth_per_col: -1.25,
                ..ItemParams::default()
            };
            let it = item_of(&p);
            let max_extent = 9.123_f64;
            let pager = Pager::new(&p, max_extent);
            assert_eq!(pager.active, fold::page_active(&it));
            let stride = fold::derive_stride(max_extent, &it);
            for row in 0..20i64 {
                for col in 0..30i64 {
                    for terminator in [false, true] {
                        let mut slots = fold::Slots::new(1);
                        slots.fl[0] = fold::F_LEADER | if terminator { fold::F_NEWLINE } else { 0 };
                        slots.lc[0] = row as u32;
                        slots.lc[1] = col as u32;
                        let base_x = 0.5f32 + col as f32 * 0.53;
                        slots.set_base_x(0, base_x);
                        fold::paginate(&mut slots, 0, &it, stride);
                        let seg = fold::wrap_segment_of(col, p.wrap_width as i64, terminator);
                        let f = pager.frame(row, col, seg);
                        let got = [paged_x(base_x, &f), f.y, f.z];
                        let want = [slots.x(0), slots.y(0), slots.z(0)];
                        assert_eq!(
                            got.map(f32::to_bits),
                            want.map(f32::to_bits),
                            "shape {rows}/{cols}/{scroll}/{wide} row {row} col {col} nl {terminator}: {got:?} vs {want:?}"
                        );
                    }
                }
            }
        }
    }
}
