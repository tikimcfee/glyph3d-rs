#!/usr/bin/env python3
"""emoji_inventory.py — what the vendored colour-emoji font holds.

The step-one instrument for the emoji line of work (out/EMOJI.md): walk the
font, print the numbers that decide the sheet's shape and the texture budget,
and assert the two things the sheet format will rest on — one strike, and
every bitmap the same pixel size. It prints and asserts; it builds nothing.

Run:  pixi run emoji-inventory
"""

from __future__ import annotations

from collections import Counter

import emoji_font


def main() -> int:
    ef = emoji_font.load()
    print(f"font      {ef.path.relative_to(emoji_font.ROOT)}")
    print(f"sha256    {ef.sha256}")
    print(f"version   {ef.version}")
    print(f"upem {ef.upem}  hhea asc/desc {ef.ascender}/{ef.descender}  glyphs {ef.num_glyphs}")
    print(f"strike    {ef.ppem} ppem, asc/desc {ef.strike_ascender}/{ef.strike_descender} px")

    cells = ef.cells
    sizes = Counter(c.png_size for c in cells.values())
    metrics = Counter((c.width, c.height) for c in cells.values())
    total = sum(len(c.png) for c in cells.values())
    print(f"\nbitmaps   {len(cells)} glyphs, CBDT format 17, PNG bytes {total:,} "
          f"(mean {total // len(cells):,}, max {max(len(c.png) for c in cells.values()):,})")
    print(f"png sizes {dict(sizes)}   metric sizes {dict(metrics)}")
    for c in cells.values():
        if c.png_size != (c.width, c.height):
            print(f"FAIL  glyph {c.glyph_id} ({c.name}): PNG {c.png_size} != metrics {(c.width, c.height)}")
            return 1
    if len(sizes) != 1:
        print("FAIL  more than one bitmap size; the sheet format assumes a uniform cell")
        return 1
    (w, h), = sizes
    bearings = Counter((c.bearing_x, c.bearing_y, c.advance) for c in cells.values())
    print(f"bearings  (x, y, advance) over all cells: {dict(bearings)}")

    single = ef.single_codepoint_cells()
    seq_only = ef.sequence_only_cells()
    no_bitmap = sorted(cp for cp, g in ef.cmap.items() if g not in cells)
    print(f"\ncmap      {len(ef.cmap)} codepoints; {len(single)} reach a bitmap, "
          f"{len(no_bitmap)} do not (structural: NUL, CR, space, ZWJ, the tag characters)")
    print(f"          without bitmap: {' '.join(f'U+{cp:04X}' for cp in no_bitmap[:6])} … "
          f"{' '.join(f'U+{cp:04X}' for cp in no_bitmap[-2:])}")
    print(f"          single-codepoint bitmaps >= U+1F000: "
          f"{sum(1 for cp in single if cp >= 0x1F000)}, below: {sum(1 for cp in single if cp < 0x1F000)}")
    lens = Counter(len(l.codepoints) for l in ef.ligatures)
    targets = {l.glyph_id for l in ef.ligatures}
    print(f"\nsequences {len(ef.ligatures)} GSUB ligature rules -> {len(targets)} glyphs; "
          f"lengths {dict(sorted(lens.items()))}")
    print(f"          bitmaps reachable only by a sequence: {len(seq_only)}; "
          f"by either: {len(cells) - len(single) - len(seq_only) == 0 and 'all' or 'NOT ALL'} "
          f"({len(cells)} = {len(single)} single + {len(seq_only)} sequence-only)")
    unreachable = set(cells) - set(single.values()) - targets
    if unreachable:
        print(f"FAIL  {len(unreachable)} bitmaps reachable by neither cmap nor ligature")
        return 1

    # The web bake's only emoji range; how much of it this font draws.
    web_lo, web_hi = 0x1F400, 0x1F64F
    covered = sum(1 for cp in single if web_lo <= cp <= web_hi)
    print(f"\nweb range U+{web_lo:04X}..U+{web_hi:04X}: {covered} of {web_hi - web_lo + 1} "
          f"codepoints have a bitmap here")

    px = len(cells) * w * h
    print(f"\ntexture   every cell as RGBA8: {len(cells)} x {w}x{h} = {px * 4 / 2**20:,.0f} MiB "
          f"base level, ~{px * 4 * 4 / 3 / 2**20:,.0f} MiB with mipmaps; "
          f"{len(cells)} cells of {w}x{h} tile an 8192-wide sheet in "
          f"{-(-len(cells) // (8192 // w))} rows = {-(-len(cells) // (8192 // w)) * h:,} px tall")
    print("\nemoji-inventory: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
