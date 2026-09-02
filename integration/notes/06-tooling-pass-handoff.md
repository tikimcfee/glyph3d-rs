# 06 — TOOLING PASS HANDOFF (Stage H) — for the implementing agent

> **STATUS: EXECUTED — all five phases landed clean (Sep 2026).** Commits
> `8d6eb5a` (naga) · `90397c3` (glam flags) · `5d23f27` (clap) · `0d32eea`
> (wgpu-profiler) · `d6daee7` (encase, option b). Full evidence and caveats:
> `glyph3d-native/out/STAGE_H_REPORT.md`. **Do not re-execute this doc** — it
> remains as the spec record. Corrections learned in execution (also folded
> into note 03): glam's debug flag is `debug-glam-assert` (not
> `debug-glam-assertions`); `approx`/`mint` are implicit features; encase
> 0.12.1 has no `derive` feature (re-exported unconditionally); the CLI
> carried a `--cam-pose` flag this doc's inventory missed; Metal lacks
> `TIMESTAMP_QUERY_INSIDE_PASSES` (nested profiler scopes inert — pass-level
> timings are the measurement). Two handoff-relevant caveats for FUTURE
> stages: `--render-file src/main.rs` makes text.png an *input-sensitive*
> baseline (a stage that edits main.rs must re-render the old input from git
> for the neutrality proof, or re-baseline explicitly), and perf claims need
> ≥30-frame runs (2-frame numbers are readback noise).

*You are picking up a scoped engineering task. This document is written to be executed
directly without further context, but companion notes live beside it:
`00-primitives-inventory.md` (what every module is), `03-graphics-helpers.md` (why these
crates), `05-integration-postures-and-roadmap.md` (the strategy). Read `00` before
touching anything.*

## Who wrote this, and how much to trust it

This handoff came out of a research/documentation pass, not the build: a full read of
`native/src`, the stage reports, and `engine-local/README.md`, plus an ecosystem survey
in which every crate version/date/dependency claim was verified against
crates.io/docs.rs/GitHub on 2026-09-01. It was **not** written by whoever built the
renderer — you were. That division of knowledge cuts both ways:

- **You know the code from A–Z; this document does not.** Where a claim here about
  your own code conflicts with what you know (a flag's exact semantics, a script's
  internals, a path), your ground truth wins. Treat the conflict as a thing to
  double-check against the acceptance gates below — not as license to skip them.
- **This document knows things you haven't had time to learn**: which crates are
  alive and version-compatible as of Sep 2026, which are known traps (a crate named
  `cameras` that is actually video capture; `puffin_egui`'s egui pin conflict;
  three-d having no wgpu backend at all), and the reasoning behind every fence. The
  fences each carry their reasoning inline so you can push back with arguments rather
  than guesses — every one of them exists to protect your own verification
  discipline, not to constrain engineering judgment.

## Mission

Adopt the "tooling tier" of ecosystem crates into `glyph3d-native` — GPU pass profiling,
WGSL validation in tests, a real CLI, and math-library assertion features — WITHOUT
perturbing the renderer's output or the verified text pipeline. Every phase is
independently shippable, gated by byte-identical render A/Bs.

## The workspace

- Crate: `/Users/lugo/localdev/viz-native/glyph3d-native/native/` (Cargo.toml, src/, shaders/)
- Repo scripts: `/Users/lugo/localdev/viz-native/glyph3d-native/tools/`
- Fixture repo for runs: `native/fixtures/g-pick-repo/`
- Stage reports (house style to follow): `out/STAGE_G_REPORT.md` is the latest
- Stack pins: **wgpu 30 (locked line), winit 0.30, glam 0.30, edition 2021**

## HARD FENCES — each with its reason

1. **DO NOT touch** `engine-local/`, `assets/atlas/`, or anything that rebuilds the
   Mojo engine. `native/libglyph_engine.dylib` is prebuilt and linked by `build.rs`;
   you never need pixi/mojo.
   *Why:* the engine has its own bit-exact contract (JS oracle, kind-split arrays,
   float discipline — see engine-local/README.md) and its own toolchain; nothing in
   this pass needs it, and `--engine-check` PASS is the proof you didn't disturb it.
2. **DO NOT bump wgpu, winit, or glam versions in this pass.** glam gets *feature
   flags only*. New deps must not force any existing dep across a semver boundary —
   check `cargo tree -d` after each phase.
   *Why (this is the fence that looks arbitrary, so here is the whole thought
   process):*
   - The acceptance bar for every phase below is **byte-identical screenshots**, and
     glam is the only chore in this pass that is not provably output-neutral. Your
     pixels are computed from CPU-side glam math (view-proj products, frustum planes,
     pick rays) uploaded to the GPU as raw f32 — a one-ULP difference in a glam
     implementation is a subpixel shift in every rasterized glyph, and `cmp` fails.
   - glam DOES promise "bit-for-bit identical results on all platforms" by default
     (their README) — that guarantee is exactly why your byte-equal screenshot gates
     are sound in the first place. But it is a **per-version guarantee, not a
     cross-version one**: nothing promises 0.30 and 0.33 produce identical bits for
     the same inputs, and version boundaries are when implementation-internal changes
     ride in. Example: 0.33.2 moved look_at/perspective into a new `camera` module;
     the free functions *should* be the same math as the deprecated methods, but
     "should" is not a bit-exactness standard. glam's `fast-math` caveat likewise
     documents that they reserve the right to reorder float ops when optimizing.
   - **One variable per gate.** Each phase is one commit gated by byte-equality. If a
     glam bump rides along and a screenshot diverges, you cannot attribute the
     divergence without bisection — the phase structure exists so that work never
     happens.
   - **Re-baselining is a conscious act.** The stage lineage (E2 → F → G) is a chain
     of documented byte-identity proofs. If the bump shifts pixels, that's
     acceptable — but only if someone characterizes it (pixel count, max per-channel
     delta, plausible cause) and records a new authoritative baseline in a stage
     report. Inside a tooling pass it would be an undocumented broken chain.
   - What the fence is **not**: fear of 0.33. Your dep tree is clean (nothing else
     consumes glam — verified), the bump is a one-line Cargo.toml change plus a
     mechanical call-site sweep (mostly the two `camera`-module renames), and nothing
     in THIS pass needs it — the crates it unlocks (trackball, tween,
     transform-gizmo, bevy_math) belong to the later camera/egui stages anyway.
   - When the bump IS done, right shape: **its own mini-stage** — bump, fix call
     sites, run the full A/B suite, then either declare byte-identity or re-baseline
     with the divergence documented in the report.
3. **The determinism chain is sacred**: byte → engine → record → instance → pixel is
   gated by bit-exact verification. You may not change: buffer byte layouts, staging
   math (text.rs / repo.rs staging paths), shader source, or render order.
   *Why:* the A/B gates below can only prove "no change" for changes you didn't make;
   this fence keeps the pass inside what those gates can actually verify.
4. **NO UI work** (egui/iced/anything) — that's the next stage, explicitly out of
   scope. *Why:* egui changes input routing, which would blur the byte-equal gates;
   mixing it here would force the attribution problem fence 2 describes.
5. **Zero-warning discipline**: `cargo build` and `cargo build --release` must finish
   with 0 warnings (house rule, see stage reports).
6. Output artifacts: new screenshots/debug logs go in `out/` (it's the established
   scratch dir). Your final report goes to `out/STAGE_H_REPORT.md` in the style of
   `out/STAGE_G_REPORT.md` (Goal / Result / per-phase detail / verification / file
   diffs). *Why:* keeps this pass inside the repo's existing history conventions, so
   the next stage can cite it the way this pass cites Stage G.

## STEP 0 — baseline (before ANY edit)

Build the current HEAD and record the verification state. If any of these fail at
baseline, STOP and report — do not build on a broken base.

```bash
cd /Users/lugo/localdev/viz-native/glyph3d-native/native
cargo build --release                                   # expect: clean, 0 warnings
cargo run --release -- --engine-check src/main.rs       # expect: PASS (engine ↔ CPU oracle)
cargo run --release -- --repo-scan-only --load-repo fixtures/g-pick-repo   # expect: stats, exit 0
```

Then record **baseline screenshots** (offscreen mode is deterministic — fixed virtual
clock — so these must remain byte-identical through every phase):

```bash
mkdir -p ../out/tooling-ab/baseline
cargo run --release -- --demo --frames 2 \
  --screenshot ../out/tooling-ab/baseline/demo.png
cargo run --release -- --render-file src/main.rs --frames 2 \
  --screenshot ../out/tooling-ab/baseline/text.png
cargo run --release -- --load-repo fixtures/g-pick-repo --focus-file alpha --zoom 3 --frames 2 \
  --screenshot ../out/tooling-ab/baseline/repo-zoom.png
cargo run --release -- --load-repo fixtures/g-pick-repo --frames 2 \
  --screenshot ../out/tooling-ab/baseline/repo-wide.png
```

Also run `tools/check-stage-g.sh` once at baseline (it builds and runs the pick
correctness gate) and record its output.

After EVERY phase, regenerate the same four screenshots into `out/tooling-ab/<phase>/`
and `cmp` each against baseline — **byte-equal is the acceptance bar** — plus re-run
the engine-check and the stage-G script.

---

## Phase 1 — naga: WGSL validation in `cargo test` (lowest risk; do first)

**Why:** the three hand-written shaders (`src/shaders/{glyph_field,cull,quad_field}.wgsl`)
are currently only validated when a render happens. A unit test pins them against the
exact compiler wgpu 30 bundles (naga 30.0.1 — wgpu 30's own dependency, so version
pairing is automatic).

**Do:**
- Add dev-dependency: `naga = { version = "30", features = ["wgsl-in"] }`.
  (dev-dependency only — it must not ship in release builds' dep tree for bins… it
  still compiles for tests only; do NOT add it as a normal dependency.)
- New test (e.g. `native/src/shaders/mod.rs` with `#[cfg(test)]`, or
  `native/tests/wgsl.rs`): for each `.wgsl` under `src/shaders/`:
  `naga::front::wgsl::parse_str` → assert no parse errors → validate with
  `naga::valid::Validator` per the docs.rs 30.0.1 API (check the exact constructor
  signature for that version; default options/capabilities are fine) → assert valid.
- Assert the file list too (fail if a shader file exists that the test didn't cover).

**Accept:** `cargo test` green with 3 validated shaders; nothing else changes;
screenshots byte-equal; release build unchanged.

## Phase 2 — glam feature flags (trivial; do second)

**Why:** free validation and interop without a version bump.

**Do:** in `native/Cargo.toml`, extend the glam entry with features. Believed correct
names for 0.30.10 — **verify on docs.rs/crate/glam/0.30.10/features before committing**:
- `approx` — epsilon-compare traits (useful for the oracle-style tests; do NOT rewrite
  existing exact-compare logic — bit-exactness stays the standard)
- `debug-glam-assertions` — validation asserts compiled in debug builds only
- `mint` — interop shim (no code needed; makes future interop crates trivial)

**Accept:** builds clean; screenshots byte-equal; `cargo tree` shows glam still 0.30.x.

## Phase 3 — clap: replace the hand-rolled CLI

**Why:** `parse_cli()` (main.rs:257) is self-described as "not a product CLI." clap
derive gives generated `--help`, errors, and shell completions. A documented CLI is
also the primary interface scripted agents use — this is an agent-ergonomics feature.

**Do:**
- Add `clap = { version = "4", features = ["derive"] }`.
- Convert the `Cli` struct to clap derive. Preserve **every** flag with identical
  semantics and defaults — full inventory:
  - Modes/scene: `--demo`, `--render-file <path>`, `--copies <n>` (default 1),
    `--zoom <f>` (default 1.0)
  - Offscreen: `--screenshot <path>`, `--frames <n>` (default 1)
  - Engine: `--engine-file`, `--engine-trie`, `--engine-loop <n>` (default 1),
    `--engine-check`, `--engine-render`
  - Repo: `--load-repo <dir>`, `--repo-engine <naive|batch>` (default naive — keep the
    value validation, clap `value_parser` for the enum), `--repo-verify`,
    `--focus-file <substr>`, `--repo-scan-only`
  - Cull: `--no-cull`
  - Stage G op stream (order matters — `Vec<Op>` via `ArgAction::Append`, which
    preserves CLI order): `--pick-file <substr>`, `--pick-row <n>`, `--pick-col <m>`,
    `--pick-px <x> <y>`, `--verb "<verb string>"`
  - Keep `parse_verb()` as a clap `value_parser` (its error messages are good).
- **Re-implement, do not lose, the `set_pick_row_col` semantics**: `--pick-row`/
  `--pick-col` upgrade the MOST RECENT `--pick-file`, and error if none precedes them.
  Easiest correct approach: collect the raw ops via clap as today, then run the
  existing post-processing function over them. Prove parity with a test.
- Help text: the old `--help` content (mode summary + the fly-camera/interaction key
  reference) moves into `about`/`after_long_help` — nothing user-facing is lost, and
  every flag gets a doc-comment line.
- Behavior parity: `-h`/`--help` exits 0; unknown flag exits nonzero with a clear
  message (clap defaults to exit 2 — matches current behavior).
- Add `--generate <shell>` completions (clap_complete) — cheap and agent-friendly.

**Accept:** `--help` shows all flags documented; a parity test (or hand-run matrix)
over ~10 representative invocations matches old behavior (including the
pick-row/col error path); all gates green; screenshots byte-equal.

## Phase 4 — wgpu-profiler: per-pass GPU timings

**Why:** stage reports currently rely on wall-clock FPS. Timestamped per-pass timings
(cull, glyph field, backdrop) make every future report quantitative. wgpu-profiler
0.28.0 is verified against `wgpu ^30.0` + `winit ^0.30` (see note 03).

**Do:**
- Add `wgpu-profiler = "0.28"`. Read its docs.rs/examples first; the API notes below
  are shape, not gospel — trust the crate's docs for exact signatures.
- **Feature gating (important):** timestamp queries need
  `wgpu::Features::TIMESTAMP_QUERY`. In `gpu.rs::init`, only request the feature when
  the adapter supports it, and construct the profiler only then; otherwise log once
  ("profiling unavailable: no TIMESTAMP_QUERY") and run clean with no profiler. A
  missing feature must never panic or crash a render.
- **Plumbing:** `SceneLike::render` takes `&GpuContext`, so the profiler (which is
  per-frame mutable state that persists across frames) fits best as a
  `RefCell<GpuProfiler>` (or `Option<RefCell<..>>`) inside `GpuContext` — borrowed
  during pass encoding. Avoid changing the `SceneLike` trait signature if you can;
  if you must, change it minimally and update both implementations (`Scene`, `GlyphScene`).
- **Scopes:** wrap the passes inside `GlyphScene::render` — (a) cull/backdrop setup,
  (b) glyph field pass(es) per chunk, (c) backdrop pass — plus the Stage A demo pass
  in `Scene::render`. Offscreen readback stays out of scopes.
- **Output:** opt-in via `GLYPH_PROFILE=1` env var (house style — see `GLYPH_G_DUMP`,
  `GLYPH_CULL_DEBUG`). When enabled, extend the once-per-second FPS line in
  `windowed.rs` (and the offscreen per-run summary) with mean per-pass ms. When
  disabled, zero overhead beyond an `enabled` check.
- **Optional sub-phase 4b (puffin):** enable wgpu-profiler's `puffin` feature +
  `puffin_http` server behind `GLYPH_PROFILE_PUFFIN=<port>`, viewable with standalone
  `puffin_viewer`. Do NOT add `puffin_egui` — its current release pins egui 0.33 which
  conflicts with the wgpu-30-compatible egui-wgpu (see note 04). Sub-phase is optional;
  skip if it drags.

**Accept:** with `GLYPH_PROFILE=1`, per-pass timings print and are plausible (glyph
pass ≫ cull pass at repo scale); WITHOUT the env var, behavior and output are
identical to baseline; screenshots byte-equal **with profiling on** (timestamp
queries must not alter raster output — prove it); stage-G script green.

## Phase 5 — (stretch, only if 1–4 landed clean) encase layout codegen

**Why:** the Stage G "strided-color bug" class (hand-mirrored layouts between Rust
structs and WGSL) is exactly what encase's generated offsets catch at compile time.
encase 0.12 ships NO math impls — you implement `ShaderType` for fixed-array structs,
which is mechanical. See the correction note in `03-graphics-helpers.md`: this is NOT
debug-only tooling; it's permanent layout code with ~zero runtime cost.

**Do (strictly gated):**
- `encase = { version = "0.12", features = ["derive"] }`.
- `#[derive(ShaderType)]` on `GlyphInstance` (48 B) and `GroupRow` (80 B); add tests
  asserting `ShaderType::SIZE` == 48/80 and the field offsets match the WGSL comments'
  lane map.
- **You may switch the buffer-write paths to encase only if** the full A/B suite
  (all four screenshots, plus `tools/check-stage-g.sh`, plus a `--verb` smoke:
  one recolor + one move-group, screenshot-compared) is byte-identical to baseline.
  If anything diverges by a single byte, revert to the bytemuck path and keep only
  the layout assertions as compile-time documentation. Report either outcome honestly.

**Accept:** either (a) encase write-paths landed with byte-identical renders, or
(b) write-paths unchanged with layout assertions added — both are success; silent
divergence is the only failure.

---

## Final checklist for the stage report (`out/STAGE_H_REPORT.md`)

- Goal/Result header in the house style; per-phase sections with the crate versions
  actually resolved (report `cargo tree` versions, not Cargo.toml ranges).
- Verification: gates table (engine-check, stage-G script, 4× screenshot `cmp` results
  per phase, `cargo test`), plus profiling numbers before/after (FPS unchanged within
  noise when profiler disabled; per-pass ms table when enabled).
- File diffs section listing every touched file.
- "Remaining gaps" section (house convention) — likely: glam bump, egui stage,
  puffin_egui blocked on egui 0.36.

Suggested commit cadence: one commit per phase, gates run per commit.
