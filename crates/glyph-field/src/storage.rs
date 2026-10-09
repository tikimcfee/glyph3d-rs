//! The chunked slot buffers and per-slot byte addressing, for any slot record.
//!
//! Hoisted from the two mode crates (C1, 2026-10-09), which carried this
//! twice with only the slot type renamed.

use crate::upload::{upload_host_slots, HostUpload, Transcode, UploadLabels};
use crate::{split_at_chunks, SlotChunk, SlotSource};

/// A mode's per-glyph record, as the shared storage and upload see it.
///
/// The record is the binding's element: `size_of::<Self>()` is the slot
/// stride every address is computed with.
pub trait SlotRecord: bytemuck::Pod + Send + Sync {
    /// Byte offset of the record's packed RGBA8 `u32` colour.
    const COLOR_OFFSET: u64;
}

/// Where a field's slots live: one buffer range per chunk.
///
/// The buffers are the producer's own slot buffers on the Device path, the
/// upload buffers otherwise. `chunks[k].offset` is slot 0's byte address in
/// `chunks[k].buffer` (0 for staged uploads; the pool slice's start for a
/// device source's extracted buffers).
pub struct SlotStorage<S: SlotRecord> {
    chunks: Vec<SlotChunk>,
    /// Live slots per chunk, mirrored out of `chunks` for the trait's slice
    /// accessor.
    chunk_counts: Vec<u32>,
    /// Slots per chunk (the cull/pick slot math keys on it, so the
    /// renderer's chunking and the producer's must agree).
    chunk_capacity: u32,
    glyph_count: u32,
    /// When mapped in host-visible memory, base address of the contiguous
    /// slot slice.
    mapped_base: Option<usize>,
    _slot: std::marker::PhantomData<S>,
}

impl<S: SlotRecord> SlotStorage<S> {
    /// The slot stride in bytes.
    pub const SLOT_BYTES: u64 = std::mem::size_of::<S>() as u64;

    pub fn new(
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
            _slot: std::marker::PhantomData,
        }
    }

    /// The storage `source` describes: a Device source's buffers bound as-is
    /// (no upload, no transcode, no copy), a Host source's records transcoded
    /// by `transcode` and uploaded.
    pub fn from_source<T: Transcode<Slot = S>>(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        source: SlotSource<'_>,
        transcode: &T,
        labels: &UploadLabels,
    ) -> Self {
        match source {
            SlotSource::Device { chunk_capacity, chunks, mapped_base, glyph_count, .. } => {
                Self::new(chunks.to_vec(), chunk_capacity, glyph_count, mapped_base)
            }
            SlotSource::Host { slices, glyph_count, direct_host_upload } => {
                let HostUpload { chunk_capacity, chunk_counts, buffers } =
                    upload_host_slots(device, queue, &slices, glyph_count, direct_host_upload, transcode, labels);
                let chunks = buffers
                    .into_iter()
                    .zip(chunk_counts.iter())
                    .map(|(buffer, &slots)| SlotChunk { buffer, offset: 0, slots })
                    .collect();
                Self::new(chunks, chunk_capacity, glyph_count, None)
            }
        }
    }

    pub fn chunks(&self) -> &[SlotChunk] {
        &self.chunks
    }

    pub fn chunk_counts(&self) -> &[u32] {
        &self.chunk_counts
    }

    pub fn chunk_capacity(&self) -> u32 {
        self.chunk_capacity
    }

    pub fn glyph_count(&self) -> u32 {
        self.glyph_count
    }

    /// The buffer holding `slot` and the byte address of `slot`'s first byte
    /// plus `field_offset` within it.
    pub fn address(&self, slot: u32, field_offset: u64) -> (&wgpu::Buffer, u64) {
        let chunk = &self.chunks[(slot / self.chunk_capacity) as usize];
        let local = (slot % self.chunk_capacity) as u64;
        (&chunk.buffer, chunk.offset + local * Self::SLOT_BYTES + field_offset)
    }

    /// Partial slot upload: `data` at byte `field_offset` within a slot
    /// (4-aligned offsets — write_buffer's requirement).
    pub fn write_field(&self, queue: &wgpu::Queue, slot: u32, field_offset: u64, data: &[u8]) {
        let (buffer, offset) = self.address(slot, field_offset);
        queue.write_buffer(buffer, offset, data);
    }

    /// Whole-slot upload of a contiguous run, one write per chunk it touches.
    pub fn write_slots(&self, queue: &wgpu::Queue, first_slot: u32, slots: &[S]) {
        for (_chunk, local, run_offset) in split_at_chunks(first_slot, slots.len() as u32, self.chunk_capacity) {
            let piece = &slots[run_offset as usize..(run_offset + local.len() as u32) as usize];
            let (buffer, offset) = self.address(first_slot + run_offset, 0);
            queue.write_buffer(buffer, offset, bytemuck::cast_slice(piece));
        }
    }

    /// Bulk color write. Mapped storage is written in place through the base
    /// pointer; otherwise one 4 B queue write per slot.
    pub fn write_colors(&self, queue: &wgpu::Queue, first_slot: u32, colors: &[u32]) {
        if colors.is_empty() {
            return;
        }
        if let Some(addr) = self.mapped_base {
            let slots = addr as *mut u8;
            let total = self.glyph_count as usize;
            let base = first_slot as usize;
            let count = colors.len().min(total.saturating_sub(base));
            let stride = Self::SLOT_BYTES as usize;
            unsafe {
                for (i, &c) in colors.iter().take(count).enumerate() {
                    // The slot's colour word. Aligned: the mapped slice is
                    // slot-aligned, and every mode's stride and colour offset
                    // are multiples of 4 (each pins its layout in slot.rs).
                    slots.add((base + i) * stride + S::COLOR_OFFSET as usize).cast::<u32>().write(c);
                }
            }
        } else {
            for (i, &color) in colors.iter().enumerate() {
                let slot = first_slot + i as u32;
                if slot >= self.glyph_count {
                    break;
                }
                self.write_field(queue, slot, S::COLOR_OFFSET, bytemuck::bytes_of(&color));
            }
        }
    }
}
