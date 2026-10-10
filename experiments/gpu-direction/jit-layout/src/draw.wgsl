// draw.wgsl — instanced quads straight from the transient slot buffer.
//
// Vertex stage derives y = origin.y - row * 1.2 from the slot and a per-item
// origin (the view's placement of files: a column for a reading view, a grid
// for the overview), the same shape as the Derived field's vertex shader. The
// wrap segment goes to depth as in `--wrap-mode back`; the camera is
// orthographic, so wraps overlap in the picture. Fragment writes the slot's
// flat colour, optionally mixed with a per-item tint so files are
// distinguishable in the overview. Quads are clamped to at least one pixel
// so a zoomed-out view still shows where the text is (a stand-in for the
// renderer's far-LOD backdrop). No glyph shapes: the Slug coverage cost of
// the real renderer is identical for both designs and is NOT under test.

struct Slot { x: f32, row: u32, glyph_and_wrap: u32, color: u32, item: u32 }
struct Cam { scale: vec2<f32>, offset: vec2<f32>, min_px: vec2<f32>, tint: u32, _pad: u32 }

@group(0) @binding(0) var<storage, read> slots: array<Slot>;
@group(0) @binding(1) var<uniform> cam: Cam;
@group(0) @binding(2) var<storage, read> origins: array<vec2<f32>>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
}

fn hue_rgb(h: f32) -> vec3<f32> {
    let k = fract(vec3<f32>(h, h + 2.0 / 3.0, h + 1.0 / 3.0)) * 6.0;
    return clamp(abs(k - 3.0) - 1.0, vec3<f32>(0.0), vec3<f32>(1.0));
}

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VsOut {
    let s = slots[ii];
    // Two triangles per quad: (0,0) (1,0) (1,1)  (0,0) (1,1) (0,1).
    let cx = f32(u32(vi == 1u || vi == 2u || vi == 4u));
    let cy = f32(u32(vi == 2u || vi == 4u || vi == 5u));
    let seg = s.glyph_and_wrap >> 16u;
    let o = origins[s.item];
    let world = vec2<f32>(o.x + s.x, o.y - f32(s.row) * 1.2);
    let size = max(vec2<f32>(0.6, 1.2) * cam.scale, cam.min_px);
    let z = -f32(seg) * 0.1;
    var out: VsOut;
    out.pos = vec4<f32>(world * cam.scale + cam.offset + vec2<f32>(cx, cy) * size, clamp(0.5 - z * 0.01, 0.0, 1.0), 1.0);
    let c = s.color;
    var rgb = vec3<f32>(f32(c & 0xFFu), f32((c >> 8u) & 0xFFu), f32((c >> 16u) & 0xFFu)) / 255.0;
    if (cam.tint != 0u) {
        var h = s.item * 2654435761u;
        h = h ^ (h >> 15u);
        rgb = mix(rgb, hue_rgb(f32(h & 0xFFFFu) / 65536.0), 0.55);
    }
    out.color = vec4<f32>(rgb, 1.0);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}
