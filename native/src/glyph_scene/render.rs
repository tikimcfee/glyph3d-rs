//! The wgpu half of a frame (2026-10-26): `render_scene` prepares the
//! frame on the CPU (`frame.rs`, `GlyphScene::prepare_frame`) and
//! `encode_frame` uploads and draws it — the group rows, faces and regions
//! it changed, the uniforms, the backdrops, the self-culling field's
//! compute, then the passes: environment, backdrops, faces and regions,
//! scene meshes, glyphs; the selection mask and tint; the composite.

use crate::gpu::GpuContext;
use crate::glyph_scene::cull::Phase;
use crate::glyph_scene::frame::FrameData;
use crate::glyph_scene::target::POOL_FORMAT;
use crate::glyph_scene::GlyphScene;

pub(super) fn render_scene(
    scene: &GlyphScene,
    ctx: &GpuContext,
    encoder: &mut wgpu::CommandEncoder,
    target: &crate::scene::FrameTarget<'_>,
    t: f32,
) {
    let frame = scene.prepare_frame((target.width, target.height), t);
    encode_frame(scene, ctx, encoder, target, &frame);
}

/// Upload and draw a prepared frame into `target`.
pub(super) fn encode_frame(
    scene: &GlyphScene,
    ctx: &GpuContext,
    encoder: &mut wgpu::CommandEncoder,
    target: &crate::scene::FrameTarget<'_>,
    frame: &FrameData<'_>,
) {
    let crate::scene::FrameTarget {
        color_view,
        color_texture,
        color_format,
        // Stage L (L3): unused — the pass renders into the pool's own
        // depth; the driver's depth is only for direct scenes.
        depth_view: _,
        width,
        height,
    } = *target;
    // The tables first: the group rows the resolve changed go up before any
    // pass reads a row (the glyph pass);
    // the page faces and regions with them. Nothing moved, nothing written.
    scene.nodes.upload(&ctx.queue, &scene.gpu.group_buf, &frame.group_runs);
    if let Some(faces) = &scene.library_faces {
        faces.upload(&ctx.queue, &frame.face_runs);
    }
    if let Some(regions) = &scene.library_regions {
        regions.upload(&ctx.queue, &frame.region_runs);
    }
    if ctx.profiler.is_some() {
        let c = &frame.costs;
        if !frame.group_runs.is_empty() {
            crate::gpu::record_cpu_scope(ctx, "group nodes flush (CPU)", scene.nodes.last_flush().0.ms);
        }
        if c.faces.rows > 0 {
            crate::gpu::record_cpu_scope(ctx, "library faces (CPU)", c.faces.ms);
        }
        if c.regions.composed > 0 {
            crate::gpu::record_cpu_scope(ctx, "library regions (CPU)", c.regions.ms);
        }
        if scene.cull.is_some() {
            crate::gpu::record_cpu_scope(ctx, "cull (CPU)", f64::from(c.cull_ms));
        }
    }
    // GLYPH_LIBRARY_TIMING: one line per frame the regions did anything (a
    // settled library prints nothing): refits taken, instances composed,
    // instances and bytes uploaded, and the CPU time of it all.
    let rc = &frame.costs.regions;
    if (rc.refits > 0 || rc.composed > 0) && std::env::var_os("GLYPH_LIBRARY_TIMING").is_some() {
        println!("REGIONTIME refits={} composed={} uploaded={} upload_bytes={} prepare_ms={:.4}", rc.refits, rc.composed, rc.uploaded, rc.bytes, rc.ms);
    }

    // Stage L (L3): the phase lists draw into the pooled view target
    // (ping-pong slot selected here) and composite into the driver's
    // view at the end of render.
    let comp = &scene.gpu.composite;
    let vt = comp
        .target
        .as_ref()
        .expect("L3: set_viewport must run before render (both drivers call it)");
    assert_eq!(
        (vt.width, vt.height),
        (width, height),
        "L3: pool/viewport size mismatch (set_viewport out of sync with the driver)"
    );
    let pool_slot = (comp.parity.get() % 2) as usize;
    comp.parity.set(comp.parity.get() + 1);

    ctx.queue.write_buffer(&scene.gpu.camera_buf, 0, bytemuck::bytes_of(&frame.uniform));
    if scene.gpu.params_uploaded.get() != frame.params_generation {
        ctx.queue.write_buffer(&scene.gpu.params_buf, 0, bytemuck::bytes_of(&frame.params));
        scene.gpu.params_uploaded.set(frame.params_generation);
    }
    if let Some(env) = &frame.env {
        scene.gpu.environment.write(&ctx.queue, env);
    }
    // The CPU cull's backdrop survivors, up to the buffer's capacity.
    if let Some(cull) = &scene.cull {
        if !frame.backdrops.is_empty() {
            let max_cap = (cull.backdrop_insts_buf.size() / std::mem::size_of::<crate::glyph_scene::cull::BackdropInst>() as u64) as usize;
            let n = frame.backdrops.len().min(max_cap);
            ctx.queue.write_buffer(&cull.backdrop_insts_buf, 0, bytemuck::cast_slice(&frame.backdrops[..n]));
        }
    }

    let scene_meshes = scene.controller.as_ref().map(|ctrl| ctrl.scene.mesh_draws());
    if let Some(meshes) = scene_meshes {
        scene.gpu.mesh_pipeline.borrow_mut().prepare(&ctx.device, &ctx.queue, meshes);
    }
    let glyph_ranges = frame.glyph_ranges.pairs();

    // Stage H: pass-level GPU timer (TIMESTAMP_QUERY; pass-boundary writes,
    // so it works on Metal). Nested in-pass scopes below additionally need
    // TIMESTAMP_QUERY_INSIDE_PASSES — where unsupported they simply report
    // no time. Queries must always be closed, timing or not.
    let pass_query = ctx
        .profiler
        .as_ref()
        .map(|p| p.borrow().begin_pass_query("glyph field pass", encoder));
    // Stage L (L3): the pass renders into the pooled target.
    let draw_depth_view = &vt.depth;
    let draw_color_view = &vt.color_views[pool_slot];
    
    let mp_ref = scene.gpu.mesh_pipeline.borrow();

    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("glyph field pass"),
        timestamp_writes: pass_query
            .as_ref()
            .and_then(|q| q.render_pass_timestamp_writes()),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: draw_color_view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(crate::config::wgpu_color(frame.clear_color)),
                store: wgpu::StoreOp::Store,
            },
            depth_slice: None,
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: draw_depth_view,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(0.0),
                store: wgpu::StoreOp::Store,
            }),
            stencil_ops: None,
        }),
        ..Default::default()
    });
    // The environment (ground/sky) goes down first, opaque, writing depth;
    // everything after sorts against it. Off: nothing recorded.
    if frame.env.is_some() {
        scene.gpu.environment.draw(&mut pass);
    }
    // Stage L (L2): record the phase lists in phase order — Backdrop
    // first, Glyphs second, exactly the Stage F order. Empty phases
    // record nothing (the legacy --no-cull branch has no Backdrop
    // content; an all-near-LOD frame has none either). Profiler query
    // names unchanged ("backdrop stream", "glyph stream").
    for phase in [Phase::Backdrop, Phase::Glyphs] {
        match phase {
            Phase::Selection => unreachable!(
                "L4: the Selection phase renders into its own mask target, \
                 never inside the glyph field pass"
            ),
            Phase::Backdrop => {
                // Far LOD stream first: one instanced draw over the
                // compacted backdrop quads (plain draw — no indirect
                // machinery, see the module header). The pipeline lives
                // in CullState; the legacy branch never reaches in here
                // (its backdrops list is empty).
                if let Some(cull) = &scene.cull {
                    if !frame.backdrops.is_empty() {
                        let q = ctx
                            .profiler
                            .as_ref()
                            .map(|p| p.borrow().begin_query("backdrop stream", &mut pass));
                        pass.set_pipeline(&cull.backdrop_pipeline);
                        pass.set_bind_group(0, &cull.backdrop_bind_group, &[]);
                        pass.draw(0..6, 0..frame.backdrops.len() as u32);
                        if let (Some(p), Some(q)) = (&ctx.profiler, q) {
                            p.borrow().end_query(&mut pass, q);
                        }
                    }
                }

                // The library's faces take the scene meshes' place (a
                // library load has no other scene mesh): same pipeline,
                // same position in the phase.
                if let Some(faces) = &scene.library_faces {
                    mp_ref.render_from(&mut pass, crate::glyph_scene::mesh::UnitMesh::Quad, 0..faces.count(), &scene.gpu.mesh_frame_bg, &faces.instances);
                }
                // The regions after the faces (a page hides the walls
                // behind it), before the glyphs (which read over them).
                if let Some(regions) = &scene.library_regions {
                    regions.draw(&mut pass);
                }
                if let Some(meshes) = &scene_meshes {
                    let q_len = meshes.quads.len() as u32;
                    let c_len = meshes.cubes.len() as u32;
                    if q_len > 0 {
                        mp_ref.render(&mut pass, crate::glyph_scene::mesh::UnitMesh::Quad, 0..q_len, &scene.gpu.mesh_frame_bg);
                    }
                    if c_len > 0 {
                        mp_ref.render(&mut pass, crate::glyph_scene::mesh::UnitMesh::Cube, q_len..(q_len + c_len), &scene.gpu.mesh_frame_bg);
                    }
                }
            }
            Phase::Glyphs => {
                // Glyph stream: one range draw per entry. The list is
                // chunk-major with arena-ascending ranges within a chunk,
                // so within-pixel blend order matches the pre-L2 loops
                // exactly; the chunk bind group is re-set only on change
                // (legacy: one full range per chunk, so every chunk sets
                // its bind group exactly once, as before).
                let q = ctx
                    .profiler
                    .as_ref()
                    .map(|p| p.borrow().begin_query("glyph stream", &mut pass));
                pass.set_pipeline(scene.field.glyph_pipeline());
                scene.field.record_draws(&mut pass, &glyph_ranges);
                if let (Some(p), Some(q)) = (&ctx.profiler, q) {
                    p.borrow().end_query(&mut pass, q);
                }
            }
        }
    }
    drop(pass);
    if let (Some(p), Some(q)) = (&ctx.profiler, pass_query) {
        p.borrow().end_query(encoder, q);
    }

    // Stage L (L4): the Selection phase — own mask target ⇒ own pass,
    // rendered after Glyphs. The variant is constructed here, when a
    // selection exists (no selection → the L3 command stream is
    // unchanged, which is every golden frame). Both drivers have the mask
    // machinery (`CompositeState::selection_fx`), so an offscreen frame
    // with a pick carries the tint.
    let selection_phase = frame.selection.is_some().then_some(Phase::Selection);
    // The pool slot the composite will read: the scene slot, or the
    // tinted ping-pong partner when a selection was rendered.
    let mut final_slot = pool_slot;
    if let (Some(Phase::Selection), Some(fx), Some(mask)) =
        (selection_phase, &comp.selection_fx, &vt.mask)
    {
        let sel = frame.selection.as_ref().expect("selection_phase implies selection");
        // Mask pass: the selected glyph quads into the mask target
        // (glyph coverage in alpha). Blend disabled; no depth.
        let mask_query = ctx
            .profiler
            .as_ref()
            .map(|p| p.borrow().begin_pass_query("selection mask pass", encoder));
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("selection mask pass"),
                timestamp_writes: mask_query
                    .as_ref()
                    .and_then(|q| q.render_pass_timestamp_writes()),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &mask.view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // TRANSPARENT, not BLACK: wgpu::Color::BLACK is
                        // (0,0,0,1) — clearing to alpha=1 blanketed the
                        // whole mask (uniform tint over the frame; caught
                        // by the K6 eyeball + a mask-dump probe).
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
            pass.set_pipeline(&fx.mask_pipeline);
            scene.field.record_draws(&mut pass, &sel.ranges.pairs());
        }
        if let (Some(p), Some(q)) = (&ctx.profiler, mask_query) {
            p.borrow().end_query(encoder, q);
        }
        // Tint pass: pool[pool_slot] + mask → pool[1 - pool_slot]
        // (additive coverage-weighted tint); the composite reads the
        // tinted slot below.
        final_slot = 1 - pool_slot;
        let tint_query = ctx
            .profiler
            .as_ref()
            .map(|p| p.borrow().begin_pass_query("selection tint pass", encoder));
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("selection tint pass"),
                timestamp_writes: tint_query
                    .as_ref()
                    .and_then(|q| q.render_pass_timestamp_writes()),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &vt.color_views[final_slot],
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
            pass.set_pipeline(&fx.tint_pipeline);
            pass.set_bind_group(0, &mask.tint_bgs[pool_slot], &[]);
            pass.draw(0..3, 0..1);
        }
        if let (Some(p), Some(q)) = (&ctx.profiler, tint_query) {
            p.borrow().end_query(encoder, q);
        }
    }

    // Stage L (L3): composite the pooled target into the driver's view.
    if color_format == POOL_FORMAT {
        // Offscreen/oracle path: same format, 1:1, no scaling —
        // copy_texture_to_texture is bit-exact BY CONSTRUCTION (this
        // is the gate-critical path; it cannot fail a byte compare).
        assert_eq!(
            color_format, POOL_FORMAT,
            "L3: copy composite requires matching formats (deliberately loud)"
        );
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &vt.colors[final_slot],
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: color_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
    } else {
        // Windowed path: the surface (Bgra8UnormSrgb) is
        // component-order-incompatible with the pool, so a copy is
        // invalid — fullscreen shader composite instead. The pass
        // clears-then-overwrites every pixel (blend disabled).
        let composite_query = ctx
            .profiler
            .as_ref()
            .map(|p| p.borrow().begin_pass_query("composite pass", encoder));
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite pass"),
                timestamp_writes: composite_query
                    .as_ref()
                    .and_then(|q| q.render_pass_timestamp_writes()),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: color_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
            pass.set_pipeline(&comp.pipeline);
            pass.set_bind_group(0, &vt.bind_groups[final_slot], &[]);
            pass.draw(0..3, 0..1);
        }
        if let (Some(p), Some(q)) = (&ctx.profiler, composite_query) {
            p.borrow().end_query(encoder, q);
        }
    }
}
