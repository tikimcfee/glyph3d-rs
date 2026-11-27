//! Non-blocking client for `g3d-termd` wire protocol 1.5 (`out/WALL-PLAN-2026-11-26.md`, A).
//!
//! [`Termd::connect_or_start`] connects to a daemon socket (starting `g3d-termd --daemon` if
//! nothing listens) and spawns one reader thread. Requests from any thread via [`Termd`] or
//! [`TermdHandle`] block only their caller until the matching `REPLY` arrives; every non-reply
//! frame (`OUTPUT`, `REPAINT`, `EVENT`) is queued onto [`Termd::events`] in wire order and
//! notifies the host via `wake`.

mod client;
pub mod frame;
pub mod msg;
pub mod paths;

pub use client::{
    namespace_left, ConnectOptions, Error, Event, Termd, TermdHandle, DEFAULT_RECONNECT_WINDOW,
    DEFAULT_REQUEST_TIMEOUT, NAMESPACE_LEFT,
};
pub use msg::{
    code, Capture, CaptureFormat, CaptureReply, Colors, Create, CreateReply, Flow, HolderInfo,
    IfExists, Key, KeyAction, Mode, Mouse, MouseAction, MouseButton, OptionAsAlt, Role, Subscribe,
    SubscriberCounts, Subscription, TermInfo, View, ViewReply, Welcome, WireError, PROTO,
};
pub use paths::{find_termd, validate_name, Target, MAX_SOCKET_PATH_BYTES};
