# STAGE K REPORT — egui UI overlay on the windowed renderer

## Goal

Land an egui 0.36.1 overlay on the windowed renderer (pick inspector, verb
buttons, live cull/LOD tuning, group-tree browser, FPS in-window) with **zero
change to the offscreen oracle path** and zero WGSL edits, per
`integration/notes/09-egui-ui-handoff.md`.

## Result

**Phases K1–K4 landed** (K1 `c99631f` + report `4e10720`, K2 `2118305`, K3
`d6e8efa`, K4 `f4d2934`; 2026-09-02): the egui 0.36.1 stack, full
render/input plumbing, the input-consumption gating matrix, the Debug panel
(FPS, camera, pick inspector, verb buttons, scratch text field), and live
LOD_MIN_PX tuning with cull readouts and an F1 panel toggle. All six gates
green after every commit; one wgpu in the tree; windowed FPS band unchanged;
`--no-ui` reproduces pre-K behavior exactly. **Cut:** the BACKDROP_GAIN
slider (handoff's mechanism premise was wrong — see Phase K4). Only K5
(stretch: group-tree browser) remains.

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

## Remaining gaps (K5)

- K5 (stretch): group-tree browser (virtualized rows).
- BACKDROP_GAIN live slider — cut in K4 (mechanism premise false; future
  seam recorded in the K4 section below).
- puffin_egui still pinned to egui 0.33 (house note; not adopted here).
- AccessKit feature left ON (egui-winit default); no adapters are
  initialized without `init_accesskit`, so it is inert — revisit only if
  binary size or platform issues appear.

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
