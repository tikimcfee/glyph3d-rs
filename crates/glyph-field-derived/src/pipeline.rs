//! The Derived field's bindings beyond the shared map. The pipelines and the
//! rest of the layout are `glyph_field::FieldCore`'s (C1).
//!
//! 0..=7 the shared map (binding 1 is the `DerivedSlot` storage), then:
//! 8 the item table (`ItemParamsGpu` per item, which Y/Z derive from), and
//! 9 the resident glyph advances (the slot carries no advance).

pub const BINDING_ITEM_TABLE: u32 = 8;
pub const BINDING_GLYPH_ADVANCES: u32 = 9;

const fn vertex_storage(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

/// The layout entries after the shared map, in binding order.
pub const EXTRA_LAYOUT: [wgpu::BindGroupLayoutEntry; 2] =
    [vertex_storage(BINDING_ITEM_TABLE), vertex_storage(BINDING_GLYPH_ADVANCES)];
