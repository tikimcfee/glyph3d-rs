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
    pub item_idx: u32,
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
    fn emit(f: &EmitFields, _line_base: u32) -> DerivedSlot {
        // Saturate like the host transcode's float->int cast did: a segment
        // past 65535 pins to the last step rather than wrapping to the front.
        let wrap = f.wrap_segment.clamp(0, u16::MAX as i64) as u16;
        DerivedSlot::new(
            f.pos[0],
            f.row as u32,
            (f.glyph_id & 0xFFFF) as u16,
            wrap,
            f.color,
            (f.item_idx & 0xFFFF) as u16,
            (f.group_id & 0xFFFF) as u16,
        )
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

#[allow(clippy::too_many_arguments)]
fn layout_pass2_chunk<E: SlotEmit>(
    chunk_bytes: &[u8],
    chunk_byte_offset: usize,
    item: &LayoutItem<'_>,
    item_idx: u32,
    pre: &ItemPrepass,
    slot_base: u32,
    item_slot_base: u32,
    initial_base_row: i64,
    initial_record_base: usize,
    initial_col: i64,
    initial_seg_adv: f32,
    initial_line_adv: f64,
    line_base: u32,
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
    dest_addr: usize,
    lut: &[f64; 256],
) -> ChunkPass2Output {
    let bytes = chunk_bytes;
    let p = &item.params;
    let group_id = item.group_id;
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
    if is_syntax_heuristic && initial_col > 0 {
        let first_nl = match memchr::memchr(b'\n', bytes) {
            Some(off) => off,
            None => bytes.len(),
        };
        let first_slice = &bytes[..first_nl];
        if first_slice.iter().all(|&b| (0x20..=0x7E).contains(&b)) {
            crate::text::colorize_pure_ascii_line(first_slice, &mut line_colors);
        } else {
            crate::text::colorize_line_into(first_slice, &mut line_colors);
        }
    }
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
            let is_pure_ascii = line_bytes.iter().all(|&b| (0x20..=0x7E).contains(&b));

            if is_syntax_heuristic {
                if is_pure_ascii {
                    crate::text::colorize_pure_ascii_line(line_bytes, &mut line_colors);
                } else {
                    crate::text::colorize_line_into(line_bytes, &mut line_colors);
                }
            }

            // Line-level ASCII fast path:
            if !matches!(item.paint, Paint::ByteSpans(_))
                && is_pure_ascii
                && (wrap_w == 0 || line_len <= wrap_w as usize)
            {
                let row = base_row;
                let wrap_segment = 0i64;
                let x_page = 0i64;

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

                let row_py = cached_py;
                let row_pz = cached_pz;
                let start_survivors = survivor_out;
                let mut line_adv_f64 = line_adv;
                let mut seg_adv_f32 = seg_adv;

                for (char_idx, &b) in line_bytes.iter().enumerate() {
                    let item_rel_x = if fold_unit > 0 { seg_adv_f32 as f64 } else { line_adv_f64 };
                    let base_x = (item_rel_x + origin_x) as f32;
                    let pos_x = if page_active {
                        (base_x as f64 + cached_page_x_off) as f32
                    } else {
                        base_x
                    };
                    let right = pos_x + ascii_adv;
                    if right > page_right {
                        page_right = right;
                    }
                    line_adv_f64 += ascii_adv as f64;
                    seg_adv_f32 += ascii_adv;

                    let glyph_id = trie.fast_byte_table[b as usize].glyph_id;
                    if glyph_id != 0 {
                        if pos_x < ink_min[0] {
                            ink_min[0] = pos_x;
                        }
                        if right > ink_max[0] {
                            ink_max[0] = right;
                        }

                        let color = if let Some(c) = flat_color {
                            c
                        } else if is_syntax_heuristic {
                            if char_idx < line_colors.len() {
                                line_colors[char_idx]
                            } else {
                                crate::layout::DEFAULT_COLOR_PACKED
                            }
                        } else if let Some(colors) = per_record_colors {
                            if record_idx + char_idx < colors.len() {
                                colors[record_idx + char_idx]
                            } else {
                                0xFFFF_FFFF
                            }
                        } else {
                            crate::layout::DEFAULT_COLOR_PACKED
                        };

                        if flat_color.is_none() {
                            let c0 = (color & 0xFF) as usize;
                            let c1 = ((color >> 8) & 0xFF) as usize;
                            let c2 = ((color >> 16) & 0xFF) as usize;
                            file_s0 += lut[c0];
                            file_s1 += lut[c1];
                            file_s2 += lut[c2];
                            file_cells += 1;
                        }

                        let fields = EmitFields {
                            pos: [pos_x, row_py, row_pz],
                            glyph_id,
                            color,
                            group_id,
                            item_idx,
                            advance: ascii_adv,
                            height: crate::text::CELL_HEIGHT_WORLD,
                            row,
                            wrap_segment: 0,
                        };
                        unsafe {
                            out_ptr.add(survivor_out).write(E::emit(&fields, line_base));
                        }
                        survivor_out += 1;

                        if has_blocks {
                            let qw = ascii_adv.max(crate::text::CELL_HEIGHT_WORLD);
                            let half_h = 0.5 * crate::text::CELL_HEIGHT_WORLD;
                            if pos_x < cur_blk_min_x { cur_blk_min_x = pos_x; }
                            let x_hi = pos_x + qw;
                            if x_hi > cur_blk_max_x { cur_blk_max_x = x_hi; }
                            let y_lo = row_py - half_h;
                            let y_hi = row_py + half_h;
                            if y_lo < cur_blk_min_y { cur_blk_min_y = y_lo; }
                            if y_hi > cur_blk_max_y { cur_blk_max_y = y_hi; }
                            if row_pz < cur_blk_min_z { cur_blk_min_z = row_pz; }
                            if row_pz > cur_blk_max_z { cur_blk_max_z = row_pz; }
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

                record_idx += line_len;
                col = line_len as i64;
                line_adv = line_adv_f64;
                seg_adv = seg_adv_f32;
                pos = line_end;

                if pos < bytes.len() && bytes[pos] == b'\n' {
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

            let gid = r.glyph_id as usize;
            if gid < trie.emoji_cell.len() && trie.emoji_cell[gid].is_some() {
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
            let fields = EmitFields {
                pos: [pos_x, pos_y, pos_z],
                glyph_id: r.glyph_id,
                color,
                group_id,
                item_idx,
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
    debug_assert!(!E::USES_LINES || inputs.line_bases.len() == inputs.items.len());
    let lut = crate::glyph_scene::srgb_to_linear_table();

    let chunk_results: Vec<ChunkPass2Output> = inputs
        .chunks
        .par_iter()
        .enumerate()
        .map(|(chunk_idx, chunk)| {
            let item_idx = chunk.item_index;
            let item = &inputs.items[item_idx];
            let item_prepass = &inputs.prepasses[item_idx];
            let slot_base = inputs.chunk_slot_bases[chunk_idx];
            let item_slot_base = inputs.slot_bases[item_idx];
            let initial_base_row = inputs.chunk_base_rows[chunk_idx];
            let initial_record_base = inputs.chunk_record_bases[chunk_idx];
            let initial_col = inputs.chunk_initial_cols[chunk_idx];
            let initial_seg_adv = inputs.chunk_initial_seg_advs[chunk_idx];
            let initial_line_adv = inputs.chunk_initial_line_advs[chunk_idx];
            let line_base = if E::USES_LINES { inputs.line_bases[item_idx] } else { 0 };

            layout_pass2_chunk::<E>(
                chunk.bytes,
                chunk.byte_offset,
                item,
                item_idx as u32,
                item_prepass,
                slot_base,
                item_slot_base,
                initial_base_row,
                initial_record_base,
                initial_col,
                initial_seg_adv,
                initial_line_adv,
                line_base,
                inputs.trie,
                inputs.bitmap_adv,
                inputs.em_height_fu,
                dest_addr,
                lut,
            )
        })
        .collect();

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
