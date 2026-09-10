#!/usr/bin/env python3
"""gen_emoji_sheet.py — bake the vendored colour-emoji font into one static sheet.

Reads tools/vendor/third-party/noto-emoji/NotoColorEmoji.ttf through
tools/emoji_font.py and writes assets/atlas/emoji-sheet.bin — a 'G3ES' blob
(byte-level format: assets/atlas/FORMAT.md). The renderer loads it the way it
loads the curve bins: a path, and a flag to point at a different one.

WHAT THE SHEET IS. The font's PNG bytes VERBATIM, one cell per bitmap glyph,
plus the tables that name them: cell -> (layer, x, y) placement, codepoint ->
glyph, and sequence -> glyph. No pixel is decoded, resampled or re-encoded
here, so the file is byte-exact on every platform and rebuild-and-compare
(`--check`, run by the battery's committed-artifacts gate) is a check that can
fail. Decoding is the renderer's job at load; downscaling is the GPU's through
mipmaps.

WHAT IT IS KEYED BY. The font's own glyph id, and every bitmap the font has
gets a cell — including the ~2,500 that only a codepoint SEQUENCE reaches.
Today's pipeline is one glyph per leader byte and can address the 1,460
single-codepoint ones through the codepoint table; the sequence table is
carried so the shaping pass that does not exist yet finds its cells already
placed. Decided 2026-09-10 (out/EMOJI.md); the price is unused cells.

PLACEMENT is a pure function of the cell's index in glyph-id order: cells
fill rows of `cols`, rows fill layers of `rows_per_layer`. The layer count is
the smallest that keeps a layer's height inside MAX_LAYER_PX (8192, the
single-dimension limit Metal and older Vulkan devices impose), and rows are
spread evenly across layers so no layer is mostly padding: 3,985 cells of
136x128 in 60 columns is 67 rows, which is 2 layers of 34 rows (8160x4352,
2.6% slack) rather than 2 of 64 (48% slack). The renderer reads the layer
geometry from the header; it derives nothing.

Run:  pixi run gen-emoji-sheet            write assets/atlas/emoji-sheet.bin
      pixi run gen-emoji-sheet --check    rebuild in memory, byte-compare, assert
                                          structure; write nothing; exit 1 on drift
"""

from __future__ import annotations

import struct
import sys
from pathlib import Path

import emoji_font

ROOT = emoji_font.ROOT
OUT = ROOT / "assets/atlas/emoji-sheet.bin"

MAGIC = 0x53453347          # 'G3ES' little-endian bytes 47 33 45 53
VERSION = 1
HEADER_WORDS = 40           # 160 bytes; words 26-31 reserved (zero), 32-39 the font sha256
MAX_LAYER_PX = 8192         # a layer's width and height stay within this
SHEET_WIDTH_PX = 8192       # cells per row = SHEET_WIDTH_PX // cell_w
SEQ_MAX = 9                 # longest GSUB sequence in the font, asserted
SEQ_STRIDE = 2 + SEQ_MAX    # [len, glyph, cp x SEQ_MAX] per sequence record
CELL_STRIDE = 6             # [glyph, layer, x, y, png_offset, png_len]
CP_STRIDE = 2               # [codepoint, glyph]


def build(ef: emoji_font.EmojiFont) -> bytes:
    cells = [ef.cells[g] for g in sorted(ef.cells)]
    if not cells:
        raise SystemExit("the font has no bitmaps")
    cw, ch = cells[0].png_size
    for c in cells:
        if c.png_size != (cw, ch) or (c.width, c.height) != (cw, ch):
            raise SystemExit(f"glyph {c.glyph_id} ({c.name}): {c.png_size} is not the "
                             f"uniform cell {cw}x{ch} — the format assumes one size")
    bx, by, adv = cells[0].bearing_x, cells[0].bearing_y, cells[0].advance
    for c in cells:
        if (c.bearing_x, c.bearing_y, c.advance) != (bx, by, adv):
            raise SystemExit(f"glyph {c.glyph_id} ({c.name}): bearings differ; the header "
                             "carries one set for every cell")

    cols = SHEET_WIDTH_PX // cw
    rows_total = -(-len(cells) // cols)
    max_rows = MAX_LAYER_PX // ch
    layers = -(-rows_total // max_rows)
    rows_per_layer = -(-rows_total // layers)
    per_layer = cols * rows_per_layer

    # ── cell table + PNG blob ────────────────────────────────────────────
    cell_words: list[int] = []
    blob = bytearray()
    for i, c in enumerate(cells):
        layer, r = divmod(i, per_layer)
        row, col = divmod(r, cols)
        cell_words += [c.glyph_id, layer, col * cw, row * ch, len(blob), len(c.png)]
        blob += c.png

    # ── codepoint table: every cmap entry, sorted ────────────────────────
    cp_words: list[int] = []
    for cp, g in sorted(ef.cmap.items()):
        cp_words += [cp, g]

    # ── sequence table: fixed stride, sorted by sequence ─────────────────
    seq_words: list[int] = []
    for l in ef.ligatures:  # already sorted by emoji_font
        if len(l.codepoints) > SEQ_MAX:
            raise SystemExit(f"sequence {l.codepoints} is longer than SEQ_MAX={SEQ_MAX}")
        seq_words += [len(l.codepoints), l.glyph_id, *l.codepoints,
                      *([0] * (SEQ_MAX - len(l.codepoints)))]

    # ── name table: cell glyph names, offsets + blob (debug/log use) ─────
    names = [c.name.encode() for c in cells]
    name_offsets: list[int] = []
    name_blob = bytearray()
    for n in names:
        name_offsets.append(len(name_blob))
        name_blob += n
    name_words = name_offsets + [len(name_blob)]
    name_blob_padded = bytes(name_blob) + b"\0" * (-len(name_blob) % 4)

    # ── header ───────────────────────────────────────────────────────────
    sha_words = list(struct.unpack("<8I", bytes.fromhex(ef.sha256)))
    tables_bytes = 4 * (len(cell_words) + len(cp_words) + len(seq_words) + len(name_words))
    png_start = HEADER_WORDS * 4 + tables_bytes + len(name_blob_padded)
    hdr = [
        MAGIC, VERSION, HEADER_WORDS * 4,
        cw, ch, len(cells),
        cols, rows_per_layer, layers, cols * cw, rows_per_layer * ch,
        ef.ppem, ef.strike_ascender & 0xFFFFFFFF, ef.strike_descender & 0xFFFFFFFF,
        bx & 0xFFFFFFFF, by & 0xFFFFFFFF, adv,
        ef.upem, ef.ascender & 0xFFFFFFFF, ef.descender & 0xFFFFFFFF, ef.num_glyphs,
        len(ef.cmap), len(ef.ligatures), SEQ_MAX,
        png_start, len(blob),
    ]
    hdr += [0] * (HEADER_WORDS - 8 - len(hdr))
    hdr += sha_words
    assert len(hdr) == HEADER_WORDS
    out = bytearray(struct.pack(f"<{HEADER_WORDS}I", *hdr))
    out += struct.pack(f"<{len(cell_words)}I", *cell_words)
    out += struct.pack(f"<{len(cp_words)}I", *cp_words)
    out += struct.pack(f"<{len(seq_words)}I", *seq_words)
    out += struct.pack(f"<{len(name_words)}I", *name_words)
    out += name_blob_padded
    assert len(out) == png_start
    out += blob
    return bytes(out)


def parse_and_assert(data: bytes) -> dict:
    """Read the blob back with no help from the writer and assert the
    invariants the renderer will rely on. Independent of build()'s variables
    on purpose: a writer that mis-sizes a table must be caught here."""
    fail: list[str] = []

    def check(cond: bool, msg: str) -> None:
        if not cond:
            fail.append(msg)

    hdr = struct.unpack(f"<{HEADER_WORDS}I", data[: HEADER_WORDS * 4])
    check(hdr[0] == MAGIC, "bad magic")
    check(hdr[1] == VERSION, "bad version")
    check(hdr[2] == HEADER_WORDS * 4, "bad header size")
    cw, ch, n = hdr[3], hdr[4], hdr[5]
    cols, rows, layers, lw, lh = hdr[6], hdr[7], hdr[8], hdr[9], hdr[10]
    n_cp, n_seq, seq_max, png_start, png_len = hdr[21], hdr[22], hdr[23], hdr[24], hdr[25]
    check(lw == cols * cw and lh == rows * ch, "layer geometry disagrees with its factors")
    check(lw <= MAX_LAYER_PX and lh <= MAX_LAYER_PX, f"a layer exceeds {MAX_LAYER_PX} px")
    check(cols * rows * layers >= n, "layers cannot hold every cell")
    check(cols * rows * (layers - 1) < n, "a whole layer would be empty")
    check(seq_max == SEQ_MAX, "sequence stride disagrees with this reader")
    check(png_start + png_len == len(data), "PNG blob does not end the file")

    at = HEADER_WORDS * 4
    cells = struct.unpack(f"<{n * CELL_STRIDE}I", data[at: at + n * CELL_STRIDE * 4]); at += n * CELL_STRIDE * 4
    cps = struct.unpack(f"<{n_cp * CP_STRIDE}I", data[at: at + n_cp * CP_STRIDE * 4]); at += n_cp * CP_STRIDE * 4
    seqs = struct.unpack(f"<{n_seq * SEQ_STRIDE}I", data[at: at + n_seq * SEQ_STRIDE * 4]); at += n_seq * SEQ_STRIDE * 4
    name_off = struct.unpack(f"<{n + 1}I", data[at: at + (n + 1) * 4]); at += (n + 1) * 4
    name_blob = data[at: at + name_off[-1]]

    glyph_of_cell: dict[int, int] = {}
    seen_pos: set[tuple[int, int, int]] = set()
    last_g = -1
    png = data[png_start:]
    for i in range(n):
        g, layer, x, y, off, ln = cells[i * CELL_STRIDE:(i + 1) * CELL_STRIDE]
        check(g > last_g, f"cell {i}: glyph ids not strictly ascending"); last_g = g
        check(layer < layers and x + cw <= lw and y + ch <= lh, f"cell {i}: outside its layer")
        check(x % cw == 0 and y % ch == 0, f"cell {i}: not on the cell grid")
        check((layer, x, y) not in seen_pos, f"cell {i}: placement collides"); seen_pos.add((layer, x, y))
        check(off + ln <= png_len, f"cell {i}: PNG range outside the blob")
        p = png[off: off + ln]
        check(p[:8] == emoji_font.PNG_MAGIC, f"cell {i}: not a PNG")
        check(struct.unpack(">II", p[16:24]) == (cw, ch), f"cell {i}: PNG is not {cw}x{ch}")
        check(name_off[i] < name_off[i + 1] <= len(name_blob), f"cell {i}: bad name span")
        glyph_of_cell[g] = i
    check(sum(cells[i * CELL_STRIDE + 5] for i in range(n)) == png_len, "cell PNG lengths do not sum to the blob")

    num_glyphs = hdr[20]
    last_cp = -1
    single = 0
    for i in range(n_cp):
        cp, g = cps[i * 2], cps[i * 2 + 1]
        check(cp > last_cp, "codepoints not strictly ascending"); last_cp = cp
        check(g < num_glyphs, f"codepoint U+{cp:04X}: glyph {g} out of range")
        single += g in glyph_of_cell
    last_seq: tuple[int, ...] = ()
    for i in range(n_seq):
        rec = seqs[i * SEQ_STRIDE:(i + 1) * SEQ_STRIDE]
        ln, g, cpl = rec[0], rec[1], rec[2:2 + rec[0]]
        check(2 <= ln <= seq_max, f"sequence {i}: length {ln}")
        check(all(v == 0 for v in rec[2 + ln:]), f"sequence {i}: padding not zero")
        check(cpl > last_seq, f"sequence {i}: not sorted"); last_seq = cpl
        check(g in glyph_of_cell, f"sequence {i}: target glyph {g} has no cell")
    if fail:
        raise SystemExit("FAIL  emoji-sheet.bin structure:\n" + "".join(f"        {f}\n" for f in fail[:12]))
    return dict(cells=n, cell=(cw, ch), layers=layers, layer=(lw, lh), single=single,
                sequences=n_seq, png_bytes=png_len, total=len(data))


def main() -> int:
    check_only = "--check" in sys.argv[1:]
    ef = emoji_font.load()
    data = build(ef)
    info = parse_and_assert(data)
    summary = (f"emoji-sheet.bin: {info['cells']} cells of {info['cell'][0]}x{info['cell'][1]} in "
               f"{info['layers']} layer(s) of {info['layer'][0]}x{info['layer'][1]}; "
               f"{info['single']} single-codepoint, {info['sequences']} sequences; "
               f"{info['png_bytes']:,} PNG bytes, {info['total']:,} total")
    if check_only:
        if not OUT.exists():
            raise SystemExit(f"FAIL  {OUT.relative_to(ROOT)} is missing — run: pixi run gen-emoji-sheet")
        on_disk = OUT.read_bytes()
        if on_disk != data:
            raise SystemExit(f"FAIL  {OUT.relative_to(ROOT)} differs from a fresh bake "
                             f"({len(on_disk):,} bytes on disk, {len(data):,} rebuilt) — "
                             "the font, the generator or the committed file moved")
        print(f"[check] {summary} — BYTE-IDENTICAL to the committed file")
        return 0
    OUT.write_bytes(data)
    print(f"[write] {summary}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
