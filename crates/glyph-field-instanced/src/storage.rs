//! The chunked slot buffers and per-slot byte addressing.

use glyph_field::{split_at_chunks, SlotChunk};

use crate::slot::{RenderSlot, SLOT_BYTES};

/// Where the field's `RenderSlot`s live: one buffer range per chunk.
///
/// The buffers are the chain's own slot buffers on the endpoint (Device)
/// path, the upload buffers otherwise. `chunks[k].offset` is slot 0's byte
/// address in `chunks[k].buffer` (0 for staged uploads; the pool slice's
/// start for the endpoint's extracted buffers).
pub struct SlotStorage {
    pub(crate) chunks: Vec<SlotChunk>,
    /// Live slots per chunk, mirrored out of `chunks` for the trait's slice
    /// accessor.
    pub(crate) chunk_counts: Vec<u32>,
    /// Slots per chunk (the cull/pick slot math keys on it, so the
    /// renderer's chunking and the producer's must agree).
    pub(crate) chunk_capacity: u32,
    pub(crate) glyph_count: u32,
    /// When mapped in host-visible memory, base address of the contiguous
    /// RenderSlot slice.
    pub(crate) mapped_base: Option<usize>,
}

impl SlotStorage {
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

    /// Partial slot upload: `data` at byte `field_offset` within a slot
    /// (32 B RenderSlot stride, 4-aligned offsets — write_buffer's
    /// requirement).
    pub(crate) fn write_field(&self, queue: &wgpu::Queue, slot: u32, field_offset: u64, data: &[u8]) {
        let (buffer, offset) = self.address(slot, field_offset);
        queue.write_buffer(buffer, offset, data);
    }

    /// Whole-slot upload of a contiguous run, one write per chunk it touches.
    pub(crate) fn write_slots(&self, queue: &wgpu::Queue, first_slot: u32, slots: &[RenderSlot]) {
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
            let ptr = addr as *mut RenderSlot;
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
                self.write_field(queue, slot, crate::slot::COLOR_OFFSET, bytemuck::bytes_of(&color));
            }
        }
    }
}
