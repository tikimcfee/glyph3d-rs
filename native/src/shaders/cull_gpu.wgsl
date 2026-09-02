// Stage K — GPU segment cull: compute passes that drive the indirect draws.
//
// THE GPU-DRIVEN CULL CONTRACT (the CPU twin is `cull_segments` in
// glyph_scene.rs; both must make the SAME decisions — `--cull-verify` diffs
// them per frame, see tools/check-mdi.sh)
// -----------------------------------------------------------------------
// Inputs are the Stage F segment table (SegCull, 48 B, one per file) and a
// per-frame CullParams uniform (frustum planes, eye, px_scale). Three
// dispatches per frame, in one compute pass:
//
//   1. cs_classify   one thread per segment → seg_mode[s]:
//        0 = culled (hidden, or AABB outside the frustum)
//        1 = glyph stream (visible, ≥ lod_min_px per em at its nearest point)
//        2 = backdrop (visible but every glyph is subpixel)
//      Same tests, same operand order as the CPU: 6-plane positive-vertex
//      AABB test with z ∈ [-1, 1], then glyph_px = px_scale / dist(eye,
//      nearest AABB point), max(dist, 0.001).
//
//   2. cs_draw_args  one thread per DRAW SLOT → DrawIndirectArgs. The slot
//      table is STATIC (built once on the CPU): for every arena chunk, in
//      segment order, one slot per (segment, chunk) intersection with the
//      chunk-local instance range. A visible glyph segment's slot becomes
//      draw(0..6, base..base+count) — `first_instance = base` — and every
//      other slot becomes a zero-instance draw. render() issues ONE
//      multi_draw_indirect per chunk over its slot range, so the draw
//      ORDER is the slot order = the CPU path's order = ascending arena
//      order per chunk: within-pixel blending is unchanged.
//
//      WHY FIXED SLOTS + ZERO PADDING: Metal has no draw-count buffer
//      (wgpu-hal's draw_indirect_count is a no-op there; the adapter reports
//      MULTI_DRAW_INDIRECT_COUNT=false) and wgpu emulates multi-draw as a
//      loop of per-slot indirect draws. A zero-instance slot costs one
//      encoder call and no GPU work — so the encode cost is O(segments),
//      the GPU cost is O(visible), and NOTHING is read back to the CPU.
//
//   3. cs_backdrop_compact  ONE workgroup, stable stream compaction of the
//      mode==2 segments into the backdrop instance list (BackdropInst, 32 B)
//      plus a DrawIndirectArgs {6, count, 0, 0} for the backdrop
//      draw_indirect. A workgroup-wide inclusive scan per 256-segment block
//      keeps SEGMENT ORDER (an atomic append would not, and backdrop quads
//      of moved files can overlap, so order is part of the output). This is
//      the one O(segments/256) serial loop in the pipeline; a multi-workgroup
//      decoupled-lookback scan is the upgrade once tables reach ~1e5 files.
//
// first_instance != 0 in an indirect draw REQUIRES the device to be created
// with Features::INDIRECT_FIRST_INSTANCE — otherwise wgpu's indirect
// validation (spec: "indirect-first-instance") turns the draw into a no-op.
// That, not Metal, was the Stage F blocker; see src/bin/mdi_probe.rs.

const SEG_HIDDEN: u32 = 1u;
const MODE_CULLED: u32 = 0u;
const MODE_GLYPHS: u32 = 1u;
const MODE_BACKDROP: u32 = 2u;
const QUAD_VERTICES: u32 = 6u;

// 48 B — mirrors `SegCull` in glyph_scene.rs (encase-asserted).
struct SegCull {
    min: vec2<f32>,
    max: vec2<f32>,
    slot_base: u32,
    slot_count: u32,
    flags: u32,      // bit0 SEG_HIDDEN (Stage G hide-group)
    _pad: u32,
    tint: vec4<f32>, // rgb linear mean ink, a = coverage E
};

// 128 B uniform — mirrors `CullParams` in glyph_scene.rs.
struct CullParams {
    planes: array<vec4<f32>, 6>,
    eye: vec3<f32>,
    px_scale: f32,
    lod_min_px: f32,
    seg_count: u32,
    slot_count: u32,
    _pad: u32,
};

// 16 B — mirrors `DrawSlot` in glyph_scene.rs.
struct DrawSlot {
    seg: u32,
    base: u32,  // chunk-local first instance
    count: u32,
    _pad: u32,
};

// 16 B — wgpu::util::DrawIndirectArgs.
struct DrawArgs {
    vertex_count: u32,
    instance_count: u32,
    first_vertex: u32,
    first_instance: u32,
};

// 32 B — mirrors BackdropInst (cull.wgsl / glyph_scene.rs).
struct BackdropInst {
    min_xy: vec2<f32>,
    max_xy: vec2<f32>,
    rgba: vec4<f32>,
};

@group(0) @binding(0) var<uniform> params: CullParams;
@group(0) @binding(1) var<storage, read> segs: array<SegCull>;
@group(0) @binding(2) var<storage, read_write> seg_mode: array<u32>;
@group(0) @binding(3) var<storage, read> draw_slots: array<DrawSlot>;
@group(0) @binding(4) var<storage, read_write> draw_args: array<DrawArgs>;
@group(0) @binding(5) var<storage, read_write> binsts: array<BackdropInst>;
@group(0) @binding(6) var<storage, read_write> backdrop_args: DrawArgs;

@compute @workgroup_size(64)
fn cs_classify(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if i >= params.seg_count {
        return;
    }
    let seg = segs[i];
    var mode = MODE_CULLED;
    if (seg.flags & SEG_HIDDEN) == 0u {
        // Frustum: positive-vertex test per plane; z margin ±1 (z=0 plane).
        var visible = true;
        for (var p = 0u; p < 6u; p = p + 1u) {
            let pl = params.planes[p];
            let px = select(seg.min.x, seg.max.x, pl.x >= 0.0);
            let py = select(seg.min.y, seg.max.y, pl.y >= 0.0);
            let pz = select(-1.0, 1.0, pl.z >= 0.0);
            if pl.x * px + pl.y * py + pl.z * pz + pl.w < 0.0 {
                visible = false;
            }
        }
        if visible {
            // LOD: em-cell pixel height at the AABB's nearest point to the
            // eye (conservative: only when even the CLOSEST glyphs are
            // subpixel does the segment drop to its backdrop).
            let nx = clamp(params.eye.x, seg.min.x, seg.max.x);
            let ny = clamp(params.eye.y, seg.min.y, seg.max.y);
            let nz = clamp(params.eye.z, -1.0, 1.0);
            let dx = params.eye.x - nx;
            let dy = params.eye.y - ny;
            let dz = params.eye.z - nz;
            let dist = max(sqrt(dx * dx + dy * dy + dz * dz), 0.001);
            let glyph_px = params.px_scale / dist;
            if glyph_px < params.lod_min_px {
                mode = select(MODE_CULLED, MODE_BACKDROP, seg.slot_count > 0u);
            } else {
                mode = MODE_GLYPHS;
            }
        }
    }
    seg_mode[i] = mode;
}

@compute @workgroup_size(64)
fn cs_draw_args(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if i >= params.slot_count {
        return;
    }
    let slot = draw_slots[i];
    let count = select(0u, slot.count, seg_mode[slot.seg] == MODE_GLYPHS);
    draw_args[i] = DrawArgs(QUAD_VERTICES, count, 0u, slot.base);
}

const SCAN_WG: u32 = 256u;
var<workgroup> scan: array<u32, 256>;

@compute @workgroup_size(256)
fn cs_backdrop_compact(@builtin(local_invocation_id) lid: vec3<u32>) {
    let t = lid.x;
    let n = params.seg_count;
    // Backdrops emitted by the blocks before this one (workgroup-uniform).
    var carry = 0u;
    for (var base = 0u; base < n; base = base + SCAN_WG) {
        let i = base + t;
        var flag = 0u;
        if i < n && seg_mode[i] == MODE_BACKDROP {
            flag = 1u;
        }
        // Inclusive Hillis–Steele scan of `flag` across the workgroup.
        scan[t] = flag;
        workgroupBarrier();
        for (var off = 1u; off < SCAN_WG; off = off << 1u) {
            var v = 0u;
            if t >= off {
                v = scan[t - off];
            }
            workgroupBarrier();
            scan[t] = scan[t] + v;
            workgroupBarrier();
        }
        if flag == 1u {
            let seg = segs[i];
            binsts[carry + scan[t] - 1u] = BackdropInst(seg.min, seg.max, seg.tint);
        }
        carry = carry + scan[SCAN_WG - 1u];
        // Everyone must be done reading scan[] before the next block reuses it.
        workgroupBarrier();
    }
    if t == 0u {
        backdrop_args = DrawArgs(QUAD_VERTICES, carry, 0u, 0u);
    }
}
