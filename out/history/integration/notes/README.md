> **History.** Moved to `out/history/integration/` on 2026-10-10: 2026-09 research
> and stage handoffs, all executed or superseded. What is true now: `README.md`,
> root `AGENTS.md`.

# glyph3d-integration-notes

*Sibling notes to `glyph3d-native/` (kept OUT of that folder on purpose — agents work
there). Research date: 2026-09-01. All crate versions/dates verified against
crates.io/docs.rs/GitHub on that date by four parallel research passes; the codebase
inventory is from a full read of `native/src` + stage reports + `engine-local/README.md`.*

## TL;DR

The question was: can the hand-rolled primitives in `glyph3d-native` be subsumed by or
wrapped around existing Rust rendering crates for free node-entity relationships,
camera controls, graphics helpers, and UI?

**Answer: the renderer stays yours — the periphery becomes free.** No 2026 framework
hosts a bespoke GPU-driven pipeline without tax (the prior research doc's rejection
survives a much deeper dig; the shell tier is thinning, the code-viz niche is empty).
But the wrap-around tier is unusually rich *right now* because your stack (wgpu 30 /
winit 0.30) is the current major and the ecosystem has caught up to it:

| want | get | note |
|---|---|---|
| **UI (the big one)** | **egui 0.36.1 + egui-wgpu + egui-winit** — verified `wgpu ^30.0` + `winit ^0.30.13` | overlay pass into your existing encoder; panels/docking/tree/IME/AccessKit; closes Stage G gaps #1/#4/#6 |
| world-space/HUD labels | **glyphon 0.12** (`wgpu ^30`) + **cosmic-text 0.19** (shaping, emoji, fallback) | screen-space projection now; cosmic-text as front-end into your atlas later |
| GPU/tooling | **wgpu-profiler 0.28** (`wgpu ^30`), **naga 30.0.1** (WGSL validation, auto-paired), **clap 4.6** | near-zero-risk adoptions |
| node-entity relationships | **hecs 0.11** (minimal) or **bevy_ecs 0.19 standalone** (verified headless, has ChildOf/Children relationships + observers); **petgraph 0.8.3** for the code graph | when scene complexity demands it; glyphs stay GPU data |
| camera controls | nothing maintained at glam 0.30 — your FlyCamera IS the ecosystem norm; **trackball/tween** unlock after a glam 0.33 bump | dolly/smooth-bevy-cameras/Bevy-0.18-FreeCamera are pattern sources |
| architecture blueprint | **Rerun 0.37 / re_renderer** — published, `wgpu ^30.0`, weekly cadence | don't depend; source-read and staged — see `07-rerun-architecture-blueprint.md` |
| "become part of" | **Bevy-as-shell is now practical** (bevy_winit verified render-free; RawHandleWrapper; official no_renderer/headless examples; FSLabs 3+ yrs production) but pins wgpu 29 in bevy_render and rides Bevy's churn | 1-day spike only on demand |

**Key ecosystem fact:** two wgpu majors can't share a Device in one binary — every
"LAGS/wrong version" verdict in these notes reduces to that. You're on the current
major; much of the ecosystem is one behind.

**Watchlist:** puffin_egui→egui 0.36, iced→wgpu 30, masonry/xilem post-alpha, Glifo
(Linebender glyph layer), renderling post-rust-gpu, wgpu Metal multi-draw-indirect fix
(#2148) → revisit GPU cull + indirect draws.

## The files

| file | contents |
|---|---|
| `00-primitives-inventory.md` | ground truth: every hand-rolled primitive, module by module, + the seams + the hard constraints (wgpu pin, bit-exact determinism chain, Mojo engine stays) |
| `01-ecs-node-entities.md` | ECS/scene-graph survey; what the GroupRow table becomes; renderling/three-d/nannou/kiss3d/Rerun verdicts; force-layout status |
| `02-camera-and-controls.md` | camera crate landscape; why your fly cam isn't tech debt; patterns worth porting; picking needs nothing |
| `03-graphics-helpers.md` | the adopt-now shopping list (wgpu-profiler, naga, clap, puffin, glam flags) + conditional (encase, etagere) + rejected-with-reasons — **tier-1 landed in Stage H**, corrections from execution folded in |
| `04-ui-layer.md` | egui verdict + integration shape; every other toolkit's blocker; glyphon/cosmic-text for labels; IME status |
| `05-integration-postures-and-roadmap.md` | the synthesis: four postures ranked, stage-mapped roadmap, watchlist |
| `06-tooling-pass-handoff.md` | **EXECUTED — Stage H landed clean** (spec record; commits + corrections in the status banner; evidence in `glyph3d-native/out/STAGE_H_REPORT.md`) |
| `07-rerun-architecture-blueprint.md` | the Rerun/re_renderer source dive (local clone, `cb5e9d6`): verified frame flow, their-concept ↔ our-code mapping, staged borrow plan — opportunistic steals, the Stage K decision, Stage L "view-structure", watchlist triggers, refusals + reference index (post-cleanup update banner re-letters its stages) |
| `08-view-structure-handoff.md` | **executable handoff for the implementing agent**: Stage L (view-structure — FrameUniform, phase lists, pooled target + composite) after the egui Stage K, written against the post-cleanup tree (`cb35552`): hard fences, STEP 0 baselines, per-phase acceptance bars, watchlist triggers. PLANNED, not executed |
| `09-egui-ui-handoff.md` | **executable handoff for the implementing agent**: Stage K (the UI stage — egui 0.36.1 overlay: deps/plumbing, input-consumption gating, debug panel, live cull/LOD sliders, group-tree browser) with the offscreen-oracle fence (`egui-ui` feature + `--no-ui`), per-phase acceptance bars. PLANNED, not executed |
| `10-rerun-borrow-decision-record.md` | the ADR for the handover: the decision stated precisely (scope table — only Stage L + O1/O2 are rerun-derived), the affirmative case (demand evidence, convergence, provenance, risk asymmetry), counterarguments steel-manned and answered, falsifiers/tripwires, options considered |

## Suggested reading order

05 (the answer) → 00 (why the constraints) → 04 (the big win) → 03 (the free wins) →
01 → 02 → 10 (the decision) → 07 (the architecture deep-dive) → 09 → 08 (the handoffs, when execution starts).
