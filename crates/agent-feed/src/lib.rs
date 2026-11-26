//! Live `gomuxai` data feeds for the native terminal wall, replacing the browser wall's Go relay
//! (`cli/gomux.go`, `cli/agentevents.go`, and the stream/window folding in `wall/WallAgentViews.js`).
//!
//! Built on `std`, `serde`, and `serde_json` alone so any host (`glyph3d-native`, `makepad`, or a
//! headless harness) can drive it without pulling in GPU, windowing, or async-runtime crates:
//!
//! - [`cli`] locates `~/.local/bin/agent` (or `agent` on `PATH` / an explicit binary path) and
//!   writes gomuxai's `--backend-config` JSON when session directories are configured.
//! - [`fleet`] follows `agent export --json --follow --all` on a background thread, folds its
//!   `snapshot` / `spawned` / `changed` / `removed` / `store` / `host` stream into the exact
//!   snapshot and host-grouped shapes `wall_core::bus` expects, invokes a `wake` callback only
//!   when the published fleet changes, and restarts a departed child process with exponential
//!   backoff.
//! - [`sessions`] runs `agent sessions --json`, deduplicating poll notifications by canonical
//!   session shape (ignoring `size`, `mtime`, `status_at`, and list order) and resolving queries
//!   by exact `session_id`, transcript `path`, workgroup `name`, or unique `session_id` prefix.
//! - [`events`] and [`transcript`] parse gomuxai's `EVENTS.md` JSONL stream and follow
//!   `agent transcript <session> --follow` per Agent View with the 256 KiB tail window
//!   ([`transcript::tail_from`]), `load_earlier` paging, `add`/`update` in-place folding, orphan
//!   update adoption across history pages, uncommitted provisional-reply pruning before live
//!   follow, and normalized [`events::EventRecord`] conversion matching `wall_core::agent_view`.

pub mod cli;
pub mod events;
pub mod fleet;
pub mod sessions;
pub mod transcript;

pub use cli::{
    describe_dirs, expand_home, find_agent_binary, home_dir, parse_rfc3339, parse_rfc3339_ms,
    parse_rfc3339_nanos, AgentCli, AgentCliConfig, SessionDir,
};
pub use events::{
    parse_stream_line, AgentEvent, AgentSession, Applied, EventBody, EventOp, EventPos,
    EventRecord, FileSnapshot, FileText, HarnessKind, ParentSession, PromptEvent, PromptImage,
    PromptOrigin, RequestEvent, SayEvent, SessionInfo, StreamFrame, SystemEvent, ThinkEvent,
    ToolEvent, ToolResult, ToolStatus, TurnInfo, TurnRef, Workgroup, ACTION_NOISE,
};
pub use fleet::{
    export_once, FleetAgent, FleetFollower, FleetHostAgent, FleetHostGroup, FleetOptions,
    FleetSnapshot, FleetState,
};
pub use sessions::{
    list_sessions, nest_subagents, ResolveError, SessionListSnapshot, SessionPoller, SessionStore,
};
pub use transcript::{
    read_transcript_window, tail_from, FoldOutcome, FoldedEntry, LiveAddNotice,
    TranscriptFollower, TranscriptOptions, TranscriptSnapshot, TranscriptState, TranscriptWindow,
    WindowParams, MAX_READ_BYTES, STREAM_WINDOW_BYTES,
};
