# The interface shell: a spec for the makepad rebuild (2026-11-01)

The egui window's interface shell (a command bus, a console and a Cmd-K
palette, with the panels reduced to line emitters) was built on 2026-10-24/25
in the second clone and never merged. Ivan's decision (2026-11-01): archive
it, and rebuild it in the makepad host, which already has most of the panels.
The code is the annotated tag `archive/egui-interface-shell` (`6d0594b`, nine
commits on `5d9a2f6`); its design note is
`git show archive/egui-interface-shell:out/INTERFACE-SHELL-2026-10-24.md`.
An earlier form (2026-09-02, "stage-m") is `archive/egui-interface-m1`
(`cf0f1d2`); the shell supersedes it. `out/history/ARCHIVED-BRANCHES.md`
has the archive record.

This note says what each feature did, its commands and keys, and which
engine seams it used, so the makepad host can rebuild it without reading
the egui code. Paths in `windowed/` are at the tag.

## What makepad already has (2026-11-01)

`makepad/src/g3_ui.rs`, `ui_model.rs`, `ui_adapter.rs`, `input.rs`: the field
HUD (F8), the Debug panel (F1) with the virtualized file browser (click
selects and flies), the Agent Sessions browser (F7), the Carrel transport
and window sliders, the verb buttons (literals through `parse_verb`), the
LOD/cull controls and the layout controls, all as a typed `G3UiAction` the
host applies (`ui_adapter::apply`). Relayouts go through
`relayout::rebuild_for_relayout`; the startup ops through `apply_op`.

What it does NOT have is the shell's core: no line form of an action, no
console, no palette, no `help`, no fly-by-name.

## The laws (from the design note, still the target)

1. **The bus is the only action path.** Every intent (button, slider
   release, hotkey, palette row, console line, self-test step) is a command
   LINE queued on one bus and run between frames by one executor, through
   the CLI's own calls. Offscreen never learns the UI exists.
2. **Panels own no behaviour.** A panel reads the probe snapshot and pushes
   lines. The only direct writes are view state, never intents: the live LOD
   tuning (text detail, show glyphs, file rectangles, greeking, file
   backgrounds), a dial's own displayed value before its release pushes the
   line, and `UiProbeState::browser_rows` (which rows the browser drew, so
   the scene refreshes their live poses; `fly` adds its row too).
3. **The palette teaches the vocabulary.** Every row is `{title, line}`, and
   its subtitle is the exact line Enter fires. The console logs every line
   the bus ran, whoever pushed it, so a click shows the line that would have
   done it.
4. **The line is the command; the CLI is the parser.** A line starting with
   `--` is parsed by `Cli::command()` and `Cli::from_arg_matches`; every
   short form is sugar that becomes those argv words. Panel, console and
   CLI agree by construction, with no hand-written inverse formatter. A
   typed launch flag is refused (`--load-repo is a launch flag`).
   Relayouts carry absolute values, never toggles.
5. **Camera moves are explicit.** Navigation is a visible line (`fly`,
   `pose`); hand input (WASD, mouse-look, click-pick, scroll) stays direct.
6. **The frame is the proof.** Offscreen bytes are the oracle; the UI is
   host-only.

## The grammar (`windowed/bus.rs`, `parse_line`)

Words split like a shell without one: whitespace separates, `"..."` and
`'...'` quote, a backslash outside single quotes takes the next character.
An `Err` is a reason for the log; no input may panic.

| Line | Lowers to |
|---|---|
| `--FLAG ...` | the CLI's parse. Op flags (`--pick-file`, `--pick-row`, `--pick-col`, `--pick-px`, `--cam-pose`, `--verb`, `--highlight`) become ops in CLI order. Relayout flags (`--field-mode`, `--repo-engine`, `--layout-mode`, `--cluster-mode`, `--z-wrap-spacing`, `--agent-session`, `--deck-window-limit`, `--deck-scroll-offset`, `--desk-revision-limit`, `--max-file-stacks`) become ONE relayout request, run before the ops on the line, as at launch. Any other flag typed (clap's `ValueSource::CommandLine`) refuses the line. `--help` prints the CLI's long help |
| `VERB [ARGS]` | `--verb "VERB [ARGS]"` on the current pick, for any head in `cli::VERB_HEADS` |
| `pick FILE[:ROW[:COL]]` | `--pick-file FILE [--pick-row ROW [--pick-col COL]]` |
| `pick px X Y` | `--pick-px X Y` |
| `pick group N` / `pick clear` | `PickCommand::Group(N)` / `Clear` (no flag spells them) |
| `pose X Y Z YAW PITCH` | `--cam-pose ...` (degrees) |
| `set FLAG VALUE` | `--FLAG VALUE` |
| `fly FILE` / `fly group N` | frame a file from the front (below) |
| `key NAME` | press a scene key: `g c v end home bracketleft bracketright` (a stopgap until those keys are verbs) |
| `panel debug\|hud\|sessions\|console` | toggle a panel |
| `live next\|prev\|reset` | step a live source |
| `sessions refresh` | rescan the agent sessions |
| `shot`, `copy-pose`, `help`, `clear` | save the next frame; print and copy the `--cam-pose` line; list the grammar (the app words, then the verb heads); empty the log |
| `# ...` | a comment: logged, runs nothing |

The scene's own carrel moves (a beat step that slid the window) were logged
as a `#` note carrying the request, since the active beat and desk scroll
have no flag and no line can say them.

## Keys

| Key | Line |
|---|---|
| F1 | `panel debug` (yields to a focused text field) |
| F2 | `shot` (works regardless of focus) |
| F3 / F4 / F5 | `live next` / `live prev` / `live reset` |
| F6 | `--layout-mode <next>`: shelf, carrel, library, back to shelf, computed from the scene choice |
| F7 | `panel sessions` |
| F8 | `panel hud` |
| F9 | `panel console` |
| Cmd-K (macOS) / Ctrl-K | toggle the palette, consumed before any text field sees it |
| Esc / backquote | release / toggle the mouse-look grab (direct input, not lines) |

Key routing (ported to the egui window on 2026-11-01 as `app::route_key`,
and worth matching in makepad's `input.rs`): a key PRESS a focused text
field took never reaches the scene; a RELEASE always does, so a fly key held
while a field takes focus cannot latch; a press with Cmd or Ctrl held never
reaches the scene (the camera ignores modifiers, so Cmd-S flew backwards).

## The features

**The bus and executor** (`bus.rs`, `exec.rs`). `push_line` queues a line;
`drain`, called between frames where the relayout was consumed, parses and
runs each FIFO and logs the line with its replies. A command queued while
running waits for the next drain. Commands: `Op` (through `apply_op`),
`Relayout` (one request), `Fly`, `Pick` (Group/Clear), `App` (shot, pose,
live, panel, sessions, help, clear), `SceneKey`. A `--cam-pose` replies with
the pose it set, in the flag's form (the applier itself returns `None` for
it). The window's own lines are plain string constants in `bus::lines`, and a
test parses every one.

**The console** (`panels/console.rs`, F9). A REPL over the bus: the log (every
line the bus ran, from any source, with its replies, capped at 500 entries,
oldest first out, stick to bottom), an input line in the same grammar, and a
history (up and down walk the lines typed, newest first; down past the newest
restores what was being typed). It is the typing-isolation trap: typing
WASD/h/g/t/x there must move nothing.

**The palette** (`palette.rs`, Cmd/Ctrl-K). A fuzzy list of every line the
window knows. The rows: the verb heads; the live settings, each as its flag
line (`--field-mode derived`, from the `ValueEnum` variants, only those the
scene has); `fly <path>` per file; `--agent-session "<path>"` per scanned
session; the app words. Ranking: a fuzzy subsequence score, case-insensitive
(a run of matches 3 a character, a word start 2, other 1, less a point per
16 characters of target; whitespace in the query ignored), the better of
title and line, ties to the shorter line and then the line, so the order
never depends on the catalogue's. Up and down move, Enter fires a row that
parses as it stands, Tab (or Enter on a template such as `nudge-glyph` or
`pick FILE[:ROW[:COL]]`) fills the console up to the first placeholder word
(one with a capital, or `...`), Esc closes.

**Fly** (`bus::fly_row`, `bus::fly_pose`, `exec::fly`). `fly FILE` takes the
exact `rel_path` first, else the first path containing the text (so `a.rs`
does not land on `ba.rs` when `a.rs` exists); `fly group N` the first row of
that group. Two steps: add the row to `browser_rows` so the scene refreshes
its live pose, then once a frame has rendered, frame it: camera facing -Z at
the box centre, at `fit_distance(max(half_h, half_w / aspect))`, as a
`--cam-pose` op. The file browser's click is `pick group N` then
`fly group N`.

**The panels** (`panels/`). Debug (F1: camera, pick and grab readouts, verb
buttons, LOD tuning, layout controls, the file browser), HUD (F8), Sessions
(F7: Load is an `--agent-session "<path>"` line, Refresh `sessions refresh`),
Carrel (transport as `key ...` lines, the four sliders as ONE flag line,
`--deck-window-limit N --deck-scroll-offset N --desk-revision-limit N
--max-file-stacks N`). Each is a pure view that pushes lines. Makepad has all
four already; the rebuild is to make their actions lines.

## Engine seams it used, and where they stand

| Seam | State on 2026-11-01 |
|---|---|
| `cli::apply_op(scene, op)`, the one op applier | ported (`worktree-stranded`); offscreen, the window and the makepad host call it |
| `cli::VERB_HEADS`, the verb list `parse_verb` knows | ported; the palette and `help` list it |
| `--max-file-stacks N` | ported; every Carrel slider is a flag |
| `SceneLike::apply_pick` / `apply_verb` / `set_cam_pose` / `apply_highlight_sidecar` / `on_key` / `cam_pose` | live (device-free since 2026-10-31; keys are `scene::SceneKey`) |
| `relayout::{RelayoutRequest, rebuild_for_relayout}` | live in the engine since 2026-10-26 |
| `try_build_ops`: `build_ops` without the panic, so a typed `--pick-row` with no `--pick-file` is an `Err` | NOT ported (no consumer yet); `bfd9773` at the tag. The console needs it, made `pub` for the host |
| `RelayoutRequest` with absolute `cluster_mode` / `layout_mode` instead of `toggle_cluster` / `toggle_layout`, plus `next_layout_mode`, `other_cluster_mode` | NOT ported; `c241a71`. Needed for law 4: a toggle has no flag spelling. Today's toggles' callers are the makepad host's `ToggleCluster` / `ToggleLayout` |
| A carrel patch (each of the four limits optional) so one slider's line moves only its own value | NOT ported; in `0866430` (`app::CarrelPatch`) |
| `glyph_scene::fit_distance`, `UiProbeState::{files, file_dyn, browser_rows}` | live |

## Follow-ups the shell named, not built

- The scene keys as verbs (`carrel-next`, `carrel-prev`, `grab`,
  `grab-carrel`, `environment-toggle` in `parse_verb`), so `key NAME` goes.
- `--pick-group N` on the CLI, so `pick group N` is sugar too (`--pick-file`
  cannot stand in: it takes the first path containing the text).
- Docking, as a view change over the same panels.

## How it was checked, and what to carry

Headless tests at the tag (`bus.rs`, `palette.rs`, `panels/console.rs`):
console lines lower to the same op stream as the CLI argv; short forms and
flag lines agree; malformed lines are refused without a panic; relayout
flags become absolute requests; quoted paths survive the splitter; every
line the window writes parses; `help` lists every app word; fly resolves
the exact path first and frames with the browser's formula; the log ring;
the history; palette ranking. Mutations `console-pick-sugar-row-col-swapped`
and `split-args-drops-quotes` proved them. The parser, ranking and fly
formula have no egui in them and could move into the engine as they are,
so the makepad host and its tests share one copy.

Not checked by anything but a human: palette focus, Cmd-K against Ctrl-K,
typing isolation in the fields, history keys, stick-to-bottom, IME.
