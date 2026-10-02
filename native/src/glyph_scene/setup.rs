use std::cell::Cell;
use glam::Vec3;
use wgpu::util::DeviceExt;

use crate::atlas::Atlas;
use crate::gpu::GpuContext;
use crate::text::StagedText;
use super::{
    CameraMode, CullState, FlyCamera, GlyphScene,
    cull::SegCull,
    instance::{FrameUniform, Params, RenderSlot, GlyphInstance, GroupRow},
    target::POOL_FORMAT,
    camera::FOV_Y,
    tint::seg_tint,
    buffers, pipelines, mesh
};

impl GlyphScene {
    pub fn new(
        ctx: &GpuContext,
        color_format: wgpu::TextureFormat,
        atlas: &Atlas,
        staged: StagedText,
        camera_mode: CameraMode,
        cull_enabled: bool,
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

        // Chunk the arena so no bound RANGE exceeds the binding limit.
        // (the cull/pick slot math keys on chunk_cap, so the renderer's
        // chunking and the arena's must agree).
        //
        // E2a (note 23): the shader binds the 32 B RenderSlot, so HOST and
        // MAPPED (48 B) arenas transcode at staging — the values the vertex
        // math reads are unchanged, so the goldens stay byte-equal.
        // E2b: a DEVICE arena (the endpoint) binds the chain's slot buffers
        // AS-IS — no upload, no transcode, no copy; each chunk's pool-slice
        // offset rides its binding.
        let t_scene_start = std::time::Instant::now();
        let binding_limit = ctx.device.limits().max_storage_buffer_binding_size as usize;
        let instances_len = arena.len();
        let mapped_slots = arena.device_slots().and_then(|d| d.mapped_slots);
        let (chunk_cap, chunk_counts, instance_bufs, chunk_offsets) =
            buffers::build_instance_buffers(ctx, &arena, instances_len);
        let upload_dur = t_scene_start.elapsed();
        let t_pipe_start = std::time::Instant::now();
        let group_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("group table"),
            contents: bytemuck::cast_slice(&groups),
            // Stage G: COPY_DST for partial per-row edit uploads (80 B/row).
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        });
        log::info!(
            "emoji sheet bound: {} cells, {} mip levels, {:.1} MiB",
            atlas.emoji.sheet.cells.len(),
            atlas.emoji.mip_levels,
            atlas.emoji.texture_bytes as f64 / (1 << 20) as f64,
        );
        log::info!(
            "glyph field: {} instances ({} MiB) in {} chunk(s) of ≤{} ({} MiB binding limit), {} groups",
            instances_len,
            (instances_len * std::mem::size_of::<RenderSlot>()) >> 20,
            chunk_counts.len(),
            chunk_cap,
            binding_limit >> 20,
            groups.len(),
        );

        let camera_buf = device.create_buffer(&wgpu::BufferDescriptor {
            // Stage L (L1): the widened FrameUniform buffer (104 B) — binds to
            // the unchanged 64 B WGSL block via the minimum-binding-size rule.
            label: Some("frame uniform"),
            size: std::mem::size_of::<FrameUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let quad_index_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("quad index buffer"),
            contents: bytemuck::cast_slice(&[0u16, 1, 2, 0, 2, 3]),
            usage: wgpu::BufferUsages::INDEX,
        });
        let emoji_view = atlas.emoji.texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("emoji sheet view"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let sheet = &atlas.emoji.sheet;
        let params = Params {
            max_groups: groups.len() as u32,
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

        let bgl = pipelines::build_glyph_bgl(device);
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
        // Stage L (O2): enumerate so captures can tell chunk bind groups
        // apart (mirrors the "glyph instances i/N" buffer labels).
        // The chunk bindings: the chain's slot buffers on the endpoint
        // (Device) path — each with its pool-slice offset — the staged
        // uploads otherwise. Each chunk's binding starts at its own index
        // 0, ≤ the binding limit by construction.
        let chunk_bindings: Vec<wgpu::BufferBinding> = instance_bufs
            .iter()
            .zip(chunk_counts.iter())
            .zip(chunk_offsets.iter())
            .map(|((b, &count), &off)| wgpu::BufferBinding {
                buffer: b,
                // The endpoint's pool slices start mid-buffer; staged
                // uploads are offset 0. The binding's size is the chunk's
                // live slots (never the pool page's padding).
                offset: off,
                size: std::num::NonZeroU64::new(count as u64 * 32),
            })
            .collect();
        let bind_group_count = chunk_bindings.len();
        let bind_groups: Vec<wgpu::BindGroup> = chunk_bindings
            .iter()
            .enumerate()
            .map(|(i, chunk_binding)| {
                let label = if bind_group_count == 1 {
                    "glyph bg".to_string()
                } else {
                    format!("glyph bg {i}/{bind_group_count}")
                };
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some(&label),
                    layout: &bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: camera_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Buffer(chunk_binding.clone()),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: group_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: wgpu::BindingResource::TextureView(
                                &atlas.glyphmap.create_view(&Default::default()),
                            ),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: wgpu::BindingResource::TextureView(
                                &atlas.curves.create_view(&Default::default()),
                            ),
                        },
                        wgpu::BindGroupEntry {
                            binding: 5,
                            resource: params_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 6,
                            resource: wgpu::BindingResource::TextureView(&emoji_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 7,
                            resource: wgpu::BindingResource::Sampler(&emoji_sampler),
                        },
                    ],
                })
            })
            .collect();

        let depth_format = wgpu::TextureFormat::Depth32Float;
        let (shader, layout, pipeline) =
            pipelines::build_glyph_pipeline(device, &bgl, depth_format);
        let mask_pipeline =
            pipelines::build_mask_pipeline(device, &shader, &layout, color_format);

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
            "scene timings: buffer upload {:.3}s | pipeline/cull init {:.3}s | total scene {:.3}s",
            upload_dur.as_secs_f64(),
            pipe_dur.as_secs_f64(),
            (upload_dur + pipe_dur).as_secs_f64(),
        );

        Self {
            pipeline,
            _emoji_view: emoji_view,
            bind_groups,
            chunk_counts,
            chunk_cap: chunk_cap as u32,
            camera_buf,
            quad_index_buf,
            depth_format,
            instance_count: instances_len as u32,
            center,
            half_w,
            half_h,
            fit,
            camera_mode,
            fly,
            cull,
            instance_bufs,
            chunk_offsets,
            mapped_slots,
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
