//! Port of `wall/WallFullscreen.js`: the fullscreen state machine for a single
//! terminal or an entire group (`split`, `tile`, or `freeform`), with no DOM or
//! CSS backdrop.
//!
//! Entering fullscreen elevates the target above the canvas (`FULLSCREEN_Z` in
//! [`crate::layout`]), re-wraps panes to fill the viewport area left by docked
//! panels ([`crate::dock::dock_layout`]) less `fullscreenPad` at the target's
//! fullscreen text size, and eases the camera onto it while remembering the
//! pre-fullscreen canvas cols × rows, spot, and camera pose. Leaving restores
//! every re-wrapped terminal and eases the camera back to its canvas pose.
//!
//! Supports group fullscreen (`⌘↵` on a group with 2+ shown panes), single-pane
//! zoom out of and back into group fullscreen (`⌘⇧↵` via `return_group`), focus
//! transitions inside and across fullscreen targets, per-terminal and per-group
//! text size stepping and reset, debounced window/dock re-fits (`REFIT_SETTLE_MS`),
//! and reload adoption from [`crate::store::FullscreenState`].

use std::collections::BTreeMap;

use crate::chord::chord_label;
use crate::config::{GroupLayout, WallConfig};
use crate::dock::{dock_layout, DockPanel, DockSide, DockState};
use crate::geom::{Point, Rect};
use crate::input::Platform;
use crate::layout::{fit_rect, tile_layout, union_rect, GridSize, Size, TileOptions, BAR_ROOM};
use crate::split;
use crate::store::{
    CameraPose, FullscreenMode, FullscreenState, PanelSide, SavedCanvasPane, WallGroup, WallStore,
};

/// Smallest cell grid fullscreen ever makes (`MIN` in `wall/WallFullscreen.js`).
pub const MIN_FULLSCREEN_SIZE: GridSize = GridSize::new(20, 5);

/// Quiet time in milliseconds a window or dock change waits before re-fitting (`REFIT_SETTLE_MS`).
pub const REFIT_SETTLE_MS: f64 = 150.0;

/// Minimum fullscreen row height in CSS pixels (`LINE_PX.min`).
pub const MIN_LINE_PX: f64 = 4.0;

/// Maximum fullscreen row height in CSS pixels (`LINE_PX.max`).
pub const MAX_LINE_PX: f64 = 200.0;

const EDGE_EPS: f64 = 1e-6;

/// Available viewport rectangle left clear of docked side panels, in CSS pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FullscreenArea {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct AreaKey {
    left: f64,
    top: f64,
    width: i64,
    height: i64,
}

impl AreaKey {
    fn of(area: FullscreenArea) -> Self {
        Self {
            left: area.left,
            top: area.top,
            width: js_round_i64(area.width),
            height: js_round_i64(area.height),
        }
    }
}

/// Configuration values read by [`WallFullscreen`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FullscreenConfig {
    pub fullscreen_min_line_px: f64,
    pub fullscreen_pad: f64,
    pub fullscreen_text_step: f64,
    pub split_gap: f64,
    pub split_bar_rows: f64,
}

impl Default for FullscreenConfig {
    fn default() -> Self {
        Self::from(&WallConfig::default())
    }
}

impl From<&WallConfig> for FullscreenConfig {
    fn from(cfg: &WallConfig) -> Self {
        Self {
            fullscreen_min_line_px: cfg.fullscreen_min_line_px,
            fullscreen_pad: cfg.fullscreen_pad,
            fullscreen_text_step: cfg.fullscreen_text_step,
            split_gap: cfg.split_gap,
            split_bar_rows: cfg.split_bar_rows,
        }
    }
}

/// Plain-data snapshot of a live terminal grid's dimensions, origin, and cell stride.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FullscreenTerminal {
    pub cols: u32,
    pub rows: u32,
    pub position: Point,
    pub cell_stride: Point,
}

/// Options for [`WallFullscreen::enter_with`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnterOptions {
    /// Group to return to when unzooming from single-pane fullscreen (`⌘⇧↵`).
    pub return_group: Option<String>,
    /// Whether single-pane mode was explicitly requested (`true`) rather than
    /// entered as a fallback for a loose/1-pane terminal (`false`).
    pub solo_override: bool,
}

impl Default for EnterOptions {
    fn default() -> Self {
        Self {
            return_group: None,
            solo_override: true,
        }
    }
}

/// Direction for [`WallFullscreen::text`]: bigger (`+1`), smaller (`-1`), or reset (`0`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextStep {
    Smaller = -1,
    Reset = 0,
    Bigger = 1,
}

impl TextStep {
    /// Signed exponent (`-1.0`, `0.0`, or `1.0`) applied to `fullscreen_text_step`.
    pub const fn exponent(self) -> i32 {
        self as i32
    }
}

/// Convert [`WallStore`]'s four panel states into a [`DockState`] for [`dock_layout`].
pub fn dock_state_from_store(store: &WallStore) -> DockState {
    const fn map_side(side: PanelSide) -> DockSide {
        match side {
            PanelSide::Left => DockSide::Left,
            PanelSide::Right => DockSide::Right,
            PanelSide::Float => DockSide::Float,
        }
    }
    let sb = store.sidebar();
    let st = store.status_window();
    let se = store.settings_panel();
    let he = store.help_panel();
    DockState {
        sidebar: DockPanel::new(sb.open, map_side(sb.side)),
        status_window: DockPanel::new(st.open, map_side(st.side)),
        settings_panel: Some(DockPanel::new(se.open, map_side(se.side))),
        help_panel: Some(DockPanel::new(he.open, map_side(he.side))),
    }
}

/// Everything [`WallFullscreen`] asks of the wall, camera, groups, and store.
pub trait FullscreenHost {
    /// Platform for formatting reload chord notices (`⌘↵` vs `Ctrl+Alt+↵`).
    fn platform(&self) -> Platform {
        Platform::host()
    }

    /// Fullscreen and split configuration values.
    fn config(&self) -> FullscreenConfig;

    /// Current dock state of the four chrome panels.
    fn dock_state(&self) -> DockState;

    /// Canvas viewport dimensions `(w, h)` in CSS pixels.
    fn viewport_size(&self) -> Size;

    /// Current camera world position `(x, y)` and `zoom` (`WallCamera.camera`),
    /// captured when entering fullscreen from the open canvas.
    fn camera_pose(&self) -> CameraPose;

    /// Current canvas zoom in CSS pixels per world unit (`WallCamera.zoom`;
    /// defaults to `self.camera_pose().zoom`).
    fn viewport_zoom(&self) -> f64 {
        self.camera_pose().zoom
    }

    /// Ease the camera to `pose` (`WallCamera.easeTo`).
    fn ease_camera(&mut self, pose: CameraPose);

    /// Quench any in-flight wheel momentum (`WallPointer.quenchWheel`).
    fn quench_wheel(&mut self) {}

    /// Live terminal's grid size, position, and cell stride (`TerminalWall.terminal`).
    fn terminal(&self, id: &str) -> Option<FullscreenTerminal>;

    /// Whether terminal `id` exists on the wall and is not hidden (`TerminalWall.isShown`).
    fn is_shown(&self, id: &str) -> bool;

    /// World-space body rectangle of terminal `id` (`TerminalWall.terminalRect`).
    fn terminal_rect(&self, id: &str) -> Option<Rect>;

    /// World-space window rectangle (body + title bar at `zoom`) of terminal `id`
    /// (`TerminalWall.windowRect`); defaults to [`Self::terminal_rect`] when no
    /// title-bar height manager is attached.
    fn window_rect(&self, id: &str, _zoom: f64) -> Option<Rect> {
        self.terminal_rect(id)
    }

    /// Title-bar world height at `zoom` (`WallNames.barHeight`), or `None` when
    /// no title-bar manager is attached.
    fn bar_height(&self, _id: &str, _zoom: f64) -> Option<f64> {
        None
    }

    /// Cap or restore a split pane's title-bar world height (`WallNames.setBarRoom`).
    fn set_bar_room(&mut self, _id: &str, _room: f64) {}

    /// Resize terminal `id` to `cols` × `rows` (`terminal.resize`).
    fn resize_terminal(&mut self, id: &str, cols: u32, rows: u32);

    /// Set terminal `id`'s world-space grid origin directly.
    fn set_terminal_position(&mut self, id: &str, pos: Point);

    /// Move terminal `id` so its body's top-left corner sits at `top_left`
    /// (`TerminalWall.placeBody`).
    fn place_body(&mut self, id: &str, top_left: Point) {
        if let (Some(term), Some(rect)) = (self.terminal(id), self.terminal_rect(id)) {
            self.set_terminal_position(
                id,
                Point::new(
                    term.position.x + (top_left.x - rect.min_x),
                    term.position.y + (top_left.y - rect.max_y),
                ),
            );
        }
    }

    /// Currently focused terminal id, if any (`TerminalWall.focusedId`).
    fn focused_id(&self) -> Option<&str>;

    /// Focus navigation history oldest-to-newest (`TerminalWall._focusHistory`).
    fn focus_history(&self) -> &[String] {
        &[]
    }

    /// Focus terminal `id` (`TerminalWall.focus`).
    fn focus(&mut self, id: &str);

    /// Display name of terminal `id` (`TerminalWall.nameOf`).
    fn terminal_name(&self, id: &str) -> String;

    /// Display name of group `gid` (`WallGroups.nameOf`).
    fn group_name(&self, gid: &str) -> String {
        if let Some(g) = self.group(gid) {
            if !g.name.is_empty() {
                return g.name.clone();
            }
        }
        let n = gid
            .strip_prefix("g-")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        format!("group {n}")
    }

    /// Show a top-level wall notice (`TerminalWall.notify`).
    fn notify(&mut self, text: String);

    /// Sync terminal visibility and Z-stacking with `elevated_ids` (`TerminalWall.applyCanvas`).
    fn apply_canvas(&mut self, _elevated_ids: &[String]) {}

    /// Re-layout group `gid` on the canvas after fullscreen exits (`WallGroups.relayout`).
    fn relayout_group(&mut self, _gid: &str) {}

    /// Notify listeners that wall state changed (`TerminalWall._emit`).
    fn emit(&mut self) {}

    /// Look up persisted group `gid` (`WallStore.group`).
    fn group(&self, gid: &str) -> Option<&WallGroup>;

    /// Look up which group owns terminal `id`, if any (`WallStore.groupOf`).
    fn group_of(&self, id: &str) -> Option<&str>;

    /// Read persisted fullscreen state (`WallStore.fullscreen`).
    fn saved_fullscreen(&self) -> Option<&FullscreenState>;

    /// Persist or clear fullscreen state (`WallStore.setFullscreen`).
    fn set_saved_fullscreen(&mut self, state: Option<FullscreenState>);

    /// Read remembered fullscreen row height in CSS px (`WallStore.fullscreenText`).
    fn fullscreen_text(&self, key: &str) -> Option<f64>;

    /// Persist or clear remembered fullscreen row height (`WallStore.setFullscreenText`).
    fn set_fullscreen_text(&mut self, key: &str, px: Option<f64>);
}

/// Fullscreen state machine (`WallFullscreen` in `wall/WallFullscreen.js`).
#[derive(Clone, Debug, Default)]
pub struct WallFullscreen {
    state: Option<FullscreenState>,
    area_key: Option<AreaKey>,
    refit_at: Option<f64>,
    fs_split_rect: Option<Rect>,
    fs_split_first_member: Option<String>,
    refitting: bool,
    adopted_notice_shown: bool,
}

impl WallFullscreen {
    /// Construct an inactive [`WallFullscreen`].
    pub const fn new() -> Self {
        Self {
            state: None,
            area_key: None,
            refit_at: None,
            fs_split_rect: None,
            fs_split_first_member: None,
            refitting: false,
            adopted_notice_shown: false,
        }
    }

    /// Active [`FullscreenState`], if any.
    pub fn state(&self) -> Option<&FullscreenState> {
        self.state.as_ref()
    }

    /// True while any fullscreen (single pane or group) is active.
    pub const fn active(&self) -> bool {
        self.state.is_some()
    }

    /// [`FullscreenMode::Single`], [`FullscreenMode::Group`], or `None`.
    pub fn mode(&self) -> Option<FullscreenMode> {
        self.state.as_ref().map(effective_mode)
    }

    /// Solo fullscreen terminal id (`mode == Single`), else `None`.
    pub fn id(&self) -> Option<&str> {
        if self.mode() == Some(FullscreenMode::Single) {
            self.state.as_ref()?.id.as_deref()
        } else {
            None
        }
    }

    /// Alias for [`Self::id`]: the lone terminal whose own `TitleBar` is hidden in fullscreen.
    pub fn solo_id(&self) -> Option<&str> {
        self.id()
    }

    /// Fullscreen group id (`mode == Group`), else `None`.
    pub fn group_id(&self) -> Option<&str> {
        if self.mode() == Some(FullscreenMode::Group) {
            self.state.as_ref()?.group_id.as_deref()
        } else {
            None
        }
    }

    /// Layout of the active fullscreen group, else `None`.
    pub fn group_layout(&self, host: &impl FullscreenHost) -> Option<GroupLayout> {
        let gid = self.group_id()?;
        Some(host.group(gid)?.layout)
    }

    /// Identifier surfaced in `snapshot().fullscreen`: group id or solo terminal id, else `None`.
    pub fn target(&self) -> Option<&str> {
        let _ = self.state.as_ref()?;
        self.group_id().or_else(|| self.id())
    }

    /// Shown terminal ids currently elevated into the fullscreen Z-band.
    pub fn elevated_ids(&self, host: &impl FullscreenHost) -> Vec<String> {
        let Some(s) = &self.state else {
            return Vec::new();
        };
        if effective_mode(s) == FullscreenMode::Group {
            return match s.group_id.as_deref() {
                Some(gid) => shown_of(host, gid),
                None => Vec::new(),
            };
        }
        match s.id.as_deref() {
            Some(id) if host.is_shown(id) => vec![id.to_owned()],
            _ => Vec::new(),
        }
    }

    /// Whether terminal `id` is currently part of the active fullscreen view.
    pub fn has(&self, host: &impl FullscreenHost, id: &str) -> bool {
        if id.is_empty() {
            return false;
        }
        let Some(s) = &self.state else {
            return false;
        };
        if effective_mode(s) == FullscreenMode::Single {
            return s.id.as_deref() == Some(id);
        }
        s.group_id
            .as_deref()
            .is_some_and(|gid| shown_of(host, gid).iter().any(|m| m == id))
    }

    /// Whether terminal `id` has a saved pre-fullscreen canvas geometry.
    pub fn has_saved(&self, id: &str) -> bool {
        if id.is_empty() {
            return false;
        }
        let Some(s) = &self.state else {
            return false;
        };
        s.saved.as_ref().is_some_and(|m| m.contains_key(id))
            || (s.id.as_deref() == Some(id) && s.canvas.is_some())
    }

    /// Active fullscreen split rect for group `group`, or `None` when `group`
    /// is not currently fullscreened as a split (`WallFullscreen.splitRectForGroup`).
    pub fn split_rect_for_group(&self, group: &WallGroup) -> Option<Rect> {
        if self.mode() != Some(FullscreenMode::Group) || group.members.is_empty() {
            return None;
        }
        let rect = self.fs_split_rect?;
        let first = self.fs_split_first_member.as_deref()?;
        group.members.iter().any(|m| m == first).then_some(rect)
    }

    /// Pre-fullscreen body rect of `id` (defaults to [`Self::id`] when `None`),
    /// reconstructed from its saved `{ cols, rows, x, y }` so directional pane
    /// navigation and canvas group frames read where it lives on the canvas.
    pub fn canvas_body_rect(&self, host: &impl FullscreenHost, id: Option<&str>) -> Option<Rect> {
        let target_id = id.or_else(|| self.id())?;
        let s = self.state.as_ref()?;
        let sv = saved_pane_of(s, target_id)?;
        let grid = host.terminal(target_id)?;
        let rect = host.terminal_rect(target_id)?;
        let stride = grid.cell_stride;
        let frame_x = rect.max_x - rect.min_x - f64::from(grid.cols) * stride.x;
        let frame_y = rect.max_y - rect.min_y - f64::from(grid.rows) * stride.y;
        let min_x = rect.min_x + (sv.x - grid.position.x);
        let max_y = rect.max_y + (sv.y - grid.position.y);
        Some(Rect {
            min_x,
            max_x: min_x + f64::from(sv.cols) * stride.x + frame_x,
            min_y: max_y - (f64::from(sv.rows) * stride.y + frame_y),
            max_y,
        })
    }

    /// Viewport region fullscreen fills: the viewport less docked side panels, in CSS pixels.
    pub fn area(&self, host: &impl FullscreenHost) -> FullscreenArea {
        let view = host.viewport_size();
        let edges = dock_layout(&host.dock_state()).edges;
        FullscreenArea {
            left: edges.left,
            top: 0.0,
            width: (view.w - edges.left - edges.right).max(0.0),
            height: view.h,
        }
    }

    /// Whether a debounced window/dock re-fit is waiting to settle (`tick`).
    pub const fn settling(&self) -> bool {
        self.refit_at.is_some()
    }

    /// Default fullscreen toggle (`⌘↵`): exits active fullscreen when `target` is
    /// `None` or matches the active solo/group target; otherwise enters group
    /// fullscreen for a named group or when the target terminal belongs to a group
    /// with 2+ shown panes, else solo fullscreen.
    pub fn toggle(&mut self, host: &mut impl FullscreenHost, target: Option<&str>) {
        if self.state.is_some()
            && (target.is_none() || target == self.id() || target == self.group_id())
        {
            self.exit(host);
            return;
        }
        if let Some(t) = target {
            if host.group(t).is_some() {
                self.enter_group(host, t);
                return;
            }
        }
        let resolved = target
            .map(str::to_owned)
            .or_else(|| host.focused_id().map(str::to_owned));
        if let Some(id) = resolved {
            self.enter_default(host, &id);
        }
    }

    /// Enter group fullscreen if `id` belongs to a group with 2+ shown members,
    /// else single-pane fullscreen on `id` with `solo_override: false`.
    pub fn enter_default(&mut self, host: &mut impl FullscreenHost, id: &str) -> bool {
        if id.is_empty() {
            return false;
        }
        if let Some(gid) = host.group_of(id).map(str::to_owned) {
            if shown_of(host, &gid).len() >= 2 {
                return self.enter_group(host, &gid);
            }
        }
        self.enter_with(
            host,
            id,
            EnterOptions {
                return_group: None,
                solo_override: false,
            },
        )
    }

    /// Single-pane fullscreen / zoom toggle (`⌘⇧↵`):
    /// - From canvas: enters solo fullscreen on `id` or `focused_id`.
    /// - From group fullscreen: zooms `id` (else `focused_id` in group, else first shown)
    ///   solo while remembering `return_group`.
    /// - From solo fullscreen: switches to `id` if different; else unzooms back to
    ///   `return_group` if it still has 2+ shown members, or exits to the canvas.
    pub fn toggle_pane(&mut self, host: &mut impl FullscreenHost, id: Option<&str>) -> bool {
        if self.state.is_none() {
            let Some(target) = id
                .map(str::to_owned)
                .or_else(|| host.focused_id().map(str::to_owned))
            else {
                return false;
            };
            return self.enter(host, &target);
        }

        if self.mode() == Some(FullscreenMode::Group) {
            let Some(gid) = self.group_id().map(str::to_owned) else {
                return false;
            };
            let shown = shown_of(host, &gid);
            let chosen = id.map(str::to_owned).or_else(|| {
                let focused = host
                    .focused_id()
                    .filter(|f| shown.iter().any(|s| s == *f))
                    .map(str::to_owned);
                focused.or_else(|| shown.first().cloned())
            });
            let Some(target) = chosen else {
                return false;
            };
            return self.enter_with(
                host,
                &target,
                EnterOptions {
                    return_group: Some(gid),
                    solo_override: true,
                },
            );
        }

        // Currently in single-pane fullscreen:
        if let Some(other) = id.filter(|other| Some(*other) != self.id()) {
            let current_rg = self
                .state
                .as_ref()
                .and_then(|s| s.return_group.clone());
            let keep_rg = current_rg.filter(|rg| host.group_of(other) == Some(rg.as_str()));
            return self.enter_with(
                host,
                other,
                EnterOptions {
                    return_group: keep_rg,
                    solo_override: true,
                },
            );
        }

        let rg = self
            .state
            .as_ref()
            .and_then(|s| s.return_group.clone());
        if let Some(rg_id) = rg {
            if host.group(&rg_id).is_some() && shown_of(host, &rg_id).len() >= 2 {
                return self.enter_group(host, &rg_id);
            }
        }
        self.exit(host);
        false
    }

    /// Put terminal `id` into single-pane fullscreen with default [`EnterOptions`].
    pub fn enter(&mut self, host: &mut impl FullscreenHost, id: &str) -> bool {
        self.enter_with(host, id, EnterOptions::default())
    }

    /// Put terminal `id` into single-pane fullscreen (`mode == Single`).
    pub fn enter_with(
        &mut self,
        host: &mut impl FullscreenHost,
        id: &str,
        opts: EnterOptions,
    ) -> bool {
        let Some(grid) = host.terminal(id) else {
            return false;
        };
        if !host.is_shown(id) {
            return false;
        }
        if self.mode() == Some(FullscreenMode::Single) && self.id() == Some(id) {
            if let Some(s) = self.state.as_mut() {
                s.return_group = opts.return_group;
                s.solo_override = Some(opts.solo_override);
            }
            self.refit(host);
            return true;
        }

        let prev = self.state.take();
        let camera = prev
            .as_ref()
            .map(|p| p.camera)
            .unwrap_or_else(|| host.camera_pose());
        let prior_canvas = prev.as_ref().and_then(|p| saved_pane_of(p, id));
        if let Some(ref prev_state) = prev {
            restore_state(
                host,
                prev_state,
                if prior_canvas.is_some() {
                    Some(id)
                } else {
                    None
                },
            );
        }

        let canvas_snap = prior_canvas.unwrap_or(SavedCanvasPane {
            cols: grid.cols,
            rows: grid.rows,
            x: grid.position.x,
            y: grid.position.y,
        });
        let cfg = host.config();
        let canvas_px = grid.cell_stride.y * host.viewport_zoom();
        let entry_line_px = prev
            .as_ref()
            .map(|p| p.entry_line_px)
            .unwrap_or_else(|| clamp_line(canvas_px.max(cfg.fullscreen_min_line_px)));

        self.fs_split_rect = None;
        self.fs_split_first_member = None;

        let line_px = host.fullscreen_text(id).unwrap_or(entry_line_px);
        let new_state = FullscreenState {
            mode: Some(FullscreenMode::Single),
            id: Some(id.to_owned()),
            group_id: None,
            return_group: opts.return_group,
            solo_override: Some(opts.solo_override),
            canvas: Some(canvas_snap),
            saved: Some(BTreeMap::from([(id.to_owned(), canvas_snap)])),
            camera,
            line_px,
            entry_line_px,
            zoom_scale: Some(1.0),
        };
        host.set_saved_fullscreen(Some(new_state.clone()));
        self.state = Some(new_state);

        host.quench_wheel();
        if host.focused_id() != Some(id) {
            host.focus(id);
        }
        let elevated = self.elevated_ids(host);
        host.apply_canvas(&elevated);
        self.refit(host);
        host.emit();
        true
    }

    /// Put group `gid` fullscreen (`mode == Group`).
    pub fn enter_group(&mut self, host: &mut impl FullscreenHost, gid: &str) -> bool {
        let Some(group_layout) = host.group(gid).map(|g| g.layout) else {
            return false;
        };
        let shown = shown_of(host, gid);
        if shown.is_empty() {
            return false;
        }
        if shown.len() == 1 && group_layout != GroupLayout::Freeform {
            return self.enter(host, &shown[0]);
        }
        if self.mode() == Some(FullscreenMode::Group) && self.group_id() == Some(gid) {
            self.refit(host);
            return true;
        }

        let prev = self.state.take();
        let camera = prev
            .as_ref()
            .map(|p| p.camera)
            .unwrap_or_else(|| host.camera_pose());
        let prev_saved: BTreeMap<String, SavedCanvasPane> = match &prev {
            Some(p) => {
                if let Some(ref map) = p.saved {
                    map.clone()
                } else if let (Some(ref pid), Some(c)) = (&p.id, p.canvas) {
                    BTreeMap::from([(pid.clone(), c)])
                } else {
                    BTreeMap::new()
                }
            }
            None => BTreeMap::new(),
        };
        if let Some(ref prev_state) = prev {
            restore_state(host, prev_state, None);
        }

        let mut saved = BTreeMap::new();
        if group_layout != GroupLayout::Freeform {
            for member in &shown {
                let Some(mg) = host.terminal(member) else {
                    continue;
                };
                let pane = prev_saved.get(member).copied().unwrap_or(SavedCanvasPane {
                    cols: mg.cols,
                    rows: mg.rows,
                    x: mg.position.x,
                    y: mg.position.y,
                });
                saved.insert(member.clone(), pane);
            }
        }

        let Some(ref_term) = host.terminal(&shown[0]) else {
            return false;
        };
        let cfg = host.config();
        let canvas_px = ref_term.cell_stride.y * host.viewport_zoom();
        let entry_line_px = prev
            .as_ref()
            .map(|p| p.entry_line_px)
            .unwrap_or_else(|| clamp_line(canvas_px.max(cfg.fullscreen_min_line_px)));
        let text_key = format!("group:{gid}");
        let line_px = host.fullscreen_text(&text_key).unwrap_or(entry_line_px);

        self.fs_split_rect = None;
        self.fs_split_first_member = None;

        let new_state = FullscreenState {
            mode: Some(FullscreenMode::Group),
            id: None,
            group_id: Some(gid.to_owned()),
            return_group: None,
            solo_override: None,
            canvas: None,
            saved: Some(saved),
            camera,
            line_px,
            entry_line_px,
            zoom_scale: Some(1.0),
        };
        host.set_saved_fullscreen(Some(new_state.clone()));
        self.state = Some(new_state);

        host.quench_wheel();
        let already_in_group = host
            .focused_id()
            .is_some_and(|f| shown.iter().any(|m| m == f));
        if !already_in_group {
            let mut pick = shown[0].clone();
            for entry in host.focus_history().iter().rev() {
                if shown.iter().any(|m| m == entry) {
                    pick = entry.clone();
                    break;
                }
            }
            host.focus(&pick);
        }

        let elevated = self.elevated_ids(host);
        host.apply_canvas(&elevated);
        self.refit(host);
        host.emit();
        true
    }

    /// Handle focus moving to `id` while fullscreen is active:
    /// - Inside group fullscreen: focusing another member of the same group stays
    ///   in place; focusing outside transitions via [`Self::enter_default`].
    /// - Inside single-pane fullscreen: moves fullscreen to `id` (via
    ///   [`Self::enter_default`] when `solo_override == Some(false)`, else solo
    ///   while keeping `return_group` if `id` belongs to it).
    pub fn on_focus(&mut self, host: &mut impl FullscreenHost, id: &str) {
        if id.is_empty() || self.state.is_none() {
            return;
        }
        if self.mode() == Some(FullscreenMode::Group) {
            if host.group_of(id) == self.group_id() {
                return;
            }
            self.enter_default(host, id);
            return;
        }
        if self.id() != Some(id) {
            let solo_override = self
                .state
                .as_ref()
                .and_then(|s| s.solo_override)
                .unwrap_or(true);
            if !solo_override {
                self.enter_default(host, id);
                return;
            }
            let rg = self
                .state
                .as_ref()
                .and_then(|s| s.return_group.clone());
            let keep_rg = rg.filter(|g| host.group_of(id) == Some(g.as_str()));
            self.enter_with(
                host,
                id,
                EnterOptions {
                    return_group: keep_rg,
                    solo_override: true,
                },
            );
        }
    }

    /// Leave fullscreen, restoring all saved canvas geometries and easing the
    /// camera back to its saved canvas pose.
    pub fn exit(&mut self, host: &mut impl FullscreenHost) {
        let Some(s) = self.state.take() else {
            return;
        };
        let gid = s.group_id.clone().or_else(|| {
            s.id.as_deref()
                .and_then(|id| host.group_of(id))
                .map(str::to_owned)
        });
        self.fs_split_rect = None;
        self.fs_split_first_member = None;
        self.adopted_notice_shown = false;

        host.set_saved_fullscreen(None);
        host.quench_wheel();
        restore_state(host, &s, None);
        host.apply_canvas(&[]);
        if let Some(ref g) = gid {
            if host.group(g).is_some() {
                host.relayout_group(g);
            }
        }
        host.ease_camera(s.camera);
        host.emit();
    }

    /// Give `id` its canvas size and spot back before it hides; in single-pane
    /// fullscreen exits immediately, while in group fullscreen [`Self::after_member_left`]
    /// finishes refitting or exiting once the hidden mark is committed.
    pub fn on_hide(&mut self, host: &mut impl FullscreenHost, id: &str) {
        if self.state.is_none() {
            return;
        }
        if self.mode() == Some(FullscreenMode::Single) {
            if self.id() == Some(id) {
                self.exit(host);
            }
            return;
        }
        let removed = self
            .state
            .as_mut()
            .and_then(|s| s.saved.as_mut()?.remove(id));
        if let Some(sv) = removed {
            give_back_one(host, id, sv);
        }
    }

    /// Re-fit remaining members of a fullscreen group after a member hid or left,
    /// or exit when none remain shown.
    pub fn after_member_left(&mut self, host: &mut impl FullscreenHost) {
        if self.mode() != Some(FullscreenMode::Group) {
            return;
        }
        let shown = self
            .group_id()
            .filter(|gid| host.group(gid).is_some())
            .map(|gid| shown_of(host, gid))
            .unwrap_or_default();
        if shown.is_empty() {
            self.exit(host);
            return;
        }
        if let Some(ref s) = self.state {
            host.set_saved_fullscreen(Some(s.clone()));
        }
        let elevated = self.elevated_ids(host);
        host.apply_canvas(&elevated);
        self.refit(host);
    }

    /// Handle terminal `id` closing or dying: ends single-pane fullscreen on `id`,
    /// or re-fits remaining group members (ending fullscreen when none remain).
    pub fn gone(&mut self, host: &mut impl FullscreenHost, id: &str) {
        let Some(ref s) = self.state else {
            return;
        };
        if effective_mode(s) == FullscreenMode::Single {
            if s.id.as_deref() != Some(id) {
                return;
            }
            let cam = s.camera;
            self.state = None;
            self.fs_split_rect = None;
            self.fs_split_first_member = None;
            host.set_saved_fullscreen(None);
            host.quench_wheel();
            host.apply_canvas(&[]);
            host.ease_camera(cam);
            host.emit();
            return;
        }

        if let Some(ref mut state) = self.state {
            if let Some(ref mut saved) = state.saved {
                saved.remove(id);
            }
        }
        let shown = self
            .group_id()
            .filter(|gid| host.group(gid).is_some())
            .map(|gid| shown_of(host, gid))
            .unwrap_or_default();
        if shown.is_empty() {
            let cam = self
                .state
                .take()
                .expect("checked Some above")
                .camera;
            self.fs_split_rect = None;
            self.fs_split_first_member = None;
            host.set_saved_fullscreen(None);
            host.quench_wheel();
            host.apply_canvas(&[]);
            host.ease_camera(cam);
            host.emit();
            return;
        }
        if let Some(ref state) = self.state {
            host.set_saved_fullscreen(Some(state.clone()));
        }
        let elevated = self.elevated_ids(host);
        host.apply_canvas(&elevated);
        self.refit(host);
        host.emit();
    }

    /// When terminal `id` registers on startup/reload, resume any saved fullscreen
    /// state naming it (or its group) and post the one-time reload notice.
    pub fn adopt(&mut self, host: &mut impl FullscreenHost, id: &str) {
        if self.state.is_some() {
            if self.mode() == Some(FullscreenMode::Group)
                && host.group_of(id) == self.group_id()
                && host.is_shown(id)
            {
                let gid = self.group_id().map(str::to_owned);
                let is_non_freeform = gid
                    .as_deref()
                    .and_then(|g| host.group(g))
                    .is_some_and(|g| g.layout != GroupLayout::Freeform);
                if is_non_freeform {
                    let already = self
                        .state
                        .as_ref()
                        .and_then(|s| s.saved.as_ref())
                        .is_some_and(|m| m.contains_key(id));
                    if !already {
                        if let (Some(grid), Some(s)) = (host.terminal(id), self.state.as_mut()) {
                            s.saved.get_or_insert_with(BTreeMap::new).insert(
                                id.to_owned(),
                                SavedCanvasPane {
                                    cols: grid.cols,
                                    rows: grid.rows,
                                    x: grid.position.x,
                                    y: grid.position.y,
                                },
                            );
                        }
                    }
                }
                let elevated = self.elevated_ids(host);
                host.apply_canvas(&elevated);
                self.refit(host);
                host.emit();
            }
            return;
        }

        let Some(saved) = host.saved_fullscreen().cloned() else {
            return;
        };
        if let Some(ref gid) = saved.group_id {
            if host.group_of(id) != Some(gid.as_str()) || !host.is_shown(id) {
                return;
            }
            let Some(group_layout) = host.group(gid).map(|g| g.layout) else {
                return;
            };
            let mut resumed = saved.clone();
            resumed.mode = Some(FullscreenMode::Group);
            if group_layout != GroupLayout::Freeform {
                let map = resumed.saved.get_or_insert_with(BTreeMap::new);
                for member in shown_of(host, gid) {
                    if let std::collections::btree_map::Entry::Vacant(vacant) = map.entry(member) {
                        if let Some(grid) = host.terminal(vacant.key()) {
                            vacant.insert(SavedCanvasPane {
                                cols: grid.cols,
                                rows: grid.rows,
                                x: grid.position.x,
                                y: grid.position.y,
                            });
                        }
                    }
                }
            }
            self.state = Some(resumed);
            let elevated = self.elevated_ids(host);
            host.apply_canvas(&elevated);
            self.refit(host);
            if !self.adopted_notice_shown {
                self.adopted_notice_shown = true;
                let label = host.group_name(gid);
                let chord = chord_label(host.platform(), &["↵"], false);
                host.notify(format!(
                    "fullscreen: {label} — {chord} shows the whole wall again"
                ));
            }
            host.emit();
            return;
        }

        if saved.id.as_deref() != Some(id) || !host.is_shown(id) {
            return;
        }
        let fallback_canvas = saved
            .canvas
            .or_else(|| saved.saved.as_ref()?.get(id).copied())
            .or_else(|| {
                host.terminal(id).map(|g| SavedCanvasPane {
                    cols: g.cols,
                    rows: g.rows,
                    x: g.position.x,
                    y: g.position.y,
                })
            });
        let saved_map = match &saved.saved {
            Some(m) => Some(m.clone()),
            None => fallback_canvas.map(|c| BTreeMap::from([(id.to_owned(), c)])),
        };
        self.state = Some(FullscreenState {
            mode: Some(FullscreenMode::Single),
            canvas: fallback_canvas,
            saved: saved_map,
            ..saved
        });
        let elevated = self.elevated_ids(host);
        host.apply_canvas(&elevated);
        self.refit(host);
        let name = host.terminal_name(id);
        let chord = chord_label(host.platform(), &["↵"], false);
        host.notify(format!(
            "fullscreen: {name} — {chord} shows the whole wall again"
        ));
        host.emit();
    }

    /// Step or reset the active fullscreen text size (`+1` bigger, `-1` smaller,
    /// `0` back to entry size), returning the new row height in CSS pixels.
    pub fn text(&mut self, host: &mut impl FullscreenHost, step_dir: TextStep) -> Option<f64> {
        self.state.as_ref()?;
        let step = host.config().fullscreen_text_step;
        let is_freeform_group = self.mode() == Some(FullscreenMode::Group)
            && self.group_layout(host) == Some(GroupLayout::Freeform);

        if is_freeform_group {
            let line_px = {
                let s = self.state.as_mut().expect("checked Some above");
                let next_scale = if step_dir == TextStep::Reset {
                    1.0
                } else {
                    let cur = s.zoom_scale.unwrap_or(1.0);
                    (cur * step.powi(step_dir.exponent())).clamp(0.2, 8.0)
                };
                s.zoom_scale = Some(next_scale);
                s.line_px = clamp_line(s.entry_line_px * next_scale);
                host.set_saved_fullscreen(Some(s.clone()));
                s.line_px
            };
            self.refit(host);
            host.emit();
            return Some(line_px);
        }

        let key = if self.mode() == Some(FullscreenMode::Group) {
            self.group_id().map(|gid| format!("group:{gid}"))
        } else {
            self.id().map(str::to_owned)
        };
        let line_px = {
            let s = self.state.as_mut().expect("checked Some above");
            if step_dir == TextStep::Reset {
                s.line_px = s.entry_line_px;
                if let Some(ref k) = key {
                    host.set_fullscreen_text(k, None);
                }
            } else {
                s.line_px = clamp_line(s.line_px * step.powi(step_dir.exponent()));
                if let Some(ref k) = key {
                    host.set_fullscreen_text(k, Some(s.line_px));
                }
            }
            host.set_saved_fullscreen(Some(s.clone()));
            s.line_px
        };
        self.refit(host);
        host.emit();
        Some(line_px)
    }

    /// Fit the active fullscreen view (single terminal, or split / tile / freeform
    /// group) into [`Self::area`].
    pub fn refit(&mut self, host: &mut impl FullscreenHost) -> Option<GridSize> {
        if self.refitting || self.state.is_none() {
            return None;
        }
        self.refitting = true;
        let area = self.area(host);
        self.area_key = Some(AreaKey::of(area));
        self.refit_at = None;
        let result = if self.mode() == Some(FullscreenMode::Group) {
            self.refit_group(host, area)
        } else {
            self.refit_single(host, area)
        };
        self.refitting = false;
        result
    }

    /// Per-frame settle check: when [`Self::area`] changes, schedules a re-fit
    /// [`REFIT_SETTLE_MS`] after the last change and executes it once `now_ms`
    /// reaches that deadline.
    pub fn tick(&mut self, host: &mut impl FullscreenHost, now_ms: f64) {
        if self.state.is_none() {
            return;
        }
        let key = AreaKey::of(self.area(host));
        if self.area_key != Some(key) {
            self.area_key = Some(key);
            self.refit_at = Some(now_ms + REFIT_SETTLE_MS);
        }
        if let Some(deadline) = self.refit_at {
            if now_ms >= deadline {
                self.refit(host);
            }
        }
    }

    fn refit_single(
        &mut self,
        host: &mut impl FullscreenHost,
        area: FullscreenArea,
    ) -> Option<GridSize> {
        let s = self.state.as_ref()?;
        let id = s.id.as_deref()?;
        let line_px = s.line_px;
        let grid = host.terminal(id)?;
        let rect = host.terminal_rect(id)?;
        self.fs_split_rect = None;
        self.fs_split_first_member = None;

        let stride = grid.cell_stride;
        let zoom = line_px / stride.y;
        let cell_w = stride.x * zoom;
        let cell_h = line_px;
        let pad = host.config().fullscreen_pad;
        let view = host.viewport_size();
        let frame_x = (rect.max_x - rect.min_x - f64::from(grid.cols) * stride.x) * zoom;
        let frame_y = (rect.max_y - rect.min_y - f64::from(grid.rows) * stride.y) * zoom;

        let raw_cols = ((area.width - 2.0 * pad - frame_x) / cell_w).floor();
        let raw_rows = ((area.height - 2.0 * pad - frame_y) / cell_h).floor();
        let cols = MIN_FULLSCREEN_SIZE
            .cols
            .max(raw_cols.max(0.0) as u32);
        let rows = MIN_FULLSCREEN_SIZE
            .rows
            .max(raw_rows.max(0.0) as u32);
        if cols != grid.cols || rows != grid.rows {
            host.resize_terminal(id, cols, rows);
        }

        let left = area.left + ((area.width - f64::from(cols) * cell_w - frame_x) / 2.0).max(0.0);
        let top = area.top + ((area.height - f64::from(rows) * cell_h - frame_y) / 2.0).max(0.0);
        host.ease_camera(CameraPose {
            x: rect.min_x + (view.w / 2.0 - left) / zoom,
            y: rect.max_y - (view.h / 2.0 - top) / zoom,
            zoom,
        });
        Some(GridSize::new(cols, rows))
    }

    fn refit_group(
        &mut self,
        host: &mut impl FullscreenHost,
        area: FullscreenArea,
    ) -> Option<GridSize> {
        let gid = self.group_id()?.to_owned();
        let group = host.group(&gid)?.clone();
        let shown = shown_of(host, &gid);
        if shown.is_empty() {
            return None;
        }
        match group.layout {
            GroupLayout::Split => self.refit_split(host, &group, &shown, area),
            GroupLayout::Tile => self.refit_tile(host, &group, &shown, area),
            GroupLayout::Freeform => self.refit_freeform(host, &shown, area),
        }
    }

    fn refit_split(
        &mut self,
        host: &mut impl FullscreenHost,
        group: &WallGroup,
        shown: &[String],
        area: FullscreenArea,
    ) -> Option<GridSize> {
        let line_px = self.state.as_ref()?.line_px;
        let split_spec = group.split.as_ref()?;
        let ref_term = host.terminal(&shown[0])?;
        let stride = ref_term.cell_stride;
        let cfg = host.config();
        let pad = cfg.fullscreen_pad;
        let split_gap = cfg.split_gap;
        let room = if split_spec.bars {
            cfg.split_bar_rows * stride.y
        } else {
            0.0
        };
        let f0 = frame_of(host, &shown[0])?;

        let mut tree = to_split_tree(&split_spec.tree);
        for member in &group.members {
            if !shown.iter().any(|s| s == member) {
                if let Some(pruned) = split::remove(&tree, member) {
                    tree = pruned;
                }
            }
        }

        let min = split::min_size(
            &tree,
            |_| {
                split::Size::new(
                    f64::from(MIN_FULLSCREEN_SIZE.cols) * stride.x + f0.x,
                    f64::from(MIN_FULLSCREEN_SIZE.rows) * stride.y + f0.y + room,
                )
            },
            split_gap,
        );
        let mut zoom = line_px / stride.y;
        let fit_w = (area.width - 2.0 * pad).max(1.0);
        let fit_h = (area.height - 2.0 * pad).max(1.0);
        if fit_w / zoom < min.w || fit_h / zoom < min.h {
            zoom = 0.05_f64.max((fit_w / min.w).min(fit_h / min.h));
        }
        let avail_w = min.w.max(fit_w / zoom);
        let avail_h = min.h.max(fit_h / zoom);
        let origin = group.origin.unwrap_or(Point::new(0.0, 0.0));
        let fs_rect = Rect {
            min_x: origin.x,
            min_y: origin.y - avail_h,
            max_x: origin.x + avail_w,
            max_y: origin.y,
        };
        self.fs_split_rect = Some(fs_rect);
        self.fs_split_first_member = group.members.first().cloned();

        let mut first_size: Option<GridSize> = None;
        for (id, pr) in split::panes(&tree, fs_rect, split_gap) {
            let Some(grid) = host.terminal(&id) else {
                continue;
            };
            let Some(f) = frame_of(host, &id) else {
                continue;
            };
            let cols = MIN_FULLSCREEN_SIZE.cols.max(
                (((pr.max_x - pr.min_x - f.x) / stride.x + EDGE_EPS).floor()).max(0.0) as u32,
            );
            let rows = MIN_FULLSCREEN_SIZE.rows.max(
                (((pr.max_y - pr.min_y - room - f.y) / stride.y + EDGE_EPS).floor()).max(0.0)
                    as u32,
            );
            if cols != grid.cols || rows != grid.rows {
                host.resize_terminal(&id, cols, rows);
            }
            host.place_body(&id, Point::new(pr.min_x, pr.max_y - room));
            host.set_bar_room(&id, room);
            first_size.get_or_insert(GridSize::new(cols, rows));
        }

        let rects: Vec<Rect> = shown
            .iter()
            .filter_map(|id| {
                if split_spec.bars {
                    host.window_rect(id, zoom)
                } else {
                    host.terminal_rect(id)
                }
            })
            .collect();
        if let Some(u) = union_rect(&rects) {
            center_rect_in_area(host, u, area, zoom);
        }
        first_size
    }

    fn refit_tile(
        &mut self,
        host: &mut impl FullscreenHost,
        group: &WallGroup,
        shown: &[String],
        area: FullscreenArea,
    ) -> Option<GridSize> {
        self.fs_split_rect = None;
        self.fs_split_first_member = None;
        let s = self.state.as_ref()?;
        let line_px = s.line_px;
        let ref_term = host.terminal(&shown[0])?;
        let stride = ref_term.cell_stride;
        let pad = host.config().fullscreen_pad;
        let gap = group.gap;
        let cols_count = (group.columns as usize).min(shown.len()).max(1);
        let rows_count = shown.len().div_ceil(cols_count);

        struct OrigPane {
            id: String,
            grid: FullscreenTerminal,
            frame: Point,
            w0: f64,
            h0: f64,
        }

        let mut orig = Vec::with_capacity(shown.len());
        for id in shown {
            let grid = host.terminal(id)?;
            let frame = frame_of(host, id)?;
            let sv = s
                .saved
                .as_ref()
                .and_then(|m| m.get(id).copied())
                .unwrap_or(SavedCanvasPane {
                    cols: grid.cols,
                    rows: grid.rows,
                    x: grid.position.x,
                    y: grid.position.y,
                });
            orig.push(OrigPane {
                id: id.clone(),
                grid,
                frame,
                w0: f64::from(sv.cols) * stride.x + frame.x,
                h0: f64::from(sv.rows) * stride.y + frame.y,
            });
        }

        let mut col_w0 = vec![0.0_f64; cols_count];
        let mut row_h0 = vec![0.0_f64; rows_count];
        for (idx, item) in orig.iter().enumerate() {
            let c = idx % cols_count;
            let r = idx / cols_count;
            col_w0[c] = col_w0[c].max(item.w0);
            row_h0[r] = row_h0[r].max(item.h0);
        }
        let sum_col_w0 = {
            let sum: f64 = col_w0.iter().sum();
            if sum == 0.0 { 1.0 } else { sum }
        };
        let sum_row_h0 = {
            let sum: f64 = row_h0.iter().sum();
            if sum == 0.0 { 1.0 } else { sum }
        };

        let mut zoom = line_px / stride.y;
        let bar_room_init = BAR_ROOM.min(22.0 / zoom);
        let f0 = orig[0].frame;
        let min_total_w = cols_count as f64 * (f64::from(MIN_FULLSCREEN_SIZE.cols) * stride.x + f0.x)
            + (cols_count.saturating_sub(1)) as f64 * gap;
        let min_total_h = rows_count as f64
            * (f64::from(MIN_FULLSCREEN_SIZE.rows) * stride.y + f0.y + bar_room_init)
            + (rows_count.saturating_sub(1)) as f64 * gap;
        let fit_w = (area.width - 2.0 * pad).max(1.0);
        let fit_h = (area.height - 2.0 * pad).max(1.0);
        if fit_w / zoom < min_total_w || fit_h / zoom < min_total_h {
            zoom = 0.05_f64.max((fit_w / min_total_w).min(fit_h / min_total_h));
        }

        let default_bar_h = BAR_ROOM.min(22.0 / zoom);
        let mut max_named_bar = 0.0_f64;
        for id in shown {
            if let Some(bh) = host.bar_height(id, zoom) {
                max_named_bar = max_named_bar.max(bh);
            }
        }
        let bar_h = if max_named_bar > 0.0 {
            max_named_bar
        } else {
            default_bar_h
        };

        let usable_w = (cols_count as f64 * (f64::from(MIN_FULLSCREEN_SIZE.cols) * stride.x + f0.x))
            .max(fit_w / zoom - (cols_count.saturating_sub(1)) as f64 * gap);
        let usable_h = (rows_count as f64 * (f64::from(MIN_FULLSCREEN_SIZE.rows) * stride.y + f0.y))
            .max(
                fit_h / zoom
                    - rows_count as f64 * bar_h
                    - (rows_count.saturating_sub(1)) as f64 * gap,
            );
        let kx = usable_w / sum_col_w0;
        let ky = usable_h / sum_row_h0;

        let saved_first = self
            .state
            .as_ref()
            .and_then(|st| st.saved.as_ref()?.get(&shown[0]).copied());
        let mut first_size: Option<GridSize> = None;
        let mut new_sizes = Vec::with_capacity(orig.len());
        for item in &orig {
            let cols = MIN_FULLSCREEN_SIZE.cols.max(
                (((item.w0 * kx - item.frame.x) / stride.x + EDGE_EPS).floor()).max(0.0) as u32,
            );
            let rows = MIN_FULLSCREEN_SIZE.rows.max(
                (((item.h0 * ky - item.frame.y) / stride.y + EDGE_EPS).floor()).max(0.0) as u32,
            );
            if cols != item.grid.cols || rows != item.grid.rows {
                host.resize_terminal(&item.id, cols, rows);
            }
            first_size.get_or_insert(GridSize::new(cols, rows));
            let r = host.terminal_rect(&item.id)?;
            new_sizes.push(Size::new(r.max_x - r.min_x, r.max_y - r.min_y));
        }

        let origin = group.origin.unwrap_or_else(|| {
            let sx = saved_first.map_or(0.0, |sv| sv.x);
            let sy = saved_first.map_or(0.0, |sv| sv.y) + bar_h;
            Point::new(sx, sy)
        });
        let tiled = tile_layout(
            &new_sizes,
            TileOptions {
                origin,
                columns: cols_count,
                gap,
                bar_room: bar_h,
            },
        )
        .expect("cols_count >= 1 and group gap >= 0");
        for (id, place) in shown.iter().zip(tiled.places) {
            host.place_body(id, place);
        }

        let win_rects: Vec<Rect> = shown
            .iter()
            .filter_map(|id| host.window_rect(id, zoom))
            .collect();
        if let Some(u) = union_rect(&win_rects) {
            center_rect_in_area(host, u, area, zoom);
        }
        first_size
    }

    fn refit_freeform(
        &mut self,
        host: &mut impl FullscreenHost,
        shown: &[String],
        area: FullscreenArea,
    ) -> Option<GridSize> {
        self.fs_split_rect = None;
        self.fs_split_first_member = None;
        let zoom_scale = self
            .state
            .as_ref()
            .and_then(|s| s.zoom_scale)
            .unwrap_or(1.0);
        let pad = host.config().fullscreen_pad;
        let fit_w = (area.width - 2.0 * pad).max(1.0);
        let fit_h = (area.height - 2.0 * pad).max(1.0);

        let win_at = |z: f64| -> Option<Rect> {
            let rects: Vec<Rect> = shown
                .iter()
                .filter_map(|id| host.window_rect(id, z))
                .collect();
            union_rect(&rects)
        };
        let r_initial = win_at(host.viewport_zoom())?;
        let first = fit_rect(r_initial, fit_w, fit_h, 1.0, f64::INFINITY);
        let r_first = win_at(first.zoom)?;
        let second = fit_rect(r_first, fit_w, fit_h, 1.0, f64::INFINITY);
        let zoom = 0.05_f64.max(second.zoom * zoom_scale);
        if let Some(r_final) = win_at(zoom) {
            center_rect_in_area(host, r_final, area, zoom);
        }
        host.terminal(&shown[0])
            .map(|t| GridSize::new(t.cols, t.rows))
    }
}

fn effective_mode(state: &FullscreenState) -> FullscreenMode {
    state.mode.unwrap_or(if state.group_id.is_some() {
        FullscreenMode::Group
    } else {
        FullscreenMode::Single
    })
}

fn saved_pane_of(state: &FullscreenState, id: &str) -> Option<SavedCanvasPane> {
    state
        .saved
        .as_ref()
        .and_then(|m| m.get(id).copied())
        .or_else(|| (state.id.as_deref() == Some(id)).then_some(state.canvas).flatten())
}

fn shown_of(host: &impl FullscreenHost, gid: &str) -> Vec<String> {
    let Some(group) = host.group(gid) else {
        return Vec::new();
    };
    group
        .members
        .iter()
        .filter(|id| host.is_shown(id))
        .cloned()
        .collect()
}

fn frame_of(host: &impl FullscreenHost, id: &str) -> Option<Point> {
    let grid = host.terminal(id)?;
    let rect = host.terminal_rect(id)?;
    Some(Point::new(
        rect.max_x - rect.min_x - f64::from(grid.cols) * grid.cell_stride.x,
        rect.max_y - rect.min_y - f64::from(grid.rows) * grid.cell_stride.y,
    ))
}

fn center_rect_in_area(
    host: &mut impl FullscreenHost,
    rect: Rect,
    area: FullscreenArea,
    zoom: f64,
) {
    let view = host.viewport_size();
    let cx = (rect.min_x + rect.max_x) / 2.0 + (view.w / 2.0 - (area.left + area.width / 2.0)) / zoom;
    let cy = (rect.min_y + rect.max_y) / 2.0 - (view.h / 2.0 - (area.top + area.height / 2.0)) / zoom;
    host.ease_camera(CameraPose { x: cx, y: cy, zoom });
}

fn restore_state(host: &mut impl FullscreenHost, state: &FullscreenState, except_id: Option<&str>) {
    if let Some(ref map) = state.saved {
        for (id, sv) in map {
            if except_id == Some(id.as_str()) {
                continue;
            }
            give_back_one(host, id, *sv);
        }
    } else if let (Some(ref id), Some(sv)) = (&state.id, state.canvas) {
        if except_id != Some(id.as_str()) {
            give_back_one(host, id, sv);
        }
    }
}

fn give_back_one(host: &mut impl FullscreenHost, id: &str, sv: SavedCanvasPane) {
    let Some(grid) = host.terminal(id) else {
        return;
    };
    if grid.cols != sv.cols || grid.rows != sv.rows {
        host.resize_terminal(id, sv.cols, sv.rows);
    }
    host.set_terminal_position(id, Point::new(sv.x, sv.y));
}

fn clamp_line(px: f64) -> f64 {
    px.clamp(MIN_LINE_PX, MAX_LINE_PX)
}

fn js_round_i64(value: f64) -> i64 {
    if !value.is_finite() || value == 0.0 {
        return 0;
    }
    let rounded = if value > 0.0 {
        let f = value.floor();
        if value - f >= 0.5 { f + 1.0 } else { f }
    } else {
        let c = value.ceil();
        if c - value > 0.5 { c - 1.0 } else { c }
    };
    rounded as i64
}

fn to_split_tree(node: &crate::store::SplitNode) -> split::SplitNode {
    match node {
        crate::store::SplitNode::Leaf { id } => split::SplitNode::Leaf { id: id.clone() },
        crate::store::SplitNode::Branch { dir, ratio, a, b } => {
            let split_dir = match dir {
                crate::store::SplitDir::Row => split::SplitDir::Row,
                crate::store::SplitDir::Col => split::SplitDir::Col,
            };
            split::SplitNode::Branch {
                dir: split_dir,
                ratio: *ratio,
                a: Box::new(to_split_tree(a)),
                b: Box::new(to_split_tree(b)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{z_stack_layout, ZStackOptions, FULLSCREEN_Z};
    use crate::store::{SidebarPatch, Storage, WallSplit};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::io;
    use std::rc::Rc;

    const PAD: f64 = 0.3;

    #[derive(Clone, Default)]
    struct MemStorage {
        data: Rc<RefCell<HashMap<String, Vec<u8>>>>,
    }

    impl Storage for MemStorage {
        fn load(&self, scope: &str) -> io::Result<Option<Vec<u8>>> {
            Ok(self.data.borrow().get(scope).cloned())
        }

        fn save(&self, scope: &str, bytes: &[u8]) -> io::Result<()> {
            self.data.borrow_mut().insert(scope.to_owned(), bytes.to_vec());
            Ok(())
        }
    }

    #[derive(Clone, Debug)]
    struct MockGrid {
        cols: u32,
        rows: u32,
        position: Point,
        cell_stride: Point,
        scale_z: f64,
    }

    /// Stand-in wall mirroring `tools/wall-fullscreen.test.mjs`.
    struct StandIn {
        platform: Platform,
        store: WallStore,
        config: WallConfig,
        focused_id: Option<String>,
        focus_history: Vec<String>,
        view: Size,
        viewport_zoom: f64,
        camera: CameraPose,
        terminals: BTreeMap<String, MockGrid>,
        z_order: Vec<String>,
        z_positions: HashMap<String, f64>,
        bar_height: Option<f64>,
        ran: Vec<Vec<String>>,
        eased: Vec<CameraPose>,
        notices: Vec<String>,
    }

    impl StandIn {
        fn new_with_storage(mem: MemStorage) -> Self {
            let mut terminals = BTreeMap::new();
            terminals.insert("a".to_owned(), mock_grid(80, 24, 0.0, 0.0));
            terminals.insert("b".to_owned(), mock_grid(100, 30, 100.0, 0.0));
            Self {
                platform: Platform::Mac,
                store: WallStore::with_storage(Box::new(mem), "relay-8099"),
                config: WallConfig::default(),
                focused_id: Some("a".to_owned()),
                focus_history: vec!["a".to_owned()],
                view: Size::new(1000.0, 600.0),
                viewport_zoom: 15.0,
                camera: CameraPose {
                    x: 5.0,
                    y: -3.0,
                    zoom: 15.0,
                },
                terminals,
                z_order: vec!["a".to_owned(), "b".to_owned()],
                z_positions: HashMap::new(),
                bar_height: None,
                ran: Vec::new(),
                eased: Vec::new(),
                notices: Vec::new(),
            }
        }

        fn new() -> Self {
            Self::new_with_storage(MemStorage::default())
        }

        fn group_screen() -> Self {
            let mem = MemStorage::default();
            let mut s = Self {
                platform: Platform::Mac,
                store: WallStore::with_storage(Box::new(mem), "relay-8097"),
                config: WallConfig::default(),
                focused_id: None,
                focus_history: Vec::new(),
                view: Size::new(1200.0, 800.0),
                viewport_zoom: 6.0,
                camera: CameraPose {
                    x: 40.0,
                    y: -20.0,
                    zoom: 6.0,
                },
                terminals: BTreeMap::new(),
                z_order: Vec::new(),
                z_positions: HashMap::new(),
                bar_height: Some(2.0),
                ran: Vec::new(),
                eased: Vec::new(),
                notices: Vec::new(),
            };
            s.insert_scaled_grid("a", 60, 20, 0.0, 0.0);
            s.insert_scaled_grid("b", 60, 20, 80.0, 0.0);
            s.insert_scaled_grid("loose", 60, 20, 300.0, 0.0);
            s
        }

        fn insert_scaled_grid(&mut self, id: &str, cols: u32, rows: u32, x: f64, y: f64) {
            self.terminals.insert(
                id.to_owned(),
                MockGrid {
                    cols,
                    rows,
                    position: Point::new(x, y),
                    cell_stride: Point::new(1.2, 2.4),
                    scale_z: 2.0,
                },
            );
            self.z_order.push(id.to_owned());
            self.store.set_position(id, Point::new(x, y));
        }

        fn focus_terminal(&mut self, fs: &mut WallFullscreen, id: &str) {
            self.focus(id);
            if fs.active() {
                fs.on_focus(self, id);
            }
        }

        fn layout_split_on_canvas(&mut self, gid: &str) {
            let Some(g) = self.store.group(gid).cloned() else {
                return;
            };
            let Some(sp) = g.split.as_ref() else {
                return;
            };
            let origin = g.origin.unwrap_or(Point::new(0.0, 0.0));
            let sr = Rect {
                min_x: origin.x,
                min_y: origin.y - sp.h,
                max_x: origin.x + sp.w,
                max_y: origin.y,
            };
            let stride = self.terminals[&g.members[0]].cell_stride;
            let room = if sp.bars {
                self.config.split_bar_rows * stride.y
            } else {
                0.0
            };
            let tree = to_split_tree(&sp.tree);
            for (id, pr) in split::panes(&tree, sr, self.config.split_gap) {
                let f = frame_of(self, &id).unwrap();
                let cols = MIN_FULLSCREEN_SIZE.cols.max(
                    (((pr.max_x - pr.min_x - f.x) / stride.x + EDGE_EPS).floor()).max(0.0) as u32,
                );
                let rows = MIN_FULLSCREEN_SIZE.rows.max(
                    (((pr.max_y - pr.min_y - room - f.y) / stride.y + EDGE_EPS).floor()).max(0.0)
                        as u32,
                );
                self.resize_terminal(&id, cols, rows);
                self.place_body(&id, Point::new(pr.min_x, pr.max_y - room));
            }
        }
    }

    fn mock_grid(cols: u32, rows: u32, x: f64, y: f64) -> MockGrid {
        MockGrid {
            cols,
            rows,
            position: Point::new(x, y),
            cell_stride: Point::new(0.6, 1.2),
            scale_z: 1.0,
        }
    }

    impl FullscreenHost for StandIn {
        fn platform(&self) -> Platform {
            self.platform
        }

        fn config(&self) -> FullscreenConfig {
            FullscreenConfig::from(&self.config)
        }

        fn dock_state(&self) -> DockState {
            dock_state_from_store(&self.store)
        }

        fn viewport_size(&self) -> Size {
            self.view
        }

        fn camera_pose(&self) -> CameraPose {
            self.camera
        }

        fn viewport_zoom(&self) -> f64 {
            self.viewport_zoom
        }

        fn ease_camera(&mut self, pose: CameraPose) {
            self.eased.push(pose);
        }

        fn terminal(&self, id: &str) -> Option<FullscreenTerminal> {
            let g = self.terminals.get(id)?;
            Some(FullscreenTerminal {
                cols: g.cols,
                rows: g.rows,
                position: g.position,
                cell_stride: g.cell_stride,
            })
        }

        fn is_shown(&self, id: &str) -> bool {
            self.terminals.contains_key(id) && !self.store.hidden(id)
        }

        fn terminal_rect(&self, id: &str) -> Option<Rect> {
            let g = self.terminals.get(id)?;
            let pad_world = PAD * g.scale_z;
            Some(Rect {
                min_x: g.position.x,
                max_x: g.position.x + f64::from(g.cols) * g.cell_stride.x + 2.0 * pad_world,
                min_y: g.position.y - f64::from(g.rows) * g.cell_stride.y - 2.0 * pad_world,
                max_y: g.position.y,
            })
        }

        fn window_rect(&self, id: &str, _zoom: f64) -> Option<Rect> {
            let mut r = self.terminal_rect(id)?;
            if let Some(bh) = self.bar_height {
                r.max_y += bh;
            }
            Some(r)
        }

        fn bar_height(&self, _id: &str, _zoom: f64) -> Option<f64> {
            self.bar_height
        }

        fn resize_terminal(&mut self, id: &str, cols: u32, rows: u32) {
            self.ran.push(vec![
                "terminal.resize".to_owned(),
                id.to_owned(),
                cols.to_string(),
                rows.to_string(),
            ]);
            if let Some(g) = self.terminals.get_mut(id) {
                g.cols = cols;
                g.rows = rows;
            }
        }

        fn set_terminal_position(&mut self, id: &str, pos: Point) {
            if let Some(g) = self.terminals.get_mut(id) {
                g.position = pos;
            }
        }

        fn focused_id(&self) -> Option<&str> {
            self.focused_id.as_deref()
        }

        fn focus_history(&self) -> &[String] {
            &self.focus_history
        }

        fn focus(&mut self, id: &str) {
            self.focused_id = Some(id.to_owned());
            if self.focus_history.last().map(String::as_str) != Some(id) {
                self.focus_history.push(id.to_owned());
            }
            if let Some(pos) = self.z_order.iter().position(|x| x == id) {
                let item = self.z_order.remove(pos);
                self.z_order.push(item);
            }
        }

        fn terminal_name(&self, id: &str) -> String {
            format!("name-{id}")
        }

        fn notify(&mut self, text: String) {
            self.notices.push(text);
        }

        fn apply_canvas(&mut self, elevated_ids: &[String]) {
            let scale_fn = |id: &str| {
                self.terminals
                    .get(id)
                    .map_or(2.0, |g| g.scale_z)
            };
            let group_fn = |id: &str| self.store.group_of(id).map(str::to_owned);
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
            self.z_positions = stack.terminals;
        }

        fn relayout_group(&mut self, gid: &str) {
            if self
                .store
                .group(gid)
                .is_some_and(|g| g.layout == GroupLayout::Split)
            {
                self.layout_split_on_canvas(gid);
            }
        }

        fn group(&self, gid: &str) -> Option<&WallGroup> {
            self.store.group(gid)
        }

        fn group_of(&self, id: &str) -> Option<&str> {
            self.store.group_of(id)
        }

        fn saved_fullscreen(&self) -> Option<&FullscreenState> {
            self.store.fullscreen()
        }

        fn set_saved_fullscreen(&mut self, state: Option<FullscreenState>) {
            self.store.set_fullscreen(state);
        }

        fn fullscreen_text(&self, key: &str) -> Option<f64> {
            self.store.fullscreen_text(key)
        }

        fn set_fullscreen_text(&mut self, key: &str, px: Option<f64>) {
            self.store.set_fullscreen_text(key, px);
        }
    }

    struct FitExpect {
        cols: u32,
        rows: u32,
        left: f64,
        top: f64,
        zoom: f64,
        frame: f64,
    }

    fn fit_of(line_px: f64, left: f64, width: f64, height: f64, pad: f64) -> FitExpect {
        let zoom = line_px / 1.2;
        let cell_w = 0.6 * zoom;
        let frame = 2.0 * PAD * zoom;
        let cols = ((width - 2.0 * pad - frame) / cell_w).floor() as u32;
        let rows = ((height - 2.0 * pad - frame) / line_px).floor() as u32;
        FitExpect {
            cols,
            rows,
            left: left + (width - f64::from(cols) * cell_w - frame) / 2.0,
            top: (height - f64::from(rows) * line_px - frame) / 2.0,
            zoom,
            frame,
        }
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-6,
            "expected {actual} to be close to {expected}"
        );
    }

    #[test]
    fn entering_keeps_canvas_text_size_and_fills_canvas() {
        let mut wall = StandIn::new();
        let mut fs = WallFullscreen::new();
        assert!(fs.enter(&mut wall, "a"));
        let pad = wall.config.fullscreen_pad;
        let f = fit_of(18.0, 0.0, 1000.0, 600.0, pad);
        assert_eq!(
            wall.ran,
            vec![vec![
                "terminal.resize".to_owned(),
                "a".to_owned(),
                f.cols.to_string(),
                f.rows.to_string(),
            ]]
        );
        assert_eq!(fs.state().unwrap().line_px, 18.0);

        let pose = *wall.eased.last().unwrap();
        assert_close(pose.zoom, f.zoom);
        assert_close((0.0 - pose.x) * pose.zoom + 500.0, f.left);
        assert_close((pose.y - 0.0) * pose.zoom + 300.0, f.top);
        assert_eq!(
            wall.store.fullscreen().unwrap().id.as_deref(),
            Some("a")
        );
        assert_eq!(wall.terminals["a"].cols, f.cols);

        let right = f.left + f64::from(f.cols) * 0.6 * f.zoom + f.frame;
        let bottom = f.top + f64::from(f.rows) * 18.0 + f.frame;
        assert!(right <= 1000.0 - pad);
        assert!(bottom <= 600.0 - pad);
        assert!(f.left >= pad);
    }

    #[test]
    fn from_far_out_entry_text_is_at_least_fullscreen_min_line_px() {
        let mut wall = StandIn::new();
        let mut fs = WallFullscreen::new();
        wall.viewport_zoom = 1.0;
        fs.enter(&mut wall, "a");
        assert_eq!(
            fs.state().unwrap().line_px,
            wall.config.fullscreen_min_line_px
        );
    }

    #[test]
    fn leaving_gives_back_canvas_cols_rows_spot_and_camera_pose() {
        let mut wall = StandIn::new();
        let mut fs = WallFullscreen::new();
        fs.enter(&mut wall, "a");
        wall.terminals.get_mut("a").unwrap().position.x = 7.0;
        fs.exit(&mut wall);
        assert_eq!(
            wall.ran.last().unwrap(),
            &vec![
                "terminal.resize".to_owned(),
                "a".to_owned(),
                "80".to_owned(),
                "24".to_owned(),
            ]
        );
        assert_eq!(wall.terminals["a"].position, Point::new(0.0, 0.0));
        assert_eq!(
            *wall.eased.last().unwrap(),
            CameraPose {
                x: 5.0,
                y: -3.0,
                zoom: 15.0,
            }
        );
        assert_eq!(fs.id(), None);
    }

    #[test]
    fn text_keys_step_size_rewrap_and_are_remembered_until_reset() {
        let mut wall = StandIn::new();
        let mut fs = WallFullscreen::new();
        fs.enter(&mut wall, "a");
        let px = fs.text(&mut wall, TextStep::Bigger).unwrap();
        assert_close(px, 18.0 * wall.config.fullscreen_text_step);
        let f = fit_of(px, 0.0, 1000.0, 600.0, wall.config.fullscreen_pad);
        assert_eq!(
            wall.ran.last().unwrap(),
            &vec![
                "terminal.resize".to_owned(),
                "a".to_owned(),
                f.cols.to_string(),
                f.rows.to_string(),
            ]
        );
        assert_close(wall.store.fullscreen_text("a").unwrap(), px);

        fs.exit(&mut wall);
        wall.viewport_zoom = 3.0;
        fs.enter(&mut wall, "a");
        assert_close(fs.state().unwrap().line_px, px);
        assert_eq!(
            fs.text(&mut wall, TextStep::Reset),
            Some(wall.config.fullscreen_min_line_px)
        );
        assert_eq!(wall.store.fullscreen_text("a"), None);
    }

    #[test]
    fn docked_panels_and_pad_narrow_what_fullscreen_fills() {
        let mut wall = StandIn::new();
        let mut fs = WallFullscreen::new();
        wall.store.set_sidebar(SidebarPatch {
            open: Some(true),
            side: Some(PanelSide::Left),
            ..SidebarPatch::default()
        });
        wall.config.fullscreen_pad = 0.0;
        fs.enter(&mut wall, "a");
        let sb_w = crate::dock::DOCK_WIDTHS.sidebar;
        let f = fit_of(18.0, sb_w, 1000.0 - sb_w, 600.0, 0.0);
        assert_eq!(
            wall.ran.last().unwrap(),
            &vec![
                "terminal.resize".to_owned(),
                "a".to_owned(),
                f.cols.to_string(),
                f.rows.to_string(),
            ]
        );
    }

    #[test]
    fn focusing_another_moves_fullscreen_to_it_and_keeps_canvas_pose() {
        let mut wall = StandIn::new();
        let mut fs = WallFullscreen::new();
        fs.enter(&mut wall, "a");
        wall.camera.x = 99.0;
        fs.enter(&mut wall, "b");
        assert!(wall.ran.contains(&vec![
            "terminal.resize".to_owned(),
            "a".to_owned(),
            "80".to_owned(),
            "24".to_owned(),
        ]));
        assert_eq!(fs.id(), Some("b"));
        assert_eq!(wall.focused_id.as_deref(), Some("b"));

        fs.exit(&mut wall);
        assert_eq!(
            *wall.eased.last().unwrap(),
            CameraPose {
                x: 5.0,
                y: -3.0,
                zoom: 15.0,
            }
        );
        assert_eq!(
            wall.ran.last().unwrap(),
            &vec![
                "terminal.resize".to_owned(),
                "b".to_owned(),
                "100".to_owned(),
                "30".to_owned(),
            ]
        );
    }

    #[test]
    fn window_change_refits_once_it_has_settled_not_before() {
        let mut wall = StandIn::new();
        let mut fs = WallFullscreen::new();
        fs.enter(&mut wall, "a");
        let before_len = wall.ran.len();
        wall.view.w = 1400.0;
        fs.tick(&mut wall, 1000.0);
        fs.tick(&mut wall, 1100.0);
        assert!(fs.settling());
        assert_eq!(wall.ran.len(), before_len);
        fs.tick(&mut wall, 1200.0);
        assert!(!fs.settling());
        let f = fit_of(18.0, 0.0, 1400.0, 600.0, wall.config.fullscreen_pad);
        assert_eq!(
            wall.ran.last().unwrap(),
            &vec![
                "terminal.resize".to_owned(),
                "a".to_owned(),
                f.cols.to_string(),
                f.rows.to_string(),
            ]
        );
    }

    #[test]
    fn reload_adopts_saved_fullscreen_and_gone_terminal_ends_it() {
        let mem = MemStorage::default();
        let mut first_wall = StandIn::new_with_storage(mem.clone());
        let mut first_fs = WallFullscreen::new();
        first_fs.enter(&mut first_wall, "a");

        let mut next_wall = StandIn::new_with_storage(mem);
        let mut next_fs = WallFullscreen::new();
        assert_eq!(next_fs.id(), None);
        next_fs.adopt(&mut next_wall, "b");
        assert_eq!(next_fs.id(), None);
        assert!(next_wall.notices.is_empty());

        next_fs.adopt(&mut next_wall, "a");
        assert_eq!(next_fs.id(), Some("a"));
        assert_eq!(
            next_fs.state().unwrap().canvas,
            Some(SavedCanvasPane {
                cols: 80,
                rows: 24,
                x: 0.0,
                y: 0.0,
            })
        );
        assert_eq!(next_wall.notices.len(), 1);
        assert_eq!(
            next_wall.notices[0],
            "fullscreen: name-a — ⌘↵ shows the whole wall again"
        );
        assert!(first_wall.notices.is_empty());

        next_fs.gone(&mut next_wall, "a");
        assert_eq!(next_fs.id(), None);
        assert_eq!(next_wall.store.fullscreen(), None);
        assert_eq!(
            *next_wall.eased.last().unwrap(),
            CameraPose {
                x: 5.0,
                y: -3.0,
                zoom: 15.0,
            }
        );
    }

    #[test]
    fn canvas_body_rect_preserves_pre_fullscreen_canvas_rect() {
        let mut wall = StandIn::new();
        let mut fs = WallFullscreen::new();
        let before = wall.terminal_rect("a").unwrap();
        fs.enter(&mut wall, "a");
        assert!(wall.terminal_rect("a").unwrap().max_x > before.max_x);
        let cb = fs.canvas_body_rect(&wall, None).unwrap();
        assert!(
            (cb.min_x - before.min_x).abs() < 1e-9
                && (cb.max_x - before.max_x).abs() < 1e-9
                && (cb.min_y - before.min_y).abs() < 1e-9
                && (cb.max_y - before.max_y).abs() < 1e-9
        );
    }

    #[test]
    fn group_fullscreen_split_tile_freeform_and_single_pane_zoom_unzoom() {
        let mut wall = StandIn::group_screen();
        let mut fs = WallFullscreen::new();

        // 1. Split Group Fullscreen + ⌘⇧↵ single-pane zoom/unzoom + live divider ratio update.
        let g_split = "g-1";
        let r_a = wall.window_rect("a", wall.viewport_zoom).unwrap();
        let r_b = wall.window_rect("b", wall.viewport_zoom).unwrap();
        let split_union = union_rect(&[r_a, r_b]).unwrap();
        wall.store.set_group(
            g_split,
            WallGroup {
                name: String::new(),
                layout: GroupLayout::Split,
                columns: 2,
                gap: 2.0,
                origin: Some(Point::new(split_union.min_x, split_union.max_y)),
                members: vec!["a".to_owned(), "b".to_owned()],
                split: Some(WallSplit {
                    w: split_union.width(),
                    h: split_union.height(),
                    bars: true,
                    tree: crate::store::SplitNode::Branch {
                        dir: crate::store::SplitDir::Row,
                        ratio: 0.5,
                        a: Box::new(crate::store::SplitNode::Leaf {
                            id: "a".to_owned(),
                        }),
                        b: Box::new(crate::store::SplitNode::Leaf {
                            id: "b".to_owned(),
                        }),
                    },
                }),
            },
        );
        wall.layout_split_on_canvas(g_split);
        let a_canvas_cols = wall.terminals["a"].cols;
        let b_canvas_cols = wall.terminals["b"].cols;
        wall.focus_terminal(&mut fs, "a");

        // ⌘↵ (`toggle`) fullscreens the whole split group because 'a' is in a 2-pane group.
        fs.toggle(&mut wall, None);
        assert_eq!(fs.mode(), Some(FullscreenMode::Group));
        assert_eq!(fs.group_id(), Some(g_split));
        assert_eq!(fs.target(), Some(g_split));
        assert!(wall.z_positions["a"] >= FULLSCREEN_Z);
        assert!(wall.z_positions["b"] >= FULLSCREEN_Z);
        assert!(wall.z_positions["loose"] < FULLSCREEN_Z);
        assert!(wall.terminals["a"].cols > a_canvas_cols);
        assert!(wall.terminals["b"].cols > b_canvas_cols);

        // Switching focus between 'a' and 'b' inside the group stays in Group Fullscreen.
        wall.focus_terminal(&mut fs, "b");
        assert_eq!(fs.mode(), Some(FullscreenMode::Group));
        assert_eq!(fs.group_id(), Some(g_split));
        wall.focus_terminal(&mut fs, "a");
        assert_eq!(fs.group_id(), Some(g_split));

        // Dragging the split divider to 0.35 inside Group Fullscreen re-wraps both panes live.
        let fs_split_rect = fs
            .split_rect_for_group(wall.store.group(g_split).unwrap())
            .expect("active fullscreen split rect");
        assert!(fs_split_rect.width() > 0.0);
        let mut updated = wall.store.group(g_split).unwrap().clone();
        if let Some(ref mut sp) = updated.split {
            if let crate::store::SplitNode::Branch { ref mut ratio, .. } = sp.tree {
                *ratio = 0.35;
            }
        }
        wall.store.set_group(g_split, updated);
        fs.refit(&mut wall);
        assert!(wall.terminals["a"].cols < wall.terminals["b"].cols);

        // ⌘⇧↵ (`toggle_pane`) zooms 'a' solo while remembering `return_group = g_split`.
        assert!(fs.toggle_pane(&mut wall, None));
        assert_eq!(fs.mode(), Some(FullscreenMode::Single));
        assert_eq!(fs.id(), Some("a"));
        assert_eq!(
            fs.state().unwrap().return_group.as_deref(),
            Some(g_split)
        );
        assert_eq!(wall.z_positions["a"], FULLSCREEN_Z);
        assert!(wall.z_positions["b"] < FULLSCREEN_Z);
        assert!(wall.terminals["a"].cols > wall.terminals["b"].cols * 2);

        // ⌘⇧↵ again unzooms back into Group Fullscreen on `g_split`.
        assert!(fs.toggle_pane(&mut wall, None));
        assert_eq!(fs.mode(), Some(FullscreenMode::Group));
        assert_eq!(fs.group_id(), Some(g_split));
        assert!(wall.z_positions["b"] >= FULLSCREEN_Z);

        // Focusing outside `g_split` onto 'loose' transitions solo, then focusing 'a' transitions back to `g_split`.
        wall.focus_terminal(&mut fs, "loose");
        assert_eq!(fs.mode(), Some(FullscreenMode::Single));
        assert_eq!(fs.id(), Some("loose"));
        wall.focus_terminal(&mut fs, "a");
        assert_eq!(fs.mode(), Some(FullscreenMode::Group));
        assert_eq!(fs.group_id(), Some(g_split));

        // ⌘↵ (`toggle`) exits all the way back to the canvas, keeping the updated 0.35 split ratio.
        fs.toggle(&mut wall, None);
        assert!(!fs.active());
        assert!(wall.terminals["a"].cols < wall.terminals["b"].cols);

        // 2. Tile Group Fullscreen preserves custom 2 : 3 : 2 column proportions while showing more text.
        wall.insert_scaled_grid("t1", 40, 20, 0.0, -120.0);
        wall.insert_scaled_grid("t2", 60, 20, 60.0, -120.0);
        wall.insert_scaled_grid("t3", 40, 20, 140.0, -120.0);
        let g_tile = "g-2";
        wall.store.set_group(
            g_tile,
            WallGroup {
                name: String::new(),
                layout: GroupLayout::Tile,
                columns: 3,
                gap: 2.0,
                origin: Some(Point::new(0.0, -120.0 + BAR_ROOM)),
                members: vec!["t1".to_owned(), "t2".to_owned(), "t3".to_owned()],
                split: None,
            },
        );
        wall.focus_terminal(&mut fs, "t2");
        fs.toggle(&mut wall, None);
        assert_eq!(fs.group_id(), Some(g_tile));
        assert!(wall.terminals["t1"].cols > 40);
        assert!(wall.terminals["t1"].rows > 20);
        assert_eq!(wall.terminals["t1"].cols, wall.terminals["t3"].cols);
        let ratio = f64::from(wall.terminals["t2"].cols) / f64::from(wall.terminals["t1"].cols);
        assert!(ratio > 1.4 && ratio < 1.6);
        fs.toggle(&mut wall, None);
        assert_eq!(
            [
                wall.terminals["t1"].cols,
                wall.terminals["t2"].cols,
                wall.terminals["t3"].cols
            ],
            [40, 60, 40]
        );

        // 3. Freeform Group Fullscreen leaves sizes & spots untouched so panes move freely and keep their new spots.
        wall.insert_scaled_grid("f1", 50, 16, 10.0, -260.0);
        wall.insert_scaled_grid("f2", 50, 16, 40.0, -270.0);
        let g_free = "g-3";
        wall.store.set_group(
            g_free,
            WallGroup {
                name: String::new(),
                layout: GroupLayout::Freeform,
                columns: 2,
                gap: 2.0,
                origin: None,
                members: vec!["f1".to_owned(), "f2".to_owned()],
                split: None,
            },
        );
        wall.focus_terminal(&mut fs, "f1");
        fs.toggle(&mut wall, None);
        assert_eq!(fs.group_id(), Some(g_free));
        assert_eq!(wall.terminals["f1"].cols, 50);
        assert_eq!(wall.terminals["f2"].cols, 50);
        assert!(wall.z_positions["f2"] >= FULLSCREEN_Z);
        assert!(wall.z_positions["f1"] - 1.0 > wall.z_positions["f2"] + 0.1);

        wall.place_body("f2", Point::new(180.0, -260.0));
        let moved_x = wall.terminals["f2"].position.x;
        fs.toggle(&mut wall, None);
        assert_eq!(wall.terminals["f2"].position.x, moved_x);
    }

    #[test]
    fn group_fullscreen_hide_recovery_and_reload_adoption() {
        let mem = MemStorage::default();
        let mut wall = StandIn::new_with_storage(mem.clone());
        wall.store.set_group(
            "g-1",
            WallGroup {
                name: "ops".to_owned(),
                layout: GroupLayout::Tile,
                columns: 2,
                gap: 2.0,
                origin: Some(Point::new(0.0, BAR_ROOM)),
                members: vec!["a".to_owned(), "b".to_owned()],
                split: None,
            },
        );
        let mut fs = WallFullscreen::new();
        assert!(fs.enter_group(&mut wall, "g-1"));
        assert!(fs.has(&wall, "a") && fs.has(&wall, "b"));

        // Reload adopts group fullscreen on the first member and emits one notice; second member re-fits silently.
        let mut reloaded_wall = StandIn::new_with_storage(mem);
        let mut reloaded_fs = WallFullscreen::new();
        reloaded_fs.adopt(&mut reloaded_wall, "a");
        assert_eq!(reloaded_fs.group_id(), Some("g-1"));
        assert_eq!(
            reloaded_wall.notices,
            vec!["fullscreen: ops — ⌘↵ shows the whole wall again"]
        );
        reloaded_fs.adopt(&mut reloaded_wall, "b");
        assert_eq!(reloaded_wall.notices.len(), 1);

        // Hiding 'a' restores 'a' to 80×24 while keeping 'b' in group fullscreen; hiding 'b' exits.
        reloaded_fs.on_hide(&mut reloaded_wall, "a");
        reloaded_wall.store.set_hidden("a", true);
        reloaded_fs.after_member_left(&mut reloaded_wall);
        assert_eq!(
            (reloaded_wall.terminals["a"].cols, reloaded_wall.terminals["a"].rows),
            (80, 24)
        );
        assert!(reloaded_fs.active());
        assert_eq!(reloaded_fs.elevated_ids(&reloaded_wall), vec!["b"]);

        reloaded_fs.on_hide(&mut reloaded_wall, "b");
        reloaded_wall.store.set_hidden("b", true);
        reloaded_fs.after_member_left(&mut reloaded_wall);
        assert!(!reloaded_fs.active());
    }
}
