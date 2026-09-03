# STAGE L REPORT — view-structure borrow (re_renderer → glyph3d-native)

## Goal

Execute the structural half of note 07 as Stage L (per
`integration/notes/08-view-structure-handoff.md`): formalize the frame
uniform (L1), phase-partition the draw lists (L2), then a pooled render
target + composite step (L3) — all provably output-neutral behind the
standing byte-equal gates. **Owner-approved scope: L1 and L2 only** (L3/L4
and the O1/O2 opportunistic steals await explicit approval).

## Result

**Phases L1–L3 landed** (L1 `67e93d3` + report `534e1dd`; L2 `94ee788` +
report `3bf99f2`; steals O1 `c4a62c4` / O2 `1934346` + report `8b17941`; L3
`a6a45f2` + escape-hatch removal `ec6e4aa`; 2026-09-03): the frame uniform is
formalized (`FrameUniform`), the draw lists are phase-partitioned
(`Phase`/`PhaseDraws`), and `GlyphScene` now renders into a pooled
ping-pong view target and composites into the driver's view — copy on the
oracle path (bit-exact by construction AND empirically), shader composite
windowed (proven byte-exact via the `GLYPH_L3_SHADER_COMPOSITE` hook). The
`--no-composite` A/B hatch was used for the neutrality proof and removed at
stage end. All eight gates green at every commit; zero new deps; the
offscreen oracle is byte-identical throughout. L4 (selection mask) awaits
explicit approval.

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

- L4 (selection mask pass) — awaits owner approval.
- O1/O2 — landed (see the steals section).
- **Live windowed re-verification of L3** — environment-blocked this
  session (display occluded; see the L3 section); FPS band + live K6
  composite eyeball pending a human with an awake display.
- **Occlusion busy-spin** (discovered during L3, pre-existing): an occluded
  window spins `request_redraw` at ~100% CPU. Not fixed here (out of scope);
  the fix is a one-line backoff in `about_to_wait`/`render`.
- Handoff's watchlist items stand (GPU cull+indirect on the wgpu Metal
  `first_instance` fix, DeviceCaps tiers re-run if wasm is greenlit,
  descriptor-keyed pipeline pool past ~8 pipelines — L3 puts us at 5,
  CpuWriteGpuReadBelt if uploads reach MBs).

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

---

## O1/O2 as-executed (opportunistic steals — explicitly NOT Stage L content)

Both landed 2026-09-03, one commit each, gates green after each.

### O1 — uncaptured-error dedup (`c4a62c4`, gpu.rs +169)

**API shape (as landed):** `gpu.rs` gains a pure `ErrorTracker`:
`HashMap<(ErrorKind, String), u64>` counts per error identity;
`track(kind, description) -> TrackDecision::{First, Repeat(total)}`; first
occurrence logs the full description at `error!`, repeats are silent except
a one-line summary at powers of ten (10, 100, 1_000, …). `init()` installs
one `Arc` closure via `device.on_uncaptured_error` (wgpu 30 signature
`Arc<dyn Fn(Error) + Send + Sync>`, verified against the wgpu-30.0.1
source). re_renderer's `ErrorTracker` contributed the *pattern* (dedup by
error identity, count, log once) — not its wgpu-core downcasting heuristic
(we key on (kind, description) directly).

**Deviation with reason (important):** the steal's premise — "a repeating
validation error would spam per frame" — is wrong for this wgpu line:
wgpu 30's DEFAULT uncaptured handler **panics** on the first error
(`default_error_handler`, wgpu-30.0.1 `src/backend/wgpu_core.rs:692`:
`log::error!("Handling wgpu errors as fatal by default"); panic!(…)`).
Installing the tracker therefore deliberately changes semantics from
panic-on-first-error to log-once-and-continue (the rerun-style behavior the
steal's text specifies). Recorded loudly in the code comment and the commit
message: any error that moves pixels is still caught by the byte-equal
gates; restoring fatality = deleting the install. DeviceLost is not routed
through this handler (wgpu surfaces it via `set_device_lost_callback`) —
untouched.

**Tested vs inspected:** the dedup logic is unit-tested (4 tests: first-vs-
repeat transitions, identity = kind+description, milestone sequence exactly
[10, 100, 1000, 10000], helper edges) — 33+1 tests green. A real GPU
validation error cannot be triggered on demand cheaply; the wgpu-facing
callback path is verified by code inspection against the wgpu 30.0.1 source
only. Inert when no error occurs — zero output change (byte-equal PNGs).

### O2 — debug labels (`1934346`, glyph_scene.rs / offscreen.rs / scene.rs / windowed.rs, +29/−12)

**Label audit result:** the inventory was nearly complete already (all 22
`create_*` call sites had `Some(label)`). Landed improvements:

- `scene.rs` (demo): prefix unified on the pipeline name — `scene bgl/bg/pl`
  → `quad field bgl/bg/pl`; `camera uniform` → `quad field camera uniform`
  (disambiguates from glyph_scene's `frame uniform`).
- `glyph_scene.rs`: `glyph bgl` → `glyph field bgl`, `glyph pl` →
  `glyph field pl`; per-chunk bind groups enumerate like the buffers they
  bind (`glyph bg i/N`; single chunk keeps `glyph bg`).
- Command encoders were the only unlabeled descriptors left
  (`Default::default()` ⇒ `label: None`): `windowed frame`, `windowed shot
  copy` (windowed.rs), `offscreen frame` (offscreen.rs — label-only, within
  the steal's explicit scope), `debug dump copy` (GLYPH_G_DUMP path).

**Already labeled, untouched:** glyph field/backdrop pipelines and passes,
group table, frame uniform, glyph params, backdrop instances,
`glyph instances i/N` arena chunks, atlas textures, depth, offscreen
target + readback, K6 windowed-shot readback, egui pass. (egui's internal
GPU resources carry egui-wgpu's own labels.)

Gates after each steal: `check-all.sh` ALL GATES GREEN (four PNGs
byte-equal — labels and the inert-when-quiet error handler don't touch
output); zero new deps, Cargo.lock untouched.

---

## Phase L3 as-executed (commits `a6a45f2` + `ec6e4aa`)

### Mechanism

re_renderer's ViewBuilder borrow, `GlyphScene` only (the demo `Scene` keeps
direct rendering — it is the minimal template by design):

- **`ViewTarget`**: ping-pong pair of `Rgba8UnormSrgb` textures + one
  `Depth32Float`, `RENDER_ATTACHMENT | COPY_SRC | TEXTURE_BINDING`, created
  and resized in `set_viewport` (GlyphScene now carries a `wgpu::Device`
  handle — the trait's `set_viewport` has no ctx param and its shape is
  fenced). Two textures, not a pool: re_renderer's `DynamicResourcePool`
  stayed reference material. Parity via `Cell<u8>`.
- **Pipeline retarget**: the glyph + backdrop pipelines now render into
  `POOL_FORMAT` (`Rgba8UnormSrgb`); the composite pipeline targets the
  DRIVER's format. `SCENE_SAMPLE_COUNT = 1` is a named const pinned at both
  pipeline creations, with an assert in `ViewTarget::new` — the copy
  composite is invalid on multisample; this assumption was silent before.
- **`FrameTarget` gained `color_texture` + `color_format`** (scene.rs): the
  copy needs the texture handle (a `TextureView` can't be a copy endpoint);
  the copy-vs-shader split keys on the format. The demo Scene ignores both.
- **Composite step** at the end of `GlyphScene::render`:
  - driver format == pool format (offscreen oracle) →
    `encoder.copy_texture_to_texture`, 1:1, bit-exact **by construction**
    plus a loud `assert_eq!` on the formats;
  - else (windowed, `Bgra8UnormSrgb`) → fullscreen shader composite through
    `composite.wgsl` (the ONE new shader fence 2 sanctions; tests/wgsl.rs's
    pinned shader set updated — that pin is the deliberate friction point).
    Fullscreen triangle from `vertex_index`, premultiplied passthrough,
    blend disabled; profiler query `"composite pass"` mirrors the Stage H
    scheme (offscreen profile output unchanged — the copy is encoder-level).
- **egui ordering untouched**: scene (pool + composite into the surface
  view) → egui pass (`LoadOp::Load`) → present. K6's capture still reads
  the composed frame.

### Copy-vs-shader rationale

The split exists for component-order copy incompatibility: the windowed
Metal surface is `Bgra8UnormSrgb`, and `copy_texture_to_texture` between
BGRA and RGBA is invalid. The oracle path must be unfailable, so it copies
(same format → bit-exact by construction); the windowed path is not
PNG-gated, so it pays one fullscreen pass. A future scaled/sub-rect
composite (minimap inset) extends the shader path only. The egui wrinkle is
recorded, not solved: `register_native_texture` demands `Rgba8Unorm`
(NON-sRGB) — an egui-hosted view would want its own non-sRGB pool or a
conversion pass.

### The one bug the gates caught mid-phase

The first L3 run diverged 100% of pixels to `[0,0,0,0]`: the copy
destination (offscreen target texture) lacked `COPY_DST`, so the composite
copy was a validation error and the frame never landed. Two notes worth
keeping: (1) gate 8's byte compare caught it instantly — the process worked
as designed; (2) O1's new handler logged the validation error cleanly
("first occurrence; repeats are counted") instead of panicking — the first
real-world exercise of the O1 semantic change, and a demonstration that the
byte-equal gates are the true net. Fix: `COPY_DST` added to the offscreen
target AND the windowed surface config (for non-Metal adapters whose surface
matches the pool format). After the fix: `text.png` byte-equal with the copy
path live — empirically confirming the by-construction claim.

### Acceptance evidence

- `check-all.sh` ALL GATES GREEN at both commits (8 gates; four PNGs
  byte-equal; 33+1 tests).
- **`--no-composite` A/B (captured before removal, `out/tooling-ab/stagel-l3/nocomposite/`)**:
  all four views byte-equal vs baseline; composite-on == composite-off
  (text.png) — the pool draw is neutral. Flag + parity-test entries + the
  direct path removed in `ec6e4aa`; the report records the removal here.
- **Cull counters** identical: `CULLDBG glyph draws=4 instances=10857 | backdrops=0`.
- **Verb smoke pair** byte-equal vs STEP 0 baselines (same log lines).
- **`GLYPH_G_DUMP`** readback works (slot 3 hex dump present).
- **Shader composite proven bit-exact WITHOUT a live display**: new dev-only
  hook `GLYPH_L3_SHADER_COMPOSITE=1` (AGENTS.md) makes the offscreen target
  `Bgra8UnormSrgb`, forcing `composite.wgsl` under the deterministic oracle
  driver; the swizzled readback is byte-equal vs baseline for BOTH text.png
  and repo-wide.png (the sRGB decode/encode round-trip is exact). The proof
  PNG at `out/tooling-ab/stagel-l3/composite.png` was opened and eyeballed:
  the fixture glyph field is fully correct (three text pages with
  white/green/yellow glyph rows, the small wide.txt page, group labels, the
  correct dark blue-gray background — no BGR swap, no blanket). NOTE: this
  PNG comes from the oracle driver, so there is no egui Debug window in it —
  offscreen never has one. Debug-window-over-composited-scene composition
  was proven live in K6's captures (`out/windowed-shot-*.png`); the L3
  windowed-path re-check is deferred to the human pass (below).
- `cargo tree -d`: 18 duplicates, unchanged — zero new deps; Cargo.lock
  untouched.

### Environment block + discovered issue (honest)

- **Live windowed verification was impossible this session**: the display
  is occluded (locked/asleep), and wgpu 30 reports
  `CurrentSurfaceTexture::Occluded`, so `render()` skips every frame — the
  app LOOKS hung (no FPS lines) but is spinning. Probe evidence: ~250,000
  occlusion-skips/second at pre-L3 HEAD too (8b17941) — **predates and is
  unrelated to L3**. The windowed ≥30-frame FPS band and the live K6
  composite.png eyeball are deferred to the human pass with the display
  awake. Expected cost: one extra fullscreen pass ≈ 0.1–0.3 ms.
- **Discovered issue (pre-existing, out of L3 scope, recorded):** on
  occlusion the event loop busy-spins `request_redraw` at ~100% CPU with no
  backoff. A future fix (skip `request_redraw` while acquires report
  Occluded) is its own small change.

### Human-pass additions

- With the display awake: windowed run on `fixtures/g-pick-repo` — FPS
  band should hold ~60.0 (one extra fullscreen pass), the Debug window
  draws over the composited scene, F2/`--screenshot-frame` captures still
  work, resize re-creates the pool (no crash, correct framing after).
- Deviations from the handoff: none beyond the two recorded above
  (COPY_DST addition; the environment block).
