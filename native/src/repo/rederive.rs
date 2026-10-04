//! Deterministic record re-derivation and fold compaction.

use std::path::Path;
use crate::layout::{GlyphRecord, InkExtent, ItemParams, PageExtent};

/// Stage G — deterministic per-file re-layout for picking: re-read the file
/// from the repo root and re-run the pure-Rust layout with the EXACT ItemParams
/// it was staged with.
pub fn rederive_records(
    root: &Path,
    _trie: &Path,
    rel_path: &str,
    item: &ItemParams,
) -> std::io::Result<(Vec<GlyphRecord>, Vec<u8>)> {
    let bytes = std::fs::read(root.join(rel_path))?;
    let trie = crate::default_trie();
    let records = crate::layout_hyper::rederive_item_records(&bytes, item, &trie);
    Ok((records, bytes))
}

pub fn rederive_from_bytes(
    _trie: &Path,
    bytes: &[u8],
    item: &ItemParams,
) -> std::io::Result<Vec<GlyphRecord>> {
    let trie = crate::default_trie();
    let records = crate::layout_hyper::rederive_item_records(bytes, item, &trie);
    Ok(records)
}

pub fn rederive_cached(
    _trie: &Path,
    bytes: &[u8],
    item: &ItemParams,
) -> std::io::Result<Vec<GlyphRecord>> {
    let trie = crate::default_trie();
    let records = crate::layout_hyper::rederive_item_records(bytes, item, &trie);
    Ok(records)
}

/// The result of folding one file's record stream: the kept records (rows
/// renumbered, y shifted up over every fold's span), what was dropped, the
/// ORIGINAL record indices kept (in order — the join key for anything with
/// parallel per-record data, e.g. the style walk's leader bytes), and the
/// extents RECOMPUTED from the kept records — the engine's page/ink lanes
/// are documented as max/min over records, so removing records and
/// recomputing is exact, not approximate.
pub struct Folded {
    pub records: Vec<GlyphRecord>,
    pub kept_ix: Vec<u32>,
    pub dropped: usize,
    pub page: PageExtent,
    pub ink: InkExtent,
}

/// P2a — the fold compaction, PURE: drop the records on folded lines,
/// renumber rows, and shift everything below up by each fold's own span.
pub fn compact_folds(
    records: &[GlyphRecord],
    folds: &[std::ops::Range<u32>],
    lines: &[u32],
) -> Folded {
    debug_assert_eq!(records.len(), lines.len(), "record/line index parallel");
    let folded_line = |line: u32| folds.iter().any(|f| f.contains(&line));

    let mut kept: Vec<GlyphRecord> = Vec::with_capacity(records.len());
    let mut kept_ix: Vec<u32> = Vec::with_capacity(records.len());
    let mut dropped = 0usize;
    // Accumulated shift, established lazily when the stream passes OUT of a
    // fold (the first kept record after it names the fold's own pitch).
    let mut y_shift = 0.0f32;
    let mut rows_hidden = 0u32;
    let mut in_fold = false;
    let mut fold_first_y = 0.0f32;
    let mut fold_first_row = 0u32;
    let mut fold_last_row = 0u32;

    let mut page = PageExtent { right: 0.0, bottom: 0.0, z_min: 0.0, z_max: 0.0 };
    let mut ink_min = [f32::INFINITY; 3];
    let mut ink_max = [f32::NEG_INFINITY; 3];
    let mut first_kept = true;

    for (i, (r, &line)) in records.iter().zip(lines).enumerate() {
        if folded_line(line) {
            dropped += 1;
            if !in_fold {
                in_fold = true;
                fold_first_y = r.y();
                fold_first_row = r.row();
            }
            fold_last_row = r.row();
            continue;
        }
        if in_fold {
            // Leaving the fold: this record's ORIGINAL y is the first kept
            // y below it — the fold's span is what separates the two.
            y_shift += fold_first_y - r.y();
            rows_hidden += fold_last_row - fold_first_row + 1;
            in_fold = false;
        }
        let mut r = *r;
        if y_shift != 0.0 {
            let [x, y, z, adv, h] = r.measures;
            r.measures = [x, y + y_shift, z, adv, h];
        }
        if rows_hidden > 0 {
            r.counts[1] = r.row() - rows_hidden;
        }
        kept.push(r);
        kept_ix.push(i as u32);

        // Extents over the kept stream — the engine's documented formulas.
        page.right = page.right.max(r.x() + r.advance());
        page.bottom = if first_kept { r.y() } else { page.bottom.min(r.y()) };
        page.z_min = if first_kept { r.z() } else { page.z_min.min(r.z()) };
        page.z_max = if first_kept { r.z() } else { page.z_max.max(r.z()) };
        ink_min[0] = ink_min[0].min(r.x());
        ink_max[0] = ink_max[0].max(r.x() + r.advance());
        ink_min[1] = ink_min[1].min(r.y() - r.height() * 0.5);
        ink_max[1] = ink_max[1].max(r.y() + r.height() * 0.5);
        ink_min[2] = ink_min[2].min(r.z());
        ink_max[2] = ink_max[2].max(r.z());
        first_kept = false;
    }
    Folded {
        records: kept,
        kept_ix,
        dropped,
        page,
        ink: InkExtent { min: ink_min, max: ink_max },
    }
}
