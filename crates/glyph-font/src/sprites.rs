//! Cell-generated terminal sprites (`box drawing`, `block elements`, `braille`,
//! `legacy computing`, and `powerline`), ported from `shaping/cellSprites.js`.
//!
//! These glyphs are generated directly to the family's unit cell (`x` in `[0, 1]`, `y` up in
//! `[0, 1]`, `(0, 0)` bottom-left) using the family's cell aspect and line weights, so adjacent
//! cells meet edge-to-edge without cracks or overlaps. Contours wind clockwise (y up) to match
//! TrueType outlines. Shades (`░ ▒ ▓` and legacy half shades) emit a full rectangle with an ink
//! alpha carried in the glyph-map texel's fourth lane (mode 3).

use std::f64::consts::PI;

use crate::encode::{Curve, PackedCurve};

/// Version bumped whenever generated sprite shapes change (for external cache keys).
pub const SPRITE_VERSION: u32 = 1;

/// Per-family geometry that sprites are drawn to (`FontChain.spriteMetrics`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpriteMetrics {
    /// Cell width divided by cell height (`advance_fu / em_height_fu`).
    pub aspect: f64,
    /// Light box-drawing line thickness as a fraction of the cell's height.
    pub light: f64,
    /// Heavy box-drawing line thickness as a fraction of the cell's height.
    pub heavy: f64,
}

/// A generated sprite's normalised unit-cell quadratic curves and ink alpha (`0..=255`).
#[derive(Clone, Debug, PartialEq)]
pub struct SpriteOutline {
    pub curves: Vec<Curve>,
    pub alpha: u8,
}

/// Returns true if `cp` is generated as a cell sprite rather than taken from any font.
///
/// Covers:
/// - Box drawing (`U+2500..=U+257F`, 128)
/// - Block elements (`U+2580..=U+259F`, 32)
/// - Braille patterns (`U+2800..=U+28FF`, 256)
/// - Powerline separators (`U+E0B0..=U+E0BF`, 16)
/// - Legacy computing sextants, wedges, triangles, eighth blocks, half shades
///   (`U+1FB00..=U+1FB90`, `U+1FB9A`, `U+1FB9B`, 147)
#[inline]
pub const fn is_sprite_codepoint(cp: u32) -> bool {
    matches!(
        cp,
        0x2500..=0x259F
            | 0x2800..=0x28FF
            | 0xE0B0..=0xE0BF
            | 0x1FB00..=0x1FB90
            | 0x1FB9A
            | 0x1FB9B
    )
}

/// Generate a sprite's unit-cell outline and ink alpha (`None` if `cp` is not a sprite codepoint
/// or `metrics` is degenerate).
pub fn sprite_outline(cp: u32, m: SpriteMetrics) -> Option<SpriteOutline> {
    if !is_sprite_codepoint(cp) || !(m.aspect > 0.0 && m.light > 0.0 && m.heavy > 0.0) {
        return None;
    }
    let mut path = Path::new();
    if cp <= 0x257F {
        box_drawing(&mut path, m, cp);
    } else if cp <= 0x259F {
        block_element(&mut path, cp);
    } else if cp <= 0x28FF {
        braille(&mut path, m, cp);
    } else if cp <= 0xE0BF {
        powerline(&mut path, m, cp);
    } else {
        legacy_computing(&mut path, cp);
    }
    Some(SpriteOutline {
        curves: path.curves,
        alpha: path.alpha,
    })
}

/// Generate and pack a sprite directly into `PackedCurve`s and its ink alpha (`0..=255`).
pub fn encode_sprite(cp: u32, m: SpriteMetrics) -> Option<(Vec<PackedCurve>, u8)> {
    let outline = sprite_outline(cp, m)?;
    let packed = outline.curves.into_iter().map(PackedCurve::pack).collect();
    Some((packed, outline.alpha))
}

// ── Path building ────────────────────────────────────────────────────────────

struct Path {
    curves: Vec<Curve>,
    alpha: u8,
}

impl Path {
    fn new() -> Self {
        Self {
            curves: Vec::new(),
            alpha: 255,
        }
    }

    /// Emit a closed contour of quadratic segments `[x0, y0, cx, cy, x1, y1, ...]`,
    /// oriented clockwise (y up) regardless of input winding, clamped to `[0, 1]`.
    fn contour(&mut self, segs: &[f64]) {
        let n = segs.len() / 6;
        if n < 2 {
            return;
        }
        // Signed area of control polygon (shoelace, y up: > 0 is CCW, < 0 is CW).
        let mut area = 0.0_f64;
        for i in 0..n {
            let o = i * 6;
            area += segs[o] * segs[o + 3] - segs[o + 2] * segs[o + 1];
            area += segs[o + 2] * segs[o + 5] - segs[o + 4] * segs[o + 3];
        }
        if area.abs() < 1e-12 {
            return;
        }
        let unit = |v: f64| v.clamp(0.0, 1.0);
        if area < 0.0 {
            for i in 0..n {
                let o = i * 6;
                self.curves.push(Curve {
                    p0x: unit(segs[o]),
                    p0y: unit(segs[o + 1]),
                    p1x: unit(segs[o + 2]),
                    p1y: unit(segs[o + 3]),
                    p2x: unit(segs[o + 4]),
                    p2y: unit(segs[o + 5]),
                });
            }
        } else {
            for i in (0..n).rev() {
                let o = i * 6;
                self.curves.push(Curve {
                    p0x: unit(segs[o + 4]),
                    p0y: unit(segs[o + 5]),
                    p1x: unit(segs[o + 2]),
                    p1y: unit(segs[o + 3]),
                    p2x: unit(segs[o]),
                    p2y: unit(segs[o + 1]),
                });
            }
        }
    }

    /// Emit a closed polygon from flat `[x, y, ...]`, dropping zero-length edges.
    fn poly(&mut self, pts: &[f64]) {
        let n = pts.len() / 2;
        if n < 3 {
            return;
        }
        let mut segs = Vec::with_capacity(n * 6);
        for i in 0..n {
            let x0 = pts[i * 2];
            let y0 = pts[i * 2 + 1];
            let next = (i + 1) % n;
            let x1 = pts[next * 2];
            let y1 = pts[next * 2 + 1];
            if (x1 - x0).abs() < 1e-9 && (y1 - y0).abs() < 1e-9 {
                continue;
            }
            segs.extend_from_slice(&[x0, y0, (x0 + x1) * 0.5, (y0 + y1) * 0.5, x1, y1]);
        }
        self.contour(&segs);
    }

    /// Emit an axis-aligned rectangle clipped to `[0, 1] x [0, 1]`.
    fn rect(&mut self, x0: f64, y0: f64, x1: f64, y1: f64) {
        let x0 = x0.max(0.0);
        let y0 = y0.max(0.0);
        let x1 = x1.min(1.0);
        let y1 = y1.min(1.0);
        if x1 - x0 < 1e-9 || y1 - y0 < 1e-9 {
            return;
        }
        self.poly(&[x0, y0, x0, y1, x1, y1, x1, y0]);
    }
}

/// Clip a convex polygon (flat `[x, y, ...]`) to the half-plane `a*x + b*y <= c`
/// using Sutherland–Hodgman clipping.
fn clip_half(pts: &[f64], a: f64, b: f64, c: f64) -> Vec<f64> {
    let mut out = Vec::with_capacity(pts.len() + 4);
    let n = pts.len() / 2;
    for i in 0..n {
        let x0 = pts[i * 2];
        let y0 = pts[i * 2 + 1];
        let next = (i + 1) % n;
        let x1 = pts[next * 2];
        let y1 = pts[next * 2 + 1];
        let d0 = a * x0 + b * y0 - c;
        let d1 = a * x1 + b * y1 - c;
        if d0 <= 0.0 {
            out.push(x0);
            out.push(y0);
        }
        if (d0 < 0.0 && d1 > 0.0) || (d0 > 0.0 && d1 < 0.0) {
            let t = d0 / (d0 - d1);
            out.push(x0 + (x1 - x0) * t);
            out.push(y0 + (y1 - y0) * t);
        }
    }
    out
}

/// Clip a convex polygon to the unit cell `[0, 1] x [0, 1]`.
fn clip_cell(pts: &[f64]) -> Vec<f64> {
    let p = clip_half(pts, -1.0, 0.0, 0.0);
    let p = clip_half(&p, 1.0, 0.0, 1.0);
    let p = clip_half(&p, 0.0, -1.0, 0.0);
    clip_half(&p, 0.0, 1.0, 1.0)
}

/// Straight stroke from `(x0, y0)` to `(x1, y1)` of thickness `t` (cell-height fraction),
/// extended past both ends and clipped to the cell so edge/corner joins fill cleanly.
fn stroke(path: &mut Path, m: SpriteMetrics, x0: f64, y0: f64, x1: f64, y1: f64, t: f64) {
    let ax = x0 * m.aspect;
    let bx = x1 * m.aspect;
    let dx = bx - ax;
    let dy = y1 - y0;
    let len = dx.hypot(dy);
    if len < 1e-9 {
        return;
    }
    let ux = dx / len;
    let uy = dy / len;
    let nx = -uy * t * 0.5;
    let ny = ux * t * 0.5;
    let ex = ux * t;
    let ey = uy * t;
    let mut pts = [
        ax - ex + nx,
        y0 - ey + ny,
        bx + ex + nx,
        y1 + ey + ny,
        bx + ex - nx,
        y1 + ey - ny,
        ax - ex - nx,
        y0 - ey - ny,
    ];
    for i in (0..pts.len()).step_by(2) {
        pts[i] /= m.aspect;
    }
    let clipped = clip_cell(&pts);
    path.poly(&clipped);
}

/// Append an elliptical arc `(cx, cy, rx, ry)` from `a0` to `a1` radians as quadratic segments
/// of at most 45 degrees each.
fn arc_to(segs: &mut Vec<f64>, cx: f64, cy: f64, rx: f64, ry: f64, a0: f64, a1: f64) {
    let steps = (((a1 - a0).abs() / (PI / 4.0) - 1e-9).ceil() as usize).max(1);
    let d = (a1 - a0) / steps as f64;
    let k = 1.0 / (d * 0.5).cos();
    for i in 0..steps {
        let s = a0 + i as f64 * d;
        let e = s + d;
        let mid = s + d * 0.5;
        segs.extend_from_slice(&[
            cx + rx * s.cos(),
            cy + ry * s.sin(),
            cx + rx * k * mid.cos(),
            cy + ry * k * mid.sin(),
            cx + rx * e.cos(),
            cy + ry * e.sin(),
        ]);
    }
}

/// Append a straight segment from the last point in `segs` to `(x, y)`.
fn line_to(segs: &mut Vec<f64>, x: f64, y: f64) {
    let n = segs.len();
    let x0 = segs[n - 2];
    let y0 = segs[n - 1];
    if (x - x0).abs() < 1e-9 && (y - y0).abs() < 1e-9 {
        return;
    }
    segs.extend_from_slice(&[x0, y0, (x0 + x) * 0.5, (y0 + y) * 0.5, x, y]);
}

// ── Box drawing (U+2500..=U+257F) ────────────────────────────────────────────

/// Four arms per character `(up, right, down, left)`: `0` none, `1` light, `2` heavy, `3` double;
/// `....` marks dashes, rounded arcs, and diagonals handled separately.
const BOX_ARMS: &[u8] = concat!(
    "0101020210102020................................01100210012002200011001200210022110012002100220010011002200120021110121021101120",
    "21202210122022201011101220111021202120121022202201110112021102120121012202210222110111021201120221012102220122021111111212111212",
    "211111212121211222111122122122121222212222212222................0303303003100130033000130031003313003100330010033001300313103130",
    "3330101330313033031301310333130331013303131331313333............................000110000100001000022000020000200201102001022010",
)
.as_bytes();

/// 3x3 centre-junction masks for double-line box drawing `U+2550..=U+256C` (rows top to bottom).
const DOUBLE_JOINS: [&[u8; 9]; 29] = [
    b"###...###",
    b"#.##.##.#",
    b".##.#..##",
    b"...####.#",
    b"####..#.#",
    b"##..#.##.",
    b"...####.#",
    b"###..##.#",
    b".##.#..##",
    b"#.####...",
    b"#.##..###",
    b"##..#.##.",
    b"#.####...",
    b"#.#..####",
    b".##.#..##",
    b"#.##.##.#",
    b"#.##..#.#",
    b"##..#.##.",
    b"#.##.##.#",
    b"#.#..##.#",
    b"###...###",
    b"...####.#",
    b"###...#.#",
    b"###...###",
    b"#.####...",
    b"#.#...###",
    b"###.#.###",
    b"#.#####.#",
    b"#.#...#.#",
];

fn box_lines(path: &mut Path, m: SpriteMetrics, up: u8, right: u8, down: u8, left: u8) {
    let tx = |w: u8| if w == 2 { m.heavy } else { m.light } / m.aspect;
    let ty = |w: u8| if w == 2 { m.heavy } else { m.light };
    let jx = (if up > 0 { tx(up) } else { 0.0 }).max(if down > 0 { tx(down) } else { 0.0 }) * 0.5;
    let jy =
        (if left > 0 { ty(left) } else { 0.0 }).max(if right > 0 { ty(right) } else { 0.0 }) * 0.5;
    if up > 0 {
        let half = tx(up) * 0.5;
        path.rect(0.5 - half, 0.5 - jy, 0.5 + half, 1.0);
    }
    if down > 0 {
        let half = tx(down) * 0.5;
        path.rect(0.5 - half, 0.0, 0.5 + half, 0.5 + jy);
    }
    if left > 0 {
        let half = ty(left) * 0.5;
        path.rect(0.0, 0.5 - half, 0.5 + jx, 0.5 + half);
    }
    if right > 0 {
        let half = ty(right) * 0.5;
        path.rect(0.5 - jx, 0.5 - half, 1.0, 0.5 + half);
    }
}

fn box_double(
    path: &mut Path,
    m: SpriteMetrics,
    up: u8,
    right: u8,
    down: u8,
    left: u8,
    join: &[u8; 9],
) {
    let tx = m.light / m.aspect;
    let ty = m.light;
    let xs = [0.5 - 1.5 * tx, 0.5 - 0.5 * tx, 0.5 + 0.5 * tx, 0.5 + 1.5 * tx];
    let ys = [0.5 - 1.5 * ty, 0.5 - 0.5 * ty, 0.5 + 0.5 * ty, 0.5 + 1.5 * ty];
    let lanes = |w: u8| -> &'static [usize] {
        match w {
            3 => &[0, 2],
            1 => &[1],
            _ => &[],
        }
    };
    for &i in lanes(up) {
        path.rect(xs[i], ys[3], xs[i + 1], 1.0);
    }
    for &i in lanes(down) {
        path.rect(xs[i], 0.0, xs[i + 1], ys[0]);
    }
    for &j in lanes(left) {
        path.rect(0.0, ys[j], xs[0], ys[j + 1]);
    }
    for &j in lanes(right) {
        path.rect(xs[3], ys[j], 1.0, ys[j + 1]);
    }
    for r in 0..3 {
        for c in 0..3 {
            if join[r * 3 + c] == b'#' {
                path.rect(xs[c], ys[2 - r], xs[c + 1], ys[3 - r]);
            }
        }
    }
}

fn box_dashes(path: &mut Path, m: SpriteMetrics, n: usize, heavy: bool, vertical: bool) {
    let t = if heavy { m.heavy } else { m.light };
    let nf = n as f64;
    let gap = 0.4 / nf;
    for i in 0..n {
        let a = i as f64 / nf + gap * 0.5;
        let b = (i + 1) as f64 / nf - gap * 0.5;
        if vertical {
            let half = t / m.aspect * 0.5;
            path.rect(0.5 - half, a, 0.5 + half, b);
        } else {
            let half = t * 0.5;
            path.rect(a, 0.5 - half, b, 0.5 + half);
        }
    }
}

fn box_arc(path: &mut Path, m: SpriteMetrics, flip_x: bool, flip_y: bool) {
    let w = m.aspect;
    let t = m.light;
    let r = w.min(1.0) * 0.5;
    let ro = r + t * 0.5;
    let ri = (r - t * 0.5).max(0.0);
    let cx = w * 0.5 + r;
    let cy = 0.5 - r;

    let mut segs = Vec::with_capacity(48);
    arc_to(&mut segs, cx, cy, ro, ro, PI, PI * 0.5);
    line_to(&mut segs, w, 0.5 + t * 0.5);
    line_to(&mut segs, w, 0.5 - t * 0.5);
    line_to(&mut segs, cx, cy + ri);
    if ri > 0.0 {
        arc_to(&mut segs, cx, cy, ri, ri, PI * 0.5, PI);
    } else {
        line_to(&mut segs, cx, cy);
    }
    line_to(&mut segs, w * 0.5 + t * 0.5, 0.0);
    line_to(&mut segs, w * 0.5 - t * 0.5, 0.0);
    let sx = segs[0];
    let sy = segs[1];
    line_to(&mut segs, sx, sy);

    for i in (0..segs.len()).step_by(2) {
        let x = segs[i] / w;
        let y = segs[i + 1];
        segs[i] = (if flip_x { 1.0 - x } else { x }).clamp(0.0, 1.0);
        segs[i + 1] = (if flip_y { 1.0 - y } else { y }).clamp(0.0, 1.0);
    }
    path.contour(&segs);
}

fn box_drawing(path: &mut Path, m: SpriteMetrics, cp: u32) {
    let idx = (cp - 0x2500) as usize;
    let arms = &BOX_ARMS[idx * 4..idx * 4 + 4];
    if arms != b"...." {
        let up = arms[0] - b'0';
        let right = arms[1] - b'0';
        let down = arms[2] - b'0';
        let left = arms[3] - b'0';
        if (0x2550..=0x256C).contains(&cp) {
            box_double(
                path,
                m,
                up,
                right,
                down,
                left,
                DOUBLE_JOINS[(cp - 0x2550) as usize],
            );
        } else {
            box_lines(path, m, up, right, down, left);
        }
        return;
    }
    if (0x2504..=0x250B).contains(&cp) {
        let k = cp - 0x2504;
        box_dashes(path, m, if k < 4 { 3 } else { 4 }, (k & 1) == 1, (k & 2) == 2);
        return;
    }
    if (0x254C..=0x254F).contains(&cp) {
        let k = cp - 0x254C;
        box_dashes(path, m, 2, (k & 1) == 1, (k & 2) == 2);
        return;
    }
    match cp {
        0x256D => box_arc(path, m, false, false),
        0x256E => box_arc(path, m, true, false),
        0x256F => box_arc(path, m, true, true),
        0x2570 => box_arc(path, m, false, true),
        0x2571 => stroke(path, m, 0.0, 0.0, 1.0, 1.0, m.light),
        0x2572 => stroke(path, m, 0.0, 1.0, 1.0, 0.0, m.light),
        0x2573 => {
            stroke(path, m, 0.0, 0.0, 1.0, 1.0, m.light);
            stroke(path, m, 0.0, 1.0, 1.0, 0.0, m.light);
        }
        _ => {}
    }
}

// ── Block elements (U+2580..=U+259F) ─────────────────────────────────────────

const QUADRANTS: [u8; 10] = [4, 8, 1, 13, 9, 7, 11, 2, 6, 14];

fn quadrants(path: &mut Path, bits: u8) {
    if bits & 1 != 0 {
        path.rect(0.0, 0.5, 0.5, 1.0);
    }
    if bits & 2 != 0 {
        path.rect(0.5, 0.5, 1.0, 1.0);
    }
    if bits & 4 != 0 {
        path.rect(0.0, 0.0, 0.5, 0.5);
    }
    if bits & 8 != 0 {
        path.rect(0.5, 0.0, 1.0, 0.5);
    }
}

fn block_element(path: &mut Path, cp: u32) {
    if cp == 0x2580 {
        path.rect(0.0, 0.5, 1.0, 1.0);
    } else if cp <= 0x2588 {
        path.rect(0.0, 0.0, 1.0, (cp - 0x2580) as f64 / 8.0);
    } else if cp <= 0x258F {
        path.rect(0.0, 0.0, (0x2590 - cp) as f64 / 8.0, 1.0);
    } else if cp == 0x2590 {
        path.rect(0.5, 0.0, 1.0, 1.0);
    } else if cp <= 0x2593 {
        path.rect(0.0, 0.0, 1.0, 1.0);
        path.alpha = (255.0 * (cp - 0x2590) as f64 / 4.0).round() as u8;
    } else if cp == 0x2594 {
        path.rect(0.0, 7.0 / 8.0, 1.0, 1.0);
    } else if cp == 0x2595 {
        path.rect(7.0 / 8.0, 0.0, 1.0, 1.0);
    } else {
        quadrants(path, QUADRANTS[(cp - 0x2596) as usize]);
    }
}

// ── Braille (U+2800..=U+28FF) ────────────────────────────────────────────────

fn braille(path: &mut Path, m: SpriteMetrics, cp: u32) {
    const ROW: [u8; 8] = [0, 1, 2, 0, 1, 2, 3, 3];
    const COL: [u8; 8] = [0, 0, 0, 1, 1, 1, 0, 1];
    let bits = cp - 0x2800;
    let side = (m.aspect / 4.0).min(1.0 / 8.0);
    let hx = side / m.aspect * 0.5;
    let hy = side * 0.5;
    for d in 0..8 {
        if bits & (1 << d) == 0 {
            continue;
        }
        let x = if COL[d] != 0 { 0.75 } else { 0.25 };
        let y = 1.0 - (ROW[d] as f64 * 2.0 + 1.0) / 8.0;
        path.rect(x - hx, y - hy, x + hx, y + hy);
    }
}

// ── Legacy computing (U+1FB00..=U+1FB90, U+1FB9A, U+1FB9B) ──────────────────

const WEDGES: &[u8] = b"L0110L0120L0210L0220L0310R0213R0223R0113R0123R0013R0122R1021R0021R1022R0022R1023L1322L0322L1321L0321L1320L0221";

fn wedge(path: &mut Path, index: u32) {
    let i = (index % 22) as usize;
    let o = i * 5;
    let corner = if WEDGES[o] == b'L' { 0.0 } else { 1.0 };
    let ax = (WEDGES[o + 1] - b'0') as f64 / 2.0;
    let ay = (WEDGES[o + 2] - b'0') as f64 / 3.0;
    let bx = (WEDGES[o + 3] - b'0') as f64 / 2.0;
    let by = (WEDGES[o + 4] - b'0') as f64 / 3.0;

    let mut nx = by - ay;
    let mut ny = ax - bx;
    let mut c = nx * ax + ny * ay;
    let mut keep_neg = nx * corner - c < 0.0;
    if index >= 22 {
        keep_neg = !keep_neg;
    }
    if !keep_neg {
        nx = -nx;
        ny = -ny;
        c = -c;
    }
    let clipped = clip_half(&[0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0, 0.0], nx, ny, c);
    path.poly(&clipped);
}

fn legacy_computing(path: &mut Path, cp: u32) {
    if cp <= 0x1FB3B {
        let mut bits = cp - 0x1FB00 + 1;
        if bits >= 21 {
            bits += 1;
        }
        if bits >= 42 {
            bits += 1;
        }
        for b in 0..6 {
            if bits & (1 << b) == 0 {
                continue;
            }
            let col = (b & 1) as f64;
            let row = (b >> 1) as f64;
            path.rect(col / 2.0, 1.0 - (row + 1.0) / 3.0, (col + 1.0) / 2.0, 1.0 - row / 3.0);
        }
        return;
    }
    if cp <= 0x1FB67 {
        wedge(path, cp - 0x1FB3C);
        return;
    }
    const L: [f64; 6] = [0.0, 0.0, 0.0, 1.0, 0.5, 0.5];
    const U: [f64; 6] = [0.0, 1.0, 1.0, 1.0, 0.5, 0.5];
    const R: [f64; 6] = [1.0, 1.0, 1.0, 0.0, 0.5, 0.5];
    const D: [f64; 6] = [1.0, 0.0, 0.0, 0.0, 0.5, 0.5];
    match cp {
        0x1FB68 => {
            path.poly(&U);
            path.poly(&R);
            path.poly(&D);
        }
        0x1FB69 => {
            path.poly(&L);
            path.poly(&D);
            path.poly(&R);
        }
        0x1FB6A => {
            path.poly(&U);
            path.poly(&L);
            path.poly(&D);
        }
        0x1FB6B => {
            path.poly(&L);
            path.poly(&U);
            path.poly(&R);
        }
        0x1FB6C => path.poly(&L),
        0x1FB6D => path.poly(&U),
        0x1FB6E => path.poly(&R),
        0x1FB6F => path.poly(&D),
        0x1FB7C => {
            path.rect(0.0, 0.0, 1.0 / 8.0, 1.0);
            path.rect(0.0, 0.0, 1.0, 1.0 / 8.0);
        }
        0x1FB7D => {
            path.rect(0.0, 0.0, 1.0 / 8.0, 1.0);
            path.rect(0.0, 7.0 / 8.0, 1.0, 1.0);
        }
        0x1FB7E => {
            path.rect(7.0 / 8.0, 0.0, 1.0, 1.0);
            path.rect(0.0, 7.0 / 8.0, 1.0, 1.0);
        }
        0x1FB7F => {
            path.rect(7.0 / 8.0, 0.0, 1.0, 1.0);
            path.rect(0.0, 0.0, 1.0, 1.0 / 8.0);
        }
        0x1FB80 => {
            path.rect(0.0, 7.0 / 8.0, 1.0, 1.0);
            path.rect(0.0, 0.0, 1.0, 1.0 / 8.0);
        }
        0x1FB81 => {
            for n in [1.0, 3.0, 5.0, 8.0] {
                path.rect(0.0, 1.0 - n / 8.0, 1.0, 1.0 - (n - 1.0) / 8.0);
            }
        }
        0x1FB8C => {
            path.rect(0.0, 0.0, 0.5, 1.0);
            path.alpha = 128;
        }
        0x1FB8D => {
            path.rect(0.5, 0.0, 1.0, 1.0);
            path.alpha = 128;
        }
        0x1FB8E => {
            path.rect(0.0, 0.5, 1.0, 1.0);
            path.alpha = 128;
        }
        0x1FB8F => {
            path.rect(0.0, 0.0, 1.0, 0.5);
            path.alpha = 128;
        }
        0x1FB90 => {
            path.rect(0.0, 0.0, 1.0, 1.0);
            path.alpha = 128;
        }
        0x1FB9A => {
            path.poly(&U);
            path.poly(&D);
        }
        0x1FB9B => {
            path.poly(&L);
            path.poly(&R);
        }
        0x1FB70..=0x1FB75 => {
            let n = (cp - 0x1FB70 + 2) as f64;
            path.rect((n - 1.0) / 8.0, 0.0, n / 8.0, 1.0);
        }
        0x1FB76..=0x1FB7B => {
            let n = (cp - 0x1FB76 + 2) as f64;
            path.rect(0.0, 1.0 - n / 8.0, 1.0, 1.0 - (n - 1.0) / 8.0);
        }
        0x1FB82..=0x1FB86 => {
            const EIGHTHS: [f64; 5] = [2.0, 3.0, 5.0, 6.0, 7.0];
            path.rect(0.0, 1.0 - EIGHTHS[(cp - 0x1FB82) as usize] / 8.0, 1.0, 1.0);
        }
        0x1FB87..=0x1FB8B => {
            const EIGHTHS: [f64; 5] = [2.0, 3.0, 5.0, 6.0, 7.0];
            path.rect(1.0 - EIGHTHS[(cp - 0x1FB87) as usize] / 8.0, 0.0, 1.0, 1.0);
        }
        _ => {}
    }
}

// ── Powerline (U+E0B0..=U+E0BF) ──────────────────────────────────────────────

fn powerline(path: &mut Path, m: SpriteMetrics, cp: u32) {
    let t = m.light;
    let mirror = |segs: &mut [f64]| {
        for i in (0..segs.len()).step_by(2) {
            segs[i] = 1.0 - segs[i];
        }
    };
    let half_ellipse = |rx: f64, ry: f64| -> Vec<f64> {
        let mut s = Vec::with_capacity(32);
        arc_to(&mut s, 0.0, 0.5, rx, ry, PI * 0.5, -PI * 0.5);
        s
    };
    match cp {
        0xE0B0 => path.poly(&[0.0, 0.0, 0.0, 1.0, 1.0, 0.5]),
        0xE0B1 => {
            stroke(path, m, 0.0, 1.0, 1.0, 0.5, t);
            stroke(path, m, 1.0, 0.5, 0.0, 0.0, t);
        }
        0xE0B2 => path.poly(&[1.0, 0.0, 1.0, 1.0, 0.0, 0.5]),
        0xE0B3 => {
            stroke(path, m, 1.0, 1.0, 0.0, 0.5, t);
            stroke(path, m, 0.0, 0.5, 1.0, 0.0, t);
        }
        0xE0B4 | 0xE0B6 => {
            let mut s = half_ellipse(1.0, 0.5);
            let (sx, sy) = (s[0], s[1]);
            line_to(&mut s, sx, sy);
            if cp == 0xE0B6 {
                mirror(&mut s);
            }
            path.contour(&s);
        }
        0xE0B5 | 0xE0B7 => {
            let tx = t / m.aspect;
            let mut s = half_ellipse(1.0, 0.5);
            line_to(&mut s, 0.0, t);
            arc_to(
                &mut s,
                0.0,
                0.5,
                (1.0 - tx).max(0.0),
                (0.5 - t).max(0.0),
                -PI * 0.5,
                PI * 0.5,
            );
            let (sx, sy) = (s[0], s[1]);
            line_to(&mut s, sx, sy);
            if cp == 0xE0B7 {
                mirror(&mut s);
            }
            path.contour(&s);
        }
        0xE0B8 => path.poly(&[0.0, 0.0, 0.0, 1.0, 1.0, 0.0]),
        0xE0B9 | 0xE0BF => stroke(path, m, 0.0, 1.0, 1.0, 0.0, t),
        0xE0BA => path.poly(&[1.0, 0.0, 1.0, 1.0, 0.0, 0.0]),
        0xE0BB | 0xE0BD => stroke(path, m, 0.0, 0.0, 1.0, 1.0, t),
        0xE0BC => path.poly(&[0.0, 0.0, 0.0, 1.0, 1.0, 1.0]),
        0xE0BE => path.poly(&[0.0, 1.0, 1.0, 1.0, 1.0, 0.0]),
        _ => {}
    }
}
