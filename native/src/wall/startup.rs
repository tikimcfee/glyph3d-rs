//! Wall startup instrumentation (`GLYPH_WALL_STARTUP=1`), persistent GPU pipeline
//! caching (Vulkan), and non-blocking `g3d-termd` startup connect/list/adopt.
//!
//! Zero overhead when `GLYPH_WALL_STARTUP` is unset or `"0"`: `enabled()` reads
//! a cached `OnceLock<bool>` and every helper short-circuits without syscalls.
//! When enabled, each phase writes one line to stderr with wall-clock time since
//! `process_start` (`t=+...ms`) and phase duration (`dt=...ms`) for spans.

use std::collections::HashSet;
#[cfg(feature = "wgpu")]
use std::path::{Path, PathBuf};
#[cfg(any(feature = "wgpu", feature = "windowed"))]
use std::sync::Arc;
#[cfg(feature = "windowed")]
use std::sync::mpsc;
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

// ── Persistent wgpu PipelineCache (Vulkan; no-op on Metal in wgpu 30) ────────

#[cfg(feature = "wgpu")]
fn pipeline_cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("glyph3d");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    if cfg!(target_os = "macos") {
        home.join("Library/Caches/glyph3d")
    } else {
        home.join(".cache/glyph3d")
    }
}

/// Open a persisted `wgpu::PipelineCache` keyed by `wgpu::util::pipeline_cache_key`
/// when the device supports `wgpu::Features::PIPELINE_CACHE` (Vulkan `VkPipelineCache`).
///
/// On Metal, `wgpu 30` does not advertise `PIPELINE_CACHE` (Metal relies on macOS's
/// OS-level `MTLCompilerService` binary cache), so this returns `None` at zero cost.
#[cfg(feature = "wgpu")]
pub fn open_pipeline_cache(
    device: &wgpu::Device,
    adapter: &wgpu::Adapter,
) -> Option<(Arc<wgpu::PipelineCache>, PathBuf)> {
    if !device.features().contains(wgpu::Features::PIPELINE_CACHE) {
        return None;
    }
    let key = wgpu::util::pipeline_cache_key(&adapter.get_info())?;
    let path = pipeline_cache_dir().join(format!("wall-{key}"));
    let data = std::fs::read(&path).ok();
    // SAFETY: `fallback: true` instructs wgpu/Vulkan to ignore stale or invalid cache
    // bytes and build fresh pipelines rather than failing; the file is keyed by adapter
    // backend, vendor, device, driver, and driver_info via `pipeline_cache_key`.
    let cache = unsafe {
        device.create_pipeline_cache(&wgpu::PipelineCacheDescriptor {
            label: Some("wall pipeline cache"),
            data: data.as_deref(),
            fallback: true,
        })
    };
    Some((Arc::new(cache), path))
}

/// Persist updated `wgpu::PipelineCache` bytes atomically via rename.
#[cfg(feature = "wgpu")]
pub fn save_pipeline_cache(cache: &wgpu::PipelineCache, path: &Path) {
    let Some(bytes) = cache.get_data() else {
        return;
    };
    if bytes.is_empty() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    if std::fs::write(&tmp, &bytes).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

// ── Non-blocking g3d-termd connect / spawn / list / adopt ───────────────────

/// Result of connecting to `g3d-termd` and adopting any already-live terminals off the
/// window thread so the window never blocks Frame 1 on daemon startup.
#[cfg(feature = "windowed")]
pub struct TermdReady {
    pub sessions: term_session::Sessions,
    pub live_ids: Vec<String>,
    pub adopted: Vec<(String, u16, u16)>,
}

/// Handle for a background `g3d-termd` startup thread.
#[cfg(feature = "windowed")]
pub struct PendingTermd {
    rx: mpsc::Receiver<Result<TermdReady, String>>,
}

#[cfg(feature = "windowed")]
impl PendingTermd {
    /// Start connecting (or spawning `g3d-termd` if absent), listing, and adopting existing
    /// terminals on a background thread; calls `wake()` when ready.
    pub fn spawn(ns: String, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (tx, rx) = mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("wall-termd-connect".to_string())
            .spawn(move || {
                let res = connect_and_adopt(&ns, wake.clone());
                let _ = tx.send(res);
                wake();
            });
        Self { rx }
    }

    /// Non-blocking check for completion.
    pub fn try_take(&self) -> Option<Result<TermdReady, String>> {
        self.rx.try_recv().ok()
    }
}

#[cfg(feature = "windowed")]
fn connect_and_adopt(
    ns: &str,
    wake: Arc<dyn Fn() + Send + Sync>,
) -> Result<TermdReady, String> {
    let mut sessions = {
        let _s = span("termd_connect_spawn");
        term_session::Sessions::open(ns, wake)
            .map_err(|e| format!("the wall could not reach its terminals: {e}"))?
    };
    let live_ids = {
        let _s = span("termd_list");
        sessions.list().map_err(|e| e.to_string())?
    };
    let mut adopted = Vec::with_capacity(live_ids.len());
    {
        let _s = span("termd_adopt");
        for id in &live_ids {
            match sessions.adopt(id) {
                Ok((cols, rows)) => adopted.push((id.clone(), cols, rows)),
                Err(e) => log::error!("{e}"),
            }
        }
    }
    Ok(TermdReady {
        sessions,
        live_ids,
        adopted,
    })
}
