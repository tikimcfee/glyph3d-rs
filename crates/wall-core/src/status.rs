//! Terminal daemon (`g3d-termd`) status document, connection-state model,
//! topbar chip, notice banner, and status-window rows, ported from
//! `packages/glyph3d-core/src/services/terminal/TermdStatus.js` and
//! `wall/StatusWindow.jsx` (against `docs/termd/PROTOCOL.md` § `status`).
//!
//! # Native Socket States (No Relay)
//!
//! In the browser app, a Go relay watched `g3d-termd`'s Unix socket and pushed
//! `{ relay, status }` frames to `TermdStatus.js`. In the native Rust app, the
//! app connects straight to `g3d-termd`'s Unix socket (`Event::Status(Value)`),
//! so the socket-level states map directly onto [`TermdStatus`]:
//! - [`TermdState::Unknown`]: initial state before the status watcher has
//!   probed the daemon socket.
//! - [`TermdState::Absent`]: nothing listens on the namespace socket
//!   (`ENOENT` / `ECONNREFUSED`), and either no live terminals were previously
//!   seen or the last status document carried `daemon.exiting == true` (idle
//!   exit or `g3d stop`). Clears `doc`.
//! - [`TermdState::Ok`]: a brain welcomed the connection and is delivering
//!   status documents (`StatusDoc`, parsed from `serde_json::Value`).
//! - [`TermdState::Restarting`]: a connection to a live non-exiting daemon
//!   closed (`ERROR restart` or brain crash EOF), or the holder accepted the
//!   socket while no brain has answered `WELCOME` yet, within the restart grace
//!   window ([`DEFAULT_RESTART_GRACE_MS`], 5 s). Keeps the last `StatusDoc`
//!   (marked stale) because the holder keeps all shells alive.
//! - [`TermdState::Unreachable`]: `Restarting` has lasted past the grace window
//!   without a brain answering `WELCOME`, or the socket path/permissions are
//!   unsafe (`EACCES`).
//! - [`TermdState::Failed`]: the holder accepted the socket and sent
//!   `ERROR {"code": "brain_failed"}` (brain crash-loop; shells alive, frozen).
//!   Keeps the last `StatusDoc`.
//! - [`TermdState::Lost`]: nothing listens on the socket (`ENOENT` /
//!   `ECONNREFUSED`) after a status document with live terminals and
//!   `daemon.exiting == false` — the holder died with shells inside it. Clears
//!   `doc` and latches [`LostLatch`] until dismissed ([`TermdStatus::dismiss_lost`]).

use std::collections::HashMap;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Grace period (milliseconds) before [`TermdState::Restarting`] escalates to
/// [`TermdState::Unreachable`] (`termdStatusGrace` = 5 s).
pub const DEFAULT_RESTART_GRACE_MS: u64 = 5_000;

/// Default delay (milliseconds) before a brain restart shows an amber banner
/// (`WALL_CONFIG.restartNoticeMs` = 400 ms).
pub const DEFAULT_RESTART_NOTICE_MS: u64 = 400;

/// Input operations required by the wall for typing and pasting (`protocol 1.2`).
pub const REQUIRED_INPUT_OPS: &[&str] = &["key", "paste"];

/// State of the `g3d-termd` daemon as observed from the native socket client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TermdState {
    Ok,
    Absent,
    Restarting,
    Unreachable,
    Failed,
    Lost,
    #[default]
    Unknown,
}

impl TermdState {
    pub const fn as_str(self) -> &'static str {
        match self {
            TermdState::Ok => "ok",
            TermdState::Absent => "absent",
            TermdState::Restarting => "restarting",
            TermdState::Unreachable => "unreachable",
            TermdState::Failed => "failed",
            TermdState::Lost => "lost",
            TermdState::Unknown => "unknown",
        }
    }

    /// Whether the daemon's brain is currently in trouble (`restarting`,
    /// `unreachable`, or `failed`; `TROUBLE` in `StatusWindow.jsx`).
    pub const fn is_trouble(self) -> bool {
        matches!(
            self,
            TermdState::Restarting | TermdState::Unreachable | TermdState::Failed
        )
    }
}

impl FromStr for TermdState {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "ok" => Ok(TermdState::Ok),
            "absent" => Ok(TermdState::Absent),
            "restarting" => Ok(TermdState::Restarting),
            "unreachable" => Ok(TermdState::Unreachable),
            "failed" => Ok(TermdState::Failed),
            "lost" => Ok(TermdState::Lost),
            "unknown" => Ok(TermdState::Unknown),
            other => Err(format!("unknown termd state '{other}'")),
        }
    }
}

/// Why a namespace left on purpose: the last status document's `daemon.exit_reason`
/// (protocol 1.6, "A namespace that leaves").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExitReason {
    /// A client asked (`g3d stop`).
    Stop,
    /// `SIGTERM` to the namespace's holder.
    Sigterm,
    /// The idle rule: no terminals, no leftovers and no connection but status followers
    /// for the idle time.
    Idle,
    /// It said `exiting` and not why: a daemon from before the field, or a word a later
    /// minor added (the protocol asks a client to read one it doesn't know as "left on
    /// purpose").
    Unsaid,
}

impl ExitReason {
    /// Read `daemon.exit_reason`.
    pub fn parse(word: Option<&str>) -> ExitReason {
        match word {
            Some("stop") => ExitReason::Stop,
            Some("sigterm") => ExitReason::Sigterm,
            Some("idle") => ExitReason::Idle,
            _ => ExitReason::Unsaid,
        }
    }

    /// The protocol's word (`unsaid` when it gave none this client knows).
    pub const fn as_str(self) -> &'static str {
        match self {
            ExitReason::Stop => "stop",
            ExitReason::Sigterm => "sigterm",
            ExitReason::Idle => "idle",
            ExitReason::Unsaid => "unsaid",
        }
    }

    /// What happened to termd, for the outage banner and the status window.
    pub const fn phrase(self) -> &'static str {
        match self {
            ExitReason::Stop => "termd stopped",
            ExitReason::Sigterm => "termd ended by SIGTERM",
            ExitReason::Idle => "termd closed after idling",
            ExitReason::Unsaid => "termd left on purpose",
        }
    }
}

/// Latched record of a namespace that went away with live shells in it
/// (`TermdStatus.lost`): a holder crash or kill, or (2026-11-27) a namespace that
/// left on purpose.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LostLatch {
    pub ns: String,
    pub at: u64,
    /// Which way it left, when the namespace left on purpose
    /// ([`TermdStatus::on_gone`]); `None` for one that simply stopped listening.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left: Option<ExitReason>,
}

/// A stopped namespace ends its terminals, then says it is exiting, within about
/// 0.2 s (measured on protocol 1.6). A document may list no terminal in between, so
/// shells listed this recently still count as lost with the namespace. The daemon's
/// idle exit comes 5 minutes after its last terminal, far outside this.
const SHELLS_LISTED_RECENTLY_MS: u64 = 3_000;

// ── Status document schema (docs/termd/PROTOCOL.md § status) ─────────────────

/// Whole status document pushed by `g3d-termd` (`status` / `status_follow`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusDoc {
    pub at: u64,
    pub daemon: DaemonStatus,
    pub terminals: Vec<StatusTerminal>,
    pub errors: Vec<StatusError>,
}

impl StatusDoc {
    /// Parse a `serde_json::Value` (either a bare status document or an `EVENT`
    /// payload carrying `"ev": "status"`) into a [`StatusDoc`].
    pub fn from_value(value: Value) -> Result<StatusDoc, String> {
        serde_json::from_value(value).map_err(|err| format!("invalid termd status JSON: {err}"))
    }
}

/// `daemon` section of [`StatusDoc`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonStatus {
    pub version: String,
    pub engine: String,
    pub proto: Vec<u32>,
    pub ns: Option<String>,
    pub socket: String,
    pub log: String,
    pub exiting: bool,
    /// (1.6) Only beside `exiting: true`: `"stop"`, `"sigterm"` or `"idle"`
    /// ([`ExitReason::parse`]).
    pub exit_reason: Option<String>,
    pub instance: String,
    pub started_at: u64,
    pub connections: u64,
    pub holder: HolderStatus,
    pub brain: BrainStatus,
    pub snapshots: SnapshotStatus,
    pub env: EnvStatus,
    pub trees: Option<TreeStatus>,
    pub leftovers: Vec<LeftoverStatus>,
}

/// `daemon.holder` section of [`StatusDoc`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HolderStatus {
    pub pid: u32,
    pub version: String,
    pub hold: Vec<u32>,
    pub rss: Option<u64>,
    pub upgrades: u64,
}

/// `daemon.brain` section of [`StatusDoc`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrainStatus {
    pub pid: u32,
    pub generation: u64,
    pub started_at: u64,
    pub rss: Option<u64>,
    pub restarts: RestartCounts,
    pub last_start: BrainLastStart,
}

/// `daemon.brain.restarts` counters by cause.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RestartCounts {
    pub crash: u64,
    pub watchdog: u64,
    pub upgrade: u64,
}

/// `daemon.brain.last_start` metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrainLastStart {
    pub reason: String,
    pub at: u64,
    pub failed_at: Option<u64>,
    pub unseen: u64,
}

/// `daemon.snapshots` gauges.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SnapshotStatus {
    pub interval_ms: u64,
    pub bytes: u64,
}

/// `daemon.env` section.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EnvStatus {
    pub source: String,
    pub config: Option<EnvConfigStatus>,
    pub warnings: Vec<String>,
}

/// `daemon.env.config` status (`termd.toml`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EnvConfigStatus {
    pub path: String,
    pub state: String,
    pub error: Option<String>,
}

/// `daemon.trees` process-tracking mode (`cgroup` or `session`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TreeStatus {
    pub mode: String,
    pub unit: Option<String>,
    pub why: Option<String>,
}

/// One orphaned process tree entry in `daemon.leftovers`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LeftoverStatus {
    pub id: String,
    pub pid: u32,
    pub since: u64,
    pub processes: Vec<LeftoverProcess>,
}

/// Process `(pid, comm)` inside [`LeftoverStatus`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LeftoverProcess {
    pub pid: u32,
    pub comm: String,
}

/// One live terminal entry in `terminals[]` of [`StatusDoc`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusTerminal {
    pub id: String,
    pub handle: u32,
    pub pid: u32,
    pub argv: Vec<String>,
    pub login: bool,
    pub cwd: String,
    pub pwd: Option<String>,
    pub title: String,
    pub cols: u16,
    pub rows: u16,
    pub alt: bool,
    pub created_at: u64,
    pub seq: u64,
    pub saved_seq: Option<u64>,
    pub out_bps: u64,
    pub in_bps: u64,
    pub last_output_at: Option<u64>,
    pub flow: String,
    pub input_queued: u64,
    pub scrollback_rows: u64,
    pub memory: Option<TerminalMemory>,
    pub restored: Option<String>,
    pub unseen: u64,
    pub processes: Option<u64>,
    pub subscribers: Vec<StatusSubscriber>,
}

/// Emulator memory usage (`resident` + `compressed` bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalMemory {
    pub resident: u64,
    pub compressed: u64,
}

/// One subscriber entry on a [`StatusTerminal`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusSubscriber {
    pub conn: u64,
    pub client: String,
    pub pid: Option<u32>,
    pub role: String,
    pub mode: String,
    pub flow: String,
    pub queued: u64,
    pub lagging: bool,
    pub lags: u64,
    pub view: u64,
}

/// One recent error entry in `errors[]` of [`StatusDoc`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusError {
    pub code: String,
    pub message: String,
    pub first_at: u64,
    pub at: u64,
    pub count: u64,
    pub id: Option<String>,
}

// ── TermdStatus model ────────────────────────────────────────────────────────

/// Snapshot envelope matching JS `termd/status` notifications (`{ state, since, ns, bundled, lacks }`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusViewMeta {
    pub state: TermdState,
    pub since: u64,
    pub ns: String,
    pub bundled: String,
    pub lacks: Vec<String>,
}

/// Stateful model of `g3d-termd`'s health, status document, per-terminal lookup,
/// missing-feature warnings, and sticky `lost` latch.
#[derive(Clone, Debug)]
pub struct TermdStatus {
    ns: String,
    bundled: String,
    state: TermdState,
    since_ms: u64,
    instance: Option<String>,
    lacks: Vec<String>,
    doc: Option<StatusDoc>,
    by_id: HashMap<String, usize>,
    /// Whether the last document that was not `exiting` listed any terminal.
    had_terminals: bool,
    /// When a document last listed a terminal.
    shells_listed_at: Option<u64>,
    last_exiting: bool,
    /// Which way the namespace left, from the document that said `exiting`: kept
    /// while nothing listens there, cleared by the next document that does not say it.
    left: Option<ExitReason>,
    lost: Option<LostLatch>,
}

impl Default for TermdStatus {
    fn default() -> TermdStatus {
        TermdStatus::new("")
    }
}

impl TermdStatus {
    /// Construct a fresh status model in [`TermdState::Unknown`] for `ns`.
    pub fn new(ns: impl Into<String>) -> TermdStatus {
        TermdStatus {
            ns: ns.into(),
            bundled: String::new(),
            state: TermdState::Unknown,
            since_ms: 0,
            instance: None,
            lacks: Vec::new(),
            doc: None,
            by_id: HashMap::new(),
            had_terminals: false,
            shells_listed_at: None,
            last_exiting: false,
            left: None,
            lost: None,
        }
    }

    /// Set the bundled/installed `g3d-termd` version string (`"g3d-termd/0.2.0"`)
    /// so [`Self::upgrade_available`] can detect a running older daemon.
    pub fn set_bundled_version(&mut self, bundled: impl Into<String>) {
        self.bundled = bundled.into();
    }

    pub const fn state(&self) -> TermdState {
        self.state
    }

    pub const fn since_ms(&self) -> u64 {
        self.since_ms
    }

    pub fn namespace(&self) -> &str {
        if !self.ns.is_empty() {
            &self.ns
        } else {
            self.doc
                .as_ref()
                .and_then(|d| d.daemon.ns.as_deref())
                .unwrap_or("")
        }
    }

    pub const fn doc(&self) -> Option<&StatusDoc> {
        self.doc.as_ref()
    }

    /// Whether a cached `doc` is being held across a brain outage (`state != Ok`).
    pub const fn is_stale(&self) -> bool {
        !matches!(self.state, TermdState::Ok) && self.doc.is_some()
    }

    /// Live terminals in the current (or retained) status document.
    pub fn terminals(&self) -> &[StatusTerminal] {
        self.doc
            .as_ref()
            .map_or(&[], |doc| doc.terminals.as_slice())
    }

    /// Look up one terminal's status row by `id` (`TermdStatus.terminal(id)`).
    pub fn terminal(&self, id: &str) -> Option<&StatusTerminal> {
        let idx = *self.by_id.get(id)?;
        self.doc.as_ref()?.terminals.get(idx)
    }

    /// Which way the namespace left on purpose, while nothing has listened there
    /// since (`None` once a daemon answers again, and for any other end).
    pub const fn left(&self) -> Option<ExitReason> {
        self.left
    }

    /// Sticky lost-namespace latch (`Some` once [`TermdState::Lost`] occurred
    /// until [`Self::dismiss_lost`]).
    pub const fn lost(&self) -> Option<&LostLatch> {
        self.lost.as_ref()
    }

    /// Shipped `g3d-termd` version when it differs from the running daemon's
    /// `daemon.version` (`TermdStatus.upgradeAvailable`).
    pub fn upgrade_available(&self) -> Option<&str> {
        let run = self.doc.as_ref()?.daemon.version.as_str();
        if !self.bundled.is_empty() && !run.is_empty() && self.bundled != run {
            Some(self.bundled.as_str())
        } else {
            None
        }
    }

    /// Required input ops (`"key"`, `"paste"`) missing from the currently
    /// answering brain (`TermdStatus.lacks`; empty whenever `state != Ok`).
    pub fn lacks(&self) -> &[String] {
        if self.state == TermdState::Ok {
            &self.lacks
        } else {
            &[]
        }
    }

    // ── Direct native socket lifecycle hooks ─────────────────────────────────

    /// Record a `WELCOME` handshake from `g3d-termd` (`instance` and advertised
    /// `features`), computing whether `key` or `paste` is missing.
    pub fn on_welcome<S: AsRef<str>>(&mut self, instance: &str, features: &[S], _now_ms: u64) {
        self.instance = Some(instance.to_string());
        self.lacks = REQUIRED_INPUT_OPS
            .iter()
            .filter(|&&op| !features.iter().any(|f| f.as_ref() == op))
            .map(|&op| op.to_string())
            .collect();
    }

    /// Ingest a raw `serde_json::Value` status event from `g3d-termd`.
    pub fn on_status_json(&mut self, value: Value, now_ms: u64) -> Result<(), String> {
        let doc = StatusDoc::from_value(value)?;
        self.on_status_doc(doc, now_ms);
        Ok(())
    }

    /// Ingest a parsed [`StatusDoc`], transitioning to [`TermdState::Ok`].
    pub fn on_status_doc(&mut self, doc: StatusDoc, now_ms: u64) {
        if self.ns.is_empty() {
            if let Some(ns) = &doc.daemon.ns {
                self.ns = ns.clone();
            }
        }
        // A namespace's last document says `exiting` and lists nothing: what it
        // held is what the document before it listed.
        if !doc.daemon.exiting {
            self.had_terminals = !doc.terminals.is_empty();
        }
        if !doc.terminals.is_empty() {
            self.shells_listed_at = Some(now_ms);
        }
        self.last_exiting = doc.daemon.exiting;
        self.left = doc
            .daemon
            .exiting
            .then(|| ExitReason::parse(doc.daemon.exit_reason.as_deref()));
        if self.state != TermdState::Ok {
            self.state = TermdState::Ok;
            self.since_ms = now_ms;
        }
        self.set_doc(Some(doc));
    }

    /// The connection ended for good (`termd_client::Event::Disconnected`).
    ///
    /// A namespace that left on purpose (`stop`, the holder's `SIGTERM`: protocol 1.6,
    /// "A namespace that leaves") with shells in it is an outage, shown at once:
    /// [`TermdState::Lost`], latched with which way it left, with no wait for a
    /// reconnect to fail. The wall follows the status document, so its notice of the
    /// leaving is the last document's `daemon.exiting` and `daemon.exit_reason`, not
    /// an `ERROR`. An empty namespace leaving stays [`TermdState::Absent`], and the
    /// idle exit always does: its rule is that the namespace held nothing, so
    /// nothing was lost whatever a document listed a moment before. Any other end
    /// is [`Self::on_disconnected`]'s.
    pub fn on_gone(&mut self, now_ms: u64) {
        let shells_lost = self.had_terminals
            || self
                .shells_listed_at
                .is_some_and(|at| now_ms.saturating_sub(at) <= SHELLS_LISTED_RECENTLY_MS);
        if self.last_exiting && shells_lost && self.left != Some(ExitReason::Idle) {
            self.transition(TermdState::Lost, false, now_ms);
            self.had_terminals = false;
            self.shells_listed_at = None;
            let left = self.left;
            if let Some(latch) = &mut self.lost {
                if latch.left.is_none() {
                    latch.left = left;
                }
            }
        } else {
            self.on_disconnected(false, now_ms);
        }
    }

    /// Handle the status socket closing: if the daemon's last document reported
    /// `exiting: true`, transitions cleanly to [`TermdState::Absent`]; otherwise
    /// enters [`TermdState::Restarting`] while keeping the last [`StatusDoc`].
    pub fn on_disconnected(&mut self, _planned_restart: bool, now_ms: u64) {
        if self.last_exiting {
            self.transition(TermdState::Absent, false, now_ms);
        } else {
            self.enter_restarting(now_ms);
        }
    }

    /// Handle `ENOENT` or `ECONNREFUSED` when dialling the namespace socket:
    /// enters [`TermdState::Lost`] (and latches `self.lost`) if the last
    /// non-exiting document held live terminals, else [`TermdState::Absent`].
    pub fn on_socket_absent(&mut self, now_ms: u64) {
        let next = if self.state == TermdState::Lost
            || (self.had_terminals && !self.last_exiting && self.state != TermdState::Absent)
        {
            TermdState::Lost
        } else {
            TermdState::Absent
        };
        self.transition(next, false, now_ms);
    }

    /// Handle a socket connection accepted by the holder whose brain has not
    /// yet answered `WELCOME` (a restart in progress).
    pub fn on_holder_waiting(&mut self, now_ms: u64) {
        self.enter_restarting(now_ms);
    }

    /// Handle `ERROR {"code": "brain_failed"}` from the holder: shells are
    /// alive and frozen while the brain is down. Keeps the last [`StatusDoc`].
    pub fn on_brain_failed(&mut self, now_ms: u64) {
        self.transition(TermdState::Failed, true, now_ms);
    }

    /// Handle an unsafe or inaccessible socket directory (`EACCES` / bad owner).
    pub fn on_socket_unreachable(&mut self, now_ms: u64) {
        self.transition(TermdState::Unreachable, false, now_ms);
    }

    /// Escalate [`TermdState::Restarting`] to [`TermdState::Unreachable`] once
    /// `now_ms - since_ms >= grace_ms` (pass [`DEFAULT_RESTART_GRACE_MS`]).
    /// Returns `true` if the state flipped.
    pub fn tick_grace(&mut self, now_ms: u64, grace_ms: u64) -> bool {
        if self.state == TermdState::Restarting
            && now_ms.saturating_sub(self.since_ms) >= grace_ms
        {
            self.transition(TermdState::Unreachable, true, now_ms);
            return true;
        }
        false
    }

    /// Apply a combined status update (`TermdStatus.update({ relay, status })`).
    pub fn update(&mut self, meta: StatusViewMeta, doc: Option<StatusDoc>) {
        self.ns = meta.ns;
        self.bundled = meta.bundled;
        self.state = meta.state;
        self.since_ms = meta.since;
        self.lacks = meta.lacks;
        if let Some(d) = &doc {
            if !d.daemon.exiting {
                self.had_terminals = !d.terminals.is_empty();
            }
            self.last_exiting = d.daemon.exiting;
            self.left = d
                .daemon
                .exiting
                .then(|| ExitReason::parse(d.daemon.exit_reason.as_deref()));
        }
        self.set_doc(doc);
        if self.state == TermdState::Lost && self.lost.is_none() {
            self.lost = Some(LostLatch {
                ns: self.ns.clone(),
                at: self.since_ms,
                left: None,
            });
        }
    }

    /// Reset live state to [`TermdState::Unknown`] while keeping any latched
    /// `lost` notice (`TermdStatus.clear`).
    pub fn clear(&mut self) {
        self.state = TermdState::Unknown;
        self.lacks.clear();
        self.set_doc(None);
    }

    /// Clear the sticky `lost` notice (`TermdStatus.dismissLost`). Returns
    /// `true` if a latch was active.
    pub fn dismiss_lost(&mut self) -> bool {
        self.lost.take().is_some()
    }

    fn enter_restarting(&mut self, now_ms: u64) {
        if matches!(
            self.state,
            TermdState::Restarting | TermdState::Unreachable | TermdState::Failed
        ) || (self.state == TermdState::Absent && self.last_exiting)
        {
            return;
        }
        self.transition(TermdState::Restarting, true, now_ms);
    }

    fn transition(&mut self, next: TermdState, keep_doc: bool, now_ms: u64) {
        if self.state != next {
            self.state = next;
            self.since_ms = now_ms;
        }
        if !keep_doc {
            self.set_doc(None);
        }
        if next == TermdState::Lost && self.lost.is_none() {
            self.lost = Some(LostLatch {
                ns: self.ns.clone(),
                at: self.since_ms,
                left: None,
            });
        }
    }

    fn set_doc(&mut self, doc: Option<StatusDoc>) {
        self.by_id.clear();
        if let Some(d) = &doc {
            for (idx, term) in d.terminals.iter().enumerate() {
                self.by_id.insert(term.id.clone(), idx);
            }
        }
        self.doc = doc;
    }
}

// ── Formatting & pure UI models from wall/StatusWindow.jsx ───────────────────

/// Semantic colour tone used by the topbar chip, notice banner, and status rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StatusTone {
    Ok,
    Muted,
    Amber,
    Red,
}

impl StatusTone {
    /// CSS hex colour matching `C` in `wall/StatusWindow.jsx`.
    pub const fn hex(self) -> &'static str {
        match self {
            StatusTone::Ok => "#6ee7a0",
            StatusTone::Muted => "#6b7383",
            StatusTone::Amber => "#e3b341",
            StatusTone::Red => "#e5534b",
        }
    }
}

/// Topbar chip presentation (`chipLook` in `StatusWindow.jsx`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChipLook {
    pub mark: &'static str,
    pub tone: StatusTone,
    pub text: String,
}

/// Compute the topbar `termd` chip's mark, colour tone, and text (`chipLook`).
pub fn chip_look(status: &TermdStatus) -> ChipLook {
    let count = status.terminals().len();
    match status.state() {
        TermdState::Ok => ChipLook {
            mark: "●",
            tone: StatusTone::Ok,
            text: format!("termd {count}"),
        },
        TermdState::Absent => ChipLook {
            mark: "○",
            tone: StatusTone::Muted,
            text: "termd idle".to_string(),
        },
        TermdState::Restarting => ChipLook {
            mark: "●",
            tone: StatusTone::Amber,
            text: "termd restarting".to_string(),
        },
        TermdState::Unknown => ChipLook {
            mark: "○",
            tone: StatusTone::Muted,
            text: "termd".to_string(),
        },
        other => ChipLook {
            mark: "●",
            tone: StatusTone::Red,
            text: format!("termd {}", other.as_str()),
        },
    }
}

/// Which condition triggered a [`StatusNotice`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoticeKind {
    Lost,
    Lacks,
    Restarting,
    Unreachable,
    Failed,
}

impl NoticeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            NoticeKind::Lost => "lost",
            NoticeKind::Lacks => "lacks",
            NoticeKind::Restarting => "restarting",
            NoticeKind::Unreachable => "unreachable",
            NoticeKind::Failed => "failed",
        }
    }
}

/// Banner notice rendered under the topbar when `termd` needs attention (`noticeFor`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusNotice {
    pub kind: NoticeKind,
    pub tone: StatusTone,
    pub text: String,
    pub dismiss: bool,
}

/// Decide whether to display a topbar `termd` notice (`noticeFor` in `StatusWindow.jsx`).
///
/// `since_ms` is when the current brain outage started (`status.since_ms()`),
/// `now_ms` is the current wall-clock time in milliseconds, and
/// `restart_notice_ms` is the grace before a brief brain restart shows an amber
/// banner (`WALL_CONFIG.restartNoticeMs`, default `400`).
pub fn notice_for(
    status: &TermdStatus,
    since_ms: u64,
    now_ms: u64,
    restart_notice_ms: u64,
) -> Option<StatusNotice> {
    if let Some(lost) = status.lost() {
        return Some(StatusNotice {
            kind: NoticeKind::Lost,
            tone: StatusTone::Red,
            dismiss: true,
            text: match lost.left {
                // The namespace said it was leaving, and which way: its shells were
                // ended with it, not lost to a crash.
                Some(left) => format!(
                    "{}: the shells in namespace \"{}\" ended with it · a new terminal starts a fresh termd",
                    left.phrase(),
                    lost.ns
                ),
                None => format!(
                    "termd lost: the shells in namespace \"{}\" are gone · a new terminal starts a fresh termd",
                    lost.ns
                ),
            },
        });
    }
    let lacks = status.lacks();
    if !lacks.is_empty() {
        return Some(StatusNotice {
            kind: NoticeKind::Lacks,
            tone: StatusTone::Red,
            dismiss: false,
            text: format!(
                "typing can't reach termd: its engine has no {} op (protocol 1.2) · g3d upgrade moves it to the relay's, every shell kept",
                lacks.join(" or ")
            ),
        });
    }
    let elapsed = now_ms.saturating_sub(since_ms);
    match status.state() {
        TermdState::Restarting => (elapsed >= restart_notice_ms).then(|| StatusNotice {
            kind: NoticeKind::Restarting,
            tone: StatusTone::Amber,
            dismiss: false,
            text: "termd's engine is restarting · the shells are alive".to_string(),
        }),
        TermdState::Unreachable => Some(StatusNotice {
            kind: NoticeKind::Unreachable,
            tone: StatusTone::Red,
            dismiss: false,
            text: format!("termd not answering · {} · retrying", format_span(elapsed)),
        }),
        TermdState::Failed => Some(StatusNotice {
            kind: NoticeKind::Failed,
            tone: StatusTone::Red,
            dismiss: false,
            text: "termd's engine is down — it keeps crashing · the shells are alive, frozen · g3d upgrade retries it now".to_string(),
        }),
        TermdState::Ok | TermdState::Absent | TermdState::Unknown | TermdState::Lost => None,
    }
}

/// Format a byte count in decimal SI powers of 1000 (`512 B`, `3.1 KB`, `42 MB`),
/// matching `bytes(n)` in `StatusWindow.jsx` and `crates/g3d/src/render.rs`.
pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0usize;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else if v < 10.0 {
        format!("{v:.1} {}", UNITS[u])
    } else {
        format!("{} {}", v.round() as u64, UNITS[u])
    }
}

/// Format a duration in milliseconds as `45s`, `12m`, `3h 12m`, or `2d 4h` (`span`).
pub fn format_span(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}d {}h", s / 86_400, (s % 86_400) / 3600)
    }
}

/// Program basename from `argv[0]` with any leading login-shell `-` stripped (`program`).
pub fn program_name(argv: &[String]) -> String {
    let a0 = argv.first().map(String::as_str).unwrap_or("");
    let base = a0.rsplit('/').next().unwrap_or(a0);
    base.strip_prefix('-').unwrap_or(base).to_string()
}

/// Summarize a terminal's subscribers (`own · 2 watch`, or `"—"` when none; `subscribers`).
pub fn format_subscribers(subs: &[StatusSubscriber]) -> String {
    let count = |role: &str| subs.iter().filter(|s| s.role == role).count();
    let mut parts = Vec::new();
    if count("own") > 0 {
        parts.push("own".to_string());
    }
    let attach = count("attach");
    if attach > 0 {
        parts.push(format!("{attach} attach"));
    }
    let watch = count("watch");
    if watch > 0 {
        parts.push(format!("{watch} watch"));
    }
    if parts.is_empty() {
        "—".to_string()
    } else {
        parts.join(" · ")
    }
}

/// Format `name/version` as `name version` (`nv` in `StatusWindow.jsx`).
pub fn format_name_version(raw: &str) -> String {
    raw.replacen('/', " ", 1)
}

/// Attention (`loud`, amber) and secondary (`quiet`, muted) badges for one terminal (`notes`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalNotes {
    pub loud: Vec<String>,
    pub quiet: Vec<String>,
}

/// Compute `loud` and `quiet` diagnostic badges for a status terminal (`notes` in `StatusWindow.jsx`).
pub fn terminal_notes(t: &StatusTerminal) -> TerminalNotes {
    let mut loud = Vec::new();
    let mut quiet = Vec::new();
    if t.flow == "blocked" {
        loud.push("blocked".to_string());
    } else if t.flow == "paused" {
        quiet.push("paused".to_string());
    }
    if t.subscribers.iter().any(|s| s.lagging) {
        loud.push("lagging".to_string());
    }
    if t.input_queued > 0 {
        loud.push(format!("{} input queued", format_bytes(t.input_queued)));
    }
    if t.alt {
        quiet.push("alt".to_string());
    }
    if let Some(procs) = t.processes.filter(|&p| p > 1) {
        quiet.push(format!("{procs} processes"));
    }
    if let Some(restored) = t.restored.as_deref().filter(|s| !s.is_empty()) {
        if t.unseen > 0 {
            quiet.push(format!(
                "restored: {restored}, {} unseen",
                format_bytes(t.unseen)
            ));
        } else {
            quiet.push(format!("restored: {restored}"));
        }
    }
    TerminalNotes { loud, quiet }
}

/// Multi-line hover summary of a terminal and its subscribers (`rowTitle` in `StatusWindow.jsx`).
pub fn row_title(t: &StatusTerminal) -> String {
    let mut lines = vec![
        format!(
            "{}{}",
            t.argv.join(" "),
            if t.login { "  (login shell)" } else { "" }
        ),
        format!(
            "{} · pid {} · handle {} · {}×{}",
            t.id, t.pid, t.handle, t.cols, t.rows
        ),
        match t.pwd.as_deref() {
            Some(pwd) if !pwd.is_empty() && pwd != t.cwd => format!("cwd {} · now {pwd}", t.cwd),
            _ => format!("cwd {}", t.cwd),
        },
    ];
    if !t.title.is_empty() {
        lines.push(format!("title {}", t.title));
    }
    for s in &t.subscribers {
        let who = if s.client.is_empty() {
            format!("connection {}", s.conn)
        } else {
            s.client.clone()
        };
        let pid = s.pid.map_or_else(String::new, |p| format!(" (pid {p})"));
        let lagging = if s.lagging { " · lagging" } else { "" };
        let lags = if s.lags > 0 {
            format!(" · {} resync{}", s.lags, if s.lags == 1 { "" } else { "s" })
        } else {
            String::new()
        };
        let view = if s.view > 0 {
            format!(" · scrolled back {}", s.view)
        } else {
            String::new()
        };
        lines.push(format!(
            "{} · {who}{pid} · {} · {}{lagging}{lags}{view}",
            s.role, s.mode, s.flow
        ));
    }
    lines.join("\n")
}

/// Formatted terminal row for the status window (`Terminals` in `StatusWindow.jsx`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusTerminalRow {
    pub id: String,
    pub name: String,
    pub program: String,
    pub size: String,
    pub subscribers: String,
    pub activity: String,
    pub activity_live: bool,
    pub notes: TerminalNotes,
    pub tooltip: String,
    /// Verb executed when clicking this row (`Some(["wall.reveal", id])` when on the wall).
    pub click_cmd: Option<Vec<String>>,
}

/// Build the status window's terminal rows (`Terminals` in `StatusWindow.jsx`).
/// `wall_name_of(id)` returns `Some(display_name)` when terminal `id` is on the wall,
/// or `None` when it is an external session in the same `termd` namespace.
pub fn terminal_rows<F>(
    status: &TermdStatus,
    now_ms: u64,
    mut wall_name_of: F,
) -> Vec<StatusTerminalRow>
where
    F: FnMut(&str) -> Option<String>,
{
    status
        .terminals()
        .iter()
        .map(|t| {
            let wall_name = wall_name_of(&t.id);
            let on_wall = wall_name.is_some();
            let name = wall_name.unwrap_or_else(|| {
                if t.title.is_empty() {
                    t.id.clone()
                } else {
                    t.title.clone()
                }
            });
            let (activity, activity_live) = if t.out_bps > 0 {
                (format!("{}/s", format_bytes(t.out_bps)), true)
            } else if let Some(last) = t.last_output_at {
                (
                    format!("idle {}", format_span(now_ms.saturating_sub(last))),
                    false,
                )
            } else {
                (String::new(), false)
            };
            StatusTerminalRow {
                id: t.id.clone(),
                name,
                program: program_name(&t.argv),
                size: format!("{}×{}", t.cols, t.rows),
                subscribers: format_subscribers(&t.subscribers),
                activity,
                activity_live,
                notes: terminal_notes(t),
                tooltip: row_title(t),
                click_cmd: on_wall.then(|| vec!["wall.reveal".to_string(), t.id.clone()]),
            }
        })
        .collect()
}

/// Empty-daemon message shown in the status window header when `doc` is `None` (`Daemon`).
pub fn empty_daemon_message(status: &TermdStatus) -> String {
    let why = match (status.state(), status.left()) {
        // It said which way it left: say that, where "no termd running" would leave a
        // person to guess.
        (TermdState::Absent | TermdState::Lost, Some(left)) => {
            format!("{} — the next terminal starts one", left.phrase())
        }
        (TermdState::Absent, None) => {
            "no termd running in this namespace — the next terminal starts one".to_string()
        }
        (TermdState::Unknown, _) => "no word from termd yet".to_string(),
        (TermdState::Lost, None) => "the namespace is gone".to_string(),
        (other, _) => format!("termd {}", other.as_str()),
    };
    let ns = status.namespace();
    if ns.is_empty() {
        why
    } else {
        format!("{why} · ns {ns}")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn brain_without_key_and_paste_ops_reports_lacks_notice_matching_wall_panels_test() {
        // Ported from tools/wall-panels.test.mjs:
        // "a brain without the key and paste ops says typing can't reach it"
        let mut status = TermdStatus::new("n");
        let doc: StatusDoc = serde_json::from_value(json!({
            "daemon": { "version": "g3d-termd/0.1.0" },
            "terminals": []
        }))
        .unwrap();
        status.update(
            StatusViewMeta {
                state: TermdState::Ok,
                since: 0,
                ns: "n".to_string(),
                bundled: String::new(),
                lacks: vec!["key".to_string(), "paste".to_string()],
            },
            Some(doc),
        );
        assert_eq!(status.lacks(), &["key".to_string(), "paste".to_string()]);
        let notice = notice_for(&status, 0, 0, DEFAULT_RESTART_NOTICE_MS).unwrap();
        assert_eq!(notice.kind, NoticeKind::Lacks);
        assert!(notice.text.contains("key or paste"));
        assert!(notice.text.contains("g3d upgrade"));

        // A current brain, or none answering: no lacks notice.
        status.update(
            StatusViewMeta {
                state: TermdState::Ok,
                since: 0,
                ns: "n".to_string(),
                bundled: String::new(),
                lacks: Vec::new(),
            },
            Some(StatusDoc::default()),
        );
        assert_eq!(notice_for(&status, 0, 0, DEFAULT_RESTART_NOTICE_MS), None);

        status.update(
            StatusViewMeta {
                state: TermdState::Restarting,
                since: 0,
                ns: "n".to_string(),
                bundled: String::new(),
                lacks: vec!["key".to_string()],
            },
            None,
        );
        assert!(status.lacks().is_empty());
    }

    #[test]
    fn protocol_example_document_parses_and_formats_rows_chip_and_notes() {
        // Mirrors the full status JSON example in docs/termd/PROTOCOL.md § status.
        let raw: serde_json::Value = serde_json::from_str(
            r#"{
            "ev": "status",
            "at": 1794519127407,
            "daemon": {
                "version": "g3d-termd/0.2.0",
                "engine": "libghostty-vt/1.0.1",
                "proto": [1, 5],
                "ns": "rs-wall",
                "socket": "/tmp/g3d-501/termd-rs-wall.sock",
                "log": "/Users/user/.local/state/g3d/termd-rs-wall.log",
                "exiting": false,
                "instance": "8c1f0e7a9b2d4c55",
                "started_at": 1794518713751,
                "connections": 4,
                "holder": {"pid": 41190, "version": "g3d-termd/0.2.0", "hold": [2, 2], "rss": 3145728, "upgrades": 1},
                "brain": {
                    "pid": 41200, "generation": 2, "started_at": 1794519100000, "rss": 41943040,
                    "restarts": {"crash": 1, "watchdog": 0, "upgrade": 0},
                    "last_start": {"reason": "crash", "at": 1794519100000, "unseen": 512}
                },
                "snapshots": {"interval_ms": 2000, "bytes": 1835008},
                "env": {"source": "launchd", "config": {"path": "/tmp/termd.toml", "state": "loaded"}, "warnings": []},
                "trees": {"mode": "session"},
                "leftovers": [{"id": "t3", "pid": 41260, "since": 1794519000000, "processes": [{"pid": 41266, "comm": "sleep"}]}]
            },
            "terminals": [{
                "id": "t1", "handle": 7, "pid": 41234, "argv": ["/bin/-zsh"], "login": true,
                "cwd": "/srv/src", "pwd": "/srv/src/app", "title": "~/src/app", "cols": 120, "rows": 40,
                "alt": true, "created_at": 1794480000123, "seq": 18211, "saved_seq": 17904,
                "out_bps": 1253000, "in_bps": 12, "last_output_at": 1794519127390,
                "flow": "blocked", "input_queued": 2048, "scrollback_rows": 1567,
                "memory": {"resident": 1986560, "compressed": 0},
                "restored": "snapshot", "unseen": 512, "processes": 3,
                "subscribers": [
                    {"conn": 12, "client": "glyph3d-native/0.1", "pid": 5011, "role": "own", "mode": "raw", "flow": "block", "queued": 0, "lagging": false, "lags": 0, "view": 0},
                    {"conn": 15, "client": "g3d/0.2", "role": "watch", "mode": "raw", "flow": "lossy", "queued": 100, "lagging": true, "lags": 2, "view": 10}
                ]
            }],
            "errors": [{
                "code": "brain_crash",
                "message": "brain exited on SIGSEGV",
                "first_at": 1794519099900,
                "at": 1794519099900,
                "count": 1
            }]
        }"#,
        )
        .unwrap();

        let mut status = TermdStatus::new("rs-wall");
        status.set_bundled_version("g3d-termd/0.3.0");
        status.on_welcome("8c1f0e7a9b2d4c55", &["key", "paste", "mouse"], 100);
        status.on_status_json(raw, 200).unwrap();

        assert_eq!(status.state(), TermdState::Ok);
        assert!(status.lacks().is_empty());
        assert_eq!(status.upgrade_available(), Some("g3d-termd/0.3.0"));
        assert!(status.terminal("t1").is_some());
        assert_eq!(
            chip_look(&status),
            ChipLook {
                mark: "●",
                tone: StatusTone::Ok,
                text: "termd 1".to_string(),
            }
        );

        let rows = terminal_rows(&status, 1794519127407, |id| {
            (id == "t1").then(|| "main shell".to_string())
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "main shell");
        assert_eq!(rows[0].program, "zsh");
        assert_eq!(rows[0].size, "120×40");
        assert_eq!(rows[0].subscribers, "own · 1 watch");
        assert_eq!(rows[0].activity, "1.3 MB/s");
        assert!(rows[0].activity_live);
        assert_eq!(
            rows[0].notes.loud,
            vec!["blocked", "lagging", "2.0 KB input queued"]
        );
        assert_eq!(
            rows[0].notes.quiet,
            vec!["alt", "3 processes", "restored: snapshot, 512 B unseen"]
        );
        assert_eq!(
            rows[0].click_cmd,
            Some(vec!["wall.reveal".to_string(), "t1".to_string()])
        );
        assert!(rows[0].tooltip.contains("now /srv/src/app"));
        assert!(rows[0].tooltip.contains("2 resyncs · scrolled back 10"));
    }

    #[test]
    fn native_socket_transitions_cover_absent_restarting_grace_failed_exiting_and_lost() {
        // Mirrors TestTermdStatusFollowsEveryState in cli/termdstatus_test.go.
        let mut status = TermdStatus::new("rs-wall");
        assert_eq!(status.state(), TermdState::Unknown);

        // Before any daemon listens: absent, doc is None, chip says idle.
        status.on_socket_absent(1_000);
        assert_eq!(status.state(), TermdState::Absent);
        assert!(status.doc().is_none());
        assert_eq!(chip_look(&status).text, "termd idle");

        // Daemon starts with one terminal: ok.
        status.on_welcome("inst-1", &["key", "paste"], 2_000);
        status
            .on_status_json(
                json!({
                    "daemon": {"version": "g3d-termd/0.2.0", "exiting": false},
                    "terminals": [{"id": "t1", "argv": ["zsh"], "cols": 80, "rows": 24}]
                }),
                2_000,
            )
            .unwrap();
        assert_eq!(status.state(), TermdState::Ok);

        // Brain crashes (disconnect without exiting): restarting keeps doc; no notice before 400 ms,
        // amber notice at 400 ms, escalates to unreachable at 5000 ms.
        status.on_disconnected(false, 3_000);
        assert_eq!(status.state(), TermdState::Restarting);
        assert!(status.is_stale());
        assert_eq!(status.terminals().len(), 1);
        assert_eq!(notice_for(&status, status.since_ms(), 3_200, 400), None);
        assert_eq!(
            notice_for(&status, status.since_ms(), 3_400, 400)
                .unwrap()
                .kind,
            NoticeKind::Restarting
        );
        assert!(status.tick_grace(8_000, DEFAULT_RESTART_GRACE_MS));
        assert_eq!(status.state(), TermdState::Unreachable);
        let unreach = notice_for(&status, status.since_ms(), 53_000, 400).unwrap();
        assert_eq!(unreach.kind, NoticeKind::Unreachable);
        assert!(unreach.text.contains("45s"));

        // Holder reports brain_failed: failed notice, shells frozen.
        status.on_brain_failed(60_000);
        assert_eq!(status.state(), TermdState::Failed);
        assert_eq!(
            notice_for(&status, status.since_ms(), 60_000, 400)
                .unwrap()
                .kind,
            NoticeKind::Failed
        );

        // Clean daemon stop (`exiting: true` with 0 terminals, then disconnect/ENOENT): absent, NOT lost.
        status
            .on_status_json(
                json!({"daemon": {"exiting": true}, "terminals": []}),
                70_000,
            )
            .unwrap();
        status.on_disconnected(false, 70_010);
        assert_eq!(status.state(), TermdState::Absent);
        assert!(status.lost().is_none());

        // Holder killed while holding a live terminal: lost, latched across a new daemon until dismissed.
        status
            .on_status_json(
                json!({"daemon": {"exiting": false}, "terminals": [{"id": "u"}]}),
                80_000,
            )
            .unwrap();
        status.on_socket_absent(81_000);
        assert_eq!(status.state(), TermdState::Lost);
        assert!(status.doc().is_none());
        let lost_notice = notice_for(&status, status.since_ms(), 81_000, 400).unwrap();
        assert_eq!(lost_notice.kind, NoticeKind::Lost);
        assert!(lost_notice.dismiss);

        // Even after a fresh daemon comes up `Ok`, the lost notice stays until `dismiss_lost()`.
        status
            .on_status_json(
                json!({"daemon": {"exiting": false}, "terminals": []}),
                85_000,
            )
            .unwrap();
        assert_eq!(status.state(), TermdState::Ok);
        assert_eq!(
            notice_for(&status, status.since_ms(), 85_000, 400)
                .unwrap()
                .kind,
            NoticeKind::Lost
        );
        assert!(status.dismiss_lost());
        assert_eq!(notice_for(&status, status.since_ms(), 85_000, 400), None);
    }

    #[test]
    fn a_namespace_that_leaves_on_purpose_with_shells_is_lost_at_once_saying_which_way() {
        let live = |ids: &[&str]| {
            let terminals: Vec<Value> = ids.iter().map(|id| json!({"id": id})).collect();
            json!({"daemon": {"ns": "rs-wall", "exiting": false}, "terminals": terminals})
        };
        // The namespace's last document, as protocol 1.6 sends it: `exiting`, which way
        // (`exit_reason`), and no terminals. `None` is a daemon from before the field.
        let exiting = |why: Option<&str>| {
            let mut doc = json!({"daemon": {"ns": "rs-wall", "exiting": true}, "terminals": []});
            if let Some(why) = why {
                doc["daemon"]["exit_reason"] = json!(why);
            }
            doc
        };
        let ended = "the shells in namespace \"rs-wall\" ended with it · a new terminal starts a fresh termd";

        // `g3d stop` with two quiet shells: the last document that listed them is 20 s
        // old, the namespace's last one lists nothing and says `exiting`, and the
        // connection ends for good. Lost on that instant, saying which way: no
        // restart grace, no wait for a reconnect to find the socket gone.
        for (why, phrase, left) in [
            (Some("stop"), "termd stopped", ExitReason::Stop),
            (Some("sigterm"), "termd ended by SIGTERM", ExitReason::Sigterm),
            // A word a later minor adds, and a daemon that says none: on purpose, unsaid.
            (Some("reboot"), "termd left on purpose", ExitReason::Unsaid),
            (None, "termd left on purpose", ExitReason::Unsaid),
        ] {
            let mut status = TermdStatus::default();
            status.on_status_json(live(&["t1", "t2"]), 10_000).unwrap();
            assert_eq!(status.left(), None);
            status.on_status_json(exiting(why), 30_000).unwrap();
            assert!(notice_for(&status, status.since_ms(), 30_000, 400).is_none());
            status.on_gone(30_001);
            assert_eq!(status.state(), TermdState::Lost, "{why:?}");
            let lost = notice_for(&status, status.since_ms(), 30_001, 400).expect("the outage notice");
            assert_eq!(lost.kind, NoticeKind::Lost);
            assert_eq!(lost.tone, StatusTone::Red);
            assert!(lost.dismiss);
            assert_eq!(lost.text, format!("{phrase}: {ended}"), "{why:?}");
            assert_eq!(status.lost().unwrap().left, Some(left));
            assert_eq!(
                empty_daemon_message(&status),
                format!("{phrase} — the next terminal starts one · ns rs-wall")
            );
            assert!(status.dismiss_lost());
            assert!(notice_for(&status, status.since_ms(), 31_000, 400).is_none());

            // A daemon answers again: what the last one left by is over.
            status.on_status_json(live(&["t9"]), 40_000).unwrap();
            assert_eq!(status.left(), None);
        }

        // A document that lists nothing may come between the terminals ending and the
        // `exiting` one (a stop takes about 0.2 s): the shells listed a moment ago
        // still went with the namespace.
        let mut status = TermdStatus::default();
        status.on_status_json(live(&["t1"]), 10_000).unwrap();
        status.on_status_json(live(&[]), 10_150).unwrap();
        status.on_status_json(exiting(Some("stop")), 10_200).unwrap();
        status.on_gone(10_201);
        assert_eq!(status.state(), TermdState::Lost);
        assert_eq!(status.lost().unwrap().left, Some(ExitReason::Stop));

        // The idle exit: the namespace has been empty for five minutes. Nothing was
        // lost, so no banner; the status window says what happened.
        let mut status = TermdStatus::default();
        status.on_status_json(live(&["t1"]), 10_000).unwrap();
        status.on_status_json(live(&[]), 12_000).unwrap();
        status.on_status_json(exiting(Some("idle")), 312_000).unwrap();
        status.on_gone(312_001);
        assert_eq!(status.state(), TermdState::Absent);
        assert!(status.lost().is_none());
        assert!(notice_for(&status, status.since_ms(), 312_001, 400).is_none());
        assert_eq!(
            empty_daemon_message(&status),
            "termd closed after idling — the next terminal starts one · ns rs-wall"
        );

        // `idle` is the daemon saying the namespace held nothing, so it is silent even
        // when a document listed a shell a moment ago (an idle time set very short).
        let mut status = TermdStatus::default();
        status.on_status_json(live(&["t1"]), 10_000).unwrap();
        status.on_status_json(live(&[]), 10_500).unwrap();
        status.on_status_json(exiting(Some("idle")), 11_000).unwrap();
        status.on_gone(11_001);
        assert_eq!(status.state(), TermdState::Absent);
        assert!(notice_for(&status, status.since_ms(), 11_001, 400).is_none());

        // A stop of a namespace that held nothing is no outage either.
        let mut status = TermdStatus::default();
        status.on_status_json(live(&[]), 10_000).unwrap();
        status.on_status_json(exiting(Some("stop")), 20_000).unwrap();
        status.on_gone(20_001);
        assert_eq!(status.state(), TermdState::Absent);
        assert!(notice_for(&status, status.since_ms(), 20_001, 400).is_none());
        assert_eq!(
            empty_daemon_message(&status),
            "termd stopped — the next terminal starts one · ns rs-wall"
        );

        // Any other end (no document said `exiting`) is a disconnect as before: a
        // restart to ride, until the socket proves absent.
        let mut status = TermdStatus::default();
        status.on_status_json(live(&["t1"]), 10_000).unwrap();
        status.on_gone(20_000);
        assert_eq!(status.state(), TermdState::Restarting);
        assert!(status.lost().is_none());
        status.on_socket_absent(20_500);
        assert_eq!(status.state(), TermdState::Lost);
        let plain = notice_for(&status, status.since_ms(), 20_500, 400).unwrap();
        assert_eq!(
            plain.text,
            "termd lost: the shells in namespace \"rs-wall\" are gone · a new terminal starts a fresh termd"
        );
        assert_eq!(empty_daemon_message(&status), "the namespace is gone · ns rs-wall");
    }

    #[test]
    fn formatting_helpers_match_g3d_status_conventions() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_253_000), "1.3 MB");
        assert_eq!(format_bytes(41_943_040), "42 MB");
        assert_eq!(format_span(45_000), "45s");
        assert_eq!(format_span(12 * 60_000), "12m");
        assert_eq!(format_span((3 * 3600 + 12 * 60) * 1000), "3h 12m");
        assert_eq!(format_span((2 * 86_400 + 4 * 3600) * 1000), "2d 4h");
        assert_eq!(program_name(&["/bin/bash".to_string()]), "bash");
        assert_eq!(program_name(&["-zsh".to_string()]), "zsh");
        assert_eq!(format_name_version("g3d-termd/0.2.0"), "g3d-termd 0.2.0");
    }
}
