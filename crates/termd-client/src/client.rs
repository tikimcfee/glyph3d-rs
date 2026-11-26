//! Non-blocking event-stream + per-caller blocking RPC client for `g3d-termd`.
//!
//! Architecture:
//! - One Unix stream socket, split into a `Mutex<Option<UnixStream>>` write handle and a
//!   dedicated background reader thread.
//! - Callers from any thread allocate a `req` id, register a one-shot reply channel in a
//!   lock-only map (never held across socket I/O, preventing bidirectional buffer deadlocks),
//!   write the request frame, and block only themselves on `recv_timeout`.
//! - The reader thread decodes every incoming frame in wire order: `REPLY` frames wake their
//!   waiting caller; `OUTPUT`, `REPAINT` and `EVENT` frames go onto the `mpsc` event channel
//!   in exact arrival order, invoking `wake` after each push.
//! - On `lagged`, the reader forwards [`Event::Lagged`] and triggers a fire-and-forget
//!   `repaint` request without waiting on its reply.
//! - On `ERROR restart` or an unexpected socket drop, the reader wakes any in-flight callers
//!   from the dead connection (`input` and requests are at-most-once), reconnects within the
//!   reconnect window verifying `WELCOME.instance`, re-subscribes active terminals and asks
//!   each `raw` subscription for a repaint, then notifies the host with [`Event::Reconnected`]
//!   (or [`Event::Disconnected`] when the namespace is gone).

use crate::frame::{self, ty, Frame, FrameDecoder};
use crate::msg::*;
use crate::paths::{self, Target};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Default timeout for a single request/reply round trip.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Default window for riding out a `g3d-termd` brain restart before declaring the daemon lost.
pub const DEFAULT_RECONNECT_WINDOW: Duration = Duration::from_secs(30);

/// Maximum `paste` text chunk size under normal 16 MiB frames (1 MiB, matching `g3d paste`).
const MAX_PASTE_CHUNK_CAP: usize = 1 << 20;

/// Client error returned by connection setup and RPC operations.
#[derive(Debug)]
pub enum Error {
    /// Socket or OS-level I/O failure.
    Io(io::Error),
    /// Structured protocol refusal (`REPLY` with `ok: false` or connection `ERROR` frame).
    Wire(WireError),
    /// Malformed wire frame or unexpected JSON payload from the daemon.
    Protocol(String),
    /// Request did not receive its `REPLY` within `timeout`.
    Timeout { op: &'static str, timeout: Duration },
    /// Connection dropped (mid-request before reconnect, or permanently ended).
    Disconnected { restart: bool, message: String },
    /// Invalid target, socket path over 100 bytes, or `g3d-termd` startup failure.
    Start(String),
}

impl Error {
    /// Wire error code when the refusal came from `g3d-termd`.
    pub fn code(&self) -> Option<&str> {
        match self {
            Error::Wire(err) => Some(err.code.as_str()),
            Error::Disconnected { restart: true, .. } => Some(code::RESTART),
            _ => None,
        }
    }

    /// Reports whether this error carries the given wire error `code`.
    pub fn is_code(&self, code: &str) -> bool {
        self.code() == Some(code)
    }

    /// Reports whether nothing is listening on the target socket (`ENOENT` or `ECONNREFUSED`).
    pub fn is_not_running(&self) -> bool {
        matches!(
            self,
            Error::Io(err)
                if matches!(err.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused)
        )
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(err) => write!(f, "{err}"),
            Error::Wire(err) => write!(f, "{err}"),
            Error::Protocol(msg) => write!(f, "protocol: {msg}"),
            Error::Timeout { op, timeout } => write!(f, "termd `{op}` timed out after {timeout:?}"),
            Error::Disconnected { restart: true, message } => {
                write!(f, "termd restarting: {message}")
            }
            Error::Disconnected { restart: false, message } => {
                write!(f, "termd disconnected: {message}")
            }
            Error::Start(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}

/// Asynchronous stream item pushed by `g3d-termd` (everything that is not a `REPLY`), in wire
/// arrival order.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Raw PTY bytes starting at byte cursor `seq`.
    Output { handle: u32, seq: u64, bytes: Vec<u8> },
    /// Full VT screen redraw up to `seq` (`view` is true when showing scrollback).
    Repaint { handle: u32, seq: u64, cols: u16, rows: u16, view: bool, bytes: Vec<u8> },
    /// Owner resized the terminal; output from `seq` onward was produced at `(cols, rows)`.
    Size { handle: u32, cols: u16, rows: u16, seq: u64 },
    /// Program changed its window title (`OSC 0` / `OSC 2`), or initial title on subscribe.
    Title { handle: u32, title: String },
    /// Terminal input/screen mode flags changed, or initial modes on subscribe.
    Modes {
        handle: u32,
        alt: bool,
        mouse: bool,
        mouse_sgr: bool,
        bracketed_paste: bool,
        app_cursor: bool,
        focus: bool,
    },
    /// Program emitted `BEL`.
    Bell { handle: u32 },
    /// Program reported its working directory (`OSC 7`), or initial directory on subscribe.
    Pwd { handle: u32, pwd: String },
    /// Subscription's role changed (e.g. another client took size ownership).
    Role { handle: u32, role: Role },
    /// Lossy subscriber fell behind by `dropped` bytes and is resyncing at `seq` (the client
    /// also requests a fresh repaint automatically).
    Lagged { handle: u32, dropped: u64, seq: u64 },
    /// Terminal's program exited at output position `seq`.
    Exit { handle: u32, code: Option<i32>, signal: Option<String>, seq: u64 },
    /// Terminal has been torn down; `handle` will produce no further frames.
    Closed { handle: u32 },
    /// Asynchronous terminal-scoped error (e.g. `read_only`, `input_full`, `panic`).
    TermError { handle: u32, code: String, message: String },
    /// Pushed daemon status document from `status_follow`.
    Status(serde_json::Value),
    /// Daemon upgrade requested on this connection failed; the existing brain continues.
    UpgradeFailed { message: String },
    /// Client rode a daemon restart (`ERROR restart` when `restart` is true, or an unexpected
    /// drop), re-subscribed active terminals, and requested repaints.
    Reconnected { restart: bool, generation: Option<u64> },
    /// Connection ended permanently (namespace stopped, instance changed, or reconnect timed out).
    Disconnected { restart: bool, message: String },
}

/// Optional tuning for connection startup, timeouts and reconnect window.
#[derive(Clone, Debug)]
pub struct ConnectOptions {
    pub termd_bin: Option<PathBuf>,
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
    pub reconnect_window: Duration,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            termd_bin: None,
            startup_timeout: Duration::from_secs(5),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            reconnect_window: DEFAULT_RECONNECT_WINDOW,
        }
    }
}

#[derive(Clone, Debug)]
struct TrackedSub {
    id: String,
    handle: u32,
    mode: Mode,
    role: Role,
    history_rows: u32,
    flow: Option<Flow>,
    size: Option<(u16, u16)>,
}

struct ConnState {
    pending: HashMap<u64, mpsc::SyncSender<Result<Vec<u8>, Error>>>,
    reconnecting: bool,
    fatal: Option<(bool, String)>,
}

struct SubTable {
    by_handle: BTreeMap<u32, TrackedSub>,
    status_follow: Option<Option<u64>>,
}

struct Shared {
    writer: Mutex<Option<UnixStream>>,
    reader_ctl: Mutex<Option<UnixStream>>,
    welcome: Mutex<Welcome>,
    max_frame: AtomicU64,
    next_req: AtomicU64,
    conn: Mutex<ConnState>,
    conn_cv: Condvar,
    subs: Mutex<SubTable>,
    closed_by_client: AtomicBool,
    request_timeout_ms: AtomicU64,
    reconnect_window_ms: AtomicU64,
}

impl Shared {
    fn request_timeout(&self) -> Duration {
        Duration::from_millis(self.request_timeout_ms.load(Ordering::Relaxed).max(1))
    }

    fn reconnect_window(&self) -> Duration {
        Duration::from_millis(self.reconnect_window_ms.load(Ordering::Relaxed))
    }

    fn max_frame_bytes(&self) -> usize {
        let val = self.max_frame.load(Ordering::Relaxed) as usize;
        if val > 0 { val } else { frame::DEFAULT_MAX_FRAME }
    }
}

/// Thread-safe (`Clone + Send + Sync`) request handle onto a [`Termd`] connection.
///
/// Any number of threads may clone a `TermdHandle` via [`Termd::handle`] and issue concurrent
/// requests; each call blocks only its own thread until its matching `REPLY` arrives or times out.
#[derive(Clone)]
pub struct TermdHandle {
    shared: Arc<Shared>,
}

impl TermdHandle {
    /// Latest `WELCOME` handshake received from the daemon (updated across reconnects).
    pub fn welcome(&self) -> Welcome {
        self.shared.welcome.lock().expect("welcome mutex poisoned").clone()
    }

    /// Adjusts the per-request reply timeout for subsequent calls.
    pub fn set_request_timeout(&self, timeout: Duration) {
        let millis = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX).max(1);
        self.shared.request_timeout_ms.store(millis, Ordering::Relaxed);
    }

    /// Adjusts how long the reader thread tries to reconnect after a brain restart or drop.
    pub fn set_reconnect_window(&self, window: Duration) {
        let millis = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
        self.shared.reconnect_window_ms.store(millis, Ordering::Relaxed);
    }

    /// Lists all live terminals in the connected namespace.
    pub fn list(&self) -> Result<Vec<TermInfo>, Error> {
        let reply: ListReply = self.request(WireOp::List)?;
        Ok(reply.terminals)
    }

    /// Starts (or adopts, with [`IfExists::Adopt`]) a terminal.
    pub fn create(&self, opts: &Create) -> Result<CreateReply, Error> {
        paths::validate_name("terminal id", &opts.id).map_err(Error::Start)?;
        self.request(WireOp::Create(opts))
    }

    /// Subscribes to terminal `id` in `raw` mode with `role`.
    ///
    /// When `role` is [`Role::Own`] and `size` is `Some((cols, rows))`, resizes the terminal as
    /// part of taking ownership. Active subscriptions are remembered and restored automatically
    /// across daemon restarts.
    pub fn subscribe(
        &self,
        id: &str,
        role: Role,
        size: Option<(u16, u16)>,
        history_rows: u32,
    ) -> Result<Subscription, Error> {
        let opts = Subscribe {
            id: id.to_owned(),
            mode: Mode::Raw,
            role,
            history_rows: (history_rows > 0).then_some(history_rows),
            flow: None,
            cols: size.map(|pair| pair.0),
            rows: size.map(|pair| pair.1),
        };
        self.subscribe_with(&opts)
    }

    /// Subscribes with explicit [`Mode`] and [`Flow`] options.
    pub fn subscribe_with(&self, opts: &Subscribe) -> Result<Subscription, Error> {
        paths::validate_name("terminal id", &opts.id).map_err(Error::Start)?;
        let sub: Subscription = self.request(WireOp::Subscribe(opts))?;
        let recorded_size = if sub.role == Role::Own {
            Some((sub.cols, sub.rows))
        } else {
            None
        };
        let mut table = self.shared.subs.lock().expect("subs mutex poisoned");
        table.by_handle.insert(
            sub.handle,
            TrackedSub {
                id: opts.id.clone(),
                handle: sub.handle,
                mode: opts.mode,
                role: sub.role,
                history_rows: opts.history_rows.unwrap_or(0),
                flow: opts.flow,
                size: recorded_size,
            },
        );
        Ok(sub)
    }

    /// Ends this connection's subscription on `handle`.
    pub fn unsubscribe(&self, handle: u32) -> Result<(), Error> {
        let _: EmptyReply = self.request(WireOp::Unsubscribe { handle })?;
        let mut table = self.shared.subs.lock().expect("subs mutex poisoned");
        table.by_handle.remove(&handle);
        Ok(())
    }

    /// Promotes an `attach` subscription on `handle` to `own`, optionally resizing in one step.
    pub fn take(&self, handle: u32, size: Option<(u16, u16)>) -> Result<Role, Error> {
        let reply: TakeReply = self.request(WireOp::Take {
            handle,
            cols: size.map(|pair| pair.0),
            rows: size.map(|pair| pair.1),
        })?;
        let mut table = self.shared.subs.lock().expect("subs mutex poisoned");
        if let Some(tracked) = table.by_handle.get_mut(&handle) {
            tracked.role = reply.role;
            if let Some(dims) = size {
                tracked.size = Some(dims);
            }
        }
        Ok(reply.role)
    }

    /// Resizes `handle` (requires [`Role::Own`]).
    pub fn resize(&self, handle: u32, cols: u16, rows: u16) -> Result<(), Error> {
        let _: EmptyReply = self.request(WireOp::Resize { handle, cols, rows })?;
        let mut table = self.shared.subs.lock().expect("subs mutex poisoned");
        if let Some(tracked) = table.by_handle.get_mut(&handle) {
            if tracked.role == Role::Own {
                tracked.size = Some((cols, rows));
            }
        }
        Ok(())
    }

    /// Requests a fresh `REPAINT` frame on `handle` (`mode: "raw"` subscriptions only).
    pub fn repaint(&self, handle: u32) -> Result<(), Error> {
        let _: EmptyReply = self.request(WireOp::Repaint { handle, history_rows: None })?;
        Ok(())
    }

    /// Scrolls this connection's view into scrollback or returns to the live screen.
    pub fn view(&self, handle: u32, view: View) -> Result<ViewReply, Error> {
        let (delta, live) = match view {
            View::Delta(rows) => (Some(rows), None),
            View::Live => (None, Some(true)),
        };
        self.request(WireOp::View { handle, delta, live })
    }

    /// Sends one W3C key event for `g3d-termd` to encode against the terminal's live modes.
    pub fn key(&self, handle: u32, key: &Key) -> Result<(), Error> {
        let _: WrittenReply = self.request(WireOp::Key { handle, key })?;
        Ok(())
    }

    /// Pastes `text` into `handle` (`allow_unsafe: true`, matching the browser wall's user-paste
    /// policy), streaming in chunks with `more: true` when `text` exceeds a single frame's budget.
    pub fn paste(&self, handle: u32, text: &str) -> Result<(), Error> {
        self.paste_checked(handle, text, true)
    }

    /// Pastes `text` into `handle`, letting the caller choose `allow_unsafe` (`false` refuses
    /// pastes containing unbracketed newlines or `ESC [ 201 ~` with `code::UNSAFE_PASTE`).
    pub fn paste_checked(&self, handle: u32, text: &str, allow_unsafe: bool) -> Result<(), Error> {
        let welcome = self.welcome();
        let max_frame = self.shared.max_frame_bytes();
        // A JSON control character escapes as 6 bytes (`\u0003`); leave 1 KiB headroom for the
        // JSON envelope so no chunk can ever exceed `max_frame`.
        let max_chunk = (max_frame.saturating_sub(1024) / 6).clamp(4, MAX_PASTE_CHUNK_CAP);

        if text.len() <= max_chunk || !welcome.has_feature("paste_stream") {
            let _: WrittenReply = self.request(WireOp::Paste {
                handle,
                text,
                allow_unsafe: allow_unsafe.then_some(true),
                more: None,
            })?;
            return Ok(());
        }

        let mut offset = 0usize;
        while offset < text.len() {
            let remaining = &text[offset..];
            let cut = floor_char_boundary(remaining, max_chunk);
            let chunk = &remaining[..cut];
            offset += cut;
            let has_more = offset < text.len();
            let allow = if offset == cut && allow_unsafe { Some(true) } else { None };
            let result: Result<WrittenReply, Error> = self.request(WireOp::Paste {
                handle,
                text: chunk,
                allow_unsafe: allow,
                more: has_more.then_some(true),
            });
            if let Err(err) = result {
                // PROTOCOL.md 1.5 ("paste"): when an open streamed paste is refused early while
                // `more: true` was set, a final chunk without `more` closes the refusal state.
                if has_more && matches!(err, Error::Wire(_)) {
                    let _: Result<WrittenReply, Error> = self.request(WireOp::Paste {
                        handle,
                        text: "",
                        allow_unsafe: None,
                        more: None,
                    });
                }
                return Err(err);
            }
        }
        Ok(())
    }

    /// Sends one pointer event on `handle` (protocol 1.4).
    pub fn mouse(&self, handle: u32, ev: &Mouse) -> Result<(), Error> {
        let _: WrittenReply = self.request(WireOp::Mouse { handle, mouse: ev })?;
        Ok(())
    }

    /// Writes raw PTY input bytes to `handle` (`INPUT` data frames, chunked under `max_frame`).
    ///
    /// `INPUT` has no reply; subscription/buffer refusals arrive asynchronously as
    /// [`Event::TermError`].
    pub fn input(&self, handle: u32, bytes: &[u8]) -> Result<(), Error> {
        self.wait_until_writable()?;
        let max_payload = self.shared.max_frame_bytes().max(1);
        let mut writer_guard = self.shared.writer.lock().expect("writer mutex poisoned");
        let Some(stream) = writer_guard.as_mut() else {
            return Err(self.disconnected_err());
        };
        if bytes.is_empty() {
            return Ok(());
        }
        for chunk in bytes.chunks(max_payload) {
            let encoded = frame::encode(ty::INPUT, handle, chunk);
            stream.write_all(&encoded)?;
        }
        Ok(())
    }

    /// Kills terminal `id` by the daemon's process-tree close rule (`SIGHUP` -> `SIGTERM` -> `SIGKILL`).
    pub fn kill(&self, id: &str) -> Result<(), Error> {
        let _: EmptyReply = self.request(WireOp::Kill { id, signal: None })?;
        Ok(())
    }

    /// Kills terminal `id` starting with `signal` (`"HUP"`, `"TERM"`, or `"KILL"`).
    pub fn kill_with_signal(&self, id: &str, signal: &str) -> Result<(), Error> {
        let _: EmptyReply = self.request(WireOp::Kill { id, signal: Some(signal) })?;
        Ok(())
    }

    /// Fetches the daemon's status document once.
    pub fn status(&self) -> Result<serde_json::Value, Error> {
        let reply: StatusReply = self.request(WireOp::Status)?;
        Ok(reply.status)
    }

    /// Subscribes to pushed [`Event::Status`] documents, returning the clamped interval in ms.
    pub fn status_follow(&self, interval_ms: Option<u64>) -> Result<u64, Error> {
        let reply: StatusFollowReply = self.request(WireOp::StatusFollow { interval_ms })?;
        let mut table = self.shared.subs.lock().expect("subs mutex poisoned");
        table.status_follow = Some(interval_ms);
        Ok(reply.interval_ms)
    }

    /// Stops pushed [`Event::Status`] documents on this connection.
    pub fn status_unfollow(&self) -> Result<(), Error> {
        let _: EmptyReply = self.request(WireOp::StatusUnfollow)?;
        let mut table = self.shared.subs.lock().expect("subs mutex poisoned");
        table.status_follow = None;
        Ok(())
    }

    /// Liveness round trip (`ping`).
    pub fn ping(&self) -> Result<(), Error> {
        let _: EmptyReply = self.request(WireOp::Ping)?;
        Ok(())
    }

    /// Kills every terminal in the connected namespace and stops the daemon.
    pub fn stop(&self) -> Result<(), Error> {
        self.shared.closed_by_client.store(true, Ordering::Release);
        let _: EmptyReply = self.request(WireOp::Stop)?;
        Ok(())
    }

    /// Starts an in-place daemon brain upgrade (or holder + brain with `holder = true`),
    /// returning the generation of the brain that accepted the upgrade.
    pub fn upgrade(&self, binary: Option<&Path>, holder: bool) -> Result<u64, Error> {
        let bin_str = binary.map(|path| path.display().to_string());
        let reply: UpgradeReply = self.request(WireOp::Upgrade {
            binary: bin_str.as_deref(),
            holder: holder.then_some(true),
        })?;
        Ok(reply.generation)
    }

    fn disconnected_err(&self) -> Error {
        let state = self.shared.conn.lock().expect("conn mutex poisoned");
        if let Some((restart, ref msg)) = state.fatal {
            Error::Disconnected { restart, message: msg.clone() }
        } else {
            Error::Disconnected {
                restart: false,
                message: "connection closed".into(),
            }
        }
    }

    fn wait_until_writable(&self) -> Result<(), Error> {
        let timeout = self.shared.request_timeout();
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.conn.lock().expect("conn mutex poisoned");
        while state.reconnecting {
            if let Some((restart, ref msg)) = state.fatal {
                return Err(Error::Disconnected { restart, message: msg.clone() });
            }
            if self.shared.closed_by_client.load(Ordering::Acquire) {
                return Err(Error::Disconnected {
                    restart: false,
                    message: "client closed".into(),
                });
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(Error::Disconnected {
                    restart: true,
                    message: "timed out waiting for termd reconnect".into(),
                });
            };
            let (next_state, _) =
                self.shared.conn_cv.wait_timeout(state, remaining).expect("conn mutex poisoned");
            state = next_state;
        }
        if let Some((restart, ref msg)) = state.fatal {
            return Err(Error::Disconnected { restart, message: msg.clone() });
        }
        Ok(())
    }

    fn request<T: DeserializeOwned>(&self, op: WireOp<'_>) -> Result<T, Error> {
        let op_name = op.name();
        let timeout = self.shared.request_timeout();
        let start = Instant::now();
        self.wait_until_writable()?;

        let req = self.shared.next_req.fetch_add(1, Ordering::Relaxed);
        let wire_bytes = frame::encode_json(ty::REQUEST, 0, &WireRequest { req, op });
        let payload_len = wire_bytes.len() - frame::HEADER_LEN;
        let max_frame = self.shared.max_frame_bytes();
        if payload_len > max_frame {
            return Err(Error::Wire(WireError::new(
                code::TOO_LARGE,
                format!(
                    "the `{op_name}` request is {payload_len} B as JSON; daemon max_frame is {max_frame} B"
                ),
            )));
        }

        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        {
            let mut state = self.shared.conn.lock().expect("conn mutex poisoned");
            if let Some((restart, ref msg)) = state.fatal {
                return Err(Error::Disconnected { restart, message: msg.clone() });
            }
            state.pending.insert(req, reply_tx);
        }

        let write_result = {
            let mut writer_guard = self.shared.writer.lock().expect("writer mutex poisoned");
            match writer_guard.as_mut() {
                Some(stream) => stream.write_all(&wire_bytes),
                None => Err(io::Error::new(io::ErrorKind::NotConnected, "socket reconnecting")),
            }
        };

        if let Err(io_err) = write_result {
            let (removed, fatal) = {
                let mut state = self.shared.conn.lock().expect("conn mutex poisoned");
                (state.pending.remove(&req).is_some(), state.fatal.clone())
            };
            if !removed {
                if let Ok(disconn_res) = reply_rx.try_recv() {
                    let payload = disconn_res?;
                    return serde_json::from_slice(&payload)
                        .map_err(|err| Error::Protocol(format!("{op_name} reply: {err}")));
                }
            }
            if let Some((restart, msg)) = fatal {
                return Err(Error::Disconnected { restart, message: msg });
            }
            return Err(Error::Io(io_err));
        }

        let remaining = timeout.saturating_sub(start.elapsed()).max(Duration::from_millis(1));
        match reply_rx.recv_timeout(remaining) {
            Ok(Ok(payload)) => serde_json::from_slice(&payload)
                .map_err(|err| Error::Protocol(format!("{op_name} reply: {err}"))),
            Ok(Err(err)) => Err(err),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let mut state = self.shared.conn.lock().expect("conn mutex poisoned");
                state.pending.remove(&req);
                Err(Error::Timeout { op: op_name, timeout })
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(self.disconnected_err()),
        }
    }
}

/// Live client connection to `g3d-termd` owning the event receiver and reader thread.
///
/// `Termd` dereferences to [`TermdHandle`], so all request methods (`list`, `create`,
/// `subscribe`, `key`, `paste`, ...) can be called directly on `Termd`, while [`Termd::handle`]
/// yields a `Clone + Send + Sync` handle for other threads.
pub struct Termd {
    handle: TermdHandle,
    events_rx: mpsc::Receiver<Event>,
    reader_thread: Option<JoinHandle<()>>,
}

impl std::ops::Deref for Termd {
    type Target = TermdHandle;

    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

impl Drop for Termd {
    fn drop(&mut self) {
        self.handle.shared.closed_by_client.store(true, Ordering::Release);
        self.handle.shared.conn_cv.notify_all();
        if let Ok(mut guard) = self.handle.shared.writer.lock() {
            if let Some(stream) = guard.take() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
        if let Ok(mut guard) = self.handle.shared.reader_ctl.lock() {
            if let Some(stream) = guard.take() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
        if let Some(join) = self.reader_thread.take() {
            let _ = join.join();
        }
    }
}

impl Termd {
    /// Connects to `target`; if nothing listens, starts `g3d-termd` (`--ns <ns> --daemon` or
    /// `--socket <path> --daemon`) and retries until it answers.
    pub fn connect_or_start(
        target: &Target,
        client_name: &str,
        wake: Box<dyn Fn() + Send>,
    ) -> Result<Termd, Error> {
        Self::connect_or_start_with(target, client_name, ConnectOptions::default(), wake)
    }

    /// Connects to `target` with custom [`ConnectOptions`], starting `g3d-termd` if not running.
    pub fn connect_or_start_with(
        target: &Target,
        client_name: &str,
        options: ConnectOptions,
        wake: Box<dyn Fn() + Send>,
    ) -> Result<Termd, Error> {
        let socket_path = target.resolve_socket().map_err(Error::Start)?;
        match dial_once(&socket_path, client_name) {
            Ok((sock, decoder, welcome)) => {
                return Self::spawn_session(target.clone(), client_name, options, sock, decoder, welcome, wake);
            }
            Err(err) if err.is_not_running() => {}
            Err(err) => return Err(err),
        }

        let bin = match options.termd_bin.as_deref() {
            Some(explicit) => {
                if !explicit.is_file() {
                    return Err(Error::Start(format!(
                        "g3d-termd binary does not exist at {}",
                        explicit.display()
                    )));
                }
                explicit.to_path_buf()
            }
            None => paths::find_termd().map_err(Error::Start)?,
        };

        let mut cmd = Command::new(&bin);
        cmd.args(target.daemon_args());
        scrub_spawn_env(&mut cmd);
        if let Some(dir) = target.explicit_termd_dir() {
            cmd.env("G3D_TERMD_DIR", dir);
        }
        let output = cmd
            .output()
            .map_err(|err| Error::Start(format!("failed to execute {}: {err}", bin.display())))?;
        let stderr_msg = String::from_utf8_lossy(&output.stderr).trim().to_owned();

        let deadline = Instant::now() + options.startup_timeout;
        let mut sleep_dur = Duration::from_millis(20);
        loop {
            match dial_once(&socket_path, client_name) {
                Ok((sock, decoder, welcome)) => {
                    return Self::spawn_session(
                        target.clone(),
                        client_name,
                        options,
                        sock,
                        decoder,
                        welcome,
                        wake,
                    );
                }
                Err(err) if err.is_not_running() && Instant::now() < deadline => {
                    thread::sleep(sleep_dur);
                    sleep_dur = (sleep_dur * 2).min(Duration::from_millis(200));
                }
                Err(err) => {
                    let log = target.log_path();
                    let log_tail = std::fs::read_to_string(&log)
                        .map(|contents| {
                            let mut lines: Vec<&str> = contents.lines().rev().take(10).collect();
                            lines.reverse();
                            lines.join("\n")
                        })
                        .unwrap_or_default();
                    let stderr_note = if stderr_msg.is_empty() {
                        String::new()
                    } else {
                        format!(" ({stderr_msg})")
                    };
                    return Err(Error::Start(format!(
                        "termd did not come up at {} ({err}); {} exited {}{stderr_note}; log {}:\n{log_tail}",
                        socket_path.display(),
                        bin.display(),
                        output.status,
                        log.display(),
                    )));
                }
            }
        }
    }

    /// Connects to an already-running daemon at `target` without starting one.
    pub fn connect(
        target: &Target,
        client_name: &str,
        wake: Box<dyn Fn() + Send>,
    ) -> Result<Termd, Error> {
        Self::connect_with(target, client_name, ConnectOptions::default(), wake)
    }

    /// Connects to an already-running daemon at `target` with custom [`ConnectOptions`].
    pub fn connect_with(
        target: &Target,
        client_name: &str,
        options: ConnectOptions,
        wake: Box<dyn Fn() + Send>,
    ) -> Result<Termd, Error> {
        let socket_path = target.resolve_socket().map_err(Error::Start)?;
        let (sock, decoder, welcome) = dial_once(&socket_path, client_name)?;
        Self::spawn_session(target.clone(), client_name, options, sock, decoder, welcome, wake)
    }

    /// Returns a `Clone + Send + Sync` handle for issuing requests from other threads.
    pub fn handle(&self) -> TermdHandle {
        self.handle.clone()
    }

    /// Ordered stream of everything `g3d-termd` sends that is not a `REPLY`.
    pub fn events(&self) -> &mpsc::Receiver<Event> {
        &self.events_rx
    }

    fn spawn_session(
        target: Target,
        client_name: &str,
        options: ConnectOptions,
        reader_sock: UnixStream,
        decoder: FrameDecoder,
        welcome: Welcome,
        wake: Box<dyn Fn() + Send>,
    ) -> Result<Termd, Error> {
        let writer_sock = reader_sock.try_clone()?;
        let ctl_sock = reader_sock.try_clone()?;
        let max_frame = welcome.max_frame;
        let req_ms = u64::try_from(options.request_timeout.as_millis()).unwrap_or(u64::MAX).max(1);
        let reconn_ms = u64::try_from(options.reconnect_window.as_millis()).unwrap_or(u64::MAX);

        let shared = Arc::new(Shared {
            writer: Mutex::new(Some(writer_sock)),
            reader_ctl: Mutex::new(Some(ctl_sock)),
            welcome: Mutex::new(welcome),
            max_frame: AtomicU64::new(max_frame),
            next_req: AtomicU64::new(1),
            conn: Mutex::new(ConnState {
                pending: HashMap::new(),
                reconnecting: false,
                fatal: None,
            }),
            conn_cv: Condvar::new(),
            subs: Mutex::new(SubTable { by_handle: BTreeMap::new(), status_follow: None }),
            closed_by_client: AtomicBool::new(false),
            request_timeout_ms: AtomicU64::new(req_ms),
            reconnect_window_ms: AtomicU64::new(reconn_ms),
        });

        let (events_tx, events_rx) = mpsc::channel();
        let reader_shared = Arc::clone(&shared);
        let client_label = client_name.to_owned();
        let reader_thread = thread::Builder::new()
            .name("termd-reader".into())
            .spawn(move || {
                reader_loop(
                    reader_shared,
                    target,
                    client_label,
                    reader_sock,
                    decoder,
                    events_tx,
                    wake,
                );
            })
            .map_err(Error::Io)?;

        Ok(Termd { handle: TermdHandle { shared }, events_rx, reader_thread: Some(reader_thread) })
    }
}

/// Strips inherited per-terminal/relay variables so a spawned `g3d-termd` starts cleanly.
fn scrub_spawn_env(cmd: &mut Command) {
    cmd.env_remove("G3D_TERMD_SOCKET");
    cmd.env_remove("G3D_TERMD_ID");
    for (key, _) in std::env::vars_os() {
        if let Some(name) = key.to_str() {
            if name.starts_with("G3D_RELAY_") {
                cmd.env_remove(name);
            }
        }
    }
}

/// Connects to `socket_path`, verifies directory ownership/mode and kernel peer UID, and completes
/// the `HELLO` / `WELCOME` handshake.
fn dial_once(socket_path: &Path, client_name: &str) -> Result<(UnixStream, FrameDecoder, Welcome), Error> {
    let uid = paths::current_uid();
    paths::check_socket_parent(socket_path, uid)?;
    let mut sock = UnixStream::connect(socket_path)?;
    if let Err(err) = paths::verify_peer_uid(&sock, uid) {
        let _ = sock.shutdown(Shutdown::Both);
        return Err(Error::Io(err));
    }

    let handshake_limit = Some(Duration::from_secs(5));
    let _ = sock.set_read_timeout(handshake_limit);
    let _ = sock.set_write_timeout(handshake_limit);

    let hello = Hello { proto: PROTO, client: client_name.to_owned() };
    // PROTOCOL.md ("Handshake"): a holder whose brain is failed answers `ERROR brain_failed`
    // immediately upon accept before reading `HELLO`; read the waiting frame even if `write_all`
    // fails so the structured `WireError` surfaces instead of `EPIPE`.
    let write_res = sock.write_all(&frame::encode_json(ty::HELLO, 0, &hello));
    let mut decoder = FrameDecoder::default();
    let read_res = read_next_frame(&mut sock, &mut decoder);

    let _ = sock.set_read_timeout(None);
    let _ = sock.set_write_timeout(None);

    let first_frame = match (read_res, write_res) {
        (Ok(frame), Ok(())) => frame,
        (Err(err @ Error::Wire(_)), _) => return Err(err),
        (Err(err), Ok(())) => return Err(err),
        (_, Err(write_err)) => return Err(Error::Io(write_err)),
    };

    if first_frame.ty != ty::WELCOME {
        return Err(Error::Protocol(format!(
            "expected WELCOME (0x81), got frame type {:#04x}",
            first_frame.ty
        )));
    }
    let welcome: Welcome = serde_json::from_slice(&first_frame.payload)
        .map_err(|err| Error::Protocol(format!("WELCOME JSON: {err}")))?;
    if welcome.proto[0] != PROTO[0] {
        return Err(Error::Protocol(format!(
            "unsupported daemon protocol major {}.{}",
            welcome.proto[0], welcome.proto[1]
        )));
    }
    if welcome.max_frame > 0 {
        decoder.set_max_frame(welcome.max_frame as usize);
    }
    Ok((sock, decoder, welcome))
}

fn read_next_frame(sock: &mut UnixStream, decoder: &mut FrameDecoder) -> Result<Frame, Error> {
    let mut scratch = [0u8; 64 * 1024];
    loop {
        match decoder.next_frame() {
            Ok(Some(frame)) => {
                if frame.ty == ty::ERROR {
                    let wire_err: WireError = serde_json::from_slice(&frame.payload)
                        .map_err(|err| Error::Protocol(format!("ERROR JSON: {err}")))?;
                    return Err(Error::Wire(wire_err));
                }
                return Ok(frame);
            }
            Ok(None) => {}
            Err(err) => return Err(Error::Protocol(err.to_string())),
        }
        match sock.read(&mut scratch) {
            Ok(0) => {
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "daemon closed the connection",
                )));
            }
            Ok(count) => decoder.feed(&scratch[..count]),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(Error::Io(err)),
        }
    }
}

fn push_event(tx: &mpsc::Sender<Event>, wake: &(dyn Fn() + Send), event: Event) -> bool {
    if tx.send(event).is_err() {
        return false;
    }
    wake();
    true
}

/// Sends a fire-and-forget request (used for `lagged` auto-repaint) without ever stalling the
/// reader thread if another thread is mid-write on `shared.writer`.
fn send_fire_and_forget_repaint(shared: &Arc<Shared>, handle: u32) {
    let req = shared.next_req.fetch_add(1, Ordering::Relaxed);
    let wire_bytes = frame::encode_json(
        ty::REQUEST,
        0,
        &WireRequest { req, op: WireOp::Repaint { handle, history_rows: None } },
    );
    if let Ok(mut guard) = shared.writer.try_lock() {
        if let Some(stream) = guard.as_mut() {
            let _ = stream.write_all(&wire_bytes);
        }
        return;
    }
    let shared_clone = Arc::clone(shared);
    let _ = thread::Builder::new().name("termd-repaint".into()).spawn(move || {
        if let Ok(mut guard) = shared_clone.writer.lock() {
            if let Some(stream) = guard.as_mut() {
                let _ = stream.write_all(&wire_bytes);
            }
        }
    });
}

fn reader_loop(
    shared: Arc<Shared>,
    target: Target,
    client_name: String,
    mut sock: UnixStream,
    mut decoder: FrameDecoder,
    events_tx: mpsc::Sender<Event>,
    wake: Box<dyn Fn() + Send>,
) {
    let mut buffered: VecDeque<Frame> = VecDeque::new();

    loop {
        let frame_res = if let Some(queued) = buffered.pop_front() {
            Ok(queued)
        } else {
            read_next_frame(&mut sock, &mut decoder)
        };

        let frame = match frame_res {
            Ok(frame) => frame,
            Err(drop_err) => {
                if shared.closed_by_client.load(Ordering::Acquire) {
                    fail_all_pending(&shared, false, "client closed", true);
                    return;
                }

                let (planned, recoverable, reason) = classify_drop(&drop_err);
                // Take down the dead writer and wake any callers whose requests were sent on the
                // old connection (PROTOCOL.md: input and mutations are at-most-once).
                if let Ok(mut writer_guard) = shared.writer.lock() {
                    *writer_guard = None;
                }
                if !recoverable {
                    fail_all_pending(&shared, planned, &reason, true);
                    let _ = push_event(
                        &events_tx,
                        wake.as_ref(),
                        Event::Disconnected { restart: planned, message: reason },
                    );
                    return;
                }

                begin_reconnecting(&shared, planned, &reason);
                let prev_instance =
                    shared.welcome.lock().expect("welcome mutex poisoned").instance.clone();

                match reconnect_and_restore(
                    &shared,
                    &target,
                    &client_name,
                    planned,
                    prev_instance.as_deref(),
                ) {
                    Ok(Some((new_sock, new_decoder, generation, closed_handles, early_frames))) => {
                        sock = new_sock;
                        decoder = new_decoder;
                        buffered = early_frames;
                        if !push_event(
                            &events_tx,
                            wake.as_ref(),
                            Event::Reconnected { restart: planned, generation },
                        ) {
                            return;
                        }
                        for dead_handle in closed_handles {
                            if !push_event(
                                &events_tx,
                                wake.as_ref(),
                                Event::Closed { handle: dead_handle },
                            ) {
                                return;
                            }
                        }
                        continue;
                    }
                    Ok(None) => {
                        // Client was dropped while sleeping in reconnect backoff.
                        fail_all_pending(&shared, false, "client closed", true);
                        return;
                    }
                    Err(fatal_msg) => {
                        fail_all_pending(&shared, planned, &fatal_msg, true);
                        let _ = push_event(
                            &events_tx,
                            wake.as_ref(),
                            Event::Disconnected { restart: planned, message: fatal_msg },
                        );
                        return;
                    }
                }
            }
        };

        if !route_frame(&shared, frame, &events_tx, wake.as_ref()) {
            return;
        }
    }
}

fn classify_drop(err: &Error) -> (bool, bool, String) {
    match err {
        Error::Wire(wire) => match wire.code.as_str() {
            code::RESTART => (true, true, wire.message.clone()),
            code::BRAIN_FAILED => (false, true, wire.message.clone()),
            _ => (false, false, wire.to_string()),
        },
        Error::Io(io_err) => (false, true, io_err.to_string()),
        Error::Protocol(msg) => (false, false, format!("protocol: {msg}")),
        other => (false, false, other.to_string()),
    }
}

fn begin_reconnecting(shared: &Shared, restart: bool, message: &str) {
    let mut state = shared.conn.lock().expect("conn mutex poisoned");
    state.reconnecting = true;
    for (_, reply_tx) in state.pending.drain() {
        let _ = reply_tx.send(Err(Error::Disconnected {
            restart,
            message: message.to_owned(),
        }));
    }
}

fn fail_all_pending(shared: &Shared, restart: bool, message: &str, permanent: bool) {
    let mut state = shared.conn.lock().expect("conn mutex poisoned");
    state.reconnecting = false;
    if permanent {
        state.fatal = Some((restart, message.to_owned()));
    }
    for (_, reply_tx) in state.pending.drain() {
        let _ = reply_tx.send(Err(Error::Disconnected {
            restart,
            message: message.to_owned(),
        }));
    }
    shared.conn_cv.notify_all();
}

type RestoreOutcome = (UnixStream, FrameDecoder, Option<u64>, Vec<u32>, VecDeque<Frame>);

fn reconnect_and_restore(
    shared: &Arc<Shared>,
    target: &Target,
    client_name: &str,
    planned: bool,
    prev_instance: Option<&str>,
) -> Result<Option<RestoreOutcome>, String> {
    let socket_path = target.resolve_socket()?;
    let window = shared.reconnect_window();
    let deadline = Instant::now() + window;

    let jitter_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |dur| u64::from(dur.subsec_nanos()) % 80_000_000)
        + 10_000_000;
    let mut wait = if planned {
        Duration::from_nanos(jitter_ns)
    } else {
        Duration::from_millis(100)
    };

    let (mut new_sock, mut new_decoder, welcome) = loop {
        if shared.closed_by_client.load(Ordering::Acquire) {
            return Ok(None);
        }
        {
            let state = shared.conn.lock().expect("conn mutex poisoned");
            if shared.closed_by_client.load(Ordering::Acquire) {
                return Ok(None);
            }
            let (guard, _) =
                shared.conn_cv.wait_timeout(state, wait).expect("conn mutex poisoned");
            drop(guard);
        }
        if shared.closed_by_client.load(Ordering::Acquire) {
            return Ok(None);
        }

        match dial_once(&socket_path, client_name) {
            Ok((sock, dec, welcome)) => {
                if let Some(expected) = prev_instance {
                    if welcome.instance.as_deref() != Some(expected) {
                        let _ = sock.shutdown(Shutdown::Both);
                        return Err(
                            "a different termd namespace instance answered: previous terminals are gone"
                                .into(),
                        );
                    }
                }
                break (sock, dec, welcome);
            }
            Err(err) if err.is_not_running() => {
                return Err("nothing listens any more: the namespace and its terminals are gone".into());
            }
            Err(Error::Io(ref io_err)) if io_err.kind() == io::ErrorKind::PermissionDenied => {
                return Err(format!("unsafe socket on reconnect: {io_err}"));
            }
            Err(err) => {
                if Instant::now() >= deadline {
                    return Err(format!("no termd brain answered within {window:?}: {err}"));
                }
            }
        }
        wait = (wait * 2).clamp(Duration::from_millis(100), Duration::from_secs(2));
    };

    // Re-establish subscriptions and ask each raw subscription for a repaint before publishing
    // the new writer socket, buffering any interleaved frames in arrival order.
    let _ = new_sock.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = new_sock.set_write_timeout(Some(Duration::from_secs(10)));

    let (subs_snapshot, status_follow) = {
        let table = shared.subs.lock().expect("subs mutex poisoned");
        (table.by_handle.values().cloned().collect::<Vec<_>>(), table.status_follow)
    };

    let mut early_frames = VecDeque::new();
    let mut closed_handles = Vec::new();
    let mut updated_subs = BTreeMap::new();

    for mut tracked in subs_snapshot {
        let sub_req = Subscribe {
            id: tracked.id.clone(),
            mode: tracked.mode,
            role: tracked.role,
            history_rows: (tracked.history_rows > 0).then_some(tracked.history_rows),
            flow: tracked.flow,
            cols: if tracked.role == Role::Own { tracked.size.map(|s| s.0) } else { None },
            rows: if tracked.role == Role::Own { tracked.size.map(|s| s.1) } else { None },
        };
        match sync_request_on_socket::<Subscription>(
            shared,
            &mut new_sock,
            &mut new_decoder,
            &mut early_frames,
            WireOp::Subscribe(&sub_req),
        ) {
            Ok(reply) => {
                tracked.handle = reply.handle;
                tracked.role = reply.role;
                if reply.role == Role::Own {
                    tracked.size = Some((reply.cols, reply.rows));
                } else {
                    tracked.size = None;
                }
                if tracked.mode == Mode::Raw {
                    let _: Result<EmptyReply, Error> = sync_request_on_socket(
                        shared,
                        &mut new_sock,
                        &mut new_decoder,
                        &mut early_frames,
                        WireOp::Repaint {
                            handle: reply.handle,
                            history_rows: (tracked.history_rows > 0)
                                .then_some(tracked.history_rows),
                        },
                    );
                }
                updated_subs.insert(reply.handle, tracked);
            }
            Err(err) if err.is_code(code::NO_TERMINAL) => {
                closed_handles.push(tracked.handle);
            }
            Err(err) => {
                let _ = new_sock.shutdown(Shutdown::Both);
                return Err(format!("re-subscribe `{}` failed: {err}", tracked.id));
            }
        }
    }

    if let Some(interval_ms) = status_follow {
        let _: Result<StatusFollowReply, Error> = sync_request_on_socket(
            shared,
            &mut new_sock,
            &mut new_decoder,
            &mut early_frames,
            WireOp::StatusFollow { interval_ms },
        );
    }

    let _ = new_sock.set_read_timeout(None);
    let _ = new_sock.set_write_timeout(None);

    let writer_clone = new_sock.try_clone().map_err(|err| err.to_string())?;
    let ctl_clone = new_sock.try_clone().map_err(|err| err.to_string())?;
    let generation = welcome.generation;
    let max_frame = welcome.max_frame;

    {
        let mut table = shared.subs.lock().expect("subs mutex poisoned");
        table.by_handle = updated_subs;
    }
    *shared.welcome.lock().expect("welcome mutex poisoned") = welcome;
    if max_frame > 0 {
        shared.max_frame.store(max_frame, Ordering::Relaxed);
    }
    *shared.reader_ctl.lock().expect("reader_ctl mutex poisoned") = Some(ctl_clone);
    *shared.writer.lock().expect("writer mutex poisoned") = Some(writer_clone);
    {
        let mut state = shared.conn.lock().expect("conn mutex poisoned");
        state.reconnecting = false;
        shared.conn_cv.notify_all();
    }

    Ok(Some((new_sock, new_decoder, generation, closed_handles, early_frames)))
}

fn sync_request_on_socket<T: DeserializeOwned>(
    shared: &Shared,
    sock: &mut UnixStream,
    decoder: &mut FrameDecoder,
    early_frames: &mut VecDeque<Frame>,
    op: WireOp<'_>,
) -> Result<T, Error> {
    let op_name = op.name();
    let req = shared.next_req.fetch_add(1, Ordering::Relaxed);
    let wire_bytes = frame::encode_json(ty::REQUEST, 0, &WireRequest { req, op });
    sock.write_all(&wire_bytes)?;
    loop {
        let frame = read_next_frame(sock, decoder)?;
        if frame.ty == ty::REPLY {
            let head: ReplyHead = serde_json::from_slice(&frame.payload)
                .map_err(|err| Error::Protocol(format!("REPLY JSON: {err}")))?;
            if head.req == req {
                if !head.ok {
                    let wire_err = head.error.unwrap_or_else(|| {
                        WireError::new("unknown", "reply ok=false without error object")
                    });
                    return Err(Error::Wire(wire_err));
                }
                return serde_json::from_slice(&frame.payload)
                    .map_err(|err| Error::Protocol(format!("{op_name} reply JSON: {err}")));
            }
        }
        early_frames.push_back(frame);
    }
}

/// Routes a single decoded frame; returns `false` only if the host dropped its [`Event`] receiver.
fn route_frame(
    shared: &Arc<Shared>,
    frame: Frame,
    events_tx: &mpsc::Sender<Event>,
    wake: &(dyn Fn() + Send),
) -> bool {
    match frame.ty {
        ty::REPLY => {
            if let Ok(head) = serde_json::from_slice::<ReplyHead>(&frame.payload) {
                let waiter = {
                    let mut state = shared.conn.lock().expect("conn mutex poisoned");
                    state.pending.remove(&head.req)
                };
                if let Some(reply_tx) = waiter {
                    let outcome = if head.ok {
                        Ok(frame.payload)
                    } else {
                        let wire_err = head.error.unwrap_or_else(|| {
                            WireError::new("unknown", "reply ok=false without error object")
                        });
                        Err(Error::Wire(wire_err))
                    };
                    let _ = reply_tx.send(outcome);
                }
            }
            true
        }
        ty::OUTPUT => {
            if let Some((seq, bytes)) = frame::parse_output(&frame.payload) {
                return push_event(
                    events_tx,
                    wake,
                    Event::Output { handle: frame.stream, seq, bytes: bytes.to_vec() },
                );
            }
            true
        }
        ty::REPAINT => {
            if let Some((head, bytes)) = frame::parse_repaint(&frame.payload) {
                let is_view = (head.flags & frame::REPAINT_FLAG_VIEW) != 0;
                return push_event(
                    events_tx,
                    wake,
                    Event::Repaint {
                        handle: frame.stream,
                        seq: head.seq,
                        cols: head.cols,
                        rows: head.rows,
                        view: is_view,
                        bytes: bytes.to_vec(),
                    },
                );
            }
            true
        }
        ty::EVENT => route_json_event(shared, frame.stream, &frame.payload, events_tx, wake),
        // PROTOCOL.md ("Frame types"): unknown frame types are skipped without error.
        _ => true,
    }
}

#[derive(Deserialize)]
struct EvTag {
    ev: String,
}

#[derive(Deserialize)]
struct SizeEv {
    cols: u16,
    rows: u16,
    seq: u64,
}

#[derive(Deserialize)]
struct TitleEv {
    title: String,
}

#[derive(Deserialize)]
struct ModesEv {
    #[serde(default)]
    alt: bool,
    #[serde(default)]
    mouse: bool,
    #[serde(default)]
    mouse_sgr: bool,
    #[serde(default)]
    bracketed_paste: bool,
    #[serde(default)]
    app_cursor: bool,
    #[serde(default)]
    focus: bool,
}

#[derive(Deserialize)]
struct PwdEv {
    pwd: String,
}

#[derive(Deserialize)]
struct RoleEv {
    role: Role,
}

#[derive(Deserialize)]
struct LaggedEv {
    dropped: u64,
    seq: u64,
}

#[derive(Deserialize)]
struct ExitEv {
    #[serde(default)]
    code: Option<i32>,
    #[serde(default)]
    signal: Option<String>,
    seq: u64,
}

#[derive(Deserialize)]
struct UpgradeFailedEv {
    message: String,
}

fn route_json_event(
    shared: &Arc<Shared>,
    handle: u32,
    payload: &[u8],
    events_tx: &mpsc::Sender<Event>,
    wake: &(dyn Fn() + Send),
) -> bool {
    let Ok(tag) = serde_json::from_slice::<EvTag>(payload) else {
        return true;
    };
    match tag.ev.as_str() {
        "size" => {
            if let Ok(ev) = serde_json::from_slice::<SizeEv>(payload) {
                return push_event(
                    events_tx,
                    wake,
                    Event::Size { handle, cols: ev.cols, rows: ev.rows, seq: ev.seq },
                );
            }
        }
        "title" => {
            if let Ok(ev) = serde_json::from_slice::<TitleEv>(payload) {
                return push_event(events_tx, wake, Event::Title { handle, title: ev.title });
            }
        }
        "modes" => {
            if let Ok(ev) = serde_json::from_slice::<ModesEv>(payload) {
                return push_event(
                    events_tx,
                    wake,
                    Event::Modes {
                        handle,
                        alt: ev.alt,
                        mouse: ev.mouse,
                        mouse_sgr: ev.mouse_sgr,
                        bracketed_paste: ev.bracketed_paste,
                        app_cursor: ev.app_cursor,
                        focus: ev.focus,
                    },
                );
            }
        }
        "bell" => return push_event(events_tx, wake, Event::Bell { handle }),
        "pwd" => {
            if let Ok(ev) = serde_json::from_slice::<PwdEv>(payload) {
                return push_event(events_tx, wake, Event::Pwd { handle, pwd: ev.pwd });
            }
        }
        "role" => {
            if let Ok(ev) = serde_json::from_slice::<RoleEv>(payload) {
                {
                    let mut table = shared.subs.lock().expect("subs mutex poisoned");
                    if let Some(tracked) = table.by_handle.get_mut(&handle) {
                        tracked.role = ev.role;
                        if ev.role != Role::Own {
                            tracked.size = None;
                        }
                    }
                }
                return push_event(events_tx, wake, Event::Role { handle, role: ev.role });
            }
        }
        "lagged" => {
            if let Ok(ev) = serde_json::from_slice::<LaggedEv>(payload) {
                let alive = push_event(
                    events_tx,
                    wake,
                    Event::Lagged { handle, dropped: ev.dropped, seq: ev.seq },
                );
                send_fire_and_forget_repaint(shared, handle);
                return alive;
            }
        }
        "exit" => {
            if let Ok(ev) = serde_json::from_slice::<ExitEv>(payload) {
                return push_event(
                    events_tx,
                    wake,
                    Event::Exit { handle, code: ev.code, signal: ev.signal, seq: ev.seq },
                );
            }
        }
        "closed" => {
            {
                let mut table = shared.subs.lock().expect("subs mutex poisoned");
                table.by_handle.remove(&handle);
            }
            return push_event(events_tx, wake, Event::Closed { handle });
        }
        "error" => {
            if let Ok(err) = serde_json::from_slice::<WireError>(payload) {
                return push_event(
                    events_tx,
                    wake,
                    Event::TermError { handle, code: err.code, message: err.message },
                );
            }
        }
        "status" => {
            if let Ok(doc) = serde_json::from_slice::<serde_json::Value>(payload) {
                return push_event(events_tx, wake, Event::Status(doc));
            }
        }
        "upgrade_failed" => {
            if let Ok(ev) = serde_json::from_slice::<UpgradeFailedEv>(payload) {
                return push_event(events_tx, wake, Event::UpgradeFailed { message: ev.message });
            }
        }
        // Unknown events from future 1.x minors are ignored per PROTOCOL.md ("Events").
        _ => {}
    }
    true
}

fn floor_char_boundary(text: &str, max_bytes: usize) -> usize {
    if max_bytes >= text.len() {
        return text.len();
    }
    let mut index = max_bytes;
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    if index == 0 {
        // Every UTF-8 scalar is at most 4 bytes; `max_chunk` is clamped >= 4, so this fallback
        // only guards callers passing a sub-character bound in unit tests.
        text.chars().next().map_or(0, char::len_utf8)
    } else {
        index
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::AtomicUsize;

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(tag: &str) -> Self {
            let path = PathBuf::from(format!("/tmp/g3d-ut-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&path).unwrap();
            Self(path)
        }

        fn socket(&self) -> PathBuf {
            self.0.join("termd-mock.sock")
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mock_welcome(instance: &str, generation: u64, max_frame: u64) -> Welcome {
        Welcome {
            proto: [1, 5],
            daemon: "g3d-termd/mock".into(),
            engine: "mock-vt".into(),
            ns: Some("mock".into()),
            pid: Some(std::process::id()),
            features: vec![
                "raw".into(),
                "view".into(),
                "status".into(),
                "key".into(),
                "paste".into(),
                "option_as_alt".into(),
                "mouse".into(),
                "paste_stream".into(),
            ],
            max_frame,
            instance: Some(instance.into()),
            generation: Some(generation),
            holder: None,
        }
    }

    fn read_raw_frame(sock: &mut UnixStream, dec: &mut FrameDecoder) -> Option<Frame> {
        let mut buf = [0u8; 4096];
        loop {
            if let Some(f) = dec.next_frame().unwrap() {
                return Some(f);
            }
            match sock.read(&mut buf) {
                Ok(0) => return None,
                Ok(n) => dec.feed(&buf[..n]),
                Err(_) => return None,
            }
        }
    }

    #[test]
    fn early_brain_failed_error_before_hello_surfaces_wire_error() {
        let scratch = ScratchDir::new("brainfail");
        let listener = UnixListener::bind(scratch.socket()).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let err = WireError::new(code::BRAIN_FAILED, "brain crashed 5 times");
            stream.write_all(&frame::encode_json(ty::ERROR, 0, &err)).unwrap();
            thread::sleep(Duration::from_millis(100));
        });

        let err = Termd::connect(&Target::socket(scratch.socket()), "ut", Box::new(|| {}))
            .err()
            .expect("must fail with brain_failed");
        assert!(err.is_code(code::BRAIN_FAILED), "got {err:?}");
        server.join().unwrap();
    }

    #[test]
    fn lagged_event_forwards_and_automatically_requests_repaint() {
        let scratch = ScratchDir::new("lagged");
        let listener = UnixListener::bind(scratch.socket()).unwrap();
        let (repaint_tx, repaint_rx) = mpsc::channel();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut dec = FrameDecoder::default();
            let hello = read_raw_frame(&mut stream, &mut dec).unwrap();
            assert_eq!(hello.ty, ty::HELLO);
            stream
                .write_all(&frame::encode_json(ty::WELCOME, 0, &mock_welcome("inst-1", 1, 16 << 20)))
                .unwrap();

            // Push a `lagged` event on handle 7.
            let lagged = serde_json::json!({"ev": "lagged", "dropped": 4096, "seq": 8192});
            stream.write_all(&frame::encode_json(ty::EVENT, 7, &lagged)).unwrap();

            // Expect the client's reader to automatically send a `repaint` request for handle 7.
            let repaint_req = read_raw_frame(&mut stream, &mut dec).unwrap();
            assert_eq!(repaint_req.ty, ty::REQUEST);
            let body: serde_json::Value = serde_json::from_slice(&repaint_req.payload).unwrap();
            assert_eq!(body["op"], "repaint");
            assert_eq!(body["handle"], 7);
            let req_id = body["req"].as_u64().unwrap();
            stream
                .write_all(&frame::encode_json(
                    ty::REPLY,
                    0,
                    &serde_json::json!({"req": req_id, "ok": true}),
                ))
                .unwrap();
            repaint_tx.send(()).unwrap();
            // Hold open until client drops.
            let _ = read_raw_frame(&mut stream, &mut dec);
        });

        let wakes = Arc::new(AtomicUsize::new(0));
        let wakes_clone = Arc::clone(&wakes);
        let client = Termd::connect(
            &Target::socket(scratch.socket()),
            "ut",
            Box::new(move || {
                wakes_clone.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .unwrap();

        let ev = client.events().recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(ev, Event::Lagged { handle: 7, dropped: 4096, seq: 8192 });
        // `push_event` sends on `events_tx` and then invokes `wake` before sending `repaint`,
        // so waiting for `repaint_rx` orders `wake()` strictly before reading `wakes`.
        repaint_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn paste_streams_chunks_with_more_when_frame_limit_is_small() {
        let scratch = ScratchDir::new("paste");
        let listener = UnixListener::bind(scratch.socket()).unwrap();
        let (chunks_tx, chunks_rx) = mpsc::channel();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut dec = FrameDecoder::default();
            let _ = read_raw_frame(&mut stream, &mut dec).unwrap();
            // max_frame = 1084 -> max_chunk = (1084 - 1024) / 6 = 10 bytes per chunk!
            stream
                .write_all(&frame::encode_json(ty::WELCOME, 0, &mock_welcome("inst-1", 1, 1084)))
                .unwrap();

            let mut received = Vec::new();
            while let Some(req_frame) = read_raw_frame(&mut stream, &mut dec) {
                let val: serde_json::Value = serde_json::from_slice(&req_frame.payload).unwrap();
                assert_eq!(val["op"], "paste");
                let text = val["text"].as_str().unwrap().to_owned();
                let more = val.get("more").and_then(serde_json::Value::as_bool).unwrap_or(false);
                let req_id = val["req"].as_u64().unwrap();
                stream
                    .write_all(&frame::encode_json(
                        ty::REPLY,
                        0,
                        &serde_json::json!({"req": req_id, "ok": true, "written": text.len()}),
                    ))
                    .unwrap();
                received.push((text, more));
                if !more {
                    break;
                }
            }
            chunks_tx.send(received).unwrap();
            let _ = read_raw_frame(&mut stream, &mut dec);
        });

        let client = Termd::connect(&Target::socket(scratch.socket()), "ut", Box::new(|| {})).unwrap();
        let sample = "héllo-world-from-chunked-paste!";
        client.paste(9, sample).unwrap();

        let chunks = chunks_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(chunks.len() >= 3, "expected multiple chunks, got {chunks:?}");
        for (_, more) in &chunks[..chunks.len() - 1] {
            assert!(*more);
        }
        assert!(!chunks.last().unwrap().1);
        let joined: String = chunks.into_iter().map(|(s, _)| s).collect();
        assert_eq!(joined, sample);

        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn request_timeout_unblocks_caller_without_wedging_connection() {
        let scratch = ScratchDir::new("timeout");
        let listener = UnixListener::bind(scratch.socket()).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut dec = FrameDecoder::default();
            let _ = read_raw_frame(&mut stream, &mut dec).unwrap();
            stream
                .write_all(&frame::encode_json(ty::WELCOME, 0, &mock_welcome("inst-1", 1, 16 << 20)))
                .unwrap();
            // Swallow first request without replying; answer second request (`ping`).
            let first = read_raw_frame(&mut stream, &mut dec).unwrap();
            let first_val: serde_json::Value = serde_json::from_slice(&first.payload).unwrap();
            assert_eq!(first_val["op"], "list");

            let second = read_raw_frame(&mut stream, &mut dec).unwrap();
            let second_val: serde_json::Value = serde_json::from_slice(&second.payload).unwrap();
            assert_eq!(second_val["op"], "ping");
            let req_id = second_val["req"].as_u64().unwrap();
            stream
                .write_all(&frame::encode_json(
                    ty::REPLY,
                    0,
                    &serde_json::json!({"req": req_id, "ok": true}),
                ))
                .unwrap();
            let _ = read_raw_frame(&mut stream, &mut dec);
        });

        let opts = ConnectOptions {
            request_timeout: Duration::from_millis(80),
            ..ConnectOptions::default()
        };
        let client =
            Termd::connect_with(&Target::socket(scratch.socket()), "ut", opts, Box::new(|| {}))
                .unwrap();
        let err = client.list().unwrap_err();
        assert!(matches!(err, Error::Timeout { op: "list", .. }), "got {err:?}");

        client.set_request_timeout(Duration::from_secs(2));
        client.ping().unwrap();
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn unexpected_drop_reconnects_resubscribes_repaints_and_emits_reconnected() {
        let scratch = ScratchDir::new("reconn");
        let listener = UnixListener::bind(scratch.socket()).unwrap();

        let server = thread::spawn(move || {
            // Generation 1: accept, subscribe `t1` on handle 4, then drop abruptly!
            let (mut s1, _) = listener.accept().unwrap();
            let mut dec1 = FrameDecoder::default();
            let _ = read_raw_frame(&mut s1, &mut dec1).unwrap();
            s1.write_all(&frame::encode_json(ty::WELCOME, 0, &mock_welcome("same-inst", 1, 16 << 20)))
                .unwrap();

            let sub_frame = read_raw_frame(&mut s1, &mut dec1).unwrap();
            let sub_val: serde_json::Value = serde_json::from_slice(&sub_frame.payload).unwrap();
            assert_eq!(sub_val["op"], "subscribe");
            let req_id = sub_val["req"].as_u64().unwrap();
            s1.write_all(&frame::encode_json(
                ty::REPLY,
                0,
                &serde_json::json!({
                    "req": req_id, "ok": true, "handle": 4, "role": "own", "seq": 0, "cols": 80, "rows": 24
                }),
            ))
            .unwrap();
            drop(s1); // Simulate unexpected brain crash!

            // Generation 2: accept reconnect, expect re-subscribe + repaint!
            let (mut s2, _) = listener.accept().unwrap();
            let mut dec2 = FrameDecoder::default();
            let _ = read_raw_frame(&mut s2, &mut dec2).unwrap();
            s2.write_all(&frame::encode_json(ty::WELCOME, 0, &mock_welcome("same-inst", 2, 16 << 20)))
                .unwrap();

            let resub = read_raw_frame(&mut s2, &mut dec2).unwrap();
            let resub_val: serde_json::Value = serde_json::from_slice(&resub.payload).unwrap();
            assert_eq!(resub_val["op"], "subscribe");
            assert_eq!(resub_val["id"], "t1");
            assert_eq!(resub_val["role"], "own");
            let resub_req = resub_val["req"].as_u64().unwrap();
            s2.write_all(&frame::encode_json(
                ty::REPLY,
                0,
                &serde_json::json!({
                    "req": resub_req, "ok": true, "handle": 4, "role": "own", "seq": 10, "cols": 80, "rows": 24
                }),
            ))
            .unwrap();

            let rep = read_raw_frame(&mut s2, &mut dec2).unwrap();
            let rep_val: serde_json::Value = serde_json::from_slice(&rep.payload).unwrap();
            assert_eq!(rep_val["op"], "repaint");
            assert_eq!(rep_val["handle"], 4);
            let rep_req = rep_val["req"].as_u64().unwrap();
            s2.write_all(&frame::encode_json(
                ty::REPLY,
                0,
                &serde_json::json!({"req": rep_req, "ok": true}),
            ))
            .unwrap();
            s2.write_all(&frame::encode_repaint(
                4,
                frame::RepaintHead { seq: 10, cols: 80, rows: 24, flags: 0 },
                b"restored",
            ))
            .unwrap();

            let _ = read_raw_frame(&mut s2, &mut dec2);
        });

        let client = Termd::connect(&Target::socket(scratch.socket()), "ut", Box::new(|| {})).unwrap();
        let sub = client.subscribe("t1", Role::Own, Some((80, 24)), 0).unwrap();
        assert_eq!(sub.handle, 4);

        let ev1 = client.events().recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(ev1, Event::Reconnected { restart: false, generation: Some(2) });

        let ev2 = client.events().recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(
            ev2,
            Event::Repaint {
                handle: 4,
                seq: 10,
                cols: 80,
                rows: 24,
                view: false,
                bytes: b"restored".to_vec(),
            }
        );

        drop(client);
        server.join().unwrap();
    }
}
