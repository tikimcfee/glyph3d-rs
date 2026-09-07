# Linux bring-up — 2026-09-07

The first run of this tree on something other than Apple Silicon. Everything
here is **[measured]** on this box unless marked otherwise; it is a record of
what happened, not the contract — `AGENTS.md` is the contract.

## The box

| | |
|---|---|
| CPU | AMD Ryzen 9 9950X3D, 16c/32t |
| RAM | 60 GiB (the M2 testbed had 16, shared with the GPU) |
| GPU | NVIDIA GeForce RTX 5090, 32 GiB VRAM, driver 610.57.04, Vulkan 1.4 |
| OS | CachyOS (Arch), kernel 7.2.3, KDE on Wayland |
| Rust | stable 1.98.1 via rustup |
| pixi | 0.80.0 |
| Mojo | 1.1.0.dev2026083005 — the SAME nightly the osx-arm64 lock names |
| Node | 22.22.3 |

## What had to change to build here

Four things, all of them the shared library's name and one BSD-ism:

1. **`pixi.toml`** gained `linux-64` as a platform, a `[target.linux-64.tasks]`
   `build-engine` that emits `native/libglyph_engine.so` (no
   `install_name_tool` — an ELF soname comes from the filename), and an EXACT
   `[target.linux-64.dependencies]` pin on mojo/max. Without the exact pin a
   fresh linux-64 solve took the newest nightly, not the one the suites were
   verified against. The osx-arm64 half of `pixi.lock` was untouched by the
   re-solve (zero removed lines in the diff).
2. **`build.toml`** names the library `native/libglyph_engine.{dylib}`; the
   runner substitutes the host extension at manifest load (`dylib_ext()` in
   `glyph/src/main.rs`), same idiom as `{scratch}` and `{mode}`.
3. **`native/build.rs`** picks the extension from `CARGO_CFG_TARGET_OS`, for
   both the engine library and the Mojo runtime (`libKGENCompilerRTShared`).
4. **`engine/check.sh`** links `ffi_selftest` against the platform's library
   name, and — the BSD-ism — both it and `tools/check-fixture-parity.sh` called
   `mktemp -t name` with no X's, which GNU mktemp refuses ("too few X's").
   Templates now carry `.XXXXXX`, which both implementations accept.

Plus one defect in the build tool that only Linux can see, found by
`cargo glyph prove`: the runner re-executes itself through `current_exe()` to
run each gate in a child. Cargo keeps TWO `glyph` artifacts — the workspace
build and the `-p glyph` build the alias makes resolve different feature
sets — and re-points `target/release/glyph` at whichever was asked for last,
without recompiling (inode alternates 11942885 / 11943448, measured). The
cargo-build gate runs the workspace build, so mid-`prove` the running
binary's file is unlinked; Linux then reads `/proc/self/exe` as
`glyph (deleted)` and every later spawn fails with ENOENT. First run: 5
mutations proved, then "could not run gate cargo-build: No such file or
directory" and 11 straight "ALREADY RED" verdicts for gates that were green.
macOS returns the plain path from `current_exe()` and never saw it. Fixed by
reading the path once at startup (`self_exe()` in `glyph/src/main.rs`);
second run: **16 of 17 mutations proved, 13/13 gates covered**, the one
unproven being `coverage-scale → pixel-ab`, which is already red here for
the reason below.

No Rust source under `native/src` and no `.mojo` file changed. `wgpu`'s
`Backends::PRIMARY` resolves to Vulkan here and picks the 5090 over the
integrated Radeon under `HighPerformance` without any code change.

## The battery, first run

`cargo glyph test`: **12 of 13 gates green**, 1m54s wall.

| gate | result | note |
|---|---|---|
| products | PASS | engine `.so` builds in 2.6 s; workspace release build 38 s |
| committed-artifacts | PASS | atlas, trie, schema, 25 fixtures all byte-identical |
| vendor-hashes | PASS | |
| engine-suites | PASS | all 16 suites + ffi_selftest + instruments; the 5 GPU suites ran on the RTX 5090 through MAX's `DeviceContext`, first try, 18.5 s |
| cargo-build / clippy / doc | PASS | 0 warnings each |
| cargo-test | PASS | 93 tests over 3 binaries, floor 93 |
| engine-check | PASS | both inputs bit-exact, including the non-zero-origin case |
| pick-oracle | PASS | |
| **pixel-ab** | **FAIL** | all five views — see below |
| repo-verify (down, back) | PASS | 407,451 records bit-exact |
| repo-verify-direct (down, back) | PASS | 407,133 instances bit-exact |
| reference-port | PASS | |

Everything that is a claim about NUMBERS — the fold, the scan, the bake, the
FFI, the direct path, the picks — is bit-exact on a different CPU
architecture, a different OS and a different GPU vendor. That is the result
worth carrying.

## pixel-ab is red, and it is not a regression

The five baselines in `out/tooling-ab/baseline/` were rendered on Metal. This
box rasterizes with NVIDIA's Vulkan driver. Measured against those baselines:

| view | differing px | of frame | max ch. delta | px with delta ≥ 16 |
|---|---:|---:|---:|---:|
| demo | 63,078 | 3.94 % | 206 | 124 |
| text | 13,571 | 0.85 % | 3 | 0 |
| repo-wide | 67,784 | 4.24 % | 60 | 6 |
| repo-down | 22,990 | 1.44 % | 69 | 59 |
| repo-zoom | 13,902 | 0.87 % | 1 | 0 |

The mean delta over differing pixels is 1.0–1.4 levels: last-bit rounding in
the analytic-coverage shader and the sRGB encode. The pixels with a large
delta are isolated single pixels at quad edges — of demo's 124, 112 have no
high-delta 8-neighbour and none has three — which is the rasterizer's edge
rule and sample position flipping coverage on a boundary, not anything that
moved. A layout or camera change produces thousands of contiguous high-delta
pixels; the same layout code is proven bit-exact three ways above.

**What is NOT done, deliberately:** no Linux baselines were generated.
Re-baselining is a human act in this repo (the runner refuses; `AGENTS.md`),
and the choice of mechanism is a design decision — per-platform golden sets
(`baseline-<platform>/`, each byte-exact against its own oracle) is the
option that keeps the gate as strict as it is today; a tolerance is the
option the repo's own rules forbid. Until one lands, `cargo glyph test` on
Linux ends `CHECK-ALL: FAILURES` on pixel-ab alone, and `cargo glyph test
engine|rust|corpus` are fully green.

## Windowed, on Wayland

`--load-repo fixtures/g-pick-repo` opens, renders 407,133 instances, and the
egui overlay attaches. It reports **75 FPS with `present=Fifo`** — that is the
display's refresh under vsync, not a throughput figure. Offscreen, the
1M-instance demo reports ≈1,168 fps steady-state (2 frames GPU-completed in
1.71 ms), which is the number to compare against the M2.

## Things to know about this box

- **A `llama-server` was holding 25.9 GiB of the 5090's 32 GiB** when the
  battery ran, leaving ~4.6 GiB. Everything above passed inside that; a
  repo-scale arena (`--load-repo` on a large tree) may not. Check
  `nvidia-smi` before a memory-sensitive measurement.
- The linux-64 `max` package resolved to its `3.12release` build (Python
  3.12 inside `.pixi/`), where osx-arm64 has `3.14release`. Mojo is the same
  build on both; nothing here calls MAX's Python.
- Two Vulkan devices are visible (the 5090 and the CPU's integrated Radeon
  via RADV). `HighPerformance` picks the 5090; nothing pins it, so a headless
  session with a different device order would need `WGPU_ADAPTER_NAME`.
