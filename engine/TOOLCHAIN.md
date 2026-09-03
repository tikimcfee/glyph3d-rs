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

**DONE 2026-09-02, and it is faster.** All 12 engine sites migrated
(`glyph_pipeline.mojo` 4, `glyph_scan.mojo` 8) plus the 9 `async def` shard
bodies de-async'd — there is no `await` anywhere in either file, so `async` was
purely a `create_task` affordance.

Two of the twelve are not flat index spaces (paginate and the bounds grains
iterate a filtered `items x shards` product with a running counter), so those
materialize an explicit task table first: same tasks, same order, same disjoint
ranges, only the dispatch changes.

Measured on the pinned build, best-of-3 each:

| | TaskGroup | parallelize | |
|---|---:|---:|---|
| pipeline | 10.327 ms | **9.921 ms** | **1.041x** |
| pipeline elided | 9.618 ms | **9.240 ms** | **1.041x** |
| scan | 12.250 ms | **11.253 ms** | **1.089x** |
| bake | 36.175 ms | 36.244 ms | 0.998x (does not use the driver) |

Correctness, which mattered more than the number: **every checksum is
bit-identical** (12178245 pipeline/elided/scan, 12471515 bake), all 15
conformance suites pass across 22 fixtures, `--engine-check` is bit-exact
against the Rust reference through the rebuilt dylib, and all four render
baselines are byte-equal. The faster path produces the same bytes.

`parallelize` coalesces consecutive work items across workers rather than
dispatching one task per shard, which is the likely source of the gain — the
scan form, with the most dispatches per run (eight groups), gains the most.

**The three bench-harness sites are deliberately NOT migrated.**
`bench/lane_write_bench.mojo` and `bench/split_bench.mojo` still construct
`TaskGroup`. They are measurement tools whose absolute numbers are quoted in the
README ledger, and changing their driver would make those historical entries
incomparable — a separate, deliberate decision, not an oversight. The SHIPPED
engine (ffi.mojo -> glyph_pipeline/glyph_scan) no longer touches the private API
at all, which is the part that matters for the product.

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

## Working in a worktree (and why you probably should)

`tools/check-all.sh` reads the WORKING TREE, not HEAD. With a second thread
active in the same repo, their uncommitted edits fail your gates and tell you
nothing about your change — this happened on 2026-09-02, where an in-flight
`PhaseDraws` refactor in `glyph_scene.rs` reddened a gate run for a commit that
touched one `.mojo` file. Isolate first, then the red is yours.

A worktree is NOT free here, because three things it needs are untracked:

| untracked | consequence | fix |
|---|---|---|
| `.pixi/` | no mojo, no max | `pixi install` (~1 min) |
| `native/libglyph_engine.dylib` | link error, though `build.rs` names it | `pixi run build-engine` |
| `engine/bench/bench.bin` | benches cannot RUN (they still COMPILE) | copy it from another tree |

```bash
git worktree add .claude/worktrees/<name> -b worktree-<name>
cd .claude/worktrees/<name>
pixi install && pixi run build-engine
cp ../../../engine/bench/bench.bin engine/bench/    # optional
./tools/check-all.sh
```
