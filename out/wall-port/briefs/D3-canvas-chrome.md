# Work package D3: what the canvas draws besides terminals

D1 is merged: the cell pass, the camera, many terminals. The lead has since put a
window in front of it (`native/src/wall/app.rs`: winit, pan and zoom, focus, drawn on
demand; `--wall --wall-test-pattern` opens it, `--frames N` makes it draw N frames and
exit). This package is the rest of what the JS wall draws IN the scene, so the window
can become the wall: the paper, the bars, the frames and the marks. Read
`out/WALL-PLAN-2026-11-26.md` again for the rules; they have not changed.

Start with `git merge wall` in this worktree.

## What the JS wall draws that is not terminal text

From the inventory of `wall/` (paths under `~/localdev/viz-web/glyph3d-js/`):

- **The drafting paper** (`wall/WallPaper.js`): the canvas colour and a grid of minor
  lines with every Nth a major line, found by a full-screen fragment shader from the
  camera, drawn first with no depth. Levels are discrete: a level whose lines would sit
  closer than a minimum on screen is not drawn, a hard switch and never a fade. A line
  is a fixed number of pixels wide in its exact colour. `crates/wall-core/src/snap.rs`
  (`grid_levels`) already decides which levels show at a zoom; use it.
- **Title bars** (`wall/TitleBar.js`, `wall/WallNames.js`): a strip above each
  terminal with its name, the name shrunk to fit, the bar capped in world height so it
  never grows past the gap between terminals. Groups have a header bar the same way.
- **Group frames** (`wall/GroupFrame.js`): a quiet backdrop behind a group's members.
- **Marks over the scene** that the browser drew as DOM/SVG and that are ours now
  (`wall/SelectionOverlay.js`, `wall/MapLabels.js`): the lasso polygon, selection
  outlines round selected terminals, the outline of where a dragged tile or pane would
  land (solid, or red and dashed when refused), map labels with their leader lines
  (a name beside a terminal too small on screen to carry its bar's text).

## What to build

Primitives the app composes, not the wall's logic (which terminal is selected, where a
label goes: `crates/wall-core` decides those).

1. **Paper**: `PaperStyle` (colours, minor spacing, major every N, line px, minimum
   px) and its pass, drawn before everything.
2. **Rectangles and lines in world space, with a z**: filled, outlined at a width in
   screen pixels, dashed; a closed polygon outline (the lasso); a line segment with a
   dot (a leader). One instanced pass.
3. **Text that is not a terminal**: a label: a string at a world position, at a height
   given either in world units or in screen pixels (a title is a fixed pixel height
   until its bar's cap; a map label is always pixels), one colour, a z, clipped to a
   world rect if given. Draw it with the same Slug path as cells. The simplest honest
   way is probably a scale on what the cell pass already does: decide and say.
4. **A scale per terminal**: `TerminalDraw` gains a scale (the JS wall has terminals at
   different scales, and fullscreen changes a terminal's text size), with fills, lines,
   cursor and border following it.
5. Grow the test pattern to show all of it: paper under three terminals, each with a
   title bar; a group frame round two of them with a header; one selected (outlined);
   a lasso; a dashed red drop outline; one terminal at half scale; a map label with
   its leader. `--wall-test-pattern` in the window shows the same.

Keep `WallRenderer::render` the one call the app makes, taking a frame description
(paper, terminals, shapes, labels). The app today builds `TerminalDraw`s in
`app.rs`'s `draw`; adjust that call site to your new signature.

## Not yours

`app.rs`'s input and terminal handling and `keys.rs` are the lead's; `session.rs` (live
terminals) is about to appear there. Touch `app.rs` only where the render call changes.
Glyphs still come from the baked atlas; another agent is finishing the run-time font
source, and switching the renderer over to it is the package after this one.

## Done

The PNGs, looked at; `cargo build --release -p glyph3d-native` with no warnings; the
tests you add pass and the existing ones still do; a commit per step. Reply as before:
what is done, what looks wrong, the API as built.
