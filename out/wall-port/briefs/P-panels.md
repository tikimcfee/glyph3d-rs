# Work package P: a native panel, `native/src/wall/panels/`

Read `out/WALL-PLAN-2026-11-26.md` first, then this. Start with `git merge wall`. The
message that sent you here names YOUR panels.

## What this is

The browser wall has flat panels over its canvas: a palette, a sidebar, a settings
panel, a status window, a help panel, a right-click menu. In the native wall those are
egui (0.36, already a dependency of `native` behind the `egui-ui` feature), drawn over
the scene. Their LOGIC is already ported and tested in `crates/wall-core` (`palette`,
`sidebar`, `settings`, `status`, `keymap`'s help sections, `menu`, `dock`); what is
missing is the drawing.

The JS panels are the look and behaviour to match:
`/Users/lugo/localdev/viz-web/glyph3d-js/wall/{Palette,Sidebar,SettingsPanel,StatusWindow,HelpPanel,ContextMenu,Panel}.jsx`,
described in `docs/architecture/wall.md` there.

## The contract every panel follows

A panel is a function, not an object that owns anything:

    pub fn show(ctx: &egui::Context, state: &mut <Panel>State, input: &<Panel>Input, out: &mut Vec<String>)

- `input` is plain data the caller built from `wall-core` (rows from
  `palette`/`sidebar`/..., the knob schema, the status document): the panel reads it.
- `state` is the panel's own view state (open, the search text, the highlighted row,
  folded sections, where it floats or docks): the caller keeps it between frames.
- `out` receives verb lines, exactly what a person could type: `wall.reveal t3`,
  `wall.config snap false`, `wall.split right`. **A panel does nothing else.** It never
  touches a terminal, the store or the daemon; every action on the wall is a verb
  (`crates/wall-core/src/commands.rs` lists them), and the caller runs what `out` holds.
- Keys: the palette and settings take the keyboard while open (typing filters, arrows
  move, Enter runs, Escape clears the search then closes), as the JS does. Report
  whether the panel wants the keyboard this frame, so the caller sends keys to it and
  not to a terminal.
- Docking: a panel floats or docks left or right; `wall_core::dock` says where docked
  panels sit and what they leave for the canvas. Take the dock rect as input.

Put each panel in its own file under `native/src/wall/panels/` (create `panels.rs` with
the `pub mod` lines if it is not there; if another agent created it first, add your
lines), behind `#[cfg(feature = "egui-ui")]`. Do not edit `app.rs`: the lead mounts
the panels.

## How you will know it works

egui runs without a window: `egui::Context::default().run(raw_input, |ctx| ...)`. Test
each panel that way: it lays out without panicking at several sizes; a simulated click
on a row, or typing a query and pressing Enter, puts the expected verb in `out`;
Escape closes it; an empty input says so instead of drawing nothing. Also write one
small example or test helper that renders the panel to a PNG if you can do it with
what is already in the workspace (egui-wgpu is; the wall's offscreen path is in
`native/src/wall/renderer.rs`), and look at it; if that costs more than an hour, skip
it and say so.

`cargo build --release -p glyph3d-native` with no warnings; your tests pass; commit as
you go. Report: the panel functions and their input/state types as built, what of the
JS panel is not there yet, and anything `wall-core`'s models lacked.
