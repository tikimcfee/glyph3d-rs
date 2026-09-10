# Emoji — the record of the ask, and what each step measured

Started 2026-09-10. This is the note for a multi-part line of work (the
`out/STAGE_*` template is retired; this is the shape that replaced it): the
decisions as they were made, dated, and the numbers each step produced.
`AGENTS.md` and `assets/atlas/FORMAT.md` are the contract; this is why.

## The ask (2026-09-10)

Full colour-emoji rendering, on top of a renderer that was rebuilt from the
ground up since the web version. The web app drew emoji through Canvas2D at
runtime — the platform's emoji font, a non-deterministic raster — which is
exactly what this tree cannot have. What it CAN have is what it already has
for curves: a static, committed, byte-exact asset that the renderer loads,
and that any other binary source of texture data could later be dropped into.

## Decisions, and who made them

- **Source: Noto Color Emoji, vendored at a release tag.** [Claude proposed,
  Ivan agreed.] Open licence (OFL 1.1), embedded PNG bitmaps (CBDT), so the
  sheet is the font's own bytes verbatim — no rasterizer, no resampler, no
  encoder in the loop. Apple's emoji cannot be vendored; Twemoji is SVG and
  would put a rasterizer back in.
- **Every glyph in the font goes into the sheet, keyed by the font's glyph
  id.** [Ivan.] Including the ~2,500 that only a codepoint SEQUENCE reaches.
  The pipeline today is one glyph per leader byte and can address only the
  single-codepoint ones; when sequence shaping arrives it produces a glyph id,
  and the cell is already there. Unused cells are the price, accepted.
- **Range growth is append-only.** [Ivan's worry, resolved.] No deleting, no
  pools, no dynamic loads. The re-bake keeps the existing 4,431 slots
  byte-for-byte and APPENDS a bitmap slot per single-codepoint emoji the font
  covers. No existing id moves, so every text frame stays byte-equal.
- **Sequences are later, but thought about now.** [Both.] The trie can mark a
  sequence's trailing codepoints (ZWJ, VS16, skin modifiers, tags) as
  zero-advance blanks — the same non-leader mechanism a future shaping pass
  builds on. Not this pass.
- **The sheet is a static file with the same handle the engine trie has**
  (a path, a flag to override it). [Ivan.] That is the hook: swapping a
  sheet is a command-line act, editing is regenerating, and a dynamic cell is
  an append to the same table.

## Step 1 — source and inventory (2026-09-10) [measured]

Vendored `googlefonts/noto-emoji` at `v2.051` (tag → commit `6202fe7c`), the
font and its OFL text, under `tools/vendor/third-party/noto-emoji/`, pinned
by fetch hash in `tools/vendor-manifest.py` and checked by the `vendor-hashes`
gate. The distro's copy on the Linux box (`noto-fonts-emoji 2.051`) is
byte-identical to the tag's file. fontTools joined the pixi env for the walk
(`tools/emoji_font.py`; the inventory is `pixi run emoji-inventory`).

| | |
|---|---|
| version | 2.051 (noto-emoji 20250818, e92753bf) |
| glyphs | 4,027, of which **3,985 have a bitmap** |
| strike | one, 109 ppem; every bitmap is CBDT format 17 (small metrics + PNG) |
| cell | **136 × 128 px, every one**; bearing (0, 101), advance 136 px; hmtx advance 2550/2048 em, every one |
| PNG bytes | 10,543,900 total, mean 2,645, max 7,777 |
| cmap | 1,501 codepoints; **1,460 reach a bitmap** (1,277 at U+1F000+, 183 below: © ® ‼ ⁉ ☀ … the keycap bases 0-9 # *) |
| cmap, no bitmap | 41: NUL, CR, space, ZWJ, and the 37 tag characters U+E0030..E007F |
| sequences | **4,166 GSUB ligature rules → 2,546 glyphs**; lengths 2:938 3:406 4:1,750 5:103 6:150 7:629 9:190 codepoints |
| reachability | 1,460 single + **2,525 sequence-only** = 3,985; nothing unreachable |
| web's emoji range | U+1F400..1F64F: 471 of 592 codepoints have a bitmap here |
| texture, all cells RGBA8 | **265 MiB base, ~354 MiB with mipmaps**; 3,985 cells tile an 8192-wide sheet in 67 rows = 8,576 px |

Three things the numbers decide:

- **The sheet needs a texture array or two layers**, not one 2D texture: 8,576
  px exceeds the 8,192 limit that Metal and older Vulkan devices impose on a
  single dimension. Two 8192×4352 layers, or a 2D array of 128-px rows, is a
  design point for step 2's container (cell → layer, x, y).
- **~350 MiB of VRAM for the full set** is nothing on the 32 GiB card and worth
  measuring on the M2's 16 GiB shared memory; a base-only upload (265 MiB) or
  a half-resolution tier are the fallbacks if it matters, and both are
  renderer-side choices — the sheet stays the font's bytes.
- **Everything is one size.** The container can carry one cell size in the
  header and a (layer, x, y) per cell, no per-cell dimensions; the inventory
  asserts uniformity so a future font that breaks it fails at bake, not at
  draw.

Sizes worth saying out loud: the vendored font is 10.7 MB and the sheet will
be ~10.5 MB (the PNG bytes plus a small table), both committed, against a
29.5 MiB pack today. Accepted for the same reason the fixture inputs are
vendored: the committed artifact must rebuild byte-for-byte here, offline.

## Step 2 — the sheet generator and container — NEXT

`tools/gen_emoji_sheet.py` → `assets/atlas/emoji-sheet.bin`, a committed
artifact with a `--check` mode, declared in build.toml. Header (magic, cell
size, layer geometry, counts), the cell table keyed by font glyph id, the
codepoint → glyph table (1,501), the sequence → glyph table (4,166, carried
for the shaping pass that does not exist yet), then the PNG bytes verbatim.

## Steps 3–6 — as agreed

3. Append-only re-bake of trie and glyph map; the gate is every existing
   pixel baseline staying byte-equal.
4. Loader and upload with the override flag; load time and memory on both
   boxes.
5. Shader, a visual-check fixture, an `emoji` golden view, a mutation.
6. Correctness sweep: picking on double-advance cells, backdrop tint, dither.
