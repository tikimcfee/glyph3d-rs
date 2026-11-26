//! Byte-bounded FIFO inbox between the hub router thread and a terminal's session thread.
//!
//! Bounding queued payload bytes (`max_bytes`) ensures a terminal flooded with `yes` cannot
//! accumulate unbounded memory on the client side even if CPU scheduling momentarily starves its
//! worker thread; backpressure propagates naturally to `termd-client`'s socket reader and
//! `g3d-termd`'s per-subscription queue.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};
use termd_client::Event;

struct QueueState {
    items: VecDeque<Event>,
    queued_bytes: usize,
    closed: bool,
}

pub(crate) struct SessionInbox {
    state: Mutex<QueueState>,
    not_empty: Condvar,
    not_full: Condvar,
    max_bytes: usize,
}

impl SessionInbox {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            state: Mutex::new(QueueState {
                items: VecDeque::with_capacity(64),
                queued_bytes: 0,
                closed: false,
            }),
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
            max_bytes: max_bytes.max(4096),
        }
    }

    /// Pushes `event` into the session's FIFO queue, blocking only if unread data frames already
    /// exceed `max_bytes`. Returns `false` if the session was closed.
    pub(crate) fn push(&self, event: Event) -> bool {
        let weight = event_byte_weight(&event);
        let mut guard = self.state.lock().expect("inbox mutex poisoned");
        // Always admit the first item when empty so a single large REPAINT (> max_bytes) never deadlocks.
        while !guard.closed && !guard.items.is_empty() && guard.queued_bytes + weight > self.max_bytes
        {
            guard = self.not_full.wait(guard).expect("inbox mutex poisoned");
        }
        if guard.closed {
            return false;
        }
        guard.queued_bytes += weight;
        guard.items.push_back(event);
        self.not_empty.notify_one();
        true
    }

    /// Pops up to `budget_bytes` of events without waiting, so the worker can feed `Vt` in
    /// bounded quanta and re-check the coalescing clock between batches.
    pub(crate) fn drain_batch(&self, out: &mut Vec<Event>, budget_bytes: usize) -> bool {
        let mut guard = self.state.lock().expect("inbox mutex poisoned");
        let mut taken_bytes = 0usize;
        let was_full = guard.queued_bytes >= self.max_bytes / 2;
        while let Some(front) = guard.items.front() {
            let weight = event_byte_weight(front);
            if !out.is_empty() && taken_bytes + weight > budget_bytes {
                break;
            }
            let ev = guard.items.pop_front().expect("checked front");
            guard.queued_bytes = guard.queued_bytes.saturating_sub(weight);
            taken_bytes += weight;
            out.push(ev);
            if taken_bytes >= budget_bytes {
                break;
            }
        }
        if was_full || guard.items.is_empty() {
            self.not_full.notify_all();
        }
        !guard.closed || !guard.items.is_empty()
    }

    /// Waits for at least one event to arrive, `deadline` (when `Some`) to pass, or closure.
    /// Returns `false` when closed and empty.
    pub(crate) fn wait_for_activity(&self, deadline: Option<Instant>) -> bool {
        let mut guard = self.state.lock().expect("inbox mutex poisoned");
        while guard.items.is_empty() && !guard.closed {
            match deadline {
                None => {
                    guard = self.not_empty.wait(guard).expect("inbox mutex poisoned");
                }
                Some(until) => {
                    let Some(remaining) = until.checked_duration_since(Instant::now()) else {
                        return true;
                    };
                    if remaining == Duration::ZERO {
                        return true;
                    }
                    let (next_guard, timed_out) = self
                        .not_empty
                        .wait_timeout(guard, remaining)
                        .expect("inbox mutex poisoned");
                    guard = next_guard;
                    if timed_out.timed_out() {
                        return !guard.closed || !guard.items.is_empty();
                    }
                }
            }
        }
        !guard.closed || !guard.items.is_empty()
    }

    /// Marks the inbox closed and wakes both producer and consumer threads immediately.
    pub(crate) fn close(&self) {
        let mut guard = self.state.lock().expect("inbox mutex poisoned");
        guard.closed = true;
        self.not_empty.notify_all();
        self.not_full.notify_all();
    }
}

fn event_byte_weight(event: &Event) -> usize {
    match event {
        Event::Output { bytes, .. } | Event::Repaint { bytes, .. } => bytes.len().max(64),
        Event::Title { title, .. } => 64 + title.len(),
        Event::Pwd { pwd, .. } => 64 + pwd.len(),
        _ => 64,
    }
}
