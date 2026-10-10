//! Stage F/H/L — Render pass orchestration (cull, backdrops, glyphs, selection, composite).
//! Extracted from `glyph_scene.rs` to modularize frame rendering.

use crate::gpu::GpuContext;
use crate::glyph_scene::camera::fov_y_deg;
use crate::glyph_scene::environment::EnvCamera;
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
    // the `[lod] show_glyphs_px` setting by construction (gate 6's byte-equal
    // PNGs are the proof).
    if let (Some(probe), Some(cull)) = (&scene.ui_probe, &scene.cull) {
        let p = probe.borrow();
        cull.lod_min_px.set(p.lod_min_px);
        cull.lod_backdrop_px.set(p.lod_backdrop_px);
        cull.file_backgrounds.set(p.file_backgrounds);
        cull.file_bg_color.set(p.file_bg_color);
    }
    if let Some(probe) = &scene.ui_probe {
        let p = probe.borrow();
        let mode = if !p.greeking {
            0
        } else if p.greek_pure {
            2
        } else {
            1
        };
        scene.set_greek_mode(&ctx.queue, mode);
        scene.set_greek_onset_px(&ctx.queue, p.greek_onset_px);
        scene.set_debug_tint(&ctx.queue, p.debug_tint);
    }
    // A field that culls and draws itself (Visible): the scene's per-file
    // glyph ranges are not built for it, its slot math is never asked, and
    // the CPU cull keeps only two jobs — the BACKDROP quads (under the
    // field's backdrop threshold, not the glyph one, so a washed item is not
    // also a backdrop) and the hidden flags.
    let self_drawing = scene.field.draws_itself();

    // Stage L (L1): fill every lane of the widened frame uniform from
    // values already computed here. px_scale is computed once and shared
    // with the cull block below (it was CullView-local before L1 — the
    // uniform needs it even under --no-cull). flags bit 0
    // (deterministic_rendering) stays 0 — reserved.
    let px_scale = height as f32 / (2.0 * (fov_y_deg().to_radians() * 0.5).tan());
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
    // The self-culling field's per-frame work (cull, LOD sort, layout of the
    // glyph-tier lines into its transient slots) goes into THIS encoder
    // before the glyph pass. The thresholds are the cull cells (live from the
    // panel in windowed runs, the `[lod]` settings offscreen): the glyph tier
    // is the existing `show_glyphs_px`, the backdrop tier the visible mode's own.
    if self_drawing {
        let p = scene.params.get();
        let (lod_glyph_px, lod_backdrop_px) = scene
            .cull
            .as_ref()
            .map(|c| (c.lod_min_px.get(), c.lod_backdrop_px.get()))
            .unwrap_or_else(|| {
                let s = &crate::config::settings().lod;
                (s.show_glyphs_px, s.visible_backdrop_px)
            });
        let prepare = glyph_field::FramePrepare {
            view_proj: frame.view_proj.to_cols_array_2d(),
            eye: frame.eye.to_array(),
            viewport: [width as f32, height as f32],
            px_scale,
            lod_glyph_px,
            lod_backdrop_px,
            greek_mode: p.greek_mode,
            debug_tint: p.debug_tint,
            time: t,
        };
        // C29 (2026-10-10): under GLYPH_CULL_DEBUG, the field's own cull
        // counters for frame 0 — the F8 HUD's figures, offscreen — read back
        // BLOCKING at the start of frame 1, before this frame's `prepare`
        // zeroes them: frame 0 has been submitted by then, where a readback
        // inside frame 0 would see zeros its own encoder had not run past.
        // Debug only (a stall per run, never the frame path).
        if std::env::var_os("GLYPH_CULL_DEBUG").is_some() && (t - 1.0 / 60.0).abs() < 1e-4 {
            if let Some(v) = scene.field.visible() {
                let c = v.read_counters(&ctx.queue);
                use glyph_field_visible::tables::counter as k;
                println!(
                    "CULLDBG visible (frame 0): items {} visible, {} backdrop, {} hidden, {} culled of {} | lines {} candidate: {} glyph, {} wash, {} dropped | segments {} | slots {} ({} dropped) | wash boxes {}",
                    c[k::ITEMS_VISIBLE],
                    c[k::ITEMS_BACKDROP],
                    c[k::ITEMS_HIDDEN],
                    c[k::ITEMS_CULLED],
                    v.stats().items_total,
                    c[k::LINES_CANDIDATE],
                    c[k::LINES_GLYPH],
                    c[k::LINES_WASH],
                    c[k::LINES_DROPPED],
                    c[k::SEG_FIT_END],
                    c[k::SLOT_FIT_END],
                    c[k::SLOTS_DROPPED],
                    c[k::WASH],
                );
            }
        }
        scene.field.prepare(&ctx.queue, encoder, &prepare);
        // The selection mask's content, right after `prepare` and before any
        // pass (same encoder): the field lays the selected byte range's
        // glyphs out again into its selection buffer, which the mask pass
        // below draws. Gated exactly as that pass is (the mask machinery is
        // built for both drivers since the mask pipeline became
        // unconditional — an offscreen frame with a pick carries the
        // highlight too, which is what lets `tests/visible_verbs.rs` witness
        // this pass by pixel).
        if let (Some(visible), Some(Selection::ByteRange { item, start, end }), true, true) = (
            scene.field.visible(),
            scene.selection.as_ref(),
            comp.selection_fx.is_some(),
            vt.mask.is_some(),
        ) {
            visible.prepare_mask(&ctx.queue, encoder, *item, *start, *end);
        }
    }
    if scene.environment.is_on() {
        let env_cam = EnvCamera {
            eye: frame.eye.as_dvec3(),
            vp_rel: frame.view_proj_rel,
            far: frame.far,
            fit: scene.fit,
        };
        let ground_y = scene.environment.ground_y(scene.scene_min_y);
        scene.environment.write(&ctx.queue, &env_cam, ground_y);
    }

    // --- Stage F: CPU segment cull (frustum + LOD) ----------------------
    // Stage L (L2): the cull output IS the phase lists (PhaseDraws). The
    // legacy --no-cull path builds the Glyphs list straight from the
    // chunk counts (one full range per chunk — identical draws to the
    // pre-L2 per-chunk loop) and has no Backdrop phase content.
    let mut cull_cpu_ms = 0.0f32;
    let phase_draws: PhaseDraws = if let Some(cull) = &scene.cull {
        // Stage H: CPU scope timing (only when GLYPH_PROFILE=1 built a
        // profiler); the HUD's cull figure reads the same clock.
        let cull_t0 = (ctx.profiler.is_some() || scene.ui_probe.is_some()).then(std::time::Instant::now);
        let view = CullView {
            planes: frustum_planes(&frame.view_proj),
            eye: frame.eye,
            px_scale,
            lod_min_px: if self_drawing { cull.lod_backdrop_px.get() } else { cull.lod_min_px.get() },
            file_backgrounds: cull.file_backgrounds.get(),
            file_bg_color: cull.file_bg_color.get(),
        };
        // No chunks for a self-drawing field: zero chunks means the cull
        // builds no glyph range (and never asks the field for its slot math).
        let (chunk_cap, chunk_count) = if self_drawing {
            (1, 0)
        } else {
            (scene.field.chunk_capacity(), scene.field.chunk_count())
        };
        let phase_draws = cull_segments(&cull.segments, &cull.hidden, &view, chunk_cap, chunk_count);
        if !phase_draws.backdrops.is_empty() {
            let max_cap = (cull.backdrop_insts_buf.size()
                / std::mem::size_of::<crate::glyph_scene::cull::BackdropInst>() as u64)
                as usize;
            let slice_to_write = if phase_draws.backdrops.len() > max_cap {
                &phase_draws.backdrops[..max_cap]
            } else {
                &phase_draws.backdrops[..]
            };
            ctx.queue.write_buffer(
                &cull.backdrop_insts_buf,
                0,
                bytemuck::cast_slice(slice_to_write),
            );
        }
        if let Some(t0) = cull_t0 {
            cull_cpu_ms = t0.elapsed().as_secs_f32() * 1000.0;
            if ctx.profiler.is_some() {
                crate::gpu::record_cpu_scope(ctx, "cull (CPU)", f64::from(cull_cpu_ms));
            }
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
    } else if self_drawing {
        // --no-cull on a self-drawing field: it still culls itself; the
        // scene simply has no backdrops to offer.
        PhaseDraws { backdrops: Vec::new(), glyph_ranges: Vec::new() }
    } else {
        PhaseDraws {
            backdrops: Vec::new(),
            glyph_ranges: scene
                .field
                .chunk_glyph_counts()
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
        p.environment = scene.environment.mode.get();
        p.last_pick = scene.picked.as_ref().map(format_pick);
        p.selection = scene.selection.as_ref().map(Selection::describe);
        p.pick_key = scene.picked.as_ref().map(|h| match &h.glyph {
            Some(g) => format!("{} byte {}", h.rel_path, g.byte_off),
            None => format!("{} (file)", h.rel_path),
        });
        p.grabbed_group = scene.grabbed_group;
        p.grabbed_zone = scene.grabbed_zone.clone();
        p.active_zone = scene.picked.as_ref().map(|h| {
            if let Some(ctrl) = &scene.controller {
                let dir = h.rel_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                ctrl.zone_for_file(&h.rel_path, dir)
            } else {
                format!("group:{}", h.group_id)
            }
        });
        p.carrel = scene.controller.as_ref().and_then(|ctrl| {
            let carrel_e = ctrl.active_carrel?;
            let carrel_comp = ctrl.scene.world.get::<crate::spatial_scene::AgentCarrel>(carrel_e)?;
            let session = ctrl.session.as_ref()?;
            let prompt = session.turns.get(carrel_comp.active_turn)
                .and_then(|t| t.prompt.as_deref())
                .unwrap_or("—");
            let touched = if let Some(desk) = ctrl.scene.world.get::<crate::spatial_scene::workdesk::Workdesk>(carrel_comp.workdesk_entity) {
                let mut files: Vec<(String, usize, usize)> = desk.file_stacks.iter().map(|(path, &stack_e)| {
                    let (act, cnt) = if let Some(st) = ctrl.scene.world.get::<crate::spatial_scene::workdesk::FileRevisionStack>(stack_e) {
                        (st.active_revision, st.revision_count)
                    } else {
                        (0, 0)
                    };
                    (path.clone(), act, cnt)
                }).collect();
                files.sort_by(|a, b| a.0.cmp(&b.0));
                files
            } else {
                Vec::new()
            };
            let events = session.linearize_events(ctrl.revision_engine.as_ref());
            let beat_summary = events.get(carrel_comp.active_beat)
                .map(|e| e.summary())
                .unwrap_or_else(|| {
                    session.turns.get(carrel_comp.active_turn).map(|t| t.summary()).unwrap_or_default()
                });

            let total_items = if !events.is_empty() { carrel_comp.beat_count } else { carrel_comp.turn_count };
            let limit = carrel_comp.layout_options.deck_window_limit.max(1);
            let max_deck_scroll = total_items.saturating_sub(limit);
            let k = carrel_comp.layout_options.deck_scroll_offset.min(max_deck_scroll);
            let m = limit.min(total_items.saturating_sub(k));
            let window_item_range = if total_items > 0 && m > 0 {
                let newest = total_items - k;
                let oldest = total_items - k - m + 1;
                (oldest, newest)
            } else {
                (0, 0)
            };

            let max_desk_scroll = ctrl.revision_engine.as_ref().map(|re| {
                re.all_histories().values().map(|h| h.revisions.len().saturating_sub(carrel_comp.layout_options.desk_revision_limit.max(1))).max().unwrap_or(0)
            }).unwrap_or(0);

            Some(crate::glyph_scene::UiCarrelState {
                session_id: carrel_comp.session_id.clone(),
                active_turn: carrel_comp.active_turn,
                turn_count: carrel_comp.turn_count,
                active_beat: carrel_comp.active_beat,
                beat_count: carrel_comp.beat_count,
                prompt_summary: prompt.to_string(),
                beat_summary,
                touched_files: touched,
                layout_options: carrel_comp.layout_options,
                window_item_range,
                max_deck_scroll,
                max_desk_scroll,
            })
        });
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
        // The HUD's readout: what the field is, what the CPU cull cost, and
        // the self-culling field's own counters (its readback lags a frame
        // or two; nothing here waits for it).
        p.field_mode = Some(scene.field.mode());
        p.cull_cpu_ms = cull_cpu_ms;
        p.segments = scene.cull.as_ref().map_or(0, |c| c.segments.len());
        p.hidden_segments = scene.cull.as_ref().map_or(0, |c| c.hidden.iter().filter(|h| **h).count());
        p.visible_stats = scene.field.visible().map(|v| v.stats());
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

    let scene_meshes = scene.controller.as_ref().map(|ctrl| ctrl.scene.mesh_draws());
    if let Some(meshes) = scene_meshes {
        scene.mesh_pipeline.borrow_mut().prepare(&ctx.device, &ctx.queue, meshes);
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
    
    let mp_ref = scene.mesh_pipeline.borrow();

    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("glyph field pass"),
        timestamp_writes: pass_query
            .as_ref()
            .and_then(|q| q.render_pass_timestamp_writes()),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: draw_color_view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(crate::config::wgpu_color(
                    crate::config::settings().glyph_scene.clear_color,
                )),
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
    if scene.environment.is_on() {
        scene.environment.draw(&mut pass);
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

                if let Some(meshes) = &scene_meshes {
                    let q_len = meshes.quads.len() as u32;
                    let c_len = meshes.cubes.len() as u32;
                    if q_len > 0 {
                        mp_ref.render(&mut pass, crate::glyph_scene::mesh::UnitMesh::Quad, 0..q_len, &scene.mesh_frame_bg);
                    }
                    if c_len > 0 {
                        mp_ref.render(&mut pass, crate::glyph_scene::mesh::UnitMesh::Cube, q_len..(q_len + c_len), &scene.mesh_frame_bg);
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
                // A self-drawing field draws what its `prepare` emitted
                // (the ranges are empty for it by construction); then its
                // wash tier — one quad per line too small for glyphs —
                // under its own pipeline.
                scene.field.record_draws(&mut pass, &phase_draws.glyph_ranges);
                if let Some(visible) = scene.field.visible() {
                    visible.record_wash_draw(&mut pass);
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
    // unchanged, which is every golden frame). Both drivers have the mask
    // machinery (`CompositeState::selection_fx`), so an offscreen frame
    // with a pick carries the tint — `tests/visible_verbs.rs` reads it.
    // The Visible field's selection is a byte range (M3): `prepare_mask`
    // above laid its glyphs out into the field's selection buffer, and the
    // mask pass draws that buffer under the same mask pipeline.
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
                // The Visible field: what `prepare_mask` emitted, one
                // indirect draw over its selection buffer.
                Selection::ByteRange { .. } => {
                    if let Some(visible) = scene.field.visible() {
                        visible.record_mask_draw(&mut pass);
                    }
                }
                Selection::Glyph { .. } | Selection::Segment { .. } => {
                    let mask_draws: Vec<(u32, std::ops::Range<u32>)> = match sel {
                        Selection::Glyph { chunk, local } => vec![(*chunk, *local..*local + 1)],
                        // Per-chunk split — the same math cull_segments uses.
                        Selection::Segment { slot_base, slot_count } => glyph_field::split_at_chunks(
                            *slot_base,
                            *slot_count,
                            scene.field.chunk_capacity(),
                        )
                        .filter(|(chunk, _, _)| *chunk < scene.field.chunk_count())
                        .map(|(chunk, local, _)| (chunk, local))
                        .collect(),
                        Selection::ByteRange { .. } => unreachable!("matched above"),
                    };
                    scene.field.record_draws(&mut pass, &mask_draws);
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
