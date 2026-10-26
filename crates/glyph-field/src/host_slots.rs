//! A field's slots resident on the host, in blocks (2026-10-26, for the
//! makepad host).
//!
//! The Derived field keeps every slot in host memory as fixed-size blocks
//! of `u32` words, each its own `Arc<[u32]>`, and the blocks are the field's
//! chunks: block `k` holds slots `k * block_slots ..`, the same slot math the
//! cull and the pick key on (`chunk_capacity`). The wgpu field uploads one
//! buffer per block from them; a makepad host publishes each `Arc` as it
//! is, and holds it while the GPU reads it, so the platform keeps one host
//! copy of the slots whichever renderer draws.
//!
//! A write is copy-on-write per block (`Arc::make_mut`): in place while the
//! field alone holds the block, a fresh copy of that one block while a
//! renderer still holds the old one. A block's identity is its generation:
//! a consumer re-reads block `k` exactly when its `Arc` is not the one it
//! holds. Why a block and not one publication with pieces: makepad's native
//! frame builder re-uploads a draw item's whole buffer whenever its
//! publication changes, so the block is the unit of an edit's upload.

use std::sync::Arc;

/// The slots, `slot_words` words each, in blocks of `block_slots` slots
/// (the last may hold fewer; an empty field holds one zeroed slot, as an
/// empty field binds one).
#[derive(Clone, Debug)]
pub struct HostSlots {
    blocks: Vec<Arc<[u32]>>,
    block_slots: usize,
    slot_words: usize,
    len: usize,
}

impl HostSlots {
    /// Zeroed blocks for `len` slots, ready to be written in place
    /// ([`Self::block_words_mut`]) before any renderer holds them.
    pub fn zeroed(len: usize, block_slots: usize, slot_words: usize) -> Self {
        assert!(block_slots > 0 && slot_words > 0, "a block holds at least one slot of at least one word");
        let stored = len.max(1);
        let blocks = (0..stored.div_ceil(block_slots))
            .map(|k| {
                let slots = (stored - k * block_slots).min(block_slots);
                // SAFETY: zero is a valid u32, so a zeroed slice is initialised.
                unsafe { Arc::<[u32]>::new_zeroed_slice(slots * slot_words).assume_init() }
            })
            .collect();
        Self { blocks, block_slots, slot_words, len }
    }

    pub fn blocks(&self) -> &[Arc<[u32]>] {
        &self.blocks
    }

    /// Slots per block: the field's chunk capacity.
    pub fn block_slots(&self) -> usize {
        self.block_slots
    }

    pub fn slot_words(&self) -> usize {
        self.slot_words
    }

    /// Live slots (0 for an empty field, which still stores one).
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Slots block `k` stores.
    pub fn block_len(&self, k: usize) -> usize {
        self.blocks[k].len() / self.slot_words
    }

    /// Every block's words, writable, for the producer that fills them right
    /// after [`Self::zeroed`]. Panics if anything else holds a block: once a
    /// renderer may hold them, writes go through [`Self::write`].
    pub fn block_words_mut(&mut self) -> Vec<&mut [u32]> {
        self.blocks
            .iter_mut()
            .map(|b| Arc::get_mut(b).expect("a block is written in place only before anything else holds it"))
            .collect()
    }

    /// Write `words` into slot `slot` from word `word` on (they must stay
    /// inside the slot), copying the block first if anything else holds it.
    pub fn write(&mut self, slot: u32, word: usize, words: &[u32]) {
        assert!(word + words.len() <= self.slot_words, "a slot write stays inside its slot");
        let (k, at) = self.locate(slot);
        let block = Arc::make_mut(&mut self.blocks[k]);
        block[at + word..at + word + words.len()].copy_from_slice(words);
    }

    /// Slot `slot`'s words.
    pub fn slot(&self, slot: u32) -> &[u32] {
        let (k, at) = self.locate(slot);
        &self.blocks[k][at..at + self.slot_words]
    }

    /// The block holding `slot` and the slot's first word in it.
    fn locate(&self, slot: u32) -> (usize, usize) {
        let slot = slot as usize;
        assert!(slot < self.len.max(1), "slot {slot} past the field's {} slots", self.len);
        (slot / self.block_slots, (slot % self.block_slots) * self.slot_words)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_cover_the_slots_and_only_the_last_is_short() {
        let slots = HostSlots::zeroed(10, 4, 5);
        let lens: Vec<usize> = (0..slots.blocks().len()).map(|k| slots.block_len(k)).collect();
        assert_eq!(lens, [4, 4, 2]);
        assert_eq!(slots.blocks()[2].len(), 10, "two slots of five words");
        let empty = HostSlots::zeroed(0, 4, 5);
        assert_eq!((empty.len(), empty.blocks().len(), empty.block_len(0)), (0, 1, 1), "an empty field stores one zeroed slot");
    }

    #[test]
    fn a_held_block_is_copied_and_only_that_block() {
        let mut slots = HostSlots::zeroed(8, 4, 5);
        let held: Vec<Arc<[u32]>> = slots.blocks().to_vec();
        slots.write(5, 3, &[7, 9]);
        assert!(Arc::ptr_eq(&held[0], &slots.blocks()[0]), "the untouched block is the same allocation");
        assert!(!Arc::ptr_eq(&held[1], &slots.blocks()[1]), "the written block is a fresh one");
        // Slot 5 is block 1's second slot: its word 3 is word 5 + 3.
        assert_eq!(held[1][5 + 3], 0, "the holder's copy is unchanged");
        assert_eq!(slots.slot(5), [0, 0, 0, 7, 9]);
        drop(held);
        let before = Arc::as_ptr(&slots.blocks()[1]);
        slots.write(4, 0, &[1]);
        assert!(std::ptr::addr_eq(before, Arc::as_ptr(&slots.blocks()[1])), "an unshared block is written in place");
        assert_eq!(slots.slot(4), [1, 0, 0, 0, 0]);
    }
}
