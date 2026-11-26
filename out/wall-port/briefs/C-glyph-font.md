# Work package C: `crates/glyph-font`

You are building one piece of a larger port. Read `out/WALL-PLAN-2026-11-26.md` first
(the goal, the pieces, and the rules everyone works under), then this.

## What this piece is

glyph3d draws text with the Slug technique: each glyph's outline is stored as
quadratic curves in a texture, and the fragment shader computes exact coverage from
them at any size. Today those curves come from a file baked once by a JavaScript
encoder in another repository; this repository can only repack that file. So it cannot
draw a font it was not handed: no bold, no italic, no Monaspace, nothing new at run
time.

This crate ends that. It loads font files at run time and encodes glyphs into the same
curve format the engine's shader already reads, into an atlas that grows as glyphs are
first used. The wall's terminals are its first user; later the rest of the engine can
take any new text through it.

## What to read

- `assets/atlas/FORMAT.md`: the byte-level format of the baked atlas, and above all
  its "Coordinate conventions" (curves normalised per glyph CELL: x over the advance,
  y from descender to ascender, y up, u16-packed) and the glyph-map entry layout.
- `native/src/atlas.rs`: how the engine loads and holds the atlas.
- `crates/glyph-field-derived/shaders/glyph_field_derived.wgsl`: the consumer. How it
  finds a glyph's curves through the glyph map and what it expects of them. Your
  output must draw correctly through this code unchanged (one known gap aside: it
  clamps `coverage` where the JS shader takes `abs(coverage)` first; that is the
  renderer's to fix, but tell the lead if your curves depend on winding direction).
- The encoder you are porting, in the JS repository (read-only):
  `/Users/lugo/localdev/viz-web/glyph3d-js/packages/glyph3d-core/src/shaping/slugData.js`
  (outline to curves), `SlugEncoder` and `LiveSlugAtlas.js` beside it (how the live
  atlas grows), `FontChain.js` (families, faces, fallback routing),
  `core/cellMetrics.js` (the cell's size and why it is snapped), and
  `fonts/families.js` (which fonts, which variable-font instances).
  `docs/architecture/rendering.md`, "Fonts" and "Cells", explains the design.
- Fonts: `tools/vendor/ref/packages/glyph3d-core/src/fonts/Cousine-Regular.ttf` here
  is the font the baked atlas was made from. The JS repository's
  `packages/glyph3d-core/src/fonts/` has the rest (Cousine's four faces, Monaspace
  Neon and Argon as variable fonts, MesloLGS NF Mono, DejaVu Sans) with their licences.

## What to build

1. **The encoder.** A glyph of a font face, at a variable-font instance if the font
   has axes, to its curves in the engine's packed form, normalised to the family's
   cell. TrueType outlines are quadratic already; CFF outlines are cubic and need
   converting. `ttf-parser` 0.25 is already in this workspace's lock file.
2. **Cell and line metrics** of a family: advance, ascender, descender, line gap,
   underline and strikethrough position and thickness, as the JS computes them.
3. **A growable atlas.** `glyph(family, face, codepoint) -> slot`, encoding on first
   use and routing through the family's fallback fonts when the first lacks the glyph;
   a missing glyph has a stable answer. It holds the curve texels and glyph-map entries
   in the layout the shader reads, and hands out the ranges that changed since they
   were last taken, so a renderer uploads only those.
4. **A font set** to start with: copy the font files the wall needs (Monaspace Neon,
   Cousine's four faces, the two fallbacks) and their licence files into
   `assets/fonts/`, and define the default families as `fonts/families.js` does.

Cell sprites (box drawing and the like), combining-mark clusters and colour emoji are
not in this package. Leave room for them: a slot is just a number, and other sources
will hand out slots too.

## How you will know it works

The baked atlas is your oracle. For Cousine Regular, encode a set of glyphs (all of
printable ASCII, a few accented letters, a few box-drawing characters) and compare
with what `assets/atlas/` holds for the same glyphs: the same number of curves, and
coordinates equal or within a stated tolerance. Where they differ, find out why and
say so; do not widen the tolerance until it passes. A second test: a variable-font
instance (Monaspace Neon at `wght` 700) encodes to different curves than at 400, and
its bold glyphs have more ink. A third: the atlas grows past its first allocation and
earlier slots still read back unchanged.

If an example helps you see your own output, a tiny CPU rasteriser of the curves to a
PGM or PNG is worth an hour: the shader's coverage maths is short.

## Handing back

Work in this worktree on its branch; commit as you go. Your final reply: what works
(with the test output), how close the oracle comparison came and what explains the
rest, what you left out, and the API as built (the renderer's author will read that
part).
