# STAGE L REPORT — view-structure borrow (re_renderer → glyph3d-native)

## Goal

Execute the structural half of note 07 as Stage L (per
`integration/notes/08-view-structure-handoff.md`): formalize the frame
uniform (L1), phase-partition the draw lists (L2), then a pooled render
target + composite step (L3) — all provably output-neutral behind the
standing byte-equal gates. **Owner-approved scope: L1 and L2 only** (L3/L4
and the O1/O2 opportunistic steals await explicit approval).

## Result

**Phases L1 and L2 landed** (L1 `67e93d3` + report `534e1dd`; L2 `94ee788`;
2026-09-03): `FrameUniform` carries the frame's values behind the unchanged
WGSL block, and the draw lists are phase-partitioned (`enum Phase { Backdrop,
Glyphs }` + `PhaseDraws`) with byte-identical output on BOTH the culled and
legacy `--no-cull` paths. Owner-approved scope (L1+L2) is complete; L3/L4 and
O1/O2 await explicit approval.

---

## STEP 0 — baseline (HEAD `2f9d62c`, 2026-09-03)

- `bash tools/check-all.sh`: **CHECK-ALL: ALL GATES GREEN** (8 gates:
  generator byte-identity ×4, 15→16 Mojo conformance suites CPU+GPU, build
  0 warnings, clippy 0 warnings, 28+1 tests, engine-check bit-exact,
  stage-g picks PASS, four-view A/B byte-equal). The in-flight
  `engine/conformance_matrix.mojo` (parallel session) was clean at baseline
  and untouched throughout.
- **Verb smoke pair** (deterministic offscreen, `fixtures/g-pick-repo`,
  `--frames 2`):
  - `out/tooling-ab/stagel-base/verb-glyph.png` —
    `pick: alpha.rs group=0 rec=3 row=0 col=3 line=0 byte=3 char=' ' slot=3 pos=(1.59,0.00,0.00)` +
    `verb recolor-glyph: alpha.rs row 0 col 3 slot 3 -> #ff5050 (4 B)`
  - `out/tooling-ab/stagel-base/verb-group.png` — same pick +
    `verb move-group: alpha.rs group 0 offset -> (5.0,3.0,0.0) (80 B row)`
- **Cull counters** (`GLYPH_CULL_DEBUG=1`, 1 frame, g-pick-repo):
  `CULLDBG glyph draws=4 instances=10857 | backdrops=0` (plus the
  construction log `cull: 4 segments …`). L2 acceptance input.
- **Pass timings** (`GLYPH_PROFILE=1`, 30 frames, g-pick-repo):
  `profile: 30 frame(s) measured | GPU: glyph field pass 5.273ms | CPU: cull (CPU) 0.000ms`;
  `offscreen: steady-state ≈ 694.9 fps (30 frames GPU-completed in 43.17ms)`.

### STEP 0 deviation (handoff vs code — code won)

The handoff's verb-smoke sketch renders `fixtures/baseline-view.txt` — but
`--render-file` builds a TEXT scene, which has NO pick context
(`GlyphScene::apply_pick`: "this scene has no pick context (repo mode
only)"), so verbs there are no-ops ("verb: nothing picked yet — ignored").
The smokes above use the g-pick-repo fixture with a preceding
`--pick-file/--pick-row/--pick-col`, mirroring `tools/check-stage-g.sh`'s
invocation shape — the verbs actually execute and the PNGs actually contain
their effects. Same purpose (L2/L3 visual A/B), working inputs.

---

## Phase L1 as-executed (commit `67e93d3`)

### What moved (glyph_scene.rs only, +64/−6)

- `CameraUniform` → **`FrameUniform`** (`#[repr(C)]`, Pod/Zeroable +
  `encase::ShaderType` like GlyphInstance/GroupRow):
  `view_proj: [f32;16]` @0 (64 B, byte-identical) · `eye: [f32;3]` @64 ·
  `_pad0` @76 · `viewport: [f32;2]` @80 · `px_scale` @88 · `time` @92 ·
  `flags: u32` @96 · `_pad1` @100 — **104 B total**.
- **Size surprise (code won over prose):** the handoff says "pad to the
  encase-correct size", which I first computed as 112 B assuming 16 B tail
  padding to GPU vec4 alignment. Wrong: repr(C) arrays are align-4 in Rust
  (no GPU vec alignment), so there is no tail padding — Rust `size_of` and
  encase `SHADER_SIZE` agree at **104 B**. The new layout test caught it on
  first run; struct comment and buffer-creation comment corrected. 104 is a
  valid wgpu uniform buffer size (multiple of 4; ≥ the 64 B minimum binding
  size; no dynamic offset).
- **The no-WGSL trick, verified against the actual code:** WGSL
  `struct Camera { view_proj: mat4x4<f32> }` (glyph_field.wgsl:62) = 64 B;
  both bind group layouts that take `camera_buf` (glyph pipeline, backdrop
  pipeline) use `min_binding_size: None`, so wgpu derives 64 B from the
  shader and the 104 B buffer binds cleanly. Zero `.wgsl` bytes touched.
- **Fill** in `render()`: `eye` = `frame.eye` (the actual cull/projection
  eye), `viewport` = target width/height, `time` = `t`, `flags` = 0 (bit 0
  reserved `deterministic_rendering`). `px_scale` is now computed ONCE and
  shared between the uniform and the `CullView` cull input (it was
  CullView-local pre-L1; the uniform needs it even under `--no-cull`) —
  same formula, same value, so cull behavior is unchanged.
- Buffer label renamed `camera uniform` → `frame uniform` (cosmetic;
  O2-spirit, zero output change).
- **Layout pins:** `layout_tests::frame_uniform_size_and_offsets` asserts
  every field offset against encase `METADATA` (view_proj pinned at 0..64)
  and `size_of == SHADER_SIZE == 104`. 29+1 tests green.
- **Demo `Scene`'s own `CameraUniform` (scene.rs): untouched** — only
  GlyphScene migrates.

### Gates

`check-all.sh`: **ALL GATES GREEN** — build 0 warnings, clippy 0 warnings,
29+1 tests (new pins green), engine-check bit-exact, stage-g ALL PASS, and
the four PNGs BYTE-EQUAL (demo/text/repo-zoom/repo-wide): the shader never
saw a change, so the oracle path is trivially unaffected. `cargo tree -d`:
duplicate list identical to the post-K tree; zero new deps (fence 1);
Cargo.lock untouched.

### Files

`native/src/glyph_scene.rs` +64/−6. Nothing else.

---

## Remaining gaps / watchlist (unchanged from the handoff unless noted)

- L2 (phase enum + per-phase draw lists) — next delegation; acceptance
  inputs captured above (cull counters, verb smokes).
- L3/L4, O1/O2 — await owner approval.
- Handoff's watchlist items stand (GPU cull+indirect on the wgpu Metal
  `first_instance` fix, DeviceCaps tiers re-run if wasm is greenlit,
  descriptor-keyed pipeline pool past ~8 pipelines, CpuWriteGpuReadBelt if
  uploads reach MBs).

---

## Phase L2 as-executed (commit `94ee788`, glyph_scene.rs +135/−86)

### What moved

- **`enum Phase { Backdrop, Glyphs }`** (near `CullView`, glyph_scene.rs) —
  re_renderer's DrawPhase borrow: flat enum, per-phase work lists, no render
  graph. No `Selection`/`Overlay` variants (they arrive WITH their phases;
  Stage J's no-dead-code rule).
- **`struct PhaseDraws { backdrops: Vec<BackdropInst>, glyph_ranges:
  Vec<(u32, Range<u32>) }`** — one frame's draw work, partitioned by phase.
- **`cull_segments` returns `PhaseDraws` directly** (handoff-literal: the
  phase lists are built during cull). The per-chunk `Vec<Vec<Range>>` is
  still constructed inside (the segment loop is chunk-splitting), then
  flattened **chunk-major** — ascending chunk, arena-ascending ranges within
  a chunk. This ordering is load-bearing: within-pixel alpha blend order
  follows record order, and the flat list's record order is identical to the
  pre-L2 loops. (A segment-major flatten would have reordered draws across
  chunks and risked blend-order divergence — avoided by construction.)
- **`render()` recording** iterates `[Phase::Backdrop, Phase::Glyphs]` and
  matches the phase. Empty phases record nothing: the legacy `--no-cull`
  branch builds `glyph_ranges` straight from `chunk_counts` (one full range
  per chunk — identical draw sequence to the pre-L2 loop, bind group set per
  chunk exactly once) and its empty `backdrops` skips the Backdrop phase just
  as the pre-L2 legacy branch had no backdrop stream. In the culled path the
  bind group is re-set only on chunk change; range-less chunks set none
  (pre-L2: `continue`). Profiler query names unchanged: "glyph field pass",
  "backdrop stream", "glyph stream".
- **K4 seams preserved**: `CullView`/`CullState::lod_min_px` untouched; the
  `GLYPH_CULL_DEBUG` t=0 counters and the Debug-panel probe readouts compute
  the SAME sums from the flat lists (`glyph_ranges.len()`, range-length sum,
  `backdrops.len()`).
- Module header gained an L2 paragraph (and a doc-lint trip: a line starting
  with `+` parses as a markdown list item under `doc_lazy_continuation` —
  reworded).

### Acceptance evidence

- `check-all.sh`: **ALL GATES GREEN** (8 gates; four-view A/B byte-equal).
- **Cull counters identical to STEP 0**: `CULLDBG glyph draws=4
  instances=10857 | backdrops=0` — the partition did not perturb cull
  outputs.
- **Both render paths byte-equal vs pre-L2 captures** (pre-L2 PNGs captured
  from `67e93d3`'s binary into `out/tooling-ab/stagel-base/` before editing):
  `cull-pre-l2.png` vs `cull-post-l2.png` BYTE-EQUAL; `nocull-pre-l2.png` vs
  `nocull-post-l2.png` BYTE-EQUAL.
- **Verb smoke pair byte-equal vs STEP 0** (`out/tooling-ab/stagel-l2/`):
  `verb-glyph.png` and `verb-group.png` both `cmp`-clean, with the same
  pick/verb log lines as STEP 0.
- `cargo tree -d`: 18 duplicate entries, identical to post-L1 — zero new
  deps; Cargo.lock untouched.

### Deviations

None. The handoff's shape mapped onto the code without a fight; the only
judgment call was WHERE the chunk-major flatten happens (inside
`cull_segments`, keeping its per-chunk construction — the handoff's "built
during cull_segments" taken literally).

## Files (stage total so far)

| File | +/- | Content |
|---|---|---|
| `native/src/glyph_scene.rs` | +199/−92 | L1 FrameUniform + L2 phase lists |
| `out/STAGE_L_REPORT.md` | new | this report |
