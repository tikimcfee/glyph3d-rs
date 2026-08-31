//! wgpu instance/adapter/device/queue init shared by windowed and offscreen modes.
//!
//! Future stages: call `init(None)` for headless work, or `init(Some(&surface))`
//! when a surface must influence adapter selection. Everything downstream only
//! needs `&GpuContext`.

use wgpu;

/// Logged adapter identity, kept around for diagnostics.
pub struct GpuContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

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
    let mut limits = wgpu::Limits::default();
    limits.max_storage_buffer_binding_size = supported.max_storage_buffer_binding_size;
    limits.max_buffer_size = supported.max_buffer_size;

    // Stage F: multi_draw_indirect is CORE in wgpu 30 (per-chunk indirect
    // draws for the cull pass need no feature gate). Log the count variant
    // for diagnostics only — the fixed-slot args scheme doesn't use it.
    log::info!(
        "adapter features: MULTI_DRAW_INDIRECT_COUNT={}",
        adapter.features().contains(wgpu::Features::MULTI_DRAW_INDIRECT_COUNT)
    );

    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("glyph3d device"),
            required_features: wgpu::Features::empty(),
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

    GpuContext {
        instance,
        adapter,
        device,
        queue,
    }
}
