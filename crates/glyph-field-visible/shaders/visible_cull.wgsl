// visible_cull.wgsl — the Visible field's per-frame cull, four dispatches in
// one compute pass, nothing read back:
//
//   cull_items     one invocation per item, 256 to a workgroup: its world
//                  box (the scene's own `SegCull` box, group offset applied)
//                  against the frustum, the `hidden` bit, and the projected
//                  row height at the box's nearest point — under
//                  `lod_backdrop_px` the item is a BACKDROP (the scene draws
//                  its quad; nothing is laid out), else its `item_flag` is
//                  set; its workgroup's sums (visible items, their lines) go
//                  to `block_sums`.
//   prefix_items   one workgroup: an exclusive scan of those block sums; the
//                  totals plan cull B's indirect dispatch.
//   compact_items  the same dispatch as cull_items: the visible items in
//                  index order into `visible`, each with its candidate-line
//                  base.
//   cull_lines     one invocation per candidate line, dispatched indirectly,
//                  256 to a workgroup: finds its item by binary search over
//                  the bases, builds the line's world box (x over its widest
//                  fold unit, rows from the table, depth segments and column
//                  pages from its leader count by the fold rules, pages from
//                  `Pager`), tests it, and sorts a visible line into the
//                  GLYPH tier (one segment entry per cut, slots for the whole
//                  line) or the WASH tier (one BOX in its spans' mean colour:
//                  that same x extent, its rows, its depth segments — C28,
//                  2026-10-10). It records the decision (`line_rec`) and its
//                  workgroup's sums of the three counts a line reserves —
//                  segments, slots, wash boxes — in `block_sums`.
//   scan_blocks    one workgroup: an exclusive scan of the block sums.
//   emit_lines     the same dispatch as cull_lines: each workgroup scans its
//                  lines' counts again, adds its block's base, and writes the
//                  segment entries and wash boxes at those offsets.
//   finalize       one invocation: the layout dispatch and both draws'
//                  indirect arguments from the counters.
//
// ORDER (C30, 2026-10-10). Every offset is a scan in (item index, line)
// order, so the transient slots, segments and wash boxes come out in arena
// order — the stored modes' draw order — and the same view lays out the same
// bytes every frame. Until C30 they were reserved by `atomicAdd`, in
// whatever order the GPU ran the invocations: the order changed frame to
// frame, and where a line's back-stacked segments overlap another line's
// glyphs the glyph pass (blended, writing depth) resolves the overlap by draw
// order, so those glyphs flickered on a still camera (Ivan, a large file in a
// big JS repo). The witness is `native/tests/visible_repeat.rs`.
//
// CAPS. Offsets are monotone in that order, so every line that fits precedes
// every line that does not, and the lines dropped at a cap are the same
// lines every frame: the LAST in (item, line) order. `seg_fit_end` /
// `slot_fit_end` (atomicMax of the fitting lines' ends — a max, the same in
// any order) are contiguous, fully written ranges, and the draw and the
// layout dispatch read those, never the totals. A line dropped at the slot
// cap still writes its segment entries, EMPTY, so the segment range has no
// hole; a reserved wash box nothing fills is written EMPTY (alpha 0, which
// the wash vertex stage moves off screen).
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

// `cols`: the line's leaders (the fold's column at its end); `width_cells`:
// its widest fold unit in cells (the whole line without one).
struct LineEntry { byte_start: u32, item: u32, base_row: u32, glyph_count: u32, cols: u32, width_cells: u32 };
struct SegmentSeed { line: u32, byte_offset: u32, col: u32, seg_adv: f32, cells: u32, _pad: u32 };
struct ByteSpan { start: u32, end: u32, color: u32 };
struct Seg { item: u32, line: u32, byte_off: u32, byte_end: u32, col: u32, cells: u32, seg_adv: f32, slot_base: u32 };
// One WASH box: the line's first row (row lane as the Derived slot carries
// it), its x extent, its mean colour, how many rows it stacks (WrapDown),
// its alpha, a debug tint (0 = none), and how many depth segments it spans
// (WrapBack: segment 0's z to the last's).
struct Wash { item: u32, row_lane: u32, x0: f32, width: f32, color: u32, rows: u32, alpha: f32, tint: u32, nseg: u32 };

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
// C30: the scans' scratch. `item_flag[i]` 1 when item i is visible this
// frame; `line_rec[v]` the decision for candidate line v — x its segment
// count (glyph tier) in bits 0..27 and the tier / wash / fade flags in
// 28..31, y the fade alpha's bits; `block_sums[b]` the segment, slot and
// wash counts of cull B's workgroup b, then (after scan_blocks) its bases.
@group(0) @binding(16) var<storage, read_write> item_flag: array<u32>;
@group(0) @binding(17) var<storage, read_write> line_rec: array<vec2<u32>>;
@group(0) @binding(18) var<storage, read_write> block_sums: array<vec4<u32>>;

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
// Cull B's workgroup: one scan block of lines.
const LWG: u32 = 256u;
const MAX_GROUPS_X: u32 = 65535u;
const ROW_MAX: u32 = 0xFFFFFFu;
const X_PAGE_MAX: u32 = 0xFFu;

const TINT_WASH_TIER: u32 = 0xFFFF8040u;     // wash tier: blue
const TINT_WASH_FULL: u32 = 0xFFFF40FFu;     // cull state: a full wash (under the glyph threshold): magenta
const TINT_WASH_BAND: u32 = 0xFFFFFF40u;     // cull state: a wash fading in over the 1 px band: cyan
const TINT_DROPPED: u32 = 0xFF4040FFu;       // cull state: a glyph-tier line dropped at a cap: red

// `plan_dispatch` (lib.rs): groups of `wg`, x capped at 65,535, y the rest.
fn write_dispatch_wg(at: u32, count: u32, wg: u32) {
    let groups = (count + wg - 1u) / wg;
    indirect[at] = min(groups, MAX_GROUPS_X);
    indirect[at + 1u] = max((groups + MAX_GROUPS_X - 1u) / MAX_GROUPS_X, 1u);
    indirect[at + 2u] = 1u;
}

fn write_dispatch(at: u32, count: u32) {
    write_dispatch_wg(at, count, WG);
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

// One item's verdict: 1 when it is laid out this frame (visible, near enough
// for lines), else 0 with the reason counted.
fn item_visible(i: u32) -> u32 {
    if (hidden[i] != 0u) {
        atomicAdd(&counters[C_ITEMS_HIDDEN], 1u);
        return 0u;
    }
    let it = items[i];
    var b: Box;
    b.lo = vec3<f32>(it.bbox_min_x, it.bbox_min_y, it.bbox_min_z);
    b.hi = vec3<f32>(it.bbox_max_x, it.bbox_max_y, it.bbox_max_z);
    if (b.lo.x > b.hi.x || !box_in_frustum(b)) {
        atomicAdd(&counters[C_ITEMS_CULLED], 1u);
        return 0u;
    }
    let gscale = groups[group_base(it.group) + 3u];
    if (row_px(b, it.line_height * gscale.y) < frame.lod.y) {
        atomicAdd(&counters[C_ITEMS_BACKDROP], 1u);
        return 0u;
    }
    return 1u;
}

// What a visible item contributes to the scan: one place, its lines.
fn item_counts(i: u32) -> vec3<u32> {
    if (i >= frame.u0.z || item_flag[i] == 0u) { return vec3<u32>(0u); }
    return vec3<u32>(1u, items[i].line_count, 0u);
}

// The verdict per item, and its workgroup's sums (block_sums, which cull B
// reuses for the lines once the items are compacted).
@compute @workgroup_size(256)
fn cull_items(@builtin(local_invocation_id) lid: vec3<u32>, @builtin(workgroup_id) wid: vec3<u32>) {
    let block = block_of(wid);
    let i = block * LWG + lid.x;
    if (i < frame.u0.z) { item_flag[i] = item_visible(i); }
    let incl = wg_scan3(lid.x, item_counts(i));
    if (lid.x == LWG - 1u) { block_sums[block] = vec4<u32>(incl, 0u); }
}

// ---- prefix -----------------------------------------------------------------

// One workgroup: the item blocks' sums -> their exclusive bases; the totals
// are the visible items and the candidate lines, which plan cull B.
@compute @workgroup_size(256)
fn prefix_items(@builtin(local_invocation_id) lid: vec3<u32>) {
    let sums = scan_block_sums(lid.x, (frame.u0.z + LWG - 1u) / LWG);
    if (lid.x == 0u) {
        atomicStore(&counters[C_ITEMS_VISIBLE], sums.x);
        atomicStore(&counters[C_LINES_CANDIDATE], sums.y);
        write_dispatch_wg(I_CULL_B, sums.y, LWG);
    }
}

// The visible items in index order: each one's place in `visible` and its
// candidate-line base, from its block's base and the block's own scan.
@compute @workgroup_size(256)
fn compact_items(@builtin(local_invocation_id) lid: vec3<u32>, @builtin(workgroup_id) wid: vec3<u32>) {
    let block = block_of(wid);
    let i = block * LWG + lid.x;
    let v = item_counts(i);
    let incl = wg_scan3(lid.x, v);
    if (v.x != 0u) {
        let at = block_sums[block].xyz + incl - v;
        visible[at.x] = i;
        line_base[at.x] = at.y;
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

// The depth segments a line's leaders fold into: `wrap_segment = col / wrap`
// for its last leader, plus one (one segment without a wrap).
fn depth_segments(it: ItemGpu, cols: u32) -> u32 {
    if (it.wrap_width == 0u) { return 1u; }
    return max(1u, (cols + it.wrap_width - 1u) / it.wrap_width);
}

// The LOCAL (pre-group) box of a line: x over its widest fold unit (Pass
// 1's `width_cells`: exact, the glyphs reach no further), rows from the
// table, depth from the fold's segment fan (`nseg`) and column pages (from
// the leader count), pages from `Pager`'s integer rules — one run of rows
// per page the line crosses.
fn line_local_box(it: ItemGpu, p: ItemParamsGpu, base_row: u32, rows: u32, cols: u32, width_cells: u32, nseg: u32) -> Box {
    let cell_adv = frame.lod.w;
    let paged_cols = it.has_page != 0u && it.page_cols > 0;
    let w = f32(width_cells) * cell_adv;
    var xp_max = 0u;
    if (paged_cols) { xp_max = cols / u32(it.page_cols); }
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

// The line record's flags (bits 28..31 of `line_rec.x`).
const TIER_WASH: u32 = 1u;
const TIER_GLYPH: u32 = 2u;
const REC_WASH: u32 = 4u;   // the line reserves one wash box
const REC_FADE: u32 = 8u;   // its wash is the fade band's (glyph tier)
const REC_NSEG_MASK: u32 = 0x0FFFFFFFu;

// A candidate line: its item, its line-table index, its byte length, rows.
struct LineCtx { it_idx: u32, li: u32, len: u32, rows: u32 };

fn line_ctx(v: u32) -> LineCtx {
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
    return LineCtx(it_idx, li, len, rows);
}

// The line's seeds: [x, y) by binary search over the sorted seed table.
fn seed_range(li: u32) -> vec2<u32> {
    let seeds_total = frame.u0.w;
    var s_lo = 0u;
    var s_hi = seeds_total;
    while (s_lo < s_hi) {
        let mid = (s_lo + s_hi) >> 1u;
        if (seeds[mid].line < li) { s_lo = mid + 1u; } else { s_hi = mid; }
    }
    var s1 = s_lo;
    while (s1 < seeds_total && seeds[s1].line == li) { s1 = s1 + 1u; }
    return vec2<u32>(s_lo, s1);
}

// What a line reserves, from its record: segments, slots, wash boxes.
fn rec_counts(rec: vec2<u32>, glyph_count: u32) -> vec3<u32> {
    let flags = rec.x >> 28u;
    var c = vec3<u32>(0u);
    if (flags & 3u) == TIER_GLYPH {
        c.x = rec.x & REC_NSEG_MASK;
        c.y = glyph_count;
    }
    if (flags & REC_WASH) != 0u { c.z = 1u; }
    return c;
}

fn make_rec(nseg: u32, flags: u32, alpha: f32) -> vec2<u32> {
    return vec2<u32>(min(nseg, REC_NSEG_MASK) | (flags << 28u), bitcast<u32>(alpha));
}

// cull_lines' per-line decision (no barrier inside: the entry point scans
// after it in uniform control flow).
fn classify(c: LineCtx) -> vec2<u32> {
    let it = items[c.it_idx];
    let p = item_params[c.it_idx];
    let line = lines[c.li];
    let gbase = group_base(it.group);
    let depth = depth_segments(it, line.cols);
    let local = line_local_box(it, p, line.base_row, c.rows, line.cols, line.width_cells, depth);
    let world = to_world(gbase, local);
    if (!box_in_frustum(world)) { return vec2<u32>(0u); }
    let gscale = groups[gbase + 3u];
    let px = row_px(world, p.line_height * gscale.y);
    let greek = frame.u0.x;
    let tint_mode = frame.u0.y;
    if (px < frame.lod.x) {
        // WASH tier: a box when greeking draws one.
        atomicAdd(&counters[C_LINES_WASH], 1u);
        var flags = TIER_WASH;
        if (greek != 0u) { flags = flags | REC_WASH; }
        return make_rec(0u, flags, 1.0);
    }
    // GLYPH tier: one segment per cut. Over the 1 px band above the glyph
    // threshold the wash fades in under the glyphs (greek_mode 1 only); the
    // cull-state tint reserves a box for a line it may paint as dropped.
    let sr = seed_range(c.li);
    let nseg = (sr.y - sr.x) + 1u;
    var flags = TIER_GLYPH;
    var alpha = 0.0;
    if (greek == 1u && px < frame.lod.x + 1.0) {
        flags = flags | REC_FADE | REC_WASH;
        alpha = frame.lod.x + 1.0 - px;
    }
    if (tint_mode == 2u) { flags = flags | REC_WASH; }
    return make_rec(nseg, flags, alpha);
}

var<workgroup> lscan: array<vec3<u32>, 256>;
var<workgroup> lcarry: vec3<u32>;
var<workgroup> lnb: u32;

// Inclusive Hillis-Steele scan of one value per invocation over LWG.
fn wg_scan3(t: u32, v: vec3<u32>) -> vec3<u32> {
    lscan[t] = v;
    workgroupBarrier();
    for (var off = 1u; off < LWG; off = off << 1u) {
        var add = vec3<u32>(0u);
        if (t >= off) { add = lscan[t - off]; }
        workgroupBarrier();
        lscan[t] = lscan[t] + add;
        workgroupBarrier();
    }
    let r = lscan[t];
    workgroupBarrier();
    return r;
}

fn block_of(wid: vec3<u32>) -> u32 {
    return wid.y * MAX_GROUPS_X + wid.x;
}

@compute @workgroup_size(256)
fn cull_lines(@builtin(local_invocation_id) lid: vec3<u32>, @builtin(workgroup_id) wid: vec3<u32>) {
    let block = block_of(wid);
    let v = block * LWG + lid.x;
    let total = atomicLoad(&counters[C_LINES_CANDIDATE]);
    var counts = vec3<u32>(0u);
    if (v < total) {
        let c = line_ctx(v);
        let rec = classify(c);
        line_rec[v] = rec;
        counts = rec_counts(rec, lines[c.li].glyph_count);
    }
    let incl = wg_scan3(lid.x, counts);
    if (lid.x == LWG - 1u) { block_sums[block] = vec4<u32>(incl, 0u); }
}

// One workgroup's scan of the first `nblocks` block sums, in place: counts
// -> exclusive bases, in block order. Returns the totals (every invocation).
// `nblocks` must be uniform: it comes from the frame uniform or a counter
// read through a workgroup variable.
fn scan_block_sums(t: u32, nblocks: u32) -> vec3<u32> {
    if (t == 0u) {
        lcarry = vec3<u32>(0u);
        lnb = nblocks;
    }
    let nb = workgroupUniformLoad(&lnb);
    for (var base = 0u; base < nb; base = base + LWG) {
        let k = base + t;
        var v = vec3<u32>(0u);
        if (k < nb) { v = block_sums[k].xyz; }
        let incl = wg_scan3(t, v);
        let carry = workgroupUniformLoad(&lcarry);
        if (k < nb) { block_sums[k] = vec4<u32>(carry + incl - v, 0u); }
        if (t == LWG - 1u) { lcarry = carry + incl; }
        workgroupBarrier();
    }
    return workgroupUniformLoad(&lcarry);
}

// One workgroup: the line blocks' sums -> their bases; the totals are the
// frame's reservations.
@compute @workgroup_size(256)
fn scan_blocks(@builtin(local_invocation_id) lid: vec3<u32>) {
    let total = atomicLoad(&counters[C_LINES_CANDIDATE]);
    let sums = scan_block_sums(lid.x, (total + LWG - 1u) / LWG);
    if (lid.x == 0u) {
        atomicStore(&counters[C_SEGMENTS], sums.x);
        atomicStore(&counters[C_SLOTS], sums.y);
        atomicStore(&counters[C_WASH], sums.z);
    }
}

fn put_wash(k: u32, w: Wash) {
    if (k >= frame.u1.z) { return; }
    wash[k] = w;
}

// One line's writes at its scanned bases (x segments, y slots, z wash).
fn emit_line(c: LineCtx, rec: vec2<u32>, base: vec3<u32>) {
    let flags = rec.x >> 28u;
    let tier = flags & 3u;
    if (tier == 0u) { return; }
    let it_idx = c.it_idx;
    let li = c.li;
    let it = items[it_idx];
    let p = item_params[it_idx];
    let line = lines[li];
    let depth = depth_segments(it, line.cols);
    let tint_mode = frame.u0.y;
    let has_wash = (flags & REC_WASH) != 0u;

    // The wash box's first-row lane (column page 0: the line's start), its
    // x origin (the first row's page) and its width: the line's widest fold
    // unit, exactly what its glyphs reach — not the cull box, which is the
    // same today but may be padded; the wash is what the eye sees.
    let row_lane = pack_row(line.base_row, 0u);
    var wash_x0 = it.origin_x_hi;
    if (it.has_page != 0u && p.page_rows > 0) {
        let screen_row = i32(line.base_row) - p.scroll_rows;
        if (screen_row >= p.page_rows) {
            let y_page = screen_row / p.page_rows;
            wash_x0 = wash_x0 + f32(y_page % max(p.pages_wide, 1)) * it.stride_hi;
        }
    }
    let wash_w = f32(line.width_cells) * frame.lod.w;
    // A reserved box nothing fills stays EMPTY (alpha 0: off screen).
    var w = Wash(it_idx, row_lane, wash_x0, wash_w, 0u, c.rows, 0.0, 0u, depth);

    if (tier == TIER_WASH) {
        if (!has_wash) { return; }
        var tint = 0u;
        if (tint_mode == 1u) { tint = TINT_WASH_TIER; }
        if (tint_mode == 2u) { tint = TINT_WASH_FULL; }
        w.color = wash_color(it, li, line.byte_start, line.byte_start + c.len);
        w.alpha = 1.0;
        w.tint = tint;
        put_wash(base.z, w);
        return;
    }

    let nseg = rec.x & REC_NSEG_MASK;
    let seg_base = base.x;
    let slot_base = base.y;
    if (seg_base + nseg > frame.u1.y) {
        atomicAdd(&counters[C_LINES_DROPPED], 1u);
        atomicAdd(&counters[C_SLOTS_DROPPED], line.glyph_count);
        if (tint_mode == 2u) { w.color = TINT_DROPPED; w.alpha = 1.0; w.tint = TINT_DROPPED; }
        if (has_wash) { put_wash(base.z, w); }
        return;
    }
    atomicMax(&counters[C_SEG_FIT_END], seg_base + nseg);
    let fits = slot_base + line.glyph_count <= frame.u1.x;
    if (!fits) {
        atomicAdd(&counters[C_LINES_DROPPED], 1u);
        atomicAdd(&counters[C_SLOTS_DROPPED], line.glyph_count);
        if (tint_mode == 2u) { w.color = TINT_DROPPED; w.alpha = 1.0; w.tint = TINT_DROPPED; }
        if (has_wash) { put_wash(base.z, w); }
        // Empty entries keep the segment range contiguous.
        for (var k = 0u; k < nseg; k = k + 1u) {
            segs[seg_base + k] = Seg(it_idx, li, 0u, 0u, 0u, 0u, 0.0, 0u);
        }
        return;
    }
    atomicMax(&counters[C_SLOT_FIT_END], slot_base + line.glyph_count);
    atomicAdd(&counters[C_LINES_GLYPH], 1u);
    let sr = seed_range(li);
    let s0 = sr.x;
    let seeds_total = frame.u0.w;
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
        seg.byte_end = c.len;
        if (k + 1u < nseg) { seg.byte_end = seeds[s0 + k].byte_offset; }
        segs[seg_base + k] = seg;
    }
    if ((flags & REC_FADE) != 0u) {
        var tint = 0u;
        if (tint_mode == 1u) { tint = TINT_WASH_TIER; }
        if (tint_mode == 2u) { tint = TINT_WASH_BAND; }
        w.color = wash_color(it, li, line.byte_start, line.byte_start + c.len);
        w.alpha = bitcast<f32>(rec.y);
        w.tint = tint;
    }
    if (has_wash) { put_wash(base.z, w); }
}

@compute @workgroup_size(256)
fn emit_lines(@builtin(local_invocation_id) lid: vec3<u32>, @builtin(workgroup_id) wid: vec3<u32>) {
    let block = block_of(wid);
    let v = block * LWG + lid.x;
    let total = atomicLoad(&counters[C_LINES_CANDIDATE]);
    var counts = vec3<u32>(0u);
    var rec = vec2<u32>(0u);
    var c = LineCtx(0u, 0u, 0u, 0u);
    if (v < total) {
        c = line_ctx(v);
        rec = line_rec[v];
        counts = rec_counts(rec, lines[c.li].glyph_count);
    }
    let incl = wg_scan3(lid.x, counts);
    let base = block_sums[block].xyz + incl - counts;
    if (v < total) { emit_line(c, rec, base); }
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
    // A wash is a box: six faces, 36 indices (visible_wash.wgsl's corner table).
    indirect[I_WASH_DRAW] = 36u;
    indirect[I_WASH_DRAW + 1u] = min(atomicLoad(&counters[C_WASH]), frame.u1.z);
    indirect[I_WASH_DRAW + 2u] = 0u;
    indirect[I_WASH_DRAW + 3u] = 0u;
    indirect[I_WASH_DRAW + 4u] = 0u;
}
