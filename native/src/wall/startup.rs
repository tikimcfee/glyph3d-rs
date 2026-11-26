//! Opt-in wall startup phase instrumentation (`GLYPH_WALL_STARTUP=1`).
//!
//! Zero overhead when `GLYPH_WALL_STARTUP` is unset or `"0"`: `enabled()` reads
//! a cached `OnceLock<bool>` and every helper short-circuits without syscalls.
//! When enabled, each phase writes one line to stderr with wall-clock time since
//! `process_start` (`t=+...ms`) and phase duration (`dt=...ms`) for spans.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static ORIGIN: OnceLock<Instant> = OnceLock::new();
static ENABLED: OnceLock<bool> = OnceLock::new();
static SEEN: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();

/// Record `process_start` at the very top of `main()`.
pub fn init_process_start() {
    let now = Instant::now();
    let _ = ORIGIN.set(now);
    let on = std::env::var_os("GLYPH_WALL_STARTUP")
        .is_some_and(|v| !v.is_empty() && v != "0");
    let _ = ENABLED.set(on);
    if on {
        eprintln!("[wall-startup] t=+0.0ms dt=0.0ms phase=process_start");
    }
}

/// Whether `GLYPH_WALL_STARTUP=1` is active for this process.
#[inline]
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        let _ = ORIGIN.get_or_init(Instant::now);
        std::env::var_os("GLYPH_WALL_STARTUP")
            .is_some_and(|v| !v.is_empty() && v != "0")
    })
}

#[inline]
fn elapsed_ms(now: Instant) -> f64 {
    let origin = *ORIGIN.get_or_init(|| now);
    now.saturating_duration_since(origin).as_secs_f64() * 1000.0
}

/// RAII timer that logs `[wall-startup] t=+...ms dt=...ms phase=<name>` on drop.
pub struct StartupSpan {
    phase: &'static str,
    started: Instant,
}

impl Drop for StartupSpan {
    fn drop(&mut self) {
        let now = Instant::now();
        let t = elapsed_ms(now);
        let dt = now.saturating_duration_since(self.started).as_secs_f64() * 1000.0;
        eprintln!("[wall-startup] t=+{t:.1}ms dt={dt:.1}ms phase={}", self.phase);
    }
}

/// Start timing `phase` if `GLYPH_WALL_STARTUP=1`; returns `None` otherwise.
#[inline]
pub fn span(phase: &'static str) -> Option<StartupSpan> {
    if !enabled() {
        return None;
    }
    Some(StartupSpan {
        phase,
        started: Instant::now(),
    })
}

/// Record a one-shot milestone (`phase` is printed at most once per process).
#[inline]
pub fn mark_once(phase: &'static str) {
    if !enabled() {
        return;
    }
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let inserted = seen
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(phase);
    if inserted {
        let t = elapsed_ms(Instant::now());
        eprintln!("[wall-startup] t=+{t:.1}ms phase={phase}");
    }
}

/// Record a one-shot milestone with an explicit detail tag (e.g. cold/warm/count).
#[inline]
pub fn mark_once_detail(phase: &'static str, detail: &str) {
    if !enabled() {
        return;
    }
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let inserted = seen
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(phase);
    if inserted {
        let t = elapsed_ms(Instant::now());
        eprintln!("[wall-startup] t=+{t:.1}ms phase={phase} ({detail})");
    }
}
