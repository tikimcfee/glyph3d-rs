# TOOLING-PLAN.md — from scripts to a toolchain

Plan of record for the build/verify tooling. Self-contained: read this, not a
conversation. Claims are **[measured]** (someone ran it) or **[reported]** (an
author's account, not independently reproduced here) — do not promote the second.

Current state: the manifest and the testing CLI landed in `a3bd22f`
(`worktree-build-manifest`, 4 commits). `build.toml` declares the artifact graph
and twelve named gates; `tools/glyph.py` is the runner (`build` / `verify` /
`check` / `gate <name>` / `gates` / `graph` / `suites`); `tools/check-all.sh` is
a 27-line shim preserving the old output contract.

## What the branch got right, and should not be undone

- **`golden` as an artifact class.** The four baseline PNGs fit the verify
  interface (render to scratch, compare) and have **no build path** — `glyph
  build pixel-baselines` prints REFUSED and exits 1. Re-baselining is a human
  act enforced by the absence of a mechanism rather than by a rule someone has
  to remember. This is also the clean resolution of the brief's own tension
  (gate 8 "looks like rebuild-and-compare and is not").
- **Declared counts, scratch-copy verification.** The fixture gate rebuilds into
  a scratch copy — the committed corpus is never deleted mid-check, so the old
  `git checkout` restore is gone — and compares against counts declared in
  `build.toml` rather than `ls`-derived from the tree under test. The Rust-side
  pins (`fixture.rs` 17, `bake.rs` 8) stay as an independent second witness.
  [measured: a 17→16 corpus shrink reddens the fixtures gate naming the counts,
  exit 1, committed corpus untouched]
- **Content-hash product stamps** (`native/target/.glyph-stamps/`), not mtime.
  This is what actually closes the edge cargo cannot see.
- **The disagreement with the brief about pixi was correct.** The brief proposed
  pixi as the manifest's home; the author argued that `depends-on` is
  task-ordering and cannot express the edge that matters (cargo links a dylib it
  does not build), so the artifact graph lives in standalone TOML and pixi got
  only the edge it can see (`build-native depends-on build-engine`). Endorsed.
  **Amend `BUILD-BRIEF.md`** so the next reader does not re-litigate it.
- **`hb.cjs` / `hb.wasm` were load-bearing atlas inputs pinned by nothing.**
  Found beyond the brief, now in a DERIVED table with a *semantic* pin —
  re-derived from their ref sources and byte-compared, not merely hashed. This
  is the same defect class as the vendored-kernels incident: a file that matches
  its own recorded hash while being the wrong thing.

## The finding that reshapes this plan

**[reported]** Wiring `ffi_selftest` did not merely turn on a dormant
instrument — the suite was red, and the cause was not a stale instrument. The
pinned Mojo nightly **miscompiles `ffi.mojo`'s descriptor read when compiled
into a test executable** (uninitialized read, per-compilation-unit victim,
reproducible at `-O0..-O3`), while the *same source* built as the shipping
dylib is bit-exact. The suite was re-architected to link
`native/libglyph_engine.dylib` through `external_call` and cross the real C
ABI, with ABI constants restated literally so engine drift reddens it.

The general rule this establishes, and the reason it belongs in a tooling plan
rather than an engine note:

> **Build mode is part of the verification surface. Verify the artifact that
> ships, not a recompilation of its sources.**

This is the repo's existing rule — *a verification surface must carry values the
same way the thing it verifies does* — one level up, at the build level. A check
that recompiles sources under different flags to inspect them is checking a
different artifact than the one that ships, and can be green while the shipped
thing is wrong, or red while it is right.

Two consequences to carry:

1. **`ffi_selftest` is now the only thing covering this miscompile class.**
   `--engine-check` is blind to it because it lays out at origin `(0,0,0)` and
   never meaningfully exercises the `origin_x` read that broke. A single
   instrument covering a whole failure class is a single point of failure.
   **Proposed, small:** give `--engine-check` a non-zero-origin case so the
   class has two independent witnesses. Cheap, and it follows the same
   two-witness pattern already used for the corpus counts.
2. `mojo run` can no longer execute `ffi_selftest.mojo` (build-and-execute
   only). Anything that assumes `mojo run` works on every engine file is now
   wrong.

## Review notes — to fix on trunk

1. **`needs` and `compare` are declared in `build.toml` and read by nothing.**
   [measured: enumerated every manifest key `glyph.py` accesses; neither
   appears, including in `cmd_graph`.] `cmd_check` iterates the gate array in
   order, so ordering is still line order — moved from a shell script into a
   TOML list. Prerequisite protection does exist, but hand-written three times
   (`glyph.py:220`, `:263`, `:500`). The brief asked to remove three
   hand-written variants of rebuild-and-compare; three hand-written variants of
   prerequisite-checking took their place. A new gate that declares `needs` and
   omits its hand-written guard is unprotected while looking protected. This is
   the single strongest argument for the typed rewrite below: in Rust an unread
   struct field is a `dead_code` warning, and this repo gates on zero warnings,
   so this exact bug becomes uncommittable.
2. **`cargo-test` carries `blind_to = "nothing, structurally — that is what the
   floor is for."`** In the field whose entire purpose is honest blind-spot
   accounting. The floor closes coverage-*count* erosion; it says nothing about
   a test that runs, passes, and asserts nothing — the failure this repo has hit
   three times. Replace with what it is actually blind to.
3. Minor: `pixi run build-engine` nests `pixi run mojo` inside a pixi task.
4. Known and accepted by the author: the `set -euo pipefail` mid-run abort in
   the pick oracle is documented, not fixed. Step 5 below is where it dies.

## The rewrite: a toolchain, not scripts

**The thesis, and every step is one move of it: bring the tool inside its own
regime.** This repo verifies things by breaking them. The build tool is the one
component exempt from that — nothing checks the checker. `tools/glyph.py` has no
tests; if it silently stopped running a gate, or ran one whose command had
rotted, every remaining gate would still print PASS and the battery would end
`ALL GATES GREEN`. The rewrite is not "shell is bad, types are good". It is:
make the tool a thing this repo can break on purpose, the way it breaks
everything else.

That is also the real reason to choose Rust over hardening the Python. A runner
that lives as an `xtask` crate is compiled by the build it manages and tested by
`cargo test` — which means it inherits the zero-warning gate, the `TEST_FLOOR`
ratchet, and the mutation battery below. The tool ends up covered by the
instruments it runs. `mypy --strict` gets types; it does not get that.

**Non-goals, explicitly.**
- **The independent oracles stay in their own languages.** `tools/g_pick_oracle.py`
  and the vendored JS oracle inputs are valuable *because* their lineage differs
  from the code they check. Porting them to Rust makes them a second Rust
  implementation that can share a fault with the renderer, weakening the check
  while it stays green. Python as a second lineage is deliberate and stays. The
  scripts that ORCHESTRATE are in scope; the implementations that ADJUDICATE are
  not.
- The generators (`gen_schema.py`, `gen_real_trie.py`, `vendor-manifest.py`) are
  not in scope. They produce committed artifacts verified by byte-comparison;
  rewriting them buys nothing and risks byte drift.
- Not a re-litigation of `build.toml`'s shape. The manifest is good. It is being
  given a type.

### Step 1 — make catching power machine-checkable

**STARTED 2026-09-06.** `glyph mutate` ships with four declared mutations, all
reddening their gates for their stated reasons [measured]; the harness itself was
verified by a knowingly-false mutation, which it correctly refused
(`gate stayed GREEN under the mutation`, exit 1). Coverage today: **12 gates,
3 with mutations, 9 uncovered** — `cargo-build`, `cargo-clippy`, `engine-check`,
`engine-suites`, `pick-oracle`, `pixel-ab`, `products-current`, `reference-port`,
`repo-verify`. Extending that list is the remaining work, cheapest first.

**`cargo-mutants`: evaluated, verdict is "occasional, scoped, never in the
battery".** [measured 2026-09-06] 875 mutants executed, 383 survivors, **one**
real defect. The reason the ratio is that bad is the finding worth keeping: the
tool runs ONLY `cargo test`, and cargo test is not this repo's main verification
surface. `diff_full_fold` / `diff_scan` / `diff_bake` are `pub fn`s driven by the
BINARY from `tools/check-fixture-parity.sh`; no `#[test]` calls them, so the
whole 1.87M-lane corpus is invisible to it. Proven rather than argued:
`Slots::advance -> 0.0` in `fold.rs` is missed by all 85 tests and reddens
reference-port on 17/17 fixtures. Most survivors are covered code seen through
the wrong lens.

It also cannot work in its default mode here — the cargo root is `native/`, so
copy-to-scratch loses `../engine`, `../assets` and `../.pixi`; `--in-place` is
required, which mutates the working tree that `check-all` reads, and a killed run
leaves the mutation behind (observed three times). Full crate ≈ 9.3 h serial;
scoped to `main.rs` / `layout.rs` / `gpu.rs` ≈ 45 min. Config and the full
rationale live in `native/.cargo/mutants.toml`. The `--in-diff` mode needs
`--relative` or it silently reports "No mutants to filter" — a green for the
wrong reason.

**It earned its keep once, and that was worth the whole evaluation.** Seven
mutations of the hex colour decoder in `parse_verb` survived the entire
twelve-gate battery, because `verb_defaults_and_forms` asserted two channels of
one badly-chosen colour: `ff0080`, whose red is `0xFF` (surviving `& -> |`),
whose green is `0x00` (surviving `>> -> <<`), and whose blue was never read.
Fixed; the test now asserts all three channels of `0x123456` and exercises the
explicit-colour arity path, and all four reported mutations are caught
[measured].

What has no off-the-shelf equivalent is the artifact-level half — and that is
where every other finding of 2026-09-06 came from: the `ls`-derived count, the
inert `needs`, the unfailable test threshold. `cargo-mutants` would have caught
none of those, and `glyph mutate` cannot see what it saw. The two tools are
disjoint, which is the argument for having both and for keeping each out of the
other's way.

*Worth doing whether or not the rest happens, and the acceptance criterion for
every step that follows.*

**The idea.** A gate's entire value is what it rejects; its green tells you
nothing you did not already assume. Today the claim "this gate would catch X" is
established by a human running a mutation once, writing a sentence about it, and
moving on — the branch author did seven, I did two, there are twelve gates, and
none of those acts is repeatable. So the claim decays exactly like every other
unmaintained number in this repo. The battery turns each gate's catching power
into an executable assertion: for a named defect, mechanically applied, **the
stated gate must go red for the stated reason** — and it reports which gates have
no such assertion at all, so uncovered gates are visible rather than assumed.

**Implementation.** Declare mutations next to the gates they exercise, so a gate
without one is a hole you can see: `[[gate.mutation]]` with a `name`, an `apply`
(a patch, a byte edit, a file move), the gate it must redden, and a fragment the
failure text must contain. The runner then enforces five things, each of which
exists because this repo has been burned by its absence:

1. **Green first.** Confirm the gate passes before mutating. A red on a tree that
   was already red proves nothing — this is the "used only as a mutation target,
   never run clean on the branch that broke it" trap.
2. **Assert the mutation landed.** "I broke it and nothing failed" and "I failed
   to break it" print identically. A failed `apply` is silent; check the edit is
   present before believing any result.
3. **Rebuild what the mutation invalidates**, before running the gate. A harness
   here once restored source without rebuilding, so the next run tested the
   previous mutation and produced three unattributable reds.
4. **The right gate, for the right reason.** A mutation that reddens some *other*
   gate is not evidence for this one. Match the declared gate and the declared
   text fragment.
5. **Restore byte-exact, and prove it.** Apply in a scratch copy where possible;
   where it must touch the tree, restore by explicit path and assert `git status`
   is clean afterward. Never `git checkout -- <dir>` — that once reverted the
   generators and vendored inputs alongside the fixtures and ate a live edit.

Report as coverage, not as a pass count: *"12 gates, N with mutations, M
uncovered"*. A battery that says "9 mutations passed" repeats the exact error the
`ls`-derived fixture count made — describing the size of what you did instead of
the size of what exists.

**Cost, and therefore cadence.** This is not part of `check`. Cheap mutations (a
byte in a fixture, a declared count, a file moved aside) run in seconds; expensive
ones (anything reddening `pixel-ab` or `cargo-test`) need a rebuild and run in
minutes. Tier them, expose `mutate [--gate <name>]`, and run the full battery
deliberately — before and after any change to the tooling, which is precisely
when a gate is most likely to quietly stop working.

### Step 2 — put the runner where the instruments can reach it

Land the cargo layout **for the reason above**, not as plumbing. There is no
workspace today; `native/Cargo.toml` is a leaf [measured], so an `xtask` crate
means a workspace root or a sibling crate, and that changes how `native/` builds.
Do it alone, with no behaviour change: battery still green through `glyph.py`,
`cargo xtask` exists and does nothing. The step is complete when the empty runner
is already subject to the zero-warning gate and its (zero) tests are counted by
the ratchet — that is the property being bought.

### Step 3 — make the manifest a checked artifact rather than a document

`build.toml` into serde structs with `#[serde(deny_unknown_fields)]`, so a typo
is a hard error instead of a silently ignored key. `kind` becomes an enum. And
**`needs` becomes an executable DAG with a topological sort** — which is the fix
for review finding 1, and the clearest instance of the thesis: today the graph is
prose that happens to sit in a data file, and a declaration nothing reads is
indistinguishable from a comment. Once the runner orders gates *by* `needs`, a
wrong edge produces a wrong run instead of a wrong impression, and an unread
field is a `dead_code` warning against a zero-warning gate. Port the read-only
commands first (`graph`, `gates`); they cannot break a build.

### Step 4 — port gates differentially

One at a time, both runners live, and the differential is itself the check: they
must agree on greens **and** on every red the step-1 battery produces.
Disagreement on a failure is worth more than agreement on a pass — it means one
of them is wrong about what a defect looks like, which is the only thing either
is for.

### Step 5 — absorb the orchestration shells, hazard first

`tools/check-pick-oracle.sh` leads, because its `set -euo pipefail` mid-run abort
silently skips every check after the abort point while still reporting a single
FAIL — a defect that exists *because* it is shell, and the concrete argument for
this entire plan. Then `tools/check-fixture-parity.sh`, then `engine/check.sh`.

### Step 6 — delete `tools/glyph.py`

When the typed runner passes the same battery. Not before.

**Fold in when convenient:** `compare` and `blind_to` are already data. A
`cargo xtask docs` that generates the AGENTS.md verification section from them
closes the drift loop permanently — the failure mode this repo spent 2026-09-06
fixing by hand stops being possible.

**Costs, priced honestly.** A compiled runner must compile before it can tell you
why your build is broken; a shell script always runs. A new gate *kind* becomes a
code change rather than a config line — correct, since a genuinely new kind of
check is new code, but it changes how the tool feels. And step 2 is a real
structural change to the cargo layout, not a formality.

**The cheaper alternative, so the choice is deliberate:** keep Python, add
`mypy --strict` and a schema for `build.toml`, and do step 1 anyway. That gets
manifest validation and catching-power coverage without the cargo restructure.
What it does not get is the tool inside its own regime — the runner stays
untested by the suite it runs, and the unused-field warning that would have
caught finding 1 never fires.

## Open

- Whether to take the typed rewrite (step 2 onward) or stop after step 1 plus
  the three review fixes. Step 1 is worth doing either way.
- The non-zero-origin case for `--engine-check`, per the miscompile finding.
- The 19 remaining `Stage N` comment sites in `native/src/*.rs` (the four in
  `tools/` are resolved by the `check-pick-oracle.sh` rename). Mechanical,
  zero-risk, unscheduled.
