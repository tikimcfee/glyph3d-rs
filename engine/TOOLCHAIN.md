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

## 3. `TaskGroup` — ALREADY MIGRATED, ONTO A PRIVATE MODULE. Unresolved.

15 `TaskGroup()` constructions across 4 files are the entire CPU parallel driver.
This tree already had to move, and moved to:

```mojo
from std.runtime import parallelism_level          # correct, and permanent
from std.runtime._asyncrt import TaskGroup         # PRIVATE MODULE
```

`parallelism_level()` moving up a level is settled and right. `TaskGroup` is the
open one: the nightly `std/runtime` docs say the async primitives live in the
private `_asyncrt` module, that Mojo's async support is unfinished and carries no
stability guarantees, and — in as many words — *do not build async patterns on
it yet*. A nightly search for `TaskGroup` returns zero hits.

So the parallel driver rests on an API with no stability promise and no
deprecation path: private modules do not get one. It works on the pinned build.
It can vanish in any nightly.

**The candidate replacement exists here but is unverified.** `std.algorithm.map`
imports cleanly on this build and is shaped exactly like the shard loops (a
unified closure over `[0, size)`). But the docs do not state that it is parallel,
and `parallelize` returns zero hits in both stable and nightly.

**Do not rewrite 15 sites on that hope — measure first.**
`engine/bench/split_bench.mojo` already carries the shard harness that can tell a
parallel `map` from a serial one in a single run, and the README's ledger records
what the current driver buys (2.7x). A silent fall to serial would show up as a
regression in that number rather than as an error, which is precisely the
green-that-lies shape this repo keeps catching. The measurement is the gate.

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
