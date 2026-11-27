//! Session list, shape-deduplicated background poller, subagent nesting, and session query
//! resolver (`agent sessions --json`).
//!
//! Direct port of the session list and resolver in
//! `/Users/lugo/localdev/viz-web/glyph3d-js/cli/agentevents.go` (`refreshSessions`,
//! `sessionShape`, `resolve`, `lookup`), plus `nest_subagents` from
//! `native/src/agent_transcript/agent_cli.rs`.

use std::fmt;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cli::AgentCli;
use crate::events::SessionInfo;

/// Default session-list poll interval matching `agentSessionsPollInterval` (`3s`) in `agentevents.go`.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Run `agent sessions --json -q` once and return the parsed session list (newest first).
pub fn list_sessions(cli: &AgentCli) -> Result<Vec<SessionInfo>, String> {
    let args = ["sessions", "--json", "-q"];
    let stdout = cli.run(&args)?;
    let list: Option<Vec<SessionInfo>> = serde_json::from_slice(&stdout).map_err(|e| {
        format!(
            "`{}` printed invalid JSON: {e}",
            cli.describe(&args)
        )
    })?;
    Ok(list.unwrap_or_default())
}

/// Group openable sessions (non-empty `path`) into `(top_level, subagents)` pairs, keeping
/// subagents whose parent session is absent as standalone entries (`agent_cli::nest_subagents`).
pub fn nest_subagents(sessions: &[SessionInfo]) -> Vec<(&SessionInfo, Vec<&SessionInfo>)> {
    let openable: Vec<&SessionInfo> = sessions.iter().filter(|s| !s.path.is_empty()).collect();
    let listed = |id: &str, harness: &str| {
        openable
            .iter()
            .any(|s| s.session_id == id && s.harness == harness)
    };
    let mut out: Vec<(&SessionInfo, Vec<&SessionInfo>)> = Vec::new();
    for s in &openable {
        match &s.parent_session {
            Some(p) if listed(&p.session_id, &s.harness) => {}
            _ => out.push((s, Vec::new())),
        }
    }
    for s in &openable {
        if let Some(p) = &s.parent_session {
            if let Some((_, subs)) = out
                .iter_mut()
                .find(|(top, _)| top.session_id == p.session_id && top.harness == s.harness)
            {
                subs.push(s);
            }
        }
    }
    out
}

/// Snapshot of the cached session list and its shape generation counter.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListSnapshot {
    pub gen: u64,
    pub sessions: Vec<SessionInfo>,
}

/// Error returned when [`SessionStore::resolve`] cannot resolve a session query to a readable
/// transcript path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveError {
    EmptyQuery,
    Miss(String),
    Ambiguous {
        query: String,
        count: usize,
        sample: Vec<String>,
    },
    NoTranscript {
        query: String,
        session_id: String,
    },
    Cli(String),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyQuery => write!(f, "session is required"),
            Self::Miss(q) => write!(f, "no session matching {q:?}"),
            Self::Ambiguous {
                query,
                count,
                sample,
            } => write!(
                f,
                "session prefix {query:?} matches {count} sessions ({})",
                sample.join(", ")
            ),
            Self::NoTranscript { query, session_id } => {
                write!(f, "session {query:?} ({session_id}) has no transcript yet")
            }
            Self::Cli(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ResolveError {}

/// Canonical shape signature for `list` (`sessionShape` in `cli/agentevents.go:580-594`):
/// captures every picker/pairing-relevant field while ignoring `size`, `mtime`, `status_at`, and
/// slice ordering (an active session writes to its transcript on every event, which shifts `size`
/// and `mtime` and moves it to the top of `agent sessions`).
fn session_shape(list: &[SessionInfo]) -> String {
    let mut rows: Vec<String> = list
        .iter()
        .map(|s| {
            let wg = s
                .workgroup
                .as_ref()
                .map(|w| format!("{}:{}:{}:{}", w.name, w.parent, w.store, w.host))
                .unwrap_or_default();
            let ps = s
                .parent_session
                .as_ref()
                .map(|p| format!("{}:{}", p.session_id, p.tool_id))
                .unwrap_or_default();
            format!(
                "{}\x00{}\x00{}\x00{}\x00{}\x00{}\x00{}\x00{}\x00{}\x00{}\x00{}\x00{}",
                s.session_id,
                s.path,
                s.harness,
                s.cwd,
                s.project,
                s.title,
                s.agent_name,
                s.status,
                s.end_reason,
                wg,
                ps,
                s.continued_in,
            )
        })
        .collect();
    rows.sort();
    rows.join("\n")
}

enum LookupOutcome {
    Hit(Box<SessionInfo>),
    Miss,
    Ambiguous { count: usize, sample: Vec<String> },
}

fn lookup_in_list(list: &[SessionInfo], q: &str) -> LookupOutcome {
    for s in list {
        if s.session_id == q {
            return LookupOutcome::Hit(Box::new(s.clone()));
        }
    }
    for s in list {
        if !s.path.is_empty() && s.path == q {
            return LookupOutcome::Hit(Box::new(s.clone()));
        }
    }
    // Workgroup agent name: `list` is newest-first (`agent sessions --json`), and subagents inherit
    // their parent's workgroup metadata (`parent_session` distinguishes them).
    for s in list {
        if s.workgroup.as_ref().is_some_and(|w| w.name == q) && s.parent_session.is_none() {
            return LookupOutcome::Hit(Box::new(s.clone()));
        }
    }
    let matches: Vec<&SessionInfo> = list
        .iter()
        .filter(|s| s.session_id.starts_with(q))
        .collect();
    match matches.len() {
        1 => LookupOutcome::Hit(Box::new(matches[0].clone())),
        n if n > 1 => LookupOutcome::Ambiguous {
            count: n,
            sample: matches
                .iter()
                .take(3)
                .map(|s| s.session_id.clone())
                .collect(),
        },
        _ => LookupOutcome::Miss,
    }
}

#[derive(Default)]
struct StoreInner {
    polled: bool,
    gen: u64,
    shape: String,
    sessions: Vec<SessionInfo>,
    last_error: Option<String>,
}

/// Thread-safe cached session list and query resolver (`agentEvents` session state in `agentevents.go`).
pub struct SessionStore {
    cli: AgentCli,
    inner: Mutex<StoreInner>,
    refresh_gate: Mutex<()>,
}

impl SessionStore {
    pub fn new(cli: AgentCli) -> Self {
        Self {
            cli,
            inner: Mutex::new(StoreInner::default()),
            refresh_gate: Mutex::new(()),
        }
    }

    pub fn cli(&self) -> &AgentCli {
        &self.cli
    }

    /// Poll `agent sessions --json` now, updating cached `sessions` (including fresh `size` and
    /// `mtime`) and returning `Ok(true)` iff `gen` advanced (first successful poll or canonical
    /// session shape changed).
    ///
    /// If the CLI fails, `last_error` is updated and the previously cached session list and `gen`
    /// remain intact (`agentevents.go:550`).
    pub fn refresh(&self) -> Result<bool, String> {
        let _gate = self
            .refresh_gate
            .lock()
            .expect("session refresh mutex poisoned");
        match list_sessions(&self.cli) {
            Err(e) => {
                self.inner
                    .lock()
                    .expect("session store mutex poisoned")
                    .last_error = Some(e.clone());
                Err(e)
            }
            Ok(list) => {
                let shape = session_shape(&list);
                let mut guard = self.inner.lock().expect("session store mutex poisoned");
                let changed = !guard.polled || shape != guard.shape;
                guard.polled = true;
                guard.sessions = list;
                guard.last_error = None;
                if changed {
                    guard.shape = shape;
                    guard.gen += 1;
                }
                Ok(changed)
            }
        }
    }

    /// Return the latest [`SessionListSnapshot`], polling once on first call if never polled yet.
    pub fn snapshot(&self) -> SessionListSnapshot {
        let needs_poll = !self
            .inner
            .lock()
            .expect("session store mutex poisoned")
            .polled;
        if needs_poll {
            let _ = self.refresh();
        }
        let guard = self.inner.lock().expect("session store mutex poisoned");
        SessionListSnapshot {
            gen: guard.gen,
            sessions: guard.sessions.clone(),
        }
    }

    /// Most recent `agent sessions --json` error, if the latest poll failed.
    pub fn last_error(&self) -> Option<String> {
        self.inner
            .lock()
            .expect("session store mutex poisoned")
            .last_error
            .clone()
    }

    /// Resolve `query` (exact `session_id`, transcript `path`, workgroup `name`, or unique
    /// `session_id` prefix) to a [`SessionInfo`] with a non-empty transcript `path`.
    ///
    /// Re-polls `agent sessions --json` at most once on a cache miss (a session started since the
    /// last poll), never on an ambiguous prefix (`agentevents.go:640-668`).
    pub fn resolve(&self, query: &str) -> Result<SessionInfo, ResolveError> {
        let q = query.trim();
        if q.is_empty() {
            return Err(ResolveError::EmptyQuery);
        }

        let needs_initial = !self
            .inner
            .lock()
            .expect("session store mutex poisoned")
            .polled;
        if needs_initial {
            self.refresh().map_err(ResolveError::Cli)?;
        }

        let first = {
            let guard = self.inner.lock().expect("session store mutex poisoned");
            lookup_in_list(&guard.sessions, q)
        };

        let hit = match first {
            LookupOutcome::Hit(s) => *s,
            LookupOutcome::Ambiguous { count, sample } => {
                return Err(ResolveError::Ambiguous {
                    query: q.to_string(),
                    count,
                    sample,
                });
            }
            LookupOutcome::Miss => {
                if let Err(e) = self.refresh() {
                    let never_polled = !self
                        .inner
                        .lock()
                        .expect("session store mutex poisoned")
                        .polled;
                    if never_polled {
                        return Err(ResolveError::Cli(e));
                    }
                }
                let guard = self.inner.lock().expect("session store mutex poisoned");
                match lookup_in_list(&guard.sessions, q) {
                    LookupOutcome::Hit(s) => *s,
                    LookupOutcome::Ambiguous { count, sample } => {
                        return Err(ResolveError::Ambiguous {
                            query: q.to_string(),
                            count,
                            sample,
                        });
                    }
                    LookupOutcome::Miss => return Err(ResolveError::Miss(q.to_string())),
                }
            }
        };

        if hit.path.is_empty() {
            return Err(ResolveError::NoTranscript {
                query: q.to_string(),
                session_id: hit.session_id,
            });
        }
        Ok(hit)
    }
}

/// Background thread polling [`SessionStore::refresh`] on an interval and invoking `wake` whenever
/// the canonical session shape changes.
pub struct SessionPoller {
    store: Arc<SessionStore>,
    stop_pair: Arc<(Mutex<bool>, Condvar)>,
    thread: Mutex<Option<JoinHandle<()>>>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl SessionPoller {
    /// Start background session polling every `interval`, firing `wake` on initial load and
    /// whenever the canonical session shape changes.
    pub fn start<F>(cli: AgentCli, interval: Duration, wake: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        let store = Arc::new(SessionStore::new(cli));
        let stop_pair = Arc::new((Mutex::new(false), Condvar::new()));
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);

        let store_bg = Arc::clone(&store);
        let stop_bg = Arc::clone(&stop_pair);
        let wake_bg = Arc::clone(&wake);
        let poll_every = interval.max(Duration::from_millis(5));

        let handle = thread::Builder::new()
            .name("agent-feed-sessions".to_string())
            .spawn(move || {
                loop {
                    if *stop_bg.0.lock().expect("session poller mutex poisoned") {
                        return;
                    }
                    if let Ok(true) = store_bg.refresh() {
                        if *stop_bg.0.lock().expect("session poller mutex poisoned") {
                            return;
                        }
                        wake_bg();
                    }
                    let (lock, cvar) = &*stop_bg;
                    let guard = lock.lock().expect("session poller mutex poisoned");
                    if *guard {
                        return;
                    }
                    let (next, _) = cvar
                        .wait_timeout_while(guard, poll_every, |stopped| !*stopped)
                        .expect("session poller condvar poisoned");
                    if *next {
                        return;
                    }
                }
            })
            .expect("spawning agent-feed-sessions thread must succeed");

        Self {
            store,
            stop_pair,
            thread: Mutex::new(Some(handle)),
            wake,
        }
    }

    /// Underlying [`SessionStore`] handle (shareable with [`crate::transcript::TranscriptFollower`]).
    pub fn store(&self) -> &Arc<SessionStore> {
        &self.store
    }

    /// Latest cached [`SessionListSnapshot`].
    pub fn snapshot(&self) -> SessionListSnapshot {
        self.store.snapshot()
    }

    /// Force an immediate poll outside the timer schedule, firing `wake` if `gen` advanced.
    pub fn refresh_now(&self) -> Result<bool, String> {
        let changed = self.store.refresh()?;
        if changed {
            (self.wake)();
        }
        Ok(changed)
    }

    /// Resolve a session query against the shared store (re-polling once on miss).
    pub fn resolve(&self, query: &str) -> Result<SessionInfo, ResolveError> {
        self.store.resolve(query)
    }

    /// Latest CLI error from polling, if any.
    pub fn last_error(&self) -> Option<String> {
        self.store.last_error()
    }

    /// Stop the polling thread cleanly.
    pub fn stop(&self) {
        {
            let (lock, cvar) = &*self.stop_pair;
            let mut stopped = lock.lock().expect("session poller mutex poisoned");
            *stopped = true;
            cvar.notify_all();
        }
        if let Some(handle) = self
            .thread
            .lock()
            .expect("session poller join mutex poisoned")
            .take()
        {
            let _ = handle.join();
        }
    }
}

impl Drop for SessionPoller {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::Instant;

    use super::*;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "agent-feed-sessions-{label}-{}-{n}",
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

    #[test]
    fn resolve_session_by_id_path_workgroup_prefix_and_repoll_on_miss() {
        let scratch = Scratch::new("resolve");
        let sessions_json = scratch.0.join("sessions.json");
        let count_file = scratch.0.join("calls");
        let stub = scratch.0.join("agent");

        write_stub(
            &stub,
            &format!(
                "#!/bin/sh\n\
                 n=0; [ -f '{calls}' ] && n=$(cat '{calls}'); n=$((n+1)); printf '%s' \"$n\" > '{calls}'\n\
                 cat '{json}'\n",
                calls = count_file.display(),
                json = sessions_json.display(),
            ),
        );
        let calls = || -> usize {
            fs::read_to_string(&count_file)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0)
        };

        let p_lead = "/tmp/lead.jsonl";
        let p_sub = "/tmp/sub.jsonl";
        let p_two = "/tmp/two.jsonl";
        fs::write(
            &sessions_json,
            serde_json::json!([
                {
                    "session_id": "aaaa1111-0000",
                    "harness": "claude",
                    "path": p_sub,
                    "workgroup": {"name": "lead"},
                    "parent_session": {"session_id": "aaaa2222-0000", "tool_id": "t1"}
                },
                {
                    "session_id": "aaaa2222-0000",
                    "harness": "claude",
                    "path": p_lead,
                    "workgroup": {"name": "lead"}
                },
                {
                    "session_id": "bbbb3333-0000",
                    "harness": "agy",
                    "path": p_two,
                    "workgroup": {"name": "worker"}
                },
                {
                    "session_id": "notranscript",
                    "harness": "claude",
                    "path": "",
                    "workgroup": {"name": "fresh"}
                }
            ])
            .to_string(),
        )
        .unwrap();

        let store = SessionStore::new(AgentCli::new(&stub));

        // Exact session_id, transcript path, workgroup name (skipping subagent!), unique prefix.
        assert_eq!(store.resolve("aaaa2222-0000").unwrap().path, p_lead);
        assert_eq!(store.resolve(p_two).unwrap().session_id, "bbbb3333-0000");
        assert_eq!(
            store.resolve("lead").unwrap().session_id,
            "aaaa2222-0000",
            "workgroup lookup skips subagent with parent_session"
        );
        assert_eq!(store.resolve("bbbb").unwrap().session_id, "bbbb3333-0000");
        assert_eq!(calls(), 1, "all hits served from single cached poll");

        // Ambiguous prefix fails WITHOUT re-polling!
        let amb = store.resolve("aaaa").unwrap_err();
        assert!(matches!(amb, ResolveError::Ambiguous { count: 2, .. }));
        assert_eq!(calls(), 1, "ambiguity never triggers a re-poll");

        // Session without transcript fails naming "no transcript yet".
        let notr = store.resolve("fresh").unwrap_err();
        assert!(notr.to_string().contains("no transcript yet"));

        // Newly arrived session triggers exactly ONE re-poll on miss and resolves.
        fs::write(
            &sessions_json,
            serde_json::json!([
                {
                    "session_id": "cccc4444-0000",
                    "harness": "claude",
                    "path": "/tmp/new.jsonl",
                    "workgroup": {"name": "newcomer"}
                }
            ])
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            store.resolve("newcomer").unwrap().session_id,
            "cccc4444-0000"
        );
        assert_eq!(calls(), 2);

        // True miss re-polls once and returns Miss.
        assert!(matches!(
            store.resolve("nobody").unwrap_err(),
            ResolveError::Miss(_)
        ));
        assert_eq!(calls(), 3);
    }

    #[test]
    fn session_shape_ignores_size_mtime_status_at_and_reordering_but_fires_on_real_changes() {
        let scratch = Scratch::new("shape");
        let sessions_json = scratch.0.join("sessions.json");
        let fail_flag = scratch.0.join("fail");
        let stub = scratch.0.join("agent");

        write_stub(
            &stub,
            &format!(
                "#!/bin/sh\n\
                 if [ -f '{fail}' ]; then echo 'boom' >&2; exit 1; fi\n\
                 cat '{json}'\n",
                fail = fail_flag.display(),
                json = sessions_json.display(),
            ),
        );

        fs::write(
            &sessions_json,
            serde_json::json!([
                {"session_id":"s1","harness":"claude","path":"/p/1","size":100,"mtime":"2026-10-12T10:00:00Z","status":"processing","status_at":"2026-10-12T10:00:00Z"},
                {"session_id":"s2","harness":"agy","path":"/p/2","size":200,"mtime":"2026-10-12T09:00:00Z","status":"idle"}
            ])
            .to_string(),
        )
        .unwrap();

        let store = SessionStore::new(AgentCli::new(&stub));
        assert!(store.refresh().unwrap());
        assert_eq!(store.snapshot().gen, 1);

        // Size, mtime, status_at, AND order change: shape does NOT change (gen stays 1),
        // yet snapshot() returns the fresh size/mtime!
        fs::write(
            &sessions_json,
            serde_json::json!([
                {"session_id":"s2","harness":"agy","path":"/p/2","size":250,"mtime":"2026-10-12T10:02:00Z","status":"idle"},
                {"session_id":"s1","harness":"claude","path":"/p/1","size":9999,"mtime":"2026-10-12T10:03:00Z","status":"processing","status_at":"2026-10-12T10:03:00Z"}
            ])
            .to_string(),
        )
        .unwrap();
        assert!(!store.refresh().unwrap(), "growth + reorder must not wake");
        let snap = store.snapshot();
        assert_eq!(snap.gen, 1);
        assert_eq!(
            snap.sessions
                .iter()
                .find(|s| s.session_id == "s1")
                .unwrap()
                .size,
            9999,
            "fresh size still stored even without shape bump"
        );

        // Status change bumps gen!
        fs::write(
            &sessions_json,
            serde_json::json!([
                {"session_id":"s2","harness":"agy","path":"/p/2","size":250,"status":"idle"},
                {"session_id":"s1","harness":"claude","path":"/p/1","size":9999,"status":"waiting"}
            ])
            .to_string(),
        )
        .unwrap();
        assert!(store.refresh().unwrap());
        assert_eq!(store.snapshot().gen, 2);

        // Failed poll preserves last good list and gen while recording last_error.
        fs::write(&fail_flag, "1").unwrap();
        assert!(store.refresh().is_err());
        assert_eq!(store.snapshot().gen, 2);
        assert_eq!(store.snapshot().sessions.len(), 2);
        assert!(store.last_error().unwrap().contains("boom"));
    }

    #[test]
    fn poller_wakes_on_initial_and_changed_shapes_and_stops_cleanly() {
        let scratch = Scratch::new("poller");
        let sessions_json = scratch.0.join("sessions.json");
        let stub = scratch.0.join("agent");
        write_stub(&stub, &format!("#!/bin/sh\ncat '{}'\n", sessions_json.display()));
        fs::write(
            &sessions_json,
            r#"[{"session_id":"a","harness":"claude","path":"/p/a","status":"idle"}]"#,
        )
        .unwrap();

        let wakes = Arc::new(AtomicUsize::new(0));
        let wakes_cb = Arc::clone(&wakes);
        let poller = SessionPoller::start(
            AgentCli::new(&stub),
            Duration::from_millis(20),
            move || {
                wakes_cb.fetch_add(1, Ordering::SeqCst);
            },
        );

        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline && wakes.load(Ordering::SeqCst) < 1 {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        assert_eq!(poller.snapshot().gen, 1);

        // Add a second session -> poller wakes exactly once more.
        fs::write(
            &sessions_json,
            r#"[{"session_id":"a","harness":"claude","path":"/p/a","status":"idle"},{"session_id":"b","harness":"claude","path":"/p/b","status":"processing"}]"#,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline && wakes.load(Ordering::SeqCst) < 2 {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
        assert_eq!(poller.snapshot().gen, 2);
        poller.stop();
    }

    #[test]
    fn nest_subagents_groups_children_under_listed_parent() {
        let list: Vec<SessionInfo> = serde_json::from_value(serde_json::json!([
            {"session_id":"parent","harness":"claude","path":"/p/parent"},
            {"session_id":"child1","harness":"claude","path":"/p/c1","parent_session":{"session_id":"parent"}},
            {"session_id":"orphan","harness":"claude","path":"/p/orphan","parent_session":{"session_id":"gone"}},
            {"session_id":"empty","harness":"claude","path":""}
        ]))
        .unwrap();
        let nested = nest_subagents(&list);
        assert_eq!(nested.len(), 2);
        assert_eq!(nested[0].0.session_id, "parent");
        assert_eq!(nested[0].1.len(), 1);
        assert_eq!(nested[0].1[0].session_id, "child1");
        assert_eq!(nested[1].0.session_id, "orphan");
    }
}
