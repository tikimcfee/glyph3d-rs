# Work package D1: the cell renderer, `native/src/wall/`

You are building one piece of a larger port, and it is the piece on the critical path.
Read `out/WALL-PLAN-2026-11-26.md` first (the goal, the pieces, and the rules everyone
works under), then this.

## What this piece is

The wall is an infinite canvas of live terminals, drawn by this engine's own renderer
(wgpu), not by a UI toolkit. A terminal is a grid of cells; an emulator elsewhere
turns a terminal's bytes into a `term_cells::Screen`. Your job is to draw a `Screen`:
a rectangle in world space, under an orthographic camera, with exact Slug text at any
zoom. Several of them at once, each at its own position and depth.

The first milestone of the whole run is one live terminal a person can type into. This
piece is what makes it visible, so a plain, correct renderer today beats a complete
one next week. Do it in the order below and commit at each step.

## Why it is its own pass

The engine's glyph field is built for source trees: tens of millions of glyphs laid
out once, in 256K-slot blocks. A terminal is the opposite: a few thousand cells that
change constantly, created and destroyed at will. So a terminal does not go in the
field. Each one owns a small buffer of cells, and the cell's position comes from its
index in the grid (column = i mod cols, row = i div cols), so a cell record carries no
position at all.

## What to read

- `crates/term-cells/src/lib.rs`: your input.
- The engine, to reuse and not reinvent: `native/src/gpu.rs` (`GpuContext`),
  `native/src/offscreen.rs` (how `--screenshot` renders a frame to a PNG with no
  window), `native/src/glyph_scene/{setup,pipelines,render,target,camera}.rs` (targets,
  formats, reverse-Z depth, how a pass is recorded), `native/src/atlas.rs` (the glyph
  atlas: codepoint to glyph id, per-glyph metrics, the curve and glyph-map data, the
  emoji sheet), `crates/glyph-field-derived/` (the glyph shader,
  `shaders/glyph_field_derived.wgsl`, and how its atlas textures and bind groups are
  made), `native/src/cli/` (how a flag is added), `native/src/scene.rs` (the quad demo
  is a small scene with its own renderer).
- How the JS app draws the same thing (read-only):
  `/Users/lugo/localdev/viz-web/glyph3d-js/docs/architecture/rendering.md`, "Cells:
  fills, styles, sprites" and "Colour"; `packages/glyph3d-core/src/core/glyphMaterial.js`
  (the fill pass, the style byte, the line bands, the greek tier);
  `packages/glyph3d-core/src/collections/TerminalGrid.js` (the cursor, the panel and
  border). The look to match is that one.
- `out/wall-port/review-carbon-2026-11-23.md`, "Risk 4" and "Risk 6": two shader
  defects a reviewer found (coverage needs `abs()` for fonts that wind contours both
  ways; and an sRGB decode that does not match its encode), and why the engine's ray
  pick and distance LOD do not apply under an orthographic camera.

## What to build, in this order

1. **A test pattern you can see.** `glyph3d-native --wall-test-pattern --screenshot
   OUT.png` renders, offscreen, a `Screen` built in code: a few lines of text in
   several colours, cells with fills, each underline style, a strikethrough, dim text,
   a wide emoji, and a cursor. Look at the PNG yourself after every step; it is your
   feedback loop.
2. **The cell pass.** A wgpu pipeline and shader (`native/src/shaders/wall_cells.wgsl`)
   drawing one terminal from its cell buffer:
   - the terminal's own background panel and a border;
   - fills: each cell with a `bg` draws its whole rectangle, flat, before any ink, and
     never over a neighbour's glyph;
   - ink: the Slug coverage of the engine's glyph shader, taking `abs(coverage)`,
     glyphs from the existing atlas (`assets/atlas/`: one face, Cousine; bold and
     italic draw as regular for now), a wide glyph spanning two cells, colour emoji
     from the sheet;
   - lines: underline (single, double, curly, dotted, dashed), strikethrough and
     overline as bands across the whole cell so a run reads as one line; dim;
   - the cursor: block, bar or underline, hollow when the terminal is not focused.
   The cell's size is the atlas's cell, snapped as the JS does so neighbours share
   their edge exactly.
3. **The orthographic camera** (`camera.rs`): a centre and a zoom, its view-projection,
   world to screen and back, zoom about a screen point. Depth still works: a terminal
   has a z, and a nearer one covers a farther one.
4. **Many terminals.** A `WallRenderer` that owns the pipelines and the shared atlas
   resources and draws a list of `{ grid, world position, z, focused }`. A terminal's
   buffer is uploaded only when its `Screen::generation` moved, and then only the rows
   whose `row_gen` did. The test pattern grows to three terminals overlapping.
5. **Replay.** `--wall-replay FILE [--wall-size COLSxROWS]` draws a recorded terminal
   byte stream through `crates/glyph-vt` (another agent is building it; its seam is in
   the plan, section B). If it has not landed when you get here, leave the flag
   behind a clear "not built yet" error and say so.

Keep the renderer free of winit: it draws into a target it is given. The window and
input are the next package.

## What not to do

- Do not put terminals in the glyph field, and do not change the glyph field, its
  shader, or anything a golden frame is drawn by. This pass is new code beside them.
- Do not build fonts. Glyphs come from the existing atlas for now; `crates/glyph-font`
  will replace the source later, so keep "which glyph id draws this cell" in one
  function.

## How you will know it works

Your own eyes on the PNG at each step, plus tests where a number is checkable: the
camera's round trips, the cell-to-rectangle arithmetic, the packing of a `Cell` into
its GPU record, the dirty-row upload ranges. `cargo build --release -p glyph3d-native`
with zero warnings. The existing test suite still passes (`cargo test --release -p
glyph3d-native`): you changed nothing it covers, so a failure there is information.

## Handing back

Work in this worktree on its branch; commit at each step. Your final reply: which
steps are done, the commands that produce the PNGs and what they show (say honestly
what looks wrong), the `WallRenderer` API as built (the app's author reads that
part), and what you would do next.
