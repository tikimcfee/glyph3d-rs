//! Deterministic single-item record generation and span color resolution.

use crate::atlas::TrieTable;
use crate::fold::{rows_for_line, wrap_row_of, wrap_segment_of};
use crate::layout::{ByteSpan, GlyphRecord, ItemParams, DEFAULT_COLOR_PACKED};
use crate::text::fu_to_world;
use super::char_resolve::{resolve_byte_char, ResolveCtx};

/// Deterministic single-item record generation in pure Rust, bit-identical to the layout pipeline.
pub fn rederive_item_records(
    bytes: &[u8],
    p: &ItemParams,
    trie: &TrieTable,
) -> Vec<GlyphRecord> {
    let em_height_fu = trie.metrics.em_height_fu;
    let cluster = super::char_resolve::clusters(p);
    let bitmap_adv = fu_to_world(trie.bitmap_advance_fu, em_height_fu);
    let rctx = ResolveCtx { trie, bitmap_adv, em_height_fu, cluster };

    let fold_unit = if p.wrap_width > 0 {
        p.wrap_width as i64
    } else if p.has_page {
        p.page_cols as i64
    } else {
        0
    };

    let page_stride_x = if p.has_page && p.page_rows > 0 {
        let mut col = 0i64;
        let mut line_adv = 0.0f64;
        let mut seg_adv = 0.0f32;
        let mut max_row_extent = 0.0f64;
        let mut trailer_until = 0usize;
        let mut pos = 0usize;
        while pos < bytes.len() {
            let r = match resolve_byte_char(bytes, pos, rctx, &mut trailer_until) {
                Some(r) => r,
                None => {
                    pos += 1;
                    continue;
                }
            };
            let item_rel_x = if fold_unit > 0 { seg_adv as f64 } else { line_adv };
            if item_rel_x > max_row_extent {
                max_row_extent = item_rel_x;
            }
            if r.is_newline {
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
        max_row_extent + p.page_gap_x
    } else {
        0.0
    };

    let page_active = p.has_page && (p.page_rows > 0 || p.page_cols > 0 || p.scroll_rows > 0);
    let mut base_row = 0i64;
    let mut col = 0i64;
    let mut line_adv = 0.0f64;
    let mut seg_adv = 0.0f32;
    let mut trailer_until = 0usize;
    let mut records = Vec::new();

    let mut pos = 0usize;
    while pos < bytes.len() {
        let r = match resolve_byte_char(bytes, pos, rctx, &mut trailer_until) {
            Some(r) => r,
            None => {
                pos += 1;
                continue;
            }
        };

        let wrap_segment = wrap_segment_of(col, p.wrap_width as i64, r.is_newline);
        let wrap_row = wrap_row_of(col, p.wrap_width as i64, r.is_newline, p.wrap_mode);
        let row = base_row + wrap_row;

        let item_rel_x = if fold_unit > 0 { seg_adv as f64 } else { line_adv };
        let base_x = (item_rel_x + p.origin_x) as f32;
        let base_y = (-(row as f64) * p.line_height + p.origin_y) as f32;
        let base_z = (-(wrap_segment as f64) * p.z_step + p.origin_z) as f32;

        let (pos_x, pos_y, pos_z) = if page_active {
            let screen_row = row - p.scroll_rows as i64;
            let y_page = if p.page_rows > 0 && screen_row >= p.page_rows as i64 {
                screen_row / p.page_rows as i64
            } else {
                0
            };
            let x_page = if p.page_cols > 0 {
                col / p.page_cols as i64
            } else {
                0
            };
            let pages_wide = (p.pages_wide as i64).max(1);
            let band = y_page / pages_wide;
            let px = (base_x as f64 + (y_page % pages_wide) as f64 * page_stride_x) as f32;
            let py = (p.origin_y
                - (screen_row - y_page * p.page_rows as i64) as f64 * p.line_height
                - band as f64 * p.band_stride_y) as f32;
            let pz = (p.origin_z - wrap_segment as f64 * p.z_step
                + band as f64 * p.depth_per_band
                + x_page as f64 * p.depth_per_col) as f32;
            (px, py, pz)
        } else {
            (base_x, base_y, base_z)
        };

        records.push(GlyphRecord {
            measures: [pos_x, pos_y, pos_z, r.advance, r.height],
            counts: [r.glyph_id, row as u32, col as u32],
        });

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

    records
}

/// Resolves an AST/LSP byte-span color stream into the exact packed RGBA8 colors
/// corresponding to survivor glyph slots (non-blank printable characters).
/// `cluster_mode` must be the one the file was laid out with: it decides
/// which leaders survive (a keycap head or a ZWJ family is one slot).
pub fn resolve_spans_to_slot_colors(
    bytes: &[u8],
    spans: &[ByteSpan],
    trie: &TrieTable,
    cluster_mode: crate::fold::ClusterMode,
) -> Vec<u32> {
    let cluster = cluster_mode == crate::fold::ClusterMode::Cluster;
    let em_height_fu = trie.metrics.em_height_fu;
    let bitmap_adv = fu_to_world(trie.bitmap_advance_fu, em_height_fu);
    let rctx = ResolveCtx { trie, bitmap_adv, em_height_fu, cluster };
    let mut colors = Vec::new();
    let mut pos = 0usize;
    let mut span_idx = 0usize;
    let mut trailer_until = 0usize;

    while pos < bytes.len() {
        let r = match resolve_byte_char(bytes, pos, rctx, &mut trailer_until) {
            Some(r) => r,
            None => {
                pos += 1;
                continue;
            }
        };

        if r.glyph_id != 0 {
            let p = pos as u32;
            while span_idx < spans.len() && p >= spans[span_idx].end {
                span_idx += 1;
            }
            let color = if span_idx < spans.len() && p >= spans[span_idx].start {
                spans[span_idx].color
            } else {
                DEFAULT_COLOR_PACKED
            };
            colors.push(color);
        }

        pos += 1;
    }

    colors
}
