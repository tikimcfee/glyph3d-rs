//! Window lifecycle for the native wall (`native/src/wall/app.rs`).
//!
//! Keeps OS window citizenship out of `app.rs` so the main event bridge stays
//! compact and pure lifecycle logic can be unit-tested without opening a real
//! display server window:
//!
//! 1. **Scale factor changes (`ScaleFactorChanged`)** — tracks the live backing
//!    scale factor and propagates it to every physical-pixel consumer
//!    (`Wall::set_viewport`, `WallStyle`'s physical-px border/cursor/greeking
//!    thresholds, surface/depth configuration, chrome px, and `egui`).
//! 2. **Live resize & zero/minimise safety** — skips `0×0` and minimised
//!    surface reconfigurations so `wgpu` never panics and `about_to_wait`
//!    sleeps instead of spinning, while redrawing synchronously inside macOS's
//!    modal edge-resize callback so frames never freeze or stretch.
//! 3. **Window persistence (`window-<ns>.json`)** — remembers normal windowed
//!    logical size, desktop position, OS fullscreen/zoom state, and monitor
//!    beside `wall-<ns>.json`, falling back cleanly when a monitor is unplugged
//!    or rearranged.
//! 4. **Native OS fullscreen & zoom vs `wall.fullscreen`** — handles `⌃⌘F` on
//!    macOS and `F11` elsewhere for OS window fullscreen (`Fullscreen::Borderless`),
//!    keeping it distinct from `wall.fullscreen` (`⌘↵` / `Ctrl+Alt+↵`) and
//!    `wall.fullscreen.pane` (`⌘⇧↵` / `Ctrl+Alt+⇧+↵`).
//! 5. **Window focus (`Focused(bool)`)** — draws the focused terminal's cursor
//!    hollow and pauses cursor-blink wakeups while the OS window is not key,
//!    and releases stuck modifiers, active drags, and close-holds on blur.
//! 6. **Cursor icons (`WallEffect::SetCursor`)** — maps all 13 `wall_core`
//!    cursor variants to `winit::window::CursorIcon` and restores the wall's
//!    cursor whenever an open `egui` panel yields the pointer back to the canvas.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use wall_core::geom::Point;
use wall_core::input::{KeyEvent, Mods, Platform};
use wall_core::layout::ResizeCursor;
use wall_core::pointer::{Buttons, CursorIcon, PointerEvent};
use wall_core::wall::{Wall, screen_lookup};
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, KeyEvent as WinitKeyEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::monitor::MonitorHandle;
use winit::window::{CursorIcon as WinitCursorIcon, Fullscreen, Window, WindowAttributes};

use super::renderer::WallStyle;

/// Default window inner width in logical points on first open.
pub const DEFAULT_LOGICAL_WIDTH: f64 = 1600.0;
/// Default window inner height in logical points on first open.
pub const DEFAULT_LOGICAL_HEIGHT: f64 = 1000.0;
/// Minimum restored logical width so a corrupt or tiny save never opens unusable.
pub const MIN_LOGICAL_WIDTH: f64 = 400.0;
/// Minimum restored logical height.
pub const MIN_LOGICAL_HEIGHT: f64 = 300.0;
/// Fallback upper bound when the display server reports no monitor list (e.g. early Wayland).
const MAX_FALLBACK_LOGICAL_DIM: f64 = 8192.0;
/// Minimum horizontal strip of the window (physical px) kept inside the target monitor.
const MIN_VISIBLE_X_PX: i32 = 120;
/// Minimum vertical room from the monitor's bottom edge (physical px) so the title bar stays grab-able.
const MIN_VISIBLE_TOP_PX: i32 = 80;
/// Delay after the last move/resize before flushing `window-<ns>.json` during idle wait.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(200);

/// Hover border colour for unfocused terminals (`#5a6b86`), matching `app.rs`.
const HOVER_BORDER_RGB: (u8, u8, u8) = (0x5a, 0x6b, 0x86);
/// Quiet focus-green border (`#2f7a52`, matching the focused title bar's border)
/// shown on the wall's focused terminal while the OS window itself is unfocused.
const UNFOCUSED_WINDOW_ACTIVE_BORDER_RGB: (u8, u8, u8) = (0x2f, 0x7a, 0x52);

/// Plain-data snapshot of a display monitor (`winit::monitor::MonitorHandle`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub name: Option<String>,
    /// Monitor top-left in desktop physical pixels.
    pub position: [i32; 2],
    /// Monitor dimensions in physical pixels.
    pub size: [u32; 2],
    pub scale_factor: f64,
}

impl MonitorInfo {
    /// Convert a live winit [`MonitorHandle`] into plain serializable data.
    pub fn from_handle(m: &MonitorHandle) -> Self {
        let pos = m.position();
        let size = m.size();
        Self {
            name: m.name(),
            position: [pos.x, pos.y],
            size: [size.width, size.height],
            scale_factor: sanitize_scale_factor(m.scale_factor()),
        }
    }

    /// Monitor width and height in logical points.
    pub fn logical_size(&self) -> (f64, f64) {
        let sf = sanitize_scale_factor(self.scale_factor);
        (f64::from(self.size[0]) / sf, f64::from(self.size[1]) / sf)
    }

    /// True if physical desktop point `(x, y)` lies inside this monitor's bounds.
    pub fn contains_physical(&self, x: i32, y: i32) -> bool {
        let max_x = self.position[0].saturating_add(self.size[0].min(i32::MAX as u32) as i32);
        let max_y = self.position[1].saturating_add(self.size[1].min(i32::MAX as u32) as i32);
        x >= self.position[0] && x < max_x && y >= self.position[1] && y < max_y
    }
}

/// Persisted window geometry (`window-<ns>.json` beside `wall-<ns>.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedWindow {
    /// Normal (non-fullscreen, non-maximized) inner width in logical points.
    pub width: f64,
    /// Normal (non-fullscreen, non-maximized) inner height in logical points.
    pub height: f64,
    /// Normal outer top-left position in desktop physical pixels (`None` on Wayland).
    pub position: Option<[i32; 2]>,
    /// Whether native OS window fullscreen (`Fullscreen::Borderless`) was active.
    pub fullscreen: bool,
    /// Whether OS window zoom / maximize was active.
    #[serde(default)]
    pub maximized: bool,
    /// Monitor the window occupied when last recorded.
    pub monitor: Option<MonitorInfo>,
}

impl Default for SavedWindow {
    fn default() -> Self {
        Self {
            width: DEFAULT_LOGICAL_WIDTH,
            height: DEFAULT_LOGICAL_HEIGHT,
            position: None,
            fullscreen: false,
            maximized: false,
            monitor: None,
        }
    }
}

/// Resolved startup geometry after matching [`SavedWindow`] against connected monitors.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowPlacement {
    pub width: f64,
    pub height: f64,
    pub position: Option<[i32; 2]>,
    pub fullscreen: bool,
    pub maximized: bool,
    /// True when the saved monitor was found among currently connected monitors.
    pub matched_saved_monitor: bool,
}

/// Non-zero physical and logical dimensions produced by [`sync_viewport_and_style`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewportSync {
    pub physical_width: u32,
    pub physical_height: u32,
    pub logical_width: f64,
    pub logical_height: f64,
    pub scale_factor: f64,
}

/// One row of the complete `wall_core` → `winit` cursor mapping table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorMapping {
    pub wall: CursorIcon,
    pub css: &'static str,
    pub winit: WinitCursorIcon,
}

/// Exhaustive table of every [`CursorIcon`] `wall-core` can emit and the
/// [`winit::window::CursorIcon`] the native window sets for it.
pub const CURSOR_TABLE: [CursorMapping; 13] = [
    CursorMapping { wall: CursorIcon::Default, css: "default", winit: WinitCursorIcon::Default },
    CursorMapping { wall: CursorIcon::Text, css: "text", winit: WinitCursorIcon::Text },
    CursorMapping { wall: CursorIcon::Pointer, css: "pointer", winit: WinitCursorIcon::Pointer },
    CursorMapping { wall: CursorIcon::Move, css: "move", winit: WinitCursorIcon::Move },
    CursorMapping { wall: CursorIcon::Grab, css: "grab", winit: WinitCursorIcon::Grab },
    CursorMapping {
        wall: CursorIcon::Crosshair,
        css: "crosshair",
        winit: WinitCursorIcon::Crosshair,
    },
    CursorMapping { wall: CursorIcon::Copy, css: "copy", winit: WinitCursorIcon::Copy },
    CursorMapping {
        wall: CursorIcon::ColResize,
        css: "col-resize",
        winit: WinitCursorIcon::ColResize,
    },
    CursorMapping {
        wall: CursorIcon::RowResize,
        css: "row-resize",
        winit: WinitCursorIcon::RowResize,
    },
    CursorMapping {
        wall: CursorIcon::Resize(ResizeCursor::NwseResize),
        css: "nwse-resize",
        winit: WinitCursorIcon::NwseResize,
    },
    CursorMapping {
        wall: CursorIcon::Resize(ResizeCursor::NeswResize),
        css: "nesw-resize",
        winit: WinitCursorIcon::NeswResize,
    },
    CursorMapping {
        wall: CursorIcon::Resize(ResizeCursor::EwResize),
        css: "ew-resize",
        winit: WinitCursorIcon::EwResize,
    },
    CursorMapping {
        wall: CursorIcon::Resize(ResizeCursor::NsResize),
        css: "ns-resize",
        winit: WinitCursorIcon::NsResize,
    },
];

/// Translate a `wall_core` [`CursorIcon`] to its `winit` counterpart.
pub const fn cursor_of(c: CursorIcon) -> WinitCursorIcon {
    match c {
        CursorIcon::Default => WinitCursorIcon::Default,
        CursorIcon::Text => WinitCursorIcon::Text,
        CursorIcon::Pointer => WinitCursorIcon::Pointer,
        CursorIcon::Move => WinitCursorIcon::Move,
        CursorIcon::Grab => WinitCursorIcon::Grab,
        CursorIcon::Crosshair => WinitCursorIcon::Crosshair,
        CursorIcon::Copy => WinitCursorIcon::Copy,
        CursorIcon::ColResize => WinitCursorIcon::ColResize,
        CursorIcon::RowResize => WinitCursorIcon::RowResize,
        CursorIcon::Resize(ResizeCursor::NwseResize) => WinitCursorIcon::NwseResize,
        CursorIcon::Resize(ResizeCursor::NeswResize) => WinitCursorIcon::NeswResize,
        CursorIcon::Resize(ResizeCursor::EwResize) => WinitCursorIcon::EwResize,
        CursorIcon::Resize(ResizeCursor::NsResize) => WinitCursorIcon::NsResize,
    }
}

/// Sanitize a window or monitor backing scale factor so `0`, negative, or `NaN`
/// values never poison coordinate conversions.
pub fn sanitize_scale_factor(scale_factor: f64) -> f64 {
    if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    }
}

/// Scale every physical-pixel field in [`WallStyle`] from logical-px `base`
/// values by `scale_factor` (`border_width_px`, `cursor_border_px`, `greek_px`,
/// `show_glyphs_px`), leaving live-tuned world-space fields (`panel_padding`,
/// `line_thickness_scale`, `underline_raise`, `curly_amplitude`, `dim_alpha`)
/// untouched.
pub fn apply_scale_to_wall_style(style: &mut WallStyle, base: &WallStyle, scale_factor: f32) {
    let sf = sanitize_scale_factor(f64::from(scale_factor)) as f32;
    style.border_width_px = base.border_width_px * sf;
    style.cursor_border_px = base.cursor_border_px * sf;
    style.greek_px = base.greek_px * sf;
    style.show_glyphs_px = base.show_glyphs_px * sf;
}

/// Shared scale-factor + viewport update path called on window open, resize,
/// and `ScaleFactorChanged`. Returns `None` when `physical_width == 0` or
/// `physical_height == 0` (minimised / collapsed window) so callers skip `0×0`
/// surface creation.
pub fn sync_viewport_and_style(
    physical_width: u32,
    physical_height: u32,
    scale_factor: f64,
    base_style: &WallStyle,
    renderer_style: &mut WallStyle,
    wall: Option<&mut Wall>,
) -> Option<ViewportSync> {
    if physical_width == 0 || physical_height == 0 {
        return None;
    }
    let sf = sanitize_scale_factor(scale_factor);
    apply_scale_to_wall_style(renderer_style, base_style, sf as f32);
    let logical_width = f64::from(physical_width) / sf;
    let logical_height = f64::from(physical_height) / sf;
    if let Some(w) = wall {
        w.set_viewport(logical_width, logical_height, sf);
    }
    Some(ViewportSync {
        physical_width,
        physical_height,
        logical_width,
        logical_height,
        scale_factor: sf,
    })
}

/// Path to `window-<ns>.json` inside `store_dir`, or `None` when the wall runs
/// without a namespace (`--wall-pattern`) or with an invalid path-traversal name.
pub fn window_store_path(store_dir: &Path, namespace: Option<&str>) -> Option<PathBuf> {
    let ns = namespace?.trim();
    if ns.is_empty()
        || ns == "."
        || ns == ".."
        || ns.contains('/')
        || ns.contains('\\')
        || ns.contains('\0')
    {
        return None;
    }
    Some(store_dir.join(format!("window-{ns}.json")))
}

/// Read `SavedWindow` from `path`, returning `None` if absent or unreadable.
pub fn load_saved_window(path: &Path) -> Option<SavedWindow> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            log::warn!("could not read saved window state {}: {e}", path.display());
            return None;
        }
    };
    match serde_json::from_str::<SavedWindow>(&raw) {
        Ok(saved) if saved.width.is_finite() && saved.height.is_finite() => Some(saved),
        Ok(_) => {
            log::warn!("ignoring non-finite dimensions in {}", path.display());
            None
        }
        Err(e) => {
            log::warn!("ignoring malformed saved window state {}: {e}", path.display());
            None
        }
    }
}

/// Atomically write `SavedWindow` to `path` (`0700` parent directory and `0600`
/// file permissions on Unix, matching `WallStore`).
pub fn save_window_atomic(path: &Path, saved: &SavedWindow) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(saved).map_err(std::io::Error::other)?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        create_private_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("window.json");
    let tmp = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
    write_private_file(&tmp, json.as_bytes())?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(bytes)?;
    file.flush()
}

fn clamp_size_to_monitor(width: f64, height: f64, monitor: Option<&MonitorInfo>) -> (f64, f64) {
    let (max_w, max_h) = monitor
        .map(MonitorInfo::logical_size)
        .filter(|(w, h)| *w > 0.0 && *h > 0.0)
        .unwrap_or((MAX_FALLBACK_LOGICAL_DIM, MAX_FALLBACK_LOGICAL_DIM));
    let min_w = MIN_LOGICAL_WIDTH.min(max_w);
    let min_h = MIN_LOGICAL_HEIGHT.min(max_h);
    (width.clamp(min_w, max_w), height.clamp(min_h, max_h))
}

fn clamp_position_in_monitor(
    pos: [i32; 2],
    logical_w: f64,
    logical_h: f64,
    monitor: &MonitorInfo,
) -> [i32; 2] {
    let sf = sanitize_scale_factor(monitor.scale_factor);
    let win_w = (logical_w * sf).round().max(1.0) as i32;
    let _win_h = (logical_h * sf).round().max(1.0) as i32;
    let mon_x = monitor.position[0];
    let mon_y = monitor.position[1];
    let mon_w = monitor.size[0].min(i32::MAX as u32) as i32;
    let mon_h = monitor.size[1].min(i32::MAX as u32) as i32;

    let visible_x = MIN_VISIBLE_X_PX.min(win_w).min(mon_w);
    let min_x = mon_x.saturating_sub(win_w.saturating_sub(visible_x));
    let max_x = mon_x.saturating_add(mon_w.saturating_sub(visible_x));
    let min_y = mon_y;
    let max_y = mon_y.saturating_add((mon_h - MIN_VISIBLE_TOP_PX).max(0));

    [
        pos[0].clamp(min_x.min(max_x), min_x.max(max_x)),
        pos[1].clamp(min_y.min(max_y), min_y.max(max_y)),
    ]
}

fn center_on_monitor(logical_w: f64, logical_h: f64, monitor: &MonitorInfo) -> [i32; 2] {
    let sf = sanitize_scale_factor(monitor.scale_factor);
    let win_w = (logical_w * sf).round().max(1.0) as i32;
    let win_h = (logical_h * sf).round().max(1.0) as i32;
    let mon_w = monitor.size[0].min(i32::MAX as u32) as i32;
    let mon_h = monitor.size[1].min(i32::MAX as u32) as i32;
    let x = monitor.position[0].saturating_add((mon_w - win_w) / 2);
    let y = monitor.position[1].saturating_add((mon_h - win_h).max(0) / 2);
    clamp_position_in_monitor([x, y], logical_w, logical_h, monitor)
}

/// Resolve startup window placement from optional saved state and connected
/// monitors. Falls back cleanly to `primary` (or the first available monitor)
/// when the saved monitor was unplugged or the saved position is off-screen.
pub fn resolve_window_placement(
    saved: Option<&SavedWindow>,
    monitors: &[MonitorInfo],
    primary: Option<&MonitorInfo>,
) -> WindowPlacement {
    let fallback_monitor = primary.or_else(|| monitors.first());
    let Some(saved) = saved else {
        let (width, height) =
            clamp_size_to_monitor(DEFAULT_LOGICAL_WIDTH, DEFAULT_LOGICAL_HEIGHT, fallback_monitor);
        return WindowPlacement {
            width,
            height,
            position: None,
            fullscreen: false,
            maximized: false,
            matched_saved_monitor: false,
        };
    };

    // 1. Exact monitor match (same name + desktop origin, or same origin + size when unnamed).
    let exact = saved.monitor.as_ref().and_then(|sm| {
        monitors.iter().find(|m| match (&sm.name, &m.name) {
            (Some(a), Some(b)) => a == b && m.position == sm.position,
            (None, None) => m.position == sm.position && m.size == sm.size,
            _ => false,
        })
    });

    // 2. Same monitor by name, rearranged or switched resolution/DPI in display settings.
    let by_name = exact.or_else(|| {
        let target_name = saved.monitor.as_ref()?.name.as_ref()?;
        monitors
            .iter()
            .find(|m| m.name.as_ref() == Some(target_name))
    });

    // 3. Any connected monitor containing the saved window's title-bar grab region.
    let by_pos = by_name.or_else(|| {
        let [px, py] = saved.position?;
        monitors
            .iter()
            .find(|m| m.contains_physical(px.saturating_add(60), py.saturating_add(20)))
    });

    if let Some(mon) = by_pos {
        let (width, height) = clamp_size_to_monitor(saved.width, saved.height, Some(mon));
        let position = match (saved.position, saved.monitor.as_ref()) {
            (Some(pos), Some(old_mon))
                if old_mon.position != mon.position
                    || (old_mon.scale_factor - mon.scale_factor).abs() > 1e-4 =>
            {
                let old_sf = sanitize_scale_factor(old_mon.scale_factor);
                let new_sf = sanitize_scale_factor(mon.scale_factor);
                let rel_logical_x = f64::from(pos[0] - old_mon.position[0]) / old_sf;
                let rel_logical_y = f64::from(pos[1] - old_mon.position[1]) / old_sf;
                let translated = [
                    mon.position[0] + (rel_logical_x * new_sf).round() as i32,
                    mon.position[1] + (rel_logical_y * new_sf).round() as i32,
                ];
                Some(clamp_position_in_monitor(translated, width, height, mon))
            }
            (Some(pos), _) => Some(clamp_position_in_monitor(pos, width, height, mon)),
            (None, _) => Some(center_on_monitor(width, height, mon)),
        };
        return WindowPlacement {
            width,
            height,
            position,
            fullscreen: saved.fullscreen,
            maximized: saved.maximized,
            matched_saved_monitor: true,
        };
    }

    // 4. Saved monitor is gone (or Wayland gave no monitor list): fall back cleanly.
    let (width, height) = clamp_size_to_monitor(saved.width, saved.height, fallback_monitor);
    let position = fallback_monitor.map(|mon| center_on_monitor(width, height, mon));
    WindowPlacement {
        width,
        height,
        position,
        fullscreen: saved.fullscreen,
        maximized: saved.maximized,
        matched_saved_monitor: false,
    }
}

/// True when a key press is the host platform's native OS window fullscreen
/// shortcut (`⌃⌘F` on macOS, plain `F11` on Linux/Windows) — kept separate from
/// `wall.fullscreen` (`⌘↵` / `Ctrl+Alt+↵`) and `wall.fullscreen.pane` (`⌘⇧↵` / `Ctrl+Alt+⇧+↵`).
pub fn is_os_fullscreen_shortcut(
    platform: Platform,
    modifiers: ModifiersState,
    code: KeyCode,
    state: ElementState,
    repeat: bool,
) -> bool {
    if !matches!(state, ElementState::Pressed) || repeat {
        return false;
    }
    match platform {
        Platform::Mac => {
            modifiers.super_key()
                && modifiers.control_key()
                && !modifiers.alt_key()
                && !modifiers.shift_key()
                && matches!(code, KeyCode::KeyF)
        }
        Platform::Other => {
            !modifiers.super_key()
                && !modifiers.control_key()
                && !modifiers.alt_key()
                && !modifiers.shift_key()
                && matches!(code, KeyCode::F11)
        }
    }
}

/// Live window citizenship state owned by `App`.
pub struct WindowLifecycle {
    store_path: Option<PathBuf>,
    saved: SavedWindow,
    dirty_save_since: Option<Instant>,
    scale_factor: f64,
    window_focused: bool,
    minimized_or_zero: bool,
    wall_cursor: CursorIcon,
    base_style: WallStyle,
}

impl Default for WindowLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowLifecycle {
    pub fn new() -> Self {
        Self {
            store_path: None,
            saved: SavedWindow::default(),
            dirty_save_since: None,
            scale_factor: 1.0,
            window_focused: true,
            minimized_or_zero: false,
            wall_cursor: CursorIcon::Default,
            base_style: WallStyle::default(),
        }
    }

    /// Current backing scale factor (`1.0` on standard displays, `2.0` on Retina, etc.).
    pub fn scale_factor(&self) -> f64 {
        self.scale_factor
    }

    /// True when the window has `0×0` inner size or is minimised to the Dock / taskbar.
    pub fn is_minimized_or_zero(&self) -> bool {
        self.minimized_or_zero
    }

    /// True when the OS window currently holds key focus.
    pub fn is_window_focused(&self) -> bool {
        self.window_focused
    }

    /// Update the OS key-window focus flag (solid vs hollow cursor).
    pub fn set_window_focused(&mut self, focused: bool) {
        self.window_focused = focused;
    }

    /// Current cursor icon requested by `wall-core`.
    pub fn wall_cursor(&self) -> CursorIcon {
        self.wall_cursor
    }

    /// Update the cached backing scale factor from `WindowEvent::ScaleFactorChanged`.
    pub fn set_scale_factor(&mut self, scale_factor: f64) {
        self.scale_factor = sanitize_scale_factor(scale_factor);
    }

    /// Load `window-<ns>.json` (if any), match against connected monitors, and
    /// build initial [`WindowAttributes`] plus whether OS fullscreen should be
    /// entered right after window creation (preserving the normal windowed frame).
    pub fn prepare_open(
        &mut self,
        event_loop: &ActiveEventLoop,
        store_dir: &Path,
        namespace: Option<&str>,
    ) -> (WindowAttributes, bool) {
        self.store_path = window_store_path(store_dir, namespace);
        let loaded = self.store_path.as_deref().and_then(load_saved_window);
        let monitors: Vec<MonitorInfo> = event_loop
            .available_monitors()
            .map(|m| MonitorInfo::from_handle(&m))
            .collect();
        let primary = event_loop
            .primary_monitor()
            .map(|m| MonitorInfo::from_handle(&m));
        let placement = resolve_window_placement(loaded.as_ref(), &monitors, primary.as_ref());

        if let Some(prev) = loaded {
            self.saved = prev;
        }
        self.saved.width = placement.width;
        self.saved.height = placement.height;
        if placement.position.is_some() {
            self.saved.position = placement.position;
        }
        self.saved.fullscreen = placement.fullscreen;
        self.saved.maximized = placement.maximized;

        let mut attrs = Window::default_attributes()
            .with_title("glyph3d — the wall")
            .with_inner_size(LogicalSize::new(placement.width, placement.height))
            .with_maximized(placement.maximized && !placement.fullscreen);
        if let Some([x, y]) = placement.position {
            attrs = attrs.with_position(PhysicalPosition::new(x, y));
        }
        (attrs, placement.fullscreen)
    }

    /// Complete initialization once the `Window` and `WallRenderer` exist:
    /// records initial scale factor, window focus, scaled [`WallStyle`], and
    /// flushes the initial `window-<ns>.json`.
    pub fn finish_open(
        &mut self,
        window: &Window,
        base_style: WallStyle,
        renderer_style: &mut WallStyle,
    ) -> f64 {
        self.base_style = base_style;
        self.scale_factor = sanitize_scale_factor(window.scale_factor());
        self.window_focused = window.has_focus();
        let size = window.inner_size();
        self.minimized_or_zero =
            size.width == 0 || size.height == 0 || window.is_minimized() == Some(true);
        apply_scale_to_wall_style(renderer_style, &self.base_style, self.scale_factor as f32);
        self.record_window_state(window);
        self.flush_save_now();
        self.scale_factor
    }

    /// Handle a window resize (or scale-factor resize), returning `None` when
    /// `width == 0 || height == 0` or the window is minimised so `app.rs` skips
    /// `0×0` surface reconfiguration.
    pub fn on_resize(
        &mut self,
        window: &Window,
        width: u32,
        height: u32,
        renderer_style: &mut WallStyle,
        wall: Option<&mut Wall>,
    ) -> Option<ViewportSync> {
        self.scale_factor = sanitize_scale_factor(window.scale_factor());
        if width == 0 || height == 0 || window.is_minimized() == Some(true) {
            self.minimized_or_zero = true;
            return None;
        }
        self.minimized_or_zero = false;
        let sync = sync_viewport_and_style(
            width,
            height,
            self.scale_factor,
            &self.base_style,
            renderer_style,
            wall,
        );
        self.record_window_state(window);
        sync
    }

    /// Record a window move (`WindowEvent::Moved`).
    pub fn on_moved(&mut self, window: &Window) {
        self.scale_factor = sanitize_scale_factor(window.scale_factor());
        self.record_window_state(window);
    }

    /// Snapshot current size, outer position, fullscreen/maximize state, and
    /// monitor from `window` without clobbering the remembered windowed rect
    /// while fullscreen, maximized, or minimised.
    pub fn record_window_state(&mut self, window: &Window) {
        if window.is_minimized() == Some(true) {
            return;
        }
        let sf = sanitize_scale_factor(window.scale_factor());
        let inner = window.inner_size();
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let fullscreen = window.fullscreen().is_some();
        let maximized = window.is_maximized();
        let monitor = window
            .current_monitor()
            .map(|m| MonitorInfo::from_handle(&m));

        let mut next = self.saved.clone();
        next.fullscreen = fullscreen;
        next.maximized = maximized;
        if monitor.is_some() {
            next.monitor = monitor;
        }
        if !fullscreen && !maximized {
            next.width = f64::from(inner.width) / sf;
            next.height = f64::from(inner.height) / sf;
            if let Ok(pos) = window.outer_position() {
                next.position = Some([pos.x, pos.y]);
            }
        }
        if next != self.saved {
            self.saved = next;
            self.dirty_save_since.get_or_insert_with(Instant::now);
        }
    }

    /// Flush `window-<ns>.json` if a geometry change settled past the 200 ms debounce.
    pub fn flush_save_if_due(&mut self) {
        if self
            .dirty_save_since
            .is_some_and(|since| since.elapsed() >= SAVE_DEBOUNCE)
        {
            self.flush_save_now();
        }
    }

    /// Immediately write `window-<ns>.json` if configured.
    pub fn flush_save_now(&mut self) {
        self.dirty_save_since = None;
        let Some(path) = &self.store_path else { return };
        if let Err(e) = save_window_atomic(path, &self.saved) {
            log::warn!("could not save window state {}: {e}", path.display());
        }
    }

    /// Handle native OS window fullscreen key (`⌃⌘F` on macOS, `F11` elsewhere).
    /// Returns `true` when consumed.
    pub fn handle_os_fullscreen_key(
        &mut self,
        event: &WinitKeyEvent,
        modifiers: ModifiersState,
        window: Option<&Window>,
    ) -> bool {
        let PhysicalKey::Code(code) = event.physical_key else {
            return false;
        };
        if !is_os_fullscreen_shortcut(Platform::host(), modifiers, code, event.state, event.repeat)
        {
            return false;
        }
        if let Some(win) = window {
            // Snapshot normal windowed bounds right before entering fullscreen.
            self.record_window_state(win);
            let next = if win.fullscreen().is_some() {
                None
            } else {
                Some(Fullscreen::Borderless(None))
            };
            win.set_fullscreen(next);
            self.saved.fullscreen = win.fullscreen().is_some();
            self.dirty_save_since = Some(Instant::now());
        }
        true
    }

    /// Handle `WindowEvent::Focused(focused)`: tracks OS key-window state and,
    /// on blur, clears stuck modifiers/buttons and cancels any active drag or
    /// held close-chord (`⌘X` / `Ctrl+Alt+X`).
    pub fn on_focused(
        &mut self,
        focused: bool,
        wall: Option<&mut Wall>,
        cursor: Point,
        time_ms: f64,
        modifiers: &mut ModifiersState,
        buttons: &mut u8,
    ) {
        self.window_focused = focused;
        if !focused {
            *modifiers = ModifiersState::empty();
            *buttons = 0;
            if let Some(w) = wall {
                let cancel_ev = PointerEvent {
                    pointer_id: 1,
                    x: cursor.x,
                    y: cursor.y,
                    button: None,
                    buttons: Buttons(0),
                    mods: Mods::default(),
                    time_ms,
                };
                w.pointer_cancel(&cancel_ev);
                // Releasing `KeyX` with empty modifiers triggers `release_close_hold`
                // without sending a key release into any terminal's PTY.
                let empty_lookup = screen_lookup(|_| None);
                let _ = w.key(
                    &KeyEvent {
                        code: "KeyX".to_string(),
                        key: "x".to_string(),
                        text: None,
                        pressed: false,
                        repeat: false,
                        mods: Mods::default(),
                    },
                    &empty_lookup,
                );
            }
        }
    }

    /// Resolve `(TerminalDraw.focused, TerminalDraw.border_color)` for a
    /// terminal given whether it is focused/hovered on the wall and whether the
    /// OS window itself is key:
    ///
    /// - When the OS window IS key (`window_focused == true`), the wall's
    ///   focused terminal gets `focused = true` (solid cursor + `#6ee7a0`
    ///   border + cursor blink).
    /// - When the OS window is NOT key (`window_focused == false`), `focused`
    ///   drops to `false` so `wall_cells.wgsl` draws its cursor **hollow** and
    ///   `next_cursor_blink` schedules no background timers, while keeping a
    ///   quiet green `#2f7a52` border on the wall's active terminal.
    pub fn terminal_draw_focus(
        &self,
        term_focused: bool,
        term_hovered: bool,
    ) -> (bool, Option<u32>) {
        if self.window_focused {
            let border = term_hovered
                .then(|| {
                    let (r, g, b) = HOVER_BORDER_RGB;
                    term_cells::rgb(r, g, b)
                })
                .filter(|_| !term_focused);
            (term_focused, border)
        } else {
            let border = if term_focused {
                let (r, g, b) = UNFOCUSED_WINDOW_ACTIVE_BORDER_RGB;
                Some(term_cells::rgb(r, g, b))
            } else if term_hovered {
                let (r, g, b) = HOVER_BORDER_RGB;
                Some(term_cells::rgb(r, g, b))
            } else {
                None
            };
            (false, border)
        }
    }

    /// Apply `WallEffect::SetCursor` and remember it so `egui` passes cannot
    /// reset the canvas cursor back to `Default`.
    pub fn set_cursor(&mut self, window: Option<&Window>, cursor: CursorIcon) {
        self.wall_cursor = cursor;
        if let Some(w) = window {
            w.set_cursor(cursor_of(cursor));
        }
    }

    /// Re-apply the wall's cursor after an `egui` pass that did not claim the pointer.
    pub fn restore_cursor(&self, window: &Window) {
        window.set_cursor(cursor_of(self.wall_cursor));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec2;
    use glyph_font::{FamilyId, FontAtlas};
    use wall_core::layout::Size;
    use wall_core::model::{CellGeometry, WallEffect};
    use wall_core::wall::{TerminalAppeared, WallOptions as RulesOptions};

    use crate::wall::camera::WallCamera;
    use crate::wall::chrome::ChromeFonts;
    use crate::wall::describe::describe_frame_chrome;
    use crate::wall::labels::{Label, LabelHeight};
    use crate::wall::metrics::CellMetrics;
    use crate::wall::shapes::Shape;

    fn load_test_fonts() -> (CellMetrics, FontAtlas, FamilyId) {
        let specs = glyph_font::default_font_chain(&crate::wall::glyphs::default_fonts_dir());
        let atlas = FontAtlas::load(&specs).expect("load default font chain");
        let family = atlas
            .family_id(glyph_font::MONASPACE_NEON_FAMILY)
            .unwrap_or_else(|| atlas.default_family());
        let fm = atlas.family_metrics(family);
        let raw_w = (f64::from(fm.advance_fu) * f64::from(crate::text::CELL_HEIGHT_WORLD)
            / fm.extents.em_height()) as f32;
        let metrics = CellMetrics::snapped(raw_w, crate::text::CELL_HEIGHT_WORLD);
        (metrics, atlas, family)
    }

    fn test_wall() -> Wall {
        let rules = RulesOptions {
            geometry: CellGeometry::with_padding(1.2, 2.4, 0.3, 0.3),
            platform: Platform::Mac,
            viewport: Size::new(1600.0, 1000.0),
            scale_factor: 1.0,
            namespace: "test".to_string(),
            ..RulesOptions::default()
        };
        let mut wall = Wall::in_memory(rules);
        wall.terminal_appeared(TerminalAppeared::new("t1", 80, 24).with_title("zsh"));
        wall.terminal_appeared(TerminalAppeared::new("t2", 80, 24).with_title("cargo test"));
        let _ = wall.drain_effects();
        wall
    }

    #[test]
    fn scale_factor_1x_to_2x_updates_every_cache() {
        let mut wall = test_wall();
        // Zoom out enough that map labels appear alongside title bars, selection
        // outlines, and a notice so every chrome pixel path is exercised.
        wall.run("wall.select t1");
        wall.run("wall.fit");
        let _ = wall.tick(1.0, 1000.0);
        wall.pinch(800.0, 500.0, 0.08);
        let _ = wall.run("wall.attach");
        let _ = wall.tick(1.0, 2000.0);
        let _ = wall.drain_effects();

        let base_style = WallStyle::default();
        let mut renderer_style = WallStyle {
            panel_padding: 0.3 / 3.2,
            line_thickness_scale: 1.4,
            ..WallStyle::default()
        };

        let (metrics, mut atlas, family) = load_test_fonts();
        assert!(
            (metrics.cell_height - crate::text::CELL_HEIGHT_WORLD).abs() < 1e-6,
            "CellMetrics is normalized in world units, independent of DPI"
        );

        // ── Pass 1: non-Retina 1.0 (1600×1000 physical = 1600×1000 logical) ──
        let sync_1x = sync_viewport_and_style(
            1600,
            1000,
            1.0,
            &base_style,
            &mut renderer_style,
            Some(&mut wall),
        )
        .expect("non-zero 1x viewport");
        let _ = wall.drain_effects();
        let described_1x = wall.frame();
        let cam_1x = WallCamera::new(
            Vec2::new(described_1x.camera.x as f32, described_1x.camera.y as f32),
            described_1x.camera.zoom as f32 * sync_1x.scale_factor as f32,
        );
        let mut shapes_1x: Vec<Shape> = Vec::new();
        let mut labels_1x: Vec<Label<'static>> = Vec::new();
        let paper_1x = {
            let mut fonts = ChromeFonts::new(metrics, &mut atlas, family);
            describe_frame_chrome(
                &described_1x,
                &cam_1x,
                (sync_1x.physical_width, sync_1x.physical_height),
                sync_1x.scale_factor as f32,
                &mut fonts,
                &mut shapes_1x,
                &mut labels_1x,
            )
        };
        let fm_after_1x = atlas.family_metrics(family).extents.em_height();
        let style_1x = renderer_style.clone();

        // ── Pass 2: Retina 2.0 (3200×2000 physical = same 1600×1000 logical) ──
        let sync_2x = sync_viewport_and_style(
            3200,
            2000,
            2.0,
            &base_style,
            &mut renderer_style,
            Some(&mut wall),
        )
        .expect("non-zero 2x viewport");
        let _ = wall.drain_effects();
        let described_2x = wall.frame();
        let cam_2x = WallCamera::new(
            Vec2::new(described_2x.camera.x as f32, described_2x.camera.y as f32),
            described_2x.camera.zoom as f32 * sync_2x.scale_factor as f32,
        );
        let mut shapes_2x: Vec<Shape> = Vec::new();
        let mut labels_2x: Vec<Label<'static>> = Vec::new();
        let paper_2x = {
            let mut fonts = ChromeFonts::new(metrics, &mut atlas, family);
            describe_frame_chrome(
                &described_2x,
                &cam_2x,
                (sync_2x.physical_width, sync_2x.physical_height),
                sync_2x.scale_factor as f32,
                &mut fonts,
                &mut shapes_2x,
                &mut labels_2x,
            )
        };
        let fm_after_2x = atlas.family_metrics(family).extents.em_height();

        // 1. Surface dimensions double; Wall logical viewport stays 1600×1000 while scale_factor becomes 2.0.
        assert_eq!((sync_1x.physical_width, sync_1x.physical_height), (1600, 1000));
        assert_eq!((sync_2x.physical_width, sync_2x.physical_height), (3200, 2000));
        assert_eq!((described_1x.viewport.w, described_1x.viewport.h), (1600.0, 1000.0));
        assert_eq!((described_2x.viewport.w, described_2x.viewport.h), (1600.0, 1000.0));
        assert_eq!(described_1x.scale_factor, 1.0);
        assert_eq!(described_2x.scale_factor, 2.0);

        // 2. Renderer camera physical zoom doubles for the same logical camera zoom.
        assert!((cam_2x.zoom - cam_1x.zoom * 2.0).abs() < 1e-4);

        // 3. WallRenderer style physical-px fields double; live-tuned world-space padding/knobs and Slug font metrics stay untouched.
        assert!((renderer_style.border_width_px - style_1x.border_width_px * 2.0).abs() < 1e-6);
        assert!((renderer_style.cursor_border_px - style_1x.cursor_border_px * 2.0).abs() < 1e-6);
        assert!((renderer_style.greek_px - style_1x.greek_px * 2.0).abs() < 1e-6);
        assert!((renderer_style.panel_padding - 0.3 / 3.2).abs() < 1e-6);
        assert!((renderer_style.line_thickness_scale - 1.4).abs() < 1e-6);
        assert!((fm_after_2x - fm_after_1x).abs() < 1e-9);

        // 4. Chrome physical pixels (paper grid, shape strokes/radii, screen-px labels) double.
        assert!((paper_2x.line_px - paper_1x.line_px * 2.0).abs() < 1e-5);
        assert!((paper_2x.min_px - paper_1x.min_px * 2.0).abs() < 1e-5);
        assert_eq!(shapes_1x.len(), shapes_2x.len());
        assert!(!shapes_1x.is_empty());
        for (s1, s2) in shapes_1x.iter().zip(&shapes_2x) {
            if let (Shape::Rect(r1), Shape::Rect(r2)) = (s1, s2) {
                assert!((r2.stroke_px - r1.stroke_px * 2.0).abs() < 1e-4);
                assert!((r2.corner_radius_px - r1.corner_radius_px * 2.0).abs() < 1e-4);
            }
        }
        assert_eq!(labels_1x.len(), labels_2x.len());
        let mut saw_screen_px = false;
        for (l1, l2) in labels_1x.iter().zip(&labels_2x) {
            if let (LabelHeight::ScreenPx(h1), LabelHeight::ScreenPx(h2)) = (l1.height, l2.height) {
                saw_screen_px = true;
                assert!((h2 - h1 * 2.0).abs() < 1e-4);
            }
        }
        assert!(saw_screen_px, "expected screen-px map labels at zoom 0.2");

        // 5. egui Context pixels_per_point follows native_pixels_per_point 1.0 -> 2.0.
        #[cfg(feature = "egui-ui")]
        {
            let ctx = egui::Context::default();
            crate::wall::ui::configure_wall_context(&ctx, &crate::config::settings().wall);
            for expected_ppp in [1.0_f32, 2.0_f32] {
                let mut raw = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::Vec2::new(1600.0, 1000.0),
                    )),
                    ..Default::default()
                };
                raw.viewports
                    .entry(egui::ViewportId::ROOT)
                    .or_default()
                    .native_pixels_per_point = Some(expected_ppp);
                let out = ctx.run_ui(raw, |_ui| {});
                assert!((out.pixels_per_point - expected_ppp).abs() < 1e-6);
                assert!((ctx.pixels_per_point() - expected_ppp).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn zero_size_resize_skips_reconfigure_without_corrupting_viewport() {
        let mut wall = test_wall();
        let base = WallStyle::default();
        let mut style = base.clone();
        assert!(
            sync_viewport_and_style(1600, 1000, 2.0, &base, &mut style, Some(&mut wall)).is_some()
        );
        assert_eq!(wall.frame().viewport, Size::new(800.0, 500.0));

        assert!(sync_viewport_and_style(0, 1000, 2.0, &base, &mut style, Some(&mut wall)).is_none());
        assert!(sync_viewport_and_style(1600, 0, 2.0, &base, &mut style, Some(&mut wall)).is_none());
        assert!(sync_viewport_and_style(0, 0, 2.0, &base, &mut style, Some(&mut wall)).is_none());
        // Wall viewport stays valid and unchanged across collapsed/minimised 0×0 events.
        assert_eq!(wall.frame().viewport, Size::new(800.0, 500.0));
    }

    #[test]
    fn saved_window_round_trips_and_falls_back_when_monitor_is_unplugged() {
        let tmp = std::env::temp_dir().join(format!("glyph-win-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        assert!(window_store_path(&tmp, None).is_none());
        assert!(window_store_path(&tmp, Some("../escape")).is_none());
        let path = window_store_path(&tmp, Some("probe")).expect("valid namespace path");
        assert_eq!(path, tmp.join("window-probe.json"));

        let retina = MonitorInfo {
            name: Some("Built-in Retina Display".to_string()),
            position: [0, 0],
            size: [2880, 1800],
            scale_factor: 2.0,
        };
        let external = MonitorInfo {
            name: Some("DELL U2723QE".to_string()),
            position: [2880, 0],
            size: [3840, 2160],
            scale_factor: 2.0,
        };

        let saved = SavedWindow {
            width: 1700.0,
            height: 1020.0,
            position: Some([3100, 120]),
            fullscreen: true,
            maximized: false,
            monitor: Some(external.clone()),
        };
        save_window_atomic(&path, &saved).expect("atomic save");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let loaded = load_saved_window(&path).expect("round-tripped saved window");
        assert_eq!(loaded, saved);

        // Both monitors present: restores onto external monitor with pre-fullscreen size + fullscreen flag.
        let both = [retina.clone(), external.clone()];
        let placed_both = resolve_window_placement(Some(&loaded), &both, Some(&retina));
        assert!(placed_both.matched_saved_monitor);
        assert_eq!((placed_both.width, placed_both.height), (1700.0, 1020.0));
        assert_eq!(placed_both.position, Some([3100, 120]));
        assert!(placed_both.fullscreen);

        // External monitor unplugged: clamps 1700×1020 -> 1440×900 (Retina logical bounds)
        // and centers on [0, 0] instead of opening off-screen at x=3100.
        let single = [retina.clone()];
        let placed_fallback = resolve_window_placement(Some(&loaded), &single, Some(&retina));
        assert!(!placed_fallback.matched_saved_monitor);
        assert_eq!((placed_fallback.width, placed_fallback.height), (1440.0, 900.0));
        assert_eq!(placed_fallback.position, Some([0, 0]));
        assert!(placed_fallback.fullscreen);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn os_fullscreen_shortcut_is_separate_from_wall_fullscreen_chords() {
        let mut ctrl_cmd = ModifiersState::empty();
        ctrl_cmd.set(ModifiersState::SUPER, true);
        ctrl_cmd.set(ModifiersState::CONTROL, true);
        assert!(is_os_fullscreen_shortcut(
            Platform::Mac,
            ctrl_cmd,
            KeyCode::KeyF,
            ElementState::Pressed,
            false,
        ));
        // Plain ⌘F, ⌘↵ (wall.fullscreen), and key repeats do NOT trigger OS fullscreen.
        let mut cmd_only = ModifiersState::empty();
        cmd_only.set(ModifiersState::SUPER, true);
        assert!(!is_os_fullscreen_shortcut(
            Platform::Mac,
            cmd_only,
            KeyCode::KeyF,
            ElementState::Pressed,
            false,
        ));
        assert!(!is_os_fullscreen_shortcut(
            Platform::Mac,
            cmd_only,
            KeyCode::Enter,
            ElementState::Pressed,
            false,
        ));
        assert!(!is_os_fullscreen_shortcut(
            Platform::Mac,
            ctrl_cmd,
            KeyCode::KeyF,
            ElementState::Pressed,
            true,
        ));

        // Linux/Windows: plain F11 toggles OS fullscreen; Ctrl+Alt+Enter (wall.fullscreen) does not.
        assert!(is_os_fullscreen_shortcut(
            Platform::Other,
            ModifiersState::empty(),
            KeyCode::F11,
            ElementState::Pressed,
            false,
        ));
        let mut ctrl_alt = ModifiersState::empty();
        ctrl_alt.set(ModifiersState::CONTROL, true);
        ctrl_alt.set(ModifiersState::ALT, true);
        assert!(!is_os_fullscreen_shortcut(
            Platform::Other,
            ctrl_alt,
            KeyCode::Enter,
            ElementState::Pressed,
            false,
        ));
    }

    #[test]
    fn window_blur_makes_cursor_hollow_and_cancels_close_hold_until_refocused() {
        let mut wall = test_wall();
        let active_id = wall.focused_id().expect("focused terminal").to_string();
        let _ = wall.drain_effects();

        // Start a held close chord (`⌘X`) on the focused terminal before blur.
        let empty_lookup = screen_lookup(|_| None);
        let _ = wall.key(
            &KeyEvent {
                code: "KeyX".to_string(),
                key: "x".to_string(),
                text: None,
                pressed: true,
                repeat: false,
                mods: Mods { meta: true, ..Mods::default() },
            },
            &empty_lookup,
        );
        let _ = wall.tick(0.05, 50.0);
        assert!(
            wall.frame()
                .terminals
                .iter()
                .find(|t| t.id == active_id)
                .and_then(|t| t.close_hold_progress)
                .is_some(),
            "close hold ring should be active before blur"
        );

        let mut win = WindowLifecycle::new();
        assert_eq!(win.terminal_draw_focus(true, false), (true, None));

        let mut mods = ModifiersState::SUPER;
        let mut buttons = 1_u8;
        win.on_focused(
            false,
            Some(&mut wall),
            Point::new(200.0, 200.0),
            100.0,
            &mut mods,
            &mut buttons,
        );
        assert!(!win.is_window_focused());
        assert_eq!(mods, ModifiersState::empty());
        assert_eq!(buttons, 0);

        // Focused terminal's draw flag drops to false (hollow cursor + no blink timer),
        // while the wall keeps `active_id` remembered as focused with a quiet green border.
        let (draw_focused, border) = win.terminal_draw_focus(true, false);
        assert!(!draw_focused);
        assert!(border.is_some());
        assert_eq!(wall.focused_id(), Some(active_id.as_str()));

        // Ticking well past close_hold_ms must NOT kill t1 after blur cancelled the hold.
        let _ = wall.tick(1.0, 1200.0);
        let effects = wall.drain_effects();
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, WallEffect::KillTerminal { .. })),
            "blur must cancel held close chord before it kills t1"
        );

        // Refocusing the window restores solid cursor (`draw_focused == true`).
        win.on_focused(
            true,
            Some(&mut wall),
            Point::new(200.0, 200.0),
            1300.0,
            &mut mods,
            &mut buttons,
        );
        assert_eq!(win.terminal_draw_focus(true, false), (true, None));
    }

    #[test]
    fn every_wall_cursor_maps_to_distinct_winit_cursor_icon() {
        assert_eq!(CURSOR_TABLE.len(), 13);
        for (i, entry) in CURSOR_TABLE.iter().enumerate() {
            assert_eq!(entry.wall.as_css(), entry.css);
            assert_eq!(cursor_of(entry.wall), entry.winit);
            for other in &CURSOR_TABLE[..i] {
                assert_ne!(
                    entry.winit, other.winit,
                    "duplicate winit cursor mapping for {} and {}",
                    other.css, entry.css
                );
            }
        }
    }
}
