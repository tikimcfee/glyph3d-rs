# 07 — Rerun / re_renderer deep-dive: the borrow plan

*Source read 2026-09-01 from a local shallow clone at `<rerun-clone>`
@ `cb5e9d6` (2026-09-01). Facts below cite `file:line` from that snapshot; sibling facts cite
`glyph3d-native` at HEAD (`4cb3c81`, Stage H landed). This doc executes posture 2 of
`05-integration-postures-and-roadmap.md` ("Rerun-style architecture borrowing"): what exactly to
borrow, what to refuse, and how it stages. The defensible case for the whole decision —
scope, counterarguments, falsifiers — is `10-rerun-borrow-decision-record.md`.*

*Post-cleanup update (same day, after Stages I & J landed at `cb35552`): the repo's Stage I turned
out to be the glam 0.33 bump + immutable-fixture baselines, and Stage J a hygiene sweep (clippy
zero, `AGENTS.md`, `tools/check-all.sh`) — so the "Stage I (UI)" / "Stage J candidate" headings
below are re-lettered here to **K (egui UI)** and **L (view-structure)**, and the executable
handoff now lives in `08-view-structure-handoff.md`. glyph3d-native line refs re-verified at
`cb35552`.*

## TL;DR

The claim from note 05 held up under a full source read: **re_renderer is our renderer one stage
ahead, organized — borrow its frame *structure*, not its *machinery*, and not the crate.** The
structure worth money: per-view render-to-pooled-target + a `composite()` step (their ViewBuilder),
a phase-partitioned draw list (their DrawPhase), and a widened frame uniform with a determinism
flag. The machinery to refuse: type-erased DrawData dispatch, descriptor-keyed resource pools,
staging belts, WebGL "data textures", device tiers — each solves a problem we don't have (12+
primitive types, fully-dynamic per-frame data, browser targets). Everything lands behind the
standing PNG-byte gates; the plan stages it as opportunistic steals + a "Stage L view-structure"
stage after the egui UI stage (K).

## Version alignment (why borrowing is cheap right now)

| | rerun @ cb5e9d6 | glyph3d-native | note |
|---|---|---|---|
| wgpu | **30.0** (workspace `Cargo.toml:492`) | 30.0.1 | same major — code-level borrowing is friction-free |
| egui | 0.36.1 (+ egui-wgpu 0.36.1) | none yet (UI stage = K; integration study in `glyph3d-native/integration/`) | exact match with note 04's pick |
| winit | 0.30.13 (examples only) | 0.30 | same line |
| edition / MSRV | 2024 / 1.96 | 2021 / wgpu-30 MSRV | only matters for copy-paste wholesale; the *patterns* port to 2021 |

re_renderer itself has **zero egui dependency** (only `ecolor` for color types); the egui bridge
lives in `re_viewer_context`. That separation is itself the pattern: renderer stays UI-agnostic.

## What re_renderer actually is (verified)

**Frame flow, one paragraph.** A long-lived `RenderContext` (never owns a surface) holds the
device/queue, lazily-created immutable `Renderer`s (pipeline holders), pooled wgpu resources, two
staging belts, and a frame-global copy encoder. Per frame: `begin_frame()` recycles pools and
reloads hot shaders → user code builds per-primitive `DrawData` through builder types that stream
vertices straight into write-combined belt memory → one `ViewBuilder` per view/camera allocates
pooled MSAA+depth targets and a per-view uniform bind group → `queue_draw()` type-erases the
DrawData into per-`DrawPhase` work lists → `ViewBuilder::draw()` sorts and records passes into a
CommandBuffer → `ViewBuilder::composite()` draws the result into a **host-owned** render pass
(egui's callback pass in the viewer; the surface pass in standalone examples) → the host does the
single `queue.submit` → `before_submit()` unmaps staging and flushes the frame-global encoder.

The load-bearing pieces, with sources:

- **`RenderContext`** (`src/context.rs:94`): 8 pooled resource collections, `CpuWriteGpuReadBelt`
  (32 MiB chunks), `GpuReadbackBelt`, in-flight submissions capped at 4 with `poll_device`
  (`context.rs:285`), and a "before view builder" frame-global encoder for copies issued during
  DrawData creation — finished and submitted in `before_submit()`.
- **`ViewBuilder`** (`src/view_builder.rs:433`): pooled `Rgba8UnormSrgb` + `Depth32Float` targets
  (**reverse-z**: clear 0.0, `GreaterEqual`, "so objects drawn later with the exact same depth
  value can overwrite earlier ones" — `view_builder.rs:406`), per-view `FrameUniformBuffer` bind
  group 0 (includes `device_tier` *and a `deterministic_rendering` flag* for snapshot tests,
  `view_builder.rs:585`), optional render processors (picking rect, outline mask via jump-flood,
  screenshot), and — the key trick — **it pre-queues its own `CompositorDrawData`**, so
  compositing is just another draw in a `Compositing` phase, not a special code path.
  `composite(ctx, pass)` (`view_builder.rs:940`) draws into whatever pass the host provides.
  `ViewBuilderId` (`view_builder.rs:35`, "Stable identity of a rendered view… so per-view renderer
  caches remain associated with the correct camera") keys cross-frame caches across frames.
- **`DrawPhase`** (`src/draw_phases/mod.rs:29`): `Opaque / Background / Transparent / PickingLayer
  / OutlineMask / OutlineMaskNoDepth / Compositing / CompositingScreenshot`. The
  `DrawPhaseManager` keeps a fixed array of per-phase drawable vectors (no HashMap), sorts opaque
  bundles by renderer-type + draw-data index with near-first early-z, transparent far-to-near.
  Their own TODO admits even this is "a hardcoded/limited-flexibility render-graph"
  (`view_builder.rs:29`) — i.e., they rejected a general render graph too.
- **`DrawData` vs `Renderer`** (`src/renderer/mod.rs:123`): "GPU sided data used by a `Renderer`
  … Expected to be recreated every frame" vs "an immutable, long-lived datastructure that only
  holds onto resources that will be needed for each of its `Renderer::draw` invocations". The
  type-erased queueing (`QueueableDrawData` boxes + downcast at dispatch) exists because they have
  ~12 renderer types; we have 2 pipelines.
- **Pools** (`src/wgpu_resources/`): `StaticResourcePool` dedupes by descriptor hash and can swap
  the inner wgpu resource *in place under the same handle* (the shader-hot-reload path);
  `DynamicResourcePool` uses slotmap generational handles and GCs by `Arc::strong_count == 1` at
  `begin_frame`, parking freed resources for one frame of reuse. Bind-group descriptors hold weak
  handles so cached bind groups never keep buffers alive (`bind_group_pool.rs:79`).
- **Transparent sorting** (`src/transparent_sort.rs:26`): CPU back-to-front per frame, previous
  frame's order as a near-sorted start keyed by `ViewBuilderId`, result uploaded as an R32Uint
  lookup texture.
- **egui bridge** (`re_viewer_context/src/gpu_bridge/re_renderer_callback.rs:24`): implements
  `egui_wgpu::CallbackTrait` — `prepare()` calls `view_builder.draw()` and returns the command
  buffers for egui_wgpu to submit; `paint()` sets an unclamped viewport and calls
  `view_builder.composite()` into egui's own render pass. egui owns the surface/device; the
  `RenderContext` is stashed in `egui_wgpu`'s `callback_resources`.
- **Standalone loop** (`re_renderer_examples/src/framework.rs`, bins `multiview`, `picking`,
  `outlines`, …): `get_current_texture → begin_frame → build ViewBuilder(s) →
  view_builder.draw() → composite into surface pass → before_submit → queue.submit → present`.
  The `multiview` example is the reference for "several cameras, one frame, one submit".

## Side-by-side: their concept ↔ our code

| re_renderer | glyph3d-native today | verdict |
|---|---|---|
| `RenderContext` pools + belts + frame-global encoder | `GpuContext` = device/queue/profiler only (`gpu.rs:17`) | **keep ours** — pools pay off at their churn rate; our buffers are staged once and partial-updated |
| `ViewBuilder` → pooled target → `composite()` | scene renders straight into the driver-provided view (`FrameTarget`, `scene.rs:15`; `GlyphScene::render` `glyph_scene.rs:2151`) | **borrow in Stage L** (see plan + note 08) — the structural win |
| `DrawPhase` lists | implicit: backdrop stream then glyph stream inside one pass (`glyph_scene.rs:2245`, `:2261`) — already phase-shaped, unformalized | **borrow the enum** when a third stream appears (selection/labels/UI) |
| `DrawData`/`Renderer` split + type-erased queueing | `render()` records directly; 2 pipelines | **refuse** — type-erasure machinery for 12 renderers is overhead for 2 |
| CPU transparent sort + `SortOrderCache` | ascending-arena-order blend, documented as deliberate (`glyph_scene.rs:2259`) | **refuse** — our order *is* the determinism spec |
| `FrameUniformBuffer` (eye, proj, tier, **deterministic flag**) | `CameraUniform` = 64 B `view_proj` (`glyph_scene.rs:294`, written at `:2167`) | **widen** (L1) — the `RenderMode::Deterministic` idea is oracle-facing gold |
| reverse-z (`GreaterEqual`, clear 0) | depth clear 1.0, `Less` | **refuse** — flipping re-baselines every PNG for zero gain |
| 4-in-flight cap + `poll_device` | Fifo, latency 2 | keep ours |
| `ErrorTracker` dedup on `on_uncaptured_error` (`context.rs:310`) | nothing | **steal now** (O1) |
| shader file server + hot reload (debug) | WGSL edits = rebuild | optional DX steal (O3) |
| `DeviceCapabilityTier` in the frame uniform | none; Metal-first | **refuse for now** — the web sibling is glyph3d-js; *revisit if the wasm port ever proceeds* (`glyph3d-native/research/wasm-port-audit.md` found the render path engine-free) |
| "assume everything changes every frame" | static arena + 4–80 B partial `write_buffer`s per edit | **refuse the stance, keep the validation** — their fully-dynamic data (logged point clouds) *forces* belts+interning; our static-instance model is cheaper and our cull-lists-rebuilt-per-frame is already the same philosophy where it counts |

## What the dive validates (no action, just confidence)

1. **No render graph, anywhere.** re_renderer partitions a flat draw list by phase; their own TODO
   calls it a hardcoded graph (`view_builder.rs:29`); Bevy deleted RenderGraph outright (note 01);
   we run one pass with two sub-streams. Three independent architectures converged on the same
   answer — the glyph_scene shape is not tech debt.
2. **Immediate-mode per-frame rebuild is compatible with determinism.** They re-collect drawables
   every frame and still ship a `deterministic_rendering` flag for snapshot tests — the same
   discipline as our virtual-clock offscreen oracle, expressed as a frame-uniform bit instead of a
   separate driver.
3. **Composite-as-a-draw** (pre-queued `CompositorDrawData`) means "render to texture, then blit"
   costs them no special case — worth copying the *idea* so our oracle path and windowed path can
   share one code shape.

## The plan

Stage discipline as usual: independent, reversible, gated. Since the cleanup the gates are one
command — **`bash tools/check-all.sh`** (build/clippy/test zero-warning, `--engine-check`,
`check-stage-g.sh`, four-view byte-equal A/B) — with FPS claims on ≥ 30-frame runs. The old
text.png input-sensitivity caveat is retired (Stage I's immutable
`native/fixtures/baseline-view.txt`); the live caveat is that the *fixture is the baseline* —
editing it is a re-baseline act.

### Opportunistic (any commit, no stage letter)

- **O1 — error dedup:** port the `ErrorTracker` pattern onto `device.on_uncaptured_error` so a
  repeated wgpu validation error prints once, not per frame. ~1 h, outside all gates' scope.
- **O2 — debug labels:** label pipelines and buffers, not just the pass ("glyph field pass"
  already exists). Pure DX; zero output change.
- **O3 — (optional) WGSL hot reload, debug-only:** their `file_resolver`/`file_server`/
  `workspace_shaders` trio reloads shaders at runtime in debug builds. Only worth it if shader
  tuning becomes a time sink again (it was the Stage C loop). Must be feature-gated and must not
  change release bytes — gate: byte-compare with the feature off.

### Stage K (UI — next stage, per `out/history/STAGE_I_REPORT.md` remaining-gaps #1)

Choose **surface-direct egui** over the rerun bridge shape: our pass, then the egui pass, same
encoder, into the surface view (the seam STAGE_H reserved at `windowed.rs:125–140`). Rerun needs
the ViewBuilder→texture→`composite()` round trip because eframe owns the surface; we own ours.
This decision is now doubly spec'd: `glyph3d-native/integration/egui-integration-report.md` (the
0.36.1 source study — overlay pass with `LoadOp::Load`, `EventResponse.consumed` input gating,
`egui_wgpu::Renderer` without the `winit` feature) and `research/egui-integration-notes.md`;
the executable handoff is `09-egui-ui-handoff.md`.
Record it in the Stage K report as a deliberate divergence from re_renderer, reversible by L3.
Stage I also unlocked the camera-crate tier (trackball/tween/etc.) if K wants it.

### Stage L candidate — "the view-structure stage" (~1–2 weeks, after K)

*(Re-lettered post-cleanup: I and J were consumed by the glam bump and the hygiene sweep. The
executable version of this section is `08-view-structure-handoff.md` — phases L1–L4 there
correspond to J1–J4 below; follow 08 where the two differ, it was written against the
post-cleanup tree.)*

The actual re_renderer skeleton borrow, in dependency order, each step gated before the next:

- **J1 — FrameUniform widening.** `CameraUniform` → `FrameUniform { view_proj, eye, viewport,
  px_scale, time, flags }` (flags reserves a `deterministic` bit à la `RenderMode::Deterministic`).
  WGSL reads what it needs; `pixel_ray` keeps using the analytic f64 path (never the uniform).
  Gate: byte-identical PNGs (camera math untouched).
- **J2 — Phase enum + per-phase draw lists inside GlyphScene.** Formalize the existing
  backdrop→glyph ordering as `Phase::{Backdrop, Glyphs}` + reserved `Selection`/`Overlay`; draw
  recording order unchanged. Gate: byte-identical PNGs + `GLYPH_CULL_DEBUG` counters unchanged.
- **J3 — pooled view target + composite.** The real ViewBuilder borrow: the glyph pass renders
  into a scene-owned pooled `Rgba8UnormSrgb`+`Depth32Float` target; a fullscreen-quad composite
  pass (a *draw*, not a special case — their CompositorDrawData idea) blits into whatever view the
  driver hands over: surface, offscreen, or (later) an egui callback. Unlocks, in order of value:
  1. **second view** — a repo-wide overview/minimap while zoomed (the exact e2-field-wide vs
     e2-file-zoom debugging pattern in `out/`, as a live feature);
     2. **in-window screenshots** sharing the offscreen PNG path;
     3. egui-in-panel views (after K, if ever wanted).
  Implementation floor: ping-pong two textures resized on `Resized` — **not** a general
  `DynamicResourcePool`; their pool exists upstream as reference if needs ever grow.
  Gates: composite proven pixel-neutral vs the direct path on the 4 baselines (or explicitly
  re-baselined); offscreen keeps a `--no-composite` escape hatch for A/B; `repo-verify` untouched.
- **J4 — (stretch) selection mask pass.** Their `OutlineMaskProcessor` (mask pass + jump-flood
  outline) is the principled shape for Stage G's open gaps "sticky flash" and "selection ranges".
  Start simpler: a mask-texture tint pass in the reserved `Selection` phase. Only if J1–J3 land
  green; reference reads: `re_renderer_examples` bins `outlines` and `picking`.

### Watchlist-triggered (do not start now)

- **wgpu Metal indirect-draw fix lands** → revisit GPU cull (Stage F's documented reason for CPU
  cull); if it happens, borrow the `PickingLayerProcessor` shape for a GPU pick layer as a
  *complement* — the CPU pick + Python oracle stays the contract (note 00 constraint).
- **pipeline count grows past ~8** (composite + mask + label/LOD variants would get us to 5–6) →
  then a descriptor-keyed `StaticResourcePool` pays; not before.
- **per-frame uploads grow KB → MB** (e.g. animated layouts rewriting the backdrop stream) → then
  a `CpuWriteGpuReadBelt`; `queue.write_buffer`'s internal staging is fine at 42 KB.

### Never (constraints restated from note 00)

Determinism chain untouched (engine FFI, `fold_leaders` oracle, PNG bytes); depth convention stays
`Less`/1.0; blend order stays ascending arena order; the pick pipeline stays CPU + oracle.

## Reference index (for the next agent)

Clone: `<rerun-clone>` @ `cb5e9d6`. Read in this order:

| file | what it is |
|---|---|
| `crates/viewer/re_renderer/src/lib.rs` | crate docs: ViewBuilder as "the main entry point", DrawData/Renderer split |
| `…/src/context.rs` | `RenderContext` (94), belts, in-flight cap (285), `begin_frame`/`before_submit`, `ErrorTracker` |
| `…/src/view_builder.rs` | target alloc + reverse-z (406), processors, `queue_draw` (714), `draw` (729), `composite` (940) |
| `…/src/draw_phases/mod.rs` + `draw_phase_manager.rs` | the phase enum (29) and the sort/dispatch machinery |
| `…/src/renderer/mod.rs` | `DrawData` (123) / `Renderer` (158) traits, `DrawDataDrawable` sort keys |
| `…/src/wgpu_resources/` | `mod.rs` (pool split rationale), static + dynamic pool internals, bind-group weak-handle trick |
| `…/src/transparent_sort.rs`, `…/src/device_caps.rs`, `…/src/allocator/` | cross-frame sort cache; tier model; belts/data-textures |
| `crates/viewer/re_renderer_examples/src/framework.rs` (+ bins `multiview`, `picking`, `outlines`) | the minimal standalone loop; the J3/J4 references |
| `crates/viewer/re_viewer_context/src/gpu_bridge/re_renderer_callback.rs` | the egui `CallbackTrait` bridge (24) — the shape Stage K deliberately diverges from |
| `glyph3d-native/integration/egui-integration-report.md` + `research/egui-integration-notes.md` | the egui 0.36.1 study backing the Stage K decision (surface-direct overlay, specced end-to-end with line cites) |
| `glyph3d-native/research/wasm-port-audit.md` | wasm feasibility audit — render path is engine-free, Text scene works without Mojo; the trigger for revisiting the tier refusal |
| root `ARCHITECTURE.md` | ecosystem map (SDK/Arrow store/viewer crates) — context only; re_renderer's own source is the blueprint |

*Sources: direct source read of the clone above; `glyph3d-native` at `4cb3c81` (module headers,
`out/STAGE_F|G|H_REPORT.md`, `out/history/PICK_FIX_REPORT.md`, `tools/check-stage-g.sh`); sibling notes
00–06. All file:line citations spot-verified on 2026-09-01.*
