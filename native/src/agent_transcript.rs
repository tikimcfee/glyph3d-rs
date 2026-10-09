//! Agent transcript ingestion, turn pairing, and normalization.
//!
//! Provides the primary ingestion seam for agent session histories:
//! - [`claude`]: Claude Code session JSONL adapter (2-pass tool pairing, distance matching, metadata harvesting).
//! - [`antigravity`]: Antigravity session JSONL adapter.
//! - [`kimi`]: Kimi Code `wire.jsonl` event-log adapter.
//! - [`types`]: Harness-agnostic domain models (`AgentSession`, `AgentTurn`, `ToolCallRecord`, `FileActionRecord`).

pub mod antigravity;
pub mod claude;
pub mod discovery;
pub mod kimi;
pub mod staging;
pub mod types;

#[cfg(test)]
mod tests;

pub use antigravity::parse_antigravity_session;
pub use claude::parse_claude_session;
pub use discovery::{scan_agent_sessions, DiscoveredSession, SessionHarnessFilter};
pub use kimi::parse_kimi_session;
pub use staging::{stage_agent_session, stage_agent_session_with_options};
pub use types::{
    AgentSession, AgentTurn, DiffHunkRecord, FileActionRecord, HarnessKind, ToolCallRecord,
    TranscriptEvent, TranscriptEventKind,
};

/// Automatically detect transcript format and parse an [`AgentSession`].
pub fn detect_and_parse_session(content: &str, session_id: &str) -> AgentSession {
    if kimi::looks_like_kimi(content) {
        return parse_kimi_session(content, session_id);
    }
    for line in content.lines().take(30) {
        if line.contains("\"step_index\"")
            || line.contains("\"PLANNER_RESPONSE\"")
            || line.contains("\"USER_INPUT\"")
        {
            return parse_antigravity_session(content, session_id);
        }
    }
    parse_claude_session(content, session_id)
}

/// Load and parse an agent transcript from a filesystem path.
pub fn load_session_from_path(path: &std::path::Path) -> Result<AgentSession, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read session file {}: {e}", path.display()))?;
    let session_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    // A Kimi transcript is `wire.jsonl` inside a session directory, which
    // holds the id, title and working directory the log itself does not.
    if kimi::looks_like_kimi(&content) {
        return Ok(kimi::parse_kimi_session_at(&content, path, session_id));
    }
    Ok(detect_and_parse_session(&content, session_id))
}

