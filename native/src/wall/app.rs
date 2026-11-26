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

use glam::Vec2;
use term_cells::Screen;
use wall_core::input::KeyEvent;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use crate::atlas::Atlas;
use crate::gpu::GpuContext;

use super::camera::WallCamera;
use super::metrics::WorldRect;
use super::paper::PaperStyle;
use super::renderer::{TerminalDraw, WALL_DEPTH_FORMAT, WallFrame, WallRenderer, WallStyle, WallTarget};

/// Physical pixels a wheel "line" is worth when a mouse reports lines.
const PX_PER_LINE: f32 = 53.0;
/// How fast the wheel zooms: the zoom multiplies by `exp(px * this)`.
const WHEEL_ZOOM_PER_PX: f32 = 0.0025;
/// World units between terminals placed side by side.
const GAP: f32 = 2.0;

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
    pub open: Box<dyn FnOnce(Arc<dyn Fn() + Send + Sync>) -> Vec<Box<dyn Terminal>>>,
}

/// A terminal on the canvas.
struct Placed {
    id: u64,
    term: Box<dyn Terminal>,
    /// Top-left of cell (0, 0), world units, y up.
    position: Vec2,
    z: f32,
}

/// What exists once there is a window.
struct Gfx {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    depth: wgpu::TextureView,
    renderer: WallRenderer,
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
        g.renderer.metrics().panel_rect(t.position, s.cols, s.rows, g.renderer.style().panel_padding)
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
        let metrics = renderer.metrics();
        let padding = renderer.style().panel_padding;
        self.gfx = Some(Gfx { window, surface, config, depth, renderer });

        let proxy = self.proxy.clone();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            // The loop may be gone at shutdown; nothing is left to wake.
            let _ = proxy.send_event(Wake);
        });
        let mut x = 0.0_f32;
        for (i, term) in (opts.open)(wake).into_iter().enumerate() {
            let (cols, rows) = (term.screen().cols, term.screen().rows);
            let position = Vec2::new(x, 0.0);
            x += metrics.grid_rect(position, cols, rows).width() + 2.0 * padding + GAP;
            self.terminals.push(Placed { id: i as u64 + 1, term, position, z: i as f32 + 1.0 });
        }
        if let Some(first) = self.terminals.first().map(|t| t.id) {
            self.focus(first);
        }
        self.frame_all();
        self.redraw();
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
                focused: self.focused == Some(t.id),
                panel_opacity: None,
                border_color: None,
            })
            .collect();
        let paper = PaperStyle::default();
        let wall_frame = WallFrame { paper: Some(&paper), terminals: &draws, ..Default::default() };
        let mut encoder = self.ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("wall frame") });
        g.renderer.render(&self.ctx.device, &self.ctx.queue, &mut encoder, &target, &self.camera, &wall_frame);
        self.ctx.queue.submit([encoder.finish()]);
        self.ctx.queue.present(frame);
        self.frames += 1;
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
                _ => {}
            }
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
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => self.resize(size.width, size.height),
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::CursorMoved { position, .. } => {
                let now = Vec2::new(position.x as f32, position.y as f32);
                if self.panning {
                    self.camera.pan_by_screen_delta(now - self.cursor);
                    self.redraw();
                }
                self.cursor = now;
            }
            WindowEvent::MouseInput { state, button, .. } => match (button, state) {
                (MouseButton::Left, ElementState::Pressed) => match self.terminal_at(self.cursor) {
                    Some(id) => {
                        self.focus(id);
                        self.redraw();
                    }
                    None => self.panning = true,
                },
                (MouseButton::Middle, ElementState::Pressed) => self.panning = true,
                (MouseButton::Left | MouseButton::Middle, ElementState::Released) => self.panning = false,
                _ => {}
            },
            WindowEvent::MouseWheel { delta, .. } => self.wheel(delta),
            WindowEvent::PinchGesture { delta, .. } => {
                let viewport = self.viewport();
                self.camera.zoom_about_screen_point(self.cursor, 1.0 + delta as f32, viewport);
                self.redraw();
            }
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => self.key(&event, event_loop),
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
    };
    event_loop.run_app(&mut app).expect("event loop error");
    println!("the wall drew {} frames", app.frames);
}
