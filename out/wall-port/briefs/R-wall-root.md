# Work package R: the wall as one object, `crates/wall-core/src/wall.rs`

Read `out/WALL-PLAN-2026-11-26.md` first (the goal and the rules), then this. This is
the piece everything else now waits on, and the least mechanical of the run.

## Where things stand

The wall is an infinite canvas of live terminals; today a browser app (`wall/` in
`~/localdev/viz-web/glyph3d-js`, read-only), being rebuilt natively here.

- A window exists (`native/src/wall/app.rs`): winit, a cell renderer, live terminals
  from the daemon. A person has typed into it. But it knows nothing of the wall's
  rules: it pans, zooms, focuses and forwards keys, and that is all.
- The wall's rules exist too, ported module by module into `crates/wall-core` with
  their JS tests (229 pass): `model` (terminals, order, focus, placement, an outbox of
  effects), `groups`, `split`, `layout`, `snap`, `camera`, `fullscreen`, `pointer` (the
  gesture state machine), `keymap`/`chord` (what a key does), `router` and `commands`
  (every `wall.*` verb), `selection`, `scroll`, `mouse`, `menu`, `names`, `labels`,
  `dock`, `config`/`settings`, `store`, `status`, `palette`, `sidebar`, `agent_view`,
  `bus`. Each was ported alone, by a different author, against a trait standing in for
  "the rest of the wall": `pointer::PointerHost`, `groups::WallHost`,
  `fullscreen::FullscreenHost`, `commands::WallApi`. Nothing implements those traits
  but test doubles. Nothing joins the modules.

In the JS, the join is `wall/TerminalWall.js`, the composition root: it constructs the
parts, hands each what it needs, and is the `wall` object they all call. That is what
is missing here, and it is yours.

## What to build

`wall_core::wall::Wall`: one object a window can hold and drive, with no window, no
renderer and no daemon in it.

- **It owns the parts** (`WallModel`, `Groups`, the fullscreen controller, the pointer
  state machine, key routing, the router with the wall's verbs registered) and
  **implements their host traits**, so the parts finally talk to each other through
  it. Where two modules carry duplicate types or private stand-ins for each other
  (their authors listed these as "seams" in their module docs and tests), make them
  one: you may edit any module in `crates/wall-core` to do it. Keep every existing test
  passing; where a test double goes away, the test moves onto the real thing.
- **Input comes in as plain data**: pointer down/move/up/cancel, wheel, pinch, key
  (`input::KeyEvent`), paste text, viewport size and scale factor, a clock (`tick(dt,
  now_ms)`), and news from the terminals (a terminal appeared with this id and size;
  its size changed; its title; its modes: mouse tracking, alternate screen; it
  ended; the list of live ids on connect).
- **What it wants done goes out as effects** the caller drains, extending
  `model::WallEffect`'s pattern: spawn a terminal (argv, size), kill, resize, send a
  key, paste, a mouse event, scroll a terminal's view, set the clipboard, read the
  clipboard, set the pointer's cursor shape, redraw.
- **What to draw comes out as one frame description**, plain data in world units and
  the camera's pose: the paper's parameters; each terminal on the canvas (id, where
  its cells start, cols x rows, scale, z, focused, hovered, border state); title bars
  (rect, text, text height); group frames and headers; selection outlines; the lasso;
  the drop preview (and whether it is refused); map labels and their leaders; the
  notice; which panels are open and where they dock. The renderer agent is building
  the primitives that draw these (`out/wall-port/briefs/D3-canvas-chrome.md` names
  them); you describe, it draws.
- **Verbs**: `run("wall.split right")` runs a line through the router and returns its
  reply, as the JS CLI, palette, menus and buttons all do.
- **Text selection** needs each terminal's screen. The caller has them
  (`term_cells::Screen`); take a way to look one up by id as a parameter where a
  gesture or verb needs it, and never copy screens in.

## How you will know it works

Scenario tests against a fake backend that applies the effects (spawn makes a terminal
appear, resize answers with the new size), driving the `Wall` only through its input
methods and checking the frame description and effects. The steps of the JS end-to-end
gate, `tools/wall-e2e.mjs`, are the script to follow as far as they need no pixels:
spawn three terminals and they flow into place; click focuses and raises; a title-bar
drag moves with snapping, Ctrl inverting it; a border drag resizes in whole cells and
asks the terminal to resize; the wheel pans, with the zoom key zooms about the
pointer, over the focused terminal scrolls it; lasso selects; chord+G groups; chord+D
splits and the new terminal lands as a pane; divider drag; pane swap and dock;
fullscreen and back; hide, show, close; a drag on a body selects text and the copy
chord yields the clipboard effect; a program that asked for the mouse gets the press;
state survives a restart through the store. Each as its own test, named for what it
shows.

Then every existing wall-core test still passes, clippy is quiet, and you have told the
lead what the window must now do: the `Wall` API as built, in the order a window calls
it per event and per frame.

## Notes

- `docs/architecture/wall.md` in the JS repo describes the behaviour in prose and is
  slightly behind the code (group fullscreen and the focus chords are newer): the
  source wins.
- `out/INTERFACE-SHELL-SPEC-2026-11-01.md` is this repo's own note on verbs, a console
  and a palette; the verbs agent recorded where it and the JS differ.
- Work in this worktree on its branch, starting with `git merge wall`. Commit as you go.
  This is days of a person's work; get the spine standing first (terminals, focus,
  move, resize, pan, zoom, keys, verbs, frame description, effects) with its scenario
  tests, commit, and report, before going on to groups, splits, fullscreen and
  selection. Say plainly what is not joined yet.
