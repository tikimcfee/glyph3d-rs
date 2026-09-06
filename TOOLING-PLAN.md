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

**Goal.** The build/verify tooling becomes a typed, compiled program that can
guarantee properties of itself. Not because shell is beneath us — because a
declaration nothing reads, a stale count, and a mid-run abort that silently
skips later checks are all things a compiler catches for free and a script
cannot.

**Non-goals, explicitly.**
- **The independent oracles stay in their own languages.** `tools/g_pick_oracle.py`
  and the vendored JS oracle inputs are valuable *because* their lineage differs
  from the code they check. Porting them to Rust makes them a second Rust
  implementation that can share a fault with the renderer, weakening the check
  while it stays green. Python as a second lineage is deliberate and stays.
- The generators (`gen_schema.py`, `gen_real_trie.py`, `vendor-manifest.py`) are
  not in scope. They produce committed artifacts verified by byte-comparison;
  rewriting them buys nothing and risks byte drift.
- Not a re-litigation of `build.toml`'s shape. The manifest is good. It is being
  given a type.

**Order, chosen so each step can fail.**

1. **Make catching power machine-checkable, before anything moves.** Today
   "does this gate still catch?" is a human running mutations by hand. A
   build-system rewrite validated by "it is still green" is validated by the one
   signal that proves nothing. Deliverable: a mutation battery — per gate, a
   named defect it must redden on, run as a command. The branch author ran seven
   such mutations by hand and recorded them [reported]; this step makes that
   repeatable rather than a one-time act. **This has standalone value even if
   the rewrite stops here**, and it is the acceptance criterion for every step
   below.
2. **Cargo layout, no behaviour change.** There is no workspace today —
   `native/Cargo.toml` is a leaf [measured]. An `xtask` crate needs a workspace
   root or a sibling crate, and that touches how `native/` builds. Land it
   alone: battery still green through `glyph.py`, `cargo xtask` exists and does
   nothing.
3. **Typed manifest, read-only commands first.** `build.toml` into serde structs
   with `#[serde(deny_unknown_fields)]` — a typo becomes a hard error instead of
   a silently ignored key. Port `graph` and `gates` first; they only read.
   **`needs` becomes a real DAG with a topological sort**, which is where
   finding 1 stops being possible.
4. **Port gates one at a time, both runners side by side.** They must agree on
   greens *and* on the mutation battery's reds. Disagreement on a failure is
   more informative than agreement on a pass.
5. **Absorb the orchestration shells**, hazard-carrying first:
   `tools/check-pick-oracle.sh` (the `set -e` abort that silently skips every
   check after the abort point), then `tools/check-fixture-parity.sh`, then
   `engine/check.sh`. These are the scripts whose complexity is the actual
   argument for this work.
6. **Delete `tools/glyph.py`** when the typed runner passes the same battery.
   Not before.

**Fold in when convenient:** `compare` and `blind_to` are already data. A
`cargo xtask docs` that generates the AGENTS.md verification section from them
closes the drift loop permanently — the failure mode this repo spent 2026-09-06
fixing by hand stops being possible.

**Costs, priced honestly.** A compiled runner must compile before it can tell
you why your build is broken; a shell script always runs. `kind` becoming an
enum means a new gate type is a code change rather than a config line — correct,
since a genuinely new kind of check *is* new code, but it changes how the tool
feels to use. And step 2 is a real structural change to the cargo layout, not a
formality.

**The cheaper alternative, so the choice is deliberate:** keep Python, add
`mypy --strict` and a schema for `build.toml`. Most of the manifest-validation
win, no cargo restructure. It does not get the unused-field warning that would
have caught finding 1, and the orchestration shells survive.

## Open

- Whether to take the typed rewrite (step 2 onward) or stop after step 1 plus
  the three review fixes. Step 1 is worth doing either way.
- The non-zero-origin case for `--engine-check`, per the miscompile finding.
- The 19 remaining `Stage N` comment sites in `native/src/*.rs` (the four in
  `tools/` are resolved by the `check-pick-oracle.sh` rename). Mechanical,
  zero-risk, unscheduled.
