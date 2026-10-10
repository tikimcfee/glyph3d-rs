//! Host records → device slots: the Derived transcode, 48 B `GlyphInstance`
//! → 20 B `DerivedSlot`. The chunking, staging and upload paths are
//! `glyph_field::upload_host_slots`, shared with the Instanced mode (C1).

use glyph_field::{GlyphInstance, ItemParamsGpu, SlotRecord, Transcode, UploadLabels};

use crate::slot::{DerivedSlot, COLOR_OFFSET};

impl SlotRecord for DerivedSlot {
    const COLOR_OFFSET: u64 = COLOR_OFFSET;
}

/// The 48 B → 20 B transcode. Reads the item table to recover a record's
/// wrap segment from its Z when the engine did not flag it.
pub struct DerivedTranscode<'a> {
    pub item_params: &'a [ItemParamsGpu],
}

impl Transcode for DerivedTranscode<'_> {
    type Slot = DerivedSlot;

    #[inline(always)]
    unsafe fn transcode(&self, src: &GlyphInstance, dst: *mut DerivedSlot) {
        std::ptr::write(dst, transcode_one(src, self.item_params));
    }
}

pub const LABELS: UploadLabels = UploadLabels {
    buffers: "derived glyph instances",
    staging: "derived glyph instance staging",
    staging_copy: "derived_instance_staging_copy",
};

#[inline(always)]
fn transcode_one(inst: &GlyphInstance, item_params: &[ItemParamsGpu]) -> DerivedSlot {
    let wrap_segment = if inst.flags != 0 {
        (inst.flags & 0xFFFF) as u16
    } else if let Some(item) = item_params.get(inst.group_id as usize) {
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
    let x_page = match item_params.get(inst.group_id as usize) {
        Some(item) if item.has_page != 0 && item.page_cols > 0 => inst.col / item.page_cols as u32,
        _ => 0,
    };
    let row = crate::derive::pack_row(inst.row, x_page);
    let item_and_group = inst.group_id;

    DerivedSlot::with_item_and_group(
        inst.pos[0],
        row,
        glyph_id,
        wrap_segment,
        inst.color,
        item_and_group,
    )
}
