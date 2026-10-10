// draw.wgsl — instanced quads straight from the transient slot buffer.
//
// Vertex stage derives y = -row * 1.2 and z = -wrap_segment * 0.1 from the
// slot, the same shape as the Derived field's vertex shader (minus its
// per-item table). Fragment writes the slot's flat colour: the Slug coverage
// cost of the real renderer is identical for both designs and is NOT under
// test here.

struct Slot { x: f32, row: u32, glyph_and_wrap: u32, color: u32, item: u32 }
struct Cam { scale: vec2<f32>, offset: vec2<f32> }

@group(0) @binding(0) var<storage, read> slots: array<Slot>;
@group(0) @binding(1) var<uniform> cam: Cam;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VsOut {
    let s = slots[ii];
    // Two triangles per quad: (0,0) (1,0) (1,1)  (0,0) (1,1) (0,1).
    let cx = f32(u32(vi == 1u || vi == 2u || vi == 4u));
    let cy = f32(u32(vi == 2u || vi == 4u || vi == 5u));
    let seg = s.glyph_and_wrap >> 16u;
    let y = -f32(s.row) * 1.2;
    let z = -f32(seg) * 0.1;
    let world = vec2<f32>(s.x + cx * 0.6, y + cy * 1.2);
    var out: VsOut;
    out.pos = vec4<f32>(world * cam.scale + cam.offset, clamp(0.5 - z * 0.01, 0.0, 1.0), 1.0);
    let c = s.color;
    out.color = vec4<f32>(f32(c & 0xFFu), f32((c >> 8u) & 0xFFu), f32((c >> 16u) & 0xFFu), f32(c >> 24u)) / 255.0;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}
