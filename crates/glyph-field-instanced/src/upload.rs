//! Host records → device slots: the Instanced transcode, 48 B engine record
//! → 32 B `RenderSlot`. The chunking, staging and upload paths are
//! `glyph_field::upload_host_slots`, shared with the Derived mode (C1).

use glyph_field::{GlyphInstance, SlotRecord, Transcode, UploadLabels};

use crate::slot::{RenderSlot, COLOR_OFFSET};

impl SlotRecord for RenderSlot {
    const COLOR_OFFSET: u64 = COLOR_OFFSET;
}

/// The 48 B → 32 B transcode: two 16 B copies, dropping `row`/`col` (the
/// field order is fixed by `From<&GlyphInstance>`, which this must match).
pub struct InstancedTranscode;

impl Transcode for InstancedTranscode {
    type Slot = RenderSlot;

    #[inline(always)]
    unsafe fn transcode(&self, src: &GlyphInstance, dst: *mut RenderSlot) {
        let s = src as *const GlyphInstance as *const u8;
        let d = dst as *mut u8;
        std::ptr::copy_nonoverlapping(s, d, 16);
        std::ptr::copy_nonoverlapping(s.add(24), d.add(16), 16);
    }
}

pub const LABELS: UploadLabels = UploadLabels {
    buffers: "glyph instances",
    staging: "glyph instance staging",
    staging_copy: "glyph_instance_staging_copy",
};
