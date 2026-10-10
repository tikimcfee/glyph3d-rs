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

use std::ops::Range;

use bytemuck::{Pod, Zeroable};
use glyph_field::{
    FieldResources, FieldTargets, FramePrepare, GlyphField, GlyphFieldMode, GlyphPlacement, ItemParamsGpu,
};
use glyph_field_derived::DerivedSlot;

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
    /// `ItemParams::origin_x`.
    pub origin_x: f32,
    /// The row-paging stride: `max_row_extent + page_gap_x` when
    /// `has_page && page_rows > 0`, else 0 (`layout_hyper::page::Pager`).
    pub stride_x: f32,
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
    _todo: (),
}

impl VisibleField {
    /// Build the field: upload the resident tables, create the cull and
    /// layout pipelines and the transient buffers, and the Derived draw over
    /// the transient slot buffer.
    pub fn new(
        _device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _inputs: &VisibleInputs<'_>,
        _resources: &FieldResources<'_>,
        _targets: FieldTargets,
        _limits: VisibleLimits,
    ) -> Self {
        todo!("M2: VisibleField::new")
    }

    /// Replace one item's colour spans (an edit arriving as byte ranges).
    pub fn set_item_spans(&self, _queue: &wgpu::Queue, _item: u32, _spans: &[ByteSpanGpu]) {
        todo!("M2: VisibleField::set_item_spans")
    }

    /// The last frame's counters (see [`VisibleStats`]).
    pub fn stats(&self) -> VisibleStats {
        todo!("M2: VisibleField::stats")
    }

    /// Record the wash tier's quads (one per WASH-tier line) into a pass
    /// whose pipeline the field sets itself; drawn after the glyph pass.
    pub fn record_wash_draw(&self, _pass: &mut wgpu::RenderPass<'_>) {
        todo!("M2: VisibleField::record_wash_draw")
    }
}

/// Headless: lay out EVERY line of `inputs` through the production kernel
/// (no cull, no LOD, every segment in line order) and read the slots back,
/// in the order HyperLayout's Pass 2 emits them (item, then line, then
/// column). The witness the sixth hyper-oracle tier diffs against the device
/// Pass 2's `DerivedSlot`s.
pub fn layout_all_lines(_device: &wgpu::Device, _queue: &wgpu::Queue, _inputs: &VisibleInputs<'_>) -> Vec<DerivedSlot> {
    todo!("M2: layout_all_lines")
}

impl GlyphField for VisibleField {
    fn mode(&self) -> GlyphFieldMode {
        GlyphFieldMode::Visible
    }
    fn glyph_count(&self) -> u32 {
        todo!()
    }
    fn chunk_capacity(&self) -> u32 {
        todo!()
    }
    fn slot_bytes(&self) -> u32 {
        glyph_field_derived::slot::SLOT_BYTES as u32
    }
    fn chunk_glyph_counts(&self) -> &[u32] {
        todo!()
    }
    fn glyph_pipeline(&self) -> &wgpu::RenderPipeline {
        todo!()
    }
    fn create_mask_pipeline(
        &self,
        _device: &wgpu::Device,
        _mask_format: wgpu::TextureFormat,
        _sample_count: u32,
    ) -> wgpu::RenderPipeline {
        todo!()
    }
    fn prepare(&self, _queue: &wgpu::Queue, _encoder: &mut wgpu::CommandEncoder, _frame: &FramePrepare) {
        todo!("M2: cull + layout")
    }
    fn draws_itself(&self) -> bool {
        true
    }
    fn record_draws(&self, _pass: &mut wgpu::RenderPass<'_>, _draws: &[(u32, Range<u32>)]) {
        todo!("M2: the indirect draw")
    }
    fn write_color(&self, _queue: &wgpu::Queue, _slot: u32, _rgba: u32) {}
    fn write_position(&self, _queue: &wgpu::Queue, _slot: u32, _position: [f32; 3]) {}
    fn write_extent(&self, _queue: &wgpu::Queue, _slot: u32, _advance: f32, _height: f32) {}
    fn write_group_id(&self, _queue: &wgpu::Queue, _slot: u32, _item: u32, _group_id: u32) {}
    fn write_placements(&self, _queue: &wgpu::Queue, _first_slot: u32, _placements: &[GlyphPlacement]) {}
    fn write_colors(&self, _queue: &wgpu::Queue, _first_slot: u32, _colors: &[u32]) {}
    fn read_slot_words(&self, _device: &wgpu::Device, _queue: &wgpu::Queue, _slot: u32, _out: &mut [u32]) {}
}
