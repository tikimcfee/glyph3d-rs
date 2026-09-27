# gpu_monoid.mojo — the scan monoid, device-side: the element `E` and its
# `combine`, the partial-lane load/store, the one-byte leaf, the ordered-key
# float<->u32 atomic helpers, and the device wrap rules the kernels evaluate
# per thread. Extracted from gpu_pipeline.mojo in the 2026-09 code-shape
# refactor (a pure move). The monoid lives in ONE place so no two scan
# dispatches can drift from each other.
#
# NOTE the wrap rules here are the DEVICE's own copies (rows_for, not
# glyph_wrap's rows_for_line): unifying them is a separate, deliberate
# question — the chain's bit-exactness proof is against THESE.

from std.memory import bitcast
from glyph_schema import (
    SM_STRIDE, SM_ADVANCE,
    PARTIAL_COUNT_STRIDE, PARTIAL_MEASURE_STRIDE,
    P_RESET, P_NL, P_GLYPHS, P_ROWS, P_HEAD_LEN, P_TAIL_LEN, P_WRAP, P_MODE, PM_TAIL_ADV,
)
from glyph_pipeline import F_LEADER, F_NEWLINE
from glyph_wrap import WRAP_DOWN, WRAP_BACK

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


comptime CHUNK = 64
comptime GROUP = 256


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
