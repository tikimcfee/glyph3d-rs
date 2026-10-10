# Rust Developer Tooling Survey — Late 2025 / 2026

> **Background survey, not policy.** What the repo actually uses and requires
> is in `.agents/rules/rust-engineering.md` and root `AGENTS.md` (there is,
> for instance, no tracy integration).
## For a native rendering engine codebase (wgpu / wgpu-native style, Rust, wasm32/web targets)

Version dates below were verified against crates.io API and GitHub release pages (checked 2026).

---

## 1. Logging & Observability

| Tool | What it does | Status (2025–26) | Why it matters for wgpu-native + wasm |
|---|---|---|---|
| **tracing / tracing-subscriber** | Structured, span-based logging/diagnostics framework; the de facto Rust standard | **Active** (tracing-subscriber 0.3.23, 2026-03) | wgpu itself emits `tracing`/`log` events; structured spans map naturally to frame/render-pass boundaries. Use `tracing-wasm` on wasm32 to route to the browser console. |
| **tracing-chrome** | Emits Chrome Trace Event JSON viewable in `chrome://tracing` / Perfetto | **Stale** (0.7.2, 2024-03; works but quiet) | Cheap frame-timeline visualization for native runs; Perfetto's UI is excellent for render pipeline spans. Fine to use, but don't expect rapid fixes. |
| **tracing-tracy** | Bridges tracing spans into the Tracy frame profiler | **Active** (0.12.0, 2026-08) | Tracy is arguably the best frame profiler for a renderer: per-frame zones, GPU context support, low overhead. Strong pick for the native engine. |
| **puffin** | Embark's lightweight instrumentation CPU profiler; in-app egui flamegraph or `puffin_http` + `puffin_viewer` | **Active** (0.20.0, 2026-03) | Purpose-built for game/render loops (`finish_frame!()`). `puffin_egui` gives you an in-engine profiling overlay with near-zero plumbing. |
| **optick** | Instrumented profiler with Windows-only GUI | **Dead** (1.3.4, 2020-08; upstream repo unmaintained) | Do not adopt. Replace with Tracy (via tracing-tracy/tracy-client) or puffin. |
| **profiling** crate | Thin macro abstraction over puffin/optick/tracy/superluminal/tracing | **Active** (1.0.18, 2026-05) | Lets you instrument once (`#[profiling::function]`) and swap backends per feature flag — ideal for an engine that wants Tracy locally and tracing in CI. |
| **cargo-flamegraph** | perf/DTrace sampling → flamegraph SVG | **Maintained but legacy** | Still works on Linux/macOS, but needs sudo/perf permissions and DTrace is painful on modern macOS. |
| **samply** | Sampling profiler recording to Firefox Profiler format; Mac/Linux/Windows, no sudo | **Active/mature** (0.13.1; teams are actively migrating to it — e.g. "switched to samply, no sudo required") | **2025 modern replacement for cargo-flamegraph.** First stop for "why is this frame slow" on native builds. |

**Modern replacements:** `samply` over `cargo-flamegraph`; Tracy/`tracing-tracy` over optick; `profiling` crate as the abstraction layer.

---

## 2. Web / Wasm Compiling Tooling

| Tool | What it does | Status | Why it matters |
|---|---|---|---|
| **wasm-pack** | One-stop build/test/packaging for Rust→wasm | **Sunset.** rustwasm org archived Sept 2025; repo transferred to drager's personal account; installer links 404'd. Not formally deprecated but stewardship is uncertain | Do not build new workflows on it. Its real jobs (install wasm32 target, match wasm-bindgen-cli version, run wasm-opt, drive test runners) are easily replicated. |
| **wasm-bindgen / wasm-bindgen-cli** | JS/TS bindings generation for wasm32-unknown-unknown | **Active** (CLI 0.2.127, 2026-08); transferred out of rustwasm to a dedicated org with new maintainers | The core of your web target. Invoke `wasm-bindgen-cli` directly (or via trunk); pin CLI version == library version. |
| **trunk** | WASM web app bundler/dev server (like Vite for Rust); orchestrates cargo build + wasm-bindgen + wasm-opt + assets | **Active** (0.21.x stable, 0.22.0-beta mid-2026) | **The wasm-pack successor for app-style web targets.** Dev server with reload, SRI hashing, wasm-opt pipeline built in. For a library/module consumed by JS, plain `cargo build --target wasm32-unknown-unknown` + `wasm-bindgen-cli` in a `just` recipe is equally valid. |
| **wasm-bindgen-test + wasm-bindgen-test-runner** | Test harness for running Rust tests in Node/headless browsers via WebDriver | **Active** (lives in wasm-bindgen repo) | The only practical way to CI-test your wasm build in a real browser (Chrome/Firefox/Safari headless). Configure via `.cargo/config.toml` runner; `wasm_bindgen_test_configure!(run_in_browser)`. |
| **wasm-opt (binaryen)** | wasm size/speed optimizer | **Active** (binaryen upstream) | Standard post-link step; `-O2`/`-Oz` for release wasm. Trunk wires it in automatically. |
| **cargo component** | Builds Wasm Component Model / WASI p2 components | **Active** (Bytecode Alliance) | Mostly irrelevant for a browser WebGPU target (which is wasm32-unknown-unknown core wasm), but relevant if you ever ship engine plugins as components. |
| **twiggy** | Code-size profiler for wasm binaries (dominators, top items, garbage) | **Alive, slow cadence** (0.8.0, 2025-06) | The go-to for "why is my wasm 8 MB" — pair with `cargo build -Z build-std` size work and `wasm-opt`. Note: naga's shader parser/validator is a common size hog in wgpu wasm builds; twiggy will show you exactly that. |
| **cargo build flags** | — | — | For wasm size: `opt-level = "z"`, `lto = true`, `codegen-units = 1`, `panic = "abort"` (where possible), `strip = true`. Baseline target: `wasm32-unknown-unknown`; `wasm32v1-none` exists for no-bulk-memory minimal output but wgpu-web needs the standard target. |

**Modern replacements:** trunk (or explicit wasm-bindgen-cli scripts) over wasm-pack; wasm-bindgen-test directly over `wasm-pack test`.

---

## 3. Static Analysis & Code Quality

| Tool | What it does | Status | Why it matters |
|---|---|---|---|
| **clippy** | Official linter; pedantic/nursery lint groups | **Active** (ships with Rust) | For an engine codebase: enable `clippy::pedantic` selectively (allow the noisy ones — `cast_possible_truncation` etc. will fight your f32/u32 index math), keep `nursery` on a warning tier. `clippy::undocumented_unsafe_blocks` is high-value for FFI/wgpu-native code. |
| **cargo-deny** | Policy gate: RustSec advisories + license allowlists + crate bans + source registries + duplicate versions | **Active** (Embark) | **The 2025 superset pick.** One config replaces cargo-audit + cargo-license checks; `bans` catches duplicate `wgpu`/`naga`/`winit` versions sneaking into your tree (real binary-size and UB-adjacent issue for gfx stacks). |
| **cargo-audit** | Lockfile vs RustSec advisory DB | **Active** (RustSec) | Redundant if you run cargo-deny's `advisories` check; keep only if you want its `--deny warnings` semantics for unmaintained crates. Otherwise drop it. |
| **cargo-semver-checks** | Detects API-breaking changes via rustdoc JSON | **Active** (0.50.0, 2026-08) | Essential if you publish the engine as crates (native + wasm consumers). Wired into release-plz automatically. |
| **cargo-machete** | Finds unused dependencies (fast, heuristic) | **Active** (0.9.2, 2026-04) | Works on stable; great in CI. Slight false-positive risk with feature-gated or macro-used deps — keep an ignore list. |
| **cargo-udeps** | Finds unused deps (precise) | **Active** (0.1.61, 2026-04) but **nightly-only** | More accurate than machete; run periodically on nightly rather than gating every PR. |
| **dylint** | Custom lints as dynamic libraries | **Active** (6.0.4, 2026-08) | Overkill unless you need project-specific rules (e.g. "no raw wgpu handle escaping module X"); clippy config covers 95%. |
| **cargo-deadlinks** | Checks rustdoc for broken links | **Quiet but functional** | Cheap CI step if you publish docs for the engine API. |
| **cargo-hack** | Feature-powerset builds (`--each-feature`, `--feature-powerset`), MSRV checks | **Active** (0.6.45, 2026-05) | Critical for a crate with native/wasm/backend feature matrices — catches feature-combination breakage that normal CI misses. |
| **cargo-minimal-versions** | Verifies lower bounds of dep version ranges | **Active** (taiki-e) | Catches "compiles on my lockfile but not on a fresh minimal resolve" bugs in published crates. |
| **Miri** | UB detector (interpreter) for unsafe Rust | **Active** (rust-lang) | **Not feasible for actual wgpu GPU paths** — Miri can't do GPU FFI/driver calls. It *is* valuable for the engine's pure-Rust core (scene graph, glyph layout, allocators, unsafe buffer-packing code). Scope it: `cargo miri test` on library crates with `#[cfg(not(miri))]` around GPU-touching tests. |
| **Sanitizers (ASan/TSan)** | `-Zsanitizer=address/thread` on nightly | **Active** (rustc) | Native-only (not wasm). Worth a periodic CI job for the unsafe buffer/FFI code around wgpu-native. TSan is useful if you have custom worker-thread pools feeding the render thread. |
| **cargo-fuzz (libFuzzer) / cargo-afl** | Coverage-guided fuzzing | **Active** | High value for shader/parsing-adjacent code and any format the engine ingests (glyph data, scene serialization). wgpu itself fuzzes naga; you should fuzz your own asset decoders. |
| **proptest** | Property-based testing | **Active** | Good fit for layout/math invariants (glyph metrics, transform round-trips, packing invariants) — pairs with your byte-identical A/B verification style of testing. |

**Modern replacements:** cargo-deny over cargo-audit (superset); cargo-machete (stable, fast) for routine unused-dep checks with cargo-udeps as the nightly deep-clean.

---

## 4. Testing / CI Speed

| Tool | What it does | Status | Why it matters |
|---|---|---|---|
| **cargo-nextest** | Process-per-test runner: parallel, isolated, retries, partitioning, JUnit output | **Active** (0.9.143, 2026-08) | **2025 default test runner.** Process isolation matters when a GPU test can crash/abort the whole harness; flaky-render-test retries and CI partitioning are built in. Used by wgpu-adjacent projects (e.g. Ruffle's CI). |
| **cargo-insta / insta** | Snapshot testing with review UI (`cargo insta review`) | **Active** (1.48.0, 2026-06; nextest-aware) | Directly relevant: snapshot serialized scene state, glyph-layout output, shader-reflection data. For pixel output itself, insta stores text/structured snapshots — pair it with an image-comparison harness (wgpu's own approach: render → readback → compare against golden PNGs) for true visual regression. |
| **cargo-llvm-cov** | Source coverage via LLVM instrumentation | **Active** (taiki-e) | **Replacement for cargo-tarpaulin.** Instruments and runs tests in a single pass (tarpaulin recompiles separately — measured ~377s of redundant compile in one report); integrates with nextest: `cargo llvm-cov nextest`. |
| **cargo-tarpaulin** | Coverage (older approach) | **Effectively superseded** | Skip; use cargo-llvm-cov. |
| **sccache** | Compiler cache (shared backend: S3/GCS/Azure) as `RUSTC_WRAPPER` | **Active** (0.17.0, 2026-07, Mozilla) | Biggest single CI win for a heavy Rust workspace: one measured pipeline went ~1700s → ~630s warm. Set `CARGO_INCREMENTAL=0` when using it. |
| **cargo-chef** | Docker layer caching via dependency "recipe" pre-build | **Active** (0.1.78, 2026-08) | Standard for containerized CI; if you're on GitHub Actions runners rather than Docker, `Swatinem/rust-cache` or sccache covers the same ground. |

**Modern replacements:** nextest over `cargo test`; cargo-llvm-cov over tarpaulin; sccache (shared storage) over per-runner target-dir caching for big workspaces.

---

## 5. Rendering / wgpu-Specific

| Tool | What it does | Status | Why it matters |
|---|---|---|---|
| **wgsl-analyzer** | LSP for WGSL (diagnostics, completion, go-to-def; naga-backed) | **Very active** (releases through 2026; WESL import support in progress; Naga v29 support) | Install for everyone touching shaders. The 2025-11 release brought an overhauled error-recovering parser and reworked type checking. Note: naga_oil preprocessor support was *removed* in favor of upcoming WESL support — plan accordingly if you use shader composition. |
| **naga-cli** | CLI shader validator/translator (WGSL↔SPIR-V/MSL/HLSL/GLSL) | **Active** — naga moved into the gfx-rs/wgpu monorepo (standalone naga repo archived Jan 2025); `cargo install naga-cli` | CI shader validation: `naga shader.wgsl` fails the build before runtime. Get WGSL from the wgpu repo, not the archived naga repo. |
| **wesl-rs / wesl-js** | Community WGSL module/linking system (`import`, conditional compilation) | **Active** (wgsl-tooling-wg) | If your shader count grows, WESL is becoming the community standard for WGSL modularity; naga_oil (Bevy) is the alternative. |
| **wgsl-bindgen / wgsl_to_wgpu** | Generate type-safe Rust bindings (structs, bind group layouts) from WGSL with layout const-assertions | **Active** (wgsl-bindgen updated 2025-12) | Eliminates the CPU/GPU struct-layout drift class of bugs — the exact class behind byte-identical verification work. Strongly consider for uniform/storage buffer definitions. |
| **wgpu's own harness** | `wgpu-info` (adapter/feature dump), examples framework with screenshot reference tests | **Active** (wgpu 29.x, 2026) | `wgpu-info` for environment triage; the examples' render→readback→golden-image pattern is the template for your own visual regression tests. |
| **RenderDoc integration** | Frame capture/debugger | **Active** upstream; wgpu exposes a `renderdoc` feature for programmatic capture triggers | The standard GPU debugger for Vulkan/DX12/GL backends — trigger captures on keypress or on test failure for frame-level inspection. |
| **PIX on Windows** | D3D12 capture/profiling | **Active** (Microsoft) | Use for DX12-backend-specific issues; wgpu works fine under PIX's automatic capture. |
| **Xcode Metal debugger / gpu-capture** | Metal frame capture | **Active** (Apple) | For the Metal backend on macOS/iOS. |
| **gpu-allocator / wgpu resource tooling** | — | — | Note: `gpu-allocator` is for ash/Vulkan, *not* wgpu (wgpu does its own allocation via `wgpu-core`). For memory insight in wgpu use `wgpu::Device` internal counters + RenderDoc's resource view; there is no separate allocator tool to adopt. |

---

## 6. Release / Versioning Tooling

| Tool | What it does | Status | Why it matters |
|---|---|---|---|
| **cargo-release** | Local, interactive release workflow: bump, changelog, tag, publish; workspace-aware | **Active** (1.1.5, 2026-08) | Best when a human drives releases from a terminal. |
| **release-plz** | CI-first releases: maintains a release PR (changelog via git-cliff, semver check via cargo-semver-checks), publishes on merge | **Active** (0.3.160, 2026-07) | **The 2025 default for team repos.** Reviewable release PRs + automatic semver-breaking detection fit a multi-maintainer engine repo. |
| **cargo-dist (now `dist`)** | Multi-platform release binaries, installers, checksums, Homebrew/npm, GitHub Releases from one config | **Active again** — the 2025 "unmaintained" scare reversed: axodotdev resumed development (0.29→0.32 through 2025–26); the astral-sh fork was archived 2025-12 with upstream absorbing its changes | If you ship engine binaries/demos/tools (e.g. your repro tool), dist generates the whole matrix build + installer story. Renamed to `dist` (0.28.1+) and no longer requires Cargo for non-Rust projects. |
| **git-cliff** | Changelog generation from conventional commits (Jinja-style templates) | **Active** (2.14.1, 2026-09) | The changelog engine underneath release-plz and usable standalone. Adopt conventional commits and this is free documentation. |

**Modern replacements:** release-plz (+ git-cliff + cargo-semver-checks) as the CI-native stack over manual `cargo release`; dist recovered from its 2025 maintenance scare and remains the binary-distribution pick.

---

## 7. Meta / Developer Workflow

| Tool | What it does | Status | Why it matters |
|---|---|---|---|
| **mise** | Polyglot tool version manager (rust, node, plus arbitrary tools incl. cargo-installed ones) + task runner; `mise.lock` for pinning | **Very active** (jdx; lockfile + renovate support) | **Pin the whole toolchain** — rustc, wasm-bindgen-cli, wasm-opt, naga-cli, trunk, wgsl-analyzer — so native and wasm builds are reproducible across the team and CI. Can also replace `just` via its task runner. |
| **aqua** | Declarative CLI version manager (from the aqua-proj ecosystem) | **Active** | Alternative to mise; mise has the larger mindshare in the Rust community in 2025–26. |
| **bacon** | Background code checker TUI (check/clippy/test on save, errors-first display) | **Active** (3.25.0, 2026-08) | Nicer than cargo-watch for the edit loop — runs clippy nextest etc. headless alongside your editor. |
| **cargo-watch** | Rerun commands on file change | **Quiet/maintenance** | Still fine, but bacon is the modern replacement for the watch loop. |
| **just** | Command runner (better Makefile) | **Active** | The natural home for your wasm pipeline recipes: `just wasm` = cargo build target + wasm-bindgen + wasm-opt; `just shaders` = naga validate; `just abi-check` = your A/B byte-identical suite. Stable syntax, no tab-purgatory. |
| **cargo-run-bin** | Run cargo-installed binaries pinned per-project | **Active** | Lightweight alternative to mise for pinning CLI tools (wasm-bindgen-cli version matching is exactly its use case). |

---

## Top Picks for This Codebase

**Adopt / standardize now:**
1. **tracing + profiling crate abstraction**, with **tracing-tracy** (native frame profiling) and **puffin_egui**-style in-app overlay; **samply** as the no-sudo sampling profiler (drop flamegraph/optick).
2. **Drop wasm-pack** (sunset): explicit `wasm-bindgen-cli` (version-pinned via mise or cargo-run-bin) + `wasm-opt`, or **trunk** if you want a dev server; **wasm-bindgen-test** headless-browser CI for the wasm target.
3. **cargo-deny** as the single supply-chain gate (advisories + licenses + bans for duplicate wgpu/naga/winit); drop cargo-audit.
4. **cargo-nextest + cargo-llvm-cov** for tests/coverage; **sccache** on shared storage for CI; **cargo-hack --each-feature** for the native/wasm feature matrix.
5. **wgsl-analyzer** for the team + **naga-cli** shader validation in CI + **wgsl-bindgen** for struct-layout safety (directly supports your byte-identical verification culture).
6. **release-plz + git-cliff + cargo-semver-checks** for releases; **dist** (cargo-dist, active again) if you ship binaries.
7. **mise** for whole-toolchain pinning (rustc + wasm-bindgen-cli + wasm-opt + naga-cli + wgsl-analyzer) + **just** recipes; **bacon** for the local edit loop.

**Use with scoped expectations:**
- **Miri / ASan / TSan**: native-only, and Miri only on the pure-Rust core (no GPU FFI paths).
- **insta**: snapshot structured data (scene/layout/shader reflection); use render→readback→golden-image (wgpu examples pattern) for pixels.
- **twiggy**: wasm size triage; expect naga to show up large.

**Avoid:**
- optick (dead since 2020), wasm-pack for new workflows (rustwasm sunset), cargo-tarpaulin (superseded by llvm-cov), gpu-allocator (Vulkan-only, not wgpu).
