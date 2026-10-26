# glyph3d-makepad

The glyph3d renderer hosted in [makepad](https://github.com/makepad/makepad):
one window holding the scene's viewport and, over it, the field HUD (F8), the
Debug panel (F1), the agent carrel's transport and the agent sessions browser
(F7). Brought up 2026-10-26; the engine is `glyph3d-native` used as a library,
without its winit/egui driver.

## Layout on disk

It expects a makepad checkout beside this repository:

```
<dir>/glyph3d-rs/   this repository
<dir>/makepad/      the makepad checkout
```

A worktree nested under `.claude/worktrees/` is two levels deeper, so give it
the same sibling with a symlink (untracked): `ln -s <dir>/makepad
.claude/worktrees/makepad`.

This crate is its own workspace with its own `Cargo.lock`, excluded from the
root workspace: `cargo glyph` and its gates never build it.

## Build and run

```sh
cd makepad
cargo build --release
./target/release/glyph3d-makepad --load-repo ../native/fixtures/g-pick-repo
```

The command line is the engine's (`--load-repo DIR`, `--render-file P`,
`--agent-session S`, `--field-mode`, `--cam-pose`, `--pick-*`, `--verb` ...;
`glyph3d-native --help`), less makepad's own flags (`--remote`, `--window`,
...). Modes with no window (`--screenshot`, the fixture checks, `--gpu-key`)
belong to `glyph3d-native` and are refused here.

For an agent or a test, run it hidden and drive it with makepad's remote
client: `MAKEPAD_HIDE_WINDOWS=1 ./target/release/glyph3d-makepad --remote
...`, then `makepad-remote app <pid> snap|grab|key|click|gq` (makepad's
`docs/agents/app-remote.md`).

## Input

Left click picks; WASD flies, E/R up, Q/F down; the right button held, or
backquote toggled, is mouse-look (Escape ends it); the wheel sets the speed;
`h` `g` `t` `x` and the deck keys are the scene's (as in the winit window).
Keys reach the scene while the viewport has key focus (click it), so typing
in a panel's field never moves the camera; F1, F6, F7 and F8 work anywhere.

## What it draws, for now

Until the makepad draw passes land, the viewport shows the engine's own wgpu
frame, read back without blocking (`src/stand_in.rs`), and each panel shows
its state as text until its widget lands.
