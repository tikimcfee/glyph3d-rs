//! Where Pass 2's emitted slots land: host blocks that are the Derived
//! field's chunks ([`emit_host_blocks`], 2026-10-26), the one host copy a
//! makepad renderer publishes and the wgpu field uploads from. No path ever
//! materializes the 48 B host record. (Until 2026-10-31 this was
//! `device_alloc.rs`, and the Instanced field's slots were emitted to the
//! device here too: mapped memory on unified Metal, staged windows
//! elsewhere.)

use rayon::prelude::*;

use crate::atlas::TrieTable;
use crate::layout::LayoutItem;
use super::pass2_device::{emit_windows, emoji_tint_pairs, layout_pass2_device, plan_windows, SlotEmit, SlotWindow, WindowSink};
use glyph_field::HostSlots;
use super::types::{ItemPrepass, Pass2DeviceOutput};

/// The per-run inputs every emission shares.
pub(crate) struct EmitInputs<'a, 'b> {
    pub items: &'a [LayoutItem<'b>],
    pub chunks: &'a [super::chunk::LayoutChunk<'b>],
    pub item_chunk_ranges: &'a [std::ops::Range<usize>],
    pub prepasses: &'a [ItemPrepass],
    pub slot_bases: &'a [u32],
    pub chunk_slot_bases: &'a [u32],
    pub chunk_base_rows: &'a [i64],
    pub chunk_record_bases: &'a [usize],
    pub chunk_initial_cols: &'a [i64],
    pub chunk_initial_seg_advs: &'a [f32],
    pub chunk_initial_line_advs: &'a [f64],
    pub trie: &'a TrieTable,
    pub bitmap_adv: f32,
    pub em_height_fu: u32,
}

impl EmitInputs<'_, '_> {
    /// Pass 2 into `dest_addr` (any writable memory sized for the total
    /// survivors of `E::Slot` — a mapped GPU buffer, or a host Vec in tests).
    pub(crate) fn run<E: SlotEmit>(&self, dest_addr: usize) -> (Pass2DeviceOutput, Vec<Vec<u32>>) {
        let sp_pass2 = tracing::info_span!("hyper.pass2").entered();
        let out = layout_pass2_device::<E>(self, dest_addr);
        drop(sp_pass2);
        // Timed apart from Pass 2: it READS the slots just written, which is
        // cheap from cached memory and was not from write-combined staging.
        let sp_tints = tracing::info_span!("hyper.emoji_tints").entered();
        let pairs = emoji_tint_pairs::<E>(dest_addr, &out);
        drop(sp_tints);
        (out, pairs)
    }
}

/// Pass 2 into host blocks of `block_slots` slots (the Derived field's form,
/// 2026-10-26): window by window through host scratch, each window then
/// copied into the blocks it covers, so peak memory is the slots plus one
/// window, never two copies of the stream. Bit for bit what
/// [`EmitInputs::run`] writes contiguously
/// (`tests::host_blocks_match_the_contiguous_emission`).
pub(crate) fn emit_host_blocks<E: SlotEmit>(
    inputs: &EmitInputs<'_, '_>,
    total_survivors: usize,
    block_slots: usize,
) -> (HostSlots, Pass2DeviceOutput, Vec<Vec<u32>>) {
    const WINDOW_BYTES: usize = 64 * 1024 * 1024;
    let slot_bytes = std::mem::size_of::<E::Slot>();
    assert!(
        slot_bytes.is_multiple_of(4) && std::mem::align_of::<E::Slot>() <= 4,
        "host blocks are u32 words: the slot must be whole words, aligned to at most 4"
    );
    let mut slots = HostSlots::zeroed(total_survivors, block_slots, slot_bytes / 4);
    let windows = plan_windows(inputs.chunk_slot_bases, total_survivors, (WINDOW_BYTES / slot_bytes).max(1));
    let mut sink = BlockSink { blocks: slots.block_words_mut(), block_slots, slot_words: slot_bytes / 4, scratch: Vec::new() };
    let sp = tracing::info_span!("hyper.pass2.blocks", windows = windows.len()).entered();
    let (pass2, pairs) = emit_windows::<E>(inputs, &windows, total_survivors, &mut sink);
    drop(sp);
    drop(sink);
    (slots, pass2, pairs)
}

/// The host-block path's sink: Pass 2 writes a window into scratch, which
/// [`WindowSink::end`] copies into the blocks the window's slots fall in.
struct BlockSink<'a> {
    blocks: Vec<&'a mut [u32]>,
    block_slots: usize,
    slot_words: usize,
    /// One window's slots, reused window to window.
    scratch: Vec<u32>,
}

impl WindowSink for BlockSink<'_> {
    fn begin(&mut self, _k: usize, window: &SlotWindow) -> usize {
        let words = (window.slots.len() * self.slot_words).max(1);
        if self.scratch.len() < words {
            self.scratch.resize(words, 0);
        }
        self.scratch.as_mut_ptr() as usize
    }

    fn end(&mut self, _k: usize, window: &SlotWindow) {
        if window.slots.is_empty() {
            return;
        }
        let (block_slots, slot_words) = (self.block_slots, self.slot_words);
        let first_block = window.slots.start / block_slots;
        let last_block = (window.slots.end - 1) / block_slots;
        let scratch = &self.scratch;
        self.blocks[first_block..=last_block].par_iter_mut().enumerate().for_each(|(i, block)| {
            let block_start = (first_block + i) * block_slots;
            let lo = window.slots.start.max(block_start);
            let hi = window.slots.end.min(block_start + block_slots);
            let from = (lo - window.slots.start) * slot_words..(hi - window.slots.start) * slot_words;
            let to = (lo - block_start) * slot_words..(hi - block_start) * slot_words;
            block[to].copy_from_slice(&scratch[from]);
        });
    }
}
