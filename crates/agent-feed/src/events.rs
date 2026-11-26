//! gomuxai's `EVENTS.md` stream shapes and normalized [`EventRecord`] projection.
//!
//! Reuses the event model in `native/src/agent_transcript/events.rs` (without depending on the
//! `native` crate) and extends it with the fields `wall/WallAgentViews.js` and
//! `wall_core::agent_view` inspect: `SessionInfo::end_reason`, `AgentEvent::abandoned`, optional
//! protocol `terminal_line` anchors, `RequestEvent` (for `--requests` streams), and
//! [`AgentEvent::to_event_record`] producing the exact JSON/struct contract of
//! `wall_core::agent_view::EventRecord`.

use std::collections::HashMap;
use std::io::BufRead;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A harness gomuxai reads, spelled as gomuxai spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HarnessKind {
    Claude,
    Agy,
    Kimi,
}

impl HarnessKind {
    pub const ALL: [HarnessKind; 3] = [HarnessKind::Claude, HarnessKind::Agy, HarnessKind::Kimi];

    /// gomuxai's name for the harness (`SessionInfo::harness`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Agy => "agy",
            Self::Kimi => "kimi",
        }
    }

    /// Display name for a person.
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

/// A `session` record from `EVENTS.md` (`type: "session"` stream header or an entry in
/// `agent sessions --json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    #[serde(default)]
    pub harness: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mtime: String,
    #[serde(default)]
    pub cwd: String,
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
    pub end_reason: String,
    #[serde(default)]
    pub status_at: String,
    #[serde(default)]
    pub parent_session: Option<ParentSession>,
    #[serde(default)]
    pub continued_in: String,
}

impl SessionInfo {
    pub fn harness_kind(&self) -> Option<HarnessKind> {
        HarnessKind::from_name(&self.harness)
    }

    /// Whether gomuxai knows this session is currently live (`status` non-empty and not `"ended"`,
    /// matching `cli/agentevents.go`'s `agentSession.runs()`).
    pub fn runs(&self) -> bool {
        !self.status.is_empty() && self.status != "ended"
    }
}

/// The gomuxai workgroup agent that owns a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workgroup {
    pub name: String,
    #[serde(default)]
    pub parent: String,
    #[serde(default)]
    pub store: String,
    #[serde(default)]
    pub host: String,
}

/// The parent session (and spawning tool call, where recorded) of a subagent session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentSession {
    pub session_id: String,
    #[serde(default)]
    pub tool_id: String,
}

/// Turn reference carried on an [`AgentEvent`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRef {
    pub id: String,
    #[serde(default)]
    pub index: u32,
    #[serde(default)]
    pub derived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_line: Option<u64>,
}

/// Origin of a `prompt` event (`EVENTS.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PromptOrigin {
    Human,
    Queued,
    Delivered,
    TaskNotification,
    Peer,
    Command,
    System,
}

impl PromptOrigin {
    /// Wire string in `EVENTS.md`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Queued => "queued",
            Self::Delivered => "delivered",
            Self::TaskNotification => "task-notification",
            Self::Peer => "peer",
            Self::Command => "command",
            Self::System => "system",
        }
    }

    /// Card label (`native/src/agent_transcript/events.rs`).
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

    /// Compact row tag (`collections/agentEvents.js` / `wall_core::agent_view`).
    pub fn row_tag(self) -> &'static str {
        match self {
            Self::Human => "you",
            Self::Queued => "you · queued",
            Self::Delivered => "delivered",
            Self::TaskNotification => "notification",
            Self::Peer => "peer",
            Self::Command => "command",
            Self::System => "harness",
        }
    }
}

/// Body of a `prompt` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptEvent {
    #[serde(default)]
    pub text: String,
    pub origin: PromptOrigin,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub images: Vec<PromptImage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_line: Option<u64>,
}

/// Inline base64 image attached to a [`PromptEvent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptImage {
    pub media_type: String,
    pub data: String,
}

/// Body of a `think` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkEvent {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub redacted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_line: Option<u64>,
}

/// Body of a `say` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SayEvent {
    #[serde(default)]
    pub text: String,
    #[serde(default, rename = "final")]
    pub is_final: bool,
    #[serde(default)]
    pub provisional: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_line: Option<u64>,
}

/// Result status of a tool call (`EVENTS.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolStatus {
    Pending,
    Ok,
    Error,
    Interrupted,
    Background,
}

impl ToolStatus {
    /// Wire status token (`"pending" | "ok" | "error" | "interrupted" | "background"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Interrupted => "interrupted",
            Self::Background => "background",
        }
    }

    /// Human label (`native/src/agent_transcript/events.rs`).
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

/// Completed or in-progress result of a [`ToolEvent`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub status: ToolStatus,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub structured: Option<Value>,
}

/// File text snapshot carried on `read`, `write`, or `edit` tool results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    pub fn is_whole(&self) -> bool {
        self.start == 1 && self.total.is_some_and(|t| t == self.text.lines().count())
    }
}

/// Body of a `tool` event (`EVENTS.md`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolEvent {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub result: Option<ToolResult>,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub meta: Option<Value>,
    #[serde(default)]
    pub file: Option<FileText>,
    #[serde(default)]
    pub sub_sessions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_line: Option<u64>,
}

/// gomuxai's `action` tag for bookkeeping tool calls (`TodoWrite`, `ToolSearch`, ...).
pub const ACTION_NOISE: &str = "noise";

impl ToolEvent {
    pub fn status(&self) -> ToolStatus {
        self.result.as_ref().map_or(ToolStatus::Pending, |r| r.status)
    }

    pub fn is_noise(&self) -> bool {
        self.action == ACTION_NOISE
    }

    pub fn meta_count(&self, key: &str) -> Option<usize> {
        self.meta.as_ref()?.get(key)?.as_u64().map(|n| n as usize)
    }
}

/// Body of a `system` event (`EVENTS.md`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemEvent {
    pub subtype: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_line: Option<u64>,
}

/// Body of a `request` event (`agent transcript --requests`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestEvent {
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_line: Option<u64>,
}

/// Variant payload of an [`AgentEvent`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventBody {
    Prompt(PromptEvent),
    Think(ThinkEvent),
    Say(SayEvent),
    Tool(Box<ToolEvent>),
    System(SystemEvent),
    Request(RequestEvent),
}

impl EventBody {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Prompt(_) => "prompt",
            Self::Think(_) => "think",
            Self::Say(_) => "say",
            Self::Tool(_) => "tool",
            Self::System(_) => "system",
            Self::Request(_) => "request",
        }
    }

    fn body_terminal_line(&self) -> Option<u64> {
        match self {
            Self::Prompt(p) => p.terminal_line,
            Self::Think(t) => t.terminal_line,
            Self::Say(s) => s.terminal_line,
            Self::Tool(t) => t.terminal_line,
            Self::System(s) => s.terminal_line,
            Self::Request(r) => r.terminal_line,
        }
    }
}

/// Stream operation on an `event` frame (`"add"` or `"update"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventOp {
    Add,
    Update,
}

/// Stable byte/line/sequence position captured when an event is first added (`WallAgentViews.js`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventPos {
    pub at: u64,
    pub line: u64,
    pub seq: u64,
}

/// One event from `agent transcript`, with its typed [`EventBody`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentEvent {
    pub id: String,
    pub seq: u64,
    pub at: u64,
    pub line: u64,
    pub ts: String,
    pub turn: Option<TurnRef>,
    pub agent: String,
    pub abandoned: bool,
    pub terminal_line: Option<u64>,
    pub body: EventBody,
}

impl AgentEvent {
    /// Whether this event is an uncommitted Stop-hook reply (`say` with `provisional: true`).
    pub fn is_provisional_reply(&self) -> bool {
        matches!(&self.body, EventBody::Say(s) if s.provisional)
    }

    /// Initial [`EventPos`] recorded from this event frame.
    pub fn pos(&self) -> EventPos {
        EventPos {
            at: self.at,
            line: self.line,
            seq: self.seq,
        }
    }

    /// Project this [`AgentEvent`] into the normalized [`EventRecord`] shape consumed by
    /// `wall_core::agent_view` (`event_category`, `row_summary`, `detail_lines`, etc.).
    ///
    /// When `at_override` is `Some(at)` (for instance, the preserved `add` byte offset after an
    /// in-place `update`), `rec.at` uses that offset, matching `WallAgentViews.js:1032`.
    /// When `show_noise` is `false`, `tool` events with `action == "noise"` return `None`.
    pub fn to_event_record(&self, at_override: Option<i64>, show_noise: bool) -> Option<EventRecord> {
        let mut rec = EventRecord {
            id: self.id.clone(),
            event_kind: self.body.kind_name().to_string(),
            at: at_override.unwrap_or(self.at as i64),
            ts: (!self.ts.is_empty()).then(|| self.ts.clone()),
            turn: self.turn.as_ref().map(|t| TurnInfo {
                id: t.id.clone(),
                index: i64::from(t.index),
                derived: t.derived,
            }),
            agent: (!self.agent.is_empty()).then(|| self.agent.clone()),
            abandoned: self.abandoned,
            ..EventRecord::default()
        };

        match &self.body {
            EventBody::Prompt(p) => {
                rec.action = "prompt".to_string();
                rec.tag = Some(p.origin.row_tag().to_string());
                rec.origin = Some(p.origin.as_str().to_string());
                rec.command = (!p.command.is_empty()).then(|| p.command.clone());
                rec.images = p.images.len();
                rec.result = p.text.clone();
            }
            EventBody::Think(t) => {
                rec.action = "think".to_string();
                rec.redacted = t.redacted;
                rec.detail = if t.redacted {
                    "redacted".to_string()
                } else {
                    String::new()
                };
                rec.result = t.text.clone();
            }
            EventBody::Say(s) => {
                rec.action = "say".to_string();
                rec.final_reply = s.is_final;
                rec.provisional = s.provisional;
                rec.result = s.text.clone();
            }
            EventBody::Tool(t) => {
                if t.action == ACTION_NOISE && !show_noise {
                    return None;
                }
                let action = if !t.action.is_empty() {
                    t.action.to_lowercase()
                } else if !t.name.is_empty() {
                    t.name.to_lowercase()
                } else {
                    "tool".to_string()
                };
                let status = t.status().as_str();
                let out = t
                    .result
                    .as_ref()
                    .map(|r| r.output.clone())
                    .unwrap_or_default();

                rec.action = action;
                rec.name = (!t.name.is_empty()).then(|| t.name.clone());
                if t.action == ACTION_NOISE && !t.name.is_empty() {
                    rec.tag = Some(t.name.clone());
                }
                rec.target = t.target.clone();
                rec.detail = t.detail.clone();
                rec.result = if !t.target.is_empty() && status != "error" {
                    String::new()
                } else {
                    out
                };
                rec.meta = t.meta.as_ref().filter(|m| m.is_object()).cloned();
                rec.file = t.file.as_ref().map(|f| FileSnapshot {
                    text: f.text.clone(),
                    start: f.start.max(1),
                    total: f.total.filter(|&tot| tot > 0),
                });
                rec.status = Some(status.to_string());
                rec.sub_sessions = t.sub_sessions.clone();
            }
            EventBody::System(s) => {
                rec.action = "system".to_string();
                rec.detail = s.subtype.clone();
                rec.subtype = (!s.subtype.is_empty()).then(|| s.subtype.clone());
                rec.result = if !s.text.is_empty() {
                    s.text.clone()
                } else if let Some(data) = s.data.as_ref().filter(|v| !v.is_null()) {
                    serde_json::to_string_pretty(data).unwrap_or_default()
                } else {
                    String::new()
                };
            }
            EventBody::Request(r) => {
                rec.action = "request".to_string();
                rec.detail = r.model.clone();
            }
        }

        Some(rec)
    }
}

/// Turn metadata attached to an [`EventRecord`] (identical wire/field contract to
/// `wall_core::agent_view::TurnInfo`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnInfo {
    pub id: String,
    pub index: i64,
    #[serde(default)]
    pub derived: bool,
}

/// File content snapshot carried on a tool [`EventRecord`] (identical wire/field contract to
/// `wall_core::agent_view::FileSnapshot`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSnapshot {
    pub text: String,
    pub start: usize,
    pub total: Option<usize>,
}

/// Normalized event record matching `wall_core::agent_view::EventRecord` (`collections/agentEvents.js`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventRecord {
    pub id: String,
    pub event_kind: String,
    pub at: i64,
    pub ts: Option<String>,
    pub turn: Option<TurnInfo>,
    pub agent: Option<String>,
    #[serde(default)]
    pub abandoned: bool,
    pub action: String,
    pub tag: Option<String>,
    pub name: Option<String>,
    pub origin: Option<String>,
    pub command: Option<String>,
    #[serde(default)]
    pub images: usize,
    #[serde(default)]
    pub redacted: bool,
    #[serde(default)]
    pub final_reply: bool,
    #[serde(default)]
    pub provisional: bool,
    pub subtype: Option<String>,
    pub target: String,
    pub detail: String,
    pub result: String,
    pub meta: Option<Value>,
    pub file: Option<FileSnapshot>,
    pub status: Option<String>,
    #[serde(default)]
    pub sub_sessions: Vec<String>,
}

#[derive(Deserialize)]
struct RawEvent {
    id: String,
    op: String,
    #[serde(default)]
    seq: u64,
    #[serde(default)]
    at: u64,
    #[serde(default)]
    line: u64,
    #[serde(default)]
    ts: String,
    #[serde(default)]
    turn: Option<TurnRef>,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    abandoned: bool,
    #[serde(default)]
    terminal_line: Option<u64>,
    kind: String,
    prompt: Option<PromptEvent>,
    think: Option<ThinkEvent>,
    say: Option<SayEvent>,
    tool: Option<ToolEvent>,
    system: Option<SystemEvent>,
    request: Option<RequestEvent>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum RawStreamLine {
    Session(Box<SessionInfo>),
    Event(Box<RawEvent>),
}

/// Parsed JSON line from `agent transcript`.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamFrame {
    Session(Box<SessionInfo>),
    Event {
        op: EventOp,
        event: Box<AgentEvent>,
    },
}

/// Parse a single non-empty JSON line from `agent transcript`.
pub fn parse_stream_line(line: &str) -> Result<StreamFrame, String> {
    let parsed: RawStreamLine =
        serde_json::from_str(line).map_err(|e| format!("not a stream line ({e})"))?;
    let raw = match parsed {
        RawStreamLine::Session(info) => return Ok(StreamFrame::Session(info)),
        RawStreamLine::Event(raw) => *raw,
    };
    let id = raw.id.clone();
    let op = match raw.op.as_str() {
        "add" => EventOp::Add,
        "update" => EventOp::Update,
        other => return Err(format!("event {id} has op {other:?}")),
    };
    let body = match raw.kind.as_str() {
        "prompt" => raw.prompt.map(EventBody::Prompt),
        "think" => raw.think.map(EventBody::Think),
        "say" => raw.say.map(EventBody::Say),
        "tool" => raw.tool.map(|t| EventBody::Tool(Box::new(t))),
        "system" => raw.system.map(EventBody::System),
        "request" => raw.request.map(EventBody::Request),
        other => {
            return Err(format!(
                "event {id} has kind {other:?}, which this reader does not know"
            ));
        }
    }
    .ok_or_else(|| format!("event {id} ({}) has no {} body", raw.kind, raw.kind))?;

    let terminal_line = raw
        .terminal_line
        .or_else(|| raw.turn.as_ref().and_then(|t| t.terminal_line))
        .or_else(|| body.body_terminal_line());

    Ok(StreamFrame::Event {
        op,
        event: Box::new(AgentEvent {
            id: raw.id,
            seq: raw.seq,
            at: raw.at,
            line: raw.line,
            ts: raw.ts,
            turn: raw.turn,
            agent: raw.agent,
            abandoned: raw.abandoned,
            terminal_line,
            body,
        }),
    })
}

/// Outcome of folding a line into [`AgentSession`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Session,
    Added(usize),
    Updated(usize),
}

/// Whole-stream folded session (`native/src/agent_transcript/events.rs`).
///
/// For windowed tailing, `load_earlier` paging, and live `--follow` with orphan adoption, use
/// [`crate::transcript::TranscriptState`] and [`crate::transcript::TranscriptFollower`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentSession {
    pub info: SessionInfo,
    pub events: Vec<AgentEvent>,
    by_id: HashMap<String, usize>,
    turns: Vec<Option<String>>,
    last_at: Option<u64>,
}

impl AgentSession {
    pub fn read_stream(reader: impl BufRead) -> Result<Self, String> {
        let mut session: Option<Self> = None;
        for (n, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| format!("event stream line {}: {e}", n + 1))?;
            if line.trim().is_empty() {
                continue;
            }
            match session.as_mut() {
                Some(s) => {
                    s.apply_line(&line)
                        .map_err(|e| format!("event stream line {}: {e}", n + 1))?;
                }
                None => {
                    let mut s = Self::default();
                    match s
                        .apply_line(&line)
                        .map_err(|e| format!("event stream line {}: {e}", n + 1))?
                    {
                        Applied::Session => session = Some(s),
                        _ => {
                            return Err(format!(
                                "event stream line {}: an event before the session line",
                                n + 1
                            ));
                        }
                    }
                }
            }
        }
        session.ok_or_else(|| "the event stream is empty: no session line".to_string())
    }

    pub fn from_stream_str(text: &str) -> Result<Self, String> {
        Self::read_stream(text.as_bytes())
    }

    pub fn apply_line(&mut self, line: &str) -> Result<Applied, String> {
        match parse_stream_line(line)? {
            StreamFrame::Session(info) => {
                self.info = *info;
                Ok(Applied::Session)
            }
            StreamFrame::Event { op, event } => {
                let event = *event;
                if !event.is_provisional_reply() {
                    self.last_at = Some(self.last_at.map_or(event.at, |a| a.max(event.at)));
                }
                let turn_id = event.turn.as_ref().map(|t| t.id.clone());
                if !self.turns.contains(&turn_id) {
                    self.turns.push(turn_id);
                }
                let id = event.id.clone();
                match (op, self.by_id.get(&id).copied()) {
                    (EventOp::Update, Some(i)) => {
                        self.events[i] = event;
                        Ok(Applied::Updated(i))
                    }
                    (EventOp::Add | EventOp::Update, None) => {
                        self.by_id.insert(id, self.events.len());
                        self.events.push(event);
                        Ok(Applied::Added(self.events.len() - 1))
                    }
                    (EventOp::Add, Some(_)) => Err(format!("event {id} is added twice")),
                }
            }
        }
    }

    pub fn last_at(&self) -> Option<u64> {
        self.last_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_record_conversion_matches_agent_view_contract() {
        let prompt_line = r#"{"type":"event","id":"p1","op":"add","seq":1,"at":120,"line":2,"ts":"2026-10-12T10:00:00Z","turn":{"id":"t1","index":1},"kind":"prompt","prompt":{"text":"inspect fleet","origin":"queued","images":[{"media_type":"image/png","data":"AAAA"}]}}"#;
        let StreamFrame::Event { event: pev, .. } = parse_stream_line(prompt_line).unwrap() else {
            panic!("expected event");
        };
        let prec = pev.to_event_record(None, false).unwrap();
        assert_eq!(prec.action, "prompt");
        assert_eq!(prec.tag.as_deref(), Some("you · queued"));
        assert_eq!(prec.origin.as_deref(), Some("queued"));
        assert_eq!(prec.images, 1);
        assert_eq!(prec.result, "inspect fleet");

        // Targeted non-error tool call clears `result` while error tool call preserves `output`.
        let ok_tool = r#"{"type":"event","id":"u1","op":"update","seq":2,"at":400,"line":5,"kind":"tool","tool":{"name":"Read","action":"read","target":"src/lib.rs","detail":"lines 1-20","result":{"status":"ok","output":"raw text"},"file":{"text":"fn main() {}","start":1,"total":1}}}"#;
        let StreamFrame::Event { event: tev, .. } = parse_stream_line(ok_tool).unwrap() else {
            panic!("expected event");
        };
        let trec = tev.to_event_record(Some(250), false).unwrap();
        assert_eq!(trec.at, 250, "preserved add offset overrides update offset");
        assert_eq!(trec.action, "read");
        assert_eq!(trec.target, "src/lib.rs");
        assert_eq!(trec.result, "", "non-error targeted tool hides output in result");
        assert_eq!(trec.status.as_deref(), Some("ok"));
        assert_eq!(trec.file.as_ref().unwrap().total, Some(1));

        let err_tool = r#"{"type":"event","id":"u2","op":"update","seq":3,"at":500,"line":6,"kind":"tool","tool":{"name":"Read","action":"read","target":"missing.rs","result":{"status":"error","output":"ENOENT"}}}"#;
        let StreamFrame::Event { event: eev, .. } = parse_stream_line(err_tool).unwrap() else {
            panic!("expected event");
        };
        assert_eq!(eev.to_event_record(None, false).unwrap().result, "ENOENT");

        // Noise tools filter out when show_noise=false and carry tool name as tag when show_noise=true.
        let noise_tool = r#"{"type":"event","id":"n1","op":"add","seq":4,"at":600,"line":7,"kind":"tool","tool":{"name":"TodoWrite","action":"noise","detail":"3 items"}}"#;
        let StreamFrame::Event { event: nev, .. } = parse_stream_line(noise_tool).unwrap() else {
            panic!("expected event");
        };
        assert!(nev.to_event_record(None, false).is_none());
        let nrec = nev.to_event_record(None, true).unwrap();
        assert_eq!(nrec.action, "noise");
        assert_eq!(nrec.tag.as_deref(), Some("TodoWrite"));

        // System event with `data` and omitted `text` pretty-prints `data`.
        let sys_line = r#"{"type":"event","id":"s1","op":"add","seq":5,"at":700,"line":8,"kind":"system","system":{"subtype":"turn_duration","data":{"ms":1200}}}"#;
        let StreamFrame::Event { event: sev, .. } = parse_stream_line(sys_line).unwrap() else {
            panic!("expected event");
        };
        let srec = sev.to_event_record(None, false).unwrap();
        assert_eq!(srec.subtype.as_deref(), Some("turn_duration"));
        assert!(srec.result.contains("\"ms\": 1200"));
    }
}
