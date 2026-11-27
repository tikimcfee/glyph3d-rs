//! Window-owned live `gomuxai` integration for the native terminal wall (`crates/agent-feed` +
//! `wall_core::agent_views::AgentViews`).
//!
//! Replaces the browser wall's Go relay (`cli/gomux.go` + `cli/agentevents.go`) and WebSocket RPC
//! bridge (`WallBus.js` + `WallAgentViews.js`) without blocking the window thread and without ever
//! reading a harness transcript directly (`agent` CLI via `crates/agent-feed` is the sole reader):
//!
//! 1. **Off-thread worker (`wall-agents-worker`)**: owns [`SessionStore`], an on-demand
//!    [`FleetFollower`] (`agent export --json --follow --all -q`), and per-session
//!    [`LineFollower`]s (`agent transcript <path> --follow [--from <byte>] -q`), waking the window
//!    through the shared `Arc<dyn Fn() + Send + Sync>` callback.
//! 2. **Zero idle cost & JS-parity cadences**:
//!    - When no Agent View is open, no mounted terminal is an agent (`gx-*`, `agent-*`, `attached`
//!      in `WallStore`, or `agent`/`wall-attach` in `termd` argv), and neither the Sidebar nor the
//!      Palette is open, **zero** subprocesses run and the worker blocks indefinitely on its
//!      [`Condvar`] (`0` timers, `0` polls, `0` CPU).
//!    - Session-list polling (`agent sessions --json -q`) runs **only** while at least one Agent
//!      View is open, at [`SESSION_POLL_INTERVAL`] (`10s`, matching `--sessions-interval` default
//!      `10 * time.Second` in `cli/main.go:218` and `AgentEvents.poll` in `cli/agentevents.go`).
//!    - Fleet updates (`agent export --json --follow --all -q`) use a single push stream (`0`
//!      polling while alive) with [`FLEET_INITIAL_BACKOFF`] (`5s`, matching `gomuxRestartDelay =
//!      5 * time.Second` in `cli/gomux.go:47`) up to [`FLEET_MAX_BACKOFF`] (`30s`) if the child
//!      exits unexpectedly.
//! 3. **Window-thread [`WallAgents::pump`]**: non-blockingly syncs `WallStore`'s Agent View flags
//!    and `termd` terminal metadata into [`AgentViews`], feeds the latest fleet snapshot to
//!    [`Wall::set_bus_snapshot`], folds queued session lists and transcript pages/lines into
//!    [`AgentViews`], dispatches outbound effects to the worker or `Wall`, and returns `true` iff
//!    any bus or Agent View state changed.
//! 4. **egui frame bridge (`#[cfg(feature = "egui-ui")]`)**: builds [`AgentViewPanelInput`] in
//!    egui logical points (`PlacementUnits::Points`) from `Wall` + viewport + scale factor, and
//!    applies [`AgentViewPanelState`]'s side outputs (`take_clicked_focus_id` -> `terminal.focus`,
//!    `take_stick_bottom_changes` -> `AgentViews::set_stick_bottom`) plus emitted `wall.*` verbs.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use agent_feed::{
    read_transcript_window, AgentCli, AgentCliConfig, FleetAgent, FleetFollower, FleetOptions,
    FleetSnapshot, LineFollower, SessionInfo, SessionStore, TranscriptWindow, WindowParams,
};
use serde_json::json;
use wall_core::agent_view::{AgentSession, AgentViewDims};
use wall_core::agent_views::{
    AgentTerminalSnapshot, AgentViews, AgentViewsConfig, AgentViewsEffect, AgentViewsHost,
    BusAgentHint, ScrollTarget, TermdTerminalStatus,
};
use wall_core::bus::{GomuxAgent, GomuxSnapshot};
use wall_core::commands::{
    AgentBulkExpandResult, AgentEarlierResult, AgentJumpResult, AgentToggleEventResult,
};
use wall_core::config::AgentSide;
use wall_core::layout::BAR_ROOM;
use wall_core::router::{redact_relay_secrets, CommandReply};
use wall_core::snap::GridSpec;
use wall_core::store::AgentViewState;
use wall_core::wall::{ScreenLookup, Wall};

#[cfg(feature = "egui-ui")]
use super::panels::agent_view::{AgentViewPanelInput, AgentViewPanelState};

/// Interval between background `agent sessions --json -q` polls while at least one Agent View is
/// open (`10s`, matching `--sessions-interval` default `10 * time.Second` in `cli/main.go:218`).
pub const SESSION_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Initial restart delay if `agent export --json --follow --all -q` exits unexpectedly (`5s`,
/// matching `gomuxRestartDelay = 5 * time.Second` in `cli/gomux.go:47`).
pub const FLEET_INITIAL_BACKOFF: Duration = Duration::from_secs(5);

/// Upper cap on exponential restart backoff for `agent export --follow` (`30s`).
pub const FLEET_MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Options controlling how [`WallAgents`] locates `agent` and paces its background feeds.
#[derive(Clone, Debug)]
pub struct WallAgentsOptions {
    /// How to invoke gomuxai's `agent` CLI (`PATH`, `~/.local/bin/agent`, or explicit binary +
    /// `--backend-config` session directory overrides).
    pub cli: AgentCli,
    /// Minimum interval between `agent sessions --json -q` polls while an Agent View is open
    /// (defaults to [`SESSION_POLL_INTERVAL`], `10s`).
    pub session_poll_interval: Duration,
    /// Store/host filters and restart backoff for `agent export --json --follow --all -q`.
    pub fleet_options: FleetOptions,
}

impl Default for WallAgentsOptions {
    fn default() -> Self {
        Self {
            cli: AgentCli::discover(),
            session_poll_interval: SESSION_POLL_INTERVAL,
            fleet_options: FleetOptions {
                initial_backoff: FLEET_INITIAL_BACKOFF,
                max_backoff: FLEET_MAX_BACKOFF,
                ..FleetOptions::default()
            },
        }
    }
}

impl WallAgentsOptions {
    /// Build [`WallAgentsOptions`] from [`crate::launch_config::LaunchConfig`], passing
    /// `agent_binary`, `claude_projects_dir`, `agy_brain_dir`, and `kimi_sessions_dir` to
    /// [`AgentCli::from_config`].
    pub fn from_launch_config(cfg: &crate::launch_config::LaunchConfig) -> Result<Self, String> {
        let cli = AgentCli::from_config(&AgentCliConfig {
            agent_binary: cfg.agent_binary.clone(),
            claude_projects_dir: cfg.claude_projects_dir.clone(),
            agy_brain_dir: cfg.agy_brain_dir.clone(),
            kimi_sessions_dir: cfg.kimi_sessions_dir.clone(),
        })?;
        let mut options = Self { cli, ..Self::default() };
        options.fleet_options.hosts = cfg.agent_hosts.clone().unwrap_or_else(|| "all".to_string());
        Ok(options)
    }
}

/// Live per-terminal metadata sampled from [`term_session::Sessions`] on the window thread.
///
/// Enriches `wall.termd()` (and stands in for it until `DaemonEvent::Status` is surfaced in
/// `term_session::News`), providing `OSC 7` `pwd` for directory-based session pairing plus
/// primary-screen `(scrollback_rows, view, cursor_row)` for write-time jump anchors.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveTerminalMeta {
    pub scrollback_rows: u64,
    pub view: u64,
    pub cursor_row: u32,
    pub alt_screen: bool,
    pub pwd: String,
    pub title: String,
}

#[derive(Debug)]
enum WorkerCmd {
    ListSessions {
        restart_follows: bool,
    },
    ReadSession {
        session_id: String,
        until: Option<i64>,
        tail_bytes: u64,
    },
    FollowSession {
        session_id: String,
        from_byte: Option<i64>,
    },
    UnfollowSession {
        session_id: String,
    },
}

#[derive(Debug)]
enum WorkerEvent {
    Sessions {
        sessions: Vec<SessionInfo>,
        restart_follows: bool,
    },
    SessionsError(String),
    ReadResult {
        session_id: String,
        until: Option<i64>,
        result: Result<TranscriptWindow, String>,
    },
    FollowError {
        session_id: String,
        error: String,
    },
}

#[derive(Default)]
struct WorkerControl {
    stopped: bool,
    want_fleet: bool,
    want_sessions_poll: bool,
    commands: VecDeque<WorkerCmd>,
}

#[derive(Default)]
struct WorkerInbox {
    events: VecDeque<WorkerEvent>,
}

fn agent_side_str(side: AgentSide) -> &'static str {
    match side {
        AgentSide::Right => "right",
        AgentSide::Left => "left",
        AgentSide::Top => "top",
        AgentSide::Bottom => "bottom",
    }
}

fn fleet_agent_to_gomux(a: &FleetAgent) -> GomuxAgent {
    GomuxAgent {
        name: a.name.clone(),
        host: a.host.clone(),
        parent: a.parent.clone(),
        backend: a.backend.clone(),
        session: a.session.clone(),
        namespace: a.namespace.clone(),
        data_dir: a.data_dir.clone(),
        cwd: a.cwd.clone(),
        created_at: a.created_at.clone(),
        live: a.live,
        attached: a.attached,
        status: a.status.clone(),
        end_reason: a.end_reason.clone(),
        event: a.event.clone(),
        detail: a.detail.clone(),
        tool: a.tool.clone(),
        tool_input: a.tool_input.clone(),
        status_at: a.status_at.clone(),
        session_id: a.session_id.clone(),
        transcript: a.transcript.clone(),
        harness: a.harness.clone(),
        transcript_size: a.transcript_size,
        transcript_mtime: a.transcript_mtime,
    }
}

fn fleet_snapshot_to_gomux(snap: &FleetSnapshot) -> GomuxSnapshot {
    GomuxSnapshot {
        gen: snap.gen,
        host: snap.host.clone(),
        dirs: snap.dirs.clone(),
        agents: snap.agents.iter().map(fleet_agent_to_gomux).collect(),
    }
}

fn session_info_to_agent_session(info: &SessionInfo) -> Option<AgentSession> {
    serde_json::to_value(info)
        .ok()
        .and_then(|v| serde_json::from_value(v).ok())
}

fn split_verb_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut escaped = false;
    for ch in line.trim().chars() {
        if escaped {
            match ch {
                'n' => cur.push('\n'),
                't' => cur.push('\t'),
                other => cur.push(other),
            }
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_quote = !in_quote;
            continue;
        }
        if ch.is_whitespace() && !in_quote {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            continue;
        }
        cur.push(ch);
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// Borrowed [`AgentViewsHost`] adapter over `Wall`, the latest [`FleetSnapshot`], and optional
/// live [`term_session::Sessions`] metadata.
struct WallHostAdapter<'a> {
    wall: &'a Wall,
    fleet: Option<&'a FleetSnapshot>,
    live_meta: &'a HashMap<String, LiveTerminalMeta>,
}

impl AgentViewsHost for WallHostAdapter<'_> {
    fn agent_views_config(&self) -> AgentViewsConfig {
        let cfg = self.wall.config();
        AgentViewsConfig {
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

    fn terminal(&self, id: &str) -> Option<AgentTerminalSnapshot> {
        let entry = self.wall.terminal(id)?;
        let body_rect = self.wall.terminal_rect(id)?;
        let live = self.live_meta.get(id);
        let status_term = self.wall.termd().terminal(id);
        let scrolled_rows = live
            .map_or(0, |m| m.scrollback_rows)
            .max(status_term.map_or(0, |t| t.scrollback_rows));
        let cursor_row = live.map_or(0, |m| m.cursor_row);
        let alt_screen = entry.modes.alt_screen
            || live.is_some_and(|m| m.alt_screen)
            || status_term.is_some_and(|t| t.alt);
        let window_title = if !entry.shell_title.is_empty() {
            entry.shell_title.clone()
        } else if let Some(t) = live.map(|m| m.title.as_str()).filter(|s| !s.is_empty()) {
            t.to_string()
        } else {
            status_term.map_or_else(String::new, |t| t.title.clone())
        };
        let on_canvas = !self.wall.store().hidden(id);
        Some(AgentTerminalSnapshot {
            body_rect,
            position: entry.position,
            scrolled_rows,
            cursor_row,
            alt_screen,
            window_title,
            on_canvas,
        })
    }

    fn bar_height(&self, id: &str, zoom: f64) -> f64 {
        let Some(entry) = self.wall.terminal(id) else {
            return BAR_ROOM;
        };
        if let Some(override_room) = entry.bar_room_override {
            return override_room.max(0.0);
        }
        if (zoom - self.wall.camera().zoom()).abs() < 1e-6 {
            if let (Some(body), Some(win)) =
                (self.wall.terminal_rect(id), self.wall.window_rect(id))
            {
                return (win.max_y - body.max_y).max(0.0);
            }
        }
        BAR_ROOM
    }

    fn terminal_name(&self, id: &str) -> String {
        self.wall.name_of(id)
    }

    fn store_name(&self, id: &str) -> Option<String> {
        self.wall.store().name(id).map(str::to_owned)
    }

    fn attached_target(&self, id: &str) -> Option<String> {
        self.wall.store().attached(id).map(str::to_owned)
    }

    fn bus_agent(&self, target: &str) -> Option<BusAgentHint> {
        let hit = self.fleet.and_then(|snap| snap.find(target))?;
        Some(BusAgentHint {
            session_id: hit.session_id.clone(),
            cwd: hit.cwd.clone(),
        })
    }

    fn focused_id(&self) -> Option<&str> {
        self.wall.focused_id()
    }

    fn fullscreen_active(&self) -> bool {
        self.wall.fullscreen().active()
    }

    fn zoom(&self) -> f64 {
        self.wall.camera().zoom()
    }
}

/// Window-owned coordinator bridging `crates/agent-feed`, `wall_core::wall::Wall`,
/// `wall_core::agent_views::AgentViews`, and the egui Agent View companion cards.
pub struct WallAgents {
    views: AgentViews,
    #[cfg(feature = "egui-ui")]
    panel_state: AgentViewPanelState,
    control: Arc<(Mutex<WorkerControl>, Condvar)>,
    inbox: Arc<Mutex<WorkerInbox>>,
    fleet_slot: Arc<Mutex<Option<Arc<FleetFollower>>>>,
    followers: Arc<Mutex<HashMap<String, Arc<LineFollower>>>>,
    worker: Option<JoinHandle<()>>,
    synced_store_views: BTreeMap<String, AgentViewState>,
    synced_termd_rows: Vec<TermdTerminalStatus>,
    synced_stick_bottom: HashMap<String, bool>,
    scrolled_rows: HashMap<String, u64>,
    live_meta: HashMap<String, LiveTerminalMeta>,
    last_fleet: Option<FleetSnapshot>,
    last_bus_gen: u64,
    last_bus_reason: String,
    feed_requested: bool,
    bus_requested: bool,
    last_notice: Option<String>,
}

impl WallAgents {
    /// Start [`WallAgents`] with default CLI discovery (`PATH`, then `~/.local/bin/agent`) and
    /// JS-parity cadences ([`SESSION_POLL_INTERVAL`] = `10s`, [`FLEET_INITIAL_BACKOFF`] = `5s`).
    ///
    /// Spawns zero `agent` child processes until [`Self::pump`] observes an open Agent View, an
    /// agent terminal, or an open Sidebar/Palette.
    pub fn new(wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self::with_options(WallAgentsOptions::default(), wake)
    }

    /// Start [`WallAgents`] with explicit [`WallAgentsOptions`].
    pub fn with_options(opts: WallAgentsOptions, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let control = Arc::new((Mutex::new(WorkerControl::default()), Condvar::new()));
        let inbox = Arc::new(Mutex::new(WorkerInbox::default()));
        let fleet_slot: Arc<Mutex<Option<Arc<FleetFollower>>>> = Arc::new(Mutex::new(None));
        let followers: Arc<Mutex<HashMap<String, Arc<LineFollower>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let control_bg = Arc::clone(&control);
        let inbox_bg = Arc::clone(&inbox);
        let fleet_bg = Arc::clone(&fleet_slot);
        let followers_bg = Arc::clone(&followers);

        let worker = thread::Builder::new()
            .name("wall-agents-worker".to_string())
            .spawn(move || {
                run_agents_worker(opts, control_bg, inbox_bg, fleet_bg, followers_bg, wake)
            })
            .expect("spawning wall-agents-worker thread must succeed");

        Self {
            views: AgentViews::new(),
            #[cfg(feature = "egui-ui")]
            panel_state: AgentViewPanelState::new(),
            control,
            inbox,
            fleet_slot,
            followers,
            worker: Some(worker),
            synced_store_views: BTreeMap::new(),
            synced_termd_rows: Vec::new(),
            synced_stick_bottom: HashMap::new(),
            scrolled_rows: HashMap::new(),
            live_meta: HashMap::new(),
            last_fleet: None,
            last_bus_gen: 0,
            last_bus_reason: String::new(),
            feed_requested: false,
            bus_requested: false,
            last_notice: None,
        }
    }

    /// Underlying [`AgentViews`] state machine.
    pub fn views(&self) -> &AgentViews {
        &self.views
    }

    /// Mutable access to the underlying [`AgentViews`] state machine.
    pub fn views_mut(&mut self) -> &mut AgentViews {
        &mut self.views
    }

    /// Build the [`wall_core::agent_views::AgentViewDescriptor`] for terminal `id` against `wall`.
    pub fn describe(
        &self,
        wall: &Wall,
        id: &str,
    ) -> Option<wall_core::agent_views::AgentViewDescriptor> {
        let host = WallHostAdapter {
            wall,
            fleet: self.last_fleet.as_ref(),
            live_meta: &self.live_meta,
        };
        self.views.describe(&host, id)
    }

    /// Update whether `id`'s event scroll list is pinned to its live bottom tail.
    pub fn set_stick_bottom(
        &mut self,
        wall: &Wall,
        id: &str,
        at_bottom: bool,
    ) -> Result<(), String> {
        let host = WallHostAdapter {
            wall,
            fleet: self.last_fleet.as_ref(),
            live_meta: &self.live_meta,
        };
        self.views.set_stick_bottom(&host, id, at_bottom)
    }

    /// Latest notice emitted by [`AgentViews`] (e.g. refusing a scrollback jump while on the
    /// alternate screen), if any.
    pub fn take_notice(&mut self) -> Option<String> {
        self.last_notice.take()
    }

    /// Explicitly request that the `gomuxai` fleet follower start (e.g. before headless `wall.bus`
    /// or `wall.attach` when the Sidebar and Palette are closed).
    pub fn request_bus(&mut self) {
        self.bus_requested = true;
    }

    /// Accumulate primary-screen rows scrolled off the top of `id` (`term_session::News::Filled`),
    /// keeping write-time jump anchors accurate while `id` stays at live tail without issuing a
    /// scrollback RPC.
    pub fn note_terminal_scrolled(
        &mut self,
        id: &str,
        scrolled_top: u32,
        resized: bool,
        within_margins: bool,
        alt_screen: bool,
    ) {
        if !resized && !within_margins && !alt_screen && scrolled_top > 0 {
            let next = {
                let entry = self.scrolled_rows.entry(id.to_owned()).or_insert(0);
                *entry = entry.saturating_add(u64::from(scrolled_top));
                *entry
            };
            let meta = self.live_meta.entry(id.to_owned()).or_default();
            meta.scrollback_rows = meta.scrollback_rows.max(next);
        }
    }

    /// Snapshot live per-terminal `pwd`, `scrollback`, `alt_screen`, and `cursor.row` from
    /// [`term_session::Sessions`] on the window thread before calling [`Self::pump`].
    pub fn sync_sessions(&mut self, sessions: &term_session::Sessions) {
        self.live_meta.clear();
        for id in sessions.ids() {
            let sb = sessions.scrollback(id).unwrap_or_default();
            let acc = self.scrolled_rows.entry(id.to_string()).or_insert(0);
            if sb.max > *acc {
                *acc = sb.max;
            }
            let scrollback_rows = sb.max.max(*acc);
            let modes = sessions.modes(id).unwrap_or_default();
            let (cursor_row, title) = match sessions.screen(id) {
                Some(s) => (u32::from(s.cursor.row), s.title.clone()),
                None => (0, String::new()),
            };
            let pwd = sessions.pwd(id).unwrap_or_default().to_string();
            self.live_meta.insert(
                id.to_string(),
                LiveTerminalMeta {
                    scrollback_rows,
                    view: sb.offset,
                    cursor_row,
                    alt_screen: modes.alt_screen,
                    pwd,
                    title,
                },
            );
        }
    }

    /// Inject live terminal metadata directly in headless tests without a `g3d-termd` daemon.
    pub fn set_live_terminal_meta(&mut self, id: impl Into<String>, meta: LiveTerminalMeta) {
        let id = id.into();
        let acc = self.scrolled_rows.entry(id.clone()).or_insert(0);
        if meta.scrollback_rows > *acc {
            *acc = meta.scrollback_rows;
        }
        self.live_meta.insert(id, meta);
    }

    /// Pump all pending `gomuxai` fleet, session-list, and transcript updates onto `wall` and
    /// `self.views` on the window thread without blocking on I/O.
    ///
    /// Returns `true` iff the bus snapshot, session pairing, event rows, or terminal scrollback
    /// effects changed and a frame redraw should be scheduled.
    pub fn pump(&mut self, wall: &mut Wall) -> bool {
        let gen_before = self.views.generation();
        let mut bus_changed = false;
        let mut wall_mutated = false;

        // 1. Prune departed terminals from AgentViews & cached live metadata.
        self.scrolled_rows
            .retain(|id, _| wall.terminal(id).is_some());
        self.live_meta
            .retain(|id, _| wall.terminal(id).is_some());
        for st in wall.termd().terminals() {
            let acc = self.scrolled_rows.entry(st.id.clone()).or_insert(0);
            if st.scrollback_rows > *acc {
                *acc = st.scrollback_rows;
            }
            if let Some(meta) = self.live_meta.get_mut(&st.id) {
                meta.scrollback_rows = meta.scrollback_rows.max(*acc);
            }
        }
        self.synced_stick_bottom
            .retain(|id, _| wall.terminal(id).is_some());
        {
            let host = WallHostAdapter {
                wall,
                fleet: self.last_fleet.as_ref(),
                live_meta: &self.live_meta,
            };
            self.views.prune(&host, |id| wall.terminal(id).is_some());
            for id in wall.model().order() {
                let want = wall.agent_stick_bottom(id);
                if self.synced_stick_bottom.get(id).copied() != Some(want) {
                    let _ = self.views.set_stick_bottom(&host, id, want);
                    self.synced_stick_bottom.insert(id.clone(), want);
                }
            }
        }

        // 2. Sync termd status + live session metadata (`pwd`, `cwd`, `scrollback_rows`, `view`, `alt`).
        let next_termd_rows = self.collect_termd_rows(wall);
        if next_termd_rows != self.synced_termd_rows {
            let host = WallHostAdapter {
                wall,
                fleet: self.last_fleet.as_ref(),
                live_meta: &self.live_meta,
            };
            self.views.sync_termd_status(&host, &next_termd_rows);
            self.synced_termd_rows = next_termd_rows;
        }

        // 3. Reconcile WallStore's durable AgentViewState (`open`, `side`, `session`, `filters`).
        self.sync_store_into_views(wall);

        // 4. Pull the latest fleet snapshot / error from `FleetFollower` (if running).
        let fleet_obs = self
            .fleet_slot
            .lock()
            .expect("fleet slot mutex poisoned")
            .as_ref()
            .map(|f| (f.snapshot(), f.last_error()));

        if let Some((snap, err)) = fleet_obs {
            let reason = err.unwrap_or_default();
            if snap.gen > 0 && (snap.gen != self.last_bus_gen || reason != self.last_bus_reason) {
                let gomux = fleet_snapshot_to_gomux(&snap);
                wall.set_bus_snapshot(Some(gomux), reason.clone());
                let hints_changed = self.last_fleet.as_ref() != Some(&snap);
                self.last_fleet = Some(snap);
                self.last_bus_gen = self.last_fleet.as_ref().map_or(0, |s| s.gen);
                self.last_bus_reason = reason;
                bus_changed = true;
                if hints_changed {
                    self.rebind_open_views(wall);
                }
            } else if snap.gen == 0 && !reason.is_empty() && reason != self.last_bus_reason {
                wall.set_bus_snapshot(None, reason.clone());
                self.last_bus_reason = reason;
                bus_changed = true;
            }
        }

        // 5. Drain worker session-list & transcript window read results.
        let worker_events: Vec<WorkerEvent> = {
            let mut guard = self.inbox.lock().expect("agents inbox mutex poisoned");
            guard.events.drain(..).collect()
        };
        for ev in worker_events {
            match ev {
                WorkerEvent::Sessions {
                    sessions,
                    restart_follows,
                } => {
                    let mapped: Vec<AgentSession> = sessions
                        .iter()
                        .filter_map(session_info_to_agent_session)
                        .collect();
                    let host = WallHostAdapter {
                        wall,
                        fleet: self.last_fleet.as_ref(),
                        live_meta: &self.live_meta,
                    };
                    self.views.apply_session_list(&host, mapped, restart_follows);
                }
                WorkerEvent::SessionsError(err) => {
                    self.views.set_unavailable(Some(err));
                }
                WorkerEvent::ReadResult {
                    session_id,
                    until,
                    result,
                } => match result {
                    Ok(win) => {
                        let host = WallHostAdapter {
                            wall,
                            fleet: self.last_fleet.as_ref(),
                            live_meta: &self.live_meta,
                        };
                        if let Err(e) = self.views.apply_read_jsonl(
                            &host,
                            &session_id,
                            &win.lines,
                            win.size,
                            until,
                        ) {
                            self.views.apply_read_error(&session_id, until, e);
                        }
                    }
                    Err(err) => {
                        self.views.apply_read_error(&session_id, until, err);
                    }
                },
                WorkerEvent::FollowError { session_id, error } => {
                    self.views.apply_follow_error(&session_id, error);
                }
            }
        }

        // 6. Drain live `agent transcript --follow` line batches without holding the map mutex
        //    across folding or follower cleanup.
        let active_followers: Vec<(String, Arc<LineFollower>)> = {
            let guard = self
                .followers
                .lock()
                .expect("agents followers mutex poisoned");
            let mut list: Vec<(String, Arc<LineFollower>)> = guard
                .iter()
                .map(|(sid, f)| (sid.clone(), Arc::clone(f)))
                .collect();
            list.sort_by(|a, b| a.0.cmp(&b.0));
            list
        };
        let mut finished_sessions = Vec::new();
        for (session_id, follower) in active_followers {
            let batch = follower.take_batch();
            if !batch.lines.is_empty() {
                let host = WallHostAdapter {
                    wall,
                    fleet: self.last_fleet.as_ref(),
                    live_meta: &self.live_meta,
                };
                self.views
                    .apply_live_jsonl(&host, &session_id, &batch.lines);
            }
            if batch.ended {
                if let Some(err) = batch.error {
                    self.views.apply_follow_error(&session_id, err);
                } else {
                    self.views.apply_stream_ended(&session_id);
                }
                finished_sessions.push((session_id, follower));
            }
        }
        if !finished_sessions.is_empty() {
            let mut guard = self
                .followers
                .lock()
                .expect("agents followers mutex poisoned");
            for (sid, done) in finished_sessions {
                if guard.get(&sid).is_some_and(|cur| Arc::ptr_eq(cur, &done)) {
                    guard.remove(&sid);
                }
            }
        }

        // 7. If at least one mounted terminal has an open Agent View and we have never requested
        //    the initial session list, trigger `on_feed_up()` now so `ListSessions` runs at once.
        let any_open_view = self.any_mounted_open_view(wall);
        if any_open_view && !self.feed_requested {
            self.feed_requested = true;
            self.views.on_feed_up();
        } else if !any_open_view {
            // Re-arm so reopening an Agent View after an idle spell immediately refreshes sessions.
            self.feed_requested = false;
        }

        // 8. Drain outbound AgentViewsEffects, forwarding IO commands to the worker and canvas/
        //    scrollback effects to `wall`.
        let mut outbound_cmds = Vec::new();
        for effect in self.views.drain_effects() {
            match effect {
                AgentViewsEffect::ListSessions => {
                    outbound_cmds.push(WorkerCmd::ListSessions {
                        restart_follows: false,
                    });
                }
                AgentViewsEffect::ReadSession {
                    session_id,
                    until,
                    tail_bytes,
                } => {
                    outbound_cmds.push(WorkerCmd::ReadSession {
                        session_id,
                        until,
                        tail_bytes,
                    });
                }
                AgentViewsEffect::FollowSession {
                    session_id,
                    from_byte,
                } => {
                    outbound_cmds.push(WorkerCmd::FollowSession {
                        session_id,
                        from_byte,
                    });
                }
                AgentViewsEffect::UnfollowSession { session_id } => {
                    outbound_cmds.push(WorkerCmd::UnfollowSession { session_id });
                }
                AgentViewsEffect::ScrollTerminal { id, target } => match target {
                    ScrollTarget::Live => {
                        let _ = wall.run(["terminal.scroll", id.as_str(), "live"]);
                        if let Some(meta) = self.live_meta.get_mut(&id) {
                            meta.view = 0;
                        }
                        wall_mutated = true;
                    }
                    ScrollTarget::Offset(target_off) => {
                        let prev_view = self
                            .live_meta
                            .get(&id)
                            .map(|m| m.view)
                            .unwrap_or_else(|| self.views.view_offset_of(&id));
                        // `terminal.scroll` takes `+` for up into history, `-` for down toward live.
                        let delta = i128::from(target_off) - i128::from(prev_view);
                        let lines =
                            delta.clamp(i128::from(i32::MIN / 2), i128::from(i32::MAX / 2)) as i32;
                        if lines != 0 {
                            let lines_arg = lines.to_string();
                            let _ =
                                wall.run(["terminal.scroll", id.as_str(), lines_arg.as_str()]);
                            if let Some(meta) = self.live_meta.get_mut(&id) {
                                meta.view = target_off;
                            }
                            wall_mutated = true;
                        }
                    }
                },
                AgentViewsEffect::MoveGroup { group_id, dx, dy } => {
                    let dx_s = dx.to_string();
                    let dy_s = dy.to_string();
                    let _ = wall.run([
                        "wall.group.move",
                        group_id.as_str(),
                        dx_s.as_str(),
                        dy_s.as_str(),
                    ]);
                    wall_mutated = true;
                }
                AgentViewsEffect::MoveTerminal { .. } => {
                    wall_mutated = true;
                }
                AgentViewsEffect::Notify { message } => {
                    log::info!("agent view: {message}");
                    self.last_notice = Some(message);
                }
                AgentViewsEffect::FootprintChanged { .. } | AgentViewsEffect::Redraw => {}
            }
        }

        // 9. Update worker demand flags (`want_fleet`, `want_sessions_poll`) and wake the worker
        //    only when commands were queued or a demand flag changed.
        let want_fleet = any_open_view
            || self.any_agent_terminal(wall)
            || wall.store().sidebar().open
            || wall.palette_open()
            || self.bus_requested;
        let want_sessions_poll = any_open_view;

        {
            let (lock, cvar) = &*self.control;
            let mut ctrl = lock.lock().expect("agents worker control mutex poisoned");
            let demand_changed =
                ctrl.want_fleet != want_fleet || ctrl.want_sessions_poll != want_sessions_poll;
            ctrl.want_fleet = want_fleet;
            ctrl.want_sessions_poll = want_sessions_poll;
            let has_cmds = !outbound_cmds.is_empty();
            ctrl.commands.extend(outbound_cmds);
            if demand_changed || has_cmds {
                cvar.notify_one();
            }
        }

        self.views.generation() != gen_before || bus_changed || wall_mutated
    }

    /// Execute a verb line, handling stream-backed `wall.agent.*` verbs (`expand`, `collapse`,
    /// `show`, `jump`, `earlier`) against [`AgentViews`] and forwarding all other verbs to `wall`,
    /// then pumping any resulting state/effect transitions immediately.
    pub fn run_verb(&mut self, wall: &mut Wall, line: &str) -> CommandReply {
        if let Some(reply) = self.try_run_stream_verb(wall, line) {
            let _ = self.pump(wall);
            return reply;
        }
        self.note_bus_verb(line);
        let reply = wall.run(line);
        let _ = self.pump(wall);
        reply
    }

    /// Execute a verb line with live [`ScreenLookup`] access (for `app.rs::serve_control` where
    /// `wall.copy` reads active terminal selections).
    pub fn run_verb_with_screens(
        &mut self,
        wall: &mut Wall,
        line: &str,
        screens: &dyn ScreenLookup,
    ) -> CommandReply {
        if let Some(reply) = self.try_run_stream_verb(wall, line) {
            let _ = self.pump(wall);
            return reply;
        }
        self.note_bus_verb(line);
        let reply = wall.run_with_screens(line, screens);
        let _ = self.pump(wall);
        reply
    }

    // ── egui panel integration (`#[cfg(feature = "egui-ui")]`) ───────────────

    /// Borrow the retained [`AgentViewPanelState`] owned by [`WallAgents`].
    #[cfg(feature = "egui-ui")]
    pub fn panel_state(&self) -> &AgentViewPanelState {
        &self.panel_state
    }

    /// Mutably borrow the retained [`AgentViewPanelState`] owned by [`WallAgents`].
    #[cfg(feature = "egui-ui")]
    pub fn panel_state_mut(&mut self) -> &mut AgentViewPanelState {
        &mut self.panel_state
    }

    /// Whether any mounted, unhidden terminal currently has an open Agent View card to draw (when
    /// single/group fullscreen is not active).
    #[cfg(feature = "egui-ui")]
    pub fn has_open_views(&self, wall: &Wall) -> bool {
        if wall.fullscreen().active() {
            return false;
        }
        self.any_mounted_open_view(wall)
    }

    /// Build [`AgentViewPanelInput`] for the current frame in egui logical points
    /// (`PlacementUnits::Points`) from `wall`, the physical framebuffer `viewport_px`, and window
    /// `scale_factor`.
    #[cfg(feature = "egui-ui")]
    pub fn panel_input(
        &self,
        wall: &Wall,
        viewport_px: (u32, u32),
        scale_factor: f64,
    ) -> AgentViewPanelInput {
        let sf = scale_factor.max(0.1);
        let vw = (f64::from(viewport_px.0) / sf).max(1.0);
        let vh = (f64::from(viewport_px.1) / sf).max(1.0);
        let camera = wall_core::camera::WallCamera::new(wall.camera().pose(), vw, vh);
        let shown_order = wall.model().shown_order();
        let host = WallHostAdapter {
            wall,
            fleet: self.last_fleet.as_ref(),
            live_meta: &self.live_meta,
        };
        AgentViewPanelInput::from_views(&host, &self.views, &camera, &shown_order)
    }

    /// Build [`AgentViewPanelInput`] and render all visible Agent View cards into `ctx` using
    /// `self.panel_state`, appending any emitted verb lines to `out_verbs`.
    #[cfg(feature = "egui-ui")]
    pub fn show_panel(
        &mut self,
        ctx: &egui::Context,
        wall: &Wall,
        viewport_px: (u32, u32),
        scale_factor: f64,
        out_verbs: &mut Vec<String>,
    ) {
        let input = self.panel_input(wall, viewport_px, scale_factor);
        super::panels::agent_view::show(ctx, &mut self.panel_state, &input, out_verbs);
    }

    /// Apply side outputs from `self.panel_state` (`take_clicked_focus_id`,
    /// `take_stick_bottom_changes`) and execute `verbs` after the egui pass completes.
    #[cfg(feature = "egui-ui")]
    pub fn apply_own_panel_outputs(&mut self, wall: &mut Wall, verbs: &[String]) -> bool {
        let mut state = std::mem::take(&mut self.panel_state);
        let changed = self.apply_panel_outputs(wall, &mut state, verbs);
        self.panel_state = state;
        changed
    }

    /// Apply side outputs from an explicit [`AgentViewPanelState`] (`take_clicked_focus_id` ->
    /// `terminal.focus <id>`, `take_stick_bottom_changes` -> [`AgentViews::set_stick_bottom`])
    /// and execute emitted `verbs`, returning `true` if `wall` or `AgentViews` state changed.
    #[cfg(feature = "egui-ui")]
    pub fn apply_panel_outputs(
        &mut self,
        wall: &mut Wall,
        panel_state: &mut AgentViewPanelState,
        verbs: &[String],
    ) -> bool {
        let mut changed = false;

        if let Some(focus_id) = panel_state.take_clicked_focus_id() {
            if wall.terminal(&focus_id).is_some() && wall.focused_id() != Some(focus_id.as_str()) {
                let _ = wall.run(["terminal.focus", focus_id.as_str()]);
                changed = true;
            }
        }

        let stick_changes = panel_state.take_stick_bottom_changes();
        if !stick_changes.is_empty() {
            let gen_before = self.views.generation();
            {
                let host = WallHostAdapter {
                    wall,
                    fleet: self.last_fleet.as_ref(),
                    live_meta: &self.live_meta,
                };
                for (id, at_bottom) in &stick_changes {
                    let _ = self.views.set_stick_bottom(&host, id, *at_bottom);
                }
            }
            for (id, at_bottom) in stick_changes {
                if wall.terminal(&id).is_some() {
                    let mode = if at_bottom { "on" } else { "off" };
                    let _ = wall.run(["wall.agent.follow", id.as_str(), mode]);
                    self.synced_stick_bottom.insert(id, at_bottom);
                }
            }
            if self.views.generation() != gen_before {
                changed = true;
            }
        }

        for line in verbs {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let _ = self.run_verb(wall, trimmed);
            changed = true;
        }

        if self.pump(wall) {
            changed = true;
        }
        changed
    }

    /// Stop the background worker, fleet follower, and all active transcript followers cleanly.
    pub fn shutdown(&mut self) {
        {
            let (lock, cvar) = &*self.control;
            let mut ctrl = lock.lock().expect("agents worker control mutex poisoned");
            ctrl.stopped = true;
            ctrl.commands.clear();
            cvar.notify_all();
        }
        let fleet = self
            .fleet_slot
            .lock()
            .expect("fleet slot mutex poisoned")
            .take();
        if let Some(f) = fleet {
            f.stop();
        }
        let followers: Vec<Arc<LineFollower>> = self
            .followers
            .lock()
            .expect("agents followers mutex poisoned")
            .drain()
            .map(|(_, f)| f)
            .collect();
        for f in followers {
            f.stop();
        }
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }

    // ── Internal helpers ─────────────────────────────────────────────────────

    fn collect_termd_rows(&self, wall: &Wall) -> Vec<TermdTerminalStatus> {
        let mut by_id: BTreeMap<String, TermdTerminalStatus> = BTreeMap::new();
        for st in wall.termd().terminals() {
            let max_view = st
                .subscribers
                .iter()
                .map(|s| s.view)
                .max()
                .unwrap_or(0);
            by_id.insert(
                st.id.clone(),
                TermdTerminalStatus {
                    id: st.id.clone(),
                    scrollback_rows: st.scrollback_rows,
                    view: max_view,
                    alt: st.alt,
                    pwd: st.pwd.clone().unwrap_or_default(),
                    cwd: st.cwd.clone(),
                    title: st.title.clone(),
                },
            );
        }
        for (id, live) in &self.live_meta {
            let entry = by_id
                .entry(id.clone())
                .or_insert_with(|| TermdTerminalStatus {
                    id: id.clone(),
                    ..TermdTerminalStatus::default()
                });
            entry.scrollback_rows = entry.scrollback_rows.max(live.scrollback_rows);
            entry.view = live.view;
            entry.alt |= live.alt_screen;
            if !live.pwd.is_empty() {
                entry.pwd.clone_from(&live.pwd);
            }
            if !live.title.is_empty() {
                entry.title.clone_from(&live.title);
            }
        }
        by_id.into_values().collect()
    }

    fn sync_store_into_views(&mut self, wall: &Wall) {
        let current = wall.store().agent_views();
        if *current == self.synced_store_views {
            // Also check if a previously unmounted terminal with `saved.open == true` just mounted!
            let newly_mounted_open = current.iter().any(|(id, saved)| {
                saved.open
                    && wall.terminal(id).is_some()
                    && !self.views.is_marked_open(id)
            });
            if !newly_mounted_open {
                return;
            }
        }

        let default_side = wall.config().agent_view_side;
        self.views.hydrate_from_store(wall.store(), default_side);

        let host = WallHostAdapter {
            wall,
            fleet: self.last_fleet.as_ref(),
            live_meta: &self.live_meta,
        };

        // Close any view that was previously open in `synced_store_views` and was removed or closed.
        let prev_ids: Vec<String> = self.synced_store_views.keys().cloned().collect();
        for id in prev_ids {
            let was_open = self
                .synced_store_views
                .get(&id)
                .is_some_and(|s| s.open);
            let now_open = current.get(&id).is_some_and(|s| s.open);
            if was_open && !now_open && wall.terminal(&id).is_some() {
                let _ = self.views.set_open(&host, None, &id, "close", None);
            }
        }

        // Apply newly opened, side-changed, session-rebound, or filter-updated entries.
        for (id, saved) in current {
            if wall.terminal(id).is_none() {
                continue;
            }
            let prev = self.synced_store_views.get(id);
            if prev != Some(saved) || (saved.open && self.views.resolved_session_of(id).is_none()) {
                if prev.map(|p| &p.session) != Some(&saved.session) {
                    let _ = self.views.set_session(&host, None, id, &saved.session);
                }
                if saved.open {
                    let _ = self.views.set_open(
                        &host,
                        None,
                        id,
                        "open",
                        Some(agent_side_str(saved.side)),
                    );
                }
            }
        }

        self.synced_store_views = current.clone();
    }

    fn rebind_open_views(&mut self, wall: &Wall) {
        let host = WallHostAdapter {
            wall,
            fleet: self.last_fleet.as_ref(),
            live_meta: &self.live_meta,
        };
        let open_ids: Vec<(String, AgentSide)> = wall
            .store()
            .agent_views()
            .iter()
            .filter(|(id, st)| st.open && wall.terminal(id).is_some())
            .map(|(id, st)| (id.clone(), st.side))
            .collect();
        for (id, side) in open_ids {
            let _ = self
                .views
                .set_open(&host, None, &id, "open", Some(agent_side_str(side)));
        }
    }

    fn any_mounted_open_view(&self, wall: &Wall) -> bool {
        let host = WallHostAdapter {
            wall,
            fleet: self.last_fleet.as_ref(),
            live_meta: &self.live_meta,
        };
        wall.model()
            .order()
            .iter()
            .any(|id| self.views.is_open(&host, id))
    }

    fn any_agent_terminal(&self, wall: &Wall) -> bool {
        for id in wall.model().order() {
            if id.starts_with("gx-") || id.starts_with("agent-") {
                return true;
            }
            if wall.store().attached(id).is_some() {
                return true;
            }
            if let Some(st) = wall.termd().terminal(id) {
                let looks_like_agent = st.argv.iter().any(|arg| {
                    arg == "agent"
                        || arg.ends_with("/agent")
                        || arg.contains("wall-attach")
                });
                if looks_like_agent {
                    return true;
                }
            }
        }
        false
    }

    fn note_bus_verb(&mut self, line: &str) {
        let words = split_verb_words(line);
        let head = words.first().map_or("", String::as_str);
        let sub = words.get(1).map_or("", String::as_str);
        if matches!(head, "wall.bus" | "wall.attach")
            || (head == "wall" && matches!(sub, "bus" | "attach"))
        {
            self.bus_requested = true;
        }
    }

    fn try_run_stream_verb(&mut self, wall: &Wall, line: &str) -> Option<CommandReply> {
        let words = split_verb_words(line);
        let first = words.first()?.as_str();
        let (canonical, args): (&str, &[String]) = match first {
            "wall.agent.expand"
            | "wall.agent.collapse"
            | "wall.agent.show"
            | "wall.agent.jump"
            | "wall.agent.earlier" => (first, &words[1..]),
            "wall" if words.get(1).map(String::as_str) == Some("agent") => {
                match words.get(2).map(String::as_str) {
                    Some("expand") => ("wall.agent.expand", &words[3..]),
                    Some("collapse") => ("wall.agent.collapse", &words[3..]),
                    Some("show") => ("wall.agent.show", &words[3..]),
                    Some("jump") => ("wall.agent.jump", &words[3..]),
                    Some("earlier") => ("wall.agent.earlier", &words[3..]),
                    _ => return None,
                }
            }
            _ => return None,
        };

        let result: Result<CommandReply, String> = (|| {
            let Some(id) = args.first().filter(|s| !s.is_empty()) else {
                return Err("name a terminal".to_string());
            };
            if wall.terminal(id).is_none() {
                return Err(format!("no terminal '{id}'"));
            }
            let host = WallHostAdapter {
                wall,
                fleet: self.last_fleet.as_ref(),
                live_meta: &self.live_meta,
            };
            match canonical {
                "wall.agent.expand" | "wall.agent.collapse" => {
                    let expand = canonical == "wall.agent.expand";
                    let cats = args.get(1..).unwrap_or(&[]);
                    let out = self.views.set_bulk_expanded(&host, id, expand, cats)?;
                    let word = if expand { "expanded" } else { "collapsed" };
                    let payload = AgentBulkExpandResult {
                        changed: out.changed,
                    };
                    Ok(CommandReply::ok(
                        format!("{word} {} event(s) on {id}", out.changed),
                        Some(json!(payload)),
                    ))
                }
                "wall.agent.show" => {
                    let event_id = args.get(1).map_or("", String::as_str);
                    let how = args.get(2).map_or("toggle", String::as_str);
                    if how == "full" {
                        self.views.toggle_event(&host, id, event_id, "on")?;
                        self.views.show_full_body(&host, id, event_id)?;
                        return Ok(CommandReply::ok(
                            format!("expanded full body of {event_id} on {id}"),
                            Some(json!({
                                "id": id,
                                "eventId": event_id,
                                "expanded": true,
                                "full": true,
                            })),
                        ));
                    }
                    if !matches!(how, "toggle" | "on" | "off") {
                        return Err(format!("show takes toggle|on|off|full, not '{how}'"));
                    }
                    let out = self.views.toggle_event(&host, id, event_id, how)?;
                    let word = if out.expanded { "expanded" } else { "collapsed" };
                    let payload = AgentToggleEventResult {
                        event_id: out.event_id,
                        expanded: out.expanded,
                    };
                    Ok(CommandReply::ok(
                        format!("{word} {event_id} on {id}"),
                        Some(json!(payload)),
                    ))
                }
                "wall.agent.jump" => {
                    let target = args.get(1).map_or("live", String::as_str);
                    let out = self.views.jump(&host, id, target)?;
                    let event_id_str = out.event_id.clone().unwrap_or_default();
                    let msg = if out.live {
                        format!("{id} scrolled to live")
                    } else {
                        format!("{id} jumped to {event_id_str} (offset {})", out.offset)
                    };
                    let payload = AgentJumpResult {
                        live: out.live,
                        event_id: event_id_str,
                        offset: usize::try_from(out.offset).unwrap_or(usize::MAX),
                    };
                    Ok(CommandReply::ok(msg, Some(json!(payload))))
                }
                "wall.agent.earlier" => {
                    let ticket = self.views.request_load_earlier(&host, id)?;
                    let queued = usize::from(ticket.until.is_some());
                    let payload = AgentEarlierResult { loaded: queued };
                    let msg = if ticket.until.is_some() {
                        format!("loading earlier events on {id}")
                    } else {
                        format!("loaded 0 earlier event(s) on {id}")
                    };
                    Ok(CommandReply::ok(msg, Some(json!(payload))))
                }
                _ => unreachable!("matched canonical stream verb above"),
            }
        })();

        Some(match result {
            Ok(reply) => reply,
            Err(msg) => CommandReply::err(redact_relay_secrets(&msg)),
        })
    }
}

impl Drop for WallAgents {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── Background worker loop (`wall-agents-worker`) ────────────────────────────

fn run_agents_worker(
    opts: WallAgentsOptions,
    control: Arc<(Mutex<WorkerControl>, Condvar)>,
    inbox: Arc<Mutex<WorkerInbox>>,
    fleet_slot: Arc<Mutex<Option<Arc<FleetFollower>>>>,
    followers: Arc<Mutex<HashMap<String, Arc<LineFollower>>>>,
    wake: Arc<dyn Fn() + Send + Sync>,
) {
    let poll_interval = opts.session_poll_interval.max(Duration::from_millis(10));
    let session_store = SessionStore::new(opts.cli.clone());
    let mut next_poll_at: Option<Instant> = None;
    let mut ever_delivered_sessions = false;

    loop {
        // Pull next state transition or sleep until either a command/demand change arrives or the
        // 10s session-poll timer expires.
        let (stopped, want_fleet, want_sessions_poll, next_cmd, poll_due) = {
            let (lock, cvar) = &*control;
            let mut ctrl = lock.lock().expect("agents worker control mutex poisoned");

            loop {
                if ctrl.stopped {
                    break (true, false, false, None, false);
                }
                let fleet_active = fleet_slot
                    .lock()
                    .expect("fleet slot mutex poisoned")
                    .is_some();
                if ctrl.want_fleet != fleet_active {
                    break (
                        false,
                        ctrl.want_fleet,
                        ctrl.want_sessions_poll,
                        ctrl.commands.pop_front(),
                        false,
                    );
                }
                if let Some(cmd) = ctrl.commands.pop_front() {
                    break (false, ctrl.want_fleet, ctrl.want_sessions_poll, Some(cmd), false);
                }
                if !ctrl.want_sessions_poll {
                    next_poll_at = None;
                    if !ctrl.want_fleet {
                        // Completely idle: ensure any leftover followers are cleaned up, then sleep
                        // indefinitely with zero timer wakeups.
                        let has_followers = !followers
                            .lock()
                            .expect("agents followers mutex poisoned")
                            .is_empty();
                        if has_followers {
                            break (false, false, false, None, false);
                        }
                    }
                    ctrl = cvar
                        .wait(ctrl)
                        .expect("agents worker control condvar poisoned");
                    continue;
                }

                let now = Instant::now();
                let due_at = *next_poll_at.get_or_insert(now);
                if now >= due_at {
                    next_poll_at = Some(now + poll_interval);
                    break (false, ctrl.want_fleet, true, None, true);
                }
                let wait_for = due_at.saturating_duration_since(now);
                let (next_ctrl, _) = cvar
                    .wait_timeout(ctrl, wait_for)
                    .expect("agents worker control condvar poisoned");
                ctrl = next_ctrl;
            }
        };

        if stopped {
            break;
        }

        // Start or stop `FleetFollower` outside the control mutex.
        let cur_fleet_running = fleet_slot
            .lock()
            .expect("fleet slot mutex poisoned")
            .is_some();
        if want_fleet && !cur_fleet_running {
            let wake_fleet = Arc::clone(&wake);
            let started = Arc::new(FleetFollower::start(
                opts.cli.clone(),
                opts.fleet_options.clone(),
                move || wake_fleet(),
            ));
            *fleet_slot.lock().expect("fleet slot mutex poisoned") = Some(started);
        } else if !want_fleet && cur_fleet_running {
            let old = fleet_slot.lock().expect("fleet slot mutex poisoned").take();
            if let Some(f) = old {
                f.stop();
            }
        }

        // If no Agent View is open, stop any remaining transcript followers.
        if !want_sessions_poll && next_cmd.is_none() {
            let drained: Vec<Arc<LineFollower>> = followers
                .lock()
                .expect("agents followers mutex poisoned")
                .drain()
                .map(|(_, f)| f)
                .collect();
            for f in drained {
                f.stop();
            }
        }

        if let Some(cmd) = next_cmd {
            handle_worker_cmd(
                cmd,
                &opts.cli,
                &session_store,
                &inbox,
                &followers,
                &wake,
                poll_interval,
                &mut next_poll_at,
                &mut ever_delivered_sessions,
            );
            continue;
        }

        if poll_due && want_sessions_poll {
            match session_store.refresh() {
                Ok(shape_changed) => {
                    if shape_changed || !ever_delivered_sessions {
                        let snap = session_store.snapshot();
                        ever_delivered_sessions = true;
                        inbox
                            .lock()
                            .expect("agents inbox mutex poisoned")
                            .events
                            .push_back(WorkerEvent::Sessions {
                                sessions: snap.sessions,
                                restart_follows: false,
                            });
                        wake();
                    }
                }
                Err(err) => {
                    if !ever_delivered_sessions {
                        inbox
                            .lock()
                            .expect("agents inbox mutex poisoned")
                            .events
                            .push_back(WorkerEvent::SessionsError(err));
                        wake();
                    }
                }
            }
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "worker command dispatch borrows the worker's owned session store, follower map, inbox, and poll schedule"
)]
fn handle_worker_cmd(
    cmd: WorkerCmd,
    cli: &AgentCli,
    session_store: &SessionStore,
    inbox: &Arc<Mutex<WorkerInbox>>,
    followers: &Arc<Mutex<HashMap<String, Arc<LineFollower>>>>,
    wake: &Arc<dyn Fn() + Send + Sync>,
    poll_interval: Duration,
    next_poll_at: &mut Option<Instant>,
    ever_delivered_sessions: &mut bool,
) {
    match cmd {
        WorkerCmd::ListSessions { restart_follows } => {
            *next_poll_at = Some(Instant::now() + poll_interval);
            match session_store.refresh() {
                Ok(_) => {
                    let snap = session_store.snapshot();
                    *ever_delivered_sessions = true;
                    inbox
                        .lock()
                        .expect("agents inbox mutex poisoned")
                        .events
                        .push_back(WorkerEvent::Sessions {
                            sessions: snap.sessions,
                            restart_follows,
                        });
                    wake();
                }
                Err(err) => {
                    let snap = session_store.snapshot();
                    if *ever_delivered_sessions || !snap.sessions.is_empty() {
                        inbox
                            .lock()
                            .expect("agents inbox mutex poisoned")
                            .events
                            .push_back(WorkerEvent::Sessions {
                                sessions: snap.sessions,
                                restart_follows,
                            });
                    } else {
                        inbox
                            .lock()
                            .expect("agents inbox mutex poisoned")
                            .events
                            .push_back(WorkerEvent::SessionsError(err));
                    }
                    wake();
                }
            }
        }
        WorkerCmd::ReadSession {
            session_id,
            until,
            tail_bytes,
        } => {
            let result = match session_store.resolve(&session_id) {
                Ok(session) => read_transcript_window(
                    cli,
                    &session,
                    &WindowParams {
                        from: None,
                        until: until.and_then(|u| u64::try_from(u).ok()),
                        tail: Some(tail_bytes),
                    },
                ),
                Err(e) => Err(e.to_string()),
            };
            inbox
                .lock()
                .expect("agents inbox mutex poisoned")
                .events
                .push_back(WorkerEvent::ReadResult {
                    session_id,
                    until,
                    result,
                });
            wake();
        }
        WorkerCmd::FollowSession {
            session_id,
            from_byte,
        } => {
            let prev = followers
                .lock()
                .expect("agents followers mutex poisoned")
                .remove(&session_id);
            if let Some(old) = prev {
                old.stop();
            }
            match session_store.resolve(&session_id) {
                Ok(session) => {
                    let wake_lines = Arc::clone(wake);
                    let from_u64 = from_byte.and_then(|b| u64::try_from(b).ok());
                    let follower = Arc::new(LineFollower::start(
                        cli.clone(),
                        session.path,
                        from_u64,
                        move || wake_lines(),
                    ));
                    followers
                        .lock()
                        .expect("agents followers mutex poisoned")
                        .insert(session_id, follower);
                }
                Err(e) => {
                    inbox
                        .lock()
                        .expect("agents inbox mutex poisoned")
                        .events
                        .push_back(WorkerEvent::FollowError {
                            session_id,
                            error: e.to_string(),
                        });
                    wake();
                }
            }
        }
        WorkerCmd::UnfollowSession { session_id } => {
            let prev = followers
                .lock()
                .expect("agents followers mutex poisoned")
                .remove(&session_id);
            if let Some(old) = prev {
                old.stop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use wall_core::model::WallEffect;
    use wall_core::wall::{TerminalAppeared, WallOptions as RulesOptions};

    use super::*;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "wall-agents-test-{label}-{}-{n}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_stub(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if pred() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        pred()
    }

    #[test]
    fn idle_wall_spawns_zero_agent_commands() {
        let scratch = Scratch::new("idle");
        let calls_file = scratch.0.join("calls.log");
        let stub = scratch.0.join("agent");

        write_stub(
            &stub,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n",
                calls_file.display()
            ),
        );

        let mut wall = Wall::in_memory(RulesOptions::default());
        wall.terminal_appeared(TerminalAppeared::new("t1", 80, 24).with_title("zsh"));
        wall.terminal_appeared(TerminalAppeared::new("t2", 80, 24).with_title("vim"));
        let _ = wall.drain_effects();

        let wakes = Arc::new(AtomicUsize::new(0));
        let wakes_cb = Arc::clone(&wakes);
        let mut agents = WallAgents::with_options(
            WallAgentsOptions {
                cli: AgentCli::new(&stub),
                // Even with a tight 25ms poll interval, an idle wall must never invoke `agent`.
                session_poll_interval: Duration::from_millis(25),
                fleet_options: FleetOptions {
                    initial_backoff: Duration::from_millis(25),
                    max_backoff: Duration::from_millis(50),
                    ..FleetOptions::default()
                },
            },
            Arc::new(move || {
                wakes_cb.fetch_add(1, Ordering::SeqCst);
            }),
        );

        for _ in 0..5 {
            assert!(
                !agents.pump(&mut wall),
                "idle wall pump must report no changes"
            );
            thread::sleep(Duration::from_millis(25));
        }
        agents.shutdown();

        let invocations = fs::read_to_string(&calls_file).unwrap_or_default();
        assert!(
            invocations.is_empty(),
            "idle wall with plain terminals and closed views must invoke `agent` 0 times, got:\n{invocations}"
        );
        assert_eq!(wakes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn stub_agent_drives_bus_snapshot_open_view_live_follow_load_earlier_and_jump() {
        let scratch = Scratch::new("full");
        let stub = scratch.0.join("agent");
        let transcript_file = scratch.0.join("coder-session.jsonl");
        let export_extra = scratch.0.join("export-extra.jsonl");
        let follow_extra = scratch.0.join("follow-extra.jsonl");
        let sessions_file = scratch.0.join("sessions.json");
        let initial_page = scratch.0.join("page-tail.jsonl");
        let earlier_page = scratch.0.join("page-earlier.jsonl");

        // Ensure `transcript_file` has >= 1024 bytes on disk so `until = 500` in `load_earlier`
        // stays below `stat_size` without clamping.
        fs::write(&transcript_file, "\n".repeat(1200)).unwrap();
        fs::write(&export_extra, "").unwrap();
        fs::write(&follow_extra, "").unwrap();

        fs::write(
            &sessions_file,
            serde_json::json!([
                {
                    "session_id": "sess-coder-1",
                    "harness": "claude",
                    "path": transcript_file,
                    "size": 1200,
                    "mtime": "2026-11-26T18:00:00Z",
                    "cwd": "/Users/lugo/work",
                    "project": "glyph3d-rs",
                    "title": "Build native WallAgents",
                    "status": "processing",
                    "workgroup": { "name": "coder", "host": "mac-dev" }
                }
            ])
            .to_string(),
        )
        .unwrap();

        // Initial tail window starts at `at: 500` (so `oldest_at = 500`, `has_earlier = true`),
        // includes an orphan `update` for `ev-0` (which preceded `at: 500`), and includes a
        // provisional Stop-hook `say` that must be pruned before `--follow` starts!
        let tail_lines = [
            serde_json::json!({
                "type": "session",
                "session_id": "sess-coder-1",
                "harness": "claude",
                "cwd": "/Users/lugo/work",
                "title": "Build native WallAgents",
                "status": "processing"
            })
            .to_string(),
            serde_json::json!({
                "type": "event",
                "op": "update",
                "id": "ev-0",
                "seq": 2,
                "at": 500,
                "line": 5,
                "ts": "2026-11-26T18:00:01Z",
                "turn": { "id": "t1", "index": 1 },
                "kind": "tool",
                "tool": {
                    "id": "c0",
                    "name": "Bash",
                    "action": "bash",
                    "detail": "cargo test",
                    "result": { "status": "ok", "output": "test result: ok. 17 passed" }
                }
            })
            .to_string(),
            serde_json::json!({
                "type": "event",
                "op": "add",
                "id": "ev-1",
                "seq": 3,
                "at": 700,
                "line": 8,
                "ts": "2026-11-26T18:00:02Z",
                "turn": { "id": "t1", "index": 1 },
                "kind": "prompt",
                "prompt": { "origin": "human", "text": "Wire WallAgents off the window thread" }
            })
            .to_string(),
            serde_json::json!({
                "type": "event",
                "op": "add",
                "id": "ev-prov",
                "seq": 4,
                "at": 900,
                "line": 10,
                "ts": "2026-11-26T18:00:03Z",
                "turn": { "id": "t1", "index": 1 },
                "kind": "say",
                "say": { "text": "provisional stop hook", "final": true, "provisional": true }
            })
            .to_string(),
        ];
        fs::write(&initial_page, format!("{}\n", tail_lines.join("\n"))).unwrap();

        let older_lines = [serde_json::json!({
            "type": "event",
            "op": "add",
            "id": "ev-0",
            "seq": 1,
            "at": 0,
            "line": 1,
            "ts": "2026-11-26T18:00:00Z",
            "turn": { "id": "t1", "index": 1 },
            "kind": "tool",
            "tool": {
                "call_id": "c0",
                "name": "Bash",
                "action": "bash",
                "status": "pending",
                "input": "cargo test",
                "summary": "$ cargo test"
            }
        })
        .to_string()];
        fs::write(&earlier_page, format!("{}\n", older_lines.join("\n"))).unwrap();

        let export_snap = serde_json::json!({
            "type": "snapshot",
            "host": "mac-dev",
            "stores": [{"dir": "/tmp/fake-store"}],
            "agents": [{
                "name": "coder",
                "host": "mac-dev",
                "store": "/tmp/fake-store",
                "backend": "claude",
                "session": "coder",
                "cwd": "/Users/lugo/work",
                "created_at": "2026-11-26T17:55:00Z",
                "live": true,
                "attached": true,
                "status": "processing",
                "session_id": "sess-coder-1",
                "transcript": transcript_file,
                "dialect": "claude"
            }]
        })
        .to_string();
        fs::write(&export_extra, format!("{export_snap}\n")).unwrap();

        // Stub `agent` shell script serving `export --follow`, `sessions --json`,
        // `transcript <path> --until <n>`, and `transcript <path> --follow`.
        // Uses `exec tail` so `child.kill()` kills `tail` directly without leaving a grandchild
        // holding stdout/stderr pipes open.
        write_stub(
            &stub,
            &format!(
                "#!/bin/sh\n\
                 sub=\"$1\"\n\
                 shift\n\
                 case \"$sub\" in\n\
                   export)\n\
                     exec tail -n +1 -f '{export_extra}'\n\
                     ;;\n\
                   sessions)\n\
                     cat '{sessions}'\n\
                     ;;\n\
                   transcript)\n\
                     mode=\"window\"\n\
                     until_val=\"\"\n\
                     while [ $# -gt 0 ]; do\n\
                       case \"$1\" in\n\
                         --follow) mode=\"follow\" ;;\n\
                         --until) shift; until_val=\"$1\" ;;\n\
                       esac\n\
                       shift\n\
                     done\n\
                     if [ \"$mode\" = \"follow\" ]; then\n\
                       exec tail -n +1 -f '{follow_extra}'\n\
                     elif [ \"$until_val\" = \"700\" ]; then\n\
                       cat '{earlier}'\n\
                     else\n\
                       cat '{initial}'\n\
                     fi\n\
                     ;;\n\
                   *)\n\
                     exit 1\n\
                     ;;\n\
                 esac\n",
                export_extra = export_extra.display(),
                sessions = sessions_file.display(),
                earlier = earlier_page.display(),
                initial = initial_page.display(),
                follow_extra = follow_extra.display(),
            ),
        );

        let mut wall = Wall::in_memory(RulesOptions::default());
        wall.terminal_appeared(TerminalAppeared::new("gx-coder", 100, 30).with_title("claude"));
        wall.terminal_appeared(TerminalAppeared::new("t2", 80, 24).with_title("zsh"));
        let _ = wall.drain_effects();

        let wakes = Arc::new(AtomicUsize::new(0));
        let wakes_cb = Arc::clone(&wakes);
        let mut agents = WallAgents::with_options(
            WallAgentsOptions {
                cli: AgentCli::new(&stub),
                session_poll_interval: Duration::from_secs(10),
                fleet_options: FleetOptions {
                    initial_backoff: Duration::from_millis(50),
                    max_backoff: Duration::from_millis(200),
                    ..FleetOptions::default()
                },
            },
            Arc::new(move || {
                wakes_cb.fetch_add(1, Ordering::SeqCst);
            }),
        );

        // `gx-coder` is an agent terminal, so `pump` starts the fleet follower and delivers
        // `Wall::set_bus_snapshot`.
        let _ = agents.pump(&mut wall);
        assert!(
            wait_until(Duration::from_secs(3), || {
                let _ = agents.pump(&mut wall);
                wall.run("wall.bus").text.contains("coder")
            }),
            "bus snapshot should populate wall.bus, got: {}",
            wall.run("wall.bus").text
        );

        // Open Agent View on `gx-coder`: `pump` polls `agent sessions --json`, pairs `sess-coder-1`,
        // reads the tail window, prunes the provisional reply, and starts `--follow`!
        let open_reply = agents.run_verb(&mut wall, "wall.agent gx-coder open");
        assert!(open_reply.text.contains("open"), "reply: {}", open_reply.text);

        assert!(
            wait_until(Duration::from_secs(3), || {
                let _ = agents.pump(&mut wall);
                let host = WallHostAdapter {
                    wall: &wall,
                    fleet: agents.last_fleet.as_ref(),
                    live_meta: &agents.live_meta,
                };
                agents
                    .views()
                    .describe(&host, "gx-coder")
                    .is_some_and(|d| d.following && d.rows.len() == 1)
            }),
            "Agent View should hydrate `ev-1`, prune `ev-prov`, hold `ev-0` in orphans, and enter following"
        );

        // Unpin `stick_bottom`, stamp live terminal scrollback metadata, and append a live `add` +
        // `update` to `follow_extra`.
        agents.set_live_terminal_meta(
            "gx-coder",
            LiveTerminalMeta {
                scrollback_rows: 120,
                view: 0,
                cursor_row: 14,
                alt_screen: false,
                pwd: "/Users/lugo/work".to_string(),
                title: "claude".to_string(),
            },
        );
        agents.set_stick_bottom(&wall, "gx-coder", false).unwrap();

        let live_events = format!(
            "{}\n{}\n",
            serde_json::json!({
                "type": "event",
                "op": "add",
                "id": "ev-live",
                "seq": 5,
                "at": 950,
                "line": 12,
                "ts": "2026-11-26T18:00:05Z",
                "turn": { "id": "t1", "index": 1 },
                "kind": "say",
                "say": { "text": "WallAgents is live!", "final": true }
            }),
            serde_json::json!({
                "type": "event",
                "op": "update",
                "id": "ev-live",
                "seq": 6,
                "at": 990,
                "line": 13,
                "ts": "2026-11-26T18:00:06Z",
                "turn": { "id": "t1", "index": 1 },
                "kind": "say",
                "say": { "text": "WallAgents is live and updated!", "final": true }
            }),
        );
        fs::write(&follow_extra, live_events).unwrap();

        assert!(
            wait_until(Duration::from_secs(3), || {
                let _ = agents.pump(&mut wall);
                let host = WallHostAdapter {
                    wall: &wall,
                    fleet: agents.last_fleet.as_ref(),
                    live_meta: &agents.live_meta,
                };
                agents.views().describe(&host, "gx-coder").is_some_and(|d| {
                    d.rows.len() == 2
                        && d.unread_count == 1
                        && d.rows[1].summary.summary.contains("live and updated")
                })
            }),
            "live add + in-place update should fold into rows and increment unread_count"
        );

        // `wall.agent.earlier` loads `ev-0` before byte 700 (`ev-1`'s `at`) and adopts the orphan
        // `ok` update (clearing `"pending"`'s `"…"` status badge and attaching `"17 passed"`).
        let earlier_reply = agents.run_verb(&mut wall, "wall.agent.earlier gx-coder");
        assert!(
            !earlier_reply.text.starts_with("ERR:"),
            "earlier failed: {}",
            earlier_reply.text
        );

        assert!(
            wait_until(Duration::from_secs(3), || {
                let _ = agents.pump(&mut wall);
                agents.describe(&wall, "gx-coder").is_some_and(|d| {
                    d.rows.len() == 3
                        && !d.has_earlier
                        && d.rows[0].summary.id == "ev-0"
                        && d.rows[0].summary.status == "ok"
                        && d.rows[0].summary.stat.is_empty()
                })
            }),
            "load_earlier should prepend `ev-0` and adopt its `status: ok` orphan update"
        );

        // `wall.agent.show` + `wall.agent.expand` + `wall.agent.collapse` work end-to-end.
        let show_reply = agents.run_verb(&mut wall, "wall.agent.show gx-coder ev-0 full");
        assert!(
            !show_reply.text.starts_with("ERR:"),
            "show failed: {}",
            show_reply.text
        );
        let shown_detail = agents
            .describe(&wall, "gx-coder")
            .and_then(|d| d.rows.into_iter().next()?.detail)
            .expect("ev-0 expanded detail");
        assert!(
            shown_detail.body.contains("17 passed"),
            "adopted orphan update should carry tool result: {shown_detail:?}"
        );
        let collapse_reply = agents.run_verb(&mut wall, "wall.agent.collapse gx-coder all");
        assert!(!collapse_reply.text.starts_with("ERR:"));

        // Advance scrollback so jumping to `ev-live` (stamped at scrollback 120, cursor_row 14)
        // scrolls up into primary history via `WallEffect::ScrollTerminal`!
        agents.set_live_terminal_meta(
            "gx-coder",
            LiveTerminalMeta {
                scrollback_rows: 200,
                view: 0,
                cursor_row: 5,
                alt_screen: false,
                pwd: "/Users/lugo/work".to_string(),
                title: "claude".to_string(),
            },
        );
        let _ = agents.pump(&mut wall);
        let _ = wall.drain_effects();

        let jump_reply = agents.run_verb(&mut wall, "wall.agent.jump gx-coder ev-live");
        assert!(
            jump_reply.text.contains("jumped to ev-live"),
            "jump reply: {}",
            jump_reply.text
        );
        let effects = wall.drain_effects();
        assert!(
            effects.iter().any(|e| matches!(
                e,
                WallEffect::ScrollTerminal { id, lines, .. } if id == "gx-coder" && *lines > 0
            )),
            "jumping to an older write-time anchor must emit a positive (+up into history) ScrollTerminal effect, got: {effects:?}"
        );

        let live_jump = agents.run_verb(&mut wall, "wall.agent.jump gx-coder live");
        assert!(live_jump.text.contains("scrolled to live"));
        let live_effects = wall.drain_effects();
        assert!(
            live_effects
                .iter()
                .any(|e| matches!(e, WallEffect::ScrollTerminalLive { id } if id == "gx-coder")),
            "jumping to live must emit ScrollTerminalLive, got: {live_effects:?}"
        );

        // Test egui `panel_input` & `apply_panel_outputs` (`take_clicked_focus_id` +
        // `take_stick_bottom_changes`).
        #[cfg(feature = "egui-ui")]
        {
            wall.set_viewport(800.0, 500.0, 2.0);
            let _ = wall.run("wall.fit");
            for step in 1..=30 {
                let _ = wall.tick(0.05, f64::from(step) * 50.0);
            }
            let _ = wall.run("terminal.focus t2");
            assert_eq!(wall.focused_id(), Some("t2"));
            let pose_before = wall.camera().pose();

            let input = agents.panel_input(&wall, (1600, 1000), 2.0);
            assert_eq!(input.cards.len(), 1);
            assert_eq!(input.cards[0].view.id, "gx-coder");
            assert!(input.cards[0].terminal_screen_rect.is_some());

            let mut panel_state = AgentViewPanelState::new();
            panel_state.clicked_focus_id = Some("gx-coder".to_string());
            panel_state
                .stick_bottom_changes
                .push(("gx-coder".to_string(), true));
            let changed = agents.apply_panel_outputs(&mut wall, &mut panel_state, &[]);
            assert!(changed);
            assert_eq!(
                wall.focused_id(),
                Some("gx-coder"),
                "take_clicked_focus_id must focus terminal without camera-framing"
            );
            assert_eq!(
                wall.camera().pose(),
                pose_before,
                "clicking card focuses terminal without moving the camera"
            );
            let after_input = agents.panel_input(&wall, (1600, 1000), 2.0);
            assert_eq!(
                after_input.cards[0].view.unread_count, 0,
                "pinning stick_bottom back to true must clear unread_count"
            );
        }

        agents.shutdown();
    }

    #[test]
    fn note_terminal_scrolled_accumulates_primary_rows_only() {
        let mut wall = Wall::in_memory(RulesOptions::default());
        wall.terminal_appeared(TerminalAppeared::new("t1", 80, 24));
        let _ = wall.drain_effects();

        let mut agents = WallAgents::with_options(
            WallAgentsOptions {
                cli: AgentCli::new(PathBuf::from("/nonexistent/agent")),
                ..WallAgentsOptions::default()
            },
            Arc::new(|| {}),
        );

        agents.note_terminal_scrolled("t1", 18, false, false, false);
        agents.note_terminal_scrolled("t1", 5, true, false, false); // resize: ignored
        agents.note_terminal_scrolled("t1", 7, false, true, false); // scroll region: ignored
        agents.note_terminal_scrolled("t1", 9, false, false, true); // alt screen: ignored
        agents.note_terminal_scrolled("t1", 12, false, false, false);

        assert_eq!(agents.live_meta.get("t1").map(|m| m.scrollback_rows), Some(30));
        agents.shutdown();
    }

    #[test]
    fn live_osc_title_overrides_stale_termd_status_title_and_rebinds_open_view() {
        let scratch = Scratch::new("osc-title-rebind");
        let stub = scratch.0.join("agent");
        let sessions_json = serde_json::json!([
            {
                "session_id": "sess-coder-1",
                "harness": "claude",
                "path": scratch.0.join("coder.jsonl"),
                "size": 128,
                "mtime": "2026-11-26T18:30:00Z",
                "cwd": scratch.0,
                "title": "Coder session",
                "status": "processing",
                "workgroup": { "name": "coder", "host": "laptop-2" }
            }
        ]);
        let sessions_file = scratch.0.join("sessions.json");
        fs::write(&sessions_file, sessions_json.to_string()).unwrap();
        write_stub(
            &stub,
            &format!(
                "#!/bin/sh\nwhile [ $# -gt 0 ]; do case \"$1\" in --backend-config) shift; shift ;; -q|--quiet) shift ;; --*) shift ;; *) break ;; esac; done\ncase \"${{1:-}}\" in\n  sessions) cat '{}' ;;\n  export) printf '{{\"type\":\"snapshot\",\"host\":\"laptop-2\",\"stores\":[],\"agents\":[]}}\\n'; exec sleep 30 ;;\n  transcript) printf '' ;;\nesac\n",
                sessions_file.display()
            ),
        );

        let mut wall = Wall::in_memory(RulesOptions::default());
        wall.terminal_appeared(TerminalAppeared::new("t1", 80, 24).with_title("lugo@laptop-2:~"));
        let _ = wall.drain_effects();
        // Simulate a live News::Status push populating wall.termd() with zsh's initial shell title.
        let _ = wall.on_termd_status(
            serde_json::json!({
                "daemon": { "ns": "probe", "pid": 1 },
                "terminals": [{
                    "id": "t1",
                    "cols": 80,
                    "rows": 24,
                    "title": "lugo@laptop-2:~",
                    "cwd": "/Users/lugo",
                    "pwd": "/Users/lugo",
                    "scrollback_rows": 0
                }]
            }),
            1_000,
        );

        let mut agents = WallAgents::with_options(
            WallAgentsOptions {
                cli: AgentCli::new(&stub),
                session_poll_interval: Duration::from_secs(10),
                ..WallAgentsOptions::default()
            },
            Arc::new(|| {}),
        );

        // With zero Agent Views open, termd status sync must NOT trigger a redraw.
        assert!(!agents.pump(&mut wall));
        let _ = wall.on_termd_status(
            serde_json::json!({
                "daemon": { "ns": "probe", "pid": 1 },
                "terminals": [{
                    "id": "t1",
                    "cols": 80,
                    "rows": 24,
                    "title": "lugo@laptop-2:~",
                    "cwd": "/Users/lugo",
                    "pwd": "/Users/lugo",
                    "scrollback_rows": 5
                }]
            }),
            2_000,
        );
        assert!(
            !agents.pump(&mut wall),
            "background termd status updates with zero open Agent Views must not trigger redraws"
        );

        // Open Agent View on t1 while title is still shell default -> falls through to /Users/lugo.
        let _ = agents.run_verb(&mut wall, "wall.agent t1 open");
        assert!(wait_until(Duration::from_secs(3), || {
            let _ = agents.pump(&mut wall);
            !agents.views().sessions().is_empty()
        }));
        assert_eq!(agents.views().resolved_session_of("t1"), None);

        // Now live glyph-vt OSC 0 title updates to `gx-coder` while wall.termd() STILL holds the
        // stale shell title `lugo@laptop-2:~`: live.title must override and rebind immediately!
        wall.terminal_title("t1", "gx-coder");
        agents.set_live_terminal_meta(
            "t1",
            LiveTerminalMeta {
                title: "gx-coder".to_string(),
                pwd: "/Users/lugo".to_string(),
                ..LiveTerminalMeta::default()
            },
        );
        assert!(
            agents.pump(&mut wall),
            "live OSC title change to gx-coder must rebind and request one redraw"
        );
        assert_eq!(
            agents.views().resolved_session_of("t1"),
            Some("sess-coder-1"),
            "live OSC title gx-coder must override stale termd status title and pair sess-coder-1"
        );

        agents.shutdown();
    }

    /// Read-only live smoke check against the host's real `~/.local/bin/agent` (`agent sessions
    /// --json` and `agent export --json --follow`), never spawning, sending to, attaching, or
    /// cleaning up any real agent.
    #[test]
    #[ignore = "live read-only host check against ~/.local/bin/agent; run explicitly with --ignored"]
    fn live_host_sessions_and_fleet_pump_readonly_smoke() {
        let cli = AgentCli::discover();
        let mut wall = Wall::in_memory(RulesOptions::default());
        wall.terminal_appeared(TerminalAppeared::new("t1", 100, 30).with_title("probe"));
        let _ = wall.drain_effects();

        let mut agents = WallAgents::with_options(
            WallAgentsOptions {
                cli,
                session_poll_interval: SESSION_POLL_INTERVAL,
                fleet_options: FleetOptions {
                    initial_backoff: FLEET_INITIAL_BACKOFF,
                    max_backoff: FLEET_MAX_BACKOFF,
                    ..FleetOptions::default()
                },
            },
            Arc::new(|| {}),
        );

        // Open Agent View on `t1` (which also starts fleet export + session list poll).
        let cwd = std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        agents.set_live_terminal_meta(
            "t1",
            LiveTerminalMeta {
                pwd: cwd,
                ..LiveTerminalMeta::default()
            },
        );
        let _ = agents.run_verb(&mut wall, "wall.agent t1 open");

        let ready = wait_until(Duration::from_secs(8), || {
            let _ = agents.pump(&mut wall);
            !agents.views().sessions().is_empty() && agents.last_fleet.is_some()
        });
        let _ = agents.pump(&mut wall);

        let session_count = agents.views().sessions().len();
        let fleet_count = agents
            .last_fleet
            .as_ref()
            .map_or(0, |f| f.agents.len());
        let paired = agents.views().resolved_session_of("t1").map(str::to_owned);
        println!(
            "live readonly pump smoke: ready={ready}, sessions={session_count}, fleet_agents={fleet_count}, t1_paired={paired:?}, unavailable={:?}",
            agents.views().unavailable_reason()
        );
        assert!(
            agents.views().unavailable_reason().is_none(),
            "real host agent CLI should be available: {:?}",
            agents.views().unavailable_reason()
        );
        assert!(session_count > 0, "expected real host to report >= 1 session");

        agents.shutdown();
    }

    #[test]
    fn fleet_follows_every_host_unless_the_config_says_otherwise() {
        use crate::launch_config::LaunchConfig;
        let hosts = |agent_hosts: Option<&str>| {
            let cfg = LaunchConfig { agent_hosts: agent_hosts.map(str::to_string), ..LaunchConfig::default() };
            WallAgentsOptions::from_launch_config(&cfg).expect("options").fleet_options.hosts
        };
        assert_eq!(hosts(None), "all", "unset follows every host, as the JS relay did");
        assert_eq!(hosts(Some("")), "", "an empty value keeps this machine only");
        assert_eq!(hosts(Some("workstation,far-host")), "workstation,far-host");
    }
}
