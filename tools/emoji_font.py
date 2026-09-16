#!/usr/bin/env python3
"""emoji_font.py — walk a CBDT/CBLC colour-emoji font into plain data.

The one place the tree reads the vendored Noto Color Emoji
(tools/vendor/third-party/noto-emoji/NotoColorEmoji.ttf). Two consumers:
`emoji_inventory.py` prints what the font holds; `gen_emoji_sheet.py` bakes
it into assets/atlas/emoji-sheet.bin. Both see the same walk, so the numbers
the inventory prints are the numbers the sheet is built from.

WHAT IS READ, AND WHAT IS NOT. Tables and bytes only: the CBLC strike, each
CBDT glyph's small metrics and its PNG payload VERBATIM (format 17), the cmap,
and the GSUB ligature rules. Nothing is decoded, resampled or re-encoded here
— the PNG bytes that leave this module are the PNG bytes in the font, which is
what makes the sheet byte-exact on every platform and rebuild-and-compare a
meaningful check. Decoding is the renderer's job at load; downscaling is the
GPU's job through mipmaps.

IDENTITY IS THE FONT'S GLYPH ID. Every bitmap in the font becomes a cell,
keyed by its glyph index in the font's own glyph order — including the ~2,500
that only a codepoint SEQUENCE can reach (flags, skin tones, ZWJ families).
The pipeline today is one glyph per leader byte and can address only the
single-codepoint ones through the cmap; when sequence shaping arrives it will
produce a glyph id, and the cell for it is already in the sheet. Unused cells
are the price, and it was chosen on purpose (out/EMOJI.md).

Deterministic by construction: cells in glyph-order index, codepoints sorted,
ligature rules sorted by their component sequence. fontTools' version cannot
change any byte that leaves here.
"""

from __future__ import annotations

import hashlib
import struct
from dataclasses import dataclass
from pathlib import Path

from fontTools.ttLib import TTFont

ROOT = Path(__file__).resolve().parent.parent
FONT = ROOT / "tools/vendor/third-party/noto-emoji/NotoColorEmoji.ttf"

PNG_MAGIC = b"\x89PNG\r\n\x1a\n"


@dataclass(frozen=True)
class Cell:
    """One bitmap glyph: the font's glyph id, its PNG bytes, and the strike's
    small metrics for it (pixels at the strike's ppem)."""

    glyph_id: int
    name: str
    png: bytes
    width: int
    height: int
    bearing_x: int
    bearing_y: int
    advance: int

    @property
    def png_size(self) -> tuple[int, int]:
        """(w, h) from the IHDR — the pixel size of the image itself, which
        the small metrics also state; the two agree for every glyph and the
        inventory asserts it."""
        if self.png[:8] != PNG_MAGIC:
            raise ValueError(f"glyph {self.glyph_id} ({self.name}): not a PNG")
        return struct.unpack(">II", self.png[16:24])


@dataclass(frozen=True)
class Ligature:
    """One GSUB ligature rule: this codepoint sequence shapes to this glyph.
    Components are the FULL sequence (first + rest), as codepoints."""

    codepoints: tuple[int, ...]
    glyph_id: int


@dataclass
class EmojiFont:
    path: Path
    sha256: str
    version: str
    upem: int
    ascender: int
    descender: int
    num_glyphs: int
    ppem: int
    strike_ascender: int
    strike_descender: int
    glyph_order: list[str]
    cells: dict[int, Cell]          # glyph id -> cell, only glyphs with a bitmap
    cmap: dict[int, int]            # codepoint -> glyph id (every cmap entry)
    ligatures: list[Ligature]       # sorted by component sequence

    # ── derived views ────────────────────────────────────────────────────
    def single_codepoint_cells(self) -> dict[int, int]:
        """codepoint -> glyph id, for codepoints whose glyph has a bitmap."""
        return {cp: g for cp, g in sorted(self.cmap.items()) if g in self.cells}

    def sequence_only_cells(self) -> set[int]:
        """Glyph ids with a bitmap that no single codepoint reaches."""
        single = set(self.cmap.values())
        return {g for g in self.cells if g not in single}


def load(path: Path = FONT) -> EmojiFont:
    data = path.read_bytes()
    f = TTFont(path)
    order = f.getGlyphOrder()
    index = {n: i for i, n in enumerate(order)}
    strikes = f["CBLC"].strikes
    if len(strikes) != 1:
        raise SystemExit(f"{path.name}: expected exactly one CBLC strike, found {len(strikes)} — "
                         "the sheet format assumes one ppem; teach it strikes before baking this font")
    bst = strikes[0].bitmapSizeTable
    strike = f["CBDT"].strikeData[0]

    cells: dict[int, Cell] = {}
    for name, g in strike.items():
        fmt = g.getFormat()
        if fmt != 17:
            raise SystemExit(f"{path.name}: glyph {name} is CBDT format {fmt}; only 17 "
                             "(small metrics + PNG) is handled — a different format is a "
                             "different sheet")
        m = g.metrics
        cells[index[name]] = Cell(
            glyph_id=index[name], name=name, png=bytes(g.imageData),
            width=m.width, height=m.height, bearing_x=m.BearingX,
            bearing_y=m.BearingY, advance=m.Advance,
        )

    cmap = {cp: index[gn] for cp, gn in f.getBestCmap().items()}
    inverse: dict[int, int] = {}
    for cp, gid in sorted(cmap.items()):
        inverse.setdefault(gid, cp)  # first (lowest) codepoint names a glyph

    ligs: list[Ligature] = []
    for lookup in f["GSUB"].table.LookupList.Lookup:
        for st in lookup.SubTable:
            if st.LookupType == 7:  # extension — unwrap
                st = st.ExtSubTable
            if st.__class__.__name__ != "LigatureSubst":
                continue
            for first, rules in st.ligatures.items():
                for r in rules:
                    comps = [first, *r.Component]
                    try:
                        cps = tuple(inverse[index[c]] for c in comps)
                    except KeyError as e:
                        raise SystemExit(f"{path.name}: ligature component {e} has no codepoint; "
                                         "every component of an emoji sequence must be a codepoint")
                    ligs.append(Ligature(cps, index[r.LigGlyph]))
    ligs.sort(key=lambda l: l.codepoints)

    name = f["name"]
    return EmojiFont(
        path=path,
        sha256=hashlib.sha256(data).hexdigest(),
        version=name.getDebugName(5) or "",
        upem=f["head"].unitsPerEm,
        ascender=f["hhea"].ascent,
        descender=f["hhea"].descent,
        num_glyphs=f["maxp"].numGlyphs,
        ppem=bst.ppemX,
        strike_ascender=bst.hori.ascender,
        strike_descender=bst.hori.descender,
        glyph_order=order,
        cells=cells,
        cmap=cmap,
        ligatures=ligs,
    )
