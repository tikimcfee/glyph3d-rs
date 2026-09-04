# The layout seam: one contract, three backends, two targets

Written 2026-09-04, before any code, so the work survives a context boundary.
**Self-contained on purpose** — everything needed is in this repo or stated here.
No memory of the session that wrote it is required. Companion to
`engine/PORT-PLAN.md` (the reference port, DONE) and
`research/wasm-port-audit.md` (the wasm blocker inventory).

## The state of play, with evidence

Three implementations of one fold exist. All three are gated bit-exact against
the same frozen JS-oracle corpus, so they agree with each other **by bits**, not
approximately:

| implementation | where | role today |
|---|---|---|
| JS | `viz-web/glyph3d-js` | frozen oracle. Not executed by the engine gates. |
| Mojo | `engine/*.mojo` | **the runtime**, reached through `native/src/layout_mojo.rs` since stage 0. `engine/ffi.mojo` → `run_pipeline`, CPU, parallel across cores. |
| Rust | `native/src/{fold,scan,bake,glyph_trie}.rs` | gate-only. **Zero production callers** — stage 1 gives it the seam. |

Verified 2026-09-04: `grep crate::fold|crate::scan|crate::bake|crate::glyph_trie`
across `native/src` returns only `fixture.rs` (the gate) and the modules' own
tests. `main.rs`, `repo.rs`, `glyph_scene.rs`, `windowed.rs`, `text.rs` reference
none of them.

**Which path a run takes depends on the launch:**

| launch | layout path | Mojo? |
|---|---|---|
| `--load-repo <dir>` | `repo::load_repo` → `Engine` FFI (both `naive` and `batch`) | yes |
| `--engine-render <file>` | `SceneChoice::EngineText` → FFI | yes |
| no flags / `--render-file` | `SceneChoice::Text` → `text::stage_file` | **no** |
| `--demo` | quad field, no text | no |

The dylib is LINK-TIME bound (`@rpath/libglyph_engine.dylib` in `otool -L`), not
`dlopen`'d, so a missing dylib fails to link rather than silently degrading.
`Engine::new()` runs `glyph_engine_fp_probe` and panics if the dylib fused a
multiply-add.

**The GPU kernels are not wired.** `gpu_decode`, `gpu_scan`, `gpu_paginate`,
`gpu_bounds`, `gpu_pipeline` are referenced only by each other and by conformance
suites. Nothing the app runs reaches a GPU kernel.

## THE FINDING that motivates this plan

`engine/README.md` reports the GPU chain at ~100 MB/s, flat from 4 MB to 24 MB,
and concludes "parity, GPU slightly behind" on an M2. That number is **not a
compute measurement**.

Read `gpu_pipeline.mojo`'s timed region (`g0` .. `gpu_ns`). It ends with six
`enqueue_copy` calls pulling the entire output back to host:

| buffer | per source byte |
|---|---:|
| `lm` (X, Y, Z, BASE_X) | 16 B |
| `lc` (ROW, COL) | 8 B |
| `wm` (LINE_ADV) | 4 B |
| `wc` (ORD) | 4 B |
| `otb` (ordToByte) | 4 B |
| | **36 B** |

For a 24 MB source that is **~864 MB copied back**, inside the timed region. 24 MB
at 100 MB/s is ~240 ms, so the measurement is reporting roughly **3.6 GB/s of
readback bandwidth**, with the kernels somewhere underneath it. The ~400 MB of
UPLOAD (four u32-per-byte tables) happens before `g0` and is excluded, so the
asymmetry runs against the GPU twice.

The README defends including the readback — "timing only the kernels would
flatter the GPU by hiding what a real caller pays" — and that is correct **for
today's caller**, because `engine.rs`'s contract is `-> Vec<GlyphRecord>`, a
CPU-owned array. The bytes genuinely must come home to satisfy it.

**But nothing needs them there.** Everything the CPU does with those 864 MB, in
`text::stage_records`, is:

1. drop blanks (`glyph_id == 0`) — a stream compaction
2. repack `GlyphRecord` (32 B) → `GlyphInstance` (48 B), where `pos`, `glyph_id`,
   `row`, `col`, `advance`, `height` are straight copies and the only additions
   are `color` (a constant in this path), `group_id: 0`, `flags: 0`, `_pad: 0`
3. min/max the positions for camera fit — a reduction

A filter, a memcpy that inserts padding, and a reduction. The engine **already
has kernels for two of them**: `k_partial_scan` is the prefix scan compaction
needs, and `gpu_bounds` is a conformance-tested min/max reduction.

And the renderer's real needs are METADATA, not data:

| operation | what the CPU needs | size |
|---|---|---|
| picking | send a click coord, get one ID from an ID pass | bytes |
| color highlight | "instances 4,182-4,733 turn amber" — a range and a write | bytes |
| group movement | one `GroupRow` (80 B) in its own storage buffer | bytes |

The CPU needs to know WHERE THINGS ARE, not what the positions are. The `Vec<GlyphRecord>`
contract dates from Stage D, whose point was printing records to a terminal. It
was never redesigned when those records started feeding an instance buffer.

Two real kernel defects are visible in the same listing and are secondary to the
above: `k_spine_scan` dispatches `grid_dim=1, block_dim=1` (one GPU thread
serially combining 1,536 supers at 24 MB), and the fan-stride derivation between
`resolveX` and `paginate` does a full device drain to compute one float per item.

## The model

Ivan's framing, adopted: **this is a compilation-target dependency problem.**

- Target is native → Mojo backend (CPU-parallel today, GPU once wired).
- Target is web → Rust backend, because Mojo cannot compile to wasm.

Rust vs Mojo is a **build-time** decision (target / cargo feature). CPU vs GPU
inside the Mojo backend is a **runtime** one, because device availability is not
a build fact.

Cross-platform determinism comes free: both backends are gated bit-exact against
the same corpus, so native and web produce IDENTICAL layouts — same positions,
rows, columns, picking results. A native screenshot is a valid reference for web.

## Stages, each with its own gate

Staged by ACCEPTANCE TEST. **Stage 0 comes first and alone** — every later stage
is an implementation of the seam it defines, and defining it twice is the one
avoidable mistake here.

| stage | work | acceptance | risk |
|---|---|---|---|
| **0** | define the seam: instances + metadata, not `Vec<GlyphRecord>` | today's Mojo-CPU path satisfies it; the four byte-equal screenshots unchanged | **DONE 2026-09-04** |
| **1** | Rust port behind the seam (slots → instance format) | `--repo-verify` diffs Rust vs Mojo over a real repo, bit-exact | low; the port is already proven, and the seam now carries the gate |
| **2** | cfg-gate + lib/bin split + wasm target | `cargo check --target wasm32-unknown-unknown --no-default-features` in check-all | medium; audit blockers 2-5 |
| **3** | device-resident GPU path: compaction + bounds on device, metadata-only return | GPU output bit-equal to CPU; readback no longer in the timed region | high, and the payoff |
| **4** | honest GPU measurement | same harness, same arena, multi-ITEM, CPU vs GPU | none, but it decides stage 3's worth |

**Stage 0 is the hinge.** If the seam is defined as "fill this buffer, return
counts and ranges," then stages 1, 2 and 3 are three implementations of it and
nothing has to be built twice. If stage 1 lands against the OLD `Vec<GlyphRecord>`
contract, the Rust fallback gets written once for records and again for
instances.

**Stage 4 could go first.** It is independent and cheap, and it decides whether
stage 3 is worth doing at all — see the open question below.

## Stage 0 — DONE, 2026-09-04

`native/src/layout.rs` (the contract) + `native/src/layout_mojo.rs` (the Mojo
backend). `ItemParams` and `GlyphRecord` moved out of `engine.rs` onto the seam
— they are the contract all three backends share, and none of them is the FFI.

What crosses it: `LayoutItem` (bytes + params + `Paint` + group) in, a
caller-owned `&mut GlyphArena` as the destination, `ItemPlacement` (slot range,
three counts, `PageExtent`, `InkExtent`) out. **No method on `LayoutGlyphs`
returns a position.** The `Vec<GlyphRecord>` readback survives only behind
`VerifyLayout`, a separate trait, so it is unreachable from the render path
rather than merely discouraged.

Four decisions worth keeping:

- **The destination is passed in.** Every call site holds an arena instead of
  receiving a `Vec`, so stage 3 changes the arena's interior and nothing else.
- **Validation is a PROVIDED trait method.** Backends implement
  `layout_validated_items`; they cannot forget `ItemParams::validate` because
  they never call it. Three implementations of one fold is three chances to
  omit a guard, and this removes all three.
- **Compaction is written once** (`compact_records_into`) and shared by every
  host backend, so two backends can differ about the FOLD — the thing the
  corpus gates — and cannot differ about blanks, paint or extents.
- **The batch/per-item split moved below the seam.** It was never a property of
  loading a repository; it is one backend's answer to "how many times do I
  cross into Mojo?", and the Rust backend will have no equivalent question.

`--repo-verify` GOT STRONGER, which is the part stage 1 depends on. It compared
RECORDS only, which cannot see a difference in compaction, in paint indexing,
or in either extent — every one of which now lives behind the seam and every one
of which a new backend has to get right. `layout::diff_backends` now compares
placements, instances AND records, bit-exact, and names where they first differ:

```
repo-verify PASS: 4 items, 10857 instances, 11168 records bit-exact
                  between mojo-cpu/per-item and mojo-cpu/batched
```

**Acceptance.** All nine gates green (21 PASS lines); the four screenshots
byte-equal; 72 cargo tests (was 60). `--engine-render` is NOT one of the four
screenshots, so it was A/B'd separately against a stashed pre-change build on
`fixtures/baseline-view.txt` — byte-equal. (The first attempt at that A/B used
`src/text.rs` as input, a file the change itself had edited, and reported a
divergence that was entirely the different input. A red proves nothing until you
know it failed for the right reason.)

**Proof the greens can go red** — nine mutations, nine reds, each edit asserted
to have landed before the check ran:

| # | mutation | result |
|---|---|---|
| M1 | paint indexed by INSTANCE instead of record | unit RED + repo-wide/repo-zoom diverge |
| M2 | the paint-length assert removed | unit RED |
| M3 | `bit_eq` degraded to `PartialEq` | unit RED |
| M4 | `diff_backends` stops comparing instances | unit RED |
| M5 | `diff_backends` stops comparing records | unit RED |
| M6 | page extent seeded empty, not at the origin | unit RED, **screenshots still byte-equal** |
| M7 | ink extent measures blanks too | unit RED |
| M8 | the seam's provided validation removed | unit RED |
| M9 | the two Mojo strategies desynchronised | unit RED + `--repo-verify` RED |

**M6 IS A MEASURED CEILING, NOT A PASS.** The page extent's origin seed only
binds for an item with ZERO records, and `fixtures/g-pick-repo` contains no
empty file — so the four-view A/B cannot see that seed at all, and the unit test
is its only cover. It is not academic: an empty `.rs` in a real repo IS walked,
laid out and staged (verified 2026-09-04 on a two-file scratch repo), and
without the seed its page would be an inverted rectangle fed to the shelf
packer. Closing this would mean adding an empty file to the pick fixture, which
re-baselines three screenshots — a conscious act, not a stage-0 one.

**What stage 1 now is.** Implement `LayoutGlyphs` over `fold`/`scan`/`bake`,
emitting the same 32 B `GlyphRecord`s and calling the SAME
`compact_records_into`. Then point `--repo-verify` at it instead of the second
Mojo strategy. That is a constructor swap plus the record producer; the differ,
the validation, the compaction and the extents are already written and already
proven able to fail.

## Open questions that want measurement, not argument

- **Does the GPU have a case?** Unknown. The existing table is a readback
  benchmark (above), and the CPU side of the comparison came from a DIFFERENT
  harness (`stream_bench` batching linux at 117 MB/s) than the GPU side
  (`gpu_pipeline --bench` on a dictionary). That is not an A/B. The clean
  experiment: one harness, one arena, multi-item, both backends, several sizes,
  and on a DISCRETE card — the M2's unified memory hides a PCIe cost a real
  discrete GPU pays, and the discrete plateau is the entire open question.
- **`bench_scaling` builds `item_count = 1`.** A single Item spanning the whole
  buffer, not the concatenated multi-file arena `load_items` actually ships. Most
  dispatches are sized by BYTES so item count probably does not move throughput —
  but "probably" is not a measurement, and `paginate` does resolve an item per
  thread.

## What NOT to do

- **Do not wire the GPU before stage 0.** It would inherit the readback and
  measure the same non-answer again, more expensively.
- **Do not merge `text::reference_layout` into `fold.rs`.** Different lineage is
  the point; the reason is at both sites and in `PORT-PLAN.md`.
- **Do not let the web backend become the untested one.** It will not, as long as
  gate 9 keeps running the Rust port on every `check-all` — which is exactly why
  the fork is affordable. If that gate is ever weakened, the fork stops being
  affordable with it.
- **Do not reach back into `viz-web/glyph3d-js`.** It is not trunk.

## Verification habits this repo runs on

- **A green must be earned.** Break the thing a check watches and confirm it
  reddens, before trusting it.
- **A red proves nothing until you know it failed for the RIGHT reason.**
- **Assert the edit landed.** A failed anchor match is silent, and a mutation
  that did not apply prints exactly like a corpus that cannot discriminate.
- **A label is a claim.** If a check is parameterized, assert the parameter
  ENGAGED.
- **Mind the shape of a measurement**, not only its number. This whole plan
  exists because a throughput figure was reporting a memcpy.
