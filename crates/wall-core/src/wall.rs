//! The wall as one object (`wall/TerminalWall.js`), with no window, no renderer
//! and no daemon in it.
//!
//! [`Wall`] is the composition root of `wall-core`. It owns every rule subsystem
//! — [`WallModel`] (terminals, order, Z-stack, focus & history, camera, config,
//! store, effects), [`Groups`] (tile, freeform, and binary-split layouts),
//! [`WallFullscreen`] (single and group fullscreen, zoom/unzoom, text scaling,
//! reload adoption), [`WallNames`] (title bars and map labels), [`WallPointer`]
//! (drag/resize/pan/zoom/lasso/divider/pane/mouse/scroll state machine),
//! [`PressedKeys`] (chords and key encoding), per-terminal [`TerminalSelection`],
//! [`TermdStatus`], and [`CommandRouter`] with every `wall.*` and `terminal.*`
//! verb registered — and implements their host traits so the subsystems talk to
//! each other directly.
//!
//! ### Event and frame contract
//! 1. **Input in as plain data**: pointer down/move/up/cancel/double-click,
//!    wheel, pinch, context menu, keyboard ([`KeyEvent`]), paste text,
//!    clipboard replies, viewport size + scale factor, clock (`tick`), and
//!    terminal/daemon news (`terminal_appeared`, `terminal_resized`,
//!    `terminal_title`, `terminal_modes`, `terminal_ended`,
//!    `set_live_terminals`).
//! 2. **Side effects out**: drain [`Wall::drain_effects`] after input, ticks, or
//!    verbs and execute each [`WallEffect`] against the daemon, window, or OS
//!    clipboard.
//! 3. **Frame out**: call [`Wall::frame`] once per redraw to obtain a plain-data
//!    [`WallFrame`] in world units (and screen px for map labels and docked
//!    panels) describing everything to draw.
//! 4. **Screens stay with the caller**: gestures and verbs that read terminal
//!    text take a [`ScreenLookup`] reference and never copy [`term_cells::Screen`]
//!    into [`Wall`].

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::str::FromStr;

use serde_json::{json, Value};
use term_cells::{Modes, Screen};

use crate::agent_view::{self, AgentViewDims};
use crate::bus::{
    attach_argv, bus_attachable, bus_hosts, find_bus_agent, gomux_screen_id,
    is_bus_target, local_bus_name, GomuxSnapshot,
};
use crate::camera::{CameraPose, ScreenBox, WallCamera};
use crate::chord::{self, is_link_open_mods, is_paste_chord};
use crate::commands::{
    self, AgentBulkExpandResult, AgentEarlierResult, AgentJumpResult, AgentOpenResult,
    AgentSideResult, AgentToggleEventResult, AttachOutcome, BusAgentEntry, BusHostGroup, BusView,
    ConfigSearchGroup, FullscreenStatus, GroupSummary, MenuTarget as CmdMenuTarget, PanelKind,
    PanelPatch, PanelPlacement, PointPayload, TerminalSummary, WallApi,
    WallSnapshot as CmdWallSnapshot,
};
use crate::config::{AgentSide, GroupLayout, WallConfig, WALL_CONFIG_KEYS};
use crate::dock::{dock_layout, DockLayout, DockSide, DockState, PanelSize};
use crate::fullscreen::{
    dock_state_from_store, EnterOptions, FullscreenArea, FullscreenConfig, FullscreenHost,
    FullscreenTerminal, TextStep, WallFullscreen, BACKDROP_OPACITY,
};
use crate::geom::{Point, Rect};
use crate::groups::{
    CreateGroupOptions, Group, GroupConfig, GroupDrag, Groups, PaneInset, SetLayoutOptions,
    SplitInsertDir, TerminalGeometry, TileMetrics, WallHost,
};
use crate::input::{Button, KeyEvent, Mods, Platform};
use crate::keymap::{
    self, FieldFocus, KeyContext, KeyDecision, PressedKeys, WallCommand,
};
use crate::labels::Leader;
use crate::layout::{
    cycle, hit_test, reading_order, union_rect, Edges, GridSize, HitPart, HitTarget, ReadingItem,
    Size, BAR_ROOM, DEFAULT_FIT_MARGIN, FULLSCREEN_Z,
};
use crate::links::{is_allowed_url_scheme, link_at_cell, HoveredLink, ScreenView};
use crate::menu::{
    self, ContextDecision, ContextEvent, MenuTarget, TerminalPart, Verdict,
};
use crate::model::{
    CellGeometry, LandedAs, MemoryStorage, OpenMenu, SplitAxis, TerminalEntry, TerminalSpec,
    WallEffect, WallModel, WallSnapshot, EDGE_GRAB_PX, FIT_ALL_MARGIN, FRAME_MAX_LINE_PX,
};
use crate::mouse::MouseTracking;
use crate::names::{
    self, NamedTerminal, NamesFrame, TitleBarModel, WallNames, GROUP_HEADER_STYLE,
};
use crate::pointer::{
    cell_at_point, CanvasHit, CellPoint, CursorIcon, DropPreview,
    FullscreenState as PointerFullscreenState, PointerConfig, PointerEvent, PointerHost,
    SelectionMode, SplitEdge, TerminalMouseEvent, WallPointer, WheelEvent, ZoomKeys, DRAG_SLOP_PX,
};
use crate::router::{CommandInput, CommandMeta, CommandReply, CommandRouter, RegisterMeta};
use crate::selection::TerminalSelection;
use crate::settings::{KnobInput, KnobValue};
use crate::sidebar::{AgentStatus, SidebarMode};
use crate::snap::{grid_levels, snap_step, snap_to, GridLevels, GridSpec};
use crate::split::{self, BranchSide, Direction, PaneMap};
use crate::status::{self, StatusNotice, TermdStatus, DEFAULT_RESTART_GRACE_MS};
use crate::store::{
    AgentViewState, FullscreenMode, FullscreenState, HelpPanelPatch, HelpPanelState,
    SettingsPanelPatch, SettingsPanelState, SidebarPatch, SidebarState, StatusWindowPatch,
    StatusWindowState, WallGroup, WallStore,
};

/// The column count taken for a terminal whose screen has not arrived yet
/// (`session::NEW_COLS`). A NEW terminal's size is the `newTerminalSize` knob
/// ([`crate::config::NEW_TERMINAL_SIZE_DEFAULT`], the same 100x30).
pub const DEFAULT_SPAWN_COLS: u32 = 100;
/// The row count taken for a terminal whose screen has not arrived yet
/// (`session::NEW_ROWS`).
pub const DEFAULT_SPAWN_ROWS: u32 = 30;
/// Default logical viewport width when none has been reported yet.
pub const DEFAULT_VIEWPORT_W: f64 = 1400.0;
/// Default logical viewport height when none has been reported yet.
pub const DEFAULT_VIEWPORT_H: f64 = 900.0;
/// Fallback unit width per character used to size title bars and group headers
/// when the window does not supply a measured font advance (`advance / line_height`).
pub const DEFAULT_TITLE_CHAR_UNIT_WIDTH: f64 = 0.6;
/// World Z offset for the fullscreen backdrop, just beneath `FULLSCREEN_Z`.
const FULLSCREEN_BACKDROP_Z: f64 = FULLSCREEN_Z - 1.0;

// ── Screen lookup seam ───────────────────────────────────────────────────────

/// Read-only lookup for live [`Screen`] instances owned by the caller.
///
/// [`Wall`] never copies terminal screens; callers hand a lookup to gestures or
/// verbs that inspect cell text (text selection drag, double/triple-click, copy).
pub trait ScreenLookup {
    fn screen(&self, id: &str) -> Option<&Screen>;

    /// Optional [`ScreenView`] providing `OSC 8` hyperlinks alongside [`Self::screen`].
    /// Defaults to forwarding [`Self::screen`] (`term_cells::Screen` implements [`ScreenView`]).
    fn screen_view(&self, id: &str) -> Option<&dyn ScreenView> {
        self.screen(id).map(|s| s as &dyn ScreenView)
    }
}

/// Empty [`ScreenLookup`] returning `None` for every terminal id.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoScreens;

impl ScreenLookup for NoScreens {
    fn screen(&self, _id: &str) -> Option<&Screen> {
        None
    }
}

impl<T: ScreenLookup + ?Sized> ScreenLookup for &T {
    fn screen(&self, id: &str) -> Option<&Screen> {
        (**self).screen(id)
    }

    fn screen_view(&self, id: &str) -> Option<&dyn ScreenView> {
        (**self).screen_view(id)
    }
}

/// Closure adapter implementing [`ScreenLookup`] without copying screens.
pub struct FnScreenLookup<'a, F> {
    f: F,
    _marker: std::marker::PhantomData<&'a Screen>,
}

/// Wrap a lookup closure `Fn(&str) -> Option<&Screen>` as a [`ScreenLookup`].
pub fn screen_lookup<'a, F>(f: F) -> FnScreenLookup<'a, F>
where
    F: Fn(&str) -> Option<&'a Screen>,
{
    FnScreenLookup {
        f,
        _marker: std::marker::PhantomData,
    }
}

impl<'a, F> ScreenLookup for FnScreenLookup<'a, F>
where
    F: Fn(&str) -> Option<&'a Screen>,
{
    fn screen(&self, id: &str) -> Option<&Screen> {
        (self.f)(id)
    }
}

impl ScreenLookup for HashMap<String, Screen> {
    fn screen(&self, id: &str) -> Option<&Screen> {
        self.get(id)
    }
}

impl ScreenLookup for BTreeMap<String, Screen> {
    fn screen(&self, id: &str) -> Option<&Screen> {
        self.get(id)
    }
}

// ── Construction options & terminal news ─────────────────────────────────────

/// Measured monospace character advances supplied by the host renderer for
/// sizing title bars, group headers, and map-label pills without coupling
/// `wall-core` to a font rasterizer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CharWidths {
    /// Character advance divided by line height (`advance / line_height`), used
    /// to size terminal title bars and group headers (default `0.6`).
    pub title_unit_width: f64,
    /// Character advance in logical CSS pixels at the `12 px` map-label font
    /// size, used to size map-label pills (default `7.2`, matching `MapLabels.js`).
    pub map_label_char_px: f64,
}

impl Default for CharWidths {
    fn default() -> Self {
        Self {
            title_unit_width: DEFAULT_TITLE_CHAR_UNIT_WIDTH,
            map_label_char_px: names::DEFAULT_MAP_LABEL_CHAR_PX,
        }
    }
}

impl CharWidths {
    /// Construct from explicit `(title_unit_width, map_label_char_px)` measurements.
    pub const fn new(title_unit_width: f64, map_label_char_px: f64) -> Self {
        Self {
            title_unit_width,
            map_label_char_px,
        }
    }

    /// Construct from a monospace font's `advance / line_height` ratio, deriving
    /// `map_label_char_px = unit_width * 12.0` (the default `12 px` map-label font).
    pub const fn from_unit_width(unit_width: f64) -> Self {
        Self {
            title_unit_width: unit_width,
            map_label_char_px: unit_width * 12.0,
        }
    }

    fn sanitized(self) -> Self {
        let defaults = Self::default();
        Self {
            title_unit_width: if self.title_unit_width.is_finite() && self.title_unit_width > 0.0 {
                self.title_unit_width
            } else {
                defaults.title_unit_width
            },
            map_label_char_px: if self.map_label_char_px.is_finite()
                && self.map_label_char_px > 0.0
            {
                self.map_label_char_px
            } else {
                defaults.map_label_char_px
            },
        }
    }
}

/// Construction parameters for [`Wall`].
#[derive(Clone, Debug)]
pub struct WallOptions {
    /// Base configuration before persisted dialled overrides from [`WallStore`].
    pub born_config: WallConfig,
    /// Cell advance stride, panel padding, and depth scale in world units.
    pub geometry: CellGeometry,
    /// Measured monospace character advances for title bars, group headers, and map labels.
    pub char_widths: CharWidths,
    /// Host platform conventions (chord key, block-select modifier, copy/paste chords).
    pub platform: Platform,
    /// Initial logical viewport dimensions in px.
    pub viewport: Size,
    /// Initial display scale factor (physical px per logical px).
    pub scale_factor: f64,
    /// `g3d-termd` namespace name carried by [`TermdStatus`].
    pub namespace: String,
    /// Initial camera pose before any framing occurs.
    pub initial_camera: CameraPose,
}

impl Default for WallOptions {
    fn default() -> Self {
        Self {
            born_config: WallConfig::default(),
            geometry: CellGeometry::default(),
            char_widths: CharWidths::default(),
            platform: Platform::host(),
            viewport: Size::new(DEFAULT_VIEWPORT_W, DEFAULT_VIEWPORT_H),
            scale_factor: 1.0,
            namespace: "rs-wall".to_owned(),
            initial_camera: CameraPose::new(0.0, 0.0, 20.0),
        }
    }
}

/// News that a terminal session has appeared on the daemon (`terminal_appeared`).
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalAppeared {
    pub id: String,
    pub cols: u32,
    pub rows: u32,
    pub title: String,
    pub modes: Modes,
    /// `false` for a newly spawned terminal (consumes a pending spawn/split and
    /// lands focused + framed); `true` for a pre-existing session adopted on
    /// connect or restart (restores its saved spot and fullscreen state).
    pub adopted: bool,
}

impl TerminalAppeared {
    /// A newly spawned terminal arriving in response to [`WallEffect::SpawnTerminal`].
    pub fn new(id: impl Into<String>, cols: u32, rows: u32) -> Self {
        Self {
            id: id.into(),
            cols,
            rows,
            title: String::new(),
            modes: Modes::default(),
            adopted: false,
        }
    }

    /// A pre-existing terminal adopted on daemon connect / restart.
    pub fn adopted(id: impl Into<String>, cols: u32, rows: u32) -> Self {
        Self {
            adopted: true,
            ..Self::new(id, cols, rows)
        }
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    pub fn with_modes(mut self, modes: Modes) -> Self {
        self.modes = modes;
        self
    }
}

/// Plain-data summary of how one terminal's [`Screen`] changed on an emulator
/// fill (mirrors `glyph_vt::FillDelta` without coupling `wall-core` to
/// `glyph-vt`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScreenFillDelta<'a> {
    /// Whether the terminal grid resized since the previous fill.
    pub resized: bool,
    /// Number of rows scrolled off row `0` since the previous fill.
    pub scrolled_top: u32,
    /// Whether scrolling was confined to margins (`DECSTBM` / `DECSLRM`) rather
    /// than shifting the whole screen upward.
    pub within_margins: bool,
    /// Per-row half-open column ranges `(lo, hi)` rewritten in place (`lo >= hi`
    /// or omitted trailing rows mean unchanged).
    pub rewritten_cols: &'a [(u16, u16)],
}

/// Extract the last non-blank line from `screen`, trimmed at both ends, into
/// `buf` (reusing `buf`'s capacity so unchanged screens do zero allocation).
fn extract_preview_into(screen: &Screen, buf: &mut String) {
    buf.clear();
    for r in (0..screen.rows).rev() {
        let row = screen.row(r);
        let mut has_non_space = false;
        for cell in row {
            if cell.width == 0 || cell.content == 0 {
                continue;
            }
            if cell.content < term_cells::CLUSTER_BASE {
                if let Some(ch) = char::from_u32(cell.content) {
                    if !ch.is_whitespace() {
                        has_non_space = true;
                        break;
                    }
                }
            } else if !screen.content_str(cell).trim().is_empty() {
                has_non_space = true;
                break;
            }
        }
        if !has_non_space {
            continue;
        }
        for cell in row {
            if cell.width == 0 {
                continue;
            }
            if cell.content == 0 {
                buf.push(' ');
            } else {
                buf.push_str(&screen.content_str(cell));
            }
        }
        let trimmed = buf.trim();
        let start = trimmed.as_ptr() as usize - buf.as_ptr() as usize;
        let len = trimmed.len();
        if start > 0 {
            buf.drain(..start);
        }
        buf.truncate(len);
        return;
    }
}

/// Extract the last non-blank line from `screen` for sidebar/palette previews.
pub fn preview_from_screen(screen: &Screen) -> String {
    let mut buf = String::new();
    extract_preview_into(screen, &mut buf);
    buf
}

// ── Plain-data frame description (`WallFrame`) ───────────────────────────────

/// Drafting-paper background and grid parameters for one frame.
#[derive(Clone, Debug, PartialEq)]
pub struct PaperFrame {
    pub background: String,
    pub grid_show: bool,
    pub grid: GridSpec,
    pub levels: GridLevels,
    pub line_px: f64,
    pub minor_color: String,
    pub major_color: String,
}

/// Terminal border visual flags (`BORDER_FLAGS` in JS).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BorderState {
    pub edge: bool,
    pub focused: bool,
    pub hovered: bool,
}

/// Highlighted text selection cell span inside one terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextSelectionFrame {
    pub r0: usize,
    pub c0: usize,
    pub r1: usize,
    pub c1: usize,
    pub block: bool,
    pub background: String,
    pub foreground: String,
}

/// One shown terminal to draw on the canvas.
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalFrame {
    pub id: String,
    /// Top-left corner of cell `(0, 0)` in world units (+X right, +Y up).
    pub cell_origin: Point,
    /// World-space rectangle of the terminal's body panel (cells + padding).
    pub body_rect: Rect,
    pub cols: u32,
    pub rows: u32,
    pub scale: f64,
    pub z: f64,
    pub focused: bool,
    pub hovered: bool,
    pub selected: bool,
    pub border: BorderState,
    pub text_selection: Option<TextSelectionFrame>,
    /// Active link span under the pointer when the link-open modifier (`⌘` / `Ctrl`) is held.
    pub hovered_link: Option<HoveredLink>,
    pub fullscreen: bool,
    /// Fill fraction (`0.0..=1.0`) while hold-to-close is active on this terminal.
    pub close_hold_progress: Option<f64>,
}

/// One terminal title bar in world space.
#[derive(Clone, Debug, PartialEq)]
pub struct TitleBarFrame {
    pub terminal_id: String,
    pub rect: Rect,
    /// Resolved display name without any `" · <status>"` agent suffix.
    pub name: String,
    /// Full display text (`name`, or `"<name> · <status>"` when `agent_status` is `Some`).
    pub text: String,
    /// Attached gomuxai agent target (`name` or `name@host`) from the store, if any.
    pub attached_agent: Option<String>,
    /// Live gomuxai agent status when attached and present in the bus snapshot.
    pub agent_status: Option<AgentStatus>,
    /// `false` when the terminal's name has been handed off to its map label.
    pub text_visible: bool,
    pub text_left: f64,
    pub text_height: f64,
    pub text_px: f64,
    pub fill_color: u32,
    pub text_color: u32,
    pub z: f64,
    pub focused: bool,
    pub hovered: bool,
}

/// One group backdrop frame and header bar in world space.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupFrame {
    pub id: String,
    pub name: String,
    pub layout: GroupLayout,
    pub frame_rect: Rect,
    pub header_rect: Rect,
    pub text_left: f64,
    pub text_height: f64,
    pub text_px: f64,
    pub fill_color: u32,
    pub text_color: u32,
    pub z: f64,
    pub focused: bool,
    pub hovered: bool,
}

/// World-space selection outline around a selected terminal.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectionOutline {
    pub terminal_id: String,
    pub rect: Rect,
    pub z: f64,
}

/// Screen-space map label and optional leader line for a zoomed-out terminal.
#[derive(Clone, Debug, PartialEq)]
pub struct MapLabelFrame {
    pub terminal_id: String,
    /// Resolved display name without any `" · <status>"` agent suffix.
    pub name: String,
    /// Full display text (`name`, or `"<name> · <status>"` when `agent_status` is `Some`).
    pub text: String,
    /// Attached gomuxai agent target (`name` or `name@host`) from the store, if any.
    pub attached_agent: Option<String>,
    /// Live gomuxai agent status when attached and present in the bus snapshot.
    pub agent_status: Option<AgentStatus>,
    pub focused: bool,
    pub hovered: bool,
    pub label_box: ScreenBox,
    pub target: ScreenBox,
    pub leader: Option<Leader>,
}

/// Backdrop covering the canvas behind the elevated fullscreen target.
#[derive(Clone, Debug, PartialEq)]
pub struct FullscreenBackdrop {
    pub mode: FullscreenMode,
    pub target: String,
    pub area: FullscreenArea,
    pub background: String,
    /// Veil alpha (`BACKDROP_OPACITY = 0.9` in `WallFullscreen.js`).
    pub opacity: f64,
    /// World-space cutout rectangles left uncovered around the active fullscreen
    /// pane(s), mirroring `WallFullscreen._cutoutBoxes`.
    pub cutouts: Vec<Rect>,
    pub z: f64,
}

/// Chrome panel open/dock states and computed pixel offsets.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelsFrame {
    pub sidebar: SidebarState,
    pub status_window: StatusWindowState,
    pub settings_panel: SettingsPanelState,
    pub help_panel: HelpPanelState,
    pub dock: DockLayout,
    /// The command palette's size (the floating half; it has no docked form).
    pub palette: PanelSize,
}

/// Complete plain-data description of one rendered frame of the wall.
#[derive(Clone, Debug, PartialEq)]
pub struct WallFrame {
    pub camera: CameraPose,
    pub viewport: Size,
    pub scale_factor: f64,
    pub animating: bool,
    pub paper: PaperFrame,
    pub terminals: Vec<TerminalFrame>,
    pub title_bars: Vec<TitleBarFrame>,
    pub groups: Vec<GroupFrame>,
    pub selection_outlines: Vec<SelectionOutline>,
    /// Active lasso polygon in world units while Shift-dragging.
    pub lasso: Option<Vec<Point>>,
    /// Active tile/pane drop preview outline in world units (`ok = false` draws red dashed).
    pub drop_preview: Option<DropPreview>,
    pub map_labels: Vec<MapLabelFrame>,
    pub fullscreen_backdrop: Option<FullscreenBackdrop>,
    /// Transient amber wall notice (e.g. fullscreen reload reminder or clipboard hint).
    pub notice: Option<String>,
    /// Daemon outage / compatibility banner computed via [`crate::status::notice_for`]
    /// (`tone` is `Amber` for `Restarting` after `restartNoticeMs` or `Red` for `Lost` /
    /// `Lacks` / `Unreachable` / `Failed`; `dismiss` is `true` for the sticky `Lost` latch).
    pub status_notice: Option<StatusNotice>,
    pub menu: Option<OpenMenu>,
    pub palette_open: bool,
    pub panels: PanelsFrame,
}

// ── Internal pending state ───────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct PendingSpawn {
    window_top_left: Option<Point>,
    zoom_when_asked: f64,
    split: Option<(String, SplitInsertDir)>,
    attach_target: Option<String>,
}

#[derive(Clone, Debug)]
struct CloseHold {
    terminal_id: String,
    started_ms: f64,
}

// ── WallCore: everything inside CommandRouter<WallCore> ──────────────────────

struct WallCore {
    model: WallModel,
    groups: Groups,
    fullscreen: WallFullscreen,
    names: WallNames,
    selections: HashMap<String, TerminalSelection>,
    selection_text_cache: HashMap<String, String>,
    /// Last `Screen::title` observed per terminal so unchanged fills never allocate
    /// and an empty `Screen::title` never clobbers a title delivered by `Event::Title`.
    last_screen_titles: HashMap<String, String>,
    /// Per-terminal Agent View stick-to-bottom pin state (`wall.agent.follow`, defaults to `true`).
    agent_stick_bottom: HashMap<String, bool>,
    preview_scratch: String,
    pressed_keys: PressedKeys,
    termd: TermdStatus,
    /// Latest clock timestamp (ms) observed on `termd` status transitions or ticks,
    /// supporting callers that pass either monotonic or Unix epoch ms into `on_termd_*`.
    termd_now_ms: Option<u64>,
    /// Anchor `(epoch_ms, wall_ms)` linking an out-of-band epoch-ms `on_termd_*` timestamp
    /// to `WallCore::now_ms` once `tick()` begins advancing, so real elapsed time drives
    /// grace escalation without sub-ms rounding drift or clamped-`dt_secs` lag.
    termd_epoch_anchor: Option<(u64, f64)>,
    bus: Option<GomuxSnapshot>,
    bus_reason: String,
    /// True when the local `agents` extra `termd` namespace is connected (`--wall-extra-ns agents`),
    /// so `wall.attach` prefers direct local subscription (`gx-<name>`) over spawning a shell wrapper.
    agents_extra_connected: bool,
    platform: Platform,
    char_widths: CharWidths,
    scale_factor: f64,
    now_ms: f64,
    base_cursor: CursorIcon,
    cursor: CursorIcon,
    hovered_group_id: Option<String>,
    hovered_label_id: Option<String>,
    hovered_link: Option<(String, HoveredLink)>,
    pending_link_click: Option<(String, Point, String)>,
    last_pointer_screen: Option<Point>,
    last_pointer_mods: Mods,
    last_map_labels: Vec<(String, ScreenBox)>,
    lasso_screen: Option<Vec<Point>>,
    active_group_drag: Option<GroupDrag>,
    pending_spawns: VecDeque<PendingSpawn>,
    close_hold: Option<CloseHold>,
    palette_open: bool,
    field_focus: FieldFocus,
    pending_quench_wheel: bool,
    pending_cancel_scroll: bool,
    closing_group: Option<String>,
}

// ── Zero-cost borrow bridges implementing host traits ────────────────────────

struct GroupsBridge<'a> {
    model: &'a mut WallModel,
    fullscreen: &'a WallFullscreen,
    names: &'a mut WallNames,
    needs_fullscreen_refit: bool,
}

impl GroupsBridge<'_> {
    fn agent_dims(&self) -> AgentViewDims {
        let cfg = self.model.config();
        AgentViewDims::new(
            cfg.agent_view_width,
            cfg.agent_view_height,
            cfg.agent_view_gap,
        )
    }

    fn open_agent_side(&self, id: &str) -> Option<AgentSide> {
        self.model
            .store()
            .agent_view(id)
            .filter(|av| av.open)
            .map(|av| av.side)
    }

    fn canvas_body_rect_of(&self, id: &str) -> Option<Rect> {
        if self.fullscreen.has_saved(id) {
            if let Some(sv) = self
                .fullscreen
                .state()
                .and_then(|s| s.saved.as_ref().and_then(|m| m.get(id)).copied())
                .or_else(|| {
                    self.fullscreen
                        .state()
                        .filter(|s| s.id.as_deref() == Some(id))
                        .and_then(|s| s.canvas)
                })
            {
                let geom = self.model.geometry();
                let scale = self.model.terminal(id).map_or(1.0, |t| t.scale);
                let width =
                    (f64::from(sv.cols) * geom.stride.x + 2.0 * geom.padding.x) * scale;
                let height =
                    (f64::from(sv.rows) * geom.stride.y + 2.0 * geom.padding.y) * scale;
                return Some(Rect::from_top_left(sv.x, sv.y, width, height));
            }
        }
        self.model.terminal_rect(id)
    }
}

impl WallHost for GroupsBridge<'_> {
    fn group_config(&self) -> GroupConfig {
        GroupConfig::from(self.model.config())
    }

    fn terminal(&self, id: &str) -> Option<TerminalGeometry> {
        let entry = self.model.terminal(id)?;
        let geom = self.model.geometry();
        Some(TerminalGeometry {
            position: entry.position,
            rect: entry.body_rect(geom),
            cols: entry.cols,
            rows: entry.rows,
            cell_stride: Point::new(geom.stride.x * entry.scale, geom.stride.y * entry.scale),
        })
    }

    fn terminal_ids(&self) -> Vec<String> {
        self.model.order().to_vec()
    }

    fn is_shown(&self, id: &str) -> bool {
        self.model.is_shown(id)
    }

    fn on_canvas(&self, id: &str) -> bool {
        self.model.is_shown(id) && !self.fullscreen.active()
    }

    fn place_body(&mut self, id: &str, pos: Point) {
        self.model.place_body(id, pos);
    }

    fn remember_position(&mut self, id: &str) {
        self.model.remember_position(id);
    }

    fn resize_terminal(&mut self, id: &str, cols: u32, rows: u32) {
        self.model.push_effect(WallEffect::ResizeTerminal {
            id: id.to_owned(),
            cols,
            rows,
        });
    }

    fn set_bar_room(&mut self, id: &str, room: Option<f64>) {
        self.names.set_bar_room(id, room);
        self.model.set_bar_room_override(id, room);
    }

    fn fullscreen_terminal_id(&self) -> Option<&str> {
        self.fullscreen.solo_id()
    }

    fn fullscreen_group_id(&self) -> Option<&str> {
        self.fullscreen.group_id()
    }

    fn fullscreen_split_rect(&self, group: &Group) -> Option<Rect> {
        self.fullscreen.split_rect_for_group(group)
    }

    fn refit_fullscreen(&mut self) {
        self.needs_fullscreen_refit = true;
    }

    fn agent_inset(&self, id: &str) -> PaneInset {
        let side = self.open_agent_side(id);
        let inset = agent_view::agent_inset(side, self.agent_dims());
        PaneInset {
            left: inset.left,
            right: inset.right,
            top: inset.top,
            bottom: inset.bottom,
        }
    }

    fn agent_tile_metrics(&self, id: &str) -> Option<TileMetrics> {
        let side = self.open_agent_side(id)?;
        let body = self.canvas_body_rect_of(id)?;
        let m = agent_view::footprint_tile_metrics(body, Some(side), self.agent_dims());
        Some(TileMetrics {
            w: m.w,
            h: m.h,
            shift_x: m.shift_x,
            shift_y: m.shift_y,
            tile_box: m.tile_box,
        })
    }

    fn canvas_window_rect(&self, id: &str, zoom: f64) -> Option<Rect> {
        let body = self.canvas_body_rect_of(id)?;
        let bar = self.names.bar_height(id, zoom);
        Some(Rect {
            max_y: body.max_y + bar,
            ..body
        })
    }

    fn canvas_footprint_rect(&self, id: &str, zoom: f64) -> Option<Rect> {
        let body = self.canvas_body_rect_of(id)?;
        let bar = self.names.bar_height(id, zoom);
        let win = Rect {
            max_y: body.max_y + bar,
            ..body
        };
        if let Some(side) = self.open_agent_side(id) {
            let ag = agent_view::agent_view_rect(body, bar, side, self.agent_dims());
            union_rect(&[win, ag])
        } else {
            Some(win)
        }
    }

    fn group_header_height(&self, _gid: &str, zoom: f64) -> f64 {
        names::bar_world_height(zoom, BAR_ROOM)
    }
}

struct FullscreenBridge<'a> {
    model: &'a mut WallModel,
    groups: &'a mut Groups,
    names: &'a mut WallNames,
    platform: Platform,
    now_ms: f64,
    quench_wheel: &'a mut bool,
    cancel_scroll: &'a mut bool,
    relayout_gids: Vec<String>,
}

impl FullscreenHost for FullscreenBridge<'_> {
    fn platform(&self) -> Platform {
        self.platform
    }

    fn config(&self) -> FullscreenConfig {
        FullscreenConfig::from(self.model.config())
    }

    fn dock_state(&self) -> DockState {
        dock_state_from_store(self.model.store())
    }

    fn viewport_size(&self) -> Size {
        let (w, h) = self.model.camera().view();
        Size::new(w, h)
    }

    fn camera_pose(&self) -> CameraPose {
        self.model.camera().pose()
    }

    fn ease_camera(&mut self, pose: CameraPose) {
        self.model.camera_mut().ease_to(pose);
        self.model.notify_changed();
    }

    fn quench_wheel(&mut self) {
        *self.quench_wheel = true;
    }

    fn terminal(&self, id: &str) -> Option<FullscreenTerminal> {
        let entry = self.model.terminal(id)?;
        let geom = self.model.geometry();
        Some(FullscreenTerminal {
            cols: entry.cols,
            rows: entry.rows,
            position: entry.position,
            cell_stride: Point::new(geom.stride.x * entry.scale, geom.stride.y * entry.scale),
        })
    }

    fn is_shown(&self, id: &str) -> bool {
        self.model.is_shown(id)
    }

    fn terminal_rect(&self, id: &str) -> Option<Rect> {
        self.model.terminal_rect(id)
    }

    fn window_rect(&self, id: &str, zoom: f64) -> Option<Rect> {
        let body = self.model.terminal_rect(id)?;
        let bar = self.names.bar_height(id, zoom);
        Some(Rect {
            max_y: body.max_y + bar,
            ..body
        })
    }

    fn bar_height(&self, id: &str, zoom: f64) -> Option<f64> {
        Some(self.names.bar_height(id, zoom))
    }

    fn set_bar_room(&mut self, id: &str, room: f64) {
        self.names.set_bar_room(id, Some(room));
        self.model.set_bar_room_override(id, Some(room));
    }

    fn resize_terminal(&mut self, id: &str, cols: u32, rows: u32) {
        self.model.push_effect(WallEffect::ResizeTerminal {
            id: id.to_owned(),
            cols,
            rows,
        });
    }

    fn set_terminal_position(&mut self, id: &str, pos: Point) {
        self.model.set_terminal_position(id, pos);
    }

    fn focused_id(&self) -> Option<&str> {
        self.model.focused_id()
    }

    fn focus_history(&self) -> &[String] {
        self.model.focus_history()
    }

    fn focus(&mut self, id: &str) {
        if self.model.focused_id() != Some(id) {
            *self.cancel_scroll = true;
        }
        self.model.set_focused_raw(Some(id), true);
    }

    fn terminal_name(&self, id: &str) -> String {
        self.model.name_of(id)
    }

    fn group_name(&self, gid: &str) -> String {
        self.groups
            .name_of(gid)
            .unwrap_or_else(|_| gid.to_owned())
    }

    fn notify(&mut self, text: String) {
        self.model.notify(text, self.now_ms);
    }

    fn apply_canvas(&mut self, elevated_ids: &[String]) {
        let depths = self.model.apply_z_order_with(elevated_ids);
        self.groups.set_depths(depths);
    }

    fn relayout_group(&mut self, gid: &str) {
        self.relayout_gids.push(gid.to_owned());
    }

    fn emit(&mut self) {
        self.model.notify_changed();
    }

    fn group(&self, gid: &str) -> Option<&WallGroup> {
        self.model.store().group(gid)
    }

    fn group_of(&self, id: &str) -> Option<&str> {
        self.model.store().group_of(id)
    }

    fn saved_fullscreen(&self) -> Option<&FullscreenState> {
        self.model.store().fullscreen()
    }

    fn set_saved_fullscreen(&mut self, state: Option<FullscreenState>) {
        self.model.store_mut().set_fullscreen(state);
    }

    fn fullscreen_text(&self, key: &str) -> Option<f64> {
        self.model.store().fullscreen_text(key)
    }

    fn set_fullscreen_text(&mut self, key: &str, px: Option<f64>) {
        self.model.store_mut().set_fullscreen_text(key, px);
    }
}

/// Read-only `FullscreenHost` view used for pure queries (`elevated_ids`, `has`, `group_layout`).
struct ReadFullscreenView<'a> {
    model: &'a WallModel,
    groups: &'a Groups,
    names: &'a WallNames,
    platform: Platform,
}

impl FullscreenHost for ReadFullscreenView<'_> {
    fn platform(&self) -> Platform {
        self.platform
    }

    fn config(&self) -> FullscreenConfig {
        FullscreenConfig::from(self.model.config())
    }

    fn dock_state(&self) -> DockState {
        dock_state_from_store(self.model.store())
    }

    fn viewport_size(&self) -> Size {
        let (w, h) = self.model.camera().view();
        Size::new(w, h)
    }

    fn camera_pose(&self) -> CameraPose {
        self.model.camera().pose()
    }

    fn ease_camera(&mut self, _pose: CameraPose) {}

    fn terminal(&self, id: &str) -> Option<FullscreenTerminal> {
        let entry = self.model.terminal(id)?;
        let geom = self.model.geometry();
        Some(FullscreenTerminal {
            cols: entry.cols,
            rows: entry.rows,
            position: entry.position,
            cell_stride: Point::new(geom.stride.x * entry.scale, geom.stride.y * entry.scale),
        })
    }

    fn is_shown(&self, id: &str) -> bool {
        self.model.is_shown(id)
    }

    fn terminal_rect(&self, id: &str) -> Option<Rect> {
        self.model.terminal_rect(id)
    }

    fn window_rect(&self, id: &str, zoom: f64) -> Option<Rect> {
        let body = self.model.terminal_rect(id)?;
        let bar = self.names.bar_height(id, zoom);
        Some(Rect {
            max_y: body.max_y + bar,
            ..body
        })
    }

    fn bar_height(&self, id: &str, zoom: f64) -> Option<f64> {
        Some(self.names.bar_height(id, zoom))
    }

    fn resize_terminal(&mut self, _id: &str, _cols: u32, _rows: u32) {}
    fn set_terminal_position(&mut self, _id: &str, _pos: Point) {}

    fn focused_id(&self) -> Option<&str> {
        self.model.focused_id()
    }

    fn focus_history(&self) -> &[String] {
        self.model.focus_history()
    }

    fn focus(&mut self, _id: &str) {}

    fn terminal_name(&self, id: &str) -> String {
        self.model.name_of(id)
    }

    fn group_name(&self, gid: &str) -> String {
        self.groups
            .name_of(gid)
            .unwrap_or_else(|_| gid.to_owned())
    }

    fn notify(&mut self, _text: String) {}

    fn group(&self, gid: &str) -> Option<&WallGroup> {
        self.model.store().group(gid)
    }

    fn group_of(&self, id: &str) -> Option<&str> {
        self.model.store().group_of(id)
    }

    fn saved_fullscreen(&self) -> Option<&FullscreenState> {
        self.model.store().fullscreen()
    }

    fn set_saved_fullscreen(&mut self, _state: Option<FullscreenState>) {}

    fn fullscreen_text(&self, key: &str) -> Option<f64> {
        self.model.store().fullscreen_text(key)
    }

    fn set_fullscreen_text(&mut self, _key: &str, _px: Option<f64>) {}
}

/// Read-only `WallHost` view used for pure group queries (`frame_rect`, `header_rect`, `hit`).
struct ReadGroupsView<'a> {
    model: &'a WallModel,
    fullscreen: &'a WallFullscreen,
    names: &'a WallNames,
}

impl ReadGroupsView<'_> {
    fn agent_dims(&self) -> AgentViewDims {
        let cfg = self.model.config();
        AgentViewDims::new(
            cfg.agent_view_width,
            cfg.agent_view_height,
            cfg.agent_view_gap,
        )
    }

    fn open_agent_side(&self, id: &str) -> Option<AgentSide> {
        self.model
            .store()
            .agent_view(id)
            .filter(|av| av.open)
            .map(|av| av.side)
    }

    fn canvas_body_rect_of(&self, id: &str) -> Option<Rect> {
        if self.fullscreen.has_saved(id) {
            if let Some(sv) = self
                .fullscreen
                .state()
                .and_then(|s| s.saved.as_ref().and_then(|m| m.get(id)).copied())
                .or_else(|| {
                    self.fullscreen
                        .state()
                        .filter(|s| s.id.as_deref() == Some(id))
                        .and_then(|s| s.canvas)
                })
            {
                let geom = self.model.geometry();
                let scale = self.model.terminal(id).map_or(1.0, |t| t.scale);
                let width =
                    (f64::from(sv.cols) * geom.stride.x + 2.0 * geom.padding.x) * scale;
                let height =
                    (f64::from(sv.rows) * geom.stride.y + 2.0 * geom.padding.y) * scale;
                return Some(Rect::from_top_left(sv.x, sv.y, width, height));
            }
        }
        self.model.terminal_rect(id)
    }
}

impl WallHost for ReadGroupsView<'_> {
    fn group_config(&self) -> GroupConfig {
        GroupConfig::from(self.model.config())
    }

    fn terminal(&self, id: &str) -> Option<TerminalGeometry> {
        let entry = self.model.terminal(id)?;
        let geom = self.model.geometry();
        Some(TerminalGeometry {
            position: entry.position,
            rect: entry.body_rect(geom),
            cols: entry.cols,
            rows: entry.rows,
            cell_stride: Point::new(geom.stride.x * entry.scale, geom.stride.y * entry.scale),
        })
    }

    fn terminal_ids(&self) -> Vec<String> {
        self.model.order().to_vec()
    }

    fn is_shown(&self, id: &str) -> bool {
        self.model.is_shown(id)
    }

    fn on_canvas(&self, id: &str) -> bool {
        self.model.is_shown(id) && !self.fullscreen.active()
    }

    fn place_body(&mut self, _id: &str, _pos: Point) {}

    fn fullscreen_terminal_id(&self) -> Option<&str> {
        self.fullscreen.solo_id()
    }

    fn fullscreen_group_id(&self) -> Option<&str> {
        self.fullscreen.group_id()
    }

    fn fullscreen_split_rect(&self, group: &Group) -> Option<Rect> {
        self.fullscreen.split_rect_for_group(group)
    }

    fn agent_inset(&self, id: &str) -> PaneInset {
        let side = self.open_agent_side(id);
        let inset = agent_view::agent_inset(side, self.agent_dims());
        PaneInset {
            left: inset.left,
            right: inset.right,
            top: inset.top,
            bottom: inset.bottom,
        }
    }

    fn agent_tile_metrics(&self, id: &str) -> Option<TileMetrics> {
        let side = self.open_agent_side(id)?;
        let body = self.canvas_body_rect_of(id)?;
        let m = agent_view::footprint_tile_metrics(body, Some(side), self.agent_dims());
        Some(TileMetrics {
            w: m.w,
            h: m.h,
            shift_x: m.shift_x,
            shift_y: m.shift_y,
            tile_box: m.tile_box,
        })
    }

    fn canvas_window_rect(&self, id: &str, zoom: f64) -> Option<Rect> {
        let body = self.canvas_body_rect_of(id)?;
        let bar = self.names.bar_height(id, zoom);
        Some(Rect {
            max_y: body.max_y + bar,
            ..body
        })
    }

    fn canvas_footprint_rect(&self, id: &str, zoom: f64) -> Option<Rect> {
        let body = self.canvas_body_rect_of(id)?;
        let bar = self.names.bar_height(id, zoom);
        let win = Rect {
            max_y: body.max_y + bar,
            ..body
        };
        if let Some(side) = self.open_agent_side(id) {
            let ag = agent_view::agent_view_rect(body, bar, side, self.agent_dims());
            union_rect(&[win, ag])
        } else {
            Some(win)
        }
    }

    fn group_header_height(&self, _gid: &str, zoom: f64) -> f64 {
        names::bar_world_height(zoom, BAR_ROOM)
    }
}

/// Split-borrow adapter wiring [`crate::agent_views::AgentViews::footprint_changed`]
/// against `WallCore` (`TerminalWall.footprintChanged` / `_giveRoomBack`).
struct AgentFootprintBridge<'a> {
    model: &'a mut WallModel,
    groups: &'a mut Groups,
    fullscreen: &'a WallFullscreen,
    names: &'a mut WallNames,
    needs_fullscreen_refit: bool,
}

impl crate::agent_views::AgentViewsHost for AgentFootprintBridge<'_> {
    fn agent_views_config(&self) -> crate::agent_views::AgentViewsConfig {
        let cfg = self.model.config();
        crate::agent_views::AgentViewsConfig {
            default_side: cfg.agent_view_side,
            dims: AgentViewDims::new(
                cfg.agent_view_width,
                cfg.agent_view_height,
                cfg.agent_view_gap,
            ),
            group_gap: cfg.group_gap,
            snap: cfg.snap,
            grid: GridSpec::new(
                cfg.grid_minor,
                cfg.grid_major_every.max(1.0) as u32,
                cfg.grid_min_px,
            ),
        }
    }

    fn terminal(&self, id: &str) -> Option<crate::agent_views::AgentTerminalSnapshot> {
        let entry = self.model.terminal(id)?;
        let rgv = ReadGroupsView {
            model: self.model,
            fullscreen: self.fullscreen,
            names: self.names,
        };
        let body_rect = rgv.canvas_body_rect_of(id)?;
        Some(crate::agent_views::AgentTerminalSnapshot {
            body_rect,
            position: entry.position,
            scrolled_rows: 0,
            cursor_row: 0,
            alt_screen: entry.modes.alt_screen,
            window_title: entry.shell_title.clone(),
            on_canvas: self.model.is_shown(id) && !self.fullscreen.active(),
        })
    }

    fn bar_height(&self, id: &str, zoom: f64) -> f64 {
        self.names.bar_height(id, zoom)
    }

    fn terminal_name(&self, id: &str) -> String {
        self.model.name_of(id)
    }

    fn store_name(&self, id: &str) -> Option<String> {
        self.model.store().name(id).map(str::to_owned)
    }

    fn focused_id(&self) -> Option<&str> {
        self.model.focused_id()
    }

    fn fullscreen_active(&self) -> bool {
        self.fullscreen.active()
    }

    fn zoom(&self) -> f64 {
        self.model.camera().zoom()
    }
}

impl crate::agent_views::FootprintCanvas for AgentFootprintBridge<'_> {
    fn terminal_order(&self) -> Vec<String> {
        self.model.order().to_vec()
    }

    fn group_of(&self, id: &str) -> Option<String> {
        self.groups.group_of(id)
    }

    fn group_stand(&self, gid: &str) -> Option<crate::agent_views::GroupStand> {
        let g = self.groups.group(gid)?;
        Some(crate::agent_views::GroupStand {
            layout: g.layout,
            origin: g.origin,
            members: g.members.clone(),
        })
    }

    fn relayout_group(&mut self, _views: &crate::agent_views::AgentViews, gid: &str) {
        let mut gb = GroupsBridge {
            model: self.model,
            fullscreen: self.fullscreen,
            names: self.names,
            needs_fullscreen_refit: false,
        };
        let _ = self.groups.relayout(&mut gb, gid);
        self.needs_fullscreen_refit |= gb.needs_fullscreen_refit;
    }

    fn group_box_rect(
        &self,
        _views: &crate::agent_views::AgentViews,
        gid: &str,
        zoom: f64,
        bare: Option<&str>,
    ) -> Option<Rect> {
        let rgv = ReadGroupsView {
            model: self.model,
            fullscreen: self.fullscreen,
            names: self.names,
        };
        self.groups.box_rect(&rgv, gid, zoom, bare).ok().flatten()
    }

    fn move_group(&mut self, gid: &str, dx: f64, dy: f64) {
        let mut gb = GroupsBridge {
            model: self.model,
            fullscreen: self.fullscreen,
            names: self.names,
            needs_fullscreen_refit: false,
        };
        let _ = self.groups.move_group(&mut gb, gid, dx, dy);
        self.needs_fullscreen_refit |= gb.needs_fullscreen_refit;
    }

    fn move_terminal(&mut self, id: &str, dx: f64, dy: f64) {
        if let Some(pos) = self
            .model
            .terminal(id)
            .map(|t| Point::new(t.position.x + dx, t.position.y + dy))
        {
            self.model.set_terminal_position(id, pos);
            self.model.remember_position(id);
        }
    }
}

// ── WallCore orchestration helpers ───────────────────────────────────────────

impl WallCore {
    fn new(store: WallStore, opts: WallOptions) -> Self {
        let initial_groups: HashMap<String, Group> = store
            .groups()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let camera = WallCamera::new(opts.initial_camera, opts.viewport.w, opts.viewport.h);
        let model = WallModel::new(store, camera, opts.born_config, opts.geometry);
        let groups = Groups::from_map(initial_groups);
        Self {
            model,
            groups,
            fullscreen: WallFullscreen::new(),
            names: WallNames::new(None),
            selections: HashMap::new(),
            selection_text_cache: HashMap::new(),
            last_screen_titles: HashMap::new(),
            agent_stick_bottom: HashMap::new(),
            preview_scratch: String::new(),
            pressed_keys: PressedKeys::new(),
            termd: TermdStatus::new(opts.namespace),
            termd_now_ms: None,
            termd_epoch_anchor: None,
            bus: None,
            bus_reason: "no gomuxai snapshot yet".to_owned(),
            agents_extra_connected: false,
            platform: opts.platform,
            char_widths: opts.char_widths.sanitized(),
            scale_factor: opts.scale_factor.max(0.1),
            now_ms: 0.0,
            base_cursor: CursorIcon::Default,
            cursor: CursorIcon::Default,
            hovered_group_id: None,
            hovered_label_id: None,
            hovered_link: None,
            pending_link_click: None,
            last_pointer_screen: None,
            last_pointer_mods: Mods::default(),
            last_map_labels: Vec::new(),
            lasso_screen: None,
            active_group_drag: None,
            pending_spawns: VecDeque::new(),
            close_hold: None,
            palette_open: false,
            field_focus: FieldFocus::None,
            pending_quench_wheel: false,
            pending_cancel_scroll: false,
            closing_group: None,
        }
    }

    fn read_fs_view(&self) -> ReadFullscreenView<'_> {
        ReadFullscreenView {
            model: &self.model,
            groups: &self.groups,
            names: &self.names,
            platform: self.platform,
        }
    }

    fn read_groups_view(&self) -> ReadGroupsView<'_> {
        ReadGroupsView {
            model: &self.model,
            fullscreen: &self.fullscreen,
            names: &self.names,
        }
    }

    fn with_groups<R>(&mut self, f: impl FnOnce(&mut Groups, &mut GroupsBridge<'_>) -> R) -> R {
        let mut bridge = GroupsBridge {
            model: &mut self.model,
            fullscreen: &self.fullscreen,
            names: &mut self.names,
            needs_fullscreen_refit: false,
        };
        let out = f(&mut self.groups, &mut bridge);
        let refit = bridge.needs_fullscreen_refit;
        self.sync_groups_to_store();
        if refit {
            self.flush_cross_effects(true, Vec::new());
        }
        out
    }

    fn with_fullscreen<R>(
        &mut self,
        f: impl FnOnce(&mut WallFullscreen, &mut FullscreenBridge<'_>) -> R,
    ) -> R {
        let mut bridge = FullscreenBridge {
            model: &mut self.model,
            groups: &mut self.groups,
            names: &mut self.names,
            platform: self.platform,
            now_ms: self.now_ms,
            quench_wheel: &mut self.pending_quench_wheel,
            cancel_scroll: &mut self.pending_cancel_scroll,
            relayout_gids: Vec::new(),
        };
        let out = f(&mut self.fullscreen, &mut bridge);
        let relayout_gids = bridge.relayout_gids;
        if !relayout_gids.is_empty() {
            self.flush_cross_effects(false, relayout_gids);
        }
        out
    }

    /// Drain deferred cross-subsystem follow-ups (`Fullscreen::refit` requested by `Groups`
    /// and `Groups::relayout` requested by `Fullscreen`) without generic closure recursion.
    fn flush_cross_effects(&mut self, mut refit_fs: bool, mut relayout_gids: Vec<String>) {
        // Bounded to 4 iterations so a degenerate layout never oscillates infinitely.
        for _ in 0..4 {
            if !refit_fs && relayout_gids.is_empty() {
                break;
            }
            let next_gids = std::mem::take(&mut relayout_gids);
            let mut next_refit = false;
            for gid in next_gids {
                let mut bridge = GroupsBridge {
                    model: &mut self.model,
                    fullscreen: &self.fullscreen,
                    names: &mut self.names,
                    needs_fullscreen_refit: false,
                };
                let _ = self.groups.relayout(&mut bridge, &gid);
                next_refit |= bridge.needs_fullscreen_refit;
                self.sync_groups_to_store();
            }
            if refit_fs {
                let mut bridge = FullscreenBridge {
                    model: &mut self.model,
                    groups: &mut self.groups,
                    names: &mut self.names,
                    platform: self.platform,
                    now_ms: self.now_ms,
                    quench_wheel: &mut self.pending_quench_wheel,
                    cancel_scroll: &mut self.pending_cancel_scroll,
                    relayout_gids: Vec::new(),
                };
                self.fullscreen.refit(&mut bridge);
                relayout_gids.extend(bridge.relayout_gids);
            }
            refit_fs = next_refit;
        }
    }

    fn sync_groups_to_store(&mut self) {
        let live_gids: HashSet<String> = self.groups.as_map().keys().cloned().collect();
        let stale_gids: Vec<String> = self
            .model
            .store()
            .groups()
            .keys()
            .filter(|gid| !live_gids.contains(*gid))
            .cloned()
            .collect();
        for gid in stale_gids {
            self.model.store_mut().delete_group(&gid);
        }
        if let Some(ref gid) = self.hovered_group_id {
            if !live_gids.contains(gid) {
                self.hovered_group_id = None;
            }
        }
        let updates: Vec<(String, WallGroup)> = self
            .groups
            .as_map()
            .iter()
            .filter(|(gid, grp)| self.model.store().group(gid) != Some(*grp))
            .map(|(gid, grp)| (gid.clone(), grp.clone()))
            .collect();
        for (gid, grp) in updates {
            self.model.store_mut().set_group(&gid, grp);
        }
    }

    fn apply_z_order(&mut self) {
        let elevated = self.fullscreen.elevated_ids(&self.read_fs_view());
        let depths = self.model.apply_z_order_with(&elevated);
        self.groups.set_depths(depths);
    }

    fn grid_spec(&self) -> GridSpec {
        let cfg = self.model.config();
        GridSpec::new(
            cfg.grid_minor,
            cfg.grid_major_every.max(1.0) as u32,
            cfg.grid_min_px,
        )
    }

    fn pointer_config(&self) -> PointerConfig {
        let cfg = self.model.config();
        PointerConfig {
            platform: self.platform,
            snap: cfg.snap,
            grid: self.grid_spec(),
            click_repeat_ms: cfg.click_repeat_ms,
            zoom_keys: ZoomKeys::parse(&cfg.zoom_keys).unwrap_or(ZoomKeys::DEFAULT),
            scroll_multiplier: cfg.scroll_multiplier,
            glide_scroll: true,
        }
    }

    fn canvas_order(&self) -> Vec<String> {
        if self.fullscreen.active() {
            Vec::new()
        } else {
            self.model.shown_order()
        }
    }

    fn bar_height(&self, id: &str, zoom: f64) -> f64 {
        self.names.bar_height(id, zoom)
    }

    fn window_rect_at_zoom(&self, id: &str, zoom: f64) -> Option<Rect> {
        let body = self.model.terminal_rect(id)?;
        let bar = self.bar_height(id, zoom);
        Some(Rect {
            max_y: body.max_y + bar,
            ..body
        })
    }

    fn window_rect(&self, id: &str) -> Option<Rect> {
        self.window_rect_at_zoom(id, self.model.camera().zoom())
    }

    fn canvas_window_rect(&self, id: &str, zoom: f64) -> Option<Rect> {
        self.read_groups_view().canvas_window_rect(id, zoom)
    }

    fn footprint_rect(&self, id: &str, zoom: f64) -> Option<Rect> {
        let body = self.model.terminal_rect(id)?;
        let bar = self.bar_height(id, zoom);
        let win = Rect {
            max_y: body.max_y + bar,
            ..body
        };
        if let Some(av) = self.model.store().agent_view(id).filter(|a| a.open) {
            let cfg = self.model.config();
            let dims = AgentViewDims::new(
                cfg.agent_view_width,
                cfg.agent_view_height,
                cfg.agent_view_gap,
            );
            let ag = agent_view::agent_view_rect(body, bar, av.side, dims);
            union_rect(&[win, ag])
        } else {
            Some(win)
        }
    }

    fn title_parts(&self, id: &str) -> (String, Option<String>, Option<AgentStatus>, String) {
        let name = self.model.name_of(id);
        let attached_agent = self
            .model
            .store()
            .attached(id)
            .map(str::to_owned)
            .or_else(|| id.strip_prefix("gx-").filter(|s| !s.is_empty()).map(str::to_owned));
        let agent_status = attached_agent
            .as_deref()
            .and_then(|target| find_bus_agent(self.bus.as_ref(), target))
            .map(|a| AgentStatus::parse(a.status.as_deref().unwrap_or("")));
        let text = names::bar_title(&name, agent_status.map(AgentStatus::word));
        (name, attached_agent, agent_status, text)
    }

    fn title_of(&self, id: &str) -> String {
        self.title_parts(id).3
    }

    fn set_hovered_group(&mut self, group_id: Option<&str>) {
        if self.hovered_group_id.as_deref() != group_id {
            self.hovered_group_id = group_id.map(str::to_owned);
            self.model.notify_changed();
        }
    }

    fn set_hovered_label(&mut self, label_id: Option<&str>) {
        if self.hovered_label_id.as_deref() != label_id {
            self.hovered_label_id = label_id.map(str::to_owned);
            self.model.notify_changed();
        }
    }

    fn hit_map_label(&self, screen_x: f64, screen_y: f64) -> Option<String> {
        if self.fullscreen.active() {
            return None;
        }
        self.last_map_labels
            .iter()
            .rev()
            .find(|(_, b)| {
                screen_x >= b.left
                    && screen_x <= b.right
                    && screen_y >= b.top
                    && screen_y <= b.bottom
            })
            .map(|(id, _)| id.clone())
    }

    fn record_termd_now_ms(&mut self, now_ms: u64) {
        self.termd_now_ms = Some(self.termd_now_ms.map_or(now_ms, |prev| prev.max(now_ms)));
        self.termd_epoch_anchor = None;
    }

    fn effective_termd_now_ms(&self) -> u64 {
        let wall_ms = self.now_ms.max(0.0) as u64;
        let since_ms = self.termd.since_ms();
        match self.termd_now_ms {
            Some(t) => t.max(since_ms).max(if t > wall_ms.saturating_add(60_000) {
                t
            } else {
                wall_ms
            }),
            None => wall_ms.max(since_ms),
        }
    }

    fn status_notice(&self) -> Option<StatusNotice> {
        let restart_notice_ms = self.model.config().restart_notice_ms.max(0.0).round() as u64;
        let now_ms = self.effective_termd_now_ms();
        status::notice_for(
            &self.termd,
            self.termd.since_ms(),
            now_ms,
            restart_notice_ms,
        )
    }

    fn advance_termd_clock(&mut self, dt_secs: f64, now_ms: f64) {
        let prev_notice = self.status_notice();
        let wall_ms = now_ms.max(0.0) as u64;
        if let Some(t) = self.termd_now_ms {
            if t > wall_ms.saturating_add(60_000) {
                let advanced = match self.termd_epoch_anchor {
                    Some((base_epoch, base_wall)) => {
                        let elapsed = (now_ms - base_wall).max(0.0).floor() as u64;
                        base_epoch.saturating_add(elapsed)
                    }
                    None => {
                        let first_delta = (dt_secs.max(0.0) * 1000.0).round() as u64;
                        let base_epoch = t.saturating_add(first_delta);
                        self.termd_epoch_anchor = Some((base_epoch, now_ms));
                        base_epoch
                    }
                };
                self.termd_now_ms = Some(advanced.max(t));
            } else {
                self.termd_now_ms = Some(wall_ms.max(t));
                self.termd_epoch_anchor = None;
            }
        }
        let effective = self.effective_termd_now_ms();
        let escalated = self.termd.tick_grace(effective, DEFAULT_RESTART_GRACE_MS);
        let next_notice = self.status_notice();
        if escalated || next_notice != prev_notice {
            self.model.notify_changed();
        }
    }

    fn cell_at(&self, id: &str, screen_x: f64, screen_y: f64) -> Option<CellPoint> {
        let entry = self.model.terminal(id)?;
        let geom = self.model.geometry();
        let world = self.model.camera().to_world(Point::new(screen_x, screen_y));
        let origin = entry.cell_origin(geom);
        let scale = entry.scale.max(1e-9);
        // `cell_at_point` places row 0's vertical centre at local_y = 0, whereas
        // `cell_origin` sits at row 0's top edge (+stride.y * 0.5 above centre).
        let local_x = (world.x - origin.x) / scale;
        let local_y = (world.y - origin.y) / scale + geom.stride.y * 0.5;
        Some(cell_at_point(
            entry.cols,
            entry.rows,
            geom.stride,
            local_x,
            local_y,
        ))
    }

    fn hit(&self, point: Point) -> Option<CanvasHit> {
        let zoom = self.model.camera().zoom().max(1e-9);
        let tol = EDGE_GRAB_PX / zoom;

        if self.fullscreen.active() {
            if self.fullscreen.mode() == Some(FullscreenMode::Single) {
                let id = self.fullscreen.id()?;
                let body = self.model.terminal_rect(id)?;
                return body.contains(point).then(|| CanvasHit::Terminal {
                    id: id.to_owned(),
                    part: HitPart::Body,
                });
            }
            let gid = self.fullscreen.group_id()?.to_owned();
            let layout = self.fullscreen.group_layout(&self.read_fs_view());
            if layout == Some(GroupLayout::Split) {
                if let Some(div) =
                    self.groups
                        .group_divider_at(&self.read_groups_view(), &gid, point, tol)
                {
                    return Some(CanvasHit::Divider {
                        group: gid,
                        path: div.path,
                        axis: div.axis,
                    });
                }
            }
            let fs_view = self.read_fs_view();
            for id in self.model.z_order().iter().rev() {
                if !self.model.is_shown(id) || !self.fullscreen.has(&fs_view, id) {
                    continue;
                }
                if layout == Some(GroupLayout::Freeform) {
                    if let (Some(win), Some(body)) =
                        (self.window_rect_at_zoom(id, zoom), self.model.terminal_rect(id))
                    {
                        let target = [HitTarget::new(id.clone(), win, body)];
                        if let Some(hit) = hit_test(&target, point.x, point.y, tol) {
                            return Some(CanvasHit::from(hit));
                        }
                    }
                } else if let (Some(win), Some(body)) =
                    (self.window_rect_at_zoom(id, zoom), self.model.terminal_rect(id))
                {
                    if win.contains(point) {
                        let part = if point.y > body.max_y {
                            HitPart::Title
                        } else {
                            HitPart::Body
                        };
                        return Some(CanvasHit::Terminal {
                            id: id.clone(),
                            part,
                        });
                    }
                }
            }
            return None;
        }

        let shown_front_to_back: Vec<String> = self
            .model
            .z_order()
            .iter()
            .rev()
            .filter(|id| self.model.is_shown(id))
            .cloned()
            .collect();

        let mut last_idx_of_group: HashMap<String, usize> = HashMap::new();
        for (idx, id) in shown_front_to_back.iter().enumerate() {
            if let Some(gid) = self.groups.group_of(id) {
                last_idx_of_group.insert(gid, idx);
            }
        }

        let groups_view = self.read_groups_view();
        let mut seen_splits: HashSet<String> = HashSet::new();
        let mut checked_headers: HashSet<String> = HashSet::new();

        for (idx, id) in shown_front_to_back.iter().enumerate() {
            let gid_opt = self.groups.group_of(id);
            if let Some(ref gid) = gid_opt {
                if seen_splits.insert(gid.clone()) {
                    if let Some(div) = self.groups.group_divider_at(&groups_view, gid, point, tol) {
                        return Some(CanvasHit::Divider {
                            group: gid.clone(),
                            path: div.path,
                            axis: div.axis,
                        });
                    }
                }
            }
            if let (Some(win), Some(body)) =
                (self.window_rect_at_zoom(id, zoom), self.model.terminal_rect(id))
            {
                let target = [HitTarget::new(id.clone(), win, body)];
                if let Some(hit) = hit_test(&target, point.x, point.y, tol) {
                    return Some(CanvasHit::from(hit));
                }
            }
            if let Some(gid) = gid_opt {
                if last_idx_of_group.get(&gid) == Some(&idx) {
                    checked_headers.insert(gid.clone());
                    if let Ok(Some(header)) = self.groups.header_rect(&groups_view, &gid, zoom, None)
                    {
                        if header.contains(point) {
                            return Some(CanvasHit::GroupHeader { group: gid });
                        }
                    }
                }
            }
        }

        if let Some(gid) = self.groups.header_at(&groups_view, point, zoom) {
            if !checked_headers.contains(&gid) {
                return Some(CanvasHit::GroupHeader { group: gid });
            }
        }
        None
    }

    fn focus(&mut self, id: Option<&str>, frame: bool) -> bool {
        let prev_focused = self.model.focused_id().map(str::to_owned);
        let Some(target_id) = id else {
            if prev_focused.is_some() {
                self.pending_cancel_scroll = true;
            }
            return self.model.set_focused_raw(None, false);
        };
        if self.model.terminal(target_id).is_none() {
            return false;
        }
        if prev_focused.as_deref() != Some(target_id) {
            self.pending_cancel_scroll = true;
        }
        if self.model.store().hidden(target_id) {
            self.show_terminal(target_id);
        }
        self.model.set_focused_raw(Some(target_id), true);
        self.apply_z_order();
        if self.fullscreen.active() {
            let owned = target_id.to_owned();
            self.with_fullscreen(|fs, host| fs.on_focus(host, &owned));
        } else if frame {
            self.frame_terminal(target_id);
        }
        self.model.notify_changed();
        true
    }

    /// The `(cols, rows)` every new terminal is asked for: the
    /// `newTerminalSize` knob (`wall.spawn`, the `+` button, `wall.split`,
    /// `wall.attach`, `terminal.spawn`).
    fn new_terminal_cells(&self) -> (u32, u32) {
        self.model.config().new_terminal_cells()
    }

    fn dock_edges(&self) -> crate::dock::EdgeWidths {
        dock_layout(&dock_state_from_store(self.model.store())).edges
    }

    /// Compute the [`CameraPose`] that fits `rect` inside the viewport area left
    /// clear of any docked left/right panels (`dockLayout.edges`), shifting the
    /// camera horizontally so `rect` centres in `[edges.left, view_w - edges.right]`.
    fn fit_target_in_dock(&self, rect: Rect, margin: f64, max_zoom: f64) -> CameraPose {
        let (vw, vh) = self.model.camera().view();
        let edges = self.dock_edges();
        let avail_w = (vw - edges.left - edges.right).max(1.0);
        let mut pose = crate::layout::fit_rect(rect, avail_w, vh, margin, max_zoom);
        let area_cx = (edges.left + avail_w * 0.5).clamp(0.0, vw);
        if pose.zoom > 0.0 {
            pose.x += (vw * 0.5 - area_cx) / pose.zoom;
        }
        pose
    }

    fn focus_in_view(&mut self, id: &str) {
        if self.fullscreen.active() {
            self.focus(Some(id), true);
            return;
        }
        let needs_frame = match self.window_rect(id) {
            Some(win) => {
                let sbox = self.model.camera().to_screen_box(win);
                let (vw, vh) = self.model.camera().view();
                let edges = self.dock_edges();
                sbox.left < edges.left
                    || sbox.top < 0.0
                    || sbox.right > vw - edges.right
                    || sbox.bottom > vh
            }
            None => false,
        };
        self.focus(Some(id), needs_frame);
    }

    fn frame_target(&self, id: &str) -> Option<CameraPose> {
        let entry = self.model.terminal(id)?;
        let geom = self.model.geometry();
        let cap = FRAME_MAX_LINE_PX / (geom.stride.y * entry.scale);
        let body = entry.body_rect(geom);
        let initial_rect = self
            .footprint_rect(id, self.model.camera().zoom())
            .unwrap_or(body);
        let first = self.fit_target_in_dock(initial_rect, DEFAULT_FIT_MARGIN, cap);
        let second_rect = self.footprint_rect(id, first.zoom).unwrap_or(body);
        Some(self.fit_target_in_dock(second_rect, DEFAULT_FIT_MARGIN, cap))
    }

    fn frame_terminal(&mut self, id: &str) -> bool {
        let Some(target) = self.frame_target(id) else {
            return false;
        };
        self.pending_quench_wheel = true;
        self.model.camera_mut().ease_to(target);
        self.model.notify_changed();
        true
    }

    fn fit_all(&mut self) -> bool {
        if self.fullscreen.active() {
            self.with_fullscreen(|fs, host| fs.exit(host));
        }
        let shown = self.model.shown_order();
        let zoom = self.model.camera().zoom();
        let first_rects: Vec<Rect> = shown
            .iter()
            .filter_map(|id| self.footprint_rect(id, zoom))
            .collect();
        let Some(all) = union_rect(&first_rects) else {
            return false;
        };
        self.pending_quench_wheel = true;
        let first = self.fit_target_in_dock(all, FIT_ALL_MARGIN, f64::INFINITY);
        let second_rects: Vec<Rect> = shown
            .iter()
            .filter_map(|id| self.footprint_rect(id, first.zoom))
            .collect();
        let target = union_rect(&second_rects)
            .map(|r| self.fit_target_in_dock(r, FIT_ALL_MARGIN, f64::INFINITY))
            .unwrap_or(first);
        self.model.camera_mut().ease_to(target);
        self.model.notify_changed();
        true
    }

    fn frame_group(&mut self, gid: &str) -> Result<bool, String> {
        self.groups.get(gid).map_err(|e| e.to_string())?;
        if self.fullscreen.active() {
            self.with_fullscreen(|fs, host| fs.exit(host));
        }
        let zoom = self.model.camera().zoom();
        let at = {
            let view = self.read_groups_view();
            self.groups
                .box_rect(&view, gid, zoom, None)
                .map_err(|e| e.to_string())?
        };
        let Some(first_box) = at else {
            return Ok(false);
        };
        self.pending_quench_wheel = true;
        let first = self.fit_target_in_dock(first_box, FIT_ALL_MARGIN, f64::INFINITY);
        let second_box = {
            let view = self.read_groups_view();
            self.groups
                .box_rect(&view, gid, first.zoom, None)
                .map_err(|e| e.to_string())?
        };
        let target = second_box
            .map(|b| self.fit_target_in_dock(b, FIT_ALL_MARGIN, f64::INFINITY))
            .unwrap_or(first);
        self.model.camera_mut().ease_to(target);
        self.model.notify_changed();
        Ok(true)
    }

    fn hide_terminal(&mut self, id: &str) -> bool {
        if self.model.terminal(id).is_none() || self.model.store().hidden(id) {
            return false;
        }
        let was_focused = self.model.focused_id() == Some(id);
        let prev_group = self.groups.group_of(id);
        let fs_has = self.fullscreen.has(&self.read_fs_view(), id);
        let was_fs_group = self.fullscreen.mode() == Some(FullscreenMode::Group) && fs_has;
        if fs_has {
            let owned = id.to_owned();
            self.with_fullscreen(|fs, host| fs.on_hide(host, &owned));
        }
        if self.groups.splits(id) {
            let owned = id.to_owned();
            let _ = self.with_groups(|g, host| g.remove(host, &[owned]));
        }
        self.model.store_mut().set_hidden(id, true);
        self.model.prune_focus_history_raw();
        if was_fs_group {
            self.with_fullscreen(|fs, host| fs.after_member_left(host));
        }
        if was_focused {
            self.restore_focus_after(id, prev_group.as_deref());
        }
        if self.model.hovered_id() == Some(id) {
            self.model.set_hovered(None);
        }
        if self.hovered_label_id.as_deref() == Some(id) {
            self.hovered_label_id = None;
        }
        self.model.deselect_one_raw(id);
        let owned = id.to_owned();
        let _ = self.with_groups(|g, host| g.relayout_of(host, &owned));
        self.apply_z_order();
        self.model.notify_changed();
        true
    }

    fn show_terminal(&mut self, id: &str) -> bool {
        if self.model.terminal(id).is_none() || !self.model.store().hidden(id) {
            return false;
        }
        self.model.store_mut().set_hidden(id, false);
        let owned = id.to_owned();
        let _ = self.with_groups(|g, host| g.relayout_of(host, &owned));
        self.apply_z_order();
        self.model.notify_changed();
        true
    }

    fn restore_focus_after(&mut self, gone_id: &str, preferred_group: Option<&str>) {
        if let Some(next_id) = self.model.pick_focus_fallback(gone_id, preferred_group) {
            self.focus(Some(&next_id), false);
        } else {
            self.model.clear_focused_raw();
        }
    }

    fn close_terminal(&mut self, id: &str) -> CommandReply {
        let gid = self.groups.group_of(id);
        self.closing_group = if self.model.focused_id() == Some(id) {
            gid.clone()
        } else {
            None
        };
        let existed = self.model.terminal(id).is_some();
        self.model.store_mut().forget(id);
        self.groups.forget_terminal(id);
        self.sync_groups_to_store();
        self.selections.remove(id);
        self.selection_text_cache.remove(id);
        self.last_screen_titles.remove(id);
        self.agent_stick_bottom.remove(id);
        if self.hovered_label_id.as_deref() == Some(id) {
            self.hovered_label_id = None;
        }
        self.model.deselect_one_raw(id);
        if self.fullscreen.active() {
            let owned = id.to_owned();
            self.with_fullscreen(|fs, host| fs.gone(host, &owned));
        }
        let (_, was_focused) = self.model.unmount_terminal_raw(id);
        self.names.prune(|tid| self.model.terminal(tid).is_some());
        if let Some(ref g) = gid {
            if self.groups.group(g).is_some() {
                let _ = self.with_groups(|grps, host| grps.relayout(host, g));
            }
        }
        if was_focused {
            let pref = self.closing_group.clone().or(gid);
            self.restore_focus_after(id, pref.as_deref());
        }
        self.closing_group = None;
        self.apply_z_order();
        self.model.push_effect(WallEffect::KillTerminal {
            id: id.to_owned(),
        });
        self.model.notify_changed();
        if existed {
            CommandReply::ok(format!("closed {id}"), Some(json!({ "id": id })))
        } else {
            CommandReply::ok(format!("forgotten {id}"), Some(json!({ "id": id })))
        }
    }

    fn terminal_appeared(&mut self, info: TerminalAppeared) -> LandedAs {
        let id = info.id;
        let pending = if info.adopted {
            None
        } else {
            self.pending_spawns.pop_front()
        };
        let track_flow = pending
            .as_ref()
            .is_none_or(|p| p.split.is_none() && p.window_top_left.is_none())
            && !info.adopted;
        let canvas_cam = self.fullscreen.state().map(|s| s.camera);
        let cur_zoom = self.model.camera().zoom().max(1e-9);
        let taken_rects: Vec<Rect> = {
            let rgv = self.read_groups_view();
            let dims = rgv.agent_dims();
            self.model
                .order()
                .iter()
                .filter(|tid| tid.as_str() != id.as_str())
                .filter_map(|tid| {
                    let tr = rgv.canvas_body_rect_of(tid)?;
                    if let Some(side) = rgv.open_agent_side(tid) {
                        let ar = agent_view::agent_view_rect(
                            tr,
                            self.names.bar_height(tid, cur_zoom),
                            side,
                            dims,
                        );
                        union_rect(&[tr, ar])
                    } else {
                        Some(tr)
                    }
                })
                .collect()
        };
        let anchor_ft = self
            .model
            .flow_anchor_id()
            .and_then(|aid| self.frame_target(aid));
        let spec = TerminalSpec {
            id: id.clone(),
            cols: info.cols,
            rows: info.rows,
            shell_title: info.title,
        };
        self.model.mount_terminal_with_taken(
            spec,
            info.modes,
            canvas_cam,
            track_flow,
            Some(&taken_rects),
            anchor_ft,
        );
        self.names.attach(&id);
        let word_chars = self.model.config().selection_word_chars.clone();
        self.selections
            .entry(id.clone())
            .or_insert_with(|| TerminalSelection::with_word_chars(&word_chars));

        if let Some(ref target) = pending.as_ref().and_then(|p| p.attach_target.clone()) {
            self.model.store_mut().set_attached(&id, Some(target));
        }

        if let Some(gid) = self.groups.group_of(&id) {
            let _ = self.with_groups(|g, host| g.relayout(host, &gid));
        }
        self.apply_z_order();

        if info.adopted {
            let owned = id.clone();
            self.with_fullscreen(|fs, host| fs.adopt(host, &owned));
            self.apply_z_order();
            self.model.notify_changed();
            return LandedAs::Canvas;
        }

        if let Some(PendingSpawn {
            split: Some((at, dir)),
            ..
        }) = pending.as_ref()
        {
            let at = at.clone();
            let dir = *dir;
            let inserted = self.with_groups(|g, host| g.insert_pane(host, &at, &id, dir));
            match inserted {
                Ok(group_id) => {
                    self.apply_z_order();
                    self.focus(Some(&id), false);
                    let axis = match dir {
                        SplitInsertDir::Right => SplitAxis::Right,
                        SplitInsertDir::Down => SplitAxis::Down,
                    };
                    return LandedAs::Split { group_id, at, axis };
                }
                Err(err) => {
                    self.model.notify(
                        format!("split {} of {at}: {err}", dir.as_str()),
                        self.now_ms,
                    );
                }
            }
        }

        self.focus(Some(&id), true);
        if let Some(PendingSpawn {
            window_top_left: Some(at),
            zoom_when_asked,
            ..
        }) = pending
        {
            self.model.clear_flow();
            let bar = self.bar_height(&id, zoom_when_asked);
            let raw_body = Point::new(at.x, at.y - bar);
            let step = if self.model.config().snap {
                snap_step(self.grid_spec(), zoom_when_asked)
            } else {
                0.0
            };
            let target_body = if step > 0.0 {
                Point::new(snap_to(raw_body.x, step), snap_to(raw_body.y, step))
            } else {
                raw_body
            };
            self.model.place_body(&id, target_body);
            self.model.remember_position(&id);
            let owned = id.clone();
            let _ = self.with_groups(|g, host| g.relayout_of(host, &owned));
            self.frame_terminal(&id);
        }
        self.model.notify_changed();
        LandedAs::Canvas
    }

    fn terminal_resized(&mut self, id: &str, cols: u32, rows: u32) {
        let prev = self.model.terminal(id).map(|t| (t.cols, t.rows));
        if !self.model.set_terminal_size_raw(id, cols, rows) {
            return;
        }
        if prev != Some((cols, rows)) {
            self.clear_text_selection(id);
        }
        let in_fs = self.fullscreen.active() && self.fullscreen.has(&self.read_fs_view(), id);
        if in_fs {
            self.with_fullscreen(|fs, host| {
                fs.refit(host);
            });
        } else {
            let owned = id.to_owned();
            let _ = self.with_groups(|g, host| g.relayout_of(host, &owned));
        }
        self.apply_z_order();
        self.model.notify_changed();
    }

    fn terminal_screen_filled(
        &mut self,
        id: &str,
        screen: &Screen,
        delta: ScreenFillDelta<'_>,
    ) -> bool {
        if self.model.terminal(id).is_none() {
            return false;
        }
        let mut changed = false;

        // 1. Maintain active text selection lifetime across scroll, rewrite, or resize.
        if let Some(sel) = self.selections.get_mut(id) {
            if sel.sync_after_fill(
                screen,
                delta.resized,
                delta.scrolled_top,
                delta.within_margins,
                delta.rewritten_cols,
            ) {
                if sel.is_empty() {
                    self.selection_text_cache.remove(id);
                    self.model.set_terminal_has_selection(id, false);
                } else {
                    let text = sel.text(screen);
                    self.selection_text_cache.insert(id.to_owned(), text);
                    self.model.set_terminal_has_selection(id, true);
                }
                changed = true;
            }
        }

        // 2. Refresh the last non-blank preview line when cells changed (or preview is still empty).
        let cells_changed = delta.resized
            || delta.scrolled_top > 0
            || delta.within_margins
            || delta.rewritten_cols.iter().any(|&(lo, hi)| lo < hi);
        let cur_preview_empty = self
            .model
            .terminal(id)
            .is_some_and(|t| t.preview.is_empty());
        if cells_changed || cur_preview_empty {
            extract_preview_into(screen, &mut self.preview_scratch);
            let unchanged = self
                .model
                .terminal(id)
                .is_some_and(|t| t.preview == self.preview_scratch);
            if !unchanged {
                let next = self.preview_scratch.clone();
                changed |= self.model.set_terminal_preview(id, next);
            }
        }

        // 3. Propagate `Screen::title` when the emulator's title changed (including an
        //    adopted terminal's initial `Repaint` OSC title), without clobbering a title
        //    delivered out-of-band by `Event::Title` when `screen.title` stays empty.
        let prev_screen_title = self
            .last_screen_titles
            .get(id)
            .map_or("", String::as_str);
        if screen.title != prev_screen_title {
            let next_title = screen.title.clone();
            if let Some(slot) = self.last_screen_titles.get_mut(id) {
                slot.clone_from(&next_title);
            } else {
                self.last_screen_titles.insert(id.to_owned(), next_title.clone());
            }
            changed |= self.model.set_terminal_title(id, next_title);
        }

        if changed {
            if self
                .hovered_link
                .as_ref()
                .is_some_and(|(tid, _)| tid == id)
                && (delta.resized || delta.scrolled_top > 0 || !delta.rewritten_cols.is_empty())
            {
                self.set_hovered_link(None);
            }
            self.model.notify_changed();
        }
        changed
    }

    fn terminal_ended(&mut self, id: &str) {
        let prev_group = self.groups.group_of(id);
        let (existed, was_focused) = self.model.unmount_terminal_raw(id);
        if !existed {
            return;
        }
        self.selections.remove(id);
        self.selection_text_cache.remove(id);
        self.last_screen_titles.remove(id);
        self.agent_stick_bottom.remove(id);
        if self.hovered_label_id.as_deref() == Some(id) {
            self.hovered_label_id = None;
        }
        if self.hovered_link.as_ref().is_some_and(|(tid, _)| tid == id) {
            self.set_hovered_link(None);
        }
        if self
            .pending_link_click
            .as_ref()
            .is_some_and(|(tid, _, _)| tid == id)
        {
            self.pending_link_click = None;
        }
        self.names.prune(|tid| self.model.terminal(tid).is_some());
        if self.fullscreen.active() {
            let owned = id.to_owned();
            self.with_fullscreen(|fs, host| fs.gone(host, &owned));
        }
        if let Some(ref gid) = prev_group {
            let _ = self.with_groups(|g, host| g.relayout(host, gid));
        }
        if was_focused {
            self.restore_focus_after(id, prev_group.as_deref());
        }
        self.apply_z_order();
        self.model.notify_changed();
    }

    fn set_live_terminals<S: AsRef<str>>(&mut self, live_ids: &[S]) -> Vec<String> {
        let mut keep: HashSet<String> = live_ids.iter().map(|s| s.as_ref().to_owned()).collect();
        keep.extend(self.model.order().iter().cloned());
        let gone = self.model.store_mut().prune(keep);
        if !gone.is_empty() {
            let map: HashMap<String, Group> = self
                .model
                .store()
                .groups()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            self.groups = Groups::from_map(map);
            let _ = self.with_groups(|g, host| g.relayout_all(host));
            self.apply_z_order();
            self.model.notify_changed();
        }
        gone
    }

    fn group_pane_order(&self, gid: &str) -> Vec<String> {
        let Ok(group) = self.groups.get(gid) else {
            return Vec::new();
        };
        let view = self.read_groups_view();
        let shown = self.groups.shown_members(&view, gid).unwrap_or_default();
        if group.layout == GroupLayout::Freeform {
            let zoom = self.model.camera().zoom();
            let items: Vec<ReadingItem> = shown
                .iter()
                .filter_map(|id| {
                    self.canvas_window_rect(id, zoom)
                        .map(|rect| ReadingItem::new(id.clone(), rect))
                })
                .collect();
            return reading_order(&items);
        }
        shown
    }

    fn top_units(&self) -> Vec<(bool, String, Vec<String>, Rect)> {
        let zoom = self.model.camera().zoom();
        let mut raw: Vec<(bool, String, Vec<String>, Rect)> = Vec::new();
        let mut grouped: HashSet<String> = HashSet::new();
        for gid in self.groups.ids() {
            let members = self.group_pane_order(&gid);
            if members.is_empty() {
                continue;
            }
            let rects: Vec<Rect> = members
                .iter()
                .filter_map(|id| self.canvas_window_rect(id, zoom))
                .collect();
            if let Some(bounds) = union_rect(&rects) {
                for m in &members {
                    grouped.insert(m.clone());
                }
                raw.push((true, gid, members, bounds));
            }
        }
        for id in self.model.shown_order() {
            if !grouped.contains(&id) {
                if let Some(rect) = self.canvas_window_rect(&id, zoom) {
                    raw.push((false, id.clone(), vec![id], rect));
                }
            }
        }
        let items: Vec<ReadingItem> = raw
            .iter()
            .enumerate()
            .map(|(i, (_, _, _, rect))| ReadingItem::new(i.to_string(), *rect))
            .collect();
        reading_order(&items)
            .into_iter()
            .filter_map(|s| s.parse::<usize>().ok())
            .map(|i| raw[i].clone())
            .collect()
    }

    fn cycle_focus(&mut self, dir: i32) -> Option<String> {
        let zoom = self.model.camera().zoom();
        let focused = self.model.focused_id().map(str::to_owned);
        let gid = focused.as_deref().and_then(|id| self.groups.group_of(id));
        let mut scope: Option<Vec<String>> = None;
        if let Some(g) = gid {
            let members = self.group_pane_order(&g);
            if members.len() >= 2 {
                scope = Some(members);
            }
        }
        let scope = scope.unwrap_or_else(|| {
            let loose: Vec<String> = self
                .model
                .shown_order()
                .into_iter()
                .filter(|id| self.groups.group_of(id).is_none())
                .collect();
            if loose.len() >= 2
                && focused
                    .as_deref()
                    .is_none_or(|f| loose.iter().any(|x| x == f))
            {
                let items: Vec<ReadingItem> = loose
                    .iter()
                    .filter_map(|id| {
                        self.canvas_window_rect(id, zoom)
                            .map(|rect| ReadingItem::new(id.clone(), rect))
                    })
                    .collect();
                reading_order(&items)
            } else {
                self.top_units()
                    .into_iter()
                    .flat_map(|(_, _, members, _)| members)
                    .collect()
            }
        });
        let next = cycle(&scope, focused.as_deref(), dir as isize)?.to_owned();
        self.focus_in_view(&next);
        Some(next)
    }

    fn cycle_group_focus(&mut self, dir: i32) -> Option<String> {
        let units = self.top_units();
        if units.is_empty() {
            return None;
        }
        let focused = self.model.focused_id().map(str::to_owned);
        let cur_idx = focused
            .as_deref()
            .and_then(|f| units.iter().position(|(_, _, members, _)| members.iter().any(|m| m == f)));
        let idx = match cur_idx {
            None => {
                if dir > 0 {
                    0
                } else {
                    units.len() - 1
                }
            }
            Some(i) => {
                let step = if dir < 0 { -1_isize } else { 1_isize };
                (i as isize + step).rem_euclid(units.len() as isize) as usize
            }
        };
        let (is_group, unit_id, members, unit_rect) = &units[idx];
        let mut target = members[0].clone();
        if *is_group && members.len() > 1 {
            for h in self.model.focus_history().iter().rev() {
                if members.iter().any(|m| m == h) {
                    target = h.clone();
                    break;
                }
            }
        }
        if self.fullscreen.active() {
            self.focus(Some(&target), true);
        } else if *is_group {
            let sbox = self.model.camera().to_screen_box(*unit_rect);
            let (vw, vh) = self.model.camera().view();
            let edges = self.dock_edges();
            self.focus(Some(&target), false);
            if sbox.left < edges.left
                || sbox.top < 0.0
                || sbox.right > vw - edges.right
                || sbox.bottom > vh
            {
                let _ = self.frame_group(unit_id);
            }
        } else {
            self.focus_in_view(&target);
        }
        Some(target)
    }

    fn step_focus_history(&mut self, dir: i32) -> Option<String> {
        let id = self.model.step_focus_history_pick(dir)?;
        self.model.set_stepping_history(true);
        self.focus_in_view(&id);
        self.model.set_stepping_history(false);
        Some(id)
    }

    fn focus_pane(&mut self, dir: Direction) -> Option<String> {
        let current = self.model.focused_id()?.to_owned();
        let zoom = self.model.camera().zoom();
        let mut rects = PaneMap::new();
        for id in self.model.shown_order() {
            if let Some(win) = self.canvas_window_rect(&id, zoom) {
                rects.insert(id, win);
            }
        }
        let next = split::neighbor(&rects, &current, dir)?;
        self.focus_in_view(&next);
        Some(next)
    }

    fn sync_effective_cursor(&mut self) {
        let desired = if self.hovered_link.is_some() {
            CursorIcon::Pointer
        } else {
            self.base_cursor
        };
        if self.cursor != desired {
            self.cursor = desired;
            self.model.push_effect(WallEffect::SetCursor { cursor: desired });
        }
    }

    fn set_cursor(&mut self, cursor: CursorIcon) {
        self.base_cursor = cursor;
        self.sync_effective_cursor();
    }

    fn set_hovered_link(&mut self, next: Option<(String, HoveredLink)>) {
        if self.hovered_link != next {
            self.hovered_link = next;
            self.sync_effective_cursor();
            self.model.notify_changed();
        }
    }

    fn exact_cell_at(&self, id: &str, screen_x: f64, screen_y: f64) -> Option<(usize, usize)> {
        let entry = self.model.terminal(id)?;
        let geom = self.model.geometry();
        let world = self.model.camera().to_world(Point::new(screen_x, screen_y));
        let origin = entry.cell_origin(geom);
        let scale = entry.scale.max(1e-9);
        let cx = (world.x - origin.x) / (scale * geom.stride.x.max(1e-9));
        let cy = (origin.y - world.y) / (scale * geom.stride.y.max(1e-9));
        if cx < 0.0 || cy < 0.0 || cx >= f64::from(entry.cols) || cy >= f64::from(entry.rows) {
            return None;
        }
        Some((cy.floor() as usize, cx.floor() as usize))
    }

    fn link_at_screen_point(
        &self,
        screen_x: f64,
        screen_y: f64,
        screens: &dyn ScreenLookup,
    ) -> Option<(String, HoveredLink)> {
        let world = self.model.camera().to_world(Point::new(screen_x, screen_y));
        let CanvasHit::Terminal {
            id,
            part: HitPart::Body,
        } = self.hit(world)?
        else {
            return None;
        };
        if self.fullscreen.active() {
            let fs_view = self.read_fs_view();
            let elevated = self.fullscreen.elevated_ids(&fs_view);
            if !elevated.iter().any(|e| e == &id) {
                return None;
            }
        }
        let (row, col) = self.exact_cell_at(&id, screen_x, screen_y)?;
        let view = screens.screen_view(&id)?;
        let link = link_at_cell(view, row, col)?;
        Some((id, link))
    }

    fn update_hovered_link(&mut self, mods: Mods, screens: &dyn ScreenLookup) {
        self.last_pointer_mods = mods;
        let next = if is_link_open_mods(self.platform, mods) {
            self.last_pointer_screen
                .and_then(|pt| self.link_at_screen_point(pt.x, pt.y, screens))
        } else {
            None
        };
        self.set_hovered_link(next);
    }

    fn refresh_selection_text(&mut self, id: &str, screens: &dyn ScreenLookup) -> String {
        let Some(sel) = self.selections.get(id) else {
            return String::new();
        };
        if sel.is_empty() {
            self.selection_text_cache.remove(id);
            self.model.set_terminal_has_selection(id, false);
            return String::new();
        }
        if let Some(screen) = screens.screen(id) {
            let text = sel.text(screen);
            self.selection_text_cache.insert(id.to_owned(), text.clone());
            self.model.set_terminal_has_selection(id, !text.is_empty());
            text
        } else {
            self.selection_text_cache
                .get(id)
                .cloned()
                .unwrap_or_default()
        }
    }

    fn refresh_all_selection_texts(&mut self, screens: &dyn ScreenLookup) {
        let ids: Vec<String> = self
            .selections
            .iter()
            .filter(|(_, s)| !s.is_empty())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let _ = self.refresh_selection_text(&id, screens);
        }
    }

    fn clear_text_selection(&mut self, id: &str) {
        if let Some(sel) = self.selections.get_mut(id) {
            sel.clear();
        }
        self.selection_text_cache.remove(id);
        self.model.set_terminal_has_selection(id, false);
    }

    fn apply_config_side_effects(&mut self, key: &str) {
        if key == "selectionWordChars" {
            let word_chars = self.model.config().selection_word_chars.clone();
            for sel in self.selections.values_mut() {
                sel.set_word_chars(&word_chars);
            }
        }
        if matches!(key, "agentViewWidth" | "agentViewHeight" | "agentViewGap") {
            let open_ids: Vec<String> = self
                .model
                .order()
                .iter()
                .filter(|id| self.model.store().agent_view(id).is_some_and(|av| av.open))
                .cloned()
                .collect();
            for id in open_ids {
                self.agent_footprint_changed(&id);
            }
        } else if matches!(key, "splitGap" | "tileGap" | "groupPad" | "groupColumns") {
            let _ = self.with_groups(|g, host| g.relayout_all(host));
        }
        if self.fullscreen.active()
            && matches!(key, "fullscreenRowPx" | "fullscreenMargin" | "splitGap")
        {
            self.with_fullscreen(|fs, host| {
                fs.refit(host);
            });
        }
    }

    fn start_close_hold(&mut self, id: &str) {
        if self.model.terminal(id).is_none() {
            return;
        }
        if self.model.config().close_hold_ms <= 0.0 {
            self.close_hold = None;
            let _ = self.close_terminal(id);
            return;
        }
        let already = self
            .close_hold
            .as_ref()
            .is_some_and(|h| h.terminal_id == id);
        if !already {
            self.close_hold = Some(CloseHold {
                terminal_id: id.to_owned(),
                started_ms: self.now_ms,
            });
            self.model.notify_changed();
        }
    }

    fn cancel_close_hold(&mut self) {
        if self.close_hold.take().is_some() {
            self.model.notify_changed();
        }
    }

    fn tick_close_hold(&mut self) -> bool {
        let Some(hold) = self.close_hold.clone() else {
            return false;
        };
        if self.model.terminal(&hold.terminal_id).is_none() {
            self.close_hold = None;
            return false;
        }
        let duration = self.model.config().close_hold_ms;
        if self.now_ms - hold.started_ms >= duration {
            self.close_hold = None;
            let _ = self.close_terminal(&hold.terminal_id);
            false
        } else {
            self.model.notify_changed();
            true
        }
    }

    fn build_frame(&mut self, drop_preview: Option<DropPreview>, pointer_gliding: bool) -> WallFrame {
        let camera = self.model.camera().pose();
        let (vw, vh) = self.model.camera().view();
        let zoom = camera.zoom.max(1e-9);
        let geom = self.model.geometry();
        let cfg = self.model.config().clone();
        let fs_view_elevated = self.fullscreen.elevated_ids(&self.read_fs_view());
        let elevated_set: HashSet<&str> = fs_view_elevated.iter().map(String::as_str).collect();

        // Update title bars and map labels for all shown terminals.
        let shown = self.model.shown_order();
        let mut title_meta: HashMap<String, (String, Option<String>, Option<AgentStatus>, String)> =
            HashMap::with_capacity(shown.len());
        let named_terminals: Vec<NamedTerminal> = shown
            .iter()
            .filter_map(|id| {
                let body_rect = self.model.terminal_rect(id)?;
                let (name, attached_agent, agent_status, title) = self.title_parts(id);
                let unit_text_width =
                    (title.chars().count() as f64) * self.char_widths.title_unit_width;
                let map_label_width_px = Some(names::estimate_map_label_width_with(
                    &title,
                    self.char_widths.map_label_char_px,
                ));
                title_meta.insert(id.clone(), (name, attached_agent, agent_status, title.clone()));
                Some(NamedTerminal {
                    id: id.clone(),
                    name: title,
                    body_rect,
                    unit_text_width,
                    map_label_width_px,
                })
            })
            .collect();
        let names_plan = self.names.update(&NamesFrame {
            camera,
            view_w: vw,
            view_h: vh,
            terminals: named_terminals,
            focused_id: self.model.focused_id().map(str::to_owned),
            hovered_id: self.model.hovered_id().map(str::to_owned),
            fullscreen_solo_id: self.fullscreen.solo_id().map(str::to_owned),
            fullscreen_active: self.fullscreen.active(),
        });

        let selected_set: HashSet<&str> =
            self.model.selection().iter().map(String::as_str).collect();
        let focused_id = self.model.focused_id().map(str::to_owned);
        let hovered_id = self.model.hovered_id().map(str::to_owned);

        // Terminals in back-to-front draw order by Z.
        let mut terminals: Vec<TerminalFrame> = shown
            .iter()
            .filter_map(|id| {
                let entry = self.model.terminal(id)?;
                let focused = focused_id.as_deref() == Some(id.as_str());
                let hovered = hovered_id.as_deref() == Some(id.as_str());
                let text_selection = self.selections.get(id).and_then(|sel| {
                    (!sel.is_empty()).then(|| TextSelectionFrame {
                        r0: sel.r0,
                        c0: sel.c0,
                        r1: sel.r1,
                        c1: sel.c1,
                        block: sel.block,
                        background: cfg.selection_color.clone(),
                        foreground: cfg.selection_foreground.clone(),
                    })
                });
                let close_hold_progress = self
                    .close_hold
                    .as_ref()
                    .filter(|h| h.terminal_id == *id)
                    .map(|h| {
                        let dur = cfg.close_hold_ms.max(1e-6);
                        ((self.now_ms - h.started_ms) / dur).clamp(0.0, 1.0)
                    });
                Some(TerminalFrame {
                    id: id.clone(),
                    cell_origin: entry.cell_origin(geom),
                    body_rect: entry.body_rect(geom),
                    cols: entry.cols,
                    rows: entry.rows,
                    scale: entry.scale,
                    z: entry.z,
                    focused,
                    hovered,
                    selected: selected_set.contains(id.as_str()),
                    border: BorderState {
                        edge: true,
                        focused,
                        hovered,
                    },
                    text_selection,
                    hovered_link: self
                        .hovered_link
                        .as_ref()
                        .filter(|(tid, _)| tid == id)
                        .map(|(_, link)| link.clone()),
                    fullscreen: elevated_set.contains(id.as_str()),
                    close_hold_progress,
                })
            })
            .collect();
        terminals.sort_by(|a, b| a.z.total_cmp(&b.z));

        // Title bars.
        let mut title_bars = Vec::new();
        for bar in &names_plan.bars {
            if !bar.visible {
                continue;
            }
            let Some(layout) = bar.layout else {
                continue;
            };
            let z = self.model.terminal(&bar.id).map_or(0.0, |t| t.z);
            let (fill_color, text_color) = self
                .names
                .bar(&bar.id)
                .map_or(
                    (
                        names::TERMINAL_BAR_STYLE.fill_idle,
                        names::TERMINAL_BAR_STYLE.text_idle,
                    ),
                    TitleBarModel::active_colors,
                );
            let (name, attached_agent, agent_status) = match title_meta.get(&bar.id) {
                Some((name, attached, status, _)) => (name.clone(), attached.clone(), *status),
                None => (bar.name.clone(), None, None),
            };
            title_bars.push(TitleBarFrame {
                terminal_id: bar.id.clone(),
                rect: layout.strip_rect,
                name,
                text: bar.name.clone(),
                attached_agent,
                agent_status,
                text_visible: bar.name_visible,
                text_left: layout.text_left,
                text_height: layout.line_height,
                text_px: layout.text_px,
                fill_color,
                text_color,
                z,
                focused: bar.focused,
                hovered: hovered_id.as_deref() == Some(bar.id.as_str()),
            });
        }

        // Group frames and headers.
        let groups_view = self.read_groups_view();
        let mut group_frames = Vec::new();
        for gid in self.groups.ids() {
            let Ok(group) = self.groups.get(&gid) else {
                continue;
            };
            let Ok(Some(frame_rect)) = self.groups.frame_rect(&groups_view, &gid, zoom, None)
            else {
                continue;
            };
            let Ok(Some(header_rect)) = self.groups.header_rect(&groups_view, &gid, zoom, None)
            else {
                continue;
            };
            let name = self.groups.name_of(&gid).unwrap_or_else(|_| gid.clone());
            let focused = focused_id
                .as_deref()
                .is_some_and(|f| group.members.iter().any(|m| m == f));
            let hovered =
                !self.fullscreen.active() && self.hovered_group_id.as_deref() == Some(gid.as_str());
            let mut header_model = TitleBarModel::group_header();
            header_model.set_name(name.clone());
            header_model.set_focused(focused);
            let unit_w = (name.chars().count() as f64) * self.char_widths.title_unit_width;
            let header_layout = header_model.layout(
                frame_rect.min_x,
                frame_rect.max_x,
                frame_rect.max_y,
                1.0,
                unit_w,
                zoom,
            );
            let (fill_color, text_color) = if focused {
                (GROUP_HEADER_STYLE.fill_focused, GROUP_HEADER_STYLE.text_focused)
            } else {
                (GROUP_HEADER_STYLE.fill_idle, GROUP_HEADER_STYLE.text_idle)
            };
            group_frames.push(GroupFrame {
                id: gid.clone(),
                name,
                layout: group.layout,
                frame_rect,
                header_rect,
                text_left: header_layout.text_left,
                text_height: header_layout.line_height,
                text_px: header_layout.text_px,
                fill_color,
                text_color,
                z: self.groups.depth_of(&gid),
                focused,
                hovered,
            });
        }
        group_frames.sort_by(|a, b| a.z.total_cmp(&b.z));

        // Selection outlines (canvas only, hidden while fullscreen veils the canvas).
        let mut selection_outlines = Vec::new();
        if !self.fullscreen.active() {
            for id in self.model.selection() {
                if self.model.is_shown(id) {
                    if let Some(rect) = self.window_rect_at_zoom(id, zoom) {
                        let z = self.model.terminal(id).map_or(0.0, |t| t.z);
                        selection_outlines.push(SelectionOutline {
                            terminal_id: id.clone(),
                            rect,
                            z,
                        });
                    }
                }
            }
        }

        // Lasso polygon converted from screen px to world space.
        let lasso = self.lasso_screen.as_ref().map(|pts| {
            pts.iter()
                .map(|p| self.model.camera().to_world(*p))
                .collect()
        });

        // Map labels placed in screen space.
        let hovered_label = (!self.fullscreen.active())
            .then_some(self.hovered_label_id.as_deref())
            .flatten();
        let map_labels: Vec<MapLabelFrame> = names_plan
            .map_labels
            .into_iter()
            .filter_map(|m| {
                let placement = m.placement?;
                let (name, attached_agent, agent_status) = match title_meta.get(&m.id) {
                    Some((name, attached, status, _)) => (name.clone(), attached.clone(), *status),
                    None => (m.name.clone(), None, None),
                };
                let hovered = m.hovered || hovered_label == Some(m.id.as_str());
                Some(MapLabelFrame {
                    terminal_id: m.id,
                    name,
                    text: m.name,
                    attached_agent,
                    agent_status,
                    focused: m.focused,
                    hovered,
                    label_box: placement.label_box,
                    target: m.target,
                    leader: placement.leader,
                })
            })
            .collect();
        self.last_map_labels = map_labels
            .iter()
            .map(|ml| (ml.terminal_id.clone(), ml.label_box))
            .collect();

        // Fullscreen backdrop.
        let fullscreen_backdrop = if self.fullscreen.active() {
            let fs_view = self.read_fs_view();
            let area = self.fullscreen.area(&fs_view);
            let mode = self.fullscreen.mode().unwrap_or(FullscreenMode::Single);
            let target = self.fullscreen.target().unwrap_or_default().to_owned();
            let cutouts = match mode {
                FullscreenMode::Single => self
                    .fullscreen
                    .id()
                    .and_then(|id| self.model.terminal_rect(id))
                    .into_iter()
                    .collect(),
                FullscreenMode::Group => {
                    let group = self
                        .fullscreen
                        .group_id()
                        .and_then(|gid| self.groups.group(gid));
                    let is_split = group.is_some_and(|g| g.layout == GroupLayout::Split);
                    if is_split {
                        let has_bars = group
                            .and_then(|g| g.split.as_ref())
                            .is_some_and(|s| s.bars);
                        let rects: Vec<Rect> = fs_view_elevated
                            .iter()
                            .filter_map(|id| {
                                if has_bars {
                                    self.window_rect_at_zoom(id, zoom)
                                } else {
                                    self.model.terminal_rect(id)
                                }
                            })
                            .collect();
                        union_rect(&rects).into_iter().collect()
                    } else {
                        fs_view_elevated
                            .iter()
                            .filter_map(|id| {
                                self.window_rect_at_zoom(id, zoom)
                                    .or_else(|| self.model.terminal_rect(id))
                            })
                            .collect()
                    }
                }
            };
            Some(FullscreenBackdrop {
                mode,
                target,
                area,
                background: cfg.background.clone(),
                opacity: BACKDROP_OPACITY,
                cutouts,
                z: FULLSCREEN_BACKDROP_Z,
            })
        } else {
            None
        };

        // Paper & grid levels.
        let grid = self.grid_spec();
        let paper = PaperFrame {
            background: cfg.background.clone(),
            grid_show: cfg.grid_show,
            grid,
            levels: grid_levels(grid, zoom),
            line_px: cfg.grid_line_px,
            minor_color: cfg.grid_minor_color.clone(),
            major_color: cfg.grid_major_color.clone(),
        };

        // Docked panels.
        let dock_state = dock_state_from_store(self.model.store());
        let panels = PanelsFrame {
            sidebar: self.model.store().sidebar().clone(),
            status_window: self.model.store().status_window().clone(),
            settings_panel: self.model.store().settings_panel().clone(),
            help_panel: self.model.store().help_panel().clone(),
            dock: dock_layout(&dock_state),
            palette: self.model.store().palette_size(),
        };

        let animating = self.model.camera().moving()
            || pointer_gliding
            || self.fullscreen.settling()
            || self.close_hold.is_some();

        WallFrame {
            camera,
            viewport: Size::new(vw, vh),
            scale_factor: self.scale_factor,
            animating,
            paper,
            terminals,
            title_bars,
            groups: group_frames,
            selection_outlines,
            lasso,
            drop_preview,
            map_labels,
            fullscreen_backdrop,
            notice: self.model.notice().map(str::to_owned),
            status_notice: self.status_notice(),
            menu: self.model.menu().cloned(),
            palette_open: self.palette_open,
            panels,
        }
    }

    fn agent_footprint_changed(&mut self, id: &str) {
        let mut av = std::mem::take(&mut self.model.agent_views);
        let default_side = self.model.config().agent_view_side;
        av.sync_open_from_store(self.model.store(), default_side);
        let mut bridge = AgentFootprintBridge {
            model: &mut self.model,
            groups: &mut self.groups,
            fullscreen: &self.fullscreen,
            names: &mut self.names,
            needs_fullscreen_refit: false,
        };
        av.prune(&bridge, |tid| {
            bridge.model.terminal(tid).is_some() || bridge.model.store().agent_view(tid).is_some()
        });
        av.footprint_changed(&mut bridge, id);
        let refit = bridge.needs_fullscreen_refit;
        let _ = av.drain_effects();
        self.model.agent_views = av;
        self.sync_groups_to_store();
        self.apply_z_order();
        if refit {
            self.flush_cross_effects(true, Vec::new());
        }
    }
}

// ── WallApi implementation for WallCore (all 54 wall.* verbs) ────────────────

fn knob_to_json(val: KnobValue) -> Value {
    match val {
        KnobValue::Bool(b) => Value::Bool(b),
        KnobValue::Number(n) => json!(n),
        KnobValue::Text(s) => Value::String(s),
    }
}

fn json_to_knob_input(val: Value) -> KnobInput {
    match val {
        Value::Bool(b) => KnobInput::Bool(b),
        Value::Number(n) => KnobInput::Number(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => KnobInput::Text(s),
        other => KnobInput::Text(other.to_string()),
    }
}

fn parse_split_dir(dir: &str) -> Result<SplitInsertDir, String> {
    SplitInsertDir::from_str(dir).map_err(|e| e.to_string())
}

fn parse_direction(dir: &str) -> Result<Direction, String> {
    match dir {
        "left" => Ok(Direction::Left),
        "right" => Ok(Direction::Right),
        "up" => Ok(Direction::Up),
        "down" => Ok(Direction::Down),
        other => Err(format!(
            "direction must be left, right, up or down, not '{other}'"
        )),
    }
}

impl WallApi for WallCore {
    fn snapshot(&self) -> CmdWallSnapshot {
        let terminals = self
            .model
            .order()
            .iter()
            .map(|id| TerminalSummary {
                id: id.clone(),
                name: self.model.name_of(id),
                hidden: self.model.store().hidden(id),
                group: self.groups.group_of(id),
            })
            .collect();
        let groups = self
            .groups
            .ids()
            .into_iter()
            .filter_map(|gid| {
                let g = self.groups.group(&gid)?;
                Some(GroupSummary {
                    id: gid.clone(),
                    name: self.groups.name_of(&gid).unwrap_or_else(|_| gid.clone()),
                    layout: g.layout.as_str().to_owned(),
                    columns: g.columns,
                    gap: g.gap,
                    bars: g.split.as_ref().map(|s| s.bars),
                    members: g
                        .members
                        .iter()
                        .filter(|id| self.model.terminal(id).is_some())
                        .cloned()
                        .collect(),
                })
            })
            .collect();
        CmdWallSnapshot {
            terminals,
            groups,
            selection: self.model.selection().to_vec(),
        }
    }

    fn has_terminal(&self, id: &str) -> bool {
        self.model.terminal(id).is_some()
    }

    fn is_shown(&self, id: &str) -> bool {
        self.model.is_shown(id)
    }

    fn shown_order(&self) -> Vec<String> {
        self.model.shown_order()
    }

    fn focused_id(&self) -> Option<String> {
        self.model.focused_id().map(str::to_owned)
    }

    fn hide(&mut self, id: &str) -> Result<(), String> {
        self.hide_terminal(id);
        Ok(())
    }

    fn show(&mut self, id: &str) -> Result<(), String> {
        self.show_terminal(id);
        Ok(())
    }

    fn focus(&mut self, id: &str) -> Result<(), String> {
        if !self.focus(Some(id), false) {
            return Err(format!("no terminal '{id}'"));
        }
        Ok(())
    }

    fn focus_and_frame(&mut self, id: &str) -> Result<(), String> {
        if !self.focus(Some(id), true) {
            return Err(format!("no terminal '{id}'"));
        }
        Ok(())
    }

    fn spawn(&mut self, at: Option<Point>) -> Result<String, String> {
        let zoom = self.model.camera().zoom();
        self.termd.dismiss_lost();
        self.pending_spawns.push_back(PendingSpawn {
            window_top_left: at,
            zoom_when_asked: zoom,
            split: None,
            attach_target: None,
        });
        self.model.push_effect(WallEffect::SpawnTerminal {
            window_top_left: at,
            argv: None,
            cols: self.new_terminal_cells().0,
            rows: self.new_terminal_cells().1,
        });
        self.model.notify_changed();
        Ok("pending".to_owned())
    }

    fn fit_all(&mut self) -> Result<(), String> {
        WallCore::fit_all(self);
        Ok(())
    }

    fn close_terminal(&mut self, id: &str) -> Result<CommandReply, String> {
        Ok(WallCore::close_terminal(self, id))
    }

    fn rename_terminal(&mut self, id: &str, name: &str) -> Result<(), String> {
        if !self.model.rename(id, name) {
            return Err(format!("no terminal '{id}'"));
        }
        Ok(())
    }

    fn terminal_name(&self, id: &str) -> String {
        self.model.name_of(id)
    }

    fn selection(&self) -> Vec<String> {
        self.model.selection().to_vec()
    }

    fn select(&mut self, ids: &[String]) -> Result<(), String> {
        self.model.select(ids);
        Ok(())
    }

    fn terminal_selection_text(&self, id: &str) -> String {
        self.selection_text_cache
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    fn selection_clipboard(&self) -> String {
        self.model.selection_clipboard().to_owned()
    }

    fn write_clipboard(&mut self, text: &str) -> Result<(), String> {
        self.model.push_effect(WallEffect::WriteClipboard {
            text: text.to_owned(),
        });
        Ok(())
    }

    fn read_clipboard(&mut self) -> Result<String, String> {
        Err("clipboard read requires host delivery".to_owned())
    }

    fn request_clipboard_paste(&mut self, id: &str) -> Result<Option<String>, String> {
        self.model.push_effect(WallEffect::ReadClipboard {
            target: id.to_owned(),
        });
        Ok(None)
    }

    fn paste_into_terminal(&mut self, id: &str, text: &str) -> Result<bool, String> {
        if self.model.terminal(id).is_none() {
            return Ok(false);
        }
        if self.model.config().selection_clear_on_typing {
            self.clear_text_selection(id);
        }
        self.model.push_effect(WallEffect::PasteTerminal {
            id: id.to_owned(),
            text: text.to_owned(),
        });
        Ok(true)
    }

    fn notify(&mut self, message: &str) {
        self.model.notify(message, self.now_ms);
    }

    fn fullscreen_info(&self) -> FullscreenStatus {
        let state = self.fullscreen.state();
        let mode = match self.fullscreen.mode() {
            Some(FullscreenMode::Single) => "single".to_owned(),
            Some(FullscreenMode::Group) => "group".to_owned(),
            None => String::new(),
        };
        FullscreenStatus {
            active: self.fullscreen.active(),
            mode,
            target: self.fullscreen.target().map(str::to_owned),
            id: self.fullscreen.id().map(str::to_owned),
            group_id: self.fullscreen.group_id().map(str::to_owned),
            return_group: state.and_then(|s| s.return_group.clone()),
            line_px: state.map_or(0.0, |s| s.line_px),
            state: state.and_then(|s| serde_json::to_value(s).ok()),
        }
    }

    fn fullscreen_exit(&mut self) -> Result<(), String> {
        self.with_fullscreen(|fs, host| fs.exit(host));
        Ok(())
    }

    fn fullscreen_enter_default(&mut self, id: &str) -> Result<bool, String> {
        Ok(self.with_fullscreen(|fs, host| fs.enter_default(host, id)))
    }

    fn fullscreen_enter(
        &mut self,
        id: &str,
        return_group: Option<&str>,
    ) -> Result<bool, String> {
        let opts = EnterOptions {
            return_group: return_group.map(str::to_owned),
            solo_override: true,
        };
        Ok(self.with_fullscreen(|fs, host| fs.enter_with(host, id, opts)))
    }

    fn fullscreen_enter_group(&mut self, gid: &str) -> Result<bool, String> {
        Ok(self.with_fullscreen(|fs, host| fs.enter_group(host, gid)))
    }

    fn fullscreen_toggle_pane(&mut self, id: Option<&str>) -> Result<(), String> {
        self.with_fullscreen(|fs, host| {
            fs.toggle_pane(host, id);
        });
        Ok(())
    }

    fn fullscreen_text(&mut self, dir: i32) -> Result<Option<f64>, String> {
        let step = if dir > 0 {
            TextStep::Bigger
        } else if dir < 0 {
            TextStep::Smaller
        } else {
            TextStep::Reset
        };
        Ok(self.with_fullscreen(|fs, host| fs.text(host, step)))
    }

    fn has_group(&self, gid: &str) -> bool {
        self.groups.group(gid).is_some()
    }

    fn group_of(&self, id: &str) -> Option<String> {
        self.groups.group_of(id)
    }

    fn group_info(&self, gid: &str) -> Result<GroupSummary, String> {
        let g = self.groups.get(gid).map_err(|e| e.to_string())?;
        Ok(GroupSummary {
            id: gid.to_owned(),
            name: self.groups.name_of(gid).map_err(|e| e.to_string())?,
            layout: g.layout.as_str().to_owned(),
            columns: g.columns,
            gap: g.gap,
            bars: g.split.as_ref().map(|s| s.bars),
            members: g.members.clone(),
        })
    }

    fn group_name(&self, gid: &str) -> Result<String, String> {
        self.groups.name_of(gid).map_err(|e| e.to_string())
    }

    fn group_create(&mut self, ids: &[String]) -> Result<String, String> {
        let gid = self
            .with_groups(|g, host| g.create(host, ids, CreateGroupOptions::default()))
            .map_err(|e| e.to_string())?;
        self.apply_z_order();
        self.model.notify_changed();
        Ok(gid)
    }

    fn group_add(&mut self, gid: &str, ids: &[String]) -> Result<(), String> {
        self.with_groups(|g, host| g.add(host, gid, ids))
            .map_err(|e| e.to_string())?;
        self.apply_z_order();
        self.model.notify_changed();
        Ok(())
    }

    fn group_remove(&mut self, ids: &[String]) -> Result<(), String> {
        self.with_groups(|g, host| g.remove(host, ids))
            .map_err(|e| e.to_string())?;
        self.apply_z_order();
        self.model.notify_changed();
        Ok(())
    }

    fn group_ungroup(&mut self, gid: &str) -> Result<(), String> {
        self.with_groups(|g, host| g.ungroup(host, gid))
            .map_err(|e| e.to_string())?;
        self.apply_z_order();
        self.model.notify_changed();
        Ok(())
    }

    fn group_set_layout(
        &mut self,
        gid: &str,
        layout: &str,
        columns: Option<i64>,
        gap: Option<f64>,
    ) -> Result<(), String> {
        let parsed = GroupLayout::from_str(layout)?;
        let cols = match columns {
            Some(c) if c < 1 => {
                return Err(format!("columns must be a whole number ≥ 1, not '{c}'"));
            }
            Some(c) => Some(c as usize),
            None => None,
        };
        self.with_groups(|g, host| {
            g.set_layout(
                host,
                gid,
                parsed,
                SetLayoutOptions { columns: cols, gap },
            )
        })
        .map_err(|e| e.to_string())?;
        self.apply_z_order();
        self.model.notify_changed();
        Ok(())
    }

    fn group_set_columns(&mut self, gid: &str, columns: usize) -> Result<(), String> {
        self.with_groups(|g, host| g.set_columns(host, gid, columns))
            .map_err(|e| e.to_string())?;
        self.model.notify_changed();
        Ok(())
    }

    fn group_set_bars(&mut self, gid: &str, on: bool) -> Result<(), String> {
        self.with_groups(|g, host| g.set_bars(host, gid, on))
            .map_err(|e| e.to_string())?;
        self.model.notify_changed();
        Ok(())
    }

    fn group_set_gap(&mut self, gid: &str, gap: f64) -> Result<(), String> {
        self.with_groups(|g, host| g.set_gap(host, gid, gap))
            .map_err(|e| e.to_string())?;
        self.model.notify_changed();
        Ok(())
    }

    fn group_rename(&mut self, gid: &str, name: &str) -> Result<(), String> {
        self.with_groups(|g, host| g.rename(host, gid, name))
            .map_err(|e| e.to_string())?;
        self.model.notify_changed();
        Ok(())
    }

    fn group_move(&mut self, gid: &str, dx: f64, dy: f64) -> Result<(), String> {
        self.with_groups(|g, host| g.move_group(host, gid, dx, dy))
            .map_err(|e| e.to_string())?;
        self.model.notify_changed();
        Ok(())
    }

    fn frame_group(&mut self, gid: &str) -> Result<bool, String> {
        WallCore::frame_group(self, gid)
    }

    fn split_terminal(&mut self, dir: &str, id: Option<&str>) -> Result<String, String> {
        let split_dir = parse_split_dir(dir)?;
        let Some(target) = id.map(str::to_owned).or_else(|| self.model.focused_id().map(str::to_owned)) else {
            return Err("no terminal to split: focus one, or name it".to_owned());
        };
        if !self.model.is_shown(&target) {
            return Err(if self.model.terminal(&target).is_some() {
                format!("terminal '{target}' is hidden")
            } else {
                format!("no terminal '{target}'")
            });
        }
        {
            let view = self.read_groups_view();
            self.groups
                .can_split(&view, &target, split_dir)
                .map_err(|e| e.to_string())?;
        }
        if self.fullscreen.active() {
            self.with_fullscreen(|fs, host| fs.exit(host));
        }
        let zoom = self.model.camera().zoom();
        self.pending_spawns.push_back(PendingSpawn {
            window_top_left: None,
            zoom_when_asked: zoom,
            split: Some((target, split_dir)),
            attach_target: None,
        });
        self.model.push_effect(WallEffect::SpawnTerminal {
            window_top_left: None,
            argv: None,
            cols: self.new_terminal_cells().0,
            rows: self.new_terminal_cells().1,
        });
        self.model.notify_changed();
        Ok("pending".to_owned())
    }

    fn focus_pane(&mut self, dir: &str) -> Result<Option<String>, String> {
        let parsed = parse_direction(dir)?;
        Ok(WallCore::focus_pane(self, parsed))
    }

    fn group_move_pane(&mut self, id: &str, dir: &str) -> Result<Option<String>, String> {
        let parsed = parse_direction(dir)?;
        let res = self
            .with_groups(|g, host| g.move_pane(host, id, parsed))
            .map_err(|e| e.to_string())?;
        self.model.notify_changed();
        Ok(res)
    }

    fn step_focus_history(&mut self, dir: i32) -> Result<Option<String>, String> {
        Ok(WallCore::step_focus_history(self, dir))
    }

    fn panel(&self, kind: PanelKind) -> PanelPlacement {
        let store = self.model.store();
        match kind {
            PanelKind::Sidebar => {
                let s = store.sidebar();
                PanelPlacement {
                    open: s.open,
                    side: s.side.as_str().to_owned(),
                    float: Some(PointPayload::from(s.float)),
                    collapsed: s.collapsed.clone(),
                    mode: Some(s.mode.as_str().to_owned()),
                    size: s.size,
                    section: None,
                }
            }
            PanelKind::Status => {
                let s = store.status_window();
                PanelPlacement {
                    open: s.open,
                    side: s.side.as_str().to_owned(),
                    float: s.float.map(PointPayload::from),
                    collapsed: Vec::new(),
                    mode: None,
                    size: s.size,
                    section: None,
                }
            }
            PanelKind::Settings => {
                let s = store.settings_panel();
                PanelPlacement {
                    open: s.open,
                    side: s.side.as_str().to_owned(),
                    float: s.float.map(PointPayload::from),
                    collapsed: s.collapsed.clone(),
                    mode: None,
                    size: s.size,
                    section: s.section.clone(),
                }
            }
            PanelKind::Help => {
                let s = store.help_panel();
                PanelPlacement {
                    open: s.open,
                    side: s.side.as_str().to_owned(),
                    float: s.float.map(PointPayload::from),
                    collapsed: Vec::new(),
                    mode: None,
                    size: s.size,
                    section: None,
                }
            }
        }
    }

    fn set_panel(&mut self, kind: PanelKind, patch: PanelPatch) {
        let side = patch.side.as_deref().and_then(DockSide::parse);
        match kind {
            PanelKind::Sidebar => self.model.set_sidebar(SidebarPatch {
                open: patch.open,
                side,
                float: patch.float,
                collapsed: patch.collapsed,
                mode: patch.mode.as_deref().and_then(SidebarMode::parse),
                size: patch.size,
            }),
            PanelKind::Status => self.model.set_status_window(StatusWindowPatch {
                open: patch.open,
                side,
                float: patch.float.map(Some),
                size: patch.size,
            }),
            PanelKind::Settings => self.model.set_settings_panel(SettingsPanelPatch {
                open: patch.open,
                side,
                float: patch.float.map(Some),
                collapsed: patch.collapsed,
                size: patch.size,
                section: patch.section,
            }),
            PanelKind::Help => self.model.set_help_panel(HelpPanelPatch {
                open: patch.open,
                side,
                float: patch.float.map(Some),
                size: patch.size,
            }),
        }
        if self.fullscreen.active() {
            self.with_fullscreen(|fs, host| {
                fs.refit(host);
            });
        }
    }

    fn termd_state(&self) -> String {
        self.termd.state().as_str().to_owned()
    }

    fn termd_dismiss_lost(&mut self) -> bool {
        let dismissed = self.termd.dismiss_lost();
        if dismissed {
            self.model.notify_changed();
        }
        dismissed
    }

    fn view_size(&self) -> (f64, f64) {
        self.model.camera().view()
    }

    fn terminal_screen_box(&self, id: &str) -> Result<ScreenBox, String> {
        let win = self
            .window_rect(id)
            .ok_or_else(|| format!("no terminal '{id}'"))?;
        Ok(self.model.camera().to_screen_box(win))
    }

    fn group_header_screen_box(&self, gid: &str) -> Result<Option<ScreenBox>, String> {
        let zoom = self.model.camera().zoom();
        let view = self.read_groups_view();
        let rect = self
            .groups
            .header_rect(&view, gid, zoom, None)
            .map_err(|e| e.to_string())?;
        Ok(rect.map(|r| self.model.camera().to_screen_box(r)))
    }

    fn open_menu(&mut self, target: CmdMenuTarget, at: Point) -> Result<Value, String> {
        let world = self.model.camera().to_world(at);
        let menu_target = match &target {
            CmdMenuTarget::Terminal { id, part } => {
                if !self.model.is_shown(id) {
                    return Err(if self.model.terminal(id).is_some() {
                        format!("terminal '{id}' is hidden")
                    } else {
                        format!("no terminal '{id}'")
                    });
                }
                let p = match part.as_str() {
                    "title" => TerminalPart::Title,
                    "edge" => TerminalPart::Edge,
                    _ => TerminalPart::Body,
                };
                MenuTarget::Terminal {
                    id: id.clone(),
                    part: p,
                }
            }
            CmdMenuTarget::Group { id } => {
                self.groups.get(id).map_err(|e| e.to_string())?;
                MenuTarget::Group { id: id.clone() }
            }
            CmdMenuTarget::Canvas => MenuTarget::Canvas { world: Some(world) },
        };
        self.model.open_menu(menu_target, at)?;
        Ok(json!({
            "target": target,
            "at": { "x": at.x, "y": at.y },
            "world": { "x": world.x, "y": world.y },
        }))
    }

    fn close_menu(&mut self) {
        self.model.close_menu();
    }

    fn config_entries(&self) -> Vec<(String, Value)> {
        WALL_CONFIG_KEYS
            .iter()
            .filter_map(|&k| self.model.config().get(k).map(|v| (k.to_owned(), knob_to_json(v))))
            .collect()
    }

    fn config_get(&self, key: &str) -> Result<Value, String> {
        self.model
            .config()
            .get(key)
            .map(knob_to_json)
            .ok_or_else(|| {
                let names = WALL_CONFIG_KEYS.join(", ");
                format!("unknown wall knob '{key}' (knobs: {names})")
            })
    }

    fn config_check(&self, key: &str, raw: &str) -> Result<Value, String> {
        commands::check_fallback_knob(key, raw)
    }

    fn config_set(&mut self, key: &str, value: Value) -> Result<Value, String> {
        let val = self.model.set_config(key, json_to_knob_input(value))?;
        self.apply_config_side_effects(key);
        Ok(knob_to_json(val))
    }

    fn config_reset(&mut self, key: &str) -> Result<Value, String> {
        let val = self.model.reset_config(key)?;
        self.apply_config_side_effects(key);
        Ok(knob_to_json(val))
    }

    fn config_search(&self, query: &str) -> Vec<ConfigSearchGroup> {
        commands::search_fallback_knobs(query)
    }

    fn bus_view(&self) -> BusView {
        let Some(ref snap) = self.bus else {
            return BusView {
                snapshot_host: None,
                reason: self.bus_reason.clone(),
                hosts: Vec::new(),
            };
        };
        let hosts = bus_hosts(Some(snap))
            .into_iter()
            .map(|h| BusHostGroup {
                host: h.host,
                agents: h
                    .agents
                    .into_iter()
                    .map(|a| BusAgentEntry {
                        name: a.agent.name.clone(),
                        target: a.target,
                        status: a.agent.status.clone(),
                        end_reason: a.agent.end_reason.clone(),
                        detail: a.agent.detail.clone(),
                        attached: a.agent.attached,
                    })
                    .collect(),
            })
            .collect();
        BusView {
            snapshot_host: snap.host.clone(),
            reason: self.bus_reason.clone(),
            hosts,
        }
    }

    fn terminals_attached_to(&self, target: &str) -> Vec<String> {
        self.model.store().attached_to(target)
    }

    fn attach_agent(&mut self, target: &str) -> Result<AttachOutcome, String> {
        if !is_bus_target(target) {
            return Err(format!("'{target}' isn't an agent name (name or name@host)"));
        }
        let local_name = local_bus_name(target, self.bus.as_ref()).map(str::to_owned);
        let mut aliases = vec![target.to_owned()];
        if let Some(ref ln) = local_name {
            aliases.push(ln.clone());
            if let Some(host) = self.bus.as_ref().and_then(|s| s.host.as_deref()) {
                aliases.push(format!("{ln}@{host}"));
            }
        }
        let open = aliases
            .iter()
            .flat_map(|a| self.model.store().attached_to(a))
            .find(|id| self.model.terminal(id).is_some())
            .or_else(|| {
                local_name
                    .as_deref()
                    .map(gomux_screen_id)
                    .filter(|gx| self.model.terminal(gx).is_some())
            });
        if let Some(id) = open {
            self.show_terminal(&id);
            self.focus(Some(&id), true);
            return Ok(AttachOutcome { id, existing: true });
        }
        let found = find_bus_agent(self.bus.as_ref(), target);
        if let Some(ref a) = found {
            if !bus_attachable(&a.agent) {
                let status = a.agent.status.as_deref().unwrap_or("unknown");
                let why = if status == "ended" {
                    match a.agent.end_reason.as_deref() {
                        Some(r) if !r.is_empty() => format!("ended ({r})"),
                        _ => "ended".to_owned(),
                    }
                } else {
                    status.to_owned()
                };
                return Err(format!("{target} has no live session to attach to ({why})"));
            }
        }
        let argv = attach_argv(target)?;
        let zoom = self.model.camera().zoom();
        let canonical_target = found
            .map(|a| a.target)
            .unwrap_or_else(|| target.to_owned());
        self.pending_spawns.push_back(PendingSpawn {
            window_top_left: None,
            zoom_when_asked: zoom,
            split: None,
            attach_target: Some(canonical_target),
        });
        if self.agents_extra_connected {
            if let Some(ref ln) = local_name {
                let gx_id = gomux_screen_id(ln);
                self.model.push_effect(WallEffect::ReattachTerminal {
                    id: gx_id.clone(),
                    fallback_argv: argv,
                    cols: self.new_terminal_cells().0,
                    rows: self.new_terminal_cells().1,
                });
                self.termd.dismiss_lost();
                self.model.notify_changed();
                return Ok(AttachOutcome {
                    id: gx_id,
                    existing: false,
                });
            }
        }
        self.model.push_effect(WallEffect::SpawnTerminal {
            window_top_left: None,
            argv: Some(argv),
            cols: self.new_terminal_cells().0,
            rows: self.new_terminal_cells().1,
        });
        self.termd.dismiss_lost();
        self.model.notify_changed();
        Ok(AttachOutcome {
            id: local_name
                .as_deref()
                .map(gomux_screen_id)
                .unwrap_or_else(|| "pending".to_owned()),
            existing: false,
        })
    }

    fn agent_set_open(&mut self, id: &str, how: &str) -> Result<AgentOpenResult, String> {
        if self.model.terminal(id).is_none() {
            return Err(format!("no terminal '{id}'"));
        }
        let prev = self.model.store().agent_view(id).cloned();
        let default_side = self.model.config().agent_view_side;
        let cur_open = prev.as_ref().is_some_and(|p| p.open);
        let cur_side = prev.as_ref().map_or(default_side, |p| p.side);
        let (open, side) = match how {
            "toggle" => (!cur_open, cur_side),
            "open" | "on" => (true, cur_side),
            "close" | "off" => (false, cur_side),
            other => match AgentSide::parse(other) {
                Some(s) => (true, s),
                None => {
                    return Err(format!(
                        "agent view takes toggle|open|close|right|left|top|bottom, not '{other}'"
                    ));
                }
            },
        };
        let next = AgentViewState {
            open,
            side,
            session: prev
                .as_ref()
                .map_or_else(|| "auto".to_owned(), |p| p.session.clone()),
            filters: prev.and_then(|p| p.filters),
        };
        let session = (next.session != "auto").then(|| next.session.clone());
        self.model.store_mut().set_agent_view(id, Some(next));
        let owned = id.to_owned();
        self.agent_footprint_changed(&owned);
        self.model.notify_changed();
        Ok(AgentOpenResult {
            open,
            side: side.as_str().to_owned(),
            session,
            missing: None,
        })
    }

    fn agent_set_side(&mut self, id: &str, side: &str) -> Result<AgentSideResult, String> {
        let parsed = AgentSide::parse(side)
            .ok_or_else(|| format!("side takes right, left, top or bottom, not '{side}'"))?;
        let res = self.agent_set_open(id, parsed.as_str())?;
        Ok(AgentSideResult { side: res.side })
    }

    fn agent_set_filter(
        &mut self,
        id: &str,
        mode: &str,
        categories: &[String],
    ) -> Result<Value, String> {
        let parsed = agent_view::FilterMode::parse(mode)?;
        let prev = self.model.store().agent_view(id).cloned();
        let next_filters =
            agent_view::apply_filter_change(prev.as_ref().and_then(|p| p.filters.as_ref()), parsed, categories)?;
        let mut state = prev.unwrap_or(AgentViewState {
            open: true,
            side: self.model.config().agent_view_side,
            session: "auto".to_owned(),
            filters: None,
        });
        state.filters = Some(next_filters.clone());
        self.model.store_mut().set_agent_view(id, Some(state));
        self.model.notify_changed();
        Ok(json!({ "id": id, "filters": next_filters }))
    }

    fn agent_set_bulk_expanded(
        &mut self,
        _id: &str,
        _expanded: bool,
        _categories: &[String],
    ) -> Result<AgentBulkExpandResult, String> {
        Ok(AgentBulkExpandResult { changed: 0 })
    }

    fn agent_toggle_event(
        &mut self,
        _id: &str,
        event_id: &str,
        how: &str,
    ) -> Result<AgentToggleEventResult, String> {
        Ok(AgentToggleEventResult {
            event_id: event_id.to_owned(),
            expanded: how != "off",
        })
    }

    fn agent_show_full_body(&mut self, _id: &str, _event_id: &str) -> Result<(), String> {
        Ok(())
    }

    fn agent_jump(&mut self, id: &str, target: &str) -> Result<AgentJumpResult, String> {
        if self.model.terminal(id).is_some_and(|t| t.modes.alt_screen) {
            let msg = format!(
                "{} is on the alternate screen — primary scrollback jump is unavailable while a full-screen TUI is active",
                self.model.name_of(id)
            );
            self.model.notify(msg.clone(), self.now_ms);
            return Err(msg);
        }
        if target == "live" {
            self.model.push_effect(WallEffect::ScrollTerminalLive {
                id: id.to_owned(),
            });
            self.model.notify_changed();
            return Ok(AgentJumpResult {
                live: true,
                event_id: String::new(),
                offset: 0,
            });
        }
        Err("agent transcript streams are not joined in wall-core yet".to_owned())
    }

    fn agent_load_earlier(&mut self, _id: &str) -> Result<AgentEarlierResult, String> {
        Ok(AgentEarlierResult { loaded: 0 })
    }

    fn palette_open(&self) -> bool {
        self.palette_open
    }

    fn set_palette_open(&mut self, open: bool) {
        if self.palette_open != open {
            self.palette_open = open;
            self.model.notify_changed();
        }
    }

    fn palette_size(&self) -> PanelSize {
        self.model.store().palette_size()
    }

    fn set_palette_size(&mut self, size: PanelSize) {
        self.model.set_palette_size(size);
    }

    fn agent_set_stick_bottom(&mut self, id: &str, mode: &str) -> Result<bool, String> {
        if self.model.terminal(id).is_none() {
            return Err(format!("no terminal '{id}'"));
        }
        let cur = self.agent_stick_bottom.get(id).copied().unwrap_or(true);
        let next = match mode {
            "off" => false,
            "toggle" => !cur,
            "on" | "tail" => true,
            other => {
                return Err(format!(
                    "agent follow takes tail|on|off|toggle, not '{other}'"
                ));
            }
        };
        self.agent_stick_bottom.insert(id.to_owned(), next);
        self.model.notify_changed();
        Ok(next)
    }

    fn relay_endpoint(&self) -> Option<(String, u16)> {
        None
    }

    fn set_relay_token(&mut self, _token: &str) -> Result<(), String> {
        Err("native wall talks to g3d-termd directly (no browser relay)".to_owned())
    }
}

// ── Core `terminal.*` verbs on the shared router ─────────────────────────────

fn register_terminal_commands(router: &mut CommandRouter<WallCore>) {
    router.register(
        "terminal.resize",
        RegisterMeta {
            description: "Request a terminal resize in character cells",
            usage: "<id> <cols> <rows>",
            returns: "",
        },
        |args, core, _meta| {
            let Some(id) = args.first().filter(|s| !s.is_empty()) else {
                return Ok(CommandReply::err("name a terminal"));
            };
            if core.model.terminal(id).is_none() {
                return Ok(CommandReply::err(format!("no terminal '{id}'")));
            }
            let cols: u32 = match args.get(1).and_then(|s| s.parse().ok()) {
                Some(c) if c >= 1 => c,
                _ => return Ok(CommandReply::err("cols must be a positive integer")),
            };
            let rows: u32 = match args.get(2).and_then(|s| s.parse().ok()) {
                Some(r) if r >= 1 => r,
                _ => return Ok(CommandReply::err("rows must be a positive integer")),
            };
            core.model.push_effect(WallEffect::ResizeTerminal {
                id: id.clone(),
                cols,
                rows,
            });
            core.model.notify_changed();
            Ok(CommandReply::ok(
                format!("resized {id} to {cols}x{rows}"),
                Some(json!({ "id": id, "cols": cols, "rows": rows })),
            ))
        },
    );

    router.register(
        "terminal.scroll",
        RegisterMeta {
            description: "Scroll a terminal's scrollback view (+ = up into history, - = down toward live, or `live`)",
            usage: "<id> <lines | live> [cell_x cell_y]",
            returns: "",
        },
        |args, core, _meta| {
            let Some(id) = args.first().filter(|s| !s.is_empty()) else {
                return Ok(CommandReply::err("name a terminal"));
            };
            if core.model.terminal(id).is_none() {
                return Ok(CommandReply::err(format!("no terminal '{id}'")));
            }
            if args.get(1).map(String::as_str) == Some("live") {
                core.model
                    .push_effect(WallEffect::ScrollTerminalLive { id: id.clone() });
                core.model.notify_changed();
                return Ok(CommandReply::ok(
                    format!("terminal '{id}' scroll live"),
                    Some(json!({ "id": id, "live": true, "offset": 0 })),
                ));
            }
            let lines: i32 = match args.get(1).and_then(|s| s.parse().ok()) {
                Some(n) => n,
                None => return Ok(CommandReply::err("lines must be an integer or 'live'")),
            };
            let cx: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let cy: f64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            core.model.push_effect(WallEffect::ScrollTerminal {
                id: id.clone(),
                lines,
                cell: Point::new(cx, cy),
            });
            core.model.notify_changed();
            Ok(CommandReply::ok(
                format!("scrolled {id} by {lines}"),
                Some(json!({ "id": id, "lines": lines, "cell": { "x": cx, "y": cy } })),
            ))
        },
    );

    router.register(
        "terminal.move",
        RegisterMeta {
            description: "Place a terminal at world coordinates (x, y) and remember the spot",
            usage: "<id> <x> <y> [z]",
            returns: "",
        },
        |args, core, _meta| {
            let Some(id) = args.first().filter(|s| !s.is_empty()) else {
                return Ok(CommandReply::err("name a terminal"));
            };
            if core.model.terminal(id).is_none() {
                return Ok(CommandReply::err(format!("no terminal '{id}'")));
            }
            let x: f64 = match args.get(1).and_then(|s| s.parse::<f64>().ok()) {
                Some(v) if v.is_finite() => v,
                _ => return Ok(CommandReply::err("x must be a number")),
            };
            let y: f64 = match args.get(2).and_then(|s| s.parse::<f64>().ok()) {
                Some(v) if v.is_finite() => v,
                _ => return Ok(CommandReply::err("y must be a number")),
            };
            core.model.set_terminal_position(id, Point::new(x, y));
            core.model.remember_position(id);
            let owned = id.clone();
            let _ = core.with_groups(|g, host| g.relayout_of(host, &owned));
            core.model.notify_changed();
            Ok(CommandReply::ok(
                format!("moved {id} to ({x}, {y})"),
                Some(json!({ "id": id, "x": x, "y": y })),
            ))
        },
    );

    router.register(
        "terminal.focus",
        RegisterMeta {
            description: "Focus a terminal so keyboard input reaches it",
            usage: "<id>",
            returns: "",
        },
        |args, core, _meta| {
            let Some(id) = args.first().filter(|s| !s.is_empty()) else {
                return Ok(CommandReply::err("name a terminal"));
            };
            if !core.focus(Some(id), false) {
                return Ok(CommandReply::err(format!("no terminal '{id}'")));
            }
            Ok(CommandReply::ok(
                format!("focused {id}"),
                Some(json!({ "id": id })),
            ))
        },
    );

    router.register(
        "terminal.spawn",
        RegisterMeta {
            description: "Request a new terminal session (optional `-- argv...`)",
            usage: "[-- argv…]",
            returns: "",
        },
        |args, core, _meta| {
            let argv = args
                .iter()
                .position(|a| a == "--")
                .map(|idx| args[idx + 1..].to_vec())
                .filter(|v| !v.is_empty());
            let zoom = core.model.camera().zoom();
            core.pending_spawns.push_back(PendingSpawn {
                window_top_left: None,
                zoom_when_asked: zoom,
                split: None,
                attach_target: None,
            });
            core.model.push_effect(WallEffect::SpawnTerminal {
                window_top_left: None,
                argv: argv.clone(),
                cols: core.new_terminal_cells().0,
                rows: core.new_terminal_cells().1,
            });
            core.model.notify_changed();
            Ok(CommandReply::ok(
                "spawn requested",
                Some(json!({ "pending": true, "argv": argv })),
            ))
        },
    );

    router.register(
        "terminal.kill",
        RegisterMeta {
            description: "Kill a terminal session on the daemon",
            usage: "<id>",
            returns: "",
        },
        |args, core, _meta| {
            let Some(id) = args.first().filter(|s| !s.is_empty()) else {
                return Ok(CommandReply::err("name a terminal"));
            };
            Ok(core.close_terminal(id))
        },
    );

    router.register(
        "terminal.refresh",
        RegisterMeta {
            description: "Ask the daemon to repaint a terminal",
            usage: "<id>",
            returns: "",
        },
        |args, core, _meta| {
            let Some(id) = args.first().filter(|s| !s.is_empty()) else {
                return Ok(CommandReply::err("name a terminal"));
            };
            if core.model.terminal(id).is_none() {
                return Ok(CommandReply::err(format!("no terminal '{id}'")));
            }
            core.model
                .push_effect(WallEffect::RefreshTerminal { id: id.clone() });
            Ok(CommandReply::ok(
                format!("refreshed {id}"),
                Some(json!({ "id": id })),
            ))
        },
    );
}

// ── PointerBridge: connects WallPointer to CommandRouter<WallCore> ───────────

struct PointerBridge<'a, 's> {
    router: &'a mut CommandRouter<WallCore>,
    screens: Option<&'s dyn ScreenLookup>,
}

impl PointerHost for PointerBridge<'_, '_> {
    fn config(&self) -> PointerConfig {
        self.router.context.pointer_config()
    }

    fn camera_pos(&self) -> Point {
        let p = self.router.context.model.camera().pose();
        Point::new(p.x, p.y)
    }

    fn camera_zoom(&self) -> f64 {
        self.router.context.model.camera().zoom()
    }

    fn viewport_height(&self) -> f64 {
        self.router.context.model.camera().view().1
    }

    fn to_world(&self, screen_x: f64, screen_y: f64) -> Point {
        self.router
            .context
            .model
            .camera()
            .to_world(Point::new(screen_x, screen_y))
    }

    fn focused_id(&self) -> Option<&str> {
        self.router.context.model.focused_id()
    }

    fn fullscreen(&self) -> Option<PointerFullscreenState> {
        let core = &self.router.context;
        if !core.fullscreen.active() {
            return None;
        }
        match core.fullscreen.mode()? {
            FullscreenMode::Single => Some(PointerFullscreenState::Single {
                id: core.fullscreen.id()?.to_owned(),
            }),
            FullscreenMode::Group => {
                let fs_view = core.read_fs_view();
                let group_layout = core
                    .fullscreen
                    .group_layout(&fs_view)
                    .unwrap_or(GroupLayout::Tile);
                let elevated_ids = core.fullscreen.elevated_ids(&fs_view);
                Some(PointerFullscreenState::Group {
                    group_layout,
                    elevated_ids,
                })
            }
        }
    }

    fn hit(&self, world: Point) -> Option<CanvasHit> {
        self.router.context.hit(world)
    }

    fn has_terminal(&self, id: &str) -> bool {
        self.router.context.model.terminal(id).is_some()
    }

    fn is_shown(&self, id: &str) -> bool {
        self.router.context.model.is_shown(id)
    }

    fn terminal_position(&self, id: &str) -> Option<Point> {
        self.router.context.model.terminal(id).map(|t| t.position)
    }

    fn terminal_size(&self, id: &str) -> Option<GridSize> {
        self.router
            .context
            .model
            .terminal(id)
            .map(|t| GridSize::new(t.cols, t.rows))
    }

    fn terminal_stride(&self, id: &str) -> Option<Point> {
        let entry = self.router.context.model.terminal(id)?;
        let geom = self.router.context.model.geometry();
        Some(Point::new(
            geom.stride.x * entry.scale,
            geom.stride.y * entry.scale,
        ))
    }

    fn terminal_rect(&self, id: &str) -> Option<Rect> {
        self.router.context.model.terminal_rect(id)
    }

    fn window_rect(&self, id: &str) -> Option<Rect> {
        self.router.context.window_rect(id)
    }

    fn canvas_order(&self) -> Vec<String> {
        self.router.context.canvas_order()
    }

    fn cell_at(&self, id: &str, screen_x: f64, screen_y: f64) -> Option<CellPoint> {
        self.router.context.cell_at(id, screen_x, screen_y)
    }

    fn mouse_tracking(&self, id: &str) -> MouseTracking {
        self.router.context.model.mouse_tracking(id)
    }

    fn has_selection(&self, id: &str) -> bool {
        self.router
            .context
            .selections
            .get(id)
            .is_some_and(|s| !s.is_empty())
    }

    fn has_selection_clipboard(&self) -> bool {
        !self.router.context.model.selection_clipboard().is_empty()
    }

    fn has_group(&self, group_id: &str) -> bool {
        self.router.context.groups.group(group_id).is_some()
    }

    fn group_of(&self, id: &str) -> Option<String> {
        self.router.context.groups.group_of(id)
    }

    fn is_tiled_member(&self, id: &str) -> bool {
        self.router.context.groups.tiles(id)
    }

    fn is_split_member(&self, id: &str) -> bool {
        self.router.context.groups.splits(id)
    }

    fn shown_group_members(&self, group_id: &str) -> Vec<String> {
        let core = &self.router.context;
        let view = core.read_groups_view();
        core.groups
            .shown_members(&view, group_id)
            .unwrap_or_default()
    }

    fn split_edge(&self, id: &str, edges: Edges) -> Option<SplitEdge> {
        let core = &self.router.context;
        let view = core.read_groups_view();
        let target = core.groups.split_edge(&view, id, edges).ok()?;
        if let Some(div) = target.divider {
            Some(SplitEdge::Divider {
                group: target.gid,
                path: div.path,
                axis: div.axis,
            })
        } else {
            target.outer.map(|outer_edges| SplitEdge::Outer {
                group: target.gid,
                edges: outer_edges,
            })
        }
    }

    fn split_rect(&self, group_id: &str) -> Option<Rect> {
        let core = &self.router.context;
        let view = core.read_groups_view();
        core.groups.split_rect(&view, group_id).ok()
    }

    fn cancel_camera_ease(&mut self) {
        let pose = self.router.context.model.camera().pose();
        if self.router.context.model.camera().target().is_some() {
            self.router.context.model.camera_mut().set_pose(pose);
        }
    }

    fn camera_move_to(&mut self, x: f64, y: f64) {
        self.router.context.model.camera_mut().move_to(x, y);
        self.router.context.model.notify_changed();
    }

    fn camera_pan_by(&mut self, dx_px: f64, dy_px: f64) {
        self.router.context.model.camera_mut().pan_by(dx_px, dy_px);
        self.router.context.model.notify_changed();
    }

    fn camera_zoom_at(&mut self, screen_x: f64, screen_y: f64, factor: f64) {
        self.router
            .context
            .model
            .camera_mut()
            .zoom_at(Point::new(screen_x, screen_y), factor);
        self.router.context.model.notify_changed();
    }

    fn camera_wheel_zoom_at(&mut self, screen_x: f64, screen_y: f64, dy_px: f64) {
        self.router
            .context
            .model
            .camera_mut()
            .wheel_zoom_at(Point::new(screen_x, screen_y), dy_px);
        self.router.context.model.notify_changed();
    }

    fn fit_all(&mut self) {
        self.router.context.fit_all();
    }

    fn focus(&mut self, id: Option<&str>, frame: bool) {
        self.router.context.focus(id, frame);
    }

    fn set_hovered(&mut self, id: Option<&str>) {
        self.router.context.model.set_hovered(id);
    }

    fn set_hovered_group(&mut self, group_id: Option<&str>) {
        self.router.context.set_hovered_group(group_id);
    }

    fn set_cursor(&mut self, cursor: CursorIcon) {
        self.router.context.set_cursor(cursor);
    }

    fn bring_to_front(&mut self, id: &str) {
        self.router.context.model.bring_to_front(id);
        self.router.context.apply_z_order();
    }

    fn bring_group_to_front(&mut self, group_id: &str) {
        self.router.context.model.bring_group_to_front(group_id);
        self.router.context.apply_z_order();
    }

    fn set_terminal_position(&mut self, id: &str, pos: Point) {
        self.router.context.model.set_terminal_position(id, pos);
    }

    fn remember_position(&mut self, id: &str) {
        self.router.context.model.remember_position(id);
    }

    fn clear_selection(&mut self, id: &str) {
        self.router.context.clear_text_selection(id);
    }

    fn select_start(
        &mut self,
        id: &str,
        cell_x: f64,
        cell_y: f64,
        mode: SelectionMode,
        block: bool,
    ) {
        let Some(screen) = self.screens.and_then(|s| s.screen(id)) else {
            return;
        };
        let core = &mut self.router.context;
        let word_chars = core.model.config().selection_word_chars.clone();
        let sel = core
            .selections
            .entry(id.to_owned())
            .or_insert_with(|| TerminalSelection::with_word_chars(&word_chars));
        sel.start(screen, cell_x, cell_y, mode, block);
        let text = sel.text(screen);
        let non_empty = !sel.is_empty();
        if non_empty {
            core.selection_text_cache.insert(id.to_owned(), text);
        } else {
            core.selection_text_cache.remove(id);
        }
        core.model.set_terminal_has_selection(id, non_empty);
    }

    fn select_extend(&mut self, id: &str, cell_x: f64, cell_y: f64) {
        let Some(screen) = self.screens.and_then(|s| s.screen(id)) else {
            return;
        };
        let core = &mut self.router.context;
        if let Some(sel) = core.selections.get_mut(id) {
            if sel.extend(screen, cell_x, cell_y) {
                let text = sel.text(screen);
                let non_empty = !sel.is_empty();
                if non_empty {
                    core.selection_text_cache.insert(id.to_owned(), text);
                } else {
                    core.selection_text_cache.remove(id);
                }
                core.model.set_terminal_has_selection(id, non_empty);
            }
        }
    }

    fn selection_made(&mut self, id: &str) {
        let text = if let Some(screens) = self.screens {
            self.router.context.refresh_selection_text(id, screens)
        } else {
            self.router
                .context
                .selection_text_cache
                .get(id)
                .cloned()
                .unwrap_or_default()
        };
        self.router.context.model.selection_made(&text);
    }

    fn set_lasso(&mut self, points: Option<&[Point]>) {
        self.router.context.lasso_screen = points.map(|pts| pts.to_vec());
        self.router.context.model.notify_changed();
    }

    fn send_mouse(&mut self, id: &str, event: TerminalMouseEvent) {
        self.router.context.model.push_effect(WallEffect::SendMouse {
            id: id.to_owned(),
            event,
        });
    }

    fn run_command(&mut self, args: &[String]) {
        let _ = self.router.execute(args.to_vec());
    }

    fn group_drag_start(&mut self, group_id: &str) {
        let drag = {
            let view = self.router.context.read_groups_view();
            self.router.context.groups.drag_start(&view, group_id).ok()
        };
        self.router.context.active_group_drag = drag;
    }

    fn group_drag_to(&mut self, group_id: &str, dx: f64, dy: f64) {
        let core = &mut self.router.context;
        let Some(mut drag) = core.active_group_drag.take() else {
            return;
        };
        if drag.gid == group_id {
            core.with_groups(|g, host| g.drag_to(host, &mut drag, dx, dy));
        }
        core.active_group_drag = Some(drag);
    }

    fn group_drag_end(&mut self, group_id: &str) {
        let core = &mut self.router.context;
        if let Some(drag) = core.active_group_drag.take() {
            if drag.gid == group_id {
                core.with_groups(|g, host| g.drag_end(host, &drag));
            }
        }
    }

    fn drop_tiled_member(&mut self, id: &str) {
        let owned = id.to_owned();
        let _ = self
            .router
            .context
            .with_groups(|g, host| g.drop_member(host, &owned));
    }

    fn relayout_member_group(&mut self, id: &str) {
        let owned = id.to_owned();
        let _ = self
            .router
            .context
            .with_groups(|g, host| g.relayout_of(host, &owned));
    }

    fn tile_drop(&mut self, id: &str) -> Option<DropPreview> {
        let core = &self.router.context;
        let view = core.read_groups_view();
        core.groups
            .tile_drop(&view, id)
            .map(|drop| DropPreview {
                rect: drop.rect,
                ok: true,
            })
    }

    fn pane_drop(&mut self, id: &str, world: Point) -> Option<DropPreview> {
        let core = &self.router.context;
        let view = core.read_groups_view();
        core.groups
            .pane_drop(&view, id, world)
            .map(|drop| DropPreview {
                rect: drop.rect,
                ok: drop.fits,
            })
    }

    fn drop_pane(&mut self, id: &str, world: Point) {
        let owned = id.to_owned();
        let _ = self
            .router
            .context
            .with_groups(|g, host| g.drop_pane(host, &owned, world));
    }

    fn drag_divider(&mut self, group_id: &str, path: &[BranchSide], coord: f64) {
        let gid = group_id.to_owned();
        let path_vec = path.to_vec();
        let _ = self
            .router
            .context
            .with_groups(|g, host| g.drag_divider(host, &gid, &path_vec, coord));
    }

    fn resize_split(&mut self, group_id: &str, rect: Rect) {
        let gid = group_id.to_owned();
        let _ = self
            .router
            .context
            .with_groups(|g, host| g.resize_split(host, &gid, rect));
    }
}

// ── Public Wall composition root ─────────────────────────────────────────────

/// The wall as one self-contained object, with no window, no renderer, and no daemon.
pub struct Wall {
    router: CommandRouter<WallCore>,
    pointer: WallPointer,
}

impl Wall {
    /// Construct a wall backed by an existing [`WallStore`].
    pub fn new(store: WallStore, opts: WallOptions) -> Self {
        let core = WallCore::new(store, opts);
        let mut router = CommandRouter::new(core);
        router.use_middleware(|_name, _args, core, _meta| {
            core.model.notify_changed();
        });
        register_terminal_commands(&mut router);
        commands::register_wall_commands(&mut router);
        Self {
            router,
            pointer: WallPointer::new(),
        }
    }

    /// Open a disk-backed wall whose durable state lives at `<store_dir>/wall-<scope>.json`.
    pub fn open(store_dir: impl Into<PathBuf>, scope: impl Into<String>, opts: WallOptions) -> Self {
        let store = WallStore::open(store_dir, scope);
        Self::new(store, opts)
    }

    /// Construct a wall backed by an in-memory [`MemoryStorage`] (for tests and ephemeral runs).
    pub fn in_memory(opts: WallOptions) -> Self {
        let store = WallStore::with_storage(Box::new(MemoryStorage::new()), "memory");
        Self::new(store, opts)
    }

    fn sync_pointer_flags(&mut self) {
        if self.router.context.pending_quench_wheel {
            self.router.context.pending_quench_wheel = false;
            self.pointer.quench_wheel(self.router.context.now_ms);
        }
        if self.router.context.pending_cancel_scroll {
            self.router.context.pending_cancel_scroll = false;
            self.pointer.cancel_scroll();
        }
    }

    // ── Viewport & clock ─────────────────────────────────────────────────────

    /// Update the logical viewport size in px and display scale factor.
    pub fn set_viewport(&mut self, width: f64, height: f64, scale_factor: f64) {
        let w = width.max(1.0);
        let h = height.max(1.0);
        let sf = scale_factor.max(0.1);
        let core = &mut self.router.context;
        let (prev_w, prev_h) = core.model.camera().view();
        if (prev_w - w).abs() < 1e-9
            && (prev_h - h).abs() < 1e-9
            && (core.scale_factor - sf).abs() < 1e-9
        {
            return;
        }
        core.model.camera_mut().set_view(w, h);
        core.scale_factor = sf;
        if core.fullscreen.active() {
            core.with_fullscreen(|fs, host| {
                fs.refit(host);
            });
        }
        core.model.notify_changed();
        self.sync_pointer_flags();
    }

    /// Update cell stride/padding after a font family, size, or line-height change,
    /// re-wrapping every terminal's grid to keep its panel footprint and re-laying out groups.
    pub fn set_geometry_preserving_panels(&mut self, geometry: CellGeometry) {
        let core = &mut self.router.context;
        let old = core.model.geometry();
        if old == geometry {
            return;
        }
        core.model.set_geometry_preserving_panels(geometry);
        if core.fullscreen.active() {
            core.with_fullscreen(|fs, host| {
                fs.rescale_saved_canvas_for_geometry(host, old, geometry);
                fs.refit(host);
            });
        }
    }

    /// Advance the wall's monotonic clock (`now_ms`) before out-of-band actions
    /// (`run`, `notify`, `key`, `deliver_clipboard`) on an idle window so timed
    /// notices and hold timers do not stamp a stale pre-idle timestamp.
    pub fn set_now_ms(&mut self, now_ms: f64) {
        if now_ms.is_finite() {
            self.router.context.now_ms = self.router.context.now_ms.max(now_ms);
        }
    }

    /// Show a timed notice line at the top of the wall (expires after `config.notice_ms`).
    pub fn notify(&mut self, message: &str) {
        let now_ms = self.router.context.now_ms;
        self.router.context.model.notify(message, now_ms);
    }

    /// Advance the wall's clock to `now_ms` and show a timed notice line for `config.notice_ms`.
    pub fn notify_at(&mut self, message: &str, now_ms: f64) {
        self.set_now_ms(now_ms);
        self.notify(message);
    }

    /// Advance camera easing, wheel scroll glides, fullscreen refit settlement,
    /// hold-to-close timers, daemon restart grace timers, and timed notices.
    /// Returns `true` if another animation frame should be scheduled.
    pub fn tick(&mut self, dt_secs: f64, now_ms: f64) -> bool {
        self.router.context.now_ms = now_ms;
        self.router.context.advance_termd_clock(dt_secs, now_ms);
        let cam_moving = self.router.context.model.step_camera_and_notice(dt_secs, now_ms);
        {
            let mut bridge = PointerBridge {
                router: &mut self.router,
                screens: None,
            };
            self.pointer.step(&mut bridge, dt_secs);
        }
        self.router.context.with_fullscreen(|fs, host| {
            fs.tick(host, now_ms);
        });
        let holding = self.router.context.tick_close_hold();
        self.sync_pointer_flags();
        cam_moving || self.pointer.gliding() || self.router.context.fullscreen.settling() || holding
    }

    /// Update the measured monospace character advances used to size title bars,
    /// group headers, and map-label pills (e.g. after loading or switching fonts).
    pub fn set_char_widths(&mut self, widths: CharWidths) {
        let next = widths.sanitized();
        if self.router.context.char_widths != next {
            self.router.context.char_widths = next;
            self.router.context.model.notify_changed();
        }
    }

    /// Measured monospace character advances currently configured on this wall.
    pub fn char_widths(&self) -> CharWidths {
        self.router.context.char_widths
    }

    // ── Pointer, wheel, pinch & context menu ─────────────────────────────────

    /// Handle a pointer button press.
    pub fn pointer_down(&mut self, event: &PointerEvent, screens: &dyn ScreenLookup) {
        self.router.context.now_ms = event.time_ms.max(self.router.context.now_ms);
        self.router.context.last_pointer_screen = Some(Point::new(event.x, event.y));
        self.router.context.last_pointer_mods = event.mods;
        self.router.context.model.close_menu();
        if event.button == Some(Button::Left)
            && is_link_open_mods(self.router.context.platform, event.mods)
        {
            if let Some((tid, link)) = self
                .router
                .context
                .link_at_screen_point(event.x, event.y, screens)
            {
                self.router.context.pending_link_click =
                    Some((tid.clone(), Point::new(event.x, event.y), link.url.clone()));
                self.router.context.set_hovered_link(Some((tid, link)));
                self.sync_pointer_flags();
                self.router.context.model.notify_changed();
                return;
            }
        }
        self.router.context.pending_link_click = None;
        self.router.context.set_hovered_link(None);
        {
            let mut bridge = PointerBridge {
                router: &mut self.router,
                screens: Some(screens),
            };
            self.pointer.on_down(&mut bridge, event);
        }
        self.sync_pointer_flags();
        self.router.context.model.notify_changed();
    }

    /// Handle pointer movement (hover cursor, drags, lasso, text selection, mouse tracking, links).
    pub fn pointer_move(&mut self, event: &PointerEvent, screens: &dyn ScreenLookup) {
        self.router.context.now_ms = event.time_ms.max(self.router.context.now_ms);
        self.router.context.last_pointer_screen = Some(Point::new(event.x, event.y));
        self.router.context.last_pointer_mods = event.mods;
        if let Some((_, start, _)) = &self.router.context.pending_link_click {
            if (event.x - start.x).hypot(event.y - start.y) > DRAG_SLOP_PX {
                self.router.context.pending_link_click = None;
            }
        }
        {
            let mut bridge = PointerBridge {
                router: &mut self.router,
                screens: Some(screens),
            };
            self.pointer.on_move(&mut bridge, event);
        }
        if !self.pointer.busy() {
            let hit_label = self.router.context.hit_map_label(event.x, event.y);
            self.router.context.set_hovered_label(hit_label.as_deref());
            self.router.context.update_hovered_link(event.mods, screens);
        } else {
            self.router.context.set_hovered_link(None);
        }
        self.sync_pointer_flags();
    }

    /// Handle a pointer button release.
    pub fn pointer_up(&mut self, event: &PointerEvent, screens: &dyn ScreenLookup) {
        self.router.context.now_ms = event.time_ms.max(self.router.context.now_ms);
        self.router.context.last_pointer_screen = Some(Point::new(event.x, event.y));
        self.router.context.last_pointer_mods = event.mods;
        if event.button == Some(Button::Left) {
            if let Some((tid, start, url)) = self.router.context.pending_link_click.take() {
                if (event.x - start.x).hypot(event.y - start.y) <= DRAG_SLOP_PX
                    && is_allowed_url_scheme(&url)
                    && self.router.context.model.terminal(&tid).is_some()
                {
                    self.router
                        .context
                        .model
                        .push_effect(WallEffect::OpenUrl { url });
                }
                self.router.context.update_hovered_link(event.mods, screens);
                self.sync_pointer_flags();
                self.router.context.model.notify_changed();
                return;
            }
        }
        {
            let mut bridge = PointerBridge {
                router: &mut self.router,
                screens: Some(screens),
            };
            self.pointer.on_up(&mut bridge, event);
        }
        if !self.pointer.busy() {
            self.router.context.update_hovered_link(event.mods, screens);
        }
        self.sync_pointer_flags();
        self.router.context.model.notify_changed();
    }

    /// Re-evaluate hovered link underline and cursor when modifier keys change
    /// without pointer movement (e.g. pressing or releasing `⌘` / `Ctrl` over a URL).
    pub fn modifiers_changed(&mut self, mods: Mods, screens: &dyn ScreenLookup) {
        if !self.pointer.busy() {
            self.router.context.update_hovered_link(mods, screens);
        } else {
            self.router.context.last_pointer_mods = mods;
            self.router.context.set_hovered_link(None);
        }
    }

    /// Handle window focus loss (`WindowEvent::Focused(false)` / browser `blur` &
    /// `visibilitychange`): clears tracked pressed keys, cancels any held close chord,
    /// cancels any in-flight pointer gesture and buffered wheel scroll glide, and clears
    /// transient hover highlights while preserving terminal keyboard focus (`focused_id`).
    pub fn blur(&mut self) {
        self.router.context.pressed_keys.clear();
        self.router.context.cancel_close_hold();
        self.router.context.pending_link_click = None;
        self.router.context.last_pointer_mods = Mods::default();
        {
            let mut bridge = PointerBridge {
                router: &mut self.router,
                screens: None,
            };
            self.pointer.cancel_active(&mut bridge);
        }
        self.router.context.model.set_hovered(None);
        self.router.context.set_hovered_group(None);
        self.router.context.set_hovered_label(None);
        self.router.context.set_hovered_link(None);
        self.sync_pointer_flags();
        self.router.context.model.notify_changed();
    }

    /// Handle pointer leaving the window (`WindowEvent::CursorLeft`): clears passive
    /// terminal, group-header, map-label, and hyperlink hover highlights and restores
    /// `CursorIcon::Default` when no pointer drag is in flight. Returns `true` if any
    /// hover state changed.
    pub fn pointer_leave(&mut self) -> bool {
        if self.pointer.busy() {
            return false;
        }
        let had_hover = self.router.context.model.hovered_id().is_some()
            || self.router.context.hovered_group_id.is_some()
            || self.router.context.hovered_label_id.is_some()
            || self.router.context.hovered_link.is_some()
            || self.router.context.cursor != CursorIcon::Default;
        if !had_hover {
            return false;
        }
        self.router.context.model.set_hovered(None);
        self.router.context.set_hovered_group(None);
        self.router.context.set_hovered_label(None);
        self.router.context.set_hovered_link(None);
        self.router.context.set_cursor(CursorIcon::Default);
        self.sync_pointer_flags();
        self.router.context.model.notify_changed();
        true
    }

    /// Handle pointer cancellation (window blur, capture lost).
    pub fn pointer_cancel(&mut self, event: &PointerEvent) {
        self.router.context.now_ms = event.time_ms.max(self.router.context.now_ms);
        self.router.context.pending_link_click = None;
        {
            let mut bridge = PointerBridge {
                router: &mut self.router,
                screens: None,
            };
            self.pointer.on_cancel(&mut bridge, event);
        }
        self.router.context.set_hovered_group(None);
        self.router.context.set_hovered_label(None);
        self.router.context.set_hovered_link(None);
        self.sync_pointer_flags();
        self.router.context.model.notify_changed();
    }

    /// Handle a double-click (frames a terminal on its title bar, a group on its
    /// header, or fits all on empty canvas).
    pub fn pointer_double_click(&mut self, event: &PointerEvent) {
        self.router.context.now_ms = event.time_ms.max(self.router.context.now_ms);
        {
            let mut bridge = PointerBridge {
                router: &mut self.router,
                screens: None,
            };
            self.pointer.on_double_click(&mut bridge, event);
        }
        self.sync_pointer_flags();
    }

    /// Handle a pointer press on terminal `id`'s map label (click focuses + frames;
    /// drag moves the terminal).
    pub fn press_label(&mut self, id: &str, event: &PointerEvent) {
        self.router.context.now_ms = event.time_ms.max(self.router.context.now_ms);
        self.router.context.model.close_menu();
        {
            let mut bridge = PointerBridge {
                router: &mut self.router,
                screens: None,
            };
            self.pointer.press_label(&mut bridge, id, event);
        }
        self.sync_pointer_flags();
        self.router.context.model.notify_changed();
    }

    /// Handle a mouse wheel or trackpad scroll event (pans, zooms with a zoom key,
    /// or scrolls the focused terminal).
    pub fn wheel(&mut self, event: &WheelEvent) {
        self.router.context.now_ms = event.time_ms.max(self.router.context.now_ms);
        self.router.context.model.close_menu();
        {
            let mut bridge = PointerBridge {
                router: &mut self.router,
                screens: None,
            };
            self.pointer.on_wheel(&mut bridge, event);
        }
        self.sync_pointer_flags();
        self.router.context.model.notify_changed();
    }

    /// Handle a trackpad pinch gesture zooming by `factor` about logical screen
    /// point `(screen_x, screen_y)`. Ignored while fullscreen is active.
    pub fn pinch(&mut self, screen_x: f64, screen_y: f64, factor: f64) {
        self.router.context.model.close_menu();
        if self.router.context.fullscreen.active() {
            return;
        }
        self.router
            .context
            .model
            .camera_mut()
            .zoom_at(Point::new(screen_x, screen_y), factor);
        self.router.context.model.notify_changed();
    }

    /// Evaluate and act on a right-click / context-menu request at logical screen
    /// coordinates `(screen_x, screen_y)` (`WallMenu.js:57-79`).
    pub fn context_menu(
        &mut self,
        screen_x: f64,
        screen_y: f64,
        mods: Mods,
    ) -> ContextDecision {
        let world = self
            .router
            .context
            .model
            .camera()
            .to_world(Point::new(screen_x, screen_y));
        let (target, mouse_reporting, focused) = if let Some(fs_id) =
            self.router.context.fullscreen.id().map(str::to_owned)
        {
            // Solo fullscreen: every click on the canvas (including edge padding) targets the
            // fullscreen terminal body (`WallMenu.js:62-64`).
            let mr = self.router.context.model.mouse_tracking(&fs_id) != MouseTracking::None;
            let foc = self.router.context.model.focused_id() == Some(fs_id.as_str());
            (
                Some(MenuTarget::Terminal {
                    id: fs_id,
                    part: TerminalPart::Body,
                }),
                mr,
                foc,
            )
        } else if let Some(fs_gid) = self.router.context.fullscreen.group_id().map(str::to_owned) {
            // Group fullscreen: hitting a pane targets that terminal; empty padding or divider
            // targets the fullscreen group (`WallMenu.js:66-69`).
            match self.router.context.hit(world) {
                Some(CanvasHit::Terminal { id, part }) => {
                    let tp = match part {
                        HitPart::Body => TerminalPart::Body,
                        HitPart::Title => TerminalPart::Title,
                        HitPart::Edge(_) => TerminalPart::Edge,
                    };
                    let mr = self.router.context.model.mouse_tracking(&id) != MouseTracking::None;
                    let foc = self.router.context.model.focused_id() == Some(id.as_str());
                    (Some(MenuTarget::Terminal { id, part: tp }), mr, foc)
                }
                _ => (Some(MenuTarget::Group { id: fs_gid }), false, false),
            }
        } else if let Some(label_id) = self.router.context.hit_map_label(screen_x, screen_y) {
            // Zoomed-out map label pill sits above the canvas and targets its terminal's title
            // even when the pill extends over empty canvas (`WallMenu.js:74-75`).
            let mr = self.router.context.model.mouse_tracking(&label_id) != MouseTracking::None;
            let foc = self.router.context.model.focused_id() == Some(label_id.as_str());
            (
                Some(MenuTarget::Terminal {
                    id: label_id,
                    part: TerminalPart::Title,
                }),
                mr,
                foc,
            )
        } else {
            let hit = self.router.context.hit(world);
            match &hit {
                Some(CanvasHit::Terminal { id, part }) => {
                    let tp = match part {
                        HitPart::Body => TerminalPart::Body,
                        HitPart::Title => TerminalPart::Title,
                        HitPart::Edge(_) => TerminalPart::Edge,
                    };
                    (
                        Some(MenuTarget::Terminal {
                            id: id.clone(),
                            part: tp,
                        }),
                        self.router.context.model.mouse_tracking(id) != MouseTracking::None,
                        self.router.context.model.focused_id() == Some(id.as_str()),
                    )
                }
                Some(CanvasHit::GroupHeader { group }) => (
                    Some(MenuTarget::Group { id: group.clone() }),
                    false,
                    false,
                ),
                Some(CanvasHit::Divider { group, .. }) => (
                    Some(MenuTarget::Group { id: group.clone() }),
                    false,
                    false,
                ),
                None => (
                    Some(MenuTarget::Canvas { world: Some(world) }),
                    false,
                    false,
                ),
            }
        };
        let decision = menu::context_policy(&ContextEvent {
            shift_key: mods.shift,
            editable: self.router.context.field_focus == FieldFocus::Input,
            chrome: false,
            menu_open: self.router.context.model.menu().is_some(),
            keyboard: false,
            target: target.clone(),
            mouse_reporting,
            focused,
        });
        match decision.verdict {
            Verdict::Ours => {
                if let Some(t) = target {
                    let _ = self
                        .router
                        .context
                        .model
                        .open_menu(t, Point::new(screen_x, screen_y));
                }
            }
            Verdict::Native => {
                if self.router.context.model.menu().is_some() {
                    self.router.context.model.close_menu();
                }
            }
            Verdict::Program => {}
        }
        decision
    }

    // ── Keyboard & clipboard ─────────────────────────────────────────────────

    /// Handle a keyboard press or release (`WallKeys` + `WallPointer::on_key`).
    pub fn key(&mut self, event: &KeyEvent, screens: &dyn ScreenLookup) -> KeyDecision {
        {
            let mut bridge = PointerBridge {
                router: &mut self.router,
                screens: Some(screens),
            };
            self.pointer.on_key(&mut bridge, event);
        }

        // Escape dismisses open wall overlays before reaching terminals.
        if event.pressed
            && event.code == "Escape"
            && !event.mods.ctrl
            && !event.mods.alt
            && !event.mods.meta
        {
            if self.router.context.model.menu().is_some() {
                self.router.context.model.close_menu();
                return KeyDecision::Chord {
                    command: None,
                    blur_field: false,
                    release_close_hold: true,
                };
            }
            if self.router.context.palette_open {
                self.router.context.palette_open = false;
                self.router.context.model.notify_changed();
                return KeyDecision::Chord {
                    command: None,
                    blur_field: false,
                    release_close_hold: true,
                };
            }
        }

        let decision = {
            let core = &mut self.router.context;
            let focused_id = core.model.focused_id().map(str::to_owned);
            let focused_has_selection = focused_id
                .as_deref()
                .and_then(|id| core.selections.get(id))
                .is_some_and(|s| !s.is_empty());
            let focused_group = focused_id
                .as_deref()
                .and_then(|id| core.groups.group_of(id));
            let focused_in_split_or_tile = focused_id
                .as_deref()
                .is_some_and(|id| core.groups.splits(id) || core.groups.tiles(id));
            let ctx = KeyContext {
                menu_open: core.model.menu().is_some(),
                field_focus: core.field_focus,
                fullscreen: core.fullscreen.active(),
                focused_id: focused_id.as_deref(),
                focused_has_selection,
                focused_group: focused_group.as_deref(),
                focused_in_split_or_tile,
                has_wall_selection: !core.model.selection().is_empty(),
                option_as_alt: core.model.config().option_as_alt,
                selection_clear_on_typing: core.model.config().selection_clear_on_typing,
                down_terminal: None,
            };
            let platform = core.platform;
            core.pressed_keys.decide(platform, event, &ctx)
        };

        if decision.releases_close_hold() {
            self.router.context.cancel_close_hold();
        }

        match &decision {
            KeyDecision::Chord { command, .. } => {
                if let Some(cmd) = command {
                    match cmd {
                        WallCommand::Verb(argv) => {
                            let _ = self.router.execute(argv.clone());
                        }
                        WallCommand::CycleFocus(dir) => {
                            self.router.context.cycle_focus(dir.step());
                        }
                        WallCommand::CycleGroupFocus(dir) => {
                            self.router.context.cycle_group_focus(dir.step());
                        }
                        WallCommand::StepFocusHistory(dir) => {
                            self.router.context.step_focus_history(dir.step());
                        }
                        WallCommand::Spawn => {
                            let _ = self.router.execute(["wall.spawn"]);
                        }
                        WallCommand::FitAll => {
                            self.router.context.fit_all();
                        }
                        WallCommand::HoldClose(id) => {
                            self.router.context.start_close_hold(id);
                        }
                        WallCommand::OpenMenu => {
                            if let Some(id) = self.router.context.model.focused_id().map(str::to_owned) {
                                let _ = self.router.execute(["wall.menu", "terminal", &id]);
                            } else {
                                let _ = self.router.execute(["wall.menu", "canvas"]);
                            }
                        }
                    }
                }
            }
            KeyDecision::Copy { terminal } => {
                let target = terminal
                    .clone()
                    .or_else(|| self.router.context.model.focused_id().map(str::to_owned));
                if let Some(id) = target {
                    let _ = self.router.context.refresh_selection_text(&id, screens);
                    let _ = self.router.execute(["wall.copy", &id]);
                }
            }
            KeyDecision::Palette => {
                self.router.context.palette_open = !self.router.context.palette_open;
                self.router.context.model.notify_changed();
            }
            KeyDecision::TerminalKey {
                terminal,
                key,
                clear_selection,
                ..
            } => {
                if *clear_selection {
                    self.router.context.clear_text_selection(terminal);
                }
                self.router.context.model.push_effect(WallEffect::SendKey {
                    id: terminal.clone(),
                    key: key.clone(),
                });
            }
            KeyDecision::Pass { .. } => {
                // Native paste chord (Cmd+V on macOS, Ctrl+Shift+V elsewhere) asks the host
                // for clipboard text when no UI text field has focus.
                let core = &mut self.router.context;
                if event.pressed
                    && core.field_focus == FieldFocus::None
                    && is_paste_chord(core.platform, event)
                {
                    if let Some(focused) = core.model.focused_id().map(str::to_owned) {
                        core.model
                            .push_effect(WallEffect::ReadClipboard { target: focused });
                    }
                }
            }
        }
        if !self.pointer.busy() {
            self.router.context.update_hovered_link(event.mods, screens);
        }
        self.sync_pointer_flags();
        decision
    }

    /// Deliver pasted text directly into the focused terminal (e.g. from a window paste event).
    pub fn paste(&mut self, text: &str) {
        let Some(id) = self.router.context.model.focused_id().map(str::to_owned) else {
            return;
        };
        self.deliver_clipboard(&id, text);
    }

    /// Answer a [`WallEffect::ReadClipboard`] request for terminal `target_id`.
    pub fn deliver_clipboard(&mut self, target_id: &str, text: &str) {
        if text.is_empty() {
            let now = self.router.context.now_ms;
            self.router
                .context
                .model
                .notify("paste: the clipboard holds no text", now);
            return;
        }
        if self.router.context.model.terminal(target_id).is_none() {
            return;
        }
        if self.router.context.model.config().selection_clear_on_typing {
            self.router.context.clear_text_selection(target_id);
        }
        self.router
            .context
            .model
            .push_effect(WallEffect::PasteTerminal {
                id: target_id.to_owned(),
                text: text.to_owned(),
            });
    }

    // ── News from terminals & daemon ─────────────────────────────────────────

    /// Notify the wall that a terminal has appeared (either spawned or adopted).
    pub fn terminal_appeared(&mut self, info: TerminalAppeared) -> LandedAs {
        let landed = self.router.context.terminal_appeared(info);
        self.sync_pointer_flags();
        landed
    }

    /// Notify the wall that terminal `id`'s grid size changed (`EVENT size` / `Repaint`).
    pub fn terminal_resized(&mut self, id: &str, cols: u32, rows: u32) {
        self.router.context.terminal_resized(id, cols, rows);
        self.sync_pointer_flags();
    }

    /// Notify the wall that terminal `id`'s OSC window title changed.
    pub fn terminal_title(&mut self, id: &str, title: impl Into<String>) {
        self.router.context.model.set_terminal_title(id, title);
    }

    /// Notify the wall that terminal `id`'s emulator modes changed (mouse tracking,
    /// alternate screen, bracketed paste, etc.).
    pub fn terminal_modes(&mut self, id: &str, modes: Modes) {
        self.router.context.model.set_terminal_modes(id, modes);
    }

    /// Update terminal `id`'s last non-blank preview line (for palette/sidebar).
    pub fn terminal_preview(&mut self, id: &str, preview: impl Into<String>) {
        self.router.context.model.set_terminal_preview(id, preview);
    }

    /// Notify the wall that terminal `id`'s [`Screen`] was refilled by its emulator.
    ///
    /// Maintains text-selection lifetime ([`TerminalSelection::sync_after_fill`]),
    /// refreshes the terminal's last non-blank preview line when cells changed, and
    /// propagates `screen.title` when the emulator's title changed (covering adopted
    /// terminals whose initial `Repaint` sets `Screen::title`).
    ///
    /// Returns `true` when any wall state changed (selection shifted/cleared,
    /// preview changed, or title changed).
    pub fn terminal_screen_filled(
        &mut self,
        id: &str,
        screen: &Screen,
        delta: ScreenFillDelta<'_>,
    ) -> bool {
        self.router
            .context
            .terminal_screen_filled(id, screen, delta)
    }

    /// Notify the wall that terminal `id` ended or closed on the daemon.
    pub fn terminal_ended(&mut self, id: &str) {
        self.router.context.terminal_ended(id);
        self.sync_pointer_flags();
    }

    /// Reconcile durable store entries against the daemon's live terminal list on connect.
    /// Call before adopting existing terminals with [`Self::terminal_appeared`] so dead
    /// group members are pruned before surviving groups lay out. Returns the forgotten ids.
    pub fn set_live_terminals<S: AsRef<str>>(&mut self, live_ids: &[S]) -> Vec<String> {
        let gone = self.router.context.set_live_terminals(live_ids);
        self.sync_pointer_flags();
        gone
    }

    /// Record whether the daemon connection is up. Independent of [`Self::set_live_terminals`]
    /// and [`Self::terminal_appeared`]; calling `set_connected(true)` after adopting existing
    /// terminals keeps `WallSnapshot::connected` false until adoption finishes.
    pub fn set_connected(&mut self, connected: bool) {
        self.router.context.model.set_connected(connected);
    }

    /// Record a `WELCOME` handshake from `g3d-termd` (`instance` and advertised `features`).
    pub fn on_termd_welcome<S: AsRef<str>>(
        &mut self,
        instance: &str,
        features: &[S],
        now_ms: u64,
    ) {
        self.router.context.record_termd_now_ms(now_ms);
        self.router
            .context
            .termd
            .on_welcome(instance, features, now_ms);
        self.router.context.model.notify_changed();
    }

    /// Ingest a `termd` status document pushed by the daemon.
    pub fn on_termd_status(&mut self, value: Value, now_ms: u64) -> Result<(), String> {
        self.router.context.record_termd_now_ms(now_ms);
        self.router.context.termd.on_status_json(value, now_ms)?;
        self.router.context.model.notify_changed();
        Ok(())
    }

    /// Notify that the `termd` status connection dropped (`Restarting` or clean `Absent`).
    pub fn on_termd_disconnected(&mut self, planned_restart: bool, now_ms: u64) {
        self.router.context.record_termd_now_ms(now_ms);
        self.router
            .context
            .termd
            .on_disconnected(planned_restart, now_ms);
        self.router.context.model.notify_changed();
    }

    /// Notify that the `termd` connection ended for good, with the client's reason: `Lost`
    /// at once when the namespace left on purpose with shells in it, else as
    /// [`Self::on_termd_disconnected`] ([`TermdStatus::on_gone`]).
    pub fn on_termd_gone(&mut self, reason: &str, now_ms: u64) {
        self.router.context.record_termd_now_ms(now_ms);
        self.router.context.termd.on_gone(reason, now_ms);
        self.router.context.model.notify_changed();
    }

    /// Notify that nothing listens on the namespace socket (`Absent`, or `Lost` if live shells vanished).
    pub fn on_termd_socket_absent(&mut self, now_ms: u64) {
        self.router.context.record_termd_now_ms(now_ms);
        self.router.context.termd.on_socket_absent(now_ms);
        self.router.context.model.notify_changed();
    }

    /// Notify that the holder accepted the socket while waiting for a brain (`Restarting`).
    pub fn on_termd_holder_waiting(&mut self, now_ms: u64) {
        self.router.context.record_termd_now_ms(now_ms);
        self.router.context.termd.on_holder_waiting(now_ms);
        self.router.context.model.notify_changed();
    }

    /// Notify that the holder reported `brain_failed` (`Failed`).
    pub fn on_termd_brain_failed(&mut self, now_ms: u64) {
        self.router.context.record_termd_now_ms(now_ms);
        self.router.context.termd.on_brain_failed(now_ms);
        self.router.context.model.notify_changed();
    }

    /// Notify that the daemon socket path is unreachable or inaccessible (`Unreachable`).
    pub fn on_termd_socket_unreachable(&mut self, now_ms: u64) {
        self.router.context.record_termd_now_ms(now_ms);
        self.router.context.termd.on_socket_unreachable(now_ms);
        self.router.context.model.notify_changed();
    }

    /// Dismiss any sticky `termd lost` notice banner. Returns `true` if one was active.
    pub fn dismiss_termd_lost(&mut self) -> bool {
        let dismissed = self.router.context.termd.dismiss_lost();
        if dismissed {
            self.router.context.model.notify_changed();
        }
        dismissed
    }

    /// Update the gomuxai agent bus snapshot.
    pub fn set_bus_snapshot(
        &mut self,
        snapshot: Option<GomuxSnapshot>,
        reason: impl Into<String>,
    ) {
        self.router.context.bus = snapshot;
        self.router.context.bus_reason = reason.into();
        self.router.context.model.notify_changed();
    }

    /// Record whether the local `agents` extra `termd` namespace is currently connected
    /// (`--wall-extra-ns agents`), so `wall.attach` prefers direct local subscription (`gx-<name>`)
    /// over spawning an `agent attach` shell wrapper.
    pub fn set_agents_extra_connected(&mut self, connected: bool) {
        self.router.context.agents_extra_connected = connected;
    }

    /// Whether the local `agents` extra `termd` namespace is marked connected.
    pub fn agents_extra_connected(&self) -> bool {
        self.router.context.agents_extra_connected
    }

    // ── Verbs ────────────────────────────────────────────────────────────────

    /// Execute a verb line or argv through the command router (as the CLI,
    /// palette, menus, and panels all do).
    pub fn run<'a, I: Into<CommandInput<'a>>>(&mut self, cmd: I) -> CommandReply {
        let reply = self.router.execute(cmd);
        self.sync_pointer_flags();
        reply
    }

    /// Execute a verb line or argv with live screen access (refreshing active
    /// text selections before `wall.copy` reads them).
    pub fn run_with_screens<'a, I: Into<CommandInput<'a>>>(
        &mut self,
        cmd: I,
        screens: &dyn ScreenLookup,
    ) -> CommandReply {
        self.router.context.refresh_all_selection_texts(screens);
        self.run(cmd)
    }

    /// All registered verbs and their metadata (feeds the Help panel).
    pub fn list_commands(&self) -> Vec<CommandMeta> {
        self.router.list_commands()
    }

    // ── Effects & frame description ──────────────────────────────────────────

    /// Drain every pending host side effect emitted since the last drain.
    pub fn drain_effects(&mut self) -> Vec<WallEffect> {
        self.router.context.model.drain_effects()
    }

    /// Build the plain-data [`WallFrame`] describing everything to draw this frame.
    pub fn frame(&mut self) -> WallFrame {
        let drop_preview = self.pointer.drop_preview();
        let gliding = self.pointer.gliding();
        self.router.context.build_frame(drop_preview, gliding)
    }

    // ── Read-only inspection & UI state ──────────────────────────────────────

    pub fn camera(&self) -> &WallCamera {
        self.router.context.model.camera()
    }

    pub fn config(&self) -> &WallConfig {
        self.router.context.model.config()
    }

    pub fn store(&self) -> &WallStore {
        self.router.context.model.store()
    }

    pub fn model(&self) -> &WallModel {
        &self.router.context.model
    }

    pub fn groups(&self) -> &Groups {
        &self.router.context.groups
    }

    pub fn fullscreen(&self) -> &WallFullscreen {
        &self.router.context.fullscreen
    }

    pub fn pointer(&self) -> &WallPointer {
        &self.pointer
    }

    pub fn termd(&self) -> &TermdStatus {
        &self.router.context.termd
    }

    /// Host platform conventions configured on this wall (`Platform::Mac` or `Platform::Other`).
    pub fn platform(&self) -> Platform {
        self.router.context.platform
    }

    /// Latest gomuxai agent bus snapshot set via [`Self::set_bus_snapshot`], if any.
    pub fn bus_snapshot(&self) -> Option<&GomuxSnapshot> {
        self.router.context.bus.as_ref()
    }

    /// Diagnostic reason paired with [`Self::bus_snapshot`] when gomuxai is unavailable.
    pub fn bus_reason(&self) -> &str {
        &self.router.context.bus_reason
    }

    /// Projected [`crate::sidebar::BusSnapshot`] ready for the sidebar and command palette panels.
    pub fn sidebar_bus_snapshot(&self) -> crate::sidebar::BusSnapshot {
        let Some(snap) = self.router.context.bus.as_ref() else {
            return crate::sidebar::BusSnapshot {
                host: String::new(),
                reason: self.router.context.bus_reason.clone(),
                agents: Vec::new(),
            };
        };
        crate::sidebar::BusSnapshot {
            host: snap.host.clone().unwrap_or_default(),
            reason: self.router.context.bus_reason.clone(),
            agents: snap
                .agents
                .iter()
                .map(|a| crate::sidebar::BusAgentInput {
                    name: a.name.clone(),
                    host: a.host.clone().unwrap_or_default(),
                    live: a.live,
                    status: a.status.clone().unwrap_or_default(),
                    end_reason: a.end_reason.clone().unwrap_or_default(),
                    attached: a.attached,
                    detail: a.detail.clone().unwrap_or_default(),
                    cwd: a.cwd.clone().unwrap_or_default(),
                })
                .collect(),
        }
    }

    /// Whether terminal `id`'s Agent View scroll list is currently pinned to its live tail
    /// (`wall.agent.follow`; defaults to `true`).
    pub fn agent_stick_bottom(&self, id: &str) -> bool {
        self.router
            .context
            .agent_stick_bottom
            .get(id)
            .copied()
            .unwrap_or(true)
    }

    pub fn cursor(&self) -> CursorIcon {
        self.router.context.cursor
    }

    pub fn focused_id(&self) -> Option<&str> {
        self.router.context.model.focused_id()
    }

    pub fn hovered_id(&self) -> Option<&str> {
        self.router.context.model.hovered_id()
    }

    /// Currently hovered group header ID, if any.
    pub fn hovered_group_id(&self) -> Option<&str> {
        self.router.context.hovered_group_id.as_deref()
    }

    pub fn selection(&self) -> &[String] {
        self.router.context.model.selection()
    }

    pub fn terminal(&self, id: &str) -> Option<&TerminalEntry> {
        self.router.context.model.terminal(id)
    }

    pub fn terminal_rect(&self, id: &str) -> Option<Rect> {
        self.router.context.model.terminal_rect(id)
    }

    pub fn window_rect(&self, id: &str) -> Option<Rect> {
        self.router.context.window_rect(id)
    }

    pub fn terminal_selection(&self, id: &str) -> Option<&TerminalSelection> {
        self.router.context.selections.get(id)
    }

    /// Read terminal `id`'s currently selected text directly against live `screens`.
    pub fn selection_text(&self, id: &str, screens: &dyn ScreenLookup) -> String {
        let Some(sel) = self.router.context.selections.get(id) else {
            return String::new();
        };
        if sel.is_empty() {
            return String::new();
        }
        screens
            .screen(id)
            .map(|s| sel.text(s))
            .unwrap_or_else(|| {
                self.router
                    .context
                    .selection_text_cache
                    .get(id)
                    .cloned()
                    .unwrap_or_default()
            })
    }

    pub fn name_of(&self, id: &str) -> String {
        self.router.context.model.name_of(id)
    }

    pub fn title_of(&self, id: &str) -> String {
        self.router.context.title_of(id)
    }

    pub fn menu(&self) -> Option<&OpenMenu> {
        self.router.context.model.menu()
    }

    pub fn notice(&self) -> Option<&str> {
        self.router.context.model.notice()
    }

    /// Current daemon outage / compatibility notice computed via [`crate::status::notice_for`].
    pub fn status_notice(&self) -> Option<StatusNotice> {
        self.router.context.status_notice()
    }

    pub fn palette_open(&self) -> bool {
        self.router.context.palette_open
    }

    pub fn set_palette_open(&mut self, open: bool) {
        if self.router.context.palette_open != open {
            self.router.context.palette_open = open;
            self.router.context.model.notify_changed();
        }
    }

    /// Tell the wall whether an egui text field or panel control currently holds keyboard focus.
    pub fn set_field_focus(&mut self, focus: FieldFocus) {
        self.router.context.field_focus = focus;
    }

    /// Set terminal `id`'s visual scale factor (`> 0`).
    pub fn set_terminal_scale(&mut self, id: &str, scale: f64) -> bool {
        self.router.context.model.set_terminal_scale(id, scale)
    }

    /// Hit-test world point `world` against terminals, dividers, and group headers.
    pub fn hit(&self, world: Point) -> Option<CanvasHit> {
        self.router.context.hit(world)
    }

    /// Immutable state snapshot for egui chrome panels (`WallModel::snapshot` shape,
    /// reflecting live fullscreen state).
    pub fn snapshot(&self) -> WallSnapshot {
        let mut snap = self.router.context.model.snapshot();
        snap.fullscreen = self.router.context.fullscreen.target().map(str::to_owned);
        snap
    }
}

/// Format a zoom key chord using [`chord::zoom_wheel_label`] for UI displays.
pub fn zoom_wheel_label(platform: Platform, zoom_keys: &str) -> Result<String, String> {
    chord::zoom_wheel_label(platform, zoom_keys)
}

/// Re-export [`keymap::help_sections`] helper convenience for panels.
pub fn help_sections(
    platform: Platform,
    zoom_keys: &str,
    verbs: &[CommandMeta],
) -> Result<Vec<keymap::HelpGroup>, String> {
    let help_verbs: Vec<keymap::HelpVerb<'_>> = verbs
        .iter()
        .map(|v| keymap::HelpVerb {
            name: &v.name,
            usage: &v.usage,
            description: &v.description,
        })
        .collect();
    keymap::help_sections(platform, zoom_keys, &help_verbs)
}
