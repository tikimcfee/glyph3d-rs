//! The wall's window: winit, the wall's rules, the terminals and the renderer,
//! joined.
//!
//! Three things meet here and none of them knows the others:
//!
//! - `wall_core::wall::Wall` holds every rule of the wall (where terminals
//!   sit, focus, groups, splits, gestures, chords, verbs). It takes input as
//!   plain data and answers with effects to carry out and a description of
//!   the frame to draw.
//! - [`Sessions`] holds the terminals: one connection to `g3d-termd` and an
//!   emulator per terminal, giving a screen each.
//! - [`WallRenderer`] draws terminals, shapes and labels.
//!
//! This file translates between them. A winit event becomes a pointer, wheel
//! or key event in logical pixels for the wall; each effect the wall emits
//! becomes a termd request, a clipboard call or a cursor change; each frame
//! the wall describes becomes draw lists. It draws on demand: the event loop
//! sleeps until input arrives, a terminal's reader thread sends [`Wake`], or
//! the wall says something is still moving.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use glam::Vec2;
use term_cells::Screen;
use wall_core::geom::{Point, Rect};
use wall_core::input::{Button, KeyEvent, Mods, Platform};
use wall_core::layout::{ResizeCursor, Size};
use wall_core::menu::Verdict;
use wall_core::model::{CellGeometry, WallEffect};
use wall_core::pointer::{Buttons, CursorIcon, PointerEvent, WheelEvent};
use wall_core::scroll::DeltaMode;
use wall_core::wall::{screen_lookup, TerminalAppeared, Wall, WallFrame as Described, WallOptions as RulesOptions};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
#[cfg(feature = "egui-ui")]
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use crate::gpu::GpuContext;

use super::camera::WallCamera;
use super::labels::{Label, LabelHeight};
use super::metrics::WorldRect;
use super::paper::PaperStyle;
use super::renderer::{TerminalDraw, WallFrame, WallRenderer, WallStyle, WallTarget, WALL_DEPTH_FORMAT};
use super::session::{News, Sessions};
use super::shapes::{DashStyle, LeaderShape, PolygonShape, RectShape, Shape};
use super::srgb_hex;
#[cfg(feature = "egui-ui")]
use super::ui::{show_debug_hud, DebugHudInput, HudTerminalInfo, UiEventOutcome, WallUi};

/// A cell's height in world units. The JS wall's terminals are drawn at
/// scale 2 of a 1.2-unit line, and every gap, pad and grid spacing in the
/// wall's rules is tuned to that, so the native wall keeps the same world.
const WORLD_CELL_HEIGHT: f32 = 2.4;
/// The panel's padding round its cells, in world units (the JS wall's 0.3).
const PANEL_PAD: f32 = 0.3;
/// Two presses this close in time and place are a double click.
const DOUBLE_CLICK_MS: f64 = 500.0;
const DOUBLE_CLICK_PX: f64 = 5.0;
/// Depth of what always draws over the canvas (map labels, the notice).
const Z_OVERLAY: f32 = 900.0;

/// What a reader thread sends the event loop: something arrived, look.
#[derive(Clone, Copy, Debug)]
pub struct Wake;

/// How the window is opened.
pub struct WallOptions {
    pub emoji_sheet: PathBuf,
    /// The termd namespace whose terminals the wall shows. None: no daemon,
    /// one fixed test screen instead.
    pub namespace: Option<String>,
    /// Draw this many frames, one after another, then exit (`--frames`): how
    /// a script checks the window came up and drew.
    pub exit_after: Option<u64>,
}

/// Where the wall's terminals come from.
enum Terminals {
    Live(Sessions),
    /// Screens that never change (the test pattern), by id.
    Fixed(BTreeMap<String, Screen>),
}

impl Terminals {
    fn screen(&self, id: &str) -> Option<&Screen> {
        match self {
            Terminals::Live(s) => s.screen(id),
            Terminals::Fixed(m) => m.get(id),
        }
    }
}

/// What exists once there is a window.
struct Gfx {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    depth: wgpu::TextureView,
    renderer: WallRenderer,
    /// Renderer scale that makes a cell [`WORLD_CELL_HEIGHT`] tall.
    cell_scale: f32,
    #[cfg(feature = "egui-ui")]
    ui: WallUi,
}

struct App {
    ctx: GpuContext,
    opts: Option<WallOptions>,
    proxy: EventLoopProxy<Wake>,
    gfx: Option<Gfx>,
    wall: Option<Wall>,
    terms: Option<Terminals>,
    /// The socket `wallctl` reaches this wall by; None if it could not bind
    /// (another wall holds the namespace) or the wall has no namespace.
    control: Option<wall_control::Server>,
    /// A frame `wallctl app.shot PATH` asked to keep, answered once it is written.
    pending_shot: Option<(PathBuf, wall_control::Responder)>,
    quit: bool,
    modifiers: ModifiersState,
    /// The pointer in logical pixels from the window's top-left.
    cursor: Point,
    /// Buttons held, as the web's `buttons` bitmask.
    buttons: u8,
    /// The last left release: when and where (for double clicks).
    last_release: Option<(f64, Point)>,
    /// The renderer keys a terminal's GPU buffers by a number.
    render_ids: HashMap<String, u64>,
    start: Instant,
    last_tick: Instant,
    /// The wall said something is still moving: draw again without waiting.
    animating: bool,
    frames: u64,
    exit_after: Option<u64>,
    #[cfg(feature = "egui-ui")]
    hud_open: bool,
    #[cfg(feature = "egui-ui")]
    panels: super::mount::Panels,
    #[cfg(feature = "egui-ui")]
    ui_repaint_at: Option<Instant>,
}

/// `#rrggbb` (or `#rgb`) as sRGB RGBA; anything else is magenta, loudly.
fn css_color(s: &str, alpha: f32) -> [f32; 4] {
    let h = s.trim().trim_start_matches('#');
    let full = match h.len() {
        3 => h.chars().flat_map(|c| [c, c]).collect::<String>(),
        6 => h.to_string(),
        _ => String::new(),
    };
    match u32::from_str_radix(&full, 16) {
        Ok(v) if full.len() == 6 => srgb_hex(v, alpha),
        _ => [1.0, 0.0, 1.0, alpha],
    }
}

fn world_rect(r: Rect) -> WorldRect {
    WorldRect::new(Vec2::new(r.min_x as f32, r.min_y as f32), Vec2::new(r.max_x as f32, r.max_y as f32))
}

fn v2(p: Point) -> Vec2 {
    Vec2::new(p.x as f32, p.y as f32)
}

fn button_of(b: MouseButton) -> Option<Button> {
    Some(match b {
        MouseButton::Left => Button::Left,
        MouseButton::Middle => Button::Middle,
        MouseButton::Right => Button::Right,
        MouseButton::Back => Button::Back,
        MouseButton::Forward => Button::Forward,
        MouseButton::Other(_) => return None,
    })
}

fn cursor_of(c: CursorIcon) -> winit::window::CursorIcon {
    use winit::window::CursorIcon as W;
    match c {
        CursorIcon::Default => W::Default,
        CursorIcon::Text => W::Text,
        CursorIcon::Pointer => W::Pointer,
        CursorIcon::Move => W::Move,
        CursorIcon::Grab => W::Grab,
        CursorIcon::Crosshair => W::Crosshair,
        CursorIcon::Copy => W::Copy,
        CursorIcon::ColResize => W::ColResize,
        CursorIcon::RowResize => W::RowResize,
        CursorIcon::Resize(ResizeCursor::NwseResize) => W::NwseResize,
        CursorIcon::Resize(ResizeCursor::NeswResize) => W::NeswResize,
        CursorIcon::Resize(ResizeCursor::EwResize) => W::EwResize,
        CursorIcon::Resize(ResizeCursor::NsResize) => W::NsResize,
    }
}

/// A screen as text, a line a row, trailing blanks and blank rows dropped.
fn screen_text(s: &Screen) -> String {
    let mut out = String::new();
    for r in 0..s.rows {
        let line: String = s.row(r).iter().map(|c| if c.content == 0 { " ".into() } else { s.content_str(c).into_owned() }).collect();
        let line = line.trim_end();
        if !line.is_empty() {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Where the wall keeps its state between runs. A scratch daemon directory
/// (`G3D_TERMD_DIR`, as tests and probes set) keeps a scratch store beside
/// it, so a probe never touches a person's saved wall.
fn store_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("G3D_TERMD_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/glyph3d/wall")
    } else {
        std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()).map(PathBuf::from).unwrap_or_else(|| home.join(".local/state")).join("glyph3d/wall")
    }
}

impl App {
    fn scale_factor(&self) -> f64 {
        self.gfx.as_ref().map_or(1.0, |g| g.window.scale_factor())
    }

    fn now_ms(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0
    }

    fn mods(&self) -> Mods {
        super::keys::mods(self.modifiers)
    }

    fn redraw(&self) {
        if let Some(g) = &self.gfx {
            g.window.request_redraw();
        }
    }

    fn pointer(&self, button: Option<Button>) -> PointerEvent {
        PointerEvent { pointer_id: 1, x: self.cursor.x, y: self.cursor.y, button, buttons: Buttons(self.buttons), mods: self.mods(), time_ms: self.now_ms() }
    }

    fn render_id(&mut self, id: &str) -> u64 {
        let next = self.render_ids.len() as u64 + 1;
        *self.render_ids.entry(id.to_string()).or_insert(next)
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
        let mut renderer = WallRenderer::new(&self.ctx.device, &self.ctx.queue, &opts.emoji_sheet, format, WallStyle::default());
        let metrics = renderer.metrics();
        let cell_scale = WORLD_CELL_HEIGHT / metrics.cell_height;
        renderer.style_mut().panel_padding = PANEL_PAD / cell_scale;
        #[cfg(feature = "egui-ui")]
        let ui = WallUi::new(&self.ctx.device, &window, format);
        let sf = window.scale_factor();

        // The rules, at the renderer's cell size and the window's viewport.
        let namespace = opts.namespace.clone().unwrap_or_else(|| "pattern".to_string());
        let rules = RulesOptions {
            geometry: CellGeometry::with_padding(f64::from(metrics.cell_width * cell_scale), f64::from(metrics.cell_height * cell_scale), f64::from(PANEL_PAD), f64::from(PANEL_PAD)),
            platform: Platform::host(),
            viewport: Size::new(f64::from(config.width) / sf, f64::from(config.height) / sf),
            scale_factor: sf,
            namespace: namespace.clone(),
            ..RulesOptions::default()
        };
        let mut wall = if opts.namespace.is_some() { Wall::open(store_dir(), namespace.clone(), rules) } else { Wall::in_memory(rules) };

        // The terminals.
        let proxy = self.proxy.clone();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            // The loop may be gone at shutdown; nothing is left to wake.
            let _ = proxy.send_event(Wake);
        });
        // The door for `wallctl`: verbs arrive on a socket beside termd's.
        if let Some(ns) = &opts.namespace {
            match wall_control::Server::bind(&wall_control::Target::namespace(ns.clone()), wake.clone()) {
                Ok(server) => self.control = Some(server),
                Err(e) => log::warn!("the wall's control socket is not open, so wallctl cannot reach this window: {e}"),
            }
        }
        let terms = match &opts.namespace {
            Some(ns) => {
                let mut sessions = match Sessions::open(ns, wake) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("error: the wall could not reach its terminals: {e}");
                        std::process::exit(1);
                    }
                };
                let ids = sessions.list().unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                });
                wall.set_live_terminals(&ids);
                for id in &ids {
                    match sessions.adopt(id) {
                        Ok((cols, rows)) => {
                            wall.terminal_appeared(TerminalAppeared::adopted(id.clone(), u32::from(cols), u32::from(rows)));
                        }
                        Err(e) => log::error!("{e}"),
                    }
                }
                wall.set_connected(true);
                if ids.is_empty() {
                    wall.run("wall.spawn");
                }
                Terminals::Live(sessions)
            }
            None => {
                let screen = super::test_pattern::build_primary_screen();
                wall.terminal_appeared(TerminalAppeared::new("pattern", u32::from(screen.cols), u32::from(screen.rows)).with_title(screen.title.clone()));
                Terminals::Fixed(BTreeMap::from([("pattern".to_string(), screen)]))
            }
        };

        self.gfx = Some(Gfx {
            window,
            surface,
            config,
            depth,
            renderer,
            cell_scale,
            #[cfg(feature = "egui-ui")]
            ui,
        });
        self.wall = Some(wall);
        self.terms = Some(terms);
        self.apply_effects();
        if let Some(wall) = &mut self.wall {
            wall.run("wall.fit");
        }
        self.apply_effects();
        self.redraw();
    }

    /// Carry out what the wall asked for, until it asks for nothing more.
    fn apply_effects(&mut self) {
        // An effect's result can make the wall ask again (a spawned terminal
        // lands and is framed); a handful of rounds settles it.
        for _ in 0..8 {
            let Some(wall) = &mut self.wall else { return };
            let effects = wall.drain_effects();
            if effects.is_empty() {
                return;
            }
            for effect in effects {
                self.apply(effect);
            }
        }
        log::warn!("the wall kept asking for more after eight rounds of effects; leaving the rest for the next event");
    }

    fn apply(&mut self, effect: WallEffect) {
        let (Some(wall), Some(terms)) = (&mut self.wall, &mut self.terms) else { return };
        match effect {
            WallEffect::Redraw => {
                if let Some(g) = &self.gfx {
                    g.window.request_redraw();
                }
            }
            WallEffect::SetCursor { cursor } => {
                if let Some(g) = &self.gfx {
                    g.window.set_cursor(cursor_of(cursor));
                }
            }
            WallEffect::WriteClipboard { text } => {
                if let Err(e) = arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
                    log::warn!("copy: the clipboard refused the text: {e}");
                }
            }
            WallEffect::ReadClipboard { target } => match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                Ok(text) => wall.deliver_clipboard(&target, &text),
                Err(e) => log::warn!("paste: the clipboard gave no text: {e}"),
            },
            WallEffect::ConfigKnobChanged { key, value } => {
                // The renderer's own knobs follow here as they are wired; the
                // rules have already taken the value.
                log::debug!("wall knob {key} is now {value:?}");
                if let Some(g) = &self.gfx {
                    g.window.request_redraw();
                }
            }
            other => {
                let Terminals::Live(sessions) = terms else { return };
                match other {
                    WallEffect::SpawnTerminal { argv, cols, rows, .. } => match sessions.spawn(argv, cols.min(u32::from(u16::MAX)) as u16, rows.min(u32::from(u16::MAX)) as u16) {
                        Ok((id, cols, rows)) => {
                            wall.terminal_appeared(TerminalAppeared::new(id, u32::from(cols), u32::from(rows)));
                        }
                        Err(e) => log::error!("the wall could not start a terminal: {e}"),
                    },
                    WallEffect::KillTerminal { id } => sessions.kill(&id),
                    WallEffect::ResizeTerminal { id, cols, rows } => sessions.resize(&id, cols.min(u32::from(u16::MAX)) as u16, rows.min(u32::from(u16::MAX)) as u16),
                    WallEffect::SendKey { id, key } => sessions.key(&id, &key),
                    WallEffect::PasteTerminal { id, text } => sessions.paste(&id, &text),
                    WallEffect::SendMouse { id, event } => sessions.mouse(&id, &event),
                    WallEffect::ScrollTerminal { id, lines, cell } => sessions.scroll(&id, lines, cell.x, cell.y),
                    WallEffect::RefreshTerminal { id } => sessions.repaint(&id),
                    _ => {}
                }
            }
        }
    }

    /// Run the verb lines `wallctl` sent, and answer each.
    fn serve_control(&mut self) {
        let Some(server) = &self.control else { return };
        let mut ran = false;
        while let Some(request) = server.try_recv() {
            let (line, responder) = request.into_parts();
            // The window's own few verbs: what a script needs that the wall's
            // rules cannot give (pixels, a terminal's text, quitting).
            let mut words = line.split_whitespace();
            match words.next() {
                Some("app.shot") => {
                    match words.next() {
                        Some(path) => self.pending_shot = Some((PathBuf::from(path), responder)),
                        None => {
                            responder.send("ERR: usage: app.shot <path.png>");
                        }
                    }
                    ran = true;
                    continue;
                }
                Some("app.text") => {
                    let reply = match (words.next(), &self.terms) {
                        (Some(id), Some(terms)) => match terms.screen(id) {
                            Some(s) => screen_text(s),
                            None => format!("ERR: no terminal '{id}'"),
                        },
                        _ => "ERR: usage: app.text <terminal id>".to_string(),
                    };
                    responder.send(reply);
                    continue;
                }
                Some("app.quit") => {
                    responder.send("OK: quitting");
                    self.quit = true;
                    ran = true;
                    continue;
                }
                Some("app.palette") => {
                    let reply = match &mut self.wall {
                        Some(wall) => {
                            let next = match words.next() {
                                Some("open") => true,
                                Some("close") => false,
                                Some("toggle") | None => !wall.palette_open(),
                                Some(other) => {
                                    responder.send(format!("ERR: takes toggle|open|close, not '{other}'"));
                                    continue;
                                }
                            };
                            wall.set_palette_open(next);
                            format!("OK: palette {}", if next { "open" } else { "closed" })
                        }
                        None => "ERR: the wall is not open yet".to_string(),
                    };
                    responder.send(reply);
                    ran = true;
                    continue;
                }
                _ => {}
            }
            let reply = match (&mut self.wall, &self.terms) {
                (Some(wall), Some(terms)) => wall.run_with_screens(line.as_str(), &screen_lookup(|id| terms.screen(id))).text,
                _ => "ERR: the wall is not open yet".to_string(),
            };
            responder.send(reply);
            ran = true;
        }
        if ran {
            self.apply_effects();
            self.redraw();
        }
    }

    /// Take in what the terminals' reader thread queued.
    fn pump(&mut self) {
        let (Some(wall), Some(Terminals::Live(sessions))) = (&mut self.wall, &mut self.terms) else { return };
        let (changed, news) = sessions.pump();
        let heard = !news.is_empty();
        for n in news {
            match n {
                News::Resized { id, cols, rows } => wall.terminal_resized(&id, u32::from(cols), u32::from(rows)),
                News::Title { id, title } => wall.terminal_title(&id, title),
                News::Modes { id, modes } => wall.terminal_modes(&id, modes),
                News::Ended { id } => wall.terminal_ended(&id),
                News::Disconnected { message } => {
                    log::error!("termd is gone: {message}");
                    wall.set_connected(false);
                }
            }
        }
        if heard {
            self.apply_effects();
        }
        if changed || heard {
            self.redraw();
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
        let sf = g.window.scale_factor();
        if let Some(wall) = &mut self.wall {
            wall.set_viewport(f64::from(width) / sf, f64::from(height) / sf, sf);
        }
        self.apply_effects();
        self.redraw();
    }

    /// The wall's description of a frame as the renderer's draw lists.
    #[expect(clippy::too_many_lines, reason = "one arm per kind of thing the wall describes; splitting it scatters the mapping")]
    fn draw(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f64().min(1.0 / 30.0);
        self.last_tick = now;
        let now_ms = self.now_ms();
        let Some(wall) = &mut self.wall else { return };
        let moving = wall.tick(dt, now_ms);
        let described: Described = wall.frame();
        self.animating = moving || described.animating;
        self.apply_effects();
        let ids: Vec<(String, u64)> = described.terminals.iter().map(|t| (t.id.clone(), 0)).collect();
        let ids: Vec<(String, u64)> = ids.into_iter().map(|(id, _)| (id.clone(), self.render_id(&id))).collect();

        let (Some(g), Some(terms)) = (&mut self.gfx, &self.terms) else { return };
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
        let sf = g.window.scale_factor() as f32;
        // The wall's camera is in logical pixels per world unit; the
        // renderer's is in physical ones.
        let camera = WallCamera::new(Vec2::new(described.camera.x as f32, described.camera.y as f32), described.camera.zoom as f32 * sf);
        let viewport = (g.config.width, g.config.height);
        // A point in logical screen pixels as a world point.
        let to_world = |x: f64, y: f64| camera.screen_to_world(Vec2::new(x as f32 * sf, y as f32 * sf), viewport);

        let mut draws: Vec<TerminalDraw<'_>> = Vec::with_capacity(described.terminals.len());
        for (t, (_, render_id)) in described.terminals.iter().zip(&ids) {
            let Some(screen) = terms.screen(&t.id) else { continue };
            draws.push(TerminalDraw {
                id: *render_id,
                screen,
                position: v2(t.cell_origin),
                z: t.z as f32,
                scale: g.cell_scale * t.scale as f32,
                focused: t.focused,
                panel_opacity: None,
                border_color: t.hovered.then_some(term_cells::rgb(0x5a, 0x6b, 0x86)).filter(|_| !t.focused),
            });
        }

        let mut shapes: Vec<Shape> = Vec::new();
        let mut labels: Vec<Label<'_>> = Vec::new();
        let solid = |rect: WorldRect, z: f32, fill: [f32; 4]| Shape::Rect(RectShape { rect, z, fill: Some(fill), stroke: None, stroke_px: 0.0, dash: DashStyle::SOLID, corner_radius_px: 0.0 });
        let outline = |rect: WorldRect, z: f32, stroke: [f32; 4], px: f32, dash: DashStyle| Shape::Rect(RectShape { rect, z, fill: None, stroke: Some(stroke), stroke_px: px, dash, corner_radius_px: 0.0 });

        // Group frames sit behind their members; their headers carry the name.
        for group in &described.groups {
            let z = group.z as f32;
            shapes.push(Shape::Rect(RectShape {
                rect: world_rect(group.frame_rect),
                z,
                fill: Some(srgb_hex(0x0d1017, 1.0)),
                stroke: Some(srgb_hex(if group.focused { 0x3b4352 } else { 0x232936 }, 1.0)),
                stroke_px: sf,
                dash: DashStyle::SOLID,
                corner_radius_px: 0.0,
            }));
            let header = world_rect(group.header_rect);
            shapes.push(solid(header, z, srgb_hex(group.fill_color, 1.0)));
            let h = group.text_height as f32;
            if h > 1e-4 {
                labels.push(Label::new(group.name.clone(), Vec2::new(group.text_left as f32, header.center().y + 0.5 * h), LabelHeight::World(h), srgb_hex(group.text_color, 1.0), z).with_clip(header));
            }
        }

        // A title bar above each terminal: a strip and its name.
        for bar in &described.title_bars {
            let strip = world_rect(bar.rect);
            let z = bar.z as f32;
            shapes.push(solid(strip, z, srgb_hex(bar.fill_color, 1.0)));
            let h = bar.text_height as f32;
            if bar.text_visible && h > 1e-4 {
                labels.push(Label::new(bar.text.clone(), Vec2::new(bar.text_left as f32, strip.center().y + 0.5 * h), LabelHeight::World(h), srgb_hex(bar.text_color, 1.0), z).with_clip(strip));
            }
        }

        for sel in &described.selection_outlines {
            shapes.push(outline(world_rect(sel.rect), sel.z as f32 + 0.5, srgb_hex(0x4c8dff, 1.0), 2.0 * sf, DashStyle::SOLID));
        }
        if let Some(lasso) = &described.lasso {
            if lasso.len() >= 2 {
                shapes.push(Shape::Polygon(PolygonShape {
                    points: lasso.iter().map(|p| v2(*p)).collect(),
                    z: Z_OVERLAY,
                    fill: Some(srgb_hex(0x4c8dff, 0.03)),
                    stroke: srgb_hex(0x4c8dff, 0.9),
                    stroke_px: 1.5 * sf,
                    dash: DashStyle::new(6.0 * sf, 4.0 * sf),
                }));
            }
        }
        if let Some(drop) = &described.drop_preview {
            let (color, dash) = if drop.ok { (srgb_hex(0x6ee7a0, 1.0), DashStyle::SOLID) } else { (srgb_hex(0xff5c5c, 1.0), DashStyle::new(6.0 * sf, 4.0 * sf)) };
            shapes.push(outline(world_rect(drop.rect), Z_OVERLAY, color, 2.0 * sf, dash));
        }

        // A terminal fullscreen: everything else is dimmed behind it.
        if let Some(veil) = &described.fullscreen_backdrop {
            shapes.push(solid(camera.visible_world_rect(viewport), veil.z as f32, css_color(&veil.background, 0.92)));
        }

        // Map labels: a terminal too small on screen to carry its name gets a
        // pill beside it, with a leader. They are placed in screen pixels.
        for label in &described.map_labels {
            let b = label.label_box;
            let top_left = to_world(b.left, b.top);
            let bottom_right = to_world(b.right, b.bottom);
            let pill = WorldRect::new(Vec2::new(top_left.x, bottom_right.y), Vec2::new(bottom_right.x, top_left.y));
            shapes.push(Shape::Rect(RectShape {
                rect: pill,
                z: Z_OVERLAY,
                fill: Some(srgb_hex(0x121722, 0.95)),
                stroke: Some(srgb_hex(if label.focused { 0x6ee7a0 } else { 0x3b4352 }, 1.0)),
                stroke_px: sf,
                dash: DashStyle::SOLID,
                corner_radius_px: 4.0 * sf,
            }));
            let px = ((b.bottom - b.top) as f32 * 0.62 * sf).max(1.0);
            let h_world = px / camera.zoom;
            labels.push(Label::new(label.text.clone(), Vec2::new(pill.min.x + 6.0 * sf / camera.zoom, pill.center().y + 0.5 * h_world), LabelHeight::ScreenPx(px), srgb_hex(0xc4ccd8, 1.0), Z_OVERLAY).with_clip(pill));
            if let Some(l) = label.leader {
                shapes.push(Shape::Leader(LeaderShape { from: to_world(l.x1, l.y1), to: to_world(l.x2, l.y2), z: Z_OVERLAY, color: srgb_hex(0x5a6b86, 1.0), width_px: sf, dot_radius_px: 2.5 * sf, dash: DashStyle::SOLID }));
            }
        }

        // The notice: one line at the top of the wall.
        if let Some(text) = &described.notice {
            let px = 14.0 * sf;
            let at = to_world(16.0, 14.0);
            labels.push(Label::new(text.clone(), at, LabelHeight::ScreenPx(px), srgb_hex(0xffc857, 1.0), Z_OVERLAY));
        }

        let paper = PaperStyle {
            show: described.paper.grid_show,
            background: css_color(&described.paper.background, 1.0),
            minor_color: css_color(&described.paper.minor_color, 1.0),
            major_color: css_color(&described.paper.major_color, 1.0),
            minor_spacing: described.paper.grid.grid_minor as f32,
            major_every: described.paper.grid.grid_major_every,
            line_px: described.paper.line_px as f32 * sf,
            min_px: described.paper.grid.grid_min_px as f32 * sf,
        };
        let wall_frame = WallFrame { paper: Some(&paper), terminals: &draws, shapes: &shapes, labels: &labels };
        let mut encoder = self.ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("wall frame") });
        let stats = g.renderer.render(&self.ctx.device, &self.ctx.queue, &mut encoder, &target, &camera, &wall_frame);
        let _ = stats;

        #[cfg(feature = "egui-ui")]
        let mut panel_verbs: Vec<String> = Vec::new();
        #[cfg(feature = "egui-ui")]
        let mut panel_egui_keys = false;
        #[cfg(feature = "egui-ui")]
        let mut ui_painted = if self.hud_open || self.wall.as_ref().is_some_and(|w| self.panels.any_open(w, &described)) || g.ui.is_active() {
            let hud_terms: Vec<HudTerminalInfo<'_>> = described
                .terminals
                .iter()
                .zip(&ids)
                .filter_map(|(t, (_, render_id))| {
                    let s = terms.screen(&t.id)?;
                    Some(HudTerminalInfo { id: *render_id, title: &t.id, cols: s.cols, rows: s.rows, scale: t.scale as f32, focused: t.focused })
                })
                .collect();
            let hud_input = DebugHudInput { camera_zoom: camera.zoom, camera_center: camera.center, viewport, stats, terminals: &hud_terms };
            let hud_open = &mut self.hud_open;
            let panels = &mut self.panels;
            let wall_ref = self.wall.as_ref();
            let mut ui_frame = g.ui.run(&g.window, viewport, |ctx| {
                if let Some(w) = wall_ref {
                    panel_verbs = panels.show(ctx, w, &described);
                }
                if *hud_open {
                    show_debug_hud(ctx, hud_open, &hud_input);
                }
            });
            panel_egui_keys = ui_frame.wants_keyboard;
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
        let asked = self.pending_shot.take();
        let counted = (self.exit_after == Some(self.frames + 1)).then(|| std::env::var("GLYPH_WALL_SHOT").ok().map(PathBuf::from)).flatten();
        if asked.is_some() || counted.is_some() {
            for path in asked.iter().map(|(p, _)| p.clone()).chain(counted) {
                super::renderer::render_wall_to_png_with_overlay(&self.ctx, &mut g.renderer, &camera, &wall_frame, viewport, 1, &path, |enc, color_view| {
                    #[cfg(feature = "egui-ui")]
                    if let Some(painted) = ui_painted.as_mut() {
                        g.ui.paint(&self.ctx.device, &self.ctx.queue, enc, color_view, painted);
                    }
                    #[cfg(not(feature = "egui-ui"))]
                    let _ = (enc, color_view);
                });
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
            if let Some((path, responder)) = asked {
                responder.send(format!("OK: wrote {}", path.display()));
            }
        }
        #[cfg(feature = "egui-ui")]
        {
            let mut changed = false;
            if let (Some(wall), Some(terms)) = (&mut self.wall, &self.terms) {
                changed |= self.panels.sync_after_pass(wall, panel_egui_keys);
                if !panel_verbs.is_empty() {
                    let lookup = screen_lookup(|id| terms.screen(id));
                    for verb in &panel_verbs {
                        let reply = wall.run_with_screens(verb.as_str(), &lookup);
                        self.panels.record_verb_reply(verb, &reply.text);
                    }
                    changed = true;
                }
            }
            if changed {
                self.apply_effects();
            }
        }
        self.frames += 1;
        self.scripted();
    }

    /// What a counted run does on its own, so a script can see the wall act:
    /// `GLYPH_WALL_RUN` is verb lines (separated by `;`) run at frame 10,
    /// `GLYPH_WALL_PASTE` is pasted and `GLYPH_WALL_TYPE` pressed a key at a
    /// time (a newline as Enter) at frame 30, through the same path a
    /// keyboard takes after winit.
    fn scripted(&mut self) {
        if self.exit_after.is_none() {
            return;
        }
        let Some(wall) = &mut self.wall else { return };
        if self.frames == 10 {
            let n = std::env::var("GLYPH_WALL_SPAWN").ok().and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
            for _ in 0..n {
                wall.run("wall.spawn");
            }
            if let Ok(lines) = std::env::var("GLYPH_WALL_RUN") {
                for line in lines.split(';').map(str::trim).filter(|l| !l.is_empty()) {
                    let reply = wall.run(line);
                    println!("wall> {line}\n{}", reply.text);
                }
            }
        }
        if self.frames == 30 {
            if let Ok(text) = std::env::var("GLYPH_WALL_PASTE") {
                wall.paste(&text);
            }
            if let Ok(text) = std::env::var("GLYPH_WALL_TYPE") {
                let terms = self.terms.as_ref();
                let lookup = screen_lookup(|id| terms.and_then(|t| t.screen(id)));
                for ch in text.chars() {
                    let (key, typed) = if ch == '\n' { ("Enter".to_string(), None) } else { (ch.to_string(), Some(ch.to_string())) };
                    wall.key(&KeyEvent { code: String::new(), key, text: typed, pressed: true, ..KeyEvent::default() }, &lookup);
                }
            }
        }
        self.apply_effects();
    }

    fn wheel(&mut self, delta: MouseScrollDelta) {
        let sf = self.scale_factor();
        // winit: positive means the content moves right and down. The web's
        // wheel, which the wall's rules speak: positive deltaY scrolls down,
        // moving the content up. So the sign flips.
        let (dx, dy, mode) = match delta {
            MouseScrollDelta::LineDelta(x, y) => (-f64::from(x), -f64::from(y), DeltaMode::Line),
            MouseScrollDelta::PixelDelta(PhysicalPosition { x, y }) => (-x / sf, -y / sf, DeltaMode::Pixel),
        };
        let ev = WheelEvent { x: self.cursor.x, y: self.cursor.y, delta_x: dx, delta_y: dy, delta_mode: mode, mods: self.mods(), time_ms: self.now_ms() };
        if let Some(wall) = &mut self.wall {
            wall.wheel(&ev);
        }
        self.apply_effects();
        self.redraw();
    }

    fn key(&mut self, ev: &winit::event::KeyEvent, event_loop: &ActiveEventLoop) {
        let key = super::keys::key_event(ev, self.modifiers);
        // Quitting is the window's, not the wall's.
        if key.pressed && key.mods.meta && !key.mods.ctrl && !key.mods.alt && key.code == "KeyQ" {
            event_loop.exit();
            return;
        }
        let (Some(wall), Some(terms)) = (&mut self.wall, &self.terms) else { return };
        #[cfg(feature = "egui-ui")]
        if key.pressed && key.code == "Escape" && !key.mods.ctrl && !key.mods.alt && !key.mods.meta {
            if self.panels.close_open_on_escape(wall) {
                self.apply_effects();
                self.redraw();
                return;
            }
            if self.hud_open {
                self.hud_open = false;
                self.redraw();
                return;
            }
        }
        let lookup = screen_lookup(|id| terms.screen(id));
        let _decision = wall.key(&key, &lookup);
        self.apply_effects();
        self.redraw();
    }

    fn mouse(&mut self, state: ElementState, button: MouseButton) {
        let Some(b) = button_of(button) else { return };
        let bit = Buttons::from_button(b).0;
        let pressed = state == ElementState::Pressed;
        if pressed {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        let ev = self.pointer(Some(b));
        let (Some(wall), Some(terms)) = (&mut self.wall, &self.terms) else { return };
        let lookup = screen_lookup(|id| terms.screen(id));
        if pressed {
            // A right press may be the wall's menu, or the focused program's.
            if b == Button::Right && wall.context_menu(ev.x, ev.y, ev.mods).verdict != Verdict::Program {
                self.apply_effects();
                self.redraw();
                return;
            }
            wall.pointer_down(&ev, &lookup);
        } else {
            wall.pointer_up(&ev, &lookup);
            if b == Button::Left {
                let here = Point::new(ev.x, ev.y);
                let double = self.last_release.is_some_and(|(t, p)| ev.time_ms - t <= DOUBLE_CLICK_MS && (p.x - here.x).abs() <= DOUBLE_CLICK_PX && (p.y - here.y).abs() <= DOUBLE_CLICK_PX);
                if double {
                    wall.pointer_double_click(&ev);
                    self.last_release = None;
                } else {
                    self.last_release = Some((ev.time_ms, here));
                }
            }
        }
        self.apply_effects();
        self.redraw();
    }
}

impl ApplicationHandler<Wake> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_none() {
            self.open(event_loop);
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _wake: Wake) {
        self.serve_control();
        self.pump();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        #[cfg(feature = "egui-ui")]
        let panels_want_keys = self.wall.as_ref().is_some_and(|w| self.panels.wants_keyboard(w));
        #[cfg(feature = "egui-ui")]
        let feed_ui = !matches!(event, WindowEvent::KeyboardInput { .. } | WindowEvent::Ime(_))
            || panels_want_keys
            || self.gfx.as_ref().is_some_and(|g| g.ui.context().egui_wants_keyboard_input());
        #[cfg(feature = "egui-ui")]
        let ui_outcome = match (&mut self.gfx, feed_ui) {
            (Some(g), true) => g.ui.on_window_event(&g.window, &event),
            _ => UiEventOutcome::default(),
        };
        #[cfg(feature = "egui-ui")]
        if ui_outcome.repaint {
            self.redraw();
        }
        #[cfg(feature = "egui-ui")]
        let (ui_pointer, ui_keys) = (
            ui_outcome.consumed || ui_outcome.wants_pointer,
            ui_outcome.consumed || ui_outcome.wants_keyboard || panels_want_keys,
        );
        #[cfg(not(feature = "egui-ui"))]
        let (ui_pointer, ui_keys) = (false, false);

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => self.resize(size.width, size.height),
            WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(g) = &self.gfx {
                    let size = g.window.inner_size();
                    self.resize(size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::CursorMoved { position, .. } => {
                let sf = self.scale_factor();
                self.cursor = Point::new(position.x / sf, position.y / sf);
                if ui_pointer && self.buttons == 0 {
                    return;
                }
                let ev = self.pointer(None);
                if let (Some(wall), Some(terms)) = (&mut self.wall, &self.terms) {
                    let lookup = screen_lookup(|id| terms.screen(id));
                    wall.pointer_move(&ev, &lookup);
                }
                self.apply_effects();
            }
            WindowEvent::MouseInput { state, button, .. } => {
                // A release always reaches the wall, so no gesture is left hanging.
                if ui_pointer && state == ElementState::Pressed {
                    return;
                }
                self.mouse(state, button);
            }
            WindowEvent::MouseWheel { delta, .. } if !ui_pointer => self.wheel(delta),
            WindowEvent::PinchGesture { delta, .. } if !ui_pointer => {
                if let Some(wall) = &mut self.wall {
                    wall.pinch(self.cursor.x, self.cursor.y, 1.0 + delta);
                }
                self.apply_effects();
                self.redraw();
            }
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => {
                #[cfg(feature = "egui-ui")]
                if event.state == ElementState::Pressed && self.modifiers.super_key() && !self.modifiers.control_key() && !self.modifiers.alt_key() && event.physical_key == PhysicalKey::Code(KeyCode::KeyQ) {
                    event_loop.exit();
                    return;
                }
                #[cfg(feature = "egui-ui")]
                if event.state == ElementState::Pressed && !event.repeat && event.physical_key == PhysicalKey::Code(KeyCode::F1) && !ui_outcome.consumed {
                    self.hud_open = !self.hud_open;
                    self.redraw();
                    return;
                }
                if !ui_keys {
                    self.key(&event, event_loop);
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.quit {
            event_loop.exit();
            return;
        }
        match self.exit_after {
            Some(n) if self.frames >= n => event_loop.exit(),
            // A counted run draws without waiting to be asked.
            Some(_) => self.redraw(),
            None => {}
        }
        // Something is still moving (an eased camera, a glide): keep drawing.
        if self.animating {
            self.redraw();
        }
        #[cfg(feature = "egui-ui")]
        if let Some(at) = self.ui_repaint_at {
            if Instant::now() >= at {
                self.ui_repaint_at = None;
                self.redraw();
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(at));
                return;
            }
        }
        event_loop.set_control_flow(ControlFlow::Wait);
    }
}

/// Open the wall and run until its window closes.
pub fn run(ctx: GpuContext, opts: WallOptions) {
    let event_loop = EventLoop::<Wake>::with_user_event().build().expect("event loop creation failed");
    let now = Instant::now();
    let mut app = App {
        ctx,
        opts: Some(opts),
        proxy: event_loop.create_proxy(),
        gfx: None,
        wall: None,
        terms: None,
        control: None,
        pending_shot: None,
        quit: false,
        modifiers: ModifiersState::empty(),
        cursor: Point::new(0.0, 0.0),
        buttons: 0,
        last_release: None,
        render_ids: HashMap::new(),
        start: now,
        last_tick: now,
        animating: false,
        frames: 0,
        exit_after: None,
        #[cfg(feature = "egui-ui")]
        hud_open: std::env::var("GLYPH_WALL_HUD").is_ok_and(|v| !v.is_empty() && v != "0"),
        #[cfg(feature = "egui-ui")]
        panels: super::mount::Panels::new(),
        #[cfg(feature = "egui-ui")]
        ui_repaint_at: None,
    };
    event_loop.run_app(&mut app).expect("event loop error");
    println!("the wall drew {} frames", app.frames);
    if app.exit_after.is_some() {
        // A counted run says what its terminals showed, as text, and where
        // the wall put them.
        if let (Some(wall), Some(terms)) = (&mut app.wall, &app.terms) {
            let described = wall.frame();
            for t in &described.terminals {
                let Some(s) = terms.screen(&t.id) else { continue };
                println!("--- terminal {} ({}x{}) at ({:.1}, {:.1}) z {:.1}{}", t.id, s.cols, s.rows, t.cell_origin.x, t.cell_origin.y, t.z, if t.focused { " focused" } else { "" });
                print!("{}", screen_text(s));
            }
        }
    }
}
