//! Stage L — the composite state setup (pool → driver view, selection tint).
//! Extracted from `glyph_scene.rs` to modularize shader and pipeline creation.
//!
//! The glyph field's own bind group layout, glyph pipeline and selection-mask
//! pipeline moved into the field-mode crates in the 2026-10 split, and from
//! there into the shared `glyph_field::FieldCore` (C1, 2026-10-09); the scene
//! asks its field for a mask pipeline and hands it in here.

use crate::glyph_scene::target::{CompositeState, SelectionFx, POOL_FORMAT};
use std::cell::Cell;
use wgpu::util::DeviceExt;

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
            contents: bytemuck::cast_slice(&[crate::config::settings().glyph_scene.selection_tint]),
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
