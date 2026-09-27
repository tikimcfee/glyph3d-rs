//! The backdrop tint fold: mean linear ink color + coverage over a slice of
//! instances. Extracted from `glyph_scene.rs` in the 2026-09 code-shape
//! refactor — a pure move.

use super::{GlyphInstance, BACKDROP_GAIN, GLYPH_CELL_AREA};

/// Stage F — mean linear ink color + backdrop coverage for a slice of
/// instances occupying a `width × height` world rect. Shared by the repo
/// per-file segments and the single-segment text scenes.
///
/// `slot_ink` (`Atlas::slot_ink`) says what a BITMAP slot's pixels average
/// to; an emoji contributes that instead of its instance colour — which is
/// the syntax colour, a colour it does not display — and counts as the two
/// cells its advance covers. Outline glyphs are summed exactly as before, so
/// a segment without emoji tints to the same bits (the goldens hold).
pub fn seg_tint(instances: &[GlyphInstance], width: f32, height: f32, slot_ink: &[Option<[f32; 4]>]) -> [f32; 4] {
    static SRGB_TO_LINEAR: std::sync::LazyLock<[f64; 256]> =
        std::sync::LazyLock::new(|| std::array::from_fn(|k| (k as f64 / 255.0).powf(2.2)));
    let mut sum = [0f64; 3];
    let mut cells = 0usize;
    for g in instances {
        if let Some(Some(ink)) = slot_ink.get(g.glyph_id as usize) {
            for (i, s) in sum.iter_mut().enumerate() {
                *s += ink[i] as f64;
            }
            cells += 2;
            continue;
        }
        // Match the shader's decode: sRGB display bytes → linear via pow 2.2.
        // Memoized over the whole domain — the input is a BYTE, so 256 table
        // entries replace 3 powf per glyph (285 M calls on the glyph3d-js repo
        // load). The table is built with the same f64 powf on the same values
        // and the sums keep their order, so the tint is bit-identical.
        for (i, s) in sum.iter_mut().enumerate() {
            *s += SRGB_TO_LINEAR[((g.color >> (8 * i)) & 0xFF) as usize];
        }
        cells += 1;
    }
    let n = instances.len().max(1) as f64;
    let area = (width as f64 * height as f64).max(1e-3);
    let ink_frac = (cells as f64 * GLYPH_CELL_AREA as f64 / area).min(1.0);
    let e = (ink_frac * BACKDROP_GAIN as f64).min(1.0);
    [
        (sum[0] / n) as f32,
        (sum[1] / n) as f32,
        (sum[2] / n) as f32,
        e as f32,
    ]
}
