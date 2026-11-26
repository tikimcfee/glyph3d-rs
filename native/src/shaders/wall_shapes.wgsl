// wall_shapes.wgsl — Instanced world-space vector shapes for the wall canvas:
// filled/outlined/dashed/rounded rectangles, solid/dashed line segments,
// circular dots (leader anchors), and triangulated polygon fills.
//
// All colour inputs are sRGB 0..1 RGBA and decoded with exact IEC 61966-2-1
// when targeting an sRGB surface. Output is premultiplied alpha.

const SHAPE_RECT: u32 = 0u;
const SHAPE_LINE: u32 = 1u;
const SHAPE_DOT: u32 = 2u;
const SHAPE_TRI: u32 = 3u;
const OFFSCREEN: vec4<f32> = vec4<f32>(2.0, 2.0, 2.0, 1.0);

struct ShapeFrameUniform {
    view_proj: mat4x4<f32>,
    viewport: vec2<f32>,
    target_is_srgb: u32,
    _pad0: u32,
};

struct ShapeGpu {
    geom: vec4<f32>,
    extra: vec4<f32>,
    fill_color: vec4<f32>,
    stroke_color: vec4<f32>,
    z: f32,
    kind: u32,
    _pad0: u32,
    _pad1: u32,
};

@group(0) @binding(0) var<uniform> frame: ShapeFrameUniform;
@group(0) @binding(1) var<storage, read> shapes: array<ShapeGpu>;

struct ShapeVsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) local_px: vec2<f32>,
    @location(1) @interpolate(flat) size_px: vec2<f32>,
    @location(2) @interpolate(flat) extra: vec4<f32>,
    @location(3) @interpolate(flat) fill_color: vec4<f32>,
    @location(4) @interpolate(flat) stroke_color: vec4<f32>,
    @location(5) @interpolate(flat) kind: u32,
};

fn quad_corner(vi: u32) -> vec2<f32> {
    var corners = array<vec2<f32>, 4>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );
    return corners[vi];
}

fn srgb_channel_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        return c / 12.92;
    }
    return pow((c + 0.055) / 1.055, 2.4);
}

fn srgb_to_output(srgb: vec3<f32>) -> vec3<f32> {
    if frame.target_is_srgb != 0u {
        return vec3<f32>(
            srgb_channel_to_linear(srgb.x),
            srgb_channel_to_linear(srgb.y),
            srgb_channel_to_linear(srgb.z),
        );
    }
    return srgb;
}

fn pulse_train(u: f32, duty: f32, d: f32) -> f32 {
    let a = u - 0.5 * d;
    let b = u + 0.5 * d;
    let fa = floor(a) * duty + min(fract(a), duty);
    let fb = floor(b) * duty + min(fract(b), duty);
    return (fb - fa) / max(d, 1e-6);
}

fn ndc_to_px(ndc_xy: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(
        (ndc_xy.x * 0.5 + 0.5) * frame.viewport.x,
        (0.5 - ndc_xy.y * 0.5) * frame.viewport.y,
    );
}

fn px_to_ndc(px: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(
        px.x / frame.viewport.x * 2.0 - 1.0,
        1.0 - px.y / frame.viewport.y * 2.0,
    );
}

@vertex
fn vs_shape(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> ShapeVsOut {
    var out: ShapeVsOut;
    let s = shapes[ii];
    let c = quad_corner(vi);
    out.extra = s.extra;
    out.fill_color = s.fill_color;
    out.stroke_color = s.stroke_color;
    out.kind = s.kind;

    if s.kind == SHAPE_RECT {
        let x0 = s.geom.x;
        let y0 = s.geom.y;
        let x1 = s.geom.z;
        let y1 = s.geom.w;
        let x = select(x0, x1, c.x > 0.5);
        let y = select(y0, y1, c.y > 0.5);
        let clip = frame.view_proj * vec4<f32>(x, y, s.z, 1.0);
        let c0 = frame.view_proj * vec4<f32>(x0, y0, s.z, 1.0);
        let c1 = frame.view_proj * vec4<f32>(x1, y1, s.z, 1.0);
        let size_px = abs(c1.xy / c1.w - c0.xy / c0.w) * 0.5 * frame.viewport;
        out.clip = clip;
        out.local_px = c * size_px;
        out.size_px = size_px;
        return out;
    }

    if s.kind == SHAPE_LINE {
        let a_clip = frame.view_proj * vec4<f32>(s.geom.x, s.geom.y, s.z, 1.0);
        let b_clip = frame.view_proj * vec4<f32>(s.geom.z, s.geom.w, s.z, 1.0);
        let a_px = ndc_to_px(a_clip.xy / a_clip.w);
        let b_px = ndc_to_px(b_clip.xy / b_clip.w);
        let delta = b_px - a_px;
        let seg_len = length(delta);
        if seg_len < 1e-4 || s.extra.x <= 0.0 {
            out.clip = OFFSCREEN;
            return out;
        }
        let dir = delta / seg_len;
        let perp = vec2<f32>(-dir.y, dir.x);
        let hw = 0.5 * s.extra.x;
        let pad = hw + 1.0;
        let t_px = mix(-pad, seg_len + pad, c.x);
        let n_px = mix(-pad, pad, c.y);
        let pos_px = a_px + dir * t_px + perp * n_px;
        let z_ndc = mix(a_clip.z / a_clip.w, b_clip.z / b_clip.w, clamp(t_px / seg_len, 0.0, 1.0));
        out.clip = vec4<f32>(px_to_ndc(pos_px), z_ndc, 1.0);
        out.local_px = vec2<f32>(t_px, n_px);
        out.size_px = vec2<f32>(seg_len, hw);
        return out;
    }

    if s.kind == SHAPE_DOT {
        let radius_px = s.geom.z;
        if radius_px <= 0.0 {
            out.clip = OFFSCREEN;
            return out;
        }
        let center_clip = frame.view_proj * vec4<f32>(s.geom.x, s.geom.y, s.z, 1.0);
        let center_px = ndc_to_px(center_clip.xy / center_clip.w);
        let ext = radius_px + 1.0;
        let offset_px = vec2<f32>(mix(-ext, ext, c.x), mix(-ext, ext, c.y));
        out.clip = vec4<f32>(px_to_ndc(center_px + offset_px), center_clip.z / center_clip.w, 1.0);
        out.local_px = offset_px;
        out.size_px = vec2<f32>(radius_px, 0.0);
        return out;
    }

    // SHAPE_TRI: quad index 0,1,2 draws the triangle; index 0,2,3 degenerates (v3 == v2).
    var p = vec2<f32>(s.extra.x, s.extra.y);
    if vi == 0u {
        p = vec2<f32>(s.geom.x, s.geom.y);
    } else if vi == 1u {
        p = vec2<f32>(s.geom.z, s.geom.w);
    }
    out.clip = frame.view_proj * vec4<f32>(p, s.z, 1.0);
    out.local_px = vec2<f32>(0.0);
    out.size_px = vec2<f32>(0.0);
    return out;
}

@fragment
fn fs_shape(in: ShapeVsOut) -> @location(0) vec4<f32> {
    if in.kind == SHAPE_RECT {
        let half = 0.5 * in.size_px;
        let p = in.local_px - half;
        let r = clamp(in.extra.w, 0.0, min(half.x, half.y));
        let q = abs(p) - (half - vec2<f32>(r));
        let sdf = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
        let outer = select(1.0, 1.0 - smoothstep(-0.5, 0.5, sdf), r > 0.0);

        let stroke_px = in.extra.x;
        var border = 0.0;
        if stroke_px > 0.0 && in.stroke_color.a > 0.0 {
            let inward = -sdf;
            border = clamp(stroke_px + 0.5 - inward, 0.0, 1.0);
            let dash_px = in.extra.y;
            let gap_px = in.extra.z;
            if dash_px > 0.0 && gap_px > 0.0 {
                let d_top = in.size_px.y - in.local_px.y;
                let d_bot = in.local_px.y;
                let d_left = in.local_px.x;
                let d_right = in.size_px.x - in.local_px.x;
                var s_px = 0.0;
                if d_top <= d_bot && d_top <= d_left && d_top <= d_right {
                    s_px = in.local_px.x;
                } else if d_right <= d_bot && d_right <= d_left {
                    s_px = in.size_px.x + (in.size_px.y - in.local_px.y);
                } else if d_bot <= d_left {
                    s_px = in.size_px.x + in.size_px.y + (in.size_px.x - in.local_px.x);
                } else {
                    s_px = 2.0 * in.size_px.x + in.size_px.y + in.local_px.y;
                }
                let period = dash_px + gap_px;
                border *= pulse_train(s_px / period, dash_px / period, 1.0 / period);
            }
        }

        let fill_a = in.fill_color.a * outer;
        let stroke_a = in.stroke_color.a * border * outer;
        let fill_pre = srgb_to_output(in.fill_color.rgb) * fill_a;
        let stroke_pre = srgb_to_output(in.stroke_color.rgb) * stroke_a;
        let out_pre = stroke_pre + fill_pre * (1.0 - stroke_a);
        let out_a = stroke_a + fill_a * (1.0 - stroke_a);
        if out_a <= 0.0 {
            discard;
        }
        return vec4<f32>(out_pre, out_a);
    }

    if in.kind == SHAPE_LINE {
        let seg_len = in.size_px.x;
        let hw = in.size_px.y;
        let t_px = in.local_px.x;
        let n_px = in.local_px.y;
        let dx = max(max(-t_px, t_px - seg_len), 0.0);
        let d = length(vec2<f32>(dx, n_px));
        var cov = clamp(hw + 0.5 - d, 0.0, 1.0);
        let dash_px = in.extra.y;
        let gap_px = in.extra.z;
        if dash_px > 0.0 && gap_px > 0.0 {
            let period = dash_px + gap_px;
            cov *= pulse_train((t_px + in.extra.w) / period, dash_px / period, 1.0 / period);
        }
        let a = in.stroke_color.a * cov;
        if a <= 0.0 {
            discard;
        }
        return vec4<f32>(srgb_to_output(in.stroke_color.rgb) * a, a);
    }

    if in.kind == SHAPE_DOT {
        let d = length(in.local_px);
        let cov = clamp(in.size_px.x + 0.5 - d, 0.0, 1.0);
        let a = in.fill_color.a * cov;
        if a <= 0.0 {
            discard;
        }
        return vec4<f32>(srgb_to_output(in.fill_color.rgb) * a, a);
    }

    // SHAPE_TRI
    let a = in.fill_color.a;
    if a <= 0.0 {
        discard;
    }
    return vec4<f32>(srgb_to_output(in.fill_color.rgb) * a, a);
}
