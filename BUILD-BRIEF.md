# Brief: build the build system

For an agent with no prior context on this repo. Everything needed is here or
cited by path. Claims are marked **[measured]** (someone ran it) or
**[inferred]** (read from source, not executed) — do not promote the second kind.

## The repo, in ten lines

`glyph3d-native` renders source code as millions of 3D text glyphs. Four
languages: **Mojo** (`engine/*.mojo`, the compute engine — the runtime layout
fold), **Rust** (`native/src/`, the renderer, wgpu), **JS** (vendored, used
only to generate the conformance corpus), and **Python** (`tools/` — three
generators plus the independent pick oracle a gate diffs against). The Mojo
compiles to a C-ABI shared
library that Rust links. Correctness rests on 25 binary fixtures that every
implementation is diffed against bit-exactly.

Toolchain is **pixi** (`pixi.toml` at the repo root) for Mojo/max, and **cargo**
for Rust (`native/Cargo.toml` — a leaf, there is no root workspace **[measured]**).

## Why pixi is above native/, which looks odd and is not

`pixi run build-engine` compiles `engine/ffi.mojo` with `-I engine` (so the whole
directory, the generated schema included, is an input) into
`native/libglyph_engine.dylib`. `native/build.rs` only **links** whatever file is
already there — it does not build it, and says so in its header. So cargo's
output depends on pixi's output; cargo cannot be the outer build. The dylib is
written *into* `native/` so build.rs finds it (via `CARGO_MANIFEST_DIR`, an
absolute path off the crate root — not a relative one) — a placement
choice, not a layering error.

## The artifacts, and the two classes that matter

| artifact | built by | class |
|---|---|---|
| `engine/glyph_schema.mojo` + `.mjs` | `tools/gen_schema.py` from `schema/glyph-identity.json` | committed |
| `assets/atlas/*.bin` (4) | `tools/export-atlas.mjs` from `tools/vendor/ref/**` | committed |
| `assets/atlas/engine-trie.bin` | `tools/gen_real_trie.py` | committed |
| `engine/fixtures/*.{pipe,bake}.bin` (25) | `engine/fixtures/gen.mjs`, `gen-bake.mjs` from `engine/fixtures/inputs/*.js` | committed |
| `native/libglyph_engine.dylib` | `pixi run build-engine` from `engine/*.mojo` | **product** |
| `native/target/release/glyph3d-native` | `cargo build --release` from `native/src/**` | **product** |
| `out/tooling-ab/baseline/*.png` (4) | the binary, **by hand, deliberately** | committed |

**COMMITTED artifacts** are verified by rebuilding and byte-comparing, and that
half already exists — but unevenly, which is itself part of the job. THREE of the
six generators expose a check mode (`gen_schema.py --check`,
`gen_real_trie.py --verify-only`, `vendor-manifest.py --check`) **[measured]**;
`export-atlas.mjs`, `gen.mjs` and `gen-bake.mjs` have none, and are verified
instead by the gate rebuilding them into a temp dir or deleting and regenerating
them in place. Same guarantee, three different hand-written mechanisms. What is
missing is the graph, and the uniformity.

**PRODUCTS** are untracked (`.gitignore` has `*.dylib`) and have nothing to
compare against. They only need to be CURRENT before anything reads them.

## The gap: nothing declares a dependency

`pixi.toml` has tasks (`suites`, `gen-trie`, `gen-schema`, `check-gen`,
`build-engine`, `check-all`) and **no `depends-on` anywhere [measured]**, though
pixi supports it. So this real chain is held only by line order in a shell
script:

```
schema/glyph-identity.json
  -> tools/gen_schema.py  -> engine/glyph_schema.mojo
  |                           -> pixi run build-engine  (mojo -I engine: the schema is an INPUT)
  |                           -> cargo build            (links the dylib)
  \_______________________ -> engine/glyph_schema.mjs
                              -> engine/fixtures/gen.mjs  (imports ../glyph_schema.mjs)
                              -> the 25 fixtures
                              -> every conformance gate
```

TWO edges leave the schema, not one **[measured]** — `gen_schema.py` emits both
halves and `gen.mjs:60` imports the `.mjs`. So editing the schema
staleness-invalidates the CORPUS as well as the dylib, and for a document whose
thesis is "nothing declares a dependency", the second edge is the more
interesting one.

Edit the schema and nothing knows the dylib is stale. **This class of failure
has already cost real time here [measured]:** `glyph_engine_load_item` used to
take twenty positional parameters; inserting one in the middle shifted every
later argument, so a binary built against the new signature calling a stale
dylib received a parameter where `has_page` should be, pagination silently
switched off, and every paged glyph landed somewhere else. Wrong answers, no
error. It was fixed by making that entry point take a descriptor rather than
registers, and by renaming the symbol so a stale dylib fails to LINK.

## Instruments that exist; three were wired only on 2026-09-04

`tools/check-all.sh` is the gate runner — twelve gates, though the headings
still say "N/9" **[measured]**. Recently added, each for a check that existed
and was never consulted:

- **gate 0** builds the dylib, because nothing did and `cargo build` does not.
- **gate 1b** deletes all 25 fixtures and rebuilds them, asserting byte-identity.
  (Unlike 0 and 8b, this one is not a check that existed and went unconsulted:
  before `f68b70f` the corpus could not be rebuilt in this tree AT ALL, because
  `gen.mjs` imported from `../../packages/`. Vendoring the inputs was the work;
  the gate came with it.)
- **gate 8b** runs `--repo-verify` (per-item vs batched FFI, bit-exact).

**Still unwired:** `engine/ffi_selftest.mojo` is neither run nor compiled by
`engine/check.sh` — 0 occurrences **[measured]**. It could have been broken for
some time with nothing saying so.

The pattern worth internalising: this repo does not lack instruments, it lacks
an account of which ones RUN. Four found in one day, one at a time, by accident.

## What to build

1. **One declarative manifest** — artifact, inputs (globs), build command,
   class. Pixi is the right home (it is already the toolchain boundary and
   supports `depends-on`), but argue if you disagree.
2. **Two modes over it.** `build` brings products current and regenerates
   committed artifacts. `verify` rebuilds committed artifacts to a scratch path
   and byte-compares. `check-all` should then assert properties of artifacts
   someone else built, instead of being the thing that remembers to build them.
3. **Subsume, do not add.** Gates 1, 1b and 8 are three hand-written variants of
   "rebuild and compare", each with its own restore logic, its own way of
   counting what it covered, and its own failure text. That divergence is
   already the cost: a fix to one reaches none of the others, and one of the
   three (1b) counts its own coverage off the tree it is checking while the
   other two do not. A fourth variant means four places to audit and four
   places for the next blind spot to sit unnoticed.
4. **Wire `ffi_selftest.mojo`** into `engine/check.sh`, or delete it with a
   stated reason. An instrument nothing runs is an absent one.
5. **A testing CLI** the owner has asked for: a single entry point for the
   flows currently spread across `tools/check-all.sh`, `tools/check-stage-g.sh`,
   `engine/check.sh`, `pixi run suites`, and ad-hoc `--repo-verify` /
   `--engine-check` invocations. Shape is yours to propose.

## What NOT to do

- **A build must never bring `out/tooling-ab/baseline/*.png` current.** They are
  derived, but re-baselining is a deliberate act and those four byte-equal
  screenshots are the only PIXEL gate. Not the only thing that catches a
  renderer change, and the difference matters: the page extent's origin seed was
  renderer-affecting and all four stayed byte-equal through it — a unit test
  caught it, and gate 8 can see it today only because a zero-byte fixture was
  added for it. In the manifest they are verified, never built. And ask what
  else gate 8 cannot see.
- **Do not weaken a gate to make it fit the graph.** If a gate resists
  generalisation, that is information. The pixel A/B is the worked example: it
  looks like the other rebuild-and-compare checks and is not one. The others
  reconstruct an artifact from its inputs and diff the result; this one re-runs
  the whole renderer and compares against a golden master that **cannot be
  derived from anything**. Fitting it to a "rebuild from inputs" manifest means
  either teaching the manifest to rebuild it — which is the forbidden
  re-baseline — or dropping it. It resists because it is a different kind of
  check, and the resistance is the signal.

- **The corpus-size pins are not in the gate runner, and are not redundant.**
  `native/src/fixture.rs` pins 17 `.pipe.bin` and `native/src/bake.rs` pins 8
  `.bake.bin`, each with "update deliberately" in the message. Everything else
  that touches fixtures GLOBS — gate 1b `ls`-derives its count, `engine/check.sh`
  globs into both instruments, and gate 9 reports whatever it was handed. That is
  fine BECAUSE the two pins exist. Verified 2026-09-06 [measured]: deleting one
  case from `gen.mjs`'s `CASES` and `git rm`-ing its `.bin` left eleven of twelve
  gates green — gate 1b printed `PASS 24 fixtures ... BYTE-IDENTICAL` and gate 9
  printed `16 fixtures` — and the battery went red only at gate 5, on the pin.
  So when you subsume gate 1b, carry a DECLARED corpus size into the manifest or
  leave the pins alone; do not retire them as duplicate coverage, and do not
  copy 1b's habit of asking the tree how big the tree is.
- **Do not edit `/Users/lugo/localdev/viz-web/glyph3d-js`.** It is the JS
  renderer this engine was ported from — the original oracle. Every expected
  answer in the conformance corpus traces back to it, but by way of
  **revision-pinned snapshots vendored into this repo** (`tools/vendor/ref/`
  and `engine/fixtures/inputs/`), and the two have deliberately forked since.
  Nothing here reads that repo at build time. So an edit there changes no
  output, is verified by nothing, and silently desynchronises your mental model
  from the pinned inputs the checks actually use.
- Note `.claude/worktrees/` may hold other agents' in-flight work. Do not touch
  it; make your own worktree.

## The verification standard here

Read `native/AGENTS.md` first. The house rule is that a green must be earned:
break what a check watches, confirm it reddens, and **assert your edit landed**
before believing a null result. Specific traps this repo has actually hit
**[all measured]**:

- A vendored file with a recorded sha256, gate-checked on every run, that could
  not have produced the fixture it was vendored for. The gate proved the file
  matched its own hash and never asked whether it was the right file.
- A mutation harness that restored source without rebuilding, so the next run
  tested the previous mutation and reported three unattributable reds.
- `git checkout -- <dir>` used to restore fixtures, which also reverted the
  generators and vendored inputs in that directory and silently ate a live edit.
- A check used only as a MUTATION TARGET and never run clean on the branch that
  broke it. A mutation reddening a check proves the check works; it does not
  prove the check was consulted.

One live example, so the standard is not abstract. The `cargo test` step
passes when the run exits zero **and** prints at least two `test result: ok`
lines. There are exactly two test binaries, so that floor is satisfied by the
shape of the tree regardless of what is inside them: 85 tests today, and
deleting 84 of them would leave this green [measured 2026-09-06]. It is also
the step that holds the two corpus-size pins. Do not generalise this one into a
manifest as though it were a working check — either leave it alone or pin a
real count, but decide it deliberately.

`tools/check-all.sh` must end `CHECK-ALL: ALL GATES GREEN`, including the four
byte-equal screenshots, before and after your change.

## Vocabulary you will meet, and which parts are dead

Three numbering schemes appear in this tree; two are historical and will
mislead you.

- **Lettered stages (`Stage A`..`Stage L`)** name past batches of work recorded
  in `out/STAGE_*_REPORT.md`. There is no index and never was — A, B and D have
  no report. Treat any stage letter outside `out/` as archaeology, including the
  `g` in `tools/check-stage-g.sh`, which is a fossil letter and not a position
  (that script is the pick oracle; one caller, cheap to rename if you are
  touching `check-all.sh` anyway).
- **`Stage 0`-`4` is ambiguous**: it names both the live reference port
  (canonically `engine/PORT-PLAN.md`) and a dissolved layout-seam list deleted
  from `engine/BACKEND-PLAN.md`. Some source comments still use the dead one
  unqualified.
- **Check numbers (`gate 1b`, `gate 8b`)** are positions in one shell script,
  already renumbered twice, printed as `N/9` for twelve steps.

Prefer names over numbers in anything you write. `AGENTS.md` names every check.

## One more thing the manifest could fix

`tools/vendor/PROVENANCE.md` is a **generated, committed** file whose prose is
hand-written inside `tools/vendor-manifest.py` and never re-validated. It
currently states that two vendored oracle files have no reader ("NOTHING READS
THEM YET") when `engine/fixtures/gen.mjs` reads both at `:93` and `:175` to
build two fixtures, and it says "all 22 committed fixtures" when there are 25.
The `--check` mode runs on every pass and only ever compares hashes. A generated
artifact whose *content* is verified and whose *claims* are not is exactly the
gap a real manifest should close.
