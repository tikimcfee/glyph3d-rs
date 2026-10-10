// layout.wgsl — one invocation per VISIBLE SEGMENT; a serial fold over its bytes.
//
// Mirrors the Derived slot (20 B): { x, row, glyph_id | wrap_seg << 16, color, item }.
// The CPU reference in main.rs (`Fold`, `layout_line`) is the same algorithm
// step for step; the readback checks hold the two bit-equal.
//
// A segment is a whole line, or a piece of a long line cut before an ASCII
// byte (HyperLayout's intra-line cut rule). A continuation carries its seeds:
// the column reached, the running f32 advance sum since the last wrap-unit
// boundary (the fold's own x, not a product), and the paint state. Folding a
// segment from its seeds yields the bytes the whole-line fold would have
// written at those slots.
//
// The segment count is read from `counters` (written by the GPU cull or by the
// CPU), so the dispatch can be indirect; the dispatch is 2-D because a count
// over 65535 * 64 segments must be.
//
// Colour, by the item's representation (`Item.repr`, a branch per segment):
//   0 none          the `//` comment heuristic (the original prototype)
//   1 spans         sorted byte-range spans for the file; the segment starts at
//                   its line's span index and walks forward as bytes advance
//   2 dense colour  one u32 per glyph, indexed by the glyph's ordinal in the file
//   3 dense + xf    as 2, plus a per-glyph transform (4 x f16: dx, dy, dz, scale)
//                   copied into a parallel transient stream the draw reads
//
// The corpus bytes may be split across up to four buffers when the adapter's
// binding limit is below the corpus size; an item never straddles a chunk
// (the CPU pads to the boundary), so a segment lives in one chunk.

struct Seg { byte_start: u32, byte_len: u32, line_idx: u32, item: u32, slot_base: u32, col_seed: u32, x_seed: f32, state: u32 }
struct Item { first_line: u32, line_count: u32, byte_start: u32, byte_len: u32, max_len: u32, glyph_count: u32, span_base: u32, span_count: u32, repr: u32, dense_base: u32, _p0: u32, _p1: u32 }
struct Line { byte_start: u32, glyph_prefix: u32 }
struct Slot { x: f32, row: u32, glyph_and_wrap: u32, color: u32, item: u32 }
struct Params { wrap_cols: u32, chunk_shift: u32, chunk_mask: u32, default_color: u32, _p0: u32, _p1: u32, _p2: u32, _p3: u32 }
struct Counters { vis_files: atomic<u32>, backdrops: atomic<u32>, cand_lines: atomic<u32>, vis_lines: atomic<u32>, seg_count: atomic<u32>, slot_total: atomic<u32>, dropped_lines: atomic<u32>, draw_limit: atomic<u32> }

@group(0) @binding(0) var<storage, read> bytes0: array<u32>;      // the corpus, 1 B per source byte, chunk 0
@group(0) @binding(1) var<storage, read> bytes1: array<u32>;      // chunk 1 (a 4 B dummy when unused)
@group(0) @binding(2) var<storage, read> bytes2: array<u32>;      // chunk 2
@group(0) @binding(3) var<storage, read> bytes3: array<u32>;      // chunk 3
@group(0) @binding(4) var<storage, read> items: array<Item>;      // 48 B per item, resident
@group(0) @binding(5) var<storage, read> segs: array<Seg>;        // 32 B per visible segment, per frame
@group(0) @binding(6) var<storage, read> advances: array<f32>;    // 256 entries, ASCII advances
@group(0) @binding(7) var<storage, read_write> slots: array<Slot>; // transient output
@group(0) @binding(8) var<uniform> params: Params;
@group(0) @binding(9) var<storage, read_write> counters: Counters;
@group(0) @binding(10) var<storage, read> lines: array<Line>;     // 8 B per line, resident
@group(0) @binding(11) var<storage, read> spans: array<vec2<u32>>; // { byte_start (file-local), len | palette << 24 }, resident
@group(0) @binding(12) var<storage, read> line_span_idx: array<u32>; // per line: first span that can cover it
@group(0) @binding(13) var<storage, read> palette: array<u32>;    // 256 colours
@group(0) @binding(14) var<storage, read> dense_color: array<u32>; // per glyph of a dense file
@group(0) @binding(15) var<storage, read> dense_xf: array<vec2<u32>>; // per glyph of a dense+xf file
@group(0) @binding(16) var<storage, read_write> slot_xf: array<vec2<u32>>; // transient, written for dense+xf slots only

const COLOR_CODE: u32 = 0xFFD8E2E8u;
const COLOR_COMMENT: u32 = 0xFF5AB06Au;

fn word_at(i: u32) -> u32 {
    let c = i >> params.chunk_shift;
    let w = (i & params.chunk_mask) >> 2u;
    var r = 0u;
    switch c {
        case 0u: { r = bytes0[w]; }
        case 1u: { r = bytes1[w]; }
        case 2u: { r = bytes2[w]; }
        default: { r = bytes3[w]; }
    }
    return r;
}

fn byte_at(i: u32) -> u32 {
    return (word_at(i) >> ((i & 3u) * 8u)) & 0xFFu;
}

fn span_end(s: u32) -> u32 { return spans[s].x + (spans[s].y & 0xFFFFFFu); }

@compute @workgroup_size(64)
fn layout_segments(@builtin(workgroup_id) wg: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>, @builtin(local_invocation_id) lid: vec3<u32>) {
    let v = (wg.y * nwg.x + wg.x) * 64u + lid.x;
    if (v >= atomicLoad(&counters.seg_count)) { return; }
    let seg = segs[v];
    let it = items[seg.item];
    let row = seg.line_idx - it.first_line;
    let repr = it.repr;
    var i = seg.byte_start;
    let end = seg.byte_start + seg.byte_len;
    var x = seg.x_seed;
    var col = seg.col_seed;
    var k = seg.slot_base;
    var in_comment = (seg.state & 1u) != 0u;
    var prev_slash = (seg.state & 2u) != 0u;
    // Spans: start at the line's index, then walk to this segment's first byte (a continuation).
    // The current span lives in registers (start, end, colour) and the NEXT span is prefetched with
    // its colour, so crossing a span costs no load on the serial chain: the table is touched one span
    // ahead, overlapped with the byte loop. (Measured: without the prefetch a 2048 B minified line
    // crossing a span every few bytes ran 10x slower — a latency chain, not bandwidth.)
    var si = 0u;
    var s_end = 0u;
    var sp_start = 0xFFFFFFFFu;
    var sp_end = 0xFFFFFFFFu;
    var sp_color = params.default_color;
    var nx = vec2<u32>(0xFFFFFFFFu, 0u);
    var nx_color = params.default_color;
    if (repr == 1u) {
        si = line_span_idx[seg.line_idx];
        s_end = it.span_base + it.span_count;
        // A continuation segment finds the first span past its cut by binary search over the rest of the
        // file's spans (a linear walk from the line's index cost 6 ms a frame on a view of 100 KB minified lines).
        if (seg.col_seed != 0u) {
            let fb = i - it.byte_start;
            var hi = s_end;
            while (si < hi) {
                let mid = (si + hi) / 2u;
                if (span_end(mid) <= fb) { si = mid + 1u; } else { hi = mid; }
            }
        }
        if (si < s_end) { let sp = spans[si]; sp_start = sp.x; sp_end = sp.x + (sp.y & 0xFFFFFFu); sp_color = palette[sp.y >> 24u]; }
        if (si + 1u < s_end) { nx = spans[si + 1u]; nx_color = palette[nx.y >> 24u]; }
    }
    // Dense: the glyph's ordinal in the file is the line's glyph prefix plus its column.
    var ord = 0u;
    if (repr >= 2u) { ord = it.dense_base + lines[seg.line_idx].glyph_prefix + col; }
    while (i < end) {
        let b = byte_at(i);
        var glyph = b;
        var adv = 0.0;
        var len = 1u;
        if (b < 0x80u) {
            adv = advances[b];
            if (b == 0x2Fu) {
                in_comment = in_comment || prev_slash;
                prev_slash = true;
            } else {
                prev_slash = false;
            }
        } else {
            var cp = 0u;
            if (b >= 0xF0u) { len = 4u; cp = b & 0x07u; }
            else if (b >= 0xE0u) { len = 3u; cp = b & 0x0Fu; }
            else { len = 2u; cp = b & 0x1Fu; }
            for (var j = 1u; j < len; j = j + 1u) {
                cp = (cp << 6u) | (byte_at(i + j) & 0x3Fu);
            }
            glyph = cp & 0xFFFFu;
            adv = 1.0;
            prev_slash = false;
        }
        if (col % params.wrap_cols == 0u) { x = 0.0; }   // the fold's wrap: x resets per wrap unit
        let wrap = col / params.wrap_cols;
        var color = params.default_color;
        if (repr == 0u) {
            color = select(COLOR_CODE, COLOR_COMMENT, in_comment);
        } else if (repr == 1u) {
            let fb = i - it.byte_start;
            while (sp_end <= fb) {
                si = si + 1u;
                sp_start = nx.x;
                sp_end = nx.x + (nx.y & 0xFFFFFFu);
                sp_color = nx_color;
                if (nx.x == 0xFFFFFFFFu) { sp_end = 0xFFFFFFFFu; }
                if (si + 1u < s_end) { nx = spans[si + 1u]; nx_color = palette[nx.y >> 24u]; } else { nx = vec2<u32>(0xFFFFFFFFu, 0u); }
            }
            if (sp_start <= fb) { color = sp_color; }
        } else {
            color = dense_color[ord];
            if (repr == 3u) { slot_xf[k] = dense_xf[ord]; }
            ord = ord + 1u;
        }
        slots[k] = Slot(x, row, glyph | (wrap << 16u), color, seg.item);
        x = x + adv;
        col = col + 1u;
        k = k + 1u;
        i = i + len;
    }
}
