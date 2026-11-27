//! The wall's central state and rules (design port of `wall/TerminalWall.js`),
//! with no window, no GPU, no DOM, and no wire socket.
//!
//! [`WallModel`] owns the wall's durable [`WallStore`], its [`WallCamera`], its
//! born and dialled [`WallConfig`], the live terminals currently mounted on the
//! canvas, arrival order and Z-stacking order, focus and 64-deep focus history,
//! reading-order and directional pane navigation, hit testing with the 7 px
//! border-grab band, new-terminal placement (remembered spot, active
//! viewport-anchored flow grid, or fresh dock-aware viewport origin) and
//! pending split landings, hide/show/reveal/close lifecycle, multi-terminal
//! selection (Shift-click and polygon lasso), two-pass framing with the 28 px
//! row-height cap, timed notices, chrome [`WallSnapshot`] generation, and live
//! session reconciliation (`reconcile_live`).
//!
//! ### Host seam
//! Rather than invoking asynchronous callbacks or holding references to a
//! daemon client or renderer, mutating methods update [`WallModel`]
//! synchronously, bump a monotonic [`WallModel::generation`] counter (so egui
//! chrome can rebuild [`WallModel::snapshot`] only when state changed), and
//! push typed [`WallEffect`] commands onto an internal outbox drained via
//! [`WallModel::drain_effects`]. Time (`now_ms`, `dt_secs`), viewport size, and
//! cell metrics ([`CellGeometry`]) are passed in as plain values.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex};

use crate::camera::{CameraPose, WallCamera};
use crate::config::{CopyOnSelect, GroupLayout, WallConfig};
use crate::dock::dock_layout;
use crate::geom::{Point, Rect};
use crate::keymap::TerminalKey;
use crate::layout::{
    cycle, free_flow_slot, hit_test, lasso_touches, pad_rect, reading_order, tile_layout,
    union_rect, z_stack_layout, Hit, HitPart, HitTarget, ReadingItem, Size, TileOptions,
    ZStackOptions, BAR_ROOM, DEFAULT_FIT_MARGIN,
};
use crate::menu::MenuTarget;
use crate::mouse::MouseTracking;
use crate::pointer::{CursorIcon, TerminalMouseEvent};
use crate::settings::{KnobInput, KnobValue};
use crate::snap::{snap_step, snap_to, GridSpec};
use crate::split::{self, Direction, Divider, InsertSide, PaneMap, SplitDir};
use crate::store::{
    HelpPanelPatch, HelpPanelState, SettingsPanelPatch, SettingsPanelState, SidebarPatch,
    SidebarState, StatusWindowPatch, StatusWindowState, Storage, WallGroup, WallSplit, WallStore,
};

/// Screen-space half-width of the border resize grab band in pixels (`EDGE_GRAB_PX`).
pub const EDGE_GRAB_PX: f64 = 7.0;

/// Maximum on-screen row height in pixels when framing a single terminal (`FRAME_MAX_LINE_PX`).
pub const FRAME_MAX_LINE_PX: f64 = 28.0;

/// Target on-screen height of a terminal title bar or group header in pixels (`TitleBar.TITLE_BAR_PX`).
pub const TITLE_BAR_PX: f64 = 22.0;

/// Viewport fill fraction used by [`WallModel::fit_all`] and [`WallModel::frame_group`].
pub const FIT_ALL_MARGIN: f64 = 0.9;

/// Maximum number of entries retained in focus navigation history.
pub const FOCUS_HISTORY_CAP: usize = 64;

/// Minimum width in character cells of a split pane (`WallGroups.MIN_PANE.cols`).
pub const MIN_PANE_COLS: u32 = 20;

/// Minimum height in character rows of a split pane (`WallGroups.MIN_PANE.rows`).
pub const MIN_PANE_ROWS: u32 = 5;

const EDGE_EPS: f64 = 1e-6;

/// Shared in-memory [`Storage`] implementation for tests and ephemeral walls.
///
/// Cloning a [`MemoryStorage`] shares the same underlying byte map, so opening a
/// second [`WallStore`] over a clone simulates reloading persisted state across
/// process restarts without touching the filesystem.
#[derive(Clone, Debug, Default)]
pub struct MemoryStorage {
    files: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
}

impl MemoryStorage {
    pub fn new() -> MemoryStorage {
        MemoryStorage::default()
    }
}

impl Storage for MemoryStorage {
    fn load(&self, scope: &str) -> io::Result<Option<Vec<u8>>> {
        let guard = self
            .files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(guard.get(scope).cloned())
    }

    fn save(&self, scope: &str, bytes: &[u8]) -> io::Result<()> {
        let mut guard = self
            .files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.insert(scope.to_owned(), bytes.to_vec());
        Ok(())
    }
}

/// World-space cell stride, panel padding, and Z thickness shared by terminals on the wall.
///
/// A terminal's `position` is the top-left corner `(min_x, max_y)` of its body
/// panel (cells + `padding` on every side); cell `(0, 0)` starts at
/// `(position.x + padding.x, position.y - padding.y)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellGeometry {
    /// World width and height of one character cell (`stride.x`, `stride.y`).
    pub stride: Point,
    /// Panel padding around the cell grid on each side in world units.
    pub padding: Point,
    /// Depth scale passed to [`z_stack_layout`] (`2.0` in JS `TerminalGrid`).
    pub scale_z: f64,
}

impl CellGeometry {
    pub const fn new(stride_x: f64, stride_y: f64) -> CellGeometry {
        CellGeometry {
            stride: Point::new(stride_x, stride_y),
            padding: Point::new(0.0, 0.0),
            scale_z: 2.0,
        }
    }

    pub const fn with_padding(
        stride_x: f64,
        stride_y: f64,
        pad_x: f64,
        pad_y: f64,
    ) -> CellGeometry {
        CellGeometry {
            stride: Point::new(stride_x, stride_y),
            padding: Point::new(pad_x, pad_y),
            scale_z: 2.0,
        }
    }
}

impl Default for CellGeometry {
    fn default() -> CellGeometry {
        CellGeometry::new(0.53, 1.0)
    }
}

/// Parameters describing a newly arriving terminal session to mount on the wall.
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalSpec {
    pub id: String,
    pub cols: u32,
    pub rows: u32,
    pub shell_title: String,
}

impl TerminalSpec {
    pub fn new(id: impl Into<String>, cols: u32, rows: u32) -> TerminalSpec {
        TerminalSpec {
            id: id.into(),
            cols,
            rows,
            shell_title: String::new(),
        }
    }
}

/// Live state of one terminal mounted on the wall.
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalEntry {
    pub id: String,
    pub cols: u32,
    pub rows: u32,
    /// Top-left corner `(min_x, max_y)` of the terminal's body panel in world units (y up).
    pub position: Point,
    /// Per-terminal scale factor (`1.0` default).
    pub scale: f64,
    /// World Z coordinate assigned by [`z_stack_layout`].
    pub z: f64,
    /// Raw window title set by the running program (`OSC 0` / `OSC 2`).
    pub shell_title: String,
    /// Last non-blank screen line shown in sidebar/palette previews.
    pub preview: String,
    /// Whether this terminal currently holds a non-empty text selection.
    pub has_selection: bool,
    /// Whether the program inside this terminal has enabled DECSET mouse tracking.
    pub mouse_reporting: bool,
    /// Latest terminal modes reported by the emulator/daemon (mouse tracking, alt screen, etc.).
    pub modes: term_cells::Modes,
    /// Split-pane title-bar room override (`None` = `BAR_ROOM`, `Some(0.0)` = hidden bar).
    pub bar_room_override: Option<f64>,
}

impl TerminalEntry {
    /// Top-left corner of cell `(0, 0)` in world space (inside `geometry.padding`).
    pub fn cell_origin(&self, geometry: CellGeometry) -> Point {
        let s = self.scale;
        Point::new(
            self.position.x + geometry.padding.x * s,
            self.position.y - geometry.padding.y * s,
        )
    }

    /// World-space rectangle of the terminal's body panel (cells + padding).
    pub fn body_rect(&self, geometry: CellGeometry) -> Rect {
        let s = self.scale;
        let width =
            (f64::from(self.cols) * geometry.stride.x + 2.0 * geometry.padding.x) * s;
        let height =
            (f64::from(self.rows) * geometry.stride.y + 2.0 * geometry.padding.y) * s;
        Rect::from_top_left(self.position.x, self.position.y, width, height)
    }

    /// Effective DECSET mouse-tracking mode for this terminal.
    pub fn mouse_tracking(&self) -> MouseTracking {
        let from_modes = MouseTracking::from(self.modes.mouse);
        if from_modes != MouseTracking::None {
            from_modes
        } else if self.mouse_reporting {
            MouseTracking::Normal
        } else {
            MouseTracking::None
        }
    }
}

/// Direction in which a terminal pane is split when spawning a new sibling (`'right' | 'down'`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SplitAxis {
    Right,
    Down,
}

impl SplitAxis {
    pub const fn as_str(self) -> &'static str {
        match self {
            SplitAxis::Right => "right",
            SplitAxis::Down => "down",
        }
    }

    const fn split_dir(self) -> SplitDir {
        match self {
            SplitAxis::Right => SplitDir::Row,
            SplitAxis::Down => SplitDir::Col,
        }
    }
}

/// How a newly mounted terminal landed on the wall (`TerminalWall._terminalHost().landed`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LandedAs {
    /// Focused and framed on the canvas (either at a remembered spot or a flow slot).
    Canvas,
    /// Inserted into split group `group_id` beside `at`.
    Split {
        group_id: String,
        at: String,
        axis: SplitAxis,
    },
}

/// Side effect requested of the host environment (window, `termd`, renderer, OS clipboard).
#[derive(Clone, Debug, PartialEq)]
pub enum WallEffect {
    /// Ask `termd` to create a new terminal session (optionally placed at `window_top_left` once it arrives).
    SpawnTerminal {
        window_top_left: Option<Point>,
        argv: Option<Vec<String>>,
        cols: u32,
        rows: u32,
    },
    /// Ask the host to subscribe directly to extra-namespace terminal `id` (e.g. `gx-<name>` via
    /// `Sessions::reattach`), falling back to spawning `fallback_argv` in the primary namespace
    /// when `id` is not hosted in the extra namespace.
    ReattachTerminal {
        id: String,
        fallback_argv: Vec<String>,
        cols: u32,
        rows: u32,
    },
    /// Ask `termd` to kill terminal `id` (its store state has already been forgotten).
    KillTerminal { id: String },
    /// Ask `termd` to resize terminal `id` to `cols × rows` cells.
    ResizeTerminal { id: String, cols: u32, rows: u32 },
    /// Ask `termd` to deliver a key event to terminal `id`.
    SendKey { id: String, key: TerminalKey },
    /// Ask `termd` to paste `text` into terminal `id`.
    PasteTerminal { id: String, text: String },
    /// Ask `termd` to deliver a DECSET mouse event to terminal `id`.
    SendMouse { id: String, event: TerminalMouseEvent },
    /// Ask `termd` to scroll terminal `id`'s view by `lines` (positive = up into scrollback
    /// history, negative = down toward live output, matching `terminalScroll.js` and
    /// `terminal.scroll`) at `cell`.
    ScrollTerminal { id: String, lines: i32, cell: Point },
    /// Ask `termd` to return terminal `id`'s scrollback view immediately to live (`View::Live`, offset 0).
    ScrollTerminalLive { id: String },
    /// Ask `termd` to send a full repaint for terminal `id`.
    RefreshTerminal { id: String },
    /// Copy `text` onto the OS system clipboard (`wall.copy` or `copyOnSelect == "clipboard"`).
    WriteClipboard { text: String },
    /// Ask the host to read the OS system clipboard and call `Wall::deliver_clipboard(target, text)`.
    ReadClipboard { target: String },
    /// Ask the host OS to open a scheme-checked external URL (`http`, `https`, or `mailto`).
    OpenUrl { url: String },
    /// Ask the window to set the pointer's cursor shape.
    SetCursor { cursor: CursorIcon },
    /// A configuration knob changed or reset; renderer/emulator state may sync to it.
    ConfigKnobChanged { key: String, value: KnobValue },
    /// Visible canvas or UI chrome state changed; schedule a frame redraw.
    Redraw,
}

/// Open right-click context menu state (`TerminalWall.menu`).
#[derive(Clone, Debug, PartialEq)]
pub struct OpenMenu {
    pub target: MenuTarget,
    /// Viewport-local screen coordinates in pixels (y down).
    pub at: Point,
    /// World point under `at` when the menu opened.
    pub world: Point,
}

/// What sits under a world point on the wall (`TerminalWall.hit`).
#[derive(Clone, Debug, PartialEq)]
pub enum CanvasHit {
    /// A terminal's border band, title bar, or cell body.
    Terminal(Hit),
    /// A group's header strip above its frame.
    GroupHeader { group_id: String },
    /// The divider gap between panes of a split group.
    SplitDivider { group_id: String, divider: Divider },
}

/// Summary of one terminal inside [`WallSnapshot`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalSnapshot {
    pub id: String,
    pub name: String,
    pub named: bool,
    pub attached: Option<String>,
    pub cols: u32,
    pub rows: u32,
    pub preview: String,
    pub hidden: bool,
    pub group: Option<String>,
    pub selected: bool,
}

/// Summary of one group inside [`WallSnapshot`] (`members` lists only terminals currently on the wall).
#[derive(Clone, Debug, PartialEq)]
pub struct GroupSnapshot {
    pub id: String,
    pub name: String,
    pub named: bool,
    pub layout: GroupLayout,
    pub columns: u32,
    pub gap: f64,
    pub bars: Option<bool>,
    pub members: Vec<String>,
}

/// Immutable snapshot of the wall's state for egui chrome panels and `wall.list` (`TerminalWall.snapshot`).
#[derive(Clone, Debug, PartialEq)]
pub struct WallSnapshot {
    pub connected: bool,
    pub focused_id: Option<String>,
    pub selection: Vec<String>,
    pub sidebar: SidebarState,
    pub status_window: StatusWindowState,
    pub settings_panel: SettingsPanelState,
    pub help_panel: HelpPanelState,
    pub fullscreen: Option<String>,
    pub config: WallConfig,
    pub dialled: Vec<String>,
    pub menu: Option<OpenMenu>,
    pub notice: Option<String>,
    pub terminals: Vec<TerminalSnapshot>,
    pub groups: Vec<GroupSnapshot>,
}

/// Viewport-anchored flow grid reused across consecutive unpositioned spawns while the camera stays put.
#[derive(Clone, Debug, PartialEq)]
struct FlowAnchor {
    origin: Point,
    camera: Point,
    anchor_id: Option<String>,
    spot: Point,
}

/// Pending split waiting for the next spawned terminal to arrive (`TerminalWall._splitLanding`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingSplit {
    at: String,
    axis: SplitAxis,
}

/// Strip a leading `user@host:` prompt prefix from a shell window title (`shortTitle` in `TerminalWall.js`).
pub fn short_title(title: &str) -> &str {
    if let Some((before_colon, after_colon)) = title.split_once(':') {
        if let Some((user, host)) = before_colon.split_once('@') {
            let valid_user = !user.is_empty()
                && !user.chars().any(|ch| ch.is_whitespace() || ch == '@');
            let valid_host = !host.is_empty() && !host.chars().any(char::is_whitespace);
            if valid_user && valid_host {
                return after_colon.trim();
            }
        }
    }
    title.trim()
}

fn group_numeral(gid: &str) -> u64 {
    gid.strip_prefix("g-")
        .and_then(|digits| digits.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Extract the last non-blank line from visible screen lines for `TerminalSnapshot::preview`.
pub fn preview_from_lines<S: AsRef<str>>(lines: &[S]) -> String {
    for line in lines.iter().rev() {
        let trimmed = line.as_ref().trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    String::new()
}

/// Central pure-Rust model of the terminal wall.
pub struct WallModel {
    store: WallStore,
    camera: WallCamera,
    born_config: WallConfig,
    config: WallConfig,
    rejected_dialled: Vec<(String, String)>,
    geometry: CellGeometry,
    connected: bool,
    terminals: BTreeMap<String, TerminalEntry>,
    order: Vec<String>,
    z_order: Vec<String>,
    group_z: HashMap<String, f64>,
    focused_id: Option<String>,
    focus_history: Vec<String>,
    focus_index: Option<usize>,
    stepping_history: bool,
    hovered_id: Option<String>,
    selection: Vec<String>,
    solo_fullscreen_id: Option<String>,
    pre_fullscreen_camera: Option<CameraPose>,
    menu: Option<OpenMenu>,
    selection_clipboard: String,
    notice: Option<String>,
    notice_expires_at_ms: Option<f64>,
    split_landing: Option<PendingSplit>,
    flow: Option<FlowAnchor>,
    pub(crate) agent_views: crate::agent_views::AgentViews,
    generation: u64,
    dirty: bool,
    effects: Vec<WallEffect>,
}

impl WallModel {
    /// Build a wall over `store`, `camera`, `born_config`, and `geometry`.
    ///
    /// Loads any remembered knob overrides from `store.config()`; entries that
    /// no longer pass [`crate::config::check_config`] are cleared from `store`
    /// and recorded in [`Self::rejected_dialled`] (`TerminalWall._dialled`).
    pub fn new(
        mut store: WallStore,
        camera: WallCamera,
        born_config: WallConfig,
        geometry: CellGeometry,
    ) -> WallModel {
        let mut config = born_config.clone();
        let rejected = config.apply_dialled(&store.config().clone());
        for (bad_key, _) in &rejected {
            store.clear_config(bad_key);
        }
        let agent_views =
            crate::agent_views::AgentViews::from_store(&store, config.agent_view_side);

        WallModel {
            store,
            camera,
            born_config,
            config,
            rejected_dialled: rejected,
            geometry,
            connected: false,
            terminals: BTreeMap::new(),
            order: Vec::new(),
            z_order: Vec::new(),
            group_z: HashMap::new(),
            focused_id: None,
            focus_history: Vec::new(),
            focus_index: None,
            stepping_history: false,
            hovered_id: None,
            selection: Vec::new(),
            solo_fullscreen_id: None,
            pre_fullscreen_camera: None,
            menu: None,
            selection_clipboard: String::new(),
            notice: None,
            notice_expires_at_ms: None,
            split_landing: None,
            flow: None,
            agent_views,
            generation: 1,
            dirty: false,
            effects: Vec::new(),
        }
    }

    // ── host & seam accessors ───────────────────────────────────────────────

    /// Monotonic counter incremented on every state change that affects rendering or chrome.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Return and clear whether state changed since the last call.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.dirty, false)
    }

    /// Drain all pending [`WallEffect`]s queued since the last drain.
    pub fn drain_effects(&mut self) -> Vec<WallEffect> {
        std::mem::take(&mut self.effects)
    }

    /// Invalid `(key, error)` entries dropped from `store.config()` at construction.
    pub fn rejected_dialled(&self) -> &[(String, String)] {
        &self.rejected_dialled
    }

    pub const fn store(&self) -> &WallStore {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut WallStore {
        &mut self.store
    }

    pub const fn camera(&self) -> &WallCamera {
        &self.camera
    }

    pub fn camera_mut(&mut self) -> &mut WallCamera {
        &mut self.camera
    }

    pub const fn config(&self) -> &WallConfig {
        &self.config
    }

    pub const fn born_config(&self) -> &WallConfig {
        &self.born_config
    }

    pub const fn geometry(&self) -> CellGeometry {
        self.geometry
    }

    /// Update cell stride/padding (for example after switching `fontFamily`),
    /// re-laying out any tiled or split groups to match the new metrics.
    pub fn set_geometry(&mut self, geometry: CellGeometry) {
        if self.geometry == geometry {
            return;
        }
        self.geometry = geometry;
        self.relayout_all_groups();
        self.apply_z_order();
        self.emit_change();
    }

    /// Update cell stride/padding after a font family, size, or line-height change and
    /// resize every terminal's `(cols, rows)` grid to keep its world panel footprint,
    /// emitting [`WallEffect::ResizeTerminal`] for every terminal whose grid changed.
    pub fn set_geometry_preserving_panels(&mut self, geometry: CellGeometry) {
        if self.geometry == geometry {
            return;
        }
        let old = self.geometry;
        let split_members: std::collections::HashSet<String> = self
            .store
            .groups()
            .values()
            .filter(|g| g.layout == GroupLayout::Split)
            .flat_map(|g| g.members.iter().cloned())
            .collect();

        let mut resizes: Vec<(String, u32, u32)> = Vec::new();
        for (id, entry) in &mut self.terminals {
            // Split panes are re-wrapped against their exact split rect in `relayout_all_groups`.
            if split_members.contains(id) {
                continue;
            }
            let panel_w = f64::from(entry.cols) * old.stride.x + 2.0 * old.padding.x;
            let panel_h = f64::from(entry.rows) * old.stride.y + 2.0 * old.padding.y;
            let new_cols = MIN_PANE_COLS.max(
                ((panel_w - 2.0 * geometry.padding.x) / geometry.stride.x.max(1e-9)).round()
                    as u32,
            );
            let new_rows = MIN_PANE_ROWS.max(
                ((panel_h - 2.0 * geometry.padding.y) / geometry.stride.y.max(1e-9)).round()
                    as u32,
            );
            if entry.cols != new_cols || entry.rows != new_rows {
                entry.cols = new_cols;
                entry.rows = new_rows;
                resizes.push((id.clone(), new_cols, new_rows));
            }
        }

        self.geometry = geometry;
        for (id, cols, rows) in resizes {
            self.effects
                .push(WallEffect::ResizeTerminal { id, cols, rows });
        }
        self.relayout_all_groups();
        self.apply_z_order();
        self.emit_change();
    }

    /// Record whether the `termd` connection is currently live.
    pub fn set_connected(&mut self, connected: bool) {
        if self.connected == connected {
            return;
        }
        self.connected = connected;
        self.emit_change();
    }

    pub const fn connected(&self) -> bool {
        self.connected
    }

    /// Advance camera animations and notice timers by `dt_secs` at `now_ms`,
    /// updating viewport pixel size to `(view_w, view_h)`.
    ///
    /// Returns `true` while an animation (camera ease or wheel glide) remains
    /// in flight and needs another frame.
    pub fn tick(&mut self, dt_secs: f64, now_ms: f64, view_w: f64, view_h: f64) -> bool {
        self.camera.set_view(view_w, view_h);
        self.camera.step(dt_secs);
        if let Some(expires) = self.notice_expires_at_ms {
            if now_ms >= expires {
                self.notice = None;
                self.notice_expires_at_ms = None;
                self.emit_change();
            }
        }
        self.camera.moving()
    }

    fn emit_change(&mut self) {
        self.prune_menu();
        self.generation += 1;
        self.dirty = true;
        if !matches!(self.effects.last(), Some(WallEffect::Redraw)) {
            self.effects.push(WallEffect::Redraw);
        }
    }

    fn grid_spec(&self) -> GridSpec {
        GridSpec::new(
            self.config.grid_minor,
            self.config.grid_major_every.max(1.0) as u32,
            self.config.grid_min_px,
        )
    }

    // ── terminals, order, z-order, rects ────────────────────────────────────

    /// Look up a live terminal by id.
    pub fn terminal(&self, id: &str) -> Option<&TerminalEntry> {
        self.terminals.get(id)
    }

    /// Arrival order of all live terminals on the wall (shown and hidden).
    pub fn order(&self) -> &[String] {
        &self.order
    }

    /// Back-to-front Z-stacking order of all live terminals.
    pub fn z_order(&self) -> &[String] {
        &self.z_order
    }

    /// World Z depth assigned to group `gid`'s backdrop frame, if any.
    pub fn group_depth(&self, gid: &str) -> Option<f64> {
        self.group_z.get(gid).copied()
    }

    /// Whether terminal `id` is mounted on the wall and not hidden.
    pub fn is_shown(&self, id: &str) -> bool {
        self.terminals.contains_key(id) && !self.store.hidden(id)
    }

    /// Shown terminals in arrival order.
    pub fn shown_order(&self) -> Vec<String> {
        self.order
            .iter()
            .filter(|id| !self.store.hidden(id))
            .cloned()
            .collect()
    }

    /// Whether any fullscreen view is currently active.
    pub const fn fullscreen_active(&self) -> bool {
        self.solo_fullscreen_id.is_some()
    }

    /// Currently active solo-fullscreen terminal id, if any.
    pub fn solo_fullscreen_id(&self) -> Option<&str> {
        self.solo_fullscreen_id.as_deref()
    }

    /// Enter or exit solo fullscreen on terminal `id` (must be shown).
    pub fn set_solo_fullscreen(&mut self, id: Option<&str>) -> Result<(), String> {
        match id {
            None => {
                if self.solo_fullscreen_id.is_none() {
                    return Ok(());
                }
                self.solo_fullscreen_id = None;
                if let Some(saved_cam) = self.pre_fullscreen_camera.take() {
                    self.camera.set_pose(saved_cam);
                }
                self.apply_z_order();
                self.emit_change();
                Ok(())
            }
            Some(target_id) => {
                if !self.terminals.contains_key(target_id) {
                    return Err(format!("no terminal '{target_id}'"));
                }
                if !self.is_shown(target_id) {
                    return Err(format!("terminal '{target_id}' is hidden"));
                }
                if self.pre_fullscreen_camera.is_none() {
                    self.pre_fullscreen_camera = Some(self.canvas_camera_pose());
                }
                self.solo_fullscreen_id = Some(target_id.to_owned());
                self.apply_z_order();
                self.emit_change();
                Ok(())
            }
        }
    }

    /// Whether `id` is currently visible on the open canvas (shown and not veiled behind fullscreen).
    pub fn on_canvas(&self, id: &str) -> bool {
        self.is_shown(id) && !self.fullscreen_active()
    }

    /// Terminals currently on the open canvas in arrival order (empty while fullscreen is active).
    pub fn canvas_order(&self) -> Vec<String> {
        if self.fullscreen_active() {
            Vec::new()
        } else {
            self.shown_order()
        }
    }

    /// World-space body rectangle of terminal `id` (cells + panel padding).
    pub fn terminal_rect(&self, id: &str) -> Option<Rect> {
        self.terminals.get(id).map(|t| t.body_rect(self.geometry))
    }

    /// World height of terminal `id`'s title bar at `zoom` (`0.0` when suppressed).
    pub fn bar_height(&self, id: &str, zoom: f64) -> f64 {
        let Some(entry) = self.terminals.get(id) else {
            return 0.0;
        };
        if self.solo_fullscreen_id.as_deref() == Some(id) {
            return 0.0;
        }
        if entry.bar_room_override == Some(0.0) {
            return 0.0;
        }
        let max_room = entry.bar_room_override.unwrap_or(BAR_ROOM);
        (TITLE_BAR_PX / zoom.max(1e-9)).min(max_room)
    }

    /// World-space window rectangle of terminal `id` (body plus its title bar at `zoom`).
    pub fn window_rect_at_zoom(&self, id: &str, zoom: f64) -> Option<Rect> {
        let body = self.terminal_rect(id)?;
        let bar = self.bar_height(id, zoom);
        Some(Rect {
            max_y: body.max_y + bar,
            ..body
        })
    }

    /// World-space window rectangle of terminal `id` at the camera's current zoom.
    pub fn window_rect(&self, id: &str) -> Option<Rect> {
        self.window_rect_at_zoom(id, self.camera.zoom())
    }

    /// Move terminal `id` so its body panel's top-left corner sits at `top_left`.
    pub fn place_body(&mut self, id: &str, top_left: Point) -> bool {
        let Some(entry) = self.terminals.get_mut(id) else {
            return false;
        };
        entry.position = top_left;
        true
    }

    /// Persist terminal `id`'s current `position` in [`WallStore`].
    pub fn remember_position(&mut self, id: &str) {
        if let Some(pos) = self.terminals.get(id).map(|t| t.position) {
            self.store.set_position(id, pos);
        }
    }

    /// Update terminal `id`'s cell dimensions as reported by `termd`, re-laying out its group if needed.
    pub fn update_terminal_size(&mut self, id: &str, cols: u32, rows: u32) -> bool {
        let Some(entry) = self.terminals.get_mut(id) else {
            return false;
        };
        if entry.cols == cols && entry.rows == rows {
            return false;
        }
        entry.cols = cols;
        entry.rows = rows;
        if let Some(gid) = self.store.group_of(id).map(str::to_owned) {
            if self.store.group(&gid).map(|g| g.layout) == Some(GroupLayout::Tile) {
                self.relayout_group(&gid);
            }
        }
        self.emit_change();
        true
    }

    /// Update terminal `id`'s OSC shell title.
    pub fn set_terminal_title(&mut self, id: &str, title: impl Into<String>) -> bool {
        let Some(entry) = self.terminals.get_mut(id) else {
            return false;
        };
        let title = title.into();
        if entry.shell_title == title {
            return false;
        }
        entry.shell_title = title;
        self.emit_change();
        true
    }

    /// Update terminal `id`'s visible preview line.
    pub fn set_terminal_preview(&mut self, id: &str, preview: impl Into<String>) -> bool {
        let Some(entry) = self.terminals.get_mut(id) else {
            return false;
        };
        let preview = preview.into();
        if entry.preview == preview {
            return false;
        }
        entry.preview = preview;
        self.emit_change();
        true
    }

    /// Update whether terminal `id` currently holds a non-empty text selection.
    pub fn set_terminal_has_selection(&mut self, id: &str, has_selection: bool) -> bool {
        let Some(entry) = self.terminals.get_mut(id) else {
            return false;
        };
        if entry.has_selection == has_selection {
            return false;
        }
        entry.has_selection = has_selection;
        self.emit_change();
        true
    }

    /// Update whether terminal `id`'s running program has DECSET mouse tracking enabled.
    pub fn set_mouse_reporting(&mut self, id: &str, on: bool) -> bool {
        let Some(entry) = self.terminals.get_mut(id) else {
            return false;
        };
        if entry.mouse_reporting == on {
            return false;
        }
        entry.mouse_reporting = on;
        true
    }

    /// Whether terminal `id` currently reports mouse events to its program.
    pub fn mouse_reporting(&self, id: &str) -> bool {
        self.terminals
            .get(id)
            .is_some_and(|entry| entry.mouse_reporting)
    }

    /// Display name of terminal `id`: custom store name, else attached agent target,
    /// else shortened shell title, else `id` (`TerminalWall.nameOf`).
    pub fn name_of(&self, id: &str) -> String {
        if let Some(custom) = self.store.name(id) {
            return custom.to_owned();
        }
        if let Some(attached) = self.store.attached(id) {
            return attached.to_owned();
        }
        if let Some(entry) = self.terminals.get(id) {
            let shortened = short_title(&entry.shell_title);
            if !shortened.is_empty() {
                return shortened.to_owned();
            }
        }
        id.to_owned()
    }

    /// Set or clear (when empty/whitespace) a custom name for terminal `id`.
    pub fn rename(&mut self, id: &str, name: &str) -> bool {
        if !self.terminals.contains_key(id) {
            return false;
        }
        self.store.set_name(id, name);
        self.emit_change();
        true
    }

    /// Currently hovered terminal id, if any.
    pub fn hovered_id(&self) -> Option<&str> {
        self.hovered_id.as_deref()
    }

    /// Update the hovered terminal id.
    pub fn set_hovered(&mut self, id: Option<&str>) {
        let next = id
            .filter(|tid| self.terminals.contains_key(*tid))
            .map(str::to_owned);
        if self.hovered_id == next {
            return;
        }
        self.hovered_id = next;
        self.emit_change();
    }

    /// Raise terminal `id` to the front of the Z-stack.
    pub fn bring_to_front(&mut self, id: &str) {
        if !self.terminals.contains_key(id) {
            return;
        }
        if let Some(pos) = self.z_order.iter().position(|x| x == id) {
            if pos + 1 == self.z_order.len() {
                self.apply_z_order();
                return;
            }
            let item = self.z_order.remove(pos);
            self.z_order.push(item);
        } else {
            self.z_order.push(id.to_owned());
        }
        self.apply_z_order();
    }

    /// Raise every member of group `gid` to the front of the Z-stack, preserving relative member order.
    pub fn bring_group_to_front(&mut self, gid: &str) {
        let Some(group) = self.store.group(gid) else {
            return;
        };
        let member_set: HashSet<&str> = group.members.iter().map(String::as_str).collect();
        let mut outside = Vec::with_capacity(self.z_order.len());
        let mut inside = Vec::new();
        for id in &self.z_order {
            if member_set.contains(id.as_str()) {
                inside.push(id.clone());
            } else {
                outside.push(id.clone());
            }
        }
        outside.extend(inside);
        self.z_order = outside;
        self.apply_z_order();
    }

    /// Recompute normalized `z_order` and world Z coordinates for all terminals and group frames.
    pub fn apply_z_order(&mut self) {
        let fullscreen_ids = self
            .solo_fullscreen_id
            .as_ref()
            .filter(|id| self.is_shown(id))
            .map(|id| vec![id.clone()])
            .unwrap_or_default();
        self.apply_z_order_with(&fullscreen_ids);
    }

    /// Recompute normalized `z_order` and world Z coordinates using explicit `elevated_ids`
    /// (from [`crate::fullscreen::WallFullscreen::elevated_ids`]), returning the assigned
    /// per-group depths.
    pub fn apply_z_order_with(&mut self, elevated_ids: &[String]) -> HashMap<String, f64> {
        let scale_z = self.geometry.scale_z;
        let scale_fn = |_: &str| scale_z;
        let store_ref = &self.store;
        let group_fn = |id: &str| store_ref.group_of(id).map(str::to_owned);

        let stack = z_stack_layout(
            &self.z_order,
            ZStackOptions {
                fullscreen_id: None,
                fullscreen_ids: Some(elevated_ids.to_vec()),
                scale_of: Some(&scale_fn),
                group_of: Some(&group_fn),
            },
        );
        self.z_order = stack.order;
        for (id, z) in stack.terminals {
            if let Some(entry) = self.terminals.get_mut(&id) {
                entry.z = z;
            }
        }
        self.group_z = stack.groups.clone();
        stack.groups
    }

    /// Push a host effect onto the unified outbox drained by [`Self::drain_effects`].
    pub fn push_effect(&mut self, effect: WallEffect) {
        self.effects.push(effect);
    }

    /// Bump generation, mark dirty, prune an invalid menu, and queue [`WallEffect::Redraw`].
    pub fn notify_changed(&mut self) {
        self.emit_change();
    }

    /// Mutable access to terminal `id`'s live entry.
    pub fn terminal_mut(&mut self, id: &str) -> Option<&mut TerminalEntry> {
        self.terminals.get_mut(id)
    }

    /// Update terminal `id`'s world position directly (without persisting or relayout).
    pub fn set_terminal_position(&mut self, id: &str, pos: Point) -> bool {
        let Some(entry) = self.terminals.get_mut(id) else {
            return false;
        };
        entry.position = pos;
        self.emit_change();
        true
    }

    /// Set terminal `id`'s visual scale factor (`> 0`).
    pub fn set_terminal_scale(&mut self, id: &str, scale: f64) -> bool {
        if !scale.is_finite() || scale <= 0.0 {
            return false;
        }
        let Some(entry) = self.terminals.get_mut(id) else {
            return false;
        };
        if (entry.scale - scale).abs() <= 1e-12 {
            return true;
        }
        entry.scale = scale;
        self.emit_change();
        true
    }

    /// Update terminal `id`'s emulator modes (mouse tracking, alternate screen, etc.).
    pub fn set_terminal_modes(&mut self, id: &str, modes: term_cells::Modes) -> bool {
        let Some(entry) = self.terminals.get_mut(id) else {
            return false;
        };
        let reporting = modes.mouse != term_cells::MouseTracking::Off;
        if entry.modes == modes && entry.mouse_reporting == reporting {
            return true;
        }
        entry.modes = modes;
        entry.mouse_reporting = reporting;
        self.emit_change();
        true
    }

    /// Effective DECSET mouse-tracking mode for terminal `id`.
    pub fn mouse_tracking(&self, id: &str) -> MouseTracking {
        self.terminals
            .get(id)
            .map_or(MouseTracking::None, TerminalEntry::mouse_tracking)
    }

    /// Override a split pane's title-bar room (`None` restores default `BAR_ROOM`).
    pub fn set_bar_room_override(&mut self, id: &str, room: Option<f64>) {
        if let Some(entry) = self.terminals.get_mut(id) {
            entry.bar_room_override = room;
        }
    }

    /// Update terminal `id`'s grid cell dimensions without triggering inline group relayout.
    pub fn set_terminal_size_raw(&mut self, id: &str, cols: u32, rows: u32) -> bool {
        let Some(entry) = self.terminals.get_mut(id) else {
            return false;
        };
        if entry.cols == cols && entry.rows == rows {
            return true;
        }
        entry.cols = cols;
        entry.rows = rows;
        self.emit_change();
        true
    }

    /// Clear the active spawn flow grid anchor (e.g. after explicit `at` placement).
    pub fn clear_flow(&mut self) {
        self.flow = None;
    }

    /// Anchor terminal id of the active spawn flow, if any.
    pub(crate) fn flow_anchor_id(&self) -> Option<&str> {
        self.flow.as_ref().and_then(|f| f.anchor_id.as_deref())
    }

    /// Focus navigation history (oldest to newest).
    pub fn focus_history(&self) -> &[String] {
        &self.focus_history
    }

    /// Mount terminal `spec` into `terminals`, `order`, and `z_order` using either a saved
    /// store spot or the next free flow slot, with optional pre-computed canvas footprint
    /// rectangles (`body_rect` unioned with any open Agent View) and anchor frame target.
    pub(crate) fn mount_terminal_with_taken(
        &mut self,
        spec: TerminalSpec,
        modes: term_cells::Modes,
        canvas_camera_override: Option<CameraPose>,
        track_flow: bool,
        taken_override: Option<&[Rect]>,
        anchor_frame_target: Option<CameraPose>,
    ) -> Point {
        let id = spec.id;
        let saved_cam = self.pre_fullscreen_camera;
        if let Some(pose) = canvas_camera_override {
            self.pre_fullscreen_camera = Some(pose);
        }
        let saved_split = self.split_landing.take();
        if !track_flow {
            self.split_landing = Some(PendingSplit {
                at: String::new(),
                axis: SplitAxis::Right,
            });
        }
        let position = self.compute_initial_position_with(
            &id,
            spec.cols,
            spec.rows,
            taken_override,
            anchor_frame_target,
        );
        self.split_landing = saved_split;
        self.pre_fullscreen_camera = saved_cam;

        let entry = TerminalEntry {
            id: id.clone(),
            cols: spec.cols,
            rows: spec.rows,
            position,
            scale: 1.0,
            z: 0.0,
            shell_title: spec.shell_title,
            preview: String::new(),
            has_selection: false,
            mouse_reporting: modes.mouse != term_cells::MouseTracking::Off,
            modes,
            bar_room_override: None,
        };
        self.terminals.insert(id.clone(), entry);
        if !self.order.contains(&id) {
            self.order.push(id.clone());
        }
        if !self.z_order.contains(&id) {
            self.z_order.push(id);
        }
        position
    }

    /// Unmount `id` from the live maps and prune focus history without running inline
    /// group relayout or focus restoration. Returns `(existed, was_focused)`.
    pub(crate) fn unmount_terminal_raw(&mut self, id: &str) -> (bool, bool) {
        let existed = self.terminals.remove(id).is_some();
        if self.store.agent_view(id).is_none() {
            self.agent_views.forget_terminal(id);
        }
        let was_focused = self.focused_id.as_deref() == Some(id);
        if was_focused {
            self.focused_id = None;
        }
        self.order.retain(|x| x != id);
        self.z_order.retain(|x| x != id);
        self.selection.retain(|x| x != id);
        if self.hovered_id.as_deref() == Some(id) {
            self.hovered_id = None;
        }
        self.prune_focus_history();
        (existed, was_focused)
    }

    /// Update `focused_id`, optionally recording focus history and raising `z_order`,
    /// without invoking inline solo-fullscreen logic.
    pub(crate) fn set_focused_raw(&mut self, id: Option<&str>, record_history: bool) -> bool {
        let Some(target_id) = id else {
            if self.focused_id.is_some() {
                self.focused_id = None;
                self.emit_change();
            }
            return true;
        };
        if !self.terminals.contains_key(target_id) {
            return false;
        }
        if self.store.hidden(target_id) {
            self.store.set_hidden(target_id, false);
        }
        self.focused_id = Some(target_id.to_owned());
        if record_history && !self.stepping_history {
            self.record_focus(target_id);
        }
        if let Some(pos) = self.z_order.iter().position(|x| x == target_id) {
            if pos + 1 != self.z_order.len() {
                self.z_order.remove(pos);
                self.z_order.push(target_id.to_owned());
            }
        }
        self.emit_change();
        true
    }

    /// Choose the best remaining shown terminal to focus after `gone_id` hid or closed,
    /// without focusing it directly.
    pub(crate) fn pick_focus_fallback(
        &mut self,
        gone_id: &str,
        preferred_group: Option<&str>,
    ) -> Option<String> {
        self.focused_id = None;
        self.prune_focus_history();
        let group_shown: Vec<String> = preferred_group
            .filter(|gid| self.store.group(gid).is_some())
            .map(|gid| {
                self.shown_group_members(gid)
                    .into_iter()
                    .filter(|m| m != gone_id)
                    .collect()
            })
            .unwrap_or_default();

        if !group_shown.is_empty() {
            for hist_id in self.focus_history.iter().rev() {
                if group_shown.contains(hist_id) {
                    return Some(hist_id.clone());
                }
            }
            return Some(group_shown[0].clone());
        }
        if let Some(cur_idx) = self.focus_index {
            for i in (0..=cur_idx.min(self.focus_history.len().saturating_sub(1))).rev() {
                let candidate = &self.focus_history[i];
                if candidate != gone_id && self.is_shown(candidate) {
                    return Some(candidate.clone());
                }
            }
        }
        for candidate in self.z_order.iter().rev() {
            if candidate != gone_id && self.is_shown(candidate) {
                return Some(candidate.clone());
            }
        }
        None
    }

    /// Advance `focus_index` by `dir` (`-1` back, `+1` forward) and return the chosen
    /// terminal id without mutating focus or camera.
    pub(crate) fn step_focus_history_pick(&mut self, dir: i32) -> Option<String> {
        self.prune_focus_history();
        if self.focus_history.is_empty() {
            return None;
        }
        if self.focused_id.is_none() {
            if dir > 0 {
                return None;
            }
            let idx = self.focus_index?;
            return self.focus_history.get(idx).cloned();
        }
        let cur = self.focus_index?;
        let next_idx = if dir < 0 {
            cur.checked_sub(1)?
        } else if cur + 1 < self.focus_history.len() {
            cur + 1
        } else {
            return None;
        };
        self.focus_index = Some(next_idx);
        self.focus_history.get(next_idx).cloned()
    }

    /// Mark whether a focus change is stepping through focus history (suppressing history push).
    pub(crate) fn set_stepping_history(&mut self, stepping: bool) {
        self.stepping_history = stepping;
    }

    /// Clear `focused_id` without pushing to history (used when hiding/closing focused).
    pub(crate) fn clear_focused_raw(&mut self) {
        self.focused_id = None;
    }

    /// Prune hidden or removed terminals from focus history.
    pub(crate) fn prune_focus_history_raw(&mut self) {
        self.prune_focus_history();
    }

    /// Remove `id` from `selection` without emitting.
    pub(crate) fn deselect_one_raw(&mut self, id: &str) {
        self.selection.retain(|x| x != id);
    }

    /// Step the camera ease/wheel glide and clear expired notices at `now_ms`.
    pub(crate) fn step_camera_and_notice(&mut self, dt_secs: f64, now_ms: f64) -> bool {
        let was_moving = self.camera.moving();
        self.camera.step(dt_secs);
        let still_moving = self.camera.moving();
        let mut changed = was_moving || still_moving;
        if let Some(expires) = self.notice_expires_at_ms {
            if now_ms >= expires {
                self.notice = None;
                self.notice_expires_at_ms = None;
                changed = true;
            }
        }
        if changed {
            self.emit_change();
        }
        still_moving
    }

    // ── placement, spawn, split landing, reconcile ──────────────────────────

    fn canvas_camera_pose(&self) -> CameraPose {
        if let Some(saved) = self.pre_fullscreen_camera {
            return saved;
        }
        if let Some(target) = self.camera.target() {
            return target;
        }
        self.camera.pose()
    }

    fn keep_flow(
        &self,
        pose: CameraPose,
        zoom: f64,
        anchor_frame_target: Option<CameraPose>,
    ) -> bool {
        let Some(flow) = &self.flow else {
            return false;
        };
        if let Some(anchor_id) = &flow.anchor_id {
            if !self.terminals.contains_key(anchor_id) {
                return (pose.x - flow.camera.x).hypot(pose.y - flow.camera.y) * zoom < 4.0;
            }
            if let Some(saved) = self.store.position(anchor_id) {
                if (saved.x - flow.spot.x).abs() > 1e-3 || (saved.y - flow.spot.y).abs() > 1e-3 {
                    return false;
                }
            }
            if !self.fullscreen_active() {
                if let Some(ft) = anchor_frame_target.or_else(|| self.frame_target(anchor_id)) {
                    if (pose.x - ft.x).hypot(pose.y - ft.y) * zoom < 4.0 {
                        return true;
                    }
                }
            }
        }
        (pose.x - flow.camera.x).hypot(pose.y - flow.camera.y) * zoom < 4.0
    }

    fn terminal_taken_rect(&self, id: &str, entry: &TerminalEntry, zoom: f64) -> Rect {
        let tr = entry.body_rect(self.geometry);
        if let Some(av) = self.store.agent_view(id).filter(|a| a.open) {
            let dims = crate::agent_view::AgentViewDims::new(
                self.config.agent_view_width,
                self.config.agent_view_height,
                self.config.agent_view_gap,
            );
            let ar = crate::agent_view::agent_view_rect(tr, self.bar_height(id, zoom), av.side, dims);
            union_rect(&[tr, ar]).unwrap_or(tr)
        } else {
            tr
        }
    }

    fn compute_initial_position(&mut self, id: &str, cols: u32, rows: u32) -> Point {
        self.compute_initial_position_with(id, cols, rows, None, None)
    }

    fn compute_initial_position_with(
        &mut self,
        id: &str,
        cols: u32,
        rows: u32,
        taken_override: Option<&[Rect]>,
        anchor_frame_target: Option<CameraPose>,
    ) -> Point {
        if let Some(saved) = self.store.position(id) {
            return saved;
        }
        let body_w = f64::from(cols) * self.geometry.stride.x + 2.0 * self.geometry.padding.x;
        let body_h = f64::from(rows) * self.geometry.stride.y + 2.0 * self.geometry.padding.y;
        let cur_zoom = self.camera.zoom().max(1e-9);
        let computed_taken: Vec<Rect>;
        let taken: &[Rect] = match taken_override {
            Some(slice) => slice,
            None => {
                computed_taken = self
                    .terminals
                    .iter()
                    .filter(|(tid, _)| tid.as_str() != id)
                    .map(|(tid, entry)| self.terminal_taken_rect(tid, entry, cur_zoom))
                    .collect();
                &computed_taken
            }
        };

        let pose = self.canvas_camera_pose();
        let zoom = if pose.zoom > 0.0 { pose.zoom } else { 1.0 };
        let origin =
            if self.split_landing.is_none() && self.keep_flow(pose, zoom, anchor_frame_target) {
                self.flow
                    .as_ref()
                    .expect("keep_flow checked flow is Some")
                    .origin
            } else {
            let docked = dock_layout(&crate::fullscreen::dock_state_from_store(&self.store));
            let (view_w, _) = self.camera.view();
            let dx_px = if view_w > docked.edges.left + docked.edges.right {
                (docked.edges.left - docked.edges.right) * 0.5
            } else {
                0.0
            };
            let cx = pose.x + dx_px / zoom;
            let cy = pose.y;
            let step = if self.config.snap {
                snap_step(self.grid_spec(), zoom)
            } else {
                0.0
            };
            let raw_x = cx - body_w * 0.5;
            let raw_y = cy + body_h * 0.5;
            if step > 0.0 {
                Point::new(snap_to(raw_x, step), snap_to(raw_y, step))
            } else {
                Point::new(raw_x, raw_y)
            }
        };

        let slot = free_flow_slot(body_w, body_h, taken, Some(origin))
            .unwrap_or(origin);
        self.store.set_position(id, slot);
        if self.split_landing.is_none() {
            self.flow = Some(FlowAnchor {
                origin,
                camera: Point::new(pose.x, pose.y),
                anchor_id: Some(id.to_owned()),
                spot: slot,
            });
        }
        slot
    }

    /// Ask the host to spawn a new terminal (optionally placing its window's
    /// top-left corner at `window_top_left` once it arrives).
    pub fn request_spawn(&mut self, window_top_left: Option<Point>) {
        let (cols, rows) = self.config.new_terminal_cells();
        self.effects.push(WallEffect::SpawnTerminal {
            window_top_left,
            argv: None,
            cols,
            rows,
        });
    }

    /// Mount a terminal onto the wall (`_place` + `_syncTerminals` + `landed`).
    ///
    /// - If `land` is `true` (a newly spawned terminal, as opposed to passive
    ///   session discovery), consumes any pending [`Self::request_split`] to
    ///   insert the pane into its split, or focuses and frames it on the canvas.
    /// - If `at_window_top_left` is `Some(at)` (`wall.spawn x y`), places the
    ///   window's top-left corner at `at` (snapped when `config.snap` is on),
    ///   resets the active flow grid, and frames it there.
    pub fn add_terminal(
        &mut self,
        spec: TerminalSpec,
        land: bool,
        at_window_top_left: Option<Point>,
    ) -> LandedAs {
        let zoom_when_asked = self.camera.zoom();
        let id = spec.id;
        let position = self.compute_initial_position(&id, spec.cols, spec.rows);
        let entry = TerminalEntry {
            id: id.clone(),
            cols: spec.cols,
            rows: spec.rows,
            position,
            scale: 1.0,
            z: 0.0,
            shell_title: spec.shell_title,
            preview: String::new(),
            has_selection: false,
            mouse_reporting: false,
            modes: term_cells::Modes::default(),
            bar_room_override: None,
        };
        self.terminals.insert(id.clone(), entry);
        if !self.order.contains(&id) {
            self.order.push(id.clone());
        }
        if !self.z_order.contains(&id) {
            self.z_order.push(id.clone());
        }

        if let Some(gid) = self.store.group_of(&id).map(str::to_owned) {
            self.relayout_group(&gid);
        }
        self.sync_bar_rooms();
        self.apply_z_order();

        if !land {
            self.emit_change();
            return LandedAs::Canvas;
        }

        if let Some(pending) = self.split_landing.take() {
            if let Ok(group_id) = self.insert_split_pane(&pending.at, &id, pending.axis) {
                self.focus(Some(&id), false);
                return LandedAs::Split {
                    group_id,
                    at: pending.at,
                    axis: pending.axis,
                };
            }
        }

        self.focus(Some(&id), true);
        if let Some(at) = at_window_top_left {
            self.flow = None;
            let bar = self.bar_height(&id, zoom_when_asked);
            let raw_body = Point::new(at.x, at.y - bar);
            let step = if self.config.snap {
                snap_step(self.grid_spec(), zoom_when_asked)
            } else {
                0.0
            };
            let snapped_body = if step > 0.0 {
                Point::new(snap_to(raw_body.x, step), snap_to(raw_body.y, step))
            } else {
                raw_body
            };
            self.place_body(&id, snapped_body);
            self.remember_position(&id);
            if let Some(gid) = self.store.group_of(&id).map(str::to_owned) {
                self.relayout_group(&gid);
            }
            self.frame(&id);
        }
        self.emit_change();
        LandedAs::Canvas
    }

    /// Reconcile [`WallStore`] against `daemon_live_ids` reported by `termd` on connect (`_recover`).
    ///
    /// Keeps any terminal either in `daemon_live_ids` or currently mounted in
    /// `self.terminals`, forgets the rest (collapsing their groups/splits), and
    /// returns the forgotten terminal ids.
    pub fn reconcile_live<S: AsRef<str>>(&mut self, daemon_live_ids: &[S]) -> Vec<String> {
        let mut keep: HashSet<String> = daemon_live_ids
            .iter()
            .map(|s| s.as_ref().to_owned())
            .collect();
        keep.extend(self.terminals.keys().cloned());
        let gone = self.store.prune(keep);
        if !gone.is_empty() {
            for gid in &gone {
                self.agent_views.forget_terminal(gid);
            }
            self.relayout_all_groups();
            self.sync_bar_rooms();
            self.apply_z_order();
            self.emit_change();
        }
        gone
    }

    // ── hide, show, reveal, close, detach ───────────────────────────────────

    /// Hide terminal `id` from the canvas while leaving its session running.
    ///
    /// A hidden pane in a split group leaves the split so its sibling reclaims
    /// the room; a hidden member of a tiled group stays in its group and
    /// re-tiles back into place on [`Self::show`].
    pub fn hide(&mut self, id: &str) -> bool {
        if !self.terminals.contains_key(id) || self.store.hidden(id) {
            return false;
        }
        let was_focused = self.focused_id.as_deref() == Some(id);
        let prev_group = self.store.group_of(id).map(str::to_owned);
        if self.solo_fullscreen_id.as_deref() == Some(id) {
            let _ = self.set_solo_fullscreen(None);
        }
        if prev_group
            .as_deref()
            .and_then(|gid| self.store.group(gid))
            .map(|g| g.layout)
            == Some(GroupLayout::Split)
        {
            let _ = self.remove_from_group(&[id]);
        }
        self.store.set_hidden(id, true);
        self.prune_focus_history();
        if was_focused {
            self.restore_focus_after(id, prev_group.as_deref());
        }
        if self.hovered_id.as_deref() == Some(id) {
            self.hovered_id = None;
        }
        self.selection.retain(|sel| sel != id);
        if let Some(gid) = self.store.group_of(id).map(str::to_owned) {
            self.relayout_group(&gid);
        }
        self.sync_bar_rooms();
        self.apply_z_order();
        self.emit_change();
        true
    }

    /// Show a hidden terminal at its remembered spot (or its group's tile).
    pub fn show(&mut self, id: &str) -> bool {
        if !self.terminals.contains_key(id) || !self.store.hidden(id) {
            return false;
        }
        self.store.set_hidden(id, false);
        if let Some(gid) = self.store.group_of(id).map(str::to_owned) {
            self.relayout_group(&gid);
        }
        self.sync_bar_rooms();
        self.apply_z_order();
        self.emit_change();
        true
    }

    /// Show terminal `id` (if hidden), focus it, and frame the camera on it (`wall.reveal`).
    pub fn reveal(&mut self, id: &str) -> bool {
        if !self.terminals.contains_key(id) {
            return false;
        }
        self.show(id);
        self.focus(Some(id), true)
    }

    /// Kill terminal `id` in `termd` (`WallEffect::KillTerminal`) and forget all
    /// of its persisted state (`WallStore::forget`), restoring focus and re-laying
    /// out its former group (`wall.close` / `TerminalWall.kill`).
    pub fn close(&mut self, id: &str) -> bool {
        let existed = self.terminals.contains_key(id);
        let prev_group = self.store.group_of(id).map(str::to_owned);
        let was_focused = self.focused_id.as_deref() == Some(id);

        self.store.forget(id);
        self.detach_in_memory(id, was_focused, prev_group.as_deref());
        self.effects.push(WallEffect::KillTerminal {
            id: id.to_owned(),
        });
        if let Some(gid) = prev_group {
            if self.store.group(&gid).is_some() {
                self.relayout_group(&gid);
            }
        }
        self.sync_bar_rooms();
        self.apply_z_order();
        self.emit_change();
        existed
    }

    /// Unmount terminal `id` from the live wall without forgetting its saved
    /// position or name in [`WallStore`] (for example when a client detaches).
    pub fn remove_terminal(&mut self, id: &str) -> bool {
        if !self.terminals.contains_key(id) {
            return false;
        }
        let prev_group = self.store.group_of(id).map(str::to_owned);
        let was_focused = self.focused_id.as_deref() == Some(id);
        self.detach_in_memory(id, was_focused, prev_group.as_deref());
        if let Some(gid) = prev_group {
            if self.store.group(&gid).is_some() {
                self.relayout_group(&gid);
            }
        }
        self.sync_bar_rooms();
        self.apply_z_order();
        self.emit_change();
        true
    }

    fn detach_in_memory(&mut self, id: &str, was_focused: bool, preferred_group: Option<&str>) {
        self.terminals.remove(id);
        if self.store.agent_view(id).is_none() {
            self.agent_views.forget_terminal(id);
        }
        self.order.retain(|x| x != id);
        self.z_order.retain(|x| x != id);
        self.selection.retain(|x| x != id);
        if self.hovered_id.as_deref() == Some(id) {
            self.hovered_id = None;
        }
        if self.solo_fullscreen_id.as_deref() == Some(id) {
            let _ = self.set_solo_fullscreen(None);
        }
        self.prune_focus_history();
        if was_focused {
            self.restore_focus_after(id, preferred_group);
        }
    }

    // ── focus, focus history, cycling, directional focus ────────────────────

    /// Currently focused terminal id, if any.
    pub fn focused_id(&self) -> Option<&str> {
        self.focused_id.as_deref()
    }

    /// Focus terminal `id` (or clear focus when `None`).
    ///
    /// Focusing a hidden terminal shows it first; unless stepping through
    /// history, records `id` in the 64-entry focus history, raises it to the
    /// front of `z_order`, and when `frame` is `true` eases the camera to fit it.
    pub fn focus(&mut self, id: Option<&str>, frame: bool) -> bool {
        let Some(target_id) = id else {
            if self.focused_id.is_some() {
                self.focused_id = None;
                self.emit_change();
            }
            return true;
        };
        if !self.terminals.contains_key(target_id) {
            return false;
        }
        self.show(target_id);
        self.focused_id = Some(target_id.to_owned());
        if !self.stepping_history {
            self.record_focus(target_id);
        }
        self.bring_to_front(target_id);
        if self.solo_fullscreen_id.is_some()
            && self.solo_fullscreen_id.as_deref() != Some(target_id)
        {
            self.solo_fullscreen_id = Some(target_id.to_owned());
            self.apply_z_order();
        } else if frame && !self.fullscreen_active() {
            self.frame(target_id);
        }
        self.emit_change();
        true
    }

    fn record_focus(&mut self, id: &str) {
        self.prune_focus_history();
        if let Some(idx) = self.focus_index {
            if self.focus_history.get(idx).map(String::as_str) == Some(id) {
                return;
            }
            self.focus_history.truncate(idx + 1);
        } else {
            self.focus_history.clear();
        }
        self.focus_history.push(id.to_owned());
        if self.focus_history.len() > FOCUS_HISTORY_CAP {
            self.focus_history.remove(0);
        }
        self.focus_index = Some(self.focus_history.len() - 1);
    }

    fn prune_focus_history(&mut self) {
        let mut out: Vec<String> = Vec::with_capacity(self.focus_history.len());
        let mut next_idx: Option<usize> = None;
        for (i, id) in self.focus_history.iter().enumerate() {
            let at_or_before = self.focus_index.is_some_and(|cur| i <= cur);
            if !self.is_shown(id) {
                if at_or_before {
                    next_idx = out.len().checked_sub(1);
                }
                continue;
            }
            if out.last() != Some(id) {
                out.push(id.clone());
            }
            if at_or_before {
                next_idx = out.len().checked_sub(1);
            }
        }
        self.focus_history = out;
        self.focus_index = if self.focus_history.is_empty() {
            None
        } else {
            Some(next_idx.unwrap_or(0).min(self.focus_history.len() - 1))
        };
    }

    fn restore_focus_after(&mut self, gone_id: &str, preferred_group: Option<&str>) {
        self.focused_id = None;
        let group_shown: Vec<String> = preferred_group
            .filter(|gid| self.store.group(gid).is_some())
            .map(|gid| {
                self.shown_group_members(gid)
                    .into_iter()
                    .filter(|m| m != gone_id)
                    .collect()
            })
            .unwrap_or_default();

        let mut fallback: Option<String> = None;
        if !group_shown.is_empty() {
            for hist_id in self.focus_history.iter().rev() {
                if group_shown.contains(hist_id) {
                    fallback = Some(hist_id.clone());
                    break;
                }
            }
            if fallback.is_none() {
                fallback = Some(group_shown[0].clone());
            }
        }
        if fallback.is_none() {
            if let Some(cur_idx) = self.focus_index {
                for i in (0..=cur_idx.min(self.focus_history.len().saturating_sub(1))).rev() {
                    let candidate = &self.focus_history[i];
                    if candidate != gone_id && self.is_shown(candidate) {
                        fallback = Some(candidate.clone());
                        break;
                    }
                }
            }
        }
        if fallback.is_none() {
            for candidate in self.z_order.iter().rev() {
                if candidate != gone_id && self.is_shown(candidate) {
                    fallback = Some(candidate.clone());
                    break;
                }
            }
        }
        if let Some(next_id) = fallback {
            self.focus(Some(&next_id), false);
        }
    }

    fn focus_in_view(&mut self, id: &str) {
        if self.fullscreen_active() {
            self.focus(Some(id), true);
            return;
        }
        let needs_frame = match self.window_rect(id) {
            Some(win) => {
                let sbox = self.camera.to_screen_box(win);
                let (view_w, view_h) = self.camera.view();
                sbox.left < 0.0 || sbox.top < 0.0 || sbox.right > view_w || sbox.bottom > view_h
            }
            None => false,
        };
        self.focus(Some(id), needs_frame);
    }

    /// Step backward (`dir < 0`) or forward (`dir > 0`) through focus history (`Ctrl+,` / `Ctrl+.`).
    pub fn step_focus_history(&mut self, dir: i32) -> Option<String> {
        self.prune_focus_history();
        if self.focus_history.is_empty() {
            return None;
        }
        if self.focused_id.is_none() {
            if dir > 0 {
                return None;
            }
            let idx = self.focus_index?;
            let id = self.focus_history.get(idx)?.clone();
            self.stepping_history = true;
            self.focus_in_view(&id);
            self.stepping_history = false;
            return Some(id);
        }
        let cur = self.focus_index?;
        let next_idx = if dir < 0 {
            cur.checked_sub(1)?
        } else if cur + 1 < self.focus_history.len() {
            cur + 1
        } else {
            return None;
        };
        self.focus_index = Some(next_idx);
        let id = self.focus_history[next_idx].clone();
        self.stepping_history = true;
        self.focus_in_view(&id);
        self.stepping_history = false;
        Some(id)
    }

    /// Move focus to the nearest overlapping pane or window in `dir` (`wall.pane`).
    pub fn focus_pane(&mut self, dir: Direction) -> Option<String> {
        let current = self.focused_id.as_deref()?;
        let mut rects = PaneMap::new();
        for id in self.shown_order() {
            if let Some(win) = self.window_rect(&id) {
                rects.insert(id, win);
            }
        }
        let next = split::neighbor(&rects, current, dir)?;
        self.focus_in_view(&next);
        Some(next)
    }

    fn group_pane_order(&self, gid: &str) -> Vec<String> {
        let Some(group) = self.store.group(gid) else {
            return Vec::new();
        };
        let shown = self.shown_group_members(gid);
        if group.layout == GroupLayout::Freeform {
            let items: Vec<ReadingItem> = shown
                .iter()
                .filter_map(|id| {
                    self.window_rect(id)
                        .map(|rect| ReadingItem::new(id.clone(), rect))
                })
                .collect();
            reading_order(&items)
        } else {
            shown
        }
    }

    fn top_units(&self) -> Vec<(Option<String>, Vec<String>, Rect)> {
        let mut raw: Vec<(Option<String>, Vec<String>, Rect)> = Vec::new();
        let mut grouped: HashSet<String> = HashSet::new();

        for gid in self.ordered_group_ids() {
            let members = self.group_pane_order(&gid);
            if members.is_empty() {
                continue;
            }
            let member_rects: Vec<Rect> = members
                .iter()
                .filter_map(|id| self.window_rect(id))
                .collect();
            if let Some(bounds) = union_rect(&member_rects) {
                for m in &members {
                    grouped.insert(m.clone());
                }
                raw.push((Some(gid), members, bounds));
            }
        }

        for id in self.shown_order() {
            if !grouped.contains(&id) {
                if let Some(win) = self.window_rect(&id) {
                    raw.push((None, vec![id], win));
                }
            }
        }

        let indexed: Vec<ReadingItem> = raw
            .iter()
            .enumerate()
            .map(|(i, (_, _, rect))| ReadingItem::new(i.to_string(), *rect))
            .collect();
        reading_order(&indexed)
            .into_iter()
            .filter_map(|idx_str| idx_str.parse::<usize>().ok().map(|i| raw[i].clone()))
            .collect()
    }

    /// Cycle focus forward (`dir > 0`) or backward (`dir < 0`) in reading order (`⌘[` / `⌘]`).
    pub fn cycle_focus(&mut self, dir: i32) -> Option<String> {
        let mut scope: Option<Vec<String>> = None;
        if let Some(focused) = self.focused_id.as_deref() {
            if let Some(gid) = self.store.group_of(focused) {
                let members = self.group_pane_order(gid);
                if members.len() >= 2 {
                    scope = Some(members);
                }
            }
        }
        let scope = scope.unwrap_or_else(|| {
            let loose: Vec<String> = self
                .shown_order()
                .into_iter()
                .filter(|id| self.store.group_of(id).is_none())
                .collect();
            let focused_is_loose = self
                .focused_id
                .as_ref()
                .is_none_or(|f| loose.contains(f));
            if loose.len() >= 2 && focused_is_loose {
                let items: Vec<ReadingItem> = loose
                    .into_iter()
                    .filter_map(|id| self.window_rect(&id).map(|r| ReadingItem::new(id, r)))
                    .collect();
                reading_order(&items)
            } else {
                self.top_units()
                    .into_iter()
                    .flat_map(|(_, members, _)| members)
                    .collect()
            }
        });
        let next = cycle(&scope, self.focused_id.as_deref(), dir as isize)?.to_owned();
        self.focus_in_view(&next);
        Some(next)
    }

    /// Cycle focus across top-level groups and loose windows (`⌃⇧[` / `⌃⇧]`).
    pub fn cycle_group_focus(&mut self, dir: i32) -> Option<String> {
        let units = self.top_units();
        if units.is_empty() {
            return None;
        }
        let cur_idx = self.focused_id.as_ref().and_then(|focused| {
            units
                .iter()
                .position(|(_, members, _)| members.contains(focused))
        });
        let chosen_idx = match cur_idx {
            None => {
                if dir > 0 {
                    0
                } else {
                    units.len() - 1
                }
            }
            Some(idx) => {
                let step = if dir < 0 { -1isize } else { 1isize };
                (idx as isize + step).rem_euclid(units.len() as isize) as usize
            }
        };
        let (gid_opt, members, unit_rect) = &units[chosen_idx];
        let mut target = members[0].clone();
        if gid_opt.is_some() && members.len() > 1 {
            for hist_id in self.focus_history.iter().rev() {
                if members.contains(hist_id) {
                    target = hist_id.clone();
                    break;
                }
            }
        }

        if self.fullscreen_active() {
            self.focus(Some(&target), true);
        } else if let Some(gid) = gid_opt {
            let sbox = self.camera.to_screen_box(*unit_rect);
            let (view_w, view_h) = self.camera.view();
            self.focus(Some(&target), false);
            if sbox.left < 0.0 || sbox.top < 0.0 || sbox.right > view_w || sbox.bottom > view_h {
                self.frame_group(gid);
            }
        } else {
            self.focus_in_view(&target);
        }
        Some(target)
    }

    // ── framing ─────────────────────────────────────────────────────────────

    /// Compute the [`CameraPose`] that frames terminal `id`, its screen-sized
    /// title bar, and any open Agent View, capped at [`FRAME_MAX_LINE_PX`] pixels per row.
    pub fn frame_target(&self, id: &str) -> Option<CameraPose> {
        let body = self.terminal_rect(id)?;
        let cap = FRAME_MAX_LINE_PX / self.geometry.stride.y.max(1e-9);
        let cur_zoom = self.camera.zoom().max(1e-9);
        let dims = crate::agent_view::AgentViewDims::new(
            self.config.agent_view_width,
            self.config.agent_view_height,
            self.config.agent_view_gap,
        );
        let open_side = self.store.agent_view(id).filter(|av| av.open).map(|av| av.side);
        let initial = match open_side {
            Some(side) => {
                let ag = crate::agent_view::agent_view_rect(body, self.bar_height(id, cur_zoom), side, dims);
                union_rect(&[body, ag]).unwrap_or(body)
            }
            None => body,
        };
        let first = self.camera.fit_target(initial, DEFAULT_FIT_MARGIN, cap);
        let win = self.window_rect_at_zoom(id, first.zoom)?;
        let second = match open_side {
            Some(side) => {
                let ag = crate::agent_view::agent_view_rect(body, self.bar_height(id, first.zoom), side, dims);
                union_rect(&[win, ag]).unwrap_or(win)
            }
            None => win,
        };
        Some(self.camera.fit_target(second, DEFAULT_FIT_MARGIN, cap))
    }

    /// Ease the camera to frame terminal `id`.
    pub fn frame(&mut self, id: &str) -> bool {
        let Some(target) = self.frame_target(id) else {
            return false;
        };
        self.camera.ease_to(target);
        self.emit_change();
        true
    }

    /// Ease the camera to fit every shown terminal (`wall.fit`).
    pub fn fit_all(&mut self) -> bool {
        if self.fullscreen_active() {
            let _ = self.set_solo_fullscreen(None);
        }
        let shown = self.shown_order();
        let current_zoom = self.camera.zoom();
        let rects_now: Vec<Rect> = shown
            .iter()
            .filter_map(|id| self.window_rect_at_zoom(id, current_zoom))
            .collect();
        let Some(bounds_now) = union_rect(&rects_now) else {
            return false;
        };
        let first = self
            .camera
            .fit_target(bounds_now, FIT_ALL_MARGIN, f64::INFINITY);
        let rects_at_first: Vec<Rect> = shown
            .iter()
            .filter_map(|id| self.window_rect_at_zoom(id, first.zoom))
            .collect();
        let final_bounds = union_rect(&rects_at_first).unwrap_or(bounds_now);
        let second = self
            .camera
            .fit_target(final_bounds, FIT_ALL_MARGIN, f64::INFINITY);
        self.camera.ease_to(second);
        self.emit_change();
        true
    }

    /// Ease the camera to fit group `gid`'s frame and header (`wall.group.frame`).
    pub fn frame_group(&mut self, gid: &str) -> bool {
        if self.fullscreen_active() {
            let _ = self.set_solo_fullscreen(None);
        }
        let current_zoom = self.camera.zoom();
        let Some(at_now) = self.group_box_rect(gid, current_zoom) else {
            return false;
        };
        let first = self
            .camera
            .fit_target(at_now, FIT_ALL_MARGIN, f64::INFINITY);
        let at_first = self.group_box_rect(gid, first.zoom).unwrap_or(at_now);
        let second = self
            .camera
            .fit_target(at_first, FIT_ALL_MARGIN, f64::INFINITY);
        self.camera.ease_to(second);
        self.emit_change();
        true
    }

    // ── hit testing ─────────────────────────────────────────────────────────

    /// Hit-test world point `point` against fullscreen or the Z-ordered canvas stack (`TerminalWall.hit`).
    pub fn hit(&self, point: Point) -> Option<CanvasHit> {
        let zoom = self.camera.zoom().max(1e-9);
        let tol = EDGE_GRAB_PX / zoom;

        if let Some(fs_id) = self.solo_fullscreen_id.as_deref() {
            let body = self.terminal_rect(fs_id)?;
            return body.contains(point).then(|| {
                CanvasHit::Terminal(Hit {
                    id: fs_id.to_owned(),
                    part: HitPart::Body,
                })
            });
        }

        let shown_front_to_back: Vec<String> = self
            .z_order
            .iter()
            .rev()
            .filter(|id| self.is_shown(id))
            .cloned()
            .collect();

        let mut last_idx_of_group: HashMap<String, usize> = HashMap::new();
        for (idx, id) in shown_front_to_back.iter().enumerate() {
            if let Some(gid) = self.store.group_of(id) {
                last_idx_of_group.insert(gid.to_owned(), idx);
            }
        }

        let mut seen_splits: HashSet<String> = HashSet::new();
        let mut checked_headers: HashSet<String> = HashSet::new();

        for (idx, id) in shown_front_to_back.iter().enumerate() {
            let gid_opt = self.store.group_of(id);
            if let Some(gid) = gid_opt {
                if !seen_splits.contains(gid) {
                    seen_splits.insert(gid.to_owned());
                    if let Some(divider) = self.group_divider_at(gid, point, tol) {
                        return Some(CanvasHit::SplitDivider {
                            group_id: gid.to_owned(),
                            divider,
                        });
                    }
                }
            }

            if let (Some(win), Some(body)) = (self.window_rect_at_zoom(id, zoom), self.terminal_rect(id)) {
                let candidate = [HitTarget::new(id.clone(), win, body)];
                if let Some(term_hit) = hit_test(&candidate, point.x, point.y, tol) {
                    return Some(CanvasHit::Terminal(term_hit));
                }
            }

            if let Some(gid) = gid_opt {
                if last_idx_of_group.get(gid) == Some(&idx) {
                    checked_headers.insert(gid.to_owned());
                    if let Some(header) = self.group_header_rect(gid, zoom) {
                        if header.contains(point) {
                            return Some(CanvasHit::GroupHeader {
                                group_id: gid.to_owned(),
                            });
                        }
                    }
                }
            }
        }

        // Check any remaining group headers front-to-back by group depth.
        let mut remaining_groups: Vec<String> = self
            .ordered_group_ids()
            .into_iter()
            .filter(|gid| !checked_headers.contains(gid))
            .collect();
        remaining_groups.sort_by(|a, b| {
            let za = self.group_depth(a).unwrap_or(0.0);
            let zb = self.group_depth(b).unwrap_or(0.0);
            zb.total_cmp(&za)
        });
        for gid in remaining_groups {
            if let Some(header) = self.group_header_rect(&gid, zoom) {
                if header.contains(point) {
                    return Some(CanvasHit::GroupHeader { group_id: gid });
                }
            }
        }
        None
    }

    // ── multi-terminal selection (Shift-click & lasso) ──────────────────────

    /// Selected shown terminal ids in selection order.
    pub fn selection(&self) -> &[String] {
        &self.selection
    }

    /// Select exactly `ids` (filtering out hidden or unknown terminals).
    pub fn select<I, S>(&mut self, ids: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut next: Vec<String> = Vec::new();
        for raw in ids {
            let id = raw.as_ref();
            if self.is_shown(id) && !next.iter().any(|existing| existing == id) {
                next.push(id.to_owned());
            }
        }
        if next == self.selection {
            return;
        }
        self.selection = next;
        self.emit_change();
    }

    /// Select every shown terminal.
    pub fn select_all(&mut self) {
        let all = self.shown_order();
        self.select(all);
    }

    /// Clear the multi-terminal selection set.
    pub fn clear_selection(&mut self) {
        self.select(std::iter::empty::<&str>());
    }

    /// Add shown terminals in `ids` to the selection set (`wall.select.add`).
    pub fn select_add<I, S>(&mut self, ids: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut combined = self.selection.clone();
        for id in ids {
            combined.push(id.as_ref().to_owned());
        }
        self.select(combined);
    }

    /// Toggle terminal `id` in or out of the selection set (Shift-click / `wall.select.toggle`).
    pub fn select_toggle(&mut self, id: &str) {
        if let Some(pos) = self.selection.iter().position(|x| x == id) {
            let mut next = self.selection.clone();
            next.remove(pos);
            self.select(next);
        } else if self.is_shown(id) {
            let mut next = self.selection.clone();
            next.push(id.to_owned());
            self.select(next);
        }
    }

    /// Select terminals on the open canvas touched by freeform lasso `polygon`.
    ///
    /// When `additive` is `true`, touched terminals join any previously selected
    /// terminals; when `false`, the selection becomes exactly the touched set.
    pub fn lasso_select(&mut self, polygon: &[Point], additive: bool) {
        let zoom = self.camera.zoom();
        let mut touched: Vec<String> = if additive {
            self.selection.clone()
        } else {
            Vec::new()
        };
        for id in self.canvas_order() {
            if let Some(win) = self.window_rect_at_zoom(&id, zoom) {
                if lasso_touches(polygon, win) && !touched.contains(&id) {
                    touched.push(id);
                }
            }
        }
        self.select(touched);
    }

    // ── groups & splits ─────────────────────────────────────────────────────

    /// Group ids in natural numeral order (`g-1`, `g-2`, …, `g-10`).
    pub fn ordered_group_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.store.groups().keys().cloned().collect();
        ids.sort_by(|a, b| group_numeral(a).cmp(&group_numeral(b)).then_with(|| a.cmp(b)));
        ids
    }

    /// Display name of group `gid` (custom name, else `"group N"`).
    pub fn group_name_of(&self, gid: &str) -> Option<String> {
        let group = self.store.group(gid)?;
        if group.name.is_empty() {
            Some(format!("group {}", group_numeral(gid)))
        } else {
            Some(group.name.clone())
        }
    }

    /// Shown live members of group `gid` in member order.
    pub fn shown_group_members(&self, gid: &str) -> Vec<String> {
        let Some(group) = self.store.group(gid) else {
            return Vec::new();
        };
        group
            .members
            .iter()
            .filter(|id| self.is_shown(id))
            .cloned()
            .collect()
    }

    /// Gather `ids` into a new group with `layout` (defaults to `config.group_layout`),
    /// returning the new group's id (`g-1`, …).
    pub fn create_group<S: AsRef<str>>(
        &mut self,
        ids: &[S],
        name: &str,
        layout: Option<GroupLayout>,
        columns: Option<u32>,
        gap: Option<f64>,
    ) -> Result<String, String> {
        let mut members: Vec<String> = Vec::new();
        for raw in ids {
            let id = raw.as_ref();
            if !self.terminals.contains_key(id) {
                return Err(format!("no terminal '{id}'"));
            }
            if !members.iter().any(|m| m == id) {
                members.push(id.to_owned());
            }
        }
        if members.is_empty() {
            return Err("a group needs at least one terminal".to_owned());
        }

        let touched_groups: Vec<String> = members
            .iter()
            .filter_map(|id| self.store.group_of(id).map(str::to_owned))
            .collect();

        let default_cols = if self.config.group_columns >= 1.0 {
            self.config.group_columns as u32
        } else {
            (members.len() as f64).sqrt().ceil().max(1.0) as u32
        };
        let chosen_cols = columns.unwrap_or(default_cols);
        if chosen_cols < 1 {
            return Err(format!("columns must be a whole number ≥ 1, not '{chosen_cols}'"));
        }
        let chosen_gap = gap.unwrap_or(self.config.group_gap);
        if !chosen_gap.is_finite() || chosen_gap < 0.0 {
            return Err(format!("gap must be a number ≥ 0, not '{chosen_gap}'"));
        }
        let chosen_layout = layout.unwrap_or(self.config.group_layout);
        let gid = self.store.new_group_id();

        let group = if chosen_layout == GroupLayout::Split {
            self.build_split_group(name.trim(), chosen_cols, chosen_gap, &members)?
        } else {
            let origin = self.origin_of_members(&members);
            WallGroup {
                name: name.trim().to_owned(),
                layout: chosen_layout,
                columns: chosen_cols as usize,
                gap: chosen_gap,
                origin,
                members,
                split: None,
            }
        };

        self.store.set_group(&gid, group);
        self.relayout_group(&gid);
        for other in touched_groups {
            if other != gid && self.store.group(&other).is_some() {
                self.relayout_group(&other);
            }
        }
        self.sync_bar_rooms();
        self.apply_z_order();
        self.emit_change();
        Ok(gid)
    }

    /// Take `ids` out of whatever group they belong to, leaving them at their current spots.
    pub fn remove_from_group<S: AsRef<str>>(&mut self, ids: &[S]) -> Result<(), String> {
        let mut by_group: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for raw in ids {
            let id = raw.as_ref();
            let Some(gid) = self.store.group_of(id).map(str::to_owned) else {
                return Err(format!("'{id}' is in no group"));
            };
            let list = by_group.entry(gid).or_default();
            if !list.iter().any(|x| x == id) {
                list.push(id.to_owned());
            }
        }

        for (gid, leaving) in by_group {
            let Some(mut group) = self.store.group(&gid).cloned() else {
                continue;
            };
            group.members.retain(|m| !leaving.contains(m));
            if group.members.is_empty() {
                self.store.delete_group(&gid);
                continue;
            }
            if let Some(ref mut split_state) = group.split {
                let mut tree = split_state.tree.clone();
                for id in &leaving {
                    if let Some(collapsed) = split::remove(&tree, id) {
                        tree = collapsed;
                    }
                }
                split_state.tree = tree;
            }
            self.store.set_group(&gid, group);
            self.relayout_group(&gid);
        }
        self.sync_bar_rooms();
        self.apply_z_order();
        self.emit_change();
        Ok(())
    }

    /// Dissolve group `gid`, leaving its terminals at their current positions.
    pub fn ungroup(&mut self, gid: &str) -> Result<(), String> {
        if self.store.group(gid).is_none() {
            return Err(format!("no group '{gid}'"));
        }
        self.store.delete_group(gid);
        self.sync_bar_rooms();
        self.apply_z_order();
        self.emit_change();
        Ok(())
    }

    /// Verify that terminal `at` can be split along `axis` with every resulting
    /// pane retaining at least [`MIN_PANE_COLS`] × [`MIN_PANE_ROWS`] cells.
    pub fn can_split(&self, at: &str, axis: SplitAxis) -> Result<(), String> {
        if !self.terminals.contains_key(at) {
            return Err(format!("no terminal '{at}'"));
        }
        if !self.is_shown(at) {
            return Err(format!("terminal '{at}' is hidden"));
        }
        let _ = self.plan_split(at, "\0new", axis)?;
        Ok(())
    }

    /// Validate splitting `at` (defaults to the focused terminal) along `axis`,
    /// arm the pending split landing, and emit [`WallEffect::SpawnTerminal`] (`wall.split`).
    pub fn request_split(&mut self, axis: SplitAxis, id: Option<&str>) -> Result<(), String> {
        let target_id = match id.or(self.focused_id.as_deref()) {
            Some(tid) => tid.to_owned(),
            None => return Err("no terminal to split: focus one, or name it".to_owned()),
        };
        self.can_split(&target_id, axis)?;
        if self.fullscreen_active() {
            let _ = self.set_solo_fullscreen(None);
        }
        self.split_landing = Some(PendingSplit {
            at: target_id,
            axis,
        });
        let (cols, rows) = self.config.new_terminal_cells();
        self.effects.push(WallEffect::SpawnTerminal {
            window_top_left: None,
            argv: None,
            cols,
            rows,
        });
        Ok(())
    }

    /// Cut pane `at` in two and place mounted terminal `id` into the half on `axis`,
    /// returning the split group's id (`WallGroups.insertPane`).
    pub fn insert_split_pane(
        &mut self,
        at: &str,
        id: &str,
        axis: SplitAxis,
    ) -> Result<String, String> {
        if !self.terminals.contains_key(id) {
            return Err(format!("no terminal '{id}'"));
        }
        let (gid, next_group, touched) = self.plan_split(at, id, axis)?;
        self.store.set_group(&gid, next_group);
        self.relayout_group(&gid);
        for other in touched {
            if other != gid && self.store.group(&other).is_some() {
                self.relayout_group(&other);
            }
        }
        self.sync_bar_rooms();
        self.apply_z_order();
        self.emit_change();
        Ok(gid)
    }

    /// Drag a split group's divider at `path` to world coordinate `coord`.
    pub fn drag_split_divider(
        &mut self,
        gid: &str,
        path: &[split::BranchSide],
        coord: f64,
    ) -> Result<(), String> {
        let Some(mut group) = self.store.group(gid).cloned() else {
            return Err(format!("no group '{gid}'"));
        };
        let Some(ref mut split_state) = group.split else {
            return Err(format!("group '{gid}' is not a split"));
        };
        let Some(origin) = group.origin else {
            return Err("a split needs its rect".to_owned());
        };
        let sr = Rect::from_top_left(origin.x, origin.y, split_state.w, split_state.h);
        let min_pane = self.min_pane_size(split_state.bars);
        let next_ratio = split::ratio_for(
            &split_state.tree,
            sr,
            self.config.split_gap,
            path,
            coord,
            |_| min_pane,
        )
        .map_err(|e| e.to_string())?;
        let updated =
            split::set_ratio(&split_state.tree, path, next_ratio).map_err(|e| e.to_string())?;
        split_state.tree = updated;
        self.store.set_group(gid, group);
        self.relayout_group(gid);
        self.emit_change();
        Ok(())
    }

    fn min_pane_size(&self, bars: bool) -> Size {
        let room = if bars {
            self.config.split_bar_rows * self.geometry.stride.y
        } else {
            0.0
        };
        Size::new(
            f64::from(MIN_PANE_COLS) * self.geometry.stride.x + 2.0 * self.geometry.padding.x,
            f64::from(MIN_PANE_ROWS) * self.geometry.stride.y
                + 2.0 * self.geometry.padding.y
                + room,
        )
    }

    fn build_split_group(
        &self,
        name: &str,
        columns: u32,
        gap: f64,
        members: &[String],
    ) -> Result<WallGroup, String> {
        let bars = self.config.split_bars;
        let room = if bars {
            self.config.split_bar_rows * self.geometry.stride.y
        } else {
            0.0
        };
        let rects: Vec<Rect> = members
            .iter()
            .filter_map(|id| self.terminal_rect(id))
            .collect();
        let Some(bounds) = union_rect(&rects) else {
            return Err("a split needs its terminals on the wall".to_owned());
        };
        let cols = (columns as usize).clamp(1, members.len());
        let tree = split::from_members(members, cols).map_err(|e| e.to_string())?;
        Ok(WallGroup {
            name: name.to_owned(),
            layout: GroupLayout::Split,
            columns: columns as usize,
            gap,
            origin: Some(Point::new(bounds.min_x, bounds.max_y + room)),
            members: members.to_vec(),
            split: Some(WallSplit {
                w: bounds.width(),
                h: bounds.height() + room,
                bars,
                tree,
            }),
        })
    }

    fn plan_split(
        &self,
        at: &str,
        id: &str,
        axis: SplitAxis,
    ) -> Result<(String, WallGroup, Vec<String>), String> {
        if !self.terminals.contains_key(at) {
            return Err(format!("no terminal '{at}'"));
        }
        let existing_gid = self.store.group_of(at).map(str::to_owned);
        let mut touched = Vec::new();
        let (gid, base_group) = match existing_gid
            .as_deref()
            .and_then(|g| self.store.group(g).map(|grp| (g, grp)))
        {
            Some((g, grp)) if grp.layout == GroupLayout::Split && grp.split.is_some() => {
                (g.to_owned(), grp.clone())
            }
            other => {
                if let Some((old_gid, _)) = other {
                    touched.push(old_gid.to_owned());
                }
                let fresh_gid = self.store.new_group_id();
                let fresh_group =
                    self.build_split_group("", 1, self.config.group_gap, &[at.to_owned()])?;
                (fresh_gid, fresh_group)
            }
        };

        let mut split_state = base_group
            .split
            .clone()
            .expect("split group carries WallSplit");
        let next_tree = split::insert(
            &split_state.tree,
            at,
            id,
            axis.split_dir(),
            InsertSide::After,
        );
        let min_pane = self.min_pane_size(split_state.bars);
        let needed = split::min_size(&next_tree, |_| min_pane, self.config.split_gap);
        if needed.w > split_state.w + EDGE_EPS || needed.h > split_state.h + EDGE_EPS {
            return Err(format!(
                "no room to split '{at}' {}: each pane needs at least {MIN_PANE_COLS}×{MIN_PANE_ROWS}",
                axis.as_str()
            ));
        }

        let next_members = split::leaves(&next_tree);
        split_state.tree = next_tree;
        let next_group = WallGroup {
            members: next_members,
            split: Some(split_state),
            ..base_group
        };
        Ok((gid, next_group, touched))
    }

    fn origin_of_members(&self, ids: &[String]) -> Option<Point> {
        let tile_boxes: Vec<Rect> = ids
            .iter()
            .filter_map(|id| {
                self.terminal_rect(id).map(|r| Rect {
                    max_y: r.max_y + BAR_ROOM,
                    ..r
                })
            })
            .collect();
        union_rect(&tile_boxes).map(|u| Point::new(u.min_x, u.max_y))
    }

    /// Re-lay out group `gid` (either packing shown tiles or fitting split panes).
    pub fn relayout_group(&mut self, gid: &str) {
        let Some(group) = self.store.group(gid).cloned() else {
            return;
        };
        match group.layout {
            GroupLayout::Freeform => {}
            GroupLayout::Tile => {
                let shown = self.shown_group_members(gid);
                if shown.is_empty() {
                    return;
                }
                let origin = group
                    .origin
                    .or_else(|| self.origin_of_members(&shown))
                    .unwrap_or_default();
                let sizes: Vec<Size> = shown
                    .iter()
                    .filter_map(|id| {
                        self.terminal_rect(id)
                            .map(|r| Size::new(r.width(), r.height()))
                    })
                    .collect();
                let Ok(tiled) = tile_layout(
                    &sizes,
                    TileOptions::new(origin, group.columns.max(1), group.gap.max(0.0)),
                ) else {
                    return;
                };
                for (id, place) in shown.iter().zip(tiled.places.iter()) {
                    self.place_body(id, *place);
                    self.remember_position(id);
                }
                if group.origin.is_none() {
                    let mut updated = group;
                    updated.origin = Some(origin);
                    self.store.set_group(gid, updated);
                }
            }
            GroupLayout::Split => {
                let (Some(origin), Some(split_state)) = (group.origin, group.split.as_ref()) else {
                    return;
                };
                let room = if split_state.bars {
                    self.config.split_bar_rows * self.geometry.stride.y
                } else {
                    0.0
                };
                let split_rect =
                    Rect::from_top_left(origin.x, origin.y, split_state.w, split_state.h);
                let pane_rects = split::panes(&split_state.tree, split_rect, self.config.split_gap);

                let mut resizes: Vec<(String, u32, u32)> = Vec::new();
                for (id, pr) in &pane_rects {
                    let Some(entry) = self.terminals.get_mut(id) else {
                        continue;
                    };
                    let cols = MIN_PANE_COLS.max(
                        ((pr.width() - 2.0 * self.geometry.padding.x) / self.geometry.stride.x
                            + EDGE_EPS)
                            .floor() as u32,
                    );
                    let rows = MIN_PANE_ROWS.max(
                        ((pr.height() - room - 2.0 * self.geometry.padding.y)
                            / self.geometry.stride.y
                            + EDGE_EPS)
                            .floor() as u32,
                    );
                    if entry.cols != cols || entry.rows != rows {
                        entry.cols = cols;
                        entry.rows = rows;
                        resizes.push((id.to_owned(), cols, rows));
                    }
                    entry.position = Point::new(pr.min_x, pr.max_y - room);
                    entry.bar_room_override = Some(room);
                }
                for (id, _) in &pane_rects {
                    self.remember_position(id);
                }
                for (id, cols, rows) in resizes {
                    self.effects
                        .push(WallEffect::ResizeTerminal { id, cols, rows });
                }
            }
        }
    }

    /// Re-lay out every group in creation order.
    pub fn relayout_all_groups(&mut self) {
        for gid in self.ordered_group_ids() {
            self.relayout_group(&gid);
        }
    }

    fn sync_bar_rooms(&mut self) {
        let mut split_rooms: HashMap<String, f64> = HashMap::new();
        for group in self.store.groups().values() {
            if group.layout == GroupLayout::Split {
                if let Some(ref sp) = group.split {
                    let room = if sp.bars {
                        self.config.split_bar_rows * self.geometry.stride.y
                    } else {
                        0.0
                    };
                    for m in &group.members {
                        split_rooms.insert(m.clone(), room);
                    }
                }
            }
        }
        for (id, entry) in &mut self.terminals {
            entry.bar_room_override = split_rooms.get(id).copied();
        }
    }

    /// Padded backdrop frame rectangle of group `gid` at `zoom` (`None` when no member is shown).
    pub fn group_frame_rect(&self, gid: &str, zoom: f64) -> Option<Rect> {
        let shown = self.shown_group_members(gid);
        if shown.is_empty() {
            return None;
        }
        let group = self.store.group(gid)?;
        if group.layout == GroupLayout::Split {
            let origin = group.origin?;
            let sp = group.split.as_ref()?;
            let raw = Rect::from_top_left(origin.x, origin.y, sp.w, sp.h);
            return Some(pad_rect(raw, self.config.group_pad));
        }
        let rects: Vec<Rect> = shown
            .iter()
            .filter_map(|id| self.window_rect_at_zoom(id, zoom))
            .collect();
        union_rect(&rects).map(|u| pad_rect(u, self.config.group_pad))
    }

    /// Header bar rectangle above group `gid`'s frame at `zoom`.
    pub fn group_header_rect(&self, gid: &str, zoom: f64) -> Option<Rect> {
        let frame = self.group_frame_rect(gid, zoom)?;
        let header_h = (TITLE_BAR_PX / zoom.max(1e-9)).min(BAR_ROOM);
        Some(Rect {
            min_x: frame.min_x,
            max_x: frame.max_x,
            min_y: frame.max_y,
            max_y: frame.max_y + header_h,
        })
    }

    /// Combined frame + header bounding box of group `gid` at `zoom`.
    pub fn group_box_rect(&self, gid: &str, zoom: f64) -> Option<Rect> {
        let frame = self.group_frame_rect(gid, zoom)?;
        match self.group_header_rect(gid, zoom) {
            Some(header) => union_rect(&[frame, header]),
            None => Some(frame),
        }
    }

    /// Hit-test a divider inside split group `gid` within `tol` world units of `point`.
    pub fn group_divider_at(&self, gid: &str, point: Point, tol: f64) -> Option<Divider> {
        let group = self.store.group(gid)?;
        if group.layout != GroupLayout::Split {
            return None;
        }
        if !group.members.iter().any(|id| self.on_canvas(id)) {
            return None;
        }
        let origin = group.origin?;
        let sp = group.split.as_ref()?;
        let split_rect = Rect::from_top_left(origin.x, origin.y, sp.w, sp.h);
        split::divider_at(&sp.tree, split_rect, self.config.split_gap, point, tol)
    }

    // ── knobs, panels, context menu, notices, clipboard, snapshot ───────────

    /// Validate and set knob `key` to `value`, persisting it in [`WallStore`] (`wall.config`).
    pub fn set_config(
        &mut self,
        key: &str,
        value: impl Into<KnobInput>,
    ) -> Result<KnobValue, String> {
        let checked = self.config.set(key, value)?;
        let json_val = serde_json::to_value(&checked).expect("KnobValue always serializes");
        self.store.set_config(key, json_val);
        self.effects.push(WallEffect::ConfigKnobChanged {
            key: key.to_owned(),
            value: checked.clone(),
        });
        self.emit_change();
        Ok(checked)
    }

    /// Reset knob `key` back to [`Self::born_config`] and clear its stored override (`wall.config <k> default`).
    pub fn reset_config(&mut self, key: &str) -> Result<KnobValue, String> {
        let restored = self.config.reset_from(key, &self.born_config)?;
        self.store.clear_config(key);
        self.effects.push(WallEffect::ConfigKnobChanged {
            key: key.to_owned(),
            value: restored.clone(),
        });
        self.emit_change();
        Ok(restored)
    }

    pub fn set_sidebar(&mut self, patch: impl Into<SidebarPatch>) {
        self.store.set_sidebar(patch);
        self.emit_change();
    }

    pub fn set_status_window(&mut self, patch: impl Into<StatusWindowPatch>) {
        self.store.set_status_window(patch);
        self.emit_change();
    }

    pub fn set_settings_panel(&mut self, patch: impl Into<SettingsPanelPatch>) {
        self.store.set_settings_panel(patch);
        self.emit_change();
    }

    /// Set the command palette's size (`wall.palette size`).
    pub fn set_palette_size(&mut self, size: crate::dock::PanelSize) {
        self.store.set_palette_size(size);
        self.notify_changed();
    }

    pub fn set_help_panel(&mut self, patch: impl Into<HelpPanelPatch>) {
        self.store.set_help_panel(patch);
        self.emit_change();
    }

    /// Open the right-click context menu on `target` at viewport-local pixel coordinates `at` (`wall.menu`).
    pub fn open_menu(&mut self, target: MenuTarget, at: Point) -> Result<(), String> {
        match &target {
            // A row in the sidebar is there for a hidden terminal too.
            MenuTarget::Terminal {
                id,
                part: crate::menu::TerminalPart::Row,
            } => {
                if !self.terminals.contains_key(id) {
                    return Err(format!("no terminal '{id}'"));
                }
            }
            MenuTarget::Terminal { id, .. } => {
                if !self.is_shown(id) {
                    return Err(if self.terminals.contains_key(id) {
                        format!("terminal '{id}' is hidden")
                    } else {
                        format!("no terminal '{id}'")
                    });
                }
            }
            MenuTarget::Group { id } => {
                if self.store.group(id).is_none() {
                    return Err(format!("no group '{id}'"));
                }
            }
            MenuTarget::Canvas { .. } | MenuTarget::Agent { .. } => {}
        }
        let client = self.camera.bounds().to_client(at);
        let world = self.camera.to_world(client);
        self.menu = Some(OpenMenu { target, at, world });
        self.emit_change();
        Ok(())
    }

    /// Close the open context menu, if any.
    pub fn close_menu(&mut self) {
        if self.menu.take().is_some() {
            self.emit_change();
        }
    }

    pub fn menu(&self) -> Option<&OpenMenu> {
        self.menu.as_ref()
    }

    fn prune_menu(&mut self) {
        let should_drop = match self.menu.as_ref().map(|m| &m.target) {
            // A row's menu outlives its terminal being hidden (its rows
            // change); a window's does not.
            Some(MenuTarget::Terminal {
                id,
                part: crate::menu::TerminalPart::Row,
            }) => !self.terminals.contains_key(id),
            Some(MenuTarget::Terminal { id, .. }) => !self.is_shown(id),
            Some(MenuTarget::Group { id }) => self.store.group(id).is_none(),
            Some(MenuTarget::Canvas { .. } | MenuTarget::Agent { .. }) | None => false,
        };
        if should_drop {
            self.menu = None;
        }
    }

    /// Current wall notice text, if any.
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Show a notice banner for `config.notice_ms` milliseconds starting at `now_ms`.
    pub fn notify(&mut self, text: impl Into<String>, now_ms: f64) {
        self.notice = Some(text.into());
        self.notice_expires_at_ms = Some(now_ms + self.config.notice_ms);
        self.emit_change();
    }

    /// Clear any active notice immediately.
    pub fn clear_notice(&mut self) {
        if self.notice.take().is_some() {
            self.notice_expires_at_ms = None;
            self.emit_change();
        }
    }

    /// Contents of the wall's middle-click selection clipboard.
    pub fn selection_clipboard(&self) -> &str {
        &self.selection_clipboard
    }

    /// Record a finished text selection from a terminal according to `config.copy_on_select`.
    pub fn selection_made(&mut self, text: &str) {
        if text.is_empty() || self.config.copy_on_select == CopyOnSelect::False {
            return;
        }
        self.selection_clipboard = text.to_owned();
        if self.config.copy_on_select == CopyOnSelect::Clipboard {
            self.effects.push(WallEffect::WriteClipboard {
                text: text.to_owned(),
            });
        }
    }

    /// Build a complete [`WallSnapshot`] for chrome panels and `wall.list`.
    pub fn snapshot(&self) -> WallSnapshot {
        let terminals = self
            .order
            .iter()
            .filter_map(|id| {
                let entry = self.terminals.get(id)?;
                Some(TerminalSnapshot {
                    id: id.clone(),
                    name: self.name_of(id),
                    named: self.store.name(id).is_some(),
                    attached: self.store.attached(id).map(str::to_owned),
                    cols: entry.cols,
                    rows: entry.rows,
                    preview: entry.preview.clone(),
                    hidden: self.store.hidden(id),
                    group: self.store.group_of(id).map(str::to_owned),
                    selected: entry.has_selection,
                })
            })
            .collect();

        let groups = self
            .ordered_group_ids()
            .into_iter()
            .filter_map(|gid| {
                let g = self.store.group(&gid)?;
                let name = self.group_name_of(&gid)?;
                Some(GroupSnapshot {
                    id: gid,
                    name,
                    named: !g.name.is_empty(),
                    layout: g.layout,
                    columns: g.columns as u32,
                    gap: g.gap,
                    bars: g.split.as_ref().map(|s| s.bars),
                    members: g
                        .members
                        .iter()
                        .filter(|id| self.terminals.contains_key(*id))
                        .cloned()
                        .collect(),
                })
            })
            .collect();

        WallSnapshot {
            connected: self.connected,
            focused_id: self.focused_id.clone(),
            selection: self.selection.clone(),
            sidebar: self.store.sidebar().clone(),
            status_window: self.store.status_window().clone(),
            settings_panel: self.store.settings_panel().clone(),
            help_panel: self.store.help_panel().clone(),
            fullscreen: self.solo_fullscreen_id.clone(),
            config: self.config.clone(),
            dialled: self.store.config().keys().cloned().collect(),
            menu: self.menu.clone(),
            notice: self.notice.clone(),
            terminals,
            groups,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::PanelSide;
    use crate::menu::TerminalPart;

    fn make_wall_with_storage(storage: MemoryStorage, scope: &str) -> WallModel {
        let store = WallStore::with_storage(Box::new(storage), scope);
        let camera = WallCamera::new(CameraPose::new(0.0, 0.0, 4.0), 1600.0, 1000.0);
        WallModel::new(
            store,
            camera,
            WallConfig::default(),
            CellGeometry::with_padding(1.0, 2.0, 0.3, 0.3),
        )
    }

    fn make_wall() -> WallModel {
        make_wall_with_storage(MemoryStorage::new(), "relay-9")
    }

    #[test]
    fn config_persists_per_scope_resets_to_born_and_drops_invalid_stored_knobs() {
        let mem = MemoryStorage::new();
        let mut wall = make_wall_with_storage(mem.clone(), "relay-9");

        assert_eq!(
            wall.set_config("snap", "off").unwrap(),
            KnobValue::Bool(false)
        );
        assert_eq!(
            wall.set_config("snap", "on").unwrap(),
            KnobValue::Bool(true)
        );
        assert_eq!(
            wall.set_config("gridShow", "off").unwrap(),
            KnobValue::Bool(false)
        );
        assert_eq!(
            wall.set_config("background", "#203040").unwrap(),
            KnobValue::Text("#203040".to_owned())
        );
        assert!(wall.set_config("gridMinor", "-2").is_err());

        // Reload on same scope preserves dialled values; another scope stays on defaults.
        let mut reloaded = make_wall_with_storage(mem.clone(), "relay-9");
        assert!(reloaded.config().snap);
        assert!(!reloaded.config().grid_show);
        assert_eq!(reloaded.config().background, "#203040");
        assert_eq!(
            reloaded.snapshot().dialled,
            vec!["background", "gridShow", "snap"]
        );

        let other_scope = make_wall_with_storage(mem.clone(), "relay-10");
        assert_eq!(
            other_scope.config().background,
            WallConfig::default().background
        );

        // Resetting `background` restores born default and forgets it in the store.
        assert_eq!(
            reloaded.reset_config("background").unwrap(),
            KnobValue::Text(WallConfig::default().background)
        );
        assert!(!reloaded.snapshot().dialled.contains(&"background".to_owned()));
        assert_eq!(
            make_wall_with_storage(mem.clone(), "relay-9")
                .config()
                .background,
            WallConfig::default().background
        );

        // Inject invalid remembered values (`gridMinor: 0` and unknown `gone: 1`): dropped at boot.
        let mut corrupt_store = WallStore::with_storage(Box::new(mem.clone()), "relay-bad");
        corrupt_store.set_config("gridMinor", serde_json::json!(0));
        corrupt_store.set_config("snap", serde_json::json!(false));
        corrupt_store.set_config("gone", serde_json::json!(1));

        let cleaned = make_wall_with_storage(mem.clone(), "relay-bad");
        assert_eq!(cleaned.config().grid_minor, WallConfig::default().grid_minor);
        assert!(!cleaned.config().snap);
        let rejected_keys: Vec<&str> = cleaned
            .rejected_dialled()
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(rejected_keys, vec!["gone", "gridMinor"]);

        let reloaded_clean = WallStore::with_storage(Box::new(mem), "relay-bad");
        assert_eq!(
            reloaded_clean.config().keys().cloned().collect::<Vec<_>>(),
            vec!["snap"]
        );
    }

    #[test]
    fn settings_panel_and_short_title_and_snapshot_reflect_wall_state() {
        let mem = MemoryStorage::new();
        let mut wall = make_wall_with_storage(mem.clone(), "relay-9");

        wall.set_settings_panel(SettingsPanelPatch {
            open: Some(true),
            side: Some(PanelSide::Right),
            collapsed: Some(vec!["groups".to_owned()]),
            ..Default::default()
        });
        let snap = wall.snapshot();
        assert!(snap.settings_panel.open);
        assert_eq!(snap.settings_panel.side, PanelSide::Right);
        assert_eq!(snap.settings_panel.collapsed, vec!["groups"]);

        let reloaded = make_wall_with_storage(mem, "relay-9");
        assert_eq!(reloaded.snapshot().settings_panel, snap.settings_panel);

        assert_eq!(short_title("lugo@laptop-2:~/dev/glyph3d "), "~/dev/glyph3d");
        assert_eq!(short_title("cargo test"), "cargo test");
    }

    #[test]
    fn placement_reuses_viewport_flow_until_camera_pans_and_respects_remembered_spots() {
        let mut wall = make_wall();
        wall.store_mut()
            .set_position("remembered", Point::new(500.0, -300.0));

        wall.add_terminal(TerminalSpec::new("remembered", 80, 24), true, None);
        assert_eq!(
            wall.terminal("remembered").unwrap().position,
            Point::new(500.0, -300.0)
        );

        // Move camera back to origin and spawn two unpositioned terminals: second flows right of first.
        wall.camera_mut().set_pose(CameraPose::new(0.0, 0.0, 4.0));
        wall.add_terminal(TerminalSpec::new("t1", 80, 24), true, None);
        let p1 = wall.terminal("t1").unwrap().position;
        let r1 = wall.terminal_rect("t1").unwrap();

        wall.add_terminal(TerminalSpec::new("t2", 80, 24), true, None);
        let p2 = wall.terminal("t2").unwrap().position;
        let r2 = wall.terminal_rect("t2").unwrap();
        assert!(!r1.overlaps(&r2));
        assert!(p2.x > p1.x, "t2 ({p2:?}) should flow to the right of t1 ({p1:?})");
        assert_eq!(p2.y, p1.y);

        // Panning far away resets the flow origin around the new camera centre.
        wall.camera_mut()
            .set_pose(CameraPose::new(2000.0, 1200.0, 4.0));
        wall.add_terminal(TerminalSpec::new("t3", 80, 24), true, None);
        let p3 = wall.terminal("t3").unwrap().position;
        assert!((p3.x - 2000.0).abs() < 200.0);
        assert!((p3.y - 1200.0).abs() < 200.0);
    }

    #[test]
    fn focus_history_caps_steps_back_and_forward_and_restores_on_hide_or_close() {
        let mut wall = make_wall();
        for id in ["a", "b", "c"] {
            wall.add_terminal(TerminalSpec::new(id, 80, 24), true, None);
        }
        assert_eq!(wall.focused_id(), Some("c"));
        assert_eq!(wall.z_order().last().map(String::as_str), Some("c"));

        // Step back twice, then forward once.
        assert_eq!(wall.step_focus_history(-1).as_deref(), Some("b"));
        assert_eq!(wall.focused_id(), Some("b"));
        assert_eq!(wall.step_focus_history(-1).as_deref(), Some("a"));
        assert_eq!(wall.step_focus_history(-1), None);
        assert_eq!(wall.step_focus_history(1).as_deref(), Some("b"));

        // Hiding focused 'b' restores focus to 'a' (prior shown history stop) and omits 'b' from shown/canvas order.
        assert!(wall.hide("b"));
        assert!(!wall.is_shown("b"));
        assert!(!wall.on_canvas("b"));
        assert_eq!(wall.shown_order(), vec!["a", "c"]);
        assert_eq!(wall.focused_id(), Some("a"));

        // Revealing 'b' shows, focuses, and eases camera to frame it.
        assert!(wall.reveal("b"));
        assert!(wall.is_shown("b"));
        assert_eq!(wall.focused_id(), Some("b"));
        assert!(wall.camera().moving());

        // Closing 'b' emits KillTerminal, forgets its store spot, and restores focus.
        wall.drain_effects();
        assert!(wall.close("b"));
        assert!(wall.terminal("b").is_none());
        assert_eq!(wall.store().position("b"), None);
        assert!(wall
            .drain_effects()
            .contains(&WallEffect::KillTerminal { id: "b".to_owned() }));
        assert!(wall.focused_id().is_some());
    }

    #[test]
    fn split_landing_cuts_pane_rewraps_cells_refuses_under_minimum_and_collapses_on_hide() {
        let mut wall = make_wall();
        wall.add_terminal(TerminalSpec::new("a", 80, 24), true, None);
        wall.place_body("a", Point::new(10.0, -5.0));
        wall.remember_position("a");
        wall.drain_effects();

        wall.request_split(SplitAxis::Right, Some("a")).unwrap();
        assert!(wall
            .drain_effects()
            .iter()
            .any(|e| matches!(e, WallEffect::SpawnTerminal { .. })));

        let landed = wall.add_terminal(TerminalSpec::new("b", 80, 24), true, None);
        let LandedAs::Split { group_id, at, axis } = landed else {
            panic!("expected split landing, got {landed:?}");
        };
        assert_eq!(at, "a");
        assert_eq!(axis, SplitAxis::Right);
        assert_eq!(wall.focused_id(), Some("b"));

        // Both halves re-wrapped equally and emitted ResizeTerminal effects.
        let cols_a = wall.terminal("a").unwrap().cols;
        let cols_b = wall.terminal("b").unwrap().cols;
        assert_eq!(cols_a, cols_b);
        assert!((MIN_PANE_COLS..80).contains(&cols_a));
        assert!(wall.drain_effects().iter().any(|e| matches!(
            e,
            WallEffect::ResizeTerminal { id, .. } if id == "a"
        )));

        // Directional focus moves between the split panes.
        assert_eq!(wall.focus_pane(Direction::Left).as_deref(), Some("a"));
        assert_eq!(wall.focus_pane(Direction::Right).as_deref(), Some("b"));

        // Hiding 'b' removes it from the split so 'a' reclaims all 80 columns.
        assert!(wall.hide("b"));
        assert_eq!(wall.terminal("a").unwrap().cols, 80);
        assert_eq!(
            wall.store().group(&group_id).unwrap().members,
            vec!["a".to_owned()]
        );

        // A 30-col terminal cannot split right (each half needs >= 20 cols), though down fits.
        wall.add_terminal(TerminalSpec::new("narrow", 30, 24), true, None);
        let err = wall.can_split("narrow", SplitAxis::Right).unwrap_err();
        assert!(err.contains("no room to split 'narrow' right"), "got: {err}");
        assert!(wall.can_split("narrow", SplitAxis::Down).is_ok());
    }

    #[test]
    fn hit_testing_respects_z_order_edge_band_split_dividers_headers_and_fullscreen() {
        let mut wall = make_wall();
        wall.camera_mut().set_pose(CameraPose::new(50.0, -25.0, 2.0)); // tol = 7 / 2 = 3.5 world units
        wall.add_terminal(TerminalSpec::new("back", 80, 24), false, None);
        wall.place_body("back", Point::new(0.0, 0.0));
        wall.add_terminal(TerminalSpec::new("front", 40, 12), false, None);
        wall.place_body("front", Point::new(60.0, -10.0));
        wall.bring_to_front("front");

        // `front` overlaps `back`'s right border at x = 80.6, y = -20: `front`'s body wins!
        assert_eq!(
            wall.hit(Point::new(80.6, -20.0)),
            Some(CanvasHit::Terminal(Hit {
                id: "front".to_owned(),
                part: HitPart::Body,
            }))
        );

        // Below `front`, `back`'s right border band grabs the right edge.
        let edge_hit = wall.hit(Point::new(80.6, -45.0));
        assert!(matches!(
            edge_hit,
            Some(CanvasHit::Terminal(Hit { ref id, part: HitPart::Edge(e) }))
                if id == "back" && e.right
        ));

        // Above `back`'s body (`y = 0`) sits its title bar (`max_y = bar_height`).
        assert_eq!(
            wall.hit(Point::new(20.0, 2.0)),
            Some(CanvasHit::Terminal(Hit {
                id: "back".to_owned(),
                part: HitPart::Title,
            }))
        );

        // Solo fullscreen restricts hits strictly to that terminal's body (no edges, no background hits).
        wall.set_solo_fullscreen(Some("front")).unwrap();
        assert_eq!(wall.canvas_order(), Vec::<String>::new());
        assert_eq!(
            wall.hit(Point::new(20.0, -20.0)),
            None
        );
        assert_eq!(
            wall.hit(Point::new(70.0, -20.0)),
            Some(CanvasHit::Terminal(Hit {
                id: "front".to_owned(),
                part: HitPart::Body,
            }))
        );
    }

    #[test]
    fn frame_target_caps_line_height_and_lasso_notices_clipboard_and_reconcile_work() {
        let mut wall = make_wall();
        // Tiny 20x5 terminal in a huge 1600x1000 viewport: uncapped zoom would exceed FRAME_MAX_LINE_PX / stride.y = 14.0.
        wall.add_terminal(TerminalSpec::new("tiny", 20, 5), true, None);
        wall.place_body("tiny", Point::new(0.0, 0.0));
        let target = wall.frame_target("tiny").unwrap();
        let max_zoom = FRAME_MAX_LINE_PX / wall.geometry().stride.y;
        assert!(
            (target.zoom - max_zoom).abs() < 1e-9,
            "expected zoom capped at {max_zoom}, got {}",
            target.zoom
        );

        // Lasso selection & toggle.
        wall.add_terminal(TerminalSpec::new("far", 20, 5), false, None);
        wall.place_body("far", Point::new(400.0, 0.0));
        let lasso = [
            Point::new(-5.0, 20.0),
            Point::new(30.0, 20.0),
            Point::new(30.0, -20.0),
            Point::new(-5.0, -20.0),
        ];
        wall.lasso_select(&lasso, false);
        assert_eq!(wall.selection(), &["tiny"]);
        wall.select_toggle("far");
        assert_eq!(wall.selection(), &["tiny", "far"]);

        // Notices expire at `now_ms + notice_ms`, and menu closes if its target hides.
        wall.open_menu(
            MenuTarget::Terminal {
                id: "far".to_owned(),
                part: TerminalPart::Body,
            },
            Point::new(200.0, 200.0),
        )
        .unwrap();
        assert!(wall.menu().is_some());
        wall.hide("far");
        assert!(wall.menu().is_none());

        wall.notify("clipboard denied", 1_000.0);
        assert_eq!(wall.notice(), Some("clipboard denied"));
        wall.tick(0.016, 5_999.0, 1600.0, 1000.0);
        assert_eq!(wall.notice(), Some("clipboard denied"));
        wall.tick(0.016, 6_000.0, 1600.0, 1000.0);
        assert_eq!(wall.notice(), None);

        // Selection clipboard + WriteClipboard effect under `copyOnSelect = clipboard`.
        wall.set_config("copyOnSelect", "clipboard").unwrap();
        wall.drain_effects();
        wall.selection_made("hello wall");
        assert_eq!(wall.selection_clipboard(), "hello wall");
        assert!(wall.drain_effects().contains(&WallEffect::WriteClipboard {
            text: "hello wall".to_owned()
        }));

        // Reconcile forgets dead store entries while preserving live/mounted ones.
        wall.store_mut()
            .set_name("stale-dead", "old build");
        wall.store_mut()
            .set_position("stale-dead", Point::new(9.0, 9.0));
        let forgotten = wall.reconcile_live(&["daemon-only"]);
        assert_eq!(forgotten, vec!["stale-dead"]);
        assert_eq!(wall.store().name("stale-dead"), None);
        assert!(wall.store().position("tiny").is_some());
    }

    #[test]
    fn groups_tile_cycle_focus_across_groups_and_spawn_at_snaps_window_corner() {
        let mut wall = make_wall();
        for (id, x, y) in [("a", 300.0, -40.0), ("b", 500.0, -90.0), ("c", 50.0, -60.0)] {
            wall.add_terminal(TerminalSpec::new(id, 80, 24), false, None);
            wall.place_body(id, Point::new(x, y));
        }
        let gid = wall
            .create_group(&["a", "b"], "backend", Some(GroupLayout::Tile), Some(2), Some(2.0))
            .unwrap();
        assert_eq!(wall.group_name_of(&gid).as_deref(), Some("backend"));

        // Cycling inside a 2-pane group stays within the group; group-cycling jumps to loose 'c' and back.
        wall.focus(Some("a"), false);
        assert_eq!(wall.cycle_focus(1).as_deref(), Some("b"));
        assert_eq!(wall.cycle_focus(1).as_deref(), Some("a"));
        assert_eq!(wall.cycle_group_focus(1).as_deref(), Some("c"));
        assert_eq!(wall.cycle_group_focus(1).as_deref(), Some("a"));

        // Explicit spawn `at` places window top-left below bar height and snaps body to gridMinor (4.0).
        wall.add_terminal(
            TerminalSpec::new("placed", 80, 24),
            true,
            Some(Point::new(101.0, 53.0)),
        );
        let placed_pos = wall.terminal("placed").unwrap().position;
        assert_eq!(placed_pos.x % 4.0, 0.0);
        assert_eq!(placed_pos.y % 4.0, 0.0);

        wall.ungroup(&gid).unwrap();
        assert!(wall.store().group(&gid).is_none());
    }

    #[test]
    fn set_geometry_preserving_panels_resizes_terminals_and_emits_effects() {
        let mut wall = make_wall();
        wall.add_terminal(TerminalSpec::new("t1", 100, 30), true, None);
        wall.drain_effects();
        let old_rect = wall.terminal_rect("t1").unwrap();

        // Doubling cell stride (e.g. fontSize 14 -> 28) halves cols & rows to keep panel footprint.
        let old_geom = wall.geometry();
        let bigger = CellGeometry {
            stride: Point::new(old_geom.stride.x * 2.0, old_geom.stride.y * 2.0),
            padding: old_geom.padding,
            scale_z: old_geom.scale_z,
        };
        wall.set_geometry_preserving_panels(bigger);
        let t1 = wall.terminal("t1").unwrap();
        assert_eq!((t1.cols, t1.rows), (50, 15));
        let new_rect = wall.terminal_rect("t1").unwrap();
        assert!((new_rect.width() - old_rect.width()).abs() < 1e-6);
        assert!((new_rect.height() - old_rect.height()).abs() < 1e-6);
        let effects = wall.drain_effects();
        assert!(effects.contains(&WallEffect::ResizeTerminal {
            id: "t1".to_owned(),
            cols: 50,
            rows: 15,
        }));

        // Line-height increase (stride.y * 1.5, stride.x unchanged) keeps cols and trims rows.
        let taller = CellGeometry {
            stride: Point::new(old_geom.stride.x, old_geom.stride.y * 1.5),
            padding: old_geom.padding,
            scale_z: old_geom.scale_z,
        };
        wall.set_geometry_preserving_panels(taller);
        let t1 = wall.terminal("t1").unwrap();
        assert_eq!((t1.cols, t1.rows), (100, 20));
    }
}
