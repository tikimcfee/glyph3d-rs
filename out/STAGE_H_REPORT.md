# Stage H — tooling pass (naga · glam flags · clap · wgpu-profiler · encase)

**Goal**: adopt the "tooling tier" of ecosystem crates — WGSL validation in
`cargo test`, math-library assertion features, a real CLI, per-pass GPU
timings, and generated buffer-layout assertions — WITHOUT perturbing the
renderer's output or the verified text pipeline. Every phase independently
shippable, gated by byte-identical render A/Bs (spec:
`glyph3d-integration-notes/06-tooling-pass-handoff.md`).

**Result**: all five phases landed clean (Phase 5 in its option-(b) form,
which the handoff declares full success; sub-phase 4b skipped as explicitly
optional). Every acceptance gate passed at every phase: 4× byte-identical
screenshots vs baseline, `--engine-check` PASS, `tools/check-stage-g.sh` ALL
PASS, `cargo test` green (1 → 19 tests), zero-warning debug+release builds,
and no new semver-boundary duplicates in `cargo tree -d`. Stack pins
untouched: wgpu 30.0.1, winit 0.30.13, glam 0.30.10 (feature flags only).

Commits (one per phase, gates run per commit; local only, nothing pushed):

| commit | phase |
|---|---|
| `8d6eb5a` | 1 — naga WGSL validation in cargo test |
| `90397c3` | 2 — glam feature flags (approx, debug-glam-assert, mint) |
| `5d23f27` | 3 — clap CLI (derive, completions, op-stream order parity) |
| `0d32eea` | 4 — wgpu-profiler per-pass GPU timings behind GLYPH_PROFILE=1 |
| `d6daee7` | 5 — encase layout assertions for GlyphInstance/GroupRow (option b) |

## STEP 0 — baseline

Clean from the start: `cargo build --release` 0 warnings; `--engine-check
src/main.rs` PASS (24,988 records bit-exact vs the CPU reference);
`--repo-scan-only` exit 0 (4 files, 11,168 records → 10,857 instances); four
baseline screenshots in `out/tooling-ab/baseline/` (demo / text / repo-zoom /
repo-wide, 2 frames each, deterministic virtual clock);
`tools/check-stage-g.sh` ALL PASS. `cargo tree -d` duplicates at baseline:
bitflags 1/2, miniz_oxide 0.8/0.9, syn 2/3, block2/objc2 family — all
transitive via wgpu/winit/image, unchanged by every phase below.

## Phase 1 — naga WGSL validation (`8d6eb5a`)

- Dev-dependency `naga = { version = "30", features = ["wgsl-in"] }` →
  resolved **naga 30.0.1**, the exact instance wgpu 30.0.1 already compiles
  (`cargo tree` shows a single naga in the tree). Test-only; the release
  bin's dep tree is unchanged.
- NEW `native/tests/wgsl.rs`: asserts the shader set is exactly
  `{cull, glyph_field, quad_field}.wgsl` (a fourth shader appearing without a
  test update fails), then `naga::front::wgsl::parse_str` +
  `Validator::new(ValidationFlags::all(), Capabilities::all())` per file.
  The test READS shaders; it never modifies them (fence 3).
- Gates: `cargo test` 1/1 green; 4× byte-equal; engine-check PASS; stage-G
  ALL PASS.

## Phase 2 — glam feature flags (`90397c3`)

- `glam = { version = "0.30", features = ["approx", "debug-glam-assert", "mint"] }`
  → resolved **glam 0.30.10** (unchanged), approx 0.5.1, mint 0.5.9.
- **Substitution vs the handoff** (it flagged the names as "believed correct,
  verify before committing"): glam 0.30.10 has no `debug-glam-assertions` —
  the real flag is `debug-glam-assert`; `approx`/`mint` are implicit features
  from optional deps, not listed in `[features]`. Verified against the
  published 0.30.10 manifest in the local registry. `glam-assert`
  (always-on) was deliberately NOT enabled — debug-only validation only.
- All three flags are additive trait impls / debug-only asserts: zero math
  change, zero release-code change. Gates: 4× byte-equal; engine-check PASS;
  stage-G ALL PASS; tests green.

## Phase 3 — clap CLI (`5d23f27`)

- `clap = { version = "4", features = ["derive"] }` → **clap 4.6.6**
  (clap_builder 4.6.6); `clap_complete = "4"` → **4.6.9**. anstream/anstyle
  deps unify with env_logger 0.11's — no new duplicates.
- The hand-rolled `parse_cli()` (self-described "not a product CLI") is
  replaced by a derive `Cli` struct. Every flag preserved with identical
  semantics and defaults, **including `--cam-pose X Y Z YAW PITCH`** (added
  after the handoff's inventory; degrees → radians at parse time, interleaves
  in the op stream) and all debug env vars (`GLYPH_G_DUMP`,
  `GLYPH_CULL_DEBUG`, `GLYPH_PICK_DEBUG` — untouched, they never went through
  the parser).
- **Op-stream order** (the one nontrivial problem): clap stores each flag's
  values separately, so `--pick-file a --verb v --pick-px 1 2` loses its
  interleaving. `build_ops()` reconstructs true CLI order from
  `ArgMatches::indices_of`. Verified against clap_builder 4.6.6 source:
  indices are pushed **per value** (`push_arg_values`: "each value is a
  distinct index to clap"), so multi-value flags (`--pick-px`, `--cam-pose`)
  are chunked by arity and zipped occurrence-by-occurrence. The
  `set_pick_row_col` upgrade semantics (mutate the MOST RECENT `--pick-file`;
  panic `--pick-row/--pick-col must follow --pick-file` otherwise) run
  unchanged over the rebuilt stream.
- Deliberate parity choices: `args_override_self = true` (old parser was
  last-wins on repeated scalar flags; clap's default errors);
  `allow_negative_numbers` on `--cam-pose`/`--pick-px`/`--zoom` (the old
  parser took raw tokens — oblique camera repros use negative coordinates);
  `parse_verb()` is now a `Result`-returning clap `value_parser` with the
  same error messages. `--repo-engine` keeps naive|batch validation via
  `value_parser = ["naive", "batch"]`.
- Help: the old `--help` content (modes, verb reference, fly-camera/interact
  keys) lives in `about`/`after_long_help`; every flag has a doc line.
  `-h`/`--help` exit 0; unknown flag exits 2 (matches old behavior).
  NEW: `--generate <shell>` completions (bash/elvish/fish/powershell/zsh).
- **Parity proof**: 15 unit tests in `main.rs::cli_tests` (defaults, scalar
  flags, last-wins repeats, full op-stream ordering incl. cam-pose and
  interleaved pick-px, row/col upgrade + both panic paths, unknown-flag,
  DisplayHelp, repo-engine rejection, verb forms/defaults/error messages,
  --generate, negative numbers) plus a hand-run matrix (engine-check,
  engine smoke, repo-scan-only, pick→verb→cam-pose→pick-px chain, pick-row
  error exit 101 — all matching old behavior).
- **text.png caveat (documented, renderer-neutral)**: `--render-file
  src/main.rs` renders the binary's own source, so editing main.rs changes
  the render INPUT. Proof of output neutrality: re-rendering the pre-Phase-3
  main.rs (extracted from git) with the Phase-3 binary is **byte-equal** to
  baseline text.png. Phases 4–5 compare text.png against the Phase-3 output
  (same input); demo/repo-zoom/repo-wide compare against the original
  baseline throughout.

## Phase 4 — wgpu-profiler (`0d32eea`)

- `wgpu-profiler = "0.28"` → **0.28.0** (deps: wgpu 30.0.1 — the same tree
  instance; parking_lot 0.12.5 single version).
- **Feature gating**: `gpu::init` requests `TIMESTAMP_QUERY` (+
  `TIMESTAMP_QUERY_INSIDE_PASSES` when the adapter has it) ONLY when
  `GLYPH_PROFILE=1`; without the env var the device is created with
  `Features::empty()` exactly as before and every profiling call site is an
  `Option::None` no-op. Missing adapter support logs once ("profiling
  unavailable: no TIMESTAMP_QUERY") and runs clean — never a panic.
- **Plumbing**: profiler lives as `Option<RefCell<GpuProfiler>>` inside
  `GpuContext` (the `SceneLike` trait signature is unchanged — both scene
  impls borrow it during pass encoding). Pass-level timers use
  `begin_pass_query` → `RenderPassDescriptor.timestamp_writes` (pass-boundary
  writes — valid on Metal). Nested in-pass scopes ("backdrop stream", "glyph
  stream") use `begin_query`/`end_query` on the pass and are active only
  where `TIMESTAMP_QUERY_INSIDE_PASSES` exists; **this adapter (Apple Metal)
  does not expose it**, so nested scopes currently report no time — the
  pass-level number is the measurement here. Scopes: `glyph field pass`
  (GlyphScene: backdrop + glyph streams, one pass by design — fence 3 forbids
  splitting it) and `quad field pass` (Stage A demo). The CPU cull is
  Instant-timed into `ctx.cpu_scopes` as "cull (CPU)".
- **Frame finalization**: `resolve_queries` before submit (adds copy commands
  to the encoder; the render target is untouched), `end_frame` after submit,
  non-blocking `PollType::Poll` + `process_finished_frame` each frame, folded
  into running means (`ProfileAccumulator`). `max_num_pending_frames` raised
  3 → 64: short offscreen runs submit frames faster than their query maps
  complete, and frames beyond the cap are silently dropped unmeasured
  (measured: 3/30 frames without the per-frame drain + cap; **30/30 with**).
- **Output**: `GLYPH_PROFILE=1` extends the once-per-second windowed FPS line
  and the offscreen per-run summary with mean per-pass ms
  (`profile: N frame(s) measured | GPU: … | CPU: …`).
- **Acceptance evidence**: screenshots byte-equal **with profiling ON** (all
  four — timestamp queries do not alter raster output) and with profiling
  OFF; stage-G ALL PASS; engine-check PASS; tests 16/16 green.

### Profiling numbers (release, Apple M2, 1600×1000, offscreen)

Per-pass GPU means, `GLYPH_PROFILE=1`, 30 frames measured per run:

| view | pass | GPU mean | CPU cull mean |
|---|---|---|---|
| demo (1,000,000 quads) | quad field pass | 7.320 ms | — |
| text (32,490 instances) | glyph field pass | 0.135 ms | 0.004 ms |
| repo-zoom (10,857 instances) | glyph field pass | 9.632 ms | 0.000 ms |
| repo-wide (10,857 instances) | glyph field pass | 4.079 ms | 0.000 ms |

Glyph pass ≫ cull pass at repo scale, as expected. repo-zoom > repo-wide is
fragment overdraw (zoom 3 fills the viewport with the same glyphs). Means
over the first frames of a cold run skew high (2-frame runs showed up to
16 ms for the same view); the 30-frame means above are the steady numbers.

FPS unchanged when disabled — 30-frame offscreen repo-wide "steady-state"
prints: pre-profiler binary (Phase-3 commit, built in a git worktree)
**577.9 fps**; Phase-4 binary OFF **557.9 / 957.0 / 1044.3** across runs;
Phase-4 binary ON **649.0 / 1105.3**. This machine's run-to-run variance is
~2× (readback-wait dominated), so ON vs OFF is indistinguishable within
noise — which is the honest claim, backed by the structural one: disabled =
empty feature set + `None` branches, and both states render byte-identically
to baseline.

### Sub-phase 4b (puffin) — SKIPPED (optional)

The handoff marks 4b optional ("skip if it drags"). Skipped to keep the phase
surface minimal; `puffin_egui` remains blocked on its egui 0.33 pin vs the
wgpu-30-compatible egui stack regardless (note 04). Revisit with the egui
stage.

## Phase 5 — encase layout assertions, option (b) (`d6daee7`)

- `encase = "0.12"` → **0.12.1** (+ encase_derive 0.12.1). **Substitution vs
  the handoff**: there is no `features = ["derive"]` — 0.12.1 re-exports
  `encase_derive::ShaderType` unconditionally. Note 03's "no math impls"
  caveat confirmed; not needed here (fields are plain f32/u32 arrays).
- `#[derive(encase::ShaderType)]` on `GlyphInstance` (48 B) and `GroupRow`
  (80 B) — the two hand-mirrored layouts behind the Stage G strided-color
  bug. `glyph_scene.rs::layout_tests` (3 tests) pin:
  - `SHADER_SIZE` == 48 / 80 == `size_of` both ways;
  - every `GlyphInstance` field offset against the WGSL lane map in the
    `glyph_field.wgsl` header (pos 0, glyph_id 12, row 16, col 20, color 24,
    group_id 28, advance 32, height 36, flags 40, _pad 44);
  - **decisive byte proof**: encase's serialization of distinctive bit
    patterns is `assert_eq!`-identical to `bytemuck::bytes_of` for both
    structs — the two representations can never diverge silently again.
- **Option (a) (switching write paths to encase) was evaluated and
  deliberately NOT taken.** The arena staging path uploads
  `Vec<GlyphInstance>` by zero-copy `bytemuck::cast_slice`; encase would
  serialize through an intermediate byte buffer — a full extra copy of the
  arena (measured staging path, repo-scale hundreds of MB) inside the
  determinism chain, in exchange for byte-identical output already proven by
  the tests above. The handoff declares option (b) — write paths unchanged,
  layout assertions added as compile-time documentation — full success; with
  the byte-equivalence test in place it is also the better engineering
  outcome. No A/B divergence was risked; nothing to revert.
- Gates: 4× byte-equal; engine-check PASS; stage-G ALL PASS; tests 19/19
  green; release build 0 warnings (encase adds derive-generated consts only —
  zero runtime cost).

## Verification (gates table)

| gate | baseline | P1 | P2 | P3 | P4 | P5 |
|---|---|---|---|---|---|---|
| `cargo build[ --release]` 0 warnings | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `--engine-check src/main.rs` | PASS | PASS | PASS | PASS | PASS | PASS |
| `tools/check-stage-g.sh` | ALL PASS | ALL PASS | ALL PASS | ALL PASS | ALL PASS | ALL PASS |
| `cargo test` | 0 tests | 1 ✓ | 1 ✓ | 16 ✓ | 16 ✓ | 19 ✓ |
| demo.png `cmp` baseline | — | = | = | = | = (ON & OFF) | = |
| repo-zoom.png `cmp` baseline | — | = | = | = | = (ON & OFF) | = |
| repo-wide.png `cmp` baseline | — | = | = | = | = (ON & OFF) | = |
| text.png `cmp` | — | = | = | input changed† | = (ON & OFF)‡ | = ‡ |
| `cargo tree -d` new dups | — | none | none | none | none | none |

† Phase 3 edited main.rs, which IS the text.png render input; the Phase-3
binary re-rendering the pre-Phase-3 main.rs is byte-equal to baseline
(renderer-neutral proof). ‡ Phases 4–5 cmp text.png against the Phase-3
output (identical input). Screenshot dirs: `out/tooling-ab/{baseline,
phase1-naga, phase2-glam, phase3-clap, phase4-off, phase4-profiler,
phase5-encase}/`.

## File diffs (by phase)

- P1: `native/Cargo.toml` (naga dev-dep), `native/Cargo.lock`, NEW
  `native/tests/wgsl.rs`.
- P2: `native/Cargo.toml` (glam features), `native/Cargo.lock`.
- P3: `native/Cargo.toml` (clap, clap_complete), `native/Cargo.lock`,
  `native/src/main.rs` (derive Cli + RawOps, build_ops, Result parse_verb,
  --generate, AFTER_LONG_HELP, 15-test cli_tests module; hand-rolled parser
  deleted).
- P4: `native/Cargo.toml` (wgpu-profiler), `native/Cargo.lock`,
  `native/src/gpu.rs` (feature-gated TIMESTAMP_QUERY request, profiler
  construction, ProfileAccumulator, CPU-scope helpers),
  `native/src/glyph_scene.rs` (pass + nested stream scopes, CPU cull
  timing), `native/src/scene.rs` (quad field pass scope),
  `native/src/offscreen.rs` (resolve/end_frame/per-frame drain, profile
  summary), `native/src/windowed.rs` (same plumbing + FPS line suffix).
- P5: `native/Cargo.toml` (encase), `native/Cargo.lock`,
  `native/src/glyph_scene.rs` (ShaderType derives + layout_tests module).

No changes to: `engine-local/`, `assets/atlas/`, `src/shaders/*.wgsl`,
staging math (`text.rs`/`repo.rs`), buffer write paths, render order, or any
version pin (fence 1–4 compliance).

## Remaining gaps (after Stage H)

1. **glam 0.30 → 0.33 bump** — still the strategic lever (unlocks trackball,
   tween, bevy_math, transform-gizmo, crevice). Do it as its own mini-stage
   per fence 2's right-shape: bump, mechanical call-site sweep, full A/B,
   then byte-identity declaration or a documented re-baseline.
2. **egui stage** — next stage, untouched here (fence 4). `puffin_egui`
   remains blocked on its egui 0.33 pin vs the wgpu-30-compatible egui stack;
   re-check after a puffin_egui release targeting egui 0.36.
3. **In-pass GPU timing granularity** — nested stream scopes are plumbed but
   report no time on Metal (no TIMESTAMP_QUERY_INSIDE_PASSES). Pass-level
   numbers only until a backend exposes it; splitting the pass for timing's
   sake is forbidden (fence 3).
4. **Windowed profile line** — exercised only via the shared plumbing (the
   offscreen path uses the same resolve/end_frame/process code); not
   interactively smoke-tested in this pass.
5. **text.png baseline is input-sensitive** — any future stage that edits
   `src/main.rs` shifts it; re-render the old input (git show) to keep the
   renderer-neutrality proof, or re-baseline explicitly.
6. **2-frame screenshot FPS prints are noise** — the byte-equal gates don't
   care, but perf claims should use ≥30-frame runs (this report's table does).
7. **clap exit-code nuance** — missing flag values / malformed numbers now
   exit 2 with clap's message instead of the old panic (101); behavior for
   valid invocations is identical and the pick-row/col misuse path keeps its
   exact panic (pinned by tests).
