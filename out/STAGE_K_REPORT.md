# Stage K — GPU-driven culling: compute cull → indirect draws (the Stage F design, unblocked)

**Goal**: make the standard GPU-driven pairing — a compute pass that tests every
segment's bounding box against the frustum/LOD and writes only the visible work
into draw-argument buffers, consumed by `multi_draw_indirect` — work on the M2,
where Stage F had found that "wgpu 30.0.1's Metal backend silently rasterizes
nothing for any indirect draw with `first_instance != 0`" and fell back to a CPU
cull with direct range draws (glyph_scene.rs header, cull.wgsl header,
out/STAGE_F_REPORT.md).

**Result**: it was never a Metal bug. The device had never requested
`Features::INDIRECT_FIRST_INSTANCE`, and WebGPU's *indirect-first-instance* rule —
enforced by wgpu's own indirect-validation compute pass, which rewrites offending
draws to zero instances in **its** copy of the args — is exactly "readbacks show
correct args, nothing draws". One feature bit unblocks the whole design. Two
GPU-driven forms now ship, both producing draw lists that are **exactly** the CPU
cull's (verified per frame by readback) and rendering **byte-identical** PNGs on
every gate view and on the 95M-glyph corpus:

- `--cull gpu` — the multi-draw form: one `DrawIndirectArgs` per (segment,
  chunk) slot, one `multi_draw_indirect` per chunk. The design as specified.
- `--cull gpu-packed` — the Metal-scaling form: the visible slots packed with an
  instance prefix sum, ONE `draw_indirect` per chunk, a vertex-shader remap.
  Encode and validation cost O(chunks) instead of O(segments).

`--cull auto` (the default) now selects the multi-draw form wherever the adapter
has `INDIRECT_FIRST_INSTANCE`: on the corpus it matches or beats the CPU path
on every view (wide 314 vs 226 fps, mid 162 vs 159, zoom 229 vs 216 — medians
of three 900-frame runs), and it is the design as specified. The packed form is
the documented scaling alternative: fastest on the all-backdrop view (361 fps),
but its vertex-shader remap costs ~12% on the glyph-heavy mid view. The GPU
cull itself costs 25–58 µs of GPU time per frame with zero CPU cull work. The
CPU path stays (`--cull cpu`) and remains the reference twin.

All numbers on Apple M2 (16 GB), macOS 26.6 / Xcode 26.6, wgpu 30.0.1 (Metal),
release build, 1600×1000 offscreen.

## Commits

| hash | part |
|---|---|
| `061d737` | `mdi-probe` — the proof tool: 3 device configs × 9 indirect-draw forms, readback-graded |
| `589290c` | GPU cull → `multi_draw_indirect` path (`--cull`), `--cull-verify`, gate 7, bench script |
| `f043b64` | packed form (`--cull gpu-packed`, cs_pack + vs_packed), profiler back-pressure, encode-cost scopes at `encoder.finish()`, the ≥4 GiB chunk clamp (§5b), measurement levers, gate 7 covers both forms |
| `b172e9e` | this report (first version) |
| (commit 5) | the fair fight: CPU-path frame memo + uniform-write memo levers (§4.3), report update |

## 1. The diagnosis — `mdi-probe`

wgpu-core, `src/indirect_validation/validate_draw.wgsl` line 67:

```wgsl
if !supports_indirect_first_instance {
    failed |= first_instance != 0u;
}
```

`supports_indirect_first_instance` is `required_features.contains(INDIRECT_FIRST_INSTANCE)`
(draw.rs:126). When it fails, the validation dispatch writes a zeroed
`DrawIndirectArgs` into wgpu's internal destination buffer and the draw reads
*that* — the application's buffer is untouched, which is why Stage F's
`GLYPH_CULL_DEBUG` readbacks looked right. `InstanceFlags::default()` keeps
`VALIDATION_INDIRECT_CALL` on even in release builds. gpu.rs requested
`Features::empty()`; wgpu-hal's Metal adapter sets `INDIRECT_FIRST_INSTANCE`
whenever indirect draws are supported (adapter.rs:1202) — the M2 has it.

`src/bin/mdi_probe.rs` (`cargo run --release --bin mdi-probe`, also run by
`cargo test` via tests/mdi_probe.rs) pins this empirically: a 64-column
Rgba32Uint target where each instance paints its own column with a tag looked
up by `instance_index` from a storage buffer (the glyph field's access pattern),
read back and decoded into the exact SET of instance ids that rasterized.

```
adapter: Apple M2 (Metal) | adapter features: INDIRECT_FIRST_INSTANCE=true MULTI_DRAW_INDIRECT_COUNT=false
  A: features=EMPTY                   validation=on  (the Stage F-J device)
  B: features=INDIRECT_FIRST_INSTANCE validation=on  (the fix)
  C: features=EMPTY                   validation=OFF (informational; wgpu: UB)

case                                                        | A observed                     | B observed                     | C observed [info]
direct draw(0..6, 5..10) [control]                          | {5..10} ok                     | {5..10} ok                     | {5..10}
draw_indirect first_instance=0                              | {0..5} ok                      | {0..5} ok                      | {0..5}
draw_indirect first_instance=7                              | ∅ ok                           | {7..12} ok                     | {7..12}
draw_indexed_indirect first_instance=9                      | ∅ ok                           | {9..12} ok                     | {9..12}
multi_draw_indirect x2 [fi=0 | fi=50]                       | {0..2} ok                      | {0..2, 50..52} ok              | {0..2, 50..52}
multi_draw_indirect x4 (one zero-instance pad slot)         | ∅ ok                           | {20..23, 30..32, 40..44} ok    | {20..23, 30..32, 40..44}
compute cull -> multi_draw_indirect(8), view [2,5)          | ∅ ok                           | {16..20, 24..28, 32..36} ok    | {16..20, 24..28, 32..36}
compute cull -> multi_draw_indirect(8), view [0,1.5)        | {0..4} ok                      | {0..4, 8..12} ok               | {0..4, 8..12}
draw_indirect first_instance=1_500_000 (chunk-scale offset) | ∅ ok                           | {1500000..1500003} ok          | {1500000..1500003}

args [A] compute cull -> multi_draw_indirect(8), view [2,5): [vc=6 ic=0 fv=0 fi=0] [vc=6 ic=0 fv=0 fi=8] [vc=6 ic=4 fv=0 fi=16] [vc=6 ic=4 fv=0 fi=24] [vc=6 ic=4 fv=0 fi=32] ...
RESULT: PASS (18/18 graded expectations met; A = spec no-op without the feature, B = draws land with it)
```

Column A is Stage F's world, verbatim: every form with a nonzero
`first_instance` paints nothing while the compute-written args read back
correct (`ic=4 fi=16 …`). Column B is the fix: plain, indexed, multi-draw with
zero-instance padding, compute-cull-driven multi-draw, and a 1.5-million
`first_instance` all land exactly. Column C shows Metal honoring base instance
natively once wgpu's validation is out of the way (documented as UB by wgpu —
not used). The A-column expectations are graded too: the tool's exit code
asserts the *diagnosis*, not just the fix.

Upstream context: gfx-rs/wgpu#2148 (open since 2021) confirms Metal
`multi_draw_indirect` is a CPU loop of single indirect draws and that the
`_count` variant is unimplemented (no draw-count concept in Metal; Indirect
Command Buffers would be needed), with zeroed-out draw slots as the suggested
pattern; wgpu#6823 (a `draw_id` builtin) is not needed here — every slot
carries its own `first_instance`, and the packed form carries none.
julpo2000/cubara#35 records the same "don't wait for wgpu" conclusion for
another project.

## 2. The design (`shaders/cull_gpu.wgsl`, `GpuCull` in glyph_scene.rs)

Inputs: the Stage F segment table (`SegCull`, 48 B, one per file — now with a
`flags` lane carrying `SEG_HIDDEN`, so the table is the single source of truth
for both cull paths) uploaded once, plus a 384 B `CullParams` uniform per frame
(the six `frustum_planes`, eye, `px_scale`, `LOD_MIN_PX`, counts, the per-chunk
slot ranges). One compute pass per frame:

1. **`cs_classify`** — one thread per segment → `seg_mode[s]` ∈ {culled,
   glyphs, backdrop}. The same tests, in the same operand order, as
   `cull_segments`: 6-plane positive-vertex AABB test with z ∈ [−1, 1], then
   `glyph_px = px_scale / max(dist(eye, nearest AABB point), 0.001)` against
   `lod_min_px`.
2. Either **`cs_draw_args`** (`gpu`) — one thread per **draw slot** →
   `DrawIndirectArgs`. The slot table is static, built once on the CPU exactly
   the way `cull_segments` walks segments: per arena chunk, in segment order,
   one slot per (segment, chunk) intersection with the chunk-local instance
   range (1,305 segments → 1,306 slots over the corpus's 2 chunks). A visible
   glyph segment's slot becomes `draw(0..6, base..base+count)` —
   `first_instance = base`, chunk-local, so `vs_main` is untouched — and every
   other slot becomes a zero-instance draw; `render()` issues **one
   `multi_draw_indirect` per chunk**.
   Or **`cs_pack`** (`gpu-packed`) — one workgroup; per chunk, a stable scan
   over that chunk's slots compacts the visible ones into `packed_slots` =
   (exclusive instance prefix, chunk-local base) and writes ONE
   `DrawIndirectArgs {6, Σ visible counts, 0, 0}` per chunk plus
   `packed_meta[c]` = (first packed slot, visible count). `render()` issues
   **one `draw_indirect` per chunk** through `vs_packed`, which binary-searches
   the chunk's prefix table (`packed_slots[first + k].prefix ≤ instance_index`)
   and hands `base + (instance_index − prefix)` to the shared `glyph_vertex`
   body. Dense instances, no padding, no `first_instance` at all.
3. **`cs_backdrop_compact`** — one workgroup, a stable stream compaction of
   the backdrop-mode segments into the `BackdropInst` list (the same buffer
   `cull.wgsl`'s `vs_backdrop` reads) plus a `DrawIndirectArgs {6, count, 0,
   0}` for a `draw_indirect`. A workgroup-wide inclusive scan per 256-segment
   block keeps **segment order** — an atomic append would not, and backdrop
   quads of moved files can overlap, so order is part of the output.

Nothing is read back. The CPU writes two small uniforms per frame and encodes
three dispatches plus `chunks + 1` indirect draws; the GPU decides visibility.
Slot order == packed order == the CPU path's draw order == ascending arena
order per chunk, so within-pixel blending is unchanged in every form.

**Why fixed slots + zero padding for `gpu`, and why `gpu-packed` exists.**
Metal has no draw-count-buffer concept; wgpu-hal's `draw_indirect_count` is
literally `//TODO` on Metal and the adapter reports
`MULTI_DRAW_INDIRECT_COUNT=false`. wgpu emulates `multi_draw_indirect(n)` as
`n` single `drawPrimitives:indirectBuffer:` calls, and its indirect-validation
pass validates every one of those `n` draws per submit (a metadata staging
buffer, a blit, a compute dispatch — per frame). A zero-instance slot costs no
GPU work, so the multi-draw form's GPU cost is O(visible) but its encode +
validation cost is O(segments). The packed form moves the O(segments) work into
the compute pass (a scan) and leaves O(chunks) indirect draws for wgpu and
Metal to see — which is what makes it scale on this backend (§4). On
Vulkan/DX12 the multi-draw form's buffers would feed a native multi-draw
(+count); the packed form is backend-agnostic.

**Edits** (Stage G verbs): `sync_segment` re-derives the row from the group TRS
as before and now mirrors the 48 B row into the GPU table; `hide-group`
sets/clears `SEG_HIDDEN` in the same row. `cull_segments` reads the flag
instead of a parallel `hidden: Vec<bool>`, so the paths cannot disagree
about what is hidden.

## 3. Verification tooling

- **`--cull-verify`** (offscreen): after the frames, reads the compute outputs
  back — the slot args, or the packed (prefix, base) table + per-chunk meta +
  instance totals — plus the backdrop args and list, and diffs them against
  `cull_segments` for the same camera frame. Same slots, same ranges, same
  order, same backdrop bytes. Exit 1 on any divergence, first difference
  printed.
- **`tools/check-mdi.sh`** = **gate 7** in `tools/check-all.sh`: the probe
  matrix; per view (single cover segment / full field / all-glyph / partial
  frustum / all-backdrop LOD / `hide-group` / `move-group`) both GPU forms
  `--cull-verify` EXACT and byte-equal to the `--cull cpu` render; then
  `GLYPH_CHUNK_CAP` (new debug lever) forces the fixture arena into 12 and 3
  chunks so the (segment, chunk) slot split — and cs_pack's per-chunk prefix
  reset — are exercised, byte-equal to each other and to the single-chunk
  render. Outputs are deleted before every render so a stale PNG can never
  pass.
- **`tools/bench-mdi.sh`**: the three Stage F corpus views, all paths, with
  `--cull-verify` and `cmp`, profiled (`PROFILE=1`) or clean (`PROFILE=0`).
- `cargo test`: encase layout assertions for `SegCull`/`DrawSlot`/`CullParams`
  (offsets pinned against cull_gpu.wgsl), the CLI resolution test, naga
  validation of `cull_gpu.wgsl` (its uniformity analysis covers the scans'
  barriers — the per-chunk slot ranges live in the uniform for exactly that
  reason) and of the two-entry-point `glyph_field.wgsl`, and the probe matrix.
  22 tests.
- Windowed smoke: both GPU forms at a vsync-locked 60 fps on the fixture repo
  with a startup `hide-group` verb through the segment-table path.

Gate 7 on the fixtures (every line PASS; 37 checks):

```
text       [gpu] [gpu-packed]  verify EXACT — 1 slot, backdrops 0                      BYTE-EQUAL vs cpu
repo-wide  [gpu] [gpu-packed]  verify EXACT — 4 visible of 4 slots                      BYTE-EQUAL
repo-zoom  [gpu] [gpu-packed]  verify EXACT — 1 visible of 4 (3 frustum-culled)         BYTE-EQUAL
repo-mid   [gpu] [gpu-packed]  verify EXACT — 1 visible of 4                            BYTE-EQUAL
repo-far   [gpu] [gpu-packed]  verify EXACT — 0 glyph draws, backdrops 4 (bytes equal)  BYTE-EQUAL  (LOD tier)
repo-hide  [gpu] [gpu-packed]  verify EXACT — 3 visible (hide-group verb)               BYTE-EQUAL
repo-move  [gpu] [gpu-packed]  verify EXACT — 4 visible (move-group verb)               BYTE-EQUAL
cap=977    [gpu] [gpu-packed]  12 chunks, 15 slots, verify EXACT; vs cpu vs single-chunk BYTE-EQUAL
cap=4096   [gpu] [gpu-packed]   3 chunks,  6 slots, verify EXACT; vs cpu vs single-chunk BYTE-EQUAL
```

And the four shipped baselines (`out/tooling-ab/baseline/`) stay **BYTE-EQUAL**
under the GPU default (gate 6).

## 4. Corpus results (glyph3d-js: 1,305 files / 95,197,527 glyph instances / 2 arena chunks / 1,306 draw slots)

Every corpus run below also carried `--cull-verify` (EXACT on every view, every
form — 1,305-entry backdrop compaction included) and byte-equal `cmp` against
the CPU render.

### 4.1 Steady-state fps — 900 frames, three interleaved repetitions, medians (unprofiled)

`--cull` path (columns) × view (rows); "no val." = `WGPU_VALIDATION_INDIRECT_CALL=0`.
All three repetitions listed after the median in parentheses; environmental
outliers (the machine has 16 GB and the arena is 4.4 GB) are left in.

| view | cpu | gpu (multi-draw) | gpu, no val. | gpu-packed | gpu-packed, no val. |
|---|---|---|---|---|---|
| wide — 1,305 backdrops, 0 glyph draws | **226** (226/299/205) | **314** (349/312/314) | 348 (435/348/327) | **361** (345/443/361) | 355 (259/466/355) |
| mid — 13 files, 240,716 glyphs | **159** (163/159/152) | **162** (165/160/162) | 159 (161/157/159) | **143** (141/143/150) | 147 (147/150/139) |
| zoom — 3 draws, 22,682 glyphs | **216** (227/216/200) | **229** (256/229/197) | 217 (251/208/217) | **239** (242/239/222) | 97 (244/27/97 — two outliers) |

Reading it:

- **Wide** (the view that drives the "GPU-driven" case: every one of the
  1,306 slots is empty, the cull's whole job is to say so) — every GPU form is
  40–60% faster than the CPU path, whose per-frame cost here is the 41 KB
  backdrop upload plus the cull. The packed form is the fastest with
  validation on because wgpu validates 3 indirect draws instead of 1,307.
- **Mid** (fragment-bound: ~4–6 ms of Slug coverage per frame) — the cull
  path is irrelevant to within noise for the multi-draw form; the packed
  form's `vs_packed` binary search (four dependent loads per vertex for 13
  visible slots, on 1.44 M vertex invocations) shows up as −12%.
- **Zoom** — multi-draw and packed both edge the CPU path; validation
  on/off is inside the noise.
- Validation off buys ~10% only on the multi-draw wide view (1,306 draws to
  validate per frame); nowhere else is it measurable at this scale.

The earlier short runs (300–600 frames, single repetitions) had shown the GPU
forms *slower* by 10–40%; those were warm-up dominated (arena page-in, first
pipeline use) and noisy — the 25/27/97 fps outliers above are of the same
kind. Nine hundred frames × three interleaved repetitions is the minimum that
gave stable medians on this machine.

### 4.2 Per-frame costs — `GLYPH_PROFILE=1`, 300 frames (validation on)

| view | path | GPU: cull pass | GPU: glyph pass† | CPU: cull | CPU: encoder finish | CPU: queue submit | fps‡ |
|---|---|---|---|---|---|---|---|
| wide | cpu | — | 0.084 ms | 0.032 ms | 0.055 ms | 0.014 ms | 101 |
| wide | gpu | 0.025 ms | 0.091 ms | — | 0.234 ms | 0.023 ms | 145 |
| wide | gpu-packed | 0.045 ms | 0.069 ms | — | 0.119 ms | 0.018 ms | 136 |
| mid | cpu | — | 5.743 ms | 0.007 ms | 0.087 ms | 0.022 ms | 102 |
| mid | gpu | 0.036 ms | 4.190 ms | — | 0.319 ms | 0.025 ms | 96 |
| mid | gpu-packed | 0.058 ms | 4.818 ms | — | 0.119 ms | 0.015 ms | 82 |
| zoom | cpu | — | 10.213 ms | 0.004 ms | 0.085 ms | 0.019 ms | 120 |
| zoom | gpu | 0.030 ms | 1.870 ms | — | 0.243 ms | 0.018 ms | 99 |
| zoom | gpu-packed | 0.054 ms | 1.867 ms | — | 0.199 ms | 0.018 ms | 118 |

† Metal pass-boundary timestamps: the same three draws read 10.2 ms on the
CPU path and 1.9 ms on the GPU paths, so these are not comparable across
command-buffer structures (Apple's TBDR overlaps the vertex phase of one
encoder with the fragment phase of the previous; the boundary stamps include
that wait). Use them within a path, not between paths.
‡ Profiled fps include query resolves and the 32-frame back-pressure stalls;
§4.1 is the honest throughput.

The deterministic CPU-side numbers are the point of this table:

- **The cull is free on the GPU**: 25–58 µs of GPU time per frame for 1,305
  segments (classify + slot/pack + backdrop compaction), versus 4–32 µs of
  CPU time on the CPU path — the same order, but the CPU path's number
  scales with segments on the CPU and the GPU path's does not.
- **`encoder finish` is the Metal encode**: 0.055–0.087 ms for the CPU path
  (its O(visible) direct draws), 0.234–0.319 ms for the multi-draw form
  (1,306 indirect draws looped by wgpu-hal + wgpu's validation batching for
  1,307 draws ≈ 0.18 µs per slot), 0.119–0.199 ms for the packed form (3
  indirect draws; the residual is the compute pass encode and validation
  setup). This is the O(segments) term of §2, priced: ~0.2 ms per 1,300
  segments per frame on the multi-draw form, gone on the packed form.

### 4.3 The fair fight — an incremental CPU path (commit 5)

§4.1's wide-view result looked like a GPU win, but the CPU path there was
paying for per-frame *traffic* (rebuilding and uploading the backdrop list,
writing the camera uniform) for an answer that never changed. The GPU pass is
stateless by design; the CPU's structural edge is that it can remember. So
the CPU path was given a memo, behind levers that default to the Stage F
behaviour:

- `GLYPH_CPU_MEMO=backdrop` — cull every frame, upload the backdrop list only
  when its bytes differ from what is on the GPU;
- `GLYPH_CPU_MEMO=frame` — when the camera key (view-proj, eye, px_scale) is
  bit-identical to last frame's, reuse last frame's draw list and backdrops
  outright (segment edits through `sync_segment` invalidate it);
- `GLYPH_UNIFORM_MEMO=1` — skip the camera / cull-params uniform writes when
  the key is unchanged, on every path.

All seven gates are green with `frame` + uniform memo exported (byte-equal
to the committed baselines; gate 7 37/37), and every render below is
byte-equal to `cpu-ref`. 900 frames × 3 interleaved repetitions, medians:

| config | wide | mid |
|---|---|---|
| cpu (Stage F: cull + upload every frame) | **246** (246/256/234) | 148 (148/131/156) |
| cpu, backdrop memo | 237 (169/237/267) | 142 (142/141/147) |
| cpu, frame memo | 282 (282/249/284) | 151 (139/154/151) |
| cpu, frame memo + uniform memo | **316** (313/491/316) | **157** (157/150/162) |
| gpu (multi-draw) | **338** (361/338/303) | 146 (88/146/162) |
| gpu + uniform memo | 325 (344/322/325) | 146 (144/171/146) |
| gpu-packed | **341** (361/258/341) | 142 (130/142/179) |
| gpu-packed + uniform memo | 324 (327/317/324) | 146 (131/149/146) |

What it says, and what it corrects:

- **The 41 KB backdrop upload was not the cost.** Skipping it alone
  (`backdrop`) moved nothing outside the noise. The earlier explanation in
  this report's first draft was wrong on that point.
- **Not submitting is the cost.** Skipping the cull's per-frame work
  (`frame`) is worth ~15%, and skipping the camera write on top of it — which
  removes wgpu's pending-writes command buffer (a staging allocation, a copy
  and a blit encoder) from every frame — takes the CPU path to 316 fps: parity
  with the GPU forms (325–341) within the spread. One repetition hit 491 fps
  with nothing but a render pass in the frame, which is roughly the harness
  floor on this machine.
- **The GPU forms gain nothing from the uniform memo.** Their frame still
  carries the compute encoders, wgpu's validation pass and the indirect
  draws, so the pending-writes buffer is not on their critical path.
- **Mid is fragment-bound** (4–6 ms of Slug coverage); all eight
  configurations sit in 142–157 fps.

So the honest verdict on "was it a performance improvement": the GPU path won
against the Stage F CPU path because that path uploaded every frame; against
a CPU path allowed to be incremental, it is a draw on this corpus, with the
GPU forms holding a flat per-frame cost that does not grow with segments and
the CPU path holding a zero-cost idle frame. Where the fight is not fair in
the GPU's favour is a moving camera over a much larger table: the CPU path
then culls and re-uploads every frame with O(segments) work on the CPU, the
GPU forms do the same O(segments) work in 25–58 µs of GPU time, and the
packed form encodes O(chunks) draws. That is the case Stage K was built for.

Defaults are unchanged in this commit (`GLYPH_CPU_MEMO=off`,
`GLYPH_UNIFORM_MEMO` off): `--cull cpu` users get the Stage F behaviour
unless they opt in. Flipping `frame` on for the CPU path is a one-line
change with all gates already green for it.

## 5. Profiler back-pressure (commit 3)

Every 300-frame profiled corpus run — both paths, all three views — died at
frame ~65 with `Buffer with 'GpuProfiler - Query Read Buffer' label is
invalid` in `map_async`. A culled frame encodes in microseconds, so the CPU
runs hundreds of frames ahead of the GPU; past `max_num_pending_frames` (64)
wgpu-profiler evicts a pending frame and recycles its query pools while their
`map_async` is still in flight (profiler.rs, `end_frame`). offscreen.rs now
drains every finished frame per iteration and blocks for the GPU once 32
profiler frames are in flight — only under `GLYPH_PROFILE=1`; the unprofiled
loop and every PNG are unchanged.

While placing the encode-cost scope I also found that wgpu 30 does **not**
encode a render pass when it ends: `render_pass_end` pushes a `RunRenderPass`
command and the HAL encoding — for the multi-draw form, the Metal loop of
single indirect draws — happens inside `CommandEncoder::finish()`. The
`"encoder finish (CPU)"` / `"queue submit (CPU)"` scopes in offscreen/windowed
time exactly that.

## 5b. A latent ≥4 GiB arena-chunk hazard (found by the cost experiments)

The first `WGPU_VALIDATION_INDIRECT_CALL=0` corpus runs rendered **blank
frames** in every view with glyph draws — and `--cull-verify` said EXACT.
Not the draws: with validation off, wgpu stops clamping the adapter limits to
u32::MAX (`adjust_limits_for_indirect_validation`), the M2 reports
`max_storage_buffer_binding_size = 9093 MiB`, and the Stage E2 chunking —
built against the "4095 MiB" it had always seen — put the whole 4,357 MiB
corpus arena into ONE chunk. naga's Metal writer carries buffer sizes for its
bounds checks in a synthesized `struct _mslBufferSizes` of `uint`s, so a
≥4 GiB storage binding reads as entirely out of bounds: every `instances[ii]`
returns zero, every quad is degenerate, nothing draws, no error anywhere. The
fixture (10k instances) was fine, which is why gate 7 could not see it.
`GlyphScene::new` now clamps the chunk cap to u32::MAX bytes itself (the exact
chunking validation-on produced: 89,478,485 slots per chunk), so the arena
layout — and every byte of output — no longer depends on an instance flag.
Recorded in AGENTS.md as a fence.

## 6. Remaining gaps / debt

1. **Backdrop compaction and cs_pack are single workgroups** — O(segments/256)
   serial blocks each. Sub-millisecond up to ~10⁵ segments; a
   decoupled-lookback scan is the upgrade past that.
2. **`MAX_CHUNKS = 16`** for the packed form (the per-chunk slot ranges ride
   in the uniform so the scan's barriers sit in provably uniform control
   flow). The corpus needs 2; a 4 GB binding limit means 16 chunks ≈ 64 GB
   of arena. Asserted at scene build with the fix spelled out.
3. **Float parity is empirical, not proven.** wgpu-hal compiles MSL with
   Metal's default fast-math, so the GPU's `sqrt`/divide may round
   differently from the CPU's at the last ulp. A segment exactly on a frustum
   plane is harmless either way (drawn-but-clipped); a segment exactly at
   `glyph_px == 1.0` could flip tiers. Gate 7 and every corpus run show zero
   divergence; `--cull-verify` remains the check for any new view. The CPU
   path stays the reference.
4. **wgpu's indirect validation** is left ON by default (spec-safe). The
   compute cull cannot emit an invalid draw (slot ranges are static and
   chunk-local; the packed totals are sums of them), so
   `WGPU_VALIDATION_INDIRECT_CALL=0` is a legitimate production setting — §4
   prices it, and §5b is the reason it is an *explicit* setting and not a
   default: it changes the limits wgpu reports, and anything sized from them
   must be clamped the way the arena now is.
5. **Windowed verbs under the GPU cull** are smoke-tested, not
   oracle-tested (the pick oracle is Stage G's, unchanged; the GPU cull only
   changes how the draw list is produced).
6. `Features::MULTI_DRAW_INDIRECT_COUNT` is unavailable on Metal; neither
   form needs it. On Vulkan/DX12 `cs_draw_args` could additionally write a
   count for `multi_draw_indirect_count` — not built (no such device here).

## File diffs

- `native/src/bin/mdi_probe.rs` — NEW: the proof tool. `native/tests/mdi_probe.rs` — NEW: runs it under `cargo test`. `Cargo.toml` — `[[bin]] mdi-probe`.
- `native/src/shaders/cull_gpu.wgsl` — NEW: `cs_classify`, `cs_draw_args`, `cs_pack`, `cs_backdrop_compact` + the GPU cull contract header.
- `native/src/shaders/glyph_field.wgsl` — `vs_main` body factored into `glyph_vertex(vi, slot)`; NEW `vs_packed` entry point (bindings 6–8); `vs_main` output byte-identical (gate 6).
- `native/src/gpu.rs` — request `INDIRECT_FIRST_INSTANCE` when the adapter has it; `GpuContext::indirect_first_instance`; instance flags from the environment (identical defaults); shared `read_buffer()`.
- `native/src/glyph_scene.rs` — `CullMode`, `SegCull::flags`/`SEG_HIDDEN`, `DrawSlot`, `CullParams`, `GpuCull` (buffers, four pipelines, slot table), `PackedDraw`, `GlyphBindings`, GPU branches of `render()`, `verify_cull()` for both forms, `sync_segment` mirrors rows, `GLYPH_CHUNK_CAP`, `GLYPH_MDI_LIMIT`, layout tests, module header rewritten.
- `native/src/shaders/cull.wgsl` — header note (shader untouched).
- `native/src/main.rs` — `--cull auto|cpu|gpu|gpu-packed|off`, `--cull-verify`, `resolve_cull_mode`, tests; `native/src/offscreen.rs` — verify hook, profiler back-pressure, finish/submit scopes; `native/src/windowed.rs` — `CullMode`, finish/submit scopes; `native/src/scene.rs` — `debug_cull_verify` hook; `native/src/text.rs`, `native/src/repo.rs` — `SegCull { flags, _pad }`.
- `native/tests/wgsl.rs` — shader set now four files.
- `tools/check-mdi.sh` — NEW gate 7; `tools/check-all.sh` — runs it; `tools/bench-mdi.sh` — NEW corpus bench.
- `native/AGENTS.md` — seven gates, the twin-shader fence, the debug levers, the cull-path section.
