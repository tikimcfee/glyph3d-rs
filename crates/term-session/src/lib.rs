//! Live terminal sessions over `g3d-termd` (`out/wall-port/briefs/S-term-session.md`).
//!
//! Joins [`termd_client`] (Unix socket protocol, reader thread, restart recovery) and [`glyph_vt`]
//! (pure-Rust Ghostty VT emulator producing [`term_cells::Screen`] grids) without any window or
//! GPU dependency:
//! - [`SessionHub`] manages one connection to a `g3d-termd` namespace, tracks daemon lifecycle
//!   events ([`DaemonState`], [`DaemonEvent`]), and routes per-terminal frames onto dedicated
//!   worker threads.
//! - Each [`TerminalSession`] runs its `Vt` emulator on its own OS thread, coalescing screen
//!   publications with a leading-edge `16 ms` / `16 KiB` policy (and DEC mode 2026 synchronized
//!   output gating) so keystroke echoes publish immediately while `yes` or 14 concurrent build
//!   floods stay strictly frame-bounded and never block the window's UI thread.

pub mod coalesce;
mod hub;
mod inbox;
mod session;

pub use coalesce::{
    CoalescePolicy, DEFAULT_COALESCE_INTERVAL, DEFAULT_FEED_QUANTUM_BYTES, DEFAULT_INBOX_MAX_BYTES,
    DEFAULT_SYNC_TIMEOUT,
};
pub use glyph_vt::Theme;
pub use hub::{DaemonEvent, DaemonState, HubOptions, SessionHub, SpawnOptions};
pub use session::{ExitInfo, ScrollbackState, SessionStats, TerminalSession};
pub use term_cells::{Modes, MouseTracking, Screen};
pub use termd_client::{
    code, Colors, ConnectOptions, Error, IfExists, Key, KeyAction, Mouse, MouseAction,
    MouseButton, OptionAsAlt, Role, Target, TermInfo, View, ViewReply, Welcome, WireError,
};
