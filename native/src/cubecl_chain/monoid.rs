use cubecl::prelude::*;

use super::{
    F_CLUSTER_TRAILER, F_LEADER, F_NEWLINE, F_SURVIVOR, ITEM_DESC_BYTE_START, ITEM_DESC_STRIDE,
    P_GLYPHS, P_HEAD_LEN, P_MODE, P_NL, P_RESET, P_ROWS, P_SURVIVORS, P_TAIL_LEN, P_WRAP,
    PARTIAL_COUNT_STRIDE, SM_ADVANCE, SM_STRIDE, WRAP_BACK,
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
    pub(super) survivors: i32,
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
        survivors: 0,
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
    let bit_repr = v.to_bits();
    let mant = (bit_repr & 0x007F_FFFFu32) | 0x0080_0000u32;
    let biased = (bit_repr >> 23u32) & 0xFFu32;
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
pub(super) fn fixed_pair(fixed: u32, high_extent: &mut f32, tail: &mut f32) {
    // The scale constants are the exact powers 2^-16 and 2^-24 by bits.
    *high_extent = ((fixed >> 8u32) as f32) * f32::from_bits(0x3780_0000u32);
    *tail = ((fixed & 0xFFu32) as f32) * f32::from_bits(0x3380_0000u32);
}


/// scan_combine, transcribed line-for-line — the GENERAL form (spine-grade:
/// rhs may carry rows and a real head line, not just a leaf's).
#[cube]
pub(super) fn combine(lhs: &mut ChainElem, rhs: &ChainElem) {
    if rhs.reset != 0 {
        lhs.reset = 1;
        lhs.nl = rhs.nl;
        lhs.glyphs = rhs.glyphs;
        lhs.rows = rhs.rows;
        lhs.head_len = rhs.head_len;
        lhs.tail_len = rhs.tail_len;
        lhs.tail_adv = rhs.tail_adv;
        lhs.wrap = rhs.wrap;
        lhs.mode = rhs.mode;
        lhs.survivors += rhs.survivors;
    } else {
        lhs.wrap = rhs.wrap;
        lhs.mode = rhs.mode;
        if rhs.nl == 0 {
            lhs.tail_len += rhs.tail_len;
            lhs.tail_adv += rhs.tail_adv; // f32 per add — the oracle's chain
            if lhs.nl == 0 {
                lhs.head_len = lhs.tail_len;
            }
        } else {
            if lhs.nl == 0 {
                lhs.head_len += rhs.head_len;
                lhs.rows = rhs.rows;
            } else {
                lhs.rows += rows_for(lhs.tail_len + rhs.head_len, rhs.wrap, rhs.mode) + rhs.rows;
            }
            lhs.tail_len = rhs.tail_len;
            lhs.tail_adv = rhs.tail_adv;
        }
        lhs.nl += rhs.nl;
        lhs.glyphs += rhs.glyphs;
        lhs.survivors += rhs.survivors;
    }
}

/// leaf_of, transcribed: reset/wrap/mode always; the rest only for leaders.
/// `glyph_flags` is the PACKED flag array — one byte per byte position, four per u32
/// word (the chain consumes only F_LEADER/F_NEWLINE, both in the low byte;
/// the full flags live CPU-side for the renderer). glyph_flags.len() is WORDS; every
/// byte-count in the kernels multiplies by 4.
#[cube]
pub(super) fn flags_at(glyph_flags: &[u32], byte_index: usize) -> u32 {
    (glyph_flags[byte_index >> 2] >> (((byte_index & 3) * 8) as u32)) & 0xFF
}

#[cube]
pub(super) fn is_survivor(glyph_flag: u32) -> bool {
    (glyph_flag & F_SURVIVOR) != 0 && (glyph_flag & F_CLUSTER_TRAILER) == 0
}

#[cube]
pub(super) fn leaf_of(
    glyph_flags: &[u32],
    advance_widths: &[f32],
    wrap_width: i32,
    wrap_mode: i32,
    is_item_reset: i32,
    byte_index: usize,
) -> ChainElem {
    let mut element = identity();
    element.reset = is_item_reset;
    element.wrap = wrap_width;
    element.mode = wrap_mode;
    let glyph_flag = flags_at(glyph_flags, byte_index);
    if (glyph_flag & F_LEADER) != 0 {
        element.glyphs = 1;
        if is_survivor(glyph_flag) {
            element.survivors = 1;
        }
        if (glyph_flag & F_NEWLINE) != 0 {
            element.nl = 1;
        } else {
            element.head_len = 1;
            element.tail_len = 1;
            element.tail_adv = advance_widths[byte_index * SM_STRIDE + SM_ADVANCE];
        }
    }
    element
}

#[cube]
pub(super) fn p_load(partial_counts: &[u32], partial_metrics: &[f32], i: usize) -> ChainElem {
    let offset = i * PARTIAL_COUNT_STRIDE;
    ChainElem {
        reset: partial_counts[offset + P_RESET] as i32,
        nl: partial_counts[offset + P_NL] as i32,
        glyphs: partial_counts[offset + P_GLYPHS] as i32,
        rows: partial_counts[offset + P_ROWS] as i32,
        head_len: partial_counts[offset + P_HEAD_LEN] as i32,
        tail_len: partial_counts[offset + P_TAIL_LEN] as i32,
        wrap: partial_counts[offset + P_WRAP] as i32,
        mode: partial_counts[offset + P_MODE] as i32,
        survivors: partial_counts[offset + P_SURVIVORS] as i32,
        tail_adv: partial_metrics[i],
    }
}

#[cube]
pub(super) fn p_store(partial_counts: &mut [u32], partial_metrics: &mut [f32], i: usize, element: &ChainElem) {
    let offset = i * PARTIAL_COUNT_STRIDE;
    partial_counts[offset + P_RESET] = element.reset as u32;
    partial_counts[offset + P_NL] = element.nl as u32;
    partial_counts[offset + P_GLYPHS] = element.glyphs as u32;
    partial_counts[offset + P_ROWS] = element.rows as u32;
    partial_counts[offset + P_HEAD_LEN] = element.head_len as u32;
    partial_counts[offset + P_TAIL_LEN] = element.tail_len as u32;
    partial_counts[offset + P_WRAP] = element.wrap as u32;
    partial_counts[offset + P_MODE] = element.mode as u32;
    partial_counts[offset + P_SURVIVORS] = element.survivors as u32;
    partial_metrics[i] = element.tail_adv;
}

/// item_search_device: the largest item whose byte_start <= id.
#[cube]
pub(super) fn item_search(item_record_bounds: &[u32], item_count: usize, id: usize) -> usize {
    let mut low = 0usize;
    let mut high = item_count - 1;
    while low < high {
        let mid = (low + high + 1) >> 1;
        if (item_record_bounds[mid * 2] as usize) <= id {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    low
}

/// item_search_desc: binary search the largest item whose byte_start <= id
/// within the consolidated flat item descriptors buffer.
#[cube]
pub(super) fn item_search_desc(item_descriptors: &[u32], item_count: usize, id: usize) -> usize {
    let mut low = 0usize;
    let mut high = item_count - 1;
    while low < high {
        let mid = (low + high + 1) >> 1;
        if (item_descriptors[mid * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize) <= id {
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
    let bit_repr = v.to_bits();
    if (bit_repr & 0x80000000) != 0 {
        !bit_repr
    } else {
        bit_repr | 0x80000000
    }
}

/// key_to_float: its inverse (for derive_stride's readback of x_max).
#[cube]
pub(super) fn key_to_float(k: u32) -> f32 {
    let float_bits = if (k & 0x80000000) != 0 { k & 0x7FFFFFFF } else { !k };
    f32::from_bits(float_bits)
}

/// Load a monoid element from the shared tile arrays (the pc lane layout, i32).
#[cube]
pub(super) fn s_load(shared_counts: &[i32], shared_metrics: &[f32], i: usize) -> ChainElem {
    let offset = i * PARTIAL_COUNT_STRIDE;
    ChainElem {
        reset: shared_counts[offset + P_RESET],
        nl: shared_counts[offset + P_NL],
        glyphs: shared_counts[offset + P_GLYPHS],
        rows: shared_counts[offset + P_ROWS],
        head_len: shared_counts[offset + P_HEAD_LEN],
        tail_len: shared_counts[offset + P_TAIL_LEN],
        wrap: shared_counts[offset + P_WRAP],
        mode: shared_counts[offset + P_MODE],
        survivors: shared_counts[offset + P_SURVIVORS],
        tail_adv: shared_metrics[i],
    }
}

/// Store a monoid element into the shared tile arrays.
#[cube]
pub(super) fn s_store(shared_counts: &mut [i32], shared_metrics: &mut [f32], i: usize, element: &ChainElem) {
    let offset = i * PARTIAL_COUNT_STRIDE;
    shared_counts[offset + P_RESET] = element.reset;
    shared_counts[offset + P_NL] = element.nl;
    shared_counts[offset + P_GLYPHS] = element.glyphs;
    shared_counts[offset + P_ROWS] = element.rows;
    shared_counts[offset + P_HEAD_LEN] = element.head_len;
    shared_counts[offset + P_TAIL_LEN] = element.tail_len;
    shared_counts[offset + P_WRAP] = element.wrap;
    shared_counts[offset + P_MODE] = element.mode;
    shared_counts[offset + P_SURVIVORS] = element.survivors;
    shared_metrics[i] = element.tail_adv;
}
