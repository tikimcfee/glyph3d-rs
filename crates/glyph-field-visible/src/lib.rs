//! The Visible glyph field — [`glyph_field::GlyphFieldMode::Visible`].
//!
//! No slot per glyph is kept. Resident on the GPU: the source bytes (1 B per
//! byte), the line table Pass 1 writes (`LineEntryGpu`, 16 B per line), the
//! long-line segment seeds (`SegmentSeedGpu`), an item table, the atlas trie
//! (`TrieUpload`) and the colour spans (`ByteSpanGpu`). Each frame
//! [`VisibleField::prepare`] culls the items' boxes and then their lines
//! against the camera, sorts every visible line into an LOD tier — GLYPHS
//! (laid out), WASH (one quad per line in its spans' mean colour) or
//! BACKDROP (the item's own quad, drawn by the scene as today) — lays the
//! glyph-tier segments out into a transient `DerivedSlot` buffer with the
//! same bits HyperLayout's Pass 2 would have written (seeded segments, the
//! trie and its emoji sequences, byte-range colour), and the Derived shader
//! draws that buffer through one indirect draw. Nothing is read back on the
//! frame path.
//!
//! Design and measurements: `out/GPU-DIRECTION-2026-10-09.md` (Round 2 and
//! the Linux-scale section); the milestone log and how to see each piece:
//! `out/VISIBLE-MODE.md`. The kernels descend from
//! `experiments/gpu-direction/{jit-layout,jit-text}`, which were held bit-equal
//! to HyperLayout on both rasterizers; the sixth hyper-oracle tier holds this
//! crate's [`layout_all_lines`] to the device Pass 2 the same way.
//!
//! THE CONTRACT WITH THE RENDERER (M2 skeleton, 2026-10-10): everything the
//! renderer hands in is in [`VisibleInputs`]; everything per frame is in
//! [`glyph_field::FramePrepare`]; what comes back for the HUD is
//! [`VisibleStats`]. The slot the draw reads is the Derived one, so the
//! shader, pipeline state and bind-group map are `glyph_field_derived`'s.

//!
//! HOW IT IS BUILT (M2, 2026-10-10). `gpu.rs` holds the device side —
//! [`gpu::Resident`] (the tables and the layout kernels, shared with the
//! headless [`layout_all_lines`]) and [`gpu::Frame`] (the cull, the
//! transient buffers, the indirect draws, the stats ring); `tables.rs` the
//! pure table builders and the records the WGSL reads; the three shaders
//! under `shaders/` (`visible_cull`, `visible_layout`, `visible_wash`). How
//! a seeded segment learns its slot count: at load, the layout kernel runs
//! in COUNT mode once per seed (`count_seed_segments`, then
//! `prefix_seed_survivors`) and leaves each seed's survivors-before
//! resident; cull B reserves a whole line's slots with one `atomicAdd` of
//! its `glyph_count` and gives segment k the base plus that prefix. The
//! public struct is unchanged; the side table is 4 B per seed.

use std::ops::Range;

use bytemuck::{Pod, Zeroable};
use glyph_field::{
    FieldCore, FieldResources, FieldTargets, FramePrepare, GlyphField, GlyphFieldMode, GlyphPlacement, ItemParamsGpu,
};
use glyph_field_derived::DerivedSlot;

pub mod gpu;
pub mod tables;
pub mod test_support;

pub use tables::{
    byte_chunk_shift, frustum_planes, fu_to_world, kernel_x, pack_trie, plan_dispatch, reference_x, FrameGpu, ItemGpu,
    LayoutParamsGpu, PackedTrie, SegGpu, TrieMetaGpu, WashGpu,
};

/// One line of one item, as Pass 1 writes it (`layout_hyper::LineEntry`,
/// the same 16 B). `byte_start` is item-relative.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
pub struct LineEntryGpu {
    pub byte_start: u32,
    pub item: u32,
    pub base_row: u32,
    pub glyph_count: u32,
}

/// A cut inside a long line and the fold state at it
/// (`layout_hyper::SegmentSeed`, the same 24 B). `byte_offset` is
/// line-relative; `col` the leaders before the cut; `seg_adv` the running
/// f32 segment advance; `cells` the line advance in cells.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct SegmentSeedGpu {
    pub line: u32,
    pub byte_offset: u32,
    pub col: u32,
    pub seg_adv: f32,
    pub cells: u32,
    pub _pad: u32,
}

/// A byte-range colour: `[start, end)` within its item, packed RGBA8.
/// Sorted by `start` per item, non-overlapping.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
pub struct ByteSpanGpu {
    pub start: u32,
    pub end: u32,
    pub color: u32,
}

/// Wrap mode as the kernel reads it.
pub const WRAP_DOWN: u32 = 0;
pub const WRAP_BACK: u32 = 1;

/// One item (file) as the kernel and the cull see it. `params` is the
/// Derived shader's own table row (Y/Z and the group derive from it); the
/// rest is what laying X out and culling needs and that row does not carry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VisibleItem {
    pub params: ItemParamsGpu,
    /// `ItemParams::origin_x`, in the fold's own f64: the kernel narrows x
    /// once, as Pass 2 does, from hi/lo halves of these (an f32 here cost an
    /// ulp on every page column past the first — found by the sixth oracle
    /// tier on paged-rows, 2026-10-10).
    pub origin_x: f64,
    /// The row-paging stride: `max_row_extent + page_gap_x` when
    /// `has_page && page_rows > 0`, else 0 (`layout_hyper::page::Pager`), f64
    /// for the same reason.
    pub stride_x: f64,
    pub wrap_width: i32,
    /// [`WRAP_DOWN`] or [`WRAP_BACK`].
    pub wrap_mode: u32,
    /// Whether the sequence pass runs for this item (cluster mode).
    pub cluster: u32,
    /// Where the item's bytes start in the resident byte buffer.
    pub byte_base: u64,
    pub byte_len: u32,
    /// Its lines in the table.
    pub first_line: u32,
    pub line_count: u32,
    /// Its colour spans.
    pub span_base: u32,
    pub span_count: u32,
    /// World-space box of its glyphs, group offset applied — what the scene
    /// culls today (`SegCull.min/max`).
    pub bbox_min: [f32; 3],
    pub bbox_max: [f32; 3],
    /// The scene's group row (the same as `params.group`).
    pub group_id: u32,
}

/// The atlas trie in the form the kernel reads: `codepoints.bin`'s sections
/// as plain words, plus the metrics every advance derives from. Built by
/// the renderer from its `TrieTable`; the crate never reads the atlas files.
#[derive(Clone, Debug, Default)]
pub struct TrieUpload {
    pub block_shift: u32,
    /// `block_index[cp >> block_shift]` → block number.
    pub block_index: Vec<u32>,
    /// `blocks[(block << block_shift | (cp & mask)) * 4 ..]` = glyph,
    /// advance_fu, height_fu, flags.
    pub blocks: Vec<u32>,
    pub entry_stride: u32,
    /// Sorted sequence records, `seq_max + 2` words each: slot, len, cps…
    pub sequences: Vec<u32>,
    pub seq_max: u32,
    /// One bit per codepoint (0..0x110000) that starts a sequence.
    pub seq_first_bitmap: Vec<u32>,
    pub em_height_fu: u32,
    pub primary_advance_fu: u32,
    pub bitmap_advance_fu: i32,
}

/// Everything the field is built from.
pub struct VisibleInputs<'a> {
    /// Each item's bytes, in item order; the field uploads them at
    /// `items[i].byte_base`.
    pub item_bytes: &'a [&'a [u8]],
    pub items: &'a [VisibleItem],
    pub lines: &'a [LineEntryGpu],
    /// Sorted by `(line, byte_offset)`.
    pub seeds: &'a [SegmentSeedGpu],
    /// The planned distance between cuts (`layout_hyper::SEGMENT_BYTES`).
    pub segment_bytes: u32,
    pub trie: &'a TrieUpload,
    /// All items' spans, each item's run sorted and `items[i].span_base..`.
    pub spans: &'a [ByteSpanGpu],
    /// The default glyph colour where no span covers a byte (packed RGBA8).
    pub default_color: u32,
}

/// A per-glyph edit keyed by (item, byte) — what the slot verbs become when
/// slots are transient (M3). Applied by the layout kernel to the glyph whose
/// leader byte is `byte`: `color` replaces the span colour when non-zero,
/// `x_nudge` is added to x (the Derived slot carries no y/z nudge, as in
/// Derived mode), and `group` (not [`NO_GROUP`]) becomes the slot's group via
/// the Derived override lane. One override per (item, byte); setting it
/// again replaces it, clearing removes it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlyphOverride {
    pub item: u32,
    pub byte: u32,
    pub color: u32,
    pub x_nudge: f32,
    pub group: u32,
}

/// `GlyphOverride::group` for "the item's own group".
pub const NO_GROUP: u32 = u32::MAX;

/// Sizing of the transient buffers.
#[derive(Clone, Copy, Debug)]
pub struct VisibleLimits {
    /// Slots the transient buffer holds; lines past it are dropped for the
    /// frame (and counted in the stats).
    pub max_slots: u32,
    /// Visible segment entries per frame.
    pub max_segments: u32,
    /// Wash quads per frame.
    pub max_wash: u32,
}

impl Default for VisibleLimits {
    fn default() -> Self {
        Self { max_slots: 16 << 20, max_segments: 1 << 20, max_wash: 1 << 20 }
    }
}

/// What the last prepared frame did, for the HUD. Counters come from a
/// readback that lags the frame it describes by one or two frames; nothing
/// on the frame path waits for it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VisibleStats {
    pub items_total: u32,
    pub items_visible: u32,
    pub items_backdrop: u32,
    pub lines_candidate: u32,
    pub lines_glyph: u32,
    pub lines_wash: u32,
    pub segments: u32,
    pub slots: u32,
    pub slots_dropped: u32,
    /// GPU time of the cull passes, the layout kernel and the glyph draw,
    /// when timestamps are available; else 0.
    pub cull_ms: f32,
    pub layout_ms: f32,
    pub draw_ms: f32,
}

/// The Visible field.
pub struct VisibleField {
    resident: gpu::Resident,
    frame: gpu::Frame,
    /// The Derived draw over the transient slot buffer (one chunk).
    core: FieldCore<DerivedSlot>,
    /// Bound as the Derived shader's group-override table (binding 10);
    /// all zero — a transient slot never carries an override.
    _group_overrides: wgpu::Buffer,
    /// Live slots in the one chunk, as the trait reports them: none that a
    /// caller may address (they are rebuilt every frame).
    chunk_counts: [u32; 1],
}

impl VisibleField {
    /// Build the field: upload the resident tables, create the cull and
    /// layout pipelines and the transient buffers, and the Derived draw over
    /// the transient slot buffer.
    ///
    /// The Derived item table (binding 8) is built from `inputs.items[i].params`;
    /// `resources.item_params` is not read. `limits.max_slots × 20 B` must
    /// fit the device's storage binding limit (asserted), and
    /// `max_segments` is rounded up to a whole workgroup.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        inputs: &VisibleInputs<'_>,
        resources: &FieldResources<'_>,
        targets: FieldTargets,
        limits: VisibleLimits,
    ) -> Self {
        let resident = gpu::Resident::new(device, queue, inputs);
        let frame = gpu::Frame::new(device, queue, &resident, resources, targets, limits);
        let group_overrides = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("visible group overrides (all zero)"),
            size: (glyph_field_derived::OVERRIDE_MAX as u64 + 1) * 4,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let entries = frame.derived_shape_entries(&resident, resources, &group_overrides);
        let shape = gpu::derived_shape(&entries);
        let core = FieldCore::new(device, frame.slot_storage(), resources, targets, &shape);
        Self { resident, frame, core, _group_overrides: group_overrides, chunk_counts: [0] }
    }

    /// Replace one item's colour spans (an edit arriving as byte ranges):
    /// in place when they fit the item's run (its spans at load plus an
    /// eighth and 16), else appended and remapped; refused with a warning
    /// when the table's tail is full too.
    pub fn set_item_spans(&self, queue: &wgpu::Queue, item: u32, spans: &[ByteSpanGpu]) {
        self.resident.set_item_spans(queue, item, spans);
    }

    /// Hide an item from every tier (the renderer's hidden flag); it is
    /// neither laid out nor counted visible.
    pub fn set_item_hidden(&self, queue: &wgpu::Queue, item: u32, hidden: bool) {
        self.resident.set_item_hidden(queue, item, hidden);
    }

    /// Replace the world box the item cull tests (after a group edit moves
    /// the item; the line cull reads the live group table and needs no
    /// update).
    pub fn set_item_bbox(&self, queue: &wgpu::Queue, item: u32, bbox_min: [f32; 3], bbox_max: [f32; 3]) {
        self.resident.set_item_bbox(queue, item, bbox_min, bbox_max);
    }

    // ── M3: edits and selection keyed by (item, byte) ─────────────────

    /// Set (or replace) the override of one glyph.
    pub fn set_glyph_override(&self, _queue: &wgpu::Queue, _ov: GlyphOverride) {
        todo!("M3: set_glyph_override")
    }

    /// Remove one glyph's override, if any.
    pub fn clear_glyph_override(&self, _queue: &wgpu::Queue, _item: u32, _byte: u32) {
        todo!("M3: clear_glyph_override")
    }

    /// Colour the byte range `[start, end)` of an item: the new span replaces
    /// whatever spans overlapped it (clipping them at its edges), the rest
    /// stay. A line recolour or a highlight run is this.
    pub fn set_item_span_range(&self, _queue: &wgpu::Queue, _item: u32, _start: u32, _end: u32, _color: u32) {
        todo!("M3: set_item_span_range")
    }

    /// Prepare the selection mask for the glyphs of item `item` whose leader
    /// byte lies in `[start, end)`: the layout kernel again, over only the
    /// visible segments that intersect the range, into a second transient
    /// buffer (the selection is drawn with the mask pipeline the scene
    /// creates, no depth, no blend). Call after `prepare` in the same encoder.
    pub fn prepare_mask(&self, _queue: &wgpu::Queue, _encoder: &mut wgpu::CommandEncoder, _item: u32, _start: u32, _end: u32) {
        todo!("M3: prepare_mask")
    }

    /// Record the mask draw of what `prepare_mask` emitted (an indirect draw
    /// over the selection buffer); the caller set the mask pipeline.
    pub fn record_mask_draw(&self, _pass: &mut wgpu::RenderPass<'_>) {
        todo!("M3: record_mask_draw")
    }

    /// Where the glyph at (item, byte) landed in the LAST prepared frame's
    /// transient buffer, if it was laid out: reads the segment list back
    /// (blocking; diagnostics such as GLYPH_G_DUMP, never the frame path)
    /// and counts survivors from the covering segment's start.
    pub fn locate(&self, _queue: &wgpu::Queue, _item: u32, _byte: u32) -> Option<u32> {
        todo!("M3: locate")
    }

    /// The last COMPLETED frame's counters (see [`VisibleStats`]); they lag
    /// the frame being prepared by two.
    pub fn stats(&self) -> VisibleStats {
        self.frame.stats()
    }

    /// Record the wash tier's quads (one per WASH-tier line) into a pass
    /// whose pipeline the field sets itself; drawn after the glyph pass.
    pub fn record_wash_draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        self.frame.record_wash_draw(pass);
    }

    /// The counters of the last prepared frame, read back BLOCKING — a test
    /// and diagnostic instrument, never the frame path. Indexed by
    /// [`tables::counter`].
    pub fn read_counters(&self, queue: &wgpu::Queue) -> [u32; tables::COUNTER_WORDS] {
        self.frame.read_counters(&self.resident, queue)
    }

    /// The first `count` transient slots of the last prepared frame, read
    /// back BLOCKING (tests).
    pub fn read_slots(&self, queue: &wgpu::Queue, count: u32) -> Vec<DerivedSlot> {
        self.frame.read_slots(&self.resident, queue, count)
    }

    /// The transient capacity in force (caps rounded as `new` applies them).
    pub fn limits(&self) -> VisibleLimits {
        self.frame.limits
    }
}

/// Headless: lay out EVERY line of `inputs` through the production kernel
/// (no cull, no LOD, every segment in line order) and read the slots back,
/// in the order HyperLayout's Pass 2 emits them (item, then line, then
/// column). The witness the sixth hyper-oracle tier diffs against the device
/// Pass 2's `DerivedSlot`s.
///
/// Blocking; batches the slots 4 M at a time, so a 90 M-slot tree needs no
/// 1.8 GB readback buffer. Slot bases come from the line table's
/// `glyph_count` and the seeds' survivors-before, so a kernel that emitted a
/// different count for some line would shift that line's slots — which the
/// diff would show as a run of mismatches from that line on.
pub fn layout_all_lines(device: &wgpu::Device, queue: &wgpu::Queue, inputs: &VisibleInputs<'_>) -> Vec<DerivedSlot> {
    gpu::layout_all_lines(device, queue, inputs)
}

impl GlyphField for VisibleField {
    fn mode(&self) -> GlyphFieldMode {
        GlyphFieldMode::Visible
    }
    /// The line table's survivors summed (what a full layout would emit;
    /// the scene's instance count). No slot is addressable across frames —
    /// the transient buffer is rebuilt every `prepare`.
    fn glyph_count(&self) -> u32 {
        self.resident.glyph_total.min(u32::MAX as u64) as u32
    }
    fn chunk_capacity(&self) -> u32 {
        self.frame.limits.max_slots
    }
    fn slot_bytes(&self) -> u32 {
        glyph_field_derived::slot::SLOT_BYTES as u32
    }
    fn chunk_glyph_counts(&self) -> &[u32] {
        &self.chunk_counts
    }
    fn glyph_pipeline(&self) -> &wgpu::RenderPipeline {
        self.core.glyph_pipeline()
    }
    fn create_mask_pipeline(
        &self,
        device: &wgpu::Device,
        mask_format: wgpu::TextureFormat,
        sample_count: u32,
    ) -> wgpu::RenderPipeline {
        self.core.create_mask_pipeline(device, mask_format, sample_count)
    }
    fn prepare(&self, queue: &wgpu::Queue, encoder: &mut wgpu::CommandEncoder, frame: &FramePrepare) {
        self.frame.prepare(&self.resident, queue, encoder, frame);
    }
    fn draws_itself(&self) -> bool {
        true
    }
    /// The ranges are ignored: one indirect draw of what `prepare` emitted.
    fn record_draws(&self, pass: &mut wgpu::RenderPass<'_>, _draws: &[(u32, Range<u32>)]) {
        self.frame.record_glyph_draw(&self.core, pass);
    }
    fn write_color(&self, _queue: &wgpu::Queue, _slot: u32, _rgba: u32) {}
    fn write_position(&self, _queue: &wgpu::Queue, _slot: u32, _position: [f32; 3]) {}
    fn write_extent(&self, _queue: &wgpu::Queue, _slot: u32, _advance: f32, _height: f32) {}
    fn write_group_id(&self, _queue: &wgpu::Queue, _slot: u32, _item: u32, _group_id: u32) {}
    fn write_placements(&self, _queue: &wgpu::Queue, _first_slot: u32, _placements: &[GlyphPlacement]) {}
    fn write_colors(&self, _queue: &wgpu::Queue, _first_slot: u32, _colors: &[u32]) {}
    fn read_slot_words(&self, _device: &wgpu::Device, _queue: &wgpu::Queue, _slot: u32, _out: &mut [u32]) {}
}
