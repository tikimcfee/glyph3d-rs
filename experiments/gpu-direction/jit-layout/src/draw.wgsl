// draw.wgsl — instanced quads straight from the transient slot buffer, plus one
// quad per backdrop file.
//
// Vertex stage derives y = origin.y - row * 1.2 from the slot and a per-item
// origin (the view's placement of files: a column for a reading view, a grid
// for the overview, the shelf for the camera presets), the same shape as the
// Derived field's vertex shader. The wrap segment goes to depth as in
// `--wrap-mode back`. The camera is a full view-projection (orthographic for
// the synthetic views, perspective for the presets); quads are clamped to at
// least one pixel in NDC so a zoomed-out view still shows where the text is.
// A slot of a dense+xf item reads its transform from the parallel transient
// stream the layout wrote: (dx, dy, dz) added in world units, scale on the quad.
// Fragment writes the slot's flat colour, optionally mixed with a per-item
// tint. No glyph shapes: the Slug coverage cost of the real renderer is
// identical for both designs and is NOT under test.

struct Slot { x: f32, row: u32, glyph_and_wrap: u32, color: u32, item: u32 }
struct Camera { vp: mat4x4<f32>, planes: array<vec4<f32>, 6>, viewport: vec2<f32>, lod_px: f32, px_scale: f32, eye: vec3<f32>, tint: u32, default_color: u32, flags: u32, _p: vec2<u32> }
struct FileBox { ox: f32, oy: f32, w: f32, h: f32, first_line: u32, line_count: u32, depth: f32, _p: u32 }
struct Item { first_line: u32, line_count: u32, byte_start: u32, byte_len: u32, max_len: u32, glyph_count: u32, span_base: u32, span_count: u32, repr: u32, dense_base: u32, _p0: u32, _p1: u32 }

@group(0) @binding(0) var<storage, read> slots: array<Slot>;
@group(0) @binding(1) var<uniform> cam: Camera;
@group(0) @binding(2) var<storage, read> origins: array<vec2<f32>>;
@group(0) @binding(3) var<storage, read> boxes: array<FileBox>;
@group(0) @binding(4) var<storage, read> backdrops: array<u32>;
@group(0) @binding(5) var<storage, read> items: array<Item>;
@group(0) @binding(6) var<storage, read> slot_xf: array<vec2<u32>>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
}

fn hue_rgb(h: f32) -> vec3<f32> {
    let k = fract(vec3<f32>(h, h + 2.0 / 3.0, h + 1.0 / 3.0)) * 6.0;
    return clamp(abs(k - 3.0) - 1.0, vec3<f32>(0.0), vec3<f32>(1.0));
}

fn item_hue(item: u32) -> vec3<f32> {
    var h = item * 2654435761u;
    h = h ^ (h >> 15u);
    return hue_rgb(f32(h & 0xFFFFu) / 65536.0);
}

/// A world-space quad from `p0` spanning `size` (x right, y up), clamped to a pixel in NDC.
fn quad(p0: vec3<f32>, size: vec2<f32>, cx: f32, cy: f32) -> vec4<f32> {
    let c0 = cam.vp * vec4<f32>(p0, 1.0);
    let c1 = cam.vp * vec4<f32>(p0 + vec3<f32>(size, 0.0), 1.0);
    let w0 = select(c0.w, 1e-6, abs(c0.w) < 1e-6);
    let w1 = select(c1.w, 1e-6, abs(c1.w) < 1e-6);
    let n0 = c0.xyz / w0;
    let n1 = c1.xy / w1;
    let sz = max(n1 - n0.xy, 2.0 / cam.viewport);
    return vec4<f32>(n0.xy + vec2<f32>(cx, cy) * sz, clamp(n0.z, 0.0, 1.0), 1.0);
}

fn corner(vi: u32) -> vec2<f32> {
    // Two triangles per quad: (0,0) (1,0) (1,1)  (0,0) (1,1) (0,1).
    return vec2<f32>(f32(u32(vi == 1u || vi == 2u || vi == 4u)), f32(u32(vi == 2u || vi == 4u || vi == 5u)));
}

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VsOut {
    let s = slots[ii];
    let c = corner(vi);
    let seg = s.glyph_and_wrap >> 16u;
    let o = origins[s.item];
    var world = vec3<f32>(o.x + s.x, o.y - f32(s.row) * 1.2, -f32(seg) * 0.1);
    var size = vec2<f32>(0.6, 1.2);
    if (items[s.item].repr == 3u) {
        let xf = slot_xf[ii];
        let a = unpack2x16float(xf.x);
        let b = unpack2x16float(xf.y);
        world = world + vec3<f32>(a, b.x);
        size = size * b.y;
    }
    var out: VsOut;
    out.pos = quad(world, size, c.x, c.y);
    let col = s.color;
    var rgb = vec3<f32>(f32(col & 0xFFu), f32((col >> 8u) & 0xFFu), f32((col >> 16u) & 0xFFu)) / 255.0;
    if (cam.tint != 0u) { rgb = mix(rgb, item_hue(s.item), 0.55); }
    out.color = vec4<f32>(rgb, 1.0);
    return out;
}

@vertex
fn vs_backdrop(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VsOut {
    let item = backdrops[ii];
    let b = boxes[item];
    let c = corner(vi);
    var out: VsOut;
    out.pos = quad(vec3<f32>(b.ox, b.oy + 1.2 - b.h, -0.05), vec2<f32>(max(b.w, 0.6), b.h), c.x, c.y);
    out.color = vec4<f32>(mix(vec3<f32>(0.45, 0.48, 0.52), item_hue(item), 0.5) * 0.6, 1.0);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}
