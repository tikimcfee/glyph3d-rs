# Apple TBDR and the glyph AA footprint — 2026-10-24

**Question.** Why does the glyph pass on Apple-on-Metal compute its AA
footprint analytically, when every other adapter uses `fwidth`? **Answer.**
On the M2 a hardware derivative in that pass depends on GPU timing, so
`repo-back-oblique` was not byte-stable (board C35). Every depth-side fix
only lowered the rate. The analytic footprint removed the dependence and is
faster on the M2, but costs the 5090 frame time, so it ships on Metal only
(3b48be2).

This replaces the raw notes of a three-agent investigation the same day,
which were not kept. Numbers are measured on the M2 (`metal-apple`) unless
they say 5090 (`vulkan-nvidia`) or are marked **inferred**. "Flicker" is the
union of differing pixels over frames 1-10 of one 12-frame process at the
golden pose; "golden" is a fresh `--frames 2` process. Every experiment ran
beside a shipping control (62-81 px of flicker).

## The symptom

- **Cross-process.** Of 36 goldens (12 per field mode), the majority frame
  came 24 times; the rest were 1-2 px off at delta 1, at fixed spots
  (767,393), (821,396), (823,412). Every other golden view was byte-stable.
- **In one process, on a still camera.** 63-81 px of flicker, all delta 1.
- **Frame 0** sat about 880 px (max delta 138) from the frames after it, and
  the golden captures this early state. Which state a frame lands in follows
  wall-clock time, not frame count: at `--frames 12`, 10 renders gave 10
  distinct hashes.
- **The 5090.** 36 of 36 goldens identical, 0 in-process flicker, frame 0
  equal to frame 1.
- **Not a regression.** Present since the glyph pass began writing depth
  (1b86e68, 2026-09-07). Depth writes off, or compare `Always`, give 0
  flicker, and remove the z-order this frame exists to see.

## The mechanism, as measured

- **Stencil count and depth.** One passing fragment per pixel, every frame
  (400,203 px at 1, the rest at 0), at the flicker pixels too, while colour
  flickered 73 and 68 px; the glyph pass's real depth was byte-identical
  over frames 1-10. The winning set is fixed; no farther fragment blends in.
- **Constant alpha.** A per-pixel colour at alpha 0.5, same discards and
  depth state: 0, 0, 0. Alpha set to the shipped coverage: 65 and 66. The
  blend is deterministic; the winner's coverage varies.
- **Single-input probes.** An output driven only by `fwidth(glyph_uv)`
  flickered 6,094-6,679 px; driven by `glyph_uv`, `clip.z` or `clip.w`, 0.
- **Quad-mates.** 93% of flicker pixels share their 2x2 quad with a pixel
  won by a different, nearer instance, against 31% of all covered pixels.

The reading **(inferred from the above)**: this view draws front to back,
so the occluder at the quad-mate pixel was drawn earlier. If its depth has
landed when the later quad is tested, that quad-mate lane is killed before
shading; if not, it runs and is rejected late. The winner's finite-difference
`fwidth` differs between the two, and coverage moves one level. Metal does
not document whether helper lanes survive a depth reject. The converse was
measured: the analytic footprint reads no quad-mate and gives 0 flicker with
early-Z still on. It also removed the frame-0 early state, which no
depth-side fix did, so that state is the same derivative channel.

**Why the 5090 is unaffected (inferred).** An immediate-mode GPU runs each
sample's depth test and write in primitive order (Vulkan's rasterization
order), so a quad-mate lane's fate does not depend on timing.

## Tried and refuted (do not retry without new evidence)

- **Depth precision and near-ties.** The competing fragments are about 1%
  apart in depth, about 2×10^5 f32 ulps. No pixel has two passing
  fragments, so a tie-break or depth offset has nothing to separate (the
  offset itself was not run).
- **Depth format, compare function, `@invariant`.** On the M2 wgpu maps
  Depth24Plus to Depth32Float, so there is no format to change. `Greater`
  for `GreaterEqual`: 72 and 68 px. `@invariant`: 72 and 63 px (and 1,702 px
  moved). `fwidthFine` and `fwidthCoarse`: 68-76 px.
- **Draw and pass splitting.** The race is inside one draw: `long.md`'s draw
  alone flickered 34 px. 21 sub-draws in one pass gave 74 and 64 px, five
  passes 26 and 32, seven passes 49 and 36. Every pass still blended and
  wrote depth.
- **`frag_depth = clip.z` (forces late-Z).** Steady flicker went to 0, but
  about 3% of goldens still flipped (4 of 118 and 3 of 126, two independent
  samples), an `fwidth` probe still flickered 5 and 21 px, and the frame-0
  state survived. Oblique frame time rose 60-65% (6.5 → 10.7 ms), though
  `repo-wide` and `repo-down` got 28-30% faster.
- **A separate depth prepass** (depth-only pass, then a colour pass with
  depth Load and writes off). Steady state was clean, but goldens still
  flipped (2 of 13 and 2 of 20) and the probe wobbled 91-413 px in 7-8 of 39
  frames, because the colour pass still depth-rejects quad-mate lanes.
  Frame time rose 52-53% at every pose.
- **Coverage-threshold discards.** Discarding fringe fragments under 1/255,
  2/255 or 8/255 of coverage left 69-81 px. The edits were verified to land
  (1,947 and 24,345 px moved).
- **Also no help.** Hoisting `fwidth` above every `discard` left frame 0
  895 px off. `[[early_fragment_tests]]`, conservative depth and raster
  order groups cannot be reached through wgpu on Metal.

## Lessons: two retractions and an instrumentation bug

- **"Near-zero-coverage fringe fragments race"**: retracted by the threshold
  discards above.
- **"Discard is the cause"**: confounded. Replacing every `discard` with
  `return vec4(0.0)` gave 0 flicker, but made every empty quad area write
  depth (234k px changed), hiding the overlap zone. Late-Z and the analytic
  footprint both reach 0 with every discard kept.
- **The first depth readback read an all-zero texture**: the offscreen
  target's depth, not the glyph pass's own (`glyph_scene/render.rs` binds
  `depth_view: _`). "Depth identical across frames" was vacuous until
  re-measured on the real texture, where it held. A measurement must show
  that it saw something nonzero.

## The decision: analytic, Metal only

**On the M2** the analytic footprint was deterministic in every sample (56
of 56 goldens; 3 × 41 in-process frames, frame 0 included; after the fix, 10
fresh renders per mode, byte-equal across modes), and faster: the prototype
cut frame time 10-20% (oblique 6.5 → 5.8 ms, `repo-wide` 4.0 → 3.2 ms), and
as shipped oblique went 156 → 178 fps and `repo-wide` 250 → 316.

**On the 5090** it lost 10-17% of fps (oblique 1,708 → 1,418, `repo-wide`
2,562 → 2,295, `repo-down` 2,898 → 2,594), and would have moved up to 40,468
px per view: a vulkan-nvidia re-adoption on a machine without the bug.

**Ivan's constraint: don't make both machines pay for one machine's bug.**
So `GpuProfile::glyph_footprint` picks `Analytic` on Metal with an Apple
vendor (an Intel or AMD Mac is immediate-mode and stays on hardware), once,
at pipeline creation. A `Params` flag or an override constant would change
what the 5090 compiles or runs. `crates/glyph-field/src/footprint.rs`
derives the analytic form from each glyph shader by anchored substitution:
three flat varyings carry the quad's projective map in pixels (from
`FrameUniform.viewport`), and the fragment inverts its 2x2 Jacobian for `fw`
and for the emoji sheet's `textureSampleGrad` (implicit gradients are the
same channel). Every other adapter compiles the source byte for byte: one
coverage body, two derivative sources.

The analytic footprint everywhere (one formula, no host-scoped mutation,
cover for future TBDR keys) was rejected on the 5090's measured cost. What
Metal-only pays: `drift` against vulkan-nvidia also carries a formula
difference now.

## What it moved

On metal-apple, against pre-fix renders on the same host, all at delta 1:
`text` 255, `repo-wide` 5,362, `repo-down` 35,598, `repo-zoom` 1, `emoji`
32, `emoji-cluster` 57, `repo-cluster` 39, `repo-highlight` 8, `demo` 0.
`repo-back-oblique` moved 15,357 px, 73 of them at ≥ 16 (max 138).
vulkan-nvidia did not move.

The two footprints agree to the probe's resolution (1.000 ± 0.006) at
400,200 of 400,202 covered pixels. The large deltas are two hard thresholds
that a 0.1% footprint change crosses. The 43-45 pixels at delta ≥ 64 were
traced and each falls in one of them; the 73 at ≥ 16 are attributed to the
same two.
- **Greeking's hard cut** (`em_px <= greek_onset_px`, 10 px). Its isoline
  crosses the receding column at y = 803, where `em_px` reads 10.006 under
  `fwidth` and 9.994 analytically: 11 px flip from coverage 0.06-0.24 to a
  full greek bar.
- **The zero-coverage fringe.** A near fragment at exactly 0 coverage is
  discarded and writes no depth; just above 0 it writes depth and hides the
  farther glyph's ink. The rivals are adjacent wrap segments, 0.13% apart.

**Inferred:** the analytic value is the more correct one, the exact
derivative at the pixel centre where the hardware differences the quad.
metal-apple was re-adopted with step 2 and C33 (83079e8) after Ivan looked
at the frames.

## How it is witnessed, and what is not seen

- **`native/tests/oblique_repeat.rs`** renders the pose 12 frames in one
  process (`GLYPH_REPEAT_CHECK=1`) in all three field modes, and requires
  every frame equal to frame 0 and over 300k ink pixels. On the hardware
  footprint on the M2, 11 of 12 frames differ (877 px, max delta 138).
- **Mutations.** `footprint-selection-dropped` (`gpu = "metal-apple"`)
  builds the field on `Hardware`; oblique_repeat reddens.
  `analytic-footprint-keeps-fwidth` (unscoped) leaves `fwidth` in the
  derived source; the anchor and naga tests redden on every host.
- **prove's `gpu` field** (94476a4). A scoped mutation is `N/A` on other
  keys, neither proven nor failed; `validate` refuses a key with no golden
  set; a run whose every selected mutation is N/A fails as `NOTHING PROVEN`.
  Without it, prove on the 5090 would report the mutation surviving.

Unseen:
- **One rasterizer per form.** The analytic form reaches pixels only on
  metal-apple; on the 5090 only naga tests hold it, and oblique_repeat there
  runs the hardware shader (it had not been run there when the fix landed).
- **Two outlier pixels**, (931,488) and (1134,812), read a hardware/analytic
  ratio of 0.14 on both axes. Untraced.
- **Emoji at an occlusion edge** are in no golden, so the emoji
  `textureSampleGrad` has no pixel witness.
- **What switches the M2** between early and steady state is untraced
  (inferred: clock ramp, or quad packing under load). Any hardware
  derivative added back to this pass would bring it back.
- **Other TBDR GPUs** (Mali, Adreno, PowerVR) have never been measured.
