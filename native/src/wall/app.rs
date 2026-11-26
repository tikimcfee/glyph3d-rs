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
use wall_core::geom::Point;
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
use super::cell::CellSelection;
use super::labels::Label;
use super::renderer::{TerminalDraw, WallFrame, WallRenderer, WallStyle, WallTarget, WALL_DEPTH_FORMAT};
use super::session::{News, Sessions};
use super::shapes::Shape;
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

    fn scrollback(&self, id: &str) -> Option<super::describe::TerminalScrollback> {
        match self {
            Terminals::Live(s) => s.scrollback(id).map(|sb| super::describe::TerminalScrollback {
                offset: sb.offset,
                max: sb.max,
                viewing: sb.viewing,
            }),
            Terminals::Fixed(_) => None,
        }
    }
}

/// What exists once there is a window.
///
/// Field order is drop order, and it matters on Wayland: the egui layer
/// (whose clipboard worker borrows the display) and the surface must go
/// before the window (found on workstation, 2026-11-26: a segfault in
/// `wl_proxy_destroy` at every exit).
struct Gfx {
    #[cfg(feature = "egui-ui")]
    ui: WallUi,
    renderer: WallRenderer,
    depth: wgpu::TextureView,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    window: Arc<Window>,
    /// Renderer scale that makes a cell [`WORLD_CELL_HEIGHT`] tall.
    cell_scale: f32,
}

struct App {
    /// Before `ctx`: dropped first, and cleared in `exiting` while the event
    /// loop's display connection is still open.
    gfx: Option<Gfx>,
    ctx: GpuContext,
    opts: Option<WallOptions>,
    proxy: EventLoopProxy<Wake>,
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
    blink_repaint_at: Option<Instant>,
}

fn parse_css_hex(s: &str) -> Option<u32> {
    let h = s.trim().trim_start_matches('#');
    let full = match h.len() {
        3 => h.chars().flat_map(|c| [c, c]).collect::<String>(),
        6 => h.to_string(),
        _ => return None,
    };
    u32::from_str_radix(&full, 16).ok()
}

/// `#rrggbb` (or `#rgb`) as packed opaque sRGB RGBA8; anything else is magenta.
fn css_rgb_u32(s: &str) -> u32 {
    let v = parse_css_hex(s).unwrap_or(0xff00ff);
    term_cells::rgb(((v >> 16) & 0xff) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8)
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
            #[cfg(feature = "egui-ui")]
            ui,
            renderer,
            depth,
            surface,
            config,
            window,
            cell_scale,
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

    /// The clipboard's text. On Linux it is read through the window's own
    /// clipboard, which speaks Wayland; a fresh `arboard` there speaks X11
    /// alone.
    fn read_clipboard(&mut self) -> Option<String> {
        #[cfg(all(target_os = "linux", feature = "egui-ui"))]
        if let Some(g) = &mut self.gfx {
            return g.ui.clipboard_text();
        }
        match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
            Ok(text) => Some(text),
            Err(e) => {
                log::warn!("paste: the clipboard gave no text: {e}");
                None
            }
        }
    }

    fn write_clipboard(&mut self, text: String) {
        #[cfg(all(target_os = "linux", feature = "egui-ui"))]
        if let Some(g) = &mut self.gfx {
            g.ui.set_clipboard_text(text);
            return;
        }
        if let Err(e) = arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
            log::warn!("copy: the clipboard refused the text: {e}");
        }
    }

    fn apply(&mut self, effect: WallEffect) {
        let effect = match effect {
            WallEffect::WriteClipboard { text } => {
                self.write_clipboard(text);
                return;
            }
            WallEffect::ReadClipboard { target } => {
                if let (Some(text), Some(wall)) = (self.read_clipboard(), &mut self.wall) {
                    wall.deliver_clipboard(&target, &text);
                }
                return;
            }
            other => other,
        };
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
                    // wall-core emits +lines for up-into-history; Sessions::scroll takes negative for up.
                    WallEffect::ScrollTerminal { id, lines, cell } => sessions.scroll(&id, lines.saturating_neg(), cell.x, cell.y),
                    WallEffect::RefreshTerminal { id } => sessions.repaint(&id),
                    _ => {}
                }
            }
        }
    }

    /// Run the verb lines `wallctl` sent, and answer each.
    fn serve_control(&mut self) {
        if self.control.is_none() {
            return;
        }
        let mut ran = false;
        while let Some(request) = self.control.as_ref().and_then(wall_control::Server::try_recv) {
            let (line, responder) = request.into_parts();
            // The window's own few verbs: what a script needs that the wall's
            // rules cannot give (pixels, a terminal's text, quitting).
            let argv = wall_core::router::CommandRouter::<()>::parse_line(&line);
            match argv.first().map(String::as_str) {
                Some("app.shot") => {
                    match argv.get(1) {
                        Some(path) if argv.len() == 2 => self.pending_shot = Some((PathBuf::from(path), responder)),
                        _ => {
                            responder.send("ERR: usage: app.shot <path.png>");
                        }
                    }
                    ran = true;
                    continue;
                }
                Some("app.text") => {
                    let reply = match (argv.get(1), &self.terms) {
                        (Some(id), Some(terms)) if argv.len() == 2 => match terms.screen(id) {
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
                            let next = match argv.get(1).map(String::as_str) {
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
            if let Some(parsed) = super::script::parse_script_line(&line) {
                let reply = match parsed {
                    Ok(cmd) => self.run_script(cmd),
                    Err(err) => format!("ERR: {err}"),
                };
                responder.send(reply);
                ran = true;
                continue;
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
    fn draw(&mut self, to_surface: bool) {
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
        let frame = if to_surface {
            match g.surface.get_current_texture() {
                Cst::Success(f) | Cst::Suboptimal(f) => Some(f),
                Cst::Lost | Cst::Outdated => {
                    g.surface.configure(&self.ctx.device, &g.config);
                    g.window.request_redraw();
                    None
                }
                // Covered or asleep: skip surface present, unless an offscreen shot is waiting.
                Cst::Occluded | Cst::Timeout => None,
                Cst::Validation => {
                    log::error!("the wall's surface failed validation; skipping the frame");
                    None
                }
            }
        } else {
            None
        };
        let need_offscreen = self.pending_shot.is_some()
            || (self.exit_after == Some(self.frames + 1) && std::env::var_os("GLYPH_WALL_SHOT").is_some());
        if frame.is_none() && !need_offscreen {
            return;
        }
        let sf = g.window.scale_factor() as f32;
        // The wall's camera is in logical pixels per world unit; the
        // renderer's is in physical ones.
        let camera = WallCamera::new(Vec2::new(described.camera.x as f32, described.camera.y as f32), described.camera.zoom as f32 * sf);
        let viewport = (g.config.width, g.config.height);

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
                selection: t.text_selection.as_ref().map(|sel| {
                    let fg = (!sel.foreground.trim().eq_ignore_ascii_case("keep"))
                        .then(|| css_rgb_u32(&sel.foreground));
                    CellSelection {
                        start_row: sel.r0.min(usize::from(u16::MAX)) as u16,
                        start_col: sel.c0.min(usize::from(u16::MAX)) as u16,
                        end_row: sel.r1.min(usize::from(u16::MAX)) as u16,
                        end_col: sel.c1.min(usize::from(u16::MAX)) as u16,
                        block: sel.block,
                        background: css_rgb_u32(&sel.background),
                        foreground: fg,
                    }
                }),
                hover_link: None,
            });
        }

        let mut shapes: Vec<Shape> = Vec::new();
        let mut labels: Vec<Label<'static>> = Vec::new();
        let paper = {
            let metrics = g.renderer.metrics();
            let family = g.renderer.font_family();
            let mut fonts = super::chrome::ChromeFonts::new(metrics, g.renderer.glyphs_mut().atlas_mut(), family);
            super::describe::describe_frame_chrome_with_scrollback(
                &described,
                &camera,
                viewport,
                sf,
                &mut fonts,
                &mut shapes,
                &mut labels,
                |id| terms.scrollback(id),
            )
        };
        let wall_frame = WallFrame { paper: Some(&paper), terminals: &draws, shapes: &shapes, labels: &labels };
        self.blink_repaint_at = g.renderer.next_cursor_blink(&draws, Instant::now());
        let view = frame.as_ref().map(|f| f.texture.create_view(&Default::default()));
        let mut encoder = view.as_ref().map(|_| self.ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("wall frame") }));
        let stats = match (&view, &mut encoder) {
            (Some(v), Some(enc)) => {
                let target = WallTarget { color_view: v, depth_view: &g.depth, width: g.config.width, height: g.config.height };
                g.renderer.render(&self.ctx.device, &self.ctx.queue, enc, &target, &camera, &wall_frame)
            }
            _ => super::renderer::UploadStats::default(),
        };
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
            if let (Some(painted), Some(v), Some(enc)) = (ui_frame.painted.as_mut(), &view, &mut encoder) {
                g.ui.paint(&self.ctx.device, &self.ctx.queue, enc, v, painted);
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
        if let (Some(enc), Some(frame)) = (encoder, frame) {
            self.ctx.queue.submit([enc.finish()]);
            self.ctx.queue.present(frame);
        }

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

    fn move_cursor(&mut self, x: f64, y: f64) {
        self.cursor = Point::new(x, y);
        let ev = self.pointer(None);
        if let (Some(wall), Some(terms)) = (&mut self.wall, &self.terms) {
            let lookup = screen_lookup(|id| terms.screen(id));
            wall.pointer_move(&ev, &lookup);
        }
        self.apply_effects();
    }

    fn pinch(&mut self, delta: f64) {
        if let Some(wall) = &mut self.wall {
            wall.pinch(self.cursor.x, self.cursor.y, 1.0 + delta);
        }
        self.apply_effects();
        self.redraw();
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

    fn key_input(&mut self, key: KeyEvent) {
        // Quitting is the window's, not the wall's (Command+Q on macOS, Ctrl+Alt+Q elsewhere).
        if key.pressed && key.code == "KeyQ" && key.mods == wall_core::chord::chord_mods(Platform::host()) {
            self.quit = true;
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

    fn key(&mut self, ev: &winit::event::KeyEvent, event_loop: &ActiveEventLoop) {
        let key = super::keys::key_event(ev, self.modifiers);
        self.key_input(key);
        if self.quit {
            event_loop.exit();
        }
    }

    fn run_script(&mut self, cmd: super::script::ScriptCommand) -> String {
        use super::script::{button_label, keys_for_typed_text, ScriptCommand};
        match cmd {
            ScriptCommand::Where => match &mut self.wall {
                Some(wall) => super::script::format_where(&wall.frame(), self.cursor),
                None => "ERR: the wall is not open yet".to_string(),
            },
            ScriptCommand::Move { x, y } => {
                self.move_cursor(x, y);
                format!("OK: moved to ({x:.2}, {y:.2})")
            }
            ScriptCommand::Down { button, mods } => {
                let prev = self.modifiers;
                if let Some(m) = mods {
                    self.modifiers = m;
                }
                self.mouse(ElementState::Pressed, button);
                if mods.is_some() {
                    self.modifiers = prev;
                }
                format!("OK: down {} at ({:.2}, {:.2})", button_label(button), self.cursor.x, self.cursor.y)
            }
            ScriptCommand::Up { button, mods } => {
                let prev = self.modifiers;
                if let Some(m) = mods {
                    self.modifiers = m;
                }
                self.mouse(ElementState::Released, button);
                if mods.is_some() {
                    self.modifiers = prev;
                }
                format!("OK: up {} at ({:.2}, {:.2})", button_label(button), self.cursor.x, self.cursor.y)
            }
            ScriptCommand::Click { x, y, button, mods } => {
                let prev = self.modifiers;
                self.modifiers = mods;
                self.last_release = None;
                self.move_cursor(x, y);
                self.mouse(ElementState::Pressed, button);
                self.mouse(ElementState::Released, button);
                self.modifiers = prev;
                format!("OK: clicked {} at ({x:.2}, {y:.2})", button_label(button))
            }
            ScriptCommand::DblClick { x, y } => {
                self.last_release = None;
                self.move_cursor(x, y);
                self.mouse(ElementState::Pressed, MouseButton::Left);
                self.mouse(ElementState::Released, MouseButton::Left);
                self.mouse(ElementState::Pressed, MouseButton::Left);
                self.mouse(ElementState::Released, MouseButton::Left);
                format!("OK: dblclick at ({x:.2}, {y:.2})")
            }
            ScriptCommand::Drag { x0, y0, x1, y1, steps, button, mods } => {
                let prev = self.modifiers;
                self.modifiers = mods;
                self.last_release = None;
                self.move_cursor(x0, y0);
                self.mouse(ElementState::Pressed, button);
                for i in 1..=steps {
                    let t = i as f64 / steps as f64;
                    self.move_cursor(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t);
                }
                self.mouse(ElementState::Released, button);
                self.modifiers = prev;
                format!("OK: dragged {} ({x0:.2}, {y0:.2}) -> ({x1:.2}, {y1:.2}) in {steps} steps", button_label(button))
            }
            ScriptCommand::Wheel { x, y, dx, dy, mods } => {
                let prev = self.modifiers;
                self.modifiers = mods;
                self.move_cursor(x, y);
                let sf = self.scale_factor();
                self.wheel(MouseScrollDelta::PixelDelta(PhysicalPosition::new(dx * sf, dy * sf)));
                self.modifiers = prev;
                format!("OK: wheel ({dx:.2}, {dy:.2}) at ({x:.2}, {y:.2})")
            }
            ScriptCommand::Pinch { x, y, factor } => {
                self.move_cursor(x, y);
                self.pinch(factor - 1.0);
                format!("OK: pinch {factor:.4} at ({x:.2}, {y:.2})")
            }
            ScriptCommand::Key(k) => {
                let prev = self.modifiers;
                self.modifiers = k.mods;
                let down = super::keys::key_from_parts(k.physical, &k.logical, k.text.as_deref(), ElementState::Pressed, false, self.modifiers);
                let up = super::keys::key_from_parts(k.physical, &k.logical, None, ElementState::Released, false, self.modifiers);
                self.key_input(down);
                self.key_input(up);
                self.modifiers = prev;
                "OK: key".to_string()
            }
            ScriptCommand::Type(text) => {
                let seq = keys_for_typed_text(&text);
                let count = seq.len();
                let prev = self.modifiers;
                for k in seq {
                    self.modifiers = k.mods;
                    let down = super::keys::key_from_parts(k.physical, &k.logical, k.text.as_deref(), ElementState::Pressed, false, self.modifiers);
                    let up = super::keys::key_from_parts(k.physical, &k.logical, None, ElementState::Released, false, self.modifiers);
                    self.key_input(down);
                    self.key_input(up);
                }
                self.modifiers = prev;
                format!("OK: typed {count} key(s)")
            }
        }
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
        let secondary = wall_core::menu::is_secondary_press(b, ev.mods, Platform::host());
        let (Some(wall), Some(terms)) = (&mut self.wall, &self.terms) else { return };
        let lookup = screen_lookup(|id| terms.screen(id));
        if pressed {
            // A secondary press (right-click, or Ctrl+left-click on macOS) may be
            // the wall's menu, or the focused program's.
            if secondary && wall.context_menu(ev.x, ev.y, ev.mods).verdict != Verdict::Program {
                self.apply_effects();
                self.redraw();
                return;
            }
            wall.pointer_down(&ev, &lookup);
        } else {
            wall.pointer_up(&ev, &lookup);
            if b == Button::Left && !secondary {
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
            WindowEvent::RedrawRequested => self.draw(true),
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::CursorMoved { position, .. } => {
                let sf = self.scale_factor();
                let (x, y) = (position.x / sf, position.y / sf);
                if ui_pointer && self.buttons == 0 {
                    self.cursor = Point::new(x, y);
                    return;
                }
                self.move_cursor(x, y);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                // A release always reaches the wall, so no gesture is left hanging.
                if ui_pointer && state == ElementState::Pressed {
                    return;
                }
                self.mouse(state, button);
            }
            WindowEvent::MouseWheel { delta, .. } if !ui_pointer => self.wheel(delta),
            WindowEvent::PinchGesture { delta, .. } if !ui_pointer => self.pinch(delta),
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

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // The egui layer, the surface and the window go while the event
        // loop's display connection is still alive (see `Gfx`).
        self.control = None;
        self.gfx = None;
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
        // Something is still moving (an eased camera, a glide) or an offscreen
        // shot is pending: ask for a redraw, and fall back to a surface-free
        // tick/shot if macOS suppresses RedrawRequested while occluded.
        if self.pending_shot.is_some() {
            self.draw(false);
        }
        if self.animating {
            self.redraw();
            if self.last_tick.elapsed() >= std::time::Duration::from_millis(32) {
                self.draw(false);
            }
            if self.animating {
                event_loop.set_control_flow(ControlFlow::WaitUntil(
                    Instant::now() + std::time::Duration::from_millis(16),
                ));
                return;
            }
        }
        let now = Instant::now();
        let mut next_repaint: Option<Instant> = None;
        #[cfg(feature = "egui-ui")]
        if let Some(deadline) = self.ui_repaint_at {
            if now >= deadline {
                self.ui_repaint_at = None;
                self.redraw();
            } else {
                next_repaint = Some(deadline);
            }
        }
        if let Some(deadline) = self.blink_repaint_at {
            if now >= deadline {
                self.blink_repaint_at = None;
                self.redraw();
            } else {
                next_repaint = Some(next_repaint.map_or(deadline, |d| d.min(deadline)));
            }
        }
        if let Some(deadline) = next_repaint {
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
            return;
        }
        event_loop.set_control_flow(ControlFlow::Wait);
    }
}

/// Open the wall and run until its window closes.
pub fn run(ctx: GpuContext, opts: WallOptions) {
    let event_loop = EventLoop::<Wake>::with_user_event().build().expect("event loop creation failed");
    let now = Instant::now();
    let mut app = App {
        gfx: None,
        ctx,
        opts: Some(opts),
        proxy: event_loop.create_proxy(),
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
        blink_repaint_at: None,
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
