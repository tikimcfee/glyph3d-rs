#!/usr/bin/env python3
"""
preview_glyphs.py — visual proof for the Stage B atlas export.

Decodes quadratic beziers straight from curves.bin + glyphmap.bin + codepoints.bin
(no other inputs), flattens them to polylines, and draws a few probe glyphs
('A', 'g', '@', '#', plus a Cousine 'W' and a fallback-glyph check) into
assets/atlas/preview.png. If the export is correct, these read as letters.

Coordinate conventions (see FORMAT.md): cell space is [0,1]², y-UP
(y = 0 at the font descender, 1 at the ascender), x = 0 at the pen origin,
1 at one advance width. uint16 texel value / 65535 → normalized coordinate.
"""
import struct
import sys
from pathlib import Path

# Managed-runtime plotting setup (see seaborn-visualization skill): derive the
# runtime root from the interpreter, then configure Agg + bundled fonts.
sys.path.insert(0, str(Path(sys.executable).parent.parent.parent))
from daimon_runtime import setup_plot

ATLAS = Path(__file__).resolve().parent.parent / "assets" / "atlas"


def read_u32(path):
    data = path.read_bytes()
    return struct.unpack(f"<{len(data)//4}I", data)


# ── load assets ───────────────────────────────────────────────────────────────
cw_raw = read_u32(ATLAS / "curves.bin")
CURVE_W, CURVE_H, CURVE_COUNT, TPC = cw_raw[3], cw_raw[4], cw_raw[5], cw_raw[6]
curve_tex = cw_raw[cw_raw[2] // 4:]

gm_raw = read_u32(ATLAS / "glyphmap.bin")
MAP_W, MAP_H, ENTRY_COUNT = gm_raw[3], gm_raw[4], gm_raw[5]
map_tex = gm_raw[gm_raw[2] // 4:]

cp_raw = read_u32(ATLAS / "codepoints.bin")
BLOCK_SHIFT, BI_LEN, BLOCK_COUNT, STRIDE = cp_raw[3], cp_raw[4], cp_raw[5], cp_raw[6]
o = cp_raw[2] // 4
block_index = cp_raw[o:o + BI_LEN]
blocks = cp_raw[o + BI_LEN:]


def glyph_for(cp):
    b = block_index[cp >> BLOCK_SHIFT]
    e = ((b << BLOCK_SHIFT) | (cp & 0xFF)) * STRIDE
    return blocks[e], blocks[e + 1], blocks[e + 2], blocks[e + 3]


def curves_of(slot):
    s, c, mode, _ = map_tex[slot * 4:slot * 4 + 4]
    assert mode == 0 and c > 0
    out = []
    for i in range(c):
        t = (s + i) * TPC * 4
        p = [curve_tex[t + k] / 65535 for k in range(6)]
        out.append(((p[0], p[1]), (p[2], p[3]), (p[4], p[5])))
    return out


def flatten(quad, n=24):
    (x0, y0), (x1, y1), (x2, y2) = quad
    pts = []
    for i in range(n + 1):
        t = i / n
        mt = 1 - t
        pts.append((mt * mt * x0 + 2 * mt * t * x1 + t * t * x2,
                    mt * mt * y0 + 2 * mt * t * y1 + t * t * y2))
    return pts


import matplotlib.pyplot as plt

setup_plot()

probes = ["A", "g", "@", "#", "W", "Ω", "┌", "→"]
fig, axes = plt.subplots(2, 4, figsize=(12, 6.6))
for ax, ch in zip(axes.flat, probes):
    slot, adv, hgt, flags = glyph_for(ord(ch))
    quads = curves_of(slot)
    for q in quads:
        pts = flatten(q)
        ax.plot([p[0] for p in pts], [p[1] for p in pts], "k-", lw=1.2)
    ax.axhline(1.0, color="0.8", lw=0.5)   # ascender line (cell top)
    ax.axhline(0.0, color="0.8", lw=0.5)   # descender line (cell bottom)
    ax.axvline(0.0, color="0.8", lw=0.5)
    ax.axvline(1.0, color="0.8", lw=0.5)
    ax.set_xlim(-0.15, 1.15)
    ax.set_ylim(-0.15, 1.15)
    ax.set_aspect("equal")
    ax.set_title(f"{ch!r} → slot {slot} ({len(quads)} curves)", fontsize=10)
    ax.set_xticks([]); ax.set_yticks([])
fig.suptitle("glyph3d-native Stage B atlas — decoded from curves.bin/glyphmap.bin/codepoints.bin (y-up, [0,1] cell)", fontsize=11)
fig.tight_layout()
out = ATLAS / "preview.png"
fig.savefig(out, dpi=160, bbox_inches="tight")
print(f"wrote {out}")
