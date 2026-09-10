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

## Step 2 — the sheet generator and container (2026-09-10) [measured]

`tools/gen_emoji_sheet.py` → `assets/atlas/emoji-sheet.bin`, magic `G3ES`,
byte-level format in `assets/atlas/FORMAT.md`. A committed artifact in
build.toml, verified by its own `--check` on every battery pass, with a
mutation (`emoji-sheet-byte`) that proves the gate reddens when the file is
edited in place.

| | |
|---|---|
| file | 10,885,128 bytes: 160 B header, 341 KB of tables, 10,543,900 B of PNG verbatim |
| cells | 3,985 of 136×128, in **2 layers of 8192×4352** (60 cols × 34 rows each = 8160 px wide, padded to 8192 so every mip's row pitch is 256-byte aligned for upload; 2.6 % slack against 48 % for two full-height layers) |
| tables | cell (glyph → layer, x, y, PNG range), codepoint → glyph (all 1,501), sequence → glyph (4,166 × fixed stride 11), cell names |
| provenance | the source font's sha256 is in the header — the sheet names the font it came from |
| bake | 0.15 s; two bakes byte-identical; `--check` rebuilds in memory, byte-compares, and asserts structure with a reader that shares no variables with the writer |

What the structural assertions hold: glyph ids strictly ascending, every cell
on the grid inside its layer with no collisions, every PNG a PNG of the
header's size, PNG lengths summing to the blob, codepoints sorted and in
range, sequences sorted with zero padding and every target owning a cell, a
layer never exceeding 8192 px, and no layer empty.

Two decisions made here rather than in the plan: the layer split is
row-balanced (`ceil(rows/layers)` rows per layer) so the last layer is not
mostly padding, and the sequence table is FIXED-stride (`2 + seqMax` words)
rather than variable — 183 KB against ~115 KB, in exchange for random access
without an index when a consumer arrives.

## Step 3 — the append-only re-bake (2026-09-10) [measured]

`export-atlas.mjs` gained step 4b: after the web bake is reproduced and
asserted against the baked envelope exactly as before, the committed sheet is
read and one bitmap slot is appended per single-codepoint emoji it can draw
and no outline font covers, in codepoint order, after the web's last slot.
The web's own bitmap slots keep their ids and have `emojiCell` re-pointed at
the sheet's cell table, or set to NO_CELL where the font has no bitmap.

| | before | after |
|---|---:|---:|
| slots | 4,431 | **5,261** (+830) |
| bitmap slots | 897 (all pointing at dead web canvas cells) | 1,727: 513 web slots re-pointed, 384 web slots NO_CELL, 830 appended |
| kept outline (emoji in the font, text here) | | 117 — digits, `#`, `*`, ©, ®, ❤ … |
| trie mapped codepoints / blocks | 5,349 / 28 | 6,160 / 39 |
| glyph-map texture | 1024×5 | 1024×6 |
| curves.bin | | **byte-identical** |

**The prefix is verbatim, proven by bytes, not argument:** in glyph-map slots
0..4430 exactly 897 words differ from the committed file, all in lane `.w`
(`emojiCell`), all in mode-1 slots; every appended texel is `[0, 0, 1, cell]`;
in the trie, 126 entries inside already-mapped blocks changed — 107 that were
MISSING and 19 that were BLANK, every one now a bitmap at an appended slot with
the double advance — and not one OUTLINE or web-bitmap entry moved (5,330
non-missing entries byte-identical). A scan of every golden
input, the pick-oracle corpus (`native/src`) and the engine-check inputs found
no codepoint whose class changed, so **all six pixel baselines stayed
byte-equal** through a re-bake that moved 830 slots — the gate this step was
allowed to move nothing else.

Two pins moved on purpose and said so: `gen_real_trie.py`'s self-test expected
the rocket to be MISSING (it refused the new trie and wrote nothing until the
expectation was updated to slot 4759 — the pin working); `verify_atlas.py`
now also asserts every bitmap slot's cell indexes the sheet.

What is deliberately NOT in this step: the 384 web-era slots with no cell
still discard (as they did); ZWJ still occupies a blank cell (slot 1, advance
1229) and VS16 maps to slot 1717 — the "trailing codepoints as zero-advance
blanks" seed is a trie policy for the sequence pass, recorded here as the
measured starting state. The pick oracle (`g_pick_oracle.py`) does not read
the trie and knows nothing of double advances; no corpus it runs on contains
one yet — a blind spot for step 6.

## Step 4 — the loader and the upload (2026-09-10) [measured on Linux]

`native/src/atlas.rs`: `EmojiSheet` parses the G3ES blob (asserting the
geometry, the cell table, the PNG ranges and the upload-pitch alignment it
relies on) and `EmojiTexture` decodes every cell into an `Rgba8UnormSrgb`
2D-array texture with box-filtered mips and uploads it. `--emoji-sheet PATH`
is the handle. The glyph scene holds the texture resident; nothing samples it
yet, which is the point of doing this step alone: **all six pixel baselines
byte-equal**, and the cost is known before a pixel depends on it.

| | Linux (9950X3D, RTX 5090) |
|---|---:|
| parse | 1.6 ms |
| decode, 3,985 palette PNGs | 15.3 ms on 32 threads (cell-row bands, no shared writes) |
| mips (3 levels, premultiplied box filter, single-threaded) | 71.7 ms |
| upload enqueue | 32.6 ms |
| texture | 2 layers × 8192×4352, 4 levels, **361.2 MiB** |
| VRAM, windowed repo scene | 2,999 → 3,380 MiB (**+381 MiB**) |

Three decisions made here, each cheap to revisit and each stated so the M2
run can disagree with numbers:

- **Straight alpha in the texture, premultiply in the shader.** The Slug
  pass already outputs premultiplied linear (`pow(color, 2.2) * alpha`); an
  sRGB texture sampled straight and premultiplied after the sample lands in
  the same space. Premultiplying BEFORE the sRGB decode would be a different
  (wrong) product.
- **Mips in premultiplied space, stored straight.** The PNGs are palette +
  tRNS and a transparent texel's colour is arbitrary; a straight-alpha
  average bleeds it into the edge. The unit test pins this on a 2×2 image
  with a transparent blue texel: alpha-weighted gives (204,153,102,160),
  straight would have put blue at 127.
- **Four mip levels, capped where the footprint would cross a cell.** Level
  3 is a 17×16-px cell; smaller than that the LOD backdrop replaces the
  segment. What this does NOT cover: draw-time bilinear at level 3 still
  mixes a cell's outermost texel with its neighbour's — a half-texel inset in
  the UV rect (step 5's job) is the standard cure.

The mip filter is the largest cost and single-threaded; on the M2's four
performance cores expect decode to be ~4x slower and mips about the same.
If ~380 MiB matters on 16 GiB shared memory, the levers are renderer-side
(upload fewer levels, or a half-resolution tier) and the sheet stays the
font's bytes.

## Steps 5–6 — as agreed

3. **DONE 2026-09-10 — see below.**
4. **DONE 2026-09-10 — see below.**
5. **NEXT.** Shader, a visual-check fixture, an `emoji` golden view, a
   mutation.
6. Correctness sweep: picking on double-advance cells, backdrop tint, dither.
