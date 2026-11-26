//! Frame-aware screen publication coalescing and mode-2026 synchronized-output gating.
//!
//! Matches the leading-edge coalescing behaviour of `glyph3d-js/cli/attach_unix.go`:
//! - Output arriving after a quiet spell (`>= interval` since the previous publish, or on the
//!   very first update) publishes **immediately** so a keystroke's echo never waits on a timer.
//! - While output continues within `interval` (default `16 ms`, ~62.5 Hz), bytes feed into `Vt`
//!   in slices of at most `feed_quantum_bytes` (`16 KiB`) and publish once at each `interval`
//!   boundary, keeping the publish rate strictly bounded under `yes` or build-log floods.
//! - While DEC mode 2026 (`CSI ? 2026 h`, synchronized output) is active, publication waits for
//!   `CSI ? 2026 l` so partial mid-redraw frames never tear, bounded by `sync_timeout` (`1 s`)
//!   so a program killed mid-frame cannot freeze its display.

use std::time::{Duration, Instant};

/// Default minimum interval between two flood-time screen publications (~62.5 Hz).
pub const DEFAULT_COALESCE_INTERVAL: Duration = Duration::from_millis(16);

/// Maximum bytes fed into `Vt` under a single mutex lock before re-checking the coalescer clock
/// and letting `text()` readers step in (16 KiB, matching `attach_unix.go`).
pub const DEFAULT_FEED_QUANTUM_BYTES: usize = 16 * 1024;

/// Maximum time DEC mode 2026 (`SynchronizedOutput`) may hold back a dirty screen without seeing
/// `CSI ? 2026 l` (1 s, matching the DEC 2026 / Ghostty / kitty synchronized-update spec).
pub const DEFAULT_SYNC_TIMEOUT: Duration = Duration::from_secs(1);

/// Per-terminal unread event-queue ceiling before backpressure pushes back to the socket (2 MiB).
pub const DEFAULT_INBOX_MAX_BYTES: usize = 2 * 1024 * 1024;

/// Tunable parameters for per-session coalescing and queue bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoalescePolicy {
    pub interval: Duration,
    pub feed_quantum_bytes: usize,
    pub sync_timeout: Duration,
    pub inbox_max_bytes: usize,
}

impl Default for CoalescePolicy {
    fn default() -> Self {
        Self {
            interval: DEFAULT_COALESCE_INTERVAL,
            feed_quantum_bytes: DEFAULT_FEED_QUANTUM_BYTES,
            sync_timeout: DEFAULT_SYNC_TIMEOUT,
            inbox_max_bytes: DEFAULT_INBOX_MAX_BYTES,
        }
    }
}

/// Decision returned by [`Coalescer::poll`] for the session worker loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishDecision {
    /// Nothing un-published; block indefinitely until new events arrive.
    Idle,
    /// Publish the current screen and metadata immediately.
    PublishNow,
    /// Hold publication and wake at `deadline` if no decisive event arrives sooner.
    WaitUntil(Instant),
}

/// Pure clock state machine deciding when a session thread publishes its `Screen`.
#[derive(Debug)]
pub struct Coalescer {
    policy: CoalescePolicy,
    dirty: bool,
    immediate: bool,
    last_publish: Option<Instant>,
    sync_deadline: Option<Instant>,
}

impl Coalescer {
    pub fn new(policy: CoalescePolicy) -> Self {
        Self {
            policy,
            dirty: false,
            immediate: false,
            last_publish: None,
            sync_deadline: None,
        }
    }

    pub fn policy(&self) -> CoalescePolicy {
        self.policy
    }

    /// Marks that terminal bytes or metadata changed since the last publish.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Marks an urgent boundary (`Exit`, `Closed`, or explicit view `Repaint`) that bypasses the
    /// normal 16 ms coalescing hold and mode-2026 hold.
    pub fn mark_immediate(&mut self) {
        self.dirty = true;
        self.immediate = true;
    }

    /// Evaluates whether the session should publish at `now`, given whether the emulator is
    /// currently inside DEC mode 2026 (`sync_output`).
    pub fn poll(&mut self, now: Instant, sync_output: bool) -> PublishDecision {
        if !self.dirty {
            self.sync_deadline = None;
            return PublishDecision::Idle;
        }

        if self.immediate {
            return PublishDecision::PublishNow;
        }

        if sync_output && !self.policy.sync_timeout.is_zero() {
            let deadline = *self.sync_deadline.get_or_insert_with(|| now + self.policy.sync_timeout);
            if now < deadline {
                return PublishDecision::WaitUntil(deadline);
            }
            // Safety timeout elapsed while `?2026h` stayed set: flush anyway and re-arm if more
            // bytes arrive under `?2026h`.
        } else {
            self.sync_deadline = None;
        }

        match self.last_publish {
            None => PublishDecision::PublishNow,
            Some(prev) => {
                let next_due = prev + self.policy.interval;
                if now >= next_due {
                    PublishDecision::PublishNow
                } else {
                    PublishDecision::WaitUntil(next_due)
                }
            }
        }
    }

    /// Records that a publish occurred (or was checked and found clean) at `now`.
    pub fn record_published(&mut self, now: Instant) {
        self.dirty = false;
        self.immediate = false;
        self.sync_deadline = None;
        self.last_publish = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_edge_fires_immediately_after_quiet_spell_and_coalesces_bursts() {
        let policy = CoalescePolicy::default();
        let mut coalescer = Coalescer::new(policy);
        let t0 = Instant::now();

        assert_eq!(coalescer.poll(t0, false), PublishDecision::Idle);

        // First byte after creation -> immediate leading-edge publish.
        coalescer.mark_dirty();
        assert_eq!(coalescer.poll(t0, false), PublishDecision::PublishNow);
        coalescer.record_published(t0);

        // Byte 3 ms later -> held until t0 + 16 ms.
        coalescer.mark_dirty();
        assert_eq!(
            coalescer.poll(t0 + Duration::from_millis(3), false),
            PublishDecision::WaitUntil(t0 + Duration::from_millis(16))
        );

        // At t0 + 16 ms -> trailing-edge publish fires.
        let t1 = t0 + Duration::from_millis(16);
        assert_eq!(coalescer.poll(t1, false), PublishDecision::PublishNow);
        coalescer.record_published(t1);

        // Quiet spell of 50 ms, then a keystroke echo -> immediate leading-edge publish!
        let t2 = t1 + Duration::from_millis(50);
        coalescer.mark_dirty();
        assert_eq!(coalescer.poll(t2, false), PublishDecision::PublishNow);
    }

    #[test]
    fn synchronized_output_holds_mid_frame_until_end_or_timeout() {
        let policy = CoalescePolicy::default();
        let mut coalescer = Coalescer::new(policy);
        let t0 = Instant::now();

        // Program starts `?2026h` and clears screen: held even though we were quiet!
        coalescer.mark_dirty();
        assert_eq!(
            coalescer.poll(t0, true),
            PublishDecision::WaitUntil(t0 + policy.sync_timeout)
        );

        // 4 ms later program finishes drawing and sends `?2026l` -> publishes whole frame at once!
        let t_done = t0 + Duration::from_millis(4);
        assert_eq!(coalescer.poll(t_done, false), PublishDecision::PublishNow);
        coalescer.record_published(t_done);

        // If a program hangs with `?2026h` left on past `sync_timeout`, safety timeout flushes.
        let t_hang = t_done + Duration::from_millis(30);
        coalescer.mark_dirty();
        assert_eq!(
            coalescer.poll(t_hang, true),
            PublishDecision::WaitUntil(t_hang + policy.sync_timeout)
        );
        assert_eq!(
            coalescer.poll(t_hang + policy.sync_timeout, true),
            PublishDecision::PublishNow
        );
    }
}
