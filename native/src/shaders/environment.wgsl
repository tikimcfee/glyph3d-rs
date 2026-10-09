// The ground/sky environment pass (glyph_scene::environment).
//
// One OPAQUE full-screen triangle, drawn FIRST in the glyph field pass, right
// after the clear: every pixel gets either the y = ground_y plane (fill + a
// fixed world-space grid + origin axes, fogged toward the horizon) or the sky
// gradient. The ground writes its true depth, so everything drawn after it
// (backdrops, meshes, glyphs — all GreaterEqual against reverse-Z) sorts
// against it normally; the sky writes 0.0, the cleared far value, so nothing
// is ever hidden by sky. No blending, no discard.
//
// PRECISION: world coordinates reach ~1e4 in repo scenes, where f32 has ~1e-3
// of resolution — too coarse for a ray built as (near point − eye). So every
// position here is CAMERA-RELATIVE: `vp_rel` is proj × view-rotation (the
// view with its translation zeroed) and `inv_vp_rel` its inverse, both built
// on the CPU. The grid's phase comes from `grid_origin` (the eye's xz modulo
// each spacing, reduced in f64 on the CPU), so grid coordinates stay small.
//
// UNIFORMITY: derivatives (fwidth) are computed unconditionally; ground vs
// sky is a select at the end, never a branch around them.
//
// Nothing here moves with the camera except by distance: the grid spacing is
// fixed in world units (config [environment]). Lines narrower than a pixel
// fade out (line_fade_*) instead of shimmering; they never re-space.

struct Env {
    inv_vp_rel: mat4x4<f32>,
    vp_rel: mat4x4<f32>,
    eye: vec4<f32>,          // xyz = eye (world), w = ground_y
    grid_origin: vec4<f32>,  // xy = eye.xz mod minor spacing, zw = eye.xz mod major spacing
    spacing: vec4<f32>,      // x = minor, y = major (world), z = line width px, w = axis width px
    fade: vec4<f32>,         // x = fog start, y = fog end (world dist), z = line fade start px, w = line fade end px
    ground: vec4<f32>,       // rgb
    minor_line: vec4<f32>,   // rgb, a = strength
    major_line: vec4<f32>,
    axis_x: vec4<f32>,       // the z = 0 line, running along x
    axis_z: vec4<f32>,       // the x = 0 line, running along z
    sky_horizon: vec4<f32>,  // rgb, w = gradient height (in sin(elevation))
    sky_zenith: vec4<f32>,
};

@group(0) @binding(0) var<uniform> env: Env;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) ndc: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    // Full-screen triangle: (-1,-1), (3,-1), (-1,3).
    let uv = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    let p = uv * 2.0 - 1.0;
    var out: VsOut;
    out.pos = vec4<f32>(p, 0.0, 1.0);
    out.ndc = p;
    return out;
}

struct FsOut {
    @location(0) color: vec4<f32>,
    @builtin(frag_depth) depth: f32,
};

// Antialiased line coverage of the integer lines of `g` (grid units), for a
// line `width_px` wide, and the on-screen size of one cell in px.
fn grid_lines(g: vec2<f32>, width_px: f32) -> vec2<f32> {
    let fw = max(fwidth(g), vec2<f32>(1e-6));
    let d = abs(fract(g - 0.5) - 0.5) / fw; // px to the nearest line, per axis
    let coverage = 1.0 - clamp(min(d.x, d.y) - (width_px * 0.5 - 0.5), 0.0, 1.0);
    let cell_px = 1.0 / max(fw.x, fw.y);
    return vec2<f32>(coverage, cell_px);
}

fn axis_line(v: f32, width_px: f32) -> f32 {
    let d = abs(v) / max(fwidth(v), 1e-6);
    return 1.0 - clamp(d - (width_px * 0.5 - 0.5), 0.0, 1.0);
}

@fragment
fn fs_main(in: VsOut) -> FsOut {
    // Ray direction, camera-relative. NDC z = 1 is the NEAR plane (reverse-Z).
    let h = env.inv_vp_rel * vec4<f32>(in.ndc, 1.0, 1.0);
    let dir = normalize(h.xyz / h.w);

    // Intersect y = ground_y, only from above and only looking down at it.
    let height = env.eye.y - env.eye.w;
    let down = -dir.y;
    let hit = height > 0.0 && down > 1e-6;
    // Clamp past the fog so the far field stays finite (it is fully fogged).
    let t = select(0.0, min(height / max(down, 1e-6), env.fade.y * 2.0), hit);
    let rel = dir * t;

    // Fixed world-space grid; phase from the CPU-reduced eye offsets.
    let minor = grid_lines((env.grid_origin.xy + rel.xz) / env.spacing.x, env.spacing.z);
    let major = grid_lines((env.grid_origin.zw + rel.xz) / env.spacing.y, env.spacing.z);
    let lf0 = env.fade.w;
    let lf1 = env.fade.z;
    let minor_a = minor.x * smoothstep(lf0, lf1, minor.y) * env.minor_line.a;
    let major_a = major.x * smoothstep(lf0, lf1, major.y) * env.major_line.a;

    let world_xz = env.eye.xz + rel.xz;
    let ax = axis_line(world_xz.y, env.spacing.w) * env.axis_x.a; // z = 0
    let az = axis_line(world_xz.x, env.spacing.w) * env.axis_z.a; // x = 0

    var ground = env.ground.rgb;
    ground = mix(ground, env.minor_line.rgb, minor_a);
    ground = mix(ground, env.major_line.rgb, major_a);
    ground = mix(ground, env.axis_x.rgb, ax);
    ground = mix(ground, env.axis_z.rgb, az);

    let fog = smoothstep(env.fade.x, env.fade.y, t);
    ground = mix(ground, env.sky_horizon.rgb, fog);

    let elevation = smoothstep(0.0, max(env.sky_horizon.w, 1e-6), max(dir.y, 0.0));
    let sky = mix(env.sky_horizon.rgb, env.sky_zenith.rgb, elevation);

    let clip = env.vp_rel * vec4<f32>(rel, 1.0);
    let ground_depth = clamp(clip.z / clip.w, 0.0, 1.0);

    var out: FsOut;
    out.color = vec4<f32>(select(sky, ground, hit), 1.0);
    out.depth = select(0.0, ground_depth, hit);
    return out;
}
