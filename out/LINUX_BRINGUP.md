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

## The golden sets are keyed by rasterizer — landed the same day

After the first run below, `out/tooling-ab/baseline/` became one directory
per `<backend>-<vendor>` key (`metal-apple/`, `vulkan-nvidia/`), the key
printed by the renderer's `--gpu-key` off the adapter wgpu picked and resolved
by the runner from a `{gpu}` token in build.toml. The Linux set was adopted
by hand on 2026-09-07 after looking at the frames and running `glyph drift`
(the numbers in the table below are what drift reports against
`metal-apple`). With it: **13/13 gates green, 17/17 mutations proved** on this
box, including `coverage-scale → pixel-ab`, which could not be proved until
there was a set to diverge from. `AGENTS.md` § pixel-ab is the contract; the
tables below are the evidence it cites. The Metal set has no `ADAPTER.txt`
yet — the gate prints a NOTE saying so until someone runs `--gpu-profile` on
the Mac and commits it.

## Where the code meets the hardware — measured 2026-09-07

The question this box was bought to answer. `--repo-scan-only`, three
strategies, best of three isolated samples, nothing else running:

| corpus | MB | strategy | backend | MB/s | cpu/wall | peak RSS |
|---|---:|---|---:|---:|---:|---:|
| egui (533 files) | 5.0 | naive | 0.585 s | 9 | 20.7 | 298 MB |
| | | batch | 0.090 s | 56 | 3.3 | 604 MB |
| | | direct | 0.032 s | 156 | 9.3 | 604 MB |
| glyph3d-js (570) | 11.2 | naive | 0.733 s | 15 | 17.4 | 758 MB |
| | | batch | 0.218 s | 51 | 3.4 | 1.3 GB |
| | | direct | 0.087 s | 129 | 7.7 | 1.3 GB |
| nototto (192) | 27.5 | naive | 0.695 s | 40 | 8.0 | 2.1 GB |
| | | batch | 0.570 s | 48 | 2.6 | 3.2 GB |
| | | direct | 0.262 s | 105 | 8.9 | 3.2 GB |
| tmuxai (5,162) | 58.8 | naive | 5.572 s | 11 | 20.2 | 3.2 GB |
| | | batch | 1.017 s | 58 | 4.2 | 6.8 GB |
| | | direct | 0.393 s | 150 | 11.6 | 6.8 GB |
| python3.14 (5,344) | 77.3 | naive | 6.187 s | 12 | 19.2 | 6.8 GB |
| | | batch | 1.503 s | 51 | 3.9 | 9.0 GB |
| | | direct | 0.520 s | 149 | 12.1 | 9.0 GB |

**The direct path runs at 105–156 MB/s here. The M2 recorded 156 MB/s
(47.1 MB in 0.301 s) with `parallelism_level()` = 4.** Per source byte this
box is no faster with 32 threads than the laptop was with four, and the fold
stage alone is identical per byte (58.8 MB in 0.085 s = 690 MB/s here; 47.1 MB
in 0.069 s = 683 MB/s there). Three probes say why, and none of them is the
runtime:

- **The Mojo runtime is fine.** `parallelism_level()` is 32 here. A
  compute-bound `parallelize` probe scales 35.3 ms → 3.0 ms from 1 to 32
  workers (11.7x; 7.3x at 8, then SMT), and `top -H` during a direct run shows
  32 threads busy summing ~2,200% CPU. The cores are running.
- **This box's memory is the wall, and one thread already hits it.** A
  C/OpenMP stream test: fill 24 GB/s at ONE thread, 33 GB/s peak; copy
  (read+write) 42 GB/s at every thread count from 1 to 32. The engine's stages
  move 40–56 B of lane traffic per source byte — on tmuxai the fold reads and
  writes ~2.3 GB in 0.085 s (≈28 GB/s), decode ≈22 GB/s, `eg_direct` writes
  2.7 GB of 48 B instances in 0.175 s (≈16 GB/s plus its reads). Every stage
  sits at 60–90% of what this machine's DRAM will do. The M2's unified memory
  is roughly 100 GB/s; its four performance cores had more bandwidth per core
  than these sixteen do in total. **The engine is memory-bandwidth-bound, on
  both machines, and the lane layout's bytes-per-source-byte is the lever** —
  the thing `engine/README.md`'s write-axis and witness splits already reduce
  and `engine/BACKEND-PLAN.md`'s memory argument already names. More cores
  will not move it. The 5090's 1.8 TB/s would.
- **Dispatch is not free at 32 workers, and the per-item strategy pays it per
  file per stage.** An empty `parallelize` costs ~55 µs here. `naive` runs
  every stage per file: tmuxai's 5,162 files turn into ~5 s of overhead over
  `batch` (the M2 doc records naive as FASTER than batched at 47 MB, on 4
  workers). Nothing ships on `naive`; it is the verification counterpart. But
  `fold_profile` shows the same shape inside one call — at 1 item 109 MB/s,
  64 items 420, 4,096 items 660 — so item count still sets the ceiling
  exactly as the plan says, and a single large file folds on one core here
  too.

`fold_profile` on this box (17 files, 718 KB): serial 109 / 209 / 420 / 623 /
660 MB/s at 1 / 8 / 64 / 512 / 4,096 items; scan 87 → 2.1 MB/s over the same
sweep (316x worse at 4,096 items, the plan's "opposite scaling" reproduced).

**What this box IS better at.** The GPU side, and memory capacity. Offscreen
with `GLYPH_PROFILE=1`, 20 frames:

| view | instances | GPU pass | steady-state |
|---|---:|---:|---:|
| demo | 1,000,000 | quad field 0.49 ms | ≈1,663 fps |
| repo-wide (g-pick-repo) | 407,133 | glyph field 0.48 ms | ≈1,779 fps |
| tmuxai, default camera | 57,068,088 | 0.007 ms (culled) | ≈543 fps |

tmuxai's field is 2.6 GB of instances in two chunks under the 2 GiB binding
limit, uploaded and rendered without incident; the 1.40 GB corpus the M2
choked on at 84 s per-item is a capacity question this box does not have
(60 GB RAM, 32 GB VRAM). Windowed, the same g-pick-repo scene runs ~75 FPS
under Fifo (vsync) and ~1,300 under `--present-mode mailbox`, ~1,700 under
`immediate` — the flag exists so that difference is never read as a
renderer change.

## First run: pixel-ab was red, and it was not a regression

(As found on the first run, before the keyed sets above existed.) The five
baselines in `out/tooling-ab/baseline/` were rendered on Metal. This
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

At the time no Linux baselines were generated: re-baselining is a human act
and the mechanism was a design decision. The decision — one golden set per
rasterizer, keyed `<backend>-<vendor>`, never a tolerance — landed the same
day; see the section above and `AGENTS.md` § pixel-ab.

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
