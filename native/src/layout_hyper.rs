//! layout_hyper.rs — Ultra-fast parallel pure-Rust layout engine.
//!
//! Direct-to-arena streaming layout with Rayon parallelism:
//! - Zero SoA 52 B/byte intermediate heap allocations
//! - Zero intermediate GlyphRecord wire materialization
//! - Cache-resident register evaluation of positions, wrap, and pagination
//! - Parallel execution across files

use std::sync::Arc;
use rayon::prelude::*;

use crate::atlas::TrieTable;
use crate::fold::rows_for_line;
use crate::layout::{
    DerivedDeviceSlots, DeviceSlots, GlyphArena, ItemPlacement, LayoutError, LayoutGlyphs,
    LayoutItem,
};
use crate::text::fu_to_world;
use glyph_field::GlyphFieldMode;

mod types;
pub use types::{ChunkPrepass, ItemPrepass, Pass2DeviceOutput, SendPtr};

pub(crate) mod chunk;
pub(crate) mod char_resolve;
pub mod line_table;
pub use line_table::{LineEntry, LineTable, SegmentSeed, SEGMENT_BYTES};
use line_table::{cells_of, ChunkLine, ChunkLines, ChunkSeed, CutPlan};
use char_resolve::{resolve_byte_char_cluster, ResolveCtx};

mod device_alloc;
use device_alloc::{layout_device_discrete, layout_device_unified, DeviceEmission, EmitInputs};

mod pass2_device;
use pass2_device::{DerivedEmit, RenderEmit};
mod page;
mod pass2_host;
pub use pass2_host::{compute_single_item_placement, layout_pass2_host, scan_item_max_row_extent};
pub mod visible;
pub use visible::{VisibleItemSeed, VisibleStaging};

pub struct HyperLayout {
    trie: Option<Arc<TrieTable>>,
    device: Option<crate::gpu::SharedDevice>,
    /// Whose slot format the device path emits. Host (no-device) runs
    /// always produce the neutral 48 B records regardless.
    field_mode: GlyphFieldMode,
    pub(crate) prefetched_data: Option<PrefetchedHyperData>,
}

impl Default for HyperLayout {
    fn default() -> Self {
        Self::new()
    }
}

impl HyperLayout {
    pub fn new() -> Self {
        Self {
            trie: None,
            device: None,
            field_mode: GlyphFieldMode::Instanced,
            prefetched_data: None,
        }
    }

    pub(crate) fn with_device(device: crate::gpu::SharedDevice, field_mode: GlyphFieldMode) -> Self {
        Self {
            trie: None,
            device: Some(device),
            field_mode,
            prefetched_data: None,
        }
    }

    /// No device, but a mode: Visible needs none (it emits no slot), so a
    /// device-less load can still stage it; the other modes behave as
    /// [`Self::new`] (host records).
    pub(crate) fn with_field_mode(field_mode: GlyphFieldMode) -> Self {
        Self {
            trie: None,
            device: None,
            field_mode,
            prefetched_data: None,
        }
    }

    pub fn with_trie(trie: Arc<TrieTable>) -> Self {
        Self {
            trie: Some(trie),
            device: None,
            field_mode: GlyphFieldMode::Instanced,
            prefetched_data: None,
        }
    }

    pub fn set_prefetched(&mut self, data: PrefetchedHyperData) {
        self.prefetched_data = Some(data);
    }
}

impl LayoutGlyphs for HyperLayout {
    fn name(&self) -> &'static str {
        "hyper-rust"
    }

    fn layout_validated_items(
        &mut self,
        items: &[LayoutItem<'_>],
        arena: &mut GlyphArena,
    ) -> Result<Vec<ItemPlacement>, LayoutError> {
        self.layout_items_internal(items, arena, true)
    }
}

impl HyperLayout {
    fn layout_items_internal(
        &mut self,
        items: &[LayoutItem<'_>],
        arena: &mut GlyphArena,
        allow_device: bool,
    ) -> Result<Vec<ItemPlacement>, LayoutError> {
        let trie = match &self.trie {
            Some(t) => Arc::clone(t),
            None => {
                let arc = crate::default_trie();
                self.trie = Some(Arc::clone(&arc));
                arc
            }
        };

        let item_count = items.len();
        if item_count == 0 {
            return Ok(Vec::new());
        }

        let em_height_fu = trie.metrics.em_height_fu;
        let bitmap_adv = fu_to_world(trie.bitmap_advance_fu, em_height_fu);

        // The visible field wants Pass 1's line table and no Pass 2; the
        // prefetch collected one only if it was told the same mode.
        let wants_lines = allow_device && self.field_mode == GlyphFieldMode::Visible;

        // --- PASS 1 (Parallel): Prepass per chunk to find counts and max_row_extent ---
        let (chunks, item_chunk_ranges, mut agg) = if let Some(pre) = self.prefetched_data.take() {
            let chunks: Vec<chunk::LayoutChunk<'_>> = pre
                .chunk_defs
                .iter()
                .map(|def| chunk::LayoutChunk {
                    item_index: def.item_index,
                    bytes: &items[def.item_index].bytes[def.byte_offset..def.byte_offset + def.byte_len],
                    byte_offset: def.byte_offset,
                })
                .collect();
            (chunks, pre.item_chunk_ranges, pre.aggregate)
        } else {
            let (chunks, item_chunk_ranges) = chunk::slice_items_into_chunks(items);
            let sp_pass1 = tracing::info_span!("hyper.pass1").entered();
            let agg = if wants_lines {
                let item_bytes: Vec<&[u8]> = items.iter().map(|it| it.bytes).collect();
                let file_params: Vec<crate::layout::ItemParams> = items.iter().map(|it| it.params).collect();
                pass1_over_chunks_with_lines(
                    &chunks,
                    &item_chunk_ranges,
                    &item_bytes,
                    &file_params,
                    &trie,
                    bitmap_adv,
                    em_height_fu,
                    Some(SEGMENT_BYTES),
                )
            } else {
                pass1_prepass_chunks(
                    &chunks,
                    &item_chunk_ranges,
                    items,
                    &trie,
                    bitmap_adv,
                    em_height_fu,
                )
            };
            drop(sp_pass1);
            (chunks, item_chunk_ranges, agg)
        };
        // A prefetch run for another mode carries no table: Pass 1 once more,
        // with lines (the only cost of switching to Visible from the panel on
        // a load whose prefetch predates the choice).
        if wants_lines && agg.line_table.is_none() {
            let item_bytes: Vec<&[u8]> = items.iter().map(|it| it.bytes).collect();
            let file_params: Vec<crate::layout::ItemParams> = items.iter().map(|it| it.params).collect();
            log::info!("visible: the prefetched Pass 1 carried no line table — running Pass 1 again with lines");
            agg = pass1_over_chunks_with_lines(
                &chunks,
                &item_chunk_ranges,
                &item_bytes,
                &file_params,
                &trie,
                bitmap_adv,
                em_height_fu,
                Some(SEGMENT_BYTES),
            );
        }
        let line_table = agg.line_table.take();

        // The Derived lanes (which the Visible field reuses) are release-time
        // limits: past them a slot would pack a wrong lane and misplace
        // glyphs, so the load falls back to Instanced, once, loudly.
        let field_mode = if allow_device {
            let params: Vec<crate::layout::ItemParams> = items.iter().map(|it| it.params).collect();
            let (mode, note) = effective_field_mode(self.field_mode, &derived_lane_limits(&agg.prepasses, &params));
            if let Some(note) = note {
                log::warn!("{note}");
            }
            mode
        } else {
            self.field_mode
        };

        let prepasses = agg.prepasses;
        let chunk_slot_bases = agg.chunk_slot_bases;
        let chunk_base_rows = agg.chunk_base_rows;
        let chunk_record_bases = agg.chunk_record_bases;
        let chunk_initial_cols = agg.chunk_initial_cols;
        let chunk_initial_seg_advs = agg.chunk_initial_seg_advs;
        let chunk_initial_line_advs = agg.chunk_initial_line_advs;
        let total_survivors = agg.total_survivors;

        let mut slot_bases = Vec::with_capacity(item_count);
        for range in &item_chunk_ranges {
            slot_bases.push(chunk_slot_bases[range.start]);
        }

        let derived = field_mode == GlyphFieldMode::Derived;
        let can_use_device = allow_device && self.device.is_some() && total_survivors > 0;

        if field_mode == GlyphFieldMode::Visible && allow_device {
            // No Pass 2: the field lays the visible lines out per frame from
            // the bytes and the table. The placements come from the serial
            // single-item measure (held to the device Pass 2 by
            // `visible::tests`), in parallel over the items.
            let sp = tracing::info_span!("hyper.visible_placements").entered();
            let placements: Vec<ItemPlacement> = items
                .par_iter()
                .zip(prepasses.par_iter())
                .zip(slot_bases.par_iter())
                .map(|((item, pre), &slot_base)| {
                    compute_single_item_placement(
                        &item.params,
                        item.bytes,
                        slot_base,
                        pre.max_row_extent,
                        &trie,
                        bitmap_adv,
                        em_height_fu,
                    )
                    .0
                })
                .collect();
            drop(sp);
            let seeds = items
                .iter()
                .zip(prepasses.iter())
                .zip(placements.iter())
                .map(|((item, pre), placed)| VisibleItemSeed {
                    params: item.params,
                    group_id: item.group_id,
                    max_row_extent: pre.max_row_extent,
                    slot_count: placed.slot_count,
                    byte_len: u32::try_from(item.bytes.len()).expect("an item's bytes exceed u32"),
                })
                .collect();
            *arena = GlyphArena::from_visible(VisibleStaging {
                item_bytes: Vec::new(),
                line_table: line_table.expect("Pass 1 ran with lines for the visible field"),
                seeds,
                glyph_count: total_survivors as u64,
                items: Vec::new(),
            });
            return Ok(placements);
        }

        if can_use_device {
            let dev = self.device.as_ref().unwrap();
            let unified = dev.is_unified() && dev.host_visible_storage;

            // Derived: each item owns `row_count` consecutive line-table
            // entries; their bases are known now, so Pass 2 writes final
            let inputs = EmitInputs {
                items,
                chunks: &chunks,
                item_chunk_ranges: &item_chunk_ranges,
                prepasses: &prepasses,
                slot_bases: &slot_bases,
                chunk_slot_bases: &chunk_slot_bases,
                chunk_base_rows: &chunk_base_rows,
                chunk_record_bases: &chunk_record_bases,
                chunk_initial_cols: &chunk_initial_cols,
                chunk_initial_seg_advs: &chunk_initial_seg_advs,
                chunk_initial_line_advs: &chunk_initial_line_advs,
                trie: &trie,
                bitmap_adv,
                em_height_fu,
            };

            let (emission, derived_extras) = if derived {
                let label = "derived glyph slots (direct)";
                // The table depends only on Pass 1; build it alongside Pass 2.
                let emission = if unified {
                    layout_device_unified::<DerivedEmit>(dev, total_survivors, &inputs, label)
                } else {
                    layout_device_discrete::<DerivedEmit>(dev, total_survivors, &inputs, label)
                };
                let extras = DerivedDeviceSlots { mapped_base: emission.mapped_base };
                (emission, Some(extras))
            } else {
                let label = "glyph render slots (direct)";
                let emission = if unified {
                    layout_device_unified::<RenderEmit>(dev, total_survivors, &inputs, label)
                } else {
                    layout_device_discrete::<RenderEmit>(dev, total_survivors, &inputs, label)
                };
                (emission, None)
            };
            let DeviceEmission { chunks, chunk_slots, mapped_base, pass2: pass2_out, emoji_tint_pairs } = emission;

            let device_slots = DeviceSlots {
                chunks,
                chunk_slots,
                len: total_survivors,
                // RenderSlot-typed readers key off this; never hand them a
                // DerivedSlot mapping.
                mapped_slots: if derived { None } else { mapped_base },
                file_tints: pass2_out.file_tints,
                file_blocks: pass2_out.file_blocks,
                format: field_mode,
                derived: derived_extras,
                emoji_tint_pairs,
            };
            *arena = GlyphArena::from_device(device_slots);
            Ok(pass2_out.placements)
        } else {
            let (tail_ptr, capacity) = arena.uninit_tail(total_survivors);
            assert!(capacity >= total_survivors);
            let placements = layout_pass2_host(
                items,
                &prepasses,
                &slot_bases,
                &trie,
                bitmap_adv,
                em_height_fu,
                SendPtr(tail_ptr),
            );
            unsafe {
                arena.commit(total_survivors);
            }
            Ok(placements)
        }
    }
}

/// The Derived slot's lanes have fixed widths (`glyph_field_derived::derive`:
/// `item:20`, `row:24 | x_page:8`), asserted only in debug builds where they
/// are packed. This is the release-time decision, made ONCE per load from
/// Pass 1's aggregate, before any slot is emitted or any field built: the
/// item count against `ITEM_MAX`, each item's `row_count` against `ROW_MAX`,
/// and each column-paged item's widest column page (`max_line_cols /
/// page_cols`) against `X_PAGE_MAX`. The Visible field reuses the lanes, so
/// it is held to the same limits. `Err` names the first item and limit.
pub fn derived_lane_limits(prepasses: &[ItemPrepass], params: &[crate::layout::ItemParams]) -> Result<(), String> {
    use glyph_field_derived::{ITEM_MAX, ROW_MAX, X_PAGE_MAX};
    if prepasses.len() as u64 > u64::from(ITEM_MAX) + 1 {
        return Err(format!(
            "{} items, but the Derived item lane holds at most {} (ITEM_MAX {ITEM_MAX})",
            prepasses.len(),
            u64::from(ITEM_MAX) + 1
        ));
    }
    for (i, (pre, p)) in prepasses.iter().zip(params).enumerate() {
        // Rows are 0..row_count.
        if pre.row_count > 0 && pre.row_count - 1 > ROW_MAX {
            return Err(format!(
                "item {i}: {} rows, but the Derived row lane holds at most {} (ROW_MAX {ROW_MAX})",
                pre.row_count,
                u64::from(ROW_MAX) + 1
            ));
        }
        if p.has_page && p.page_cols > 0 {
            let pages = pre.max_line_cols / p.page_cols as u32;
            if pages > X_PAGE_MAX {
                return Err(format!(
                    "item {i}: a line of {} leaders reaches column page {pages} at page_cols {}, but the Derived lane holds at most {X_PAGE_MAX} (X_PAGE_MAX)",
                    pre.max_line_cols, p.page_cols
                ));
            }
        }
    }
    Ok(())
}

/// The field mode a load builds, given the one asked for and the lane
/// check: a Derived or Visible request past the lanes becomes Instanced,
/// with the warning to print. Instanced has no lanes to exceed.
pub fn effective_field_mode(requested: GlyphFieldMode, limits: &Result<(), String>) -> (GlyphFieldMode, Option<String>) {
    match (requested, limits) {
        (GlyphFieldMode::Instanced, _) | (_, Ok(())) => (requested, None),
        (mode, Err(why)) => (
            GlyphFieldMode::Instanced,
            Some(format!("field mode {mode} refused for this load — {why}; falling back to instanced")),
        ),
    }
}

/// Prepass aggregation output holding per-item prepasses and per-chunk starting offsets.
#[derive(Clone, Debug)]
pub struct PrepassAggregate {
    pub prepasses: Vec<ItemPrepass>,
    pub chunk_slot_bases: Vec<u32>,
    pub chunk_base_rows: Vec<i64>,
    pub chunk_record_bases: Vec<usize>,
    pub chunk_initial_cols: Vec<i64>,
    pub chunk_initial_seg_advs: Vec<f32>,
    pub chunk_initial_line_advs: Vec<f64>,
    pub total_survivors: usize,
    /// The line table (`line_table.rs`), when Pass 1 was asked for it: the
    /// visible-set field's resident index. `None` for every other mode.
    pub line_table: Option<LineTable>,
}

/// Precomputed chunks and Pass 1 metadata from background prefetch.
#[derive(Clone, Debug)]
pub struct PrefetchedHyperData {
    pub chunk_defs: Vec<chunk::ChunkDef>,
    pub item_chunk_ranges: Vec<std::ops::Range<usize>>,
    pub aggregate: PrepassAggregate,
}

/// The widest item-relative x a pure-ASCII line of `l` glyphs reaches, in the
/// sense of the fold's scalar 7 (`fold::layout_item`): the maximum over the
/// line's LEADERS of each one's x BEFORE its advance is added — so a glyph
/// counts at its left edge, and the newline (when `terminated`) at the x the
/// line closed on. Every ASCII glyph has `ascii_adv`, so with a fold unit the
/// x of column `c` is `seg_adv_table[c % fu]` (the table is the running f32
/// sum, the fold's own carrier); without one it is `c * ascii_adv` in f64,
/// which is exact. 0.0 for a line with no leader, the fold's starting value.
#[inline]
fn ascii_line_max_x(l: usize, terminated: bool, fu: usize, seg_adv_table: &[f32], ascii_adv: f32) -> f64 {
    // The last leader's column: the newline sits at `l`, the last glyph at
    // `l - 1`; the x is increasing in the column inside a fold unit.
    let last = if terminated {
        l
    } else if l > 0 {
        l - 1
    } else {
        return 0.0;
    };
    if fu > 0 {
        // A line that reaches the end of a fold unit has seen its widest
        // column, fu - 1; a terminator at an exact multiple sits at 0.
        seg_adv_table[last.min(fu - 1)] as f64
    } else {
        last as f64 * ascii_adv as f64
    }
}

/// Evaluates Pass 1 on a single byte slice (whole file or chunk).
///
/// `continues_line`: the chunk starts mid-line (an intra-line cut, see
/// `chunk.rs`). Its first line's x positions depend on the column and
/// segment advance it inherits, which only the aggregation knows, so that
/// portion contributes nothing to `max_row_extent` here; the aggregation
/// measures it with the true seed where the stride can reach an output
/// (`measure_continued_lines`).
///
/// `lines`: when `Some`, the chunk's lines and long-line cuts are collected
/// for the line table (`line_table.rs`), cuts planned every `segment_bytes`;
/// the layout counts are the same either way.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pass1_prepass_chunk_bytes(
    bytes: &[u8],
    p: &crate::layout::ItemParams,
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
    global_seg_adv_table: &[f32],
    continues_line: bool,
    lines: Option<&mut ChunkLines>,
    segment_bytes: usize,
) -> ChunkPrepass {
    // Two monomorphizations, so a load that wants no line table runs the
    // walk with none of the collection in it — not even a predictable
    // branch per byte (measured 2026-10-10: a runtime flag cost ~0.5 ms of
    // a 4 ms Pass 1 on the 93 MB tree).
    match lines {
        Some(out) => pass1_prepass_chunk_walk::<true>(bytes, p, trie, bitmap_adv, em_height_fu, global_seg_adv_table, continues_line, out, segment_bytes),
        None => {
            let mut none = ChunkLines::default();
            pass1_prepass_chunk_walk::<false>(bytes, p, trie, bitmap_adv, em_height_fu, global_seg_adv_table, continues_line, &mut none, segment_bytes)
        }
    }
}

// `inline(never)`: with both instantiations inlined into the dispatching
// wrapper, LLVM kept COLLECT as a runtime branch inside one 14 KB function
// instead of two specialised walks (the M2 measured it: +0.7 ms of an 11 ms
// Pass 1, 2026-10-10). As separate functions the `false` walk is the old one.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn pass1_prepass_chunk_walk<const COLLECT: bool>(
    bytes: &[u8],
    p: &crate::layout::ItemParams,
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
    global_seg_adv_table: &[f32],
    continues_line: bool,
    out: &mut ChunkLines,
    segment_bytes: usize,
) -> ChunkPrepass {
    let fold_unit = if p.wrap_width > 0 {
        p.wrap_width as i64
    } else if p.has_page {
        p.page_cols as i64
    } else {
        0
    };

    let mut survivor_count = 0u32;
    let mut leader_count = 0u32;
    let mut max_row_extent = 0.0f64;
    let mut trailer_until = 0usize;
    let mut has_cluster = false;
    let mut has_emoji = false;
    let cluster = char_resolve::clusters(p);
    let rctx = ResolveCtx { trie, bitmap_adv, em_height_fu, cluster };
    let wrap_w = p.wrap_width as i64;
    let ascii_adv = fu_to_world(1229, em_height_fu);

    let fu = fold_unit as usize;
    let seg_adv_table: &[f32] = if fu > 0 && fu < global_seg_adv_table.len() {
        &global_seg_adv_table[..=fu]
    } else {
        &[]
    };

    let has_newline = memchr::memchr(b'\n', bytes).is_some();

    if !has_newline {
        let mut col = 0i64;
        let mut line_adv = 0.0f64;
        let mut seg_adv = 0.0f32;
        let mut cells = 0u32;
        // The line's widest fold unit so far and the cells in the current
        // one (`LineEntry::width_cells`, C28; COLLECT only).
        let mut unit_cells = 0u32;
        let mut width_cells = 0u32;
        let mut plan = CutPlan::new(segment_bytes);

        let is_pure_ascii = crate::text::is_pure_printable_ascii(bytes);
        if is_pure_ascii {
            let l = bytes.len();
            survivor_count = l as u32;
            leader_count = l as u32;
            col = l as i64;
            line_adv = l as f64 * ascii_adv as f64;
            if fold_unit > 0 {
                seg_adv = seg_adv_table[l % fu];
            }
            if COLLECT {
                // Every byte is one cell: a full unit, or the whole line.
                width_cells = if fold_unit > 0 { l.min(fu) as u32 } else { l as u32 };
            }
            // No newline: the widest x is the last glyph's own (see
            // `ascii_line_max_x`).
            let widest = ascii_line_max_x(l, false, fu, seg_adv_table, ascii_adv);
            if widest > max_row_extent {
                max_row_extent = widest;
            }
            if COLLECT {
                // Every byte is a cell: the cuts land on the targets.
                let mut off = segment_bytes;
                while off < l {
                    out.seeds.push(ChunkSeed {
                        line: 0,
                        byte_offset: off as u32,
                        col: off as u32,
                        seg_adv: if fold_unit > 0 { seg_adv_table[off % fu] } else { 0.0 },
                        cells: off as u32,
                    });
                    off += segment_bytes;
                }
            }
        } else {
            for i in 0..bytes.len() {
                if COLLECT && plan.cut_here(i, bytes[i]) {
                    out.seeds.push(ChunkSeed { line: 0, byte_offset: i as u32, col: col as u32, seg_adv, cells });
                }
                let r = match resolve_byte_char_cluster(bytes, i, rctx, &mut trailer_until, &mut has_cluster) {
                    Some(r) => r,
                    None => continue,
                };
                has_emoji |= trie.is_emoji_glyph(r.glyph_id);
                leader_count += 1;
                if r.glyph_id != 0 {
                    survivor_count += 1;
                }
                // The fold's scalar 7: this leader's own x, BEFORE its
                // advance is added.
                let x = if fold_unit > 0 { seg_adv as f64 } else { line_adv };
                if x > max_row_extent {
                    max_row_extent = x;
                }
                col += 1;
                line_adv += r.advance as f64;
                if COLLECT {
                    let k = cells_of(r.advance, ascii_adv);
                    cells += k;
                    unit_cells += k;
                    if fold_unit > 0 && col % fold_unit == 0 {
                        width_cells = width_cells.max(unit_cells);
                        unit_cells = 0;
                    }
                }
                if fold_unit > 0 {
                    if col % fold_unit == 0 {
                        seg_adv = 0.0;
                    } else {
                        seg_adv += r.advance;
                    }
                }
            }
        }
        // An empty item is one empty chunk, and has no line.
        if COLLECT && !bytes.is_empty() {
            out.lines.push(ChunkLine { start: 0, glyphs: survivor_count, col: col as u32, width_cells: width_cells.max(unit_cells), terminated: false });
        }

        return ChunkPrepass {
            survivor_count,
            leader_count,
            // A chunk that continues a line is that line's continuation
            // throughout: none of its x is known here.
            max_row_extent: if continues_line { 0.0 } else { max_row_extent },
            has_cluster,
            has_emoji,
            completed_rows: 0,
            has_newline: false,
            delta_col: col,
            delta_line_adv: line_adv,
            delta_seg_adv: seg_adv,
            first_seg_col: 0,
            last_seg_col: 0,
            last_seg_line_adv: 0.0,
            last_seg_seg_adv: 0.0,
            max_line_cols: col as u32,
        };
    }

    let mut pos = 0usize;
    let mut line_index = 0usize;
    let mut completed_rows = 0u32;
    let mut first_seg_col = 0i64;
    let mut last_seg_col = 0i64;
    let mut last_seg_line_adv = 0.0f64;
    let mut last_seg_seg_adv = 0.0f32;
    let mut max_line_cols = 0u32;

    while pos < bytes.len() {
        let nl_pos = match memchr::memchr(b'\n', &bytes[pos..]) {
            Some(offset) => pos + offset,
            None => bytes.len(),
        };
        let line = &bytes[pos..nl_pos];
        let mut line_survivors = 0u32;
        let mut line_leaders = 0u32;
        let mut line_col = 0i64;
        let mut line_adv = 0.0f64;
        let mut seg_adv = 0.0f32;
        let mut cells = 0u32;
        // The line's widest fold unit so far and the cells in the current
        // one (`LineEntry::width_cells`, C28; COLLECT only).
        let mut unit_cells = 0u32;
        let mut width_cells = 0u32;
        let mut plan = CutPlan::new(segment_bytes);
        // The first line of a continuing chunk is measured with its true
        // seed by the aggregation, not here (see the doc above).
        let widest_before_line = max_row_extent;

        let is_pure_ascii = crate::text::is_pure_printable_ascii(line);
        if is_pure_ascii {
            let l = line.len();
            line_survivors = l as u32;
            line_leaders = l as u32;
            line_col = l as i64;
            line_adv = l as f64 * ascii_adv as f64;
            if fold_unit > 0 {
                seg_adv = seg_adv_table[l % fu];
            }
            if COLLECT {
                width_cells = if fold_unit > 0 { l.min(fu) as u32 } else { l as u32 };
            }
            let widest = ascii_line_max_x(l, nl_pos < bytes.len(), fu, seg_adv_table, ascii_adv);
            if widest > max_row_extent {
                max_row_extent = widest;
            }
            if COLLECT {
                let mut off = segment_bytes;
                while off < l {
                    out.seeds.push(ChunkSeed {
                        line: line_index as u32,
                        byte_offset: (pos + off) as u32,
                        col: off as u32,
                        seg_adv: if fold_unit > 0 { seg_adv_table[off % fu] } else { 0.0 },
                        cells: off as u32,
                    });
                    off += segment_bytes;
                }
            }
        } else {
            for i in pos..nl_pos {
                if COLLECT && plan.cut_here(i - pos, bytes[i]) {
                    out.seeds.push(ChunkSeed {
                        line: line_index as u32,
                        byte_offset: i as u32,
                        col: line_col as u32,
                        seg_adv,
                        cells,
                    });
                }
                let r = match resolve_byte_char_cluster(bytes, i, rctx, &mut trailer_until, &mut has_cluster) {
                    Some(r) => r,
                    None => continue,
                };
                has_emoji |= trie.is_emoji_glyph(r.glyph_id);
                line_leaders += 1;
                if r.glyph_id != 0 {
                    line_survivors += 1;
                }
                let x = if fold_unit > 0 { seg_adv as f64 } else { line_adv };
                if x > max_row_extent {
                    max_row_extent = x;
                }
                line_col += 1;
                line_adv += r.advance as f64;
                if COLLECT {
                    let k = cells_of(r.advance, ascii_adv);
                    cells += k;
                    unit_cells += k;
                    if fold_unit > 0 && line_col % fold_unit == 0 {
                        width_cells = width_cells.max(unit_cells);
                        unit_cells = 0;
                    }
                }
                if fold_unit > 0 {
                    if line_col % fold_unit == 0 {
                        seg_adv = 0.0;
                    } else {
                        seg_adv += r.advance;
                    }
                }
            }
            if nl_pos < bytes.len() {
                // The newline is a leader too, at the x the line closed on.
                let x = if fold_unit > 0 { seg_adv as f64 } else { line_adv };
                if x > max_row_extent {
                    max_row_extent = x;
                }
            }
        }

        survivor_count += line_survivors;
        if continues_line && line_index == 0 {
            max_row_extent = widest_before_line;
        }
        // Once per line, outside the per-byte loop (the lane-limit check).
        if line_col as u32 > max_line_cols {
            max_line_cols = line_col as u32;
        }
        if COLLECT {
            out.lines.push(ChunkLine {
                start: pos as u32,
                glyphs: line_survivors,
                col: line_col as u32,
                width_cells: width_cells.max(unit_cells),
                terminated: nl_pos < bytes.len(),
            });
        }

        if nl_pos < bytes.len() {
            line_leaders += 1;
            leader_count += line_leaders;

            if line_index == 0 {
                first_seg_col = line_col;
            } else {
                completed_rows += rows_for_line(line_col, wrap_w, p.wrap_mode) as u32;
            }
            pos = nl_pos + 1;
            line_index += 1;
        } else {
            leader_count += line_leaders;
            last_seg_col = line_col;
            last_seg_line_adv = line_adv;
            last_seg_seg_adv = seg_adv;
            break;
        }
    }

    ChunkPrepass {
        survivor_count,
        leader_count,
        max_row_extent,
        has_cluster,
        has_emoji,
        completed_rows,
        has_newline: true,
        delta_col: 0,
        delta_line_adv: 0.0,
        delta_seg_adv: 0.0,
        first_seg_col,
        last_seg_col,
        last_seg_line_adv,
        last_seg_seg_adv,
        max_line_cols,
    }
}

/// Evaluates Pass 1 across all chunks in parallel and aggregates per-item results.
pub(crate) fn pass1_prepass_chunks(
    chunks: &[chunk::LayoutChunk<'_>],
    item_chunk_ranges: &[std::ops::Range<usize>],
    items: &[LayoutItem<'_>],
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
) -> PrepassAggregate {
    let item_bytes: Vec<&[u8]> = items.iter().map(|it| it.bytes).collect();
    let file_params: Vec<crate::layout::ItemParams> = items.iter().map(|it| it.params).collect();
    pass1_over_chunks(chunks, item_chunk_ranges, &item_bytes, &file_params, trie, bitmap_adv, em_height_fu)
}

/// [`pass1_over_chunks`] without a line table.
#[allow(clippy::too_many_arguments)]
fn pass1_over_chunks(
    chunks: &[chunk::LayoutChunk<'_>],
    item_chunk_ranges: &[std::ops::Range<usize>],
    item_bytes: &[&[u8]],
    file_params: &[crate::layout::ItemParams],
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
) -> PrepassAggregate {
    pass1_over_chunks_with_lines(chunks, item_chunk_ranges, item_bytes, file_params, trie, bitmap_adv, em_height_fu, None)
}

/// Whether a chunk starts mid-line: an intra-line cut (`chunk.rs`).
#[inline]
fn chunk_continues_line(item_bytes: &[u8], byte_offset: usize) -> bool {
    byte_offset > 0 && item_bytes[byte_offset - 1] != b'\n'
}

/// Pass 1 proper, shared by the inline path ([`pass1_prepass_chunks`]) and
/// the background prefetch ([`prefetch_hyper`]): the per-chunk walk in
/// parallel, the serial aggregation, then the continued lines measured.
/// With `line_segment_bytes` the walk also collects the line table
/// (`line_table.rs`), cuts planned at that spacing.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pass1_over_chunks_with_lines(
    chunks: &[chunk::LayoutChunk<'_>],
    item_chunk_ranges: &[std::ops::Range<usize>],
    item_bytes: &[&[u8]],
    file_params: &[crate::layout::ItemParams],
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
    line_segment_bytes: Option<usize>,
) -> PrepassAggregate {
    let max_fold_unit = file_params
        .iter()
        .map(|p| {
            if p.wrap_width > 0 {
                p.wrap_width as usize
            } else if p.has_page {
                p.page_cols as usize
            } else {
                0
            }
        })
        .max()
        .unwrap_or(0);

    let ascii_adv = fu_to_world(1229, em_height_fu);
    let mut global_seg_adv_table = Vec::with_capacity(max_fold_unit + 1);
    if max_fold_unit > 0 {
        let mut cur = 0.0f32;
        for _ in 0..=max_fold_unit {
            global_seg_adv_table.push(cur);
            cur += ascii_adv;
        }
    }

    let segment_bytes = line_segment_bytes.unwrap_or(SEGMENT_BYTES).max(1);
    // Two collects, not one with a tuple: a load that wants no table keeps
    // the indexed single-allocation collect it always had (an `unzip` over
    // the parallel iterator folds through a list and cost the M2 a measurable
    // slice of an 11 ms Pass 1, 2026-10-10).
    let (chunk_prepasses, chunk_lines): (Vec<ChunkPrepass>, Vec<ChunkLines>) = if line_segment_bytes.is_none() {
        let cps: Vec<ChunkPrepass> = chunks
            .par_iter()
            .map(|chunk| {
                pass1_prepass_chunk_bytes(
                    chunk.bytes,
                    &file_params[chunk.item_index],
                    trie,
                    bitmap_adv,
                    em_height_fu,
                    &global_seg_adv_table,
                    chunk_continues_line(item_bytes[chunk.item_index], chunk.byte_offset),
                    None,
                    segment_bytes,
                )
            })
            .collect();
        (cps, Vec::new())
    } else {
        let both: Vec<(ChunkPrepass, ChunkLines)> = chunks
            .par_iter()
            .map(|chunk| {
                let mut lines = ChunkLines::default();
                let cp = pass1_prepass_chunk_bytes(
                    chunk.bytes,
                    &file_params[chunk.item_index],
                    trie,
                    bitmap_adv,
                    em_height_fu,
                    &global_seg_adv_table,
                    chunk_continues_line(item_bytes[chunk.item_index], chunk.byte_offset),
                    Some(&mut lines),
                    segment_bytes,
                );
                (cp, lines)
            })
            .collect();
        both.into_iter().unzip()
    };

    let mut agg = aggregate_chunk_prepasses(
        &chunk_prepasses,
        item_chunk_ranges,
        chunks,
        file_params,
        trie,
        bitmap_adv,
        em_height_fu,
    );
    measure_continued_lines(&mut agg, chunks, item_bytes, file_params, trie, bitmap_adv, em_height_fu);
    if line_segment_bytes.is_some() {
        let chunk_continues: Vec<bool> = chunks
            .iter()
            .map(|c| chunk_continues_line(item_bytes[c.item_index], c.byte_offset))
            .collect();
        let mut table = line_table::assemble(
            &chunk_lines,
            item_chunk_ranges,
            chunks,
            &chunk_continues,
            &agg.chunk_initial_cols,
            &agg.chunk_initial_seg_advs,
            &agg.chunk_initial_line_advs,
            file_params,
            ascii_adv,
            segment_bytes,
        );
        line_table::seed_continued_lines(
            &mut table,
            chunks,
            &chunk_continues,
            &agg.chunk_initial_cols,
            &agg.chunk_initial_seg_advs,
            &agg.chunk_initial_line_advs,
            file_params,
            trie,
            bitmap_adv,
            em_height_fu,
        );
        line_table::measure_continued_line_widths(
            &mut table,
            chunks,
            &chunk_continues,
            &agg.chunk_initial_cols,
            &agg.chunk_initial_seg_advs,
            &agg.chunk_initial_line_advs,
            file_params,
            trie,
            bitmap_adv,
            em_height_fu,
        );
        agg.line_table = Some(table);
    }
    agg
}

/// The segment advance a line has reached at the END of a chunk that holds
/// no newline — the seed of the chunk after it — computed as the fold
/// computes it: the running f32 sum of the advances since the segment's last
/// reset, in order. (Until 2026-10-09 this was `(col % fold_unit) * ascii_adv`,
/// a product that is neither that sum nor aware of non-ASCII advances: an ulp
/// of x for the next chunk's first partial segment, C15.)
///
/// The chunk spans columns `c0 .. c_end` of its line and enters with
/// `seg_in`. If a fold-unit boundary falls inside it, the sum restarts from 0
/// over its last `c_end % fold_unit` leaders; otherwise it continues from
/// `seg_in` over all of them. Either way only a TAIL of the chunk is walked:
/// it is found by counting leader bytes back from the end (a leader is any
/// byte that is not a continuation byte or an invalid lead — one per
/// record, trailers included), and resolved forward from the nearest ASCII
/// byte at or before it (or the chunk's start). No sequence or trailer span
/// crosses an ASCII byte (`chunk.rs`), so resolution from there agrees with
/// the walk of the whole chunk. Typical cost: one fold unit of bytes.
fn continued_segment_advance(
    bytes: &[u8],
    c0: i64,
    c_end: i64,
    fold_unit: i64,
    seg_in: f32,
    rctx: ResolveCtx<'_>,
) -> f32 {
    if c_end % fold_unit == 0 {
        return 0.0;
    }
    let last_reset = c_end - c_end % fold_unit;
    let (tail, mut seg) = if last_reset > c0 { (c_end - last_reset, 0.0f32) } else { (c_end - c0, seg_in) };
    if tail == 0 {
        // A chunk with no leader carries its seed through unchanged. Not
        // reachable from the cut rule (a chunk after a cut starts at an
        // ASCII leader), but the walk below must never index past the end.
        return seg;
    }
    let mut tail_start = bytes.len();
    let mut found = 0i64;
    while found < tail {
        tail_start -= 1;
        if char_resolve::is_leader_byte(bytes[tail_start]) {
            found += 1;
        }
    }
    let mut from = tail_start;
    while from > 0 && bytes[from] >= 0x80 {
        from -= 1;
    }
    let mut trailer_until = 0usize;
    for pos in from..bytes.len() {
        if let Some(r) =
            char_resolve::resolve_byte_char(bytes, pos, rctx, &mut trailer_until)
        {
            if pos >= tail_start {
                seg += r.advance;
            }
        }
    }
    seg
}

pub(crate) fn aggregate_chunk_prepasses(
    chunk_prepasses: &[ChunkPrepass],
    item_chunk_ranges: &[std::ops::Range<usize>],
    chunks: &[chunk::LayoutChunk<'_>],
    file_params: &[crate::layout::ItemParams],
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
) -> PrepassAggregate {
    let total_chunks = chunks.len();
    let mut prepasses = Vec::with_capacity(file_params.len());
    let mut chunk_slot_bases = Vec::with_capacity(total_chunks);
    let mut chunk_base_rows = Vec::with_capacity(total_chunks);
    let mut chunk_record_bases = Vec::with_capacity(total_chunks);
    let mut chunk_initial_cols = Vec::with_capacity(total_chunks);
    let mut chunk_initial_seg_advs = Vec::with_capacity(total_chunks);
    let mut chunk_initial_line_advs = Vec::with_capacity(total_chunks);
    let mut total_survivors = 0usize;

    for (item_idx, range) in item_chunk_ranges.iter().enumerate() {
        let p = &file_params[item_idx];
        let wrap_w = p.wrap_width as i64;
        let fold_unit = if p.wrap_width > 0 {
            p.wrap_width as i64
        } else if p.has_page {
            p.page_cols as i64
        } else {
            0
        };

        let mut item_survivor_count = 0u32;
        let mut item_leader_count = 0u32;
        let mut item_max_row_extent = 0.0f64;
        let mut item_has_cluster = false;
        let mut item_has_emoji = false;
        let mut item_max_line_cols = 0u32;

        let mut cur_base_row = 0i64;
        let mut cur_record_base = 0usize;
        let mut cur_col = 0i64;
        let mut cur_line_adv = 0.0f64;
        let mut cur_seg_adv = 0.0f32;

        for chunk_index in range.clone() {
            let cp = &chunk_prepasses[chunk_index];

            chunk_slot_bases.push(total_survivors as u32);
            chunk_base_rows.push(cur_base_row);
            chunk_record_bases.push(cur_record_base);
            chunk_initial_cols.push(cur_col);
            chunk_initial_seg_advs.push(cur_seg_adv);
            chunk_initial_line_advs.push(cur_line_adv);

            total_survivors += cp.survivor_count as usize;
            cur_record_base += cp.leader_count as usize;

            item_survivor_count += cp.survivor_count;
            item_leader_count += cp.leader_count;
            if cp.max_row_extent > item_max_row_extent {
                item_max_row_extent = cp.max_row_extent;
            }
            if cp.has_cluster {
                item_has_cluster = true;
            }
            item_has_emoji |= cp.has_emoji;
            // The widest line: the chunk's own lines, and the line it
            // continues at its true column (the inherited prefix added).
            item_max_line_cols = item_max_line_cols.max(cp.max_line_cols);
            let continued_total = if cp.has_newline { cur_col + cp.first_seg_col } else { cur_col + cp.delta_col };
            item_max_line_cols = item_max_line_cols.max(u32::try_from(continued_total).unwrap_or(u32::MAX));

            if !cp.has_newline {
                let c0 = cur_col;
                cur_col += cp.delta_col;
                // f64 sums of f32 advances are exact at any realistic line
                // length, so the chunk's own sum added to the prefix is the
                // fold's serial sum.
                cur_line_adv += cp.delta_line_adv;
                if fold_unit > 0 {
                    // Only a later chunk of this item reads the seed.
                    if chunk_index + 1 < range.end {
                        cur_seg_adv = continued_segment_advance(
                            chunks[chunk_index].bytes,
                            c0,
                            cur_col,
                            fold_unit,
                            cur_seg_adv,
                            ResolveCtx { trie, bitmap_adv, em_height_fu, cluster: char_resolve::clusters(p) },
                        );
                    }
                } else {
                    cur_seg_adv += cp.delta_seg_adv;
                }
            } else {
                let first_line_total = cur_col + cp.first_seg_col;
                cur_base_row += rows_for_line(first_line_total, wrap_w, p.wrap_mode);
                cur_base_row += cp.completed_rows as i64;

                cur_col = cp.last_seg_col;
                cur_line_adv = cp.last_seg_line_adv;
                cur_seg_adv = cp.last_seg_seg_adv;
            }
        }

        if cur_col > 0 {
            cur_base_row += rows_for_line(cur_col, wrap_w, p.wrap_mode);
        }

        prepasses.push(ItemPrepass {
            survivor_count: item_survivor_count,
            leader_count: item_leader_count,
            max_row_extent: item_max_row_extent,
            row_count: u32::try_from(cur_base_row).expect("item row count exceeds u32"),
            has_cluster: item_has_cluster,
            has_emoji: item_has_emoji,
            max_line_cols: item_max_line_cols,
        });
    }

    PrepassAggregate {
        prepasses,
        chunk_slot_bases,
        chunk_base_rows,
        chunk_record_bases,
        chunk_initial_cols,
        chunk_initial_seg_advs,
        chunk_initial_line_advs,
        total_survivors,
        line_table: None,
    }
}

/// Whether an item's page stride can reach any output: only a row-paged item
/// fans pages across x, only by `(y_page % pages_wide) * stride`, and that is
/// zero for every record unless some screen row reaches past the first page
/// and there is more than one page column. When it cannot, every emitter adds
/// `0 * stride` (an exact zero, the stride being finite) whatever
/// `max_row_extent` holds.
fn stride_reaches_output(p: &crate::layout::ItemParams, row_count: u32) -> bool {
    p.has_page
        && p.page_rows > 0
        && p.pages_wide > 1
        && row_count as i64 - 1 - p.scroll_rows as i64 >= p.page_rows as i64
}

/// The widest x of the first line of every chunk that CONTINUES a line,
/// measured with the seed the aggregation gave it, folded into its item's
/// `max_row_extent` — the part of the fold's scalar 7 the per-chunk walk
/// cannot see. Only for items whose stride reaches an output
/// ([`stride_reaches_output`]), since that is `max_row_extent`'s only
/// consumer; elsewhere it stays the widest x of the lines that start inside
/// a chunk. In parallel over the chunks; nothing runs for a corpus with no
/// intra-line cut.
fn measure_continued_lines(
    agg: &mut PrepassAggregate,
    chunks: &[chunk::LayoutChunk<'_>],
    item_bytes: &[&[u8]],
    file_params: &[crate::layout::ItemParams],
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
) {
    let wanted: Vec<usize> = (0..chunks.len())
        .filter(|&ci| {
            let c = &chunks[ci];
            chunk_continues_line(item_bytes[c.item_index], c.byte_offset)
                && stride_reaches_output(&file_params[c.item_index], agg.prepasses[c.item_index].row_count)
        })
        .collect();
    if wanted.is_empty() {
        return;
    }
    let widest: Vec<(usize, f64)> = wanted
        .par_iter()
        .map(|&ci| {
            let c = &chunks[ci];
            let p = &file_params[c.item_index];
            let fold_unit = if p.wrap_width > 0 {
                p.wrap_width as i64
            } else if p.has_page {
                p.page_cols as i64
            } else {
                0
            };
            let cluster = char_resolve::clusters(p);
            let rctx = ResolveCtx { trie, bitmap_adv, em_height_fu, cluster };
            let mut col = agg.chunk_initial_cols[ci];
            let mut seg = agg.chunk_initial_seg_advs[ci];
            let mut line_adv = agg.chunk_initial_line_advs[ci];
            let mut widest = 0.0f64;
            let mut trailer_until = 0usize;
            for pos in 0..c.bytes.len() {
                let Some(r) = char_resolve::resolve_byte_char(
                    c.bytes,
                    pos,
                    rctx,
                    &mut trailer_until,
                ) else {
                    continue;
                };
                // The fold's scalar 7: each leader's x before its advance,
                // the newline that closes the line included.
                let x = if fold_unit > 0 { seg as f64 } else { line_adv };
                if x > widest {
                    widest = x;
                }
                if r.is_newline {
                    break;
                }
                col += 1;
                line_adv += r.advance as f64;
                if fold_unit > 0 {
                    if col % fold_unit == 0 {
                        seg = 0.0;
                    } else {
                        seg += r.advance;
                    }
                }
            }
            (c.item_index, widest)
        })
        .collect();
    for (item, w) in widest {
        if w > agg.prepasses[item].max_row_extent {
            agg.prepasses[item].max_row_extent = w;
        }
    }
}

/// Computes Pass 1 chunk prepasses ahead of time during repository prefetching.
/// For the Visible field the walk also collects the line table, so the
/// background Pass 1 is the whole of the load's layout work.
pub fn prefetch_hyper(
    files: &[crate::repo::RepoFile],
    file_params: &[crate::layout::ItemParams],
    field_mode: GlyphFieldMode,
) -> PrefetchedHyperData {
    let trie = crate::default_trie();
    let em_height_fu = trie.metrics.em_height_fu;
    let bitmap_adv = fu_to_world(trie.bitmap_advance_fu, em_height_fu);

    let byte_slices: Vec<&[u8]> = files.iter().map(|f| f.bytes.as_slice()).collect();
    let (chunk_defs, item_chunk_ranges) = chunk::slice_byte_buffers_into_chunk_defs(&byte_slices);
    let chunks: Vec<chunk::LayoutChunk<'_>> = chunk_defs
        .iter()
        .map(|def| chunk::LayoutChunk {
            item_index: def.item_index,
            bytes: &byte_slices[def.item_index][def.byte_offset..def.byte_offset + def.byte_len],
            byte_offset: def.byte_offset,
        })
        .collect();

    let aggregate = pass1_over_chunks_with_lines(
        &chunks,
        &item_chunk_ranges,
        &byte_slices,
        file_params,
        &trie,
        bitmap_adv,
        em_height_fu,
        (field_mode == GlyphFieldMode::Visible).then_some(SEGMENT_BYTES),
    );

    PrefetchedHyperData {
        chunk_defs,
        item_chunk_ranges,
        aggregate,
    }
}

/// The DEVICE Pass 2 (`pass2_device.rs`, what every GPU load runs) written
/// into host memory, for instruments (`hyper_oracle`).
///
/// Faithful by construction, not by imitation: the three production
/// destinations (unified Metal's mapped buffer, the discrete path's mapped
/// staging buffer, and the host staging allocation) all hand
/// `EmitInputs::run` a raw address of writable memory sized for the total
/// survivors, built from the same Pass 1 calls `layout_items_internal` makes.
/// This hands it a host `Vec`. What it does not reach is what happens to the
/// bytes afterwards (the staging copy into VRAM, chunking across buffers)
/// and the background prefetch of Pass 1 (`prefetch_hyper`, which
/// `repo-verify` holds to the inline Pass 1 used here).
fn device_pass2_on_host<E: pass2_device::SlotEmit>(
    items: &[LayoutItem<'_>],
    trie: &TrieTable,
) -> (Vec<E::Slot>, Vec<ItemPlacement>)
where
    E::Slot: bytemuck::Zeroable,
{
    let em_height_fu = trie.metrics.em_height_fu;
    let bitmap_adv = fu_to_world(trie.bitmap_advance_fu, em_height_fu);
    let (chunks, item_chunk_ranges) = chunk::slice_items_into_chunks(items);
    let agg = pass1_prepass_chunks(&chunks, &item_chunk_ranges, items, trie, bitmap_adv, em_height_fu);
    let slot_bases: Vec<u32> = item_chunk_ranges.iter().map(|r| agg.chunk_slot_bases[r.start]).collect();
    let mut slots = vec![<E::Slot as bytemuck::Zeroable>::zeroed(); agg.total_survivors.max(1)];
    let inputs = EmitInputs {
        items,
        chunks: &chunks,
        item_chunk_ranges: &item_chunk_ranges,
        prepasses: &agg.prepasses,
        slot_bases: &slot_bases,
        chunk_slot_bases: &agg.chunk_slot_bases,
        chunk_base_rows: &agg.chunk_base_rows,
        chunk_record_bases: &agg.chunk_record_bases,
        chunk_initial_cols: &agg.chunk_initial_cols,
        chunk_initial_seg_advs: &agg.chunk_initial_seg_advs,
        chunk_initial_line_advs: &agg.chunk_initial_line_advs,
        trie,
        bitmap_adv,
        em_height_fu,
    };
    let (out, _pairs) = inputs.run::<E>(slots.as_mut_ptr() as usize);
    slots.truncate(agg.total_survivors);
    (slots, out.placements)
}

/// [`device_pass2_on_host`] in the Instanced field's format (32 B `RenderSlot`).
pub(crate) fn device_pass2_render_on_host(
    items: &[LayoutItem<'_>],
    trie: &TrieTable,
) -> (Vec<crate::glyph_scene::RenderSlot>, Vec<ItemPlacement>) {
    device_pass2_on_host::<RenderEmit>(items, trie)
}

/// [`device_pass2_on_host`] in the Derived field's format (20 B `DerivedSlot`).
pub(crate) fn device_pass2_derived_on_host(
    items: &[LayoutItem<'_>],
    trie: &TrieTable,
) -> (Vec<glyph_field_derived::DerivedSlot>, Vec<ItemPlacement>) {
    device_pass2_on_host::<DerivedEmit>(items, trie)
}

impl crate::layout::VerifyLayout for HyperLayout {
    fn layout_validated_items_recording(
        &mut self,
        items: &[LayoutItem<'_>],
        arena: &mut GlyphArena,
    ) -> Result<(Vec<ItemPlacement>, Vec<crate::layout::GlyphRecord>), LayoutError> {
        let placements = self.layout_items_internal(items, arena, false)?;
        let trie = match &self.trie {
            Some(t) => Arc::clone(t),
            None => {
                let arc = crate::default_trie();
                self.trie = Some(Arc::clone(&arc));
                arc
            }
        };
        let mut all_records = Vec::new();
        for item in items {
            all_records.extend(rederive_item_records(item.bytes, &item.params, &trie));
        }
        Ok((placements, all_records))
    }
}

mod rederive;
pub use rederive::{rederive_item_records, resolve_spans_to_slot_colors};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph_scene::RenderSlot;
    use crate::layout::{ItemParams, Paint};
    use glyph_field_derived::DerivedSlot;

    /// A host `WindowSink`: two window buffers used alternately (as the GPU
    /// sink uses two staging buffers), each flushed into one stream buffer at
    /// its window's offset — what the GPU copy does.
    struct HostWindowSink {
        bufs: [Vec<u8>; 2],
        stream: Vec<u8>,
        slot_bytes: usize,
        windows_seen: usize,
    }

    impl pass2_device::WindowSink for HostWindowSink {
        fn begin(&mut self, k: usize, w: &pass2_device::SlotWindow) -> usize {
            let buf = &mut self.bufs[k % 2];
            buf.clear();
            buf.resize((w.slots.len() * self.slot_bytes).max(1), 0xA5);
            buf.as_mut_ptr() as usize
        }
        fn end(&mut self, k: usize, w: &pass2_device::SlotWindow) {
            let n = w.slots.len() * self.slot_bytes;
            let at = w.slots.start * self.slot_bytes;
            self.stream[at..at + n].copy_from_slice(&self.bufs[k % 2][..n]);
            self.windows_seen += 1;
        }
    }

    fn windowed_matches_unwindowed<E: pass2_device::SlotEmit>(items: &[LayoutItem<'_>], window_slots: &[usize])
    where
        E::Slot: bytemuck::Pod,
    {
        let trie = crate::default_trie();
        let em_height_fu = trie.metrics.em_height_fu;
        let bitmap_adv = fu_to_world(trie.bitmap_advance_fu, em_height_fu);
        let (chunks, item_chunk_ranges) = chunk::slice_items_into_chunks(items);
        let agg = pass1_prepass_chunks(&chunks, &item_chunk_ranges, items, &trie, bitmap_adv, em_height_fu);
        let slot_bases: Vec<u32> = item_chunk_ranges.iter().map(|r| agg.chunk_slot_bases[r.start]).collect();
        let inputs = EmitInputs {
            items,
            chunks: &chunks,
            item_chunk_ranges: &item_chunk_ranges,
            prepasses: &agg.prepasses,
            slot_bases: &slot_bases,
            chunk_slot_bases: &agg.chunk_slot_bases,
            chunk_base_rows: &agg.chunk_base_rows,
            chunk_record_bases: &agg.chunk_record_bases,
            chunk_initial_cols: &agg.chunk_initial_cols,
            chunk_initial_seg_advs: &agg.chunk_initial_seg_advs,
            chunk_initial_line_advs: &agg.chunk_initial_line_advs,
            trie: &trie,
            bitmap_adv,
            em_height_fu,
        };
        let total = agg.total_survivors;
        let size = std::mem::size_of::<E::Slot>();
        let mut reference = vec![<E::Slot as bytemuck::Zeroable>::zeroed(); total.max(1)];
        let (ref_out, ref_pairs) = inputs.run::<E>(reference.as_mut_ptr() as usize);
        let ref_bytes: &[u8] = bytemuck::cast_slice(&reference[..total]);
        assert!(ref_pairs.iter().any(|p| !p.is_empty()), "the corpus must exercise the emoji scratch path");
        assert!(chunks.len() > items.len(), "the corpus must cut some item into several chunks");

        for &ws in window_slots {
            let windows = pass2_device::plan_windows(&agg.chunk_slot_bases, total, ws);
            let mut sink = HostWindowSink {
                bufs: [Vec::new(), Vec::new()],
                stream: vec![0x5A; total * size],
                slot_bytes: size,
                windows_seen: 0,
            };
            let (out, pairs) = pass2_device::emit_windows::<E>(&inputs, &windows, total, &mut sink);
            assert_eq!(sink.windows_seen, windows.len());
            assert!(sink.stream == ref_bytes, "window_slots {ws}: windowed slot bytes differ from the unwindowed emission");
            assert_eq!(out.placements, ref_out.placements, "window_slots {ws}: placements");
            assert_eq!(out.file_tints, ref_out.file_tints, "window_slots {ws}: tints");
            assert_eq!(pairs, ref_pairs, "window_slots {ws}: emoji tint pairs");
        }
    }

    /// C22: emitting the slot stream window by window into staging memory —
    /// with the emoji items' chunks detoured through host scratch for their
    /// tint pairs — lands exactly the bytes, placements, tints and pairs of
    /// the one-buffer emission, for one chunk per window, a few hundred
    /// slots per window, and one window. Host memory only, so it holds on
    /// every machine, unlike pixel-ab (which sees staging only on a discrete
    /// GPU).
    #[test]
    fn windowed_emission_matches_unwindowed() {
        use crate::fold::WrapMode;
        let read = |p: &str| std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(p)).expect(p);
        let emoji = read("fixtures/emoji-view.txt");
        let cluster = read("fixtures/g-cluster-repo/main.rs");
        let cut = read("fixtures/chunk-cut-paint.txt");
        let plain = b"fn main() {\n    let x = 1;\n}\n".to_vec();
        let params = ItemParams { line_height: 1.25, wrap_width: 80, wrap_mode: WrapMode::Back, z_step: 0.15, ..Default::default() };
        let bodies: Vec<(&[u8], Paint)> = vec![
            (&plain, Paint::Flat(0x1234_5678)),
            (&emoji, Paint::Flat(0x2233_4455)),
            (&cut, Paint::SyntaxHeuristic),
            (b"", Paint::Flat(0x0102_0304)),
            (&cluster, Paint::SyntaxHeuristic),
        ];
        let items: Vec<LayoutItem<'_>> = bodies
            .iter()
            .enumerate()
            .map(|(i, (bytes, paint))| LayoutItem { bytes, params, group_id: i as u32, paint: *paint })
            .collect();
        let sizes = [1, 700, usize::MAX];
        windowed_matches_unwindowed::<pass2_device::DerivedEmit>(&items, &sizes);
        windowed_matches_unwindowed::<pass2_device::RenderEmit>(&items, &sizes);
    }

    #[test]
    fn hyper_and_batched_agree_on_samples() {
        const SAMPLES: [&[u8]; 3] = [
            b"fn main() {\n    let x = 1;\n}\n",
            b"no trailing newline",
            b"   \t   \n\n   \n",
        ];
        let colors: Vec<Vec<u32>> =
            SAMPLES.iter().map(|b| crate::text::colorize_leaders(b)).collect();
        let items: Vec<LayoutItem<'_>> = SAMPLES
            .iter()
            .enumerate()
            .map(|(i, bytes)| LayoutItem {
                bytes,
                params: ItemParams { line_height: 1.25, ..Default::default() },
                group_id: i as u32,
                paint: Paint::PerRecord(&colors[i]),
            })
            .collect();

        let mut hyper = HyperLayout::new();
        let mut hyper_arena = GlyphArena::new();
        let hyper_places = hyper
            .layout_items(&items, &mut hyper_arena)
            .expect("hyper layout");

        assert_eq!(hyper_places.len(), 3);
        assert_eq!(hyper_places[0].record_count, 29);
        assert_eq!(hyper_places[0].slot_count, 26);
        assert_eq!(hyper_places[0].slot_base, 0);

        assert_eq!(hyper_places[1].record_count, 19);
        assert_eq!(hyper_places[1].slot_count, 19);
        assert_eq!(hyper_places[1].slot_base, 26);

        assert_eq!(hyper_places[2].record_count, 13);
        assert_eq!(hyper_places[2].slot_count, 9);
        assert_eq!(hyper_places[2].slot_base, 45);
    }

    #[test]
    fn hyper_byte_spans_painting() {
        use crate::layout::ByteSpan;
        let text = b"fn main() {\n    let x = 42;\n}\n";
        let spans = [
            ByteSpan { start: 0, end: 2, color: 0x1111_1111 },  // "fn"
            ByteSpan { start: 16, end: 19, color: 0x2222_2222 }, // "let"
            ByteSpan { start: 24, end: 26, color: 0x3333_3333 }, // "42"
        ];
        let item = LayoutItem {
            bytes: text,
            params: ItemParams { line_height: 1.25, ..Default::default() },
            group_id: 0,
            paint: Paint::ByteSpans(&spans),
        };
        let mut hyper = HyperLayout::new();
        let mut arena = GlyphArena::new();
        let places = hyper
            .layout_items(&[item], &mut arena)
            .expect("hyper layout");
        assert_eq!(places.len(), 1);
        let instances = arena.instances();
        // Instance 0 is 'f', 1 is 'n': color should be 0x1111_1111
        assert_eq!(instances[0].color, 0x1111_1111);
        assert_eq!(instances[1].color, 0x1111_1111);
        // ' ' is dropped/blank, 'm' is default color
        assert_eq!(instances[2].color, crate::layout::DEFAULT_COLOR_PACKED);
    }

    #[test]
    fn resolve_spans_and_inplace_update_slot_colors() {
        use crate::layout::ByteSpan;
        let text = b"fn main() {\n    let x = 42;\n}\n";
        let spans = [
            ByteSpan { start: 0, end: 2, color: 0x1111_1111 },  // "fn"
            ByteSpan { start: 16, end: 19, color: 0x2222_2222 }, // "let"
            ByteSpan { start: 24, end: 26, color: 0x3333_3333 }, // "42"
        ];
        let mut hyper = HyperLayout::new();
        let trie = crate::default_trie();

        // 1. Layout directly with Paint::ByteSpans
        let item_spanned = LayoutItem {
            bytes: text,
            params: ItemParams { line_height: 1.25, ..Default::default() },
            group_id: 0,
            paint: Paint::ByteSpans(&spans),
        };
        let mut arena_spanned = GlyphArena::new();
        let places_spanned = hyper
            .layout_items(&[item_spanned], &mut arena_spanned)
            .expect("layout spanned");

        // 2. Layout with Paint::Flat
        let item_flat = LayoutItem {
            bytes: text,
            params: ItemParams { line_height: 1.25, ..Default::default() },
            group_id: 0,
            paint: Paint::Flat(crate::layout::DEFAULT_COLOR_PACKED),
        };
        let mut arena_flat = GlyphArena::new();
        let places_flat = hyper
            .layout_items(&[item_flat], &mut arena_flat)
            .expect("layout flat");

        assert_eq!(places_flat[0].slot_count, places_spanned[0].slot_count);

        // 3. Resolve spans to slot colors
        let resolved = resolve_spans_to_slot_colors(text, &spans, &trie, crate::fold::ClusterMode::default());
        assert_eq!(resolved.len(), places_flat[0].slot_count as usize);

        // Verify resolved colors exactly match the layout-time Paint::ByteSpans colors
        let spanned_colors: Vec<u32> = arena_spanned.instances().iter().map(|s| s.color).collect();
        assert_eq!(resolved, spanned_colors);

        // 4. Update flat arena in-place
        let updated = arena_flat.update_slot_colors(places_flat[0].slot_base as usize, &resolved);
        assert_eq!(updated, resolved.len());

        // Verify arena_flat now has identical colors to arena_spanned
        let flat_updated_colors: Vec<u32> = arena_flat.instances().iter().map(|s| s.color).collect();
        assert_eq!(flat_updated_colors, spanned_colors);
    }

    /// The direct Derived emission (Pass 2 writing 20 B `DerivedSlot`s with
    /// final `line_idx` from Pass 1's row counts) must agree slot for slot
    /// with the host path it replaced: 48 B records transcoded afterwards.
    /// Covers unfolded, WrapBack and WrapDown folds, a wrap-heavy line, an
    /// empty item, and an emoji (non-ASCII) item.
    #[test]
    fn derived_direct_emission_matches_host_transcode() {
        use bytemuck::Zeroable;
        use crate::fold::WrapMode;
        use crate::layout::{ItemParams, Paint};
        let long_line: Vec<u8> = b"0123456789abcdefghij".repeat(40);
        let mut wrapped = b"short\n".to_vec();
        wrapped.extend_from_slice(&long_line);
        wrapped.extend_from_slice(b"\n\n  tail line\nno newline at end");
        let emoji = "smile \u{1F600} ok\nflag \u{1F1FA}\u{1F1F8} x\n".as_bytes().to_vec();
        let plain = b"fn main() {\n    let x = 1;\n}\n".to_vec();
        let bodies: Vec<(&[u8], ItemParams)> = vec![
            (&plain, ItemParams { line_height: 1.25, ..Default::default() }),
            (
                &wrapped,
                ItemParams {
                    line_height: 1.25,
                    wrap_width: 37,
                    wrap_mode: WrapMode::Back,
                    z_step: 0.15,
                    origin_z: 2.0,
                    ..Default::default()
                },
            ),
            (
                &wrapped,
                ItemParams {
                    line_height: 1.25,
                    wrap_width: 23,
                    wrap_mode: WrapMode::Down,
                    z_step: 0.15,
                    ..Default::default()
                },
            ),
            (b"", ItemParams { line_height: 1.25, ..Default::default() }),
            (&emoji, ItemParams { line_height: 1.25, wrap_width: 8, z_step: 0.1, ..Default::default() }),
        ];
        let items: Vec<LayoutItem<'_>> = bodies
            .iter()
            .enumerate()
            .map(|(i, (bytes, params))| LayoutItem {
                bytes,
                params: *params,
                group_id: i as u32,
                paint: Paint::Flat(0x1234_5678 + i as u32),
            })
            .collect();

        // Host path: neutral 48 B records.
        let mut hyper = HyperLayout::new();
        let mut arena = GlyphArena::new();
        let host_places = hyper.layout_items(&items, &mut arena).expect("host layout");
        let host = arena.instances();

        // Direct path: Pass 1 + DerivedEmit into a host Vec.
        let trie = crate::default_trie();
        let em_height_fu = trie.metrics.em_height_fu;
        let bitmap_adv = fu_to_world(trie.bitmap_advance_fu, em_height_fu);
        let (chunks, item_chunk_ranges) = chunk::slice_items_into_chunks(&items);
        let agg = pass1_prepass_chunks(&chunks, &item_chunk_ranges, &items, &trie, bitmap_adv, em_height_fu);
        let prepasses = agg.prepasses;
        let mut slot_bases = Vec::new();
        let mut slots_acc = 0u32;
        for p in &prepasses {
            slot_bases.push(slots_acc);
            slots_acc += p.survivor_count;
        }
        assert_eq!(slots_acc as usize, host.len());
        let mut direct = vec![DerivedSlot::zeroed(); host.len()];
        let inputs = EmitInputs {
            items: &items,
            chunks: &chunks,
            item_chunk_ranges: &item_chunk_ranges,
            prepasses: &prepasses,
            slot_bases: &slot_bases,
            chunk_slot_bases: &agg.chunk_slot_bases,
            chunk_base_rows: &agg.chunk_base_rows,
            chunk_record_bases: &agg.chunk_record_bases,
            chunk_initial_cols: &agg.chunk_initial_cols,
            chunk_initial_seg_advs: &agg.chunk_initial_seg_advs,
            chunk_initial_line_advs: &agg.chunk_initial_line_advs,
            trie: &trie,
            bitmap_adv,
            em_height_fu,
        };
        let (out, _pairs) = inputs.run::<DerivedEmit>(direct.as_mut_ptr() as usize);

        assert_eq!(out.placements.len(), host_places.len());
        for (a, b) in out.placements.iter().zip(&host_places) {
            assert_eq!((a.slot_base, a.slot_count), (b.slot_base, b.slot_count));
        }
        for (k, (d, h)) in direct.iter().zip(host).enumerate() {
            let p = &items[h.group_id as usize].params;
            let want_wrap = if p.z_step.abs() > 1e-6 {
                ((p.origin_z as f32 - h.pos[2]) / p.z_step as f32).round().max(0.0) as u32
            } else {
                0
            };
            assert_eq!(d.x.to_bits(), h.pos[0].to_bits(), "slot {k}: x");
            assert_eq!(d.glyph_and_wrap & 0xFFFF, h.glyph_id & 0xFFFF, "slot {k}: glyph");
            assert_eq!(d.glyph_and_wrap >> 16, want_wrap, "slot {k}: wrap segment");
            assert_eq!(d.color, h.color, "slot {k}: color");
            assert_eq!(d.group_id(), h.group_id, "slot {k}: group");
            assert_eq!(d.row, h.row, "slot {k}: row");
        }
    }

    #[test]
    fn block_cull_bounds_enclose_all_slots() {
        let mut sample = String::new();
        for line_index in 0..40 {
            let indent = if line_index % 3 == 0 { "                    " } else { "    " };
            let content = format!("let variable_{line_index} = compute_value_{line_index}(foo, bar, baz); // comment\n");
            sample.push_str(indent);
            sample.push_str(&content);
        }
        let bytes = sample.as_bytes();
        let items = [LayoutItem {
            bytes,
            params: ItemParams { line_height: 1.25, ..Default::default() },
            group_id: 0,
            paint: Paint::SyntaxHeuristic,
        }];

        let trie = crate::default_trie();
        let em_height_fu = trie.metrics.em_height_fu;
        let bitmap_adv = fu_to_world(trie.bitmap_advance_fu, em_height_fu);
        let (chunks, item_chunk_ranges) = chunk::slice_items_into_chunks(&items);
        let agg = pass1_prepass_chunks(&chunks, &item_chunk_ranges, &items, &trie, bitmap_adv, em_height_fu);
        let prepasses = agg.prepasses;
        assert!(prepasses[0].survivor_count > crate::glyph_scene::SUBSEG_BLOCK_SIZE as u32);

        let mut slot_bases = Vec::new();
        let mut slots_acc = 0u32;
        for p in &prepasses {
            slot_bases.push(slots_acc);
            slots_acc += p.survivor_count;
        }
        let mut direct: Vec<RenderSlot> = vec![bytemuck::Zeroable::zeroed(); slots_acc as usize];
        let inputs = EmitInputs {
            items: &items,
            chunks: &chunks,
            item_chunk_ranges: &item_chunk_ranges,
            prepasses: &prepasses,
            slot_bases: &slot_bases,
            chunk_slot_bases: &agg.chunk_slot_bases,
            chunk_base_rows: &agg.chunk_base_rows,
            chunk_record_bases: &agg.chunk_record_bases,
            chunk_initial_cols: &agg.chunk_initial_cols,
            chunk_initial_seg_advs: &agg.chunk_initial_seg_advs,
            chunk_initial_line_advs: &agg.chunk_initial_line_advs,
            trie: &trie,
            bitmap_adv,
            em_height_fu,
        };
        let (out, _pairs) = inputs.run::<RenderEmit>(direct.as_mut_ptr() as usize);
        assert!(!out.file_blocks.is_empty());
        let blocks = &out.file_blocks[0];
        assert!(blocks.len() >= 2, "must have multiple blocks for > 512 survivors, got {}", blocks.len());

        for (block_index, block) in blocks.iter().enumerate() {
            assert!(
                block.min[0] <= block.max[0],
                "block {block_index} has inverted X: min {} > max {}",
                block.min[0],
                block.max[0]
            );
            assert!(
                block.min[1] <= block.max[1],
                "block {block_index} has inverted Y: min {} > max {}",
                block.min[1],
                block.max[1]
            );
            assert!(
                block.min[2] <= block.max[2],
                "block {block_index} has inverted Z: min {} > max {}",
                block.min[2],
                block.max[2]
            );

            let start = block.slot_base as usize;
            let end = (block.slot_base + block.slot_count) as usize;
            for (slot_idx, slot) in direct[start..end].iter().enumerate() {
                assert!(
                    slot.pos[0] >= block.min[0] - 1e-4,
                    "block {block_index}, slot {slot_idx}: x {} < min_x {}",
                    slot.pos[0],
                    block.min[0]
                );
                assert!(
                    slot.pos[0] <= block.max[0] + 1e-4,
                    "block {block_index}, slot {slot_idx}: x {} > max_x {}",
                    slot.pos[0],
                    block.max[0]
                );
                let half_h = 0.5 * slot.height;
                assert!(
                    slot.pos[1] - half_h >= block.min[1] - 1e-4,
                    "block {block_index}, slot {slot_idx}: y_lo {} < min_y {}",
                    slot.pos[1] - half_h,
                    block.min[1]
                );
                assert!(
                    slot.pos[1] + half_h <= block.max[1] + 1e-4,
                    "block {block_index}, slot {slot_idx}: y_hi {} > max_y {}",
                    slot.pos[1] + half_h,
                    block.max[1]
                );
            }
        }
    }

    #[test]
    fn hyper_device_chunking_splits_and_binds_across_limits() {
        use glyph_field::{FieldResources, FieldTargets, GlyphField, SlotSource};
        use glyph_field_derived::DerivedField;

        let ctx = pollster::block_on(crate::gpu::init(None));
        let dev = crate::gpu::SharedDevice::from_ctx(&ctx);

        let text = b"line 1: hello world\nline 2: testing chunked emission\nline 3: across buffer boundaries\n";
        let item = LayoutItem {
            bytes: text,
            params: ItemParams { line_height: 1.25, ..Default::default() },
            group_id: 0,
            paint: Paint::Flat(0xFFFFFFFF),
        };

        let trie = crate::default_trie();
        let em_height_fu = trie.metrics.em_height_fu;
        let bitmap_adv = fu_to_world(trie.bitmap_advance_fu, em_height_fu);
        let items = [item];
        let (chunks, item_chunk_ranges) = chunk::slice_items_into_chunks(&items);
        let agg = pass1_prepass_chunks(&chunks, &item_chunk_ranges, &items, &trie, bitmap_adv, em_height_fu);
        let prepasses = agg.prepasses;
        let slot_bases = vec![0u32];
        let total_survivors = prepasses[0].survivor_count as usize;
        assert!(total_survivors > 20);

        let inputs = EmitInputs {
            items: &items,
            chunks: &chunks,
            item_chunk_ranges: &item_chunk_ranges,
            prepasses: &prepasses,
            slot_bases: &slot_bases,
            chunk_slot_bases: &agg.chunk_slot_bases,
            chunk_base_rows: &agg.chunk_base_rows,
            chunk_record_bases: &agg.chunk_record_bases,
            chunk_initial_cols: &agg.chunk_initial_cols,
            chunk_initial_seg_advs: &agg.chunk_initial_seg_advs,
            chunk_initial_line_advs: &agg.chunk_initial_line_advs,
            trie: &trie,
            bitmap_adv,
            em_height_fu,
        };

        // Force chunk_cap = 16 slots to test multi-chunk emission on real GPU buffers
        let chunk_cap = 16;
        let emission = device_alloc::layout_device_discrete_chunked::<pass2_device::DerivedEmit>(
            &dev,
            total_survivors,
            &inputs,
            "test derived chunked",
            chunk_cap,
        );

        let expected_chunks = total_survivors.div_ceil(chunk_cap);
        assert_eq!(emission.chunks.len(), expected_chunks);
        assert_eq!(emission.chunk_slots, chunk_cap);

        let total_chunk_slots: u32 = emission.chunks.iter().map(|c| c.slots).sum();
        assert_eq!(total_chunk_slots as usize, total_survivors);

        // Verify DerivedField builds bind groups for all chunks without validation error
        let dummy_advances = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dummy advances"),
            size: 256,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let dummy_frame_uniform = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dummy frame uniform"),
            size: 256,
            usage: wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: false,
        });
        let dummy_group_table = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dummy group table"),
            size: 256,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let dummy_glyph_map = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dummy glyph map"),
            size: wgpu::Extent3d { width: 1024, height: 161, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Uint,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let dummy_glyph_map_view = dummy_glyph_map.create_view(&wgpu::TextureViewDescriptor::default());

        let dummy_emoji_sheet = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dummy emoji sheet"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let dummy_emoji_sheet_view = dummy_emoji_sheet.create_view(&wgpu::TextureViewDescriptor::default());

        let dummy_emoji_sampler = ctx.device.create_sampler(&wgpu::SamplerDescriptor::default());

        let dummy_curves = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dummy curves"),
            size: wgpu::Extent3d { width: 1024, height: 161, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Uint,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let dummy_curves_view = dummy_curves.create_view(&wgpu::TextureViewDescriptor::default());

        let dummy_params = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dummy params"),
            size: 256,
            usage: wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: false,
        });

        let resources = FieldResources {
            frame_uniform: &dummy_frame_uniform,
            group_table: &dummy_group_table,
            glyph_map: &dummy_glyph_map_view,
            curves: &dummy_curves_view,
            params: &dummy_params,
            emoji_sheet: &dummy_emoji_sheet_view,
            emoji_sampler: &dummy_emoji_sampler,
            glyph_advances: &dummy_advances,
            item_params: &[],
        };

        let source = SlotSource::Device {
            chunk_capacity: emission.chunk_slots,
            chunks: &emission.chunks,
            glyph_count: total_survivors,
            mapped_base: emission.mapped_base,
        };

        let targets = FieldTargets {
            color_format: wgpu::TextureFormat::Rgba8UnormSrgb,
            depth_format: wgpu::TextureFormat::Depth32Float,
            sample_count: 1,
        };

        let field = DerivedField::new(&ctx.device, &ctx.queue, source, &resources, targets);
        assert_eq!(field.chunk_count(), expected_chunks as u32);
        assert_eq!(field.chunk_capacity(), chunk_cap as u32);
        assert_eq!(field.glyph_count(), total_survivors as u32);
    }

    fn prepass_with(row_count: u32, max_line_cols: u32) -> ItemPrepass {
        ItemPrepass {
            survivor_count: 1,
            leader_count: 1,
            max_row_extent: 0.0,
            row_count,
            has_cluster: false,
            has_emoji: false,
            max_line_cols,
        }
    }

    /// THE LANE LIMITS ARE A LOAD-TIME DECISION. A synthetic prepass past
    /// each Derived lane is refused with the item and the limit named; one
    /// within every lane passes, including the exact boundaries.
    #[test]
    fn derived_lane_limits_refuse_past_each_lane_and_name_the_item() {
        use glyph_field_derived::{ITEM_MAX, ROW_MAX, X_PAGE_MAX};
        let plain = ItemParams::default();
        let paged = ItemParams { has_page: true, page_cols: 10, ..ItemParams::default() };

        // Within: the boundary rows and the boundary column page pass.
        let ok = [prepass_with(ROW_MAX + 1, 5), prepass_with(1, (X_PAGE_MAX + 1) * 10 - 1), prepass_with(0, 0)];
        assert_eq!(derived_lane_limits(&ok, &[plain, paged, paged]), Ok(()));

        // Rows: item 1 has one row too many.
        let rows = [prepass_with(3, 1), prepass_with(ROW_MAX + 2, 1)];
        let why = derived_lane_limits(&rows, &[plain, plain]).unwrap_err();
        assert!(why.contains("item 1") && why.contains("ROW_MAX"), "{why}");

        // Column pages: item 0's widest line reaches page X_PAGE_MAX + 1 —
        // only because it IS column-paged; the same line under a plain item
        // has no column page to exceed.
        let wide = [prepass_with(1, (X_PAGE_MAX + 1) * 10)];
        let why = derived_lane_limits(&wide, &[paged]).unwrap_err();
        assert!(why.contains("item 0") && why.contains("X_PAGE_MAX"), "{why}");
        assert_eq!(derived_lane_limits(&wide, &[plain]), Ok(()));

        // Items: one past the item lane.
        let many = vec![prepass_with(1, 1); ITEM_MAX as usize + 2];
        let params = vec![plain; many.len()];
        let why = derived_lane_limits(&many, &params).unwrap_err();
        assert!(why.contains("ITEM_MAX"), "{why}");
        assert_eq!(derived_lane_limits(&many[..ITEM_MAX as usize + 1], &params[..ITEM_MAX as usize + 1]), Ok(()));
    }

    /// The fallback decision: Derived and Visible past the lanes become
    /// Instanced with a warning that names the reason; within them, or for
    /// Instanced itself, the request stands and nothing is said.
    #[test]
    fn effective_field_mode_falls_back_to_instanced_past_the_lanes() {
        let refused: Result<(), String> = Err("item 7: too many rows".into());
        for mode in [GlyphFieldMode::Derived, GlyphFieldMode::Visible] {
            let (got, note) = effective_field_mode(mode, &refused);
            assert_eq!(got, GlyphFieldMode::Instanced);
            let note = note.expect("a fallback is announced");
            assert!(note.contains(mode.as_str()) && note.contains("item 7") && note.contains("instanced"), "{note}");
            assert_eq!(effective_field_mode(mode, &Ok(())), (mode, None));
        }
        assert_eq!(effective_field_mode(GlyphFieldMode::Instanced, &refused), (GlyphFieldMode::Instanced, None));
    }

    /// `max_line_cols` is the widest line in leaders over the whole item,
    /// chunk cuts joined: held to the oracle-backed records (a line's leaders
    /// are its records before the newline) over corpora with intra-line
    /// chunk cuts (`chunk-cut.txt`) and multi-byte leaders, in both modes.
    #[test]
    fn max_line_cols_is_the_widest_line_in_leaders() {
        use crate::fold::ClusterMode;
        let trie = crate::default_trie();
        let em = trie.metrics.em_height_fu;
        let bitmap_adv = fu_to_world(trie.bitmap_advance_fu, em);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut checked = 0usize;
        for mode in [ClusterMode::Cluster, ClusterMode::Leader] {
            for p in ["native/fixtures/chunk-cut.txt", "native/fixtures/g-pick-repo", "native/fixtures/g-cluster-repo", "native/fixtures/cubecl-fork"] {
                let corpus = crate::hyper_oracle::load_corpus(&root.join(p), mode).expect(p);
                let items: Vec<LayoutItem<'_>> = corpus
                    .items
                    .iter()
                    .map(|it| LayoutItem { bytes: &it.bytes, params: it.params, group_id: 0, paint: Paint::Flat(0) })
                    .collect();
                let (chunks, ranges) = chunk::slice_items_into_chunks(&items);
                let agg = pass1_prepass_chunks(&chunks, &ranges, &items, &trie, bitmap_adv, em);
                for (i, it) in corpus.items.iter().enumerate() {
                    let r = crate::hyper_oracle::reference_item(&it.bytes, &it.params, &trie);
                    let mut widest = 0u32;
                    let mut start = 0usize;
                    for nl in memchr::memchr_iter(b'\n', &it.bytes).chain(std::iter::once(it.bytes.len())) {
                        if nl > start || nl < it.bytes.len() {
                            let leaders = r.record_bytes.iter().filter(|&&b| b >= start && b < nl).count() as u32;
                            widest = widest.max(leaders);
                        }
                        start = nl + 1;
                        if start > it.bytes.len() {
                            break;
                        }
                    }
                    assert_eq!(agg.prepasses[i].max_line_cols, widest, "{} / {}", corpus.name, it.label);
                    checked += 1;
                }
            }
        }
        assert!(checked >= 20, "checked only {checked} items");
    }
}

