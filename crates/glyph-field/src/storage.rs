//! The chunked slot buffers on a wgpu device and per-slot byte addressing,
//! for any slot record.
//!
//! Hoisted from the two mode crates (C1, 2026-10-09), which carried this
//! twice with only the slot type renamed. Since 2026-10-31 it is the
//! device copy of a field's host blocks alone: one buffer per block,
//! uploaded whole at build and again whenever a block's version moves
//! ([`SlotStorage::sync`]). The field's writes land in the blocks; nothing
//! writes these buffers but the sync.

use std::cell::RefCell;

use crate::{HostSlots, SlotChunk, SlotRecord};

/// A mode's buffer labels, so captures and validation errors name the mode.
pub struct UploadLabels {
    /// The slot buffers: this, or `"{buffers} {i}/{n}"` when chunked.
    pub buffers: &'static str,
}

/// COPY_DST for the block uploads; COPY_SRC for the slot-dump verification
/// readback.
pub(crate) const SLOT_USAGE: wgpu::BufferUsages = wgpu::BufferUsages::STORAGE
    .union(wgpu::BufferUsages::COPY_DST)
    .union(wgpu::BufferUsages::COPY_SRC);

/// The host-block upload's batch: the staging wgpu holds for an upload
/// in flight is at most two of these (`SlotStorage::from_host_slots`).
pub const UPLOAD_BATCH_BYTES: usize = 256 * 1024 * 1024;

/// A queued write's staging range, handed to the rayon worker that fills
/// it. wgpu-types 30 makes `WriteOnly<T>` `Send` for a sized `T` only, so a
/// range of bytes is not, though it is exclusive access to them as a
/// `&mut [u8]` is.
struct StagingRange<'a>(wgpu::WriteOnly<'a, [u8]>);

// SAFETY: the range is the only access to its bytes for `'a` (its view is
// borrowed mutably for as long), and bytes are `Send`: moving it to another
// thread is moving a `&mut [u8]`.
unsafe impl Send for StagingRange<'_> {}

/// Where a field's slots live on the device: one buffer per host block.
///
/// `chunks[k].offset` is slot 0's byte address in `chunks[k].buffer` (0).
/// (A device slot source, the Instanced emitter's own buffers bound as-is,
/// was retired 2026-10-31.)
pub struct SlotStorage<S: SlotRecord> {
    chunks: Vec<SlotChunk>,
    /// Live slots per chunk, mirrored out of `chunks`.
    chunk_counts: Vec<u32>,
    /// Slots per chunk (the cull/pick slot math keys on it, so the
    /// renderer's chunking and the producer's must agree).
    chunk_capacity: u32,
    glyph_count: u32,
    /// Per block, the host version the buffer holds.
    uploaded: RefCell<Vec<u64>>,
    _slot: std::marker::PhantomData<S>,
}

impl<S: SlotRecord> SlotStorage<S> {
    /// The slot stride in bytes.
    pub const SLOT_BYTES: u64 = std::mem::size_of::<S>() as u64;

    /// One buffer per host block, uploaded from it.
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
    pub fn from_host_slots(device: &wgpu::Device, queue: &wgpu::Queue, slots: &HostSlots, labels: &UploadLabels) -> Self {
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
        let chunk_counts = chunks.iter().map(|c| c.slots).collect();
        let uploaded = RefCell::new((0..slots.blocks().len()).map(|k| slots.version(k)).collect());
        Self {
            chunks,
            chunk_counts,
            chunk_capacity: slots.block_slots() as u32,
            glyph_count: slots.len() as u32,
            uploaded,
            _slot: std::marker::PhantomData,
        }
    }

    /// Bring the buffers up to `slots`: every block whose version moved
    /// since its last upload goes up whole (the unit a field edit changes,
    /// as for the makepad host, whose publications re-upload whole). Same
    /// blocks, same shape: the field never resizes.
    pub fn sync(&self, queue: &wgpu::Queue, slots: &HostSlots) {
        let mut uploaded = self.uploaded.borrow_mut();
        assert_eq!(uploaded.len(), slots.blocks().len(), "the host blocks are the ones the buffers were built from");
        for (k, done) in uploaded.iter_mut().enumerate() {
            let now = slots.version(k);
            if *done != now {
                queue.write_buffer(&self.chunks[k].buffer, 0, bytemuck::cast_slice(&slots.blocks()[k]));
                *done = now;
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
}
