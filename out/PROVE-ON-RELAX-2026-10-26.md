# `cargo glyph prove` on makepad's relax JIT — spike, 2026-10-26

Branch `mp-prove-jit`, cut from `main` @ d05a4fe. Question: can a warm JIT
make each mutation a one-function recompile plus a run, and does what it
proves match what ships? Everything below was measured on one 32-core Linux
host (`vulkan-nvidia` golden set) shared with other agents' builds (load
average 2 to 35 during the runs). §§ 1 to 3 used the relax binary of the
makepad tree's 8dc8fc6f4 (the last commit touching `libs/relax`); § 5 used a
rebuild later the same day that adds `MAKEPAD_RUN_OVERLAY`, a source overlay
written for this spike by relax's owners.

**Short answer.** Fidelity holds: everything checked gave rustc's answer
under relax, including every golden frame byte for byte (§ 3). With relax's
source overlay, `prove --jit` (a spike flag, `glyph/src/prove_jit.rs`)
proves mutations without ever writing the working tree, and in parallel.
**The whole 116-mutation `main` proved in 13 min 50 s** (93 under relax, 23
on cargo), every mutation red, against 41.4 min for a cargo `prove` of 112
mutations on a slightly earlier main (§ 6). As a
merge check, the 27 mutations a real merge selects proved in **107 s**, at
host load 24 (§ 6). Per mutant, warm, under relax against cargo: a
glyph3d-native lib test 5.7 to 7.7 s (cargo: 29 to 32 s), the scene graph's
1.5 s (21 s), the runner's 2.0 s (21 s). What relax runs slower than cargo
is everything that starts many relax processes: pixel-ab, pick-oracle and
the integration tests that spawn the renderer (§ 6). The on-disk route of
§§ 1 and 2, on the older relax binary, was slower than cargo for the native
crate; § 2 keeps that measurement, because it names the relax cost (since
fixed by relax's owners: an exponential recursion in `drop_may_unwind`).

## 1. The mutation set, by what a JIT could do for it

110 `[[mutation]]` entries:

| target | count | gates | JIT gains? |
|---|---|---|---|
| Rust, library/bin source | 75 | cargo-test 50, hyper-oracle 12, pixel-ab 4, reference-port 2, pick-oracle 1, plus 6 compiler-diagnostic gates (cargo-build, -clippy, -doc ×2, -check-no-ui, orphans) | yes, except the 6: relax is not rustc's warnings, clippy, rustdoc or cargo-modules |
| Rust, the runner (`glyph/src`) | 5 | cargo-test | yes |
| Rust, test files | 0 | | |
| WGSL | 17 | cargo-test 9, pixel-ab 5, hyper-oracle 3 | yes where an `include_str!` pulls the shader in: relax's overlay serves it like any source (§ 6). Not where a test reads the shader with `std::fs` (curve_cap.rs, the naga tests) |
| build.toml, data, Python, JS, fonts | 13 | manifest, committed-artifacts, vendor-hashes, atlas-*, reference-port | no: no Rust is rebuilt |

Of the 55 Rust mutations on cargo-test, 54 name the test they redden in their
`expect` (checked against the suite's 376 test names), so a JIT route can run
that one test instead of the suite. The exception, `tests-silently-ignored`
(`expect = "Coverage DROPPED"`), needs the floor count, i.e. the whole suite.

What relax runs today, from its source and by trying it:

- `relax test <package> [--lib|--bins|--test T] [--no-fail-fast] -- <filter>`
  is `cargo test -p` under the JIT, one in-process harness per test target
  (`compiler/src/test_pkg.rs`, `harness.rs`, libtest semantics). It ran every
  test target in this workspace (§ 3).
- `relax run <package> -- <args>` runs the renderer, wgpu and the GPU
  included: the hyper-oracle visible tier and the offscreen golden renders run
  under it.
- **A persistent code cache across processes** (`target/makepad-run`): a fresh
  process on an unchanged tree is warm (0.6 s to run one glyph3d-native lib
  test, against 2 m 38 s cold). This is what the on-disk spike below runs on.
- **Hot reload exists, in-process and item-granular**:
  `reload::apply(unit, Vec<Edit{file, path, src}>)` takes new source text in
  memory, applies body and const edits as one transaction (other edit classes
  are refused with a reason, never applied stale), and returns an `Undo` that
  `reload::rollback` reverses. Two doors reach it: `relax live <v0.rs> <edit.rs>…`
  (single-file crates only, no cargo graph) and the file watcher of a
  `relax run` live session (disk files; a test harness is deliberately not a
  live session). Neither door is a package test harness taking in-memory patches.
- relax cannot fork a JIT'd process without an exec: a lazily compiled
  function reached in the child has no compiler thread
  (`compiler/src/forkexec.rs`). So a resident gate loop has to call its gate
  in-process, and the renderer's check instruments are all `-> !`
  (`run_hyper_oracle_check`, `run_fixture_*`): they exit.

## 2. Measured: prove today vs the relax route

Today's prove, scoped (`cargo glyph prove --mutation …`), per mutation:

| mutation | gate | build | gate | total |
|---|---|---|---|---|
| fu-to-world-wrong-denominator | cargo-test | (in gate) | 30.4 s | 30 s |
| scene-graph-stale-handle-aliases | cargo-test | (in gate) | 22.8 s | 23 s |
| tab-cells-always-a-whole-stop | cargo-test | (in gate) | 36.4 s | 36 s |
| cluster-static-zero-off | pixel-ab | 20.8 s | 8.5 s | 29 s |
| advance-zeroed | reference-port | 23.0 s | 1.2 s | 24 s |
| hyper-head-advance | hyper-oracle | 21.4 s | 1.4 s | 23 s |

On top of that, once per run: the clean-tree check of each gate (68 s for
cargo-test, 11 s for the three bin gates) and a final renderer rebuild (21 to
27 s). The cargo-test gate under a mutant is short because `cargo test` stops
at the first red test binary.

The relax route, the mutation applied on disk, a fresh relax process each
time:

| mutation | relax command | mutant, 1st run | same mutant again | restored tree | verdict |
|---|---|---|---|---|---|
| scene-graph-stale-handle-aliases | `test glyph-scene-graph --lib -- <test>` | 3.9 s | 0.1 s | 4.5 s | same |
| fu-to-world-wrong-denominator | `test glyph3d-native --lib -- <test>` | 93 s | 6.0 s | 7.7 s | same |
| tab-cells-always-a-whole-stop | same form | 62 s | | | same |
| hyper-device-page-cols-run | same form | 116 s | 1.6 s | | same |
| hyper-pass1-stride-after-advance | same form | 86 s | | | same |
| hyper-head-advance | the four hyper-oracle commands, renderer under `relax run` | 66 s | 13 s | | same: `hyper-oracle FAIL`, 7/32 corpora |

So relax wins (6×) on a small crate and loses (2 to 4×) on glyph3d-native,
where 65 of the 80 Rust mutations live. Where the time goes, from
`MAKEPAD_RUN_BOOT_TRACE=1 relax test --verbose …` on a fresh mutant
(hyper-pass1-stride-after-advance, 86 s):

- the cache keeps the 344 crates before the edited one in load order and
  collects the 30 after it again ("retained 344 crates; rebuilding 30
  changed/dependent crates"). 29 of them come back from their crate images; only
  glyph3d_native is parsed. That takes 6.1 s (parse CPU 15.3 s, 13.8 s of it
  the proc-macro derives);
- then boot waits **about 75 s** in the allocator swap
  (`jit.rs pause_compile_jobs`: "the allocator swap waits for compile jobs …
  jobs active 7", then 4) before resolve starts at t = 82 s;
- the rest is small: resolve 0.2 s, image 0.2 s, the test itself 0.9 s.

The 75 s wait is the whole gap between relax and cargo. Without it, a new
glyph3d-native mutant would cost about 8 s; with in-process patching (§ 4),
about the test's own run time.

Test execution is also slower under the JIT, which matters for any gate that
runs more than one test. Warm, the glyph3d-native lib suite runs in 23.6 s
(cargo: 8.7 s) and `tests/derived_verbs.rs` in 59.6 s (cargo: 1.8 s). The
whole native package warm is 5 m 57 s of wall time, against 54 s for cargo's
whole workspace, because each test target boots its own process. A
per-mutation run should therefore run the one named test, never the suite.

## 3. Fidelity: unmutated, relax gives rustc's answers

Every check below was run on the unmutated tree, natively and under relax,
and compared:

- **pixel-ab, all 30 renders** (10 golden views × {plain, `--field-mode
  instanced`, `--field-mode visible`}), rendered by the renderer under
  `relax run` and `cmp`'d against `out/tooling-ab/baseline/vulkan-nvidia/`:
  **30 of 30 byte-equal.** This covers glam's matrices, the camera fit, every
  layout path to the GPU, and the shaders as the JIT'd host sets them up.
- **hyper-oracle, the gate's four runs**: every result line identical,
  volumes included (1,349,336 / 1,504,558 / 413,131 / 5,311 records; the
  visible kernel and its span half ran on the GPU under relax).
- **reference-port, five instruments**: identical lines: 1,874,328 per-byte
  fold lanes bit-exact, the scan's 1,188,024 bit-exact and 53,752 within 1e-4,
  530 bake queries, 5,332 reference records.
- **cargo-test**: `relax test --no-fail-fast` over all 7 workspace packages
  against `cargo test --release --no-fail-fast`: the same 376 test names, all
  passing in both. That includes the naga WGSL validation, the GPU tests,
  `fu_to_world_is_the_nearest_f32` (the exact-quotient sweep over every u16)
  and the tab-stop and pagination grids.

From relax's source (`libs/relax/compiler/src/lower.rs`,
`libs/relax/codegen/src/opt.rs`, `libs/relax/backend/src/rir.rs`,
`libs/relax/compiler/std/core/libm.rs`):

- **No FMA contraction.** `Inst::FMulAdd` is emitted only for the `fma`
  intrinsic (`f32::mul_add`, which std routes to it only under `fma_native()`,
  so it rounds once whether or not the host has FMA3) and `simd_fma`.
  `simd_relaxed_fma` on x86-64 is a separate multiply and add, "the same bits
  as rustc" (its x86-64 baseline has no FMA).
- **The optimizer's float identities are the exact ones**: x·1, x/1, x−0,
  x+(−0) fold; x+0 does not (−0 + 0 = +0). This is pinned by
  `float_identity_tests`.
- **NaN payloads** are typed per op (`NanBits::Host / Intel / Neon`): the
  host instruction's NaN, as rustc's codegen leaves it.
- **Float-to-int `as` saturates** with NaN → 0, i128/u128 included.
- **Transcendentals call the platform C libm** on native targets, the same
  library rustc's std calls. The musl port in relax's std is for web targets
  only. `round` is inline, not libm.
- **MXCSR/FPCR** are touched only by an explicit `FpEnv` effect. Nothing
  sets flush-to-zero or denormals-are-zero behind the program's back.
- relax's `KNOWN_GAPS.md` lists no float issue. Its open gaps are a field
  reorder for maybe-unsized structs (gap 1) and derive/bang macro resolution
  by bare name across the graph (gap 2). Either one could make relax's answer
  differ from rustc's, so a verdict from relax is only as good as the
  clean-tree agreement above. § 4's design keeps that check.

## 4. Proposal

**What prove-on-relax should be.** A `--jit` route for the mutations whose
gate is cargo-test or a renderer instrument (hyper-oracle, reference-port,
pick-oracle, pixel-ab), and cargo for everything else, unchanged:

1. **Fidelity first, per gate, per run.** Before any mutant, run each selected
   gate's clean tree both ways, natively (today's clean-tree check) and under
   relax, and require the same verdict and the same result lines. A gate whose
   relax run disagrees FAILS, naming the diff; it never falls back to cargo,
   which would launder a relax defect into a green verdict (§ 6). This is
   the "green first" guard extended to the second compiler. It is also the
   only honest guard against relax's open gaps, since an argument from relax's
   source cannot cover them.
2. **One resident session per test target.** It loads the package harness
   once (warm from the code cache), and per mutation it: applies the mutated
   file's text **in memory** (`reload::apply`; the working tree is never
   written, so the tree is no longer held hostage); asserts the patch applied
   (the outcome's rung and changed-item count; a "restart" rung is reported
   as unprovable here and handed to cargo); runs the test that `expect` names,
   with libtest's filter; requires it to fail with `expect` in its output;
   then calls `reload::rollback`. The renderer instruments need the same loop
   over `relax run`, which needs them to return a status instead of exiting.
   That is a glyph3d change (the `-> !` instruments return an exit code that
   `main` passes to `exit`), small and with no second path.
3. **Restore proven** becomes "rollback proven": rerun the same filter and
   require green before the next mutant.

**What relax was asked for, and what came of it.** Nothing in the makepad
tree was changed from here; relax's owners took these:

1. The ~75 s boot wait after re-collecting an edited crate (§ 2's trace):
   FIXED (an exponential recursion in `drop_may_unwind` over syn's AST,
   memoized). A native mutant on disk went from 62 to 116 s to 5 to 6 s.
2. An in-memory patch: delivered as `MAKEPAD_RUN_OVERLAY` (a file overlay in
   relax's VFS), which is enough for prove: the tree is never written.
3. A crash on re-running a cached native mutant (an AppendVec out of
   bounds): FIXED (NESTED_UNIT, makepad 0635bdd77).
4. A resident `relax test --serve` (patch, run, rollback over the cargo
   graph's harness): designed (§ 4's sketch, and the interface sent to its
   owners: JSON lines per mutant, `{path, new_text}` in), then CANCELLED by
   Ivan's scope cut, as were a per-test coverage map and IR-level generated
   mutations.

Still open, for whoever owns relax next (§ 6): a copy of the run dir at
another path invalidates 49 crates' records, so a mutant's first process
on a cloned cache costs 20 s where the shared cache costs 11 s.

## 5. The prototype: `prove --jit` on relax's overlay

`cargo glyph prove --jit [--mutation …]` (`glyph/src/prove_jit.rs`, behind
the flag, a spike, not a path to merge as is). It routes a mutation when its
gate is cargo-test, it is a `replace` in a Rust file, and its `expect` names
exactly one `#[test]` fn of the file's package. That fn's file picks the
target: `--lib`, `--bins` for a bin-only package (`glyph`), or `--test <stem>`.
Anything else is NOT ROUTED and fails the run. Per mutation:

1. **Green first:** the named test, unmutated, under `relax --allow-c test
   <pkg> <target> -- <test>`, once per test. It must pass at least one test.
2. **The mutant goes to a scratch file**, `target/prove-jit/<name>/<file>`.
   A one-line overlay list maps the file's absolute path to it, and the test
   runs with `MAKEPAD_RUN_OVERLAY=<list> MAKEPAD_RUN_CACHE=readonly`. Readonly
   keeps the mutant out of the cache the clean runs read. Its owners
   recommend it: a cached native mutant re-run crashed relax (an `AppendVec`
   out-of-bounds, being fixed).
3. **The verdict:** the test must fail, its output must contain `expect`, and
   libtest must have reported a result. A relax that died before libtest
   reported is no verdict.
4. **Restore proven** is "never written": the file is byte-compared with what
   it was before the run. Across both runs below, a hash of every tracked
   source in `native/src`, `crates/glyph-scene-graph/src` and `glyph/src` was
   identical before and after.

The selection was two glyph-scene-graph mutations, three in the glyph3d-native
lib, one native integration test that renders on the GPU (`tests/tab_stops.rs`)
and one in the runner. All seven reddened their tests under both routes:

| mutation | test target | `prove` (cargo) | `--jit`, 1st run | `--jit`, warm |
|---|---|---|---|---|
| scene-graph-stale-handle-aliases | glyph-scene-graph --lib | 21.6 s | 1.8 s | 1.6 s |
| scene-graph-cpu-mirror-compose-swapped | glyph-scene-graph --lib | 20.6 s | 1.9 s | 1.5 s |
| lod-ignores-group-scale | glyph3d-native --lib | 31.8 s | 5.5 s | 5.7 s |
| fold-tab-line-advance-rounded | glyph3d-native --lib | 29.0 s | 6.8 s | 7.7 s |
| windowed-chunk-order-reversed | glyph3d-native --lib | 31.5 s | 5.4 s | 7.0 s |
| visible-item-tab-width-dropped | glyph3d-native --test tab_stops | 42.6 s | 51.7 s | 55.3 s |
| since-matches-whole-gate-command | glyph --bins | 20.8 s | 1.9 s | 2.0 s |
| clean-tree checks (once per run) | | 26 s (+22 s final rebuild) | 87 s | 13 s |
| **the run** | | **246 s** | **162 s** | **94 s** |

Readings:

- **The native-lib stall of § 2 did not appear.** A native lib mutant cost
  about 6 s here, against 62 to 116 s on disk with the older binary. Two
  things changed at once: the binary was rebuilt, and the cache was read-only.
  The runs here do not separate those two, so this is not yet a claim that
  ask 1 is fixed.
- **The integration test is the exception.** Its clean run is 11 s warm (cargo
  runs the whole binary in about 1 s), and its mutant 55 s. An integration
  target is its own program with the lib as a dependency, so the edited lib
  crate is re-collected inside it. That is the cost to look at next. 8 of the 54
  routable mutations name a test in an integration target (7 in
  `native/tests/`, 1 in `crates/glyph-field-visible/tests/`).
- **The first `--jit` run's 87 s of clean checks** is relax warming the cache
  after the branch's own edit to `glyph/`, plus the first GPU test run. Warm,
  they cost 13 s.
- What the JIT route proves is narrower than `prove`: the named test fails
  under the mutant, not the cargo-test gate as a whole (the floor, the other
  binaries). The 54 routable cargo-test mutations' declarations only claim the
  former.

## 6. All of it, in parallel, and as the merge bar

Since § 5 (2026-10-26, on relax with both fixes above):

- **Every gate declares its route** in build.toml: `jit = "test"` on
  cargo-test, `jit = "renderer"` on pick-oracle, reference-port,
  hyper-oracle and pixel-ab. A gate with none stays on cargo, by
  declaration and never decided at run time. That covers the 6
  compiler-diagnostic mutations, the data mutations, and a mutation no
  route can take (`tests-silently-ignored`, whose expect is the floor's
  text; `derived-curve-cap-128`, whose test reads its shader from disk).
  Each one is printed with its reason and proven on cargo.
- **The renderer gates take the renderer as a parameter**, `GLYPH_RENDERER`.
  glyph always exports it (default: the built product), and the gate
  command, both scripts, the golden renders, repo-verify and the gpu key all
  read it. `--jit` points it at a two-line wrapper over `relax run`.
- **Two compilers agree, or the route fails loud.** The clean tree runs
  natively (the named tests in one cargo run at workspace scope, then each
  gate) and under relax (one run per test target, one per gate). Each route
  compares its own lines: test by test, or the gate's result lines with
  their volumes. A disagreement fails that route and names the diff; it
  never falls back. On its first try it caught a false one: the
  hyper-oracle gate's PASS line was a wgpu WARN log line, timestamp and all.
  `gate_cmd` now surfaces the child's `pass_line` summaries instead.
- **Parallel and pipelined.** `--jobs` (default 8) mutants at once. A
  route's mutants start as soon as its own clean check agrees. Each mutant
  has its own scratch: overlay, golden renders (`GLYPH_SWEEP`), `TMPDIR`. A
  mutant that is one relax process reads the shared cache read-only. One
  that is many (a renderer gate; a test that spawns the renderer through
  `CARGO_BIN_EXE_*`) runs on a reflinked clone of the cache, which it
  writes and deletes. That took tab_stops' mutant from 49 s, when each of
  its seven renderer processes compiled the mutant again, to 33 s.
- **The cargo route runs beside relax** for Rust mutations. relax reads
  those files from untouched copies through the overlay, so it never sees a
  cargo mutant. Data mutations run after relax, since the renderer reads the
  atlas and fixtures from disk at run time.
- **A golden mutant renders the one view its expect names**, and the gate's
  clean check renders the union of those views. As with the named test, this
  is the claim the declaration makes, not the whole gate.
- **`--since` selects by the expect's test file too.**

Two hazards were found and fixed on the way. A package-scope `cargo test
-p glyph3d-native` re-linked the renderer behind a stamp that still read
current, under a pixel-ab clean check running at the same time; the native
side now builds at workspace scope. And the golden gate asked
`target/release/glyph3d-native` for the gpu key directly, a binary that a
concurrent cargo mutant re-links.

**The full run** (unscoped, `--jobs 8`, before the pipelining and with the
cargo route after relax rather than beside it, host load 9 to 27): **13 min
50 s, all 116 red.** relax: 497 s for 93 mutations, with 138 s of clean
checks over 58 routes, every one agreeing. cargo: 333 s for 23. Per route,
under that load and 8 at a time: lib and bin tests 6.6 s mean (max 15),
integration tests 35.5 s mean, hyper-oracle 43 s, reference-port 31 s,
pick-oracle 49 s, pixel-ab 95 s (all 30 views; one view since). Against it,
`prove` unscoped: 41.4 min, its TIME line 2,482 s over 112 mutations (mutant
builds 454 s, gates under mutation 1,953 s), a sibling's run on a main slightly
before c56ae30; nobody ran cargo's on c56ae30 itself.

**The merge check.** It runs only what a branch touched: `prove --jit
--since <merge-base>`. A real merge stands in for a branch: mp-cpu-twins
(431279e) touched 38 files, which select 27 mutations once its 2 Visible
ones are dropped. Proved at `--jobs 12`, 27 of 27 red:

| | wall | clean checks | mutants |
|---|---|---|---|
| per-route clean checks, then all mutants | 218 s | 115 s | 99 s |
| batched clean checks | 117 s | 53 s | 60 s |
| pipelined, golden views by expect | **107 s** (load 24) | last route agreed at 45 s | done at 104 s |

The tail is the many-process routes. pick-oracle's clean run takes 35.5 s
under relax (3.9 s native; it launches the renderer 29 times) and its
mutant 58 s. The library-region integration test's mutant takes 54 to 58 s.
The cloned cache's first process accounts for about 9 s of each, which the
relax-side fix above would remove.

## 7. Merge-ready: the declared route is prove (2026-10-26, rebased on b42034b)

The spike flag is gone. `cargo glyph prove` proves every mutation as its gate
declares (`relax = "test" | "renderer"`), and a relax gate's mutation is never
handed to cargo. Without a relax that runs (`GLYPH_RELAX`), prove stops before
proving anything and says how to build one. AGENTS.md says so under § The tool.
What changed from § 6:

- **Every mutation of a relax gate has a route.** A test mutation whose
  `expect` names no test (`tests-silently-ignored`, whose claim is the floor)
  runs the whole suite under relax, every test target in parallel, judged by
  the cargo-test gate's own floor rule (`gates::judge_tests`). A renderer
  gate's mutation of a file the renderer reads at run time (`fixture-deleted`)
  is applied on disk, last and one at a time, with its gate under relax.
  `curve_cap.rs` now reads the shaders the renderer embeds (each field crate's
  `include_str!` constant), so `derived-curve-cap-128` rides the overlay too.
- **relax's cache served one mutant stale, until makepad 80ffba413.** A const
  aggregate over another crate's `include_str!` constant kept its cached
  value when the included file changed: on disk and through the overlay, with
  the cache on or read-only. `curve_cap.rs`'s shader array is such an
  aggregate. The repro, 9 lines in two crates, went to relax's owners
  (makepad's mail, `RELAX_STALE_CONST.md`). The cause was relax's file digest,
  which hashed an item only up to its authored semicolon and missed a macro
  expansion inside an interface item. While it stood, `derived-curve-cap-128`
  declared `relax_cache = "off"` and ran uncached (50 s). Since the fix the
  repro reddens with the cache on and read-only, and so does the mutant
  (5.6 s), so the declaration and its route are gone.
- **Cloning the relax cache tolerated nothing.** `cp -r` failed when a clean
  run renamed a cache file mid-copy. `clone_dir` copies file by file and skips
  an entry that vanishes; each entry is content-keyed.
- **Per-mutant scratch** is `GLYPH_SCRATCH`, which the runner's `scratch()`
  and pick-oracle's script honour, beside `GLYPH_SWEEP`.

Measured on b42034b (97 mutations after the test diet), host load 3 to 5:

- `cargo glyph test`: all 15 gates green in 20 s (342 tests, floor 342).
- One dev run over eight mutations, one per route: a named test, a renderer
  gate, renderer WGSL, the suite, the cache-off mutant, a disk mutant, and two
  cargo mutations beside relax. All eight reddened.
- **The merge check** (`prove --since <merge-base>`), standing in the same
  real merge as § 6: mp-cpu-twins (431279e) selects 27 of today's mutations.
  **79 s**, 27 of 27 red: relax's 24 in 76 s (the last clean-tree route agreed
  at 57 s), cargo's 3 in 3 s. Tail: pick-off-by-one 38 s.
- **The cache clone, since makepad a5299abd2**, is `cache/` alone: a
  redirected run dir now keeps its build outputs at the workspace's, so the
  clone no longer invalidates the build-script crates. A renderer mutant's
  first process re-collects 31 crates, not 334: 8.3 s, where it was 20 s (the
  shared read-only cache's is 6.0 s), then 1.2 s a process. pick-off-by-one
  alone went from 38 s to 25 s. The same 27-mutation merge check, run again:
  **96 s**, 27 of 27 red. The last clean-tree route agreed at 38 s, where it
  was 57 s, but pick-off-by-one took 54 s among 12 parallel jobs. So the
  check is load-sensitive, and two runs at host load 3 gave 79 s and 96 s.

The full unscoped run was not repeated (the coordinator's call: `--since` and
`--mutation` are enough).

## Reproducing

From the repo root, with `R` the relax binary:

```sh
cargo glyph prove --mutation fu-to-world-wrong-denominator   # today's figure
GLYPH_RELAX=<relax> cargo glyph prove --jit --mutation fu-to-world-wrong-denominator   # § 5
GLYPH_RELAX=<relax> cargo glyph prove --jit --jobs 12 --since main   # § 6, the merge check
$R test --allow-c glyph3d-native --lib -- fu_to_world_is_the_nearest_f32
# apply the mutation's find/with to native/src/text.rs, rerun the line above
# (first run: the cold-mutant cost; again: the warm one), restore, rerun.
MAKEPAD_RUN_BOOT_TRACE=1 $R test --verbose --allow-c glyph3d-native --lib -- <test>
# the boot trace: "rebuilding N changed/dependent crates", the allocator-swap
# wait, and where resolve starts.
$R test --allow-c --no-fail-fast <package>    # fidelity, per package
# golden frames: from native/, `$R run --quiet --allow-c glyph3d-native --
#   <golden_view.cmd> [<equivalent>] --screenshot <png>`, cmp against the set.
```
