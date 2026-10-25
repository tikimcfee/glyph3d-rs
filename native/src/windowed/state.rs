//! The window's state: surface/config, the boxed scene, input bookkeeping,
//! FPS accounting, the egui overlay handle and the per-frame render.
//! Extracted from `windowed.rs` in the 2026-09 code-shape refactor — a pure
//! move; `pub(super)` stands in for the same-module privacy these items had
//! (the app handler constructs and drives every field).

use std::sync::Arc;
use std::time::Instant;

use winit::window::{CursorGrabMode, Window};

use crate::gpu::GpuContext;
use crate::scene::{self, SceneLike};

#[cfg(feature = "egui-ui")]
use super::ui::EguiUi;

pub(super) struct WindowState {
    pub(super) window: Arc<Window>,
    pub(super) surface: wgpu::Surface<'static>,
    pub(super) config: wgpu::SurfaceConfiguration,
    pub(super) depth: wgpu::TextureView,
    pub(super) scene: Box<dyn SceneLike>,
    pub(super) start: Instant,
    /// Stage F/G: mouse-look is active only while the pointer is grabbed
    /// (right button held, or backquote toggle).
    pub(super) grabbed: bool,
    /// True once any DeviceEvent::MouseMotion has arrived during the current
    /// grab. Environments where raw device deltas never arrive (some macOS
    /// trackpad / remote-desktop paths) fall back to CursorMoved deltas;
    /// without the flag both paths would fire and look would double-apply.
    pub(super) saw_device_delta: bool,
    /// Last cursor position, physical px (click picks use it).
    pub(super) cursor: (f32, f32),
    pub(super) last_frame: Instant,
    /// The keyboard modifiers held now (`WindowEvent::ModifiersChanged`):
    /// a press with Cmd or Ctrl held never reaches the scene (`route_key`).
    pub(super) modifiers: winit::keyboard::ModifiersState,
    /// Stage K (K6): frames presented since startup (the `--screenshot-frame`
    /// counter — distinct from `frames`, which the FPS line resets every
    /// second).
    pub(super) frames_total: u64,
    /// Stage K (K6): armed CLI capture — capture the frame that reaches N.
    pub(super) shot_at_frame: Option<(u64, std::path::PathBuf)>,
    /// Stage K (K6): capture THIS frame to the path after the final submit
    /// (armed by F2, or by the CLI frame counter).
    pub(super) capture_pending: Option<std::path::PathBuf>,
    /// Post-L3 fix: set when the surface reports Occluded (fully covered /
    /// display asleep). While set, about_to_wait throttles redraw requests
    /// to ~2 Hz instead of spinning at ~100% CPU (measured pre-fix: ~250k
    /// occlusion-skips/s). winit 0.30 gives no reliable wake on
    /// un-occlusion — WindowEvent::Occluded is iOS-only ("Others:
    /// Unsupported"), and RedrawRequested fires only on OS invalidation or
    /// an explicit request_redraw — so a purely event-driven wait could
    /// strand the window blank; the 500 ms retry guarantees recovery.
    pub(super) occluded: bool,
    /// Last time an occluded retry was issued (the 2 Hz throttle).
    pub(super) occluded_retry_at: Instant,
    // FPS accounting
    pub(super) frames: u32,
    pub(super) fps_window_start: Instant,
    /// Stage H: GPU pass timing accumulator for the once-per-second line
    /// (only fed when GLYPH_PROFILE=1 built a profiler).
    pub(super) profile: crate::gpu::ProfileAccumulator,
    /// Stage K: egui overlay state; `None` under `--no-ui`.
    #[cfg(feature = "egui-ui")]
    pub(super) egui: Option<EguiUi>,
    /// Stage K (K3): debug-UI probe — camera/pick snapshot written by the
    /// scene each frame (None for the demo scene and under --no-ui).
    #[cfg(feature = "egui-ui")]
    pub(super) ui_probe: Option<crate::glyph_scene::UiProbe>,
    /// Stage K (K3): the 1 Hz FPS figure, mirrored for the Debug panel (the
    /// println below stays the primary output — offscreen logs read it).
    #[cfg(feature = "egui-ui")]
    pub(super) ui_fps: f32,
    /// Stage K (K4) dev-only verification hook state (GLYPH_K4_SELFTEST=1):
    /// 0 = off/done, 1 = armed (fires at t>3 s), 2 = fired (print the
    /// after-counters next frame). See AGENTS.md debug env vars.
    #[cfg(feature = "egui-ui")]
    pub(super) k4_selftest: u8,
    /// The command bus's queue (2026-10-24, `bus.rs`): lines the hotkeys,
    /// panels and self-tests pushed (`push_line`), run FIFO between frames by
    /// `exec::drain` in the RedrawRequested arm. It replaced the one-slot
    /// relayout signal, which a second request in the same frame overwrote.
    pub(super) commands: Vec<super::bus::Parsed>,
    /// A fly waiting for its row's live pose: (row, `frames_total` when
    /// queued). `exec::finish_fly` frames it once a frame has rendered.
    #[cfg(feature = "egui-ui")]
    pub(super) pending_fly: Option<(usize, u64)>,
    /// Dev-only verification hook state (GLYPH_ZSPACE_SELFTEST=1): same
    /// 0/1/2/3 arming as cluster_selftest below, driving the layout dial
    /// through the same command-bus line the slider's release pushes.
    #[cfg(feature = "egui-ui")]
    pub(super) zspace_selftest: u8,
    /// Dev-only verification hook state (GLYPH_CLUSTER_SELFTEST=1): same
    /// 0/1/2/3 arming as zspace_selftest, toggling the cluster mode through
    /// the same command-bus line the panel's button pushes.
    #[cfg(feature = "egui-ui")]
    pub(super) cluster_selftest: u8,
    /// Dev-only verification hook state (GLYPH_FIELDMODE_SELFTEST=1): at
    /// t≈3 s cycles the field mode Instanced → Derived → Visible through the
    /// same command-bus line the panel's selector pushes, printing the
    /// HUD line after each rebuild. 0 = off/done; odd states fire, even
    /// states are the quiet frame after a rebuild (see `render`).
    #[cfg(feature = "egui-ui")]
    pub(super) fieldmode_selftest: u8,
    /// Dev-only verification hook state (GLYPH_VISIBLE_VERB_SELFTEST=1, M3):
    /// at t≈3 s picks a glyph through `apply_pick` (the CLI/click entry) and
    /// applies recolor-glyph, nudge-glyph, set-glyph-background, hide-group
    /// and show-group one every few frames, printing each verb's reply and
    /// the HUD line after it. 0 = off/done; 1 = armed; 2.. = the step to run.
    #[cfg(feature = "egui-ui")]
    pub(super) visible_verb_selftest: u8,
    /// Dev-only verification hook state (GLYPH_GRAB_SELFTEST=file[:dx:dy],
    /// C29): the windowed GRAB path — `g` on a picked file, cursor drags
    /// through `SceneLike::on_cursor`, `g` again — printing the HUD line
    /// after each phase. 0 = off/done; 1 = armed; 2.. = the step to run.
    #[cfg(feature = "egui-ui")]
    pub(super) grab_selftest: u8,
}

/// The `--cam-pose` argument that reproduces a Fly pose: eye, then yaw and
/// pitch in DEGREES (the CLI converts back; `cli::ops`). One formatter for
/// the panel's copy button and the F2 `GLYPH_POSE_PRINT` line, so the two
/// cannot disagree about units.
pub(super) fn cam_pose_arg(eye: [f32; 3], yaw: f32, pitch: f32) -> String {
    format!(
        "--cam-pose {:.3} {:.3} {:.3} {:.3} {:.3}",
        eye[0],
        eye[1],
        eye[2],
        yaw.to_degrees(),
        pitch.to_degrees()
    )
}

impl WindowState {
    pub(super) fn resize(&mut self, ctx: &GpuContext, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return; // minimized
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&ctx.device, &self.config);
        self.depth = scene::create_depth(&ctx.device, self.scene.depth_format(), width, height);
    }

    /// Stage F: grab the pointer for mouse-look (confined to the window —
    /// Locked is unsupported on macOS — and hidden). DeviceEvent deltas keep
    /// arriving at the window edges, which is what look control needs.
    ///
    /// Robustness fix: a failed cursor grab must NOT silently kill mouse-look
    /// (that was the "right-drag does nothing" bug — no log, grabbed stayed
    /// false). We now try Confined then Locked, log the outcome, and stay
    /// grabbed even if both fail: the CursorMoved fallback still gives look
    /// control, just without edge confinement.
    pub(super) fn grab(&mut self) {
        if self.grabbed {
            return;
        }
        self.saw_device_delta = false;
        if self.window.set_cursor_grab(CursorGrabMode::Confined).is_ok() {
            self.window.set_cursor_visible(false);
            log::info!("mouse-look: pointer grabbed (confined)");
        } else if self.window.set_cursor_grab(CursorGrabMode::Locked).is_ok() {
            self.window.set_cursor_visible(false);
            log::info!("mouse-look: pointer grabbed (locked)");
        } else {
            log::warn!(
                "mouse-look: cursor grab unsupported here — look still works, \
                 but the pointer is not confined to the window"
            );
        }
        self.grabbed = true;
    }

    pub(super) fn ungrab(&mut self) {
        if !self.grabbed {
            return;
        }
        let _ = self.window.set_cursor_grab(CursorGrabMode::None);
        self.window.set_cursor_visible(true);
        self.grabbed = false;
    }

    /// Stage K (K2): does egui currently want the pointer (hovering a panel,
    /// or mid-drag of a widget)? Feature off / `--no-ui`: never.
    /// `EventResponse.consumed` for CursorMoved only covers the mid-drag
    /// case (`egui_is_using_pointer`); this query additionally covers hover,
    /// so scene cursor effects (grabbed-group drags) don't track the pointer
    /// across egui windows.
    pub(super) fn egui_wants_pointer(&self) -> bool {
        #[cfg(feature = "egui-ui")]
        {
            self.egui
                .as_ref()
                .is_some_and(|e| e.ctx.egui_wants_pointer_input())
        }
        #[cfg(not(feature = "egui-ui"))]
        {
            false
        }
    }

    /// Stage K (K6): read back the just-encoded surface texture (the
    /// COMPOSED frame — scene pass + egui pass) and write it to `path` as
    /// PNG. Mirrors offscreen.rs's readback (256-byte-aligned rows,
    /// blocking map) with one difference: the windowed surface is
    /// Bgra8UnormSrgb, so every pixel is swizzled BGRA→RGBA (offscreen's
    /// target is Rgba and skips this). Called after the final queue.submit
    /// and before present; the blocking wait stalls the loop for one frame,
    /// which is fine for an on-demand capture.
    pub(super) fn capture_to_png(&self, ctx: &GpuContext, frame: &wgpu::SurfaceTexture, path: &std::path::Path) {
        let (w, h) = (self.config.width, self.config.height);
        let unpadded_bpr = w * 4;
        let padded_bpr = unpadded_bpr.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("windowed shot readback"),
            size: (padded_bpr * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("windowed shot copy"), // Stage L (O2)
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &frame.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bpr),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        ctx.queue.submit([encoder.finish()]);

        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        ctx.device
            .poll(wgpu::PollType::Wait { submission_index: None, timeout: None })
            .expect("device poll failed during windowed-shot readback");
        rx.recv()
            .expect("windowed-shot map_async callback dropped")
            .expect("windowed-shot buffer map failed");

        let data = slice.get_mapped_range().expect("windowed-shot range not mapped");
        let mut pixels = Vec::with_capacity((unpadded_bpr * h) as usize);
        for row in 0..h {
            let start = (row * padded_bpr) as usize;
            for px in data[start..start + unpadded_bpr as usize].as_chunks::<4>().0 {
                // BGRA → RGBA swizzle.
                pixels.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
            }
        }
        drop(data);
        readback.unmap();

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create windowed-shot directory");
        }
        image::save_buffer(path, &pixels, w, h, image::ColorType::Rgba8)
            .expect("failed to write windowed-shot PNG");
        let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        println!("windowed shot: wrote {}", abs.display());
    }

    pub(super) fn render(&mut self, ctx: &GpuContext) {
        // The dev-only GLYPH_*_SELFTEST hooks (selftest.rs) run first, as they
        // always have: a hook that fires a rebuild does so before this frame.
        #[cfg(feature = "egui-ui")]
        self.run_selftests(ctx);
        // wgpu 30: get_current_texture returns a status enum instead of Result.
        use wgpu::CurrentSurfaceTexture as Cst;
        let frame = match self.surface.get_current_texture() {
            Cst::Success(f) | Cst::Suboptimal(f) => f,
            // Lost/outdated surfaces happen on resize; reconfigure and skip.
            Cst::Lost | Cst::Outdated => {
                self.surface.configure(&ctx.device, &self.config);
                return;
            }
            // Post-L3 fix: occlusion is sticky (fully covered window /
            // display asleep) — mark it so about_to_wait throttles instead
            // of spinning. Timeout is transient acquire contention: skip the
            // frame and keep today's cadence (unchanged).
            Cst::Occluded => {
                if !self.occluded {
                    log::info!("surface occluded — throttling redraws to ~2 Hz until visible");
                }
                self.occluded = true;
                return;
            }
            Cst::Timeout => return,
            Cst::Validation => {
                log::error!("surface validation error on acquire");
                return;
            }
        };
        let view = frame.texture.create_view(&Default::default());

        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("windowed frame"), // Stage L (O2)
        });
        self.scene.render(
            ctx,
            &mut encoder,
            &crate::scene::FrameTarget {
                color_view: &view,
                // Stage L (L3): the composite's copy-vs-shader split keys on
                // the format; the copy path needs the texture handle.
                color_texture: &frame.texture,
                color_format: self.config.format,
                depth_view: &self.depth,
                width: self.config.width,
                height: self.config.height,
            },
            self.time(),
        );

        // Stage K (K1): egui overlay, painted as a second pass on the SAME
        // encoder after the scene pass. Exact 0.36 lifecycle (verified against
        // the vendored source): take_egui_input → run_ui →
        // handle_platform_output → tessellate → update_texture per
        // textures_delta.set (drained: TexturesDelta debug-panics if dropped
        // with unapplied deltas) → update_buffers (mandatory — render()
        // panics otherwise) → render pass with LoadOp::Load and NO depth
        // attachment (egui paints OVER the scene) → submit → free_texture per
        // textures_delta.free AFTER queue.submit (the freed textures may be
        // referenced by the in-flight submission). `forget_lifetime` turns
        // encoder-aliasing mistakes into runtime errors, so the pass is
        // scoped and dropped before we touch the encoder again.
        #[cfg(feature = "egui-ui")]
        let mut egui_textures_delta: Option<egui::TexturesDelta> = None;
        // Stage K (K2): set inside the egui block (post-run_ui, so egui's
        // focus/hover state is fresh for THIS frame); acted on after the
        // borrow ends.
        #[cfg(feature = "egui-ui")]
        let mut egui_wants_input = false;
        #[cfg(feature = "egui-ui")]
        let mut panel_lines: Vec<String> = Vec::new();
        #[cfg(feature = "egui-ui")]
        if let Some(egui) = self.egui.as_mut() {
            let raw_input = egui.state.take_egui_input(&self.window);
            // Clone the Context (an Arc bump) so the closure below can borrow
            // the other EguiUi / WindowState fields disjointly.
            let egui_ctx = egui.ctx.clone();
            // K3: snapshot the probe before the closure (RefCell stays
            // unborrowed across the UI build).
            let probe_snap = self.ui_probe.as_ref().map(|p| p.borrow().clone());
            let fps = self.ui_fps;
            // The panels' own state (open flags, filters, the session list).
            let shell = &mut egui.shell;
            shell.selected_group = self.scene.selected_group_id();
            let mut clipboard = shell.pending_clipboard.take();
            // Every action a panel takes is a line pushed here, queued on
            // the command bus after the pass and run between frames
            // (exec::drain). The panels read the probe snapshot and never
            // touch the scene; the only cell writes left are the K4 LOD
            // tuning, the z dial's own value and the browser's row
            // feedback, which are view state, not intents.
            let panel_lines = &mut panel_lines;
            let full_output = egui_ctx.run_ui(raw_input, |root_ui| {
                if let Some(text) = clipboard.take() {
                    root_ui.ctx().copy_text(text);
                }
                let ctx = root_ui.ctx();
                let snap = probe_snap.as_ref();
                let cell = self.ui_probe.as_ref();
                super::panels::hud::show(ctx, snap, fps, shell.hud_open);
                super::panels::debug::show(ctx, snap, cell, fps, shell, panel_lines);
                super::panels::carrel::show(ctx, snap, panel_lines);
                super::panels::sessions::show(ctx, shell, panel_lines);
            });
            let egui::FullOutput {
                platform_output,
                mut textures_delta,
                shapes,
                pixels_per_point,
                viewport_output: _,
            } = full_output;
            egui.state
                .handle_platform_output(&self.window, platform_output);
            let clipped = egui.ctx.tessellate(shapes, pixels_per_point);
            let screen = egui_wgpu::ScreenDescriptor {
                size_in_pixels: [self.config.width, self.config.height],
                pixels_per_point,
            };
            for (id, image_deltas) in textures_delta.set.drain() {
                for image_delta in image_deltas {
                    egui.renderer
                        .update_texture(&ctx.device, &ctx.queue, id, &image_delta);
                }
            }
            let user_cmds = egui.renderer.update_buffers(
                &ctx.device,
                &ctx.queue,
                &mut encoder,
                &clipped,
                &screen,
            );
            // Callback command buffers (from paint callbacks) must be
            // submitted before the main buffer. K1 registers no callbacks, so
            // this is always empty — the branch keeps the contract explicit.
            if !user_cmds.is_empty() {
                ctx.queue.submit(user_cmds);
            }
            // Stage H scheme, mirrored: pass-boundary timestamp query.
            let pass_query = ctx
                .profiler
                .as_ref()
                .map(|p| p.borrow().begin_pass_query("egui pass", &mut encoder));
            {
                let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("egui pass"),
                    timestamp_writes: pass_query
                        .as_ref()
                        .and_then(|q| q.render_pass_timestamp_writes()),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    ..Default::default()
                });
                egui.renderer
                    .render(&mut pass.forget_lifetime(), &clipped, &screen);
            }
            if let (Some(p), Some(q)) = (&ctx.profiler, pass_query) {
                p.borrow().end_query(&mut encoder, q);
            }
            egui_textures_delta = Some(textures_delta);
            egui_wants_input =
                egui.ctx.egui_wants_pointer_input() || egui.ctx.egui_wants_keyboard_input();
        }
        #[cfg(feature = "egui-ui")]
        for line in panel_lines {
            self.push_line(&line);
        }

        // Stage K (K2): UI focus and the mouse-look grab cannot coexist —
        // while grabbed, raw DeviceEvent deltas drive look and bypass egui
        // entirely, and a hidden/confined pointer must never leave egui
        // thinking it is being hovered. The moment egui wants input (pointer
        // over a panel, or a focused widget), release the grab. Grab
        // re-acquires normally once the pointer leaves egui's area
        // (`egui_wants_pointer_input` is false again, so right-press routes
        // to the scene). Mid-look-drags are NOT interrupted: while a button
        // is down and the drag started outside egui, egui reports wanting
        // nothing (see `egui_wants_pointer_input`'s `any_down` clause).
        #[cfg(feature = "egui-ui")]
        if egui_wants_input && self.grabbed {
            self.ungrab();
        }

        // Stage H: resolve profiler queries before submit (see offscreen.rs).
        if let Some(p) = &ctx.profiler {
            p.borrow_mut().resolve_queries(&mut encoder);
        }
        ctx.queue.submit([encoder.finish()]);
        // Stage K: free egui textures only AFTER the submit that referenced
        // them is handed to the queue.
        #[cfg(feature = "egui-ui")]
        if let (Some(egui), Some(delta)) = (self.egui.as_mut(), egui_textures_delta.as_mut()) {
            for id in delta.free.drain() {
                egui.renderer.free_texture(&id);
            }
        }
        // Stage K (K6): scripted CLI capture — arm once this frame reaches N
        // (the current frame is the (frames_total+1)-th).
        if let Some((n, path)) = &self.shot_at_frame {
            if self.frames_total + 1 >= *n {
                self.capture_pending = Some(path.clone());
                self.shot_at_frame = None;
            }
        }
        // Stage K (K6): capture BEFORE present — the acquired texture is the
        // composed frame (scene + egui), still ours to copy from.
        if let Some(path) = self.capture_pending.take() {
            self.capture_to_png(ctx, &frame, &path);
        }
        // wgpu 30: presentation goes through the queue, not the texture.
        ctx.queue.present(frame);
        self.frames_total += 1;
        // Post-L3 fix: a successful present clears occlusion (the 2 Hz retry
        // found the window visible again).
        self.occluded = false;

        // Stage H: close the profiler frame and fold any GPU-completed frame
        // into the running means (non-blocking pump for the query maps).
        if let Some(p) = &ctx.profiler {
            if let Err(e) = p.borrow_mut().end_frame() {
                log::warn!("profiler end_frame: {e}");
            }
            let _ = ctx.device.poll(wgpu::PollType::Poll);
            let period = ctx.queue.get_timestamp_period();
            if let Some(results) = p.borrow_mut().process_finished_frame(period) {
                self.profile.add_frame(&results);
            }
        }

        // FPS: print a line every second.
        self.frames += 1;
        let elapsed = self.fps_window_start.elapsed();
        if elapsed.as_secs_f32() >= 1.0 {
            let profile_suffix = if ctx.profiler.is_some() {
                let cpu = crate::gpu::take_cpu_scope_summary(ctx);
                format!(
                    " | profile: GPU {} | CPU {}",
                    if self.profile.scopes.is_empty() {
                        "—".to_string()
                    } else {
                        self.profile.summary()
                    },
                    if cpu.is_empty() { "—".to_string() } else { cpu },
                )
            } else {
                String::new()
            };
            // The present mode rides on the line so a figure is never read
            // without its cap: under Fifo this IS the display's refresh.
            println!(
                "FPS: {:.1} ({} frames in {:.2?}, {} instances, present={:?}){profile_suffix}",
                self.frames as f32 / elapsed.as_secs_f32(),
                self.frames,
                elapsed,
                self.scene.instance_count(),
                self.config.present_mode,
            );
            // Stage K (K3): mirror the same figure for the Debug panel.
            #[cfg(feature = "egui-ui")]
            {
                self.ui_fps = self.frames as f32 / elapsed.as_secs_f32();
            }
            self.frames = 0;
            self.fps_window_start = Instant::now();
            self.profile = crate::gpu::ProfileAccumulator::default();
        }
    }

    /// Wall-clock animation time (windowed mode is not determinism-bound).
    pub(super) fn time(&self) -> f32 {
        self.start.elapsed().as_secs_f32()
    }
}
