//! Windowed mode: winit window + wgpu surface, uncapped continuous render loop,
//! FPS printed to stdout once per second. Renders whichever scene the CLI chose.
//!
//! Stage F: the glyph scenes run a FLY camera — WASD strafe/forward, E|R up,
//! Q|F down, scroll wheel = speed multiplier.
//! Stage G: interaction is split between the two mouse buttons so picking and
//! the fly camera coexist:
//!   - LEFT click (ungrabbed pointer) = PICK the glyph under the cursor
//!     (prints file:row:col:char, flash-highlights it);
//!   - RIGHT press-and-drag = mouse-look (pointer confined + hidden while
//!     held; raw DeviceEvent deltas); Esc also releases;
//!   - verb keys act on the last pick: `h` highlight line, `g` grab/release
//!     the picked file (mouse drags it in the view plane, scroll scales it),
//!     `t` cycle the tint palette, `x` toggle hidden.
//!
//! Stage K: egui 0.36 overlay (feature `egui-ui`, default ON; `--no-ui` at
//! runtime gives exact pre-K behavior). egui sees every window event FIRST;
//! scene input routing only handles events egui did not consume. Per frame,
//! after the scene pass on the SAME encoder, an egui pass (LoadOp::Load, no
//! depth attachment) paints the tessellated UI onto the surface. K1 shipped
//! the plumbing with an EMPTY CentralPanel. K2 input-consumption rules:
//!   - keys route only when egui hasn't consumed them (a focused egui widget
//!     swallows keys — typing WASD/h/g/t/x in a text field must not fly the
//!     camera or fire verbs; Tab is always egui's);
//!   - right-press grab and left-click pick route only when egui doesn't
//!     want the pointer (hovering a panel counts); right-RELEASE always
//!     ungrabs (consumed releases must not latch the grab);
//!   - cursor bookkeeping always updates; scene cursor effects (look
//!     fallback, grabbed-group drags) skip while egui wants the pointer;
//!   - the moment egui wants ANY input, the look-grab is released (checked
//!     per frame in render()); it re-acquires once the pointer leaves egui.
//!
//! K3 added the Debug window; K4 the live LOD dial; K5 the group browser.
//!
//! Stage M replaces the single Debug window with the interface shell
//! (`ui.rs`): HUD (top), Files (left), Inspector (right), Console (bottom),
//! palette popup. Every panel action returns a `ui::Command`; the executor
//! below is the single place where intents become effects, through the exact
//! SceneLike APIs the CLI op-stream uses. F1 toggles the whole shell;
//! Cmd/Ctrl-K toggles the palette (app-level, like F2, so it fires even from
//! a focused text field — a chord never types).
//!
//! K6: in-window screenshot. The surface is configured with COPY_SRC (an
//! assert at configure checks the adapter supports it); on demand (F2 — an
//! app-level hotkey that fires regardless of egui focus — or the
//! `--screenshot-frame N --screenshot-out PATH` CLI pair) the just-encoded
//! surface texture is copied to a MAP_READ buffer AFTER the final
//! queue.submit and BEFORE present, blocking-mapped, swizzled BGRA→RGBA
//! (the windowed surface is Bgra8UnormSrgb — offscreen is Rgba and needs no
//! swizzle), and written as PNG. The readback happens after BOTH the scene
//! pass and the egui pass, so the PNG is the COMPOSED frame — 3D scene AND
//! the Debug window; that inclusion is the point (pixel-verification seam
//! for "invisible by construction" UI claims, per the stage erratum).

use std::sync::Arc;
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

use crate::glyph_scene::CameraMode;
use crate::gpu::GpuContext;
use crate::scene::{self, SceneLike};
use crate::{build_scene, Op, SceneChoice};

/// Stage K: the egui overlay — context, winit event translation, and the
/// wgpu painter. `None` in WindowState under `--no-ui`; the type (and all
/// egui code below) is compiled out entirely without the `egui-ui` feature.
#[cfg(feature = "egui-ui")]
struct EguiUi {
    ctx: egui::Context,
    state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
    /// Stage M: all interface memory — panel visibility, console log/input/
    /// history, palette state, the K5 filter/selection (promoted), the
    /// inspector's parameterized-verb scratch. One struct so a future stage
    /// can serialize the session (notes/11, M6).
    ui_state: crate::ui::UiState,
}

struct WindowState {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    depth: wgpu::TextureView,
    scene: Box<dyn SceneLike>,
    start: Instant,
    /// Stage F/G: mouse-look is active only while the pointer is grabbed
    /// (right button held, or backquote toggle).
    grabbed: bool,
    /// True once any DeviceEvent::MouseMotion has arrived during the current
    /// grab. Environments where raw device deltas never arrive (some macOS
    /// trackpad / remote-desktop paths) fall back to CursorMoved deltas;
    /// without the flag both paths would fire and look would double-apply.
    saw_device_delta: bool,
    /// Last cursor position, physical px (click picks use it).
    cursor: (f32, f32),
    /// Stage M: tracked from WindowEvent::ModifiersChanged — winit 0.30
    /// removed KeyEvent's modifiers field, and chord hotkeys (Cmd/Ctrl-K)
    /// need them app-level.
    modifiers: winit::keyboard::ModifiersState,
    last_frame: Instant,
    /// Stage K (K6): frames presented since startup (the `--screenshot-frame`
    /// counter — distinct from `frames`, which the FPS line resets every
    /// second).
    frames_total: u64,
    /// Stage K (K6): armed CLI capture — capture the frame that reaches N.
    shot_at_frame: Option<(u64, std::path::PathBuf)>,
    /// Stage K (K6): capture THIS frame to the path after the final submit
    /// (armed by F2, or by the CLI frame counter).
    capture_pending: Option<std::path::PathBuf>,
    // FPS accounting
    frames: u32,
    fps_window_start: Instant,
    /// Stage H: GPU pass timing accumulator for the once-per-second line
    /// (only fed when GLYPH_PROFILE=1 built a profiler).
    profile: crate::gpu::ProfileAccumulator,
    /// Stage K: egui overlay state; `None` under `--no-ui`.
    #[cfg(feature = "egui-ui")]
    egui: Option<EguiUi>,
    /// Stage K (K3): debug-UI probe — camera/pick snapshot written by the
    /// scene each frame (None for the demo scene and under --no-ui).
    #[cfg(feature = "egui-ui")]
    ui_probe: Option<crate::glyph_scene::UiProbe>,
    /// Stage K (K3): the 1 Hz FPS figure, mirrored for the Debug panel (the
    /// println below stays the primary output — offscreen logs read it).
    #[cfg(feature = "egui-ui")]
    ui_fps: f32,
    /// Stage K (K4) dev-only verification hook state (GLYPH_K4_SELFTEST=1):
    /// 0 = off/done, 1 = armed (fires at t>3 s), 2 = fired (print the
    /// after-counters next frame). See AGENTS.md debug env vars.
    #[cfg(feature = "egui-ui")]
    k4_selftest: u8,
}

impl WindowState {
    fn resize(&mut self, ctx: &GpuContext, width: u32, height: u32) {
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
    fn grab(&mut self) {
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

    fn ungrab(&mut self) {
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
    fn egui_wants_pointer(&self) -> bool {
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
    fn capture_to_png(&self, ctx: &GpuContext, frame: &wgpu::SurfaceTexture, path: &std::path::Path) {
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
        let mut encoder = ctx.device.create_command_encoder(&Default::default());
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

    fn render(&mut self, ctx: &GpuContext) {
        // Stage K (K4) dev-only verification hook (GLYPH_K4_SELFTEST=1):
        // move the LOD slider programmatically once (t≈3 s) and log the cull
        // counters before/after — exercises the full panel → probe →
        // CullState → cull_segments write path without a human at the mouse.
        #[cfg(feature = "egui-ui")]
        match self.k4_selftest {
            1 if self.time() > 3.0 => {
                if let Some(probe) = &self.ui_probe {
                    let mut p = probe.borrow_mut();
                    println!(
                        "K4SELFTEST before: lod_min_px={:.2} -> ranges={} instances={} backdrops={}",
                        p.lod_min_px, p.cull_ranges, p.cull_instances, p.cull_backdrops
                    );
                    // The panel slider's range max: at 16 px/em every visible
                    // fixture segment drops to its backdrop quad.
                    p.lod_min_px = 16.0;
                }
                self.k4_selftest = 2;
            }
            2 => {
                if let Some(probe) = &self.ui_probe {
                    let p = probe.borrow();
                    println!(
                        "K4SELFTEST after:  lod_min_px={:.2} -> ranges={} instances={} backdrops={}",
                        p.lod_min_px, p.cull_ranges, p.cull_instances, p.cull_backdrops
                    );
                }
                self.k4_selftest = 0;
            }
            _ => {}
        }
        // wgpu 30: get_current_texture returns a status enum instead of Result.
        use wgpu::CurrentSurfaceTexture as Cst;
        let frame = match self.surface.get_current_texture() {
            Cst::Success(f) | Cst::Suboptimal(f) => f,
            // Lost/outdated surfaces happen on resize; reconfigure and skip.
            Cst::Lost | Cst::Outdated => {
                self.surface.configure(&ctx.device, &self.config);
                return;
            }
            // Occluded/Timeout: skip this frame, try again next redraw.
            Cst::Occluded | Cst::Timeout => return,
            Cst::Validation => {
                log::error!("surface validation error on acquire");
                return;
            }
        };
        let view = frame.texture.create_view(&Default::default());

        let mut encoder = ctx.device.create_command_encoder(&Default::default());
        self.scene.render(
            ctx,
            &mut encoder,
            &crate::scene::FrameTarget {
                color_view: &view,
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
        // Stage M: commands collected by this frame's UI build; executed
        // after the egui pass closes (loop below the block).
        #[cfg(feature = "egui-ui")]
        let mut commands: Vec<crate::ui::Command> = Vec::new();
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
            // Stage M: build one interface frame INSIDE the egui pass
            // (panels must register against an active pass). draw_ui is pure
            // (input, state) → commands; execution happens after the pass
            // closes, through the bus below — one place where intents become
            // effects, so the console log records exactly that stream.
            let ui_input = crate::ui::UiInput {
                probe: probe_snap,
                probe_cell: self.ui_probe.clone(),
                fps,
                instances: self.scene.instance_count(),
            };
            let full_output = egui_ctx.run_ui(raw_input, |root_ui| {
                commands = crate::ui::draw_ui(root_ui, &ui_input, &mut egui.ui_state);
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

        // Stage M: run the frame's commands. The egui block held &mut self's
        // egui field; here, after the pass closes and before queue.submit,
        // verb buffer-writes land in this frame's submission — the same
        // record→write→submit ordering K3's in-closure buttons had.
        #[cfg(feature = "egui-ui")]
        for cmd in commands.drain(..) {
            self.execute(ctx, cmd);
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
            println!(
                "FPS: {:.1} ({} frames in {:.2?}, {} instances){profile_suffix}",
                self.frames as f32 / elapsed.as_secs_f32(),
                self.frames,
                elapsed,
                self.scene.instance_count(),
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

    /// Stage M: the command-bus executor — the single place where user
    /// intents become effects. Every scene-facing branch calls the exact API
    /// the CLI op-stream uses; the console log records the echo plus the
    /// same result line the CLI op prints, so console and stdout tell one
    /// story (and panel ⇔ CLI equivalence stays checkable by eye).
    #[cfg(feature = "egui-ui")]
    fn execute(&mut self, ctx: &GpuContext, cmd: crate::ui::Command) {
        use crate::ui::{format_command, help_text, Command, LogKind, PanelId};
        let echo = format_command(&cmd);
        let (result, error): (Option<String>, Option<String>) = match cmd {
            Command::Verb(v) => (self.scene.apply_verb(ctx, &v), None),
            Command::PickRowCol { file, row, col } => (
                self.scene.apply_pick(
                    ctx,
                    &crate::glyph_scene::PickCommand::RowCol { file, row, col },
                ),
                None,
            ),
            Command::FlyFile(substr) => match self.fly_to_file(&substr) {
                Ok(line) => (Some(line), None),
                Err(e) => (None, Some(e)),
            },
            Command::FlyFileBox { center, half } => {
                let aspect = self.config.width as f32 / self.config.height.max(1) as f32;
                let (eye, yaw, pitch) = crate::ui::fly_to_framing(center, half, aspect);
                self.scene.set_cam_pose(eye, yaw, pitch);
                (
                    Some(format!(
                        "camera: framed ({:.1}, {:.1}) at dist {:.1}",
                        center[0], center[1], eye[2]
                    )),
                    None,
                )
            }
            Command::Screenshot => {
                self.capture_pending = Some(windowed_shot_path());
                (None, None)
            }
            Command::TogglePanel(p) => match self.egui.as_mut() {
                Some(egui) => {
                    let flag = match p {
                        PanelId::Files => &mut egui.ui_state.files_open,
                        PanelId::Inspector => &mut egui.ui_state.inspector_open,
                        PanelId::Console => &mut egui.ui_state.console_open,
                    };
                    *flag = !*flag;
                    (Some(format!("panel {}: now {}", p.name(), *flag)), None)
                }
                None => (None, None),
            },
            Command::Help => {
                if let Some(egui) = self.egui.as_mut() {
                    for line in help_text().split('\n') {
                        egui.ui_state.log(LogKind::Info, line);
                    }
                }
                (None, None)
            }
            Command::ClearLog => {
                if let Some(egui) = self.egui.as_mut() {
                    egui.ui_state.log.clear();
                }
                (None, None)
            }
        };
        if let Some(egui) = self.egui.as_mut() {
            egui.ui_state.log(LogKind::Cmd, echo);
            if let Some(line) = result {
                println!("{line}");
                egui.ui_state.log(LogKind::Result, line);
            }
            if let Some(e) = error {
                egui.ui_state.log(LogKind::Error, e);
            }
        }
    }

    /// Resolve `fly <substr>` against the probe's group table and frame the
    /// first match with the K5 formula (live viewport aspect).
    #[cfg(feature = "egui-ui")]
    fn fly_to_file(&mut self, substr: &str) -> Result<String, String> {
        let Some(probe) = self.ui_probe.as_ref() else {
            return Err("fly: no group table (demo scene has no files)".to_string());
        };
        let probe = probe.borrow();
        let Some(idx) = probe.files.iter().position(|r| r.rel_path.contains(substr)) else {
            return Err(format!("fly: no file matches {substr:?}"));
        };
        let rel = probe.files[idx].rel_path.clone();
        let dynst = probe.file_dyn.get(idx).copied().unwrap_or_default();
        // End the RefCell borrow before touching the scene — the K4 slider
        // holds borrow_muts only inside the egui pass, but keeping shared
        // borrows short is the rule that makes that safe.
        drop(probe);
        let aspect = self.config.width as f32 / self.config.height.max(1) as f32;
        let (eye, yaw, pitch) = crate::ui::fly_to_framing(dynst.center, dynst.half, aspect);
        self.scene.set_cam_pose(eye, yaw, pitch);
        Ok(format!("camera: fly {rel} (dist {:.1})", eye[2]))
    }

    /// Wall-clock animation time (windowed mode is not determinism-bound).
    fn time(&self) -> f32 {
        self.start.elapsed().as_secs_f32()
    }
}

struct App<'a> {
    ctx: GpuContext,
    choice: &'a SceneChoice,
    cull: bool,
    /// Stage G: scripted picks/verbs applied once at startup (smoke testing
    /// the same code path the windowed verbs use).
    ops: &'a [Op],
    start: Instant,
    state: Option<WindowState>,
    /// Stage K: build the egui overlay (false under `--no-ui`).
    #[cfg(feature = "egui-ui")]
    ui: bool,
    /// Stage K (K6): scripted in-window capture (`--screenshot-frame N`
    /// `--screenshot-out PATH`).
    shot: Option<(u64, std::path::PathBuf)>,
}

impl ApplicationHandler for App<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("glyph3d-native — glyph field + interface shell")
                        .with_inner_size(winit::dpi::LogicalSize::new(1600.0, 1000.0)),
                )
                .expect("window creation failed"),
        );

        // Arc<Window> gives the surface a 'static lifetime handle.
        let surface = self
            .ctx
            .instance
            .create_surface(window.clone())
            .expect("surface creation failed");

        let caps = surface.get_capabilities(&self.ctx.adapter);
        // Prefer an sRGB format to match the offscreen oracle's output space.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        // Stage K (K6): COPY_SRC enables the in-window screenshot readback
        // (copy_texture_to_buffer off the acquired surface texture). Metal
        // supports it; assert once at configure so an exotic adapter fails
        // loud at startup, not at capture time.
        assert!(
            caps.usages.contains(wgpu::TextureUsages::COPY_SRC),
            "K6: surface/adapter does not support COPY_SRC — in-window screenshot impossible \
             (caps.usages = {:?})",
            caps.usages
        );
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            format,
            // Auto == sRGB for 8-bit formats; required field in wgpu 30.
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width,
            height: size.height,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&self.ctx.device, &config);

        // Stage K (K1): egui overlay state, constructed after the surface is
        // configured. The renderer gets the ACTUAL configured surface format
        // (never hardcode it): our format is sRGB, which egui accepts via its
        // linear-framebuffer shader path — it logs a once-per-pipeline warning
        // (accepted: switching the surface to gamma-space would perturb the
        // glyph pass's output encoding for zero UI gain).
        #[cfg(feature = "egui-ui")]
        let egui = if self.ui {
            let egui_ctx = egui::Context::default();
            let state = egui_winit::State::new(
                egui_ctx.clone(),
                egui::ViewportId::ROOT,
                &*window,
                Some(window.scale_factor() as f32),
                None,
                Some(self.ctx.device.limits().max_texture_dimension_2d as usize),
            );
            let renderer = egui_wgpu::Renderer::new(
                &self.ctx.device,
                format,
                egui_wgpu::RendererOptions::default(),
            );
            Some(EguiUi {
                ctx: egui_ctx,
                state,
                renderer,
                // Seed the log so the console is never a blank void on first
                // open — it teaches the two entry points and `help`.
                ui_state: {
                    let mut s = crate::ui::UiState::default();
                    s.log(
                        crate::ui::LogKind::Info,
                        "shell ready — F1 toggle · ⌘K palette · type help below",
                    );
                    s
                },
            })
        } else {
            None
        };

        // Stage F: windowed glyph scenes get the fly camera (the Stage A demo
        // scene keeps its internal orbit; it ignores camera_mode).
        // Stage K (K3): with the UI on, install the debug-UI probe on the
        // concrete GlyphScene BEFORE type erasure (see build_scene_probed).
        #[cfg(feature = "egui-ui")]
        let (mut scene, ui_probe) = if self.ui {
            crate::build_scene_probed(&self.ctx, format, self.choice, CameraMode::Fly, self.cull)
        } else {
            (build_scene(&self.ctx, format, self.choice, CameraMode::Fly, self.cull), None)
        };
        #[cfg(not(feature = "egui-ui"))]
        let mut scene = build_scene(&self.ctx, format, self.choice, CameraMode::Fly, self.cull);
        let depth = scene::create_depth(&self.ctx.device, scene.depth_format(), config.width, config.height);
        log::info!(
            "surface: {}x{} {:?} present={:?}",
            config.width,
            config.height,
            format,
            config.present_mode
        );
        println!(
            "fly camera: WASD move | E|R up, Q|F down | RIGHT-drag or ` (backquote) = look | scroll = speed | Esc releases\n\
             \x20 interact: LEFT click = pick glyph | h highlight line | g grab file \
             (mouse drags, scroll scales) | t cycle tint | x hide/show"
        );
        #[cfg(feature = "egui-ui")]
        if self.ui {
            println!(
                "ui shell: F1 toggle | Cmd/Ctrl-K palette | panels: files · inspector · console | \
                 verbs via palette buttons or the console input (help lists the grammar)"
            );
        }
        println!("screenshot: F2 saves the next presented frame to out/windowed-shot-<timestamp>.png");

        // Stage G: scripted startup picks/verbs (same entry points the
        // windowed event handlers use).
        scene.set_viewport(config.width, config.height);
        for op in self.ops {
            let line = match op {
                Op::Pick(p) => scene.apply_pick(&self.ctx, p),
                Op::Verb(v) => scene.apply_verb(&self.ctx, v),
                Op::CamPose(eye, yaw, pitch) => {
                    scene.set_cam_pose(*eye, *yaw, *pitch);
                    None
                }
            };
            if let Some(line) = line {
                println!("{line}");
            }
        }

        self.state = Some(WindowState {
            window,
            surface,
            config,
            depth,
            scene,
            start: self.start,
            grabbed: false,
            saw_device_delta: false,
            cursor: (0.0, 0.0),
            modifiers: winit::keyboard::ModifiersState::empty(),
            last_frame: Instant::now(),
            frames_total: 0,
            shot_at_frame: self.shot.clone(),
            capture_pending: None,
            frames: 0,
            fps_window_start: Instant::now(),
            profile: crate::gpu::ProfileAccumulator::default(),
            #[cfg(feature = "egui-ui")]
            egui,
            #[cfg(feature = "egui-ui")]
            ui_probe,
            #[cfg(feature = "egui-ui")]
            ui_fps: 0.0,
            #[cfg(feature = "egui-ui")]
            k4_selftest: u8::from(std::env::var_os("GLYPH_K4_SELFTEST").is_some()),
        });
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else { return };
        // Stage K: egui sees every window event FIRST. Scene routing below only
        // handles events egui did not consume (0.36 semantics: pointer/wheel
        // consumed when egui wants pointer input; CursorMoved consumed while
        // egui is using the pointer; keys consumed when a widget has focus,
        // and Tab always). CloseRequested/Resized/RedrawRequested are never
        // consumed, so the lifecycle arms stay ungated — and are matched
        // before the consumption arm defensively anyway.
        #[cfg(feature = "egui-ui")]
        let egui_consumed = match state.egui.as_mut() {
            Some(egui) => egui.state.on_window_event(&state.window, &event).consumed,
            None => false,
        };
        #[cfg(not(feature = "egui-ui"))]
        let egui_consumed = false;
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            // Tracked BEFORE the egui-consumed arm: egui_winit also reads
            // ModifiersChanged, but our tracker must never miss an update.
            WindowEvent::ModifiersChanged(m) => state.modifiers = m.state(),
            WindowEvent::Resized(size) => {
                state.resize(&self.ctx, size.width, size.height);
                state.scene.set_viewport(size.width, size.height);
            }
            WindowEvent::RedrawRequested => state.render(&self.ctx),
            // Stage K (K6)/Stage M: F2 = capture the next presented frame to
            // PNG (the console `shot` command lands here too). App-level
            // hotkey that must work REGARDLESS of egui focus (e.g. while
            // typing in the console input), so it matches ABOVE the
            // egui-consumed arm — unlike F1, which deliberately yields to a
            // focused field. egui still saw the event first (the feed at the
            // top); it ignores F2.
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && matches!(event.physical_key, PhysicalKey::Code(KeyCode::F2)) =>
            {
                state.capture_pending = Some(windowed_shot_path());
            }
            // Stage M: Cmd/Ctrl-K toggles the palette — app-level like F2 so
            // it fires even from a focused text field (a chord never types).
            // Opening the palette also un-hides the shell: a palette over a
            // bare field is the web app's CommandBar posture.
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && matches!(event.physical_key, PhysicalKey::Code(KeyCode::KeyK))
                    && (state.modifiers.super_key() || state.modifiers.control_key()) =>
            {
                if let Some(egui) = state.egui.as_mut() {
                    egui.ui_state.palette_open = !egui.ui_state.palette_open;
                    if egui.ui_state.palette_open {
                        egui.ui_state.shell_open = true;
                    } else {
                        egui.ui_state.palette_query.clear();
                        egui.ui_state.palette_selected = 0;
                    }
                }
            }
            // Stage K (K2): right-RELEASE always ungrabs, even when egui
            // consumed the event (e.g. the release landed on a panel).
            // Otherwise a grab started outside a panel would latch on
            // forever (the K1 ordering swallowed consumed releases). ungrab()
            // is a no-op when not grabbed, so panel releases stay harmless.
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Right, .. } => {
                state.ungrab();
            }
            // Consumed by egui (e.g. click on a panel, typing in a focused
            // widget): never routed to the scene.
            _ if egui_consumed => {}
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    let pressed = event.state == ElementState::Pressed;
                    // Stage M: F1 toggles the whole interface shell (the K3
                    // Debug window's content lives on inside the panels).
                    // Never scene input. (With a text field focused, egui
                    // consumes F1 first; release focus with Esc, then F1.)
                    #[cfg(feature = "egui-ui")]
                    if code == KeyCode::F1 && pressed {
                        if let Some(egui) = state.egui.as_mut() {
                            egui.ui_state.shell_open = !egui.ui_state.shell_open;
                            if !egui.ui_state.shell_open {
                                egui.ui_state.palette_open = false;
                            }
                        }
                        return;
                    }
                    // Esc releases the pointer grab (and is not camera input).
                    if code == KeyCode::Escape && pressed {
                        state.ungrab();
                    } else if code == KeyCode::Backquote && pressed {
                        // Backquote toggles mouse-look grab — an always-available
                        // alternative to holding the right button (trackpads,
                        // mice without a usable right button, accessibility).
                        if state.grabbed {
                            state.ungrab();
                        } else {
                            state.grab();
                        }
                    } else {
                        state.scene.on_key(&self.ctx, code, pressed);
                    }
                }
            }
            // Stage G: LEFT click picks (only while the pointer is NOT
            // grabbed); RIGHT press grabs for mouse-look. (The release arm
            // lives above the egui-consumed arm — see the K2 note there.)
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Right, .. } => {
                state.grab();
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                if !state.grabbed {
                    let (x, y) = state.cursor;
                    state.scene.on_click(&self.ctx, x, y);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let (px, py) = (position.x as f32, position.y as f32);
                // Stage K (K2): cursor POSITION bookkeeping always updates
                // (click picks read it), but scene cursor effects route only
                // while egui doesn't want the pointer — hovering an egui
                // window included (EventResponse.consumed for CursorMoved
                // covers only mid-widget-drag, not hover). This stops a
                // g-grabbed group from tracking the pointer across panels.
                let route_to_scene = !state.egui_wants_pointer();
                // Fallback look path: while grabbed, if no raw DeviceEvent
                // deltas have ever arrived this grab (dead device-delta
                // environments), drive look from cursor movement instead.
                // Mid-look-drag egui wants nothing (drag started outside
                // it), so this still fires while grabbed over a panel.
                if route_to_scene && state.grabbed && !state.saw_device_delta {
                    let (dx, dy) = (px - state.cursor.0, py - state.cursor.1);
                    if dx != 0.0 || dy != 0.0 {
                        state.scene.on_mouse_look(&self.ctx, dx, dy);
                    }
                }
                state.cursor = (px, py);
                if route_to_scene {
                    state.scene.on_cursor(&self.ctx, px, py);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    // Pixel deltas (macOS trackpads): ~53 px per notch.
                    MouseScrollDelta::PixelDelta(p) => (p.y / 53.0) as f32,
                };
                state.scene.on_scroll(&self.ctx, lines);
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: winit::event::DeviceId, event: DeviceEvent) {
        // Raw mouse deltas drive look while grabbed (unaffected by the
        // confined pointer hitting the window edge).
        if let DeviceEvent::MouseMotion { delta: (dx, dy) } = event {
            if let Some(state) = self.state.as_mut() {
                if state.grabbed {
                    state.saw_device_delta = true;
                    state.scene.on_mouse_look(&self.ctx, dx as f32, dy as f32);
                }
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(state) = &mut self.state {
            // Fixed-cap dt so a stalled frame (resize, HUD hiccup) doesn't
            // launch the camera.
            let dt = state.last_frame.elapsed().as_secs_f32().min(0.1);
            state.last_frame = Instant::now();
            state.scene.tick(dt);
            state.window.request_redraw(); // continuous render loop
        }
    }
}

/// Stage M: the F2 / console `shot` destination — repo-root out/ (the K6
/// anchoring rule: CARGO_MANIFEST_DIR is native/, and running from native/
/// used to scatter shots into native/out/), utc-stamped.
fn windowed_shot_path() -> std::path::PathBuf {
    std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../out")).join(format!(
        "windowed-shot-{}.png",
        utc_stamp(std::time::SystemTime::now())
    ))
}

/// Stage K (K6): `yyyymmdd-hhmmss` UTC stamp for shot filenames (no chrono
/// dep; Howard Hinnant's civil-from-days algorithm).
fn utc_stamp(now: std::time::SystemTime) -> String {
    let secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before the unix epoch")
        .as_secs();
    let days = (secs / 86400) as i64;
    let tod = secs % 86400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    // Canonical Hinnant: (5*doy + 2)/153. (An earlier edit used the +456
    // variant's constant with the canonical d/m formulas — the variants are
    // not mixable; produced e.g. month=12 day=89 for 2026-09-02.)
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", tod / 3600, tod % 3600 / 60, tod % 60)
}

#[cfg(test)]
mod tests {
    use super::utc_stamp;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn utc_stamp_known_epochs() {
        // Values pinned against `date -u` (macOS): epoch 0, and the K6 fix
        // date 2026-09-02 18:21:12 UTC (the bad stamp that exposed the bug
        // read "202612-89-182112").
        let at = |s: u64| utc_stamp(UNIX_EPOCH + Duration::from_secs(s));
        assert_eq!(at(0), "19700101-000000");
        assert_eq!(at(1788373272), "20260902-182112");
        assert_eq!(at(951782400), "20000229-000000"); // leap day, era boundary math
        assert_eq!(at(4102444800), "21000101-000000"); // non-leap century year
    }
}

pub fn run(
    ctx: GpuContext,
    choice: &SceneChoice,
    cull: bool,
    ops: &[Op],
    ui: bool,
    shot: Option<(u64, std::path::PathBuf)>,
) {
    // Without the `egui-ui` feature the overlay is compiled out entirely;
    // the flag is accepted (and ignored) so the CLI is identical either way.
    #[cfg(not(feature = "egui-ui"))]
    let _ = ui;
    let event_loop = EventLoop::new().expect("event loop creation failed");
    let mut app = App {
        ctx,
        choice,
        cull,
        ops,
        start: Instant::now(),
        state: None,
        #[cfg(feature = "egui-ui")]
        ui,
        shot,
    };
    event_loop.run_app(&mut app).expect("event loop error");
}
