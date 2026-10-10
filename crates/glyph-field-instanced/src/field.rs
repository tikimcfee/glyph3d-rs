//! [`InstancedField`]: the `GlyphField` implementation over `RenderSlot`s.

use std::ops::Range;

use glyph_field::{
    FieldCore, FieldResources, FieldShape, FieldTargets, GlyphField, GlyphFieldMode, GlyphPlacement,
    SlotSource, SlotStorage,
};

use crate::slot::{RenderSlot, COLOR_OFFSET, EXTENT_OFFSET, GROUP_ID_OFFSET, POSITION_OFFSET, SLOT_BYTES};
use crate::upload::{InstancedTranscode, LABELS};

const SHAPE: FieldShape<'static> = FieldShape {
    label_prefix: "",
    shader_label: "glyph_field.wgsl",
    shader_source: crate::GLYPH_FIELD_WGSL,
    extra_layout: &[],
    extra_entries: &[],
};

/// The Instanced glyph field: the shared pipelines, chunk bind groups and
/// storage ([`FieldCore`]) over `RenderSlot`s, read as-is by the vertex stage.
pub struct InstancedField {
    core: FieldCore<RenderSlot>,
}

impl InstancedField {
    /// Build the field from `source`, binding it alongside the scene's shared
    /// `resources`.
    ///
    /// The shader binds the 32 B RenderSlot, so HOST (48 B) records
    /// transcode at upload — the values the vertex math reads are unchanged,
    /// so the goldens stay byte-equal. A DEVICE source binds the producer's
    /// slot buffers AS-IS — no upload, no transcode, no copy; each chunk's
    /// pool-slice offset rides its binding.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        source: SlotSource<'_>,
        resources: &FieldResources<'_>,
        targets: FieldTargets,
    ) -> Self {
        let storage = SlotStorage::from_source(device, queue, source, &InstancedTranscode, &LABELS);
        Self { core: FieldCore::new(device, storage, resources, targets, &SHAPE) }
    }
}

impl GlyphField for InstancedField {
    fn mode(&self) -> GlyphFieldMode {
        GlyphFieldMode::Instanced
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
        self.core.storage().write_field(queue, slot, POSITION_OFFSET, bytemuck::cast_slice(&position));
    }

    fn write_extent(&self, queue: &wgpu::Queue, slot: u32, advance: f32, height: f32) {
        self.core.storage().write_field(queue, slot, EXTENT_OFFSET, bytemuck::cast_slice(&[advance, height]));
    }

    fn write_group_id(&self, queue: &wgpu::Queue, slot: u32, _item: u32, group_id: u32) {
        self.core.storage().write_field(queue, slot, GROUP_ID_OFFSET, &group_id.to_le_bytes());
    }

    fn write_placements(&self, queue: &wgpu::Queue, first_slot: u32, placements: &[GlyphPlacement]) {
        let slots: Vec<RenderSlot> = placements.iter().map(RenderSlot::from).collect();
        self.core.storage().write_slots(queue, first_slot, &slots);
    }

    fn write_colors(&self, queue: &wgpu::Queue, first_slot: u32, colors: &[u32]) {
        self.core.storage().write_colors(queue, first_slot, colors);
    }

    fn read_slot_words(&self, device: &wgpu::Device, queue: &wgpu::Queue, slot: u32, out: &mut [u32]) {
        self.core.read_slot_words(device, queue, slot, out);
    }
}
