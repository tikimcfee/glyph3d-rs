//! The chunked slot buffers and per-slot byte addressing, for any slot record.
//!
//! Hoisted from the two mode crates (C1, 2026-10-09), which carried this
//! twice with only the slot type renamed. A storage built from host blocks
//! ([`SlotSource::Blocks`], the Derived field since 2026-10-26) also keeps
//! the blocks, and every slot write patches the block as well as the
//! buffer, so the host copy a makepad renderer draws from stays current.

use std::cell::{Ref, RefCell};

use crate::upload::{upload_host_slots, HostUpload, Transcode, UploadLabels, SLOT_USAGE, UPLOAD_BATCH_BYTES};
use crate::{split_at_chunks, HostSlots, SlotChunk, SlotSource};

/// A mode's per-glyph record, as the shared storage and upload see it.
///
/// The record is the binding's element: `size_of::<Self>()` is the slot
/// stride every address is computed with.
pub trait SlotRecord: bytemuck::Pod + Send + Sync {
    /// Byte offset of the record's packed RGBA8 `u32` colour.
    const COLOR_OFFSET: u64;
}

/// A queued write's staging range, handed to the rayon worker that fills
/// it. wgpu-types 30 makes `WriteOnly<T>` `Send` for a sized `T` only, so a
/// range of bytes is not, though it is exclusive access to them as a
/// `&mut [u8]` is.
struct StagingRange<'a>(wgpu::WriteOnly<'a, [u8]>);

// SAFETY: the range is the only access to its bytes for `'a` (its view is
// borrowed mutably for as long), and bytes are `Send`: moving it to another
// thread is moving a `&mut [u8]`.
unsafe impl Send for StagingRange<'_> {}

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
    /// The host blocks the buffers were uploaded from, one per chunk, kept
    /// current by every write (None unless built from them).
    host: Option<RefCell<HostSlots>>,
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
            host: None,
            _slot: std::marker::PhantomData,
        }
    }

    /// One buffer per host block, uploaded from it; the blocks are kept.
    ///
    /// In batches of 256 MiB (`UPLOAD_BATCH_BYTES`), at most two in flight
    /// (2026-10-26): a block is written through a staging buffer wgpu keeps
    /// until a submission carrying its copy completes, so an unbatched
    /// upload held a staging copy of every block at once, a second full
    /// copy of the slots. On an RTX 5090 that copy lands in device memory:
    /// 8.7 GiB of slots peaked 15.9 GB above idle unbatched and 9.1 GB
    /// batched, for 0.12 s more of field build.
    ///
    /// A batch's buffers and its queued writes' staging are made on this
    /// thread, then the blocks are copied into the staging in parallel (one
    /// thread left them serial: 198 ms of a 71.6M-slot tree's build, 274
    /// blocks). Only the staging ranges and the blocks cross threads, never
    /// the device (2026-10-27): proving `&wgpu::Device: Send` inside a rayon
    /// job overflows the trait solver's recursion limit under relax. The
    /// staging is a queued write's (`write_buffer_with`), not a buffer
    /// created mapped: wgpu zero-fills the latter's on the creating thread,
    /// which made this thread's share 0.21 s of drivers/net's 535 blocks
    /// (field build 0.23 s before, 0.34 s mapped, 0.17 s queued).
    pub fn from_host_slots(device: &wgpu::Device, queue: &wgpu::Queue, slots: HostSlots, labels: &UploadLabels) -> Self {
        assert_eq!(
            slots.slot_words() * 4,
            std::mem::size_of::<S>(),
            "host blocks hold this mode's slots ({} B), not {} words each",
            std::mem::size_of::<S>(),
            slots.slot_words()
        );
        use rayon::prelude::*;
        let n = slots.blocks().len();
        let block_bytes = slots.block_slots() * slots.slot_words() * 4;
        let per_batch = (UPLOAD_BATCH_BYTES / block_bytes).max(1);
        let mut chunks = Vec::with_capacity(n);
        let mut in_flight: Option<wgpu::SubmissionIndex> = None;
        for (b, batch) in slots.blocks().chunks(per_batch).enumerate() {
            let buffers: Vec<wgpu::Buffer> = batch
                .iter()
                .enumerate()
                .map(|(i, block)| {
                    let k = b * per_batch + i;
                    let label = if n == 1 { labels.buffers.to_string() } else { format!("{} {k}/{n}", labels.buffers) };
                    let bytes = std::mem::size_of_val::<[u32]>(block) as wgpu::BufferAddress;
                    device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some(&label),
                        size: bytes,
                        usage: SLOT_USAGE,
                        mapped_at_creation: false,
                    })
                })
                .collect();
            let mut views: Vec<wgpu::QueueWriteBufferView> = buffers
                .iter()
                .map(|buffer| {
                    let size = wgpu::BufferSize::new(buffer.size()).expect("a host block holds at least one slot");
                    queue.write_buffer_with(buffer, 0, size).expect("a slot buffer takes a write of its whole size")
                })
                .collect();
            let ranges: Vec<StagingRange<'_>> = views.iter_mut().map(|view| StagingRange(view.slice(..))).collect();
            ranges.into_par_iter().zip(batch.par_iter()).for_each(|(StagingRange(mut range), block)| {
                range.copy_from_slice(bytemuck::cast_slice(block));
            });
            // A view queues its copy into its buffer when it drops.
            drop(views);
            for (i, buffer) in buffers.into_iter().enumerate() {
                chunks.push(SlotChunk { buffer, offset: 0, slots: slots.block_len(b * per_batch + i) as u32 });
            }
            // An empty submit carries the batch's staging copies; waiting on
            // the one before frees that batch's staging buffers.
            let submitted = queue.submit([]);
            if let Some(previous) = in_flight.replace(submitted) {
                device
                    .poll(wgpu::PollType::Wait { submission_index: Some(previous), timeout: None })
                    .expect("the host-block upload's previous batch completes");
            }
        }
        let mut storage = Self::new(chunks, slots.block_slots(), slots.len(), None);
        storage.host = Some(RefCell::new(slots));
        storage
    }

    /// The host blocks alone, no device copy: for a field a host renderer
    /// draws from its blocks (`FieldResources::slots_on_device`). The chunk
    /// counts are the blocks'; there are no chunk buffers.
    pub fn host_only(slots: HostSlots) -> Self {
        let chunk_counts = (0..slots.blocks().len()).map(|k| slots.block_len(k) as u32).collect();
        Self {
            chunks: Vec::new(),
            chunk_counts,
            chunk_capacity: slots.block_slots() as u32,
            glyph_count: slots.len() as u32,
            mapped_base: None,
            host: Some(RefCell::new(slots)),
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
            SlotSource::Blocks(slots) => Self::from_host_slots(device, queue, slots, labels),
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

    /// Whether the slots have a device copy (every storage but
    /// [`Self::host_only`]).
    pub fn on_device(&self) -> bool {
        !self.chunks.is_empty()
    }

    /// The host blocks, when the storage keeps them.
    pub fn host_slots(&self) -> Option<Ref<'_, HostSlots>> {
        self.host.as_ref().map(RefCell::borrow)
    }

    /// Patch the host block too (when kept): `data` at byte `field_offset`
    /// of `slot`, whole words.
    fn patch_host(&self, slot: u32, field_offset: u64, data: &[u8]) {
        if let Some(host) = &self.host {
            let (whole, _) = data.as_chunks::<4>();
            let words: Vec<u32> = whole.iter().map(|w| u32::from_le_bytes(*w)).collect();
            host.borrow_mut().write(slot, (field_offset / 4) as usize, &words);
        }
    }

    /// The buffer holding `slot` and the byte address of `slot`'s first byte
    /// plus `field_offset` within it.
    pub fn address(&self, slot: u32, field_offset: u64) -> (&wgpu::Buffer, u64) {
        assert!(self.on_device(), "a host-only field has no device copy of slot {slot}");
        let chunk = &self.chunks[(slot / self.chunk_capacity) as usize];
        let local = (slot % self.chunk_capacity) as u64;
        (&chunk.buffer, chunk.offset + local * Self::SLOT_BYTES + field_offset)
    }

    /// Partial slot upload: `data` at byte `field_offset` within a slot
    /// (4-aligned offsets — write_buffer's requirement).
    pub fn write_field(&self, queue: &wgpu::Queue, slot: u32, field_offset: u64, data: &[u8]) {
        self.patch_host(slot, field_offset, data);
        if self.on_device() {
            let (buffer, offset) = self.address(slot, field_offset);
            queue.write_buffer(buffer, offset, data);
        }
    }

    /// Whole-slot upload of a contiguous run, one write per chunk it touches.
    pub fn write_slots(&self, queue: &wgpu::Queue, first_slot: u32, slots: &[S]) {
        for (_chunk, local, run_offset) in split_at_chunks(first_slot, slots.len() as u32, self.chunk_capacity) {
            let piece = &slots[run_offset as usize..(run_offset + local.len() as u32) as usize];
            for (i, slot) in piece.iter().enumerate() {
                self.patch_host(first_slot + run_offset + i as u32, 0, bytemuck::bytes_of(slot));
            }
            if self.on_device() {
                let (buffer, offset) = self.address(first_slot + run_offset, 0);
                queue.write_buffer(buffer, offset, bytemuck::cast_slice(piece));
            }
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
            // SAFETY: `mapped_base` is the host address of one contiguous mapping of
            // all `glyph_count` slots (SlotSource::Device), kept alive by
            // `self.chunks`; `count` clamps every write below `glyph_count`, and the
            // colour word is aligned (below).
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
