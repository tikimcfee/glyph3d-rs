//! The Derived field's bind group layout and pipelines (bindings 0..10).

use glyph_field::{shared_layout_entries, FieldTargets, BINDING_SLOTS};

pub const BINDING_LINE_TABLE: u32 = 8;
pub const BINDING_ITEM_TABLE: u32 = 9;
pub const BINDING_GLYPH_ADVANCES: u32 = 10;

/// Creates the bind group layout for derived glyph field rendering (bindings 0..10):
/// 0: frame uniform
/// 1: instances (DerivedSlot)
/// 2: groups
/// 3: glyphmap
/// 4: curves
/// 5: params
/// 6: emoji sheet
/// 7: emoji sampler
/// 8: line table
/// 9: item table
/// 10: glyph advances
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
    let line_table = wgpu::BindGroupLayoutEntry {
        binding: BINDING_LINE_TABLE,
        visibility: wgpu::ShaderStages::VERTEX,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    };
    let item_table = wgpu::BindGroupLayoutEntry {
        binding: BINDING_ITEM_TABLE,
        visibility: wgpu::ShaderStages::VERTEX,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    };
    let glyph_advances = wgpu::BindGroupLayoutEntry {
        binding: BINDING_GLYPH_ADVANCES,
        visibility: wgpu::ShaderStages::VERTEX,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    };

    let mut entries = Vec::with_capacity(11);
    entries.push(frame_uniform);
    entries.push(slot_storage);
    entries.extend(rest);
    entries.push(line_table);
    entries.push(item_table);
    entries.push(glyph_advances);

    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("derived glyph field bgl"),
        entries: &entries,
    })
}

pub fn build_glyph_pipeline(
    device: &wgpu::Device,
    bgl: &wgpu::BindGroupLayout,
    targets: FieldTargets,
) -> (wgpu::ShaderModule, wgpu::PipelineLayout, wgpu::RenderPipeline) {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("glyph_field_derived.wgsl"),
        source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/glyph_field_derived.wgsl").into()),
    });

    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("derived glyph field pl"),
        bind_group_layouts: &[Some(bgl)],
        immediate_size: 0,
    });

    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("derived glyph field pipeline"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: targets.color_format,
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
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
            stencil: Default::default(),
            bias: Default::default(),
        }),
        multisample: wgpu::MultisampleState {
            count: targets.sample_count,
            ..Default::default()
        },
        multiview_mask: None,
        cache: None,
    });

    (shader, layout, pipeline)
}

pub fn build_mask_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::PipelineLayout,
    mask_format: wgpu::TextureFormat,
    sample_count: u32,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("derived selection mask pipeline"),
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
        depth_stencil: None,
        multisample: wgpu::MultisampleState {
            count: sample_count,
            ..Default::default()
        },
        multiview_mask: None,
        cache: None,
    })
}
