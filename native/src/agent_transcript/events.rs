//! gomuxai's agent event stream, read into an [`AgentSession`].
//!
//! `agent transcript <session>` (gomuxai, the one reader of agent
//! transcripts) prints one session as JSON lines: a `session` line, then an
//! `event` line per prompt, thinking block, reply, tool call and harness
//! record, in transcript order, never grouped. The contract is gomuxai's
//! `docs/EVENTS.md`; its conformance corpus is vendored under
//! `tools/vendor/gomuxai/` and read by this module's tests. Nothing here reads
//! a harness's transcript.
//!
//! Folding the stream: an event that changes later (a tool's result lands, a
//! provisional reply is confirmed) comes again, whole, with the same `id` and
//! `op: "update"`. [`AgentSession::apply_line`] replaces it IN PLACE, so an
//! event keeps the position its add gave it. A later `session` line replaces
//! the session's metadata. Every line goes through `apply_line`, one at a
//! time, which is also the shape a live feed takes: `agent transcript
//! --follow` prints the same lines as the transcript grows, and each one can
//! be applied as it arrives ([`AgentSession::last_at`] is where such a feed
//! resumes).
//!
//! A line this reader cannot read is an error naming the line, never a
//! skipped line: a stream that drifted from the contract must not render as
//! a quietly wrong session.

use std::collections::HashMap;
use std::io::BufRead;

use serde::Deserialize;
use serde_json::Value;

/// A harness gomuxai reads, spelled as gomuxai spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HarnessKind {
    Claude,
    Agy,
    Kimi,
}

impl HarnessKind {
    pub const ALL: [HarnessKind; 3] = [HarnessKind::Claude, HarnessKind::Agy, HarnessKind::Kimi];

    /// gomuxai's name for the harness (a Session's `harness`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Agy => "agy",
            Self::Kimi => "kimi",
        }
    }

    /// The app's own name, for a person.
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Agy => "Antigravity",
            Self::Kimi => "Kimi Code",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|h| h.name() == name)
    }
}

/// A Session as EVENTS.md describes it: the stream's `session` line, and
/// each entry of `agent sessions --json`. An absent string is empty, as
/// gomuxai omits empty fields.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    /// `claude`, `agy` or `kimi`; a workgroup agent with no transcript is
    /// listed under its backend's name (and an empty `path`).
    pub harness: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub size: u64,
    /// RFC 3339; absent from a stream's session lines.
    #[serde(default)]
    pub mtime: String,
    #[serde(default)]
    pub cwd: String,
    /// The last element of `cwd`, never the storage folder's name.
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub agent_name: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub git_branch: String,
    #[serde(default)]
    pub workgroup: Option<Workgroup>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub status_at: String,
    /// Set on a subagent's session.
    #[serde(default)]
    pub parent_session: Option<ParentSession>,
    #[serde(default)]
    pub continued_in: String,
}

impl SessionInfo {
    pub fn harness_kind(&self) -> Option<HarnessKind> {
        HarnessKind::from_name(&self.harness)
    }
}

/// The gomuxai workgroup agent that owns a session.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Workgroup {
    pub name: String,
    #[serde(default)]
    pub parent: String,
    #[serde(default)]
    pub store: String,
    #[serde(default)]
    pub host: String,
}

/// The session (and the tool call, where the harness records it) that
/// started a subagent.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ParentSession {
    pub session_id: String,
    #[serde(default)]
    pub tool_id: String,
}

/// The prompt an event belongs to: the transcript's own turn where it
/// records one, derived otherwise (`derived` says which). `index` counts
/// human prompts, so two turns can share one (a task notification opens a
/// turn of its own with the index of the prompt before it); 0 means the
/// transcript doesn't say.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct TurnRef {
    pub id: String,
    #[serde(default)]
    pub index: u32,
    #[serde(default)]
    pub derived: bool,
}

/// Where a message into the agent came from (EVENTS.md, `prompt`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PromptOrigin {
    /// Typed by a person.
    Human,
    /// Sent while the agent was busy (mid-turn).
    Queued,
    /// gomuxai's held-message delivery.
    Delivered,
    /// A background task reporting back.
    TaskNotification,
    /// Another agent (a subagent's task is its parent's words).
    Peer,
    /// A slash command.
    Command,
    /// Injected by the harness (a wakeup, an idle notice, hook feedback).
    System,
}

impl PromptOrigin {
    /// Whose words, for a card.
    pub fn label(self) -> &'static str {
        match self {
            Self::Human => "You",
            Self::Queued => "You (queued)",
            Self::Delivered => "Delivered",
            Self::TaskNotification => "Notification",
            Self::Peer => "Peer",
            Self::Command => "Command",
            Self::System => "Harness",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PromptEvent {
    pub text: String,
    pub origin: PromptOrigin,
    /// A slash command's `/name`; `text` is then `/name args`.
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub images: Vec<PromptImage>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PromptImage {
    pub media_type: String,
    /// Base64, whole.
    pub data: String,
}

/// A thinking block; a redacted one (signature only) has no text.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ThinkEvent {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub redacted: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SayEvent {
    pub text: String,
    /// A turn's closing reply.
    #[serde(default, rename = "final")]
    pub is_final: bool,
    /// Taken from the Stop hook before the transcript holds it.
    #[serde(default)]
    pub provisional: bool,
}

/// A tool call's result status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolStatus {
    Pending,
    Ok,
    Error,
    Interrupted,
    Background,
}

impl ToolStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Interrupted => "interrupted",
            Self::Background => "running in background",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ToolResult {
    pub status: ToolStatus,
    #[serde(default)]
    pub output: String,
    /// The harness's structured response (Claude Code's `toolUseResult`), whole.
    #[serde(default)]
    pub structured: Option<Value>,
}

/// Part or all of a file's text: its lines from `start` (1-based), of the
/// file's `total` lines when the result says.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct FileText {
    pub text: String,
    #[serde(default = "first_line")]
    pub start: usize,
    #[serde(default)]
    pub total: Option<usize>,
}

fn first_line() -> usize {
    1
}

impl FileText {
    /// The whole file: from its first line to its last. A read of part of
    /// a file, or one whose result doesn't say how long the file is, is not.
    pub fn is_whole(&self) -> bool {
        self.start == 1 && self.total.is_some_and(|t| t == self.text.lines().count())
    }
}

/// A tool call (EVENTS.md, `tool`). `action`, `target`, `detail` and `meta`
/// come from gomuxai's one tool table (`transcript.SummarizeTool`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ToolEvent {
    pub name: String,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub result: Option<ToolResult>,
    /// read, edit, write, bash, grep, glob, task, ask, fetch, search,
    /// skill, send, `noise`, or an unknown tool's own name, lowercased.
    pub action: String,
    /// The file the call is about, relative to the session's cwd when
    /// under it.
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub meta: Option<Value>,
    /// The target's text as the call saw it (a read) or left it (a write;
    /// an edit: the whole file after it), where the result holds it.
    #[serde(default)]
    pub file: Option<FileText>,
    /// The sessions a subagent-spawning call started.
    #[serde(default)]
    pub sub_sessions: Vec<String>,
}

/// The tool table's action for bookkeeping tools (TodoWrite, ToolSearch, ...).
pub const ACTION_NOISE: &str = "noise";

impl ToolEvent {
    pub fn status(&self) -> ToolStatus {
        self.result.as_ref().map_or(ToolStatus::Pending, |r| r.status)
    }

    pub fn is_noise(&self) -> bool {
        self.action == ACTION_NOISE
    }

    /// A number from `meta` (an edit's `added`, a read's `lines`, ...).
    pub fn meta_count(&self, key: &str) -> Option<usize> {
        self.meta.as_ref()?.get(key)?.as_u64().map(|n| n as usize)
    }
}

/// A harness record about the session (turn timing, compaction, API errors,
/// interrupts, model fallbacks).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SystemEvent {
    pub subtype: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub data: Option<Value>,
}

/// What an event is: exactly one of EVENTS.md's kinds. (`request` exists
/// only under `--requests`, which this reader never asks for.)
#[derive(Debug, Clone, PartialEq)]
pub enum EventBody {
    Prompt(PromptEvent),
    Think(ThinkEvent),
    Say(SayEvent),
    /// Boxed: a tool event is several times any other's size.
    Tool(Box<ToolEvent>),
    System(SystemEvent),
}

/// One thing that happened in a session, as it now stands (its latest
/// update applied).
#[derive(Debug, Clone, PartialEq)]
pub struct AgentEvent {
    /// Stable: a tool's tool_use id, else `<record>#<block>`.
    pub id: String,
    pub seq: u64,
    /// Byte offset and line index of the source line of the event's
    /// latest add or update.
    pub at: u64,
    pub line: u64,
    pub ts: String,
    /// None before the first prompt a parse has seen.
    pub turn: Option<TurnRef>,
    /// The subagent id, on a subagent's own events.
    pub agent: String,
    pub body: EventBody,
}

/// An `event` line as written.
#[derive(Deserialize)]
struct RawEvent {
    id: String,
    op: String,
    #[serde(default)]
    seq: u64,
    at: u64,
    #[serde(default)]
    line: u64,
    #[serde(default)]
    ts: String,
    #[serde(default)]
    turn: Option<TurnRef>,
    #[serde(default)]
    agent: String,
    kind: String,
    prompt: Option<PromptEvent>,
    think: Option<ThinkEvent>,
    say: Option<SayEvent>,
    tool: Option<ToolEvent>,
    system: Option<SystemEvent>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum StreamLine {
    // Both boxed: a line is read once and taken apart at once.
    Session(Box<SessionInfo>),
    Event(Box<RawEvent>),
}

/// Whether an event line added an event or replaced one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Session,
    /// A new event, at this position.
    Added(usize),
    /// The event at this position, replaced in place.
    Updated(usize),
}

/// One agent session: what gomuxai says it is, and its events in the order
/// their adds arrived, each as its latest update left it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentSession {
    pub info: SessionInfo,
    pub events: Vec<AgentEvent>,
    /// id -> position in `events`.
    by_id: HashMap<String, usize>,
    /// Turn ids (None: before any prompt) in the order the stream first
    /// named them; an event's turn ordinal is its position here.
    turns: Vec<Option<String>>,
    /// The `at` of the newest line read that isn't a provisional reply.
    last_at: Option<u64>,
}

impl AgentSession {
    /// Read a whole stream: a `session` line first, then its events. An
    /// empty stream, or one that doesn't open with its session, is an error.
    pub fn read_stream(reader: impl BufRead) -> Result<Self, String> {
        let mut session: Option<Self> = None;
        for (n, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| format!("event stream line {}: {e}", n + 1))?;
            if line.trim().is_empty() {
                continue;
            }
            match session.as_mut() {
                Some(s) => {
                    s.apply_line(&line).map_err(|e| format!("event stream line {}: {e}", n + 1))?;
                }
                None => {
                    let mut s = Self::default();
                    match s.apply_line(&line).map_err(|e| format!("event stream line {}: {e}", n + 1))? {
                        Applied::Session => session = Some(s),
                        _ => return Err(format!("event stream line {}: an event before the session line", n + 1)),
                    }
                }
            }
        }
        session.ok_or_else(|| "the event stream is empty: no session line".to_string())
    }

    /// [`Self::read_stream`] over text.
    pub fn from_stream_str(text: &str) -> Result<Self, String> {
        Self::read_stream(text.as_bytes())
    }

    /// Fold one stream line in: a session line replaces the metadata, an
    /// add appends, an update replaces its event in place. An update of an
    /// id this session never saw added (a feed resumed with `--from` past
    /// the add) is the event's current state either way, and is appended.
    pub fn apply_line(&mut self, line: &str) -> Result<Applied, String> {
        let parsed: StreamLine = serde_json::from_str(line).map_err(|e| format!("not a stream line ({e})"))?;
        let raw = match parsed {
            StreamLine::Session(info) => {
                self.info = *info;
                return Ok(Applied::Session);
            }
            StreamLine::Event(raw) => *raw,
        };
        let id = raw.id.clone();
        let body = match raw.kind.as_str() {
            "prompt" => raw.prompt.map(EventBody::Prompt),
            "think" => raw.think.map(EventBody::Think),
            "say" => raw.say.map(EventBody::Say),
            "tool" => raw.tool.map(|t| EventBody::Tool(Box::new(t))),
            "system" => raw.system.map(EventBody::System),
            other => return Err(format!("event {id} has kind {other:?}, which this reader does not know")),
        }
        .ok_or_else(|| format!("event {id} ({}) has no {} body", raw.kind, raw.kind))?;
        let provisional = matches!(&body, EventBody::Say(s) if s.provisional);
        let event = AgentEvent {
            id: raw.id,
            seq: raw.seq,
            at: raw.at,
            line: raw.line,
            ts: raw.ts,
            turn: raw.turn,
            agent: raw.agent,
            body,
        };
        if !provisional {
            self.last_at = Some(self.last_at.map_or(event.at, |a| a.max(event.at)));
        }
        let turn_id = event.turn.as_ref().map(|t| t.id.clone());
        if !self.turns.contains(&turn_id) {
            self.turns.push(turn_id);
        }
        match (raw.op.as_str(), self.by_id.get(&id).copied()) {
            ("update", Some(i)) => {
                self.events[i] = event;
                Ok(Applied::Updated(i))
            }
            ("add" | "update", None) => {
                self.by_id.insert(id, self.events.len());
                self.events.push(event);
                Ok(Applied::Added(self.events.len() - 1))
            }
            ("add", Some(_)) => Err(format!("event {id} is added twice")),
            (op, _) => Err(format!("event {id} has op {op:?}")),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.info.session_id
    }

    pub fn harness(&self) -> Option<HarnessKind> {
        self.info.harness_kind()
    }

    /// The working directory the session recorded first, if any.
    pub fn cwd(&self) -> Option<&str> {
        Some(self.info.cwd.as_str()).filter(|c| !c.is_empty())
    }

    /// How many turns the stream named (events before any prompt are one).
    pub fn turn_count(&self) -> usize {
        self.turns.len()
    }

    /// An event's turn ordinal: the position of its turn among the
    /// session's turns, in the order the stream first named them.
    pub fn turn_ordinal(&self, event: &AgentEvent) -> usize {
        let id = event.turn.as_ref().map(|t| &t.id);
        self.turns
            .iter()
            .position(|t| t.as_ref() == id)
            .expect("every applied event's turn is recorded")
    }

    /// The text of the first prompt in turn `ordinal`.
    pub fn turn_prompt(&self, ordinal: usize) -> Option<&str> {
        let id = self.turns.get(ordinal)?.as_ref();
        self.events.iter().find_map(|e| match &e.body {
            EventBody::Prompt(p) if e.turn.as_ref().map(|t| &t.id) == id => Some(p.text.as_str()),
            _ => None,
        })
    }

    /// Where a live feed resumes (`agent transcript --follow --from`): the
    /// `at` of the newest line read that isn't a provisional reply.
    pub fn last_at(&self) -> Option<u64> {
        self.last_at
    }
}
