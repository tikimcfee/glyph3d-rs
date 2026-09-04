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

`pixi run build-engine` compiles `engine/*.mojo` into
`native/libglyph_engine.dylib`. `native/build.rs` only **links** whatever file is
already there — it does not build it, and says so in its header. So cargo's
output depends on pixi's output; cargo cannot be the outer build. The dylib is
written *into* `native/` so build.rs finds it by relative path — a placement
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
  -> tools/gen_schema.py    -> engine/glyph_schema.mojo
  -> pixi run build-engine  (mojo -I engine, so the schema is an INPUT)
  -> cargo build            (links the dylib)
```

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
   "rebuild and compare". A fourth variant is a regression.
4. **Wire `ffi_selftest.mojo`** into `engine/check.sh`, or delete it with a
   stated reason. An instrument nothing runs is an absent one.
5. **A testing CLI** the owner has asked for: a single entry point for the
   flows currently spread across `tools/check-all.sh`, `tools/check-stage-g.sh`,
   `engine/check.sh`, `pixi run suites`, and ad-hoc `--repo-verify` /
   `--engine-check` invocations. Shape is yours to propose.

## What NOT to do

- **A build must never bring `out/tooling-ab/baseline/*.png` current.** They are
  derived, but re-baselining is a deliberate act and those four byte-equal
  screenshots are the only gate that catches an unintended renderer change. In
  the manifest they are verified, never built.
- **Do not weaken a gate to make it fit the graph.** If a gate resists
  generalisation, that is information.
- **Do not edit `/Users/lugo/localdev/viz-web/glyph3d-js`.** Historical
  reference, read-only.
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

`tools/check-all.sh` must end `CHECK-ALL: ALL GATES GREEN`, including the four
byte-equal screenshots, before and after your change.
