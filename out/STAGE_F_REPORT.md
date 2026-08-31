# Stage F — interactivity: fly camera + per-file frustum/LOD culling

**Goal**: make the 95M-glyph repo field navigable at interactive rates.
Stage E2 left the full-field view at 1.1 fps — vertex-bound at 571M vertex
invocations per frame, no culling, no LOD.

**Result**: full-field view runs at **60–222 fps** (vs 1.0–1.1), mid-range at
**104 fps**, single-file zoom at **126 fps** — with zoomed renders
**pixel-identical** to Stage E2 and `--repo-verify` still passing bit-exact.
The windowed mode gets a real fly camera (WASD + E|R/Q|F + mouse-look +
scroll speed).

All numbers on glyph3d-js (1,305 files / 96.9 MB / 95,181,245 glyph
instances, 2 arena chunks), release build, Apple M2, 1600×1000.

## fps, before → after (offscreen, fixed camera, GPU-completed steady state)

| view | camera | before | after | drawn after cull |
|---|---|---|---|---|
| full field | fit all 15,098×22,683 | **1.0 fps** | **77 fps** (120 frames) / **222 fps** (600 frames, warm) | 0 glyph instances, 1,305 backdrop quads |
| mid-range (~13 files) | `--focus-file liveTrie.js --zoom 0.2` | **1.0 fps** | **104 fps** (300 frames) | 233,975 instances in 13 range draws |
| single-file zoom | `--focus-file liveTrie.js` | **1.0 fps** | **126 fps** (300 frames) | 22,708 instances in 3 range draws |

The before column is flat 1.0 fps because every view drew all 95M instances.
The 120-frame full-field figure includes the first frames after the 4.3 GB
arena upload (paging/drain); the warm 600-frame figure is the true steady
state. Windowed smoke run: 94–120 fps sustained at the full-field fly start
pose (FIFO present), no errors.

## Cull design (and why it is NOT the GPU-indirect design)

Two-level scheme over the E2 data: instances are appended per file, so each
file is a contiguous arena range — one **segment** per file (`SegCull`, 48 B:
world AABB incl. group offset, slot base/count, backdrop tint). 1,305
segments = the whole cull problem is kilobyte-scale.

Per frame (`cull_segments`, glyph_scene.rs):

1. **Frustum**: 6-plane positive-vertex AABB test. Planes are extracted
   CPU-side from the view-proj (Gribb-Hartmann; the near plane is row2
   because the glam GL-style projection is clipped by wgpu at z_ndc ≥ 0).
   Lossless by construction — the AABB conservatively contains every
   instance (z margin ±1).
2. **LOD** (below): visible-but-deep-subpixel segments drop to backdrops.
3. **Draws**: one vertex-range draw per visible segment per chunk
   (`pass.draw(0..6, base..base+count)` — instance_index stays chunk-local,
   the glyph shader is untouched). Ranges ascend in arena order per chunk,
   so within-pixel blend order matches the legacy full draws exactly.
   Backdrops are compacted into a 41 KB buffer (`write_buffer`) and drawn as
   ONE instanced draw through a tiny flat-quad pipeline (cull.wgsl).

**Why CPU, not the standard GPU compute → indirect pattern**: that design
was built first (cull compute pass writing `DrawIndirect` args, per-chunk
`multi_draw_indirect`, atomic-appended backdrop list + `draw_indirect`).
It is empirically **broken in wgpu 30.0.1's Metal backend**: any indirect
draw with `first_instance != 0` rasterizes nothing. Evidence:
`GLYPH_CULL_DEBUG=1` readbacks showed byte-correct args on GPU; draws with
`first_instance = 0` rendered (chunk-0 files at arena start), draws with any
nonzero `first_instance` produced empty frames regardless of chunk, buffer
offset (per-chunk buffers at offset 0 were tried), or call order — all
through wgpu's indirect-validation batcher (`MULTI_DRAW_INDIRECT_COUNT` is
also false on this adapter). 1,305 CPU AABB tests cost ~microseconds, are
fully deterministic (fixed camera → identical draws), and need no indirect
machinery. The compute path can be revisited after a wgpu upgrade; the
shader-side backdrop stream is unchanged by the decision.

## LOD threshold math

Per segment: `glyph_px = px_scale / dist`, where
`px_scale = viewport_h / (2·tan(fov/2))` (= 1,373 at 1000 px, fov 40°) and
`dist` = distance from the eye to the AABB's **nearest** point —
conservative: a segment only substitutes when even its closest glyphs are
below the threshold, so a huge file near the camera is still drawn
glyph-by-glyph (documented tradeoff: at oblique full-field-adjacent views
one monster file can contribute millions of instances).

Threshold **LOD_MIN_PX = 1.0 px/em** ("truly subpixel"): below 1 px per em
cell, a glyph quad covers no pixel center reliably and Slug renders it as a
~2 px dilated blob (dilate 0.75 px ≫ quad size); the only visually readable
content at that scale is the file-scale haze, which the backdrop reproduces.
Measured glyph_px per benchmark view: full field 0.041 px (all 1,305 files
→ backdrops), mid 2.0 px (glyphs drawn), zoom 10 px (glyphs drawn).

**Backdrop color**: mean LINEAR ink color over the file's instances (sRGB
bytes pow-2.2 decoded at staging), alpha = effective coverage
`E = ink_frac × BACKDROP_GAIN`, clamped to 1, where
`ink_frac = slot_count × cell_area / (width × height)`
(cell area = 0.53 advance × 1.25 pitch ≈ 0.66 world²). BACKDROP_GAIN was
fitted against the Stage E2 full-field render's mean linear pixel value:
1.9 → +45% too bright, 1.3 → +29%, 1.0 → +17%, **0.7 → within ~5%**
(0.092/0.090/0.111 vs 0.087/0.086/0.107). Residual difference: one flat quad
per file fills intra-file page gaps the real render leaves dark — the wide
shot reads the same at a glance (shelf rows, directory tints, the two
monster-file fans) but is flatter per file. Whole-file substitution only;
no content subsampling anywhere.

## Fly camera (windowed)

`CameraMode::Fly` (default for windowed glyph scenes): WASD forward/strafe,
E|R up, Q|F down, mouse-look via pointer grab (click to grab — Confined +
hidden cursor, raw DeviceEvent deltas; Esc releases), scroll wheel =
persistent speed multiplier (×1.15 per notch, clamped to [0.005, 8]× the fit
distance), exponential velocity damping (~63% of target per 100 ms, dt
capped at 100 ms). Starts at the Front fit pose. Input flows through new
default-no-op `SceneLike` hooks (`on_key` / `on_mouse_look` / `on_scroll` /
`tick`); the Stage A demo scene ignores them. Smoke run: 14 s windowed, FPS
lines healthy, no panics. Human-driven mouse/keys can't be exercised in this
environment — the input path is code-reviewed and compiled, tick runs every
frame.

## Verification

- `cargo build --release` — clean, 0 warnings (final binary).
- `--repo-verify` — **PASS**: 96,860,762 records bit-exact naive vs batch
  (no Mojo/engine-local changes this stage).
- Zoom view (`--focus-file liveTrie.js`): **pixel-identical** to
  `out/e2-file-zoom.png` (`cmp` byte-equal) — nothing visible culls, LOD
  never engages at 10 px/em. Re-checked on the final binary.
- Mid view (`--zoom 0.2`): **pixel-identical** to the pre-culling render of
  the same camera (`out/f-mid-before.png`) — frustum culling alone, exactly
  lossless.
- Full-field with backdrops (`out/f-field-after.png`): viewed; shelf
  structure/tints match `out/f-field-before.png` at a glance, mean linear
  brightness within ~5%.
- Deterministic offscreen mode: fixed camera → identical cull decisions →
  identical output; `--no-cull` reproduces the legacy path bit-for-bit.

## Memory

Segment table 63 KB + backdrop buffer 42 KB + per-frame Vec allocs — no
instance-sized buffers added (arena still 4,357 MiB in 2 chunks).

## Remaining gaps

1. **Picking / navigation** — the FileView/SegCull tables have everything a
   click-to-focus needs; no hit-testing yet (E2 gap, unchanged).
2. **Per-instance editing / live atlas growth** — unchanged from E1/E2.
3. **Emoji atlas** — bitmap slots still discard (unchanged).
4. **Sub-file LOD granularity** — the LOD decision is whole-file; a monster
   file near the camera draws all its glyphs. Page-level segments (one per
   128-row page) are the natural refinement; FileView already knows page
   bounds.
5. **GPU-indirect culling** — revisit after a wgpu upgrade (the Metal
   first_instance bug blocked it this stage); per-instance culling/LOD
   rejection in the vertex shader would follow the same fix.
6. **Backdrop fidelity** — one quad per file flattens intra-file page
   structure; a 2×2 or per-page tint grid would close most of the residual
   ~5% brightness/texture gap.

## File diffs

- `native/src/glyph_scene.rs` — SegCull + seg_tint + GLYPH_CELL_AREA,
  LOD_MIN_PX / BACKDROP_GAIN, frustum_planes, cull_segments (CPU frustum+LOD),
  CullState (backdrop pipeline + buffers), FlyCamera, CameraMode::Fly,
  camera_frame, render() split into culled range draws / legacy draws,
  GLYPH_CULL_DEBUG counter dump.
- `native/src/shaders/cull.wgsl` — NEW: backdrop quad pipeline + the
  cull/LOD contract header (the compute cull shader was written, debugged,
  and removed after the wgpu/Metal indirect bug; the header records why).
- `native/src/shaders/glyph_field.wgsl` — header note only (shader code
  untouched; draws are now per-segment ranges).
- `native/src/windowed.rs` — pointer grab + raw mouse deltas, key/scroll
  plumbing, per-frame tick, Fly camera default, help line.
- `native/src/scene.rs` — SceneLike input/tick hooks; render() takes
  physical width/height (LOD needs viewport pixels).
- `native/src/main.rs` — `--no-cull`, cull flag plumbing, help text.
- `native/src/offscreen.rs` — cull flag; render() size args.
- `native/src/gpu.rs` — MULTI_DRAW_INDIRECT_COUNT probe log (multi-draw is
  core in wgpu 30; no features requested).
- `native/src/text.rs` — StagedText.segments + single cover segment for
  text/engine scenes.
- `native/src/repo.rs` — per-file SegCull built in into_staged (world AABB
  from FileView offset/width/height, backdrop tint from the file's own ink).
