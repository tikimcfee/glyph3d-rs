//! CubeCL scan chain — dev-only (`--cubecl-chain-check`), the standard
//! parallel structure.
//!
//!   tileScan (rake + workgroup Blelloch) -> spineScan (one cube)
//!                -> apply (rake + Blelloch + chase) -> resolveX
//!                -> deriveStride -> paginate
//!
//! The note-16 phase-2 skeleton transcribed the Mojo device chain
//! line-for-line — thread-per-chunk serial 64-byte folds, a single-thread
//! spine — which proved the monoid but measured transcription quality, not
//! the algorithm. This is the textbook hierarchical scan instead: each cube
//! owns a `units x rake`-byte tile; every unit rakes its `rake` bytes into
//! one monoid element; a workgroup Blelloch scan over the per-unit partials
//! (shared memory, `sync_cube` between rounds) produces exclusive prefixes;
//! the spine is one cube doing the same over tile totals; `apply` re-rakes
//! and chases its bytes seeded with (global tile prefix + own micro prefix).
//! The CPU reference stays `scan.rs::run_scan_pipeline` — a DIFFERENT tree
//! shape at a different tuning, so agreement is associativity checked in
//! situ, the same evidence pattern as scan.rs's own chunk-sweep tests.
//!
//! Precision contract, deliberately restated for this structure: integer
//! lanes (lc/wc/otb) diff BIT-EXACT — the monoid's count lanes are exact and
//! any order-preserving association agrees. `tail_adv` is f32-per-add and the
//! Blelloch tree REASSOCIATES those adds within a tile, so line_advance
//! leaves bit-parity with scan.rs and lands in the ORACLE's existing 1e-4
//! eps tier (scan-vs-serial-fold was already eps there — only this
//! instrument's same-tuning bit check is loosened, max deviation reported).
//! The fold>0 X lanes stay bit-exact: resolve_x still re-sums each wrap
//! segment serially, in the same left-fold order as the serial recurrence.
//!
//! Two tree-specific invariants, both load-bearing:
//! - PAD elements (tail units of the last tile, empty spine blocks) carry the
//!   wrap/mode in force at their position, NOT pure identity — combine copies
//!   wrap/mode off its RIGHT operand unconditionally, and a zero-wrap pad at
//!   the end of a tile would clobber the tile total's wrap and mis-junction
//!   every later combine that reads it.
//! - The exclusive prefix at the tree root is seeded with pure identity; that
//!   is safe because a prefix element's OWN wrap/mode lanes are never read —
//!   combine only reads them off the right operand, which is always a real
//!   leaf or a real-derived element on every path that matters.

use std::path::Path;

use cubecl::prelude::*;
use cubecl::wgpu::{AutoGraphicsApi, GraphicsApi, WgpuSetup};

use crate::fold::WrapMode;
use crate::gpu::GpuContext;
use crate::scan::{DEFAULT_CHUNK_SIZE, DEFAULT_GROUP_SIZE, run_scan_pipeline};

// ── lane layout (glyph-identity.json, hash-pinned) ──────────────────────────
const PARTIAL_COUNT_STRIDE: usize = 8;
const P_RESET: usize = 0;
const P_NL: usize = 1;
const P_GLYPHS: usize = 2;
const P_ROWS: usize = 3;
const P_HEAD_LEN: usize = 4;
const P_TAIL_LEN: usize = 5;
const P_WRAP: usize = 6;
const P_MODE: usize = 7;
/// The measure static is ADVANCE ONLY. Height is renderer statics the
/// scan never reads; carrying it here doubled the per-byte measure traffic.
const SM_STRIDE: usize = 1;
const SM_ADVANCE: usize = 0;
/// resolveX's per-cube shared reduction slots, item-relative from the item
/// at the cube's first byte. A 2 KB tile spanning more than this many items
/// (never in practice) takes the global-atomic overflow path instead.
const RESOLVE_SLOTS: usize = 16;
const LM_STRIDE: usize = 4;
const LM_X: usize = 0;
const LM_Y: usize = 1;
const LM_Z: usize = 2;
const LM_BASE_X: usize = 3;
const LC_STRIDE: usize = 2;
const LC_ROW: usize = 0;
const LC_COL: usize = 1;
const IM_STRIDE: usize = 9;
const IM_ORIGIN_Y: usize = 0;
const IM_ORIGIN_Z: usize = 1;
const IM_LINE_HEIGHT: usize = 2;
const IM_Z_STEP: usize = 3;
const IM_BAND_STRIDE_Y: usize = 4;
const IM_DEPTH_PER_BAND: usize = 5;
const IM_DEPTH_PER_COL: usize = 6;
const IM_ORIGIN_X: usize = 8;
const IE_STRIDE: usize = 8;
const IE_PAGE_ROWS: usize = 0;
const IE_PAGE_COLS: usize = 1;
const IE_SCROLL_ROWS: usize = 2;
const IE_PAGES_WIDE: usize = 3;
const IE_WRAP_WIDTH: usize = 4;
const IE_HAS_PAGE: usize = 5;
const IE_WRAP_MODE: usize = 6;

const F_LEADER: u32 = 1;
const F_NEWLINE: u32 = 4;
const WRAP_BACK: i32 = 1;

// ── the monoid, device-side ──────────────────────────────────────────────────

/// scan.rs's ScanElem, register-resident. i32 lanes where the CPU rides i64:
/// counts, every one — no lane approaches 2^31 inside a chunk.
#[derive(CubeType, Clone, Copy)]
struct ChainElem {
    reset: i32,
    nl: i32,
    glyphs: i32,
    rows: i32,
    head_len: i32,
    tail_len: i32,
    wrap: i32,
    mode: i32,
    tail_adv: f32,
}

#[cube]
fn identity() -> ChainElem {
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
fn rows_for(length: i32, wrap: i32, mode: i32) -> i32 {
    if mode == WRAP_BACK || wrap <= 0 || length <= 0 {
        1
    } else {
        (length - 1) / wrap + 1
    }
}

/// wrap_segment_of, transcribed — mode-free depth fan.
#[cube]
fn wrap_segment_of(col: i32, wrap: i32, terminator: bool) -> i32 {
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
fn wrap_row_of(col: i32, wrap: i32, terminator: bool, mode: i32) -> i32 {
    if mode == WRAP_BACK {
        0
    } else {
        wrap_segment_of(col, wrap, terminator)
    }
}

/// scan_combine, transcribed line-for-line — the GENERAL form (spine-grade:
/// b may carry rows and a real head line, not just a leaf's).
#[cube]
fn combine(a: &mut ChainElem, b: &ChainElem) {
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
#[cube]
fn leaf_of(fl: &[u32], sm: &[f32], wrap: i32, mode: i32, reset: i32, id: usize) -> ChainElem {
    let mut e = identity();
    e.reset = reset;
    e.wrap = wrap;
    e.mode = mode;
    let f = fl[id];
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
fn p_load(pc: &[u32], pm: &[f32], i: usize) -> ChainElem {
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
fn p_store(pc: &mut [u32], pm: &mut [f32], i: usize, e: &ChainElem) {
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
fn item_search(ir: &[u32], item_count: usize, id: usize) -> usize {
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
fn ordered_key(v: f32) -> u32 {
    let b = v.to_bits();
    if (b & 0x80000000) != 0 {
        !b
    } else {
        b | 0x80000000
    }
}

/// key_to_float: its inverse (for derive_stride's readback of x_max).
#[cube]
fn key_to_float(k: u32) -> f32 {
    let b = if (k & 0x80000000) != 0 { k & 0x7FFFFFFF } else { !k };
    f32::from_bits(b)
}

/// Load a monoid element from the shared tile arrays (the pc lane layout, i32).
#[cube]
fn s_load(sc: &[i32], sf: &[f32], i: usize) -> ChainElem {
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
fn s_store(sc: &mut [i32], sf: &mut [f32], i: usize, e: &ChainElem) {
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

// ── dispatch 1: tileScan — one cube per tile; rake + workgroup Blelloch ──────
//
// Each unit rakes its `rake` bytes into one monoid element (a serial,
// order-preserving micro-fold); the cube Blelloch-scans the `units` partials
// in shared memory into exclusive micro prefixes; unit units-1 publishes the
// tile total. The critical path per tile is rake + 2·log(units) combines —
// against the old thread-per-chunk serial fold's one-thread `chunk`-deep
// chain with units-fold less parallelism.
#[cube(launch_unchecked)]
fn tile_scan(
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
    let n = fl.len();
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
fn spine_scan(
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

// ── dispatch 3: apply — rake + Blelloch + per-byte chase ─────────────────────
//
// Same tile decomposition as tile_scan (the rake and tree are re-derived —
// the alternative, publishing per-unit micro prefixes to global memory,
// costs n/rake elements of write+read against re-reading fl/sm once). Each
// unit chases its `rake` bytes seeded with combine(global tile prefix, own
// exclusive micro prefix), emitting the per-byte lanes exactly as the old
// thread-per-chunk k_apply did — but an 8-deep serial chase per unit at
// n/(units·rake)·units-way parallelism instead of a 64-deep one.
#[cube(launch_unchecked)]
fn apply(
    fl: &[u32],
    sm: &[f32],
    lc: &mut [u32],
    ir: &[u32],
    ie: &[u32],
    xc: &[u32],
    xm: &[f32],
    wm: &mut [f32],
    wc: &mut [u32],
    otb: &mut [u32],
    #[comptime] units: usize,
    #[comptime] rake: usize,
    #[comptime] log: usize,
) {
    let tile = CUBE_POS;
    let u = UNIT_POS as usize;
    let n = fl.len();
    let item_count = ir.len() / 2;
    let lo = tile * (units * rake) + u * rake;
    let hi = if lo + rake < n { lo + rake } else { n };
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
    it = 0usize;
    start = 0usize;
    nxt = n;
    w_wrap = 0i32;
    w_mode = 0i32;
    if has {
        it = item_search(ir, item_count, lo);
        start = ir[it * 2] as usize;
        nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
        w_wrap = ie[it * IE_STRIDE + IE_WRAP_WIDTH] as i32;
        w_mode = ie[it * IE_STRIDE + IE_WRAP_MODE] as i32;
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
            let f = fl[id];
            if (f & F_LEADER) != 0 {
                // lanes_from_prefix, inline.
                let col = run.tail_len;
                let mut closed = 0i32;
                if run.nl > 0 {
                    closed = rows_for(run.head_len, w_wrap, w_mode) + run.rows;
                }
                let wr = wrap_row_of(col, w_wrap, (f & F_NEWLINE) != 0, w_mode);
                let co = id * LC_STRIDE;
                lc[co + LC_ROW] = (closed + wr) as u32;
                lc[co + LC_COL] = col as u32;
                wc[id] = run.glyphs as u32;
                wm[id] = run.tail_adv;
                otb[start + run.glyphs as usize] = id as u32;
            }
            let leaf = leaf_of(fl, sm, w_wrap, w_mode, if reset { 1i32 } else { 0i32 }, id);
            combine(&mut run, &leaf);
            id += 1;
        }
    }
}

// ── dispatch 4: resolveX — thread per byte, leaders only ─────────────────────
//
// The x re-sum is UNCHANGED (measured +0.2ms at wrap=96 — within a line the
// predecessor advances are contiguous and L1-local — and its serial left-fold
// order is what holds the fold>0 X lanes' bit-exact tier). What changed is
// the maxima: the old kernel fired two GLOBAL fetch_max per leader, and with
// per-item cells that serializes every leader in an item (the bench's single
// item made all 24M leaders contend on two words — 41.6ms of the 69ms
// chain). Now each cube reduces its leaders' maxima through SHARED atomics
// in RESOLVE_SLOTS item-relative slots and flushes one global RMW per
// touched slot: contention drops from leaders-per-item to
// cubes-per-item. Cubes spanning more than RESOLVE_SLOTS items (never in
// practice; items are files and tiles are 2 KB) fall back to the global
// atomics on the overflow path. An untouched slot flushes 0, which can never
// beat a real value: rows count from 1 and every x is >= 0, whose ordered
// keys all exceed 0.
#[cube(launch_unchecked)]
fn resolve_x(
    sm: &[f32],
    fl: &[u32],
    lm: &mut [f32],
    lc: &[u32],
    items: &[f32],
    ie: &[u32],
    ir: &[u32],
    wm: &[f32],
    wc: &[u32],
    otb: &[u32],
    row_max: &mut [Atomic<u32>],
    x_max: &mut [Atomic<u32>],
    #[comptime] units: usize,
) {
    let id = ABSOLUTE_POS;
    let n = fl.len();
    let item_count = ir.len() / 2;
    let u = UNIT_POS as usize;
    let srow = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS);
    let sx = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS);
    let mut sbase = Shared::<u32>::new();
    let cube_lo = CUBE_POS * units;
    if u == 0 {
        // The item at this cube's first byte anchors the slot numbering.
        let probe = if cube_lo < n { cube_lo } else { n - 1 };
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
    sync_cube();
    let it_base = *sbase as usize;

    if id < n && (fl[id] & F_LEADER) != 0 && item_count > 0 {
        let it = item_search(ir, item_count, id);
        let io = it * IM_STRIDE;
        let ie_off = it * IE_STRIDE;
        let wrap = ie[ie_off + IE_WRAP_WIDTH] as i32;
        let mut fold = wrap;
        if fold == 0 && ie[ie_off + IE_HAS_PAGE] != 0 {
            fold = ie[ie_off + IE_PAGE_COLS] as i32;
        }
        let col = lc[id * LC_STRIDE + LC_COL] as i32;
        let ord = wc[id] as i32;
        let mut x = 0.0f32;
        if fold > 0 {
            // The forward re-sum from the segment start — the serial segAdv order.
            let mut k = col % fold;
            while k >= 1 {
                let q = otb[ir[it * 2] as usize + (ord - k) as usize] as usize;
                x += sm[q];
                k -= 1;
            }
        } else {
            x = wm[id];
        }
        let row = lc[id * LC_STRIDE + LC_ROW] as i32;
        let seg = wrap_segment_of(col, wrap, (fl[id] & F_NEWLINE) != 0);
        let lh = items[io + IM_LINE_HEIGHT];
        let mo = id * LM_STRIDE;
        let base = x + items[io + IM_ORIGIN_X];
        lm[mo + LM_BASE_X] = base;
        lm[mo + LM_X] = base;
        lm[mo + LM_Y] = (row as f32) * (-lh) + items[io + IM_ORIGIN_Y];
        lm[mo + LM_Z] = (seg as f32) * (-items[io + IM_Z_STEP]) + items[io + IM_ORIGIN_Z];
        let slot = it - it_base;
        if slot < RESOLVE_SLOTS {
            srow[slot].fetch_max((row + 1) as u32);
            sx[slot].fetch_max(ordered_key(x));
        } else {
            row_max[it].fetch_max((row + 1) as u32);
            x_max[it].fetch_max(ordered_key(x));
        }
    }
    sync_cube();
    if u < RESOLVE_SLOTS {
        let it = it_base + u;
        if it < item_count {
            row_max[it].fetch_max(srow[u].load());
            x_max[it].fetch_max(sx[u].load());
        }
    }
}

// ── dispatch 8b: derive the fan stride ON DEVICE — thread per item ───────────
#[cube(launch_unchecked)]
fn derive_stride(x_max: &[u32], ie: &[u32], page_gap_x: &[f32], strides: &mut [f32]) {
    let i = ABSOLUTE_POS;
    let item_count = ie.len() / IE_STRIDE;
    if i < item_count {
        let ie_off = i * IE_STRIDE;
        let has_page = ie[ie_off + IE_HAS_PAGE] != 0;
        let rows = ie[ie_off + IE_PAGE_ROWS] as i32;
        if has_page && rows > 0 {
            strides[i] = key_to_float(x_max[i]) + page_gap_x[i];
        } else {
            strides[i] = 0.0;
        }
    }
}

// ── dispatch 8: paginate — thread per byte, leaders only ─────────────────────
#[cube(launch_unchecked)]
fn paginate(
    lm: &mut [f32],
    fl: &[u32],
    lc: &[u32],
    items: &[f32],
    ie: &[u32],
    ir: &[u32],
    strides: &[f32],
) {
    let id = ABSOLUTE_POS;
    let n = fl.len();
    let item_count = ir.len() / 2;
    if id < n && (fl[id] & F_LEADER) != 0 && item_count > 0 {
        let it = item_search(ir, item_count, id);
        let io = it * IM_STRIDE;
        let ie_off = it * IE_STRIDE;
        let has_page = ie[ie_off + IE_HAS_PAGE] != 0;
        let rows = if has_page { ie[ie_off + IE_PAGE_ROWS] as i32 } else { 0 };
        let cols = if has_page { ie[ie_off + IE_PAGE_COLS] as i32 } else { 0 };
        let scroll = if has_page { ie[ie_off + IE_SCROLL_ROWS] as i32 } else { 0 };
        if rows != 0 || cols != 0 || scroll != 0 {
            let row = lc[id * LC_STRIDE + LC_ROW] as i32;
            let col = lc[id * LC_STRIDE + LC_COL] as i32;
            let screen_row = row - scroll;
            let mut y_page = 0;
            if rows > 0 && screen_row >= rows {
                y_page = screen_row / rows;
            }
            let mut x_page = 0;
            if cols > 0 {
                x_page = col / cols;
            }
            let wide_raw = ie[ie_off + IE_PAGES_WIDE] as i32;
            let wide = if wide_raw > 1 { wide_raw } else { 1 };
            let band = y_page / wide;
            let wrap = ie[ie_off + IE_WRAP_WIDTH] as i32;
            let seg = wrap_segment_of(col, wrap, (fl[id] & F_NEWLINE) != 0);
            let lh = items[io + IM_LINE_HEIGHT];
            let mo = id * LM_STRIDE;
            lm[mo + LM_X] = lm[mo + LM_BASE_X] + (y_page % wide) as f32 * strides[it];
            lm[mo + LM_Y] = items[io + IM_ORIGIN_Y]
                - (screen_row - y_page * rows) as f32 * lh
                - band as f32 * items[io + IM_BAND_STRIDE_Y];
            lm[mo + LM_Z] = items[io + IM_ORIGIN_Z]
                - seg as f32 * items[io + IM_Z_STEP]
                + band as f32 * items[io + IM_DEPTH_PER_BAND]
                + x_page as f32 * items[io + IM_DEPTH_PER_COL];
        }
    }
}

// ── the driver ────────────────────────────────────────────────────────────────

pub fn run(ctx: &GpuContext, fixture_path: &Path) -> ! {
    let fx = crate::fixture::load_pipe_fixture(fixture_path).unwrap_or_else(|e| {
        eprintln!("cubecl-chain-check: {e}");
        std::process::exit(1);
    });
    let n = fx.bytes.len();
    let item_count = fx.items.len();
    // The tile shape, env-overridable so a (units, rake) sweep runs through
    // the same instrument — cross-shape agreement with the CPU scan's
    // (64, 256) chunks is associativity checked in situ.
    let units: usize = std::env::var("GLYPH_CHAIN_TILE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256);
    let rake: usize = std::env::var("GLYPH_CHAIN_RAKE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    assert!(units.is_power_of_two(), "GLYPH_CHAIN_TILE must be a power of two");
    let log = units.ilog2() as usize;
    let n_tiles = n.div_ceil(units * rake).max(1);

    // The CPU reference — a different tree shape at the (64, 256) tuning.
    let r = run_scan_pipeline(&fx.bytes, &fx.trie, &fx.items, DEFAULT_CHUNK_SIZE, DEFAULT_GROUP_SIZE, 1);

    // Uploads: statics from the CPU decode (the bench's mode 0 shape). The
    // measure static is ADVANCE ONLY — the scan never reads height.
    let mut fl = Vec::with_capacity(n);
    let mut sm = Vec::with_capacity(n);
    for i in 0..n {
        fl.push(r.slots.flags(i));
        sm.push(r.slots.advance(i));
    }
    let mut ir = Vec::with_capacity(item_count * 2);
    let mut ie = Vec::with_capacity(item_count * IE_STRIDE);
    let mut im = Vec::with_capacity(item_count * IM_STRIDE);
    let mut page_gap_x = Vec::with_capacity(item_count);
    for item in &fx.items {
        ir.push(item.byte_start as u32);
        ir.push((item.byte_start + item.byte_count) as u32);
        ie.push(item.page_rows as u32);
        ie.push(item.page_cols as u32);
        ie.push(item.scroll_rows as u32);
        ie.push(item.pages_wide as u32);
        ie.push(item.wrap_width as u32);
        ie.push(item.has_page as u32);
        ie.push(match item.wrap_mode {
            WrapMode::Down => 0u32,
            WrapMode::Back => 1,
        });
        ie.push(0u32);
        im.push(item.origin_y as f32);
        im.push(item.origin_z as f32);
        im.push(item.line_height as f32);
        im.push(item.z_step as f32);
        im.push(item.band_stride_y as f32);
        im.push(item.depth_per_band as f32);
        im.push(item.depth_per_col as f32);
        im.push(0.0f32); // IM_PAGE_STRIDE_X: the device chain derives it on device
        im.push(item.origin_x as f32);
        page_gap_x.push(item.page_gap_x as f32);
    }

    let setup = WgpuSetup {
        instance: ctx.instance.clone(),
        adapter: ctx.adapter.clone(),
        device: ctx.device.clone(),
        queue: ctx.queue.clone(),
        backend: AutoGraphicsApi::backend(),
    };
    let cdev = cubecl::wgpu::init_device(setup, Default::default());
    let client = cubecl::Device::Wgpu(cdev).client();

    let h_fl = client.create_from_slice(bytemuck::cast_slice(&fl));
    let h_sm = client.create_from_slice(bytemuck::cast_slice(&sm));
    let h_ir = client.create_from_slice(bytemuck::cast_slice(&ir));
    let h_ie = client.create_from_slice(bytemuck::cast_slice(&ie));
    let h_im = client.create_from_slice(bytemuck::cast_slice(&im));
    let h_gap = client.create_from_slice(bytemuck::cast_slice(&page_gap_x));
    let h_tc = client.empty(n_tiles * PARTIAL_COUNT_STRIDE * 4);
    let h_tm = client.empty(n_tiles * 4);
    let h_xc = client.empty(n_tiles * PARTIAL_COUNT_STRIDE * 4);
    let h_xm = client.empty(n_tiles * 4);
    let h_lc = client.empty(n * LC_STRIDE * 4);
    let h_wm = client.empty(n * 4);
    let h_wc = client.empty(n * 4);
    let h_otb = client.empty(n * 4);
    let h_lm = client.empty(n * LM_STRIDE * 4);
    let h_strides = client.empty(item_count * 4);
    let zeroes = vec![0u32; item_count];
    let h_rmax = client.create_from_slice(bytemuck::cast_slice(&zeroes));
    let h_xmax = client.create_from_slice(bytemuck::cast_slice(&zeroes));

    // This M2's ADAPTER caps workgroups per grid dimension at 65535 (verified
    // live: a 94075-cube dispatch was rejected) — not a wgpu default to lift.
    // Spill into Y; ABSOLUTE_POS is the flattened id across axes, so the
    // kernels need no index change. gpu.rs still requests the adapter's value,
    // so an adapter with a higher cap takes the plain grid automatically.
    let cubes_of = |threads: usize| {
        let cubes = threads.div_ceil(256);
        CubeCount::Static(cubes.min(65535) as u32, cubes.div_ceil(65535) as u32, 1)
    };
    // GLYPH_CHAIN_STAGES=N runs only the first N dispatches (bisection aid).
    let stages: usize = std::env::var("GLYPH_CHAIN_STAGES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6);
    // One cube per tile (dim = units); the byte-wide kernels stay 256-unit.
    let tiles_grid = |tiles: usize| {
        CubeCount::Static(tiles.min(65535) as u32, tiles.div_ceil(65535) as u32, 1)
    };
    let t0 = std::time::Instant::now();
    unsafe {
        tile_scan::launch_unchecked(
            &client,
            tiles_grid(n_tiles),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(h_fl.clone(), n),
            BufferArg::from_raw_parts(h_sm.clone(), n),
            BufferArg::from_raw_parts(h_ir.clone(), item_count * 2),
            BufferArg::from_raw_parts(h_ie.clone(), item_count * IE_STRIDE),
            BufferArg::from_raw_parts(h_tc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
            BufferArg::from_raw_parts(h_tm.clone(), n_tiles),
            units,
            rake,
            log,
        );
        if stages >= 2 {
            spine_scan::launch_unchecked(
                &client,
                CubeCount::new_single(),
                CubeDim::new_1d(units as u32),
                BufferArg::from_raw_parts(h_tc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
                BufferArg::from_raw_parts(h_tm.clone(), n_tiles),
                BufferArg::from_raw_parts(h_xc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
                BufferArg::from_raw_parts(h_xm.clone(), n_tiles),
                units,
                log,
            );
        }
        if stages >= 3 {
            if std::env::var_os("GLYPH_CHAIN_DEBUG").is_some() {
                // Force stages 1-2 to land in their own submission, so a later
                // batch failing cannot take tile_scan's write down with it.
                let probe = client.read_one(h_tc.clone()).expect("pre-apply probe");
                let pv: &[u32] = bytemuck::cast_slice(&probe);
                println!("  dbg pre-apply tc[{} {} {} {}]", pv[0], pv[1], pv[2], pv[3]);
            }
            apply::launch_unchecked(
                &client,
                tiles_grid(n_tiles),
                CubeDim::new_1d(units as u32),
                BufferArg::from_raw_parts(h_fl.clone(), n),
                BufferArg::from_raw_parts(h_sm.clone(), n),
                BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                BufferArg::from_raw_parts(h_ir.clone(), item_count * 2),
                BufferArg::from_raw_parts(h_ie.clone(), item_count * IE_STRIDE),
                BufferArg::from_raw_parts(h_xc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
                BufferArg::from_raw_parts(h_xm.clone(), n_tiles),
                BufferArg::from_raw_parts(h_wm.clone(), n),
                BufferArg::from_raw_parts(h_wc.clone(), n),
                BufferArg::from_raw_parts(h_otb.clone(), n),
                units,
                rake,
                log,
            );
        }
        if stages >= 4 {
            resolve_x::launch_unchecked(
                &client,
                cubes_of(n),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(h_sm.clone(), n),
                BufferArg::from_raw_parts(h_fl.clone(), n),
                BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
                BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                BufferArg::from_raw_parts(h_im.clone(), item_count * IM_STRIDE),
                BufferArg::from_raw_parts(h_ie.clone(), item_count * IE_STRIDE),
                BufferArg::from_raw_parts(h_ir.clone(), item_count * 2),
                BufferArg::from_raw_parts(h_wm.clone(), n),
                BufferArg::from_raw_parts(h_wc.clone(), n),
                BufferArg::from_raw_parts(h_otb.clone(), n),
                BufferArg::from_raw_parts(h_rmax.clone(), item_count),
                BufferArg::from_raw_parts(h_xmax.clone(), item_count),
                256,
            );
        }
        if stages >= 5 {
            derive_stride::launch_unchecked(
                &client,
                cubes_of(item_count),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(h_xmax.clone(), item_count),
                BufferArg::from_raw_parts(h_ie.clone(), item_count * IE_STRIDE),
                BufferArg::from_raw_parts(h_gap.clone(), item_count),
                BufferArg::from_raw_parts(h_strides.clone(), item_count),
            );
        }
        if stages >= 6 {
            paginate::launch_unchecked(
                &client,
                cubes_of(n),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
                BufferArg::from_raw_parts(h_fl.clone(), n),
                BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                BufferArg::from_raw_parts(h_im.clone(), item_count * IM_STRIDE),
                BufferArg::from_raw_parts(h_ie.clone(), item_count * IE_STRIDE),
                BufferArg::from_raw_parts(h_ir.clone(), item_count * 2),
                BufferArg::from_raw_parts(h_strides.clone(), item_count),
            );
        }
    }
    let lc_bytes = client.read_one(h_lc).expect("read lc");
    let wc_bytes = client.read_one(h_wc).expect("read wc");
    let wm_bytes = client.read_one(h_wm).expect("read wm");
    let lm_bytes = client.read_one(h_lm).expect("read lm");
    let rmax_bytes = client.read_one(h_rmax).expect("read rmax");
    let xmax_bytes = client.read_one(h_xmax).expect("read xmax");
    let dt = t0.elapsed();
    if std::env::var_os("GLYPH_CHAIN_DEBUG").is_some() {
        let fl_bytes = client.read_one(h_fl).expect("read fl");
        let flb: &[u32] = bytemuck::cast_slice(&fl_bytes);
        println!(
            "  dbg fl readback: [{} {} {} {} {} {} {} {}]",
            flb[0], flb[1], flb[2], flb[3], flb[4], flb[5], flb[6], flb[7]
        );
        let tc_bytes = client.read_one(h_tc).expect("read tc");
        let xc_bytes = client.read_one(h_xc).expect("read xc");
        let tc: &[u32] = bytemuck::cast_slice(&tc_bytes);
        let xc: &[u32] = bytemuck::cast_slice(&xc_bytes);
        for c in 0..n_tiles.min(3) {
            let o = c * PARTIAL_COUNT_STRIDE;
            println!(
                "  dbg tile {c} tc[reset={} nl={} glyphs={} rows={} head={} tail={} wrap={} mode={}] xc[same={}]",
                tc[o], tc[o + 1], tc[o + 2], tc[o + 3], tc[o + 4], tc[o + 5], tc[o + 6], tc[o + 7],
                xc[o] == tc[o] && xc[o + 2] == tc[o + 2]
            );
        }
    }
    let lc: &[u32] = bytemuck::cast_slice(&lc_bytes);
    let wc: &[u32] = bytemuck::cast_slice(&wc_bytes);
    let wm: &[f32] = bytemuck::cast_slice(&wm_bytes);
    let lm: &[f32] = bytemuck::cast_slice(&lm_bytes);
    let rmax: &[u32] = bytemuck::cast_slice(&rmax_bytes);
    let xmax: &[u32] = bytemuck::cast_slice(&xmax_bytes);

    // The item maxima, diffed DIRECTLY against the CPU fold's item_bounds
    // lanes (TOTAL_ROWS, MAX_ROW_EXTENT) — the shared-atomic reduction in
    // resolve_x is invisible to the lm lanes on unpaged fixtures, so without
    // this it would have no witness at all.
    let host_key_to_float = |k: u32| -> f32 {
        let b = if (k & 0x8000_0000) != 0 { k & 0x7FFF_FFFF } else { !k };
        f32::from_bits(b)
    };
    let mut bad = 0usize;
    let mut max_x_dev = 0.0f64;
    if stages >= 4 {
        for i in 0..item_count {
            let want_rows = r.item_bounds[i * 8 + 6];
            if rmax[i] as f64 != want_rows {
                if bad < 8 {
                    println!("  MISMATCH item {i} total_rows: cpu {want_rows} gpu {}", rmax[i]);
                }
                bad += 1;
            }
            let want_x = r.item_bounds[i * 8 + 7];
            let got_x = host_key_to_float(xmax[i]) as f64;
            let x_dev = (got_x - want_x).abs() / want_x.abs().max(1.0);
            if x_dev > max_x_dev {
                max_x_dev = x_dev;
            }
        }
    }

    // The diff: counts bit-exact; line_advance and positions reported with
    // max deviation and held to the oracle's 1e-4 eps tier (the module
    // header's contract note — the Blelloch tree reassociates tail_adv).
    let mut max_pos_dev = 0.0f64;
    let mut max_line_dev = 0.0f64;
    let mut leaders = 0usize;
    for id in 0..n {
        if r.slots.flags(id) & F_LEADER == 0 {
            continue;
        }
        leaders += 1;
        let checks = [
            (r.slots.row(id), lc[id * LC_STRIDE + LC_ROW] as i64, "row"),
            (r.slots.col(id), lc[id * LC_STRIDE + LC_COL] as i64, "col"),
            (r.slots.wc[id] as i64, wc[id] as i64, "ord"),
        ];
        for (want, got, name) in checks {
            if want != got {
                if bad < 8 {
                    println!("  MISMATCH byte {id} {name}: cpu {want} gpu {got}");
                }
                bad += 1;
            }
        }
        let la_cpu = r.slots.wm[id] as f64;
        let la_rel = (wm[id] as f64 - la_cpu).abs() / la_cpu.abs().max(1.0);
        if la_rel > max_line_dev {
            max_line_dev = la_rel;
        }
        for (k, acc) in [(LM_X, r.slots.x(id)), (LM_Y, r.slots.y(id)), (LM_Z, r.slots.z(id))] {
            let dev = (lm[id * LM_STRIDE + k] as f64 - acc as f64).abs();
            let rel = dev / (acc as f64).abs().max(1.0);
            if rel > max_pos_dev {
                max_pos_dev = rel;
            }
        }
    }

    println!(
        "cubecl-chain-check: {} ({} B, {} items, {} leaders, tile {}x{}) — {} count-lane mismatches, \
         max line_adv deviation {:.2e}, max x-extent deviation {:.2e}, max position deviation {:.2e}; \
         chain+readbacks {:?} (smoke timing only)",
        fx.name,
        n,
        item_count,
        leaders,
        units,
        rake,
        bad,
        max_line_dev,
        max_x_dev,
        max_pos_dev,
        dt
    );
    if bad > 0 || max_line_dev > 1e-4 || max_x_dev > 1e-4 || max_pos_dev > 1e-4 {
        eprintln!(
            "cubecl-chain-check FAIL: {bad} count mismatches, {max_line_dev:.2e} line_adv, {max_x_dev:.2e} x-extent, {max_pos_dev:.2e} position deviation"
        );
        std::process::exit(1);
    }
    println!("cubecl-chain-check PASS: counts bit-exact, maxima + line_advance + positions inside 1e-4");
    std::process::exit(0);
}

// ── the bench driver ──────────────────────────────────────────────────────────

/// `--cubecl-chain-bench <corpus>`: the chain over a raw file as ONE item.
///
/// Timing is PER-DISPATCH GPU WINDOWS (`profile_start`/`profile_end` — device
/// timestamps when the shared device carries TIMESTAMP_QUERY, which gpu.rs
/// requests unconditionally where supported), `GLYPH_CHAIN_LOOP` samples per
/// dispatch with the MINIMUM kept (the "run it a few times" rule, automated).
/// Each window flushes, so stages cannot overlap: these are per-dispatch
/// latencies in the same posture as the Mojo bench's `mark()` table, and the
/// sum of minima is the chain estimate — the chain is dependency-serialized,
/// so nothing is lost to that. The old batched wall-clock loop mode is
/// retired: it measured repeat-overlap, not the chain.
///
/// `GLYPH_CHAIN_WRAP=<width>` swaps the single item to wrap_width>0 /
/// WRAP_DOWN so the segment re-sum in resolve_x actually executes — the plain
/// shape leaves that path dead at fold==0 and measures only atomic
/// throughput.
pub fn bench(ctx: &GpuContext, corpus_path: &Path) -> ! {
    let bytes = std::fs::read(corpus_path).unwrap_or_else(|e| {
        eprintln!("cubecl-chain-bench: {e}");
        std::process::exit(1);
    });
    let n = bytes.len();
    // The tile shape (the same dials as the check instrument, so a sweep
    // measures exactly what the fixtures verify).
    let units: usize = std::env::var("GLYPH_CHAIN_TILE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256);
    let rake: usize = std::env::var("GLYPH_CHAIN_RAKE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    assert!(units.is_power_of_two(), "GLYPH_CHAIN_TILE must be a power of two");
    let log = units.ilog2() as usize;
    let n_tiles = n.div_ceil(units * rake).max(1);
    // The wrapped shape: fold>0 makes resolve_x take the re-sum path.
    let wrap_width: i64 = std::env::var("GLYPH_CHAIN_WRAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let item = crate::fold::Item {
        byte_start: 0,
        byte_count: n as i64,
        origin_x: 0.0,
        origin_y: 0.0,
        origin_z: 0.0,
        wrap_width,
        wrap_mode: WrapMode::Down,
        cluster_mode: crate::fold::ClusterMode::Leader,
        z_step: 2.0,
        line_height: 1.25,
        has_page: false,
        page_rows: 0,
        page_cols: 0,
        scroll_rows: 0,
        pages_wide: 1,
        page_gap_x: 0.0,
        band_stride_y: 0.0,
        depth_per_band: 0.0,
        depth_per_col: 0.0,
        page_line_height: 1.25,
    };
    let items = [item];
    let trie = crate::atlas::TrieTable::load(&crate::atlas_dir());

    let t_decode = std::time::Instant::now();
    let r = run_scan_pipeline(&bytes, &trie, &items, DEFAULT_CHUNK_SIZE, DEFAULT_GROUP_SIZE, 1);
    let decode_dt = t_decode.elapsed();

    let mut fl = Vec::with_capacity(n);
    let mut sm = Vec::with_capacity(n);
    for i in 0..n {
        fl.push(r.slots.flags(i));
        sm.push(r.slots.advance(i));
    }
    let ir: Vec<u32> = vec![0, n as u32];
    let ie: Vec<u32> = vec![0, 0, 0, 1, wrap_width as u32, 0, 0, 0];
    let im: Vec<f32> = vec![0.0, 0.0, 1.25, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let page_gap_x: Vec<f32> = vec![0.0];

    let setup = WgpuSetup {
        instance: ctx.instance.clone(),
        adapter: ctx.adapter.clone(),
        device: ctx.device.clone(),
        queue: ctx.queue.clone(),
        backend: AutoGraphicsApi::backend(),
    };
    let cdev = cubecl::wgpu::init_device(setup, Default::default());
    let client = cubecl::Device::Wgpu(cdev).client();
    let h_fl = client.create_from_slice(bytemuck::cast_slice(&fl));
    let h_sm = client.create_from_slice(bytemuck::cast_slice(&sm));
    let h_ir = client.create_from_slice(bytemuck::cast_slice(&ir));
    let h_ie = client.create_from_slice(bytemuck::cast_slice(&ie));
    let h_im = client.create_from_slice(bytemuck::cast_slice(&im));
    let h_gap = client.create_from_slice(bytemuck::cast_slice(&page_gap_x));
    let h_tc = client.empty(n_tiles * PARTIAL_COUNT_STRIDE * 4);
    let h_tm = client.empty(n_tiles * 4);
    let h_xc = client.empty(n_tiles * PARTIAL_COUNT_STRIDE * 4);
    let h_xm = client.empty(n_tiles * 4);
    let h_lc = client.empty(n * LC_STRIDE * 4);
    let h_wm = client.empty(n * 4);
    let h_wc = client.empty(n * 4);
    let h_otb = client.empty(n * 4);
    let h_lm = client.empty(n * LM_STRIDE * 4);
    let h_strides = client.empty(4);
    let h_rmax = client.create_from_slice(bytemuck::cast_slice(&[0u32]));
    let h_xmax = client.create_from_slice(bytemuck::cast_slice(&[0u32]));

    // This M2's ADAPTER caps workgroups per grid dimension at 65535 (verified
    // live: a 94075-cube dispatch was rejected) — not a wgpu default to lift.
    // Spill into Y; ABSOLUTE_POS is the flattened id across axes, so the
    // kernels need no index change. gpu.rs still requests the adapter's value,
    // so an adapter with a higher cap takes the plain grid automatically.
    let cubes_of = |threads: usize| {
        let cubes = threads.div_ceil(256);
        CubeCount::Static(cubes.min(65535) as u32, cubes.div_ceil(65535) as u32, 1)
    };
    let stages: usize = std::env::var("GLYPH_CHAIN_STAGES").ok().and_then(|v| v.parse().ok()).unwrap_or(6);
    // One cube per tile (dim = units); the byte-wide kernels stay 256-unit.
    let tiles_grid = |tiles: usize| {
        CubeCount::Static(tiles.min(65535) as u32, tiles.div_ceil(65535) as u32, 1)
    };
    // Timed samples per dispatch; the minimum is reported.
    let samples: usize = std::env::var("GLYPH_CHAIN_LOOP").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
    let launch = |s: usize| {
        unsafe {
            match s {
                0 => {
                    tile_scan::launch_unchecked(
                        &client,
                        tiles_grid(n_tiles),
                        CubeDim::new_1d(units as u32),
                        BufferArg::from_raw_parts(h_fl.clone(), n),
                        BufferArg::from_raw_parts(h_sm.clone(), n),
                        BufferArg::from_raw_parts(h_ir.clone(), 2),
                        BufferArg::from_raw_parts(h_ie.clone(), IE_STRIDE),
                        BufferArg::from_raw_parts(h_tc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
                        BufferArg::from_raw_parts(h_tm.clone(), n_tiles),
                        units,
                        rake,
                        log,
                    );
                }
                1 => {
                    spine_scan::launch_unchecked(
                        &client,
                        CubeCount::new_single(),
                        CubeDim::new_1d(units as u32),
                        BufferArg::from_raw_parts(h_tc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
                        BufferArg::from_raw_parts(h_tm.clone(), n_tiles),
                        BufferArg::from_raw_parts(h_xc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
                        BufferArg::from_raw_parts(h_xm.clone(), n_tiles),
                        units,
                        log,
                    );
                }
                2 => {
                    apply::launch_unchecked(
                        &client,
                        tiles_grid(n_tiles),
                        CubeDim::new_1d(units as u32),
                        BufferArg::from_raw_parts(h_fl.clone(), n),
                        BufferArg::from_raw_parts(h_sm.clone(), n),
                        BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                        BufferArg::from_raw_parts(h_ir.clone(), 2),
                        BufferArg::from_raw_parts(h_ie.clone(), IE_STRIDE),
                        BufferArg::from_raw_parts(h_xc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
                        BufferArg::from_raw_parts(h_xm.clone(), n_tiles),
                        BufferArg::from_raw_parts(h_wm.clone(), n),
                        BufferArg::from_raw_parts(h_wc.clone(), n),
                        BufferArg::from_raw_parts(h_otb.clone(), n),
                        units,
                        rake,
                        log,
                    );
                }
                3 => {
                    resolve_x::launch_unchecked(
                        &client,
                        cubes_of(n),
                        CubeDim::new_1d(256),
                        BufferArg::from_raw_parts(h_sm.clone(), n),
                        BufferArg::from_raw_parts(h_fl.clone(), n),
                        BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
                        BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                        BufferArg::from_raw_parts(h_im.clone(), IM_STRIDE),
                        BufferArg::from_raw_parts(h_ie.clone(), IE_STRIDE),
                        BufferArg::from_raw_parts(h_ir.clone(), 2),
                        BufferArg::from_raw_parts(h_wm.clone(), n),
                        BufferArg::from_raw_parts(h_wc.clone(), n),
                        BufferArg::from_raw_parts(h_otb.clone(), n),
                        BufferArg::from_raw_parts(h_rmax.clone(), 1),
                        BufferArg::from_raw_parts(h_xmax.clone(), 1),
                        256,
                    );
                }
                4 => {
                    derive_stride::launch_unchecked(
                        &client,
                        CubeCount::new_single(),
                        CubeDim::new_1d(1),
                        BufferArg::from_raw_parts(h_xmax.clone(), 1),
                        BufferArg::from_raw_parts(h_ie.clone(), IE_STRIDE),
                        BufferArg::from_raw_parts(h_gap.clone(), 1),
                        BufferArg::from_raw_parts(h_strides.clone(), 1),
                    );
                }
                _ => {
                    paginate::launch_unchecked(
                        &client,
                        cubes_of(n),
                        CubeDim::new_1d(256),
                        BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
                        BufferArg::from_raw_parts(h_fl.clone(), n),
                        BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                        BufferArg::from_raw_parts(h_im.clone(), IM_STRIDE),
                        BufferArg::from_raw_parts(h_ie.clone(), IE_STRIDE),
                        BufferArg::from_raw_parts(h_ir.clone(), 2),
                        BufferArg::from_raw_parts(h_strides.clone(), 1),
                    );
                }
            }
        }
    };
    // The per-dispatch GPU windows. Every stage runs `samples` times; the
    // minimum survives. A window that resolved to no measurement counts as a
    // missing sample, never as a zero.
    let stage_names = [
        "tile_scan",
        "spine_scan",
        "apply",
        "resolve_x",
        "derive_stride",
        "paginate",
    ];
    let stage_meta = |s: usize| -> (&'static str, usize, u32) {
        let (cubes, dim) = match s {
            0 | 2 => (n_tiles, units as u32),
            1 => (1, units as u32),
            4 => (1, 1),
            _ => (n.div_ceil(256), 256),
        };
        (stage_names[s], cubes, dim)
    };
    let mut mins: Vec<Option<std::time::Duration>> = vec![None; stages];
    let mut missing_windows = 0usize;
    let mut timing_method = String::new();
    for _ in 0..samples {
        for (s, slot) in mins.iter_mut().enumerate() {
            let window = client.profile_start().expect("profile_start");
            launch(s);
            let dur = client.profile_end(window).expect("profile_end");
            if timing_method.is_empty() {
                timing_method = format!("{}", dur.timing_method());
            }
            match pollster::block_on(dur.resolve()) {
                Some(ticks) => {
                    let d = ticks.duration();
                    *slot = Some(slot.map_or(d, |cur| cur.min(d)));
                }
                None => missing_windows += 1,
            }
        }
    }

    // Readbacks: host-side, wall clock (the product flow binds instead).
    let t1 = std::time::Instant::now();
    let lc_bytes = client.read_one(h_lc.clone()).expect("read lc");
    let _wc = client.read_one(h_wc.clone()).expect("read wc");
    let _wm = client.read_one(h_wm.clone()).expect("read wm");
    let _lm = client.read_one(h_lm.clone()).expect("read lm");
    let readback_dt = t1.elapsed();
    // Correctness at speed: the bench's whole number is worthless if the fast
    // path is wrong — diff the leader row/col lanes against the CPU reference.
    let lc: &[u32] = bytemuck::cast_slice(&lc_bytes);
    if stages < 3 {
        println!(
            "cubecl-chain-bench: {} ({} B, {} tiles @ {}x{}, wrap {}) stages {} — pre-apply stages only, no verification",
            corpus_path.display(),
            n,
            n_tiles,
            units,
            rake,
            wrap_width,
            stages
        );
        for (s, m) in mins.iter().enumerate() {
            let (name, cubes, dim) = stage_meta(s);
            println!("  {name:<14} cubes={cubes} units={dim} min={m:?}");
        }
        std::process::exit(0);
    }
    let mut bad = 0usize;
    for id in 0..n {
        if r.slots.flags(id) & F_LEADER == 0 {
            continue;
        }
        if r.slots.row(id) != lc[id * LC_STRIDE + LC_ROW] as i64
            || r.slots.col(id) != lc[id * LC_STRIDE + LC_COL] as i64
        {
            bad += 1;
        }
    }
    assert_eq!(bad, 0, "bench verification failed: {bad} leader lane mismatches");

    let chain: std::time::Duration = mins.iter().filter_map(|d| *d).sum();
    let total = chain + readback_dt;
    println!(
        "cubecl-chain-bench: {} ({} B, {} tiles @ {}x{}, wrap {}, samples {}) timing={} missing_windows={} — \
         cpu decode+scan {:?} | chain (sum of per-dispatch minima) {:?} readbacks {:?} total {:?} ({:.1} MB/s)",
        corpus_path.display(),
        n,
        n_tiles,
        units,
        rake,
        wrap_width,
        samples,
        timing_method,
        missing_windows,
        decode_dt,
        chain,
        readback_dt,
        total,
        n as f64 / 1e6 / total.as_secs_f64()
    );
    for (s, m) in mins.iter().enumerate() {
        let (name, cubes, dim) = stage_meta(s);
        println!("  {name:<14} cubes={cubes:>7} units={dim} min={m:?}");
    }
    std::process::exit(0);
}
