//! Pass 2 parallel layout worker for host memory arena glyph instances.

use rayon::prelude::*;
use crate::atlas::TrieTable;
use crate::fold::{rows_for_line, wrap_row_of, wrap_segment_of};
use crate::glyph_scene::GlyphInstance;
use crate::layout::{InkExtent, ItemPlacement, LayoutItem, PageExtent, Paint};
use super::char_resolve::{resolve_byte_char, resolve_byte_char_cluster};
use super::types::{ItemPrepass, SendPtr};

pub fn layout_pass2_host(
    items: &[LayoutItem<'_>],
    prepasses: &[ItemPrepass],
    slot_bases: &[u32],
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
    dest: SendPtr<GlyphInstance>,
) -> Vec<ItemPlacement> {
    let dest_addr = dest.0 as usize;
    items
        .par_iter()
        .zip(prepasses.par_iter())
        .zip(slot_bases.par_iter())
        .map(|((item, pre), &slot_base)| {
            let bytes = item.bytes;
            let p = &item.params;
            let cluster = super::char_resolve::clusters(p);
            let group_id = item.group_id;

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

            let out_ptr = unsafe { (dest_addr as *mut GlyphInstance).add(slot_base as usize) };
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

            let mut pos = 0usize;
            let mut span_idx = 0usize;
            while pos < bytes.len() {
                let r = match resolve_byte_char(bytes, pos, trie, bitmap_adv, em_height_fu, cluster, &mut trailer_until) {
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

                let item_rel_x = if fold_unit > 0 { seg_adv as f64 } else { line_adv };
                let base_x = (item_rel_x + origin_x) as f32;
                let base_y = (-(row as f64) * line_height + origin_y) as f32;
                let base_z = (-(wrap_segment as f64) * z_step + origin_z) as f32;

                let (pos_x, pos_y, pos_z) = if page_active {
                    let (y_page, x_page, screen_row) = if page_rows > 0 {
                        let y_page = row / page_rows;
                        let screen_row = row + scroll_rows;
                        let x_page = if page_cols > 0 {
                            col / page_cols
                        } else {
                            0
                        };
                        (y_page, x_page, screen_row)
                    } else {
                        (0, 0, row)
                    };
                    let band = y_page / pages_wide;
                    let px = (base_x as f64 + (y_page % pages_wide) as f64 * page_stride_x) as f32;
                    let py = (origin_y
                        - (screen_row - y_page * page_rows) as f64 * line_height
                        - band as f64 * band_stride_y) as f32;
                    let pz = (origin_z - wrap_segment as f64 * z_step
                        + band as f64 * depth_per_band
                        + x_page as f64 * depth_per_col) as f32;
                    (px, py, pz)
                } else {
                    (base_x, base_y, base_z)
                };

                let right = pos_x + r.advance;
                if right > page_right {
                    page_right = right;
                }
                if pos_y < page_bottom {
                    page_bottom = pos_y;
                }
                if pos_z < page_z_min {
                    page_z_min = pos_z;
                }
                if pos_z > page_z_max {
                    page_z_max = pos_z;
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

                    unsafe {
                        *out_ptr.add(survivor_out) = GlyphInstance {
                            pos: [pos_x, pos_y, pos_z],
                            glyph_id: r.glyph_id,
                            row: row as u32,
                            col: col as u32,
                            color,
                            group_id,
                            advance: r.advance,
                            height: r.height,
                            flags: 0,
                            _pad: 0,
                        };
                    }
                    survivor_out += 1;
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
            }
        })
        .collect()
}

/// Computes bit-exact ItemPlacement for a single item from its parameters and bytes.
pub fn compute_single_item_placement(
    p: &crate::layout::ItemParams,
    bytes: &[u8],
    slot_base: u32,
    max_row_extent: f64,
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
) -> (ItemPlacement, f32, bool, bool) {
    let fold_unit = if p.wrap_width > 0 {
        p.wrap_width as i64
    } else if p.has_page {
        p.page_cols as i64
    } else {
        0
    };
    let page_stride_x = if p.has_page && p.page_rows > 0 {
        max_row_extent + p.page_gap_x
    } else {
        0.0
    };
    let page_active = p.has_page && (p.page_rows > 0 || p.page_cols > 0 || p.scroll_rows > 0);

    let mut page_right = 0.0f32;
    let mut page_bottom = 0.0f32;
    let mut page_z_min = 0.0f32;
    let mut page_z_max = 0.0f32;
    let mut measured_max_row_extent = 0.0f32;
    let mut is_multi_page = false;
    let mut has_cluster = false;
    let cluster = super::char_resolve::clusters(p);

    let mut ink_min = [f32::INFINITY; 3];
    let mut ink_max = [f32::NEG_INFINITY; 3];

    let mut base_row = 0i64;
    let mut col = 0i64;
    let mut line_adv = 0.0f64;
    let mut seg_adv = 0.0f32;
    let mut record_idx = 0usize;
    let mut survivor_out = 0usize;
    let mut trailer_until = 0usize;

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

    let cell_advance = crate::text::fu_to_world(trie.metrics.advance_fu as i32, em_height_fu);
    let cell_height = crate::text::fu_to_world(trie.metrics.em_height_fu as i32, em_height_fu);
    let half = cell_height * 0.5;
    let mut seg_adv_stack = [0.0f32; 256];
    let mut seg_adv_heap = Vec::new();
    let fold_u = if fold_unit > 0 { fold_unit as usize } else { 0 };
    let seg_adv_table: &[f32] = if fold_u < 256 {
        let mut cur = 0.0f32;
        for slot in seg_adv_stack.iter_mut().take(fold_u + 1) {
            *slot = cur;
            cur += cell_advance;
        }
        &seg_adv_stack[..=fold_u]
    } else {
        seg_adv_heap.reserve(fold_u + 1);
        let mut cur = 0.0f32;
        for _ in 0..=fold_u {
            seg_adv_heap.push(cur);
            cur += cell_advance;
        }
        &seg_adv_heap
    };
    let wrap_w_u = if fold_unit > 0 && wrap_w > 0 { wrap_w as usize } else { usize::MAX };

    let mut pos = 0usize;
    while pos < bytes.len() {
        let nl_pos = match memchr::memchr(b'\n', &bytes[pos..]) {
            Some(offset) => pos + offset,
            None => bytes.len(),
        };
        let line = &bytes[pos..nl_pos];
        if col == 0 && page_cols == 0 && pos >= trailer_until && line.iter().all(|b| (0x20..0x7F).contains(b)) {
            let l = line.len();
            if l == 0 {
                if nl_pos < bytes.len() {
                    let wrap_segment = 0i64;
                    let row = base_row;
                    let base_x = origin_x as f32;
                    let (pos_x, pos_y, pos_z) = if page_active {
                        let y_page = if page_rows > 0 { row / page_rows } else { 0 };
                        let screen_row = row + scroll_rows;
                        let band = y_page / pages_wide;
                        let px = (base_x as f64 + (y_page % pages_wide) as f64 * page_stride_x) as f32;
                        let py = (origin_y
                            - (screen_row - y_page * page_rows) as f64 * line_height
                            - band as f64 * band_stride_y) as f32;
                        let pz = (origin_z - wrap_segment as f64 * z_step
                            + band as f64 * depth_per_band) as f32;
                        (px, py, pz)
                    } else {
                        let base_y = (-(row as f64) * line_height + origin_y) as f32;
                        let base_z = (-(wrap_segment as f64) * z_step + origin_z) as f32;
                        (base_x, base_y, base_z)
                    };
                    if page_active && page_rows > 0 && pages_wide > 1 && row >= page_rows {
                        is_multi_page = true;
                    }
                    if pos_x > page_right { page_right = pos_x; }
                    if pos_y < page_bottom { page_bottom = pos_y; }
                    if pos_z < page_z_min { page_z_min = pos_z; }
                    if pos_z > page_z_max { page_z_max = pos_z; }

                    record_idx += 1;
                    base_row += rows_for_line(0, wrap_w, p.wrap_mode);
                    pos = nl_pos + 1;
                } else {
                    pos = nl_pos;
                }
                continue;
            }

            // l > 0: process each segment in the line
            let mut seg_start = 0usize;
            let mut s = 0i64;
            while seg_start < l {
                let seg_len = (l - seg_start).min(wrap_w_u);
                let wrap_segment = s;
                let row = if is_wrap_back {
                    base_row
                } else {
                    base_row + s
                };
                if page_active && page_rows > 0 && pages_wide > 1 && row >= page_rows {
                    is_multi_page = true;
                }
                let (py, pz, y_page_mod) = if page_active {
                    let y_page = if page_rows > 0 { row / page_rows } else { 0 };
                    let screen_row = row + scroll_rows;
                    let band = y_page / pages_wide;
                    let py = (origin_y
                        - (screen_row - y_page * page_rows) as f64 * line_height
                        - band as f64 * band_stride_y) as f32;
                    let pz = (origin_z - wrap_segment as f64 * z_step
                        + band as f64 * depth_per_band) as f32;
                    (py, pz, y_page % pages_wide)
                } else {
                    let base_y = (-(row as f64) * line_height + origin_y) as f32;
                    let base_z = (-(wrap_segment as f64) * z_step + origin_z) as f32;
                    (base_y, base_z, 0)
                };

                let pos_x_0 = if page_active {
                    ((origin_x as f32) as f64 + y_page_mod as f64 * page_stride_x) as f32
                } else {
                    origin_x as f32
                };
                let last_item_rel_x = if fold_u > 0 { seg_adv_table[seg_len - 1] as f64 } else { (seg_len - 1) as f64 * cell_advance as f64 };
                let last_base_x = (last_item_rel_x + origin_x) as f32;
                let pos_x_last = if page_active {
                    (last_base_x as f64 + y_page_mod as f64 * page_stride_x) as f32
                } else {
                    last_base_x
                };
                let max_right = pos_x_last + cell_advance;
                let seg_max_rel_x = if fold_u > 0 {
                    if seg_len > 0 { seg_adv_table[seg_len - 1] } else { 0.0 }
                } else {
                    if seg_len > 0 { (seg_len - 1) as f32 * cell_advance } else { 0.0 }
                };
                if seg_max_rel_x > measured_max_row_extent {
                    measured_max_row_extent = seg_max_rel_x;
                }

                if max_right > page_right { page_right = max_right; }
                if py < page_bottom { page_bottom = py; }
                if pz < page_z_min { page_z_min = pz; }
                if pz > page_z_max { page_z_max = pz; }

                if pos_x_0 < ink_min[0] { ink_min[0] = pos_x_0; }
                if max_right > ink_max[0] { ink_max[0] = max_right; }
                if py - half < ink_min[1] { ink_min[1] = py - half; }
                if py + half > ink_max[1] { ink_max[1] = py + half; }
                if pz < ink_min[2] { ink_min[2] = pz; }
                if pz > ink_max[2] { ink_max[2] = pz; }

                seg_start += seg_len;
                s += 1;
            }

            survivor_out += l;
            record_idx += l;

            if nl_pos < bytes.len() {
                record_idx += 1;
                base_row += rows_for_line(l as i64, wrap_w, p.wrap_mode);
                let nl_rel_x = if fold_u > 0 {
                    if l.is_multiple_of(fold_u) { 0.0 } else { seg_adv_table[l % fold_u] }
                } else {
                    l as f32 * cell_advance
                };
                if nl_rel_x > measured_max_row_extent {
                    measured_max_row_extent = nl_rel_x;
                }
                col = 0;
                line_adv = 0.0;
                seg_adv = 0.0;
                pos = nl_pos + 1;
            } else {
                pos = nl_pos;
            }
            continue;
        }

        // Fallback: process this line character by character from pos to nl_pos
        let mut cur_pos = pos;
        while cur_pos < nl_pos {
            let r = match resolve_byte_char_cluster(bytes, cur_pos, trie, bitmap_adv, em_height_fu, cluster, &mut trailer_until, &mut has_cluster) {
                Some(r) => r,
                None => {
                    cur_pos += 1;
                    continue;
                }
            };

            let wrap_segment = wrap_segment_of(col, wrap_w, r.is_newline);
            let row = if is_wrap_back {
                base_row
            } else {
                base_row + wrap_row_of(col, wrap_w, r.is_newline, p.wrap_mode)
            };
            if page_active && page_rows > 0 && pages_wide > 1 && row >= page_rows {
                is_multi_page = true;
            }

            let item_rel_x = if fold_unit > 0 { seg_adv as f64 } else { line_adv };
            if item_rel_x as f32 > measured_max_row_extent {
                measured_max_row_extent = item_rel_x as f32;
            }
            let base_x = (item_rel_x + origin_x) as f32;
            let base_y = (-(row as f64) * line_height + origin_y) as f32;
            let base_z = (-(wrap_segment as f64) * z_step + origin_z) as f32;

            let (pos_x, pos_y, pos_z) = if page_active {
                let (y_page, x_page, screen_row) = if page_rows > 0 {
                    let y_page = row / page_rows;
                    let screen_row = row + scroll_rows;
                    let x_page = if page_cols > 0 {
                        col / page_cols
                    } else {
                        0
                    };
                    (y_page, x_page, screen_row)
                } else {
                    (0, 0, row)
                };
                let band = y_page / pages_wide;
                let px = (base_x as f64 + (y_page % pages_wide) as f64 * page_stride_x) as f32;
                let py = (origin_y
                    - (screen_row - y_page * page_rows) as f64 * line_height
                    - band as f64 * band_stride_y) as f32;
                let pz = (origin_z - wrap_segment as f64 * z_step
                    + band as f64 * depth_per_band
                    + x_page as f64 * depth_per_col) as f32;
                (px, py, pz)
            } else {
                (base_x, base_y, base_z)
            };

            let right = pos_x + r.advance;
            if right > page_right {
                page_right = right;
            }
            if pos_y < page_bottom {
                page_bottom = pos_y;
            }
            if pos_z < page_z_min {
                page_z_min = pos_z;
            }
            if pos_z > page_z_max {
                page_z_max = pos_z;
            }

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
                survivor_out += 1;
            }

            record_idx += 1;
            col += 1;
            line_adv += r.advance as f64;
            if fold_unit > 0 && col % fold_unit == 0 {
                seg_adv = 0.0;
            } else {
                seg_adv += r.advance;
            }

            cur_pos += 1;
        }

        // Process newline if present
        if nl_pos < bytes.len() {
            let r = match resolve_byte_char(bytes, nl_pos, trie, bitmap_adv, em_height_fu, cluster, &mut trailer_until) {
                Some(r) => r,
                None => {
                    pos = nl_pos + 1;
                    continue;
                }
            };
            let wrap_segment = wrap_segment_of(col, wrap_w, true);
            let row = if is_wrap_back {
                base_row
            } else {
                base_row + wrap_row_of(col, wrap_w, true, p.wrap_mode)
            };
            if page_active && page_rows > 0 && pages_wide > 1 && row >= page_rows {
                is_multi_page = true;
            }
            let item_rel_x = if fold_unit > 0 { seg_adv as f64 } else { line_adv };
            if item_rel_x as f32 > measured_max_row_extent {
                measured_max_row_extent = item_rel_x as f32;
            }
            let base_x = (item_rel_x + origin_x) as f32;
            let base_y = (-(row as f64) * line_height + origin_y) as f32;
            let base_z = (-(wrap_segment as f64) * z_step + origin_z) as f32;

            let (pos_x, pos_y, pos_z) = if page_active {
                let (y_page, x_page, screen_row) = if page_rows > 0 {
                    let y_page = row / page_rows;
                    let screen_row = row + scroll_rows;
                    let x_page = if page_cols > 0 {
                        col / page_cols
                    } else {
                        0
                    };
                    (y_page, x_page, screen_row)
                } else {
                    (0, 0, row)
                };
                let band = y_page / pages_wide;
                let px = (base_x as f64 + (y_page % pages_wide) as f64 * page_stride_x) as f32;
                let py = (origin_y
                    - (screen_row - y_page * page_rows) as f64 * line_height
                    - band as f64 * band_stride_y) as f32;
                let pz = (origin_z - wrap_segment as f64 * z_step
                    + band as f64 * depth_per_band
                    + x_page as f64 * depth_per_col) as f32;
                (px, py, pz)
            } else {
                (base_x, base_y, base_z)
            };

            let right = pos_x + r.advance;
            if right > page_right {
                page_right = right;
            }
            if pos_y < page_bottom {
                page_bottom = pos_y;
            }
            if pos_z < page_z_min {
                page_z_min = pos_z;
            }
            if pos_z > page_z_max {
                page_z_max = pos_z;
            }

            record_idx += 1;
            base_row += rows_for_line(col, wrap_w, p.wrap_mode);
            col = 0;
            line_adv = 0.0;
            seg_adv = 0.0;
            pos = nl_pos + 1;
        } else {
            pos = nl_pos;
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
        measured_max_row_extent,
        is_multi_page,
        has_cluster,
    )
}

/// Fast parallel scan to compute an item's measured max_row_extent using SIMD memchr.
#[inline]
pub fn scan_item_max_row_extent(
    bytes: &[u8],
    fold_unit: usize,
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
    cluster_mode: crate::fold::ClusterMode,
) -> f32 {
    let cluster = cluster_mode == crate::fold::ClusterMode::Cluster;
    let mut widest = 0.0f32;
    let mut trailer_until = 0usize;
    let mut pos = 0usize;

    let cell_advance = crate::text::fu_to_world(trie.metrics.advance_fu as i32, em_height_fu);
    let fu = fold_unit;
    let mut seg_adv_stack = [0.0f32; 256];
    let mut seg_adv_heap = Vec::new();
    let seg_adv_table: &[f32] = if fu < 256 {
        let mut cur = 0.0f32;
        for slot in seg_adv_stack.iter_mut().take(fu + 1) {
            *slot = cur;
            cur += cell_advance;
        }
        &seg_adv_stack[..=fu]
    } else {
        seg_adv_heap.reserve(fu + 1);
        let mut cur = 0.0f32;
        for _ in 0..=fu {
            seg_adv_heap.push(cur);
            cur += cell_advance;
        }
        &seg_adv_heap
    };

    while pos < bytes.len() {
        let nl_pos = match memchr::memchr(b'\n', &bytes[pos..]) {
            Some(offset) => pos + offset,
            None => bytes.len(),
        };
        let line = &bytes[pos..nl_pos];
        if pos >= trailer_until && line.iter().all(|b| (0x20..0x7F).contains(b)) {
            let l = line.len();
            let seg_max = if fu > 0 {
                if l >= fu {
                    seg_adv_table[fu - 1]
                } else if l > 0 {
                    seg_adv_table[l - 1]
                } else {
                    0.0
                }
            } else if l > 0 {
                (l - 1) as f32 * cell_advance
            } else {
                0.0
            };
            if seg_max > widest {
                widest = seg_max;
            }
            if nl_pos < bytes.len() {
                let nl_rel_x = if fu > 0 {
                    if l.is_multiple_of(fu) { 0.0 } else { seg_adv_table[l % fu] }
                } else {
                    l as f32 * cell_advance
                };
                if nl_rel_x > widest {
                    widest = nl_rel_x;
                }
                pos = nl_pos + 1;
            } else {
                pos = nl_pos;
            }
            continue;
        }

        let mut cur_pos = pos;
        let mut col = 0usize;
        let mut seg_adv = 0.0f32;
        let mut line_adv = 0.0f64;
        while cur_pos < nl_pos {
            let r = match resolve_byte_char(bytes, cur_pos, trie, bitmap_adv, em_height_fu, cluster, &mut trailer_until) {
                Some(r) => r,
                None => {
                    cur_pos += 1;
                    continue;
                }
            };
            let item_rel_x = if fu > 0 { seg_adv as f64 } else { line_adv };
            if item_rel_x as f32 > widest {
                widest = item_rel_x as f32;
            }
            col += 1;
            line_adv += r.advance as f64;
            if fu > 0 && col.is_multiple_of(fu) {
                seg_adv = 0.0;
            } else {
                seg_adv += r.advance;
            }
            cur_pos += 1;
        }

        if nl_pos < bytes.len() {
            let item_rel_x = if fu > 0 { seg_adv as f64 } else { line_adv };
            if item_rel_x as f32 > widest {
                widest = item_rel_x as f32;
            }
            pos = nl_pos + 1;
        } else {
            pos = nl_pos;
        }
    }

    widest
}

