//! Host records → a mode's slots, on the host (2026-10-31: the CPU half of
//! what was `upload.rs`, which transcoded and uploaded in one step; the
//! upload of host records went with the Instanced field, and a field now
//! keeps its slots in host blocks that a renderer uploads).
//!
//! The 48 B engine records (`GlyphInstance`) are transcoded into a mode's
//! slot record by its [`Transcode`], in parallel, into [`HostSlots`]. The
//! input is a list of host slices rather than the `GlyphArena` that
//! produced them, so no field crate depends on the layout engine.

use crate::{GlyphInstance, HostSlots};

/// A mode's per-glyph record, as the transcode and the storage see it.
///
/// The record is the binding's element: `size_of::<Self>()` is the slot
/// stride every address is computed with.
pub trait SlotRecord: bytemuck::Pod + Send + Sync {
    /// Byte offset of the record's packed RGBA8 `u32` colour.
    const COLOR_OFFSET: u64;
}

/// Where a field's slots come from at build.
pub enum SlotSource<'a> {
    /// Already on the host in the mode's own slot format, in blocks that are
    /// the field's chunks ([`crate::HostSlots`]; the Derived field's form
    /// since 2026-10-26, as HyperLayout's Pass 2 emits them). The field
    /// keeps them.
    Blocks(crate::HostSlots),
    /// The engine's neutral records on the host, possibly in several slices
    /// that concatenate to the glyph sequence. The mode transcodes them into
    /// blocks.
    Host { slices: Vec<&'a [GlyphInstance]>, glyph_count: usize },
}

/// How a mode turns one engine record into its slot.
///
/// Implementations carry whatever the transcode reads beyond the record
/// (Derived's item table), and are shared across the parallel transcode.
pub trait Transcode: Sync {
    type Slot: SlotRecord;

    /// Write the slot for `src` to `dst`.
    ///
    /// # Safety
    /// `dst` must be valid for a write of one `Self::Slot`; it need not be
    /// initialised.
    unsafe fn transcode(&self, src: &GlyphInstance, dst: *mut Self::Slot);
}

const PARALLEL_TRANSCODE_THRESHOLD: usize = 16384;
const CHUNK_SIZE: usize = 65536;

struct SendPtr<S>(*mut S);
impl<S> Clone for SendPtr<S> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<S> Copy for SendPtr<S> {}
// SAFETY: only an address crosses threads; every write through it is an unsafe block of
// its own, and `transcode_into`'s parallel writers take disjoint slot ranges.
unsafe impl<S> Send for SendPtr<S> {}
// SAFETY: shared, it is only copied; see Send.
unsafe impl<S> Sync for SendPtr<S> {}
impl<S> SendPtr<S> {
    #[inline(always)]
    unsafe fn add(self, offset: usize) -> *mut S {
        self.0.add(offset)
    }
}

/// One binding chunk's share of the host records: the arena pieces that
/// cover it, each at its slot offset within the chunk.
struct ChunkSources<'s> {
    pieces: Vec<(usize, &'s [GlyphInstance])>,
    /// Slots the pieces cover.
    written: usize,
    /// Slots the chunk binds (`written`, or 1 for the empty field's zeroed slot).
    count: usize,
}

impl ChunkSources<'_> {
    /// Transcode every piece into `dest_ptr`, then zero the slots no piece
    /// covers.
    ///
    /// # Safety
    /// `dest_ptr` must be valid for writes of `self.count` slots, and nothing
    /// else may access them during the call.
    unsafe fn transcode_into<T: Transcode>(&self, dest_ptr: *mut T::Slot, transcode: &T) {
        let dest = SendPtr(dest_ptr);
        for &(dst_off, slice) in &self.pieces {
            if slice.len() >= PARALLEL_TRANSCODE_THRESHOLD {
                use rayon::prelude::*;
                slice
                    .par_chunks(CHUNK_SIZE)
                    .enumerate()
                    .for_each(|(chunk_idx, chunk)| {
                        // SAFETY: this chunk of the piece lies inside `dest_ptr`'s
                        // slots: `dst_off + chunk_idx * CHUNK_SIZE + chunk.len() <=
                        // dst_off + slice.len() <= self.written <= self.count`.
                        let out_ptr = unsafe { dest.add(dst_off + chunk_idx * CHUNK_SIZE) };
                        for (k, g) in chunk.iter().enumerate() {
                            // SAFETY: `k < chunk.len()`, so the slot is inside this
                            // chunk's range (above), which no other task writes.
                            unsafe {
                                transcode.transcode(g, out_ptr.add(k));
                            }
                        }
                    });
            } else {
                // SAFETY: the piece lies inside `dest_ptr`'s slots: `dst_off +
                // slice.len() <= self.written <= self.count`.
                let out_ptr = unsafe { dest.0.add(dst_off) };
                for (k, g) in slice.iter().enumerate() {
                    // SAFETY: `k < slice.len()`, so the slot is inside the piece's
                    // range (above).
                    unsafe {
                        transcode.transcode(g, out_ptr.add(k));
                    }
                }
            }
        }
        // Pad any remainder (e.g. mapped-empty arena zeroed slot):
        for k in self.written..self.count {
            // SAFETY: `self.written <= k < self.count`: inside `dest_ptr`'s slots, past
            // every piece.
            unsafe {
                std::ptr::write(dest_ptr.add(k), <T::Slot as bytemuck::Zeroable>::zeroed());
            }
        }
    }
}

/// The host records' pieces covering each chunk of `chunk_cap` slots. The
/// render chunks (slot stride) intersect the arena's own chunks (host Vec or
/// mapped slices): the two chunkings do not in general coincide, and a
/// straddling range must transcode bit-identically to a contiguous one (the
/// transcode is per record).
fn chunk_sources<'s>(slices: &[&'s [GlyphInstance]], chunk_counts: &[u32], chunk_cap: usize) -> Vec<ChunkSources<'s>> {
    let mut ac = 0usize;
    let mut arena_base = 0usize;
    chunk_counts
        .iter()
        .enumerate()
        .map(|(i, &count)| {
            let first = i * chunk_cap;
            let need_end = first + count as usize;
            let mut chunk = ChunkSources { pieces: Vec::new(), written: 0, count: count as usize };
            while chunk.written < chunk.count && ac < slices.len() {
                let c = slices[ac];
                if arena_base + c.len() <= first {
                    arena_base += c.len();
                    ac += 1;
                    continue;
                }
                let lo = first.saturating_sub(arena_base);
                let hi = (need_end - arena_base).min(c.len());
                if hi > lo {
                    chunk.pieces.push((chunk.written, &c[lo..hi]));
                    chunk.written += hi - lo;
                }
                if hi == c.len() {
                    arena_base += c.len();
                    ac += 1;
                }
            }
            chunk
        })
        .collect()
}

/// The host records transcoded into host blocks of `block_slots` slots (a
/// field keeps its slots on the host: Derived, 2026-10-26), for
/// [`SlotSource::Blocks`].
pub fn transcode_host_slots<T: Transcode>(
    slices: &[&[GlyphInstance]],
    glyph_count: usize,
    block_slots: usize,
    transcode: &T,
) -> HostSlots {
    let slot_bytes = std::mem::size_of::<T::Slot>();
    assert!(
        slot_bytes.is_multiple_of(4) && std::mem::align_of::<T::Slot>() <= 4,
        "a host block is u32 words: the slot must be whole words, aligned to at most 4"
    );
    let mut slots = HostSlots::zeroed(glyph_count, block_slots, slot_bytes / 4);
    let counts: Vec<u32> = (0..slots.blocks().len()).map(|k| slots.block_len(k) as u32).collect();
    let sources = chunk_sources(slices, &counts, block_slots);
    for (block, chunk) in slots.block_words_mut().into_iter().zip(&sources) {
        // SAFETY: the block holds exactly `chunk.count` slots (its words are
        // `count * slot_bytes / 4`, the counts came from the blocks), is
        // aligned for a u32 and so for the slot (asserted above), and is
        // borrowed mutably for the call.
        unsafe {
            chunk.transcode_into(block.as_mut_ptr().cast::<T::Slot>(), transcode);
        }
    }
    slots
}
