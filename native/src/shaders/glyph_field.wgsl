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
// Skipped vs the web (documented in the Stage C report):
//   - bitmap emoji pixels (no atlas exported): mode==1 discards
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

// Per-instance glyph slot — 48 B / 12 lanes. Deliberately close to the web's
// stride-11 byte-slot layout (pos xyz + advance/height bitcast lanes + count
// lanes): the two extra lanes here (color, group_id) are per-instance
// ATTRIBUTES on classic web fields; keeping them inline makes the record
// self-contained for the native port. Layout:
//   w0-2  pos.xyz       world anchor: pen origin (left edge), cell-vertical center
//   w3    glyph_id      FontChain global slot (keys glyphmap)
//   w4-5  row, col      grid position (Stage D picking/far-texture parity)
//   w6    color         packed RGBA8 (sRGB display values)
//   w7    group_id      index into the group table
//   w8-9  advance, height   world units (advance = cell width; height = cell height)
//   w10   flags         trie flags (bit0 MISSING, bit1 BITMAP, bit2 BLANK)
//   w11   _pad
struct InstanceSlot {
    pos: vec3<f32>,
    glyph_id: u32,
    row: u32,
    col: u32,
    color: u32,
    group_id: u32,
    advance: f32,
    height: f32,
    flags: u32,
    _pad: u32,
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
};

@group(0) @binding(0) var<uniform> camera: Camera;
@group(0) @binding(1) var<storage, read> instances: array<InstanceSlot>;
@group(0) @binding(2) var<storage, read> groups: array<vec4<f32>>;
@group(0) @binding(3) var glyphmap: texture_2d<u32>;
@group(0) @binding(4) var curves: texture_2d<u32>;
@group(0) @binding(5) var<uniform> params: Params;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec3<f32>,
    @location(1) group_alpha: f32,
    @location(2) glyph_uv: vec2<f32>,
    @location(3) @interpolate(flat) curve_start: u32,
    @location(4) @interpolate(flat) curve_count: u32,
    @location(5) @interpolate(flat) mode: u32,
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

    var out: VsOut;
    out.clip = clip;
    out.color = blended;
    out.group_alpha = gcolor.a;
    out.glyph_uv = c;
    out.curve_start = info.x;
    out.curve_count = info.y;
    out.mode = mode;
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
    // Bitmap emoji branch: no bitmap atlas was exported — discard (the staging
    // code also skips emoji slots; this is the belt-and-braces branch so the
    // mode test precedes the curveCount==0 empty test, as FORMAT.md requires).
    if in.mode == 1u {
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
