#!/usr/bin/env python3
"""Stage G pick oracle: replicate the engine fold (glyph_pipeline.mojo THE
FOLD) over raw file bytes and answer (file, row, col) -> (char, byte_off,
line). Used to assert the native binary's pick output independently.

fold conventions: col = raw leader count within the source line;
row = base_row + wrap_row_of(col, wrap, is_newline); a newline rides at
col == line length but on the row it CLOSES, and a line covers
rows_for_line(len, wrap) = ceil(len / wrap) rows, floored at one.

This is an INDEPENDENT oracle — it must state the rule itself rather than
call into the engine, which is the whole reason gate 7 is worth running. It
is not, however, licensed to state a DIFFERENT rule: the 2026-09-04
phantom-row correction is transcribed here deliberately.
"""
import sys


def rows_for_line(length: int, wrap: int) -> int:
    """Visual rows a line of `length` cells occupies — ceiling, floored at one."""
    if wrap <= 0 or length <= 0:
        return 1
    return (length - 1) // wrap + 1


def wrap_row_of(col: int, wrap: int, terminator: bool) -> int:
    """Line-local row of a cell. A newline is a terminator at one-past-the-last
    cell, so at an exact wrap multiple it stays on the row it closes."""
    if wrap <= 0:
        return 0
    if terminator:
        return rows_for_line(col, wrap) - 1
    return col // wrap


def fold_leaders(data: bytes, wrap: int):
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
        row = base_row + wrap_row_of(col, wrap, is_newline)
        out.append((row, col, line, i, cp))
        if is_newline:
            base_row += rows_for_line(col, wrap)
            col = 0
            line += 1
        else:
            col += 1
        i += 1
    return out


def resolve(path: str, row: int, col: int, wrap: int = 100):
    data = open(path, "rb").read()
    for (r, c, line, off, cp) in fold_leaders(data, wrap):
        if r == row and c == col:
            ch = chr(cp) if cp <= 0x10FFFF else ""
            return ch, off, line
    return None


if __name__ == "__main__":
    # args: file row col [row col ...]
    path = sys.argv[1]
    for i in range(2, len(sys.argv), 2):
        row, col = int(sys.argv[i]), int(sys.argv[i + 1])
        got = resolve(path, row, col)
        if got is None:
            print(f"{path} row={row} col={col} -> NO RECORD")
        else:
            ch, off, line = got
            print(f"{path} row={row} col={col} -> char={ch!r} byte={off} line={line}")
