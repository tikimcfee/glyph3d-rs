//! The line table: what the visible-set field keeps resident instead of a
//! slot per glyph (M1 of `out/GPU-DIRECTION-2026-10-09.md`, 2026-10-10).
//!
//! One [`LineEntry`] per line of every item — where its bytes start, which
//! item it belongs to, the row its first cell sits on, how many glyph
//! slots it produces, its leaders and its widest fold unit (the cull's and
//! the wash's extent, C28) — plus a [`SegmentSeed`] at every cut of a long line, so
//! a kernel can lay out any segment of any line from its seed alone, with the
//! same bits the whole-line fold produces.
//!
//! WHAT A LINE IS. The bytes between two newlines (or the item's start/end),
//! the newline excluded; a file that ends in `\n` has no trailing empty line
//! (the fold emits no record for it and `rows_for_line` counts no row), an
//! unterminated non-empty tail is a line, an empty item has none. The
//! entry's `base_row` is the sum of `fold::rows_for_line` over the lines
//! before it (`WrapMode::Down` folds a long line down several rows;
//! `WrapMode::Back` spends every fold in depth and keeps one row per line),
//! which is exactly the recurrence Pass 1's aggregation already runs for the
//! item's `row_count`; the table is that recurrence written down per line.
//!
//! THE CUT RULE, inherited from the chunk cuts (`chunk.rs`, C15): a long line
//! is cut only BEFORE AN ASCII BYTE. No sequence has an ASCII member after
//! its first (asserted when the atlas loads), so no sequence and no trailer
//! span crosses a cut, and a segment resolves its bytes exactly as a walk of
//! the whole line does. The seed carries what the fold had reached at the
//! cut: the column (leaders so far, trailers included), the segment advance
//! as the RUNNING f32 SUM since the last fold-unit boundary — the fold's own
//! carrier, never `col × adv`, which differs by ulps — and the line advance
//! in CELLS, from which the foldless x is `f32(cells) × cell_adv`: every
//! advance the atlas carries is a whole number of cells (one test below
//! holds that over the entire trie), so the fold's f64 sum of f32 advances is
//! exactly `cells × cell_adv` and narrows to the same f32 as the product.
//!
//! Cuts are planned every [`SEGMENT_BYTES`] from the line's start (or from a
//! chunk's start, for the continuation of a line a chunk cut split), landing
//! on the first ASCII byte at or after each target. A chunk cut inside a
//! line is a seed too, with the state the aggregation computed for it.
//!
//! The table is built by Pass 1's per-chunk walk (`pass1_prepass_chunk_bytes`
//! collects a `ChunkLines` when asked), assembled by the aggregation
//! (`assemble`), and the seeds of lines that start before their chunk get
//! their state from a serial replay of the chunk (`seed_continued_lines`),
//! as `measure_continued_lines` does for widths.

use rayon::prelude::*;

use crate::atlas::TrieTable;
use crate::fold::rows_for_line;
use crate::layout::ItemParams;
use crate::text::fu_to_world;

use super::char_resolve::{self, ResolveCtx};
use super::chunk::LayoutChunk;

/// Planned distance between the cuts of a long line, in bytes. One GPU
/// invocation lays out one segment serially, so this bounds the longest
/// serial walk; on an Apple M2 a 64 KiB minified line cost 31 ms as one
/// invocation and 3.7 ms at this size (Round 2 of the GPU-direction report).
pub const SEGMENT_BYTES: usize = 2048;

/// One line of one item. 24 B, `repr(C)`, uploaded as-is.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct LineEntry {
    /// Offset of the line's first byte within its item.
    pub byte_start: u32,
    /// The item (file) the line belongs to.
    pub item: u32,
    /// The row of the line's first cell within its item: the sum of
    /// `rows_for_line` over the item's earlier lines.
    pub base_row: u32,
    /// Glyph slots the line produces (leaders whose glyph is not 0; the
    /// newline is never one).
    pub glyph_count: u32,
    /// The line's leaders, trailers included — the fold's column at its end
    /// (the newline excluded). The cull counts the line's depth segments
    /// (`col / wrap`) and column pages from it (C28, 2026-10-10).
    pub cols: u32,
    /// The line's widest fold unit in cells — the sum of its leaders'
    /// advances between two fold-unit boundaries, the largest such — or the
    /// whole line's advance when the item has no fold unit. Exactly the x
    /// its glyphs reach from the item's origin: the cull's x bound and the
    /// wash box's width (C28). Measured by Pass 1's walk; a line cut by a
    /// chunk boundary gets its continuation measured again with the true
    /// fold state (`measure_continued_line_widths`).
    pub width_cells: u32,
}

/// A cut inside a long line, and the fold state at it. 24 B, `repr(C)`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct SegmentSeed {
    /// Index into the table's entries.
    pub line: u32,
    /// Offset of the cut within the line; the byte there is ASCII.
    pub byte_offset: u32,
    /// Leaders before the cut (trailers included) — the fold's column.
    pub col: u32,
    /// The segment advance at the cut: the running f32 sum since the last
    /// fold-unit boundary, 0 when `col` is on one or the item has no fold unit.
    pub seg_adv: f32,
    /// The line advance at the cut, in cells: `cells × cell_adv` is the
    /// fold's `line_adv` exactly.
    pub cells: u32,
    pub _pad: u32,
}

/// The assembled table for a set of items.
#[derive(Clone, Debug, Default)]
pub struct LineTable {
    pub entries: Vec<LineEntry>,
    /// Per item, the index of its first entry; `entries.len()` is appended
    /// as a sentinel so `item_lines(i)` is `first[i]..first[i + 1]`.
    pub item_first_line: Vec<u32>,
    /// Sorted by `(line, byte_offset)`.
    pub seeds: Vec<SegmentSeed>,
    pub segment_bytes: u32,
}

impl LineTable {
    /// The entries of item `item`.
    pub fn item_lines(&self, item: usize) -> std::ops::Range<usize> {
        self.item_first_line[item] as usize..self.item_first_line[item + 1] as usize
    }

    /// The byte length of entry `line` (its newline excluded), given its
    /// item's byte length.
    pub fn byte_len(&self, line: usize, item_byte_len: usize) -> usize {
        let e = self.entries[line];
        let next_start = match self.entries.get(line + 1) {
            Some(n) if n.item == e.item => n.byte_start as usize - 1,
            _ => item_byte_len,
        };
        next_start - e.byte_start as usize
    }
}

/// One line as the per-chunk walk saw it (offsets chunk-relative).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChunkLine {
    pub start: u32,
    pub glyphs: u32,
    /// Leaders in the part of the line this chunk holds.
    pub col: u32,
    /// The widest fold unit of that part, in cells, with the fold boundaries
    /// counted from the CHUNK's start: final for a line that starts in the
    /// chunk (its last, possibly partial, unit a lower bound the
    /// continuation's replay raises), meaningless for a continuation's
    /// first line (a unit counted from the wrong boundary can straddle two
    /// true units and over-count), which `measure_continued_line_widths`
    /// measures instead.
    pub width_cells: u32,
    pub terminated: bool,
}

/// A cut the per-chunk walk planned (offsets chunk-relative, state relative
/// to the chunk's start — final for a line that starts in the chunk, a
/// placeholder for a continuation, which [`seed_continued_lines`] replays).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChunkSeed {
    /// Index into the chunk's `lines`.
    pub line: u32,
    pub byte_offset: u32,
    pub col: u32,
    pub seg_adv: f32,
    pub cells: u32,
}

/// What one chunk contributes.
#[derive(Clone, Debug, Default)]
pub(crate) struct ChunkLines {
    pub lines: Vec<ChunkLine>,
    pub seeds: Vec<ChunkSeed>,
}

/// The cut planner for one line (or one chunk's part of a line).
#[derive(Clone, Copy, Debug)]
pub(crate) struct CutPlan {
    next_target: usize,
    segment_bytes: usize,
}

impl CutPlan {
    #[inline(always)]
    pub(crate) fn new(segment_bytes: usize) -> Self {
        Self { next_target: segment_bytes, segment_bytes }
    }

    /// Whether a cut lands before the byte at `offset` (line- or
    /// chunk-relative, as the plan was started): a target has been reached
    /// and the byte is ASCII. Several targets may pass inside a non-ASCII
    /// run; the next target is then the first one past this cut.
    #[inline(always)]
    pub(crate) fn cut_here(&mut self, offset: usize, byte: u8) -> bool {
        if offset >= self.next_target && byte < 0x80 {
            self.next_target = (offset / self.segment_bytes + 1) * self.segment_bytes;
            true
        } else {
            false
        }
    }
}

/// The number of cells an advance is, exactly. Every atlas advance is a whole
/// number of cells (see `every_trie_advance_is_whole_cells`); this is only
/// ever called on advances that came out of the trie.
#[inline(always)]
pub(crate) fn cells_of(advance: f32, cell_adv: f32) -> u32 {
    (advance / cell_adv).round() as u32
}

/// Assemble the per-chunk lines into the table, in item and chunk order,
/// with each entry's base row from the fold's recurrence and the seeds'
/// chunk-relative offsets made line-relative. `chunk_continues` says
/// whether a chunk's first line continues the previous chunk's last;
/// `chunk_initial` is the aggregation's (col, seg_adv, line_adv) at each
/// chunk's start, which seeds the chunk-cut seed and the replay of the
/// continuation's own seeds.
#[allow(clippy::too_many_arguments)]
pub(crate) fn assemble(
    chunk_lines: &[ChunkLines],
    item_chunk_ranges: &[std::ops::Range<usize>],
    chunks: &[LayoutChunk<'_>],
    chunk_continues: &[bool],
    chunk_initial_cols: &[i64],
    chunk_initial_seg_advs: &[f32],
    chunk_initial_line_advs: &[f64],
    file_params: &[ItemParams],
    cell_adv: f32,
    segment_bytes: usize,
) -> LineTable {
    let mut t = LineTable {
        entries: Vec::new(),
        item_first_line: Vec::with_capacity(file_params.len() + 1),
        seeds: Vec::new(),
        segment_bytes: segment_bytes as u32,
    };
    // Seeds of continuation lines, to be replayed: (chunk, seed index in
    // the table) — the table entry's line and byte_offset are final, its
    // state is not.
    for (item, range) in item_chunk_ranges.iter().enumerate() {
        t.item_first_line.push(t.entries.len() as u32);
        let p = &file_params[item];
        let wrap_w = p.wrap_width as i64;
        let mut base_row = 0i64;
        // The entry still open across a chunk boundary, and its leaders so far.
        let mut open: Option<(usize, i64)> = None;
        for ci in range.clone() {
            let cl = &chunk_lines[ci];
            let chunk = &chunks[ci];
            let continues = chunk_continues[ci];
            if continues {
                // The chunk cut is a seed of the open line, with the state the
                // aggregation computed; the first chunk line extends it.
                let (open_idx, open_col) = open.expect("a continuing chunk follows an unterminated line");
                debug_assert!(chunk.byte_offset as u32 > t.entries[open_idx].byte_start);
                t.seeds.push(SegmentSeed {
                    line: open_idx as u32,
                    byte_offset: chunk.byte_offset as u32 - t.entries[open_idx].byte_start,
                    col: chunk_initial_cols[ci] as u32,
                    seg_adv: chunk_initial_seg_advs[ci],
                    cells: (chunk_initial_line_advs[ci] / cell_adv as f64).round() as u32,
                    _pad: 0,
                });
                debug_assert_eq!(chunk_initial_cols[ci], open_col);
            }
            for (k, line) in cl.lines.iter().enumerate() {
                let entry_idx = if k == 0 && continues {
                    let (open_idx, open_col) = open.take().expect("continuation without an open line");
                    t.entries[open_idx].glyph_count += line.glyphs;
                    let total_col = open_col + line.col as i64;
                    // The continuation's width is NOT merged here: its fold
                    // boundaries were counted from the chunk's start
                    // (`ChunkLine::width_cells`); the replay measures it.
                    t.entries[open_idx].cols = total_col as u32;
                    if line.terminated {
                        base_row += rows_for_line(total_col, wrap_w, p.wrap_mode);
                    } else {
                        open = Some((open_idx, total_col));
                    }
                    open_idx
                } else {
                    let idx = t.entries.len();
                    t.entries.push(LineEntry {
                        byte_start: (chunk.byte_offset + line.start as usize) as u32,
                        item: item as u32,
                        base_row: base_row as u32,
                        glyph_count: line.glyphs,
                        cols: line.col,
                        width_cells: line.width_cells,
                    });
                    if line.terminated {
                        base_row += rows_for_line(line.col as i64, wrap_w, p.wrap_mode);
                    } else {
                        open = Some((idx, line.col as i64));
                    }
                    idx
                };
                // This line's seeds. For a continuation's first line they are
                // placeholders until the replay below.
                for s in cl.seeds.iter().filter(|s| s.line as usize == k) {
                    let line_start = t.entries[entry_idx].byte_start as usize;
                    t.seeds.push(SegmentSeed {
                        line: entry_idx as u32,
                        byte_offset: (chunk.byte_offset + s.byte_offset as usize - line_start) as u32,
                        col: s.col,
                        seg_adv: s.seg_adv,
                        cells: s.cells,
                        _pad: 0,
                    });
                }
            }
        }
        // An unterminated last line closed the item's rows in the aggregation
        // (`if cur_col > 0`); here it is already an entry. Nothing to add.
        let _ = base_row;
    }
    t.item_first_line.push(t.entries.len() as u32);
    t
}

/// Replay the continuation lines' seeds with their true state: for every
/// chunk whose first line continues the previous chunk's, walk the chunk from
/// its start with the aggregation's initial (col, seg_adv, line_adv) and
/// snapshot the state at each of that line's cuts. In parallel over such
/// chunks; nothing runs for a corpus with no intra-line chunk cut.
#[allow(clippy::too_many_arguments)]
pub(crate) fn seed_continued_lines(
    t: &mut LineTable,
    chunks: &[LayoutChunk<'_>],
    chunk_continues: &[bool],
    chunk_initial_cols: &[i64],
    chunk_initial_seg_advs: &[f32],
    chunk_initial_line_advs: &[f64],
    file_params: &[ItemParams],
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
) {
    let cell_adv = fu_to_world(trie.metrics.advance_fu as i32, em_height_fu);
    // Which seeds need a replay: those of a continuing chunk's first line
    // that lie inside that chunk (the chunk-cut seed itself is final).
    let wanted: Vec<(usize, Vec<usize>)> = (0..chunks.len())
        .filter(|&ci| chunk_continues[ci])
        .filter_map(|ci| {
            let c = &chunks[ci];
            let chunk_start = c.byte_offset;
            let chunk_end = chunk_start + c.bytes.len();
            // The open line's entry: the last entry of this item whose start
            // is before the chunk.
            let seeds: Vec<usize> = t
                .seeds
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    let e = t.entries[s.line as usize];
                    e.item as usize == c.item_index && {
                        let abs = e.byte_start as usize + s.byte_offset as usize;
                        abs > chunk_start && abs < chunk_end && (e.byte_start as usize) < chunk_start
                    }
                })
                .filter(|(_, s)| {
                    // Only the FIRST line of the chunk continues; a seed of a
                    // later line (which starts inside the chunk) is final.
                    let e = t.entries[s.line as usize];
                    let line_end = memchr::memchr(b'\n', c.bytes).map(|n| chunk_start + n).unwrap_or(chunk_end);
                    (e.byte_start as usize) < chunk_start && e.byte_start as usize + s.byte_offset as usize <= line_end
                })
                .map(|(i, _)| i)
                .collect();
            if seeds.is_empty() {
                None
            } else {
                Some((ci, seeds))
            }
        })
        .collect();
    if wanted.is_empty() {
        return;
    }
    let replayed: Vec<(usize, SegmentSeed)> = wanted
        .par_iter()
        .flat_map_iter(|(ci, seed_idx)| {
            let ci = *ci;
            let c = &chunks[ci];
            let p = &file_params[c.item_index];
            let fold_unit = if p.wrap_width > 0 {
                p.wrap_width as i64
            } else if p.has_page {
                p.page_cols as i64
            } else {
                0
            };
            let rctx = ResolveCtx { trie, bitmap_adv, em_height_fu, cluster: char_resolve::clusters(p) };
            let mut col = chunk_initial_cols[ci];
            let mut seg = chunk_initial_seg_advs[ci];
            let mut cells = (chunk_initial_line_advs[ci] / cell_adv as f64).round() as u32;
            let mut trailer_until = 0usize;
            // Targets in ascending chunk offset.
            let mut targets: Vec<(usize, usize)> = seed_idx
                .iter()
                .map(|&si| {
                    let s = t.seeds[si];
                    let e = t.entries[s.line as usize];
                    (e.byte_start as usize + s.byte_offset as usize - c.byte_offset, si)
                })
                .collect();
            targets.sort_unstable();
            let mut out = Vec::with_capacity(targets.len());
            let mut ti = 0usize;
            for pos in 0..c.bytes.len() {
                while ti < targets.len() && targets[ti].0 == pos {
                    let si = targets[ti].1;
                    let s = t.seeds[si];
                    out.push((si, SegmentSeed { col: col as u32, seg_adv: seg, cells, ..s }));
                    ti += 1;
                }
                if ti >= targets.len() {
                    break;
                }
                let Some(r) = char_resolve::resolve_byte_char(c.bytes, pos, rctx, &mut trailer_until) else {
                    continue;
                };
                if r.is_newline {
                    break;
                }
                col += 1;
                cells += cells_of(r.advance, cell_adv);
                if fold_unit > 0 {
                    if col % fold_unit == 0 {
                        seg = 0.0;
                    } else {
                        seg += r.advance;
                    }
                }
            }
            debug_assert_eq!(ti, targets.len(), "every continuation seed lies inside its chunk's first line");
            out
        })
        .collect();
    for (si, s) in replayed {
        t.seeds[si] = s;
    }
    t.seeds.sort_by_key(|s| (s.line, s.byte_offset));
}

/// The widest fold unit of every line a chunk cut continues, in cells
/// (`LineEntry::width_cells`, C28): the per-chunk walk measured the
/// continuation's part with fold boundaries counted from the CHUNK's start,
/// which can land a unit across two true units and over-count it, so each
/// continuing chunk's first line is walked again here from its true state —
/// the aggregation's column, and the cells already in the unit the cut fell
/// in (the running segment advance, whole cells) — to the line's end or the
/// chunk's, and the entry takes the larger of what its head measured and
/// this. In parallel over the continuing chunks; nothing runs for a corpus
/// with no intra-line chunk cut.
#[allow(clippy::too_many_arguments)]
pub(crate) fn measure_continued_line_widths(
    t: &mut LineTable,
    chunks: &[LayoutChunk<'_>],
    chunk_continues: &[bool],
    chunk_initial_cols: &[i64],
    chunk_initial_seg_advs: &[f32],
    chunk_initial_line_advs: &[f64],
    file_params: &[ItemParams],
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
) {
    let cell_adv = fu_to_world(trie.metrics.advance_fu as i32, em_height_fu);
    let wanted: Vec<usize> = (0..chunks.len()).filter(|&ci| chunk_continues[ci]).collect();
    if wanted.is_empty() {
        return;
    }
    let measured: Vec<(usize, u32)> = wanted
        .par_iter()
        .filter_map(|&ci| {
            let c = &chunks[ci];
            let p = &file_params[c.item_index];
            let fold_unit = if p.wrap_width > 0 {
                p.wrap_width as i64
            } else if p.has_page {
                p.page_cols as i64
            } else {
                0
            };
            // The open entry: the item's last entry that starts before the chunk.
            let range = t.item_lines(c.item_index);
            let idx = t.entries[range.clone()].iter().rposition(|e| (e.byte_start as usize) < c.byte_offset).map(|k| range.start + k)?;
            let rctx = ResolveCtx { trie, bitmap_adv, em_height_fu, cluster: char_resolve::clusters(p) };
            let mut col = chunk_initial_cols[ci];
            let mut unit = if fold_unit > 0 {
                cells_of(chunk_initial_seg_advs[ci], cell_adv)
            } else {
                (chunk_initial_line_advs[ci] / cell_adv as f64).round() as u32
            };
            let mut width = 0u32;
            let mut trailer_until = 0usize;
            for pos in 0..c.bytes.len() {
                let Some(r) = char_resolve::resolve_byte_char(c.bytes, pos, rctx, &mut trailer_until) else {
                    continue;
                };
                if r.is_newline {
                    break;
                }
                col += 1;
                unit += cells_of(r.advance, cell_adv);
                if fold_unit > 0 && col % fold_unit == 0 {
                    width = width.max(unit);
                    unit = 0;
                }
            }
            Some((idx, width.max(unit)))
        })
        .collect();
    for (idx, w) in measured {
        let e = &mut t.entries[idx];
        e.width_cells = e.width_cells.max(w);
    }
}

/// Build the table for one item on its own, as a repo file is laid out: its
/// chunks, Pass 1 with lines, the aggregation and the replay. The item's
/// aggregate rides along (its `row_count` is the table's checksum).
pub fn build_for_item(bytes: &[u8], params: &ItemParams, segment_bytes: usize) -> (LineTable, super::PrepassAggregate) {
    let trie = crate::default_trie();
    let em = trie.metrics.em_height_fu;
    let bitmap_adv = fu_to_world(trie.bitmap_advance_fu, em);
    let buffers = [bytes];
    let (defs, ranges) = super::chunk::slice_byte_buffers_into_chunk_defs(&buffers);
    let chunks: Vec<LayoutChunk<'_>> = defs
        .iter()
        .map(|d| LayoutChunk { item_index: 0, bytes: &bytes[d.byte_offset..d.byte_offset + d.byte_len], byte_offset: d.byte_offset })
        .collect();
    let mut agg = super::pass1_over_chunks_with_lines(&chunks, &ranges, &[bytes], &[*params], &trie, bitmap_adv, em, Some(segment_bytes));
    let table = agg.line_table.take().expect("asked for lines");
    (table, agg)
}

/// `--line-table-stats`: the table over the inputs `--hyper-oracle-check`
/// takes, summarised per corpus, with the first entries and seeds of the
/// first item that has any, so what Pass 1 writes down can be read.
pub fn run_line_table_stats(paths: &[std::path::PathBuf], cluster_mode: crate::fold::ClusterMode) -> ! {
    let mut shown = false;
    let mut all_ok = true;
    for path in paths {
        let corpus = match crate::hyper_oracle::load_corpus(path, cluster_mode) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("line-table: {e}");
                all_ok = false;
                continue;
            }
        };
        let t0 = std::time::Instant::now();
        let (mut bytes, mut lines, mut glyphs, mut seeds, mut seeded_lines, mut longest, mut rows) =
            (0usize, 0usize, 0u64, 0usize, 0usize, 0usize, 0u64);
        for it in &corpus.items {
            let (t, agg) = build_for_item(&it.bytes, &it.params, SEGMENT_BYTES);
            bytes += it.bytes.len();
            lines += t.entries.len();
            glyphs += t.entries.iter().map(|e| e.glyph_count as u64).sum::<u64>();
            seeds += t.seeds.len();
            let mut last_line = u32::MAX;
            for s in &t.seeds {
                if s.line != last_line {
                    seeded_lines += 1;
                    last_line = s.line;
                }
            }
            for i in 0..t.entries.len() {
                longest = longest.max(t.byte_len(i, it.bytes.len()));
            }
            rows += agg.prepasses[0].row_count as u64;
            if !shown && !t.seeds.is_empty() {
                shown = true;
                println!("  first seeded item: {}", it.label);
                for e in t.entries.iter().take(3) {
                    println!("    entry {e:?}");
                }
                for s in t.seeds.iter().take(3) {
                    println!("    seed  {s:?}");
                }
            }
        }
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        println!(
            "line-table: {}: {} items, {} B, {} lines ({:.1} B/line), {} glyphs, {} rows; {} seeds on {} lines (segment {} B, longest line {} B); table {:.1} MB + seeds {:.1} KB; built in {:.1} ms",
            corpus.name,
            corpus.items.len(),
            bytes,
            lines,
            bytes as f64 / lines.max(1) as f64,
            glyphs,
            rows,
            seeds,
            seeded_lines,
            SEGMENT_BYTES,
            longest,
            lines as f64 * std::mem::size_of::<LineEntry>() as f64 / 1e6,
            seeds as f64 * std::mem::size_of::<SegmentSeed>() as f64 / 1e3,
            ms
        );
    }
    std::process::exit(if all_ok { 0 } else { 1 });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold::{ClusterMode, WrapMode};
    use crate::hyper_oracle::{load_corpus, reference_item};
    use std::path::Path;

    fn corpora(cluster_mode: ClusterMode) -> Vec<crate::hyper_oracle::Corpus> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(root.join("engine/fixtures"))
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.to_string_lossy().ends_with(".pipe.bin"))
            .collect();
        paths.sort();
        for p in [
            "native/fixtures/cubecl-fork",
            "native/fixtures/g-cluster-repo",
            "native/fixtures/g-pick-repo",
            "native/fixtures/overflow-leads.txt",
            "native/fixtures/chunk-cut.txt",
            "native/fixtures/chunk-cut-paint.txt",
            "native/fixtures/emoji-corpus-small.txt",
        ] {
            paths.push(root.join(p));
        }
        paths.iter().map(|p| load_corpus(p, cluster_mode).unwrap()).collect()
    }

    fn table_of(bytes: &[u8], params: &ItemParams, segment_bytes: usize) -> (LineTable, u32) {
        let (t, agg) = build_for_item(bytes, params, segment_bytes);
        (t, agg.prepasses[0].row_count)
    }

    /// The fold's lines: for each line, (byte start, glyphs, first row).
    fn fold_lines(bytes: &[u8], params: &ItemParams) -> Vec<(usize, u32, u32)> {
        let trie = crate::default_trie();
        let r = reference_item(bytes, params, &trie);
        let mut out = Vec::new();
        let mut start = 0usize;
        let mut ri = 0usize;
        loop {
            let end = memchr::memchr(b'\n', &bytes[start..]).map(|n| start + n).unwrap_or(bytes.len());
            if start == bytes.len() && end == bytes.len() {
                break;
            }
            let mut glyphs = 0u32;
            let mut first_row: Option<u32> = None;
            while ri < r.records.len() && r.record_bytes[ri] < end {
                let rec = &r.records[ri];
                if rec.counts[0] != 0 {
                    glyphs += 1;
                }
                first_row.get_or_insert(rec.counts[1]);
                ri += 1;
            }
            // An empty line's row is its newline's.
            if first_row.is_none() && ri < r.records.len() && r.record_bytes[ri] == end {
                first_row = Some(r.records[ri].counts[1]);
            }
            if end < bytes.len() {
                // Skip the newline's own record.
                if ri < r.records.len() && r.record_bytes[ri] == end {
                    ri += 1;
                }
            }
            out.push((start, glyphs, first_row.unwrap_or(0)));
            if end >= bytes.len() {
                break;
            }
            start = end + 1;
            if start == bytes.len() {
                break;
            }
        }
        out
    }

    #[test]
    fn line_table_agrees_with_the_fold() {
        let mut items = 0usize;
        let mut lines = 0usize;
        for mode in [ClusterMode::Cluster, ClusterMode::Leader] {
            for corpus in corpora(mode) {
                for it in &corpus.items {
                    let (t, row_count) = table_of(&it.bytes, &it.params, SEGMENT_BYTES);
                    let want = fold_lines(&it.bytes, &it.params);
                    assert_eq!(t.entries.len(), want.len(), "{}: line count", it.label);
                    assert_eq!(t.item_lines(0), 0..want.len(), "{}: item range", it.label);
                    for (i, (e, w)) in t.entries.iter().zip(want.iter()).enumerate() {
                        assert_eq!(e.byte_start as usize, w.0, "{}: line {i} byte start", it.label);
                        assert_eq!(e.glyph_count, w.1, "{}: line {i} glyphs", it.label);
                        assert_eq!(e.base_row, w.2, "{}: line {i} base row", it.label);
                        assert_eq!(e.item, 0);
                    }
                    // The recurrence written down per line sums to the item's
                    // rows — with the aggregation's one exception: an
                    // unterminated tail with no leader at all (an item of
                    // continuation bytes) has no record and takes no row.
                    let mut rows = 0i64;
                    for (i, e) in t.entries.iter().enumerate() {
                        let len = t.byte_len(i, it.bytes.len());
                        let line = &it.bytes[e.byte_start as usize..e.byte_start as usize + len];
                        let col = col_of(line, &it.params);
                        assert_eq!(e.base_row as i64, rows, "{}: line {i} recurrence", it.label);
                        let terminated = e.byte_start as usize + len < it.bytes.len();
                        if terminated || col > 0 {
                            rows += rows_for_line(col, it.params.wrap_width as i64, it.params.wrap_mode);
                        }
                    }
                    assert_eq!(rows as u32, row_count, "{}: rows", it.label);
                    items += 1;
                    lines += t.entries.len();
                }
            }
        }
        // 96 items / 12,620 lines on 2026-10-10 (both cluster modes).
        assert!(items > 60 && lines > 10_000, "the corpora shrank: {items} items, {lines} lines");
    }

    /// Leaders in a line (trailers included), the fold's column.
    fn col_of(line: &[u8], params: &ItemParams) -> i64 {
        let trie = crate::default_trie();
        let em = trie.metrics.em_height_fu;
        let rctx = ResolveCtx {
            trie: &trie,
            bitmap_adv: fu_to_world(trie.bitmap_advance_fu, em),
            em_height_fu: em,
            cluster: char_resolve::clusters(params),
        };
        let mut trailer_until = 0usize;
        (0..line.len()).filter(|&i| char_resolve::resolve_byte_char(line, i, rctx, &mut trailer_until).is_some()).count() as i64
    }

    #[test]
    fn seeds_obey_the_cut_rule_and_the_fold_state() {
        let trie = crate::default_trie();
        let em = trie.metrics.em_height_fu;
        let bitmap_adv = fu_to_world(trie.bitmap_advance_fu, em);
        let cell_adv = fu_to_world(trie.metrics.advance_fu as i32, em);
        let mut seeds_checked = 0usize;
        let mut sequences_near_cuts = 0usize;
        // A small segment so every corpus line of any length carries cuts,
        // under both wrap shapes (the fold unit decides the seg_adv resets).
        for mode in [ClusterMode::Cluster, ClusterMode::Leader] {
            for corpus in corpora(mode) {
                for it in &corpus.items {
                    for wrap in [0i32, 7, 100] {
                        let mut params = it.params;
                        params.wrap_width = wrap;
                        params.wrap_mode = WrapMode::Back;
                        let (t, _) = table_of(&it.bytes, &params, 32);
                        let fold_unit = if wrap > 0 { wrap as i64 } else if params.has_page { params.page_cols as i64 } else { 0 };
                        let rctx = ResolveCtx { trie: &trie, bitmap_adv, em_height_fu: em, cluster: char_resolve::clusters(&params) };
                        let mut prev = (u32::MAX, 0u32);
                        for s in &t.seeds {
                            assert!((s.line, s.byte_offset) > prev || prev.0 == u32::MAX, "{}: seeds sorted", it.label);
                            prev = (s.line, s.byte_offset);
                            let e = t.entries[s.line as usize];
                            let len = t.byte_len(s.line as usize, it.bytes.len());
                            let line = &it.bytes[e.byte_start as usize..e.byte_start as usize + len];
                            let cut = s.byte_offset as usize;
                            assert!(cut > 0 && cut < len, "{}: cut {cut} inside line of {len}", it.label);
                            assert!(line[cut] < 0x80, "{}: cut before a non-ASCII byte at {cut}", it.label);
                            // Replay the line to the cut: the fold's own state.
                            let mut col = 0i64;
                            let mut seg = 0.0f32;
                            let mut line_adv = 0.0f64;
                            let mut trailer_until = 0usize;
                            for pos in 0..cut {
                                let Some(r) = char_resolve::resolve_byte_char(line, pos, rctx, &mut trailer_until) else { continue };
                                if trailer_until > pos {
                                    sequences_near_cuts += 1;
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
                            assert!(trailer_until <= cut, "{}: a sequence spans the cut at {cut}", it.label);
                            assert_eq!(s.col as i64, col, "{}: col at cut {cut} of line {}", it.label, s.line);
                            assert_eq!(s.seg_adv.to_bits(), seg.to_bits(), "{}: seg_adv at cut {cut} of line {}", it.label, s.line);
                            assert_eq!(s.cells as f64 * cell_adv as f64, line_adv, "{}: cells at cut {cut}", it.label);
                            assert_eq!(s.cells as f32 * cell_adv, line_adv as f32, "{}: cells narrow as the fold does", it.label);
                            seeds_checked += 1;
                        }
                    }
                }
            }
        }
        assert!(seeds_checked > 20_000, "the corpora stopped carrying cuts: {seeds_checked}");
        assert!(sequences_near_cuts > 100, "no sequence ever preceded a cut: {sequences_near_cuts}");
    }

    #[test]
    fn every_trie_advance_is_whole_cells() {
        // The seed stores the line advance in cells; this is what makes that
        // exact. Every codepoint the trie answers, and the bitmap advance.
        let trie = crate::default_trie();
        let cell = trie.metrics.advance_fu as i32;
        assert_eq!(trie.bitmap_advance_fu, 2 * cell, "the bitmap advance is two cells");
        let mut distinct = std::collections::BTreeSet::new();
        for cp in 0u32..0x11_0000 {
            let e = trie.lookup(cp);
            assert_eq!(e.advance_fu % cell, 0, "U+{cp:04X} advance {} fu is not whole cells", e.advance_fu);
            distinct.insert(e.advance_fu / cell);
        }
        assert!(distinct.iter().all(|&c| (0..=2).contains(&c)), "advances in cells: {distinct:?}");
    }
}
