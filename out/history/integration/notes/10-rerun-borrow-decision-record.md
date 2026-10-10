> **History.** Moved to `out/history/integration/` on 2026-10-10: 2026-09 research
> and stage handoffs, all executed or superseded. What is true now: `README.md`,
> root `AGENTS.md`.

# 10 — Decision record: the Rerun/re_renderer borrow (the defensible case)

*Status: **RECOMMENDED by the research pass, awaiting the owner's go.** This is the ADR for the
handover: if you (implementer, reviewer, skeptic) read one doc before executing, read this — it
states the decision precisely, argues it, steel-mans the opposition, and names what would change
our mind. Written 2026-09-01 against `glyph3d-native` @ `cb35552` and the rerun clone @ `cb5e9d6`
(full-source read of the load-bearing files; every claim traces to `07-rerun-architecture-blueprint.md`'s
line-cited dive or the repo's own stage reports).*

## The decision, precisely

**Borrow three structural patterns from re_renderer — a widened frame uniform, phase-partitioned
draw lists, and per-view pooled targets with a composite step — as gated stages (08's L1–L4, plus
the O1/O2 micro-steals), and refuse the rest of the blueprint: the crate itself, its resource
pools, belts, type-erased dispatch, device tiers, and data textures.**

Scope honesty, because it matters for defensibility: of the whole forward program, **only Stage L
and O1/O2 are rerun-derived.** Stage K (egui UI, `09-egui-ui-handoff.md`) derives from note 04 and
the egui source study — its surface-direct overlay decision actually *diverges* from rerun's
bridge shape. The engine, the glyph pipeline, the pick oracle, and K all stand if the rerun thesis
collapses tomorrow. The blast radius of being wrong is one ~1–2 week, fully-gated, revertible
stage.

| in scope (rerun-derived) | out of scope (stands on its own) |
|---|---|
| L1 FrameUniform (rerun's `FrameUniformBuffer` pattern) | Stage K egui UI (note 04 + egui study) |
| L2 Phase lists (rerun's `DrawPhase`) | the Mojo engine, atlas, Slug shaders, pick pipeline |
| L3 pooled view target + composite (rerun's `ViewBuilder`) | any dep on `re_renderer` or any rerun crate |
| L4 selection mask (rerun's `OutlineMaskProcessor` shape) | the Arrow store, the viewer, everything else rerun |
| O1 error-dedup, O2 labels (code patterns) | Bevy-as-shell, device tiers, wasm posture |

## Why it's right (the affirmative case)

1. **It's demand-driven, not fashion-driven.** Each borrow answers a demand already in the repo's
   own history: L3's overview/minimap automates the wide+zoom render pairs the team generates by
   hand every debug session (`out/e2-field-wide.png` vs `out/e2-file-zoom.png`, `g-zoom-base` vs
   `g-mid-base`, and ~48 more PNGs in that pattern); in-window screenshots extend a workflow that
   currently shells out to CLI flags; L4 is the principled fix for Stage G's *named* open gaps
   (sticky flash, selection ranges); L1/L2 are formalizations of structure `glyph_scene.rs` already
   has (backdrop stream → glyph stream *is* a two-phase draw list; `CameraUniform` is a frame
   uniform that stopped growing). We did not go looking for rerun's ideas — we found our own shape
   in their codebase, one stage further along.
2. **Convergent evolution, three ways.** re_renderer partitions flat draw lists by phase and calls
   its own processors "a hardcoded render-graph" in a TODO (`view_builder.rs:29`); Bevy 0.19
   *deleted* its RenderGraph outright (note 01); our renderer was born one-pass-two-stream. Three
   independent architectures, same conclusion: phase lists, not graphs. When the ecosystem
   converges from both directions onto your incumbent design, borrowing the shared vocabulary is
   the low-variance move.
3. **Provenance is as good as it gets.** Production renderer shipped weekly, ~1.24M downloads, on
   **our exact wgpu major** (30.0 — verified in their workspace manifest), MIT/Apache-2 dual
   licensed, source read in full rather than trusted. Pattern-borrowing carries no dependency
   risk at all; the one code-level lift (O1's `ErrorTracker`) is small enough to attribute in a
   comment and owe them nothing but a thank-you.
4. **The risk asymmetry is extreme.** Cost: zero new dependencies (Stage L is internal
   restructuring), one new WGSL file, one texture pair, ~1–2 weeks. Reversibility: each phase is
   an independent gated commit; the oracle path's byte-equality is *structural* where it matters —
   L1 never touches WGSL (a larger buffer binds fine to the old block), L3's offscreen composite
   is a same-format `copy_texture_to_texture`, bit-exact by construction. The gates don't have to
   be lucky; the design removes the luck.
5. **Even the failure mode is cheap.** If L3's composite proves pointless post-K, L1/L2 still pay
   as structure (the phase lists are where selection/labels/UI passes hook; the frame uniform is
   where deterministic-rendering flags live). There is no version of executing this plan that
   leaves the repo worse-gated or more entangled than it started.

## The counterarguments, steel-manned and answered

- **"You are not rerun — their machinery exists because their problems aren't yours."** Correct,
  and it's half the decision: the refusal list (07's side-by-side table) is as deliberate as the
  borrow list. Their pools/belts/dispatch exist for 12+ primitive types, fully-dynamic per-frame
  data, and browser targets; we have 2 pipelines, a static 95M-instance arena cheaper than any
  belt, and a native-Metal posture. We borrow the *shape*, refuse the *machinery*, and the refusal
  has triggers (pipeline count >8, MB-scale uploads, a greenlit wasm port) so it's a position, not
  a blind spot.
- **"Why not just depend on `re_renderer`? It's published, standalone, wgpu 30."** Because
  depending means re-expressing the differentiator in their model: our Slug analytic-coverage glyph
  pass, 48 B instance slots, and static-arena-plus-partial-writes would have to become per-frame
  rebuilt `DrawData` — trading our cheaper model for theirs, and adopting their egui-based text
  over our engine. Their edition-2024/MSRV-1.96 tree would drag ours. And we'd inherit a
  dependency to re-evaluate at every wgpu major. Note 05 posture 2 ruled this before the dive; the
  dive confirmed it.
- **"YAGNI — multi-view and selection masks are speculative."** The demands are in the stage
  reports and the `out/` directory, not in a wish list (see point 1 above). And this repo's own
  self-description (main.rs: "dev/verification tool, not a product CLI") means debug ergonomics
  *are* the product — stages F, G, H, I, J were all tooling/hygiene for exactly this workflow.
- **"An extra fullscreen pass and a texture pool cost something."** ~0.1–0.3 ms GPU and two
  textures, FPS-band-gated (≥30-frame runs, per the Stage H noise lesson). If the band breaks, the
  phase doesn't land.
- **"The composite round trip could shift pixels."** On the gated path it's a copy, not a round
  trip; on the windowed path (shader composite, BGRA surface) there's no byte gate to break. The
  one place bytes are contractual is safe by construction, not by argument.
- **"Why L next, instead of labels / editing / the ECS refactor?"** Sequencing, not superiority:
  L is small and mechanical while the egui surface from K is fresh in the same files; labels
  (roadmap item 6) hook into L2's phase lists; editing needs a resizable arena (its own stage,
  Stage G's named gap); the ECS refactor (note 01) has an explicit complexity trigger that hasn't
  fired. None of them are blocked by L, and L3's targets make two of them cheaper.

## What would change our mind (falsifiers — the tripwires)

1. **Any L-phase gate fails un-explainably** (a byte divergence the copy-path design says can't
   happen, or a FPS-band break) → revert the phase, stop the stage, re-argue from 07. The plan's
   gates are its falsifier; "re-baseline casually" is never an answer (AGENTS.md).
2. **K lands and nobody wants the overview/screenshot features in practice** → defer L3+
   indefinitely (keep L1/L2 as pure structure). The decision record survives; only its tail
   shortens.
3. **The wasm port is greenlit** (`research/wasm-port-audit.md` found the render path engine-free)
   → re-run the tier/data-texture refusals; some "machinery" becomes relevant on WebGL fallback.
4. **The wgpu Metal indirect-draw fix ships** → revisit the CPU-cull standing design and the GPU
   pick complement; a wgpu bump is its own re-baselined mini-stage regardless.
5. **Ecosystem re-alignment** (Bevy's bevy_render reaches wgpu 30; iced lands wgpu 30) → posture-3
   math changes; re-check note 05 quarterly watchlist, which subsumes this.
6. **Pipeline count crosses ~8 with runtime-created variants** → the pool refusal flips to adopt.

## Options considered (one-line dispositions; detail in note 05's four postures)

| option | disposition |
|---|---|
| Depend on `re_renderer` | rejected — rewrite of the differentiator, dep surface at every wgpu major (posture 2's "don't depend") |
| **Borrow structure, refuse machinery** | **chosen** — this record |
| Do nothing beyond K | viable floor — L1/L2 still recommended (near-zero cost, hooks for what's next); L3+ deferrable via tripwire 2 |
| Bevy-as-shell | on-demand spike only — pins wgpu 29 in bevy_render, ~4-month churn cadence (posture 3) |
| Other hosts (vello/renderling/notan/pixels/kiss3d) | closed — wrong computational model, stale, or own-renderer traps (posture 4) |

## Provenance

rerun @ `cb5e9d6` (2026-09-01, shallow clone at `<rerun-clone>`; the
load-bearing files read in full, line-cited in 07). Our tree @ `cb35552` (post Stage I/J). Demand
evidence: `out/STAGE_F|G|H|I|J_REPORT.md` gap lists + the `out/*.png` wide/zoom pair pattern.
Execution artifacts: `08-view-structure-handoff.md` (L), `09-egui-ui-handoff.md` (K, non-rerun).
Review cadence: the quarterly watchlist in note 05 covers this decision's environment; tripwires
above cover its specifics.
