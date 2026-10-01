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

/// The 32 B slot scatter — THE product tail (note 23, E2b). One thread
/// per byte, writing the 8-word slot at the global survivor ordinal
/// DIRECTLY at its final address in the buffer the renderer binds — no
/// window base, no rolling chunk, no hop, no copy. The dropped lanes
/// (row/col, flags, _pad) are proven dead readers by note 22's sweep
/// (the shader never reads them; pick/verbs ride the engine cache;
/// row/col stay fenced by the records tier). The tint stream rides
/// beside it: (glyph_id, color) per slot in slot order, the one
/// slot-derived readback — seg_tint's bit-exact fold input.
///
/// The scatter is pure streaming writes (slot + tint) — the extent fold
/// moved to `extent_fold` (2026-09-30): 960M per-leader global atomics onto
/// 52 KB of lanes cost 263ms of the scatter's 364ms at the flagship, while
/// the writes alone run at the copy floor. (E1's mask lesson stands: a
/// duplicate reducer is a MASK — exactly one folder per reduction, now
/// `extent_fold`.)
#[cube(launch_unchecked)]
pub(super) fn scatter_slots(
    fl: &[u32],
    wc: &[u32],
    ir: &[u32],
    lm: &[f32],
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
    tint: &mut [u32],
) {
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
        if gi[b] != 0u32 {
            let w = sv[b] as usize * 8;
            let color = if is_per_record[it] != 0u32 {
                pr_colors[(color_base[it] + wc[b]) as usize]
            } else {
                flat_colors[it]
            };
            // The tint stream: (glyph_id, color) per slot, slot order —
            // seg_tint's bit-exact input once no host arena exists (the
            // fold's order IS the arena's order, and both are sv order).
            let t = sv[b] as usize * 2;
            if t + 2 <= tint.len() {
                tint[t] = gi[b];
                tint[t + 1] = color;
            }
            // Whole-extent guard, not just the start: the 1-word dummy
            // `out` of the extents-only Instances form must discard
            // EVERY slot write, including the zeroth.
            if w + 8 <= out.len() {
                out[w] = x.to_bits();
                out[w + 1] = y.to_bits();
                out[w + 2] = z.to_bits();
                out[w + 3] = gi[b];
                out[w + 4] = color;
                out[w + 5] = groups[it];
                out[w + 6] = adv.to_bits();
                out[w + 7] = height.to_bits();
            }
        }
    }
}

/// THE SOLE EXTENT FOLDER (taken off the scatter, 2026-09-30): the scatter's
/// per-leader global atomics (~960M onto 52 KB of lanes) cost 263ms of its
/// 364ms at the flagship, while its writes alone run at the copy floor.
/// Min/max under the ordered key is EXACT and order-free, so any grouping
/// reproduces the bits — the fold runs at the tile_scan shape instead
/// (raked units, full occupancy): a thread accumulates its rake in
/// registers and flushes ONE atomic set per item-run (the item tracking
/// mirrors tile_scan's `nxt` walk). Page lanes over ALL records (the 0.0
/// seeds ride the local accumulators — folding the seed again is
/// idempotent), ink lanes over survivors (±inf), and the scatter's
/// arithmetic unchanged (a bare add, a power-of-two multiply —
/// contraction-neutral by construction).
#[cube(launch_unchecked)]
pub(super) fn extent_fold(
    fl: &[u32],
    lm: &[f32],
    sm: &[f32],
    hgt: &[f32],
    gi: &[u32],
    ir: &[u32],
    ext: &mut [Atomic<u32>],
    #[comptime] units: usize,
    #[comptime] rake: usize,
) {
    let tile = CUBE_POS;
    let u = UNIT_POS as usize;
    let n = fl.len() * 4; // packed: words -> bytes
    let item_count = ir.len() / 2;
    let lo = tile * (units * rake) + u * rake;
    let hi = if lo + rake < n { lo + rake } else { n };
    if lo < n && item_count > 0 {
        let mut it = item_search(ir, item_count, lo);
        let mut nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
        // The current item-run's accumulators. The any-flags keep
        // leaderless/survivorless runs silent; the seeds reproduce the
        // buffer's, so a flush is idempotent against them.
        let mut pg_rmax = f32::new(0.0f32);
        let mut pg_ymin = f32::new(0.0f32);
        let mut pg_zmin = f32::new(0.0f32);
        let mut pg_zmax = f32::new(0.0f32);
        // (f32::new of MAX literals, not INFINITY: the guard means these
        // sentinels never reach an atomic unfolded.)
        let mut ink_xmin = f32::new(3.4028235e38f32);
        let mut ink_ymin = f32::new(3.4028235e38f32);
        let mut ink_rmax = f32::new(-3.4028235e38f32);
        let mut ink_ymax = f32::new(-3.4028235e38f32);
        let mut ink_zmin = f32::new(3.4028235e38f32);
        let mut ink_zmax = f32::new(-3.4028235e38f32);
        let mut any_leader = false;
        let mut any_survivor = false;
        let mut id = lo;
        while id <= hi {
            if id == hi || nxt <= id {
                // The rake's end or an item boundary: flush the run.
                if any_leader {
                    let e = it * EXT_STRIDE;
                    ext[e].fetch_max(ordered_key(pg_rmax));
                    ext[e + 1].fetch_min(ordered_key(pg_ymin));
                    ext[e + 2].fetch_min(ordered_key(pg_zmin));
                    ext[e + 3].fetch_max(ordered_key(pg_zmax));
                    if any_survivor {
                        ext[e + 4].fetch_min(ordered_key(ink_xmin));
                        ext[e + 5].fetch_min(ordered_key(ink_ymin));
                        ext[e + 6].fetch_max(ordered_key(ink_rmax));
                        ext[e + 7].fetch_max(ordered_key(ink_ymax));
                        ext[e + 8].fetch_min(ordered_key(ink_zmin));
                        ext[e + 9].fetch_max(ordered_key(ink_zmax));
                    }
                    pg_rmax = f32::new(0.0f32);
                    pg_ymin = f32::new(0.0f32);
                    pg_zmin = f32::new(0.0f32);
                    pg_zmax = f32::new(0.0f32);
                    ink_xmin = f32::new(3.4028235e38f32);
                    ink_ymin = f32::new(3.4028235e38f32);
                    ink_rmax = f32::new(-3.4028235e38f32);
                    ink_ymax = f32::new(-3.4028235e38f32);
                    ink_zmin = f32::new(3.4028235e38f32);
                    ink_zmax = f32::new(-3.4028235e38f32);
                    any_leader = false;
                    any_survivor = false;
                }
                if id == hi {
                    break;
                }
                // Advance past the boundary — a WHILE, not an if: empty
                // items share their start with the next item, and a
                // single-step advance misassigns the boundary byte's leader
                // to the empty item's lanes (found by the repo-verify seam
                // on g-pick-repo's empty.rs — the fork fixture then had no
                // empty file; it does now).
                while nxt <= id {
                    it += 1;
                    nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
                }
            }
            if flags_at(fl, id) & F_LEADER != 0 {
                any_leader = true;
                let x = lm[id * LM_STRIDE + LM_X];
                let y = lm[id * LM_STRIDE + LM_Y];
                let z = lm[id * LM_STRIDE + LM_Z];
                let right = x + sm[id];
                if right > pg_rmax {
                    pg_rmax = right;
                }
                if y < pg_ymin {
                    pg_ymin = y;
                }
                if z < pg_zmin {
                    pg_zmin = z;
                }
                if z > pg_zmax {
                    pg_zmax = z;
                }
                if gi[id] != 0u32 {
                    any_survivor = true;
                    let half = hgt[id] * 0.5f32;
                    let y_lo = y - half;
                    let y_hi = y + half;
                    if x < ink_xmin {
                        ink_xmin = x;
                    }
                    if y_lo < ink_ymin {
                        ink_ymin = y_lo;
                    }
                    if right > ink_rmax {
                        ink_rmax = right;
                    }
                    if y_hi > ink_ymax {
                        ink_ymax = y_hi;
                    }
                    if z < ink_zmin {
                        ink_zmin = z;
                    }
                    if z > ink_zmax {
                        ink_zmax = z;
                    }
                }
            }
            id += 1;
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
