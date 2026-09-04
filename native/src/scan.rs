//! Stage 3 of the reference port — the same fold as a segmented monoid scan.
//!
//! SOURCES. The oracle is `glyphPipelineScan.js` (313 lines); the working
//! reference is `engine/glyph_scan.mojo` plus the monoid itself, which lives in
//! `engine/glyph_bake.mojo` (`ScanElem`, `scan_combine`, `lanes_from_prefix`).
//!
//! WHY A SECOND FORM AT ALL. This is the GPU's dispatch structure:
//!
//!   chunk_reduce -> spine_reduce -> spine_scan -> partial_scan
//!                -> apply -> resolve_x -> paginate -> bounds
//!
//! The serial fold in `fold.rs` is a left-to-right recurrence and a GPU cannot
//! run one. Recasting it as an ASSOCIATIVE monoid is what makes it parallel, and
//! the price is that associativity has to be true rather than assumed. This port
//! is serial like `fold.rs` — but it keeps the chunk/group DECOMPOSITION, because
//! that decomposition is the thing under test. Sweeping chunk and group sizes and
//! getting identical integer lanes IS associativity checked in situ.
//!
//! ── THE PRECISION CONTRACT IS TIERED, AND THAT IS NOT A WEAKENING ────────────
//! Mirrors the repo's own comparator (`tools/scan-layout.test.mjs`,
//! `engine/conformance_scan.mojo`):
//!
//! | lanes | agreement with the serial fold |
//! |---|---|
//! | non-leader slots, every lane | BIT-equal |
//! | exact lanes (glyph_id, advance, height, row, col, flags, ord) | BIT-equal |
//! | `fold > 0` position lanes (X, Y, Z, BASE_X) | BIT-equal |
//! | foldless position lanes and LINE_ADV | <= 1e-4 RELATIVE |
//! | bounds: TOTAL_ROWS | exact |
//! | bounds: float lanes | <= 1e-4 relative (+-inf by bits) |
//! | leaders, misses, ordToByte | exact |
//!
//! The one tolerant row is tolerant BY CONSTRUCTION, not by resignation: the
//! serial fold's foldless X is an f64 prefix and the scan's is an f32 monoid
//! lane, so their GROUPING differs and their rounding must. Every INTEGER lane
//! is exact in both, and the `fold > 0` lanes are bit-equal because `resolve_x`
//! performs the same f32 additions in the same left-fold order as the serial
//! recurrence — it IS that re-sum, scheduled differently.

use crate::fold::{
    batch_union, bounds_range, decode_all, derive_stride, page_active, paginate, rows_for_line,
    wrap_row_of, wrap_segment_of, FoldResult, Item, Slots, WrapMode, F_LEADER, F_NEWLINE,
    F_RENDERED,
};
use crate::text::ResolveGlyph;

/// The GPU's tuning defaults. Both are dials the tests sweep.
pub const DEFAULT_CHUNK_SIZE: usize = 64;
pub const DEFAULT_GROUP_SIZE: usize = 256;

/// The segmented monoid's element.
///
/// `reset` ABSORBS: `combine(a, b) == b` whenever `b.reset` is set, which is what
/// makes file isolation structural rather than a bounds check. An item boundary
/// emits a resetting leaf, so no prefix can leak across it no matter how the
/// intervals were grouped.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScanElem {
    /// This interval begins at an item start; absorbs everything to its left.
    pub reset: i64,
    pub newlines: i64,
    pub glyphs: i64,
    /// Rows fully closed strictly inside this interval.
    pub rows: i64,
    /// Length of the (possibly partial) line this interval opens with.
    pub head_len: i64,
    /// Length of the still-open line this interval ends with.
    pub tail_len: i64,
    /// Advance sum of that still-open line.
    ///
    /// f32, AND THAT IS THE POINT: `f32 + f32` per combine IS the oracle's
    /// `Math.fround` chain. Widening it would be the same mistake as landmine 2,
    /// one form over.
    pub tail_advance: f32,
    /// The wrap width in force at the interval's right edge.
    pub wrap: i64,
    /// The wrap MODE in force at the interval's right edge.
    ///
    /// It rides beside `wrap` for the same reason and with the same caveat: the
    /// junction term reads it off `b`, so `combine` is not associative across a
    /// change of it. Two item-level parameters in that term now instead of one —
    /// the non-associative surface is WIDER than it was, not narrower.
    pub mode: WrapMode,
}

pub(crate) fn scan_identity() -> ScanElem {
    ScanElem::default()
}

/// One byte's monoid element, from its decoded facts alone.
pub(crate) fn scan_leaf_value(
    is_newline: bool,
    advance: f32,
    is_leader: bool,
    wrap: i64,
    is_item_start: bool,
    mode: WrapMode,
) -> ScanElem {
    let mut leaf = ScanElem {
        reset: i64::from(is_item_start),
        wrap,
        mode,
        ..ScanElem::default()
    };
    if !is_leader {
        return leaf; // continuation byte: reset/wrap/mode only
    }
    leaf.glyphs = 1;
    if is_newline {
        // head/tail stay 0: the line this closes started before the interval.
        leaf.newlines = 1;
    } else {
        leaf.head_len = 1;
        leaf.tail_len = 1;
        leaf.tail_advance = advance;
    }
    leaf
}

/// `combine(accumulator, next)` — the accumulator's interval followed by
/// `next`'s. Associative; `next.reset` absorbs.
pub(crate) fn scan_combine(accumulator: &mut ScanElem, next: &ScanElem) {
    if next.reset != 0 {
        *accumulator = *next;
        accumulator.reset = 1;
        return;
    }
    accumulator.wrap = next.wrap;
    accumulator.mode = next.mode;
    if next.newlines == 0 {
        accumulator.tail_len += next.tail_len;
        accumulator.tail_advance += next.tail_advance;
        if accumulator.newlines == 0 {
            // Still one open line across the whole interval: head IS tail.
            accumulator.head_len = accumulator.tail_len;
        }
    } else if accumulator.newlines == 0 {
        // The accumulator's open run extends `next`'s head line.
        accumulator.head_len += next.head_len;
        accumulator.rows = next.rows;
        accumulator.tail_len = next.tail_len;
        accumulator.tail_advance = next.tail_advance;
    } else {
        // THE JUNCTION LINE: the accumulator's tail plus next's head, closed by
        // next's first newline. This term is the whole reason the element needs
        // head_len and tail_len separately — a single length could not express a
        // line that straddles the seam.
        //
        // `next.wrap` is written for intent, but note it is not a CHOICE at this
        // point: `accumulator.wrap = next.wrap` ran above, before the branch, so
        // the two are already the same value and swapping them here is a no-op.
        // Verified as one — it was mutated and nothing, corpus or unit test,
        // could tell. Where the wrap genuinely matters is ACROSS groupings, and
        // that is `mixed_wrap_is_outside_the_monoid_s_domain`.
        accumulator.rows +=
            rows_for_line(accumulator.tail_len + next.head_len, next.wrap, next.mode) + next.rows;
        accumulator.tail_len = next.tail_len;
        accumulator.tail_advance = next.tail_advance;
    }
    accumulator.newlines += next.newlines;
    accumulator.glyphs += next.glyphs;
}

/// A leader's exact lanes from its EXCLUSIVE prefix — the O(1) query that
/// replaces walking the line.
pub(crate) struct PrefixLanes {
    pub row: i64,
    pub col: i64,
    pub line_advance: f32,
    pub ord: i64,
}

/// `terminator` is whether the byte being queried is itself a NEWLINE. It rides
/// at one-past-the-last cell of its line, so at an exact wrap multiple it stays
/// on the row it closes — the same distinction `fold::wrap_row_of` makes, which
/// is why both forms call that one function rather than each spelling the rule.
pub(crate) fn lanes_from_prefix(
    prefix: &ScanElem,
    wrap: i64,
    terminator: bool,
    mode: WrapMode,
) -> PrefixLanes {
    let col = prefix.tail_len;
    let closed = if prefix.newlines > 0 {
        rows_for_line(prefix.head_len, wrap, mode) + prefix.rows
    } else {
        0
    };
    PrefixLanes {
        row: closed + wrap_row_of(col, wrap, terminator, mode),
        col,
        line_advance: prefix.tail_advance,
        ord: prefix.glyphs,
    }
}

/// Which item owns byte `id`: the largest item whose `byte_start <= id`.
///
/// NOTE IT DOES NOT CHECK OWNERSHIP — a byte in a HOLE between two items
/// resolves to the preceding one. Callers must test containment separately; see
/// the gap guard in `apply_chunk`.
fn item_for_byte(items: &[Item], id: usize) -> usize {
    let mut low = 0usize;
    let mut high = items.len() - 1;
    while low < high {
        let mid = (low + high + 1) >> 1;
        if items[mid].byte_start <= id as i64 {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    low
}

/// Advance the item cursor past any boundaries `id` has crossed.
fn cursor_advance(items: &[Item], index: &mut usize, id: usize) {
    while *index + 1 < items.len() && id as i64 >= items[*index + 1].byte_start {
        *index += 1;
    }
}

/// The leaf for byte `id`, read from the decoded STATIC arrays only — which is
/// what lets chunk_reduce run before anything knows a byte's row or column.
fn scan_leaf(
    slots: &Slots,
    id: usize,
    wrap: i64,
    is_item_start: bool,
    mode: WrapMode,
) -> ScanElem {
    let flags = slots.flags(id);
    scan_leaf_value(
        flags & F_NEWLINE != 0,
        slots.advance(id),
        flags & F_LEADER != 0,
        wrap,
        is_item_start,
        mode,
    )
}

/// Serial fold of leaves over `[from_byte, to_byte)` — the body of chunk_reduce.
#[allow(clippy::too_many_arguments)]
fn fold_range(
    slots: &Slots,
    items: &[Item],
    wraps: &[i64],
    modes: &[WrapMode],
    from_byte: usize,
    to_byte: usize,
    accumulator: &mut ScanElem,
) {
    if from_byte >= to_byte {
        return;
    }
    let mut index = item_for_byte(items, from_byte);
    for id in from_byte..to_byte {
        cursor_advance(items, &mut index, id);
        let leaf = scan_leaf(
            slots,
            id,
            wraps[index],
            id as i64 == items[index].byte_start,
            modes[index],
        );
        scan_combine(accumulator, &leaf);
    }
}

/// APPLY — one chunk: seed from the chunk's exclusive prefix, then walk its
/// bytes writing the lanes the monoid can answer (row, col, ord, LINE_ADV) and
/// zeroing everything that is not a laid-out leader.
#[allow(clippy::too_many_arguments)]
fn apply_chunk(
    slots: &mut Slots,
    items: &[Item],
    wraps: &[i64],
    modes: &[WrapMode],
    chunk_prefix: &ScanElem,
    byte_len: usize,
    chunk_size: usize,
    chunk: usize,
) {
    let from_byte = chunk * chunk_size;
    let to_byte = ((chunk + 1) * chunk_size).min(byte_len);
    if from_byte >= to_byte {
        return;
    }
    let mut run = *chunk_prefix;
    let mut index = item_for_byte(items, from_byte);
    for id in from_byte..to_byte {
        cursor_advance(items, &mut index, id);
        let item = &items[index];
        let is_item_start = id as i64 == item.byte_start;
        if is_item_start {
            run = scan_identity();
            run.wrap = wraps[index];
            run.mode = modes[index];
        }
        let flags = slots.flags(id);
        // THE GAP GUARD. `item_for_byte` does not check ownership, so a byte in a
        // hole between two items resolves to the PRECEDING one. Without this
        // test the scan form lays those bytes out as if they belonged to it:
        // ROW/COL/ORD written, F_RENDERED set, the ordinal counter advanced
        // through the gap, and ord_to_byte written PAST the item's range.
        //
        // Measured in the Mojo port on a 3-item arena with a 100-byte hole and a
        // 200-byte tail: 300 count-lane disagreements against the serial form,
        // 300 gap bytes marked F_RENDERED, 300 ordToByte disagreements. The
        // serial `layout_item` cannot do this — it stops at the item's end.
        //
        // The TSL kernel has had this guard all along, so it was a PORT
        // divergence rather than a shared defect. No fixture has a gap, so it is
        // covered by a constructed test below instead.
        let byte_index = id as i64;
        let in_item =
            byte_index >= item.byte_start && byte_index < item.byte_start + item.byte_count;
        if flags & F_LEADER != 0 && in_item {
            let lanes =
                lanes_from_prefix(&run, wraps[index], flags & F_NEWLINE != 0, modes[index]);
            slots.lc[id * 2] = lanes.row as u32;
            slots.lc[id * 2 + 1] = lanes.col as u32;
            slots.fl[id] = flags | F_RENDERED;
            // LINE_ADV and ORD are pure projections of the monoid prefix. The
            // scan form ALWAYS fills the witness tier — resolve_x consumes ORD
            // and ordToByte to seed its shards, so it is load-bearing here
            // rather than optional.
            slots.wm[id] = lanes.line_advance;
            slots.wc[id] = lanes.ord as u32;
            slots.ord_to_byte[(item.byte_start + lanes.ord) as usize] = id as u32;
        } else {
            // THE SPLIT'S COVERAGE DUTY, scan form: every byte that is not a
            // laid-out leader — continuation bytes, gap bytes, and gap LEADERS
            // the guard above excludes — gets its zeros here. The chunks tile
            // [0, byte_len) so this walk visits every byte exactly once.
            slots.zero_positional(id);
            slots.wm[id] = 0.0;
            slots.wc[id] = 0;
        }
        let leaf = scan_leaf(slots, id, wraps[index], is_item_start, modes[index]);
        scan_combine(&mut run, &leaf);
    }
}

/// RESOLVE_X — the segment walk, amortized. One seed per shard, one f32 add per
/// leader after that.
///
/// The naive form re-derives each `fold > 0` leader's x by walking its
/// same-segment predecessors through ordToByte: O(fold) loads and adds PER
/// LEADER, ~50 at wrap 100. The open segment's partial sum cannot ride the
/// monoid — regrouping an f32 chain changes its value, the same reason
/// `tail_advance` is not a carried prefix — but nothing requires re-deriving it
/// per leader. This runs AFTER apply, so ordToByte is complete and the walk can
/// run ONCE to seed a serial accumulator.
///
/// BIT-EXACT BY CONSTRUCTION, not by tolerance: the recurrence performs the SAME
/// f32 additions in the SAME left-fold order as the per-leader re-sum, so every
/// x is bit-identical to the serial fold's. A carried sum in any other grouping
/// would diverge for a reason that is not a bug; this one cannot, because it IS
/// the re-sum, scheduled differently.
#[allow(clippy::too_many_arguments)]
fn resolve_x_shard(
    slots: &mut Slots,
    item: &Item,
    shard_scalars: &mut [f64],
    scalar_base: usize,
    start: usize,
    stop: usize,
    paged: bool,
) {
    let wrap = item.wrap_width;
    let fold_unit = if wrap > 0 {
        wrap
    } else if item.has_page {
        item.page_cols
    } else {
        0
    };
    let line_height = item.line_height;
    let mut segment_advance: f32 = 0.0;
    let mut seeded = false;
    let mut row_max = shard_scalars[scalar_base + 6];
    let mut x_max = shard_scalars[scalar_base + 7];

    for id in start..stop {
        if slots.flags(id) & F_LEADER == 0 {
            continue;
        }
        let col = slots.col(id);
        let item_relative_x: f64;
        if fold_unit > 0 {
            if col % fold_unit == 0 {
                segment_advance = 0.0; // segment start: empty prefix, no walk
                seeded = true;
            } else if !seeded {
                // THE ONE WALK: this shard begins mid-segment, so re-derive the
                // open prefix exactly as the per-leader form would — forward,
                // one f32 add at a time. Sharding differently is what exercises
                // this branch, which is why the shard count is a swept dial.
                let mut walked: f32 = 0.0;
                let ord = slots.wc[id] as i64;
                let mut back = col % fold_unit;
                while back >= 1 {
                    let predecessor =
                        slots.ord_to_byte[(item.byte_start + ord - back) as usize] as usize;
                    walked += slots.advance(predecessor);
                    back -= 1;
                }
                segment_advance = walked;
                seeded = true;
            }
            item_relative_x = segment_advance as f64;
            segment_advance += slots.advance(id);
        } else {
            // The foldless prefix comes from the monoid's f32 lane, where the
            // serial fold used an f64 accumulator. This is the one tiered lane.
            item_relative_x = slots.wm[id] as f64;
        }
        let row = slots.row(id);
        slots.set_base_x(id, (item_relative_x + item.origin_x) as f32);
        if !paged {
            // A PAGE-ACTIVE ITEM SKIPS X/Y/Z ENTIRELY. paginate's per-byte gate
            // is the exact complement of `page_active` on the same item-level
            // params, so for a page-active item paginate writes X/Y/Z for every
            // leader — these three stores were dead the moment it ran. paginate
            // reads only BASE_X/ROW/COL, all still written.
            // The DEPTH fan's segment index, never the row contribution: under
            // WrapBack the row is flat and the segments are all z has left.
            let wrap_segment = wrap_segment_of(col, wrap, slots.flags(id) & F_NEWLINE != 0);
            slots.set_position(
                id,
                (item_relative_x + item.origin_x) as f32,
                (-(row as f64) * line_height + item.origin_y) as f32,
                (-(wrap_segment as f64) * item.z_step + item.origin_z) as f32,
            );
        }
        if (row + 1) as f64 > row_max {
            row_max = (row + 1) as f64;
        }
        if item_relative_x > x_max {
            x_max = item_relative_x;
        }
    }
    shard_scalars[scalar_base + 6] = row_max;
    shard_scalars[scalar_base + 7] = x_max;
}

/// Start of contiguous shard `which` of `[start, stop)` split `shards` ways.
fn shard_lo(start: usize, stop: usize, shards: usize, which: usize) -> usize {
    let per = (stop - start).div_ceil(shards);
    (start + which * per).min(stop)
}

/// Run the pipeline by the scan — same inputs and outputs as
/// `fold::run_pipeline`, computed in the GPU's dispatch structure.
///
/// `chunk_size`, `group_size` and `shards` are the tuning dials. Invariance
/// across them is associativity in situ, which is what the sweep in the tests
/// below actually checks.
pub fn run_scan_pipeline<T: ResolveGlyph + ?Sized>(
    bytes: &[u8],
    trie: &T,
    items: &[Item],
    chunk_size: usize,
    group_size: usize,
    shards: usize,
) -> FoldResult {
    let chunk_size = chunk_size.max(1);
    let group_size = group_size.max(1);
    let shards = shards.max(1);
    let byte_len = bytes.len();
    let mut slots = Slots::new(byte_len);

    // ── dispatch 1: decode (the same kernel the serial form runs) ─────────────
    let (misses, leaders) = decode_all(bytes, &mut slots, trie);
    if items.is_empty() {
        return FoldResult {
            slots,
            misses,
            leaders,
            item_bounds: Vec::new(),
            batch_bounds: vec![
                f64::INFINITY,
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
                0.0,
                0.0,
            ],
        };
    }
    let wraps: Vec<i64> = items.iter().map(|item| item.wrap_width).collect();
    let modes: Vec<WrapMode> = items.iter().map(|item| item.wrap_mode).collect();

    // ── dispatch 2: chunk_reduce — one element per chunk ──────────────────────
    let num_chunks = byte_len.div_ceil(chunk_size);
    let mut partials = vec![scan_identity(); num_chunks];
    for (chunk, partial) in partials.iter_mut().enumerate() {
        let from_byte = chunk * chunk_size;
        let to_byte = (from_byte + chunk_size).min(byte_len);
        let mut accumulator = scan_identity();
        fold_range(&slots, items, &wraps, &modes, from_byte, to_byte, &mut accumulator);
        *partial = accumulator;
    }

    // ── dispatch 3: spine_reduce — one element per group of chunks ────────────
    let num_supers = num_chunks.div_ceil(group_size);
    let mut supers = vec![scan_identity(); num_supers];
    for (super_group, super_elem) in supers.iter_mut().enumerate() {
        let mut accumulator = scan_identity();
        let first = super_group * group_size;
        let last = (first + group_size).min(num_chunks);
        for partial in &partials[first..last] {
            scan_combine(&mut accumulator, partial);
        }
        *super_elem = accumulator;
    }

    // ── dispatch 4: spine_scan — ONE thread, exclusive scan of the supers ─────
    // Serial on hardware too. The spine is the only sequential dependency left,
    // and it is num_chunks/group_size long rather than byte_len.
    let mut super_prefix = vec![scan_identity(); num_supers];
    let mut spine = scan_identity();
    for (prefix, super_elem) in super_prefix.iter_mut().zip(supers.iter()) {
        *prefix = spine;
        scan_combine(&mut spine, super_elem);
    }

    // ── dispatch 5: partial_scan — each chunk's exclusive prefix ──────────────
    let mut chunk_prefix = vec![scan_identity(); num_chunks];
    for (super_group, &group_prefix) in super_prefix.iter().enumerate() {
        let mut accumulator = group_prefix;
        let first = super_group * group_size;
        let last = (first + group_size).min(num_chunks);
        for (prefix, partial) in chunk_prefix[first..last]
            .iter_mut()
            .zip(&partials[first..last])
        {
            *prefix = accumulator;
            scan_combine(&mut accumulator, partial);
        }
    }

    // ── dispatch 6: apply ─────────────────────────────────────────────────────
    // One iteration per chunk IS one GPU thread; the index is the thread id.
    for (chunk, prefix) in chunk_prefix.iter().enumerate() {
        apply_chunk(&mut slots, items, &wraps, &modes, prefix, byte_len, chunk_size, chunk);
    }

    // ── dispatch 7: resolve_x + the fold-scalar reduce ────────────────────────
    // Per-shard scalar rows, max-merged — exact under regrouping, unlike a
    // shared read-compare-write.
    let mut item_bounds = vec![0.0f64; items.len() * 8];
    for (index, item) in items.iter().enumerate() {
        let start = item.byte_start as usize;
        let stop = (item.byte_start + item.byte_count) as usize;
        let mut shard_scalars = vec![0.0f64; shards * 8];
        for which in 0..shards {
            resolve_x_shard(
                &mut slots,
                item,
                &mut shard_scalars,
                which * 8,
                shard_lo(start, stop, shards, which),
                shard_lo(start, stop, shards, which + 1),
                page_active(item),
            );
        }
        for which in 0..shards {
            if shard_scalars[which * 8 + 6] > item_bounds[index * 8 + 6] {
                item_bounds[index * 8 + 6] = shard_scalars[which * 8 + 6];
            }
            if shard_scalars[which * 8 + 7] > item_bounds[index * 8 + 7] {
                item_bounds[index * 8 + 7] = shard_scalars[which * 8 + 7];
            }
        }
    }

    // ── dispatch 8: paginate the active items ─────────────────────────────────
    for (index, item) in items.iter().enumerate() {
        if !page_active(item) {
            continue;
        }
        let stride = derive_stride(item_bounds[index * 8 + 7], item);
        let start = item.byte_start as usize;
        let stop = (item.byte_start + item.byte_count) as usize;
        for id in start..stop {
            paginate(&mut slots, id, item, stride);
        }
    }

    // ── dispatch 9: per-item boxes, then the batch union ──────────────────────
    // Unlike the serial form, EVERY item needs this pass: the scan never
    // accumulated a box during its walk (apply has no position yet, and
    // resolve_x owns positions but not row/col ordering), so there is no
    // "already computed by the fold" case to skip.
    for (index, item) in items.iter().enumerate() {
        let start = item.byte_start as usize;
        let stop = (item.byte_start + item.byte_count) as usize;
        let box_lanes = bounds_range(&slots, start, stop);
        item_bounds[index * 8..index * 8 + 6].copy_from_slice(&box_lanes);
    }

    let batch_bounds = batch_union(&item_bounds, items.len());

    FoldResult {
        slots,
        misses,
        leaders,
        item_bounds,
        batch_bounds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold::run_pipeline;
    use crate::glyph_trie::{build_glyph_trie, BuiltTrie, GlyphMetrics};

    fn trie() -> BuiltTrie {
        build_glyph_trie(
            (0x20u32..0x7Fu32).chain(std::iter::once(0x0A)),
            // Awkward mantissas, so an f32 re-sum is a real test and not a
            // sequence of exactly-representable halves.
            |cp| {
                Some(GlyphMetrics {
                    glyph_id: cp + 1,
                    advance: 0.6 + (cp % 13) as f32 * 0.0173,
                    height: 1.2,
                })
            },
            0.61,
            1.25,
        )
    }

    /// A deterministic pseudo-random element generator. Real elements, reachable
    /// by folding real leaves — a hand-built ScanElem could violate invariants
    /// the monoid relies on and "disprove" associativity for the wrong reason.
    /// `wrap` AND `mode` ARE UNIFORM ACROSS THE SAMPLE, and that is a
    /// precondition rather than a convenience — see
    /// `mixed_wrap_is_outside_the_monoid_s_domain` and its mode twin.
    fn elements(count: usize, wrap: i64, mode: WrapMode) -> Vec<ScanElem> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let mut out = Vec::new();
        for _ in 0..count {
            let mut accumulator = scan_identity();
            let len = 1 + (next() % 9) as usize;
            for _ in 0..len {
                let roll = next() % 10;
                let leaf = scan_leaf_value(
                    roll < 3,                             // is_newline
                    0.6 + (next() % 17) as f32 * 0.0173,  // advance
                    roll != 9,                            // is_leader
                    wrap,
                    false,
                    mode,
                );
                scan_combine(&mut accumulator, &leaf);
            }
            out.push(accumulator);
        }
        out
    }

    /// THE MATHEMATICAL HEART OF THE SCAN FORM, and it is deliberately TIERED —
    /// the same fact the fixture comparison's tolerant row reports, stated at
    /// the source.
    ///
    /// Every INTEGER field is exactly associative WITHIN ONE WRAP REGIME, and
    /// that is what licenses regrouping the intervals: row, col and ord cannot
    /// depend on how the buffer was chunked. `tail_advance` is f32, and f32
    /// addition is not associative, so regrouping genuinely moves it. That is
    /// not a defect to be fixed but the reason the contract has two tiers at
    /// all — and the reason `resolve_x` re-sums rather than carrying the
    /// monoid's value forward.
    #[test]
    fn the_monoid_is_associative_on_every_integer_field() {
        let mut checked = 0usize;
        let mut advance_differed = 0usize;
        // TWELVE regimes: six wraps CROSSED WITH both modes.
        //
        // Six wraps, not four: `elements` builds lines of 1..9 cells, so wraps 1,
        // 2 and 4 DIVIDE some of them and wraps 3 and 7 mostly do not. The
        // corrected `rows_for_line` changes value exactly on the multiples, so a
        // sweep that never hit one would be testing associativity of the old rule
        // in the new rule's clothing.
        //
        // Both modes, because `rows_for_line` is a DIFFERENT FUNCTION under each
        // and sweeping only WrapDown would say nothing about WrapBack's monoid.
        // That the cross actually discriminates is asserted below, not assumed —
        // the same lesson as the wrap widening, one parameter over.
        let mut rows_by_regime: Vec<(i64, WrapMode, Vec<i64>)> = Vec::new();
        for wrap in [0i64, 1, 2, 3, 4, 7] {
        for mode in [WrapMode::Down, WrapMode::Back] {
        let mut regime_rows: Vec<i64> = Vec::new();
        let sample = elements(60, wrap, mode);
        for a in sample.iter().take(15) {
            for b in sample.iter().skip(15).take(15) {
                for c in sample.iter().skip(30).take(15) {
                    // (a . b) . c
                    let mut left = *a;
                    scan_combine(&mut left, b);
                    scan_combine(&mut left, c);
                    // a . (b . c)
                    let mut right_inner = *b;
                    scan_combine(&mut right_inner, c);
                    let mut right = *a;
                    scan_combine(&mut right, &right_inner);

                    assert_eq!(left.reset, right.reset, "reset");
                    assert_eq!(left.newlines, right.newlines, "newlines");
                    assert_eq!(left.glyphs, right.glyphs, "glyphs");
                    assert_eq!(left.rows, right.rows, "rows");
                    assert_eq!(left.head_len, right.head_len, "head_len");
                    assert_eq!(left.tail_len, right.tail_len, "tail_len");
                    assert_eq!(left.wrap, right.wrap, "wrap");
                    assert_eq!(left.mode, right.mode, "mode");
                    regime_rows.push(left.rows);
                    let delta = (left.tail_advance - right.tail_advance).abs();
                    assert!(
                        delta <= 1e-4 * left.tail_advance.abs().max(1.0),
                        "tail_advance may reround under regrouping, not diverge: \
                         {} vs {}",
                        left.tail_advance,
                        right.tail_advance
                    );
                    if left.tail_advance.to_bits() != right.tail_advance.to_bits() {
                        advance_differed += 1;
                    }
                    checked += 1;
                }
            }
        }
        rows_by_regime.push((wrap, mode, regime_rows));
        }
        }
        assert_eq!(checked, 6 * 2 * 15 * 15 * 15, "the triple sweep must actually run");
        assert_eq!(rows_by_regime.len(), 12, "twelve regimes: six wraps x two modes");
        // THE REGIMES MUST DISCRIMINATE. A cross that swept the same rule twice
        // under two names would pass this test while proving associativity of one
        // mode only — exactly the failure the wrap widening was aimed at, so it is
        // MEASURED here rather than argued. At every wrap that can fold (> 1),
        // the two modes must produce different `rows` on this sample.
        for wrap in [1i64, 2, 3, 4, 7] {
            let down = &rows_by_regime.iter().find(|r| r.0 == wrap && r.1 == WrapMode::Down).unwrap().2;
            let back = &rows_by_regime.iter().find(|r| r.0 == wrap && r.1 == WrapMode::Back).unwrap().2;
            // Compared as a COUNT of differing triples, not with assert_ne! on the
            // vectors: 3,375 rows each, and a failure that dumps both is unreadable
            // exactly when someone needs to read it.
            let differing = down.iter().zip(back.iter()).filter(|(a, b)| a != b).count();
            assert!(
                differing > 0,
                "wrap {wrap}: the two modes agreed on all {} triples — this regime \
                 cannot tell the modes apart and the cross is decorative there",
                down.len()
            );
        }
        // And wrap 0 must AGREE, because there is no fold to spend: a mode that
        // changed something at wrap 0 would be reaching past its own definition.
        {
            let down = &rows_by_regime.iter().find(|r| r.0 == 0 && r.1 == WrapMode::Down).unwrap().2;
            let back = &rows_by_regime.iter().find(|r| r.0 == 0 && r.1 == WrapMode::Back).unwrap().2;
            let differing = down.iter().zip(back.iter()).filter(|(a, b)| a != b).count();
            assert_eq!(
                differing, 0,
                "at wrap 0 nothing wraps, so the modes must coincide — {differing} triples differ"
            );
        }
        // ANTI-VACUITY on the claim itself. If regrouping never moved
        // tail_advance on this sample, the test would be silently asserting
        // something stronger than the contract, and the tiered comparison
        // elsewhere would look like unnecessary slack.
        assert!(
            advance_differed > 0,
            "no regrouping moved tail_advance — this sample cannot show why the \
             contract needs a tolerant tier"
        );
    }

    /// A hand-built element for the DOMAIN tests. Deliberately not from
    /// `elements()`: those two tests need a triple with exactly the shape that
    /// reaches the junction term (`a.nl > 0, b.nl > 0, c.nl == 0`), and a random
    /// sample cannot be asked for one.
    #[allow(clippy::too_many_arguments)]
    fn domain_elem(
        newlines: i64, glyphs: i64, rows: i64, head: i64, tail: i64, wrap: i64, mode: WrapMode,
    ) -> ScanElem {
        ScanElem {
            reset: 0, newlines, glyphs, rows, head_len: head, tail_len: tail,
            tail_advance: 0.0, wrap, mode,
        }
    }

    fn group_left_rows(x: &ScanElem, y: &ScanElem, z: &ScanElem) -> i64 {
        let mut acc = *x;
        scan_combine(&mut acc, y);
        scan_combine(&mut acc, z);
        acc.rows
    }

    fn group_right_rows(x: &ScanElem, y: &ScanElem, z: &ScanElem) -> i64 {
        let mut inner = *y;
        scan_combine(&mut inner, z);
        let mut acc = *x;
        scan_combine(&mut acc, &inner);
        acc.rows
    }

    /// `reset` absorbs: an item boundary makes file isolation STRUCTURAL, so no
    /// prefix can leak across it however the intervals were grouped.
    /// THE MONOID'S DOMAIN HAS A PRECONDITION, and it is worth pinning because
    /// nothing in the reference states it and the parallel scan silently depends
    /// on it.
    ///
    /// `combine` is NOT associative across a change of `wrap`. The junction term
    /// is `rows_for_line(a.tail_len + b.head_len, b.wrap)`, and for
    /// `a.nl > 0, b.nl > 0, c.nl == 0` the left grouping evaluates it with
    /// `b.wrap` while the right evaluates it with `c.wrap`. Measured by
    /// exhaustive search over reachable elements: **0 non-associative triples in
    /// 2,370,816 under a uniform wrap**, and this counterexample the moment wrap
    /// varies.
    ///
    /// It is safe anyway, STRUCTURALLY: `wrap` is an item-level parameter, and
    /// every item boundary emits a resetting leaf that absorbs whatever preceded
    /// it. So two elements with different wraps and no reset between them cannot
    /// arise from any real buffer. This test exists so that stops being an
    /// accident — if wrap ever becomes per-line or per-range, the scan form's
    /// regrouping freedom goes with it, and this is where that shows up.
    #[test]
    fn mixed_wrap_is_outside_the_monoid_s_domain() {
        let a = domain_elem(1, 3, 1, 1, 2, 5, WrapMode::Down);
        let b = domain_elem(1, 4, 1, 3, 1, 5, WrapMode::Down);
        let c_mixed = domain_elem(0, 2, 0, 2, 2, 2, WrapMode::Down); // wrap 2, not 5
        let c_uniform = domain_elem(0, 2, 0, 2, 2, 5, WrapMode::Down);

        assert_ne!(
            group_left_rows(&a, &b, &c_mixed),
            group_right_rows(&a, &b, &c_mixed),
            "a change of wrap with no reset between MUST break associativity — \
             if this ever passes, the precondition has moved and the scan form's \
             regrouping freedom needs re-deriving"
        );
        assert_eq!(
            group_left_rows(&a, &b, &c_uniform),
            group_right_rows(&a, &b, &c_uniform),
            "and under one wrap regime it must hold exactly"
        );
    }

    /// THE MODE TWIN of the test above, and the reason it has to exist.
    ///
    /// The junction term is `rows_for_line(a.tail_len + b.head_len, b.wrap,
    /// b.mode)`. It reads TWO fields of `b` now instead of one, so the surface on
    /// which `combine` is non-associative got WIDER when the mode landed, not
    /// narrower: a triple with `a.nl > 0, b.nl > 0, c.nl == 0` evaluates that term
    /// with `b`'s parameters under the left grouping and `c`'s under the right, and
    /// EITHER parameter differing is enough to split them.
    ///
    /// MEASURED, not argued. Same wrap throughout — 2 — so the only difference
    /// between `c_mixed` and `c_uniform` is the mode, and the junction line is
    /// 5 cells: 3 rows under WrapDown, 1 under WrapBack.
    ///
    /// It is safe anyway, and safe in exactly the way wrap is: mode is an
    /// ITEM-LEVEL parameter, every item boundary emits a resetting leaf, and a
    /// reset absorbs whatever preceded it. So no interval without a reset can span
    /// two modes, and `mixed_modes_across_an_item_boundary_agree_with_the_serial_fold`
    /// is the constructive half of that claim. If mode ever becomes per-line or
    /// per-range, the scan form's regrouping freedom goes with it and this is where
    /// that shows up.
    #[test]
    fn mixed_mode_is_outside_the_monoid_s_domain() {
        let a = domain_elem(1, 3, 1, 1, 2, 2, WrapMode::Down);
        let b = domain_elem(1, 4, 1, 3, 1, 2, WrapMode::Down);
        let c_mixed = domain_elem(0, 2, 0, 2, 2, 2, WrapMode::Back); // mode Back, not Down
        let c_uniform = domain_elem(0, 2, 0, 2, 2, 2, WrapMode::Down);

        // The junction line is a.tail_len + b.head_len = 5 cells at wrap 2.
        assert_eq!(crate::fold::rows_for_line(5, 2, WrapMode::Down), 3);
        assert_eq!(crate::fold::rows_for_line(5, 2, WrapMode::Back), 1);

        let left = group_left_rows(&a, &b, &c_mixed);
        let right = group_right_rows(&a, &b, &c_mixed);
        assert_ne!(
            left, right,
            "a change of MODE with no reset between MUST break associativity — \
             mode joined wrap in the junction term, so the domain got NARROWER, \
             not wider. Measured: {left} vs {right}"
        );
        // The exact counterexample, pinned. a.rows + b.rows is 2 either way; the
        // junction contributes 3 under the left grouping (evaluated with b's
        // WrapDown) and 1 under the right (evaluated with c's WrapBack).
        assert_eq!((left, right), (5, 3), "the measured counterexample");
        assert_eq!(
            group_left_rows(&a, &b, &c_uniform),
            group_right_rows(&a, &b, &c_uniform),
            "and under one mode it must hold exactly"
        );
    }

    /// THE CONSTRUCTIVE HALF. Two items in one arena with DIFFERENT modes, run
    /// through the scan form at chunk/group/shard tunings chosen so intervals
    /// straddle the boundary — and it agrees with the serial fold on every exact
    /// lane at every tuning, because the boundary's resetting leaf absorbs the
    /// prefix before a mode can leak across it.
    ///
    /// This is what would go red if item isolation were ever weakened, and it is a
    /// different claim from the corpus's `wrapback-items` fixture: that one gates
    /// the ANSWER, this one gates the REGROUPING.
    #[test]
    fn mixed_modes_across_an_item_boundary_agree_with_the_serial_fold() {
        let t = trie();
        // Lines of 6 cells at wrap 3: two rows each in item A (WrapDown), one row
        // each in item B (WrapBack).
        let bytes: Vec<u8> =
            (0..180).map(|i| if i % 7 == 6 { b'\n' } else { b'a' + (i % 20) as u8 }).collect();
        let items = [
            Item {
                byte_start: 0, byte_count: 90, wrap_width: 3, wrap_mode: WrapMode::Down,
                z_step: 0.2, line_height: 1.0, ..Item::default()
            },
            Item {
                byte_start: 90, byte_count: 90, wrap_width: 3, wrap_mode: WrapMode::Back,
                origin_y: 4.0, z_step: 0.2, line_height: 1.0, ..Item::default()
            },
        ];
        let serial = run_pipeline(&bytes, &t, &items);
        // ANTI-VACUITY: the two items must actually lay out differently, or the
        // agreement below is about one mode twice.
        assert!(
            serial.item_bounds[6] > serial.item_bounds[8 + 6],
            "item A (WrapDown) must occupy more rows than item B (WrapBack): {} vs {}",
            serial.item_bounds[6], serial.item_bounds[8 + 6]
        );
        for &(chunk, group, shards) in
            &[(64usize, 256usize, 1usize), (7, 3, 3), (1, 1, 8), (91, 2, 4), (13, 5, 11)]
        {
            let scanned = run_scan_pipeline(&bytes, &t, &items, chunk, group, shards);
            for id in 0..bytes.len() {
                let where_ = format!("byte {id} at K={chunk}/G={group}/S={shards}");
                assert_eq!(scanned.slots.lc[id * 2], serial.slots.lc[id * 2], "ROW, {where_}");
                assert_eq!(
                    scanned.slots.lc[id * 2 + 1], serial.slots.lc[id * 2 + 1], "COL, {where_}"
                );
                assert_eq!(scanned.slots.wc[id], serial.slots.wc[id], "ORD, {where_}");
                assert_eq!(scanned.slots.fl[id], serial.slots.fl[id], "FLAGS, {where_}");
                assert_eq!(
                    scanned.slots.z(id).to_bits(), serial.slots.z(id).to_bits(), "Z, {where_}"
                );
                assert_eq!(
                    scanned.slots.y(id).to_bits(), serial.slots.y(id).to_bits(), "Y, {where_}"
                );
            }
            assert_eq!(scanned.slots.ord_to_byte, serial.slots.ord_to_byte, "ordToByte");
        }
    }

    #[test]
    fn reset_absorbs_everything_to_its_left() {
        let sample = elements(8, 4, WrapMode::Down);
        let mut boundary = sample[3];
        boundary.reset = 1;
        for left in &sample {
            let mut combined = *left;
            scan_combine(&mut combined, &boundary);
            let mut expected = boundary;
            expected.reset = 1;
            assert_eq!(combined, expected, "combine(a, b) must equal b when b.reset");
        }
    }

    /// THE GAP GUARD — and no fixture can check it, because no fixture has a
    /// hole between items.
    ///
    /// `item_for_byte` returns the largest item whose start <= id and does NOT
    /// check ownership, so a gap byte resolves to the PRECEDING item. Without
    /// the containment test in `apply_chunk` the scan form lays those bytes out
    /// as if they belonged to it — and the serial fold cannot, because it stops
    /// at each item's end. That is a port divergence rather than a shared
    /// defect, which makes it exactly the kind the corpus is blind to.
    #[test]
    fn the_scan_agrees_with_the_serial_fold_across_a_gap() {
        let t = trie();
        let bytes: Vec<u8> = (0..40).map(|i| if i % 7 == 6 { b'\n' } else { b'a' + (i % 20) as u8 }).collect();
        let items = [
            Item { byte_start: 0, byte_count: 10, line_height: 1.0, ..Item::default() },
            // bytes 10..25 belong to NO item — the hole.
            Item { byte_start: 25, byte_count: 15, origin_y: 4.0, line_height: 1.0, ..Item::default() },
        ];
        let serial = run_pipeline(&bytes, &t, &items);
        for &(chunk, group, shards) in &[(64usize, 256usize, 1usize), (3, 2, 4), (1, 1, 3)] {
            let scanned = run_scan_pipeline(&bytes, &t, &items, chunk, group, shards);
            for id in 0..bytes.len() {
                assert_eq!(
                    (scanned.slots.lc[id * 2], scanned.slots.lc[id * 2 + 1], scanned.slots.fl[id], scanned.slots.wc[id]),
                    (serial.slots.lc[id * 2], serial.slots.lc[id * 2 + 1], serial.slots.fl[id], serial.slots.wc[id]),
                    "byte {id} at K={chunk}/G={group}/S={shards}"
                );
            }
            assert_eq!(
                scanned.slots.ord_to_byte, serial.slots.ord_to_byte,
                "ordToByte at K={chunk}/G={group}/S={shards}"
            );
        }
        // ANTI-VACUITY: the hole must really contain leaders, or the guard was
        // never asked anything.
        let gap_leaders = (10..25)
            .filter(|&id| serial.slots.flags(id) & F_LEADER != 0)
            .count();
        assert!(gap_leaders > 0, "the gap must contain decoded leaders");
        // And those leaders must NOT be marked rendered by either form.
        for id in 10..25 {
            assert_eq!(
                serial.slots.flags(id) & F_RENDERED, 0,
                "gap byte {id} must not be laid out"
            );
        }
    }

    /// Tuning invariance on a WRAPPED, PAGED arena — associativity in situ,
    /// against the serial fold rather than against the corpus.
    #[test]
    fn scan_and_serial_agree_across_tunings_on_a_wrapped_paged_item() {
        let t = trie();
        let bytes: Vec<u8> =
            (0..300).map(|i| if i % 11 == 10 { b'\n' } else { b'!' + (i % 60) as u8 }).collect();
        let items = [Item {
            byte_start: 0,
            byte_count: 300,
            origin_x: 0.25,
            origin_y: -1.0,
            origin_z: 0.5,
            wrap_width: 7,
            wrap_mode: WrapMode::Down,
            z_step: 0.1,
            line_height: 1.1,
            has_page: true,
            page_rows: 5,
            page_cols: 3,
            scroll_rows: 2,
            pages_wide: 2,
            page_gap_x: 0.75,
            band_stride_y: 2.0,
            depth_per_band: 0.3,
            depth_per_col: 0.05,
            page_line_height: f64::NAN,
        }];
        let serial = run_pipeline(&bytes, &t, &items);
        assert!(page_active(&items[0]) && items[0].wrap_width > 0, "paged AND wrapped");
        for &(chunk, group, shards) in
            &[(64usize, 256usize, 1usize), (7, 3, 3), (1, 1, 8), (4096, 8, 5), (13, 5, 11)]
        {
            let scanned = run_scan_pipeline(&bytes, &t, &items, chunk, group, shards);
            for id in 0..bytes.len() {
                let where_ = format!("byte {id} at K={chunk}/G={group}/S={shards}");
                // Integer lanes: EXACT, at every tuning. This is the claim.
                assert_eq!(scanned.slots.lc[id * 2], serial.slots.lc[id * 2], "ROW, {where_}");
                assert_eq!(scanned.slots.lc[id * 2 + 1], serial.slots.lc[id * 2 + 1], "COL, {where_}");
                assert_eq!(scanned.slots.wc[id], serial.slots.wc[id], "ORD, {where_}");
                assert_eq!(scanned.slots.fl[id], serial.slots.fl[id], "FLAGS, {where_}");
                // fold > 0 here, so the positions are BIT-equal too.
                assert_eq!(
                    scanned.slots.x(id).to_bits(), serial.slots.x(id).to_bits(),
                    "X, {where_}"
                );
                assert_eq!(
                    scanned.slots.base_x(id).to_bits(), serial.slots.base_x(id).to_bits(),
                    "BASE_X, {where_}"
                );
            }
        }
    }

    /// THE PHANTOM ROW, in the scan form (2026-09-04).
    ///
    /// `scan_and_serial_agree_across_tunings_on_a_wrapped_paged_item` uses lines
    /// of 10 cells at wrap 7 — never an exact multiple, so it could not see this
    /// at all. Here every line is exactly 2 * wrap, which is the defect's whole
    /// domain, and the item is UNPAGED so that the fold's own Z (not paginate's)
    /// is the one under comparison.
    ///
    /// Two claims at once: the scan agrees with the serial fold, AND both of
    /// them agree with rows counted by hand.
    #[test]
    fn scan_and_serial_agree_on_exact_multiple_lines() {
        let t = trie();
        // 12 lines of 8 cells each, wrap 4: line k occupies rows 2k and 2k+1.
        let mut bytes: Vec<u8> = Vec::new();
        for line in 0..12u8 {
            bytes.extend_from_slice(&[b'a' + line; 8]);
            bytes.push(b'\n');
        }
        let items = [Item {
            byte_start: 0,
            byte_count: bytes.len() as i64,
            origin_x: 0.25,
            origin_y: -1.0,
            origin_z: 0.5,
            wrap_width: 4,
            z_step: 0.1,
            line_height: 1.1,
            ..Item::default()
        }];
        let serial = run_pipeline(&bytes, &t, &items);
        assert!(!page_active(&items[0]), "unpaged, so the fold writes Z itself");

        // Counted by hand, not derived from the code under test.
        for line in 0..12usize {
            let start = line * 9;
            assert_eq!(serial.slots.row(start), (line * 2) as i64, "line {line} base row");
            assert_eq!(serial.slots.row(start + 7), (line * 2 + 1) as i64, "line {line} last glyph");
            assert_eq!(
                serial.slots.row(start + 8),
                (line * 2 + 1) as i64,
                "line {line}'s newline rides the row it closes"
            );
        }
        assert_eq!(serial.item_bounds[6], 24.0, "TOTAL_ROWS = 12 lines * 2 rows");

        for &(chunk, group, shards) in
            &[(64usize, 256usize, 1usize), (9, 3, 3), (1, 1, 8), (4096, 8, 5), (13, 5, 11)]
        {
            let scanned = run_scan_pipeline(&bytes, &t, &items, chunk, group, shards);
            for id in 0..bytes.len() {
                let where_ = format!("byte {id} at K={chunk}/G={group}/S={shards}");
                assert_eq!(scanned.slots.lc[id * 2], serial.slots.lc[id * 2], "ROW, {where_}");
                assert_eq!(scanned.slots.lc[id * 2 + 1], serial.slots.lc[id * 2 + 1], "COL, {where_}");
                assert_eq!(scanned.slots.wc[id], serial.slots.wc[id], "ORD, {where_}");
                assert_eq!(scanned.slots.fl[id], serial.slots.fl[id], "FLAGS, {where_}");
                // Z is the lane the terminator rule reaches through wrap_row.
                assert_eq!(
                    scanned.slots.z(id).to_bits(), serial.slots.z(id).to_bits(),
                    "Z, {where_}"
                );
                assert_eq!(
                    scanned.slots.y(id).to_bits(), serial.slots.y(id).to_bits(),
                    "Y, {where_}"
                );
            }
            assert_eq!(scanned.item_bounds, serial.item_bounds, "bounds at K={chunk}/G={group}/S={shards}");
        }
    }
}
