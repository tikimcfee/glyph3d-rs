# AGENTS.md — house rules for the `native/` crate

**Read the root `AGENTS.md` first.** It is canonical for everything repo-wide:
what each check compares and what it cannot see, what is fenced
and why, and what the stage/gate vocabulary means.

This file is the Rust crate: the layout seam, style discipline, debug env vars,
and the module contracts.

```bash
cargo glyph test        # runs the entire verification battery; exit 0 = all green
```

**Run in a WORKTREE if anyone else is working in this repo.** `cargo glyph test` reads
the WORKING TREE, not HEAD, so another thread's uncommitted edits fail your
checks and tell you nothing about your own change. That has already happened:

```bash
git worktree add .claude/worktrees/<name> -b worktree-<name>
cd .claude/worktrees/<name>
cargo glyph test
```


## The determinism chain (why the checks can be this strict)

Offscreen renders use a fixed virtual clock (1/60 s per frame), CPU-side
culling/picking (no GPU-dependent traversal order), and a fixed atlas. So a
given commit + given input ⇒ byte-identical PNG. That property is what makes the
pixel comparison meaningful, and it is a fact about this crate's
offscreen path.

Everything that could break it is fenced, and **the fence table lives in root
`AGENTS.md`** — it used to be restated here with different reasons and a stale
claim (`engine/` as READ-ONLY, which it has not been since engine work moved
into this tree). One invariant worth carrying: any layout or rendering change
must leave every golden pixel baseline byte-equal. If a change moves a PNG,
the change is wrong.

## The layout seam

Everything that lays glyphs out goes through `native/src/layout.rs`
(the layout seam). Read that module header before adding a backend or a
caller; the short version:

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

  What DID delete the copy is the direct path: `HyperLayout::layout_items`
  writes instances straight into the caller's arena and materializes no 32 B
  wire record at all. The record path (`VerifyLayout`) remains as the
  verification form, and only `--repo-verify` takes it. Run
  `--repo-scan-only --repo-engine hyper|direct|batch|naive` for the per-stage
  split rather than trusting a figure here.
- `ItemParams::validate` runs in `LayoutGlyphs::layout_items`, a PROVIDED
  method. Implement `layout_validated_items`; a backend cannot forget the
  guard because it never calls it.
- Record compaction has one host reference, `layout::compact_records_into`
  (test-only today; the device Pass 2 emits compacted slots directly). It is
  the statement of what blanks, paint indexing and the extents mean, so a
  backend may differ about the FOLD — the thing the corpus checks — and is
  held to this for the rest.
- **Paint is indexed by RECORD, not by instance.** Compaction destroys the
  index that names a byte, so paint crosses the seam and is applied during
  compaction. Indexing it by instance is the tempting mistake and
  `layout::tests::paint_is_indexed_by_record_so_blanks_consume_an_entry`
  is what catches it.
- `--repo-verify` diffs two backends at the seam — placements and instances
  always, and records when BOTH paths have them (`layout::diff_backends`). It
  used to compare records only, which cannot see compaction, paint or extents at
  all. Today it diffs the chosen `--repo-engine` against a recording
  `HyperLayout` reference run; `direct` has no records and reports `0 records`.
  Gated again since 2026-10-09 as `repo-verify` (`hyper`) and
  `repo-verify-direct`. Every strategy is the same HyperLayout, so these are
  HyperLayout against HyperLayout; `--hyper-oracle-check` (gate
  `hyper-oracle`, `hyper_oracle.rs`) is the one that holds HyperLayout to the
  oracle-backed fold.
- **`HyperLayout` (`hyper`) is the DEFAULT layout engine** in pure Rust: a parallel,
  cache-blocked CPU layout engine using Rayon, intra-file line chunking, wrap-aware
  chunking for minified files, background pipelined prepasses, aligned 8-burst / 4-burst
  register slot emission, and zero-allocation streaming lexer coloring. Emits 32-byte
  `RenderSlot` instances in `Instanced` mode or compact 20-byte `DerivedSlot` instances
  in `Derived` mode directly into mapped GPU unified memory without intermediate copies.
- `--repo-engine hyper|direct|batch|naive` selects the layout engine (the
  CubeCL compute engine, `cubecl`, was retired 2026-10-09):
  - `hyper` (default): parallel Rust CPU layout with sub-200ms visual init.
  - `direct` / `batch` / `naive`: the same `HyperLayout`, without `hyper`'s
    background prefetch. Under `--repo-verify`, `direct` records nothing (the
    diff covers placements and instances only) while `batch` and `naive` take
    the recording path; the stats line prints their readback/compaction phases.
- `--field-mode instanced|derived` selects the glyph field mode:
  - `instanced` (32 B per glyph): precomputed 3D coordinates.
  - `derived` (20 B per glyph): compact word layout, Y/Z derived dynamically in vertex WGSL.
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

## Apple Silicon vs. Desktop x86_64 & Discrete GPU Specifics

Detailed technical audit lives in `research/desktop-platform-audit.md`. Key touchpoints:
- **Unified vs. Discrete Slot Buffers (`native/src/layout_hyper/device_alloc.rs`)**:
  - `#[cfg(target_os = "macos")]` allocates Metal `MTLStorageModeShared` memory and maps it (`mapped_base: Some(addr)`). Pass 2 writes directly to device memory with zero copies.
  - On non-macOS/desktop, `layout_device_discrete` writes to a mapped staging buffer, unmaps, and blits to device VRAM via `encoder.copy_buffer_to_buffer` (`mapped_base: None`).
  - ⚠️ **Dynamic Color Writes**: When `mapped_base` is `None`, `crates/glyph-field/src/storage.rs` `SlotStorage::write_colors` falls back to one 4-byte `queue.write_buffer` per slot. On discrete GPUs with large repos, batch these writes to avoid driver call overhead.
- **Cache Sizing & Chunk Threshold (`native/src/layout_hyper/chunk.rs`)**:
  - `CHUNK_THRESHOLD_BYTES = 64 * 1024` (64 KiB) is tuned for Apple Silicon M-series L1 Data Cache (128 KiB per P-core).
  - Desktop x86_64 (AMD Zen 3/4/5, Intel Raptor Lake) has **32 KiB or 48 KiB L1D** per core. A 64 KiB chunk spills to L2. On desktop, testing 32 KiB or 16 KiB thresholds can keep chunks 100% L1D-resident.
  - A line with no newline near the threshold is cut INSIDE the line, and only before an ASCII byte (C15): no cluster sequence has an ASCII member after its first (asserted at atlas load), so every chunk resolves as the whole item does. The chunk after such a cut inherits the line's column, line advance and segment advance from Pass 1's aggregation, which computes the segment advance as the fold's running f32 sum. Changing the threshold moves `native/fixtures/chunk-cut.txt`'s cuts off the sequences it plants: re-prove its mutations.
- **Cache Lines & Burst Stores (`native/src/layout_hyper/pass2_device.rs`)**:
  - `emit_burst8` writes 8 `DerivedSlot`s (160 B) or 4 `RenderSlot`s (128 B). Standard x86_64 cache line width is 64 B (Apple M2 SLC/L2 is 128 B).
- **Copy sizes (`crates/glyph-field/src/copy.rs`, C16, measured 2026-10-09 on an RTX 5090)**:
  - A `copy_buffer_to_buffer` whose size is off 16 B runs whole at about half speed; wgpu only asks for 4. A 20 B `DerivedSlot` stream is 16-aligned only at counts ≡ 0 mod 4, so an odd count cost ~+20 ms of backend on a 94 MB tree. Discrete staging buffers are padded with `padded_staging_size` and slot copies go through `copy_split` (16-aligned body + small tail). Same bytes, same buffer sizes; the unified single-chunk path makes no copy and is untouched.
  - An off-16 source OFFSET also costs; chunked Derived buffers start chunk k at `k x chunk_cap x 20` B (≡ 8 mod 16 at the 2 GiB binding limit; a forced two-chunk split of the 94 MB tree measured ~+1 ms at 8 mod 16, ~+5 ms at 4 mod 16). Left as is: moving it would move which slot lives in which chunk.
- **Golden Pixel Keys**:
  - macOS Metal: `metal-apple`. Desktop Linux/Windows: `vulkan-nvidia`, `vulkan-amd`. Golden baselines are keyed per hardware in `out/tooling-ab/baseline/<key>/`.
- **Pure Rust Portability**:
  - Zero target-specific inline assembly or architecture-specific intrinsics. LLVM auto-vectorizes clean slice loops to AVX2/AVX-512 on x86_64 and NEON on ARM64. Numerical pick checks (`tools/check-pick-oracle.sh`) are 100% bit-exact across platforms.

## Debug env vars

- `GLYPH_PROFILE=1` — requests TIMESTAMP_QUERY and builds a wgpu-profiler;
  per-pass GPU timings print (windowed: 1 Hz; offscreen: once per run).
  Without it the device is created exactly as before (zero-cost Option).
- `GLYPH_TRACE=<filter>` — the load path's span instrument (integration
  note 22): `repo.{walk,backend,verify,views,layout,staged,segments}` and
  `hyper.{pass1,pass2,emoji_tints,pass2.windowed,staging.stream}`, printed on span CLOSE with busy/idle times. The filter is a tracing EnvFilter string (fallback
  `RUST_LOG`; unset = off, one atomic per span). `glyph3d_native=info`
  is the useful setting — a bare `info` also admits wgpu's own
  tracing records, which is loud. The spans mirror the
  LoadStats Instant boundaries exactly so the two can be
  cross-checked; the prints stay the presentation contract.
- `GLYPH_STAGING=host|single` — discrete GPUs only. The default is WINDOWED
  emission (C22): Pass 2 writes each 64 MiB window of the slot stream straight
  into one of two mapped staging buffers, uploaded while the next fills; the
  chunks of emoji-holding items detour through host scratch for their tint
  pairs. `host` writes host memory and streams it through one 64 MiB buffer
  (a full extra copy); `single` is one mapped-at-creation buffer, which
  wgpu-core zero-fills and copies. On the measured NVIDIA box (derived, 102 MB
  tree): windowed 121 ms, host 194, single 482. The fallbacks stay for
  hardware nobody has measured.
- `GLYPH_PICK_DEBUG=1` — pick-path diagnostics: pixel ray, AABB hits, local
  point, candidate records (glyph_scene/pick.rs pick functions).
- `GLYPH_CULL_DEBUG=1` — at t=0.0 prints cull stats: visible draw ranges,
  instance count, backdrop count.
- `GLYPH_G_DUMP=<slot>[,<len>]` — offscreen only: reads back instance bytes
  at `slot` from the glyph arena and prints hex (buffer write-path audits).
- `GLYPH_K4_SELFTEST=1` — windowed, dev-only (Stage K): at t≈3 s moves the
  Debug panel's LOD threshold slider programmatically (1.0 → 16.0) and logs the
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

The dev-only CubeCL instruments (`--cubecl-*`) and their env vars
(`GLYPH_CHAIN_*`, `GLYPH_RECORD_CHUNK`, `GLYPH_REPO_CHECK_*`) were retired with
the engine on 2026-10-09; `git show 659efdb:native/AGENTS.md` has their
documentation.

## Commit cadence

One logical change per commit; run `tools/check-all.sh` before each lands and
put what you ran in the message. Untracked scratch (`out/tooling-ab/sweep/`,
proof PNGs) is fine to regenerate; tracked artifacts change only on purpose.

The `out/STAGE_<X>_REPORT.md` convention is **retired** — see root `AGENTS.md`
§ "Where work lands". Do not open a new letter. Multi-part work still deserves a
written note in `out/`; it just does not need that template.

## Read next

Module headers in `src/*.rs` carry the real contracts (cull/LOD, pick, the
32 B record / slot formats, CLI op-stream ordering) — they are the most
reliable documentation in this crate, because they sit next to the code they
describe.

`out/` reports are design **history**, not current state; read one to learn why
a decision was made.
