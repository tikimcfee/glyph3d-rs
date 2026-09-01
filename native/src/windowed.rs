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
    last_frame: Instant,
    // FPS accounting
    frames: u32,
    fps_window_start: Instant,
    /// Stage H: GPU pass timing accumulator for the once-per-second line
    /// (only fed when GLYPH_PROFILE=1 built a profiler).
    profile: crate::gpu::ProfileAccumulator,
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

    fn render(&mut self, ctx: &GpuContext) {
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
        // Stage H: resolve profiler queries before submit (see offscreen.rs).
        if let Some(p) = &ctx.profiler {
            p.borrow_mut().resolve_queries(&mut encoder);
        }
        ctx.queue.submit([encoder.finish()]);
        // wgpu 30: presentation goes through the queue, not the texture.
        ctx.queue.present(frame);

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
            self.frames = 0;
            self.fps_window_start = Instant::now();
            self.profile = crate::gpu::ProfileAccumulator::default();
        }
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
                        .with_title("glyph3d-native — Stage F (fly camera + GPU cull/LOD)")
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
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
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

        // Stage F: windowed glyph scenes get the fly camera (the Stage A demo
        // scene keeps its internal orbit; it ignores camera_mode).
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
            last_frame: Instant::now(),
            frames: 0,
            fps_window_start: Instant::now(),
            profile: crate::gpu::ProfileAccumulator::default(),
        });
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else { return };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                state.resize(&self.ctx, size.width, size.height);
                state.scene.set_viewport(size.width, size.height);
            }
            WindowEvent::RedrawRequested => state.render(&self.ctx),
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    let pressed = event.state == ElementState::Pressed;
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
            // grabbed); RIGHT press grabs for mouse-look, release ungrabs.
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Right, .. } => {
                state.grab();
            }
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Right, .. } => {
                state.ungrab();
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                if !state.grabbed {
                    let (x, y) = state.cursor;
                    state.scene.on_click(&self.ctx, x, y);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let (px, py) = (position.x as f32, position.y as f32);
                // Fallback look path: while grabbed, if no raw DeviceEvent
                // deltas have ever arrived this grab (dead device-delta
                // environments), drive look from cursor movement instead.
                if state.grabbed && !state.saw_device_delta {
                    let (dx, dy) = (px - state.cursor.0, py - state.cursor.1);
                    if dx != 0.0 || dy != 0.0 {
                        state.scene.on_mouse_look(&self.ctx, dx, dy);
                    }
                }
                state.cursor = (px, py);
                state.scene.on_cursor(&self.ctx, px, py);
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

pub fn run(ctx: GpuContext, choice: &SceneChoice, cull: bool, ops: &[Op]) {
    let event_loop = EventLoop::new().expect("event loop creation failed");
    let mut app = App {
        ctx,
        choice,
        cull,
        ops,
        start: Instant::now(),
        state: None,
    };
    event_loop.run_app(&mut app).expect("event loop error");
}
