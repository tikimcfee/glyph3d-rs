# The interface shell in the makepad host: a spec (2026-11-01)

The egui window's interface shell was built on 2026-10-24/25 in the second
clone and never merged. It had a command bus, a console and a Cmd-K palette,
and it reduced the panels to line emitters. Ivan's decision (2026-11-01):
archive it, and rebuild it in the makepad host. This note is the spec for
that rebuild, written against the makepad host as it stands, not against
`native/src/windowed/`.

The archived code is the annotated tag `archive/egui-interface-shell`
(`6d0594b`, nine commits on `5d9a2f6`), and its design note is
`git show archive/egui-interface-shell:out/INTERFACE-SHELL-2026-10-24.md`.
The first form, "stage-m" (2026-09-02), is the tag `archive/egui-interface-m1`
(`cf0f1d2`). It is the only one of the two that docked its panels.
`out/history/ARCHIVED-BRANCHES.md` has the archive record. Paths under
`windowed/` below are at the tag; paths under `makepad/src/` are live.

## What makepad has, and what this spec adds

Already built and wired live through `makepad/src/g3_ui.rs` (the widgets),
`ui_model.rs` (`G3UiState`, read once a frame, and `G3UiAction`, a flat enum
of everything a panel can ask) and `ui_adapter.rs` (`ui_state` builds the
state; `apply` turns each action into its engine call):

- the field HUD (F8);
- the Debug panel (F1), with the virtualized file browser;
- the Carrel transport and window sliders;
- the agent sessions browser (F7).

`input.rs` maps keys: `scene_key` for the scene, `host_key` for F1/F6/F7/F8.
`main.rs` routes `g3_ui_actions` and `host_key` into `ui_adapter::apply`.

Not built, and specified here: the four archived features.

1. **The command bus**: a line form for every action, and one queue.
2. **The console**: the bus's log and input line. Output still goes to
   `println!` from `apply` and `EngineHost::new`.
3. **The palette**: Cmd/Ctrl-K over every line the host knows.
4. **Docked panels**: makepad has `Dock` (`widgets/families/data/src/dock.rs`)
   and `Splitter` (`widgets/core/src/splitter.rs`). The egui shell kept
   floating windows and named docking as a follow-up; M1 docked them (HUD
   top, files left, inspector right, console bottom).

## The laws (from the design note, the target)

1. **The bus is the only action path.** Every intent (button, slider release,
   hotkey, palette row, console line) is a command LINE queued on one bus
   and run between frames by one executor, through the engine's own calls.
   Offscreen never learns the UI exists.
2. **Panels own no behaviour.** A panel reads the state and emits lines (or
   actions that have a line, below). The only direct writes are view state,
   never intents: the live LOD tuning, a dial's displayed value before its
   release, the browser's drawn rows, and the sessions filter and tab.
3. **The palette teaches the vocabulary.** Every row is `{title, line}`, and
   its subtitle is the exact line Enter fires. The console logs every line
   the bus ran, whoever pushed it, so a click shows the line that would have
   done it.
4. **The line is the command; the CLI is the parser.** A line starting with
   `--` is parsed by `Cli::command()` and `Cli::from_arg_matches`; every
   short form is sugar for those argv words, so panel, console and CLI agree
   by construction. A typed launch flag is refused (`--load-repo is a launch
   flag`). Relayouts carry absolute values, never toggles.
5. **Camera moves are explicit.** Navigation is a visible line (`fly`,
   `pose`); hand input (WASD, mouse-look, click-pick, scroll) stays direct.
6. **The frame is the proof.** Offscreen bytes are the oracle; the UI is the
   host's alone.

## 1. The bus, on the existing seams

`G3UiAction` stays the panels' vocabulary: the widgets compile against
`ui_model` alone and should keep doing so. The bus adds a LINE for each
action and one queue in front of `ui_adapter::apply`:

- **`ui_adapter::line_of(&G3UiAction) -> Option<String>`**: the line an
  action stands for. `None` marks a view-state write, which is applied
  directly and not logged.
- **`ui_adapter::parse_line(&str) -> Result<Vec<HostCommand>, String>`**: the
  grammar below. A `HostCommand` is an existing `G3UiAction`, an `Op` (run
  through `glyph3d_native::apply_op`, the applier `EngineHost::new` already
  uses for the startup ops), or one of the new words (`fly`, `help`,
  `clear`, the panels).
- **A queue on `EngineHost`**, drained where `apply_pending_relayout` runs.
  `main.rs`'s `handle_actions` and `handle_key_down` push the action's line
  instead of calling `apply`, and the console and palette push typed lines.
  Each line runs, then is logged with its replies.
- **`apply` returns its replies** (`Option<String>`) instead of printing
  them, so the console can log them. Startup ops log too.

### Every current action, as its line

| `G3UiAction` | Line | Note |
|---|---|---|
| `TogglePanel(Hud\|Debug\|Sessions)` | `panel hud\|debug\|sessions` | add `Console` and `Palette` to `G3Panel` |
| `CopyPose` | `copy-pose` | its doc says clipboard, but today it only prints |
| `ToggleCarrelGrab` / `ToggleFileGrab` / `ToggleEnvironment` | `key c` / `key g` / `key b` | a stopgap until these are verbs (below) |
| `Verb(spec)` | `VERB [ARGS]`, i.e. `--verb "..."` | `PANEL_VERBS` is a subset of `glyph3d_native::cli::VERB_HEADS` |
| `ClearPick` | `pick clear` | |
| `SetTextDetailPx`, `SetShowGlyphsPx`, `SetGreeking`, `SetGreekPure`, `SetFileBackgrounds`, `SetFileBgColor` | none | view state (law 2) |
| `SetZWrapSpacing(z)` | `--z-wrap-spacing z` | `f64` Display round-trips |
| `SetStrategy(i)` | `--repo-engine <name>` | name from the `ValueEnum` |
| `SetFieldMode(i)` | `--field-mode <name>` | |
| `ToggleCluster` | `--cluster-mode <the other>` | needs absolute relayout values (seams table) |
| `ToggleLayout` (also F6) | `--layout-mode <next>`: shelf, carrel, library, then shelf | same |
| `SetCarrelOptions(o)` | `--deck-window-limit N --deck-scroll-offset N --desk-revision-limit N --max-file-stacks N` | `desk_scroll_offset` has no flag. Give it one, or log a `#` note as the egui shell did for the scene's own carrel moves |
| `BrowserRows(rows)` | none | view state |
| `PickFile(row)` | `pick group N` then `fly group N`; the selected row again: `pick clear` | `pick_file` already uses the fly formula |
| `CarrelOldest`/`Prev`/`Next`/`Latest`/`Mode` | `key end` / `bracketleft` / `bracketright` / `home` / `v` | stopgap, as above |
| `RefreshSessions` | `sessions refresh` | |
| `SetSessionFilter`, `SetSessionHarness` | none | view state |
| `LoadSession(path)` | `--agent-session "<path>"` | quote the path so spaces survive |

## 2. The grammar (`windowed/bus.rs` `parse_line`, at the tag)

Words split like a shell without one: whitespace separates, `"..."` and
`'...'` quote, and a backslash outside single quotes takes the next
character. An `Err` is a reason for the log. No input may panic.

| Line | Lowers to |
|---|---|
| `--FLAG ...` | the CLI's own parse, details below |
| `VERB [ARGS]` | `--verb "VERB [ARGS]"` on the current pick, for any head in `cli::VERB_HEADS` |
| `pick FILE[:ROW[:COL]]` | `--pick-file FILE [--pick-row ROW [--pick-col COL]]` |
| `pick px X Y` | `--pick-px X Y` |
| `pick group N` / `pick clear` | `PickCommand::Group(N)` / `Clear` (no flag spells them) |
| `pose X Y Z YAW PITCH` | `--cam-pose ...` (degrees) |
| `set FLAG VALUE` | `--FLAG VALUE` |
| `fly FILE` / `fly group N` | frame a file from the front (§ 5) |
| `key NAME` | press a scene key: `g c v b end home bracketleft bracketright` |
| `panel NAME` | toggle a panel |
| `sessions refresh` | rescan the sessions |
| `copy-pose`, `help`, `clear` | print the `--cam-pose` line; list the grammar (the app words, then the verb heads); empty the log |
| `# ...` | a comment: logged, runs nothing |

How a `--FLAG ...` line is parsed:

- **Op flags** become ops in CLI order: `--pick-file`, `--pick-row`,
  `--pick-col`, `--pick-px`, `--cam-pose`, `--verb`, `--highlight`.
- **Relayout flags** become ONE `RelayoutRequest`, run before the line's
  ops, as at launch: `--field-mode`, `--repo-engine`, `--layout-mode`,
  `--cluster-mode`, `--z-wrap-spacing`, `--agent-session`,
  `--deck-window-limit`, `--deck-scroll-offset`, `--desk-revision-limit`,
  `--max-file-stacks`.
- **Any other flag typed** refuses the line. "Typed" is clap's
  `ValueSource::CommandLine`.
- **`--help`** prints the CLI's long help.

The egui shell's `shot` (F2) and `live next|prev|reset` (F3-F5) have no
makepad counterpart yet: offscreen owns screenshots in the makepad host
(`host.rs` refuses `--screenshot`), and there are no live sources.

## 3. The console

`windowed/panels/console.rs` at the tag. It belongs in the bottom dock, or
in an overlay panel until docking lands. It has three parts:

- **The log**: every line the bus ran, from any source, with its replies.
  It keeps 500 entries, dropping the oldest first, and sticks to the bottom.
- **The input line**: a `TextInput` taking the same grammar.
- **The history**: up and down walk the lines typed here, newest first;
  down past the newest restores what was being typed.

The console is the typing-isolation trap: typing WASD/h/g/t/x into it must
move nothing. `G3UiLayer::text_focused` already gates `host_key` in
`main.rs`. The scene keys go through the viewport's own `Hit::KeyDown`, so
they reach the scene only while the viewport has key focus. Key: F9, line
`panel console`.

## 4. The palette

`windowed/palette.rs` at the tag. Cmd-K on macOS, Ctrl-K elsewhere, consumed
before any text field sees it. It is a fuzzy list of every line the host
knows:

- `VERB_HEADS`;
- each live setting as its flag line (`--field-mode derived`, from the
  `ValueEnum` variants: the choices `ui_adapter` already builds for
  `G3Choice`);
- `fly <path>` per file (`UiProbeState::files`);
- `--agent-session "<path>"` per listed session;
- the app words.

Ranking is a fuzzy subsequence score, case-insensitive:

- a run of matching characters scores 3 a character, a match at a word
  start 2, any other match 1;
- less a point per 16 characters of target;
- whitespace in the query is ignored;
- a row's score is the better of its title's and its line's;
- ties go to the shorter line, then the line itself, so the order never
  depends on the catalogue's.

Keys:

- up and down move;
- Enter fires a row that parses as it stands;
- Tab, or Enter on a template such as `nudge-glyph` or
  `pick FILE[:ROW[:COL]]`, fills the console up to the first placeholder
  word (one with a capital, or `...`);
- Esc closes.

`fuzzy_score`, `rank` and `fill` contain no egui and port as they are.

## 5. Fly

`windowed/bus.rs` `fly_row` and `fly_pose`, at the tag.

- **`fly FILE`** takes the exact `rel_path` first, else the first path
  containing the text, so `a.rs` does not land on `ba.rs` when `a.rs`
  exists.
- **`fly group N`** takes the first row of that group.

The framing is `ui_adapter::pick_file`'s formula: camera facing -Z at the box
centre, at `fit_distance(max(half_h, half_w / aspect))`, set as a
`--cam-pose`. The egui shell flew in two steps, because the scene refreshes
`file_dyn` only for `browser_rows`. It added the row to `browser_rows`, then
framed the row from the refreshed pose once a frame had rendered. Otherwise
a file scrolled out of the browser, or typed in the console, frames a stale
or zero box. `pick_file` today reads `file_dyn` at once, which is right only
for a drawn row.

## 6. Docking

With `Dock` and `Splitter`, following M1's layout:

- HUD at the top;
- the file browser on the left, promoted out of Debug;
- an inspector on the right (the Debug panel's pick, verbs and controls);
- the console at the bottom;
- the field in the centre.

Panel toggles become dock-tab visibility. The egui shell deferred docking
partly because fly framing must use the viewport's rect inside the dock,
not the window's, or the framed file lands behind a dock. The makepad host
already does this: `viewport.rs` sets `EngineHost::set_viewport` from the
viewport widget's own rect, so a docked viewport frames correctly as long
as that stays true.

## Keys

| Key | Line | makepad today |
|---|---|---|
| F1 / F7 / F8 | `panel debug` / `panel sessions` / `panel hud` | `host_key`, as actions |
| F6 | `--layout-mode <next>` | `host_key` → `ToggleLayout` |
| F9 | `panel console` | new |
| Cmd-K / Ctrl-K | toggle the palette | new |
| Backquote / Esc | toggle / end mouse-look (direct, not lines) | the viewport |

Key routing, ported to the egui window on 2026-11-01 (`app::route_key`), is
worth matching in the viewport's key handling:

- **A key RELEASE always reaches the scene.** Check that a W held while
  focus moves to a text field still delivers its `KeyUp` to `host.key`. If
  the release goes to the field instead, the camera latches moving, which
  was the egui bug.
- **A press with Cmd or Ctrl held never reaches the scene.** Today the
  viewport's `Hit::KeyDown` ignores modifiers, so Cmd-S would fly the camera
  backwards, as it did in egui.

## Engine seams, and where they stand

| Seam | State on 2026-11-01 |
|---|---|
| `glyph3d_native::apply_op(scene, op)`, the one op applier | ported (`worktree-stranded`); offscreen, the egui window and `EngineHost::new` call it |
| `cli::VERB_HEADS`, every verb `parse_verb` knows | ported |
| `--max-file-stacks N` | ported: every Carrel slider but `desk_scroll_offset` has a flag |
| `SceneLike` `apply_pick` / `apply_verb` / `set_cam_pose` / `apply_highlight_sidecar` / `on_key` / `cam_pose` / `selected_group_id` | live, device-free; keys are `scene::SceneKey` |
| `relayout::{RelayoutRequest, rebuild_for_relayout}` | live; `EngineHost::apply_pending_relayout` applies it |
| `glyph_scene::fit_distance`, `UiProbeState::{files, file_dyn, browser_rows}` | live |
| A non-panicking op builder (`try_build_ops`: a typed `--pick-row` with no `--pick-file` is an `Err`) | NOT ported; `bfd9773` at the tag. The bus's `--` lines need it, made `pub` |
| Absolute `cluster_mode` / `layout_mode` on `RelayoutRequest`, replacing `toggle_cluster` / `toggle_layout`, with `next_layout_mode` and `other_cluster_mode` | NOT ported; `c241a71`. Law 4 needs it, because a toggle has no flag spelling. The toggles' callers today are `ui_adapter`'s `ToggleCluster` and `ToggleLayout` |
| A carrel patch (each limit optional), so one slider's line moves only its own value | NOT ported; `0866430` (`app::CarrelPatch`). `ui_adapter`'s `SetCarrelOptions` already merges into the current options, which covers the action form |
| The parser, the palette ranking and the fly formula | at the tag in `windowed/`. They contain no egui, so moving them into the engine (or into `makepad/src/`) gives the host and its tests one copy |

## Follow-ups the shell named, not built

- **The scene keys as verbs** (`carrel-next`, `carrel-prev`, `grab`,
  `grab-carrel`, `environment-toggle` in `parse_verb`), so the `key NAME`
  lines and `host.press` actions become verbs.
- **`--pick-group N` on the CLI**, so `pick group N` is sugar too.
  `--pick-file` cannot stand in: it takes the first path containing the
  text.

## How it was checked, and what to carry

Headless tests at the tag (`bus.rs`, `palette.rs`, `panels/console.rs`):

- console lines lower to the same op stream as the CLI argv;
- short forms and flag lines agree;
- malformed lines are refused without a panic;
- relayout flags become absolute requests;
- quoted paths survive the splitter;
- every line the window writes parses;
- `help` lists every app word;
- fly resolves the exact path first and frames with the browser's formula;
- the log ring, the history and the palette ranking.

Mutations `console-pick-sugar-row-col-swapped` and `split-args-drops-quotes`
proved them. They carry over unchanged once the parser has a home, plus
one new test: every `G3UiAction` with a line parses back to itself.

Not checked by anything but a human: palette focus, Cmd-K against Ctrl-K,
typing isolation in the fields, the history keys, stick-to-bottom and IME.
