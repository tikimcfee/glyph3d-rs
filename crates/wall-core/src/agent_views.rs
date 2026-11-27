//! Stateful coordinator for per-terminal Agent Views on the 2D wall, ported from
//! `wall/WallAgentViews.js`, together with the stream reader it embeds from
//! `packages/glyph3d-core/src/collections/agentEvents.js` (`AgentEventReader`) and
//! the canvas footprint shove / give-room-back coordination in `wall/TerminalWall.js`
//! (`footprintChanged`, `_standOf`, `_giveRoomBack`).
//!
//! ### Responsibilities
//! - **Open / side / session / filter state** per terminal, hydrated from and
//!   persisted through [`WallStore::agent_views`](crate::store::WallStore::agent_views).
//! - **Terminal-to-session pairing**: either automatic (`"auto"`, delegated to
//!   [`crate::agent_view::resolve_terminal_session`]) or pinned to an explicit
//!   chosen `session_id` ([`SessionBinding::Pinned`]).
//! - **Per-session event stream state**: 256 KiB tail hydration
//!   ([`STREAM_WINDOW_BYTES`]), `add`/`update` in-place event folding, uncommitted
//!   provisional Stop-hook reply pruning before resuming live follow at `last_at`,
//!   backward `load_earlier` pagination with orphan `update` adoption, category
//!   filtering, single/bulk row & full-body expansion, and tail `stick_bottom` /
//!   `unread_count` tracking.
//! - **Canvas footprint & give-room-back memory**: coordinates with
//!   [`crate::groups::Groups`] (via [`AgentViews::tile_metrics_for_body`] and
//!   [`AgentViews::pane_inset_with_dims`]) so tiled and split groups absorb open
//!   Agent Views, and uses [`crate::layout::shove_aside`] to push covered canvas
//!   terminals and groups clear when a view opens or widens a group — restoring
//!   unmoved items when the view moves or closes.
//! - **Write-time scrollback line anchors & terminal jumps**: stamps primary-screen
//!   `(scrollback_rows, cursor_row)` coordinates as live `add` frames arrive, honors
//!   protocol `terminal_line` overrides and byte-interpolated approximate anchors for
//!   hydrated history, refuses jumps while on the alternate screen (emitting a wall
//!   notice), and emits scroll effects (`live` or `to <offset>`).
//! - **Per-frame screen placement**: projects open Agent Views through
//!   [`WallCamera`], snapping outer bounds to whole screen pixels, computing inner
//!   scale and `--av-hairline = ceil(1 / scale).max(1)` so internal 1 px dividers
//!   never vanish when zoomed out, switching to badge mode below
//!   [`BADGE_FLOOR_PX`], culling off-screen cards, and hiding all cards in fullscreen.
//!
//! ### Host & feed seams
//! No WebSocket RPC, DOM, or React lives here:
//! - Events and session lists enter as plain data ([`AgentSession`],
//!   [`SessionReadPage`], [`AgentStreamFrame`], [`TermdTerminalStatus`], or raw
//!   `EVENTS.md` JSONL lines).
//! - Read-only wall queries enter through [`AgentViewsHost`] (and
//!   [`FootprintCanvas`] when mutating canvas positions/groups on footprint changes).
//! - Outbound requests to `crates/agent-feed`, `termd`, and the wall chrome leave
//!   through an internal outbox of [`AgentViewsEffect`] values drained via
//!   [`AgentViews::drain_effects`].

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::agent_view::{
    agent_inset, agent_view_rect, apply_filter_change, categories_of_record, compute_jump_offset,
    default_filters, describe_active_filter, event_record, expand_category_tokens,
    extract_protocol_terminal_line, footprint_tile_metrics, format_expanded_event,
    format_session_label, matches_filter, matches_subset, normalize_filters,
    resolve_terminal_session, summarize_event_row, AgentInset, AgentSession, AgentViewDims,
    EventRecord, EventRowSummary, ExpandOptions, ExpandedEvent, FilterMap, FilterMode,
    FootprintTileMetrics, LineAnchor, ResolveVia, TerminalSessionMeta, CATEGORY_IDS,
    DEFAULT_JUMP_CONTEXT_ROWS,
};
use crate::camera::{CameraPose, WallCamera};
use crate::config::AgentSide;
use crate::geom::{Point, Rect};
use crate::groups::{GroupLayout, PaneInset, TileMetrics};
use crate::layout::{overlaps, shove_aside, union_rect, ShoveDir, ShoveItem, BAR_ROOM};
use crate::snap::{snap_step, GridSpec};
use crate::store::{AgentViewState, WallStore};

/// Tail byte window requested on initial stream open and each `load_earlier` page
/// (`WallAgentViews.STREAM_WINDOW_BYTES`: `256 KiB`).
pub const STREAM_WINDOW_BYTES: u64 = 256 * 1024;

/// Base zoom (screen pixels per world unit) at which an Agent View lays out its
/// internal logical box (`WallAgentViews.AGENT_BASE_ZOOM`).
pub const AGENT_BASE_ZOOM: f64 = 4.0;

/// Minimum rendered font size (screen pixels) before an Agent View collapses to
/// its compact far-zoom badge (`WallAgentViews.BADGE_FLOOR_PX`).
pub const BADGE_FLOOR_PX: f64 = 7.2;

/// Base font size in screen pixels at [`AGENT_BASE_ZOOM`] (`WallAgentViews.BASE_FONT_PX`).
pub const BASE_FONT_PX: f64 = 11.0;

const POS_EPS: f64 = 1e-6;

fn js_round(v: f64) -> f64 {
    if !v.is_finite() || v == 0.0 {
        return 0.0;
    }
    let r = (v + 0.5).floor();
    if r == 0.0 {
        0.0
    } else {
        r
    }
}

fn parse_agent_side(raw: &str) -> Result<AgentSide, String> {
    match raw {
        "right" => Ok(AgentSide::Right),
        "left" => Ok(AgentSide::Left),
        "top" => Ok(AgentSide::Top),
        "bottom" => Ok(AgentSide::Bottom),
        other => Err(format!("side is right|left|top|bottom, not '{other}'")),
    }
}

const fn side_to_shove_dir(side: AgentSide) -> ShoveDir {
    match side {
        AgentSide::Right => ShoveDir::Right,
        AgentSide::Left => ShoveDir::Left,
        AgentSide::Top => ShoveDir::Top,
        AgentSide::Bottom => ShoveDir::Bottom,
    }
}

// ── Plain-data feed & terminal-sync input types ──────────────────────────────

/// Operation carried by an `EVENTS.md` event frame (`"add"` or `"update"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentEventOp {
    Add,
    Update,
}

/// Transcript byte/line/sequence position of an event's introducing `add` frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventPosition {
    pub at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

/// One validated `{"type":"event", ...}` frame from `agent transcript <session>` /
/// `crates/agent-feed`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentEventFrame {
    pub id: String,
    pub op: AgentEventOp,
    pub at: i64,
    pub line: Option<u64>,
    pub seq: Option<u64>,
    /// Normalized event record (`agentEvents.eventRecord` with `showNoise: true`).
    pub record: EventRecord,
    /// Explicit primary-screen line supplied on the event, turn, or body (`terminal_line`).
    pub protocol_terminal_line: Option<u64>,
    /// Whether this event is a `tool` call whose raw `tool.action` is `"noise"`.
    pub is_noise_tool: bool,
}

impl AgentEventFrame {
    /// Whether this frame is an uncommitted provisional Stop-hook reply
    /// (`kind == "say"` and `say.provisional == true`).
    pub fn is_provisional(&self) -> bool {
        self.record.event_kind == "say" && self.record.provisional
    }

    /// Return a copy of `self.record` stamped with `pos.at` (when present) and
    /// filtered by `show_noise` (`None` when `is_noise_tool && !show_noise`).
    pub fn record_at(&self, pos: Option<EventPosition>, show_noise: bool) -> Option<EventRecord> {
        if self.is_noise_tool && !show_noise {
            return None;
        }
        let mut rec = self.record.clone();
        if let Some(p) = pos {
            rec.at = p.at;
        }
        Some(rec)
    }
}

/// One plain-data frame from an agent session's event stream (`EVENTS.md`).
///
/// `crates/agent-feed` may construct [`AgentStreamFrame`] values directly or pass
/// raw JSONL lines through [`AgentStreamFrame::from_json_line`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AgentStreamFrame {
    /// Session header/update frame (`{"type":"session", ...}`).
    Session(AgentSession),
    /// Event `add` or `update` frame (`{"type":"event", ...}`).
    Event(AgentEventFrame),
}

impl AgentStreamFrame {
    /// Validate and convert one parsed `EVENTS.md` JSON object (`checkFrame` in
    /// `collections/agentEvents.js`).
    pub fn from_json_value(value: &serde_json::Value, where_ctx: &str) -> Result<Self, String> {
        let obj = value
            .as_object()
            .ok_or_else(|| format!("[agentEvents] {where_ctx}: not a JSON object"))?;
        let frame_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if frame_type == "session" {
            let mut copy = obj.clone();
            copy.remove("type");
            let session: AgentSession = serde_json::from_value(serde_json::Value::Object(copy))
                .map_err(|e| format!("[agentEvents] {where_ctx}: invalid session frame ({e})"))?;
            return Ok(AgentStreamFrame::Session(session));
        }
        if frame_type != "event" {
            return Err(format!(
                "[agentEvents] {where_ctx}: unknown frame type {:?}",
                obj.get("type")
            ));
        }
        let id = obj
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("[agentEvents] {where_ctx}: event without an id"))?
            .to_string();
        let op = match obj.get("op").and_then(|v| v.as_str()) {
            Some("add") => AgentEventOp::Add,
            Some("update") => AgentEventOp::Update,
            other => {
                return Err(format!(
                    "[agentEvents] {where_ctx}: event {id} has op {other:?}"
                ));
            }
        };
        let kind = obj.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        if !matches!(
            kind,
            "prompt" | "think" | "say" | "tool" | "system" | "request"
        ) {
            return Err(format!(
                "[agentEvents] {where_ctx}: event {id} has kind {kind:?}"
            ));
        }
        let Some(body) = obj.get(kind).and_then(|v| v.as_object()) else {
            return Err(format!(
                "[agentEvents] {where_ctx}: event {id} ({kind}) has no {kind} body"
            ));
        };
        let at_val = obj.get("at");
        let at = at_val
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().filter(|n| n.is_finite()).map(|n| n as i64)))
            .ok_or_else(|| format!("[agentEvents] {where_ctx}: event {id} has no byte offset (at)"))?;
        let line = obj.get("line").and_then(|v| v.as_u64());
        let seq = obj.get("seq").and_then(|v| v.as_u64());
        let is_noise_tool =
            kind == "tool" && body.get("action").and_then(|v| v.as_str()) == Some("noise");
        let record = event_record(value, Some(at), true)?
            .expect("event_record with show_noise=true always yields Some");
        let protocol_terminal_line = extract_protocol_terminal_line(value);

        Ok(AgentStreamFrame::Event(AgentEventFrame {
            id,
            op,
            at,
            line,
            seq,
            record,
            protocol_terminal_line,
            is_noise_tool,
        }))
    }

    /// Parse a single JSONL line into an [`AgentStreamFrame`] (`Ok(None)` for blank lines).
    pub fn from_json_line(line: &str, where_ctx: &str) -> Result<Option<Self>, String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        let val: serde_json::Value = serde_json::from_str(trimmed).map_err(|e| {
            let preview: String = trimmed.chars().take(200).collect();
            format!("[agentEvents] {where_ctx}: not JSON ({e}): {preview}")
        })?;
        Self::from_json_value(&val, where_ctx).map(Some)
    }
}

/// Change emitted by [`AgentEventReader`] for one accepted stream frame.
#[derive(Clone, Debug, PartialEq)]
pub enum EventChange {
    Session(AgentSession),
    Add {
        frame: AgentEventFrame,
        pos: EventPosition,
    },
    Update {
        frame: AgentEventFrame,
        /// Position of the event's original `add`, or `None` when the `add` predates this reader.
        pos: Option<EventPosition>,
    },
}

/// Retain-or-framing stream reader ported from `collections/agentEvents.js`
/// (`AgentEventReader`).
///
/// With `retain: true`, folds `add`/`update` frames by `id` in arrival order.
/// With `retain: false`, validates frames and tracks `last_at` without storing
/// events twice during a live follow.
#[derive(Clone, Debug)]
pub struct AgentEventReader {
    pub retain: bool,
    /// Byte offset `at` of the newest non-provisional event read (`agentEvents/follow { from }`).
    pub last_at: Option<i64>,
    /// Latest `{"type":"session", ...}` metadata seen on this stream.
    pub session: Option<AgentSession>,
    pub by_id: HashMap<String, (AgentEventFrame, EventPosition)>,
    pub order: Vec<String>,
    tail: String,
    line_no: u64,
}

impl AgentEventReader {
    pub fn new(retain: bool) -> Self {
        AgentEventReader {
            retain,
            last_at: None,
            session: None,
            by_id: HashMap::new(),
            order: Vec::new(),
            tail: String::new(),
            line_no: 0,
        }
    }

    /// Feed a text chunk of JSONL frames; complete lines are parsed immediately and
    /// any trailing partial line is held until the next `push` or [`Self::end`].
    pub fn push(&mut self, chunk: &str) -> Result<Vec<EventChange>, String> {
        self.tail.push_str(chunk);
        let Some(cut) = self.tail.rfind('\n') else {
            return Ok(Vec::new());
        };
        let complete = self.tail[..cut].to_string();
        let rest = self.tail[cut + 1..].to_string();
        self.tail = rest;
        self.read_lines(&complete)
    }

    /// Flush any final unterminated line at EOF.
    pub fn end(&mut self) -> Result<Vec<EventChange>, String> {
        let rest = std::mem::take(&mut self.tail);
        if rest.trim().is_empty() {
            Ok(Vec::new())
        } else {
            self.read_lines(&rest)
        }
    }

    fn read_lines(&mut self, text: &str) -> Result<Vec<EventChange>, String> {
        let mut out = Vec::new();
        for line in text.split('\n') {
            let n = self.line_no;
            self.line_no += 1;
            let ctx = format!("stream line {n}");
            if let Some(frame) = AgentStreamFrame::from_json_line(line, &ctx)? {
                out.push(self.accept_frame(frame));
            }
        }
        Ok(out)
    }

    /// Accept one validated [`AgentStreamFrame`] and fold it into this reader.
    pub fn accept_frame(&mut self, frame: AgentStreamFrame) -> EventChange {
        match frame {
            AgentStreamFrame::Session(session) => {
                self.session = Some(session.clone());
                EventChange::Session(session)
            }
            AgentStreamFrame::Event(ev) => {
                if !ev.is_provisional() && self.last_at.is_none_or(|prev| ev.at > prev) {
                    self.last_at = Some(ev.at);
                }
                if !self.retain {
                    return match ev.op {
                        AgentEventOp::Add => {
                            let pos = EventPosition {
                                at: ev.at,
                                line: ev.line,
                                seq: ev.seq,
                            };
                            EventChange::Add { frame: ev, pos }
                        }
                        AgentEventOp::Update => EventChange::Update {
                            frame: ev,
                            pos: None,
                        },
                    };
                }
                if let Some(existing) = self.by_id.get_mut(&ev.id) {
                    let pos = existing.1;
                    existing.0 = ev.clone();
                    return EventChange::Update {
                        frame: ev,
                        pos: Some(pos),
                    };
                }
                if ev.op == AgentEventOp::Update {
                    return EventChange::Update {
                        frame: ev,
                        pos: None,
                    };
                }
                let pos = EventPosition {
                    at: ev.at,
                    line: ev.line,
                    seq: ev.seq,
                };
                self.order.push(ev.id.clone());
                self.by_id.insert(ev.id.clone(), (ev.clone(), pos));
                EventChange::Add { frame: ev, pos }
            }
        }
    }
}

/// Plain-data page of transcript events returned by a window read (`agentEvents/read`).
///
/// - When `until` is `None`, this page hydrates the initial tail window of the session.
/// - When `until` is `Some(offset)`, this page answers an older `load_earlier` window
///   preceding byte `offset`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionReadPage {
    pub frames: Vec<AgentStreamFrame>,
    pub transcript_size: u64,
    pub until: Option<i64>,
}

impl SessionReadPage {
    /// Parse a slice of JSONL lines (`agent transcript <session>`) into a [`SessionReadPage`].
    pub fn from_jsonl_lines<S: AsRef<str>>(
        lines: &[S],
        transcript_size: u64,
        until: Option<i64>,
    ) -> Result<Self, String> {
        let mut frames = Vec::with_capacity(lines.len());
        for (idx, line) in lines.iter().enumerate() {
            let ctx = format!("stream line {idx}");
            if let Some(frame) = AgentStreamFrame::from_json_line(line.as_ref(), &ctx)? {
                frames.push(frame);
            }
        }
        Ok(SessionReadPage {
            frames,
            transcript_size,
            until,
        })
    }
}

/// Per-terminal daemon metadata synced from `termd/status` (`WallAgentViews._onTermdStatus`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TermdTerminalStatus {
    pub id: String,
    pub scrollback_rows: u64,
    /// Maximum `view` scrollback offset across `termd` subscribers (`0` = live).
    pub view: u64,
    pub alt: bool,
    pub pwd: String,
    pub cwd: String,
    pub title: String,
}

// ── Session binding, host traits & outbound effects ──────────────────────────

/// How a terminal's Agent View pairs with an agent session (`WallStore.AgentViewState.session`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SessionBinding {
    /// Resolve automatically via [`resolve_terminal_session`] (`"auto"`).
    #[default]
    Auto,
    /// Pin to a specific chosen `session_id`.
    Pinned(String),
}

impl SessionBinding {
    /// Parse `"auto"` (or empty string) into [`Self::Auto`], and any other session id
    /// into [`Self::Pinned`].
    pub fn from_store_str(raw: &str) -> Self {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "auto" {
            SessionBinding::Auto
        } else {
            SessionBinding::Pinned(trimmed.to_string())
        }
    }

    /// String persisted in [`AgentViewState::session`].
    pub fn as_store_str(&self) -> String {
        match self {
            SessionBinding::Auto => "auto".to_string(),
            SessionBinding::Pinned(sid) => sid.clone(),
        }
    }
}

/// Session hint reported for an attached gomuxai agent on the wall bus (`wall.bus.find`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BusAgentHint {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
}

/// Wall configuration subset read by [`AgentViews`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AgentViewsConfig {
    pub default_side: AgentSide,
    pub dims: AgentViewDims,
    pub group_gap: f64,
    pub snap: bool,
    pub grid: GridSpec,
}

impl Default for AgentViewsConfig {
    fn default() -> Self {
        AgentViewsConfig {
            default_side: AgentSide::Right,
            dims: AgentViewDims::new(56.0, 34.0, 1.5),
            group_gap: 2.0,
            snap: true,
            grid: GridSpec::new(4.0, 5, 10.0),
        }
    }
}

/// Live terminal snapshot read by [`AgentViewsHost::terminal`].
#[derive(Clone, Debug, PartialEq)]
pub struct AgentTerminalSnapshot {
    /// Pre-fullscreen canvas body rectangle (`canvasTerminalRect` or `terminalRect`).
    pub body_rect: Rect,
    /// Body top-left position (`grid.position`).
    pub position: Point,
    /// Cumulative lines scrolled off the primary screen (`grid.scrolledRows`).
    pub scrolled_rows: u64,
    /// Zero-based cursor row inside the visible viewport (`grid.cursorRow`).
    pub cursor_row: u32,
    /// Whether the emulator is currently on the alternate screen (`grid.altScreen`).
    pub alt_screen: bool,
    /// Emulator window title (`grid.windowTitle`).
    pub window_title: String,
    /// Whether this terminal is currently drawn on the canvas (`wall.onCanvas(id)`).
    pub on_canvas: bool,
}

/// Read-only wall queries consumed by [`AgentViews`].
pub trait AgentViewsHost {
    /// Active Agent View & snap configuration knobs.
    fn agent_views_config(&self) -> AgentViewsConfig {
        AgentViewsConfig::default()
    }

    /// Snapshot of live terminal `id`, or `None` if no such terminal is mounted.
    fn terminal(&self, id: &str) -> Option<AgentTerminalSnapshot>;

    /// World height of terminal `id`'s title bar at `zoom` (`wall.names.barHeight(id, zoom)`).
    fn bar_height(&self, _id: &str, _zoom: f64) -> f64 {
        BAR_ROOM
    }

    /// Display name of terminal `id` (`wall.nameOf(id)`).
    fn terminal_name(&self, id: &str) -> String {
        self.store_name(id).unwrap_or_else(|| id.to_string())
    }

    /// User-assigned custom name in `WallStore`, if any (`wall.store.name(id)`).
    fn store_name(&self, _id: &str) -> Option<String> {
        None
    }

    /// Attached gomuxai agent target (`name` or `name@host`) from `WallStore`, if any.
    fn attached_target(&self, _id: &str) -> Option<String> {
        None
    }

    /// Session/cwd hint from `WallBus` for an attached gomuxai agent target.
    fn bus_agent(&self, _target: &str) -> Option<BusAgentHint> {
        None
    }

    /// Currently focused terminal id (`wall.focusedId`).
    fn focused_id(&self) -> Option<&str> {
        None
    }

    /// Whether single-pane or group fullscreen is currently active (`wall.fullscreen.active`).
    fn fullscreen_active(&self) -> bool {
        false
    }

    /// Current camera zoom (`wall.viewport.zoom`).
    fn zoom(&self) -> f64 {
        AGENT_BASE_ZOOM
    }
}

/// Group metadata queried during canvas footprint shoving (`TerminalWall._standOf` / `footprintChanged`).
#[derive(Clone, Debug, PartialEq)]
pub struct GroupStand {
    pub layout: GroupLayout,
    pub origin: Option<Point>,
    pub members: Vec<String>,
}

/// Mutable canvas adapter enabling [`AgentViews::footprint_changed`] to relayout
/// tiled/split groups and shove or restore covered canvas items (`TerminalWall.footprintChanged`).
pub trait FootprintCanvas: AgentViewsHost {
    /// Terminal ids in arrival order (`wall.order`).
    fn terminal_order(&self) -> Vec<String>;

    /// Group id containing terminal `id`, if any (`store.groupOf(id)`).
    fn group_of(&self, id: &str) -> Option<String>;

    /// Group layout, stored origin, and member list for `gid`.
    fn group_stand(&self, gid: &str) -> Option<GroupStand>;

    /// Re-layout tiled or split group `gid` using updated Agent View footprints/insets.
    fn relayout_group(&mut self, views: &AgentViews, gid: &str);

    /// Combined canvas bounds (backdrop frame + header) of group `gid` at `zoom`
    /// (`bare` names a member whose open Agent View is excluded).
    fn group_box_rect(
        &self,
        views: &AgentViews,
        gid: &str,
        zoom: f64,
        bare: Option<&str>,
    ) -> Option<Rect>;

    /// Pre-fullscreen window rectangle (body + title bar) of terminal `id` at `zoom`.
    fn canvas_window_rect(&self, id: &str, zoom: f64) -> Option<Rect> {
        let term = self.terminal(id)?;
        let bar = self.bar_height(id, zoom).max(0.0);
        Some(Rect {
            max_y: term.body_rect.max_y + bar,
            ..term.body_rect
        })
    }

    /// Translate group `gid` by `(dx, dy)` and persist its members' positions & origin.
    fn move_group(&mut self, gid: &str, dx: f64, dy: f64);

    /// Translate loose/freeform terminal `id` by `(dx, dy)` and persist its position.
    fn move_terminal(&mut self, id: &str, dx: f64, dy: f64);
}

/// Target passed in [`AgentViewsEffect::ScrollTerminal`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollTarget {
    /// Return to live bottom (`terminal.scroll <id> live`).
    Live,
    /// Scroll `offset` rows above live bottom (`terminal.scroll <id> to <offset>`).
    Offset(u64),
}

/// Outbound side effect requested of `crates/agent-feed`, `termd`, or the wall.
#[derive(Clone, Debug, PartialEq)]
pub enum AgentViewsEffect {
    /// Request the current `agent sessions --json` list (`agentSessions/list`).
    ListSessions,
    /// Request a transcript byte window (`agentEvents/read`).
    ReadSession {
        session_id: String,
        /// `None` reads the latest `tail_bytes` at EOF; `Some(offset)` reads up to
        /// `tail_bytes` strictly before byte `offset` (`load_earlier`).
        until: Option<i64>,
        tail_bytes: u64,
    },
    /// Start following live transcript frames from `from_byte` (`agentEvents/follow`).
    FollowSession {
        session_id: String,
        from_byte: Option<i64>,
    },
    /// Stop following live transcript frames for `session_id` (`agentEvents/unfollow`).
    UnfollowSession { session_id: String },
    /// Scroll attached terminal `id` to live bottom or a primary-scrollback offset.
    ScrollTerminal { id: String, target: ScrollTarget },
    /// Notify the wall that terminal `id`'s Agent View footprint changed (emitted by
    /// [`AgentViews::set_open`] / [`AgentViews::set_side`] when not using `*_on_canvas`).
    FootprintChanged { id: String },
    /// Canvas shove or give-room-back moved group `group_id` by `(dx, dy)`.
    MoveGroup {
        group_id: String,
        dx: f64,
        dy: f64,
    },
    /// Canvas shove or give-room-back moved terminal `id` by `(dx, dy)`.
    MoveTerminal { id: String, dx: f64, dy: f64 },
    /// Display a user-facing wall notice (`wall.notify`).
    Notify { message: String },
    /// Request a chrome snapshot / frame redraw (`wall._emit()`).
    Redraw,
}

// ── Return records & UI/screen descriptors ───────────────────────────────────

/// Outcome returned by [`AgentViews::set_open`] and [`AgentViews::set_side`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenOutcome {
    pub open: bool,
    pub side: AgentSide,
    pub session: Option<String>,
    pub missing: Option<String>,
}

/// Outcome returned by [`AgentViews::set_filter`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilterOutcome {
    pub id: String,
    pub filters: FilterMap,
}

/// Outcome returned by [`AgentViews::toggle_event`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToggleEventOutcome {
    pub id: String,
    pub event_id: String,
    pub expanded: bool,
}

/// Outcome returned by [`AgentViews::set_bulk_expanded`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BulkExpandOutcome {
    pub id: String,
    pub changed: usize,
    pub expanded_count: usize,
}

/// Outcome returned by [`AgentViews::jump`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JumpOutcome {
    pub id: String,
    pub live: bool,
    pub event_id: Option<String>,
    pub offset: u64,
    pub now_scrollback: u64,
    pub exact: bool,
}

/// Outcome returned when initiating an asynchronous/effect-driven `load_earlier` read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadEarlierTicket {
    pub id: String,
    pub session_id: String,
    /// `Some(until_byte)` when a [`AgentViewsEffect::ReadSession`] was queued; `None`
    /// when already loading or at byte `0`.
    pub until: Option<i64>,
    pub has_earlier: bool,
}

/// One visible row in [`AgentViewDescriptor::rows`] (`WallAgentViews.describe`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentEventRow {
    pub summary: EventRowSummary,
    pub expanded: bool,
    pub jumped: bool,
    pub exact_anchor: bool,
    pub detail: Option<ExpandedEvent>,
}

/// Snapshot of an open Agent View for chrome rendering (`WallAgentViews.describe`).
#[derive(Clone, Debug, PartialEq)]
pub struct AgentViewDescriptor {
    pub id: String,
    pub open: bool,
    pub side: AgentSide,
    pub session_binding: SessionBinding,
    pub session_id: Option<String>,
    pub session_via: Option<ResolveVia>,
    pub missing: Option<String>,
    pub session: Option<AgentSession>,
    pub session_label: String,
    pub filters: FilterMap,
    pub filter_label: String,
    pub category_counts: BTreeMap<String, usize>,
    pub expanded_count: usize,
    pub total_count: usize,
    pub standard_count: usize,
    pub visible_count: usize,
    /// Live events added while `!stick_bottom`.
    pub unread_count: usize,
    /// Whether the event list is pinned to its live bottom tail.
    pub stick_bottom: bool,
    pub loading: bool,
    pub loading_earlier: bool,
    pub has_earlier: bool,
    pub following: bool,
    pub error: Option<String>,
    pub view_offset: u64,
    pub alt_screen: bool,
    pub rows: Vec<AgentEventRow>,
}

/// Visible per-frame screen placement computed from [`WallCamera`] (`updateScreenTransforms`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VisibleScreenPlacement {
    /// World-space rectangle of the Agent View.
    pub world_rect: Rect,
    /// Integer-snapped left screen coordinate in viewport pixels.
    pub left: i64,
    /// Integer-snapped top screen coordinate in viewport pixels.
    pub top: i64,
    /// Integer-snapped outer frame width in pixels (`>= 28` in badge mode, else `>= 8`).
    pub width: u32,
    /// Integer-snapped outer frame height in pixels (`>= 18` in badge mode, else `>= 8`).
    pub height: u32,
    /// Scale factor (`zoom / AGENT_BASE_ZOOM`) applied to inner card layout.
    pub scale: f64,
    /// True when `BASE_FONT_PX * scale < BADGE_FLOOR_PX` (`11 * scale < 7.2`).
    pub badge_mode: bool,
    /// Whether this Agent View's attached terminal currently holds focus.
    pub focused: bool,
    /// Stacking tier (`4` when focused, `3` when idle).
    pub z_index: u8,
    /// Unscaled logical width (`(width - 2).max(1) / scale`) of the inner card (`0.0` in badge mode).
    pub logical_width: f64,
    /// Unscaled logical height (`(height - 2).max(1) / scale`) of the inner card (`0.0` in badge mode).
    pub logical_height: f64,
    /// Minimum internal hairline border thickness (`ceil(1 / scale).max(1)`) so internal
    /// 1 px dividers never round to `0 px` when zoomed out.
    pub hairline_px: u32,
}

/// Per-frame visibility and screen geometry of a terminal's Agent View (`updateScreenTransforms`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScreenPlacement {
    /// Hidden because fullscreen is active, the terminal is off-canvas, or its view is closed.
    Hidden,
    /// Open on the canvas, but outside the camera's viewport bounds.
    Culled,
    /// Visible inside the viewport at integer-snapped screen coordinates.
    Visible(VisibleScreenPlacement),
}

// ── Internal state records ───────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct ViewEntry {
    open: bool,
    side: AgentSide,
    session_binding: SessionBinding,
    filters: FilterMap,
    expanded_ids: HashSet<String>,
    full_body_ids: HashSet<String>,
    anchors: HashMap<String, LineAnchor>,
    resolved_session_id: Option<String>,
    resolved_via: Option<ResolveVia>,
    missing: Option<String>,
    jumped_event_id: Option<String>,
    stick_bottom: bool,
    unread_count: usize,
}

impl ViewEntry {
    fn new(default_side: AgentSide) -> Self {
        ViewEntry {
            open: false,
            side: default_side,
            session_binding: SessionBinding::Auto,
            filters: default_filters(),
            expanded_ids: HashSet::new(),
            full_body_ids: HashSet::new(),
            anchors: HashMap::new(),
            resolved_session_id: None,
            resolved_via: None,
            missing: None,
            jumped_event_id: None,
            stick_bottom: true,
            unread_count: 0,
        }
    }
}

#[derive(Clone, Debug)]
struct StreamEntry {
    frame: AgentEventFrame,
    pos: Option<EventPosition>,
}

#[derive(Clone, Debug)]
struct SessionStream {
    entries: HashMap<String, StreamEntry>,
    order: Vec<String>,
    orphans: HashMap<String, AgentEventFrame>,
    session_line: Option<AgentSession>,
    oldest_at: Option<i64>,
    last_at: Option<i64>,
    transcript_size: Option<u64>,
    loading: bool,
    loading_earlier: bool,
    has_earlier: bool,
    following: bool,
    error: Option<String>,
    live_reader: AgentEventReader,
    generation: u64,
}

impl SessionStream {
    fn new() -> Self {
        SessionStream {
            entries: HashMap::new(),
            order: Vec::new(),
            orphans: HashMap::new(),
            session_line: None,
            oldest_at: None,
            last_at: None,
            transcript_size: None,
            loading: false,
            loading_earlier: false,
            has_earlier: false,
            following: false,
            error: None,
            live_reader: AgentEventReader::new(false),
            generation: 1,
        }
    }
}

#[derive(Clone, Debug)]
struct TermSync {
    scrollback_rows: u64,
    grid_scrolled_at_sync: u64,
    view: u64,
    alt: bool,
    pwd: String,
    cwd: String,
    title: String,
}

#[derive(Clone, Debug)]
struct ShovedUnit {
    id: Option<String>,
    gid: Option<String>,
    anchor: String,
    dx: f64,
    dy: f64,
    at: Point,
}

#[derive(Clone, Debug)]
struct ShoveMemory {
    at: Option<Point>,
    shoves: Vec<ShovedUnit>,
}

// ── Coordinator (`AgentViews`) ───────────────────────────────────────────────

/// Stateful coordinator for all per-terminal Agent Views on the wall (`WallAgentViews`).
#[derive(Clone, Debug)]
pub struct AgentViews {
    feed_ready: bool,
    unavailable_reason: Option<String>,
    sessions: Vec<AgentSession>,
    views: HashMap<String, ViewEntry>,
    streams: HashMap<String, SessionStream>,
    term_sync: HashMap<String, TermSync>,
    shoves: HashMap<String, ShoveMemory>,
    effects: Vec<AgentViewsEffect>,
    generation: u64,
}

impl Default for AgentViews {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentViews {
    /// Construct an empty [`AgentViews`] coordinator before store hydration.
    pub fn new() -> Self {
        AgentViews {
            feed_ready: false,
            unavailable_reason: None,
            sessions: Vec::new(),
            views: HashMap::new(),
            streams: HashMap::new(),
            term_sync: HashMap::new(),
            shoves: HashMap::new(),
            effects: Vec::new(),
            generation: 0,
        }
    }

    /// Construct an [`AgentViews`] coordinator hydrated from `store` using `default_side`
    /// as fallback side (`WallAgentViews._hydrateFromStore`).
    pub fn from_store(store: &WallStore, default_side: AgentSide) -> Self {
        let mut this = Self::new();
        this.hydrate_from_store(store, default_side);
        this
    }

    /// Monotonic state counter bumped whenever observable Agent View state changes.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Borrow pending outbound effects without draining them.
    pub fn effects(&self) -> &[AgentViewsEffect] {
        &self.effects
    }

    /// Drain all queued [`AgentViewsEffect`] items in FIFO order.
    pub fn drain_effects(&mut self) -> Vec<AgentViewsEffect> {
        std::mem::take(&mut self.effects)
    }

    fn emit_effect(&mut self, effect: AgentViewsEffect) {
        self.effects.push(effect);
    }

    fn bump_and_redraw(&mut self) {
        self.generation += 1;
        self.effects.push(AgentViewsEffect::Redraw);
    }

    // ── Lifecycle & feed connection ──────────────────────────────────────────

    /// Request an initial session list when the agent feed comes up (`WallAgentViews.onRelayUp`).
    pub fn on_feed_up(&mut self) {
        self.emit_effect(AgentViewsEffect::ListSessions);
    }

    /// Record that `agentSessions/*` / `agent` CLI is unavailable (e.g. `--no-gomuxai`).
    pub fn set_unavailable(&mut self, reason: Option<String>) {
        self.unavailable_reason = reason;
        self.bump_and_redraw();
    }

    /// Reason the agent session feed is unavailable, if any.
    pub fn unavailable_reason(&self) -> Option<&str> {
        self.unavailable_reason.as_deref()
    }

    /// Currently cached `agent sessions --json` list.
    pub fn sessions(&self) -> &[AgentSession] {
        &self.sessions
    }

    /// Tear down all active streams, emitting [`AgentViewsEffect::UnfollowSession`] for
    /// any followed session (`WallAgentViews.stop`).
    pub fn stop(&mut self) {
        let mut unfollow_ids: Vec<String> = self
            .streams
            .iter()
            .filter(|(_, s)| s.following)
            .map(|(sid, _)| sid.clone())
            .collect();
        unfollow_ids.sort();
        for stream in self.streams.values_mut() {
            stream.generation += 1;
            stream.following = false;
        }
        self.streams.clear();
        self.feed_ready = false;
        for session_id in unfollow_ids {
            self.emit_effect(AgentViewsEffect::UnfollowSession { session_id });
        }
    }

    // ── Store hydration, persistence & pruning ───────────────────────────────

    /// Hydrate per-terminal open/side/session/filter settings from `store`
    /// (`WallAgentViews._hydrateFromStore`).
    pub fn hydrate_from_store(&mut self, store: &WallStore, default_side: AgentSide) {
        for (id, saved) in store.agent_views() {
            let entry = self
                .views
                .entry(id.clone())
                .or_insert_with(|| ViewEntry::new(default_side));
            entry.open = saved.open;
            entry.side = saved.side;
            entry.session_binding = SessionBinding::from_store_str(&saved.session);
            entry.filters = normalize_filters(saved.filters.as_ref());
        }
    }

    fn persist_entry(&self, store: Option<&mut WallStore>, id: &str) {
        let (Some(store), Some(v)) = (store, self.views.get(id)) else {
            return;
        };
        store.set_agent_view(
            id,
            Some(AgentViewState {
                open: v.open,
                side: v.side,
                session: v.session_binding.as_store_str(),
                filters: Some(v.filters.clone()),
            }),
        );
    }

    fn ensure_entry(&mut self, id: &str, default_side: AgentSide) -> &mut ViewEntry {
        self.views
            .entry(id.to_string())
            .or_insert_with(|| ViewEntry::new(default_side))
    }

    /// Drop runtime view, sync, shove, and unused stream state for terminals that
    /// were killed or pruned (`WallAgentViews.prune`).
    pub fn prune<F>(&mut self, host: &dyn AgentViewsHost, mut is_live: F)
    where
        F: FnMut(&str) -> bool,
    {
        let mut changed = false;
        let ids: Vec<String> = self
            .views
            .keys()
            .chain(self.term_sync.keys())
            .chain(self.shoves.keys())
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        for id in ids {
            if !is_live(&id) {
                changed |= self.views.remove(&id).is_some();
                self.term_sync.remove(&id);
                self.shoves.remove(&id);
            }
        }
        if changed {
            self.gc_streams(host);
            self.generation += 1;
        }
    }

    /// Synchronize open/side/session/filter state from `store`, marking any existing
    /// entry absent from `store` closed while preserving in-memory shove records so
    /// [`Self::footprint_changed`] can still give room back when a view closes.
    pub fn sync_open_from_store(&mut self, store: &WallStore, default_side: AgentSide) {
        for (id, entry) in &mut self.views {
            if !store.agent_views().contains_key(id) {
                entry.open = false;
            }
        }
        self.hydrate_from_store(store, default_side);
    }

    /// Forget all view, sync, and shove state for a killed or pruned terminal `id`.
    pub fn forget_terminal(&mut self, id: &str) {
        self.views.remove(id);
        self.term_sync.remove(id);
        self.shoves.remove(id);
    }

    // ── Queries & geometry ───────────────────────────────────────────────────

    /// Whether an Agent View is open on mounted terminal `id` (`WallAgentViews.isOpen`).
    pub fn is_open(&self, host: &dyn AgentViewsHost, id: &str) -> bool {
        self.views.get(id).is_some_and(|v| v.open) && host.terminal(id).is_some()
    }

    /// Whether `id`'s stored view flag is open without querying a host (useful inside
    /// split-borrow [`crate::groups::WallHost`] adapters where `id` is already known live).
    pub fn is_marked_open(&self, id: &str) -> bool {
        self.views.get(id).is_some_and(|v| v.open)
    }

    /// Attached side (`right` | `left` | `top` | `bottom`) when open on mounted terminal
    /// `id`, else `None` (`WallAgentViews.sideOf`).
    pub fn side_of(&self, host: &dyn AgentViewsHost, id: &str) -> Option<AgentSide> {
        self.is_open(host, id).then(|| self.views[id].side)
    }

    /// Attached side when `id`'s view is marked open, without querying a host.
    pub fn marked_side_of(&self, id: &str) -> Option<AgentSide> {
        self.views.get(id).filter(|v| v.open).map(|v| v.side)
    }

    /// Current session pairing mode (`Auto` or `Pinned`) for terminal `id`.
    pub fn session_binding_of(&self, id: &str) -> SessionBinding {
        self.views
            .get(id)
            .map(|v| v.session_binding.clone())
            .unwrap_or_default()
    }

    /// Currently resolved `session_id` for terminal `id`, if paired.
    pub fn resolved_session_of(&self, id: &str) -> Option<&str> {
        self.views
            .get(id)
            .and_then(|v| v.resolved_session_id.as_deref())
    }

    /// Configured world dimensions `{ width, height, gap }` (`WallAgentViews.dims`).
    pub fn dims(&self, host: &dyn AgentViewsHost) -> AgentViewDims {
        host.agent_views_config().dims
    }

    /// World-space rectangle of `id`'s open Agent View at `zoom`, or `None` when closed
    /// (`WallAgentViews.rect`).
    pub fn rect(&self, host: &dyn AgentViewsHost, id: &str, zoom: f64) -> Option<Rect> {
        let side = self.side_of(host, id)?;
        let term = host.terminal(id)?;
        let bar = host.bar_height(id, zoom);
        Some(agent_view_rect(
            term.body_rect,
            bar,
            side,
            host.agent_views_config().dims,
        ))
    }

    /// Combined canvas footprint (window + open Agent View) of `id` at `zoom`
    /// (`TerminalWall.canvasFootprintRect`).
    pub fn canvas_footprint_rect(
        &self,
        host: &dyn AgentViewsHost,
        id: &str,
        zoom: f64,
    ) -> Option<Rect> {
        let term = host.terminal(id)?;
        let bar = host.bar_height(id, zoom).max(0.0);
        let win = Rect {
            max_y: term.body_rect.max_y + bar,
            ..term.body_rect
        };
        let dims = host.agent_views_config().dims;
        Some(self.footprint_rect_from_parts(id, term.body_rect, win, bar, dims))
    }

    /// Compute a terminal's canvas footprint directly from pre-fetched geometry
    /// (avoids re-borrowing the host inside [`crate::groups::WallHost`] implementations).
    pub fn footprint_rect_from_parts(
        &self,
        id: &str,
        body_rect: Rect,
        window_rect: Rect,
        bar_height: f64,
        dims: AgentViewDims,
    ) -> Rect {
        let Some(side) = self.marked_side_of(id) else {
            return window_rect;
        };
        let ag = agent_view_rect(body_rect, bar_height, side, dims);
        union_rect(&[window_rect, ag]).unwrap_or(window_rect)
    }

    /// Zoom-invariant tiling metrics for `id` (`WallAgentViews.tileMetrics`).
    pub fn tile_metrics(
        &self,
        host: &dyn AgentViewsHost,
        id: &str,
    ) -> Option<FootprintTileMetrics> {
        let term = host.terminal(id)?;
        Some(footprint_tile_metrics(
            term.body_rect,
            self.side_of(host, id),
            host.agent_views_config().dims,
        ))
    }

    /// Convert [`AgentViews`] tiling metrics for `id` directly into [`crate::groups::TileMetrics`]
    /// for [`crate::groups::WallHost::agent_tile_metrics`].
    pub fn tile_metrics_for_body(
        &self,
        id: &str,
        body_rect: Rect,
        dims: AgentViewDims,
    ) -> TileMetrics {
        let m = footprint_tile_metrics(body_rect, self.marked_side_of(id), dims);
        TileMetrics {
            w: m.w,
            h: m.h,
            shift_x: m.shift_x,
            shift_y: m.shift_y,
            tile_box: m.tile_box,
        }
    }

    /// Per-edge room `id`'s open Agent View takes inside a split pane (`WallAgentViews.inset`).
    pub fn inset(&self, host: &dyn AgentViewsHost, id: &str) -> AgentInset {
        agent_inset(self.side_of(host, id), host.agent_views_config().dims)
    }

    /// Convert `id`'s split-pane inset directly into [`crate::groups::PaneInset`] for
    /// [`crate::groups::WallHost::agent_inset`].
    pub fn pane_inset_with_dims(&self, id: &str, dims: AgentViewDims) -> PaneInset {
        let ins = agent_inset(self.marked_side_of(id), dims);
        PaneInset {
            left: ins.left,
            right: ins.right,
            top: ins.top,
            bottom: ins.bottom,
        }
    }

    /// Current estimated primary-screen `scrollback_rows` for terminal `id`
    /// (`WallAgentViews.scrollbackNow`).
    pub fn scrollback_now(&self, host: &dyn AgentViewsHost, id: &str) -> u64 {
        let grid = host.terminal(id);
        let Some(sync) = self.term_sync.get(id) else {
            return grid.map_or(0, |t| t.scrolled_rows);
        };
        let live_delta = if !sync.alt && sync.view == 0 {
            grid.map_or(0, |t| t.scrolled_rows.saturating_sub(sync.grid_scrolled_at_sync))
        } else {
            0
        };
        sync.scrollback_rows + live_delta
    }

    /// Current `termd` subscriber `view` offset (`0` = live) for terminal `id`
    /// (`WallAgentViews.viewOffsetOf`).
    pub fn view_offset_of(&self, id: &str) -> u64 {
        self.term_sync.get(id).map_or(0, |s| s.view)
    }

    /// Whether terminal `id` is currently on the alternate screen (`WallAgentViews.isAltScreen`).
    pub fn is_alt_screen(&self, host: &dyn AgentViewsHost, id: &str) -> bool {
        host.terminal(id).is_some_and(|t| t.alt_screen)
            || self.term_sync.get(id).is_some_and(|s| s.alt)
    }

    // ── Commands & mutations ─────────────────────────────────────────────────

    fn require_terminal(&self, host: &dyn AgentViewsHost, id: &str) -> Result<(), String> {
        if id.is_empty() {
            return Err("name a terminal".to_string());
        }
        if host.terminal(id).is_none() {
            return Err(format!("no terminal '{id}'"));
        }
        Ok(())
    }

    fn apply_open_state(
        &mut self,
        host: &dyn AgentViewsHost,
        store: Option<&mut WallStore>,
        id: &str,
        how: &str,
        side: Option<AgentSide>,
    ) -> Result<OpenOutcome, String> {
        self.require_terminal(host, id)?;
        let default_side = host.agent_views_config().default_side;
        let next_how_side = parse_agent_side(how).ok();
        {
            let v = self.ensure_entry(id, default_side);
            if let Some(s) = next_how_side {
                v.open = true;
                v.side = s;
            } else {
                match how {
                    "toggle" => v.open = !v.open,
                    "open" | "on" => v.open = true,
                    "close" | "off" => v.open = false,
                    other => {
                        return Err(format!(
                            "agent view takes toggle|open|close|right|left|top|bottom, not '{other}'"
                        ));
                    }
                }
            }
            if let Some(explicit_side) = side {
                v.side = explicit_side;
            }
        }
        self.persist_entry(store, id);
        self.rebind_one(host, id, false);
        self.gc_streams(host);
        let v = &self.views[id];
        Ok(OpenOutcome {
            open: v.open,
            side: v.side,
            session: v.resolved_session_id.clone(),
            missing: if v.open { v.missing.clone() } else { None },
        })
    }

    /// Open, close, or toggle terminal `id`'s Agent View (`WallAgentViews.setOpen`),
    /// emitting [`AgentViewsEffect::FootprintChanged`] + [`AgentViewsEffect::Redraw`].
    pub fn set_open(
        &mut self,
        host: &dyn AgentViewsHost,
        store: Option<&mut WallStore>,
        id: &str,
        how: &str,
        side: Option<&str>,
    ) -> Result<OpenOutcome, String> {
        let parsed_side = match side {
            Some(s) => Some(parse_agent_side(s)?),
            None => None,
        };
        let out = self.apply_open_state(host, store, id, how, parsed_side)?;
        self.emit_effect(AgentViewsEffect::FootprintChanged {
            id: id.to_string(),
        });
        self.bump_and_redraw();
        Ok(out)
    }

    /// Open, close, or toggle terminal `id`'s Agent View and immediately run
    /// [`Self::footprint_changed`] against `canvas` (`WallAgentViews.setOpen`).
    pub fn set_open_on_canvas<C: FootprintCanvas>(
        &mut self,
        canvas: &mut C,
        store: Option<&mut WallStore>,
        id: &str,
        how: &str,
        side: Option<&str>,
    ) -> Result<OpenOutcome, String> {
        let parsed_side = match side {
            Some(s) => Some(parse_agent_side(s)?),
            None => None,
        };
        let out = self.apply_open_state(canvas, store, id, how, parsed_side)?;
        self.footprint_changed(canvas, id);
        self.bump_and_redraw();
        Ok(out)
    }

    /// Move terminal `id`'s Agent View to `side` (`right`|`left`|`top`|`bottom`),
    /// opening it if closed (`WallAgentViews.setSide`).
    pub fn set_side(
        &mut self,
        host: &dyn AgentViewsHost,
        store: Option<&mut WallStore>,
        id: &str,
        side: &str,
    ) -> Result<OpenOutcome, String> {
        let _ = parse_agent_side(side)?;
        self.set_open(host, store, id, "open", Some(side))
    }

    /// Move terminal `id`'s Agent View to `side` and immediately run
    /// [`Self::footprint_changed`] against `canvas`.
    pub fn set_side_on_canvas<C: FootprintCanvas>(
        &mut self,
        canvas: &mut C,
        store: Option<&mut WallStore>,
        id: &str,
        side: &str,
    ) -> Result<OpenOutcome, String> {
        let _ = parse_agent_side(side)?;
        self.set_open_on_canvas(canvas, store, id, "open", Some(side))
    }

    /// Set terminal `id`'s session pairing to `"auto"` ([`SessionBinding::Auto`]) or a
    /// chosen `session_id` ([`SessionBinding::Pinned`]), persisting it in `store` and
    /// rebinding streams.
    pub fn set_session(
        &mut self,
        host: &dyn AgentViewsHost,
        store: Option<&mut WallStore>,
        id: &str,
        session: &str,
    ) -> Result<OpenOutcome, String> {
        self.require_terminal(host, id)?;
        let default_side = host.agent_views_config().default_side;
        let binding = SessionBinding::from_store_str(session);
        self.ensure_entry(id, default_side).session_binding = binding;
        self.persist_entry(store, id);
        self.rebind_one(host, id, false);
        self.gc_streams(host);
        self.bump_and_redraw();
        let v = &self.views[id];
        Ok(OpenOutcome {
            open: v.open,
            side: v.side,
            session: v.resolved_session_id.clone(),
            missing: if v.open { v.missing.clone() } else { None },
        })
    }

    /// Change `id`'s active category filters (`reset`|`only`|`on`|`off`|`toggle`;
    /// `WallAgentViews.setFilter`).
    pub fn set_filter<S: AsRef<str>>(
        &mut self,
        host: &dyn AgentViewsHost,
        store: Option<&mut WallStore>,
        id: &str,
        mode: &str,
        tokens: &[S],
    ) -> Result<FilterOutcome, String> {
        self.require_terminal(host, id)?;
        let parsed_mode = FilterMode::parse(mode)?;
        let default_side = host.agent_views_config().default_side;
        let next_filters = {
            let v = self.ensure_entry(id, default_side);
            let updated = apply_filter_change(Some(&v.filters), parsed_mode, tokens)?;
            v.filters = updated.clone();
            updated
        };
        self.persist_entry(store, id);
        self.bump_and_redraw();
        Ok(FilterOutcome {
            id: id.to_string(),
            filters: next_filters,
        })
    }

    /// Toggle or set expansion (`"toggle"` | `"on"` | `"off"`) on a single event row
    /// (`WallAgentViews.toggleEvent`).
    pub fn toggle_event(
        &mut self,
        host: &dyn AgentViewsHost,
        id: &str,
        event_id: &str,
        how: &str,
    ) -> Result<ToggleEventOutcome, String> {
        self.require_terminal(host, id)?;
        if event_id.is_empty() {
            return Err("name an event id".to_string());
        }
        let default_side = host.agent_views_config().default_side;
        let v = self.ensure_entry(id, default_side);
        let next = if how == "toggle" {
            !v.expanded_ids.contains(event_id)
        } else {
            how == "on"
        };
        if next {
            v.expanded_ids.insert(event_id.to_string());
        } else {
            v.expanded_ids.remove(event_id);
            v.full_body_ids.remove(event_id);
        }
        self.bump_and_redraw();
        Ok(ToggleEventOutcome {
            id: id.to_string(),
            event_id: event_id.to_string(),
            expanded: next,
        })
    }

    /// Expand the full uncapped text of a truncated event body (`WallAgentViews.showFullBody`).
    pub fn show_full_body(
        &mut self,
        host: &dyn AgentViewsHost,
        id: &str,
        event_id: &str,
    ) -> Result<(), String> {
        self.require_terminal(host, id)?;
        if event_id.is_empty() {
            return Err("name an event id".to_string());
        }
        let default_side = host.agent_views_config().default_side;
        let v = self.ensure_entry(id, default_side);
        v.expanded_ids.insert(event_id.to_string());
        v.full_body_ids.insert(event_id.to_string());
        self.bump_and_redraw();
        Ok(())
    }

    /// Bulk expand (`expand = true`) or collapse (`expand = false`) all visible events or
    /// a subset of categories/groups (`WallAgentViews.setBulkExpanded`).
    pub fn set_bulk_expanded<S: AsRef<str>>(
        &mut self,
        host: &dyn AgentViewsHost,
        id: &str,
        expand: bool,
        tokens: &[S],
    ) -> Result<BulkExpandOutcome, String> {
        self.require_terminal(host, id)?;
        let default_side = host.agent_views_config().default_side;
        self.ensure_entry(id, default_side);

        let has_all = tokens
            .iter()
            .any(|t| t.as_ref().eq_ignore_ascii_case("all"));
        if !expand && (tokens.is_empty() || has_all) {
            let v = self.views.get_mut(id).expect("entry ensured above");
            let count = v.expanded_ids.len();
            v.expanded_ids.clear();
            v.full_body_ids.clear();
            self.bump_and_redraw();
            return Ok(BulkExpandOutcome {
                id: id.to_string(),
                changed: count,
                expanded_count: 0,
            });
        }

        if !tokens.is_empty() && !has_all {
            expand_category_tokens(tokens)?;
        }

        let records = if expand {
            self.visible_records_for(id)
        } else {
            self.all_records_for(id)
        };
        let mut matching_ids = Vec::new();
        for rec in &records {
            if matches_subset(rec, tokens)? {
                matching_ids.push(rec.id.clone());
            }
        }

        let v = self.views.get_mut(id).expect("entry ensured above");
        let mut changed = 0usize;
        for rec_id in matching_ids {
            if expand && !v.expanded_ids.contains(&rec_id) {
                v.expanded_ids.insert(rec_id);
                changed += 1;
            } else if !expand && v.expanded_ids.remove(&rec_id) {
                v.full_body_ids.remove(&rec_id);
                changed += 1;
            }
        }
        let expanded_count = v.expanded_ids.len();
        self.bump_and_redraw();
        Ok(BulkExpandOutcome {
            id: id.to_string(),
            changed,
            expanded_count,
        })
    }

    /// Update whether `id`'s event scroll list is pinned to its live bottom tail;
    /// pinning to bottom clears `unread_count` (`AgentViewCard` scroll tracking).
    pub fn set_stick_bottom(
        &mut self,
        host: &dyn AgentViewsHost,
        id: &str,
        at_bottom: bool,
    ) -> Result<(), String> {
        self.require_terminal(host, id)?;
        let default_side = host.agent_views_config().default_side;
        let v = self.ensure_entry(id, default_side);
        let prev = (v.stick_bottom, v.unread_count);
        v.stick_bottom = at_bottom;
        if at_bottom {
            v.unread_count = 0;
        }
        if (v.stick_bottom, v.unread_count) != prev {
            self.bump_and_redraw();
        }
        Ok(())
    }

    /// Re-pin `id`'s event scroll list to the live tail and clear its unread count.
    pub fn scroll_to_tail(&mut self, host: &dyn AgentViewsHost, id: &str) -> Result<(), String> {
        self.set_stick_bottom(host, id, true)
    }

    /// Jump attached terminal `id` to `"live"` or an `event_id` in its paired stream
    /// (`WallAgentViews.jump`).
    ///
    /// Refuses while `id` is on the alternate screen, queuing [`AgentViewsEffect::Notify`]
    /// and returning `Err`. Otherwise queues [`AgentViewsEffect::ScrollTerminal`].
    pub fn jump(
        &mut self,
        host: &dyn AgentViewsHost,
        id: &str,
        target: &str,
    ) -> Result<JumpOutcome, String> {
        self.require_terminal(host, id)?;
        let default_side = host.agent_views_config().default_side;
        self.ensure_entry(id, default_side);

        if self.is_alt_screen(host, id) {
            let msg = format!(
                "{} is on the alternate screen — primary scrollback jump is unavailable while a full-screen TUI is active",
                host.terminal_name(id)
            );
            self.generation += 1;
            self.emit_effect(AgentViewsEffect::Notify {
                message: msg.clone(),
            });
            return Err(msg);
        }

        let now_scrollback = self.scrollback_now(host, id);
        let grid_scrolled = host.terminal(id).map_or(0, |t| t.scrolled_rows);

        if target.is_empty() || target == "live" {
            if let Some(v) = self.views.get_mut(id) {
                v.jumped_event_id = None;
            }
            if let Some(sync) = self.term_sync.get_mut(id) {
                sync.scrollback_rows = now_scrollback;
                sync.grid_scrolled_at_sync = grid_scrolled;
                sync.view = 0;
            }
            self.emit_effect(AgentViewsEffect::ScrollTerminal {
                id: id.to_string(),
                target: ScrollTarget::Live,
            });
            self.bump_and_redraw();
            return Ok(JumpOutcome {
                id: id.to_string(),
                live: true,
                event_id: None,
                offset: 0,
                now_scrollback,
                exact: true,
            });
        }

        let sid = self
            .views
            .get(id)
            .and_then(|v| v.resolved_session_id.clone());
        let (entry_proto, approx) = {
            let stream = sid.as_deref().and_then(|s| self.streams.get(s));
            let Some(stream) = stream else {
                return Err(format!("no event '{target}' in {id}'s agent stream"));
            };
            let Some(entry) = stream.entries.get(target) else {
                return Err(format!("no event '{target}' in {id}'s agent stream"));
            };
            (
                entry.frame.protocol_terminal_line,
                Self::approximate_anchor(stream, entry, now_scrollback),
            )
        };

        let mut anchor = self
            .views
            .get(id)
            .and_then(|v| v.anchors.get(target).copied());
        if anchor.is_none() {
            anchor = if let Some(proto) = entry_proto {
                Some(LineAnchor {
                    terminal_line: Some(proto as f64),
                    scrollback_rows: None,
                    cursor_row: None,
                    exact: Some(true),
                })
            } else {
                approx
            };
            if let (Some(a), Some(v)) = (anchor, self.views.get_mut(id)) {
                v.anchors.insert(target.to_string(), a);
            }
        }

        let offset = compute_jump_offset(anchor.as_ref(), now_scrollback as f64, DEFAULT_JUMP_CONTEXT_ROWS);
        if let Some(v) = self.views.get_mut(id) {
            v.jumped_event_id = Some(target.to_string());
        }
        if let Some(sync) = self.term_sync.get_mut(id) {
            sync.scrollback_rows = now_scrollback;
            sync.grid_scrolled_at_sync = grid_scrolled;
            sync.view = offset;
        }
        let scroll_target = if offset == 0 {
            ScrollTarget::Live
        } else {
            ScrollTarget::Offset(offset)
        };
        self.emit_effect(AgentViewsEffect::ScrollTerminal {
            id: id.to_string(),
            target: scroll_target,
        });
        self.bump_and_redraw();
        Ok(JumpOutcome {
            id: id.to_string(),
            live: false,
            event_id: Some(target.to_string()),
            offset,
            now_scrollback,
            exact: anchor.and_then(|a| a.exact).unwrap_or(false),
        })
    }

    /// Queue a `load_earlier` transcript window read before `oldest_at` for `id`'s
    /// active session (`WallAgentViews.loadEarlier`).
    ///
    /// Emits [`AgentViewsEffect::ReadSession`] with `until: Some(oldest_at)` when older
    /// bytes exist; feed the resulting page back via [`Self::apply_read_page`] or
    /// [`Self::apply_read_jsonl`].
    pub fn request_load_earlier(
        &mut self,
        host: &dyn AgentViewsHost,
        id: &str,
    ) -> Result<LoadEarlierTicket, String> {
        self.require_terminal(host, id)?;
        let Some(sid) = self
            .views
            .get(id)
            .and_then(|v| v.resolved_session_id.clone())
        else {
            return Err(format!("{id} has no active agent session"));
        };
        let Some(stream) = self.streams.get_mut(&sid) else {
            return Ok(LoadEarlierTicket {
                id: id.to_string(),
                session_id: sid,
                until: None,
                has_earlier: false,
            });
        };
        let oldest = stream.oldest_at.unwrap_or(0);
        if stream.loading_earlier || !stream.has_earlier || oldest <= 0 {
            return Ok(LoadEarlierTicket {
                id: id.to_string(),
                session_id: sid,
                until: None,
                has_earlier: stream.has_earlier,
            });
        }
        stream.loading_earlier = true;
        self.emit_effect(AgentViewsEffect::ReadSession {
            session_id: sid.clone(),
            until: Some(oldest),
            tail_bytes: STREAM_WINDOW_BYTES,
        });
        self.bump_and_redraw();
        Ok(LoadEarlierTicket {
            id: id.to_string(),
            session_id: sid,
            until: Some(oldest),
            has_earlier: true,
        })
    }

    // ── Plain-data feed & termd ingestion ────────────────────────────────────

    /// Apply a `termd/status` terminal snapshot, updating primary scrollback baselines
    /// and rebinding sessions if any terminal's `pwd` or `cwd` changed
    /// (`WallAgentViews._onTermdStatus`).
    pub fn sync_termd_status(
        &mut self,
        host: &dyn AgentViewsHost,
        rows: &[TermdTerminalStatus],
    ) {
        let mut rebind_needed = false;
        for row in rows {
            let grid_scrolled = host.terminal(&row.id).map_or(0, |t| t.scrolled_rows);
            let next = TermSync {
                scrollback_rows: row.scrollback_rows,
                grid_scrolled_at_sync: grid_scrolled,
                view: row.view,
                alt: row.alt,
                pwd: row.pwd.clone(),
                cwd: row.cwd.clone(),
                title: row.title.clone(),
            };
            let changed = self
                .term_sync
                .get(&row.id)
                .is_none_or(|prev| prev.pwd != next.pwd || prev.cwd != next.cwd);
            if changed {
                rebind_needed = true;
            }
            self.term_sync.insert(row.id.clone(), next);
        }
        if rebind_needed {
            self.rebind_all(host, false);
        }
        self.bump_and_redraw();
    }

    /// Apply a fresh `agent sessions --json` list from `crates/agent-feed`, pairing
    /// open views and starting/restarting streams as needed (`onSessions` / `onRelayUp`).
    pub fn apply_session_list(
        &mut self,
        host: &dyn AgentViewsHost,
        sessions: Vec<AgentSession>,
        restart_follows: bool,
    ) {
        self.feed_ready = true;
        self.unavailable_reason = None;
        self.sessions = sessions;
        self.rebind_all(host, restart_follows);
        self.bump_and_redraw();
    }

    /// Apply a hydrated transcript window (`until == None` for initial tail open, or
    /// `until == Some(...)` for `load_earlier`), returning the count of newly loaded
    /// events (`WallAgentViews._openStream` & `loadEarlier`).
    pub fn apply_read_page(
        &mut self,
        host: &dyn AgentViewsHost,
        session_id: &str,
        page: SessionReadPage,
    ) -> usize {
        if page.until.is_some() {
            return self.apply_earlier_page(host, session_id, page);
        }
        let Some(stream) = self.streams.get_mut(session_id) else {
            return 0;
        };
        stream.transcript_size = Some(page.transcript_size);
        let mut reader = AgentEventReader::new(true);
        let mut changes = Vec::with_capacity(page.frames.len());
        for frame in page.frames {
            changes.push(reader.accept_frame(frame));
        }
        if let Some(session) = reader.session {
            stream.session_line = Some(session);
        }
        stream.last_at = reader.last_at;
        stream.entries.clear();
        stream.order.clear();
        stream.orphans.clear();

        let mut min_at: Option<i64> = None;
        for eid in &reader.order {
            let Some((frame, pos)) = reader.by_id.get(eid) else {
                continue;
            };
            min_at = Some(min_at.map_or(pos.at, |cur| cur.min(pos.at)));
            stream.entries.insert(
                eid.clone(),
                StreamEntry {
                    frame: frame.clone(),
                    pos: Some(*pos),
                },
            );
            stream.order.push(eid.clone());
        }
        for change in changes {
            if let EventChange::Update { frame, pos: None } = change {
                if !stream.entries.contains_key(&frame.id) {
                    stream.orphans.insert(frame.id.clone(), frame);
                }
            }
        }
        let oldest = min_at.unwrap_or(0);
        stream.oldest_at = Some(oldest);
        stream.has_earlier = oldest > 0;
        stream.loading = false;
        stream.error = None;
        let loaded = stream.order.len();

        self.seed_hydrated_anchors(host, session_id);

        let should_follow = self
            .sessions
            .iter()
            .find(|s| s.session_id == session_id)
            .is_none_or(|s| s.status != "ended");
        if should_follow {
            self.start_follow(session_id);
        }
        self.bump_and_redraw();
        loaded
    }

    fn apply_earlier_page(
        &mut self,
        host: &dyn AgentViewsHost,
        session_id: &str,
        page: SessionReadPage,
    ) -> usize {
        let Some(stream) = self.streams.get_mut(session_id) else {
            return 0;
        };
        let mut reader = AgentEventReader::new(true);
        let mut changes = Vec::with_capacity(page.frames.len());
        for frame in page.frames {
            changes.push(reader.accept_frame(frame));
        }

        let mut min_at = stream.oldest_at.unwrap_or(0);
        let mut older_ids = Vec::new();
        for eid in &reader.order {
            let Some((frame, pos)) = reader.by_id.get(eid) else {
                continue;
            };
            if pos.at < min_at {
                min_at = pos.at;
            }
            if stream.entries.contains_key(eid) {
                continue;
            }
            // Adopt any orphan update that landed inside the newer window while keeping
            // the position (`pos`) of this introducing `add`.
            let adopted = stream
                .orphans
                .remove(eid)
                .unwrap_or_else(|| frame.clone());
            stream.entries.insert(
                eid.clone(),
                StreamEntry {
                    frame: adopted,
                    pos: Some(*pos),
                },
            );
            older_ids.push(eid.clone());
        }
        for change in changes {
            if let EventChange::Update { frame, pos: None } = change {
                if !stream.entries.contains_key(&frame.id) {
                    stream.orphans.insert(frame.id.clone(), frame);
                }
            }
        }

        let loaded = older_ids.len();
        if loaded > 0 {
            older_ids.extend(std::mem::take(&mut stream.order));
            stream.order = older_ids;
            stream.oldest_at = Some(min_at);
            stream.has_earlier = min_at > 0;
            stream.loading_earlier = false;
            self.seed_hydrated_anchors(host, session_id);
        } else {
            stream.has_earlier = false;
            stream.loading_earlier = false;
        }
        self.bump_and_redraw();
        loaded
    }

    /// Convenience wrapper parsing JSONL lines into a [`SessionReadPage`] and applying it.
    pub fn apply_read_jsonl<S: AsRef<str>>(
        &mut self,
        host: &dyn AgentViewsHost,
        session_id: &str,
        lines: &[S],
        transcript_size: u64,
        until: Option<i64>,
    ) -> Result<usize, String> {
        let page = SessionReadPage::from_jsonl_lines(lines, transcript_size, until)?;
        Ok(self.apply_read_page(host, session_id, page))
    }

    /// Record a failed transcript read (`_openStream` / `loadEarlier` catch branch).
    pub fn apply_read_error(
        &mut self,
        session_id: &str,
        until: Option<i64>,
        error: impl Into<String>,
    ) {
        if let Some(stream) = self.streams.get_mut(session_id) {
            if until.is_some() {
                stream.loading_earlier = false;
            } else {
                stream.loading = false;
            }
            stream.error = Some(error.into());
            self.bump_and_redraw();
        }
    }

    /// Fold live stream frames (`agentEvents/lines`) into `session_id`'s stream,
    /// stamping write-time scrollback anchors onto open views (`WallAgentViews._onStreamLines`).
    pub fn apply_live_frames(
        &mut self,
        host: &dyn AgentViewsHost,
        session_id: &str,
        frames: &[AgentStreamFrame],
    ) -> bool {
        if frames.is_empty() || !self.streams.contains_key(session_id) {
            return false;
        }
        let mut changed = false;
        for frame in frames {
            let change = {
                let stream = self.streams.get_mut(session_id).expect("checked above");
                let c = stream.live_reader.accept_frame(frame.clone());
                if let Some(r_last) = stream.live_reader.last_at {
                    if stream.last_at.is_none_or(|prev| r_last > prev) {
                        stream.last_at = Some(r_last);
                    }
                }
                c
            };
            match change {
                EventChange::Session(session) => {
                    if let Some(stream) = self.streams.get_mut(session_id) {
                        stream.session_line = Some(session);
                    }
                    changed = true;
                }
                EventChange::Add { frame: ev, pos } => {
                    let already_held = self
                        .streams
                        .get(session_id)
                        .is_some_and(|s| s.entries.contains_key(&ev.id));
                    if already_held {
                        let proto = ev.protocol_terminal_line;
                        let eid = ev.id.clone();
                        if let Some(stream) = self.streams.get_mut(session_id) {
                            if let Some(existing) = stream.entries.get_mut(&eid) {
                                existing.frame = ev;
                            }
                        }
                        if let Some(line) = proto {
                            self.stamp_protocol_line(session_id, &eid, line);
                        }
                    } else {
                        if let Some(stream) = self.streams.get_mut(session_id) {
                            if stream.oldest_at.is_none() {
                                stream.oldest_at = Some(pos.at);
                            }
                            stream.order.push(ev.id.clone());
                            stream.entries.insert(
                                ev.id.clone(),
                                StreamEntry {
                                    frame: ev.clone(),
                                    pos: Some(pos),
                                },
                            );
                        }
                        self.stamp_live_add(host, session_id, &ev);
                        self.bump_unread_on_live_add(host, session_id, &ev);
                    }
                    changed = true;
                }
                EventChange::Update { frame: ev, .. } => {
                    let proto = ev.protocol_terminal_line;
                    let eid = ev.id.clone();
                    let updated_existing = {
                        let stream = self.streams.get_mut(session_id).expect("checked above");
                        if let Some(existing) = stream.entries.get_mut(&eid) {
                            existing.frame = ev;
                            true
                        } else {
                            stream.orphans.insert(eid.clone(), ev);
                            false
                        }
                    };
                    if updated_existing {
                        if let Some(line) = proto {
                            self.stamp_protocol_line(session_id, &eid, line);
                        }
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.bump_and_redraw();
        }
        changed
    }

    /// Fold raw live JSONL lines (`agentEvents/lines`) into `session_id`'s stream,
    /// ignoring blank or malformed lines just like `WallAgentViews._onStreamLines`.
    pub fn apply_live_jsonl<S: AsRef<str>>(
        &mut self,
        host: &dyn AgentViewsHost,
        session_id: &str,
        lines: &[S],
    ) -> bool {
        let mut parsed = Vec::with_capacity(lines.len());
        let ctx = format!("follow {session_id}");
        for raw in lines {
            if let Ok(Some(frame)) = AgentStreamFrame::from_json_line(raw.as_ref(), &ctx) {
                parsed.push(frame);
            }
        }
        self.apply_live_frames(host, session_id, &parsed)
    }

    /// Mark `session_id`'s live follow stream as ended (`agentEvents/ended`).
    pub fn apply_stream_ended(&mut self, session_id: &str) {
        if let Some(stream) = self.streams.get_mut(session_id) {
            if stream.following {
                stream.following = false;
                self.bump_and_redraw();
            }
        }
    }

    /// Record a live follow failure (`_startFollow` catch branch).
    pub fn apply_follow_error(&mut self, session_id: &str, error: impl Into<String>) {
        if let Some(stream) = self.streams.get_mut(session_id) {
            stream.following = false;
            stream.error = Some(error.into());
            self.bump_and_redraw();
        }
    }

    // ── Snapshot descriptors & 60fps screen placement ────────────────────────

    /// Build the UI view descriptor for terminal `id`, or `None` when closed or
    /// unmounted (`WallAgentViews.describe`).
    pub fn describe(&self, host: &dyn AgentViewsHost, id: &str) -> Option<AgentViewDescriptor> {
        let v = self.views.get(id)?;
        if !v.open || host.terminal(id).is_none() {
            return None;
        }
        let active_session = v
            .resolved_session_id
            .as_deref()
            .and_then(|sid| self.sessions.iter().find(|s| s.session_id == sid))
            .cloned();
        let stream = v
            .resolved_session_id
            .as_deref()
            .and_then(|sid| self.streams.get(sid));

        let mut category_counts: BTreeMap<String, usize> = CATEGORY_IDS
            .iter()
            .map(|&cid| (cid.to_string(), 0usize))
            .collect();
        let mut visible = Vec::new();
        if let Some(st) = stream {
            for eid in &st.order {
                let Some(item) = st.entries.get(eid) else {
                    continue;
                };
                let Some(rec) = item.frame.record_at(item.pos, true) else {
                    continue;
                };
                for cid in categories_of_record(&rec) {
                    if let Some(slot) = category_counts.get_mut(cid) {
                        *slot += 1;
                    }
                }
                if !matches_filter(&rec, &v.filters) {
                    continue;
                }
                visible.push(rec);
            }
        }
        let total_count = stream.map_or(0, |st| st.order.len());
        let noise_count = category_counts.get("noise").copied().unwrap_or(0);
        let standard_count = total_count.saturating_sub(noise_count);

        let rows: Vec<AgentEventRow> = visible
            .into_iter()
            .map(|rec| {
                let expanded = v.expanded_ids.contains(&rec.id);
                let summary = summarize_event_row(&rec, rec.abandoned);
                let detail = expanded.then(|| {
                    format_expanded_event(
                        &rec,
                        ExpandOptions {
                            full: v.full_body_ids.contains(&rec.id),
                            ..ExpandOptions::default()
                        },
                    )
                });
                AgentEventRow {
                    jumped: v.jumped_event_id.as_deref() == Some(rec.id.as_str()),
                    exact_anchor: v
                        .anchors
                        .get(&rec.id)
                        .and_then(|a| a.exact)
                        .unwrap_or(false),
                    summary,
                    expanded,
                    detail,
                }
            })
            .collect();

        let session_label = format_session_label(
            active_session
                .as_ref()
                .or_else(|| stream.and_then(|st| st.session_line.as_ref())),
        );
        let error = stream
            .and_then(|st| st.error.clone())
            .or_else(|| self.unavailable_reason.clone());

        Some(AgentViewDescriptor {
            id: id.to_string(),
            open: true,
            side: v.side,
            session_binding: v.session_binding.clone(),
            session_id: v.resolved_session_id.clone(),
            session_via: v.resolved_via,
            missing: v.missing.clone(),
            session: active_session,
            session_label,
            filters: v.filters.clone(),
            filter_label: describe_active_filter(&v.filters),
            category_counts,
            expanded_count: v.expanded_ids.len(),
            total_count,
            standard_count,
            visible_count: rows.len(),
            unread_count: v.unread_count,
            stick_bottom: v.stick_bottom,
            loading: stream.is_some_and(|st| st.loading),
            loading_earlier: stream.is_some_and(|st| st.loading_earlier),
            has_earlier: stream.is_some_and(|st| st.has_earlier),
            following: stream.is_some_and(|st| st.following),
            error,
            view_offset: self.view_offset_of(id),
            alt_screen: self.is_alt_screen(host, id),
            rows,
        })
    }

    /// Build [`AgentViewDescriptor`] snapshots for every open Agent View in `order`
    /// (`wall.snapshot().agentViews`).
    pub fn describe_all<S: AsRef<str>>(
        &self,
        host: &dyn AgentViewsHost,
        order: &[S],
    ) -> Vec<AgentViewDescriptor> {
        order
            .iter()
            .filter_map(|id| self.describe(host, id.as_ref()))
            .collect()
    }

    /// Compute the per-frame screen placement of terminal `id`'s Agent View from
    /// `camera` (`WallAgentViews.updateScreenTransforms`).
    pub fn screen_placement_of(
        &self,
        host: &dyn AgentViewsHost,
        camera: &WallCamera,
        id: &str,
    ) -> ScreenPlacement {
        let (view_w, view_h) = camera.view();
        self.screen_placement_at(host, camera.pose(), view_w, view_h, id)
    }

    /// Compute the per-frame screen placement of terminal `id`'s Agent View from an
    /// explicit [`CameraPose`] and viewport pixel size `(view_w, view_h)`.
    pub fn screen_placement_at(
        &self,
        host: &dyn AgentViewsHost,
        pose: CameraPose,
        view_w: f64,
        view_h: f64,
        id: &str,
    ) -> ScreenPlacement {
        if host.fullscreen_active() {
            return ScreenPlacement::Hidden;
        }
        let Some(term) = host.terminal(id) else {
            return ScreenPlacement::Hidden;
        };
        if !term.on_canvas || !self.is_open(host, id) {
            return ScreenPlacement::Hidden;
        }
        let zoom = pose.zoom;
        let Some(world_rect) = self.rect(host, id, zoom) else {
            return ScreenPlacement::Hidden;
        };
        let screen_box = pose.to_screen_box(world_rect, view_w, view_h);
        if screen_box.right < 0.0
            || screen_box.left > view_w
            || screen_box.bottom < 0.0
            || screen_box.top > view_h
        {
            return ScreenPlacement::Culled;
        }

        let scale = zoom / AGENT_BASE_ZOOM;
        let badge_mode = BASE_FONT_PX * scale < BADGE_FLOOR_PX;
        let focused = host.focused_id() == Some(id);
        let left = js_round(screen_box.left) as i64;
        let top = js_round(screen_box.top) as i64;
        let min_w = if badge_mode { 28 } else { 8 };
        let min_h = if badge_mode { 18 } else { 8 };
        let width = (js_round(screen_box.right) as i64 - left).max(min_w) as u32;
        let height = (js_round(screen_box.bottom) as i64 - top).max(min_h) as u32;
        let z_index = if focused { 4 } else { 3 };

        let (logical_width, logical_height, hairline_px) = if badge_mode {
            (0.0, 0.0, 1)
        } else {
            let inner_w = (i64::from(width) - 2).max(1) as f64;
            let inner_h = (i64::from(height) - 2).max(1) as f64;
            let hairline = (1.0 / scale).ceil().max(1.0) as u32;
            (inner_w / scale, inner_h / scale, hairline)
        };

        ScreenPlacement::Visible(VisibleScreenPlacement {
            world_rect,
            left,
            top,
            width,
            height,
            scale,
            badge_mode,
            focused,
            z_index,
            logical_width,
            logical_height,
            hairline_px,
        })
    }

    /// Compute visible per-frame screen placements for every open Agent View in `order`.
    pub fn visible_screen_placements<S: AsRef<str>>(
        &self,
        host: &dyn AgentViewsHost,
        camera: &WallCamera,
        order: &[S],
    ) -> Vec<(String, VisibleScreenPlacement)> {
        let mut out = Vec::new();
        for id_ref in order {
            let id = id_ref.as_ref();
            if let ScreenPlacement::Visible(placement) =
                self.screen_placement_of(host, camera, id)
            {
                out.push((id.to_string(), placement));
            }
        }
        out
    }

    // ── Canvas footprint shoving & give-room-back (`TerminalWall.footprintChanged`) ──

    /// Reconcile canvas layout when terminal `id`'s Agent View opened, closed, moved
    /// side, or changed configured dimensions (`TerminalWall.footprintChanged`).
    ///
    /// - Re-layouts `id`'s group when `id` belongs to a tiled or split group.
    /// - Restores any canvas terminals/groups previously shoved aside by `id` that
    ///   have not been moved since and whose original spots are now clear.
    /// - Shoves aside any loose terminals or other groups covered by the newly
    ///   claimed view (or widened group), snapping when `snap` is enabled, and
    ///   remembers their post-shove anchors for future restoration.
    pub fn footprint_changed<C: FootprintCanvas>(&mut self, canvas: &mut C, id: &str) {
        let zoom = canvas.zoom();
        let own = canvas.group_of(id);
        let laid = own
            .as_deref()
            .and_then(|gid| canvas.group_stand(gid))
            .is_some_and(|g| matches!(g.layout, GroupLayout::Tile | GroupLayout::Split));
        let on_canvas = canvas.terminal(id).is_some_and(|t| t.on_canvas);

        let before = if laid && on_canvas {
            own.as_deref()
                .and_then(|gid| canvas.group_box_rect(self, gid, zoom, Some(id)))
        } else {
            None
        };
        if laid {
            if let Some(ref gid) = own {
                canvas.relayout_group(self, gid);
            }
        }

        let on_canvas_now = canvas.terminal(id).is_some_and(|t| t.on_canvas);
        let side = self.side_of(canvas, id);
        let room = if !on_canvas_now {
            None
        } else if laid {
            own.as_deref()
                .and_then(|gid| canvas.group_box_rect(self, gid, zoom, None))
        } else {
            self.canvas_footprint_rect_on(canvas, id, zoom)
        };

        self.give_room_back(canvas, id, room);

        let (Some(side), Some(room_rect)) = (side, room) else {
            return;
        };
        let Some(claim) = (if laid {
            Some(room_rect)
        } else {
            self.rect(canvas, id, zoom)
        }) else {
            return;
        };
        let dir = if !laid {
            side_to_shove_dir(side)
        } else if matches!(side, AgentSide::Left | AgentSide::Right) {
            ShoveDir::Right
        } else {
            ShoveDir::Bottom
        };

        #[derive(Clone)]
        struct CanvasUnit {
            key: String,
            id: Option<String>,
            gid: Option<String>,
            anchor: String,
            rect: Option<Rect>,
        }

        let mut units: Vec<CanvasUnit> = Vec::new();
        let mut seen_keys: HashSet<String> = HashSet::new();
        for t in canvas.terminal_order() {
            if t == id || !canvas.terminal(&t).is_some_and(|term| term.on_canvas) {
                continue;
            }
            let gid = canvas.group_of(&t);
            if laid && gid == own {
                continue;
            }
            if let Some(g) = gid.filter(|g| Some(g.as_str()) != own.as_deref()) {
                let key = format!("g:{g}");
                if seen_keys.insert(key.clone()) {
                    let rect = canvas.group_box_rect(self, &g, zoom, None);
                    units.push(CanvasUnit {
                        key,
                        id: None,
                        gid: Some(g),
                        anchor: t,
                        rect,
                    });
                }
            } else {
                let key = format!("t:{t}");
                if seen_keys.insert(key.clone()) {
                    let rect = self.canvas_footprint_rect_on(canvas, &t, zoom);
                    units.push(CanvasUnit {
                        key,
                        id: Some(t.clone()),
                        gid: None,
                        anchor: t,
                        rect,
                    });
                }
            }
        }

        let items: Vec<ShoveItem> = units
            .iter()
            .filter_map(|u| {
                let r = u.rect?;
                if before.is_some_and(|b| overlaps(r, b)) {
                    return None;
                }
                Some(ShoveItem::new(u.key.clone(), r))
            })
            .collect();

        let cfg = canvas.agent_views_config();
        let step = if cfg.snap {
            snap_step(cfg.grid, zoom)
        } else {
            0.0
        };
        let Ok(moved_map) = shove_aside(claim, dir, &items, cfg.group_gap, step) else {
            return;
        };

        let mut shoves = Vec::new();
        for u in &units {
            let Some(delta) = moved_map.get(&u.key) else {
                continue;
            };
            if let Some(ref gid) = u.gid {
                canvas.move_group(gid, delta.dx, delta.dy);
                self.emit_effect(AgentViewsEffect::MoveGroup {
                    group_id: gid.clone(),
                    dx: delta.dx,
                    dy: delta.dy,
                });
            } else if let Some(ref tid) = u.id {
                canvas.move_terminal(tid, delta.dx, delta.dy);
                self.emit_effect(AgentViewsEffect::MoveTerminal {
                    id: tid.clone(),
                    dx: delta.dx,
                    dy: delta.dy,
                });
            }
            if let Some(anchor_term) = canvas.terminal(&u.anchor) {
                shoves.push(ShovedUnit {
                    id: u.id.clone(),
                    gid: u.gid.clone(),
                    anchor: u.anchor.clone(),
                    dx: delta.dx,
                    dy: delta.dy,
                    at: anchor_term.position,
                });
            }
        }

        if !shoves.is_empty() {
            let at = Self::stand_of(canvas, id);
            self.shoves.insert(id.to_string(), ShoveMemory { at, shoves });
        }
    }

    fn canvas_footprint_rect_on<C: FootprintCanvas>(
        &self,
        canvas: &C,
        id: &str,
        zoom: f64,
    ) -> Option<Rect> {
        let win = canvas.canvas_window_rect(id, zoom)?;
        match self.rect(canvas, id, zoom) {
            Some(ag) => union_rect(&[win, ag]),
            None => Some(win),
        }
    }

    fn stand_of<C: FootprintCanvas>(canvas: &C, id: &str) -> Option<Point> {
        if let Some(gid) = canvas.group_of(id) {
            if let Some(stand) = canvas.group_stand(&gid) {
                if stand.layout != GroupLayout::Freeform {
                    if let Some(origin) = stand.origin {
                        return Some(origin);
                    }
                }
            }
        }
        canvas.terminal(id).map(|t| t.position)
    }

    fn give_room_back<C: FootprintCanvas>(
        &mut self,
        canvas: &mut C,
        id: &str,
        room: Option<Rect>,
    ) {
        let Some(rec) = self.shoves.remove(id) else {
            return;
        };
        let at = Self::stand_of(canvas, id);
        let (Some(at_now), Some(at_then)) = (at, rec.at) else {
            return;
        };
        if (at_now.x - at_then.x).abs() > POS_EPS || (at_now.y - at_then.y).abs() > POS_EPS {
            return;
        }
        let zoom = canvas.zoom();
        for s in rec.shoves {
            let Some(anchor_now) = canvas.terminal(&s.anchor).map(|t| t.position) else {
                continue;
            };
            if (anchor_now.x - s.at.x).abs() > POS_EPS || (anchor_now.y - s.at.y).abs() > POS_EPS {
                continue;
            }
            if let Some(ref gid) = s.gid {
                let still_member = canvas
                    .group_stand(gid)
                    .is_some_and(|g| g.members.iter().any(|m| m == &s.anchor));
                if !still_member {
                    continue;
                }
            }
            let current_rect = if let Some(ref gid) = s.gid {
                canvas.group_box_rect(self, gid, zoom, None)
            } else if let Some(ref tid) = s.id {
                self.canvas_footprint_rect_on(canvas, tid, zoom)
            } else {
                None
            };
            let back = current_rect.map(|r| Rect {
                min_x: r.min_x - s.dx,
                max_x: r.max_x - s.dx,
                min_y: r.min_y - s.dy,
                max_y: r.max_y - s.dy,
            });
            if let (Some(room_rect), Some(back_rect)) = (room, back) {
                if overlaps(back_rect, room_rect) {
                    continue;
                }
            }
            if let Some(gid) = s.gid {
                canvas.move_group(&gid, -s.dx, -s.dy);
                self.emit_effect(AgentViewsEffect::MoveGroup {
                    group_id: gid,
                    dx: -s.dx,
                    dy: -s.dy,
                });
            } else if let Some(tid) = s.id {
                canvas.move_terminal(&tid, -s.dx, -s.dy);
                self.emit_effect(AgentViewsEffect::MoveTerminal {
                    id: tid,
                    dx: -s.dx,
                    dy: -s.dy,
                });
            }
        }
    }

    // ── Stream binding, anchor stamping & GC internals ───────────────────────

    fn term_meta(&self, host: &dyn AgentViewsHost, id: &str) -> TerminalSessionMeta {
        let sync = self.term_sync.get(id);
        let grid = host.terminal(id);
        let attached = host.attached_target(id).unwrap_or_default();
        let bus_lookup = if !attached.is_empty() {
            Some(attached.as_str())
        } else {
            id.strip_prefix("gx-").filter(|s| !s.is_empty())
        };
        let bus_agent = bus_lookup.and_then(|target| host.bus_agent(target));
        let pwd = sync.map(|s| s.pwd.clone()).unwrap_or_default();
        let cwd = sync
            .map(|s| s.cwd.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| bus_agent.as_ref().and_then(|b| b.cwd.clone()))
            .unwrap_or_default();
        let title = host
            .store_name(id)
            .filter(|s| !s.is_empty())
            .or_else(|| sync.map(|s| s.title.clone()).filter(|s| !s.is_empty()))
            .or_else(|| grid.map(|g| g.window_title).filter(|s| !s.is_empty()))
            .unwrap_or_default();
        let session_id = bus_agent
            .and_then(|b| b.session_id)
            .unwrap_or_default();

        TerminalSessionMeta {
            id: Some(id.to_string()),
            attached: (!attached.is_empty()).then_some(attached),
            session_id: (!session_id.is_empty()).then_some(session_id),
            pwd: (!pwd.is_empty()).then_some(pwd),
            cwd: (!cwd.is_empty()).then_some(cwd),
            title: (!title.is_empty()).then_some(title),
        }
    }

    fn rebind_all(&mut self, host: &dyn AgentViewsHost, restart_follows: bool) {
        let mut ids: Vec<String> = self.views.keys().cloned().collect();
        ids.sort();
        for id in ids {
            if host.terminal(&id).is_some() {
                self.rebind_one(host, &id, restart_follows);
            }
        }
        self.gc_streams(host);
    }

    fn rebind_one(&mut self, host: &dyn AgentViewsHost, id: &str, restart_follows: bool) {
        let Some(is_open) = self.views.get(id).map(|v| v.open) else {
            return;
        };
        if !is_open {
            if let Some(v) = self.views.get_mut(id) {
                v.resolved_session_id = None;
            }
            return;
        }

        let binding = self.views[id].session_binding.clone();
        let resolved = match binding {
            SessionBinding::Auto => {
                let meta = self.term_meta(host, id);
                resolve_terminal_session(&meta, &self.sessions)
            }
            SessionBinding::Pinned(ref chosen_sid) => {
                let mut pinned_meta = self.term_meta(host, id);
                pinned_meta.session_id = Some(chosen_sid.clone());
                let candidate = resolve_terminal_session(&pinned_meta, &self.sessions);
                if candidate
                    .session
                    .as_ref()
                    .is_some_and(|s| s.session_id == *chosen_sid)
                {
                    candidate
                } else {
                    crate::agent_view::ResolvedSession {
                        session: None,
                        via: None,
                        missing: Some(format!("pinned session '{chosen_sid}' is not listed")),
                    }
                }
            }
        };

        let next_sid = resolved.session.as_ref().map(|s| s.session_id.clone());
        let matched = resolved.session;
        let feed_ready = self.feed_ready;
        {
            let v = self.views.get_mut(id).expect("checked above");
            v.resolved_via = resolved.via;
            v.missing = if matched.is_some() {
                None
            } else if feed_ready {
                resolved.missing
            } else {
                None
            };
            if next_sid != v.resolved_session_id {
                v.resolved_session_id = next_sid.clone();
                v.anchors.clear();
                v.jumped_event_id = None;
                v.stick_bottom = true;
                v.unread_count = 0;
            }
        }

        if let (true, Some(sid)) = (feed_ready, next_sid) {
            self.ensure_stream(&sid, matched.as_ref(), restart_follows);
            self.seed_hydrated_anchors(host, &sid);
        }
    }

    fn gc_streams(&mut self, host: &dyn AgentViewsHost) {
        let mut wanted = HashSet::new();
        for (id, v) in &self.views {
            if v.open && host.terminal(id).is_some() {
                if let Some(ref sid) = v.resolved_session_id {
                    wanted.insert(sid.clone());
                }
            }
        }
        let mut stale: Vec<String> = self
            .streams
            .keys()
            .filter(|sid| !wanted.contains(*sid))
            .cloned()
            .collect();
        stale.sort();
        for sid in stale {
            if let Some(mut stream) = self.streams.remove(&sid) {
                stream.generation += 1;
                if stream.following {
                    self.emit_effect(AgentViewsEffect::UnfollowSession {
                        session_id: sid,
                    });
                }
            }
        }
    }

    fn ensure_stream(
        &mut self,
        session_id: &str,
        session_info: Option<&AgentSession>,
        restart_follows: bool,
    ) {
        if !self.streams.contains_key(session_id) {
            let mut stream = SessionStream::new();
            stream.loading = true;
            stream.error = None;
            self.streams.insert(session_id.to_string(), stream);
            self.emit_effect(AgentViewsEffect::ReadSession {
                session_id: session_id.to_string(),
                until: None,
                tail_bytes: STREAM_WINDOW_BYTES,
            });
            return;
        }
        let should_follow = session_info.is_none_or(|s| s.status != "ended");
        let (is_loading, is_following) = {
            let st = &self.streams[session_id];
            (st.loading, st.following)
        };
        if restart_follows && !is_loading {
            if let Some(st) = self.streams.get_mut(session_id) {
                st.following = false;
            }
            if should_follow {
                self.start_follow(session_id);
            }
        } else if !is_following && !is_loading && should_follow {
            self.start_follow(session_id);
        }
    }

    fn start_follow(&mut self, session_id: &str) {
        let from_byte = {
            let Some(stream) = self.streams.get_mut(session_id) else {
                return;
            };
            if stream.following {
                return;
            }
            // Per EVENTS.md: drop uncommitted provisional replies before resuming a follow from `last_at`.
            let entries_ref = &mut stream.entries;
            stream.order.retain(|eid| {
                let drop_it = entries_ref
                    .get(eid)
                    .is_some_and(|item| item.frame.is_provisional());
                if drop_it {
                    entries_ref.remove(eid);
                }
                !drop_it
            });
            stream.live_reader = AgentEventReader::new(false);
            stream.following = true;
            stream.last_at
        };
        self.emit_effect(AgentViewsEffect::FollowSession {
            session_id: session_id.to_string(),
            from_byte,
        });
    }

    fn stamp_live_add(
        &mut self,
        host: &dyn AgentViewsHost,
        session_id: &str,
        ev: &AgentEventFrame,
    ) {
        let proto = ev.protocol_terminal_line;
        let matching_terms: Vec<String> = self
            .views
            .iter()
            .filter(|(_, v)| v.open && v.resolved_session_id.as_deref() == Some(session_id))
            .map(|(tid, _)| tid.clone())
            .collect();
        for term_id in matching_terms {
            let anchor = if let Some(line) = proto {
                LineAnchor {
                    terminal_line: Some(line as f64),
                    scrollback_rows: None,
                    cursor_row: None,
                    exact: Some(true),
                }
            } else {
                let sb_now = self.scrollback_now(host, &term_id);
                let view_off = self.view_offset_of(&term_id);
                let cursor_row = if view_off > 0 {
                    0
                } else {
                    host.terminal(&term_id).map_or(0, |g| g.cursor_row)
                };
                LineAnchor {
                    terminal_line: None,
                    scrollback_rows: Some(sb_now as f64),
                    cursor_row: Some(f64::from(cursor_row)),
                    exact: Some(true),
                }
            };
            if let Some(v) = self.views.get_mut(&term_id) {
                v.anchors.insert(ev.id.clone(), anchor);
            }
        }
    }

    fn bump_unread_on_live_add(
        &mut self,
        host: &dyn AgentViewsHost,
        session_id: &str,
        ev: &AgentEventFrame,
    ) {
        for (term_id, v) in &mut self.views {
            if !v.open
                || v.stick_bottom
                || v.resolved_session_id.as_deref() != Some(session_id)
                || host.terminal(term_id).is_none()
            {
                continue;
            }
            let show_noise = v.filters.get("noise").copied().unwrap_or(false);
            if let Some(rec) = ev.record_at(None, show_noise) {
                if matches_filter(&rec, &v.filters) {
                    v.unread_count += 1;
                }
            }
        }
    }

    fn stamp_protocol_line(&mut self, session_id: &str, event_id: &str, terminal_line: u64) {
        for v in self.views.values_mut() {
            if v.open && v.resolved_session_id.as_deref() == Some(session_id) {
                v.anchors.insert(
                    event_id.to_string(),
                    LineAnchor {
                        terminal_line: Some(terminal_line as f64),
                        scrollback_rows: None,
                        cursor_row: None,
                        exact: Some(true),
                    },
                );
            }
        }
    }

    fn seed_hydrated_anchors(&mut self, host: &dyn AgentViewsHost, session_id: &str) {
        if !self
            .streams
            .get(session_id)
            .is_some_and(|st| !st.order.is_empty())
        {
            return;
        }
        let matching_terms: Vec<(String, u64)> = self
            .views
            .iter()
            .filter(|(_, v)| v.open && v.resolved_session_id.as_deref() == Some(session_id))
            .map(|(tid, _)| (tid.clone(), self.scrollback_now(host, tid)))
            .collect();

        let Some(stream) = self.streams.get(session_id) else {
            return;
        };
        for (term_id, now_sb) in matching_terms {
            let Some(v) = self.views.get_mut(&term_id) else {
                continue;
            };
            for eid in &stream.order {
                if v.anchors.contains_key(eid) {
                    continue;
                }
                let Some(entry) = stream.entries.get(eid) else {
                    continue;
                };
                if let Some(proto) = entry.frame.protocol_terminal_line {
                    v.anchors.insert(
                        eid.clone(),
                        LineAnchor {
                            terminal_line: Some(proto as f64),
                            scrollback_rows: None,
                            cursor_row: None,
                            exact: Some(true),
                        },
                    );
                } else if now_sb > 0 {
                    if let Some(approx) = Self::approximate_anchor(stream, entry, now_sb) {
                        v.anchors.insert(eid.clone(), approx);
                    }
                }
            }
        }
    }

    fn approximate_anchor(
        stream: &SessionStream,
        entry: &StreamEntry,
        now_scrollback: u64,
    ) -> Option<LineAnchor> {
        if now_scrollback == 0 {
            return None;
        }
        let at = entry.pos.map_or(entry.frame.at, |p| p.at).max(0) as f64;
        let max_at = (stream.transcript_size.unwrap_or(0) as f64)
            .max(stream.last_at.unwrap_or(0).max(0) as f64)
            .max(at)
            .max(1.0);
        let frac = (at / max_at).clamp(0.0, 1.0);
        Some(LineAnchor {
            terminal_line: Some(js_round(frac * (now_scrollback as f64))),
            scrollback_rows: None,
            cursor_row: None,
            exact: Some(false),
        })
    }

    fn visible_records_for(&self, id: &str) -> Vec<EventRecord> {
        let Some(v) = self.views.get(id) else {
            return Vec::new();
        };
        let Some(stream) = v
            .resolved_session_id
            .as_deref()
            .and_then(|sid| self.streams.get(sid))
        else {
            return Vec::new();
        };
        let show_noise = v.filters.get("noise").copied().unwrap_or(false);
        let mut out = Vec::new();
        for eid in &stream.order {
            let Some(item) = stream.entries.get(eid) else {
                continue;
            };
            let Some(rec) = item.frame.record_at(item.pos, show_noise) else {
                continue;
            };
            if matches_filter(&rec, &v.filters) {
                out.push(rec);
            }
        }
        out
    }

    fn all_records_for(&self, id: &str) -> Vec<EventRecord> {
        let Some(v) = self.views.get(id) else {
            return Vec::new();
        };
        let Some(stream) = v
            .resolved_session_id
            .as_deref()
            .and_then(|sid| self.streams.get(sid))
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for eid in &stream.order {
            let Some(item) = stream.entries.get(eid) else {
                continue;
            };
            if let Some(rec) = item.frame.record_at(item.pos, true) {
                out.push(rec);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_view::AGENT_SIDES;
    use crate::config::WallConfig;
    use crate::groups::{
        CreateGroupOptions, GroupConfig, GroupLayout, Groups, SplitInsertDir, TerminalGeometry,
        WallHost,
    };
    use crate::input::{KeyEvent, Mods};
    use crate::keymap::{chord_for, ChordAction, KeyMode};
    use crate::model::MemoryStorage;
    use crate::split::SplitNode;

    fn make_event_json(
        id: &str,
        at: i64,
        kind: &str,
        body: serde_json::Value,
        extra: Option<serde_json::Value>,
    ) -> String {
        let mut obj = serde_json::Map::new();
        obj.insert("type".into(), serde_json::json!("event"));
        obj.insert("op".into(), serde_json::json!("add"));
        obj.insert("id".into(), serde_json::json!(id));
        obj.insert("seq".into(), serde_json::json!(1));
        obj.insert("at".into(), serde_json::json!(at));
        obj.insert("line".into(), serde_json::json!(1));
        obj.insert("ts".into(), serde_json::json!("2026-11-14T22:00:00Z"));
        obj.insert(
            "turn".into(),
            serde_json::json!({ "id": "turn-1", "index": 1 }),
        );
        obj.insert("kind".into(), serde_json::json!(kind));
        obj.insert(kind.into(), body);
        if let Some(serde_json::Value::Object(ex)) = extra {
            for (k, v) in ex {
                obj.insert(k, v);
            }
        }
        serde_json::Value::Object(obj).to_string()
    }

    // ── Stand-in headless wall + groups + agent views harness ────────────────

    #[derive(Clone, Debug)]
    struct TestTerm {
        position: Point,
        cols: u32,
        rows: u32,
        stride: Point,
        fixed_size: Option<(f64, f64)>,
        scrolled_rows: u64,
        cursor_row: u32,
        alt_screen: bool,
        window_title: String,
        on_canvas: bool,
        bar_room: Option<f64>,
    }

    impl TestTerm {
        fn fixed(w: f64, h: f64, x: f64, y: f64) -> Self {
            TestTerm {
                position: Point::new(x, y),
                cols: 80,
                rows: 24,
                stride: Point::new(1.0, 1.0),
                fixed_size: Some((w, h)),
                scrolled_rows: 0,
                cursor_row: 0,
                alt_screen: false,
                window_title: String::new(),
                on_canvas: true,
                bar_room: None,
            }
        }

        fn grid(cols: u32, rows: u32, stride_x: f64, stride_y: f64, x: f64, y: f64) -> Self {
            TestTerm {
                position: Point::new(x, y),
                cols,
                rows,
                stride: Point::new(stride_x, stride_y),
                fixed_size: None,
                scrolled_rows: 0,
                cursor_row: 0,
                alt_screen: false,
                window_title: String::new(),
                on_canvas: true,
                bar_room: None,
            }
        }

        fn body_rect(&self) -> Rect {
            let (w, h) = match self.fixed_size {
                Some((w, h)) => (w, h),
                None => (
                    f64::from(self.cols) * self.stride.x,
                    f64::from(self.rows) * self.stride.y,
                ),
            };
            Rect::from_top_left(self.position.x, self.position.y, w, h)
        }
    }

    struct HeadlessCanvas {
        terminals: HashMap<String, TestTerm>,
        order: Vec<String>,
        groups: Groups,
        camera: WallCamera,
        av_config: AgentViewsConfig,
        group_config: GroupConfig,
        focused_id: Option<String>,
        fullscreen_active: bool,
    }

    impl HeadlessCanvas {
        fn new_fixed(specs: &[(&str, f64, f64, f64, f64)]) -> Self {
            let wall_cfg = WallConfig::default();
            let mut terminals = HashMap::new();
            let mut order = Vec::new();
            for &(id, w, h, x, y) in specs {
                terminals.insert(id.to_string(), TestTerm::fixed(w, h, x, y));
                order.push(id.to_string());
            }
            HeadlessCanvas {
                terminals,
                order,
                groups: Groups::new(),
                camera: WallCamera::new(CameraPose::new(0.0, 0.0, 4.0), 1280.0, 800.0),
                av_config: AgentViewsConfig {
                    default_side: wall_cfg.agent_view_side,
                    dims: AgentViewDims::new(
                        wall_cfg.agent_view_width,
                        wall_cfg.agent_view_height,
                        wall_cfg.agent_view_gap,
                    ),
                    group_gap: wall_cfg.group_gap,
                    snap: wall_cfg.snap,
                    grid: GridSpec::new(
                        wall_cfg.grid_minor,
                        wall_cfg.grid_major_every as u32,
                        wall_cfg.grid_min_px,
                    ),
                },
                group_config: GroupConfig::default(),
                focused_id: None,
                fullscreen_active: false,
            }
        }

        fn new_split_cells(specs: &[(&str, u32, u32)], sx: f64, sy: f64) -> Self {
            let mut this = Self::new_fixed(&[]);
            let mut cursor_x = 0.0;
            for &(id, cols, rows) in specs {
                this.terminals.insert(
                    id.to_string(),
                    TestTerm::grid(cols, rows, sx, sy, cursor_x, 0.0),
                );
                this.order.push(id.to_string());
                cursor_x += f64::from(cols) * sx + 12.0;
            }
            this
        }

        fn at(&self, id: &str) -> Point {
            self.terminals[id].position
        }

        fn window_rect(&self, id: &str, zoom: f64) -> Rect {
            let t = &self.terminals[id];
            let bar = self.bar_height(id, zoom);
            let b = t.body_rect();
            Rect {
                max_y: b.max_y + bar,
                ..b
            }
        }

        fn footprint_rect(&self, views: &AgentViews, id: &str, zoom: f64) -> Rect {
            views
                .canvas_footprint_rect(self, id, zoom)
                .expect("terminal exists")
        }
    }

    impl AgentViewsHost for HeadlessCanvas {
        fn agent_views_config(&self) -> AgentViewsConfig {
            self.av_config
        }

        fn terminal(&self, id: &str) -> Option<AgentTerminalSnapshot> {
            let t = self.terminals.get(id)?;
            Some(AgentTerminalSnapshot {
                body_rect: t.body_rect(),
                position: t.position,
                scrolled_rows: t.scrolled_rows,
                cursor_row: t.cursor_row,
                alt_screen: t.alt_screen,
                window_title: t.window_title.clone(),
                on_canvas: t.on_canvas,
            })
        }

        fn bar_height(&self, id: &str, _zoom: f64) -> f64 {
            self.terminals
                .get(id)
                .and_then(|t| t.bar_room)
                .unwrap_or(BAR_ROOM)
        }

        fn focused_id(&self) -> Option<&str> {
            self.focused_id.as_deref()
        }

        fn fullscreen_active(&self) -> bool {
            self.fullscreen_active
        }

        fn zoom(&self) -> f64 {
            self.camera.zoom()
        }
    }

    /// Borrow-splitting adapter connecting `(HeadlessCanvas, &AgentViews)` to `groups::WallHost`.
    struct GroupsBridge<'a> {
        terminals: &'a mut HashMap<String, TestTerm>,
        order: &'a [String],
        av_config: AgentViewsConfig,
        group_config: GroupConfig,
        views: &'a AgentViews,
    }

    impl WallHost for GroupsBridge<'_> {
        fn group_config(&self) -> GroupConfig {
            self.group_config
        }

        fn terminal(&self, id: &str) -> Option<TerminalGeometry> {
            let t = self.terminals.get(id)?;
            Some(TerminalGeometry {
                position: t.position,
                rect: t.body_rect(),
                cols: t.cols,
                rows: t.rows,
                cell_stride: t.stride,
            })
        }

        fn terminal_ids(&self) -> Vec<String> {
            self.order.to_vec()
        }

        fn is_shown(&self, id: &str) -> bool {
            self.terminals.get(id).is_some_and(|t| t.on_canvas)
        }

        fn place_body(&mut self, id: &str, pos: Point) {
            if let Some(t) = self.terminals.get_mut(id) {
                t.position = pos;
            }
        }

        fn resize_terminal(&mut self, id: &str, cols: u32, rows: u32) {
            if let Some(t) = self.terminals.get_mut(id) {
                t.cols = cols;
                t.rows = rows;
            }
        }

        fn set_bar_room(&mut self, id: &str, room: Option<f64>) {
            if let Some(t) = self.terminals.get_mut(id) {
                t.bar_room = room;
            }
        }

        fn agent_inset(&self, id: &str) -> PaneInset {
            self.views.pane_inset_with_dims(id, self.av_config.dims)
        }

        fn agent_tile_metrics(&self, id: &str) -> Option<TileMetrics> {
            let t = self.terminals.get(id)?;
            Some(
                self.views
                    .tile_metrics_for_body(id, t.body_rect(), self.av_config.dims),
            )
        }

        fn canvas_window_rect(&self, id: &str, _zoom: f64) -> Option<Rect> {
            let t = self.terminals.get(id)?;
            let bar = t.bar_room.unwrap_or(BAR_ROOM);
            let b = t.body_rect();
            Some(Rect {
                max_y: b.max_y + bar,
                ..b
            })
        }

        fn canvas_footprint_rect(&self, id: &str, zoom: f64) -> Option<Rect> {
            let t = self.terminals.get(id)?;
            let bar = t.bar_room.unwrap_or(BAR_ROOM);
            let body = t.body_rect();
            let win = self.canvas_window_rect(id, zoom)?;
            Some(
                self.views
                    .footprint_rect_from_parts(id, body, win, bar, self.av_config.dims),
            )
        }
    }

    /// Read-only `WallHost` bridge for `frame_rect` / `box_rect` / `pane_rects` queries.
    struct ReadGroupsBridge<'a> {
        canvas: &'a HeadlessCanvas,
        views: &'a AgentViews,
    }

    impl WallHost for ReadGroupsBridge<'_> {
        fn group_config(&self) -> GroupConfig {
            self.canvas.group_config
        }

        fn terminal(&self, id: &str) -> Option<TerminalGeometry> {
            let t = self.canvas.terminals.get(id)?;
            Some(TerminalGeometry {
                position: t.position,
                rect: t.body_rect(),
                cols: t.cols,
                rows: t.rows,
                cell_stride: t.stride,
            })
        }

        fn terminal_ids(&self) -> Vec<String> {
            self.canvas.order.clone()
        }

        fn is_shown(&self, id: &str) -> bool {
            self.canvas.terminals.get(id).is_some_and(|t| t.on_canvas)
        }

        fn place_body(&mut self, _id: &str, _pos: Point) {}

        fn agent_inset(&self, id: &str) -> PaneInset {
            self.views
                .pane_inset_with_dims(id, self.canvas.av_config.dims)
        }

        fn agent_tile_metrics(&self, id: &str) -> Option<TileMetrics> {
            let t = self.canvas.terminals.get(id)?;
            Some(
                self.views
                    .tile_metrics_for_body(id, t.body_rect(), self.canvas.av_config.dims),
            )
        }

        fn canvas_window_rect(&self, id: &str, zoom: f64) -> Option<Rect> {
            Some(self.canvas.window_rect(id, zoom))
        }

        fn canvas_footprint_rect(&self, id: &str, zoom: f64) -> Option<Rect> {
            self.views.canvas_footprint_rect(self.canvas, id, zoom)
        }
    }

    impl FootprintCanvas for HeadlessCanvas {
        fn terminal_order(&self) -> Vec<String> {
            self.order.clone()
        }

        fn group_of(&self, id: &str) -> Option<String> {
            self.groups.group_of(id)
        }

        fn group_stand(&self, gid: &str) -> Option<GroupStand> {
            let g = self.groups.group(gid)?;
            Some(GroupStand {
                layout: g.layout,
                origin: g.origin,
                members: g.members.clone(),
            })
        }

        fn relayout_group(&mut self, views: &AgentViews, gid: &str) {
            let mut bridge = GroupsBridge {
                terminals: &mut self.terminals,
                order: &self.order,
                av_config: self.av_config,
                group_config: self.group_config,
                views,
            };
            let _ = self.groups.relayout(&mut bridge, gid);
        }

        fn group_box_rect(
            &self,
            views: &AgentViews,
            gid: &str,
            zoom: f64,
            bare: Option<&str>,
        ) -> Option<Rect> {
            let bridge = ReadGroupsBridge {
                canvas: self,
                views,
            };
            self.groups.box_rect(&bridge, gid, zoom, bare).ok().flatten()
        }

        fn move_group(&mut self, gid: &str, dx: f64, dy: f64) {
            let empty_views = AgentViews::new();
            let mut bridge = GroupsBridge {
                terminals: &mut self.terminals,
                order: &self.order,
                av_config: self.av_config,
                group_config: self.group_config,
                views: &empty_views,
            };
            let _ = self.groups.move_group(&mut bridge, gid, dx, dy);
        }

        fn move_terminal(&mut self, id: &str, dx: f64, dy: f64) {
            if let Some(t) = self.terminals.get_mut(id) {
                t.position.x += dx;
                t.position.y += dy;
            }
        }
    }

    impl HeadlessCanvas {
        fn create_group(&mut self, views: &AgentViews, ids: &[&str], opts: CreateGroupOptions) -> String {
            let mut bridge = GroupsBridge {
                terminals: &mut self.terminals,
                order: &self.order,
                av_config: self.av_config,
                group_config: self.group_config,
                views,
            };
            self.groups.create(&mut bridge, ids, opts).unwrap()
        }

        fn insert_pane(&mut self, views: &AgentViews, at: &str, id: &str, dir: SplitInsertDir) -> String {
            let mut bridge = GroupsBridge {
                terminals: &mut self.terminals,
                order: &self.order,
                av_config: self.av_config,
                group_config: self.group_config,
                views,
            };
            self.groups.insert_pane(&mut bridge, at, id, dir).unwrap()
        }

        fn frame_rect(&self, views: &AgentViews, gid: &str, zoom: f64) -> Rect {
            let bridge = ReadGroupsBridge {
                canvas: self,
                views,
            };
            self.groups
                .frame_rect(&bridge, gid, zoom, None)
                .unwrap()
                .unwrap()
        }

        fn box_rect(&self, views: &AgentViews, gid: &str, zoom: f64) -> Rect {
            let bridge = ReadGroupsBridge {
                canvas: self,
                views,
            };
            self.groups
                .box_rect(&bridge, gid, zoom, None)
                .unwrap()
                .unwrap()
        }

        fn pane_rects(&self, views: &AgentViews, gid: &str) -> crate::split::PaneMap {
            let bridge = ReadGroupsBridge {
                canvas: self,
                views,
            };
            self.groups.pane_rects(&bridge, gid).unwrap()
        }
    }

    // ── Ported from tools/wall-agent-view.test.mjs ───────────────────────────

    #[test]
    fn tiled_group_with_four_agent_views_on_all_four_sides_never_overlaps() {
        let mut canvas = HeadlessCanvas::new_fixed(&[
            ("a", 100.0, 50.0, 0.0, 0.0),
            ("b", 100.0, 50.0, 200.0, 0.0),
            ("c", 100.0, 50.0, 0.0, -120.0),
            ("d", 100.0, 50.0, 200.0, -120.0),
        ]);
        let mut views = AgentViews::new();

        views
            .set_side_on_canvas(&mut canvas, None, "a", "right")
            .unwrap();
        views
            .set_side_on_canvas(&mut canvas, None, "b", "left")
            .unwrap();
        views
            .set_side_on_canvas(&mut canvas, None, "c", "top")
            .unwrap();
        views
            .set_side_on_canvas(&mut canvas, None, "d", "bottom")
            .unwrap();

        let gid = canvas.create_group(
            &views,
            &["a", "b", "c", "d"],
            CreateGroupOptions {
                layout: Some(GroupLayout::Tile),
                columns: Some(2),
                gap: Some(4.0),
                ..CreateGroupOptions::default()
            },
        );

        let footprints: Vec<Rect> = ["a", "b", "c", "d"]
            .iter()
            .map(|id| canvas.footprint_rect(&views, id, 4.0))
            .collect();
        for i in 0..footprints.len() {
            for j in (i + 1)..footprints.len() {
                assert!(
                    !overlaps(footprints[i], footprints[j]),
                    "footprints {i} and {j} overlapped: {:?} vs {:?}",
                    footprints[i],
                    footprints[j]
                );
            }
        }

        // Group backdrop encloses every terminal window AND every attached Agent View.
        let frame = canvas.frame_rect(&views, &gid, 4.0);
        let all_union = union_rect(&footprints).unwrap();
        let pad = canvas.group_config.group_pad;
        assert!(frame.min_x <= all_union.min_x - pad + 1e-6);
        assert!(frame.max_x >= all_union.max_x + pad - 1e-6);
        assert!(frame.min_y <= all_union.min_y - pad + 1e-6);
        assert!(frame.max_y >= all_union.max_y + pad - 1e-6);
    }

    #[test]
    fn ungrouped_agent_view_shoves_covered_items_and_gives_room_back_unless_moved() {
        let mut canvas = HeadlessCanvas::new_fixed(&[
            ("a", 100.0, 50.0, 0.0, 0.0),
            ("b", 60.0, 40.0, 120.0, 0.0),
            ("c", 40.0, 20.0, 150.0, -20.0),
            ("d", 40.0, 20.0, 195.0, -20.0),
            ("e", 60.0, 40.0, 0.0, -200.0),
        ]);
        canvas.av_config.snap = false;
        let mut views = AgentViews::new();

        let gid = canvas.create_group(
            &views,
            &["c", "d"],
            CreateGroupOptions {
                layout: Some(GroupLayout::Freeform),
                ..CreateGroupOptions::default()
            },
        );
        let start_a = canvas.at("a");
        let start_b = canvas.at("b");
        let start_c = canvas.at("c");
        let start_d = canvas.at("d");
        let start_e = canvas.at("e");

        views
            .set_open_on_canvas(&mut canvas, None, "a", "open", Some("right"))
            .unwrap();
        let zoom = canvas.zoom();
        let view = views.rect(&canvas, "a", zoom).unwrap();
        assert_eq!(canvas.at("a"), start_a);
        assert_eq!(canvas.at("e"), start_e);
        for (id, start_pos) in [("b", start_b), ("c", start_c), ("d", start_d)] {
            assert!(!overlaps(view, canvas.footprint_rect(&views, id, zoom)));
            assert_eq!(canvas.at(id).y, start_pos.y);
            assert!(canvas.at(id).x > start_pos.x);
        }
        // The freeform group {c, d} moved as one rigid unit.
        assert!(
            ((canvas.at("d").x - canvas.at("c").x) - (start_d.x - start_c.x)).abs() < 1e-9
        );
        assert!(!overlaps(
            canvas.footprint_rect(&views, "b", zoom),
            canvas.frame_rect(&views, &gid, zoom)
        ));

        // Moving the view to the bottom gives the right-hand room back first.
        views
            .set_side_on_canvas(&mut canvas, None, "a", "bottom")
            .unwrap();
        assert_eq!(canvas.at("b"), start_b);
        assert_eq!(canvas.at("c"), start_c);
        assert_eq!(canvas.at("d"), start_d);
        let bottom_view = views.rect(&canvas, "a", zoom).unwrap();
        for id in ["b", "c", "d", "e"] {
            assert!(!overlaps(
                bottom_view,
                canvas.footprint_rect(&views, id, zoom)
            ));
        }

        // Closing gives back what it shoved; an item moved by the user stays where put.
        views
            .set_side_on_canvas(&mut canvas, None, "a", "right")
            .unwrap();
        let shoved_b = canvas.at("b");
        views
            .set_open_on_canvas(&mut canvas, None, "a", "close", None)
            .unwrap();
        assert_eq!(canvas.at("b"), start_b);
        assert_eq!(canvas.at("c"), start_c);
        assert_eq!(canvas.at("d"), start_d);

        views
            .set_open_on_canvas(&mut canvas, None, "a", "open", None)
            .unwrap();
        canvas.terminals.get_mut("b").unwrap().position.x += 7.0;
        let moved_b = canvas.at("b");
        views
            .set_open_on_canvas(&mut canvas, None, "a", "close", None)
            .unwrap();
        assert_eq!(canvas.at("b"), moved_b);
        assert!(shoved_b.x > start_b.x);
        assert_eq!(canvas.at("c"), start_c);
    }

    #[test]
    fn tiled_group_growing_for_agent_view_shoves_canvas_and_gives_room_back_on_close() {
        let mut canvas = HeadlessCanvas::new_fixed(&[
            ("a", 100.0, 50.0, 0.0, 0.0),
            ("b", 100.0, 50.0, 110.0, 0.0),
            ("c", 60.0, 40.0, 260.0, 0.0),
            ("far", 60.0, 40.0, 0.0, -300.0),
        ]);
        canvas.av_config.snap = false;
        let mut views = AgentViews::new();
        let gid = canvas.create_group(
            &views,
            &["a", "b"],
            CreateGroupOptions {
                layout: Some(GroupLayout::Tile),
                columns: Some(2),
                gap: Some(4.0),
                ..CreateGroupOptions::default()
            },
        );
        let zoom = canvas.zoom();
        assert!(!overlaps(
            canvas.box_rect(&views, &gid, zoom),
            canvas.footprint_rect(&views, "c", zoom)
        ));
        let c0 = canvas.at("c");
        let far0 = canvas.at("far");

        views
            .set_open_on_canvas(&mut canvas, None, "b", "right", None)
            .unwrap();
        let box_after = canvas.box_rect(&views, &gid, zoom);
        assert!(!overlaps(
            box_after,
            canvas.footprint_rect(&views, "c", zoom)
        ));
        assert!(canvas.at("c").x > c0.x);
        assert_eq!(canvas.at("c").y, c0.y);
        assert_eq!(canvas.at("far"), far0);

        views
            .set_open_on_canvas(&mut canvas, None, "b", "close", None)
            .unwrap();
        assert_eq!(canvas.at("c"), c0);
    }

    #[test]
    fn live_event_folding_scrollback_anchors_jumps_filters_reload_and_screen_transforms() {
        let mem = MemoryStorage::new();
        let mut store = WallStore::with_storage(Box::new(mem.clone()), "port-8199");
        let mut canvas = HeadlessCanvas::new_fixed(&[("t1", 80.0, 40.0, 0.0, 0.0)]);
        canvas.focused_id = Some("t1".to_string());
        let mut views = AgentViews::from_store(&store, canvas.av_config.default_side);

        let initial_lines = vec![
            serde_json::json!({
                "type": "session",
                "session_id": "sess-1",
                "harness": "claude",
                "cwd": "/srv/wall",
                "title": "Agent View Task"
            })
            .to_string(),
            make_event_json(
                "p1",
                50,
                "prompt",
                serde_json::json!({ "origin": "human", "text": "build agent view" }),
                None,
            ),
            // Provisional Stop-hook reply: last_at stays at 50 per EVENTS.md!
            make_event_json(
                "prov-1",
                90,
                "say",
                serde_json::json!({ "text": "working…", "provisional": true }),
                None,
            ),
        ];
        let earlier_lines = vec![make_event_json(
            "p0",
            10,
            "prompt",
            serde_json::json!({ "origin": "human", "text": "earlier turn" }),
            None,
        )];

        // Sync termd status so `t1` is in `/srv/wall` with 100 scrollback rows.
        views.sync_termd_status(
            &canvas,
            &[TermdTerminalStatus {
                id: "t1".to_string(),
                cwd: "/srv".to_string(),
                pwd: "/srv/wall".to_string(),
                scrollback_rows: 100,
                view: 0,
                alt: false,
                title: String::new(),
            }],
        );
        views.on_feed_up();
        views.apply_session_list(
            &canvas,
            vec![AgentSession {
                session_id: "sess-1".to_string(),
                harness: "claude".to_string(),
                cwd: Some("/srv/wall".to_string()),
                status: "processing".to_string(),
                mtime: "2026-11-14T22:30:00Z".to_string(),
                title: Some("Agent View Task".to_string()),
                ..AgentSession::default()
            }],
            true,
        );
        let _ = views.drain_effects();

        // Chord+A maps to ToggleAgentView on the canvas.
        let chord_a = chord_for(
            &KeyEvent {
                key: "a".to_string(),
                code: "KeyA".to_string(),
                mods: Mods {
                    meta: true,
                    ..Mods::default()
                },
                pressed: true,
                repeat: false,
                text: None,
            },
            KeyMode::Canvas,
        );
        assert_eq!(chord_a.and_then(|c| c.action), Some(ChordAction::ToggleAgentView));

        let open_res = views
            .set_open_on_canvas(&mut canvas, Some(&mut store), "t1", "toggle", None)
            .unwrap();
        assert_eq!(
            open_res,
            OpenOutcome {
                open: true,
                side: AgentSide::Right,
                session: Some("sess-1".to_string()),
                missing: None,
            }
        );

        let effects = views.drain_effects();
        assert!(effects.contains(&AgentViewsEffect::ReadSession {
            session_id: "sess-1".to_string(),
            until: None,
            tail_bytes: STREAM_WINDOW_BYTES,
        }));

        // Deliver initial tail read; uncommitted provisional reply `prov-1` is dropped
        // before starting live follow at `from_byte: Some(50)`.
        views
            .apply_read_jsonl(&canvas, "sess-1", &initial_lines, 200, None)
            .unwrap();
        let effects = views.drain_effects();
        assert!(effects.contains(&AgentViewsEffect::FollowSession {
            session_id: "sess-1".to_string(),
            from_byte: Some(50),
        }));

        let desc = views.describe(&canvas, "t1").unwrap();
        assert_eq!(desc.session_id.as_deref(), Some("sess-1"));
        assert_eq!(
            desc.rows
                .iter()
                .map(|r| r.summary.id.as_str())
                .collect::<Vec<_>>(),
            vec!["p1"]
        );

        // Terminal outputs 20 scrolled rows (scrollback_now = 120, cursor_row = 6) right as
        // live tool call `t-read` arrives (`add`), then completes in place (`update`).
        {
            let grid = canvas.terminals.get_mut("t1").unwrap();
            grid.scrolled_rows = 20;
            grid.cursor_row = 6;
        }
        views.apply_live_jsonl(
            &canvas,
            "sess-1",
            &[make_event_json(
                "t-read",
                110,
                "tool",
                serde_json::json!({
                    "name": "Read",
                    "action": "read",
                    "target": "wall/wallLayout.js",
                    "result": { "status": "pending", "output": "" }
                }),
                None,
            )],
        );

        let desc = views.describe(&canvas, "t1").unwrap();
        assert_eq!(
            desc.rows
                .iter()
                .map(|r| (r.summary.id.as_str(), r.summary.stat.as_str()))
                .collect::<Vec<_>>(),
            vec![("p1", ""), ("t-read", "…")]
        );

        // 60 more lines scroll off (`scrollback_now = 180`): `t-read` updates in place
        // AND keeps its write-time anchor (`abs_row = 120 + 6 = 126`), while `s-reply`
        // arrives with explicit protocol `terminal_line: 150`.
        canvas.terminals.get_mut("t1").unwrap().scrolled_rows = 80;
        views.apply_live_jsonl(
            &canvas,
            "sess-1",
            &[
                make_event_json(
                    "t-read",
                    145,
                    "tool",
                    serde_json::json!({
                        "name": "Read",
                        "action": "read",
                        "target": "wall/wallLayout.js",
                        "meta": { "range": [1, 30] },
                        "file": { "text": "export const FLOW_GAP = 12;\n", "start": 1, "total": 252 },
                        "result": { "status": "ok", "output": "" }
                    }),
                    Some(serde_json::json!({ "op": "update" })),
                ),
                make_event_json(
                    "s-reply",
                    170,
                    "say",
                    serde_json::json!({ "text": "Done reading!", "final": true }),
                    Some(serde_json::json!({ "terminal_line": 150 })),
                ),
            ],
        );

        let desc = views.describe(&canvas, "t1").unwrap();
        assert_eq!(
            desc.rows
                .iter()
                .map(|r| (r.summary.id.as_str(), r.summary.stat.as_str()))
                .collect::<Vec<_>>(),
            vec![("p1", ""), ("t-read", "L1–30"), ("s-reply", "final")]
        );

        // Jump to `t-read`: now_scrollback = 180, abs_row = 126, desired_top = 124 -> offset = 56.
        let _ = views.drain_effects();
        let jump_res = views.jump(&canvas, "t1", "t-read").unwrap();
        assert_eq!(jump_res.offset, 56);
        assert!(jump_res.exact);
        assert!(views.drain_effects().contains(&AgentViewsEffect::ScrollTerminal {
            id: "t1".to_string(),
            target: ScrollTarget::Offset(56),
        }));

        // Jump to `s-reply` (protocol terminal_line = 150): desired_top = 148 -> offset = 32.
        let proto_jump = views.jump(&canvas, "t1", "s-reply").unwrap();
        assert_eq!(proto_jump.offset, 32);
        assert!(views.drain_effects().contains(&AgentViewsEffect::ScrollTerminal {
            id: "t1".to_string(),
            target: ScrollTarget::Offset(32),
        }));

        // Return to live (`offset = 0`).
        let live_jump = views.jump(&canvas, "t1", "live").unwrap();
        assert!(live_jump.live);
        assert_eq!(live_jump.offset, 0);
        assert!(views.drain_effects().contains(&AgentViewsEffect::ScrollTerminal {
            id: "t1".to_string(),
            target: ScrollTarget::Live,
        }));

        // Alternate screen refuses scrollback jump with a clear notice.
        canvas.terminals.get_mut("t1").unwrap().alt_screen = true;
        let alt_err = views.jump(&canvas, "t1", "t-read").unwrap_err();
        assert!(alt_err.contains("alternate screen"));
        let alt_effects = views.drain_effects();
        assert!(alt_effects.iter().any(|e| matches!(
            e,
            AgentViewsEffect::Notify { message } if message.contains("alternate screen")
        )));
        canvas.terminals.get_mut("t1").unwrap().alt_screen = false;

        // Show / hide single event, subset expand (`tools`), and collapse all.
        views.toggle_event(&canvas, "t1", "t-read", "on").unwrap();
        let expanded_body = views
            .describe(&canvas, "t1")
            .unwrap()
            .rows
            .into_iter()
            .find(|r| r.summary.id == "t-read")
            .unwrap()
            .detail
            .unwrap()
            .body;
        assert!(expanded_body.contains("1 │ export const FLOW_GAP = 12;"));

        views
            .set_bulk_expanded(&canvas, "t1", false, &["all"])
            .unwrap();
        assert_eq!(views.describe(&canvas, "t1").unwrap().expanded_count, 0);

        views
            .set_bulk_expanded(&canvas, "t1", true, &["tools"])
            .unwrap();
        assert_eq!(
            views
                .describe(&canvas, "t1")
                .unwrap()
                .rows
                .iter()
                .map(|r| (r.summary.id.as_str(), r.expanded))
                .collect::<Vec<_>>(),
            vec![("p1", false), ("t-read", true), ("s-reply", false)]
        );

        // Load earlier events before byte 50 (`p0` at 10).
        let ticket = views.request_load_earlier(&canvas, "t1").unwrap();
        assert_eq!(ticket.until, Some(50));
        let loaded = views
            .apply_read_jsonl(&canvas, "sess-1", &earlier_lines, 200, Some(50))
            .unwrap();
        assert_eq!(loaded, 1);
        assert_eq!(
            views
                .describe(&canvas, "t1")
                .unwrap()
                .rows
                .iter()
                .map(|r| r.summary.id.as_str())
                .collect::<Vec<_>>(),
            vec!["p0", "p1", "t-read", "s-reply"]
        );

        // Move side + filter subset persist across a wall reload, and closing unfollows cleanly.
        views
            .set_side_on_canvas(&mut canvas, Some(&mut store), "t1", "left")
            .unwrap();
        views
            .set_filter(&canvas, Some(&mut store), "t1", "only", &["tools"])
            .unwrap();
        let tools_snap = views.describe(&canvas, "t1").unwrap();
        assert_eq!(tools_snap.side, AgentSide::Left);
        assert_eq!(tools_snap.filter_label, "tool calls");
        assert_eq!(tools_snap.category_counts.get("prompt"), Some(&2));
        assert_eq!(tools_snap.category_counts.get("read"), Some(&1));
        assert_eq!(tools_snap.category_counts.get("say"), Some(&1));
        assert_eq!(tools_snap.category_counts.get("noise"), Some(&0));
        assert_eq!(
            tools_snap
                .rows
                .iter()
                .map(|r| r.summary.id.as_str())
                .collect::<Vec<_>>(),
            vec!["t-read"]
        );

        // Screen transform sizing snaps outer card to whole pixels and keeps `hairline_px >= 1`.
        canvas.camera.set_pose(CameraPose::new(0.0, 0.0, 10.0));
        let ScreenPlacement::Visible(p_in) = views.screen_placement_of(&canvas, &canvas.camera, "t1")
        else {
            panic!("expected visible placement at zoom 10");
        };
        assert!(p_in.width >= 8 && p_in.height >= 8);
        assert!(!p_in.badge_mode);
        assert_eq!(p_in.hairline_px, 1);
        assert_eq!(p_in.z_index, 4);

        canvas.camera.set_pose(CameraPose::new(0.0, 0.0, 2.8));
        let ScreenPlacement::Visible(p_out) =
            views.screen_placement_of(&canvas, &canvas.camera, "t1")
        else {
            panic!("expected visible placement at zoom 2.8");
        };
        assert_eq!(p_out.hairline_px, 2);

        // Far zoom collapses into badge mode (`11 * (2.0 / 4.0) = 5.5 < 7.2`).
        canvas.camera.set_pose(CameraPose::new(0.0, 0.0, 2.0));
        let ScreenPlacement::Visible(p_badge) =
            views.screen_placement_of(&canvas, &canvas.camera, "t1")
        else {
            panic!("expected badge placement at zoom 2.0");
        };
        assert!(p_badge.badge_mode);

        // Reloading from shared storage preserves open + side + filters.
        let mut reloaded_store = WallStore::with_storage(Box::new(mem), "port-8199");
        let reloaded_views =
            AgentViews::from_store(&reloaded_store, canvas.av_config.default_side);
        assert!(reloaded_views.is_open(&canvas, "t1"));
        assert_eq!(reloaded_views.side_of(&canvas, "t1"), Some(AgentSide::Left));
        assert_eq!(
            reloaded_views.describe(&canvas, "t1").unwrap().filter_label,
            "tool calls"
        );

        // Closing unfollows `sess-1`.
        let _ = views.drain_effects();
        views
            .set_open_on_canvas(&mut canvas, Some(&mut store), "t1", "close", None)
            .unwrap();
        assert!(!views.is_open(&canvas, "t1"));
        assert!(views.drain_effects().contains(&AgentViewsEffect::UnfollowSession {
            session_id: "sess-1".to_string(),
        }));

        // Forgetting / pruning a terminal cleans its stored Agent View entry.
        reloaded_store.forget("t1");
        assert!(reloaded_store.agent_view("t1").is_none());
    }

    // ── Ported from tools/wall-split.test.mjs (the two Agent View split tests) ──

    fn assert_split_filled(canvas: &HeadlessCanvas, views: &AgentViews, gid: &str) {
        let group = canvas.groups.get(gid).unwrap();
        let split = group.split.as_ref().unwrap();
        let origin = group.origin.unwrap();
        let split_rect = Rect::from_top_left(origin.x, origin.y, split.w, split.h);
        let panes = canvas.pane_rects(views, gid);
        let leaves = crate::split::leaves(&split.tree);
        assert_eq!(panes.len(), leaves.len());
        for (i, id_a) in leaves.iter().enumerate() {
            let r = *panes.get(id_a).unwrap();
            assert!(r.min_x >= split_rect.min_x - 1e-9);
            assert!(r.max_x <= split_rect.max_x + 1e-9);
            assert!(r.min_y >= split_rect.min_y - 1e-9);
            assert!(r.max_y <= split_rect.max_y + 1e-9);
            for id_b in leaves.iter().skip(i + 1) {
                assert!(!overlaps(r, *panes.get(id_b).unwrap()));
            }
        }
    }

    #[test]
    fn agent_view_on_split_pane_takes_share_of_pane_and_closing_gives_it_back() {
        let (sx, sy) = (0.6, 1.2);
        let mut canvas = HeadlessCanvas::new_split_cells(
            &[("a", 80, 24), ("b", 80, 24), ("c", 80, 24)],
            sx,
            sy,
        );
        canvas.av_config.dims = AgentViewDims::new(12.0, 8.0, 0.5);
        let mut views = AgentViews::new();

        let gid = canvas.insert_pane(&views, "a", "b", SplitInsertDir::Right);
        canvas.insert_pane(&views, "a", "c", SplitInsertDir::Down);

        let room = canvas.group_config.split_bar_rows * sy;
        let before = (canvas.terminals["a"].cols, canvas.terminals["a"].rows);

        for &side in AGENT_SIDES {
            views
                .set_side_on_canvas(&mut canvas, None, "a", side.as_str())
                .unwrap();
            let pr = *canvas.pane_rects(&views, &gid).get("a").unwrap();
            let body = canvas.terminals["a"].body_rect();
            let view = agent_view_rect(body, room, side, canvas.av_config.dims);
            let win = Rect {
                max_y: body.max_y + room,
                ..body
            };
            // Both the Agent View and the terminal's window sit inside `a`'s pane…
            for r in [view, win] {
                assert!(r.min_x >= pr.min_x - 1e-9);
                assert!(r.max_x <= pr.max_x + 1e-9);
                assert!(r.min_y >= pr.min_y - 1e-9);
                assert!(r.max_y <= pr.max_y + 1e-9);
            }
            // …so it covers no other pane's terminal.
            for other in ["b", "c"] {
                assert!(!overlaps(view, canvas.window_rect(other, 4.0)));
            }
        }

        views
            .set_open_on_canvas(&mut canvas, None, "a", "close", None)
            .unwrap();
        assert_eq!(
            (canvas.terminals["a"].cols, canvas.terminals["a"].rows),
            before
        );
        assert_split_filled(&canvas, &views, &gid);
    }

    #[test]
    fn small_split_pane_grows_split_and_adjusts_ratio_for_agent_view_and_restores_on_close() {
        let (sx, sy) = (0.6, 1.2);
        let mut canvas =
            HeadlessCanvas::new_split_cells(&[("a", 44, 24), ("b", 44, 24)], sx, sy);
        canvas.av_config.dims = AgentViewDims::new(30.0, 8.0, 0.5);
        let mut views = AgentViews::new();

        let gid = canvas.insert_pane(&views, "a", "b", SplitInsertDir::Right);
        let w0 = canvas.groups.get(&gid).unwrap().split.as_ref().unwrap().w;

        views
            .set_side_on_canvas(&mut canvas, None, "a", "left")
            .unwrap();
        let g = canvas.groups.get(&gid).unwrap();
        assert!(g.split.as_ref().unwrap().w > w0);
        let pa = *canvas.pane_rects(&views, &gid).get("a").unwrap();
        let body = canvas.terminals["a"].body_rect();
        assert!(((body.min_x - pa.min_x) - 30.5).abs() < 1e-6);
        assert!(canvas.terminals["a"].cols >= 20);
        assert!(body.max_x <= pa.max_x + 1e-9);

        // Closing restores the split's original width, divider ratio, and equal pane columns.
        views
            .set_open_on_canvas(&mut canvas, None, "a", "close", None)
            .unwrap();
        let restored = canvas.groups.get(&gid).unwrap().split.as_ref().unwrap();
        assert_eq!(restored.w, w0);
        match &restored.tree {
            SplitNode::Branch { ratio, .. } => assert_eq!(*ratio, 0.5),
            SplitNode::Leaf { .. } => panic!("expected split branch"),
        }
        assert_eq!(canvas.terminals["a"].cols, canvas.terminals["b"].cols);
        assert_split_filled(&canvas, &views, &gid);
    }

    // ── Additional coordinator invariants (pinned sessions, orphan adoption, unread) ──

    #[test]
    fn pinned_session_overrides_ambiguous_directory_and_persists_in_store() {
        let mem = MemoryStorage::new();
        let mut store = WallStore::with_storage(Box::new(mem.clone()), "pin-test");
        let canvas = HeadlessCanvas::new_fixed(&[("t1", 80.0, 40.0, 0.0, 0.0)]);
        let mut views = AgentViews::new();

        views.sync_termd_status(
            &canvas,
            &[TermdTerminalStatus {
                id: "t1".to_string(),
                pwd: "/repo/app".to_string(),
                ..TermdTerminalStatus::default()
            }],
        );
        views.apply_session_list(
            &canvas,
            vec![
                AgentSession {
                    session_id: "live-older".to_string(),
                    harness: "claude".to_string(),
                    cwd: Some("/repo/app".to_string()),
                    status: "idle".to_string(),
                    mtime: "2026-11-14T21:00:00Z".to_string(),
                    ..AgentSession::default()
                },
                AgentSession {
                    session_id: "live-newer".to_string(),
                    harness: "claude".to_string(),
                    cwd: Some("/repo/app".to_string()),
                    status: "processing".to_string(),
                    mtime: "2026-11-14T22:10:00Z".to_string(),
                    ..AgentSession::default()
                },
            ],
            false,
        );

        // Auto cannot disambiguate 2 live agents in `/repo/app`.
        let auto_out = views
            .set_open(&canvas, Some(&mut store), "t1", "open", None)
            .unwrap();
        assert!(auto_out.session.is_none());
        assert!(auto_out
            .missing
            .as_deref()
            .unwrap_or("")
            .contains("2 live agents"));

        // Explicit session pin resolves immediately and round-trips through WallStore.
        let pinned_out = views
            .set_session(&canvas, Some(&mut store), "t1", "live-older")
            .unwrap();
        assert_eq!(pinned_out.session.as_deref(), Some("live-older"));
        assert!(pinned_out.missing.is_none());

        let reloaded_store = WallStore::with_storage(Box::new(mem), "pin-test");
        let reloaded_views =
            AgentViews::from_store(&reloaded_store, canvas.av_config.default_side);
        assert_eq!(
            reloaded_views.session_binding_of("t1"),
            SessionBinding::Pinned("live-older".to_string())
        );
    }

    #[test]
    fn orphan_updates_in_tail_window_are_adopted_when_earlier_page_loads_their_add() {
        let canvas = HeadlessCanvas::new_fixed(&[("t1", 80.0, 40.0, 0.0, 0.0)]);
        let mut views = AgentViews::new();
        views.sync_termd_status(
            &canvas,
            &[TermdTerminalStatus {
                id: "t1".to_string(),
                pwd: "/srv/wall".to_string(),
                ..TermdTerminalStatus::default()
            }],
        );
        views.apply_session_list(
            &canvas,
            vec![AgentSession {
                session_id: "s1".to_string(),
                harness: "claude".to_string(),
                cwd: Some("/srv/wall".to_string()),
                status: "processing".to_string(),
                ..AgentSession::default()
            }],
            false,
        );
        views.set_open(&canvas, None, "t1", "open", None).unwrap();

        // Tail window contains an `update` for `old-tool` (whose `add` was before byte 100)
        // plus a newer `add` `p2` at byte 100.
        let tail_lines = vec![
            make_event_json(
                "old-tool",
                85,
                "tool",
                serde_json::json!({
                    "name": "Read",
                    "action": "read",
                    "target": "src/lib.rs",
                    "meta": { "range": [1, 50] },
                    "result": { "status": "ok", "output": "" }
                }),
                Some(serde_json::json!({ "op": "update" })),
            ),
            make_event_json(
                "p2",
                100,
                "prompt",
                serde_json::json!({ "origin": "human", "text": "next" }),
                None,
            ),
        ];
        views
            .apply_read_jsonl(&canvas, "s1", &tail_lines, 300, None)
            .unwrap();
        assert_eq!(
            views
                .describe(&canvas, "t1")
                .unwrap()
                .rows
                .iter()
                .map(|r| r.summary.id.as_str())
                .collect::<Vec<_>>(),
            vec!["p2"]
        );

        // Paging earlier before byte 100 brings in `old-tool`'s original pending `add`,
        // which immediately adopts the completed `L1–50` orphan update!
        let earlier_lines = vec![make_event_json(
            "old-tool",
            20,
            "tool",
            serde_json::json!({
                "name": "Read",
                "action": "read",
                "target": "src/lib.rs",
                "result": { "status": "pending", "output": "" }
            }),
            None,
        )];
        views.request_load_earlier(&canvas, "t1").unwrap();
        views
            .apply_read_jsonl(&canvas, "s1", &earlier_lines, 300, Some(100))
            .unwrap();
        assert_eq!(
            views
                .describe(&canvas, "t1")
                .unwrap()
                .rows
                .iter()
                .map(|r| (r.summary.id.as_str(), r.summary.stat.as_str()))
                .collect::<Vec<_>>(),
            vec![("old-tool", "L1–50"), ("p2", "")]
        );

        // Unread count increments when unpinned from bottom and resets on `scroll_to_tail`.
        views.set_stick_bottom(&canvas, "t1", false).unwrap();
        views.apply_live_jsonl(
            &canvas,
            "s1",
            &[make_event_json(
                "s-live",
                160,
                "say",
                serde_json::json!({ "text": "hello", "final": true }),
                None,
            )],
        );
        assert_eq!(views.describe(&canvas, "t1").unwrap().unread_count, 1);
        views.scroll_to_tail(&canvas, "t1").unwrap();
        assert_eq!(views.describe(&canvas, "t1").unwrap().unread_count, 0);
    }
}
