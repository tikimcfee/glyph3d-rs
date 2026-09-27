# gpu_kernels.mojo — the device scan chain's kernels: ItemWalk (the register
# merge walk over item facts), the chunk/spine/partial/apply scan passes, and
# resolve_x / init_zeros / derive_stride / paginate. Extracted from
# gpu_pipeline.mojo in the 2026-09 code-shape refactor (a pure move); the
# bench driver there enqueues these.

from max.gpu import global_idx
from std.atomic import Atomic
from glyph_schema import (
    SM_STRIDE, SM_ADVANCE, SM_HEIGHT,
    LM_STRIDE, LM_X, LM_Y, LM_Z, LM_BASE_X,
    LC_STRIDE, LC_ROW, LC_COL,
    IM_STRIDE, IM_ORIGIN_X, IM_ORIGIN_Y, IM_ORIGIN_Z, IM_LINE_HEIGHT,
    IM_Z_STEP, IM_BAND_STRIDE_Y, IM_DEPTH_PER_BAND, IM_DEPTH_PER_COL,
    IM_PAGE_STRIDE_X,
    IE_STRIDE, IE_PAGE_ROWS, IE_PAGE_COLS, IE_SCROLL_ROWS, IE_PAGES_WIDE,
    IE_WRAP_WIDTH, IE_WRAP_MODE, IE_HAS_PAGE,
    PARTIAL_COUNT_STRIDE, PARTIAL_MEASURE_STRIDE,
)
from glyph_pipeline import F_LEADER, F_NEWLINE, item_search_device
from glyph_wrap import WRAP_DOWN, WRAP_BACK
from gpu_monoid import (
    E, combine, p_load, p_store, leaf_of, ordered_key, key_to_float,
    rows_for, wrap_segment_of, wrap_row_of,
    CHUNK, GROUP,
)

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


