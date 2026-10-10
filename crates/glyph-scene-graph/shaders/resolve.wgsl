// resolve.wgsl — derive WORLD transforms from LOCAL ones, over the dirty
// depth-first ranges only (glyph-scene-graph, step 1 of
// out/DESIGN-VIEWS-AND-TRANSFORMS-2026-10-10.md).
//
// One thread per node; each thread walks its own parent chain to a root,
// composing bottom-up (acc = local[parent] ∘ acc). That is Wicked Engine's
// hierarchy update (`Scene::RunHierarchyUpdateSystem`, wiScene.cpp, MIT),
// moved from CPU jobs onto the GPU: no thread reads another's output, so the
// pass needs no ordering, no barrier between levels and nothing special for
// pooled rows. A node's world is a function of its chain's LOCAL rows alone
// (never of a world row a previous frame left), so resolving a clean node
// rewrites the bits it had — the host may coalesce ranges freely, and the
// result never depends on which edits arrived in which frame.
//
// The arithmetic mirrors `src/transform.rs` term for term (the rotation is
// glyph_field.wgsl's cross-product sandwich), so the CPU reference differs
// only by what the GPU fuses. Under an identity parent every step returns its
// operand exactly: products with 1, sums with 0.
//
// Transitional output (binding group 1): a node with a group row writes its
// world transform and appearance into the renderer's 96 B GroupRow table in
// the columns the draw shaders already read — col 0 offset.xyz (w kept), col
// 1 quaternion, col 2 tint.rgb + inherited alpha, col 3 world scale x post
// scale + colourBlend. Columns 4 (clip) and 5 (background) are left alone:
// they are not the tree's to own.

const NONE: u32 = 0xFFFFFFFFu;
const WG: u32 = 64u;
const MAX_RANGES: u32 = 64u;
const GROUP_STRIDE: u32 = 6u;

struct Similarity {
    t_s: vec4<f32>, // translation.xyz, uniform scale in w
    q: vec4<f32>,   // rotation xyzw
}

struct Appearance {
    tint: vec4<f32>, // rgb + alpha (alpha inherits multiplicatively)
    blend: f32,      // colourBlend: 0 multiply, 1 replace
    flags: u32,
    _p0: u32,
    _p1: u32,
}

struct ResolveParams {
    range_count: u32,
    total: u32,       // threads: the ranges' summed lengths
    max_depth: u32,   // chain-walk cap: a corrupted parent cycle cannot hang the GPU
    row_limit: u32,   // rows in the output group table (0: no output)
    // (dfs start, first thread) per range, two ranges per vec4.
    ranges: array<vec4<u32>, 32>,
}

@group(0) @binding(0) var<uniform> params: ResolveParams;
@group(0) @binding(1) var<storage, read> locals: array<Similarity>;
@group(0) @binding(2) var<storage, read> post: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read> appearance: array<Appearance>;
@group(0) @binding(4) var<storage, read> topo: array<vec2<u32>>; // (parent, group row)
@group(0) @binding(5) var<storage, read> order: array<u32>;      // dfs position → node
@group(0) @binding(6) var<storage, read_write> worlds: array<Similarity>;
@group(1) @binding(0) var<storage, read_write> groups: array<vec4<f32>>;

fn range_at(r: u32) -> vec2<u32> {
    let v = params.ranges[r / 2u];
    if (r & 1u) == 0u {
        return v.xy;
    }
    return v.zw;
}

fn rotate(q: vec4<f32>, v: vec3<f32>) -> vec3<f32> {
    let qc = cross(q.xyz, v) + v * q.w;
    return v + 2.0 * cross(q.xyz, qc);
}

fn quat_mul(a: vec4<f32>, b: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(a.w * b.xyz + b.w * a.xyz + cross(a.xyz, b.xyz), a.w * b.w - dot(a.xyz, b.xyz));
}

fn compose(p: Similarity, c: Similarity) -> Similarity {
    var out: Similarity;
    out.t_s = vec4<f32>(p.t_s.xyz + rotate(p.q, c.t_s.xyz * p.t_s.w), p.t_s.w * c.t_s.w);
    out.q = quat_mul(p.q, c.q);
    return out;
}

@compute @workgroup_size(64)
fn resolve(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let t = gid.x + gid.y * nwg.x * WG;
    if t >= params.total {
        return;
    }
    // The range holding thread t: the last whose first thread is <= t.
    var lo = 0u;
    var hi = params.range_count;
    while hi - lo > 1u {
        let mid = (lo + hi) / 2u;
        if range_at(mid).y <= t {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let r = range_at(lo);
    let node = order[r.x + (t - r.y)];

    var acc = locals[node];
    var alpha = appearance[node].tint.w;
    var p = topo[node].x;
    var steps = 0u;
    loop {
        if p == NONE || steps >= params.max_depth {
            break;
        }
        acc = compose(locals[p], acc);
        alpha = appearance[p].tint.w * alpha;
        p = topo[p].x;
        steps = steps + 1u;
    }
    worlds[node] = acc;

    let row = topo[node].y;
    if row != NONE && row < params.row_limit {
        let g = row * GROUP_STRIDE;
        let app = appearance[node];
        groups[g] = vec4<f32>(acc.t_s.xyz, groups[g].w);
        groups[g + 1u] = acc.q;
        groups[g + 2u] = vec4<f32>(app.tint.xyz, alpha);
        groups[g + 3u] = vec4<f32>(post[node].xyz * acc.t_s.w, app.blend);
    }
}
