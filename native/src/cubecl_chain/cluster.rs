use cubecl::prelude::*;

use super::decode::byte_at;
use super::monoid::item_search;
use super::{F_CLUSTER_TRAILER, F_LEADER};

// ── the cluster pass, phase 3b: probe (parallel) + chain (per item) ───────────
//
// Re-derived scan-shaped per the design session: the PROBE is pure per byte
// (its outcome depends only on the byte's own decode, forward bytes, the
// tables, and the item end — verified against resolve_clusters), so it runs
// one thread per WORD like decode. The CONSUMPTION is serial per item with
// a single integer of state (the resume pointer) — the brief's counterexample
// killed the naive max-scan: with W=[0,10), Y=[5,15), X=[12,14) the prefix-
// max sees Y's 15 and suppresses X, which greedy commits. v1 runs the chain
// one thread per ITEM (the product corpus is 1,306 items — item-parallel);
// the chunked function-composition form (the Mojo zone tables) is the
// follow-up only if a dense-single-item measurement demands it.
//
// Lookup is DESCENDING-LENGTH BINARY SEARCH over the sorted sequence
// section (the section order is asserted at bake; the longest exact prefix
// is unique) — semantics-neutral vs the Mojo st_probe hash by that same
// argument, and it reuses the item_search pattern already proven here. The
// comparator re-walks the probe's effective codepoints from the head byte
// instead of storing a key array: the walk is deterministic, so no local
// storage exists to spill.
//
// GAP-BYTE GUARD: the CPU walks only [start, stop); item_search attributes
// bytes to the largest start <= id WITHOUT an end check, so every per-byte
// test here carries its own id < stop guard — no fixture pins this (items
// tile every fixture's blob), the landmine list does.

/// sequence_length at i: the lenient classifier over the packed corpus.
#[cube]
pub(super) fn seq_len_at(bytes: &[u32], i: usize, n: usize) -> u32 {
    let b = byte_at(bytes, i, n);
    if b & 0x80u32 == 0u32 {
        1u32
    } else if b & 0xE0u32 == 0xC0u32 {
        2u32
    } else if b & 0xF0u32 == 0xE0u32 {
        3u32
    } else if b & 0xF8u32 == 0xF0u32 {
        4u32
    } else {
        0u32
    }
}

/// decode_codepoint_at at i for a known length.
#[cube]
pub(super) fn cp_at(bytes: &[u32], i: usize, len: u32, n: usize) -> u32 {
    let b = byte_at(bytes, i, n);
    let b1 = byte_at(bytes, i + 1, n);
    let b2 = byte_at(bytes, i + 2, n);
    let b3 = byte_at(bytes, i + 3, n);
    if len == 1u32 {
        b
    } else if len == 2u32 {
        ((b & 0x1Fu32) << 6u32) | (b1 & 0x3Fu32)
    } else if len == 3u32 {
        ((b & 0x0Fu32) << 12u32) | ((b1 & 0x3Fu32) << 6u32) | (b2 & 0x3Fu32)
    } else {
        ((b & 0x07u32) << 18u32) | ((b1 & 0x3Fu32) << 12u32) | ((b2 & 0x3Fu32) << 6u32) | (b3 & 0x3Fu32)
    }
}

/// is_static_zero_cp: ZWJ, the variation selectors, the tag characters.
#[cube]
// clippy:manual-range-contains allowed here — the cube macro has no
// RangeInclusive::contains expansion, and this is the spelled-out form it
// takes on device.
#[allow(clippy::manual_range_contains)]
pub(super) fn is_static_zero(cp: u32) -> u32 {
    if cp == 0x200Du32 || (cp >= 0xFE00u32 && cp <= 0xFE0Fu32) || (cp >= 0xE0020u32 && cp <= 0xE007Fu32) {
        1u32
    } else {
        0u32
    }
}


/// The cluster PROBE: thread per word. Static-zero bytes of cluster items
/// are marked here (unconditionally — match-independent, fold.rs:543-549);
/// candidates build their EFFECTIVE key into a per-thread LOCAL array (the
/// head's own codepoint first, FE0F skipped but riding, newline/VS15/
/// continuation/item-end breaking) and run a descending-length binary
/// search over the sorted sequence section — the longest exact prefix is
/// unique, so this is answer-identical to the CPU's linear scan and the
/// Mojo's hash probe alike. The span end re-walks counting CONSUMED key
/// elements, so trailing FE0Fs past the last consumer stay outside.
///
/// The key scratch is a LOCAL `Array`, not `Shared`: each unit only ever
/// touches its own row, so workgroup memory was never doing inter-thread
/// work — but its allocation (WGSL zero-initializes `var<workgroup>`) cost
/// every cube 8KB of memset whether or not any candidate ran. Measured
/// 2026-09-27: 63.9ms of probe time on a 24MB text corpus whose bytes all
/// bitmap-reject — 2.7µs/cube of pure scratch init.
///
/// All of this lives INLINE in the kernel because the walk/search shapes
/// only compile in kernel context — loops in HELPERS break the macro's
/// assign typing (recorded landmine; the deleted helper drafts are in the
/// commit history).
#[cube(launch_unchecked)]
pub(super) fn cluster_probe(
    bytes: &[u32],
    bitmap: &[u32],
    sec_off: &[u32],
    sec_val: &[u32],
    seq: &[u32],
    ir: &[u32],
    ic: &[u32],
    fl: &mut [u32],
    sm: &mut [f32],
    gi: &mut [u32],
    cslot: &mut [u32],
    cend: &mut [u32],
    #[comptime] seq_max: u32,
) {
    let w = ABSOLUTE_POS;
    let n = bytes.len() * 4;
    let item_count = ir.len() / 2;
    if w < fl.len() {
        let mut word = fl[w];
        let mut lane = 0usize;
        while lane < 4 {
            let id = w * 4 + lane;
            if id < n {
                let len = seq_len_at(bytes, id, n);
                if len > 0u32 {
                    let mut start = 0usize;
                    let mut stop = 0usize;
                    let mut cluster = false;
                    if item_count > 0 {
                        let it = item_search(ir, item_count, id);
                        start = ir[it * 2] as usize;
                        stop = ir[it * 2 + 1] as usize;
                        cluster = ic[it] != 0;
                    }
                    // The gap-byte guard: only bytes INSIDE the item range.
                    // BOTH edges are load-bearing — item_search clamps to
                    // item 0 for bytes BEFORE the first item (a leading gap
                    // would otherwise take item 0's static-zero marking,
                    // which the CPU never applies there).
                    if cluster && id >= start && id < stop {
                        let cp = cp_at(bytes, id, len, n);
                        if is_static_zero(cp) != 0u32 {
                            // fold.rs:543-548 zeroes ALL THREE static lanes
                            // for a static-zero byte in a cluster item — gi
                            // included. The probe wrote two of them until
                            // the repo parity driver caught the third.
                            sm[id] = f32::from_bits(0u32);
                            gi[id] = 0u32;
                            word |= F_CLUSTER_TRAILER << ((lane as u32) * 8u32);
                        } else {
                            // cp above 0x10FFFF is malformed decode — the
                            // bitmap covers real codepoints only; reject
                            // here rather than lean on the backend's
                            // OOB-read-is-zero (decode guards this class
                            // itself).
                            let mut bit = 0u32;
                            if cp <= 0x10FFFFu32 {
                                bit = (bitmap[(cp >> 5u32) as usize] >> (cp & 0x1Fu32)) & 1u32;
                            }
                            if bit != 0u32 {
                                // Pair filter: every reachable table entry
                                // has effective length >= 2 (the matcher's
                                // own guard), so a candidate whose SECOND
                                // effective element cannot follow its first
                                // in ANY entry can never match. Walk just
                                // far enough for that second element —
                                // skipping FE0F riders, stopping at the same
                                // breakers as the key walk — then one small
                                // binary search over this first's seconds.
                                // On source text this is where the keycap
                                // digits/#/* die: a few loads instead of a
                                // full key walk plus up to seven table
                                // searches (measured 42.6ms of the 24MB
                                // text probe before it).
                                let mut q = id + len as usize;
                                let mut second = 0u32;
                                let mut hunting = 1u32;
                                while hunting == 1u32 && q < stop {
                                    let l2 = seq_len_at(bytes, q, n);
                                    let c2 = cp_at(bytes, q, l2, n);
                                    let dead2 = if l2 == 0u32 || c2 == 0x0Au32 || c2 == 0xFE0Eu32 {
                                        1u32
                                    } else {
                                        0u32
                                    };
                                    if dead2 == 1u32 {
                                        hunting = 0u32;
                                    }
                                    if dead2 == 0u32 {
                                        if c2 != 0xFE0Fu32 {
                                            second = c2;
                                            hunting = 0u32;
                                        }
                                        q += l2 as usize;
                                    }
                                }
                                let mut pair_alive = 0u32;
                                if second != 0u32 {
                                    let plo = sec_off[cp as usize];
                                    let phi = sec_off[cp as usize + 1usize];
                                    let mut lo2 = plo;
                                    let mut hi2 = phi;
                                    while lo2 < hi2 {
                                        let mid = (lo2 + hi2) / 2u32;
                                        if sec_val[mid as usize] < second {
                                            lo2 = mid + 1u32;
                                        }
                                        if sec_val[mid as usize] >= second {
                                            hi2 = mid;
                                        }
                                    }
                                    if lo2 < phi && sec_val[lo2 as usize] == second {
                                        pair_alive = 1u32;
                                    }
                                }
                                if pair_alive == 1u32 {
                                // Key build: the head's own cp is element 0.
                                // The scratch is LOCAL to this thread —
                                // declared here so only candidates pay for it.
                                let mut skey = Array::<u32>::new(seq_max as usize);
                                let mut klen = 0u32;
                                let mut p = id;
                                let mut alive = 1u32;
                                while alive == 1u32 && p < stop && klen < seq_max {
                                    let len2 = seq_len_at(bytes, p, n);
                                    let cp2 = cp_at(bytes, p, len2, n);
                                    let dead = if len2 == 0u32 || cp2 == 0x0Au32 || cp2 == 0xFE0Eu32 {
                                        1u32
                                    } else {
                                        0u32
                                    };
                                    if dead == 1u32 {
                                        alive = 0u32;
                                    }
                                    if dead == 0u32 {
                                        if cp2 != 0xFE0Fu32 {
                                            skey[klen as usize] = cp2;
                                            klen += 1u32;
                                        }
                                        p += len2 as usize;
                                    }
                                }
                                // Descending-length binary search.
                                let stride = 2u32 + seq_max;
                                let seq_count = (seq.len() / stride as usize) as u32;
                                let mut elen = if klen < seq_max { klen } else { seq_max };
                                let mut slot = 0u32;
                                let mut need = 0u32;
                                while elen >= 2u32 && slot == 0u32 {
                                    let mut lo = 0u32;
                                    let mut hi = seq_count;
                                    while lo < hi {
                                        let mid = (lo + hi) / 2u32;
                                        let eoff = mid as usize * stride as usize;
                                        let entry_len = seq[eoff + 1];
                                        let kmax = if entry_len < elen { entry_len } else { elen };
                                        let mut ord = 0i32;
                                        let mut k = 0u32;
                                        while k < kmax && ord == 0i32 {
                                            let want = seq[eoff + 2 + k as usize];
                                            let probe = skey[k as usize];
                                            if probe < want {
                                                ord = -1i32;
                                            }
                                            if ord == 0i32 && probe > want {
                                                ord = 1i32;
                                            }
                                            k += 1u32;
                                        }
                                        if ord == 0i32 {
                                            // Shorter-prefix-first order: with equal
                                            // elements the SHORTER sequence sorts first,
                                            // so a longer entry is GREATER than the probe.
                                            if entry_len < elen {
                                                ord = 1i32;
                                            }
                                            if entry_len > elen {
                                                ord = -1i32;
                                            }
                                        }
                                        if ord < 0i32 {
                                            hi = mid;
                                        }
                                        if ord > 0i32 {
                                            lo = mid + 1u32;
                                        }
                                        if ord == 0i32 {
                                            slot = seq[eoff];
                                            need = elen;
                                            lo = hi;
                                        }
                                    }
                                    elen -= 1u32;
                                }
                                if slot != 0u32 {
                                    // Span end: re-walk, counting consumers.
                                    let mut got2 = 0u32;
                                    let mut p2 = id;
                                    let mut send = id as u32;
                                    let mut alive2 = 1u32;
                                    while alive2 == 1u32 && p2 < stop {
                                        let len3 = seq_len_at(bytes, p2, n);
                                        let cp3 = cp_at(bytes, p2, len3, n);
                                        let dead3 = if len3 == 0u32 || cp3 == 0x0Au32 || cp3 == 0xFE0Eu32 {
                                            1u32
                                        } else {
                                            0u32
                                        };
                                        if dead3 == 1u32 {
                                            alive2 = 0u32;
                                        }
                                        if dead3 == 0u32 {
                                            if cp3 != 0xFE0Fu32 {
                                                got2 += 1u32;
                                                if got2 == need {
                                                    send = (p2 + len3 as usize) as u32;
                                                }
                                            }
                                            p2 += len3 as usize;
                                        }
                                    }
                                    cslot[id] = slot;
                                    cend[id] = send;
                                }
                                }
                            }
                        }
                    }
                }
            }
            lane += 1usize;
        }
        fl[w] = word;
    }
}

/// The cluster CHAIN, scan-shaped: LIST RANKING over the candidate jump
/// graph. The serial walk's state is MEMORYLESS — the entire state is the
/// current position — so its committed set is exactly greedy-by-start
/// interval scheduling: `c1` = first candidate at/after the item start,
/// `c_{k+1}` = first candidate at/after `cend[c_k]` (plain stepping visits
/// every codepoint in order, so "first candidate ≥ x" is well-defined).
/// That makes the commits the ORBIT of each item's first candidate under
/// `jump`, and orbits in a functional graph are list ranking — pointer
/// doubling, the textbook primitive. This replaced thread-per-item
/// cluster_chain (measured 2026-09-27: 5.65s on 24MB text — one GPU thread
/// stepping every byte; 1.24s on 7MB emoji) with graph work proportional to
/// MATCHED candidates, which is ~0 on text and ~1/2.5 of codepoints on the
/// emoji corpus. A chunk+boundary-fixup form was considered and rejected:
/// on dense corpora the true and assumed walks stay permanently out of
/// phase (both commit at different phases), so the fixup degrades to the
/// serial walk.
///
/// The stages: compact (count-scan + scatter → sorted head positions hp),
/// jump_build (`hp[cend]` lower-bound → parent forest, terminal self-loop,
/// clamped to the candidate's ITEM so a span ending at an item edge can
/// never jump into the next item), K = ceil(log2(C+1)) rank_steps
/// (L_{k+1} = L_k∘L_k with depth sums D; level tables stored flat — the
/// orbit test needs arbitrary lifts), and cluster_mark: candidate i is
/// committed iff lifting its item's root by exactly `T[root]−T[i]` levels
/// lands on i. Merging branches (suppressed candidates can share a jump
/// target) do not fool that test: the lift follows the root's UNIQUE
/// chain, and equality against i is index-exact.
///
/// The marking body is the old chain kernel's, unchanged: head advance,
/// trailer zeroing gated on the leader bit, packed-flag fetch_or (a span
/// can straddle threads), and the trailer walk CLAMPED to the item end —
/// cend can overrun it by up to one codepoint, and the serial side only
/// ever marks members strictly inside (the Mojo chain's ownership rule).
///
/// Scope notes, deliberately recorded: the device chain writes sm/fl
/// only — the serial `gi` lane (slot / zeroed trailers) has NO device
/// writer yet, and the check's diff covers flags+advance; the renderer's
/// consumption of this path (phase 4) must grow one or the diff must
/// gain the lane. Level tables price at K·(C+1)·4B — ~40MB at the 7MB
/// emoji corpus (C ≈ 600K), ~126MB at a hypothetical 24MB emoji-dense
/// worst case; `hp`/`cslot`/`cend` add 4B/byte beside them. Never size
/// `lvl` worst-case at C=n (2.4GB) — read C once per corpus, as both
/// drivers here do.
///
/// Two instrument-era facts this build paid for, kept because they will
/// bite again: (1) `rank_step`'s step/stride MUST be `#[comptime]` — as
/// runtime u32 scalars after five same-typed slice params, the macro's
/// launch misbound the buffer args outright (sentinel-verified: writes
/// landed in the wrong buffers, values swapped across statements);
/// comptime specialization fixed it untouched otherwise. Comptime tuning
/// scalars are house style for a reason. (2) The depth ping-pong must
/// NEVER write the d0 seed buffer — a naive two-buffer alternation
/// writes round 1's depths into d0, and every replay (the bench's sample
/// loop) seeds itself with the previous run's depths. Single-run drivers
/// pass with the bug; only repeat sampling exposes it.
#[cube(launch_unchecked)]
pub(super) fn count_tile(
    cslot: &[u32],
    tc: &mut [u32],
    up: &mut [u32],
    #[comptime] units: usize,
    #[comptime] rake: usize,
    #[comptime] log: usize,
) {
    let tile = CUBE_POS;
    let u = UNIT_POS as usize;
    let n = cslot.len();
    let lo = tile * (units * rake) + u * rake;
    let hi = if lo + rake < n { lo + rake } else { n };
    let mut c = 0u32;
    if lo < n {
        let mut id = lo;
        while id < hi {
            if cslot[id] != 0u32 {
                c += 1u32;
            }
            id += 1usize;
        }
    }
    // The additive little sibling of tile_scan's monoid Blelloch. The load
    // phase writes EVERY shared slot before any read — naga only inserts
    // workgroup zero-init when initialization-before-read is unprovable,
    // and conditional writes (the probe's key scratch, formerly) force it.
    let mut sc = Shared::<[u32]>::new_slice(units);
    sc[u] = c;
    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = 1usize << d;
        if (u + 1) & (2 * s - 1) == 0 {
            sc[u] += sc[u - s];
        }
    }
    sync_cube();
    if u == units - 1 {
        tc[tile] = sc[u];
        sc[u] = 0u32;
    }
    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = units >> (d + 1);
        if (u + 1) & (2 * s - 1) == 0 {
            let t = sc[u];
            sc[u] += sc[u - s];
            sc[u - s] = t;
        }
    }
    sync_cube();
    up[tile * units + u] = sc[u];
}

/// The compaction spine: one cube Blelloch-scans the tile totals into
/// exclusive tile prefixes, chasing like spine_scan (contiguous blocks keep
/// the order for the chase writes). The unit whose block owns the LAST tile
/// publishes the grand total C — the candidate count the graph stages and
/// the host both key on (the host reads it once, in setup, to size the
/// level tables; the timed loop re-derives it on device).
#[cube(launch_unchecked)]
pub(super) fn count_spine(
    tc: &[u32],
    xc: &mut [u32],
    total: &mut [u32],
    #[comptime] units: usize,
    #[comptime] log: usize,
) {
    let u = UNIT_POS as usize;
    let n_tiles = tc.len();
    let per = n_tiles.div_ceil(units);
    let first = u * per;
    let last = if first + per < n_tiles { first + per } else { n_tiles };
    let mut acc = 0u32;
    if first < n_tiles {
        let mut t = first;
        while t < last {
            acc += tc[t];
            t += 1usize;
        }
    }
    let mut sc = Shared::<[u32]>::new_slice(units);
    sc[u] = acc;
    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = 1usize << d;
        if (u + 1) & (2 * s - 1) == 0 {
            sc[u] += sc[u - s];
        }
    }
    sync_cube();
    if u == units - 1 {
        sc[u] = 0u32;
    }
    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = units >> (d + 1);
        if (u + 1) & (2 * s - 1) == 0 {
            let t = sc[u];
            sc[u] += sc[u - s];
            sc[u - s] = t;
        }
    }
    sync_cube();
    let mut pre = sc[u];
    if first < n_tiles {
        for t in first..last {
            xc[t] = pre;
            pre += tc[t];
        }
        if last == n_tiles {
            total[0] = pre;
        }
    }
}

/// Scatter: unit re-walks its rake seeded with (tile prefix + unit prefix)
/// and appends candidate head positions — compaction into the sorted hp
/// array the binary searches ride on.
#[cube(launch_unchecked)]
pub(super) fn cand_scatter(
    cslot: &[u32],
    xc: &[u32],
    up: &[u32],
    hp: &mut [u32],
    #[comptime] units: usize,
    #[comptime] rake: usize,
) {
    let tile = CUBE_POS;
    let u = UNIT_POS as usize;
    let n = cslot.len();
    let lo = tile * (units * rake) + u * rake;
    let hi = if lo + rake < n { lo + rake } else { n };
    let mut c = xc[tile] + up[tile * units + u];
    if lo < n {
        let mut id = lo;
        while id < hi {
            if cslot[id] != 0u32 {
                if (c as usize) < hp.len() {
                    hp[c as usize] = id as u32;
                }
                c += 1u32;
            }
            id += 1usize;
        }
    }
}

/// In-place comb sort of candidate positions in `hp` and associated `cslot` / `cend`.
/// Typically ≤200 candidates in real corpora, executing on thread 0 in <1 microsecond.
#[allow(clippy::manual_swap)]
#[cube(launch_unchecked)]
pub(super) fn cand_sort(
    hp: &mut [u32],
    cslot: &mut [u32],
    cend: &mut [u32],
    ctotal: &mut [Atomic<u32>],
    #[comptime] c_cap: usize,
) {
    if ABSOLUTE_POS == 0 {
        let raw_c = ctotal[0].load();
        let c = if (raw_c as usize) > c_cap { c_cap } else { raw_c as usize };
        if (raw_c as usize) > c_cap {
            ctotal[0].store(c_cap as u32);
        }
        if c > 1 {
            let mut gap = c;
            let mut swapped = true;
            while gap > 1 || swapped {
                gap = (gap * 10) / 13;
                if gap < 1 {
                    gap = 1;
                }
                swapped = false;
                let mut i = 0usize;
                while i + gap < c {
                    let j = i + gap;
                    if hp[i] > hp[j] {
                        let t_hp = hp[i];
                        hp[i] = hp[j];
                        hp[j] = t_hp;

                        let t_cs = cslot[i];
                        cslot[i] = cslot[j];
                        cslot[j] = t_cs;

                        let t_ce = cend[i];
                        cend[i] = cend[j];
                        cend[j] = t_ce;

                        swapped = true;
                    }
                    i += 1usize;
                }
            }
        }
    }
}

/// The jump graph: `parent[i]` = first candidate at/after `cend[i]`,
/// CLAMPED to `hp[i]`'s item (a span ending exactly at an item edge must
/// never jump into the next item — the serial walk restarts there). Index C
/// is the terminal: self-loop, written by thread C itself.
#[cube(launch_unchecked)]
pub(super) fn jump_build(
    hp: &[u32],
    cend: &[u32],
    ir: &[u32],
    c_count: &[u32],
    parent: &mut [u32],
    d0: &mut [u32],
) {
    let i = ABSOLUTE_POS;
    let c = c_count[0] as usize;
    if i < parent.len() {
        if i == c {
            parent[i] = i as u32;
            d0[i] = 0u32;
        }
        if i < c {
            d0[i] = 1u32;
            let p = hp[i] as usize;
            let e = cend[i] as usize;
            let item_count = ir.len() / 2;
            let stop = if item_count > 0 {
                let it = item_search(ir, item_count, p);
                ir[it * 2 + 1] as usize
            } else {
                e
            };
            // Lower bound over hp (guarded-if form — the landmine-safe binary
            // search shape the probe uses).
            let mut lo = 0u32;
            let mut hi = c as u32;
            while lo < hi {
                let mid = (lo + hi) / 2u32;
                if (hp[mid as usize] as usize) < e {
                    lo = mid + 1u32;
                }
                if (hp[mid as usize] as usize) >= e {
                    hi = mid;
                }
            }
            let j = lo as usize;
            if j < c && (hp[j] as usize) < stop {
                parent[i] = j as u32;
            } else {
                parent[i] = c as u32;
            }
        }
        if i > c {
            parent[i] = c as u32;
            d0[i] = 0u32;
        }
    }
}

/// One pointer-doubling round: L_{k+1} = L_k∘L_k with depth sums
/// D_{k+1} = D_k + D_k∘L_k (terminal carries 0, so sums saturate at the
/// true chain length). Also archives level k's parent table into the flat
/// lvl store — the orbit test lifts by ARBITRARY distances and needs every
/// level, not just the saturated end state.
#[cube(launch_unchecked)]
pub(super) fn rank_step(
    cur_p: &[u32],
    cur_d: &[u32],
    nxt_p: &mut [u32],
    nxt_d: &mut [u32],
    lvl: &mut [u32],
    #[comptime] step: usize,
    #[comptime] stride: usize,
) {
    let i = ABSOLUTE_POS;
    if i < cur_p.len() {
        let p = cur_p[i] as usize;
        nxt_p[i] = cur_p[p];
        nxt_d[i] = cur_d[i] + cur_d[p];
        lvl[step * stride + i] = cur_p[i];
    }
}

/// Per item (cluster items only): the orbit ROOT — first candidate at/after
/// the item start, or C when the item carries none.
#[cube(launch_unchecked)]
pub(super) fn item_roots(
    hp: &[u32],
    c_count: &[u32],
    ir: &[u32],
    ic: &[u32],
    roots: &mut [u32],
) {
    let it = ABSOLUTE_POS;
    let item_count = ir.len() / 2;
    let c = c_count[0];
    if it < item_count {
        if ic[it] != 0u32 {
            let s = ir[it * 2] as usize;
            let mut lo = 0u32;
            let mut hi = c;
            while lo < hi {
                let mid = (lo + hi) / 2u32;
                if (hp[mid as usize] as usize) < s {
                    lo = mid + 1u32;
                }
                if (hp[mid as usize] as usize) >= s {
                    hi = mid;
                }
            }
            let stop = ir[it * 2 + 1] as usize;
            if lo < c && (hp[lo as usize] as usize) < stop {
                roots[it] = lo;
            } else {
                roots[it] = c;
            }
        } else {
            roots[it] = c;
        }
    }
}

/// The commit: candidate i is committed iff it lies on its item's root
/// chain — lift the root by exactly `T[root]−T[i]` levels (binary
/// decomposition over the archived level tables) and compare. The marking
/// body is the retired serial kernel's, byte for byte: head advance,
/// trailer zeroing gated on the leader bit, packed-flag fetch_or (spans
/// straddle threads).
#[cube(launch_unchecked)]
pub(super) fn cluster_mark(
    hp: &[u32],
    tdepth: &[u32],
    lvl: &[u32],
    c_count: &[u32],
    roots: &[u32],
    ir: &[u32],
    cend: &[u32],
    cslot: &[u32],
    sm: &mut [f32],
    gi: &mut [u32],
    fl_atomic: &mut [Atomic<u32>],
    #[comptime] kmax: usize,
    #[comptime] stride: usize,
    bitmap_advance: f32,
) {
    // Comptime on purpose — landmine #7's shape (runtime scalars after
    // several same-typed slices) misbound rank_step outright before these
    // were comptime; there is no reason to keep a second instance of the
    // shape to find out how narrow the trigger is.
    let i = ABSOLUTE_POS;
    let c = c_count[0] as usize;
    let item_count = ir.len() / 2;
    if i < c && item_count > 0 {
        let p = hp[i] as usize;
        let it = item_search(ir, item_count, p);
        let r = roots[it] as usize;
        if r < c {
            let tr = tdepth[r];
            let ti = tdepth[i];
            // ti == tr is the ROOT itself — distance 0, committed by
            // definition (the lift loop below then never runs).
            if ti <= tr {
                let mut x = r as u32;
                let mut rem = tr - ti;
                let mut k = 0usize;
                while k < kmax && rem > 0u32 {
                    if rem & 1u32 == 1u32 {
                        x = lvl[k * stride + x as usize];
                    }
                    rem >>= 1u32;
                    k += 1usize;
                }
                if x as usize == i {
                    // The trailer walk clamps to the ITEM END — cend can
                    // overrun it by up to one codepoint (the span-end walk
                    // starts a member before stop), and the serial side only
                    // ever marks members strictly inside the item. The Mojo
                    // chain's ownership rule, verbatim.
                    let stop = ir[it * 2 + 1] as usize;
                    let e = cend[i] as usize;
                    let lim = if e < stop { e } else { stop };
                    sm[p] = bitmap_advance;
                    // A committed head's glyph IS the sequence's slot —
                    // the record emitter's GLYPH_ID for cluster heads
                    // (fold.rs:607's slots.gi[id] = best_slot). The
                    // consumer is the repo parity driver / phase 4.
                    gi[p] = cslot[i];
                    let mut t = p + 1usize;
                    while t < lim {
                        if flags_at_from_atomic(fl_atomic, t) & F_LEADER != 0 {
                            sm[t] = f32::from_bits(0u32);
                            // fold.rs:610-612: a trailer member's gi zeroes
                            // with its advance — the engine's records carry
                            // gi 0 for cluster trailers, and the parity
                            // driver catches exactly this.
                            gi[t] = 0u32;
                            fl_atomic[t >> 2].fetch_or(F_CLUSTER_TRAILER << (((t & 3) as u32) * 8u32));
                        }
                        t += 1usize;
                    }
                }
            }
        }
    }
}

/// flags_at over the atomic view of the packed flag buffer (the chain reads
/// leader bits while OR-ing trailer bits into the same words — the fetch_or
/// never touches bit 0, so reads stay consistent).
#[cube]
fn flags_at_from_atomic(fl: &mut [Atomic<u32>], i: usize) -> u32 {
    fl[i >> 2].load() >> (((i & 3) as u32) * 8u32) & 0xFFu32
}

/// Host-side cluster inputs from a flat sequence table: the candidacy
/// bitmap (one bit per codepoint, set for every sequence's first member)
/// and the per-item cluster flags.
pub(crate) fn cluster_host_inputs(
    seq: &[u32],
    seq_max: u32,
    items: &[crate::fold::Item],
) -> (Vec<u32>, Vec<u32>) {
    let stride = 2 + seq_max as usize;
    let mut bitmap = vec![0u32; 0x110000 / 32 + 1];
    for i in (0..seq.len()).step_by(stride) {
        let cp = seq[i + 2] as usize;
        bitmap[cp >> 5] |= 1 << (cp & 31);
    }
    let ic = items
        .iter()
        .map(|it| u32::from(it.cluster_mode == crate::fold::ClusterMode::Cluster))
        .collect();
    (bitmap, ic)
}

/// The probe's second-level filter: for every first codepoint that starts
/// sequences, the SORTED list of second elements that actually occur. The
/// table stores EFFECTIVE keys (FE0F already stripped — the keycap entry
/// is `[49, 8419]`), so the recorded seconds are the effective seconds; a
/// pathological raw-FE0F entry would only yield a dead, over-accept-only
/// pair. `sec_off` is indexed by cp (0x110002 entries — the +1 read gives
/// each first's end), `sec_val` the flat seconds grouped by first. Entries
/// shorter than 2 are skipped: the matcher's own `elen >= 2` guard makes
/// them unreachable on both sides, so rejecting before the search is
/// behavior-identical.
pub(crate) fn cluster_pair_filter(seq: &[u32], seq_max: u32) -> (Vec<u32>, Vec<u32>) {
    let stride = 2 + seq_max as usize;
    // The kernel's "no second found" sentinel is codepoint 0 — pin that no
    // entry carries a zero ELEMENT at all (a NUL second would be
    // indistinguishable from "none" and silently diverge from the CPU,
    // which keys it). Trie data, checked once, fails loudly.
    assert!(
        (0..seq.len())
            .step_by(stride)
            .all(|e| (0..seq[e + 1] as usize).all(|k| seq[e + 2 + k] != 0)),
        "sequence entry with a zero element would break the pair filter's sentinel"
    );
    let mut pairs: Vec<(u32, u32)> = Vec::new();
    for e in (0..seq.len()).step_by(stride) {
        if seq[e + 1] >= 2 {
            pairs.push((seq[e + 2], seq[e + 3]));
        }
    }
    pairs.sort_unstable();
    pairs.dedup();
    let mut off = vec![0u32; 0x110000 + 2];
    for &(first, _) in &pairs {
        off[first as usize + 1] += 1;
    }
    for i in 1..off.len() {
        off[i] += off[i - 1];
    }
    let mut cursor = off.clone();
    let mut val = vec![0u32; pairs.len()];
    for &(first, second) in &pairs {
        let s = cursor[first as usize] as usize;
        val[s] = second;
        cursor[first as usize] += 1;
    }
    (off, val)
}
