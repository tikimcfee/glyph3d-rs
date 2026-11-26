//! Per-terminal session: dedicated `Vt` worker thread, double-buffered `Screen` swap,
//! selection text extraction, and synchronous + non-blocking input helpers.

use crate::coalesce::{CoalescePolicy, Coalescer, PublishDecision};
use crate::hub::HubInner;
use crate::inbox::SessionInbox;
use glyph_vt::{Theme, Vt};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;
use term_cells::{Modes, Screen};
use termd_client::{Error, Event, Key, Mouse, Role, TermdHandle, View, ViewReply};

/// Scrollback view position of a subscription (`offset = 0` is live).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScrollbackState {
    /// Rows scrolled back from the live bottom (`0` = live).
    pub offset: u64,
    /// Deepest scrollback offset currently available in `g3d-termd`.
    pub max: u64,
    /// `true` while showing a scrollback view (`REPAINT` had `view` flag set; cursor hidden).
    pub viewing: bool,
}

/// How a terminal's process exited (`Event::Exit`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<String>,
    pub seq: u64,
}

/// Live throughput and coalescing counters for a [`TerminalSession`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionStats {
    /// Total raw `OUTPUT` + `REPAINT` VT payload bytes fed into this session's emulator.
    pub bytes_parsed: u64,
    /// Total coalesced snapshots published from the session thread to the window slot.
    pub publishes: u64,
    /// Total `resize` RPCs actually sent over the wire for this session.
    pub resizes_sent: u64,
    /// Total explicit `repaint` RPCs actually sent over the wire for this session.
    pub repaints_sent: u64,
}

#[derive(Clone)]
struct PublishedMeta {
    osc_title: String,
    pwd: Option<String>,
    modes: Modes,
    mouse_sgr: bool,
    cursor_color: u32,
    scrollback: ScrollbackState,
    bell_total: u64,
    role: Role,
    exit: Option<ExitInfo>,
    closed: bool,
    last_error: Option<(String, String)>,
    meta_gen: u64,
}

struct PublishedSlot {
    screen: Screen,
    meta: PublishedMeta,
}

pub(crate) struct SessionShared {
    published: Mutex<PublishedSlot>,
    has_news: AtomicBool,
    bytes_parsed: AtomicU64,
    publishes: AtomicU64,
    resizes_sent: AtomicU64,
    repaints_sent: AtomicU64,
}

/// Non-blocking fire-and-forget commands dispatched from UI callbacks so the window thread
/// never waits on socket round-trips or slow PTY readers.
pub(crate) enum AsyncCmd {
    Key { handle: u32, id: String, key: Key },
    Mouse { handle: u32, id: String, mouse: Mouse },
    Resize {
        handle: u32,
        id: String,
        cols: u16,
        rows: u16,
        shared: Arc<SessionShared>,
    },
    Input { handle: u32, id: String, bytes: Vec<u8> },
    View {
        handle: u32,
        id: String,
        view: View,
        shared: Arc<SessionShared>,
        wake: Arc<dyn Fn() + Send + Sync>,
    },
    Repaint {
        handle: u32,
        id: String,
        shared: Arc<SessionShared>,
    },
    Kill { id: String },
    Unsubscribe { handle: u32 },
}

struct AsyncQueueState {
    cmds: VecDeque<AsyncCmd>,
    closed: bool,
}

/// Outbound async command queue that preserves FIFO ordering for input/lifecycle requests while
/// coalescing queued `Resize` (keeping only the latest `(cols, rows)` per handle) and
/// deduplicating queued `Repaint` per handle so a multi-step drag never queues a backlog of stale
/// synchronous RPCs.
pub(crate) struct AsyncCmdQueue {
    state: Mutex<AsyncQueueState>,
    cv: Condvar,
}

impl AsyncCmdQueue {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(AsyncQueueState {
                cmds: VecDeque::with_capacity(32),
                closed: false,
            }),
            cv: Condvar::new(),
        }
    }

    pub(crate) fn push(&self, cmd: AsyncCmd) {
        let mut guard = self.state.lock().expect("async cmd queue mutex poisoned");
        if guard.closed {
            return;
        }
        match &cmd {
            AsyncCmd::Resize { handle, cols, rows, .. } => {
                for existing in &mut guard.cmds {
                    if let AsyncCmd::Resize {
                        handle: h,
                        cols: existing_cols,
                        rows: existing_rows,
                        ..
                    } = existing
                    {
                        if h == handle {
                            *existing_cols = *cols;
                            *existing_rows = *rows;
                            return;
                        }
                    }
                }
            }
            AsyncCmd::Repaint { handle, .. } => {
                let already_queued = guard
                    .cmds
                    .iter()
                    .any(|c| matches!(c, AsyncCmd::Repaint { handle: h, .. } if h == handle));
                if already_queued {
                    return;
                }
            }
            _ => {}
        }
        guard.cmds.push_back(cmd);
        self.cv.notify_one();
    }

    pub(crate) fn pop(&self) -> Option<AsyncCmd> {
        let mut guard = self.state.lock().expect("async cmd queue mutex poisoned");
        while guard.cmds.is_empty() && !guard.closed {
            guard = self.cv.wait(guard).expect("async cmd queue mutex poisoned");
        }
        guard.cmds.pop_front()
    }

    pub(crate) fn close(&self) {
        let mut guard = self.state.lock().expect("async cmd queue mutex poisoned");
        guard.closed = true;
        self.cv.notify_all();
    }
}

/// Window-side handle to one live terminal running its emulator on its own background thread.
///
/// Calling [`TerminalSession::pump`] swaps/copies only changed rows from the latest published
/// [`Screen`] in microseconds and never parses VT bytes on the window thread.
pub struct TerminalSession {
    hub: Arc<HubInner>,
    id: String,
    handle: u32,
    vt: Arc<Mutex<Vt>>,
    inbox: Arc<SessionInbox>,
    shared: Arc<SessionShared>,
    front_screen: Screen,
    meta: PublishedMeta,
    display_title: String,
    seen_bells: u64,
    seen_meta_gen: u64,
    worker: Option<JoinHandle<()>>,
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.inbox.close();
        self.hub.unregister_session(self.handle);
        if let Some(join) = self.worker.take() {
            let _ = join.join();
        }
        if !self.meta.closed && self.meta.exit.is_none() && !self.hub.is_closing() {
            self.hub.send_async(AsyncCmd::Unsubscribe { handle: self.handle });
        }
    }
}

impl TerminalSession {
    #[expect(
        clippy::too_many_arguments,
        reason = "internal constructor wiring hub, subscription dimensions, theme, policy, inbox, and wake"
    )]
    pub(crate) fn spawn_worker(
        hub: Arc<HubInner>,
        id: String,
        handle: u32,
        role: Role,
        cols: u16,
        rows: u16,
        theme: Theme,
        policy: CoalescePolicy,
        inbox: Arc<SessionInbox>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, Error> {
        let mut initial_vt = Vt::new(cols, rows, &theme);
        let mut initial_screen = Screen::new(cols, rows);
        initial_vt.fill(&mut initial_screen);
        let initial_cursor_color = initial_vt.cursor_color();
        let initial_modes = initial_screen.modes;

        let front_screen = initial_screen.clone();
        let initial_meta = PublishedMeta {
            osc_title: String::new(),
            pwd: None,
            modes: initial_modes,
            mouse_sgr: false,
            cursor_color: initial_cursor_color,
            scrollback: ScrollbackState::default(),
            bell_total: 0,
            role,
            exit: None,
            closed: false,
            last_error: None,
            meta_gen: 0,
        };

        let shared = Arc::new(SessionShared {
            published: Mutex::new(PublishedSlot {
                screen: initial_screen,
                meta: initial_meta.clone(),
            }),
            has_news: AtomicBool::new(false),
            bytes_parsed: AtomicU64::new(0),
            publishes: AtomicU64::new(0),
            resizes_sent: AtomicU64::new(0),
            repaints_sent: AtomicU64::new(0),
        });

        let vt = Arc::new(Mutex::new(initial_vt));
        let worker_vt = Arc::clone(&vt);
        let worker_inbox = Arc::clone(&inbox);
        let worker_shared = Arc::clone(&shared);
        let worker_hub = Arc::downgrade(&hub);
        let worker_id = id.clone();
        let thread_name = format!("term-vt:{id}");

        let worker = thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                session_worker_loop(
                    handle,
                    worker_id,
                    cols,
                    rows,
                    theme,
                    policy,
                    worker_vt,
                    worker_inbox,
                    worker_shared,
                    worker_hub,
                    wake,
                );
            })
            .map_err(Error::Io)?;

        Ok(Self {
            hub,
            display_title: id.clone(),
            id,
            handle,
            vt,
            inbox,
            shared,
            front_screen,
            meta: initial_meta,
            seen_bells: 0,
            seen_meta_gen: 0,
            worker: Some(worker),
        })
    }

    /// Terminal identifier in `g3d-termd`.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Numeric stream handle assigned by `g3d-termd`.
    pub fn handle(&self) -> u32 {
        self.handle
    }

    /// Latest swapped-in [`Screen`] (zero locking during rendering).
    pub fn screen(&self) -> &Screen {
        &self.front_screen
    }

    /// Resolved cursor colour (`0xAABBGGRR`).
    pub fn cursor_color(&self) -> u32 {
        self.meta.cursor_color
    }

    /// Display title: the program's `OSC 0`/`2` title when non-empty, else [`Self::id`].
    pub fn title(&self) -> &str {
        &self.display_title
    }

    /// Raw `OSC 0`/`2` title reported by the program (empty when unset).
    pub fn osc_title(&self) -> &str {
        &self.meta.osc_title
    }

    /// Working directory last reported by the program via `OSC 7`.
    pub fn pwd(&self) -> Option<&str> {
        self.meta.pwd.as_deref()
    }

    /// Active terminal input/screen modes (`alt_screen`, `mouse`, `bracketed_paste`, `app_cursor`, `focus_events`).
    pub fn modes(&self) -> Modes {
        self.meta.modes
    }

    /// Whether DECSET 1006 (`SGR` mouse encoding) is active on the daemon side.
    pub fn mouse_sgr(&self) -> bool {
        self.meta.mouse_sgr
    }

    /// Current scrollback view state (`offset`, `max`, `viewing`).
    pub fn scrollback(&self) -> ScrollbackState {
        self.meta.scrollback
    }

    /// Cumulative bell count observed on this session.
    pub fn bell_count(&self) -> u64 {
        self.meta.bell_total
    }

    /// Returns the number of bells rung since the previous call to `take_bell`.
    pub fn take_bell(&mut self) -> u64 {
        let delta = self.meta.bell_total.saturating_sub(self.seen_bells);
        self.seen_bells = self.meta.bell_total;
        delta
    }

    /// Current subscription role (`Own`, `Attach`, or `Watch`).
    pub fn role(&self) -> Role {
        self.meta.role
    }

    /// Process exit details once the program has exited.
    pub fn exit_status(&self) -> Option<&ExitInfo> {
        self.meta.exit.as_ref()
    }

    /// `true` once the program has exited or the terminal has closed.
    pub fn is_ended(&self) -> bool {
        self.meta.exit.is_some() || self.meta.closed
    }

    /// `true` once `Event::Closed` has arrived for this handle.
    pub fn is_closed(&self) -> bool {
        self.meta.closed
    }

    /// Most recent asynchronous terminal error (`code`, `message`), if any.
    pub fn last_error(&self) -> Option<(&str, &str)> {
        self.meta.last_error.as_ref().map(|(c, m)| (c.as_str(), m.as_str()))
    }

    /// Live byte-parse, screen-publish, and outbound resize/repaint RPC counters.
    pub fn stats(&self) -> SessionStats {
        SessionStats {
            bytes_parsed: self.shared.bytes_parsed.load(Ordering::Relaxed),
            publishes: self.shared.publishes.load(Ordering::Relaxed),
            resizes_sent: self.shared.resizes_sent.load(Ordering::Relaxed),
            repaints_sent: self.shared.repaints_sent.load(Ordering::Relaxed),
        }
    }

    /// Swaps in any finished screen/metadata published since the last call without parsing VT
    /// bytes. Returns `true` iff the visible screen or session metadata changed.
    pub fn pump(&mut self) -> bool {
        if !self.shared.has_news.swap(false, Ordering::Acquire) {
            return false;
        }
        let guard = self.shared.published.lock().expect("published mutex poisoned");
        let screen_changed = sync_screen_into(&guard.screen, &guard.screen.title, &mut self.front_screen);
        let meta_changed = guard.meta.meta_gen != self.seen_meta_gen;
        if meta_changed {
            self.meta = guard.meta.clone();
            self.seen_meta_gen = guard.meta.meta_gen;
            if self.meta.osc_title.is_empty() {
                if self.display_title != self.id {
                    self.display_title.clone_from(&self.id);
                }
            } else if self.display_title != self.meta.osc_title {
                self.display_title.clone_from(&self.meta.osc_title);
            }
        }
        screen_changed || meta_changed
    }

    /// Extracts stream selection text between `(col0, row0)` and `(col1, row1)` (`col` in
    /// `0..=cols`, end-exclusive) directly through the session's `Vt` emulator.
    ///
    /// Because the worker thread unlocks `Vt` at least every 16 KiB, this returns in microseconds
    /// even during a heavy output flood.
    pub fn text(&self, col0: u16, row0: u16, col1: u16, row1: u16) -> String {
        self.vt.lock().expect("vt mutex poisoned").text(col0, row0, col1, row1)
    }

    /// Extracts rectangular column-block selection text directly through `Vt`.
    pub fn block_text(&self, col0: u16, row0: u16, col1: u16, row1: u16) -> String {
        self.vt.lock().expect("vt mutex poisoned").block_text(col0, row0, col1, row1)
    }

    /// Extracts a single visual row (`0..cols`) with trailing whitespace trimmed through `Vt`.
    pub fn row_text(&self, row: u16) -> String {
        self.vt.lock().expect("vt mutex poisoned").row_text(row)
    }

    // ── Synchronous RPCs (block caller until daemon REPLY) ───────────────────

    /// Resizes the terminal in `g3d-termd` (requires [`Role::Own`]).
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), Error> {
        self.shared.resizes_sent.fetch_add(1, Ordering::Relaxed);
        self.hub.termd_handle().resize(self.handle, cols, rows)
    }

    /// Sends one W3C key event synchronously.
    pub fn key(&self, key: &Key) -> Result<(), Error> {
        self.hub.termd_handle().key(self.handle, key)
    }

    /// Pastes `text` synchronously (`allow_unsafe: true`, chunked with `more: true` past a frame).
    pub fn paste(&self, text: &str) -> Result<(), Error> {
        self.hub.termd_handle().paste(self.handle, text)
    }

    /// Pastes `text` synchronously with explicit `allow_unsafe`.
    pub fn paste_checked(&self, text: &str, allow_unsafe: bool) -> Result<(), Error> {
        self.hub.termd_handle().paste_checked(self.handle, text, allow_unsafe)
    }

    /// Sends one pointer event synchronously.
    pub fn mouse(&self, mouse: &Mouse) -> Result<(), Error> {
        self.hub.termd_handle().mouse(self.handle, mouse)
    }

    /// Writes raw PTY input bytes synchronously.
    pub fn input(&self, bytes: &[u8]) -> Result<(), Error> {
        self.hub.termd_handle().input(self.handle, bytes)
    }

    /// Scrolls this subscriber's view into scrollback (`View::Delta`) or returns to live
    /// (`View::Live`), updating [`Self::scrollback`] with `(offset, max)` from the reply.
    pub fn view(&mut self, view: View) -> Result<ViewReply, Error> {
        let reply = self.hub.termd_handle().view(self.handle, view)?;
        {
            let mut guard = self.shared.published.lock().expect("published mutex poisoned");
            guard.meta.scrollback.offset = reply.offset;
            guard.meta.scrollback.max = reply.max;
            if reply.offset == 0 {
                guard.meta.scrollback.viewing = false;
            }
            guard.meta.meta_gen += 1;
            self.meta.scrollback = guard.meta.scrollback;
            self.seen_meta_gen = guard.meta.meta_gen;
        }
        Ok(reply)
    }

    /// Requests an explicit `REPAINT` from `g3d-termd`.
    pub fn repaint(&self) -> Result<(), Error> {
        self.shared.repaints_sent.fetch_add(1, Ordering::Relaxed);
        self.hub.termd_handle().repaint(self.handle)
    }

    /// Kills this terminal in `g3d-termd`.
    pub fn kill(&self) -> Result<(), Error> {
        self.hub.termd_handle().kill(&self.id)
    }

    // ── Non-blocking UI dispatchers (never stall the window event loop) ──────

    /// Queues a key event in FIFO order without blocking the window thread.
    pub fn send_key(&self, key: Key) {
        if self.is_ended() {
            return;
        }
        self.hub.send_async(AsyncCmd::Key {
            handle: self.handle,
            id: self.id.clone(),
            key,
        });
    }

    /// Spawns a background paste (chunked with `more: true` at the program's pace) so even a
    /// multi-megabyte paste into a slow reader never stalls the window thread or subsequent keys.
    pub fn send_paste(&self, text: String) {
        if self.is_ended() || text.is_empty() {
            return;
        }
        let termd = self.hub.termd_handle();
        let handle = self.handle;
        let id = self.id.clone();
        let _ = thread::Builder::new().name("term-paste".into()).spawn(move || {
            if let Err(e) = termd.paste(handle, &text) {
                log::warn!("terminal {id}: paste was not sent: {e}");
            }
        });
    }

    /// Queues a pointer event in FIFO order without blocking the window thread.
    pub fn send_mouse(&self, mouse: Mouse) {
        if self.is_ended() {
            return;
        }
        self.hub.send_async(AsyncCmd::Mouse {
            handle: self.handle,
            id: self.id.clone(),
            mouse,
        });
    }

    /// Queues a terminal resize without blocking the window thread, coalescing in place so only
    /// the latest `(cols, rows)` for this terminal is sent over the wire during a multi-step drag.
    pub fn send_resize(&self, cols: u16, rows: u16) {
        if self.is_ended() {
            return;
        }
        self.hub.send_async(AsyncCmd::Resize {
            handle: self.handle,
            id: self.id.clone(),
            cols,
            rows,
            shared: Arc::clone(&self.shared),
        });
    }

    /// Queues a scrollback `view` step without blocking the window thread; updates `scrollback`
    /// and invokes `wake` once `ViewReply` arrives.
    pub fn send_view(&self, view: View) {
        if self.is_ended() {
            return;
        }
        self.hub.send_async(AsyncCmd::View {
            handle: self.handle,
            id: self.id.clone(),
            view,
            shared: Arc::clone(&self.shared),
            wake: self.hub.wake_fn(),
        });
    }

    /// Queues an explicit `repaint` request without blocking the window thread (deduplicated per handle).
    pub fn send_repaint(&self) {
        if self.is_ended() {
            return;
        }
        self.hub.send_async(AsyncCmd::Repaint {
            handle: self.handle,
            id: self.id.clone(),
            shared: Arc::clone(&self.shared),
        });
    }

    /// Queues raw PTY input bytes in FIFO order without blocking the window thread.
    pub fn send_input(&self, bytes: Vec<u8>) {
        if self.is_ended() || bytes.is_empty() {
            return;
        }
        self.hub.send_async(AsyncCmd::Input {
            handle: self.handle,
            id: self.id.clone(),
            bytes,
        });
    }
}

// ── Background session worker ────────────────────────────────────────────────

#[expect(
    clippy::too_many_arguments,
    reason = "worker entrypoint owns all per-session handles moved onto its dedicated OS thread"
)]
fn session_worker_loop(
    handle: u32,
    session_id: String,
    initial_cols: u16,
    initial_rows: u16,
    theme: Theme,
    policy: CoalescePolicy,
    vt_mutex: Arc<Mutex<Vt>>,
    inbox: Arc<SessionInbox>,
    shared: Arc<SessionShared>,
    hub_weak: std::sync::Weak<HubInner>,
    wake: Arc<dyn Fn() + Send + Sync>,
) {
    let mut working_screen = Screen::new(initial_cols, initial_rows);
    {
        let mut vt = vt_mutex.lock().expect("vt mutex poisoned");
        vt.fill(&mut working_screen);
    }

    let mut coalescer = Coalescer::new(policy);
    let mut batch: Vec<Event> = Vec::with_capacity(32);

    let mut osc_title = String::new();
    let mut prev_vt_title = String::new();
    let mut pwd: Option<String> = None;
    let mut mouse_sgr = false;
    let mut viewing_scrollback = false;
    let mut view_offset_zeroed = false;
    let mut bell_delta = 0u64;
    let mut role_override: Option<Role> = None;
    let mut exit_info: Option<ExitInfo> = None;
    let mut closed = false;
    let mut last_error: Option<(String, String)> = None;
    let mut meta_dirty = false;
    let mut grow_repaint_due: Option<Instant> = None;

    loop {
        batch.clear();
        let alive = inbox.drain_batch(&mut batch, policy.feed_quantum_bytes);

        if !batch.is_empty() {
            let mut parsed_this_batch = 0u64;
            for event in batch.drain(..) {
                match event {
                    Event::Output { bytes, .. } => {
                        parsed_this_batch += bytes.len() as u64;
                        for slice in bytes.chunks(policy.feed_quantum_bytes.max(1024)) {
                            let mut vt = vt_mutex.lock().expect("vt mutex poisoned");
                            vt.feed(slice);
                        }
                        coalescer.mark_dirty();
                    }
                    Event::Repaint { cols, rows, view, bytes, .. } => {
                        parsed_this_batch += bytes.len() as u64;
                        {
                            // Rebuild a clean `Vt` on every `REPAINT` so no truncated escape
                            // sequence, dynamic colour override, or un-reset DEC mode from before
                            // a lag resync or brain crash can bleed across the repaint boundary.
                            let mut vt = vt_mutex.lock().expect("vt mutex poisoned");
                            *vt = Vt::new(cols, rows, &theme);
                            for slice in bytes.chunks(policy.feed_quantum_bytes.max(1024)) {
                                vt.feed(slice);
                            }
                        }
                        prev_vt_title.clear();
                        if viewing_scrollback != view {
                            viewing_scrollback = view;
                            meta_dirty = true;
                        }
                        if !view {
                            view_offset_zeroed = true;
                            meta_dirty = true;
                            // A live repaint already carries any scrollback rows `termd` pulled
                            // onto the active grid, so cancel any pending post-resize repaint.
                            grow_repaint_due = None;
                        }
                        // Every `REPAINT` is a single authoritative snapshot from `termd`
                        // (initial attach, scrollback view, reconnect, lag resync, or settled
                        // primary-grow pull-back), never a byte flood: publish immediately.
                        coalescer.mark_immediate();
                    }
                    Event::Size { cols, rows, .. } => {
                        let (grew_primary, on_alt) = {
                            let mut vt = vt_mutex.lock().expect("vt mutex poisoned");
                            let prev_cols = vt.cols();
                            let prev_rows = vt.rows();
                            let alt = vt.modes().alt_screen;
                            vt.resize(cols, rows);
                            (!alt && (cols > prev_cols || rows > prev_rows), alt)
                        };
                        viewing_scrollback = false;
                        view_offset_zeroed = true;
                        meta_dirty = true;
                        coalescer.mark_dirty();
                        // `g3d-termd` sends raw subscribers `EVENT size` without `REPAINT`, so
                        // when a primary-screen terminal grows `cols` or `rows` (pulling history
                        // rows out of daemon scrollback), debounce and ask once after the drag
                        // settles (`grow_repaint_settle`) rather than on every intermediate step,
                        // and never on the alternate screen where the program redraws itself.
                        if on_alt {
                            grow_repaint_due = None;
                        } else if grew_primary || grow_repaint_due.is_some() {
                            if policy.grow_repaint_settle.is_zero() {
                                grow_repaint_due = None;
                                if let Some(hub) = hub_weak.upgrade() {
                                    hub.send_async(AsyncCmd::Repaint {
                                        handle,
                                        id: session_id.clone(),
                                        shared: Arc::clone(&shared),
                                    });
                                }
                            } else {
                                grow_repaint_due = Some(Instant::now() + policy.grow_repaint_settle);
                            }
                        }
                    }
                    Event::Title { title, .. } => {
                        if osc_title != title {
                            osc_title = title;
                            meta_dirty = true;
                            coalescer.mark_dirty();
                        }
                    }
                    Event::Modes { mouse_sgr: sgr, .. } => {
                        if mouse_sgr != sgr {
                            mouse_sgr = sgr;
                            meta_dirty = true;
                            coalescer.mark_dirty();
                        }
                    }
                    Event::Bell { .. } => {
                        bell_delta += 1;
                        meta_dirty = true;
                        coalescer.mark_dirty();
                    }
                    Event::Pwd { pwd: next_pwd, .. } => {
                        if pwd.as_deref() != Some(next_pwd.as_str()) {
                            pwd = Some(next_pwd);
                            meta_dirty = true;
                            coalescer.mark_dirty();
                        }
                    }
                    Event::Role { role, .. } => {
                        role_override = Some(role);
                        meta_dirty = true;
                        coalescer.mark_dirty();
                    }
                    Event::Lagged { .. } => {
                        // `termd-client` already triggers a repaint and `termd` pushes resync
                        // state; wait for the incoming `Repaint` to replace the screen cleanly.
                    }
                    Event::Exit { code, signal, seq, .. } => {
                        exit_info = Some(ExitInfo { code, signal, seq });
                        meta_dirty = true;
                        coalescer.mark_immediate();
                    }
                    Event::Closed { .. } => {
                        closed = true;
                        meta_dirty = true;
                        coalescer.mark_immediate();
                    }
                    Event::TermError { code, message, .. } => {
                        last_error = Some((code, message));
                        meta_dirty = true;
                        coalescer.mark_dirty();
                    }
                    _ => {}
                }
            }
            if parsed_this_batch > 0 {
                shared.bytes_parsed.fetch_add(parsed_this_batch, Ordering::Relaxed);
            }
        }

        if let Some(due) = grow_repaint_due {
            if Instant::now() >= due {
                grow_repaint_due = None;
                if alive && !closed && exit_info.is_none() {
                    if let Some(hub) = hub_weak.upgrade() {
                        hub.send_async(AsyncCmd::Repaint {
                            handle,
                            id: session_id.clone(),
                            shared: Arc::clone(&shared),
                        });
                    }
                }
            }
        }

        let sync_active = {
            let vt = vt_mutex.lock().expect("vt mutex poisoned");
            vt.synchronized_output()
        };

        let now = Instant::now();
        match coalescer.poll(now, sync_active && !closed && exit_info.is_none()) {
            PublishDecision::PublishNow => {
                publish_snapshot(
                    &vt_mutex,
                    &mut working_screen,
                    &shared,
                    &mut osc_title,
                    &mut prev_vt_title,
                    &pwd,
                    mouse_sgr,
                    viewing_scrollback,
                    &mut view_offset_zeroed,
                    &mut bell_delta,
                    &mut role_override,
                    &exit_info,
                    closed,
                    &last_error,
                    &mut meta_dirty,
                    wake.as_ref(),
                );
                coalescer.record_published(Instant::now());
            }
            PublishDecision::WaitUntil(deadline) => {
                if !alive {
                    // Inbox closed while dirty bytes remain: flush final state before exiting.
                    publish_snapshot(
                        &vt_mutex,
                        &mut working_screen,
                        &shared,
                        &mut osc_title,
                        &mut prev_vt_title,
                        &pwd,
                        mouse_sgr,
                        viewing_scrollback,
                        &mut view_offset_zeroed,
                        &mut bell_delta,
                        &mut role_override,
                        &exit_info,
                        closed,
                        &last_error,
                        &mut meta_dirty,
                        wake.as_ref(),
                    );
                    return;
                }
                let wake_at = grow_repaint_due.map_or(deadline, |g| deadline.min(g));
                let _ = inbox.wait_for_activity(Some(wake_at));
                continue;
            }
            PublishDecision::Idle => {
                if !alive || closed {
                    return;
                }
                if !inbox.wait_for_activity(grow_repaint_due) {
                    return;
                }
            }
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "borrows worker local state fields cleanly without allocating an intermediate struct per frame"
)]
fn publish_snapshot(
    vt_mutex: &Arc<Mutex<Vt>>,
    working_screen: &mut Screen,
    shared: &Arc<SessionShared>,
    osc_title: &mut String,
    prev_vt_title: &mut String,
    pwd: &Option<String>,
    mouse_sgr: bool,
    viewing_scrollback: bool,
    view_offset_zeroed: &mut bool,
    bell_delta: &mut u64,
    role_override: &mut Option<Role>,
    exit_info: &Option<ExitInfo>,
    closed: bool,
    last_error: &Option<(String, String)>,
    meta_dirty: &mut bool,
    wake: &(dyn Fn() + Send + Sync),
) {
    let (mut screen_changed, cursor_color) = {
        let mut vt = vt_mutex.lock().expect("vt mutex poisoned");
        let changed = vt.fill(working_screen);
        (changed, vt.cursor_color())
    };

    if viewing_scrollback && working_screen.cursor.visible {
        working_screen.cursor.visible = false;
        working_screen.generation += 1;
        screen_changed = true;
    }

    if working_screen.title != *prev_vt_title {
        prev_vt_title.clone_from(&working_screen.title);
        if *osc_title != working_screen.title {
            osc_title.clone_from(&working_screen.title);
            *meta_dirty = true;
        }
    }

    let mut slot = shared.published.lock().expect("published mutex poisoned");
    let copied = sync_screen_into(working_screen, osc_title, &mut slot.screen);

    let mut metadata_updated = *meta_dirty;
    if slot.meta.modes != slot.screen.modes {
        slot.meta.modes = slot.screen.modes;
        metadata_updated = true;
    }
    if slot.meta.cursor_color != cursor_color {
        slot.meta.cursor_color = cursor_color;
        metadata_updated = true;
    }
    if slot.meta.osc_title != *osc_title {
        slot.meta.osc_title.clone_from(osc_title);
        metadata_updated = true;
    }
    if slot.meta.pwd != *pwd {
        slot.meta.pwd.clone_from(pwd);
        metadata_updated = true;
    }
    if slot.meta.mouse_sgr != mouse_sgr {
        slot.meta.mouse_sgr = mouse_sgr;
        metadata_updated = true;
    }
    if slot.meta.scrollback.viewing != viewing_scrollback {
        slot.meta.scrollback.viewing = viewing_scrollback;
        metadata_updated = true;
    }
    if *view_offset_zeroed {
        *view_offset_zeroed = false;
        if slot.meta.scrollback.offset != 0 {
            slot.meta.scrollback.offset = 0;
            metadata_updated = true;
        }
    }
    if *bell_delta > 0 {
        slot.meta.bell_total += *bell_delta;
        *bell_delta = 0;
        metadata_updated = true;
    }
    if let Some(new_role) = role_override.take() {
        if slot.meta.role != new_role {
            slot.meta.role = new_role;
            metadata_updated = true;
        }
    }
    if slot.meta.exit != *exit_info {
        slot.meta.exit.clone_from(exit_info);
        metadata_updated = true;
    }
    if slot.meta.closed != closed {
        slot.meta.closed = closed;
        metadata_updated = true;
    }
    if slot.meta.last_error != *last_error {
        slot.meta.last_error.clone_from(last_error);
        metadata_updated = true;
    }
    if metadata_updated {
        slot.meta.meta_gen += 1;
    }
    *meta_dirty = false;
    drop(slot);

    if screen_changed || copied || metadata_updated {
        shared.publishes.fetch_add(1, Ordering::Relaxed);
        shared.has_news.store(true, Ordering::Release);
        wake();
    }
}

/// Incrementally synchronises `src` into `dst`, copying only rows whose `row_gen` or `wrapped`
/// changed and reusing `dst`'s allocations. Preserves `src.row_gen` verbatim and advances
/// `dst.generation` monotonically so downstream GPU cell renderers upload only dirty row ranges.
fn sync_screen_into(src: &Screen, effective_title: &str, dst: &mut Screen) -> bool {
    let dims_changed = dst.cols != src.cols || dst.rows != src.rows;
    let title_changed = dst.title != effective_title;
    let grid_changed = dims_changed
        || dst.row_gen != src.row_gen
        || dst.wrapped != src.wrapped
        || dst.cursor != src.cursor
        || dst.modes != src.modes
        || dst.default_fg != src.default_fg
        || dst.default_bg != src.default_bg
        || dst.clusters.len() != src.clusters.len();
    if !grid_changed && !title_changed {
        return false;
    }

    if dims_changed {
        dst.cols = src.cols;
        dst.rows = src.rows;
        dst.cells.clear();
        dst.cells.extend_from_slice(&src.cells);
        dst.row_gen.clear();
        dst.row_gen.extend_from_slice(&src.row_gen);
        dst.wrapped.clear();
        dst.wrapped.extend_from_slice(&src.wrapped);
    } else {
        for r in 0..src.rows {
            let idx = usize::from(r);
            if dst.row_gen[idx] != src.row_gen[idx] || dst.wrapped[idx] != src.wrapped[idx] {
                dst.row_mut(r).copy_from_slice(src.row(r));
                dst.row_gen[idx] = src.row_gen[idx];
                dst.wrapped[idx] = src.wrapped[idx];
            }
        }
    }

    if dst.clusters.len() > src.clusters.len() || !src.clusters.starts_with(&dst.clusters) {
        dst.clusters.clone_from(&src.clusters);
    } else if dst.clusters.len() < src.clusters.len() {
        dst.clusters.extend(src.clusters[dst.clusters.len()..].iter().cloned());
    }

    dst.generation = dst.generation.saturating_add(1).max(src.generation);
    dst.cursor = src.cursor;
    dst.modes = src.modes;
    dst.default_fg = src.default_fg;
    dst.default_bg = src.default_bg;
    if title_changed {
        dst.title.clear();
        dst.title.push_str(effective_title);
    }
    true
}

pub(crate) fn run_async_cmd_worker(termd: TermdHandle, queue: Arc<AsyncCmdQueue>) {
    while let Some(cmd) = queue.pop() {
        match cmd {
            AsyncCmd::Key { handle, id, key } => {
                if let Err(e) = termd.key(handle, &key) {
                    log::warn!("terminal {id}: key {:?} was not sent: {e}", key.code);
                }
            }
            AsyncCmd::Mouse { handle, id, mouse } => {
                if let Err(e) = termd.mouse(handle, &mouse) {
                    log::warn!("terminal {id}: mouse event was not sent: {e}");
                }
            }
            AsyncCmd::Resize {
                handle,
                id,
                cols,
                rows,
                shared,
            } => {
                shared.resizes_sent.fetch_add(1, Ordering::Relaxed);
                if let Err(e) = termd.resize(handle, cols, rows) {
                    log::warn!("terminal {id}: resize to {cols}x{rows} was refused: {e}");
                }
            }
            AsyncCmd::Input { handle, id, bytes } => {
                if let Err(e) = termd.input(handle, &bytes) {
                    log::warn!("terminal {id}: input was not sent: {e}");
                }
            }
            AsyncCmd::View { handle, id, view, shared, wake } => match termd.view(handle, view) {
                Ok(reply) => {
                    let mut guard = shared.published.lock().expect("published mutex poisoned");
                    guard.meta.scrollback.offset = reply.offset;
                    guard.meta.scrollback.max = reply.max;
                    if reply.offset == 0 {
                        guard.meta.scrollback.viewing = false;
                    }
                    guard.meta.meta_gen += 1;
                    drop(guard);
                    shared.has_news.store(true, Ordering::Release);
                    wake();
                }
                Err(e) => log::debug!("terminal {id}: view did not move: {e}"),
            },
            AsyncCmd::Repaint { handle, id, shared } => {
                shared.repaints_sent.fetch_add(1, Ordering::Relaxed);
                if let Err(e) = termd.repaint(handle) {
                    if !id.is_empty() {
                        log::warn!("terminal {id}: repaint was refused: {e}");
                    }
                }
            }
            AsyncCmd::Kill { id } => {
                if let Err(e) = termd.kill(&id) {
                    log::warn!("terminal {id}: kill was refused: {e}");
                }
            }
            AsyncCmd::Unsubscribe { handle } => {
                let _ = termd.unsubscribe(handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_shared() -> Arc<SessionShared> {
        Arc::new(SessionShared {
            published: Mutex::new(PublishedSlot {
                screen: Screen::new(80, 24),
                meta: PublishedMeta {
                    osc_title: String::new(),
                    pwd: None,
                    modes: Modes::default(),
                    mouse_sgr: false,
                    cursor_color: 0,
                    scrollback: ScrollbackState::default(),
                    bell_total: 0,
                    role: Role::Own,
                    exit: None,
                    closed: false,
                    last_error: None,
                    meta_gen: 0,
                },
            }),
            has_news: AtomicBool::new(false),
            bytes_parsed: AtomicU64::new(0),
            publishes: AtomicU64::new(0),
            resizes_sent: AtomicU64::new(0),
            repaints_sent: AtomicU64::new(0),
        })
    }

    #[test]
    fn async_cmd_queue_coalesces_resize_in_place_and_dedups_repaint_per_handle() {
        let q = AsyncCmdQueue::new();
        let s1 = dummy_shared();
        let s2 = dummy_shared();

        for step in 1..=14u16 {
            q.push(AsyncCmd::Resize {
                handle: 1,
                id: "t1".into(),
                cols: 80 + step,
                rows: 24 + step,
                shared: Arc::clone(&s1),
            });
            q.push(AsyncCmd::Repaint {
                handle: 1,
                id: "t1".into(),
                shared: Arc::clone(&s1),
            });
        }
        q.push(AsyncCmd::Resize {
            handle: 2,
            id: "t2".into(),
            cols: 100,
            rows: 30,
            shared: Arc::clone(&s2),
        });
        q.close();

        let mut popped = Vec::new();
        while let Some(cmd) = q.pop() {
            popped.push(cmd);
        }
        assert_eq!(popped.len(), 3, "14 resizes + 14 repaints on h1 + 1 resize on h2 -> 3 cmds");
        assert!(matches!(
            popped[0],
            AsyncCmd::Resize { handle: 1, cols: 94, rows: 38, .. }
        ));
        assert!(matches!(popped[1], AsyncCmd::Repaint { handle: 1, .. }));
        assert!(matches!(
            popped[2],
            AsyncCmd::Resize { handle: 2, cols: 100, rows: 30, .. }
        ));
    }
}
