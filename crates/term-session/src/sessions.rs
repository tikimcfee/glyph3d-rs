//! Window-facing [`Sessions`] collection and [`News`] events matching `native/src/wall/app.rs`'s
//! seam (`out/wall-port/briefs/S-term-session.md`).
//!
//! Each adopted or spawned terminal runs its [`glyph_vt::Vt`] emulator on its own background OS
//! thread ([`TerminalSession`]). On the window thread:
//! - [`Sessions::pump`] swaps finished, frame-coalesced [`Screen`] rows into each session's
//!   window-owned `front_screen` and emits [`News`] transitions (`Resized`, `Title`, `Modes`,
//!   `Ended`, `Disconnected`).
//! - [`Sessions::screen`] returns a plain `&Screen` borrow into that window-owned `front_screen`
//!   with zero locking, valid until the next [`Sessions::pump`].
//! - Input and control calls (`key`, `paste`, `mouse`, `scroll`, `resize`, `kill`, `repaint`)
//!   dispatch onto the hub's background IO worker so socket round-trips and `kill_grace` waits
//!   never stall the window's event loop.
//! - When `g3d-termd` stops (`DaemonEvent::Gone`), [`Sessions`] passively watches for a daemon
//!   returning on the same [`Target`] (`SessionHub::connect_with`, never restarting a stopped
//!   daemon behind the user's back) and surfaces the restored hub via [`Sessions::take_reconnected`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use term_cells::{Modes, Screen};
use termd_client::{
    ConnectOptions, Flow, Key, KeyAction, Mode, Mouse, MouseAction, MouseButton, OptionAsAlt,
    Role, Subscribe, Target, View,
};
use wall_core::keymap::{self, TerminalKey, TerminalKeyAction};
use wall_core::mouse::MouseAction as WallMouseAction;
use wall_core::pointer::{ProgramMouseButton, TerminalMouseEvent};

use crate::hub::{DaemonEvent, HubOptions, SessionHub, SpawnOptions};
use crate::session::{ExitInfo, ScrollbackState, TerminalSession};

/// The namespace the native wall keeps its terminals in (`rs-wall`, separate from the browser
/// wall's `wall` namespace so two owners do not fight over terminal sizes).
pub const NAMESPACE: &str = "rs-wall";

/// Reconnect grace window for brain crashes / `upgrade` handoffs before declaring `g3d-termd`
/// offline (`g3d-termd`'s brain restart delay is 100 ms and `upgrade` completes in < 200 ms, so
/// 2 s rides restarts comfortably while surfacing `g3d stop` promptly).
const WALL_RECONNECT_WINDOW: Duration = Duration::from_secs(2);

/// Passive poll cadence while waiting for an externally stopped `g3d-termd` to return.
const PASSIVE_RECONNECT_POLL: Duration = Duration::from_millis(300);

/// What happened to a terminal or the daemon since the last [`Sessions::pump`], for the wall's
/// state machine (`wall_core::wall::Wall`) to hear.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum News {
    /// Its screen was refilled by its emulator thread since the last [`Sessions::pump`].
    /// `rewritten_cols` holds per-row `(lo, hi)` in-place rewritten spans aligned to the
    /// published screen's row indices and trimmed of trailing `(0, 0)` rows (empty when no
    /// cells were rewritten in place).
    Filled {
        id: String,
        resized: bool,
        scrolled_top: u32,
        within_margins: bool,
        rewritten_cols: Vec<(u16, u16)>,
    },
    /// The terminal is now this size (its owner resized it).
    Resized { id: String, cols: u16, rows: u16 },
    /// The program updated its `OSC 0`/`2` window title.
    Title { id: String, title: String },
    /// The modes a host routes input by (`alt_screen`, `mouse`, `bracketed_paste`, …) changed.
    Modes { id: String, modes: Modes },
    /// Its program exited and the terminal closed: nothing more arrives.
    Ended { id: String },
    /// The daemon connection is gone for good.
    Disconnected { message: String },
}

struct TrackedSession {
    session: TerminalSession,
    /// `None` for primary-namespace terminals; `Some((extra_index, raw_termd_id))` for terminals
    /// attached from an extra namespace.
    extra_origin: Option<(usize, String)>,
    reported_size: (u16, u16),
    reported_osc_title: String,
    reported_modes: Modes,
    logged_exit: bool,
    logged_error: Option<(String, String)>,
}

impl TrackedSession {
    fn new(session: TerminalSession, extra_origin: Option<(usize, String)>) -> Self {
        let reported_size = (session.screen().cols, session.screen().rows);
        let reported_modes = session.modes();
        Self {
            session,
            extra_origin,
            reported_size,
            reported_osc_title: String::new(),
            reported_modes,
            logged_exit: false,
            logged_error: None,
        }
    }
}

struct ExtraNamespace {
    name: String,
    target: Target,
    hub: Option<SessionHub>,
    reconnected_hub: Arc<Mutex<Option<SessionHub>>>,
    watching_reconnect: Arc<AtomicBool>,
    detached_raw_ids: BTreeSet<String>,
}

/// Maps an extra namespace name and its raw `g3d-termd` terminal id onto a collision-free canvas
/// terminal id (`gx-<name>` for `gomuxai` `agent-<name>` sessions in namespace `"agents"`, else
/// `<ns>/<id>`).
pub fn wall_id_for_extra(ns: &str, raw_id: &str) -> String {
    if ns == "agents" {
        if let Some(agent_name) = raw_id.strip_prefix("agent-").filter(|s| !s.is_empty()) {
            let clean = agent_name.len() <= 61
                && agent_name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
            if clean {
                return format!("gx-{agent_name}");
            }
        }
    }
    format!("{ns}/{raw_id}")
}

/// All live terminals attached in one primary `g3d-termd` namespace (plus optional extra
/// namespaces such as `agents`), keyed by canvas terminal `id`, with one background emulator
/// thread per terminal.
pub struct Sessions {
    target: Target,
    options: Box<HubOptions>,
    wake: Arc<dyn Fn() + Send + Sync>,
    hub: SessionHub,
    extras: Vec<ExtraNamespace>,
    live: BTreeMap<String, TrackedSession>,
    external_appeared: BTreeSet<String>,
    detached_ended: Vec<String>,
    reconnected_hub: Arc<Mutex<Option<SessionHub>>>,
    watching_reconnect: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

impl Drop for Sessions {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

impl Sessions {
    /// Connects to `namespace`, starting its `g3d-termd` daemon if none runs. `wake` is called
    /// whenever a terminal publishes a coalesced screen update or a daemon lifecycle event arrives.
    pub fn open(namespace: &str, wake: Arc<dyn Fn() + Send + Sync>) -> Result<Self, String> {
        Self::open_with_extra(namespace, &[], wake)
    }

    /// Connects to primary `namespace` (starting `g3d-termd` if absent) and passively attaches to
    /// each extra namespace in `extra_namespaces` (never starting extra daemons; watching for them
    /// in the background if not yet running).
    pub fn open_with_extra(
        namespace: &str,
        extra_namespaces: &[String],
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, String> {
        let target = Target::namespace(namespace);
        let mut seen = BTreeSet::new();
        let mut extra_targets = Vec::new();
        for raw in extra_namespaces {
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed == namespace || !seen.insert(trimmed.to_owned()) {
                continue;
            }
            extra_targets.push((trimmed.to_owned(), Target::namespace(trimmed)));
        }
        Self::open_targets_with(&target, &extra_targets, default_wall_hub_options(), wake)
            .map_err(|e| format!("termd namespace `{namespace}`: {e}"))
    }

    /// Connects to an explicit [`Target`] (such as `Target::namespace_in` in scratch tests),
    /// starting `g3d-termd` if needed.
    pub fn open_target(target: &Target, wake: Arc<dyn Fn() + Send + Sync>) -> Result<Self, String> {
        Self::open_with(target, default_wall_hub_options(), wake)
            .map_err(|e| format!("termd target: {e}"))
    }

    /// Connects to an explicit primary [`Target`] plus named extra [`Target`]s (used by isolated
    /// multi-namespace scratch tests).
    pub fn open_targets(
        target: &Target,
        extra_targets: &[(String, Target)],
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, String> {
        Self::open_targets_with(target, extra_targets, default_wall_hub_options(), wake)
            .map_err(|e| format!("termd target: {e}"))
    }

    /// Connects to `target` with custom [`HubOptions`].
    pub fn open_with(
        target: &Target,
        options: HubOptions,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, termd_client::Error> {
        Self::open_targets_with(target, &[], options, wake)
    }

    /// Connects to `target` with custom [`HubOptions`] and optional passive extra targets.
    pub fn open_targets_with(
        target: &Target,
        extra_targets: &[(String, Target)],
        options: HubOptions,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, termd_client::Error> {
        let hub = SessionHub::open_with(target, options.clone(), Arc::clone(&wake))?;
        let _ = hub.status_follow(Some(500));

        let mut extras = Vec::with_capacity(extra_targets.len());
        for (name, extra_target) in extra_targets {
            let connected =
                SessionHub::connect_with(extra_target, options.clone(), Arc::clone(&wake)).ok();
            if let Some(ref ehub) = connected {
                let _ = ehub.status_follow(Some(500));
            }
            extras.push(ExtraNamespace {
                name: name.clone(),
                target: extra_target.clone(),
                hub: connected,
                reconnected_hub: Arc::new(Mutex::new(None)),
                watching_reconnect: Arc::new(AtomicBool::new(false)),
                detached_raw_ids: BTreeSet::new(),
            });
        }

        let sessions = Self {
            target: target.clone(),
            options: Box::new(options),
            wake,
            hub,
            extras,
            live: BTreeMap::new(),
            external_appeared: BTreeSet::new(),
            detached_ended: Vec::new(),
            reconnected_hub: Arc::new(Mutex::new(None)),
            watching_reconnect: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicBool::new(false)),
        };
        for idx in 0..sessions.extras.len() {
            if sessions.extras[idx].hub.is_none() {
                sessions.start_extra_reconnect_watcher(idx);
            }
        }
        Ok(sessions)
    }

    /// Underlying primary [`SessionHub`].
    pub fn hub(&self) -> &SessionHub {
        &self.hub
    }

    /// Underlying [`TerminalSession`] for `id`, if currently adopted.
    pub fn session(&self, id: &str) -> Option<&TerminalSession> {
        self.live.get(id).map(|t| &t.session)
    }

    /// Initial display title for `id` when adopted onto the wall: its program's `OSC 0`/`2` title
    /// if non-empty, else `<name>` for a `gomuxai` `gx-<name>` session from namespace `"agents"`,
    /// else `None` (so `Wall` falls back to `id`).
    pub fn default_title_for(&self, id: &str) -> Option<String> {
        if let Some(tracked) = self.live.get(id) {
            let osc = tracked.session.osc_title();
            if !osc.is_empty() {
                return Some(osc.to_owned());
            }
            if let Some((idx, ref raw_id)) = tracked.extra_origin {
                if self.extras[idx].name == "agents" {
                    if let Some(agent_name) =
                        raw_id.strip_prefix("agent-").filter(|s| !s.is_empty())
                    {
                        return Some(agent_name.to_owned());
                    }
                }
            }
        }
        None
    }

    fn resolve_extra_id(&self, wall_id: &str) -> Option<(usize, String)> {
        resolve_extra_id_in(&self.extras, wall_id)
    }

    /// If a stopped primary daemon has returned and our passive watcher connected to it, swaps in
    /// the new [`SessionHub`] and returns the sorted list of live terminal ids so the host can
    /// re-adopt them at their saved spots and clear its offline state.
    pub fn take_reconnected(&mut self) -> Option<Vec<String>> {
        let next_hub = self
            .reconnected_hub
            .lock()
            .expect("reconnected_hub mutex poisoned")
            .take()?;
        self.hub = next_hub;
        let ids = self.list().unwrap_or_default();
        Some(ids)
    }

    /// Drains terminal ids that appeared externally in the primary or any extra namespace
    /// (discovered via `status_follow` or extra-daemon connect) and are not yet adopted in
    /// `self.live`.
    pub fn take_external_appeared(&mut self) -> Vec<String> {
        let ids: Vec<String> = self
            .external_appeared
            .iter()
            .filter(|id| !self.live.contains_key(*id))
            .cloned()
            .collect();
        self.external_appeared.clear();
        ids
    }

    fn start_passive_reconnect_watcher(&self) {
        Self::spawn_reconnect_thread(
            "term-reconnect",
            self.target.clone(),
            (*self.options).clone(),
            Arc::clone(&self.wake),
            Arc::clone(&self.reconnected_hub),
            Arc::clone(&self.watching_reconnect),
            Arc::clone(&self.dropped),
        );
    }

    fn start_extra_reconnect_watcher(&self, idx: usize) {
        let extra = &self.extras[idx];
        Self::spawn_reconnect_thread(
            &format!("term-extra-{}", extra.name),
            extra.target.clone(),
            (*self.options).clone(),
            Arc::clone(&self.wake),
            Arc::clone(&extra.reconnected_hub),
            Arc::clone(&extra.watching_reconnect),
            Arc::clone(&self.dropped),
        );
    }

    fn spawn_reconnect_thread(
        thread_name: &str,
        target: Target,
        mut opts: HubOptions,
        wake: Arc<dyn Fn() + Send + Sync>,
        slot: Arc<Mutex<Option<SessionHub>>>,
        watching: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    ) {
        if dropped.load(Ordering::Acquire) {
            return;
        }
        if watching
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        opts.connect.reconnect_window = WALL_RECONNECT_WINDOW;

        let _ = thread::Builder::new()
            .name(thread_name.to_owned())
            .spawn(move || {
                while !dropped.load(Ordering::Acquire) {
                    thread::sleep(PASSIVE_RECONNECT_POLL);
                    if dropped.load(Ordering::Acquire) {
                        break;
                    }
                    if let Ok(new_hub) =
                        SessionHub::connect_with(&target, opts.clone(), Arc::clone(&wake))
                    {
                        let _ = new_hub.status_follow(Some(500));
                        *slot.lock().expect("reconnected_hub mutex poisoned") = Some(new_hub);
                        watching.store(false, Ordering::Release);
                        wake();
                        return;
                    }
                }
                watching.store(false, Ordering::Release);
            });
    }

    fn primary_list(&self) -> Result<Vec<String>, String> {
        let mut ids: Vec<String> = self
            .hub
            .list()
            .map_err(|e| format!("termd list: {e}"))?
            .into_iter()
            .map(|t| t.id)
            .collect();
        ids.sort();
        Ok(ids)
    }

    /// The ids of the terminals held by the primary daemon and any connected extra namespaces
    /// (excluding extra-namespace terminals the user explicitly detached on this wall), sorted.
    pub fn list(&self) -> Result<Vec<String>, String> {
        let mut ids = self.primary_list()?;
        for extra in &self.extras {
            let Some(ref ehub) = extra.hub else {
                continue;
            };
            if let Ok(terms) = ehub.list() {
                for t in terms {
                    if extra.detached_raw_ids.contains(&t.id) {
                        continue;
                    }
                    ids.push(wall_id_for_extra(&extra.name, &t.id));
                }
            }
        }
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    /// The ids of the terminals this `Sessions` instance is currently drawing.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.live.keys().map(String::as_str)
    }

    /// Takes terminal `id` as its size owner (`Role::Own`, with `Flow::Lossy` for extra-namespace
    /// terminals so a viewer never stalls an agent's PTY), spawns its dedicated emulator thread,
    /// and starts drawing it. Returns its `(cols, rows)`.
    pub fn adopt(&mut self, id: &str) -> Result<(u16, u16), String> {
        self.external_appeared.remove(id);
        if let Some((idx, raw_id)) = self.resolve_extra_id(id) {
            self.extras[idx].detached_raw_ids.remove(&raw_id);
            let ns_name = self.extras[idx].name.clone();
            let Some(ref ehub) = self.extras[idx].hub else {
                return Err(format!("extra termd namespace `{ns_name}` is not running"));
            };
            let session = ehub
                .attach_with_options(&Subscribe {
                    id: raw_id.clone(),
                    mode: Mode::Raw,
                    role: Role::Own,
                    history_rows: None,
                    flow: Some(Flow::Lossy),
                    cols: None,
                    rows: None,
                })
                .map_err(|e| format!("termd ({ns_name}) subscribe {raw_id}: {e}"))?;
            let dims = (session.screen().cols, session.screen().rows);
            self.live.insert(
                id.to_owned(),
                TrackedSession::new(session, Some((idx, raw_id))),
            );
            return Ok(dims);
        }

        let session = self
            .hub
            .attach_owner(id, None)
            .map_err(|e| format!("termd subscribe {id}: {e}"))?;
        let dims = (session.screen().cols, session.screen().rows);
        self.live
            .insert(id.to_owned(), TrackedSession::new(session, None));
        Ok(dims)
    }

    /// Starts a terminal (the account's login shell when `argv` is `None`) in the primary
    /// namespace under the first free `tN` id, pipelining `create` and `subscribe` in one socket
    /// write, and adopts it as size owner. Returns `(id, cols, rows)`.
    pub fn spawn(
        &mut self,
        argv: Option<Vec<String>>,
        cols: u16,
        rows: u16,
    ) -> Result<(String, u16, u16), String> {
        let taken = self.primary_list()?;
        let id = (1u64..)
            .map(|n| format!("t{n}"))
            .find(|cand| !taken.contains(cand) && !self.live.contains_key(cand))
            .expect("some tN is free");
        self.spawn_with(&SpawnOptions {
            argv,
            ..SpawnOptions::shell(id, cols, rows)
        })
    }

    /// Starts a terminal in the primary namespace with full [`SpawnOptions`] and adopts it.
    pub fn spawn_with(&mut self, opts: &SpawnOptions) -> Result<(String, u16, u16), String> {
        let session = self
            .hub
            .spawn(opts)
            .map_err(|e| format!("termd spawn {}: {e}", opts.id))?;
        let id = session.id().to_owned();
        let dims = (session.screen().cols, session.screen().rows);
        self.external_appeared.remove(&id);
        self.live
            .insert(id.clone(), TrackedSession::new(session, None));
        Ok((id, dims.0, dims.1))
    }

    /// Latest swapped-in [`Screen`] of terminal `id`, held directly on the window thread with zero
    /// locking until the next [`Self::pump`].
    pub fn screen(&self, id: &str) -> Option<&Screen> {
        self.live.get(id).map(|t| t.session.screen())
    }

    /// Stream selection text of a cell range of terminal `id` (`col` in `0..=cols`, end-exclusive).
    pub fn text(&self, id: &str, col0: u16, row0: u16, col1: u16, row1: u16) -> Option<String> {
        self.live
            .get(id)
            .map(|t| t.session.text(col0, row0, col1, row1))
    }

    /// Rectangular block-selection text of a cell range of terminal `id`.
    pub fn block_text(
        &self,
        id: &str,
        col0: u16,
        row0: u16,
        col1: u16,
        row1: u16,
    ) -> Option<String> {
        self.live
            .get(id)
            .map(|t| t.session.block_text(col0, row0, col1, row1))
    }

    /// Single trimmed visual row text of terminal `id`.
    pub fn row_text(&self, id: &str, row: u16) -> Option<String> {
        self.live.get(id).map(|t| t.session.row_text(row))
    }

    /// `OSC 8` hyperlink URI at `(col, row)` (`0`-based) on terminal `id`, if the cell carries one.
    pub fn link_at(&self, id: &str, col: u16, row: u16) -> Option<String> {
        self.live.get(id).and_then(|t| t.session.link_at(col, row))
    }

    /// Working directory last reported via `OSC 7` for terminal `id`.
    pub fn pwd(&self, id: &str) -> Option<&str> {
        self.live.get(id).and_then(|t| t.session.pwd())
    }

    /// Current terminal modes of `id`.
    pub fn modes(&self, id: &str) -> Option<Modes> {
        self.live.get(id).map(|t| t.session.modes())
    }

    /// Current scrollback view state (`offset`, `max`, `at_bottom`, `viewing`) of `id`.
    pub fn scrollback(&self, id: &str) -> Option<ScrollbackState> {
        self.live.get(id).map(|t| t.session.scrollback())
    }

    /// Cumulative bell count observed on terminal `id`.
    pub fn bell_count(&self, id: &str) -> u64 {
        self.live.get(id).map_or(0, |t| t.session.bell_count())
    }

    /// Unread bell count waiting to be drained on terminal `id`.
    pub fn unread_bells(&self, id: &str) -> u64 {
        self.live.get(id).map_or(0, |t| t.session.unread_bells())
    }

    /// Drains any unread bells rung on `id` since the previous call.
    pub fn take_bell(&mut self, id: &str) -> u64 {
        self.live.get_mut(id).map_or(0, |t| t.session.take_bell())
    }

    /// Drains unread bell counts across every live terminal, returning `(id, count)` for each
    /// terminal that rang at least once since the last drain.
    pub fn take_bells(&mut self) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        for (id, tracked) in &mut self.live {
            let n = tracked.session.take_bell();
            if n > 0 {
                out.push((id.clone(), n));
            }
        }
        out
    }

    /// Exit status of `id`, if its process has exited.
    pub fn exit_status(&self, id: &str) -> Option<&ExitInfo> {
        self.live.get(id).and_then(|t| t.session.exit_status())
    }

    /// Swaps in any coalesced screens published by session worker threads since the last call and
    /// collects [`News`] for the wall. Never parses VT bytes on the window thread.
    ///
    /// The returned `bool` is `true` iff at least one terminal's [`Screen`] changed.
    pub fn pump(&mut self) -> (bool, Vec<News>) {
        let mut news = Vec::new();
        for id in self.detached_ended.drain(..) {
            news.push(News::Ended { id });
        }
        let mut daemon_gone = false;

        for ev in self.hub.poll_daemon_events() {
            match ev {
                DaemonEvent::Reconnected { restart, .. } => {
                    log::info!("termd came back (restart: {restart}); its terminals repaint");
                }
                DaemonEvent::Gone { message, .. } => {
                    daemon_gone = true;
                    news.push(News::Disconnected { message });
                }
                DaemonEvent::UpgradeFailed { message } => {
                    log::warn!("termd upgrade failed: {message}");
                }
                DaemonEvent::Status(doc) => {
                    if let Some(terms) = doc.get("terminals").and_then(|v| v.as_array()) {
                        for item in terms {
                            let exited = item.get("exit").is_some_and(|e| !e.is_null());
                            if exited {
                                continue;
                            }
                            if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                                if let Some(tracked) = self.live.get_mut(id) {
                                    if let Some(sb_rows) =
                                        item.get("scrollback_rows").and_then(|v| v.as_u64())
                                    {
                                        tracked.session.note_status_scrollback(sb_rows);
                                    }
                                } else {
                                    self.external_appeared.insert(id.to_owned());
                                }
                            }
                        }
                    }
                }
            }
        }

        let mut extra_gone_indices = Vec::new();
        for idx in 0..self.extras.len() {
            let arrived_hub = self.extras[idx]
                .reconnected_hub
                .lock()
                .expect("reconnected_hub mutex poisoned")
                .take();
            if let Some(new_hub) = arrived_hub {
                log::info!("extra termd ({}) connected", self.extras[idx].name);
                if let Ok(terms) = new_hub.list() {
                    let alive_raw: BTreeSet<&str> = terms.iter().map(|t| t.id.as_str()).collect();
                    self.extras[idx]
                        .detached_raw_ids
                        .retain(|r| alive_raw.contains(r.as_str()));
                    let ns_name = self.extras[idx].name.clone();
                    for t in terms {
                        if self.extras[idx].detached_raw_ids.contains(&t.id) {
                            continue;
                        }
                        let wall_id = wall_id_for_extra(&ns_name, &t.id);
                        if !self.live.contains_key(&wall_id) {
                            self.external_appeared.insert(wall_id);
                        }
                    }
                }
                self.extras[idx].hub = Some(new_hub);
            }

            let events = match &self.extras[idx].hub {
                Some(ehub) => ehub.poll_daemon_events(),
                None => Vec::new(),
            };
            let ns_name = self.extras[idx].name.clone();
            let mut this_extra_gone = false;
            for ev in events {
                match ev {
                    DaemonEvent::Reconnected { restart, .. } => {
                        log::info!("extra termd ({ns_name}) came back (restart: {restart})");
                    }
                    DaemonEvent::Gone { message, .. } => {
                        log::info!("extra termd ({ns_name}) stopped: {message}");
                        this_extra_gone = true;
                    }
                    DaemonEvent::UpgradeFailed { message } => {
                        log::warn!("extra termd ({ns_name}) upgrade failed: {message}");
                    }
                    DaemonEvent::Status(doc) => {
                        if let Some(terms) = doc.get("terminals").and_then(|v| v.as_array()) {
                            let mut alive_raw = BTreeSet::new();
                            for item in terms {
                                let exited = item.get("exit").is_some_and(|e| !e.is_null());
                                if exited {
                                    continue;
                                }
                                if let Some(raw_id) = item.get("id").and_then(|v| v.as_str()) {
                                    alive_raw.insert(raw_id.to_owned());
                                    let wall_id = wall_id_for_extra(&ns_name, raw_id);
                                    if let Some(tracked) = self.live.get_mut(&wall_id) {
                                        if let Some(sb_rows) =
                                            item.get("scrollback_rows").and_then(|v| v.as_u64())
                                        {
                                            tracked.session.note_status_scrollback(sb_rows);
                                        }
                                    } else if !self.extras[idx].detached_raw_ids.contains(raw_id) {
                                        self.external_appeared.insert(wall_id);
                                    }
                                }
                            }
                            self.extras[idx]
                                .detached_raw_ids
                                .retain(|r| alive_raw.contains(r));
                        }
                    }
                }
            }
            if this_extra_gone {
                self.extras[idx].hub = None;
                extra_gone_indices.push(idx);
            }
        }

        let mut any_screen_changed = false;
        let mut closed_ids = Vec::new();

        for (id, tracked) in &mut self.live {
            let prev_gen = tracked.session.screen().generation;
            if !tracked.session.pump() {
                continue;
            }
            if tracked.session.screen().generation != prev_gen {
                any_screen_changed = true;
            }

            let dims = (tracked.session.screen().cols, tracked.session.screen().rows);
            if dims != tracked.reported_size {
                tracked.reported_size = dims;
                news.push(News::Resized {
                    id: id.clone(),
                    cols: dims.0,
                    rows: dims.1,
                });
            }

            if let Some(delta) = tracked.session.take_fill_delta() {
                news.push(News::Filled {
                    id: id.clone(),
                    resized: delta.resized,
                    scrolled_top: delta.scrolled_top,
                    within_margins: delta.within_margins,
                    rewritten_cols: delta.rewritten_cols,
                });
            }

            if tracked.session.osc_title() != tracked.reported_osc_title {
                tracked
                    .reported_osc_title
                    .clone_from(&tracked.session.osc_title().to_owned());
                news.push(News::Title {
                    id: id.clone(),
                    title: tracked.reported_osc_title.clone(),
                });
            }

            let modes = tracked.session.modes();
            if modes != tracked.reported_modes {
                tracked.reported_modes = modes;
                news.push(News::Modes {
                    id: id.clone(),
                    modes,
                });
            }

            if !tracked.logged_exit {
                if let Some(exit) = tracked.session.exit_status() {
                    tracked.logged_exit = true;
                    log::info!(
                        "terminal {id} ended (code {:?}, signal {:?})",
                        exit.code,
                        exit.signal
                    );
                }
            }

            if let Some((code, message)) = tracked.session.last_error() {
                let needs_log = tracked
                    .logged_error
                    .as_ref()
                    .is_none_or(|(c, m)| c != code || m != message);
                if needs_log {
                    tracked.logged_error = Some((code.to_owned(), message.to_owned()));
                    log::warn!("terminal {id}: {code}: {message}");
                }
            }

            if tracked.session.is_closed() {
                closed_ids.push(id.clone());
                news.push(News::Ended { id: id.clone() });
            }
        }

        for id in closed_ids {
            self.live.remove(&id);
        }

        for idx in extra_gone_indices {
            let lingering: Vec<String> = self
                .live
                .iter()
                .filter(|(_, t)| t.extra_origin.as_ref().is_some_and(|(i, _)| *i == idx))
                .map(|(id, _)| id.clone())
                .collect();
            for id in lingering {
                self.live.remove(&id);
                news.push(News::Ended { id });
            }
            self.start_extra_reconnect_watcher(idx);
        }

        if daemon_gone {
            // Any remaining primary-namespace sessions whose socket dropped at EOF before
            // `EVENT closed` arrived belong to the dead primary daemon: unmount them cleanly
            // (preserving their saved spots in `WallStore`, while leaving extra-namespace sessions
            // intact) and start passively watching for a daemon returning on `target`.
            let lingering: Vec<String> = self
                .live
                .iter()
                .filter(|(_, t)| t.extra_origin.is_none())
                .map(|(id, _)| id.clone())
                .collect();
            for id in &lingering {
                self.live.remove(id);
            }
            self.external_appeared
                .retain(|id| resolve_extra_id_in(&self.extras, id).is_some());
            for id in lingering {
                news.insert(0, News::Ended { id });
            }
            self.start_passive_reconnect_watcher();
        }

        (any_screen_changed, news)
    }

    /// Ends a primary-namespace terminal `id` (and its process tree) in `g3d-termd`, or — when
    /// `id` belongs to an extra namespace (`agents`, etc.) — detaches the wall's subscription
    /// without killing the running process (`--detach-on-kill` parity with `cli/gomux.go`).
    pub fn kill(&mut self, id: &str) {
        if let Some((idx, raw_id)) = self
            .live
            .get(id)
            .and_then(|t| t.extra_origin.clone())
            .or_else(|| self.resolve_extra_id(id))
        {
            self.extras[idx].detached_raw_ids.insert(raw_id);
            self.external_appeared.remove(id);
            if self.live.remove(id).is_some() {
                self.detached_ended.push(id.to_owned());
                (self.wake)();
            }
            return;
        }
        self.hub.send_kill(id);
    }

    /// Requests terminal `id` to resize to `cols` by `rows`; the new size arrives as
    /// [`News::Resized`].
    pub fn resize(&mut self, id: &str, cols: u16, rows: u16) {
        let Some(tracked) = self.live.get(id) else {
            return;
        }
        ;
        tracked.session.send_resize(cols, rows);
    }

    /// Sends a W3C-mapped [`TerminalKey`] to `id` (encoded by `g3d-termd` for the program's active
    /// key modes).
    pub fn key(&mut self, id: &str, k: &TerminalKey) {
        let Some(tracked) = self.live.get(id) else {
            return;
        };
        let key = Key {
            code: k.code.clone(),
            key: k.key.clone(),
            mods: k.mods.iter().map(|m| (*m).to_string()).collect(),
            text: k.text.clone(),
            action: Some(match k.action {
                TerminalKeyAction::Press => KeyAction::Press,
                TerminalKeyAction::Repeat => KeyAction::Repeat,
                TerminalKeyAction::Release => KeyAction::Release,
            }),
            option_as_alt: match k.option_as_alt {
                keymap::OptionAsAlt::Auto => OptionAsAlt::Auto,
                keymap::OptionAsAlt::True => OptionAsAlt::True,
                keymap::OptionAsAlt::False => OptionAsAlt::False,
            },
        };
        tracked.session.send_key(key);
    }

    /// Pastes `text` into terminal `id` (bracketed if the program requested DECSET 2004, streamed
    /// in chunks on a background worker so large pastes never stall the window thread).
    pub fn paste(&mut self, id: &str, text: &str) {
        let Some(tracked) = self.live.get(id) else {
            return;
        };
        tracked.session.send_paste(text.to_owned());
    }

    /// Sends a pointer event to a program listening for terminal mouse reporting on `id`.
    pub fn mouse(&mut self, id: &str, ev: &TerminalMouseEvent) {
        let Some(tracked) = self.live.get(id) else {
            return;
        };
        let m = Mouse {
            action: match ev.action {
                WallMouseAction::Press => MouseAction::Press,
                WallMouseAction::Release => MouseAction::Release,
                WallMouseAction::Move => MouseAction::Move,
            },
            button: match ev.button {
                ProgramMouseButton::Left => MouseButton::Left,
                ProgramMouseButton::Middle => MouseButton::Middle,
                ProgramMouseButton::Right => MouseButton::Right,
                ProgramMouseButton::Back => MouseButton::Extra(8),
                ProgramMouseButton::Forward => MouseButton::Extra(9),
                ProgramMouseButton::None => MouseButton::None,
            },
            col: ev.col,
            row: ev.row,
            mods: ev.mod_names().iter().map(|m| (*m).to_string()).collect(),
        };
        tracked.session.send_mouse(m);
    }

    /// Scrolls terminal `id` by `lines` at cell `(col, row)` (`lines < 0` scrolls up into history).
    /// On the primary screen this moves this subscriber's scrollback `view` in `g3d-termd` (and a
    /// zero delta jumps back to live output, bridging `wall.agent.jump <id> live`); on the
    /// alternate screen there is no scrollback, so wheel notches at `(col, row)` are sent to the
    /// program instead.
    pub fn scroll(&mut self, id: &str, lines: i32, col: f64, row: f64) {
        if lines == 0 {
            self.scroll_to_live(id);
            return;
        }
        let Some(tracked) = self.live.get(id) else {
            return;
        };
        if tracked.session.is_ended() {
            return;
        }
        if tracked.session.modes().alt_screen {
            let button = if lines < 0 {
                MouseButton::WheelUp
            } else {
                MouseButton::WheelDown
            };
            for _ in 0..lines.unsigned_abs().min(64) {
                tracked.session.send_mouse(Mouse {
                    action: MouseAction::Press,
                    button,
                    col,
                    row,
                    mods: Vec::new(),
                });
            }
        } else {
            tracked.session.send_view(View::Delta(-i64::from(lines)));
        }
    }

    /// Returns terminal `id`'s scrollback view immediately to live (`View::Live`, offset 0,
    /// `at_bottom = true`). No-op while the terminal is ended or showing the alternate screen.
    pub fn scroll_live(&mut self, id: &str) {
        let Some(tracked) = self.live.get(id) else {
            return;
        };
        if tracked.session.is_ended() || tracked.session.modes().alt_screen {
            return;
        }
        tracked.session.send_view(View::Live);
    }

    /// Alias for [`Self::scroll_live`] (`offset = 0`, `at_bottom = true`).
    pub fn scroll_to_live(&mut self, id: &str) {
        self.scroll_live(id);
    }

    /// Requests `g3d-termd` to send a fresh `REPAINT` for terminal `id`.
    pub fn repaint(&mut self, id: &str) {
        let Some(tracked) = self.live.get(id) else {
            return;
        };
        tracked.session.send_repaint();
    }
}

fn resolve_extra_id_in(extras: &[ExtraNamespace], wall_id: &str) -> Option<(usize, String)> {
    if let Some((ns, raw)) = wall_id.split_once('/') {
        if !raw.is_empty() {
            if let Some(idx) = extras.iter().position(|e| e.name == ns) {
                return Some((idx, raw.to_owned()));
            }
        }
    }
    if let Some(agent_name) = wall_id.strip_prefix("gx-").filter(|s| !s.is_empty()) {
        if let Some(idx) = extras.iter().position(|e| e.name == "agents") {
            return Some((idx, format!("agent-{agent_name}")));
        }
    }
    None
}

fn default_wall_hub_options() -> HubOptions {
    HubOptions {
        connect: ConnectOptions {
            reconnect_window: WALL_RECONNECT_WINDOW,
            ..ConnectOptions::default()
        },
        ..HubOptions::default()
    }
}
