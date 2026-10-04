//! Pass 2 parallel layout worker for host memory arena glyph instances.

use rayon::prelude::*;
use crate::atlas::TrieTable;
use crate::fold::{rows_for_line, wrap_row_of, wrap_segment_of};
use crate::glyph_scene::GlyphInstance;
use crate::layout::{InkExtent, ItemPlacement, LayoutItem, PageExtent, Paint};
use super::char_resolve::resolve_byte_char;
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
