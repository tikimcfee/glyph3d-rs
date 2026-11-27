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
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use glam::Vec2;
use term_cells::Screen;
use wall_core::config::{CopyOnSelect, OptionAsAlt};
use wall_core::geom::Point;
use wall_core::input::{Button, KeyEvent, Mods, Platform};
use wall_core::keymap::FieldFocus;
#[cfg(feature = "egui-ui")]
use wall_core::keymap::KeyDecision;
use wall_core::layout::Size;
use wall_core::menu::Verdict;
use wall_core::model::{CellGeometry, WallEffect};
use wall_core::pointer::{Buttons, GestureKind, PointerEvent, WheelEvent, DRAG_SLOP_PX};
use wall_core::scroll::DeltaMode;
use wall_core::status::TermdState;
use wall_core::store::FullscreenMode;
use wall_core::wall::{screen_lookup, CharWidths, ScreenFillDelta, TerminalAppeared, Wall, WallFrame as Described, WallOptions as RulesOptions};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use crate::gpu::GpuContext;

use super::camera::WallCamera;
use super::cell::{CellLinkHover, CellSelection};
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
/// Maximum pointer drift in logical px between clicks counted as a double click
/// (interval comes live from `wall.config().click_repeat_ms`).
const DOUBLE_CLICK_PX: f64 = 5.0;
/// Lines scrolled per discrete physical mouse-wheel detent (`MouseScrollDelta::LineDelta`),
/// matching Firefox's 3-line `deltaMode = 1` notch and Chrome's ~100 CSS px notch (`~3.3` rows).
const LINES_PER_WHEEL_NOTCH: f64 = 3.0;
/// Logical pixel delta per discrete physical mouse-wheel detent when a `zoomKeys` modifier is
/// held (`WallCamera.js` `WHEEL_ZOOM.pixelsPerNotch = 100`), so a physical `Ctrl+wheel` notch
/// exceeds `WallPointer`'s `PINCH_MAX_DELTA` (`50.0`) and glides by `stepFactor = 1.12`.
const WHEEL_ZOOM_NOTCH_PX: f64 = 100.0;

/// What a reader thread sends the event loop: something arrived, look.
#[derive(Clone, Copy, Debug)]
pub struct Wake;

/// How the window is opened.
pub struct WallOptions {
    pub emoji_sheet: PathBuf,
    /// The termd namespace whose terminals the wall shows. None: no daemon,
    /// one fixed test screen instead.
    pub namespace: Option<String>,
    /// Extra termd namespaces to attach alongside `namespace` (off by default).
    pub extra_namespaces: Vec<String>,
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

/// Linux `PRIMARY` selection bridge (middle-click paste & copy-on-select).
///
/// Speaks Wayland (`zwp_primary_selection_device_manager_v1` via `smithay-clipboard`) when the
/// window owns a Wayland display handle, and X11 (`XA_PRIMARY` via a persistent `arboard::Clipboard`
/// so selection ownership stays alive after `set_text`) otherwise.
#[cfg(target_os = "linux")]
struct LinuxPrimaryClipboard {
    wayland: Option<smithay_clipboard::Clipboard>,
    x11: Option<arboard::Clipboard>,
}

#[cfg(target_os = "linux")]
impl LinuxPrimaryClipboard {
    fn new(window: &Window) -> Self {
        use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
        if let Ok(handle) = window.display_handle() {
            if let RawDisplayHandle::Wayland(wl) = handle.as_raw() {
                // SAFETY: `Gfx` drops `linux_primary` before `window`, so `wl.display`
                // stays valid for the full lifetime of `smithay_clipboard::Clipboard`.
                let sc = unsafe { smithay_clipboard::Clipboard::new(wl.display.as_ptr()) };
                return Self {
                    wayland: Some(sc),
                    x11: None,
                };
            }
        }
        Self {
            wayland: None,
            x11: arboard::Clipboard::new().ok(),
        }
    }

    fn get(&mut self) -> Option<String> {
        if let Some(wl) = &self.wayland {
            return wl.load_primary().ok().filter(|s| !s.is_empty());
        }
        if let Some(x11) = &mut self.x11 {
            use arboard::{GetExtLinux, LinuxClipboardKind};
            return x11
                .get()
                .clipboard(LinuxClipboardKind::Primary)
                .text()
                .ok()
                .filter(|s| !s.is_empty());
        }
        None
    }

    fn set(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if let Some(wl) = &self.wayland {
            wl.store_primary(text);
            return;
        }
        if let Some(x11) = &mut self.x11 {
            use arboard::{LinuxClipboardKind, SetExtLinux};
            if let Err(e) = x11
                .set()
                .clipboard(LinuxClipboardKind::Primary)
                .text(text)
            {
                log::warn!("primary selection write failed: {e}");
            }
        }
    }
}

/// What exists once there is a window.
///
/// Field order is drop order, and it matters on Wayland: the egui layer
/// and `linux_primary` (whose clipboard workers borrow `wl_display`) and the
/// surface must go before the window (found on workstation, 2026-11-26: a
/// segfault in `wl_proxy_destroy` at every exit).
struct Gfx {
    #[cfg(feature = "egui-ui")]
    ui: Option<WallUi>,
    #[cfg(target_os = "linux")]
    linux_primary: LinuxPrimaryClipboard,
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
    persistent_ns: Option<String>,
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
    /// The renderer keys a terminal's GPU buffers by a monotonic number.
    render_ids: RenderIds,
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
    /// Last drawn scrolled-back jump-to-live pills (back-to-front by Z), for hover/click.
    scrollback_pills: Vec<super::describe::ScrollbackPillHit>,
    /// Terminal whose scrolled-back pill is currently under the pointer.
    hovered_scrollback_pill: Option<String>,
    /// Terminal whose scrolled-back pill received a primary press (released click runs jump-live).
    pressed_scrollback_pill: Option<String>,
    /// Last seen scrollback state per live terminal so async `ViewReply` / `status_follow`
    /// metadata updates trigger a redraw even when `screen.generation` did not change.
    scrollback_snapshot: HashMap<String, super::describe::TerminalScrollback>,
    win: super::window::WindowLifecycle,
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
    /// True while `wallctl app.ime preedit` is active; kept separate from [`Self::ime_composing`]
    /// because Wayland `zwp_text_input_v3.done` replies to earlier `set_ime_cursor_area` commits
    /// make winit emit `Ime::Preedit("", None)` asynchronously.
    script_ime_composing: bool,
    /// Last quantized logical `(x, y, w, h)` sent to `Window::set_ime_cursor_area`.
    last_ime_area: Option<(i32, i32, u32, u32)>,
    #[cfg(target_os = "macos")]
    macos_option_as_alt: Option<winit::platform::macos::OptionAsAlt>,
    /// Monotonic synthetic clock advance (`ms`) applied by timed scripted sequences (`app.wheel_seq`).
    clock_offset_ms: f64,
    /// Last text written by `WallEffect::WriteClipboard` (inspected via `app.clipboard`).
    last_copied: Option<String>,
    /// Process-local clipboard used when `GLYPH_WALL_CLIPBOARD=memory` is set so automated
    /// smoke tests never read or overwrite a user's live OS clipboard.
    memory_clipboard: Option<String>,
    /// Process-local stand-in for Linux's `PRIMARY` selection when `GLYPH_WALL_CLIPBOARD=memory`.
    memory_primary: Option<String>,
    /// Last `wall.model().selection_clipboard()` value synced out to `PRIMARY`.
    last_synced_selection_clip: String,
    /// Where the middle mouse button went down, and whether it travelled past `DRAG_SLOP_PX`.
    middle_press_pos: Option<Point>,
    middle_dragged: bool,
    /// External files currently hovering over the window (`WindowEvent::HoveredFile`).
    hovered_files: Vec<PathBuf>,
    /// External files dropped in the current event batch (`WindowEvent::DroppedFile`),
    /// coalesced into one shell-quoted paste before `about_to_wait` sleeps.
    pending_dropped_files: Vec<PathBuf>,
    use_memory_clipboard: bool,
}

#[derive(Default)]
struct WakeGate {
    pending: std::sync::atomic::AtomicBool,
    redraw_armed: std::sync::atomic::AtomicBool,
}

/// Maps terminal string ids to monotonic `u64` renderer keys so closed terminals can be
/// freed from [`WallRenderer`] without ever colliding ids across spawn/close cycles.
#[derive(Default)]
struct RenderIds {
    next: u64,
    ids: HashMap<String, u64>,
}

impl RenderIds {
    fn id_for(&mut self, id: &str) -> u64 {
        if let Some(&existing) = self.ids.get(id) {
            return existing;
        }
        self.next = self.next.saturating_add(1);
        self.ids.insert(id.to_string(), self.next);
        self.next
    }

    /// Forget `id` and free its cached GPU buffers in `renderer`, if present.
    fn remove_from(&mut self, id: &str, renderer: Option<&mut WallRenderer>) -> bool {
        let Some(rid) = self.ids.remove(id) else {
            return false;
        };
        renderer.is_some_and(|r| r.remove_terminal(rid))
    }

    /// Keep only `live_ids`, freeing cached GPU buffers for terminals absent after reconnect.
    fn retain_live(&mut self, live_ids: &[String], renderer: Option<&mut WallRenderer>) {
        self.ids.retain(|id, _| live_ids.iter().any(|live| live == id));
        if let Some(r) = renderer {
            r.retain_terminals(|rid| self.ids.values().any(|&live_rid| live_rid == rid));
        }
    }
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

/// Launch a scheme-checked external URL (`http`, `https`, `mailto`) via the OS handler
/// (`open` on macOS, `xdg-open` on other Unix platforms) without blocking the event loop.
fn open_external_url(url: &str) {
    if !wall_core::links::is_allowed_url_scheme(url) {
        log::warn!("refused to open URL with disallowed scheme: {url:?}");
        return;
    }
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";
    match std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => log::debug!("opened external URL via {program}: {url}"),
        Err(err) => log::warn!("could not launch {program} for {url}: {err}"),
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

/// Where the wall keeps its state between runs (`wall_core::store::default_store_dir`).
fn store_dir() -> PathBuf {
    wall_core::store::default_store_dir()
}

/// Convert a winit [`MouseScrollDelta`] + [`TouchPhase`] into the browser DOM wheel convention
/// `(delta_x, delta_y, DeltaMode)` expected by `wall_core::pointer::WallPointer`.
///
/// - **Sign & Natural Scrolling**: On macOS (`macos/view.rs`), Wayland (`libinput`), and X11,
///   winit already incorporates the OS natural-scrolling setting and reports positive deltas when
///   content moves right/down (scrolling up/left). DOM `WheelEvent` uses the opposite sign
///   (`+deltaY` scrolls down, moving content up), so both axes negate.
/// - **1:1 Trackpad Pan**: `PixelDelta` is in physical pixels (`scrollingDelta * scale_factor`),
///   so dividing by `scale_factor` produces exact 1:1 logical pixel travel at every camera zoom.
/// - **Mouse Wheel Detents (`LineDelta`)**: winit reports `±1.0` per physical wheel detent on
///   Wayland (`discrete`), X11 (`Button4/5` / `XInput2`), and macOS non-precise mice. Plain
///   scroll scales by [`LINES_PER_WHEEL_NOTCH`] (`3.0` lines in `DeltaMode::Line`, keeping
///   `TerminalScroller.trackpad = false`); wheel-zoom scales by [`WHEEL_ZOOM_NOTCH_PX`]
///   (`100.0` px in `DeltaMode::Pixel`) so one detent glides by `WHEEL_ZOOM.step_factor` (`1.12x`)
///   instead of falling under `WallPointer`'s `50 px` trackpad-pinch heuristic.
/// - **TouchPhase & Momentum**: Zero-delta bookends (`MayBegin`, `Ended`, Wayland `axis_stop`)
///   and `TouchPhase::Cancelled` return `None` so they do not extend `WheelStreamGate`'s `140 ms`
///   quiet window after a gesture ends. `TouchPhase::Started` / `Ended` deliberately do **not**
///   reset `WheelStreamGate`, because macOS emits `Ended` (finger lift) followed ~8 ms later by
///   `Started` (`momentumPhase == Began`); keeping the latch intact across that transition stops
///   a canvas pan flick from hijacking a focused terminal that slides under a stationary cursor.
fn convert_scroll_delta(
    delta: MouseScrollDelta,
    phase: TouchPhase,
    scale_factor: f64,
    mods: Mods,
    zoom_keys: &str,
) -> Option<(f64, f64, DeltaMode)> {
    if phase == TouchPhase::Cancelled {
        return None;
    }
    let sf = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };
    let (dx, dy, mode) = match delta {
        MouseScrollDelta::PixelDelta(PhysicalPosition { x, y }) => {
            (-x / sf, -y / sf, DeltaMode::Pixel)
        }
        MouseScrollDelta::LineDelta(x, y) => {
            let (lx, ly) = (f64::from(x), f64::from(y));
            let zooming = wall_core::chord::zooms_wheel(mods, zoom_keys).unwrap_or(false);
            if zooming {
                (
                    -lx * WHEEL_ZOOM_NOTCH_PX,
                    -ly * WHEEL_ZOOM_NOTCH_PX,
                    DeltaMode::Pixel,
                )
            } else {
                (
                    -lx * LINES_PER_WHEEL_NOTCH,
                    -ly * LINES_PER_WHEEL_NOTCH,
                    DeltaMode::Line,
                )
            }
        }
    };
    if !dx.is_finite() || !dy.is_finite() || (dx == 0.0 && dy == 0.0) {
        return None;
    }
    Some((dx, dy, mode))
}

/// Convert a winit [`WindowEvent::PinchGesture`] `delta` (`NSEvent.magnification`) into the
/// multiplicative zoom factor passed to `Wall::pinch`.
///
/// Browsers map `magnification` to `WheelEvent { ctrlKey: true, deltaY: -100 * delta }`, which
/// `WallPointer.js` turns into `Math.exp(-dy * 0.01) = Math.exp(delta)`. `delta.exp()` matches
/// that exact rate, stays strictly positive for every finite `delta`, and composes exact across
/// frame subdivisions (`exp(a) * exp(b) == exp(a + b)`).
fn pinch_zoom_factor(delta: f64, phase: TouchPhase) -> Option<f64> {
    if phase == TouchPhase::Cancelled || !delta.is_finite() || delta == 0.0 {
        return None;
    }
    Some(delta.exp())
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SmartZoomTarget {
    Terminal(String),
    Group(String),
    Canvas,
}

fn smart_zoom_target_at(described: &Described, pt: Point) -> SmartZoomTarget {
    let vw = described.viewport.w;
    let vh = described.viewport.h;
    let cam = described.camera;

    let mut best_term: Option<(f64, &str)> = None;
    for t in &described.terminals {
        let sb = cam.to_screen_box(t.body_rect, vw, vh);
        let in_body = pt.x >= sb.left && pt.x <= sb.right && pt.y >= sb.top && pt.y <= sb.bottom;
        let in_bar = described.title_bars.iter().any(|b| {
            if b.terminal_id != t.id {
                return false;
            }
            let bb = cam.to_screen_box(b.rect, vw, vh);
            pt.x >= bb.left && pt.x <= bb.right && pt.y >= bb.top && pt.y <= bb.bottom
        });
        if (in_body || in_bar) && best_term.is_none_or(|(z, _)| t.z >= z) {
            best_term = Some((t.z, t.id.as_str()));
        }
    }
    if let Some((_, id)) = best_term {
        return SmartZoomTarget::Terminal(id.to_string());
    }
    for g in &described.groups {
        let hb = cam.to_screen_box(g.header_rect, vw, vh);
        if pt.x >= hb.left && pt.x <= hb.right && pt.y >= hb.top && pt.y <= hb.bottom {
            return SmartZoomTarget::Group(g.id.clone());
        }
    }
    SmartZoomTarget::Canvas
}

/// Quote `path` as a POSIX shell token suitable for pasting into an interactive shell.
///
/// Plain ASCII paths containing only `[A-Za-z0-9/._\-+@:,%=]` stay unquoted; every other path
/// (spaces, quotes, `$`, globs, parens, backslashes, newlines, non-ASCII) is wrapped in single
/// quotes with embedded `'` escaped as `'\''`.
fn shell_quote_path(path: &std::path::Path) -> String {
    let raw = path.to_string_lossy();
    let safe_unquoted = !raw.is_empty()
        && raw.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'/' | b'.' | b'_' | b'-' | b'+' | b'@' | b':' | b',' | b'%' | b'='
                )
        });
    if safe_unquoted {
        raw.into_owned()
    } else {
        format!("'{}'", raw.replace('\'', r"'\''"))
    }
}

/// Format one or more dropped file paths as a space-separated shell-quoted string with a
/// trailing space (matching Terminal.app, Ghostty, kitty, and WezTerm).
fn format_dropped_paths(paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let mut out = paths
        .iter()
        .map(|p| shell_quote_path(p))
        .collect::<Vec<_>>()
        .join(" ");
    out.push(' ');
    out
}

fn hovered_drop_notice(paths: &[PathBuf]) -> Option<String> {
    match paths {
        [] => None,
        [only] => {
            let label = only
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| only.display().to_string());
            Some(format!("drop to paste path: {label}"))
        }
        many => Some(format!("drop to paste {} paths", many.len())),
    }
}

/// Find the terminal body hit by a middle-click paste at `pt`, matching `WallPointer::on_down`
/// (`HitPart::Body` inside the 6 px resize-edge band on the open canvas, or the active pane in
/// fullscreen).
fn middle_paste_target_at(described: &Described, pt: Point) -> Option<String> {
    const EDGE_GRAB_PX: f64 = 6.0;
    if let Some(fb) = &described.fullscreen_backdrop {
        if fb.mode == FullscreenMode::Single {
            return Some(fb.target.clone());
        }
    }
    let vw = described.viewport.w;
    let vh = described.viewport.h;
    let cam = described.camera;
    let mut best: Option<(f64, &str)> = None;
    for t in &described.terminals {
        let sb = cam.to_screen_box(t.body_rect, vw, vh);
        let inset_x = if sb.right - sb.left > EDGE_GRAB_PX * 3.0 {
            EDGE_GRAB_PX
        } else {
            0.0
        };
        let inset_y = if sb.bottom - sb.top > EDGE_GRAB_PX * 3.0 {
            EDGE_GRAB_PX
        } else {
            0.0
        };
        if pt.x >= sb.left + inset_x
            && pt.x <= sb.right - inset_x
            && pt.y >= sb.top + inset_y
            && pt.y <= sb.bottom - inset_y
            && best.is_none_or(|(z, _)| t.z >= z)
        {
            best = Some((t.z, t.id.as_str()));
        }
    }
    best.map(|(_, id)| id.to_string())
}

impl App {
    fn scale_factor(&self) -> f64 {
        self.win.scale_factor()
    }

    fn now_ms(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0 + self.clock_offset_ms
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
        self.render_ids.id_for(id)
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
        self.persistent_ns.clone_from(&opts.namespace);

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
        let pending_termd = opts.namespace.as_ref().map(|ns| {
            super::startup::PendingTermd::spawn_with_extra(
                ns.clone(),
                opts.extra_namespaces.clone(),
                session_wake,
            )
        });

        let store = store_dir();
        let window = {
            let _s = super::startup::span("window_create");
            let (win_attrs, restore_fullscreen) = self.win.prepare_open(event_loop, &store, opts.namespace.as_deref());
            let w = Arc::new(event_loop.create_window(win_attrs).expect("window creation failed"));
            if restore_fullscreen {
                w.set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
            }
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
        #[cfg(target_os = "linux")]
        let linux_primary = LinuxPrimaryClipboard::new(&window);
        let sf = self.win.finish_open(&window, WallStyle::default(), renderer.style_mut());

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
            if opts.namespace.is_some() { Wall::open(store, namespace.clone(), rules) } else { Wall::in_memory(rules) }
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
            #[cfg(target_os = "linux")]
            linux_primary,
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
            Ok(mut ready) => {
                let welcome = ready.sessions.hub().welcome().clone();
                if let Some(wall) = &self.wall {
                    ready.sessions.set_bold_is_bright(wall.config().bold_is_bright);
                }
                let mut appeared_list = Vec::with_capacity(ready.adopted.len());
                for (id, cols, rows) in ready.adopted {
                    let mut appeared =
                        TerminalAppeared::adopted(id.clone(), u32::from(cols), u32::from(rows));
                    if let Some(title) = ready.sessions.default_title_for(&id) {
                        appeared = appeared.with_title(title);
                    }
                    appeared_list.push(appeared);
                }
                self.terms = Some(Terminals::Live(Box::new(ready.sessions)));
                if let Some(wall) = &mut self.wall {
                    wall.on_termd_welcome(
                        welcome.instance.as_deref().unwrap_or(""),
                        &welcome.features,
                        epoch_now_ms(),
                    );
                    wall.set_live_terminals(&ready.live_ids);
                    for appeared in appeared_list {
                        wall.terminal_appeared(appeared);
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

    fn is_ime_composing(&self) -> bool {
        self.ime_composing || self.script_ime_composing
    }

    fn sync_ime_cursor_area(&mut self, described: &Described) {
        // On Wayland (`zwp_text_input_v3`), `set_ime_cursor_area` sends `text_input.commit()`,
        // whose `done` reply without a new `preedit_string` makes winit emit `Ime::Preedit("", None)`
        // and abort an active composition. Defer cursor-rect updates until composition finishes.
        if self.is_ime_composing() {
            return;
        }
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
        let now_ms = self.now_ms();
        let (Some(g), Some(wall)) = (&mut self.gfx, &mut self.wall) else { return };
        wall.set_now_ms(now_ms);
        let cfg = wall.config();
        let family = cfg.font_family.clone();
        let font_size = cfg.font_size;
        let line_height = cfg.line_height;
        let sf = self.win.scale_factor() as f32;
        let greek_px = cfg.greek_px as f32;
        self.win.set_base_greek_px(greek_px);
        let style = g.renderer.style_mut();
        style.panel_opacity = cfg.panel_opacity as f32;
        style.greek_px = greek_px * sf;
        style.cursor_blink_ms = cfg.cursor_blink_ms.clamp(0.0, f64::from(u32::MAX)).round() as u32;
        style.line_thickness_scale = cfg.line_thickness as f32;
        style.underline_raise = cfg.underline_raise as f32;
        style.curly_amplitude = cfg.curly_amplitude as f32;
        style.dim_alpha = cfg.dim_alpha as f32;

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

    fn read_primary_selection(&mut self) -> Option<String> {
        if self.use_memory_clipboard {
            return self.memory_primary.clone().filter(|s| !s.is_empty());
        }
        #[cfg(target_os = "linux")]
        if let Some(g) = &mut self.gfx {
            return g.linux_primary.get();
        }
        None
    }

    fn write_primary_selection(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if self.use_memory_clipboard {
            self.memory_primary = Some(text);
        } else {
            #[cfg(target_os = "linux")]
            if let Some(g) = &mut self.gfx {
                g.linux_primary.set(text);
            }
            #[cfg(not(target_os = "linux"))]
            let _ = text;
        }
    }

    fn drop_files(&mut self, paths: &[PathBuf]) {
        let payload = format_dropped_paths(paths);
        if payload.is_empty() {
            return;
        }
        if let Some(wall) = &mut self.wall {
            if wall.menu().is_some() {
                let _ = wall.run("wall.menu close");
            }
            let described = wall.frame();
            let target = if let Some(fb) = &described.fullscreen_backdrop {
                match fb.mode {
                    FullscreenMode::Single => Some(fb.target.clone()),
                    FullscreenMode::Group => match smart_zoom_target_at(&described, self.cursor) {
                        SmartZoomTarget::Terminal(id) => {
                            if wall.focused_id() != Some(id.as_str()) {
                                let _ = wall.run(format!("terminal.focus {id}"));
                            }
                            Some(id)
                        }
                        _ => wall.focused_id().map(str::to_owned),
                    },
                }
            } else {
                match smart_zoom_target_at(&described, self.cursor) {
                    SmartZoomTarget::Terminal(id) => {
                        if wall.focused_id() != Some(id.as_str()) {
                            let _ = wall.run(format!("terminal.focus {id}"));
                        }
                        Some(id)
                    }
                    _ => wall.focused_id().map(str::to_owned),
                }
            };
            if let Some(id) = target {
                wall.deliver_clipboard(&id, &payload);
            }
        }
        self.apply_effects();
        self.redraw();
    }

    fn flush_dropped_files(&mut self) {
        if self.pending_dropped_files.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut self.pending_dropped_files);
        self.drop_files(&batch);
    }

    fn apply(&mut self, effect: WallEffect) {
        let effect = match effect {
            WallEffect::WriteClipboard { text } => {
                self.write_clipboard(text);
                return;
            }
            WallEffect::ReadClipboard { target } => {
                let now_ms = self.now_ms();
                if let (Some(text), Some(wall)) = (self.read_clipboard(), &mut self.wall) {
                    wall.set_now_ms(now_ms);
                    wall.deliver_clipboard(&target, &text);
                }
                return;
            }
            WallEffect::OpenUrl { url } => {
                open_external_url(&url);
                return;
            }
            WallEffect::ConfigKnobChanged { key, value } => {
                log::debug!("wall knob {key} is now {value:?}");
                if key == "optionAsAlt" {
                    self.sync_macos_option_as_alt();
                }
                if key == "boldIsBright" {
                    if let (Some(wall), Some(Terminals::Live(sessions))) =
                        (&self.wall, &mut self.terms)
                    {
                        sessions.set_bold_is_bright(wall.config().bold_is_bright);
                    }
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
                self.win.set_cursor(self.gfx.as_ref().map(|g| g.window.as_ref()), cursor);
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
                Some("wall.import") => {
                    let reply = self.handle_wall_import(&argv);
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
            let now_ms = self.now_ms();
            let reply = match (&mut self.wall, &self.terms) {
                (Some(wall), Some(terms)) => {
                    wall.set_now_ms(now_ms);
                    wall.run_with_screens(line.as_str(), &screen_lookup(|id| terms.screen(id))).text
                }
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

    /// Import a browser-wall JSON export into this namespace's native [`WallStore`]
    /// and re-adopt live terminals against the imported layout, groups, splits, names,
    /// panels, and config knobs.
    fn handle_wall_import(&mut self, argv: &[String]) -> String {
        if !(2..=3).contains(&argv.len()) {
            return "ERR: usage: wall.import <browser-export.json> [browser-scope]".to_string();
        }
        let Some(ns) = self.persistent_ns.clone() else {
            return "ERR: wall.import requires a namespace-backed wall (not --wall-test-pattern)"
                .to_string();
        };
        let dir = store_dir();
        let export_path = std::path::Path::new(&argv[1]);
        let source_scope = argv.get(2).map(String::as_str);
        let report = match wall_core::import::import_browser_file(export_path, source_scope, &dir, &ns) {
            Ok(r) => r,
            Err(msg) => return format!("ERR: {msg}"),
        };
        if let Some(g) = &self.gfx {
            let was_connected = self
                .wall
                .as_ref()
                .is_none_or(|w| w.model().snapshot().connected);
            let metrics = g.renderer.metrics();
            let sf = g.window.scale_factor();
            let rules = RulesOptions {
                geometry: CellGeometry::with_padding(
                    f64::from(metrics.cell_width * g.cell_scale),
                    f64::from(metrics.cell_height * g.cell_scale),
                    f64::from(PANEL_PAD),
                    f64::from(PANEL_PAD),
                ),
                platform: Platform::host(),
                viewport: Size::new(
                    f64::from(g.config.width) / sf,
                    f64::from(g.config.height) / sf,
                ),
                scale_factor: sf,
                namespace: ns.clone(),
                ..RulesOptions::default()
            };
            let mut wall = Wall::open(dir, ns, rules);
            if let Some(Terminals::Live(sessions)) = &self.terms {
                let welcome = sessions.hub().welcome();
                wall.on_termd_welcome(
                    welcome.instance.as_deref().unwrap_or(""),
                    &welcome.features,
                    epoch_now_ms(),
                );
                let live_ids: Vec<String> = sessions.ids().map(str::to_string).collect();
                if !live_ids.is_empty() {
                    wall.set_live_terminals(&live_ids);
                }
                for id in &live_ids {
                    let (cols, rows, title, modes) = sessions.screen(id).map_or(
                        (
                            wall_core::wall::DEFAULT_SPAWN_COLS,
                            wall_core::wall::DEFAULT_SPAWN_ROWS,
                            String::new(),
                            term_cells::Modes::default(),
                        ),
                        |s| (u32::from(s.cols), u32::from(s.rows), s.title.clone(), s.modes),
                    );
                    wall.terminal_appeared(
                        TerminalAppeared::adopted(id.clone(), cols, rows)
                            .with_title(title)
                            .with_modes(modes),
                    );
                }
                wall.set_connected(was_connected);
            }
            self.wall = Some(wall);
            if let (Some(wall), Some(Terminals::Live(sessions))) = (&self.wall, &mut self.terms) {
                sessions.set_bold_is_bright(wall.config().bold_is_bright);
            }
            self.sync_font_and_text_knobs(None);
            self.sync_macos_option_as_alt();
            self.apply_effects();
            self.refresh_termd_status();
            if let Some(wall) = &mut self.wall {
                if wall.frame().fullscreen_backdrop.is_none() {
                    wall.run("wall.fit");
                }
            }
        }
        report.summary_text()
    }

    /// Take in what the terminals' reader thread queued.
    fn pump(&mut self, request_redraw: bool) {
        let _perf = self.perf.pump_scope();
        let (Some(wall), Some(Terminals::Live(sessions))) = (&mut self.wall, &mut self.terms) else { return };
        let (mut changed, news) = sessions.pump();
        // Catch metadata-only scrollback updates (`ViewReply` offset/max or
        // `status_follow` scrollback_rows arriving after `Event::Repaint`).
        let mut live_count = 0usize;
        for id in sessions.ids() {
            live_count += 1;
            if let Some(sb) = sessions.scrollback(id) {
                let next = super::describe::TerminalScrollback {
                    offset: sb.offset,
                    max: sb.max,
                    viewing: sb.viewing,
                };
                if self.scrollback_snapshot.get(id).copied() != Some(next) {
                    self.scrollback_snapshot.insert(id.to_owned(), next);
                    changed = true;
                }
            }
        }
        if self.scrollback_snapshot.len() != live_count {
            self.scrollback_snapshot.retain(|id, _| sessions.screen(id).is_some());
            changed = true;
        }
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
                    self.render_ids.remove_from(&id, self.gfx.as_mut().map(|g| &mut g.renderer));
                    heard = true;
                    ended_any = true;
                }
                News::Status(doc) => {
                    let status_open = wall.store().status_window().open;
                    let prev_state = wall.termd().state();
                    let prev_terms = wall.termd().doc().map_or(0, |d| d.terminals.len());
                    let prev_errors = wall.termd().doc().map_or(0, |d| d.errors.len());
                    let _ = wall.on_termd_status(doc, epoch_now_ms());
                    let next_state = wall.termd().state();
                    let next_terms = wall.termd().doc().map_or(0, |d| d.terminals.len());
                    let next_errors = wall.termd().doc().map_or(0, |d| d.errors.len());
                    if status_open
                        || next_state != prev_state
                        || next_terms != prev_terms
                        || next_errors != prev_errors
                    {
                        heard = true;
                    }
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
            self.render_ids.retain_live(&live_ids, self.gfx.as_mut().map(|g| &mut g.renderer));
            for id in &live_ids {
                match sessions.adopt(id) {
                    Ok((cols, rows)) => {
                        let mut appeared = TerminalAppeared::adopted(id.clone(), u32::from(cols), u32::from(rows));
                        if let Some(title) = sessions.default_title_for(id) {
                            appeared = appeared.with_title(title);
                        }
                        wall.terminal_appeared(appeared);
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
                    let mut appeared = TerminalAppeared::adopted(id.clone(), u32::from(cols), u32::from(rows));
                    if let Some(title) = sessions.default_title_for(&id) {
                        appeared = appeared.with_title(title);
                    }
                    wall.terminal_appeared(appeared);
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
        let Some(sync) = self.win.on_resize(&g.window, width, height, g.renderer.style_mut(), self.wall.as_mut()) else {
            return;
        };
        g.config.width = sync.physical_width;
        g.config.height = sync.physical_height;
        g.surface.configure(&self.ctx.device, &g.config);
        g.depth = crate::scene::create_depth(&self.ctx.device, WALL_DEPTH_FORMAT, sync.physical_width, sync.physical_height);
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
        if matches!(wall.termd().state(), TermdState::Restarting | TermdState::Unreachable)
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
        if described.notice.is_none() {
            described.notice = hovered_drop_notice(&self.hovered_files);
        }
        let ids: Vec<(String, u64)> = described.terminals.iter().map(|t| (t.id.clone(), 0)).collect();
        let ids: Vec<(String, u64)> = ids.into_iter().map(|(id, _)| (id.clone(), self.render_id(&id))).collect();

        let (Some(g), Some(terms)) = (&mut self.gfx, &self.terms) else { return };
        use wgpu::CurrentSurfaceTexture as Cst;
        let t_acq = Instant::now();
        let frame = if to_surface && !self.win.is_minimized_or_zero() {
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
        let sf = self.win.scale_factor() as f32;
        // The wall's camera is in logical pixels per world unit; the
        // renderer's is in physical ones.
        let camera = WallCamera::new(Vec2::new(described.camera.x as f32, described.camera.y as f32), described.camera.zoom as f32 * sf);
        let viewport = (g.config.width, g.config.height);

        let mut draws: Vec<TerminalDraw<'_>> = Vec::with_capacity(described.terminals.len());
        for (t, (_, render_id)) in described.terminals.iter().zip(&ids) {
            let Some(screen) = terms.screen(&t.id) else { continue };
            let (focused, border_color) = self.win.terminal_draw_focus(t.focused, t.hovered);
            draws.push(TerminalDraw {
                id: *render_id,
                screen,
                position: v2(t.cell_origin),
                z: t.z as f32,
                scale: g.cell_scale * t.scale as f32,
                focused,
                panel_opacity: None,
                border_color,
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
                hover_link: t.hovered_link.as_ref().map(|link| {
                    CellLinkHover::new(
                        link.start_row.min(usize::from(u16::MAX)) as u16,
                        link.start_col.min(usize::from(u16::MAX)) as u16,
                        link.end_row.min(usize::from(u16::MAX)) as u16,
                        link.end_col.min(usize::from(u16::MAX)) as u16,
                    )
                }),
            });
        }

        let mut shapes: Vec<Shape> = Vec::new();
        let mut labels: Vec<Label<'static>> = Vec::new();
        let mut pill_hits: Vec<super::describe::ScrollbackPillHit> = Vec::new();
        if described.notice.is_none() {
            described.notice = self
                .starting_termd
                .then(|| "starting termd...".to_string())
                .or_else(|| g.renderer.shader_error_notice().map(str::to_owned));
        }
        let paper = {
            let metrics = g.renderer.metrics();
            let family = g.renderer.font_family();
            let mut fonts = super::chrome::ChromeFonts::new(metrics, g.renderer.glyphs_mut().atlas_mut(), family);
            super::describe::describe_frame_chrome_with_options(
                &described,
                &camera,
                viewport,
                sf,
                &mut fonts,
                &mut shapes,
                &mut labels,
                |id| terms.scrollback(id),
                self.hovered_scrollback_pill.as_deref(),
                &mut pill_hits,
                !cfg!(feature = "egui-ui"),
            )
        };
        if let Some(ref hid) = self.hovered_scrollback_pill {
            if !pill_hits.iter().any(|p| p.terminal_id == *hid) {
                self.hovered_scrollback_pill = None;
            }
        }
        self.scrollback_pills = pill_hits;
        let wall_frame = WallFrame { paper: Some(&paper), terminals: &draws, shapes: &shapes, labels: &labels };
        self.blink_repaint_at = g.renderer.next_cursor_blink(&draws, Instant::now());
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
                if !ui_frame.wants_pointer {
                    self.win.restore_cursor(&g.window);
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

    fn hit_scrollback_pill(&self, x: f64, y: f64) -> Option<&str> {
        self.scrollback_pills
            .iter()
            .rev()
            .find(|p| p.contains_screen(x, y))
            .map(|p| p.terminal_id.as_str())
    }

    fn move_cursor(&mut self, x: f64, y: f64) {
        self.cursor = Point::new(x, y);
        if (self.buttons & Buttons::MIDDLE.0) != 0
            && self
                .middle_press_pos
                .is_some_and(|p| (x - p.x).hypot(y - p.y) > DRAG_SLOP_PX)
        {
            self.middle_dragged = true;
        }
        if let Some(pressed_tid) = self.pressed_scrollback_pill.as_deref() {
            let over = (self.hit_scrollback_pill(x, y) == Some(pressed_tid))
                .then(|| pressed_tid.to_owned());
            if over != self.hovered_scrollback_pill {
                self.hovered_scrollback_pill = over;
                self.redraw();
            }
            return;
        }
        let hovered_pill = if self.buttons == 0 {
            self.hit_scrollback_pill(x, y).map(str::to_owned)
        } else {
            None
        };
        let pill_hover_changed = hovered_pill != self.hovered_scrollback_pill;
        self.hovered_scrollback_pill = hovered_pill;
        let ev = self.pointer(None);
        if let (Some(wall), Some(terms)) = (&mut self.wall, &self.terms) {
            let lookup = screen_lookup(|id| terms.screen(id));
            wall.pointer_move(&ev, &lookup);
        }
        self.apply_effects();
        if self.hovered_scrollback_pill.is_some() {
            if let Some(g) = &self.gfx {
                g.window.set_cursor(winit::window::CursorIcon::Pointer);
            }
        }
        if pill_hover_changed {
            self.redraw();
        }
    }

    fn cursor_left(&mut self) {
        let mut changed = self.hovered_scrollback_pill.take().is_some();
        if let Some(wall) = &mut self.wall {
            if wall.pointer_leave() {
                changed = true;
            }
        }
        self.apply_effects();
        if changed {
            self.redraw();
        }
    }

    fn pinch(&mut self, delta: f64, phase: TouchPhase) {
        let factor = pinch_zoom_factor(delta, phase);
        if let Some(wall) = &mut self.wall {
            if (phase == TouchPhase::Started || factor.is_some()) && wall.menu().is_some() {
                let _ = wall.run("wall.menu close");
            }
            if wall.fullscreen().active() {
                self.apply_effects();
                self.redraw();
                return;
            }
            if let Some(f) = factor {
                wall.pinch(self.cursor.x, self.cursor.y, f);
            }
        }
        self.apply_effects();
        self.redraw();
    }

    fn smart_zoom(&mut self) {
        if let Some(wall) = &mut self.wall {
            if wall.menu().is_some() {
                let _ = wall.run("wall.menu close");
            }
            if wall.fullscreen().active() {
                self.apply_effects();
                self.redraw();
                return;
            }
            let described = wall.frame();
            match smart_zoom_target_at(&described, self.cursor) {
                SmartZoomTarget::Terminal(id) => {
                    let pre = wall.camera().clone();
                    let _ = wall.run(format!("wall.reveal {id}"));
                    if wall.camera().target().is_some_and(|t| pre.is_at(t)) {
                        let _ = wall.run("wall.fit");
                    }
                }
                SmartZoomTarget::Group(gid) => {
                    let pre = wall.camera().clone();
                    let _ = wall.run(format!("wall.group.frame {gid}"));
                    if wall.camera().target().is_some_and(|t| pre.is_at(t)) {
                        let _ = wall.run("wall.fit");
                    }
                }
                SmartZoomTarget::Canvas => {
                    let _ = wall.run("wall.fit");
                }
            }
        }
        self.apply_effects();
        self.redraw();
    }

    fn wheel_at(&mut self, delta: MouseScrollDelta, phase: TouchPhase, time_ms: f64) {
        let sf = self.scale_factor();
        let mods = self.mods();
        let zoom_keys = self
            .wall
            .as_ref()
            .map_or("ctrl alt meta", |w| w.config().zoom_keys.as_str());
        let Some((dx, dy, mode)) = convert_scroll_delta(delta, phase, sf, mods, zoom_keys) else {
            if phase == TouchPhase::Started {
                if let Some(wall) = &mut self.wall {
                    if wall.menu().is_some() {
                        let _ = wall.run("wall.menu close");
                        self.apply_effects();
                        self.redraw();
                    }
                }
            }
            return;
        };
        let ev = WheelEvent {
            x: self.cursor.x,
            y: self.cursor.y,
            delta_x: dx,
            delta_y: dy,
            delta_mode: mode,
            mods,
            time_ms,
        };
        if let Some(wall) = &mut self.wall {
            wall.wheel(&ev);
        }
        self.apply_effects();
        self.redraw();
    }

    fn wheel(&mut self, delta: MouseScrollDelta, phase: TouchPhase) {
        let time_ms = self.now_ms();
        self.wheel_at(delta, phase, time_ms);
    }

    fn key_input(&mut self, key: KeyEvent) {
        if self.is_ime_composing() && key.pressed {
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
        let now_ms = self.now_ms();
        let (Some(wall), Some(terms)) = (&mut self.wall, &self.terms) else { return };
        wall.set_now_ms(now_ms);
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
        self.win.set_window_focused(focused);
        if !focused {
            self.modifiers = ModifiersState::empty();
            self.ime_composing = false;
            self.hovered_files.clear();
            self.pressed_scrollback_pill = None;
            self.script_ime_composing = false;
            self.buttons = 0;
            if let Some(wall) = &mut self.wall {
                wall.blur();
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
                        let mut out = super::script::format_where(&described, self.cursor, wall.platform());
                        out.push_str(&format!(
                            "\npaper grid_show={} grid_minor={}",
                            described.paper.grid_show, described.paper.grid.grid_minor
                        ));
                        if let Some(g) = &self.gfx {
                            out.push_str(&format!(
                                "\nrenderer cached_terminals={}",
                                g.renderer.cached_terminal_count()
                            ));
                        }
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
            ScriptCommand::TplClick { x, y } => {
                self.last_release = None;
                self.move_cursor(x, y);
                for _ in 0..3 {
                    self.mouse(ElementState::Pressed, MouseButton::Left);
                    self.mouse(ElementState::Released, MouseButton::Left);
                }
                self.last_release = None;
                format!("OK: tplclick at ({x:.2}, {y:.2})")
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
            ScriptCommand::Wheel { x, y, dx, dy, unit, phase, dt_ms, mods } => {
                use super::script::ScriptScrollUnit;
                if let Some(dt) = dt_ms {
                    self.clock_offset_ms += dt;
                }
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
                    let delta = match unit {
                        ScriptScrollUnit::Pixel => {
                            let sf = self.scale_factor();
                            MouseScrollDelta::PixelDelta(PhysicalPosition::new(dx * sf, dy * sf))
                        }
                        ScriptScrollUnit::Line => MouseScrollDelta::LineDelta(dx as f32, dy as f32),
                    };
                    self.wheel(delta, phase);
                }
                self.draw(false);
                self.modifiers = prev;
                #[cfg(feature = "egui-ui")]
                if let Some(ui) = self.gfx.as_mut().and_then(|g| g.ui.as_mut()) {
                    ui.set_script_modifiers(super::ui::winit_mods_to_egui(prev));
                }
                format!("OK: wheel ({dx:.2}, {dy:.2}) at ({x:.2}, {y:.2})")
            }
            ScriptCommand::WheelSeq { x, y, mods, steps } => {
                use super::script::ScriptScrollUnit;
                let prev = self.modifiers;
                self.modifiers = mods;
                self.script_move(x, y, true);
                #[cfg(feature = "egui-ui")]
                let ui_pointer = self.script_pointer_goes_to_ui();
                #[cfg(not(feature = "egui-ui"))]
                let ui_pointer = false;
                let sf = self.scale_factor();
                let mut t_ms = self.now_ms();
                let n = steps.len();
                for step in steps {
                    if ui_pointer {
                        break;
                    }
                    t_ms += step.dt_ms;
                    let delta = match step.unit {
                        ScriptScrollUnit::Pixel => MouseScrollDelta::PixelDelta(
                            PhysicalPosition::new(step.dx * sf, step.dy * sf),
                        ),
                        ScriptScrollUnit::Line => {
                            MouseScrollDelta::LineDelta(step.dx as f32, step.dy as f32)
                        }
                    };
                    self.wheel_at(delta, step.phase, t_ms);
                }
                let live_now = self.start.elapsed().as_secs_f64() * 1000.0;
                if t_ms > live_now + self.clock_offset_ms {
                    self.clock_offset_ms = t_ms - live_now;
                }
                self.modifiers = prev;
                format!("OK: wheel_seq {n} step(s) at ({x:.2}, {y:.2})")
            }
            ScriptCommand::Pinch { x, y, amount, phase } => {
                use super::script::ScriptPinchAmount;
                self.script_move(x, y, true);
                #[cfg(feature = "egui-ui")]
                let ui_pointer = self.script_pointer_goes_to_ui();
                #[cfg(not(feature = "egui-ui"))]
                let ui_pointer = false;
                let delta = match amount {
                    ScriptPinchAmount::Factor(factor) => factor.ln(),
                    ScriptPinchAmount::Delta(d) => d,
                };
                if !ui_pointer {
                    self.pinch(delta, phase);
                }
                format!("OK: pinch delta={delta:.4} at ({x:.2}, {y:.2})")
            }
            ScriptCommand::DoubleTap { at } => {
                if let Some((x, y)) = at {
                    self.move_cursor(x, y);
                }
                self.smart_zoom();
                format!("OK: doubletap at ({:.2}, {:.2})", self.cursor.x, self.cursor.y)
            }
            ScriptCommand::Drop { at, paths } => {
                if let Some((x, y)) = at {
                    self.move_cursor(x, y);
                }
                self.hovered_files.clear();
                self.pending_dropped_files.clear();
                let count = paths.len();
                self.drop_files(&paths);
                format!("OK: dropped {count} file(s)")
            }
            ScriptCommand::HoverFile(paths) => {
                let count = paths.len();
                self.hovered_files = paths;
                self.redraw();
                format!("OK: hovering {count} file(s)")
            }
            ScriptCommand::CancelFile => {
                self.hovered_files.clear();
                self.redraw();
                "OK: hover file cancelled".to_string()
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
                        self.script_ime_composing = !text.is_empty();
                        "OK: ime preedit".to_string()
                    }
                    ScriptIme::Commit(text) => {
                        self.script_ime_composing = false;
                        self.ime_commit(&text);
                        "OK: ime commit".to_string()
                    }
                    ScriptIme::Disable => {
                        self.script_ime_composing = false;
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
            ScriptCommand::Primary(clip_cmd) => {
                use super::script::ScriptClipboard;
                match clip_cmd {
                    ScriptClipboard::Get => self.read_primary_selection().unwrap_or_default(),
                    ScriptClipboard::Clear => {
                        if self.use_memory_clipboard {
                            self.memory_primary = None;
                        }
                        self.last_synced_selection_clip.clear();
                        "OK: primary cleared".to_string()
                    }
                    ScriptClipboard::Set(text) => {
                        if !self.use_memory_clipboard {
                            "ERR: app.primary set requires GLYPH_WALL_CLIPBOARD=memory".to_string()
                        } else {
                            self.memory_primary = Some(text);
                            "OK: primary set".to_string()
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
        if b == Button::Left && !secondary {
            if pressed {
                if let Some(tid) = self.hit_scrollback_pill(ev.x, ev.y).map(str::to_owned) {
                    self.pressed_scrollback_pill = Some(tid);
                    return;
                }
            } else if let Some(pressed_tid) = self.pressed_scrollback_pill.take() {
                let still_inside = self.hit_scrollback_pill(ev.x, ev.y) == Some(pressed_tid.as_str());
                if still_inside {
                    if let (Some(wall), Some(terms)) = (&mut self.wall, &self.terms) {
                        let lookup = screen_lookup(|id| terms.screen(id));
                        let _ = wall.run_with_screens(
                            format!("terminal.scroll {pressed_tid} live"),
                            &lookup,
                        );
                    }
                    self.hovered_scrollback_pill = None;
                    self.apply_effects();
                    self.redraw();
                }
                return;
            }
        }
        if pressed && b == Button::Middle {
            self.middle_press_pos = Some(Point::new(ev.x, ev.y));
            self.middle_dragged = false;
        }
        // On Linux (or in `GLYPH_WALL_CLIPBOARD=memory` tests), an un-dragged middle-click on a
        // terminal body pastes the OS `PRIMARY` selection when non-empty (unless a focused DECSET
        // mouse-reporting program claimed the middle press), falling back to the wall's internal
        // `selection_clipboard` when `PRIMARY` is empty.
        if !pressed && b == Button::Middle && !self.middle_dragged {
            self.middle_press_pos = None;
            let primary_target = self.wall.as_mut().and_then(|wall| {
                if !matches!(
                    wall.pointer().active_gesture(),
                    Some(GestureKind::Pan | GestureKind::FsPaste)
                ) {
                    return None;
                }
                let described = wall.frame();
                middle_paste_target_at(&described, Point::new(ev.x, ev.y))
            });
            if let Some(tid) = primary_target {
                if let Some(primary_text) = self.read_primary_selection() {
                    if let Some(wall) = &mut self.wall {
                        wall.pointer_cancel(&ev);
                        wall.deliver_clipboard(&tid, &primary_text);
                    }
                    self.apply_effects();
                    self.redraw();
                    return;
                }
            }
        }
        let mut new_primary: Option<String> = None;
        {
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
                    if wall.config().copy_on_select != CopyOnSelect::False {
                        let sel_clip = wall.model().selection_clipboard();
                        if !sel_clip.is_empty() && sel_clip != self.last_synced_selection_clip {
                            self.last_synced_selection_clip = sel_clip.to_owned();
                            new_primary = Some(sel_clip.to_owned());
                        }
                    }
                    let here = Point::new(ev.x, ev.y);
                    let click_repeat_ms = wall.config().click_repeat_ms;
                    let double = self.last_release.is_some_and(|(t, p)| ev.time_ms - t <= click_repeat_ms && (p.x - here.x).abs() <= DOUBLE_CLICK_PX && (p.y - here.y).abs() <= DOUBLE_CLICK_PX);
                    if double {
                        wall.pointer_double_click(&ev);
                        self.last_release = None;
                    } else {
                        self.last_release = Some((ev.time_ms, here));
                    }
                }
            }
        }
        if let Some(primary_text) = new_primary {
            self.write_primary_selection(primary_text);
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
            && !matches!(event, WindowEvent::RedrawRequested)
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
            WindowEvent::Moved(_) => {
                if let Some(g) = &self.gfx {
                    self.win.on_moved(&g.window);
                }
            }
            WindowEvent::Focused(focused) => self.focused(focused),
            WindowEvent::Occluded(occluded) => {
                self.occluded = occluded;
                if occluded {
                    self.wakes.redraw_armed.store(false, std::sync::atomic::Ordering::SeqCst);
                } else {
                    self.redraw();
                }
            }
            WindowEvent::Resized(size) => {
                self.resize(size.width, size.height);
                #[cfg(target_os = "macos")]
                if !self.win.is_minimized_or_zero() {
                    self.draw(true);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.win.set_scale_factor(scale_factor);
                if let Some(g) = &self.gfx {
                    let size = g.window.inner_size();
                    self.resize(size.width, size.height);
                    #[cfg(target_os = "macos")]
                    if !self.win.is_minimized_or_zero() {
                        self.draw(true);
                    }
                }
            }
            WindowEvent::RedrawRequested => self.draw(true),
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
                let mods = self.mods();
                if let (Some(wall), Some(terms)) = (&mut self.wall, &self.terms) {
                    let lookup = screen_lookup(|id| terms.screen(id));
                    wall.modifiers_changed(mods, &lookup);
                }
                self.apply_effects();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let sf = self.scale_factor();
                let (x, y) = (position.x / sf, position.y / sf);
                if ui_pointer {
                    return;
                }
                self.move_cursor(x, y);
            }
            WindowEvent::CursorLeft { .. } => self.cursor_left(),
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
            WindowEvent::MouseWheel { delta, phase, .. } if !ui_pointer => self.wheel(delta, phase),
            WindowEvent::PinchGesture { delta, phase, .. } if !ui_pointer => self.pinch(delta, phase),
            WindowEvent::DoubleTapGesture { .. } if !ui_pointer => self.smart_zoom(),
            // Terminals on the wall sit strictly axis-aligned; two-finger rotation is a no-op.
            WindowEvent::RotationGesture { .. } => {}
            WindowEvent::HoveredFile(path) if !ui_pointer => {
                self.hovered_files.push(path);
                self.redraw();
            }
            WindowEvent::HoveredFileCancelled => {
                if !self.hovered_files.is_empty() {
                    self.hovered_files.clear();
                    self.redraw();
                }
            }
            WindowEvent::DroppedFile(path) if !ui_pointer => {
                self.hovered_files.clear();
                self.pending_dropped_files.push(path);
                self.redraw();
            }
            WindowEvent::Ime(ime) if !ui_keys => self.ime(&ime),
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => {
                if self.win.handle_os_fullscreen_key(&event, self.modifiers, self.gfx.as_ref().map(|g| g.window.as_ref())) {
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
        if let Some(g) = &self.gfx {
            self.win.record_window_state(&g.window);
        }
        self.win.flush_save_now();
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
        self.flush_dropped_files();
        self.win.flush_save_if_due();
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
        if self.win.is_minimized_or_zero() && self.exit_after.is_none() && !self.perf.is_enabled() {
            self.blink_repaint_at = None;
            event_loop.set_control_flow(ControlFlow::Wait);
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
        persistent_ns: None,
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
        render_ids: RenderIds::default(),
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
        scrollback_pills: Vec::new(),
        hovered_scrollback_pill: None,
        pressed_scrollback_pill: None,
        scrollback_snapshot: HashMap::new(),
        win: super::window::WindowLifecycle::new(),
        termd_tick_at: None,
        pending_termd: None,
        starting_termd: false,
        occluded: false,
        wakes: Arc::new(WakeGate::default()),
        perf: super::perf::WallPerf::from_env(),
        ime_composing: false,
        script_ime_composing: false,
        last_ime_area: None,
        #[cfg(target_os = "macos")]
        macos_option_as_alt: None,
        clock_offset_ms: 0.0,
        last_copied: None,
        memory_clipboard: None,
        memory_primary: None,
        last_synced_selection_clip: String::new(),
        middle_press_pos: None,
        middle_dragged: false,
        hovered_files: Vec::new(),
        pending_dropped_files: Vec::new(),
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

#[cfg(test)]
mod render_id_tests {
    use super::*;
    use crate::wall::renderer::{WallFrame, WallStyle};

    #[test]
    fn render_ids_free_closed_and_reconnected_terminals_in_renderer() {
        let ctx = crate::gpu::test_device::context();
        let mut renderer = WallRenderer::new(
            &ctx.device,
            &ctx.queue,
            &crate::default_emoji_sheet(),
            wgpu::TextureFormat::Rgba8UnormSrgb,
            WallStyle::default(),
        );
        let mut ids = RenderIds::default();
        let cam = WallCamera::new(Vec2::ZERO, 20.0);
        let vp = (320, 200);
        let screen = Screen::new(40, 12);

        let draw = |rid: u64, x: f32| TerminalDraw {
            id: rid,
            screen: &screen,
            position: Vec2::new(x, 0.0),
            z: 1.0,
            scale: 1.0,
            focused: false,
            panel_opacity: Some(1.0),
            border_color: None,
            selection: None,
            hover_link: None,
        };

        assert_eq!(renderer.cached_terminal_count(), 0);

        // Two live terminals populate the GPU cache.
        let r1 = ids.id_for("t1");
        let r2 = ids.id_for("t2");
        assert_ne!(r1, r2);
        let _ = renderer.prepare(
            &ctx.device,
            &ctx.queue,
            vp,
            &cam,
            &WallFrame { terminals: &[draw(r1, 0.0), draw(r2, 50.0)], ..Default::default() },
        );
        assert_eq!(renderer.cached_terminal_count(), 2);

        // Closing t1 (News::Ended) frees its GPU buffer and returns count to 1.
        assert!(ids.remove_from("t1", Some(&mut renderer)));
        assert_eq!(renderer.cached_terminal_count(), 1);
        assert!(!ids.remove_from("t1", Some(&mut renderer)));

        // Spawning t3 after t1 closed gets a fresh monotonic id (never collides with t2).
        let r3 = ids.id_for("t3");
        assert_ne!(r3, r2);
        assert_ne!(r3, r1);
        let _ = renderer.prepare(
            &ctx.device,
            &ctx.queue,
            vp,
            &cam,
            &WallFrame { terminals: &[draw(r2, 0.0), draw(r3, 50.0)], ..Default::default() },
        );
        assert_eq!(renderer.cached_terminal_count(), 2);

        // Reconnect pruning retains only terminals still alive in termd.
        ids.retain_live(&["t3".to_string()], Some(&mut renderer));
        assert_eq!(renderer.cached_terminal_count(), 1);
        assert_eq!(ids.id_for("t3"), r3);

        // Reconnect with zero live terminals returns cached count all the way to baseline 0.
        ids.retain_live(&[], Some(&mut renderer));
        assert_eq!(renderer.cached_terminal_count(), 0);
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

#[cfg(test)]
mod pointer_fidelity_tests {
    use super::*;

    fn drain_scroll_lines(wall: &mut Wall) -> i32 {
        wall.drain_effects()
            .into_iter()
            .filter_map(|e| match e {
                WallEffect::ScrollTerminal { lines, .. } => Some(lines),
                _ => None,
            })
            .sum()
    }

    #[test]
    fn trackpad_pixel_delta_pans_one_to_one_at_every_zoom_and_respects_natural_sign() {
        for &zoom in &[0.45_f64, 1.0, 3.75, 20.0] {
            let mut wall = Wall::in_memory(RulesOptions {
                viewport: Size::new(1600.0, 1000.0),
                scale_factor: 2.0,
                ..RulesOptions::default()
            });
            let cur_z = wall.camera().zoom();
            wall.pinch(800.0, 500.0, zoom / cur_z);
            assert!((wall.camera().zoom() - zoom).abs() < 1e-6);
            let anchor_world = Point::new(12.0, -8.0);
            let before_px = wall.camera().to_screen(anchor_world);

            // Recorded macOS two-finger swipe right (+42 pt) & down (+18 pt) on a 2x Retina
            // display: winit delivers `PhysicalPosition(84.0, 36.0)` across Started -> Moved -> Ended.
            let recorded = [
                (TouchPhase::Started, 0.0, 0.0, 0.0),
                (TouchPhase::Moved, 50.0, 20.0, 8.0),
                (TouchPhase::Moved, 34.0, 16.0, 16.0),
                (TouchPhase::Ended, 0.0, 0.0, 24.0),
            ];
            for (phase, px_x, px_y, t_ms) in recorded {
                if let Some((dx, dy, mode)) = convert_scroll_delta(
                    MouseScrollDelta::PixelDelta(PhysicalPosition::new(px_x, px_y)),
                    phase,
                    2.0,
                    Mods::default(),
                    "ctrl alt meta",
                ) {
                    wall.wheel(&WheelEvent {
                        x: 800.0,
                        y: 500.0,
                        delta_x: dx,
                        delta_y: dy,
                        delta_mode: mode,
                        mods: Mods::default(),
                        time_ms: t_ms,
                    });
                }
            }

            let after_px = wall.camera().to_screen(anchor_world);
            assert!(
                (after_px.x - (before_px.x + 42.0)).abs() < 1e-6
                    && (after_px.y - (before_px.y + 18.0)).abs() < 1e-6,
                "zoom={zoom}: world content moved ({:.4}, {:.4}) logical px instead of (+42, +18)",
                after_px.x - before_px.x,
                after_px.y - before_px.y,
            );
        }
    }

    #[test]
    fn canvas_pan_flick_with_macos_momentum_never_hijacks_sliding_terminal_until_pointer_moves() {
        let mut wall = Wall::in_memory(RulesOptions {
            viewport: Size::new(1600.0, 1000.0),
            scale_factor: 2.0,
            ..RulesOptions::default()
        });
        wall.terminal_appeared(TerminalAppeared::new("t1", 80, 24));
        wall.run("wall.fit");
        for i in 1..=60 {
            wall.tick(1.0 / 60.0, f64::from(i) * 16.0);
        }
        wall.pinch(800.0, 500.0, 0.5);
        let _ = wall.drain_effects();

        let described = wall.frame();
        let t1_box = described.camera.to_screen_box(
            described.terminals[0].body_rect,
            described.viewport.w,
            described.viewport.h,
        );
        // Place stationary cursor 80 px to the left of focused `t1` (empty paper), then flick
        // left (`scrollingDeltaX < 0`) so `t1` slides directly underneath `(cursor_x, cursor_y)`
        // right as macOS switches from finger lift (`Ended`) to `momentumPhase::Began` (`Started`).
        let cursor_x = t1_box.left - 80.0;
        let cursor_y = 0.5 * (t1_box.top + t1_box.bottom);
        assert!(cursor_x > 20.0);

        let flick_and_momentum = [
            (TouchPhase::Started, 0.0, 0.0, 1000.0),
            (TouchPhase::Moved, -60.0, -10.0, 1008.0),
            (TouchPhase::Moved, -60.0, -10.0, 1016.0),
            (TouchPhase::Ended, 0.0, 0.0, 1024.0),       // fingers lift
            (TouchPhase::Started, -50.0, -24.0, 1032.0), // macOS momentumPhase == Began!
            (TouchPhase::Moved, -40.0, -30.0, 1040.0),   // t1 is now squarely under cursor!
            (TouchPhase::Moved, -20.0, -30.0, 1048.0),
            (TouchPhase::Ended, 0.0, 0.0, 1056.0),       // momentum ends
        ];
        for (phase, pt_x, pt_y, t_ms) in flick_and_momentum {
            if let Some((dx, dy, mode)) = convert_scroll_delta(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(pt_x * 2.0, pt_y * 2.0)),
                phase,
                2.0,
                Mods::default(),
                "ctrl alt meta",
            ) {
                wall.wheel(&WheelEvent {
                    x: cursor_x,
                    y: cursor_y,
                    delta_x: dx,
                    delta_y: dy,
                    delta_mode: mode,
                    mods: Mods::default(),
                    time_ms: t_ms,
                });
            }
        }
        // Confirm `t1` really slid under `(cursor_x, cursor_y)` yet received 0 scroll lines.
        let after = wall.frame();
        let slid_box = after.camera.to_screen_box(
            after.terminals[0].body_rect,
            after.viewport.w,
            after.viewport.h,
        );
        assert!(
            cursor_x >= slid_box.left && cursor_x <= slid_box.right,
            "expected t1 ({:.1}..{:.1}) to slide under cursor_x={cursor_x:.1}",
            slid_box.left,
            slid_box.right
        );
        assert_eq!(drain_scroll_lines(&mut wall), 0);

        // Moving the pointer > LATCH_SLOP_PX (24 px) inside `t1` breaks the canvas latch so the
        // user can immediately scroll `t1` without waiting 140 ms; then moving the pointer > 24 px
        // off `t1` mid-momentum stops scrolling `t1`.
        let inside_x = cursor_x + 40.0;
        let (dx, dy, mode) = convert_scroll_delta(
            MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 120.0)),
            TouchPhase::Moved,
            2.0,
            Mods::default(),
            "ctrl alt meta",
        )
        .unwrap();
        wall.wheel(&WheelEvent {
            x: inside_x,
            y: cursor_y,
            delta_x: dx,
            delta_y: dy,
            delta_mode: mode,
            mods: Mods::default(),
            time_ms: 1064.0,
        });
        assert!(
            drain_scroll_lines(&mut wall) > 0,
            "moving >24px onto focused t1 should scroll t1"
        );

        let outside_x = slid_box.left - 40.0;
        wall.wheel(&WheelEvent {
            x: outside_x,
            y: cursor_y,
            delta_x: dx,
            delta_y: dy,
            delta_mode: mode,
            mods: Mods::default(),
            time_ms: 1072.0,
        });
        assert_eq!(
            drain_scroll_lines(&mut wall),
            0,
            "momentum must stop scrolling t1 once pointer moves >24px off it"
        );
    }

    #[test]
    fn mouse_wheel_line_delta_scrolls_three_lines_per_notch_and_zooms_by_step_factor() {
        let mut wall = Wall::in_memory(RulesOptions {
            viewport: Size::new(1600.0, 1000.0),
            scale_factor: 1.0,
            ..RulesOptions::default()
        });
        wall.terminal_appeared(TerminalAppeared::new("t1", 80, 24));
        wall.run("wall.fit");
        for i in 1..=60 {
            wall.tick(1.0 / 60.0, f64::from(i) * 16.0);
        }
        wall.pinch(800.0, 500.0, 0.5);
        let _ = wall.drain_effects();
        let described = wall.frame();
        let sb = described.camera.to_screen_box(
            described.terminals[0].body_rect,
            described.viewport.w,
            described.viewport.h,
        );
        let (cx, cy) = (0.5 * (sb.left + sb.right), 0.5 * (sb.top + sb.bottom));

        // One upward wheel detent (`LineDelta(0, 1)`): 1 immediate lead line + 2 glided = 3 lines.
        let (dx, dy, mode) = convert_scroll_delta(
            MouseScrollDelta::LineDelta(0.0, 1.0),
            TouchPhase::Moved,
            1.0,
            Mods::default(),
            "ctrl alt meta",
        )
        .unwrap();
        assert_eq!(mode, DeltaMode::Line);
        assert_eq!(dy, -3.0);
        wall.wheel(&WheelEvent {
            x: cx,
            y: cy,
            delta_x: dx,
            delta_y: dy,
            delta_mode: mode,
            mods: Mods::default(),
            time_ms: 2000.0,
        });
        let mut total_lines = drain_scroll_lines(&mut wall);
        for i in 1..=30 {
            wall.tick(1.0 / 60.0, 2000.0 + f64::from(i) * 16.0);
            total_lines += drain_scroll_lines(&mut wall);
        }
        assert_eq!(total_lines, 3);

        // `Ctrl + LineDelta(0, 1)` glides camera zoom by exact `WHEEL_ZOOM.step_factor` (`1.12x`).
        let z0 = wall.camera().zoom();
        let anchor_before = wall.camera().to_world(Point::new(420.0, 310.0));
        let ctrl = Mods {
            ctrl: true,
            ..Mods::default()
        };
        let (zdx, zdy, zmode) = convert_scroll_delta(
            MouseScrollDelta::LineDelta(0.0, 1.0),
            TouchPhase::Moved,
            1.0,
            ctrl,
            "ctrl alt meta",
        )
        .unwrap();
        assert_eq!(zmode, DeltaMode::Pixel);
        assert_eq!(zdy, -100.0);
        wall.wheel(&WheelEvent {
            x: 420.0,
            y: 310.0,
            delta_x: zdx,
            delta_y: zdy,
            delta_mode: zmode,
            mods: ctrl,
            time_ms: 3000.0,
        });
        for i in 1..=60 {
            wall.tick(1.0 / 60.0, 3000.0 + f64::from(i) * 16.0);
        }
        let z1 = wall.camera().zoom();
        let anchor_after = wall.camera().to_world(Point::new(420.0, 310.0));
        assert!(
            ((z1 / z0) - 1.12).abs() < 1e-4,
            "expected 1.12x zoom step per mouse notch, got {:.6}",
            z1 / z0
        );
        assert!(
            (anchor_after.x - anchor_before.x).hypot(anchor_after.y - anchor_before.y) < 1e-4
        );
    }

    #[test]
    fn pinch_uses_exponential_browser_rate_keeps_anchor_and_stays_positive_on_large_outward_deltas() {
        assert_eq!(pinch_zoom_factor(0.0, TouchPhase::Started), None);
        assert_eq!(pinch_zoom_factor(0.2, TouchPhase::Cancelled), None);

        // Symmetric round-trip (`+0.35` then `-0.35`) returns to exact 1.0x, unlike `1 + delta`.
        let f_in = pinch_zoom_factor(0.35, TouchPhase::Moved).unwrap();
        let f_out = pinch_zoom_factor(-0.35, TouchPhase::Moved).unwrap();
        assert!((f_in * f_out - 1.0).abs() < 1e-12);
        assert!(pinch_zoom_factor(-1.5, TouchPhase::Moved).unwrap() > 0.0);

        let mut wall = Wall::in_memory(RulesOptions {
            viewport: Size::new(1600.0, 1000.0),
            scale_factor: 2.0,
            ..RulesOptions::default()
        });
        let cur_z = wall.camera().zoom();
        wall.pinch(800.0, 500.0, 4.0 / cur_z);
        assert!((wall.camera().zoom() - 4.0).abs() < 1e-6);
        let pt = Point::new(510.0, 290.0);
        let world_before = wall.camera().to_world(pt);
        for d in [0.08, 0.12, 0.05] {
            wall.pinch(pt.x, pt.y, pinch_zoom_factor(d, TouchPhase::Moved).unwrap());
        }
        let world_after = wall.camera().to_world(pt);
        assert!((wall.camera().zoom() - 4.0 * (0.25_f64).exp()).abs() < 1e-6);
        assert!((world_after.x - world_before.x).hypot(world_after.y - world_before.y) < 1e-6);
    }

    #[test]
    fn shell_quote_path_and_dropped_file_formatting_escape_unsafe_shell_metacharacters() {
        use std::path::Path;

        assert_eq!(
            shell_quote_path(Path::new("/tmp/plain_file-1.2+ok@host:dir/a=b,c%d")),
            "/tmp/plain_file-1.2+ok@host:dir/a=b,c%d"
        );
        assert_eq!(
            shell_quote_path(Path::new("/tmp/my file's (1) $HOME & *.txt")),
            r"'/tmp/my file'\''s (1) $HOME & *.txt'"
        );
        assert_eq!(shell_quote_path(Path::new("")), "''");
        assert_eq!(
            shell_quote_path(Path::new("/tmp/日本語.rs")),
            "'/tmp/日本語.rs'"
        );

        let batch = vec![
            PathBuf::from("/tmp/clean.rs"),
            PathBuf::from("/tmp/two words'here.txt"),
        ];
        assert_eq!(
            format_dropped_paths(&batch),
            "/tmp/clean.rs '/tmp/two words'\\''here.txt' "
        );
        assert_eq!(
            hovered_drop_notice(&batch[..1]).as_deref(),
            Some("drop to paste path: clean.rs")
        );
        assert_eq!(
            hovered_drop_notice(&batch).as_deref(),
            Some("drop to paste 2 paths")
        );
    }

    #[test]
    fn middle_paste_target_hits_interior_body_and_excludes_resize_perimeter_and_empty_paper() {
        let mut wall = Wall::in_memory(RulesOptions {
            viewport: Size::new(1600.0, 1000.0),
            scale_factor: 1.0,
            ..RulesOptions::default()
        });
        wall.terminal_appeared(TerminalAppeared::new("t1", 80, 24));
        wall.run("wall.fit");
        for i in 1..=60 {
            wall.tick(1.0 / 60.0, f64::from(i) * 16.0);
        }
        wall.pinch(800.0, 500.0, 0.5);
        let described = wall.frame();
        let sb = described.camera.to_screen_box(
            described.terminals[0].body_rect,
            described.viewport.w,
            described.viewport.h,
        );
        let center = Point::new(0.5 * (sb.left + sb.right), 0.5 * (sb.top + sb.bottom));
        assert_eq!(
            middle_paste_target_at(&described, center).as_deref(),
            Some("t1")
        );
        // Right on the outer resize edge (`sb.left + 2.0`) is `HitPart::Edge`, not `Body`.
        assert_eq!(
            middle_paste_target_at(&described, Point::new(sb.left + 2.0, center.y)),
            None
        );
        assert_eq!(
            middle_paste_target_at(&described, Point::new(20.0, 20.0)),
            None
        );
    }
}
