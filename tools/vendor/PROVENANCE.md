# Vendored files — provenance

Everything in this tree that was copied from somewhere else, with where it
came from and its hash. Most of it is from `viz-web/glyph3d-js`; the
colour-emoji font is from its own upstream (see "Third-party" below).
Recorded because a vendored file with no recorded origin is indistinguishable
from a local invention six months later.

  upstream repo    viz-web/glyph3d-js
  upstream commit  2ef79b7e762a07ebbf72713f97527a6831b7a839
  regenerated      2026-09-20  (tools/vendor-manifest.py)

## The fixture oracle is pinned PER FILE

`engine/fixtures/{gen,gen-bake}.mjs` reproduce all 25 committed
fixtures (17 `.pipe.bin` + 8 `.bake.bin`; counts read from
`build.toml [artifact.fixtures]`) byte-for-byte, in this tree, with no web
repo present — but only from these
revisions. Today's upstream cannot: `glyphPipelineReference.js` stopped
exporting `FLOAT_LANES` at 3da6542 and the generators do not even load.

  engine/fixtures/inputs/GlyphTrie.js                  70ce30e
  engine/fixtures/inputs/foldGeometry.js               70ce30e
  engine/fixtures/inputs/glyphBake.js                  70ce30e
  engine/fixtures/inputs/glyphPipelineKernels.js       59a2a44
  engine/fixtures/inputs/glyphPipelineReference.js     70ce30e
  engine/fixtures/inputs/glyphPipelineScan.js          70ce30e

A single "upstream commit" is the wrong model for these files and hid a real
defect: the `glyphPipelineKernels.js` vendored here until 2026-09-04 was
78,567 bytes, but `real-kernels.pipe.bin` embeds 90,515 — the file at 59a2a44.
The old copy matched its own recorded hash on every `--check` run. The gate
asked whether the file had been edited locally; it could not ask whether it
was the file the corpus came from. Byte-identical regeneration of the whole
corpus from these inputs — run by the battery's fixture gate on every pass —
is that check.

## Verifying

  LOCAL drift (did someone edit a vendored copy?) — no web repo needed:
      python3 tools/vendor-manifest.py --check
  This runs in the battery's generator-reproduction gate (`tools/check-all.sh`
  step 1; build.toml gate `vendor-hashes`).

  UPSTREAM drift (did the web repo move on?) — needs the web repo present:
      python3 tools/vendor-manifest.py     and read the last column.
  A difference is INFORMATION, not a failure: the native tree is trunk and is
  not obliged to track the web repo. Refreshing is a decision, never a reflex.

## What is here

| local path | upstream path | bytes | sha256 (local) | upstream matches |
|---|---|---:|---|:--:|
| `engine/fixtures/inputs/GlyphTrie.js` | `packages/glyph3d-core/src/compute/GlyphTrie.js` | 9498 | `288dcb3cdac888b7…` | MISMATCH |
| `engine/fixtures/inputs/foldGeometry.js` | `packages/glyph3d-core/src/core/foldGeometry.js` | 14806 | `c925c6d2c8036f2b…` | yes |
| `engine/fixtures/inputs/glyphBake.js` | `packages/glyph3d-core/src/compute/glyphBake.js` | 11761 | `5ba631bdbd6ed5e5…` | MISMATCH |
| `engine/fixtures/inputs/glyphPipelineKernels.js` | `packages/glyph3d-core/src/compute/glyphPipelineKernels.js` | 90515 | `c785a572091d2f37…` | MISMATCH |
| `engine/fixtures/inputs/glyphPipelineReference.js` | `packages/glyph3d-core/src/compute/glyphPipelineReference.js` | 51879 | `dd2bc84bfc3cd1be…` | MISMATCH |
| `engine/fixtures/inputs/glyphPipelineScan.js` | `packages/glyph3d-core/src/compute/glyphPipelineScan.js` | 15254 | `4b83b7447b66695e…` | MISMATCH |
| `schema/glyph-identity.json` | `schema/glyph-identity.json` | 19158 | `b4b838430677b383…` | MISMATCH |
| `tools/vendor/ref/app/public/slug-core/slug-core.1tstke3lync.bin` | `app/public/slug-core/slug-core.1tstke3lync.bin` | 609569 | `647ccdaba087de1f…` | yes |
| `tools/vendor/ref/packages/glyph3d-core/src/fonts/Cousine-Regular.ttf` | `packages/glyph3d-core/src/fonts/Cousine-Regular.ttf` | 300208 | `dcd526004fcfec4e…` | yes |
| `tools/vendor/ref/packages/glyph3d-core/src/fonts/DejaVuSans.ttf` | `packages/glyph3d-core/src/fonts/DejaVuSans.ttf` | 759720 | `6038a160b491e121…` | yes |
| `tools/vendor/ref/packages/glyph3d-core/src/fonts/MesloLGS-NF-Mono.ttf` | `packages/glyph3d-core/src/fonts/MesloLGS-NF-Mono.ttf` | 2831664 | `3cb52e923ca3981c…` | yes |
| `tools/vendor/ref/packages/glyph3d-core/src/shaping/FontChain.js` | `packages/glyph3d-core/src/shaping/FontChain.js` | 14522 | `5ac242c70910d059…` | yes |
| `tools/vendor/ref/packages/glyph3d-core/src/shaping/HarfBuzzShaper.js` | `packages/glyph3d-core/src/shaping/HarfBuzzShaper.js` | 7266 | `dfc4bbaba2639428…` | yes |
| `tools/vendor/ref/packages/glyph3d-core/src/shaping/MonospaceShapeCache.js` | `packages/glyph3d-core/src/shaping/MonospaceShapeCache.js` | 7320 | `c38a3ed428af522a…` | yes |
| `tools/vendor/ref/packages/glyph3d-core/src/shaping/vendor/harfbuzz.js` | `packages/glyph3d-core/src/shaping/vendor/harfbuzz.js` | 1186 | `9958483ddc5ae4a4…` | yes |
| `tools/vendor/ref/packages/glyph3d-core/src/shaping/vendor/hb.js` | `packages/glyph3d-core/src/shaping/vendor/hb.js` | 24522 | `79bc0bd8aa25ca23…` | yes |
| `tools/vendor/ref/packages/glyph3d-core/src/shaping/vendor/hb.wasm` | `packages/glyph3d-core/src/shaping/vendor/hb.wasm` | 397190 | `ea319787a8efdf90…` | yes |
| `tools/vendor/ref/packages/glyph3d-core/src/shaping/vendor/hbjs.js` | `packages/glyph3d-core/src/shaping/vendor/hbjs.js` | 55836 | `4ef474e7567c2f2a…` | yes |
| `tools/vendor/ref/packages/glyph3d-r3f/src/coreRanges.js` | `packages/glyph3d-r3f/src/coreRanges.js` | 2874 | `33dbad8e14c5ecc0…` | yes |
| `tools/vendor/ref/tools/headlessFontChain.mjs` | `tools/headlessFontChain.mjs` | 3752 | `6f49cde4cc655eb2…` | yes |

## Derived, not copied: `tools/vendor/hb.*`

These two are not upstream mirror entries — each is DERIVED from a vendored
ref file by the stated transformation, and `--check` re-runs that derivation
in memory on every pass. The bare hash catches a local edit; the
re-derivation catches the ref copy and the derived file drifting apart (e.g.
a refreshed ref with a stale derived twin). They exist because
`tools/export-atlas.mjs` (:87-91) must `require()` HarfBuzz from Node >= 22,
which refuses the ref `hb.js` — UMD with a trailing ESM `export default`
(ERR_AMBIGUOUS_MODULE_SYNTAX; Bun, the web repo's bake runtime, accepts it).

| local path | derived from | transformation | bytes | sha256 (local) | derivation holds |
|---|---|---|---:|---|:--:|
| `tools/vendor/hb.cjs` | `tools/vendor/ref/packages/glyph3d-core/src/shaping/vendor/hb.js` | drop the trailing `export default createHarfBuzz;` line | 24491 | `a3550bf1fc195c22…` | yes |
| `tools/vendor/hb.wasm` | `tools/vendor/ref/packages/glyph3d-core/src/shaping/vendor/hb.wasm` | verbatim copy | 397190 | `ea319787a8efdf90…` | yes |

## Third-party: not from the web repo

Copied from another project at a RELEASE TAG and pinned to the commit that tag
resolved to on the day. There is no "upstream matches" column: a tag does not
move, and `--check` additionally requires SHA256SUMS to agree with the fetch
hash recorded in `THIRD_PARTY`, so a re-vendor that forgot the record is caught.

| local path | project | tag | commit | bytes | sha256 | licence |
|---|---|---|---|---:|---|---|
| `tools/vendor/third-party/noto-emoji/LICENSE.txt` | googlefonts/noto-emoji | `v2.051` | `6202fe7c20dd` | 4301 | `6a73f9541c2de741…` | the OFL text itself |
| `tools/vendor/third-party/noto-emoji/NotoColorEmoji.ttf` | googlefonts/noto-emoji | `v2.051` | `6202fe7c20dd` | 10673480 | `72a635cb3d2f3524…` | SIL Open Font License 1.1 (LICENSE.txt beside it, from the same tag) |
| `tools/vendor/third-party/unicode-ucd/GraphemeBreakProperty.txt` | unicode.org UCD | `17.0.0` | `release-dir` | 99377 | `d6b51d1d2ae5c33b…` | Unicode Terms of Use (license.txt beside it) |
| `tools/vendor/third-party/unicode-ucd/emoji-data.txt` | unicode.org UCD | `17.0.0` | `release-dir` | 107324 | `2cb2bb9455cda83e…` | Unicode Terms of Use (license.txt beside it) |
| `tools/vendor/third-party/unicode-ucd/license.txt` | unicode.org UCD | `17.0.0` | `release-dir` | 1995 | `e7a93b009565cfce…` | the Unicode Terms of Use text itself |

- `tools/vendor/third-party/noto-emoji/LICENSE.txt` — the font's licence travels with the font
- `tools/vendor/third-party/noto-emoji/NotoColorEmoji.ttf` — the colour-emoji bitmap source for assets/atlas/emoji-sheet.bin; CBDT/CBLC, one 109 ppem strike, 3,985 PNG glyphs of 136x128
- `tools/vendor/third-party/unicode-ucd/GraphemeBreakProperty.txt` — Grapheme_Cluster_Break classes for the cluster-mode segmentation rule — the source tools/gen_cluster_table.py reads
- `tools/vendor/third-party/unicode-ucd/emoji-data.txt` — Extended_Pictographic / Emoji_Modifier / keycap classes for the same generator — cluster head/trailer candidacy
- `tools/vendor/third-party/unicode-ucd/license.txt` — the licence travels with the data

## Notes

- `tools/vendor/ref/**` mirrors the web repo's LAYOUT exactly so every file stays
  byte-verbatim and `export-atlas.mjs` needs only one path constant. Do not edit
  these; a vendored file you edited is a fork you did not declare, which is what
  the `--check` gate exists to catch.
- `engine/fixtures/inputs/foldGeometry.js` and `glyphPipelineKernels.js` are
  FIXTURE CORPUS inputs, and they ARE read: `engine/fixtures/gen.mjs` loads
  `inputs/foldGeometry.js` at :93 (the `repo-file` fixture's source bytes) and
  `inputs/glyphPipelineKernels.js` at :175 (the `real-kernels` fixture). The
  generators DO run in this tree — `engine/glyph_schema.mjs` is emitted here by
  `tools/gen_schema.py` — and the battery's fixture gate regenerates the whole
  corpus from these inputs BYTE-IDENTICALLY on every pass. They are vendored so
  the corpus input is frozen at the pinned revisions above rather than drifting
  with upstream: today's upstream `glyphPipelineKernels.js` differs from the
  59a2a44 pin (`3da6542` landed after it), so an unpinned input would regenerate
  a DIFFERENT corpus and redden that gate.
- `schema/glyph-identity.json` is the source of truth for `tools/gen_schema.py`.
