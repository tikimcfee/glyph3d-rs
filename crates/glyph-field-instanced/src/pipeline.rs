//! The Instanced field's bind group layout and pipelines (Stage C/L), moved
//! from `native/src/glyph_scene/pipelines.rs` — same labels, same state.

use glyph_field::{shared_layout_entries, FieldTargets, BINDING_SLOTS};

/// Creates the bind group layout for glyph field rendering (bindings 0..7):
/// the shared map plus binding 1, this mode's `RenderSlot` storage.
pub fn build_glyph_bgl(device: &wgpu::Device) -> wgpu::BindGroupLayout {
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
    let mut entries = Vec::with_capacity(8);
    entries.push(frame_uniform);
    entries.push(slot_storage);
    entries.extend(rest);
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("glyph field bgl"),
        entries: &entries,
    })
}

/// Builds the Slug analytic-coverage glyph render pipeline.
pub fn build_glyph_pipeline(
    device: &wgpu::Device,
    bgl: &wgpu::BindGroupLayout,
    targets: FieldTargets,
) -> (wgpu::ShaderModule, wgpu::PipelineLayout, wgpu::RenderPipeline) {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("glyph_field.wgsl"),
        source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/glyph_field.wgsl").into()),
    });

    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("glyph field pl"),
        bind_group_layouts: &[Some(bgl)],
        immediate_size: 0,
    });

    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("glyph field pipeline"),
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
                // Stage L (L3): the POOL format (the composite pass
                // targets the driver's format instead).
                format: targets.color_format,
                // Premultiplied-alpha compositing: fragment outputs
                // rgb·alpha and alpha; ONE / 1−SrcAlpha is the correct
                // coverage composite (see shader header note).
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
            // page plane (see the backdrop pipeline's note above). The
            // cost of writing: a glyph's ≤1 px coverage fringe also
            // writes depth, so where a NEARER glyph is drawn before a
            // farther one, the farther one's ink under that fringe is
            // rejected rather than blended — a faint halo, at edges,
            // only where two glyphs at different depths overlap on
            // screen. The wrong-order alternative was whole glyphs.
            // LessEqual: coplanar fragments still pass, so a flat page
            // blends in exactly the order it did before.
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
            stencil: Default::default(),
            bias: Default::default(),
        }),
        multisample: wgpu::MultisampleState {
            count: targets.sample_count, // Stage L (L3): loud non-MSAA pin
            ..Default::default()
        }, // AA is analytic
        multiview_mask: None,
        cache: None,
    });

    (shader, layout, pipeline)
}

/// Stage L (L4): the selection mask pipeline — same glyph_field.wgsl,
/// same layout (the per-chunk bind groups work unchanged), only the
/// target format (MASK_FORMAT — data, not sRGB) and blend (coverage
/// overwrite) differ. Shader path only: the copy path (offscreen)
/// never renders selection visuals — the scene decides whether to build it.
pub fn build_mask_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::PipelineLayout,
    mask_format: wgpu::TextureFormat,
    sample_count: u32,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("selection mask pipeline"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
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
