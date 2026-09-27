//! CubeCL scan skeleton — dev-only (`--cubecl-chain-check`), the note-16 phase 2.
//!
//! The full raking scan as CubeCL kernels over the shared device:
//!
//!   chunkReduce -> spineReduce -> spineScan -> partialScan
//!                -> apply -> resolveX -> deriveStride -> paginate
//!
//! Every kernel is a line-for-line transcription of the Mojo device chain
//! (`engine/gpu_kernels.mojo` / `gpu_monoid.mojo`), which is itself a
//! line-for-line mirror of `scan.rs` — and the CPU reference here is
//! `scan.rs::run_scan_pipeline` at the same (64, 256) tuning, so the
//! validation chain stays fixtures -> Rust CPU -> Rust GPU with no Mojo in
//! the loop. Count lanes (lc/wc, otb) diff BIT-EXACT; the f32 line-advance
//! diffs bit-exact too (same grouping, same add order); positions (lm)
//! report max deviation against the f64-narrowed CPU lanes (device f32
//! arithmetic, InstCombinePass included — the phase-0 measurement said ≤1
//! ulp, and this is where that shows up in situ).

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
const SM_STRIDE: usize = 2;
const SM_ADVANCE: usize = 0;
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

// ── dispatch 2: chunkReduce — thread per chunk ───────────────────────────────
#[cube(launch_unchecked)]
fn chunk_reduce(
    fl: &[u32],
    sm: &[f32],
    ir: &[u32],
    ie: &[u32],
    pc: &mut [u32],
    pm: &mut [f32],
    #[comptime] chunk: usize,
) {
    let c = ABSOLUTE_POS;
    let n = fl.len();
    let item_count = ir.len() / 2;
    let lo = c * chunk;
    if lo < n {
        let hi = if lo + chunk < n { lo + chunk } else { n };
        // ItemWalk seed.
        let mut it = 0usize;
        let mut start = 0usize;
        let mut nxt = n;
        let mut w_wrap = 0i32;
        let mut w_mode = 0i32;
        let has = item_count > 0;
        if has {
            it = item_search(ir, item_count, lo);
            start = ir[it * 2] as usize;
            nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
            w_wrap = ie[it * IE_STRIDE + IE_WRAP_WIDTH] as i32;
            w_mode = ie[it * IE_STRIDE + IE_WRAP_MODE] as i32;
        }
        let mut acc = identity();
        let mut id = lo;
        while id < hi {
            // ItemWalk step.
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
        p_store(pc, pm, c, &acc);
    }
}

// ── dispatch 3: spineReduce — thread per group ───────────────────────────────
#[cube(launch_unchecked)]
fn spine_reduce(pc: &[u32], pm: &[f32], uc: &mut [u32], um: &mut [f32], #[comptime] group: usize) {
    let sg = ABSOLUTE_POS;
    let n_chunks = pc.len() / PARTIAL_COUNT_STRIDE;
    let first = sg * group;
    if first < n_chunks {
        let last = if first + group < n_chunks { first + group } else { n_chunks };
        let mut acc = identity();
        for c in first..last {
            let e = p_load(pc, pm, c);
            combine(&mut acc, &e);
        }
        p_store(uc, um, sg, &acc);
    }
}

// ── dispatch 4: spineScan — ONE thread, exclusive scan of the supers ─────────
#[cube(launch_unchecked)]
fn spine_scan(uc: &[u32], um: &[f32], fc: &mut [u32], fm: &mut [f32]) {
    if ABSOLUTE_POS == 0 {
        let n_supers = uc.len() / PARTIAL_COUNT_STRIDE;
        let mut acc = identity();
        for sg in 0..n_supers {
            p_store(fc, fm, sg, &acc); // exclusive: store BEFORE combining
            let e = p_load(uc, um, sg);
            combine(&mut acc, &e);
        }
    }
}

// ── dispatch 5: partialScan — thread per group ───────────────────────────────
#[cube(launch_unchecked)]
fn partial_scan(
    pc: &[u32],
    pm: &[f32],
    fc: &[u32],
    fm: &[f32],
    xc: &mut [u32],
    xm: &mut [f32],
    #[comptime] group: usize,
) {
    let sg = ABSOLUTE_POS;
    let n_chunks = pc.len() / PARTIAL_COUNT_STRIDE;
    let first = sg * group;
    if first < n_chunks {
        let last = if first + group < n_chunks { first + group } else { n_chunks };
        let mut acc = p_load(fc, fm, sg);
        for c in first..last {
            p_store(xc, xm, c, &acc); // exclusive: store BEFORE combining
            let e = p_load(pc, pm, c);
            combine(&mut acc, &e);
        }
    }
}

// ── dispatch 6: apply — thread per chunk, re-fold and write the lanes ────────
#[cube(launch_unchecked)]
fn k_apply(
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
    #[comptime] chunk: usize,
) {
    let c = ABSOLUTE_POS;
    let n = fl.len();
    let item_count = ir.len() / 2;
    let lo = c * chunk;
    if lo < n {
        let hi = if lo + chunk < n { lo + chunk } else { n };
        let mut it = 0usize;
        let mut start = 0usize;
        let mut nxt = n;
        let mut w_wrap = 0i32;
        let mut w_mode = 0i32;
        let has = item_count > 0;
        if has {
            it = item_search(ir, item_count, lo);
            start = ir[it * 2] as usize;
            nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
            w_wrap = ie[it * IE_STRIDE + IE_WRAP_WIDTH] as i32;
            w_mode = ie[it * IE_STRIDE + IE_WRAP_MODE] as i32;
        }
        let mut run = p_load(xc, xm, c);
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

// ── dispatch 7: resolveX — thread per byte, leaders only ─────────────────────
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
) {
    let id = ABSOLUTE_POS;
    let n = fl.len();
    let item_count = ir.len() / 2;
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
                x += sm[q * SM_STRIDE + SM_ADVANCE];
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
        row_max[it].fetch_max((row + 1) as u32);
        x_max[it].fetch_max(ordered_key(x));
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
    let chunk = DEFAULT_CHUNK_SIZE;
    let group = DEFAULT_GROUP_SIZE;
    let n_chunks = n.div_ceil(chunk);
    let n_supers = n_chunks.div_ceil(group);

    // The CPU reference at the same tuning.
    let r = run_scan_pipeline(&fx.bytes, &fx.trie, &fx.items, chunk, group, 1);

    // Uploads: statics from the CPU decode (the bench's mode 0 shape).
    let mut fl = Vec::with_capacity(n);
    let mut sm = Vec::with_capacity(n * SM_STRIDE);
    for i in 0..n {
        fl.push(r.slots.flags(i));
        sm.push(r.slots.advance(i));
        sm.push(r.slots.height(i));
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
    let h_pc = client.empty(n_chunks * PARTIAL_COUNT_STRIDE * 4);
    let h_pm = client.empty(n_chunks * 4);
    let h_uc = client.empty(n_supers * PARTIAL_COUNT_STRIDE * 4);
    let h_um = client.empty(n_supers * 4);
    let h_fc = client.empty(n_supers * PARTIAL_COUNT_STRIDE * 4);
    let h_fm = client.empty(n_supers * 4);
    let h_xc = client.empty(n_chunks * PARTIAL_COUNT_STRIDE * 4);
    let h_xm = client.empty(n_chunks * 4);
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
        .unwrap_or(8);
    let t0 = std::time::Instant::now();
    unsafe {
        chunk_reduce::launch_unchecked(
            &client,
            cubes_of(n_chunks),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_fl.clone(), n),
            BufferArg::from_raw_parts(h_sm.clone(), n * SM_STRIDE),
            BufferArg::from_raw_parts(h_ir.clone(), item_count * 2),
            BufferArg::from_raw_parts(h_ie.clone(), item_count * IE_STRIDE),
            BufferArg::from_raw_parts(h_pc.clone(), n_chunks * PARTIAL_COUNT_STRIDE),
            BufferArg::from_raw_parts(h_pm.clone(), n_chunks),
            chunk,
        );
        if stages >= 2 {
            spine_reduce::launch_unchecked(
                &client,
                cubes_of(n_supers),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(h_pc.clone(), n_chunks * PARTIAL_COUNT_STRIDE),
                BufferArg::from_raw_parts(h_pm.clone(), n_chunks),
                BufferArg::from_raw_parts(h_uc.clone(), n_supers * PARTIAL_COUNT_STRIDE),
                BufferArg::from_raw_parts(h_um.clone(), n_supers),
                group,
            );
        }
        if stages >= 3 {
            spine_scan::launch_unchecked(
                &client,
                CubeCount::new_single(),
                CubeDim::new_1d(1),
                BufferArg::from_raw_parts(h_uc.clone(), n_supers * PARTIAL_COUNT_STRIDE),
                BufferArg::from_raw_parts(h_um.clone(), n_supers),
                BufferArg::from_raw_parts(h_fc.clone(), n_supers * PARTIAL_COUNT_STRIDE),
                BufferArg::from_raw_parts(h_fm.clone(), n_supers),
            );
        }
        if stages >= 4 {
            partial_scan::launch_unchecked(
                &client,
                cubes_of(n_supers),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(h_pc.clone(), n_chunks * PARTIAL_COUNT_STRIDE),
                BufferArg::from_raw_parts(h_pm.clone(), n_chunks),
                BufferArg::from_raw_parts(h_fc.clone(), n_supers * PARTIAL_COUNT_STRIDE),
                BufferArg::from_raw_parts(h_fm.clone(), n_supers),
                BufferArg::from_raw_parts(h_xc.clone(), n_chunks * PARTIAL_COUNT_STRIDE),
                BufferArg::from_raw_parts(h_xm.clone(), n_chunks),
                group,
            );
        }
        if stages >= 5 {
            if std::env::var_os("GLYPH_CHAIN_DEBUG").is_some() {
                // Force stages 1-4 to land in their own submission, so a later
                // batch failing cannot take chunk_reduce's write down with it.
                let probe = client.read_one(h_pc.clone()).expect("pre-apply probe");
                let pv: &[u32] = bytemuck::cast_slice(&probe);
                println!("  dbg pre-apply pc[{} {} {} {}]", pv[0], pv[1], pv[2], pv[3]);
            }
            k_apply::launch_unchecked(
                &client,
                cubes_of(n_chunks),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(h_fl.clone(), n),
                BufferArg::from_raw_parts(h_sm.clone(), n * SM_STRIDE),
                BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                BufferArg::from_raw_parts(h_ir.clone(), item_count * 2),
                BufferArg::from_raw_parts(h_ie.clone(), item_count * IE_STRIDE),
                BufferArg::from_raw_parts(h_xc.clone(), n_chunks * PARTIAL_COUNT_STRIDE),
                BufferArg::from_raw_parts(h_xm.clone(), n_chunks),
                BufferArg::from_raw_parts(h_wm.clone(), n),
                BufferArg::from_raw_parts(h_wc.clone(), n),
                BufferArg::from_raw_parts(h_otb.clone(), n),
                chunk,
            );
        }
        if stages >= 6 {
            resolve_x::launch_unchecked(
                &client,
                cubes_of(n),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(h_sm.clone(), n * SM_STRIDE),
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
            );
        }
        if stages >= 7 {
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
        if stages >= 8 {
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
    let dt = t0.elapsed();
    if std::env::var_os("GLYPH_CHAIN_DEBUG").is_some() {
        let fl_bytes = client.read_one(h_fl).expect("read fl");
        let flb: &[u32] = bytemuck::cast_slice(&fl_bytes);
        println!(
            "  dbg fl readback: [{} {} {} {} {} {} {} {}]",
            flb[0], flb[1], flb[2], flb[3], flb[4], flb[5], flb[6], flb[7]
        );
        let pc_bytes = client.read_one(h_pc).expect("read pc");
        let xc_bytes = client.read_one(h_xc).expect("read xc");
        let pc: &[u32] = bytemuck::cast_slice(&pc_bytes);
        let xc: &[u32] = bytemuck::cast_slice(&xc_bytes);
        for c in 0..n_chunks.min(3) {
            let o = c * PARTIAL_COUNT_STRIDE;
            println!(
                "  dbg chunk {c} pc[reset={} nl={} glyphs={} rows={} head={} tail={} wrap={} mode={}] xc[same={}]",
                pc[o], pc[o + 1], pc[o + 2], pc[o + 3], pc[o + 4], pc[o + 5], pc[o + 6], pc[o + 7],
                xc[o] == pc[o] && xc[o + 2] == pc[o + 2]
            );
        }
    }
    let lc: &[u32] = bytemuck::cast_slice(&lc_bytes);
    let wc: &[u32] = bytemuck::cast_slice(&wc_bytes);
    let wm: &[f32] = bytemuck::cast_slice(&wm_bytes);
    let lm: &[f32] = bytemuck::cast_slice(&lm_bytes);

    // The diff: counts bit-exact, floats reported with max deviation.
    let mut bad = 0usize;
    let mut max_pos_dev = 0.0f64;
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
        if r.slots.wm[id].to_bits() != wm[id].to_bits() {
            if bad < 8 {
                println!("  MISMATCH byte {id} line_advance: cpu {:e} gpu {:e}", r.slots.wm[id], wm[id]);
            }
            bad += 1;
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
        "cubecl-chain-check: {} ({} B, {} items, {} leaders) — {} count-lane mismatches, \
         max position deviation {:.2e}; chain+readbacks {:?} (smoke timing only)",
        fx.name, n, item_count, leaders, bad, max_pos_dev, dt
    );
    if bad > 0 || max_pos_dev > 1e-4 {
        eprintln!("cubecl-chain-check FAIL: {bad} count mismatches, {max_pos_dev:.2e} position deviation");
        std::process::exit(1);
    }
    println!("cubecl-chain-check PASS: counts + line_advance bit-exact, positions inside 1e-4");
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
    let chunk = DEFAULT_CHUNK_SIZE;
    let group = DEFAULT_GROUP_SIZE;
    let n_chunks = n.div_ceil(chunk);
    let n_supers = n_chunks.div_ceil(group);
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
    let r = run_scan_pipeline(&bytes, &trie, &items, chunk, group, 1);
    let decode_dt = t_decode.elapsed();

    let mut fl = Vec::with_capacity(n);
    let mut sm = Vec::with_capacity(n * SM_STRIDE);
    for i in 0..n {
        fl.push(r.slots.flags(i));
        sm.push(r.slots.advance(i));
        sm.push(r.slots.height(i));
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
    let h_pc = client.empty(n_chunks * PARTIAL_COUNT_STRIDE * 4);
    let h_pm = client.empty(n_chunks * 4);
    let h_uc = client.empty(n_supers * PARTIAL_COUNT_STRIDE * 4);
    let h_um = client.empty(n_supers * 4);
    let h_fc = client.empty(n_supers * PARTIAL_COUNT_STRIDE * 4);
    let h_fm = client.empty(n_supers * 4);
    let h_xc = client.empty(n_chunks * PARTIAL_COUNT_STRIDE * 4);
    let h_xm = client.empty(n_chunks * 4);
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
    let stages: usize = std::env::var("GLYPH_CHAIN_STAGES").ok().and_then(|v| v.parse().ok()).unwrap_or(8);
    // Timed samples per dispatch; the minimum is reported.
    let samples: usize = std::env::var("GLYPH_CHAIN_LOOP").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
    let launch = |s: usize| {
        unsafe {
            match s {
                0 => {
                    chunk_reduce::launch_unchecked(
                        &client,
                        cubes_of(n_chunks),
                        CubeDim::new_1d(256),
                        BufferArg::from_raw_parts(h_fl.clone(), n),
                        BufferArg::from_raw_parts(h_sm.clone(), n * SM_STRIDE),
                        BufferArg::from_raw_parts(h_ir.clone(), 2),
                        BufferArg::from_raw_parts(h_ie.clone(), IE_STRIDE),
                        BufferArg::from_raw_parts(h_pc.clone(), n_chunks * PARTIAL_COUNT_STRIDE),
                        BufferArg::from_raw_parts(h_pm.clone(), n_chunks),
                        chunk,
                    );
                }
                1 => {
                    spine_reduce::launch_unchecked(
                        &client,
                        cubes_of(n_supers),
                        CubeDim::new_1d(256),
                        BufferArg::from_raw_parts(h_pc.clone(), n_chunks * PARTIAL_COUNT_STRIDE),
                        BufferArg::from_raw_parts(h_pm.clone(), n_chunks),
                        BufferArg::from_raw_parts(h_uc.clone(), n_supers * PARTIAL_COUNT_STRIDE),
                        BufferArg::from_raw_parts(h_um.clone(), n_supers),
                        group,
                    );
                }
                2 => {
                    spine_scan::launch_unchecked(
                        &client,
                        CubeCount::new_single(),
                        CubeDim::new_1d(1),
                        BufferArg::from_raw_parts(h_uc.clone(), n_supers * PARTIAL_COUNT_STRIDE),
                        BufferArg::from_raw_parts(h_um.clone(), n_supers),
                        BufferArg::from_raw_parts(h_fc.clone(), n_supers * PARTIAL_COUNT_STRIDE),
                        BufferArg::from_raw_parts(h_fm.clone(), n_supers),
                    );
                }
                3 => {
                    partial_scan::launch_unchecked(
                        &client,
                        cubes_of(n_supers),
                        CubeDim::new_1d(256),
                        BufferArg::from_raw_parts(h_pc.clone(), n_chunks * PARTIAL_COUNT_STRIDE),
                        BufferArg::from_raw_parts(h_pm.clone(), n_chunks),
                        BufferArg::from_raw_parts(h_fc.clone(), n_supers * PARTIAL_COUNT_STRIDE),
                        BufferArg::from_raw_parts(h_fm.clone(), n_supers),
                        BufferArg::from_raw_parts(h_xc.clone(), n_chunks * PARTIAL_COUNT_STRIDE),
                        BufferArg::from_raw_parts(h_xm.clone(), n_chunks),
                        group,
                    );
                }
                4 => {
                    k_apply::launch_unchecked(
                        &client,
                        cubes_of(n_chunks),
                        CubeDim::new_1d(256),
                        BufferArg::from_raw_parts(h_fl.clone(), n),
                        BufferArg::from_raw_parts(h_sm.clone(), n * SM_STRIDE),
                        BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                        BufferArg::from_raw_parts(h_ir.clone(), 2),
                        BufferArg::from_raw_parts(h_ie.clone(), IE_STRIDE),
                        BufferArg::from_raw_parts(h_xc.clone(), n_chunks * PARTIAL_COUNT_STRIDE),
                        BufferArg::from_raw_parts(h_xm.clone(), n_chunks),
                        BufferArg::from_raw_parts(h_wm.clone(), n),
                        BufferArg::from_raw_parts(h_wc.clone(), n),
                        BufferArg::from_raw_parts(h_otb.clone(), n),
                        chunk,
                    );
                }
                5 => {
                    resolve_x::launch_unchecked(
                        &client,
                        cubes_of(n),
                        CubeDim::new_1d(256),
                        BufferArg::from_raw_parts(h_sm.clone(), n * SM_STRIDE),
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
                    );
                }
                6 => {
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
        "chunk_reduce",
        "spine_reduce",
        "spine_scan",
        "partial_scan",
        "k_apply",
        "resolve_x",
        "derive_stride",
        "paginate",
    ];
    let stage_meta = |s: usize| -> (&'static str, usize, u32) {
        let threads = match s {
            0 | 4 => n_chunks,
            1 | 3 => n_supers,
            2 | 6 => 1,
            _ => n,
        };
        let cubes = threads.div_ceil(256);
        (stage_names[s], cubes, 256u32)
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
    if stages < 5 {
        println!(
            "cubecl-chain-bench: {} ({} B, {} chunks, wrap {}) stages {} — pre-apply stages only, no verification",
            corpus_path.display(),
            n,
            n_chunks,
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
        "cubecl-chain-bench: {} ({} B, {} chunks, wrap {}, samples {}) timing={} missing_windows={} — \
         cpu decode+scan {:?} | chain (sum of per-dispatch minima) {:?} readbacks {:?} total {:?} ({:.1} MB/s)",
        corpus_path.display(),
        n,
        n_chunks,
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
