//! Stage 4 of the reference port — the bake, and the seed protocol it ships.
//!
//! SOURCES. The oracle is `glyphBake.js` (250 lines); the working reference is
//! `engine/glyph_bake.mojo`, which also happens to be where the Mojo keeps the
//! scan monoid. This port keeps the monoid in `scan.rs` (stage 3 needed it
//! first) and imports it here — the same three functions either way.
//!
//! WHAT THE BAKE IS FOR. One streaming pass over a file produces a record that
//! answers questions about it later WITHOUT re-folding: how many rows under an
//! arbitrary wrap, what the exclusive prefix at byte 500,000 is, what the box
//! is. The checkpoints are what make the second of those O(K) instead of O(n) —
//! seed from the nearest checkpoint, fold at most `checkpoint_interval` bytes.
//!
//! That is the "seed and fold" contract the state split ships to clients, and
//! it is why an edit can re-lay 4 KB instead of a whole file.
//!
//! ── THE ORDER OF READS INSIDE THE LOOP IS THE CONTRACT ───────────────────────
//! A leader's lanes are read from the accumulator BEFORE its own leaf folds in,
//! because the accumulator at that moment IS its exclusive prefix. Folding
//! first would give the inclusive one and shift every row, col and ord by one
//! glyph. This mirrors `layout_item`'s order in `fold.rs`, where the position is
//! computed before `col`/`line_advance` advance.

use std::collections::{BTreeMap, BTreeSet};

use crate::fixture::Reader;
use crate::fold::{rows_for_line, sequence_length, WrapMode, TRIE_FLAG_MISSING};
use crate::scan::{scan_combine, scan_identity, scan_leaf_value, ScanElem};
use crate::text::ResolveGlyph;

/// 'G3DB'.
const BAKE_MAGIC: u32 = 0x4244_3347;
const BAKE_VERSION: u32 = 3;

/// Default distance between checkpoints, in bytes. A query seeds from the
/// nearest one and folds at most this many bytes.
///
/// The fixtures each carry their OWN interval (that is the dial
/// `bake-repo-small-k` exists to vary), so nothing in the corpus path reads
/// this; it is the reference's documented default and what the tests bake with.
#[allow(dead_code)]
pub const CHECKPOINT_INTERVAL: usize = 4096;

/// One checkpoint: the six carried fields of an exclusive prefix. `reset`,
/// `wrap` and `mode` are absent on purpose — a checkpoint is always mid-file,
/// and the bake folds at wrap 0, where `rows_for_line` is 1 under EITHER mode.
/// So nothing a checkpoint stores could depend on the mode; the mode is a QUERY
/// parameter here, exactly as the wrap is.
pub const CK_STRIDE: usize = 6;
const CK_NL: usize = 0;
const CK_GLYPHS: usize = 1;
const CK_ROWS: usize = 2;
const CK_HEAD_LEN: usize = 3;
const CK_TAIL_LEN: usize = 4;
const CK_TAIL_ADV: usize = 5;

const NEWLINE: u32 = 0x0A;

#[derive(Clone, Debug, Default)]
pub struct BakeRecord {
    pub byte_length: usize,
    pub leaders: usize,
    pub newlines: i64,
    pub total_rows: i64,
    pub max_line_len: i64,
    /// The fold scalar (lane 7): max x, where x is the line prefix.
    pub max_row_extent: f64,
    /// The box edge (lane 3): max x + advance, in exact f64.
    pub max_line_width: f64,
    pub max_height: f64,
    pub has_box: bool,
    /// minX minY minZ maxX maxY maxZ
    pub box_lanes: [f64; 6],
    pub total: ScanElem,
    /// `checkpoint_count * CK_STRIDE`
    pub checkpoints: Vec<f64>,
    pub checkpoint_interval: usize,
    /// Line lengths, ascending, with their counts — the histogram that makes
    /// `rows_under_wrap` exact for ANY wrap without re-reading the file.
    pub hist_lens: Vec<i64>,
    pub hist_counts: Vec<i64>,
    /// Sorted unique codepoints seen, and the subset the trie could not map.
    pub census: Vec<u32>,
    pub missing: Vec<u32>,
    pub line_height: f64,
}

/// THE BAKE — one streaming pass, the record out.
pub fn bake_file<T: ResolveGlyph + ?Sized>(
    bytes: &[u8],
    trie: &T,
    line_height: f64,
    checkpoint_interval: usize,
) -> Result<BakeRecord, String> {
    // FAIL LOUD AT THE SEAM: a non-positive or NaN line height is malformed
    // input. `!(x > 0.0)` rather than `x <= 0.0` so NaN is refused too.
    // NaN is refused EXPLICITLY rather than by negating a comparison: an
    // unset line height arrives as NaN, and `line_height <= 0.0` alone would
    // ACCEPT it, letting it propagate into every Y in the box — where it would
    // then compare equal to itself in any bit check downstream. (+inf is
    // accepted, matching the reference.)
    if line_height.is_nan() || line_height <= 0.0 {
        return Err(format!("bake_file: a positive lineHeight is required, got {line_height}"));
    }
    let interval = checkpoint_interval.max(1);
    let byte_len = bytes.len();

    let mut record = BakeRecord {
        byte_length: byte_len,
        checkpoint_interval: interval,
        line_height,
        ..BakeRecord::default()
    };
    // One checkpoint per interval BOUNDARY strictly inside the file: a file of
    // exactly `interval` bytes has none, since there is no byte after the
    // boundary to seed.
    let checkpoint_count = if byte_len > 0 {
        (byte_len - 1) / interval
    } else {
        0
    };
    record.checkpoints = vec![0.0; checkpoint_count * CK_STRIDE];

    // Sets and a map, sorted ONCE at the end — what the oracle does with
    // Set/Map. A sorted insert per leader would cost a binary search plus a
    // possible memmove, millions of times over.
    let mut census = BTreeSet::new();
    let mut missing = BTreeSet::new();
    let mut histogram: BTreeMap<i64, i64> = BTreeMap::new();

    let mut accumulator = scan_identity();
    let mut max_row: i64 = -1;
    let mut max_top = f64::NEG_INFINITY;

    for id in 0..byte_len {
        // The checkpoint is written for every position that is a multiple of the
        // interval, INCLUDING one that lands on a continuation byte — a query's
        // seed point is a byte offset, not a glyph.
        if id > 0 && id % interval == 0 {
            let offset = (id / interval - 1) * CK_STRIDE;
            record.checkpoints[offset + CK_NL] = accumulator.newlines as f64;
            record.checkpoints[offset + CK_GLYPHS] = accumulator.glyphs as f64;
            record.checkpoints[offset + CK_ROWS] = accumulator.rows as f64;
            record.checkpoints[offset + CK_HEAD_LEN] = accumulator.head_len as f64;
            record.checkpoints[offset + CK_TAIL_LEN] = accumulator.tail_len as f64;
            record.checkpoints[offset + CK_TAIL_ADV] = accumulator.tail_advance as f64;
        }

        let sequence_len = sequence_length(bytes, id);
        if sequence_len == 0 {
            continue; // continuation or invalid byte: identity leaf, skipped
        }
        let codepoint = crate::fold::decode_codepoint_at(bytes, id, sequence_len);
        let resolved = trie.resolve(codepoint);
        census.insert(codepoint);
        if resolved.flags & TRIE_FLAG_MISSING != 0 {
            missing.insert(codepoint);
        }
        record.leaders += 1;

        // THE EXCLUSIVE PREFIX IS THE ACCUMULATOR RIGHT NOW — read the leader's
        // wrap-0 lanes before its own leaf folds in.
        let row = accumulator.newlines; // wrap 0: every closed line is one row
        let x = accumulator.tail_advance as f64; // the foldless x IS the line prefix
        if row > max_row {
            max_row = row;
        }
        if x > record.max_row_extent {
            record.max_row_extent = x;
        }
        // The box edge is x + advance in exact f64, distinct from the fold
        // scalar above — one is where the last glyph STARTS, the other where it
        // ENDS.
        let right_edge = x + resolved.advance as f64;
        if right_edge > record.max_line_width {
            record.max_line_width = right_edge;
        }
        let top = resolved.height as f64 - row as f64 * line_height;
        if top > max_top {
            max_top = top;
        }
        if resolved.height as f64 > record.max_height {
            record.max_height = resolved.height as f64;
        }
        if codepoint == NEWLINE {
            *histogram.entry(accumulator.tail_len).or_insert(0) += 1;
        }

        // THE BAKE FOLDS AT WRAP 0 AND MODE Down, and both are inert there: at
        // wrap 0 `rows_for_line` is 1 whatever the mode. Stated rather than
        // defaulted, because a reader has to know the record is mode-free.
        let leaf = scan_leaf_value(
            codepoint == NEWLINE,
            resolved.advance,
            true,
            0,
            id == 0,
            WrapMode::Down,
        );
        scan_combine(&mut accumulator, &leaf);
    }

    record.census = census.into_iter().collect();
    record.missing = missing.into_iter().collect();
    for (length, count) in histogram {
        record.hist_lens.push(length);
        record.hist_counts.push(count);
    }

    record.newlines = accumulator.newlines;
    record.total_rows = max_row + 1;
    // The longest line is the open tail OR any closed line in the histogram —
    // a file whose last line is short still has a long one somewhere.
    record.max_line_len = accumulator.tail_len;
    for &length in &record.hist_lens {
        if length > record.max_line_len {
            record.max_line_len = length;
        }
    }
    if record.leaders > 0 {
        record.has_box = true;
        record.box_lanes = [
            0.0,
            -(max_row as f64) * line_height,
            0.0,
            record.max_line_width,
            max_top,
            0.0,
        ];
    }
    record.total = accumulator;
    Ok(record)
}

/// Checkpoint `index` — the exclusive prefix at byte `(index + 1) * interval`.
fn checkpoint_at(checkpoints: &[f64], index: usize) -> ScanElem {
    let offset = index * CK_STRIDE;
    ScanElem {
        newlines: checkpoints[offset + CK_NL] as i64,
        glyphs: checkpoints[offset + CK_GLYPHS] as i64,
        rows: checkpoints[offset + CK_ROWS] as i64,
        head_len: checkpoints[offset + CK_HEAD_LEN] as i64,
        tail_len: checkpoints[offset + CK_TAIL_LEN] as i64,
        tail_advance: checkpoints[offset + CK_TAIL_ADV] as f32,
        ..ScanElem::default()
    }
}

/// Fold bytes `[from_byte, to_byte)` onto `accumulator` — the seeding
/// primitive. Identity (or a checkpoint) plus this reaches the exact exclusive
/// prefix of `to_byte`.
fn fold_bytes<T: ResolveGlyph + ?Sized>(
    bytes: &[u8],
    trie: &T,
    from_byte: usize,
    to_byte: usize,
    accumulator: &mut ScanElem,
) {
    for id in from_byte..to_byte {
        let sequence_len = sequence_length(bytes, id);
        if sequence_len == 0 {
            continue;
        }
        let codepoint = crate::fold::decode_codepoint_at(bytes, id, sequence_len);
        let resolved = trie.resolve(codepoint);
        let leaf = scan_leaf_value(
            codepoint == NEWLINE,
            resolved.advance,
            true,
            0,
            id == 0,
            WrapMode::Down,
        );
        scan_combine(accumulator, &leaf);
    }
}

/// The exclusive prefix of `byte_index` — nearest checkpoint plus a tail fold of
/// at most `checkpoint_interval` bytes. THE POINT OF THE WHOLE RECORD.
pub fn prefix_at<T: ResolveGlyph + ?Sized>(
    bytes: &[u8],
    trie: &T,
    record: &BakeRecord,
    byte_index: usize,
) -> ScanElem {
    let interval = record.checkpoint_interval;
    let available = record.checkpoints.len() / CK_STRIDE;
    // THE CHECKPOINT INDEX IS A HINT, AND THE ERROR IS ONE-SIDED: seeding from
    // any EARLIER checkpoint is still correct — it just folds more bytes —
    // while seeding from a later one skips a prefix and is wrong. Measured:
    // clamping one checkpoint low passes the whole corpus and every test,
    // because it is a performance mutation, not a correctness one. Clamping one
    // high reads off the end. Only the `min` matters, and it matters upward.
    let checkpoint = (byte_index / interval).min(available);
    let mut accumulator = if checkpoint > 0 {
        checkpoint_at(&record.checkpoints, checkpoint - 1)
    } else {
        scan_identity()
    };
    fold_bytes(bytes, trie, checkpoint * interval, byte_index, &mut accumulator);
    accumulator
}

/// Exact visual rows under ANY wrap width AND mode, from the histogram plus the
/// open tail — without re-reading a byte.
///
/// Under [`WrapMode::Back`] every line contributes exactly one row, so this
/// counts LINES and the histogram's lengths stop mattering. It still walks the
/// histogram rather than short-circuiting on the count: the per-line rule is one
/// function, asked once per line, and the two answers fall out of the same loop.
pub fn rows_under_wrap(record: &BakeRecord, wrap: i64, mode: WrapMode) -> i64 {
    let mut rows = 0i64;
    for (&length, &count) in record.hist_lens.iter().zip(record.hist_counts.iter()) {
        rows += rows_for_line(length, wrap, mode) * count;
    }
    // The still-open final line, if any. This USED to spell out `(tail - 1) /
    // wrap + 1` because the shared helper over-counted a terminated line by one
    // at an exact multiple — the phantom row, seen from here and worked around
    // locally in 2026-09-02. With `rows_for_line` corrected the two rules are
    // the same rule, so the special case is gone: a line covers the rows its
    // cells reach whether or not a newline closes it. (A file ending in a
    // newline and one that does not now agree at exact multiples, which is
    // right — a trailing newline adds no content-bearing row, and at every
    // NON-multiple length they always did agree.)
    let tail = record.total.tail_len;
    if tail > 0 {
        rows += rows_for_line(tail, wrap, mode);
    }
    rows
}

// ── The 'G3DB' fixture: the seed protocol, serialized from the JS oracle ────

/// One bake fixture: inputs (bytes, trie, line height, interval) plus the
/// oracle's expected record AND its expected QUERY answers.
///
/// The query half is what makes this more than a second record comparison: a
/// bake whose totals are right but whose checkpoints are subtly wrong answers
/// every whole-file question correctly and every random-access one incorrectly.
pub struct BakeFixture {
    pub name: String,
    pub bytes: Vec<u8>,
    pub trie: crate::fixture::FixtureTrie,
    pub line_height: f64,
    pub checkpoint_interval: usize,
    pub expected: BakeRecord,
    pub prefix_queries: Vec<PrefixQuery>,
    /// (wrap, mode, expected rows)
    pub wrap_queries: Vec<(i64, WrapMode, i64)>,
}

/// One recorded seed-protocol query: what the oracle answered when asked for
/// the exclusive prefix of `byte_index`, and the lanes that prefix yields at
/// `wrap`.
pub struct PrefixQuery {
    pub byte_index: usize,
    pub wrap: i64,
    /// v3: the mode the recorded lanes were resolved under. The PREFIX is
    /// mode-free; the row it resolves to is not.
    pub mode: WrapMode,
    pub prefix: [f64; 7],
    pub row: u32,
    pub col: u32,
    pub ord: u32,
    pub line_advance: f64,
}

/// A ScanElem as the fixture serializes it: reset, nl, glyphs, rows, headLen,
/// tailLen, tailAdv. The `wrap` and `mode` fields are deliberately absent —
/// they are query parameters, not part of a prefix.
fn elem_lanes(element: &ScanElem) -> [f64; 7] {
    [
        element.reset as f64,
        element.newlines as f64,
        element.glyphs as f64,
        element.rows as f64,
        element.head_len as f64,
        element.tail_len as f64,
        element.tail_advance as f64,
    ]
}

pub fn load_bake_fixture(path: &std::path::Path) -> Result<BakeFixture, String> {
    let raw = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    load_bake_bytes(&raw, name).map_err(|e| format!("{}: {e}", path.display()))
}

fn load_bake_bytes(raw: &[u8], name: String) -> Result<BakeFixture, String> {
    let mut reader = Reader::new(raw);
    if reader.u32()? != BAKE_MAGIC {
        return Err("bad magic (not a .bake.bin fixture)".into());
    }
    let version = reader.u32()?;
    if version != BAKE_VERSION {
        return Err(format!("unknown bake version {version} (expected {BAKE_VERSION})"));
    }
    let byte_len = reader.u32()? as usize;
    let block_index_len = reader.u32()? as usize;
    let blocks_len = reader.u32()? as usize;
    let line_height = reader.f64()?;
    let checkpoint_interval = reader.u32()? as usize;

    let bytes = reader.take_bytes(byte_len)?;
    let mut block_index = Vec::with_capacity(block_index_len);
    for _ in 0..block_index_len {
        block_index.push(reader.u32()?);
    }
    // Same v2 carrier split as the .pipe.bin loader: f64 VALUES on disk,
    // realized here as f32 measures and native u32 identity/bitfield.
    let entries = blocks_len / 4;
    let mut blocks_m = Vec::with_capacity(entries * 2);
    let mut blocks_c = Vec::with_capacity(entries * 2);
    for _ in 0..entries {
        let glyph_id = reader.f64()?;
        let advance = reader.f64()?;
        let height = reader.f64()?;
        let flags = reader.f64()?;
        blocks_m.push(advance as f32);
        blocks_m.push(height as f32);
        blocks_c.push(glyph_id as u32);
        blocks_c.push(flags as u32);
    }
    let trie = crate::fixture::FixtureTrie {
        block_index,
        blocks_m,
        blocks_c,
    };

    let mut expected = BakeRecord {
        byte_length: byte_len,
        checkpoint_interval,
        line_height,
        ..BakeRecord::default()
    };
    expected.leaders = reader.u32()? as usize;
    expected.newlines = reader.u32()? as i64;
    expected.total_rows = reader.u32()? as i64;
    expected.max_line_len = reader.u32()? as i64;
    expected.max_row_extent = reader.f64()?;
    expected.max_line_width = reader.f64()?;
    expected.max_height = reader.f64()?;
    expected.has_box = reader.u32()? != 0;
    for lane in 0..6 {
        expected.box_lanes[lane] = reader.f64()?;
    }
    let mut total = [0.0f64; 7];
    for lane in total.iter_mut() {
        *lane = reader.f64()?;
    }
    expected.total = ScanElem {
        reset: total[0] as i64,
        newlines: total[1] as i64,
        glyphs: total[2] as i64,
        rows: total[3] as i64,
        head_len: total[4] as i64,
        tail_len: total[5] as i64,
        tail_advance: total[6] as f32,
        // Neither is serialized: the fixture's `total` is a PREFIX, and wrap and
        // mode are query parameters. The bake folds at wrap 0, where the mode
        // cannot change an answer.
        wrap: 0,
        mode: WrapMode::Down,
    };
    let checkpoint_count = reader.u32()? as usize;
    expected.checkpoints = Vec::with_capacity(checkpoint_count * CK_STRIDE);
    for _ in 0..checkpoint_count * CK_STRIDE {
        expected.checkpoints.push(reader.f64()?);
    }
    let hist_count = reader.u32()? as usize;
    for _ in 0..hist_count {
        expected.hist_lens.push(reader.u32()? as i64);
        expected.hist_counts.push(reader.u32()? as i64);
    }
    let census_count = reader.u32()? as usize;
    for _ in 0..census_count {
        expected.census.push(reader.u32()?);
    }
    let missing_count = reader.u32()? as usize;
    for _ in 0..missing_count {
        expected.missing.push(reader.u32()?);
    }

    let prefix_query_count = reader.u32()? as usize;
    let mut prefix_queries = Vec::with_capacity(prefix_query_count);
    for _ in 0..prefix_query_count {
        let byte_index = reader.u32()? as usize;
        let wrap = reader.u32()? as i64;
        let mode = WrapMode::from_code(reader.u32()? as i64);
        let mut prefix = [0.0f64; 7];
        for lane in prefix.iter_mut() {
            *lane = reader.f64()?;
        }
        let row = reader.u32()?;
        let col = reader.u32()?;
        let ord = reader.u32()?;
        let line_advance = reader.f64()?;
        prefix_queries.push(PrefixQuery {
            byte_index,
            wrap,
            mode,
            prefix,
            row,
            col,
            ord,
            line_advance,
        });
    }
    let wrap_query_count = reader.u32()? as usize;
    let mut wrap_queries = Vec::with_capacity(wrap_query_count);
    for _ in 0..wrap_query_count {
        let wrap = reader.u32()? as i64;
        let mode = WrapMode::from_code(reader.u32()? as i64);
        let rows = reader.u32()? as i64;
        wrap_queries.push((wrap, mode, rows));
    }

    // Same structural check the .pipe.bin loader makes: every section length is
    // derived from a header field, so a wrong stride reads plausible values and
    // stops in the wrong place.
    if reader.at != raw.len() {
        return Err(format!(
            "parse consumed {} of {} bytes — {} left over",
            reader.at,
            raw.len(),
            raw.len() - reader.at
        ));
    }

    Ok(BakeFixture {
        name,
        bytes,
        trie,
        line_height,
        checkpoint_interval,
        expected,
        prefix_queries,
        wrap_queries,
    })
}

pub struct BakeDiff {
    pub leaders: usize,
    pub checkpoints: usize,
    pub prefix_queries: usize,
    pub wrap_queries: usize,
    pub bad: Vec<String>,
}

/// Replay a bake fixture and diff BIT-EXACT — the record AND every query.
pub fn diff_bake(fixture: &BakeFixture) -> BakeDiff {
    let mut bad = Vec::new();
    let got = match bake_file(
        &fixture.bytes,
        &fixture.trie,
        fixture.line_height,
        fixture.checkpoint_interval,
    ) {
        Ok(record) => record,
        Err(e) => {
            return BakeDiff {
                leaders: 0,
                checkpoints: 0,
                prefix_queries: 0,
                wrap_queries: 0,
                bad: vec![e],
            }
        }
    };
    let expected = &fixture.expected;

    let check_i64 = |name: &str, got: i64, want: i64, bad: &mut Vec<String>| {
        if got != want {
            bad.push(format!("{name}: got {got} vs fixture {want}"));
        }
    };
    // The record echoes its own inputs; check them rather than carry them
    // unread. A byte_length that disagrees with the buffer would mean the
    // record and the bytes a query folds have drifted apart.
    check_i64("byteLength", got.byte_length as i64, fixture.bytes.len() as i64, &mut bad);
    if got.line_height.to_bits() != fixture.line_height.to_bits() {
        bad.push(format!(
            "lineHeight: got {} vs fixture {}",
            got.line_height, fixture.line_height
        ));
    }
    check_i64("leaders", got.leaders as i64, expected.leaders as i64, &mut bad);
    check_i64("newlines", got.newlines, expected.newlines, &mut bad);
    check_i64("totalRows", got.total_rows, expected.total_rows, &mut bad);
    check_i64("maxLineLen", got.max_line_len, expected.max_line_len, &mut bad);

    // f64 lanes compare as BIT patterns, so an infinity or a NaN is caught like
    // any other value instead of slipping through a float compare.
    let check_f64 = |name: &str, got: f64, want: f64, bad: &mut Vec<String>| {
        if got.to_bits() != want.to_bits() {
            bad.push(format!("{name}: got {got} ({:#018x}) vs fixture {want}", got.to_bits()));
        }
    };
    check_f64("maxRowExtent", got.max_row_extent, expected.max_row_extent, &mut bad);
    check_f64("maxLineWidth", got.max_line_width, expected.max_line_width, &mut bad);
    check_f64("maxHeight", got.max_height, expected.max_height, &mut bad);
    if got.has_box != expected.has_box {
        bad.push(format!("hasBox: got {} vs fixture {}", got.has_box, expected.has_box));
    }
    if got.has_box && expected.has_box {
        for lane in 0..6 {
            check_f64(
                &format!("box[{lane}]"),
                got.box_lanes[lane],
                expected.box_lanes[lane],
                &mut bad,
            );
        }
    }
    let (got_total, want_total) = (elem_lanes(&got.total), elem_lanes(&expected.total));
    for lane in 0..7 {
        check_f64(&format!("total[{lane}]"), got_total[lane], want_total[lane], &mut bad);
    }

    if got.checkpoints.len() != expected.checkpoints.len() {
        bad.push(format!(
            "ckCount: got {} vs fixture {}",
            got.checkpoints.len() / CK_STRIDE,
            expected.checkpoints.len() / CK_STRIDE
        ));
    } else {
        for (i, (&g, &e)) in got.checkpoints.iter().zip(expected.checkpoints.iter()).enumerate() {
            check_f64(&format!("checkpoint[{}][{}]", i / CK_STRIDE, i % CK_STRIDE), g, e, &mut bad);
        }
    }
    if got.hist_lens != expected.hist_lens || got.hist_counts != expected.hist_counts {
        bad.push(format!(
            "histogram: got {} bins vs fixture {}",
            got.hist_lens.len(),
            expected.hist_lens.len()
        ));
    }
    if got.census != expected.census {
        bad.push(format!(
            "census: got {} codepoints vs fixture {}",
            got.census.len(),
            expected.census.len()
        ));
    }
    if got.missing != expected.missing {
        bad.push(format!(
            "missing: got {:?} vs fixture {:?}",
            got.missing, expected.missing
        ));
    }

    // ── The query side: checkpoint-seeded random access ──────────────────────
    for query in &fixture.prefix_queries {
        let at = query.byte_index;
        let prefix = prefix_at(&fixture.bytes, &fixture.trie, &got, at);
        let got_prefix = elem_lanes(&prefix);
        for (lane, (&got_lane, &want_lane)) in
            got_prefix.iter().zip(query.prefix.iter()).enumerate()
        {
            check_f64(&format!("prefix@{at}[{lane}]"), got_lane, want_lane, &mut bad);
        }
        // Whether the QUERIED byte is itself a newline decides which row rule
        // applies to it (`fold::wrap_row_of`), and the prefix cannot know: it
        // describes everything BEFORE the byte.
        let terminator = at < fixture.bytes.len()
            && sequence_length(&fixture.bytes, at) > 0
            && crate::fold::decode_codepoint_at(&fixture.bytes, at, sequence_length(&fixture.bytes, at))
                == NEWLINE;
        let lanes =
            crate::scan::lanes_from_prefix(&prefix, query.wrap, terminator, query.mode);
        check_i64(
            &format!("row@{at}w{}m{}", query.wrap, query.mode.code()),
            lanes.row,
            query.row as i64,
            &mut bad,
        );
        check_i64(&format!("col@{at}"), lanes.col, query.col as i64, &mut bad);
        check_i64(&format!("ord@{at}"), lanes.ord, query.ord as i64, &mut bad);
        check_f64(&format!("lineAdv@{at}"), lanes.line_advance as f64, query.line_advance, &mut bad);
    }
    for &(wrap, mode, want_rows) in &fixture.wrap_queries {
        check_i64(
            &format!("rowsUnderWrap({wrap}, mode {})", mode.code()),
            rows_under_wrap(&got, wrap, mode),
            want_rows,
            &mut bad,
        );
    }

    BakeDiff {
        leaders: got.leaders,
        checkpoints: got.checkpoints.len() / CK_STRIDE,
        prefix_queries: fixture.prefix_queries.len(),
        wrap_queries: fixture.wrap_queries.len(),
        bad,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph_trie::{build_glyph_trie, BuiltTrie, GlyphMetrics};
    use std::path::{Path, PathBuf};

    fn trie() -> BuiltTrie {
        build_glyph_trie(
            (0x20u32..0x7Fu32).chain(std::iter::once(0x0A)),
            |cp| Some(GlyphMetrics { glyph_id: cp + 1, advance: 0.6 + (cp % 13) as f32 * 0.0173, height: 1.2 }),
            0.61,
            1.25,
        )
    }

    fn bake_fixtures() -> Vec<PathBuf> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/fixtures");
        let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
            .expect("fixtures dir")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.to_string_lossy().ends_with(".bake.bin"))
            .collect();
        v.sort();
        assert_eq!(v.len(), 8, "bake corpus size changed — update deliberately");
        v
    }

    /// No fixture can carry an invalid line height — `gen-bake.mjs` would not
    /// produce one — so the guard is uncoverable by the corpus.
    ///
    /// NaN matters as much as zero here, and is why the guard is written
    /// `!(line_height > 0.0)` rather than `line_height <= 0.0`: the second form
    /// ACCEPTS NaN, which would then propagate silently into every Y in the box
    /// and compare equal to itself in any bit check downstream.
    #[test]
    fn a_non_positive_line_height_is_refused() {
        let t = trie();
        for bad in [0.0, -1.0, -0.0, f64::NAN] {
            assert!(
                bake_file(b"abc", &t, bad, CHECKPOINT_INTERVAL).is_err(),
                "line_height {bad} must be refused"
            );
        }
        assert!(bake_file(b"abc", &t, 1.0, CHECKPOINT_INTERVAL).is_ok());
    }

    /// THE SEED PROTOCOL'S ACTUAL CLAIM, at every byte rather than at the ~33
    /// the fixtures sample: seeding from the nearest checkpoint and folding the
    /// tail must reach exactly what folding from zero reaches.
    ///
    /// The fixture's sampled indices can only ever spot-check this. Checking all
    /// of them is what makes an off-by-one in the checkpoint INDEX impossible to
    /// miss, since such a bug is correct at every index below the first
    /// checkpoint and wrong above it.
    #[test]
    fn a_seeded_prefix_equals_a_full_fold_at_every_byte() {
        let mut checked = 0usize;
        let mut seeded_from_a_checkpoint = 0usize;
        for path in bake_fixtures() {
            let fx = load_bake_fixture(&path).unwrap();
            let record = bake_file(
                &fx.bytes,
                &fx.trie,
                fx.line_height,
                fx.checkpoint_interval,
            )
            .unwrap();
            for byte_index in 0..=fx.bytes.len() {
                let seeded = prefix_at(&fx.bytes, &fx.trie, &record, byte_index);
                let mut scratch = scan_identity();
                fold_bytes(&fx.bytes, &fx.trie, 0, byte_index, &mut scratch);
                assert_eq!(
                    (seeded.newlines, seeded.glyphs, seeded.rows, seeded.head_len, seeded.tail_len),
                    (scratch.newlines, scratch.glyphs, scratch.rows, scratch.head_len, scratch.tail_len),
                    "{} @ {byte_index}: seeded prefix != full fold",
                    fx.name
                );
                assert_eq!(
                    seeded.tail_advance.to_bits(),
                    scratch.tail_advance.to_bits(),
                    "{} @ {byte_index}: tail_advance must be BIT-equal — the seeded \
                     path performs the same f32 adds in the same order",
                    fx.name
                );
                checked += 1;
                if byte_index >= fx.checkpoint_interval && !record.checkpoints.is_empty() {
                    seeded_from_a_checkpoint += 1;
                }
            }
        }
        // ANTI-VACUITY: most indices are below the first checkpoint in a small
        // fixture, where `prefix_at` folds from zero and the comparison is a
        // tautology. The claim is only tested where a checkpoint is actually
        // used as the seed.
        assert!(checked > 20_000, "only {checked} byte positions checked");
        assert!(
            seeded_from_a_checkpoint > 1000,
            "only {seeded_from_a_checkpoint} positions actually seeded from a checkpoint"
        );
    }

    /// An independent cross-check the fixture format does not state: at wrap 0
    /// nothing wraps, so the histogram plus the open tail must reproduce
    /// `total_rows`, which the bake derived by a completely different route
    /// (the max row observed during the streaming pass).
    #[test]
    fn rows_under_wrap_zero_agrees_with_total_rows() {
        for path in bake_fixtures() {
            let fx = load_bake_fixture(&path).unwrap();
            let record =
                bake_file(&fx.bytes, &fx.trie, fx.line_height, fx.checkpoint_interval).unwrap();
            assert_eq!(
                rows_under_wrap(&record, 0, WrapMode::Down),
                record.total_rows,
                "{}: histogram+tail and the streamed max row must agree at wrap 0",
                fx.name
            );
            // At wrap 0 nothing folds, so WrapBack must give the SAME answer —
            // the mode is about how a wrap is spent, and there is no wrap here.
            assert_eq!(
                rows_under_wrap(&record, 0, WrapMode::Back),
                record.total_rows,
                "{}: at wrap 0 the modes must coincide",
                fx.name
            );
        }
    }

    /// THE PHANTOM ROW, seen from the histogram (2026-09-04).
    ///
    /// `rows_under_wrap` answers ANY wrap from the line histogram without
    /// re-reading a byte, so it must land on the same total the fold does. It
    /// used to spell the tail line's rule out separately BECAUSE the shared
    /// `rows_for_line` over-counted a terminated line at an exact multiple —
    /// which is the defect, worked around locally instead of fixed. With one
    /// rule there is one answer, and this pins it against rows counted by hand.
    #[test]
    fn rows_under_wrap_counts_exact_multiple_lines_once() {
        let t = trie();
        // Three closed 8-cell lines and an open 8-cell tail. At wrap 4 each is
        // exactly two rows; at wrap 8 each is exactly one; at wrap 3 each is
        // three (8 = 3+3+2). None of those is `len/wrap + 1`.
        let bytes = b"aaaaaaaa\nbbbbbbbb\ncccccccc\ndddddddd".to_vec();
        let record = bake_file(&bytes, &t, 1.0, 4096).unwrap();
        assert_eq!(record.hist_lens, vec![8], "three closed lines, all 8 cells");
        assert_eq!(record.hist_counts, vec![3]);
        assert_eq!(record.total.tail_len, 8, "and an unterminated 8-cell tail");
        for (wrap, want) in [(1i64, 32i64), (2, 16), (3, 12), (4, 8), (5, 8), (8, 4), (9, 4), (0, 4)] {
            assert_eq!(
                rows_under_wrap(&record, wrap, WrapMode::Down),
                want,
                "rows_under_wrap({wrap}) over 4 lines of 8 cells"
            );
            // WrapBack: four lines, four rows, at EVERY wrap — the answer stops
            // depending on the wrap at all, which is the whole point of the mode.
            assert_eq!(
                rows_under_wrap(&record, wrap, WrapMode::Back),
                4,
                "rows_under_wrap({wrap}, Back) counts LINES: 4"
            );
        }

        // A TERMINATED file and an UNTERMINATED one must now agree — the tail
        // rule and the closed-line rule are the same rule. They always agreed
        // at non-multiples; disagreeing only at multiples was the bug.
        let terminated = bake_file(b"aaaaaaaa\n", &t, 1.0, 4096).unwrap();
        let open = bake_file(b"aaaaaaaa", &t, 1.0, 4096).unwrap();
        for wrap in [1i64, 2, 3, 4, 5, 8, 9] {
            assert_eq!(
                rows_under_wrap(&terminated, wrap, WrapMode::Down),
                rows_under_wrap(&open, wrap, WrapMode::Down),
                "a trailing newline adds no row at wrap {wrap}"
            );
            assert_eq!(
                rows_under_wrap(&terminated, wrap, WrapMode::Back),
                rows_under_wrap(&open, wrap, WrapMode::Back),
                "and the same under WrapBack at wrap {wrap}"
            );
        }
        assert_eq!(
            rows_under_wrap(&terminated, 4, WrapMode::Down),
            2,
            "8 cells at wrap 4 is two rows"
        );
        assert_eq!(
            rows_under_wrap(&terminated, 4, WrapMode::Back),
            1,
            "and one row under WrapBack, with the fold spent in depth"
        );
    }
}
