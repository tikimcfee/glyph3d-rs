//! Live terminal sessions over `g3d-termd` (`out/wall-port/briefs/S-term-session.md`).
//!
//! Joins [`termd_client`] (Unix socket protocol, reader thread, restart recovery), [`glyph_vt`]
//! (pure-Rust Ghostty VT emulator producing [`term_cells::Screen`] grids), and [`wall_core`]'s
//! input/mode types without any window or GPU dependency:
//! - [`Sessions`] and [`News`] provide the exact window-facing seam consumed by
//!   `native/src/wall/app.rs` (`open`, `list`, `ids`, `adopt`, `spawn`, `screen`, `text`, `pump`,
//!   `kill`, `resize`, `key`, `paste`, `mouse`, `scroll`, `repaint`), keeping a window-owned
//!   `front_screen` per terminal swapped at `pump()` so `screen(id)` returns `&Screen` for the
//!   duration of a frame with zero locking.
//! - [`SessionHub`] and [`TerminalSession`] expose the underlying per-terminal worker threads,
//!   `16 ms` / `16 KiB` leading-edge coalescer (plus DEC mode 2026 synchronized-output gating),
//!   scrollback state, `OSC 7` pwd, bells, and throughput counters.

pub mod coalesce;
mod hub;
mod inbox;
mod session;
pub mod sessions;

pub use coalesce::{
    CoalescePolicy, DEFAULT_COALESCE_INTERVAL, DEFAULT_FEED_QUANTUM_BYTES,
    DEFAULT_GROW_REPAINT_SETTLE, DEFAULT_INBOX_MAX_BYTES, DEFAULT_SYNC_TIMEOUT,
};
pub use glyph_vt::Theme;
pub use hub::{DaemonEvent, DaemonState, HubOptions, SessionHub, SpawnOptions};
pub use session::{ExitInfo, ScreenFillDelta, ScrollbackState, SessionStats, TerminalSession};
pub use sessions::{NAMESPACE, News, Sessions};
pub use term_cells::{Modes, MouseTracking, Screen};
pub use termd_client::{
    code, Capture, CaptureFormat, CaptureReply, Colors, ConnectOptions, Error, IfExists, Key,
    KeyAction, Mouse, MouseAction, MouseButton, OptionAsAlt, Role, Target, TermInfo, View,
    ViewReply, Welcome, WireError,
};
