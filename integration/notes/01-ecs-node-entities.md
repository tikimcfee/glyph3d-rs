# 01 — ECS / node-entity relationships for the group table

*Research date 2026-09-01; versions verified against crates.io dependency manifests and
GitHub activity on that day. Companion: `00-primitives-inventory.md`.*

## TL;DR

The "node-entity relationship" free lunch **exists, but only as a data-model substitution,
not a renderer**: `bevy_ecs` 0.19 standalone (no wgpu/winit/glam deps — verified from its
manifest) is the leading candidate, `hecs` 0.11 is the minimal counter-proposal, and
`petgraph` 0.8.3 is the right substrate for the code-relationship graph itself. The
95M glyphs stay GPU instance data either way — only the ~1.3k file groups / cull
segments / camera / selection become entities. The strongest validation found: **Bevy 0.19
deleted its own RenderGraph and reduced its renderer to "regular systems in schedules"
over wgpu** — i.e., the architecture you'd be adopting is the one the biggest Rust engine
just converged on from the other direction.

## What you'd be re-expressing

Today's parallel arrays keyed by `group_id == file index` (see inventory §glyph_scene):

| hand-rolled today | would become |
|---|---|
| `Vec<GroupRow>` (80 B TRS/color rows) | `Transform`/tint components on per-file entities |
| `CullState` side-tables (local AABB, base tint, hidden) | components on the same or child entities |
| `PickContext.files: Vec<PickFileInfo>` | components (rel_path, slot range, ItemParams, AABB) |
| `sync_segment()` re-derivation after edits | an observer / derived system on group-change |
| `geom_overrides` map | a component (sparse) on glyph-run child entities |
| camera state + modes | camera entity |
| `SceneLike` trait dispatch | systems in a schedule (or stays as the render-side seam) |

Wins: relationships (`ChildOf`/`Children`, or hecs-style `Children(Vec<Entity>)`) formalize
file→segment→glyph-run; observers kill the manual re-sync invariants that Stage G had to
document at three call sites; selection/highlight state stops being ad-hoc.

Costs: API churn (bevy_ecs migrates every ~4 months, MSRV 1.95), and the determinism chain
must not move — the ECS only reorganizes *CPU bookkeeping*; byte→record→instance ordering
is untouched because entities reference slot ranges, they don't contain instances.

## Candidates (all verified 2026-09-01)

| crate | version / date | status | wgpu-30 / glam-0.30 coupling | verdict |
|---|---|---|---|---|
| **bevy_ecs** | 0.19.1, 2026-08-13 | Bevy org, ~4-month cadence | none (headless kernel: no wgpu/winit/glam deps — verified manifest) | **ADOPT (leading)** |
| **hecs** | 0.11.1, 2026-07-28 | Ralith active — commits 2026-09-01 | none | **ADOPT (minimal alternative)** |
| shipyard | 0.11.5, 2026-07-10 | active (leudz) | none | reference-only (no relationship primitives; nothing beats the top two here) |
| flecs_ecs (C bindings) | 0.2.2, 2025-11-17 / flecs C 4.1.6 | active; binding self-described **alpha** | C toolchain + FFI | wrap ONLY if relationships-first modeling wins and alpha/C risk is acceptable |
| evenio | 0.6.0, 2024-05-19 | stalled ~28 months | — | reject |
| legion | 0.4.0, 2021 | dead (5 yrs) | — | reject |
| **petgraph** | 0.8.3, 2025-09-30 | ecosystem bedrock (488M dl) | none | **ADOPT** as the import/dependency/semantic graph feeding layout/grouping |
| edict | 1.0.0-rc10, 2026-07-24 | active, RC for 2 yrs | none | honorable mention |

**Relationships detail (bevy_ecs):** Bevy 0.16 replaced parent/child with generalized
`ChildOf`/`Children` relationship components (hook-maintained, constant-time, custom
relationship types via `#[relationship]`). 0.19 made resources components on singleton
entities and added `commands.delayed()`. Observers are mature (run-conditions work on
them as of 0.19).

**Precedent that bevy_ecs + custom wgpu renderer works:** Bevy 0.19 itself (RenderGraph
deleted; render passes are systems on a render World), hotline-rs, several community
engines; no single canonical maintained template exists — that space is fragmented, which
is the honest caveat.

## Scene-graph / "host renderer" options checked (all rejected as hosts)

- **renderling** — concept is *your architecture taken further* (scene graph resident on
  GPU in a slab: nested nodes, GPU frustum/occlusion cull, GPU picking = "group table on
  steroids"). But crates.io release 2 yrs stale; main branch pins **wgpu 26** (vs your 30),
  rust-gpu toolchain, alpha. **Mine its GPU-slab scene-graph design; don't wrap.**
- **three-d 0.19** (2026-04-17) — no wgpu backend at all (glow/OpenGL + cgmath + winit
  0.28; the wgpu port is issue #221, open for years). Reject as dep; its
  `renderer::control` (OrbitControl/FlyControl, event-enum-decoupled) is worth reading.
- **nannou 0.20** (2026-06-22, revived) — full creative-coding framework on **wgpu 29**,
  now pulling bevy for windowing. Reject.
- **kiss3d 0.46.0** (2026-08-15, **revived by Dimforge**) — the sleeper: verified
  **wgpu ^30 + winit ^0.30**, BSD-3, `SceneNode3d` attach/transform tree, orbit +
  first-person cameras, optional egui overlay. Caveats: 11 breaking releases in 11
  months, own renderer (you can't push your glyph field through it), glamx math bridge
  (exact glam version unverified). **Not a host for GlyphScene, but the only off-the-shelf
  wgpu-30 window/camera scaffold — plausible for separate tool/preview windows, or as
  reference.**
- **Rerun 0.37.0** (released 2026-09-01) — production precedent on **wgpu 30 + egui 0.36**:
  custom renderer (`re_renderer`) + columnar Arrow entity store, "ECS-shaped data without
  an ECS framework." Not reusable as a library, but decisive evidence that
  *hand-rolled renderer + columnar data* scales — validates both your current design and
  the "keep the renderer, adopt the data model" posture. Architecture reference:
  rerun-io/rerun `ARCHITECTURE.md`.
- **rend3** — dead since ~2022, forks ground down by wgpu churn. Cautionary tale against
  git-pinning renderling.

## Force-directed / code-graph layout (the "node relationships → positions" half)

- fdg_sim 0.9.1 — stale (2022). force_graph 0.4.0 — revived 2025-11-21, simple CPU option.
- forceatlas2 0.8.0 — n-dimensional and active but **AGPL-3.0-only** → license-blocked.
- **No maintained GPU-accelerated force-layout crate exists (Sep 2026).** Realistic path:
  a small WGSL Fruchterman-Reingold/ForceAtlas2 compute pass over per-file positions —
  you already run compute + cull; seeded by petgraph data.

## Recommendation

1. **Now:** nothing. The group table works and is verified; ECS is a refactor with real
   payoffs, not a bug fix. Stage it as its own stage (the repo works in stages).
2. **If/when scene complexity grows** (selection sets, semantic groups, folder
   hierarchies, layout solvers): adopt **hecs 0.11** if you want minimal substrate and
   keep your loop (lowest risk, matches current structure); adopt **bevy_ecs 0.19** if you
   want relationships + observers + schedules and accept the churn. Either way:
   **petgraph 0.8.3** for the code graph, glyphs stay GPU data.
3. **Watchlist:** flecs_ecs (relationships), masonry/vello line (UI note §04), kiss3d
   (tooling windows), renderling (GPU scene-graph ideas).

*Sources: crates.io API metadata + per-version dependency manifests (all quoted
version/date pairs), bevy.org 0.16/0.19 release notes, github.com/Ralith/hecs,
github.com/SanderMertens/flecs, github.com/schell/renderling, github.com/dimforge/kiss3d,
github.com/rerun-io/rerun (incl. ARCHITECTURE.md), github.com/asny/three-d issue #221.*
