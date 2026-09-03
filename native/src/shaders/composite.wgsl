// Stage L (L3): fullscreen composite — sample the pooled view target
// (Rgba8UnormSrgb) into the driver's framebuffer with a single fullscreen
// triangle. WINDOWED path only: the windowed surface is Bgra8UnormSrgb,
// component-order-incompatible with the pool, so copy_texture_to_texture
// is invalid there. The offscreen oracle target IS Rgba8UnormSrgb and uses
// the copy instead (bit-exact by construction) — see ViewTarget in
// glyph_scene.rs.
//
// Content is premultiplied-alpha and the pipeline has blending DISABLED:
// the composite is an exact-overwrite passthrough, not a blend.
// sRGB note: sampling an sRGB texture decodes to linear and writing an
// sRGB target re-encodes; the round trip may differ by ≤1 LSB — invisible,
// and this path is not PNG-gated (the oracle path is the copy).

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    // Fullscreen triangle: uv (0,0), (2,0), (0,2) → NDC positions
    // (-1,1), (3,1), (-1,-3). uv (0,0) lands on NDC (-1,+1) = framebuffer
    // top-left, sampling texture texel row 0 (wgpu texture v-axis points
    // down, matching framebuffer rows) — a 1:1 mapping over the screen.
    var out: VsOut;
    out.uv = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    out.pos = vec4<f32>(out.uv.x * 2.0 - 1.0, 1.0 - out.uv.y * 2.0, 0.0, 1.0);
    return out;
}

@group(0) @binding(0) var pool_tex: texture_2d<f32>;
@group(0) @binding(1) var pool_sampler: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(pool_tex, pool_sampler, in.uv);
}
