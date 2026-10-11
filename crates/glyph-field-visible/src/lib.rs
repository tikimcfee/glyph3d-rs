//! The Visible glyph field — [`glyph_field::GlyphFieldMode::Visible`].
//!
//! No slot per glyph is kept. Resident on the GPU: the source bytes (1 B per
//! byte), the line table Pass 1 writes (`LineEntryGpu`, 24 B per line), the
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
//! resident; cull B gives a whole line its slots at a scanned base (its
//! `glyph_count` summed over the lines before it in (item, line) order,
//! C30 — an `atomicAdd` until then) and segment k the base plus that
//! prefix. The public struct is unchanged; the side table is 4 B per seed.
//!
//! EDITS AND SELECTION KEYED BY (ITEM, BYTE) (M3, 2026-10-10). A slot here
//! lives one frame and lands where the cull puts it (arena order since C30,
//! but only among the lines in view), so nothing the renderer keyed by slot
//! survives; every such consumer is re-keyed:
//!
//! - **Per-glyph verbs** → [`VisibleField::set_glyph_override`] /
//!   [`VisibleField::clear_glyph_override`]: a resident override table
//!   (`tables::GlyphOverrideGpu`, 16 B, one run per item sorted by byte,
//!   allocated like the spans) the layout kernel walks beside the spans.
//!   Colour beats the span's; the x nudge is an f32 add after the single
//!   rounding (NOT part of the oracle contract — the oracle has no nudges;
//!   `tests/gpu.rs` holds it to the host's f32 add); a group makes the slot's
//!   lane `glyph_field_derived::override_lane(item, k)`, k a row the field
//!   allocates in the Derived draw's group-override table (binding 10; 4,095
//!   rows, one per distinct group named, never freed).
//! - **Line and highlight recolours** → [`VisibleField::set_item_span_range`]:
//!   a merge into the item's spans on the host, uploaded through
//!   `set_item_spans`.
//! - **The selection mask** → [`VisibleField::prepare_mask`] +
//!   [`VisibleField::record_mask_draw`]: the layout kernel again in MASK
//!   mode, over the frame's own segment list, emitting only the item's
//!   leaders in the byte range into a SECOND transient buffer with its own
//!   indirect draw; a second `FieldCore` over that buffer gives the scene's
//!   mask pipeline (built from the main core; identical layout) its bind
//!   group. A line in the WASH tier has no segment entry and so no mask —
//!   a selection there draws nothing, by construction.
//! - **`GLYPH_G_DUMP`** → [`VisibleField::locate`] names the transient slot
//!   of (item, byte) in the last frame by reading the segment list back and
//!   re-walking the covering segment on the CPU (`walk.rs`, over the packed
//!   trie the kernel reads); `GlyphField::read_slot_words` then reads it.
//!
//! Costs: the layout bind group gains two bindings (15 storage + 2 uniform
//! per stage); the frame path gains one 20 B `write_buffer` (the mask draw's
//! reset); a selection costs two dispatches (one indirect, the frame's
//! segment count, early-out per entry) and a 20 B copy; resident memory
//! grows by the override table (1 MiB, or 64 B per item past 16 K items),
//! the group table (16 KiB) and the mask slots (`mask_capacity` × 20 B,
//! 20 MiB at the default limits).

use std::ops::Range;

use bytemuck::{Pod, Zeroable};
use glyph_field::{
    FieldCore, FieldResources, FieldTargets, FramePrepare, GlyphField, GlyphFieldMode, GlyphPlacement, ItemParamsGpu,
};
use glyph_field_derived::DerivedSlot;

pub mod gpu;
pub mod tables;
pub mod test_support;
pub mod walk;

pub use tables::{
    byte_chunk_shift, frustum_planes, fu_to_world, kernel_x, merge_span_range, pack_trie, plan_dispatch, reference_x,
    FrameGpu, GlyphOverrideGpu, ItemGpu, LayoutParamsGpu, PackedTrie, SegGpu, TrieMetaGpu, WashGpu,
};

/// One line of one item, as Pass 1 writes it (`layout_hyper::LineEntry`,
/// the same 24 B). `byte_start` is item-relative; `cols` the line's leaders
/// (the fold's column at its end, the newline excluded), from which the cull
/// counts its depth segments and column pages; `width_cells` its widest
/// fold unit in cells — the whole line's advance when the item has no fold
/// unit — which bounds the cull's x and is the wash box's width (C28,
/// 2026-10-10; before it the wash drew the cull's `2 × fold_unit` byte bound).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
pub struct LineEntryGpu {
    pub byte_start: u32,
    pub item: u32,
    pub base_row: u32,
    pub glyph_count: u32,
    pub cols: u32,
    pub width_cells: u32,
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
        // max_wash 4 Mi (144 MiB of 36 B boxes; 1 Mi until 2026-10-10): a
        // library deck lays out every page behind its head, and a near view
        // of the kernel tree's biggest volume reserved 1.26 M wash boxes.
        Self { max_slots: 16 << 20, max_segments: 1 << 20, max_wash: 4 << 20 }
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
    /// Wash boxes reserved past `VisibleLimits::max_wash` this frame: the
    /// cull writes none of them, so those lines draw NOTHING — the last in
    /// (item, line) order, the same every frame. Counted from 2026-10-10;
    /// before, the stats said "0 dropped" while a kernel-tree library view
    /// reserved 1,264,446 boxes against the 1,048,576 cap.
    pub wash_dropped: u32,
    /// GPU time of the cull passes, the layout kernel and the glyph draw,
    /// when timestamps are available; else 0.
    pub cull_ms: f32,
    pub layout_ms: f32,
    pub draw_ms: f32,
}

/// What the headless layout returns: the slots in Pass 2 order, and the
/// group-override table the overrides' group indices point into (index k →
/// group row; `[0]` unused), so a caller can turn a slot's lane back into a
/// group.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HeadlessLayout {
    pub slots: Vec<DerivedSlot>,
    pub group_overrides: Vec<u32>,
}

/// The Visible field.
pub struct VisibleField {
    resident: gpu::Resident,
    frame: gpu::Frame,
    /// The Derived draw over the transient slot buffer (one chunk).
    core: FieldCore<DerivedSlot>,
    /// The same shader and layout over the selection MASK buffer: the
    /// scene's mask pipeline (built from `core`) binds this core's group,
    /// which wgpu accepts because identical layouts are one layout (the
    /// device's bind-group-layout pool dedups by entries).
    mask_core: FieldCore<DerivedSlot>,
    /// Live slots in the one chunk, as the trait reports them: none that a
    /// caller may address (they are rebuilt every frame).
    chunk_counts: [u32; 1],
}

impl VisibleField {
    /// Build the field: upload the resident tables, create the cull and
    /// layout pipelines and the transient buffers, and the Derived draw over
    /// the transient slot buffer (and a second over the mask buffer).
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
        let entries = frame.derived_shape_entries(&resident, resources, &resident.group_overrides);
        let shape = gpu::derived_shape(&entries);
        let core = FieldCore::new(device, frame.slot_storage(), resources, targets, &shape);
        let mask_shape = glyph_field::FieldShape { label_prefix: "visible mask ", ..shape };
        let mask_core = FieldCore::new(device, frame.mask_slot_storage(), resources, targets, &mask_shape);
        Self { resident, frame, core, mask_core, chunk_counts: [0] }
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

    /// Set (or replace) the override of one glyph. One that changes nothing
    /// (colour 0, nudge 0, [`NO_GROUP`]) clears. A byte past the item is
    /// ignored with a warning; a byte that is not a surviving leader is
    /// kept but never applied (the kernel passes it over). The group's row
    /// in the Derived override table is allocated on first use and never
    /// freed; past 4,095 distinct groups the group part is dropped with a
    /// warning and the colour and nudge still apply. When the item's run
    /// has no slack the run is remapped to the table's tail; when the tail
    /// is full the edit is refused with a warning.
    pub fn set_glyph_override(&self, queue: &wgpu::Queue, ov: GlyphOverride) {
        self.resident.set_glyph_override(queue, ov);
    }

    /// Remove one glyph's override, if any.
    pub fn clear_glyph_override(&self, queue: &wgpu::Queue, item: u32, byte: u32) {
        self.resident.clear_glyph_override(queue, item, byte);
    }

    /// Colour the byte range `[start, end)` of an item: the new span replaces
    /// whatever spans overlapped it (clipping them at its edges), the rest
    /// stay. A line recolour or a highlight run is this. `color` 0 clears
    /// the range back to the default colour; `end` is clamped to the item
    /// and an empty range is a no-op. See [`merge_span_range`] for the
    /// exact rule; the upload is `set_item_spans`'s, with its remap and
    /// refusal.
    pub fn set_item_span_range(&self, queue: &wgpu::Queue, item: u32, start: u32, end: u32, color: u32) {
        self.resident.set_item_span_range(queue, item, start, end, color);
    }

    /// Prepare the selection mask for the glyphs of item `item` whose leader
    /// byte lies in `[start, end)`: the layout kernel again, over only the
    /// visible segments that intersect the range, into a second transient
    /// buffer (the selection is drawn with the mask pipeline the scene
    /// creates, no depth, no blend). Call after `prepare` in the same
    /// encoder, at most once per frame (a second call's slots would append
    /// to the first's count but its filter would replace it). An empty
    /// range, or no call at all, leaves the mask draw at zero instances —
    /// `prepare` resets it every frame. A selection over a line in the WASH
    /// tier, or one culled or dropped at a cap, has no segment entry and
    /// draws nothing; one past [`Self::mask_capacity`] slots is truncated.
    pub fn prepare_mask(&self, queue: &wgpu::Queue, encoder: &mut wgpu::CommandEncoder, item: u32, start: u32, end: u32) {
        self.frame.prepare_mask(&self.resident, queue, encoder, item, start, end);
    }

    /// Record the mask draw of what `prepare_mask` emitted (an indirect draw
    /// over the selection buffer); the caller set the mask pipeline.
    pub fn record_mask_draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        self.frame.record_mask_draw(&self.mask_core, pass);
    }

    /// Slots the selection mask buffer holds (the frame's slot cap or
    /// `tables::MASK_SLOTS_MAX`, whichever is lower).
    pub fn mask_capacity(&self) -> u32 {
        self.frame.mask_capacity
    }

    /// Where the glyph at (item, byte) landed in the LAST prepared frame's
    /// transient buffer, if it was laid out: reads the segment list back
    /// (blocking; diagnostics such as GLYPH_G_DUMP, never the frame path)
    /// and counts survivors from the covering segment's start. `None` when
    /// the byte is not a surviving leader (a continuation byte, a glyph-0
    /// cell such as a tab, a sequence trailer, the newline), or its line was
    /// not in the glyph tier this frame.
    pub fn locate(&self, queue: &wgpu::Queue, item: u32, byte: u32) -> Option<u32> {
        self.frame.locate(&self.resident, queue, item, byte)
    }

    /// The overrides of one item as the device has them (tests).
    pub fn item_overrides(&self, item: u32) -> Vec<GlyphOverrideGpu> {
        self.resident.item_overrides(item)
    }

    /// The Derived group-override table as the device has it (index →
    /// group; `[0]` unused), for turning a slot's lane back into its group.
    pub fn group_override_table(&self) -> Vec<u32> {
        self.resident.group_override_table()
    }

    /// The mask draw's instance count after the last `prepare_mask`
    /// (blocking; tests).
    pub fn read_mask_count(&self, queue: &wgpu::Queue) -> u32 {
        self.frame.read_mask_count(&self.resident, queue)
    }

    /// The first `count` mask slots (blocking; tests).
    pub fn read_mask_slots(&self, queue: &wgpu::Queue, count: u32) -> Vec<DerivedSlot> {
        self.frame.read_mask_slots(&self.resident, queue, count)
    }

    /// The first `count` wash entries of the last prepared frame — one box
    /// per WASH-tier line (`tables::counter::WASH` is how many) — read back
    /// BLOCKING (tests: the wash-vs-glyph extent witness).
    pub fn read_wash(&self, queue: &wgpu::Queue, count: u32) -> Vec<WashGpu> {
        self.frame.read_wash(&self.resident, queue, count)
    }

    /// The last COMPLETED frame's counters (see [`VisibleStats`]); they lag
    /// the frame being prepared by two.
    pub fn stats(&self) -> VisibleStats {
        self.frame.stats()
    }

    /// Record the wash tier's boxes (one per WASH-tier line: its x extent,
    /// its rows, its depth segments) into a pass whose pipeline the field
    /// sets itself; drawn after the glyph pass.
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
    gpu::layout_all_lines(device, queue, inputs, &[]).slots
}

/// [`layout_all_lines`] with per-glyph overrides applied first (the same
/// kernel the frame runs them through), returning the slots and the
/// group-override table their lanes index. The witness for the override
/// path without a frame.
pub fn layout_all_lines_with_overrides(device: &wgpu::Device, queue: &wgpu::Queue, inputs: &VisibleInputs<'_>, overrides: &[GlyphOverride]) -> HeadlessLayout {
    gpu::layout_all_lines(device, queue, inputs, overrides)
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
    /// The words of a TRANSIENT slot of the last prepared frame — the index
    /// [`VisibleField::locate`] names (GLYPH_G_DUMP); blocking.
    fn read_slot_words(&self, device: &wgpu::Device, queue: &wgpu::Queue, slot: u32, out: &mut [u32]) {
        if slot >= self.frame.limits.max_slots {
            out.fill(0);
            return;
        }
        self.core.read_slot_words(device, queue, slot, out);
    }
}
