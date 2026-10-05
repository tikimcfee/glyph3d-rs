//! What a field mode is built from: the scene-owned GPU resources every
//! mode's shader binds, the render targets, and the slot storage source.
//!
//! The binding map is shared across modes so the scene's resources (frame
//! uniform, group table, atlas textures, params, emoji sheet) bind the same
//! way whichever mode draws. A mode owns exactly one binding of its own —
//! [`BINDING_SLOTS`], its per-glyph slot storage, whose format is the mode's
//! business.
//!
//! | binding | resource                         | stages            |
//! |---------|----------------------------------|-------------------|
//! | 0       | frame uniform (camera)           | vertex            |
//! | 1       | the MODE's slot storage          | vertex (mode's)   |
//! | 2       | group table (`GroupRow` × N)     | vertex            |
//! | 3       | glyph map (u32 2D)               | vertex            |
//! | 4       | curves (u32 2D)                  | fragment          |
//! | 5       | params uniform                   | vertex + fragment |
//! | 6       | emoji sheet (filterable 2D array)| fragment          |
//! | 7       | emoji sampler                    | fragment          |

use crate::GlyphInstance;

/// The frame uniform's binding (the camera block).
pub const BINDING_FRAME_UNIFORM: u32 = 0;
/// The one binding a mode supplies itself: its per-glyph slot storage.
pub const BINDING_SLOTS: u32 = 1;

/// The scene-owned resources every mode's bind group references. Borrowed
/// only for construction — wgpu bind groups keep what they bind alive.
pub struct FieldResources<'a> {
    pub frame_uniform: &'a wgpu::Buffer,
    pub group_table: &'a wgpu::Buffer,
    pub glyph_map: &'a wgpu::TextureView,
    pub curves: &'a wgpu::TextureView,
    pub params: &'a wgpu::Buffer,
    pub emoji_sheet: &'a wgpu::TextureView,
    pub emoji_sampler: &'a wgpu::Sampler,
}

/// The targets the field's main pipeline renders into.
#[derive(Clone, Copy, Debug)]
pub struct FieldTargets {
    /// The pooled scene color format the glyph pass draws into.
    pub color_format: wgpu::TextureFormat,
    pub depth_format: wgpu::TextureFormat,
    /// Scene MSAA count (1 today — coverage is analytic).
    pub sample_count: u32,
}

/// One device buffer range holding a run of a mode's slots: slot 0 starts at
/// byte `offset` of `buffer`, `slots` slots follow. Produced on device by the
/// GPU emitters (CubeCL chain, HyperLayout's device pass) in the format of
/// the mode they emit for.
#[derive(Clone, Debug)]
pub struct SlotChunk {
    pub buffer: wgpu::Buffer,
    pub offset: u64,
    pub slots: u32,
}

/// Where a field's slots come from at construction.
pub enum SlotSource<'a> {
    /// Already on device, in the mode's own slot format, chunked by the
    /// producer. Bound as-is — no upload, no transcode, no copy.
    Device {
        /// Slots per chunk (uniform; the last chunk may hold fewer). Slot
        /// `s` lives in chunk `s / chunk_capacity`.
        chunk_capacity: usize,
        chunks: &'a [SlotChunk],
        /// Total slots (the producer's length).
        glyph_count: usize,
        /// When the chunks are one contiguous host-visible mapping, its base
        /// address — lets bulk color writes skip the queue.
        mapped_base: Option<usize>,
    },
    /// The engine's neutral records on the host, possibly in several slices
    /// that concatenate to the glyph sequence. The mode converts and uploads.
    Host {
        slices: Vec<&'a [GlyphInstance]>,
        glyph_count: usize,
        /// Create storage mapped and write it in place (unified memory)
        /// instead of staging + copy. The caller decides from the adapter.
        direct_host_upload: bool,
    },
}

fn uniform_entry(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn uint_texture_entry(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Uint,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

/// The shared bind-group-layout entries, in binding order 0, 2, 3, … 7 (the
/// mode inserts its own [`BINDING_SLOTS`] entry after the first).
pub fn shared_layout_entries() -> [wgpu::BindGroupLayoutEntry; 7] {
    [
        uniform_entry(BINDING_FRAME_UNIFORM, wgpu::ShaderStages::VERTEX),
        wgpu::BindGroupLayoutEntry {
            binding: 2,
            visibility: wgpu::ShaderStages::VERTEX,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        },
        uint_texture_entry(3, wgpu::ShaderStages::VERTEX),   // glyphmap
        uint_texture_entry(4, wgpu::ShaderStages::FRAGMENT), // curves
        uniform_entry(5, wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT),
        // The emoji sheet: a filterable sRGB 2D array + its sampler.
        wgpu::BindGroupLayoutEntry {
            binding: 6,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2Array,
                multisampled: false,
            },
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 7,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        },
    ]
}

/// The shared bind-group entries matching [`shared_layout_entries`], same
/// order (0, 2, 3, … 7).
pub fn shared_bind_group_entries<'a>(resources: &FieldResources<'a>) -> [wgpu::BindGroupEntry<'a>; 7] {
    [
        wgpu::BindGroupEntry {
            binding: BINDING_FRAME_UNIFORM,
            resource: resources.frame_uniform.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 2,
            resource: resources.group_table.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 3,
            resource: wgpu::BindingResource::TextureView(resources.glyph_map),
        },
        wgpu::BindGroupEntry {
            binding: 4,
            resource: wgpu::BindingResource::TextureView(resources.curves),
        },
        wgpu::BindGroupEntry {
            binding: 5,
            resource: resources.params.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 6,
            resource: wgpu::BindingResource::TextureView(resources.emoji_sheet),
        },
        wgpu::BindGroupEntry {
            binding: 7,
            resource: wgpu::BindingResource::Sampler(resources.emoji_sampler),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared map leaves exactly one hole, at the mode's slot binding,
    /// and stays in ascending order (the mode splices its entry in after the
    /// first).
    #[test]
    fn shared_layout_leaves_only_the_slot_binding() {
        let bindings: Vec<u32> = shared_layout_entries().iter().map(|e| e.binding).collect();
        assert_eq!(bindings, [0, 2, 3, 4, 5, 6, 7]);
        assert!(!bindings.contains(&BINDING_SLOTS));
    }
}
