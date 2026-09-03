//! wgpu instance/adapter/device/queue init shared by windowed and offscreen modes.
//!
//! Future stages: call `init(None)` for headless work, or `init(Some(&surface))`
//! when a surface must influence adapter selection. Everything downstream only
//! needs `&GpuContext`.
//!
//! Stage H: opt-in GPU pass profiling (wgpu-profiler). `GLYPH_PROFILE=1`
//! requests TIMESTAMP_QUERY (when the adapter supports it) and constructs a
//! GpuProfiler; without the env var the device is created exactly as before
//! and every profiling call site is an `Option` no-op.
//!
//! Stage L (O1): `device.on_uncaptured_error` gets a dedup tracker — see
//! `ErrorTracker` below.

use std::cell::RefCell;

// ── Stage L (O1): uncaptured-error dedup ─────────────────────────────────
// ErrorTracker pattern from re_renderer (rerun/crates/viewer/re_renderer/
// src/error_handling/error_tracker.rs): dedup by error identity, log the
// first occurrence in full, count repeats and summarize at powers of ten.
// Ours is a pure structure (unit-tested at the bottom of this file) — no
// wgpu-core downcasting (re_renderer's native dedup heuristic needs wgc
// types; the identity key here is simply (kind, description)).
//
// SEMANTIC NOTE (verified against wgpu-30.0.1 src/backend/wgpu_core.rs:692):
// wgpu 30's DEFAULT uncaptured handler PANICS on the first error
// ("Handling wgpu errors as fatal by default"). Installing this tracker
// changes that to log-once-and-continue — the rerun-style behavior the
// steal asks for ("log once"): a repeating per-frame error cannot spam, and
// an interactive windowed session survives it. Any error that moves pixels
// is still caught by the byte-equal gates; any error that doesn't is
// precisely the class where continuing is informative. To restore fatality,
// delete the on_uncaptured_error install in init().

/// The wgpu::Error variant, for dedup identity.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum ErrorKind {
    Validation,
    Internal,
    OutOfMemory,
}

/// What `ErrorTracker::track` decided for one occurrence.
#[derive(Debug, PartialEq, Eq)]
enum TrackDecision {
    /// First sighting of this (kind, description): log it in full.
    First,
    /// A repeat; the payload is the total occurrence count. The caller logs
    /// a one-line summary only when `is_count_milestone` holds.
    Repeat(u64),
}

/// Log a repeat summary at 10, 100, 1000, … occurrences (bounded chatter,
/// monotonically sparser).
fn is_count_milestone(n: u64) -> bool {
    if n < 10 {
        return false;
    }
    let mut p = 10u64;
    while p < n {
        p *= 10;
    }
    p == n
}

/// Dedup state for uncaptured wgpu errors. Pure: no GPU types inside.
#[derive(Default)]
struct ErrorTracker {
    /// (kind, description) → total occurrence count.
    counts: std::collections::HashMap<(ErrorKind, String), u64>,
}

impl ErrorTracker {
    fn track(&mut self, kind: ErrorKind, description: &str) -> TrackDecision {
        let n = {
            let e = self
                .counts
                .entry((kind, description.to_string()))
                .or_insert(0);
            *e += 1;
            *e
        };
        if n == 1 {
            TrackDecision::First
        } else {
            TrackDecision::Repeat(n)
        }
    }

    /// Route one uncaptured wgpu error through the tracker and log per the
    /// decision. This is the on_uncaptured_error callback body.
    fn handle(tracker: &std::sync::Mutex<Self>, err: &wgpu::Error) {
        let (kind, desc) = match err {
            wgpu::Error::Validation { description, .. } => {
                (ErrorKind::Validation, description.clone())
            }
            wgpu::Error::Internal { description, .. } => {
                (ErrorKind::Internal, description.clone())
            }
            wgpu::Error::OutOfMemory { .. } => (ErrorKind::OutOfMemory, err.to_string()),
        };
        match tracker
            .lock()
            .expect("ErrorTracker mutex poisoned")
            .track(kind, &desc)
        {
            TrackDecision::First => log::error!(
                "wgpu {kind:?} error (first occurrence; repeats are counted, not spammed): {desc}"
            ),
            TrackDecision::Repeat(n) if is_count_milestone(n) => {
                log::error!("wgpu {kind:?} error has now occurred {n} times: {desc}")
            }
            TrackDecision::Repeat(_) => {}
        }
    }
}


/// Logged adapter identity, kept around for diagnostics.
pub struct GpuContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    /// Stage H: per-frame GPU pass profiler. `Some` only when GLYPH_PROFILE=1
    /// AND the adapter supports TIMESTAMP_QUERY. RefCell because scenes render
    /// through `&GpuContext` while the profiler holds per-frame mutable state.
    pub profiler: Option<RefCell<wgpu_profiler::GpuProfiler>>,
    /// Stage H: CPU-side scope times (e.g. the cull pass), merged into the
    /// profile summary. Written by scenes only when `profiler` is `Some`.
    pub cpu_scopes: RefCell<std::collections::BTreeMap<String, (f64, u64)>>,
}

/// Record one CPU scope sample (ms) — no-op semantics live at the call site
/// (callers guard on `ctx.profiler.is_some()`).
pub fn record_cpu_scope(ctx: &GpuContext, label: &str, ms: f64) {
    let mut scopes = ctx.cpu_scopes.borrow_mut();
    let entry = scopes.entry(label.to_string()).or_insert((0.0, 0));
    entry.0 += ms;
    entry.1 += 1;
}

/// Stage H: running mean of GPU scope times across frames.
#[derive(Default)]
pub struct ProfileAccumulator {
    /// label path ("pass/nested") → (total ms, sample count)
    pub scopes: std::collections::BTreeMap<String, (f64, u64)>,
    pub frames_measured: u64,
}

impl ProfileAccumulator {
    /// Fold one finished frame's query tree into the running means.
    pub fn add_frame(&mut self, results: &[wgpu_profiler::GpuTimerQueryResult]) {
        self.frames_measured += 1;
        for r in results {
            Self::add_recursive("", r, &mut self.scopes);
        }
    }

    fn add_recursive(
        prefix: &str,
        r: &wgpu_profiler::GpuTimerQueryResult,
        scopes: &mut std::collections::BTreeMap<String, (f64, u64)>,
    ) {
        let path = if prefix.is_empty() {
            r.label.clone()
        } else {
            format!("{prefix}/{}", r.label)
        };
        if let Some(time) = &r.time {
            let ms = (time.end - time.start) * 1000.0;
            let entry = scopes.entry(path.clone()).or_insert((0.0, 0));
            entry.0 += ms;
            entry.1 += 1;
        }
        for n in &r.nested_queries {
            Self::add_recursive(&path, n, scopes);
        }
    }

    /// "label 0.42ms, nested/label 0.01ms" — mean per scope over all samples.
    pub fn summary(&self) -> String {
        self.scopes
            .iter()
            .map(|(label, (total, count))| format!("{label} {:.3}ms", total / *count as f64))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Format the CPU scope means collected in `ctx.cpu_scopes` and clear them
/// (per-window reporting: windowed prints once a second, offscreen once a run).
pub fn take_cpu_scope_summary(ctx: &GpuContext) -> String {
    let scopes = std::mem::take(&mut *ctx.cpu_scopes.borrow_mut());
    scopes
        .iter()
        .map(|(label, (total, count))| format!("{label} {:.3}ms", total / *count as f64))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Create the instance/adapter/device/queue. Requests the adapter's FULL
/// storage-buffer/buffer-size headroom (repo-scale arenas need it) and, only
/// under GLYPH_PROFILE=1, TIMESTAMP_QUERY. Panics when no adapter is found —
/// there is no CPU fallback to recover to.
pub async fn init(compatible_surface: Option<&wgpu::Surface<'_>>) -> GpuContext {
    // wgpu 30: takes the descriptor by value.
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        // PRIMARY backends on macOS resolve to Metal.
        backends: wgpu::Backends::PRIMARY,
        // wgpu 30: InstanceDescriptor has no Default impl; start from the
        // no-display-handle constructor and override what we need.
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });

    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface,
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        })
        .await
        .expect("no suitable wgpu adapter found");

    let info = adapter.get_info();
    log::info!(
        "adapter: name={:?} backend={:?} vendor=0x{:04x} device=0x{:04x} driver={:?}",
        info.name,
        info.backend,
        info.vendor,
        info.device,
        info.driver_info,
    );

    // Stage E2: a repo-scale glyph arena can exceed the default 128 MiB
    // storage binding by an order of magnitude (48 B × tens of millions of
    // glyphs). Request the adapter's FULL headroom; the scene chunks the
    // arena so no single binding exceeds max_storage_buffer_binding_size, so
    // any adapter value works — but the bigger the limit, the fewer chunks.
    let supported = adapter.limits();
    let limits = wgpu::Limits {
        max_storage_buffer_binding_size: supported.max_storage_buffer_binding_size,
        max_buffer_size: supported.max_buffer_size,
        ..Default::default()
    };

    // Stage F: multi_draw_indirect is CORE in wgpu 30 (per-chunk indirect
    // draws for the cull pass need no feature gate). Log the count variant
    // for diagnostics only — the fixed-slot args scheme doesn't use it.
    log::info!(
        "adapter features: MULTI_DRAW_INDIRECT_COUNT={}",
        adapter.features().contains(wgpu::Features::MULTI_DRAW_INDIRECT_COUNT)
    );

    // Stage H: opt-in profiling. Request TIMESTAMP_QUERY (+ INSIDE_PASSES for
    // nested in-pass scopes) ONLY when GLYPH_PROFILE=1 and the adapter
    // supports it; a missing feature must never break a render.
    let profile_wanted = std::env::var_os("GLYPH_PROFILE").is_some();
    let adapter_features = adapter.features();
    let mut required_features = wgpu::Features::empty();
    let profiling_supported = profile_wanted
        && adapter_features.contains(wgpu::Features::TIMESTAMP_QUERY);
    if profile_wanted {
        if profiling_supported {
            required_features |= wgpu::Features::TIMESTAMP_QUERY;
            if adapter_features.contains(wgpu::Features::TIMESTAMP_QUERY_INSIDE_PASSES) {
                required_features |= wgpu::Features::TIMESTAMP_QUERY_INSIDE_PASSES;
            }
        } else {
            log::warn!("profiling unavailable: no TIMESTAMP_QUERY (running unprofiled)");
        }
    }

    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("glyph3d device"),
            required_features,
            required_limits: limits.clone(),
            ..Default::default()
        })
        .await
        .expect("device request failed");

    // Stage L (O1): dedup uncaptured errors (see ErrorTracker above). NOTE:
    // this replaces wgpu 30's panic-on-first-error default with
    // log-once-and-continue — the rerun-style behavior, and a deliberate,
    // recorded semantic change (the byte-equal gates remain the
    // pixel-correctness net).
    {
        let tracker = std::sync::Mutex::new(ErrorTracker::default());
        device.on_uncaptured_error(std::sync::Arc::new(move |err| {
            ErrorTracker::handle(&tracker, &err);
        }));
    }

    log::info!(
        "device limits: max_storage_buffer_binding_size={} ({} MiB) max_buffer_size={} ({} MiB)",
        limits.max_storage_buffer_binding_size,
        limits.max_storage_buffer_binding_size >> 20,
        limits.max_buffer_size,
        limits.max_buffer_size >> 20,
    );

    // Debug groups off: they only label captures and we want the smallest
    // possible footprint on the encode path.
    let profiler = if profiling_supported {
        match wgpu_profiler::GpuProfiler::new(
            &device,
            wgpu_profiler::GpuProfilerSettings {
                enable_timer_queries: true,
                enable_debug_groups: false,
                // Higher than the suggested 2-4: offscreen runs submit many
                // frames before their query maps complete (CPU outpaces the
                // GPU in short verification runs), and frames beyond the cap
                // are dropped unmeasured. Windowed drains every frame, so it
                // never approaches this. Cost is a few tiny query buffers.
                max_num_pending_frames: 64,
            },
        ) {
            Ok(p) => {
                log::info!("GLYPH_PROFILE: per-pass GPU timings enabled");
                Some(RefCell::new(p))
            }
            Err(e) => {
                log::warn!("profiler creation failed ({e}); running unprofiled");
                None
            }
        }
    } else {
        None
    };

    GpuContext {
        instance,
        adapter,
        device,
        queue,
        profiler,
        cpu_scopes: RefCell::new(std::collections::BTreeMap::new()),
    }
}

// ── Stage L (O1): ErrorTracker unit tests (pure structure — no GPU) ──────
#[cfg(test)]
mod error_tracker_tests {
    use super::*;

    #[test]
    fn first_occurrence_logs_then_repeats_count() {
        let mut t = ErrorTracker::default();
        assert_eq!(t.track(ErrorKind::Validation, "bad buffer"), TrackDecision::First);
        assert_eq!(t.track(ErrorKind::Validation, "bad buffer"), TrackDecision::Repeat(2));
        assert_eq!(t.track(ErrorKind::Validation, "bad buffer"), TrackDecision::Repeat(3));
    }

    #[test]
    fn identity_is_kind_plus_description() {
        let mut t = ErrorTracker::default();
        // Same description, different kind → a distinct error.
        assert_eq!(t.track(ErrorKind::Validation, "boom"), TrackDecision::First);
        assert_eq!(t.track(ErrorKind::Internal, "boom"), TrackDecision::First);
        // Same kind, different description → a distinct error.
        assert_eq!(t.track(ErrorKind::Validation, "other"), TrackDecision::First);
        // And the originals still count as repeats.
        assert_eq!(t.track(ErrorKind::Validation, "boom"), TrackDecision::Repeat(2));
    }

    #[test]
    fn milestones_are_powers_of_ten_starting_at_10() {
        let mut t = ErrorTracker::default();
        let mut milestones = Vec::new();
        for _ in 0..12_000 {
            if let TrackDecision::Repeat(n) = t.track(ErrorKind::Validation, "spam") {
                if is_count_milestone(n) {
                    milestones.push(n);
                }
            }
        }
        assert_eq!(milestones, [10, 100, 1_000, 10_000]);
    }

    #[test]
    fn milestone_helper_edges() {
        assert!(!is_count_milestone(0));
        assert!(!is_count_milestone(1));
        assert!(!is_count_milestone(9));
        assert!(is_count_milestone(10));
        assert!(!is_count_milestone(11));
        assert!(is_count_milestone(100));
        assert!(!is_count_milestone(999));
        assert!(is_count_milestone(1_000));
    }
}
