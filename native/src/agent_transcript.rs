//! Agent sessions, read from gomuxai's `agent` CLI and staged as an Agent Carrel.
//!
//! gomuxai is the one reader of agent transcripts (Claude Code, agy, Kimi
//! Code); this renderer reads only what its `agent` CLI prints, and nothing
//! here opens a harness's transcript or scans a harness's directories.
//! - [`agent_cli`]: runs `agent sessions --json` (the session list) and
//!   `agent transcript <session>` (one session's events).
//! - [`events`]: the event stream (gomuxai's `docs/EVENTS.md`) folded into an
//!   [`AgentSession`]: events in stream order, updates applied in place.
//! - [`types`]: beats, one per event (`AgentSession::linearize_events`).
//! - [`staging`]: beats and file revisions staged as glyph records.
//!
//! The first three are UI-free: the terminal launcher, the F7 session
//! browser and any other view call the same functions.

pub mod agent_cli;
pub mod events;
pub mod staging;
pub mod types;

#[cfg(test)]
pub(crate) mod test_stream;
#[cfg(test)]
mod tests;

pub use agent_cli::AgentCli;
pub use events::{AgentSession, HarnessKind, SessionInfo};
pub use staging::{stage_agent_session, stage_agent_session_with_options};
pub use types::{DiffHunkRecord, FileActionRecord, TranscriptEvent, TranscriptEventKind};

/// A session read and its file revisions built: what an Agent Carrel shows.
pub type LoadedSession = std::sync::Arc<(AgentSession, crate::revision::RevisionEngine)>;

/// Read one session through `agent transcript` and build its revisions.
pub fn load_session(agent: &AgentCli, session: &str) -> Result<LoadedSession, String> {
    let session = agent.transcript(session)?;
    let engine = crate::revision::RevisionEngine::for_session(&session);
    Ok(std::sync::Arc::new((session, engine)))
}
