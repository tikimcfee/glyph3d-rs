//! CPU Slug coverage evaluator and PGM rasteriser.
//!
//! Mirrors `compute_coverage` and `rot90` in
//! `crates/glyph-field-derived/shaders/glyph_field_derived.wgsl` so tests can measure glyph ink,
//! italic lean, and winding-direction sensitivity without a GPU, and write visual PGM proofs.

use std::io::Write;
use std::path::Path;

use crate::constants::unpack_u16;
use crate::encode::PackedCurve;

/// Rotate a 2D vector 90 degrees clockwise (`(v.y, -v.x)`), matching `rot90` in WGSL.
#[inline]
fn rot90(v: [f32; 2]) -> [f32; 2] {
    [v[1], -v[0]]
}

/// Evaluate one horizontal Slug ray's signed coverage contribution for a single quadratic curve
/// shifted into sample-relative coordinates (`p - uv`), matching `compute_coverage` in WGSL.
pub fn compute_coverage(inv_diameter: f32, dilate: f32, p0: [f32; 2], p1: [f32; 2], p2: [f32; 2]) -> f32 {
    let all_above = p0[1] > 0.0 && p1[1] > 0.0 && p2[1] > 0.0;
    let all_below = p0[1] < 0.0 && p1[1] < 0.0 && p2[1] < 0.0;
    if all_above || all_below {
        return 0.0;
    }

    let a = [p0[0] - 2.0 * p1[0] + p2[0], p0[1] - 2.0 * p1[1] + p2[1]];
    let b = [p0[0] - p1[0], p0[1] - p1[1]];
    let c = p0;

    let mut t0 = -1.0_f32;
    let mut t1 = -1.0_f32;
    let mut solvable = true;

    if a[1].abs() >= 1e-5 {
        let radicand = b[1] * b[1] - a[1] * c[1];
        if radicand > 0.0 {
            let s = radicand.sqrt();
            let q = b[1] + if b[1] >= 0.0 { s } else { -s };
            if b[1] >= 0.0 {
                t0 = c[1] / q;
                t1 = q / a[1];
            } else {
                t0 = q / a[1];
                t1 = c[1] / q;
            }
        } else {
            solvable = false;
        }
    } else {
        let denom = p0[1] - p2[1];
        if denom.abs() >= 1e-6 {
            let t = p0[1] / denom;
            if p0[1] < p2[1] {
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

    let mut result = 0.0_f32;
    if solvable {
        if (0.0..1.0).contains(&t0) {
            let x = (a[0] * t0 - 2.0 * b[0]) * t0 + c[0];
            result += (x * inv_diameter + 0.5 + dilate).clamp(0.0, 1.0);
        }
        if (0.0..1.0).contains(&t1) {
            let x = (a[0] * t1 - 2.0 * b[0]) * t1 + c[0];
            result -= (x * inv_diameter + 0.5 - dilate).clamp(0.0, 1.0);
        }
    }
    result
}

/// Evaluate raw signed Slug coverage (`coverage * 0.5`, BEFORE `abs` or clamping) at cell UV `(u, v)`.
///
/// Positive values correspond to clockwise (TrueType) interiors; negative values correspond to
/// counter-clockwise interiors.
pub fn signed_coverage(
    curves: &[PackedCurve],
    u: f32,
    v: f32,
    inv_diameter_x: f32,
    inv_diameter_y: f32,
) -> f32 {
    let mut sum = 0.0_f32;
    for c in curves {
        let p0 = [unpack_u16(c.p0x) - u, unpack_u16(c.p0y) - v];
        let p1 = [unpack_u16(c.p1x) - u, unpack_u16(c.p1y) - v];
        let p2 = [unpack_u16(c.p2x) - u, unpack_u16(c.p2y) - v];
        sum += compute_coverage(inv_diameter_x, 0.0, p0, p1, p2);
        sum += compute_coverage(inv_diameter_y, 0.0, rot90(p0), rot90(p1), rot90(p2));
    }
    sum * 0.5
}

/// Rasterise a packed glyph into a `width × height` 8-bit grayscale image (top-to-bottom rows).
///
/// When `abs_winding` is `false`, uses `clamp(cov, 0, 1)` (the current `glyph_field_derived.wgsl`);
/// when `true`, uses `clamp(cov.abs(), 0, 1)` (the JS shader's non-zero winding rule).
pub fn rasterize_glyph(
    curves: &[PackedCurve],
    width: usize,
    height: usize,
    abs_winding: bool,
) -> Vec<u8> {
    let mut pixels = vec![0u8; width * height];
    let inv_dx = width as f32;
    let inv_dy = height as f32;
    for row in 0..height {
        // Image row 0 is the top of the cell (v near 1.0); row height-1 is the bottom (v near 0.0).
        let v = 1.0 - (row as f32 + 0.5) / height as f32;
        for col in 0..width {
            let u = (col as f32 + 0.5) / width as f32;
            let raw = signed_coverage(curves, u, v, inv_dx, inv_dy);
            let cov = if abs_winding {
                raw.abs().clamp(0.0, 1.0)
            } else {
                raw.clamp(0.0, 1.0)
            };
            pixels[row * width + col] = (cov * 255.0).round() as u8;
        }
    }
    pixels
}

/// Summary of a glyph's rasterised ink (for testing bold weight, italic slant, and winding).
#[derive(Clone, Copy, Debug)]
pub struct InkSummary {
    /// Mean coverage over the whole cell (`0.0..=1.0`).
    pub mean_coverage: f32,
    /// Fraction of pixels whose signed coverage was noticeably negative (`< -0.25`).
    pub negative_fraction: f32,
    /// Horizontal centre of ink near the top minus near the bottom (positive = leans right).
    pub lean: f32,
}

/// Measure ink area, negative-winding fraction, and horizontal lean of a glyph.
pub fn measure_ink(curves: &[PackedCurve], width: usize, height: usize) -> InkSummary {
    let inv_dx = width as f32;
    let inv_dy = height as f32;
    let mut sum_cov = 0.0_f64;
    let mut neg_pixels = 0usize;
    let mut row_centers: Vec<(usize, f64)> = Vec::new();

    for row in 0..height {
        let v = 1.0 - (row as f32 + 0.5) / height as f32;
        let mut row_w = 0.0_f64;
        let mut row_x = 0.0_f64;
        for col in 0..width {
            let u = (col as f32 + 0.5) / width as f32;
            let raw = signed_coverage(curves, u, v, inv_dx, inv_dy);
            if raw < -0.25 {
                neg_pixels += 1;
            }
            let cov = raw.abs().clamp(0.0, 1.0) as f64;
            sum_cov += cov;
            if cov > 0.1 {
                row_w += cov;
                row_x += cov * u as f64;
            }
        }
        if row_w > 0.0 {
            row_centers.push((row, row_x / row_w));
        }
    }

    let lean = if row_centers.len() >= 8 {
        // row_centers[0..4] are near the top (small row index); last 4 are near the foot.
        let top: f64 = row_centers[..4].iter().map(|&(_, x)| x).sum::<f64>() / 4.0;
        let foot: f64 = row_centers[row_centers.len() - 4..]
            .iter()
            .map(|&(_, x)| x)
            .sum::<f64>()
            / 4.0;
        (top - foot) as f32
    } else {
        0.0
    };

    let total = (width * height) as f64;
    InkSummary {
        mean_coverage: (sum_cov / total) as f32,
        negative_fraction: (neg_pixels as f64 / total) as f32,
        lean,
    }
}

/// Write an 8-bit grayscale image in binary PGM (`P5`) format.
pub fn write_pgm(path: &Path, width: usize, height: usize, pixels: &[u8]) -> std::io::Result<()> {
    assert_eq!(pixels.len(), width * height, "PGM pixel buffer length mismatch");
    let mut file = std::fs::File::create(path)?;
    write!(file, "P5\n{width} {height}\n255\n")?;
    file.write_all(pixels)?;
    Ok(())
}
