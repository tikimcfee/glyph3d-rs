use cubecl::prelude::*;

use super::monoid::{
    combine, flags_at, identity, item_search, leaf_of, ordered_key, p_load, p_store, rows_for,
    s_load, s_store, wrap_row_of, wrap_segment_of,
};
use super::{
    F_LEADER, F_NEWLINE, IE_HAS_PAGE, IE_PAGE_COLS, IE_STRIDE, IE_WRAP_MODE, IE_WRAP_WIDTH,
    IM_LINE_HEIGHT, IM_ORIGIN_X, IM_ORIGIN_Y, IM_ORIGIN_Z, IM_STRIDE, IM_Z_STEP, IM_Z_STEP_LO,
    LC_COL, LC_ROW, LC_STRIDE, LM_BASE_X, LM_STRIDE, LM_X, LM_Y, LM_Z, P_MODE, P_WRAP,
    PARTIAL_COUNT_STRIDE, RESOLVE_SLOTS,
};

// ── dispatch 1: tileScan — one cube per tile; rake + workgroup Blelloch ──────
//
// Each unit rakes its `rake` bytes into one monoid element (a serial,
// order-preserving micro-fold); the cube Blelloch-scans the `units` partials
// in shared memory into exclusive micro prefixes; unit units-1 publishes the
// tile total. The critical path per tile is rake + 2·log(units) combines —
// against the old thread-per-chunk serial fold's one-thread `chunk`-deep
// chain with units-fold less parallelism.
#[cube(launch_unchecked)]
pub(super) fn tile_scan(
    fl: &[u32],
    sm: &[f32],
    ir: &[u32],
    ie: &[u32],
    tc: &mut [u32],
    tm: &mut [f32],
    #[comptime] units: usize,
    #[comptime] rake: usize,
    #[comptime] log: usize,
) {
    let tile = CUBE_POS;
    let u = UNIT_POS as usize;
    let n = fl.len() * 4; // packed: words -> bytes
    let item_count = ir.len() / 2;
    let lo = tile * (units * rake) + u * rake;
    let hi = if lo + rake < n { lo + rake } else { n };
    // ItemWalk seed. For pad units (lo >= n) the seed clamps to the last byte
    // so the pad element carries the wrap/mode in force at the tile's end —
    // see the module header for why a pure-identity pad would poison the
    // tile total's wrap lanes. (n == 0 makes the clamp wrap; the seed then
    // feeds only an unused walk, and every ir/ie read stays in bounds.)
    let mut seed = lo;
    if seed >= n {
        seed = n - 1;
    }
    let mut it = 0usize;
    let mut start = 0usize;
    let mut nxt = n;
    let mut w_wrap = 0i32;
    let mut w_mode = 0i32;
    let has = item_count > 0;
    if has {
        it = item_search(ir, item_count, seed);
        start = ir[it * 2] as usize;
        nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
        w_wrap = ie[it * IE_STRIDE + IE_WRAP_WIDTH] as i32;
        w_mode = ie[it * IE_STRIDE + IE_WRAP_MODE] as i32;
    }
    let mut acc = identity();
    if lo < n {
        let mut id = lo;
        while id < hi {
            while has && nxt <= id {
                it += 1;
                start = ir[it * 2] as usize;
                nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
                w_wrap = ie[it * IE_STRIDE + IE_WRAP_WIDTH] as i32;
                w_mode = ie[it * IE_STRIDE + IE_WRAP_MODE] as i32;
            }
            let reset = if has && id == start { 1i32 } else { 0i32 };
            let leaf = leaf_of(fl, sm, w_wrap, w_mode, reset, id);
            combine(&mut acc, &leaf);
            id += 1;
        }
    } else {
        acc.wrap = w_wrap;
        acc.mode = w_mode;
    }

    let mut sc = Shared::<[i32]>::new_slice(units * PARTIAL_COUNT_STRIDE);
    let mut sf = Shared::<[f32]>::new_slice(units);
    s_store(&mut sc, &mut sf, u, &acc);

    // Up-sweep: x[u] = combine(x[u-s], x[u]) at the ends of 2s-blocks. The
    // LEFT operand is the lower element — the fold order.
    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = 1usize << d;
        if (u + 1) & (2 * s - 1) == 0 {
            let mut a = s_load(&sc, &sf, u - s);
            let b = s_load(&sc, &sf, u);
            combine(&mut a, &b);
            s_store(&mut sc, &mut sf, u, &a);
        }
    }
    // Exclusive: hold the total in a register (it publishes below), then seed
    // the root with identity.
    sync_cube();
    if u == units - 1 {
        let total = s_load(&sc, &sf, u);
        p_store(tc, tm, tile, &total);
        let e = identity();
        s_store(&mut sc, &mut sf, u, &e);
    }
    // Down-sweep — NON-COMMUTATIVE form, derived and unit-tested by hand on
    // n=8: t = x[u]; x[u] = combine(x[u], x[u-s]); x[u-s] = t. The carried
    // prefix is the LEFT operand and the left child's TOTAL the right; the
    // commutative textbook form (combine(x[u-s], x[u])) silently scrambles
    // reset/head/tail lanes.
    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = units >> (d + 1);
        if (u + 1) & (2 * s - 1) == 0 {
            let t = s_load(&sc, &sf, u);
            let mut a = s_load(&sc, &sf, u);
            let b = s_load(&sc, &sf, u - s);
            combine(&mut a, &b);
            s_store(&mut sc, &mut sf, u, &a);
            s_store(&mut sc, &mut sf, u - s, &t);
        }
    }
    sync_cube();
}

// ── dispatch 2: spineScan — ONE cube over the tile totals ────────────────────
//
// Each unit owns a CONTIGUOUS block of tile totals (order-preserving for the
// non-commutative monoid — a strided rake would interleave blocks across
// units and the workgroup scan could not recompose them), rakes them into
// one partial, the cube Blelloch-scans the units, then each unit chases its
// block writing global exclusive tile prefixes. One cube handles
// units·(tiles/units) tiles by raking deeper; the scaling path beyond that
// is a second spine level, not built yet.
#[cube(launch_unchecked)]
pub(super) fn spine_scan(
    tc: &[u32],
    tm: &[f32],
    xc: &mut [u32],
    xm: &mut [f32],
    #[comptime] units: usize,
    #[comptime] log: usize,
) {
    let u = UNIT_POS as usize;
    let n_tiles = tc.len() / PARTIAL_COUNT_STRIDE;
    let per = n_tiles.div_ceil(units);
    let first = u * per;
    let last = if first + per < n_tiles { first + per } else { n_tiles };
    let mut acc = identity();
    if first < n_tiles {
        for t in first..last {
            let e = p_load(tc, tm, t);
            combine(&mut acc, &e);
        }
    } else {
        // Pad: identity counts, the LAST tile's wrap/mode — see module header.
        let o = (n_tiles - 1) * PARTIAL_COUNT_STRIDE;
        acc.wrap = tc[o + P_WRAP] as i32;
        acc.mode = tc[o + P_MODE] as i32;
    }

    let mut sc = Shared::<[i32]>::new_slice(units * PARTIAL_COUNT_STRIDE);
    let mut sf = Shared::<[f32]>::new_slice(units);
    s_store(&mut sc, &mut sf, u, &acc);

    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = 1usize << d;
        if (u + 1) & (2 * s - 1) == 0 {
            let mut a = s_load(&sc, &sf, u - s);
            let b = s_load(&sc, &sf, u);
            combine(&mut a, &b);
            s_store(&mut sc, &mut sf, u, &a);
        }
    }
    sync_cube();
    if u == units - 1 {
        let e = identity();
        s_store(&mut sc, &mut sf, u, &e);
    }
    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = units >> (d + 1);
        if (u + 1) & (2 * s - 1) == 0 {
            let t = s_load(&sc, &sf, u);
            let mut a = s_load(&sc, &sf, u);
            let b = s_load(&sc, &sf, u - s);
            combine(&mut a, &b);
            s_store(&mut sc, &mut sf, u, &a);
            s_store(&mut sc, &mut sf, u - s, &t);
        }
    }
    sync_cube();

    // Chase: this unit's block of tiles gets its global exclusive prefixes.
    let mut pre = s_load(&sc, &sf, u);
    if first < n_tiles {
        for t in first..last {
            p_store(xc, xm, t, &pre);
            let e = p_load(tc, tm, t);
            combine(&mut pre, &e);
        }
    }
}

/// The fold width in force for item `it`: its wrap width, or its page
/// columns when it has no wrap. fold==0 means x IS the line-advance lane —
/// no segment re-sum exists.
#[cube]
pub(super) fn fold_of(ie: &[u32], it: usize, wrap: i32) -> i32 {
    if wrap > 0 {
        wrap
    } else if ie[it * IE_STRIDE + IE_HAS_PAGE] != 0 {
        ie[it * IE_STRIDE + IE_PAGE_COLS] as i32
    } else {
        0
    }
}

// ── dispatch 3: apply — rake + Blelloch + per-byte chase ─────────────────────
//
// Same tile decomposition as tile_scan (the rake and tree are re-derived).
// The publish-the-micro-prefixes alternative was BUILT AND MEASURED
// (2026-09-27) and LOST: apply 21.0 -> 31.4ms, chain 28.6 -> 40.2. The
// rake's "redundant" re-read of fl/sm rides L1 — the chase re-reads the
// same 8 bytes it just raked — so it is nearly free, while 36B/unit of
// published prefixes are cold global traffic, and the tree's 18 barriers
// are cheap. Reverted; do not re-derive this trade, re-measure it. Each
// unit chases its `rake` bytes seeded with combine(global tile prefix, own
// exclusive micro prefix), emitting the per-byte lanes.
//
// THE TREE ITSELF costs ~2.3-2.6ms per kernel that carries one (measured
// same day by emptying the loops: tile_scan 5.6 -> 3.3ms), so ~5ms of the
// ~25.6ms chain — the C6 plane-op restructure's ceiling. It stays parked:
// plane ops are subgroup intrinsics on the WGSL path (a pinned-naga
// behavior, absent on weaker backends), they diverge per compute target
// (the CPU runtime's PLANE_DIM differs), and they are scalar-numeric-only,
// so this monoid would need a manual shuffle tree with the non-commutative
// down-swap rebuilt lane by lane. If taken: as a #[comptime] capability-
// gated variant beside this default, the inline_resolve pattern — never as
// the foundation.
//
// FOLDLESS ITEMS RESOLVE HERE: for fold==0 the chase already holds
// everything resolve_x would recompute — x IS run.tail_adv, and the old
// kernel read ~700MB back (fl, lc, wm, wc, otb) to recover values that were
// in registers at this exact point. The chase writes their lm lanes and
// reduces their maxima directly; wm/wc/otb are written ONLY for fold>0
// items, whose segment re-sum genuinely needs the cross-unit ordinal table,
// and the resolve_x dispatch is skipped entirely when no item folds. Row
// maxima reduce here for EVERY item (rows are final in the chase); x maxima
// only where x is final (fold==0) — resolve_x owns the fold>0 x maxima.
//
// `inline_resolve` is COMPTIME (the k_decode_probe[probe,walk] pattern): the
// inline branch's live state (four item-parameter loads, the lm writes)
// inflates the whole kernel's register set, and measured on a pure-wrapped
// corpus that costs 2x on the division-heavy fold>0 path — stalls that the
// memory-bound foldless path hides. Corpora where EVERY item folds compile
// the inline branch out entirely; mixed and foldless corpora compile it in.
#[cube(launch_unchecked)]
pub(super) fn apply(
    fl: &[u32],
    sm: &[f32],
    lc: &mut [u32],
    lm: &mut [f32],
    ir: &[u32],
    ie: &[u32],
    items: &[f32],
    xc: &[u32],
    xm: &[f32],
    wm: &mut [f32],
    wc: &mut [u32],
    otb: &mut [u32],
    row_max: &mut [Atomic<u32>],
    x_max: &mut [Atomic<u32>],
    #[comptime] units: usize,
    #[comptime] rake: usize,
    #[comptime] log: usize,
    #[comptime] inline_resolve: bool,
) {
    let tile = CUBE_POS;
    let u = UNIT_POS as usize;
    let n = fl.len() * 4; // packed: words -> bytes
    let item_count = ir.len() / 2;
    let lo = tile * (units * rake) + u * rake;
    let hi = if lo + rake < n { lo + rake } else { n };
    // Maxima-reduction slots, seeded before the tree so its barriers cover
    // visibility (see resolve_x's header for the slot protocol). The whole
    // apparatus is compiled out for pure-wrapped corpora: measured there,
    // the per-leader RMWs cost ~8ms inside apply's division-stalled chase,
    // while riding FREE inside resolve_x, whose re-sum already dominates —
    // so resolve_x owns both maxima for that shape (fetch_max idempotence
    // makes the mixed-corpus double reduction harmless).
    let srow = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS);
    let sx = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS);
    let mut sbase = Shared::<u32>::new();
    let tile_lo = tile * (units * rake);
    let mut it_base = 0usize;
    if inline_resolve {
        if u == 0 {
            let probe = if tile_lo < n { tile_lo } else { n - 1 };
            let mut b = 0usize;
            if item_count > 0 {
                b = item_search(ir, item_count, probe);
            }
            *sbase = b as u32;
        }
        let mut z = u;
        while z < RESOLVE_SLOTS {
            srow[z].store(0u32);
            sx[z].store(0u32);
            z += units;
        }
    }
    let mut seed = lo;
    if seed >= n {
        seed = n - 1;
    }
    let mut it = 0usize;
    let mut start = 0usize;
    let mut nxt = n;
    let mut w_wrap = 0i32;
    let mut w_mode = 0i32;
    let has = item_count > 0;
    if has {
        it = item_search(ir, item_count, seed);
        start = ir[it * 2] as usize;
        nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
        w_wrap = ie[it * IE_STRIDE + IE_WRAP_WIDTH] as i32;
        w_mode = ie[it * IE_STRIDE + IE_WRAP_MODE] as i32;
    }
    let mut acc = identity();
    if lo < n {
        let mut id = lo;
        while id < hi {
            while has && nxt <= id {
                it += 1;
                start = ir[it * 2] as usize;
                nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
                w_wrap = ie[it * IE_STRIDE + IE_WRAP_WIDTH] as i32;
                w_mode = ie[it * IE_STRIDE + IE_WRAP_MODE] as i32;
            }
            let reset = if has && id == start { 1i32 } else { 0i32 };
            let leaf = leaf_of(fl, sm, w_wrap, w_mode, reset, id);
            combine(&mut acc, &leaf);
            id += 1;
        }
    } else {
        acc.wrap = w_wrap;
        acc.mode = w_mode;
    }

    let mut sc = Shared::<[i32]>::new_slice(units * PARTIAL_COUNT_STRIDE);
    let mut sf = Shared::<[f32]>::new_slice(units);
    s_store(&mut sc, &mut sf, u, &acc);

    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = 1usize << d;
        if (u + 1) & (2 * s - 1) == 0 {
            let mut a = s_load(&sc, &sf, u - s);
            let b = s_load(&sc, &sf, u);
            combine(&mut a, &b);
            s_store(&mut sc, &mut sf, u, &a);
        }
    }
    sync_cube();
    if u == units - 1 {
        let e = identity();
        s_store(&mut sc, &mut sf, u, &e);
    }
    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = units >> (d + 1);
        if (u + 1) & (2 * s - 1) == 0 {
            let t = s_load(&sc, &sf, u);
            let mut a = s_load(&sc, &sf, u);
            let b = s_load(&sc, &sf, u - s);
            combine(&mut a, &b);
            s_store(&mut sc, &mut sf, u, &a);
            s_store(&mut sc, &mut sf, u - s, &t);
        }
    }
    sync_cube();

    // The chase: the running prefix at this unit's first byte is the global
    // tile prefix combined with the exclusive micro prefix from the tree.
    // The walk is RE-SEEDED first — the rake advanced it past this unit's
    // range, and each byte's reset/wrap/mode must come from ITS item, not
    // the range's last one (the multi-item fixtures caught exactly this).
    let mut run = p_load(xc, xm, tile);
    let micro = s_load(&sc, &sf, u);
    combine(&mut run, &micro);
    if inline_resolve {
        it_base = *sbase as usize;
    }
    it = 0usize;
    start = 0usize;
    nxt = n;
    w_wrap = 0i32;
    w_mode = 0i32;
    let mut w_fold = 0i32;
    if has {
        it = item_search(ir, item_count, lo);
        start = ir[it * 2] as usize;
        nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
        w_wrap = ie[it * IE_STRIDE + IE_WRAP_WIDTH] as i32;
        w_mode = ie[it * IE_STRIDE + IE_WRAP_MODE] as i32;
        w_fold = fold_of(ie, it, w_wrap);
    }
    if lo < n {
        let mut id = lo;
        while id < hi {
            while has && nxt <= id {
                it += 1;
                start = ir[it * 2] as usize;
                nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
                w_wrap = ie[it * IE_STRIDE + IE_WRAP_WIDTH] as i32;
                w_mode = ie[it * IE_STRIDE + IE_WRAP_MODE] as i32;
                w_fold = fold_of(ie, it, w_wrap);
            }
            let reset = has && id == start;
            if reset {
                // run = identity(), then the item's params ride again.
                run.reset = 0;
                run.nl = 0;
                run.glyphs = 0;
                run.rows = 0;
                run.head_len = 0;
                run.tail_len = 0;
                run.tail_adv = 0.0;
                run.wrap = w_wrap;
                run.mode = w_mode;
            }
            let f = flags_at(fl, id);
            if (f & F_LEADER) != 0 {
                // lanes_from_prefix, inline.
                let col = run.tail_len;
                let mut closed = 0i32;
                if run.nl > 0 {
                    closed = rows_for(run.head_len, w_wrap, w_mode) + run.rows;
                }
                let wr = wrap_row_of(col, w_wrap, (f & F_NEWLINE) != 0, w_mode);
                let row = closed + wr;
                let co = id * LC_STRIDE;
                lc[co + LC_ROW] = row as u32;
                lc[co + LC_COL] = col as u32;
                if inline_resolve {
                    // Rows are final here for every item — reduce them all.
                    // (Compiled out for pure-wrapped corpora: resolve_x owns
                    // both maxima there — see the slot-seeding note above.)
                    let slot = it - it_base;
                    if slot < RESOLVE_SLOTS {
                        srow[slot].fetch_max((row + 1) as u32);
                    } else {
                        row_max[it].fetch_max((row + 1) as u32);
                    }
                }
                // The forward ordinal map (wc: byte -> item-relative
                // ordinal) and the inverse table (otb) are written on
                // EVERY path — the record emitter (phase 4 rung 2)
                // gathers through wc, and the foldless inline path used
                // to leave both unwritten. wm stays resolve-only (the
                // re-sum's line-advance input, diffed where fold>0).
                wc[id] = run.glyphs as u32;
                otb[start + run.glyphs as usize] = id as u32;
                if w_fold > 0 || !inline_resolve {
                    wm[id] = run.tail_adv;
                } else {
                    // Foldless: x IS the line-advance lane — resolve_x's
                    // whole per-item computation, in registers, now.
                    let x = run.tail_adv;
                    let io = it * IM_STRIDE;
                    let wrap_segment = wrap_segment_of(col, w_wrap, (f & F_NEWLINE) != 0);
                    let lh = items[io + IM_LINE_HEIGHT];
                    let mo = id * LM_STRIDE;
                    let base = x + items[io + IM_ORIGIN_X];
                    lm[mo + LM_BASE_X] = base;
                    lm[mo + LM_X] = base;
                    // Y/Z: one OPAQUE fma each — the engine's f64
                    // two-term expressions narrowed once, bit-exact (see
                    // paginate's fma note for why opaque); Z folds the
                    // z_step tail through the second fma.
                    lm[mo + LM_Y] = fma(-(row as f32), lh, items[io + IM_ORIGIN_Y]);
                    let depth_steps = -(wrap_segment as f32);
                    let z_tail_folded = fma(
                        depth_steps,
                        items[io + IM_Z_STEP_LO],
                        items[io + IM_ORIGIN_Z],
                    );
                    lm[mo + LM_Z] = fma(depth_steps, items[io + IM_Z_STEP], z_tail_folded);
                    let slot = it - it_base;
                    if slot < RESOLVE_SLOTS {
                        sx[slot].fetch_max(ordered_key(x));
                    } else {
                        x_max[it].fetch_max(ordered_key(x));
                    }
                }
            }
            let leaf = leaf_of(fl, sm, w_wrap, w_mode, if reset { 1i32 } else { 0i32 }, id);
            combine(&mut run, &leaf);
            id += 1;
        }
    }
    if inline_resolve {
        sync_cube();
        if u < RESOLVE_SLOTS {
            let it2 = it_base + u;
            if it2 < item_count {
                row_max[it2].fetch_max(srow[u].load());
                x_max[it2].fetch_max(sx[u].load());
            }
        }
    }
}
