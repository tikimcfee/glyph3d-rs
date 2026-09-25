# gpu_pipeline.mojo — the WHOLE scan on device, and the composition claim.
#
# decode, chunkReduce, paginate and bounds are each proven in isolation against the
# CPU port. That is not the same as proving they COMPOSE: every one of them was
# handed CPU-computed inputs. Nothing yet showed that nine dispatches, each feeding
# the next on device with no host round-trip, produce the pipeline's answers.
#
# This runs the sequence pass and the full raking scan on the GPU —
#
#   (decode+)clusterProbe -> clusterChain -> chunkReduce -> spineReduce
#          -> spineScan -> partialScan -> apply -> resolveX -> paginate
#
# — with every intermediate staying in device memory, and compares the FINAL lanes
# against the CPU scan under the same tiered contract conformance_scan uses:
#   ROW / COL / ORD / ordToByte   exact (they are counts; nothing may round)
#   LINE_ADV                      eps    (foldless f64 prefix vs the scan's grouping)
#
# The statics arrive in one of three modes (check_fixture's `mode`): 0 decodes on
# CPU and uploads (the shipped form); 1 decodes on device with the probe unfused;
# 2 fuses decode+probe into k_decode_probe — the probe's head arrives in registers
# instead of being re-walked from bytes, and no statics cross the bus at all. The
# suite runs 0 and 2 over every fixture and synthetic case; modes 1/2 add a
# bit-exact gi/sm tier against the reference's RESOLVED statics, so the device
# decode is pinned end to end, not assumed from gpu_decode's isolated proof.
#
# The monoid lives in one place (`E` + `combine` below) and every scan dispatch calls
# it, so the seven kernels cannot drift from each other the way seven
# transcriptions would. The device kernels are cluster_device.mojo's, shared with
# gpu_cluster's standalone proof.
#
# Run: mojo run -I engine engine/gpu_pipeline.mojo engine/fixtures/*.pipe.bin

from std.sys import argv, has_accelerator
from std.time import perf_counter_ns
from max.gpu import global_idx
from std.atomic import Atomic
from std.memory import bitcast
from max.gpu.host import DeviceContext
from glyph_schema import (
    SM_STRIDE, SM_ADVANCE,
    LM_STRIDE, LM_X, LM_Y, LM_Z, LM_BASE_X,
    LC_STRIDE, LC_ROW, LC_COL,
    IM_STRIDE, IM_ORIGIN_X, IM_ORIGIN_Y, IM_ORIGIN_Z, IM_LINE_HEIGHT,
    IM_Z_STEP, IM_BAND_STRIDE_Y, IM_DEPTH_PER_BAND, IM_DEPTH_PER_COL,
    IM_PAGE_STRIDE_X,
    IE_STRIDE, IE_PAGE_ROWS, IE_PAGE_COLS, IE_SCROLL_ROWS, IE_PAGES_WIDE,
    IE_WRAP_WIDTH, IE_WRAP_MODE, IE_HAS_PAGE,
    PARTIAL_COUNT_STRIDE, PARTIAL_MEASURE_STRIDE,
    P_RESET, P_NL, P_GLYPHS, P_ROWS, P_HEAD_LEN, P_TAIL_LEN, P_WRAP, P_MODE, PM_TAIL_ADV,
)
from glyph_pipeline import (
    F_LEADER, F_NEWLINE, item_for_byte, derive_stride,
    WRAP_DOWN, WRAP_BACK, run_pipeline, CLUSTER_LEADER, CLUSTER_CLUSTER,
)


def key_to_float(k: UInt32) -> Float32:
    var b: UInt32
    if (k & 0x80000000) != 0:
        b = k & 0x7FFFFFFF
    else:
        b = ~k
    return bitcast[DType.float32](b)


def ordered_key(v: Float32) -> UInt32:
    var b = UInt32(v.to_bits())
    if (b & 0x80000000) != 0:
        return ~b
    return b | 0x80000000
from glyph_scan import run_scan_pipeline
from cluster_device import (
    k_cluster_probe, k_cluster_chain, k_decode_probe, build_head_bitmap, HEAD_BMP_WORDS,
)
from fixture_io import load_pipe_fixture, PipeFixture, load_trie_blob
from glyph_pipeline import Item, Trie

comptime MAX_PRINTED = 8
comptime CHUNK = 64
comptime GROUP = 256
comptime EPS = 1e-4


struct E(Copyable, Movable):
    """The monoid element, device-side. Mirrors ScanElem lane for lane."""

    var reset: Int
    var nl: Int
    var glyphs: Int
    var rows: Int
    var head_len: Int
    var tail_len: Int
    var wrap: Int
    var mode: Int
    var tail_adv: Float32

    def __init__(out self):
        self.reset = 0
        self.nl = 0
        self.glyphs = 0
        self.rows = 0
        self.head_len = 0
        self.tail_len = 0
        self.wrap = 0
        self.mode = WRAP_DOWN
        self.tail_adv = 0


def rows_for(length: Int, wrap: Int, mode: Int = WRAP_DOWN) -> Int:
    """Mirror of rows_for_line — TRANSCRIBE it, do not re-derive it.

    A ceiling with a floor of one under WRAP_DOWN, as of the 2026-09-04
    phantom-row correction; ONE row under WRAP_BACK, whatever the length.
    Note the history, because it reads like a contradiction: the FIRST attempt
    here wrote a bare ceiling `(length + wrap - 1) // wrap` and was off by one —
    against the then-current `length // wrap + 1`, and also at length 0, where a
    bare ceiling gives 0 rows for a line that occupies one. The rule moved; the
    lesson did not. Keep this a transcription of glyph_pipeline.rows_for_line."""
    if mode == WRAP_BACK:
        return 1
    if wrap <= 0 or length <= 0:
        return 1
    return (length - 1) // wrap + 1


def wrap_segment_of(col: Int, wrap: Int, terminator: Bool) -> Int:
    """Mirror of glyph_pipeline.wrap_segment_of — the DEPTH fan's segment index,
    mode-free, and what Z reads in both modes.

    A NEWLINE is a terminator riding at one-past-the-last cell, so at an exact
    wrap multiple it stays in the segment it closes rather than opening the next."""
    if wrap <= 0:
        return 0
    if terminator:
        if col <= 0:
            return 0
        return (col - 1) // wrap
    return col // wrap


def wrap_row_of(col: Int, wrap: Int, terminator: Bool, mode: Int = WRAP_DOWN) -> Int:
    """Mirror of glyph_pipeline.wrap_row_of — the ROW CONTRIBUTION of a cell.

    WRAP_DOWN is the segment index; WRAP_BACK is zero, because a wrap does not
    advance the row in that mode."""
    if mode == WRAP_BACK:
        return 0
    return wrap_segment_of(col, wrap, terminator)


def combine(mut a: E, b: E):
    """THE monoid — a mirror of scan_combine, line for line.

    Transcribe this from glyph_bake.mojo, do not re-derive it. The first attempt
    here was written to be "obviously right" for a LEAF b (one byte: rows == 0,
    head_len <= 1) and was wrong for a chunk-level b, which has a real head line
    and real rows. chunkReduce only ever combines leaves, so it passed in
    isolation; the spine combines chunks, so only composing them exposed it.
    """
    if b.reset != 0:
        a.reset = 1
        a.nl = b.nl
        a.glyphs = b.glyphs
        a.rows = b.rows
        a.head_len = b.head_len
        a.tail_len = b.tail_len
        a.tail_adv = b.tail_adv
        a.wrap = b.wrap
        a.mode = b.mode
        return
    a.wrap = b.wrap
    a.mode = b.mode
    if b.nl == 0:
        a.tail_len += b.tail_len
        a.tail_adv = a.tail_adv + b.tail_adv  # f32 per add — exact under regrouping
        if a.nl == 0:
            a.head_len = a.tail_len  # still one open line: head == tail
    else:
        if a.nl == 0:
            a.head_len += b.head_len  # a's open run extends b's head line
            a.rows = b.rows
        else:
            # The junction line: a's tail + b's head, closed by b's first newline.
            a.rows += rows_for(a.tail_len + b.head_len, b.wrap, b.mode) + b.rows
        a.tail_len = b.tail_len
        a.tail_adv = b.tail_adv
    a.nl += b.nl
    a.glyphs += b.glyphs


def p_load(pc: MutPointer[UInt32, MutAnyOrigin], pm: MutPointer[Float32, MutAnyOrigin], i: Int) -> E:
    var e = E()
    var o = i * PARTIAL_COUNT_STRIDE
    e.reset = Int(pc[unsafe_offset = o + P_RESET])
    e.nl = Int(pc[unsafe_offset = o + P_NL])
    e.glyphs = Int(pc[unsafe_offset = o + P_GLYPHS])
    e.rows = Int(pc[unsafe_offset = o + P_ROWS])
    e.head_len = Int(pc[unsafe_offset = o + P_HEAD_LEN])
    e.tail_len = Int(pc[unsafe_offset = o + P_TAIL_LEN])
    e.wrap = Int(pc[unsafe_offset = o + P_WRAP])
    e.mode = Int(pc[unsafe_offset = o + P_MODE])
    e.tail_adv = pm[unsafe_offset = i * PARTIAL_MEASURE_STRIDE + PM_TAIL_ADV]
    return e^


def p_store(pc: MutPointer[UInt32, MutAnyOrigin], pm: MutPointer[Float32, MutAnyOrigin], i: Int, e: E):
    var o = i * PARTIAL_COUNT_STRIDE
    pc[unsafe_offset = o + P_RESET] = UInt32(e.reset)
    pc[unsafe_offset = o + P_NL] = UInt32(e.nl)
    pc[unsafe_offset = o + P_GLYPHS] = UInt32(e.glyphs)
    pc[unsafe_offset = o + P_ROWS] = UInt32(e.rows)
    pc[unsafe_offset = o + P_HEAD_LEN] = UInt32(e.head_len)
    pc[unsafe_offset = o + P_TAIL_LEN] = UInt32(e.tail_len)
    pc[unsafe_offset = o + P_WRAP] = UInt32(e.wrap)
    pc[unsafe_offset = o + P_MODE] = UInt32(e.mode)
    pm[unsafe_offset = i * PARTIAL_MEASURE_STRIDE + PM_TAIL_ADV] = e.tail_adv


def leaf_of(
    fl: MutPointer[UInt32, MutAnyOrigin], sm: MutPointer[Float32, MutAnyOrigin],
    wrap_of: MutPointer[UInt32, MutAnyOrigin], mode_of: MutPointer[UInt32, MutAnyOrigin],
    is_start: MutPointer[UInt32, MutAnyOrigin],
    id: Int,
) -> E:
    var e = E()
    e.reset = Int(is_start[unsafe_offset=id])
    e.wrap = Int(wrap_of[unsafe_offset=id])
    e.mode = Int(mode_of[unsafe_offset=id])
    var f = Int(fl[unsafe_offset=id])
    if (f & F_LEADER) == 0:
        return e^
    e.glyphs = 1
    if (f & F_NEWLINE) != 0:
        e.nl = 1
    else:
        e.head_len = 1
        e.tail_len = 1
        e.tail_adv = sm[unsafe_offset = id * SM_STRIDE + SM_ADVANCE]
    return e^


# ── dispatch 2: chunkReduce — thread per chunk ──────────────────────────────
def k_chunk_reduce(
    fl: MutPointer[UInt32, MutAnyOrigin], sm: MutPointer[Float32, MutAnyOrigin],
    wrap_of: MutPointer[UInt32, MutAnyOrigin], mode_of: MutPointer[UInt32, MutAnyOrigin],
    is_start: MutPointer[UInt32, MutAnyOrigin],
    pc: MutPointer[UInt32, MutAnyOrigin], pm: MutPointer[Float32, MutAnyOrigin],
    n_bytes: Int32, k: Int32, n_chunks: Int32,
):
    var c = global_idx.x
    if c >= Int(n_chunks):
        return
    var n = Int(n_bytes)
    # E() IS the identity: combine(identity, b) == b for reset and non-reset b
    # alike, which is why no first-element special case is needed here or below.
    var acc = E()
    var id = c * Int(k)
    var to = (c + 1) * Int(k)
    if to > n:
        to = n
    while id < to:
        combine(acc, leaf_of(fl, sm, wrap_of, mode_of, is_start, id))
        id += 1
    p_store(pc, pm, c, acc)


# ── dispatch 3: spineReduce — thread per group ──────────────────────────────
def k_spine_reduce(
    pc: MutPointer[UInt32, MutAnyOrigin], pm: MutPointer[Float32, MutAnyOrigin],
    uc: MutPointer[UInt32, MutAnyOrigin], um: MutPointer[Float32, MutAnyOrigin],
    n_chunks: Int32, g: Int32, n_supers: Int32,
):
    var sg = global_idx.x
    if sg >= Int(n_supers):
        return
    var acc = E()
    var c = sg * Int(g)
    var last = (sg + 1) * Int(g)
    if last > Int(n_chunks):
        last = Int(n_chunks)
    while c < last:
        combine(acc, p_load(pc, pm, c))
        c += 1
    p_store(uc, um, sg, acc)


# ── dispatch 4: spineScan — ONE thread, exclusive scan of the supers ────────
def k_spine_scan(
    uc: MutPointer[UInt32, MutAnyOrigin], um: MutPointer[Float32, MutAnyOrigin],
    fc: MutPointer[UInt32, MutAnyOrigin], fm: MutPointer[Float32, MutAnyOrigin],
    n_supers: Int32,
):
    if global_idx.x != 0:
        return
    var acc = E()
    for sg in range(Int(n_supers)):
        p_store(fc, fm, sg, acc)          # exclusive: store BEFORE combining
        combine(acc, p_load(uc, um, sg))


# ── dispatch 5: partialScan — thread per group ──────────────────────────────
def k_partial_scan(
    pc: MutPointer[UInt32, MutAnyOrigin], pm: MutPointer[Float32, MutAnyOrigin],
    fc: MutPointer[UInt32, MutAnyOrigin], fm: MutPointer[Float32, MutAnyOrigin],
    xc: MutPointer[UInt32, MutAnyOrigin], xm: MutPointer[Float32, MutAnyOrigin],
    n_chunks: Int32, g: Int32, n_supers: Int32,
):
    var sg = global_idx.x
    if sg >= Int(n_supers):
        return
    var acc = p_load(fc, fm, sg)
    var c = sg * Int(g)
    var last = (sg + 1) * Int(g)
    if last > Int(n_chunks):
        last = Int(n_chunks)
    while c < last:
        p_store(xc, xm, c, acc)          # exclusive: store BEFORE combining
        combine(acc, p_load(pc, pm, c))
        c += 1


# ── dispatch 6: apply — thread per chunk, re-fold and write the lanes ───────
def k_apply(
    fl: MutPointer[UInt32, MutAnyOrigin], sm: MutPointer[Float32, MutAnyOrigin],
    lm: MutPointer[Float32, MutAnyOrigin], lc: MutPointer[UInt32, MutAnyOrigin],
    wrap_of: MutPointer[UInt32, MutAnyOrigin], mode_of: MutPointer[UInt32, MutAnyOrigin],
    is_start: MutPointer[UInt32, MutAnyOrigin],
    item_start: MutPointer[UInt32, MutAnyOrigin],
    xc: MutPointer[UInt32, MutAnyOrigin], xm: MutPointer[Float32, MutAnyOrigin],
    wm: MutPointer[Float32, MutAnyOrigin], wc: MutPointer[UInt32, MutAnyOrigin],
    otb: MutPointer[UInt32, MutAnyOrigin],
    n_bytes: Int32, k: Int32, n_chunks: Int32,
):
    var c = global_idx.x
    if c >= Int(n_chunks):
        return
    var n = Int(n_bytes)
    var id = c * Int(k)
    var to = (c + 1) * Int(k)
    if to > n:
        to = n
    if id >= to:
        return
    var run = p_load(xc, xm, c)
    while id < to:
        if Int(is_start[unsafe_offset=id]) != 0:
            run = E()
            run.wrap = Int(wrap_of[unsafe_offset=id])
            run.mode = Int(mode_of[unsafe_offset=id])
        var f = Int(fl[unsafe_offset=id])
        if (f & F_LEADER) != 0:
            # lanes_from_prefix, inline
            var wrap = Int(wrap_of[unsafe_offset=id])
            var mode = Int(mode_of[unsafe_offset=id])
            var col = run.tail_len
            var closed = 0
            if run.nl > 0:
                closed = rows_for(run.head_len, wrap, mode) + run.rows
            var wrap_row = wrap_row_of(col, wrap, (f & F_NEWLINE) != 0, mode)
            var co = id * LC_STRIDE
            lc[unsafe_offset = co + LC_ROW] = UInt32(closed + wrap_row)
            lc[unsafe_offset = co + LC_COL] = UInt32(col)
            # The witness tier (read-axis split): LINE_ADV/ORD live in their
            # own device buffers, mirroring the CPU container exactly.
            wc[unsafe_offset=id] = UInt32(run.glyphs)
            wm[unsafe_offset=id] = run.tail_adv
            otb[unsafe_offset = Int(item_start[unsafe_offset=id]) + run.glyphs] = UInt32(id)
        combine(run, leaf_of(fl, sm, wrap_of, mode_of, is_start, id))
        id += 1


# ── dispatch 7: resolveX — thread per byte ─────────────────────────────────
def k_resolve_x(
    sm: MutPointer[Float32, MutAnyOrigin], fl: MutPointer[UInt32, MutAnyOrigin],
    lm: MutPointer[Float32, MutAnyOrigin], lc: MutPointer[UInt32, MutAnyOrigin],
    items: MutPointer[Float32, MutAnyOrigin],
    items_e: MutPointer[UInt32, MutAnyOrigin], item_of: MutPointer[UInt32, MutAnyOrigin],
    item_start: MutPointer[UInt32, MutAnyOrigin],
    wm: MutPointer[Float32, MutAnyOrigin], wc: MutPointer[UInt32, MutAnyOrigin],
    otb: MutPointer[UInt32, MutAnyOrigin],
    row_max: MutPointer[UInt32, MutAnyOrigin], x_max: MutPointer[UInt32, MutAnyOrigin],
    n_bytes: Int32,
):
    """Resolve x from the EXACT lanes and place the unpaginated position.

    With a fold unit, x re-sums the glyph's `col % fold` same-row predecessors
    FORWARD from the segment start — the same f32 order the serial segAdv
    accumulates, which is why fold>0 x is bit-identical across oracle, scan and
    hardware. Foldless, x IS the f32 LINE_ADV lane.

    The two fold scalars go to atomics BY KIND, as in gpu_bounds: the row count to
    a native u32 atomicMax, the widest extent through the ordered key."""
    var id = global_idx.x
    if id >= Int(n_bytes):
        return
    if (Int(fl[unsafe_offset=id]) & F_LEADER) == 0:
        return
    var mo = id * LM_STRIDE
    var it = Int(item_of[unsafe_offset=id])
    var io = it * IM_STRIDE
    var ie = it * IE_STRIDE

    # Exact page geometry: NATIVE u32 reads since the kind correction.
    var wrap = Int(items_e[unsafe_offset = ie + IE_WRAP_WIDTH])
    var fold = wrap
    if fold == 0 and items_e[unsafe_offset = ie + IE_HAS_PAGE] != 0:
        fold = Int(items_e[unsafe_offset = ie + IE_PAGE_COLS])
    var col = Int(lc[unsafe_offset = id * LC_STRIDE + LC_COL])
    var ord = Int(wc[unsafe_offset=id])

    var x = Float32(0)
    if fold > 0:
        var k = col % fold
        while k >= 1:
            var q = Int(otb[unsafe_offset = Int(item_start[unsafe_offset=id]) + ord - k])
            x = x + sm[unsafe_offset = q * SM_STRIDE + SM_ADVANCE]
            k -= 1
    else:
        x = wm[unsafe_offset=id]

    var row = Int(lc[unsafe_offset = id * LC_STRIDE + LC_ROW])
    # The DEPTH fan's SEGMENT index, never the row contribution: under WRAP_BACK
    # they differ and the segments are all z has left. ROW already carries the mode.
    var seg = wrap_segment_of(col, wrap, (Int(fl[unsafe_offset=id]) & F_NEWLINE) != 0)
    # lineHeight is the ITEM's, never the glyph's — the SIXTH copy of the
    # deleted fallback died here. It survived five sweeps because it is spelled
    # `lh != lh` with I_LINE_HEIGHT, matching none of the greps that found the
    # others (lh_unset, is_nan(item.line_height), I_PAGE_LINE_HEIGHT). Found by
    # the split refactor forcing every lane site to be read, not by search.
    var lh = items[unsafe_offset = io + IM_LINE_HEIGHT]

    var ox = items[unsafe_offset = io + IM_ORIGIN_X]
    lm[unsafe_offset = mo + LM_BASE_X] = x + ox
    lm[unsafe_offset = mo + LM_X] = x + ox
    lm[unsafe_offset = mo + LM_Y] = (
        Float32(-row) * lh + items[unsafe_offset = io + IM_ORIGIN_Y]
    )
    lm[unsafe_offset = mo + LM_Z] = (
        Float32(-seg) * items[unsafe_offset = io + IM_Z_STEP]
        + items[unsafe_offset = io + IM_ORIGIN_Z]
    )

    _ = Atomic.max(row_max.unsafe_offset(it), UInt32(row + 1))  # a COUNT: native u32
    _ = Atomic.max(x_max.unsafe_offset(it), ordered_key(x))     # a MEASURE: ordered key


# ── dispatch 8: paginate — thread per byte, pure per-slot remap ────────────
def k_paginate(
    lm: MutPointer[Float32, MutAnyOrigin], fl: MutPointer[UInt32, MutAnyOrigin],
    lc: MutPointer[UInt32, MutAnyOrigin],
    items: MutPointer[Float32, MutAnyOrigin],
    items_e: MutPointer[UInt32, MutAnyOrigin], item_of: MutPointer[UInt32, MutAnyOrigin],
    strides: MutPointer[Float32, MutAnyOrigin], n_bytes: Int32,
):
    var id = global_idx.x
    if id >= Int(n_bytes):
        return
    if (Int(fl[unsafe_offset=id]) & F_LEADER) == 0:
        return
    var it = Int(item_of[unsafe_offset=id])
    var io = it * IM_STRIDE
    var ie = it * IE_STRIDE
    var has_page = items_e[unsafe_offset = ie + IE_HAS_PAGE] != 0
    var rows = Int(items_e[unsafe_offset = ie + IE_PAGE_ROWS]) if has_page else 0
    var cols = Int(items_e[unsafe_offset = ie + IE_PAGE_COLS]) if has_page else 0
    var scroll = Int(items_e[unsafe_offset = ie + IE_SCROLL_ROWS]) if has_page else 0
    if rows == 0 and cols == 0 and scroll == 0:
        return
    var row = Int(lc[unsafe_offset = id * LC_STRIDE + LC_ROW])
    var col = Int(lc[unsafe_offset = id * LC_STRIDE + LC_COL])
    var screen_row = row - scroll
    var y_page = 0
    if rows > 0 and screen_row >= rows:
        y_page = screen_row // rows
    var x_page = 0
    if cols > 0:
        x_page = col // cols
    var wide_raw = Int(items_e[unsafe_offset = ie + IE_PAGES_WIDE])
    var wide = wide_raw if wide_raw > 1 else 1
    var band = y_page // wide
    var wrap = Int(items_e[unsafe_offset = ie + IE_WRAP_WIDTH])
    # The DEPTH fan's SEGMENT index, terminator case included — paginate
    # recomputes Z from the COL lane, and Z is mode-free.
    var seg = wrap_segment_of(col, wrap, (Int(fl[unsafe_offset=id]) & F_NEWLINE) != 0)
    # The page's own lineHeight is NOT consulted — mirrors 4697e3b. The fallback
    # could only fire on an item with a NaN lineHeight, which the oracle now
    # refuses, so it was reachable solely through malformed input. Proven, not
    # argued: poisoning this branch left all twelve suites green, while poisoning
    # the TAKEN read below failed both GPU suites — so they do exercise this line.
    var lh = items[unsafe_offset = io + IM_LINE_HEIGHT]
    var mo = id * LM_STRIDE
    lm[unsafe_offset = mo + LM_X] = (
        lm[unsafe_offset = mo + LM_BASE_X]
        + Float32(y_page % wide) * strides[unsafe_offset=it]
    )
    lm[unsafe_offset = mo + LM_Y] = (
        items[unsafe_offset = io + IM_ORIGIN_Y]
        - Float32(screen_row - y_page * rows) * lh
        - Float32(band) * items[unsafe_offset = io + IM_BAND_STRIDE_Y]
    )
    lm[unsafe_offset = mo + LM_Z] = (
        items[unsafe_offset = io + IM_ORIGIN_Z]
        - Float32(seg) * items[unsafe_offset = io + IM_Z_STEP]
        + Float32(band) * items[unsafe_offset = io + IM_DEPTH_PER_BAND]
        + Float32(x_page) * items[unsafe_offset = io + IM_DEPTH_PER_COL]
    )


def rel_close(a: Float64, b: Float64) -> Bool:
    var d = a - b
    if d < 0:
        d = -d
    var m = a if a > 0 else -a
    var mb = b if b > 0 else -b
    if mb > m:
        m = mb
    if m < 1.0:
        m = 1.0
    return d / m <= EPS


def check_case(path: String, ctx: DeviceContext) raises -> Int:
    # Every fixture runs mode 0 (CPU-decoded statics, unfused probe) AND mode 2
    # (fused decode+probe) against the same reference — the suite proves the
    # fused form on every run, not just when someone benches it.
    var bad = check_fixture(load_pipe_fixture(path), ctx)
    bad += check_fixture(load_pipe_fixture(path), ctx, mode=2)
    return bad


def check_fixture(var fx: PipeFixture, ctx: DeviceContext, bench: Bool = False, mode: Int = 0) raises -> Int:
    """The device chain over one fixture, in one of three statics modes:
    0 — statics decoded on CPU (leader-forced) and uploaded; probe+chain on
        device. The shipped form.
    1 — decode on device (k_decode_probe[False]), probe unfused. Bench-only:
        isolates the upload elimination from the dispatch fusion.
    2 — decode and probe fused (k_decode_probe[True]); chain unchanged.
    Modes 1/2 additionally compare the device-produced statics (gi, sm)
    bit-exact against the reference's RESOLVED statics — the fused decode is
    pinned end to end, not assumed from gpu_decode's separate proof."""
    var n = fx.byte_len
    if n == 0:
        return 0
    var n_chunks = (n + CHUNK - 1) // CHUNK
    var n_supers = (n_chunks + GROUP - 1) // GROUP

    # The CPU scan is the reference — itself already proven against the oracle.
    var t0 = perf_counter_ns()
    var cpu = run_scan_pipeline(fx.bytes, fx.trie, fx.items, CHUNK, GROUP)
    var cpu_ns = perf_counter_ns() - t0
    # Everything from here to the first dispatch is the host prep the mode pays
    # for: mode 0's price includes a whole leader-forced CPU pipeline run.
    var t_prep = perf_counter_ns()

    # Per-byte item facts, as the GPU pipeline gets them from itemStarts.
    var wrap_of = List[UInt32](unsafe_uninit_length=n)
    var mode_of = List[UInt32](unsafe_uninit_length=n)
    var is_start = List[UInt32](unsafe_uninit_length=n)
    var item_start = List[UInt32](unsafe_uninit_length=n)
    var item_of = List[UInt32](unsafe_uninit_length=n)
    for id in range(n):
        var i = item_for_byte(fx.items, id)
        item_of[id] = UInt32(i) if i >= 0 else UInt32(0)
        wrap_of[id] = UInt32(fx.items[i].wrap_width) if i >= 0 else 0
        mode_of[id] = UInt32(fx.items[i].wrap_mode) if i >= 0 else 0
        is_start[id] = UInt32(1) if (i >= 0 and fx.items[i].byte_start == id) else UInt32(0)
        item_start[id] = UInt32(fx.items[i].byte_start) if i >= 0 else UInt32(0)

    # The sequence pass runs ON DEVICE here (it used to arrive pre-computed in
    # the uploaded lanes). Mode 0 seeds the decoded-but-unresolved statics from
    # a leader-forced CPU run (the fills below); modes 1/2 decode on device via
    # k_decode_probe (decode's kernel is proven in gpu_decode; the fused form is
    # pinned by this suite's gi/sm tier). The device probe+chain dispatches
    # rewrite the statics before the scan consumes any advance. The scan
    # reference (cpu, resolving) stays the comparison either way, so the pass is
    # covered end to end.

    var item_end_of = List[UInt32](unsafe_uninit_length=n)
    var cluster_of = List[UInt32](unsafe_uninit_length=n)
    for id in range(n):
        var i = item_for_byte(fx.items, id)
        item_end_of[id] = UInt32(fx.items[i].byte_start + fx.items[i].byte_count) if i >= 0 else 0
        cluster_of[id] = UInt32(1) if (i >= 0 and fx.items[i].cluster_mode == CLUSTER_CLUSTER) else UInt32(0)
    var item_count = len(fx.items)
    var item_ranges = List[UInt32](unsafe_uninit_length=item_count * 2)
    var item_cluster = List[UInt32](unsafe_uninit_length=item_count)
    for i in range(item_count):
        item_ranges[i * 2] = UInt32(fx.items[i].byte_start)
        item_ranges[i * 2 + 1] = UInt32(fx.items[i].byte_start + fx.items[i].byte_count)
        item_cluster[i] = UInt32(1) if fx.items[i].cluster_mode == CLUSTER_CLUSTER else UInt32(0)
    var seq_count = 0
    if fx.trie.seq_max > 0:
        seq_count = len(fx.trie.seq) // (2 + fx.trie.seq_max)

    # ── upload the DECODED lanes (decode itself is proven in gpu_decode) ─────
    var h_fl = ctx.enqueue_create_host_buffer[DType.uint32](n)
    var h_sm = ctx.enqueue_create_host_buffer[DType.float32](n * SM_STRIDE)
    var h_lm = ctx.enqueue_create_host_buffer[DType.float32](n * LM_STRIDE)
    var h_lc = ctx.enqueue_create_host_buffer[DType.uint32](n * LC_STRIDE)
    var h_w = ctx.enqueue_create_host_buffer[DType.uint32](n)
    var h_md = ctx.enqueue_create_host_buffer[DType.uint32](n)
    var h_s = ctx.enqueue_create_host_buffer[DType.uint32](n)
    var h_is = ctx.enqueue_create_host_buffer[DType.uint32](n)
    var h_wm = ctx.enqueue_create_host_buffer[DType.float32](n)
    var h_wc = ctx.enqueue_create_host_buffer[DType.uint32](n)
    var h_otb = ctx.enqueue_create_host_buffer[DType.uint32](n)
    ctx.synchronize()
    # The seeded statics are the LEADER-FORCED decode (unresolved): the pass
    # itself is what the device must produce, so the resolved form cannot be
    # handed to it. ROW/COL/ORD/LINE_ADV stay zeroed as before. Modes 1/2 skip
    # this entirely — k_decode_probe decodes on device, so the statics leave
    # the host as device-side zero-fills instead of an upload.
    var h_bytes = ctx.enqueue_create_host_buffer[DType.uint8](n)
    var h_gi = ctx.enqueue_create_host_buffer[DType.uint32](n)
    var n_seq = len(fx.trie.seq) if len(fx.trie.seq) > 0 else 1
    var h_seq = ctx.enqueue_create_host_buffer[DType.uint32](n_seq)
    var h_bmp = ctx.enqueue_create_host_buffer[DType.uint32](HEAD_BMP_WORDS)
    var h_ceof = ctx.enqueue_create_host_buffer[DType.uint32](n)
    var h_cof = ctx.enqueue_create_host_buffer[DType.uint32](n)
    var h_ir = ctx.enqueue_create_host_buffer[DType.uint32](item_count * 2)
    var h_ic = ctx.enqueue_create_host_buffer[DType.uint32](item_count)
    # The decode trie tables, needed only when decode runs on device. Filled
    # and uploaded solely in modes 1/2; mode 0 never reads them.
    var n_idx = len(fx.trie.block_index)
    var n_bm = len(fx.trie.blocks_m)
    var n_bc = len(fx.trie.blocks_c)
    var h_index = ctx.enqueue_create_host_buffer[DType.uint32](n_idx)
    var h_bm = ctx.enqueue_create_host_buffer[DType.float32](n_bm)
    var h_bc = ctx.enqueue_create_host_buffer[DType.uint32](n_bc)
    ctx.synchronize()
    if mode == 0:
        var items_leader = List[Item]()
        for i in range(len(fx.items)):
            var t = fx.items[i].copy()
            t.cluster_mode = CLUSTER_LEADER
            items_leader.append(t^)
        var dec = run_pipeline[witness=False](fx.bytes, fx.trie, items_leader^)
        for id in range(n):
            h_fl[id] = dec.fl[id]
            h_gi[id] = dec.gi[id]
        for i in range(n * SM_STRIDE):
            h_sm[i] = dec.sm[i]
    if mode != 0:
        for i in range(n_idx):
            h_index[i] = fx.trie.block_index[i]
        for i in range(n_bm):
            h_bm[i] = fx.trie.blocks_m[i]
        for i in range(n_bc):
            h_bc[i] = fx.trie.blocks_c[i]
    for id in range(n):
        h_bytes[id] = fx.bytes[id]
        h_ceof[id] = item_end_of[id]
        h_cof[id] = cluster_of[id]
    for i in range(len(fx.trie.seq)):
        h_seq[i] = fx.trie.seq[i]
    if len(fx.trie.seq) == 0:
        h_seq[0] = 0
    var head_bmp = build_head_bitmap(fx.trie)
    for i in range(HEAD_BMP_WORDS):
        h_bmp[i] = head_bmp[i]
    for i in range(item_count * 2):
        h_ir[i] = item_ranges[i]
    for i in range(item_count):
        h_ic[i] = item_cluster[i]
    for i in range(n * LM_STRIDE):
        h_lm[i] = 0
    for i in range(n * LC_STRIDE):
        h_lc[i] = 0
    for i in range(n):
        h_w[i] = wrap_of[i]
        h_md[i] = mode_of[i]
        h_s[i] = is_start[i]
        h_is[i] = item_start[i]
        h_wm[i] = 0
        h_wc[i] = 0
        h_otb[i] = 0

    var ni0 = fx.item_count if fx.item_count > 0 else 1
    var h_it = ctx.enqueue_create_host_buffer[DType.float32](ni0 * IM_STRIDE)
    var h_ie = ctx.enqueue_create_host_buffer[DType.uint32](ni0 * IE_STRIDE)
    var h_io = ctx.enqueue_create_host_buffer[DType.uint32](n)
    var h_rmax = ctx.enqueue_create_host_buffer[DType.uint32](ni0)
    var h_xmax = ctx.enqueue_create_host_buffer[DType.uint32](ni0)
    ctx.synchronize()
    for i in range(ni0 * IM_STRIDE):
        h_it[i] = 0
    for i in range(ni0 * IE_STRIDE):
        h_ie[i] = 0
    for i in range(fx.item_count):
        var o = i * IM_STRIDE
        var oe = i * IE_STRIDE
        var t = fx.items[i].copy()
        h_it[o + IM_ORIGIN_X] = Float32(t.origin_x)
        h_it[o + IM_ORIGIN_Y] = Float32(t.origin_y)
        h_it[o + IM_ORIGIN_Z] = Float32(t.origin_z)
        h_it[o + IM_LINE_HEIGHT] = Float32(t.line_height)
        h_it[o + IM_Z_STEP] = Float32(t.z_step)
        h_it[o + IM_BAND_STRIDE_Y] = Float32(t.band_stride_y)
        h_it[o + IM_DEPTH_PER_BAND] = Float32(t.depth_per_band)
        h_it[o + IM_DEPTH_PER_COL] = Float32(t.depth_per_col)
        h_ie[oe + IE_WRAP_WIDTH] = UInt32(t.wrap_width)
        h_ie[oe + IE_WRAP_MODE] = UInt32(t.wrap_mode)
        h_ie[oe + IE_PAGE_ROWS] = UInt32(t.page_rows)
        h_ie[oe + IE_PAGE_COLS] = UInt32(t.page_cols)
        h_ie[oe + IE_SCROLL_ROWS] = UInt32(t.scroll_rows)
        h_ie[oe + IE_PAGES_WIDE] = UInt32(t.pages_wide)
        h_ie[oe + IE_HAS_PAGE] = UInt32(1) if t.has_page else UInt32(0)
    for i in range(n):
        h_io[i] = item_of[i]
    for i in range(ni0):
        h_rmax[i] = 0
        h_xmax[i] = 0

    var d_fl = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_sm = ctx.enqueue_create_buffer[DType.float32](n * SM_STRIDE)
    var d_lm = ctx.enqueue_create_buffer[DType.float32](n * LM_STRIDE)
    var d_lc = ctx.enqueue_create_buffer[DType.uint32](n * LC_STRIDE)
    var d_w = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_md = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_s = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_is = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_wm = ctx.enqueue_create_buffer[DType.float32](n)
    var d_wc = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_otb = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_pc = ctx.enqueue_create_buffer[DType.uint32](n_chunks * PARTIAL_COUNT_STRIDE)
    var d_pm = ctx.enqueue_create_buffer[DType.float32](n_chunks * PARTIAL_MEASURE_STRIDE)
    var d_uc = ctx.enqueue_create_buffer[DType.uint32](n_supers * PARTIAL_COUNT_STRIDE)
    var d_um = ctx.enqueue_create_buffer[DType.float32](n_supers * PARTIAL_MEASURE_STRIDE)
    var d_fc = ctx.enqueue_create_buffer[DType.uint32](n_supers * PARTIAL_COUNT_STRIDE)
    var d_fm = ctx.enqueue_create_buffer[DType.float32](n_supers * PARTIAL_MEASURE_STRIDE)
    var d_xc = ctx.enqueue_create_buffer[DType.uint32](n_chunks * PARTIAL_COUNT_STRIDE)
    var d_xm = ctx.enqueue_create_buffer[DType.float32](n_chunks * PARTIAL_MEASURE_STRIDE)
    var ni = fx.item_count if fx.item_count > 0 else 1
    var d_it = ctx.enqueue_create_buffer[DType.float32](ni * IM_STRIDE)
    var d_ie = ctx.enqueue_create_buffer[DType.uint32](ni * IE_STRIDE)
    var d_io = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_rmax = ctx.enqueue_create_buffer[DType.uint32](ni)
    var d_xmax = ctx.enqueue_create_buffer[DType.uint32](ni)
    var d_bytes = ctx.enqueue_create_buffer[DType.uint8](n)
    var d_gi = ctx.enqueue_create_buffer[DType.uint32](n)
    # The decode trie tables: uploaded and read only in modes 1/2 (device
    # decode). In mode 0 they sit unused — allocated, never filled.
    var d_index = ctx.enqueue_create_buffer[DType.uint32](n_idx)
    var d_bm = ctx.enqueue_create_buffer[DType.float32](n_bm)
    var d_bc = ctx.enqueue_create_buffer[DType.uint32](n_bc)
    var d_seq = ctx.enqueue_create_buffer[DType.uint32](n_seq)
    var d_bmp = ctx.enqueue_create_buffer[DType.uint32](HEAD_BMP_WORDS)
    var d_ceof = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_cof = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_ir = ctx.enqueue_create_buffer[DType.uint32](item_count * 2)
    var d_ic = ctx.enqueue_create_buffer[DType.uint32](item_count)
    var d_cslot = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_cend = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_cblk = ctx.enqueue_create_buffer[DType.uint32]((n + 127) >> 7)
    if mode == 0:
        ctx.enqueue_copy(dst_buf=d_fl, src_buf=h_fl)
        ctx.enqueue_copy(dst_buf=d_sm, src_buf=h_sm)
    else:
        # Device decode writes every leader lane; continuations stay zero.
        d_fl.enqueue_fill(0)
        d_sm.enqueue_fill(0.0)
        d_gi.enqueue_fill(0)
        ctx.enqueue_copy(dst_buf=d_index, src_buf=h_index)
        ctx.enqueue_copy(dst_buf=d_bm, src_buf=h_bm)
        ctx.enqueue_copy(dst_buf=d_bc, src_buf=h_bc)
    ctx.enqueue_copy(dst_buf=d_lm, src_buf=h_lm)
    ctx.enqueue_copy(dst_buf=d_lc, src_buf=h_lc)
    ctx.enqueue_copy(dst_buf=d_w, src_buf=h_w)
    ctx.enqueue_copy(dst_buf=d_md, src_buf=h_md)
    ctx.enqueue_copy(dst_buf=d_s, src_buf=h_s)
    ctx.enqueue_copy(dst_buf=d_is, src_buf=h_is)
    ctx.enqueue_copy(dst_buf=d_wm, src_buf=h_wm)
    ctx.enqueue_copy(dst_buf=d_wc, src_buf=h_wc)
    ctx.enqueue_copy(dst_buf=d_otb, src_buf=h_otb)
    ctx.enqueue_copy(dst_buf=d_it, src_buf=h_it)
    ctx.enqueue_copy(dst_buf=d_ie, src_buf=h_ie)
    ctx.enqueue_copy(dst_buf=d_io, src_buf=h_io)
    ctx.enqueue_copy(dst_buf=d_rmax, src_buf=h_rmax)
    ctx.enqueue_copy(dst_buf=d_xmax, src_buf=h_xmax)
    ctx.enqueue_copy(dst_buf=d_bytes, src_buf=h_bytes)
    if mode == 0:
        ctx.enqueue_copy(dst_buf=d_gi, src_buf=h_gi)
    ctx.enqueue_copy(dst_buf=d_seq, src_buf=h_seq)
    ctx.enqueue_copy(dst_buf=d_bmp, src_buf=h_bmp)
    ctx.enqueue_copy(dst_buf=d_ceof, src_buf=h_ceof)
    ctx.enqueue_copy(dst_buf=d_cof, src_buf=h_cof)
    ctx.enqueue_copy(dst_buf=d_ir, src_buf=h_ir)
    ctx.enqueue_copy(dst_buf=d_ic, src_buf=h_ic)
    d_cslot.enqueue_fill(0)
    d_cend.enqueue_fill(0)
    d_cblk.enqueue_fill(0)
    d_pc.enqueue_fill(0)
    d_uc.enqueue_fill(0)
    d_fc.enqueue_fill(0)
    d_xc.enqueue_fill(0)
    d_pm.enqueue_fill(0.0)
    d_um.enqueue_fill(0.0)
    d_fm.enqueue_fill(0.0)
    d_xm.enqueue_fill(0.0)

    # ── the chain. Every intermediate stays on device. ──────────────────────
    ctx.synchronize()
    var g0 = perf_counter_ns()
    var prep_ns = g0 - t_prep
    comptime B = 128
    # THE SEQUENCE PASS, on device. Mode 0: the probe (thread per byte) writes
    # candidates over uploaded statics, the chain (thread per item) commits
    # them. Mode 1 decodes on device first (upload eliminated, probe unfused).
    # Mode 2 fuses decode+probe into k_decode_probe — one dispatch, and the
    # probe's head arrives in registers instead of being re-walked from bytes.
    # The scan's kernels below read the rewritten statics — same order as the
    # CPU's resolve-before-fold.
    if mode == 2:
        ctx.enqueue_function[k_decode_probe[True]](
            d_bytes.unsafe_ptr(), d_index.unsafe_ptr(), d_bm.unsafe_ptr(), d_bc.unsafe_ptr(),
            d_sm.unsafe_ptr(), d_gi.unsafe_ptr(), d_fl.unsafe_ptr(),
            d_cof.unsafe_ptr(), d_ceof.unsafe_ptr(),
            d_seq.unsafe_ptr(), d_bmp.unsafe_ptr(),
            d_cslot.unsafe_ptr(), d_cend.unsafe_ptr(), d_cblk.unsafe_ptr(),
            Int32(n), Int32(seq_count), Int32(fx.trie.seq_max),
            grid_dim=(n + B - 1) // B, block_dim=B,
        )
    else:
        if mode == 1:
            ctx.enqueue_function[k_decode_probe[False]](
                d_bytes.unsafe_ptr(), d_index.unsafe_ptr(), d_bm.unsafe_ptr(), d_bc.unsafe_ptr(),
                d_sm.unsafe_ptr(), d_gi.unsafe_ptr(), d_fl.unsafe_ptr(),
                d_cof.unsafe_ptr(), d_ceof.unsafe_ptr(),
                d_seq.unsafe_ptr(), d_bmp.unsafe_ptr(),
                d_cslot.unsafe_ptr(), d_cend.unsafe_ptr(), d_cblk.unsafe_ptr(),
                Int32(n), Int32(seq_count), Int32(fx.trie.seq_max),
                grid_dim=(n + B - 1) // B, block_dim=B,
            )
        ctx.enqueue_function[k_cluster_probe](
            d_bytes.unsafe_ptr(), d_cof.unsafe_ptr(), d_ceof.unsafe_ptr(),
            d_seq.unsafe_ptr(), d_bmp.unsafe_ptr(), d_gi.unsafe_ptr(), d_sm.unsafe_ptr(), d_fl.unsafe_ptr(),
            d_cslot.unsafe_ptr(), d_cend.unsafe_ptr(), d_cblk.unsafe_ptr(),
            Int32(n), Int32(seq_count), Int32(fx.trie.seq_max),
            grid_dim=(n + B - 1) // B, block_dim=B,
        )
    ctx.enqueue_function[k_cluster_chain](
        d_bytes.unsafe_ptr(), d_ir.unsafe_ptr(), d_ic.unsafe_ptr(),
        d_cslot.unsafe_ptr(), d_cend.unsafe_ptr(), d_cblk.unsafe_ptr(),
        d_gi.unsafe_ptr(), d_sm.unsafe_ptr(), d_fl.unsafe_ptr(),
        fx.trie.bitmap_advance, Int32(item_count),
        grid_dim=(item_count + 63) // 64, block_dim=64,
    )
    ctx.enqueue_function[k_chunk_reduce](
        d_fl.unsafe_ptr(), d_sm.unsafe_ptr(), d_w.unsafe_ptr(), d_md.unsafe_ptr(),
        d_s.unsafe_ptr(),
        d_pc.unsafe_ptr(), d_pm.unsafe_ptr(),
        Int32(n), Int32(CHUNK), Int32(n_chunks),
        grid_dim=(n_chunks + B - 1) // B, block_dim=B,
    )
    ctx.enqueue_function[k_spine_reduce](
        d_pc.unsafe_ptr(), d_pm.unsafe_ptr(), d_uc.unsafe_ptr(), d_um.unsafe_ptr(),
        Int32(n_chunks), Int32(GROUP), Int32(n_supers),
        grid_dim=(n_supers + B - 1) // B, block_dim=B,
    )
    ctx.enqueue_function[k_spine_scan](
        d_uc.unsafe_ptr(), d_um.unsafe_ptr(), d_fc.unsafe_ptr(), d_fm.unsafe_ptr(),
        Int32(n_supers), grid_dim=1, block_dim=1,
    )
    ctx.enqueue_function[k_partial_scan](
        d_pc.unsafe_ptr(), d_pm.unsafe_ptr(), d_fc.unsafe_ptr(), d_fm.unsafe_ptr(),
        d_xc.unsafe_ptr(), d_xm.unsafe_ptr(),
        Int32(n_chunks), Int32(GROUP), Int32(n_supers),
        grid_dim=(n_supers + B - 1) // B, block_dim=B,
    )
    ctx.enqueue_function[k_apply](
        d_fl.unsafe_ptr(), d_sm.unsafe_ptr(), d_lm.unsafe_ptr(), d_lc.unsafe_ptr(),
        d_w.unsafe_ptr(), d_md.unsafe_ptr(), d_s.unsafe_ptr(),
        d_is.unsafe_ptr(), d_xc.unsafe_ptr(), d_xm.unsafe_ptr(),
        d_wm.unsafe_ptr(), d_wc.unsafe_ptr(), d_otb.unsafe_ptr(),
        Int32(n), Int32(CHUNK), Int32(n_chunks),
        grid_dim=(n_chunks + B - 1) // B, block_dim=B,
    )
    ctx.enqueue_function[k_resolve_x](
        d_sm.unsafe_ptr(), d_fl.unsafe_ptr(), d_lm.unsafe_ptr(), d_lc.unsafe_ptr(),
        d_it.unsafe_ptr(), d_ie.unsafe_ptr(), d_io.unsafe_ptr(),
        d_is.unsafe_ptr(),
        d_wm.unsafe_ptr(), d_wc.unsafe_ptr(), d_otb.unsafe_ptr(),
        d_rmax.unsafe_ptr(), d_xmax.unsafe_ptr(),
        Int32(n), grid_dim=(n + B - 1) // B, block_dim=B,
    )
    # The fan stride is DERIVED from each item's widest fold row — a fold scalar
    # resolveX just produced. The CPU driver computes it between dispatches too, so
    # this readback mirrors the reference rather than shortcutting it.
    var h_xr = ctx.enqueue_create_host_buffer[DType.uint32](ni0)
    var h_st = ctx.enqueue_create_host_buffer[DType.float32](ni0)
    ctx.enqueue_copy(dst_buf=h_xr, src_buf=d_xmax)
    ctx.synchronize()
    for i in range(fx.item_count):
        var widest = Float64(key_to_float(h_xr[i]))
        h_st[i] = Float32(derive_stride(widest, fx.items[i]))
    var d_st = ctx.enqueue_create_buffer[DType.float32](ni0)
    ctx.enqueue_copy(dst_buf=d_st, src_buf=h_st)
    ctx.enqueue_function[k_paginate](
        d_lm.unsafe_ptr(), d_fl.unsafe_ptr(), d_lc.unsafe_ptr(),
        d_it.unsafe_ptr(), d_ie.unsafe_ptr(), d_io.unsafe_ptr(),
        d_st.unsafe_ptr(), Int32(n),
        grid_dim=(n + B - 1) // B, block_dim=B,
    )
    if mode != 0 and not bench:
        # The fused path's statics are device-produced: read them back for the
        # gi/sm comparison tier below. (Bench returns before comparing; the
        # extra copies would tax the timed phase.)
        ctx.enqueue_copy(dst_buf=h_gi, src_buf=d_gi)
        ctx.enqueue_copy(dst_buf=h_sm, src_buf=d_sm)
    ctx.enqueue_copy(dst_buf=h_lc, src_buf=d_lc)
    ctx.enqueue_copy(dst_buf=h_lm, src_buf=d_lm)
    ctx.enqueue_copy(dst_buf=h_wm, src_buf=d_wm)
    ctx.enqueue_copy(dst_buf=h_wc, src_buf=d_wc)
    ctx.enqueue_copy(dst_buf=h_otb, src_buf=d_otb)
    ctx.enqueue_copy(dst_buf=h_rmax, src_buf=d_rmax)
    ctx.synchronize()
    var gpu_ns = perf_counter_ns() - g0
    if bench:
        var mb = Float64(n) / 1048576.0
        print(
            "  ", n, "B  mode", mode, "  prep", Float64(prep_ns) / 1e6,
            "ms   |   cpu(sharded)", Float64(cpu_ns) / 1e6, "ms =",
            mb / (Float64(cpu_ns) / 1e9), "MB/s   |   gpu", Float64(gpu_ns) / 1e6,
            "ms =", mb / (Float64(gpu_ns) / 1e9), "MB/s   |   x",
            Float64(cpu_ns) / Float64(gpu_ns),
        )
        return 0

    # ── the tiered comparison ───────────────────────────────────────────────
    var bad = 0
    var printed = 0
    if mode != 0:
        # Modes 1/2 produce the statics ON DEVICE: pin them bit-exact against
        # the reference's RESOLVED statics — device decode and the sequence
        # pass's rewrite are both covered here, per byte, no tolerance.
        for id in range(n):
            if h_gi[id] != cpu.gi[id]:
                bad += 1
                if printed < MAX_PRINTED:
                    print("  byte", id, "GLYPH_ID gpu", h_gi[id], "cpu", cpu.gi[id])
                    printed += 1
        for i in range(n * SM_STRIDE):
            if UInt32(h_sm[i].to_bits()) != UInt32(cpu.sm[i].to_bits()):
                bad += 1
                if printed < MAX_PRINTED:
                    print("  static lane", i, "— gpu", h_sm[i], "cpu", cpu.sm[i])
                    printed += 1
    for id in range(n):
        var co = id * LC_STRIDE
        if (Int(cpu.fl[id]) & F_LEADER) == 0:
            continue
        # counts: EXACT. Nothing here is allowed to round.
        if h_lc[co + LC_ROW] != cpu.lc[co + LC_ROW]:
            bad += 1
            if printed < MAX_PRINTED:
                print("  byte", id, "ROW gpu", h_lc[co + LC_ROW], "cpu", cpu.lc[co + LC_ROW])
                printed += 1
        if h_lc[co + LC_COL] != cpu.lc[co + LC_COL]:
            bad += 1
            if printed < MAX_PRINTED:
                print("  byte", id, "COL gpu", h_lc[co + LC_COL], "cpu", cpu.lc[co + LC_COL])
                printed += 1
        if h_wc[id] != cpu.wc[id]:
            bad += 1
            if printed < MAX_PRINTED:
                print("  byte", id, "ORD gpu", h_wc[id], "cpu", cpu.wc[id])
                printed += 1
        # LINE_ADV: eps (foldless f64 prefix vs the scan's f32 grouping).
        var g = Float64(h_wm[id])
        var e = Float64(cpu.wm[id])
        if not rel_close(g, e):
            bad += 1
            if printed < MAX_PRINTED:
                print("  byte", id, "LINE_ADV gpu", g, "cpu", e)
                printed += 1
        # resolveX's positions: eps, like paginate — f64 on CPU, f32 on device.
        for lane in range(3):
            var gp = Float64(h_lm[id * LM_STRIDE + LM_X + lane])
            var ep = Float64(cpu.lm[id * LM_STRIDE + LM_X + lane])
            if not rel_close(gp, ep):
                bad += 1
                if printed < MAX_PRINTED:
                    print("  byte", id, "pos lane", lane, "gpu", gp, "cpu", ep)
                    printed += 1
    # the fold scalar that is a COUNT: exact.
    for i in range(fx.item_count):
        var want_rows = Int(cpu.item_bounds[i * 8 + 6])
        if Int(h_rmax[i]) != want_rows:
            bad += 1
            if printed < MAX_PRINTED:
                print("  item", i, "totalRows gpu", Int(h_rmax[i]), "cpu", want_rows)
                printed += 1
    for i in range(n):
        if h_otb[i] != cpu.ord_to_byte[i]:
            bad += 1
            if printed < MAX_PRINTED:
                print("  ordToByte[", i, "] gpu", h_otb[i], "cpu", cpu.ord_to_byte[i])
                printed += 1
    return bad


def synthetic_case(
    trie: Trie, n: Int, wrap: Int, line_len: Int, ctx: DeviceContext,
    mode: Int = WRAP_DOWN, smode: Int = 0,
) raises -> Int:
    """A corpus large enough to reach the SPINE's multi-super path.

    Every checked-in fixture is under 6KB — 82 chunks, ONE super. With a single
    super the spine scan's exclusive/inclusive distinction is invisible (the
    absorbing reset at byte 0 discards the only prefix it affects) and a 33-byte
    fixture has one chunk, so no chunk-level combine happens at all. Both of those
    are exactly the paths that were wrong when this file was first written, and the
    fixtures could not have caught either. GROUP is 256, so > 16384 bytes is the
    threshold; this runs well past it."""
    var bytes = List[UInt8](unsafe_uninit_length=n)
    for i in range(n):
        bytes[i] = UInt8(10) if (i % line_len) == (line_len - 1) else UInt8(97 + (i % 26))
    var it = Item()
    it.byte_start = 0
    it.byte_count = n
    it.line_height = 1
    it.wrap_width = wrap
    it.wrap_mode = mode
    var items = List[Item]()
    items.append(it^)
    var fx = PipeFixture()
    fx.byte_len = n
    fx.item_count = 1
    fx.bytes = bytes^
    fx.trie = trie.copy()
    fx.items = items^
    return check_fixture(fx^, ctx, mode=smode)


def bench_scaling(trie: Trie, path: String, ctx: DeviceContext, cluster: Bool = False) raises:
    """Time the SAME chain the conformance suite proves, across corpus sizes.

    The GPU timing spans the whole device phase — the dispatches AND the readbacks,
    including the host-side stride derivation between resolveX and paginate. Timing
    only the kernels would flatter the GPU by hiding the part a real caller pays.
    `cluster` runs the items under cluster mode (the sequence pass's device form
    included); leader mode pays the pass's dispatches but no-ops inside the
    kernels (per-thread cluster_of/item_cluster early-returns).

    Every size runs the three statics modes: 0 = CPU decode + upload (the
    shipped form), 1 = device decode, probe unfused, 2 = fused decode+probe.
    The printed `prep` column is the host fill+upload time the mode pays (mode
    0's includes a whole leader-forced CPU pipeline run), so the A/B attributes
    the delta: upload elimination (0→1) vs dispatch fusion (1→2)."""
    var f = open(path, "r")
    var all_bytes = f.read_bytes()
    f.close()
    print("corpus:", path, "(", len(all_bytes), "bytes )", "cluster" if cluster else "leader")
    print("")
    var sizes = List[Int]()
    sizes.append(65536)
    sizes.append(262144)
    sizes.append(1048576)
    sizes.append(4194304)
    sizes.append(8388608)
    sizes.append(16777216)
    sizes.append(25165824)
    for si in range(len(sizes)):
        var nb = sizes[si]
        if nb > len(all_bytes):
            continue
        var bytes = List[UInt8](capacity=nb)
        for i in range(nb):
            bytes.append(all_bytes[i])
        for m in range(3):
            var it = Item()
            it.byte_start = 0
            it.byte_count = nb
            it.line_height = 1
            it.cluster_mode = CLUSTER_CLUSTER if cluster else CLUSTER_LEADER
            var items = List[Item]()
            items.append(it^)
            var fx = PipeFixture()
            fx.byte_len = nb
            fx.item_count = 1
            fx.bytes = List[UInt8](copy=bytes)
            fx.trie = trie.copy()
            fx.items = items^
            _ = check_fixture(fx^, ctx, True, m)


def main() raises:
    comptime assert has_accelerator(), "gpu_pipeline requires a GPU"
    var args = argv()
    if len(args) < 2:
        print("usage: mojo run -I engine engine/gpu_pipeline.mojo <fixture.pipe.bin> ...")
        return
    var ctx = DeviceContext()
    print("device:", ctx.name())
    if String(args[1]) == "--bench":
        var seed = load_pipe_fixture(String(args[2]))
        bench_scaling(seed.trie, String(args[3]), ctx)
        return
    # --bench-cluster <corpus>: the same sweep under cluster mode against the
    # REAL atlas trie (the full 4,166-sequence table), so dense emoji content
    # resolves for real instead of probing a fixture's synthetic table.
    if String(args[1]) == "--bench-cluster":
        var real_trie = load_trie_blob(String(args[2]))
        bench_scaling(real_trie, String(args[3]), ctx, cluster=True)
        return
    var total_bad = 0
    for i in range(1, len(args)):
        var path = String(args[i])
        var bad = check_case(path, ctx)
        if bad == 0:
            print("PASS", path)
        else:
            print("FAIL", path, "—", bad, "mismatches")
        total_bad += bad
    # The fixtures are all single-super. These reach the spine.
    var seed = load_pipe_fixture(String(args[1]))
    print("")
    print("multi-super cases (GROUP=256, so >16384 bytes crosses into 2+ supers):")
    var cases = List[Int]()
    cases.append(20000)
    cases.append(40000)
    cases.append(70000)
    for ci in range(len(cases)):
        var nb = cases[ci]
        # Each case in statics modes 0 and 2 — the fused path meets the spine
        # too (mode 1 is bench-only instrumentation).
        var b1 = synthetic_case(seed.trie, nb, 0, 40, ctx)
        b1 += synthetic_case(seed.trie, nb, 0, 40, ctx, smode=2)
        var b2 = synthetic_case(seed.trie, nb, 7, 23, ctx)
        b2 += synthetic_case(seed.trie, nb, 7, 23, ctx, smode=2)
        # WRAP_BACK on the same spine path. Every WrapBack FIXTURE is single-super
        # (the largest is 5,212 bytes), so without this the mode's junction term
        # would never meet a chunk-level or super-level combine on device — the
        # same blind spot this synthetic case was written for, one parameter over.
        var b3 = synthetic_case(seed.trie, nb, 7, 23, ctx, WRAP_BACK)
        b3 += synthetic_case(seed.trie, nb, 7, 23, ctx, WRAP_BACK, smode=2)
        var supers = ((nb + CHUNK - 1) // CHUNK + GROUP - 1) // GROUP
        print("  ", nb, "bytes,", supers, "supers — unwrapped", b1,
              "bad, wrapped", b2, "bad, wrapback", b3, "bad")
        total_bad += b1 + b2 + b3

    if total_bad == 0:
        print("gpu pipeline: the device chain, statics modes 0+2 — counts exact, statics bit-exact, positions within 1e-4")
    else:
        raise Error("gpu pipeline diverged")
