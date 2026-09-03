# 11 — INTERFACE DESIGN (the Stage M series) — the native IDE-in-a-glyph-field

> **STATUS: M1 implemented alongside this document** (worktree `interface`, branch
> `worktree-interface`). The doc is the whole-system design; each M-stage cites it.
> Lineage: the second-pass GUI of `viz-web/glyph3d-js` (surveyed 2026-09-02 — the
> command bus, Books, terminals, CameraDock/Carrels, palette, agent streams), the
> verdict records in notes 04/05, and Stage K's landed egui seam (`09-egui-ui-handoff.md`,
> `out/STAGE_K_REPORT.md`). Stage L (`08`) remains pending and orthogonal until its L3
> phase; everything here that needs multi-view says so.

## The one-paragraph thesis

The web app proved the shape: **everything is a command-bus verb**, panels are thin
`{fire: verb, reflect: state}` bindings, and the 3D glyph field is the working
surface — 2D panels only orbit, navigate, and feed it. The native port inherits that
constitution wholesale, with one structural gift the web never had: **there is no
relay**. The native binary is already the host — files are on disk, language servers
and shells are stdio children, agent sessions are JSONL files. Every web concept that
existed to bridge browser↔host (WebSocket relays, tmux adapters, GitHub trees)
collapses into direct reads, and one new capability opens that the web could not
have: **anything with a (file, row, col) address can fly the camera**, because that
is literally the field's coordinate space.

## The laws (inherited; amend only with a stage report)

1. **The bus is the only action path.** Every user intent — button, palette pick,
   console line, keyboard verb, future LSP goto — becomes a `Command` dispatched by
   one executor that calls the exact `SceneLike` APIs the CLI op-stream uses
   (`apply_pick` / `apply_verb` / `set_cam_pose`). The determinism chain never
   learns the UI exists (Stage K fence 4, unchanged forever).
2. **Panels own no behavior.** One-way state→view; they read a snapshot
   (`UiProbeState`) and enqueue `Command`s. A panel never touches the scene, a
   buffer, or the engine directly. The only sanctioned write-cell is the K4 probe
   seam (`lod_min_px`) — live-tuning dials, not intents, and offscreen never
   installs the probe.
3. **The palette teaches the vocabulary.** Every palette/console row's subtitle IS
   the exact command line it fires (web `CommandBar` law). The console accepts the
   same grammar, so the palette is the tutorial and the console is the REPL.
4. **Camera moves are explicit.** Nothing moves the camera as a side effect of
   existing (web AgentsPanel law: "NOTHING here moves the camera"). Navigation is
   always a visible command: click-to-fly, `fly <substr>`, future `goto` from LSP.
5. **The frame is the proof.** Offscreen bytes are the oracle (AGENTS.md); UI lives
   only in the windowed layer behind `--no-ui`/`egui-ui`; composed-frame captures
   (K6) are the windowed evidence seam.

## The M1 architecture (landed)

All in `native/src/ui.rs` (feature `egui-ui`), driven from `windowed.rs`:

- **`Command`** — the bus: `Verb(Verb)`, `PickRowCol{file,row,col}`, `FlyTo{center,half}`,
  `Screenshot`, `TogglePanel(PanelId)`, `ConsoleHelp`, `ClearLog`. Pure data; the
  executor lives in `windowed.rs` where the scene/window/capture handles live.
- **`UiState`** — panel visibility, console input/history/log ring, palette state,
  files filter + selection. All UI memory in one struct (session persistence later
  serializes exactly this).
- **`draw_ui(input) -> Vec<Command>`** — pure frame build: HUD (top), Files (left),
  Inspector (right), Console (bottom), palette popup. Commands execute after the
  egui pass closes, through the executor.
- **Composition through the bus** — panel actions that need a target compose
  commands: "hide this file's group" = `PickRowCol{file,0,0}` then `Verb(SetHidden)`,
  exactly what the CLI op-stream would do.

Panels in M1:

| Panel | Content (all reflecting `UiProbeState` + the log) |
|---|---|
| **HUD** (top strip) | FPS, camera eye/yaw/pitch, instance count, cull sums, last-pick line, panel-toggle menu, ⌘K hint |
| **Files** (left) | the K5 virtualized group browser promoted to its own dock: filter, per-row tint swatch/hidden state, click = select + fly (K5 framing), hide/show per row (pick+verb composition) |
| **Inspector** (right) | pick detail, verb buttons incl. parameterized forms (tint color picker, scale slider, nudge), K4's LOD slider + cull readouts, camera readout |
| **Console** (bottom) | the command log (every executed `Command` + its result line) + input line: verb grammar + `help`/`clear`/`panel <id>`/`fly <substr>`/`shot`, ↑/↓ history |
| **Palette** (⌘K/Ctrl-K) | fuzzy subsequence search over verbs (arg-templates insert into the console), files (fly-to), app commands; subtitle = exact fired line; Tab completes, ↑/↓ select, Enter fires |

F1 now toggles the whole shell (was: the K3 Debug window; its content is fully
redistributed into HUD/Inspector/Files — K2 typing isolation is now exercised by the
console/palette/filter fields for real). The K4 selftest hook is untouched (it writes
the probe cell directly).

## The rest of the system (staged; each stage = commits + gates + report)

- **M2 — code viewer + agent streams.** A read-only file viewer panel (monospace
  egui text, line numbers, highlights the last pick's row; reads from disk on
  selection) — the EditorPanel mirror, pre-editing. A **Streams** panel tailing
   Claude Code session JSONL from `~/.claude/projects/**`: sheets as log entries
   (tool call / text / thinking), click = `fly` to the file it touched. This is
   AgentBooks minus the 3D — and it needs no relay, just `std::fs` + a poll timer
   inside `about_to_wait`.
- **M3 — LSP + goto-as-camera-flight.** Spawn `rust-analyzer` (configurable) as a
   stdio child; JSON-RPC with a tiny hand-rolled codec (no serde_json dep without a
   mini-stage — or justify it then; the crate already parses JSON nowhere, so this
   is the stage that likely buys serde_json deliberately). Panels: diagnostics list
   (click = fly), hover-in-inspector, symbol search in the palette. The defining
   feature: `goto-definition` and `find-refs` resolve to (file,row,col) and the
   camera flies there. Fold range into `FlyTo{.., row}` — a targeted zoom that
   frames the pick cell (extends the K5 framing formula with a row-window).
- **M4 — terminals.** Real PTYs: `openpty` via libc on macOS (or `portable-pty` if
   a dep-stage prefers), a child shell, byte pump on the event loop's idle path.
   The native payoff the web had to fight for: **the terminal renders AS a glyph
   field region** — a live group whose cells are glyph instances (the arena/verb
   partial-upload machinery from Stage G is exactly the write path needed; the
   Mojo engine is NOT in this loop — terminal cells go through `text.rs` staging).
   A 2D egui mirror panel can come first if the in-world grid slips; input routing
   needs a keyboard-capture tier (web's Ctrl+Shift+Enter lesson).
- **M5 — Books.** The universal container, in-world: deck/splay page-pair spreads
   for files (library), agent sessions (live sheet append), diffs (verso base /
   recto head), search results (windowed materialization). This is the largest
   scene-side lift (new group layout grammar beyond the flat repo grid) and gets
   its own design note before code. 2D stopgap: the Streams panel from M2 carries
   agent books; search lands in the palette first.
- **M6 — docking grows up.** M1's fixed docks (left/right/top/bottom) become a
   tab/split tree (web IdeDock ≈ `egui_dock`; prefer vendoring into `integration/`
   over a crates.io dep, matching the egui pattern, or hand-roll a two-level
   tabbed pane if the dep fight isn't worth it). Layout + UiState persist to a
   session file (`out/` scratch first, then a real config path). Canvas-hosted-in-
   a-panel stays **post-L3** (note 08's sRGB wrinkle is already recorded there).
- **M7 — capture & sensors.** Desktop capture via ScreenCaptureKit (a real
   platform integration — its own stage, likely needs a small `objc`-surface or
   a screen-capture crate decision); the FrameGrid idea (screen as movable glyph
   cells) rides the M5 book/group grammar. Minimap rides L3's second view.
- **Editing (the long game).** In-world editing = `edit.start/goto/stop` verbs on
   the engine-backed layout: a caret state in the scene, codepoint-consistent
   re-stage of the edited file's group (web's `_relayoutPreservingCursor` lesson),
   write-back to disk as an explicit `save` command (never silently). The engine
   relayout cost per keystroke is the open question — the batched corpus path
   exists for exactly this class of problem. Design note before code, after M3
   (LSP hover wants a caret anyway).

## Dependencies & prerequisites map

| Stage | Needs | New deps (each its own mini-stage per AGENTS.md) |
|---|---|---|
| M2 | none (M1) | none |
| M3 | M1 | serde_json (likely), lsp server binary at runtime |
| M4 | M1 | libc (or portable-pty) |
| M5 | M4 ideally (terminal books), M2 (agent books) | none |
| M6 | M1 | egui_dock (vendored?) — optional |
| M7 | L3 for in-panel views; M5 for FrameGrid | ScreenCaptureKit surface |
| Editing | M3 + engine relayout measurement | none |

## Open questions for the owner (asked with the M1 build)

1. Does the shell composition feel right — HUD/Files/Inspector/Console around a
   full-bleed field — or should panels default-hidden with the field bare until
   summoned (the web's overlay/inline chrome modes)?
2. Is the palette-as-tutor (subtitle = fired line) worth keeping as a law, or noise?
3. Streams-from-`~/.claude` (M2): reading agent session files directly — in or out
   of scope by preference?
4. Terminals in-world (M4) vs 2D-first: which proves the concept faster for you?
