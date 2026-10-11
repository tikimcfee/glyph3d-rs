// library_regions.wgsl — the library's bounding regions (2026-10-10; the
// retired JS renderer's ContentTreeMarkers): one translucent box per
// directory below the root, enclosing its whole subtree, so containment and
// relative placement read without a label. Nested boxes overlap, so nesting
// depth reads as density of tint.
//
// Two halves, like the page faces (library_faces.wgsl):
//
// - `write_regions` (compute): one thread per region composes its box, kept
//   in its DIRECTORY's frame (the tree cull's subtree box, padded on the
//   host: regions.rs `region_box`), with that directory's resolved world
//   (resolve.wgsl's `Similarity`) into a world-space instance: three scaled
//   axes and a centre. A drag, an ease, a page turn or a form change moves
//   the box with no host write at all; the host uploads a box only when a
//   refit changed it (a child moved inside it). It runs on frames that
//   resolved something or uploaded a box, never on a settled frame.
// - `vs_region` / `fs_region` (render): 36 vertices per instance from
//   vertex_index, double-sided, drawn after the page faces and before the
//   glyphs with depth TESTED but not written, so a page occludes the box
//   faces behind it, a box face in front of a page tints it, and glyphs
//   drawn later read crisply over every face. The edge is a hairline in
//   screen space (each face's uv against its own derivative), so a box reads
//   as a box at any distance; a face seen edge-on draws as its edge.

struct Similarity {
    t_s: vec4<f32>, // translation.xyz, uniform scale in w
    q: vec4<f32>,   // rotation xyzw
}

// One region, in its directory's frame: 48 B.
struct Region {
    lo: vec4<f32>,    // box min (xyz); w = the directory node's pool slot (bits)
    hi: vec4<f32>,    // box max (xyz)
    color: vec4<f32>, // linear rgb, fill opacity
}

// The world-space instance the render half draws: 64 B (vertex attributes).
struct RegionInstance {
    col0: vec4<f32>, // x axis scaled by the box width, centre.x
    col1: vec4<f32>, // y axis scaled by the box height, centre.y
    col2: vec4<f32>, // z axis scaled by the box depth, centre.z
    color: vec4<f32>,
}

// 32 B.
struct RegionParams {
    edge_opacity: f32,
    edge_px: f32,
    picked_opacity: f32,
    picked_edge_opacity: f32,
    picked: u32, // region index, 0xffffffff: none
    count: u32,
    _p0: u32,
    _p1: u32,
}

@group(0) @binding(0) var<uniform> params: RegionParams;
@group(0) @binding(1) var<storage, read> worlds: array<Similarity>;
@group(0) @binding(2) var<storage, read> regions: array<Region>;
@group(0) @binding(3) var<storage, read_write> instances: array<RegionInstance>;

fn rotate(q: vec4<f32>, v: vec3<f32>) -> vec3<f32> {
    let qc = cross(q.xyz, v) + v * q.w;
    return v + 2.0 * cross(q.xyz, qc);
}

@compute @workgroup_size(64)
fn write_regions(@builtin(global_invocation_id) gid: vec3<u32>) {
    let k = gid.x;
    if k >= params.count {
        return;
    }
    let r = regions[k];
    let w = worlds[bitcast<u32>(r.lo.w)];
    let s = w.t_s.w;
    let q = w.q;
    let size = r.hi.xyz - r.lo.xyz;
    let centre = (r.lo.xyz + r.hi.xyz) * 0.5;
    // glam's Mat3::from_quat, column by column (library_faces.wgsl's).
    let x2 = q.x + q.x;
    let y2 = q.y + q.y;
    let z2 = q.z + q.z;
    let xx = q.x * x2;
    let xy = q.x * y2;
    let xz = q.x * z2;
    let yy = q.y * y2;
    let yz = q.y * z2;
    let zz = q.z * z2;
    let wx = q.w * x2;
    let wy = q.w * y2;
    let wz = q.w * z2;
    let mx = vec3<f32>(1.0 - (yy + zz), xy + wz, xz - wy) * s;
    let my = vec3<f32>(xy - wz, 1.0 - (xx + zz), yz + wx) * s;
    let mz = vec3<f32>(xz + wy, yz - wx, 1.0 - (xx + yy)) * s;
    let at = w.t_s.xyz + rotate(q, centre * s);
    var out: RegionInstance;
    out.col0 = vec4<f32>(mx * size.x, at.x);
    out.col1 = vec4<f32>(my * size.y, at.y);
    out.col2 = vec4<f32>(mz * size.z, at.z);
    out.color = r.color;
    instances[k] = out;
}

// ── render half ─────────────────────────────────────────────────────────

struct Camera {
    view_proj: mat4x4<f32>,
}

// `params` (binding 0) is shared with the compute half; the frame uniform's
// view_proj leads it (the 64 B Camera block, as mesh.wgsl binds it).
@group(0) @binding(4) var<uniform> camera: Camera;

struct InstanceInput {
    @location(0) col0: vec4<f32>,
    @location(1) col1: vec4<f32>,
    @location(2) col2: vec4<f32>,
    @location(3) color: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    // The face's own coordinates, 0..1 across it (the hairline's measure).
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    // 1.0 on the picked region.
    @location(2) @interpolate(flat) picked: f32,
}

// Six faces of two triangles; per face, the axis it faces (0 x, 1 y, 2 z)
// and the side (-0.5 or +0.5). The corner order within a face is shared.
const FACE_UV: array<vec2<f32>, 6> = array<vec2<f32>, 6>(
    vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
);

@vertex
fn vs_region(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32, inst: InstanceInput) -> VertexOutput {
    let face = vi / 6u;
    let uv = FACE_UV[vi % 6u];
    let axis = face / 2u;
    let side = select(-0.5, 0.5, (face & 1u) == 1u);
    // The two in-face axes, in cyclic order after the facing one.
    var c = vec3<f32>(0.0);
    c[axis] = side;
    c[(axis + 1u) % 3u] = uv.x - 0.5;
    c[(axis + 2u) % 3u] = uv.y - 0.5;
    let world = inst.col0.xyz * c.x + inst.col1.xyz * c.y + inst.col2.xyz * c.z + vec3<f32>(inst.col0.w, inst.col1.w, inst.col2.w);
    var out: VertexOutput;
    out.position = camera.view_proj * vec4<f32>(world, 1.0);
    out.uv = uv;
    out.color = inst.color;
    out.picked = select(0.0, 1.0, ii == params.picked);
    return out;
}

@fragment
fn fs_region(in: VertexOutput) -> @location(0) vec4<f32> {
    // Pixels to the nearest edge of this face: each uv coordinate's distance
    // to 0 or 1 over its screen-space rate.
    let fw = max(fwidth(in.uv), vec2<f32>(1e-6));
    let d = min(in.uv, vec2<f32>(1.0) - in.uv) / fw;
    let px = min(d.x, d.y);
    let edge = clamp(params.edge_px + 0.5 - px, 0.0, 1.0);
    let fill = select(in.color.a, params.picked_opacity, in.picked > 0.5);
    let rim = select(params.edge_opacity, params.picked_edge_opacity, in.picked > 0.5);
    let a = mix(fill, rim, edge);
    // Premultiplied, like every other pass into the pool.
    return vec4<f32>(in.color.rgb * a, a);
}
