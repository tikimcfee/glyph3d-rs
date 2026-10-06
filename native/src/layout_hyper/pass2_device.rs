//! Pass 2 parallel layout worker for device-mapped / VRAM slot buffers.
//!
//! One walk, two slot formats. The per-glyph math (fold, wrap, pagination,
//! extents, tint, cull blocks) is written once; what a survivor becomes in
//! memory is the [`SlotEmit`] parameter, monomorphized per format so neither
//! format pays a branch for the other:
//!
//! - [`RenderEmit`] — the Instanced field's 32 B `RenderSlot`.
//! - [`DerivedEmit`] — the Derived field's 20 B `DerivedSlot`, whose Y/Z the
//!   vertex stage re-derives from `line_table[line_base + row]` and the
//!   slot's wrap segment.

use rayon::prelude::*;
use crate::atlas::TrieTable;
use crate::fold::{rows_for_line, wrap_row_of, wrap_segment_of};
use crate::glyph_scene::{BlockCull, RenderSlot, SUBSEG_BLOCK_SIZE};
use crate::layout::{FileTintAccum, InkExtent, ItemPlacement, LayoutItem, PageExtent, Paint};
use glyph_field_derived::DerivedSlot;
use super::char_resolve::resolve_byte_char;
use super::types::{ItemPrepass, Pass2DeviceOutput};

/// Everything Pass 2 knows about one survivor at the moment it is emitted.
#[derive(Clone, Copy)]
pub(crate) struct EmitFields {
    pub pos: [f32; 3],
    pub glyph_id: u32,
    pub color: u32,
    pub group_id: u32,
    pub advance: f32,
    pub height: f32,
    /// Item-local row (WrapDown rows included) — the Derived line key.
    pub row: i64,
    /// Fold count along the line — the Derived Z step index.
    pub wrap_segment: i64,
}

/// A device slot format Pass 2 can emit directly.
pub(crate) trait SlotEmit: Sync {
    type Slot: Copy + Send + Sync;
    /// Whether this format indexes a line table (and so needs `line_bases`
    /// and the row bound checked).
    const USES_LINES: bool;
    fn emit(f: &EmitFields, line_base: u32) -> Self::Slot;
    /// `(glyph_id, color)` — the tint fold's view of a slot.
    fn tint_pair(s: &Self::Slot) -> [u32; 2];
}

pub(crate) struct RenderEmit;

impl SlotEmit for RenderEmit {
    type Slot = RenderSlot;
    const USES_LINES: bool = false;
    #[inline(always)]
    fn emit(f: &EmitFields, _line_base: u32) -> RenderSlot {
        RenderSlot {
            pos: f.pos,
            glyph_id: f.glyph_id,
            color: f.color,
            group_id: f.group_id,
            advance: f.advance,
            height: f.height,
        }
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
    fn emit(f: &EmitFields, line_base: u32) -> DerivedSlot {
        // Saturate like the host transcode's float->int cast did: a segment
        // past 65535 pins to the last step rather than wrapping to the front.
        let wrap = f.wrap_segment.clamp(0, u16::MAX as i64) as u16;
        DerivedSlot::new(
            f.pos[0],
            line_base + f.row as u32,
            (f.glyph_id & 0xFFFF) as u16,
            wrap,
            f.color,
            f.group_id,
        )
    }
    #[inline(always)]
    fn tint_pair(s: &DerivedSlot) -> [u32; 2] {
        [s.glyph_id() as u32, s.color]
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_pass2_device<E: SlotEmit>(
    items: &[LayoutItem<'_>],
    prepasses: &[ItemPrepass],
    slot_bases: &[u32],
    line_bases: &[u32],
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
    dest_addr: usize,
) -> Pass2DeviceOutput {
    debug_assert!(!E::USES_LINES || line_bases.len() == items.len());
    let lut = crate::glyph_scene::srgb_to_linear_table();
    let results: Vec<_> = items
        .par_iter()
        .zip(prepasses.par_iter())
        .zip(slot_bases.par_iter())
        .enumerate()
        .map(|(item_idx, ((item, pre), &slot_base))| {
            let bytes = item.bytes;
            let p = &item.params;
            let group_id = item.group_id;
            let line_base = if E::USES_LINES { line_bases[item_idx] } else { 0 };
            let mut max_row_seen = -1i64;

            let fold_unit = if p.wrap_width > 0 {
                p.wrap_width as i64
            } else if p.has_page {
                p.page_cols as i64
            } else {
                0
            };
            let page_stride_x = if p.has_page && p.page_rows > 0 {
                pre.max_row_extent + p.page_gap_x
            } else {
                0.0
            };
            let page_active = p.has_page && (p.page_rows > 0 || p.page_cols > 0 || p.scroll_rows > 0);

            let mut page_right = 0.0f32;
            let mut page_bottom = 0.0f32;
            let mut page_z_min = 0.0f32;
            let mut page_z_max = 0.0f32;

            let mut ink_min = [f32::INFINITY; 3];
            let mut ink_max = [f32::NEG_INFINITY; 3];

            let mut base_row = 0i64;
            let mut col = 0i64;
            let mut line_adv = 0.0f64;
            let mut seg_adv = 0.0f32;
            let mut record_idx = 0usize;
            let mut survivor_out = 0usize;
            let mut trailer_until = 0usize;

            let out_ptr = unsafe { (dest_addr as *mut E::Slot).add(slot_base as usize) };
            let flat_color = if let Paint::Flat(c) = item.paint { Some(c) } else { None };
            let (local_colors_buf, per_record_colors) = match item.paint {
                Paint::PerRecord(c) => (None, Some(c)),
                Paint::SyntaxHeuristic => {
                    let cols = crate::text::colorize_leaders(bytes);
                    (Some(cols), None)
                }
                _ => (None, None),
            };
            let colors_slice = per_record_colors.or(local_colors_buf.as_deref());
            let mut file_s0 = 0.0f64;
            let mut file_s1 = 0.0f64;
            let mut file_s2 = 0.0f64;
            let mut file_cells = 0usize;
            let mut file_has_emoji = false;

            let wrap_w = p.wrap_width as i64;
            let is_wrap_back = p.wrap_mode == crate::fold::WrapMode::Back;
            let pages_wide = (p.pages_wide as i64).max(1);
            let page_rows = p.page_rows as i64;
            let page_cols = p.page_cols as i64;
            let scroll_rows = p.scroll_rows as i64;
            let line_height = p.line_height;
            let band_stride_y = p.band_stride_y;
            let depth_per_band = p.depth_per_band;
            let depth_per_col = p.depth_per_col;
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

            let mut last_row = i64::MIN;
            let mut last_wrap_seg = i64::MIN;
            let mut last_x_page = i64::MIN;
            let mut cached_page_x_off = 0.0f64;
            let mut cached_py = 0.0f32;
            let mut cached_pz = 0.0f32;

            let mut pos = 0usize;
            let mut span_idx = 0usize;
            while pos < bytes.len() {
                let r = match resolve_byte_char(bytes, pos, trie, bitmap_adv, em_height_fu, &mut trailer_until) {
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

                let x_page = if page_active && page_cols > 0 { col / page_cols } else { 0 };

                if row != last_row || wrap_segment != last_wrap_seg || x_page != last_x_page {
                    last_row = row;
                    last_wrap_seg = wrap_segment;
                    last_x_page = x_page;

                    if page_active {
                        let (y_page, screen_row) = if page_rows > 0 {
                            (row / page_rows, row + scroll_rows)
                        } else {
                            (0, row)
                        };
                        let band = y_page / pages_wide;
                        cached_page_x_off = (y_page % pages_wide) as f64 * page_stride_x;
                        cached_py = (origin_y
                            - (screen_row - y_page * page_rows) as f64 * line_height
                            - band as f64 * band_stride_y) as f32;
                        cached_pz = (origin_z - wrap_segment as f64 * z_step
                            + band as f64 * depth_per_band
                            + x_page as f64 * depth_per_col) as f32;
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
                } else if let Some(colors) = colors_slice {
                    if record_idx < colors.len() {
                        colors[record_idx]
                    } else {
                        0xFFFF_FFFF
                    }
                } else if let Paint::ByteSpans(spans) = item.paint {
                    let p = pos as u32;
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
                    if pos_y - half < ink_min[1] {
                        ink_min[1] = pos_y - half;
                    }
                    if pos_y + half > ink_max[1] {
                        ink_max[1] = pos_y + half;
                    }
                    if pos_z < ink_min[2] {
                        ink_min[2] = pos_z;
                    }
                    if pos_z > ink_max[2] {
                        ink_max[2] = pos_z;
                    }

                    let gid = r.glyph_id as usize;
                    if gid < trie.emoji_cell.len() && trie.emoji_cell[gid].is_some() {
                        file_has_emoji = true;
                    } else if flat_color.is_some() {
                        file_cells += 1;
                    } else {
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
                    let fields = EmitFields {
                        pos: [pos_x, pos_y, pos_z],
                        glyph_id: r.glyph_id,
                        color,
                        group_id,
                        advance: r.advance,
                        height: r.height,
                        row,
                        wrap_segment,
                    };
                    unsafe {
                        out_ptr.add(survivor_out).write(E::emit(&fields, line_base));
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
                                    slot_base: (survivor_out - SUBSEG_BLOCK_SIZE) as u32,
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

            // The line table reserved `row_count` entries for this item before
            // the walk; a row at or past it would index the NEXT item's lines
            // and draw this glyph at that item's Y. Loud, not silent.
            if E::USES_LINES {
                assert!(
                    max_row_seen < pre.row_count as i64,
                    "pass 1 counted {} rows for item {item_idx} but pass 2 emitted row {max_row_seen}",
                    pre.row_count,
                );
            }

            if has_blocks
                && cur_blk_count > 0
                && cur_blk_min_x <= cur_blk_max_x
                && cur_blk_min_y <= cur_blk_max_y
            {
                local_blocks.push(BlockCull {
                    min: [cur_blk_min_x, cur_blk_min_y, cur_blk_min_z],
                    max: [cur_blk_max_x, cur_blk_max_y, cur_blk_max_z],
                    slot_base: (survivor_out - cur_blk_count) as u32,
                    slot_count: cur_blk_count as u32,
                });
            }

            if let Some(c) = flat_color {
                if file_cells > 0 {
                    let c0 = (c & 0xFF) as usize;
                    let c1 = ((c >> 8) & 0xFF) as usize;
                    let c2 = ((c >> 16) & 0xFF) as usize;
                    file_s0 = lut[c0] * file_cells as f64;
                    file_s1 = lut[c1] * file_cells as f64;
                    file_s2 = lut[c2] * file_cells as f64;
                }
            }

            (
                ItemPlacement {
                    slot_base,
                    slot_count: survivor_out as u32,
                    record_count: record_idx as u32,
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
                },
                FileTintAccum {
                    sum: [file_s0, file_s1, file_s2],
                    cells: file_cells,
                    has_emoji: file_has_emoji,
                },
                local_blocks,
            )
        })
        .collect();

    let mut placements = Vec::with_capacity(results.len());
    let mut file_tints = Vec::with_capacity(results.len());
    let mut file_blocks = Vec::with_capacity(results.len());
    for (p, t, b) in results {
        placements.push(p);
        file_tints.push(t);
        file_blocks.push(b);
    }

    Pass2DeviceOutput {
        placements,
        file_tints,
        file_blocks,
    }
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
