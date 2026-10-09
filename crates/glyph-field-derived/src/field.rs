//! [`DerivedField`]: the `GlyphField` implementation over `DerivedSlot`s (20 B).

use std::ops::Range;

use glyph_field::{
    FieldCore, FieldResources, FieldShape, FieldTargets, GlyphField, GlyphFieldMode, GlyphPlacement,
    ItemParamsGpu, SlotSource, SlotStorage,
};
use wgpu::util::DeviceExt;

use crate::pipeline::{BINDING_GLYPH_ADVANCES, BINDING_ITEM_TABLE, EXTRA_LAYOUT};
use crate::slot::{DerivedSlot, COLOR_OFFSET, ITEM_AND_GROUP_OFFSET, SLOT_BYTES, X_OFFSET};
use crate::upload::{DerivedTranscode, LABELS};

const _: () = assert!(ITEM_AND_GROUP_OFFSET == COLOR_OFFSET + 4, "write_placements writes colour and group as one 8 B run");

/// The Derived glyph field: compact 20 B slots per glyph, Y/Z derived in the
/// vertex stage from each slot's row and its item's `ItemParamsGpu`. The
/// pipelines, chunk bind groups and storage are the shared [`FieldCore`].
pub struct DerivedField {
    core: FieldCore<DerivedSlot>,
    /// Bound in every chunk's group (binding 8); held so it outlives them.
    _item_table_buffer: wgpu::Buffer,
}

impl DerivedField {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        source: SlotSource<'_>,
        resources: &FieldResources<'_>,
        targets: FieldTargets,
    ) -> Self {
        let default_items = [ItemParamsGpu::default()];
        let item_params = if resources.item_params.is_empty() {
            &default_items[..]
        } else {
            resources.item_params
        };

        // A device source for this field is the producer's direct Derived
        // emission (HyperLayout Pass 2), bound as-is. A host source
        // transcodes against the same item table.
        let storage = SlotStorage::from_source(device, queue, source, &DerivedTranscode { item_params }, &LABELS);

        let item_table_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("derived item table"),
            contents: bytemuck::cast_slice(item_params),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        });
        let extra_entries = [
            wgpu::BindGroupEntry {
                binding: BINDING_ITEM_TABLE,
                resource: item_table_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: BINDING_GLYPH_ADVANCES,
                resource: resources.glyph_advances.as_entire_binding(),
            },
        ];
        let shape = FieldShape {
            label_prefix: "derived ",
            shader_label: "glyph_field_derived.wgsl",
            shader_source: crate::GLYPH_FIELD_DERIVED_WGSL,
            extra_layout: &EXTRA_LAYOUT,
            extra_entries: &extra_entries,
        };
        let core = FieldCore::new(device, storage, resources, targets, &shape);

        Self { core, _item_table_buffer: item_table_buffer }
    }
}

impl GlyphField for DerivedField {
    fn mode(&self) -> GlyphFieldMode {
        GlyphFieldMode::Derived
    }

    fn glyph_count(&self) -> u32 {
        self.core.storage().glyph_count()
    }

    fn chunk_capacity(&self) -> u32 {
        self.core.storage().chunk_capacity()
    }

    fn slot_bytes(&self) -> u32 {
        SLOT_BYTES as u32
    }

    fn chunk_glyph_counts(&self) -> &[u32] {
        self.core.storage().chunk_counts()
    }

    fn glyph_pipeline(&self) -> &wgpu::RenderPipeline {
        self.core.glyph_pipeline()
    }

    fn create_mask_pipeline(
        &self,
        device: &wgpu::Device,
        mask_format: wgpu::TextureFormat,
        sample_count: u32,
    ) -> wgpu::RenderPipeline {
        self.core.create_mask_pipeline(device, mask_format, sample_count)
    }

    fn record_draws(&self, pass: &mut wgpu::RenderPass<'_>, draws: &[(u32, Range<u32>)]) {
        self.core.record_draws(pass, draws);
    }

    fn write_color(&self, queue: &wgpu::Queue, slot: u32, rgba: u32) {
        self.core.storage().write_field(queue, slot, COLOR_OFFSET, &rgba.to_le_bytes());
    }

    fn write_position(&self, queue: &wgpu::Queue, slot: u32, position: [f32; 3]) {
        self.core.storage().write_field(queue, slot, X_OFFSET, bytemuck::bytes_of(&position[0]));
    }

    fn write_extent(&self, _queue: &wgpu::Queue, _slot: u32, _advance: f32, _height: f32) {
        // In Derived mode, glyph advance and height are looked up from the atlas table in GPU memory.
    }

    fn write_group_id(&self, queue: &wgpu::Queue, slot: u32, group_id: u32) {
        self.core.storage().write_field(queue, slot, ITEM_AND_GROUP_OFFSET, bytemuck::bytes_of(&group_id));
    }

    /// What a Derived slot can take from a placement: X, colour and group.
    /// Its row and wrap segment are the producer's (Y/Z derive from them in
    /// the vertex stage) and the glyph id shares a word with the wrap, so
    /// those stay as loaded. Until 2026-10-09 this wrote whole slots with
    /// row and wrap zeroed, moving every edited glyph to its item's first
    /// row (C19: the highlight sidecar emptied the frame).
    fn write_placements(&self, queue: &wgpu::Queue, first_slot: u32, placements: &[GlyphPlacement]) {
        let storage = self.core.storage();
        for (i, p) in placements.iter().enumerate() {
            let slot = first_slot + i as u32;
            storage.write_field(queue, slot, X_OFFSET, bytemuck::bytes_of(&p.position[0]));
            // w3 colour and w4 group are adjacent (pinned below): one write.
            storage.write_field(queue, slot, COLOR_OFFSET, bytemuck::cast_slice(&[p.color, p.group_id]));
        }
    }

    fn write_colors(&self, queue: &wgpu::Queue, first_slot: u32, colors: &[u32]) {
        self.core.storage().write_colors(queue, first_slot, colors);
    }

    fn read_slot_words(&self, device: &wgpu::Device, queue: &wgpu::Queue, slot: u32, out: &mut [u32]) {
        self.core.read_slot_words(device, queue, slot, out);
    }
}
