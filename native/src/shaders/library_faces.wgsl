// library_faces.wgsl — the library's page faces as mesh instances, written on
// the GPU from the transform tree's resolved world rows (2026-10-10; the
// library on glyph-scene-graph, out/LIBRARY-LAYOUT-FINDINGS-2026-10-10.md
// E16). Until then every face was a bevy scene mesh, and ANY moved sheet
// re-extracted and re-uploaded every face (2.35 MB at 29k files). Now the
// host uploads nothing when a page moves: after the resolve pass, one thread
// per face reads its sheet's world (a `Similarity`, resolve.wgsl's output)
// and writes the 80 B `MeshInstance` mesh.wgsl draws, unchanged.
//
// The arithmetic is bevy's for the face as it was spawned — a child of the
// sheet at (0, 0, -gap), drawing a unit quad scaled to the page and centred
// on `origin + size / 2` — written as the similarity it is: the face's world
// translation is the sheet's plus its offset carried through the sheet's
// rotation and scale (resolve.wgsl's `compose`), its matrix the sheet's
// rotation (glam's `Mat3::from_quat`) times its scale times the page size.
// For the library's sheets, which never rotate, every product is with 1 or 0
// and every sum is with 0 except the offset's, so the instance comes out bit
// for bit as bevy's extraction made it (`library::tests` holds the CPU twin,
// `face_instance`, to bevy's extraction; this pass to the twin through the
// witness frames).

struct Similarity {
    t_s: vec4<f32>, // translation.xyz, uniform scale in w
    q: vec4<f32>,   // rotation xyzw
}

// mesh.wgsl's InstanceInput, as a storage row: 80 B.
struct MeshInstance {
    col0: vec4<f32>, // matrix x axis, translation.x
    col1: vec4<f32>, // matrix y axis, translation.y
    col2: vec4<f32>, // matrix z axis, translation.z
    color: vec4<f32>,
    params: vec4<f32>,
}

// 64 B.
struct FaceParams {
    offset: vec4<f32>, // the face in its sheet's frame (xyz)
    quad: vec4<f32>,   // size.x, size.y, origin.x, origin.y
    color: vec4<f32>,
    count: u32,
    _p0: u32,
    _p1: u32,
    _p2: u32,
}

@group(0) @binding(0) var<uniform> params: FaceParams;
@group(0) @binding(1) var<storage, read> worlds: array<Similarity>;
@group(0) @binding(2) var<storage, read> sheets: array<u32>;
@group(0) @binding(3) var<storage, read_write> faces: array<MeshInstance>;

fn rotate(q: vec4<f32>, v: vec3<f32>) -> vec3<f32> {
    let qc = cross(q.xyz, v) + v * q.w;
    return v + 2.0 * cross(q.xyz, qc);
}

@compute @workgroup_size(64)
fn write_faces(@builtin(global_invocation_id) gid: vec3<u32>) {
    let k = gid.x;
    if k >= params.count {
        return;
    }
    let w = worlds[sheets[k]];
    let s = w.t_s.w;
    let q = w.q;
    let t = w.t_s.xyz + rotate(q, params.offset.xyz * s);
    // glam's Mat3::from_quat, column by column.
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
    var out: MeshInstance;
    // bevy's extraction skipped a face whose world had (almost) no scale; a
    // zero instance draws nothing.
    if dot(mx, mx) + dot(my, my) + dot(mz, mz) < 1e-6 {
        faces[k] = out;
        return;
    }
    let size = params.quad.xy;
    let centre = params.quad.zw + size * 0.5;
    let ax = mx * size.x;
    let ay = my * size.y;
    let at = t + (mx * centre.x + my * centre.y);
    out.col0 = vec4<f32>(ax, at.x);
    out.col1 = vec4<f32>(ay, at.y);
    out.col2 = vec4<f32>(mz, at.z);
    out.color = params.color;
    out.params = vec4<f32>(0.0);
    faces[k] = out;
}
