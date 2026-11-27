//! Fleet follower: runs `agent export --json --follow --all`, folds its JSONL stream into the
//! fleet snapshot and host-ordered groups `wall_core::bus` expects, invokes a `wake` callback
//! only when the published fleet changes, and restarts a departed child process with exponential
//! backoff.
//!
//! Direct port of `glyph3d-js/cli/gomux.go` and the host/status
//! ordering of `wall/WallBus.js` (`crates/wall-core/src/bus.rs`).

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read as _};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cli::{parse_rfc3339_ms, parse_rfc3339_nanos, AgentCli};

/// One agent entry in a [`FleetSnapshot`] (`gomuxNode` in `cli/gomux.go`, matching
/// `wall_core::bus::GomuxAgent` field-for-field and in camelCase JSON).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetAgent {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub backend: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub data_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub created_at: String,
    #[serde(default)]
    pub live: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_mtime: Option<u64>,
}

impl FleetAgent {
    /// Whether `agent attach` can attach to this agent (`WallBus.busAttachable`).
    ///
    /// An omitted `attached` (`None`, when a remote host could not report it) is treated as
    /// attachable; only an explicit `Some(false)` refuses.
    pub fn attachable(&self) -> bool {
        self.attached != Some(false)
    }

    /// Whether this agent has a confirmed live local `termd` session (`gomuxNode.hasSession`).
    pub fn has_session(&self) -> bool {
        self.attached == Some(true)
    }

    /// Format the `agent attach` target (`name` or `name@host`, `WallBus.busTarget`).
    pub fn target(&self) -> String {
        match self.host.as_deref().filter(|h| !h.is_empty()) {
            Some(host) => format!("{}@{host}", self.name),
            None => self.name.clone(),
        }
    }

    /// Display priority rank for sorting within a host (`WallBus.BUS_STATUSES`).
    pub fn status_rank(&self) -> u8 {
        match self.status.as_deref() {
            Some("waiting") => 0,
            Some("error") => 1,
            Some("processing") => 2,
            Some("idle") => 3,
            Some("ended") => 5,
            _ => 4,
        }
    }
}

/// Live agent entry inside a [`FleetHostGroup`], carrying precomputed `target` and `attachable`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FleetHostAgent {
    pub agent: FleetAgent,
    pub target: String,
    pub attachable: bool,
}

impl std::ops::Deref for FleetHostAgent {
    type Target = FleetAgent;

    fn deref(&self) -> &Self::Target {
        &self.agent
    }
}

/// One host's sorted live agents (`WallBus.busHosts`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FleetHostGroup {
    pub host: String,
    pub agents: Vec<FleetHostAgent>,
}

/// Published fleet snapshot (`gomuxSnapshot` in `cli/gomux.go`, matching
/// `wall_core::bus::GomuxSnapshot`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetSnapshot {
    #[serde(default)]
    pub gen: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default)]
    pub dirs: Vec<String>,
    #[serde(default)]
    pub agents: Vec<FleetAgent>,
}

fn locale_like_cmp(a: &str, b: &str) -> Ordering {
    for (ca, cb) in a.chars().zip(b.chars()) {
        let la = ca.to_ascii_lowercase();
        let lb = cb.to_ascii_lowercase();
        match la.cmp(&lb) {
            Ordering::Equal => match ca.cmp(&cb) {
                Ordering::Equal => continue,
                other => return other.reverse(),
            },
            other => return other,
        }
    }
    a.chars().count().cmp(&b.chars().count())
}

impl FleetSnapshot {
    /// Group live agents by host in `wall_core::bus::bus_hosts` order:
    /// this machine's host (`""` or `self.host`) first, then remote hosts alphabetically; within
    /// each host, `waiting` first (`status_rank`), then by name.
    pub fn by_host(&self) -> Vec<FleetHostGroup> {
        let mut by_host: BTreeMap<String, Vec<FleetHostAgent>> = BTreeMap::new();
        let mut host_order: Vec<String> = Vec::new();

        for a in &self.agents {
            if !a.live || a.name.is_empty() {
                continue;
            }
            let host = a.host.clone().unwrap_or_default();
            if !by_host.contains_key(&host) {
                host_order.push(host.clone());
            }
            by_host.entry(host).or_default().push(FleetHostAgent {
                target: a.target(),
                attachable: a.attachable(),
                agent: a.clone(),
            });
        }

        for list in by_host.values_mut() {
            list.sort_by(|x, y| {
                x.status_rank()
                    .cmp(&y.status_rank())
                    .then_with(|| locale_like_cmp(&x.name, &y.name))
            });
        }

        let local_host = self.host.as_deref().unwrap_or("");
        let here = |h: &str| -> u8 {
            if h.is_empty() || (!local_host.is_empty() && h == local_host) {
                0
            } else {
                1
            }
        };
        host_order.sort_by(|x, y| here(x).cmp(&here(y)).then_with(|| locale_like_cmp(x, y)));

        host_order
            .into_iter()
            .filter_map(|host| {
                by_host
                    .remove(&host)
                    .map(|agents| FleetHostGroup { host, agents })
            })
            .collect()
    }

    /// Look up an agent on THIS machine by bare `name` (`gomuxProvider.Agent` in `cli/gomux.go`),
    /// never matching a remote namesake.
    pub fn local_agent(&self, name: &str) -> Option<&FleetAgent> {
        let local_host = self.host.as_deref().unwrap_or("");
        self.agents
            .iter()
            .find(|a| a.name == name && a.host.as_deref().unwrap_or("") == local_host)
    }

    /// Find a live agent by `name` or `name@host` (`WallBus.find`).
    pub fn find(&self, target: &str) -> Option<FleetHostAgent> {
        let hosts = self.by_host();
        for h in &hosts {
            for a in &h.agents {
                if a.target == target {
                    return Some(a.clone());
                }
            }
        }
        if !target.is_empty() && !target.contains('@') {
            if let Some(local_host) = self.host.as_deref().filter(|h| !h.is_empty()) {
                for h in hosts {
                    if h.host != local_host {
                        continue;
                    }
                    for a in h.agents {
                        if a.name == target {
                            return Some(a);
                        }
                    }
                }
            }
        }
        None
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ExportStore {
    #[serde(default)]
    dir: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ExportHost {
    #[serde(default)]
    host: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ExportAgent {
    #[serde(default)]
    name: String,
    #[serde(default)]
    host: String,
    #[serde(default)]
    store: String,
    /// Which run of `name` this is (the store's run id; 0 for a store from before runs and
    /// for another host's agents).
    #[serde(default)]
    run: i64,
    #[serde(default)]
    parent: String,
    #[serde(default)]
    backend: String,
    #[serde(default)]
    session: String,
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    live: bool,
    #[serde(default)]
    attached: Option<bool>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    end_reason: String,
    #[serde(default)]
    event: String,
    #[serde(default)]
    detail: String,
    #[serde(default)]
    tool: String,
    #[serde(default)]
    tool_input: String,
    #[serde(default)]
    status_at: String,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    transcript: String,
    #[serde(default)]
    dialect: String,
    #[serde(default)]
    transcript_size: i64,
    #[serde(default)]
    transcript_mtime: String,
}

#[derive(Deserialize)]
struct ExportLine {
    #[serde(rename = "type")]
    line_type: String,
    #[serde(default)]
    host: serde_json::Value,
    #[serde(default)]
    stores: Vec<ExportStore>,
    /// An event line (`spawned`, `changed`, `removed`) names its store's directory as a
    /// string; a `store` line carries the store as an object. Typed as the object alone
    /// until 2026-11-27, every event line failed to parse and this host's agents never
    /// changed after the snapshot.
    #[serde(default)]
    store: serde_json::Value,
    #[serde(default)]
    agents: Vec<ExportAgent>,
    #[serde(default)]
    agent: Option<ExportAgent>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct AgentKey {
    host: String,
    store: String,
    name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct HostNameKey {
    host: String,
    name: String,
}

fn opt_non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

fn created_sort_key(s: &str) -> i128 {
    parse_rfc3339_nanos(s).unwrap_or(i128::MIN)
}

fn export_prefer(a: &ExportAgent, b: &ExportAgent) -> bool {
    if a.live != b.live {
        return a.live;
    }
    created_sort_key(&a.created_at) > created_sort_key(&b.created_at)
}

fn file_basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn to_fleet_agent(a: &ExportAgent, snap_host: &str) -> FleetAgent {
    let mut status = a.status.clone();
    let mut end_reason = a.end_reason.clone();
    if !a.live && status.is_empty() {
        status = "ended".to_string();
        if end_reason.is_empty() {
            end_reason = "closed".to_string();
        }
    }
    // Export prints the last REPORT, not the resolved status: an agent whose terminal
    // session is gone (its daemon stopped, a reboot) reports nothing more and would read
    // `processing` for good. `agent status` resolves that to `ended (gone)`; so does this.
    // Only this host's agents (the ones a store records): another host sends the status it
    // resolved itself. And only an explicit `false`: absent means unknown.
    if a.live && !a.store.is_empty() && a.attached == Some(false) && status != "ended" {
        status = "ended".to_string();
        end_reason = "gone".to_string();
    }

    let mut ns = a.namespace.clone();
    if ns.is_empty() && (a.host.is_empty() || (!snap_host.is_empty() && a.host == snap_host)) {
        ns = "agents".to_string();
    }

    let mut sid = a.session_id.clone();
    if sid.is_empty() && a.dialect == "claude" && !a.transcript.is_empty() {
        if let Some(stem) = file_basename(&a.transcript).strip_suffix(".jsonl") {
            if !stem.is_empty() {
                sid = stem.to_string();
            }
        }
    }

    let transcript_size = u64::try_from(a.transcript_size).ok().filter(|&n| n > 0);
    let transcript_mtime = if a.transcript_mtime.is_empty() {
        None
    } else {
        parse_rfc3339_ms(&a.transcript_mtime)
    };

    FleetAgent {
        name: a.name.clone(),
        host: opt_non_empty(&a.host),
        parent: a.parent.clone(),
        backend: a.backend.clone(),
        session: a.session.clone(),
        namespace: opt_non_empty(&ns),
        data_dir: a.store.clone(),
        cwd: opt_non_empty(&a.cwd),
        created_at: a.created_at.clone(),
        live: a.live,
        attached: a.attached,
        status: opt_non_empty(&status),
        end_reason: opt_non_empty(&end_reason),
        event: opt_non_empty(&a.event),
        detail: opt_non_empty(&a.detail),
        tool: opt_non_empty(&a.tool),
        tool_input: opt_non_empty(&a.tool_input),
        status_at: opt_non_empty(&a.status_at),
        session_id: opt_non_empty(&sid),
        transcript: opt_non_empty(&a.transcript),
        harness: opt_non_empty(&a.dialect),
        transcript_size,
        transcript_mtime,
    }
}

/// Pure folding state machine for `agent export --json [--follow] --all` (`gomuxState` in `cli/gomux.go`).
#[derive(Clone, Debug, Default)]
pub struct FleetState {
    host: String,
    stores: Vec<String>,
    agents: HashMap<AgentKey, ExportAgent>,
    snap: FleetSnapshot,
}

impl FleetState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current published [`FleetSnapshot`].
    pub fn snapshot(&self) -> &FleetSnapshot {
        &self.snap
    }

    fn put_agent(&mut self, a: ExportAgent) {
        if a.name.is_empty() {
            return;
        }
        let key = AgentKey {
            host: a.host.clone(),
            store: a.store.clone(),
            name: a.name.clone(),
        };
        self.agents.insert(key, a);
    }

    /// Whether `a`, from a `changed` line, describes a run this fold already holds as
    /// departed. The exporter prints a status report that lands after cleanup (a hook of a
    /// tool still running when its pane closed) as the run "as it was": `live: true`, with
    /// the reported status. Folded in, the cleaned-up agent would be back in the live list
    /// with a status nothing will ever correct. A run that departed stays departed; the
    /// name spawned again is another run.
    fn is_report_of_departed_run(&self, a: &ExportAgent) -> bool {
        if !a.live || a.run == 0 {
            return false;
        }
        let key = AgentKey {
            host: a.host.clone(),
            store: a.store.clone(),
            name: a.name.clone(),
        };
        self.agents
            .get(&key)
            .is_some_and(|held| !held.live && held.run == a.run)
    }

    /// Fold one JSONL line from `agent export --json --follow` and return `Ok(true)` iff the
    /// published [`FleetSnapshot`] changed (`gen` incremented).
    pub fn apply_line(&mut self, raw: &str) -> Result<bool, String> {
        let line: ExportLine =
            serde_json::from_str(raw).map_err(|e| format!("invalid export JSON: {e}"))?;
        self.apply_export(line)
    }

    /// Fold one or more whitespace-separated JSON objects (handling both pretty-printed
    /// non-`--follow` `agent export --json` output and compact JSONL streams).
    pub fn apply_json_stream(&mut self, raw: &[u8]) -> Result<bool, String> {
        let mut any_changed = false;
        let stream = serde_json::Deserializer::from_slice(raw).into_iter::<ExportLine>();
        for (idx, item) in stream.enumerate() {
            let line = item.map_err(|e| format!("export object {}: invalid export JSON: {e}", idx + 1))?;
            if self.apply_export(line)? {
                any_changed = true;
            }
        }
        Ok(any_changed)
    }

    fn apply_export(&mut self, line: ExportLine) -> Result<bool, String> {
        match line.line_type.as_str() {
            "snapshot" => {
                self.agents.clear();
                self.host = line.host.as_str().unwrap_or_default().to_string();
                self.stores.clear();
                for st in line.stores {
                    if !st.dir.is_empty() {
                        self.stores.push(st.dir);
                    }
                }
                for a in line.agents {
                    self.put_agent(a);
                }
            }
            "spawned" | "removed" => {
                if let Some(a) = line.agent {
                    self.put_agent(a);
                }
            }
            "changed" => {
                if let Some(a) = line.agent {
                    if !self.is_report_of_departed_run(&a) {
                        self.put_agent(a);
                    }
                }
            }
            "store" => {
                let st: ExportStore = serde_json::from_value(line.store)
                    .map_err(|e| format!("export `store` line invalid `store` object: {e}"))?;
                self.agents.retain(|k, _| k.store != st.dir);
                for a in line.agents {
                    self.put_agent(a);
                }
                if !st.dir.is_empty() && !self.stores.contains(&st.dir) {
                    self.stores.push(st.dir);
                }
            }
            "host" => {
                let h: ExportHost = serde_json::from_value(line.host)
                    .map_err(|e| format!("export `host` line invalid `host` object: {e}"))?;
                self.agents
                    .retain(|k, _| !(k.host == h.host && k.store.is_empty()));
                for a in line.agents {
                    self.put_agent(a);
                }
            }
            "unrecorded" => return Ok(false),
            other => return Err(format!("unknown line type {other:?}")),
        }

        // Deduplicate by (host, name) across stores: live beats departed, then newer createdAt,
        // with deterministic tie-breaking by store path so HashMap iteration order cannot jitter.
        let mut by_name: HashMap<HostNameKey, &ExportAgent> = HashMap::new();
        let mut sorted_raw: Vec<&ExportAgent> = self.agents.values().collect();
        sorted_raw.sort_by(|x, y| {
            x.host
                .cmp(&y.host)
                .then_with(|| x.name.cmp(&y.name))
                .then_with(|| x.store.cmp(&y.store))
        });
        for a in sorted_raw {
            let key = HostNameKey {
                host: a.host.clone(),
                name: a.name.clone(),
            };
            match by_name.get(&key) {
                None => {
                    by_name.insert(key, a);
                }
                Some(cur) if export_prefer(a, cur) => {
                    by_name.insert(key, a);
                }
                Some(_) => {}
            }
        }

        let mut list: Vec<FleetAgent> = by_name
            .into_values()
            .map(|a| to_fleet_agent(a, &self.host))
            .collect();
        list.sort_by(|x, y| {
            created_sort_key(&x.created_at)
                .cmp(&created_sort_key(&y.created_at))
                .then_with(|| x.name.cmp(&y.name))
                .then_with(|| x.host.cmp(&y.host))
        });

        let mut dirs = self.stores.clone();
        dirs.sort();
        let host = opt_non_empty(&self.host);

        if self.snap.host == host && self.snap.dirs == dirs && self.snap.agents == list {
            return Ok(false);
        }

        self.snap = FleetSnapshot {
            gen: self.snap.gen + 1,
            host,
            dirs,
            agents: list,
        };
        Ok(true)
    }
}

/// Options for [`FleetFollower`] and [`export_once`].
#[derive(Clone, Debug)]
pub struct FleetOptions {
    /// Explicit `--store <dir>` flags; empty lets gomuxai discover every live store on the host.
    pub dirs: Vec<PathBuf>,
    /// Optional `--hosts <hosts>` flag (`all` or comma-separated SSH hosts).
    pub hosts: String,
    /// Initial restart delay when `agent export --follow` exits unexpectedly (default 250ms).
    pub initial_backoff: Duration,
    /// Maximum exponential backoff cap between restarts (default 5s).
    pub max_backoff: Duration,
}

impl Default for FleetOptions {
    fn default() -> Self {
        Self {
            dirs: Vec::new(),
            hosts: String::new(),
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(5),
        }
    }
}

fn build_export_args(opts: &FleetOptions, follow: bool) -> Vec<String> {
    let mut args = vec!["export".to_string(), "--json".to_string()];
    if follow {
        args.push("--follow".to_string());
    }
    args.push("--all".to_string());
    args.push("-q".to_string());
    for dir in &opts.dirs {
        args.push("--store".to_string());
        args.push(dir.display().to_string());
    }
    if !opts.hosts.trim().is_empty() {
        args.push("--hosts".to_string());
        args.push(opts.hosts.clone());
    }
    args
}

/// Run a single non-following `agent export --json --all -q` snapshot and fold its output.
///
/// Note that without `--follow`, gomuxai pretty-prints the snapshot across multiple lines rather
/// than emitting compact JSONL; [`FleetState::apply_json_stream`] accepts both formats.
pub fn export_once(cli: &AgentCli, opts: &FleetOptions) -> Result<FleetSnapshot, String> {
    let owned_args = build_export_args(opts, false);
    let arg_refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
    let stdout = cli.run(&arg_refs)?;
    let mut state = FleetState::new();
    state.apply_json_stream(&stdout)?;
    Ok(state.snapshot().clone())
}

struct FollowerShared {
    snap: FleetSnapshot,
    last_error: Option<String>,
    restarts: u64,
    stopped: bool,
}

/// Background thread following `agent export --json --follow --all`.
///
/// Publishes the latest [`FleetSnapshot`], calls `wake` whenever it changes, and restarts the
/// child process with exponential backoff if it exits while the follower is still active.
pub struct FleetFollower {
    shared: Arc<(Mutex<FollowerShared>, Condvar)>,
    child: Arc<Mutex<Option<Child>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl FleetFollower {
    /// Spawn the background export follower thread.
    pub fn start<F>(cli: AgentCli, opts: FleetOptions, wake: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        let shared = Arc::new((
            Mutex::new(FollowerShared {
                snap: FleetSnapshot::default(),
                last_error: None,
                restarts: 0,
                stopped: false,
            }),
            Condvar::new(),
        ));
        let child_slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));

        let shared_bg = Arc::clone(&shared);
        let child_bg = Arc::clone(&child_slot);
        let wake = Arc::new(wake);

        let handle = thread::Builder::new()
            .name("agent-feed-fleet".to_string())
            .spawn(move || run_fleet_loop(cli, opts, shared_bg, child_bg, wake))
            .expect("spawning agent-feed-fleet thread must succeed");

        Self {
            shared,
            child: child_slot,
            thread: Mutex::new(Some(handle)),
        }
    }

    /// Latest published [`FleetSnapshot`].
    pub fn snapshot(&self) -> FleetSnapshot {
        self.shared
            .0
            .lock()
            .expect("fleet follower mutex poisoned")
            .snap
            .clone()
    }

    /// Live agents grouped and sorted by host (`wall_core::bus::bus_hosts` order).
    pub fn hosts(&self) -> Vec<FleetHostGroup> {
        self.snapshot().by_host()
    }

    /// Most recent child exit or parse error, cleared once a subsequent snapshot change lands.
    pub fn last_error(&self) -> Option<String> {
        self.shared
            .0
            .lock()
            .expect("fleet follower mutex poisoned")
            .last_error
            .clone()
    }

    /// How many times the child process was restarted after exiting unexpectedly.
    pub fn restart_count(&self) -> u64 {
        self.shared
            .0
            .lock()
            .expect("fleet follower mutex poisoned")
            .restarts
    }

    /// Stop the follower thread and terminate any running `agent export` child process.
    pub fn stop(&self) {
        {
            let (lock, cvar) = &*self.shared;
            let mut guard = lock.lock().expect("fleet follower mutex poisoned");
            guard.stopped = true;
            cvar.notify_all();
        }
        if let Some(child) = self
            .child
            .lock()
            .expect("fleet child mutex poisoned")
            .as_mut()
        {
            let _ = child.kill();
        }
        if let Some(handle) = self
            .thread
            .lock()
            .expect("fleet join mutex poisoned")
            .take()
        {
            let _ = handle.join();
        }
    }
}

impl Drop for FleetFollower {
    fn drop(&mut self) {
        self.stop();
    }
}

fn read_capped_stderr(mut r: impl std::io::Read) -> String {
    let mut buf = Vec::new();
    let _ = (&mut r).take(8192).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).trim().to_string()
}

fn run_fleet_loop(
    cli: AgentCli,
    opts: FleetOptions,
    shared: Arc<(Mutex<FollowerShared>, Condvar)>,
    child_slot: Arc<Mutex<Option<Child>>>,
    wake: Arc<dyn Fn() + Send + Sync>,
) {
    let owned_args = build_export_args(&opts, true);
    let arg_refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
    let initial_backoff = opts.initial_backoff.max(Duration::from_millis(1));
    let max_backoff = opts.max_backoff.max(initial_backoff);
    let mut backoff = initial_backoff;
    let mut state = FleetState::new();
    let mut first_attempt = true;

    loop {
        {
            let guard = shared.0.lock().expect("fleet follower mutex poisoned");
            if guard.stopped {
                return;
            }
        }
        if !first_attempt {
            let mut guard = shared.0.lock().expect("fleet follower mutex poisoned");
            guard.restarts += 1;
        }
        first_attempt = false;

        let mut cmd = cli.command(&arg_refs);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut saw_snapshot = false;
        let run_err: Option<String> = match crate::cli::spawn_command(&mut cmd) {
            Err(e) => Some(format!(
                "could not spawn `{}`: {e}",
                cli.describe(&arg_refs)
            )),
            Ok(mut child) => {
                let stdout = child.stdout.take().expect("stdout was piped");
                let stderr = child.stderr.take().expect("stderr was piped");
                let stderr_handle = thread::spawn(move || read_capped_stderr(stderr));

                // Install child in slot; if stop() raced us, kill immediately.
                {
                    let is_stopped = shared
                        .0
                        .lock()
                        .expect("fleet follower mutex poisoned")
                        .stopped;
                    let mut slot = child_slot.lock().expect("fleet child mutex poisoned");
                    if is_stopped {
                        let _ = child.kill();
                    }
                    *slot = Some(child);
                }

                let mut stream_err: Option<String> = None;
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    match line {
                        Ok(raw) => {
                            let trimmed = raw.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            let mut is_snapshot = false;
                            if trimmed.contains("\"type\":\"snapshot\"")
                                || trimmed.contains("\"type\": \"snapshot\"")
                            {
                                saw_snapshot = true;
                                is_snapshot = true;
                            }
                            match state.apply_line(trimmed) {
                                Ok(true) => {
                                    let snap = state.snapshot().clone();
                                    {
                                        let mut guard = shared
                                            .0
                                            .lock()
                                            .expect("fleet follower mutex poisoned");
                                        if guard.stopped {
                                            break;
                                        }
                                        guard.snap = snap;
                                        guard.last_error = None;
                                    }
                                    wake();
                                }
                                // A child's snapshot replaces the whole fold, so whatever
                                // the last child died of (or could not read) is over, even
                                // when the fleet it describes is the one already published.
                                // Until 2026-11-27 only a CHANGED fleet cleared the error,
                                // so a restart onto the same agents left the wall saying
                                // the feed was down for good.
                                Ok(false) if is_snapshot => {
                                    let cleared = shared
                                        .0
                                        .lock()
                                        .expect("fleet follower mutex poisoned")
                                        .last_error
                                        .take()
                                        .is_some();
                                    if cleared {
                                        wake();
                                    }
                                }
                                Ok(false) => {}
                                Err(e) => {
                                    shared
                                        .0
                                        .lock()
                                        .expect("fleet follower mutex poisoned")
                                        .last_error = Some(e);
                                }
                            }
                        }
                        Err(e) => {
                            stream_err = Some(format!("export stdout read error: {e}"));
                            break;
                        }
                    }
                }

                let waited = child_slot
                    .lock()
                    .expect("fleet child mutex poisoned")
                    .take()
                    .and_then(|mut c| c.wait().ok());
                let stderr_text = stderr_handle.join().unwrap_or_default();

                if let Some(err) = stream_err {
                    Some(err)
                } else {
                    match waited {
                        Some(status) if status.success() => {
                            Some("`agent export --follow` exited cleanly (EOF)".to_string())
                        }
                        Some(status) => {
                            if stderr_text.is_empty() {
                                Some(format!("`agent export --follow` exited with {status}"))
                            } else {
                                Some(format!(
                                    "`agent export --follow` exited with {status}: {stderr_text}"
                                ))
                            }
                        }
                        None => Some("`agent export --follow` ended".to_string()),
                    }
                }
            }
        };

        if saw_snapshot {
            backoff = initial_backoff;
        }

        let (lock, cvar) = &*shared;
        let mut guard = lock.lock().expect("fleet follower mutex poisoned");
        if guard.stopped {
            return;
        }
        if let Some(err) = run_err {
            guard.last_error = Some(err);
            // The error is news for whoever draws the fleet: say so, as a change is said.
            drop(guard);
            wake();
            guard = lock.lock().expect("fleet follower mutex poisoned");
            if guard.stopped {
                return;
            }
        }
        let (next_guard, _) = cvar
            .wait_timeout_while(guard, backoff, |s| !s.stopped)
            .expect("fleet follower condvar poisoned");
        if next_guard.stopped {
            return;
        }
        backoff = backoff.saturating_mul(2).min(max_backoff);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering};
    use std::time::Instant;

    use super::*;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let n = SEQ.fetch_add(1, AtomicOrdering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "agent-feed-fleet-{label}-{}-{n}",
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

    fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if cond() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn fleet_state_folds_export_stream_porting_gomux_test() {
        let mut st = FleetState::new();
        let apply = |st: &mut FleetState, line: &str| st.apply_line(line).unwrap();

        // Initial snapshot: two stores via one snapshot, plus a Claude transcript without session_id.
        assert!(apply(
            &mut st,
            r#"{"type":"snapshot","host":"workstation","stores":[{"dir":"/s/a"}],"agents":[
                {"name":"lead","host":"workstation","store":"/s/a","backend":"claude","session":"lead","cwd":"/repo","created_at":"2026-10-12T10:00:00Z","live":true,"attached":true,"status":"idle","dialect":"claude","transcript":"/home/u/.claude/projects/p/abc-123.jsonl","transcript_size":4096,"transcript_mtime":"2026-10-12T10:05:00Z"},
                {"name":"worker","host":"workstation","store":"/s/a","parent":"lead","backend":"agy","session":"worker","created_at":"2026-10-12T10:01:00Z","live":true,"attached":true,"status":"processing","tool":"Bash","tool_input":"cargo test","dialect":"agy","transcript":"/brain/uuid/.system_generated/logs/transcript.jsonl"}
            ]}"#,
        ));

        let s = st.snapshot();
        assert_eq!(s.gen, 1);
        assert_eq!(s.host.as_deref(), Some("workstation"));
        assert_eq!(s.dirs, vec!["/s/a"]);
        assert_eq!(s.agents.len(), 2);

        let lead = &s.agents[0];
        assert_eq!(lead.name, "lead");
        assert_eq!(
            lead.namespace.as_deref(),
            Some("agents"),
            "local agent defaults namespace to 'agents'"
        );
        assert_eq!(
            lead.session_id.as_deref(),
            Some("abc-123"),
            "Claude sessionId derived from .jsonl basename"
        );
        assert!(lead.has_session() && lead.attachable());
        assert_eq!(
            lead.transcript_mtime,
            Some(1_791_799_500_000),
            "RFC3339 transcript_mtime converted to unix ms"
        );

        let worker = &s.agents[1];
        assert_eq!(
            worker.session_id, None,
            "agy sessionId lives in parent dir, never guessed from transcript.jsonl"
        );
        assert_eq!(worker.tool.as_deref(), Some("Bash"));
        assert_eq!(worker.harness.as_deref(), Some("agy"));

        // Identical snapshot does not bump gen; unrecorded is ignored.
        assert!(!apply(
            &mut st,
            r#"{"type":"snapshot","host":"workstation","stores":[{"dir":"/s/a"}],"agents":[
                {"name":"lead","host":"workstation","store":"/s/a","backend":"claude","session":"lead","cwd":"/repo","created_at":"2026-10-12T10:00:00Z","live":true,"attached":true,"status":"idle","dialect":"claude","transcript":"/home/u/.claude/projects/p/abc-123.jsonl","transcript_size":4096,"transcript_mtime":"2026-10-12T10:05:00Z"},
                {"name":"worker","host":"workstation","store":"/s/a","parent":"lead","backend":"agy","session":"worker","created_at":"2026-10-12T10:01:00Z","live":true,"attached":true,"status":"processing","tool":"Bash","tool_input":"cargo test","dialect":"agy","transcript":"/brain/uuid/.system_generated/logs/transcript.jsonl"}
            ]}"#,
        ));
        assert!(!apply(&mut st, r#"{"type":"unrecorded","session":"stray"}"#));
        assert_eq!(st.snapshot().gen, 1);

        // Second store appears with an older departed "lead" -> live "lead" in /s/a wins.
        assert!(apply(
            &mut st,
            r#"{"type":"store","store":{"dir":"/s/b"},"agents":[
                {"name":"lead","host":"workstation","store":"/s/b","created_at":"2026-10-11T10:00:00Z","live":false,"status":"ended"}
            ]}"#,
        ));
        let lead_after = st.snapshot().local_agent("lead").unwrap();
        assert!(lead_after.live && lead_after.data_dir == "/s/a");
        assert_eq!(st.snapshot().dirs, vec!["/s/a", "/s/b"]);

        // Status change on worker.
        assert!(apply(
            &mut st,
            r#"{"type":"changed","agent":{"name":"worker","host":"workstation","store":"/s/a","parent":"lead","backend":"agy","session":"worker","created_at":"2026-10-12T10:01:00Z","live":true,"attached":true,"status":"waiting","detail":"Run command?"}}"#,
        ));
        let worker_after = st.snapshot().local_agent("worker").unwrap();
        assert_eq!(worker_after.status.as_deref(), Some("waiting"));
        assert_eq!(worker_after.detail.as_deref(), Some("Run command?"));

        // Removed with --all carries departed agent (`live:false, attached:false`) keeping status.
        assert!(apply(
            &mut st,
            r#"{"type":"removed","agent":{"name":"worker","host":"workstation","store":"/s/a","created_at":"2026-10-12T10:01:00Z","live":false,"attached":false,"status":"ended","end_reason":"completed"}}"#,
        ));
        let worker_gone = st.snapshot().local_agent("worker").unwrap();
        assert!(!worker_gone.live);
        assert_eq!(worker_gone.attached, Some(false));
        assert!(!worker_gone.attachable() && !worker_gone.has_session());
        assert_eq!(worker_gone.end_reason.as_deref(), Some("completed"));

        // Remote host lines: omitted `attached` stays None (attachable!), explicit false stays false;
        // remote namespace stays None when omitted, and local_agent("lead") ignores remote "lead".
        assert!(apply(
            &mut st,
            r#"{"type":"host","host":{"host":"far-host"},"agents":[
                {"name":"lead","host":"far-host","backend":"claude","session":"lead","created_at":"2026-10-12T11:00:00Z","live":true,"status":"idle"},
                {"name":"dead","host":"far-host","backend":"claude","session":"dead","created_at":"2026-10-12T11:01:00Z","live":true,"attached":false,"status":"error"}
            ]}"#,
        ));
        let snap = st.snapshot();
        assert_eq!(
            snap.local_agent("lead").unwrap().host.as_deref(),
            Some("workstation"),
            "local_agent never returns remote namesake"
        );
        let remote_lead = snap
            .agents
            .iter()
            .find(|a| a.name == "lead" && a.host.as_deref() == Some("far-host"))
            .unwrap();
        assert_eq!(remote_lead.attached, None);
        assert!(remote_lead.attachable());
        assert!(!remote_lead.has_session());
        assert_eq!(remote_lead.namespace, None);

        let remote_dead = snap
            .agents
            .iter()
            .find(|a| a.name == "dead" && a.host.as_deref() == Some("far-host"))
            .unwrap();
        assert_eq!(remote_dead.attached, Some(false));
        assert!(!remote_dead.attachable());

        // Host & status ordering (`by_host`): local host workstation first, then m2; within m2,
        // `dead` (error, rank 1) precedes `lead` (idle, rank 3).
        let groups = snap.by_host();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].host, "workstation");
        assert_eq!(groups[0].agents.len(), 1, "departed worker excluded from by_host");
        assert_eq!(groups[1].host, "far-host");
        assert_eq!(
            groups[1]
                .agents
                .iter()
                .map(|a| a.target.as_str())
                .collect::<Vec<_>>(),
            vec!["dead@far-host", "lead@far-host"]
        );

        // Unknown line type errors cleanly without corrupting snapshot.
        let prev_gen = st.snapshot().gen;
        assert!(st.apply_line(r#"{"type":"mystery"}"#).is_err());
        assert_eq!(st.snapshot().gen, prev_gen);

        // Pretty-printed multi-line JSON (as emitted by `agent export --json` without `--follow`):
        let mut pretty_state = FleetState::new();
        let pretty = br#"{
  "type": "snapshot",
  "host": "laptop-2",
  "stores": [
    { "dir": "/s/main" }
  ],
  "agents": [
    {
      "name": "wall-config",
      "host": "laptop-2",
      "store": "/s/main",
      "backend": "agy",
      "live": true,
      "attached": true,
      "status": "processing"
    }
  ]
}
"#;
        assert!(pretty_state.apply_json_stream(pretty).unwrap());
        assert_eq!(pretty_state.snapshot().host.as_deref(), Some("laptop-2"));
        assert_eq!(pretty_state.snapshot().agents.len(), 1);
    }

    /// One agent object as `agent export` prints it, every field present.
    fn export_agent(name: &str, run: u64, live: bool, attached: bool, status: &str, rest: &str) -> String {
        format!(
            r#"{{"name":"{name}","host":"laptop-2","kind":"termd","run":{run},"parent":"lead","backend":"claude","session":"agent-{name}","namespace":"agents","store":"/s/main","cwd":"/repo","created_at":"2026-11-27T19:01:44.945484Z","live":{live},"attached":{attached},"resumable":false,"status":"{status}"{rest}}}"#
        )
    }

    /// An event line as `agent export --json --follow` prints it: beside the agent, its `id`,
    /// the store's directory as a STRING, `at`, and for `changed` a `reason`.
    fn event_line(kind: &str, id: u64, reason: &str, agent: &str) -> String {
        let reason = if reason.is_empty() {
            String::new()
        } else {
            format!(r#","reason":"{reason}""#)
        };
        format!(
            r#"{{"type":"{kind}","id":{id},"store":"/s/main","at":"2026-11-27T20:34:54.278997Z"{reason},"agent":{agent}}}"#
        )
    }

    /// The live rows a display draws: `(target, status)` in [`FleetSnapshot::by_host`] order.
    fn live_rows(snap: &FleetSnapshot) -> Vec<(String, String)> {
        snap.by_host()
            .into_iter()
            .flat_map(|h| h.agents)
            .map(|a| (a.target.clone(), a.status.clone().unwrap_or_default()))
            .collect()
    }

    /// The bug of 2026-11-27: the sidebar showed a helper as `processing` an hour after it was
    /// cleaned up. The follow stream's event lines name their store as a string
    /// (`"store":"/dir"`), a `store` line as an object, and the fold read only the object: every
    /// `spawned`, `changed` and `removed` line failed to parse, so this host's agents kept the
    /// status the wall started with. (The fold's older test wrote its event lines without the
    /// keys the CLI prints, which is how it passed.)
    #[test]
    fn follow_lines_as_the_cli_prints_them_take_a_cleaned_up_agent_out_of_the_live_list() {
        let mut st = FleetState::new();
        let snapshot = format!(
            r#"{{"type":"snapshot","host":"laptop-2","stores":[{{"dir":"/s/main","this":true,"cursor":65740,"events":true}}],"hosts":[],"agents":[{},{}],"unrecorded":[]}}"#,
            export_agent("lead", 79, true, true, "idle", ""),
            export_agent("helper", 95, true, true, "processing", r#","event":"PostToolUse","tool":"Bash""#),
        );
        assert!(st.apply_line(&snapshot).unwrap());
        assert_eq!(
            live_rows(st.snapshot()),
            vec![
                ("helper@laptop-2".to_string(), "processing".to_string()),
                ("lead@laptop-2".to_string(), "idle".to_string()),
            ]
        );

        // Its turn ends: a status report is a `changed` line.
        let idle = event_line(
            "changed",
            65750,
            "status",
            &export_agent("helper", 95, true, true, "idle", r#","event":"Stop","detail":"turn finished""#),
        );
        assert!(st.apply_line(&idle).unwrap(), "a changed line as the CLI prints it must fold");
        assert_eq!(
            st.snapshot().local_agent("helper").unwrap().status.as_deref(),
            Some("idle")
        );

        // Its CLI quits, then cleanup deletes the record.
        let quit = event_line(
            "changed",
            65782,
            "status",
            &export_agent("helper", 95, true, true, "ended", r#","end_reason":"quit","event":"SessionEnd""#),
        );
        assert!(st.apply_line(&quit).unwrap());
        let removed = event_line(
            "removed",
            65783,
            "",
            &export_agent(
                "helper",
                95,
                false,
                false,
                "ended",
                r#","end_reason":"quit","ended_at":"2026-11-27T20:34:54.278997Z""#,
            ),
        );
        assert!(st.apply_line(&removed).unwrap());
        let gone = st.snapshot().local_agent("helper").unwrap();
        assert!(!gone.live, "a removed agent is departed");
        assert_eq!(gone.status.as_deref(), Some("ended"));
        assert_eq!(gone.end_reason.as_deref(), Some("quit"));
        assert_eq!(
            live_rows(st.snapshot()),
            vec![("lead@laptop-2".to_string(), "idle".to_string())],
            "a cleaned-up agent leaves the live list"
        );

        // A hook that reports after cleanup (a tool still running when its pane closed): the
        // exporter describes the run "as it was", live, since nothing in its tree holds it any
        // more. The run has departed, and a late report does not bring it back.
        let late = event_line(
            "changed",
            65790,
            "status",
            &export_agent("helper", 95, true, false, "processing", r#","event":"PostToolUse""#),
        );
        assert!(!st.apply_line(&late).unwrap(), "a late report of a departed run changes nothing");
        assert!(!st.snapshot().local_agent("helper").unwrap().live);
        assert_eq!(live_rows(st.snapshot()).len(), 1);

        // The name spawned again is another run, and is live.
        let again = event_line(
            "spawned",
            65800,
            "",
            &export_agent("helper", 96, true, true, "processing", ""),
        );
        assert!(st.apply_line(&again).unwrap());
        assert_eq!(
            live_rows(st.snapshot()),
            vec![
                ("helper@laptop-2".to_string(), "processing".to_string()),
                ("lead@laptop-2".to_string(), "idle".to_string()),
            ]
        );

        // A store found while following is a `store` line, whose `store` is an object.
        assert!(st
            .apply_line(r#"{"type":"store","store":{"dir":"/s/other","cursor":0,"events":false},"agents":[]}"#)
            .unwrap());
        assert_eq!(st.snapshot().dirs, vec!["/s/main", "/s/other"]);
        assert!(st.apply_line(r#"{"type":"store","store":"/s/other","agents":[]}"#).is_err());
    }

    /// An agent whose record is live but whose terminal session is gone (its `g3d-termd` was
    /// stopped, the machine rebooted) reports nothing more, so export keeps its last status
    /// for good. `agent status` calls it `ended (gone)`, and so does the fold: a status that
    /// can never change again is not a live one.
    #[test]
    fn a_live_agent_whose_session_is_gone_reads_as_ended_not_by_its_last_report() {
        let mut st = FleetState::new();
        let snapshot = format!(
            r#"{{"type":"snapshot","host":"laptop-2","stores":[{{"dir":"/s/main"}}],"agents":[{},{},{}]}}"#,
            export_agent("stranded", 7, true, false, "processing", ""),
            export_agent("quit-in-pane", 8, true, true, "ended", r#","end_reason":"quit""#),
            export_agent("working", 9, true, true, "processing", ""),
        );
        assert!(st.apply_line(&snapshot).unwrap());
        let snap = st.snapshot();

        let stranded = snap.local_agent("stranded").unwrap();
        assert!(stranded.live, "its record is still there");
        assert_eq!(stranded.status.as_deref(), Some("ended"));
        assert_eq!(stranded.end_reason.as_deref(), Some("gone"));
        assert!(!stranded.attachable());

        let quit = snap.local_agent("quit-in-pane").unwrap();
        assert_eq!(
            (quit.status.as_deref(), quit.end_reason.as_deref()),
            (Some("ended"), Some("quit")),
            "an end the agent reported keeps its own reason"
        );
        assert_eq!(snap.local_agent("working").unwrap().status.as_deref(), Some("processing"));

        // Another host resolves its own agents' status, so what it sends stands: one that
        // predates `attached` leaves it out (unknown, not gone).
        assert!(st
            .apply_line(
                r#"{"type":"host","host":{"host":"far-host","agents":2},"agents":[{"name":"old","host":"far-host","live":true,"status":"processing"},{"name":"stuck","host":"far-host","live":true,"attached":false,"status":"error"}]}"#
            )
            .unwrap());
        let remote = |name: &str| {
            st.snapshot()
                .agents
                .iter()
                .find(|a| a.name == name)
                .unwrap()
                .status
                .clone()
        };
        assert_eq!(remote("old").as_deref(), Some("processing"));
        assert_eq!(remote("stuck").as_deref(), Some("error"));

        // The session comes back (its daemon restarted under it): the last report stands again.
        let back = event_line(
            "changed",
            12,
            "session",
            &export_agent("stranded", 7, true, true, "processing", ""),
        );
        assert!(st.apply_line(&back).unwrap());
        assert_eq!(
            st.snapshot().local_agent("stranded").unwrap().status.as_deref(),
            Some("processing")
        );
    }

    /// The same bug end to end: a stub `agent` that prints what the real one prints, followed
    /// by [`FleetFollower`].
    #[test]
    fn fleet_follower_drops_a_cleaned_up_agent_from_a_stream_shaped_like_the_clis() {
        let scratch = Scratch::new("cleanup");
        let stream = scratch.0.join("stream.jsonl");
        let stub = scratch.0.join("agent");
        let lines = [
            format!(
                r#"{{"type":"snapshot","host":"laptop-2","stores":[{{"dir":"/s/main","this":true,"cursor":65740,"events":true}}],"hosts":[],"agents":[{}],"unrecorded":[]}}"#,
                export_agent("helper", 95, true, true, "processing", r#","event":"PostToolUse""#),
            ),
            event_line(
                "changed",
                65782,
                "status",
                &export_agent("helper", 95, true, true, "ended", r#","end_reason":"quit","event":"SessionEnd""#),
            ),
            event_line(
                "removed",
                65783,
                "",
                &export_agent(
                    "helper",
                    95,
                    false,
                    false,
                    "ended",
                    r#","end_reason":"quit","ended_at":"2026-11-27T20:34:54.278997Z""#,
                ),
            ),
        ];
        fs::write(&stream, lines.join("\n") + "\n").unwrap();
        write_stub(
            &stub,
            &format!("#!/bin/sh\ncat '{}'\nexec sleep 60\n", stream.display()),
        );

        let follower = FleetFollower::start(AgentCli::new(&stub), FleetOptions::default(), || {});
        wait_until("the removed line folded", || {
            follower
                .snapshot()
                .local_agent("helper")
                .is_some_and(|a| !a.live)
        });
        let snap = follower.snapshot();
        assert_eq!(snap.gen, 3, "the snapshot and both event lines each changed the fleet");
        assert_eq!(
            follower.last_error(),
            None,
            "every line the CLI prints parses"
        );
        assert!(
            follower.hosts().is_empty(),
            "no live agent is left to draw: {:?}",
            live_rows(&snap)
        );
        let helper = snap.local_agent("helper").unwrap();
        assert_eq!(
            (helper.status.as_deref(), helper.end_reason.as_deref()),
            (Some("ended"), Some("quit"))
        );
    }

    /// A child that dies and comes back onto the same agents: the fleet does not change, and
    /// the error it died with must not outlive it.
    #[test]
    fn fleet_follower_error_ends_when_a_restart_reads_the_same_fleet() {
        let scratch = Scratch::new("same-fleet");
        let count_file = scratch.0.join("runs");
        let stub = scratch.0.join("agent");
        let snapshot = format!(
            r#"{{"type":"snapshot","host":"laptop-2","stores":[{{"dir":"/s/main"}}],"agents":[{}]}}"#,
            export_agent("helper", 95, true, true, "idle", ""),
        );
        let script = format!(
            "#!/bin/sh\n\
             n=0\n\
             if [ -f '{runs}' ]; then n=$(cat '{runs}'); fi\n\
             n=$((n + 1))\n\
             printf '%s' \"$n\" > '{runs}'\n\
             cat <<'LINE'\n{snapshot}\nLINE\n\
             if [ \"$n\" -eq 1 ]; then echo 'bus went away' >&2; exit 1; fi\n\
             exec sleep 60\n",
            runs = count_file.display(),
        );
        write_stub(&stub, &script);

        let wakes = Arc::new(AtomicUsize::new(0));
        let wakes_cb = Arc::clone(&wakes);
        let opts = FleetOptions {
            // Long enough to read the error while the follower waits to restart.
            initial_backoff: Duration::from_millis(400),
            max_backoff: Duration::from_millis(400),
            ..FleetOptions::default()
        };
        let follower = FleetFollower::start(AgentCli::new(&stub), opts, move || {
            wakes_cb.fetch_add(1, AtomicOrdering::SeqCst);
        });

        wait_until("the first child's exit reported", || {
            follower.last_error().is_some_and(|e| e.contains("bus went away"))
        });
        assert_eq!(follower.snapshot().gen, 1);
        wait_until("the restarted child's snapshot read", || {
            follower.restart_count() >= 1 && follower.last_error().is_none()
        });
        assert_eq!(follower.snapshot().gen, 1, "the same fleet is not a change");
        assert_eq!(
            wakes.load(AtomicOrdering::SeqCst),
            3,
            "the fleet, the error, and the error's end"
        );
    }

    #[test]
    fn fleet_follower_streams_wakes_only_on_change_and_restarts_with_backoff() {
        let scratch = Scratch::new("follower");
        let count_file = scratch.0.join("runs");
        let pid_file = scratch.0.join("child.pid");
        let stub = scratch.0.join("agent");

        // Stub `agent export`:
        // - Run 1 prints an initial snapshot, then a quiet repeat (must NOT wake), then exits 1!
        // - Run 2 prints an updated snapshot (`status: "waiting"`), records its PID, and execs sleep 60.
        let script = format!(
            "#!/bin/sh\n\
             n=0\n\
             if [ -f '{runs}' ]; then n=$(cat '{runs}'); fi\n\
             n=$((n + 1))\n\
             printf '%s' \"$n\" > '{runs}'\n\
             if [ \"$n\" -eq 1 ]; then\n\
               printf '{{\"type\":\"snapshot\",\"host\":\"box1\",\"agents\":[{{\"name\":\"alpha\",\"host\":\"box1\",\"store\":\"/s\",\"created_at\":\"2026-10-12T10:00:00Z\",\"live\":true,\"attached\":true,\"status\":\"idle\"}}]}}\\n'\n\
               printf '{{\"type\":\"snapshot\",\"host\":\"box1\",\"agents\":[{{\"name\":\"alpha\",\"host\":\"box1\",\"store\":\"/s\",\"created_at\":\"2026-10-12T10:00:00Z\",\"live\":true,\"attached\":true,\"status\":\"idle\"}}]}}\\n'\n\
               echo 'boom' >&2\n\
               exit 1\n\
             fi\n\
             echo $$ > '{pid}'\n\
             printf '{{\"type\":\"snapshot\",\"host\":\"box1\",\"agents\":[{{\"name\":\"alpha\",\"host\":\"box1\",\"store\":\"/s\",\"created_at\":\"2026-10-12T10:00:00Z\",\"live\":true,\"attached\":true,\"status\":\"waiting\"}}]}}\\n'\n\
             exec sleep 60\n",
            runs = count_file.display(),
            pid = pid_file.display(),
        );
        write_stub(&stub, &script);

        let wakes = Arc::new(AtomicUsize::new(0));
        let wakes_cb = Arc::clone(&wakes);
        let opts = FleetOptions {
            dirs: Vec::new(),
            hosts: "all".to_string(),
            initial_backoff: Duration::from_millis(20),
            max_backoff: Duration::from_millis(80),
        };

        let follower = FleetFollower::start(AgentCli::new(&stub), opts, move || {
            wakes_cb.fetch_add(1, AtomicOrdering::SeqCst);
        });

        wait_until("restarted snapshot with waiting status", || {
            follower
                .snapshot()
                .local_agent("alpha")
                .and_then(|a| a.status.as_deref())
                == Some("waiting")
        });

        // Two distinct snapshots across two runs -> exact gen 2 (quiet repeat ignored), and 3
        // wake calls: each changed fleet, and the first child's exit (its error is news too).
        assert_eq!(follower.snapshot().gen, 2);
        assert_eq!(wakes.load(AtomicOrdering::SeqCst), 3);
        assert!(follower.restart_count() >= 1);
        assert_eq!(follower.last_error(), None, "the restarted child's snapshot ends the error");

        wait_until("pid file written", || pid_file.is_file());
        let pid: i32 = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        drop(follower);

        // Verify child process was terminated promptly by Drop.
        wait_until("child process killed on drop", || {
            !Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        });
    }
}
