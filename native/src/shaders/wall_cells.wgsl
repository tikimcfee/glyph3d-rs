// wall_cells.wgsl — Terminal wall cell renderer (panel, fills, Slug ink, lines, cursor).
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

const TEX_W: u32 = 1024u;
const MAX_CURVES: u32 = 512u;
const NO_CELL: u32 = 0xFFFFFFFFu;
const OFFSCREEN: vec4<f32> = vec4<f32>(2.0, 2.0, 2.0, 1.0);

const STYLE_DIM: u32 = 1u << 0u;
const STYLE_STRIKE: u32 = 1u << 1u;
const STYLE_OVERLINE: u32 = 1u << 2u;
const STYLE_UNDERLINE_SHIFT: u32 = 3u;
const STYLE_UNDERLINE_MASK: u32 = 7u << STYLE_UNDERLINE_SHIFT;
const STYLE_UNDERLINE_COLOR: u32 = 1u << 6u;
const STYLE_LINES: u32 = STYLE_STRIKE | STYLE_OVERLINE | STYLE_UNDERLINE_MASK;

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

@group(0) @binding(0) var<uniform> frame: WallFrameUniform;
@group(0) @binding(1) var glyphmap: texture_2d<u32>;
@group(0) @binding(2) var curves: texture_2d<u32>;
@group(0) @binding(3) var emoji_tex: texture_2d_array<f32>;
@group(0) @binding(4) var emoji_samp: sampler;

@group(1) @binding(0) var<uniform> term: TerminalUniform;
@group(1) @binding(1) var<storage, read> cells: array<CellGpu>;

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

// ── Pass 1: Background panel ────────────────────────────────────────────────

struct PanelVsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) fill_color: vec4<f32>,
};

@vertex
fn vs_panel(@builtin(vertex_index) vi: u32) -> PanelVsOut {
    let c = quad_corner(vi);
    let pad = frame.panel_padding;
    let grid_w = f32(term.cols) * frame.cell_size.x;
    let grid_h = f32(term.rows) * frame.cell_size.y;
    let x = select(term.origin.x - pad, term.origin.x + grid_w + pad, c.x > 0.5);
    let y = select(term.origin.y - grid_h - pad, term.origin.y + pad, c.y > 0.5);
    let z = term.z + Z_OFFSET_PANEL;

    let bg = unpack_rgba8(term.default_bg);
    var out: PanelVsOut;
    out.clip = frame.view_proj * vec4<f32>(x, y, z, 1.0);
    out.uv = c;
    out.fill_color = vec4<f32>(bg.rgb, term.panel_opacity);
    return out;
}

@fragment
fn fs_panel(in: PanelVsOut) -> @location(0) vec4<f32> {
    let rgb = srgb_to_output(in.fill_color.rgb);
    let alpha = in.fill_color.a;
    return vec4<f32>(rgb * alpha, alpha);
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

// ── Pass 3: Cell ink (Slug curves) ──────────────────────────────────────────

struct InkVsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) glyph_uv: vec2<f32>,
    @location(1) @interpolate(flat) fg_srgb: vec3<f32>,
    @location(2) @interpolate(flat) curve_start: u32,
    @location(3) @interpolate(flat) curve_count: u32,
    @location(4) @interpolate(flat) mode: u32,
    @location(5) @interpolate(flat) foot_p0: vec3<f32>,
    @location(6) @interpolate(flat) foot_pu: vec3<f32>,
    @location(7) @interpolate(flat) foot_pv: vec3<f32>,
};

@vertex
fn vs_ink(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> InkVsOut {
    var out: InkVsOut;
    let cell = cells[ii];
    let glyph_id = cell.glyph_and_flags & 0xFFFFu;
    let style = (cell.glyph_and_flags >> 16u) & 0x7Fu;
    let span_cols = (cell.glyph_and_flags >> 24u) & 0x3u;

    if span_cols == 0u || (glyph_id == 0u && (style & STYLE_LINES) == 0u) {
        out.clip = OFFSCREEN;
        return out;
    }

    let info = textureLoad(glyphmap, vec2<i32>(i32(glyph_id % TEX_W), i32(glyph_id / TEX_W)), 0);
    let curve_start = info.x;
    let curve_count = info.y;
    let mode = info.z;
    let has_ink = glyph_id != 0u && mode == 0u && curve_count > 0u;
    if !has_ink {
        out.clip = OFFSCREEN;
        return out;
    }

    let col = ii % term.cols;
    let row = ii / term.cols;
    let c = quad_corner(vi);
    let x0 = term.origin.x + f32(col) * frame.cell_size.x;
    let x1 = term.origin.x + f32(col + span_cols) * frame.cell_size.x;
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

    out.clip = clip;
    out.glyph_uv = c;
    out.fg_srgb = unpack_rgba8(cell.fg).rgb;
    out.curve_start = curve_start;
    out.curve_count = curve_count;
    out.mode = mode;
    out.foot_p0 = foot_c0.xyw * foot_scale;
    out.foot_pu = foot_cu.xyw * foot_scale;
    out.foot_pv = foot_cv.xyw * foot_scale;
    return out;
}

@fragment
fn fs_ink(in: InkVsOut) -> @location(0) vec4<f32> {
    let j = foot_jacobian(in.foot_p0, in.foot_pu, in.foot_pv, in.glyph_uv);
    let fw = max(abs(j[0]) + abs(j[1]), vec2<f32>(1e-6));
    let inv_w = vec2<f32>(1.0) / fw;
    let lo = -0.5 * fw;

    var coverage = 0.0;
    let n = min(in.curve_count, MAX_CURVES);
    for (var i = 0u; i < n; i = i + 1u) {
        let ci = (in.curve_start + i) * 2u;
        let t0 = textureLoad(curves, vec2<i32>(i32(ci % TEX_W), i32(ci / TEX_W)), 0);
        let t1 = textureLoad(curves, vec2<i32>(i32((ci + 1u) % TEX_W), i32((ci + 1u) / TEX_W)), 0);

        let p0 = vec2<f32>(f32(t0.x), f32(t0.y)) / 65535.0 - in.glyph_uv;
        let p1 = vec2<f32>(f32(t0.z), f32(t0.w)) / 65535.0 - in.glyph_uv;
        let p2 = vec2<f32>(f32(t1.x), f32(t1.y)) / 65535.0 - in.glyph_uv;

        coverage += compute_coverage(lo.x, inv_w.x, p0, p1, p2);
        coverage += compute_coverage(lo.y, inv_w.y, rot90(p0), rot90(p1), rot90(p2));
    }

    let cov = clamp(abs(coverage) * 0.5, 0.0, 1.0);
    if cov <= 0.0 {
        discard;
    }
    let rgb = srgb_to_output(in.fg_srgb);
    return vec4<f32>(rgb * cov, cov);
}

// ── Pass 4: Cursor placeholder (completed in step 2) ────────────────────────

struct CursorVsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
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
    let x = select(
        term.origin.x + f32(term.cursor_col) * frame.cell_size.x,
        term.origin.x + f32(term.cursor_col + span_cols) * frame.cell_size.x,
        c.x > 0.5,
    );
    let y = select(
        term.origin.y - f32(term.cursor_row + 1u) * frame.cell_size.y,
        term.origin.y - f32(term.cursor_row) * frame.cell_size.y,
        c.y > 0.5,
    );
    let z = term.z + Z_OFFSET_CURSOR;
    out.clip = frame.view_proj * vec4<f32>(x, y, z, 1.0);
    out.uv = c;
    return out;
}

@fragment
fn fs_cursor(in: CursorVsOut) -> @location(0) vec4<f32> {
    let rgb = srgb_to_output(frame.cursor_color.rgb);
    let a = frame.cursor_fill_alpha;
    return vec4<f32>(rgb * a, a);
}
