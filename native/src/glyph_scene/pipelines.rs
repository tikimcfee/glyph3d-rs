//! Stage C/L — Render pipelines, bind group layouts, and composite state setup.
//! Extracted from `glyph_scene.rs` to modularize shader and pipeline creation.

use crate::glyph_scene::target::{CompositeState, SelectionFx, MASK_FORMAT, POOL_FORMAT, SCENE_SAMPLE_COUNT, SELECTION_TINT};
use std::cell::Cell;
use wgpu::util::DeviceExt;

/// Creates the bind group layout for glyph field rendering (bindings 0..7).
pub(super) fn build_glyph_bgl(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    let uint_tex = |binding: u32, visibility: wgpu::ShaderStages| wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Uint,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    };
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("glyph field bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
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
            uint_tex(3, wgpu::ShaderStages::VERTEX),   // glyphmap
            uint_tex(4, wgpu::ShaderStages::FRAGMENT), // curves
            wgpu::BindGroupLayoutEntry {
                binding: 5,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
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
        ],
    })
}

/// Builds the Slug analytic-coverage glyph render pipeline.
pub(super) fn build_glyph_pipeline(
    device: &wgpu::Device,
    bgl: &wgpu::BindGroupLayout,
    depth_format: wgpu::TextureFormat,
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
                format: POOL_FORMAT,
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
            format: depth_format,
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
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: Default::default(),
            bias: Default::default(),
        }),
        multisample: wgpu::MultisampleState {
            count: SCENE_SAMPLE_COUNT, // Stage L (L3): loud non-MSAA pin
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
/// never renders selection visuals.
pub(super) fn build_mask_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::PipelineLayout,
    color_format: wgpu::TextureFormat,
) -> Option<wgpu::RenderPipeline> {
    (color_format != POOL_FORMAT).then(|| {
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
                    format: MASK_FORMAT,
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
                count: SCENE_SAMPLE_COUNT,
                ..Default::default()
            },
            multiview_mask: None,
            cache: None,
        })
    })
}

/// Stage L (L3): the composite machinery (persistent half).
pub(super) fn build_composite_state(
    device: &wgpu::Device,
    color_format: wgpu::TextureFormat,
    mask_pipeline: Option<wgpu::RenderPipeline>,
) -> CompositeState {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("composite.wgsl"),
        source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/composite.wgsl").into()),
    });
    let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("composite bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    multisampled: false,
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("composite pl"),
        bind_group_layouts: &[Some(&bgl)],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("composite pipeline"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[], // fullscreen triangle from vertex_index
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                // The DRIVER's format (e.g. Bgra8UnormSrgb windowed).
                format: color_format,
                // Exact-overwrite passthrough — NOT a blend.
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("composite sampler"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });

    // Stage L (L4): the tint half of the selection machinery.
    let selection_fx = mask_pipeline.map(|mask_pipeline| {
        let tint_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tint bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let tint_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tint pl"),
            bind_group_layouts: &[Some(&tint_bgl)],
            immediate_size: 0,
        });
        let tint_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("selection tint pipeline"),
            layout: Some(&tint_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_tint"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: POOL_FORMAT,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let tint_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("selection tint"),
            contents: bytemuck::cast_slice(&[SELECTION_TINT]),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        SelectionFx {
            mask_pipeline,
            tint_pipeline,
            tint_bgl,
            tint_buf,
        }
    });

    CompositeState {
        pipeline,
        bind_group_layout: bgl,
        sampler,
        target: None,
        parity: Cell::new(0),
        selection_fx,
    }
}
