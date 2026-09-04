# The engine plan

One engine that computes the per-glyph data a renderer needs, for two targets,
using the GPU when the machine has one and the CPU when it doesn't.

Evidence for everything below is in `engine/delta/` (four subsystem reports plus
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

- **The corpus is ours.** 22 fixtures rebuild byte-identical from vendored,
  per-file-pinned inputs with no web repo present (gate `1b`). The JS oracle is
  a spent correctness source; the web *target* is served by Rust→wasm.
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

## The work, in order

**1. The phantom row.** A line whose glyph count is an exact multiple of the
wrap width gets a blank row, because the newline owns a column and
`rows_for_line(n,w) = n/w + 1` counts it. Three fixtures encode it. Remove it,
oracle first.

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

## Bounds: the work in flight, in order

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
