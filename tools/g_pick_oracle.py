#!/usr/bin/env python3
"""Pick oracle: an independent Python implementation of the layout fold (the
rule native/src/fold.rs implements; this file shares no code with it) over raw
file bytes, answering (file, row, col) -> (char, byte_off,
line). Used to assert the native binary's pick output independently.

fold conventions: col = raw leader count within the source line;
row = base_row + wrap_row_of(col, wrap, is_newline); a newline rides at
col == line length but on the row it CLOSES, and a line covers
rows_for_line(len, wrap) = ceil(len / wrap) rows, floored at one.

This is an INDEPENDENT oracle — it must state the rule itself rather than
call into the engine, which is the whole reason the pick-oracle gate is
worth running. It
is not, however, licensed to state a DIFFERENT rule: the 2026-09-04
phantom-row correction is transcribed here deliberately.

CLUSTER MODE (2026-09-20): deliberately NOT implemented here. The sequence
pass moves glyph ids and advances, never ROW/COL — col still counts leaders
and a trailer keeps its record — so this oracle's answers are identical in
both modes by construction. What a pixel through a cluster cell resolves to
(the head's char) is covered by the renderer's pick probes in
check-pick-oracle.sh, not by this file.
"""
import sys


def rows_for_line(length: int, wrap: int, mode: str = "down") -> int:
    """Visual rows a line of `length` cells occupies — ceiling, floored at one.

    Under `back` a wrap costs DEPTH rather than a row, so however far a line
    runs it occupies exactly one row. Written from that rule, not transcribed
    from the renderer: this file is an independent implementation and its worth
    depends on staying one."""
    if mode == "back":
        return 1
    if wrap <= 0 or length <= 0:
        return 1
    return (length - 1) // wrap + 1


def wrap_row_of(col: int, wrap: int, terminator: bool, mode: str = "down") -> int:
    """Line-local row of a cell. A newline is a terminator at one-past-the-last
    cell, so at an exact wrap multiple it stays on the row it closes.

    Under `back` every cell of a line shares that line's row, whatever its
    column — the wrap moved it in z, not in y."""
    if mode == "back":
        return 0
    if wrap <= 0:
        return 0
    if terminator:
        return rows_for_line(col, wrap) - 1
    return col // wrap


def fold_leaders(data: bytes, wrap: int, mode: str = "down"):
    out = []  # (row, col, line, byte_off, codepoint)
    base_row = col = line = 0
    i = 0
    n_bytes = len(data)
    while i < n_bytes:
        b0 = data[i]
        if b0 & 0x80 == 0x00:
            n = 1
        elif b0 & 0xE0 == 0xC0:
            n = 2
        elif b0 & 0xF0 == 0xE0:
            n = 3
        elif b0 & 0xF8 == 0xF0:
            n = 4
        else:
            i += 1
            continue
        def at(k):
            return data[i + k] if i + k < n_bytes else 0
        if n == 1:
            cp = b0
        elif n == 2:
            cp = ((b0 & 0x1F) << 6) | (at(1) & 0x3F)
        elif n == 3:
            cp = ((b0 & 0x0F) << 12) | ((at(1) & 0x3F) << 6) | (at(2) & 0x3F)
        else:
            cp = ((b0 & 0x07) << 18) | ((at(1) & 0x3F) << 12) | ((at(2) & 0x3F) << 6) | (at(3) & 0x3F)
        is_newline = cp == 0x0A
        row = base_row + wrap_row_of(col, wrap, is_newline, mode)
        out.append((row, col, line, i, cp))
        if is_newline:
            base_row += rows_for_line(col, wrap, mode)
            col = 0
            line += 1
        else:
            col += 1
        i += 1
    return out


def resolve(path: str, row: int, col: int, wrap: int = 100, mode: str = "down"):
    data = open(path, "rb").read()
    for (r, c, line, off, cp) in fold_leaders(data, wrap, mode):
        if r == row and c == col:
            ch = chr(cp) if cp <= 0x10FFFF else ""
            return ch, off, line
    return None


if __name__ == "__main__":
    # args: [--mode down|back] file row col [row col ...]
    argv = sys.argv[1:]
    mode = "down"
    if argv and argv[0] == "--mode":
        mode = argv[1]
        argv = argv[2:]
        if mode not in ("down", "back"):
            raise SystemExit(f"unknown wrap mode {mode!r}")
    path = argv[0]
    for i in range(1, len(argv), 2):
        row, col = int(argv[i]), int(argv[i + 1])
        got = resolve(path, row, col, mode=mode)
        if got is None:
            print(f"{path} row={row} col={col} -> NO RECORD")
        else:
            ch, off, line = got
            print(f"{path} row={row} col={col} -> char={ch!r} byte={off} line={line}")
