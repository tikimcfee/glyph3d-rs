# Plan: the launcher moves into the renderer (C27)

Decided with Ivan, 2026-10-10. **Not started.** Validate the facts below
against the code first (they were read on 2026-10-10 at `7899d90`), then
implement in the steps at the end.

## The problem

The launcher TUI (`cargo glyph tui`) lives in `glyph`, the build/verify
tool, and starts a different program, the renderer (`glyph3d-native`),
which `glyph` deliberately does not link (the tool stays independent of what
it verifies). So the only way to start the renderer with the TUI's settings
was to translate them into command-line flags, and everything else followed:

1. **Two parsers of one file, with different rules.** `launch_config.toml`
   is read by the renderer (`native/src/launch_config.rs`, `LaunchConfig`:
   `deny_unknown_fields`, all keys plus `[section]` settings overrides,
   refused at startup when invalid) and by the TUI
   (`glyph/src/tui/state.rs`, `FileLaunchConfig`: a 16-key subset, unknown
   keys ignored, a file that fails to parse silently ignored via `if let Ok`,
   a bad value silently defaulted). They look in different places: renderer
   `./` then `../`; TUI `./` then the repo root.
2. **The file is applied twice.** The TUI reads it, turns its state into
   flags (`build_cli_args`), launches the renderer, and the renderer
   discovers the same file again (`cli::args::discovers_launch_config`) and
   merges it under the flags. Keys the TUI does not know (`greek_onset_px`,
   `lod_min_px`, `file_bg_color`, `frames`, the sections) always come from
   the file, invisibly; for the rest, precedence depends on whether the TUI
   happens to pass a flag.
3. **The TUI shells out to cargo to run itself.** Its `b`/`t` keys run
   `cargo glyph build` / `cargo glyph test` (`glyph/src/tui/terminal.rs`):
   the `glyph` binary invokes cargo, which rebuilds and relaunches `glyph`
   (through the doubled-alias strip in a nested worktree).
4. **Option spellings live in three places:** the renderer's clap
   definitions, the TUI's config parse, and the TUI's flag builder (plus the
   TUI's own copies of the option enums: `WrapMode`, `ColorMode`,
   `ClusterMode`, `RepoEngine`, `FieldMode`, ...). Adding `visible` meant
   touching all three by hand. The TUI also duplicates the renderer's agent
   session discovery (`discover_agent_sessions`).

Ivan's framing: "a CLI that houses a TUI is the core idea"; the trouble is
that the thing that builds the CLI is the main application, and the launcher
lives somewhere else.

## The decision

**The launcher becomes a front end of the renderer itself**, over the model
the renderer already has (its clap `Cli` merged with `LaunchConfig` into one
typed plan, `native/src/cli/`).

- `glyph3d-native` with no scene arguments (or `--launcher`) opens the TUI.
- The launcher's choice lists are GENERATED from the clap definitions
  (`ValueEnum` variants and their help text), so a new option or value
  appears in the launcher with no second copy of its spelling.
- The config file is read once, by the one strict loader; a bad file is an
  error shown on the launcher's status line, never swallowed.
- Session picking uses the renderer's own discovery (the F7 browser's code
  and `glyph-session-dirs`).
- `glyph` keeps build, test, prove, gates. `cargo glyph run` already builds
  the renderer first, so with no arguments it opens the launcher;
  `cargo glyph tui` stays as a shortcut for exactly that.

### The one wrinkle: one event loop per process

winit allows one event loop per process, and macOS cannot recreate it, so a
launcher that opened the window in-process could not return to its menu
when the window closes (today's TUI does return). So the launcher **starts a
child of itself with `--launch-config <file>`**, where the file is the typed
`LaunchConfig` it built, serialized by the same struct (it gains
`Serialize`) and parsed by the same loader. This is not the old flag
translation: the file is the contract, written and read by one type in one
binary. Side benefits: a renderer crash returns you to the launcher, and
every launch leaves a reproducible config behind (write it under the
system temp dir, or keep the last one as `target/last-launch.toml` — decide
when implementing; relative paths in it must be made absolute, because
`run` resolves paths against the directory you typed in).

## What goes, what stays

- **Goes:** `glyph/src/tui/` (its rendering code moves; its model is
  replaced), `FileLaunchConfig`, `build_cli_args`, the duplicated option
  enums and spellings, the duplicated session discovery, the `b`/`t`
  shell-outs.
- **Moves:** the TUI's drawing and key handling into a renderer module
  (`native/src/launcher/`), behind a default-on Cargo feature like
  `egui-ui` (ratatui + crossterm become renderer dependencies; the
  `cargo-check-no-ui` gate already catches feature-gating mistakes —
  decide whether `--no-default-features` drops the launcher too).
- **Stays:** everything `glyph` verifies and builds; `cargo glyph run`;
  `launch_config.toml`'s format (one schema, now with one reader).
- **One visible change:** there is no build key in the launcher; you start
  it through `cargo glyph run` (or `cargo glyph tui`), which builds first.

## Steps (each its own commit, battery green)

1. **The model.** `LaunchConfig` gains `Serialize`; the launcher's state is a
   `LaunchConfig` (plus UI-only state such as focus); choice lists come from
   clap's `ValueEnum` metadata. Unit tests: round-trip a config through
   serialize/parse; every clap value enum appears in the launcher's lists.
2. **The launcher module** in the renderer: the TUI's panels and keys ported
   over the new model, opened when no scene argument is given. Port the
   `TestBackend` render tests (`glyph/src/tui/draw.rs`) and the state tests
   (`glyph/src/tui/state.rs`) that still apply.
3. **Self-launch.** Enter writes the config and spawns
   `current_exe() --launch-config <file>`; the launcher waits, then returns
   to its menu (status line reports the exit). A test that the written file
   parses back to the same plan the CLI would build from equivalent flags.
4. **Delete the old TUI** from `glyph`; `cargo glyph tui` becomes
   `cargo glyph run` with no arguments; docs (AGENTS.md, README,
   `.agents/skills/glyph-engine-testing/SKILL.md`,
   `launch_config.example.toml`) updated; test floor adjusted on purpose
   (tests move rather than vanish: count them).

Check per step: `cargo glyph test` green; the launcher opened by hand
(visible mode selectable and launching, a bad config shown as an error, the
return to the menu after closing the window); prove the runner mutations if
`glyph/src` changes (`cargo glyph prove --changed`).

## Open questions for the implementing session

- Should the launcher save choices back to `launch_config.toml` (and how,
  without clobbering comments), or only write per-launch files?
- Which renderer options deserve a launcher control at all (today's TUI
  shows a subset)? Generating lists from clap does not mean showing all.
- Related but separate: C26 (split the LOD panel controls) is queued for the
  visible-mode delegate; the launcher should present the same two ideas
  ("Text detail", "Show glyphs") once C26 lands, not the old names.
