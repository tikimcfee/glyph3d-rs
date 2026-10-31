//! [`DerivedFieldGpu`]: a [`crate::DerivedField`]'s draw on a wgpu device
//! (2026-10-31: split from the field, which keeps its slots on the host).

use std::cell::Cell;
use std::ops::Range;

use glyph_field::{FieldCore, FieldResources, FieldShape, FieldTargets, GlyphField, GlyphFieldGpu, SlotStorage};
use wgpu::util::DeviceExt;

use crate::pipeline::{BINDING_GLYPH_ADVANCES, BINDING_GROUP_OVERRIDES, BINDING_ITEM_TABLE, EXTRA_LAYOUT};
use crate::slot::DerivedSlot;
use crate::upload::LABELS;

/// The device copy of a Derived field's blocks (one buffer per block), its
/// item table and group overrides, and the shared [`FieldCore`] pipelines
/// and per-chunk bind groups.
pub struct DerivedFieldGpu {
    core: FieldCore<DerivedSlot>,
    /// Bound in every chunk's group (binding 8); held so it outlives them.
    _item_table_buffer: wgpu::Buffer,
    /// The group overrides (binding 10), and the field's overrides version
    /// it holds.
    group_overrides: wgpu::Buffer,
    overrides_uploaded: Cell<u64>,
}

impl DerivedFieldGpu {
    /// Upload `field`'s blocks, item table and overrides, and build the
    /// pipelines over them.
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, field: &dyn GlyphField, resources: &FieldResources<'_>, targets: FieldTargets) -> Self {
        let storage = SlotStorage::from_host_slots(device, queue, &field.host_slots(), &LABELS);
        let item_table_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("derived item table"),
            contents: bytemuck::cast_slice(field.item_table()),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        });
        let group_overrides = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("derived group overrides"),
            contents: bytemuck::cast_slice(&field.group_overrides()),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        });
        let extra_entries = [
            wgpu::BindGroupEntry { binding: BINDING_ITEM_TABLE, resource: item_table_buffer.as_entire_binding() },
            wgpu::BindGroupEntry { binding: BINDING_GLYPH_ADVANCES, resource: resources.glyph_advances.as_entire_binding() },
            wgpu::BindGroupEntry { binding: BINDING_GROUP_OVERRIDES, resource: group_overrides.as_entire_binding() },
        ];
        let shape = FieldShape {
            label_prefix: "derived ",
            shader_label: "glyph_field_derived.wgsl",
            shader_source: crate::GLYPH_FIELD_DERIVED_WGSL,
            extra_layout: &EXTRA_LAYOUT,
            extra_entries: &extra_entries,
        };
        let core = FieldCore::new(device, storage, resources, targets, &shape);
        Self {
            core,
            _item_table_buffer: item_table_buffer,
            group_overrides,
            overrides_uploaded: Cell::new(field.group_overrides_version()),
        }
    }
}

impl GlyphFieldGpu for DerivedFieldGpu {
    fn sync(&self, queue: &wgpu::Queue, field: &dyn GlyphField) {
        self.core.storage().sync(queue, &field.host_slots());
        let version = field.group_overrides_version();
        if version != self.overrides_uploaded.get() {
            queue.write_buffer(&self.group_overrides, 0, bytemuck::cast_slice(&field.group_overrides()));
            self.overrides_uploaded.set(version);
        }
    }

    fn glyph_pipeline(&self) -> &wgpu::RenderPipeline {
        self.core.glyph_pipeline()
    }

    fn create_mask_pipeline(&self, device: &wgpu::Device, mask_format: wgpu::TextureFormat, sample_count: u32) -> wgpu::RenderPipeline {
        self.core.create_mask_pipeline(device, mask_format, sample_count)
    }

    fn record_draws(&self, pass: &mut wgpu::RenderPass<'_>, draws: &[(u32, Range<u32>)]) {
        self.core.record_draws(pass, draws);
    }

    fn read_slot_words(&self, device: &wgpu::Device, queue: &wgpu::Queue, slot: u32, out: &mut [u32]) {
        self.core.read_slot_words(device, queue, slot, out);
    }
}
