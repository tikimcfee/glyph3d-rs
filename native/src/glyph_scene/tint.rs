//! The backdrop tint fold: mean linear ink color + coverage over a slice of
//! instances. Extracted from `glyph_scene.rs` in the 2026-09 code-shape
//! refactor — a pure move.

use super::{GlyphInstance, RenderSlot, BACKDROP_GAIN, GLYPH_CELL_AREA};

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

pub fn srgb_to_linear_table() -> &'static [f64; 256] {
    &SRGB_TO_LINEAR
}

static SRGB_TO_LINEAR: std::sync::LazyLock<[f64; 256]> =
    std::sync::LazyLock::new(|| std::array::from_fn(|k| (k as f64 / 255.0).powf(2.2)));

impl<'a> SegTintAccum<'a> {
    pub fn new(slot_ink: &'a [Option<[f32; 4]>]) -> Self {
        Self { sum: [0f64; 3], cells: 0, slot_ink }
    }

    #[inline(always)]
    pub fn from_parts(sum: [f64; 3], cells: usize, slot_ink: &'a [Option<[f32; 4]>]) -> Self {
        Self { sum, cells, slot_ink }
    }

    pub fn add(&mut self, instances: &[GlyphInstance]) {
        let table = &*SRGB_TO_LINEAR;
        let slot_ink = self.slot_ink;
        let mut s0 = self.sum[0];
        let mut s1 = self.sum[1];
        let mut s2 = self.sum[2];
        let mut cells = self.cells;
        for g in instances {
            let gid = g.glyph_id as usize;
            if gid < slot_ink.len() {
                if let Some(ink) = unsafe { slot_ink.get_unchecked(gid) } {
                    s0 += ink[0] as f64;
                    s1 += ink[1] as f64;
                    s2 += ink[2] as f64;
                    cells += 2;
                    continue;
                }
            }
            let c0 = (g.color & 0xFF) as usize;
            let c1 = ((g.color >> 8) & 0xFF) as usize;
            let c2 = ((g.color >> 16) & 0xFF) as usize;
            s0 += table[c0];
            s1 += table[c1];
            s2 += table[c2];
            cells += 1;
        }
        self.sum = [s0, s1, s2];
        self.cells = cells;
    }

    /// The endpoint's tint-stream form (note 23, E2b): (glyph_id, color)
    /// per slot in slot order — the SAME fold as `add` reads from the 48 B
    /// instances, so the tints are bit-identical whichever arena form fed
    /// the load. Slot order IS arena order (both are survivor order).
    pub fn add_tint(&mut self, pairs: &[u32]) {
        let table = &*SRGB_TO_LINEAR;
        let slot_ink = self.slot_ink;
        let mut s0 = self.sum[0];
        let mut s1 = self.sum[1];
        let mut s2 = self.sum[2];
        let mut cells = self.cells;
        for &[gi, color] in pairs.as_chunks::<2>().0 {
            let gid = gi as usize;
            if gid < slot_ink.len() {
                if let Some(ink) = unsafe { slot_ink.get_unchecked(gid) } {
                    s0 += ink[0] as f64;
                    s1 += ink[1] as f64;
                    s2 += ink[2] as f64;
                    cells += 2;
                    continue;
                }
            }
            let c0 = (color & 0xFF) as usize;
            let c1 = ((color >> 8) & 0xFF) as usize;
            let c2 = ((color >> 16) & 0xFF) as usize;
            s0 += table[c0];
            s1 += table[c1];
            s2 += table[c2];
            cells += 1;
        }
        self.sum = [s0, s1, s2];
        self.cells = cells;
    }

    /// Direct fold over 32 B RenderSlots (the pure-Rust mapped endpoint path).
    /// Extracts glyph_id and color directly from the mapped buffer with zero
    /// intermediate allocations.
    pub fn add_slots(&mut self, slots: &[RenderSlot]) {
        let table = &*SRGB_TO_LINEAR;
        let slot_ink = self.slot_ink;
        let mut s0 = self.sum[0];
        let mut s1 = self.sum[1];
        let mut s2 = self.sum[2];
        let mut cells = self.cells;
        for s in slots {
            let gid = s.glyph_id as usize;
            if gid < slot_ink.len() {
                if let Some(ink) = unsafe { slot_ink.get_unchecked(gid) } {
                    s0 += ink[0] as f64;
                    s1 += ink[1] as f64;
                    s2 += ink[2] as f64;
                    cells += 2;
                    continue;
                }
            }
            let c0 = (s.color & 0xFF) as usize;
            let c1 = ((s.color >> 8) & 0xFF) as usize;
            let c2 = ((s.color >> 16) & 0xFF) as usize;
            s0 += table[c0];
            s1 += table[c1];
            s2 += table[c2];
            cells += 1;
        }
        self.sum = [s0, s1, s2];
        self.cells = cells;
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
