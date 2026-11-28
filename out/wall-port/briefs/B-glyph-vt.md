# Work package B: `crates/glyph-vt`

You are building one piece of a larger port. Read `out/WALL-PLAN-2026-11-26.md` first
(the goal, the pieces, and the rules everyone works under), then this.

## What this piece is

The wall shows live terminals. A daemon (`g3d-termd`) owns each terminal and sends the
app the terminal's raw bytes: first a "repaint" (VT bytes that redraw the current
screen), then the program's output as it comes. Something in the app has to turn those
bytes into a grid of cells a renderer can draw. That is this crate: bytes in, a
`term_cells::Screen` out.

The emulator itself is not to be written. makepad ships a pure-Rust port of Ghostty's
terminal core, and you are vendoring it and putting one seam in front of it.

## What to read

- `crates/term-cells/src/lib.rs` in this repo: the `Screen` you fill. It is the
  contract with the renderer; if it cannot express something a terminal shows, tell
  the lead instead of changing it.
- `~/localdev/makepad/apps/terminal/src/term/` (start at `mod.rs`): the VT
  core. It says of itself "no Makepad, no PTY, no I/O". Its one outside dependency is
  `~/localdev/makepad/libs/cell_width`. Licence: MIT (Ghostty's and
  makepad's), Unicode-3.0 for the width data.
- `out/wall-port/review-carbon-2026-11-23.md`, "Decision 1" and "Risk 3": three gaps a
  reviewer found in that core when it is used as a passive client. Check each against
  the source before trusting it.
- `~/localdev/viz-web/glyph3d-js/docs/termd/PROTOCOL.md`, sections `REPAINT`
  and "Terminal queries are termd's to answer": what the bytes are, and why this
  emulator must never answer a query.
- `~/localdev/viz-web/glyph3d-js/packages/glyph3d-core/src/collections/TerminalEmulator.js`
  (`readScreen`) and `docs/architecture/terminals.md` "Cells": how the JS app resolves
  a cell today (inverse, conceal, bold-is-bright, wide characters, clusters). Match
  its rules; they are what the user is used to.

## What to build

- Vendor `term/` and `cell_width` into this crate (for example `src/term/`,
  `src/cell_width/`), keeping every licence header, with a `NOTICE` naming the source,
  its licence and the makepad commit you copied from. Leave out what a display-only
  client does not need if it comes away cleanly (the key and mouse encoders: termd
  encodes input itself), but keep it if removing it means editing the core.
- The seam in the plan's section B: `Vt::new`, `feed`, `resize`, `fill`, and text
  extraction for a cell range (rows joined across soft wraps, trailing blanks
  trimmed), which selection and copy will use.
- `fill` resolves every cell to final colours: the 16-colour palette, the 256-colour
  cube, true colour, the theme's default foreground and background, inverse (swap),
  conceal (blank content, fill kept). Attributes map onto `term_cells::attrs` bit for
  bit. A wide character's first cell has `width` 2 and its second `width` 0. A base
  with combining marks, or an emoji sequence, is one cluster in `Screen::clusters`.
  Cursor, modes, title and the per-row soft-wrap flags come across too.
- `fill` touches only rows that changed. Comparing each row against what the `Screen`
  already holds is an acceptable way to get there; say which way you chose.
- The emulator's own replies to queries are discarded as they are produced, so that
  buffer cannot grow.
- A soft reset (`CSI ! p`) must reset what a repaint relies on it resetting; every
  repaint begins with one.
- The emulator keeps the visible screen only. termd owns scrollback and sends a
  scrolled view as another repaint.
- A `Theme` (16 palette colours, foreground, background, cursor) with a sensible dark
  default.

## How you will know it works

- Unit tests through the seam, not the core's internals: SGR colours of each kind, each
  attribute, the five underline styles and an underline colour, inverse, conceal, a
  wide character, a combining cluster, the cursor's position and shape, the alternate
  screen, a resize, soft-wrap flags, the title.
- The 29 conformance snippets at
  `~/localdev/viz-web/glyph3d-js/tools/fixtures/vt-conf/*.vt`: copy them
  into this crate's test data. For each, feeding it whole and feeding it one byte at a
  time must give the same `Screen` (frame boundaries mean nothing), and no snippet
  panics.
- `fill` on an unchanged emulator reports no change and bumps no row.
- A small example, `examples/vt_dump.rs`: read bytes from a file or stdin at a given
  size and print the resulting screen as text, one line a row. The lead will use it.

The round trip through the real daemon (a snippet fed to termd, its repaint fed here,
compared with the snippet fed here directly) needs work package A and will be wired
by the lead afterwards; leave a note on where it would go.

## Handing back

Work in this worktree on its branch; commit as you go. Your final reply: what works
(with the test output), what you left out, what you found true and false in the
reviewer's three gaps, and anything `term-cells` could not express.
