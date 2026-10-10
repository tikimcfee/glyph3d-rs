// visible_layout.wgsl — the Visible field's layout kernel: one invocation per
// SEGMENT (a whole short line, or one cut of a long line), a serial walk over
// its bytes that resolves every leader through the resident atlas trie — in
// cluster mode exactly as HyperLayout's `char_resolve.rs` (the ASCII fast
// table with its seq_lead guard, the lookahead probe that drops FE0F from the
// key and stops at a newline / FE0E / a non-leader, longest prefix first,
// the head taking two cells and its trailers zero; in leader mode the trie's
// own entry for every leader) — and writes the Derived slot (20 B) of every
// glyph it draws, with the bits HyperLayout's device Pass 2 (`DerivedEmit`)
// writes. Descends from `experiments/gpu-direction/jit-text/src/layout.wgsl`
// (lookup variant A: the trie as shipped).
//
// Four entry points, three sharing `walk_segment`:
//   layout_segments       one per segment entry (cull B's output, or the
//                         headless list): emits slots at seg.slot_base. In
//                         MASK mode (params.mode = 1; M3) only the segments
//                         of `filter_item` that intersect [filter_lo,
//                         filter_hi) walk, and only the leaders inside the
//                         range are emitted, appended by atomic counter into
//                         the (mask) slot buffer — a selection keyed by
//                         (item, byte range), with no slot to name.
//   count_seed_segments   one per segment seed, at load: counts the
//                         survivors of the segment ENDING at the seed, so
//   prefix_seed_survivors can turn them into each seed's survivors-before —
//                         the slot base of a seeded segment inside its line.
//   finalize_mask         one invocation after a mask dispatch: the mask
//                         draw's indirect arguments from its counter.
//
// OVERRIDES (M3). The slot verbs keyed a glyph by its slot; here a slot is
// transient, so a per-glyph edit is keyed by (item, byte): the item's run of
// `GlyphOverride`s, sorted by byte, walked alongside the bytes exactly as the
// spans are (one binary search per segment start, then advance). A colour
// override beats the span's; an x nudge is an f32 add AFTER the narrowing
// (`add1`, so it is exactly the IEEE add the host twin does — it is not part
// of the oracle contract, which has no nudges); a group override puts the
// group's index in the Derived lane's upper 12 bits (derive.rs
// `override_lane`), and the Derived draw's binding 10 maps it to the group
// row.
//
// X. HyperLayout narrows the fold's x TWICE: `base_x = f32(rel + origin_x)`
// (rel the f32 segment advance when the item has a fold unit, else the
// fold's f64 line advance, which is exactly `cells x cell_adv`), then, for a
// paged item, `f32(f64(base_x) + x_off)` with `x_off = (y_page % pages_wide)
// x stride_x`. Each is one rounding of an exact (or f64-exact) sum, and
// `fma` is one rounding of an exact product-sum, so: fold unit > 0 →
// `seg_adv + origin` (one f32 add is that rounding); fold unit 0 →
// `fma(f32(cells), cell_adv, origin)`; paged → `fma(f32(m), stride, base_x)`.
// `origin_x_lo` / `stride_lo` carry a low part if the host ever widens them
// (both are 0 today: the inputs are f32). The oracle tier judges the bits.

struct ItemGpu {
    chunk: u32,
    chunk_off: u32,
    byte_len: u32,
    first_line: u32,
    line_count: u32,
    span_base: u32,
    span_count: u32,
    wrap_width: u32,
    wrap_mode: u32,
    cluster: u32,
    origin_x_hi: f32,
    origin_x_lo: f32,
    stride_hi: f32,
    stride_lo: f32,
    page_rows: i32,
    page_cols: i32,
    scroll_rows: i32,
    pages_wide: i32,
    has_page: u32,
    line_height: f32,
    group: u32,
    bbox_min_x: f32,
    bbox_min_y: f32,
    bbox_min_z: f32,
    bbox_max_x: f32,
    bbox_max_y: f32,
    bbox_max_z: f32,
    override_base: u32,
    override_count: u32,
    _pad2: u32,
    _pad3: u32,
    _pad4: u32,
};

// `cols` and `width_cells` are the cull's (C28); the kernel reads neither.
struct LineEntry { byte_start: u32, item: u32, base_row: u32, glyph_count: u32, cols: u32, width_cells: u32 };
struct SegmentSeed { line: u32, byte_offset: u32, col: u32, seg_adv: f32, cells: u32, _pad: u32 };
struct ByteSpan { start: u32, end: u32, color: u32 };
// One per-glyph override (tables.rs GlyphOverrideGpu): the leader's item-
// relative byte, a colour (0 = the span's), an x nudge, a group index (0 =
// the item's group).
struct GlyphOverride { byte: u32, color: u32, x_nudge: f32, group_index: u32 };
// One segment to lay out: `byte_off .. byte_end` are line-relative; `col`,
// `cells`, `seg_adv` the fold state at `byte_off` (zeros for a line's first
// segment); `slot_base` where its first slot goes.
struct Seg { item: u32, line: u32, byte_off: u32, byte_end: u32, col: u32, cells: u32, seg_adv: f32, slot_base: u32 };
struct DerivedSlot { x: f32, row: u32, glyph_and_wrap: u32, color: u32, item_and_group: u32 };

struct TrieMeta {
    off_ascii: u32,
    off_index: u32,
    off_blocks: u32,
    off_bitmap: u32,
    off_seq: u32,
    seq_count: u32,
    seq_stride: u32,
    seq_max: u32,
    block_shift: u32,
    block_mask: u32,
    cell_adv: f32,
    // 1.0, loaded at runtime: `add1` routes every add of the x narrowing
    // through `fma(a, one, b)` so the shader compiler cannot fold or
    // reassociate it (see `two_sum`).
    one: f32,
};

// What the frame (or the headless run) sets: `count` is the dispatch's
// element count (segments, or seeds for the seed kernels); `mode` 0 lays
// every segment out, 1 is the selection mask over `filter_item`'s leaders
// in [filter_lo, filter_hi), at most `mask_cap` of them.
struct LayoutParams {
    count: u32,
    debug_tint: u32,
    default_color: u32,
    chunk_shift: u32,
    mode: u32,
    filter_item: u32,
    filter_lo: u32,
    filter_hi: u32,
    mask_cap: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
};

@group(0) @binding(0) var<uniform> params: LayoutParams;
@group(0) @binding(1) var<uniform> trie: TrieMeta;
@group(0) @binding(2) var<storage, read> bytes0: array<u32>;
@group(0) @binding(3) var<storage, read> bytes1: array<u32>;
@group(0) @binding(4) var<storage, read> bytes2: array<u32>;
@group(0) @binding(5) var<storage, read> bytes3: array<u32>;
@group(0) @binding(6) var<storage, read> items: array<ItemGpu>;
@group(0) @binding(7) var<storage, read> lines: array<LineEntry>;
@group(0) @binding(8) var<storage, read> seeds: array<SegmentSeed>;
@group(0) @binding(9) var<storage, read_write> survivors: array<u32>;
@group(0) @binding(10) var<storage, read> segs: array<Seg>;
@group(0) @binding(11) var<storage, read_write> slots: array<DerivedSlot>;
@group(0) @binding(12) var<storage, read> tables: array<u32>;
@group(0) @binding(13) var<storage, read> spans: array<ByteSpan>;
@group(0) @binding(14) var<storage, read> first_span: array<u32>;
@group(0) @binding(15) var<storage, read> overrides: array<GlyphOverride>;
// The mask's slot counter at word 0; `finalize_mask` writes the mask draw's
// DrawIndexedIndirectArgs at words 4..9 (the host copies them out).
@group(0) @binding(16) var<storage, read_write> mask_args: array<atomic<u32>, 12>;

const PK_GLYPH_MASK: u32 = 0xFFFFu;
const PK_K_SHIFT: u32 = 16u;
const PK_SEQ_FIRST: u32 = 1u << 18u;
const PK_RESOLVED_MASK: u32 = 0x3FFFFu;   // glyph + cells
const MAX_SEQ_KEY: u32 = 16u;
const ROW_MAX: u32 = 0xFFFFFFu;           // derive.rs ROW_BITS = 24
const X_PAGE_MAX: u32 = 0xFFu;
const ITEM_MAX: u32 = 0xFFFFFu;           // derive.rs ITEM_BITS = 20
const ITEM_BITS: u32 = 20u;
const OVERRIDE_MAX: u32 = 0xFFFu;         // derive.rs OVERRIDE_MAX (12 bits)
const MODE_MASK: u32 = 1u;
const WG: u32 = 64u;
const MAX_GROUPS_X: u32 = 65535u;

// Debug tints (frame.debug_tint): 1 = by tier, 2 = by cull state.
const TINT_GLYPH_TIER: u32 = 0xFF60E0FFu;      // glyph tier: warm yellow
const TINT_FIRST_SEGMENT: u32 = 0xFF80FF80u;   // a line's first segment: green
const TINT_CONTINUATION: u32 = 0xFF40A0FFu;    // a continuation segment: orange

// The invocation's linear index under the 2D dispatch the host plans
// (`plan_dispatch`): x is capped at 65,535 workgroups, y carries the rest.
fn linear_id(gid: vec3<u32>) -> u32 {
    return gid.y * (MAX_GROUPS_X * WG) + gid.x;
}

fn word_at(chunk: u32, i: u32) -> u32 {
    let w = i >> 2u;
    var r = 0u;
    switch chunk {
        case 0u: { r = bytes0[w]; }
        case 1u: { r = bytes1[w]; }
        case 2u: { r = bytes2[w]; }
        default: { r = bytes3[w]; }
    }
    return r;
}

// Byte `i` (chunk-relative) of an item whose true end is `lim`; 0 past it,
// as the reference's bounds-checked decode reads 0 past its slice.
fn byte_at(chunk: u32, i: u32, lim: u32) -> u32 {
    if (i >= lim) { return 0u; }
    return (word_at(chunk, i) >> ((i & 3u) * 8u)) & 0xFFu;
}

fn seq_len(b: u32) -> u32 {
    if ((b & 0x80u) == 0u) { return 1u; }
    if ((b & 0xE0u) == 0xC0u) { return 2u; }
    if ((b & 0xF0u) == 0xE0u) { return 3u; }
    if ((b & 0xF8u) == 0xF0u) { return 4u; }
    return 0u;
}

fn decode(chunk: u32, i: u32, len: u32, lim: u32) -> u32 {
    let b0 = byte_at(chunk, i, lim);
    if (len == 1u) { return b0; }
    let b1 = byte_at(chunk, i + 1u, lim) & 0x3Fu;
    if (len == 2u) { return ((b0 & 0x1Fu) << 6u) | b1; }
    let b2 = byte_at(chunk, i + 2u, lim) & 0x3Fu;
    if (len == 3u) { return ((b0 & 0x0Fu) << 12u) | (b1 << 6u) | b2; }
    let b3 = byte_at(chunk, i + 3u, lim) & 0x3Fu;
    return ((b0 & 0x07u) << 18u) | (b1 << 12u) | (b2 << 6u) | b3;
}

fn is_static_zero(cp: u32) -> bool {
    return cp == 0x200Du || (cp >= 0xFE00u && cp <= 0xFE0Fu) || (cp >= 0xE0020u && cp <= 0xE007Fu);
}

// The trie as shipped (`TrieTable::lookup`): an out-of-range codepoint
// resolves through block 0, the shared missing block, reading entry
// `cp & mask` of it as the reference does.
fn lookup(cp: u32) -> u32 {
    var block = 0u;
    if (cp <= 0x10FFFFu) { block = tables[trie.off_index + (cp >> trie.block_shift)]; }
    return tables[trie.off_blocks + ((block << trie.block_shift) | (cp & trie.block_mask))];
}

fn starts_seq(cp: u32) -> bool {
    if (cp > 0x10FFFFu) { return false; }
    return ((tables[trie.off_bitmap + (cp >> 5u)] >> (cp & 31u)) & 1u) != 0u;
}

// -1 / 0 / 1 for key[0..n) against entry `e` of the section (elementwise,
// a strict prefix sorts first — the section's own order).
fn cmp_key(key: ptr<function, array<u32, 16>>, n: u32, e: u32) -> i32 {
    let o = trie.off_seq + e * trie.seq_stride;
    let elen = tables[o + 1u];
    let m = min(n, elen);
    for (var k = 0u; k < m; k = k + 1u) {
        let a = (*key)[k];
        let b = tables[o + 2u + k];
        if (a < b) { return -1; }
        if (a > b) { return 1; }
    }
    if (n < elen) { return -1; }
    if (n > elen) { return 1; }
    return 0;
}

// Binary search over the sorted section (`TrieTable::sequence_lookup`).
// Returns slot | 0x80000000 on a hit, 0 on a miss.
fn seq_lookup(key: ptr<function, array<u32, 16>>, n: u32) -> u32 {
    var lo = 0u;
    var hi = trie.seq_count;
    while (lo < hi) {
        let mid = (lo + hi) >> 1u;
        if (cmp_key(key, n, mid) > 0) { lo = mid + 1u; } else { hi = mid; }
    }
    if (lo < trie.seq_count && cmp_key(key, n, lo) == 0) {
        return tables[trie.off_seq + lo * trie.seq_stride] | 0x80000000u;
    }
    return 0u;
}

// The slow path (`char_resolve.rs::resolve_leader`). `end` bounds the probe
// at the segment's end (a cut before an ASCII byte, or the line's newline:
// either ends every candidate the whole-item walk would have built); `lim`
// is the item's true end for the byte reads. Returns glyph | cells << 16.
fn resolve_leader(chunk: u32, i: u32, len: u32, end: u32, lim: u32, cluster: bool, trailer_until: ptr<function, u32>) -> u32 {
    let cp = decode(chunk, i, len, lim);
    let entry = lookup(cp);
    // Leader mode: the trie's own entry, the invisible-by-design
    // codepoints included — they occupy a cell, as the corpus pins.
    if (!cluster) { return entry & PK_RESOLVED_MASK; }
    if (i < *trailer_until || is_static_zero(cp)) { return 0u; }
    if (starts_seq(cp)) {
        var key: array<u32, 16>;
        var key_end: array<u32, 16>;
        key[0] = cp;
        key_end[0] = i + len;
        var n = 1u;
        let max_key = min(trie.seq_max, MAX_SEQ_KEY);
        var p = i + len;
        while (p < end && n < max_key) {
            let n2 = seq_len(byte_at(chunk, p, lim));
            if (n2 == 0u) { break; }
            let cp2 = decode(chunk, p, n2, lim);
            if (cp2 == 0x0Au || cp2 == 0xFE0Eu) { break; }
            if (cp2 != 0xFE0Fu) {
                key[n] = cp2;
                key_end[n] = p + n2;
                n = n + 1u;
            }
            p = p + n2;
        }
        // The longest table prefix of the key wins.
        for (var try_len = n; try_len >= 2u; try_len = try_len - 1u) {
            let hit = seq_lookup(&key, try_len);
            if (hit != 0u) {
                *trailer_until = key_end[try_len - 1u];
                return (hit & PK_GLYPH_MASK) | (2u << PK_K_SHIFT);
            }
        }
    }
    return entry & PK_RESOLVED_MASK;
}

// `derive.rs::pack_row`: row:24 | x_page:8, saturated.
fn pack_row(row: u32, x_page: u32) -> u32 {
    return min(row, ROW_MAX) | (min(x_page, X_PAGE_MAX) << 24u);
}

// The slow path's x (see the header): one rounding per narrowing point.
// Error-free transforms. WGSL lets an implementation reassociate and
// simplify float expressions, and this device's compiler does: the plain
// TwoSum error `(a - (s - bb)) + (b - bb)` compiled to 0 (measured
// 2026-10-10, RTX 5090 / Vulkan; `tests/fp_probe.rs` pins it), while `fma`
// stays one rounding of the exact value. So every add here is
// `fma(a, one, b)` with `one` a runtime 1.0 the compiler cannot see
// through — each is exactly the IEEE add, and none can be folded away.
fn add1(a: f32, b: f32) -> f32 {
    return fma(a, trie.one, b);
}

// The exact sum as (value, error).
fn two_sum(a: f32, b: f32) -> vec2<f32> {
    let s = add1(a, b);
    let bb = add1(s, -a);
    let e = add1(add1(a, -add1(s, -bb)), add1(b, -bb));
    return vec2<f32>(s, e);
}

// The exact product as (value, error).
fn two_prod(a: f32, b: f32) -> vec2<f32> {
    let p = a * b;
    return vec2<f32>(p, fma(a, b, -p));
}

// fl32 of the exact value (a_hi + a_lo) + (b_hi + b_lo), with ONE effective
// rounding: TwoSum the big halves, gather the small terms (the sum's error
// and both low halves), add once. HyperLayout's `f32(f64 sum)`.
fn narrow(a_hi: f32, a_lo: f32, b_hi: f32, b_lo: f32) -> f32 {
    let st = two_sum(a_hi, b_hi);
    let r = add1(st.y, add1(a_lo, b_lo));
    return add1(st.x, r);
}

// The item's f64 constants travel as (hi, lo) f32 pairs (`tables.rs`,
// `item_gpu`); `rel` is the f32 segment advance when the item has a fold
// unit, else the exact product cells x cell_adv as a TwoProd pair.
fn glyph_x(it: ItemGpu, fold: bool, seg_adv: f32, cells: u32, row: u32) -> f32 {
    var base_x = 0.0;
    if (fold) {
        base_x = narrow(seg_adv, 0.0, it.origin_x_hi, it.origin_x_lo);
    } else {
        let rel = two_prod(f32(cells), trie.cell_adv);
        base_x = narrow(rel.x, rel.y, it.origin_x_hi, it.origin_x_lo);
    }
    // `layout_hyper/page.rs::Pager`: every page decision reads the integer row.
    let rows = select(0, it.page_rows, it.has_page != 0u);
    let cols = select(0, it.page_cols, it.has_page != 0u);
    let scroll = select(0, it.scroll_rows, it.has_page != 0u);
    if (rows != 0 || cols != 0 || scroll != 0) {
        let screen_row = i32(row) - scroll;
        var y_page = 0;
        if (rows > 0 && screen_row >= rows) { y_page = screen_row / rows; }
        let pages_wide = select(1, it.pages_wide, it.pages_wide > 1);
        let m = f32(y_page % pages_wide);
        // x_off = m x (stride_hi + stride_lo), exactly: the product's error
        // and m x lo are the small terms. `paged_x`: f32(f64(base_x) + x_off).
        let off = two_prod(m, it.stride_hi);
        base_x = narrow(base_x, 0.0, off.x, add1(off.y, m * it.stride_lo));
    }
    return base_x;
}

// The walk. Returns the segment's survivor count; writes slots when `emit`.
fn walk_segment(seg: Seg, emit: bool) -> u32 {
    let it = items[seg.item];
    let line = lines[seg.line];
    let chunk = it.chunk;
    let lim = it.chunk_off + it.byte_len;
    let line_start = it.chunk_off + line.byte_start;
    var i = line_start + seg.byte_off;
    let end = line_start + seg.byte_end;
    let cluster = it.cluster != 0u;
    let wrap = it.wrap_width;
    let fold_unit = select(select(0u, u32(it.page_cols), it.has_page != 0u && it.page_cols > 0), wrap, wrap > 0u);
    let fold = fold_unit > 0u;
    let page_cols = select(0u, u32(it.page_cols), it.has_page != 0u && it.page_cols > 0);
    let wrap_back = it.wrap_mode == 1u;
    let item_lane = min(seg.item, ITEM_MAX);

    var col = seg.col;
    var cells = seg.cells;
    var seg_adv = seg.seg_adv;
    var k = seg.slot_base;
    var trailer_until = 0u;

    // Colour: the first span that can cover this segment's first byte.
    let span_end = it.span_base + it.span_count;
    var si = span_end;
    if (it.span_count > 0u) {
        if (seg.byte_off == 0u) {
            si = first_span[seg.line];
        } else {
            let p0 = line.byte_start + seg.byte_off;
            var lo = it.span_base;
            var hi = span_end;
            while (lo < hi) {
                let mid = (lo + hi) >> 1u;
                if (spans[mid].end <= p0) { lo = mid + 1u; } else { hi = mid; }
            }
            si = lo;
        }
    }
    // Overrides: the first of the item's at or past this segment's first
    // byte (the run is sorted by byte; one binary search per segment).
    let ov_end = it.override_base + it.override_count;
    var oi = ov_end;
    if (it.override_count > 0u) {
        let p0 = line.byte_start + seg.byte_off;
        var lo = it.override_base;
        var hi = ov_end;
        while (lo < hi) {
            let mid = (lo + hi) >> 1u;
            if (overrides[mid].byte < p0) { lo = mid + 1u; } else { hi = mid; }
        }
        oi = lo;
    }
    var tint = 0u;
    if (params.debug_tint == 1u) { tint = TINT_GLYPH_TIER; }
    if (params.debug_tint == 2u) { tint = select(TINT_CONTINUATION, TINT_FIRST_SEGMENT, seg.byte_off == 0u); }
    let mask_mode = params.mode == MODE_MASK;

    var survivors = 0u;
    while (i < end) {
        let b = byte_at(chunk, i, lim);
        var r = 0u;
        if (b < 0x80u) {
            let e = tables[trie.off_ascii + b];
            // The fast answer, unless this seq_lead byte, in cluster mode,
            // is followed by a non-ASCII byte (char_resolve.rs).
            let next_non_ascii = (i + 1u < end) && (byte_at(chunk, i + 1u, lim) >= 0x80u);
            if (i >= trailer_until && !((e & PK_SEQ_FIRST) != 0u && cluster && next_non_ascii)) {
                r = e & PK_RESOLVED_MASK;
            } else {
                r = resolve_leader(chunk, i, 1u, end, lim, cluster, &trailer_until);
            }
        } else {
            let len = seq_len(b);
            if (len == 0u) { i = i + 1u; continue; }   // a continuation or invalid lead: no record
            r = resolve_leader(chunk, i, len, end, lim, cluster, &trailer_until);
        }
        let glyph = r & PK_GLYPH_MASK;
        let kk = (r >> PK_K_SHIFT) & 3u;
        if (glyph != 0u) {
            if (emit) {
                let wrap_segment = select(0u, col / wrap, wrap > 0u);
                let row = select(line.base_row + wrap_segment, line.base_row, wrap_back);
                let x_page = select(0u, col / page_cols, page_cols > 0u);
                var x = glyph_x(it, fold, seg_adv, cells, row);
                var lane = item_lane;
                // The leader's colour: the span covering its byte (item-
                // relative, as the spans are), else the default.
                let q = line.byte_start + (i - line_start);
                while (si < span_end && q >= spans[si].end) { si = si + 1u; }
                var color = params.default_color;
                if (si < span_end && q >= spans[si].start) { color = spans[si].color; }
                // Its override, if the item has one at this byte (an
                // override at a byte that is not a surviving leader is
                // passed over, never applied to a neighbour).
                while (oi < ov_end && overrides[oi].byte < q) { oi = oi + 1u; }
                if (oi < ov_end && overrides[oi].byte == q) {
                    let ov = overrides[oi];
                    if (ov.color != 0u) { color = ov.color; }
                    if (ov.x_nudge != 0.0) { x = add1(ov.x_nudge, x); }
                    if (ov.group_index != 0u) { lane = item_lane | (min(ov.group_index, OVERRIDE_MAX) << ITEM_BITS); }
                }
                if (tint != 0u) { color = tint; }
                let slot = DerivedSlot(x, pack_row(row, x_page), glyph | ((wrap_segment & 0xFFFFu) << 16u), color, lane);
                if (mask_mode) {
                    // Only the leaders in the selection, appended in atomic
                    // order (a mask is coverage; order is nothing to it).
                    if (q >= params.filter_lo && q < params.filter_hi) {
                        let m = atomicAdd(&mask_args[0], 1u);
                        if (m < params.mask_cap) { slots[m] = slot; }
                    }
                } else {
                    slots[k] = slot;
                    k = k + 1u;
                }
            }
            survivors = survivors + 1u;
        }
        col = col + 1u;
        cells = cells + kk;
        if (fold && (col % fold_unit) == 0u) {
            seg_adv = 0.0;
        } else {
            seg_adv = seg_adv + f32(kk) * trie.cell_adv;
        }
        // ONE byte, not the lead's declared length: the reference classifies
        // every byte on its own (`is_leader_byte`), so a lead whose
        // "continuation" bytes are really ASCII yields a record for the lead
        // AND one per ASCII byte. Well-formed text is unaffected — the
        // continuation bytes classify as non-leaders and are skipped above
        // (`malformed.pipe.bin` on the oracle tier, 2026-10-10).
        i = i + 1u;
    }
    return survivors;
}

@compute @workgroup_size(64)
fn layout_segments(@builtin(global_invocation_id) gid: vec3<u32>) {
    let v = linear_id(gid);
    if (v >= params.count) { return; }
    let seg = segs[v];
    if (seg.byte_end <= seg.byte_off) { return; }   // an empty entry (a line dropped at the slot cap)
    if (params.mode == MODE_MASK) {
        // The selection's segments only: the item's, intersecting the range
        // (item-relative, as the filter is).
        if (seg.item != params.filter_item) { return; }
        let s = lines[seg.line].byte_start;
        if (s + seg.byte_end <= params.filter_lo || s + seg.byte_off >= params.filter_hi) { return; }
    }
    let _n = walk_segment(seg, true);
}

// After a mask dispatch: the mask draw's DrawIndexedIndirectArgs from the
// counter, clamped to the buffer (a selection past `mask_cap` is truncated,
// never read past the buffer). first_instance 0, as Metal requires.
@compute @workgroup_size(1)
fn finalize_mask() {
    let n = min(atomicLoad(&mask_args[0]), params.mask_cap);
    atomicStore(&mask_args[4], 6u);
    atomicStore(&mask_args[5], n);
    atomicStore(&mask_args[6], 0u);
    atomicStore(&mask_args[7], 0u);
    atomicStore(&mask_args[8], 0u);
}

// At load: the survivors of the segment that ENDS at seed `v` — from the
// previous seed of the same line (or the line's start) to this cut.
@compute @workgroup_size(64)
fn count_seed_segments(@builtin(global_invocation_id) gid: vec3<u32>) {
    let v = linear_id(gid);
    if (v >= params.count) { return; }
    let s = seeds[v];
    var seg: Seg;
    seg.line = s.line;
    seg.item = lines[s.line].item;
    seg.byte_end = s.byte_offset;
    seg.byte_off = 0u;
    seg.col = 0u;
    seg.cells = 0u;
    seg.seg_adv = 0.0;
    seg.slot_base = 0u;
    if (v > 0u && seeds[v - 1u].line == s.line) {
        let p = seeds[v - 1u];
        seg.byte_off = p.byte_offset;
        seg.col = p.col;
        seg.cells = p.cells;
        seg.seg_adv = p.seg_adv;
    }
    survivors[v] = walk_segment(seg, false);
}

// Then: each seed's survivors-before = the counts of its line's seeds up to
// and including it (seeds are sorted by line, then offset). Serial per seed
// over its predecessors — seeds per line are few (one per 2 KiB).
@compute @workgroup_size(64)
fn prefix_seed_survivors(@builtin(global_invocation_id) gid: vec3<u32>) {
    let v = linear_id(gid);
    if (v >= params.count) { return; }
    let line = seeds[v].line;
    var sum = 0u;
    var j = v;
    loop {
        if (seeds[j].line != line) { break; }
        sum = sum + survivors[j];
        if (j == 0u) { break; }
        j = j - 1u;
    }
    // Every invocation reads its predecessors' counts before any writes its
    // prefix: the counts come from the previous dispatch, and this one
    // writes `survivors` only at its own index after reading — but a
    // neighbour may have already overwritten ITS index with a prefix.
    // So the prefix goes to the upper half: `survivors` holds counts in
    // [0, n) and prefixes in [n, 2n).
    survivors[params.count + v] = sum;
}
