# The windowed interface shell: one command bus (2026-10-24)

What landed, and the laws it holds. This supersedes the laws of
`integration/notes/11-interface-design.md` on the unmerged `worktree-interface`
branch (commit `cf0f1d2`, "M1"), whose ideas it brings to today's tree. M1's
code was not merged: its command type, inverse formatters and fixed docks had
rotted against main (22 verb heads against its 11; Sessions and Carrel
replaced its Streams). What survives is its constitution, amended below.

## The laws

1. **The bus is the only action path.** Every intent the window acts on — a
   panel button, a slider release, a hotkey, a palette row, a console line, a
   self-test step — is a command LINE pushed onto the bus
   (`WindowState::push_line`) and run between frames by one executor
   (`windowed/exec.rs`), through the CLI's own calls: ops through
   `cli::ops::apply_op` (the applier offscreen uses), rebuilds through
   `app::apply_relayout`. The determinism chain never learns the UI exists:
   offscreen sees only `apply_op`.

2. **Panels own no behaviour.** Each panel (`windowed/panels/`) reads the
   probe snapshot and `UiState` and pushes lines; none touches the scene. The
   sanctioned cell writes, each view state rather than an intent, and none
   reachable offscreen (offscreen never installs the probe):
   - the K4 live LOD tuning (the Text detail / Show glyphs / File rectangle
     sliders, greeking, file backgrounds, debug tint);
   - the z dial's own displayed value (its release pushes the line);
   - `UiProbeState::browser_rows`, the rows the file browser drew, which the
     scene refreshes live poses for. The executor writes it too: a `fly` adds
     its row so the pose it frames is live (amending M1's law 2, whose only
     carve-out was `lod_min_px`).

3. **The palette teaches the vocabulary.** Every palette row is `{title,
   line}` and its subtitle is the exact line Enter fires; a row that still
   needs arguments fills the console instead. The console takes the same
   grammar and logs every line the bus ran, whoever pushed it, so a click
   shows the line that would have done it. (M1 broke its own law here: its
   subtitles read "verb: tint-cycle".)

4. **The line is the command; the CLI is the parser.** A line starting with
   `--` is parsed by `Cli::command()`, `Cli::from_arg_matches` and
   `cli::ops::try_build_ops`; every short form (`tint-cycle`, `pick
   a.rs:4:5`, `pose`, `set FLAG VALUE`) is sugar that becomes those argv
   words. Panel ⇔ console ⇔ CLI equivalence is by construction, not by a
   hand-written inverse formatter (M1's `format_command` / `verb_line` /
   `VERB_WORDS` are not ported). The window's own lines are plain strings in
   `bus::lines`, and a test parses every one. A typed launch flag is refused
   (`--load-repo is a launch flag`). Relayouts carry absolute values, never
   toggles, so each is a real flag (`--layout-mode library`).

5. **Camera moves are explicit.** Nothing moves the camera as a side effect;
   navigation is a visible line (`fly FILE`, `fly group N`, `pose ...`).
   Hand input — WASD, mouse-look, click-pick, scroll — stays direct input,
   not a command.

6. **The frame is the proof.** Offscreen bytes are the oracle; the UI lives
   only in the windowed layer behind `--no-ui` / `egui-ui`; the composed F2
   capture is the windowed evidence seam.

## What has no flag

Argv cannot spell everything a panel does, so a few app words exist, each
listed in `bus::APP_WORDS` (which `help` prints and the palette lists):
`fly FILE|group N`, `pick group N|clear` (`PickCommand::Group` / `Clear`
have no flag), `shot`, `copy-pose`, `panel debug|hud|sessions|console`,
`live next|prev|reset`, `sessions refresh`, `clear`, `help`. Two stopgaps:

- `key NAME` presses a scene key for the grab and carrel buttons
  (`g c v end home bracketleft bracketright`), until those keys are verbs.
- The scene's own carrel moves (a beat step that slid the window) are queued
  as a `#` NOTE carrying a `RelayoutRequest`: the active beat and desk scroll
  they set have no flag, so no line can say them.

`--max-file-stacks` was added (its own commit) so every Carrel slider is a
flag line.

## Floating windows, bus underneath

The panels stay floating egui windows, as they were. Docking now would
occlude the field (fly-to framing would land behind the docks) and today's
Debug content would need a redesign, not a port, with no headless check on
any of it. "Snapshot in, lines out" makes docking a pure view change later.

## How it is checked

Headless (cargo-test): console lines lower to the same `Op` stream as the
CLI argv; short forms and flag lines agree; malformed lines are refused
without a panic; relayout flags become absolute requests; quoted paths
survive the splitter; every line the window writes parses; `help` lists
every app word; fly resolves the exact path first and frames with the
browser's formula; the key routing decision (`app::route_key`); the log
ring; the console history; palette ranking (word starts first,
case-insensitive, unmatched dropped, deterministic ties) and that every
palette row fires its subtitle or fills the console. Three mutations prove
the new checks: `key-route-ignores-egui-focus`,
`console-pick-sugar-row-col-swapped`, `split-args-drops-quotes`.

Not checked by anything but a human: palette focus, Cmd-K versus Ctrl-K,
typing isolation in the console, palette and filter fields, history arrows,
stick-to-bottom, IME, the composed F2 capture, and the `GLYPH_*_SELFTEST`
runs (they need a window; no gate runs them).

## Follow-ups (not built)

- **C9 — the scene keys as verbs.** `carrel-next`, `carrel-prev`, `grab`,
  `grab-carrel`, `environment-toggle` in `parse_verb`; `verb_key`
  (`glyph_scene/interaction.rs`) maps keys to `Verb`s as the library's keys
  already do, and `key NAME` / `Command::SceneKey` go away.
- **C10 — `--pick-group N`** on the CLI, so `pick group N` is sugar too.
  (`--pick-file` cannot stand in: it takes the first path CONTAINING the
  text, so `b/a.rs` matches `ab/a.rs`.)
- **C11 — docking**, as a view change over the same panels.
- Optional: `--show-glyphs-px` / `--text-detail-px` as live lines writing the
  same probe cell (and calling `keep_lod_order`).

## Found while doing it, not fixed

- `repo-back-oblique` is not deterministic on metal-apple: ten renders of its
  command from the unchanged binary gave 4 distinct PNGs (and so does every
  binary since). It is one of the eight frames the metal-apple set awaits
  re-adoption for, so the gate is red on it anyway; but a set adopted from one
  render will not stay byte-equal.
- The FIELDMODE self-test's "after step N" lines read the new scene's probe
  before its first render fills it (`segments 0`, `instances 0`), on main as
  on this branch: its odd/even states leave no quiet frame, as ZSPACE and
  CLUSTER do. Once, on this branch, ZSPACE's after-line read `z_extent=None`
  where main's one run read the doubled extent; the display slept before it
  could be repeated, so whether that is a frame of timing this branch moved
  or a skipped acquire after the 95 ms rebuild is not known.
- Offscreen prints "op: scene does not support picking/manipulation" for every
  `--cam-pose`, which the scene does support (`apply_op` returns `None` for a
  pose). Kept: it is offscreen's stdout as it was.
