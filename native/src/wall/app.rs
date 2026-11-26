//! The wall's window: winit in front of [`WallRenderer`].
//!
//! One window, an orthographic camera over a canvas of terminals, and the
//! keys of the focused one. It draws on demand: the event loop sleeps until
//! input arrives or a terminal's reader thread sends [`Wake`], and a frame
//! is drawn only then (`out/WALL-PLAN-2026-11-26.md`, milestone 1).
//!
//! What a terminal IS comes through [`Terminal`]: a screen that can be
//! brought up to date and given keys. `session.rs` implements it for a live
//! terminal in `g3d-termd`; the test pattern implements it for a fixed one.

use std::path::PathBuf;
use std::sync::Arc;
#[cfg(feature = "egui-ui")]
use std::time::Instant;

use glam::Vec2;
use term_cells::Screen;
use wall_core::input::KeyEvent;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
#[cfg(feature = "egui-ui")]
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use crate::atlas::Atlas;
use crate::gpu::GpuContext;

use super::camera::WallCamera;
use super::metrics::WorldRect;
use super::labels::{measure_label_width, Label, LabelHeight};
use super::paper::PaperStyle;
use super::shapes::{DashStyle, RectShape, Shape};
use super::renderer::{TerminalDraw, WallFrame, WallRenderer, WallStyle, WallTarget, WALL_DEPTH_FORMAT};
#[cfg(feature = "egui-ui")]
use super::ui::{DebugHudInput, HudTerminalInfo, UiEventOutcome, WallUi, show_debug_hud};

/// Physical pixels a wheel "line" is worth when a mouse reports lines.
const PX_PER_LINE: f32 = 53.0;
/// How fast the wheel zooms: the zoom multiplies by `exp(px * this)`.
const WHEEL_ZOOM_PER_PX: f32 = 0.0025;
/// World units between terminals placed side by side.
const GAP: f32 = 2.0;
/// A title bar's height on screen, in physical pixels, and the most world
/// height it takes when zoomed far out (the JS wall's TitleBar).
const TITLE_BAR_PX: f32 = 22.0;
const TITLE_BAR_MAX_WORLD: f32 = 2.0;

/// A terminal as the window sees it.
pub trait Terminal {
    /// The screen to draw.
    fn screen(&self) -> &Screen;
    /// Take in what arrived since the last call; true if the screen changed.
    fn pump(&mut self) -> bool {
        false
    }
    /// A key for the terminal's program.
    fn key(&mut self, _ev: &KeyEvent) {}
    /// Text pasted into the terminal.
    fn paste(&mut self, _text: &str) {}
    /// What the title bar would say.
    fn title(&self) -> &str {
        ""
    }
}

/// A terminal that never changes: the test pattern.
pub struct StaticTerminal(pub Screen);

impl Terminal for StaticTerminal {
    fn screen(&self) -> &Screen {
        &self.0
    }
    fn title(&self) -> &str {
        &self.0.title
    }
}

/// What a reader thread sends the event loop: something arrived, look.
#[derive(Clone, Copy, Debug)]
pub struct Wake;

/// How the window is opened.
pub struct WallOptions {
    pub emoji_sheet: PathBuf,
    /// Draw this many frames, one after another, then exit (`--frames`): how
    /// a script checks the window came up and drew.
    pub exit_after: Option<u64>,
    /// Builds the terminals once the window exists. It is handed a waker
    /// that reader threads call when a terminal has news.
    pub open: Box<dyn FnOnce(Arc<dyn Fn() + Send + Sync>) -> Opened>,
}

/// Starts one more terminal (the spawn chord).
pub type Spawn = Box<dyn FnMut() -> Result<Box<dyn Terminal>, String>>;

/// What opening the wall's terminals gives the window: the terminals that
/// exist, and a way to start another, if this source can.
pub struct Opened {
    pub terminals: Vec<Box<dyn Terminal>>,
    pub spawn: Option<Spawn>,
}

/// A terminal on the canvas.
struct Placed {
    id: u64,
    term: Box<dyn Terminal>,
    /// Top-left of cell (0, 0), world units, y up.
    position: Vec2,
    z: f32,
    /// 1.0 draws the terminal at the atlas's cell size.
    scale: f32,
    border_color: Option<u32>,
}

/// What exists once there is a window.
struct Gfx {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    depth: wgpu::TextureView,
    renderer: WallRenderer,
    #[cfg(feature = "egui-ui")]
    ui: WallUi,
}

struct App {
    ctx: GpuContext,
    opts: Option<WallOptions>,
    proxy: EventLoopProxy<Wake>,
    gfx: Option<Gfx>,
    camera: WallCamera,
    terminals: Vec<Placed>,
    focused: Option<u64>,
    modifiers: ModifiersState,
    /// The pointer, physical px from the window's top-left.
    cursor: Vec2,
    /// A drag on empty canvas is panning.
    panning: bool,
    frames: u64,
    exit_after: Option<u64>,
    spawn: Option<Spawn>,
    next_id: u64,
    #[cfg(feature = "egui-ui")]
    hud_open: bool,
    #[cfg(feature = "egui-ui")]
    ui_repaint_at: Option<Instant>,
}

impl App {
    fn viewport(&self) -> (u32, u32) {
        self.gfx.as_ref().map_or((1, 1), |g| (g.config.width, g.config.height))
    }

    fn redraw(&self) {
        if let Some(g) = &self.gfx {
            g.window.request_redraw();
        }
    }

    /// A terminal's panel in world space.
    fn rect_of(&self, t: &Placed) -> WorldRect {
        let g = self.gfx.as_ref().expect("a terminal is placed only once the renderer exists");
        let s = t.term.screen();
        g.renderer.metrics().terminal_panel_rect(t.position, s.cols, s.rows, g.renderer.style().panel_padding, t.scale)
    }

    /// The topmost terminal under a screen point.
    fn terminal_at(&self, screen: Vec2) -> Option<u64> {
        let world = self.camera.screen_to_world(screen, self.viewport());
        self.terminals
            .iter()
            .filter(|t| self.rect_of(t).contains(world))
            .max_by(|a, b| a.z.total_cmp(&b.z))
            .map(|t| t.id)
    }

    /// Put the camera on everything.
    fn frame_all(&mut self) {
        let Some(bounds) = self.terminals.iter().map(|t| self.rect_of(t)).reduce(|a, b| a.union(b)) else { return };
        self.camera = WallCamera::frame_rect(bounds, self.viewport(), 1.08);
    }

    /// Focus a terminal and raise it above the others.
    fn focus(&mut self, id: u64) {
        self.focused = Some(id);
        let top = self.terminals.iter().map(|t| t.z).fold(0.0_f32, f32::max);
        if let Some(t) = self.terminals.iter_mut().find(|t| t.id == id) {
            if t.z < top || top == 0.0 {
                t.z = top + 1.0;
            }
        }
    }

    fn open(&mut self, event_loop: &ActiveEventLoop) {
        let window = Arc::new(
            event_loop
                .create_window(Window::default_attributes().with_title("glyph3d — the wall").with_inner_size(winit::dpi::LogicalSize::new(1600.0, 1000.0)))
                .expect("window creation failed"),
        );
        let surface = self.ctx.instance.create_surface(window.clone()).expect("surface creation failed");
        let caps = surface.get_capabilities(&self.ctx.adapter);
        let format = caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&self.ctx.device, &config);
        let depth = crate::scene::create_depth(&self.ctx.device, WALL_DEPTH_FORMAT, config.width, config.height);

        let opts = self.opts.take().expect("the window opens once");
        self.exit_after = opts.exit_after;
        let atlas = Arc::new(Atlas::load(&opts.emoji_sheet));
        let renderer = WallRenderer::new(&self.ctx.device, &self.ctx.queue, atlas, format, WallStyle::default());
        #[cfg(feature = "egui-ui")]
        let ui = WallUi::new(&self.ctx.device, &window, format);
        let metrics = renderer.metrics();
        let padding = renderer.style().panel_padding;
        self.gfx = Some(Gfx {
            window,
            surface,
            config,
            depth,
            renderer,
            #[cfg(feature = "egui-ui")]
            ui,
        });

        let proxy = self.proxy.clone();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            // The loop may be gone at shutdown; nothing is left to wake.
            let _ = proxy.send_event(Wake);
        });
        let _ = (metrics, padding);
        let opened = (opts.open)(wake);
        self.spawn = opened.spawn;
        for term in opened.terminals {
            self.place(term);
        }
        if let Some(first) = self.terminals.first().map(|t| t.id) {
            self.focus(first);
        }
        self.frame_all();
        self.redraw();
    }

    /// Put a terminal on the canvas, to the right of everything there.
    fn place(&mut self, term: Box<dyn Terminal>) -> u64 {
        let right = self.terminals.iter().map(|t| self.rect_of(t).max.x).fold(f32::NEG_INFINITY, f32::max);
        let padding = self.gfx.as_ref().map_or(0.0, |g| g.renderer.style().panel_padding);
        let x = if right.is_finite() { right + GAP + padding } else { 0.0 };
        let top = self.terminals.iter().map(|t| t.z).fold(0.0_f32, f32::max);
        self.next_id += 1;
        let id = self.next_id;
        self.terminals.push(Placed { id, term, position: Vec2::new(x, 0.0), z: top + 1.0, scale: 1.0, border_color: None });
        id
    }

    /// The spawn chord: one more terminal, focused and brought into view.
    fn spawn_terminal(&mut self) {
        let Some(spawn) = &mut self.spawn else { return };
        match spawn() {
            Ok(term) => {
                let id = self.place(term);
                self.focus(id);
                self.frame_all();
                self.redraw();
            }
            Err(e) => log::error!("the wall could not start a terminal: {e}"),
        }
    }

    fn resize(&mut self, width: u32, height: u32) {
        let Some(g) = &mut self.gfx else { return };
        if width == 0 || height == 0 {
            return;
        }
        g.config.width = width;
        g.config.height = height;
        g.surface.configure(&self.ctx.device, &g.config);
        g.depth = crate::scene::create_depth(&self.ctx.device, WALL_DEPTH_FORMAT, width, height);
        g.window.request_redraw();
    }

    fn draw(&mut self) {
        let Some(g) = &mut self.gfx else { return };
        use wgpu::CurrentSurfaceTexture as Cst;
        let frame = match g.surface.get_current_texture() {
            Cst::Success(f) | Cst::Suboptimal(f) => f,
            Cst::Lost | Cst::Outdated => {
                g.surface.configure(&self.ctx.device, &g.config);
                g.window.request_redraw();
                return;
            }
            // Covered or asleep: the next event that matters asks again.
            Cst::Occluded | Cst::Timeout => return,
            Cst::Validation => {
                log::error!("the wall's surface failed validation; skipping the frame");
                return;
            }
        };
        let view = frame.texture.create_view(&Default::default());
        let target = WallTarget { color_view: &view, depth_view: &g.depth, width: g.config.width, height: g.config.height };
        let draws: Vec<TerminalDraw<'_>> = self
            .terminals
            .iter()
            .map(|t| TerminalDraw {
                id: t.id,
                screen: t.term.screen(),
                position: t.position,
                z: t.z,
                scale: t.scale,
                focused: self.focused == Some(t.id),
                panel_opacity: None,
                border_color: t.border_color,
            })
            .collect();
        let mut encoder = self.ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("wall frame") });
        // A title bar above each terminal: a strip and its name, shrunk to
        // fit. (The wall's own rules for names and map labels are
        // wall-core's; this is the plain version until they drive it.)
        let metrics = g.renderer.metrics();
        let trie = &g.renderer.atlas().trie;
        let padding = g.renderer.style().panel_padding;
        // TITLE_BAR_PX is in logical pixels: a Retina display doubles it.
        let bar_h = (TITLE_BAR_PX * g.window.scale_factor() as f32 / self.camera.zoom).min(TITLE_BAR_MAX_WORLD);
        let mut shapes: Vec<Shape> = Vec::new();
        let mut labels: Vec<Label<'_>> = Vec::new();
        for t in &self.terminals {
            let s = t.term.screen();
            let panel = metrics.terminal_panel_rect(t.position, s.cols, s.rows, padding, t.scale);
            let strip = WorldRect::new(Vec2::new(panel.min.x, panel.max.y), Vec2::new(panel.max.x, panel.max.y + bar_h));
            let focused = self.focused == Some(t.id);
            shapes.push(Shape::Rect(RectShape {
                rect: strip,
                z: t.z,
                fill: Some(if focused { [0.13, 0.16, 0.22, 1.0] } else { [0.08, 0.09, 0.12, 1.0] }),
                stroke: None,
                stroke_px: 0.0,
                dash: DashStyle::SOLID,
                corner_radius_px: 0.0,
            }));
            let title = t.term.title();
            let inset = bar_h * 0.45;
            let room = (strip.width() - 2.0 * inset).max(0.0);
            let natural = bar_h * 0.62;
            let full = measure_label_width(metrics, trie, title, natural);
            let text_h = if full > room && full > 1e-4 { natural * (room / full) } else { natural };
            if text_h > 1e-4 && room > 1e-4 {
                let color = if focused { [0.85, 0.88, 0.94, 1.0] } else { [0.55, 0.60, 0.68, 1.0] };
                labels.push(Label::new(title, Vec2::new(strip.min.x + inset, strip.center().y + 0.5 * text_h), LabelHeight::World(text_h), color, t.z).with_clip(strip));
            }
        }
        let paper = PaperStyle::default();
        let wall_frame = WallFrame { paper: Some(&paper), terminals: &draws, shapes: &shapes, labels: &labels };
        let stats = g.renderer.render(&self.ctx.device, &self.ctx.queue, &mut encoder, &target, &self.camera, &wall_frame);
        let _ = stats;
        #[cfg(feature = "egui-ui")]
        let mut ui_painted = if self.hud_open || g.ui.is_active() {
            let hud_terms: Vec<HudTerminalInfo<'_>> = self
                .terminals
                .iter()
                .map(|t| {
                    let s = t.term.screen();
                    HudTerminalInfo {
                        id: t.id,
                        title: t.term.title(),
                        cols: s.cols,
                        rows: s.rows,
                        scale: t.scale,
                        focused: self.focused == Some(t.id),
                    }
                })
                .collect();
            let hud_input = DebugHudInput {
                camera_zoom: self.camera.zoom,
                camera_center: self.camera.center,
                viewport: (g.config.width, g.config.height),
                stats,
                terminals: &hud_terms,
            };
            let hud_open = &mut self.hud_open;
            let mut ui_frame = g.ui.run(&g.window, (g.config.width, g.config.height), |ctx| {
                if *hud_open {
                    show_debug_hud(ctx, hud_open, &hud_input);
                }
            });
            if let Some(painted) = ui_frame.painted.as_mut() {
                g.ui.paint(&self.ctx.device, &self.ctx.queue, &mut encoder, &view, painted);
            }
            match ui_frame.repaint_after {
                Some(delay) if delay.is_zero() => {
                    self.ui_repaint_at = None;
                    g.window.request_redraw();
                }
                Some(delay) => self.ui_repaint_at = Some(Instant::now() + delay),
                None => self.ui_repaint_at = None,
            }
            ui_frame.painted
        } else {
            self.ui_repaint_at = None;
            None
        };
        self.ctx.queue.submit([encoder.finish()]);
        self.ctx.queue.present(frame);
        // A counted run can keep its last frame: GLYPH_WALL_SHOT=path.png
        // draws the same frame once more offscreen and saves it.
        if self.exit_after == Some(self.frames + 1) {
            if let Ok(path) = std::env::var("GLYPH_WALL_SHOT") {
                let path = std::path::PathBuf::from(path);
                let viewport = (g.config.width, g.config.height);
                super::renderer::render_wall_to_png_with_overlay(
                    &self.ctx,
                    &mut g.renderer,
                    &self.camera,
                    &wall_frame,
                    viewport,
                    1,
                    &path,
                    |enc, color_view| {
                        #[cfg(feature = "egui-ui")]
                        if let Some(painted) = ui_painted.as_mut() {
                            g.ui.paint(&self.ctx.device, &self.ctx.queue, enc, color_view, painted);
                        }
                        #[cfg(not(feature = "egui-ui"))]
                        let _ = (enc, color_view);
                    },
                );
                // The window's surface is BGRA; the PNG writer assumed RGBA.
                if matches!(g.config.format, wgpu::TextureFormat::Bgra8UnormSrgb | wgpu::TextureFormat::Bgra8Unorm) {
                    if let Ok(img) = image::open(&path) {
                        let mut rgba = img.to_rgba8();
                        for px in rgba.pixels_mut() {
                            px.0.swap(0, 2);
                        }
                        if let Err(e) = rgba.save(&path) {
                            log::warn!("could not rewrite {}: {e}", path.display());
                        }
                    }
                }
            }
        }
        self.frames += 1;
        // A counted run can type: GLYPH_WALL_PASTE is pasted into the
        // focused terminal a moment in, so a script sees a program answer.
        // A counted run can also spawn: GLYPH_WALL_SPAWN=N starts N more terminals.
        if self.exit_after.is_some() && self.frames == 10 {
            let n = std::env::var("GLYPH_WALL_SPAWN").ok().and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
            for _ in 0..n {
                self.spawn_terminal();
            }
        }
        if self.exit_after.is_some() && self.frames == 30 {
            if let Ok(text) = std::env::var("GLYPH_WALL_PASTE") {
                if let Some(t) = self.focused.and_then(|id| self.terminals.iter_mut().find(|t| t.id == id)) {
                    t.term.paste(&text);
                }
            }
            // And press keys: GLYPH_WALL_TYPE goes in a key at a time, a
            // newline as Enter, through the same path a keyboard takes
            // after winit.
            if let Ok(text) = std::env::var("GLYPH_WALL_TYPE") {
                if let Some(t) = self.focused.and_then(|id| self.terminals.iter_mut().find(|t| t.id == id)) {
                    for ch in text.chars() {
                        let (key, typed) = if ch == '\n' { ("Enter".to_string(), None) } else { (ch.to_string(), Some(ch.to_string())) };
                        t.term.key(&KeyEvent { code: String::new(), key, text: typed, pressed: true, ..KeyEvent::default() });
                    }
                }
            }
        }
    }

    fn wheel(&mut self, delta: MouseScrollDelta) {
        let px = match delta {
            MouseScrollDelta::LineDelta(x, y) => Vec2::new(x, y) * PX_PER_LINE,
            MouseScrollDelta::PixelDelta(PhysicalPosition { x, y }) => Vec2::new(x as f32, y as f32),
        };
        if self.modifiers.super_key() || self.modifiers.control_key() || self.modifiers.alt_key() {
            let viewport = self.viewport();
            self.camera.zoom_about_screen_point(self.cursor, (px.y * WHEEL_ZOOM_PER_PX).exp(), viewport);
        } else {
            self.camera.pan_by_screen_delta(px);
        }
        self.redraw();
    }

    fn key(&mut self, ev: &winit::event::KeyEvent, event_loop: &ActiveEventLoop) {
        let key = super::keys::key_event(ev, self.modifiers);
        // The wall's own chords, the few there are yet. Command on macOS;
        // the full table is wall-core's keymap.
        if key.pressed && key.mods.meta && !key.mods.ctrl && !key.mods.alt {
            match key.code.as_str() {
                "KeyQ" | "KeyW" => {
                    event_loop.exit();
                    return;
                }
                "Digit0" => {
                    self.frame_all();
                    self.redraw();
                    return;
                }
                "Backslash" => {
                    self.spawn_terminal();
                    return;
                }
                _ => {}
            }
        }
        // The paste chord (Cmd+V on macOS, Ctrl+Shift+V elsewhere) pastes the
        // system clipboard; the program never sees the key itself.
        if wall_core::chord::is_paste_chord(wall_core::input::Platform::host(), &key) {
            if key.pressed && !key.repeat {
                match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                    Ok(text) if !text.is_empty() => {
                        if let Some(t) = self.focused.and_then(|id| self.terminals.iter_mut().find(|t| t.id == id)) {
                            t.term.paste(&text);
                        }
                    }
                    Ok(_) => {}
                    Err(e) => log::warn!("paste: the clipboard gave no text: {e}"),
                }
            }
            return;
        }
        if let Some(t) = self.focused.and_then(|id| self.terminals.iter_mut().find(|t| t.id == id)) {
            t.term.key(&key);
        }
    }
}

impl ApplicationHandler<Wake> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_none() {
            self.open(event_loop);
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _wake: Wake) {
        let mut changed = false;
        for t in &mut self.terminals {
            changed |= t.term.pump();
        }
        if changed {
            self.redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        #[cfg(feature = "egui-ui")]
        let ui_outcome = match &mut self.gfx {
            Some(g) => g.ui.on_window_event(&g.window, &event),
            None => UiEventOutcome::default(),
        };
        #[cfg(feature = "egui-ui")]
        if ui_outcome.repaint {
            self.redraw();
        }
        #[cfg(feature = "egui-ui")]
        let (ui_consumed, ui_wants_keyboard, ui_wants_pointer) = (
            ui_outcome.consumed,
            ui_outcome.wants_keyboard,
            ui_outcome.wants_pointer,
        );
        #[cfg(not(feature = "egui-ui"))]
        let (ui_consumed, ui_wants_keyboard, ui_wants_pointer) = (false, false, false);

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => self.resize(size.width, size.height),
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::CursorMoved { position, .. } => {
                let now = Vec2::new(position.x as f32, position.y as f32);
                if self.panning && !ui_consumed {
                    self.camera.pan_by_screen_delta(now - self.cursor);
                    self.redraw();
                }
                self.cursor = now;
            }
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left | MouseButton::Middle, .. } => {
                self.panning = false;
            }
            WindowEvent::MouseInput { state, button, .. } if !ui_consumed && !ui_wants_pointer => match (button, state) {
                (MouseButton::Left, ElementState::Pressed) => match self.terminal_at(self.cursor) {
                    Some(id) => {
                        self.focus(id);
                        self.redraw();
                    }
                    None => self.panning = true,
                },
                (MouseButton::Middle, ElementState::Pressed) => self.panning = true,
                _ => {}
            },
            WindowEvent::MouseWheel { delta, .. } if !ui_consumed && !ui_wants_pointer => self.wheel(delta),
            WindowEvent::PinchGesture { delta, .. } if !ui_consumed && !ui_wants_pointer => {
                let viewport = self.viewport();
                self.camera.zoom_about_screen_point(self.cursor, 1.0 + delta as f32, viewport);
                self.redraw();
            }
            #[cfg(feature = "egui-ui")]
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. }
                if !ui_consumed
                    && event.state == ElementState::Pressed
                    && !event.repeat
                    && event.physical_key == PhysicalKey::Code(KeyCode::F1) =>
            {
                self.hud_open = !self.hud_open;
                self.redraw();
            }
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } if !ui_consumed && !ui_wants_keyboard => {
                self.key(&event, event_loop);
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        match self.exit_after {
            Some(n) if self.frames >= n => event_loop.exit(),
            // A counted run draws without waiting to be asked.
            Some(_) => self.redraw(),
            None => {}
        }
        #[cfg(feature = "egui-ui")]
        if let Some(deadline) = self.ui_repaint_at {
            if Instant::now() >= deadline {
                self.ui_repaint_at = None;
                self.redraw();
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
                return;
            }
        }
        // Nothing animates yet: sleep until the next event or wake.
        event_loop.set_control_flow(ControlFlow::Wait);
    }
}

/// Open the wall and run until its window closes.
pub fn run(ctx: GpuContext, opts: WallOptions) {
    let event_loop = EventLoop::<Wake>::with_user_event().build().expect("event loop creation failed");
    let mut app = App {
        ctx,
        opts: Some(opts),
        proxy: event_loop.create_proxy(),
        gfx: None,
        camera: WallCamera::default(),
        terminals: Vec::new(),
        focused: None,
        modifiers: ModifiersState::empty(),
        cursor: Vec2::ZERO,
        panning: false,
        frames: 0,
        exit_after: None,
        spawn: None,
        next_id: 0,
        #[cfg(feature = "egui-ui")]
        hud_open: std::env::var_os("GLYPH_WALL_HUD")
            .is_some_and(|v| !v.is_empty() && v != "0"),
        #[cfg(feature = "egui-ui")]
        ui_repaint_at: None,
    };
    event_loop.run_app(&mut app).expect("event loop error");
    println!("the wall drew {} frames", app.frames);
    if app.exit_after.is_some() {
        // A counted run says what its terminals showed, as text.
        for t in &app.terminals {
            let s = t.term.screen();
            println!("--- terminal {} ({}x{}) {:?}", t.id, s.cols, s.rows, t.term.title());
            for r in 0..s.rows {
                let line: String = s.row(r).iter().map(|c| if c.content == 0 { " ".into() } else { s.content_str(c).into_owned() }).collect();
                let line = line.trim_end();
                if !line.is_empty() {
                    println!("{line}");
                }
            }
        }
    }
}
