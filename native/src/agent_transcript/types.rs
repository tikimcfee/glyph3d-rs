//! Beats: one card's worth of an agent session, one per event.
//!
//! [`AgentSession::linearize_events`] maps the session's events
//! ([`super::events`]) to [`TranscriptEvent`]s 1:1, in the order the stream
//! added them: a prompt, a thinking block, a reply, a harness record, and a
//! tool call by the action gomuxai's tool table gave it (read, edit and
//! write calls that returned become file beats, `bash` a command beat,
//! anything else a tool beat, noise included). A tool whose result landed
//! later is one beat, as its latest update left it.

use crate::revision::RevisionEngine;
use crate::spatial_scene::workdesk::FileActionKind;
use serde::{Deserialize, Serialize};

use super::events::{
    AgentSession, EventBody, FileText, PromptOrigin, ToolEvent, ToolStatus, TurnRef,
};

impl AgentSession {
    /// The session's beats, one per event, in event order. A file beat's
    /// shown text is the event's own `file` text where gomuxai carries it,
    /// else the revision the engine holds for that beat.
    pub fn linearize_events(&self, revision_engine: Option<&RevisionEngine>) -> Vec<TranscriptEvent> {
        self.events
            .iter()
            .enumerate()
            .map(|(index, event)| {
                let turn_index = self.turn_ordinal(event);
                let revision_text = |path: &str| {
                    revision_engine
                        .and_then(|re| re.history(path))
                        .and_then(|h| h.revision_for_event(index).or_else(|| h.revision_for_turn(turn_index)))
                        .map(|r| r.text.as_ref().clone())
                };
                let kind = match &event.body {
                    EventBody::Prompt(p) => TranscriptEventKind::UserPrompt {
                        prompt: p.text.clone(),
                        origin: p.origin,
                        command: Some(p.command.clone()).filter(|c| !c.is_empty()),
                        images: p.images.len(),
                    },
                    EventBody::Think(t) => TranscriptEventKind::Thinking { thought: t.text.clone(), redacted: t.redacted },
                    EventBody::Say(s) => TranscriptEventKind::AssistantResponse { message: s.text.clone(), is_final: s.is_final },
                    EventBody::System(s) => TranscriptEventKind::System { subtype: s.subtype.clone(), text: s.text.clone() },
                    EventBody::Tool(tool) => match FileActionRecord::from_tool(&event.id, tool) {
                        Some(record) => match record.action {
                            FileActionKind::Read => TranscriptEventKind::FileRead {
                                file_path: record.file_path.clone(),
                                content: match &record.file {
                                    Some(f) => Some(f.text.clone()),
                                    None => revision_text(&record.file_path),
                                },
                                first_line: record.file.as_ref().map_or(1, |f| f.start),
                                action_record: record,
                            },
                            FileActionKind::Edit => TranscriptEventKind::FileEdit {
                                file_path: record.file_path.clone(),
                                post_edit_content: match &record.file {
                                    Some(f) => Some(f.text.clone()),
                                    None => revision_text(&record.file_path),
                                },
                                action_record: record,
                            },
                            FileActionKind::Write => TranscriptEventKind::FileWrite {
                                file_path: record.file_path.clone(),
                                content: match &record.file {
                                    Some(f) => Some(f.text.clone()),
                                    None => record.written.clone().or_else(|| revision_text(&record.file_path)),
                                },
                                action_record: record,
                            },
                        },
                        None if tool.action == "bash" => TranscriptEventKind::Command {
                            name: tool.name.clone(),
                            command_line: tool.detail.clone(),
                            output: tool_output(tool),
                            status: tool.status(),
                        },
                        None => TranscriptEventKind::ToolInvocation {
                            name: tool.name.clone(),
                            action: tool.action.clone(),
                            detail: tool.detail.clone(),
                            input: tool.input.clone(),
                            output: tool_output(tool),
                            status: tool.status(),
                            sub_sessions: tool.sub_sessions.clone(),
                        },
                    },
                };
                TranscriptEvent {
                    index,
                    turn_index,
                    turn: event.turn.clone(),
                    id: event.id.clone(),
                    kind,
                    ts: event.ts.clone(),
                }
            })
            .collect()
    }
}

/// A tool's result text, when it has any.
fn tool_output(tool: &ToolEvent) -> Option<String> {
    tool.result.as_ref().map(|r| r.output.clone()).filter(|o| !o.is_empty())
}

/// Truncate a string to at most `max_chars` Unicode scalar values, slicing safely on a character boundary.
pub fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => &s[..byte_idx],
        None => s,
    }
}

/// Truncate a string with an ellipsis ("...") if it exceeds `max_chars` Unicode characters.
pub fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    if s.chars().count() > max_chars {
        let keep = max_chars.saturating_sub(3);
        format!("{}...", truncate_chars(s, keep))
    } else {
        s.to_string()
    }
}

/// One replacement an edit asked for (its input), for an edit whose event
/// carries no file text: the revision engine replays it.
#[derive(Debug, Clone, PartialEq)]
pub struct Replacement {
    pub old: String,
    pub new: String,
    pub all: bool,
}

/// A read, edit or write that returned, as the revision engine and the file
/// beats see it.
#[derive(Debug, Clone, PartialEq)]
pub struct FileActionRecord {
    pub tool_id: String,
    pub tool_name: String,
    /// The event's `target`: relative to the session's cwd when under it.
    pub file_path: String,
    pub action: FileActionKind,
    pub summary: String,
    /// The target's text as the call saw or left it, where gomuxai has it.
    pub file: Option<FileText>,
    /// An edit's hunks: `meta.patch`, the result's own diff.
    pub hunks: Vec<DiffHunkRecord>,
    /// Lines added and removed (`meta`), when the tool table counted them.
    pub added_removed: Option<(usize, usize)>,
    /// What an edit asked to replace (no-file fallback only).
    pub replacements: Vec<Replacement>,
    /// What a write asked to write (no-file fallback only).
    pub written: Option<String>,
}

impl FileActionRecord {
    /// The file action a tool call is, if it is one: a read, edit or write
    /// with a target that returned. (A call that failed, was interrupted or
    /// never returned changed no file anyone saw; it stays a tool beat.)
    pub fn from_tool(tool_id: &str, tool: &ToolEvent) -> Option<Self> {
        let action = match tool.action.as_str() {
            "read" => FileActionKind::Read,
            "edit" => FileActionKind::Edit,
            "write" => FileActionKind::Write,
            _ => return None,
        };
        if tool.target.is_empty() || tool.status() != ToolStatus::Ok {
            return None;
        }
        let added_removed = match (tool.meta_count("added"), tool.meta_count("removed")) {
            (Some(a), Some(r)) => Some((a, r)),
            _ => None,
        };
        Some(Self {
            tool_id: tool_id.to_string(),
            tool_name: tool.name.clone(),
            file_path: tool.target.clone(),
            action,
            summary: format!("{} {}", tool.name, tool.target),
            file: tool.file.clone(),
            hunks: patch_hunks(tool),
            added_removed,
            replacements: if action == FileActionKind::Edit { input_replacements(&tool.input) } else { Vec::new() },
            written: if action == FileActionKind::Write { input_text(&tool.input, &["content", "CodeContent", "Content"]) } else { None },
        })
    }
}

/// `meta.patch`: the result's structuredPatch hunks, as gomuxai keeps them.
fn patch_hunks(tool: &ToolEvent) -> Vec<DiffHunkRecord> {
    let Some(patch) = tool.meta.as_ref().and_then(|m| m.get("patch")).and_then(|p| p.as_array()) else {
        return Vec::new();
    };
    let num = |h: &serde_json::Value, k: &str| h.get(k).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    patch
        .iter()
        .map(|h| DiffHunkRecord {
            old_start: num(h, "oldStart"),
            old_lines: num(h, "oldLines"),
            new_start: num(h, "newStart"),
            new_lines: num(h, "newLines"),
            lines: h
                .get("lines")
                .and_then(|l| l.as_array())
                .map(|l| l.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
        })
        .collect()
}

/// The first of `keys` set to a string in a tool's input.
fn input_text(input: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| input.get(*k)?.as_str().map(str::to_string))
}

/// An edit's replacements, from its input, read by the keys gomuxai's tool
/// table reads them by (`countReplacements` in its `tools.go`): `edits[]`
/// or the input itself; `old_string` / `new_string` / `replace_all`, else
/// agy's `TargetContent` / `ReplacementContent` / `AllowMultiple`.
fn input_replacements(input: &serde_json::Value) -> Vec<Replacement> {
    let edits: Vec<&serde_json::Value> = match input.get("edits").and_then(|e| e.as_array()) {
        Some(list) => list.iter().collect(),
        None => vec![input],
    };
    edits
        .into_iter()
        .filter_map(|e| {
            let s = |k: &str| e.get(k).and_then(|v| v.as_str()).map(str::to_string);
            let b = |k: &str| e.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
            match (s("old_string"), s("new_string")) {
                (Some(old), Some(new)) => Some(Replacement { old, new, all: b("replace_all") }),
                _ => Some(Replacement { old: s("TargetContent")?, new: s("ReplacementContent")?, all: b("AllowMultiple") }),
            }
        })
        .collect()
}

/// A unified diff hunk (an edit's structuredPatch, as `meta.patch` carries it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffHunkRecord {
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    pub lines: Vec<String>,
}

/// An atomic narrative beat in the agent session history: one event.
///
/// Every card in 3D space corresponds to exactly one event:
/// - Left Page: Spec & Metadata (Tool name, arguments, line count, status).
/// - Right Page: Artifact / Payload (Full file with highlighted edits/reads, diff hunks, terminal output, reasoning, or assistant message).
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptEvent {
    /// The beat's position: its event's position in the stream's add order.
    pub index: usize,
    /// The turn's ordinal among the session's turns (0-based), for grouping
    /// and turn navigation.
    pub turn_index: usize,
    /// The turn as the event names it.
    pub turn: Option<TurnRef>,
    /// The event's id.
    pub id: String,
    pub kind: TranscriptEventKind,
    /// The source record's own timestamp, as written.
    pub ts: String,
}

impl TranscriptEvent {
    /// Concise summary for card labels and rolodex/splay HUD.
    pub fn summary(&self) -> String {
        self.kind.summary()
    }

    /// File path target, if this event interacts with a file.
    pub fn file_path(&self) -> Option<&str> {
        self.kind.file_path()
    }

    /// Colors for the Left and Right page header banners: (left_banner_rgba, right_banner_rgba).
    pub fn banner_colors(&self) -> ([f32; 4], [f32; 4]) {
        self.kind.banner_colors()
    }

    /// The turn as a card names it: the transcript's own number (which a
    /// notification's turn shares with the prompt before it), or `?` where
    /// the transcript doesn't say.
    pub fn turn_label(&self) -> String {
        match &self.turn {
            Some(t) if t.index > 0 => format!("Turn {}{}", t.index, if t.derived { " (derived)" } else { "" }),
            _ => "Turn ?".to_string(),
        }
    }
}

/// The specific payload and cognitive nature of an atomic narrative beat.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptEventKind {
    UserPrompt {
        prompt: String,
        origin: PromptOrigin,
        /// A slash command's `/name`.
        command: Option<String>,
        /// How many images the prompt carried.
        images: usize,
    },
    Thinking {
        thought: String,
        /// Signature only: the harness kept no text.
        redacted: bool,
    },
    FileRead {
        file_path: String,
        action_record: FileActionRecord,
        content: Option<String>,
        /// The file line `content` starts at (a partial read's first line).
        first_line: usize,
    },
    FileEdit {
        file_path: String,
        action_record: FileActionRecord,
        post_edit_content: Option<String>,
    },
    FileWrite {
        file_path: String,
        action_record: FileActionRecord,
        content: Option<String>,
    },
    Command {
        name: String,
        command_line: String,
        output: Option<String>,
        status: ToolStatus,
    },
    ToolInvocation {
        name: String,
        /// The tool table's action (`noise` for bookkeeping tools).
        action: String,
        detail: String,
        input: serde_json::Value,
        output: Option<String>,
        status: ToolStatus,
        /// Sessions the call started (a subagent's).
        sub_sessions: Vec<String>,
    },
    AssistantResponse {
        message: String,
        /// A turn's closing reply.
        is_final: bool,
    },
    /// A harness record: turn timing, compaction, an API error, an
    /// interrupt, a model fallback.
    System {
        subtype: String,
        text: String,
    },
}

/// A status worth a tag in a one-line summary (ok says nothing).
fn status_tag(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Ok => "",
        ToolStatus::Error => " [err]",
        ToolStatus::Pending => " [pending]",
        ToolStatus::Interrupted => " [interrupted]",
        ToolStatus::Background => " [background]",
    }
}

impl TranscriptEventKind {
    pub fn summary(&self) -> String {
        let file_name = |file_path: &str| {
            std::path::Path::new(file_path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(file_path)
                .to_string()
        };
        match self {
            Self::UserPrompt { prompt, origin, .. } => {
                let first = prompt.lines().next().unwrap_or("").trim();
                format!("{}: {}", origin.label(), truncate_with_ellipsis(first, 55))
            }
            Self::Thinking { thought, redacted } => {
                if *redacted {
                    return "Thinking: (redacted)".to_string();
                }
                let first = thought.lines().next().unwrap_or("").trim();
                format!("Thinking: {}", truncate_with_ellipsis(first, 55))
            }
            Self::FileRead { file_path, .. } => format!("Read: {}", file_name(file_path)),
            Self::FileEdit { file_path, action_record, .. } => {
                let diff_note = match action_record.added_removed {
                    Some((added, removed)) => format!(" (+{added} -{removed})"),
                    None => String::new(),
                };
                format!("Edit: {}{diff_note}", file_name(file_path))
            }
            Self::FileWrite { file_path, .. } => format!("Write: {}", file_name(file_path)),
            Self::Command { command_line, status, .. } => {
                let first = command_line.lines().next().unwrap_or("").trim();
                format!("$ {}{}", truncate_with_ellipsis(first, 50), status_tag(*status))
            }
            Self::ToolInvocation { name, action, status, .. } => {
                let label = if action == super::events::ACTION_NOISE { "Noise" } else { "Tool" };
                format!("{label}: {name}{}", status_tag(*status))
            }
            Self::AssistantResponse { message, .. } => {
                let first = message.lines().next().unwrap_or("").trim();
                format!("Assistant: {}", truncate_with_ellipsis(first, 55))
            }
            Self::System { subtype, .. } => format!("System: {subtype}"),
        }
    }

    pub fn file_path(&self) -> Option<&str> {
        match self {
            Self::FileRead { file_path, .. }
            | Self::FileEdit { file_path, .. }
            | Self::FileWrite { file_path, .. } => Some(file_path.as_str()),
            _ => None,
        }
    }

    /// A bookkeeping tool's beat (the tool table's `noise`): shown, in the
    /// dim `noise` banner and accent.
    pub fn is_noise(&self) -> bool {
        matches!(self, Self::ToolInvocation { action, .. } if action == super::events::ACTION_NOISE)
    }

    /// Banner pair (mind, impact): `[agent_cards.banners]`.
    pub fn banner_colors(&self) -> ([f32; 4], [f32; 4]) {
        let b = &crate::config::settings().agent_cards.banners;
        let [mind, impact] = match self {
            Self::UserPrompt { .. } => b.user_prompt,
            Self::Thinking { .. } => b.thinking,
            Self::FileRead { .. } => b.file_read,
            Self::FileEdit { .. } => b.file_edit,
            Self::FileWrite { .. } => b.file_write,
            Self::Command { .. } => b.command,
            Self::ToolInvocation { .. } if self.is_noise() => b.noise,
            Self::ToolInvocation { .. } => b.tool_invocation,
            Self::AssistantResponse { .. } => b.assistant_response,
            Self::System { .. } => b.system,
        };
        (mind, impact)
    }
}
