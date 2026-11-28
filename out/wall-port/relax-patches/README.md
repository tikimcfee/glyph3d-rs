# relax: the owner's constant drain read one unit's ids in another unit

One patch for makepad's `relax` JIT, made on `upstream/work` 63f80c998 on 2026-11-27:

    0001-relax-const_drain-makes-an-item-in-the-unit-that-not.patch

It is commit 1c2756591 on the local branch `relax-const-drain-unit`, checked out in the worktree
`~/localdev/makepad-upstream-work`. Nothing was pushed. `git apply --check` also passes
against the local `work` branch (7a258a352); it was not built or run there.

## The bug

`relax` ends with a host fault on its owner thread (`run-owner`) while the wall is starting or
running. The frames are always these, innermost first:

    Unit::vtable | Unit::const_addr | Unit::promoted_addr     (the fault, or CTFE code they run)
    jit::const_drain
    owner::serve                  a Job request, served inside a wait
    owner::pump_wait_at  <-  QueryLatch::wait  <-  Tcx::typeck  <-  compile_slot  <-  rt_compile
    (JIT code of the macro world)
    jit::call_jit_in  <-  pmhost::call        a proc macro's call, a Program request on the owner

What happens:

- A background build's front that needs a constant, a vtable, a static or a slot's entry nobody
  made yet notes it in the process-wide queue `CONSTS_WANTED` and asks the owner to drain it
  (`prefront_slot` -> `owner::call(Class::Job, const_drain)`).
- `const_drain` made every queued item in `cur_unit()`: the unit whose code the draining thread
  runs.
- A proc macro's call runs on the owner inside the macro world's unit: `call_jit_in` makes that
  unit the thread's. The macro world has its own `Program`, `Tcx` and `Unit` (pmhost.rs), so the
  program's `DefId`s, `TyId`s and `FnKey`s mean something else there, or nothing.
- When a lazy compile inside that call waits on a type check another thread runs, the owner pumps,
  and a pump serves Job and Publish requests (`Class::keeps_job`). `const_drain` is the one Job
  that takes its unit from the thread instead of carrying it.
- The epoch check in the drain does not stop it: both units' collect epochs are equal.

So an item noted by a program front was made in the macro world's unit. An id past that unit's
tables is a SIGSEGV in host code (`Unit::vtable`, `Unit::const_addr`, `Types::flags` under
`Unit::promoted_addr`). An id inside them names another item, and its constant evaluation ran:
"illegal instruction at pc in JIT ctfe:core::num::prim_traits::<&usize as Shr>::Output", signal 4.

It needs the macro world to be built while the program already runs and its background builds
note items. A settled warm start maps the macro world from the cache and makes its 48 macro calls
in about 60 ms with no wait inside them; then nothing happens. The world is built again after
`main` started when its stores are missing or stale. One sure way into that state: the relax
sources in the makepad checkout are newer than the running binary (`relax live: N source inputs
changed (...): Relax's own sources changed`; reload_inputs.rs compares each file's mtime with the
binary's, so a checkout that rewrites a file to the same content counts). Another: a boot that
crashed. In these runs the world was built again after `main` in every boot until two boots in a
row had exited cleanly, and a crash keeps that from happening.

`RELAX_BG_TYPECK=0` does not prevent it: three of the crashes below had it set to 0.

## The fix

- Each item in `CONSTS_WANTED` names the unit that noted it (`want(u, ..)`), and `const_drain`
  makes it in that unit, with that unit's epoch.
- An item whose unit the thread cannot name as alive (the thread's own, the runtime's, the booted
  program's, a registered auxiliary unit) is made by no drain, and `retire_unit` takes a retiring
  unit's items out of the queue, as it does for the other state keyed by a unit's address.
- `wanted_key` holds the unit, so equal ids of two units are two items for the left-out builds.
- A test, `jit::tier_stuck_tests::the_drain_makes_an_item_in_the_unit_that_noted_it`: two units,
  a constant noted by the first, the drain served while the owner's thread unit is the second.

2 files, +120 -30: jit.rs +51 -28, tier_stuck_tests.rs +69 -2.

## What was run

Machine: laptop-2. App: `glyph3d-native --wall` from the `wall` worktree at 18135e8, through
`tools/wall-relax.sh --ns relaxprobe --verbose`, a scratch `g3d-termd` in `/tmp/g3d-rx`. Each boot:
wait for the control socket, `wallctl app.where`, hold, `wallctl app.quit`, read the exit status.
All but the first five boots used a scratch `RELAX_CACHE_DIR`, one per relax build, so the wall
worktree's own `target/relax` holds no store of any build but the one it had. Some boots ran
beside CPU load (a `cargo test` build, or six busy loops): 5 of the unfixed build's boots of the
kind that can crash, 4 of which crashed, and 8 of the fixed build's.

Unit test (`cargo test -p relax-compiler --lib`, the new test and the existing
`a_left_out_build_is_built_again_once_per_thing_it_waited_on`):

- with the fix: 2 passed.
- with the drain put back to `cur_unit()` (one line of the fix reverted, then restored): the new
  test fails, "the drain did not make K1 in the unit that noted it".

Boots, by build and by what the boot did with the macro world (read from each boot's log:
`item graph: 11156 nodes mapped` or `(11156 walked ...)`, before or after `boot done, calling
main`):

| build | macro world in that boot | `RELAX_BG_TYPECK` | boots | crashed | answered `app.where` | time to control socket, s |
|---|---|---|---|---|---|---|
| existing relax (7a258a352) | built before `main` | 0 | 2 | 0 | 2 | 8.7 to 35.9 |
| existing relax (7a258a352) | built before `main` | 1 | 3 | 0 | 3 | 9.3 to 35.7 |
| existing relax (7a258a352) | mapped from the cache | 0 | 2 | 0 | 2 | 1.6 to 1.6 |
| existing relax (7a258a352) | mapped from the cache | 1 | 5 | 0 | 5 | 1.1 to 18.8 |
| upstream 63f80c998 | built after `main` started | 0 | 6 | 3 (3 SIGSEGV) | 4 | 1.1 to 1.7 |
| upstream 63f80c998 | built after `main` started | 1 | 21 | 8 (3 SIGILL, 5 SIGSEGV) | 13 | 1.1 to 1.7 |
| upstream 63f80c998 | built before `main` | 0 | 16 | 0 | 16 | 3.7 to 46.4 |
| upstream 63f80c998 | built before `main` | 1 | 20 | 0 | 20 | 3.2 to 6.3 |
| upstream 63f80c998 | mapped from the cache | 0 | 13 | 0 | 13 | 1.1 to 1.7 |
| upstream 63f80c998 | mapped from the cache | 1 | 19 | 0 | 19 | 1.1 to 3.8 |
| upstream + patch | built after `main` started | 0 | 4 | 0 | 4 | 1.1 to 1.7 |
| upstream + patch | built after `main` started | 1 | 13 | 0 | 13 | 1.1 to 1.7 |
| upstream + patch | built before `main` | 0 | 11 | 0 | 11 | 3.7 to 50.1 |
| upstream + patch | built before `main` | 1 | 6 | 0 | 6 | 3.7 to 5.3 |
| upstream + patch | mapped from the cache | 0 | 19 | 0 | 19 | 1.1 to 3.7 |
| upstream + patch | mapped from the cache | 1 | 32 | 0 | 32 | 1.1 to 4.2 |
| probe builds (upstream + prints) | built before `main` | 0 | 2 | 0 | 2 | 8.3 to 36.2 |
| probe builds (upstream + prints) | built before `main` | 1 | 7 | 0 | 7 | 6.3 to 48.7 |
| probe builds (upstream + prints) | mapped from the cache | 0 | 2 | 0 | 2 | 1.7 to 1.7 |
| probe builds (upstream + prints) | mapped from the cache | 1 | 2 | 0 | 2 | 1.6 to 1.6 |

Crash reports of the pristine upstream build (`~/Library/Logs/DiagnosticReports/`): 11 files,
`relax-2026-11-27-00*.ips` and `relax-2026-11-27-01*.ips`. 8 are SIGSEGV (5 in `Unit::vtable`, 3 in
`Types::flags` under `Unit::promoted_addr`) and 3 are SIGILL (constant-evaluation code run by
`Unit::const_addr`). In each the faulting thread is `run-owner` with `const_drain` under
`pmhost::call`. Three of the 11 were with the worktree's sources matching the binary and no other
load on the machine.

The table counts every boot made, 205 in all. The probe builds are upstream plus diagnostic prints
(never committed); they were used to count the owner's waits inside the macro world's code: 0 in
a settled boot, 181 to 273 when the world is built.

## What this does not show

- The existing relax (the 7a258a352 build the report of 2026-11-26 used) did not crash today:
  12 boots, 0 crashes (5 in the wall worktree's own cache with `RELAX_BG_TYPECK=1`, 7 in a scratch
  cache). In none of them was the macro world built after `main` started: its boots after a cold
  start exited cleanly and settled. Its four crash reports of 2026-11-26 have the same frames as
  the 11 above. Forcing that binary into the state (ending a boot before its exit write, or
  touching the makepad checkout it reads) was not tried.
- The crash is a race. The unfixed build crashed in 11 of its 27 boots of the kind that can
  crash; the fixed build made 17 such boots without one. The test is what shows the mechanism
  every time.
- The patch does not touch two other places that take the thread's unit: `frame_leave` clears
  `building` on `cur_unit()` for builds a panic left open, and `left_out_drain` / `realtime_drain`
  use `cur_unit()` (both are called only between requests, where it is the runtime's unit).
