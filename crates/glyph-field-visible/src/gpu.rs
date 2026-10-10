//! The device side of the Visible field: the resident tables and the layout
//! kernels ([`Resident`], shared by the field and the headless
//! [`layout_all_lines`]), the per-frame cull ([`Frame`]: the three compute
//! passes `prepare` records, the indirect draws, the stats ring) and the
//! wash pipeline.
//!
//! BIND GROUPS. Three layouts, one per shader file: every entry point of a
//! file binds the whole set (an unused binding costs nothing), so a kernel
//! never needs its own group. The counts stay under Metal's 31 buffers per
//! stage: cull 15 storage + 1 uniform, layout 13 + 2, wash 3 + 1.
//!
//! THE FRAME PATH READS NOTHING BACK. Counters are copied into a ring of
//! three staging buffers; the copy's submit happens after `prepare`
//! returns, so the map is requested on the NEXT `prepare` and read on the
//! one after — `stats()` lags its frame by two. `device.poll(Poll)` runs
//! the map callbacks without waiting.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use glyph_field::{FieldCore, FieldResources, FieldShape, FieldTargets, FramePrepare, ItemParamsGpu, SlotChunk, SlotStorage};
use glyph_field_derived::DerivedSlot;
use wgpu::util::DeviceExt;

use crate::tables::{self, *};
use crate::{ByteSpanGpu, VisibleInputs, VisibleLimits, VisibleStats};

pub const CULL_WGSL: &str = include_str!("../shaders/visible_cull.wgsl");
pub const LAYOUT_WGSL: &str = include_str!("../shaders/visible_layout.wgsl");
pub const WASH_WGSL: &str = include_str!("../shaders/visible_wash.wgsl");

/// Slots per headless batch: 4 M slots = 80 MiB of readback at a time.
const HEADLESS_BATCH_SLOTS: u64 = 1 << 22;

fn storage_entry(binding: u32, read_only: bool, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn uniform_entry(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn bind(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry { binding, resource: buffer.as_entire_binding() }
}

/// A storage buffer holding `data`, never empty (a zero-size binding is
/// refused; an empty table binds one zeroed element).
fn storage_init<T: bytemuck::Pod>(device: &wgpu::Device, label: &str, data: &[T], extra: wgpu::BufferUsages) -> wgpu::Buffer {
    let usage = wgpu::BufferUsages::STORAGE | extra;
    if data.is_empty() {
        return device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: (std::mem::size_of::<T>() as u64).max(16),
            usage,
            mapped_at_creation: false,
        });
    }
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some(label), contents: bytemuck::cast_slice(data), usage })
}

fn storage_zeroed(device: &wgpu::Device, label: &str, size: u64, extra: wgpu::BufferUsages) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: size.max(16),
        usage: wgpu::BufferUsages::STORAGE | extra,
        mapped_at_creation: false,
    })
}

fn compute_pipeline(device: &wgpu::Device, layout: &wgpu::PipelineLayout, module: &wgpu::ShaderModule, entry: &str) -> wgpu::ComputePipeline {
    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(&format!("visible {entry}")),
        layout: Some(layout),
        module,
        entry_point: Some(entry),
        compilation_options: Default::default(),
        cache: None,
    })
}

/// Block on a readback of `size` bytes from `src` (a verification path,
/// never the frame's).
fn read_back(device: &wgpu::Device, queue: &wgpu::Queue, src: &wgpu::Buffer, offset: u64, size: u64) -> Vec<u8> {
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("visible readback"),
        size: size.max(4),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("visible readback copy") });
    if size > 0 {
        encoder.copy_buffer_to_buffer(src, offset, &staging, 0, size);
    }
    queue.submit([encoder.finish()]);
    let slice = staging.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .poll(wgpu::PollType::Wait { submission_index: None, timeout: None })
        .expect("visible readback: poll failed");
    rx.recv().expect("visible readback: callback dropped").expect("visible readback: map failed");
    let data = slice.get_mapped_range().expect("visible readback: range");
    let out = data[..size as usize].to_vec();
    drop(data);
    staging.unmap();
    out
}

// ── the resident set ────────────────────────────────────────────────────────

/// Everything uploaded once per load, plus the layout kernels over it.
pub struct Resident {
    pub device: wgpu::Device,
    pub items_total: u32,
    pub lines_total: u32,
    /// The line table's survivors summed: what a full layout would emit.
    pub glyph_total: u64,
    pub seeds_total: u32,
    pub cell_adv: f32,
    pub default_color: u32,
    pub byte_shift: u32,
    pub layout_params: wgpu::Buffer,
    pub trie_meta: wgpu::Buffer,
    pub trie_words: wgpu::Buffer,
    pub bytes: Vec<wgpu::Buffer>,
    pub items: wgpu::Buffer,
    pub item_params: wgpu::Buffer,
    pub hidden: wgpu::Buffer,
    pub lines: wgpu::Buffer,
    pub seeds: wgpu::Buffer,
    /// `2 × seeds` words: per-segment counts, then each seed's survivors-before.
    pub survivors: wgpu::Buffer,
    pub spans: wgpu::Buffer,
    pub first_span: wgpu::Buffer,
    /// The host mirror the span edits need.
    pub span_alloc: RefCell<SpanAlloc>,
    pub line_starts: Vec<u32>,
    /// Per item `(first_line, line_count)` and its byte length.
    pub item_lines: Vec<(u32, u32)>,
    pub item_byte_lens: Vec<u32>,
    pub layout_bgl: wgpu::BindGroupLayout,
    pub layout_pipeline: wgpu::ComputePipeline,
    pub count_pipeline: wgpu::ComputePipeline,
    pub prefix_pipeline: wgpu::ComputePipeline,
}

impl Resident {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, inputs: &VisibleInputs<'_>) -> Self {
        tables::check_inputs(inputs);
        let items = inputs.items;
        let limits = device.limits();
        let byte_shift = byte_chunk_shift(limits.max_storage_buffer_binding_size);
        let plan = plan_bytes(items, byte_shift);

        // The bytes: one buffer per chunk, written in place while mapped
        // (zero elsewhere), four bindings in the kernel.
        let mut bytes = Vec::with_capacity(MAX_BYTE_CHUNKS);
        for (c, &size) in plan.chunk_sizes.iter().enumerate() {
            let padded = size.div_ceil(4) * 4;
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(&format!("visible bytes {c}/{}", plan.chunk_sizes.len())),
                size: padded.max(16),
                usage: wgpu::BufferUsages::STORAGE,
                mapped_at_creation: true,
            });
            {
                let mut view = buffer.slice(..).get_mapped_range_mut().expect("visible bytes: map at creation");
                for (i, &(chunk, off)) in plan.place.iter().enumerate() {
                    if chunk as usize == c {
                        let src = inputs.item_bytes[i];
                        view.slice(off as usize..off as usize + src.len()).copy_from_slice(src);
                    }
                }
            }
            buffer.unmap();
            bytes.push(buffer);
        }
        while bytes.len() < MAX_BYTE_CHUNKS {
            bytes.push(storage_zeroed(device, "visible bytes (unused chunk)", 16, wgpu::BufferUsages::empty()));
        }

        let (span_table, span_alloc) = plan_spans(items, inputs.spans);
        let first = first_span_index(items, inputs.lines, &span_table, &span_alloc);
        let items_gpu: Vec<ItemGpu> = items
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let (base, count, _) = span_alloc.runs[i];
                item_gpu(i, it, plan.place[i], base, count)
            })
            .collect();
        let params: Vec<ItemParamsGpu> = items.iter().map(|it| it.params).collect();
        let packed = pack_trie(inputs.trie);

        let layout_params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("visible layout params"),
            size: std::mem::size_of::<LayoutParamsGpu>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let trie_meta = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("visible trie meta"),
            contents: bytemuck::bytes_of(&packed.meta),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let copy_dst = wgpu::BufferUsages::COPY_DST;
        let items_buf = storage_init(device, "visible items", &items_gpu, copy_dst);
        let item_params = storage_init(device, "visible item params", &params, copy_dst);
        let hidden = storage_zeroed(device, "visible hidden", items.len() as u64 * 4, copy_dst);
        let lines = storage_init(device, "visible lines", inputs.lines, wgpu::BufferUsages::empty());
        let seeds = storage_init(device, "visible seeds", inputs.seeds, wgpu::BufferUsages::empty());
        let survivors = storage_zeroed(device, "visible seed survivors", inputs.seeds.len() as u64 * 8, wgpu::BufferUsages::COPY_SRC);
        let spans = storage_init(device, "visible spans", &span_table, copy_dst);
        let first_span = storage_init(device, "visible first span per line", &first, copy_dst);
        let trie_words = storage_init(device, "visible trie tables", &packed.words, wgpu::BufferUsages::empty());

        let compute = wgpu::ShaderStages::COMPUTE;
        let layout_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("visible layout bgl"),
            entries: &[
                uniform_entry(0, compute),
                uniform_entry(1, compute),
                storage_entry(2, true, compute),
                storage_entry(3, true, compute),
                storage_entry(4, true, compute),
                storage_entry(5, true, compute),
                storage_entry(6, true, compute),
                storage_entry(7, true, compute),
                storage_entry(8, true, compute),
                storage_entry(9, false, compute),
                storage_entry(10, true, compute),
                storage_entry(11, false, compute),
                storage_entry(12, true, compute),
                storage_entry(13, true, compute),
                storage_entry(14, true, compute),
            ],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("visible_layout.wgsl"),
            source: wgpu::ShaderSource::Wgsl(LAYOUT_WGSL.into()),
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("visible layout pl"),
            bind_group_layouts: &[Some(&layout_bgl)],
            immediate_size: 0,
        });
        let layout_pipeline = compute_pipeline(device, &pl, &module, "layout_segments");
        let count_pipeline = compute_pipeline(device, &pl, &module, "count_seed_segments");
        let prefix_pipeline = compute_pipeline(device, &pl, &module, "prefix_seed_survivors");

        let this = Self {
            device: device.clone(),
            items_total: items.len() as u32,
            lines_total: inputs.lines.len() as u32,
            glyph_total: inputs.lines.iter().map(|l| l.glyph_count as u64).sum(),
            seeds_total: inputs.seeds.len() as u32,
            cell_adv: packed.meta.cell_adv,
            default_color: inputs.default_color,
            byte_shift,
            layout_params,
            trie_meta,
            trie_words,
            bytes,
            items: items_buf,
            item_params,
            hidden,
            lines,
            seeds,
            survivors,
            spans,
            first_span,
            span_alloc: RefCell::new(span_alloc),
            line_starts: inputs.lines.iter().map(|l| l.byte_start).collect(),
            item_lines: items.iter().map(|it| (it.first_line, it.line_count)).collect(),
            item_byte_lens: items.iter().map(|it| it.byte_len).collect(),
            layout_bgl,
            layout_pipeline,
            count_pipeline,
            prefix_pipeline,
        };
        this.count_seed_survivors(queue);
        this
    }

    /// A layout bind group over `segs` and `slots`.
    pub fn layout_bind_group(&self, segs: &wgpu::Buffer, slots: &wgpu::Buffer) -> wgpu::BindGroup {
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("visible layout bg"),
            layout: &self.layout_bgl,
            entries: &[
                bind(0, &self.layout_params),
                bind(1, &self.trie_meta),
                bind(2, &self.bytes[0]),
                bind(3, &self.bytes[1]),
                bind(4, &self.bytes[2]),
                bind(5, &self.bytes[3]),
                bind(6, &self.items),
                bind(7, &self.lines),
                bind(8, &self.seeds),
                bind(9, &self.survivors),
                bind(10, segs),
                bind(11, slots),
                bind(12, &self.trie_words),
                bind(13, &self.spans),
                bind(14, &self.first_span),
            ],
        })
    }

    pub fn write_layout_params(&self, queue: &wgpu::Queue, count: u32, debug_tint: u32) {
        let p = LayoutParamsGpu { count, debug_tint, default_color: self.default_color, chunk_shift: self.byte_shift };
        queue.write_buffer(&self.layout_params, 0, bytemuck::bytes_of(&p));
    }

    /// At load: every seeded segment's survivor count, then each seed's
    /// survivors-before — the slot base of a seeded segment inside its line
    /// (`visible_layout.wgsl`, the two seed kernels). One dispatch each, no
    /// readback; the result stays resident for cull B.
    fn count_seed_survivors(&self, queue: &wgpu::Queue) {
        if self.seeds_total == 0 {
            return;
        }
        self.write_layout_params(queue, self.seeds_total, 0);
        let dummy_segs = storage_zeroed(&self.device, "visible segs (seed count)", 32, wgpu::BufferUsages::empty());
        let dummy_slots = storage_zeroed(&self.device, "visible slots (seed count)", 20, wgpu::BufferUsages::empty());
        let bg = self.layout_bind_group(&dummy_segs, &dummy_slots);
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("visible seed survivors") });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("visible seed count"), timestamp_writes: None });
            let [x, y, z] = plan_dispatch(self.seeds_total);
            pass.set_bind_group(0, &bg, &[]);
            pass.set_pipeline(&self.count_pipeline);
            pass.dispatch_workgroups(x, y, z);
            pass.set_pipeline(&self.prefix_pipeline);
            pass.dispatch_workgroups(x, y, z);
        }
        queue.submit([encoder.finish()]);
    }

    /// Each seed's survivors-before, read back (blocking; the headless path).
    pub fn read_survivors_before(&self, queue: &wgpu::Queue) -> Vec<u32> {
        if self.seeds_total == 0 {
            return Vec::new();
        }
        let n = self.seeds_total as u64;
        let data = read_back(&self.device, queue, &self.survivors, n * 4, n * 4);
        bytemuck::cast_slice(&data).to_vec()
    }

    pub fn set_item_hidden(&self, queue: &wgpu::Queue, item: u32, hidden: bool) {
        assert!(item < self.items_total, "set_item_hidden: item {item} of {}", self.items_total);
        queue.write_buffer(&self.hidden, item as u64 * 4, bytemuck::bytes_of(&u32::from(hidden)));
    }

    pub fn set_item_bbox(&self, queue: &wgpu::Queue, item: u32, bbox_min: [f32; 3], bbox_max: [f32; 3]) {
        assert!(item < self.items_total, "set_item_bbox: item {item} of {}", self.items_total);
        let words = [bbox_min[0], bbox_min[1], bbox_min[2], bbox_max[0], bbox_max[1], bbox_max[2]];
        let offset = item as u64 * std::mem::size_of::<ItemGpu>() as u64 + std::mem::offset_of!(ItemGpu, bbox_min) as u64;
        queue.write_buffer(&self.items, offset, bytemuck::cast_slice(&words));
    }

    /// Replace one item's spans: in place when they fit the run's slack,
    /// else in a fresh run at the table's tail (the old run becomes a hole);
    /// when the tail is full too the edit is refused with a warning and the
    /// item keeps its colours. Rewrites the item's row and its lines' first-
    /// span index.
    pub fn set_item_spans(&self, queue: &wgpu::Queue, item: u32, spans: &[ByteSpanGpu]) {
        assert!(item < self.items_total, "set_item_spans: item {item} of {}", self.items_total);
        let i = item as usize;
        let (first_line, line_count) = self.item_lines[i];
        check_spans(i, spans, self.item_byte_lens[i]);
        let n = spans.len() as u32;
        let mut alloc = self.span_alloc.borrow_mut();
        let (old_base, _, cap) = alloc.runs[i];
        let base = if n <= cap {
            old_base
        } else {
            let need = n + span_slack(n);
            if alloc.next_free + need > alloc.capacity {
                log::warn!(
                    "visible field: item {item} needs {need} span slots and the table has {} free; the edit is dropped",
                    alloc.capacity - alloc.next_free
                );
                return;
            }
            let b = alloc.next_free;
            alloc.next_free += need;
            alloc.runs[i] = (b, n, need);
            b
        };
        alloc.runs[i].1 = n;
        if n > 0 {
            queue.write_buffer(&self.spans, base as u64 * std::mem::size_of::<ByteSpanGpu>() as u64, bytemuck::cast_slice(spans));
        }
        let row_offset = item as u64 * std::mem::size_of::<ItemGpu>() as u64 + std::mem::offset_of!(ItemGpu, span_base) as u64;
        queue.write_buffer(&self.items, row_offset, bytemuck::cast_slice(&[base, n]));
        if line_count > 0 {
            let starts = &self.line_starts[first_line as usize..(first_line + line_count) as usize];
            let idx = first_span_index_for_item(starts, spans, base);
            queue.write_buffer(&self.first_span, first_line as u64 * 4, bytemuck::cast_slice(&idx));
        }
    }
}

// ── the frame ───────────────────────────────────────────────────────────────

const STATS_RING: usize = 3;
const STATS_COUNTERS_BYTES: u64 = (COUNTER_WORDS * 4) as u64;
const STATS_TIMESTAMP_OFFSET: u64 = 64;
const STATS_SLOT_BYTES: u64 = 96;
const TIMESTAMP_QUERIES: u32 = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Free,
    /// The copy is recorded; the map waits for the next `prepare`.
    Copied,
    Mapping,
}

struct StatsSlot {
    buffer: wgpu::Buffer,
    state: Cell<SlotState>,
    /// 0 pending, 1 mapped, 2 failed.
    ready: Arc<AtomicU8>,
}

/// The per-frame machinery.
pub struct Frame {
    pub limits: VisibleLimits,
    pub frame_uniform: wgpu::Buffer,
    pub visible: wgpu::Buffer,
    pub line_base: wgpu::Buffer,
    pub counters: wgpu::Buffer,
    pub segs: wgpu::Buffer,
    pub wash: wgpu::Buffer,
    pub slots: wgpu::Buffer,
    /// The dispatch and draw arguments as the kernels WRITE them (storage).
    /// A buffer bound read-write in a dispatch's bind group cannot be that
    /// dispatch's indirect source (wgpu refuses the usage pair), so they
    /// are copied into `indirect` between passes.
    pub args: wgpu::Buffer,
    pub indirect: wgpu::Buffer,
    cull_bg: wgpu::BindGroup,
    cull_items: wgpu::ComputePipeline,
    prefix_items: wgpu::ComputePipeline,
    cull_lines: wgpu::ComputePipeline,
    finalize: wgpu::ComputePipeline,
    layout_bg: wgpu::BindGroup,
    wash_pipeline: wgpu::RenderPipeline,
    wash_bg: wgpu::BindGroup,
    quad_index: wgpu::Buffer,
    stats: Vec<StatsSlot>,
    last_stats: Cell<VisibleStats>,
    timestamps: Option<(wgpu::QuerySet, wgpu::Buffer, f32)>,
    frames: Cell<u64>,
}

impl Frame {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, resident: &Resident, resources: &FieldResources<'_>, targets: FieldTargets, limits: VisibleLimits) -> Self {
        // The segment cap is a whole number of workgroups: finalize blanks
        // the tail of the last group so the layout dispatch reads no stale
        // entry (`visible_cull.wgsl`).
        let max_segments = limits.max_segments.div_ceil(WORKGROUP).max(1) * WORKGROUP;
        let limits = VisibleLimits { max_segments, max_slots: limits.max_slots.max(1), max_wash: limits.max_wash.max(1) };
        let binding_limit = device.limits().max_storage_buffer_binding_size;
        let slot_bytes = limits.max_slots as u64 * std::mem::size_of::<DerivedSlot>() as u64;
        assert!(
            slot_bytes <= binding_limit,
            "visible field: {} slots = {slot_bytes} B exceeds the storage binding limit {binding_limit}; lower VisibleLimits::max_slots",
            limits.max_slots
        );

        let frame_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("visible frame uniform"),
            size: std::mem::size_of::<FrameGpu>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let n_items = resident.items_total.max(1) as u64;
        let visible = storage_zeroed(device, "visible item list", n_items * 4, wgpu::BufferUsages::empty());
        let line_base = storage_zeroed(device, "visible line bases", n_items * 4, wgpu::BufferUsages::empty());
        let counters = storage_zeroed(device, "visible counters", STATS_COUNTERS_BYTES, wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC);
        let segs = storage_zeroed(device, "visible segments", limits.max_segments as u64 * std::mem::size_of::<SegGpu>() as u64, wgpu::BufferUsages::empty());
        let wash = storage_zeroed(device, "visible wash quads", limits.max_wash as u64 * std::mem::size_of::<WashGpu>() as u64, wgpu::BufferUsages::empty());
        let slots = storage_zeroed(device, "visible transient slots", slot_bytes, wgpu::BufferUsages::COPY_SRC);
        let args = storage_zeroed(device, "visible dispatch/draw args (storage)", (INDIRECT_WORDS * 4) as u64, wgpu::BufferUsages::COPY_SRC);
        let indirect = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("visible indirect args"),
            size: (INDIRECT_WORDS * 4) as u64,
            usage: wgpu::BufferUsages::INDIRECT | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let quad_index = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("visible wash quad index buffer"),
            contents: bytemuck::cast_slice(&[0u16, 1, 2, 0, 2, 3]),
            usage: wgpu::BufferUsages::INDEX,
        });

        // The cull kernels.
        let compute = wgpu::ShaderStages::COMPUTE;
        let cull_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("visible cull bgl"),
            entries: &[
                uniform_entry(0, compute),
                storage_entry(1, true, compute),
                storage_entry(2, true, compute),
                storage_entry(3, true, compute),
                storage_entry(4, true, compute),
                storage_entry(5, true, compute),
                storage_entry(6, true, compute),
                storage_entry(7, true, compute),
                storage_entry(8, true, compute),
                storage_entry(9, true, compute),
                storage_entry(10, false, compute),
                storage_entry(11, false, compute),
                storage_entry(12, false, compute),
                storage_entry(13, false, compute),
                storage_entry(14, false, compute),
                storage_entry(15, false, compute),
            ],
        });
        let cull_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("visible cull bg"),
            layout: &cull_bgl,
            entries: &[
                bind(0, &frame_uniform),
                bind(1, &resident.items),
                bind(2, &resident.item_params),
                bind(3, &resident.hidden),
                bind(4, &resident.lines),
                bind(5, &resident.seeds),
                bind(6, &resident.survivors),
                bind(7, &resident.spans),
                bind(8, &resident.first_span),
                bind(9, resources.group_table),
                bind(10, &visible),
                bind(11, &line_base),
                bind(12, &counters),
                bind(13, &segs),
                bind(14, &wash),
                bind(15, &args),
            ],
        });
        let cull_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("visible_cull.wgsl"),
            source: wgpu::ShaderSource::Wgsl(CULL_WGSL.into()),
        });
        let cull_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("visible cull pl"),
            bind_group_layouts: &[Some(&cull_bgl)],
            immediate_size: 0,
        });
        let cull_items = compute_pipeline(device, &cull_pl, &cull_module, "cull_items");
        let prefix_items = compute_pipeline(device, &cull_pl, &cull_module, "prefix_items");
        let cull_lines = compute_pipeline(device, &cull_pl, &cull_module, "cull_lines");
        let finalize = compute_pipeline(device, &cull_pl, &cull_module, "finalize");
        let layout_bg = resident.layout_bind_group(&segs, &slots);

        // The wash draw: the glyph pass's depth and blend state.
        let vertex = wgpu::ShaderStages::VERTEX;
        let wash_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("visible wash bgl"),
            entries: &[uniform_entry(0, vertex), storage_entry(1, true, vertex), storage_entry(2, true, vertex), storage_entry(3, true, vertex)],
        });
        let wash_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("visible wash bg"),
            layout: &wash_bgl,
            entries: &[bind(0, &frame_uniform), bind(1, &wash), bind(2, resources.group_table), bind(3, &resident.item_params)],
        });
        let wash_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("visible_wash.wgsl"),
            source: wgpu::ShaderSource::Wgsl(WASH_WGSL.into()),
        });
        let wash_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("visible wash pl"),
            bind_group_layouts: &[Some(&wash_bgl)],
            immediate_size: 0,
        });
        let blend = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        };
        let wash_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("visible wash pipeline"),
            layout: Some(&wash_pl),
            vertex: wgpu::VertexState {
                module: &wash_module,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &wash_module,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: targets.color_format,
                    blend: Some(wgpu::BlendState { color: blend, alpha: blend }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default() },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: targets.depth_format,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState { count: targets.sample_count, ..Default::default() },
            multiview_mask: None,
            cache: None,
        });

        let stats = (0..STATS_RING)
            .map(|k| StatsSlot {
                buffer: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(&format!("visible stats staging {k}/{STATS_RING}")),
                    size: STATS_SLOT_BYTES,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                }),
                state: Cell::new(SlotState::Free),
                ready: Arc::new(AtomicU8::new(0)),
            })
            .collect();
        let timestamps = device.features().contains(wgpu::Features::TIMESTAMP_QUERY).then(|| {
            let set = device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("visible pass timestamps"),
                ty: wgpu::QueryType::Timestamp,
                count: TIMESTAMP_QUERIES,
            });
            let resolve = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("visible timestamp resolve"),
                size: wgpu::QUERY_RESOLVE_BUFFER_ALIGNMENT,
                usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
            (set, resolve, queue.get_timestamp_period())
        });

        Self {
            limits,
            frame_uniform,
            visible,
            line_base,
            counters,
            segs,
            wash,
            slots,
            args,
            indirect,
            cull_bg,
            cull_items,
            prefix_items,
            cull_lines,
            finalize,
            layout_bg,
            wash_pipeline,
            wash_bg,
            quad_index,
            stats,
            last_stats: Cell::new(VisibleStats { items_total: resident.items_total, ..Default::default() }),
            timestamps,
            frames: Cell::new(0),
        }
    }

    /// The Derived draw's storage: one chunk, the whole transient buffer.
    pub fn slot_storage(&self) -> SlotStorage<DerivedSlot> {
        let n = self.limits.max_slots as usize;
        SlotStorage::new(vec![SlotChunk { buffer: self.slots.clone(), offset: 0, slots: n as u32 }], n, n, None)
    }

    fn frame_gpu(&self, resident: &Resident, frame: &FramePrepare) -> FrameGpu {
        FrameGpu {
            view_proj: frame.view_proj,
            planes: frustum_planes(&frame.view_proj),
            eye: [frame.eye[0], frame.eye[1], frame.eye[2], frame.px_scale],
            lod: [frame.lod_glyph_px, frame.lod_backdrop_px, frame.time, resident.cell_adv],
            u0: [frame.greek_mode, frame.debug_tint, resident.items_total, resident.seeds_total],
            u1: [self.limits.max_slots, self.limits.max_segments, self.limits.max_wash, resident.default_color],
        }
    }

    /// `GlyphField::prepare`: the cull and the layout, in `encoder`.
    pub fn prepare(&self, resident: &Resident, queue: &wgpu::Queue, encoder: &mut wgpu::CommandEncoder, frame: &FramePrepare) {
        self.frames.set(self.frames.get() + 1);
        self.collect_stats(resident);

        queue.write_buffer(&self.frame_uniform, 0, bytemuck::bytes_of(&self.frame_gpu(resident, frame)));
        queue.write_buffer(&self.counters, 0, &[0u8; STATS_COUNTERS_BYTES as usize]);
        // The layout kernel bounds itself by the (whole-workgroup) segment
        // cap; finalize blanks the entries past the live count.
        resident.write_layout_params(queue, self.limits.max_segments, frame.debug_tint);

        let ts = |begin: u32, end: u32| {
            self.timestamps.as_ref().map(|(set, _, _)| wgpu::ComputePassTimestampWrites {
                query_set: set,
                beginning_of_pass_write_index: Some(begin),
                end_of_pass_write_index: Some(end),
            })
        };
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("visible cull A"), timestamp_writes: ts(0, 1) });
            pass.set_bind_group(0, &self.cull_bg, &[]);
            let [x, y, z] = plan_dispatch(resident.items_total);
            pass.set_pipeline(&self.cull_items);
            pass.dispatch_workgroups(x, y, z);
            pass.set_pipeline(&self.prefix_items);
            pass.dispatch_workgroups(1, 1, 1);
        }
        encoder.copy_buffer_to_buffer(&self.args, INDIRECT_CULL_B, &self.indirect, INDIRECT_CULL_B, 12);
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("visible cull B"), timestamp_writes: None });
            pass.set_bind_group(0, &self.cull_bg, &[]);
            pass.set_pipeline(&self.cull_lines);
            pass.dispatch_workgroups_indirect(&self.indirect, INDIRECT_CULL_B);
            pass.set_pipeline(&self.finalize);
            pass.dispatch_workgroups(1, 1, 1);
        }
        encoder.copy_buffer_to_buffer(&self.args, INDIRECT_LAYOUT, &self.indirect, INDIRECT_LAYOUT, (INDIRECT_WORDS * 4) as u64 - INDIRECT_LAYOUT);
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("visible layout"), timestamp_writes: ts(2, 3) });
            pass.set_bind_group(0, &self.layout_bg, &[]);
            pass.set_pipeline(&resident.layout_pipeline);
            pass.dispatch_workgroups_indirect(&self.indirect, INDIRECT_LAYOUT);
        }
        self.record_stats_copy(encoder);
    }

    /// Step the stats ring: read what has mapped, request maps for what was
    /// copied last frame.
    fn collect_stats(&self, resident: &Resident) {
        resident.device.poll(wgpu::PollType::Poll).expect("visible field: poll");
        for slot in &self.stats {
            match slot.state.get() {
                SlotState::Mapping => match slot.ready.load(Ordering::Acquire) {
                    1 => {
                        {
                            let data = slot.buffer.slice(..).get_mapped_range().expect("visible stats: mapped range");
                            self.last_stats.set(self.decode_stats(resident, &data));
                        }
                        slot.buffer.unmap();
                        slot.state.set(SlotState::Free);
                    }
                    2 => slot.state.set(SlotState::Free),
                    _ => {}
                },
                SlotState::Copied => {
                    slot.ready.store(0, Ordering::Release);
                    let ready = slot.ready.clone();
                    slot.buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
                        ready.store(if r.is_ok() { 1 } else { 2 }, Ordering::Release);
                    });
                    slot.state.set(SlotState::Mapping);
                }
                SlotState::Free => {}
            }
        }
    }

    fn record_stats_copy(&self, encoder: &mut wgpu::CommandEncoder) {
        let Some(slot) = self.stats.iter().find(|s| s.state.get() == SlotState::Free) else {
            return;
        };
        encoder.copy_buffer_to_buffer(&self.counters, 0, &slot.buffer, 0, STATS_COUNTERS_BYTES);
        if let Some((set, resolve, _)) = &self.timestamps {
            encoder.resolve_query_set(set, 0..TIMESTAMP_QUERIES, resolve, 0);
            encoder.copy_buffer_to_buffer(resolve, 0, &slot.buffer, STATS_TIMESTAMP_OFFSET, TIMESTAMP_QUERIES as u64 * 8);
        }
        slot.state.set(SlotState::Copied);
    }

    fn decode_stats(&self, resident: &Resident, data: &[u8]) -> VisibleStats {
        let c: &[u32] = bytemuck::cast_slice(&data[..STATS_COUNTERS_BYTES as usize]);
        let (mut cull_ms, mut layout_ms) = (0.0, 0.0);
        if let Some((_, _, period_ns)) = &self.timestamps {
            let t: &[u64] = bytemuck::cast_slice(&data[STATS_TIMESTAMP_OFFSET as usize..STATS_TIMESTAMP_OFFSET as usize + 32]);
            let ms = |a: u64, b: u64| b.saturating_sub(a) as f32 * period_ns / 1e6;
            cull_ms = ms(t[0], t[1]);
            layout_ms = ms(t[2], t[3]);
        }
        VisibleStats {
            items_total: resident.items_total,
            items_visible: c[counter::ITEMS_VISIBLE],
            items_backdrop: c[counter::ITEMS_BACKDROP],
            lines_candidate: c[counter::LINES_CANDIDATE],
            lines_glyph: c[counter::LINES_GLYPH],
            lines_wash: c[counter::LINES_WASH],
            segments: c[counter::SEG_FIT_END].min(self.limits.max_segments),
            slots: c[counter::SLOT_FIT_END].min(self.limits.max_slots),
            slots_dropped: c[counter::SLOTS_DROPPED],
            cull_ms,
            layout_ms,
            draw_ms: 0.0,
        }
    }

    pub fn stats(&self) -> VisibleStats {
        self.last_stats.get()
    }

    pub fn record_glyph_draw(&self, core: &FieldCore<DerivedSlot>, pass: &mut wgpu::RenderPass<'_>) {
        core.record_draw_indirect(pass, 0, &self.indirect, INDIRECT_DRAW);
    }

    pub fn record_wash_draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        pass.set_pipeline(&self.wash_pipeline);
        pass.set_bind_group(0, &self.wash_bg, &[]);
        pass.set_index_buffer(self.quad_index.slice(..), wgpu::IndexFormat::Uint16);
        pass.draw_indexed_indirect(&self.indirect, INDIRECT_WASH_DRAW);
    }

    /// The counters of the LAST prepared frame, read back blocking — a test
    /// instrument, never the frame path.
    pub fn read_counters(&self, resident: &Resident, queue: &wgpu::Queue) -> [u32; COUNTER_WORDS] {
        let data = read_back(&resident.device, queue, &self.counters, 0, STATS_COUNTERS_BYTES);
        let mut out = [0u32; COUNTER_WORDS];
        out.copy_from_slice(bytemuck::cast_slice(&data));
        out
    }

    /// The first `count` transient slots, read back blocking (tests).
    pub fn read_slots(&self, resident: &Resident, queue: &wgpu::Queue, count: u32) -> Vec<DerivedSlot> {
        let count = count.min(self.limits.max_slots) as u64;
        let data = read_back(&resident.device, queue, &self.slots, 0, count * std::mem::size_of::<DerivedSlot>() as u64);
        bytemuck::cast_slice(&data).to_vec()
    }

    /// The Derived draw over the transient buffer.
    pub fn derived_shape_entries<'a>(&self, resident: &'a Resident, resources: &FieldResources<'a>, group_overrides: &'a wgpu::Buffer) -> [wgpu::BindGroupEntry<'a>; 3] {
        use glyph_field_derived::pipeline::{BINDING_GLYPH_ADVANCES, BINDING_GROUP_OVERRIDES, BINDING_ITEM_TABLE};
        [
            wgpu::BindGroupEntry { binding: BINDING_ITEM_TABLE, resource: resident.item_params.as_entire_binding() },
            wgpu::BindGroupEntry { binding: BINDING_GLYPH_ADVANCES, resource: resources.glyph_advances.as_entire_binding() },
            wgpu::BindGroupEntry { binding: BINDING_GROUP_OVERRIDES, resource: group_overrides.as_entire_binding() },
        ]
    }
}

pub fn derived_shape<'a>(entries: &'a [wgpu::BindGroupEntry<'a>; 3]) -> FieldShape<'a> {
    FieldShape {
        label_prefix: "visible ",
        shader_label: "glyph_field_derived.wgsl (visible)",
        shader_source: glyph_field_derived::GLYPH_FIELD_DERIVED_WGSL,
        extra_layout: &glyph_field_derived::pipeline::EXTRA_LAYOUT,
        extra_entries: entries,
    }
}

// ── headless ────────────────────────────────────────────────────────────────

/// Every segment of every line in `(item, line, byte)` order with its slot
/// base: the line prefix of `glyph_count` plus the seed's survivors-before.
pub fn all_segments(inputs: &VisibleInputs<'_>, survivors_before: &[u32]) -> (Vec<SegGpu>, u64) {
    let mut segs = Vec::with_capacity(inputs.lines.len() + inputs.seeds.len());
    let mut base = 0u64;
    let mut si = 0usize;
    for (i, it) in inputs.items.iter().enumerate() {
        for li in it.first_line..it.first_line + it.line_count {
            let line = &inputs.lines[li as usize];
            let len = if li + 1 < it.first_line + it.line_count {
                inputs.lines[li as usize + 1].byte_start - 1 - line.byte_start
            } else {
                it.byte_len - line.byte_start
            };
            while si < inputs.seeds.len() && inputs.seeds[si].line < li {
                si += 1;
            }
            let s0 = si;
            while si < inputs.seeds.len() && inputs.seeds[si].line == li {
                si += 1;
            }
            let line_seeds = &inputs.seeds[s0..si];
            let mut prev: Option<(usize, &crate::SegmentSeedGpu)> = None;
            for (k, s) in line_seeds.iter().enumerate() {
                assert!(s.byte_offset <= len, "seed {} of line {li} (item {i}) cuts at {} past the line's {len} bytes", s0 + k, s.byte_offset);
                let (off, col, cells, seg_adv, before) = match prev {
                    None => (0, 0, 0, 0.0, 0),
                    Some((pk, p)) => (p.byte_offset, p.col, p.cells, p.seg_adv, survivors_before[pk]),
                };
                segs.push(SegGpu { item: i as u32, line: li, byte_off: off, byte_end: s.byte_offset, col, cells, seg_adv, slot_base: (base + before as u64) as u32 });
                prev = Some((s0 + k, s));
            }
            let (off, col, cells, seg_adv, before) = match prev {
                None => (0, 0, 0, 0.0, 0),
                Some((pk, p)) => (p.byte_offset, p.col, p.cells, p.seg_adv, survivors_before[pk]),
            };
            segs.push(SegGpu { item: i as u32, line: li, byte_off: off, byte_end: len, col, cells, seg_adv, slot_base: (base + before as u64) as u32 });
            base += line.glyph_count as u64;
        }
    }
    (segs, base)
}

/// `crate::layout_all_lines`.
pub fn layout_all_lines(device: &wgpu::Device, queue: &wgpu::Queue, inputs: &VisibleInputs<'_>) -> Vec<DerivedSlot> {
    let resident = Resident::new(device, queue, inputs);
    let before = resident.read_survivors_before(queue);
    let (segs, total) = all_segments(inputs, &before);
    assert!(total <= u32::MAX as u64, "layout_all_lines: {total} slots exceed a u32");
    let mut out: Vec<DerivedSlot> = Vec::with_capacity(total as usize);

    // Batches of whole lines whose slots fit the readback budget; a batch's
    // segments are rebased to its first slot.
    let mut k = 0usize;
    while k < segs.len() {
        let batch_base = segs[k].slot_base as u64;
        let mut end = k;
        let mut batch_slots = 0u64;
        while end < segs.len() {
            // The slots a segment's LINE reaches: the next line's base, or the total.
            let line = segs[end].line;
            let mut e = end;
            while e < segs.len() && segs[e].line == line {
                e += 1;
            }
            let line_end = if e < segs.len() { segs[e].slot_base as u64 } else { total };
            let reach = line_end - batch_base;
            if reach > HEADLESS_BATCH_SLOTS && end > k {
                break;
            }
            batch_slots = reach;
            end = e;
        }
        let batch: Vec<SegGpu> = segs[k..end].iter().map(|s| SegGpu { slot_base: (s.slot_base as u64 - batch_base) as u32, ..*s }).collect();
        let segs_buf = storage_init(device, "visible headless segments", &batch, wgpu::BufferUsages::empty());
        let slots_buf = storage_zeroed(device, "visible headless slots", batch_slots * std::mem::size_of::<DerivedSlot>() as u64, wgpu::BufferUsages::COPY_SRC);
        let bg = resident.layout_bind_group(&segs_buf, &slots_buf);
        resident.write_layout_params(queue, batch.len() as u32, 0);
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("visible headless layout") });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("visible headless layout"), timestamp_writes: None });
            let [x, y, z] = plan_dispatch(batch.len() as u32);
            pass.set_bind_group(0, &bg, &[]);
            pass.set_pipeline(&resident.layout_pipeline);
            pass.dispatch_workgroups(x, y, z);
        }
        queue.submit([encoder.finish()]);
        let data = read_back(device, queue, &slots_buf, 0, batch_slots * std::mem::size_of::<DerivedSlot>() as u64);
        out.extend_from_slice(bytemuck::cast_slice(&data));
        k = end;
    }
    assert_eq!(out.len() as u64, total, "layout_all_lines: batches do not cover the slot total");
    out
}
