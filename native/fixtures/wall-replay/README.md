# `wall-replay` terminal byte-stream fixtures

Raw subscriber byte streams (`REPAINT` + `OUTPUT` frames concatenated in wire
order) captured from live programs running inside an isolated scratch
`g3d-termd` daemon at **80×24** cells (`TERM=xterm-256color`,
`COLORTERM=truecolor`, `CLICOLOR=1`).

## How they were recorded

Each fixture was recorded with the `termd_record` example in
`crates/termd-client/examples/termd_record.rs`, which creates a private `0700`
scratch socket directory `/tmp/g3d-rec-<pid>` (`G3D_TERMD_DIR`), starts
`g3d-termd` in scratch namespace `rec` (refusing `wall`, `agents`, and
`default`), subscribes with `Role::Own` and `Flow::Block` at `80x24` before the
command emits its first byte so the initial `REPAINT` frame is included, and
stops the scratch daemon and deletes `/tmp/g3d-rec-<pid>` on exit:

```sh
# 1. JS wall's 80x23 visual test pattern (blocks, shades, box drawing, braille,
#    sextants/wedges, powerline, all SGR underline/strike/overline/dim/inverse/
#    conceal styles, SGR 58 underline colours, wide emoji on fills, combining marks):
cargo run --release -p termd-client --example termd_record -- \
  --out native/fixtures/wall-replay/term-test-pattern.vt --size 80x24 \
  -- /bin/sh ~/localdev/viz-web/glyph3d-js/tools/term-test-pattern.sh

# 2. Multi-column coloured directory listing (`ls --color`) of `tools/`:
cargo run --release -p termd-client --example termd_record -- \
  --out native/fixtures/wall-replay/ls-color.vt --size 80x24 \
  -- /bin/ls --color tools

# 3. Coloured commit graph (`git log --graph --oneline --decorate --color=always`):
cargo run --release -p termd-client --example termd_record -- \
  --out native/fixtures/wall-replay/git-log-graph.vt --size 80x24 \
  -- /usr/bin/git --no-pager log --graph --oneline --decorate --color=always -n 22

# 4. Full-screen alternate-screen editor (`vim --clean` with syntax + line numbers + ruler):
cargo run --release -p termd-client --example termd_record -- \
  --out native/fixtures/wall-replay/vim-screen.vt --size 80x24 --settle-ms 600 \
  -- /usr/bin/vim --clean -c "syntax on" -c "set number ruler" native/src/wall/replay.rs

# 5. Offline Claude Code Ink TUI (`claude --bare` in an isolated scratch HOME with
#    `ANTHROPIC_BASE_URL=http://127.0.0.1:1`, `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`,
#    `DISABLE_TELEMETRY=1`, `DISABLE_AUTOUPDATER=1` — exercises CHA `CSI G`, 24-bit RGB
#    inline diff preview, Kitty keyboard `CSI < u` / `CSI > 5 u`, APC graphics query,
#    DECRQM, and box-drawing rules without touching any live backend):
./tools/vt-diff.sh --record-claude-tui native/fixtures/wall-replay/claude-tui.vt

# 6-10. Wall default-size (100x30) visual parity captures (`out/wall-port/cell-parity-2026-11-26.md`):
# - `zsh-prompt.vt`: Oh My Zsh `agnoster` powerline prompt with git status segments + `ls --color`
# - `vim-rust.vt`: Vim 9 (`habamax`, `termguicolors`, italic doc comments, bold keywords/types, cursorline, statusline) on `native/src/wall/cell.rs`
# - `top-live.vt`: Interactive macOS `/usr/bin/top -s 1 -n 18 -o cpu`
# - `git-diff.vt`: `git --no-pager diff --color=always` with bold headers, cyan hunks, red deletions, and green additions
# - `man-page.vt`: `/usr/bin/man ls | /usr/bin/ul -t xterm-256color` with genuine ANSI bold and underline
```

## How to render offscreen

```sh
./target/release/glyph3d-native \
  --wall-replay native/fixtures/wall-replay/term-test-pattern.vt \
  --wall-size 80x24 \
  --screenshot /tmp/term-test-pattern.png
```
