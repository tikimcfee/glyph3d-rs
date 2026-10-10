//! jit-layout — headless measurement of "lay out only the visible lines, on the GPU".
//!
//! The status quo lays out a whole repo up front into one slot per glyph
//! (Derived mode: 20 B/slot). The hypothesis measured here: keep the source
//! BYTES (1 B/byte) and a 16 B per-line table resident on the GPU, and each
//! frame run a compute pass over just the visible lines (one invocation per
//! line, a serial fold over its bytes) into a transient slot buffer, then
//! draw from that. Every timing repeats `--repeat` times and reports min and
//! median because the GPU is shared.
//!
//! What this does NOT model: emoji/ZWJ sequences, the real atlas trie, Slug
//! coverage in the fragment stage, pagination, a camera (windows are random).
//!
//!     jit-layout <corpus-dir> [--visible-lines N] [--repeat K] [--long-line-bytes L]

use bytemuck::{Pod, Zeroable};
use std::path::{Path, PathBuf};
use std::time::Instant;
use wgpu::util::DeviceExt;

const SLOT_BYTES: u64 = 20;
const WRAP_COLS: u32 = 120;
const MAX_FILE_BYTES: u64 = 10 << 20;
const FILL_THREADS: usize = 8;
const TARGET: (u32, u32) = (1600, 1000);

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
struct Slot { x: f32, row: u32, glyph_and_wrap: u32, color: u32, item: u32 }
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Line { byte_start: u32, byte_len: u32, item: u32, row: u32 }
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vis { line_idx: u32, slot_base: u32 }
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Params { visible_count: u32, wrap_cols: u32, _pad: [u32; 2] }

// ---------------------------------------------------------------- CLI

struct Args { corpus: PathBuf, visible_lines: usize, repeat: usize, long_line_bytes: usize }

fn parse_args() -> Args {
    let mut a = Args { corpus: PathBuf::new(), visible_lines: 100_000, repeat: 5, long_line_bytes: 65_536 };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut num = |name: &str| it.next().unwrap_or_else(|| panic!("{name} needs a value")).parse::<usize>().unwrap_or_else(|e| panic!("{name}: {e}"));
        match arg.as_str() {
            "--visible-lines" => a.visible_lines = num("--visible-lines"),
            "--repeat" => a.repeat = num("--repeat"),
            "--long-line-bytes" => a.long_line_bytes = num("--long-line-bytes"),
            _ if a.corpus.as_os_str().is_empty() => a.corpus = PathBuf::from(arg),
            _ => panic!("unexpected argument {arg}"),
        }
    }
    assert!(!a.corpus.as_os_str().is_empty(), "usage: jit-layout <corpus-dir> [--visible-lines N] [--repeat K] [--long-line-bytes L]");
    assert!(a.repeat >= 1 && a.visible_lines >= 1);
    a
}

fn loadavg() -> String { std::fs::read_to_string("/proc/loadavg").map(|s| s.trim().to_string()).unwrap_or_else(|_| "n/a".into()) }

fn ms(t: Instant) -> f64 { t.elapsed().as_secs_f64() * 1e3 }

/// (min, median) of a sample set.
fn stats(v: &[f64]) -> (f64, f64) {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (s[0], s[s.len() / 2])
}

// ---------------------------------------------------------------- corpus

struct Corpus { bytes: Vec<u8>, lines: Vec<Line>, glyphs: Vec<u32>, files: usize, scan_ms: f64 }

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() { walk(&e.path(), out) } else if ft.is_file() { out.push(e.path()) }
    }
}

/// Every file < 10 MiB that is valid UTF-8, concatenated (each terminated by '\n'),
/// then the "Pass 1" the renderer does today: newline scan + per-line leading-byte count.
fn load_corpus(dir: &Path) -> Corpus {
    let mut paths = Vec::new();
    walk(dir, &mut paths);
    paths.sort();
    let mut bytes = Vec::new();
    let mut file_ranges = Vec::new();
    for p in &paths {
        let Ok(meta) = std::fs::metadata(p) else { continue };
        if meta.len() >= MAX_FILE_BYTES { continue }
        let Ok(data) = std::fs::read(p) else { continue };
        if std::str::from_utf8(&data).is_err() { continue }
        let start = bytes.len();
        bytes.extend_from_slice(&data);
        if bytes.last() != Some(&b'\n') { bytes.push(b'\n') }
        file_ranges.push(start..bytes.len());
    }
    let t = Instant::now();
    let mut lines = Vec::new();
    let mut glyphs = Vec::new();
    for (item, r) in file_ranges.iter().enumerate() {
        let mut line_start = r.start;
        let mut row = 0u32;
        for nl in memchr::memchr_iter(b'\n', &bytes[r.clone()]) {
            let end = r.start + nl;
            let n = bytes[line_start..end].iter().filter(|&&b| (b & 0xC0) != 0x80).count();
            lines.push(Line { byte_start: line_start as u32, byte_len: (end - line_start) as u32, item: item as u32, row });
            glyphs.push(n as u32);
            line_start = end + 1;
            row += 1;
        }
    }
    let scan_ms = ms(t);
    Corpus { bytes, lines, glyphs, files: file_ranges.len(), scan_ms }
}

fn advance_table() -> [f32; 256] {
    let mut t = [0.6f32; 256];
    for b in *b" il.,'" { t[b as usize] = 0.3 }
    for b in *b"mwMW" { t[b as usize] = 0.9 }
    t[b'\t' as usize] = 2.4;
    t
}

/// The CPU twin of layout.wgsl, step for step.
fn layout_line(bytes: &[u8], line: &Line, adv: &[f32; 256], out: &mut [Slot]) {
    let (mut i, end) = (line.byte_start as usize, (line.byte_start + line.byte_len) as usize);
    let (mut x, mut col, mut k) = (0.0f32, 0u32, 0usize);
    let (mut in_comment, mut prev_slash) = (false, false);
    while i < end {
        let b = bytes[i] as u32;
        let (glyph, a, len);
        if b < 0x80 {
            glyph = b;
            a = adv[b as usize];
            len = 1;
            if b == 0x2F { in_comment |= prev_slash; prev_slash = true } else { prev_slash = false }
        } else {
            let (l, mut cp) = if b >= 0xF0 { (4, b & 0x07) } else if b >= 0xE0 { (3, b & 0x0F) } else { (2, b & 0x1F) };
            for j in 1..l { cp = (cp << 6) | (bytes[i + j] as u32 & 0x3F) }
            glyph = cp & 0xFFFF;
            a = 1.0;
            len = l;
            prev_slash = false;
        }
        if col % WRAP_COLS == 0 { x = 0.0 }
        let seg = col / WRAP_COLS;
        out[k] = Slot { x, row: line.row, glyph_and_wrap: glyph | (seg << 16), color: if in_comment { 0xFF00FF00 } else { 0xFFFFFFFF }, item: line.item };
        x += a;
        col += 1;
        k += 1;
        i += len;
    }
}

/// Exclusive prefix over the window's glyph counts: the per-line slot base.
fn visible_list(corpus: &Corpus, window: &[u32]) -> (Vec<Vis>, usize, u64) {
    let (mut vis, mut base, mut bytes_read) = (Vec::with_capacity(window.len()), 0u32, 0u64);
    for &li in window {
        vis.push(Vis { line_idx: li, slot_base: base });
        base += corpus.glyphs[li as usize];
        bytes_read += corpus.lines[li as usize].byte_len as u64;
    }
    (vis, base as usize, bytes_read)
}

fn cpu_layout_serial(corpus: &Corpus, vis: &[Vis], adv: &[f32; 256], out: &mut [Slot]) {
    for v in vis {
        let n = corpus.glyphs[v.line_idx as usize] as usize;
        layout_line(&corpus.bytes, &corpus.lines[v.line_idx as usize], adv, &mut out[v.slot_base as usize..v.slot_base as usize + n]);
    }
}

fn cpu_layout_threads(corpus: &Corpus, vis: &[Vis], adv: &[f32; 256], out: &mut [Slot]) {
    let per = vis.len().div_ceil(FILL_THREADS).max(1);
    std::thread::scope(|s| {
        let mut rest = out;
        for chunk in vis.chunks(per) {
            let end = chunk.last().map(|v| v.slot_base as usize + corpus.glyphs[v.line_idx as usize] as usize).unwrap();
            let start = chunk[0].slot_base as usize;
            let (mine, tail) = std::mem::take(&mut rest).split_at_mut(end - start);
            rest = tail;
            s.spawn(move || {
                for v in chunk {
                    let n = corpus.glyphs[v.line_idx as usize] as usize;
                    let b = v.slot_base as usize - start;
                    layout_line(&corpus.bytes, &corpus.lines[v.line_idx as usize], adv, &mut mine[b..b + n]);
                }
            });
        }
    });
}

// ---------------------------------------------------------------- GPU

struct Gpu { device: wgpu::Device, queue: wgpu::Queue, ts_period: f32, adapter_info: wgpu::AdapterInfo }

fn init_gpu() -> Gpu {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::VULKAN;
    let instance = wgpu::Instance::new(desc);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() })).expect("no Vulkan adapter");
    assert!(adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY), "adapter lacks TIMESTAMP_QUERY");
    let al = adapter.limits();
    let required_limits = wgpu::Limits { max_storage_buffer_binding_size: al.max_storage_buffer_binding_size, max_buffer_size: al.max_buffer_size, ..wgpu::Limits::default() };
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("jit-layout"),
        required_features: wgpu::Features::TIMESTAMP_QUERY,
        required_limits,
        ..Default::default()
    })).expect("request_device");
    let ts_period = queue.get_timestamp_period();
    Gpu { device, queue, ts_period, adapter_info: adapter.get_info() }
}

impl Gpu {
    fn wait(&self) { self.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll"); }

    fn submit_wait(&self, cb: wgpu::CommandBuffer) -> f64 {
        let t = Instant::now();
        self.queue.submit([cb]);
        self.wait();
        ms(t)
    }

    fn buffer(&self, label: &str, size: u64, usage: wgpu::BufferUsages, mapped: bool) -> wgpu::Buffer {
        self.device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: size.max(4), usage, mapped_at_creation: mapped })
    }

    /// Copy `size` bytes out of `src` and hand them back (map_async + poll).
    fn read_back(&self, src: &wgpu::Buffer, size: u64) -> Vec<u8> {
        let dst = self.buffer("readback", size, wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST, false);
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_buffer_to_buffer(src, 0, &dst, 0, size);
        self.submit_wait(enc.finish());
        let (tx, rx) = std::sync::mpsc::channel();
        dst.slice(..size).map_async(wgpu::MapMode::Read, move |r| tx.send(r).unwrap());
        self.wait();
        rx.recv().unwrap().expect("map_async");
        let v = dst.slice(..size).get_mapped_range().expect("mapped range").to_vec();
        dst.unmap();
        v
    }

    fn storage_entry(binding: u32, read_only: bool, stages: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
        wgpu::BindGroupLayoutEntry { binding, visibility: stages, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only }, has_dynamic_offset: false, min_binding_size: None }, count: None }
    }
    fn uniform_entry(binding: u32, stages: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
        wgpu::BindGroupLayoutEntry { binding, visibility: stages, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }, count: None }
    }
}

/// Everything resident across frames: the corpus bytes + line table (the design's
/// whole GPU footprint), the transient slot buffer, both pipelines, the query set.
struct Resources {
    vis_buf: wgpu::Buffer, slots_buf: wgpu::Buffer, params_buf: wgpu::Buffer,
    compute: wgpu::ComputePipeline, compute_bg: wgpu::BindGroup,
    render: wgpu::RenderPipeline, render_bg: wgpu::BindGroup, target: wgpu::TextureView,
    query_set: wgpu::QuerySet, ts_resolve: wgpu::Buffer, ts_read: wgpu::Buffer,
}

fn build_resources(gpu: &Gpu, bytes_buf: &wgpu::Buffer, lines_buf: &wgpu::Buffer, adv: &[f32; 256], max_visible: usize, max_slots: usize) -> Resources {
    use wgpu::{BufferUsages as U, ShaderStages as S};
    let d = &gpu.device;
    let adv_buf = d.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("advances"), contents: bytemuck::cast_slice(adv), usage: U::STORAGE });
    let vis_buf = gpu.buffer("visible", (max_visible as u64) * 8, U::STORAGE | U::COPY_DST, false);
    let slots_buf = gpu.buffer("slots", (max_slots as u64) * SLOT_BYTES, U::STORAGE | U::COPY_SRC | U::COPY_DST, false);
    let params_buf = gpu.buffer("params", 16, U::UNIFORM | U::COPY_DST, false);
    // Camera: 120 columns of 0.6 across the width, 2000 rows of 1.2 down the height; rows past that fall off-screen.
    let cam: [f32; 4] = [2.0 / (WRAP_COLS as f32 * 0.6), 2.0 / 2400.0, -1.0, 1.0];
    let cam_buf = d.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("cam"), contents: bytemuck::bytes_of(&cam), usage: U::UNIFORM });

    let compute_module = d.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("layout.wgsl"), source: wgpu::ShaderSource::Wgsl(include_str!("layout.wgsl").into()) });
    let compute_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: None, entries: &[
        Gpu::storage_entry(0, true, S::COMPUTE), Gpu::storage_entry(1, true, S::COMPUTE), Gpu::storage_entry(2, true, S::COMPUTE),
        Gpu::storage_entry(3, true, S::COMPUTE), Gpu::storage_entry(4, false, S::COMPUTE), Gpu::uniform_entry(5, S::COMPUTE),
    ] });
    let compute_bg = d.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &compute_bgl, entries: &[
        wgpu::BindGroupEntry { binding: 0, resource: bytes_buf.as_entire_binding() },
        wgpu::BindGroupEntry { binding: 1, resource: lines_buf.as_entire_binding() },
        wgpu::BindGroupEntry { binding: 2, resource: vis_buf.as_entire_binding() },
        wgpu::BindGroupEntry { binding: 3, resource: adv_buf.as_entire_binding() },
        wgpu::BindGroupEntry { binding: 4, resource: slots_buf.as_entire_binding() },
        wgpu::BindGroupEntry { binding: 5, resource: params_buf.as_entire_binding() },
    ] });
    let compute_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[Some(&compute_bgl)], immediate_size: 0 });
    let compute = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some("layout"), layout: Some(&compute_layout), module: &compute_module, entry_point: Some("layout_lines"), compilation_options: Default::default(), cache: None });

    let draw_module = d.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("draw.wgsl"), source: wgpu::ShaderSource::Wgsl(include_str!("draw.wgsl").into()) });
    let render_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: None, entries: &[Gpu::storage_entry(0, true, S::VERTEX), Gpu::uniform_entry(1, S::VERTEX)] });
    let render_bg = d.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &render_bgl, entries: &[
        wgpu::BindGroupEntry { binding: 0, resource: slots_buf.as_entire_binding() },
        wgpu::BindGroupEntry { binding: 1, resource: cam_buf.as_entire_binding() },
    ] });
    let render_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[Some(&render_bgl)], immediate_size: 0 });
    let render = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("draw"), layout: Some(&render_layout),
        vertex: wgpu::VertexState { module: &draw_module, entry_point: Some("vs_main"), compilation_options: Default::default(), buffers: &[] },
        primitive: Default::default(), depth_stencil: None, multisample: Default::default(),
        fragment: Some(wgpu::FragmentState { module: &draw_module, entry_point: Some("fs_main"), compilation_options: Default::default(), targets: &[Some(wgpu::TextureFormat::Rgba8Unorm.into())] }),
        multiview_mask: None, cache: None,
    });
    let target = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("target"), size: wgpu::Extent3d { width: TARGET.0, height: TARGET.1, depth_or_array_layers: 1 },
        mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2, format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT, view_formats: &[],
    }).create_view(&Default::default());

    let query_set = d.create_query_set(&wgpu::QuerySetDescriptor { label: Some("ts"), ty: wgpu::QueryType::Timestamp, count: 4 });
    let ts_resolve = gpu.buffer("ts-resolve", 32, U::QUERY_RESOLVE | U::COPY_SRC, false);
    let ts_read = gpu.buffer("ts-read", 32, U::MAP_READ | U::COPY_DST, false);
    Resources { vis_buf, slots_buf, params_buf, compute, compute_bg, render, render_bg, target, query_set, ts_resolve, ts_read }
}

struct Frame { cpu_ms: f64, wall_ms: f64, compute_ms: f64, render_ms: f64, slots: usize, bytes_read: u64 }

/// One "frame": prefix + upload of the visible list, compute layout, draw; GPU
/// times from timestamp queries around each pass.
fn run_frame(gpu: &Gpu, r: &Resources, corpus: &Corpus, window: &[u32]) -> Frame {
    let t_cpu = Instant::now();
    let (vis, slots, bytes_read) = visible_list(corpus, window);
    gpu.queue.write_buffer(&r.vis_buf, 0, bytemuck::cast_slice(&vis));
    gpu.queue.write_buffer(&r.params_buf, 0, bytemuck::bytes_of(&Params { visible_count: vis.len() as u32, wrap_cols: WRAP_COLS, _pad: [0; 2] }));
    let cpu_ms = ms(t_cpu);

    let mut enc = gpu.device.create_command_encoder(&Default::default());
    {
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("layout"), timestamp_writes: Some(wgpu::ComputePassTimestampWrites { query_set: &r.query_set, beginning_of_pass_write_index: Some(0), end_of_pass_write_index: Some(1) }) });
        pass.set_pipeline(&r.compute);
        pass.set_bind_group(0, &r.compute_bg, &[]);
        pass.dispatch_workgroups((vis.len() as u32).div_ceil(64), 1, 1);
    }
    {
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("draw"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &r.target, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store } })],
            depth_stencil_attachment: None,
            timestamp_writes: Some(wgpu::RenderPassTimestampWrites { query_set: &r.query_set, beginning_of_pass_write_index: Some(2), end_of_pass_write_index: Some(3) }),
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&r.render);
        pass.set_bind_group(0, &r.render_bg, &[]);
        pass.draw(0..6, 0..slots as u32);
    }
    enc.resolve_query_set(&r.query_set, 0..4, &r.ts_resolve, 0);
    enc.copy_buffer_to_buffer(&r.ts_resolve, 0, &r.ts_read, 0, 32);
    let wall_ms = gpu.submit_wait(enc.finish());

    let (tx, rx) = std::sync::mpsc::channel();
    r.ts_read.slice(..).map_async(wgpu::MapMode::Read, move |res| tx.send(res).unwrap());
    gpu.wait();
    rx.recv().unwrap().expect("map ts");
    let ts: [u64; 4] = *bytemuck::from_bytes(&r.ts_read.slice(..).get_mapped_range().expect("mapped range")[..32]);
    r.ts_read.unmap();
    let tick = gpu.ts_period as f64 / 1e6;
    Frame { cpu_ms, wall_ms, compute_ms: ts[1].wrapping_sub(ts[0]) as f64 * tick, render_ms: ts[3].wrapping_sub(ts[2]) as f64 * tick, slots, bytes_read }
}

// ---------------------------------------------------------------- main

/// Deterministic window offsets (an LCG; reproducible across runs, no crate).
struct Lcg(u64);
impl Lcg {
    fn next(&mut self, n: usize) -> usize { self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); ((self.0 >> 33) as usize) % n.max(1) }
}

fn main() {
    let args = parse_args();
    println!("loadavg at start: {}", loadavg());

    // 1. Load + the CPU Pass 1 (newline scan, leading-byte counts).
    let t = Instant::now();
    let mut corpus = load_corpus(&args.corpus);
    let load_ms = ms(t);
    let total_glyphs: u64 = corpus.glyphs.iter().map(|&g| g as u64).sum();
    println!("corpus: {} files, {} bytes, {} lines, {} glyphs; read+concat {:.0} ms; line table + glyph counts {:.1} ms (1 thread, memchr newlines, byte loop for counts)",
        corpus.files, corpus.bytes.len(), corpus.lines.len(), total_glyphs, load_ms - corpus.scan_ms, corpus.scan_ms);

    // 5. The pathological line, appended as its own item; it enters a window only when asked.
    let long_idx = corpus.lines.len() as u32;
    let start = corpus.bytes.len();
    corpus.bytes.extend((0..args.long_line_bytes).map(|i| if i % 8 == 7 { b' ' } else { b'a' + (i % 26) as u8 }));
    corpus.bytes.push(b'\n');
    corpus.lines.push(Line { byte_start: start as u32, byte_len: args.long_line_bytes as u32, item: corpus.files as u32, row: 0 });
    corpus.glyphs.push(args.long_line_bytes as u32);
    while !corpus.bytes.len().is_multiple_of(4) { corpus.bytes.push(0) }
    let real_lines = long_idx as usize;

    // Window plan: N in {10k, 100k, 1M, --visible-lines}, `repeat` random contiguous windows each.
    let mut sizes = vec![10_000usize, 100_000, 1_000_000, args.visible_lines];
    sizes.sort();
    sizes.dedup();
    let mut rng = Lcg(0x9E3779B97F4A7C15);
    let plan: Vec<(usize, Vec<Vec<u32>>)> = sizes.iter().map(|&n| {
        let n = n.min(real_lines);
        (n, (0..args.repeat).map(|_| { let off = rng.next(real_lines - n + 1); (off as u32..(off + n) as u32).collect() }).collect())
    }).collect();
    let prefix = |w: &[u32]| w.iter().map(|&i| corpus.glyphs[i as usize] as u64).sum::<u64>();
    let max_slots = plan.iter().flat_map(|(_, ws)| ws.iter().map(|w| prefix(w))).max().unwrap() as usize + args.long_line_bytes;
    let max_visible = plan.iter().map(|(n, _)| *n).max().unwrap() + 1;

    let gpu = init_gpu();
    println!("adapter: {} ({:?}, driver {} {}); timestamp period {} ns", gpu.adapter_info.name, gpu.adapter_info.backend, gpu.adapter_info.driver, gpu.adapter_info.driver_info, gpu.ts_period);
    let lim = gpu.device.limits();
    assert!((max_slots as u64) * SLOT_BYTES <= lim.max_storage_buffer_binding_size, "largest window needs {} slot bytes, binding limit {}", max_slots as u64 * SLOT_BYTES, lim.max_storage_buffer_binding_size);

    // 2. Upload the resident set (bytes + line table), repeated.
    let (mut create_ms, mut upload_ms) = (vec![], vec![]);
    let mut resident = None;
    for _ in 0..args.repeat {
        drop(resident.take());
        gpu.wait();
        let t = Instant::now();
        let bytes_buf = gpu.device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("bytes"), contents: &corpus.bytes, usage: wgpu::BufferUsages::STORAGE });
        let lines_buf = gpu.device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("lines"), contents: bytemuck::cast_slice(&corpus.lines), usage: wgpu::BufferUsages::STORAGE });
        create_ms.push(ms(t));
        upload_ms.push(gpu.submit_wait(gpu.device.create_command_encoder(&Default::default()).finish()));
        resident = Some((bytes_buf, lines_buf));
    }
    let (bytes_buf, lines_buf) = resident.unwrap();
    let resident_bytes = corpus.bytes.len() as u64 + corpus.lines.len() as u64 * 16;

    // 2b. The status quo's data movement: a whole-corpus slot buffer, mapped at creation,
    //     filled by 8 threads, unmapped and copied to a device buffer.
    let whole_slots = total_glyphs * SLOT_BYTES;
    let (mut sq_create, mut sq_fill, mut sq_submit) = (vec![], vec![], vec![]);
    if whole_slots <= lim.max_buffer_size {
        for _ in 0..args.repeat {
            let t = Instant::now();
            let staging = gpu.buffer("staging", whole_slots, wgpu::BufferUsages::COPY_SRC, true);
            let dev = gpu.buffer("device-slots", whole_slots, wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::STORAGE, false);
            sq_create.push(ms(t));
            let t = Instant::now();
            {
                // wgpu 30's mapped range is write-only (`WriteOnly<[u8]>`); `fill(0)` is a memset, `split_at` hands threads disjoint pieces.
                let mut view = staging.slice(..).get_mapped_range_mut().expect("mapped range");
                let per = (view.len() / FILL_THREADS).max(1);
                // wgpu's `Send` impl for WriteOnly is `impl<T: Send>` without `?Sized`, so a
                // `WriteOnly<[u8]>` piece is not Send; disjoint pieces of one host mapping are
                // sound to fill from different threads, which is all this wrapper claims.
                struct Piece<'a>(wgpu::WriteOnly<'a, [u8]>);
                unsafe impl Send for Piece<'_> {}
                let mut rest = view.slice(..);
                std::thread::scope(|s| {
                    while !rest.is_empty() {
                        let cut = per.min(rest.len());
                        let (mine, tail) = rest.split_at(cut);
                        rest = tail;
                        let piece = Piece(mine);
                        s.spawn(move || { let mut piece = piece; piece.0.fill(0u8) });
                    }
                });
            }
            sq_fill.push(ms(t));
            let t = Instant::now();
            staging.unmap();
            let mut enc = gpu.device.create_command_encoder(&Default::default());
            enc.copy_buffer_to_buffer(&staging, 0, &dev, 0, whole_slots);
            gpu.queue.submit([enc.finish()]);
            gpu.wait();
            sq_submit.push(ms(t));
            drop((staging, dev));
            gpu.wait();
        }
    } else {
        println!("status-quo buffer of {whole_slots} B exceeds max_buffer_size {}; skipped", lim.max_buffer_size);
    }

    // 3/4/5. Frames.
    let adv = advance_table();
    let res = build_resources(&gpu, &bytes_buf, &lines_buf, &adv, max_visible, max_slots);
    let mut rows: Vec<(String, Vec<Frame>)> = Vec::new();
    for (n, windows) in &plan {
        let frames: Vec<Frame> = windows.iter().map(|w| run_frame(&gpu, &res, &corpus, w)).collect();
        rows.push((format!("N={n}"), frames));
        if *n == args.visible_lines.min(real_lines) {
            let frames: Vec<Frame> = windows.iter().map(|w| { let mut w = w.clone(); w.push(long_idx); run_frame(&gpu, &res, &corpus, &w) }).collect();
            rows.push((format!("N={n}+long({})", args.long_line_bytes), frames));
        }
    }

    // Verify: the compute output of one window against the CPU twin, every slot.
    let verify_window = &plan.iter().find(|(n, _)| *n == args.visible_lines.min(real_lines)).unwrap().1[0];
    let f = run_frame(&gpu, &res, &corpus, verify_window);
    let gpu_slots: Vec<Slot> = bytemuck::cast_slice(&gpu.read_back(&res.slots_buf, f.slots as u64 * SLOT_BYTES)).to_vec();
    let (vis, n_slots, _) = visible_list(&corpus, verify_window);
    let mut cpu_slots = vec![Slot::zeroed(); n_slots];
    cpu_layout_serial(&corpus, &vis, &adv, &mut cpu_slots);
    let mismatch = gpu_slots.iter().zip(&cpu_slots).position(|(a, b)| a != b);
    let verdict = match mismatch {
        None => format!("PASS: {} slots bit-equal GPU vs CPU (first 64 included)", n_slots),
        Some(i) => format!("FAIL: first mismatch at slot {i}: gpu {:?} cpu {:?} ({} differ)", gpu_slots[i], cpu_slots[i], gpu_slots.iter().zip(&cpu_slots).filter(|(a, b)| a != b).count()),
    };

    // 6. The CPU alternative over the same windows: lay out on the CPU, upload, poll.
    let mut cpu_rows: Vec<(usize, Vec<f64>, Vec<f64>, Vec<f64>)> = Vec::new();
    for (n, windows) in &plan {
        let (mut serial_ms, mut threads_ms, mut cpu_upload_ms) = (vec![], vec![], vec![]);
        for w in windows {
            let (vis, n_slots, _) = visible_list(&corpus, w);
            let mut out = vec![Slot::zeroed(); n_slots];
            let t = Instant::now();
            cpu_layout_serial(&corpus, &vis, &adv, &mut out);
            serial_ms.push(ms(t));
            let t = Instant::now();
            cpu_layout_threads(&corpus, &vis, &adv, &mut out);
            threads_ms.push(ms(t));
            let t = Instant::now();
            gpu.queue.write_buffer(&res.slots_buf, 0, bytemuck::cast_slice(&out));
            gpu.queue.submit([]);
            gpu.wait();
            cpu_upload_ms.push(ms(t));
        }
        cpu_rows.push((*n, serial_ms, threads_ms, cpu_upload_ms));
    }
    // The long line alone on one CPU thread, for contrast with its one GPU thread.
    let long_cpu_ms: Vec<f64> = (0..args.repeat).map(|_| {
        let mut out = vec![Slot::zeroed(); args.long_line_bytes];
        let t = Instant::now();
        layout_line(&corpus.bytes, &corpus.lines[long_idx as usize], &adv, &mut out);
        ms(t)
    }).collect();

    // ---------------------------------------------------------------- report
    let mm = |v: &[f64]| { let (a, b) = stats(v); format!("{a:9.3} {b:9.3}") };
    println!();
    println!("=== jit-layout: {} files, {} B source, {} lines, {} glyphs; {} repeats; min / median in ms ===", corpus.files, corpus.bytes.len(), real_lines, total_glyphs, args.repeat);
    println!("{:<44} {:>9} {:>9}", "measurement", "min", "median");
    println!("{:<44} {}", "CPU line table + glyph counts (1 thread)".to_string(), format!("{:9.3} {:9.3}", corpus.scan_ms, corpus.scan_ms));
    println!("{:<44} {}", format!("resident upload: create_buffer_init ({} MB)", resident_bytes / 1_000_000), mm(&create_ms));
    println!("{:<44} {}", "resident upload: submit + poll", mm(&upload_ms));
    if !sq_fill.is_empty() {
        println!("{:<44} {}", format!("status quo: create staging+device ({} MB)", whole_slots / 1_000_000), mm(&sq_create));
        println!("{:<44} {}", "status quo: 8-thread zero fill of staging", mm(&sq_fill));
        println!("{:<44} {}", "status quo: unmap + copy + submit + poll", mm(&sq_submit));
    }
    for (name, frames) in &rows {
        let c: Vec<f64> = frames.iter().map(|f| f.compute_ms).collect();
        let r: Vec<f64> = frames.iter().map(|f| f.render_ms).collect();
        let cpu: Vec<f64> = frames.iter().map(|f| f.cpu_ms).collect();
        let wall: Vec<f64> = frames.iter().map(|f| f.wall_ms).collect();
        let slots: Vec<f64> = frames.iter().map(|f| f.slots as f64).collect();
        let bytes: Vec<f64> = frames.iter().map(|f| f.bytes_read as f64).collect();
        let (_, med_slots) = stats(&slots);
        let (_, med_bytes) = stats(&bytes);
        let (min_c, med_c) = stats(&c);
        println!("{:<44} {}   slots {:>10.0}, bytes read {:>10.0} (median); {:.1} / {:.1} G glyphs/s at min / median", format!("{name}: compute pass GPU"), mm(&c), med_slots, med_bytes, med_slots / min_c / 1e6, med_slots / med_c / 1e6);
        println!("{:<44} {}", format!("{name}: render pass GPU (flat colour)"), mm(&r));
        println!("{:<44} {}", format!("{name}: CPU prefix + write_buffer"), mm(&cpu));
        println!("{:<44} {}", format!("{name}: frame wall (submit..poll)"), mm(&wall));
    }
    for (n, serial_ms, threads_ms, cpu_upload_ms) in &cpu_rows {
        println!("{:<44} {}", format!("CPU alt N={n}: layout 1 thread"), mm(serial_ms));
        println!("{:<44} {}", format!("CPU alt N={n}: layout {FILL_THREADS} threads"), mm(threads_ms));
        println!("{:<44} {}", format!("CPU alt N={n}: write_buffer + submit + poll"), mm(cpu_upload_ms));
    }
    println!("{:<44} {}", format!("CPU: the {} B long line alone, 1 thread", args.long_line_bytes), mm(&long_cpu_ms));
    println!();
    println!("resident GPU footprint: {} MB (bytes + 16 B/line) vs status quo {} MB (20 B/glyph); the transient slot buffer here was {} MB for the largest window",
        resident_bytes / 1_000_000, whole_slots / 1_000_000, max_slots as u64 * SLOT_BYTES / 1_000_000);
    println!("readback check: {verdict}");
    println!("not measured: Slug coverage in the fragment stage (identical for both designs), emoji/ZWJ sequences, the atlas trie, pagination, a real camera (windows are random contiguous line ranges), depth test.");
    println!("loadavg at end: {}", loadavg());
}
