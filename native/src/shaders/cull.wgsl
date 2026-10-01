// Stage F — far-LOD backdrop pipeline (flat tinted quads).
//
// THE CULL/LOD CONTRACT (full story in glyph_scene.rs's module header)
// --------------------------------------------------------------------
// Per frame, the CPU (`cull_segments` in glyph_scene.rs) walks the segment
// table (one SegCull per file in repo mode) and classifies each segment:
//
//   1. FRUSTUM: 6-plane positive-vertex AABB test. Outside → draws nothing.
//      Lossless by construction (the AABB conservatively contains every
//      instance, with a z margin of ±1).
//
//   2. LOD: glyph_px = px_scale / dist, px_scale = viewport_h /
//      (2·tan(fov/2)), dist = distance from the eye to the AABB's NEAREST
//      point (conservative: a segment only drops to backdrop when even its
//      closest glyphs are below the threshold). glyph_px < lod_min_px
//      (1.0 px/em) → the segment's glyphs are raster-lottery subpixel blobs
//      and are REPLACED by one backdrop quad from THIS file: the segment
//      rect tinted with the mean linear ink color at alpha = effective ink
//      coverage E, both baked at staging into SegCull.tint. At ≥1 px/em,
//      glyphs are always drawn — legible text is never substituted.
//
//   3. DRAW STREAMS: glyph survivors render as per-segment vertex-range
//      draws (ascending arena order per chunk, so blend order matches the
//      legacy full draws); backdrop survivors are compacted CPU-side into
//      the instance buffer below and drawn with ONE instanced draw.
//
// The original Stage F design ran this cull as a GPU compute pass feeding
// indirect multi-draws. wgpu 30.0.1's Metal backend silently rasterizes
// NOTHING for any indirect draw with first_instance != 0 (verified via
// readback: args were correct), so the draws are CPU-issued. The backdrop
// shader side is unchanged by that decision.

// 48 B — mirrors BackdropInst in glyph_scene/cull.rs.
struct BackdropInst {
    min_xy: vec2<f32>,
    max_xy: vec2<f32>,
    rgba: vec4<f32>, // rgb linear, a = coverage E
    z: f32,
    _pad: vec3<f32>,
};

struct Camera {
    view_proj: mat4x4<f32>,
};

@group(0) @binding(0) var<uniform> camera: Camera;
@group(0) @binding(1) var<storage, read> binsts: array<BackdropInst>;

struct BackdropVsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) @interpolate(flat) rgba: vec4<f32>,
};

@vertex
fn vs_backdrop(
    @builtin(vertex_index) vi: u32,
    @builtin(instance_index) ii: u32,
) -> BackdropVsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );
    let b = binsts[ii];
    let c = corners[vi];
    // y-up world: c.y = 0 is the rect's bottom (min_y).
    let p = vec2<f32>(
        mix(b.min_xy.x, b.max_xy.x, c.x),
        mix(b.min_xy.y, b.max_xy.y, c.y),
    );
    var out: BackdropVsOut;
    out.clip = camera.view_proj * vec4<f32>(p, b.z, 1.0);
    out.rgba = b.rgba;
    return out;
}

@fragment
fn fs_backdrop(in: BackdropVsOut) -> @location(0) vec4<f32> {
    // Premultiplied: mean linear ink color attenuated by the coverage E.
    return vec4<f32>(in.rgba.rgb * in.rgba.a, in.rgba.a);
}
