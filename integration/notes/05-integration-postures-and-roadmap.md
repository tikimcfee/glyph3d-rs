# 05 — Integration postures & roadmap (the synthesis)

*Synthesized 2026-09-01 from four deep research passes (ECS, UI, camera/helpers,
engine-integration) + a full read of the codebase. Facts are in notes 01–04; this
file is the judgment.*

## The one-paragraph answer

Nothing in the 2026 Rust ecosystem should host your renderer — that conclusion from
`glyph3d-native/research/native-rendering-stack-comparison.md` **survives contact with a
much deeper dig** (the shell tier is literally thinning: notan is seeking maintainers,
pixels is being abandoned by emulators, renderling is stalled on an archived
rust-gpu, vello is the wrong computational model for 95M instanced primitives, and the
entire Rust "code city" niche is empty — 9 repos, none 3D/GPU). But the question has
changed since that doc: the **shell layer's tax has dropped to near zero**, and the
cherry-pick tier has precise, version-verified picks. The posture that wins:
**stay raw wgpu 30 and adopt crates around the renderer, not under it** — egui 0.36
(UI), wgpu-profiler + naga + clap (tooling), hecs-or-bevy_ecs + petgraph (data model,
when scene complexity demands it), cosmic-text/glyphon (labels + emoji/IME gaps),
borrowing Rerun's renderer architecture as the blueprint.

## The four postures, ranked

### 1. Stay raw wgpu 30 + cherry-pick crates — KEEP (with a shopping list)
Lowest risk, immediate leverage. Verified exact-stack matches as of today:
`egui 0.36.1`/`egui-wgpu`/`egui-winit`, `glyphon 0.12`, `wgpu-profiler 0.28`,
`naga 30.0.1`, `clap 4.6`, `etagere` (conditional), glam feature flags. See notes
03–04. This is "wrapping around" the ecosystem in the literal sense: the renderer
stays yours, the periphery becomes free.

### 2. Rerun-style architecture borrowing — the strongest new discovery
**Rerun 0.37.0 (shipped 2026-09-01) runs `re_renderer` on `wgpu ^30.0` — the only
framework-adjacent renderer in the ecosystem on your exact major.** Published on
crates.io (1.24M dl), described as usable standalone with its own examples, with the
architecture you're converging toward, one stage ahead: `ViewBuilder` entry point,
per-primitive `DrawData` structs, `DrawPhase` work queues, aggressive resource pooling,
fully-dynamic per-frame data, headless mode, egui chrome, custom columnar entity store
(ECS-shaped data *without* an ECS framework). You almost certainly should NOT
depend on it (your pipeline is more specialized than its, and its text is egui's) —
but `ARCHITECTURE.md` and its source are the best free blueprint for the problems you
hit next (draw-data versioning, resource caching, multi-view). Version alignment means
even code-level borrowing is friction-free. **Executed 2026-09-01:** the source dive lives in
`07-rerun-architecture-blueprint.md`, the executable handoff in `08-view-structure-handoff.md`
(verdict: borrow the frame structure — pooled view target + composite, phase lists, frame
uniform; refuse the pools/type-erasure/belts machinery. Stage letters moved after this note was
written: the repo's I/J landed as the glam bump + hygiene sweep, so the UI stage is **K** and the
view-structure stage is **L**.)

### 3. Bevy-as-shell — practical now, documented, but only worth it on demand
Verified mechanics (Bevy 0.19.1, Jun 2026): **`bevy_winit` has zero bevy_render and
zero wgpu dependency** (read from its Cargo.toml — the shell genuinely doesn't drag
wgpu in); `bevy_window` exposes `RawHandleWrapper` so your wgpu-30 renderer can create
its own surface from a Bevy window entity; official v0.19 examples cover your exact
modes — `no_renderer.rs`, `headless_renderer.rs`, `externally_driven_headless_renderer.rs`
(your offscreen harness), `custom_loop.rs`, `without_winit.rs`. Production precedent:
Foresight Spatial Labs, 3+ years Bevy on massive instanced spatial data (they maintain
bevy_aabb_instancing — the closest published cousin to a glyph-instancing plugin).
Honest costs: bevy_render pins **wgpu 29.0.3** exactly — keep `bevy_render` out
(default-features = false, MinimalPlugins + WinitPlugin + AssetPlugin) or accept dead
wgpu-29 weight via `backends: None`; the fully-render-excised configuration has **no
official example and no 2025–26 writeup — flag as a 1-day spike**; you ride Bevy's
~4-month breaking-cadence release train forever; and you permanently give up Bevy's
renderer ecosystem (bevy_ui, its text/PBR) next to your pipeline. Verdict: the only
"plug into" target where you keep 100% of the renderer. Prototype **only if** you find
yourself wanting Bevy's asset/picking/windowing machinery more than you resent its
churn.

### 4. Hosts (renderling / vello / notan / pixels / mach) — closed
- **renderling**: crates.io release 2 yrs stale, rust-gpu (its shader toolchain)
  archived 2025-10-31, community "stalled renderer" thread. Revisit only if a
  post-rust-gpu release ships. Meanwhile: mine its **GPU-resident scene-graph slab**
  design (nested nodes, GPU frustum/occlusion cull, GPU picking) — it's your group
  table taken further, and relevant when the Metal indirect-draw bug clears.
- **vello** (0.10, Aug 2026; wgpu 29 per 0.9 notes): compute-based path rasterization
  is for resolution-scale 2D, not your instanced field; subsuming quad_field/backdrop
  would forfeit the architecture. **Glifo 0.3.0** (Linebender's glyph layer) is the
  piece to watch — experimental, backend-agnostic.
- **notan/pixels/minifb/mini-shells**: maintenance trouble + abstraction without
  ecosystem. **Mach**: 100% Zig, pre-alpha — rejection stands.

## Roadmap (mapped to the repo's stage discipline)

Each item is independent, reversible, and none touches the byte→record→instance
determinism chain un-gated.

**Stage H — "the tooling stage" — ✅ LANDED (Sep 2026, `out/STAGE_H_REPORT.md`).**
wgpu-profiler (pass-level GPU timings behind `GLYPH_PROFILE=1`; first real numbers:
repo-wide glyph pass 4.079 ms GPU vs cull 0.000 ms CPU — the "cull is free" claim is
now measured), naga validation (19 tests), clap 4.6.6 CLI with a 15-test parity suite,
glam flags, encase layout assertions (option b). Caveats carried forward: Metal lacks
in-pass timestamps; text.png baseline is input-sensitive to main.rs edits; puffin_egui
still blocked on its egui 0.33 pin.

**Stages I & J — ✅ LANDED (Sep 2026, `out/STAGE_I_REPORT.md` + `out/STAGE_J_REPORT.md`).**
I: glam 0.30 → 0.33.6 **byte-identical** + the text.png caveat retired (immutable
`fixtures/baseline-view.txt` — the caveat below is historical). J: hygiene sweep — clippy
14→0, dead-code/unwrap triage, `native/AGENTS.md` house rules, `tools/check-all.sh`
(six gates, one command). The UI stage therefore moves to the next free letter:

**Stage K candidate — "the UI stage" (the big win, ~1–2 weeks; specced):**
5. egui 0.36 overlay: pick inspector, verb buttons, cull/LOD sliders
   (`LOD_MIN_PX`/`BACKDROP_GAIN` become live), group-tree browser, FPS in-window.
   egui-winit consumes events first; skip scene routing when consumed.
   Specs: `glyph3d-native/integration/egui-integration-report.md` (source study) +
   `research/egui-integration-notes.md`; note 04 is the verdict record.
   Gate: `bash tools/check-all.sh` + PNG byte-compares with UI *disabled* in
   offscreen mode to prove the oracle path untouched.
6. glyphon overlay labels (projected file names / HUD) — optional in the same stage.
   Then Stage L — "the view-structure stage" — per notes 07/08.

**Opportunistic (between stages):**
7. ~~glam 0.30 → 0.33 bump~~ — **DONE (Stage I)**; the
   trackball/tween/bevy_math/transform-gizmo tier is now adoptable.
   *(encase, originally item 8 here, landed early as Stage H Phase 5 — option b,
   write paths intact.)*

**On demand (when scene complexity demands it):**
9. hecs 0.11 (minimal) or bevy_ecs 0.19 (relationships/observers/schedules) to
   formalize file groups/segments/selection as entities; petgraph underneath for the
   code graph; WGSL force-layout compute pass when layout becomes a solver problem.
   Trigger: when `CullState`/`PickContext`/`geom_overrides` invariants start needing
   cross-module docs to stay coherent (they already need module-header paragraphs).
10. Bevy-shell spike (1 day: does `bevy_winit + bevy_window + bevy_asset` without
    bevy_render compile cleanly?) — only if item 9 lands and asset/windowing needs grow.

**Watchlist (re-check quarterly):** puffin_egui → egui 0.36; iced → wgpu 30;
masonry/xilem post-alpha; Glifo post-experimental; renderling post-rust-gpu;
wgpu Metal multi-draw-indirect fix (#2148) → revisit GPU cull pass + indirect draws;
Bevy wgpu-29 → 30 bump (changes posture-3 math).

## Environment facts underpinning all of this (verified 2026-09-01)

wgpu 30.0.1 (2026-08-22) is current; quarterly breaking cadence. winit 0.30.13 is
still the line (Bevy's bevy_winit also pins it — version-unifiable). glam 0.33.6
current vs your 0.30.10. Two wgpu majors cannot share a Device in one binary (Cargo
will hold both; types are incompatible) — every "LAGS" verdict in notes 01–04
ultimately reduces to this fact. Your stack is the current one; the ecosystem is
largely one wgpu major behind you, which is the *good* problem to have.
