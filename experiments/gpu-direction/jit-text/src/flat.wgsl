// flat.wgsl — the SAME Derived slots drawn as flat quads: the control for
// "the cost of real text". It binds the Slug pipeline's bind group (only the
// bindings it names are read), derives Y/Z exactly as glyph_field_derived.wgsl
// does (one group per item, identity rotation), poses the quad through the
// group offset, and writes the slot colour with no coverage evaluation.

struct DerivedSlot { x: f32, row: u32, glyph_and_wrap: u32, color: u32, item_and_group: u32 }
struct ItemParamsGpu {
    line_height: f32, origin_y: f32, origin_z: f32, z_step: f32,
    z_step_lo: f32, band_stride_y: f32, depth_per_band: f32, depth_per_col: f32,
    page_rows: i32, pages_wide: i32, page_cols: i32, scroll_rows: i32,
    has_page: u32, line_height_lo: f32, _pad1: u32, _pad2: u32,
}
struct Camera { view_proj: mat4x4<f32> }

@group(0) @binding(0) var<uniform> camera: Camera;
@group(0) @binding(1) var<storage, read> instances: array<DerivedSlot>;
@group(0) @binding(2) var<storage, read> groups: array<vec4<f32>>;
@group(0) @binding(8) var<storage, read> item_table: array<ItemParamsGpu>;
@group(0) @binding(9) var<storage, read> glyph_advances: array<f32>;

struct VsOut { @builtin(position) clip: vec4<f32>, @location(0) color: vec4<f32> }

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VsOut {
    var corners = array<vec2<f32>, 4>(vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0));
    let inst = instances[ii];
    let item = item_table[inst.item_and_group];
    let wrap_segment = inst.glyph_and_wrap >> 16u;
    let glyph_id = inst.glyph_and_wrap & 0xFFFFu;
    let depth_steps = -(f32(wrap_segment));
    let z = fma(depth_steps, item.z_step, fma(depth_steps, item.z_step_lo, item.origin_z));
    let y = fma(-(f32(inst.row)), item.line_height, fma(-(f32(inst.row)), item.line_height_lo, item.origin_y));
    let c = corners[vi];
    let local = vec3<f32>(inst.x + c.x * glyph_advances[glyph_id], y + (c.y - 0.5), z);
    let gbase = inst.item_and_group * 6u;
    let gpos = groups[gbase];
    let gcolor = groups[gbase + 2u];
    var out: VsOut;
    out.clip = camera.view_proj * vec4<f32>(local + gpos.xyz, 1.0);
    let icolor = vec3<f32>(f32(inst.color & 0xFFu), f32((inst.color >> 8u) & 0xFFu), f32((inst.color >> 16u) & 0xFFu)) / 255.0;
    out.color = vec4<f32>(pow(icolor * gcolor.rgb, vec3<f32>(2.2)), 1.0);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}
