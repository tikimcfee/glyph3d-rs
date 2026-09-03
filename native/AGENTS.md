# AGENTS.md — house rules for glyph3d-native

One page. Read this before touching anything. The renderer's output is the
contract: **every refactor must be provably output-neutral**, proven by gates,
not by argument.

## The gates — run after EVERY commit

```bash
bash tools/check-all.sh        # from the repo root; exit 0 = all green
```

Six gates: (1) `cargo build --release` with **zero warnings**, (2)
`cargo clippy --release` zero warnings, (3) `cargo test` all green (19 tests:
naga WGSL validation, clap CLI parity, encase layout assertions), (4)
`--engine-check src/main.rs` — bit-exact Mojo engine vs the text.rs CPU
oracle, (5) `tools/check-stage-g.sh` — scripted picks cross-checked against
an independent python fold oracle, (6) four-view **byte-equal** A/B:
demo / text / repo-zoom / repo-wide re-rendered into `out/tooling-ab/sweep/`
and `cmp`'d against `out/tooling-ab/baseline/`. Any divergence means the
commit is wrong — revert or fix, never re-baseline casually.

## The determinism chain (why the gates can be this strict)

Offscreen renders use a fixed virtual clock (1/60 s per frame), CPU-side
culling/picking (no GPU-dependent traversal order), and a fixed atlas. So a
given commit + given input ⇒ byte-identical PNG. Everything that could break
this is fenced:

- `engine/` — READ-ONLY. The Mojo engine is built by pixi/mojo, not cargo.
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
  Debug panel's LOD_MIN_PX slider programmatically (1.0 → 64.0) and logs the
  cull counters before/after — exercises the panel → probe → CullState →
  cull path without a human at the mouse.

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
