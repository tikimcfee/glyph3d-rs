//! [`DerivedField`]: the `GlyphField` implementation over `DerivedSlot`s (20 B).

use std::ops::Range;

use glyph_field::{
    shared_bind_group_entries, FieldResources, FieldTargets, GlyphField, GlyphFieldMode,
    GlyphPlacement, ItemParamsGpu, SlotChunk, SlotSource, BINDING_SLOTS,
};
use wgpu::util::DeviceExt;

use crate::pipeline::{
    build_glyph_bgl, build_glyph_pipeline, build_mask_pipeline, BINDING_GLYPH_ADVANCES,
    BINDING_ITEM_TABLE, BINDING_LINE_TABLE,
};
use crate::slot::{DerivedSlot, COLOR_OFFSET, SLOT_BYTES};
use crate::storage::DerivedSlotStorage;
use crate::upload::upload_derived_slots;

/// The Derived glyph field: compact 20 B slots per glyph, Y/Z derived from line tables.
pub struct DerivedField {
    pipeline: wgpu::RenderPipeline,
    shader: wgpu::ShaderModule,
    pipeline_layout: wgpu::PipelineLayout,
    quad_index_buffer: wgpu::Buffer,
    _line_table_buffer: wgpu::Buffer,
    _item_table_buffer: wgpu::Buffer,
    bind_groups: Vec<wgpu::BindGroup>,
    storage: DerivedSlotStorage,
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

        let (storage, line_table) = match source {
            SlotSource::Device { chunk_capacity, chunks, mapped_base, glyph_count, line_table } => {
                // A device source for this field is the producer's direct
                // Derived emission (HyperLayout Pass 2): 20 B slots whose
                // `line_idx` already indexes this table. No upload, no
                // transcode — bind as-is.
                let line_table = line_table.expect(
                    "Derived field given a device source without a line table: \
                     the producer emitted another field's slot format",
                );
                let storage = DerivedSlotStorage::new(chunks.to_vec(), chunk_capacity, glyph_count, mapped_base);
                (storage, std::borrow::Cow::Borrowed(line_table))
            }
            SlotSource::Host { slices, glyph_count, direct_host_upload } => {
                let upload = upload_derived_slots(device, queue, &slices, glyph_count, direct_host_upload, item_params);
                let chunks = upload
                    .buffers
                    .into_iter()
                    .zip(upload.chunk_counts.iter())
                    .map(|(buffer, &slots)| SlotChunk { buffer, offset: 0, slots })
                    .collect();
                let storage = DerivedSlotStorage::new(chunks, upload.chunk_capacity, glyph_count, None);
                (storage, std::borrow::Cow::Owned(upload.line_table))
            }
        };

        let quad_index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("derived quad index buffer"),
            contents: bytemuck::cast_slice(&[0u16, 1, 2, 0, 2, 3]),
            usage: wgpu::BufferUsages::INDEX,
        });

        let line_table_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("derived line table"),
            contents: bytemuck::cast_slice(&line_table),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        });

        let item_table_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("derived item table"),
            contents: bytemuck::cast_slice(item_params),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        });

        let bgl = build_glyph_bgl(device);
        let bind_groups = build_chunk_bind_groups(
            device,
            &bgl,
            &storage.chunks,
            resources,
            &line_table_buffer,
            &item_table_buffer,
        );
        let (shader, pipeline_layout, pipeline) = build_glyph_pipeline(device, &bgl, targets);

        Self {
            pipeline,
            shader,
            pipeline_layout,
            quad_index_buffer,
            _line_table_buffer: line_table_buffer,
            _item_table_buffer: item_table_buffer,
            bind_groups,
            storage,
        }
    }
}

fn build_chunk_bind_groups(
    device: &wgpu::Device,
    bgl: &wgpu::BindGroupLayout,
    chunks: &[SlotChunk],
    resources: &FieldResources<'_>,
    line_table_buf: &wgpu::Buffer,
    item_table_buf: &wgpu::Buffer,
) -> Vec<wgpu::BindGroup> {
    let bind_group_count = chunks.len();
    chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let label = if bind_group_count == 1 {
                "derived glyph bg".to_string()
            } else {
                format!("derived glyph bg {i}/{bind_group_count}")
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
            let line_storage = wgpu::BindGroupEntry {
                binding: BINDING_LINE_TABLE,
                resource: line_table_buf.as_entire_binding(),
            };
            let item_storage = wgpu::BindGroupEntry {
                binding: BINDING_ITEM_TABLE,
                resource: item_table_buf.as_entire_binding(),
            };
            let advances_storage = wgpu::BindGroupEntry {
                binding: BINDING_GLYPH_ADVANCES,
                resource: resources.glyph_advances.as_entire_binding(),
            };

            let mut entries = Vec::with_capacity(11);
            entries.push(frame_uniform);
            entries.push(slot_storage);
            entries.extend(rest);
            entries.push(line_storage);
            entries.push(item_storage);
            entries.push(advances_storage);

            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(&label),
                layout: bgl,
                entries: &entries,
            })
        })
        .collect()
}

impl GlyphField for DerivedField {
    fn mode(&self) -> GlyphFieldMode {
        GlyphFieldMode::Derived
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
        self.storage.write_field(queue, slot, COLOR_OFFSET, &rgba.to_le_bytes());
    }

    fn write_position(&self, queue: &wgpu::Queue, slot: u32, position: [f32; 3]) {
        self.storage.write_x(queue, slot, position[0]);
    }

    fn write_extent(&self, _queue: &wgpu::Queue, _slot: u32, _advance: f32, _height: f32) {
        // In Derived mode, glyph advance and height are looked up from the atlas table in GPU memory.
    }

    fn write_group_id(&self, queue: &wgpu::Queue, slot: u32, group_id: u32) {
        self.storage.write_group_id(queue, slot, group_id);
    }

    fn write_placements(&self, queue: &wgpu::Queue, first_slot: u32, placements: &[GlyphPlacement]) {
        let slots: Vec<DerivedSlot> = placements
            .iter()
            .map(|p| {
                DerivedSlot::new(
                    p.position[0],
                    0, // placements edit writes X, color, group
                    p.glyph_id as u16,
                    0,
                    p.color,
                    p.group_id,
                )
            })
            .collect();
        self.storage.write_slots(queue, first_slot, &slots);
    }

    fn write_colors(&self, queue: &wgpu::Queue, first_slot: u32, colors: &[u32]) {
        self.storage.write_colors(queue, first_slot, colors);
    }

    fn read_slot_words(&self, device: &wgpu::Device, queue: &wgpu::Queue, slot: u32, out: &mut [u32]) {
        let size = (out.len() * 4) as u64;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("derived debug dump"),
            size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("derived debug dump copy"),
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
