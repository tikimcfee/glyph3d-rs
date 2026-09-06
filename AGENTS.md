# AGENTS.md — glyph3d-native (root)

Orientation for anyone, human or agent, working in this tree. The renderer's
output is the contract: **every refactor must be provably output-neutral**,
proven by running the checks, not by argument.

**This file is canonical for anything repo-wide** — what the checks do, what is
fenced, what the vocabulary means. `native/AGENTS.md` is canonical for the Rust
crate (style, module contracts, debug env vars). `engine/README.md` is canonical
for pipeline internals. When they disagree about a repo-wide fact, this file
wins and the other is stale — say so in your commit rather than patching a
correction on top of the stale text, which is how this file rotted the last
time.

## Layout

- `engine/` — Mojo/MAX glyph pipeline + FFI + conformance suites + fixtures +
  benches. Built by pixi, not cargo.
- `native/` — the Rust/wgpu renderer binary; links `native/libglyph_engine.dylib`.
- `tools/` — the check scripts, the generators, and repro helpers.
- `assets/atlas/` — prebaked glyph-geometry binaries (+ `FORMAT.md`).
- `schema/glyph-identity.json` — layout source of truth (vendored, hash-pinned).
- `out/` — historical reports, proof PNGs, `tooling-ab/baseline/` (the pixel oracle).
- `integration/` — vendored egui 0.36.1 source. A **grep reference only**: the egui
  that actually compiles comes from crates.io via `Cargo.toml`. Patching this copy
  changes nothing.
- `research/` — background surveys.
- `engine-local/`, `.claude/worktrees/` — untracked. `.claude/worktrees/` may hold
  **another agent's in-flight work**: do not read from it, do not stage it, make
  your own. A `git add -A` here has twice committed things nobody intended,
  once as submodule pointers.

## Build

```sh
pixi install                # mojo + max (pins in pixi.toml; pixi.lock is binary — never hand-merge)
pixi run build              # the manifest runner: products current + committed artifacts regenerated
(cd native && cargo build --release)
```

**The dependency graph is declared in `build.toml`** (artifact, input globs,
build command, class) and executed by `tools/glyph.py`. `pixi run build` brings
products current (content-hash stamps, not mtimes) and regenerates committed
artifacts in place — a deliberate, committable act. `pixi run verify` builds
nothing: it asserts currency and byte-compares every committed artifact against
a scratch rebuild. The four baseline PNGs are class **golden**: verified,
never built — the runner refuses. `pixi run build-native` is the one pixi
`depends-on` edge (cargo after build-engine); the rest of the graph is
artifact-level and lives in build.toml because pixi cannot see that cargo
links the dylib.

**`cargo build` does not build the dylib, and this is the trap that has cost the
most time in this repo.** `native/build.rs` only *links* whatever file already
sits at `native/libglyph_engine.dylib`; its `cargo:rerun-if-changed` watches that
file's mtime, not the Mojo sources behind it. So editing `engine/*.mojo` and
running `cargo build` gives you a green build of the previous engine. It has
produced a reported "regression" that was a stale artifact, and a bisect that
compared a rebuilt commit against a non-rebuilt one. Run `pixi run build-engine`
after any `engine/*.mojo` change or branch switch — or just run the battery,
whose first step is exactly that.

Two guards exist because prose was not enough: `build.rs` scans the dylib for the
exported symbol `glyph_engine_fp_probe` and panics if absent (it predates
2026-09-02), and `Engine::new()` calls that probe at runtime and panics if the
dylib was built without `--fp-mode contract=off`.

That flag is load-bearing, not hygiene: FMA contraction fuses multiply-add pairs
and changes results in the last bit, which breaks bit-exactness against the
oracle the whole corpus is built on. `pixi run build-engine` passes it. Never
invoke `mojo build` by hand without it.

GPU work needs Apple Silicon; `pixi.toml` declares `osx-arm64` only.

## Verification — what actually runs

```sh
bash tools/check-all.sh     # or: pixi run check — exit 0 = all green
```

**Twelve gates, with names.** The artifact graph is declared in **`build.toml`**
(artifact, inputs, build command, class: committed / golden / product) and
executed by **`tools/glyph.py`**; `check-all.sh` is a thin shim over
`python3 tools/glyph.py check`, kept because every doc and every pair of hands
reaches for it. Useful forms: `pixi run build` / `verify` (the build/verify
split), `python3 tools/glyph.py gate <name>` (one gate alone),
`... gates` (what each compares and cannot see), `... graph` (the DAG,
including BOTH edges that leave the schema). Gates used to be numbered
positions in one shell script (`N/9`, renumbered twice, one with a fossil
stage letter in its filename); the names below are the live identifiers and
match `[[gate]] name =` in build.toml.

**Run it in a worktree if anyone else is working in this repo.** `check` reads
the WORKING TREE, not HEAD, so another thread's uncommitted edits fail your checks
and tell you nothing about your own change. This has happened. It is a property of
the runner, not of any one language's code, so it applies just as much to pure
engine or tooling work — `native/AGENTS.md` has the worktree setup commands.

For each: what it compares, what makes it red, and **what it cannot see**. The
last is the part worth reading. A check is a claim about a counterfactual, and a
check whose blind spot you don't know is a green you can't price.

**products-current** (was "0"). The dylib is a *product*: nothing to compare
against, it only has to be CURRENT. Currency is a content hash of the declared
inputs (`engine/*.mojo` + the pixi pins), stamped at build time — rebuilt only
when that hash moved, not on every pass. Red only on a Mojo compile error.
Blind to whether the result is *correct* — it exists solely so that nothing
downstream links a stale engine. A failed rebuild is FATAL (the battery stops):
a stale dylib makes every gate below a statement about the wrong binary.

**committed-artifacts** (was "1"). One mechanism per generator, all driven from
build.toml: the trie (`gen_real_trie.py --verify-only`) and the schema
(`gen_schema.py --check`, which also runs the schema's own tier validation) use
generator-native check modes; the four atlas bins are rebuilt by
`export-atlas.mjs` into a scratch dir and `cmp`'d. Red when a generated
artifact is hand-edited, or a generator changes behaviour. Blind to
whether the *inputs* are right: the trie check proves `engine-trie.bin` is a
faithful derivation of `codepoints.bin`/`glyphs.bin`, not that those are correct.

**vendor-hashes** (was part of "1"). The hashes of 22 vendored + derived files
(`vendor-manifest.py --check`; the two `hb.*` files are additionally re-derived
from their ref sources and byte-compared — a semantic pin, not just a hash).
Blind to upstream drift **by design** (a difference there is information, not a
failure), and blind to a vendored file that matches its own recorded hash while
being the wrong revision for the fixtures that depend on it — which has
happened here, and is caught today only by the fixtures gate.

**fixtures** (was "1b"; part of the committed-artifacts gate in the runner).
Rebuilds the corpus in a **scratch copy** of `engine/fixtures` (generators +
vendored inputs + the `../glyph_schema.mjs` edge) and byte-compares against
the committed 25 — the old gate deleted the committed fixtures in place and
restored them with `git checkout`, which needed the restore to be exactly
right. The expected counts (17 pipe + 8 bake) are **declared in build.toml**,
never derived from the tree under test: the old gate `ls`-counted the tree it
was checking, so a deleted fixture lowered both sides of the comparison and
stayed green (measured 2026-09-06: eleven of twelve gates green on a shrunken
corpus; only the fixture.rs pin caught it). The Rust-side pins stay — declared
count in build.toml and hard pin in the test suite are two independent
witnesses, not duplicate coverage. Blind to whether the oracle is *correct* —
it proves reproducibility, not truth.

**engine-suites** (was "2"). Sixteen suites — 11 CPU, 5 on Metal — plus
**ffi_selftest** (wired 2026-09-06; it links the SHIPPED dylib through the real
C ABI after the pinned toolchain was found to miscompile the in-process import
— see its header), plus a compile pass over all six benches (compiled, never
run). Each suite loads fixtures and asserts bit-exact agreement; a failure
raises and exits nonzero. Red on any lane of the ported pipeline disagreeing
with its fixture. Blind in two specific ways worth knowing: `conformance_real`
is **oracle-free** — it folds arbitrary real source and checks the serial and
scan forms against each other, so it catches divergence but never a fault the
two forms share (its header says so, and names the pinned fixtures as the
cover for that case). And the two **instruments** — `fixture_census` and
`fixture_manifest` — assert nothing: neither ever raises. They print. A census
reporting that every field is pinned to a single value would still exit zero.
Read their output; do not count them as gates.

**cargo-build** (was "3"). `cargo build --release`, zero warnings. Red on any
warning rustc emits; a build ERROR is fatal (the battery stops). Blind to
anything silenced with `#[allow(...)]`.

**cargo-clippy** (was "4"). `cargo clippy --release`, zero warnings. Same, for
lints.

**cargo-test** (was "5"). 85+ tests: naga WGSL validation, CLI parity, encase
lane layout, `ItemParams` validation, the layout-seam suites, the wrap-mode
monoid domain, and the reference-port suites. Red when a test fails, when a
whole test binary stops reporting, or when **fewer than `test_floor` tests
actually run** — the floor lives in `build.toml [settings]` now, not in shell.
That floor is a ratchet, not an equality: adding tests never reddens it, and
when the real count rises above it every green run prints a NOTE naming the
number to raise it to — so it cannot decay into a figure far below reality
without saying so. Raise it in the same commit that adds the tests.

The floor exists because the previous form could not fail. It counted
`test result: ok` summary lines and required two; there are exactly two binaries
(`unittests src/main.rs` with 84, `tests/wgsl.rs` with 1), so the threshold was
met by the tree's shape. Verified 2026-09-06: marking three tests `#[ignore]`
left the old check printing `PASS tests green` and the new one printing
`FAIL — 82 tests ran, floor is 85`. This is also the check that holds the two
corpus-size pins (`native/src/fixture.rs`, 17 pipe; `native/src/bake.rs`, 8 bake,
both worded "update deliberately"), so until now corpus protection rested on
those tests continuing to run with nothing asserting that they did. The realistic
loss was never deletion — it is a dropped `mod` declaration or an `#[ignore]`
that outlives its reason, neither of which rustc says a word about. **This matters more than it looks**, because the pins that keep
the fixture corpus from silently shrinking (`native/src/fixture.rs`, 17 pipe;
`native/src/bake.rs`, 8 bake — both worded "update deliberately") live inside
this check. They protect the corpus; nothing yet protects them.

**engine-check** (was "6"), twice. The Mojo engine through the FFI versus
`text::reference_layout`, an independent Rust CPU fold, diffed record-by-record.
Run on `src/main.rs` and on `fixtures/overflow-leads.txt` — the second because
`main.rs` is well-formed UTF-8 by construction and can never reach the
out-of-range decode path where the two implementations actually disagreed in
September 2026. Blind to the **per-item** FFI strategy: this hardcodes the
batched one (`main.rs:127`). Blind to malformed shapes other than the one that
fixture carries.

**pick-oracle** (`tools/check-pick-oracle.sh`; was `check-stage-g.sh` — the `g`
was a fossil stage letter, not a position). Scripted picks and pixel-ray round
trips from the native binary against an independent Python fold oracle. Red on
any pick resolving to the wrong record. One mechanical caution survives the
rename: under `set -euo pipefail` an oracle that exits nonzero inside a command
substitution aborts the script mid-run. The wrapper still reports FAIL, but
every check after the abort point silently did not run. (The missing
`[ -x "$BIN" ]` guard was added when the file was renamed.)

**pixel-ab** (was "8"). `demo`, `text`, `repo-zoom`, `repo-wide` re-rendered
and byte-compared against `out/tooling-ab/baseline/`. This is the **only**
check that sees pixels. In build.toml these four PNGs are class **golden**:
verified, with NO build path — the runner refuses to regenerate them, because
re-baselining is a human act. Red on any change to camera, shading, layout,
shaping or culling that reaches those four frames. Blind to everything outside
them — and it cannot distinguish a regression from an intentional change, which
is deliberate. It is also less all-seeing than it looks. The page-extent origin
seed was renderer-affecting and all four stayed byte-equal, because the seed
only binds for an item with zero records and no fixture had an empty file. It
can see that class today only because `native/fixtures/g-pick-repo/empty.rs`
was added for it. **Do not tidy that file away.** Ask what else these four
frames cannot see.

**repo-verify** (was "8b"), both wrap modes. The per-item and batched FFI
strategies diffed bit-exact at the layout seam — placements, instance bytes and
records — in `down` and `back`. Red when the two paths disagree. Blind to
whether *either* is right: this is strategy-versus-strategy, so a fault shared
by both is invisible. Ground truth comes from engine-check, and only for the
batched path.

**reference-port** (was "9"). Six halves against the JS oracle's recorded
answers, with the volumes it currently clears — quote these when you change it,
because a count that quietly drops is how this check would go vacuous without
going red: parse parity (Rust's fixture loader versus Mojo's over parsed typed
values — 17 fixtures, 11 section checksums each), the trie rebuilt from raw
bytes (17 fixtures, 13,568 entries), the full serial fold over every lane of
every byte (155,136 leaders, 1,872,012 lanes), the scan form across 8 tunings
(17 × 8 = 136 cases, 1,187,896 leader-lanes bit-exact and 53,192 within 1e-4),
the bake and its seed protocol (8 fixtures, 27,315 leaders, 167 checkpoints,
530 queries), and `text.rs`'s independent fold over its declared domain
(4 fixtures, 5,332 records, 47,988 lanes). Two of these carry
explicit anti-vacuity guards — the bake fails if no query ran, the reference
fails if nothing was in domain — which is the right pattern. Blind to a fault
shared by both loaders in parse parity (there is no third parser), and blind,
silently, to any fixture outside `text.rs`'s domain.

### What the whole battery cannot see

Worth holding in one place, because each check's blind spot is defensible alone
and the union is not:

- **Nothing executes the benches.** `engine/bench/*.mojo` is compile-checked only.
- **`ffi_selftest` only covers the single-item C ABI entry.** It was wired into
  `engine/check.sh` on 2026-09-06 (it had been red for an unknown time — the
  pinned toolchain miscompiled the in-process `ffi` import in executable
  codegen, so it now links the SHIPPED dylib and genuinely crosses the
  boundary). It skips the three multi-item fixtures by design; batched-entry
  coverage is repo-verify. The toolchain miscompile class itself is not pinned
  by anything else: gate engine-check uses a (0,0,0) origin and is blind to
  exactly the `origin_x` read that broke.
- **`tools/verify_atlas.py`, `preview_glyphs.py`, `repro_pick_oblique.py`** are
  manual tools, run by **zero** checks. So the atlas bins' structural and semantic
  correctness, and the oblique-pick repro, are exercised by nothing in the battery
  — the atlas is only ever checked for being byte-identical to what it was, which
  says nothing about whether what it was is right.
- **PROVENANCE.md's prose is generated but only partially validated.** The stale
  claims found 2026-09-06 ("NOTHING READS THEM YET" for files `gen.mjs` reads at
  :93 and :175; "all 22 committed fixtures" when there are 25) are fixed, and the
  fixture count is now READ FROM build.toml at generation time — the generator
  refuses to write an unverifiable number. But the rest of the prose is still
  hand-written inside `tools/vendor-manifest.py` and nothing re-validates it;
  the vendor-hashes gate only ever compares hashes and re-derivations.

### Earning a green

A pass is a claim about a counterfactual, so test the counterfactual: break what
a check watches and confirm it reddens. Some of that is now mechanical —
`python3 tools/glyph.py mutate` applies each mutation declared in `build.toml`,
requires the named gate to go red for the named reason, and restores byte-exact.
It reports COVERAGE rather than a pass count, so a gate nobody has proven is
listed as uncovered instead of being counted as working. Eight mutations cover
five of the twelve gates today; seven gates are still uncovered — run it and
read the COVERAGE line rather than trusting this sentence, which has already
gone stale once. Three times this repo shipped a check
that could not fail — a gate asserting on float noise, a ceiling constant no test
protected, and a fixture checksum comparing bytes guaranteed identical before the
command ran. Every one was caught by execution; not one by inspection.

And **a mutation that produced no failure proves nothing until you know it
landed.** "I broke it and nothing failed" and "I failed to break it" print
identically. Assert the edit applied, then read the result. Equally: a source
scan is worth exactly what its match set is worth — "no grep match" is not "does
not exist," which has produced a wrong conclusion here as recently as
2026-09-06.

There is a THIRD outcome, beyond "landed" and "failed to land": **landed in a
region nothing reads.** Measured 2026-09-06 — resolving an out-of-range
codepoint to a real trie block instead of the shared missing block leaves all
sixteen suites, ffi_selftest and both instruments GREEN, because no fixture in
the corpus carries an F5–F7 lead byte; the same edit reddens engine-check,
whose `fixtures/overflow-leads.txt` is the only input in the tree that reaches
that branch. So a mutation's `why` names the CONSUMER it perturbs, not just the
defect it stands for, and "the gate stayed green" is a claim about the corpus
until you have shown the mutated line is on a path the corpus walks.

The same run measured a shared fault. `phantom-row` (the wrap rule counting the
terminating newline's own row) reddens `conformance` on 3 of 17 fixtures and
leaves `conformance_real` green: the serial fold and the scan monoid both call
`rows_for_line`, so the oracle-free cross-form runner cannot see a defect that
lives inside the function they share. That is its documented blind spot, now
with a number against it.

## Fences — generated, vendored, or immutable

| Path | Status | Why it is fenced |
|---|---|---|
| `assets/atlas/*.bin` | generated | `tools/export-atlas.mjs` from `tools/vendor/ref`; hand-edits are reverted by the next rebuild-and-compare |
| `assets/atlas/engine-trie.bin` | generated | `tools/gen_real_trie.py` |
| `engine/glyph_schema.{mojo,mjs}` | generated | `tools/gen_schema.py` from `schema/glyph-identity.json` — **two** edges leave the schema; editing it invalidates the corpus as well as the dylib |
| `tools/vendor/` | vendored, hash-pinned | `vendor-manifest.py --check`; upstream drift is information, not failure |
| `schema/glyph-identity.json` | vendored verbatim | drift means an upstream refresh, not a local edit |
| `native/src/shaders/*.wgsl` | fenced | the naga test pins the shader *set* — that it compiles and exists, not what it draws. The only thing that sees a pixel change is the four-view A/B, whose blind spots are above. That gap is why edits here need their own re-baselined change rather than an ordinary commit |
| `native/fixtures/baseline-view.txt` | IMMUTABLE | it is the input to `text.png`; editing it re-baselines that check silently |
| `native/fixtures/g-pick-repo/empty.rs` | IMMUTABLE, zero bytes | the only input that reaches the page-extent origin seed; deleting it removes a check's ability to see its subject without removing the check |
| `out/tooling-ab/baseline/` | tracked pixel oracle; **golden** in build.toml | changes only on purpose, with a note saying why; the runner refuses to regenerate it |
| `integration/egui/` | vendored reference | never compiled; the real dependency is from crates.io |

Hand-editing a generated file buys a failure on the next run. Regenerate instead
(`pixi run gen-trie` / `gen-schema`, `node tools/export-atlas.mjs`).

**The sibling web repo at `../../viz-web/glyph3d-js` is read-only history.** It is
the JS oracle this engine was ported from, now retired: `tools/vendor/ref` and
`engine/fixtures/inputs/` are revision-pinned snapshots of it, and the two have
deliberately forked. Edits there are invisible to every check here, so they
cannot be verified and cannot be trusted.

Dependency pins (wgpu 30, winit 0.30, glam 0.33, egui 0.36, mojo/max per
`pixi.toml`): no bump without its own pass. The past bumps were done as multi-part
work and their reports (`out/STAGE_H_REPORT.md`, `STAGE_I_REPORT.md`) are worth
reading — but they agree on less than they look like they do, each having
reinvented its own structure, so take the invariant and not the format: **one
dependency per commit, the full battery green before each commit lands** (not
after, in bulk), the resolved version checked against the published manifest
rather than against a plan, a call-site sweep, and the four views `cmp`'d into a
named scratch dir. Record what moved and why.

## Vocabulary — and which numbers are alive

Three numbering schemes exist in this tree and two of them are dead. This is the
single most common way to misread the repo, so:

- **Lettered stages (A–L)** are **history, not structure.** `out/STAGE_*_REPORT.md`
  are records of landed work and keep their names on purpose. There has never
  been a canonical index, and there cannot be one now: A and B have no report and
  survive only as retrospective mentions inside later ones, and D has no report
  either — it has a dedicated doc instead, `engine/README-FFI.md`, whose title
  carries the letter. The convention is
  **retired** — see "Where work lands". Outside `out/`, a stage letter is
  archaeology. When you touch a comment carrying one, prefer the
  substance ("since the layout seam", "since the carrier split") over the letter.
- **`Stage 0`–`4` is ambiguous and binds to two different lists.** The live one is
  the reference port, canonically enumerated with dates and acceptance criteria in
  **`engine/PORT-PLAN.md`** (0 = fixture parity, 1 = trie, 2 = fold, 3 = scan,
  4 = bake). The dead one numbered the layout-seam work and was deleted from
  `engine/BACKEND-PLAN.md`; that file now carries a warning that its own numbered
  list is a *different* list. Some source comments still reference the dead
  scheme unqualified. Name the thing, not the number.
- **Check numbers (0–9, 1b, 8b)** were positions in one shell script and were
  renumbered twice before the gates got names (2026-09-06). The live
  identifiers are the `[[gate]] name =` strings in `build.toml`
  (products-current … reference-port); `python3 tools/glyph.py gates` lists
  them. Old reports and comments still use the numbers — map by name.

## Where work lands

One logical change per commit, with what you ran in the message. Proof PNGs cited
by a report are committed; scratch renders are not. The battery writes to
`out/tooling-ab/sweep/` (untracked).

**The lettered-stage report convention is retired.** It ran C through L and
stopped on 2026-09-03. Since then 34 commits (33 excluding one merge) have landed
the whole reference port, WrapBack, the phantom-row fix and the corpus vendoring,
and **not one filed a lettered-stage report**. Read that precisely: the word
"stage" is still in use — the reference-port commits are `port: stage 0`..`4`,
which is the live numbering, not the retired letters. What stopped is the
`out/STAGE_<LETTER>_REPORT.md` artifact and its template. Do not start a new
letter; `out/STAGE_*.md` stays as history.

Some older handoff notes have not caught up and will tell you otherwise —
`integration/notes/08-view-structure-handoff.md:71` instructs you to file a report
"(house cadence, per AGENTS.md)", and `06` and `09` say the same for their stages.
**Those stages already executed and those instructions are superseded by this
file.** `integration/notes/README.md` still lists 08 and 09 as "PLANNED, not
executed", which is also no longer true.

Multi-part work that moves the pixel baseline or a dependency still deserves a
written note in `out/` saying what changed and what you ran; it just is not a
"stage" any more and does not need that template.

## Read next

- `native/AGENTS.md` — Rust crate rules, module contracts, debug env vars.
- `engine/README.md`, `engine/README-FFI.md` (the C ABI), `engine/TOOLCHAIN.md`.
- `engine/PORT-PLAN.md` — the reference port's live stage record.
- `engine/BACKEND-PLAN.md` — the layout seam and what is planned next.
- `native/src/*.rs` module headers — the real per-module contracts.
- `assets/atlas/FORMAT.md`; `schema/glyph-identity.json` when touching layout.

`out/*_REPORT.md` are **records, not current state**, and at least one states a
fact that later stopped being true (`ENGINE_TOOLCHAIN_REPORT.md` says no node is
needed; checks 1 and 1b both run node). Read them for why something was done, not
for how things are.
