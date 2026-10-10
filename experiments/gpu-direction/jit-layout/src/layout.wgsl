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
// The corpus bytes may be split across up to four buffers when the adapter's
// binding limit is below the corpus size; an item never straddles a chunk
// (the CPU pads to the boundary), so a segment lives in one chunk.

struct Seg { byte_start: u32, byte_len: u32, line_idx: u32, item: u32, slot_base: u32, col_seed: u32, x_seed: f32, state: u32 }
struct Item { first_line: u32, line_count: u32, byte_start: u32, byte_len: u32, max_len: u32, longest_line: u32, _p0: u32, _p1: u32 }
struct Slot { x: f32, row: u32, glyph_and_wrap: u32, color: u32, item: u32 }
struct Params { segment_count: u32, wrap_cols: u32, chunk_shift: u32, chunk_mask: u32 }

@group(0) @binding(0) var<storage, read> bytes0: array<u32>;      // the corpus, 1 B per source byte, chunk 0
@group(0) @binding(1) var<storage, read> bytes1: array<u32>;      // chunk 1 (a 4 B dummy when unused)
@group(0) @binding(2) var<storage, read> bytes2: array<u32>;      // chunk 2
@group(0) @binding(3) var<storage, read> bytes3: array<u32>;      // chunk 3
@group(0) @binding(4) var<storage, read> items: array<Item>;      // 32 B per item, resident
@group(0) @binding(5) var<storage, read> segs: array<Seg>;        // 32 B per visible segment, per frame
@group(0) @binding(6) var<storage, read> advances: array<f32>;    // 256 entries, ASCII advances
@group(0) @binding(7) var<storage, read_write> slots: array<Slot>; // transient output
@group(0) @binding(8) var<uniform> params: Params;

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

@compute @workgroup_size(64)
fn layout_segments(@builtin(global_invocation_id) gid: vec3<u32>) {
    let v = gid.x;
    if (v >= params.segment_count) { return; }
    let seg = segs[v];
    let row = seg.line_idx - items[seg.item].first_line;
    var i = seg.byte_start;
    let end = seg.byte_start + seg.byte_len;
    var x = seg.x_seed;
    var col = seg.col_seed;
    var k = seg.slot_base;
    var in_comment = (seg.state & 1u) != 0u;
    var prev_slash = (seg.state & 2u) != 0u;
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
        var color = COLOR_CODE;
        if (in_comment) { color = COLOR_COMMENT; }
        slots[k] = Slot(x, row, glyph | (wrap << 16u), color, seg.item);
        x = x + adv;
        col = col + 1u;
        k = k + 1u;
        i = i + len;
    }
}
