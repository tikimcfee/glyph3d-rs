# Toolchain bump — the prepared pass (write-up for the next session)

Status: RESEARCHED, not started. Prepared 2026-09-24 from a live read of
Modular's channels (MAX releases, Mojo changelog, GitHub releases) against
this tree's pins and call sites.

## The move

From the pinned nightly (`mojo ==1.1.0.dev2026083005`, `max ==26.6.0.dev2026083005`
— pixi.toml's exact per-platform pins for both osx-arm64 and linux-64) to the
**stable cut of the same family: `mojo ==1.1.0`, `max ==26.6`** (released
2026-09-17).

DO NOT overshoot to the 26.7/1.2 nightly: it removes `MutStringSpan`/
`MutStringSlice`, deprecates pointer+length `hash()`, reworks `Layout`
alignment — no hits in this tree, pure churn. NOTE: pixi.toml's movable ranges
(`mojo <2`, `max <27`) plus max-nightly listed first mean a plain
`pixi update` sails past 26.6 stable to the 26.7 nightly — set the exact pins,
and consider capping the ranges below 26.7.

`pixi.lock` is binary — never hand-merge; regenerate by install.

## The one confirmed break

`std.gpu` is private in 1.1.0 (`std._gpu`); the import fails with a pointer to
the new home. Replace `from std.gpu import global_idx` with
`from max.gpu import global_idx` in SEVEN files: `engine/gpu_decode.mojo`,
`engine/gpu_scan.mojo`, `engine/gpu_pipeline.mojo`, `engine/gpu_paginate.mojo`,
`engine/gpu_bounds.mojo`, `engine/gpu_cluster.mojo`, `engine/cluster_device.mojo`.
`engine/TOOLCHAIN.md`'s
documented exit condition counts five — gpu_cluster.mojo and
cluster_device.mojo (the kernels' shared module, since b7601fe) landed after
it was written; update that count while there.

## What the bump buys (all 26.6/1.1.0, Apple silicon is the theme)

- Metal: `DeviceExternalFunction` no longer crashes on launch; Row-API
  reduction nondeterminism fixed (byte-identical across runs);
  device-to-device copy race fixed; bf16 math intrinsics widened around Metal;
  a wrong-code const-propagation fix ("loop result constant on some paths");
  `MODULAR_DEBUG=device-sync-mode` works on Apple GPUs; missing Metal
  toolchain now prints the actionable xcodebuild line.
- `DeviceBuffer.unsafe_host_ptr()`: unified-memory readback after
  `synchronize()` with NO `enqueue_copy` round trip — squarely on the
  small-size dispatch tax the cluster benches measured (2026-09-24).
- `DeviceContext.is_host_unified()`, `create_event()`/`DeviceEvent` on Apple.
- DeviceContext API: no renames, only additions — the five call families this
  tree uses (`enqueue_create_host_buffer`, `enqueue_create_buffer`,
  `enqueue_copy`, `enqueue_function`, `synchronize`) are unchanged.

## The two quirks to re-probe after the bump (neither confirmed fixed anywhere)

1. The two-keyword-comptime-param parse failure (the `split` param of
   run_pipeline became `cluster_split` to dodge it). Try a two-kwarg call
   site; if it parses, the dodge can stay as-is (it reads fine) — just record
   the answer.
2. The executable-codegen/allocator miscompile class: two hand-built `Slots`
   views over function-local `List`s crashed the allocator during the
   conformance_split bring-up (2026-09-22); the suite compares whole
   PipelineResults instead. `engine/ffi_selftest.mojo`'s header documents the
   related in-process-import miscompile. If the dylib discipline wants to
   relax, re-probe with a minimal two-view program first.

## The discipline (house rules for a dependency bump)

One dependency per commit; the full battery green before each commit lands
(not after, in bulk); the resolved version checked against the published
manifest; a call-site sweep; the golden views cmp'd into a named scratch dir.
Record what moved and why. `engine/check.sh` fails loudly at parse if anything
else moved — that is the gate the pin strategy was designed around. The
`--fp-mode contract=off` flag is load-bearing (bit-exactness); never build the
dylib without it.

## State at handoff

All green at HEAD. The sequence-pass line is complete: CPU rule, split form
proven (conformance_split), device kernels (cluster_device.mojo) proven
bit-exact (gpu_cluster) and integrated into the full-device pipeline
(gpu_pipeline.mojo runs the pass on device). The bench trio exists
(`gpu_pipeline.mojo --bench` / `--bench-cluster`); 2026-09-24 numbers and
the measured lever list (table caching, device-class search choice, fusion)
are in the day's commits (6be54a1, 15bf453, bf0a117).

---

## Results — EXECUTED 2026-09-24

The pass landed as written, with one correction: the confirmed break was SIX
files, not seven — `gpu_cluster.mojo` never imported `global_idx` (it is the
host-side suite over `cluster_device`'s kernels). The seven-count was module
membership, not imports; grep is the witness.

- Pins: `mojo ==1.1.0` / `max ==26.6` both platforms; ranges capped inside the
  family (`>=1.1.0,<1.2` / `>=26.6,<26.7`). The lock resolved the STABLE
  release builds — published on the max-nightly channel as `-release.conda`,
  so channel order never came into play. `mojo --version`: `1.1.0 (8189361e)`.
- Import flip: six files, `from std.gpu import global_idx` →
  `from max.gpu import global_idx`. No other call-site moved.
- Gates: full battery green on stable (every golden view byte-equal on
  metal-apple — the new compiler is bit-identical on this tree); full prove
  green, 14/14 coverage, every mutation firing.
- Quirk 1 (two keyword comptime params at one call site): **FIXED in
  stable.** Probe: the run_pipeline signature shape called with
  `[witness=False, cluster_split=True]` — parses and evaluates correctly. The
  `cluster_split` name stays (it reads fine); future call sites may use two
  kwargs freely.
- Quirk 2 (two hand-built `Slots` views over function-local `List`s crashing
  the allocator): **FIXED in stable.** Probe: the conformance_split bring-up
  shape rebuilt as an executable — both instantiations, `.slots()` on both
  results, lane-wise diff through the views — bit-exact, no crash. The
  shipped whole-PipelineResult comparison stays regardless: it is the
  stronger proof (it covers the fold's output). ffi_selftest keeps linking
  the shipped dylib for the same reason — stronger, not forced.
- New surface noted while probing, no action needed: positional `__getitem__`
  on `Pointer` is deprecated in stable (`use unsafe_offset=`). The engine's
  accessors already use `unsafe_offset=`; only the probe tripped the warning.
- Not yet harvested (the bump's payload, queued behind it):
  `DeviceBuffer.unsafe_host_ptr()`, `is_host_unified()`, `create_event()`.
