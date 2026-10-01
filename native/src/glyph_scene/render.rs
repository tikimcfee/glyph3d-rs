//! Stage F/H/L — Render pass orchestration (cull, backdrops, glyphs, selection, composite).
//! Extracted from `glyph_scene.rs` to modularize frame rendering.

use crate::gpu::GpuContext;
use crate::glyph_scene::camera::FOV_Y;
use crate::glyph_scene::cull::{cull_segments, frustum_planes, CullView, Phase, PhaseDraws};
use crate::glyph_scene::instance::FrameUniform;
use crate::glyph_scene::pick::format_pick;
use crate::glyph_scene::target::{Selection, POOL_FORMAT};
use crate::glyph_scene::ui_probe::UiFileDyn;
use crate::glyph_scene::GlyphScene;

pub(super) fn render_scene(
    scene: &GlyphScene,
    ctx: &GpuContext,
    encoder: &mut wgpu::CommandEncoder,
    target: &crate::scene::FrameTarget<'_>,
    t: f32,
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
    let aspect = width as f32 / height.max(1) as f32;
    scene.viewport.set((width, height));
    let frame = scene.camera_frame(t, aspect);

    // Stage L (L3): the phase lists draw into the pooled view target
    // (ping-pong slot selected here) and composite into the driver's
    // view at the end of render.
    let comp = &scene.composite;
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

    // Stage K (K4): apply live UI controls BEFORE culling so a slider
    // drag takes effect this frame. This is the SINGLE write site of
    // CullState::lod_min_px, and it runs only when a windowed probe is
    // installed — offscreen never installs one, so offscreen culls with
    // the LOD_MIN_PX const by construction (gate 6's byte-equal PNGs are
    // the proof).
    if let (Some(probe), Some(cull)) = (&scene.ui_probe, &scene.cull) {
        cull.lod_min_px.set(probe.borrow().lod_min_px);
    }

    // Stage L (L1): fill every lane of the widened frame uniform from
    // values already computed here. px_scale is computed once and shared
    // with the cull block below (it was CullView-local before L1 — the
    // uniform needs it even under --no-cull). flags bit 0
    // (deterministic_rendering) stays 0 — reserved.
    let px_scale = height as f32 / (2.0 * (FOV_Y.to_radians() * 0.5).tan());
    let cam = FrameUniform {
        view_proj: frame.view_proj.to_cols_array(),
        eye: frame.eye.to_array(),
        _pad0: 0.0,
        viewport: [width as f32, height as f32],
        px_scale,
        time: t,
        flags: 0,
        _pad1: 0,
    };
    ctx.queue
        .write_buffer(&scene.camera_buf, 0, bytemuck::bytes_of(&cam));

    // --- Stage F: CPU segment cull (frustum + LOD) ----------------------
    // Stage L (L2): the cull output IS the phase lists (PhaseDraws). The
    // legacy --no-cull path builds the Glyphs list straight from the
    // chunk counts (one full range per chunk — identical draws to the
    // pre-L2 per-chunk loop) and has no Backdrop phase content.
    let phase_draws: PhaseDraws = if let Some(cull) = &scene.cull {
        // Stage H: CPU scope timing (only when GLYPH_PROFILE=1 built a profiler).
        let cull_t0 = ctx.profiler.as_ref().map(|_| std::time::Instant::now());
        let view = CullView {
            planes: frustum_planes(&frame.view_proj),
            eye: frame.eye,
            px_scale,
            lod_min_px: cull.lod_min_px.get(),
        };
        let phase_draws = cull_segments(
            &cull.segments,
            &cull.hidden,
            &view,
            scene.chunk_cap,
            scene.bind_groups.len() as u32,
        );
        if !phase_draws.backdrops.is_empty() {
            ctx.queue.write_buffer(
                &cull.backdrop_insts_buf,
                0,
                bytemuck::cast_slice(&phase_draws.backdrops),
            );
        }
        if let Some(t0) = cull_t0 {
            crate::gpu::record_cpu_scope(ctx, "cull (CPU)", t0.elapsed().as_secs_f64() * 1000.0);
        }
        if std::env::var_os("GLYPH_CULL_DEBUG").is_some() && t == 0.0 {
            let insts: u64 = phase_draws
                .glyph_ranges
                .iter()
                .map(|(_, r)| (r.end - r.start) as u64)
                .sum();
            println!(
                "CULLDBG glyph draws={} instances={} | backdrops={}",
                phase_draws.glyph_ranges.len(),
                insts,
                phase_draws.backdrops.len(),
            );
        }
        phase_draws
    } else {
        PhaseDraws {
            backdrops: Vec::new(),
            glyph_ranges: scene
                .chunk_counts
                .iter()
                .enumerate()
                .map(|(c, &n)| (c as u32, 0..n))
                .collect(),
        }
    };

    // Stage K: refresh the windowed debug-UI probe (installed only by
    // windowed runs; offscreen skips this entirely). Camera fields are
    // this frame's ACTUAL products; the pick line is the same string
    // format_pick produces for the stdout log; the K4 cull counters are
    // the same sums GLYPH_CULL_DEBUG prints (zeros under --no-cull).
    if let Some(probe) = &scene.ui_probe {
        let mut p = probe.borrow_mut();
        p.camera_mode = Some(scene.camera_mode);
        p.eye = frame.eye.to_array();
        p.yaw = scene.fly.yaw;
        p.pitch = scene.fly.pitch;
        p.last_pick = scene.picked.as_ref().map(format_pick);
        if scene.cull.is_some() {
            p.cull_ranges = phase_draws.glyph_ranges.len();
            p.cull_instances = phase_draws
                .glyph_ranges
                .iter()
                .map(|(_, r)| (r.end - r.start) as u64)
                .sum();
            p.cull_backdrops = phase_draws.backdrops.len();
        } else {
            p.cull_ranges = 0;
            p.cull_instances = 0;
            p.cull_backdrops = 0;
        }
        // The layout dial's readout: the field's depth extent over the
        // segment table (per frame, so group z-moves show too). None
        // under --no-cull — no segment table to measure.
        p.z_extent = scene.cull.as_ref().map(|c| {
            c.segments.iter().fold([f32::INFINITY, f32::NEG_INFINITY], |[lo, hi], s| {
                [lo.min(s.min[2]), hi.max(s.max[2])]
            })
        });
        // K5: refresh the browser's dynamic row state (world pose under
        // the live group TRS, hidden, tint). ~1.3k cheap iterations at
        // repo scale; skipped entirely offscreen (no probe installed).
        if !p.files.is_empty() {
            let files = p.files.clone();
            p.file_dyn = files
                .iter()
                .map(|r| {
                    let Some(g) = scene.groups_cpu.get(r.group_id as usize) else {
                        return UiFileDyn::default();
                    };
                    let (ox, oy) = (g.cols[0][0], g.cols[0][1]);
                    let (sx, sy) = (g.cols[3][0].max(0.0), g.cols[3][1].max(0.0));
                    let wmin = [r.aabb_min[0] * sx + ox, r.aabb_min[1] * sy + oy];
                    let wmax = [r.aabb_max[0] * sx + ox, r.aabb_max[1] * sy + oy];
                    UiFileDyn {
                        center: [(wmin[0] + wmax[0]) * 0.5, (wmin[1] + wmax[1]) * 0.5],
                        half: [(wmax[0] - wmin[0]) * 0.5, (wmax[1] - wmin[1]) * 0.5],
                        hidden: g.cols[2][3] == 0.0,
                        tint: [
                            (g.cols[2][0].clamp(0.0, 1.0) * 255.0) as u8,
                            (g.cols[2][1].clamp(0.0, 1.0) * 255.0) as u8,
                            (g.cols[2][2].clamp(0.0, 1.0) * 255.0) as u8,
                        ],
                    }
                })
                .collect();
        }
    }

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
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("glyph field pass"),
        timestamp_writes: pass_query
            .as_ref()
            .and_then(|q| q.render_pass_timestamp_writes()),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: draw_color_view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color {
                    r: 0.07,
                    g: 0.07,
                    b: 0.09,
                    a: 1.0,
                }),
                store: wgpu::StoreOp::Store,
            },
            depth_slice: None,
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: draw_depth_view,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(1.0),
                store: wgpu::StoreOp::Store,
            }),
            stencil_ops: None,
        }),
        ..Default::default()
    });
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
                    if !phase_draws.backdrops.is_empty() {
                        let q = ctx
                            .profiler
                            .as_ref()
                            .map(|p| p.borrow().begin_query("backdrop stream", &mut pass));
                        pass.set_pipeline(&cull.backdrop_pipeline);
                        pass.set_bind_group(0, &cull.backdrop_bind_group, &[]);
                        pass.draw(0..6, 0..phase_draws.backdrops.len() as u32);
                        if let (Some(p), Some(q)) = (&ctx.profiler, q) {
                            p.borrow().end_query(&mut pass, q);
                        }
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
                pass.set_pipeline(&scene.pipeline);
                let mut cur_chunk = u32::MAX;
                for (c, r) in &phase_draws.glyph_ranges {
                    if *c != cur_chunk {
                        cur_chunk = *c;
                        pass.set_bind_group(0, &scene.bind_groups[*c as usize], &[]);
                    }
                    pass.draw(0..6, r.clone());
                }
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
    // unchanged). Windowed shader path only: on the copy path
    // (offscreen) selection_fx/mask are None and this block is skipped,
    // so offscreen output never carries the tint.
    let selection_phase = scene.selection.is_some().then_some(Phase::Selection);
    // The pool slot the composite will read: the scene slot, or the
    // tinted ping-pong partner when a selection was rendered.
    let mut final_slot = pool_slot;
    if let (Some(Phase::Selection), Some(fx), Some(mask)) =
        (selection_phase, &comp.selection_fx, &vt.mask)
    {
        let sel = scene.selection.as_ref().expect("selection_phase implies selection");
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
            match sel {
                Selection::Glyph { chunk, local } => {
                    pass.set_bind_group(0, &scene.bind_groups[*chunk as usize], &[]);
                    pass.draw(0..6, *local..*local + 1);
                }
                Selection::Segment { slot_base, slot_count } => {
                    // Per-chunk split — the same math cull_segments uses.
                    let slot_end = slot_base + slot_count;
                    for c in 0..scene.bind_groups.len() as u32 {
                        let c_lo = c * scene.chunk_cap;
                        let lo = (*slot_base).max(c_lo);
                        let hi = slot_end.min(c_lo + scene.chunk_cap);
                        if hi > lo {
                            pass.set_bind_group(0, &scene.bind_groups[c as usize], &[]);
                            pass.draw(0..6, (lo - c_lo)..(hi - c_lo));
                        }
                    }
                }
            }
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
