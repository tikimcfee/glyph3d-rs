//! The glyph scene's wgpu renderer (2026-10-31): every wgpu object the
//! scene draws with, so the CPU scene (`GlyphScene`) holds none. A frame
//! is computed on the CPU (`frame.rs`, `FrameData`); `render.rs` uploads
//! what it changed into these and draws them. That is the same contract a
//! host renderer draws from (the makepad port), which builds none of this.
//!
//! Built from a finished scene ([`GlyphRenderer::new`]): it uploads the
//! atlas, the group rows, the params and the field's blocks as they are
//! then, and every later change reaches it through a frame (the group,
//! face and region runs, the params generation, the field's block and
//! overrides versions).

use std::cell::{Cell, RefCell};

use glyph_field::{FieldResources, FieldTargets, GlyphFieldGpu, GlyphFieldMode};
use glyph_field_derived::DerivedFieldGpu;
use wgpu::util::DeviceExt;

use super::cull::BackdropGpu;
use super::environment::EnvironmentGpu;
use super::instance::{FrameUniform, GroupRow};
use super::mesh::MeshPipeline;
use super::regions::RegionsGpu;
use super::target::{CompositeState, MASK_FORMAT, POOL_FORMAT, SCENE_SAMPLE_COUNT};
use super::{pipelines, GlyphScene};
use crate::gpu::GpuContext;

/// The depth format every glyph pass tests against.
pub(crate) const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

pub struct GlyphRenderer {
    /// For the pooled targets the render (re)creates at the frame's size.
    pub(super) device: wgpu::Device,
    pub(super) environment: EnvironmentGpu,
    /// The far-LOD backdrop pass (None without the cull, `--no-cull`).
    pub(super) backdrops: Option<BackdropGpu>,
    /// The library's page-face instances (None without page faces).
    pub(super) face_instances: Option<wgpu::Buffer>,
    /// The library's bounding regions (None without them).
    pub(super) regions: Option<RegionsGpu>,
    /// The glyph field's draw: the device copy of its host blocks and
    /// tables, synced before every frame's passes.
    pub(super) field: Box<dyn GlyphFieldGpu>,
    /// The frame uniform (`FrameData::uniform`), written every frame.
    pub(super) camera_buf: wgpu::Buffer,
    /// The glyph params (`FrameData::params`) and the generation it holds.
    pub(super) params_buf: wgpu::Buffer,
    pub(super) params_uploaded: Cell<u64>,
    /// The group table (96 B rows), written by the frame's runs.
    pub(super) group_buf: wgpu::Buffer,
    pub(super) mesh_pipeline: RefCell<MeshPipeline>,
    pub(super) mesh_frame_bg: wgpu::BindGroup,
    /// Keeps the emoji sheet's view alive for the field's bind groups.
    pub(super) _emoji_view: wgpu::TextureView,
    /// Stage L (L3): composite machinery — the ONLY release path (the
    /// --no-composite A/B escape hatch proved neutrality and was removed at
    /// stage end; see out/history/STAGE_L_REPORT.md).
    pub(super) composite: CompositeState,
}

impl GlyphRenderer {
    /// The scene's GPU objects on `ctx`, drawing into `color_format` (the
    /// driver's view; the passes draw into the pooled format and composite).
    pub fn new(ctx: &GpuContext, color_format: wgpu::TextureFormat, scene: &GlyphScene) -> Self {
        let t0 = std::time::Instant::now();
        let device = &ctx.device;
        let atlas = &scene.atlas;
        let atlas_gpu = crate::atlas::AtlasGpu::upload(device, &ctx.queue, atlas);
        log::info!(
            "emoji sheet bound: {} cells, {} mip levels, {:.1} MiB",
            atlas.emoji.sheet.cells.len(),
            atlas.emoji.mip_levels,
            atlas_gpu.emoji.texture_bytes as f64 / (1 << 20) as f64,
        );

        // The group table, sized for every row a verb may allocate, holding
        // the rows as they are now (`GroupNodes` starts them sent).
        let params = scene.params.get();
        let group_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("group table"),
            size: (params.max_groups as usize * std::mem::size_of::<GroupRow>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        ctx.queue.write_buffer(&group_buf, 0, bytemuck::cast_slice(&scene.nodes.rows()));

        let camera_buf = device.create_buffer(&wgpu::BufferDescriptor {
            // Stage L (L1): the widened FrameUniform buffer (104 B) — binds to
            // the unchanged 64 B WGSL block via the minimum-binding-size rule.
            label: Some("frame uniform"),
            size: std::mem::size_of::<FrameUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let emoji_view = atlas_gpu.emoji.texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("emoji sheet view"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let params_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("glyph params"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let mesh_frame_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("mesh frame bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let mesh_frame_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mesh frame bg"),
            layout: &mesh_frame_bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: camera_buf.as_entire_binding() }],
        });
        let mesh_pipeline = RefCell::new(MeshPipeline::new(device, POOL_FORMAT, &mesh_frame_bgl));

        // Trilinear, clamped: the UV rect is inset half a texel so clamping
        // never engages inside a cell; it only guards the sheet's padding.
        let emoji_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("emoji sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        let glyph_map_view = atlas_gpu.glyphmap.create_view(&Default::default());
        let curves_view = atlas_gpu.curves.create_view(&Default::default());
        let resources = FieldResources {
            frame_uniform: &camera_buf,
            group_table: &group_buf,
            glyph_map: &glyph_map_view,
            curves: &curves_view,
            params: &params_buf,
            emoji_sheet: &emoji_view,
            emoji_sampler: &emoji_sampler,
            glyph_advances: &atlas_gpu.glyph_advances,
        };
        // Stage L (L3): the glyph pipeline renders into the POOL format. The
        // AA footprint is the adapter's: analytic on Apple TBDR (C35).
        let targets = FieldTargets {
            color_format: POOL_FORMAT,
            depth_format: DEPTH_FORMAT,
            sample_count: SCENE_SAMPLE_COUNT,
            footprint: ctx.profile.glyph_footprint(),
        };
        let field: Box<dyn GlyphFieldGpu> = match scene.field.mode() {
            GlyphFieldMode::Derived => Box::new(DerivedFieldGpu::new(device, &ctx.queue, &*scene.field, &resources, targets)),
        };
        // Stage L (L4): create selection mask pipeline unconditionally so both
        // windowed platforms (Metal Bgra8UnormSrgb and Linux/Vulkan Rgba8UnormSrgb)
        // render selection visuals.
        let mask_pipeline = Some(field.create_mask_pipeline(device, MASK_FORMAT, SCENE_SAMPLE_COUNT));

        let face_instances = scene.library_faces.as_ref().map(|f| f.buffer(device));
        let regions = scene.library_regions.as_ref().map(|r| RegionsGpu::new(device, &camera_buf, POOL_FORMAT, r));
        // The backdrop pass draws the CPU cull's survivors; no cull, no pass.
        // Stage L (L3): the backdrop pipeline renders into the pool.
        let backdrops = scene
            .cull
            .as_ref()
            .map(|c| BackdropGpu::new(device, POOL_FORMAT, DEPTH_FORMAT, &camera_buf, c.segments.len() as u32));
        // Stage L (L3): the composite machinery (persistent half). The
        // pooled target itself is sized by the first frame's target.
        let composite = pipelines::build_composite_state(device, color_format, mask_pipeline);
        log::info!(
            "renderer: field upload, pipelines and tables {:.3}s ({} chunk(s), {} MiB binding limit)",
            t0.elapsed().as_secs_f64(),
            scene.field.chunk_count(),
            device.limits().max_storage_buffer_binding_size >> 20,
        );

        Self {
            device: device.clone(),
            environment: EnvironmentGpu::new(device, DEPTH_FORMAT),
            backdrops,
            face_instances,
            regions,
            field,
            camera_buf,
            params_buf,
            params_uploaded: Cell::new(scene.params_generation.get()),
            group_buf,
            mesh_pipeline,
            mesh_frame_bg,
            _emoji_view: emoji_view,
            composite,
        }
    }
}
