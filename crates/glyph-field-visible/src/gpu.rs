//! The device side of the Visible field: the resident tables and the layout
//! kernels ([`Resident`], shared by the field and the headless
//! [`layout_all_lines`]), the per-frame cull ([`Frame`]: the three compute
//! passes `prepare` records, the indirect draws, the stats ring) and the
//! wash pipeline.
//!
//! BIND GROUPS. Three layouts, one per shader file: every entry point of a
//! file binds the whole set (an unused binding costs nothing), so a kernel
//! never needs its own group. The counts stay under Metal's 31 buffers per
//! stage: cull 15 storage + 1 uniform, layout 15 + 2 (M3 added the override
//! table and the mask's argument words), wash 3 + 1. The layout group is
//! built twice per frame object: over the frame's slot buffer and its
//! params, and over the MASK slot buffer and the mask's own params uniform
//! (a second uniform, not a rewrite: every `queue.write_buffer` of a frame
//! lands before its encoder runs, so one buffer could not carry both).
//!
//! THE FRAME PATH READS NOTHING BACK. Counters are copied into a ring of
//! three staging buffers; the copy's submit happens after `prepare`
//! returns, so the map is requested on the NEXT `prepare` and read on the
//! one after — `stats()` lags its frame by two. `device.poll(Poll)` runs
//! the map callbacks without waiting. `locate` and the `read_*` methods do
//! block: they are diagnostics and tests, never the frame path.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use glyph_field::{FieldCore, FieldResources, FieldShape, FieldTargets, FramePrepare, ItemParamsGpu, SlotChunk, SlotStorage};
use glyph_field_derived::DerivedSlot;
use wgpu::util::DeviceExt;

use crate::tables::{self, *};
use crate::walk;
use crate::{ByteSpanGpu, GlyphOverride, VisibleInputs, VisibleLimits, VisibleStats, NO_GROUP};

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

/// Block on a readback of `size` bytes (a multiple of 4) from `src` (a
/// verification path, never the frame's). Returned as WORDS so a cast to
/// any 4-aligned record is sound: a `Vec<u8>` has no alignment promise, and
/// a 20 B one came back 2 mod 4 on this box (2026-10-10) — the larger reads
/// had only ever been aligned by the allocator's habit.
fn read_back(device: &wgpu::Device, queue: &wgpu::Queue, src: &wgpu::Buffer, offset: u64, size: u64) -> Vec<u32> {
    assert!(size.is_multiple_of(4), "visible readback: {size} B is not whole words");
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
    let mut out = vec![0u32; (size / 4) as usize];
    bytemuck::cast_slice_mut::<u32, u8>(&mut out).copy_from_slice(&data[..size as usize]);
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
    /// Each item's spans as the device has them — what a range edit merges
    /// into (`set_item_span_range`).
    pub spans_host: RefCell<Vec<Vec<ByteSpanGpu>>>,
    /// The per-glyph override table (M3): `GlyphOverrideGpu` runs per item,
    /// and the host copy every edit is made on.
    pub overrides: wgpu::Buffer,
    pub override_alloc: RefCell<OverrideAlloc>,
    /// The Derived draw's group-override table (binding 10 of
    /// `glyph_field_derived.wgsl`): index k → group row, `OVERRIDE_MAX + 1`
    /// words, 0 unused. Rows are allocated per distinct group a glyph
    /// override names and never freed; `group_index` is the reverse map,
    /// `group_table` the host mirror.
    pub group_overrides: wgpu::Buffer,
    pub group_index: RefCell<HashMap<u32, u32>>,
    pub group_table: RefCell<Vec<u32>>,
    /// The trie as uploaded, kept for `locate`'s CPU walk (the words the
    /// kernel reads; a few MB for the atlas).
    pub trie_host: PackedTrie,
    pub line_starts: Vec<u32>,
    /// Per item `(first_line, line_count)`, its byte length, where its bytes
    /// landed `(chunk, offset)`, and whether it runs the sequence pass.
    pub item_lines: Vec<(u32, u32)>,
    pub item_byte_lens: Vec<u32>,
    pub item_place: Vec<(u32, u32)>,
    pub item_cluster: Vec<bool>,
    pub layout_bgl: wgpu::BindGroupLayout,
    pub layout_pipeline: wgpu::ComputePipeline,
    pub count_pipeline: wgpu::ComputePipeline,
    pub prefix_pipeline: wgpu::ComputePipeline,
    pub finalize_mask_pipeline: wgpu::ComputePipeline,
}

impl Resident {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, inputs: &VisibleInputs<'_>) -> Self {
        tables::check_inputs(inputs);
        let items = inputs.items;
        let limits = device.limits();
        let byte_shift = byte_chunk_shift(limits.max_storage_buffer_binding_size);
        let plan = plan_bytes(items, byte_shift);

        // The bytes: one buffer per chunk, written in place while mapped
        // (zero elsewhere), four bindings in the kernel. COPY_SRC for
        // `locate`, which reads one segment's bytes back rather than keep a
        // second copy of a tree on the host.
        let mut bytes = Vec::with_capacity(MAX_BYTE_CHUNKS);
        for (c, &size) in plan.chunk_sizes.iter().enumerate() {
            let padded = size.div_ceil(4) * 4;
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(&format!("visible bytes {c}/{}", plan.chunk_sizes.len())),
                size: padded.max(16),
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
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
        let override_alloc = OverrideAlloc::new(items.len());
        let overrides = storage_zeroed(device, "visible glyph overrides", override_alloc.capacity as u64 * std::mem::size_of::<GlyphOverrideGpu>() as u64, copy_dst);
        let group_overrides = storage_zeroed(device, "visible group overrides (Derived binding 10)", (glyph_field_derived::OVERRIDE_MAX as u64 + 1) * 4, copy_dst);

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
                storage_entry(15, true, compute),
                storage_entry(16, false, compute),
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
        let finalize_mask_pipeline = compute_pipeline(device, &pl, &module, "finalize_mask");

        let spans_host: Vec<Vec<ByteSpanGpu>> = items.iter().map(|it| inputs.spans[it.span_base as usize..(it.span_base + it.span_count) as usize].to_vec()).collect();
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
            spans_host: RefCell::new(spans_host),
            overrides,
            override_alloc: RefCell::new(override_alloc),
            group_overrides,
            group_index: RefCell::new(HashMap::new()),
            group_table: RefCell::new(vec![0]),
            trie_host: packed,
            line_starts: inputs.lines.iter().map(|l| l.byte_start).collect(),
            item_lines: items.iter().map(|it| (it.first_line, it.line_count)).collect(),
            item_byte_lens: items.iter().map(|it| it.byte_len).collect(),
            item_place: plan.place,
            item_cluster: items.iter().map(|it| it.cluster != 0).collect(),
            layout_bgl,
            layout_pipeline,
            count_pipeline,
            prefix_pipeline,
            finalize_mask_pipeline,
        };
        this.count_seed_survivors(queue);
        this
    }

    /// A layout bind group over `segs` and `slots`, told by `params` (the
    /// frame's or the mask's uniform) and with `mask_args` for the mask
    /// counter (any 48 B storage buffer when the dispatch is not a mask).
    pub fn layout_bind_group(&self, params: &wgpu::Buffer, segs: &wgpu::Buffer, slots: &wgpu::Buffer, mask_args: &wgpu::Buffer) -> wgpu::BindGroup {
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("visible layout bg"),
            layout: &self.layout_bgl,
            entries: &[
                bind(0, params),
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
                bind(15, &self.overrides),
                bind(16, mask_args),
            ],
        })
    }

    /// The frame's (or the headless) parameters into the resident uniform.
    pub fn write_layout_params(&self, queue: &wgpu::Queue, count: u32, debug_tint: u32) {
        let p = LayoutParamsGpu::all(count, debug_tint, self.default_color, self.byte_shift);
        queue.write_buffer(&self.layout_params, 0, bytemuck::bytes_of(&p));
    }

    /// A parameter block into any params uniform (the mask's).
    pub fn write_params_to(&self, queue: &wgpu::Queue, buffer: &wgpu::Buffer, params: &LayoutParamsGpu) {
        queue.write_buffer(buffer, 0, bytemuck::bytes_of(params));
    }

    /// A 48 B storage buffer for the mask-args binding of a dispatch that is
    /// not a mask.
    pub fn dummy_mask_args(&self, label: &str) -> wgpu::Buffer {
        storage_zeroed(&self.device, label, (MASK_ARGS_WORDS * 4) as u64, wgpu::BufferUsages::empty())
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
        let dummy_mask = self.dummy_mask_args("visible mask args (seed count)");
        let bg = self.layout_bind_group(&self.layout_params, &dummy_segs, &dummy_slots, &dummy_mask);
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
        read_back(&self.device, queue, &self.survivors, n * 4, n * 4)
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
        self.spans_host.borrow_mut()[i] = spans.to_vec();
    }

    /// `crate::VisibleField::set_item_span_range`: merge into the host copy
    /// (`merge_span_range`), then the whole-item path above.
    pub fn set_item_span_range(&self, queue: &wgpu::Queue, item: u32, start: u32, end: u32, color: u32) {
        assert!(item < self.items_total, "set_item_span_range: item {item} of {}", self.items_total);
        let end = end.min(self.item_byte_lens[item as usize]);
        if start >= end {
            return;
        }
        let mut merged = self.spans_host.borrow()[item as usize].clone();
        merge_span_range(&mut merged, start, end, color);
        self.set_item_spans(queue, item, &merged);
    }

    // ── per-glyph overrides (M3) ──────────────────────────────────────

    /// The index of `group` in the Derived group-override table, allocating
    /// a row the first time a group is named; 0 (the group part dropped,
    /// with a warning) once the 4,095 rows are in use.
    fn group_index_for(&self, queue: &wgpu::Queue, group: u32) -> u32 {
        if group == NO_GROUP {
            return 0;
        }
        if let Some(&k) = self.group_index.borrow().get(&group) {
            return k;
        }
        let mut table = self.group_table.borrow_mut();
        let k = table.len() as u32;
        if k > glyph_field_derived::OVERRIDE_MAX {
            log::warn!("visible field: out of group overrides ({} in use); the glyph keeps its item's group", glyph_field_derived::OVERRIDE_MAX);
            return 0;
        }
        table.push(group);
        self.group_index.borrow_mut().insert(group, k);
        queue.write_buffer(&self.group_overrides, k as u64 * 4, bytemuck::bytes_of(&group));
        k
    }

    /// The group-override table as the device has it (index → group row;
    /// `[0]` is unused).
    pub fn group_override_table(&self) -> Vec<u32> {
        self.group_table.borrow().clone()
    }

    /// `crate::VisibleField::set_glyph_override`. An override that changes
    /// nothing (colour 0, no nudge, `NO_GROUP`) clears.
    pub fn set_glyph_override(&self, queue: &wgpu::Queue, ov: GlyphOverride) {
        assert!(ov.item < self.items_total, "set_glyph_override: item {} of {}", ov.item, self.items_total);
        let i = ov.item as usize;
        if ov.byte >= self.item_byte_lens[i] {
            log::warn!("visible field: override at byte {} of item {} ({} bytes); ignored", ov.byte, ov.item, self.item_byte_lens[i]);
            return;
        }
        let rec = override_gpu(&ov, self.group_index_for(queue, ov.group));
        self.apply_override_edit(queue, i, rec);
    }

    /// `crate::VisibleField::clear_glyph_override`.
    pub fn clear_glyph_override(&self, queue: &wgpu::Queue, item: u32, byte: u32) {
        assert!(item < self.items_total, "clear_glyph_override: item {item} of {}", self.items_total);
        self.apply_override_edit(queue, item as usize, GlyphOverrideGpu { byte, ..Default::default() });
    }

    /// The host edit, then its mirror on the device: the run rewritten from
    /// the edit on (in place) or whole (after a remap), and the item row's
    /// `(override_base, override_count)`.
    fn apply_override_edit(&self, queue: &wgpu::Queue, item: usize, rec: GlyphOverrideGpu) {
        let mut alloc = self.override_alloc.borrow_mut();
        let edit = alloc.edit(item, rec);
        let (base, _) = alloc.runs[item];
        let list = &alloc.items[item];
        let rewrite_from = match edit {
            OverrideEdit::Unchanged => return,
            OverrideEdit::Refused => {
                log::warn!(
                    "visible field: item {item} needs {} override slots and the table has {} free; the edit is dropped",
                    list.len() + 1 + span_slack(list.len() as u32 + 1) as usize,
                    alloc.capacity - alloc.next_free
                );
                return;
            }
            OverrideEdit::InPlace { from } => from,
            OverrideEdit::Remapped { .. } => 0,
        };
        if rewrite_from < list.len() {
            let stride = std::mem::size_of::<GlyphOverrideGpu>() as u64;
            queue.write_buffer(&self.overrides, (base as u64 + rewrite_from as u64) * stride, bytemuck::cast_slice(&list[rewrite_from..]));
        }
        let row_offset = item as u64 * std::mem::size_of::<ItemGpu>() as u64 + std::mem::offset_of!(ItemGpu, override_base) as u64;
        queue.write_buffer(&self.items, row_offset, bytemuck::cast_slice(&[base, list.len() as u32]));
    }

    /// The overrides of one item as the device has them (tests).
    pub fn item_overrides(&self, item: u32) -> Vec<GlyphOverrideGpu> {
        self.override_alloc.borrow().items[item as usize].clone()
    }

    /// The bytes `[start, end)` of an item, read back from the resident
    /// chunk (blocking; `locate`). Copies are 4-aligned, so the read is
    /// widened to word bounds and trimmed.
    pub fn read_item_bytes(&self, queue: &wgpu::Queue, item: u32, start: u32, end: u32) -> Vec<u8> {
        let (chunk, chunk_off) = self.item_place[item as usize];
        let buffer = &self.bytes[chunk as usize];
        let from = (chunk_off + start) as u64 & !3;
        let to = ((chunk_off + end) as u64).div_ceil(4) * 4;
        let to = to.min(buffer.size());
        if to <= from {
            return Vec::new();
        }
        let words = read_back(&self.device, queue, buffer, from, to - from);
        let bytes: &[u8] = bytemuck::cast_slice(&words);
        let skip = ((chunk_off + start) as u64 - from) as usize;
        let take = (end - start) as usize;
        bytes[skip..(skip + take).min(bytes.len())].to_vec()
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
    /// The selection mask (M3): its own transient slot buffer
    /// (`mask_capacity` slots), the kernel's counter and draw words
    /// (`MASK_ARGS_WORDS`), the INDIRECT copy the mask draw reads, the
    /// mask dispatch's own params uniform and its layout bind group (over
    /// the FRAME's segment list and the mask slots).
    pub mask_slots: wgpu::Buffer,
    pub mask_args: wgpu::Buffer,
    pub mask_indirect: wgpu::Buffer,
    pub mask_capacity: u32,
    mask_params: wgpu::Buffer,
    mask_layout_bg: wgpu::BindGroup,
    cull_bg: wgpu::BindGroup,
    cull_items: wgpu::ComputePipeline,
    prefix_items: wgpu::ComputePipeline,
    compact_items: wgpu::ComputePipeline,
    cull_lines: wgpu::ComputePipeline,
    scan_blocks: wgpu::ComputePipeline,
    emit_lines: wgpu::ComputePipeline,
    finalize: wgpu::ComputePipeline,
    layout_bg: wgpu::BindGroup,
    wash_pipeline: wgpu::RenderPipeline,
    wash_bg: wgpu::BindGroup,
    /// The identity 0..36: a wash is a box of six faces, and the vertex
    /// stage turns the index into a cube corner (`visible_wash.wgsl`).
    wash_index: wgpu::Buffer,
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
        // C30: the scans' scratch (`visible_cull.wgsl` ORDER). A candidate
        // line is a line of a visible item, so `lines_total` bounds them.
        let item_flag = storage_zeroed(device, "visible item flags", n_items * 4, wgpu::BufferUsages::empty());
        let line_rec_bytes = resident.lines_total.max(1) as u64 * 8;
        assert!(
            line_rec_bytes <= binding_limit,
            "visible field: {} lines need {line_rec_bytes} B of cull records, over the storage binding limit {binding_limit}",
            resident.lines_total
        );
        let line_rec = storage_zeroed(device, "visible line records", line_rec_bytes, wgpu::BufferUsages::empty());
        // Block sums: the item blocks' in cull A, then the line blocks' in cull B.
        let blocks = (resident.lines_total.max(resident.items_total) as u64).div_ceil(CULL_LINES_WORKGROUP as u64).max(1);
        let block_sums = storage_zeroed(device, "visible cull block sums", blocks * 16, wgpu::BufferUsages::empty());
        let counters = storage_zeroed(device, "visible counters", STATS_COUNTERS_BYTES, wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC);
        // COPY_SRC on the segment list is for `locate`'s readback.
        let segs = storage_zeroed(device, "visible segments", limits.max_segments as u64 * std::mem::size_of::<SegGpu>() as u64, wgpu::BufferUsages::COPY_SRC);
        // COPY_SRC on the wash list is for the tests' extent witness.
        let wash = storage_zeroed(device, "visible wash boxes", limits.max_wash as u64 * std::mem::size_of::<WashGpu>() as u64, wgpu::BufferUsages::COPY_SRC);
        let slots = storage_zeroed(device, "visible transient slots", slot_bytes, wgpu::BufferUsages::COPY_SRC);
        let args = storage_zeroed(device, "visible dispatch/draw args (storage)", (INDIRECT_WORDS * 4) as u64, wgpu::BufferUsages::COPY_SRC);
        let indirect = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("visible indirect args"),
            size: (INDIRECT_WORDS * 4) as u64,
            usage: wgpu::BufferUsages::INDIRECT | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let box_indices: Vec<u16> = (0u16..36).collect();
        let wash_index = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("visible wash box index buffer"),
            contents: bytemuck::cast_slice(&box_indices),
            usage: wgpu::BufferUsages::INDEX,
        });

        // The selection mask's buffers. COPY_SRC on the indirect copy is
        // for the tests, which read the draw's instance count back.
        let mask_capacity = limits.max_slots.min(MASK_SLOTS_MAX);
        let mask_slots = storage_zeroed(device, "visible mask slots", mask_capacity as u64 * std::mem::size_of::<DerivedSlot>() as u64, wgpu::BufferUsages::COPY_SRC);
        let mask_args = storage_zeroed(device, "visible mask args (storage)", (MASK_ARGS_WORDS * 4) as u64, wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST);
        let mask_indirect = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("visible mask indirect args"),
            size: 20,
            usage: wgpu::BufferUsages::INDIRECT | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let mask_params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("visible mask layout params"),
            size: std::mem::size_of::<LayoutParamsGpu>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
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
                storage_entry(16, false, compute),
                storage_entry(17, false, compute),
                storage_entry(18, false, compute),
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
                bind(16, &item_flag),
                bind(17, &line_rec),
                bind(18, &block_sums),
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
        let compact_items = compute_pipeline(device, &cull_pl, &cull_module, "compact_items");
        let cull_lines = compute_pipeline(device, &cull_pl, &cull_module, "cull_lines");
        let scan_blocks = compute_pipeline(device, &cull_pl, &cull_module, "scan_blocks");
        let emit_lines = compute_pipeline(device, &cull_pl, &cull_module, "emit_lines");
        let finalize = compute_pipeline(device, &cull_pl, &cull_module, "finalize");
        let layout_bg = resident.layout_bind_group(&resident.layout_params, &segs, &slots, &mask_args);
        let mask_layout_bg = resident.layout_bind_group(&mask_params, &segs, &mask_slots, &mask_args);

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
            mask_slots,
            mask_args,
            mask_indirect,
            mask_capacity,
            mask_params,
            mask_layout_bg,
            cull_bg,
            cull_items,
            prefix_items,
            compact_items,
            cull_lines,
            scan_blocks,
            emit_lines,
            finalize,
            layout_bg,
            wash_pipeline,
            wash_bg,
            wash_index,
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
        // No selection until `prepare_mask` says otherwise: a mask draw
        // recorded this frame without one draws zero instances (the write
        // lands before the encoder, so a later `prepare_mask` copy wins).
        queue.write_buffer(&self.mask_indirect, 0, bytemuck::cast_slice(&[6u32, 0, 0, 0, 0]));

        // The cull's time spans every cull pass (the begin of the first to
        // the end of the last, C30), the layout's its one pass.
        let ts = |begin: Option<u32>, end: Option<u32>| {
            self.timestamps.as_ref().map(|(set, _, _)| wgpu::ComputePassTimestampWrites {
                query_set: set,
                beginning_of_pass_write_index: begin,
                end_of_pass_write_index: end,
            })
        };
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("visible cull A"), timestamp_writes: ts(Some(0), None) });
            pass.set_bind_group(0, &self.cull_bg, &[]);
            // Verdict and block sums per item, their scan, the compaction
            // (C30: the visible items in index order, every frame).
            let [x, y, z] = plan_dispatch_wg(resident.items_total, CULL_LINES_WORKGROUP);
            pass.set_pipeline(&self.cull_items);
            pass.dispatch_workgroups(x, y, z);
            pass.set_pipeline(&self.prefix_items);
            pass.dispatch_workgroups(1, 1, 1);
            pass.set_pipeline(&self.compact_items);
            pass.dispatch_workgroups(x, y, z);
        }
        encoder.copy_buffer_to_buffer(&self.args, INDIRECT_CULL_B, &self.indirect, INDIRECT_CULL_B, 12);
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("visible cull B"), timestamp_writes: ts(None, Some(1)) });
            pass.set_bind_group(0, &self.cull_bg, &[]);
            // Classify every candidate line, scan the workgroups' counts,
            // write at the scanned offsets (C30: arena order, every frame).
            pass.set_pipeline(&self.cull_lines);
            pass.dispatch_workgroups_indirect(&self.indirect, INDIRECT_CULL_B);
            pass.set_pipeline(&self.scan_blocks);
            pass.dispatch_workgroups(1, 1, 1);
            pass.set_pipeline(&self.emit_lines);
            pass.dispatch_workgroups_indirect(&self.indirect, INDIRECT_CULL_B);
            pass.set_pipeline(&self.finalize);
            pass.dispatch_workgroups(1, 1, 1);
        }
        encoder.copy_buffer_to_buffer(&self.args, INDIRECT_LAYOUT, &self.indirect, INDIRECT_LAYOUT, (INDIRECT_WORDS * 4) as u64 - INDIRECT_LAYOUT);
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("visible layout"), timestamp_writes: ts(Some(2), Some(3)) });
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
        pass.set_index_buffer(self.wash_index.slice(..), wgpu::IndexFormat::Uint16);
        pass.draw_indexed_indirect(&self.indirect, INDIRECT_WASH_DRAW);
    }

    /// The first `count` wash entries of the last prepared frame, read back
    /// blocking (tests).
    pub fn read_wash(&self, resident: &Resident, queue: &wgpu::Queue, count: u32) -> Vec<WashGpu> {
        let count = count.min(self.limits.max_wash) as u64;
        let data = read_back(&resident.device, queue, &self.wash, 0, count * std::mem::size_of::<WashGpu>() as u64);
        bytemuck::cast_slice(&data).to_vec()
    }

    /// The counters of the LAST prepared frame, read back blocking — a test
    /// instrument, never the frame path.
    pub fn read_counters(&self, resident: &Resident, queue: &wgpu::Queue) -> [u32; COUNTER_WORDS] {
        let data = read_back(&resident.device, queue, &self.counters, 0, STATS_COUNTERS_BYTES);
        let mut out = [0u32; COUNTER_WORDS];
        out.copy_from_slice(&data);
        out
    }

    /// The first `count` transient slots, read back blocking (tests).
    pub fn read_slots(&self, resident: &Resident, queue: &wgpu::Queue, count: u32) -> Vec<DerivedSlot> {
        let count = count.min(self.limits.max_slots) as u64;
        let data = read_back(&resident.device, queue, &self.slots, 0, count * std::mem::size_of::<DerivedSlot>() as u64);
        bytemuck::cast_slice(&data).to_vec()
    }

    // ── the selection mask (M3) ───────────────────────────────────────

    /// `crate::VisibleField::prepare_mask`: the layout kernel in MASK mode
    /// over the frame's segment list (the same indirect dispatch size as the
    /// layout pass — every entry is visited, those not the item's or outside
    /// the range return at once), `finalize_mask`, and the draw words
    /// copied into the INDIRECT buffer. Per call: two dispatches, one 20 B
    /// copy, two `write_buffer`s (the counter's 16 B and the 48 B params).
    pub fn prepare_mask(&self, resident: &Resident, queue: &wgpu::Queue, encoder: &mut wgpu::CommandEncoder, item: u32, start: u32, end: u32) {
        if item >= resident.items_total {
            log::warn!("visible field: prepare_mask item {item} of {}; nothing selected", resident.items_total);
            return;
        }
        let end = end.min(resident.item_byte_lens[item as usize]);
        if start >= end {
            return; // `prepare` already zeroed the draw
        }
        queue.write_buffer(&self.mask_args, 0, &[0u8; 16]);
        let params = LayoutParamsGpu::mask(self.limits.max_segments, resident.default_color, resident.byte_shift, item, start, end, self.mask_capacity);
        resident.write_params_to(queue, &self.mask_params, &params);
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("visible selection mask"), timestamp_writes: None });
            pass.set_bind_group(0, &self.mask_layout_bg, &[]);
            pass.set_pipeline(&resident.layout_pipeline);
            pass.dispatch_workgroups_indirect(&self.indirect, INDIRECT_LAYOUT);
            pass.set_pipeline(&resident.finalize_mask_pipeline);
            pass.dispatch_workgroups(1, 1, 1);
        }
        encoder.copy_buffer_to_buffer(&self.mask_args, MASK_ARGS_DRAW, &self.mask_indirect, 0, 20);
    }

    /// `crate::VisibleField::record_mask_draw`.
    pub fn record_mask_draw(&self, mask_core: &FieldCore<DerivedSlot>, pass: &mut wgpu::RenderPass<'_>) {
        mask_core.record_draw_indirect(pass, 0, &self.mask_indirect, 0);
    }

    /// The Derived draw's storage over the mask buffer: one chunk.
    pub fn mask_slot_storage(&self) -> SlotStorage<DerivedSlot> {
        let n = self.mask_capacity as usize;
        SlotStorage::new(vec![SlotChunk { buffer: self.mask_slots.clone(), offset: 0, slots: n as u32 }], n, n, None)
    }

    /// The mask draw's instance count as the last `prepare_mask` left it
    /// (blocking; tests): the INDIRECT copy's second word.
    pub fn read_mask_count(&self, resident: &Resident, queue: &wgpu::Queue) -> u32 {
        read_back(&resident.device, queue, &self.mask_indirect, 0, 20)[1]
    }

    /// The first `count` mask slots (blocking; tests).
    pub fn read_mask_slots(&self, resident: &Resident, queue: &wgpu::Queue, count: u32) -> Vec<DerivedSlot> {
        let count = count.min(self.mask_capacity) as u64;
        let data = read_back(&resident.device, queue, &self.mask_slots, 0, count * std::mem::size_of::<DerivedSlot>() as u64);
        bytemuck::cast_slice(&data).to_vec()
    }

    // ── locate (M3) ──────────────────────────────────────────────────

    /// `crate::VisibleField::locate`: three blocking readbacks (the
    /// counters, the segment list, the covering segment's bytes) and the
    /// CPU walk (`walk.rs`) counting survivors to the byte.
    pub fn locate(&self, resident: &Resident, queue: &wgpu::Queue, item: u32, byte: u32) -> Option<u32> {
        if item >= resident.items_total || byte >= resident.item_byte_lens[item as usize] {
            return None;
        }
        let counters = self.read_counters(resident, queue);
        let seg_count = counters[counter::SEG_FIT_END].min(self.limits.max_segments);
        if seg_count == 0 {
            return None;
        }
        let data = read_back(&resident.device, queue, &self.segs, 0, seg_count as u64 * std::mem::size_of::<SegGpu>() as u64);
        let segs: &[SegGpu] = bytemuck::cast_slice(&data);
        let line_start = |s: &SegGpu| resident.line_starts[s.line as usize];
        let seg = segs.iter().find(|s| s.item == item && s.byte_end > s.byte_off && line_start(s) + s.byte_off <= byte && byte < line_start(s) + s.byte_end)?;
        let seg_start = line_start(seg) + seg.byte_off;
        let seg_end = line_start(seg) + seg.byte_end;
        // The walk reads up to three bytes past the segment's end for a lead
        // cut there (zero past the item, as the kernel reads).
        let read_end = (seg_end + 3).min(resident.item_byte_lens[item as usize]);
        let bytes = resident.read_item_bytes(queue, item, seg_start, read_end);
        let survivors = walk::surviving_leader_offsets(&resident.trie_host, &bytes, (seg_end - seg_start) as usize, resident.item_cluster[item as usize]);
        let k = survivors.binary_search(&(byte - seg_start)).ok()?;
        Some(seg.slot_base + k as u32)
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

/// `crate::layout_all_lines_with_overrides`.
pub fn layout_all_lines(device: &wgpu::Device, queue: &wgpu::Queue, inputs: &VisibleInputs<'_>, overrides: &[GlyphOverride]) -> crate::HeadlessLayout {
    let resident = Resident::new(device, queue, inputs);
    for ov in overrides {
        resident.set_glyph_override(queue, *ov);
    }
    let dummy_mask = resident.dummy_mask_args("visible mask args (headless)");
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
        let bg = resident.layout_bind_group(&resident.layout_params, &segs_buf, &slots_buf, &dummy_mask);
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
    crate::HeadlessLayout { slots: out, group_overrides: resident.group_override_table() }
}
