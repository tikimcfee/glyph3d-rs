# glyph_wrap.mojo — the wrap modes and their three scalar rules (rows_for_line,
# wrap_segment_of, wrap_row_of), extracted from glyph_pipeline.mojo in the
# 2026-09 code-shape refactor (a pure move). The rules are mode-parameterized
# SCALARS — no Slots, no Item — so the fold, the scan monoid and the GPU chain
# all share exactly one wrap arithmetic.

# ── THE WRAP MODES ───────────────────────────────────────────────────────────
# An ITEM-LEVEL parameter, never per line and never per range.
#
#   WRAP_DOWN  a wrap advances the visual ROW. A line of n cells occupies
#              ceil(n / wrap) rows. The original behaviour and the default.
#   WRAP_BACK  a wrap does NOT advance the row. Every wrap segment of a line
#              shares ONE row and the segments stack in DEPTH, each z_step
#              further back, so a line's row is just its line index.
#
# `col` still counts within the LOGICAL line in both modes, `seg_adv` still
# resets at every fold boundary (each segment starts at x = 0), and the wrap
# SEGMENT index still exists in both — under WRAP_BACK it feeds z and no longer
# feeds row. Picking by (row, col) still resolves uniquely because col differs
# between segments.
#
# WHY IT MUST STAY ITEM-LEVEL: glyph_bake.scan_combine's junction term evaluates
# rows_for_line with `b`'s parameters, so it is not associative across a change of
# them. Mode joins wrap in that term, which makes the non-associative surface
# WIDER, not narrower. What keeps the scan form safe is structural and unchanged:
# an item boundary emits a resetting leaf, so no interval without a reset spans two
# items.
comptime WRAP_DOWN: Int = 0
comptime WRAP_BACK: Int = 1


def rows_for_line(length: Int, wrap: Int, mode: Int = WRAP_DOWN) -> Int:
    """Visual rows a line of `length` cells occupies under `wrap` and `mode`.

    Under WRAP_DOWN a CEILING with a floor of one, since an empty line still
    occupies the row it sits on. Under WRAP_BACK it is ONE for every line, whatever
    the length — that identity IS the mode: the folds go into depth, and depth
    costs no rows.

    THE PHANTOM ROW (corrected 2026-09-04). The WrapDown rule was `length // wrap
    + 1`, which counts the row the terminating newline rides on. The newline rides
    at column `length`, so when `wrap` divides `length` that column rolled onto a
    fresh row holding nothing else: the line claimed a blank row and every later
    line moved down one. The two rules agree at every other length, which is why
    the defect was invisible except at exact multiples."""
    if mode == WRAP_BACK:
        return 1
    if wrap <= 0 or length <= 0:
        return 1
    return (length - 1) // wrap + 1


def wrap_segment_of(col: Int, wrap: Int, terminator: Bool) -> Int:
    """The WRAP SEGMENT index of a cell at column `col` — how many times its line
    has already folded before reaching it. MODE-FREE: this is the DEPTH fan's index
    and it exists in both modes; only its contribution to the ROW is a mode
    question (wrap_row_of).

    An ordinary glyph at column `col` sits in segment `col // wrap`. A NEWLINE is a
    terminator riding at one-past-the-last cell (`col` == the line's glyph count),
    so at an exact multiple `col // wrap` would roll it into a segment that holds
    nothing else; it belongs to the last segment its line reaches.

    Every consumer of (col, wrap) -> segment goes through here. Deriving both cases
    from one expression is what let the terminator open a phantom row."""
    if wrap <= 0:
        return 0
    if terminator:
        # `rows_for_line(col, wrap, WRAP_DOWN) - 1`, written out so the segment
        # index cannot pick up a mode through the helper it used to borrow.
        if col <= 0:
            return 0
        return (col - 1) // wrap
    return col // wrap


def wrap_row_of(col: Int, wrap: Int, terminator: Bool, mode: Int = WRAP_DOWN) -> Int:
    """The LINE-LOCAL ROW CONTRIBUTION of a cell at column `col`.

    WRAP_DOWN delegates to wrap_segment_of — which is the whole of the default's
    proof: mode A's row IS the segment index, byte for byte, as it was before modes
    existed. WRAP_BACK contributes ZERO, because a wrap does not advance the row at
    all in that mode; it steps in z instead."""
    if mode == WRAP_BACK:
        return 0
    return wrap_segment_of(col, wrap, terminator)
