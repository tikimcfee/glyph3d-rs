# STAGE K REPORT — egui UI overlay on the windowed renderer

## Goal

Land an egui 0.36.1 overlay on the windowed renderer (pick inspector, verb
buttons, live cull/LOD tuning, group-tree browser, FPS in-window) with **zero
change to the offscreen oracle path** and zero WGSL edits, per
`integration/notes/09-egui-ui-handoff.md`.

## Result

**Stage K COMPLETE — all five phases landed** (K1 `c99631f` + report
`4e10720`, K2 `2118305`, K3 `d6e8efa`, K4 `f4d2934` + report `1adf47e`, K5
`3a4f75b`; 2026-09-02): the egui 0.36.1 stack, full render/input plumbing,
the input-consumption gating matrix, the Debug panel (FPS, camera, pick
inspector, verb buttons, scratch text field), live LOD_MIN_PX tuning with
cull readouts and an F1 panel toggle, and the virtualized group browser with
click-to-fly navigation. All six gates green after every commit; one wgpu in
the tree; windowed FPS band unchanged (60 fps vsync-capped); `--no-ui`
reproduces pre-K behavior exactly; the offscreen oracle path is provably
untouched (four byte-equal PNGs after every commit). **One cut:**
BACKDROP_GAIN live slider (handoff's mechanism premise was wrong — see Phase
K4; feasible future seam recorded).

---

## Phase K1 as-executed

### API names as they ACTUALLY resolved (vendored source, `integration/egui` @ dadf573, 0.36.1)

| Handoff/study claim | As resolved | Source |
|---|---|---|
| `Context::run` renamed → `run_ui` | **Confirmed.** `Context::run_ui(RawInput, FnMut(&mut Ui)) -> FullOutput` is the entry point | `crates/egui/src/context.rs:800` |
| `egui_winit::State::new(ctx, ViewportId::ROOT, &window, Some(scale), None, Some(max_tex))` | **Confirmed exactly** (`display_target: &dyn HasDisplayHandle`; we pass `&*window`, eframe passes the event loop — both valid) | `crates/egui-winit/src/lib.rs:138` |
| `egui_wgpu::Renderer::new(&device, format, RendererOptions::default())` | **Confirmed exactly** | `crates/egui-wgpu/src/renderer.rs:273` |
| `update_buffers` mandatory before `render` (panics otherwise) | **Confirmed** (documented `# Panic` on `render`) | `renderer.rs:504-505,964` |
| `render(&mut pass.forget_lifetime(), …)` wants `RenderPass<'static>` | **Confirmed** | `renderer.rs:496-511` |
| `free_texture` after submit | **Confirmed** | `renderer.rs:790`, `egui-wgpu/src/winit.rs:738-747` |
| `TexturesDelta` panics if dropped unapplied | **Nuance found:** the assert is `debug_assert!` (`debug` builds only) on `TexturesDelta::drop` — and `FullOutput` itself has **no** `Drop`, so eframe-style destructuring works. eframe **drains** both `set` and `free` (`winit.rs:569,744`); we do the same | `crates/epaint/src/textures.rs:335` |
| `CentralPanel::default().show(...)` | **Rename found:** 0.36 `CentralPanel::show` now takes `&mut Ui` (not `&Context`); `show_inside` is deprecated. Inside `run_ui`'s root `Ui` closure: `egui::CentralPanel::default().show(ui, |_ui| {})` | `crates/egui/src/containers/panel.rs:1212` |
| Consumption cheat-sheet (pointer/wheel/keys/Tab) | **Confirmed**; additionally verified that an empty `CentralPanel` lives on the **background layer**, so `is_pointer_over_egui` returns false and the empty UI consumes nothing | `context.rs:3054-3087` |

No deviations from the handoff's plan were needed — every API matched the
study; the two naming gotchas (`run_ui`, `CentralPanel::show(ui, …)`) were
exactly where the study warned.

### Files changed (the only four)

- `native/Cargo.toml` (+16): `egui`/`egui-winit`/`egui-wgpu` 0.36 as optional
  deps; `[features] default = ["egui-ui"]`, `egui-ui = [dep:…]`. egui-wgpu
  **without** its `winit` feature. No other new deps, no eframe.
- `native/Cargo.lock` (+777/−13): all new entries are the egui stack +
  transitive deps. The 13 deletions are **dependency-reference
  disambiguation only** inside existing package blocks (e.g. `"rustc-hash"` →
  `"rustc-hash 1.1.0"` in naga's dep list) — forced because a second version
  of those crates entered the tree. **Zero `-name`/`-version` lines: no pin
  moved, no package removed.**
- `native/src/main.rs` (+11/−3): `--no-ui` flag (clap derive, `no_ui: bool`),
  passed through to `windowed::run`; parity tests extended (default
  assertion + `scalar_flags_parse` round-trip) — all 19 tests green.
- `native/src/windowed.rs` (+174/−13): `EguiUi` struct; construction in
  `App::resumed` after `surface.configure` with the **actual** surface format
  (`Bgra8UnormSrgb` observed — never hardcoded); `on_window_event` fed FIRST
  in `window_event` with a `_ if egui_consumed => {}` arm placed after the
  lifecycle arms and before every scene-routing arm; full 0.36 frame
  lifecycle in `WindowState::render` on the same encoder after the scene
  pass, egui pass with `LoadOp::Load` and `depth_stencil_attachment: None`,
  wrapped in a wgpu-profiler `"egui pass"` query mirroring the Stage H
  scheme; `free_texture` drained after `queue.submit`, before present. All
  egui code is `#[cfg(feature = "egui-ui")]`; `cargo check --release
  --no-default-features` compiles warning-free (feature off = pre-K).

### Gates (commit c99631f)

```
PASS  build — 0 warnings
PASS  clippy — 0 warnings
PASS  tests green (18 + 1)
PASS  engine-check (34060 records bit-exact)
PASS  stage-g ALL PASS
PASS  demo.png / text.png / repo-zoom.png / repo-wide.png BYTE-EQUAL
CHECK-ALL: ALL GATES GREEN
```

Offscreen was untouched (no egui symbol is reachable from `offscreen.rs`), so
the four PNGs are **trivially byte-equal**.

### Dependency tree

- `cargo tree -i wgpu`: exactly **one** wgpu (30.0.1) — depended on by
  `glyph3d-native`, `egui-wgpu 0.36.1`, `wgpu-profiler 0.28.0`. One winit
  (0.30.13) — `glyph3d-native` + `egui-winit`.
- `cargo tree -d` duplicates **introduced by K1**: `itertools 0.15` (egui;
  rav1e keeps 0.14), `rustc-hash 2.1.3` (type-map ← egui-wgpu; naga keeps
  1.1.0), `objc2-app-kit 0.3.2` / `objc2-foundation 0.3.2` (arboard +
  webbrowser ← egui-winit clipboard/URL helpers; winit keeps 0.2.2).
  Pre-existing duplicates unchanged: bitflags 1/2, objc2 0.5/0.6, block2
  0.5/0.6, syn 2/3, miniz_oxide 0.8/0.9.

### FPS bands (windowed, `--load-repo fixtures/g-pick-repo`, 1600×1000 logical, Fifo)

| Run | Band | Mode |
|---|---|---|
| pre-K (cb35552, 35 s) | 58.2–60.0 | 60.0 |
| K1 (c99631f, 30 s, UI on) | 59.0–60.1 | 60.0 |
| K1 `--no-ui` (12 s) | 60.0 | 60.0 |

Vsync-capped at 60 Hz; the egui overlay costs nothing measurable against the
band (well within the ~2 ms/frame acceptance). `GLYPH_PROFILE=1` (10 s):
`profile: GPU egui pass 7.5–8.3ms, glyph field pass 6.8–7.5ms` — the **"egui
pass" label appears as required**. The absolute egui figure for an empty pass
is implausibly large; it is almost certainly a Metal/TBDR pass-boundary
timestamp artifact (the LoadOp::Load pass's begin timestamp waits on the
glyph pass's tile flush), not real work — FPS is flat at 60.0 and the scene
pass reports in the same range. Not investigated further in K1; flagged for
K2 if it matters.

### Decisions (from the handoff)

- **(a) sRGB surface kept.** The surface stays `Bgra8UnormSrgb` (matches the
  offscreen oracle's output space). egui logs its once-per-pipeline warning
  (`Detected a linear (sRGBA aware) framebuffer Bgra8UnormSrgb. egui prefers
  Rgba8Unorm or Bgra8Unorm`) and uses its linear-framebuffer shader path —
  **accepted**: switching the surface to gamma-space would perturb the glyph
  pass's output encoding for zero UI gain.
- **(b) Skipped frames.** No code path skips painting today (every
  `RedrawRequested` renders; surface-error paths `return` before any egui
  state is touched — no `FullOutput` exists yet at those points, so nothing
  can leak). Recorded: if a skip-painting path is ever added after
  `run_ui`, it must call `FullOutput::drop_without_applying_deltas()`.

### Verification limits (honest)

- **Live keyboard/mouse interaction was NOT manually tested** (agent cannot
  operate the window). Verified by code inspection: every scene-routing arm
  in `window_event` sits behind the `_ if egui_consumed => {}` arm;
  `device_event` raw-look is unchanged (correct — it only drives look while
  grabbed, and egui input translation is irrelevant during grab). The empty
  background-layer `CentralPanel` consumes nothing per the egui source. A
  human pass over the STEP 0 interaction inventory (WASD/ERQF, scroll,
  right-drag look, backquote toggle, Esc, left-click pick, h/g/t/x verbs) is
  the recommended first act of K2.
- Known K2 boundary (documented, not fixed): if a right-press starts a
  look-grab and the release lands over a future egui panel, the consumed
  release would leave the grab latched. Empty UI in K1 makes this
  unreachable; K2 owns the grab/UI interplay rules.

### Process hygiene

Every smoke run used `& PID=$!; sleep N; kill $PID; wait`. Post-run checks
found **no leftover process from these runs**. Note: `pgrep -f
glyph3d-native` also matches an unrelated concurrent experiment running from
`.claude/worktrees/multi-drawindirect/` (another session's `--frames 900
--screenshot` loop) — those processes were identified by full path and left
untouched.

---

## Phase K2 as-executed (commit `2118305`, windowed.rs only)

### The gating matrix (final rules, all in `App::window_event` + `WindowState::render`)

egui sees EVERY window event first (`egui_state.on_window_event`); the
returned `EventResponse.consumed` implements the study §2.2 cheat-sheet
(pointer/wheel consumed ⇔ `egui_wants_pointer_input`; CursorMoved consumed ⇔
`egui_is_using_pointer`; keys consumed ⇔ `egui_wants_keyboard_input` OR Tab,
which is always egui's). Scene routing:

| Event | Routed to scene when | Notes |
|---|---|---|
| CloseRequested / Resized / RedrawRequested | always (egui never consumes; arms match before the consumption arm) | resize handling stays ours (egui-winit never touches the wgpu surface) |
| KeyboardInput (fly WASD/ERQF, verbs h/g/t/x, Esc, backquote) | `!consumed` | a focused egui widget (K3's text field) swallows keys → typing cannot fly the camera or fire verbs; Esc/backquote are egui's whenever a widget has focus |
| MouseInput Right **Pressed** (grab look) | `!consumed` | press over a panel starts NO grab |
| MouseInput Right **Released** | **always ungrabs** — arm placed ABOVE the consumption arm | the K1 latch (grab starts outside, release lands on a panel, release consumed → grab stuck) is fixed by construction; `ungrab()` is a no-op when ungrabbed |
| MouseInput Left Pressed (pick) | `!consumed` && `!grabbed` | click on a panel never picks |
| CursorMoved | bookkeeping (`state.cursor`) ALWAYS; scene effects (look fallback, `on_cursor` group drags) only when `!egui_wants_pointer_input()` | `consumed` alone is insufficient here — it excludes hover; a g-grabbed group must not track the pointer across panels |
| MouseWheel | `!consumed` | scroll over a panel never changes fly speed |
| DeviceEvent::MouseMotion | while `grabbed` (unchanged) | raw look bypasses egui by design; coexistence is prevented by the ungrab-on-focus rule |
| Ime | never scene-routed; egui gets it first; `handle_platform_output` drives `set_ime_allowed`/`cursor_area` | candidate-window positioning quirks = known rough edge (documented, not fixed) |

### Ungrab-on-UI-focus (the grab interplay rule)

Each frame, after `run_ui` (focus/hover state fresh for THIS frame):
`if (egui_wants_pointer_input() || egui_wants_keyboard_input()) && grabbed { ungrab() }`.
Rationale: while grabbed, raw `DeviceEvent` deltas drive look and bypass
egui, and the pointer is hidden/confined — egui must not believe it is
hovered. Re-acquisition is automatic: once the pointer leaves egui's area,
`egui_wants_pointer_input()` is false, so the next right-press routes to the
scene's grab arm. Mid-look-drags over panels are NOT interrupted: while a
button is down and the drag started outside egui, egui's `any_down` clause
reports wanting nothing (verified against `context.rs:3084-3087`).

### Verification status — **verified by code inspection; human pass pending**

I cannot drive the GUI with real input events. The matrix above was verified
by tracing every event arm against the egui 0.36.1 source semantics; the
empty-UI K1 smoke and the K3 smoke ran clean. **The human pass should
exercise** (STEP 0 inventory + K2 additions): WASD/ERQF fly, scroll speed,
right-drag look, backquote toggle, Esc release, left-click pick + flash,
h/g/t/x verbs, `saw_device_delta` fallback — PLUS: type WASD/h/g/t/x into
the Debug window's text field (scene must not react), right-drag starting on
the Debug window (no grab), right-drag starting on the scene then releasing
over the Debug window (grab releases), Tab with the field focused (never
reaches the scene), IME dead-key/CJK input into the field (preedit renders;
candidate-window position may be off — document only).

Gates: `check-all.sh` ALL GATES GREEN; `cargo check --no-default-features`
clean; 10 s windowed smoke 60.0 FPS, no panic/error, no leftover process.

---

## Phase K3 as-executed (commit `d6e8efa`)

### The Debug window

One `egui::Window` titled "Debug", explicit `.id(egui::Id::new("stage_k_debug_panel"))`:

- **FPS readout** — the same 1 Hz figure the stdout line prints (mirrored
  into `WindowState.ui_fps` at the same site; the println is untouched and
  remains the offscreen-facing output).
- **Camera readout** — mode + this frame's ACTUAL eye (`CamFrame.eye`, the
  same eye the cull pass uses) + fly yaw/pitch.
- **Pick inspector** — the last pick formatted by the SAME `format_pick`
  that produces the stdout pick line; "(none yet)" before the first pick.
- **Verb buttons** — `recolor-glyph`, `recolor-line`, `tint-cycle`,
  `hide-group`, `show-group`, `toggle-hidden`. Each button parses its
  CLI-literal string through `crate::parse_verb` (the very parser the
  `--verb` flag uses) and applies it through `SceneLike::apply_verb`, with
  the returned line println'd exactly like the op stream. **Panel ⇔ CLI
  equivalence is by construction** — same parser, same entry point, same
  log format. Parameterized verbs (nudge/scale/move, tint-group rrggbb)
  stay CLI-only until a phase adds arg entry (cut deliberately, recorded
  here).
- **Scratch text field** — the K2 typing-isolation trap test.

### The readout seam (fence-4-clean; no `SceneLike` change)

Windowed holds the scene as `Box<dyn SceneLike>`, and fence 4 forbids trait
changes, so the panel cannot query the concrete scene. Solution: a
**probe cell** — `glyph_scene::UiProbeState { camera_mode, eye, yaw, pitch,
last_pick }` behind `Rc<RefCell<_>>` (single-threaded: winit's event-loop
thread owns writer and reader). `GlyphScene::init_ui_probe()` installs it on
the CONCRETE scene before boxing; `main.rs` gained `build_scene_probed`
(windowed) while `build_scene` (offscreen) keeps its exact signature and
passes `probe=false` through the shared `build_scene_impl` — construction
order and semantics byte-identical for offscreen. The probe is written once
per frame at the top of `GlyphScene::render` ONLY when installed, so the
offscreen determinism chain provably never touches it (and the four PNG
gates stay byte-equal). The Stage A demo scene has no probe — the panel
shows "n/a".

### Verification

- `check-all.sh` ALL GATES GREEN; `cargo check --no-default-features` clean.
- 20 s windowed smoke (`--load-repo fixtures/g-pick-repo --pick-file alpha
  --pick-row 2 --pick-col 5 --verb "recolor-line"`): scripted pick and verb
  produced the exact expected log lines (`pick: alpha.rs group=0 rec=20
  row=2 col=5 line=2 byte=20 char='i' slot=18 …`, `verb recolor-line:
  alpha.rs row 2 — 11 glyphs in 1 run(s), 528 B uploaded`); FPS settles to
  60.0 after a ~2 s startup transient (egui font atlas upload); no
  panic/error; no leftover process.
- **OS screenshot NOT captured**: `screencapture` is blocked by macOS Screen
  Recording permission in this environment ("could not create image from
  display"). The panel code path executes 60×/s without fault; visual
  eyeballing of the Debug window is part of the pending human pass.

### Deviations

None from the handoff. Two scope choices recorded: (1) verb buttons cover
the zero-arg/default CLI verbs only (see above); (2) the panel is always
open — there is no close/reopen affordance yet (a `.open(&mut bool)` toggle
plus a hotkey/menu is a natural K4 add; `--no-ui` remains the clean view).

---

## Phase K4 as-executed (commit `f4d2934`)

### LOD_MIN_PX — live, with offscreen consts preserved by construction

**Read-site audit (grep-verified):** exactly ONE code read of `LOD_MIN_PX`
in the crate — the `glyph_px < LOD_MIN_PX` classification inside
`cull_segments` (glyph_scene.rs). Every other mention is a doc comment or
the seed. `cull_segments` is shared by windowed and offscreen, so the
handoff's "shared read site" case applies.

**Mechanism:**
- `CullState` gains `lod_min_px: Cell<f32>`, seeded `Cell::new(LOD_MIN_PX)`
  in `CullState::new` (the `viewport: Cell` precedent — `render(&self)`
  stays immutable).
- `cull_segments` takes the threshold as part of a new `CullView` bundle
  (`planes/eye/px_scale/lod_min_px` — view-derived per-frame inputs; the
  bundle also keeps the function under clippy's 7-arg limit without an
  `#[allow]`).
- The Debug-panel slider writes `UiProbeState.lod_min_px` (seeded from the
  const at install); `GlyphScene::render` copies it into the Cell **before
  culling**, so a drag takes effect the same frame. That copy is the SINGLE
  write site and runs only when a probe is installed.
- **Proof of construction for offscreen:** probes are installed only by
  `build_scene_probed` (windowed); `build_scene` (offscreen) passes
  `probe=false`. Offscreen ⇒ no probe ⇒ no write ⇒ the Cell holds the const
  ⇒ `cull_segments` receives exactly `1.0` ⇒ gate 6's four byte-equal PNGs
  are the empirical proof (they pass).

### BACKDROP_GAIN — CUT (handoff mechanism premise was wrong)

The handoff assumed the gain lived in the Params uniform, tunable by a
per-change `queue.write_buffer`. Ground truth: `Params` (glyph_field.wgsl)
carries only the Slug minification dials (`dilate_px/soften/min_lo/min_hi`);
`cull.wgsl`'s own header documents that the backdrop color/alpha are "baked
at staging into `SegCull.tint`" — `seg_tint` computes
`E = min(ink_frac × BACKDROP_GAIN, 1)` on the CPU at staging, and that
staging path is shared by `text.rs` and `repo.rs` (offscreen included). A
live gain therefore requires either a `cull.wgsl` edit (fence 4 — forbidden)
or an `ink_frac` plumbing redesign across `seg_tint` → `CullState` →
`sync_segment` with f32/f64 rounding care — a different design than
sanctioned, touching the determinism-adjacent staging modules. **Cut per
the "don't improvise scope" rule.**

Feasible future seam (recorded, not built): store per-segment `ink_frac` at
staging (`seg_tint` returns it alongside the tint), keep the baked
`tint[3]` as the `gain == BACKDROP_GAIN` fast path (offscreen-exact), and
apply `min(ink_frac × live_gain, 1)` at backdrop-compaction time in
`cull_segments` when the slider deviates — the backdrop instance buffer is
already rewritten every frame, so the GPU cost is zero. `sync_segment`'s
alpha handling needs the same treatment. That is its own mini-stage with an
A/B suite, not a fence workaround.

### Live cull readouts

The Debug window shows `cull: N draw ranges, M instances | K backdrops` —
the same sums `GLYPH_CULL_DEBUG` prints at t=0, written into the probe every
frame (only when installed; zeros under `--no-cull`). The probe refresh
moved to a single point after the cull section so counters and camera/pick
state are written together.

### Panel close/reopen

`.open(&mut debug_open)` on the Debug window (its own close button works)
plus **F1 toggles** it. F1 sits in the unconsumed-key path of the K2 matrix
and is never scene input; if egui has keyboard focus it consumes F1 first
(Esc the field, then F1) — acceptable, does not complicate the matrix.

### Verification

- `check-all.sh`: ALL GATES GREEN (0 build + 0 clippy warnings; 19 tests;
  engine-check bit-exact; stage-g PASS; four PNGs byte-equal — offscreen
  kept the const, per above).
- `cargo check --release --no-default-features`: clean.
- **`GLYPH_K4_SELFTEST=1`** (new dev-only env var, documented in AGENTS.md):
  moves the slider programmatically at t≈3 s and logs the counters:
  `before: lod_min_px=1.00 → ranges=4 instances=10857 backdrops=0`, then
  `after: lod_min_px=16.00 → ranges=0 instances=0 backdrops=4` — the full
  panel → probe → `CullState` → `cull_segments` write path proven in a live
  run. (First iteration used 64.0 and exposed that egui sliders clamp
  out-of-range values into the range on show — harmless in real use since
  the slider is the only writer; the selftest now uses the range max.)
- FPS held at 60.0 in both K4 runs; no panics; no leftover processes.
- **Human pass pending:** visually dragging the slider trades glyphs for
  backdrop quads (mechanism proven by the selftest; the visual judgement
  needs eyes), and F1 toggle feel.

### Human-pass checklist additions

- Drag LOD_MIN_PX up: visible segments collapse into backdrop quads; drag
  back down: glyphs return. Counters in the panel track the change live.
- With the pointer over the slider, scroll must NOT change fly speed (K2
  wheel gating) and WASD must not fly while the scratch field is focused.
- F1 closes and reopens the Debug window; the window's own × closes it.
- `--no-ui` runs keep pre-K behavior (no panel, F1 inert).

---

## Phase K5 as-executed (commit `3a4f75b`)

### Design choice: FLAT virtualized list (not a tree)

`egui::ScrollArea::show_rows` builds only the visible rows, so per-frame
work is bounded by the viewport, not the file count. A
`CollapsingHeader`-per-directory tree is **not** virtualized — every header
would be laid out every frame, the classic egui large-list trap at ~1.3k
files (study §4). A properly virtualized collapsible tree needs a per-frame
visible-row flattening pass over an expansion-state set — real complexity
for a stretch goal. So: flat list, path-indented by directory depth, with a
substring filter field. **Vanilla egui only — `egui_ltreeview` was not
needed and no new dep was added (fence 2).**

Rows: tint swatch (painter-drawn rect — no font-glyph dependency), the
rel_path, `(hidden)` marker. Filter field doubles as a second K2
typing-isolation test.

### Data flow (read-only probe extension — no trait/WGSL/offscreen contact)

- `UiProbeState.files: Rc<Vec<UiFileRow>>` — STATIC (rel_path, group_id,
  local-space AABB from `PickFileInfo`), built once at `init_ui_probe` from
  the pick context (repo scenes only; empty for text/engine/demo → panel
  hides the browser). Rc-shared so the panel's per-frame snapshot clones a
  refcount, not the rows — the large-list trap applies to widget
  construction AND to data cloning; both are avoided.
- `UiProbeState.file_dyn: Vec<UiFileDyn>` — refreshed per frame in the
  probe write: world center/half under the LIVE group TRS (`groups_cpu`
  offset+scale × local AABB — the same math `sync_segment` uses), hidden
  from the group row alpha (`cols[2][3] == 0.0` — the same place the
  hide/show verbs write, so it's correct even under `--no-cull`), tint
  bytes from `cols[2]`. ~1.3k cheap iterations/frame at repo scale; never
  executed offscreen (no probe installed → PNGs byte-equal, gate 6 passes).

### Navigation: IMPLEMENTED (not cut)

Row click = selection highlight + fly-to via the exact `--cam-pose` API
(`SceneLike::set_cam_pose` — note 04's rule holds). The pose mirrors
`camera_eye_target`'s Front framing:
`dist = max(half_h, half_w/aspect) / tan(FOV_Y/2) × 1.08 + 2.0`,
`eye = (center.x, center.y, dist)`, yaw/pitch = 0 (text plane faces +Z).
`FOV_Y` was made `pub` for this (read-only const export). Because
`file_dyn` refreshes per frame, clicking a group that was moved by a verb
flies to its CURRENT pose.

Click deliberately does NOT call `apply_pick`: `PickCommand::File` matching
is substring-based ("first file whose rel path contains"), so a row click
could flash a different file whose path merely contains this one. An
exact-match pick variant would be new scene surface — skipped to stay
fence-clean; recorded here as the follow-up if pick-from-browser is wanted
(add `PickCommand::ExactFile(String)` or match by `group_id`, then the
inspector + flash can follow row clicks).

### Verification

- `check-all.sh`: ALL GATES GREEN; `cargo check --no-default-features`
  clean.
- 30 s windowed smoke on `fixtures/g-pick-repo` with the browser open:
  FPS 59.9–60.1 (mode 60.0) — band held; no panic/error; no leftover
  process.
- **Honesty note:** the fixture has ~5 files. The 1.3k-file scroll
  responsiveness is asserted BY CONSTRUCTION (show_rows builds only visible
  rows; the filter scan is a bounded O(n) substring pass; static rows are
  Rc-shared) — NOT measured. Human pass on a real repo pending (checklist
  above).

---

## Remaining gaps (end of stage)

- **BACKDROP_GAIN live slider** — cut in K4 (mechanism premise false; the
  gain is staging-baked into `SegCull.tint`, not a uniform). Feasible future
  seam recorded in the K4 section (store `ink_frac` at staging, apply live
  gain at backdrop-compaction time; needs its own mini-stage with A/B care).
- **puffin_egui** still pinned to egui 0.33 upstream (house note; not
  adopted — `wgpu-profiler` + the K1 `"egui pass"` query already cover
  per-pass GPU timing; no puffin integration was attempted).
- **AccessKit**: egui-winit's `accesskit` feature is ON (its default) but
  **inert** — `State::init_accesskit` is never called, so no adapter is
  created and no tree is built. Left on: disabling it saves compile time
  only and would fight the crate's default-feature surface; revisit if
  binary size or platform issues appear.
- **IME**: comes free via egui-winit (`Ime` events → preedit;
  `handle_platform_output` drives `set_ime_allowed`/`cursor_area`).
  Candidate-window positioning quirks are a known rough edge upstream —
  documented, not fixed. Human smoke with a dead-key/CJK input pending.
- **glyphon labels** (roadmap item 6) deferred — untouched by this stage.
- **Human interaction pass pending** — consolidated checklist at the bottom
  of this report.

## Verification summary (whole stage)

### Gates per commit

| Commit | Phase | check-all.sh |
|---|---|---|
| `c99631f` | K1 deps + plumbing | ALL GATES GREEN |
| `2118305` | K2 gating matrix | ALL GATES GREEN |
| `d6e8efa` | K3 Debug panel | ALL GATES GREEN |
| `f4d2934` | K4 live LOD_MIN_PX | ALL GATES GREEN |
| `3a4f75b` | K5 group browser | ALL GATES GREEN |

Every run: build 0 warnings, clippy 0 warnings, 19 tests green,
engine-check bit-exact, stage-g picks PASS, four-view A/B BYTE-EQUAL. The
byte-equal PNGs are trivially guaranteed in K1–K3/K5 (offscreen code path
untouched) and **by construction** in K4 (probe-guarded single write site —
offscreen never installs a probe, so `cull_segments` receives the const).
`cargo check --release --no-default-features` (feature off = pre-K) verified
clean at every phase.

### FPS bands (windowed, `--load-repo fixtures/g-pick-repo`, Fifo)

| Run | Band | Mode |
|---|---|---|
| pre-K (`cb35552`, 35 s) | 58.2–60.0 | 60.0 |
| K1 (`c99631f`, 30 s, UI on) | 59.0–60.1 | 60.0 |
| K2 (`2118305`, 10 s) | 60.0 | 60.0 |
| K3 (`d6e8efa`, 20 s) | 60.0 after ~2 s startup transient (font atlas) | 60.0 |
| K4 (`f4d2934`, 2 runs) | 60.0 | 60.0 |
| K5 (`3a4f75b`, 30 s, browser open) | 59.9–60.1 | 60.0 |

Vsync-capped; the egui overlay never moved the band. `GLYPH_PROFILE=1`
shows both passes (`glyph field pass`, `egui pass`); the absolute per-pass
figures carry a Metal/TBDR pass-boundary timestamp artifact (recorded in
K1), not real cost — FPS is flat.

### Dependency tree / lock

- `cargo tree -i wgpu`: exactly ONE wgpu (30.0.1) — glyph3d-native,
  egui-wgpu, wgpu-profiler. One winit (0.30.13).
- `cargo tree -d`: duplicates introduced by the egui stack — itertools 0.15
  (egui; rav1e keeps 0.14), rustc-hash 2.1.3 (type-map; naga keeps 1.1.0),
  objc2-app-kit/objc2-foundation 0.3.2 (arboard/webbrowser; winit keeps
  0.2.2). Pre-existing duplicates (bitflags 1/2, objc2 0.5/0.6, block2,
  syn 2/3, miniz_oxide) unchanged.
- `native/Cargo.lock`: +777/−13 — the 13 deletions are dependency-reference
  disambiguation qualifiers inside existing package blocks only (e.g.
  `"rustc-hash"` → `"rustc-hash 1.1.0"`); zero version pins moved, zero
  packages removed (verified line-by-line: no `-name`/`-version` lines).

### File diffs (stage total, `cb35552..HEAD`)

| File | +/- | Content |
|---|---|---|
| `native/Cargo.toml` | +16 | three optional egui 0.36 deps + `egui-ui` feature (default ON) |
| `native/Cargo.lock` | +790/−13 | egui stack + transitive deps (new entries only, per above) |
| `native/src/main.rs` | +53/−3 | `--no-ui` flag + parity-test extension; `build_scene_probed` (windowed) with `build_scene` (offscreen) as a `probe=false` wrapper over shared `build_scene_impl` |
| `native/src/windowed.rs` | +555/−19 | all egui logic: EguiUi, construction, event gating matrix, frame lifecycle, Debug window (K3–K5 content), F1 toggle, K4 selftest hook |
| `native/src/glyph_scene.rs` | +240/−17 | read-only probe types + install + per-frame write; `CullState::lod_min_px` Cell; `CullView` bundle; `FOV_Y` made pub |
| `native/AGENTS.md` | +4 | `GLYPH_K4_SELFTEST` debug env var doc |

Untouched: `offscreen.rs`, all `.wgsl`, the `SceneLike` trait,
`engine-local/`, `assets/atlas/`, fixtures, all version pins.

## Consolidated human-pass checklist (the one pending item)

An agent cannot drive the GUI; everything below is verified by code
inspection + scripted smoke and needs one human session. Run:
`./target/release/glyph3d-native --load-repo fixtures/g-pick-repo` (then a
real ~1.3k-file repo for K5 scale).

**Base inventory (STEP 0):** WASD/ERQF fly; scroll = speed; right-drag look
(confined pointer, hidden cursor); backquote toggle grab; Esc release;
left-click pick + flash; h/g/t/x verbs; g-grabbed group drags + scroll
scales it.

**K2 gating matrix:** type WASD/h/g/t/x into the scratch field AND the
browser filter — camera must not move, no verbs fire, no pick changes; Tab
with a field focused never reaches the scene; right-press ON the Debug
window starts no look-grab; right-drag started on the scene then released
over the panel releases the grab (no latch); hover the panel while a group
is g-grabbed — the group must not track the pointer across the panel;
scroll over the panel must not change fly speed; pointer over the panel
while grabbed (backquote) releases the grab; grab re-acquires after the
pointer leaves the panel.

**K3 panel:** FPS/camera readouts live and match the stdout line; click a
glyph → the pick inspector shows the same string as stdout; each verb
button produces the same log line and visual effect as its `--verb` CLI
counterpart.

**K4:** drag LOD_MIN_PX up — segments collapse into backdrop quads, panel
counters track live; drag back — glyphs return; F1 closes/reopens the
window, the window's × closes it; `--no-ui` keeps pre-K behavior (F1 inert).

**K5:** browser scrolls smoothly on a ~1.3k-file repo (virtualized —
bounded work by construction, but feel needs eyes); filter narrows rows;
click a row → highlight + camera flies to the file (group moves AFTER a
row's position was computed are reflected next frame — file_dyn refreshes
per frame, so re-clicking a moved group flies to its new pose); hidden
files show `(hidden)` and tint swatches follow t/tint verbs.

**IME:** dead-key/CJK input into a text field renders preedit; note any
candidate-window positioning oddity (document, don't fix).


---

## Post-stage erratum (commit ebaaec4, 2026-09-02)

**First human run caught what every agent smoke missed.** K1's "empty UI" used
`CentralPanel::default()`, which paints an opaque full-viewport `panel_fill`
rect — "background layer" only governs input order, not fill. Result on any
repo: Debug window visible over an opaque gray blanket, scene healthy
underneath (cull/draw/present all nominal — 127,589 instances @ 60 fps on the
integration-notes repo). Log/FPS-only smoke verification structurally cannot
see this class of bug; it needed eyes. Fix: the vestigial panel is removed
(`egui::Window` needs no CentralPanel). Gates: check-all ALL GATES GREEN.
Lesson recorded for the human-pass column: **any "invisible by construction"
UI claim requires one pixel-level check per stage** — the windowed readback
seam (COPY_SRC surface + copy_texture_to_buffer, note 10's in-window
screenshot) would make that agent-verifiable.
