//! [`InstancedField`]: the `GlyphField` implementation over `RenderSlot`s.

use std::ops::Range;

use glyph_field::{
    shared_bind_group_entries, FieldResources, FieldTargets, GlyphField, GlyphFieldMode,
    GlyphPlacement, SlotChunk, SlotSource, BINDING_SLOTS,
};
use wgpu::util::DeviceExt;

use crate::pipeline::{build_glyph_bgl, build_glyph_pipeline, build_mask_pipeline};
use crate::slot::{RenderSlot, EXTENT_OFFSET, POSITION_OFFSET, SLOT_BYTES};
use crate::storage::SlotStorage;
use crate::upload::upload_host_slots;

/// The Instanced glyph field: one bind group per slot chunk over the shared
/// resources, the glyph pipeline, and the quad index buffer every glyph draw
/// expands from.
pub struct InstancedField {
    pipeline: wgpu::RenderPipeline,
    /// Kept for building mask pipelines over the same shader and layout.
    shader: wgpu::ShaderModule,
    pipeline_layout: wgpu::PipelineLayout,
    quad_index_buffer: wgpu::Buffer,
    /// Stage E2: one bind group per instance-buffer CHUNK. A repo-scale field
    /// can exceed `max_storage_buffer_binding_size` (32 B × tens of millions
    /// of glyphs), so the slots are split into buffers that each fit the
    /// binding limit; draws name a chunk. instance_index is chunk-local,
    /// which is exactly right — each chunk binding starts at its slot 0.
    bind_groups: Vec<wgpu::BindGroup>,
    storage: SlotStorage,
}

impl InstancedField {
    /// Build the field from `source`, binding it alongside the scene's shared
    /// `resources`.
    ///
    /// E2a (note 23): the shader binds the 32 B RenderSlot, so HOST (48 B)
    /// records transcode at upload — the values the vertex math reads are
    /// unchanged, so the goldens stay byte-equal. E2b: a DEVICE source (the
    /// endpoint) binds the producer's slot buffers AS-IS — no upload, no
    /// transcode, no copy; each chunk's pool-slice offset rides its binding.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        source: SlotSource<'_>,
        resources: &FieldResources<'_>,
        targets: FieldTargets,
    ) -> Self {
        let storage = match source {
            SlotSource::Device { chunk_capacity, chunks, mapped_base, glyph_count, line_table: _ } => {
                SlotStorage::new(chunks.to_vec(), chunk_capacity, glyph_count, mapped_base)
            }
            SlotSource::Host { slices, glyph_count, direct_host_upload } => {
                let upload = upload_host_slots(device, queue, &slices, glyph_count, direct_host_upload);
                let chunks = upload
                    .buffers
                    .into_iter()
                    .zip(upload.chunk_counts.iter())
                    .map(|(buffer, &slots)| SlotChunk { buffer, offset: 0, slots })
                    .collect();
                SlotStorage::new(chunks, upload.chunk_capacity, glyph_count, None)
            }
        };

        let quad_index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("quad index buffer"),
            contents: bytemuck::cast_slice(&[0u16, 1, 2, 0, 2, 3]),
            usage: wgpu::BufferUsages::INDEX,
        });

        let bgl = build_glyph_bgl(device);
        let bind_groups = build_chunk_bind_groups(device, &bgl, &storage.chunks, resources);
        let (shader, pipeline_layout, pipeline) = build_glyph_pipeline(device, &bgl, targets);

        Self { pipeline, shader, pipeline_layout, quad_index_buffer, bind_groups, storage }
    }
}

/// Stage L (O2): enumerate so captures can tell chunk bind groups apart
/// (mirrors the "glyph instances i/N" buffer labels). Each chunk's binding
/// starts at its own slot 0, ≤ the binding limit by construction: the
/// endpoint's pool slices start mid-buffer, staged uploads are offset 0, and
/// the binding's size is the chunk's live slots (never the pool page's
/// padding).
fn build_chunk_bind_groups(
    device: &wgpu::Device,
    bgl: &wgpu::BindGroupLayout,
    chunks: &[SlotChunk],
    resources: &FieldResources<'_>,
) -> Vec<wgpu::BindGroup> {
    let bind_group_count = chunks.len();
    chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let label = if bind_group_count == 1 {
                "glyph bg".to_string()
            } else {
                format!("glyph bg {i}/{bind_group_count}")
            };
            let [frame_uniform, rest @ ..] = shared_bind_group_entries(resources);
            let slot_storage = wgpu::BindGroupEntry {
                binding: BINDING_SLOTS,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &chunk.buffer,
                    offset: chunk.offset,
                    size: std::num::NonZeroU64::new(chunk.slots as u64 * SLOT_BYTES),
                }),
            };
            let mut entries = Vec::with_capacity(8);
            entries.push(frame_uniform);
            entries.push(slot_storage);
            entries.extend(rest);
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(&label),
                layout: bgl,
                entries: &entries,
            })
        })
        .collect()
}

impl GlyphField for InstancedField {
    fn mode(&self) -> GlyphFieldMode {
        GlyphFieldMode::Instanced
    }

    fn glyph_count(&self) -> u32 {
        self.storage.glyph_count
    }

    fn chunk_capacity(&self) -> u32 {
        self.storage.chunk_capacity
    }

    fn slot_bytes(&self) -> u32 {
        SLOT_BYTES as u32
    }

    fn chunk_glyph_counts(&self) -> &[u32] {
        &self.storage.chunk_counts
    }

    fn glyph_pipeline(&self) -> &wgpu::RenderPipeline {
        &self.pipeline
    }

    fn create_mask_pipeline(
        &self,
        device: &wgpu::Device,
        mask_format: wgpu::TextureFormat,
        sample_count: u32,
    ) -> wgpu::RenderPipeline {
        build_mask_pipeline(device, &self.shader, &self.pipeline_layout, mask_format, sample_count)
    }

    fn record_draws(&self, pass: &mut wgpu::RenderPass<'_>, draws: &[(u32, Range<u32>)]) {
        pass.set_index_buffer(self.quad_index_buffer.slice(..), wgpu::IndexFormat::Uint16);
        let mut bound_chunk = u32::MAX;
        for (chunk, slots) in draws {
            if *chunk != bound_chunk {
                bound_chunk = *chunk;
                pass.set_bind_group(0, &self.bind_groups[*chunk as usize], &[]);
            }
            pass.draw_indexed(0..6, 0, slots.clone());
        }
    }

    fn write_color(&self, queue: &wgpu::Queue, slot: u32, rgba: u32) {
        self.storage.write_field(queue, slot, crate::slot::COLOR_OFFSET, &rgba.to_le_bytes());
    }

    fn write_position(&self, queue: &wgpu::Queue, slot: u32, position: [f32; 3]) {
        self.storage.write_field(queue, slot, POSITION_OFFSET, bytemuck::cast_slice(&position));
    }

    fn write_extent(&self, queue: &wgpu::Queue, slot: u32, advance: f32, height: f32) {
        self.storage.write_field(queue, slot, EXTENT_OFFSET, bytemuck::cast_slice(&[advance, height]));
    }

    fn write_group_id(&self, queue: &wgpu::Queue, slot: u32, group_id: u32) {
        self.storage.write_field(queue, slot, crate::slot::GROUP_ID_OFFSET, &group_id.to_le_bytes());
    }

    fn write_placements(&self, queue: &wgpu::Queue, first_slot: u32, placements: &[GlyphPlacement]) {
        let slots: Vec<RenderSlot> = placements.iter().map(RenderSlot::from).collect();
        self.storage.write_slots(queue, first_slot, &slots);
    }

    fn write_colors(&self, queue: &wgpu::Queue, first_slot: u32, colors: &[u32]) {
        self.storage.write_colors(queue, first_slot, colors);
    }

    fn read_slot_words(&self, device: &wgpu::Device, queue: &wgpu::Queue, slot: u32, out: &mut [u32]) {
        let size = (out.len() * 4) as u64;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("debug dump"),
            size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("debug dump copy"), // Stage L (O2)
        });
        let (buffer, offset) = self.storage.address(slot, 0);
        encoder.copy_buffer_to_buffer(buffer, offset, &readback, 0, size);
        queue.submit([encoder.finish()]);
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .expect("debug dump poll failed");
        rx.recv().expect("dump cb dropped").expect("dump map failed");
        let data = slice.get_mapped_range().expect("dump range");
        out.copy_from_slice(bytemuck::cast_slice(&data[..size as usize]));
        drop(data);
        readback.unmap();
    }
}
