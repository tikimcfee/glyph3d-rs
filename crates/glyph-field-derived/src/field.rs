//! [`DerivedField`]: the `GlyphField` implementation over `DerivedSlot`s
//! (20 B), on the host; [`crate::field_gpu::DerivedFieldGpu`] draws it.

use std::cell::{Cell, Ref, RefCell};

use glyph_field::{GlyphField, GlyphFieldMode, GlyphPlacement, HostSlots, ItemParamsGpu, SlotSource};

use crate::slot::{COLOR_OFFSET, ITEM_AND_GROUP_OFFSET, SLOT_BYTES, X_OFFSET};
use crate::upload::DerivedTranscode;

const _: () = assert!(ITEM_AND_GROUP_OFFSET == COLOR_OFFSET + 4, "write_placements writes colour and group as one 8 B run");

/// The Derived glyph field: compact 20 B slots per glyph, Y/Z derived in the
/// vertex stage from each slot's row and its item's `ItemParamsGpu`.
///
/// The slots live on the host, in blocks that are the field's chunks
/// (2026-10-26), and every edit lands there (2026-10-31: until then the
/// field held its device copy too and wrote both). A renderer draws from
/// them: the wgpu one through a `DerivedFieldGpu`, which uploads the blocks
/// whose version moved; the makepad host by publishing the blocks.
pub struct DerivedField {
    slots: RefCell<HostSlots>,
    /// Live slots per chunk (the blocks' counts; fixed for the field's life).
    chunk_counts: Vec<u32>,
    /// The item table, one row per item (an empty scene's one default row).
    item_table: Vec<ItemParamsGpu>,
    /// Each item's group row, as the item table carries it: a verb that
    /// sets a glyph back to it clears the glyph's override.
    item_groups: Vec<u32>,
    /// The group overrides (the shader's binding 10, `derive.rs`):
    /// `OVERRIDE_MAX + 1` words, index 0 unused. Filled as group verbs
    /// allocate rows; its version bumps with every write.
    group_overrides: RefCell<Vec<u32>>,
    overrides_version: Cell<u64>,
    /// The next free override index; never reused (a verb's group row is
    /// never freed either).
    next_override: Cell<u32>,
}

impl DerivedField {
    /// The field over `source`: host blocks as HyperLayout's Pass 2 emitted
    /// them, kept; or host records, transcoded into blocks of `block_slots`
    /// slots (`[glyph_scene] derived_block_slots`) against `item_params`,
    /// `item_of_group` naming each group row's item (`glyph_field::NO_ITEM`
    /// for a row no single item draws with).
    pub fn new(source: SlotSource<'_>, item_params: &[ItemParamsGpu], item_of_group: &[u32], block_slots: usize) -> Self {
        let default_items = [ItemParamsGpu::default()];
        let item_table = if item_params.is_empty() { &default_items[..] } else { item_params };
        let transcode = DerivedTranscode { item_params: item_table, item_of_group, no_items: item_params.is_empty() };
        let slots = match source {
            SlotSource::Blocks(slots) => slots,
            SlotSource::Host { slices, glyph_count } => {
                glyph_field::transcode_host_slots(&slices, glyph_count, block_slots, &transcode)
            }
        };
        let chunk_counts = (0..slots.blocks().len()).map(|k| slots.block_len(k) as u32).collect();
        Self {
            slots: RefCell::new(slots),
            chunk_counts,
            item_groups: item_table.iter().map(|p| p.group).collect(),
            item_table: item_table.to_vec(),
            group_overrides: RefCell::new(vec![0; crate::derive::OVERRIDE_MAX as usize + 1]),
            overrides_version: Cell::new(0),
            next_override: Cell::new(1),
        }
    }

    /// `data` at byte `field_offset` of `slot`, whole words, into its block.
    fn write_field(&self, slot: u32, field_offset: u64, data: &[u8]) {
        let (whole, _) = data.as_chunks::<4>();
        let words: Vec<u32> = whole.iter().map(|w| u32::from_le_bytes(*w)).collect();
        self.slots.borrow_mut().write(slot, (field_offset / 4) as usize, &words);
    }
}

impl GlyphField for DerivedField {
    fn mode(&self) -> GlyphFieldMode {
        GlyphFieldMode::Derived
    }

    fn glyph_count(&self) -> u32 {
        self.slots.borrow().len() as u32
    }

    fn chunk_capacity(&self) -> u32 {
        self.slots.borrow().block_slots() as u32
    }

    fn slot_bytes(&self) -> u32 {
        SLOT_BYTES as u32
    }

    fn chunk_glyph_counts(&self) -> &[u32] {
        &self.chunk_counts
    }

    fn write_color(&self, slot: u32, rgba: u32) {
        self.write_field(slot, COLOR_OFFSET, &rgba.to_le_bytes());
    }

    fn write_position(&self, slot: u32, position: [f32; 3]) {
        self.write_field(slot, X_OFFSET, bytemuck::bytes_of(&position[0]));
    }

    fn write_extent(&self, _slot: u32, _advance: f32, _height: f32) {
        // In Derived mode, glyph advance and height are looked up from the atlas table in GPU memory.
    }

    /// The lane stays the glyph's ITEM (Y/Z derive from it); the group
    /// rides as an override index into the resident table, or none when the
    /// group is the item's own again (`derive.rs`).
    fn write_group_id(&self, slot: u32, item: u32, group_id: u32) {
        let own = self.item_groups.get(item as usize).copied().unwrap_or(item);
        let lane = if group_id == own {
            crate::derive::item_lane(item)
        } else {
            let idx = self.next_override.get();
            if idx > crate::derive::OVERRIDE_MAX {
                log::warn!("derived field: out of group overrides ({} in use); glyph keeps its group", crate::derive::OVERRIDE_MAX);
                return;
            }
            self.next_override.set(idx + 1);
            self.group_overrides.borrow_mut()[idx as usize] = group_id;
            self.overrides_version.set(self.overrides_version.get() + 1);
            crate::derive::override_lane(item, idx)
        };
        self.write_field(slot, ITEM_AND_GROUP_OFFSET, bytemuck::bytes_of(&lane));
    }

    /// What a Derived slot can take from a placement: X, colour and the
    /// plain lane of its item (`GlyphPlacement::item` — its view; the group
    /// it draws with is that item's, from the item table), so a recolour
    /// clears any override the glyph had, as it replaced its group before.
    /// Its row and wrap segment are the producer's (Y/Z derive from them in
    /// the vertex stage) and the glyph id shares a word with the wrap, so
    /// those stay as loaded. Until 2026-10-09 this wrote whole slots with
    /// row and wrap zeroed, moving every edited glyph to its item's first
    /// row (C19: the highlight sidecar emptied the frame).
    fn write_placements(&self, first_slot: u32, placements: &[GlyphPlacement]) {
        for (i, p) in placements.iter().enumerate() {
            let slot = first_slot + i as u32;
            self.write_field(slot, X_OFFSET, bytemuck::bytes_of(&p.position[0]));
            // w3 colour and w4 group are adjacent (pinned above): one write.
            self.write_field(slot, COLOR_OFFSET, bytemuck::cast_slice(&[p.color, crate::derive::item_lane(p.item)]));
        }
    }

    fn write_colors(&self, first_slot: u32, colors: &[u32]) {
        let count = self.glyph_count();
        for (i, &color) in colors.iter().enumerate() {
            let slot = first_slot + i as u32;
            if slot >= count {
                break;
            }
            self.write_field(slot, COLOR_OFFSET, &color.to_le_bytes());
        }
    }

    fn host_slots(&self) -> Ref<'_, HostSlots> {
        self.slots.borrow()
    }

    fn item_table(&self) -> &[ItemParamsGpu] {
        &self.item_table
    }

    fn group_overrides(&self) -> Ref<'_, [u32]> {
        Ref::map(self.group_overrides.borrow(), Vec::as_slice)
    }

    fn group_overrides_version(&self) -> u64 {
        self.overrides_version.get()
    }
}
