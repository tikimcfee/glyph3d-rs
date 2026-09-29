use cubecl::prelude::*;

use super::{
    F_LEADER, F_NEWLINE, P_GLYPHS, P_HEAD_LEN, P_MODE, P_NL, P_RESET, P_ROWS, P_TAIL_LEN,
    P_WRAP, PARTIAL_COUNT_STRIDE, SM_ADVANCE, SM_STRIDE, WRAP_BACK,
};

// ── the monoid, device-side ──────────────────────────────────────────────────

/// scan.rs's ScanElem, register-resident. i32 lanes where the CPU rides i64:
/// counts, every one — no lane approaches 2^31 inside a chunk.
#[derive(CubeType, Clone, Copy)]
pub(super) struct ChainElem {
    pub(super) reset: i32,
    pub(super) nl: i32,
    pub(super) glyphs: i32,
    pub(super) rows: i32,
    pub(super) head_len: i32,
    pub(super) tail_len: i32,
    pub(super) wrap: i32,
    pub(super) mode: i32,
    pub(super) tail_adv: f32,
}

#[cube]
pub(super) fn identity() -> ChainElem {
    ChainElem {
        reset: 0,
        nl: 0,
        glyphs: 0,
        rows: 0,
        head_len: 0,
        tail_len: 0,
        wrap: 0,
        mode: 0,
        tail_adv: 0.0,
    }
}

/// rows_for_line, transcribed (the phantom-row correction included).
#[cube]
pub(super) fn rows_for(length: i32, wrap: i32, mode: i32) -> i32 {
    if mode == WRAP_BACK || wrap <= 0 || length <= 0 {
        1
    } else {
        (length - 1) / wrap + 1
    }
}

/// wrap_segment_of, transcribed — mode-free depth fan.
#[cube]
pub(super) fn wrap_segment_of(col: i32, wrap: i32, terminator: bool) -> i32 {
    if wrap <= 0 {
        0
    } else if terminator {
        if col <= 0 {
            0
        } else {
            (col - 1) / wrap
        }
    } else {
        col / wrap
    }
}

/// wrap_row_of, transcribed — WRAP_DOWN: the segment index; WRAP_BACK: zero.
#[cube]
pub(super) fn wrap_row_of(col: i32, wrap: i32, terminator: bool, mode: i32) -> i32 {
    if mode == WRAP_BACK {
        0
    } else {
        wrap_segment_of(col, wrap, terminator)
    }
}

// ── double-single arithmetic ─────────────────────────────────────────────
// The engine's position lanes are f64 expressions narrowed ONCE at store
// (fold.rs's contract table): integer × param products are exact in f64,
// and only the store rounds. WGSL has no f64 — but every value these lanes
// hold is text-magnitude with multipliers bounded by pages_wide and wrap
// counts, so the exact results need well under 48 significant bits, and a
// two-f32 pair carries 48. Two-sums are exact regardless of association;
// Veltkamp-Dekker products are exact without FMA (cubecl's mul_add is
// RELAXED — it may lower to a*b+c — so the split-based product is used).
// Each write site folds the pair into ONE final add: the single rounding,
// like the engine's `as f32`. The repo-check census is the proof burden —
// the X (m ≥ 1) and Z (seg ≥ 3) deviation buckets must read ZERO.

/// Advance -> u32 fixed-point at scale 2^-24: the mantissa (with
/// implicit bit) shifted by the exponent. Every advance >= 0.5 converts
/// EXACTLY (its lowest mantissa bit lands on the grid); smaller ones
/// round below 2^-24, which nothing downstream can see. Integer adds
/// are then exact and reassociation-proof BY CONSTRUCTION — the
/// optimizer cannot break what has no rounding to break. (WGSL has no
/// int64 here — the scale is chosen so segment sums of up to ~2^31.6
/// fit u32: a hundred 4-unit advances is the ceiling, far past any
/// real wrap segment.) This is the final form of the extent walk:
/// every float error-extraction scheme measured dead on device with
/// faithful WGSL (landmine 9 and cousins), and the magnitude-split
/// pair that replaced them was unnormalized and cost 86M deviations.
#[cube]
pub(super) fn advance_fixed(v: f32) -> u32 {
    let b = v.to_bits();
    let mant = (b & 0x007F_FFFFu32) | 0x0080_0000u32;
    let biased = (b >> 23u32) & 0xFFu32;
    if biased >= 126u32 {
        mant << (biased - 126u32)
    } else {
        mant >> (126u32 - biased)
    }
}

/// The exact fixed-point sum as a (value, tail) pair: the top bits as
/// an exact f32, the low 8 fixed-point bits as a small tail. Both
/// conversions are exact; the pair sums to the true value. The tail is
/// coarse in stride terms (~2^-16), but the consumers fold it FIRST,
/// into the base's much finer grid, where it lands whole and the
/// dominant term's single fma rounding is all that remains.
#[cube]
pub(super) fn fixed_pair(fixed: u32, hi: &mut f32, tail: &mut f32) {
    // The scale constants are the exact powers 2^-16 and 2^-24 by bits.
    *hi = ((fixed >> 8u32) as f32) * f32::from_bits(0x3780_0000u32);
    *tail = ((fixed & 0xFFu32) as f32) * f32::from_bits(0x3380_0000u32);
}


/// scan_combine, transcribed line-for-line — the GENERAL form (spine-grade:
/// b may carry rows and a real head line, not just a leaf's).
#[cube]
pub(super) fn combine(a: &mut ChainElem, b: &ChainElem) {
    if b.reset != 0 {
        a.reset = 1;
        a.nl = b.nl;
        a.glyphs = b.glyphs;
        a.rows = b.rows;
        a.head_len = b.head_len;
        a.tail_len = b.tail_len;
        a.tail_adv = b.tail_adv;
        a.wrap = b.wrap;
        a.mode = b.mode;
    } else {
        a.wrap = b.wrap;
        a.mode = b.mode;
        if b.nl == 0 {
            a.tail_len += b.tail_len;
            a.tail_adv += b.tail_adv; // f32 per add — the oracle's chain
            if a.nl == 0 {
                a.head_len = a.tail_len;
            }
        } else {
            if a.nl == 0 {
                a.head_len += b.head_len;
                a.rows = b.rows;
            } else {
                a.rows += rows_for(a.tail_len + b.head_len, b.wrap, b.mode) + b.rows;
            }
            a.tail_len = b.tail_len;
            a.tail_adv = b.tail_adv;
        }
        a.nl += b.nl;
        a.glyphs += b.glyphs;
    }
}

/// leaf_of, transcribed: reset/wrap/mode always; the rest only for leaders.
/// `fl` is the PACKED flag array — one byte per byte position, four per u32
/// word (the chain consumes only F_LEADER/F_NEWLINE, both in the low byte;
/// the full flags live CPU-side for the renderer). fl.len() is WORDS; every
/// byte-count in the kernels multiplies by 4.
#[cube]
pub(super) fn flags_at(fl: &[u32], i: usize) -> u32 {
    (fl[i >> 2] >> (((i & 3) * 8) as u32)) & 0xFF
}

#[cube]
pub(super) fn leaf_of(fl: &[u32], sm: &[f32], wrap: i32, mode: i32, reset: i32, id: usize) -> ChainElem {
    let mut e = identity();
    e.reset = reset;
    e.wrap = wrap;
    e.mode = mode;
    let f = flags_at(fl, id);
    if (f & F_LEADER) != 0 {
        e.glyphs = 1;
        if (f & F_NEWLINE) != 0 {
            e.nl = 1;
        } else {
            e.head_len = 1;
            e.tail_len = 1;
            e.tail_adv = sm[id * SM_STRIDE + SM_ADVANCE];
        }
    }
    e
}

#[cube]
pub(super) fn p_load(pc: &[u32], pm: &[f32], i: usize) -> ChainElem {
    let o = i * PARTIAL_COUNT_STRIDE;
    ChainElem {
        reset: pc[o + P_RESET] as i32,
        nl: pc[o + P_NL] as i32,
        glyphs: pc[o + P_GLYPHS] as i32,
        rows: pc[o + P_ROWS] as i32,
        head_len: pc[o + P_HEAD_LEN] as i32,
        tail_len: pc[o + P_TAIL_LEN] as i32,
        wrap: pc[o + P_WRAP] as i32,
        mode: pc[o + P_MODE] as i32,
        tail_adv: pm[i],
    }
}

#[cube]
pub(super) fn p_store(pc: &mut [u32], pm: &mut [f32], i: usize, e: &ChainElem) {
    let o = i * PARTIAL_COUNT_STRIDE;
    pc[o + P_RESET] = e.reset as u32;
    pc[o + P_NL] = e.nl as u32;
    pc[o + P_GLYPHS] = e.glyphs as u32;
    pc[o + P_ROWS] = e.rows as u32;
    pc[o + P_HEAD_LEN] = e.head_len as u32;
    pc[o + P_TAIL_LEN] = e.tail_len as u32;
    pc[o + P_WRAP] = e.wrap as u32;
    pc[o + P_MODE] = e.mode as u32;
    pm[i] = e.tail_adv;
}

/// item_search_device: the largest item whose byte_start <= id.
#[cube]
pub(super) fn item_search(ir: &[u32], item_count: usize, id: usize) -> usize {
    let mut low = 0usize;
    let mut high = item_count - 1;
    while low < high {
        let mid = (low + high + 1) >> 1;
        if (ir[mid * 2] as usize) <= id {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    low
}

/// ordered_key: the monotonic float -> u32 map for the x_max atomic.
#[cube]
pub(super) fn ordered_key(v: f32) -> u32 {
    let b = v.to_bits();
    if (b & 0x80000000) != 0 {
        !b
    } else {
        b | 0x80000000
    }
}

/// key_to_float: its inverse (for derive_stride's readback of x_max).
#[cube]
pub(super) fn key_to_float(k: u32) -> f32 {
    let b = if (k & 0x80000000) != 0 { k & 0x7FFFFFFF } else { !k };
    f32::from_bits(b)
}

/// Load a monoid element from the shared tile arrays (the pc lane layout, i32).
#[cube]
pub(super) fn s_load(sc: &[i32], sf: &[f32], i: usize) -> ChainElem {
    let o = i * PARTIAL_COUNT_STRIDE;
    ChainElem {
        reset: sc[o + P_RESET],
        nl: sc[o + P_NL],
        glyphs: sc[o + P_GLYPHS],
        rows: sc[o + P_ROWS],
        head_len: sc[o + P_HEAD_LEN],
        tail_len: sc[o + P_TAIL_LEN],
        wrap: sc[o + P_WRAP],
        mode: sc[o + P_MODE],
        tail_adv: sf[i],
    }
}

/// Store a monoid element into the shared tile arrays.
#[cube]
pub(super) fn s_store(sc: &mut [i32], sf: &mut [f32], i: usize, e: &ChainElem) {
    let o = i * PARTIAL_COUNT_STRIDE;
    sc[o + P_RESET] = e.reset;
    sc[o + P_NL] = e.nl;
    sc[o + P_GLYPHS] = e.glyphs;
    sc[o + P_ROWS] = e.rows;
    sc[o + P_HEAD_LEN] = e.head_len;
    sc[o + P_TAIL_LEN] = e.tail_len;
    sc[o + P_WRAP] = e.wrap;
    sc[o + P_MODE] = e.mode;
    sf[i] = e.tail_adv;
}
