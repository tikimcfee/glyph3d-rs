// Stage C — Slug analytic-coverage glyph field.
//
// WGSL port of the web renderer's two shaders (semantics, not TSL API):
//   vertex:   packages/glyph3d-core/src/core/glyphVertex.js
//             (glyph-map lookup → quad sizing → per-instance position →
//              group TRS → MVP → vertex culls)
//   fragment: packages/glyph3d-core/src/GlyphField.js _buildOutputNode()
//             (fractional winding-number coverage over quadratic beziers
//              along +X and +Y rays, scaled by fwidth(glyphUV); continuous
//              minification ramp = dilate + soften)
//
// Textures are Rgba32Uint, sampled with textureLoad (no filtering):
//   glyphmap[g] = [curveStart, curveCount, mode, emojiCell]  (1 texel/slot)
//   curves: 2 texels/curve: [P0.xy, P1.xy], [P2.xy, _, _], uint16-in-u32,
//           normalized per-glyph-cell [0,1], y-UP (0=descender, 1=ascender).
//
// Colour emoji (2026-09-10, out/EMOJI.md): mode==1 samples the emoji sheet —
// an Rgba8UnormSrgb 2D-array texture of STRAIGHT-alpha cells (atlas.rs). The
// alpha contract, stated once so another platform can check its own product:
//   1. the sampler returns LINEAR rgb (the sRGB decode is the format's) and
//      straight alpha, mip-filtered from levels that were box-filtered in
//      PREMULTIPLIED space and stored straight (atlas.rs box_down_straight);
//   2. the group tint is authored sRGB, decoded here with the same pow(2.2)
//      the outline path uses, and multiplied in — identity for a white group;
//   3. output is PREMULTIPLIED, rgb * alpha, into the same ONE/ONE_MINUS_SRC
//      blend as the outline path. Premultiplying BEFORE the sRGB decode would
//      be a different product; that is the mistake to look for if emoji edges
//      differ across platforms while text does not.
// The per-instance colour is not applied: it is the syntax colour, and an
// image has its own. Group alpha and the clip/cull rules apply as for text.
//
// Skipped vs the web (documented in the Stage C report):
//   - frame mode (external video grid)
//   - highlight tint/fill (vAddedColor/vFillAmount)
//   - stipple-dither LOD fade band (ditherSpan); hard discard at alpha==0
//   - width-compression dial (k = 1)

// Stage F note: the renderer no longer draws the whole chunk per frame.
// The CPU cull (glyph_scene.rs module header) issues ONE RANGE DRAW per
// visible segment per chunk — draw(0..6, base..base+count), instance_index
// chunk-local — so this shader is byte-for-byte unchanged from Stage E2 and
// blending order within each chunk still matches the legacy full draws
// (segment ranges ascend in arena order). Far (subpixel) segments are
// substituted by flat backdrop quads from cull.wgsl, never partially drawn.

const MAX_CURVES: u32 = 256u;
const TEX_W: i32 = 1024;
const GROUP_STRIDE: u32 = 5u; // vec4s per group row (glyphVertex.js GROUP_STRIDE)

// Per-instance glyph slot — 32 B / 8 lanes, the endpoint form (note 23):
// what remains of the web's stride-11 byte-slot layout once the dead lanes
// fell out (row/col, flags, _pad — note 22's sweep found no live reader:
// pick rides the engine cache, the verbs write by slot offset, the tint
// fold wants glyph_id+color). color and group_id are per-instance
// ATTRIBUTES on classic web fields; keeping them inline makes the record
// self-contained for the native port. Layout:
//   w0-2  pos.xyz       world anchor: pen origin (left edge), cell-vertical center
//   w3    glyph_id      FontChain global slot (keys glyphmap)
//   w4    color         packed RGBA8 (sRGB display values)
//   w5    group_id      index into the group table
//   w6-7  advance, height   world units (advance = cell width; height = cell height)
struct InstanceSlot {
    pos: vec3<f32>,
    glyph_id: u32,
    color: u32,
    group_id: u32,
    advance: f32,
    height: f32,
};

struct Camera {
    view_proj: mat4x4<f32>,
};

struct Params {
    max_groups: u32,   // group table row count (OOB group ids cull)
    _pad0: u32,
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
    _pad3: vec2<u32>,
};

@group(0) @binding(0) var<uniform> camera: Camera;
@group(0) @binding(1) var<storage, read> instances: array<InstanceSlot>;
@group(0) @binding(2) var<storage, read> groups: array<vec4<f32>>;
@group(0) @binding(3) var glyphmap: texture_2d<u32>;
@group(0) @binding(4) var curves: texture_2d<u32>;
@group(0) @binding(5) var<uniform> params: Params;
@group(0) @binding(6) var emoji_tex: texture_2d_array<f32>;
@group(0) @binding(7) var emoji_samp: sampler;

// Sentinel in the glyph map's .w for a bitmap slot the sheet has no cell
// for (a web-era slot the vendored font cannot draw): rendered blank.
const NO_CELL: u32 = 0xFFFFFFFFu;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec3<f32>,
    @location(1) group_alpha: f32,
    @location(2) glyph_uv: vec2<f32>,
    @location(3) @interpolate(flat) curve_start: u32,
    @location(4) @interpolate(flat) curve_count: u32,
    @location(5) @interpolate(flat) mode: u32,
    // Emoji: the sheet UV (texels/layer size, already inset half a texel and
    // flipped so v runs down the PNG's rows) and the layer. group_rgb is the
    // tint without the instance colour, which an image does not take.
    @location(6) emoji_uv: vec2<f32>,
    @location(7) @interpolate(flat) emoji_layer: u32,
    @location(8) group_rgb: vec3<f32>,
};

@vertex
fn vs_main(
    @builtin(vertex_index) vi: u32,
    @builtin(instance_index) ii: u32,
) -> VsOut {
    // Unit quad corners, uv == position: (0,0)=bottom-left … (1,1)=top-right.
    // Matches the web's PlaneGeometry where uv = positionLocal + 0.5 and the
    // v axis runs bottom→top, so NO y-flip against the y-up curve data.
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );

    let inst = instances[ii];

    // Glyph-map lookup: slot → curve range + mode.
    let gid = i32(inst.glyph_id);
    let info = textureLoad(glyphmap, vec2<i32>(gid % TEX_W, gid / TEX_W), 0);
    let mode = info.z; // 0 = outline, 1 = bitmap emoji

    // Quad sizing: bitmap glyphs get a SQUARE quad; outline keeps the narrow
    // advance. (widthCompress k = 1 — dial not ported.)
    var quad_w = inst.advance;
    if mode == 1u {
        quad_w = inst.height;
    }

    let c = corners[vi];
    // local quad: x ∈ [0, quadW] (pen origin at left edge), y ∈ [-h/2, +h/2],
    // anchored at inst.pos — same shape as scaled+alignOffset+iPos in the web.
    let aligned = vec3<f32>(c.x * quad_w, (c.y - 0.5) * inst.height, 0.0) + inst.pos;

    // Group table row (5 vec4s): offset / quat / color+alpha / scale+colorBlend / clip.
    // Robust storage access CLAMPS OOB reads — so clamp here AND cull below,
    // exactly like the web's explicit bound check.
    let grow = min(inst.group_id, params.max_groups - 1u);
    let gbase = grow * GROUP_STRIDE;
    let gpos = groups[gbase];        // col 0: offset.xyz
    let gquat = groups[gbase + 1u];  // col 1: rotation quaternion xyzw
    let gcolor = groups[gbase + 2u]; // col 2: color.rgb + alpha
    let gscale = groups[gbase + 3u]; // col 3: scale.xyz + colorBlend (w)
    let gclip = groups[gbase + 4u];  // col 4: clipTop, clipBottom, clipEnabled

    // World = rotate(quat, aligned * groupScale) + groupOffset  (T·R·S).
    let local = aligned * gscale.xyz;
    // v' = v + 2·q.xyz × (q.xyz × v + q.w·v) — quat sandwich, cross-form.
    let qc = cross(gquat.xyz, local) + local * gquat.w;
    let posed = local + 2.0 * cross(gquat.xyz, qc);
    var clip = camera.view_proj * vec4<f32>(posed + gpos.xyz, 1.0);

    // Vertex culls → degenerate to outside-NDC (z/w = 2 > 1): GPU clips them.
    if inst.group_id >= params.max_groups || gcolor.a <= 0.01 {
        clip = vec4<f32>(2.0, 2.0, 2.0, 1.0);
    }
    if gclip.z > 0.5 && (inst.pos.y > gclip.x || inst.pos.y < gclip.y) {
        clip = vec4<f32>(2.0, 2.0, 2.0, 1.0);
    }

    // Instance color (packed sRGB RGBA8) blended with the group color:
    // colorBlend 0 = multiply, 1 = replace. Explicit lerp (web comment:
    // TSL .mix() returned the wrong operand at t=0).
    let icolor = vec3<f32>(
        f32(inst.color & 0xFFu),
        f32((inst.color >> 8u) & 0xFFu),
        f32((inst.color >> 16u) & 0xFFu),
    ) / 255.0;
    let base_color = icolor * gcolor.rgb;
    let blended = base_color + (gcolor.rgb - base_color) * gscale.w;

    // Emoji cell → sheet UV. Cell i sits at layer i / (cols·rows), row
    // (i mod cols·rows) / cols, col i mod cols. The sample rect is the cell
    // inset by half a texel on every side so bilinear filtering never reads
    // the neighbouring cell's edge texel, at any mip the loader built
    // (atlas.rs mip_levels_for keeps every level's footprint inside a cell).
    // v is flipped: glyph_uv.y runs bottom→top, the PNG's rows run top→down.
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
    // A bitmap slot with no cell draws nothing: pass mode 2 so the fragment
    // stage discards it before the curve path can misread curve_count == 0.
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
    return out;
}

// Rotate 90° so the +X ray becomes a +Y ray in the rotated frame.
fn rot90(v: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(v.y, -v.x);
}

// Analytic coverage of one quadratic bezier for a +X ray through the origin
// (endpoints pre-translated by the sample point). invDiameter = 1 / pixel
// footprint along the ray axis; fractional crossings give sub-pixel coverage.
// Direct port of computeCoverage in GlyphField.js (Dobbie/Lengyel "Slug"),
// including the stable-root fix and the near-horizontal line guard.
fn compute_coverage(inv_diameter: f32, dilate: f32, p0: vec2<f32>, p1: vec2<f32>, p2: vec2<f32>) -> f32 {
    var result = 0.0;

    // Cheap reject: curve entirely on one side of the ray (y == 0).
    let all_above = p0.y > 0.0 && p1.y > 0.0 && p2.y > 0.0;
    let all_below = p0.y < 0.0 && p1.y < 0.0 && p2.y < 0.0;

    if !(all_above || all_below) {
        // Q(t).y = 0 → a.y·t² − 2·b.y·t + c.y = 0 (factor of −2 baked into b).
        let a = p0 - 2.0 * p1 + p2;
        let b = p0 - p1;
        let c = p0;

        var t0 = -1.0;
        var t1 = -1.0;
        var solvable = true;

        if abs(a.y) >= 1e-5 {
            // Quadratic: two roots — t0 always exits, t1 always enters.
            let radicand = b.y * b.y - a.y * c.y;
            if radicand > 0.0 {
                let s = sqrt(radicand);
                // STABLE roots: q = b.y + sign(b.y)·s, then q/a.y and c.y/q.
                let q = b.y + select(-s, s, b.y >= 0.0);
                if b.y >= 0.0 {
                    t0 = c.y / q;
                    t1 = q / a.y;
                } else {
                    t0 = q / a.y;
                    t1 = c.y / q;
                }
            } else {
                solvable = false; // radicand ≤ 0 → no crossing
            }
        } else {
            // Degenerate quadratic = line segment; one root, by direction.
            // Guard: endpoints at (nearly) the same y → segment ∥ ray → skip;
            // the orthogonal ray resolves it stably.
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
    // Bitmap emoji branch — BEFORE the curveCount==0 empty test, as FORMAT.md
    // requires (a bitmap slot has zero curves). See the header for the alpha
    // contract; this is its one implementation.
    if in.mode == 1u {
        let s = textureSample(emoji_tex, emoji_samp, in.emoji_uv, in.emoji_layer);
        let alpha = s.a * in.group_alpha;
        if alpha <= 0.0 {
            discard;
        }
        let rgb = s.rgb * pow(in.group_rgb, vec3<f32>(2.2)) * alpha;
        return vec4<f32>(rgb, alpha);
    }
    // mode 2: a bitmap slot the sheet has no cell for — blank, keeps its cell.
    if in.mode == 2u {
        discard;
    }

    // Empty glyph (space / .notdef): no ink.
    if in.curve_count == 0u {
        discard;
    }

    // Pixel footprint in glyph-UV space, per axis — resolution-independent AA.
    let fw = fwidth(in.glyph_uv);

    // Minification amount m, smoothstep-ramped over [min_lo, min_hi].
    let fw_max = max(fw.x, fw.y);
    var m = clamp((fw_max - params.min_lo) / (params.min_hi - params.min_lo), 0.0, 1.0);
    m = m * m * (3.0 - 2.0 * m);

    // Dilation half-width + softened inverse footprint (identity at m=0).
    let dilate = m * params.dilate_px;
    let inv_d = (vec2<f32>(1.0) / fw) * (1.0 - m * params.soften);

    var coverage = 0.0;
    let n = min(in.curve_count, MAX_CURVES);
    for (var i = 0u; i < n; i = i + 1u) {
        // 2 texels per curve: [P0.xy, P1.xy] then [P2.xy, _, _].
        let ci = (in.curve_start + i) * 2u;
        let t0 = textureLoad(curves, vec2<i32>(i32(ci % 1024u), i32(ci / 1024u)), 0);
        let t1 = textureLoad(curves, vec2<i32>(i32((ci + 1u) % 1024u), i32((ci + 1u) / 1024u)), 0);

        // Unpack uint16 → [0,1], translate so the sample point is the origin.
        let p0 = vec2<f32>(f32(t0.x), f32(t0.y)) / 65535.0 - in.glyph_uv;
        let p1 = vec2<f32>(f32(t0.z), f32(t0.w)) / 65535.0 - in.glyph_uv;
        let p2 = vec2<f32>(f32(t1.x), f32(t1.y)) / 65535.0 - in.glyph_uv;

        coverage += compute_coverage(inv_d.x, dilate, p0, p1, p2);
        coverage += compute_coverage(inv_d.y, dilate, rot90(p0), rot90(p1), rot90(p2));
    }
    // Average the two rays; fills accumulate positive under y-up normalization.
    let cov = clamp(coverage * 0.5, 0.0, 1.0);

    let alpha = cov * in.group_alpha;
    if alpha <= 0.0 {
        discard;
    }

    // Colors are authored as display (sRGB) values; decode to linear — the
    // sRGB target's hardware encode round-trips them back to authored.
    // Output is PREMULTIPLIED (pipeline blends ONE / OneMinusSrcAlpha): the
    // web's vColor·cov with alpha=cov under three's NormalBlending applies cov
    // twice at edges; premultiplied is the correct coverage composite.
    let rgb = pow(in.color, vec3<f32>(2.2)) * alpha;
    return vec4<f32>(rgb, alpha);
}
