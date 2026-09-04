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
