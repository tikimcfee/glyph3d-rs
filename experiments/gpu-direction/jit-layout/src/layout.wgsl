// layout.wgsl — one invocation per VISIBLE line; a serial fold over its bytes.
//
// Mirrors the Derived slot (20 B): { x, row, glyph_id | wrap_seg << 16, color, item }.
// The CPU reference in main.rs (`layout_line`) is the same algorithm step for
// step; the readback check holds the two bit-equal.

struct Line { byte_start: u32, byte_len: u32, item: u32, row: u32 }
struct Vis { line_idx: u32, slot_base: u32 }
struct Slot { x: f32, row: u32, glyph_and_wrap: u32, color: u32, item: u32 }
struct Params { visible_count: u32, wrap_cols: u32, _pad0: u32, _pad1: u32 }

@group(0) @binding(0) var<storage, read> bytes: array<u32>;       // the corpus, 1 B per source byte
@group(0) @binding(1) var<storage, read> lines: array<Line>;      // 16 B per line, resident
@group(0) @binding(2) var<storage, read> visible: array<Vis>;     // 8 B per visible line, per frame
@group(0) @binding(3) var<storage, read> advances: array<f32>;    // 256 entries, ASCII advances
@group(0) @binding(4) var<storage, read_write> slots: array<Slot>; // transient output
@group(0) @binding(5) var<uniform> params: Params;

fn byte_at(i: u32) -> u32 {
    return (bytes[i >> 2u] >> ((i & 3u) * 8u)) & 0xFFu;
}

@compute @workgroup_size(64)
fn layout_lines(@builtin(global_invocation_id) gid: vec3<u32>) {
    let v = gid.x;
    if (v >= params.visible_count) { return; }
    let vis = visible[v];
    let line = lines[vis.line_idx];
    var i = line.byte_start;
    let end = line.byte_start + line.byte_len;
    var x = 0.0;
    var col = 0u;
    var k = vis.slot_base;
    var in_comment = false;
    var prev_slash = false;
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
        if (col % params.wrap_cols == 0u) { x = 0.0; }   // the fold's wrap: x resets per segment
        let seg = col / params.wrap_cols;
        var color = 0xFFFFFFFFu;
        if (in_comment) { color = 0xFF00FF00u; }
        slots[k] = Slot(x, line.row, glyph | (seg << 16u), color, line.item);
        x = x + adv;
        col = col + 1u;
        k = k + 1u;
        i = i + len;
    }
}
