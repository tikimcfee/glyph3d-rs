// pick.wgsl — (item, byte offset) -> transient slot, with no per-glyph storage.
//
// One workgroup per query. Every thread finds the byte's line by binary search
// on the item's line table (lines are sorted by byte_start), then the workgroup
// strides over the frame's segment list — unordered, so a search, not a lookup —
// and the thread whose segment covers the byte counts the glyph-leading bytes
// from the segment's start to the probe: slot = slot_base + that count. The
// result is 0xFFFFFFFF when no visible segment covers the byte.

struct Item { first_line: u32, line_count: u32, byte_start: u32, byte_len: u32, max_len: u32, glyph_count: u32, span_base: u32, span_count: u32, repr: u32, dense_base: u32, _p0: u32, _p1: u32 }
struct Line { byte_start: u32, glyph_prefix: u32 }
struct Seg { byte_start: u32, byte_len: u32, line_idx: u32, item: u32, slot_base: u32, col_seed: u32, x_seed: f32, state: u32 }
struct Counters { vis_files: atomic<u32>, backdrops: atomic<u32>, cand_lines: atomic<u32>, vis_lines: atomic<u32>, seg_count: atomic<u32>, slot_total: atomic<u32>, dropped_lines: atomic<u32>, draw_limit: atomic<u32> }
struct Params { wrap_cols: u32, chunk_shift: u32, chunk_mask: u32, default_color: u32, _p0: u32, _p1: u32, _p2: u32, _p3: u32 }
struct Query { item: u32, byte: u32 }

@group(0) @binding(0) var<storage, read> bytes0: array<u32>;
@group(0) @binding(1) var<storage, read> bytes1: array<u32>;
@group(0) @binding(2) var<storage, read> bytes2: array<u32>;
@group(0) @binding(3) var<storage, read> bytes3: array<u32>;
@group(0) @binding(4) var<storage, read> items: array<Item>;
@group(0) @binding(5) var<storage, read> lines: array<Line>;
@group(0) @binding(6) var<storage, read> segs: array<Seg>;
@group(0) @binding(7) var<storage, read_write> counters: Counters;
@group(0) @binding(8) var<uniform> params: Params;
@group(0) @binding(9) var<storage, read> queries: array<Query>;
@group(0) @binding(10) var<storage, read_write> results: array<u32>;

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

fn byte_at(i: u32) -> u32 { return (word_at(i) >> ((i & 3u) * 8u)) & 0xFFu; }

@compute @workgroup_size(256)
fn pick(@builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_id) lid: vec3<u32>) {
    let q = queries[wg.x];
    let it = items[q.item];
    let b = it.byte_start + q.byte;
    var lo = it.first_line;
    var hi = it.first_line + it.line_count;
    while (lo + 1u < hi) {
        let mid = (lo + hi) / 2u;
        if (lines[mid].byte_start <= b) { lo = mid; } else { hi = mid; }
    }
    let li = lo;
    let n = atomicLoad(&counters.seg_count);
    for (var s = lid.x; s < n; s = s + 256u) {
        let sg = segs[s];
        if (sg.line_idx == li && sg.byte_start <= b && b < sg.byte_start + sg.byte_len) {
            var k = sg.slot_base;
            for (var i = sg.byte_start; i < b; i = i + 1u) {
                if ((byte_at(i) & 0xC0u) != 0x80u) { k = k + 1u; }
            }
            results[wg.x] = k;
        }
    }
}
