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

## A definition run

`--definition-run <id or prefix>` (with `--definition-store DIR`, else
`$DEFN_STORE`) shows one run of the definition engine
(`crates/definition-engine`) instead of the command line's scene: the
passage, the options for the word in focus to its right, and the stack of
diff cards to its left. A click on a word asks for its expansion; an agent
answers it (`defn pending --wait`, the `definition-engine` skill); a click
on an option's line takes it; a click on a card checks its version out. The
run's file is the only channel: the window rebuilds when it changes on disk,
within 100 ms.

```sh
defn new "Letter" --file letter.txt            # prints the run's id
./target/release/glyph3d-makepad --definition-run letter
```

## Input

Left click picks; WASD flies, E/R up, Q/F down; the right button held, or
backquote toggled, is mouse-look (Escape ends it); the wheel sets the speed;
`h` `g` `t` `x` and the deck keys are the scene's (as in the winit window).
Keys reach the scene while the viewport has key focus (click it), so typing
in a panel's field never moves the camera. F8 works anywhere; F1, F6 and F7
unless a panel's text field has focus.

## How it draws

The engine prepares each frame on the CPU (`SceneLike::prepare_frame`:
camera, cull, environment, the rows that changed) and the six glyph3d passes,
ported to makepad draw shaders (`src/g3_passes.rs`), draw it; the glyphs come
from the Derived field's host blocks, published to the GPU as they are
(`src/frame_feed.rs`). Only the blocks the view draws are published, within
makepad's publication envelope (a quarter of the device's memory budget,
which makepad reads from the driver); the rest are evicted
and published again from the host blocks as the camera moves, so a tree
larger than the device (the linux kernel's 1.29B glyphs, 24 GiB) draws a view
at a time. The engine still builds a wgpu device of its own for the scene's
construction; the glyph slots are not uploaded to it, and nothing is drawn
through it. A load past the host's memory is refused before it allocates,
with the numbers.
