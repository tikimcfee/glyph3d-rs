# 08 — VIEW-STRUCTURE HANDOFF (Stage L) — for the implementing agent

> **STATUS: PLANNED — not executed.** No commits have been made by the planning pass; the
> working tree at `glyph3d-native` HEAD `cb35552` (stage-j report) also carries *untracked*
> study material from other passes (`integration/egui/`, `integration/egui-integration-report.md`,
> `research/*.md`) — leave it untracked and out of stage commits unless the owner says otherwise.

*You are picking up a scoped engineering task. This document is written to be executed directly
without further context. Companions: `07-rerun-architecture-blueprint.md` (the what-and-why:
every pattern here traces to a rerun `re_renderer` source citation), `00-primitives-inventory.md`
(the ground truth), `native/AGENTS.md` (the house rules — read it first), and
`glyph3d-native/integration/egui-integration-report.md` + `research/egui-integration-notes.md`
(the Stage K specs this plan sequences against).*

## Who wrote this, and how much to trust it

Same division of knowledge as the note-06 handoff: this came out of a research pass (rerun source
dive + a fresh trace of `native/src` at HEAD `cb35552`), not the build. You know the code A–Z;
this doc knows the blueprint and the traps. Where a claim here conflicts with your ground truth,
yours wins — the acceptance gates decide, not the prose. Every fence carries its reason so you
can argue with it.

## Mission

Execute the structural half of note 07 (the re_renderer borrow) as **Stage L**, after the egui
UI stage (**Stage K**) lands: formalize the frame uniform, phase-partition the draw lists, and
give the scene a pooled render target + composite step — the ViewBuilder skeleton — so that
multi-view (overview/minimap), in-window screenshots, and egui-panel views become cheap later.
All of it provably output-neutral behind the standing byte-equal gates. Plus two opportunistic
steals that can land in any commit along the way.

**Why after K, not before:** K is the big user-facing win, specced three ways (note 04, the egui
source study, and its executable handoff `09-egui-ui-handoff.md`), and named "next stage" in
`out/history/STAGE_I_REPORT.md` remaining-gaps #1. L1/L2 are K-order-independent, but
sequencing them after K keeps one-variable-per-gate and means L3's composite integrates against
the real egui pass instead of a prediction. If K slips, L1+L2 may proceed standalone (they touch
nothing K touches); L3 wants K landed.

**Why this is the right moment (the cleanup changed the math):** Stage I retired the text.png
input-sensitivity caveat (immutable `fixtures/baseline-view.txt`), Stage J delivered
`tools/check-all.sh` (six gates, one command) + `native/AGENTS.md` fences + the `FrameTarget`
bundle — which is literally the first half of L1's job — and clippy-zero means every structural
edit is now loudly visible in review. The tree has never been cheaper to restructure safely.

## The workspace

- Crate: `native/` (src/, shaders/, tests)
- Gates: `bash tools/check-all.sh` from the repo root — the ONLY gate command you need
- Pins (do not touch any): **wgpu 30, winit 0.30, glam 0.33.6, edition 2021**
- Key anchors at HEAD `cb35552`: `FrameTarget` `scene.rs:15`, `SceneLike::render` `scene.rs:27`
  (4-arg, takes `&FrameTarget`), `GlyphScene::render` `glyph_scene.rs:2151`, "glyph field pass"
  `:2224`, backdrop stream `:2262`, glyph stream `:2270`, `CameraUniform` `:294`/write `:2167`,
  `cull_segments` `:329`, `CullState` `:549`; windowed seam `windowed.rs:125` → submit `:140`;
  offscreen seam `offscreen.rs:111` → submit `:146`.

## HARD FENCES — each with its reason

1. **Zero new dependencies in Stage L.** Unlike H (tooling crates) and K (egui stack), L is pure
   internal restructuring: the composite shader is ours, the pool is two `create_texture` calls.
   *Why:* the value here is the *shape*, and the shape is free. A dep would add re-baseline
   surface for zero blueprint value. `cargo tree -d` after every phase regardless (house habit).
2. **Shaders: L1–L2 touch NO `.wgsl` bytes; L3 adds exactly one new file (`composite.wgsl`).**
   The naga test enumerates the shader dir, so the new file auto-extends coverage.
   *Why:* `glyph_field.wgsl`'s uniform block and coverage math are load-bearing for byte-equality;
   L1 is specifically designed to avoid needing a WGSL edit (see the trick below). A dedicated
   stage may add shaders; it may not quietly edit the pinned ones.
3. **AGENTS.md fences in full:** `engine-local/`, `assets/atlas/`, `native/fixtures/` read-only;
   fail-loud `expect` style; zero build+clippy warnings; no `cargo fmt` mass-reformat; comments
   explain WHY with stage tags (`// Stage L: ...`).
4. **One variable per commit, `bash tools/check-all.sh` green after each, results in the commit
   message, report at `out/history/STAGE_L_REPORT.md`** (house cadence, per AGENTS.md).
5. **The oracle path stays copy-exact by construction where possible** — see L3's
   copy-vs-shader composite split. Where a gate *could* be argued, it must instead be
   constructed unfailable.
6. **No commits of untracked scratch** (`out/tooling-ab/*`, `integration/`, `research/`) —
   regenerated study material, not stage content.

## STEP 0 — baseline (before ANY edit)

```bash
cd <repo>
bash tools/check-all.sh        # expect: CHECK-ALL: ALL GATES GREEN (6 gates)
```

If anything fails at baseline, STOP and report. Then record the L-specific baselines the six
gates don't cover (L touches render *recording*, so verb paths need visual A/Bs — the note-06
phase-5 pattern):

```bash
cd native
# verb smoke pair (one glyph verb + one group verb), deterministic offscreen:
cargo run --release -- --render-file fixtures/baseline-view.txt --frames 2 \
  --verb "recolor-glyph" --screenshot ../out/tooling-ab/stagel-base/verb-glyph.png
cargo run --release -- --render-file fixtures/baseline-view.txt --frames 2 \
  --verb "move-group" --screenshot ../out/tooling-ab/stagel-base/verb-group.png
# (check the exact verb spellings against parse_verb() in main.rs before running —
#  the op-stream requires a preceding --pick-* for verb targets; mirror check-stage-g.sh's
#  invocation shape if unsure.)
# cull counters (L2's acceptance input):
GLYPH_CULL_DEBUG=1 cargo run --release -- --load-repo fixtures/g-pick-repo --frames 1 \
  --screenshot /tmp/l-base.png 2>&1 | grep -A3 "cull"
# pass timings (before/after comparison for the report):
GLYPH_PROFILE=1 cargo run --release -- --load-repo fixtures/g-pick-repo --frames 30 \
  --screenshot /tmp/l-base.png 2>&1 | grep -i "profile\|fps" | tail -5
```

Record all four outputs in the stage report's STEP 0 section.

---

## Opportunistic steals O1/O2 (any commit, any time — not Stage L content)

- **O1 — uncaptured-error dedup.** `gpu.rs::init` installs nothing on
  `device.on_uncaptured_error`; a repeating validation error would spam per frame. Port
  re_renderer's `ErrorTracker` pattern (`rerun/crates/viewer/re_renderer/src/context.rs:310`,
  `error_handling/`): dedup by (error type, first-seen count), log once. ~1 h, outside every
  gate's scope, zero output change. *Why steal it:* it is the single highest value/effort ratio
  in the whole blueprint, and it makes every future stage's debugging quieter.
- **O2 — debug labels.** Label pipelines and buffers (`"glyph pipeline"`, `"group table"`,
  `"backdrop instances"`, `"arena chunk N"`), matching the existing `"glyph field pass"` label
  convention. RenderDoc/Xcode GPU captures become readable. Zero output change.

Acceptance for both: `check-all.sh` green; nothing else moves.

---

## Phase L1 — FrameUniform widening (no WGSL edit)

**Borrow:** re_renderer's `FrameUniformBuffer` (`global_bindings.rs`) carries everything a
frame needs — including a `deterministic_rendering` flag — behind one reserved bind group.

**Do:**
- Rename `CameraUniform` → `FrameUniform` (`glyph_scene.rs:294`) and grow it: keep `view_proj`
  at offset 0 (byte-for-byte the same 64 B), then append `eye: [f32; 3] + pad`, `viewport:
  [f32; 2]`, `px_scale: f32`, `time: f32`, `flags: u32` + pad to the encase-correct size. Bit 0
  of `flags` is reserved `deterministic_rendering` (re_renderer's `RenderMode::Deterministic`
  idea); set it to 0 everywhere for now — nothing consumes it yet.
- **The trick that keeps WGSL untouched:** the glyph_field.wgsl uniform block stays declared at
  its current size; wgpu only requires the bound buffer ≥ the block's minimum binding size, so a
  larger Rust-side struct binds fine to the unchanged block. No shader bytes move; the extra
  lanes are consumed in a future stage when a shader change is warranted anyway.
- Fill the new lanes in `render()` (`glyph_scene.rs:2151`) from values already computed there
  (`frame.eye`, `width/height`, the `px_scale` from the cull block, `t`).
- Extend the existing encase layout tests (the Stage-H `layout_tests` module) with
  `FrameUniform` size/offset assertions, `view_proj` pinned at 0..64.

**Accept:** `check-all.sh` green (all four PNGs byte-equal — the shader never saw a change);
`cargo test` including the new layout pins; the demo `Scene`'s own `CameraUniform` (separate
struct in `scene.rs`) is left alone — only `GlyphScene` migrates.

## Phase L2 — Phase enum + per-phase draw lists

**Borrow:** `DrawPhase` (`rerun/…/draw_phases/mod.rs:29`) — a flat enum partitioning draw
order, per-phase work lists, no render graph.

**Do:**
- In `glyph_scene.rs`, introduce `enum Phase { Backdrop, Glyphs }` (reserve `Selection`,
  `Overlay` as variants only when their phases exist — no dead code, Stage J removed the
  `#[allow(dead_code)]`es; add variants when used).
- Restructure `render()`'s pass body so recording iterates phase lists: build
  `draws_by_phase` (today: backdrop quad count + per-chunk segment ranges) during
  `cull_segments`, then record `Backdrop` first, `Glyphs` second — exactly today's order
  (`:2262`, `:2270`). Behavior identical; the profiler query names stay.
- Keep the `--no-cull` legacy branch working through the same phase lists.

**Accept:** byte-equal PNGs; the `GLYPH_CULL_DEBUG` t=0 counters **identical to STEP 0**
(same visible ranges, instance count, backdrop count — the partition must not perturb cull
outputs); `--no-cull` path spot-checked byte-equal too.

## Phase L3 — pooled view target + composite (the ViewBuilder borrow)

**Borrow:** `ViewBuilder` (`rerun/…/view_builder.rs:433`) — render each view into pooled
targets, then composite into whatever pass the host provides; composite-as-a-draw, not a
special case.

**Do:**
- New `ViewTarget` (or extend `CullState`'s owner — your call, keep it small): a ping-pong pair
  of `Rgba8UnormSrgb` + one `Depth32Float`, sized to the viewport, `RENDER_ATTACHMENT |
  COPY_SRC | TEXTURE_BINDING`, resized on `set_viewport`. Two textures, not a pool — re_renderer's
  `DynamicResourcePool` is reference material, not a dependency.
- `GlyphScene::render` draws the phase lists into the pooled target instead of
  `target.color_view`/`depth_view`, then composites into the driver's view:
  - **Offscreen/oracle path — `encoder.copy_texture_to_texture`.** Same format, 1:1, no
    scaling: bit-exact by construction. This is the gate-critical path and it *cannot* fail a
    byte compare. (`offscreen.rs:111` driver's FrameTarget view is `Rgba8UnormSrgb` — same
    format; assert it.)
  - **Windowed path — shader composite.** The surface is typically `Bgra8UnormSrgb` on Metal,
    which is *not* copy-compatible with RGBA; draw a fullscreen quad through a new
    `composite.wgsl` (sample pool texture, write out; premultiplied passthrough). This path is
    not PNG-gated (surfaces aren't byte-gated) — its check is the ≥30-frame FPS band + visual
    smoke + the oracle path proving the scene pass itself unchanged.
  - Document in the stage report why the split exists (component-order copy incompatibility),
    and that a future scaled/sub-rect composite (minimap inset) extends the shader path only.
- Assert `sample_count == 1` on the glyph pipelines at pool creation (copy is invalid on
  multisample; we are non-MSAA today — make that assumption loud, it was silent before).
- `--no-composite` CLI flag: routes render back to direct-to-FrameTarget (the L2 code path).
  Used for the A/B neutrality proof, then **removed at stage end** — one release path only;
  the report records the removal.
- Migrate `GlyphScene` only. The Stage A demo `Scene` keeps direct rendering (it is the
  minimal-template scene by design; migrating it buys nothing and doubles review surface).
- **egui wrinkle to record, not solve:** if a future egui panel hosts the view as a native
  texture, `register_native_texture` demands `Rgba8Unorm` (non-sRGB) — that variant would want
  its own non-sRGB pool or a shader conversion. Out of scope; noted so it surprises nobody.

**Accept:** `check-all.sh` green — with the copy path live, the four PNGs are byte-equal *by
construction*, and the `--no-composite` A/B run must also be byte-equal (proving the pool
draw itself is neutral); verb-smoke pair byte-equal vs STEP 0; windowed ≥30-frame FPS within
the pre-L band (one extra fullscreen pass ≈ 0.1–0.3 ms is the expected, acceptable cost);
`GLYPH_G_DUMP` readback still works; `Scene` demo untouched.

## Phase L4 — (stretch) selection mask pass

**Borrow:** `OutlineMaskProcessor` (mask pass + post process) — the principled shape for Stage
G's open gaps "sticky flash" and "selection ranges". Reference reads: `re_renderer_examples`
bins `outlines` and `picking`.

**Do (only if L1–L3 landed green):** add `Phase::Selection`; render selected segments' glyph
quads into a mask target (reuse the pool machinery); composite a tinted mask contribution in
the windowed shader path. Replaces the click-flash state hack with per-selection state.

**Accept — different gate, by design:** this phase *changes pixels when a selection exists*.
Gate: with no selection, all four PNGs byte-equal (flash path removed → no-flash frames must
not change); `check-stage-g.sh` ALL PASS (picks unaffected); selection-on renders reviewed
visually (one screenshot set in the report); no new deps.

---

## Watchlist-triggered (record in the report's remaining-gaps; do not start)

- **GPU cull + indirect draws** when the wgpu Metal `first_instance` fix ships — then borrow
  re_renderer's `PickingLayerProcessor` *shape* for a GPU pick complement (CPU pick + python
  oracle stays the contract). Any wgpu bump is its own re-baselined mini-stage (AGENTS.md).
- **DeviceCaps-style tiers**: note 07 refused them ("web sibling is glyph3d-js"); since then
  `research/wasm-port-audit.md` found the render path is engine-free and a wasm gate is
  plausible (Text scene works today without Mojo). If the wasm port is ever greenlit, re-run
  the tier decision (WebGPU-first wasm needs no data textures; a WebGL fallback tier would).
- **Descriptor-keyed pipeline pool** if pipeline count passes ~8 (composite + selection + label
  variants put us at 5–6; the pool pays when *runtime-created* variants appear).
- **`CpuWriteGpuReadBelt`** if per-frame uploads grow from KBs (backdrop stream ≈ 42 KB) to MBs
  (animated layouts).

## Never (constraints, unchanged)

Determinism chain (engine FFI, `fold_leaders` oracle, PNG bytes); depth convention `Less`/1.0
(reverse-z would re-baseline every PNG for zero gain); ascending-arena blend order; CPU pick
pipeline; `SceneLike` trait shape (both drivers and both scenes ride it — L changes impls, not
the contract).

## Final checklist for `out/history/STAGE_L_REPORT.md`

Goal/Result header; per-phase detail (what moved, file:line); verification table (six
`check-all.sh` gates per commit + the L-specific extras: cull counters, FPS band, verb smokes,
`--no-composite` A/B, and for L3 the copy-vs-shader rationale); file diffs; remaining gaps
(watchlist items above + anything discovered). One commit per phase; gates in every commit
message; O1/O2 land as their own tiny commits whenever convenient.

---

## Addendum 2026-09-03 — Stage L COMPLETE (L1–L4 landed, O1/O2 stolen)

Executed against a post-K tree (the handoff targeted `cb35552`). L1 FrameUniform
(104 B, zero WGSL edits — bigger buffer binds to the unchanged block); L2 Phase
enum + per-phase draw lists (chunk-major flatten is load-bearing — blend order
follows record order); L3 pooled ping-pong view target + composite (copy path
on the oracle driver, bit-exact by construction AND empirically; `composite.wgsl`
shader path windowed; `--no-composite` escape hatch proved neutrality then was
removed); L4 selection mask (`Phase::Selection`; click-flash hack removed —
pick selects, miss clears, persistent until next pick). O1/O2 steals landed as
their own commits. Notable deviations recorded in `out/history/STAGE_L_REPORT.md`:
wgpu 30's default error handler PANICS (O1 deliberately changed semantics to
log-once, owner-ratified with a loud `[GPU-ERROR]` marker); `wgpu::Color::BLACK`
is (0,0,0,1) — a mask "clear" bug the K6 pixel seam caught in minutes; no
re-baseline was needed anywhere (stage-g proofs never contained flash pixels;
offscreen has no mask path by construction). Watchlist triggers unchanged
(pipelines now ~5–6 of the ~8 pool trigger).
