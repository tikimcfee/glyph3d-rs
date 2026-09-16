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

## Step 5 — the shader, the fixture, the frame (2026-09-10) [measured on Linux]

`glyph_field.wgsl`'s `mode 1` branch samples the sheet: the vertex stage
places the cell from its index alone (the same pure function the generator
used, its geometry in the params uniform, so no cell table crosses to the
GPU), insets the UV rect half a texel and flips v; the fragment stage
samples trilinear, multiplies the group tint in, and outputs premultiplied.
A `NO_CELL` slot becomes mode 2 and discards. The CPU staging path stages
bitmap slots like any glyph (only MISSING is dropped), so `--render-file`
shows emoji too — which is what lets the frame live on that path.

**Colour, as decided with Ivan:** the flag is the glyph map's `mode` lane,
the same place a curve glyph's flags live, and the branch on it is the same
branch. What differs is what "colour" means once a glyph has its own: the
per-instance colour is the syntax colour and an image does not take it; the
group tint is the same multiply every glyph gets, identity for a white
group. So a flag looks like a flag in an untinted file, a tinted file tints
its emoji, and the highlight verbs stay uniform. The alpha contract (straight
in the texture, premultiplied mips, tint after the sRGB decode, premultiplied
out) is stated ONCE in the shader header so another platform has a checklist
if its emoji edges differ while its text does not.

`native/fixtures/emoji-view.txt` (IMMUTABLE) is one line per class of slot:
web-era re-pointed, appended, the rat worked example, regional indicators as
single glyphs, emoji-in-the-font-that-are-text, keycap and ZWJ sequences as
their pieces, skin modifiers as swatches, no-cell slots blank. Rendered at
1.25× as the `emoji` golden view; the `emoji-uv-flip` mutation (sampling the
sheet upside down) proves it reddens and that no text frame samples the
sheet. All six earlier baselines byte-equal, again.

What the frame shows that is not yet right, deliberately left for a change
that moves it on purpose: emoji fill the web's SQUARE quad, so the 136×128
cell is squeezed 6 % horizontally and the bitmap baseline (27 px of 128 up)
sits ~5 % of an em below text's. A bearing-aware quad is a one-line vertex
change plus a re-baseline of this one frame.

## Step 6 — as agreed

3. **DONE 2026-09-10 — see below.**
4. **DONE 2026-09-10 — see below.**
5. **DONE 2026-09-10 — see below.**
## Step 6 — the correctness sweep (2026-09-10) [measured on Linux]

**Picking.** Row/col picks never see an advance — `col` is a leader count on
both the renderer and the oracle — so the five probes on `emoji-view.txt`
(a web-era rocket, an appended rocket, the `d` two leaders after it, a
flag, a sparkle) agreed with `g_pick_oracle.py` before any change. The ray
path is the one that could be wrong (a record's rect is `[x, x + advance]`),
so the pick-oracle gate now round-trips a pixel through the rocket, through
the `d` after it, and through a web-era slot; all resolve to the record the
row/col pick named. Nothing in the pick code changed.

**Backdrop tint.** `seg_tint` averaged every instance's colour — for an
emoji that is the syntax colour, which it does not display. `Atlas` now
carries `slot_ink`: per slot, the alpha-weighted mean LINEAR rgb of a bitmap
cell (computed in the decode workers from the same pixels the texture gets,
through the same pow-2.2 table as the shader) and its mean alpha; the tint
uses it for bitmap slots and counts them as the two cells their advance
covers. Outline glyphs sum exactly as before, so **all seven goldens are
byte-equal** (none has an emoji). Measured on the emoji fixture at 0.02×
(one backdrop quad): mean backdrop rgb 121/126/123 → 125/129/125 — a few
levels warmer on a segment that is 593 glyphs, 40 of them emoji. Correct
direction, small, and nothing gates it: no golden frames a far emoji-heavy
segment. Recorded as a blind spot rather than papered with a frame that
would be one tinted rectangle.

**LOD and dither.** The fade band the web had is not ported (the shader
header has said so since Stage C; it hard-discards at alpha 0), and the LOD
swap is per SEGMENT in the CPU cull, so emoji and text swap together. Nothing
emoji-specific to do; the mip cap in `mip_levels_for` is what keeps the
far end of an emoji clean until the swap.

**Windowed.** `--render-file fixtures/emoji-view.txt` on Wayland: 75 FPS at
Fifo, frame 30 captured, no GPU errors.

**For the M2, whenever it pulls:** the sheet load (decode + mips + upload) is
~120 ms and 361 MiB here; expect a few hundred ms and the same bytes there.
`cargo glyph test` will report `emoji.png` as having no `metal-apple`
baseline and print the adoption commands; look at the frame first — this is
the one view where the two rasterizers' FILTERS, not their edge rules, are
being compared, and `cargo glyph drift` will say more than "edge noise".

## Step 7 — the M2 pulled (2026-09-16) [measured on macOS/Metal]

Step 6 ended with a note "for the M2, whenever it pulls". It pulled. Every
prediction in this file held, and the emoji work needed no Metal-specific fix.

| | Linux (9950X3D, RTX 5090) | macOS (M2, Metal) |
|---|---:|---:|
| parse | 1.6 ms | 3.2 ms |
| decode, 3,985 palette PNGs | 15.3 ms / 32 threads | 58.3 ms / 8 threads |
| mips (3 levels, single-threaded) | 71.7 ms | 85.1 ms |
| upload enqueue | 32.6 ms | 39.1 ms |
| total sheet load | ~121 ms | ~186 ms |
| texture | 361.2 MiB | **361.2 MiB, same bytes** |

Decode is 3.8x slower on four performance cores — step 4 predicted ~4x — and
mips are within 19 %, also as predicted. Peak RSS rendering the whole
`native/src` tree (773,346 instances) is 887 MB, of which the sheet is 361 MiB:
a **fixed floor every scene now pays** on 16 GiB of shared memory. Comfortable
here, and the levers if it ever stops being comfortable are the renderer-side
ones step 4 already named.

**The append-only claim held on the other rasterizer.** All six pre-existing
`metal-apple` baselines byte-equal through a re-bake that appended 830 slots —
which is the point: that claim is about slot ids, not pixels, so a move there
would have been a real defect rather than a rasterizer difference.

**The colour-bitmap path is not where the platforms differ.** `glyph drift`,
metal-apple against vulkan-nvidia:

| view | differing | max delta | >= 16 | clustered |
|---|---:|---:|---:|---:|
| emoji | 1.92 % | 3 | 0 | 0 |
| text | 0.85 % | 3 | 0 | 0 |
| repo-back-oblique | 8.93 % | 69 | 2,487 | 236 |

Step 5 expected "more cross-vendor drift here than on text (filtered sampling
is not analytic coverage)". The opposite is what happened: the emoji frame
drifts *less* than the oblique text frame and sits in the same band as flat
text. Two rasterizers decoded the sheet to the same texels and only filter
rounding separates them — which is what committing the font's own PNG bytes
buys. `Rgba8UnormSrgb` 2D-array, 2 layers of 8192x4352, bound and drew on
Metal without a complaint; the 8,192 limit that shaped the container in step 1
was the right thing to design around.

`emoji.png` was adopted as the `metal-apple` baseline after that looking. Both
generators (`emoji-sheet`, the atlas re-bake) reproduce BYTE-IDENTICAL here
offline, and the exact `osx-arm64` mojo/max pins added with fonttools did their
job: the solve stayed on `dev2026083005`.

**Windowed, on Metal:** `--render-file fixtures/emoji-view.txt` draws emoji
through the swapchain with the egui overlay up, 60 FPS at Fifo, frame 30
captured, no GPU errors. The gates are all offscreen, so this path is only ever
covered by hand.

Still open, unchanged by this run and still deliberate: the SQUARE quad (emoji
squeezed ~6 %, bitmap baseline ~5 % of an em low), sequence shaping, and the
far emoji-heavy backdrop segment that no golden frames.
