//! Per-Agent-View transcript reader and `--follow` stream state.
//!
//! Combines the two halves that power an Agent View in the browser wall:
//!
//! 1. **Windowed reading & newline-scanning tail math** from
//!    `/Users/lugo/localdev/viz-web/glyph3d-js/cli/agentevents.go` ([`tail_from`],
//!    [`read_transcript_window`]): computes the `--from` byte offset for the last ~256 KiB of a
//!    transcript in O(window) I/O without parsing the transcript, treats a live session whose
//!    transcript file has not yet been created on disk as `{lines: [], size: 0}`, and enforces
//!    [`MAX_READ_BYTES`] (`64 MiB`).
//! 2. **Stream folding, provisional-reply pruning, `load_earlier`, and orphan adoption** from
//!    `/Users/lugo/localdev/viz-web/glyph3d-js/wall/WallAgentViews.js` ([`TranscriptState`],
//!    [`TranscriptFollower`]):
//!    - Hydrates the initial 256 KiB tail window, tracking each event's first-seen [`EventPos`],
//!      placing `op: "update"` frames whose `add` preceded the window into `orphans`, and
//!      computing `oldest_at`, `has_earlier`, and `last_at` (the highest `at` of non-provisional
//!      events).
//!    - Drops uncommitted provisional `say` events (`say.provisional == true`) before spawning
//!      `agent transcript <path> --follow --from <last_at>` so the live stream's fresh Stop-hook
//!      or committed transcript reply never collides with a stale provisional entry.
//!    - Folds live `add` and `update` frames in place (preserving original `pos` on updates and
//!      recording [`LiveAddNotice`] entries so the host can stamp terminal scrollback anchors).
//!    - Pages older history via [`TranscriptFollower::load_earlier`] (`until = oldest_at`,
//!      `tail = window_bytes`), prepending newly uncovered events while adopting any newer state
//!      already waiting in `orphans`.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read as _, Seek as _, SeekFrom};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use serde::{Deserialize, Serialize};

use crate::cli::AgentCli;
use crate::events::{
    parse_stream_line, AgentEvent, EventOp, EventPos, EventRecord, SessionInfo, StreamFrame,
};
use crate::sessions::SessionStore;

/// Initial tail and per-`load_earlier` byte window (`STREAM_WINDOW_BYTES = 256 KiB` in `WallAgentViews.js`).
pub const STREAM_WINDOW_BYTES: u64 = 256 * 1024;

/// Hard ceiling on a single `read_transcript_window` response (`agentEventsMaxReadBytes = 64 MiB`
/// in `cli/agentevents.go`).
pub const MAX_READ_BYTES: u64 = 64 * 1024 * 1024;

const NEWLINE_SCAN_CHUNK: usize = 64 * 1024;

fn read_at_full(file: &mut File, mut buf: &mut [u8], mut offset: u64) -> io::Result<usize> {
    file.seek(SeekFrom::Start(offset))?;
    let mut total = 0usize;
    while !buf.is_empty() {
        match file.read(buf) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                offset += n as u64;
                let _ = offset;
                buf = &mut buf[n..];
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

/// Offset of the last `'\n'` strictly before `p`, or `None` (`lastNewlineBefore` in `agentevents.go`).
fn last_newline_before(file: &mut File, p: u64) -> io::Result<Option<u64>> {
    let mut buf = vec![0u8; NEWLINE_SCAN_CHUNK];
    let mut hi = p;
    while hi > 0 {
        let lo = hi.saturating_sub(NEWLINE_SCAN_CHUNK as u64);
        let len = (hi - lo) as usize;
        let n = read_at_full(file, &mut buf[..len], lo)?;
        if let Some(i) = buf[..n].iter().rposition(|&b| b == b'\n') {
            return Ok(Some(lo + i as u64));
        }
        hi = lo;
    }
    Ok(None)
}

/// Offset of the first `'\n'` in `[p.max(0), end)`, or `None` (`firstNewlineFrom` in `agentevents.go`).
fn first_newline_from(file: &mut File, p: i64, end: u64) -> io::Result<Option<u64>> {
    let mut buf = vec![0u8; NEWLINE_SCAN_CHUNK];
    let mut lo = if p < 0 { 0u64 } else { p as u64 };
    while lo < end {
        let len = (end - lo).min(NEWLINE_SCAN_CHUNK as u64) as usize;
        let n = read_at_full(file, &mut buf[..len], lo)?;
        if n == 0 {
            break;
        }
        if let Some(i) = buf[..n].iter().position(|&b| b == b'\n') {
            return Ok(Some(lo + i as u64));
        }
        lo += n as u64;
    }
    Ok(None)
}

/// Compute the `--from` byte offset for reading the last `~n` bytes of `file`'s first `size` bytes
/// (`tailFrom` in `cli/agentevents.go:813-851`).
///
/// Returns:
/// - `Ok(None)` when the window starts at byte 0 (`size <= n`, no complete line yet, or the
///   chosen window includes the first line) — callers omit `--from` and read from the start.
/// - `Ok(Some(from_offset))` when `--from <from_offset>` should be passed to `agent transcript`:
///   because `agent transcript --from X` resumes *after* the record starting at `X`, `from_offset`
///   is the start of the complete line immediately *before* the window.
pub fn tail_from(path: &Path, size: u64, n: u64) -> Result<Option<u64>, String> {
    if n == 0 {
        return Err("tail must be a positive byte count".to_string());
    }
    let mut file = File::open(path)
        .map_err(|e| format!("could not open transcript {}: {e}", path.display()))?;
    tail_from_file(&mut file, size, n)
        .map_err(|e| format!("could not scan transcript {}: {e}", path.display()))
}

pub(crate) fn tail_from_file(file: &mut File, size: u64, n: u64) -> io::Result<Option<u64>> {
    if n >= size {
        return Ok(None);
    }
    let Some(last) = last_newline_before(file, size)? else {
        return Ok(None); // no complete line at all
    };
    let end = last + 1; // complete lines end here
    let target = size - n;
    let mut w: Option<u64> = None;
    if target < end {
        let scan_from = i64::try_from(target).unwrap_or(i64::MAX) - 1;
        if let Some(nl) = first_newline_from(file, scan_from, end)? {
            if nl + 1 < end {
                w = Some(nl + 1);
            }
        }
    }
    let w = match w {
        Some(pos) => pos,
        None => {
            // No complete line starts inside [target, end): fall back to the last complete line.
            match last_newline_before(file, last)? {
                Some(prev) => prev + 1,
                None => 0,
            }
        }
    };
    if w == 0 {
        return Ok(None);
    }
    let prev_start = match last_newline_before(file, w - 1)? {
        Some(prev) => prev + 1,
        None => 0,
    };
    Ok(Some(prev_start))
}

/// Parameters for [`read_transcript_window`] (`readParams` in `cli/agentevents.go`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WindowParams {
    /// Exclusive resume offset (`--from`). Mutually exclusive with `tail`.
    pub from: Option<u64>,
    /// Inclusive end offset (`--until`), clamped to the transcript's current byte length.
    pub until: Option<u64>,
    /// Approximate byte count from the end of `[0, until)` (`tail_from`). Mutually exclusive with `from`.
    pub tail: Option<u64>,
}

/// Result of [`read_transcript_window`] (`readResult` in `cli/agentevents.go`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptWindow {
    /// Raw JSONL lines emitted by `agent transcript`.
    pub lines: Vec<String>,
    /// File byte bound `until` used for this read (`0` when a live session has not written its
    /// transcript yet).
    pub size: u64,
}

/// Read a bounded byte window of `session`'s transcript via `agent transcript` (`readWindow` in
/// `cli/agentevents.go:720-805`).
pub fn read_transcript_window(
    cli: &AgentCli,
    session: &SessionInfo,
    params: &WindowParams,
) -> Result<TranscriptWindow, String> {
    if params.tail.is_some() && params.from.is_some() {
        return Err("tail and from cannot both be set".to_string());
    }
    if params.tail == Some(0) {
        return Err("tail must be positive".to_string());
    }
    if session.path.is_empty() {
        return Err(format!(
            "session {:?} has no transcript path",
            session.session_id
        ));
    }

    let path = Path::new(&session.path);
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound && session.runs() => {
            // A freshly spawned workgroup agent has a session path before its CLI writes line 1.
            return Ok(TranscriptWindow {
                lines: Vec::new(),
                size: 0,
            });
        }
        Err(e) => return Err(format!("open transcript {}: {e}", path.display())),
    };

    let stat_size = file
        .metadata()
        .map_err(|e| format!("stat transcript {}: {e}", path.display()))?
        .len();
    let size = params.until.map_or(stat_size, |u| u.min(stat_size));

    let effective_from = match params.tail {
        Some(n) => tail_from_file(&mut file, size, n)
            .map_err(|e| format!("scan transcript {}: {e}", path.display()))?,
        None => params.from,
    };
    drop(file);

    let mut owned_args = vec![
        "transcript".to_string(),
        session.path.clone(),
        "--until".to_string(),
        size.to_string(),
        "-q".to_string(),
    ];
    if let Some(from) = effective_from {
        owned_args.push("--from".to_string());
        owned_args.push(from.to_string());
    }
    let arg_refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();

    let mut cmd = cli.command(&arg_refs);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = crate::cli::spawn_command(&mut cmd)
        .map_err(|e| format!("could not run `{}`: {e}", cli.describe(&arg_refs)))?;
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let stderr_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.take(4096).read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).trim().to_string()
    });

    let mut lines = Vec::new();
    let mut total_bytes: u64 = 0;
    let mut overflow = false;
    let mut read_err: Option<String> = None;

    for raw in BufReader::new(stdout).lines() {
        match raw {
            Ok(line) => {
                if line.is_empty() {
                    continue;
                }
                total_bytes += line.len() as u64 + 1;
                if total_bytes > MAX_READ_BYTES {
                    overflow = true;
                    let _ = child.kill();
                    break;
                }
                lines.push(line);
            }
            Err(e) => {
                read_err = Some(format!("transcript stdout read error: {e}"));
                let _ = child.kill();
                break;
            }
        }
    }

    let status = child.wait().ok();
    let stderr_text = stderr_handle.join().unwrap_or_default();

    if overflow {
        return Err(format!(
            "transcript output exceeds {} MiB; pass `from` or `tail` to read a window",
            MAX_READ_BYTES >> 20
        ));
    }
    if let Some(err) = read_err {
        return Err(err);
    }
    if let Some(st) = status {
        if !st.success() {
            return Err(format!(
                "`{}` failed ({st}): {stderr_text}",
                cli.describe(&arg_refs)
            ));
        }
    }

    Ok(TranscriptWindow { lines, size })
}

/// One folded event entry in [`TranscriptState`], pairing the latest [`AgentEvent`] with the
/// [`EventPos`] of its original `add`.
#[derive(Clone, Debug, PartialEq)]
pub struct FoldedEntry {
    pub pos: EventPos,
    pub event: AgentEvent,
}

impl FoldedEntry {
    /// Convert into an [`EventRecord`] whose `at` preserves the original `add` byte offset
    /// (`WallAgentViews.js:1032`).
    pub fn to_event_record(&self, show_noise: bool) -> Option<EventRecord> {
        self.event
            .to_event_record(Some(self.pos.at as i64), show_noise)
    }
}

/// Notification recorded whenever a live `add` frame arrives on `--follow`, allowing the host UI
/// to stamp a terminal scrollback anchor (`WallAgentViews.js:488-496`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveAddNotice {
    pub id: String,
    pub protocol_terminal_line: Option<u64>,
}

/// Result of folding a single JSONL line into [`TranscriptState`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FoldOutcome {
    Session,
    Added(String),
    UpdatedInPlace(String),
    OrphanedUpdate(String),
}

/// Pure windowed & live folding state for one Agent View (`WallAgentViews.js`).
#[derive(Clone, Debug, Default)]
pub struct TranscriptState {
    session: Option<SessionInfo>,
    entries: HashMap<String, FoldedEntry>,
    orphans: HashMap<String, AgentEvent>,
    order: Vec<String>,
    last_at: Option<u64>,
    oldest_at: u64,
    has_earlier: bool,
    live_adds: Vec<LiveAddNotice>,
    gen: u64,
}

impl TranscriptState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn gen(&self) -> u64 {
        self.gen
    }

    pub fn session(&self) -> Option<&SessionInfo> {
        self.session.as_ref()
    }

    pub fn set_session_fallback(&mut self, info: SessionInfo) {
        if self.session.is_none() {
            self.session = Some(info);
        }
    }

    /// Byte offset of the newest non-provisional event folded so far (`--from` cursor for `--follow`).
    pub fn last_at(&self) -> Option<u64> {
        self.last_at
    }

    /// Lowest `pos.at` among folded entries (`0` if empty or the stream starts at the first event).
    pub fn oldest_at(&self) -> u64 {
        self.oldest_at
    }

    /// Whether older events precede the loaded tail window (`oldest_at > 0`).
    pub fn has_earlier(&self) -> bool {
        self.has_earlier
    }

    /// Number of visible folded entries in `order`.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Ordered event IDs from oldest loaded to newest.
    pub fn order(&self) -> &[String] {
        &self.order
    }

    /// Pending orphan updates whose `add` occurred before the loaded tail window.
    pub fn orphan_count(&self) -> usize {
        self.orphans.len()
    }

    /// Iterate folded entries in display order.
    pub fn entries(&self) -> impl Iterator<Item = &FoldedEntry> {
        self.order.iter().filter_map(|id| self.entries.get(id))
    }

    /// Build normalized [`EventRecord`]s in display order (`WallAgentViews.rebuildRows` input).
    pub fn event_records(&self, show_noise: bool) -> Vec<EventRecord> {
        self.entries()
            .filter_map(|e| e.to_event_record(show_noise))
            .collect()
    }

    /// Drain [`LiveAddNotice`] items accumulated from live `--follow` `add` frames.
    pub fn take_live_adds(&mut self) -> Vec<LiveAddNotice> {
        std::mem::take(&mut self.live_adds)
    }

    /// Fold one JSONL line from `agent transcript` (`foldFrame` in `WallAgentViews.js:455-511`).
    ///
    /// When `live` is `true`, new `add` frames also push a [`LiveAddNotice`]; when `live` is
    /// `false` (initial tail hydration), `oldest_at` and `has_earlier` are recomputed via
    /// [`Self::finish_initial_hydration`].
    pub fn fold_line(&mut self, raw: &str, live: bool) -> Result<FoldOutcome, String> {
        match parse_stream_line(raw)? {
            StreamFrame::Session(info) => {
                self.session = Some(*info);
                self.gen += 1;
                Ok(FoldOutcome::Session)
            }
            StreamFrame::Event { op, event } => {
                let ev = *event;
                if !ev.is_provisional_reply() {
                    self.last_at = Some(self.last_at.map_or(ev.at, |cur| cur.max(ev.at)));
                }
                let id = ev.id.clone();
                if let Some(existing) = self.entries.get_mut(&id) {
                    existing.event = ev;
                    self.gen += 1;
                    return Ok(FoldOutcome::UpdatedInPlace(id));
                }
                if op == EventOp::Update {
                    // Seen in the tail window without its earlier `add`; keep for `load_earlier`.
                    self.orphans.insert(id.clone(), ev);
                    self.gen += 1;
                    return Ok(FoldOutcome::OrphanedUpdate(id));
                }
                let pos = ev.pos();
                let proto_line = ev.terminal_line;
                self.entries
                    .insert(id.clone(), FoldedEntry { pos, event: ev });
                self.order.push(id.clone());
                if live {
                    self.live_adds.push(LiveAddNotice {
                        id: id.clone(),
                        protocol_terminal_line: proto_line,
                    });
                }
                self.gen += 1;
                Ok(FoldOutcome::Added(id))
            }
        }
    }

    /// Recompute `oldest_at` and `has_earlier` after initial tail hydration (`WallAgentViews.js:856-864`).
    pub fn finish_initial_hydration(&mut self) {
        let mut oldest = u64::MAX;
        for entry in self.entries.values() {
            if entry.pos.at < oldest {
                oldest = entry.pos.at;
            }
        }
        self.oldest_at = if oldest == u64::MAX { 0 } else { oldest };
        self.has_earlier = self.oldest_at > 0;
    }

    /// Drop uncommitted provisional `say` replies before starting `agent transcript --follow`
    /// (`WallAgentViews.js:881-890`).
    ///
    /// Returns the number of provisional entries removed.
    pub fn prune_provisional_replies(&mut self) -> usize {
        let provisional_ids: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.event.is_provisional_reply())
            .map(|(id, _)| id.clone())
            .collect();
        if provisional_ids.is_empty() {
            return 0;
        }
        for id in &provisional_ids {
            self.entries.remove(id);
        }
        self.order.retain(|id| self.entries.contains_key(id));
        self.gen += 1;
        provisional_ids.len()
    }

    /// Fold an older page of JSONL lines (`until = oldest_at`), prepending newly uncovered `add`
    /// entries to `order` and adopting any newer state waiting in `orphans` (`WallAgentViews.js:910-953`).
    ///
    /// Returns the number of older entries prepended.
    pub fn apply_earlier_lines(&mut self, lines: &[String]) -> Result<usize, String> {
        let mut older_map: HashMap<String, FoldedEntry> = HashMap::new();
        let mut older_ids: Vec<String> = Vec::new();
        let mut older_orphans: HashMap<String, AgentEvent> = HashMap::new();
        let mut oldest = self.oldest_at;

        for raw in lines {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            match parse_stream_line(trimmed)? {
                StreamFrame::Session(info) => {
                    if self.session.is_none() {
                        self.session = Some(*info);
                    }
                }
                StreamFrame::Event { op, event } => {
                    let ev = *event;
                    let id = ev.id.clone();
                    if self.entries.contains_key(&id) {
                        continue;
                    }
                    if let Some(prev) = older_map.get_mut(&id) {
                        prev.event = ev;
                        continue;
                    }
                    if op == EventOp::Update {
                        older_orphans.insert(id, ev);
                        continue;
                    }
                    let pos = ev.pos();
                    if pos.at < oldest {
                        oldest = pos.at;
                    }
                    older_map.insert(id.clone(), FoldedEntry { pos, event: ev });
                    older_ids.push(id);
                }
            }
        }

        // Adopt newer updates already seen in the tail/live stream while preserving the older add's `pos`.
        for (eid, entry) in &mut older_map {
            if let Some(newer) = self.orphans.remove(eid) {
                entry.event = newer;
            }
        }
        for (eid, ev) in older_orphans {
            self.orphans.entry(eid).or_insert(ev);
        }

        if !older_ids.is_empty() {
            for (eid, entry) in older_map {
                self.entries.insert(eid, entry);
            }
            let mut combined = older_ids.clone();
            combined.append(&mut self.order);
            self.order = combined;
            self.oldest_at = oldest;
            self.has_earlier = oldest > 0;
        } else {
            self.oldest_at = 0;
            self.has_earlier = false;
        }
        self.gen += 1;
        Ok(older_ids.len())
    }
}

/// Snapshot of a [`TranscriptFollower`]'s current state for rendering an Agent View.
#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptSnapshot {
    pub gen: u64,
    pub session: SessionInfo,
    pub entries: Vec<FoldedEntry>,
    pub oldest_at: u64,
    pub has_earlier: bool,
    pub following: bool,
    pub ended_error: Option<String>,
}

impl TranscriptSnapshot {
    /// Project all folded entries into normalized [`EventRecord`]s for `wall_core::agent_view`.
    pub fn event_records(&self, show_noise: bool) -> Vec<EventRecord> {
        self.entries
            .iter()
            .filter_map(|e| e.to_event_record(show_noise))
            .collect()
    }
}

/// Configuration for [`TranscriptFollower::open`].
#[derive(Clone, Debug)]
pub struct TranscriptOptions {
    /// Byte window per initial tail read and per `load_earlier` page (default [`STREAM_WINDOW_BYTES`]).
    pub window_bytes: u64,
    /// Whether to start `agent transcript --follow` when `session.status != "ended"` (default `true`).
    pub follow_live: bool,
}

impl Default for TranscriptOptions {
    fn default() -> Self {
        Self {
            window_bytes: STREAM_WINDOW_BYTES,
            follow_live: true,
        }
    }
}

struct TranscriptInner {
    state: TranscriptState,
    resolved_session: SessionInfo,
    following: bool,
    stopped: bool,
    ended_error: Option<String>,
}

/// Per-Agent-View transcript reader & live `--follow` supervisor.
pub struct TranscriptFollower {
    cli: AgentCli,
    window_bytes: u64,
    inner: Arc<Mutex<TranscriptInner>>,
    child: Arc<Mutex<Option<Child>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl TranscriptFollower {
    /// Resolve `query` via `sessions` and open a [`TranscriptFollower`] on it.
    pub fn open_query<F>(
        sessions: &SessionStore,
        query: &str,
        opts: TranscriptOptions,
        wake: F,
    ) -> Result<Self, String>
    where
        F: Fn() + Send + Sync + 'static,
    {
        let session = sessions.resolve(query).map_err(|e| e.to_string())?;
        Self::open(sessions.cli().clone(), session, opts, wake)
    }

    /// Hydrate the initial tail window of `session` and, unless `session.status == "ended"` or
    /// `!opts.follow_live`, spawn `agent transcript <path> --follow` from `last_at`.
    pub fn open<F>(
        cli: AgentCli,
        session: SessionInfo,
        opts: TranscriptOptions,
        wake: F,
    ) -> Result<Self, String>
    where
        F: Fn() + Send + Sync + 'static,
    {
        let window_bytes = if opts.window_bytes == 0 {
            STREAM_WINDOW_BYTES
        } else {
            opts.window_bytes
        };

        let win = read_transcript_window(
            &cli,
            &session,
            &WindowParams {
                from: None,
                until: None,
                tail: Some(window_bytes),
            },
        )?;

        let mut state = TranscriptState::new();
        state.set_session_fallback(session.clone());
        for line in &win.lines {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            state.fold_line(trimmed, false)?;
        }
        state.finish_initial_hydration();

        let should_follow = opts.follow_live && session.status != "ended";
        let from_cursor = if should_follow {
            let cursor = state.last_at();
            state.prune_provisional_replies();
            cursor
        } else {
            None
        };

        let inner = Arc::new(Mutex::new(TranscriptInner {
            state,
            resolved_session: session.clone(),
            following: should_follow,
            stopped: false,
            ended_error: None,
        }));
        let child_slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);

        let thread_handle = if should_follow {
            let cli_bg = cli.clone();
            let path = session.path.clone();
            let inner_bg = Arc::clone(&inner);
            let child_bg = Arc::clone(&child_slot);
            let wake_bg = Arc::clone(&wake);
            Some(
                thread::Builder::new()
                    .name(format!("agent-feed-tx-{}", session.session_id))
                    .spawn(move || {
                        run_transcript_follow(cli_bg, path, from_cursor, inner_bg, child_bg, wake_bg)
                    })
                    .expect("spawning agent-feed-tx thread must succeed"),
            )
        } else {
            None
        };

        Ok(Self {
            cli,
            window_bytes,
            inner,
            child: child_slot,
            thread: Mutex::new(thread_handle),
            wake,
        })
    }

    /// Snapshot current session metadata, ordered entries, `has_earlier`, and follow status.
    pub fn snapshot(&self) -> TranscriptSnapshot {
        let guard = self.inner.lock().expect("transcript follower mutex poisoned");
        let session = guard
            .state
            .session()
            .cloned()
            .unwrap_or_else(|| guard.resolved_session.clone());
        TranscriptSnapshot {
            gen: guard.state.gen(),
            session,
            entries: guard.state.entries().cloned().collect(),
            oldest_at: guard.state.oldest_at(),
            has_earlier: guard.state.has_earlier(),
            following: guard.following,
            ended_error: guard.ended_error.clone(),
        }
    }

    /// Drain live `add` notifications accumulated since the previous call.
    pub fn take_live_adds(&self) -> Vec<LiveAddNotice> {
        self.inner
            .lock()
            .expect("transcript follower mutex poisoned")
            .state
            .take_live_adds()
    }

    /// Load one older `window_bytes` page before `oldest_at`, adopting any orphan updates already
    /// seen in the tail/live stream and invoking `wake`.
    ///
    /// Returns `Ok(prepended_count)`, or `Ok(0)` immediately if `!has_earlier`.
    pub fn load_earlier(&self) -> Result<usize, String> {
        let (session, oldest_at, has_earlier) = {
            let guard = self.inner.lock().expect("transcript follower mutex poisoned");
            (
                guard.resolved_session.clone(),
                guard.state.oldest_at(),
                guard.state.has_earlier(),
            )
        };
        if !has_earlier || oldest_at == 0 {
            return Ok(0);
        }

        let win = read_transcript_window(
            &self.cli,
            &session,
            &WindowParams {
                from: None,
                until: Some(oldest_at),
                tail: Some(self.window_bytes),
            },
        )?;

        let added = {
            let mut guard = self.inner.lock().expect("transcript follower mutex poisoned");
            guard.state.apply_earlier_lines(&win.lines)?
        };
        (self.wake)();
        Ok(added)
    }

    /// Stop the `agent transcript --follow` child process and background reader thread.
    pub fn stop(&self) {
        {
            let mut guard = self.inner.lock().expect("transcript follower mutex poisoned");
            guard.stopped = true;
            guard.following = false;
        }
        if let Some(child) = self
            .child
            .lock()
            .expect("transcript child mutex poisoned")
            .as_mut()
        {
            let _ = child.kill();
        }
        if let Some(handle) = self
            .thread
            .lock()
            .expect("transcript join mutex poisoned")
            .take()
        {
            let _ = handle.join();
        }
    }
}

impl Drop for TranscriptFollower {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_transcript_follow(
    cli: AgentCli,
    path: String,
    from: Option<u64>,
    inner: Arc<Mutex<TranscriptInner>>,
    child_slot: Arc<Mutex<Option<Child>>>,
    wake: Arc<dyn Fn() + Send + Sync>,
) {
    let mut owned_args = vec![
        "transcript".to_string(),
        path,
        "--follow".to_string(),
        "-q".to_string(),
    ];
    if let Some(off) = from {
        owned_args.push("--from".to_string());
        owned_args.push(off.to_string());
    }
    let arg_refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();

    let mut cmd = cli.command(&arg_refs);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match crate::cli::spawn_command(&mut cmd) {
        Ok(c) => c,
        Err(e) => {
            let mut guard = inner.lock().expect("transcript follower mutex poisoned");
            guard.following = false;
            if !guard.stopped {
                guard.ended_error = Some(format!(
                    "could not spawn `{}`: {e}",
                    cli.describe(&arg_refs)
                ));
                drop(guard);
                wake();
            }
            return;
        }
    };

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let stderr_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.take(4096).read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).trim().to_string()
    });

    {
        let stopped = inner
            .lock()
            .expect("transcript follower mutex poisoned")
            .stopped;
        let mut slot = child_slot.lock().expect("transcript child mutex poisoned");
        if stopped {
            let _ = child.kill();
        }
        *slot = Some(child);
    }

    let mut stream_err: Option<String> = None;
    for raw in BufReader::new(stdout).lines() {
        match raw {
            Ok(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let should_wake = {
                    let mut guard = inner.lock().expect("transcript follower mutex poisoned");
                    if guard.stopped {
                        break;
                    }
                    match guard.state.fold_line(trimmed, true) {
                        Ok(_) => true,
                        Err(e) => {
                            guard.ended_error = Some(e);
                            true
                        }
                    }
                };
                if should_wake {
                    wake();
                }
            }
            Err(e) => {
                stream_err = Some(format!("transcript follow read error: {e}"));
                break;
            }
        }
    }

    let status = child_slot
        .lock()
        .expect("transcript child mutex poisoned")
        .take()
        .and_then(|mut c| c.wait().ok());
    let stderr_text = stderr_handle.join().unwrap_or_default();

    let notify_end = {
        let mut guard = inner.lock().expect("transcript follower mutex poisoned");
        guard.following = false;
        if guard.stopped {
            false
        } else {
            if let Some(err) = stream_err {
                guard.ended_error = Some(err);
            } else if let Some(st) = status {
                if !st.success() {
                    guard.ended_error = Some(if stderr_text.is_empty() {
                        format!("`{}` exited with {st}", cli.describe(&arg_refs))
                    } else {
                        stderr_text
                    });
                }
            }
            true
        }
    };
    if notify_end {
        wake();
    }
}

/// Drained raw JSONL lines and terminal state from a [`LineFollower`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineBatch {
    /// Complete non-empty JSONL lines emitted since the previous [`LineFollower::take_batch`].
    pub lines: Vec<String>,
    /// `true` once the `agent transcript --follow` child has exited (reported once).
    pub ended: bool,
    /// Non-zero exit or spawn/read error when `ended` is `true`.
    pub error: Option<String>,
}

struct LineFollowerInner {
    lines: Vec<String>,
    following: bool,
    stopped: bool,
    ended_pending: bool,
    ended_error: Option<String>,
}

/// Raw JSONL line follower for `agent transcript <path> --follow [--from <byte>] -q`
/// (`agentEvents/follow` in `cli/agentevents.go`).
///
/// Unlike [`TranscriptFollower`] (which owns its own [`TranscriptState`]), [`LineFollower`]
/// hands raw `EVENTS.md` lines to callers whose model (such as `wall_core::agent_views::AgentViews`)
/// folds streams and stamps terminal scrollback anchors itself.
pub struct LineFollower {
    inner: Arc<Mutex<LineFollowerInner>>,
    child: Arc<Mutex<Option<Child>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl LineFollower {
    /// Spawn `agent transcript <path> --follow [--from <from_byte>] -q` on a background reader
    /// thread, invoking `wake` whenever new lines or process termination arrive.
    pub fn start<F>(cli: AgentCli, path: impl Into<String>, from_byte: Option<u64>, wake: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        let path = path.into();
        let inner = Arc::new(Mutex::new(LineFollowerInner {
            lines: Vec::new(),
            following: true,
            stopped: false,
            ended_pending: false,
            ended_error: None,
        }));
        let child_slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);

        let inner_bg = Arc::clone(&inner);
        let child_bg = Arc::clone(&child_slot);
        let handle = thread::Builder::new()
            .name("agent-feed-lines".to_string())
            .spawn(move || run_line_follow(cli, path, from_byte, inner_bg, child_bg, wake))
            .expect("spawning agent-feed-lines thread must succeed");

        Self {
            inner,
            child: child_slot,
            thread: Mutex::new(Some(handle)),
        }
    }

    /// Whether the underlying `agent transcript --follow` process is still running.
    pub fn is_following(&self) -> bool {
        self.inner
            .lock()
            .expect("line follower mutex poisoned")
            .following
    }

    /// Drain all queued lines and any newly latched exit/error transition in FIFO order.
    pub fn take_batch(&self) -> LineBatch {
        let mut guard = self.inner.lock().expect("line follower mutex poisoned");
        let lines = std::mem::take(&mut guard.lines);
        let ended = std::mem::replace(&mut guard.ended_pending, false);
        let error = if ended {
            guard.ended_error.clone()
        } else {
            None
        };
        LineBatch {
            lines,
            ended,
            error,
        }
    }

    /// Stop the `agent transcript --follow` child process and join its reader thread.
    pub fn stop(&self) {
        {
            let mut guard = self.inner.lock().expect("line follower mutex poisoned");
            guard.stopped = true;
            guard.following = false;
        }
        if let Some(child) = self
            .child
            .lock()
            .expect("line follower child mutex poisoned")
            .as_mut()
        {
            let _ = child.kill();
        }
        if let Some(handle) = self
            .thread
            .lock()
            .expect("line follower join mutex poisoned")
            .take()
        {
            let _ = handle.join();
        }
    }
}

impl Drop for LineFollower {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_line_follow(
    cli: AgentCli,
    path: String,
    from: Option<u64>,
    inner: Arc<Mutex<LineFollowerInner>>,
    child_slot: Arc<Mutex<Option<Child>>>,
    wake: Arc<dyn Fn() + Send + Sync>,
) {
    let mut owned_args = vec![
        "transcript".to_string(),
        path,
        "--follow".to_string(),
        "-q".to_string(),
    ];
    if let Some(off) = from {
        owned_args.push("--from".to_string());
        owned_args.push(off.to_string());
    }
    let arg_refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();

    let mut cmd = cli.command(&arg_refs);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let mut guard = inner.lock().expect("line follower mutex poisoned");
            guard.following = false;
            if !guard.stopped {
                guard.ended_pending = true;
                guard.ended_error = Some(format!(
                    "could not spawn `{}`: {e}",
                    cli.describe(&arg_refs)
                ));
                drop(guard);
                wake();
            }
            return;
        }
    };

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let stderr_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.take(4096).read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).trim().to_string()
    });

    {
        let stopped = inner.lock().expect("line follower mutex poisoned").stopped;
        let mut slot = child_slot.lock().expect("line follower child mutex poisoned");
        if stopped {
            let _ = child.kill();
        }
        *slot = Some(child);
    }

    let mut stream_err: Option<String> = None;
    for raw in BufReader::new(stdout).lines() {
        match raw {
            Ok(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let should_wake = {
                    let mut guard = inner.lock().expect("line follower mutex poisoned");
                    if guard.stopped {
                        break;
                    }
                    let was_empty = guard.lines.is_empty();
                    guard.lines.push(trimmed.to_string());
                    was_empty
                };
                if should_wake {
                    wake();
                }
            }
            Err(e) => {
                stream_err = Some(format!("transcript follow read error: {e}"));
                break;
            }
        }
    }

    let status = child_slot
        .lock()
        .expect("line follower child mutex poisoned")
        .take()
        .and_then(|mut c| c.wait().ok());
    let stderr_text = stderr_handle.join().unwrap_or_default();

    let notify_end = {
        let mut guard = inner.lock().expect("line follower mutex poisoned");
        guard.following = false;
        if guard.stopped {
            false
        } else {
            guard.ended_pending = true;
            if let Some(err) = stream_err {
                guard.ended_error = Some(err);
            } else if let Some(st) = status {
                if !st.success() {
                    guard.ended_error = Some(if stderr_text.is_empty() {
                        format!("`{}` exited with {st}", cli.describe(&arg_refs))
                    } else {
                        stderr_text
                    });
                }
            }
            true
        }
    };
    if notify_end {
        wake();
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::events::EventBody;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "agent-feed-tx-{label}-{}-{n}",
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
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if cond() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
    }

    /// Reference oracle from `cli/agentevents_test.go` (`bruteTailFrom`).
    fn brute_tail_from(data: &[u8], n: u64) -> Option<u64> {
        if n >= data.len() as u64 {
            return None;
        }
        let mut starts = Vec::new();
        let mut pos = 0usize;
        for (i, &b) in data.iter().enumerate() {
            if b == b'\n' {
                starts.push(pos as u64);
                pos = i + 1;
            }
        }
        if starts.is_empty() {
            return None;
        }
        let target = data.len() as u64 - n;
        let idx = starts
            .iter()
            .position(|&s| s >= target)
            .unwrap_or(starts.len() - 1);
        if idx == 0 {
            return None;
        }
        Some(starts[idx - 1])
    }

    #[test]
    fn tail_from_matches_explicit_cases_and_randomized_oracle() {
        let scratch = Scratch::new("tail");
        let path = scratch.0.join("t.jsonl");

        // Explicit edge cases from TestTailFrom in cli/agentevents_test.go:
        let cases: &[(&[u8], u64, Option<u64>)] = &[
            (b"", 10, None),
            (b"abc\n", 10, None),
            (b"abc", 2, None), // no complete line at all
            // offsets: 0="aaa\n"(4), 4="bbb\n"(4), 8="ccc\n"(4), len=12
            (b"aaa\nbbb\nccc\n", 4, Some(4)),  // target=8 -> window at 8 -> prev line at 4
            (b"aaa\nbbb\nccc\n", 5, Some(4)),  // target=7 -> first start >=7 is 8 -> prev at 4
            (b"aaa\nbbb\nccc\n", 8, Some(0)),  // target=4 -> window at 4 -> prev line at 0
            (b"aaa\nbbb\nccc\n", 9, None),     // target=3 -> first start >=3 is 4 -> prev at 0 -> wait: starts=[0,4,8], target=3 -> idx=1 (4), starts[0]=0!
            (b"aaa\nbbb\nccc\n", 12, None),    // n >= size -> None
            // Trailing partial line ignored as incomplete:
            // offsets: 0="aaa\n"(4), 4="bbb\n"(4), 8="partial" (no '\n'), len=15
            (b"aaa\nbbb\npartial", 3, Some(0)), // target=12, no complete line >=12 -> last complete is 4 -> prev is 0
            (b"aaa\nbbb\npartial", 11, Some(0)),
            (b"aaa\nbbb\npartial", 12, None),    // target=3 -> idx=1(4)? Wait: let's check against brute_tail_from!
        ];

        // Verify explicit cases against brute_tail_from and tail_from:
        for &(bytes, n, _) in cases {
            fs::write(&path, bytes).unwrap();
            let got = tail_from(&path, bytes.len() as u64, n).unwrap();
            let want = brute_tail_from(bytes, n);
            assert_eq!(
                got,
                want,
                "case len={} n={n}: got={got:?} want={want:?}",
                bytes.len()
            );
        }
        assert!(tail_from(&path, 10, 0).is_err(), "tail=0 must error");

        // 300 deterministic PRNG trials across multi-chunk sizes (> 128 KiB, crossing 64 KiB scan boundaries).
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next_u64 = || -> u64 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for trial in 0..300 {
            let line_count = (next_u64() % 40) as usize;
            let mut data = Vec::new();
            for _ in 0..line_count {
                // Occasionally generate lines > 64 KiB to cross NEWLINE_SCAN_CHUNK boundaries.
                let len = if next_u64() % 17 == 0 {
                    NEWLINE_SCAN_CHUNK + (next_u64() % 2048) as usize
                } else {
                    (next_u64() % 200) as usize
                };
                data.resize(data.len() + len, b'x');
                data.push(b'\n');
            }
            if next_u64() % 3 == 0 {
                let tail_partial = (next_u64() % 500) as usize;
                data.resize(data.len() + tail_partial, b'p');
            }
            fs::write(&path, &data).unwrap();
            let max_n = data.len() as u64 + 32;
            let n = (next_u64() % max_n).max(1);
            let got = tail_from(&path, data.len() as u64, n).unwrap();
            let want = brute_tail_from(&data, n);
            assert_eq!(got, want, "trial {trial} (len={}, n={n})", data.len());
        }
    }

    #[test]
    fn unwritten_transcript_of_live_session_returns_empty_while_ended_fails() {
        let scratch = Scratch::new("unwritten");
        let stub = scratch.0.join("agent");
        write_stub(&stub, "#!/bin/sh\necho 'should not run' >&2; exit 1\n");
        let cli = AgentCli::new(&stub);

        let missing = scratch.0.join("not-yet.jsonl");
        let mut live_sess = SessionInfo {
            session_id: "s-live".to_string(),
            harness: "claude".to_string(),
            path: missing.display().to_string(),
            status: "idle".to_string(),
            ..SessionInfo::default()
        };
        let win = read_transcript_window(&cli, &live_sess, &WindowParams::default()).unwrap();
        assert!(win.lines.is_empty() && win.size == 0);

        // Ended or bare session with missing file must return an error!
        live_sess.status = "ended".to_string();
        assert!(read_transcript_window(&cli, &live_sess, &WindowParams::default()).is_err());
        live_sess.status.clear();
        assert!(read_transcript_window(&cli, &live_sess, &WindowParams::default()).is_err());
    }

    /// Build a realistic stub `agent` shell script that understands:
    ///   `agent transcript <path> [--until U] [--from F] [--follow] [-q]`
    /// Where `<path>` stores raw `EVENTS.md` JSONL lines whose `"at"` equals each line's byte
    /// offset in `<path>` (matching real gomuxai transcripts!).
    fn install_transcript_stub(dir: &Path) -> (AgentCli, PathBuf, PathBuf) {
        let stub = dir.join("agent");
        let pid_file = dir.join("follow.pid");
        let args_log = dir.join("calls.log");

        // Uses /usr/bin/python3? NO — pure POSIX sh + awk/dd/tail so zero external interpreters needed!
        // Let's check: awk can track byte offset (length($0) + 1) cleanly in POSIX awk!
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{log}'\n\
             shift # 'transcript'\n\
             file=\"$1\"; shift\n\
             from=-1; until_b=-1; follow=0\n\
             while [ $# -gt 0 ]; do\n\
               case \"$1\" in\n\
                 --from) from=\"$2\"; shift 2 ;;\n\
                 --until) until_b=\"$2\"; shift 2 ;;\n\
                 --follow) follow=1; shift ;;\n\
                 -q) shift ;;\n\
                 *) shift ;;\n\
               esac\n\
             done\n\
             if [ \"$follow\" -eq 0 ]; then\n\
               dd if=\"$file\" bs=1 count=\"$until_b\" 2>/dev/null | awk -v f=\"$from\" '\n\
                 BEGIN {{ off = 0 }}\n\
                 {{ len = length($0) + 1; if ($0 ~ /\"type\":\"session\"/ || f < 0 || off > f) print $0; off += len }}\n\
               '\n\
               exit 0\n\
             fi\n\
             echo $$ > '{pid}'\n\
             # In --follow mode, emit lines after `from`, then tail -f new lines appended to file.\n\
             cur_lines=$(wc -l < \"$file\" | tr -d ' ')\n\
             awk -v f=\"$from\" '\n\
               BEGIN {{ off = 0 }}\n\
               {{ len = length($0) + 1; if ($0 ~ /\"type\":\"session\"/ || f < 0 || off > f) print $0; off += len }}\n\
             ' \"$file\"\n\
             next_line=$((cur_lines + 1))\n\
             exec tail -n +\"$next_line\" -f \"$file\"\n",
            log = args_log.display(),
            pid = pid_file.display(),
        );
        write_stub(&stub, &script);
        (AgentCli::new(&stub), pid_file, args_log)
    }

    fn append_event_line(path: &Path, make_json_with_at: impl FnOnce(u64) -> String) -> u64 {
        let at = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let mut line = make_json_with_at(at);
        line.push('\n');
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(line.as_bytes()).unwrap();
        f.flush().unwrap();
        at
    }

    #[test]
    fn transcript_follower_tails_prunes_provisional_folds_live_adopts_orphans_and_stops() {
        let scratch = Scratch::new("follow");
        let (cli, pid_file, args_log) = install_transcript_stub(&scratch.0);
        let tx_path = scratch.0.join("sess.jsonl");

        // 1. Session header
        append_event_line(&tx_path, |_| {
            r#"{"type":"session","session_id":"s1","harness":"claude","cwd":"/repo","status":"processing"}"#.to_string()
        });
        // 2. Old prompt + old pending tool `t-old` (will fall BEFORE our small initial tail window!)
        let p1_at = append_event_line(&tx_path, |at| {
            format!(
                r#"{{"type":"event","id":"p1","op":"add","seq":1,"at":{at},"line":1,"kind":"prompt","prompt":{{"text":"early prompt padding padding padding padding padding","origin":"human"}}}}"#
            )
        });
        let t_old_add_at = append_event_line(&tx_path, |at| {
            format!(
                r#"{{"type":"event","id":"t-old","op":"add","seq":2,"at":{at},"line":2,"kind":"tool","tool":{{"name":"Bash","action":"bash","detail":"cargo check padding padding padding"}}}}"#
            )
        });

        // Measure offset where recent tail window begins:
        let window_start = fs::metadata(&tx_path).unwrap().len();

        // 3. Inside tail window:
        //    - An `update` to `t-old` (orphan in initial window! Its `add` was at `t_old_add_at`!)
        //    - A recent prompt `p2`
        //    - An uncommitted provisional reply `say-prov` (must be pruned before `--follow`, and
        //      excluded from `last_at` so `--follow --from` resumes from `p2_at`!)
        let _t_old_upd_at = append_event_line(&tx_path, |at| {
            format!(
                r#"{{"type":"event","id":"t-old","op":"update","seq":2,"at":{at},"line":3,"kind":"tool","tool":{{"name":"Bash","action":"bash","detail":"cargo check padding padding padding","result":{{"status":"ok","output":"done"}}}}}}"#
            )
        });
        let p2_at = append_event_line(&tx_path, |at| {
            format!(
                r#"{{"type":"event","id":"p2","op":"add","seq":3,"at":{at},"line":4,"kind":"prompt","prompt":{{"text":"second prompt","origin":"human"}}}}"#
            )
        });
        let _prov_at = append_event_line(&tx_path, |at| {
            format!(
                r#"{{"type":"event","id":"say-prov","op":"add","seq":4,"at":{at},"line":5,"kind":"say","say":{{"text":"draft reply","final":true,"provisional":true}}}}"#
            )
        });

        let total_size = fs::metadata(&tx_path).unwrap().len();
        let tail_bytes = total_size - window_start;

        let session = SessionInfo {
            session_id: "s1".to_string(),
            harness: "claude".to_string(),
            path: tx_path.display().to_string(),
            status: "processing".to_string(),
            ..SessionInfo::default()
        };

        let wakes = Arc::new(AtomicUsize::new(0));
        let wakes_cb = Arc::clone(&wakes);
        let follower = TranscriptFollower::open(
            cli,
            session,
            TranscriptOptions {
                window_bytes: tail_bytes,
                follow_live: true,
            },
            move || {
                wakes_cb.fetch_add(1, Ordering::SeqCst);
            },
        )
        .unwrap();

        // Wait for `--follow` child to start and re-deliver/settle initial follow lines:
        wait_until("follow pid written", || pid_file.is_file());

        // Verify `--follow` was invoked with `--from <p2_at>` (NOT `_prov_at`!).
        let log_text = fs::read_to_string(&args_log).unwrap();
        assert!(
            log_text.contains(&format!("--follow -q --from {p2_at}")),
            "follow must resume from last non-provisional at ({p2_at}), saw:\n{log_text}"
        );

        // Now append a live committed reply + a live pending tool + its in-place update!
        let tool_live_at = append_event_line(&tx_path, |at| {
            format!(
                r#"{{"type":"event","id":"t-live","op":"add","seq":5,"at":{at},"line":6,"terminal_line":42,"kind":"tool","tool":{{"name":"Read","action":"read","target":"Cargo.toml"}}}}"#
            )
        });
        let _tool_live_upd_at = append_event_line(&tx_path, |at| {
            format!(
                r#"{{"type":"event","id":"t-live","op":"update","seq":5,"at":{at},"line":7,"kind":"tool","tool":{{"name":"Read","action":"read","target":"Cargo.toml","result":{{"status":"ok","output":"[workspace]"}}}}}}"#
            )
        });

        wait_until("live tool update folded", || {
            let snap = follower.snapshot();
            snap.entries.iter().any(|e| {
                e.event.id == "t-live"
                    && matches!(&e.event.body, EventBody::Tool(t) if t.status().as_str() == "ok")
            })
        });

        let snap_before_page = follower.snapshot();
        assert!(snap_before_page.has_earlier);
        assert_eq!(snap_before_page.oldest_at, p2_at);

        // Verify in-place update on `t-live` preserved its original `add` offset (`tool_live_at`).
        let t_live_entry = snap_before_page
            .entries
            .iter()
            .find(|e| e.event.id == "t-live")
            .unwrap();
        assert_eq!(t_live_entry.pos.at, tool_live_at);

        // Verify LiveAddNotice captured protocol `terminal_line: 42`.
        let live_adds = follower.take_live_adds();
        assert!(live_adds
            .iter()
            .any(|n| n.id == "t-live" && n.protocol_terminal_line == Some(42)));

        // Page older history with `load_earlier()` until `has_earlier` reaches false:
        // Page 1 uncovers `t-old` and adopts its newer `status: "ok"` update from `orphans`
        // while preserving `t_old_add_at`; subsequent page(s) uncover `p1` and then hit the
        // session header, flipping `has_earlier` to false (`WallAgentViews.js:947-950`).
        let mut total_prepended = 0usize;
        let mut pages = 0usize;
        while follower.snapshot().has_earlier {
            pages += 1;
            assert!(pages <= 5, "load_earlier must terminate");
            total_prepended += follower.load_earlier().unwrap();
        }
        assert_eq!(total_prepended, 2);
        assert_eq!(follower.load_earlier().unwrap(), 0, "no-op once has_earlier is false");

        let full_snap = follower.snapshot();
        assert!(!full_snap.has_earlier);
        assert_eq!(full_snap.oldest_at, 0);
        let ids: Vec<&str> = full_snap
            .entries
            .iter()
            .map(|e| e.event.id.as_str())
            .collect();
        assert_eq!(&ids[..3], &["p1", "t-old", "p2"]);
        assert_eq!(full_snap.entries[0].pos.at, p1_at);

        let t_old = &full_snap.entries[1];
        assert_eq!(
            t_old.pos.at, t_old_add_at,
            "adopted orphan keeps older add's pos.at"
        );
        match &t_old.event.body {
            EventBody::Tool(t) => assert_eq!(
                t.status().as_str(),
                "ok",
                "orphan update from tail window adopted onto older add"
            ),
            other => panic!("expected tool, got {other:?}"),
        }

        // Stop / Drop kills the `--follow` process cleanly.
        let pid: i32 = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        drop(follower);
        wait_until("transcript follow child killed on drop", || {
            !Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        });
    }

    #[test]
    fn transcript_follower_unexpected_exit_records_error_and_wakes() {
        let scratch = Scratch::new("follow-exit");
        let stub = scratch.0.join("agent");
        let tx_path = scratch.0.join("sess.jsonl");
        fs::write(
            &tx_path,
            "{\"type\":\"session\",\"session_id\":\"s-err\",\"harness\":\"claude\",\"status\":\"processing\"}\n",
        )
        .unwrap();

        write_stub(
            &stub,
            "#!/bin/sh\n\
             for arg in \"$@\"; do\n\
               if [ \"$arg\" = \"--follow\" ]; then\n\
                 echo 'corrupt transcript tail' >&2\n\
                 exit 2\n\
               fi\n\
             done\n\
             printf '{\"type\":\"session\",\"session_id\":\"s-err\",\"harness\":\"claude\",\"status\":\"processing\"}\\n'\n",
        );

        let wakes = Arc::new(AtomicUsize::new(0));
        let wakes_cb = Arc::clone(&wakes);
        let follower = TranscriptFollower::open(
            AgentCli::new(&stub),
            SessionInfo {
                session_id: "s-err".to_string(),
                harness: "claude".to_string(),
                path: tx_path.display().to_string(),
                status: "processing".to_string(),
                ..SessionInfo::default()
            },
            TranscriptOptions::default(),
            move || {
                wakes_cb.fetch_add(1, Ordering::SeqCst);
            },
        )
        .unwrap();

        wait_until("follow exit error recorded", || {
            let snap = follower.snapshot();
            !snap.following && snap.ended_error.is_some()
        });
        let snap = follower.snapshot();
        assert!(
            snap.ended_error
                .as_deref()
                .unwrap_or_default()
                .contains("corrupt transcript tail")
        );
        assert!(wakes.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn line_follower_streams_raw_lines_and_reports_clean_or_error_exit() {
        let scratch = Scratch::new("line-follow");
        let stub = scratch.0.join("agent");
        write_stub(
            &stub,
            "#!/bin/sh\n\
             printf '{\"type\":\"session\",\"session_id\":\"s-ln\",\"harness\":\"claude\",\"status\":\"processing\"}\\n'\n\
             printf '{\"type\":\"event\",\"op\":\"add\",\"id\":\"e1\",\"at\":10,\"kind\":\"prompt\",\"prompt\":{\"text\":\"hi\"}}\\n'\n",
        );

        let wakes = Arc::new(AtomicUsize::new(0));
        let wakes_cb = Arc::clone(&wakes);
        let follower = LineFollower::start(
            AgentCli::new(&stub),
            "/tmp/fake.jsonl",
            Some(4),
            move || {
                wakes_cb.fetch_add(1, Ordering::SeqCst);
            },
        );

        wait_until("line follower finishes", || !follower.is_following());
        let batch = follower.take_batch();
        assert_eq!(batch.lines.len(), 2);
        assert!(batch.ended);
        assert!(batch.error.is_none());
        assert!(wakes.load(Ordering::SeqCst) >= 1);

        // Subsequent drain returns empty batch with ended == false.
        let empty = follower.take_batch();
        assert!(empty.lines.is_empty() && !empty.ended);
    }
}
