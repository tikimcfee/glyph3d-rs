# Stage M (M1) — the interface shell: command bus, docked panels, palette

## Goal

Give the windowed renderer a real interface skeleton, inheriting the web app's
proven constitution (notes/11): **everything is a command-bus verb**, panels are
one-way `{fire, reflect}` bindings, and the palette teaches the grammar it fires.
M1 = the bus + four docked panels (HUD/files/inspector/console) + the ⌘K palette,
all windowed-only, zero offscreen bytes moved, zero new dependencies, zero WGSL
edits. The full-system design (editor, LSP-as-camera-flight, agent streams,
terminals, books, docking, capture) is staged in
`integration/notes/11-interface-design.md`; M1 is its substrate.

Worktree: `.claude/worktrees/interface`, branch `worktree-interface`, off
`main` @ `3bf99f2` (which already carries Stage L1/L2).

## Result

Landed as one commit (see git log on the branch): `native/src/ui.rs` (new,
~1000 lines incl. tests), `windowed.rs` (EguiUi slims to ctx/state/renderer +
`UiState`; the K3–K5 Debug-window closure is replaced by `draw_ui` + an
executor), `main.rs` (`mod ui`, `parse_verb` → `pub(crate)`, help text).
**`tools/check-all.sh`: ALL GATES GREEN** — the four oracle PNGs byte-equal by
construction (offscreen never compiles a single ui.rs line: the module is
`#[cfg(feature = "egui-ui")]` and offscreen installs no probe and no egui).

## The architecture that landed

- **`ui::Command`** — the bus: `Verb` / `PickRowCol` / `FlyFile` / `FlyFileBox`
  / `Screenshot` / `TogglePanel` / `Help` / `ClearLog`. Pure data.
- **`windowed.rs::execute`** — the single executor. Scene-facing branches call
  the exact `SceneLike` APIs the CLI op-stream calls (`apply_verb`,
  `apply_pick(PickCommand::RowCol)`, `set_cam_pose`); the console log records
  the echo (`format_command`) plus the same result line the CLI prints.
  Panel⇔CLI equivalence stays by construction: parameterized verbs are
  composed as CLI strings and parsed through `crate::parse_verb`.
- **`ui::draw_ui(root_ui, &UiInput, &mut UiState) -> Vec<Command>`** — pure
  frame build inside `run_ui`; commands execute after the pass closes, before
  `queue.submit` (same record→write→submit ordering K3's buttons had).
- **Composition through the bus**: the files panel's hide/show is
  `PickRowCol{file,0,0}` + `Verb(SetHidden)` — the op-stream's own targeting.
- **K4 carve-out preserved**: the LOD slider writes the probe cell directly
  (continuous tuning ≠ intent); it is the only sanctioned direct write.

## As-executed details worth keeping

- **egui 0.36 API truths** (verified against `integration/egui` source during
  the build, since the K handoff's `Window::show(ctx)` era partially aged):
  `Panel` is one type with `left/right/top/bottom(id)` constructors, builder
  methods `default_size/min_size/resizable`, and — the big one —
  `Panel::show(&mut Ui, …)`, NOT `show(&Context, …)`. `Window::show` remains
  Context-based. `Label` has no `text_color`; styling goes through
  `RichText::new(..).small().color(..)`. The palette subtitle styling uses it.
- **winit 0.30 removed `KeyEvent::modifiers`** — chord hotkeys need tracked
  state: `WindowState::modifiers` fed by `WindowEvent::ModifiersChanged`
  (routed before the egui-consumed arm so it never misses), read via
  `.super_key()/.control_key()` for ⌘K/Ctrl-K.
- **F1 semantics grew**: it now toggles the whole shell (the K3 Debug window's
  content is fully redistributed into HUD/Inspector/Files — FPS/camera/pick →
  HUD+Inspector, verb buttons → Inspector (+ parameterized forms: tint color
  picker, scale drag-values, nudge steppers), LOD dial + cull counters →
  Inspector, group browser → its own left dock with filter + hide/show).
  K2's typing-isolation is now exercised by three real fields (console input,
  palette query, files filter) instead of the scratch box.
- **⌘K/Ctrl-K is app-level** (matched above the egui-consumed arm, like F2 —
  a chord never types); opening the palette un-hides the shell; Esc via the
  window's close button or ⌘K again closes it.
- **Console**: verb grammar + `help|clear|shot|panel <id>|fly <substr>`,
  ↑/↓ history recall (arrows also nudge the caret in a single-line field —
  noted tradeoff), colored log ring capped at 500 lines (Cmd blue, Result
  gray, Error red, Info dim).
- **Palette**: fuzzy subsequence matcher (contiguous/word-start bonuses,
  case-insensitive both sides — the one-line query-side lowercase was a real
  bug the test caught), entries = verbs (zero-arg fire directly;
  parameterized ones and `fly` complete into the console input — Tab or
  Enter), `fly: <file>` rows frame the file's AABB through `FlyFileBox`,
  app commands. Every row's subtitle is the exact line fired or filled.
- **`fly` framing** uses the K5 formula verbatim, hoisted to
  `ui::fly_to_framing` (unit-tested with the box-fit cases); the executor
  computes it with the LIVE viewport aspect, which is why `UiInput` carries
  no viewport.

## Verification summary

| Gate | Result |
|---|---|
| `tools/check-all.sh` (all 8 steps) | ALL GATES GREEN — run twice (pre- and post-seed-line polish); engine suites report "all 16 green, CPU + GPU" |
| cargo build --release | 0 warnings |
| cargo clippy --release | 0 warnings |
| cargo test | 38 green (28 prior + 10 new: console grammar ×3, fuzzy, fly framing, format_command echo, palette literals, log ring cap) |
| four-view A/B | byte-equal (offscreen untouched by construction) |
| `--no-ui` / feature-off | unchanged paths; ui.rs not compiled without `egui-ui` |
| windowed smoke + proof PNG | `out/m1-shell-fixture.png` (fixture repo, frame 90, 60 FPS with the shell live): HUD/files/inspector/console all present and non-overlapping, field visible center — inspected on the captured pixels, per the K1-erratum lesson |

Windowed manual matrix (owner smoke pending — this is the "do you like this"
build): F1 shell toggle, ⌘K palette (query/↑↓/Tab/Enter/Esc), console
(`help`, `tint-cycle` after a click-pick, `fly alpha`, `panel console`,
`shot`), files panel (filter/click-fly/hide/show), inspector (verb buttons,
tint picker, LOD slider), K2 isolation (typing WASD/h/g/t/x in any field),
grab interplay (right-drag over panel releases per K2 rules).

## File diffs

| File | Change |
|---|---|
| `native/src/ui.rs` | NEW — bus, UiState/UiInput, panels, palette, matcher, 10 tests |
| `native/src/windowed.rs` | EguiUi → ctx/state/renderer + UiState; Debug closure → draw_ui + executor; ⌘K + ModifiersChanged arms; F1 → shell; `windowed_shot_path()` shared by F2/`shot`; title/help text |
| `native/src/main.rs` | `#[cfg(feature="egui-ui")] mod ui`; `parse_verb` pub(crate); AFTER_LONG_HELP + module header |
| `integration/notes/11-interface-design.md` | NEW — the whole-system design + M-staging + open questions |
| `out/STAGE_M_REPORT.md` | this file |

## Remaining gaps (the honest list)

- The console's seeded "shell ready" Info line (gray 185 on dark after one
  brightness pass) was not unambiguously resolved in the scripted capture —
  the panels are proven, the line's legibility at 1× is the owner's 2-second
  call; if it reads dim, bump the Info color or give the log a leading blank.
- Palette keyboard state is per-frame `ui.input` polling (works; a future
  stage may want egui's built-in focus/activation flow if it ever misbehaves).
- Console history recall fights the caret only cosmetically (single-line
  field); a dedicated history widget is an M2 nicety.
- The HUD pick line truncates at 80 chars; the Inspector holds the full line.
- Everything in notes/11 §"rest of the system" (M2+): code viewer, streams,
  LSP, terminals, books, real docking, session persistence, capture.

## Read next

`integration/notes/11-interface-design.md` — the M2+ plan and the four open
questions asked of the owner alongside this build.
