use cubecl::prelude::*;

use super::monoid::{flags_at, item_search, ordered_key};
use super::{F_LEADER, LC_COL, LC_ROW, LC_STRIDE, LM_STRIDE, LM_X, LM_Y, LM_Z};

/// The record emitter — phase 4, rung 2. One thread per LEADER ORDINAL
/// over the otb compaction, gathering the per-byte lanes into the 32 B
/// wire record stream [X Y Z ADVANCE HEIGHT][GLYPH_ID ROW COL], in byte
/// order (= item order; otb is the global byte-order compaction and items
/// tile the corpus). The engine emits one record per leader INCLUDING
/// newlines, blanks, and missing codepoints — parity means the same
/// stream, so nothing is dropped here; consumers filter.
///
/// Tier contract (the chain-check records diff below): gi/row/col exact,
/// height/advance bit-exact (their producing lanes are already
/// bit-fenced elsewhere in the check); X/Y/Z ride the position eps tiers.
/// The fold>0 X bit-tier stays in the byte-indexed lane diff — THIS diff
/// witnesses the GATHER, and the gather's own failure mode is a wrong
/// byte or order, which the ordinal/byte identity check catches exactly
/// (the reference's own ord_to_byte is the independent compaction).
#[cube(launch_unchecked)]
pub(super) fn emit_records(
    fl: &[u32],
    wc: &[u32],
    ir: &[u32],
    base: &[u32],
    lm: &[f32],
    lc: &[u32],
    sm: &[f32],
    hgt: &[f32],
    gi: &[u32],
    recs: &mut [u32],
    win: &[u32],
) {
    // The window base rides a 1-element params BUFFER, not a comptime
    // scalar: all windows reuse the ONE compiled kernel (a comptime
    // rec_first cost a fresh JIT specialization per window — ~3.5s cold
    // across six on the 97MB shape), and a buffer binds positionally like
    // every other slice, clear of the scalar/info region where landmine 7
    // lives. 0usize: bare int literals fail expansion (landmine 10).
    let rec_first = win[0usize] as usize;
    let b = ABSOLUTE_POS;
    let n = wc.len();
    let item_count = ir.len() / 2;
    if b < n && item_count > 0 && flags_at(fl, b) & F_LEADER != 0 {
            // wc is the FORWARD ordinal map (apply writes it on every
            // path): this leader's item-relative ordinal. base is the
            // per-item record offset — together the record stream is
            // item order, ordinal order within items, exactly the
            // engine's emission order. rec_first windows this launch at
            // one CHUNK of the stream — the record buffer stays a fixed
            // rolling slice instead of a whole-corpus allocation (the
            // 97MB repo shape's 3.1GB single buffer was what pushed the
            // instrument past the machine's memory ceiling).
            let it = item_search(ir, item_count, b);
            let o = base[it] + wc[b];
            if o as usize >= rec_first && (o as usize - rec_first) < recs.len() / 8 {
                    let w = (o as usize - rec_first) * 8;
                recs[w] = lm[b * LM_STRIDE + LM_X].to_bits();
                recs[w + 1] = lm[b * LM_STRIDE + LM_Y].to_bits();
                recs[w + 2] = lm[b * LM_STRIDE + LM_Z].to_bits();
                recs[w + 3] = sm[b].to_bits();
                recs[w + 4] = hgt[b].to_bits();
                recs[w + 5] = gi[b];
                recs[w + 6] = lc[b * LC_STRIDE + LC_ROW];
                recs[w + 7] = lc[b * LC_STRIDE + LC_COL];
            }
    }
}

// ── rung 5b: the instance tail ───────────────────────────────────────────────
//
// The survivor pass and the pack kernel — the device replacement for
// `compact_records_into` (layout.rs). The survivor ordinals come from the
// SAME integer scan machinery the cluster counter uses (count_tile /
// count_spine on a plain byte flag — note 13's shape: a separate pass,
// never a second counter inside the proven monoid). The packer writes the
// 48 B GlyphInstance wire form and folds the extents with per-item atomics
// over `ordered_key` — min/max is order-free, so the reduction is
// deterministic however the atomics interleave, and its lanes are
// bit-identical to the host loop's by construction: `right = x + advance`
// is a bare add (nothing to contract), and the half-height lanes multiply
// by 0.5 — a power of two, exact — so even a forced fma contraction rounds
// identically to the host's two-step form.

/// Extent lanes per item in the `ext` buffer. Order matches the decode in
/// run_repo_chain's tail: page right/bottom/z_min/z_max over ALL records,
/// then ink min-xyz / max-xyz over survivors.
pub(super) const EXT_STRIDE: usize = 10;

/// The HOST twins of ordered_key / key_to_float — they seed the extent
/// lanes and decode their readback, so they must agree bit for bit with
/// the #[cube] pair (unit-tested together).
pub(super) fn ordered_key_host(v: f32) -> u32 {
    let b = v.to_bits();
    if (b & 0x8000_0000) != 0 {
        !b
    } else {
        b | 0x8000_0000
    }
}

pub(super) fn key_to_float_host(k: u32) -> f32 {
    let b = if (k & 0x8000_0000) != 0 {
        k & 0x7FFF_FFFF
    } else {
        !k
    };
    f32::from_bits(b)
}

/// Unpack the leader flag and AND it with glyph-id-resolved into two plain
/// u32 byte flags — the predicate inputs count_tile/count_spine already
/// run on (any nonzero byte counts).
#[cube(launch_unchecked)]
pub(super) fn survivor_flags(fl: &[u32], gi: &[u32], lflag: &mut [u32], sflag: &mut [u32]) {
    let b = ABSOLUTE_POS;
    let n = lflag.len();
    if b < n {
        let lead = if flags_at(fl, b) & F_LEADER != 0u32 { 1u32 } else { 0u32 };
        let surv = if lead != 0u32 && gi[b] != 0u32 { 1u32 } else { 0u32 };
        lflag[b] = lead;
        sflag[b] = surv;
    }
}

/// Per-byte EXCLUSIVE leader/survivor ordinals, written at EVERY byte (not
/// just leaders) so item-boundary reads are well-defined everywhere:
/// `lv[b]` = leaders strictly before b, `sv[b]` = survivors strictly
/// before b. Global byte order is walk order, which IS the arena's slot
/// order — so `sv[b]` is a survivor's global slot index, and lv at an
/// item's start is
/// that item's record base. The flag buffers are consumed and overwritten
/// in the same serial walk (flags read before ordinals written per slot),
/// which is why lv/sv may alias lflag/sflag.
#[cube(launch_unchecked)]
pub(super) fn ordinal_scatter(
    lflag: &[u32],
    sflag: &[u32],
    lxc: &[u32],
    sxc: &[u32],
    lup: &[u32],
    sup: &[u32],
    lv: &mut [u32],
    sv: &mut [u32],
    #[comptime] units: usize,
    #[comptime] rake: usize,
) {
    let tile = CUBE_POS;
    let u = UNIT_POS as usize;
    let n = lflag.len();
    let lo = tile * (units * rake) + u * rake;
    let hi = if lo + rake < n { lo + rake } else { n };
    let mut cl = lxc[tile] + lup[tile * units + u];
    let mut cs = sxc[tile] + sup[tile * units + u];
    if lo < n {
        let mut id = lo;
        while id < hi {
            let fl_ = lflag[id];
            let fs_ = sflag[id];
            lv[id] = cl;
            sv[id] = cs;
            cl += fl_;
            cs += fs_;
            id += 1usize;
        }
    }
}

/// Per-item leader and survivor totals from the boundary ordinals:
/// `tot[it]` = `lv[item_end]` − `lv[item_start]`. An end at the corpus edge
/// (the last item, or an empty file parked there) closes on the grand
/// totals from the spines — `lv[n]` is the pad region and unwritten.
#[cube(launch_unchecked)]
pub(super) fn item_totals(
    ir: &[u32],
    lv: &[u32],
    sv: &[u32],
    ltot: &mut [u32],
    stot: &mut [u32],
    lgrand: &[u32],
    sgrand: &[u32],
) {
    let it = ABSOLUTE_POS;
    let item_count = ir.len() / 2;
    if it < item_count {
        let n = lv.len();
        let s = ir[it * 2] as usize;
        let e = ir[it * 2 + 1] as usize;
        let ls = if s < n { lv[s] } else { lgrand[0] };
        let le = if e < n { lv[e] } else { lgrand[0] };
        ltot[it] = le - ls;
        let ss = if s < n { sv[s] } else { sgrand[0] };
        let se = if e < n { sv[e] } else { sgrand[0] };
        stot[it] = se - ss;
    }
}

/// The instance packer. One thread per byte; every LEADER folds the page
/// extents (seeds 0.0, over ALL records — blanks carry extents but no
/// slot, exactly like the host loop); every SURVIVOR (leader AND gi != 0)
/// additionally writes its 48 B slot at the global survivor ordinal and
/// folds the ink extents (seeds ±inf). Paint arrives as the two tables of
/// `InstanceInputs` — jagged per-record colors indexed by the record
/// ordinal `rbase[it] + wc[b]` (the SAME index emit_records gathers by),
/// or the per-item flat color. The window base rides the params buffer
/// (one compiled kernel — the 5b0 pattern); extent folds are gated to
/// window zero because atomics over the same keys are idempotent and
/// re-folding per window is pure waste.
#[cube(launch_unchecked)]
pub(super) fn pack_instances(
    fl: &[u32],
    wc: &[u32],
    ir: &[u32],
    lm: &[f32],
    lc: &[u32],
    sm: &[f32],
    hgt: &[f32],
    gi: &[u32],
    pr_colors: &[u32],
    color_base: &[u32],
    is_per_record: &[u32],
    flat_colors: &[u32],
    groups: &[u32],
    sv: &[u32],
    out: &mut [u32],
    ext: &mut [Atomic<u32>],
    win: &[u32],
) {
    let win_first = win[0usize] as usize;
    let b = ABSOLUTE_POS;
    let n = wc.len();
    let item_count = ir.len() / 2;
    if b < n && item_count > 0 && flags_at(fl, b) & F_LEADER != 0 {
        let it = item_search(ir, item_count, b);
        let x = lm[b * LM_STRIDE + LM_X];
        let y = lm[b * LM_STRIDE + LM_Y];
        let z = lm[b * LM_STRIDE + LM_Z];
        let adv = sm[b];
        let height = hgt[b];
        // The host loop's own arithmetic: a bare add (nothing to contract)
        // and a multiply by 0.5 — exact, so contraction is bit-neutral.
        let right = x + adv;
        let half = height * 0.5f32;
        if win_first == 0 {
            let e = it * EXT_STRIDE;
            ext[e].fetch_max(ordered_key(right));
            ext[e + 1].fetch_min(ordered_key(y));
            ext[e + 2].fetch_min(ordered_key(z));
            ext[e + 3].fetch_max(ordered_key(z));
        }
        if gi[b] != 0u32 {
            let slot = sv[b] as usize;
            if slot >= win_first && (slot - win_first) < out.len() / 12 {
                let w = (slot - win_first) * 12;
                out[w] = x.to_bits();
                out[w + 1] = y.to_bits();
                out[w + 2] = z.to_bits();
                out[w + 3] = gi[b];
                out[w + 4] = lc[b * LC_STRIDE + LC_ROW];
                out[w + 5] = lc[b * LC_STRIDE + LC_COL];
                out[w + 6] = if is_per_record[it] != 0u32 {
                    pr_colors[(color_base[it] + wc[b]) as usize]
                } else {
                    flat_colors[it]
                };
                out[w + 7] = groups[it];
                out[w + 8] = adv.to_bits();
                out[w + 9] = height.to_bits();
                // flags: the wire record carries none; the shader reads mode
                // from the glyphmap. _pad: zero.
                out[w + 10] = 0u32;
                out[w + 11] = 0u32;
            }
            if win_first == 0 {
                let e = it * EXT_STRIDE;
                ext[e + 4].fetch_min(ordered_key(x));
                ext[e + 5].fetch_min(ordered_key(y - half));
                ext[e + 6].fetch_max(ordered_key(right));
                ext[e + 7].fetch_max(ordered_key(y + half));
                ext[e + 8].fetch_min(ordered_key(z));
                ext[e + 9].fetch_max(ordered_key(z));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// The extent lanes' key encoding — the HOST half that seeds the lanes
    /// and decodes their readback. Roundtrip (the seeds decode back to the
    /// floats that made them) and MONOTONICITY (the per-item
    /// fetch_max/fetch_min reductions are only correct if the key preserves
    /// float order — that property, not the bit pattern, is what makes the
    /// order-free atomics deterministic). The #[cube] twin is the same bit
    /// logic; the fork gate's instance and placement tiers fence the
    /// device reduction end to end.
    #[test]
    fn ordered_key_host_roundtrip_and_monotonic() {
        let vals = [
            0.0f32,
            -0.0,
            1.0,
            -1.0,
            0.5,
            -0.5,
            3.178_448_2e0,
            -1.337_5e2,
            f32::MIN_POSITIVE,
            -f32::MIN_POSITIVE,
            f32::MAX,
            f32::MIN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ];
        for &v in &vals {
            assert_eq!(super::key_to_float_host(super::ordered_key_host(v)), v);
        }
        let mut sorted = vals;
        sorted.sort_by(f32::total_cmp);
        for w in sorted.windows(2) {
            assert!(super::ordered_key_host(w[0]) <= super::ordered_key_host(w[1]));
        }
    }
}
