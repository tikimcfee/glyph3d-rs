//! What every mode's field is, beyond its slot format and extra bindings: the
//! glyph and mask pipelines, one bind group per slot chunk, the quad index
//! buffer every glyph draw expands from, and the slot storage.
//!
//! Hoisted from the two mode crates (C1, 2026-10-09), whose `field.rs` and
//! `pipeline.rs` were the same code with a `"derived "` label prefix, the
//! Derived mode's two extra bindings, and its own WGSL. The pipeline state
//! was moved verbatim from `native/src/glyph_scene/pipelines.rs` when the
//! field split into modes.

use std::ops::Range;

use crate::{
    shared_bind_group_entries, shared_layout_entries, FieldResources, FieldTargets, SlotChunk,
    SlotRecord, SlotStorage, BINDING_SLOTS,
};

/// A mode's contribution to the shared pipelines and bind groups.
pub struct FieldShape<'a> {
    /// Prefixed to every label (`""` for Instanced, `"derived "` for
    /// Derived), so captures and validation errors name the mode.
    pub label_prefix: &'static str,
    /// The WGSL module: its label and source. Entry points are `vs_main` and
    /// `fs_main`.
    pub shader_label: &'static str,
    pub shader_source: &'static str,
    /// Layout entries after the shared map (bindings above 7).
    pub extra_layout: &'a [wgpu::BindGroupLayoutEntry],
    /// The matching resources, bound identically in every chunk's group.
    pub extra_entries: &'a [wgpu::BindGroupEntry<'a>],
}

/// The shared half of a glyph field. A mode wraps one and adds its slot
/// writes; the `GlyphField` methods that do not depend on the slot layout
/// delegate here.
pub struct FieldCore<S: SlotRecord> {
    pipeline: wgpu::RenderPipeline,
    /// Kept for building mask pipelines over the same shader and layout.
    shader: wgpu::ShaderModule,
    pipeline_layout: wgpu::PipelineLayout,
    quad_index_buffer: wgpu::Buffer,
    /// One bind group per slot-buffer CHUNK. A repo-scale field can exceed
    /// `max_storage_buffer_binding_size` (tens of millions of glyphs), so the
    /// slots are split into buffers that each fit the binding limit; draws
    /// name a chunk. instance_index is chunk-local, which is exactly right —
    /// each chunk binding starts at its slot 0.
    bind_groups: Vec<wgpu::BindGroup>,
    label_prefix: &'static str,
    storage: SlotStorage<S>,
}

impl<S: SlotRecord> FieldCore<S> {
    pub fn new(
        device: &wgpu::Device,
        storage: SlotStorage<S>,
        resources: &FieldResources<'_>,
        targets: FieldTargets,
        shape: &FieldShape<'_>,
    ) -> Self {
        let p = shape.label_prefix;
        let quad_index_buffer = wgpu::util::DeviceExt::create_buffer_init(
            device,
            &wgpu::util::BufferInitDescriptor {
                label: Some(&format!("{p}quad index buffer")),
                contents: bytemuck::cast_slice(&[0u16, 1, 2, 0, 2, 3]),
                usage: wgpu::BufferUsages::INDEX,
            },
        );
        let bgl = build_glyph_bgl(device, shape);
        let bind_groups = build_chunk_bind_groups::<S>(device, &bgl, storage.chunks(), resources, shape);
        let (shader, pipeline_layout, pipeline) = build_glyph_pipeline(device, &bgl, targets, shape);
        Self { pipeline, shader, pipeline_layout, quad_index_buffer, bind_groups, label_prefix: p, storage }
    }

    pub fn storage(&self) -> &SlotStorage<S> {
        &self.storage
    }

    pub fn glyph_pipeline(&self) -> &wgpu::RenderPipeline {
        &self.pipeline
    }

    /// The selection mask pipeline — the same shader and layout (the
    /// per-chunk bind groups work unchanged); only the target format
    /// (MASK_FORMAT — data, not sRGB) and blend (coverage overwrite) differ.
    /// Shader path only: the copy path (offscreen) never renders selection
    /// visuals — the scene decides whether to build it.
    pub fn create_mask_pipeline(
        &self,
        device: &wgpu::Device,
        mask_format: wgpu::TextureFormat,
        sample_count: u32,
    ) -> wgpu::RenderPipeline {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(&format!("{}selection mask pipeline", self.label_prefix)),
            layout: Some(&self.pipeline_layout),
            vertex: wgpu::VertexState {
                module: &self.shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &self.shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: mask_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None, // the mask is flat 2D coverage
            multisample: wgpu::MultisampleState {
                count: sample_count,
                ..Default::default()
            },
            multiview_mask: None,
            cache: None,
        })
    }

    pub fn record_draws(&self, pass: &mut wgpu::RenderPass<'_>, draws: &[(u32, Range<u32>)]) {
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

    /// Copy `out.len()` words starting at `slot` back from the device
    /// (blocking; a verification readback, not a frame path).
    pub fn read_slot_words(&self, device: &wgpu::Device, queue: &wgpu::Queue, slot: u32, out: &mut [u32]) {
        let p = self.label_prefix;
        let size = (out.len() * 4) as u64;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(&format!("{p}debug dump")),
            size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some(&format!("{p}debug dump copy")),
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

/// The bind group layout: the shared map plus binding 1, this mode's slot
/// storage, then the mode's extra entries.
fn build_glyph_bgl(device: &wgpu::Device, shape: &FieldShape<'_>) -> wgpu::BindGroupLayout {
    let [frame_uniform, rest @ ..] = shared_layout_entries();
    let slot_storage = wgpu::BindGroupLayoutEntry {
        binding: BINDING_SLOTS,
        visibility: wgpu::ShaderStages::VERTEX,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    };
    let mut entries = Vec::with_capacity(8 + shape.extra_layout.len());
    entries.push(frame_uniform);
    entries.push(slot_storage);
    entries.extend(rest);
    entries.extend_from_slice(shape.extra_layout);
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(&format!("{}glyph field bgl", shape.label_prefix)),
        entries: &entries,
    })
}

/// One bind group per chunk, enumerated so captures can tell them apart
/// (mirroring the "glyph instances i/N" buffer labels). Each chunk's binding
/// starts at its own slot 0, ≤ the binding limit by construction: a device
/// source's pool slices start mid-buffer, staged uploads are offset 0, and
/// the binding's size is the chunk's live slots (never the pool page's
/// padding).
fn build_chunk_bind_groups<S: SlotRecord>(
    device: &wgpu::Device,
    bgl: &wgpu::BindGroupLayout,
    chunks: &[SlotChunk],
    resources: &FieldResources<'_>,
    shape: &FieldShape<'_>,
) -> Vec<wgpu::BindGroup> {
    let bind_group_count = chunks.len();
    chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let p = shape.label_prefix;
            let label = if bind_group_count == 1 {
                format!("{p}glyph bg")
            } else {
                format!("{p}glyph bg {i}/{bind_group_count}")
            };
            let [frame_uniform, rest @ ..] = shared_bind_group_entries(resources);
            let slot_storage = wgpu::BindGroupEntry {
                binding: BINDING_SLOTS,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &chunk.buffer,
                    offset: chunk.offset,
                    size: std::num::NonZeroU64::new(chunk.slots as u64 * SlotStorage::<S>::SLOT_BYTES),
                }),
            };
            let mut entries = Vec::with_capacity(8 + shape.extra_entries.len());
            entries.push(frame_uniform);
            entries.push(slot_storage);
            entries.extend(rest);
            entries.extend_from_slice(shape.extra_entries);
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(&label),
                layout: bgl,
                entries: &entries,
            })
        })
        .collect()
}

/// The Slug analytic-coverage glyph render pipeline.
fn build_glyph_pipeline(
    device: &wgpu::Device,
    bgl: &wgpu::BindGroupLayout,
    targets: FieldTargets,
    shape: &FieldShape<'_>,
) -> (wgpu::ShaderModule, wgpu::PipelineLayout, wgpu::RenderPipeline) {
    let p = shape.label_prefix;
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(shape.shader_label),
        source: wgpu::ShaderSource::Wgsl(shape.shader_source.into()),
    });

    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(&format!("{p}glyph field pl")),
        bind_group_layouts: &[Some(bgl)],
        immediate_size: 0,
    });

    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(&format!("{p}glyph field pipeline")),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[], // geometry from vertex_index — no vertex buffers
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                // The POOL format (the composite pass targets the driver's
                // format instead).
                format: targets.color_format,
                // Premultiplied-alpha compositing: fragment outputs
                // rgb·alpha and alpha; ONE / 1−SrcAlpha is the correct
                // coverage composite (see the shader header's alpha note).
                blend: Some(wgpu::BlendState {
                    color: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::One,
                        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                        operation: wgpu::BlendOperation::Add,
                    },
                    alpha: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::One,
                        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                        operation: wgpu::BlendOperation::Add,
                    },
                }),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            cull_mode: None,
            ..Default::default()
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: targets.depth_format,
            // Blended coverage pass that WRITES depth. It used to test
            // only, which made glyph-vs-glyph visibility a fact about
            // draw order — fine while every glyph sat at z=0, wrong the
            // moment `--wrap-mode back` put wrapped segments behind the
            // page plane. The cost of writing: a glyph's ≤1 px coverage
            // fringe also writes depth, so where a NEARER glyph is drawn
            // before a farther one, the farther one's ink under that
            // fringe is rejected rather than blended — a faint halo, at
            // edges, only where two glyphs at different depths overlap on
            // screen. The wrong-order alternative was whole glyphs.
            // GreaterEqual (reverse-Z): coplanar fragments still pass, so a
            // flat page blends in exactly the order it did before.
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
            stencil: Default::default(),
            bias: Default::default(),
        }),
        multisample: wgpu::MultisampleState {
            count: targets.sample_count, // a loud non-MSAA pin
            ..Default::default()
        }, // AA is analytic
        multiview_mask: None,
        cache: None,
    });

    (shader, layout, pipeline)
}
