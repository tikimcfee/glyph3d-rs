# Toolchain state — what is true in THIS tree

Companion to the survey in the web repo's `engine/toolchain-migration.md`
(2026-09-02). That document is correct for the tree it was written against and
**this tree is on a different channel**, so the three breakages it forecasts are
in three different states here. Measured 2026-09-02.

## The channel split is the whole reason this file exists

| tree | channel | version |
|---|---|---|
| `viz-web/glyph3d-js` | pip / `.venv-mojo` | **Mojo 1.0.0** (stable) |
| `viz-native/glyph3d-native` | pixi / `max-nightly` | **Mojo 1.1.0.dev2026083005** |

So the migration doc's framing — "the next release, not one you can `pip install`
yet" — is right for the web tree and already past tense here. The two-line
`MOJO-1.1-PORT` delta in `glyph_pipeline.mojo` / `glyph_scan.mojo` is not this
tree running ahead by choice; it is the minimum needed to compile on the channel
it is pinned to.

## 1. `std.gpu` → `max.gpu` — BLOCKED, and we are inside the window

The doc says to replace `from std.gpu import global_idx` with `from max.gpu ...`.
**Do not do that yet.** Measured on the pinned nightly:

```
from max.gpu import global_idx
  -> error: package 'gpu' does not contain 'global_idx'
```

`std.gpu` still resolves here and all five GPU suites pass; `max.gpu` does not yet
mirror it. The privatization landed in a nightly AFTER `dev2026083005`. So there
is a window where neither the old nor the new import is wrong, and only one of
them works.

**Exit condition:** re-run that probe. When `max.gpu.global_idx` resolves, change
the five import lines (`gpu_decode`, `gpu_scan`, `gpu_paginate`, `gpu_bounds`,
`gpu_pipeline` — the 15 `global_idx.x` use sites do not change) and update this
section. `max.gpu.host` is already correct and unaffected.

**Why the pin is NOT being tightened:** `pixi.lock` is tracked and names
`mojo-1.1.0.dev2026083005` in nine places, so a fresh clone resolves to the exact
build these suites were verified against. Only a deliberate `pixi update` moves
it — and if someone runs one past the privatization, `engine/check.sh` fails
LOUDLY at parse rather than silently degrading. A working pin plus a loud gate is
better than a narrower pin that a pruned nightly channel could make unsolvable.

## 2. `memcpy` → `unsafe_memcpy` — DONE, and it was already broken

Not a forecast here: `engine/bench/blob_bench.mojo` had NOT built for some time.
`std.memory.memcpy` is already gone on this channel. Fixed (one import, two call
sites, same signature).

The reason it sat: **nothing compiled the benches.** They cannot be RUN in a fresh
tree — `engine/bench/bench.bin` is untracked and its generator needs the JS
reference pipeline — so they were outside every gate entirely. `engine/check.sh`
now COMPILES every bench file (`check.sh bench`, and in the default `all` path).
Mutation-proved: restoring the old spelling fails the gate at parse.

## 3. `TaskGroup` — RESOLVED FROM SOURCE (2026-09-02). Migration scoped, not done.

Mojo is open source and the tree is at `viz-native/modular/`, so this stopped
being a measurement question and became a reading one.

**`std.algorithm.map` is NOT the replacement.** Its whole body is:

```mojo
for i in range(size):
    func(i)
```

(`Mojo/stdlib/std/algorithm/backend/cpu/map.mojo`). A serial loop. Rewriting 15
sites onto it would have silently turned the parallel driver into a sequential
one — the ledger's 2.7x quietly becoming 1x, with every conformance suite still
green because the RESULTS are identical. Exactly the failure shape this repo
keeps catching, and it was one plausible-looking doc sentence away.

**There is no public `TaskGroup`.** Confirmed twice: `max/mojo/max/runtime/asyncrt.mojo`
does not define one, and a build probe on the pinned nightly answers
`module 'asyncrt' does not contain 'TaskGroup'`. `std.runtime._asyncrt` is the
only one, and it is private.

**The public parallel primitive is `max.algorithm.parallelize`** — the same
consolidation as `std.gpu` -> `max.gpu`, and in a package this repo ALREADY
depends on. `max/mojo/max/algorithm/__init__.mojo` exports `parallelize`,
`parallelize_over_rows` and `sync_parallelize`. Verified running on the pinned
build, not just importable:

```
workers: 4  results: 0 9 49        # parallelize(body, 8) over i*i
```

It does not use `_asyncrt` at all: it goes through `DeviceContext(api="cpu")`,
`enqueue_cpu_range`, `synchronize()`.

**All 15 sites are the same shape**, which makes the migration mechanical:

```mojo
var tg = TaskGroup()                      def body(w: Int) {imm ...}:
for w in range(workers):                      var a = shard_lo(0, n, workers, w)
    var a = shard_lo(0, n, workers, w)        var b = shard_lo(0, n, workers, w + 1)
    var b = shard_lo(0, n, workers, w+1)      _shard(..., w, a, b)
    tg.create_task(_shard(..., w, a, b))  parallelize(body, workers)
tg.wait()
```

No nesting, no inter-task dependencies, no task results — every one of the 15 is
`create_task` in a `for w in range(workers)` loop followed by `wait()`.

**Why it is scoped and not done here.** Three reasons, in order:

1. **The 2.7x must be re-measured, not assumed.** `parallelize` creates and
   synchronizes a `DeviceContext` per call; `glyph_scan.mojo` alone opens EIGHT
   groups per run. The `ctx` parameter exists precisely so one context can be
   reused across them, and whether that matters is a measurement.
   `bench/split_bench.mojo` is the harness.
2. **It touches the two most-shared files.** `glyph_pipeline.mojo` and
   `glyph_scan.mojo` are otherwise near-byte-identical to the web tree; 15 edits
   there is a deliberate widening of the fork, not a drive-by.
3. Nothing is on fire. The private import works on a tracked, pinned lock, and
   `check.sh` fails loudly at parse if a `pixi update` ever moves past it.

So: a scoped change of its own, with split_bench as its gate — not a bullet in
this file's margin.

## Checked and clear on this channel

`Atomic` (every site uses the inferred static form and writes no explicit
parameter), `std.bit`, `std.memory.bitcast`, `std.collections`, `std.math`,
`std.sys`, `std.time`, the `hash(-0.0)` change (integer keys only), and the
removed `std.gpu.profiler` (timing here is already `perf_counter_ns`). No
`@parameter` closures, no `capturing`, no `read` convention, no `InlineArray`,
no `.mojopkg`.

One to remember rather than act on: **uncaught exceptions now print to `stderr`,
not `stdout`.** The suites report pass/fail on stdout, so a raise now leaves by a
different door. `check.sh` captures `2>&1`, so it is covered — but a future runner
that does not would read green while the failure went somewhere nothing was
looking.
