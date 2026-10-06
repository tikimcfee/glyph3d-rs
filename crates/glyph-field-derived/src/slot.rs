//! The Derived field's slot format: 20 B per glyph.
//!
//! Layout:
//!   w0 (offset 0):  x: f32 (pen origin in world units)
//!   w1 (offset 4):  line_idx: u32 (indexes line table {item_idx, row})
//!   w2 (offset 8):  glyph_and_wrap: u32 (low 16: glyph_id, high 16: wrap_segment)
//!   w3 (offset 12): color: u32 (packed RGBA8)
//!   w4 (offset 16): group_id: u32 (the addressability hook into GroupRow)

use bytemuck::{Pod, Zeroable};

/// The compact 20 B slot record.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable, encase::ShaderType)]
pub struct DerivedSlot {
    pub x: f32,
    pub row: u32,
    pub glyph_and_wrap: u32,
    pub color: u32,
    pub item_and_group: u32,
}

impl DerivedSlot {
    #[inline(always)]
    pub fn new(x: f32, row: u32, glyph_id: u16, wrap_segment: u16, color: u32, item_idx: u16, group_id: u16) -> Self {
        Self {
            x,
            row,
            glyph_and_wrap: (glyph_id as u32) | ((wrap_segment as u32) << 16),
            color,
            item_and_group: (item_idx as u32) | ((group_id as u32) << 16),
        }
    }

    #[inline(always)]
    pub fn glyph_id(&self) -> u16 {
        (self.glyph_and_wrap & 0xFFFF) as u16
    }

    #[inline(always)]
    pub fn wrap_segment(&self) -> u16 {
        (self.glyph_and_wrap >> 16) as u16
    }
}

pub const SLOT_BYTES: u64 = 20;
pub const X_OFFSET: u64 = 0;
pub const ROW_OFFSET: u64 = 4;
pub const GLYPH_AND_WRAP_OFFSET: u64 = 8;
pub const COLOR_OFFSET: u64 = 12;
pub const ITEM_AND_GROUP_OFFSET: u64 = 16;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_slot_size_and_offsets() {
        assert_eq!(std::mem::size_of::<DerivedSlot>(), 20);
        assert_eq!(std::mem::align_of::<DerivedSlot>(), 4);
        assert_eq!(core::mem::offset_of!(DerivedSlot, x) as u64, X_OFFSET);
        assert_eq!(core::mem::offset_of!(DerivedSlot, row) as u64, ROW_OFFSET);
        assert_eq!(core::mem::offset_of!(DerivedSlot, glyph_and_wrap) as u64, GLYPH_AND_WRAP_OFFSET);
        assert_eq!(core::mem::offset_of!(DerivedSlot, color) as u64, COLOR_OFFSET);
        assert_eq!(core::mem::offset_of!(DerivedSlot, item_and_group) as u64, ITEM_AND_GROUP_OFFSET);
    }

    #[test]
    fn encase_bytes_match_bytemuck() {
        let slot = DerivedSlot::new(123.456, 789, 42, 5, 0xDEADBEEF, 10, 11);
        let mut buf = Vec::<u8>::new();
        encase::StorageBuffer::new(&mut buf).write(&slot).unwrap();
        assert_eq!(buf.len(), 20);
        assert_eq!(&buf[..], bytemuck::bytes_of(&slot), "DerivedSlot bytes");
        assert_eq!(slot.glyph_id(), 42);
        assert_eq!(slot.wrap_segment(), 5);
    }
}
