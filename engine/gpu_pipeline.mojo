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
# The statics arrive in one of four modes (check_fixture's `mode`): 0 decodes on
# CPU and uploads (the shipped form); 1 decodes on device with the probe unfused;
# 2 fuses decode+probe into k_decode_probe — the probe's head arrives in registers
# instead of being re-walked from bytes, and no statics cross the bus at all; 3
# swaps the probe's table form for the state walk's hash. The suite runs 0, 2 and
# 3 over every fixture and synthetic case (plus 3 with the chunked chain); modes
# 1/2/3 add a bit-exact gi/sm tier against the reference's RESOLVED statics, so
# the device decode is pinned end to end, not assumed from gpu_decode's isolated
# proof. The per-byte ITEM facts (wrap/mode/start/cluster, 28 B/byte of upload)
# are gone entirely: the kernels resolve an item per thread from the item-ranges
# table on device (ItemWalk / item_search_device).
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
from std.memory import bitcast, unsafe_memcpy
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
    F_LEADER, F_NEWLINE,
    WRAP_DOWN, WRAP_BACK, run_pipeline, CLUSTER_LEADER, CLUSTER_CLUSTER,
    item_search_device,
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
    k_cluster_probe, k_cluster_chain, k_decode_probe,
    k_chain_free, k_chain_sb_free, k_chain_sb_stitch, k_chain_sb_apply, k_chain_commit, k_chain_sb_cascade,
    build_head_bitmap, build_state_table, HEAD_BMP_WORDS, ST_STRIDE,
    BLOCK, BLOCK_LOG2, CLIST_CAP, CLIST_STRIDE, SB_BLOCKS,
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


struct ItemWalk:
    """The item-facts merge walk, device side. Seed once per thread (one
    binary search into the item ranges), then step while the next item's
    start passes the byte — register-resident, replacing the per-byte loads
    of the w/md/s/is arrays (16 B/byte on the bus) with a walk over an
    item_count-sized table that rides in cache. The while-step (not an if)
    keeps stacked byte_starts on item_for_byte's largest-start-≤-id semantics
    — the host facts walk made the same choice. With an empty item list the
    seed leaves start=0/wrap=0/mode=WRAP_DOWN and reset_at never fires — the
    old zero-fill, exactly (unreachable through the harnesses: the scan
    reference rejects an empty arena before any kernel runs, but a kernel
    can't see its callers)."""
    var it: Int      # the owning item
    var start: Int   # its byte_start
    var nxt: Int     # the NEXT item's byte_start (n when it is last)
    var wrap: Int    # its wrap width
    var mode: Int    # its wrap mode
    var has: Bool    # item_count > 0

    def __init__(
        out self,
        ir: MutPointer[UInt32, MutAnyOrigin],
        ie: MutPointer[UInt32, MutAnyOrigin],
        item_count: Int,
        id: Int,
        n: Int,
    ):
        self.it = 0
        self.start = 0
        self.nxt = n
        self.wrap = 0
        self.mode = WRAP_DOWN
        self.has = item_count > 0
        if self.has:
            self.it = item_search_device(ir, item_count, id)
            self.start = Int(ir[unsafe_offset = self.it * 2])
            self.nxt = Int(ir[unsafe_offset = (self.it + 1) * 2]) if self.it + 1 < item_count else n
            self.wrap = Int(ie[unsafe_offset = self.it * IE_STRIDE + IE_WRAP_WIDTH])
            self.mode = Int(ie[unsafe_offset = self.it * IE_STRIDE + IE_WRAP_MODE])

    def step(
        mut self,
        ir: MutPointer[UInt32, MutAnyOrigin],
        ie: MutPointer[UInt32, MutAnyOrigin],
        item_count: Int,
        id: Int,
        n: Int,
    ):
        while self.has and self.nxt <= id:
            self.it += 1
            self.start = Int(ir[unsafe_offset = self.it * 2])
            self.nxt = Int(ir[unsafe_offset = (self.it + 1) * 2]) if self.it + 1 < item_count else n
            self.wrap = Int(ie[unsafe_offset = self.it * IE_STRIDE + IE_WRAP_WIDTH])
            self.mode = Int(ie[unsafe_offset = self.it * IE_STRIDE + IE_WRAP_MODE])

    def reset_at(self, id: Int) -> Int:
        return Int(self.has and id == self.start)


def leaf_of(
    fl: MutPointer[UInt32, MutAnyOrigin], sm: MutPointer[Float32, MutAnyOrigin],
    wrap: Int, mode: Int, reset: Int,
    id: Int,
) -> E:
    """The one-byte leaf. The item facts arrive as SCALARS from the caller's
    merge walk (chunk kernels) or search (byte kernels) — they used to be three
    per-byte arrays (wrap_of/mode_of/is_start, 12 B/byte on the bus) answering
    a question that is a function of item_count, not of n."""
    var e = E()
    e.reset = reset
    e.wrap = wrap
    e.mode = mode
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
    ir: MutPointer[UInt32, MutAnyOrigin], ie: MutPointer[UInt32, MutAnyOrigin],
    pc: MutPointer[UInt32, MutAnyOrigin], pm: MutPointer[Float32, MutAnyOrigin],
    n_bytes: Int32, k: Int32, n_chunks: Int32, item_count: Int32,
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
    var w = ItemWalk(ir, ie, Int(item_count), id, n)
    while id < to:
        w.step(ir, ie, Int(item_count), id, n)
        combine(acc, leaf_of(fl, sm, w.wrap, w.mode, w.reset_at(id), id))
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
    ir: MutPointer[UInt32, MutAnyOrigin], ie: MutPointer[UInt32, MutAnyOrigin],
    xc: MutPointer[UInt32, MutAnyOrigin], xm: MutPointer[Float32, MutAnyOrigin],
    wm: MutPointer[Float32, MutAnyOrigin], wc: MutPointer[UInt32, MutAnyOrigin],
    otb: MutPointer[UInt32, MutAnyOrigin],
    n_bytes: Int32, k: Int32, n_chunks: Int32, item_count: Int32,
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
    var w = ItemWalk(ir, ie, Int(item_count), id, n)
    while id < to:
        w.step(ir, ie, Int(item_count), id, n)
        var reset = w.reset_at(id)
        if reset != 0:
            run = E()
            run.wrap = w.wrap
            run.mode = w.mode
        var f = Int(fl[unsafe_offset=id])
        if (f & F_LEADER) != 0:
            # lanes_from_prefix, inline
            var wrap = w.wrap
            var mode = w.mode
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
            otb[unsafe_offset = w.start + run.glyphs] = UInt32(id)
        combine(run, leaf_of(fl, sm, w.wrap, w.mode, reset, id))
        id += 1


# ── dispatch 7: resolveX — thread per byte ─────────────────────────────────
def k_resolve_x(
    sm: MutPointer[Float32, MutAnyOrigin], fl: MutPointer[UInt32, MutAnyOrigin],
    lm: MutPointer[Float32, MutAnyOrigin], lc: MutPointer[UInt32, MutAnyOrigin],
    items: MutPointer[Float32, MutAnyOrigin],
    items_e: MutPointer[UInt32, MutAnyOrigin], item_ranges: MutPointer[UInt32, MutAnyOrigin],
    wm: MutPointer[Float32, MutAnyOrigin], wc: MutPointer[UInt32, MutAnyOrigin],
    otb: MutPointer[UInt32, MutAnyOrigin],
    row_max: MutPointer[UInt32, MutAnyOrigin], x_max: MutPointer[UInt32, MutAnyOrigin],
    n_bytes: Int32, item_count: Int32,
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
    if Int(item_count) == 0:
        return  # unreachable through the harnesses (the scan reference rejects an empty arena first) — a kernel can't see its callers
    var mo = id * LM_STRIDE
    var it = item_search_device(item_ranges, Int(item_count), id)
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
            var q = Int(otb[unsafe_offset = Int(item_ranges[unsafe_offset = it * 2]) + ord - k])
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


# ── dispatch 8b: derive the fan stride ON DEVICE — thread per item ──────────
# ── dispatch -1: init — every zero the chain expects, one pass ─────────────
def k_init_zeros[zero_statics: Bool](
    otb: MutPointer[UInt32, MutAnyOrigin],
    cslot: MutPointer[UInt32, MutAnyOrigin],
    cblk: MutPointer[UInt32, MutAnyOrigin],
    pc: MutPointer[UInt32, MutAnyOrigin], pm: MutPointer[Float32, MutAnyOrigin],
    uc: MutPointer[UInt32, MutAnyOrigin], um: MutPointer[Float32, MutAnyOrigin],
    fc: MutPointer[UInt32, MutAnyOrigin], fm: MutPointer[Float32, MutAnyOrigin],
    xc: MutPointer[UInt32, MutAnyOrigin], xm: MutPointer[Float32, MutAnyOrigin],
    rmax: MutPointer[UInt32, MutAnyOrigin], xmax: MutPointer[UInt32, MutAnyOrigin],
    fl: MutPointer[UInt32, MutAnyOrigin], gi: MutPointer[UInt32, MutAnyOrigin],
    sm: MutPointer[Float32, MutAnyOrigin],
    n_bytes: Int32, n_blocks: Int32, n_chunks: Int32, n_supers: Int32, n_items: Int32,
):
    """All of prep's zero-init in one dispatch, at device write bandwidth. This
    replaces the host zero-fills AND uploads of the zero lanes, plus the
    eleven enqueue_fill calls (each a slow path on this backend — measured
    8-14 ms per 128 MB, scaling with n). Zero bits are zero for f32 and u32
    alike. statics (fl/gi/sm) zero only when the device decodes (modes 1-3);
    mode 0 uploads them.

    What is NOT here, and why (audited 2026-09-26, consumers read end to
    end): lm/lc/wm/wc are written by k_apply/k_resolve_x only at LEADER
    lanes and read only at leader lanes (apply, resolveX, paginate all
    early-return on non-leaders; the conformance tier compares leader lanes
    only) — zeroing continuations was 32 B/byte of dead stores. cand_end is
    read only where cand_slot != 0, and the probe writes the pair together
    at every site. What stays is load-bearing: cand_slot is the chain's
    candidacy test (`slot != 0` read at every leader in a present block —
    garbage there is a phantom candidate); cblk/row_max/x_max are
    Atomic.max targets (zero is the identity); fl/gi/sm continuation lanes
    are READ as zero by the chain's every-byte F_LEADER test; otb is
    compared over all n positions by the suite."""
    var id = Int(global_idx.x)
    if id < Int(n_bytes):
        otb[unsafe_offset=id] = 0
        cslot[unsafe_offset=id] = 0
        if zero_statics:
            fl[unsafe_offset=id] = 0
            gi[unsafe_offset=id] = 0
            var s = id * SM_STRIDE
            for k in range(SM_STRIDE):
                sm[unsafe_offset = s + k] = 0
    if id < Int(n_blocks):
        cblk[unsafe_offset=id] = 0
    if id < Int(n_chunks) * PARTIAL_COUNT_STRIDE:
        pc[unsafe_offset=id] = 0
        xc[unsafe_offset=id] = 0
    if id < Int(n_chunks) * PARTIAL_MEASURE_STRIDE:
        pm[unsafe_offset=id] = 0
        xm[unsafe_offset=id] = 0
    if id < Int(n_supers) * PARTIAL_COUNT_STRIDE:
        uc[unsafe_offset=id] = 0
        fc[unsafe_offset=id] = 0
    if id < Int(n_supers) * PARTIAL_MEASURE_STRIDE:
        um[unsafe_offset=id] = 0
        fm[unsafe_offset=id] = 0
    if id < Int(n_items):
        rmax[unsafe_offset=id] = 0
        xmax[unsafe_offset=id] = 0


def k_derive_stride(
    x_max: MutPointer[UInt32, MutAnyOrigin],
    items_e: MutPointer[UInt32, MutAnyOrigin],
    page_gap_x: MutPointer[Float32, MutAnyOrigin],
    strides: MutPointer[Float32, MutAnyOrigin],
    item_count: Int32,
):
    """The glyph_pipeline.mojo derive_stride, transcribed: widest row + pageGapX
    when paged, else 0. This used to be a host round-trip between resolveX and
    paginate (readback, derive, upload — a full drain mid-chain). The one
    numerical judgment: device computes f32(xmax) + f32(gap) where the host
    added in f64 and narrowed — ≤1 ulp of stride, feeding only paginate's X
    fan, which the suite compares in the 1e-4 position tier. Orders of
    magnitude inside the contract; no exact lane reads it."""
    var i = Int(global_idx.x)
    if i >= Int(item_count):
        return
    var ie = i * IE_STRIDE
    var has_page = items_e[unsafe_offset = ie + IE_HAS_PAGE] != 0
    var rows = Int(items_e[unsafe_offset = ie + IE_PAGE_ROWS])
    if not has_page or rows <= 0:
        strides[unsafe_offset=i] = 0
        return
    strides[unsafe_offset=i] = key_to_float(x_max[unsafe_offset=i]) + page_gap_x[unsafe_offset=i]


# ── dispatch 8: paginate — thread per byte, pure per-slot remap ────────────
def k_paginate(
    lm: MutPointer[Float32, MutAnyOrigin], fl: MutPointer[UInt32, MutAnyOrigin],
    lc: MutPointer[UInt32, MutAnyOrigin],
    items: MutPointer[Float32, MutAnyOrigin],
    items_e: MutPointer[UInt32, MutAnyOrigin], item_ranges: MutPointer[UInt32, MutAnyOrigin],
    strides: MutPointer[Float32, MutAnyOrigin], n_bytes: Int32, item_count: Int32,
):
    var id = global_idx.x
    if id >= Int(n_bytes):
        return
    if (Int(fl[unsafe_offset=id]) & F_LEADER) == 0:
        return
    if Int(item_count) == 0:
        return  # unreachable through the harnesses — see k_resolve_x
    var it = item_search_device(item_ranges, Int(item_count), id)
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


def stage_name(i: Int) -> String:
    if i == 0:
        return "pass(decode+probe)"
    if i == 1:
        return "chain"
    if i == 2:
        return "chunkReduce"
    if i == 3:
        return "spineReduce"
    if i == 4:
        return "spineScan"
    if i == 5:
        return "partialScan"
    if i == 6:
        return "apply"
    if i == 7:
        return "resolveX"
    if i == 8:
        return "stride derive (host)"
    if i == 9:
        return "paginate"
    if i == 10:
        return "final readbacks"
    if i == 11:
        return "chain_free"
    if i == 12:
        return "chain_sb_cascade"
    if i == 13:
        return "chain_sb_free"
    if i == 14:
        return "chain_sb_stitch"
    if i == 15:
        return "chain_sb_apply"
    return "chain_commit"


def mark(ctx: DeviceContext, mut t_prev: Int, mut stages: List[Int], idx: Int) raises:
    """Per-dispatch timing: synchronize, then stamp. Profiling only — the
    extra syncs serialize host and device, so stage times carry launch gaps
    that overlap in a free-running chain. Read them as per-dispatch shares,
    not as a sum that must equal gpu_ns."""
    ctx.synchronize()
    var t_now = perf_counter_ns()
    stages[idx] = t_now - t_prev
    t_prev = t_now


comptime PREP_STAGES = 12


def pmark(mut tk: Int, mut pk: List[Int], idx: Int):
    """Prep-stage timing: stamp into bucket idx, retake the base. Unlike mark
    there is no synchronize here — prep's dispatches are async submissions, so
    `upload-sub` measures submission cost and the transfer itself lands in
    `drain` (the final sync before the chain)."""
    var t_now = perf_counter_ns()
    pk[idx] = t_now - tk
    tk = t_now


def prep_stage_name(i: Int) -> String:
    if i == 0:
        return "item-tabs"
    if i == 1:
        return "host-bufs-a"
    if i == 2:
        return "host-bufs-b"
    if i == 3:
        return "mode0-decode"
    if i == 4:
        return "trie-tabs"
    if i == 5:
        return "item-copy"
    if i == 6:
        return "host-bufs-c"
    if i == 7:
        return "item-fills"
    if i == 8:
        return "device-bufs"
    if i == 9:
        return "upload-sub"
    if i == 10:
        return "init-dispatch"
    return "drain"


def check_case(path: String, ctx: DeviceContext) raises -> Int:
    # Every fixture runs mode 0 (CPU-decoded statics, unfused probe), mode 2
    # (fused decode+probe, binary search) AND mode 3 (fused, state walk)
    # against the same reference — plus mode 3 with the CHUNKED chain, so the
    # carry-stitched commit is proven bit-exact beside the serial one.
    var bad = check_fixture(load_pipe_fixture(path), ctx)
    bad += check_fixture(load_pipe_fixture(path), ctx, mode=2)
    bad += check_fixture(load_pipe_fixture(path), ctx, mode=3)
    bad += check_fixture(load_pipe_fixture(path), ctx, mode=3, chain=1)
    return bad


def check_fixture(var fx: PipeFixture, ctx: DeviceContext, bench: Bool = False, mode: Int = 0, profile: Bool = False, chain: Int = 0) raises -> Int:
    """The device chain over one fixture, in one of three statics modes:
    0 — statics decoded on CPU (leader-forced) and uploaded; probe+chain on
        device. The shipped form.
    1 — decode on device (k_decode_probe[False]), probe unfused. Bench-only:
        isolates the upload elimination from the dispatch fusion.
    2 — decode and probe fused (k_decode_probe[True]); chain unchanged.
    3 — as 2, with the probe's table form swapped: the state walk's hash
        (build_state_table, built at load from the same section) replaces the
        descending binary searches.
    Modes 1/2/3 additionally compare the device-produced statics (gi, sm)
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
    var pk = List[Int](length=PREP_STAGES, fill=0)
    var tk = t_prep

    # The sequence pass runs ON DEVICE here (it used to arrive pre-computed in
    # the uploaded lanes). Mode 0 seeds the decoded-but-unresolved statics from
    # a leader-forced CPU run (the fills below); modes 1/2 decode on device via
    # k_decode_probe (decode's kernel is proven in gpu_decode; the fused form is
    # pinned by this suite's gi/sm tier). The device probe+chain dispatches
    # rewrite the statics before the scan consumes any advance. The scan
    # reference (cpu, resolving) stays the comparison either way, so the pass is
    # covered end to end.

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
    pmark(tk, pk, 0)

    # ── upload the DECODED lanes (decode itself is proven in gpu_decode) ─────
    var h_fl = ctx.enqueue_create_host_buffer[DType.uint32](n)
    var h_sm = ctx.enqueue_create_host_buffer[DType.float32](n * SM_STRIDE)
    var h_lm = ctx.enqueue_create_host_buffer[DType.float32](n * LM_STRIDE)
    var h_lc = ctx.enqueue_create_host_buffer[DType.uint32](n * LC_STRIDE)
    var h_wm = ctx.enqueue_create_host_buffer[DType.float32](n)
    var h_wc = ctx.enqueue_create_host_buffer[DType.uint32](n)
    var h_otb = ctx.enqueue_create_host_buffer[DType.uint32](n)
    ctx.synchronize()
    pmark(tk, pk, 1)
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
    var h_ir = ctx.enqueue_create_host_buffer[DType.uint32](item_count * 2)
    var h_ic = ctx.enqueue_create_host_buffer[DType.uint32](item_count)
    # The decode trie tables, needed only when decode runs on device. Filled
    # and uploaded solely in modes 1/2/3; mode 0 never reads them.
    var n_idx = len(fx.trie.block_index)
    var n_bm = len(fx.trie.blocks_m)
    var n_bc = len(fx.trie.blocks_c)
    var h_index = ctx.enqueue_create_host_buffer[DType.uint32](n_idx)
    var h_bm = ctx.enqueue_create_host_buffer[DType.float32](n_bm)
    var h_bc = ctx.enqueue_create_host_buffer[DType.uint32](n_bc)
    # The walk form's state table (mode 3): built at load time from the SAME
    # sequence section the search form reads, so the two table forms share one
    # source and cannot drift. Mode 3 also leaves the sequence section itself
    # off the bus — the walk's hash replaces it.
    var st_tab = List[UInt32]()
    var st_mask = UInt32(0)
    if mode == 3:
        st_tab = build_state_table(fx.trie.seq, seq_count, Int(fx.trie.seq_max))
        st_mask = UInt32(len(st_tab) // ST_STRIDE - 1)
    var n_st = len(st_tab) if len(st_tab) > 0 else 1
    var h_tab = ctx.enqueue_create_host_buffer[DType.uint32](n_st)
    ctx.synchronize()
    pmark(tk, pk, 2)
    if mode == 3 and len(st_tab) > 0:
        unsafe_memcpy(dest=h_tab.unsafe_ptr(), src=st_tab.unsafe_ptr(), count=len(st_tab))
    if mode == 0:
        var items_leader = List[Item]()
        for i in range(len(fx.items)):
            var t = fx.items[i].copy()
            t.cluster_mode = CLUSTER_LEADER
            items_leader.append(t^)
        var dec = run_pipeline[witness=False](fx.bytes, fx.trie, items_leader^)
        unsafe_memcpy(dest=h_fl.unsafe_ptr(), src=dec.fl.unsafe_ptr(), count=n)
        unsafe_memcpy(dest=h_gi.unsafe_ptr(), src=dec.gi.unsafe_ptr(), count=n)
        unsafe_memcpy(dest=h_sm.unsafe_ptr(), src=dec.sm.unsafe_ptr(), count=n * SM_STRIDE)
    pmark(tk, pk, 3)
    if mode != 0:
        unsafe_memcpy(dest=h_index.unsafe_ptr(), src=fx.trie.block_index.unsafe_ptr(), count=n_idx)
        unsafe_memcpy(dest=h_bm.unsafe_ptr(), src=fx.trie.blocks_m.unsafe_ptr(), count=n_bm)
        unsafe_memcpy(dest=h_bc.unsafe_ptr(), src=fx.trie.blocks_c.unsafe_ptr(), count=n_bc)
    unsafe_memcpy(dest=h_bytes.unsafe_ptr(), src=fx.bytes.unsafe_ptr(), count=n)
    if mode != 3:
        if len(fx.trie.seq) > 0:
            unsafe_memcpy(dest=h_seq.unsafe_ptr(), src=fx.trie.seq.unsafe_ptr(), count=len(fx.trie.seq))
        else:
            h_seq[0] = 0
    var head_bmp = build_head_bitmap(fx.trie)
    unsafe_memcpy(dest=h_bmp.unsafe_ptr(), src=head_bmp.unsafe_ptr(), count=HEAD_BMP_WORDS)
    pmark(tk, pk, 4)
    if item_count > 0:
        unsafe_memcpy(dest=h_ir.unsafe_ptr(), src=item_ranges.unsafe_ptr(), count=item_count * 2)
        unsafe_memcpy(dest=h_ic.unsafe_ptr(), src=item_cluster.unsafe_ptr(), count=item_count)
    # The output-lane zeros moved device-side: k_init_zeros (dispatched below)
    # writes them at device bandwidth, and they no longer cross the bus.
    pmark(tk, pk, 5)
    # The seven per-byte item-facts arrays (w/md/s/is/io/ceof/cof — 28 B/byte
    # of upload and the loop that filled them) are GONE: the kernels resolve
    # an item per byte from d_ir/d_ie/d_ic themselves (ItemWalk in the chunk
    # kernels, item_search_device in the byte kernels).

    var ni0 = fx.item_count if fx.item_count > 0 else 1
    var h_it = ctx.enqueue_create_host_buffer[DType.float32](ni0 * IM_STRIDE)
    var h_ie = ctx.enqueue_create_host_buffer[DType.uint32](ni0 * IE_STRIDE)
    var h_rmax = ctx.enqueue_create_host_buffer[DType.uint32](ni0)
    var h_xmax = ctx.enqueue_create_host_buffer[DType.uint32](ni0)
    # pageGapX, per item — the stride derive's one input that never had a
    # device home (it lived only in the host's between-dispatches derive).
    var h_pg = ctx.enqueue_create_host_buffer[DType.float32](ni0)
    ctx.synchronize()
    pmark(tk, pk, 6)
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
        h_pg[i] = Float32(t.page_gap_x)
    pmark(tk, pk, 7)

    var d_fl = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_sm = ctx.enqueue_create_buffer[DType.float32](n * SM_STRIDE)
    var d_lm = ctx.enqueue_create_buffer[DType.float32](n * LM_STRIDE)
    var d_lc = ctx.enqueue_create_buffer[DType.uint32](n * LC_STRIDE)
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
    var d_pg = ctx.enqueue_create_buffer[DType.float32](ni)
    var d_rmax = ctx.enqueue_create_buffer[DType.uint32](ni)
    var d_xmax = ctx.enqueue_create_buffer[DType.uint32](ni)
    var d_bytes = ctx.enqueue_create_buffer[DType.uint8](n)
    var d_gi = ctx.enqueue_create_buffer[DType.uint32](n)
    # The decode trie tables: uploaded and read only in modes 1/2/3 (device
    # decode). In mode 0 they sit unused — allocated, never filled.
    var d_index = ctx.enqueue_create_buffer[DType.uint32](n_idx)
    var d_bm = ctx.enqueue_create_buffer[DType.float32](n_bm)
    var d_bc = ctx.enqueue_create_buffer[DType.uint32](n_bc)
    # The walk form's state table: uploaded only in mode 3.
    var d_tab = ctx.enqueue_create_buffer[DType.uint32](n_st)
    var d_seq = ctx.enqueue_create_buffer[DType.uint32](n_seq)
    var d_bmp = ctx.enqueue_create_buffer[DType.uint32](HEAD_BMP_WORDS)
    var d_ir = ctx.enqueue_create_buffer[DType.uint32](item_count * 2)
    var d_ic = ctx.enqueue_create_buffer[DType.uint32](item_count)
    var d_cslot = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_cend = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_cblk = ctx.enqueue_create_buffer[DType.uint32]((n + 127) >> 7)
    # The chunked chain's carry arrays (chain=1): per-block open span end, and
    # the stitch's resolved in-carry. Same block count as the presence table.
    var d_cout = ctx.enqueue_create_buffer[DType.uint32]((n + 127) >> 7)
    var d_cres = ctx.enqueue_create_buffer[DType.uint32]((n + 127) >> 7)
    # The compact candidate lists the stitch/commit walk (chain=1).
    var d_ccount = ctx.enqueue_create_buffer[DType.uint32]((n + 127) >> 7)
    var d_clist = ctx.enqueue_create_buffer[DType.uint32](((n + 127) >> 7) * CLIST_CAP * CLIST_STRIDE)
    # The two-level stitch's super-block exits and resolved entries (chain=1).
    var d_sbout = ctx.enqueue_create_buffer[DType.uint32](((n + 127) >> 7) // SB_BLOCKS + 1)
    var d_sbres = ctx.enqueue_create_buffer[DType.uint32](((n + 127) >> 7) // SB_BLOCKS + 1)
    # The zone-index byte table and the cascaded zone outs (chain=1).
    var d_czidx = ctx.enqueue_create_buffer[DType.uint8](((n + 127) >> 7) * BLOCK)
    var d_csc = ctx.enqueue_create_buffer[DType.uint32](((n + 127) >> 7) * (CLIST_CAP + 1))
    pmark(tk, pk, 8)
    if mode == 0:
        ctx.enqueue_copy(dst_buf=d_fl, src_buf=h_fl)
        ctx.enqueue_copy(dst_buf=d_sm, src_buf=h_sm)
    else:
        # Device decode writes every leader lane; continuations stay zero —
        # k_init_zeros below owns that zeroing now (one dispatch, not three
        # fills).
        ctx.enqueue_copy(dst_buf=d_index, src_buf=h_index)
        ctx.enqueue_copy(dst_buf=d_bm, src_buf=h_bm)
        ctx.enqueue_copy(dst_buf=d_bc, src_buf=h_bc)
        if mode == 3:
            ctx.enqueue_copy(dst_buf=d_tab, src_buf=h_tab)
    ctx.enqueue_copy(dst_buf=d_it, src_buf=h_it)
    ctx.enqueue_copy(dst_buf=d_ie, src_buf=h_ie)
    ctx.enqueue_copy(dst_buf=d_pg, src_buf=h_pg)
    ctx.enqueue_copy(dst_buf=d_bytes, src_buf=h_bytes)
    if mode == 0:
        ctx.enqueue_copy(dst_buf=d_gi, src_buf=h_gi)
    if mode != 3:
        # The walk form reads its own hash, not the sequence section.
        ctx.enqueue_copy(dst_buf=d_seq, src_buf=h_seq)
    ctx.enqueue_copy(dst_buf=d_bmp, src_buf=h_bmp)
    ctx.enqueue_copy(dst_buf=d_ir, src_buf=h_ir)
    ctx.enqueue_copy(dst_buf=d_ic, src_buf=h_ic)
    pmark(tk, pk, 9)
    # One dispatch writes every zero the chain expects (see k_init_zeros) —
    # this replaces eleven enqueue_fill calls that each cost like a slow
    # kernel launch on this backend, plus the zero lanes' host fills+uploads.
    comptime B = 128
    if mode == 0:
        ctx.enqueue_function[k_init_zeros[False]](
            d_otb.unsafe_ptr(),
            d_cslot.unsafe_ptr(), d_cblk.unsafe_ptr(),
            d_pc.unsafe_ptr(), d_pm.unsafe_ptr(),
            d_uc.unsafe_ptr(), d_um.unsafe_ptr(),
            d_fc.unsafe_ptr(), d_fm.unsafe_ptr(),
            d_xc.unsafe_ptr(), d_xm.unsafe_ptr(),
            d_rmax.unsafe_ptr(), d_xmax.unsafe_ptr(),
            d_fl.unsafe_ptr(), d_gi.unsafe_ptr(), d_sm.unsafe_ptr(),
            Int32(n), Int32((n + 127) >> 7), Int32(n_chunks), Int32(n_supers), Int32(ni),
            grid_dim=(n + B - 1) // B, block_dim=B,
        )
    else:
        ctx.enqueue_function[k_init_zeros[True]](
            d_otb.unsafe_ptr(),
            d_cslot.unsafe_ptr(), d_cblk.unsafe_ptr(),
            d_pc.unsafe_ptr(), d_pm.unsafe_ptr(),
            d_uc.unsafe_ptr(), d_um.unsafe_ptr(),
            d_fc.unsafe_ptr(), d_fm.unsafe_ptr(),
            d_xc.unsafe_ptr(), d_xm.unsafe_ptr(),
            d_rmax.unsafe_ptr(), d_xmax.unsafe_ptr(),
            d_fl.unsafe_ptr(), d_gi.unsafe_ptr(), d_sm.unsafe_ptr(),
            Int32(n), Int32((n + 127) >> 7), Int32(n_chunks), Int32(n_supers), Int32(ni),
            grid_dim=(n + B - 1) // B, block_dim=B,
        )
    pmark(tk, pk, 10)

    # ── the chain. Every intermediate stays on device. ──────────────────────
    ctx.synchronize()
    var g0 = perf_counter_ns()
    var prep_ns = g0 - t_prep
    pmark(tk, pk, 11)
    var stages = List[Int](length=17, fill=0)
    var t_prev = g0
    # THE SEQUENCE PASS, on device. Mode 0: the probe (thread per byte) writes
    # candidates over uploaded statics, the chain (thread per item) commits
    # them. Mode 1 decodes on device first (upload eliminated, probe unfused).
    # Mode 2 fuses decode+probe into k_decode_probe — one dispatch, and the
    # probe's head arrives in registers instead of being re-walked from bytes.
    # Mode 3 swaps the probe's table form: the state walk's hash replaces the
    # descending binary searches (and the sequence section stays off the bus).
    # The scan's kernels below read the rewritten statics — same order as the
    # CPU's resolve-before-fold.
    if mode == 2:
        ctx.enqueue_function[k_decode_probe[True, False]](
            d_bytes.unsafe_ptr(), d_index.unsafe_ptr(), d_bm.unsafe_ptr(), d_bc.unsafe_ptr(),
            d_sm.unsafe_ptr(), d_gi.unsafe_ptr(), d_fl.unsafe_ptr(),
            d_ir.unsafe_ptr(), d_ic.unsafe_ptr(),
            d_seq.unsafe_ptr(), d_bmp.unsafe_ptr(),
            d_cslot.unsafe_ptr(), d_cend.unsafe_ptr(), d_cblk.unsafe_ptr(),
            # the walk's tab is comptime-eliminated in the search form; any
            # non-aliased u32 pointer satisfies the signature
            d_lc.unsafe_ptr(), UInt32(0),
            Int32(n), Int32(item_count), Int32(seq_count), Int32(fx.trie.seq_max),
            grid_dim=(n + B - 1) // B, block_dim=B,
        )
    elif mode == 3:
        ctx.enqueue_function[k_decode_probe[True, True]](
            d_bytes.unsafe_ptr(), d_index.unsafe_ptr(), d_bm.unsafe_ptr(), d_bc.unsafe_ptr(),
            d_sm.unsafe_ptr(), d_gi.unsafe_ptr(), d_fl.unsafe_ptr(),
            d_ir.unsafe_ptr(), d_ic.unsafe_ptr(),
            d_seq.unsafe_ptr(), d_bmp.unsafe_ptr(),
            d_cslot.unsafe_ptr(), d_cend.unsafe_ptr(), d_cblk.unsafe_ptr(),
            d_tab.unsafe_ptr(), st_mask,
            Int32(n), Int32(item_count), Int32(seq_count), Int32(fx.trie.seq_max),
            grid_dim=(n + B - 1) // B, block_dim=B,
        )
    else:
        if mode == 1:
            ctx.enqueue_function[k_decode_probe[False, False]](
                d_bytes.unsafe_ptr(), d_index.unsafe_ptr(), d_bm.unsafe_ptr(), d_bc.unsafe_ptr(),
                d_sm.unsafe_ptr(), d_gi.unsafe_ptr(), d_fl.unsafe_ptr(),
                d_ir.unsafe_ptr(), d_ic.unsafe_ptr(),
                d_seq.unsafe_ptr(), d_bmp.unsafe_ptr(),
                d_cslot.unsafe_ptr(), d_cend.unsafe_ptr(), d_cblk.unsafe_ptr(),
                # tab is comptime-eliminated without the walk, as above
                d_lc.unsafe_ptr(), UInt32(0),
                Int32(n), Int32(item_count), Int32(seq_count), Int32(fx.trie.seq_max),
                grid_dim=(n + B - 1) // B, block_dim=B,
            )
        ctx.enqueue_function[k_cluster_probe](
            d_bytes.unsafe_ptr(), d_ir.unsafe_ptr(), d_ic.unsafe_ptr(),
            d_seq.unsafe_ptr(), d_bmp.unsafe_ptr(), d_gi.unsafe_ptr(), d_sm.unsafe_ptr(), d_fl.unsafe_ptr(),
            d_cslot.unsafe_ptr(), d_cend.unsafe_ptr(), d_cblk.unsafe_ptr(),
            Int32(n), Int32(item_count), Int32(seq_count), Int32(fx.trie.seq_max),
            grid_dim=(n + B - 1) // B, block_dim=B,
        )
    if profile:
        mark(ctx, t_prev, stages, 0)
    if chain == 1:
        # The chunk-parallel chain: free walk per 128B block, one-thread
        # stitch of the one-integer carry, commit per block with the
        # resolved in-carry. Item-shape independent — a single-item corpus
        # stops being a trap.
        var nc = (n + BLOCK - 1) >> BLOCK_LOG2
        var nsb = (nc + SB_BLOCKS - 1) // SB_BLOCKS
        ctx.enqueue_function[k_chain_free](
            d_bytes.unsafe_ptr(), d_cslot.unsafe_ptr(), d_cend.unsafe_ptr(), d_cblk.unsafe_ptr(),
            d_ccount.unsafe_ptr(), d_clist.unsafe_ptr(), d_czidx.unsafe_ptr(), d_cout.unsafe_ptr(),
            Int32(n), Int32(nc),
            grid_dim=(nc + B - 1) // B, block_dim=B,
        )
        if profile:
            mark(ctx, t_prev, stages, 11)
        ctx.enqueue_function[k_chain_sb_cascade](
            d_bytes.unsafe_ptr(), d_cslot.unsafe_ptr(), d_cend.unsafe_ptr(),
            d_gi.unsafe_ptr(), d_sm.unsafe_ptr(), d_fl.unsafe_ptr(),
            fx.trie.bitmap_advance, d_ccount.unsafe_ptr(), d_clist.unsafe_ptr(), d_czidx.unsafe_ptr(),
            d_cout.unsafe_ptr(), d_csc.unsafe_ptr(),
            Int32(n), Int32(nc),
            grid_dim=(nc + B - 1) // B, block_dim=B,
        )
        if profile:
            mark(ctx, t_prev, stages, 12)
        ctx.enqueue_function[k_chain_sb_free](
            d_bytes.unsafe_ptr(), d_cslot.unsafe_ptr(), d_cend.unsafe_ptr(),
            d_gi.unsafe_ptr(), d_sm.unsafe_ptr(), d_fl.unsafe_ptr(),
            fx.trie.bitmap_advance, d_ccount.unsafe_ptr(), d_clist.unsafe_ptr(), d_czidx.unsafe_ptr(),
            d_cout.unsafe_ptr(), d_sbout.unsafe_ptr(),
            Int32(n), Int32(nc),
            grid_dim=(nsb + B - 1) // B, block_dim=B,
        )
        if profile:
            mark(ctx, t_prev, stages, 13)
        ctx.enqueue_function[k_chain_sb_stitch](
            d_bytes.unsafe_ptr(), d_cslot.unsafe_ptr(), d_cend.unsafe_ptr(),
            d_gi.unsafe_ptr(), d_sm.unsafe_ptr(), d_fl.unsafe_ptr(),
            fx.trie.bitmap_advance, d_ccount.unsafe_ptr(), d_clist.unsafe_ptr(), d_czidx.unsafe_ptr(),
            d_cout.unsafe_ptr(), d_sbout.unsafe_ptr(), d_sbres.unsafe_ptr(), d_csc.unsafe_ptr(),
            Int32(n), Int32(nc),
            grid_dim=1, block_dim=1,
        )
        if profile:
            mark(ctx, t_prev, stages, 14)
        ctx.enqueue_function[k_chain_sb_apply](
            d_bytes.unsafe_ptr(), d_cslot.unsafe_ptr(), d_cend.unsafe_ptr(),
            d_gi.unsafe_ptr(), d_sm.unsafe_ptr(), d_fl.unsafe_ptr(),
            fx.trie.bitmap_advance, d_ccount.unsafe_ptr(), d_clist.unsafe_ptr(), d_czidx.unsafe_ptr(),
            d_cout.unsafe_ptr(), d_sbres.unsafe_ptr(), d_cres.unsafe_ptr(),
            Int32(n), Int32(nc),
            grid_dim=(nsb + B - 1) // B, block_dim=B,
        )
        if profile:
            mark(ctx, t_prev, stages, 15)
        ctx.enqueue_function[k_chain_commit](
            d_bytes.unsafe_ptr(), d_cslot.unsafe_ptr(), d_cend.unsafe_ptr(), d_cblk.unsafe_ptr(),
            d_cres.unsafe_ptr(), d_ccount.unsafe_ptr(), d_clist.unsafe_ptr(),
            d_gi.unsafe_ptr(), d_sm.unsafe_ptr(), d_fl.unsafe_ptr(),
            fx.trie.bitmap_advance,
            Int32(n), Int32(nc),
            grid_dim=(nc + B - 1) // B, block_dim=B,
        )
        if profile:
            mark(ctx, t_prev, stages, 16)
    else:
        ctx.enqueue_function[k_cluster_chain](
            d_bytes.unsafe_ptr(), d_ir.unsafe_ptr(), d_ic.unsafe_ptr(),
            d_cslot.unsafe_ptr(), d_cend.unsafe_ptr(), d_cblk.unsafe_ptr(),
            d_gi.unsafe_ptr(), d_sm.unsafe_ptr(), d_fl.unsafe_ptr(),
            fx.trie.bitmap_advance, Int32(item_count),
            grid_dim=(item_count + 63) // 64, block_dim=64,
        )
    if profile:
        mark(ctx, t_prev, stages, 1)
    ctx.enqueue_function[k_chunk_reduce](
        d_fl.unsafe_ptr(), d_sm.unsafe_ptr(), d_ir.unsafe_ptr(), d_ie.unsafe_ptr(),
        d_pc.unsafe_ptr(), d_pm.unsafe_ptr(),
        Int32(n), Int32(CHUNK), Int32(n_chunks), Int32(item_count),
        grid_dim=(n_chunks + B - 1) // B, block_dim=B,
    )
    if profile:
        mark(ctx, t_prev, stages, 2)
    ctx.enqueue_function[k_spine_reduce](
        d_pc.unsafe_ptr(), d_pm.unsafe_ptr(), d_uc.unsafe_ptr(), d_um.unsafe_ptr(),
        Int32(n_chunks), Int32(GROUP), Int32(n_supers),
        grid_dim=(n_supers + B - 1) // B, block_dim=B,
    )
    if profile:
        mark(ctx, t_prev, stages, 3)
    ctx.enqueue_function[k_spine_scan](
        d_uc.unsafe_ptr(), d_um.unsafe_ptr(), d_fc.unsafe_ptr(), d_fm.unsafe_ptr(),
        Int32(n_supers), grid_dim=1, block_dim=1,
    )
    if profile:
        mark(ctx, t_prev, stages, 4)
    ctx.enqueue_function[k_partial_scan](
        d_pc.unsafe_ptr(), d_pm.unsafe_ptr(), d_fc.unsafe_ptr(), d_fm.unsafe_ptr(),
        d_xc.unsafe_ptr(), d_xm.unsafe_ptr(),
        Int32(n_chunks), Int32(GROUP), Int32(n_supers),
        grid_dim=(n_supers + B - 1) // B, block_dim=B,
    )
    if profile:
        mark(ctx, t_prev, stages, 5)
    ctx.enqueue_function[k_apply](
        d_fl.unsafe_ptr(), d_sm.unsafe_ptr(), d_lm.unsafe_ptr(), d_lc.unsafe_ptr(),
        d_ir.unsafe_ptr(), d_ie.unsafe_ptr(), d_xc.unsafe_ptr(), d_xm.unsafe_ptr(),
        d_wm.unsafe_ptr(), d_wc.unsafe_ptr(), d_otb.unsafe_ptr(),
        Int32(n), Int32(CHUNK), Int32(n_chunks), Int32(item_count),
        grid_dim=(n_chunks + B - 1) // B, block_dim=B,
    )
    if profile:
        mark(ctx, t_prev, stages, 6)
    ctx.enqueue_function[k_resolve_x](
        d_sm.unsafe_ptr(), d_fl.unsafe_ptr(), d_lm.unsafe_ptr(), d_lc.unsafe_ptr(),
        d_it.unsafe_ptr(), d_ie.unsafe_ptr(), d_ir.unsafe_ptr(),
        d_wm.unsafe_ptr(), d_wc.unsafe_ptr(), d_otb.unsafe_ptr(),
        d_rmax.unsafe_ptr(), d_xmax.unsafe_ptr(),
        Int32(n), Int32(item_count), grid_dim=(n + B - 1) // B, block_dim=B,
    )
    if profile:
        mark(ctx, t_prev, stages, 7)
    # The fan stride is DERIVED from each item's widest fold row — a fold scalar
    # resolveX just produced. Derived ON DEVICE (k_derive_stride): the mid-chain
    # readback / host derive / upload and its full drain are gone.
    var d_st = ctx.enqueue_create_buffer[DType.float32](ni0)
    ctx.enqueue_function[k_derive_stride](
        d_xmax.unsafe_ptr(), d_ie.unsafe_ptr(), d_pg.unsafe_ptr(), d_st.unsafe_ptr(),
        Int32(item_count),
        grid_dim=(item_count + 63) // 64, block_dim=64,
    )
    if profile:
        mark(ctx, t_prev, stages, 8)
    ctx.enqueue_function[k_paginate](
        d_lm.unsafe_ptr(), d_fl.unsafe_ptr(), d_lc.unsafe_ptr(),
        d_it.unsafe_ptr(), d_ie.unsafe_ptr(), d_ir.unsafe_ptr(),
        d_st.unsafe_ptr(), Int32(n), Int32(item_count),
        grid_dim=(n + B - 1) // B, block_dim=B,
    )
    if profile:
        mark(ctx, t_prev, stages, 9)
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
    if profile:
        mark(ctx, t_prev, stages, 10)
    ctx.synchronize()
    var gpu_ns = perf_counter_ns() - g0
    if bench:
        var mb = Float64(n) / 1048576.0
        print(
            "  ", n, "B  mode", mode, " chain", chain, "  prep", Float64(prep_ns) / 1e6,
            "ms   |   cpu(sharded)", Float64(cpu_ns) / 1e6, "ms =",
            mb / (Float64(cpu_ns) / 1e9), "MB/s   |   gpu", Float64(gpu_ns) / 1e6,
            "ms =", mb / (Float64(gpu_ns) / 1e9), "MB/s   |   x",
            Float64(cpu_ns) / Float64(gpu_ns),
        )
        if profile:
            for i in range(17):
                print("      ", stage_name(i), Float64(stages[i]) / 1e6, "ms")
            for i in range(PREP_STAGES):
                print("      prep/", prep_stage_name(i), Float64(pk[i]) / 1e6, "ms")
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


def bench_modes(trie: Trie, bytes: List[UInt8], items: List[Item], ctx: DeviceContext, profile: Bool) raises:
    """One timing block: modes 0-3 × chain 0/1 over the same (bytes, items)."""
    for m in range(4):
        for ch in range(2):
            var fx = PipeFixture()
            fx.byte_len = len(bytes)
            fx.item_count = len(items)
            fx.bytes = List[UInt8](copy=bytes)
            fx.trie = trie.copy()
            fx.items = List[Item](copy=items)
            _ = check_fixture(fx^, ctx, True, m, profile, ch)


def bench_scaling(trie: Trie, path: String, ctx: DeviceContext, cluster: Bool = False, profile: Bool = False) raises:
    """Time the SAME chain the conformance suite proves, across corpus sizes.

    The GPU timing spans the whole device phase — the dispatches AND the readbacks,
    including the host-side stride derivation between resolveX and paginate. Timing
    only the kernels would flatter the GPU by hiding the part a real caller pays.
    `cluster` runs the items under cluster mode (the sequence pass's device form
    included); leader mode pays the pass's dispatches but no-ops inside the
    kernels (per-thread item_cluster early-returns behind the item search).

    Every size runs the statics modes: 0 = CPU decode + upload (the shipped
    form), 1 = device decode, probe unfused, 2 = fused decode+probe (binary
    search), 3 = fused decode+probe (state walk). The printed `prep` column is
    the host fill+upload time the mode pays (mode 0's includes a whole
    leader-forced CPU pipeline run), so the A/B attributes the delta: upload
    elimination (0→1), dispatch fusion (1→2), table form (2→3)."""
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
        var it = Item()
        it.byte_start = 0
        it.byte_count = nb
        it.line_height = 1
        it.cluster_mode = CLUSTER_CLUSTER if cluster else CLUSTER_LEADER
        var items = List[Item]()
        items.append(it^)
        bench_modes(trie, bytes, items, ctx, profile)


def bench_items(trie: Trie, path: String, ctx: DeviceContext, profile: Bool = False) raises:
    """Item-count sweep over the FULL corpus — the A/B harness for the
    device-side item search (ItemWalk / item_search_device), which replaced the
    seven per-byte item-facts arrays (w/md/s/is/io/ceof/cof — 28 B/byte of
    upload plus the host facts loop) with a per-thread resolve over the item
    ranges the chain already uploaded.

    bench_scaling holds item_count=1, which blinds exactly this trade: the
    search's per-byte cost grows with log2(items); the facts upload it replaced
    did not depend on items at all. The product case is a repo load — ~1.3k
    items over ~100 MB — so sweep counts 1 → 4096 and let the crossover show
    itself. Splits are contiguous equal chunks (the last takes the remainder);
    boundaries fall where they fall, mid-codepoint included — the timing
    doesn't care, and the suite owns correctness."""
    var f = open(path, "r")
    var all_bytes = f.read_bytes()
    f.close()
    var n = len(all_bytes)
    print("corpus:", path, "(", n, "bytes ) cluster, item-count sweep")
    print("")
    var counts = List[Int]()
    counts.append(1)
    counts.append(16)
    counts.append(64)
    counts.append(256)
    counts.append(1024)
    counts.append(4096)
    for ci in range(len(counts)):
        var k = counts[ci]
        var per = n // k
        if per < 1024:
            # Below ~1 KB/item it's a pathological case, off the realistic end.
            continue
        var items = List[Item]()
        for i in range(k):
            var it = Item()
            it.byte_start = i * per
            it.byte_count = per if i + 1 < k else n - i * per
            it.line_height = 1
            it.cluster_mode = CLUSTER_CLUSTER
            items.append(it^)
        print("── items:", k, "( ~", per, "B/item )")
        bench_modes(trie, all_bytes, items, ctx, profile)


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
        bench_scaling(seed.trie, String(args[3]), ctx, profile=len(args) > 4 and String(args[4]) == "profile")
        return
    # --bench-cluster <corpus>: the same sweep under cluster mode against the
    # REAL atlas trie (the full 4,166-sequence table), so dense emoji content
    # resolves for real instead of probing a fixture's synthetic table.
    if String(args[1]) == "--bench-cluster":
        var real_trie = load_trie_blob(String(args[2]))
        bench_scaling(real_trie, String(args[3]), ctx, cluster=True, profile=len(args) > 4 and String(args[4]) == "profile")
        return
    # --bench-items <trie> <corpus> [profile]: the item-count sweep — the A/B
    # the device-side item search is judged by (see bench_items' docstring).
    if String(args[1]) == "--bench-items":
        var real_trie = load_trie_blob(String(args[2]))
        bench_items(real_trie, String(args[3]), ctx, profile=len(args) > 4 and String(args[4]) == "profile")
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
        print("gpu pipeline: the device chain, statics modes 0+2+3 (+mode 3 chunked chain) — counts exact, statics bit-exact, positions within 1e-4")
    else:
        raise Error("gpu pipeline diverged")
