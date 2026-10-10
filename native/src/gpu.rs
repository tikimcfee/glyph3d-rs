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
//! `ErrorTracker` below. (O1 follow-up, owner review: every emitted line
//! carries the greppable `[GPU-ERROR]` marker token.)

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

/// Stage L (O1 follow-up, owner review): render the log line for a track
/// decision — pure, so the exact wording is unit-tested. Every line carries
/// the greppable `[GPU-ERROR]` marker; the first-occurrence line says how
/// repeats are handled so a lone line can't hide a storm.
fn render_log_line(kind: ErrorKind, desc: &str, decision: &TrackDecision) -> Option<String> {
    match decision {
        TrackDecision::First => Some(format!(
            "[GPU-ERROR] wgpu {kind:?} error (first occurrence — repeats are counted \
             and summarized at 10/100/1000…x, not spammed): {desc}"
        )),
        TrackDecision::Repeat(n) if is_count_milestone(*n) => Some(format!(
            "[GPU-ERROR] wgpu {kind:?} error has now occurred {n}x (summary; see the \
             first-occurrence line above for the full context): {desc}"
        )),
        TrackDecision::Repeat(_) => None,
    }
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
        let decision = tracker
            .lock()
            .expect("ErrorTracker mutex poisoned")
            .track(kind, &desc);
        if let Some(line) = render_log_line(kind, &desc, &decision) {
            log::error!("{line}");
        }
    }
}

// ── the hardware profile ─────────────────────────────────────────────────

/// Hardware memory architecture of the GPU device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryArchitecture {
    /// Unified Memory Architecture (UMA):
    /// CPU and GPU share the same physical DRAM pool (e.g. Apple Silicon Metal, AMD APUs).
    /// Direct host-visible storage buffers can be written by CPU and read by GPU shaders
    /// without PCIe bus transfer or staging buffers.
    Unified,
    /// Discrete GPU Architecture (NUMA):
    /// GPU has dedicated high-bandwidth VRAM (GDDR6/GDDR7/HBM, 1-2 TB/s) connected via PCIe bus
    /// (e.g. NVIDIA RTX 5090, AMD Radeon discrete).
    /// High-speed shader execution requires device-local VRAM (`STORAGE | COPY_DST`).
    /// Host upload coordinates through mapped host staging buffers (`MAP_WRITE | COPY_SRC`)
    /// followed by PCIe DMA transfers.
    Discrete,
}

/// What this process is rendering on, resolved ONCE from the adapter wgpu
/// picked and carried in `GpuContext` so nothing downstream re-derives it.
///
/// Two consumers today, and the shape is meant to grow. The golden-view
/// key (`key()`) selects which byte-exact baseline set the pixel gate
/// compares against — the baselines were Metal renders, and the first Linux
/// run (2026-09-07) showed NVIDIA's Vulkan rasterizer flips isolated edge
/// pixels against them while every numeric gate stays bit-exact, so a golden
/// set is a property of the rasterizer, not of the tree. `render_text()` is
/// the provenance record committed beside that set (`ADAPTER.txt`).
///
/// The key is DELIBERATELY coarser than the record: `backend-vendor` treats
/// every NVIDIA card under Vulkan as one rasterizer until a diff proves
/// otherwise, at which point the record beside the baselines names exactly
/// which device and driver produced them, and escalating the key to device
/// level is a change to `key()` alone. Rendering paths that need to branch on
/// hardware (present mode, indirect-draw support, the Metal
/// `first_instance` workaround in glyph_scene.rs) should read this struct,
/// not `cfg!(target_os)`: the OS is the wrong axis for every one of those.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuProfile {
    pub backend: wgpu::Backend,
    pub vendor_id: u32,
    pub device_id: u32,
    pub device_name: String,
    pub device_type: wgpu::DeviceType,
    pub driver: String,
    pub driver_info: String,
    pub target_os: &'static str,
    pub target_arch: &'static str,
    pub multi_draw_indirect_count: bool,
    pub timestamp_query: bool,
    /// MAP_WRITE on storage-class buffers (unified-memory direct upload). On
    /// Metal this is strictly a win (shared storage IS the same DRAM the GPU
    /// reads); on discrete adapters wgpu still advertises it but a host-visible
    /// storage buffer trades the one fast upload for slower per-frame shader
    /// reads, so the upload path reads `backend` as well, not just this flag.
    pub mappable_primary_buffers: bool,
    pub max_storage_buffer_binding_size: u64,
    pub max_buffer_size: u64,
}

impl GpuProfile {
    pub fn memory_architecture(&self) -> MemoryArchitecture {
        if self.device_type == wgpu::DeviceType::IntegratedGpu
            || (self.backend == wgpu::Backend::Metal && self.vendor_slug() == "apple")
        {
            MemoryArchitecture::Unified
        } else {
            MemoryArchitecture::Discrete
        }
    }

    pub fn from_adapter(adapter: &wgpu::Adapter) -> Self {
        let info = adapter.get_info();
        let feats = adapter.features();
        let lim = adapter.limits();
        Self {
            backend: info.backend,
            vendor_id: info.vendor,
            device_id: info.device,
            device_name: info.name,
            device_type: info.device_type,
            driver: info.driver,
            driver_info: info.driver_info,
            target_os: std::env::consts::OS,
            target_arch: std::env::consts::ARCH,
            multi_draw_indirect_count: feats.contains(wgpu::Features::MULTI_DRAW_INDIRECT_COUNT),
            timestamp_query: feats.contains(wgpu::Features::TIMESTAMP_QUERY),
            mappable_primary_buffers: feats.contains(wgpu::Features::MAPPABLE_PRIMARY_BUFFERS),
            max_storage_buffer_binding_size: lim.max_storage_buffer_binding_size,
            max_buffer_size: lim.max_buffer_size,
        }
    }

    /// PCI vendor id → a name that can be a directory.
    ///
    /// AN UNRECOGNISED ID CANNOT STAND ALONE, and this comment used to claim it
    /// could ("two unknowns never collide on their hex id"). That is true of two
    /// DIFFERENT unknown ids and false of the one that matters: `0x0000` is not
    /// an id, it is "the driver declined to answer", so every adapter that
    /// declines lands on it. Two unrelated parts would then share a golden set,
    /// and the gate would report DIVERGES — blaming the renderer for what is
    /// really a hardware mismatch, which is worse than saying nothing. So the
    /// device NAME joins the key whenever the id is unrecognised: it is the only
    /// stable identifier left when the numeric one is absent.
    ///
    /// APPLE REPORTS NO VENDOR ID. Measured on an M2, 2026-09-07: wgpu's Metal
    /// backend gives `vendor=0x0000 device=0x0000 driver=""`. The id is ABSENT,
    /// not unknown, so the 0x106b arm below is unreachable on real hardware and
    /// the key came out `metal-vendor0000` — leaving the `metal-apple` golden
    /// set unreachable on the very machine that produced it. The name is what
    /// actually identifies the rasterizer there, so it is the fallback.
    ///
    /// Not folded into a blanket "Metal means Apple": Metal also runs on Intel
    /// Macs with AMD parts, and those are a different rasterizer that must not
    /// silently adopt this set. Such an adapter gets `vendor0000-<its name>`
    /// and the pixel gate then says it has no baseline — the honest answer
    /// rather than a wrong one, and a DIFFERENT one per part.
    pub fn vendor_slug(&self) -> String {
        match self.vendor_id {
            0x10de => "nvidia",
            0x1002 => "amd",
            0x8086 => "intel",
            0x106b => "apple",
            0x13b5 => "arm",
            0x5143 => "qualcomm",
            0x1414 => "microsoft",
            0x10005 => "mesa",
            0 if self.backend == wgpu::Backend::Metal
                && self.device_name.starts_with("Apple") =>
            {
                "apple"
            }
            other => {
                let name = Self::name_slug(&self.device_name);
                if name.is_empty() {
                    return format!("vendor{other:04x}");
                }
                return format!("vendor{other:04x}-{name}");
            }
        }
        .to_string()
    }

    /// A device name reduced to a path-safe slug: lowercase, runs of anything
    /// else collapsed to one `-`, trimmed, and capped so a chatty driver string
    /// cannot produce an unwieldy directory. Only used when the vendor id is
    /// unrecognised, where it is the sole thing separating two adapters.
    fn name_slug(name: &str) -> String {
        let mut out = String::new();
        for c in name.chars() {
            if c.is_ascii_alphanumeric() {
                out.push(c.to_ascii_lowercase());
            } else if !out.ends_with('-') && !out.is_empty() {
                out.push('-');
            }
            if out.len() >= 40 {
                break;
            }
        }
        out.trim_end_matches('-').to_string()
    }

    pub fn backend_slug(&self) -> &'static str {
        match self.backend {
            wgpu::Backend::Vulkan => "vulkan",
            wgpu::Backend::Metal => "metal",
            wgpu::Backend::Dx12 => "dx12",
            wgpu::Backend::Gl => "gl",
            wgpu::Backend::BrowserWebGpu => "webgpu",
            wgpu::Backend::Noop => "noop",
        }
    }

    /// The golden-set key: `<backend>-<vendor>`, filesystem-safe. See the
    /// struct doc for why this is coarser than the record.
    pub fn key(&self) -> String {
        format!("{}-{}", self.backend_slug(), self.vendor_slug())
    }

    /// The provenance record: one `field: value` per line, key first. This is
    /// what `--gpu-profile` prints and what lives beside a golden set as
    /// ADAPTER.txt, so the pixel gate can say "the baselines were made on X,
    /// you are on Y" when the two differ.
    pub fn render_text(&self) -> String {
        format!(
            "key: {}\nbackend: {:?}\nvendor: 0x{:04x} ({})\ndevice: 0x{:04x} {} ({:?})\n\
             driver: {} {}\nhost: {} {}\nfeatures: multi_draw_indirect_count={} timestamp_query={}\n\
             limits: max_storage_buffer_binding_size={} max_buffer_size={}\n",
            self.key(),
            self.backend,
            self.vendor_id,
            self.vendor_slug(),
            self.device_id,
            self.device_name,
            self.device_type,
            self.driver,
            self.driver_info,
            self.target_os,
            self.target_arch,
            self.multi_draw_indirect_count,
            self.timestamp_query,
            self.max_storage_buffer_binding_size,
            self.max_buffer_size,
        )
    }

    #[cfg(test)]
    fn synthetic(backend: wgpu::Backend, vendor_id: u32) -> Self {
        Self::synthetic_named(backend, vendor_id, "test")
    }

    #[cfg(test)]
    fn synthetic_named(backend: wgpu::Backend, vendor_id: u32, device_name: &str) -> Self {
        Self {
            backend,
            vendor_id,
            device_id: 0,
            device_name: device_name.into(),
            device_type: wgpu::DeviceType::Other,
            driver: String::new(),
            driver_info: String::new(),
            target_os: "test",
            target_arch: "test",
            multi_draw_indirect_count: false,
            timestamp_query: false,
            mappable_primary_buffers: false,
            max_storage_buffer_binding_size: 0,
            max_buffer_size: 0,
        }
    }
}

/// Logged adapter identity, kept around for diagnostics.
pub struct GpuContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    /// The hardware this context was created on; see `GpuProfile`.
    pub profile: GpuProfile,
    /// Stage H: per-frame GPU pass profiler. `Some` only when GLYPH_PROFILE=1
    /// AND the adapter supports TIMESTAMP_QUERY. RefCell because scenes render
    /// through `&GpuContext` while the profiler holds per-frame mutable state.
    pub profiler: Option<RefCell<wgpu_profiler::GpuProfiler>>,
    /// Stage H: CPU-side scope times (e.g. the cull pass), merged into the
    /// profile summary. Written by scenes only when `profiler` is `Some`.
    pub cpu_scopes: RefCell<std::collections::BTreeMap<String, (f64, u64)>>,
    pub prefetched_walk: std::sync::Arc<std::sync::Mutex<Option<std::thread::JoinHandle<crate::repo::PrefetchedRepo>>>>,
    pub prefetched_atlas: std::sync::Arc<std::sync::Mutex<Option<std::thread::JoinHandle<crate::atlas::Atlas>>>>,
}

/// The renderer's device context handles passed across layout stages.
/// The wgpu handles clone as cheap Arcs.
#[derive(Clone)]
pub struct SharedDevice {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub max_buffer_size: u64,
    pub host_visible_storage: bool,
    pub memory_arch: MemoryArchitecture,
    pub prefetched_walk: std::sync::Arc<std::sync::Mutex<Option<std::thread::JoinHandle<crate::repo::PrefetchedRepo>>>>,
    pub prefetched_atlas: std::sync::Arc<std::sync::Mutex<Option<std::thread::JoinHandle<crate::atlas::Atlas>>>>,
}

impl SharedDevice {
    pub fn from_ctx(ctx: &GpuContext) -> Self {
        let memory_arch = ctx.profile.memory_architecture();
        Self {
            instance: ctx.instance.clone(),
            adapter: ctx.adapter.clone(),
            device: ctx.device.clone(),
            queue: ctx.queue.clone(),
            max_buffer_size: ctx.profile.max_buffer_size,
            host_visible_storage: memory_arch == MemoryArchitecture::Unified
                && ctx.profile.mappable_primary_buffers,
            memory_arch,
            prefetched_walk: ctx.prefetched_walk.clone(),
            prefetched_atlas: ctx.prefetched_atlas.clone(),
        }
    }

    #[inline(always)]
    pub fn is_unified(&self) -> bool {
        self.memory_arch == MemoryArchitecture::Unified
    }

    #[inline(always)]
    pub fn is_discrete(&self) -> bool {
        self.memory_arch == MemoryArchitecture::Discrete
    }
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
    let profile = GpuProfile::from_adapter(&adapter);
    log::info!("gpu profile key: {}", profile.key());

    // Stage E2: a repo-scale glyph arena can exceed the default 128 MiB
    // storage binding by an order of magnitude (48 B × tens of millions of
    // glyphs). Request the adapter's FULL headroom; the scene chunks the
    // arena so no single binding exceeds max_storage_buffer_binding_size, so
    // any adapter value works — but the bigger the limit, the fewer chunks.
    // max_storage_buffers_per_shader_stage and the compute-grid cap are
    // requested at the adapter's value too (the retired CubeCL chain needed
    // them; asking costs nothing and leaves headroom for compute work).
    let supported = adapter.limits();
    let limits = wgpu::Limits {
        max_storage_buffer_binding_size: supported.max_storage_buffer_binding_size,
        max_buffer_size: supported.max_buffer_size,
        max_storage_buffers_per_shader_stage: supported.max_storage_buffers_per_shader_stage,
        max_compute_workgroups_per_dimension: supported.max_compute_workgroups_per_dimension,
        ..Default::default()
    };

    // Stage F: multi_draw_indirect is CORE in wgpu 30 (per-chunk indirect
    // draws for the cull pass need no feature gate). Log the count variant
    // for diagnostics only — the fixed-slot args scheme doesn't use it.
    log::info!(
        "adapter features: MULTI_DRAW_INDIRECT_COUNT={}",
        adapter.features().contains(wgpu::Features::MULTI_DRAW_INDIRECT_COUNT)
    );

    // Stage H: TIMESTAMP_QUERY is requested whenever the adapter has it, so
    // GPU-side timing is available without recreating the device. The
    // renderer's own in-pass scopes
    // (INSIDE_PASSES) stay opt-in behind GLYPH_PROFILE=1; a missing feature
    // must never break a render.
    let profile_wanted = std::env::var_os("GLYPH_PROFILE").is_some();
    let adapter_features = adapter.features();
    let mut required_features = wgpu::Features::empty();
    if adapter_features.contains(wgpu::Features::TIMESTAMP_QUERY) {
        required_features |= wgpu::Features::TIMESTAMP_QUERY;
    } else {
        log::warn!("timestamp queries unavailable: GPU-side timing falls back to the system clock");
    }
    if profile_wanted && adapter_features.contains(wgpu::Features::TIMESTAMP_QUERY_INSIDE_PASSES) {
        required_features |= wgpu::Features::TIMESTAMP_QUERY_INSIDE_PASSES;
    }
    // MAP_WRITE on storage buffers lets the instance upload write the device
    // buffer directly instead of wgpu's zero-fill-then-stage-then-blit path
    // (~22% of the repo-load profile). Requested wherever the adapter offers
    // it; the upload path still picks by backend (see GpuProfile's flag).
    if adapter_features.contains(wgpu::Features::MAPPABLE_PRIMARY_BUFFERS) {
        required_features |= wgpu::Features::MAPPABLE_PRIMARY_BUFFERS;
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
    let profiler = if profile_wanted {
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
        profile,
        profiler,
        cpu_scopes: RefCell::new(std::collections::BTreeMap::new()),
        prefetched_walk: std::sync::Arc::new(std::sync::Mutex::new(None)),
        prefetched_atlas: std::sync::Arc::new(std::sync::Mutex::new(None)),
    }
}

#[cfg(test)]
mod profile_tests {
    use super::*;

    /// The key is the directory name a golden set lives under, so it has to
    /// be stable across machines with the same rasterizer and safe as a path.
    #[test]
    fn key_is_backend_dash_vendor() {
        assert_eq!(GpuProfile::synthetic(wgpu::Backend::Metal, 0x106b).key(), "metal-apple");
        assert_eq!(GpuProfile::synthetic(wgpu::Backend::Vulkan, 0x10de).key(), "vulkan-nvidia");
        assert_eq!(GpuProfile::synthetic(wgpu::Backend::Vulkan, 0x1002).key(), "vulkan-amd");
    }

    /// THE SHAPE REAL APPLE HARDWARE REPORTS, which the synthetic 0x106b case
    /// above does not reach. wgpu's Metal backend gives vendor 0x0000 on an M2,
    /// so the golden set `metal-apple` was unreachable from the machine that
    /// made it — the key resolved to `metal-vendor0000` and the pixel gate said
    /// it had no baseline. The mapping test passed throughout, because it
    /// asserted the lookup and not the resolution.
    #[test]
    fn apple_metal_reports_no_vendor_id_and_still_keys_to_apple() {
        let real = GpuProfile::synthetic_named(wgpu::Backend::Metal, 0x0000, "Apple M2");
        assert_eq!(real.key(), "metal-apple");
    }

    /// The other half of that arm, and the reason it is not "Metal means
    /// Apple": Metal runs on Intel Macs with AMD parts, a different rasterizer
    /// that must not inherit Apple's golden set. It gets an unidentified key
    /// carrying its own name, and the gate then reports honestly that it has no
    /// baseline rather than diffing against Apple's frames.
    ///
    /// The assertion is on the PROPERTY, not the exact string: what matters is
    /// that it is not Apple's key. An earlier version pinned the literal
    /// `metal-vendor0000` and went red the moment the device name joined the
    /// key — the test was right to fail, and it was asserting more than it
    /// cared about.
    #[test]
    fn a_non_apple_metal_adapter_does_not_adopt_apples_set() {
        let amd = GpuProfile::synthetic_named(wgpu::Backend::Metal, 0x0000, "AMD Radeon Pro 5500M");
        let apple = GpuProfile::synthetic_named(wgpu::Backend::Metal, 0x0000, "Apple M2");
        assert_ne!(amd.key(), apple.key(), "a non-Apple Metal part must not adopt metal-apple");
        assert_eq!(apple.key(), "metal-apple");
        assert!(
            amd.key().starts_with("metal-vendor0000-"),
            "an unidentified Metal part keeps an unidentified key, got {:?}",
            amd.key(),
        );
    }

    /// An unknown vendor keeps its id rather than collapsing to a shared
    /// name — two unknowns must not share a golden set by accident.
    #[test]
    fn unknown_vendor_keeps_its_id() {
        let p = GpuProfile::synthetic_named(wgpu::Backend::Gl, 0xbeef, "Some Part");
        assert_eq!(p.key(), "gl-vendorbeef-some-part");
    }

    /// THE COLLISION 0x0000 CREATES, which the hex-id fallback alone does not
    /// prevent. `0x0000` is not an id, it is "declined to answer", so it is the
    /// value every unidentified adapter shares — two unrelated parts would key
    /// the same and the second would diff its pixels against the first's golden
    /// set, reporting DIVERGES as if the renderer had changed. The name is what
    /// separates them.
    #[test]
    fn two_adapters_that_report_no_vendor_id_do_not_share_a_key() {
        let amd = GpuProfile::synthetic_named(wgpu::Backend::Metal, 0, "AMD Radeon Pro 5500M");
        let intel = GpuProfile::synthetic_named(wgpu::Backend::Metal, 0, "Intel UHD Graphics 630");
        assert_ne!(amd.key(), intel.key(), "two unidentified adapters must not share a golden set");
        assert_eq!(amd.key(), "metal-vendor0000-amd-radeon-pro-5500m");
        assert_eq!(intel.key(), "metal-vendor0000-intel-uhd-graphics-630");
    }

    /// The slug has to survive being a directory name whatever the driver says.
    #[test]
    fn a_hostile_device_name_still_makes_one_safe_path_segment() {
        let p = GpuProfile::synthetic_named(
            wgpu::Backend::Vulkan,
            0,
            "  llvmpipe (LLVM 17.0.6, 256 bits) /../weird\name  ",
        );
        let k = p.key();
        assert!(
            k.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "key must be one safe path segment, got {k:?}",
        );
        assert!(!k.contains(".."), "key must not contain a traversal, got {k:?}");
        assert!(k.len() < 80, "key must stay a sane directory name, got {k:?}");
    }

    #[test]
    fn key_is_filesystem_safe_and_record_leads_with_it() {
        let p = GpuProfile::synthetic(wgpu::Backend::Dx12, 0x8086);
        assert!(p.key().chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        assert!(p.render_text().starts_with(&format!("key: {}\n", p.key())));
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

    #[test]
    fn log_lines_carry_the_gpu_error_marker() {
        // O1 follow-up (owner review): the emitted line must be unmistakable
        // and greppable; the first-occurrence line must say how repeats are
        // handled.
        let first = render_log_line(ErrorKind::Validation, "bad buffer", &TrackDecision::First)
            .expect("first occurrence logs");
        assert!(first.starts_with("[GPU-ERROR] wgpu Validation error"), "{first}");
        assert!(first.contains("first occurrence"), "{first}");
        assert!(first.contains("summarized"), "{first}");
        assert!(first.contains("bad buffer"), "{first}");

        // Ordinary repeats stay silent; milestones log with the count.
        assert!(render_log_line(ErrorKind::Validation, "bad buffer", &TrackDecision::Repeat(2))
            .is_none());
        let milestone =
            render_log_line(ErrorKind::Validation, "bad buffer", &TrackDecision::Repeat(100))
                .expect("milestone logs");
        assert!(milestone.starts_with("[GPU-ERROR]"), "{milestone}");
        assert!(milestone.contains("100x"), "{milestone}");
    }
}
