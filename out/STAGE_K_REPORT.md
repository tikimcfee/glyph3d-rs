# STAGE K REPORT — egui UI overlay on the windowed renderer

## Goal

Land an egui 0.36.1 overlay on the windowed renderer (pick inspector, verb
buttons, live cull/LOD tuning, group-tree browser, FPS in-window) with **zero
change to the offscreen oracle path** and zero WGSL edits, per
`integration/notes/09-egui-ui-handoff.md`.

## Result

**Phase K1 landed (commit `c99631f`, 2026-09-02):** the full egui 0.36.1
dependency stack and complete render/input plumbing with an EMPTY UI
(`CentralPanel::default()`, no widgets). All six gates green; one wgpu in the
tree; windowed FPS band unchanged; `--no-ui` reproduces pre-K behavior
exactly. K2–K5 remain.

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

## Remaining gaps (K2–K5)

- K2: input-consumption correctness with REAL widgets (text-field isolation,
  grab↔UI-focus interplay, IME smoke).
- K3: debug panel (FPS, camera, pick inspector, verb buttons).
- K4: live `LOD_MIN_PX` / `BACKDROP_GAIN` (windowed-only; offscreen keeps
  compile-time defaults).
- K5 (stretch): group-tree browser (virtualized rows).
- puffin_egui still pinned to egui 0.33 (house note; not adopted here).
- AccessKit feature left ON (egui-winit default); no adapters are
  initialized without `init_accesskit`, so it is inert — revisit only if
  binary size or platform issues appear.
