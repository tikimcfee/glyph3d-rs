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

use std::cell::RefCell;

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
    /// Stage K: the device was created with INDIRECT_FIRST_INSTANCE. The GPU
    /// cull's indirect draws carry `first_instance = chunk-local base`, and
    /// WebGPU only honors a nonzero first_instance in an indirect draw under
    /// this feature — wgpu's indirect-validation pass zeroes the draw
    /// otherwise (the Stage F "Metal bug"; src/bin/mdi_probe.rs is the proof).
    pub indirect_first_instance: bool,
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
    // Stage K: instance flags honor the WGPU_* environment (identical to the
    // build defaults when unset). The lever that matters here is
    // WGPU_VALIDATION_INDIRECT_CALL=0 — it drops wgpu's per-submit
    // indirect-validation compute pass, which the GPU cull's own compute
    // shader makes redundant (every slot it writes is a valid draw). Used by
    // the Stage K cost experiments; the default keeps validation ON.
    let flags = wgpu::InstanceFlags::from_env_or_default();
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        // PRIMARY backends on macOS resolve to Metal.
        backends: wgpu::Backends::PRIMARY,
        flags,
        // wgpu 30: InstanceDescriptor has no Default impl; start from the
        // no-display-handle constructor and override what we need.
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    log::info!("instance flags: {flags:?}");

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

    let adapter_features = adapter.features();
    let mut required_features = wgpu::Features::empty();

    // Stage K: multi_draw_indirect is core in wgpu 30 (Metal emulates it as a
    // loop of single indirect draws; the _COUNT variant is unimplemented
    // there — gfx-rs/wgpu#2148 — so the cull uses fixed slots with
    // zero-instance padding and never needs it). The feature that matters is
    // INDIRECT_FIRST_INSTANCE: without it WebGPU makes every indirect draw
    // with first_instance != 0 a no-op, and wgpu's indirect-validation pass
    // enforces exactly that. Stage F requested Features::empty() and read the
    // resulting silence as a Metal bug. Request it whenever the adapter has
    // it; GlyphScene keeps the CPU cull where it doesn't.
    let indirect_first_instance =
        adapter_features.contains(wgpu::Features::INDIRECT_FIRST_INSTANCE);
    if indirect_first_instance {
        required_features |= wgpu::Features::INDIRECT_FIRST_INSTANCE;
    }
    log::info!(
        "adapter features: INDIRECT_FIRST_INSTANCE={} MULTI_DRAW_INDIRECT_COUNT={}",
        indirect_first_instance,
        adapter_features.contains(wgpu::Features::MULTI_DRAW_INDIRECT_COUNT)
    );

    // Stage H: opt-in profiling. Request TIMESTAMP_QUERY (+ INSIDE_PASSES for
    // nested in-pass scopes) ONLY when GLYPH_PROFILE=1 and the adapter
    // supports it; a missing feature must never break a render.
    let profile_wanted = std::env::var_os("GLYPH_PROFILE").is_some();
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
        indirect_first_instance,
    }
}

/// Stage K: synchronous readback of `size` bytes at `offset` of `src` —
/// debug/verification paths only (it blocks until the GPU has drained).
/// Shared by the GLYPH_G_DUMP instance dump and `--cull-verify`.
pub fn read_buffer(ctx: &GpuContext, src: &wgpu::Buffer, offset: u64, size: u64) -> Vec<u8> {
    let buf = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = ctx.device.create_command_encoder(&Default::default());
    enc.copy_buffer_to_buffer(src, offset, &buf, 0, size);
    ctx.queue.submit([enc.finish()]);
    let slice = buf.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    ctx.device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .expect("readback poll failed");
    rx.recv().expect("readback callback dropped").expect("readback map failed");
    let data = slice
        .get_mapped_range()
        .expect("readback range not mapped")
        .to_vec();
    buf.unmap();
    data
}
