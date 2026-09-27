# AGENTS.md — house rules for the `native/` crate

**Read the root `AGENTS.md` first.** It is canonical for everything repo-wide:
what each check compares and what it cannot see, what is fenced
and why, the build trap (`cargo build` does not build the Mojo dylib), and what
the stage/gate vocabulary means. That account used to be duplicated here and
drifted out of sync in both directions; this file no longer restates it.

This file is the Rust crate: the layout seam, style discipline, debug env vars,
and the module contracts.

```bash
bash tools/check-all.sh        # from the repo root; exit 0 = all green
```

**Run in a WORKTREE if anyone else is working in this repo.** `check-all` reads
the WORKING TREE, not HEAD, so another thread's uncommitted edits fail your
checks and tell you nothing about your own change. That has already happened. A
fresh worktree is not free — `.pixi/`, the dylib and `engine/bench/bench.bin`
are all untracked:

```bash
git worktree add .claude/worktrees/<name> -b worktree-<name>
cd .claude/worktrees/<name>
pixi install                                   # ~1 min, .pixi is untracked
pixi run build-engine                          # the dylib is untracked too
# engine/bench/bench.bin does not exist here; check.sh only COMPILES the
# benches, so nothing needs it. gen-bench.mjs still reaches into the web repo.
```

Setup details and the toolchain's live constraints: `engine/TOOLCHAIN.md`.


## The determinism chain (why the checks can be this strict)

Offscreen renders use a fixed virtual clock (1/60 s per frame), CPU-side
culling/picking (no GPU-dependent traversal order), and a fixed atlas. So a
given commit + given input ⇒ byte-identical PNG. That property is what makes the
pixel comparison meaningful, and it is a fact about this crate's
offscreen path.

Everything that could break it is fenced, and **the fence table lives in root
`AGENTS.md`** — it used to be restated here with different reasons and a stale
claim (`engine/` as READ-ONLY, which it has not been since engine work moved
into this tree). One correction worth carrying: an engine change is gated by the
Mojo suites *and* must leave every pixel baseline byte-equal. If an engine change
moves a PNG, the change is wrong.

## The layout seam

Everything that lays glyphs out goes through `native/src/layout.rs`
(the layout seam; `engine/BACKEND-PLAN.md` no longer numbers it). Read that module header before adding
a backend or a caller; the short version:

- A backend takes `LayoutItem`s (bytes + `ItemParams` + `Paint` + group),
  appends instances to a caller-owned `GlyphArena`, and returns
  `ItemPlacement`s — a slot range, three counts, two extents. **No method on
  `LayoutGlyphs` returns a position.** That is the point: it is what lets a
  device-resident backend keep the glyphs on the device.
- A gate that needs the 32 B wire records asks `VerifyLayout`, a SEPARATE
  trait, so no method a caller holds RETURNS a position. That is what would let
  a device-resident backend keep glyphs on the device. **It does NOT mean the
  readback is gone** on the strategies that HAVE one — this file claimed
  otherwise until 2026-09-04 and was wrong, because `VerifyLayout` gates the
  API, not the copy. Do not widen `LayoutGlyphs` to return records.

  What DID delete the copy is `Strategy::Direct` (2026-09-07): the engine writes
  instances straight into the caller's arena, and no wire record is materialized
  on either side of the FFI. The record strategies remain, as the verification
  form, and `repo-verify-direct` diffs the two against each other. Run
  `--repo-scan-only --repo-engine direct|batch|naive` for the per-stage split
  rather than trusting a figure here; measurements and their dates live under
  items 3 and 4 of `engine/BACKEND-PLAN.md`.
- `ItemParams::validate` runs in `LayoutGlyphs::layout_items`, a PROVIDED
  method. Implement `layout_validated_items`; a backend cannot forget the
  guard because it never calls it.
- Compaction is written once (`layout::compact_records_into`) and shared by
  every host backend, so two backends can differ about the FOLD — the thing
  the corpus gates — and cannot differ about blanks, paint indexing, or the
  extents.
- **Paint is indexed by RECORD, not by instance.** Compaction destroys the
  index that names a byte, so paint crosses the seam and is applied during
  compaction. Indexing it by instance is the tempting mistake and
  `layout::tests::paint_is_indexed_by_record_so_blanks_consume_an_entry`
  is what catches it.
- `--repo-verify` diffs two backends at the seam — placements and instances
  always, and records when BOTH paths have them (`layout::diff_backends`). It
  used to compare records only, which cannot see compaction, paint or extents at
  all. Today it runs the Mojo backend's FFI strategies against each other, in
  two gates: `repo-verify` pairs the record strategies, `repo-verify-direct`
  pairs `Direct` against batched and reports `0 records` because the direct path
  produces none. The Rust backend is next to receive the same call.
- **`direct` is the DEFAULT strategy.** `--repo-engine naive|batch` selects a
  record strategy when you want one; the gates that name a specific pair pin it
  explicitly, and the repo golden views deliberately do not, so the default gets
  pixel coverage.
- A verify over ZERO items refuses. Before 2026-09-07 a missing corpus directory
  printed `PASS: 0 items, 0 instances` and exited 0 — the gate passing having
  compared nothing.

`fixtures/g-pick-repo/empty.rs` IS ZERO BYTES ON PURPOSE, and it is the only
input in the tree that reaches the page extent's origin seed. The seed binds
only for an item with ZERO records; before that file existed, seeding the
extent empty instead reddened its unit test and left all four screenshots
byte-equal, so gate 8 could not see it at all. With the empty file in place
that same mutation moves `repo-wide.png` (verified 2026-09-04; `repo-zoom` is
framed on alpha.rs and still cannot see it, which is fine — one gate seeing it
is the point). Do not "tidy up" the empty file, and if the fixture is ever
rebuilt, put one back.

## Style discipline

- **Zero warnings** from `cargo build` and `cargo clippy`, always. Fix lints
  properly; `#[allow]` only when the lint is genuinely wrong for the code,
  with a one-line justification comment.
- **Fail-loud panics**: this is a binary, not a library. `expect("...")` /
  `assert!` with a diagnostic message is the documented convention — do NOT
  convert to error-returning style. Bare `unwrap()` only in `#[cfg(test)]`.
- **`cargo fmt`**: the tree is NOT fmt-clean (≈234 hunks across all src files,
  mostly long-line wrapping). Do not mass-reformat — the diff/review cost
  exceeds the value. Match the local style of the file you're editing.
- Comments explain WHY (empirical findings, bug history, invariants), not
  what the code does. Stage-tagged (`// Stage F: ...`) for archaeology.

## The hardware profile

`gpu::GpuProfile` is resolved once in `gpu::init` from the adapter wgpu picked
and carried in `GpuContext`. Anything that must branch on hardware — present
mode, indirect-draw support, the Metal `first_instance` workaround in
`glyph_scene.rs` — reads it; `cfg!(target_os)` is the wrong axis for all of
those and is not used for any of them. `--gpu-key` prints the golden-set key
(`backend-vendor`), `--gpu-profile` the full record; root `AGENTS.md` § pixel-ab
says how the build tool uses both. `--present-mode fifo|mailbox|immediate` is
windowed-only and rides on the FPS line, because under Fifo that figure is the
display's refresh (75 on the first Linux box) and not a fact about the renderer.

## Debug env vars

- `GLYPH_PROFILE=1` — requests TIMESTAMP_QUERY and builds a wgpu-profiler;
  per-pass GPU timings print (windowed: 1 Hz; offscreen: once per run).
  Without it the device is created exactly as before (zero-cost Option).
- `GLYPH_PICK_DEBUG=1` — pick-path diagnostics: pixel ray, AABB hits, local
  point, candidate records (glyph_scene/pick.rs pick functions).
- `GLYPH_CULL_DEBUG=1` — at t=0.0 prints cull stats: visible draw ranges,
  instance count, backdrop count.
- `GLYPH_G_DUMP=<slot>[,<len>]` — offscreen only: reads back instance bytes
  at `slot` from the glyph arena and prints hex (buffer write-path audits).
- `GLYPH_K4_SELFTEST=1` — windowed, dev-only (Stage K): at t≈3 s moves the
  Debug panel's LOD_MIN_PX slider programmatically (1.0 → 16.0) and logs the
  cull counters before/after — exercises the panel → probe → CullState →
  cull path without a human at the mouse.
- `GLYPH_ZSPACE_SELFTEST=1` — windowed, dev-only: at t≈3 s drives the Debug
  panel's z_wrap_spacing dial to 2× and fires the SAME scene-rebuild arm the
  slider's drag-release uses, logging instance count (must not change —
  z_step moves no slot counts) and the field's z extent (must ~double)
  before/after. Exercises the panel → probe → pending_relayout → rebuild
  path without a human.
- `GLYPH_CLUSTER_SELFTEST=1` — windowed, dev-only: toggles the Debug panel's
  cluster-mode button through the SAME rebuild arm, logging the instance
  count before/after (on cluster-bearing content the count moves — trailers
  enter/leave the arena; the DIRECTION follows the starting mode: with
  cluster the default since 2026-09-22, the hook's toggle goes OFF and the
  count rises. Measured leader→cluster on fixtures/g-cluster-repo: 359 →
  345; on fixtures/emoji-corpus-small.txt, a TEXT scene: 893 → 801).
  The button shows on repo and text scenes; demo/engine-text scenes carry
  no mode and hide it.
- `GLYPH_L3_SHADER_COMPOSITE=1` — offscreen, dev-only (Stage L): makes the
  offscreen target Bgra8UnormSrgb, forcing the WINDOWED shader-composite
  path (composite.wgsl) under the deterministic oracle driver; the readback
  swizzles BGRA→RGBA so the PNG compares directly against the Rgba
  baselines. The live-display-free proof of the composite shader.

And the dev-only CubeCL instruments (all exit drivers, none in the
battery; the state handoff is note 18 in the integration notes):

- `--cubecl-smoke` — bring-up smoke (`cubecl_smoke.rs`, the note-16 phase
  0): device-share + both-direction buffer interop + the contraction
  measurement, verdicts printed.
- `--cubecl-scan-check <fixture>` — phase 1: chunk partials bit-exact.
- `--cubecl-chain-check <fixture>` — the full chain vs `scan.rs`: counts +
  rows exact, fold>0 X bit-level, line_adv/positions eps.
- `--cubecl-chain-bench <corpus>` — per-dispatch GPU-timestamp table.
- `--cubecl-decode-check <fixture>` — decode vs `decode_all`, bit-exact.
- `--cubecl-cluster-check <fixture>` — decode + cluster vs `decode_all` +
  `resolve_clusters`, bit-exact.

Shared env vars: `GLYPH_CHAIN_STAGES` (absolute dispatch count — bisection),
`GLYPH_CHAIN_LOOP` (samples, minimum reported), `GLYPH_CHAIN_WRAP=<w>`
(fold>0 shape), `GLYPH_CHAIN_TILE`/`RAKE` (scan shape), `GLYPH_CHAIN_SPAN`
(resolve worker bytes), `GLYPH_CHAIN_DECODE=1` (bench runs from raw bytes),
`GLYPH_CHAIN_CLUSTER=1` (bench adds cluster mode — implies DECODE; the
bench item flips to Cluster, the ranked chain runs as stages 1-4
(probe / compact / rank / mark — list ranking over the candidate jump
graph, note 18 §6c), a 4 B setup readback sizes the level tables, and
fl/sm are diffed bit-exact against `decode_all`+`resolve_clusters`),
`GLYPH_CHAIN_DEBUG=1` (dumps, incl. the cluster candidate table).

## Commit cadence

One logical change per commit; run `tools/check-all.sh` before each lands and
put what you ran in the message. Untracked scratch (`out/tooling-ab/sweep/`,
proof PNGs) is fine to regenerate; tracked artifacts change only on purpose.

The `out/STAGE_<X>_REPORT.md` convention is **retired** — see root `AGENTS.md`
§ "Where work lands". Do not open a new letter. Multi-part work still deserves a
written note in `out/`; it just does not need that template.

## Read next

Module headers in `src/*.rs` carry the real contracts (cull/LOD, pick, FFI wire
format, CLI op-stream ordering) — they are the most reliable documentation in
this crate, because they sit next to the code they describe.

`out/` reports are design **history**, not current state; read one to learn why
a decision was made. For the layout seam and what comes next, `engine/BACKEND-PLAN.md`;
for the reference port's stage record, `engine/PORT-PLAN.md`.
