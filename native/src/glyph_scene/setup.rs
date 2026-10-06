use std::cell::Cell;
use glam::Vec3;
use wgpu::util::DeviceExt;

use crate::atlas::Atlas;
use crate::gpu::GpuContext;
use crate::text::StagedText;
use glyph_field::{FieldResources, FieldTargets, GlyphField, GlyphFieldMode, SlotSource};
use glyph_field_instanced::InstancedField;
use glyph_field_derived::DerivedField;
use super::{
    CameraMode, CullState, FlyCamera, GlyphScene,
    cull::SegCull,
    instance::{FrameUniform, Params, GlyphInstance, GroupRow},
    target::{MASK_FORMAT, POOL_FORMAT, SCENE_SAMPLE_COUNT},
    camera::FOV_Y,
    tint::seg_tint,
    pipelines, mesh
};

impl GlyphScene {
    pub fn new(
        ctx: &GpuContext,
        color_format: wgpu::TextureFormat,
        atlas: &Atlas,
        staged: StagedText,
        camera_mode: CameraMode,
        cull_enabled: bool,
        field_mode: GlyphFieldMode,
    ) -> Self {
        let device = &ctx.device;

        // Stage L (L3): the glyph/backdrop pipelines render into the POOL
        // format (Rgba8UnormSrgb); the composite step maps the pool into the
        // driver's view. (The --no-composite direct path proved the pool
        // draw byte-neutral and was removed at stage end.)

        // --- instance + group buffers --------------------------------------
        let mut arena = staged.instances;
        if arena.is_empty() && !arena.is_device() {
            arena.push(GlyphInstance {
                pos: [0.0; 3],
                glyph_id: 0,
                row: 0,
                col: 0,
                color: 0,
                group_id: 0,
                advance: 0.0,
                height: 0.0,
                flags: 0,
                _pad: 0,
            });
        }
        let mut groups = staged.groups;
        if groups.is_empty() {
            groups.push(GroupRow::identity([0.0; 3]));
        }

        let t_scene_start = std::time::Instant::now();
        let binding_limit = ctx.device.limits().max_storage_buffer_binding_size as usize;
        let instances_len = arena.len();
        
        let max_groups = 65536;
        let group_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("group table"),
            size: (max_groups * std::mem::size_of::<GroupRow>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        ctx.queue.write_buffer(&group_buf, 0, bytemuck::cast_slice(&groups));
        log::info!(
            "emoji sheet bound: {} cells, {} mip levels, {:.1} MiB",
            atlas.emoji.sheet.cells.len(),
            atlas.emoji.mip_levels,
            atlas.emoji.texture_bytes as f64 / (1 << 20) as f64,
        );
        let camera_buf = device.create_buffer(&wgpu::BufferDescriptor {
            // Stage L (L1): the widened FrameUniform buffer (104 B) — binds to
            // the unchanged 64 B WGSL block via the minimum-binding-size rule.
            label: Some("frame uniform"),
            size: std::mem::size_of::<FrameUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let emoji_view = atlas.emoji.texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("emoji sheet view"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let sheet = &atlas.emoji.sheet;
        let params = Params {
            max_groups: max_groups as u32,
            greek_mode: 2,
            _pad1: 0,
            _pad2: 0,
            // GLYPH_LOD_DEFAULTS (GlyphField.js)
            dilate_px: 0.75,
            soften: 0.45,
            min_lo: 0.06,
            min_hi: 0.20,
            emoji_cell: [sheet.cell_w, sheet.cell_h],
            emoji_cols: sheet.cols,
            emoji_rows: sheet.rows_per_layer,
            emoji_layer: [sheet.layer_w as f32, sheet.layer_h as f32],
            greek_onset_px: 10.0,
            _pad3: 0,
        };
        let params_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("glyph params"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let mesh_frame_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("mesh frame bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let mesh_frame_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mesh frame bg"),
            layout: &mesh_frame_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: camera_buf.as_entire_binding(),
                },
            ],
        });
        let mesh_pipeline = std::cell::RefCell::new(mesh::MeshPipeline::new(device, POOL_FORMAT, &mesh_frame_bgl));

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
        let glyph_map_view = atlas.glyphmap.create_view(&Default::default());
        let curves_view = atlas.curves.create_view(&Default::default());
        let resources = FieldResources {
            frame_uniform: &camera_buf,
            group_table: &group_buf,
            glyph_map: &glyph_map_view,
            curves: &curves_view,
            params: &params_buf,
            emoji_sheet: &emoji_view,
            emoji_sampler: &emoji_sampler,
            glyph_advances: &atlas.glyph_advances,
            item_params: &staged.item_params,
        };
        let depth_format = wgpu::TextureFormat::Depth32Float;
        let targets = FieldTargets {
            // Stage L (L3): the glyph pipeline renders into the POOL format.
            color_format: POOL_FORMAT,
            depth_format,
            sample_count: SCENE_SAMPLE_COUNT,
        };
        // The arena's two forms map onto the field's two sources. A DEVICE
        // arena is already slots in the field's OWN format (the producer was
        // told the mode — 32 B RenderSlots for Instanced, 20 B DerivedSlots
        // plus a line table for Derived), chunked by its producer; a HOST
        // arena is the engine's neutral records, which the mode converts and
        // uploads. Unified-memory direct upload is a property of the adapter,
        // decided here (Metal + MAPPABLE_PRIMARY_BUFFERS — see
        // glyph_field_instanced::upload).
        let source = match arena.device_slots() {
            Some(dev) => {
                assert_eq!(
                    dev.format, field_mode,
                    "device arena was emitted for {:?} but the scene builds a {:?} field",
                    dev.format, field_mode
                );
                SlotSource::Device {
                    chunk_capacity: dev.chunk_slots,
                    chunks: &dev.chunks,
                    glyph_count: instances_len,
                    mapped_base: dev
                        .mapped_slots
                        .or_else(|| dev.derived.as_ref().and_then(|d| d.mapped_base)),
                }
            }
            None => SlotSource::Host {
                slices: arena.instance_chunks(),
                glyph_count: instances_len,
                direct_host_upload: ctx.profile.backend == wgpu::Backend::Metal
                    && ctx.profile.mappable_primary_buffers,
            },
        };
        let field: Box<dyn GlyphField> = match field_mode {
            GlyphFieldMode::Instanced => {
                Box::new(InstancedField::new(device, &ctx.queue, source, &resources, targets))
            }
            GlyphFieldMode::Derived => {
                Box::new(DerivedField::new(device, &ctx.queue, source, &resources, targets))
            }
        };
        let field_dur = t_scene_start.elapsed();
        let t_pipe_start = std::time::Instant::now();
        log::info!(
            "glyph field ({}): {} instances ({} MiB) in {} chunk(s) of ≤{} ({} MiB binding limit), {} groups",
            field.mode(),
            instances_len,
            (instances_len * field.slot_bytes() as usize) >> 20,
            field.chunk_count(),
            field.chunk_capacity(),
            binding_limit >> 20,
            groups.len(),
        );
        // Stage L (L4): the selection mask only exists on the shader path —
        // the copy path (offscreen, color_format == POOL_FORMAT) never
        // renders selection visuals.
        let mask_pipeline = (color_format != POOL_FORMAT)
            .then(|| field.create_mask_pipeline(device, MASK_FORMAT, SCENE_SAMPLE_COUNT));

        // Camera fit from staged bounds (or the Stage E2 focus-file override).
        let (center, half_w, half_h) = match staged.focus_bounds {
            Some((c, half)) => (
                Vec3::new(c[0], c[1], 0.0),
                half[0].max(1.0),
                half[1].max(1.0),
            ),
            None => {
                let min = staged.bounds_min;
                let max = staged.bounds_max;
                (
                    Vec3::new((min[0] + max[0]) * 0.5, (min[1] + max[1]) * 0.5, 0.0),
                    ((max[0] - min[0]) * 0.5).max(1.0),
                    ((max[1] - min[1]) * 0.5).max(1.0),
                )
            }
        };
        let fit = {
            // aspect is unknown at build time; the Front/Fly fit distance is
            // dominated by the vertical half-extent, and render() recomputes
            // the exact value per frame — this is for Fly's near/far/speed.
            let half_h_needed = half_h.max(half_w / 1.6);
            half_h_needed / (FOV_Y.to_radians() * 0.5).tan() * 1.08 + 2.0
        };
        let fly = FlyCamera::new(center + Vec3::new(0.0, 0.0, fit), fit);

        // --- Stage F: cull/LOD subsystem --------------------------------------
        let mut segments = staged.segments;
        if segments.is_empty() {
            // Safety net: one cover segment over the whole arena (the text
            // staging paths always provide one; never ship an empty table to
            // the cull pass).
            segments.push(SegCull {
                min: staged.bounds_min,
                max: staged.bounds_max,
                slot_base: 0,
                slot_count: arena.len() as u32,
                tint: seg_tint(
                    arena.instances(),
                    staged.bounds_max[0] - staged.bounds_min[0],
                    staged.bounds_max[1] - staged.bounds_min[1],
                    &atlas.slot_ink,
                ),
                blocks: Vec::new(),
            });
        }
        // multi_draw_indirect is core in wgpu 30 — but its Metal backend
        // silently drops any indirect draw with first_instance != 0 (see the
        // module header), so Stage F culls on the CPU instead; --no-cull
        // keeps the legacy per-chunk full draws for A/B.
        let cull = if cull_enabled {
            Some(CullState::new(
                ctx,
                POOL_FORMAT, // Stage L (L3): the backdrop pipeline renders into the pool
                depth_format,
                &camera_buf,
                &segments,
                &groups,
            ))
        } else {
            log::info!("culling disabled (--no-cull) — legacy per-chunk draws");
            None
        };

        // Stage L (L3): the composite machinery (persistent half). The
        // pooled target itself is sized by set_viewport — GlyphScene::new
        // doesn't know the viewport (the drivers decide it later).
        let composite = pipelines::build_composite_state(device, color_format, mask_pipeline);

        let pick = staged.pick;
        let controller = staged.controller;
        let groups_cpu = groups.clone();
        let tint_step = vec![0u32; groups.len()];
        let pipe_dur = t_pipe_start.elapsed();
        log::info!(
            "scene timings: field build (upload + glyph pipeline) {:.3}s | cull/composite init {:.3}s | total scene {:.3}s",
            field_dur.as_secs_f64(),
            pipe_dur.as_secs_f64(),
            (field_dur + pipe_dur).as_secs_f64(),
        );

        Self {
            field,
            _emoji_view: emoji_view,
            camera_buf,
            depth_format,
            center,
            half_w,
            half_h,
            fit,
            camera_mode,
            fly,
            cull,
            group_buf,
            groups_cpu,
            pick,
            probe_cluster_mode: None,
            probe_layout_mode: None,
            picked: None,
            selection: None,
            geom_overrides: std::collections::HashMap::new(),
            cache: None,
            grabbed_group: None,
            controller,
            grabbed_zone: None,
            cursor: (0.0, 0.0),
            viewport: Cell::new((1600, 1000)), // refreshed every render()
            tint_step,
            ui_probe: None,
            device: device.clone(),
            composite,
            params_buf,
            params: Cell::new(params),
            mesh_pipeline,
            mesh_frame_bg,
        }
    }

    /// Configure Greeking mode: 0 = disabled, 1 = smooth fade, 2 = pure hard bypass.
    pub fn set_greek_mode(&self, queue: &wgpu::Queue, mode: u32) {
        let mut p = self.params.get();
        if p.greek_mode != mode {
            p.greek_mode = mode;
            self.params.set(p);
            queue.write_buffer(&self.params_buf, 0, bytemuck::bytes_of(&p));
        }
    }

    /// Configure whether Greeking (anti-Moiré subpixel bars) is enabled.
    pub fn set_greeking(&self, queue: &wgpu::Queue, on: bool) {
        let cur = self.params.get().greek_mode;
        let mode = if on {
            if cur == 1 { 1 } else { 2 }
        } else {
            0
        };
        self.set_greek_mode(queue, mode);
    }

    /// Configure the on-screen glyph height in px/em where Greeking begins (default: 10.0).
    pub fn set_greek_onset_px(&self, queue: &wgpu::Queue, onset: f32) {
        let mut p = self.params.get();
        if (p.greek_onset_px - onset).abs() > 1e-3 {
            p.greek_onset_px = onset;
            self.params.set(p);
            queue.write_buffer(&self.params_buf, 0, bytemuck::bytes_of(&p));
        }
    }

    /// Configure whether file background bounding quads are emitted behind glyphs when near.
    pub fn set_file_backgrounds(&mut self, on: bool) {
        if let Some(cull) = &self.cull {
            cull.file_backgrounds.set(on);
        }
    }

    /// Set the RGBA color for near file background bounding quads.
    pub fn set_file_bg_color(&mut self, rgba: [f32; 4]) {
        if let Some(cull) = &self.cull {
            cull.file_bg_color.set(rgba);
        }
    }

    /// Set the LOD minimum pixel threshold.
    pub fn set_lod_min_px(&mut self, lod: f32) {
        if let Some(cull) = &self.cull {
            cull.lod_min_px.set(lod);
        }
    }
}
