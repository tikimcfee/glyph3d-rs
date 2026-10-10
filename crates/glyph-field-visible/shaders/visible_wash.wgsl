// visible_wash.wgsl — the WASH tier: one BOX per line whose rows project
// under `lod_glyph_px`, in its spans' mean colour, drawn after the glyph
// pass with the glyph pass's depth state (tested and written, GreaterEqual)
// and premultiplied blend.
//
// The box is the volume the line's glyphs would fill: x from the entry's
// origin over its widest fold unit (Pass 1's `width_cells`, exactly what the
// glyphs reach); y the first row's cell, stacked `rows` rows down for a
// WrapDown line; z from segment 0's depth to the last segment's (`nseg`,
// the Derived shader's `derive_yz` on both, so the front face sits exactly
// where the line's first row of glyphs would and the back face where its
// deepest fold would). A line that spans no depth draws its front face
// only: the other five would coincide with it or have no area, and a
// coincident face blended twice would double the fade band's alpha.
//
// C28 (2026-10-10): before this the wash was ONE flat quad at segment 0's
// depth, as wide as the cull's conservative byte bound (2 x fold unit): a
// back-wrapped line at the wash tier ran off to the right instead of
// receding, the full-wash layer of the cull-state tint being where Ivan
// saw it. The witness is `tests/gpu.rs`' wash-vs-glyph extent check.
//
// Colour = entry colour x group colour, as the Derived shader blends; alpha
// = the entry's alpha (1 under the threshold, the fade over the 1 px band
// above it) x the group's.

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

struct Wash { item: u32, row_lane: u32, x0: f32, width: f32, color: u32, rows: u32, alpha: f32, tint: u32, nseg: u32 };

struct Frame {
    view_proj: mat4x4<f32>,
    planes: array<vec4<f32>, 6>,
    eye: vec4<f32>,
    lod: vec4<f32>,
    u0: vec4<u32>,
    u1: vec4<u32>,
};

@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var<storage, read> wash: array<Wash>;
@group(0) @binding(2) var<storage, read> groups: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read> item_params: array<ItemParamsGpu>;

const GROUP_STRIDE: u32 = 6u;
const OFF_SCREEN: vec4<f32> = vec4<f32>(2.0, 2.0, 2.0, 1.0);

// glyph_field_derived.wgsl's derive_yz, verbatim.
fn derive_yz(row_lane: u32, wrap_segment: u32, item: ItemParamsGpu) -> vec2<f32> {
    var derived_y = 0.0;
    var derived_z = 0.0;
    let row = row_lane & 0xFFFFFFu;
    let x_page = row_lane >> 24u;

    let depth_steps = -(f32(wrap_segment));
    let z_tail = fma(depth_steps, item.z_step_lo, item.origin_z);
    let z_stepped = fma(depth_steps, item.z_step, z_tail);

    if (item.has_page != 0u) {
        let screen_row = i32(row) - item.scroll_rows;
        var y_page = 0;
        if (item.page_rows > 0 && screen_row >= item.page_rows) {
            y_page = screen_row / item.page_rows;
        }
        let pages_wide = max(item.pages_wide, 1);
        let band = y_page / pages_wide;
        let row_in_page = f32(screen_row - y_page * item.page_rows);
        let y_tail = fma(-row_in_page, item.line_height_lo, item.origin_y);
        let y_row_folded = fma(-row_in_page, item.line_height, y_tail);
        derived_y = fma(-(f32(band)), item.band_stride_y, y_row_folded);

        let z_banded = fma(f32(band), item.depth_per_band, z_stepped);
        derived_z = fma(f32(x_page), item.depth_per_col, z_banded);
    } else {
        let y_tail = fma(-(f32(row)), item.line_height_lo, item.origin_y);
        derived_y = fma(-(f32(row)), item.line_height, y_tail);
        derived_z = z_stepped;
    }

    return vec2<f32>(derived_y, derived_z);
}

// The box's 36 vertices as unit-cube corners: face vi / 6 — 0 front (z 0),
// 1 back (z 1), 2 top (y 1), 3 bottom (y 0), 4 left (x 0), 5 right (x 1) —
// two triangles each over the face's (u, v) square: (0,0) (1,0) (1,1) (1,1)
// (0,1) (0,0). The index buffer is the identity 0..36 (gpu.rs).
fn box_corner(vi: u32) -> vec3<f32> {
    let face = vi / 6u;
    let k = vi % 6u;
    let u = f32(k == 1u || k == 2u || k == 3u);
    let v = f32(k == 2u || k == 3u || k == 4u);
    switch face {
        case 0u: { return vec3<f32>(u, v, 0.0); }
        case 1u: { return vec3<f32>(u, v, 1.0); }
        case 2u: { return vec3<f32>(u, 1.0, v); }
        case 3u: { return vec3<f32>(u, 0.0, v); }
        case 4u: { return vec3<f32>(0.0, v, u); }
        default: { return vec3<f32>(1.0, v, u); }
    }
}

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec3<f32>,
    @location(1) alpha: f32,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VsOut {
    let e = wash[ii];
    let item = item_params[e.item];
    let nseg = max(e.nseg, 1u);
    // The first row's y and segment 0's z; the last segment's z.
    let yz0 = derive_yz(e.row_lane, 0u, item);
    let yz1 = derive_yz(e.row_lane, nseg - 1u, item);
    let c = box_corner(vi);
    // The first row's cell spans y +- 0.5 (the glyph quad); further rows
    // stack down by the line height.
    let top = yz0.x + 0.5;
    let bottom = yz0.x - 0.5 - f32(max(e.rows, 1u) - 1u) * item.line_height;
    let aligned = vec3<f32>(e.x0 + c.x * e.width, mix(bottom, top, c.y), mix(yz0.y, yz1.y, c.z));
    // No depth to span: the front face alone.
    let flat = nseg == 1u || abs(yz1.y - yz0.y) < 1e-6;

    let rows = arrayLength(&groups) / GROUP_STRIDE;
    let gbase = min(item.group, rows - 1u) * GROUP_STRIDE;
    let gpos = groups[gbase];
    let gquat = groups[gbase + 1u];
    let gcolor = groups[gbase + 2u];
    let gscale = groups[gbase + 3u];
    let gclip = groups[gbase + 4u];
    let local = aligned * gscale.xyz;
    let qc = cross(gquat.xyz, local) + local * gquat.w;
    let posed = local + 2.0 * cross(gquat.xyz, qc);
    var clip = frame.view_proj * vec4<f32>(posed + gpos.xyz, 1.0);
    if (gcolor.a <= 0.01 || e.alpha <= 0.0) {
        clip = OFF_SCREEN;
    }
    if (gclip.z > 0.5 && (yz0.x > gclip.x || yz0.x < gclip.y)) {
        clip = OFF_SCREEN;
    }
    if (flat && vi >= 6u) {
        clip = OFF_SCREEN;
    }

    var packed = e.color;
    if (e.tint != 0u) { packed = e.tint; }
    let icolor = vec3<f32>(
        f32(packed & 0xFFu),
        f32((packed >> 8u) & 0xFFu),
        f32((packed >> 16u) & 0xFFu),
    ) / 255.0;
    let base_color = icolor * gcolor.rgb;
    let blended = base_color + (gcolor.rgb - base_color) * gscale.w;

    var out: VsOut;
    out.clip = clip;
    out.color = blended;
    out.alpha = clamp(e.alpha, 0.0, 1.0) * gcolor.a;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    if (in.alpha <= 0.0) { discard; }
    return vec4<f32>(pow(in.color, vec3<f32>(2.2)) * in.alpha, in.alpha);
}
