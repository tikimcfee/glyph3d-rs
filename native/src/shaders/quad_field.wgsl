// Stage A stress-test shader: instanced colored quads.
//
// Design contract for future stages:
//  - Per-instance data comes from a READ-ONLY STORAGE BUFFER indexed by
//    instance_index in the vertex shader (WebGPU style), NOT from
//    vertex-step-mode instance attributes. Stage B+ will bind the glyph
//    slot buffer (8 x u32 lanes + 7 x f32 measures, ~60B) at the same
//    binding point with the same access pattern — only the struct body
//    changes.
//  - The quad geometry is derived from vertex_index (two triangles,
//    6 verts per instance) so there is no vertex buffer at all.
//    One draw(0..6, 0..N) call renders the whole field.

struct InstanceSlot {
    // --- 48 bytes total, 12 x 4-byte words ---
    // Stage A layout (placeholder; final glyph-slot layout lands in a later stage):
    pos:   vec3<f32>,   // words 0-2:  world-space center on the XZ plane
    scale: f32,         // word 3:     quad edge length
    color: vec4<f32>,   // words 4-7:  linear-ish RGBA (written to an sRGB target)
    lane0: u32,         // words 8-11: identity/count lanes (reserved, unused in A)
    lane1: u32,
    lane2: u32,
    lane3: u32,
};

struct Camera {
    view_proj: mat4x4<f32>,
};

@group(0) @binding(0) var<uniform> camera: Camera;
@group(0) @binding(1) var<storage, read> instances: array<InstanceSlot>;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(
    @builtin(vertex_index) vi: u32,
    @builtin(instance_index) ii: u32,
) -> VsOut {
    // Two CCW triangles as a unit quad, corner lookup by vertex_index.
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-0.5, -0.5),
        vec2<f32>( 0.5, -0.5),
        vec2<f32>( 0.5,  0.5),
        vec2<f32>(-0.5, -0.5),
        vec2<f32>( 0.5,  0.5),
        vec2<f32>(-0.5,  0.5),
    );

    let inst = instances[ii];
    let c = corners[vi];

    // Lay the quad flat on the XZ ground plane around its center.
    let world = vec4<f32>(
        inst.pos.x + c.x * inst.scale,
        inst.pos.y,
        inst.pos.z + c.y * inst.scale,
        1.0,
    );

    var out: VsOut;
    out.clip = camera.view_proj * world;
    out.color = inst.color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}
