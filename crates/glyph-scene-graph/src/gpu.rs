//! The GPU mirror of the node tables and the passes that keep it current.
//!
//! Buffers, one row per pool slot: `local` (32 B), `post` (16 B),
//! `appearance` (32 B) and `topo` (8 B: parent, output group row), written
//! only by the host; `order` (4 B per depth-first position), written by the
//! host when structure changes; and `world` (32 B), written only by the
//! resolve pass — the two writers never share a buffer.
//!
//! Per frame ([`SceneGraphGpu::flush`], into the caller's encoder):
//! 1. [`NodeTables::prepare_frame`] closes the frame's edits into a plan;
//! 2. each table's dirty rows go up by [`plan_upload`]'s choice — coalesced
//!    `write_buffer` runs, one staged batch for the scatter pass, or the whole
//!    table; a changed span of the order goes up as one write;
//! 3. one compute pass: the scatter dispatches, then the resolve over the
//!    plan's ranges (wgpu orders dispatches that touch the same buffer).
//!
//! Nothing dirty, nothing dispatched: a settled scene costs one empty plan.
//! One flush per submit: the scatter's staging buffers are written by
//! `Queue::write_buffer`, which lands at the next submit, so two flushes in
//! one submit would scatter the second batch twice.

use std::time::Instant;

use bytemuck::{Pod, Zeroable};

use crate::tables::{FramePlan, NodeTables, MAX_DEPTH, MAX_RESOLVE_RANGES};
use crate::transform::{Appearance, PostScale, Similarity};
use crate::upload::{plan_upload, UploadPlan};

pub const RESOLVE_WGSL: &str = include_str!("../shaders/resolve.wgsl");
pub const SCATTER_WGSL: &str = include_str!("../shaders/scatter.wgsl");

const RESOLVE_WG: u32 = 64;
const SCATTER_WG: u32 = 256;
const MAX_GROUPS_X: u32 = 65_535;

/// The resolve pass's uniform: 528 B (`ResolveParams` in resolve.wgsl).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct ResolveParamsGpu {
    pub range_count: u32,
    pub total: u32,
    pub max_depth: u32,
    pub row_limit: u32,
    /// (dfs start, first thread) per range, two ranges per element.
    pub ranges: [[u32; 4]; MAX_RESOLVE_RANGES / 2],
}

/// The scatter pass's uniform: 16 B (`ScatterParams` in scatter.wgsl).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct ScatterParamsGpu {
    pub row_words: u32,
    pub rows: u32,
    pub _pad: [u32; 2],
}

/// What one flush did, for the bench and the renderer's instruments.
#[derive(Clone, Debug, Default)]
pub struct FlushStats {
    pub ranges: usize,
    /// Threads the resolve ran (nodes resolved, coalesced gaps included).
    pub resolved: u32,
    /// (table, upload mode, bytes) for local, post, appearance, topo.
    pub uploads: [(&'static str, &'static str, u64); 4],
    pub order_bytes: u64,
    pub params_bytes: u64,
    /// Every byte put on the queue: the tables, the order span, the
    /// scatter's metadata and the resolve's uniform.
    pub bytes: u64,
    pub dispatched: bool,
    /// The buffers grew this flush (everything re-uploaded and resolved).
    pub grew: bool,
    /// Host time: closing the plan (order rebuild, ranges, dirty rows).
    pub cpu_plan_us: f64,
    /// Host time: building and queueing the uploads and the pass.
    pub cpu_encode_us: f64,
}

struct Scatter {
    src: wgpu::Buffer,
    indices: wgpu::Buffer,
    params: wgpu::Buffer,
    bind_group: Option<wgpu::BindGroup>,
}

struct Table {
    name: &'static str,
    buffer: wgpu::Buffer,
    row_bytes: u64,
    scatter: Option<Scatter>,
}

struct Timing {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    readback: wgpu::Buffer,
    period_ns: f32,
    written: bool,
}

pub struct SceneGraphGpu {
    capacity: u32,
    tables: [Table; 4], // local, post, appearance, topo
    order: wgpu::Buffer,
    world: wgpu::Buffer,
    params: wgpu::Buffer,
    resolve_pipeline: wgpu::ComputePipeline,
    resolve_bgl: wgpu::BindGroupLayout,
    output_bgl: wgpu::BindGroupLayout,
    resolve_bg: wgpu::BindGroup,
    output_bg: wgpu::BindGroup,
    output_rows: u32,
    scatter_pipeline: wgpu::ComputePipeline,
    scatter_bgl: wgpu::BindGroupLayout,
    timing: Option<Timing>,
}

const LOCAL: usize = 0;
const POST: usize = 1;
const APPEARANCE: usize = 2;
const TOPO: usize = 3;

fn storage_entry(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
        count: None,
    }
}

fn bind(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry { binding, resource: buffer.as_entire_binding() }
}

fn storage(device: &wgpu::Device, label: &str, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: size.max(16),
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    })
}

/// Workgroups for `threads` at `wg` per group, folded into 2-D past the
/// per-dimension cap (the shaders linearise `x + y * nx * wg`).
fn grid(threads: u32, wg: u32) -> (u32, u32) {
    let groups = threads.div_ceil(wg).max(1);
    if groups <= MAX_GROUPS_X {
        (groups, 1)
    } else {
        (MAX_GROUPS_X, groups.div_ceil(MAX_GROUPS_X))
    }
}

impl SceneGraphGpu {
    pub fn new(device: &wgpu::Device) -> Self {
        let resolve_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scene graph resolve bgl"),
            entries: &[
                uniform_entry(0),
                storage_entry(1, true),
                storage_entry(2, true),
                storage_entry(3, true),
                storage_entry(4, true),
                storage_entry(5, true),
                storage_entry(6, false),
            ],
        });
        let output_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scene graph output bgl"),
            entries: &[storage_entry(0, false)],
        });
        let scatter_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scene graph scatter bgl"),
            entries: &[storage_entry(0, false), storage_entry(1, true), storage_entry(2, true), uniform_entry(3)],
        });
        let resolve_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("resolve.wgsl"),
            source: wgpu::ShaderSource::Wgsl(RESOLVE_WGSL.into()),
        });
        let scatter_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("scatter.wgsl"),
            source: wgpu::ShaderSource::Wgsl(SCATTER_WGSL.into()),
        });
        let resolve_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("scene graph resolve pl"),
            bind_group_layouts: &[Some(&resolve_bgl), Some(&output_bgl)],
            immediate_size: 0,
        });
        let scatter_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("scene graph scatter pl"),
            bind_group_layouts: &[Some(&scatter_bgl)],
            immediate_size: 0,
        });
        let pipeline = |layout: &wgpu::PipelineLayout, module: &wgpu::ShaderModule, entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(layout),
                module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let resolve_pipeline = pipeline(&resolve_pl, &resolve_module, "resolve");
        let scatter_pipeline = pipeline(&scatter_pl, &scatter_module, "scatter");
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("scene graph resolve params"),
            size: std::mem::size_of::<ResolveParamsGpu>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let dummy = storage(device, "scene graph no output", 16);
        let output_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scene graph no output bg"),
            layout: &output_bgl,
            entries: &[bind(0, &dummy)],
        });
        let capacity = 0;
        let make = |name: &'static str, row_bytes: u64| Table { name, buffer: storage(device, name, 16), row_bytes, scatter: None };
        let tables = [
            make("local", std::mem::size_of::<Similarity>() as u64),
            make("post", std::mem::size_of::<PostScale>() as u64),
            make("appearance", std::mem::size_of::<Appearance>() as u64),
            make("topo", 8),
        ];
        let order = storage(device, "scene graph order", 16);
        let world = storage(device, "scene graph world", 16);
        let resolve_bg = Self::resolve_bind_group(device, &resolve_bgl, &params, &tables, &order, &world);
        Self {
            capacity,
            tables,
            order,
            world,
            params,
            resolve_pipeline,
            resolve_bgl,
            output_bgl,
            resolve_bg,
            output_bg,
            output_rows: 0,
            scatter_pipeline,
            scatter_bgl,
            timing: None,
        }
    }

    fn resolve_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        params: &wgpu::Buffer,
        tables: &[Table; 4],
        order: &wgpu::Buffer,
        world: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scene graph resolve bg"),
            layout,
            entries: &[
                bind(0, params),
                bind(1, &tables[LOCAL].buffer),
                bind(2, &tables[POST].buffer),
                bind(3, &tables[APPEARANCE].buffer),
                bind(4, &tables[TOPO].buffer),
                bind(5, order),
                bind(6, world),
            ],
        })
    }

    /// Route nodes' group rows into `buffer` (96 B rows; `rows` of them).
    /// The transitional draw path: the renderer's group table.
    pub fn set_group_output(&mut self, device: &wgpu::Device, buffer: &wgpu::Buffer, rows: u32) {
        self.output_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scene graph group output bg"),
            layout: &self.output_bgl,
            entries: &[bind(0, buffer)],
        });
        self.output_rows = rows;
    }

    /// Record GPU timestamps around each flush's pass, when the device can
    /// (`TIMESTAMP_QUERY`). Read with [`Self::read_gpu_ms`] after submit.
    pub fn enable_timing(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> bool {
        if !device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            return false;
        }
        let set = device.create_query_set(&wgpu::QuerySetDescriptor { label: Some("scene graph timestamps"), ty: wgpu::QueryType::Timestamp, count: 2 });
        let resolve = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("scene graph timestamp resolve"),
            size: 16,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("scene graph timestamp readback"),
            size: 16,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        self.timing = Some(Timing { set, resolve, readback, period_ns: queue.get_timestamp_period(), written: false });
        true
    }

    /// The last flush's pass (scatter + resolve), in GPU milliseconds; None
    /// when timing is off or the flush dispatched nothing. Blocks.
    pub fn read_gpu_ms(&mut self, device: &wgpu::Device) -> Option<f64> {
        let t = self.timing.as_mut()?;
        if !t.written {
            return None;
        }
        t.written = false;
        let slice = t.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("scene graph timing: poll");
        rx.recv().expect("scene graph timing: callback").expect("scene graph timing: map");
        let ticks: [u64; 2] = {
            let data = slice.get_mapped_range().expect("scene graph timing: range");
            bytemuck::pod_read_unaligned(&data[..16])
        };
        t.readback.unmap();
        Some(ticks[1].wrapping_sub(ticks[0]) as f64 * f64::from(t.period_ns) / 1.0e6)
    }

    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    pub fn world_buffer(&self) -> &wgpu::Buffer {
        &self.world
    }

    /// Grow every buffer to hold `slots` rows; returns whether it grew (the
    /// new buffers are empty, so the caller re-uploads and re-resolves all).
    fn ensure_capacity(&mut self, device: &wgpu::Device, slots: u32) -> bool {
        if slots <= self.capacity {
            return false;
        }
        let cap = slots.next_power_of_two().max(1024);
        for t in &mut self.tables {
            t.buffer = storage(device, t.name, u64::from(cap) * t.row_bytes);
            if let Some(s) = &mut t.scatter {
                s.bind_group = None;
            }
        }
        self.order = storage(device, "scene graph order", u64::from(cap) * 4);
        self.world = storage(device, "scene graph world", u64::from(cap) * std::mem::size_of::<Similarity>() as u64);
        self.resolve_bg = Self::resolve_bind_group(device, &self.resolve_bgl, &self.params, &self.tables, &self.order, &self.world);
        self.capacity = cap;
        true
    }

    /// Upload the frame's edits and encode the scatter + resolve pass into
    /// `encoder`. See the module header for the steps.
    pub fn flush(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        tables: &mut NodeTables,
    ) -> FlushStats {
        let t0 = Instant::now();
        let grew = self.ensure_capacity(device, tables.slot_count());
        if grew {
            tables.mark_all_dirty();
        }
        let plan = tables.prepare_frame();
        let t1 = Instant::now();
        let mut stats = FlushStats { grew, ranges: plan.ranges.len(), resolved: plan.resolve_count, ..Default::default() };
        let mut scatters: Vec<(usize, u32)> = Vec::new();
        let len = plan.slot_count;

        let topo: Vec<[u32; 2]>;
        let topo_bytes: &[u8] = if plan.topo_rows.is_empty() {
            &[]
        } else {
            topo = (0..len).map(|i| tables.topo_row(i)).collect();
            bytemuck::cast_slice(&topo)
        };
        let sources: [(&[u8], &[u32]); 4] = [
            (bytemuck::cast_slice(tables.local_rows()), &plan.local_rows),
            (bytemuck::cast_slice(tables.post_rows()), &plan.post_rows),
            (bytemuck::cast_slice(tables.appearance_rows()), &plan.appearance_rows),
            (topo_bytes, &plan.topo_rows),
        ];
        for (k, (data, rows)) in sources.into_iter().enumerate() {
            let up = plan_upload(rows, len);
            let rb = self.tables[k].row_bytes;
            stats.uploads[k] = (self.tables[k].name, up.label(), up.bytes(rb, len));
            match &up {
                UploadPlan::None => {}
                UploadPlan::Full => queue.write_buffer(&self.tables[k].buffer, 0, &data[..(u64::from(len) * rb) as usize]),
                UploadPlan::Runs(runs) => {
                    for &(first, n) in runs {
                        let (a, b) = ((u64::from(first) * rb) as usize, (u64::from(first + n) * rb) as usize);
                        queue.write_buffer(&self.tables[k].buffer, a as u64, &data[a..b]);
                    }
                }
                UploadPlan::Scatter(rows) => {
                    self.stage_scatter(device, queue, k, data, rows);
                    stats.params_bytes += std::mem::size_of::<ScatterParamsGpu>() as u64;
                    scatters.push((k, rows.len() as u32));
                }
            }
        }
        if let Some((a, b)) = plan.order_span {
            let order = tables.order_rows();
            queue.write_buffer(&self.order, u64::from(a) * 4, bytemuck::cast_slice(&order[a as usize..b as usize]));
            stats.order_bytes = u64::from(b - a) * 4;
        }

        if plan.resolve_count > 0 || !scatters.is_empty() {
            self.encode(queue, encoder, &plan, &scatters);
            stats.params_bytes += std::mem::size_of::<ResolveParamsGpu>() as u64;
            stats.dispatched = true;
        }
        stats.bytes = stats.uploads.iter().map(|u| u.2).sum::<u64>() + stats.order_bytes + stats.params_bytes;
        stats.cpu_plan_us = (t1 - t0).as_secs_f64() * 1e6;
        stats.cpu_encode_us = t1.elapsed().as_secs_f64() * 1e6;
        stats
    }

    fn stage_scatter(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, k: usize, data: &[u8], rows: &[u32]) {
        let table = &mut self.tables[k];
        let rb = table.row_bytes as usize;
        let mut packed = Vec::with_capacity(rows.len() * rb);
        for &r in rows {
            packed.extend_from_slice(&data[r as usize * rb..(r as usize + 1) * rb]);
        }
        let need_src = packed.len() as u64;
        let need_idx = rows.len() as u64 * 4;
        let stale = table.scatter.as_ref().is_none_or(|s| s.src.size() < need_src || s.indices.size() < need_idx);
        if stale {
            let grow = |n: u64| n.next_power_of_two().max(256);
            table.scatter = Some(Scatter {
                src: storage(device, "scene graph scatter rows", grow(need_src)),
                indices: storage(device, "scene graph scatter indices", grow(need_idx)),
                params: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("scene graph scatter params"),
                    size: std::mem::size_of::<ScatterParamsGpu>() as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
                bind_group: None,
            });
        }
        let s = table.scatter.as_mut().expect("scatter staging was just ensured");
        if s.bind_group.is_none() {
            s.bind_group = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("scene graph scatter bg"),
                layout: &self.scatter_bgl,
                entries: &[bind(0, &table.buffer), bind(1, &s.src), bind(2, &s.indices), bind(3, &s.params)],
            }));
        }
        queue.write_buffer(&s.src, 0, &packed);
        queue.write_buffer(&s.indices, 0, bytemuck::cast_slice(rows));
        let p = ScatterParamsGpu { row_words: (rb / 4) as u32, rows: rows.len() as u32, _pad: [0; 2] };
        queue.write_buffer(&s.params, 0, bytemuck::bytes_of(&p));
    }

    fn encode(&mut self, queue: &wgpu::Queue, encoder: &mut wgpu::CommandEncoder, plan: &FramePlan, scatters: &[(usize, u32)]) {
        let mut params = ResolveParamsGpu {
            range_count: plan.ranges.len() as u32,
            total: plan.resolve_count,
            max_depth: MAX_DEPTH,
            row_limit: self.output_rows,
            ranges: [[0; 4]; MAX_RESOLVE_RANGES / 2],
        };
        let mut first = 0u32;
        for (k, &(start, n)) in plan.ranges.iter().enumerate() {
            params.ranges[k / 2][(k % 2) * 2] = start;
            params.ranges[k / 2][(k % 2) * 2 + 1] = first;
            first += n;
        }
        queue.write_buffer(&self.params, 0, bytemuck::bytes_of(&params));

        let timestamp_writes = self.timing.as_ref().map(|t| wgpu::ComputePassTimestampWrites {
            query_set: &t.set,
            beginning_of_pass_write_index: Some(0),
            end_of_pass_write_index: Some(1),
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("scene graph resolve"), timestamp_writes });
            for &(k, rows) in scatters {
                let s = self.tables[k].scatter.as_ref().expect("a staged scatter has its buffers");
                let words = rows * (self.tables[k].row_bytes / 4) as u32;
                let (x, y) = grid(words, SCATTER_WG);
                pass.set_pipeline(&self.scatter_pipeline);
                pass.set_bind_group(0, s.bind_group.as_ref().expect("a staged scatter has its bind group"), &[]);
                pass.dispatch_workgroups(x, y, 1);
            }
            if plan.resolve_count > 0 {
                let (x, y) = grid(plan.resolve_count, RESOLVE_WG);
                pass.set_pipeline(&self.resolve_pipeline);
                pass.set_bind_group(0, &self.resolve_bg, &[]);
                pass.set_bind_group(1, &self.output_bg, &[]);
                pass.dispatch_workgroups(x, y, 1);
            }
        }
        if let Some(t) = &mut self.timing {
            encoder.resolve_query_set(&t.set, 0..2, &t.resolve, 0);
            encoder.copy_buffer_to_buffer(&t.resolve, 0, &t.readback, 0, 16);
            t.written = true;
        }
    }
}

/// Block on a readback of a whole buffer as words (verification and the
/// bench, never the frame path).
pub fn read_buffer_words(device: &wgpu::Device, queue: &wgpu::Queue, src: &wgpu::Buffer, bytes: u64) -> Vec<u32> {
    let size = bytes.div_ceil(4) * 4;
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("scene graph readback"),
        size: size.max(4),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("scene graph readback") });
    if size > 0 {
        encoder.copy_buffer_to_buffer(src, 0, &staging, 0, size);
    }
    queue.submit([encoder.finish()]);
    let slice = staging.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("scene graph readback: poll");
    rx.recv().expect("scene graph readback: callback").expect("scene graph readback: map");
    let data = slice.get_mapped_range().expect("scene graph readback: range");
    let mut out = vec![0u32; (size / 4) as usize];
    bytemuck::cast_slice_mut::<u32, u8>(&mut out).copy_from_slice(&data[..size as usize]);
    drop(data);
    staging.unmap();
    out
}

impl SceneGraphGpu {
    /// The world table as the last flush left it (blocking; tests, bench).
    pub fn read_worlds(&self, device: &wgpu::Device, queue: &wgpu::Queue, slots: u32) -> Vec<Similarity> {
        let words = read_buffer_words(device, queue, &self.world, u64::from(slots) * 32);
        bytemuck::cast_slice(&words[..slots as usize * 8]).to_vec()
    }
}
