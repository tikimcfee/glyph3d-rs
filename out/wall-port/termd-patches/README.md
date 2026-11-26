# `g3d-termd` Differential-Fidelity Patches (`T1`–`T3`)

Three standalone git patches against `glyph3d-js` (`crates/g3d-termd`, `crates/g3d-hold`, and `docs/termd/PROTOCOL.md`), produced from an isolated scratch copy without modifying `/Users/lugo/localdev/viz-web/glyph3d-js` or `~/.local/bin`.

Apply in `/Users/lugo/localdev/viz-web/glyph3d-js` with:

```sh
git am out/wall-port/termd-patches/0001-t1-repaint-preserve-soft-wraps.patch \
       out/wall-port/termd-patches/0002-t2-resize-repaint-on-scrollback-pull.patch \
       out/wall-port/termd-patches/0003-t3-pty-child-reset-signals-and-mask.patch
# (or `git apply` each patch in 0001 → 0002 → 0003 order)
```

---

## Summary of Patches

### 1. `0001-t1-repaint-preserve-soft-wraps.patch` (`T1`)

- **Bug**: `crates/g3d-termd/src/repaint.rs` (`live()`) formatted both scrollback history (`lines 65–73`) and the active screen (`lines 90–98`) with `unwrap = false`. `libghostty-vt`'s `PageFormatter` (`vendor/ghostty/src/terminal/formatter.zig:1368`: `if (!row.wrap or !self.opts.unwrap) blank_rows += 1;`) therefore emitted hard `\r\n` after every soft-wrapped row. Any viewer (`glyph-vt`, `xterm.js`, or `libghostty-vt`) parsing `REPAINT` recorded `row.wrapped = false`, breaking logical-line copy/unwrap and subsequent client-side resize reflow.
- **Fix**: Pass `unwrap = true` to `format_range` in `repaint::live` for both `hist` (`trim = true, unwrap = true`) and `screen` (`trim = false, unwrap = true`). `libghostty-vt`'s `PageFormatter` was designed for `Format::Vt` with `unwrap = true`: it omits `\r\n` after wrapped rows, carries trailing blank runs across wrapped rows as spaces (`formatter.zig:1478–1500`), and skips `.spacer_head` cells at the right margin so wide characters wrap naturally on the viewer (`formatter.zig:1435–1440`). `repaint::view` (scrolled-back viewport) is intentionally left at `unwrap = false` because `PageListFormatter` (`formatter.zig:955–959`) advances `br` past a bottom-right `.spacer_head` onto the next row when `unwrap = true`, which is a no-op at the end of `Active` in `live()` (`rightWrap(1)` is `null`) but would scroll a mid-history `view` window by one row.
- **Test in patch**: `repaint_preserves_soft_wraps_and_reflows_identically` in `crates/g3d-termd/tests/daemon.rs` (95-char styled ASCII soft wrap + col-59 CJK wide-spacer soft wrap; verifies both unwrapped plain text and standalone viewer reflow `60×12 → 110×12`). Failed before fix (`REPAINT turned soft-wrapped rows into hard line breaks`); passes after.

### 2. `0002-t2-resize-repaint-on-scrollback-pull.patch` (`T2`)

- **Bug**: When the primary screen holds scrollback (`sb_before > 0`) and `resize` grows `cols` or `rows` (`80×24 → 132×40` in `git-log-graph`), `termd` reflows its 10 MB scrollback and pulls scrolled-off history rows back onto the active grid, shifting the cursor down accordingly — but `Owner::resize` (`crates/g3d-termd/src/owner.rs:1452`) sent raw subscribers only `EVENT size` and no `REPAINT`. A viewer with `scrollback = 0` (both the JS wall's `TerminalEmulator` and the native wall's `glyph-vt`) or a late subscriber only holds the previously visible rows, leaving its active grid and cursor row out of sync with `termd`.
- **Why daemon-pushed `REPAINT` beats relying solely on a host `repaint` request**: Even when a host (`cli/attach_unix.go:425` or `crates/term-session/src/session.rs:599`) sends an asynchronous `{"op": "repaint"}` after seeing `EVENT size`, there is a round-trip window between `EVENT size` (`seq = S`) and when `termd` processes that `repaint` request (`seq = S + Δ`). Any post-`SIGWINCH` relative cursor motion emitted by the program in `[S, S + Δ)` parses against the wrong cursor row on the viewer, and passive raw clients (`g3d watch`, `g3d attach`, `PROTOCOL.md`'s `watch.py`) never request `repaint` on `size` at all. Emitting `push_live_repaint(i, 0)` directly inside `Owner::resize` right after `EVENT size` places the `REPAINT` at exact `seq = S` with zero RTT, and only fires when `!alt && sb_before > 0 && (cols > old_cols || rows > old_rows)` (never on alternate-screen TUIs, empty scrollback, or pure shrinks).
- **Fix**: Check `!alt && sb_before > 0 && (cols > old_cols || rows > old_rows)` in `Owner::resize` (`crates/g3d-termd/src/owner.rs`) and push `push_live_repaint(i, 0)` to `Mode::Raw` subscribers at `self.seq`; document in `docs/termd/PROTOCOL.md` (`### resize`).
- **Test in patch**: `resize_pulling_scrollback_onto_screen_repaints_zero_scrollback_viewer` in `crates/g3d-termd/tests/daemon.rs` (prints 15 lines on `40×6`, leaving 10 in scrollback, drives a `scrollback = 0` viewer in wire order across `Resize(60, 12)`, and asserts both the post-`size` `REPAINT` and full line-by-line parity with `Capture`). Failed before fix; passes after.

### 3. `0003-t3-pty-child-reset-signals-and-mask.patch` (`T3`)

- **Audit & Bug**: Auditing `crates/g3d-hold/src/spawn.rs:148–155` and `crates/g3d-hold/src/holder.rs:1146, 1785` against Ghostty (`src/Command.zig` / Zig `std.process.Child`) and xterm (`main.c`) showed that `g3d-hold` already looped `for sig in 1..32 { libc::signal(sig, libc::SIG_DFL); }` + `sigprocmask(SIG_SETMASK, &empty, null)` (so our earlier 30 s timeout on `yes | head -c` at host load 140 was PTY throughput + shell-startup race rather than `SIGPIPE` missing on macOS), **but** uncovered three real defects in that reset path and its test gate:
  1. **`1..32` misses all Linux real-time signals (`32..=64`, `_NSIG = 65`)**: any ignored real-time signal (`SIGRTMIN..=SIGRTMAX`) inherited from a starter or service wrapper survived `execve` into every shell.
  2. **`libc::signal(sig, SIG_DFL)` leaves `sa_flags = SA_RESTART` instead of `0`** (and is not guaranteed async-signal-safe with flags across platforms), whereas Ghostty and xterm reset via `sigaction(2)` with zeroed `sa_flags = 0` (clearing `SA_RESTART`, `SA_NOCLDSTOP`, `SA_NOCLDWAIT`, `SA_ONSTACK`) and empty `sa_mask`.
  3. **`crates/g3d-termd/tests/env.rs` masked `SIGPIPE` regressions**: `probe()` is a Rust test binary whose own `libstd` pre-main startup sets `SIGPIPE = SIG_IGN`, so `env.rs:133` filtered out `s != libc::SIGPIPE`, and `odd_daemon` only ignored `SIGINT` without testing `SIGPIPE`, `SIGQUIT`, `SIGTSTP`, `SIGHUP`, `SIGCHLD` flags, or a non-empty `sigprocmask`.
- **Fix**: Introduce `g3d_hold::spawn::reset_signals_and_mask()` + `NSIG_LIMIT` (`65` on Linux/Android, `32` on macOS/BSD) using `libc::sigaction` with `sa_sigaction = SIG_DFL`, `sa_flags = 0`, empty `sa_mask`, and `sigprocmask(SIG_SETMASK, &empty, null)`, shared across `spawn::spawn` and both `holder.rs` forks.
- **Tests in patch**:
  - `pty_child_gets_clean_default_signals_flags_and_empty_mask` in `crates/g3d-hold/src/spawn.rs` (verifies `sa_sigaction == SIG_DFL && sa_flags == 0` and empty mask across `1..NSIG_LIMIT`, plus `yes | head -c 64` `SIGPIPE` termination and `wait $p` `SIGCHLD` reaping on a real PTY). Failed before fix (`child signal check failed on sig/code=1` due to `SA_RESTART`); passes after.
  - Extended `odd_daemon` and `a_terminal_from_an_odd_starter_gets_the_policy_environment` in `crates/g3d-termd/tests/env.rs` to ignore `SIGINT`/`SIGQUIT`/`SIGTSTP`/`SIGHUP`/`SIGPIPE`, block `SIGALRM`/`SIGUSR1`, check `1..NSIG_LIMIT`, and verify a non-Rust `/bin/sh` pipeline (`yes | head -c 64`) receives `SIGPIPE = SIG_DFL`.

---

## Protocol Consequences for Other Clients

### Browser Wall (`cli/attach_unix.go` + `xterm.js` `TerminalGrid`)
- **`T1` (un-wrapped `REPAINT` rows)**: **Strictly beneficial — fixes a latent bug in the browser wall.** `docs/architecture/terminals.md` (line 102) explicitly states: *"a soft wrap (`isWrapped`, which xterm sets as the program's text runs past the edge — termd's repaint keeps it: `wall-e2e` reads a wrapped line as one after `terminal.refresh`) joined"*. Because `REPAINT` always starts with `PREAMBLE` (`\x1b[?7h` autowrap on, `\x1b[H\x1b[2J` home + clear) and `xterm.js` runs at `REPAINT`'s exact `(cols, rows)`, omitting `\r\n` after wrapped rows makes `xterm.js` set `line.isWrapped = true` on the continuation row while placing every cell at the identical `(col, row)`. Logical-line selection and copy after `terminal.refresh` or page reload now work as `terminals.md` intended.
- **`T2` (`REPAINT` after scrollback-pull `resize`)**: **Harmless and idempotent.** `cli/attach_unix.go:420–425` already calls `repaint()` on every `EVENT size`. With `T2`, when scrollback is pulled onto the screen, the first `REPAINT` arrives immediately at `size`'s `seq` (eliminating the 1-RTT window), and the adapter's follow-up `repaint` request produces a second identical `REPAINT` that `attach_unix.go:529` coalesces inside its 16 ms window (`PREAMBLE` clears and redraws the same screen). When `sb_before == 0`, `alt == true`, or the terminal shrinks, no extra `REPAINT` is sent.

### `gomuxai` (`agent` CLI) & `g3d` CLI (`g3d attach` / `g3d watch` / `g3d capture`)
- **`agent` CLI**: Screen inspection (`agent capture`) uses `capture` (`Format::Plain`), which already took an explicit `unwrap` boolean and is unaffected by `REPAINT`. Any raw stream consumer resets cleanly on `PREAMBLE` and wraps at `(cols, rows)`.
- **`g3d attach` / `g3d watch`** (`crates/g3d/src/main.rs:613–618`): Filters terminal queries out of `REPAINT` and writes the VT bytes to the host terminal. Under `T1`, the host terminal soft-wraps at `cols` (when its window matches `sub.cols`, which `g3d attach --take` enforces) so mouse selection in Ghostty/Terminal.app copies wrapped lines without spurious newlines. Under `T2`, resizing a terminal larger with `g3d attach --take` now redraws the pulled-back history rows onto the attached real terminal instead of leaving the top rows blank/stale.

### Native Wall (`crates/term-session`)
- `session_worker_loop` (`crates/term-session/src/session.rs:559–604`) rebuilds a clean `Vt::new(cols, rows, &theme)` on every `Event::Repaint` and also queues an async `Repaint` on `Event::Size`. With `T1` + `T2`, `glyph-vt` preserves `Screen::wrapped` across repaints and receives the scrollback-pull `REPAINT` at exact `seq` with zero intermediate frame mismatch.

---

## Verification

1. **Red → Green per-patch tests** (built in an isolated scratch copy outside both repos, with zero edits to `/Users/lugo/localdev/viz-web/glyph3d-js` or `~/.local/bin`):
   - `T1`: `repaint_preserves_soft_wraps_and_reflows_identically` (`crates/g3d-termd/tests/daemon.rs`) — failed before fix (`REPAINT turned soft-wrapped rows into hard line breaks`), passed after (`0.21 s`).
   - `T2`: `resize_pulling_scrollback_onto_screen_repaints_zero_scrollback_viewer` (`crates/g3d-termd/tests/daemon.rs`) — failed before fix (`expected a live REPAINT after primary-screen resize with scrollback`), passed after (`0.18 s`).
   - `T3`: `pty_child_gets_clean_default_signals_flags_and_empty_mask` (`crates/g3d-hold/src/spawn.rs`) — failed before fix (`child signal check failed on sig/code=1` due to `SA_RESTART`), passed after (`0.21 s`), along with all 11 tests in `crates/g3d-termd/tests/env.rs` (`1.77 s`).
2. **Patch application check**: verified both `git am 0001-*.patch 0002-*.patch 0003-*.patch` and sequential `git apply --check && git apply` cleanly against baseline `glyph3d-js` sources with zero diff.
3. **Full `vt-diff` differential run against the patched scratch `g3d-termd` binary** (invoked by absolute path via `G3D_TERMD_BIN` / `--termd-bin` in an isolated `/tmp/g3d-vt-$$` `G3D_TERMD_DIR`, namespace `probe`):
   - **36 of 36 quiet-point comparisons matched with `0 total disagreement(s)`** across `capture(plain)`, `capture(unwrap: true)`, live `Vt`, and mid-session `REPAINT` (`zsh-wrapped-prompt`, `vim-split-visual`, `less-search`, `top-3s`, `tmux-two-panes`, `git-log-graph`, `unicode-initial-default`, `unicode-mode2027-off`, `unicode-mode2027-on`, `tab-stops`), eliminating all 20 prior `T1` wrapped-row `REPAINT` divergences and the `T2` `git-log-graph@132x40` scrollback-pull divergence.
