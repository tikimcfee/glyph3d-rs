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

// ── the item / group lane ───────────────────────────────────────────────────
//
// A Derived slot's fifth word is `item:20 | override:12`. The vertex stage
// needs the ITEM for Y/Z (`item_table[item]`) and it needs a GROUP for the
// transform; until 2026-10-10 the word was one number read as both, which
// held only because every item's group id equals its index at load — the
// moment a group verb (`SetGlyphBackground`, `SetGlyphTransform`) allocated
// a new group row for one glyph, the vertex stage read `item_table[new
// group]`: another item's rows, or past the table. Now the group is the
// item's own (`ItemParamsGpu::group`) unless the lane names an override,
// an index into a small resident table (`BINDING_GROUP_OVERRIDES`; index 0
// is "none") that the field fills as verbs allocate groups. 1,048,575 items,
// 4,095 live overrides per field.

/// Bits of the lane that hold the item.
pub const ITEM_BITS: u32 = 20;
/// The largest item index a Derived slot can carry.
pub const ITEM_MAX: u32 = (1 << ITEM_BITS) - 1;
/// The largest override index a Derived slot can carry (0 means none).
pub const OVERRIDE_MAX: u32 = (1 << (32 - ITEM_BITS)) - 1;

/// The lane of a glyph in item `item` with no group override: what every
/// producer emits at load.
#[inline(always)]
pub fn item_lane(item: u32) -> u32 {
    debug_assert!(item <= ITEM_MAX, "Derived item {item} exceeds {ITEM_MAX}");
    item.min(ITEM_MAX)
}

/// The lane of a glyph in item `item` whose group is override `override_idx`
/// (1..=OVERRIDE_MAX; 0 is the plain item lane).
#[inline(always)]
pub fn override_lane(item: u32, override_idx: u32) -> u32 {
    debug_assert!(override_idx <= OVERRIDE_MAX, "Derived override {override_idx} exceeds {OVERRIDE_MAX}");
    item_lane(item) | (override_idx.min(OVERRIDE_MAX) << ITEM_BITS)
}

/// The item half of a lane.
#[inline(always)]
pub fn item_of(lane: u32) -> u32 {
    lane & ITEM_MAX
}

/// The override half of a lane (0 = none).
#[inline(always)]
pub fn override_of(lane: u32) -> u32 {
    lane >> ITEM_BITS
}

/// The group the vertex stage draws a slot with: its item's group, unless
/// the lane names an override. `item_groups[i]` is `item_table[i].group`;
/// `overrides` is the override table, index 0 unused.
pub fn resolve_group(lane: u32, item_groups: &[u32], overrides: &[u32]) -> u32 {
    let ov = override_of(lane);
    if ov != 0 {
        overrides[ov as usize]
    } else {
        item_groups[item_of(lane) as usize]
    }
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
    fn group_lane_resolves_to_the_item_or_its_override() {
        // Items 0..3 have groups 0..3 (the load-time identity), overrides 1
        // and 2 name groups 7 and 9.
        let item_groups = [0u32, 1, 2, 3];
        let overrides = [u32::MAX, 7, 9];
        for item in 0..4u32 {
            assert_eq!(item_lane(item), item, "a plain lane is the item number, the pre-2026-10-10 word");
            assert_eq!(resolve_group(item_lane(item), &item_groups, &overrides), item);
            assert_eq!(resolve_group(override_lane(item, 1), &item_groups, &overrides), 7);
            assert_eq!(resolve_group(override_lane(item, 2), &item_groups, &overrides), 9);
            assert_eq!(item_of(override_lane(item, 2)), item, "the override leaves the item intact");
            assert_eq!(override_of(override_lane(item, 2)), 2);
        }
        assert_eq!(item_of(override_lane(ITEM_MAX, OVERRIDE_MAX)), ITEM_MAX);
        assert_eq!(override_of(override_lane(ITEM_MAX, OVERRIDE_MAX)), OVERRIDE_MAX);
    }

    #[test]
    fn wgsl_resolves_the_group_from_the_item_or_the_override_table() {
        let wgsl = crate::GLYPH_FIELD_DERIVED_WGSL;
        assert!(wgsl.contains("let item_idx = inst.item_and_group & 0xFFFFFu;"), "the shader masks the item");
        assert!(wgsl.contains("let override_idx = inst.item_and_group >> 20u;"), "the shader unpacks the override");
        assert!(wgsl.contains("var group_id = item.group;"), "the group is the item's by default");
        assert!(wgsl.contains("group_id = group_overrides[override_idx];"), "an override names the group");
        assert!(wgsl.contains("@binding(10) var<storage, read> group_overrides: array<u32>;"), "the override table is bound");
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
