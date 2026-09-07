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

1. **Measure the fold (item 4).** Cheap, and it decides whether device-side
   folding is worth building at all. No honest number exists today — both
   existing benchmarks measure something other than the fold. Doing this first
   means the substrate question stops being answered by argument.
2. **The readback (item 3).** 3.10 GB per load on a 97 MB corpus, for records
   the renderer does not need. Worth doing whatever item 4 concludes: the copy
   is waste under either substrate. This is the finding the layout seam was
   built to make fixable, so it is the thesis coming due.
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

**3. The readback.** `MojoLayout::run` calls `engine.records()` unconditionally
in both strategies — 3.10 GB per load on a 97 MB corpus, 1.44 s of staging,
8.53 GB peak RSS. `VerifyLayout` gates the API, not the copy. Compaction has to
run where the data already is.

**4. Measure the fold, then decide the substrate.** No honest number exists:
native's GPU benchmark times 36 B-per-source-byte of readback inside its timed
region, and the web's `kernelMs` brackets nine enqueues with no fence. One
harness, one multi-item arena, both paths, several sizes, discrete card. This is
cheap and it decides whether device-side folding is worth building at all.

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
