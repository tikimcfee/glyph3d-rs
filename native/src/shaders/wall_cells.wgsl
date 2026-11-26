// wall_cells.wgsl — Terminal wall cell renderer (panel + border, fills, Slug ink,
// colour emoji, style lines, greek tier, and cursor).
//
// Drawn per terminal from its compact 16 B CellGpu buffer. Cell positions are
// derived in the vertex stage from instance_index (col = ii % cols, row = ii / cols),
// using snapped cell dimensions and exact corner selection so adjacent cells share
// edges bit-identically with zero cracks and zero overlap.
//
// Colour contract (review-carbon Risk 4):
//   - All colour inputs (cell fg/bg/ul, panel default_bg, border, cursor) are sRGB.
//   - Decoded with the exact IEC 61966-2-1 EOTF (not pow(2.2)) when rendering to an
//     sRGB target so dark terminal palettes round-trip with 0-LSB crush.
//   - Slug non-zero winding uses abs(coverage) so reverse-wound contours and
//     overlapping variable-font strokes fill cleanly.
//   - Analytic pixel-footprint Jacobian (glyph-field footprint.rs) replaces fwidth
//     everywhere so Apple TBDR GPUs never flicker and emoji sampling uses explicit
//     gradients cleanly inside mode branches.

const TEX_W: u32 = 1024u;
const MAX_CURVES: u32 = 512u;
const NO_CELL: u32 = 0xFFFFFFFFu;
const OFFSCREEN: vec4<f32> = vec4<f32>(2.0, 2.0, 2.0, 1.0);
const TAU: f32 = 6.28318530718;

const GREEK_BAR_LO: f32 = 0.27;
const GREEK_BAR_HI: f32 = 0.47;

const STYLE_DIM: u32 = 1u << 0u;
const STYLE_STRIKE: u32 = 1u << 1u;
const STYLE_OVERLINE: u32 = 1u << 2u;
const STYLE_UNDERLINE_SHIFT: u32 = 3u;
const STYLE_UNDERLINE_MASK: u32 = 7u << STYLE_UNDERLINE_SHIFT;
const STYLE_UNDERLINE_COLOR: u32 = 1u << 6u;
const STYLE_LINES: u32 = STYLE_STRIKE | STYLE_OVERLINE | STYLE_UNDERLINE_MASK;

const UNDERLINE_SINGLE: u32 = 1u;
const UNDERLINE_DOUBLE: u32 = 2u;
const UNDERLINE_CURLY: u32 = 3u;
const UNDERLINE_DOTTED: u32 = 4u;
const UNDERLINE_DASHED: u32 = 5u;

// Sub-layer Z offsets within one terminal (larger z = nearer in reverse-Z).
const Z_OFFSET_PANEL: f32 = -0.002;
const Z_OFFSET_FILL: f32 = -0.001;
const Z_OFFSET_INK: f32 = 0.0;
const Z_OFFSET_CURSOR: f32 = 0.001;

struct WallFrameUniform {
    view_proj: mat4x4<f32>,
    viewport: vec2<f32>,
    cell_size: vec2<f32>,
    line_a: vec4<f32>,
    line_b: vec4<f32>,
    border_focused_color: vec4<f32>,
    border_unfocused_color: vec4<f32>,
    cursor_color: vec4<f32>,
    emoji_cell: vec2<u32>,
    emoji_cols: u32,
    emoji_rows: u32,
    emoji_layer: vec2<f32>,
    greek_px: f32,
    dim_alpha: f32,
    panel_padding: f32,
    panel_border_px: f32,
    border_focused_scale: f32,
    cursor_fill_alpha: f32,
    cursor_border_px: f32,
    target_is_srgb: u32,
    _pad0: u32,
    _pad1: u32,
};

struct TerminalUniform {
    origin: vec2<f32>,
    z: f32,
    focused: u32,
    cols: u32,
    rows: u32,
    default_bg: u32,
    panel_opacity: f32,
    cursor_col: u32,
    cursor_row: u32,
    cursor_flags: u32,
    border_override: u32,
};

struct CellGpu {
    glyph_and_flags: u32,
    fg: u32,
    bg: u32,
    ul: u32,
};

struct LabelGlyphGpu {
    rect: vec4<f32>,
    uv_clip: vec4<f32>,
    z: f32,
    glyph_id: u32,
    fg_rgba: u32,
    span_cols: u32,
};

@group(0) @binding(0) var<uniform> frame: WallFrameUniform;
@group(0) @binding(1) var glyphmap: texture_2d<u32>;
@group(0) @binding(2) var curves: texture_2d<u32>;
@group(0) @binding(3) var emoji_tex: texture_2d_array<f32>;
@group(0) @binding(4) var emoji_samp: sampler;

@group(1) @binding(0) var<uniform> term: TerminalUniform;
@group(1) @binding(1) var<storage, read> cells: array<CellGpu>;
@group(1) @binding(2) var<storage, read> label_glyphs: array<LabelGlyphGpu>;

fn quad_corner(vi: u32) -> vec2<f32> {
    var corners = array<vec2<f32>, 4>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );
    return corners[vi];
}

fn unpack_rgba8(packed: u32) -> vec4<f32> {
    return vec4<f32>(
        f32(packed & 0xFFu),
        f32((packed >> 8u) & 0xFFu),
        f32((packed >> 16u) & 0xFFu),
        f32((packed >> 24u) & 0xFFu),
    ) / 255.0;
}

fn srgb_channel_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        return c / 12.92;
    }
    return pow((c + 0.055) / 1.055, 2.4);
}

fn linear_channel_to_srgb(l: f32) -> f32 {
    let clamped = max(l, 0.0);
    if clamped <= 0.0031308 {
        return clamped * 12.92;
    }
    return 1.055 * pow(clamped, 1.0 / 2.4) - 0.055;
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

fn linear_to_output(lin: vec3<f32>) -> vec3<f32> {
    if frame.target_is_srgb != 0u {
        return lin;
    }
    return vec3<f32>(
        linear_channel_to_srgb(lin.x),
        linear_channel_to_srgb(lin.y),
        linear_channel_to_srgb(lin.z),
    );
}

fn rot90(v: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(v.y, -v.x);
}

// Analytic quadratic Bezier coverage along the +X ray over pixel window [lo, lo + 1/inv_w].
fn compute_coverage(lo: f32, inv_w: f32, p0: vec2<f32>, p1: vec2<f32>, p2: vec2<f32>) -> f32 {
    let all_above = p0.y > 0.0 && p1.y > 0.0 && p2.y > 0.0;
    let all_below = p0.y < 0.0 && p1.y < 0.0 && p2.y < 0.0;
    if all_above || all_below {
        return 0.0;
    }

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

    var result = 0.0;
    if solvable {
        if t0 >= 0.0 && t0 < 1.0 {
            let x = (a.x * t0 - 2.0 * b.x) * t0 + c.x;
            result += clamp((x - lo) * inv_w, 0.0, 1.0);
        }
        if t1 >= 0.0 && t1 < 1.0 {
            let x = (a.x * t1 - 2.0 * b.x) * t1 + c.x;
            result -= clamp((x - lo) * inv_w, 0.0, 1.0);
        }
    }
    return result;
}

// How much of pixel window [lo, lo + 1/inv] the interval [a, b] covers.
fn band_coverage(a: f32, b: f32, lo: f32, inv: f32) -> f32 {
    return clamp((b - lo) * inv, 0.0, 1.0) - clamp((a - lo) * inv, 0.0, 1.0);
}

// A pulse train (on while fract(u) < duty), box-filtered over a window of width d centred on u.
fn pulse_train(u: f32, duty: f32, d: f32) -> f32 {
    let a = u - 0.5 * d;
    let b = u + 0.5 * d;
    let fa = floor(a) * duty + min(fract(a), duty);
    let fb = floor(b) * duty + min(fract(b), duty);
    return (fb - fa) / max(d, 1e-6);
}

// Analytic Jacobian of screen pixels with respect to quad UV (works under both
// orthographic and perspective projections, immune to Apple TBDR fwidth flicker).
fn foot_jacobian(p0: vec3<f32>, pu: vec3<f32>, pv: vec3<f32>, uv: vec2<f32>) -> mat2x2<f32> {
    let p = p0 + uv.x * pu + uv.y * pv;
    let w2 = p.z * p.z;
    let xu = (pu.x * p.z - p.x * pu.z) / w2;
    let xv = (pv.x * p.z - p.x * pv.z) / w2;
    let yu = (pu.y * p.z - p.y * pu.z) / w2;
    let yv = (pv.y * p.z - p.y * pv.z) / w2;
    let det = xu * yv - xv * yu;
    let inv = 1.0 / select(det, 1e-30, abs(det) < 1e-30);
    return mat2x2<f32>(vec2<f32>(yv, -yu) * inv, vec2<f32>(-xv, xu) * inv);
}

// ── Pass 1: Background panel + screen-pixel border ──────────────────────────

struct PanelVsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) fill_color: vec4<f32>,
    @location(2) @interpolate(flat) border_color: vec4<f32>,
    @location(3) @interpolate(flat) border_width_px: f32,
    @location(4) @interpolate(flat) foot_p0: vec3<f32>,
    @location(5) @interpolate(flat) foot_pu: vec3<f32>,
    @location(6) @interpolate(flat) foot_pv: vec3<f32>,
};

@vertex
fn vs_panel(@builtin(vertex_index) vi: u32) -> PanelVsOut {
    let c = quad_corner(vi);
    let pad = frame.panel_padding;
    let grid_w = f32(term.cols) * frame.cell_size.x;
    let grid_h = f32(term.rows) * frame.cell_size.y;
    let x0 = term.origin.x - pad;
    let x1 = term.origin.x + grid_w + pad;
    let y0 = term.origin.y - grid_h - pad;
    let y1 = term.origin.y + pad;
    let x = select(x0, x1, c.x > 0.5);
    let y = select(y0, y1, c.y > 0.5);
    let z = term.z + Z_OFFSET_PANEL;

    let clip = frame.view_proj * vec4<f32>(x, y, z, 1.0);
    let foot_cu = frame.view_proj * vec4<f32>(x1 - x0, 0.0, 0.0, 0.0);
    let foot_cv = frame.view_proj * vec4<f32>(0.0, y1 - y0, 0.0, 0.0);
    let foot_c0 = clip - c.x * foot_cu - c.y * foot_cv;
    let foot_scale = vec3<f32>(frame.viewport.x * 0.5, -frame.viewport.y * 0.5, 1.0);

    let bg = unpack_rgba8(term.default_bg);
    let is_focused = term.focused != 0u;
    var border_col = select(frame.border_unfocused_color, frame.border_focused_color, is_focused);
    if term.border_override != 0u {
        border_col = unpack_rgba8(term.border_override);
    }
    let border_w = frame.panel_border_px * select(1.0, frame.border_focused_scale, is_focused);

    var out: PanelVsOut;
    out.clip = clip;
    out.uv = c;
    out.fill_color = vec4<f32>(bg.rgb, term.panel_opacity);
    out.border_color = border_col;
    out.border_width_px = border_w;
    out.foot_p0 = foot_c0.xyw * foot_scale;
    out.foot_pu = foot_cu.xyw * foot_scale;
    out.foot_pv = foot_cv.xyw * foot_scale;
    return out;
}

@fragment
fn fs_panel(in: PanelVsOut) -> @location(0) vec4<f32> {
    let j = foot_jacobian(in.foot_p0, in.foot_pu, in.foot_pv, in.uv);
    let fw = max(abs(j[0]) + abs(j[1]), vec2<f32>(1e-6));
    let edge = vec2<f32>(0.5) - abs(in.uv - vec2<f32>(0.5));
    let edge_px = edge / fw;
    let dist_px = min(edge_px.x, edge_px.y);
    let w = in.border_width_px;
    let band = select(0.0, 1.0 - smoothstep(w - 0.5, w + 0.5, dist_px), w > 0.0);
    let rim = band * in.border_color.a;

    let fill_rgb = srgb_to_output(in.fill_color.rgb);
    let border_rgb = srgb_to_output(in.border_color.rgb);
    let final_rgb = mix(fill_rgb, border_rgb, rim);
    let final_alpha = max(in.fill_color.a, rim);
    return vec4<f32>(final_rgb * final_alpha, final_alpha);
}

// ── Pass 2: Per-cell background fills ───────────────────────────────────────

struct FillVsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) @interpolate(flat) fill_color: vec4<f32>,
};

@vertex
fn vs_fill(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> FillVsOut {
    var out: FillVsOut;
    let cell = cells[ii];
    let bg = unpack_rgba8(cell.bg);
    if bg.a <= 0.0 {
        out.clip = OFFSCREEN;
        out.fill_color = vec4<f32>(0.0);
        return out;
    }
    let col = ii % term.cols;
    let row = ii / term.cols;
    let c = quad_corner(vi);
    let x = select(
        term.origin.x + f32(col) * frame.cell_size.x,
        term.origin.x + f32(col + 1u) * frame.cell_size.x,
        c.x > 0.5,
    );
    let y = select(
        term.origin.y - f32(row + 1u) * frame.cell_size.y,
        term.origin.y - f32(row) * frame.cell_size.y,
        c.y > 0.5,
    );
    let z = term.z + Z_OFFSET_FILL;
    out.clip = frame.view_proj * vec4<f32>(x, y, z, 1.0);
    out.fill_color = bg;
    return out;
}

@fragment
fn fs_fill(in: FillVsOut) -> @location(0) vec4<f32> {
    let rgb = srgb_to_output(in.fill_color.rgb);
    return vec4<f32>(rgb * in.fill_color.a, in.fill_color.a);
}

// ── Pass 3: Cell ink (Slug outlines, sprites, colour emoji) + style lines ───

struct InkVsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) glyph_uv: vec2<f32>,
    @location(1) @interpolate(flat) fg_srgb: vec4<f32>,
    @location(2) @interpolate(flat) decor_srgb: vec3<f32>,
    @location(3) @interpolate(flat) curve_start: u32,
    @location(4) @interpolate(flat) curve_count: u32,
    @location(5) @interpolate(flat) mode: u32,
    @location(6) @interpolate(flat) glyph_arg: u32,
    @location(7) @interpolate(flat) style: u32,
    // x: quad_w / cell_w, y: cell_w / cell_h (cell aspect), z: span_cols, w: quad_w / cell_h (emoji aspect)
    @location(8) @interpolate(flat) cell_info: vec4<f32>,
    @location(9) @interpolate(flat) emoji_cell_xy: vec2<f32>,
    @location(10) @interpolate(flat) emoji_layer: u32,
    @location(11) @interpolate(flat) foot_p0: vec3<f32>,
    @location(12) @interpolate(flat) foot_pu: vec3<f32>,
    @location(13) @interpolate(flat) foot_pv: vec3<f32>,
};

@vertex
fn vs_ink(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> InkVsOut {
    var out: InkVsOut;
    let cell = cells[ii];
    let glyph_id = cell.glyph_and_flags & 0xFFFFu;
    let style = (cell.glyph_and_flags >> 16u) & 0x7Fu;
    let span_cols = (cell.glyph_and_flags >> 24u) & 0x3u;

    if span_cols == 0u {
        out.clip = OFFSCREEN;
        return out;
    }

    let has_lines = (style & STYLE_LINES) != 0u;
    if glyph_id == 0u && !has_lines {
        out.clip = OFFSCREEN;
        return out;
    }

    var curve_start = 0u;
    var curve_count = 0u;
    var mode = 2u; // blank
    var glyph_arg = 0u;
    if glyph_id != 0u {
        let info = textureLoad(glyphmap, vec2<i32>(i32(glyph_id % TEX_W), i32(glyph_id / TEX_W)), 0);
        curve_start = info.x;
        curve_count = info.y;
        mode = info.z;
        glyph_arg = info.w;
        if mode == 1u && glyph_arg == NO_CELL {
            mode = 2u;
        }
    }

    let has_ink = mode == 1u || (mode != 2u && curve_count > 0u);
    if !has_ink && !has_lines {
        out.clip = OFFSCREEN;
        return out;
    }

    let col = ii % term.cols;
    let row = ii / term.cols;
    let c = quad_corner(vi);
    let span_w = f32(span_cols) * frame.cell_size.x;
    let quad_w = select(span_w, max(span_w, frame.cell_size.y), mode == 1u);

    let x0 = term.origin.x + f32(col) * frame.cell_size.x;
    let exact_span_right = term.origin.x + f32(col + span_cols) * frame.cell_size.x;
    let x1 = select(x0 + quad_w, exact_span_right, quad_w == span_w);
    let y0 = term.origin.y - f32(row + 1u) * frame.cell_size.y;
    let y1 = term.origin.y - f32(row) * frame.cell_size.y;
    let x = select(x0, x1, c.x > 0.5);
    let y = select(y0, y1, c.y > 0.5);
    let z = term.z + Z_OFFSET_INK;

    let clip = frame.view_proj * vec4<f32>(x, y, z, 1.0);
    let foot_cu = frame.view_proj * vec4<f32>(x1 - x0, 0.0, 0.0, 0.0);
    let foot_cv = frame.view_proj * vec4<f32>(0.0, y1 - y0, 0.0, 0.0);
    let foot_c0 = clip - c.x * foot_cu - c.y * foot_cv;
    let foot_scale = vec3<f32>(frame.viewport.x * 0.5, -frame.viewport.y * 0.5, 1.0);

    var emoji_cell_xy = vec2<f32>(0.0);
    var emoji_layer = 0u;
    if mode == 1u {
        let per_layer = frame.emoji_cols * frame.emoji_rows;
        emoji_layer = glyph_arg / per_layer;
        let rem = glyph_arg % per_layer;
        emoji_cell_xy = vec2<f32>(f32(rem % frame.emoji_cols), f32(rem / frame.emoji_cols))
            * vec2<f32>(frame.emoji_cell);
    }

    let fg_srgb = vec4<f32>(unpack_rgba8(cell.fg).rgb, 1.0);
    let decor_srgb = select(fg_srgb.rgb, unpack_rgba8(cell.ul).rgb, (style & STYLE_UNDERLINE_COLOR) != 0u);

    out.clip = clip;
    out.glyph_uv = c;
    out.fg_srgb = fg_srgb;
    out.decor_srgb = decor_srgb;
    out.curve_start = curve_start;
    out.curve_count = curve_count;
    out.mode = mode;
    out.glyph_arg = glyph_arg;
    out.style = style;
    out.cell_info = vec4<f32>(
        quad_w / max(frame.cell_size.x, 1e-6),
        frame.cell_size.x / max(frame.cell_size.y, 1e-6),
        f32(span_cols),
        quad_w / max(frame.cell_size.y, 1e-6),
    );
    out.emoji_cell_xy = emoji_cell_xy;
    out.emoji_layer = emoji_layer;
    out.foot_p0 = foot_c0.xyw * foot_scale;
    out.foot_pu = foot_cu.xyw * foot_scale;
    out.foot_pv = foot_cv.xyw * foot_scale;
    return out;
}

// Non-terminal text labels (title bars, group headers, map labels) routed through
// the exact same Slug/emoji/greek fragment shader (fs_ink).
@vertex
fn vs_label(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> InkVsOut {
    var out: InkVsOut;
    let g = label_glyphs[ii];
    if g.glyph_id == 0u {
        out.clip = OFFSCREEN;
        return out;
    }

    let info = textureLoad(
        glyphmap,
        vec2<i32>(i32(g.glyph_id % TEX_W), i32(g.glyph_id / TEX_W)),
        0,
    );
    let curve_start = info.x;
    let curve_count = info.y;
    var mode = info.z;
    let glyph_arg = info.w;
    if mode == 1u && glyph_arg == NO_CELL {
        mode = 2u;
    }
    let has_ink = mode == 1u || (mode != 2u && curve_count > 0u);
    if !has_ink {
        out.clip = OFFSCREEN;
        return out;
    }

    let c = quad_corner(vi);
    let uv = vec2<f32>(
        mix(g.uv_clip.x, g.uv_clip.z, c.x),
        mix(g.uv_clip.y, g.uv_clip.w, c.y),
    );
    let x0 = g.rect.x;
    let y0 = g.rect.y;
    let x1 = g.rect.z;
    let y1 = g.rect.w;
    let x = mix(x0, x1, uv.x);
    let y = mix(y0, y1, uv.y);

    let clip = frame.view_proj * vec4<f32>(x, y, g.z, 1.0);
    let foot_cu = frame.view_proj * vec4<f32>(x1 - x0, 0.0, 0.0, 0.0);
    let foot_cv = frame.view_proj * vec4<f32>(0.0, y1 - y0, 0.0, 0.0);
    let foot_c0 = clip - uv.x * foot_cu - uv.y * foot_cv;
    let foot_scale = vec3<f32>(frame.viewport.x * 0.5, -frame.viewport.y * 0.5, 1.0);

    var emoji_cell_xy = vec2<f32>(0.0);
    var emoji_layer = 0u;
    if mode == 1u {
        let per_layer = frame.emoji_cols * frame.emoji_rows;
        emoji_layer = glyph_arg / per_layer;
        let rem = glyph_arg % per_layer;
        emoji_cell_xy = vec2<f32>(f32(rem % frame.emoji_cols), f32(rem / frame.emoji_cols))
            * vec2<f32>(frame.emoji_cell);
    }

    let quad_w = max(x1 - x0, 1e-6);
    let quad_h = max(y1 - y0, 1e-6);
    let fg_srgb = unpack_rgba8(g.fg_rgba);

    out.clip = clip;
    out.glyph_uv = uv;
    out.fg_srgb = fg_srgb;
    out.decor_srgb = fg_srgb.rgb;
    out.curve_start = curve_start;
    out.curve_count = curve_count;
    out.mode = mode;
    out.glyph_arg = glyph_arg;
    out.style = 0u;
    out.cell_info = vec4<f32>(
        f32(max(g.span_cols, 1u)),
        quad_w / (f32(max(g.span_cols, 1u)) * quad_h),
        f32(max(g.span_cols, 1u)),
        quad_w / quad_h,
    );
    out.emoji_cell_xy = emoji_cell_xy;
    out.emoji_layer = emoji_layer;
    out.foot_p0 = foot_c0.xyw * foot_scale;
    out.foot_pu = foot_cu.xyw * foot_scale;
    out.foot_pv = foot_cv.xyw * foot_scale;
    return out;
}

@fragment
fn fs_ink(in: InkVsOut) -> @location(0) vec4<f32> {
    let j = foot_jacobian(in.foot_p0, in.foot_pu, in.foot_pv, in.glyph_uv);
    let fw = max(abs(j[0]) + abs(j[1]), vec2<f32>(1e-6));

    let fg = srgb_to_output(in.fg_srgb.rgb);
    var ink = fg;
    var cov = 0.0;

    let greek = frame.greek_px > 0.0 && (1.0 / fw.y) < frame.greek_px;

    if in.mode == 1u {
        // Bitmap colour emoji, centred as a square inside the cell quad so 2-cell
        // emoji stay aspect-true while lines still span both columns.
        let aspect_x = in.cell_info.w;
        let local_x = in.glyph_uv.x * aspect_x - 0.5 * (aspect_x - 1.0);
        if local_x >= 0.0 && local_x <= 1.0 {
            let inset = vec2<f32>(0.5);
            let span = vec2<f32>(frame.emoji_cell) - vec2<f32>(1.0);
            let emoji_uv = (in.emoji_cell_xy + inset + vec2<f32>(local_x, 1.0 - in.glyph_uv.y) * span)
                / frame.emoji_layer;
            let grad_scale = (span / frame.emoji_layer) * vec2<f32>(aspect_x, -1.0);
            let texel = textureSampleGrad(
                emoji_tex,
                emoji_samp,
                emoji_uv,
                in.emoji_layer,
                j[0] * grad_scale,
                j[1] * grad_scale,
            );
            ink = linear_to_output(texel.rgb);
            cov = texel.a;
        }
    } else if in.mode != 2u && in.curve_count > 0u {
        let is_sprite = in.mode == 3u;
        if greek && !is_sprite {
            let y_cov = clamp((in.glyph_uv.y - GREEK_BAR_LO) / fw.y + 0.5, 0.0, 1.0)
                - clamp((in.glyph_uv.y - GREEK_BAR_HI) / fw.y + 0.5, 0.0, 1.0);
            let x_cov = clamp(in.glyph_uv.x / fw.x + 0.5, 0.0, 1.0)
                - clamp((in.glyph_uv.x - 1.0) / fw.x + 0.5, 0.0, 1.0);
            cov = clamp(x_cov * y_cov, 0.0, 1.0);
        } else {
            var at = in.glyph_uv;
            var lo = at - 0.5 * fw;
            var hi = at + 0.5 * fw;
            if is_sprite {
                lo = max(lo, vec2<f32>(0.0));
                hi = min(hi, vec2<f32>(1.0));
                at = clamp(at, vec2<f32>(1e-4), vec2<f32>(1.0 - 1e-4));
            }
            let inv_w = vec2<f32>(1.0) / max(hi - lo, vec2<f32>(1e-6));
            lo = lo - at;

            var coverage = 0.0;
            let n = min(in.curve_count, MAX_CURVES);
            for (var i = 0u; i < n; i = i + 1u) {
                let ci = (in.curve_start + i) * 2u;
                let t0 = textureLoad(curves, vec2<i32>(i32(ci % TEX_W), i32(ci / TEX_W)), 0);
                let t1 = textureLoad(curves, vec2<i32>(i32((ci + 1u) % TEX_W), i32((ci + 1u) / TEX_W)), 0);

                let p0 = vec2<f32>(f32(t0.x), f32(t0.y)) / 65535.0 - at;
                let p1 = vec2<f32>(f32(t0.z), f32(t0.w)) / 65535.0 - at;
                let p2 = vec2<f32>(f32(t1.x), f32(t1.y)) / 65535.0 - at;

                coverage += compute_coverage(lo.x, inv_w.x, p0, p1, p2);
                coverage += compute_coverage(lo.y, inv_w.y, rot90(p0), rot90(p1), rot90(p2));
            }
            // Non-zero winding rule in either contour orientation (review-carbon Risk 4).
            cov = clamp(abs(coverage) * 0.5, 0.0, 1.0);
            if is_sprite {
                cov *= f32(in.glyph_arg) / 255.0;
            }
        }
    }

    // Premultiplied composite: underline -> strikethrough/overline -> glyph ink.
    var pre = vec3<f32>(0.0);
    var alpha = 0.0;

    if !greek && (in.style & STYLE_LINES) != 0u {
        let x = in.glyph_uv.x * in.cell_info.x;
        let dx = fw.x * in.cell_info.x;
        let in_cell = step(x, in.cell_info.z + 0.5 * dx);
        let y = in.glyph_uv.y;
        let lo = max(y - 0.5 * fw.y, 0.0);
        let inv = 1.0 / max(min(y + 0.5 * fw.y, 1.0) - lo, 1e-6);
        let t = max(frame.line_a.y, fw.y);
        let h = 0.5 * t;

        let ul = (in.style >> STYLE_UNDERLINE_SHIFT) & 7u;
        var ul_cov = 0.0;
        if ul == UNDERLINE_SINGLE || ul == UNDERLINE_DOTTED || ul == UNDERLINE_DASHED {
            let c = max(frame.line_a.x, h);
            ul_cov = band_coverage(c - h, c + h, lo, inv);
            if ul == UNDERLINE_DOTTED {
                let dots = max(1.0, floor(in.cell_info.y / (2.0 * t) + 0.5));
                ul_cov *= pulse_train(x * dots, 0.5, dx * dots);
            } else if ul == UNDERLINE_DASHED {
                ul_cov *= pulse_train(x + 1.0 / 3.0, 2.0 / 3.0, dx);
            }
        } else if ul == UNDERLINE_DOUBLE {
            let c0 = max(frame.line_b.x, h);
            let c1 = max(frame.line_b.y, c0 + 2.0 * t);
            ul_cov = band_coverage(c0 - h, c0 + h, lo, inv)
                + band_coverage(c1 - h, c1 + h, lo, inv);
        } else if ul == UNDERLINE_CURLY {
            let amp = max(frame.line_b.w, 0.5 * fw.y);
            let w = TAU * x;
            let cy = max(frame.line_b.z, amp + h) + amp * sin(w);
            let slope = amp * TAU * cos(w) * dx / fw.y;
            let dist = abs(y - cy) / fw.y / sqrt(1.0 + slope * slope);
            ul_cov = clamp(0.5 + h / fw.y - dist, 0.0, 1.0);
        }
        ul_cov = clamp(ul_cov, 0.0, 1.0) * in_cell;
        pre = srgb_to_output(in.decor_srgb) * ul_cov;
        alpha = ul_cov;

        var line_cov = 0.0;
        if (in.style & STYLE_STRIKE) != 0u {
            line_cov = band_coverage(frame.line_a.z - h, frame.line_a.z + h, lo, inv);
        }
        if (in.style & STYLE_OVERLINE) != 0u {
            line_cov = max(line_cov, band_coverage(1.0 - t, 1.0, lo, inv));
        }
        line_cov *= in_cell;
        pre = fg * line_cov + pre * (1.0 - line_cov);
        alpha = line_cov + alpha * (1.0 - line_cov);
    }

    pre = ink * cov + pre * (1.0 - cov);
    alpha = cov + alpha * (1.0 - cov);

    let keep = in.fg_srgb.a * select(1.0, frame.dim_alpha, (in.style & STYLE_DIM) != 0u);
    alpha *= keep;
    if alpha <= 0.0 {
        discard;
    }
    return vec4<f32>(pre * keep, alpha);
}

// ── Pass 4: Cursor (block, bar, underline; hollow when unfocused) ───────────

struct CursorVsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) foot_p0: vec3<f32>,
    @location(2) @interpolate(flat) foot_pu: vec3<f32>,
    @location(3) @interpolate(flat) foot_pv: vec3<f32>,
};

@vertex
fn vs_cursor(@builtin(vertex_index) vi: u32) -> CursorVsOut {
    var out: CursorVsOut;
    if (term.cursor_flags & 1u) == 0u || term.cursor_col >= term.cols || term.cursor_row >= term.rows {
        out.clip = OFFSCREEN;
        return out;
    }
    let span_cols = max((term.cursor_flags >> 4u) & 3u, 1u);
    let c = quad_corner(vi);
    let x0 = term.origin.x + f32(term.cursor_col) * frame.cell_size.x;
    let x1 = term.origin.x + f32(term.cursor_col + span_cols) * frame.cell_size.x;
    let y0 = term.origin.y - f32(term.cursor_row + 1u) * frame.cell_size.y;
    let y1 = term.origin.y - f32(term.cursor_row) * frame.cell_size.y;
    let x = select(x0, x1, c.x > 0.5);
    let y = select(y0, y1, c.y > 0.5);
    let z = term.z + Z_OFFSET_CURSOR;

    let clip = frame.view_proj * vec4<f32>(x, y, z, 1.0);
    let foot_cu = frame.view_proj * vec4<f32>(x1 - x0, 0.0, 0.0, 0.0);
    let foot_cv = frame.view_proj * vec4<f32>(0.0, y1 - y0, 0.0, 0.0);
    let foot_c0 = clip - c.x * foot_cu - c.y * foot_cv;
    let foot_scale = vec3<f32>(frame.viewport.x * 0.5, -frame.viewport.y * 0.5, 1.0);

    out.clip = clip;
    out.uv = c;
    out.foot_p0 = foot_c0.xyw * foot_scale;
    out.foot_pu = foot_cu.xyw * foot_scale;
    out.foot_pv = foot_cv.xyw * foot_scale;
    return out;
}

@fragment
fn fs_cursor(in: CursorVsOut) -> @location(0) vec4<f32> {
    let j = foot_jacobian(in.foot_p0, in.foot_pu, in.foot_pv, in.uv);
    let fw = max(abs(j[0]) + abs(j[1]), vec2<f32>(1e-6));
    let cell_px = vec2<f32>(1.0) / fw;
    let px_pos = in.uv * cell_px;

    let focused = term.focused != 0u;
    let shape = (term.cursor_flags >> 1u) & 3u;
    var alpha = 0.0;

    if !focused {
        // Hollow crisp screen-pixel outline when unfocused (cursorMaterial.js).
        let edge_px = min(px_pos, cell_px - px_pos);
        let dist_px = min(edge_px.x, edge_px.y);
        let bw = frame.cursor_border_px;
        alpha = 1.0 - smoothstep(bw - 0.5, bw + 0.5, dist_px);
    } else if shape == 1u {
        // Bar cursor on the left edge of the cell.
        let bar_w = max(2.0, cell_px.x * 0.14);
        alpha = (1.0 - smoothstep(bar_w - 0.5, bar_w + 0.5, px_pos.x)) * 0.92;
    } else if shape == 2u {
        // Underline cursor along the bottom edge of the cell.
        let ul_h = max(2.0, cell_px.y * 0.12);
        alpha = (1.0 - smoothstep(ul_h - 0.5, ul_h + 0.5, px_pos.y)) * 0.92;
    } else {
        // Focused block cursor: translucent solid fill so the glyph reads through it.
        alpha = frame.cursor_fill_alpha;
    }

    if alpha <= 0.0 {
        discard;
    }
    let rgb = srgb_to_output(frame.cursor_color.rgb);
    return vec4<f32>(rgb * alpha, alpha);
}
