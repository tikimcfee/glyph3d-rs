//! The winit application handler: window creation, event routing (egui
//! first, scene second), the relayout arm and the redraw driver. Extracted
//! from `windowed.rs` in the 2026-09 code-shape refactor — a pure move;
//! `pub(super)` stands in for the same-module privacy these items had.

use std::sync::Arc;
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::glyph_scene::CameraMode;
use crate::gpu::GpuContext;
#[cfg(feature = "egui-ui")]
use crate::relayout::{rebuild_for_relayout, RelayoutRequest};
use crate::scene::{self, SceneKey};
use crate::{Op, SceneChoice};

use super::state::WindowState;
#[cfg(feature = "egui-ui")]
use super::ui::EguiUi;
use super::utc_stamp;

/// The winit boundary for keys: the one translation from a winit physical
/// key to the engine's [`SceneKey`]. A key the scene does not act on is
/// None and never reaches it.
fn scene_key(code: KeyCode) -> Option<SceneKey> {
    Some(match code {
        KeyCode::KeyW => SceneKey::W,
        KeyCode::KeyA => SceneKey::A,
        KeyCode::KeyS => SceneKey::S,
        KeyCode::KeyD => SceneKey::D,
        KeyCode::KeyE => SceneKey::E,
        KeyCode::KeyR => SceneKey::R,
        KeyCode::KeyQ => SceneKey::Q,
        KeyCode::KeyF => SceneKey::F,
        KeyCode::KeyH => SceneKey::H,
        KeyCode::KeyT => SceneKey::T,
        KeyCode::KeyX => SceneKey::X,
        KeyCode::KeyB => SceneKey::B,
        KeyCode::KeyG => SceneKey::G,
        KeyCode::KeyC => SceneKey::C,
        KeyCode::KeyV => SceneKey::V,
        KeyCode::KeyN => SceneKey::N,
        KeyCode::KeyP => SceneKey::P,
        KeyCode::BracketLeft => SceneKey::BracketLeft,
        KeyCode::BracketRight => SceneKey::BracketRight,
        KeyCode::ArrowLeft => SceneKey::ArrowLeft,
        KeyCode::ArrowRight => SceneKey::ArrowRight,
        KeyCode::Comma => SceneKey::Comma,
        KeyCode::Period => SceneKey::Period,
        KeyCode::Home => SceneKey::Home,
        KeyCode::End => SceneKey::End,
        _ => return None,
    })
}

/// Where one keyboard event goes: the KeyboardInput arm's decision, pure so
/// it is tested (2026-10-24). F2-F6 and F8 are matched before this, on press
/// only, regardless of focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyRoute {
    /// Nowhere: a focused egui widget had the press, or a Cmd/Ctrl chord.
    Drop,
    /// `SceneLike::on_key` (through [`scene_key`]), pressed or released.
    Scene,
    /// F1: toggle the Debug window.
    DebugPanel,
    /// F7: toggle the Agent Sessions browser.
    SessionsPanel,
    /// Esc: release the pointer grab.
    Ungrab,
    /// Backquote: toggle the mouse-look grab.
    ToggleGrab,
}

/// Route a key. `egui_consumed` is what egui-winit said of the event (it
/// judges by LAST frame's focus); `chord` is Cmd or Ctrl held.
///
/// - Every RELEASE reaches the scene, consumed or not. Until 2026-10-24 a
///   consumed release was dropped, so W held while a text field took focus
///   latched the fly camera moving until W was pressed again. Safe because
///   the scene's verb keys fire on press only (`GlyphScene::on_key`); a
///   release only clears a fly key's bit.
/// - A consumed PRESS never reaches anything: typing WASD/h/g/t/x in a
///   field must not fly the camera or fire a verb.
/// - A press with Cmd or Ctrl held never reaches the scene (the camera
///   ignores modifiers, so Cmd-S flew backwards); the app keys still apply.
fn route_key(egui_consumed: bool, pressed: bool, chord: bool, code: KeyCode) -> KeyRoute {
    if !pressed {
        return KeyRoute::Scene;
    }
    if egui_consumed {
        return KeyRoute::Drop;
    }
    match code {
        KeyCode::F1 => KeyRoute::DebugPanel,
        KeyCode::F7 => KeyRoute::SessionsPanel,
        KeyCode::Escape => KeyRoute::Ungrab,
        KeyCode::Backquote => KeyRoute::ToggleGrab,
        _ if chord => KeyRoute::Drop,
        _ => KeyRoute::Scene,
    }
}

/// The relayout arm: the engine rebuilds the scene for the request
/// ([`rebuild_for_relayout`]) and the window swaps in what it built.
/// A free function, not an App method: the caller already holds
/// `state: &mut WindowState` borrowed from `self.state`, so `&mut self`
/// would double-borrow.
#[cfg(feature = "egui-ui")]
fn relayout(
    ctx: &GpuContext,
    choice: &mut SceneChoice,
    cull_opts: &mut crate::SceneCullOptions,
    ui: bool,
    state: &mut WindowState,
    req: RelayoutRequest,
) {
    let viewport = (state.config.width, state.config.height);
    let live = state.ui_probe.as_ref();
    if let Some((scene, probe)) = rebuild_for_relayout(&crate::SceneLoad::of(ctx), viewport, choice, cull_opts, live, ui, req) {
        // A new scene, a new draw of it.
        state.renderer = scene.make_renderer(ctx, state.config.format);
        state.scene = scene;
        state.ui_probe = probe;
    }
}

pub(super) struct App<'a> {
    pub(super) ctx: GpuContext,
    /// Owned, not borrowed: the relayout arm mutates the choice's params in
    /// place (Repo's z_wrap_spacing, Repo/Text cluster_mode) before rebuilding
    /// the scene.
    pub(super) choice: SceneChoice,
    pub(super) cull_opts: crate::SceneCullOptions,
    /// Stage G: scripted picks/verbs applied once at startup (smoke testing
    /// the same code path the windowed verbs use).
    pub(super) ops: &'a [Op],
    pub(super) live: Option<super::LiveSource>,
    pub(super) live_step_ix: usize,
    pub(super) start: Instant,
    pub(super) state: Option<WindowState>,
    /// Stage K: build the egui overlay (false under `--no-ui`).
    #[cfg(feature = "egui-ui")]
    pub(super) ui: bool,
    /// Stage K (K6): scripted in-window capture (`--screenshot-frame N`
    /// `--screenshot-out PATH`).
    pub(super) shot: Option<(u64, std::path::PathBuf)>,
    /// `--present-mode`. Fifo is vsync and caps the FPS line at the display's
    /// refresh (75 on the first Linux box, 2026-09-07 — a number that says
    /// nothing about the renderer). Applied only if the surface offers it.
    pub(super) present_mode: wgpu::PresentMode,
    /// Stop after N presented frames.
    pub(super) frames: Option<u32>,
}

impl App<'_> {
    fn live_step_title(&mut self, cmd: super::LiveStep) -> Option<String> {
        let live = self.live.as_ref()?;
        if let Some(step) = &live.step {
            let _ = step.send(cmd);
        }
        let count = live.step_count;
        self.live_step_ix = match cmd {
            super::LiveStep::Next => {
                if count > 0 { (self.live_step_ix + 1).min(count - 1) } else { 0 }
            }
            super::LiveStep::Prev => self.live_step_ix.saturating_sub(1),
            super::LiveStep::Reset => 0,
        };
        Some(if count > 0 {
            format!("fieldzed live — step {}/{}", self.live_step_ix + 1, count)
        } else {
            "fieldzed live".to_string()
        })
    }
}

fn handle_panic_or_broken_pipe(action: &str, err: Box<dyn std::any::Any + Send>) -> ! {
    let is_broken_pipe = err
        .downcast_ref::<String>()
        .is_some_and(|s| s.contains("Broken pipe") || s.contains("os error 32"))
        || err
            .downcast_ref::<&str>()
            .is_some_and(|s| s.contains("Broken pipe") || s.contains("os error 32"));
    if is_broken_pipe {
        std::process::exit(0);
    }
    eprintln!("Panic during {action}: {:?}", err);
    std::process::exit(1);
}

impl ApplicationHandler for App<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let Err(err) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.resumed_impl(event_loop))) {
            handle_panic_or_broken_pipe("window resume / startup", err);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if let Err(err) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.window_event_impl(event_loop, id, event))) {
            handle_panic_or_broken_pipe("window event handling", err);
        }
    }

    fn device_event(&mut self, event_loop: &ActiveEventLoop, id: winit::event::DeviceId, event: DeviceEvent) {
        if let Err(err) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.device_event_impl(event_loop, id, event))) {
            handle_panic_or_broken_pipe("window device_event", err);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Err(err) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.about_to_wait_impl(event_loop))) {
            handle_panic_or_broken_pipe("window about_to_wait", err);
        }
    }
}

impl App<'_> {
    fn resumed_impl(&mut self, event_loop: &ActiveEventLoop) {
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
        let present_mode = if caps.present_modes.contains(&self.present_mode) {
            self.present_mode
        } else {
            log::warn!(
                "present mode {:?} not offered by this surface (offers {:?}); using Fifo",
                self.present_mode,
                caps.present_modes
            );
            wgpu::PresentMode::Fifo
        };
        let config = wgpu::SurfaceConfiguration {
            // Stage K (K6): COPY_SRC = in-window screenshot readback.
            // Stage L (L3): COPY_DST = the composite's copy path when the
            // surface format happens to match the pool (non-Metal adapters);
            // windowed normally composites through the shader (BGRA).
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST,
            format,
            // Auto == sRGB for 8-bit formats; required field in wgpu 30.
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width,
            height: size.height,
            present_mode,
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
                scratch: String::new(),
                debug_open: true,
                filter: String::new(),
                selected_group: None,
                session_browser_open: false,
                session_filter_text: String::new(),
                session_filter_harness: None,
                discovered_sessions: None,
                session_dirs_report: Ok(String::new()),
                session_load_error: None,
                hud_open: false,
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
            crate::build_scene_probed(&crate::SceneLoad::of(&self.ctx), &self.choice, CameraMode::Fly, self.cull_opts)
        } else {
            (crate::build_scene_with_options(&crate::SceneLoad::of(&self.ctx), &self.choice, CameraMode::Fly, self.cull_opts), None)
        };
        #[cfg(not(feature = "egui-ui"))]
        let mut scene = crate::build_scene_with_options(&crate::SceneLoad::of(&self.ctx), &self.choice, CameraMode::Fly, self.cull_opts);
        // The options follow the mode the scene BUILT, as the relayout arm
        // does (a lane-limit fallback built Instanced until 2026-10-31).
        #[cfg(feature = "egui-ui")]
        if let Some(built) = ui_probe.as_ref().and_then(|p| p.borrow().field_mode) {
            self.cull_opts.field_mode = built;
        }
        let renderer = scene.make_renderer(&self.ctx, format);
        let depth = scene::create_depth(&self.ctx.device, renderer.depth_format(), config.width, config.height);
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
            println!("debug panel: F1 toggles the egui Debug window (sliders tune the LOD threshold live)");
            println!("agent sessions: F7 toggles the Agent Sessions browser");
            println!("field HUD: F8 toggles the top-left field readout (mode, cull tiers, per-frame ms)");
        }
        println!("screenshot: F2 saves the next presented frame to out/windowed-shot-<timestamp>.png (GLYPH_POSE_PRINT=1 also prints its --cam-pose)");

        // Stage G: scripted startup picks/verbs (same entry points the
        // windowed event handlers use).
        scene.set_viewport(config.width, config.height);
        for op in self.ops {
            if let Some(line) = crate::cli::apply_op(scene.as_mut(), op) {
                println!("{line}");
            }
        }

        self.state = Some(WindowState {
            renderer,
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
            modifiers: winit::keyboard::ModifiersState::empty(),
            frames_total: 0,
            shot_at_frame: self.shot.clone(),
            capture_pending: None,
            occluded: false,
            occluded_retry_at: Instant::now(),
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
            #[cfg(feature = "egui-ui")]
            pending_relayout: None,
            #[cfg(feature = "egui-ui")]
            zspace_selftest: u8::from(std::env::var_os("GLYPH_ZSPACE_SELFTEST").is_some()),
            #[cfg(feature = "egui-ui")]
            cluster_selftest: u8::from(std::env::var_os("GLYPH_CLUSTER_SELFTEST").is_some()),
            #[cfg(feature = "egui-ui")]
            #[cfg(feature = "egui-ui")]
            grab_selftest: u8::from(std::env::var_os("GLYPH_GRAB_SELFTEST").is_some()),
        });
    }

    fn window_event_impl(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
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
            WindowEvent::Resized(size) => {
                // Post-L3 fix: a resize means the window is being shown —
                // clear occlusion so the next about_to_wait resumes the
                // continuous redraw immediately.
                state.occluded = false;
                state.resize(&self.ctx, size.width, size.height);
                state.scene.set_viewport(size.width, size.height);
            }
            WindowEvent::RedrawRequested => {
                // The layout controls apply BETWEEN frames: the panel set
                // pending_relayout on slider release / toggle click during
                // previous render or keypress; rebuild before render if pending.
                #[cfg(feature = "egui-ui")]
                {
                    if let Some(opts) = state.scene.take_pending_carrel_options() {
                        state.pending_relayout = Some(RelayoutRequest {
                            carrel_options: Some(opts),
                            ..Default::default()
                        });
                    }
                    if let Some(req) = state.pending_relayout.take() {
                        relayout(&self.ctx, &mut self.choice, &mut self.cull_opts, self.ui, state, req);
                    }
                }
                state.render(&self.ctx);
                if let Some(max_frames) = self.frames {
                    if state.frames_total >= max_frames as u64 {
                        event_loop.exit();
                        return;
                    }
                }
                #[cfg(feature = "egui-ui")]
                {
                    if let Some(opts) = state.scene.take_pending_carrel_options() {
                        state.pending_relayout = Some(RelayoutRequest {
                            carrel_options: Some(opts),
                            ..Default::default()
                        });
                    }
                    if let Some(req) = state.pending_relayout.take() {
                        relayout(&self.ctx, &mut self.choice, &mut self.cull_opts, self.ui, state, req);
                        state.window.request_redraw();
                    }
                }
                if self.live.is_some() {
                    #[cfg(feature = "egui-ui")]
                    let ui = self.ui;
                    #[cfg(not(feature = "egui-ui"))]
                    let ui = false;
                    poll_live(&self.ctx, self.cull_opts, ui, &mut self.live, state);
                }
            }
            // Stage K (K6): F2 = capture the next presented frame to PNG.
            // App-level hotkey that must work REGARDLESS of egui focus (e.g.
            // while typing in the scratch field), so it matches ABOVE the
            // egui-consumed arm — unlike F1, which deliberately yields to a
            // focused field. egui still saw the event first (the feed at the
            // top); it ignores F2.
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && matches!(
                        event.physical_key,
                        PhysicalKey::Code(KeyCode::F2)
                            | PhysicalKey::Code(KeyCode::F3)
                            | PhysicalKey::Code(KeyCode::F4)
                            | PhysicalKey::Code(KeyCode::F5)
                            | PhysicalKey::Code(KeyCode::F6)
                            | PhysicalKey::Code(KeyCode::F8)
                    ) =>
            {
                match event.physical_key {
                    PhysicalKey::Code(KeyCode::F2) => {
                        state.capture_pending = Some(std::path::PathBuf::from(concat!(
                            env!("CARGO_MANIFEST_DIR"),
                            "/../out"
                        )).join(format!(
                            "windowed-shot-{}.png",
                            utc_stamp(std::time::SystemTime::now())
                        )));
                        // GLYPH_POSE_PRINT=1: the shot's camera as the
                        // `--cam-pose` argument that reproduces it (degrees),
                        // so a view seen once can be a golden view's command.
                        if std::env::var_os("GLYPH_POSE_PRINT").is_some() {
                            match state.scene.cam_pose() {
                                Some((eye, yaw, pitch)) => println!("{}", super::state::cam_pose_arg(eye, yaw, pitch)),
                                None => println!("pose: this scene has no fly camera"),
                            }
                        }
                    }
                    PhysicalKey::Code(KeyCode::F8) => {
                        #[cfg(feature = "egui-ui")]
                        if let Some(egui) = state.egui.as_mut() {
                            egui.hud_open = !egui.hud_open;
                        }
                    }
                    PhysicalKey::Code(KeyCode::F3) => {
                        let window = state.window.clone();
                        if let Some(t) = self.live_step_title(super::LiveStep::Next) {
                            window.set_title(&t);
                        }
                    }
                    PhysicalKey::Code(KeyCode::F4) => {
                        let window = state.window.clone();
                        if let Some(t) = self.live_step_title(super::LiveStep::Prev) {
                            window.set_title(&t);
                        }
                    }
                    PhysicalKey::Code(KeyCode::F5) => {
                        let window = state.window.clone();
                        if let Some(t) = self.live_step_title(super::LiveStep::Reset) {
                            window.set_title(&t);
                        }
                    }
                    PhysicalKey::Code(KeyCode::F6) => {
                        #[cfg(feature = "egui-ui")]
                        {
                            state.pending_relayout = Some(RelayoutRequest {
                                toggle_layout: true,
                                ..Default::default()
                            });
                            state.window.request_redraw();
                        }
                    }
                    _ => {}
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
            // The modifier state, for route_key's chord test. Tracked above
            // the consumed arm so a chord pressed over a panel still counts.
            WindowEvent::ModifiersChanged(modifiers) => {
                state.modifiers = modifiers.state();
            }
            // Keys route through one decision (route_key), above the
            // consumed arm: a RELEASE reaches the scene even when egui
            // consumed it (2026-10-24), so a fly key held while a text
            // field takes focus cannot latch the camera moving.
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    let pressed = event.state == ElementState::Pressed;
                    let chord = state.modifiers.super_key() || state.modifiers.control_key();
                    match route_key(egui_consumed, pressed, chord, code) {
                        KeyRoute::Drop => {}
                        // Stage K (K4): F1 toggles the Debug window — never
                        // scene input. (With a text field focused, egui
                        // consumes F1 first; release focus with Esc, then F1.)
                        KeyRoute::DebugPanel => {
                            #[cfg(feature = "egui-ui")]
                            if let Some(egui) = state.egui.as_mut() {
                                egui.debug_open = !egui.debug_open;
                            }
                        }
                        KeyRoute::SessionsPanel => {
                            #[cfg(feature = "egui-ui")]
                            if let Some(egui) = state.egui.as_mut() {
                                egui.session_browser_open = !egui.session_browser_open;
                            }
                        }
                        // Esc releases the pointer grab (and is not camera input).
                        KeyRoute::Ungrab => state.ungrab(),
                        // Backquote toggles mouse-look grab — an always-available
                        // alternative to holding the right button (trackpads,
                        // mice without a usable right button, accessibility).
                        KeyRoute::ToggleGrab => {
                            if state.grabbed {
                                state.ungrab();
                            } else {
                                state.grab();
                            }
                        }
                        KeyRoute::Scene => {
                            if let Some(key) = scene_key(code) {
                                state.scene.on_key(key, pressed);
                            }
                            state.window.request_redraw();
                        }
                    }
                }
            }
            // Consumed by egui (e.g. click on a panel, a focused widget):
            // never routed to the scene.
            _ if egui_consumed => {}
            // Stage G: LEFT click picks (only while the pointer is NOT
            // grabbed); RIGHT press grabs for mouse-look. (The release arm
            // lives above the egui-consumed arm — see the K2 note there.)
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Right, .. } => {
                state.grab();
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                if !state.grabbed {
                    let (x, y) = state.cursor;
                    state.scene.on_click(x, y);
                    state.window.request_redraw();
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
                        state.scene.on_mouse_look(dx, dy);
                    }
                }
                state.cursor = (px, py);
                if route_to_scene {
                    state.scene.on_cursor(px, py);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    // Pixel deltas (macOS trackpads): ~53 px per notch.
                    MouseScrollDelta::PixelDelta(p) => (p.y / 53.0) as f32,
                };
                state.scene.on_scroll(lines);
            }
            _ => {}
        }
    }

    fn device_event_impl(&mut self, _event_loop: &ActiveEventLoop, _id: winit::event::DeviceId, event: DeviceEvent) {
        // Raw mouse deltas drive look while grabbed (unaffected by the
        // confined pointer hitting the window edge).
        if let DeviceEvent::MouseMotion { delta: (dx, dy) } = event {
            if let Some(state) = self.state.as_mut() {
                if state.grabbed {
                    state.saw_device_delta = true;
                    state.scene.on_mouse_look(dx as f32, dy as f32);
                }
            }
        }
    }

    fn about_to_wait_impl(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(state) = &mut self.state {
            if let Some(max_frames) = self.frames {
                if state.frames_total >= max_frames as u64 {
                    event_loop.exit();
                    return;
                }
            }
            // Post-L3 fix: while the surface reports Occluded, do NOT spin
            // request_redraw (measured pre-fix: ~250k skipped acquires/s,
            // ~100% CPU). winit 0.30 gives no reliable wake when a fully
            // occluded macOS window becomes visible again
            // (WindowEvent::Occluded is iOS-only; RedrawRequested fires on
            // OS invalidation or an explicit request_redraw), so a purely
            // event-driven wait could strand the window blank — instead
            // retry at ~2 Hz via a WaitUntil deadline; recovery takes at
            // most half a second. Not occluded: restore the default Wait
            // (the continuous redraw below keeps the loop hot as today).
            if state.occluded {
                const RETRY: std::time::Duration = std::time::Duration::from_millis(500);
                let elapsed = state.occluded_retry_at.elapsed();
                if elapsed < RETRY {
                    event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + (RETRY - elapsed)));
                    return;
                }
                state.occluded_retry_at = Instant::now();
            } else {
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            // Fixed-cap dt so a stalled frame (resize, HUD hiccup) doesn't
            // launch the camera.
            let dt = state.last_frame.elapsed().as_secs_f32().min(0.1);
            state.last_frame = Instant::now();
            state.scene.tick(dt);
            state.scene.animate(dt);
            state.window.request_redraw(); // continuous render loop
        }
    }
}

fn poll_live(
    ctx: &GpuContext,
    cull_opts: crate::SceneCullOptions,
    ui: bool,
    live: &mut Option<super::LiveSource>,
    state: &mut WindowState,
) {
    // The live rebuild keeps the run's cull switch and field mode (until
    // 2026-10-10 it rebuilt as Instanced whatever the run was launched with).
    let (cull, field_mode) = (cull_opts.cull, cull_opts.field_mode);
    let Some(src) = live else { return };
    let mut arrived: Vec<crate::seam::SurfaceUpdate> = std::mem::take(&mut src.backlog);
    while let Ok(update) = src.rx.try_recv() {
        arrived.push(update);
    }
    if arrived.is_empty() {
        return;
    }
    for update in &arrived {
        let entry = src.content.entry(update.file.0.clone()).or_default();
        match &update.content {
            crate::seam::ContentDelta::Opened(bytes) => *entry = Arc::new(bytes.clone()),
            crate::seam::ContentDelta::Edited { .. } => {
                let owned = Arc::make_mut(entry);
                update.content.apply(owned);
            }
            crate::seam::ContentDelta::Tombstone => {
                src.content.remove(&update.file.0);
                src.last_style.remove(&update.file.0);
                continue;
            }
        }
        src.last_style.insert(update.file.0.clone(), update.clone());
    }

    // The live camera pose rides the UI probe, which exists only with the
    // egui overlay; without it a live rebuild keeps the scene's own pose.
    #[cfg(feature = "egui-ui")]
    let pose = state.ui_probe.as_ref().map(|p| {
        let p = p.borrow();
        (p.eye, p.yaw, p.pitch)
    });
    #[cfg(not(feature = "egui-ui"))]
    let pose: Option<([f32; 3], f32, f32)> = None;
    let t_all = Instant::now();
    let mut folds: std::collections::HashMap<String, Vec<std::ops::Range<u32>>> =
        std::collections::HashMap::new();
    for (rel, update) in &src.last_style {
        if update.structure.is_empty() {
            continue;
        }
        if let Some(bytes) = src.content.get(rel) {
            let rows_est = bytes
                .iter()
                .filter(|&&b| b == b'\n')
                .count()
                .max(bytes.len() / src.params.wrap_cols.max(1) as usize)
                .max(1);
            let paginated =
                src.params.page_rows > 0 && rows_est > src.params.page_rows as usize;
            if paginated {
                continue;
            }
            let starts = crate::repo::line_starts_of(bytes);
            let lines = crate::seam::normalized_fold_lines(&update.structure, &starts);
            if !lines.is_empty() {
                folds.insert(rel.clone(), lines);
            }
        }
    }
    let mut files: Vec<crate::repo::RepoFile> = src
        .content
        .iter()
        .map(|(rel, bytes)| crate::repo::RepoFile::in_memory(rel.clone(), bytes.as_ref().clone()))
        .collect();
    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    let load = crate::repo::load_items(
        crate::repo::WalkResult::from_files(files),
        std::time::Duration::ZERO,
        std::path::Path::new("."),
        &src.params,
        crate::repo::Strategy::Hyper,
        if folds.is_empty() { None } else { Some(&folds) },
    );
    let t_fold = t_all.elapsed();
    let t = Instant::now();
    let atlas = &src.atlas;
    let t_atlas = t.elapsed();
    let t = Instant::now();
    let mut staged = load.into_staged(None, &atlas.slot_ink);
    if let Some(pick) = &mut staged.pick {
        pick.content = Some(src.content.clone());
        pick.folds = folds.clone();
    }
    let t_stage = t.elapsed();
    let t = Instant::now();
    let (mut scene, probe) = if ui {
        crate::build_scene_from_staged_probed(
            src.atlas.clone(),
            staged,
            CameraMode::Fly,
            cull,
            field_mode,
        )
    } else {
        (
            crate::build_scene_from_staged_mode(
                src.atlas.clone(),
                staged,
                CameraMode::Fly,
                cull,
                field_mode,
            ),
            None,
        )
    };
    scene.set_viewport(state.config.width, state.config.height);
    if let Some((eye, yaw, pitch)) = pose {
        scene.set_cam_pose(eye, yaw, pitch);
    }
    let t_scene = t.elapsed();
    let t = Instant::now();
    for update in src.last_style.values() {
        if let Some(line) = scene.apply_surface_updates(std::slice::from_ref(update)) {
            log::debug!("{line}");
        }
    }
    let t_restyle = t.elapsed();
    println!(
        "live: rebuilt from {} file(s) in {:.1?} (fold {:.1?}, atlas {:.1?}, stage {:.1?}, scene {:.1?}, restyle {:.1?}) — {} update(s) applied",
        src.content.len(),
        t_all.elapsed(),
        t_fold,
        t_atlas,
        t_stage,
        t_scene,
        t_restyle,
        arrived.len(),
    );
    state.renderer = scene.make_renderer(ctx, state.config.format);
    state.scene = scene;
    #[cfg(feature = "egui-ui")]
    {
        state.ui_probe = probe;
    }
    #[cfg(not(feature = "egui-ui"))]
    let _ = probe;
}

#[cfg(test)]
mod key_tests {
    use super::{route_key, scene_key, KeyRoute};
    use winit::keyboard::KeyCode as K;

    /// Every key the scene acts on: the fly keys and the verb keys, each
    /// one `scene_key` translates.
    const SCENE_KEYS: &[K] = &[
        K::KeyW, K::KeyA, K::KeyS, K::KeyD, K::KeyE, K::KeyR, K::KeyQ, K::KeyF,
        K::KeyH, K::KeyG, K::KeyT, K::KeyX, K::KeyB, K::KeyC, K::KeyV, K::KeyN, K::KeyP,
        K::BracketLeft, K::BracketRight, K::ArrowLeft, K::ArrowRight, K::Comma, K::Period,
        K::Home, K::End,
    ];

    #[test]
    fn typing_in_a_field_never_reaches_the_scene() {
        for &code in SCENE_KEYS {
            assert!(scene_key(code).is_some(), "{code:?} is a scene key");
            assert_eq!(route_key(true, true, false, code), KeyRoute::Drop, "{code:?} typed into a field");
            assert_eq!(route_key(false, true, false, code), KeyRoute::Scene, "{code:?} over the scene");
        }
        // The app keys yield to a focused field too.
        for code in [K::F1, K::F7, K::Escape, K::Backquote] {
            assert_eq!(route_key(true, true, false, code), KeyRoute::Drop, "{code:?}");
        }
    }

    #[test]
    fn every_release_reaches_the_scene() {
        for &code in SCENE_KEYS {
            for (consumed, chord) in [(false, false), (true, false), (false, true), (true, true)] {
                assert_eq!(route_key(consumed, false, chord, code), KeyRoute::Scene, "{code:?} {consumed} {chord}");
            }
        }
    }

    #[test]
    fn a_chord_never_moves_the_scene() {
        for &code in SCENE_KEYS {
            assert_eq!(route_key(false, true, true, code), KeyRoute::Drop, "{code:?} with Cmd/Ctrl");
        }
        assert_eq!(route_key(false, true, true, K::F1), KeyRoute::DebugPanel);
        assert_eq!(route_key(false, true, false, K::F7), KeyRoute::SessionsPanel);
        assert_eq!(route_key(false, true, false, K::Escape), KeyRoute::Ungrab);
        assert_eq!(route_key(false, true, false, K::Backquote), KeyRoute::ToggleGrab);
    }
}
