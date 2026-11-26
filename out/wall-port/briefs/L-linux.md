# Work package L: the wall on Linux (workstation)

Read `out/WALL-PLAN-2026-11-26.md` first (the goal and the rules), then this.

## What this is

The wall, an infinite canvas of live terminals, is being built natively in this
repository on branch `wall`. All of it so far was written and run on one machine, an
Apple-silicon Mac (Metal). The goal says macOS **and** Linux. You are on the Linux
machine (workstation: 32 cores, an RTX 5090, Vulkan), in a worktree of branch `wall`, and
your job is to find out what is true here and fix what is not.

On this machine the JS repository (read-only) is at `~/dev/glyph3d-js`, and the daemon
and its client are installed at `~/.local/bin/g3d-termd` and `~/.local/bin/g3d`.
`agent index search "..."` finds code and transcripts across `~/dev` if you need to
look something up.

## What to do

1. **Build and test everything the wall is made of**, natively:
   `cargo build --release -p glyph3d-native`, then the tests of `term-cells`,
   `termd-client` (its live tests start scratch daemons), `glyph-vt`, `glyph-font`,
   `wall-core`, `wall-control`, `agent-feed`, and the wall's own tests in the app
   (`cargo test --release -p glyph3d-native --lib wall::`, which include GPU tests).
   Zero warnings is the house rule. Record every failure with its output before you
   fix anything.
2. **Look at what it draws here.** Offscreen, no window needed:
   `target/release/glyph3d-native --wall-test-pattern --screenshot OUT.png`, the same at
   `--zoom 2.5` and `--zoom 0.14`, and each recorded program in
   `native/fixtures/wall-replay/` through `--wall-replay FILE --wall-size 80x24
   --screenshot OUT.png`. Look at every picture. The Mac's pictures of two of these are
   in `out/wall-port/` (`replay-term-test-pattern-after-fonts.png`); differences between
   rasterizers at the level of an edge pixel are expected and uninteresting, anything
   with a shape is not.
3. **The daemon on Linux.** `termd-client` picks its socket directory and checks its
   peer differently here (`/run/user/<uid>/g3d`, `SO_PEERCRED`). Its tests cover the
   scratch case; also check, by reading and by a scratch run, that a default-directory
   namespace of your own works and is cleaned up (use a namespace named for this task,
   stop its daemon after). Never touch namespaces `wall`, `agents` or `default`: people
   and agents are working in them on this machine too.
4. **The window.** This machine may or may not have a display you can open a window on
   (`echo $DISPLAY $WAYLAND_DISPLAY`). If it does not, say so and stop there for the
   window; if it does, a counted run draws a few frames and exits and can save its last
   frame: `GLYPH_WALL_SHOT=out.png target/release/glyph3d-native --wall
   --wall-test-pattern --frames 30`, and against a scratch daemon
   (`G3D_TERMD_DIR=/tmp/g3d-l-$$`, `--wall-ns probe`) with
   `GLYPH_WALL_TYPE=$'echo well hello there\n'`, which prints the terminal's text at
   exit.
5. **Fix what is Linux's fault** in this worktree on its branch (`wall-linux`), one
   commit per fix, saying what you ran. A fix that changes behaviour on macOS, or that
   you cannot check without a Mac, is described in your reply instead of made.

## Handing back

Push your branch to `origin` when it has commits (`git push -u origin wall-linux`;
never push `wall`, `work` or `main`). Your final reply: what built and passed
unchanged, each failure and what it was, each fix, what the pictures show and how they
differ from the Mac's, whether a window can open here, and the build and test times on
this machine (the lead is deciding which work to move here).
