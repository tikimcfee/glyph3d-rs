//! Host records → device slots: the Derived transcode, 48 B `GlyphInstance`
//! → 20 B `DerivedSlot`. The chunking, staging and upload paths are
//! `glyph_field::upload_host_slots`, shared with the Instanced mode (C1).

use glyph_field::{GlyphInstance, ItemParamsGpu, SlotRecord, Transcode, UploadLabels};

use crate::slot::{DerivedSlot, COLOR_OFFSET};

impl SlotRecord for DerivedSlot {
    const COLOR_OFFSET: u64 = COLOR_OFFSET;
}

/// The 48 B → 20 B transcode. A host record names its GROUP row; the
/// item it belongs to — the slot's item lane, and the item-table row its
/// wrap segment is recovered against when the engine did not flag it — is
/// `item_of_group[group]` (the scene's view registry answers it). Until
/// 2026-10-11 the group id was written as the item, which held only because
/// every host producer padded its item table until the two agreed.
pub struct DerivedTranscode<'a> {
    pub item_params: &'a [ItemParamsGpu],
    pub item_of_group: &'a [u32],
    /// The producer registered no item: its one zeroed padding record
    /// names none (`transcode_one`).
    pub no_items: bool,
}

impl Transcode for DerivedTranscode<'_> {
    type Slot = DerivedSlot;

    #[inline(always)]
    unsafe fn transcode(&self, src: &GlyphInstance, dst: *mut DerivedSlot) {
        std::ptr::write(dst, transcode_one(src, self.item_params, self.item_of_group, self.no_items));
    }
}

pub const LABELS: UploadLabels = UploadLabels {
    buffers: "derived glyph instances",
    staging: "derived glyph instance staging",
    staging_copy: "derived_instance_staging_copy",
};

#[inline(always)]
fn transcode_one(inst: &GlyphInstance, item_params: &[ItemParamsGpu], item_of_group: &[u32], no_items: bool) -> DerivedSlot {
    // A load with no items still stages one zeroed record (the slot buffer
    // is never empty) and draws none; it has no item to name, so it takes
    // lane 0. Anywhere else an unnamed group row is a producer's mistake.
    let item_index = match item_of_group.get(inst.group_id as usize).copied() {
        Some(item) if item != glyph_field::NO_ITEM => item,
        _ if no_items => 0,
        _ => panic!("a host record's group row {} names no single item", inst.group_id),
    };
    let wrap_segment = if inst.flags != 0 {
        (inst.flags & 0xFFFF) as u16
    } else if let Some(item) = item_params.get(item_index as usize) {
        if item.z_step.abs() > 1e-6 {
            ((item.origin_z - inst.pos[2]) / item.z_step).round().max(0.0) as u16
        } else {
            0
        }
    } else {
        0
    };

    let glyph_id = (inst.glyph_id & 0xFFFF) as u16;
    // The row lane carries the column page (derive.rs): col / page_cols for
    // a column-paged item, else 0.
    let x_page = match item_params.get(item_index as usize) {
        Some(item) if item.has_page != 0 && item.page_cols > 0 => inst.col / item.page_cols as u32,
        _ => 0,
    };
    let row = crate::derive::pack_row(inst.row, x_page);
    let item_and_group = crate::derive::item_lane(item_index);

    DerivedSlot::with_item_and_group(
        inst.pos[0],
        row,
        glyph_id,
        wrap_segment,
        inst.color,
        item_and_group,
    )
}
