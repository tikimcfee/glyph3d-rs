//! Pass 2 parallel layout worker for device-mapped / VRAM slot buffers.
//!
//! One walk, two slot formats. The per-glyph math (fold, wrap, pagination,
//! extents, tint, cull blocks) is written once; what a survivor becomes in
//! memory is the [`SlotEmit`] parameter, monomorphized per format so neither
//! format pays a branch for the other:
//!
//! - [`RenderEmit`] — the Instanced field's 32 B `RenderSlot`.
//! - [`DerivedEmit`] — the Derived field's 20 B `DerivedSlot`, whose Y/Z the
//!   vertex stage re-derives from the slot's row, its item's `ItemParamsGpu`
//!   and its wrap segment.

use rayon::prelude::*;
use crate::fold::{rows_for_line, wrap_row_of, wrap_segment_of};
use crate::glyph_scene::{BlockCull, RenderSlot, SUBSEG_BLOCK_SIZE};
use crate::layout::{FileTintAccum, InkExtent, ItemPlacement, LayoutItem, PageExtent, Paint};
use glyph_field_derived::DerivedSlot;
use super::char_resolve::{resolve_byte_char, ResolveCtx};
use super::page::Pager;
use super::types::Pass2DeviceOutput;

/// What every glyph of one row run shares, computed once per run and handed
/// to the run emitters (`emit_fast`, `emit_burst8/4`) by value: `Copy`, seven
/// scalars, which the inlined emitters break back into registers. Each slot
/// format reads its own subset (Instanced the world Y/Z, Derived the row and
/// wrap word). Named in C3 (2026-10-09), where the seven travelled as loose
/// arguments to every emitter.
#[derive(Clone, Copy)]
pub(crate) struct RowFrame {
    pub py: f32,
    pub pz: f32,
    pub row: u32,
    /// The wrap segment, pre-shifted into the high half of Derived's glyph word.
    pub wrap_high: u32,
    pub item_and_group: u32,
    pub group_id: u32,
    pub ascii_adv: f32,
}

/// A device slot format Pass 2 can emit directly.
pub(crate) trait SlotEmit: Sync {
    type Slot: Copy + Send + Sync;
    /// Whether this format indexes a line table (and so needs `line_bases`
    /// and the row bound checked).
    const USES_LINES: bool;
    /// The general path: one glyph, every field it can carry. A constructor —
    /// its arguments ARE the slot's fields, so a struct of them would be a
    /// second copy of the slot (C3 keeps this allow on purpose).
    #[allow(clippy::too_many_arguments)]
    fn emit(
        pos_x: f32,
        pos_y: f32,
        pos_z: f32,
        glyph_id: u32,
        color: u32,
        group_id: u32,
        item_idx: u32,
        advance: f32,
        height: f32,
        row: i64,
        wrap_segment: i64,
        x_page: i64,
    ) -> Self::Slot;
    fn emit_fast(
        pos_x: f32,
        glyph_id: u32,
        color: u32,
        frame: RowFrame,
    ) -> Self::Slot;
    unsafe fn emit_burst8(
        dest: *mut Self::Slot,
        pos_xs: [f32; 8],
        glyph_ids: [u32; 8],
        colors: [u32; 8],
        frame: RowFrame,
    );
    unsafe fn emit_burst4(
        dest: *mut Self::Slot,
        pos_xs: [f32; 4],
        glyph_ids: [u32; 4],
        colors: [u32; 4],
        frame: RowFrame,
    );
    /// `(glyph_id, color)` — the tint fold's view of a slot.
    fn tint_pair(s: &Self::Slot) -> [u32; 2];
}

pub(crate) struct RenderEmit;

impl SlotEmit for RenderEmit {
    type Slot = RenderSlot;
    const USES_LINES: bool = false;
    #[inline(always)]
    fn emit(
        pos_x: f32,
        pos_y: f32,
        pos_z: f32,
        glyph_id: u32,
        color: u32,
        group_id: u32,
        _item_idx: u32,
        advance: f32,
        height: f32,
        _row: i64,
        _wrap_segment: i64,
        _x_page: i64,
    ) -> RenderSlot {
        RenderSlot {
            pos: [pos_x, pos_y, pos_z],
            glyph_id,
            color,
            group_id,
            advance,
            height,
        }
    }
    #[inline(always)]
    fn emit_fast(
        pos_x: f32,
        glyph_id: u32,
        color: u32,
        frame: RowFrame,
    ) -> RenderSlot {
        let RowFrame { py: row_py, pz: row_pz, group_id, ascii_adv, .. } = frame;
        RenderSlot {
            pos: [pos_x, row_py, row_pz],
            glyph_id,
            color,
            group_id,
            advance: ascii_adv,
            height: crate::text::CELL_HEIGHT_WORLD,
        }
    }
    #[inline(always)]
    unsafe fn emit_burst8(
        dest: *mut RenderSlot,
        pos_xs: [f32; 8],
        glyph_ids: [u32; 8],
        colors: [u32; 8],
        frame: RowFrame,
    ) {
        let RowFrame { py: row_py, pz: row_pz, group_id, ascii_adv, .. } = frame;
        let cell_h = crate::text::CELL_HEIGHT_WORLD;
        dest.write(RenderSlot { pos: [pos_xs[0], row_py, row_pz], glyph_id: glyph_ids[0], color: colors[0], group_id, advance: ascii_adv, height: cell_h });
        dest.add(1).write(RenderSlot { pos: [pos_xs[1], row_py, row_pz], glyph_id: glyph_ids[1], color: colors[1], group_id, advance: ascii_adv, height: cell_h });
        dest.add(2).write(RenderSlot { pos: [pos_xs[2], row_py, row_pz], glyph_id: glyph_ids[2], color: colors[2], group_id, advance: ascii_adv, height: cell_h });
        dest.add(3).write(RenderSlot { pos: [pos_xs[3], row_py, row_pz], glyph_id: glyph_ids[3], color: colors[3], group_id, advance: ascii_adv, height: cell_h });
        dest.add(4).write(RenderSlot { pos: [pos_xs[4], row_py, row_pz], glyph_id: glyph_ids[4], color: colors[4], group_id, advance: ascii_adv, height: cell_h });
        dest.add(5).write(RenderSlot { pos: [pos_xs[5], row_py, row_pz], glyph_id: glyph_ids[5], color: colors[5], group_id, advance: ascii_adv, height: cell_h });
        dest.add(6).write(RenderSlot { pos: [pos_xs[6], row_py, row_pz], glyph_id: glyph_ids[6], color: colors[6], group_id, advance: ascii_adv, height: cell_h });
        dest.add(7).write(RenderSlot { pos: [pos_xs[7], row_py, row_pz], glyph_id: glyph_ids[7], color: colors[7], group_id, advance: ascii_adv, height: cell_h });
    }
    #[inline(always)]
    unsafe fn emit_burst4(
        dest: *mut RenderSlot,
        pos_xs: [f32; 4],
        glyph_ids: [u32; 4],
        colors: [u32; 4],
        frame: RowFrame,
    ) {
        let RowFrame { py: row_py, pz: row_pz, group_id, ascii_adv, .. } = frame;
        let cell_h = crate::text::CELL_HEIGHT_WORLD;
        dest.write(RenderSlot { pos: [pos_xs[0], row_py, row_pz], glyph_id: glyph_ids[0], color: colors[0], group_id, advance: ascii_adv, height: cell_h });
        dest.add(1).write(RenderSlot { pos: [pos_xs[1], row_py, row_pz], glyph_id: glyph_ids[1], color: colors[1], group_id, advance: ascii_adv, height: cell_h });
        dest.add(2).write(RenderSlot { pos: [pos_xs[2], row_py, row_pz], glyph_id: glyph_ids[2], color: colors[2], group_id, advance: ascii_adv, height: cell_h });
        dest.add(3).write(RenderSlot { pos: [pos_xs[3], row_py, row_pz], glyph_id: glyph_ids[3], color: colors[3], group_id, advance: ascii_adv, height: cell_h });
    }
    #[inline(always)]
    fn tint_pair(s: &RenderSlot) -> [u32; 2] {
        [s.glyph_id, s.color]
    }
}

pub(crate) struct DerivedEmit;

impl SlotEmit for DerivedEmit {
    type Slot = DerivedSlot;
    const USES_LINES: bool = true;
    #[inline(always)]
    fn emit(
        pos_x: f32,
        _pos_y: f32,
        _pos_z: f32,
        glyph_id: u32,
        color: u32,
        _group_id: u32,
        item_idx: u32,
        _advance: f32,
        _height: f32,
        row: i64,
        wrap_segment: i64,
        x_page: i64,
    ) -> DerivedSlot {
        let wrap = wrap_segment.clamp(0, u16::MAX as i64) as u16;
        DerivedSlot::with_item_and_group(
            pos_x,
            glyph_field_derived::pack_row(row.max(0) as u32, x_page.max(0) as u32),
            (glyph_id & 0xFFFF) as u16,
            wrap,
            color,
            glyph_field_derived::item_lane(item_idx),
        )
    }
    #[inline(always)]
    fn emit_fast(
        pos_x: f32,
        glyph_id: u32,
        color: u32,
        frame: RowFrame,
    ) -> DerivedSlot {
        let RowFrame { row: row_u32, wrap_high, item_and_group, .. } = frame;
        DerivedSlot {
            x: pos_x,
            row: row_u32,
            glyph_and_wrap: (glyph_id & 0xFFFF) | wrap_high,
            color,
            item_and_group,
        }
    }
    #[inline(always)]
    unsafe fn emit_burst8(
        dest: *mut DerivedSlot,
        pos_xs: [f32; 8],
        glyph_ids: [u32; 8],
        colors: [u32; 8],
        frame: RowFrame,
    ) {
        let RowFrame { row: row_u32, wrap_high, item_and_group, .. } = frame;
        dest.write(DerivedSlot { x: pos_xs[0], row: row_u32, glyph_and_wrap: (glyph_ids[0] & 0xFFFF) | wrap_high, color: colors[0], item_and_group });
        dest.add(1).write(DerivedSlot { x: pos_xs[1], row: row_u32, glyph_and_wrap: (glyph_ids[1] & 0xFFFF) | wrap_high, color: colors[1], item_and_group });
        dest.add(2).write(DerivedSlot { x: pos_xs[2], row: row_u32, glyph_and_wrap: (glyph_ids[2] & 0xFFFF) | wrap_high, color: colors[2], item_and_group });
        dest.add(3).write(DerivedSlot { x: pos_xs[3], row: row_u32, glyph_and_wrap: (glyph_ids[3] & 0xFFFF) | wrap_high, color: colors[3], item_and_group });
        dest.add(4).write(DerivedSlot { x: pos_xs[4], row: row_u32, glyph_and_wrap: (glyph_ids[4] & 0xFFFF) | wrap_high, color: colors[4], item_and_group });
        dest.add(5).write(DerivedSlot { x: pos_xs[5], row: row_u32, glyph_and_wrap: (glyph_ids[5] & 0xFFFF) | wrap_high, color: colors[5], item_and_group });
        dest.add(6).write(DerivedSlot { x: pos_xs[6], row: row_u32, glyph_and_wrap: (glyph_ids[6] & 0xFFFF) | wrap_high, color: colors[6], item_and_group });
        dest.add(7).write(DerivedSlot { x: pos_xs[7], row: row_u32, glyph_and_wrap: (glyph_ids[7] & 0xFFFF) | wrap_high, color: colors[7], item_and_group });
    }
    #[inline(always)]
    unsafe fn emit_burst4(
        dest: *mut DerivedSlot,
        pos_xs: [f32; 4],
        glyph_ids: [u32; 4],
        colors: [u32; 4],
        frame: RowFrame,
    ) {
        let RowFrame { row: row_u32, wrap_high, item_and_group, .. } = frame;
        dest.write(DerivedSlot { x: pos_xs[0], row: row_u32, glyph_and_wrap: (glyph_ids[0] & 0xFFFF) | wrap_high, color: colors[0], item_and_group });
        dest.add(1).write(DerivedSlot { x: pos_xs[1], row: row_u32, glyph_and_wrap: (glyph_ids[1] & 0xFFFF) | wrap_high, color: colors[1], item_and_group });
        dest.add(2).write(DerivedSlot { x: pos_xs[2], row: row_u32, glyph_and_wrap: (glyph_ids[2] & 0xFFFF) | wrap_high, color: colors[2], item_and_group });
        dest.add(3).write(DerivedSlot { x: pos_xs[3], row: row_u32, glyph_and_wrap: (glyph_ids[3] & 0xFFFF) | wrap_high, color: colors[3], item_and_group });
    }
    #[inline(always)]
    fn tint_pair(s: &DerivedSlot) -> [u32; 2] {
        [s.glyph_id() as u32, s.color]
    }
}

#[repr(align(64))]
pub(crate) struct ChunkPass2Output {
    pub slot_count: u32,
    pub record_count: u32,
    pub page_right: f32,
    pub page_bottom: f32,
    pub page_z_min: f32,
    pub page_z_max: f32,
    pub ink_min: [f32; 3],
    pub ink_max: [f32; 3],
    pub file_s0: f64,
    pub file_s1: f64,
    pub file_s2: f64,
    pub file_cells: usize,
    pub file_has_emoji: bool,
    pub emoji_cells: usize,
    pub local_blocks: Vec<BlockCull>,
    pub max_row_seen: i64,
}

/// Syntax colours for the two places a chunk can hold PART of a line (C17).
/// The colourisers are line-local, but a line can carry state across a cut
/// (an open string or comment; a word's colour is decided at its END and
/// filled back to its start), so colouring a chunk's share of a cut line on
/// its own disagrees with a walk of the whole item — 578 of
/// `g-pick-repo/wide.txt`'s glyphs did until 2026-10-09.
#[derive(Default)]
pub(crate) struct ChunkCutColors {
    /// The chunk STARTS inside a line: the colours of that line's leaders
    /// that fall in this chunk, out of a colouring of the whole line.
    head: Vec<u32>,
    /// The chunk's LAST line starts in the chunk and runs past its end: the
    /// colours of its leaders that fall in this chunk, likewise.
    tail: Vec<u32>,
}

/// Colour every line that a chunk cut falls inside ONCE, over the whole
/// line, and hand each chunk its share. Lines are found from the cuts alone
/// (a cut at a newline needs nothing), and each line's start and end are
/// searched once, so the work is O(cut lines), not O(chunks x line).
fn whole_line_colors_at_cuts(
    chunks: &[super::chunk::LayoutChunk<'_>],
    items: &[LayoutItem<'_>],
) -> Vec<ChunkCutColors> {
    let mut out: Vec<ChunkCutColors> = (0..chunks.len()).map(|_| ChunkCutColors::default()).collect();
    // (item, line start, line end — its newline or the item's end, first
    // chunk, last chunk)
    let mut lines: Vec<(usize, usize, usize, usize, usize)> = Vec::new();
    for c in 1..chunks.len() {
        let (prev, next) = (&chunks[c - 1], &chunks[c]);
        let item = &items[next.item_index];
        if prev.item_index != next.item_index || !matches!(item.paint, Paint::SyntaxHeuristic) {
            continue;
        }
        let cut = next.byte_offset;
        if cut == 0 || item.bytes[cut - 1] == b'\n' {
            continue;
        }
        if let Some(last) = lines.last_mut() {
            if last.0 == next.item_index && cut <= last.2 {
                last.4 = c;
                continue;
            }
        }
        let start = memchr::memrchr(b'\n', &item.bytes[..cut]).map_or(0, |i| i + 1);
        let end = memchr::memchr(b'\n', &item.bytes[cut..]).map_or(item.bytes.len(), |i| cut + i);
        lines.push((next.item_index, start, end, c - 1, c));
    }
    if lines.is_empty() {
        return out;
    }
    let leaders = |b: &[u8]| b.iter().filter(|&&x| crate::text::is_colorizer_leader(x)).count();
    let shares: Vec<Vec<(usize, bool, Vec<u32>)>> = lines
        .par_iter()
        .map(|&(it, start, end, c0, c1)| {
            let bytes = items[it].bytes;
            let mut colors = Vec::new();
            crate::text::colorize_line_into(&bytes[start..end], &mut colors);
            let mut k = 0usize;
            let mut res = Vec::with_capacity(c1 - c0 + 1);
            for (c, ch) in chunks.iter().enumerate().take(c1 + 1).skip(c0) {
                let a = ch.byte_offset.max(start);
                let b = (ch.byte_offset + ch.bytes.len()).min(end);
                let n = leaders(&bytes[a..b]);
                res.push((c, ch.byte_offset > start, colors[k..k + n].to_vec()));
                k += n;
            }
            res
        })
        .collect();
    for (c, is_head, run) in shares.into_iter().flatten() {
        if is_head {
            out[c].head = run;
        } else {
            out[c].tail = run;
        }
    }
    out
}

/// Pass 2 over chunk `chunk_idx` of `inputs`, into `dest_addr`. The chunk's
/// item, prepass, slot bases and the seed a mid-line chunk inherits (C15)
/// are read from `inputs` here rather than passed one by one (C3).
fn layout_pass2_chunk<E: SlotEmit>(
    inputs: &super::device_alloc::EmitInputs<'_, '_>,
    chunk_idx: usize,
    dest_addr: usize,
    lut: &[f64; 256],
    cut: &ChunkCutColors,
) -> ChunkPass2Output {
    let chunk = &inputs.chunks[chunk_idx];
    let chunk_bytes = chunk.bytes;
    let chunk_byte_offset = chunk.byte_offset;
    let item_idx = chunk.item_index as u32;
    let item = &inputs.items[chunk.item_index];
    let pre = &inputs.prepasses[chunk.item_index];
    let slot_base = inputs.chunk_slot_bases[chunk_idx];
    let item_slot_base = inputs.slot_bases[chunk.item_index];
    let initial_base_row = inputs.chunk_base_rows[chunk_idx];
    let initial_record_base = inputs.chunk_record_bases[chunk_idx];
    let initial_col = inputs.chunk_initial_cols[chunk_idx];
    let initial_seg_adv = inputs.chunk_initial_seg_advs[chunk_idx];
    let initial_line_adv = inputs.chunk_initial_line_advs[chunk_idx];
    let (trie, bitmap_adv, em_height_fu) = (inputs.trie, inputs.bitmap_adv, inputs.em_height_fu);
    let bytes = chunk_bytes;
    let p = &item.params;
    let cluster = super::char_resolve::clusters(p);
    let rctx = ResolveCtx { trie, bitmap_adv, em_height_fu, cluster };
    let group_id = item.group_id;
    let mut max_row_seen = -1i64;

    let fold_unit = if p.wrap_width > 0 {
        p.wrap_width as i64
    } else if p.has_page {
        p.page_cols as i64
    } else {
        0
    };
    let pager = Pager::new(p, pre.max_row_extent);
    let page_active = pager.active;
    // A column-paged item's cells change frame every `page_cols` columns;
    // the line fast path cuts its runs there too.
    let page_cols_run = if page_active { std::num::NonZeroUsize::new(pager.cols() as usize) } else { None };

    let mut page_right = 0.0f32;
    let mut page_bottom = 0.0f32;
    let mut page_z_min = 0.0f32;
    let mut page_z_max = 0.0f32;

    let mut ink_min = [f32::INFINITY; 3];
    let mut ink_max = [f32::NEG_INFINITY; 3];

    let mut base_row = initial_base_row;
    let mut col = initial_col;
    let mut line_adv = initial_line_adv;
    let mut seg_adv = initial_seg_adv;
    let mut line_start_col = initial_col;
    let mut record_idx = initial_record_base;
    let mut survivor_out = 0usize;
    let mut trailer_until = 0usize;

    let out_ptr = unsafe { (dest_addr as *mut E::Slot).add(slot_base as usize) };
    let flat_color = if let Paint::Flat(c) = item.paint { Some(c) } else { None };
    let is_syntax_heuristic = matches!(item.paint, Paint::SyntaxHeuristic);
    let per_record_colors = if let Paint::PerRecord(c) = item.paint { Some(c) } else { None };
    let mut line_colors = if is_syntax_heuristic {
        Vec::with_capacity(256)
    } else {
        Vec::new()
    };
    let mut stack_line_colors = [crate::layout::DEFAULT_COLOR_PACKED; 256];

    // A chunk that starts mid-line paints that line's leaders from a
    // colouring of the WHOLE line (C17), never from its own share of it.
    if is_syntax_heuristic && initial_col > 0 {
        line_colors.extend_from_slice(&cut.head);
    }
    let mut file_s0 = 0.0f64;
    let mut file_s1 = 0.0f64;
    let mut file_s2 = 0.0f64;
    let mut file_cells = 0usize;
    let mut file_has_emoji = false;
    let mut file_syntax_counts = [0u64; 6];

    let wrap_w = p.wrap_width as i64;
    let is_wrap_back = p.wrap_mode == crate::fold::WrapMode::Back;
    let line_height = p.line_height;
    let z_step = p.z_step;
    let origin_x = p.origin_x;
    let origin_y = p.origin_y;
    let origin_z = p.origin_z;

    let has_blocks = pre.survivor_count as usize > SUBSEG_BLOCK_SIZE;
    let mut cur_blk_min_x = f32::INFINITY;
    let mut cur_blk_min_y = f32::INFINITY;
    let mut cur_blk_min_z = f32::INFINITY;
    let mut cur_blk_max_x = f32::NEG_INFINITY;
    let mut cur_blk_max_y = f32::NEG_INFINITY;
    let mut cur_blk_max_z = f32::NEG_INFINITY;
    let mut cur_blk_count = 0usize;
    let mut local_blocks = if has_blocks {
        Vec::with_capacity((pre.survivor_count as usize).div_ceil(SUBSEG_BLOCK_SIZE))
    } else {
        Vec::new()
    };
    let chunk_rel_slot = slot_base - item_slot_base;

    let mut last_row = i64::MIN;
    let mut last_wrap_seg = i64::MIN;
    let mut last_x_page = i64::MIN;
    let mut cached_page_x_off = 0.0f64;
    let mut cached_py = 0.0f32;
    let mut cached_pz = 0.0f32;

    let ascii_adv = crate::text::fu_to_world(1229, em_height_fu);
    let mut last_ink_y = f32::NAN;
    let mut last_ink_z = f32::NAN;
    let mut emoji_cells = 0usize;

    let tint_default = [
        lut[crate::text::palette::DEFAULT[0] as usize],
        lut[crate::text::palette::DEFAULT[1] as usize],
        lut[crate::text::palette::DEFAULT[2] as usize],
    ];
    let tint_keyword = [
        lut[crate::text::palette::KEYWORD[0] as usize],
        lut[crate::text::palette::KEYWORD[1] as usize],
        lut[crate::text::palette::KEYWORD[2] as usize],
    ];
    let tint_number = [
        lut[crate::text::palette::NUMBER[0] as usize],
        lut[crate::text::palette::NUMBER[1] as usize],
        lut[crate::text::palette::NUMBER[2] as usize],
    ];
    let tint_string = [
        lut[crate::text::palette::STRING[0] as usize],
        lut[crate::text::palette::STRING[1] as usize],
        lut[crate::text::palette::STRING[2] as usize],
    ];
    let tint_comment = [
        lut[crate::text::palette::COMMENT[0] as usize],
        lut[crate::text::palette::COMMENT[1] as usize],
        lut[crate::text::palette::COMMENT[2] as usize],
    ];
    let tint_punct = [
        lut[crate::text::palette::PUNCT[0] as usize],
        lut[crate::text::palette::PUNCT[1] as usize],
        lut[crate::text::palette::PUNCT[2] as usize],
    ];

    let mut pos = 0usize;
    let mut span_idx = if let Paint::ByteSpans(spans) = item.paint {
        spans.partition_point(|s| s.end <= chunk_byte_offset as u32)
    } else {
        0
    };

    while pos < bytes.len() {
        if col == 0 {
            let nl_offset = memchr::memchr(b'\n', &bytes[pos..]);
            let line_end = match nl_offset {
                Some(off) => pos + off,
                None => bytes.len(),
            };
            let line_bytes = &bytes[pos..line_end];
            let line_len = line_bytes.len();
            let is_pure_ascii = crate::text::is_pure_printable_ascii(line_bytes);

            let mut ascii_syntax_counts = None;
            let mut is_comment_line = false;
            // The chunk's last line, cut at the chunk's end, likewise (C17).
            let cut_tail = if nl_offset.is_none() && !cut.tail.is_empty() { Some(&cut.tail[..]) } else { None };
            if let (true, Some(tail)) = (is_syntax_heuristic, cut_tail) {
                if is_pure_ascii {
                    let dst = if line_len <= 256 { &mut stack_line_colors[..line_len] } else {
                        line_colors.clear();
                        line_colors.resize(line_len, 0);
                        &mut line_colors[..]
                    };
                    dst.copy_from_slice(tail);
                    let counts = crate::text::palette_counts(tail);
                    is_comment_line = counts[4] as usize == line_len;
                    ascii_syntax_counts = Some(counts);
                } else {
                    line_colors.clear();
                    line_colors.extend_from_slice(tail);
                }
            } else if is_syntax_heuristic {
                if is_pure_ascii {
                    let counts = if line_len <= 256 {
                        crate::text::colorize_pure_ascii_line_slice(line_bytes, &mut stack_line_colors[..line_len])
                    } else {
                        crate::text::colorize_pure_ascii_line(line_bytes, &mut line_colors)
                    };
                    // Every cell comment: paint the line flat. Indentation is
                    // PUNCT, not comment, so an indented comment line takes the
                    // per-cell colours; until 2026-10-09 this counted the
                    // indentation in and painted it comment, which no colouriser
                    // does (C17; a space has no ink, so no pixel moved).
                    is_comment_line = counts[4] as usize == line_len;
                    ascii_syntax_counts = Some(counts);
                } else {
                    crate::text::colorize_line_into(line_bytes, &mut line_colors);
                }
            }

            // Line-level ASCII fast path (handles both single-segment and wrapped multi-segment lines):
            if !matches!(item.paint, Paint::ByteSpans(_)) && is_pure_ascii {
                if flat_color.is_none() {
                    if let Some(counts) = ascii_syntax_counts {
                        file_syntax_counts[0] += counts[0] as u64;
                        file_syntax_counts[1] += counts[1] as u64;
                        file_syntax_counts[2] += counts[2] as u64;
                        file_syntax_counts[3] += counts[3] as u64;
                        file_syntax_counts[4] += counts[4] as u64;
                        file_syntax_counts[5] += counts[5] as u64;
                        file_cells += line_len;
                    } else if let Some(colors) = per_record_colors {
                        let line_c = if record_idx + line_len <= colors.len() {
                            &colors[record_idx..record_idx + line_len]
                        } else {
                            &colors[record_idx..]
                        };
                        for &c in line_c {
                            file_s0 += lut[(c & 0xFF) as usize];
                            file_s1 += lut[((c >> 8) & 0xFF) as usize];
                            file_s2 += lut[((c >> 16) & 0xFF) as usize];
                        }
                        file_cells += line_c.len();
                    } else {
                        let def = crate::layout::DEFAULT_COLOR_PACKED;
                        let c0 = (def & 0xFF) as usize;
                        let c1 = ((def >> 8) & 0xFF) as usize;
                        let c2 = ((def >> 16) & 0xFF) as usize;
                        file_s0 += lut[c0] * line_len as f64;
                        file_s1 += lut[c1] * line_len as f64;
                        file_s2 += lut[c2] * line_len as f64;
                        file_cells += line_len;
                    }
                }

                // A RUN is a stretch of cells that share one frame and one
                // segment-advance carrier: it ends where the fold unit closes
                // (the segment advance resets) and, for a column-paged item,
                // where the column page turns. Without column paging a run is
                // a wrap segment, as it always was.
                let seg_limit = if fold_unit > 0 { fold_unit as usize } else { usize::MAX };
                let mut seg_offset = 0usize;
                let mut wrap_segment = 0i64;
                let mut x_page = 0i64;
                let mut line_adv_f64 = line_adv;
                let mut seg_adv_f32 = seg_adv;
                // The last run's advance before its reset: the newline's x
                // when the line does not end on a fold-unit boundary.
                let mut last_seg_adv = seg_adv_f32;

                while seg_offset < line_len {
                    let seg_len = match page_cols_run {
                        None => (line_len - seg_offset).min(seg_limit),
                        Some(page_cols) => {
                            // seg_limit is finite here: a column-paged item
                            // folds at wrap_width, or at page_cols itself.
                            let page_cols = page_cols.get();
                            wrap_segment = if wrap_w > 0 { seg_offset as i64 / wrap_w } else { 0 };
                            x_page = (seg_offset / page_cols) as i64;
                            (line_len - seg_offset)
                                .min(seg_limit - seg_offset % seg_limit)
                                .min(page_cols - seg_offset % page_cols)
                        }
                    };
                    let seg_bytes = &line_bytes[seg_offset..seg_offset + seg_len];

                    let row = if is_wrap_back {
                        base_row
                    } else {
                        base_row + wrap_segment
                    };

                    if row != last_row || wrap_segment != last_wrap_seg || x_page != last_x_page {
                        last_row = row;
                        last_wrap_seg = wrap_segment;
                        last_x_page = x_page;

                        if page_active {
                            let f = pager.frame(row, seg_offset as i64, wrap_segment);
                            cached_page_x_off = f.x_off;
                            cached_py = f.y;
                            cached_pz = f.z;
                        } else {
                            cached_page_x_off = 0.0;
                            cached_py = (-(row as f64) * line_height + origin_y) as f32;
                            cached_pz = (-(wrap_segment as f64) * z_step + origin_z) as f32;
                        }

                        if cached_py < page_bottom {
                            page_bottom = cached_py;
                        }
                        if cached_pz < page_z_min {
                            page_z_min = cached_pz;
                        }
                        if cached_pz > page_z_max {
                            page_z_max = cached_pz;
                        }
                    }

                    let row_py = cached_py;
                    let row_pz = cached_pz;
                    // The Derived row lane carries the column page (derive.rs).
                    let row_u32 = glyph_field_derived::pack_row(row.max(0) as u32, x_page.max(0) as u32);
                    let wrap_high = ((wrap_segment.max(0) as u32) & 0xFFFF) << 16;
                    // The Derived lane is the ITEM (its group comes from the item
                    // table; an override rides the high 12 bits, derive.rs).
                    let item_and_group = glyph_field_derived::item_lane(item_idx);
                    let frame = RowFrame { py: row_py, pz: row_pz, row: row_u32, wrap_high, item_and_group, group_id, ascii_adv };
                    let start_survivors = survivor_out;

                    let qw = ascii_adv.max(crate::text::CELL_HEIGHT_WORLD);
                    let half_h = 0.5 * crate::text::CELL_HEIGHT_WORLD;
                    let y_lo = row_py - half_h;
                    let y_hi = row_py + half_h;

                    let mut seg_first_survivor_x = f32::NAN;
                    let mut seg_last_survivor_right = f32::NAN;
                    let mut last_char_pos_x = f32::NAN;
                    let effective_flat_color = flat_color.or(if is_comment_line { Some(crate::text::palette::C_COMMENT) } else { None });

                    let get_color = |idx: usize| -> u32 {
                        if let Some(c) = effective_flat_color {
                            c
                        } else if is_syntax_heuristic {
                            if line_len <= 256 {
                                if idx < line_len {
                                    stack_line_colors[idx]
                                } else {
                                    crate::layout::DEFAULT_COLOR_PACKED
                                }
                            } else if idx < line_colors.len() {
                                line_colors[idx]
                            } else {
                                crate::layout::DEFAULT_COLOR_PACKED
                            }
                        } else if let Some(colors) = per_record_colors {
                            if record_idx + idx < colors.len() {
                                colors[record_idx + idx]
                            } else {
                                0xFFFF_FFFF
                            }
                        } else {
                            crate::layout::DEFAULT_COLOR_PACKED
                        }
                    };

                    let ascii_adv_f64 = ascii_adv as f64;
                    let mut char_idx_in_seg = 0usize;

                    // 8-wide burst loop:
                    while char_idx_in_seg + 8 <= seg_bytes.len() {
                        let b0 = seg_bytes[char_idx_in_seg];
                        let b1 = seg_bytes[char_idx_in_seg + 1];
                        let b2 = seg_bytes[char_idx_in_seg + 2];
                        let b3 = seg_bytes[char_idx_in_seg + 3];
                        let b4 = seg_bytes[char_idx_in_seg + 4];
                        let b5 = seg_bytes[char_idx_in_seg + 5];
                        let b6 = seg_bytes[char_idx_in_seg + 6];
                        let b7 = seg_bytes[char_idx_in_seg + 7];

                        let g0 = trie.fast_byte_table[b0 as usize].glyph_id;
                        let g1 = trie.fast_byte_table[b1 as usize].glyph_id;
                        let g2 = trie.fast_byte_table[b2 as usize].glyph_id;
                        let g3 = trie.fast_byte_table[b3 as usize].glyph_id;
                        let g4 = trie.fast_byte_table[b4 as usize].glyph_id;
                        let g5 = trie.fast_byte_table[b5 as usize].glyph_id;
                        let g6 = trie.fast_byte_table[b6 as usize].glyph_id;
                        let g7 = trie.fast_byte_table[b7 as usize].glyph_id;

                        if g0 != 0 && g1 != 0 && g2 != 0 && g3 != 0
                            && g4 != 0 && g5 != 0 && g6 != 0 && g7 != 0
                            && (!has_blocks || cur_blk_count + 8 <= SUBSEG_BLOCK_SIZE)
                        {
                            let char_idx0 = seg_offset + char_idx_in_seg;

                            let adv0 = line_adv_f64;
                            let adv1 = adv0 + ascii_adv_f64;
                            let adv2 = adv1 + ascii_adv_f64;
                            let adv3 = adv2 + ascii_adv_f64;
                            let adv4 = adv3 + ascii_adv_f64;
                            let adv5 = adv4 + ascii_adv_f64;
                            let adv6 = adv5 + ascii_adv_f64;
                            let adv7 = adv6 + ascii_adv_f64;
                            line_adv_f64 = adv7 + ascii_adv_f64;

                            let s0 = seg_adv_f32;
                            let s1 = s0 + ascii_adv;
                            let s2 = s1 + ascii_adv;
                            let s3 = s2 + ascii_adv;
                            let s4 = s3 + ascii_adv;
                            let s5 = s4 + ascii_adv;
                            let s6 = s5 + ascii_adv;
                            let s7 = s6 + ascii_adv;
                            seg_adv_f32 = s7 + ascii_adv;

                            let (x0, x1, x2, x3, x4, x5, x6, x7) = if fold_unit > 0 {
                                (
                                    (s0 as f64 + origin_x) as f32,
                                    (s1 as f64 + origin_x) as f32,
                                    (s2 as f64 + origin_x) as f32,
                                    (s3 as f64 + origin_x) as f32,
                                    (s4 as f64 + origin_x) as f32,
                                    (s5 as f64 + origin_x) as f32,
                                    (s6 as f64 + origin_x) as f32,
                                    (s7 as f64 + origin_x) as f32,
                                )
                            } else {
                                (
                                    (adv0 + origin_x) as f32,
                                    (adv1 + origin_x) as f32,
                                    (adv2 + origin_x) as f32,
                                    (adv3 + origin_x) as f32,
                                    (adv4 + origin_x) as f32,
                                    (adv5 + origin_x) as f32,
                                    (adv6 + origin_x) as f32,
                                    (adv7 + origin_x) as f32,
                                )
                            };

                            let (pos_x0, pos_x1, pos_x2, pos_x3, pos_x4, pos_x5, pos_x6, pos_x7) = if page_active {
                                (
                                    (x0 as f64 + cached_page_x_off) as f32,
                                    (x1 as f64 + cached_page_x_off) as f32,
                                    (x2 as f64 + cached_page_x_off) as f32,
                                    (x3 as f64 + cached_page_x_off) as f32,
                                    (x4 as f64 + cached_page_x_off) as f32,
                                    (x5 as f64 + cached_page_x_off) as f32,
                                    (x6 as f64 + cached_page_x_off) as f32,
                                    (x7 as f64 + cached_page_x_off) as f32,
                                )
                            } else {
                                (x0, x1, x2, x3, x4, x5, x6, x7)
                            };
                            last_char_pos_x = pos_x7;

                            if seg_first_survivor_x.is_nan() {
                                seg_first_survivor_x = pos_x0;
                            }
                            seg_last_survivor_right = pos_x7 + ascii_adv;

                            let (c0, c1, c2, c3, c4, c5, c6, c7) = if let Some(c) = effective_flat_color {
                                (c, c, c, c, c, c, c, c)
                            } else if is_syntax_heuristic {
                                if line_len <= 256 {
                                    let colors_chunk: &[u32; 8] = stack_line_colors[char_idx0..char_idx0 + 8].try_into().unwrap();
                                    (
                                        colors_chunk[0],
                                        colors_chunk[1],
                                        colors_chunk[2],
                                        colors_chunk[3],
                                        colors_chunk[4],
                                        colors_chunk[5],
                                        colors_chunk[6],
                                        colors_chunk[7],
                                    )
                                } else {
                                    let colors_chunk: &[u32; 8] = line_colors[char_idx0..char_idx0 + 8].try_into().unwrap();
                                    (
                                        colors_chunk[0],
                                        colors_chunk[1],
                                        colors_chunk[2],
                                        colors_chunk[3],
                                        colors_chunk[4],
                                        colors_chunk[5],
                                        colors_chunk[6],
                                        colors_chunk[7],
                                    )
                                }
                            } else if per_record_colors.is_some() {
                                (
                                    get_color(char_idx0),
                                    get_color(char_idx0 + 1),
                                    get_color(char_idx0 + 2),
                                    get_color(char_idx0 + 3),
                                    get_color(char_idx0 + 4),
                                    get_color(char_idx0 + 5),
                                    get_color(char_idx0 + 6),
                                    get_color(char_idx0 + 7),
                                )
                            } else {
                                let def = crate::layout::DEFAULT_COLOR_PACKED;
                                (def, def, def, def, def, def, def, def)
                            };

                            unsafe {
                                E::emit_burst8(
                                    out_ptr.add(survivor_out),
                                    [pos_x0, pos_x1, pos_x2, pos_x3, pos_x4, pos_x5, pos_x6, pos_x7],
                                    [g0, g1, g2, g3, g4, g5, g6, g7],
                                    [c0, c1, c2, c3, c4, c5, c6, c7],
                                    frame,
                                );
                            }
                            survivor_out += 8;

                            if has_blocks {
                                if pos_x0 < cur_blk_min_x {
                                    cur_blk_min_x = pos_x0;
                                }
                                let burst_max_x = pos_x7 + qw;
                                if burst_max_x > cur_blk_max_x {
                                    cur_blk_max_x = burst_max_x;
                                }
                                if y_lo < cur_blk_min_y {
                                    cur_blk_min_y = y_lo;
                                }
                                if y_hi > cur_blk_max_y {
                                    cur_blk_max_y = y_hi;
                                }
                                if row_pz < cur_blk_min_z {
                                    cur_blk_min_z = row_pz;
                                }
                                if row_pz > cur_blk_max_z {
                                    cur_blk_max_z = row_pz;
                                }
                                cur_blk_count += 8;
                                if cur_blk_count == SUBSEG_BLOCK_SIZE {
                                    if cur_blk_min_x <= cur_blk_max_x && cur_blk_min_y <= cur_blk_max_y {
                                        local_blocks.push(BlockCull {
                                            min: [cur_blk_min_x, cur_blk_min_y, cur_blk_min_z],
                                            max: [cur_blk_max_x, cur_blk_max_y, cur_blk_max_z],
                                            slot_base: chunk_rel_slot + (survivor_out - SUBSEG_BLOCK_SIZE) as u32,
                                            slot_count: SUBSEG_BLOCK_SIZE as u32,
                                        });
                                    }
                                    cur_blk_min_x = f32::INFINITY;
                                    cur_blk_min_y = f32::INFINITY;
                                    cur_blk_min_z = f32::INFINITY;
                                    cur_blk_max_x = f32::NEG_INFINITY;
                                    cur_blk_max_y = f32::NEG_INFINITY;
                                    cur_blk_max_z = f32::NEG_INFINITY;
                                    cur_blk_count = 0;
                                }
                            }

                            char_idx_in_seg += 8;
                            continue;
                        }

                        break;
                    }

                    // 4-wide burst loop:
                    while char_idx_in_seg + 4 <= seg_bytes.len() {
                        let b0 = seg_bytes[char_idx_in_seg];
                        let b1 = seg_bytes[char_idx_in_seg + 1];
                        let b2 = seg_bytes[char_idx_in_seg + 2];
                        let b3 = seg_bytes[char_idx_in_seg + 3];

                        let g0 = trie.fast_byte_table[b0 as usize].glyph_id;
                        let g1 = trie.fast_byte_table[b1 as usize].glyph_id;
                        let g2 = trie.fast_byte_table[b2 as usize].glyph_id;
                        let g3 = trie.fast_byte_table[b3 as usize].glyph_id;

                        if g0 != 0 && g1 != 0 && g2 != 0 && g3 != 0 && (!has_blocks || cur_blk_count + 4 <= SUBSEG_BLOCK_SIZE) {


                            let char_idx0 = seg_offset + char_idx_in_seg;

                            let adv0 = line_adv_f64;
                            let adv1 = adv0 + ascii_adv_f64;
                            let adv2 = adv1 + ascii_adv_f64;
                            let adv3 = adv2 + ascii_adv_f64;
                            line_adv_f64 = adv3 + ascii_adv_f64;

                            let s0 = seg_adv_f32;
                            let s1 = s0 + ascii_adv;
                            let s2 = s1 + ascii_adv;
                            let s3 = s2 + ascii_adv;
                            seg_adv_f32 = s3 + ascii_adv;

                            let (x0, x1, x2, x3) = if fold_unit > 0 {
                                (
                                    (s0 as f64 + origin_x) as f32,
                                    (s1 as f64 + origin_x) as f32,
                                    (s2 as f64 + origin_x) as f32,
                                    (s3 as f64 + origin_x) as f32,
                                )
                            } else {
                                (
                                    (adv0 + origin_x) as f32,
                                    (adv1 + origin_x) as f32,
                                    (adv2 + origin_x) as f32,
                                    (adv3 + origin_x) as f32,
                                )
                            };

                            let (pos_x0, pos_x1, pos_x2, pos_x3) = if page_active {
                                (
                                    (x0 as f64 + cached_page_x_off) as f32,
                                    (x1 as f64 + cached_page_x_off) as f32,
                                    (x2 as f64 + cached_page_x_off) as f32,
                                    (x3 as f64 + cached_page_x_off) as f32,
                                )
                            } else {
                                (x0, x1, x2, x3)
                            };
                            last_char_pos_x = pos_x3;

                            if seg_first_survivor_x.is_nan() {
                                seg_first_survivor_x = pos_x0;
                            }
                            seg_last_survivor_right = pos_x3 + ascii_adv;

                            let (c0, c1, c2, c3) = if let Some(c) = effective_flat_color {
                                (c, c, c, c)
                            } else if is_syntax_heuristic {
                                if line_len <= 256 {
                                    let colors_chunk: &[u32; 4] = stack_line_colors[char_idx0..char_idx0 + 4].try_into().unwrap();
                                    (
                                        colors_chunk[0],
                                        colors_chunk[1],
                                        colors_chunk[2],
                                        colors_chunk[3],
                                    )
                                } else {
                                    let colors_chunk: &[u32; 4] = line_colors[char_idx0..char_idx0 + 4].try_into().unwrap();
                                    (
                                        colors_chunk[0],
                                        colors_chunk[1],
                                        colors_chunk[2],
                                        colors_chunk[3],
                                    )
                                }
                            } else if per_record_colors.is_some() {
                                (
                                    get_color(char_idx0),
                                    get_color(char_idx0 + 1),
                                    get_color(char_idx0 + 2),
                                    get_color(char_idx0 + 3),
                                )
                            } else {
                                (
                                    crate::layout::DEFAULT_COLOR_PACKED,
                                    crate::layout::DEFAULT_COLOR_PACKED,
                                    crate::layout::DEFAULT_COLOR_PACKED,
                                    crate::layout::DEFAULT_COLOR_PACKED,
                                )
                            };

                            unsafe {
                                E::emit_burst4(
                                    out_ptr.add(survivor_out),
                                    [pos_x0, pos_x1, pos_x2, pos_x3],
                                    [g0, g1, g2, g3],
                                    [c0, c1, c2, c3],
                                    frame,
                                );
                            }
                            survivor_out += 4;

                            if has_blocks {
                                if pos_x0 < cur_blk_min_x {
                                    cur_blk_min_x = pos_x0;
                                }
                                let burst_max_x = pos_x3 + qw;
                                if burst_max_x > cur_blk_max_x {
                                    cur_blk_max_x = burst_max_x;
                                }
                                if y_lo < cur_blk_min_y {
                                    cur_blk_min_y = y_lo;
                                }
                                if y_hi > cur_blk_max_y {
                                    cur_blk_max_y = y_hi;
                                }
                                if row_pz < cur_blk_min_z {
                                    cur_blk_min_z = row_pz;
                                }
                                if row_pz > cur_blk_max_z {
                                    cur_blk_max_z = row_pz;
                                }
                                cur_blk_count += 4;
                                if cur_blk_count == SUBSEG_BLOCK_SIZE {
                                    if cur_blk_min_x <= cur_blk_max_x && cur_blk_min_y <= cur_blk_max_y {
                                        local_blocks.push(BlockCull {
                                            min: [cur_blk_min_x, cur_blk_min_y, cur_blk_min_z],
                                            max: [cur_blk_max_x, cur_blk_max_y, cur_blk_max_z],
                                            slot_base: chunk_rel_slot + (survivor_out - SUBSEG_BLOCK_SIZE) as u32,
                                            slot_count: SUBSEG_BLOCK_SIZE as u32,
                                        });
                                    }
                                    cur_blk_min_x = f32::INFINITY;
                                    cur_blk_min_y = f32::INFINITY;
                                    cur_blk_min_z = f32::INFINITY;
                                    cur_blk_max_x = f32::NEG_INFINITY;
                                    cur_blk_max_y = f32::NEG_INFINITY;
                                    cur_blk_max_z = f32::NEG_INFINITY;
                                    cur_blk_count = 0;
                                }
                            }

                            char_idx_in_seg += 4;
                            continue;
                        }

                        // Fallback single character:
                        let b = seg_bytes[char_idx_in_seg];
                        let char_idx = seg_offset + char_idx_in_seg;
                        let item_rel_x = if fold_unit > 0 { seg_adv_f32 as f64 } else { line_adv_f64 };
                        let base_x = (item_rel_x + origin_x) as f32;
                        let pos_x = if page_active {
                            (base_x as f64 + cached_page_x_off) as f32
                        } else {
                            base_x
                        };
                        last_char_pos_x = pos_x;
                        line_adv_f64 += ascii_adv_f64;
                        seg_adv_f32 += ascii_adv;

                        let g0 = trie.fast_byte_table[b as usize].glyph_id;
                        if g0 != 0 {
                            if seg_first_survivor_x.is_nan() {
                                seg_first_survivor_x = pos_x;
                            }
                            seg_last_survivor_right = pos_x + ascii_adv;

                            let color = get_color(char_idx);

                            unsafe {
                                out_ptr.add(survivor_out).write(E::emit_fast(
                                    pos_x,
                                    g0,
                                    color,
                                    frame,
                                ));
                            }
                            survivor_out += 1;

                            if has_blocks {
                                if pos_x < cur_blk_min_x {
                                    cur_blk_min_x = pos_x;
                                }
                                let char_max_x = pos_x + qw;
                                if char_max_x > cur_blk_max_x {
                                    cur_blk_max_x = char_max_x;
                                }
                                if y_lo < cur_blk_min_y {
                                    cur_blk_min_y = y_lo;
                                }
                                if y_hi > cur_blk_max_y {
                                    cur_blk_max_y = y_hi;
                                }
                                if row_pz < cur_blk_min_z {
                                    cur_blk_min_z = row_pz;
                                }
                                if row_pz > cur_blk_max_z {
                                    cur_blk_max_z = row_pz;
                                }
                                cur_blk_count += 1;

                                if cur_blk_count == SUBSEG_BLOCK_SIZE {
                                    if cur_blk_min_x <= cur_blk_max_x && cur_blk_min_y <= cur_blk_max_y {
                                        local_blocks.push(BlockCull {
                                            min: [cur_blk_min_x, cur_blk_min_y, cur_blk_min_z],
                                            max: [cur_blk_max_x, cur_blk_max_y, cur_blk_max_z],
                                            slot_base: chunk_rel_slot + (survivor_out - SUBSEG_BLOCK_SIZE) as u32,
                                            slot_count: SUBSEG_BLOCK_SIZE as u32,
                                        });
                                    }
                                    cur_blk_min_x = f32::INFINITY;
                                    cur_blk_min_y = f32::INFINITY;
                                    cur_blk_min_z = f32::INFINITY;
                                    cur_blk_max_x = f32::NEG_INFINITY;
                                    cur_blk_max_y = f32::NEG_INFINITY;
                                    cur_blk_max_z = f32::NEG_INFINITY;
                                    cur_blk_count = 0;
                                }
                            }
                        }
                        char_idx_in_seg += 1;

                    }

                    // Trailing 0..3 characters in segment:
                    while char_idx_in_seg < seg_bytes.len() {
                        let b = seg_bytes[char_idx_in_seg];
                        let char_idx = seg_offset + char_idx_in_seg;
                        let item_rel_x = if fold_unit > 0 { seg_adv_f32 as f64 } else { line_adv_f64 };
                        let base_x = (item_rel_x + origin_x) as f32;
                        let pos_x = if page_active {
                            (base_x as f64 + cached_page_x_off) as f32
                        } else {
                            base_x
                        };
                        last_char_pos_x = pos_x;
                        line_adv_f64 += ascii_adv as f64;
                        seg_adv_f32 += ascii_adv;

                        let glyph_id = trie.fast_byte_table[b as usize].glyph_id;
                        if glyph_id != 0 {
                            if seg_first_survivor_x.is_nan() {
                                seg_first_survivor_x = pos_x;
                            }
                            seg_last_survivor_right = pos_x + ascii_adv;

                            let color = get_color(char_idx);

                            unsafe {
                                out_ptr.add(survivor_out).write(E::emit_fast(
                                    pos_x,
                                    glyph_id,
                                    color,
                                    frame,
                                ));
                            }
                            survivor_out += 1;

                            if has_blocks {
                                if pos_x < cur_blk_min_x {
                                    cur_blk_min_x = pos_x;
                                }
                                let char_max_x = pos_x + qw;
                                if char_max_x > cur_blk_max_x {
                                    cur_blk_max_x = char_max_x;
                                }
                                if y_lo < cur_blk_min_y {
                                    cur_blk_min_y = y_lo;
                                }
                                if y_hi > cur_blk_max_y {
                                    cur_blk_max_y = y_hi;
                                }
                                if row_pz < cur_blk_min_z {
                                    cur_blk_min_z = row_pz;
                                }
                                if row_pz > cur_blk_max_z {
                                    cur_blk_max_z = row_pz;
                                }
                                cur_blk_count += 1;

                                if cur_blk_count == SUBSEG_BLOCK_SIZE {
                                    if cur_blk_min_x <= cur_blk_max_x && cur_blk_min_y <= cur_blk_max_y {
                                        local_blocks.push(BlockCull {
                                            min: [cur_blk_min_x, cur_blk_min_y, cur_blk_min_z],
                                            max: [cur_blk_max_x, cur_blk_max_y, cur_blk_max_z],
                                            slot_base: chunk_rel_slot + (survivor_out - SUBSEG_BLOCK_SIZE) as u32,
                                            slot_count: SUBSEG_BLOCK_SIZE as u32,
                                        });
                                    }
                                    cur_blk_min_x = f32::INFINITY;
                                    cur_blk_min_y = f32::INFINITY;
                                    cur_blk_min_z = f32::INFINITY;
                                    cur_blk_max_x = f32::NEG_INFINITY;
                                    cur_blk_max_y = f32::NEG_INFINITY;
                                    cur_blk_max_z = f32::NEG_INFINITY;
                                    cur_blk_count = 0;
                                }
                            }
                        }
                        char_idx_in_seg += 1;
                    }

                    if !last_char_pos_x.is_nan() {
                        let right = last_char_pos_x + ascii_adv;
                        if right > page_right {
                            page_right = right;
                        }
                    }
                    if !seg_first_survivor_x.is_nan() {
                        if seg_first_survivor_x < ink_min[0] {
                            ink_min[0] = seg_first_survivor_x;
                        }
                        if seg_last_survivor_right > ink_max[0] {
                            ink_max[0] = seg_last_survivor_right;
                        }
                    }

                    if survivor_out > start_survivors {
                        let half = 0.5 * crate::text::CELL_HEIGHT_WORLD;
                        if row_py - half < ink_min[1] {
                            ink_min[1] = row_py - half;
                        }
                        if row_py + half > ink_max[1] {
                            ink_max[1] = row_py + half;
                        }
                        if row_pz < ink_min[2] {
                            ink_min[2] = row_pz;
                        }
                        if row_pz > ink_max[2] {
                            ink_max[2] = row_pz;
                        }
                        last_ink_y = row_py;
                        last_ink_z = row_pz;
                        if E::USES_LINES && row > max_row_seen {
                            max_row_seen = row;
                        }
                    }

                    seg_offset += seg_len;
                    last_seg_adv = seg_adv_f32;
                    if page_cols_run.is_none() {
                        wrap_segment += 1;
                        seg_adv_f32 = 0.0;
                    } else if seg_offset.is_multiple_of(seg_limit) {
                        // A column-page cut inside a fold unit carries the
                        // segment advance on; only the fold unit resets it.
                        seg_adv_f32 = 0.0;
                    }
                }

                record_idx += line_len;
                col = line_len as i64;
                line_adv = line_adv_f64;
                seg_adv = seg_adv_f32;
                pos = line_end;

                if pos < bytes.len() && bytes[pos] == b'\n' {
                    // The newline's own record. PageExtent is measured over
                    // EVERY record (layout.rs), and the newline occupies its
                    // advance at column `col` on the row it closes: for a
                    // non-empty line that is the last run's frame (a
                    // terminator stays in the segment it closes: already
                    // folded into the page above, and still cached) unless a
                    // column page turns exactly at the line's end; for an
                    // empty line a row no glyph visited. Once per line; the
                    // per-byte paths are untouched.
                    let nl_col = line_len as i64;
                    let nl_seg = crate::fold::wrap_segment_of(nl_col, wrap_w, true);
                    let nl_x_page = if page_active { pager.x_page(nl_col) } else { 0 };
                    {
                        let row = if is_wrap_back { base_row } else { base_row + nl_seg };
                        if row != last_row || nl_seg != last_wrap_seg || nl_x_page != last_x_page {
                            last_row = row;
                            last_wrap_seg = nl_seg;
                            last_x_page = nl_x_page;
                            if page_active {
                                let f = pager.frame(row, nl_col, nl_seg);
                                cached_page_x_off = f.x_off;
                                cached_py = f.y;
                                cached_pz = f.z;
                            } else {
                                cached_page_x_off = 0.0;
                                cached_py = (-(row as f64) * line_height + origin_y) as f32;
                                cached_pz = (-(nl_seg as f64) * z_step + origin_z) as f32;
                            }
                            if cached_py < page_bottom {
                                page_bottom = cached_py;
                            }
                            if cached_pz < page_z_min {
                                page_z_min = cached_pz;
                            }
                            if cached_pz > page_z_max {
                                page_z_max = cached_pz;
                            }
                        }
                    }
                    let nl_rel_x = if fold_unit > 0 {
                        if line_len.is_multiple_of(fold_unit as usize) { 0.0 } else { last_seg_adv as f64 }
                    } else {
                        line_adv
                    };
                    let nl_x = (nl_rel_x + origin_x) as f32;
                    let nl_x = if page_active { (nl_x as f64 + cached_page_x_off) as f32 } else { nl_x };
                    let right = nl_x + trie.fast_byte_table[b'\n' as usize].advance;
                    if right > page_right {
                        page_right = right;
                    }

                    record_idx += 1;
                    base_row += rows_for_line(col, wrap_w, p.wrap_mode);
                    col = 0;
                    line_adv = 0.0;
                    seg_adv = 0.0;
                    line_start_col = 0;
                    pos += 1;
                }
                continue;
            }
        }

        let r = match resolve_byte_char(bytes, pos, rctx, &mut trailer_until) {
            Some(r) => r,
            None => {
                pos += 1;
                continue;
            }
        };

        let wrap_segment = wrap_segment_of(col, wrap_w, r.is_newline);
        let row = if is_wrap_back {
            base_row
        } else {
            base_row + wrap_row_of(col, wrap_w, r.is_newline, p.wrap_mode)
        };

        let x_page = if page_active { pager.x_page(col) } else { 0 };

        if row != last_row || wrap_segment != last_wrap_seg || x_page != last_x_page {
            last_row = row;
            last_wrap_seg = wrap_segment;
            last_x_page = x_page;

            if page_active {
                let f = pager.frame(row, col, wrap_segment);
                cached_page_x_off = f.x_off;
                cached_py = f.y;
                cached_pz = f.z;
            } else {
                cached_page_x_off = 0.0;
                cached_py = (-(row as f64) * line_height + origin_y) as f32;
                cached_pz = (-(wrap_segment as f64) * z_step + origin_z) as f32;
            }

            if cached_py < page_bottom {
                page_bottom = cached_py;
            }
            if cached_pz < page_z_min {
                page_z_min = cached_pz;
            }
            if cached_pz > page_z_max {
                page_z_max = cached_pz;
            }
        }

        let item_rel_x = if fold_unit > 0 { seg_adv as f64 } else { line_adv };
        let base_x = (item_rel_x + origin_x) as f32;
        let pos_x = if page_active {
            (base_x as f64 + cached_page_x_off) as f32
        } else {
            base_x
        };
        let pos_y = cached_py;
        let pos_z = cached_pz;

        let right = pos_x + r.advance;
        if right > page_right {
            page_right = right;
        }

        let color = if let Some(c) = flat_color {
            c
        } else if is_syntax_heuristic {
            let col_in_line = (col - line_start_col) as usize;
            if col_in_line < line_colors.len() {
                line_colors[col_in_line]
            } else {
                crate::layout::DEFAULT_COLOR_PACKED
            }
        } else if let Some(colors) = per_record_colors {
            if record_idx < colors.len() {
                colors[record_idx]
            } else {
                0xFFFF_FFFF
            }
        } else if let Paint::ByteSpans(spans) = item.paint {
            let p = (chunk_byte_offset + pos) as u32;
            while span_idx < spans.len() && p >= spans[span_idx].end {
                span_idx += 1;
            }
            if span_idx < spans.len() && p >= spans[span_idx].start {
                spans[span_idx].color
            } else {
                crate::layout::DEFAULT_COLOR_PACKED
            }
        } else {
            crate::layout::DEFAULT_COLOR_PACKED
        };

        if r.glyph_id != 0 {
            let half = r.height * 0.5;
            if pos_x < ink_min[0] {
                ink_min[0] = pos_x;
            }
            if right > ink_max[0] {
                ink_max[0] = right;
            }
            if pos_y != last_ink_y || r.height != crate::text::CELL_HEIGHT_WORLD {
                if pos_y - half < ink_min[1] {
                    ink_min[1] = pos_y - half;
                }
                if pos_y + half > ink_max[1] {
                    ink_max[1] = pos_y + half;
                }
                last_ink_y = pos_y;
            }
            if pos_z != last_ink_z {
                if pos_z < ink_min[2] {
                    ink_min[2] = pos_z;
                }
                if pos_z > ink_max[2] {
                    ink_max[2] = pos_z;
                }
                last_ink_z = pos_z;
            }

            if trie.is_emoji_glyph(r.glyph_id) {
                file_has_emoji = true;
                emoji_cells += 1;
            } else if flat_color.is_none() {
                let c0 = (color & 0xFF) as usize;
                let c1 = ((color >> 8) & 0xFF) as usize;
                let c2 = ((color >> 16) & 0xFF) as usize;
                file_s0 += lut[c0];
                file_s1 += lut[c1];
                file_s2 += lut[c2];
                file_cells += 1;
            }

            if E::USES_LINES && row > max_row_seen {
                max_row_seen = row;
            }
            unsafe {
                out_ptr.add(survivor_out).write(E::emit(
                    pos_x,
                    pos_y,
                    pos_z,
                    r.glyph_id,
                    color,
                    group_id,
                    item_idx,
                    r.advance,
                    r.height,
                    row,
                    wrap_segment,
                    x_page,
                ));
            }
            survivor_out += 1;

            if has_blocks {
                let qw = r.advance.max(r.height);
                let half_h = 0.5 * r.height;
                if pos_x < cur_blk_min_x { cur_blk_min_x = pos_x; }
                let x_hi = pos_x + qw;
                if x_hi > cur_blk_max_x { cur_blk_max_x = x_hi; }
                let y_lo = pos_y - half_h;
                let y_hi = pos_y + half_h;
                if y_lo < cur_blk_min_y { cur_blk_min_y = y_lo; }
                if y_hi > cur_blk_max_y { cur_blk_max_y = y_hi; }
                if pos_z < cur_blk_min_z { cur_blk_min_z = pos_z; }
                if pos_z > cur_blk_max_z { cur_blk_max_z = pos_z; }
                cur_blk_count += 1;

                if cur_blk_count == SUBSEG_BLOCK_SIZE {
                    if cur_blk_min_x <= cur_blk_max_x && cur_blk_min_y <= cur_blk_max_y {
                        local_blocks.push(BlockCull {
                            min: [cur_blk_min_x, cur_blk_min_y, cur_blk_min_z],
                            max: [cur_blk_max_x, cur_blk_max_y, cur_blk_max_z],
                            slot_base: chunk_rel_slot + (survivor_out - SUBSEG_BLOCK_SIZE) as u32,
                            slot_count: SUBSEG_BLOCK_SIZE as u32,
                        });
                    }
                    cur_blk_min_x = f32::INFINITY;
                    cur_blk_min_y = f32::INFINITY;
                    cur_blk_min_z = f32::INFINITY;
                    cur_blk_max_x = f32::NEG_INFINITY;
                    cur_blk_max_y = f32::NEG_INFINITY;
                    cur_blk_max_z = f32::NEG_INFINITY;
                    cur_blk_count = 0;
                }
            }
        }

        record_idx += 1;

        if r.is_newline {
            base_row += rows_for_line(col, p.wrap_width as i64, p.wrap_mode);
            col = 0;
            line_adv = 0.0;
            seg_adv = 0.0;
            line_start_col = 0;
        } else {
            col += 1;
            line_adv += r.advance as f64;
            if fold_unit > 0 && col % fold_unit == 0 {
                seg_adv = 0.0;
            } else {
                seg_adv += r.advance;
            }
        }

        pos += 1;
    }

    if has_blocks
        && cur_blk_count > 0
        && cur_blk_min_x <= cur_blk_max_x
        && cur_blk_min_y <= cur_blk_max_y
    {
        local_blocks.push(BlockCull {
            min: [cur_blk_min_x, cur_blk_min_y, cur_blk_min_z],
            max: [cur_blk_max_x, cur_blk_max_y, cur_blk_max_z],
            slot_base: chunk_rel_slot + (survivor_out - cur_blk_count) as u32,
            slot_count: cur_blk_count as u32,
        });
    }

    if flat_color.is_none() && is_syntax_heuristic {
        file_s0 += file_syntax_counts[0] as f64 * tint_default[0]
                 + file_syntax_counts[1] as f64 * tint_keyword[0]
                 + file_syntax_counts[2] as f64 * tint_number[0]
                 + file_syntax_counts[3] as f64 * tint_string[0]
                 + file_syntax_counts[4] as f64 * tint_comment[0]
                 + file_syntax_counts[5] as f64 * tint_punct[0];
        file_s1 += file_syntax_counts[0] as f64 * tint_default[1]
                 + file_syntax_counts[1] as f64 * tint_keyword[1]
                 + file_syntax_counts[2] as f64 * tint_number[1]
                 + file_syntax_counts[3] as f64 * tint_string[1]
                 + file_syntax_counts[4] as f64 * tint_comment[1]
                 + file_syntax_counts[5] as f64 * tint_punct[1];
        file_s2 += file_syntax_counts[0] as f64 * tint_default[2]
                 + file_syntax_counts[1] as f64 * tint_keyword[2]
                 + file_syntax_counts[2] as f64 * tint_number[2]
                 + file_syntax_counts[3] as f64 * tint_string[2]
                 + file_syntax_counts[4] as f64 * tint_comment[2]
                 + file_syntax_counts[5] as f64 * tint_punct[2];
    }

    ChunkPass2Output {
        slot_count: survivor_out as u32,
        record_count: (record_idx - initial_record_base) as u32,
        page_right,
        page_bottom,
        page_z_min,
        page_z_max,
        ink_min,
        ink_max,
        file_s0,
        file_s1,
        file_s2,
        file_cells,
        file_has_emoji,
        emoji_cells,
        local_blocks,
        max_row_seen,
    }
}

pub(crate) fn layout_pass2_device<E: SlotEmit>(
    inputs: &super::device_alloc::EmitInputs<'_, '_>,
    dest_addr: usize,
) -> Pass2DeviceOutput {
    let cut_colors = pass2_cut_colors(inputs);
    let chunk_results = pass2_chunk_range::<E>(inputs, 0..inputs.chunks.len(), dest_addr, &cut_colors);
    pass2_merge::<E>(inputs, &chunk_results)
}

/// The whole-line colours every intra-line cut needs (C17), computed once
/// before any chunk of Pass 2 runs.
pub(crate) fn pass2_cut_colors(inputs: &super::device_alloc::EmitInputs<'_, '_>) -> Vec<ChunkCutColors> {
    whole_line_colors_at_cuts(inputs.chunks, inputs.items)
}

/// Pass 2 over the chunks in `range`, in parallel, each writing its slots at
/// `dest_addr + chunk_slot_base * size_of::<E::Slot>()`. A caller emitting a
/// WINDOW of the slot stream passes the window's address minus its first
/// slot's byte offset (C22). Results come back in chunk order.
pub(crate) fn pass2_chunk_range<E: SlotEmit>(
    inputs: &super::device_alloc::EmitInputs<'_, '_>,
    range: std::ops::Range<usize>,
    dest_addr: usize,
    cut_colors: &[ChunkCutColors],
) -> Vec<ChunkPass2Output> {
    let lut = crate::glyph_scene::srgb_to_linear_table();
    range
        .into_par_iter()
        .map(|chunk_idx| layout_pass2_chunk::<E>(inputs, chunk_idx, dest_addr, lut, &cut_colors[chunk_idx]))
        .collect()
}

/// Fold the per-chunk results into per-item placements, tints and blocks.
pub(crate) fn pass2_merge<E: SlotEmit>(
    inputs: &super::device_alloc::EmitInputs<'_, '_>,
    chunk_results: &[ChunkPass2Output],
) -> Pass2DeviceOutput {
    let lut = crate::glyph_scene::srgb_to_linear_table();
    let mut placements = Vec::with_capacity(inputs.items.len());
    let mut file_tints = Vec::with_capacity(inputs.items.len());
    let mut file_blocks = Vec::with_capacity(inputs.items.len());

    for (item_idx, range) in inputs.item_chunk_ranges.iter().enumerate() {
        let item_slot_base = inputs.slot_bases[item_idx];
        let mut total_slots = 0u32;
        let mut total_records = 0u32;
        let mut page_right = 0.0f32;
        let mut page_bottom = 0.0f32;
        let mut page_z_min = 0.0f32;
        let mut page_z_max = 0.0f32;
        let mut ink_min = [f32::INFINITY; 3];
        let mut ink_max = [f32::NEG_INFINITY; 3];
        let mut file_s0 = 0.0f64;
        let mut file_s1 = 0.0f64;
        let mut file_s2 = 0.0f64;
        let mut file_cells = 0usize;
        let mut file_has_emoji = false;
        let mut emoji_cells = 0usize;
        let mut item_blocks = Vec::new();
        let mut max_row_seen = -1i64;

        for chunk_idx in range.clone() {
            let cr = &chunk_results[chunk_idx];
            total_slots += cr.slot_count;
            total_records += cr.record_count;
            if cr.page_right > page_right { page_right = cr.page_right; }
            if cr.page_bottom < page_bottom { page_bottom = cr.page_bottom; }
            if cr.page_z_min < page_z_min { page_z_min = cr.page_z_min; }
            if cr.page_z_max > page_z_max { page_z_max = cr.page_z_max; }

            for d in 0..3 {
                if cr.ink_min[d] < ink_min[d] { ink_min[d] = cr.ink_min[d]; }
                if cr.ink_max[d] > ink_max[d] { ink_max[d] = cr.ink_max[d]; }
            }

            file_s0 += cr.file_s0;
            file_s1 += cr.file_s1;
            file_s2 += cr.file_s2;
            file_cells += cr.file_cells;
            if cr.file_has_emoji { file_has_emoji = true; }
            emoji_cells += cr.emoji_cells;

            item_blocks.extend_from_slice(&cr.local_blocks);
            if cr.max_row_seen > max_row_seen { max_row_seen = cr.max_row_seen; }
        }

        if let Paint::Flat(c) = inputs.items[item_idx].paint {
            let non_emoji = (total_slots as usize).saturating_sub(emoji_cells);
            if non_emoji > 0 {
                let c0 = (c & 0xFF) as usize;
                let c1 = ((c >> 8) & 0xFF) as usize;
                let c2 = ((c >> 16) & 0xFF) as usize;
                file_s0 = lut[c0] * non_emoji as f64;
                file_s1 = lut[c1] * non_emoji as f64;
                file_s2 = lut[c2] * non_emoji as f64;
                file_cells = non_emoji;
            }
        }

        // Pass 1 planned with its own reading of which items hold emoji; a
        // staging path captures tint pairs only for those (C22), so a
        // disagreement would lose an item's tint silently. Same test
        // (`TrieTable::is_emoji_glyph`), same resolution; it must agree.
        assert_eq!(
            file_has_emoji, inputs.prepasses[item_idx].has_emoji,
            "pass 1 and pass 2 disagree on whether item {item_idx} holds emoji",
        );

        if E::USES_LINES {
            assert!(
                max_row_seen < inputs.prepasses[item_idx].row_count as i64,
                "pass 1 counted {} rows for item {item_idx} but pass 2 emitted row {max_row_seen}",
                inputs.prepasses[item_idx].row_count,
            );
        }

        placements.push(ItemPlacement {
            slot_base: item_slot_base,
            slot_count: total_slots,
            record_count: total_records,
            page: PageExtent {
                right: page_right,
                bottom: page_bottom,
                z_min: page_z_min,
                z_max: page_z_max,
            },
            ink: InkExtent {
                min: ink_min,
                max: ink_max,
            },
        });
        file_tints.push(FileTintAccum {
            sum: [file_s0, file_s1, file_s2],
            cells: file_cells,
            has_emoji: file_has_emoji,
        });
        file_blocks.push(item_blocks);
    }

    Pass2DeviceOutput {
        placements,
        file_tints,
        file_blocks,
    }
}

/// One window of the slot stream (C22): whole chunks, so contiguous slots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SlotWindow {
    pub chunks: std::ops::Range<usize>,
    pub slots: std::ops::Range<usize>,
}

/// Cut the slot stream into windows of WHOLE chunks, each at most
/// `window_slots` slots — unless one chunk alone is bigger, which then gets
/// a window of its own. Chunk `i` owns slots `chunk_slot_bases[i] ..` the
/// next chunk's base (Pass 1 lays them out in chunk order), so a window is
/// a contiguous slot range and its address is known before Pass 2 runs.
pub(crate) fn plan_windows(chunk_slot_bases: &[u32], total_slots: usize, window_slots: usize) -> Vec<SlotWindow> {
    let n = chunk_slot_bases.len();
    let end_of = |i: usize| if i + 1 < n { chunk_slot_bases[i + 1] as usize } else { total_slots };
    let mut windows = Vec::new();
    let mut i = 0;
    while i < n {
        let start = chunk_slot_bases[i] as usize;
        let mut j = i + 1;
        while j < n && end_of(j) - start <= window_slots {
            j += 1;
        }
        windows.push(SlotWindow { chunks: i..j, slots: start..end_of(j - 1) });
        i = j;
    }
    windows
}

/// Where each window's slots are written (C22). `begin` returns the address
/// of writable memory for window `k` (its first slot at offset 0, room for
/// `window.slots.len()` slots); `end` is called once the window is fully
/// written, to hand it on. The GPU sink is a pair of mapped staging
/// buffers; the unit test's is host memory.
pub(crate) trait WindowSink {
    fn begin(&mut self, k: usize, window: &SlotWindow) -> usize;
    fn end(&mut self, k: usize, window: &SlotWindow);
}

/// Pass 2 window by window into `sink`, returning what `EmitInputs::run`
/// returns: the merged output and each item's `(glyph, colour)` tint pairs.
///
/// A window's chunks run in parallel and write straight into the sink's
/// memory, which on a discrete GPU is write-combined: fast to write, very
/// slow to read back (an emoji tint re-read cost ~200 ms there). So the
/// chunks of items Pass 1 flagged `has_emoji` are emitted into host scratch
/// first, their pairs taken from cached memory, then copied into the window.
/// Emoji items are rare; every other chunk writes once, in place.
pub(crate) fn emit_windows<E: SlotEmit>(
    inputs: &super::device_alloc::EmitInputs<'_, '_>,
    windows: &[SlotWindow],
    total_slots: usize,
    sink: &mut impl WindowSink,
) -> (Pass2DeviceOutput, Vec<Vec<u32>>) {
    let lut = crate::glyph_scene::srgb_to_linear_table();
    let cut_colors = pass2_cut_colors(inputs);
    let n = inputs.chunks.len();
    let slot_end = |c: usize| if c + 1 < n { inputs.chunk_slot_bases[c + 1] as usize } else { total_slots };
    let size = std::mem::size_of::<E::Slot>();
    let mut results: Vec<ChunkPass2Output> = Vec::with_capacity(n);
    let mut chunk_pairs: Vec<Vec<u32>> = Vec::with_capacity(n);
    for (k, w) in windows.iter().enumerate() {
        let window_addr = sink.begin(k, w);
        // Chunk c writes at dest_addr + base(c) * size; the window holds
        // slot `w.slots.start` at offset 0.
        let dest_addr = window_addr.wrapping_sub(w.slots.start * size);
        let done: Vec<(ChunkPass2Output, Vec<u32>)> = w
            .chunks
            .clone()
            .into_par_iter()
            .map(|c| {
                let item = inputs.chunks[c].item_index;
                if !inputs.prepasses[item].has_emoji {
                    return (layout_pass2_chunk::<E>(inputs, c, dest_addr, lut, &cut_colors[c]), Vec::new());
                }
                let base = inputs.chunk_slot_bases[c] as usize;
                let count = slot_end(c) - base;
                let mut scratch: Vec<std::mem::MaybeUninit<E::Slot>> = Vec::with_capacity(count.max(1));
                let scratch_addr = scratch.as_mut_ptr() as usize;
                let out = layout_pass2_chunk::<E>(inputs, c, scratch_addr.wrapping_sub(base * size), lut, &cut_colors[c]);
                assert_eq!(out.slot_count as usize, count, "chunk {c} emitted {} of its {count} slots", out.slot_count);
                // SAFETY: the chunk wrote exactly `count` slots into scratch
                // (asserted), and the window has room for them at `base`.
                let slots = unsafe { std::slice::from_raw_parts(scratch_addr as *const E::Slot, count) };
                let mut pairs = Vec::with_capacity(count * 2);
                for slot in slots {
                    pairs.extend_from_slice(&E::tint_pair(slot));
                }
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        scratch_addr as *const u8,
                        (dest_addr + base * size) as *mut u8,
                        count * size,
                    );
                }
                (out, pairs)
            })
            .collect();
        sink.end(k, w);
        for (out, pairs) in done {
            results.push(out);
            chunk_pairs.push(pairs);
        }
    }
    assert_eq!(results.len(), n, "windows must cover every chunk exactly once");
    let out = pass2_merge::<E>(inputs, &results);
    let pairs = inputs
        .item_chunk_ranges
        .iter()
        .enumerate()
        .map(|(item, range)| {
            if !inputs.prepasses[item].has_emoji {
                return Vec::new();
            }
            range.clone().flat_map(|c| chunk_pairs[c].iter().copied()).collect()
        })
        .collect();
    (out, pairs)
}

/// `(glyph_id, color)` pairs for every item whose fast tint cannot stand
/// alone (`has_emoji`: its bitmap slots fold the atlas's per-slot ink, which
/// Pass 2 does not have). Read from the slots just written, while the
/// destination is still host-visible, so no consumer ever has to read the
/// device buffer back. Emoji items are rare; every other entry is empty.
pub(crate) fn emoji_tint_pairs<E: SlotEmit>(
    dest_addr: usize,
    out: &Pass2DeviceOutput,
) -> Vec<Vec<u32>> {
    out.placements
        .par_iter()
        .zip(out.file_tints.par_iter())
        .map(|(pl, t)| {
            if !t.has_emoji {
                return Vec::new();
            }
            let base = dest_addr as *const E::Slot;
            let slots = unsafe {
                std::slice::from_raw_parts(base.add(pl.slot_base as usize), pl.slot_count as usize)
            };
            let mut pairs = Vec::with_capacity(slots.len() * 2);
            for s in slots {
                pairs.extend_from_slice(&E::tint_pair(s));
            }
            pairs
        })
        .collect()
}
