# 09 — EGUI UI HANDOFF (Stage K) — for the implementing agent

> **STATUS: PLANNED — not executed.** No commits made by the planning pass. Written against
> `glyph3d-native` HEAD `cb35552`; supersedes the "suggested stage shape" sketch in
> `glyph3d-native/research/egui-integration-notes.md` §7 (same content, promoted to handoff
> format with fences and gates).

*You are picking up a scoped engineering task, executable without further context. Companions:
`native/AGENTS.md` (house rules — read first), `glyph3d-native/integration/egui-integration-report.md`
(the egui 0.36.1 source study — every API claim below traces to its line cites),
`research/egui-integration-notes.md` (codebase attachment map), note `04-ui-layer.md` (the verdict
record), `08-view-structure-handoff.md` (Stage L, which follows this one).*

## Who wrote this, and how much to trust it

Research-pass product, same as notes 06/08: the API facts were verified against the vendored egui
source (`integration/egui`, master @ `dadf573`, 0.36.1), but this pass never built the integration.
Your ground truth wins over the prose; the gates decide.

## Mission

Land **Stage K — the UI stage**: an egui 0.36.1 overlay on the windowed renderer — pick inspector,
verb buttons, live cull/LOD tuning, group-tree browser, FPS in-window — with **zero change to the
offscreen oracle path** and zero WGSL edits. This closes Stage G's remaining gaps #1 (selection
UI), #4 (IME), #6 (flash restore UI story) per note 04, and is the precondition Stage L was
sequenced after.

**The one-paragraph integration shape** (established pattern, verified against egui source):
egui-winit's `State` sits *first* in `window_event` and marks events consumed; scene input routing
happens only for unconsumed events. Each frame, after the scene pass on the same encoder, an egui
pass with `LoadOp::Load` and **no depth attachment** paints the tessellated UI onto the surface.
We keep our event loop, surface, and scene pass — no eframe. The heavy work is input-consumption
gating (none exists today), not rendering.

## The workspace

- Crate: `/Users/lugo/localdev/viz-native/glyph3d-native/native/`; gates: `bash tools/check-all.sh`
- Pins (must not move): **wgpu 30, winit 0.30, glam 0.33.6, edition 2021**
- **MSRV check: egui 0.36 requires rustc ≥ 1.95** (per its changelog) — verify the local toolchain
  in STEP 0 before adding anything.
- Attachment map (verified at `cb35552`): events + grab logic `windowed.rs` `App::window_event`
  (~`:299`), grab helper (~`:76–103`), raw-look `device_event` (~`:368`); the render seam
  `WindowState::render` — `scene.render` at `:125`, `queue.submit` at `:140`, present after;
  resize → `WindowState::resize` (`:57`). Line numbers drift; the symbols don't.
- The scene-facing API the UI calls is exactly the CLI op surface: `SceneLike::apply_pick` /
  `apply_verb` (+ your pick-result log lines). Note 04's rule: *panels call the exact API the CLI
  ops use; the determinism chain never learns egui exists.*
- **Handover hazard:** the egui study (`integration/egui-integration-report.md`), the vendored
  source (`integration/egui/`), and `research/*.md` are **untracked** in the glyph3d-native repo —
  but load-bearing references for this handoff. Don't `git clean` them; whether to commit them is
  the owner's call (recommended: yes, they're cited by the stage docs and cheap to keep).

## HARD FENCES — each with its reason

1. **Offscreen is untouchable.** No edits to `offscreen.rs`; egui lives only in `windowed.rs`.
   Belt and suspenders: an `egui-ui` cargo feature (default ON) and a `--no-ui` runtime flag for
   windowed smoke runs. *Why:* offscreen is the byte-compare oracle for every stage including this
   one; a UI dependency that leaks into it poisons every later gate.
2. **Exactly three new deps**: `egui 0.36`, `egui-winit 0.36`, `egui-wgpu 0.36` — the last
   **without its `winit` feature** (that pulls in `Painter`, which owns the surface and clears the
   frame — wrong for an overlay). No eframe, no egui_extras/egui_plot/egui_ltreeview without a
   stage-report justification (vanilla `egui` covers phases 1–4 below). egui-wgpu disables wgpu
   default features — our own `wgpu` entry already selects backends, so we're unaffected, but say
   so in the report. *Why:* smallest tree that closes the gaps; every extra crate is re-baseline
   surface at the next wgpu major.
3. **No existing pin may move.** `Cargo.lock` diff after K1 must be new entries only (egui stack
   + its transitive deps); `cargo tree -d` gets the usual look. egui's stack resolving against
   wgpu 30.0.1 / winit 0.30.13 unifies with ours — one wgpu in the binary, verified by
   `cargo tree -i wgpu`.
4. **Zero `.wgsl` edits; zero `SceneLike` trait changes; zero `engine-local/` / `assets/atlas/` /
   fixture contact** (AGENTS.md in full). The UI reads scene state and issues existing verbs —
   that's the whole coupling.
5. **UI state stays out of the determinism chain.** `LOD_MIN_PX`/`BACKDROP_GAIN` become live *in
   windowed runs only*; the offscreen path keeps the compile-time defaults (see K4 for the
   mechanism). Slider state never reaches staging, buffers, or the engine.
6. House cadence: one logical change per commit, `bash tools/check-all.sh` green after each with
   results in the commit message, report at `out/STAGE_K_REPORT.md`, zero build+clippy warnings,
   fail-loud `expect` style, no fmt mass-reformat.

## STEP 0 — baseline (before ANY edit)

```bash
rustc --version                 # expect ≥ 1.95 (egui 0.36 MSRV) — if lower, STOP and report
cd /Users/lugo/localdev/viz-native/glyph3d-native
bash tools/check-all.sh         # expect: CHECK-ALL: ALL GATES GREEN
# windowed FPS band (K adds ~1–2 ms CPU/frame; record the pre-K band for the report):
# run windowed on the fixture repo ≥ 30 s, note the 1 Hz FPS lines (scene + mode of your choice)
```

**Re-verify the version stars on execution day** (the study is dated 2026-09-01): check
crates.io that egui's current line is still 0.36.x on `wgpu ^30`. If 0.37+ has shipped against
wgpu 31, STOP — the whole K plan's version math (and note 04's verdict) needs a re-run before
any dep is added. The date gap between a handoff and its execution is where version surveys go
stale; this check is the tripwire.

Record the **interaction inventory** you will re-verify after every phase (from the Stage F/G
reports and `windowed.rs`): WASD/ERQF fly, scroll = speed, right-drag = look-grab (Confined →
Locked fallback), backquote = toggle grab, Esc = ungrab, left-click = pick + flash, verb keys
(exact set per `GlyphScene::on_key` — h/g/t/x family), `saw_device_delta` cursor fallback.

---

## Phase K1 — deps + plumbing, empty UI (the riskiest commit, made boring)

**Do:**
- Add the three deps behind `feature = "egui-ui"` (default). `--no-ui` runtime flag on the clap
  CLI (house parity-test pattern from Stage H phase 3 applies).
- In `App::resumed`, after `surface.configure`: `egui::Context::default()`,
  `egui_winit::State::new(ctx, ViewportId::ROOT, &window, Some(scale_factor), None,
  Some(device.limits().max_texture_dimension_2d))`, `egui_wgpu::Renderer::new(&device,
  **actual surface format** — never hardcode, see the sRGB note below)`.
- In `App::window_event`, feed egui FIRST: `let resp = egui_state.on_window_event(&window,
  &event);` then gate every scene routing branch on `!resp.consumed`. (Semantics cheat-sheet,
  from the study §2.2: pointer/wheel consumed = `egui_wants_pointer_input`; CursorMoved consumed =
  `egui_is_using_pointer`; keys consumed = `egui_wants_keyboard_input` or Tab, which is always
  consumed.)
- In `WindowState::render`, after the scene pass, same encoder: the 0.36 lifecycle —
  `take_egui_input` → `ctx.run_ui(raw_input, |ui| { /* nothing yet — CentralPanel::default()
  with no content */ })` → `handle_platform_output` → `tessellate` → `update_texture` per
  `textures_delta.set` → `update_buffers` (**mandatory before render — it panics otherwise**) →
  egui render pass (`LoadOp::Load`, no depth attachment, `render(&mut pass.forget_lifetime(),
  ...)`, drop the pass before touching the encoder again) → submit as today →
  `free_texture` per `textures_delta.free` **after** `queue.submit` → present.
- Wrap the egui pass in its own wgpu-profiler query block mirroring the scene scheme
  (`"egui pass"`), so `GLYPH_PROFILE=1` shows both.
- Repaint policy: **change nothing** — our continuous redraw in `about_to_wait` is exactly what
  egui wants; ignore eframe's `ControlFlow::Wait` optimization for now.

**Decisions to record in the report:** (a) *sRGB surface*: we keep the sRGB surface preference;
  egui logs a once-per-pipeline warning and uses its linear-framebuffer shader path — accepted
  (switching the surface to gamma-space would perturb the glyph pass's output encoding for zero
  UI gain). (b) *skipped frames*: if a code path ever skips painting, call
  `full_output.drop_without_applying_deltas()` — `TexturesDelta` panics if dropped unapplied.

**Accept:** `check-all.sh` green (offscreen untouched → PNGs trivially byte-equal; say so);
`cargo tree -i wgpu` shows one wgpu; windowed smoke: empty UI invisible, full interaction
inventory still works, FPS band within ~2 ms of STEP 0; `--no-ui` run identical to pre-K
behavior.

## Phase K2 — input-consumption correctness (the actual feature)

**Do:** tighten the gating matrix and the grab interplay:
- Rule from the prep notes: **opening/focusing UI ungrabs** the pointer (right-drag look and egui
  cannot coexist; while grabbed, raw `DeviceEvent::MouseMotion` drives look and bypasses egui —
  fine). Wire `ungrab` into UI-focus paths; verify grab re-acquires after the pointer leaves egui.
- Verify the trap the prep notes call out: today every bare key falls through to fly-camera and
  the verb keys — typing in an egui text field must not fly the camera or fire verbs. Test with
  an actual text field (add a scratch one temporarily or fold into K3's inspector).
- IME comes free via `egui_winit` (`Ime` events → preedit rendering) — smoke-test a dead-key/CJK
  input if you have one; candidate-window positioning quirks are a known rough edge, document
  rather than fix.

**Accept:** manual interaction matrix (STEP 0 inventory + text-field isolation + grab rules)
documented in the report — this is the house precedent for windowed-only behavior (cf. STAGE_H
gap 4); `check-all.sh` green.

## Phase K3 — the debug panel (first real widgets)

**Do:** one `egui::Window` ("Debug") with: FPS readout (the same 1 Hz figure `windowed.rs`
prints — keep the println for offscreen), camera readout (mode/eye/yaw/pitch), the **pick
inspector** (last pick's log line: file, row/col, byte, char, slot — the exact string
`apply_pick` returns), and **verb buttons** that call `apply_verb` with the same strings the CLI
`--verb` path parses. Unique window `.id()`s if more windows appear.

**Accept:** a click-pick shows in the panel; each verb button produces the same log line and
visual effect as its CLI counterpart on the same fixture; `check-all.sh` green.

## Phase K4 — live cull/LOD tuning (the one that touches scene state)

**Do:**
- `LOD_MIN_PX` (CPU cull classification, `glyph_scene.rs`) → a `Cell<f32>` in `CullState` seeded
  from today's const (the `viewport: Cell` precedent for `render(&self)` immutability); slider
  writes it. Offscreen never writes it → consts preserved.
- `BACKDROP_GAIN` (shader-side via the Params uniform) → per-change `queue.write_buffer` on the
  params buffer when the slider moves (dirty-flag; not per frame). Offscreen never writes.
- Optional readouts: the `GLYPH_CULL_DEBUG` counters (visible ranges / instances / backdrops) as
  live labels — reading the values cull already computes.

**Accept:** `check-all.sh` green — the four PNGs prove offscreen kept the defaults; windowed:
dragging `LOD_MIN_PX` visibly trades glyphs for backdrop quads, `BACKDROP_GAIN` dims/brightens
backdrops; interaction matrix subset still green.

## Phase K5 — (stretch) group-tree browser

**Do:** a scrollable tree over `PickContext.files` (~1.3 k files — the egui large-list trap:
`ScrollArea::show_rows` or equivalent virtualization, NOT a naive loop; study §4 performance
notes). Rows show rel_path + hidden/tint state; row click = focus/`set_cam_pose`-style navigation
if cheap, else just selection highlight. Only reach for `egui_ltreeview 0.9` (note 04's verified
pick) if vanilla egui trees actually hurt — and then justify the dep in the report.

**Accept:** windowed smoke at repo scale (60 fps band held, scroll stays responsive);
`check-all.sh` green.

---

## Sequencing notes (K ↔ L)

- Stage L (`08-view-structure-handoff.md`) follows K; L1/L2 are K-independent and may be done
  first if K slips, but keep stages separate — one variable per gate.
- K's overlay is the deliberate divergence from re_renderer's composite round-trip (07's Stage K
  section records why: we own the surface; eframe's ownership is what forces rerun's shape).
- The *framed* pattern (canvas as an egui-hosted texture) is post-L territory: L3's pooled target
  is the prerequisite, and `register_native_texture` demands `Rgba8Unorm` (non-sRGB) — the
  wrinkle is already recorded in 08's L3. Don't prototype it inside K.

## Final checklist for `out/STAGE_K_REPORT.md`

Goal/Result header; per-phase detail (API names as actually resolved — e.g. confirm `run_ui` vs
any 0.36.x renames against the vendored source, it's the known gotcha); verification table (six
gates per commit + the manual matrices + FPS bands + `cargo tree -i wgpu` / `-d` / lock diff);
file diffs; remaining gaps (house convention — likely entries: puffin_egui still pinned to egui
0.33; AccessKit feature left on/off and why; candidate-window quirks; glyphon labels (roadmap
item 6) deferred).
