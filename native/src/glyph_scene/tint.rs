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
    let mut acc = SegTintAccum::new(slot_ink);
    acc.add(instances);
    acc.finish(instances.len(), width, height)
}

/// The fold's accumulator, exposed so a file's slot range can cross arena
/// chunk boundaries without changing the arithmetic: the chunk slices fold
/// in order into ONE accumulator, which reproduces the contiguous fold's
/// exact addition sequence. f64 sums are order-sensitive — do NOT fold
/// chunks into partial tints and merge; carry the accumulator.
pub struct SegTintAccum<'a> {
    sum: [f64; 3],
    cells: usize,
    slot_ink: &'a [Option<[f32; 4]>],
}

impl<'a> SegTintAccum<'a> {
    pub fn new(slot_ink: &'a [Option<[f32; 4]>]) -> Self {
        Self { sum: [0f64; 3], cells: 0, slot_ink }
    }

    pub fn add(&mut self, instances: &[GlyphInstance]) {
        static SRGB_TO_LINEAR: std::sync::LazyLock<[f64; 256]> =
            std::sync::LazyLock::new(|| std::array::from_fn(|k| (k as f64 / 255.0).powf(2.2)));
        for g in instances {
            if let Some(Some(ink)) = self.slot_ink.get(g.glyph_id as usize) {
                for (i, s) in self.sum.iter_mut().enumerate() {
                    *s += ink[i] as f64;
                }
                self.cells += 2;
                continue;
            }
            // Match the shader's decode: sRGB display bytes → linear via pow
            // 2.2. Memoized over the whole domain — the input is a BYTE, so
            // 256 table entries replace 3 powf per glyph (285 M calls on the
            // glyph3d-js repo load). The table is built with the same f64
            // powf on the same values and the sums keep their order, so the
            // tint is bit-identical.
            for (i, s) in self.sum.iter_mut().enumerate() {
                *s += SRGB_TO_LINEAR[((g.color >> (8 * i)) & 0xFF) as usize];
            }
            self.cells += 1;
        }
    }

    /// The endpoint's tint-stream form (note 23, E2b): (glyph_id, color)
    /// per slot in slot order — the SAME fold as `add` reads from the 48 B
    /// instances, so the tints are bit-identical whichever arena form fed
    /// the load. Slot order IS arena order (both are survivor order).
    pub fn add_tint(&mut self, pairs: &[u32]) {
        static SRGB_TO_LINEAR: std::sync::LazyLock<[f64; 256]> =
            std::sync::LazyLock::new(|| std::array::from_fn(|k| (k as f64 / 255.0).powf(2.2)));
        for &[gi, color] in pairs.as_chunks::<2>().0 {
            if let Some(Some(ink)) = self.slot_ink.get(gi as usize) {
                for (i, s) in self.sum.iter_mut().enumerate() {
                    *s += ink[i] as f64;
                }
                self.cells += 2;
                continue;
            }
            for (i, s) in self.sum.iter_mut().enumerate() {
                *s += SRGB_TO_LINEAR[((color >> (8 * i)) & 0xFF) as usize];
            }
            self.cells += 1;
        }
    }

    /// `n` is the range's instance count (the divisor), `width × height` the
    /// file's world rect.
    pub fn finish(self, n: usize, width: f32, height: f32) -> [f32; 4] {
        let n = n.max(1) as f64;
        let area = (width as f64 * height as f64).max(1e-3);
        let ink_frac = (self.cells as f64 * GLYPH_CELL_AREA as f64 / area).min(1.0);
        let e = (ink_frac * BACKDROP_GAIN as f64).min(1.0);
        [
            (self.sum[0] / n) as f32,
            (self.sum[1] / n) as f32,
            (self.sum[2] / n) as f32,
            e as f32,
        ]
    }
}
