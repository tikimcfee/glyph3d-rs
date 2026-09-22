# glyph3d-native atlas assets — byte-level format documentation

Stage B export of the glyph3d-js Slug glyph atlas for the Rust+wgpu port.
All files are little-endian arrays of 32-bit words, each starting with a
self-describing header. Regenerate with:

```
node tools/export-atlas.mjs           # writes assets/atlas/*.bin
python3 tools/verify_atlas.py         # structural + semantic assertions
python3 tools/preview_glyphs.py       # visual proof → assets/atlas/preview.png
```

## Provenance (what these bytes ARE)

The curve payload is the **exact texture image** the web renderer uploads, and
the glyph map's first 4,431 entries are its exact texels with one lane
re-pointed (`emojiCell`, see below); the entries after them are the native
emoji slots (2026-09-10). Both are recovered from the build-time baked asset
`app/public/slug-core/slug-core.1tstke3lync.bin` in the reference repo (gzip of the
`SLGC` envelope defined in `packages/glyph3d-core/src/shaping/slugCoreCache.js`,
payload format `SLUG_BUFFER_FORMAT = 2`).

The codepoint→slot mapping and per-slot metrics are **not** in that envelope; the
exporter reproduces the bake boot headlessly (`tools/headlessFontChain.mjs` in the
reference repo: font chain `Cousine → MesloLGS NF Mono → DejaVu Sans`, stub emoji
atlas, prime the shape cache over `LARGE_CORE_RANGES` from
`packages/glyph3d-r3f/src/coreRanges.js`). Slot allocation is a dense counter in
prime order, so the reproduction is deterministic — and the exporter **aborts** if
the reproduced slot set, slot count, or bitmap emoji cells differ from the baked
envelope. They matched exactly on export.

## Glyph identity model (read this first)

- A **glyph id** is a FontChain **global slot**: a dense integer identifying a
  `(fontIndex, per-font glyph id)` pair across the 3-font fallback chain. Slot 0 is
  the reserved **blank** cell (uncoverable codepoints).
- Slots are allocated in the order codepoints are first shaped at boot. Slot ids
  are stable **only** for a fixed (font files, LARGE_CORE_RANGES) tuple — any drift
  there is a different baked core with different ids.
- A slot is either an **outline** glyph (Slug bezier curves), a **bitmap** glyph
  (color-emoji cell — no curves), or **empty** (space, zero-width, `.notdef`-like).
- Every texture/map/trie is keyed by this slot id.

## Coordinate conventions (what the WGSL port must know)

- Curve coordinates are **normalized per-glyph-cell to [0,1]**, stored as
  `uint16 = round(clamp(v,0,1) * 65535)` inside each 32-bit channel
  (`packUint16`, `slug-constants.js`). Unpack: `v = bits / 65535.0`.
  - **X**: `[0, advance] → [0,1]` in the glyph's **own font's** units; x = 0 is the
    pen origin (left bearing edge of the cell), x = 1 is one advance width.
  - **Y**: `[descender, ascender] → [0,1]` in the glyph's own font's units.
    **Y is UP**: y = 0 at the descender, y = 1 at the ascender. The Slug coverage
    math relies on this winding ("fills accumulate positive under y-up
    normalization"). **No y-flip** when the quad UV's v axis also runs bottom→top
    (as the web quad does); flip only if your quad UV runs top→down.
  - The clamp means ink outside the cell (overhang: negative side bearings,
    overshoots, control-point hull past the advance) is **clipped to the cell
    edge** — benign for this monospace chain; see `bbox` in `glyphs.bin` (values
    may sit exactly on 0.0/1.0 because of clamping).
- The per-slot normalization denominators are exported in `glyphs.bin`
  (`advanceFu`, `ascenderFu`, `descenderFu`), so a native encoder can reproduce
  the normalization for NEW glyphs: `x_norm = x_fu / advanceFu`,
  `y_norm = (y_fu - descenderFu) / (ascenderFu - descenderFu)`.
- Texture addressing: both textures are width-1024, row-major, RGBA32Uint.
  Texel `i` lives at `(x, y) = (i % 1024, i / 1024)`; WGSL:
  `textureLoad(tex, vec2<i32>(i % 1024, i / 1024), 0)`.
- Texels-per-curve is 2; `curveStart` is a **curve index**, so the texel index of
  curve `c` of a glyph is `(curveStart + c) * 2`.

## File: `curves.bin` (magic `G3CV`)

The Slug curve texture, verbatim. Bind as `texture_2d<u32>` (RGBA32Uint).

| word offset | field | value |
|---|---|---|
| 0 | magic | `'G3CV'` (bytes `47 33 43 56`) |
| 1 | version | 1 |
| 2 | headerBytes | 32 |
| 3 | width | 1024 |
| 4 | height | 161 |
| 5 | curveCount | 82239 |
| 6 | texelsPerCurve | 2 |
| 7 | reserved | 0 |
| 8 … | payload | `width × height × 4` u32 texels |

Payload layout: curve `c` (0-based, globally across all glyphs) occupies texels
`2c` and `2c+1`:

```
texel 2c   = [P0.x, P0.y, P1.x, P1.y]   (uint16-in-u32 each)
texel 2c+1 = [P2.x, P2.y, 0, 0]
```

for the quadratic bezier P0 →(control P1)→ P2. Straight line segments are encoded
as degenerate quadratics with the control point at the midpoint. Only the first
`curveCount × 2` texels are used; the remainder of the final row is zero padding.
**Note**: P0/P1/P2 of consecutive curves are NOT implicitly connected — each curve
carries its own endpoints (a contour's closing edge is an explicit curve).

## File: `glyphmap.bin` (magic `G3GM`)

The Slug glyph-map texture, verbatim. Same binding convention.

| word offset | field | value |
|---|---|---|
| 0 | magic | `'G3GM'` |
| 1 | version | 1 |
| 2 | headerBytes | 32 |
| 3 | width | 1024 |
| 4 | height | 10 |
| 5 | entryCount | 9427 (= maxGlyphId + 1: the web's 4,431 + 830 appended emoji slots + 4,166 appended sequence slots) |
| 6–7 | reserved | 0 |
| 8 … | payload | `width × height × 4` u32 texels |

Texel `g` (glyph slot id) is:

```
map[g] = [curveStart, curveCount, mode, emojiCell]
  mode 0 = outline: curves [curveStart, curveStart+curveCount) in curves.bin;
           curveCount == 0 ⇒ empty glyph (space…) — discard unless a background
           fill paints the cell.
  mode 1 = color-emoji bitmap: curveStart/curveCount are 0; emojiCell indexes
           emoji-sheet.bin's CELL TABLE (0..cellCount-1), or is NO_CELL
           (0xFFFFFFFF) for a web-era bitmap slot the vendored font has no
           bitmap for (384 of the web's 897 — mostly U+2Bxx symbols the web let
           fall to its canvas). The shader must branch on mode BEFORE treating
           curveCount == 0 as "empty", or emoji render invisible.
```

**Slots 0–4430 are the web bake; 4431–5260 are appended** (2026-09-10) by
`export-atlas.mjs` step 4b: one bitmap slot per single-codepoint emoji the
sheet can draw and no outline font covers, allocated in codepoint order. No
existing id moved, which is what keeps every text frame byte-equal. The
policy is the web's own: a codepoint an outline font draws stays outline
(digits, `#`, `*`, ©, ®, ❤ … are emoji in the font and text here — 117 such).

**Slots 5261–9426 are the sequence slots** (2026-09-20, step 4c): one bitmap
slot per entry in `emoji-sheet.bin`'s sequence table, allocated in the
table's own (sorted) order, so a sequence's slot id is `5261 + index` — a
pure function of the sheet, carried verbatim into the engine trie
(`gen_real_trie.py` cross-checks the slot base against glyphs.bin). Each is a
`[0, 0, 1, cell]` texel like any appended emoji slot; the sequence pass
resolves a cluster head to one of these slots (the trie v2 sections above —
`--cluster-mode cluster` renders through them today). Cluster heads render
through the same mode-1 branch and the same square quad as any emoji.

Slots that exist but were never encoded (holes in the id space) read as
`[0, 0, 0, 0]`: mode 0, zero curves — rendered blank. Slot 0 (blank) is such an
entry.

## File: `glyphs.bin` (magic `G3GL`)

Per-slot metrics + the font table + a debug name table.

| word offset | field | value |
|---|---|---|
| 0 | magic | `'G3GL'` |
| 1 | version | 1 |
| 2 | headerBytes | 44 |
| 3 | fontCount | 3 |
| 4 | slotCount | 9427 |
| 5 | primaryUpem | 2048 (Cousine units-per-em — the layout reference) |
| 6 | primaryAdvanceFu | 1229 (forced monospace cell advance, primary font units) |
| 7 | primaryEmHeightFu | 2320 (primary ascender − descender = 1705 + 615) |
| 8 | fontRecordBytes | 64 |
| 9 | slotRecordBytes | 56 |
| 10 | reserved | 0 |

Then, in order:

### Font records — `fontCount × 64` bytes

| byte | type | field |
|---|---|---|
| 0 | u32 | upem |
| 4 | i32 | ascender (hhea horizontal extents) |
| 8 | i32 | descender (negative) |
| 12 | i32 | lineGap |
| 16 | char[48] | font name, UTF-8, NUL-padded |

Exported fonts, in chain-priority order: `[0] Cousine (upem 2048, asc 1705,
desc −615)`, `[1] MesloLGS NF Mono (upem 2048, asc 2001, desc −583)`,
`[2] DejaVu Sans (upem 2048, asc 1901, desc −483)`. Routing is per-codepoint:
the first font whose cmap covers a codepoint draws it.

### Slot records — `slotCount × 56` bytes (index = glyph slot id)

| word | type | field |
|---|---|---|
| 0 | u32 | fontIdx — 0..fontCount−1, or `0xFFFFFFFF` = blank slot, `0xFFFFFFFE` = bitmap |
| 1 | u32 | gid — per-font HarfBuzz glyph id (0 for blank/bitmap) |
| 2 | u32 | flags — bit0 `BITMAP`, bit1 `EMPTY` (outline mode, no curves) |
| 3 | u32 | emojiCell — bitmap slots: index into `emoji-sheet.bin`'s cell table, or `0xFFFFFFFF` (no bitmap in the font); else `0xFFFFFFFF` |
| 4 | i32 | advanceFu — this glyph's advance in **its own font's** units (curve-normalization denominator; NOT the layout advance — layout always uses primaryAdvanceFu, or 2× for bitmap) |
| 5 | i32 | ascenderFu — own font's ascender (y-normalization range top) |
| 6 | i32 | descenderFu — own font's descender (negative) |
| 7 | u32 | curveStart — mirror of the glyphmap texel |
| 8 | u32 | curveCount — mirror of the glyphmap texel |
| 9 | f32 | bboxMinX — control-point-hull bbox in normalized cell coords |
| 10 | f32 | bboxMinY |
| 11 | f32 | bboxMaxX |
| 12 | f32 | bboxMaxY |
| 13 | u32 | reserved (0) |

### Name table

`slotCount` u32 offsets (relative to the blob start), then `u32 blobBytes`, then
the blob: concatenated UTF-8 names (HarfBuzz glyph names, e.g. `A`, `numbersign`;
`.blank` for slot 0, `<emoji U+XXXX>` or `<emoji U+XXXX, no cell>` for bitmap slots), **no terminators** — name
`i` spans `offsets[i] .. offsets[i+1]` (or blobBytes for the last). Debug/log use
only.

## File: `codepoints.bin` (magic `G3CP`)

Codepoint → (glyph slot, advance, height, flags) as a two-level trie — the exact
structure the web GPU decode kernel walks (`GlyphTrie.js` /
`glyphPipelineKernels.js`), with measures stored as **integer primary-font units**
instead of the web's bitcast-f32 world units (world scale is a native-side choice;
font units are lossless).

| word offset | field | value |
|---|---|---|
| 0 | magic | `'G3CP'` |
| 1 | version | 2 |
| 2 | headerBytes | 68 |
| 3 | blockShift | 8 (block = 256 codepoints) |
| 4 | blockIndexLength | 4352 (= 0x110000 >> 8) |
| 5 | blockCount | 39 (content-deduplicated blocks) |
| 6 | entryStride | 4 (u32 lanes per entry) |
| 7 | mappedCount | 6160 (codepoints with a real entry) |
| 8 | missingAdvanceFu | 1229 (advance the missing block carries) |
| 9 | missingHeightFu | 2320 |
| 10 | primaryUpem | 2048 |
| 11 | bitmapAdvanceFu | 2458 — the cluster head's advance, fu (v2) |
| 12 | sequenceCount | 4166 (v2) |
| 13 | seqMax | 9 — the sequence entry stride is `2 + seqMax` (v2) |
| 14 | seqOff | word offset of the sequence section (v2) |
| 15 | classOff | word offset of the class section (v2) |
| 16 | classWords | the class section's size in words (v2) |
| 17 … | blockIndex | 4352 u32 |
| | blocks | `blockCount × 256 × 4` u32 |
| | sequence section | `sequenceCount × (2 + seqMax)` u32: `[slot, len, cp₀ … cp₍len−1₎, 0-pad]`, sorted by sequence |
| | class section | `cluster-classes.bin` verbatim |

**v2 (2026-09-20, the sequence pass):** the header grows 44 → 68 B and the two
sections land after the blocks, byte-identical to the `engine-trie.bin`
sections `gen_real_trie.py` carries across.

Lookup (two dependent loads, exactly what the WGSL decode kernel should do):

```
block   = blockIndex[cp >> 8]
e       = ((block << 8) | (cp & 0xFF)) * 4
glyphId = blocks[e + 0]          // u32, exact identity (0 = blank)
advance = blocks[e + 1] as i32   // primary-font units; bitmap entries carry 2× cell
height  = blocks[e + 2] as i32   // constant = primaryEmHeightFu for every entry
flags   = blocks[e + 3]          // bitfield
```

Flags: bit0 `MISSING` (never encoded — render blank, keep the advance, report for
live growth), bit1 `BITMAP` (glyphId is an emoji bitmap slot), bit2 `BLANK`
(codepoint IS covered by the mapping but resolves to slot 0 — e.g. unassigned
codepoints inside a mapped block; do NOT report as missing).

Block 0 of `blocks` is the shared **missing block** (all entries
`[0, missingAdvanceFu, missingHeightFu, FLAG_MISSING]`); every unmapped
`blockIndex` slot points at it. Other blocks are content-deduplicated, so
identical blocks share storage.

**UTF-8 first**: the trie keys on codepoints. The byte→codepoint step is plain
UTF-8 decoding; the web kernel does it per lead byte
(`glyphPipelineKernels.js::_buildDecode`): sequence length from the lead byte
(`0xxxxxxx`=1, `110xxxxx`=2, `1110xxxx`=3, `11110xxx`=4, `10xxxxxx`=continuation),
codepoint assembled from the masked payload bits. Continuation bytes produce no
glyph (they are non-leaders).

**World-space conversion** (matching the web builder):
`advance_world = advanceFu × cellHeightWorld / primaryEmHeightFu`, and every
glyph's quad height is the constant `cellHeightWorld`. In the web app
`cellHeightWorld = fontSize × (primaryEmHeightFu / primaryUpem) × worldScale`
with `fontSize = 48` — but the native port picks its own cell height; only the
ratio matters. Bitmap (emoji) glyphs get a **square** quad of side
`cellHeightWorld` and a **double-width** layout advance (already baked into their
trie advance: 2458 fu = 2 × 1229).

## Worked example: 'A' (U+0041)

1. UTF-8: single byte `0x41` → codepoint `0x41`.
2. Trie: `blockIndex[0x41 >> 8] = blockIndex[0] = 1`; entry at
   `((1 << 8) | 0x41) * 4` in `blocks` reads **`[34, 1229, 2320, 0]`** →
   slot **34**, advance 1229 fu (one cell), flags 0 (outline).
3. Glyph map: texel 34 = **`[557, 14, 0, 0]`** → 14 curves starting at curve 557,
   mode 0 (outline).
4. Curve 557 texels: `[55137, 17372, 51458, 22768]` + `[47778, 28163, 0, 0]`
   → P0 = (0.84134, 0.26508), P1 = (0.78520, 0.34742), P2 = (0.72905, 0.42974)
   in cell space. In Cousine font units that's
   P0 = (0.84134 × 1229, 0.26508 × 2320 − 615) ≈ (1034, 0) — the bottom-right
   foot of the 'A' (y_fu = 0 is the baseline: 0.26508 × 2320 ≈ 615 = −descender).
5. `glyphs.bin` slot 34: fontIdx 0 (Cousine), gid 36, flags 0, advanceFu 1229,
   asc 1705, desc −615, bbox [0.0000, 0.2651, 0.9992, 0.8466], name `A`.

More: `' '` (U+0020) → slot 1, `[0, 0, 0, 0]` in the map (empty), trie advance
1229 (occupies a cell). `'@'` → slot 33, 53 curves. `'🐀'` (U+1F400) → bitmap slot
3839, map texel `[0, 0, 1, 487]` (sheet cell 487), trie advance 2458 (double
width), flags BITMAP. `'🚀'` (U+1F680, outside the web's baked ranges) →
**appended** bitmap slot 4759, map texel `[0, 0, 1, 958]`, advance 2458.
`U+E0020` (tag space: known to the font, no bitmap) → missing block, slot 0.

## File: `engine-trie.bin` (magic `G3TR`) — Stage E1

The same codepoint→slot trie as `codepoints.bin`, re-containered for the Mojo
engine (`engine/ffi.mojo::glyph_engine_load_trie_file` dispatches on
magic: `G3DF` fixture or `G3TR` blob). Generated by `tools/gen_real_trie.py`;
the web trie's container layout (GlyphTrie.js): GLYPH_ID/FLAGS native u32,
ADVANCE/HEIGHT **bitcast f32 world units** (not integer font units).

**v2 (2026-09-20, the sequence pass)**: the header grows 44 → 68 B and the blob
gains two sections after the blocks — the sequence table and the class table —
byte-identical to the `codepoints.bin` sections they are carried from:

| word offset | field | value |
|---|---|---|
| 0 | magic | `'G3TR'` |
| 1 | version | 2 |
| 2 | headerBytes | 68 |
| 3 | blockShift | 8 |
| 4 | blockIndexLength | 4352 |
| 5 | blockCount | 39 |
| 6 | entryStride | 4 |
| 7 | mappedCount | 6160 (informational) |
| 8 | primaryUpem | 2048 (informational) |
| 9 | primaryEmHeightFu | 2320 (the conversion denominator) |
| 10 | cellHeightWorld | 1.0 as f32 bits (the world cell height) |
| 11 | bitmapAdvance | the cluster head's advance, f32 bits (= the bitmap 2× cell) |
| 12 | sequenceCount | 4166 |
| 13 | seqMax | 9 — the sequence entry stride is `2 + seqMax` |
| 14 | seqOff | word offset of the sequence section (appended after the blocks) |
| 15 | classOff | word offset of the class section |
| 16 | classWords | the class section's size in words |
| 17 … | blockIndex | 4352 u32 — verbatim copy of codepoints.bin's |
| | blocks | `blockCount × 256 × 4` words: `[GLYPH_ID u32][ADVANCE f32][HEIGHT f32][FLAGS u32]` |
| | sequence section | `sequenceCount × (2 + seqMax)` words: `[slot, len, cp₀ … cp₍len−1₎, 0-pad]`, sorted by sequence |
| | class section | `cluster-classes.bin` VERBATIM (its own `G3CC` header included) |

The loader reads v1 (44 B header, no sections) or v2 and refuses ≥ 3.
`codepoints.bin` v2 carries the IDENTICAL two sections (same byte layout; the
word-11 advance is fu there, f32 bits here — the only difference).

Conversion (rounded once per store, f64 → f32): `advance_world =
fround(advanceFu × cellHeightWorld / primaryEmHeightFu)`, so one cell =
`fround(1229/2320) ≈ 0.52974`, bitmap (emoji) entries carry exactly 2× that,
and every HEIGHT is exactly `1.0` (the renderer's quad height). Flags are
codepoints.bin's verbatim (bit0 MISSING, bit1 BITMAP, bit2 BLANK); the engine
reads only bit0.

## File: `emoji-sheet.bin` (magic `G3ES`) — the colour-emoji bitmaps

Generated by `tools/gen_emoji_sheet.py` (`pixi run gen-emoji-sheet`) from the
vendored `tools/vendor/third-party/noto-emoji/NotoColorEmoji.ttf` (Noto Color
Emoji 2.051, OFL). **The PNG bytes are the font's own, verbatim** — nothing is
decoded, resampled or re-encoded at bake, so the file is byte-exact on every
platform and `--check` rebuilds and compares it on every battery pass. This is
the pixel source the bitmap branch (`mode 1`) never had: the web app drew emoji
through Canvas2D at runtime, which this tree cannot reproduce.

**Identity is the font's glyph id**, not a FontChain slot and not a codepoint.
Every bitmap the font has is a cell — including the ~2,500 that only a
codepoint SEQUENCE reaches (flags, skin tones, ZWJ families). Leader mode
addresses only the single-codepoint cells through the codepoint table; under
cluster mode the sequence pass resolves a cluster head to its sequence slot
(the 2026-09-20 sequence pass — `out/EMOJI.md`, `engine/delta/cluster-mode.md`).

Header — 40 u32 words (160 bytes):

| word | field | value (as baked) |
|---|---|---|
| 0 | magic | `'G3ES'` |
| 1 | version | 1 |
| 2 | headerBytes | 160 |
| 3, 4 | cellW, cellH | 136, 128 — every cell, asserted at bake |
| 5 | cellCount | 3985 |
| 6 | cols | 60 — cells per row (8192 // cellW) |
| 7 | rowsPerLayer | 34 |
| 8 | layers | 2 |
| 9, 10 | layerW, layerH | 8192, 4352 — the texture size per layer; read these, derive nothing. `layerW` is `cols × cellW` (8160) padded up to a multiple of 64 px so every mip level's row pitch is a multiple of wgpu's 256-byte upload alignment; the padding holds no cell |
| 11 | ppem | 109 — the strike |
| 12, 13 | strikeAscender, strikeDescender | 101, −27 px (i32) |
| 14, 15, 16 | bearingX, bearingY, advancePx | 0, 101, 136 — one set for every cell |
| 17 | fontUpem | 2048 |
| 18, 19 | hheaAscender, hheaDescender | 1900, −500 (i32) |
| 20 | numGlyphs | 4027 — the font's glyph count; every glyph id is below it |
| 21 | codepointCount | 1501 |
| 22 | sequenceCount | 4166 |
| 23 | seqMax | 9 — longest sequence; the sequence stride is `2 + seqMax` |
| 24 | pngStart | byte offset of the PNG blob |
| 25 | pngBytes | 10543900 |
| 26–31 | reserved | 0 |
| 32–39 | fontSha256 | the source font's sha256 as 8 words — the sheet names the font it came from |

Then, in order, all u32 little-endian:

- **Cell table** — `cellCount × 6`: `[glyphId, layer, x, y, pngOffset, pngLen]`.
  Sorted by glyphId (strictly ascending). `(x, y)` in pixels within the layer,
  on the cell grid; placement is a pure function of the cell's index:
  `layer = i / (cols·rowsPerLayer)`, `row = (i % (cols·rowsPerLayer)) / cols`,
  `col = i % cols`. `pngOffset` is relative to `pngStart`.
- **Codepoint table** — `codepointCount × 2`: `[codepoint, glyphId]`, sorted by
  codepoint. EVERY cmap entry, including the 41 whose glyph has no cell (NUL,
  CR, space, ZWJ U+200D, the tag characters U+E0030–E007F): "known to the font,
  no bitmap" is what the sequence pass's trailer posture needed to mark a
  sequence's trailing codepoints as zero-advance blanks.
- **Sequence table** — `sequenceCount × (2 + seqMax)`: `[len, glyphId, cp₀ … cp₍len−1₎, 0 …]`,
  sorted by the codepoint sequence. Every target glyph has a cell. Read by
  export-atlas.mjs's step 4c to mint the sequence slots above; the renderer
  never parses it (the trie's v2 section carries the same rows).
- **Name table** — `cellCount + 1` offsets then the blob: the font's glyph names
  for the cells (`u1F600`, `u1F1E6_1F1E8`, …), no terminators, padded to 4
  bytes. Debug/log use only.
- **PNG blob** — the cells' PNG files, concatenated in cell order, verbatim.

**Renderer contract (`native/src/atlas.rs`, `EmojiSheet` / `EmojiTexture`,
2026-09-10):** every cell is decoded at load (the `image` crate's PNG path;
the PNGs are palette + tRNS) into an `Rgba8UnormSrgb` 2D-array texture of
`layers` × `layerW` × `layerH`, **straight alpha**, with `mip_levels_for(cell)`
levels — 4 for the 136×128 cell: level `k` averages 2^k × 2^k texels and the
cap is the largest `k` whose footprint still tiles the cell, so no mip bleeds
a neighbour's colour into a cell's border (below a 17×16-px cell the LOD
backdrop replaces the segment anyway). Mips are box-filtered in
**premultiplied** space and un-premultiplied for storage, so a transparent
texel's arbitrary palette colour cannot bleed into its opaque neighbours'
average; the shader premultiplies after the sample. The glyph map's
`emojiCell` (`mode 1` slots) indexes the cell table; `--emoji-sheet PATH`
points the renderer at another G3ES file. The shader's `mode 1` branch samples
it (`glyph_field.wgsl`, whose header states the alpha contract in three lines:
linear rgb + straight alpha from the sampler, the group tint decoded with the
same `pow(2.2)` as text, premultiplied output). The cell's UV rect is the cell
inset half a texel and v-flipped (the quad's v runs up, the PNG's rows run
down); a cell index of `0xFFFFFFFF` draws nothing. **Emoji are drawn into the
existing SQUARE quad** (`quad_w = height`, the web's rule), so the 136×128
cell is squeezed 6 % horizontally and its bitmap baseline (27 px of 128 up)
sits ~5 % of an em below the text baseline; both are visible in `emoji.png`
and are the honest starting state — a bearing-aware quad is a later choice
that moves that frame on purpose. Load cost: ~120 ms and 361 MiB on the first
Linux box.

Cells are square-ish (136×128) at a 2× advance; the layout side of that is
already in the trie (bitmap entries carry `2 × 1229` fu), which is why no
engine change is needed.

## Scope limits / what is NOT in this export

- **Emoji bitmap pixels** — NO LONGER a gap as of 2026-09-10: `emoji-sheet.bin`
  above is the raster source. What is still true: the 897 `emojiCell` values in
  the four Stage B bins are the web's runtime canvas indices and mean nothing
  here; step 3 of `out/EMOJI.md` replaces them.
- **Runtime growth**: codepoints outside `LARGE_CORE_RANGES` (CJK, kana, most of
  the Nerd-Font PUA) are MISSING in the trie. Single-codepoint emoji are NOT a
  gap any more (every one the vendored font draws has a slot); emoji SEQUENCES
  have slots as of 2026-09-20 (step 4c above) but the codepoint trie remains
  one-glyph-per-codepoint — resolving a sequence to its slot is the engine-side
  sequence pass. The web app grows the atlas live
  (shape → allocate slot → encode → re-upload); the native port's answer is
  append-only re-bakes, never runtime growth.
- The three TTF font files are NOT copied into this export; they remain in the
  reference repo under `packages/glyph3d-core/src/fonts/` (copy them over if the
  native side wants to do its own HarfBuzz shaping for live growth).

## Numbers at a glance (as exported)

- 9427 slots: 3511 outline glyphs, 22 empty, 5893 bitmap (the web's 897, of
  which 513 have a sheet cell, + 830 appended single-codepoint, + 4,166
  appended sequence slots, all with one); slot 0 = blank
- 82239 quadratic curves; curve texture 1024×161 RGBA32Uint (2.5 MiB payload)
- glyph-map texture 1024×10 RGBA32Uint (160 KiB payload)
- trie: 6160 mapped codepoints, 39 unique blocks, 4352-entry index (173 KiB)
- emoji sheet: 3985 cells of 136×128 in 2 layers of 8160×4352 (10.9 MB)
- cluster classes: 1615 ranges over UCD 17.0 (19.4 KB; generated, not hand-written)
- fonts: Cousine (primary, monospace cell advance 1229/2048 em), MesloLGS NF Mono,
  DejaVu Sans; em height 2320 fu (asc 1705 / desc −615)
