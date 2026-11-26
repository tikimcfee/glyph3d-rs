//! Font outline extraction and Slug quadratic-curve encoding.
//!
//! Converts a `ttf_parser::Face`'s glyph outline into packed quadratic Bézier curves
//! normalised to the glyph's own cell (`x` over `[0, advance]`, `y` over `[descender, ascender]`,
//! y up), matching `assets/atlas/FORMAT.md` and `slugData.js`.
//!
//! - TrueType (`glyf`) outlines are already quadratic and pass through directly.
//! - Straight lines become degenerate quadratics with `P1` at the midpoint.
//! - Open contours close explicitly if the end point differs from the start by > 0.01 fu.
//! - CFF cubic Béziers are converted to $G^1$-continuous quadratic splines via recursive
//!   de Casteljau subdivision until the exact maximum error is below tolerance. CFF outlines
//!   use counter-clockwise outer winding (opposite of TrueType's clockwise outer winding),
//!   so CFF curves are reversed to match TrueType winding — compatible with both signed
//!   (`clamp(cov, 0, 1)`) and unsigned (`abs(cov)`) Slug fragment shaders.

use ttf_parser::{Face, GlyphId, OutlineBuilder};

use crate::constants::{pack_u16, U32S_PER_CURVE};
use crate::metrics::FontExtents;

/// A quadratic Bézier curve `(P0, P1, P2)` in un-packed floating-point coordinates
/// (either font units before normalisation or `[0, 1]` cell coordinates after).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Curve {
    pub p0x: f64,
    pub p0y: f64,
    pub p1x: f64,
    pub p1y: f64,
    pub p2x: f64,
    pub p2y: f64,
}

/// A quadratic Bézier curve packed in the engine's `curves` texture format:
/// two RGBA32Uint texels, each coordinate quantised to `0..=65535`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackedCurve {
    pub p0x: u32,
    pub p0y: u32,
    pub p1x: u32,
    pub p1y: u32,
    pub p2x: u32,
    pub p2y: u32,
}

impl PackedCurve {
    /// Pack a normalised `[0, 1]` curve into texel coordinates.
    #[inline]
    pub fn pack(curve: Curve) -> Self {
        Self {
            p0x: pack_u16(curve.p0x),
            p0y: pack_u16(curve.p0y),
            p1x: pack_u16(curve.p1x),
            p1y: pack_u16(curve.p1y),
            p2x: pack_u16(curve.p2x),
            p2y: pack_u16(curve.p2y),
        }
    }

    /// The two RGBA32Uint texels (`8` `u32` words) written to the curve texture.
    #[inline]
    pub const fn to_texels(self) -> [u32; U32S_PER_CURVE as usize] {
        [self.p0x, self.p0y, self.p1x, self.p1y, self.p2x, self.p2y, 0, 0]
    }

    /// Reconstruct a `PackedCurve` from its two RGBA32Uint texels.
    #[inline]
    pub const fn from_texels(texels: [u32; U32S_PER_CURVE as usize]) -> Self {
        Self {
            p0x: texels[0],
            p0y: texels[1],
            p1x: texels[2],
            p1y: texels[3],
            p2x: texels[4],
            p2y: texels[5],
        }
    }
}

/// Control-point-hull bounding box in font units or normalised cell coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CurveBBox {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

/// Result of encoding a single glyph.
#[derive(Clone, Debug)]
pub struct EncodedGlyph {
    /// Packed curves in cell-normalised u16 coordinates. Empty for whitespace / blank glyphs.
    pub curves: Vec<PackedCurve>,
    /// This glyph's horizontal advance in its own font's units (the X normalisation denominator).
    pub advance_fu: i32,
    /// Conservative control-point-hull bounding box in font units (`None` when `curves` is empty).
    pub raw_bbox: Option<CurveBBox>,
    /// Maximum fraction of `advance_fu` by which the hull overhangs `[0, advance_fu]`, if > 1%.
    pub overhang_fraction: Option<f64>,
    /// True if any cubic Bézier curves were converted (e.g. from a CFF outline).
    pub had_cubics: bool,
}

/// Maximum subdivision depth when converting a CFF cubic Bézier into quadratics.
/// Each split divides the approximation error by 8, so depth 6 gives a 262,144x reduction.
const MAX_CUBIC_SPLIT_DEPTH: u32 = 6;

/// Tolerance for cubic-to-quadratic conversion, as a fraction of the font's vertical em.
/// `1 / 4096` of an em (~0.25 fu on a 1000-upem CFF font) is ~16 u16 texel steps worst-case
/// while keeping curve counts compact (typically 2 or 4 quadratics per cubic).
const CUBIC_TOLERANCE_EM: f64 = 1.0 / 4096.0;

/// Collects `ttf_parser` outline callbacks directly into raw font-unit quadratic curves.
pub struct OutlineCollector {
    curves: Vec<Curve>,
    current_x: f64,
    current_y: f64,
    start_x: f64,
    start_y: f64,
    cubic_tolerance: f64,
    had_cubics: bool,
}

impl OutlineCollector {
    /// Create a collector with the given cubic-to-quadratic error tolerance in font units.
    pub fn new(cubic_tolerance: f64) -> Self {
        Self {
            curves: Vec::new(),
            current_x: 0.0,
            current_y: 0.0,
            start_x: 0.0,
            start_y: 0.0,
            cubic_tolerance: cubic_tolerance.max(1e-4),
            had_cubics: false,
        }
    }

    /// Consume the collector and return its raw font-unit curves and whether any cubics occurred.
    pub fn into_curves(self) -> (Vec<Curve>, bool) {
        (self.curves, self.had_cubics)
    }

    fn push_cubic(&mut self, p0: (f64, f64), c1: (f64, f64), c2: (f64, f64), p3: (f64, f64), depth: u32) {
        // Exact error vector D = P3 - 3*C2 + 3*C1 - P0.
        // For the G1-continuous 2-quadratic midpoint split, max ||C(t) - Q(t)|| = ||D|| / 54.
        let dx = p3.0 - 3.0 * c2.0 + 3.0 * c1.0 - p0.0;
        let dy = p3.1 - 3.0 * c2.1 + 3.0 * c1.1 - p0.1;
        let dist = dx.hypot(dy);

        // Near-quadratic or near-linear cubic: one midpoint quadratic has exact max error ||D|| / (12 * sqrt(3)).
        if dist / (12.0 * 3.0_f64.sqrt()) <= self.cubic_tolerance {
            let q1x = 0.75 * (c1.0 + c2.0) - 0.25 * (p0.0 + p3.0);
            let q1y = 0.75 * (c1.1 + c2.1) - 0.25 * (p0.1 + p3.1);
            self.curves.push(Curve {
                p0x: p0.0,
                p0y: p0.1,
                p1x: q1x,
                p1y: q1y,
                p2x: p3.0,
                p2y: p3.1,
            });
            return;
        }

        let err_two_quad = dist / 54.0;
        if err_two_quad <= self.cubic_tolerance || depth >= MAX_CUBIC_SPLIT_DEPTH {
            // Two G1-continuous quadratics meeting at the exact cubic midpoint M = C(0.5):
            //   Qa = (P0 + 3*C1) / 4,  Qb = (3*C2 + P3) / 4,  M = (Qa + Qb) / 2.
            let qax = 0.25 * p0.0 + 0.75 * c1.0;
            let qay = 0.25 * p0.1 + 0.75 * c1.1;
            let qbx = 0.75 * c2.0 + 0.25 * p3.0;
            let qby = 0.75 * c2.1 + 0.25 * p3.1;
            let mx = 0.5 * (qax + qbx);
            let my = 0.5 * (qay + qby);
            self.curves.push(Curve {
                p0x: p0.0,
                p0y: p0.1,
                p1x: qax,
                p1y: qay,
                p2x: mx,
                p2y: my,
            });
            self.curves.push(Curve {
                p0x: mx,
                p0y: my,
                p1x: qbx,
                p1y: qby,
                p2x: p3.0,
                p2y: p3.1,
            });
            return;
        }

        // De Casteljau split at t = 0.5; each half's D shrinks by 8x.
        let m01 = (0.5 * (p0.0 + c1.0), 0.5 * (p0.1 + c1.1));
        let m12 = (0.5 * (c1.0 + c2.0), 0.5 * (c1.1 + c2.1));
        let m23 = (0.5 * (c2.0 + p3.0), 0.5 * (c2.1 + p3.1));
        let m012 = (0.5 * (m01.0 + m12.0), 0.5 * (m01.1 + m12.1));
        let m123 = (0.5 * (m12.0 + m23.0), 0.5 * (m12.1 + m23.1));
        let mid = (0.5 * (m012.0 + m123.0), 0.5 * (m012.1 + m123.1));
        self.push_cubic(p0, m01, m012, mid, depth + 1);
        self.push_cubic(mid, m123, m23, p3, depth + 1);
    }
}

impl OutlineBuilder for OutlineCollector {
    fn move_to(&mut self, x: f32, y: f32) {
        let (x, y) = (x as f64, y as f64);
        self.start_x = x;
        self.start_y = y;
        self.current_x = x;
        self.current_y = y;
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let (ex, ey) = (x as f64, y as f64);
        self.curves.push(Curve {
            p0x: self.current_x,
            p0y: self.current_y,
            p1x: (self.current_x + ex) * 0.5,
            p1y: (self.current_y + ey) * 0.5,
            p2x: ex,
            p2y: ey,
        });
        self.current_x = ex;
        self.current_y = ey;
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (cpx, cpy, ex, ey) = (x1 as f64, y1 as f64, x as f64, y as f64);
        self.curves.push(Curve {
            p0x: self.current_x,
            p0y: self.current_y,
            p1x: cpx,
            p1y: cpy,
            p2x: ex,
            p2y: ey,
        });
        self.current_x = ex;
        self.current_y = ey;
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.had_cubics = true;
        let p0 = (self.current_x, self.current_y);
        let c1 = (x1 as f64, y1 as f64);
        let c2 = (x2 as f64, y2 as f64);
        let p3 = (x as f64, y as f64);
        self.push_cubic(p0, c1, c2, p3, 0);
        self.current_x = p3.0;
        self.current_y = p3.1;
    }

    fn close(&mut self) {
        if (self.current_x - self.start_x).abs() > 0.01
            || (self.current_y - self.start_y).abs() > 0.01
        {
            self.curves.push(Curve {
                p0x: self.current_x,
                p0y: self.current_y,
                p1x: (self.current_x + self.start_x) * 0.5,
                p1y: (self.current_y + self.start_y) * 0.5,
                p2x: self.start_x,
                p2y: self.start_y,
            });
        }
        self.current_x = self.start_x;
        self.current_y = self.start_y;
    }
}

/// Returns true when the face uses CFF/CFF2 outlines (which use CCW outer winding,
/// the opposite of TrueType `glyf`'s CW outer winding).
#[inline]
pub fn is_cff_face(face: &Face<'_>) -> bool {
    face.tables().cff.is_some() || face.tables().cff2.is_some()
}

/// Encode a single glyph from a `ttf_parser::Face` into packed Slug curves normalised to its cell.
///
/// `extents` must be the vertical extents of `face` (already including line-gap adjustment if
/// enabled on that font). X is normalised by the glyph's own horizontal advance; Y is normalised
/// by `[extents.descender, extents.ascender]` with y up.
pub fn encode_glyph(face: &Face<'_>, glyph_id: GlyphId, extents: FontExtents) -> EncodedGlyph {
    let advance_fu = face.glyph_hor_advance(glyph_id).unwrap_or(0) as i32;
    let y_range = extents.em_height();
    let cubic_tolerance = (y_range.abs().max(1.0)) * CUBIC_TOLERANCE_EM;

    let mut collector = OutlineCollector::new(cubic_tolerance);
    if face.outline_glyph(glyph_id, &mut collector).is_none() {
        return EncodedGlyph {
            curves: Vec::new(),
            advance_fu,
            raw_bbox: None,
            overhang_fraction: None,
            had_cubics: false,
        };
    }

    let (mut raw_curves, had_cubics) = collector.into_curves();
    if raw_curves.is_empty() {
        return EncodedGlyph {
            curves: Vec::new(),
            advance_fu,
            raw_bbox: None,
            overhang_fraction: None,
            had_cubics,
        };
    }

    // CFF outlines are wound CCW-outer / CW-inner; flip to TrueType CW-outer / CCW-inner so
    // the engine's current `clamp(coverage * 0.5, 0, 1)` shader draws positive coverage even
    // before it switches to `abs(coverage)`. Under `abs(coverage)` the flip is a no-op.
    if is_cff_face(face) {
        reverse_curves(&mut raw_curves);
    }

    encode_raw_curves(&raw_curves, advance_fu, extents, had_cubics)
}

/// Normalise and pack raw font-unit curves against `(advance_fu, extents)`.
///
/// Exposed for tests and synthetic outlines (e.g. verifying cubic conversion directly).
pub fn encode_raw_curves(
    raw_curves: &[Curve],
    advance_fu: i32,
    extents: FontExtents,
    had_cubics: bool,
) -> EncodedGlyph {
    if raw_curves.is_empty() {
        return EncodedGlyph {
            curves: Vec::new(),
            advance_fu,
            raw_bbox: None,
            overhang_fraction: None,
            had_cubics,
        };
    }

    let bbox = compute_bbox(raw_curves);
    let overhang_fraction = if advance_fu > 0 {
        let adv = advance_fu as f64;
        let over = (bbox.max_x - adv).max(-bbox.min_x);
        if over > adv * 0.01 {
            Some(over / adv)
        } else {
            None
        }
    } else {
        None
    };

    // Match slugData.js: multiply by 1/advance and 1/(ascender - descender) in IEEE-754 f64.
    let x_scale = if advance_fu > 0 {
        1.0 / (advance_fu as f64)
    } else {
        1.0
    };
    let y_range = extents.em_height();
    let y_scale = if y_range > 0.0 { 1.0 / y_range } else { 1.0 };
    let descender = extents.descender;

    let mut packed = Vec::with_capacity(raw_curves.len());
    for c in raw_curves {
        packed.push(PackedCurve::pack(Curve {
            p0x: c.p0x * x_scale,
            p0y: (c.p0y - descender) * y_scale,
            p1x: c.p1x * x_scale,
            p1y: (c.p1y - descender) * y_scale,
            p2x: c.p2x * x_scale,
            p2y: (c.p2y - descender) * y_scale,
        }));
    }

    EncodedGlyph {
        curves: packed,
        advance_fu,
        raw_bbox: Some(bbox),
        overhang_fraction,
        had_cubics,
    }
}

/// Reverse the orientation of every curve (`P0 <-> P2`), flipping contour winding.
pub fn reverse_curves(curves: &mut [Curve]) {
    for c in curves {
        std::mem::swap(&mut c.p0x, &mut c.p2x);
        std::mem::swap(&mut c.p0y, &mut c.p2y);
    }
}

/// Compute the conservative control-point-hull bounding box of a non-empty curve slice.
pub fn compute_bbox(curves: &[Curve]) -> CurveBBox {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for c in curves {
        min_x = min_x.min(c.p0x).min(c.p1x).min(c.p2x);
        min_y = min_y.min(c.p0y).min(c.p1y).min(c.p2y);
        max_x = max_x.max(c.p0x).max(c.p1x).max(c.p2x);
        max_y = max_y.max(c.p0y).max(c.p1y).max(c.p2y);
    }
    CurveBBox {
        min_x,
        min_y,
        max_x,
        max_y,
    }
}
