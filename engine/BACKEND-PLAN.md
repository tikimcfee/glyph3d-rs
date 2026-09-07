# The engine plan

One engine that computes the per-glyph data a renderer needs, for two targets,
using the GPU when the machine has one and the CPU when it doesn't.

Evidence for everything below is in `engine/delta/` (five subsystem reports plus
a cross-review). This file is the plan; that directory is why.

## The shape

|  | GPU available | no GPU |
|---|---|---|
| **native** | WGSL compute (wgpu) | Mojo CPU fold |
| **web** | WGSL compute (WebGPU) | Rust CPU fold (wasm) |

Target is a build-time choice; device is a **runtime** one, on both targets.
WGSL is the diagonal that serves both. The `gpu_*.mojo` kernels do not compile
to WebGPU, so they are a native-only optimization rather than the GPU path.
Mojo's standing win is the CPU fold: 122 MB/s against the oracle's 46, across
all cores, off any frame budget.

## What holds it together

- **The corpus is ours.** 25 fixtures rebuild byte-identical from vendored,
  per-file-pinned inputs with no web repo present (gate `1b`). The JS oracle is
  a spent correctness source; the web *target* is served by Rust→wasm.
- **THE TWO ORACLES HAVE FORKED, deliberately, and this is the record of it.**
  `c9667ec` corrected `engine/fixtures/inputs/glyphPipelineReference.js` to
  `floor((len-1)/wrap)+1` and `343c039` gave it a wrap mode. The web repo's copy
  still returns `floor(len/wrap)+1` with no mode and will not be updated: the JS
  is a spent correctness source, and the web TARGET is served by Rust→wasm, not
  by the JS app. Treat a difference between the two as expected, not as drift.
  (`engine/delta/review.md:338-372` says there is "no live inconsistency" — that
  report predates the fork and is a record of its moment, not current state.)
- **Direction of change is fixed:** edit the oracle, regenerate the corpus, let
  it red the port, then fix the port. The reverse makes bit-exactness a
  tautology. The oracle stays serial and unscanned — independence of
  construction is what makes the diff mean anything now that we own both ends.
- **Precision is a machine property.** Test for the class, assert it, move on.
  Every discrete lane — `ROW`, `COL`, `GLYPH_ID`, `ORD`, flags — is integer and
  exact on any machine, so picking, caret, wrap and pagination are bit-identical
  in all four cells. Only x carries a precision class.
- **The native scene graph is a harness.** It proved Mojo→FFI→wgpu works. Its
  layout and UI choices carry no authority.

## A note on "Stage N", which this file no longer has

`native/src/layout.rs` and `native/AGENTS.md` refer to "Stage 0 / 1 / 3 of
`engine/BACKEND-PLAN.md`". Those stages were REMOVED from this file in `b6827fb`
when it was rewritten as the plan rather than a record of how it changed. The
hazard is not a dead pointer — it is that the numbered list below is a DIFFERENT
list, so a reader lands on it and matches up. They do not correspond: old stage 1
was the Rust backend; item 1 below is the phantom row. Read those references as
naming the layout seam and the device-resident path by description, not by number.

## Ordering — what blocks what

The numbered list below is a catalogue, not a queue: several of its items touch
nothing the others touch. Read this section for the order, that one for the
detail.

**The spine.** Each of these changes what the next one is deciding about:

1. **Measure the fold (item 4) — CPU HALF DONE 2026-09-07, GPU half open.**
   Doing this first is what stopped the substrate question being answered by
   argument, and it paid twice: it corrected the readback pass's own claim that
   "the CPU fold is 58-77%" (that was the whole FFI call; the fold stage is
   3-13%) and it found a third copy of the record stream nobody had counted,
   inside the engine. Details and the table are under item 4.

   The existing benchmark is `engine/gpu_pipeline.mojo --bench`. It is not
   dishonest — its docstring is explicit that the timed region spans the
   dispatches AND the readbacks, "timing only the kernels would flatter the
   GPU by hiding the part a real caller pays." That was true under the
   `-> Vec<GlyphRecord>` contract. The seam deleted the thing that made it
   true, and the CPU side of the same comparison pays no transport at all, so
   the printed ratio is fold-vs-fold-plus-a-tax-one-side-pays. Two known holes
   to close when this runs: the timed region needs splitting at the fences, and
   `bench_scaling` builds `item_count = 1`, which amortizes the mid-chain host
   round trip (resolveX -> host derives the fan stride -> back to device) over
   exactly one item when a real load has thousands.

   That mid-chain hop is NOT worth benchmarking as a load test: `derive_stride`
   (`glyph_pipeline.mojo:838`) is `max_row_extent + item.page_gap_x`, three
   lines of per-item arithmetic, and the hop exists to mirror the CPU driver
   for conformance rather than out of necessity. It is a one-kernel map away
   from not existing. Measure around it; do not enshrine it.
2. **The readback (item 3) — DONE ON THE CPU 2026-09-07, 3.6x on backend.**
   `Strategy::Direct` removed all three copies at once; details under item 3.
   The device version is now a smaller step than it was, because the host no
   longer has a detour to remove — only an arena to relocate.

   The reasoning that got here, kept because it is the reusable part: The stage split (table under item 4) shows the
   record stream compacted in the engine, copied across the FFI, then compacted
   again on the host: **87% of backend batched, 63% per-item**, against 3-13%
   for the layout computation. So item 3 was understated, not overstated, and a
   borrowing accessor is still the wrong shape — it removes the middle copy and
   leaves two compactions doing overlapping filtering work. The volume claim is
   untouched: 3.10 GB on a 97 MB corpus is a memory argument, and RSS is what
   OOMs a device.

   The engine-side one (`glyph_record.mojo::compact`) is the newly visible
   piece and the most tractable: serial by construction, and its docstring
   already says the GPU form is a prefix sum over the leader flag plus a
   scatter, which the scan machinery computes today.
3. **Bounds step 4 (the FFI accessor)** — but only after the readback, and this
   ordering is the correction earned on 2026-09-07. The plan called it "replace
   the host reduction with the engine box"; they are NOT interchangeable — the
   host seeds at the origin, the engine at ±infinity, so `page` contains the
   engine box rather than equalling it. As long as records cross the seam the
   host reduction rides along nearly free, so this buys a tighter cull and a
   re-baseline. It becomes NECESSARY, not optional, the moment records stop
   crossing — which is what the readback work does.
4. **Displacement input (item 2).** Wants the layout path settled first, since
   it adds a post-fold stage to it.

**Parallel — independent of the spine and of each other:**

- **Emoji (item 5).** Atlas re-bake plus a varying. Touches the shader and the
  atlas, neither of which the spine moves. Gated by the instance-payload
  question below.
- **The web target (item 6).** Cfg-gating and a split. Orthogonal to layout.
- **`z_wrap_spacing` has no CLI flag.** Small, self-contained, named by Ivan.
- **Runtime wrap-mode toggling.** A command-bus question, not a layout one:
  mode is baked into `ItemParams` at load, so toggling means re-running the fold
  (cheap — 0.04 s for 407k records, measured).
- **`engine/check.sh` fails fast rather than accumulating.** One failing suite
  hides the other fifteen. Its sibling `check-fixture-parity.sh` has always
  accumulated; this is the same three-line shape as the pick-oracle fix.

**Measurement debts that gate specific work.** Each is cheap and each currently
blocks a decision by being unmeasured:

- **Instance payload: 32 B or 48 B?** Blocks emoji and highlight both. 40 is
  unreachable (16-byte alignment), 32 forecloses highlight, 48 keeps `_pad`
  where highlight would live at zero cost. Also: nothing ties `InstanceSlot`'s
  layout to `GlyphInstance`'s, so `encase` and `naga` can disagree while the
  test stays green.
- **Is f64 already inert on the render path?** Marked "read from source, not
  measured" — and `cargo glyph prove` now exists to settle exactly this class
  of question by mutation rather than by reading.
- **The bare byte offsets** (`write_instance(…, 24, …)`) have no relationship
  to the struct, and the verbs that use them have **zero pixel coverage**. That
  gap is closable now: adding a golden view is a known, cheap move since
  `repo-down` proved the shape.

## The work, in order

**1. The phantom row — DONE, `c9667ec`.** A line whose glyph count was an exact
multiple of the wrap width claimed a blank row, because the newline owns a
column and `rows_for_line(n,w) = n/w + 1` counted it. Corrected to a ceiling
with the newline placed on the row it closes, oracle first, then the corpus
regenerated from it, then the port. Kept here because the ORDER is the reusable
part: oracle -> corpus -> let it red the port -> fix the port. Never the
reverse, which makes bit-exactness a tautology.

Also landed since: **WrapBack** (`343c039`) — a wrap can cost depth instead of a
row, per item, default `Down`; and the per-item FFI stopped being positional
(`cc814b3`), so a stale dylib is now a link error rather than silently disabled
pagination.

**2. Displacement input.** The fold is a pure function of (bytes, params); an
arranger authors a per-slot delta table; the kernel adds it as a post-fold
stage; bounds, cull, caret and picking all read fold+delta as one truth. Strata
(z from AST depth) and structure (xy packing) are two authors of that one table.
Hide is *not* a delta — it needs a visibility lane, and `flags` is its home.
Land the identity case first: a table of zeros must reproduce today's layout
bit-exact, which the corpus can check without anyone's eye.

**3. The readback — BUILT 2026-09-07 as `Strategy::Direct`, on the CPU.**

The engine now writes 48 B render instances straight into the caller's arena
(`GlyphArena::uninit_tail` / `commit`), applying the blank filter, the paint and
both extents in ONE pass. No wire record is materialized on either side of the
FFI. `--repo-engine direct`.

**47.1 MB corpus, M2, three samples each: backend ~1.99 s -> ~0.39 s (5.2x);
whole load 3.133 s -> 0.915 s (3.4x).** The three lanes that were 87% of backend
are gone — `eg_compact`, `read_back` and `compact_records_into` do not run at
all, and the report prints `n/a` for them rather than `0.000s`, because a zero
reads exactly like a stage that ran and was fast.

The write itself is **grained and parallel** — count, prefix, scatter, over
`BOUNDS_GRAIN` ranges rather than items. `eg_direct` 0.505 s -> 0.193 s. Two
findings from getting there, both measured:

- **Grains, not items.** Item-level decomposition is size-blind and cannot
  parallelize a single large file at all. On one 8 MB file, `eg_direct` is
  0.029 s stable against the record path's 0.191 s — **6.6x**, and that case is
  the one `fold_profile` had already flagged as a ceiling.
- **A struct holding Lists in the parallel region cost more than the
  parallelism was worth.** The first grained form measured 0.659 s against the
  serial 0.505 s — SLOWER. Per-grain heap allocation was the whole difference;
  flat `Float32` scratch fixed it. The bounds pass had already learned this and
  said so in a comment; it still had to be rediscovered by measurement, which is
  the argument for measuring rather than reasoning.

**It is gated, not asserted.** `repo-verify-direct` diffs it against the batched
record path bit-exact on placements and instances, both wrap modes, under TWO
mutations: `direct-ink-half-height` (the arithmetic) and
`direct-prefix-counts-records` (the scan/scatter's core invariant — slots
advance by survivors, paint by records; confusing them leaves uninitialized
gaps). The second exists because the first reddens under the serial writer too,
so it could not see the defect class grains introduce. Coverage stays 12/12. At 47.1 MB the two paths agree on **43,424,013
instances**, byte for byte.

**What it does NOT do, stated because the PASS line says `0 records`:** the
direct path produces no 32 B wire stream, so `VerifyLayout` refuses it rather
than returning an empty Vec a gate could read as "nothing differed". The record
tier stays covered by `repo-verify` and `engine-check` on the other strategies —
the same relationship `witness` already has with the elided fold: a verification
form and a production form, with something adjudicating them.

**Still open here:** the default is still `naive` (per-item). Making `direct`
the default is a behaviour change and a re-baseline decision, not a refactor.
And `eg_counts` — the serial O(bytes) per-item record count — survives on the
batched path; the direct path derives the same counts from the write itself.

**MEASURED 2026-09-07, and the headline is not what this item assumed.** Until
now the only number here was `stage 1.438s` from `out/g-windowed-smoke.log`
(2026-08-31) — which is PRE-SEAM, on the per-item route, and brackets the fold,
the readback and compaction together under phase boundaries that no longer
exist. `backend` is now split at the source (`layout_mojo::BackendPhases`), and
`unattributed` is printed alongside so the four parts can be checked against
the whole. Six corpora, both FFI strategies, `--repo-scan-only`, M2:

| source | strategy | backend | fold | readback | compact |
|---|---|---|---|---|---|
| 6.5 MB | per-item | 0.299 s | 75% | 10% | 15% |
| 6.5 MB | batched | 0.185 s | 59% | 21% | 19% |
| 15.6 MB | per-item | 0.524 s | 70% | 12% | 18% |
| 15.6 MB | batched | 0.446 s | 59% | 22% | 19% |
| 47.1 MB | per-item | 1.666 s | 71% | 12% | 17% |
| 47.1 MB | batched | 1.338 s | 58% | 22% | 20% |
| 61.9 MB | per-item | 3.347 s | 77% | 9% | 14% |
| 61.9 MB | batched | 2.806 s | 39% | 19% | 41% |
| 1.40 GB | per-item | 84.35 s | 59% | 9% | 32% |

**The readback is 9-22% of backend time. It is not the bottleneck; the fold
is.** That is the "oopsie" Ivan named on 2026-09-04 — deciding the seam's shape
against a cost that turns out to be another version of the CPU bottleneck —
arriving exactly where he predicted it would, which is why the measurement went
first.

Read it carefully, though, because it does not say the item is wrong:

- **The volume claim stands and is a MEMORY argument, not a time one.**
  96,860,762 records x 32 B = 3.10 GB is arithmetic and correct; the 1.40 GB
  corpus moves 44.39 GB. RSS is what OOMs a device, and no ratio above touches
  that.
- **The ordering matters more than the totals.** The fold's 58-77% is the CPU
  fold. Move the fold to the device and the remaining costs become the whole of
  what is left — so the readback is not the bottleneck TODAY and becomes it the
  moment item 4 succeeds. Measuring in the other order would have hidden this.
- **Compaction is the larger of the two host costs**, 14-41% against the
  readback's 9-22%, and the two are not independent: device-side compaction
  deletes both, because nothing is left to read back.
- **Batched trades a worse readback for a much faster fold** and wins overall.
  Same bytes, ~2x the readback cost — one 1.85 GB allocation is worse than 9241
  small ones. At 61.9 MB its compaction also goes 2.5x (0.463 s -> 1.153 s),
  which is a cache effect, not more work: per-item compacts records still hot
  from the copy that just wrote them.

**So the decision this measurement was for:** a borrowing accessor (hand out a
pointer instead of `vec![default; n]` + `copy_slots`) buys the readback alone —
9-22% of backend, and it is the half that dies anyway when compaction moves.
It is not worth building as its own step. **Go straight to device-side
compaction**, which takes readback and compact together, and do it after the
fold measurement (item 4), not before — because that is the change that decides
how much is left to win.

**4. Measure the fold — CPU SIDE DONE 2026-09-07, and it moved the target.**

`engine/fold_profile.mojo` is the instrument (third one, runs in `check.sh`),
`run_pipeline` carries seven stage timers, and `glyph_engine_stage_ns` hands
them across the FFI so a load is attributable end to end.

**THE CORRECTION THIS FORCED.** The 2026-09-07 readback pass reported "the CPU
fold is 58-77% of backend". That was wrong, by exactly the defect it had just
diagnosed one layer up: `fold` in `BackendPhases` was not the fold, it was the
whole `glyph_engine_load_items` call — which also runs a SECOND compaction into
the engine arena and a serial O(bytes) walk to attribute records to items.
Naming an aggregate after its largest hoped-for component is how this keeps
happening; the lane is still called `fold` because it is the FFI call's cost,
and the engine line under it is what says how much of that is folding.

**47.1 MB corpus, isolated runs, M2.** Per cent of backend:

| lane | batched | per-item |
|---|---|---|
| `compact_records_into` (host) | 0.887 s / 42% | 0.285 s / 17% |
| `eg_compact` (engine arena) | 0.486 s / 23% | 0.550 s / 34% |
| `read_back` (FFI copy) | 0.469 s / 22% | 0.196 s / 12% |
| `eg_counts` (serial per-item walk) | 0.090 s / 4% | — |
| all of `run_pipeline` | 0.196 s / 9% | 0.597 s / 36% |
| **— the fold stage alone** | **0.069 s / 3.2%** | **0.209 s / 13%** |

**The record stream is copied and repacked THREE times**, and the plan only
ever named one of them:

1. `compact` in `glyph_record.mojo` — per-byte lanes to 32 B records in the
   engine's arena. **Serial by construction**, and its own docstring names the
   fix: "on the GPU this is a prefix-sum over the leader flag plus a scatter,
   which the scan machinery already computes."
2. `read_back` — engine arena to a host `Vec`.
3. `compact_records_into` — host `Vec` to 48 B instances.

Together: **87% of backend batched, 63% per-item.** The layout computation is
3-13%. Item 3 is therefore understated rather than overstated — but the target
is all three passes, not the middle one, and the first is inside the engine
where nobody was looking.

**Two findings from the instrument itself:**

- **`run_pipeline` scales hard with ITEM COUNT**, because `parallelize` is over
  items: 8 MB as one item is 164 MB/s, as 64 items 661 MB/s. A single large
  file folds on one core. That is a real ceiling for a big-file view, and it is
  invisible in any repo-shaped corpus.
- **The two forms scale OPPOSITELY in item count.** Scan beats serial at one
  item (0.86x at 675 KB); at 4096 items it is 81x WORSE (5.3 vs 432 MB/s). So
  `gpu_pipeline.mojo --bench` comparing the device against the SCAN form is
  doubly misleading — that is not the form the FFI ships, and its item shape
  (`item_count = 1`) is the one case where the scan form looks good.

**Still open, and unchanged by this:** the GPU side. One harness, both paths,
several sizes, discrete card — now with the knowledge that a fair CPU baseline
is `run_pipeline` at realistic item counts, not `run_scan_pipeline` at one.

**5. Emoji.** The atlas already carries 897 colour-bitmap slots with doubled
advances; what was never exported is the pixel sheet. `emojiCell` is read in the
vertex stage and never becomes a varying, so nothing can reach the fragment
shader. The re-bake has no external dependency — `REF_ROOT` is vendored and the
shaping crates are already compiled in.

**6. The web target.** Cfg-gate the Mojo backend out, split lib from bin, clear
the wasm blockers, and put `cargo check --target wasm32-unknown-unknown` in
check-all.

## Bounds — steps 1-3 DONE (`d6f33ff`, `dde3f82`), step 4 restated

The cull reads real depth in both the frustum test and the LOD distance;
`PageExtent`, `InkExtent`, `SegCull`, `FileView` and `StagedText` all carry z;
`SLAB_Z`, the named placeholder for the old flat assumption, is deleted with
zero references anywhere, which is the by-construction proof nothing still
reproduces it. `back` is now the default wrap mode, and a fifth golden view
(`repo-down`) keeps the non-default geometry under pixel coverage.

**Two things the work found that this plan had wrong.** First: `page` does NOT
contain `ink` in Y — `page.bottom` tracks baselines, ink tracks the glyph quad
which hangs half a height below. It contains in Z, because a glyph quad has no
thickness, which is the axis these fields are used for; the choice was right for
a reason that was not true as stated. Second: the engine box and the host
reduction are not interchangeable (see Ordering above).

**Still open here:** the `--engine-check` origin coverage is closed, but
`repo-zoom` covers neither wrap geometry meaningfully — `alpha.rs`'s longest
line is 24 columns and never wraps in either mode. That view is pinning a camera
angle, not a layout.

## Bounds: the original plan, for the record

Branch `bounds`, off `cc814b3`. WrapBack made this urgent rather than tidy: a
file's extent is now mostly DEPTH by design, and nothing that reasons about
where things are knows about z.

The surface is four places (measured 2026-09-04):

| where | today |
|---|---|
| `layout.rs:330` `PageExtent` | `right`, `bottom` — no depth |
| `layout.rs:345` `InkExtent` | `[f32; 2]` x 2 |
| `glyph_scene.rs:255` `SegCull` | `[f32; 2]` x 2 |
| `glyph_scene.rs:408,427` | frustum tested at `z = +/-1`; LOD clamps `eye.z` to `+/-1` |

Plus `B_MIN_Z`/`B_MAX_Z`, computed per item by the engine and corpus-gated, with
no FFI accessor — so the fix has a SOURCE and does not need a new reduction.

**1. Make the cull falsifiable first.** Unit-test `cull_segments` with segments
at real depth: a segment at z = -50 that the frustum should keep, and one it
should reject. On today's code these must FAIL — prove that before changing
anything. A screenshot will not do this job: the offscreen camera fits the whole
field, so the +/-1 slab may never change what is drawn from that angle, and a
byte-equal green would mean nothing. This is the empty-file lesson: build the
thing that can see the defect before fixing the defect.

**2. Widen `PageExtent` and `InkExtent` to carry depth.** One more lane in the
reduction `compact_records_into` already runs. Output-neutral — nothing reads it
yet — and gate 8b covers it for free the moment it lands, because `ItemPlacement`
is what `--repo-verify` diffs across the two FFI paths.

**3. Widen `SegCull`; remove the slab from the frustum and the LOD.** Step 1's
test goes green. The four screenshots MUST stay byte-equal: if one moves, the
old cull was dropping or keeping something it should not have, and that is a
finding to understand before accepting a re-baseline.

**4. Then the FFI accessor, last and only after a comparison.** Expose the
engine's per-item box and replace the host reduction with it. DO NOT assume they
agree: the host reduction is seeded at the origin and runs over every record
including blanks (`layout.rs`, `compact_records_into`); whether the engine's
`bounds_range` matches that seeding is UNVERIFIED. Compare first, in both
directions, and treat a disagreement as the interesting result rather than a
merge conflict to settle.

Two follow-ons named by Ivan, deliberately NOT folded in: the fold pitch
(`RepoParams::z_wrap_spacing`) has no CLI flag and making it configurable is its
own small change; runtime wrap-mode toggling is a command-bus question, not a
layout one, since mode is baked into `ItemParams` at load and toggling means
re-running the fold (cheap now — 0.04 s for 407k records, measured).

## Open, and worth deciding before the work that depends on it

- **Does the row/column bookkeeping need to exist?** It is not a layout idea —
  it bounds the cost of the backward walk, and it bought order-independence
  (x spread 6.6e-3 → 7.2e-6 across dispatch orders). The 2019 Metal original
  needed none of it: pagination was `fmod` on a continuous position. Any
  replacement must keep determinism across dispatch orders.
- **Is f64 already inert on the render path?** With wrap on, x comes from
  `segment_advance` (f32); the f64 chain survives only as the `LINE_ADV` witness
  lane, which the FFI does not expose. Read from source, not measured.
- **Instance payload.** With `pos: vec3<f32>` the only reachable sizes are 32 B
  and 48 B — 16-byte alignment makes 40 unreachable. 32 forecloses highlight;
  48 keeps `_pad`, which is where highlight belongs at zero cost. And nothing
  ties `InstanceSlot`'s layout to `GlyphInstance`'s: `encase` and `naga` can
  disagree while the test stays green.
- **Byte offsets.** `write_instance(…, 24, …)` and friends are bare literals
  with no relationship to the struct, and the verbs that use them have zero
  pixel coverage in any gate.
