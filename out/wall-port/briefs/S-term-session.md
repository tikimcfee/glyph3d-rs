# Work package S: `crates/term-session`

Read `out/WALL-PLAN-2026-11-26.md` first, then this. Start with `git merge wall`.

## What this is

The native wall shows live terminals from the daemon `g3d-termd`. Two crates do the
work: `termd-client` (yours: the socket, a reader thread, events) and `glyph-vt` (an
emulator: bytes in, a `term_cells::Screen` out). The lead joined them in the window in
about 180 lines, `native/src/wall/session.rs`: one connection, a hub that sorts events
by terminal, one emulator per terminal fed ON THE WINDOW'S THREAD each time the reader
wakes the event loop. It works; a person has typed into it. It will not survive a
flood: `yes`, a build log, fourteen busy terminals. The browser version's ceiling was
exactly this, its emulator on the UI thread.

This crate replaces that file with the real thing, with no window in it, so it can be
tested against a scratch daemon.

## What to build

- **A session per terminal on its own thread**: it takes that terminal's bytes, feeds
  the emulator, and publishes the screen. The window's thread only ever swaps in a
  finished screen; it never parses.
- **Publishing that respects a frame**: the window is told "terminal N has news" (a
  wake callback, as `termd-client` does) and takes the latest screen when it draws.
  Under a flood the session coalesces: news after a quiet spell is published at once
  (a keystroke's echo must not wait), then at most once per interval or volume of
  bytes while output keeps coming. The browser wall's relay just gained the same rule
  (`/Users/lugo/localdev/viz-web/glyph3d-js/cli/attach_unix.go`, uncommitted there: the
  leading-edge coalescing timer, 16 ms or 16 KiB): read it and match its behaviour.
  Synchronized output (mode 2026) should publish whole frames if the emulator exposes
  it.
- **The whole surface a window needs**, so `session.rs` can go: open a namespace
  (start the daemon if needed), list and adopt its terminals as size owner, spawn
  (argv, size, cwd), kill, resize, key, paste, mouse, scrollback (`view`: the
  subscriber's own view into history arrives as a repaint with the cursor hidden;
  expose offset and max), title, working directory, modes (mouse tracking, alternate
  screen, bracketed paste), bell, exit code, and what happened to the daemon
  (restarted and recovered, or gone).
- **Text out** for selection and copy: a range of cells as text, through the emulator,
  callable from the window's thread without stopping the session for long.

## How you will know it works

Against the real daemon in scratch directories, as your client tests do (never
`wall`, `agents` or `default`; never without `G3D_TERMD_DIR`):

- the round trip the emulator's author left a note for
  (`crates/glyph-vt/tests/vt_conf.rs`, bottom): each of the 29 conformance snippets fed
  to a terminal in termd, a late subscriber's repaint fed to an emulator, compared
  with the snippet fed to an emulator directly. termd's own gate saw 28 of 29 agree
  with xterm; say what you see and why for each that differs;
- a flood (`yes`, or `seq 1 2000000`) into one terminal and into fourteen: the publish
  rate stays bounded, the final screen is right, memory does not grow, and a key typed
  during the flood reaches the program promptly. Report bytes per second parsed and
  publishes per second;
- typing echo latency after a quiet spell: the time from `key` to the published screen
  that contains it, measured;
- resize while output streams; scrollback out and back to live; a daemon restart
  ridden with the screens intact.

Then switch `native/src/wall/session.rs` and its call site in `main.rs` over to the
crate, keeping `app.rs`'s `Terminal` trait as the seam (the lead owns `app.rs`; touch it
only if the trait must grow, and say so). `cargo build --release -p glyph3d-native`
with no warnings, and the counted run still works:

    D=/tmp/g3d-s-$$; mkdir -m 700 $D
    G3D_TERMD_DIR=$D GLYPH_WALL_TYPE=$'echo well hello there\n' \
      target/release/glyph3d-native --wall --wall-ns probe --frames 120
    # prints the terminal's screen as text at exit; then stop that daemon, remove $D

Report as before, with the measurements, and every further wart you find in termd.
