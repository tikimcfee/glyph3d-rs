# AGENTS.md — glyph3d-native (root)

Orientation for anyone (human or agent) working in this tree. The renderer's
output is the contract: **every refactor must be provably output-neutral**,
proven by gates, not by argument. Rust-crate style and rules live in
`native/AGENTS.md`; engine internals in `engine/README.md`. This file is the
map, the commands, and the fences.

## Layout

- `engine/` — Mojo/MAX glyph pipeline + FFI + 16 conformance suites + fixtures
  + benches. Built by pixi, not cargo.
- `native/` — the Rust/wgpu renderer binary; links `native/libglyph_engine.dylib`.
- `tools/` — the gate scripts, the generators, and repro helpers.
- `assets/atlas/` — prebaked glyph-geometry binaries (+ `FORMAT.md`).
- `schema/glyph-identity.json` — layout source of truth (vendored, hash-pinned).
- `out/` — stage reports, proof PNGs, `tooling-ab/baseline/` (the A/B oracle).
- `integration/` — vendored egui 0.36.1 (READ-ONLY reference) + handoff notes.
- `research/` — background surveys.
- `engine-local/`, `.claude/worktrees/` — untracked local scratch / agent
  worktrees; not part of the tree. Ignore them.

## Environment & build

```sh
pixi install                # mojo + max (pins in pixi.toml; pixi.lock is binary — never hand-merge)
pixi run build-engine       # → native/libglyph_engine.dylib (gitignored)
(cd native && cargo build --release)
```

- `pixi run build-engine` is the only correct way to build the dylib: the
  `--fp-mode contract=off` flag is LOAD-BEARING for bit-exactness vs the JS
  oracle (rationale in `engine/check.sh`'s header). Never invoke `mojo build`
  without it.
- After engine or toolchain changes, `cargo build` failing at the linker — or
  build.rs panicking about a missing/stale dylib — means: re-run
  `pixi run build-engine`. There is also a runtime probe (`fp_probe`) that
  panics if the dylib was built without the flag.
- GPU work needs Apple Silicon (Metal); the five GPU suites run as part of the
  default gate.

## The gate — run after EVERY commit

```sh
bash tools/check-all.sh     # from the repo root; exit 0 = all green
```

TWELVE steps as of 2026-09-04 — including (0) build the dylib, (1b) rebuild the 25-fixture corpus, (8b) `--repo-verify` across both FFI paths, and (9) the reference port, none of which are listed below; see `native/AGENTS.md` for the current list. Historically eight steps: generators byte-identical (trie, schema, atlas, vendor manifest) →
16 Mojo suites (CPU+GPU) → zero-warning build → zero-warning clippy →
`cargo test` → `--engine-check` bit-exact vs CPU oracle → stage-g pick oracle →
four-view byte-equal A/B vs `out/tooling-ab/baseline/`. **Any byte divergence
means the commit is wrong — revert or fix; never re-baseline casually.**
(`native/AGENTS.md` describes the original six cargo-side gates; steps 1–2
were added when the JS generators moved into this tree.)

Narrower: `pixi run suites` / `suites-gpu` (engine only), `pixi run check-gen`
(generator byte-identity).

## Fences — read-only or generated, by design

| Path | Status | Authority |
|---|---|---|
| `assets/atlas/*.bin` | generated | `tools/export-atlas.mjs` (inputs in `tools/vendor/ref`) |
| `assets/atlas/engine-trie.bin` | generated | `tools/gen_real_trie.py` |
| `engine/glyph_schema.mojo` | generated | `tools/gen_schema.py` from `schema/glyph-identity.json` |
| `tools/vendor/` | vendored, hash-pinned | `tools/vendor-manifest.py` (`--check` is a gate) |
| `schema/glyph-identity.json` | vendored verbatim from the web repo | drift = upstream refresh, not a local edit |
| `native/src/shaders/*.wgsl` | read-only without a dedicated stage | naga test pins the shader set |
| `native/fixtures/baseline-view.txt` | IMMUTABLE | editing it silently re-baselines the text.png gate |
| `out/tooling-ab/baseline/` | tracked oracle suite | changes only on purpose, with a report |
| `integration/egui/` | vendored upstream egui | read-only API reference — do not modify |

Hand-editing a generated file buys you a gate failure on the next
`check-all`; regenerate instead (`pixi run gen-trie` / `gen-schema`,
`node tools/export-atlas.mjs`).

Dependency pins (wgpu 30, winit 0.30, glam 0.33, egui 0.36, mojo/max
nightly per `pixi.toml`): **no bump without its own re-baselined mini-stage**
— Stages H/I are the template (one dep, full A/B suite, report in `out/`).

## Where things land

- Work lands as a stage: one logical change per commit, gate results in the
  commit message, `out/STAGE_<X>_REPORT.md` for multi-part work (goal/result,
  per-commit detail, gates table, remaining gaps).
- Offscreen `--screenshot` writes where told; the gate writes to
  `out/tooling-ab/sweep/` (untracked). Windowed F2 writes
  `out/windowed-shot-<utc-stamp>.png` (anchored to repo-root `out/`, not cwd).
- Proof PNGs cited by reports are committed; scratch renders are not.
- Engine suites and benches print to stdout — no result files.

## Gotchas

- `engine/bench/gen-bench.mjs` imports the JS oracle from the web repo and does
  NOT run in this tree. `engine/fixtures/gen.mjs` and `gen-bake.mjs` DO: since
  `f68b70f` they read vendored, revision-pinned inputs under
  `engine/fixtures/inputs/`, and gate 1b deletes all 25 fixtures and rebuilds
  them byte-identically on every `check-all`. The conformance corpus is
  regenerable here; only the bench corpus is not.
- The atlas exporter's `hb.cjs` is the one knowingly-modified vendored file;
  `SHA256SUMS` covers `tools/vendor/ref/**`, not all of `tools/vendor/`.
- Mojo uncaught exceptions print to **stderr** — gate scripts capture `2>&1`.
- Comments explain WHY, not what; stage tags (`// Stage F: ...`) mark
  archaeology. Full stage history: `out/STAGE_*_REPORT.md`.

## Read next

`out/ENGINE_TOOLCHAIN_REPORT.md` (a RECORD of the migration, not current state —
its "no node is needed" claim is contradicted by gates 1 and 1b) → the latest stage
report → `engine/README.md` / `README-FFI.md` / `TOOLCHAIN.md` →
`native/AGENTS.md` → module headers in `native/src/*.rs` (the real contracts).
