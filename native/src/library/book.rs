//! The Book carrier's arithmetic: the slot laws, the splay grid, the
//! contain-fit and the easing step — pure functions, ported from the retired
//! JS renderer's `collections/Book.js` (`_slotFor`, `splayGrid`, `_fitSheet`,
//! `update`). Kept in f64 because the JS computed in doubles and the tests
//! hold these to its numbers; the scene boundary casts to f32.

/// The splay's grid shape for `n` pages (`Book.splayGrid`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplayGrid {
    pub cols: usize,
    pub rows: usize,
    pub w: f64,
    pub h: f64,
}

/// The splay knobs plus the page they lay out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplayDims {
    /// Fixed column count; 0 derives it from `aspect`.
    pub cols: u32,
    pub aspect: f64,
    pub page_w: f64,
    pub page_h: f64,
    pub gap_x: f64,
    pub gap_y: f64,
}

/// JavaScript's `Math.round`: halves round toward +infinity (Rust's
/// `f64::round` rounds them away from zero, which differs below zero).
fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// Columns from the aspect target (grid w/h ≈ aspect) unless fixed, rows to
/// cover. The ONE home for the arithmetic: the slot law places by it and the
/// library sizes a splayed directory's footprint by it.
pub fn splay_grid(n: usize, d: &SplayDims) -> SplayGrid {
    let step_x = d.page_w + d.gap_x;
    let step_y = d.page_h + d.gap_y;
    let n1 = n.max(1) as f64;
    let mut c = if d.cols > 0 {
        d.cols as f64
    } else {
        js_round(((n1 * d.aspect * step_y) / step_x).sqrt())
    };
    c = c.max(1.0).min(n1);
    let cols = c as usize;
    let rows = n.div_ceil(cols).max(usize::from(n > 0));
    SplayGrid {
        cols,
        rows,
        w: (cols as f64 * step_x - d.gap_x).max(0.0),
        h: (rows as f64 * step_y - d.gap_y).max(0.0),
    }
}

/// The deck (rolodex) law: sheet `i` rests `slot = (order·(i − head)) mod n`
/// steps back along −z. `order` −1 is recency (an agent book), +1 page order
/// (a library volume: turned pages wrap to the back in turn order).
pub fn deck_slot(i: usize, head: usize, n: usize, order: i64, z_pitch: f64) -> [f64; 3] {
    if n == 0 {
        return [0.0; 3];
    }
    let head = head.min(n - 1) as i64;
    let n = n as i64;
    let slot = ((order * (i as i64 - head)) % n + n) % n;
    [0.0, 0.0, -(slot as f64) * z_pitch]
}

/// The splay law: page order in an m×n grid — columns centred on x, the
/// first row's page centres at y = 0 and rows descend; the head floats
/// `lift` forward as the visible bookmark.
pub fn splay_slot(i: usize, head: usize, n: usize, d: &SplayDims, lift: f64) -> [f64; 3] {
    let g = splay_grid(n, d);
    let col = (i % g.cols) as f64;
    let row = (i / g.cols) as f64;
    [
        (col - (g.cols as f64 - 1.0) / 2.0) * (d.page_w + d.gap_x),
        -row * (d.page_h + d.gap_y),
        if i == head { lift } else { 0.0 },
    ]
}

/// One side's contain-fit onto its page rect.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fit {
    /// Uniform scale on the mount (fit, never skew; capped at max_upscale).
    pub scale: f64,
    /// The mount's translation: the content box's centre carried to the
    /// page centre (the sheet's origin).
    pub mount: [f64; 3],
    pub content_w: f64,
    pub content_h: f64,
}

/// `Book._fitSheet` for a one-sided sheet (slotX = 0). A content box with no
/// extent at all (an empty file) fits as a point at the page centre with
/// scale 1, where the JS fits a non-finite box — the same neutral seat.
///
/// `front` chooses where the content's depth goes: false centres it on the
/// page plane (the JS: its content was flat), true puts the content's FRONT
/// (max z — the reading surface; HyperLayout's wrap staircase recedes into
/// −z from it) on the page plane, so the depth goes into the book instead of
/// standing out in front of every page (`[library] depth_align`).
pub fn contain_fit(min: [f32; 3], max: [f32; 3], page_w: f64, page_h: f64, max_upscale: f64, front: bool) -> Fit {
    let (lo, hi) = (min.map(f64::from), max.map(f64::from));
    let empty = !(hi[0] > lo[0] || hi[1] > lo[1]);
    let w = (hi[0] - lo[0]).max(1e-6);
    let h = (hi[1] - lo[1]).max(1e-6);
    if empty {
        return Fit { scale: 1.0, mount: [0.0; 3], content_w: w, content_h: h };
    }
    let s = (page_w / w).min(page_h / h).min(max_upscale);
    let z_anchor = if front { hi[2] } else { (lo[2] + hi[2]) / 2.0 };
    Fit {
        scale: s,
        mount: [-(lo[0] + hi[0]) / 2.0 * s, -(lo[1] + hi[1]) / 2.0 * s, -z_anchor * s],
        content_w: w,
        content_h: h,
    }
}

/// The easing factor for one frame: `1 − e^(−rate·dt)`, dt clamped to
/// [0, 0.1] s so a stalled frame does not launch the pages (`Book.update`).
/// A rate of 0 or less snaps.
pub fn ease_k(rate: f64, dt: f64) -> f64 {
    if rate > 0.0 {
        1.0 - (-rate * dt.clamp(0.0, 0.1)).exp()
    } else {
        1.0
    }
}

/// One node's ease toward its target. Returns the new position and whether
/// it moved; a node within `settle` (Chebyshev distance) snaps onto its
/// target and is settled.
pub fn ease_step(at: [f64; 3], target: [f64; 3], k: f64, settle: f64) -> ([f64; 3], bool) {
    let d = [target[0] - at[0], target[1] - at[1], target[2] - at[2]];
    let dist = d[0].abs().max(d[1].abs()).max(d[2].abs());
    if dist < settle {
        return (target, dist > 0.0);
    }
    ([at[0] + d[0] * k, at[1] + d[1] * k, at[2] + d[2] * k], true)
}
