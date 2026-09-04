# Atlas & emoji delta — web (reference) vs native

Web tree: `/Users/lugo/localdev/viz-web/glyph3d-js` (read-only reference).
Native tree: `/Users/lugo/localdev/viz-native/glyph3d-native`.
Measurements below were taken by parsing `assets/atlas/*.bin` directly, 2026-09-04.

---

## Summary

**The native atlas contains emoji.** 897 of its 4431 slots are colour-bitmap emoji
slots, and 897 codepoints across 20 ranges resolve to them with a doubled advance.
What was never exported is the emoji **pixels** — the raster sheet. Every other part
of the emoji path is present: slot identity, glyph-map `mode == 1`, the `emojiCell`
index, and the double-width layout advance.

**The "1 distinct height, 2 distinct advances" measurement is a fact about text, not
an artifact of emoji being absent — and it is emoji that make it 2 rather than 1.**
Reproduced exactly over all 0x110000 codepoints through `codepoints.bin`: advances
`{1229 fu: 4452, 2458 fu: 897}`, heights `{2320 fu: 5349}`. The 897 carrying the
second advance are precisely the 897 `FLAG_BITMAP` entries. Removing emoji from the
bake would make the measurement *stronger* (1 advance), not weaker. The measurement
holds because `FontChain` forces every glyph to the primary font's `M` advance and
because quad height is a single global constant — it is a property of the
forced-monospace model, not of the sample. `engine/PLAN-DRAFT.md:110-121` already
records the measurement and had already withdrawn the conclusion it was cited for,
on the separate ground that layout is not the only writer of those lanes.

The real gap is narrower and sharper than "native has no emoji": native has the emoji
*entries* and no way to draw them, plus one schema hole (`emojiCell` never reaches the
fragment stage) that must be closed before anything can.

---

## Measured facts

`assets/atlas/glyphs.bin` (4431 slot records, 56 B each):

| | count |
|---|---:|
| outline slots (`flags == 0`) | 3511 |
| empty slots (`SLOT_FLAG_EMPTY`, outline mode, 0 curves) | 23 |
| **bitmap emoji slots (`SLOT_FLAG_BITMAP`)** | **897** |
| blank slot (`fontIdx == 0xFFFFFFFF`) | 1 |
| by font: Cousine / MesloLGS NF Mono / DejaVu Sans | 1699 / 957 / 877 |
| distinct per-glyph `advanceFu` (own-font, curve-normalization denominator) | **175** |
| distinct per-glyph `ascenderFu` / `descenderFu` | 4 / 4 (3 fonts + 0 for bitmap/blank) |
| slots with `curveCount == 0` | 920 (= 897 bitmap + 23 empty) |

`assets/atlas/codepoints.bin` (5349 mapped codepoints):

| | count |
|---|---:|
| outline (`flags == 0`) | 3576 |
| `FLAG_BLANK` (covered, resolves to slot 0) | 876 |
| **`FLAG_BITMAP`** | **897** |
| distinct layout advance | **2** — `1229` (×4452), `2458` (×897) |
| distinct layout height | **1** — `2320` (×5349) |

`assets/atlas/engine-trie.bin` agrees in world units: advances
`{0.5297414: 4452, 1.0594828: 897}`, heights `{1.0: 5349}`.

Bitmap codepoint ranges (20 runs): `U+269D`, `U+26B9..26BF`, `U+26C4..26E1`,
`U+26E3..2700`, `U+2705`, `U+270A..270B`, `U+2728`, `U+274C`, `U+274E`,
`U+2753..2755`, `U+2757`, `U+275F..2760`, `U+2795..2797`, `U+27B0`, `U+27BF`,
`U+2B1B..2B1E`, `U+2B25..2B52`, `U+2B55..2B57`, `U+2B59..2BFF`, **`U+1F400..1F64F`**
(592).

Spot checks: `U+1F400` 🐀 → slot 3839, advance 2458, `FLAG_BITMAP`. `U+1F600` 😀 →
slot 4351, advance 2458, `FLAG_BITMAP`. `U+1F680` 🚀 → slot 0, `FLAG_MISSING` (above
the baked range). `U+2764` ❤ → slot 3112, advance 1229, **flags 0** — an outline
glyph, because the chain is outline-first for text-default symbols. `U+4E00` and
`U+3042` → `FLAG_MISSING`.

Note the contrast that the load-bearing measurement obscures: the atlas *does* carry
**175 distinct advances** (`glyphs.bin` `advanceFu`) — they are just per-glyph
*normalization denominators*, not layout advances, and `atlas.rs:6` says that record
is parsed for validation and never uploaded.

Visual confirmation: `out/crop_emoji.png` (rendered from `out/test_emoji.txt`,
`emoji: 🐀🚀 between…`) shows the two emoji as blank space of the correct width — the
layout reserves the cells, no ink is drawn.

---

## Difference table

| # | Difference | Bucket |
|---|---|---|
| 1 | Codepoint→glyph resolved at **build time** (one headless replay of the web chain) instead of at runtime; no HarfBuzz wasm, no font files, no boot shaping cost | 1 — better |
| 2 | The exporter **cross-validates its reproduction against the web's own baked envelope** and aborts on drift (slot set, slot count, bitmap cells, mode lane) — a self-check the web does not perform on itself | 1 — better |
| 3 | Trie measures stored as **integer font units** (`codepoints.bin`) rather than the web's bitcast-f32 world units; world scale becomes a native choice, storage is lossless | 1 — better |
| 4 | Trie entry lanes **split by carrier** (`blocks_exact: Vec<u32>` / `blocks_measure: Vec<f32>`) — no bitcasts, a count cannot land in a float array | 1 — better |
| 5 | Emoji set is **closed at export**, so a pre-rendered deterministic emoji sheet is available to native where the web is at the mercy of the host's emoji font | 1 — better (unrealized) |
| 6 | No Canvas2D and no system emoji font in a Rust+wgpu binary — the web's rasterizer cannot be ported as-is | 2 — platform |
| 7 | Fragment output is **premultiplied** with a hard `discard`; the web's non-premultiplied `vColor·cov` + stipple-dither LOD band is not ported | 2 — platform/deliberate |
| 8 | `frame` mode (external video grid) has no native substrate | 2 — platform |
| 9 | Regenerating the atlas hard-depends on the web repo being checked out at `REF_ROOT` | 2 — platform, but worth naming |
| 10 | **No emoji raster**: 897 slots resolve and none render | **3 — missing** |
| 11 | **`emojiCell` never reaches the fragment stage** — `VsOut` has no lane for it | **3 — missing** |
| 12 | **No runtime atlas growth**: no shaper, no fonts in the dependency graph. Anything outside the bake is permanently blank | **3 — missing** |
| 13 | Missing codepoints are reported as an **aggregate count with no identity**; `FORMAT.md`'s "report for live growth" is unimplemented | **3 — missing** |
| 14 | `FLAG_BLANK` (bit 2) is parsed by nothing | **3 — missing** |
| 15 | No highlight / added-colour lane (`vAddedColor` / `vFillAmount`) | 3 — missing |
| 16 | **Two different drop predicates for one concept**: `text.rs` drops on `MISSING\|BITMAP` flags; `layout.rs` drops on `glyph_id == 0`. Emoji vanish before the GPU in one path and reach the shader in the other | **4 — port artifact** |
| 17 | `text.rs` writes a **constant** `advance: cell_w` into the instance, discarding the trie's per-codepoint advance | **4 — port artifact** |
| 18 | Native anchors the emoji square at the pen origin; the web **centres** it in the 2-cell span | **4 — latent artifact** |
| 19 | `FLAG_BLANK` codepoints (876) are counted in neither `missing_or_bitmap` nor `glyphs_emitted` | **4 — accounting hole** |

Bucket counts: **1 — better: 5** (one unrealized) · **2 — platform: 4** ·
**3 — missing: 6** · **4 — worse/artifact: 4**.

---

## Detail

### How a codepoint becomes a drawable glyph

**Web — everything at runtime.**

1. `FontChain.routeCodepoint` (`packages/glyph3d-core/src/shaping/FontChain.js:140-149`)
   tests cmap coverage over the 3-font chain in priority order, first match wins,
   memoized in `_routeCache`. Coverage sets come from `shaper.collectUnicodes()` at
   `init()` (`FontChain.js:122`).
2. `FontChain.shape` (`FontChain.js:225-260`) picks the slot:
   - `cp >= 0x1F000` and an emoji atlas is attached → **bitmap slot first**
     (`FontChain.js:239-242`) — "DejaVu has a mono ☺, but 😀 should be the color one".
   - else HarfBuzz-shape through the routed font → `slotFor(fontIdx, gid)`
     (`FontChain.js:243-246`).
   - else if `isEmojiCodepoint(cp)` (`FontChain.js:51-56`: `1F000–1FAFF`,
     `2600–27BF`, `2B00–2BFF`) → bitmap slot (`FontChain.js:247-250`).
   - else `BLANK_SLOT` (0) — an empty cell, **not** a tofu box
     (`FontChain.js:159`, `:270`).
3. Advance is forced monospace: `_primaryAx()` is the primary font's `'M'` advance
   (`FontChain.js:205-213`), and
   `const ax = this.isBitmapSlot(slot) ? cellAx * 2 : cellAx;` (`FontChain.js:255`).
4. `MonospaceShapeCache` caches `{g, ax}` verbatim (`MonospaceShapeCache.js:76-82`).
5. Outline slots are Slug-encoded (curve texture + glyph-map texel
   `[curveStart, curveCount, 0, 0]`, `slugData.js:158-167`); bitmap slots get a
   **map-only** entry `[0, 0, 1, cell]` (`slugData.js:116-132`) and their pixels
   drawn by `EmojiAtlas` (below).
6. The codepoint→metrics trie is built from the live shape cache
   (`compute/liveTrie.js:31-51` → `compute/GlyphTrie.js:106-179`).
7. Growth is real and continuous — see the fallback-chain section.

**Native — resolved once, at build time.**

1. `tools/export-atlas.mjs` re-runs **the web's own** `FontChain` + `HarfBuzzShaper`
   headlessly (`export-atlas.mjs:101-135`), with the same font list and the stub
   emoji atlas (`:126`), priming `LARGE_CORE_RANGES`.
2. It then **cross-checks the reproduction against the baked slug core** and aborts
   on any drift — slot set (`:150-154`), slot count (`:156-159`), and per-bitmap-slot
   `emojiCell` + `mode == 1` (`:163-173`).
3. Four files are written; `native/src/atlas.rs:216-258` uploads `curves.bin` and
   `glyphmap.bin` verbatim as `Rgba32Uint` and parses the trie CPU-side
   (`TrieTable::load`, `atlas.rs:62-95`, with a sanity assert that `'A'` → slot 34 /
   advance 1229).
4. Lookup is the same two dependent loads as the web kernel
   (`atlas.rs:109-122`).

**Which is better for the native target: the static pre-export, clearly.** It removes
a wasm HarfBuzz, a Canvas2D dependency, three TTFs and all boot-time shaping from a
binary whose whole premise is owned memory and predictable startup; it turns the
atlas into two texture uploads and a `Vec<u32>`. More than that, it is what makes the
engine's bit-exact conformance corpus *possible* — a runtime-shaped atlas has no
fixed answer to compare against. The exporter's abort-on-drift check
(`export-atlas.mjs:150-173`) is a genuinely stronger position than the web's, which
has no equivalent guard that its runtime chain still matches its own bake.

What it forecloses is stated plainly and is the price: **native can never show a
glyph that was not anticipated at export time.** There is no `rustybuzz`,
`ttf-parser`, `swash`, `cosmic-text`, `fontdue` or `ab_glyph` in `native/Cargo.toml`,
and no `.ttf`/`.otf` anywhere under `native/` or `assets/`. CJK, kana, most of the
Nerd-Font PUA and every emoji above `U+1F64F` are `FLAG_MISSING` forever, and the
only growth mechanism is a re-bake that requires the web repo present.

### What the web does to render an emoji

A **separate texture and a separate branch**, sharing nothing with Slug but the
glyph-map texel.

- **Rasterizer**: `packages/glyph3d-core/src/EmojiAtlas.js` — a Canvas2D grid,
  `cellPx = 72`, `cols = 16` (square, `EmojiAtlas.js:40-43`), drawn with `fillText`
  (`EmojiAtlas.js:139-151`) over the CSS stack
  `"Noto Color Emoji","Apple Color Emoji","Segoe UI Emoji","Twemoji Mozilla"`
  (`EmojiAtlas.js:44-45`). No COLRv1/CBDT/sbix parsing — it delegates entirely to the
  host's emoji font. Growth doubles both axes and **re-creates** the texture
  (`EmojiAtlas.js:110-137`), capped at `MAX_ATLAS_DIM_PX = 9216`
  (`EmojiAtlas.js:31`).
- **Texture**: a filterable `THREE.CanvasTexture` (`EmojiAtlas.js:162-175`),
  `flipY = false`, `LinearFilter` — distinct in kind from the Slug
  `RGBAIntegerFormat` `DataTexture`s. `GlyphField` resolves it through **live
  getters** every draw (`GlyphField.js:1073-1080`) so an atlas growth that disposes
  the old texture can never strand a field.
- **Vertex**: `core/glyphVertex.js:243-254` loads the glyph-map texel and takes
  `.z` as mode, `.w` as `emojiCell`; `:259-261` forces the square quad —
  `const quadW = isBitmap.select(iSize.y, iSize.x);` — while `alignOffset` keeps the
  full (double) `iSize.x * 0.5` (`:268`), which is what **centres** the square inside
  its 2-cell span.
- **Fragment**: `GlyphField.js:289-316`. The mode branch precedes the empty-glyph
  discard (`GlyphField.js:342`), which is mandatory — a bitmap slot has
  `curveCount == 0` and would otherwise be discarded as empty. Cell→UV is pure
  arithmetic, with an active V flip against the `flipY = false` canvas:

  ```js
  const col = float(vEmojiCell).mod(emojiCols);
  const row = float(vEmojiCell).div(emojiCols).floor();
  const atlasUV = vec2(
      col.add(vGlyphUV.x).div(emojiCols),
      row.add(float(1).sub(vGlyphUV.y)).div(emojiRows)
  );
  const texel = emojiTex.sample(atlasUV);
  Discard(texel.a.lessThan(0.01));
  outColor.assign(vec4(texel.rgb.pow(vec3(2.2)), texel.a.mul(vGroupAlpha)));
  ```

- **Colour**: emoji **ignore `instanceColor`/tint entirely** — `vColor` is not
  referenced in that branch. Only the bitmap's own RGBA and `vGroupAlpha` apply, so
  group fade/hide still works on emoji, per-glyph tint does not. Contrast the Slug
  branch's explicit `vColor.mul(cov)` at `GlyphField.js:427`.

**What native would need.** The vertex half is already done and correct — mode is
read (`glyph_field.wgsl:117`) and the square quad applied
(`glyph_field.wgsl:119-124`). Missing:

1. **A raster source.** Canvas2D and a system emoji font do not exist here. Because
   the emoji set is *closed at export* (897 known cells with indices already in
   `glyphs.bin`/`glyphmap.bin`), the natural native answer is a fifth export file: a
   pre-rendered RGBA8 sheet of exactly those cells. That is strictly better than the
   web's approach — deterministic and free of host-font variance — and it is bucket 1
   territory the port has simply not reached yet.
2. **A filterable texture + sampler binding.** The current bind group is six entries,
   all non-filtering integer textures (`glyph_field.wgsl:78-83`).
3. **`emojiCell` as a varying.** `VsOut` (`glyph_field.wgsl:85-93`) carries
   `curve_start`, `curve_count`, `mode` — and *not* `emojiCell`. `info.w` is loaded
   at `:116` and never read. This is a one-line schema gap that blocks everything
   downstream.
4. **The fragment branch.** Replace the `discard` at `glyph_field.wgsl:262-264` with
   the cell→UV math above, adjusting the V flip to the sheet's orientation and
   emitting **premultiplied** output to match this pipeline
   (`glyph_field.wgsl:307-313`) rather than copying the web's non-premultiplied line.
5. **Stop dropping bitmap glyphs at staging** (`text.rs:143`); the repo/fold path
   already emits them.

### Metrics: where `advance` and `height` come from

| | web outline | web emoji | native outline | native emoji |
|---|---|---|---|---|
| layout advance | primary `'M'` advance, forced (`FontChain.js:255`) | **2×** that, same line | `1229` fu → `0.5297414` world | **`2458` fu → `1.0594828` world** |
| stored where | per-codepoint, `GlyphTrie.blocksMeasure` (`Float32Array`) | same | per-codepoint, `codepoints.bin` lane 1 (int fu) / `engine-trie.bin` (f32 world) | same |
| quad height | one global constant `hWorld` (`liveTrie.js:38`) | same | `CELL_HEIGHT_WORLD = 1.0` (`text.rs:21`) | same |
| quad width | `iSize.x` | `iSize.y` — square (`glyphVertex.js:260`) | `inst.advance` | `inst.height` — square (`wgsl:121-124`) |
| glyph's own-font advance | `FontChain.glyphAdvance(slot)` — Slug normalization only | n/a | `glyphs.bin` `advanceFu`, **175 distinct** — normalization only, never uploaded (`atlas.rs:6`) | 0 |

So: metrics are **per-codepoint data in both trees**, carried in the trie, not a
per-slot constant and not recomputed downstream. The answer differs for emoji only in
value (double advance) and in how the quad consumes it (square, not advance-wide).
`compute/GlyphLayoutKernel.js:24-25` states the invariant the web depends on and the
native fold inherits: *"X IS A LOOKUP, NOT A MULTIPLY. A color-emoji codepoint
occupies ONE slot but advances TWO cells, so `col × cellWidth` is wrong for every
glyph"*.

The native fold honours this — `layout.rs:583-584` writes `record.advance()` /
`record.height()` per instance, and the engine fixture corpus exercises the doubling
(`native/src/fixture.rs:801-802`, `engine/fixtures/gen.mjs:68-69`, fixtures
`utf8-emoji` and `wrap-emoji`) with *per-glyph varying heights* the real atlas never
produces. The simple `text.rs` path does **not** honour it: `text.rs:267` hardcodes
`advance: cell_w` into the instance. Today that is invisible because the same
function has already dropped every double-advance glyph three lines earlier.

### The fallback chain

**Web: a live 5-stage chain.** Cousine → MesloLGS NF Mono → DejaVu Sans → colour
bitmap → blank slot 0, with the emoji-plane preference inverting the first two stages
for `cp >= 0x1F000` (`FontChain.js:225-260`). A codepoint no stage covers is an empty
cell of one cell width — never a tofu box.

Growth is real, and there are two paths:

- **Synchronous** — `LiveSlugAtlas.ensureCodepoints` (`LiveSlugAtlas.js:227-238`) →
  `ensureGlyphsEncoded` (`:108-192`), which partitions bitmap vs outline slots,
  re-encodes, rebuilds both `DataTexture`s at the new size, hot-swaps every
  registered field (`:141-149`) and disposes the orphans (`:156-163`) — all before
  returning, so the caller's next instance-buffer write is already renderable.
  Callers: `TerminalGrid.js:450,601,1453`, `ContentTreeLabels.js:450`.
- **Asynchronous** — the GPU byte pipeline reads back the kernel's *reported misses*
  and grows off the load path (`compute/GlyphPipelineArena.js:558-583`):
  `readMisses()` → `encodeMisses(atlas, misses)` (`compute/liveTrie.js:60-71`) →
  rebuild the trie → realloc kernels → re-flush, with a generation guard. The comment
  at `:557-558` states why this is safe to defer: *"the layout is already correct
  (missing entries occupy their advance)"*.

**Native: no chain at runtime.** The chain's *results* are baked — `glyphs.bin` records
a `fontIdx` per slot (1699 Cousine / 957 Meslo / 877 DejaVu / 897 bitmap / 1 blank) —
but there is no routing, no cmap, no shaper and no font file. The chain was collapsed
into a lookup table at export time and cannot be re-entered.

**What happens to a codepoint with no glyph.** The trie returns `glyph_id = 0`,
`FLAG_MISSING`, advance = one cell, height = one em (`atlas.rs:109-122`;
`glyph_trie.rs:97-110` and its test `a_miss_is_a_value_not_a_failure`). Layout is
therefore **correct** — the cell is occupied, columns stay aligned, and
`layout.rs:872` pins that a blank's advance still widens the page.

Rendering is a drop, and reporting is where it goes quiet:

- `text.rs:143-152` counts it into `missing_or_bitmap` and emits nothing;
  `main.rs:196-201` prints the **count**.
- `repo.rs:707-714` prints `"{n} blank/missing dropped"`.
- **No path records which codepoints were missing.** `FORMAT.md`'s specification of
  `FLAG_MISSING` — *"render blank, keep the advance, report for live growth"* — has
  its first two clauses implemented and its third not. The web's equivalent
  (`readMisses()`) returns identities and is the thing that drives the encode.
- `FLAG_BLANK` (bit 2) is consumed by nothing; `atlas.rs:21-23` says so outright. In
  `text.rs` a `FLAG_BLANK` codepoint matches neither the flags test nor
  `glyph_id != 0`, so all 876 of them fall out of **both** counters — rendered
  correctly, accounted nowhere.

So the behaviour is *correct in layout and quiet in diagnosis*. Given that native has
no growth path to act on the information, the quiet is currently harmless — but it is
also exactly the information a future re-bake would need, and it is being discarded
per-run.

### Two drop predicates, one concept

Worth naming on its own because it is the kind of divergence that reads as fine in
each file:

- `text.rs:143` drops on **flags**: `entry.flags & (FLAG_MISSING | FLAG_BITMAP)`.
  Emoji never reach the GPU.
- `layout.rs:564` drops on **identity**: `record.glyph_id() == 0`. Emoji have
  non-zero slots (3839, 4351, …), so in the repo/fold path they *are* emitted, with
  the correct double advance, and reach `fs_main` — where `glyph_field.wgsl:262-264`
  discards them.

The shader comment calls its own branch "belt-and-braces". In the repo path it is not
belt-and-braces; it is the only thing stopping 897 slots' worth of garbage. Both
answers are defensible; having both is the problem, and it will bite whoever wires up
the emoji sheet and finds emoji appear in one scene mode and not the other.

### One latent divergence to fix at the same time

Web: `positionLocal ∈ [-0.5, 0.5]`, `scaled.x = positionLocal.x · quadW`,
`alignOffset.x = iSize.x · 0.5` (`glyphVertex.js:266-268`) → the quad is centred at
`advance/2`. For an outline glyph (`quadW == advance`) that is `[0, advance]`; for an
emoji it centres the square in the 2-cell span, `centre = 1.0594828/2 = 0.5297414`.

Native: `corners ∈ [0,1]`, `aligned.x = c.x · quad_w` (`glyph_field.wgsl:126-129`) →
the quad starts at the pen origin. For an outline glyph that is identical; for an
emoji the square spans `[0, 1.0]`, `centre = 0.5`.

A left shift of `0.0297` world units — about 5.6 % of a cell. Invisible today,
guaranteed to be visible the moment the discard is removed.

---

## Files cited

Web (read-only):
`packages/glyph3d-core/src/shaping/FontChain.js`,
`packages/glyph3d-core/src/EmojiAtlas.js`,
`packages/glyph3d-core/src/GlyphField.js`,
`packages/glyph3d-core/src/core/glyphVertex.js`,
`packages/glyph3d-core/src/shaping/slugData.js`,
`packages/glyph3d-core/src/shaping/SlugEncoder.js`,
`packages/glyph3d-core/src/shaping/LiveSlugAtlas.js`,
`packages/glyph3d-core/src/shaping/MonospaceShapeCache.js`,
`packages/glyph3d-core/src/compute/GlyphTrie.js`,
`packages/glyph3d-core/src/compute/liveTrie.js`,
`packages/glyph3d-core/src/compute/GlyphPipelineArena.js`,
`packages/glyph3d-core/src/compute/GlyphLayoutKernel.js`,
`packages/glyph3d-r3f/src/coreRanges.js`,
`tools/headlessFontChain.mjs`.

Native:
`tools/export-atlas.mjs`, `tools/verify_atlas.py`, `tools/gen_real_trie.py`,
`assets/atlas/FORMAT.md`, `assets/atlas/{curves,glyphmap,glyphs,codepoints,engine-trie}.bin`,
`native/src/atlas.rs`, `native/src/glyph_trie.rs`, `native/src/text.rs`,
`native/src/layout.rs`, `native/src/repo.rs`, `native/src/main.rs`,
`native/src/shaders/glyph_field.wgsl`, `native/Cargo.toml`,
`engine/glyph_schema.mojo`, `engine/glyph_pipeline.mojo`, `engine/PLAN-DRAFT.md`,
`engine/fixtures/gen.mjs`, `native/src/fixture.rs`, `out/crop_emoji.png`.
