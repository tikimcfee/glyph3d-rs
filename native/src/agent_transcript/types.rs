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
        }
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
            if first_line.len() > 60 {
                format!("{}...", &first_line[..57])
            } else {
                first_line.to_string()
            }
        } else if let Some(first_action) = self.file_actions.first() {
            format!("{:?} {}", first_action.action, first_action.file_path)
        } else if let Some(first_tool) = self.tool_calls.first() {
            format!("Tool {}", first_tool.name)
        } else if let Some(first_msg) = self.assistant_messages.first() {
            let first_line = first_msg.lines().next().unwrap_or("").trim();
            if first_line.len() > 60 {
                format!("{}...", &first_line[..57])
            } else {
                first_line.to_string()
            }
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
