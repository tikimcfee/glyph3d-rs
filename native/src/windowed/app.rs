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
use crate::scene;
use crate::{Op, SceneChoice};

use super::state::WindowState;
#[cfg(feature = "egui-ui")]
use super::ui::EguiUi;
use super::utc_stamp;

/// The relayout arm: mutate the scene choice's layout params, then swap the
/// scene in place — rebuilt through the same builder startup used, so it is
/// correct by construction (the one true layout path rebuilds everything the
/// params touch: cull AABBs, per-file pick params, bounds) at the cost of a
/// full reload per apply, which is why the panel fires on release/click, not
/// per tick. The camera pose survives via the old probe's last frame;
/// pick/selection/grab state resets with the scene (a fresh load has none —
/// same as the JS `grid.layout` refold). Repo scenes carry the z dial and
/// the cluster toggle; text scenes carry the toggle only; anything else has
/// no layout params and a stale signal is a no-op.
/// A free function, not an App method: the caller already holds
/// `state: &mut WindowState` borrowed from `self.state`, so `&mut self`
/// would double-borrow.
#[cfg(feature = "egui-ui")]
pub(super) fn apply_relayout(
    ctx: &GpuContext,
    choice: &mut SceneChoice,
    cull_opts: &mut crate::SceneCullOptions,
    ui: bool,
    state: &mut WindowState,
    req: RelayoutRequest,
) {
    let mut changed = false;
    let mut note = String::new();
    let reset_pose = req.switch_agent_session.is_some();
    if let Some(session_path) = req.switch_agent_session {
        let emoji_sheet = match choice {
            SceneChoice::AgentSession { emoji_sheet, .. } => emoji_sheet.clone(),
            SceneChoice::Text { emoji_sheet, .. } => emoji_sheet.clone(),
            SceneChoice::EngineText { emoji_sheet, .. } => emoji_sheet.clone(),
            SceneChoice::Repo { emoji_sheet, .. } => emoji_sheet.clone(),
            SceneChoice::Demo => crate::default_emoji_sheet(),
        };
        *choice = SceneChoice::AgentSession {
            session_path: session_path.clone(),
            emoji_sheet,
            layout_options: crate::spatial_scene::CarrelLayoutOptions::default(),
            cached_session: std::sync::Arc::new(std::sync::RwLock::new(None)),
        };
        changed = true;
        note += &format!("switch_agent_session -> {} ", session_path.display());
    }
    if let SceneChoice::AgentSession { layout_options, .. } = choice {
        if let Some(new_opts) = req.carrel_options {
            if *layout_options != new_opts {
                *layout_options = new_opts;
                changed = true;
                note += &format!("carrel_options -> limit: {}, scroll: {} ", new_opts.deck_window_limit, new_opts.deck_scroll_offset);
            }
        }
    }
    // The z dial exists on repo scenes only.
    if let SceneChoice::Repo { z_wrap_spacing, .. } = choice {
        if let Some(new_z) = req.z_wrap_spacing {
            if z_wrap_spacing.to_bits() != new_z.to_bits() {
                *z_wrap_spacing = new_z;
                changed = true;
                note += &format!("z_wrap_spacing -> {new_z} ");
            }
        }
    }
    // Layout engine strategy (repo scenes only).
    if let Some(new_strategy) = req.set_strategy {
        if let SceneChoice::Repo { strategy, .. } = choice {
            if *strategy != new_strategy {
                *strategy = new_strategy;
                changed = true;
                note += &format!("strategy -> {:?} ", new_strategy);
            }
        }
    }
    // Glyph field render mode (all scenes).
    if let Some(new_mode) = req.set_field_mode {
        if cull_opts.field_mode != new_mode {
            cull_opts.field_mode = new_mode;
            changed = true;
            note += &format!("field_mode -> {:?} ", new_mode);
        }
    }
    // The cluster toggle rides any scene whose choice carries the mode —
    // repo and text alike (SceneChoice::Text has carried it since the pass
    // landed; the panel seed for text scenes is GlyphScene's
    // probe_cluster_mode).
    if req.toggle_cluster {
        let mode = match choice {
            SceneChoice::Repo { cluster_mode, .. } | SceneChoice::Text { cluster_mode, .. } => {
                Some(cluster_mode)
            }
            _ => None,
        };
        if let Some(mode) = mode {
            *mode = match *mode {
                crate::fold::ClusterMode::Leader => crate::fold::ClusterMode::Cluster,
                crate::fold::ClusterMode::Cluster => crate::fold::ClusterMode::Leader,
            };
            changed = true;
            note += &format!("cluster_mode -> {mode:?} ");
        }
    }
    if req.toggle_layout {
        if let SceneChoice::Repo { layout_mode, .. } = choice {
            *layout_mode = match *layout_mode {
                crate::repo::RepoLayoutMode::Shelf => crate::repo::RepoLayoutMode::Carrel,
                crate::repo::RepoLayoutMode::Carrel => crate::repo::RepoLayoutMode::Shelf,
            };
            changed = true;
            note += &format!("layout_mode -> {:?} ", *layout_mode);
        }
    }
    if !changed {
        return; // a release without a move (a click, a typed repeat) rebuilds nothing
    }
    // Snapshot the camera and live cull settings BEFORE the swap: the probe holds
    // last frame's actual eye/yaw/pitch and live background/cull settings, and a rebuild
    // that teleports the viewer or resets the background would make the dial unusable.
    let (pose, live_cull_opts) = if let Some(p) = state.ui_probe.as_ref() {
        let p = p.borrow();
        (
            Some((p.eye, p.yaw, p.pitch)),
            crate::SceneCullOptions {
                cull: cull_opts.cull,
                file_backgrounds: p.file_backgrounds,
                file_bg_color: p.file_bg_color,
                lod_min_px: Some(p.lod_min_px),
                greeking: p.greeking,
                greek_pure: p.greek_pure,
                greek_onset_px: Some(p.greek_onset_px),
                field_mode: cull_opts.field_mode,
            },
        )
    } else {
        (None, *cull_opts)
    };
    let t = Instant::now();
    let (mut scene, probe) = if ui {
        crate::build_scene_probed(ctx, state.config.format, choice, CameraMode::Fly, live_cull_opts)
    } else {
        (crate::build_scene_with_options(ctx, state.config.format, choice, CameraMode::Fly, live_cull_opts), None)
    };
    scene.set_viewport(state.config.width, state.config.height);
    if !reset_pose {
        if let Some((eye, yaw, pitch)) = pose {
            scene.set_cam_pose(eye, yaw, pitch);
        }
    }
    state.scene = scene;
    state.ui_probe = probe;
    println!(
        "relayout: {note}(scene rebuilt in {:?}; pick/selection/grab state reset)",
        t.elapsed()
    );
}

/// What the panel asked the rebuild arm to change. Either field alone may be
/// set; the arm applies both and rebuilds only if one actually moved.
#[cfg(feature = "egui-ui")]
#[derive(Clone, Default)]
pub(super) struct RelayoutRequest {
    pub(super) z_wrap_spacing: Option<f64>,
    pub(super) toggle_cluster: bool,
    pub(super) toggle_layout: bool,
    pub(super) switch_agent_session: Option<std::path::PathBuf>,
    pub(super) carrel_options: Option<crate::spatial_scene::CarrelLayoutOptions>,
    pub(super) set_strategy: Option<crate::repo::Strategy>,
    pub(super) set_field_mode: Option<glyph_field::GlyphFieldMode>,
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
                session_filter_harness: crate::agent_transcript::discovery::SessionHarnessFilter::All,
                discovered_sessions: None,
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
            crate::build_scene_probed(&self.ctx, format, &self.choice, CameraMode::Fly, self.cull_opts)
        } else {
            (crate::build_scene_with_options(&self.ctx, format, &self.choice, CameraMode::Fly, self.cull_opts), None)
        };
        #[cfg(not(feature = "egui-ui"))]
        let mut scene = crate::build_scene_with_options(&self.ctx, format, &self.choice, CameraMode::Fly, self.cull_opts);
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
            println!("debug panel: F1 toggles the egui Debug window (sliders tune LOD_MIN_PX live)");
            println!("agent sessions: F7 toggles the Agent Sessions browser");
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
                Op::Highlight(p) => scene.apply_highlight_sidecar(&self.ctx, p),
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
                        apply_relayout(&self.ctx, &mut self.choice, &mut self.cull_opts, self.ui, state, req);
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
                        apply_relayout(&self.ctx, &mut self.choice, &mut self.cull_opts, self.ui, state, req);
                        state.window.request_redraw();
                    }
                }
                if self.live.is_some() {
                    #[cfg(feature = "egui-ui")]
                    let ui = self.ui;
                    #[cfg(not(feature = "egui-ui"))]
                    let ui = false;
                    poll_live(&self.ctx, self.cull_opts.cull, ui, &mut self.live, state);
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
            // Consumed by egui (e.g. click on a panel, typing in a focused
            // widget): never routed to the scene.
            _ if egui_consumed => {}
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    let pressed = event.state == ElementState::Pressed;
                    // Stage K (K4): F1 toggles the Debug window — never scene
                    // input. (With the scratch field focused, egui consumes
                    // F1 first; release focus with Esc, then F1.)
                    #[cfg(feature = "egui-ui")]
                    if code == KeyCode::F1 && pressed {
                        if let Some(egui) = state.egui.as_mut() {
                            egui.debug_open = !egui.debug_open;
                        }
                        return;
                    }
                    #[cfg(feature = "egui-ui")]
                    if code == KeyCode::F7 && pressed {
                        if let Some(egui) = state.egui.as_mut() {
                            egui.session_browser_open = !egui.session_browser_open;
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
                        state.window.request_redraw();
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

    fn device_event_impl(&mut self, _event_loop: &ActiveEventLoop, _id: winit::event::DeviceId, event: DeviceEvent) {
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
            state.window.request_redraw(); // continuous render loop
        }
    }
}

fn poll_live(
    ctx: &GpuContext,
    cull: bool,
    ui: bool,
    live: &mut Option<super::LiveSource>,
    state: &mut WindowState,
) {
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

    let pose = state.ui_probe.as_ref().map(|p| {
        let p = p.borrow();
        (p.eye, p.yaw, p.pitch)
    });
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
        &src.trie,
        &src.params,
        crate::repo::Strategy::Hyper,
        false,
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
            ctx,
            state.config.format,
            atlas,
            staged,
            CameraMode::Fly,
            cull,
        )
    } else {
        (
            crate::build_scene_from_staged(
                ctx,
                state.config.format,
                atlas,
                staged,
                CameraMode::Fly,
                cull,
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
        if let Some(line) = scene.apply_surface_updates(ctx, std::slice::from_ref(update)) {
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
    state.scene = scene;
    #[cfg(feature = "egui-ui")]
    {
        state.ui_probe = probe;
    }
}
