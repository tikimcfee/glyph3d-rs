// wall_paper.wgsl — Full-screen drafting-paper grid pass (port of wall/WallPaper.js).
//
// Drawn first in the wall render pass with depth test/write off. Discrete minor
// and major grid levels are decided on the CPU via wall_core::snap::grid_levels:
// a spacing of 0 disables that level completely (hard switch, never a fade).
// A device pixel lies on a line when a multiple of the spacing falls inside its
// half-open footprint [u - h, u + h], so every line is exact-width and crisp in
// its specified sRGB colour.

struct WallPaperUniform {
    origin: vec2<f32>,
    half_buf: vec2<f32>,
    zoom: f32,
    half_line: f32,
    minor_step: f32,
    major_step: f32,
    minor_color: vec4<f32>,
    major_color: vec4<f32>,
    target_is_srgb: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
};

@group(0) @binding(0) var<uniform> paper: WallPaperUniform;

fn srgb_channel_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        return c / 12.92;
    }
    return pow((c + 0.055) / 1.055, 2.4);
}

fn srgb_to_output(srgb: vec3<f32>) -> vec3<f32> {
    if paper.target_is_srgb != 0u {
        return vec3<f32>(
            srgb_channel_to_linear(srgb.x),
            srgb_channel_to_linear(srgb.y),
            srgb_channel_to_linear(srgb.z),
        );
    }
    return srgb;
}

fn on_line(u: vec2<f32>, h: f32, step: f32) -> bool {
    if step <= 0.0 {
        return false;
    }
    let a = floor((u - vec2<f32>(h)) / step);
    let b = floor((u + vec2<f32>(h)) / step);
    return a.x != b.x || a.y != b.y;
}

@vertex
fn vs_paper(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    return vec4<f32>(pos[vi], 0.0, 1.0);
}

@fragment
fn fs_paper(@builtin(position) frag_coord: vec4<f32>) -> @location(0) vec4<f32> {
    // WebGPU frag_coord has origin at top-left (+Y down); wall world space is +Y up.
    let dx = (frag_coord.x - paper.half_buf.x) / paper.zoom;
    let dy = (paper.half_buf.y - frag_coord.y) / paper.zoom;
    let u = paper.origin + vec2<f32>(dx, dy);
    let h = paper.half_line / paper.zoom;

    var col: vec4<f32>;
    if on_line(u, h, paper.major_step) {
        col = paper.major_color;
    } else if on_line(u, h, paper.minor_step) {
        col = paper.minor_color;
    } else {
        discard;
    }

    let rgb = srgb_to_output(col.rgb);
    return vec4<f32>(rgb * col.a, col.a);
}
