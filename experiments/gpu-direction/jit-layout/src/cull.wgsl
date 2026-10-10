// cull.wgsl — the GPU visible set, four small passes, no readback between them:
//
//   cull_files    one invocation per FILE: box vs frustum; a survivor whose rows
//                 project below `lod_px` pixels becomes a backdrop (drawn as one
//                 quad, never laid out), the rest append to `vis`.
//   prefix_lines  one workgroup: exclusive prefix of the visible files' line
//                 counts (so a line invocation can find its file), and the
//                 indirect dispatch for the next pass.
//   cull_lines    one invocation per CANDIDATE LINE of a visible file, dispatched
//                 indirectly: line box vs frustum; a survivor reserves its slot
//                 range with one atomicAdd and appends its segment(s) — one for
//                 a line, several for a long line, from the resident seed table.
//   finalize      one thread: the layout dispatch and the glyph draw arguments.
//
// Order of segments and slots is whatever the atomics make it — not stable
// across frames, which the layout does not need. Caps: a line whose slots do
// not fit below `cap_slots` is dropped and lowers `draw_limit` to its base, so
// the draw never touches a slot no kernel wrote this frame; segments past
// `cap_segs` are written as empty (zero bytes) so the layout skips them.

struct Camera { vp: mat4x4<f32>, planes: array<vec4<f32>, 6>, viewport: vec2<f32>, lod_px: f32, px_scale: f32, eye: vec3<f32>, tint: u32, default_color: u32, flags: u32, _p: vec2<u32> }
struct FileBox { ox: f32, oy: f32, w: f32, h: f32, first_line: u32, line_count: u32, depth: f32, _p: u32 }
struct Item { first_line: u32, line_count: u32, byte_start: u32, byte_len: u32, max_len: u32, glyph_count: u32, span_base: u32, span_count: u32, repr: u32, dense_base: u32, _p0: u32, _p1: u32 }
struct Line { byte_start: u32, glyph_prefix: u32 }
struct VisFile { item: u32, first_line: u32, line_count: u32, cum: u32 }
struct SeedDir { line_idx: u32, seed_base: u32, seed_count: u32, _p: u32 }
struct Seed { byte_off: u32, col: u32, x: f32, state: u32 }
struct Seg { byte_start: u32, byte_len: u32, line_idx: u32, item: u32, slot_base: u32, col_seed: u32, x_seed: f32, state: u32 }
struct Counters { vis_files: atomic<u32>, backdrops: atomic<u32>, cand_lines: atomic<u32>, vis_lines: atomic<u32>, seg_count: atomic<u32>, slot_total: atomic<u32>, dropped_lines: atomic<u32>, draw_limit: atomic<u32> }
struct CullParams { n_items: u32, segment_bytes: u32, wrap_cols: u32, cap_segs: u32, cap_slots: u32, _p0: u32, _p1: u32, _p2: u32 }

@group(0) @binding(0) var<uniform> cam: Camera;
@group(0) @binding(1) var<uniform> cp: CullParams;
@group(0) @binding(2) var<storage, read> boxes: array<FileBox>;
@group(0) @binding(3) var<storage, read> items: array<Item>;
@group(0) @binding(4) var<storage, read> lines: array<Line>;
@group(0) @binding(5) var<storage, read> seed_dir: array<SeedDir>;   // sorted by line_idx, sentinel last
@group(0) @binding(6) var<storage, read> seeds: array<Seed>;
@group(0) @binding(7) var<storage, read_write> vis: array<VisFile>;
@group(0) @binding(8) var<storage, read_write> backdrops: array<u32>;
@group(0) @binding(9) var<storage, read_write> segs: array<Seg>;
@group(0) @binding(10) var<storage, read_write> counters: Counters;
// Its own group: only prefix_lines and finalize bind it, so the passes that DISPATCH from it never also bind it.
@group(1) @binding(0) var<storage, read_write> indirect: array<u32>; // [0,3) dispatch lines  [3,6) dispatch layout  [6,10) draw glyphs  [10,14) draw backdrops

const ROW_H: f32 = 1.2;
const MAX_ADV: f32 = 2.4;   // a tab; the widest advance, so glyphs * MAX_ADV bounds a line's x extent

fn box_visible(mn: vec3<f32>, mx: vec3<f32>) -> bool {
    for (var i = 0u; i < 6u; i = i + 1u) {
        let p = cam.planes[i];
        let v = vec3<f32>(select(mn.x, mx.x, p.x >= 0.0), select(mn.y, mx.y, p.y >= 0.0), select(mn.z, mx.z, p.z >= 0.0));
        if (dot(p.xyz, v) + p.w < 0.0) { return false; }
    }
    return true;
}

@compute @workgroup_size(64)
fn cull_files(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= cp.n_items) { return; }
    let b = boxes[i];
    if (b.line_count == 0u) { return; }
    let y1 = b.oy + ROW_H;
    if (!box_visible(vec3<f32>(b.ox, y1 - b.h, -b.depth), vec3<f32>(b.ox + b.w, y1, 0.0))) { return; }
    // LOD: the projected height of one row at the box's nearest face (z = 0).
    let dist = select(max(cam.eye.z, 1e-3), 1.0, (cam.flags & 1u) != 0u);
    if (ROW_H * cam.px_scale / dist < cam.lod_px) {
        backdrops[atomicAdd(&counters.backdrops, 1u)] = i;
        return;
    }
    let k = atomicAdd(&counters.vis_files, 1u);
    vis[k] = VisFile(i, b.first_line, b.line_count, 0u);
}

var<workgroup> partial: array<u32, 256>;

@compute @workgroup_size(256)
fn prefix_lines(@builtin(local_invocation_id) lid: vec3<u32>) {
    let n = atomicLoad(&counters.vis_files);
    let per = (n + 255u) / 256u;
    let start = min(lid.x * per, n);
    let end = min(start + per, n);
    var sum = 0u;
    for (var j = start; j < end; j = j + 1u) { sum = sum + vis[j].line_count; }
    partial[lid.x] = sum;
    workgroupBarrier();
    var excl = 0u;
    for (var j = 0u; j < lid.x; j = j + 1u) { excl = excl + partial[j]; }
    for (var j = start; j < end; j = j + 1u) { vis[j].cum = excl; excl = excl + vis[j].line_count; }
    if (lid.x == 255u) {
        atomicStore(&counters.cand_lines, excl);
        let groups = (excl + 63u) / 64u;
        indirect[0] = min(groups, 65535u);
        indirect[1] = (groups + 65534u) / 65535u;
        indirect[2] = 1u;
        indirect[10] = 6u;
        indirect[11] = atomicLoad(&counters.backdrops);
        indirect[12] = 0u;
        indirect[13] = 0u;
    }
}

@compute @workgroup_size(64)
fn cull_lines(@builtin(workgroup_id) wg: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>, @builtin(local_invocation_id) lid: vec3<u32>) {
    let g = (wg.y * nwg.x + wg.x) * 64u + lid.x;
    if (g >= atomicLoad(&counters.cand_lines)) { return; }
    // The last visible file whose prefix is <= g.
    var lo = 0u;
    var hi = atomicLoad(&counters.vis_files);
    while (lo + 1u < hi) {
        let mid = (lo + hi) / 2u;
        if (vis[mid].cum <= g) { lo = mid; } else { hi = mid; }
    }
    let f = vis[lo];
    let row = g - f.cum;
    let li = f.first_line + row;
    let b = boxes[f.item];
    let it = items[f.item];
    let ln = lines[li];
    let last = li + 1u == it.first_line + it.line_count;
    let next_prefix = select(lines[li + 1u].glyph_prefix, it.glyph_count, last);
    let glyphs = next_prefix - ln.glyph_prefix;
    let len = min(lines[li + 1u].byte_start, it.byte_start + it.byte_len) - ln.byte_start - 1u;
    let w = min(b.w, f32(min(glyphs, cp.wrap_cols)) * MAX_ADV);
    let y1 = b.oy - f32(row) * ROW_H + ROW_H;
    if (!box_visible(vec3<f32>(b.ox, y1 - ROW_H, -b.depth), vec3<f32>(b.ox + w, y1, 0.0))) { return; }
    atomicAdd(&counters.vis_lines, 1u);
    let base = atomicAdd(&counters.slot_total, glyphs);
    if (base + glyphs > cp.cap_slots) {
        atomicAdd(&counters.dropped_lines, 1u);
        atomicMin(&counters.draw_limit, base);
        return;
    }
    var sd_base = 0u;
    var sd_count = 0u;
    if (len > cp.segment_bytes) {
        let m = arrayLength(&seed_dir);
        var l = 0u;
        var h = m;
        while (l < h) {
            let mid = (l + h) / 2u;
            if (seed_dir[mid].line_idx < li) { l = mid + 1u; } else { h = mid; }
        }
        if (l < m && seed_dir[l].line_idx == li) { sd_base = seed_dir[l].seed_base; sd_count = seed_dir[l].seed_count; }
    }
    let nseg = sd_count + 1u;
    let s0 = atomicAdd(&counters.seg_count, nseg);
    if (s0 + nseg > cp.cap_segs) {
        atomicAdd(&counters.dropped_lines, 1u);
        atomicMin(&counters.draw_limit, base);
        for (var k = s0; k < min(s0 + nseg, cp.cap_segs); k = k + 1u) { segs[k] = Seg(ln.byte_start, 0u, li, f.item, base, 0u, 0.0, 0u); }
        return;
    }
    var prev = Seed(0u, 0u, 0.0, 0u);
    for (var k = 0u; k < sd_count; k = k + 1u) {
        let sd = seeds[sd_base + k];
        segs[s0 + k] = Seg(ln.byte_start + prev.byte_off, sd.byte_off - prev.byte_off, li, f.item, base + prev.col, prev.col, prev.x, prev.state);
        prev = sd;
    }
    segs[s0 + sd_count] = Seg(ln.byte_start + prev.byte_off, len - prev.byte_off, li, f.item, base + prev.col, prev.col, prev.x, prev.state);
}

@compute @workgroup_size(1)
fn finalize() {
    let s = min(atomicLoad(&counters.seg_count), cp.cap_segs);
    let groups = (s + 63u) / 64u;
    indirect[3] = min(groups, 65535u);
    indirect[4] = (groups + 65534u) / 65535u;
    indirect[5] = 1u;
    indirect[6] = 6u;
    indirect[7] = min(atomicLoad(&counters.slot_total), atomicLoad(&counters.draw_limit));
    indirect[8] = 0u;
    indirect[9] = 0u;
}
