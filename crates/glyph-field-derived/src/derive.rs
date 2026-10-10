//! The vertex stage's Y/Z derivation, transcribed: what
//! `glyph_field_derived.wgsl`'s `derive_yz` computes from a slot's row lane,
//! its wrap segment and its item's [`ItemParamsGpu`], written once more in
//! Rust so a test can hold it to the Instanced emitter's positions — the
//! Derived field's one lane no oracle sees (the slot carries no Y or Z).
//!
//! Keep the two in step by hand: the shader is the contract, this is its
//! witness, and a fault both share is invisible here (the same shape as
//! `fold::rows_for_line` under hyper-oracle). `wgsl_derive_yz_carries_the_column_page_term`
//! pins the one term that was missing until 2026-10-10 in the shader's text.
//!
//! THE ROW LANE. A Derived slot's `row` word is `row:24 | x_page:8`
//! ([`pack_row`]): the column page a cell falls in (`col / page_cols` for a
//! column-paged item, else 0) rides the row's high byte, because the vertex
//! stage has no column to derive it from and `fold::paginate`'s z has an
//! `x_page × depth_per_col` term. Until 2026-10-10 the lane was the bare row
//! and that term was dropped: every column-paged item's z was wrong in
//! Derived mode, unseen because no golden view is column-paged (the repo
//! never sets `page_cols`) and hyper-oracle compares slots, not the vertex
//! stage. Limits: 16,777,215 rows per item, 255 column pages — asserted in
//! debug builds, saturated in release.

use glyph_field::ItemParamsGpu;

/// Bits of the row lane that hold the row.
pub const ROW_BITS: u32 = 24;
/// The largest row a Derived slot can carry.
pub const ROW_MAX: u32 = (1 << ROW_BITS) - 1;
/// The largest column page a Derived slot can carry.
pub const X_PAGE_MAX: u32 = (1 << (32 - ROW_BITS)) - 1;

/// Pack a row and its column page into the slot's row lane.
#[inline(always)]
pub fn pack_row(row: u32, x_page: u32) -> u32 {
    debug_assert!(row <= ROW_MAX, "Derived row {row} exceeds {ROW_MAX}");
    debug_assert!(x_page <= X_PAGE_MAX, "Derived column page {x_page} exceeds {X_PAGE_MAX}");
    (row.min(ROW_MAX)) | (x_page.min(X_PAGE_MAX) << ROW_BITS)
}

/// The row half of a packed row lane.
#[inline(always)]
pub fn row_of(lane: u32) -> u32 {
    lane & ROW_MAX
}

/// The column-page half of a packed row lane.
#[inline(always)]
pub fn x_page_of(lane: u32) -> u32 {
    lane >> ROW_BITS
}

/// `derive_yz` as the shader computes it, over the packed row lane: the
/// cell's world y and z. Every `fma` is the shader's own, in its order.
pub fn derive_yz(row_lane: u32, wrap_segment: u32, item: &ItemParamsGpu) -> [f32; 2] {
    let row = row_of(row_lane);
    let x_page = x_page_of(row_lane);
    let depth_steps = -(wrap_segment as f32);
    let z_tail = depth_steps.mul_add(item.z_step_lo, item.origin_z);
    let z_stepped = depth_steps.mul_add(item.z_step, z_tail);
    if item.has_page != 0 {
        let screen_row = row as i32 - item.scroll_rows;
        let mut y_page = 0i32;
        if item.page_rows > 0 && screen_row >= item.page_rows {
            y_page = screen_row / item.page_rows;
        }
        let pages_wide = item.pages_wide.max(1);
        let band = y_page / pages_wide;
        let row_in_page = (screen_row - y_page * item.page_rows) as f32;
        let y_tail = (-row_in_page).mul_add(item.line_height_lo, item.origin_y);
        let y_row_folded = (-row_in_page).mul_add(item.line_height, y_tail);
        let y = (-(band as f32)).mul_add(item.band_stride_y, y_row_folded);
        let z_banded = (band as f32).mul_add(item.depth_per_band, z_stepped);
        let z = (x_page as f32).mul_add(item.depth_per_col, z_banded);
        [y, z]
    } else {
        let y_tail = (-(row as f32)).mul_add(item.line_height_lo, item.origin_y);
        let y = (-(row as f32)).mul_add(item.line_height, y_tail);
        [y, z_stepped]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_lane_packs_and_unpacks() {
        for (row, xp) in [(0u32, 0u32), (1, 0), (0, 1), (123_456, 7), (ROW_MAX, X_PAGE_MAX)] {
            let lane = pack_row(row, xp);
            assert_eq!(row_of(lane), row);
            assert_eq!(x_page_of(lane), xp);
        }
        // A bare row (x_page 0) is the lane itself: the pre-2026-10-10 word.
        assert_eq!(pack_row(42, 0), 42);
    }

    #[test]
    fn wgsl_derive_yz_carries_the_column_page_term() {
        // The shader is the contract; this pins the term the transcription
        // reproduces, so the two cannot silently part on it.
        let wgsl = crate::GLYPH_FIELD_DERIVED_WGSL;
        assert!(wgsl.contains("let x_page = row_lane >> 24u;"), "the shader unpacks the column page from the row lane");
        assert!(wgsl.contains("fma(f32(x_page), item.depth_per_col, z_banded)"), "the shader adds x_page * depth_per_col");
        assert!(wgsl.contains("let row = row_lane & 0xFFFFFFu;"), "the shader masks the row");
    }
}
