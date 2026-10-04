//! Agent transcript ingestion, turn pairing, and normalization.
//!
//! Provides the primary ingestion seam for agent session histories:
//! - [`claude`]: Claude Code session JSONL adapter (2-pass tool pairing, distance matching, metadata harvesting).
//! - [`antigravity`]: Antigravity session JSONL adapter.
//! - [`types`]: Harness-agnostic domain models (`AgentSession`, `AgentTurn`, `ToolCallRecord`, `FileActionRecord`).

pub mod antigravity;
pub mod claude;
pub mod types;

#[cfg(test)]
mod tests;

pub use antigravity::parse_antigravity_session;
pub use claude::parse_claude_session;
pub use types::{
    AgentSession, AgentTurn, DiffHunkRecord, FileActionRecord, HarnessKind, ToolCallRecord,
};
