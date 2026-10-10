//! Device bring-up and the small helpers every pass shares. The adapter is
//! whatever `Backends::PRIMARY` picks (Vulkan here, Metal on the M2); GPU
//! times come from timestamp queries around each pass when the adapter has
//! them, else the frame wall alone is reported.

use std::time::Instant;

use wgpu::util::DeviceExt;

/// How many storage buffers the kernel binds (bytes ×4, items, segs, slots, advs, tables).
pub const KERNEL_STORAGE_BUFFERS: u32 = 9;

pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub ts_period: f32,
    pub timestamps: bool,
    pub adapter_info: wgpu::AdapterInfo,
    pub adapter_limits: wgpu::Limits,
}

pub fn ms(t: Instant) -> f64 { t.elapsed().as_secs_f64() * 1e3 }

pub fn init_gpu() -> Gpu {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::PRIMARY;
    let instance = wgpu::Instance::new(desc);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() })).expect("no adapter");
    let timestamps = adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY);
    let al = adapter.limits();
    assert!(al.max_storage_buffers_per_shader_stage >= KERNEL_STORAGE_BUFFERS, "adapter binds only {} storage buffers per stage; the kernel needs {}", al.max_storage_buffers_per_shader_stage, KERNEL_STORAGE_BUFFERS);
    let required_limits = wgpu::Limits {
        max_storage_buffer_binding_size: al.max_storage_buffer_binding_size,
        max_buffer_size: al.max_buffer_size,
        max_storage_buffers_per_shader_stage: KERNEL_STORAGE_BUFFERS.max(wgpu::Limits::default().max_storage_buffers_per_shader_stage),
        max_texture_dimension_2d: al.max_texture_dimension_2d,
        ..wgpu::Limits::default()
    };
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("jit-text"),
        required_features: if timestamps { wgpu::Features::TIMESTAMP_QUERY } else { wgpu::Features::empty() },
        required_limits,
        ..Default::default()
    })).expect("request_device");
    let ts_period = if timestamps { queue.get_timestamp_period() } else { 0.0 };
    Gpu { device, queue, ts_period, timestamps, adapter_info: adapter.get_info(), adapter_limits: al }
}

impl Gpu {
    pub fn wait(&self) { self.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll"); }

    pub fn submit_wait(&self, cb: wgpu::CommandBuffer) -> f64 {
        let t = Instant::now();
        self.queue.submit([cb]);
        self.wait();
        ms(t)
    }

    pub fn buffer(&self, label: &str, size: u64, usage: wgpu::BufferUsages) -> wgpu::Buffer {
        self.device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: size.max(4), usage, mapped_at_creation: false })
    }

    pub fn init_buffer(&self, label: &str, contents: &[u8], usage: wgpu::BufferUsages) -> wgpu::Buffer {
        if contents.is_empty() { return self.buffer(label, 4, usage) }
        self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some(label), contents, usage })
    }

    pub fn map_read(&self, buf: &wgpu::Buffer, size: u64) -> Vec<u8> {
        let (tx, rx) = std::sync::mpsc::channel();
        buf.slice(..size).map_async(wgpu::MapMode::Read, move |r| tx.send(r).unwrap());
        self.wait();
        rx.recv().unwrap().expect("map_async");
        let v = buf.slice(..size).get_mapped_range().expect("mapped range").to_vec();
        buf.unmap();
        v
    }

    /// Copy `size` bytes out of `src` and hand them back.
    pub fn read_back(&self, src: &wgpu::Buffer, size: u64) -> Vec<u8> {
        if size == 0 { return Vec::new() }
        let dst = self.buffer("readback", size, wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST);
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_buffer_to_buffer(src, 0, &dst, 0, size);
        self.submit_wait(enc.finish());
        self.map_read(&dst, size)
    }

    pub fn storage_entry(binding: u32, read_only: bool, stages: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
        wgpu::BindGroupLayoutEntry { binding, visibility: stages, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only }, has_dynamic_offset: false, min_binding_size: None }, count: None }
    }
    pub fn uniform_entry(binding: u32, stages: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
        wgpu::BindGroupLayoutEntry { binding, visibility: stages, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }, count: None }
    }

    /// Timestamp writes for a pass, if the adapter has them.
    pub fn compute_ts<'a>(&self, qs: &'a wgpu::QuerySet, begin: u32) -> Option<wgpu::ComputePassTimestampWrites<'a>> {
        self.timestamps.then_some(wgpu::ComputePassTimestampWrites { query_set: qs, beginning_of_pass_write_index: Some(begin), end_of_pass_write_index: Some(begin + 1) })
    }
    pub fn render_ts<'a>(&self, qs: &'a wgpu::QuerySet, begin: u32) -> Option<wgpu::RenderPassTimestampWrites<'a>> {
        self.timestamps.then_some(wgpu::RenderPassTimestampWrites { query_set: qs, beginning_of_pass_write_index: Some(begin), end_of_pass_write_index: Some(begin + 1) })
    }
}

/// `/proc/loadavg`, or `uptime`'s tail where there is no procfs (macOS).
pub fn loadavg() -> String {
    if let Ok(s) = std::fs::read_to_string("/proc/loadavg") { return s.trim().to_string() }
    std::process::Command::new("uptime").output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.split("load average").nth(1).map(|t| format!("load average{}", t.trim_end())))
        .unwrap_or_else(|| "n/a".into())
}

/// (min, median) of a sample set.
pub fn stats(v: &[f64]) -> (f64, f64) {
    if v.is_empty() { return (f64::NAN, f64::NAN) }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (s[0], s[s.len() / 2])
}
