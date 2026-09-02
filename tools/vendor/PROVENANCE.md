# Vendored from the web repo — provenance

Everything in this tree that was copied out of `viz-web/glyph3d-js`, with the
commit it came from and its hash. Recorded because a vendored file with no
recorded origin is indistinguishable from a local invention six months later.

  upstream repo    viz-web/glyph3d-js
  upstream commit  2ef79b7e762a07ebbf72713f97527a6831b7a839
  regenerated      2026-09-02  (tools/vendor-manifest.py)

## Verifying

  LOCAL drift (did someone edit a vendored copy?) — no web repo needed:
      python3 tools/vendor-manifest.py --check
  This runs as part of `tools/check-all.sh` gate 1.

  UPSTREAM drift (did the web repo move on?) — needs the web repo present:
      python3 tools/vendor-manifest.py     and read the last column.
  A difference is INFORMATION, not a failure: the native tree is trunk and is
  not obliged to track the web repo. Refreshing is a decision, never a reflex.

## What is here

| local path | upstream path | bytes | sha256 (local) | upstream matches |
|---|---|---:|---|:--:|
| `engine/fixtures/inputs/foldGeometry.js` | `packages/glyph3d-core/src/core/foldGeometry.js` | 14806 | `c925c6d2c8036f2b…` | yes |
| `engine/fixtures/inputs/glyphPipelineKernels.js` | `packages/glyph3d-core/src/compute/glyphPipelineKernels.js` | 78567 | `9cd528affba50d7d…` | yes |
| `schema/glyph-identity.json` | `schema/glyph-identity.json` | 18233 | `b27a4f0cb6ae7ecd…` | yes |
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

## Notes

- `tools/vendor/ref/**` mirrors the web repo's LAYOUT exactly so every file stays
  byte-verbatim and `export-atlas.mjs` needs only one path constant. Do not edit
  these; a vendored file you edited is a fork you did not declare, which is what
  the `--check` gate exists to catch.
- `engine/fixtures/inputs/foldGeometry.js` and `glyphPipelineKernels.js` are
  FIXTURE CORPUS inputs, not code. The fixture generators read live web-repo
  source for these two; `minified-sample.js` was already vendored for exactly
  this reason and these two complete the set. NOTHING READS THEM YET — the
  generators cannot run in this tree (they need a `packages/` path that does not
  exist here, and `engine/glyph_schema.mjs`, which this tree does not emit).
  They are vendored now so the corpus input is frozen at a known commit rather
  than drifting until someone gets to the Rust port.
- The committed fixtures CANNOT be reproduced from these inputs:
  `glyphPipelineKernels.js` changed upstream in `3da6542` after those fixtures
  were generated. Regeneration will produce a NEW corpus, deliberately. The old
  bytes live in git history, which is where superseded evidence belongs.
- `schema/glyph-identity.json` is the source of truth for `tools/gen_schema.py`.
