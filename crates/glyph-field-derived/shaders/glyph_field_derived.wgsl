// glyph_field_derived.wgsl — Slug analytic-coverage text renderer in Derived mode.
//
// In Derived mode, instance slots are compact 20 B records:
//   w0    x: f32                 world X coordinate (pen origin at left edge)
//   w1    row: u32               row:24 | x_page:8 (derive.rs) — the cell's row in its item and its column page
//   w2    glyph_and_wrap: u32    low 16: glyph_id (atlas slot), high 16: wrap_segment
//   w3    color: u32             packed sRGB RGBA8 (r in bits 0..7, a in 24..31)
//   w4    group_id: u32          index into the group table
//
// Y and Z coordinates are derived in the vertex stage from the item table:
//   item = item_table[inst.item_and_group]   // ItemParamsGpu (64 B)
//   yz   = derive_yz(inst.row, wrap_segment, item)   // row lane: row:24 | x_page:8
// (`derive.rs` carries the same function in Rust, held to the Instanced
// emitter's positions by `hyper_oracle::tests::derived_vertex_stage_agrees_with_instanced`.)
//
// Advance is looked up from the resident atlas table:
//   advance = glyph_advances[glyph_id]
// Cell height is constant 1.0.

const TEX_W: i32 = 1024;
const MAX_CURVES: u32 = 128u;
const GROUP_STRIDE: u32 = 6u;

struct DerivedSlot {
    x: f32,
    row: u32,
    glyph_and_wrap: u32,
    color: u32,
    item_and_group: u32,
};

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
    _pad1: u32,
    _pad2: u32,
};

struct Camera {
    view_proj: mat4x4<f32>,
};

struct Params {
    max_groups: u32,   // group table row count (OOB group ids cull)
    greek_mode: u32,   // 0 = disabled, 1 = smooth blend (default), 2 = pure hard bypass
    _pad1: u32,
    _pad2: u32,
    // Minification dials — GLYPH_LOD_DEFAULTS (GlyphField.js)
    dilate_px: f32,    // 0.75 — stroke fattening half-width at full zoom-out
    soften: f32,       // 0.45 — AA ramp widening factor
    min_lo: f32,       // 0.06 — fuzz onset (footprint)
    min_hi: f32,       // 0.20 — fuzz full  (footprint)
    // Emoji sheet geometry (G3ES header, atlas.rs). A cell's placement is a
    // pure function of its index — the same function the generator used —
    // so no cell table crosses to the GPU.
    emoji_cell: vec2<u32>,    // cell width, height in texels
    emoji_cols: u32,          // cells per row
    emoji_rows: u32,          // rows per layer
    emoji_layer: vec2<f32>,   // layer width, height in texels (as f32 for UV math)
    greek_onset_px: f32,      // on-screen glyph px/em onset for Greeking (default 10.0)
    _pad3: u32,
};

@group(0) @binding(0) var<uniform> camera: Camera;
@group(0) @binding(1) var<storage, read> instances: array<DerivedSlot>;
@group(0) @binding(2) var<storage, read> groups: array<vec4<f32>>;
@group(0) @binding(3) var glyphmap: texture_2d<u32>;
@group(0) @binding(4) var curves: texture_2d<u32>;
@group(0) @binding(5) var<uniform> params: Params;
@group(0) @binding(6) var emoji_tex: texture_2d_array<f32>;
@group(0) @binding(7) var emoji_samp: sampler;
@group(0) @binding(8) var<storage, read> item_table: array<ItemParamsGpu>;
@group(0) @binding(9) var<storage, read> glyph_advances: array<f32>;

// Sentinel in the glyph map's .w for a bitmap slot the sheet has no cell
// for (a web-era slot the vendored font cannot draw): rendered blank.
const NO_CELL: u32 = 0xFFFFFFFFu;

// The row lane is `row:24 | x_page:8` (derive.rs): the column page rides the
// high byte so z can carry fold::paginate's `x_page * depth_per_col` term,
// which has no other source in the vertex stage (the slot has no column).
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

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec3<f32>,
    @location(1) group_alpha: f32,
    @location(2) glyph_uv: vec2<f32>,
    @location(3) @interpolate(flat) curve_start: u32,
    @location(4) @interpolate(flat) curve_count: u32,
    @location(5) @interpolate(flat) mode: u32,
    @location(6) emoji_uv: vec2<f32>,
    @location(7) @interpolate(flat) emoji_layer: u32,
    @location(8) group_rgb: vec3<f32>,
    @location(9) bg_color: vec4<f32>,
};

@vertex
fn vs_main(
    @builtin(vertex_index) vi: u32,
    @builtin(instance_index) ii: u32,
) -> VsOut {
    var corners = array<vec2<f32>, 4>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );

    let inst = instances[ii];
    let item_idx = inst.item_and_group;
    let group_id = inst.item_and_group;
    let item = item_table[item_idx];
    let wrap_segment = inst.glyph_and_wrap >> 16u;
    let glyph_id = inst.glyph_and_wrap & 0xFFFFu;
    let yz = derive_yz(inst.row, wrap_segment, item);
    let inst_pos = vec3<f32>(inst.x, yz.x, yz.y);

    // Glyph-map lookup: slot → curve range + mode.
    let gid = i32(glyph_id);
    let info = textureLoad(glyphmap, vec2<i32>(gid % TEX_W, gid / TEX_W), 0);
    let mode = info.z; // 0 = outline, 1 = bitmap emoji

    var quad_w = glyph_advances[glyph_id];
    if mode == 1u {
        quad_w = 1.0;
    }

    let c = corners[vi];
    let aligned = vec3<f32>(c.x * quad_w, (c.y - 0.5) * 1.0, 0.0) + inst_pos;

    // Group table row (6 vec4s): offset / quat / color+alpha / scale+colorBlend / clip / bg_color.
    let grow = min(group_id, params.max_groups - 1u);
    let gbase = grow * GROUP_STRIDE;
    let gpos = groups[gbase];        // col 0: offset.xyz
    let gquat = groups[gbase + 1u];  // col 1: rotation quaternion xyzw
    let gcolor = groups[gbase + 2u]; // col 2: color.rgb + alpha
    let gscale = groups[gbase + 3u]; // col 3: scale.xyz + colorBlend (w)
    let gclip = groups[gbase + 4u];  // col 4: clipTop, clipBottom, clipEnabled
    let gbg = groups[gbase + 5u];    // col 5: bg_color.rgba

    // World = rotate(quat, aligned * groupScale) + groupOffset (T·R·S).
    let local = aligned * gscale.xyz;
    let qc = cross(gquat.xyz, local) + local * gquat.w;
    let posed = local + 2.0 * cross(gquat.xyz, qc);
    var clip = camera.view_proj * vec4<f32>(posed + gpos.xyz, 1.0);

    // Vertex culls → degenerate to outside-NDC.
    if group_id >= params.max_groups || gcolor.a <= 0.01 {
        clip = vec4<f32>(2.0, 2.0, 2.0, 1.0);
    }
    if gclip.z > 0.5 && (inst_pos.y > gclip.x || inst_pos.y < gclip.y) {
        clip = vec4<f32>(2.0, 2.0, 2.0, 1.0);
    }

    let icolor = vec3<f32>(
        f32(inst.color & 0xFFu),
        f32((inst.color >> 8u) & 0xFFu),
        f32((inst.color >> 16u) & 0xFFu),
    ) / 255.0;
    let base_color = icolor * gcolor.rgb;
    let blended = base_color + (gcolor.rgb - base_color) * gscale.w;

    var emoji_uv = vec2<f32>(0.0);
    var emoji_layer = 0u;
    if mode == 1u && info.w != NO_CELL {
        let per_layer = params.emoji_cols * params.emoji_rows;
        emoji_layer = info.w / per_layer;
        let r = info.w % per_layer;
        let cell_xy = vec2<f32>(f32(r % params.emoji_cols), f32(r / params.emoji_cols)) * vec2<f32>(params.emoji_cell);
        let inset = vec2<f32>(0.5);
        let span = vec2<f32>(params.emoji_cell) - vec2<f32>(1.0);
        let t = vec2<f32>(c.x, 1.0 - c.y);
        emoji_uv = (cell_xy + inset + t * span) / params.emoji_layer;
    }
    var out_mode = mode;
    if mode == 1u && info.w == NO_CELL {
        out_mode = 2u;
    }

    var out: VsOut;
    out.clip = clip;
    out.color = blended;
    out.group_alpha = gcolor.a;
    out.glyph_uv = c;
    out.curve_start = info.x;
    out.curve_count = info.y;
    out.mode = out_mode;
    out.emoji_uv = emoji_uv;
    out.emoji_layer = emoji_layer;
    out.group_rgb = gcolor.rgb;
    out.bg_color = gbg;
    return out;
}

fn rot90(v: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(v.y, -v.x);
}

fn compute_coverage(inv_diameter: f32, dilate: f32, p0: vec2<f32>, p1: vec2<f32>, p2: vec2<f32>) -> f32 {
    var result = 0.0;

    let all_above = p0.y > 0.0 && p1.y > 0.0 && p2.y > 0.0;
    let all_below = p0.y < 0.0 && p1.y < 0.0 && p2.y < 0.0;

    if !(all_above || all_below) {
        let a = p0 - 2.0 * p1 + p2;
        let b = p0 - p1;
        let c = p0;

        var t0 = -1.0;
        var t1 = -1.0;
        var solvable = true;

        if abs(a.y) >= 1e-5 {
            let radicand = b.y * b.y - a.y * c.y;
            if radicand > 0.0 {
                let s = sqrt(radicand);
                let q = b.y + select(-s, s, b.y >= 0.0);
                if b.y >= 0.0 {
                    t0 = c.y / q;
                    t1 = q / a.y;
                } else {
                    t0 = q / a.y;
                    t1 = c.y / q;
                }
            } else {
                solvable = false;
            }
        } else {
            let denom = p0.y - p2.y;
            if abs(denom) >= 1e-6 {
                let t = p0.y / denom;
                if p0.y < p2.y {
                    t0 = -1.0;
                    t1 = t;
                } else {
                    t0 = t;
                    t1 = -1.0;
                }
            } else {
                solvable = false;
            }
        }

        if solvable {
            if t0 >= 0.0 && t0 < 1.0 {
                let x = (a.x * t0 - 2.0 * b.x) * t0 + c.x;
                result += clamp(x * inv_diameter + 0.5 + dilate, 0.0, 1.0);
            }
            if t1 >= 0.0 && t1 < 1.0 {
                let x = (a.x * t1 - 2.0 * b.x) * t1 + c.x;
                result -= clamp(x * inv_diameter + 0.5 - dilate, 0.0, 1.0);
            }
        }
    }

    return result;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    if in.mode == 1u {
        let s = textureSample(emoji_tex, emoji_samp, in.emoji_uv, in.emoji_layer);
        let alpha = s.a * in.group_alpha;
        if alpha <= 0.0 {
            discard;
        }
        let rgb = s.rgb * pow(in.group_rgb, vec3<f32>(2.2)) * alpha;
        return vec4<f32>(rgb, alpha);
    }
    if in.mode == 2u {
        discard;
    }

    // Empty glyph (space / .notdef): no ink.
    if in.curve_count == 0u && in.bg_color.a <= 0.0 {
        discard;
    }

    let fw = fwidth(in.glyph_uv);
    let em_px = 1.0 / max(fw.y, 1e-4);
    var greek = 0.0;
    if in.curve_count > 0u {
        if params.greek_mode == 1u {
            let onset = params.greek_onset_px;
            let full = onset * 0.45;
            let t = clamp((onset - em_px) / max(onset - full, 0.01), 0.0, 1.0);
            greek = t * t * (3.0 - 2.0 * t);
        } else if params.greek_mode == 2u {
            if em_px <= params.greek_onset_px {
                greek = 1.0;
            }
        }
    }

    var cov = 0.0;
    if greek < 1.0 && in.curve_count > 0u {
        let fw_max = max(fw.x, fw.y);
        var m = clamp((fw_max - params.min_lo) / (params.min_hi - params.min_lo), 0.0, 1.0);
        m = m * m * (3.0 - 2.0 * m);

        let dilate = m * params.dilate_px;
        let inv_d = (vec2<f32>(1.0) / fw) * (1.0 - m * params.soften);

        var coverage = 0.0;
        let n = min(in.curve_count, MAX_CURVES);
        for (var i = 0u; i < n; i = i + 1u) {
            let ci = (in.curve_start + i) * 2u;
            let t0 = textureLoad(curves, vec2<i32>(i32(ci % 1024u), i32(ci / 1024u)), 0);
            let t1 = textureLoad(curves, vec2<i32>(i32((ci + 1u) % 1024u), i32((ci + 1u) / 1024u)), 0);

            let p0 = vec2<f32>(f32(t0.x), f32(t0.y)) / 65535.0 - in.glyph_uv;
            let p1 = vec2<f32>(f32(t0.z), f32(t0.w)) / 65535.0 - in.glyph_uv;
            let p2 = vec2<f32>(f32(t1.x), f32(t1.y)) / 65535.0 - in.glyph_uv;

            coverage += compute_coverage(inv_d.x, dilate, p0, p1, p2);
            coverage += compute_coverage(inv_d.y, dilate, rot90(p0), rot90(p1), rot90(p2));
        }
        cov = clamp(coverage * 0.5, 0.0, 1.0);
    }

    if greek > 0.0 && in.curve_count > 0u {
        let dy = max(fw.y, 1e-4);
        let y_cov = clamp((in.glyph_uv.y - 0.20) / dy + 0.5, 0.0, 1.0)
                  - clamp((in.glyph_uv.y - 0.75) / dy + 0.5, 0.0, 1.0);
        let dx = max(fw.x, 1e-4);
        let x_cov = clamp((in.glyph_uv.x - 0.0) / dx + 0.5, 0.0, 1.0)
                  - clamp((in.glyph_uv.x - 1.0) / dx + 0.5, 0.0, 1.0);
        let bar_cov = clamp(x_cov * y_cov, 0.0, 1.0);
        cov = mix(cov, bar_cov, greek);
    }

    let fg_alpha = cov * in.group_alpha;
    let bg_alpha = in.bg_color.a * in.group_alpha;

    if fg_alpha <= 0.0 && bg_alpha <= 0.0 {
        discard;
    }

    let fg_rgb = pow(in.color, vec3<f32>(2.2)) * fg_alpha;
    let bg_rgb = pow(in.bg_color.rgb, vec3<f32>(2.2)) * bg_alpha;

    let out_alpha = fg_alpha + bg_alpha * (1.0 - fg_alpha);
    let out_rgb = fg_rgb + bg_rgb * (1.0 - fg_alpha);

    return vec4<f32>(out_rgb, out_alpha);
}
