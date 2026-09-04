# AGENTS.md — house rules for glyph3d-native

One page. Read this before touching anything. The renderer's output is the
contract: **every refactor must be provably output-neutral**, proven by gates,
not by argument.

## The gates — run after EVERY commit

```bash
bash tools/check-all.sh        # from the repo root; exit 0 = all green
```

**Run them in a WORKTREE if anyone else is working in this repo.** `check-all`
reads the WORKING TREE, not HEAD, so a second thread's uncommitted edits will
fail your gates and tell you nothing about your own change. That has already
happened once. A fresh worktree is not free — `.pixi/`, the dylib and
`engine/bench/bench.bin` are all untracked, so it needs:

```bash
git worktree add .claude/worktrees/<name> -b worktree-<name>
cd .claude/worktrees/<name>
pixi install                                   # ~1 min, .pixi is untracked
pixi run build-engine                          # the dylib is untracked too
cp ../../../engine/bench/bench.bin engine/bench/   # optional; benches only
```

Setup details and the toolchain's live constraints: `engine/TOOLCHAIN.md`.

NINE gates since 2026-09-02 (was six). Two run FIRST — inputs before
consumers: (1) three generators plus the atlas exporter each rebuild their
committed output and require **byte-identity** (`engine-trie.bin`,
`engine/glyph_schema.mojo`, the 16-file vendor manifest, the four
`assets/atlas/*.bin`); (2) `engine/check.sh` — **16 Mojo conformance suites**
(11 CPU + 5 GPU on Metal), the two INSTRUMENTS (`fixture_census`,
`fixture_manifest` — an instrument nothing runs is an absent one, and the
census was found carrying its own fault), plus a compile pass over all six
benches. Then the
original six: (3) `cargo build --release` with **zero warnings**, (4)
`cargo clippy --release` zero warnings, (5) `cargo test` all green (60 tests:
naga WGSL validation, clap CLI parity, encase layout assertions, ItemParams
validation, and the reference port's reader/trie/fold/scan/bake suites), (6)
`--engine-check src/main.rs` — bit-exact Mojo engine vs the text.rs CPU
oracle, (7) `tools/check-stage-g.sh` — scripted picks cross-checked against
an independent python fold oracle, (8) four-view **byte-equal** A/B:
demo / text / repo-zoom / repo-wide re-rendered into `out/tooling-ab/sweep/`
and `cmp`'d against `out/tooling-ab/baseline/`. Any divergence means the
commit is wrong — revert or fix, never re-baseline casually. And (9)
`tools/check-fixture-parity.sh` — stage 0 of the reference port: Rust's
`.pipe.bin` reader (`native/src/fixture.rs`) and Mojo's `fixture_io` agree on
FNV-1a checksums over their **parsed** values across all 14 fixtures, and
`text.rs`'s CPU fold is diffed **bit-exact** against the oracle's own expected
lanes on every fixture inside its domain (4 today, 5332 records). It fails if
nothing was in domain. Stage 1 added a third half: every fixture's trie rebuilt
from its own BYTES by the ported `GlyphTrie` (`native/src/glyph_trie.rs`) and
compared through the wire-order serializer — 14 fixtures, 11520 entries. Stage 2
added a fourth: the ported serial fold (`native/src/fold.rs`) run over the whole
corpus with **every lane of every byte** compared bit-exact, plus `ordToByte`,
misses, leaders, per-item boxes and the batch union — 14 fixtures, 149,767
leaders, 1,807,512 lanes. Stage 3 added a fifth: the ported scan form
(`native/src/scan.rs`) swept across **8 chunk/group/shard tunings** — 112 cases,
1,144,944 leader lanes bit-exact — under the tiered contract, where invariance
across tunings is monoid associativity checked in situ. Stage 4 added a sixth:
the ported bake (`native/src/bake.rs`) replayed against the 8 `.bake.bin`
fixtures — the streaming record AND the seed protocol (checkpoint-seeded
`prefix_at`, `lanes_from_prefix`, `rows_under_wrap`), 265 queries bit-exact.
That completes the reference port; no JS runs in any gate.

## The determinism chain (why the gates can be this strict)

Offscreen renders use a fixed virtual clock (1/60 s per frame), CPU-side
culling/picking (no GPU-dependent traversal order), and a fixed atlas. So a
given commit + given input ⇒ byte-identical PNG. Everything that could break
this is fenced:

- `engine/` — not cargo's. The Mojo engine is built by pixi/mojo
  (`pixi run build-engine`), never by the Rust build. It is no longer READ-ONLY
  as this file once said — engine work happens here — but a change to it is
  gated by `engine/check.sh` (16 suites) AND must leave the four render
  baselines byte-equal. If an engine change moves a PNG, the change is wrong.
- `assets/atlas/` — READ-ONLY. Atlas binaries define the glyph geometry.
- `native/src/shaders/*.wgsl` — READ-ONLY without a dedicated stage; the naga
  test pins the shader *set*, encase tests pin the lane maps.
- `native/fixtures/baseline-view.txt` — IMMUTABLE. It is the text.png gate's
  input; editing it silently re-baselines (see out/STAGE_I_REPORT.md).
- Version pins: wgpu 30.x, winit 0.30, glam 0.33 (see Cargo.toml comments).
  **No dependency bump without its own re-baselined mini-stage** (Stages H/I
  are the template: one dep, full A/B suite, report in out/).

## Style discipline

- **Zero warnings** from `cargo build` and `cargo clippy`, always. Fix lints
  properly; `#[allow]` only when the lint is genuinely wrong for the code,
  with a one-line justification comment.
- **Fail-loud panics**: this is a binary, not a library. `expect("...")` /
  `assert!` with a diagnostic message is the documented convention — do NOT
  convert to error-returning style. Bare `unwrap()` only in `#[cfg(test)]`.
- **`cargo fmt`**: the tree is NOT fmt-clean (≈77 hunks across all src files,
  mostly long-line wrapping). Do not mass-reformat — the diff/review cost
  exceeds the value. Match the local style of the file you're editing.
- Comments explain WHY (empirical findings, bug history, invariants), not
  what the code does. Stage-tagged (`// Stage F: ...`) for archaeology.

## Debug env vars (all verified present at Stage J)

- `GLYPH_PROFILE=1` — requests TIMESTAMP_QUERY and builds a wgpu-profiler;
  per-pass GPU timings print (windowed: 1 Hz; offscreen: once per run).
  Without it the device is created exactly as before (zero-cost Option).
- `GLYPH_PICK_DEBUG=1` — pick-path diagnostics: pixel ray, AABB hits, local
  point, candidate records (glyph_scene.rs pick functions).
- `GLYPH_CULL_DEBUG=1` — at t=0.0 prints cull stats: visible draw ranges,
  instance count, backdrop count.
- `GLYPH_G_DUMP=<slot>[,<len>]` — offscreen only: reads back instance bytes
  at `slot` from the glyph arena and prints hex (buffer write-path audits).
- `GLYPH_K4_SELFTEST=1` — windowed, dev-only (Stage K): at t≈3 s moves the
  Debug panel's LOD_MIN_PX slider programmatically (1.0 → 16.0) and logs the
  cull counters before/after — exercises the panel → probe → CullState →
  cull path without a human at the mouse.
- `GLYPH_L3_SHADER_COMPOSITE=1` — offscreen, dev-only (Stage L): makes the
  offscreen target Bgra8UnormSrgb, forcing the WINDOWED shader-composite
  path (composite.wgsl) under the deterministic oracle driver; the readback
  swizzles BGRA→RGBA so the PNG compares directly against the Rgba
  baselines. The live-display-free proof of the composite shader.

## Commit cadence

One logical change per commit; run `tools/check-all.sh` after each and put
the gate results in the commit message. Multi-part work lands as a stage with
a report in `out/STAGE_<X>_REPORT.md` (goal/result, per-commit detail, gates
table, file diffs, untouched debt). Untracked scratch (`out/tooling-ab/*`,
proof PNGs) is fine to regenerate; tracked artifacts change only on purpose.

## Read next

`out/` stage reports (A→J) are the design history — read the latest two
before non-trivial work. Module headers in `src/*.rs` carry the real
contracts (cull/LOD, pick, FFI wire format, CLI op-stream ordering).
