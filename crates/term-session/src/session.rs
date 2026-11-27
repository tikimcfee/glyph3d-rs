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

/// Scrollback view position and history depth of a subscription (`offset = 0` is live).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollbackState {
    /// Rows scrolled back from the live bottom (`0` = live).
    pub offset: u64,
    /// History length (`max` scrollback rows reported by `g3d-termd` via `view` replies and
    /// `status_follow`'s `scrollback_rows`).
    pub max: u64,
    /// `true` when the viewer sits at the live bottom (`offset == 0 && !viewing`).
    pub at_bottom: bool,
    /// `true` while showing a scrollback view (`REPAINT` had `view` flag set; cursor hidden).
    pub viewing: bool,
}

impl Default for ScrollbackState {
    fn default() -> Self {
        Self {
            offset: 0,
            max: 0,
            at_bottom: true,
            viewing: false,
        }
    }
}

impl ScrollbackState {
    /// Known scrollback history depth in rows (`self.max`).
    pub fn history_len(&self) -> u64 {
        self.max
    }

    /// Whether this terminal view sits at the live bottom (`self.at_bottom`).
    pub fn is_at_bottom(&self) -> bool {
        self.at_bottom
    }
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

/// Accumulated per-fill delta across all [`Vt::fill`] runs between two window
/// [`TerminalSession::pump`] drains.
///
/// Mirrors `glyph_vt::FillDelta` with plain owned types so `Sessions::pump`
/// can hand it to `wall_core::wall::Wall::terminal_screen_filled` without
/// coupling `wall-core` to `glyph-vt`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScreenFillDelta {
    /// Whether the grid resized in any fill since the previous drain.
    pub resized: bool,
    /// Cumulative rows scrolled off row `0` since the previous drain.
    pub scrolled_top: u32,
    /// Whether any scroll since the previous drain was confined to margins
    /// (`DECSTBM` / `DECSLRM`), downward, or line-delete rather than a
    /// whole-screen upward scroll.
    pub within_margins: bool,
    /// Per-row half-open `(lo, hi)` column spans rewritten in place, aligned to
    /// the post-scroll row indices of the published [`Screen`] and trimmed of
    /// trailing `(0, 0)` rows (empty when no cells were rewritten in place).
    pub rewritten_cols: Vec<(u16, u16)>,
}

/// Worker-to-window accumulator for [`ScreenFillDelta`] stored inside [`PublishedSlot`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PendingFillDelta {
    has_fill: bool,
    delta: ScreenFillDelta,
}

impl PendingFillDelta {
    fn capture_from_vt(&mut self, d: &glyph_vt::FillDelta) {
        self.has_fill = true;
        self.delta.resized = d.resized;
        self.delta.scrolled_top = d.scrolled_top;
        self.delta.within_margins = d.within_margins();
        self.delta.rewritten_cols.clear();
        if !d.resized {
            let rw = d.rewritten_cols();
            let rw_len = rw.iter().rposition(|&(lo, hi)| lo < hi).map_or(0, |i| i + 1);
            if rw_len > 0 {
                self.delta.rewritten_cols.extend_from_slice(&rw[..rw_len]);
            }
        }
    }

    fn merge(&mut self, next: &ScreenFillDelta, cols: u16, rows: u16) {
        if !self.has_fill {
            self.has_fill = true;
            self.delta.resized = next.resized;
            self.delta.scrolled_top = next.scrolled_top;
            self.delta.within_margins = next.within_margins;
            self.delta.rewritten_cols.clear();
            if !next.resized && !next.rewritten_cols.is_empty() {
                self.delta
                    .rewritten_cols
                    .extend_from_slice(&next.rewritten_cols);
            }
            return;
        }
        if self.delta.resized || next.resized {
            self.delta.resized = true;
            self.delta.scrolled_top = 0;
            self.delta.within_margins = false;
            self.delta.rewritten_cols.clear();
            return;
        }

        let prev_whole_scroll = !self.delta.within_margins && self.delta.scrolled_top > 0;
        let next_whole_scroll = !next.within_margins && next.scrolled_top > 0;

        if (prev_whole_scroll && next.within_margins)
            || (self.delta.within_margins && next_whole_scroll)
        {
            // A margin scroll dominates a whole-screen scroll; because rows
            // outside the margin also shifted, mark all rows rewritten so no
            // selection is left pointing at stale rows.
            self.delta.within_margins = true;
            self.delta.scrolled_top = self.delta.scrolled_top.saturating_add(next.scrolled_top);
            self.delta.rewritten_cols.clear();
            if cols > 0 && rows > 0 {
                self.delta
                    .rewritten_cols
                    .resize(usize::from(rows), (0, cols));
            }
            return;
        }

        if next_whole_scroll {
            let shift = next.scrolled_top as usize;
            if shift >= self.delta.rewritten_cols.len() {
                self.delta.rewritten_cols.clear();
            } else if shift > 0 {
                self.delta.rewritten_cols.drain(..shift);
            }
        }
        if next.within_margins {
            self.delta.within_margins = true;
        }
        self.delta.scrolled_top = self.delta.scrolled_top.saturating_add(next.scrolled_top);
        union_rewritten_cols(&mut self.delta.rewritten_cols, &next.rewritten_cols);
    }

    fn take(&mut self) -> Option<ScreenFillDelta> {
        if !self.has_fill {
            return None;
        }
        self.has_fill = false;
        Some(std::mem::take(&mut self.delta))
    }
}

fn union_rewritten_cols(dst: &mut Vec<(u16, u16)>, src: &[(u16, u16)]) {
    let src_len = src.iter().rposition(|&(lo, hi)| lo < hi).map_or(0, |i| i + 1);
    if src_len == 0 {
        return;
    }
    if dst.len() < src_len {
        dst.resize(src_len, (0, 0));
    }
    for (y, &(slo, shi)) in src[..src_len].iter().enumerate() {
        if slo >= shi {
            continue;
        }
        let (dlo, dhi) = dst[y];
        dst[y] = if dlo < dhi {
            (dlo.min(slo), dhi.max(shi))
        } else {
            (slo, shi)
        };
    }
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
    pending_delta: PendingFillDelta,
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
    last_fill_delta: Option<ScreenFillDelta>,
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
                pending_delta: PendingFillDelta::default(),
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
            last_fill_delta: None,
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

    /// Updates this session's [`Theme`] (including `bold_is_bright`) on its worker thread and
    /// schedules an immediate re-fill + publish so existing cells reflect the new theme right away.
    pub fn set_theme(&self, theme: Theme) {
        self.inbox.set_theme(theme);
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

    /// Current scrollback view state (`offset`, `max`, `at_bottom`, `viewing`).
    pub fn scrollback(&self) -> ScrollbackState {
        self.meta.scrollback
    }

    /// Updates known scrollback history length (`max`) from `g3d-termd`'s pushed `status_follow`
    /// document (`scrollback_rows`), keeping `max >= offset` while scrolled back without
    /// fast-forwarding `seen_meta_gen` (which would swallow pending worker metadata updates such
    /// as `Role`, `Title`, `Modes`, or `Closed`).
    pub(crate) fn note_status_scrollback(&mut self, scrollback_rows: u64) {
        let mut guard = self.shared.published.lock().expect("published mutex poisoned");
        apply_status_scrollback(&mut guard.meta, &mut self.meta, scrollback_rows);
    }

    /// Cumulative bell count observed on this session.
    pub fn bell_count(&self) -> u64 {
        self.meta.bell_total
    }

    /// Unread bell count waiting to be drained via [`Self::take_bell`].
    pub fn unread_bells(&self) -> u64 {
        self.meta.bell_total.saturating_sub(self.seen_bells)
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
        let mut guard = self.shared.published.lock().expect("published mutex poisoned");
        let screen_changed = sync_screen_into(&guard.screen, &guard.screen.title, &mut self.front_screen);
        let fill_delta = guard.pending_delta.take();
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
        drop(guard);
        if let Some(next) = fill_delta {
            match &mut self.last_fill_delta {
                Some(existing) => {
                    let mut acc = PendingFillDelta {
                        has_fill: true,
                        delta: std::mem::take(existing),
                    };
                    acc.merge(&next, self.front_screen.cols, self.front_screen.rows);
                    *existing = acc.delta;
                }
                None => self.last_fill_delta = Some(next),
            }
        }
        screen_changed || meta_changed
    }

    /// Takes the accumulated [`ScreenFillDelta`] from every worker fill drained by [`Self::pump`]
    /// since the previous take (`None` when no screen fill occurred).
    pub fn take_fill_delta(&mut self) -> Option<ScreenFillDelta> {
        self.last_fill_delta.take()
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

    /// Returns the `OSC 8` hyperlink URI at `(col, row)` (`0`-based), or `None` when the cell
    /// carries no link or is out of bounds. Wide-character continuation cells resolve to their
    /// leading cell.
    pub fn link_at(&self, col: u16, row: u16) -> Option<String> {
        self.vt
            .lock()
            .expect("vt mutex poisoned")
            .hyperlink(col, row)
            .map(str::to_owned)
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
    /// (`View::Live`), updating [`Self::scrollback`] with `(offset, max, at_bottom)` from the reply.
    pub fn view(&mut self, view: View) -> Result<ViewReply, Error> {
        let reply = self.hub.termd_handle().view(self.handle, view)?;
        {
            let mut guard = self.shared.published.lock().expect("published mutex poisoned");
            guard.meta.scrollback.offset = reply.offset;
            guard.meta.scrollback.max = reply.max;
            if reply.offset == 0 {
                guard.meta.scrollback.viewing = false;
            }
            guard.meta.scrollback.at_bottom =
                guard.meta.scrollback.offset == 0 && !guard.meta.scrollback.viewing;
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

/// Whether a daemon speaking `proto` repaints a raw subscriber by itself after a resize that
/// pulled scrollback rows onto the screen (protocol 1.6, PROTOCOL.md `resize`). One that does
/// is never asked for a repaint after a grow.
pub(crate) fn daemon_repaints_after_grow(proto: [u32; 2]) -> bool {
    proto[0] > 1 || (proto[0] == 1 && proto[1] >= 6)
}

#[expect(
    clippy::too_many_arguments,
    reason = "worker entrypoint owns all per-session handles moved onto its dedicated OS thread"
)]
fn session_worker_loop(
    handle: u32,
    session_id: String,
    initial_cols: u16,
    initial_rows: u16,
    mut theme: Theme,
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
    let mut fill_scratch = PendingFillDelta::default();

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
        if let Some(next_theme) = inbox.take_theme() {
            theme = next_theme;
            {
                let mut vt = vt_mutex.lock().expect("vt mutex poisoned");
                vt.set_theme(&theme);
            }
            coalescer.mark_immediate();
        }
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
                        // A primary-screen terminal that grows `cols` or `rows` pulls history
                        // rows out of the daemon's scrollback, and a raw subscriber needs a
                        // `REPAINT` to see them. A protocol 1.6 daemon (PROTOCOL.md `resize`,
                        // the T2 fix) sends one itself right after `size` whenever there is
                        // scrollback to pull, and a `size` it sends alone pulled nothing: asking
                        // it is never needed, and until 2026-11-27 this still asked once per
                        // drag, for a second copy of the screen it had just sent. A 1.5 daemon
                        // sends `size` alone either way, so: debounce and ask once after the
                        // drag settles (`grow_repaint_settle`) rather than on every
                        // intermediate step. Never on the alternate screen, where the program
                        // redraws itself. The daemon is asked for its protocol here, not once:
                        // an upgrade can change the brain under a live session.
                        let daemon_repaints = hub_weak.upgrade().is_some_and(|hub| {
                            daemon_repaints_after_grow(hub.termd_handle().welcome().proto)
                        });
                        if on_alt || daemon_repaints {
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
                        coalescer.mark_immediate();
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
                    &mut fill_scratch,
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
                        &mut fill_scratch,
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
    fill_scratch: &mut PendingFillDelta,
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
    fill_scratch.has_fill = false;
    let (mut screen_changed, cursor_color) = {
        let mut vt = vt_mutex.lock().expect("vt mutex poisoned");
        let changed = vt.fill(working_screen);
        if changed {
            fill_scratch.capture_from_vt(vt.last_fill_delta());
        }
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
    if fill_scratch.has_fill {
        slot.pending_delta
            .merge(&fill_scratch.delta, working_screen.cols, working_screen.rows);
    }

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
    let at_bottom = slot.meta.scrollback.offset == 0 && !slot.meta.scrollback.viewing;
    if slot.meta.scrollback.at_bottom != at_bottom {
        slot.meta.scrollback.at_bottom = at_bottom;
        metadata_updated = true;
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
    let links_changed = dst.hyperlinks != src.hyperlinks || dst.link_spans != src.link_spans;
    let cursor_color_changed = dst.cursor_color != src.cursor_color;
    let grid_changed = dims_changed
        || dst.row_gen != src.row_gen
        || dst.wrapped != src.wrapped
        || dst.cursor != src.cursor
        || dst.modes != src.modes
        || dst.default_fg != src.default_fg
        || dst.default_bg != src.default_bg
        || cursor_color_changed
        || links_changed
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
    if links_changed {
        dst.hyperlinks.clone_from(&src.hyperlinks);
        dst.link_spans.clone_from(&src.link_spans);
    }

    dst.generation = dst.generation.saturating_add(1).max(src.generation);
    dst.cursor = src.cursor;
    dst.modes = src.modes;
    dst.default_fg = src.default_fg;
    dst.default_bg = src.default_bg;
    dst.cursor_color = src.cursor_color;
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
                    guard.meta.scrollback.at_bottom =
                        guard.meta.scrollback.offset == 0 && !guard.meta.scrollback.viewing;
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

fn apply_status_scrollback(
    published_meta: &mut PublishedMeta,
    front_meta: &mut PublishedMeta,
    scrollback_rows: u64,
) {
    let next_max = scrollback_rows.max(published_meta.scrollback.offset);
    if published_meta.scrollback.max != next_max {
        published_meta.scrollback.max = next_max;
    }
    let front_max = scrollback_rows.max(front_meta.scrollback.offset);
    if front_meta.scrollback.max != front_max {
        front_meta.scrollback.max = front_max;
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
                pending_delta: PendingFillDelta::default(),
            }),
            has_news: AtomicBool::new(false),
            bytes_parsed: AtomicU64::new(0),
            publishes: AtomicU64::new(0),
            resizes_sent: AtomicU64::new(0),
            repaints_sent: AtomicU64::new(0),
        })
    }

    #[test]
    fn a_daemon_repaints_after_a_grow_from_protocol_1_6_on() {
        for (proto, repaints) in [
            ([1, 0], false),
            ([1, 4], false),
            ([1, 5], false),
            ([1, 6], true),
            ([1, 7], true),
            ([1, 60], true),
            ([2, 0], true),
            ([0, 9], false),
        ] {
            assert_eq!(daemon_repaints_after_grow(proto), repaints, "protocol {proto:?}");
        }
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

    fn delta(
        resized: bool,
        scrolled_top: u32,
        within_margins: bool,
        rewritten_cols: Vec<(u16, u16)>,
    ) -> ScreenFillDelta {
        ScreenFillDelta {
            resized,
            scrolled_top,
            within_margins,
            rewritten_cols,
        }
    }

    #[test]
    fn fill_delta_accumulates_scrolls_shifts_prior_rewrites_and_unions_spans() {
        let mut acc = PendingFillDelta::default();
        assert_eq!(acc.take(), None);

        // First fill: in-place rewrite on row 4 cols 2..6.
        acc.merge(&delta(false, 0, false, vec![(0, 0), (0, 0), (0, 0), (0, 0), (2, 6)]), 20, 6);
        // Second fill: whole-screen scroll up by 2 rows, plus rewrite on new row 2 cols 8..12
        // (same original row 4!) and new row 5 cols 0..4.
        acc.merge(
            &delta(false, 2, false, vec![(0, 0), (0, 0), (8, 12), (0, 0), (0, 0), (0, 4)]),
            20,
            6,
        );
        // Third fill: whole-screen scroll up by 1 row with no in-place rewrites.
        acc.merge(&delta(false, 1, false, Vec::new()), 20, 6);

        let merged = acc.take().expect("accumulated delta");
        assert!(!merged.resized);
        assert_eq!(merged.scrolled_top, 3);
        assert!(!merged.within_margins);
        // Row 4 shifted up 2 -> row 2 unioned (2,6)U(8,12) = (2,12), then shifted up 1 -> row 1.
        // Row 5 shifted up 1 -> row 4.
        assert_eq!(
            merged.rewritten_cols,
            vec![(0, 0), (2, 12), (0, 0), (0, 0), (0, 4)]
        );
        assert_eq!(acc.take(), None);
    }

    #[test]
    fn fill_delta_preserves_margin_scroll_and_ors_resize_and_avoids_empty_alloc() {
        // Pure scroll without in-place rewrites keeps rewritten_cols unallocated.
        let mut pure = PendingFillDelta::default();
        pure.merge(&delta(false, 2, false, Vec::new()), 20, 6);
        pure.merge(&delta(false, 3, false, Vec::new()), 20, 6);
        let d = pure.take().expect("pure scroll");
        assert_eq!(d.scrolled_top, 5);
        assert!(!d.within_margins);
        assert_eq!(d.rewritten_cols.capacity(), 0);

        // Margin scroll followed by non-scrolling edit stays a margin scroll with spans unioned.
        let mut margin = PendingFillDelta::default();
        margin.merge(&delta(false, 0, true, vec![(0, 0), (0, 20)]), 20, 6);
        margin.merge(&delta(false, 0, false, vec![(3, 7)]), 20, 6);
        let d = margin.take().expect("margin scroll");
        assert!(d.within_margins);
        assert_eq!(d.rewritten_cols, vec![(3, 7), (0, 20)]);

        // Whole-screen scroll mixed with margin scroll stays a margin scroll and marks all rows.
        let mut mixed = PendingFillDelta::default();
        mixed.merge(&delta(false, 2, false, Vec::new()), 20, 4);
        mixed.merge(&delta(false, 0, true, vec![(0, 0), (0, 20)]), 20, 4);
        let d = mixed.take().expect("mixed");
        assert!(d.within_margins);
        assert_eq!(d.rewritten_cols, vec![(0, 20); 4]);

        // Resize ORs in either order and clears rewritten_cols.
        let mut resized = PendingFillDelta::default();
        resized.merge(&delta(false, 1, false, vec![(1, 5)]), 20, 4);
        resized.merge(&delta(true, 0, false, Vec::new()), 30, 8);
        resized.merge(&delta(false, 0, false, vec![(0, 10)]), 30, 8);
        let d = resized.take().expect("resized");
        assert!(d.resized);
        assert!(d.rewritten_cols.is_empty());
    }

    #[test]
    fn sync_screen_into_copies_osc8_links_and_cursor_color_and_bumps_generation() {
        let mut src = Screen::new(20, 4);
        let mut dst = Screen::new(20, 4);
        assert!(sync_screen_into(&src, "", &mut dst) || dst.generation == src.generation);
        let g0 = dst.generation;

        // No changes -> returns false without bumping generation.
        assert!(!sync_screen_into(&src, "", &mut dst));
        assert_eq!(dst.generation, g0);

        // Mutating ONLY OSC 8 links on src must sync into dst and bump generation.
        src.hyperlinks.push("https://example.org/osc8".to_owned());
        src.link_spans.push(term_cells::LinkSpan {
            row: 0,
            start_col: 2,
            end_col: 10,
            link_id: 0,
        });
        assert!(sync_screen_into(&src, "", &mut dst));
        assert!(dst.generation > g0);
        assert_eq!(dst.hyperlink_at(5, 0), Some("https://example.org/osc8"));
        let g1 = dst.generation;

        // Mutating ONLY cursor_color (OSC 12) must also sync and bump generation.
        src.cursor_color = term_cells::rgb(0xff, 0xaa, 0x00);
        assert!(sync_screen_into(&src, "", &mut dst));
        assert!(dst.generation > g1);
        assert_eq!(dst.cursor_color, src.cursor_color);
    }

    #[test]
    fn status_scrollback_update_preserves_unread_worker_meta_gen() {
        let shared = dummy_shared();
        let mut front = shared.published.lock().unwrap().meta.clone();
        let seen_meta_gen = front.meta_gen;

        // Simulate worker thread publishing `Role::Attach` (`meta_gen` 0 -> 1) before `pump()`.
        {
            let mut slot = shared.published.lock().unwrap();
            slot.meta.role = Role::Attach;
            slot.meta.meta_gen += 1;
        }

        // `DaemonEvent::Status` arrives in the same `Sessions::pump` pass with new `scrollback_rows`.
        {
            let mut slot = shared.published.lock().unwrap();
            apply_status_scrollback(&mut slot.meta, &mut front, 42);
            assert_eq!(slot.meta.scrollback.max, 42);
            assert_eq!(front.scrollback.max, 42);
            // Unread worker metadata change must still be visible (`meta_gen != seen_meta_gen`).
            assert_ne!(slot.meta.meta_gen, seen_meta_gen);
            assert_eq!(slot.meta.role, Role::Attach);
        }
    }
}
