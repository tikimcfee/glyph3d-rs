//! The host side of the Visible field's tables: the `repr(C)` records the
//! kernels read (sizes pinned against the WGSL by `tests/wgsl.rs`), and the
//! pure functions that build them from [`crate::VisibleInputs`] — the
//! packed trie, the extended item table, the span table with its per-line
//! first-span index, the byte-chunk placement — plus the frame's frustum
//! planes and the dispatch plan. Nothing here touches a device, so every
//! piece has a unit test.

use bytemuck::{Pod, Zeroable};

use crate::{ByteSpanGpu, LineEntryGpu, TrieUpload, VisibleItem};

/// Workgroup size of every per-element kernel.
pub const WORKGROUP: u32 = 64;
/// wgpu's default `max_compute_workgroups_per_dimension`.
pub const MAX_GROUPS_X: u32 = 65_535;
/// Words of the counters buffer (`visible_cull.wgsl`'s `counters`).
pub const COUNTER_WORDS: usize = 16;
/// Words of the indirect buffer: cull-B dispatch at 0, layout dispatch at
/// 4, the glyph draw's args at 8, the wash draw's at 16 (byte offset × 4).
pub const INDIRECT_WORDS: usize = 24;
pub const INDIRECT_CULL_B: u64 = 0;
pub const INDIRECT_LAYOUT: u64 = 16;
pub const INDIRECT_DRAW: u64 = 32;
pub const INDIRECT_WASH_DRAW: u64 = 64;

/// Counter slots, as the cull shader names them.
pub mod counter {
    pub const ITEMS_VISIBLE: usize = 0;
    pub const ITEMS_BACKDROP: usize = 1;
    pub const LINES_CANDIDATE: usize = 2;
    pub const LINES_GLYPH: usize = 3;
    pub const LINES_WASH: usize = 4;
    pub const SEGMENTS: usize = 5;
    pub const SLOTS: usize = 6;
    pub const SLOTS_DROPPED: usize = 7;
    pub const SEG_FIT_END: usize = 8;
    pub const SLOT_FIT_END: usize = 9;
    pub const WASH: usize = 10;
    pub const LINES_DROPPED: usize = 11;
    pub const ITEMS_HIDDEN: usize = 12;
    pub const ITEMS_CULLED: usize = 13;
}

/// One item as the kernels read it (128 B): where its bytes live, its lines
/// and spans, the fold and page constants X needs, and the world box the
/// item cull tests. Built from a [`VisibleItem`] by [`item_gpu`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct ItemGpu {
    pub chunk: u32,
    pub chunk_off: u32,
    pub byte_len: u32,
    pub first_line: u32,
    pub line_count: u32,
    pub span_base: u32,
    pub span_count: u32,
    /// `wrap_width` clamped at 0.
    pub wrap_width: u32,
    pub wrap_mode: u32,
    pub cluster: u32,
    /// `origin_x` as (hi, lo): lo is 0 while the input is f32.
    pub origin_x_hi: f32,
    pub origin_x_lo: f32,
    pub stride_hi: f32,
    pub stride_lo: f32,
    pub page_rows: i32,
    pub page_cols: i32,
    pub scroll_rows: i32,
    pub pages_wide: i32,
    pub has_page: u32,
    pub line_height: f32,
    pub group: u32,
    pub bbox_min: [f32; 3],
    pub bbox_max: [f32; 3],
    pub _pad: [u32; 5],
}

/// One segment entry (32 B), cull B's output and the layout kernel's input.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct SegGpu {
    pub item: u32,
    pub line: u32,
    /// Line-relative byte range.
    pub byte_off: u32,
    pub byte_end: u32,
    /// The fold state at `byte_off` (zeros for a line's first segment).
    pub col: u32,
    pub cells: u32,
    pub seg_adv: f32,
    pub slot_base: u32,
}

/// One wash quad (32 B).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct WashGpu {
    pub item: u32,
    pub row_lane: u32,
    pub x0: f32,
    pub width: f32,
    pub color: u32,
    pub rows: u32,
    pub alpha: f32,
    pub tint: u32,
}

/// The per-frame uniform every Visible kernel reads (224 B).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct FrameGpu {
    /// Column-major, as glam's `to_cols_array_2d` and the renderer's
    /// `Camera` uniform carry it.
    pub view_proj: [[f32; 4]; 4],
    /// Gribb-Hartmann planes, normalised: left, right, bottom, top, near, far.
    pub planes: [[f32; 4]; 6],
    /// xyz the eye, w `px_scale`.
    pub eye: [f32; 4],
    /// `lod_glyph_px`, `lod_backdrop_px`, `time`, `cell_adv`.
    pub lod: [f32; 4],
    /// `greek_mode`, `debug_tint`, items total, seeds total.
    pub u0: [u32; 4],
    /// `max_slots`, `max_segments`, `max_wash`, `default_color`.
    pub u1: [u32; 4],
}

/// The trie tables' offsets into the one packed word buffer (48 B).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct TrieMetaGpu {
    pub off_ascii: u32,
    pub off_index: u32,
    pub off_blocks: u32,
    pub off_bitmap: u32,
    pub off_seq: u32,
    pub seq_count: u32,
    pub seq_stride: u32,
    pub seq_max: u32,
    pub block_shift: u32,
    pub block_mask: u32,
    /// `fu_to_world(primary_advance_fu)`: one cell, world units.
    pub cell_adv: f32,
    /// Always 1.0: the kernel's `add1` multiplies by it inside `fma` so the
    /// shader compiler cannot fold the error-free transforms (see
    /// `visible_layout.wgsl`).
    pub one: f32,
}

/// What a layout dispatch is told (16 B).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct LayoutParamsGpu {
    /// Elements in the dispatch (segments, or seeds).
    pub count: u32,
    pub debug_tint: u32,
    pub default_color: u32,
    pub chunk_shift: u32,
}

// ── packed trie entries ─────────────────────────────────────────────────────

pub const PK_GLYPH_MASK: u32 = 0xFFFF;
pub const PK_K_SHIFT: u32 = 16;
pub const PK_SEQ_FIRST: u32 = 1 << 18;
pub const PK_MISSING: u32 = 1 << 19;
/// The ASCII table's entry for a byte that is not a leader.
pub const PK_SENTINEL: u32 = u32::MAX;

/// `text::fu_to_world` with the renderer's `CELL_HEIGHT_WORLD = 1.0`: the
/// nearest f32 to the exact quotient.
pub fn fu_to_world(fu: i32, em_height_fu: u32) -> f32 {
    (fu as f64 * 1.0f64 / em_height_fu as f64) as f32
}

/// The trie, packed for the kernel: one u32 per entry — glyph in bits
/// 0..16, advance in CELLS in 16..18, bit 18 "starts a sequence", bit 19
/// "missing" — because every atlas advance is a whole number of cells (the
/// renderer's `line_table::every_trie_advance_is_whole_cells` pins it; this
/// refuses an upload where it does not hold) and the fold's f64 sum of
/// such advances is exactly `cells x cell_adv`.
pub struct PackedTrie {
    pub words: Vec<u32>,
    pub meta: TrieMetaGpu,
}

pub fn pack_trie(t: &TrieUpload) -> PackedTrie {
    assert_eq!(t.entry_stride, 4, "trie entry stride {} (the kernel reads glyph, advance, height, flags)", t.entry_stride);
    assert!(t.block_shift >= 1 && t.block_shift <= 16, "trie block shift {}", t.block_shift);
    let adv = t.primary_advance_fu;
    assert!(adv > 0, "primary advance is 0 fu");
    assert_eq!(t.bitmap_advance_fu, 2 * adv as i32, "the bitmap advance must be two cells");
    let block_size = 1usize << t.block_shift;
    assert_eq!(t.blocks.len() % (4 * block_size), 0, "blocks are not whole {block_size}-entry blocks");
    let block_count = t.blocks.len() / (4 * block_size);
    assert!(block_count >= 1, "a trie needs its missing block");
    let index_len = (0x110000usize >> t.block_shift).max(1);
    assert!(t.block_index.len() >= index_len, "block index has {} entries, needs {index_len}", t.block_index.len());
    for (i, &b) in t.block_index.iter().enumerate() {
        assert!((b as usize) < block_count, "block index[{i}] = {b} names a block past {block_count}");
    }
    let stride = 2 + t.seq_max as usize;
    assert!(t.sequences.len().is_multiple_of(stride), "sequence section is not whole {stride}-word records");
    let seq_count = t.sequences.len() / stride;
    let seq_first_set: std::collections::HashSet<u32> = t.sequences.chunks_exact(stride).map(|e| e[2]).collect();
    let bitmap_words = 0x110000usize / 32;
    assert!(t.seq_first_bitmap.len() >= bitmap_words, "seq_first_bitmap has {} words, needs {bitmap_words}", t.seq_first_bitmap.len());
    for cp in &seq_first_set {
        let cp = *cp as usize;
        assert!(cp < 0x110000 && t.seq_first_bitmap[cp >> 5] >> (cp & 31) & 1 == 1, "sequence head U+{cp:X} is not in the bitmap");
    }

    let packed = |e: &[u32]| -> u32 {
        let (glyph, advance_fu, flags) = (e[0], e[1], e[3]);
        assert_eq!(advance_fu % adv, 0, "trie advance {advance_fu} fu is not whole cells");
        let k = advance_fu / adv;
        assert!(k <= 2, "trie advance {advance_fu} fu is {k} cells; the packing holds 0, 1 or 2");
        let mut p = (glyph & PK_GLYPH_MASK) | (k << PK_K_SHIFT);
        if flags & 1 != 0 {
            p |= PK_MISSING;
        }
        p
    };
    let lookup = |cp: u32| -> u32 {
        let block = if cp <= 0x10FFFF { t.block_index[(cp >> t.block_shift) as usize] } else { 0 };
        let e = (((block as usize) << t.block_shift) | (cp as usize & (block_size - 1))) * 4;
        let mut p = packed(&t.blocks[e..e + 4]);
        if seq_first_set.contains(&cp) {
            p |= PK_SEQ_FIRST;
        }
        p
    };

    let mut words: Vec<u32> = Vec::with_capacity(256 + t.block_index.len() + block_count * block_size + bitmap_words + t.sequences.len());
    let off_ascii = 0u32;
    for b in 0..256u32 {
        words.push(if b < 128 {
            // Lines exclude their newline; the fast table answers it with
            // glyph 0 anyway, as the renderer's does.
            let p = lookup(b);
            if b == 0x0A { p & !PK_GLYPH_MASK } else { p }
        } else {
            PK_SENTINEL
        });
    }
    let off_index = words.len() as u32;
    words.extend_from_slice(&t.block_index);
    let off_blocks = words.len() as u32;
    for e in t.blocks.as_chunks::<4>().0 {
        words.push(packed(e));
    }
    let off_bitmap = words.len() as u32;
    words.extend_from_slice(&t.seq_first_bitmap[..bitmap_words]);
    let off_seq = words.len() as u32;
    words.extend_from_slice(&t.sequences);
    if words.is_empty() {
        words.push(0);
    }
    PackedTrie {
        words,
        meta: TrieMetaGpu {
            off_ascii,
            off_index,
            off_blocks,
            off_bitmap,
            off_seq,
            seq_count: seq_count as u32,
            seq_stride: stride as u32,
            seq_max: t.seq_max,
            block_shift: t.block_shift,
            block_mask: (block_size - 1) as u32,
            cell_adv: fu_to_world(adv as i32, t.em_height_fu),
            one: 1.0,
        },
    }
}

// ── byte chunks ─────────────────────────────────────────────────────────────

/// Where each item's bytes land: `chunk` buffers of at most `1 << shift`
/// bytes, items packed contiguously in order, an item that would straddle
/// a chunk end starting the next. Four chunks at most (the kernel's byte
/// bindings).
pub struct BytePlan {
    pub shift: u32,
    pub chunk_sizes: Vec<u64>,
    /// Per item `(chunk, offset)`.
    pub place: Vec<(u32, u32)>,
}

pub const MAX_BYTE_CHUNKS: usize = 4;

/// The chunk shift for a device: 1 GiB, or the largest power of two under
/// the device's storage binding limit.
pub fn byte_chunk_shift(max_storage_binding: u64) -> u32 {
    let mut shift = 30u32;
    while shift > 16 && (1u64 << shift) > max_storage_binding {
        shift -= 1;
    }
    shift
}

pub fn plan_bytes(items: &[VisibleItem], shift: u32) -> BytePlan {
    let cap = 1u64 << shift;
    let mut chunk_sizes: Vec<u64> = Vec::new();
    let mut place = Vec::with_capacity(items.len());
    let mut cur = 0u64;
    for (i, it) in items.iter().enumerate() {
        let len = it.byte_len as u64;
        assert!(len <= cap, "item {i} is {len} B, more than one {cap} B byte chunk");
        if chunk_sizes.is_empty() || cur + len > cap {
            chunk_sizes.push(0);
            cur = 0;
        }
        let chunk = chunk_sizes.len() - 1;
        place.push((chunk as u32, cur as u32));
        cur += len;
        chunk_sizes[chunk] = cur;
    }
    assert!(
        chunk_sizes.len() <= MAX_BYTE_CHUNKS,
        "{} byte chunks of {cap} B; the kernel binds {MAX_BYTE_CHUNKS}",
        chunk_sizes.len()
    );
    BytePlan { shift, chunk_sizes, place }
}

// ── the item table ──────────────────────────────────────────────────────────

pub fn item_gpu(i: usize, it: &VisibleItem, place: (u32, u32), span_base: u32, span_count: u32) -> ItemGpu {
    assert!(it.first_line as u64 + it.line_count as u64 <= u32::MAX as u64, "item {i} line range overflows");
    ItemGpu {
        chunk: place.0,
        chunk_off: place.1,
        byte_len: it.byte_len,
        first_line: it.first_line,
        line_count: it.line_count,
        span_base,
        span_count,
        wrap_width: it.wrap_width.max(0) as u32,
        wrap_mode: it.wrap_mode,
        cluster: it.cluster,
        // f64 → (hi, lo): hi is the nearest f32, lo the exact remainder
        // (itself an f32 to within an ulp of an ulp). The kernel folds lo in
        // after hi; the sixth oracle tier is the judge of whether that is the
        // fold's single rounding on every corpus (paged-rows was an ulp off
        // with lo = 0, 2026-10-10).
        origin_x_hi: it.origin_x as f32,
        origin_x_lo: (it.origin_x - it.origin_x as f32 as f64) as f32,
        stride_hi: it.stride_x as f32,
        stride_lo: (it.stride_x - it.stride_x as f32 as f64) as f32,
        page_rows: it.params.page_rows,
        page_cols: it.params.page_cols,
        scroll_rows: it.params.scroll_rows,
        pages_wide: it.params.pages_wide,
        has_page: it.params.has_page,
        line_height: it.params.line_height,
        group: it.group_id,
        bbox_min: it.bbox_min,
        bbox_max: it.bbox_max,
        _pad: [0; 5],
    }
}

// ── spans ───────────────────────────────────────────────────────────────────

/// The span table's host mirror: each item's run `(base, count, capacity)`
/// in the device buffer, and the free tail. An edit that fits its run's
/// slack is rewritten in place; one that does not takes a fresh run at the
/// tail and the old run becomes a hole (nothing compacts).
#[derive(Clone, Debug)]
pub struct SpanAlloc {
    pub runs: Vec<(u32, u32, u32)>,
    pub next_free: u32,
    pub capacity: u32,
}

/// Slack per item: an eighth of its spans plus 16.
pub fn span_slack(count: u32) -> u32 {
    count / 8 + 16
}

/// Lay the items' spans out with slack. Returns the device table (padded
/// with zero spans) and the allocation.
pub fn plan_spans(items: &[VisibleItem], spans: &[ByteSpanGpu]) -> (Vec<ByteSpanGpu>, SpanAlloc) {
    let mut runs = Vec::with_capacity(items.len());
    let mut table: Vec<ByteSpanGpu> = Vec::new();
    for (i, it) in items.iter().enumerate() {
        let (b, n) = (it.span_base as usize, it.span_count as usize);
        assert!(b + n <= spans.len(), "item {i} spans {b}..{} exceed the table ({})", b + n, spans.len());
        let run = &spans[b..b + n];
        check_spans(i, run, it.byte_len);
        let cap = n as u32 + span_slack(n as u32);
        runs.push((table.len() as u32, n as u32, cap));
        table.extend_from_slice(run);
        table.resize(table.len() + span_slack(n as u32) as usize, ByteSpanGpu::zeroed());
    }
    // Tail room for remaps: a quarter of the table, at least 256 K spans
    // (3 MiB) — a repo loads with no spans at all and `--highlight` edits
    // arrive later, each item's first one past its 16-span slack.
    let tail = (table.len() / 4).max(1 << 18) + 1024;
    let next_free = table.len() as u32;
    table.resize(table.len() + tail, ByteSpanGpu::zeroed());
    let capacity = table.len() as u32;
    (table, SpanAlloc { runs, next_free, capacity })
}

/// Spans must be sorted, non-overlapping, non-empty and inside the item.
pub fn check_spans(item: usize, run: &[ByteSpanGpu], byte_len: u32) {
    let mut last_end = 0u32;
    for (k, s) in run.iter().enumerate() {
        assert!(s.start < s.end, "item {item} span {k} is empty or reversed ({}..{})", s.start, s.end);
        assert!(s.start >= last_end, "item {item} span {k} starts at {} before the previous end {last_end}", s.start);
        assert!(s.end <= byte_len, "item {item} span {k} ends at {} past the item's {byte_len} bytes", s.end);
        last_end = s.end;
    }
}

/// Per line, the index (into the device span table) of the first span of
/// its item that can cover the line's first byte: the partition point of
/// `span.end <= byte_start`. Lines of an item with no spans get the run's
/// base (the kernel never reads past `span_count`).
pub fn first_span_index(items: &[VisibleItem], lines: &[LineEntryGpu], table: &[ByteSpanGpu], alloc: &SpanAlloc) -> Vec<u32> {
    let mut out = vec![0u32; lines.len()];
    for (i, it) in items.iter().enumerate() {
        let (base, count, _) = alloc.runs[i];
        let run = &table[base as usize..(base + count) as usize];
        let line_range = it.first_line as usize..(it.first_line + it.line_count) as usize;
        assert!(line_range.end <= lines.len(), "item {i} lines {line_range:?} exceed the table ({})", lines.len());
        let mut si = 0usize;
        for li in line_range {
            let s = lines[li].byte_start;
            while si < run.len() && run[si].end <= s {
                si += 1;
            }
            out[li] = base + si as u32;
        }
    }
    out
}

/// The same for one item's lines after an edit (`set_item_spans`).
pub fn first_span_index_for_item(line_starts: &[u32], run: &[ByteSpanGpu], base: u32) -> Vec<u32> {
    let mut si = 0usize;
    line_starts
        .iter()
        .map(|&s| {
            while si < run.len() && run[si].end <= s {
                si += 1;
            }
            base + si as u32
        })
        .collect()
}

// ── frame math ──────────────────────────────────────────────────────────────

/// The six frustum planes of a column-major view-projection (Gribb-Hartmann;
/// wgpu clip z in [0, w], so near is row 2 alone): left, right, bottom, top,
/// near, far, each normalised. The renderer's `cull.rs::frustum_planes`,
/// transcribed.
pub fn frustum_planes(view_proj: &[[f32; 4]; 4]) -> [[f32; 4]; 6] {
    let row = |r: usize| [view_proj[0][r], view_proj[1][r], view_proj[2][r], view_proj[3][r]];
    let (r0, r1, r2, r3) = (row(0), row(1), row(2), row(3));
    let add = |a: [f32; 4], b: [f32; 4]| [a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3]];
    let sub = |a: [f32; 4], b: [f32; 4]| [a[0] - b[0], a[1] - b[1], a[2] - b[2], a[3] - b[3]];
    let mut planes = [add(r3, r0), sub(r3, r0), add(r3, r1), sub(r3, r1), r2, sub(r3, r2)];
    for p in &mut planes {
        let len = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt().max(1e-12);
        for c in p.iter_mut() {
            *c /= len;
        }
    }
    planes
}

/// Workgroups for `count` elements at [`WORKGROUP`] per group: x capped at
/// [`MAX_GROUPS_X`], y carrying the rest (the kernels' `linear_id`).
pub fn plan_dispatch(count: u32) -> [u32; 3] {
    let groups = count.div_ceil(WORKGROUP);
    [groups.min(MAX_GROUPS_X), groups.div_ceil(MAX_GROUPS_X).max(1), 1]
}

// ── the x narrowing, as the kernel does it ─────────────────────────────────

/// An f64 as the kernel receives it: its nearest f32 and the remainder's
/// nearest f32 (exact for a value of up to 48 significant bits).
pub fn split_f64(v: f64) -> (f32, f32) {
    let hi = v as f32;
    (hi, (v - hi as f64) as f32)
}

/// `two_sum` of `visible_layout.wgsl`: the f32 sum and its exact error.
#[inline]
pub fn two_sum(a: f32, b: f32) -> (f32, f32) {
    let s = a + b;
    let bb = s - a;
    (s, (a - (s - bb)) + (b - bb))
}

/// `two_prod`: the f32 product and its exact error (`fma` as `mul_add`).
#[inline]
pub fn two_prod(a: f32, b: f32) -> (f32, f32) {
    let p = a * b;
    (p, a.mul_add(b, -p))
}

/// `narrow`: fl32 of the exact (a_hi + a_lo) + (b_hi + b_lo) in one
/// effective rounding.
#[inline]
pub fn narrow(a_hi: f32, a_lo: f32, b_hi: f32, b_lo: f32) -> f32 {
    let (s, t) = two_sum(a_hi, b_hi);
    s + (t + (a_lo + b_lo))
}

/// `glyph_x` of `visible_layout.wgsl` in Rust, operation for operation, for
/// the unit tests that hold it to HyperLayout's f64 form ([`reference_x`]).
pub fn kernel_x(fold: bool, seg_adv: f32, cells: u32, cell_adv: f32, origin_x: f64, m: Option<u32>, stride: f64) -> f32 {
    let (o_hi, o_lo) = split_f64(origin_x);
    let base = if fold {
        narrow(seg_adv, 0.0, o_hi, o_lo)
    } else {
        let (p, e) = two_prod(cells as f32, cell_adv);
        narrow(p, e, o_hi, o_lo)
    };
    match m {
        None => base,
        Some(m) => {
            let (s_hi, s_lo) = split_f64(stride);
            let (p, e) = two_prod(m as f32, s_hi);
            narrow(base, 0.0, p, e + m as f32 * s_lo)
        }
    }
}

/// HyperLayout's x (`pass2_device.rs`): `f32(rel + origin)` then, for a
/// paged item, `f32(f64(base) + m x stride)`.
pub fn reference_x(fold: bool, seg_adv: f32, cells: u32, cell_adv: f32, origin_x: f64, m: Option<u32>, stride: f64) -> f32 {
    let rel: f64 = if fold { seg_adv as f64 } else { cells as f64 * cell_adv as f64 };
    let base = (rel + origin_x) as f32;
    match m {
        None => base,
        Some(m) => (base as f64 + m as f64 * stride) as f32,
    }
}

/// Validate the inputs' shape before any upload: lengths agree, lines are
/// item-major and in range, seeds sorted by `(line, byte_offset)` and
/// inside their lines.
pub fn check_inputs(inputs: &crate::VisibleInputs<'_>) {
    let items = inputs.items;
    assert_eq!(inputs.item_bytes.len(), items.len(), "item_bytes and items differ in length");
    let mut next_line = 0u32;
    for (i, it) in items.iter().enumerate() {
        assert_eq!(inputs.item_bytes[i].len(), it.byte_len as usize, "item {i}: byte_len disagrees with its bytes");
        assert_eq!(it.first_line, next_line, "item {i}: lines are not item-major and contiguous");
        next_line += it.line_count;
        assert!(it.wrap_mode <= 1, "item {i}: wrap mode {}", it.wrap_mode);
        for li in it.first_line..it.first_line + it.line_count {
            let l = &inputs.lines[li as usize];
            assert_eq!(l.item, i as u32, "line {li} names item {} inside item {i}'s range", l.item);
            assert!(l.byte_start <= it.byte_len, "line {li} starts past item {i}'s end");
        }
    }
    assert_eq!(next_line as usize, inputs.lines.len(), "the items' line counts do not sum to the table");
    let mut prev: Option<(u32, u32)> = None;
    for (k, s) in inputs.seeds.iter().enumerate() {
        assert!((s.line as usize) < inputs.lines.len(), "seed {k} names line {} past the table", s.line);
        if let Some(p) = prev {
            assert!((s.line, s.byte_offset) > p, "seed {k} is out of order (sorted by line, then offset)");
        }
        assert!(s.byte_offset > 0, "seed {k} cuts at offset 0");
        prev = Some((s.line, s.byte_offset));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_plan_covers_the_count_in_two_dimensions() {
        assert_eq!(plan_dispatch(0), [0, 1, 1]);
        assert_eq!(plan_dispatch(1), [1, 1, 1]);
        assert_eq!(plan_dispatch(64), [1, 1, 1]);
        assert_eq!(plan_dispatch(65), [2, 1, 1]);
        let n = MAX_GROUPS_X * WORKGROUP;
        assert_eq!(plan_dispatch(n), [MAX_GROUPS_X, 1, 1]);
        assert_eq!(plan_dispatch(n + 1), [MAX_GROUPS_X, 2, 1]);
        // 38 M lines (the Linux tree): still fits, 10 rows of x.
        let p = plan_dispatch(38_000_000);
        assert!(p[0] as u64 * p[1] as u64 * WORKGROUP as u64 >= 38_000_000);
        assert_eq!(p[1], 10);
    }

    #[test]
    fn frustum_planes_of_an_orthographic_box() {
        // Identity view-proj: clip = world, so the frustum is x,y in [-1, 1],
        // z in [0, 1].
        let id = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];
        let p = frustum_planes(&id);
        let inside = |pt: [f32; 3]| p.iter().all(|pl| pl[0] * pt[0] + pl[1] * pt[1] + pl[2] * pt[2] + pl[3] >= 0.0);
        assert!(inside([0.0, 0.0, 0.5]));
        assert!(inside([0.99, -0.99, 0.01]));
        assert!(!inside([1.5, 0.0, 0.5]), "right");
        assert!(!inside([0.0, -1.5, 0.5]), "bottom");
        assert!(!inside([0.0, 0.0, -0.1]), "near (z >= 0)");
        assert!(!inside([0.0, 0.0, 1.1]), "far");
        for pl in &p {
            let n = (pl[0] * pl[0] + pl[1] * pl[1] + pl[2] * pl[2]).sqrt();
            assert!((n - 1.0).abs() < 1e-6, "normalised");
        }
    }

    #[test]
    fn byte_chunks_never_straddle() {
        let item = |len: u32| VisibleItem {
            params: Default::default(),
            origin_x: 0.0,
            stride_x: 0.0,
            wrap_width: 0,
            wrap_mode: 0,
            cluster: 1,
            byte_base: 0,
            byte_len: len,
            first_line: 0,
            line_count: 0,
            span_base: 0,
            span_count: 0,
            bbox_min: [0.0; 3],
            bbox_max: [0.0; 3],
            group_id: 0,
        };
        let items = [item(100), item(900), item(30), item(1000), item(1024)];
        let plan = plan_bytes(&items, 10);
        assert_eq!(plan.place, [(0, 0), (0, 100), (1, 0), (2, 0), (3, 0)]);
        assert_eq!(plan.chunk_sizes, [1000, 30, 1000, 1024]);
        assert_eq!(byte_chunk_shift(1 << 31), 30);
        assert_eq!(byte_chunk_shift(128 << 20), 27);
        assert_eq!(byte_chunk_shift((128 << 20) - 1), 26);
    }

    #[test]
    fn first_span_index_is_the_partition_point_per_line() {
        let mut it = VisibleItem {
            params: Default::default(),
            origin_x: 0.0,
            stride_x: 0.0,
            wrap_width: 0,
            wrap_mode: 0,
            cluster: 1,
            byte_base: 0,
            byte_len: 100,
            first_line: 0,
            line_count: 4,
            span_base: 0,
            span_count: 3,
            bbox_min: [0.0; 3],
            bbox_max: [0.0; 3],
            group_id: 0,
        };
        let spans = [
            ByteSpanGpu { start: 2, end: 5, color: 1 },
            ByteSpanGpu { start: 20, end: 30, color: 2 },
            ByteSpanGpu { start: 45, end: 60, color: 3 },
        ];
        let lines = [
            LineEntryGpu { byte_start: 0, item: 0, base_row: 0, glyph_count: 0 },
            LineEntryGpu { byte_start: 10, item: 0, base_row: 1, glyph_count: 0 },
            LineEntryGpu { byte_start: 25, item: 0, base_row: 2, glyph_count: 0 },
            LineEntryGpu { byte_start: 70, item: 0, base_row: 3, glyph_count: 0 },
        ];
        let (table, alloc) = plan_spans(std::slice::from_ref(&it), &spans);
        assert_eq!(alloc.runs, [(0, 3, 3 + 16)]);
        assert_eq!(first_span_index(std::slice::from_ref(&it), &lines, &table, &alloc), [0, 1, 1, 3]);
        // A line inside a span (25 in 20..30) still indexes that span.
        assert_eq!(first_span_index_for_item(&[0, 10, 25, 70], &spans, 100), [100, 101, 101, 103]);
        it.span_count = 0;
        let (table, alloc) = plan_spans(std::slice::from_ref(&it), &spans);
        assert_eq!(first_span_index(std::slice::from_ref(&it), &lines, &table, &alloc), [0, 0, 0, 0]);
    }

    #[test]
    #[should_panic(expected = "before the previous end")]
    fn overlapping_spans_are_refused() {
        check_spans(0, &[ByteSpanGpu { start: 0, end: 5, color: 0 }, ByteSpanGpu { start: 4, end: 9, color: 0 }], 10);
    }

    /// The kernel's x against HyperLayout's two narrowings, over random fold
    /// states with the item constants as the renderer has them — an f64
    /// origin (not f32-representable) and an f64 stride that is a foldless
    /// line advance plus the page gap — and page columns m >= 1. The kernel
    /// gets (hi, lo) halves and rounds once per narrowing through the
    /// error-free transforms; the first form (`fma(m, hi, base)` then the lo
    /// part) failed 72 of 1,052 paged-rows slots on the oracle tier
    /// (2026-10-10) and fails thousands here.
    #[test]
    fn kernel_x_matches_the_reference_bit_for_bit() {
        let cell_adv = fu_to_world(1229, 2320);
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut rnd = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut mismatches = 0usize;
        let mut checked = 0usize;
        let mut paged_cases = 0usize;
        for _ in 0..200_000 {
            let r = rnd();
            let r2 = rnd();
            let cells = (r & 0x7FFFF) as u32; // up to 524,287 cells (a 500 KB line)
            // An origin with bits past f32: a quarter-step grid plus a fine f64 fraction.
            let origin_x = (((r >> 20) & 0xFFFFF) as f64 - 0x80000 as f64) * 0.25 + (r2 & 0xFFFFFF) as f64 * 2f64.powi(-40);
            let fold = (r >> 40) & 1 == 1;
            // seg_adv as the fold accumulates it: an f32 running sum of up to
            // 255 cell advances (two cells per leader at most).
            let n = ((r >> 41) & 0xFF) as u32;
            let mut seg_adv = 0f32;
            for _ in 0..n {
                seg_adv += cell_adv;
            }
            let paged = (r >> 50) & 3 != 0;
            let m = if paged { Some(1 + ((r >> 52) & 7) as u32) } else { None };
            // The stride the renderer builds in f64: a foldless item's widest
            // line advance (cells x cell_adv, exact in f64, not in f32) plus
            // page_gap_x 4.0; or a fold-unit item's f32 widest segment advance.
            let widest = ((r2 >> 24) & 0xFFF) as u32;
            let stride: f64 = if (r2 >> 36) & 1 == 1 { widest as f64 * cell_adv as f64 + 4.0 } else { (widest as f32 * cell_adv) as f64 + 4.0 };
            let want = reference_x(fold, seg_adv, cells, cell_adv, origin_x, m, stride);
            let got = kernel_x(fold, seg_adv, cells, cell_adv, origin_x, m, stride);
            checked += 1;
            paged_cases += usize::from(paged);
            if want.to_bits() != got.to_bits() {
                if mismatches < 5 {
                    eprintln!("mismatch: fold={fold} seg_adv={seg_adv} cells={cells} origin={origin_x} m={m:?} stride={stride}: want {want} ({:#x}) got {got} ({:#x})", want.to_bits(), got.to_bits());
                }
                mismatches += 1;
            }
        }
        assert_eq!(checked, 200_000);
        assert!(paged_cases > 100_000, "the draw must exercise page columns ({paged_cases})");
        assert_eq!(mismatches, 0, "kernel x differs from the reference in {mismatches} of {checked} draws");
        // The halves are exact for a value of up to 48 significant bits (a
        // widest f32 advance plus 4.0 is one).
        let v = 1_000_000.0f64 + 2f64.powi(-20);
        let (hi, lo) = split_f64(v);
        assert_eq!(hi as f64 + lo as f64, v);
        assert_ne!(lo, 0.0);
    }

    #[test]
    fn packed_trie_carries_glyph_cells_and_flags() {
        let t = crate::test_support::synthetic_trie();
        let p = pack_trie(&t);
        let word = |cp: u32| -> u32 {
            let block = p.words[(p.meta.off_index + (cp >> p.meta.block_shift)) as usize];
            p.words[(p.meta.off_blocks + ((block << p.meta.block_shift) | (cp & p.meta.block_mask))) as usize]
        };
        assert_eq!(word(b'A' as u32) & PK_GLYPH_MASK, b'A' as u32);
        assert_eq!((word(b'A' as u32) >> PK_K_SHIFT) & 3, 1);
        assert_eq!(p.words[(p.meta.off_ascii + 0x0A) as usize] & PK_GLYPH_MASK, 0, "the newline's fast entry is glyph 0");
        assert_ne!(p.words[(p.meta.off_ascii + b'#' as u32) as usize] & PK_SEQ_FIRST, 0, "'#' heads a keycap");
        assert_eq!(p.words[(p.meta.off_ascii + b'A' as u32) as usize] & PK_SEQ_FIRST, 0);
        assert_eq!(p.words[(p.meta.off_ascii + 200) as usize], PK_SENTINEL);
        assert_eq!((word(0x4E2D) >> PK_K_SHIFT) & 3, 2, "a wide glyph is two cells");
        assert_ne!(word(0x10FFFF) & PK_MISSING, 0, "an unmapped codepoint resolves through the missing block");
        assert_eq!(p.meta.seq_count, 3);
        assert_eq!(p.meta.seq_stride, 5);
        assert_eq!(p.meta.cell_adv, fu_to_world(1229, 2320));
    }
}
