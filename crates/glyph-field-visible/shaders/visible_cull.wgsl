// visible_cull.wgsl — the Visible field's per-frame cull, four dispatches in
// one compute pass, nothing read back:
//
//   cull_items     one invocation per item: its world box (the scene's own
//                  `SegCull` box, group offset applied) against the frustum,
//                  the `hidden` bit, and the projected row height at the
//                  box's nearest point — under `lod_backdrop_px` the item is
//                  a BACKDROP (the scene draws its quad; nothing is laid
//                  out), else it is appended to `visible` (atomic order).
//   prefix_items   one workgroup: an exclusive scan of the visible items'
//                  line counts gives each its candidate-line base, and the
//                  total plans cull_lines' indirect dispatch.
//   cull_lines     one invocation per candidate line, dispatched indirectly:
//                  finds its item by binary search over the bases, builds the
//                  line's world box (x from its byte length, rows and depth
//                  segments from the fold rules, pages from `Pager`), tests
//                  it, and sorts a visible line into the GLYPH tier (one
//                  segment entry per cut, slots reserved for the whole line
//                  at once) or the WASH tier (one quad in its spans' mean
//                  colour).
//   finalize       one invocation: the layout dispatch and both draws'
//                  indirect arguments from the counters.
//
// CAPS. Slots and segments are reserved by monotone atomics, so every line
// that fits precedes every line that does not: `slot_fit_end` /
// `seg_fit_end` (atomicMax of the fitting reservations) are contiguous,
// fully written ranges, and the draw and the layout dispatch read those,
// never the raw counters. A line dropped at the slot cap still writes its
// reserved segment entries, EMPTY, so the segment range has no hole.
//
// The frustum planes come from the host (`frustum_planes`, the same
// Gribb-Hartmann extraction as the renderer's `cull.rs`); the test is the
// positive-vertex test on a world AABB.

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
    override_base: u32,      // the layout kernel's; the cull never reads them
    override_count: u32,
    _pad2: u32,
    _pad3: u32,
    _pad4: u32,
};

struct ItemParamsGpu {
    line_height: f32,
    origin_y: f32,
    origin_z: f32,
    z_step: f32,
    z_step_lo: f32,
    band_stride_y: f32,
    depth_per_band: f32,
    depth_per_col: f32,
    page_rows: i32,
    pages_wide: i32,
    page_cols: i32,
    scroll_rows: i32,
    has_page: u32,
    line_height_lo: f32,
    group: u32,
    _pad2: u32,
};

struct LineEntry { byte_start: u32, item: u32, base_row: u32, glyph_count: u32 };
struct SegmentSeed { line: u32, byte_offset: u32, col: u32, seg_adv: f32, cells: u32, _pad: u32 };
struct ByteSpan { start: u32, end: u32, color: u32 };
struct Seg { item: u32, line: u32, byte_off: u32, byte_end: u32, col: u32, cells: u32, seg_adv: f32, slot_base: u32 };
// One WASH quad: the line's first row (row lane as the Derived slot
// carries it), its x extent, its mean colour, how many rows it stacks, its
// alpha, and a debug tint (0 = none).
struct Wash { item: u32, row_lane: u32, x0: f32, width: f32, color: u32, rows: u32, alpha: f32, tint: u32 };

struct Frame {
    view_proj: mat4x4<f32>,
    planes: array<vec4<f32>, 6>,
    eye: vec4<f32>,      // xyz the eye, w px_scale
    lod: vec4<f32>,      // x lod_glyph_px, y lod_backdrop_px, z time, w cell_adv
    u0: vec4<u32>,       // greek_mode, debug_tint, items_total, seeds_total
    u1: vec4<u32>,       // max_slots, max_segments, max_wash, default_color
};

@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var<storage, read> items: array<ItemGpu>;
@group(0) @binding(2) var<storage, read> item_params: array<ItemParamsGpu>;
@group(0) @binding(3) var<storage, read> hidden: array<u32>;
@group(0) @binding(4) var<storage, read> lines: array<LineEntry>;
@group(0) @binding(5) var<storage, read> seeds: array<SegmentSeed>;
@group(0) @binding(6) var<storage, read> survivors: array<u32>;   // counts in [0, n), prefixes in [n, 2n)
@group(0) @binding(7) var<storage, read> spans: array<ByteSpan>;
@group(0) @binding(8) var<storage, read> first_span: array<u32>;
@group(0) @binding(9) var<storage, read> groups: array<vec4<f32>>;
@group(0) @binding(10) var<storage, read_write> visible: array<u32>;
@group(0) @binding(11) var<storage, read_write> line_base: array<u32>;
@group(0) @binding(12) var<storage, read_write> counters: array<atomic<u32>, 16>;
@group(0) @binding(13) var<storage, read_write> segs: array<Seg>;
@group(0) @binding(14) var<storage, read_write> wash: array<Wash>;
// The dispatch and draw arguments, copied into the INDIRECT buffer by the host between passes.
@group(0) @binding(15) var<storage, read_write> indirect: array<u32>;

// Counter slots (stats.rs reads the same map).
const C_ITEMS_VISIBLE: u32 = 0u;
const C_ITEMS_BACKDROP: u32 = 1u;
const C_LINES_CANDIDATE: u32 = 2u;
const C_LINES_GLYPH: u32 = 3u;
const C_LINES_WASH: u32 = 4u;
const C_SEGMENTS: u32 = 5u;
const C_SLOTS: u32 = 6u;
const C_SLOTS_DROPPED: u32 = 7u;
const C_SEG_FIT_END: u32 = 8u;
const C_SLOT_FIT_END: u32 = 9u;
const C_WASH: u32 = 10u;
const C_LINES_DROPPED: u32 = 11u;
const C_ITEMS_HIDDEN: u32 = 12u;
const C_ITEMS_CULLED: u32 = 13u;

// Indirect buffer map (u32 offsets): cull_lines dispatch, layout dispatch,
// the glyph draw's DrawIndexedIndirectArgs, the wash draw's.
const I_CULL_B: u32 = 0u;
const I_LAYOUT: u32 = 4u;
const I_DRAW: u32 = 8u;
const I_WASH_DRAW: u32 = 16u;

const GROUP_STRIDE: u32 = 6u;
const WG: u32 = 64u;
const MAX_GROUPS_X: u32 = 65535u;
const ROW_MAX: u32 = 0xFFFFFFu;
const X_PAGE_MAX: u32 = 0xFFu;

const TINT_WASH_TIER: u32 = 0xFFFF8040u;     // wash tier: blue
const TINT_WASH_FULL: u32 = 0xFFFF40FFu;     // cull state: a full wash (under the glyph threshold): magenta
const TINT_WASH_BAND: u32 = 0xFFFFFF40u;     // cull state: a wash fading in over the 1 px band: cyan
const TINT_DROPPED: u32 = 0xFF4040FFu;       // cull state: a glyph-tier line dropped at a cap: red

fn linear_id(gid: vec3<u32>) -> u32 {
    return gid.y * (MAX_GROUPS_X * WG) + gid.x;
}

// `plan_dispatch` (lib.rs): groups of 64, x capped at 65,535, y the rest.
fn write_dispatch(at: u32, count: u32) {
    let groups = (count + WG - 1u) / WG;
    indirect[at] = min(groups, MAX_GROUPS_X);
    indirect[at + 1u] = max((groups + MAX_GROUPS_X - 1u) / MAX_GROUPS_X, 1u);
    indirect[at + 2u] = 1u;
}

struct Box { lo: vec3<f32>, hi: vec3<f32> };

// Positive-vertex test: inside (or straddling) every plane.
fn box_in_frustum(b: Box) -> bool {
    for (var k = 0u; k < 6u; k = k + 1u) {
        let p = frame.planes[k];
        let v = vec3<f32>(
            select(b.lo.x, b.hi.x, p.x > 0.0),
            select(b.lo.y, b.hi.y, p.y > 0.0),
            select(b.lo.z, b.hi.z, p.z > 0.0),
        );
        if (dot(p.xyz, v) + p.w < 0.0) { return false; }
    }
    return true;
}

// Distance from the eye to the box's nearest point (0 inside).
fn box_distance(b: Box) -> f32 {
    let c = clamp(frame.eye.xyz, b.lo, b.hi);
    return length(c - frame.eye.xyz);
}

// The projected height of one row, in pixels, at the box's nearest point.
fn row_px(b: Box, row_world: f32) -> f32 {
    let d = box_distance(b);
    return row_world * frame.eye.w / max(d, 1e-6);
}

// The group's T·R·S (glyph_field_derived.wgsl's vertex stage) on one point.
fn group_transform(gbase: u32, p: vec3<f32>) -> vec3<f32> {
    let gpos = groups[gbase];
    let gquat = groups[gbase + 1u];
    let gscale = groups[gbase + 3u];
    let local = p * gscale.xyz;
    let qc = cross(gquat.xyz, local) + local * gquat.w;
    let posed = local + 2.0 * cross(gquat.xyz, qc);
    return posed + gpos.xyz;
}

fn group_base(group: u32) -> u32 {
    let rows = arrayLength(&groups) / GROUP_STRIDE;
    return min(group, rows - 1u) * GROUP_STRIDE;
}

// The world AABB of a local box under the group's transform (8 corners).
fn to_world(gbase: u32, b: Box) -> Box {
    var out: Box;
    out.lo = vec3<f32>(1e30);
    out.hi = vec3<f32>(-1e30);
    for (var c = 0u; c < 8u; c = c + 1u) {
        let p = vec3<f32>(
            select(b.lo.x, b.hi.x, (c & 1u) != 0u),
            select(b.lo.y, b.hi.y, (c & 2u) != 0u),
            select(b.lo.z, b.hi.z, (c & 4u) != 0u),
        );
        let w = group_transform(gbase, p);
        out.lo = min(out.lo, w);
        out.hi = max(out.hi, w);
    }
    return out;
}

// ---- cull A ----------------------------------------------------------------

@compute @workgroup_size(64)
fn cull_items(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = linear_id(gid);
    if (i >= frame.u0.z) { return; }
    if (hidden[i] != 0u) {
        atomicAdd(&counters[C_ITEMS_HIDDEN], 1u);
        return;
    }
    let it = items[i];
    var b: Box;
    b.lo = vec3<f32>(it.bbox_min_x, it.bbox_min_y, it.bbox_min_z);
    b.hi = vec3<f32>(it.bbox_max_x, it.bbox_max_y, it.bbox_max_z);
    if (b.lo.x > b.hi.x || !box_in_frustum(b)) {
        atomicAdd(&counters[C_ITEMS_CULLED], 1u);
        return;
    }
    let gscale = groups[group_base(it.group) + 3u];
    if (row_px(b, it.line_height * gscale.y) < frame.lod.y) {
        atomicAdd(&counters[C_ITEMS_BACKDROP], 1u);
        return;
    }
    let k = atomicAdd(&counters[C_ITEMS_VISIBLE], 1u);
    visible[k] = i;
}

// ---- prefix -----------------------------------------------------------------

var<workgroup> scan: array<u32, 256>;
var<workgroup> wg_n: u32;
var<workgroup> wg_carry: u32;

@compute @workgroup_size(256)
fn prefix_items(@builtin(local_invocation_id) lid: vec3<u32>) {
    let t = lid.x;
    if (t == 0u) {
        wg_n = atomicLoad(&counters[C_ITEMS_VISIBLE]);
        wg_carry = 0u;
    }
    let n = workgroupUniformLoad(&wg_n);
    for (var base = 0u; base < n; base = base + 256u) {
        let k = base + t;
        var v = 0u;
        if (k < n) { v = items[visible[k]].line_count; }
        scan[t] = v;
        workgroupBarrier();
        // Hillis-Steele inclusive scan.
        for (var off = 1u; off < 256u; off = off << 1u) {
            var add = 0u;
            if (t >= off) { add = scan[t - off]; }
            workgroupBarrier();
            scan[t] = scan[t] + add;
            workgroupBarrier();
        }
        let carry = workgroupUniformLoad(&wg_carry);
        if (k < n) { line_base[k] = carry + scan[t] - v; }
        workgroupBarrier();
        if (t == 0u) { wg_carry = carry + scan[255]; }
        workgroupBarrier();
    }
    let total = workgroupUniformLoad(&wg_carry);
    if (t == 0u) {
        atomicStore(&counters[C_LINES_CANDIDATE], total);
        write_dispatch(I_CULL_B, total);
    }
}

// ---- cull B -----------------------------------------------------------------

// `derive_yz`'s y for one row of a paged or plain item, page frame included
// (glyph_field_derived.wgsl); the z of a cell at depth segment `seg`, column
// page `x_page`, band `band`.
fn row_y(p: ItemParamsGpu, row_in_page: f32, band: f32) -> f32 {
    let y_tail = fma(-row_in_page, p.line_height_lo, p.origin_y);
    let y_folded = fma(-row_in_page, p.line_height, y_tail);
    return fma(-band, p.band_stride_y, y_folded);
}

fn cell_z(p: ItemParamsGpu, seg: f32, x_page: f32, band: f32) -> f32 {
    let z_tail = fma(-seg, p.z_step_lo, p.origin_z);
    let z_stepped = fma(-seg, p.z_step, z_tail);
    let z_banded = fma(band, p.depth_per_band, z_stepped);
    return fma(x_page, p.depth_per_col, z_banded);
}

// The LOCAL (pre-group) box of a line: x from its byte length (bytes >=
// cells, every advance <= 2 cells, a fold unit resets x), rows from the
// table, depth from the fold's segment fan and column pages, pages from
// `Pager`'s integer rules — one run of rows per page the line crosses.
fn line_local_box(it: ItemGpu, p: ItemParamsGpu, base_row: u32, rows: u32, len: u32) -> Box {
    let cell_adv = frame.lod.w;
    let wrap = it.wrap_width;
    let paged_cols = it.has_page != 0u && it.page_cols > 0;
    let fold_unit = select(select(0u, u32(it.page_cols), paged_cols), wrap, wrap > 0u);
    var width_cells = len;
    if (fold_unit > 0u) { width_cells = min(len, 2u * fold_unit); }
    let w = f32(width_cells) * cell_adv;
    var nseg = 1u;
    if (wrap > 0u) { nseg = max(1u, (len + wrap - 1u) / wrap); }
    var xp_max = 0u;
    if (paged_cols) { xp_max = len / u32(it.page_cols); }
    let s_hi = f32(nseg - 1u);
    let x_hi = f32(xp_max);
    // z over the segment and column-page extremes (band added per run).
    var z_lo = min(min(cell_z(p, 0.0, 0.0, 0.0), cell_z(p, s_hi, 0.0, 0.0)), min(cell_z(p, 0.0, x_hi, 0.0), cell_z(p, s_hi, x_hi, 0.0)));
    var z_hi = max(max(cell_z(p, 0.0, 0.0, 0.0), cell_z(p, s_hi, 0.0, 0.0)), max(cell_z(p, 0.0, x_hi, 0.0), cell_z(p, s_hi, x_hi, 0.0)));

    let page_rows = select(0, p.page_rows, p.has_page != 0u);
    let scroll = select(0, p.scroll_rows, p.has_page != 0u);
    let pages_wide = max(p.pages_wide, 1);
    var b: Box;
    b.lo = vec3<f32>(1e30);
    b.hi = vec3<f32>(-1e30);
    var r = i32(base_row);
    let r_end = i32(base_row + rows) - 1;
    loop {
        let screen_row = r - scroll;
        var y_page = 0;
        if (page_rows > 0 && screen_row >= page_rows) { y_page = screen_row / page_rows; }
        var run_end = r_end;
        if (page_rows > 0) { run_end = min(r_end, scroll + (y_page + 1) * page_rows - 1); }
        var band = 0.0;
        var x_off = 0.0;
        var row0 = f32(screen_row);
        var row1 = f32(run_end - scroll);
        if (p.has_page != 0u) {
            band = f32(y_page / pages_wide);
            x_off = f32(y_page % pages_wide) * it.stride_hi;
            row0 = f32(screen_row - y_page * page_rows);
            row1 = f32(run_end - scroll - y_page * page_rows);
        } else {
            row0 = f32(r);
            row1 = f32(run_end);
        }
        let y0 = row_y(p, row0, band);
        let y1 = row_y(p, row1, band);
        let x0 = it.origin_x_hi + x_off;
        let zb_lo = fma(band, p.depth_per_band, z_lo);
        let zb_hi = fma(band, p.depth_per_band, z_hi);
        b.lo = min(b.lo, vec3<f32>(x0, min(y0, y1) - 0.5, zb_lo));
        b.hi = max(b.hi, vec3<f32>(x0 + w, max(y0, y1) + 0.5, zb_hi));
        if (run_end >= r_end) { break; }
        r = run_end + 1;
    }
    return b;
}

fn pack_row(row: u32, x_page: u32) -> u32 {
    return min(row, ROW_MAX) | (min(x_page, X_PAGE_MAX) << 24u);
}

fn channel(c: u32, shift: u32) -> f32 {
    return f32((c >> shift) & 0xFFu);
}

// The line's wash colour: its spans' colours weighted by the bytes they
// cover in [s, e), the default colour over the rest.
fn wash_color(it: ItemGpu, line: u32, s: u32, e: u32) -> u32 {
    let def = frame.u1.w;
    var acc = vec3<f32>(0.0);
    var covered = 0u;
    if (it.span_count > 0u) {
        var si = first_span[line];
        let span_end = it.span_base + it.span_count;
        while (si < span_end) {
            let sp = spans[si];
            if (sp.start >= e) { break; }
            let a = max(sp.start, s);
            let b = min(sp.end, e);
            if (b > a) {
                let n = f32(b - a);
                acc = acc + vec3<f32>(channel(sp.color, 0u), channel(sp.color, 8u), channel(sp.color, 16u)) * n;
                covered = covered + (b - a);
            }
            si = si + 1u;
        }
    }
    let total = e - s;
    if (total == 0u) { return def; }
    let rest = f32(total - covered);
    acc = acc + vec3<f32>(channel(def, 0u), channel(def, 8u), channel(def, 16u)) * rest;
    let m = vec3<u32>(clamp(acc / f32(total) + 0.5, vec3<f32>(0.0), vec3<f32>(255.0)));
    return m.x | (m.y << 8u) | (m.z << 16u) | 0xFF000000u;
}

fn push_wash(it_idx: u32, row_lane: u32, x0: f32, width: f32, color: u32, rows: u32, alpha: f32, tint: u32) {
    let k = atomicAdd(&counters[C_WASH], 1u);
    if (k >= frame.u1.z) { return; }
    wash[k] = Wash(it_idx, row_lane, x0, width, color, rows, alpha, tint);
}

@compute @workgroup_size(64)
fn cull_lines(@builtin(global_invocation_id) gid: vec3<u32>) {
    let v = linear_id(gid);
    let total = atomicLoad(&counters[C_LINES_CANDIDATE]);
    if (v >= total) { return; }
    // The visible item whose line range holds v: the last base <= v.
    let n = atomicLoad(&counters[C_ITEMS_VISIBLE]);
    var lo = 0u;
    var hi = n;
    while (lo + 1u < hi) {
        let mid = (lo + hi) >> 1u;
        if (line_base[mid] <= v) { lo = mid; } else { hi = mid; }
    }
    let it_idx = visible[lo];
    let it = items[it_idx];
    let p = item_params[it_idx];
    let li = it.first_line + (v - line_base[lo]);
    let line = lines[li];
    let last_line = it.first_line + it.line_count - 1u;
    var len = it.byte_len - line.byte_start;
    var rows = 1u;
    if (li < last_line) {
        let next = lines[li + 1u];
        len = next.byte_start - 1u - line.byte_start;
        rows = max(next.base_row - line.base_row, 1u);
    } else if (it.wrap_mode == 0u && it.wrap_width > 0u) {
        rows = max(1u, (len + it.wrap_width - 1u) / it.wrap_width);
    }

    let gbase = group_base(it.group);
    let local = line_local_box(it, p, line.base_row, rows, len);
    let world = to_world(gbase, local);
    if (!box_in_frustum(world)) { return; }
    let gscale = groups[gbase + 3u];
    let px = row_px(world, p.line_height * gscale.y);
    let greek = frame.u0.x;
    let tint_mode = frame.u0.y;

    // The wash quad's first-row lane (column page 0: the line's start) and
    // x origin (the first row's page).
    let row_lane = pack_row(line.base_row, 0u);
    var wash_x0 = it.origin_x_hi;
    if (it.has_page != 0u && p.page_rows > 0) {
        let screen_row = i32(line.base_row) - p.scroll_rows;
        if (screen_row >= p.page_rows) {
            let y_page = screen_row / p.page_rows;
            wash_x0 = wash_x0 + f32(y_page % max(p.pages_wide, 1)) * it.stride_hi;
        }
    }
    let wash_w = local.hi.x - local.lo.x;

    if (px < frame.lod.x) {
        // WASH tier.
        atomicAdd(&counters[C_LINES_WASH], 1u);
        if (greek == 0u) { return; }
        var tint = 0u;
        if (tint_mode == 1u) { tint = TINT_WASH_TIER; }
        if (tint_mode == 2u) { tint = TINT_WASH_FULL; }
        push_wash(it_idx, row_lane, wash_x0, wash_w, wash_color(it, li, line.byte_start, line.byte_start + len), rows, 1.0, tint);
        return;
    }

    // GLYPH tier: one segment per cut. The line's seeds: [s0, s1) by binary
    // search over the sorted seed table.
    let seeds_total = frame.u0.w;
    var s_lo = 0u;
    var s_hi = seeds_total;
    while (s_lo < s_hi) {
        let mid = (s_lo + s_hi) >> 1u;
        if (seeds[mid].line < li) { s_lo = mid + 1u; } else { s_hi = mid; }
    }
    let s0 = s_lo;
    var s1 = s0;
    while (s1 < seeds_total && seeds[s1].line == li) { s1 = s1 + 1u; }
    let nseg = (s1 - s0) + 1u;

    let seg_base = atomicAdd(&counters[C_SEGMENTS], nseg);
    if (seg_base + nseg > frame.u1.y) {
        atomicAdd(&counters[C_LINES_DROPPED], 1u);
        atomicAdd(&counters[C_SLOTS_DROPPED], line.glyph_count);
        if (tint_mode == 2u) { push_wash(it_idx, row_lane, wash_x0, wash_w, TINT_DROPPED, rows, 1.0, TINT_DROPPED); }
        return;
    }
    atomicMax(&counters[C_SEG_FIT_END], seg_base + nseg);
    let slot_base = atomicAdd(&counters[C_SLOTS], line.glyph_count);
    let fits = slot_base + line.glyph_count <= frame.u1.x;
    if (!fits) {
        atomicAdd(&counters[C_LINES_DROPPED], 1u);
        atomicAdd(&counters[C_SLOTS_DROPPED], line.glyph_count);
        if (tint_mode == 2u) { push_wash(it_idx, row_lane, wash_x0, wash_w, TINT_DROPPED, rows, 1.0, TINT_DROPPED); }
        // Empty entries keep the segment range contiguous.
        for (var k = 0u; k < nseg; k = k + 1u) {
            segs[seg_base + k] = Seg(it_idx, li, 0u, 0u, 0u, 0u, 0.0, 0u);
        }
        return;
    }
    atomicMax(&counters[C_SLOT_FIT_END], slot_base + line.glyph_count);
    atomicAdd(&counters[C_LINES_GLYPH], 1u);
    for (var k = 0u; k < nseg; k = k + 1u) {
        var seg: Seg;
        seg.item = it_idx;
        seg.line = li;
        seg.byte_off = 0u;
        seg.col = 0u;
        seg.cells = 0u;
        seg.seg_adv = 0.0;
        seg.slot_base = slot_base;
        if (k > 0u) {
            let sd = seeds[s0 + k - 1u];
            seg.byte_off = sd.byte_offset;
            seg.col = sd.col;
            seg.cells = sd.cells;
            seg.seg_adv = sd.seg_adv;
            seg.slot_base = slot_base + survivors[seeds_total + s0 + k - 1u];
        }
        seg.byte_end = len;
        if (k + 1u < nseg) { seg.byte_end = seeds[s0 + k].byte_offset; }
        segs[seg_base + k] = seg;
    }
    // Over the 1 px band above the glyph threshold the wash fades in under
    // the glyphs (greek_mode 1 only).
    if (greek == 1u && px < frame.lod.x + 1.0) {
        var tint = 0u;
        if (tint_mode == 1u) { tint = TINT_WASH_TIER; }
        if (tint_mode == 2u) { tint = TINT_WASH_BAND; }
        push_wash(it_idx, row_lane, wash_x0, wash_w, wash_color(it, li, line.byte_start, line.byte_start + len), rows, frame.lod.x + 1.0 - px, tint);
    }
}

// ---- finalize ---------------------------------------------------------------

@compute @workgroup_size(1)
fn finalize() {
    let seg_count = min(atomicLoad(&counters[C_SEG_FIT_END]), frame.u1.y);
    write_dispatch(I_LAYOUT, seg_count);
    // DrawIndexedIndirectArgs: index_count, instance_count, first_index,
    // base_vertex, first_instance (always 0: Metal drops a nonzero one).
    indirect[I_DRAW] = 6u;
    indirect[I_DRAW + 1u] = min(atomicLoad(&counters[C_SLOT_FIT_END]), frame.u1.x);
    indirect[I_DRAW + 2u] = 0u;
    indirect[I_DRAW + 3u] = 0u;
    indirect[I_DRAW + 4u] = 0u;
    indirect[I_WASH_DRAW] = 6u;
    indirect[I_WASH_DRAW + 1u] = min(atomicLoad(&counters[C_WASH]), frame.u1.z);
    indirect[I_WASH_DRAW + 2u] = 0u;
    indirect[I_WASH_DRAW + 3u] = 0u;
    indirect[I_WASH_DRAW + 4u] = 0u;
}
