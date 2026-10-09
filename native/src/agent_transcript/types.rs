//! Universal domain models for autonomous agent transcripts, turns, and file interactions.
//!
//! Provides harness-agnostic representations that support both Claude Code JSONL,
//! Antigravity JSONL, and future agent wire logs.

use serde::{Deserialize, Serialize};
use crate::spatial_scene::workdesk::FileActionKind;

/// Autonomous agent harness or environment that generated the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HarnessKind {
    ClaudeCode,
    Antigravity,
    KimiCode,
    Generic,
}

impl HarnessKind {
    pub fn name(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Antigravity => "antigravity",
            Self::KimiCode => "kimi",
            Self::Generic => "generic",
        }
    }
}

/// Metadata and sequential turns of an agent session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSession {
    pub harness: HarnessKind,
    pub session_id: String,
    pub cwd: Option<String>,
    pub slug: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub version: Option<String>,
    pub git_branch: Option<String>,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
    pub turns: Vec<AgentTurn>,
}

impl AgentSession {
    pub fn new(harness: HarnessKind, session_id: impl Into<String>) -> Self {
        Self {
            harness,
            session_id: session_id.into(),
            cwd: None,
            slug: None,
            title: None,
            model: None,
            version: None,
            git_branch: None,
            first_ts: None,
            last_ts: None,
            turns: Vec::new(),
        }
    }

    /// Number of turns in the session.
    pub fn turn_count(&self) -> usize {
        self.turns.len()
    }

    /// Total number of tool invocations across all turns.
    pub fn total_tool_calls(&self) -> usize {
        self.turns.iter().map(|t| t.tool_calls.len()).sum()
    }

    /// Total number of file actions (reads, edits, writes) across all turns.
    pub fn total_file_actions(&self) -> usize {
        self.turns.iter().map(|t| t.file_actions.len()).sum()
    }

    /// Set of unique file paths touched across all turns.
    pub fn touched_files(&self) -> Vec<String> {
        let mut paths = Vec::new();
        for turn in &self.turns {
            for action in &turn.file_actions {
                if !paths.contains(&action.file_path) {
                    paths.push(action.file_path.clone());
                }
            }
        }
        paths
    }

    /// Linearize all turns into an atomic chronological sequence of transcript events.
    pub fn linearize_events(&self, revision_engine: Option<&crate::revision::RevisionEngine>) -> Vec<TranscriptEvent> {
        let mut events = Vec::new();
        let mut event_idx = 0;

        for turn in &self.turns {
            // 1. User Prompt (if present)
            if let Some(ref prompt) = turn.prompt {
                events.push(TranscriptEvent {
                    index: event_idx,
                    turn_index: turn.turn_index,
                    kind: TranscriptEventKind::UserPrompt {
                        prompt: prompt.clone(),
                    },
                    timestamp: turn.timestamp,
                });
                event_idx += 1;
            }

            // 2. Thinking, tool calls and assistant text, in the order they
            // happened (transcript line order, block order within a line), the
            // way the JS adapter's one event stream has always run. Turns built
            // without recorded steps keep the old grouped order.
            let mut matched_file_actions = std::collections::HashSet::new();
            for step in turn.ordered_steps() {
                match step {
                    TurnStep::Thinking(i) => {
                        events.push(TranscriptEvent {
                            index: event_idx,
                            turn_index: turn.turn_index,
                            kind: TranscriptEventKind::Thinking {
                                thought: turn.thinking[i].clone(),
                            },
                            timestamp: turn.timestamp,
                        });
                        event_idx += 1;
                    }
                    TurnStep::Message(i) => {
                        events.push(TranscriptEvent {
                            index: event_idx,
                            turn_index: turn.turn_index,
                            kind: TranscriptEventKind::AssistantResponse {
                                message: turn.assistant_messages[i].clone(),
                            },
                            timestamp: turn.timestamp,
                        });
                        event_idx += 1;
                    }
                    TurnStep::Tool(i) => {
                        let tool = &turn.tool_calls[i];
                        // Match with FileActionRecord if tool_id aligns
                        let fa_opt = turn.file_actions.iter().find(|fa| fa.tool_id == tool.id);
                        if let Some(fa) = fa_opt {
                            matched_file_actions.insert(fa.tool_id.clone());
                            match fa.action {
                                FileActionKind::Edit => {
                                    let post_edit = revision_engine
                                        .and_then(|re| re.history(&fa.file_path))
                                        .and_then(|h| h.revision_for_event(event_idx).or_else(|| h.revision_for_turn(turn.turn_index)))
                                        .map(|r| r.text.as_ref().clone())
                                        .or_else(|| fa.new_content.clone());
                                    events.push(TranscriptEvent {
                                        index: event_idx,
                                        turn_index: turn.turn_index,
                                        kind: TranscriptEventKind::FileEdit {
                                            file_path: fa.file_path.clone(),
                                            action_record: fa.clone(),
                                            post_edit_content: post_edit,
                                        },
                                        timestamp: tool.timestamp.or(turn.timestamp),
                                    });
                                    event_idx += 1;
                                }
                                FileActionKind::Read => {
                                    let content = fa
                                        .new_content
                                        .clone()
                                        .or_else(|| {
                                            revision_engine
                                                .and_then(|re| re.history(&fa.file_path))
                                                .and_then(|h| h.get(0))
                                                .map(|r| r.text.as_ref().clone())
                                        });
                                    events.push(TranscriptEvent {
                                        index: event_idx,
                                        turn_index: turn.turn_index,
                                        kind: TranscriptEventKind::FileRead {
                                            file_path: fa.file_path.clone(),
                                            action_record: fa.clone(),
                                            content,
                                        },
                                        timestamp: tool.timestamp.or(turn.timestamp),
                                    });
                                    event_idx += 1;
                                }
                                FileActionKind::Write => {
                                    let content = fa.new_content.clone();
                                    events.push(TranscriptEvent {
                                        index: event_idx,
                                        turn_index: turn.turn_index,
                                        kind: TranscriptEventKind::FileWrite {
                                            file_path: fa.file_path.clone(),
                                            action_record: fa.clone(),
                                            content,
                                        },
                                        timestamp: tool.timestamp.or(turn.timestamp),
                                    });
                                    event_idx += 1;
                                }
                                FileActionKind::AstAnalysis => {
                                    let output = extract_output_str(tool.response.as_ref());
                                    events.push(TranscriptEvent {
                                        index: event_idx,
                                        turn_index: turn.turn_index,
                                        kind: TranscriptEventKind::ToolInvocation {
                                            name: tool.name.clone(),
                                            input: tool.input.clone(),
                                            output,
                                            is_error: tool.is_error,
                                        },
                                        timestamp: tool.timestamp.or(turn.timestamp),
                                    });
                                    event_idx += 1;
                                }
                            }
                        } else if is_command_tool(&tool.name) {
                            let cmd_str = extract_command_str(&tool.input);
                            let output = extract_output_str(tool.response.as_ref());
                            events.push(TranscriptEvent {
                                index: event_idx,
                                turn_index: turn.turn_index,
                                kind: TranscriptEventKind::Command {
                                    name: tool.name.clone(),
                                    command_line: cmd_str,
                                    output,
                                    is_error: tool.is_error,
                                },
                                timestamp: tool.timestamp.or(turn.timestamp),
                            });
                            event_idx += 1;
                        } else {
                            let output = extract_output_str(tool.response.as_ref());
                            events.push(TranscriptEvent {
                                index: event_idx,
                                turn_index: turn.turn_index,
                                kind: TranscriptEventKind::ToolInvocation {
                                    name: tool.name.clone(),
                                    input: tool.input.clone(),
                                    output,
                                    is_error: tool.is_error,
                                },
                                timestamp: tool.timestamp.or(turn.timestamp),
                            });
                            event_idx += 1;
                        }
                    }
                }
            }

            // Unmatched file actions (if any)
            for fa in &turn.file_actions {
                if !matched_file_actions.contains(&fa.tool_id) {
                    match fa.action {
                        FileActionKind::Edit => {
                            let post_edit = revision_engine
                                .and_then(|re| re.history(&fa.file_path))
                                .and_then(|h| h.revision_for_event(event_idx).or_else(|| h.revision_for_turn(turn.turn_index)))
                                .map(|r| r.text.as_ref().clone())
                                .or_else(|| fa.new_content.clone());
                            events.push(TranscriptEvent {
                                index: event_idx,
                                turn_index: turn.turn_index,
                                kind: TranscriptEventKind::FileEdit {
                                    file_path: fa.file_path.clone(),
                                    action_record: fa.clone(),
                                    post_edit_content: post_edit,
                                },
                                timestamp: turn.timestamp,
                            });
                            event_idx += 1;
                        }
                        FileActionKind::Read => {
                            let content = fa.new_content.clone();
                            events.push(TranscriptEvent {
                                index: event_idx,
                                turn_index: turn.turn_index,
                                kind: TranscriptEventKind::FileRead {
                                    file_path: fa.file_path.clone(),
                                    action_record: fa.clone(),
                                    content,
                                },
                                timestamp: turn.timestamp,
                            });
                            event_idx += 1;
                        }
                        FileActionKind::Write => {
                            let content = fa.new_content.clone();
                            events.push(TranscriptEvent {
                                index: event_idx,
                                turn_index: turn.turn_index,
                                kind: TranscriptEventKind::FileWrite {
                                    file_path: fa.file_path.clone(),
                                    action_record: fa.clone(),
                                    content,
                                },
                                timestamp: turn.timestamp,
                            });
                            event_idx += 1;
                        }
                        FileActionKind::AstAnalysis => {}
                    }
                }
            }
        }

        events
    }
}

/// A coherent interactive turn between the user and the agent.
///
/// Embodies the 2-page Agent Turn Card mental model:
/// - Left Page (Mind): User prompt, chain-of-thought reasoning, tools invoked.
/// - Right Page (Impact): Assistant messages, tool outputs, diffs, touched files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentTurn {
    pub turn_index: usize,
    pub prompt: Option<String>,
    pub thinking: Vec<String>,
    pub assistant_messages: Vec<String>,
    pub tool_calls: Vec<ToolCallRecord>,
    pub file_actions: Vec<FileActionRecord>,
    pub timestamp: Option<i64>,
    /// The order thinking, tool calls and assistant text happened in, as
    /// indices into those three vectors. Filled by the `push_*` methods;
    /// empty for a turn built field by field, which then linearizes in the
    /// old grouped order (see `ordered_steps`).
    #[serde(default)]
    pub steps: Vec<TurnStep>,
}

/// One step of a turn, indexing into `AgentTurn::{thinking, tool_calls,
/// assistant_messages}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnStep {
    Thinking(usize),
    Tool(usize),
    Message(usize),
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

impl AgentTurn {
    pub fn new(turn_index: usize) -> Self {
        Self {
            turn_index,
            prompt: None,
            thinking: Vec::new(),
            assistant_messages: Vec::new(),
            tool_calls: Vec::new(),
            file_actions: Vec::new(),
            timestamp: None,
            steps: Vec::new(),
        }
    }

    /// Append a thinking block, recording its place in the turn.
    pub fn push_thinking(&mut self, thought: String) {
        self.steps.push(TurnStep::Thinking(self.thinking.len()));
        self.thinking.push(thought);
    }

    /// Append an assistant text block, recording its place in the turn.
    pub fn push_message(&mut self, message: String) {
        self.steps.push(TurnStep::Message(self.assistant_messages.len()));
        self.assistant_messages.push(message);
    }

    /// Append a tool call, recording its place in the turn.
    pub fn push_tool(&mut self, call: ToolCallRecord) {
        self.steps.push(TurnStep::Tool(self.tool_calls.len()));
        self.tool_calls.push(call);
    }

    /// The turn's steps in the order they happened. A turn with no recorded
    /// steps (built field by field) falls back to the old grouping: every
    /// thinking block, then every tool call, then every assistant message.
    pub fn ordered_steps(&self) -> Vec<TurnStep> {
        if !self.steps.is_empty() {
            return self.steps.clone();
        }
        (0..self.thinking.len())
            .map(TurnStep::Thinking)
            .chain((0..self.tool_calls.len()).map(TurnStep::Tool))
            .chain((0..self.assistant_messages.len()).map(TurnStep::Message))
            .collect()
    }

    /// Concatenated reasoning prose from all thinking blocks in this turn.
    pub fn combined_thinking(&self) -> String {
        self.thinking.join("\n\n")
    }

    /// Concatenated assistant response prose from all text blocks in this turn.
    pub fn combined_assistant_text(&self) -> String {
        self.assistant_messages.join("\n\n")
    }

    /// A concise one-line summary of this turn for card labels or deck navigation.
    pub fn summary(&self) -> String {
        if let Some(ref p) = self.prompt {
            let first_line = p.lines().next().unwrap_or("").trim();
            truncate_with_ellipsis(first_line, 60)
        } else if let Some(first_action) = self.file_actions.first() {
            format!("{:?} {}", first_action.action, first_action.file_path)
        } else if let Some(first_tool) = self.tool_calls.first() {
            format!("Tool {}", first_tool.name)
        } else if let Some(first_msg) = self.assistant_messages.first() {
            let first_line = first_msg.lines().next().unwrap_or("").trim();
            truncate_with_ellipsis(first_line, 60)
        } else {
            format!("Turn {}", self.turn_index)
        }
    }
}

/// Record of an invoked tool call paired with its execution response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
    pub response: Option<serde_json::Value>,
    pub is_error: bool,
    pub timestamp: Option<i64>,
}

/// File interaction extracted from an agent tool call (Read, Edit, Write, etc.).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileActionRecord {
    pub tool_id: String,
    pub file_path: String,
    pub action: FileActionKind,
    pub summary: String,
    pub old_content: Option<String>,
    pub new_content: Option<String>,
    #[serde(default)]
    pub original_file: Option<String>,
    pub hunks: Vec<DiffHunkRecord>,
}

/// A unified diff hunk extracted from structuredPatch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffHunkRecord {
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    pub lines: Vec<String>,
}

/// An atomic narrative beat in the agent session history.
///
/// Embodies literal 1:1 event atomicity:
/// Every card in 3D space corresponds to exactly one event:
/// - Left Page: Spec & Metadata (Tool name, arguments, line count, status).
/// - Right Page: Artifact / Payload (Full file with highlighted edits/reads, diff hunks, terminal output, reasoning, or assistant message).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptEvent {
    pub index: usize,
    pub turn_index: usize,
    pub kind: TranscriptEventKind,
    pub timestamp: Option<i64>,
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
}

/// The specific payload and cognitive nature of an atomic narrative beat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TranscriptEventKind {
    UserPrompt {
        prompt: String,
    },
    Thinking {
        thought: String,
    },
    FileRead {
        file_path: String,
        action_record: FileActionRecord,
        content: Option<String>,
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
        is_error: bool,
    },
    ToolInvocation {
        name: String,
        input: serde_json::Value,
        output: Option<String>,
        is_error: bool,
    },
    AssistantResponse {
        message: String,
    },
}

impl TranscriptEventKind {
    pub fn summary(&self) -> String {
        match self {
            Self::UserPrompt { prompt } => {
                let first = prompt.lines().next().unwrap_or("").trim();
                format!("User: {}", truncate_with_ellipsis(first, 55))
            }
            Self::Thinking { thought } => {
                let first = thought.lines().next().unwrap_or("").trim();
                format!("Thinking: {}", truncate_with_ellipsis(first, 55))
            }
            Self::FileRead { file_path, .. } => {
                let name = std::path::Path::new(file_path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(file_path);
                format!("Read: {name}")
            }
            Self::FileEdit { file_path, action_record, .. } => {
                let name = std::path::Path::new(file_path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(file_path);
                let diff_note = if !action_record.hunks.is_empty() {
                    let added: usize = action_record
                        .hunks
                        .iter()
                        .map(|h| h.lines.iter().filter(|l| l.starts_with('+')).count())
                        .sum();
                    let removed: usize = action_record
                        .hunks
                        .iter()
                        .map(|h| h.lines.iter().filter(|l| l.starts_with('-')).count())
                        .sum();
                    format!(" (+{added} -{removed})")
                } else {
                    String::new()
                };
                format!("Edit: {name}{diff_note}")
            }
            Self::FileWrite { file_path, .. } => {
                let name = std::path::Path::new(file_path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(file_path);
                format!("Write: {name}")
            }
            Self::Command {
                command_line,
                is_error,
                ..
            } => {
                let first = command_line.lines().next().unwrap_or("").trim();
                let status = if *is_error { " [err]" } else { "" };
                format!("$ {}{status}", truncate_with_ellipsis(first, 50))
            }
            Self::ToolInvocation { name, is_error, .. } => {
                let status = if *is_error { " [err]" } else { "" };
                format!("Tool: {name}{status}")
            }
            Self::AssistantResponse { message } => {
                let first = message.lines().next().unwrap_or("").trim();
                format!("Assistant: {}", truncate_with_ellipsis(first, 55))
            }
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
            Self::ToolInvocation { .. } => b.tool_invocation,
            Self::AssistantResponse { .. } => b.assistant_response,
        };
        (mind, impact)
    }
}

pub fn is_command_tool(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("bash")
        || lower.contains("command")
        || lower.contains("shell")
        || lower.contains("terminal")
        || lower.contains("exec")
        || lower.contains("run_command")
}

pub fn extract_command_str(input: &serde_json::Value) -> String {
    if let serde_json::Value::Object(map) = input {
        if let Some(cmd) = map
            .get("CommandLine")
            .or_else(|| map.get("command"))
            .or_else(|| map.get("cmd"))
        {
            if let Some(s) = cmd.as_str() {
                return s.to_string();
            }
        }
    }
    serde_json::to_string(input).unwrap_or_default()
}

pub fn extract_output_str(response: Option<&serde_json::Value>) -> Option<String> {
    let resp = response?;
    match resp {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(map) => {
            if let Some(out) = map
                .get("output")
                .or_else(|| map.get("stdout"))
                .or_else(|| map.get("result"))
                .or_else(|| map.get("content"))
            {
                if let Some(s) = out.as_str() {
                    return Some(s.to_string());
                } else {
                    return Some(serde_json::to_string_pretty(out).unwrap_or_default());
                }
            }
            Some(serde_json::to_string_pretty(resp).unwrap_or_default())
        }
        _ => Some(serde_json::to_string_pretty(resp).unwrap_or_default()),
    }
}
