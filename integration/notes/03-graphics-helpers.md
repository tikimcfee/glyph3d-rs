# 03 — Graphics helpers: the near-zero-risk shopping list

*Research date 2026-09-01, versions verified via crates.io manifests/releases that day.
This is the "adopt now" note — everything here is outside the determinism chain except
encase/etagere, which carry explicit caveats.*

## TL;DR

**STATUS: the adopt-now tier landed in Stage H (Sep 2026) — see
`glyph3d-native/out/STAGE_H_REPORT.md`.** All five phases clean: naga 30.0.1
(dev-dep, single tree instance), glam flags, clap 4.6.6 + clap_complete 4.6.9
(15-test parity suite), wgpu-profiler 0.28.0 (behind `GLYPH_PROFILE=1`), encase
0.12.1 layout assertions. Every gate green: 4× byte-identical screenshots,
`--engine-check` PASS, stage-G ALL PASS, 19 tests, zero-warning builds, no new
`cargo tree -d` duplicates.

Four adoptions are effectively free today: **wgpu-profiler 0.28** (verified
`wgpu ^30.0.0` + `winit ^0.30`), **naga 30.0.1** WGSL validation in tests
(auto-paired — naga is already in your tree via wgpu), **clap 4.6** for the hand-rolled
CLI, and **glam feature flags you already own** (`approx`, `debug-glam-assert`, `mint`).
Two more are conditional: **encase** (buffer layout with generated offsets — the bug
class Stage G hit) and **etagere** (only if dynamic atlas regions arrive). One strategic
lever recurs: **glam 0.30 → 0.33** unlocks a cluster of currently reference-only crates.

## The list

### Adopt now — verified exact-stack matches

| crate | version / date | why it's free for glyph3d-native |
|---|---|---|
| **wgpu-profiler** | 0.28.0, 2026-07-31 (Wumpf) | THE wgpu timestamp profiler: per-pass scopes, optional automatic GPU trace dumps (viewable in Chrome). Tracks wgpu majors within weeks (0.28 landed ~4 weeks after wgpu 30). Deps verified: `wgpu ^30.0.0`, `winit ^0.30`. Features: `puffin`, `tracy`. Direct fit for the stage reports' FPS-only timing. **Stage H caveat:** this Apple Metal adapter does not expose `TIMESTAMP_QUERY_INSIDE_PASSES`, so nested in-pass scopes report no time — pass-level timings are the measurement here (Stage H report, gap #3). Also: raise `max_num_pending_frames` (Stage H: 3 → 64) or short offscreen runs silently drop frames from measurement. |
| **naga** (+ **naga-cli**) | 30.0.1, 2026-08-22 | Same compiler wgpu 30 bundles (`naga ^30.0.0`) — add `wgsl-in` validation of all three WGSL files to `cargo test`; `naga validate` CLI for manual checks; wgsl-analyzer in editors. Version pairing is automatic if you keep them on the same major. |
| **clap** | 4.6.6, 2026-08-06 | Replaces `parse_cli()` (main.rs:257, self-labeled "not a product CLI") — derive API, generated `--help`/completions. Pure CLI-layer, zero render-path risk. |
| **puffin** | 0.20.0, 2026-03-18 (Embark) | CPU scope profiler; view via standalone **puffin_viewer** (TCP) or wgpu-profiler's `puffin` feature. **Not puffin_egui yet** — its current release pins egui ^0.33.3, which cannot pair with egui-wgpu-on-wgpu-30 (see note 04). Re-check after a puffin_egui release targeting egui 0.36. |
| glam features | 0.30.10 in tree | **LANDED (Stage H).** Corrected names (verified against the published 0.30.10 manifest): the debug-only validation flag is **`debug-glam-assert`** (not `debug-glam-assertions` — and `glam-assert`, the always-on variant, was deliberately NOT enabled); `approx` and `mint` are implicit features from optional deps, not listed in `[features]`, but enabling them by name works. `approx` = epsilon compares for oracle-style tests (bit-exactness stays the standard), `mint` = interop shim. Zero math change, zero release-code change. |
| winit_input_helper | 0.17.0, 2025-09-29 | Optional: pressed/released edge detection on winit ^0.30. Your key-bit handling works; adopt only if input grows. |
| wgpu-text | 30.0.0, 2026-07-08 | Optional zero-egui HUD text (glyph_brush on `wgpu ^30.0.0` + `winit ^0.30.13`) — stats/pick results on-screen without a full UI toolkit. Mostly superseded by egui if you adopt note 04. |

### Conditional adopts

| crate | version / date | condition + caveat |
|---|---|---|
| **encase** | 0.12.1, 2026-08-16 (teoxoy; MIT-0; bevy_render depends on it) | Adopt to kill the layout-mirroring bug class: `#[derive(ShaderType)]` gives generated offsets + `UniformBuffer`/`DynamicStorageBuffer` wrappers. **Note: 0.12 ships no math impls at all** (no glam feature — verified from published manifest); you implement `ShaderType` for your 48 B `GlyphInstance` / 80 B `GroupRow` (trivial — fixed arrays). **Also no `derive` feature** — 0.12.1 re-exports the derive unconditionally (the handoff's `features = ["derive"]` suggestion was wrong; Stage H corrected it). Stage G's strided-color bug (colors stomping neighbors across 48 B strides) is exactly the failure mode encase's layout tests catch. **LANDED (Stage H, option b):** derives + layout tests added, including a decisive byte-equivalence proof (encase serialization == `bytemuck::bytes_of` for distinctive bit patterns, so the two representations can never silently diverge). Write paths deliberately NOT switched — encase serializes through an intermediate buffer, which would add a full copy of the arena (hundreds of MB at repo scale) inside the determinism chain, versus the current zero-copy `bytemuck::cast_slice` upload. Option (b) is the better engineering outcome, not a consolation prize. |
| **etagere** | 0.3.0, 2026-03-18 (nical; WebRender lineage, 2.4M dl) | Only if you allocate dynamic atlas regions (runtime glyph patches, emoji experiments, backdrop variants). Your baked binary atlas (FORMAT.md) doesn't need it. (guillotiere 0.7.0 same author/date — equivalent role.) |
| **tween** | 2.2.0, 2026-02-24 | Engine-agnostic tweening, optional `glam ^0.32` — adoptable after the glam bump; for camera moves / animated verbs. |

**encase is NOT a debug-only tool — nothing to disable in runnable builds.** It's
compile-time layout codegen: `#[derive(ShaderType)]` computes size/alignment/offsets as
`const`s and the buffer wrappers write through them. At runtime it costs ~nothing (it
replaces the same byte math bytemuck code does by hand, plus a one-time validation).
What *is* gateable: wgpu-profiler scopes (timestamp queries have a small real cost —
feature-gate them), naga validation (test-only), glam `debug-glam-assertions`
(debug-only by design).

### Reference-only (port the pattern, not the crate)

- **GPU-driven culling / indirect draw: no reusable crate exists (2026).** Closest:
  **Frizi/dovetail** — experimental two-pass compute occlusion culling + LOD +
  multi-draw-indirect on wgpu; compare against your cull design when the Metal
  indirect bug (wgpu #2148-era) is fixed. Bevy 0.19's GPU-driven internals aren't
  extractable. Note wgpu 30 added `transition_resources` ComputePass explicitly
  "useful for GPU-driven pipelines" — relevant when you revisit the GPU cull pass.
- **Debug lines/gizmos: nothing exists on raw wgpu.** bevy_gizmos 0.19 is the pattern
  (immediate-mode list → batched line/quad mesh); transform-gizmo 0.11 (2026-08-17,
  active) is egui/glam-0.32-based manipulation gizmos — revisit post-glam-bump if you
  want drag-handles for the group verbs.
- **GPU readback: hand-rolled MAP_READ + copy_texture_to_buffer (256 B row pitch) +
  `image` remains the norm**; nothing structured exists. miniscreenshot-wgpu 0.2.1 is
  wgpu 28/29, tiny adoption — polish reference only. Your offscreen oracle is already
  beyond the ecosystem norm here.
- **Glam math ecosystem**: bevy_math 0.19.1 (primitives, Ray3D, curves) is standalone
  but glam ^0.32; cam-geom for calibrated lenses; parry3d only if picking becomes
  general collision.

### Rejected (with reasons, to save the next agent the dig)

crevice 0.20.1 (active but math support is glam ^0.33 — no 0.30); rend3 (dead);
bevy_spectator/smooth-bevy-cameras as deps (bevy-locked/stale); "cameras" 0.3.2
(video-capture crate — name trap); easer/keyframe/interpolation/easing (stale);
texture_packer (sprite-sheet oriented, low adoption); parry3d for picking (overkill,
nalgebra-typed).

## The strategic lever: glam 0.30 → 0.33

Current glam is **0.33.6 (2026-08-28)**, cadence ~2-3 months. Your 0.30 pin is what
keeps trackball, tween, bevy_math, transform-gizmo, crevice, and (partially) kiss3d's
math bridge at reference-only. The migration is mechanical (math call sites only —
bytemuck/GPU layouts don't change), and it's the single change that flips the most
verdicts to "adoptable." Do it opportunistically between stages, not mid-stage.

*Sources: crates.io API + per-version dependency manifests (versions/dates as quoted),
github.com/Wumpf/wgpu-profiler, github.com/teoxoy/encase (published 0.12.1 Cargo.toml),
github.com/Frizi/dovetail, github.com/sunsided/miniscreenshot, wgpu 30 release notes,
github.com/gfx-rs/wgpu issues #742/#2148, docs.rs pages as retrieved 2026-09-01.*
