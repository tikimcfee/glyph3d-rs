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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use glam::Vec2;
use term_cells::Screen;
use wall_core::config::OptionAsAlt;
use wall_core::geom::Point;
use wall_core::input::{Button, KeyEvent, Mods, Platform};
use wall_core::keymap::FieldFocus;
#[cfg(feature = "egui-ui")]
use wall_core::keymap::KeyDecision;
use wall_core::layout::{ResizeCursor, Size};
use wall_core::menu::Verdict;
use wall_core::model::{CellGeometry, WallEffect};
use wall_core::pointer::{Buttons, CursorIcon, PointerEvent, WheelEvent};
use wall_core::scroll::DeltaMode;
use wall_core::status::TermdState;
use wall_core::wall::{screen_lookup, CharWidths, ScreenFillDelta, TerminalAppeared, Wall, WallFrame as Described, WallOptions as RulesOptions};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
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
    /// Connecting/spawning `g3d-termd` off the window thread.
    Connecting,
    Live(Box<Sessions>),
    /// Screens that never change (the test pattern), by id.
    Fixed(BTreeMap<String, Screen>),
}

impl Terminals {
    fn screen(&self, id: &str) -> Option<&Screen> {
        match self {
            Terminals::Connecting => None,
            Terminals::Live(s) => s.screen(id),
            Terminals::Fixed(m) => m.get(id),
        }
    }

    fn scrollback(&self, id: &str) -> Option<super::describe::TerminalScrollback> {
        match self {
            Terminals::Connecting | Terminals::Fixed(_) => None,
            Terminals::Live(s) => s.scrollback(id).map(|sb| super::describe::TerminalScrollback {
                offset: sb.offset,
                max: sb.max,
                viewing: sb.viewing,
            }),
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
    ui: Option<WallUi>,
    renderer: WallRenderer,
    depth: wgpu::TextureView,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    window: Arc<Window>,
    /// Renderer scale that makes a cell [`WORLD_CELL_HEIGHT`] tall.
    cell_scale: f32,
}

#[cfg(feature = "egui-ui")]
impl Gfx {
    fn ensure_ui(&mut self, device: &wgpu::Device) -> &mut WallUi {
        self.ui.get_or_insert_with(|| {
            let _s = super::startup::span("egui_init");
            WallUi::new(device, &self.window, self.config.format)
        })
    }
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
    /// Next wake deadline while `termd` is in a restarting/unreachable trouble state
    /// so the rules' notice grace timers advance even while the window is idle.
    termd_tick_at: Option<Instant>,
    /// Background `g3d-termd` connect/spawn/list/adopt in flight at startup.
    pending_termd: Option<super::startup::PendingTermd>,
    /// True while waiting for initial `g3d-termd` startup (shows `"starting termd..."` notice).
    starting_termd: bool,
    occluded: bool,
    wakes: Arc<WakeGate>,
    perf: super::perf::WallPerf,
    /// True while an OS dead-key or IME preedit composition is active (`e.isComposing`).
    ime_composing: bool,
    /// Last quantized logical `(x, y, w, h)` sent to `Window::set_ime_cursor_area`.
    last_ime_area: Option<(i32, i32, u32, u32)>,
    #[cfg(target_os = "macos")]
    macos_option_as_alt: Option<winit::platform::macos::OptionAsAlt>,
    /// Physical `code`s whose press reached `key_input` and have not yet been released.
    held_codes: HashSet<String>,
    /// Last text written by `WallEffect::WriteClipboard` (inspected via `app.clipboard`).
    last_copied: Option<String>,
    /// Process-local clipboard used when `GLYPH_WALL_CLIPBOARD=memory` is set so automated
    /// smoke tests never read or overwrite a user's live OS clipboard.
    memory_clipboard: Option<String>,
    use_memory_clipboard: bool,
}

#[derive(Default)]
struct WakeGate {
    pending: std::sync::atomic::AtomicBool,
    redraw_armed: std::sync::atomic::AtomicBool,
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

/// Whether `key` is a wall-level chord that must bypass a focused panel search box
/// (`SettingsPanel`, `HelpPanel`, `Palette`) when no context menu is open.
fn chord_bypasses_panel_input(platform: Platform, key: &KeyEvent) -> bool {
    if wall_core::chord::focus_history_dir(key).is_some()
        || wall_core::chord::group_cycle_dir(key).is_some()
    {
        return true;
    }
    if !wall_core::chord::is_chord(platform, key) {
        return false;
    }
    // On macOS, `⌘A` (Select All), `⌘X` (Cut), and `⌘+Arrow` (line start/end) belong
    // to a focused `egui::TextEdit` search/input box (`WallKeys.js` `inField` rule).
    if matches!(platform, Platform::Mac)
        && matches!(
            key.code.as_str(),
            "KeyA" | "KeyX" | "ArrowLeft" | "ArrowRight" | "ArrowUp" | "ArrowDown"
        )
    {
        return false;
    }
    wall_core::keymap::KEYMAP.iter().any(|c| c.code == key.code)
        || matches!(key.code.as_str(), "KeyQ" | "F1")
}

#[cfg(feature = "egui-ui")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PointerTarget {
    Ui,
    Wall,
}

/// Decide whether a pointer event at logical window point `pos` belongs to an
/// open `egui` panel (`PointerTarget::Ui`) or to the canvas underneath
/// (`PointerTarget::Wall`).
///
/// - While a wall button press/drag is in flight (`wall_buttons != 0`), moves,
///   wheels, and releases stay with the wall so no gesture is stranded.
/// - While `egui` is actively dragging a widget (`ctx.is_using_pointer()`),
///   events stay with `egui` even if the cursor leads the panel box.
/// - Otherwise, any event over an active `egui` `Area` (`ctx.layer_id_at(pos)`
///   above `Order::Background`, or `egui_wants_pointer_input` / `is_pointer_over_area`
///   when `pos` matches `ctx.pointer_latest_pos()`) goes to `egui`, and
///   everything else goes to the wall.
#[cfg(feature = "egui-ui")]
fn pointer_target(
    ui_active: bool,
    ctx: &egui::Context,
    pos: Point,
    wall_buttons: u8,
) -> PointerTarget {
    if wall_buttons != 0 || !ui_active {
        return PointerTarget::Wall;
    }
    let p = egui::pos2(pos.x as f32, pos.y as f32);
    let over_area = ctx
        .layer_id_at(p)
        .is_some_and(|layer| layer.order != egui::Order::Background);
    let over_settled = ctx.pointer_latest_pos().is_some_and(|lp| {
        (lp - p).length_sq() <= 1.0
            && (ctx.egui_wants_pointer_input() || ctx.is_pointer_over_egui())
    });
    if ctx.egui_is_using_pointer() || over_area || over_settled {
        PointerTarget::Ui
    } else {
        PointerTarget::Wall
    }
}

fn epoch_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
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
            if !self.occluded {
                self.wakes.redraw_armed.store(true, std::sync::atomic::Ordering::SeqCst);
            }
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

    #[cfg(feature = "egui-ui")]
    fn pointer_goes_to_ui(&self, ui_outcome: &UiEventOutcome) -> bool {
        if self.buttons != 0 {
            return false;
        }
        if ui_outcome.consumed {
            return true;
        }
        self.gfx.as_ref().and_then(|g| g.ui.as_ref()).is_some_and(|ui| {
            pointer_target(ui.is_active(), ui.context(), self.cursor, self.buttons)
                == PointerTarget::Ui
        })
    }

    /// Pull the connected daemon's live `status` document (or `WELCOME` metadata
    /// on first connect if `status` is unavailable) into [`Wall::on_termd_status`].
    fn refresh_termd_status(&mut self) {
        let (Some(wall), Some(Terminals::Live(sessions))) = (&mut self.wall, &self.terms) else {
            return;
        };
        if !wall.model().connected() {
            return;
        }
        let now_ms = epoch_now_ms();
        let doc = match sessions.hub().status() {
            Ok(val) => val,
            Err(_) => {
                // Never overwrite a real status document (or `had_terminals`) with an empty
                // synthetic doc when the daemon is disconnecting or shutting down.
                if wall.termd().doc().is_some() {
                    return;
                }
                let welcome = sessions.hub().welcome();
                let terms: Vec<serde_json::Value> = sessions
                    .ids()
                    .map(|id| {
                        let (cols, rows) = sessions
                            .screen(id)
                            .map_or((0, 0), |s| (s.cols, s.rows));
                        serde_json::json!({ "id": id, "cols": cols, "rows": rows })
                    })
                    .collect();
                serde_json::json!({
                    "daemon": {
                        "version": welcome.daemon,
                        "engine": welcome.engine,
                        "ns": welcome.ns,
                        "pid": welcome.pid,
                    },
                    "terminals": terms,
                })
            }
        };
        let _ = wall.on_termd_status(doc, now_ms);
    }

    fn open(&mut self, event_loop: &ActiveEventLoop) {
        let opts = self.opts.take().expect("the window opens once");
        self.exit_after = opts.exit_after;

        let proxy_ctl = self.proxy.clone();
        let gate_ctl = self.wakes.clone();
        let control_wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if !gate_ctl.pending.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let _ = proxy_ctl.send_event(Wake);
            }
        });
        // Single-owner gate per namespace: refuse before creating a second GPU
        // window, opening a second `WallStore` writer, or stealing `Role::Own`
        // size ownership from the running wall window.
        if let Some(ns) = &opts.namespace {
            let _s = super::startup::span("control_socket_bind");
            match wall_control::Server::bind(&wall_control::Target::namespace(ns.clone()), control_wake.clone()) {
                Ok(server) => self.control = Some(server),
                Err(wall_control::Error::AlreadyRunning { path, .. }) => {
                    eprintln!(
                        "error: another wall window is already open on namespace `{ns}` ({}); close it first or choose another namespace with --wall-ns",
                        path.display()
                    );
                    std::process::exit(1);
                }
                Err(e) => log::warn!("the wall's control socket is not open, so wallctl cannot reach this window: {e}"),
            }
        }

        // The terminals: coalesce duplicate queued `Wake`s and suppress wakes
        // while a surface redraw is already armed (`draw` pumps before building).
        let proxy_sess = self.proxy.clone();
        let gate_sess = self.wakes.clone();
        let session_wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if !gate_sess.redraw_armed.load(std::sync::atomic::Ordering::SeqCst)
                && !gate_sess.pending.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                let _ = proxy_sess.send_event(Wake);
            }
        });
        // Start `g3d-termd` connect/spawn/list/adopt off the window thread right away so it
        // overlaps macOS AppKit window creation and never blocks Frame 1.
        let pending_termd = opts
            .namespace
            .as_ref()
            .map(|ns| super::startup::PendingTermd::spawn(ns.clone(), session_wake));

        let window = {
            let _s = super::startup::span("window_create");
            let w = Arc::new(
                event_loop
                    .create_window(Window::default_attributes().with_title("glyph3d — the wall").with_inner_size(winit::dpi::LogicalSize::new(1600.0, 1000.0)))
                    .expect("window creation failed"),
            );
            w.set_ime_allowed(true);
            w
        };
        let (surface, format, config, depth) = {
            let _s = super::startup::span("surface_configure");
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
            (surface, format, config, depth)
        };

        let mut renderer = if self.exit_after.is_some() {
            WallRenderer::new(&self.ctx.device, &self.ctx.queue, &opts.emoji_sheet, format, WallStyle::default())
        } else {
            WallRenderer::new_fast(
                &self.ctx.device,
                &self.ctx.queue,
                &self.ctx.adapter,
                &opts.emoji_sheet,
                format,
                WallStyle::default(),
                control_wake,
            )
        };
        let metrics = renderer.metrics();
        let cell_scale = WORLD_CELL_HEIGHT / metrics.cell_height;
        renderer.style_mut().panel_padding = PANEL_PAD / cell_scale;
        let sf = window.scale_factor();

        // The rules, at the renderer's cell size and the window's viewport.
        let namespace = opts.namespace.clone().unwrap_or_else(|| "pattern".to_string());
        let rules = RulesOptions {
            geometry: CellGeometry::with_padding(f64::from(metrics.cell_width * cell_scale), f64::from(metrics.cell_height * cell_scale), f64::from(PANEL_PAD), f64::from(PANEL_PAD)),
            char_widths: CharWidths::from_unit_width(f64::from(metrics.cell_width / metrics.cell_height)),
            platform: Platform::host(),
            viewport: Size::new(f64::from(config.width) / sf, f64::from(config.height) / sf),
            scale_factor: sf,
            namespace: namespace.clone(),
            ..RulesOptions::default()
        };
        let mut wall = {
            let _s = super::startup::span("wall_store_load");
            if opts.namespace.is_some() { Wall::open(store_dir(), namespace.clone(), rules) } else { Wall::in_memory(rules) }
        };

        let terms = if opts.namespace.is_some() {
            Terminals::Connecting
        } else {
            let screen = super::test_pattern::build_primary_screen();
            wall.terminal_appeared(TerminalAppeared::new("pattern", u32::from(screen.cols), u32::from(screen.rows)).with_title(screen.title.clone()));
            wall.run("wall.fit");
            Terminals::Fixed(BTreeMap::from([("pattern".to_string(), screen)]))
        };

        self.gfx = Some(Gfx {
            #[cfg(feature = "egui-ui")]
            ui: None,
            renderer,
            depth,
            surface,
            config,
            window,
            cell_scale,
        });
        self.wall = Some(wall);
        self.terms = Some(terms);
        self.starting_termd = pending_termd.is_some();
        self.pending_termd = pending_termd;
        self.sync_font_and_text_knobs(None);
        self.apply_effects();
        self.serve_control();
        // Paint and present Frame 1 (drafting paper) immediately — before waiting on any
        // `termd` spawn/adopt/status RPC, egui init, or background shapes/cells pipelines.
        if self.exit_after.is_none() {
            self.draw(true);
        }
        self.poll_pending_termd();
        self.sync_macos_option_as_alt();
        self.perf.mark_opened(&self.proxy);
        self.redraw();
    }

    /// Attach `g3d-termd` sessions once the background connect/spawn/list/adopt thread completes.
    fn poll_pending_termd(&mut self) -> bool {
        let Some(res) = self.pending_termd.as_ref().and_then(|p| p.try_take()) else {
            return false;
        };
        self.pending_termd = None;
        self.starting_termd = false;
        match res {
            Ok(ready) => {
                let welcome = ready.sessions.hub().welcome().clone();
                self.terms = Some(Terminals::Live(Box::new(ready.sessions)));
                if let Some(wall) = &mut self.wall {
                    wall.on_termd_welcome(
                        welcome.instance.as_deref().unwrap_or(""),
                        &welcome.features,
                        epoch_now_ms(),
                    );
                    wall.set_live_terminals(&ready.live_ids);
                    for (id, cols, rows) in ready.adopted {
                        wall.terminal_appeared(TerminalAppeared::adopted(id, u32::from(cols), u32::from(rows)));
                    }
                    wall.set_connected(true);
                    if ready.live_ids.is_empty() {
                        wall.run("wall.spawn");
                    }
                }
                self.apply_effects();
                if let Some(wall) = &mut self.wall {
                    wall.run("wall.fit");
                }
                self.apply_effects();
                self.refresh_termd_status();
                self.pump(false);
                if let Ok(hover_id) = std::env::var("GLYPH_WALL_HOVER") {
                    let hover_pt = self.wall.as_mut().and_then(|wall| {
                        let f = wall.frame();
                        let bar = f.title_bars.iter().find(|b| b.terminal_id == hover_id)?;
                        let sb = f.camera.to_screen_box(bar.rect, f.viewport.w, f.viewport.h);
                        Some(Point::new(0.5 * (sb.left + sb.right), 0.5 * (sb.top + sb.bottom)))
                    });
                    if let Some(pt) = hover_pt {
                        self.cursor = pt;
                        let ev = self.pointer(None);
                        if let (Some(wall), Some(terms)) = (&mut self.wall, &self.terms) {
                            wall.pointer_move(&ev, &screen_lookup(|id| terms.screen(id)));
                        }
                        self.apply_effects();
                    }
                }
                self.redraw();
            }
            Err(msg) => {
                eprintln!("error: {msg}");
                if let Some(wall) = &mut self.wall {
                    wall.notify(&msg);
                }
                self.quit = true;
            }
        }
        self.sync_macos_option_as_alt();
        true
    }

    fn sync_macos_option_as_alt(&mut self) {
        #[cfg(target_os = "macos")]
        {
            use winit::platform::macos::{OptionAsAlt as WinitOptAlt, WindowExtMacOS};
            let Some(g) = &self.gfx else { return };
            let ui_text_focus = {
                #[cfg(feature = "egui-ui")]
                {
                    self.wall.as_ref().is_some_and(|w| self.panels.wants_keyboard(w))
                        || g.ui.as_ref().is_some_and(|ui| ui.context().egui_wants_keyboard_input())
                }
                #[cfg(not(feature = "egui-ui"))]
                {
                    false
                }
            };
            let want = match self.wall.as_ref().map(|w| w.config().option_as_alt) {
                Some(OptionAsAlt::True) if !ui_text_focus => WinitOptAlt::Both,
                _ => WinitOptAlt::None,
            };
            if self.macos_option_as_alt != Some(want) {
                g.window.set_option_as_alt(want);
                self.macos_option_as_alt = Some(want);
            }
        }
    }

    fn sync_ime_cursor_area(&mut self, described: &Described) {
        let (Some(g), Some(terms)) = (&self.gfx, &self.terms) else { return };
        #[cfg(feature = "egui-ui")]
        if g.ui.as_ref().is_some_and(|ui| ui.context().egui_wants_keyboard_input()) {
            return;
        }
        let Some(focused) = described.terminals.iter().find(|t| t.focused) else { return };
        let Some(screen) = terms.screen(&focused.id) else { return };
        let sb = described.camera.to_screen_box(focused.body_rect, described.viewport.w, described.viewport.h);
        let pad_px = f64::from(PANEL_PAD) * focused.scale * described.camera.zoom;
        let inner_w = ((sb.right - sb.left) - 2.0 * pad_px).max(1.0);
        let inner_h = ((sb.bottom - sb.top) - 2.0 * pad_px).max(1.0);
        let cw = (inner_w / f64::from(screen.cols.max(1))).max(1.0);
        let ch = (inner_h / f64::from(screen.rows.max(1))).max(1.0);
        let cx = (sb.left + pad_px + f64::from(screen.cursor.col) * cw).clamp(0.0, described.viewport.w.max(1.0));
        let cy = (sb.top + pad_px + f64::from(screen.cursor.row) * ch).clamp(0.0, described.viewport.h.max(1.0));
        let area = (
            cx.round() as i32,
            cy.round() as i32,
            cw.round().max(1.0) as u32,
            ch.round().max(1.0) as u32,
        );
        if self.last_ime_area != Some(area) {
            self.last_ime_area = Some(area);
            g.window.set_ime_cursor_area(
                LogicalPosition::new(f64::from(area.0), f64::from(area.1)),
                LogicalSize::new(f64::from(area.2), f64::from(area.3)),
            );
        }
    }

    /// Apply font family, size, line-height, and text-style knobs from `wall.config()` to the
    /// renderer and cell geometry, re-wrapping terminals to preserve panel footprints.
    fn sync_font_and_text_knobs(&mut self, changed_key: Option<&str>) {
        let (Some(g), Some(wall)) = (&mut self.gfx, &mut self.wall) else { return };
        let cfg = wall.config();
        let family = cfg.font_family.clone();
        let font_size = cfg.font_size;
        let line_height = cfg.line_height;
        g.renderer.style_mut().line_thickness_scale = cfg.line_thickness as f32;
        g.renderer.style_mut().underline_raise = cfg.underline_raise as f32;
        g.renderer.style_mut().curly_amplitude = cfg.curly_amplitude as f32;
        g.renderer.style_mut().dim_alpha = cfg.dim_alpha as f32;

        let font_changed = matches!(changed_key, None | Some("fontFamily" | "fontSize" | "lineHeight"));
        if !font_changed {
            return;
        }
        if matches!(changed_key, None | Some("fontFamily")) {
            if let Err(err) = g.renderer.reload_font(&self.ctx.device, &self.ctx.queue, &family, line_height) {
                let kept = g.renderer.glyphs().active_family_name().to_string();
                log::warn!("wall font '{family}': {err}; keeping '{kept}'");
                wall.notify(&format!("font '{family}' is not installed — keeping {kept}"));
                return;
            }
        } else if changed_key == Some("lineHeight") {
            let active = g.renderer.glyphs().active_family_name().to_string();
            if let Err(err) = g.renderer.reload_font(&self.ctx.device, &self.ctx.queue, &active, line_height) {
                log::warn!("wall lineHeight {line_height} reload failed: {err}");
                return;
            }
        }
        let metrics = g.renderer.metrics();
        let size_factor = (font_size / wall_core::config::FONT_SIZE_DEFAULT).max(0.1) as f32;
        let active_lh = g.renderer.glyphs().line_height_scale() as f32;
        let base_cell_h = (metrics.cell_height / active_lh.max(0.1)).max(1e-4);
        g.cell_scale = (WORLD_CELL_HEIGHT * size_factor) / base_cell_h;
        g.renderer.style_mut().panel_padding = PANEL_PAD / g.cell_scale;
        let geom = CellGeometry::with_padding(
            f64::from(metrics.cell_width * g.cell_scale),
            f64::from(metrics.cell_height * g.cell_scale),
            f64::from(PANEL_PAD),
            f64::from(PANEL_PAD),
        );
        wall.set_geometry_preserving_panels(geom);
        wall.set_char_widths(CharWidths::from_unit_width(f64::from(
            metrics.cell_width / metrics.cell_height.max(1e-6),
        )));
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
        if self.use_memory_clipboard {
            return self.memory_clipboard.clone();
        }
        #[cfg(all(target_os = "linux", feature = "egui-ui"))]
        if let Some(ui) = self.gfx.as_mut().and_then(|g| g.ui.as_mut()) {
            return ui.clipboard_text();
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
        self.last_copied = Some(text.clone());
        if self.use_memory_clipboard {
            self.memory_clipboard = Some(text);
            return;
        }
        #[cfg(all(target_os = "linux", feature = "egui-ui"))]
        if let Some(ui) = self.gfx.as_mut().and_then(|g| g.ui.as_mut()) {
            ui.set_clipboard_text(text);
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
            WallEffect::ConfigKnobChanged { key, value } => {
                log::debug!("wall knob {key} is now {value:?}");
                if key == "optionAsAlt" {
                    self.sync_macos_option_as_alt();
                }
                self.sync_font_and_text_knobs(Some(&key));
                self.redraw();
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
            other => {
                let Terminals::Live(sessions) = terms else { return };
                match other {
                    WallEffect::SpawnTerminal { argv, cols, rows, .. } => {
                        match sessions.spawn(argv, cols.min(u32::from(u16::MAX)) as u16, rows.min(u32::from(u16::MAX)) as u16) {
                            Ok((id, cols, rows)) => {
                                wall.terminal_appeared(TerminalAppeared::new(id, u32::from(cols), u32::from(rows)));
                            }
                            Err(e) => log::error!("the wall could not start a terminal: {e}"),
                        }
                        self.refresh_termd_status();
                    }
                    WallEffect::KillTerminal { id } => {
                        sessions.kill(&id);
                        self.refresh_termd_status();
                    }
                    WallEffect::ResizeTerminal { id, cols, rows } => sessions.resize(&id, cols.min(u32::from(u16::MAX)) as u16, rows.min(u32::from(u16::MAX)) as u16),
                    WallEffect::SendKey { id, key } => sessions.key(&id, &key),
                    WallEffect::PasteTerminal { id, text } => sessions.paste(&id, &text),
                    WallEffect::SendMouse { id, event } => sessions.mouse(&id, &event),
                    // wall-core emits +lines for up-into-history; Sessions::scroll takes negative for up.
                    WallEffect::ScrollTerminal { id, lines, cell } => sessions.scroll(&id, lines.saturating_neg(), cell.x, cell.y),
                    WallEffect::ScrollTerminalLive { id } => sessions.scroll_live(&id),
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
                Some("app.perf") => {
                    let reset = match argv.get(1).map(String::as_str) {
                        None => false,
                        Some("reset") if argv.len() == 2 => true,
                        _ => {
                            responder.send("ERR: usage: app.perf [reset]");
                            continue;
                        }
                    };
                    responder.send(self.perf.control_summary(reset));
                    continue;
                }
                Some("app.state") => {
                    let reply = match &self.wall {
                        Some(wall) => {
                            let snap = wall.model().snapshot();
                            let notice = snap.notice.or_else(|| wall.status_notice().map(|n| n.text));
                            let terms_desc: Vec<_> = snap
                                .terminals
                                .iter()
                                .map(|t| {
                                    let pos = wall
                                        .model()
                                        .terminal(&t.id)
                                        .map_or((0.0, 0.0), |e| {
                                            ((e.position.x * 10.0).round() / 10.0, (e.position.y * 10.0).round() / 10.0)
                                        });
                                    (t.id.clone(), t.name.clone(), pos, (t.cols, t.rows))
                                })
                                .collect();
                            format!(
                                "connected={} focused={:?} notice={:?} terminals={:?}",
                                snap.connected, snap.focused_id, notice, terms_desc
                            )
                        }
                        None => "ERR: the wall is not open yet".to_string(),
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
                #[cfg(feature = "egui-ui")]
                Some("app.hud") => {
                    let next = match argv.get(1).map(String::as_str) {
                        Some("open") => true,
                        Some("close") => false,
                        Some("toggle") | None => !self.hud_open,
                        Some(other) => {
                            responder.send(format!("ERR: takes toggle|open|close, not '{other}'"));
                            continue;
                        }
                    };
                    self.hud_open = next;
                    responder.send(format!("OK: hud {}", if next { "open" } else { "closed" }));
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
            self.draw(false);
            self.redraw();
        }
    }

    /// Take in what the terminals' reader thread queued.
    fn pump(&mut self, request_redraw: bool) {
        let _perf = self.perf.pump_scope();
        let (Some(wall), Some(Terminals::Live(sessions))) = (&mut self.wall, &mut self.terms) else { return };
        let (changed, news) = sessions.pump();
        let mut heard = false;
        let mut ended_any = false;
        for n in news {
            match n {
                News::Filled { id, resized, scrolled_top, within_margins, rewritten_cols } => {
                    super::startup::mark_once("first_repaint_applied");
                    if let Some(screen) = sessions.screen(&id) {
                        heard |= wall.terminal_screen_filled(
                            &id,
                            screen,
                            ScreenFillDelta { resized, scrolled_top, within_margins, rewritten_cols: &rewritten_cols },
                        );
                    }
                }
                News::Resized { id, cols, rows } => {
                    wall.terminal_resized(&id, u32::from(cols), u32::from(rows));
                    heard = true;
                }
                News::Title { id, title } => {
                    wall.terminal_title(&id, title);
                    heard = true;
                }
                News::Modes { id, modes } => {
                    wall.terminal_modes(&id, modes);
                    heard = true;
                }
                News::Ended { id } => {
                    wall.terminal_ended(&id);
                    heard = true;
                    ended_any = true;
                }
                News::Disconnected { message } => {
                    log::error!("termd is gone: {message}");
                    wall.set_connected(false);
                    wall.on_termd_disconnected(false, epoch_now_ms());
                    heard = true;
                }
            }
        }
        if let Some(live_ids) = sessions.take_reconnected() {
            log::info!("termd came back ({} terminal(s))", live_ids.len());
            let welcome = sessions.hub().welcome();
            wall.on_termd_welcome(
                welcome.instance.as_deref().unwrap_or(""),
                &welcome.features,
                epoch_now_ms(),
            );
            wall.dismiss_termd_lost();
            if !live_ids.is_empty() {
                wall.set_live_terminals(&live_ids);
            }
            for id in &live_ids {
                match sessions.adopt(id) {
                    Ok((cols, rows)) => {
                        wall.terminal_appeared(TerminalAppeared::adopted(id.clone(), u32::from(cols), u32::from(rows)));
                    }
                    Err(e) => log::error!("{e}"),
                }
            }
            wall.set_connected(true);
            heard = true;
            ended_any = true;
        }
        for id in sessions.take_external_appeared() {
            match sessions.adopt(&id) {
                Ok((cols, rows)) => {
                    wall.terminal_appeared(TerminalAppeared::adopted(id, u32::from(cols), u32::from(rows)));
                    heard = true;
                }
                Err(e) => log::error!("{e}"),
            }
        }
        if ended_any {
            self.refresh_termd_status();
        }
        if heard {
            self.apply_effects();
        }
        if request_redraw && (changed || heard) {
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
        self.wakes.redraw_armed.store(false, std::sync::atomic::Ordering::SeqCst);
        self.pump(false);
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f64().min(1.0 / 30.0);
        self.last_tick = now;
        let now_ms = self.now_ms();
        let Some(wall) = &mut self.wall else { return };
        let moving = wall.tick(dt, now_ms);
        if wall.termd().state() == TermdState::Unreachable
            && matches!(&self.terms, Some(Terminals::Live(s)) if s.is_reconnect_socket_absent())
        {
            wall.on_termd_socket_absent(epoch_now_ms());
        }
        let mut described: Described = wall.frame();
        let termd_state = wall.termd().state();
        self.animating = moving || described.animating;
        self.termd_tick_at = match termd_state {
            TermdState::Restarting => Some(Instant::now() + std::time::Duration::from_millis(200)),
            TermdState::Unreachable => Some(Instant::now() + std::time::Duration::from_secs(1)),
            _ => None,
        };
        self.apply_effects();
        let ids: Vec<(String, u64)> = described.terminals.iter().map(|t| (t.id.clone(), 0)).collect();
        let ids: Vec<(String, u64)> = ids.into_iter().map(|(id, _)| (id.clone(), self.render_id(&id))).collect();

        let (Some(g), Some(terms)) = (&mut self.gfx, &self.terms) else { return };
        use wgpu::CurrentSurfaceTexture as Cst;
        let t_acq = Instant::now();
        let frame = if to_surface {
            let _s = (self.frames == 0).then(|| super::startup::span("first_surface_acquire"));
            match g.surface.get_current_texture() {
                Cst::Success(f) | Cst::Suboptimal(f) => {
                    self.occluded = false;
                    Some(f)
                }
                Cst::Lost | Cst::Outdated => {
                    g.surface.configure(&self.ctx.device, &g.config);
                    g.window.request_redraw();
                    None
                }
                // Covered or asleep: skip surface present, unless an offscreen shot is waiting.
                Cst::Occluded | Cst::Timeout => {
                    self.occluded = true;
                    None
                }
                Cst::Validation => {
                    log::error!("the wall's surface failed validation; skipping the frame");
                    None
                }
            }
        } else {
            None
        };
        let t_post_acq = Instant::now();
        let need_offscreen = self.pending_shot.is_some()
            || self.perf.is_enabled()
            || (self.exit_after == Some(self.frames + 1) && std::env::var_os("GLYPH_WALL_SHOT").is_some());
        #[cfg(feature = "egui-ui")]
        let need_ui = (!to_surface && self.exit_after.is_none())
            || g.ui.as_ref().is_some_and(WallUi::has_pending_input)
            || self.ui_repaint_at.is_some_and(|d| now >= d);
        #[cfg(not(feature = "egui-ui"))]
        let need_ui = false;
        if frame.is_none() && !need_offscreen && !need_ui {
            if !to_surface && self.exit_after.is_some() {
                self.frames += 1;
                self.scripted();
            }
            return;
        }
        let sf = g.window.scale_factor() as f32;
        // The wall's camera is in logical pixels per world unit; the
        // renderer's is in physical ones.
        let camera = WallCamera::new(Vec2::new(described.camera.x as f32, described.camera.y as f32), described.camera.zoom as f32 * sf);
        let viewport = (g.config.width, g.config.height);

        let build_scene = frame.is_some() || need_offscreen;
        let mut draws: Vec<TerminalDraw<'_>> = Vec::new();
        if build_scene {
            draws.reserve(described.terminals.len());
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
        }

        let mut shapes: Vec<Shape> = Vec::new();
        let mut labels: Vec<Label<'static>> = Vec::new();
        if described.notice.is_none() {
            described.notice = self
                .starting_termd
                .then(|| "starting termd...".to_string())
                .or_else(|| g.renderer.shader_error_notice().map(str::to_owned));
        }
        let paper = build_scene.then(|| {
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
        });
        let wall_frame = WallFrame { paper: paper.as_ref(), terminals: &draws, shapes: &shapes, labels: &labels };
        if build_scene {
            self.blink_repaint_at = g.renderer.next_cursor_blink(&draws, Instant::now());
        }
        let view = frame
            .as_ref()
            .map(|f| f.texture.create_view(&Default::default()))
            .or_else(|| self.perf.occluded_view(&self.ctx.device, g.config.format, g.config.width, g.config.height));
        let mut encoder = view.as_ref().map(|_| self.ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("wall frame") }));
        let stats = match (&view, &mut encoder) {
            (Some(v), Some(enc)) => {
                let target = WallTarget { color_view: v, depth_view: &g.depth, width: g.config.width, height: g.config.height };
                g.renderer.render(&self.ctx.device, &self.ctx.queue, enc, &target, &camera, &wall_frame)
            }
            _ => super::renderer::UploadStats::default(),
        };

        #[cfg(feature = "egui-ui")]
        self.panels.set_wall_dragging(self.buttons != 0);
        #[cfg(feature = "egui-ui")]
        let mut panel_verbs: Vec<String> = Vec::new();
        #[cfg(feature = "egui-ui")]
        let mut panel_egui_keys = false;
        #[cfg(feature = "egui-ui")]
        let mut ui_painted_on_gpu = false;
        #[cfg(feature = "egui-ui")]
        let mut ui_painted = {
            let skip_bootstrap_ui =
                self.frames == 0 && to_surface && self.exit_after.is_none() && self.pending_shot.is_none();
            let ui_needed = !skip_bootstrap_ui
                && (self.hud_open
                    || self.wall.as_ref().is_some_and(|w| self.panels.any_open(w, &described))
                    || g.ui.as_ref().is_some_and(WallUi::is_active));
            if ui_needed {
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
                let window_arc = g.window.clone();
                let ui = g.ensure_ui(&self.ctx.device);
                let mut ui_frame = ui.run(&window_arc, viewport, |ctx| {
                    if let Some(w) = wall_ref {
                        panel_verbs = panels.show(ctx, w, &described);
                    }
                    if *hud_open {
                        show_debug_hud(ctx, hud_open, &hud_input);
                    }
                });
                panel_egui_keys = ui_frame.wants_keyboard;
                if let (Some(painted), Some(v), Some(enc)) = (ui_frame.painted.as_mut(), &view, &mut encoder) {
                    ui.paint(&self.ctx.device, &self.ctx.queue, enc, v, painted);
                    ui_painted_on_gpu = true;
                }
                match ui_frame.repaint_after {
                    Some(delay) if delay.is_zero() => {
                        self.ui_repaint_at = Some(Instant::now() + std::time::Duration::from_millis(16));
                        g.window.request_redraw();
                    }
                    Some(delay) => self.ui_repaint_at = Some(Instant::now() + delay),
                    None => self.ui_repaint_at = None,
                }
                ui_frame.painted
            } else {
                self.ui_repaint_at = None;
                None
            }
        };
        if let Some(enc) = encoder {
            let presented = frame.is_some();
            self.perf.submit_and_record(&self.ctx, enc, frame, (now, t_acq, t_post_acq), stats);
            if presented {
                super::startup::mark_once("first_frame_presented");
                if !draws.is_empty() && g.renderer.pipelines_ready() {
                    super::startup::mark_once("first_terminal_frame_presented");
                }
            }
        }

        // A counted run can keep its last frame: GLYPH_WALL_SHOT=path.png
        // draws the same frame once more offscreen and saves it.
        let asked = self.pending_shot.take();
        let counted = (self.exit_after == Some(self.frames + 1)).then(|| std::env::var("GLYPH_WALL_SHOT").ok().map(PathBuf::from)).flatten();
        if asked.is_some() || counted.is_some() {
            g.renderer.wait_for_pipelines();
            for path in asked.iter().map(|(p, _)| p.clone()).chain(counted) {
                super::renderer::render_wall_to_png_with_overlay(&self.ctx, &mut g.renderer, &camera, &wall_frame, viewport, 1, &path, |enc, color_view| {
                    #[cfg(feature = "egui-ui")]
                    if let (Some(ui), Some(painted)) = (g.ui.as_mut(), ui_painted.as_mut()) {
                        ui.paint(&self.ctx.device, &self.ctx.queue, enc, color_view, painted);
                        ui_painted_on_gpu = true;
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
        if !ui_painted_on_gpu {
            if let (Some(ui), Some(painted)) = (g.ui.as_mut(), ui_painted.as_mut()) {
                ui.sync_textures_only(&self.ctx.device, &self.ctx.queue, painted);
            }
        }
        #[cfg(feature = "egui-ui")]
        {
            let mut changed = false;
            let mut status_opened = false;
            if let (Some(wall), Some(terms)) = (&mut self.wall, &self.terms) {
                changed |= self.panels.sync_after_pass(wall, panel_egui_keys);
                if !panel_verbs.is_empty() {
                    let lookup = screen_lookup(|id| terms.screen(id));
                    for verb in &panel_verbs {
                        if verb.starts_with("wall.status") {
                            status_opened = true;
                        }
                        let reply = wall.run_with_screens(verb.as_str(), &lookup);
                        self.panels.record_verb_reply(verb, &reply.text);
                    }
                    changed = true;
                }
            }
            if status_opened {
                self.refresh_termd_status();
                changed = true;
            }
            if changed {
                self.apply_effects();
                self.redraw();
            }
        }
        self.sync_ime_cursor_area(&described);
        self.sync_macos_option_as_alt();
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
        if !key.code.is_empty() {
            if key.pressed {
                self.held_codes.insert(key.code.clone());
            } else {
                self.held_codes.remove(&key.code);
            }
        }
        if self.ime_composing && key.pressed {
            return;
        }
        let host_chord = wall_core::chord::chord_mods(Platform::host());
        // Quitting is the window's, not the wall's (Command+Q on macOS, Ctrl+Alt+Q elsewhere).
        if key.pressed && !key.repeat && key.code == "KeyQ" && key.mods == host_chord {
            self.quit = true;
            return;
        }
        #[cfg(feature = "egui-ui")]
        if key.pressed && !key.repeat && key.code == "F1" && key.mods == host_chord {
            self.hud_open = !self.hud_open;
            self.redraw();
            return;
        }
        let (Some(wall), Some(terms)) = (&mut self.wall, &self.terms) else { return };
        #[cfg(feature = "egui-ui")]
        if key.pressed && key.code == "Escape" && !key.mods.ctrl && !key.mods.alt && !key.mods.meta {
            if self.panels.close_open_on_escape(wall) {
                self.apply_effects();
                self.sync_macos_option_as_alt();
                self.redraw();
                return;
            }
            if self.hud_open {
                self.hud_open = false;
                self.redraw();
                return;
            }
        }
        let bypass_panel_focus = wall.menu().is_none()
            && chord_bypasses_panel_input(Platform::host(), &key)
            && !matches!(key.code.as_str(), "BracketLeft" | "BracketRight")
            && wall_core::chord::group_cycle_dir(&key).is_none();
        let saved_focus = bypass_panel_focus.then(|| {
            // Non-text-editing wall chords (`⌘,`, `⌘/`, `⌘K`, `⌘B`, `⌘S`, `⌘\`, `⌘D`, …)
            // execute even while a panel search field is active.
            let cur = if wall.palette_open() {
                FieldFocus::Input
            } else {
                FieldFocus::None
            };
            wall.set_field_focus(FieldFocus::None);
            cur
        });
        let lookup = screen_lookup(|id| terms.screen(id));
        let decision = wall.key(&key, &lookup);
        if let Some(prev) = saved_focus {
            if prev != FieldFocus::None && wall.palette_open() {
                wall.set_field_focus(prev);
            }
        }
        #[cfg(feature = "egui-ui")]
        if matches!(decision, KeyDecision::Chord { blur_field: true, .. }) {
            self.panels.blur_docked_inputs();
            if let Some(ui) = self.gfx.as_ref().and_then(|g| g.ui.as_ref()) {
                ui.surrender_focus();
            }
        }
        #[cfg(not(feature = "egui-ui"))]
        let _ = decision;
        self.apply_effects();
        self.sync_macos_option_as_alt();
        self.redraw();
    }

    fn key(&mut self, ev: &winit::event::KeyEvent, event_loop: &ActiveEventLoop) {
        let opt_alt = self
            .wall
            .as_ref()
            .map_or(OptionAsAlt::Auto, |w| w.config().option_as_alt);
        let key = super::keys::key_event(ev, self.modifiers, opt_alt);
        self.key_input(key);
        if self.quit {
            event_loop.exit();
        }
    }

    fn ime(&mut self, ime: &Ime) {
        match ime {
            Ime::Enabled => {}
            Ime::Preedit(text, _) => {
                self.ime_composing = !text.is_empty();
            }
            Ime::Commit(text) => {
                self.ime_composing = false;
                self.ime_commit(text);
            }
            Ime::Disabled => {
                self.ime_composing = false;
            }
        }
    }

    fn ime_commit(&mut self, text: &str) {
        let cleaned: String = text.chars().filter(|c| !c.is_control()).collect();
        if cleaned.is_empty() {
            return;
        }
        let (Some(wall), Some(terms)) = (&mut self.wall, &self.terms) else { return };
        let lookup = screen_lookup(|id| terms.screen(id));
        let mut rest = cleaned.as_str();
        while !rest.is_empty() {
            let mut end = rest.len().min(4096);
            while end > 0 && !rest.is_char_boundary(end) {
                end -= 1;
            }
            let chunk = &rest[..end];
            rest = &rest[end..];
            let key_name = match chunk {
                "Shift" | "Control" | "Alt" | "Meta" | "AltGraph" | "CapsLock" | "NumLock"
                | "ScrollLock" | "Fn" | "FnLock" | "Hyper" | "Super" | "Symbol"
                | "SymbolLock" | "Dead" | "Process" | "Unidentified" | "Escape" => " ".to_string(),
                other => other.to_string(),
            };
            let ev = KeyEvent {
                code: String::new(),
                key: key_name,
                text: Some(chunk.to_string()),
                mods: Mods::default(),
                repeat: false,
                pressed: true,
            };
            let _ = wall.key(&ev, &lookup);
        }
        self.apply_effects();
        self.redraw();
    }

    fn focused(&mut self, focused: bool) {
        if !focused {
            self.modifiers = ModifiersState::empty();
            self.ime_composing = false;
            let had_buttons = self.buttons != 0;
            self.buttons = 0;
            let now_ms = self.now_ms();
            let cursor = self.cursor;
            let held: Vec<String> = self.held_codes.drain().collect();
            if let (Some(wall), Some(terms)) = (&mut self.wall, &self.terms) {
                if had_buttons {
                    let ev = PointerEvent {
                        pointer_id: 1,
                        x: cursor.x,
                        y: cursor.y,
                        button: None,
                        buttons: Buttons(0),
                        mods: Mods::default(),
                        time_ms: now_ms,
                    };
                    wall.pointer_cancel(&ev);
                }
                let lookup = screen_lookup(|id| terms.screen(id));
                for code in &held {
                    let key_name = match code.as_str() {
                        "MetaLeft" | "MetaRight" => "Meta",
                        "ControlLeft" | "ControlRight" => "Control",
                        "AltLeft" | "AltRight" => "Alt",
                        "ShiftLeft" | "ShiftRight" => "Shift",
                        _ => "a",
                    };
                    let up = KeyEvent {
                        code: code.clone(),
                        key: key_name.to_string(),
                        text: None,
                        mods: Mods::default(),
                        repeat: false,
                        pressed: false,
                    };
                    let _ = wall.key(&up, &lookup);
                }
                let meta_up = KeyEvent {
                    code: "MetaLeft".to_string(),
                    key: "Meta".to_string(),
                    text: None,
                    mods: Mods::default(),
                    repeat: false,
                    pressed: false,
                };
                let _ = wall.key(&meta_up, &lookup);
            }
            self.apply_effects();
            self.redraw();
        } else {
            if let Some(g) = &self.gfx {
                g.window.set_ime_allowed(true);
            }
            self.sync_macos_option_as_alt();
            self.redraw();
        }
    }

    #[cfg(feature = "egui-ui")]
    fn script_pointer_goes_to_ui(&self) -> bool {
        if self.buttons != 0 {
            return false;
        }
        self.gfx.as_ref().and_then(|g| g.ui.as_ref()).is_some_and(|ui| {
            pointer_target(ui.is_active(), ui.context(), self.cursor, self.buttons)
                == PointerTarget::Ui
        })
    }

    fn script_move(&mut self, x: f64, y: f64, run_ui_pass: bool) {
        self.cursor = Point::new(x, y);
        #[cfg(feature = "egui-ui")]
        let ui_pointer = self.script_pointer_goes_to_ui();
        #[cfg(not(feature = "egui-ui"))]
        let ui_pointer = false;
        #[cfg(feature = "egui-ui")]
        if let Some(g) = &mut self.gfx {
            g.ensure_ui(&self.ctx.device)
                .push_script_pointer_moved(egui::pos2(x as f32, y as f32));
        }
        if !ui_pointer {
            self.move_cursor(x, y);
        }
        if run_ui_pass {
            self.draw(false);
        }
    }

    fn script_down(&mut self, button: MouseButton, mods: Option<ModifiersState>) {
        let prev = self.modifiers;
        if let Some(m) = mods {
            self.modifiers = m;
        }
        #[cfg(feature = "egui-ui")]
        let ui_pointer = self.script_pointer_goes_to_ui();
        #[cfg(not(feature = "egui-ui"))]
        let ui_pointer = false;
        #[cfg(feature = "egui-ui")]
        if let (Some(g), Some(eb)) = (&mut self.gfx, super::ui::winit_button_to_egui(button)) {
            let emods = super::ui::winit_mods_to_egui(self.modifiers);
            let pos = egui::pos2(self.cursor.x as f32, self.cursor.y as f32);
            let ui = g.ensure_ui(&self.ctx.device);
            ui.push_script_pointer_moved(pos);
            ui.push_script_pointer_button(pos, eb, true, emods);
        }
        if !ui_pointer {
            self.mouse(ElementState::Pressed, button);
        }
        self.draw(false);
        if mods.is_some() {
            self.modifiers = prev;
            #[cfg(feature = "egui-ui")]
            if let Some(ui) = self.gfx.as_mut().and_then(|g| g.ui.as_mut()) {
                ui.set_script_modifiers(super::ui::winit_mods_to_egui(prev));
            }
        }
    }

    fn script_up(&mut self, button: MouseButton, mods: Option<ModifiersState>) {
        let prev = self.modifiers;
        if let Some(m) = mods {
            self.modifiers = m;
        }
        #[cfg(feature = "egui-ui")]
        if let (Some(g), Some(eb)) = (&mut self.gfx, super::ui::winit_button_to_egui(button)) {
            let emods = super::ui::winit_mods_to_egui(self.modifiers);
            let pos = egui::pos2(self.cursor.x as f32, self.cursor.y as f32);
            g.ensure_ui(&self.ctx.device)
                .push_script_pointer_button(pos, eb, false, emods);
        }
        if button_of(button).is_some_and(|b| (self.buttons & Buttons::from_button(b).0) != 0) {
            self.mouse(ElementState::Released, button);
        }
        self.draw(false);
        if mods.is_some() {
            self.modifiers = prev;
            #[cfg(feature = "egui-ui")]
            if let Some(ui) = self.gfx.as_mut().and_then(|g| g.ui.as_mut()) {
                ui.set_script_modifiers(super::ui::winit_mods_to_egui(prev));
            }
        }
    }

    fn script_key(&mut self, k: &super::script::ScriptKey) -> &'static str {
        use super::script::{modifier_bit_of_physical, ScriptKeyPhase};
        #[cfg(feature = "egui-ui")]
        let panels_want_keys = {
            let eff_mods = self.modifiers | k.mods;
            let mapped = super::keys::key_from_parts(
                k.physical,
                &k.logical,
                k.text.as_deref(),
                ElementState::Pressed,
                false,
                eff_mods,
            );
            let menu_open = self.wall.as_ref().is_some_and(|w| w.menu().is_some());
            let wall_chord_bypass =
                !menu_open && chord_bypasses_panel_input(Platform::host(), &mapped);
            !wall_chord_bypass
                && (self.wall.as_ref().is_some_and(|w| self.panels.wants_keyboard(w))
                    || self.gfx.as_ref().and_then(|g| g.ui.as_ref()).is_some_and(|ui| {
                        ui.is_active() && ui.context().egui_wants_keyboard_input()
                    }))
        };
        #[cfg(not(feature = "egui-ui"))]
        let panels_want_keys = false;

        if panels_want_keys {
            let prev = self.modifiers;
            match k.phase {
                ScriptKeyPhase::Tap => {
                    self.modifiers |= k.mods;
                    #[cfg(feature = "egui-ui")]
                    if let Some(g) = &mut self.gfx {
                        let ekey = super::ui::winit_key_to_egui(k.physical, &k.logical);
                        let emods = super::ui::winit_mods_to_egui(self.modifiers);
                        g.ensure_ui(&self.ctx.device)
                            .push_script_key(ekey, true, emods, k.text.as_deref());
                    }
                    self.draw(false);
                    #[cfg(feature = "egui-ui")]
                    if let Some(g) = &mut self.gfx {
                        let ekey = super::ui::winit_key_to_egui(k.physical, &k.logical);
                        let emods = super::ui::winit_mods_to_egui(self.modifiers);
                        g.ensure_ui(&self.ctx.device)
                            .push_script_key(ekey, false, emods, None);
                    }
                    self.draw(false);
                    self.modifiers = prev;
                    "OK: key"
                }
                ScriptKeyPhase::Repeat => {
                    self.modifiers |= k.mods;
                    #[cfg(feature = "egui-ui")]
                    if let Some(g) = &mut self.gfx {
                        let ekey = super::ui::winit_key_to_egui(k.physical, &k.logical);
                        let emods = super::ui::winit_mods_to_egui(self.modifiers);
                        g.ensure_ui(&self.ctx.device)
                            .push_script_key(ekey, true, emods, k.text.as_deref());
                    }
                    self.draw(false);
                    self.modifiers = prev;
                    "OK: key repeat"
                }
                ScriptKeyPhase::Down => {
                    if let Some(bit) = modifier_bit_of_physical(k.physical) {
                        self.modifiers |= bit;
                    }
                    let eff = self.modifiers | k.mods;
                    #[cfg(feature = "egui-ui")]
                    if let Some(g) = &mut self.gfx {
                        let ekey = super::ui::winit_key_to_egui(k.physical, &k.logical);
                        let emods = super::ui::winit_mods_to_egui(eff);
                        g.ensure_ui(&self.ctx.device)
                            .push_script_key(ekey, true, emods, k.text.as_deref());
                    }
                    self.draw(false);
                    if modifier_bit_of_physical(k.physical).is_none() {
                        self.modifiers = prev | k.mods;
                    }
                    "OK: key down"
                }
                ScriptKeyPhase::Up => {
                    if let Some(bit) = modifier_bit_of_physical(k.physical) {
                        self.modifiers.remove(bit);
                    }
                    #[cfg(feature = "egui-ui")]
                    if let Some(g) = &mut self.gfx {
                        let ekey = super::ui::winit_key_to_egui(k.physical, &k.logical);
                        let emods = super::ui::winit_mods_to_egui(self.modifiers);
                        g.ensure_ui(&self.ctx.device)
                            .push_script_key(ekey, false, emods, None);
                    }
                    self.draw(false);
                    "OK: key up"
                }
            }
        } else {
            match k.phase {
                ScriptKeyPhase::Tap => {
                    let prev = self.modifiers;
                    self.modifiers |= k.mods;
                    let down = super::keys::key_from_parts(k.physical, &k.logical, k.text.as_deref(), ElementState::Pressed, false, self.modifiers);
                    let up = super::keys::key_from_parts(k.physical, &k.logical, None, ElementState::Released, false, self.modifiers);
                    self.key_input(down);
                    self.key_input(up);
                    self.modifiers = prev;
                    "OK: key"
                }
                ScriptKeyPhase::Repeat => {
                    let prev = self.modifiers;
                    self.modifiers |= k.mods;
                    let rep = super::keys::key_from_parts(k.physical, &k.logical, k.text.as_deref(), ElementState::Pressed, true, self.modifiers);
                    self.key_input(rep);
                    self.modifiers = prev;
                    "OK: key repeat"
                }
                ScriptKeyPhase::Down => {
                    if let Some(bit) = modifier_bit_of_physical(k.physical) {
                        self.modifiers |= bit;
                    }
                    let eff = self.modifiers | k.mods;
                    let prev = self.modifiers;
                    self.modifiers = eff;
                    let down = super::keys::key_from_parts(k.physical, &k.logical, k.text.as_deref(), ElementState::Pressed, false, self.modifiers);
                    self.key_input(down);
                    if modifier_bit_of_physical(k.physical).is_none() {
                        self.modifiers = prev | k.mods;
                    }
                    "OK: key down"
                }
                ScriptKeyPhase::Up => {
                    if let Some(bit) = modifier_bit_of_physical(k.physical) {
                        self.modifiers.remove(bit);
                    }
                    let up = super::keys::key_from_parts(k.physical, &k.logical, None, ElementState::Released, false, self.modifiers);
                    self.key_input(up);
                    "OK: key up"
                }
            }
        }
    }

    fn run_script(&mut self, cmd: super::script::ScriptCommand) -> String {
        use super::script::{button_label, keys_for_typed_text, ScriptCommand};
        match cmd {
            ScriptCommand::Where => {
                self.draw(false);
                match &mut self.wall {
                    Some(wall) => {
                        let described = wall.frame();
                        let mut out = super::script::format_where(&described, self.cursor);
                        out.push_str(&format!(
                            "\npaper grid_show={} grid_minor={}",
                            described.paper.grid_show, described.paper.grid.grid_minor
                        ));
                        #[cfg(feature = "egui-ui")]
                        {
                            let panel_lines = self.panels.format_where(wall, &described);
                            if !panel_lines.is_empty() {
                                out.push('\n');
                                out.push_str(&panel_lines);
                            }
                        }
                        out
                    }
                    None => "ERR: the wall is not open yet".to_string(),
                }
            }
            ScriptCommand::Move { x, y } => {
                self.script_move(x, y, true);
                format!("OK: moved to ({x:.2}, {y:.2})")
            }
            ScriptCommand::Down { button, mods } => {
                self.script_down(button, mods);
                format!("OK: down {} at ({:.2}, {:.2})", button_label(button), self.cursor.x, self.cursor.y)
            }
            ScriptCommand::Up { button, mods } => {
                self.script_up(button, mods);
                format!("OK: up {} at ({:.2}, {:.2})", button_label(button), self.cursor.x, self.cursor.y)
            }
            ScriptCommand::Click { x, y, button, mods } => {
                self.last_release = None;
                self.script_move(x, y, true);
                self.script_down(button, Some(mods));
                self.script_up(button, Some(mods));
                format!("OK: clicked {} at ({x:.2}, {y:.2})", button_label(button))
            }
            ScriptCommand::DblClick { x, y } => {
                self.last_release = None;
                self.script_move(x, y, true);
                self.script_down(MouseButton::Left, None);
                self.script_up(MouseButton::Left, None);
                self.script_down(MouseButton::Left, None);
                self.script_up(MouseButton::Left, None);
                format!("OK: dblclick at ({x:.2}, {y:.2})")
            }
            ScriptCommand::Drag { x0, y0, x1, y1, steps, button, mods } => {
                let prev = self.modifiers;
                self.modifiers = mods;
                self.last_release = None;
                self.script_move(x0, y0, true);
                self.script_down(button, Some(mods));
                self.modifiers = mods;
                let wall_gesture = self.buttons != 0;
                for i in 1..=steps {
                    let t = i as f64 / steps as f64;
                    self.script_move(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t, !wall_gesture);
                }
                self.script_up(button, Some(mods));
                self.modifiers = prev;
                format!("OK: dragged {} ({x0:.2}, {y0:.2}) -> ({x1:.2}, {y1:.2}) in {steps} steps", button_label(button))
            }
            ScriptCommand::Wheel { x, y, dx, dy, mods } => {
                let prev = self.modifiers;
                self.modifiers = mods;
                self.script_move(x, y, true);
                #[cfg(feature = "egui-ui")]
                let ui_pointer = self.script_pointer_goes_to_ui();
                #[cfg(not(feature = "egui-ui"))]
                let ui_pointer = false;
                #[cfg(feature = "egui-ui")]
                if let Some(g) = &mut self.gfx {
                    let emods = super::ui::winit_mods_to_egui(self.modifiers);
                    g.ensure_ui(&self.ctx.device).push_script_wheel(
                        egui::pos2(x as f32, y as f32),
                        egui::vec2(dx as f32, dy as f32),
                        emods,
                    );
                }
                if !ui_pointer {
                    let sf = self.scale_factor();
                    self.wheel(MouseScrollDelta::PixelDelta(PhysicalPosition::new(dx * sf, dy * sf)));
                }
                self.draw(false);
                self.modifiers = prev;
                #[cfg(feature = "egui-ui")]
                if let Some(ui) = self.gfx.as_mut().and_then(|g| g.ui.as_mut()) {
                    ui.set_script_modifiers(super::ui::winit_mods_to_egui(prev));
                }
                format!("OK: wheel ({dx:.2}, {dy:.2}) at ({x:.2}, {y:.2})")
            }
            ScriptCommand::Pinch { x, y, factor } => {
                self.script_move(x, y, true);
                #[cfg(feature = "egui-ui")]
                let ui_pointer = self.script_pointer_goes_to_ui();
                #[cfg(not(feature = "egui-ui"))]
                let ui_pointer = false;
                if !ui_pointer {
                    self.pinch(factor - 1.0);
                }
                format!("OK: pinch {factor:.4} at ({x:.2}, {y:.2})")
            }
            ScriptCommand::Key(k) => self.script_key(&k).to_string(),
            ScriptCommand::Type(text) => {
                let seq = keys_for_typed_text(&text);
                let count = seq.len();
                for k in &seq {
                    self.script_key(k);
                }
                format!("OK: typed {count} key(s)")
            }
            ScriptCommand::Ime(ime_cmd) => {
                use super::script::ScriptIme;
                match ime_cmd {
                    ScriptIme::Preedit(text) => {
                        let cursor = (!text.is_empty()).then_some((0, text.len()));
                        self.ime(&Ime::Preedit(text, cursor));
                        "OK: ime preedit".to_string()
                    }
                    ScriptIme::Commit(text) => {
                        self.ime(&Ime::Preedit(String::new(), None));
                        self.ime(&Ime::Commit(text));
                        "OK: ime commit".to_string()
                    }
                    ScriptIme::Disable => {
                        self.ime(&Ime::Disabled);
                        "OK: ime disabled".to_string()
                    }
                }
            }
            ScriptCommand::Focus(on) => {
                self.focused(on);
                format!("OK: focus {}", if on { "on" } else { "off" })
            }
            ScriptCommand::Clipboard(clip_cmd) => {
                use super::script::ScriptClipboard;
                match clip_cmd {
                    ScriptClipboard::Get => self.last_copied.clone().unwrap_or_default(),
                    ScriptClipboard::Clear => {
                        self.last_copied = None;
                        if self.use_memory_clipboard {
                            self.memory_clipboard = None;
                        }
                        "OK: clipboard cleared".to_string()
                    }
                    ScriptClipboard::Set(text) => {
                        if !self.use_memory_clipboard {
                            "ERR: app.clipboard set requires GLYPH_WALL_CLIPBOARD=memory".to_string()
                        } else {
                            self.last_copied = Some(text.clone());
                            self.memory_clipboard = Some(text);
                            "OK: clipboard set".to_string()
                        }
                    }
                }
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
            #[cfg(feature = "egui-ui")]
            {
                self.panels.blur_docked_inputs();
                if let Some(ui) = self.gfx.as_ref().and_then(|g| g.ui.as_ref()) {
                    ui.surrender_focus();
                }
            }
            // A secondary press (right-click, or Ctrl+left-click on macOS) opens (or moves) the wall's menu unless a focused
            // mouse-reporting program claims it; no browser-menu passthrough.
            if secondary {
                if wall.menu().is_some() {
                    let _ = wall.run("wall.menu close");
                }
                if wall.context_menu(ev.x, ev.y, ev.mods).verdict != Verdict::Program {
                    self.apply_effects();
                    self.redraw();
                    return;
                }
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

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _wake: Wake) {
        self.wakes.pending.store(false, std::sync::atomic::Ordering::SeqCst);
        self.perf.record_wake();
        if self.perf.should_exit_on_wake(self.quit) {
            event_loop.exit();
            return;
        }
        self.poll_pending_termd();
        if self.gfx.as_mut().is_some_and(|g| g.renderer.poll_ready_pipelines()) {
            self.redraw();
        }
        self.serve_control();
        self.pump(true);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if let WindowEvent::CursorMoved { position, .. } = &event {
            let sf = self.scale_factor();
            self.cursor = Point::new(position.x / sf, position.y / sf);
        }
        #[cfg(feature = "egui-ui")]
        let wall_chord_bypass = match &event {
            WindowEvent::KeyboardInput { event: kev, is_synthetic: false, .. } => {
                let menu_open = self.wall.as_ref().is_some_and(|w| w.menu().is_some());
                let opt_alt = self
                    .wall
                    .as_ref()
                    .map_or(OptionAsAlt::Auto, |w| w.config().option_as_alt);
                let mapped = super::keys::key_event(kev, self.modifiers, opt_alt);
                !menu_open && chord_bypasses_panel_input(Platform::host(), &mapped)
            }
            _ => false,
        };
        #[cfg(feature = "egui-ui")]
        let panels_want_keys = self.wall.as_ref().is_some_and(|w| self.panels.wants_keyboard(w));
        #[cfg(feature = "egui-ui")]
        let feed_ui = !wall_chord_bypass
            && (!matches!(event, WindowEvent::KeyboardInput { .. } | WindowEvent::Ime(_))
                || panels_want_keys
                || self.gfx.as_ref().and_then(|g| g.ui.as_ref()).is_some_and(|ui| ui.context().egui_wants_keyboard_input()));
        #[cfg(feature = "egui-ui")]
        let ui_outcome = match (&mut self.gfx, feed_ui) {
            (Some(g), true) => g.ui.as_mut().map_or_else(UiEventOutcome::default, |ui| ui.on_window_event(&g.window, &event)),
            _ => UiEventOutcome::default(),
        };
        #[cfg(feature = "egui-ui")]
        if ui_outcome.repaint {
            self.redraw();
        }
        #[cfg(feature = "egui-ui")]
        let (ui_pointer, ui_keys) = (
            self.pointer_goes_to_ui(&ui_outcome),
            !wall_chord_bypass
                && (ui_outcome.consumed || ui_outcome.wants_keyboard || panels_want_keys),
        );
        #[cfg(not(feature = "egui-ui"))]
        let (ui_pointer, ui_keys) = (false, false);

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Focused(focused) => self.focused(focused),
            WindowEvent::Occluded(occluded) => {
                self.occluded = occluded;
                if occluded {
                    self.wakes.redraw_armed.store(false, std::sync::atomic::Ordering::SeqCst);
                } else {
                    self.redraw();
                }
            }
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
                if ui_pointer {
                    return;
                }
                self.move_cursor(x, y);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if state == ElementState::Pressed {
                    if ui_pointer {
                        return;
                    }
                    self.mouse(state, button);
                } else if button_of(button).is_some_and(|b| (self.buttons & Buttons::from_button(b).0) != 0) {
                    // Only releases matching an active wall press reach the wall,
                    // keeping wall drags whole without leaking UI button clicks.
                    self.mouse(state, button);
                }
            }
            WindowEvent::MouseWheel { delta, .. } if !ui_pointer => self.wheel(delta),
            WindowEvent::PinchGesture { delta, .. } if !ui_pointer => self.pinch(delta),
            WindowEvent::Ime(ime) if !ui_keys => self.ime(&ime),
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } if !ui_keys => {
                self.key(&event, event_loop);
            }
            _ => {}
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // The egui layer, the surface and the window go while the event
        // loop's display connection is still alive (see `Gfx`).
        self.perf.clear_gpu();
        self.control = None;
        self.gfx = None;
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        const COVERED_TICK: std::time::Duration = std::time::Duration::from_millis(32);
        self.poll_pending_termd();
        if self.gfx.as_mut().is_some_and(|g| g.renderer.poll_ready_pipelines()) {
            self.redraw();
        }
        self.serve_control();
        if self.quit || self.perf.should_exit_on_wake(false) {
            event_loop.exit();
            return;
        }
        let (shader_dirty, shader_repaint_at) = match self.gfx.as_mut() {
            Some(g) => g.renderer.poll_shaders(&self.ctx.device, Instant::now()),
            None => (false, None),
        };
        if shader_dirty {
            self.redraw();
        }
        // Something is still moving (an eased camera, a glide), a counted run
        // is advancing, or an offscreen shot is pending: ask for a redraw, and
        // when covered tick at a low rate (~31 Hz) instead of spinning.
        if self.pending_shot.is_some() {
            self.draw(false);
        }
        if self.exit_after.is_some_and(|n| self.frames >= n) {
            event_loop.exit();
            return;
        }
        let counted = self.exit_after.is_some();
        if counted || self.animating {
            if self.occluded && !self.perf.is_enabled() {
                if self.last_tick.elapsed() >= COVERED_TICK {
                    self.draw(false);
                    if self.exit_after.is_some_and(|n| self.frames >= n) {
                        event_loop.exit();
                        return;
                    }
                    self.redraw();
                }
                if self.exit_after.is_some() || self.animating {
                    let wait = COVERED_TICK.saturating_sub(self.last_tick.elapsed());
                    event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + wait));
                    return;
                }
            } else {
                self.redraw();
                if self.last_tick.elapsed() >= COVERED_TICK {
                    self.occluded = true;
                    self.draw(false);
                    if self.exit_after.is_some_and(|n| self.frames >= n) {
                        event_loop.exit();
                        return;
                    }
                }
                if self.animating || counted {
                    let (perf_exit, cf) = self.perf.control_flow(
                        self.frames,
                        self.animating,
                        Some(Instant::now() + std::time::Duration::from_millis(16)),
                    );
                    if perf_exit {
                        event_loop.exit();
                    } else {
                        event_loop.set_control_flow(cf);
                    }
                    return;
                }
            }
        }
        let now = Instant::now();
        let mut next_repaint: Option<Instant> = shader_repaint_at;
        if self.wakes.redraw_armed.load(std::sync::atomic::Ordering::Relaxed) {
            if self.last_tick.elapsed() >= COVERED_TICK {
                self.wakes.redraw_armed.store(false, std::sync::atomic::Ordering::SeqCst);
            } else {
                let arm_deadline = now + COVERED_TICK;
                next_repaint = Some(next_repaint.map_or(arm_deadline, |d| d.min(arm_deadline)));
            }
        }
        #[cfg(feature = "egui-ui")]
        if let Some(deadline) = self.ui_repaint_at {
            if now >= deadline {
                self.ui_repaint_at = None;
                self.redraw();
                self.draw(false);
                if let Some(next_ui) = self.ui_repaint_at {
                    next_repaint = Some(next_ui);
                }
            } else {
                next_repaint = Some(next_repaint.map_or(deadline, |d| d.min(deadline)));
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
        if let Some(deadline) = self.termd_tick_at {
            if now >= deadline {
                self.termd_tick_at = None;
                self.redraw();
                if self.last_tick.elapsed() >= std::time::Duration::from_millis(180) {
                    self.draw(false);
                }
            } else {
                next_repaint = Some(next_repaint.map_or(deadline, |d| d.min(deadline)));
            }
        }
        let (perf_exit, cf) = self.perf.control_flow(self.frames, self.animating, next_repaint);
        if perf_exit {
            event_loop.exit();
        } else {
            event_loop.set_control_flow(cf);
        }
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
        termd_tick_at: None,
        pending_termd: None,
        starting_termd: false,
        occluded: false,
        wakes: Arc::new(WakeGate::default()),
        perf: super::perf::WallPerf::from_env(),
        ime_composing: false,
        last_ime_area: None,
        #[cfg(target_os = "macos")]
        macos_option_as_alt: None,
        held_codes: HashSet::new(),
        last_copied: None,
        memory_clipboard: None,
        use_memory_clipboard: std::env::var("GLYPH_WALL_CLIPBOARD")
            .is_ok_and(|v| v.eq_ignore_ascii_case("memory")),
    };
    event_loop.run_app(&mut app).expect("event loop error");
    println!("the wall drew {} frames", app.frames);
    app.perf.print_summary(app.frames);
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

#[cfg(all(test, feature = "egui-ui"))]
mod tests {
    use super::*;

    fn draw_right_sidebar_area(ctx: &egui::Context, events: Vec<egui::Event>) -> bool {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            events,
            ..Default::default()
        };
        let full = ctx.run_ui(raw, |root_ui| {
            egui::Area::new(egui::Id::new("test_panel"))
                .order(egui::Order::Foreground)
                .fixed_pos(egui::pos2(952.0, 0.0))
                .default_size(egui::vec2(248.0, 800.0))
                .interactable(true)
                .show(root_ui.ctx(), |ui| {
                    if ui.is_sizing_pass() {
                        ui.ctx().request_discard("test_panel initial sizing");
                    }
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(248.0, 800.0), egui::Sense::click_and_drag());
                    ui.painter().rect_filled(rect, 0.0, egui::Color32::BLACK);
                });
        });
        !full.shapes.is_empty()
    }

    #[test]
    fn pointer_target_blocks_clicks_wheels_and_drags_over_open_panels_and_passes_canvas() {
        let ctx = egui::Context::default();

        // Inactive UI: every point goes to the wall.
        assert_eq!(
            pointer_target(false, &ctx, Point::new(1050.0, 200.0), 0),
            PointerTarget::Wall
        );

        // Open docked right panel covering x in [952..1200], y in [0..800].
        let active = draw_right_sidebar_area(&ctx, Vec::new());
        assert!(active);

        // Points inside the open panel route to `Ui` (even before any `PointerMoved` ran in `ctx`);
        // points on the uncovered canvas route to `Wall`.
        assert_eq!(
            pointer_target(true, &ctx, Point::new(1050.0, 200.0), 0),
            PointerTarget::Ui
        );
        assert_eq!(
            pointer_target(true, &ctx, Point::new(400.0, 300.0), 0),
            PointerTarget::Wall
        );

        // Settled hover over the panel sets `is_pointer_over_egui()` / `egui_wants_pointer_input()`;
        // moving off onto the canvas between frames still routes the live canvas point to `Wall`.
        let _ = draw_right_sidebar_area(
            &ctx,
            vec![egui::Event::PointerMoved(egui::pos2(1050.0, 200.0))],
        );
        assert!(ctx.is_pointer_over_egui());
        assert_eq!(
            pointer_target(true, &ctx, Point::new(1050.0, 200.0), 0),
            PointerTarget::Ui
        );
        assert_eq!(
            pointer_target(true, &ctx, Point::new(300.0, 300.0), 0),
            PointerTarget::Wall
        );

        // An active egui drag (`egui_is_using_pointer()`) keeps the pointer even if the cursor moves
        // outside the panel's bounds mid-drag.
        let _ = draw_right_sidebar_area(
            &ctx,
            vec![
                egui::Event::PointerMoved(egui::pos2(1050.0, 200.0)),
                egui::Event::PointerButton {
                    pos: egui::pos2(1050.0, 200.0),
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        let _ = draw_right_sidebar_area(
            &ctx,
            vec![egui::Event::PointerMoved(egui::pos2(900.0, 200.0))],
        );
        assert!(ctx.egui_is_using_pointer());
        assert_eq!(
            pointer_target(true, &ctx, Point::new(900.0, 200.0), 0),
            PointerTarget::Ui
        );

        // Release egui drag; a drag started on the wall (`wall_buttons != 0`) stays with `Wall`
        // even when passing across an open panel.
        let _ = draw_right_sidebar_area(
            &ctx,
            vec![egui::Event::PointerButton {
                pos: egui::pos2(900.0, 200.0),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(!ctx.egui_is_using_pointer());
        assert_eq!(
            pointer_target(true, &ctx, Point::new(1050.0, 200.0), Buttons::LEFT.0),
            PointerTarget::Wall
        );
    }
}
