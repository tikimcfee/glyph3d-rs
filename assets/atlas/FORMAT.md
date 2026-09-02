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

The curve/glyph-map payloads are the **exact texture images** the web renderer
uploads, recovered from the build-time baked asset
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
| 4 | height | 5 |
| 5 | entryCount | 4431 (= maxGlyphId + 1) |
| 6–7 | reserved | 0 |
| 8 … | payload | `width × height × 4` u32 texels |

Texel `g` (glyph slot id) is:

```
map[g] = [curveStart, curveCount, mode, emojiCell]
  mode 0 = outline: curves [curveStart, curveStart+curveCount) in curves.bin;
           curveCount == 0 ⇒ empty glyph (space…) — discard unless a background
           fill paints the cell.
  mode 1 = color-emoji bitmap: curveStart/curveCount are 0; emojiCell indexes the
           emoji atlas grid. The shader must branch on mode BEFORE treating
           curveCount == 0 as "empty", or emoji render invisible.
```

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
| 4 | slotCount | 4431 |
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
| 3 | u32 | emojiCell — bitmap slots only, else `0xFFFFFFFF` |
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
`.blank` for slot 0, `<emoji cell N>` for bitmap slots), **no terminators** — name
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
| 1 | version | 1 |
| 2 | headerBytes | 44 |
| 3 | blockShift | 8 (block = 256 codepoints) |
| 4 | blockIndexLength | 4352 (= 0x110000 >> 8) |
| 5 | blockCount | 28 (content-deduplicated blocks) |
| 6 | entryStride | 4 (u32 lanes per entry) |
| 7 | mappedCount | 5349 (codepoints with a real entry) |
| 8 | missingAdvanceFu | 1229 (advance the missing block carries) |
| 9 | missingHeightFu | 2320 |
| 10 | primaryUpem | 2048 |
| 11 … | blockIndex | 4352 u32 |
| | blocks | `blockCount × 256 × 4` u32 |

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
3839, map texel `[0, 0, 1, 305]`, trie advance 2458 (double width), flags BITMAP.
`'🚀'` (U+1F680, outside the baked ranges) → missing block, flags MISSING, slot 0.

## File: `engine-trie.bin` (magic `G3TR`) — Stage E1

The same codepoint→slot trie as `codepoints.bin`, re-containered for the Mojo
engine (`engine-local/ffi.mojo::glyph_engine_load_trie_file` dispatches on
magic: `G3DF` fixture or `G3TR` blob). Generated by `tools/gen_real_trie.py`;
the web trie's container layout (GlyphTrie.js): GLYPH_ID/FLAGS native u32,
ADVANCE/HEIGHT **bitcast f32 world units** (not integer font units).

| word offset | field | value |
|---|---|---|
| 0 | magic | `'G3TR'` |
| 1 | version | 1 |
| 2 | headerBytes | 44 |
| 3 | blockShift | 8 |
| 4 | blockIndexLength | 4352 |
| 5 | blockCount | 28 |
| 6 | entryStride | 4 |
| 7 | mappedCount | 5349 (informational) |
| 8 | primaryUpem | 2048 (informational) |
| 9 | primaryEmHeightFu | 2320 (the conversion denominator) |
| 10 | cellHeightWorld | 1.0 as f32 bits (the world cell height) |
| 11 … | blockIndex | 4352 u32 — verbatim copy of codepoints.bin's |
| | blocks | `blockCount × 256 × 4` words: `[GLYPH_ID u32][ADVANCE f32][HEIGHT f32][FLAGS u32]` |

Conversion (rounded once per store, f64 → f32): `advance_world =
fround(advanceFu × cellHeightWorld / primaryEmHeightFu)`, so one cell =
`fround(1229/2320) ≈ 0.52974`, bitmap (emoji) entries carry exactly 2× that,
and every HEIGHT is exactly `1.0` (the renderer's quad height). Flags are
codepoints.bin's verbatim (bit0 MISSING, bit1 BITMAP, bit2 BLANK); the engine
reads only bit0.

## Scope limits / what is NOT in this export

- **Emoji bitmap pixels**: bitmap slots reference cells of a Canvas2D-rendered
  emoji atlas the web app draws at runtime. Cell indices are exported (glyphmap
  `.w` / glyphs.bin `emojiCell`) but the atlas image is not — the native port
  needs its own emoji raster source, or may render bitmap slots as blanks.
- **Runtime growth**: codepoints outside `LARGE_CORE_RANGES` (CJK, kana, most of
  the Nerd-Font PUA, 🚀-class emoji beyond U+1F64F) are MISSING in the trie. The
  web app grows the atlas live (shape → allocate slot → encode → re-upload); the
  native port can do the same using the `glyphs.bin` normalization denominators,
  or ship additional range bakes.
- The three TTF font files are NOT copied into this export; they remain in the
  reference repo under `packages/glyph3d-core/src/fonts/` (copy them over if the
  native side wants to do its own HarfBuzz shaping for live growth).

## Numbers at a glance (as exported)

- 4431 slots: 3511 outline glyphs, 22 empty, 897 bitmap (emoji); slot 0 = blank
- 82239 quadratic curves; curve texture 1024×161 RGBA32Uint (2.5 MiB payload)
- glyph-map texture 1024×5 RGBA32Uint (80 KiB payload)
- trie: 5349 mapped codepoints, 28 unique blocks, 4352-entry index (129 KiB)
- fonts: Cousine (primary, monospace cell advance 1229/2048 em), MesloLGS NF Mono,
  DejaVu Sans; em height 2320 fu (asc 1705 / desc −615)
