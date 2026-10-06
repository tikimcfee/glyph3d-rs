//! The chunked slot buffers and per-slot byte addressing for DerivedSlot (20 B).

use glyph_field::{split_at_chunks, SlotChunk};

use crate::slot::{DerivedSlot, COLOR_OFFSET, GROUP_ID_OFFSET, SLOT_BYTES, X_OFFSET};

/// Where the Derived field's `DerivedSlot`s live: one buffer range per chunk.
pub struct DerivedSlotStorage {
    pub(crate) chunks: Vec<SlotChunk>,
    pub(crate) chunk_counts: Vec<u32>,
    pub(crate) chunk_capacity: u32,
    pub(crate) glyph_count: u32,
    pub(crate) mapped_base: Option<usize>,
}

impl DerivedSlotStorage {
    pub(crate) fn new(
        chunks: Vec<SlotChunk>,
        chunk_capacity: usize,
        glyph_count: usize,
        mapped_base: Option<usize>,
    ) -> Self {
        let chunk_counts = chunks.iter().map(|c| c.slots).collect();
        Self {
            chunks,
            chunk_counts,
            chunk_capacity: chunk_capacity as u32,
            glyph_count: glyph_count as u32,
            mapped_base,
        }
    }

    /// The buffer holding `slot` and the byte address of `slot`'s first byte
    /// plus `field_offset` within it.
    pub(crate) fn address(&self, slot: u32, field_offset: u64) -> (&wgpu::Buffer, u64) {
        let chunk = &self.chunks[(slot / self.chunk_capacity) as usize];
        let local = (slot % self.chunk_capacity) as u64;
        (&chunk.buffer, chunk.offset + local * SLOT_BYTES + field_offset)
    }

    /// Partial slot upload: `data` at byte `field_offset` within a slot (20 B stride).
    pub(crate) fn write_field(&self, queue: &wgpu::Queue, slot: u32, field_offset: u64, data: &[u8]) {
        let (buffer, offset) = self.address(slot, field_offset);
        queue.write_buffer(buffer, offset, data);
    }

    /// Whole-slot upload of a contiguous run, one write per chunk it touches.
    pub(crate) fn write_slots(&self, queue: &wgpu::Queue, first_slot: u32, slots: &[DerivedSlot]) {
        for (_chunk, local, run_offset) in split_at_chunks(first_slot, slots.len() as u32, self.chunk_capacity) {
            let piece = &slots[run_offset as usize..(run_offset + local.len() as u32) as usize];
            let (buffer, offset) = self.address(first_slot + run_offset, 0);
            queue.write_buffer(buffer, offset, bytemuck::cast_slice(piece));
        }
    }

    /// Bulk color write. Mapped storage is written in place through the base
    /// pointer; otherwise one 4 B queue write per slot.
    pub(crate) fn write_colors(&self, queue: &wgpu::Queue, first_slot: u32, colors: &[u32]) {
        if colors.is_empty() {
            return;
        }
        if let Some(addr) = self.mapped_base {
            let ptr = addr as *mut DerivedSlot;
            let total = self.glyph_count as usize;
            let base = first_slot as usize;
            let count = colors.len().min(total.saturating_sub(base));
            unsafe {
                for (i, &c) in colors.iter().take(count).enumerate() {
                    (*ptr.add(base + i)).color = c;
                }
            }
        } else {
            for (i, &color) in colors.iter().enumerate() {
                let slot = first_slot + i as u32;
                if slot >= self.glyph_count {
                    break;
                }
                self.write_field(queue, slot, COLOR_OFFSET, bytemuck::bytes_of(&color));
            }
        }
    }

    pub(crate) fn write_x(&self, queue: &wgpu::Queue, slot: u32, x: f32) {
        self.write_field(queue, slot, X_OFFSET, bytemuck::bytes_of(&x));
    }

    pub(crate) fn write_group_id(&self, queue: &wgpu::Queue, slot: u32, group_id: u32) {
        self.write_field(queue, slot, GROUP_ID_OFFSET, bytemuck::bytes_of(&group_id));
    }
}
