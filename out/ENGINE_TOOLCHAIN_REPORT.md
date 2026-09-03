# ENGINE / TOOLCHAIN REPORT — the JS dependency is gone, and the gates grew

Merged to `main` as `a37a847` (+ `7cd9d55`), 2026-09-02. Nine commits from
`worktree-python-generators`, merged with **zero conflicts** against stage-K —
exactly one file overlapped (`native/src/main.rs`, two lines: a doc comment and
a message string).

Written for whoever is iterating in this repo. Ordered by what will interrupt
you, not by what was hardest.

---

## 1. DO THIS FIRST, or your build fails at the linker

```sh
pixi install          # pixi.toml gained `max`
pixi run build-engine # the dylib is now older than the Rust that links it
```

`native/libglyph_engine.dylib` is untracked, so every tree has its own and every
tree hits this independently. The Rust side now calls `glyph_engine_fp_probe`,
which a dylib built before 2026-09-02 does not export. Without the rebuild you
get:

```
error: linking with `cc` failed
  Undefined symbols for architecture arm64
```

`native/build.rs` now catches that before the linker does and prints the rebuild
command instead, so you should see an instruction rather than that dump. If you
see the dump, your `build.rs` is behind.

---

## 2. Things that MOVED

| was | is |
|---|---|
| `engine-local/` (with an `engine ->` symlink) | **`engine/`**, a real directory, no symlink |
| `tools/gen-real-trie.mjs` | **`tools/gen_real_trie.py`** |
| — | `tools/gen_schema.py`, `tools/vendor-manifest.py`, `schema/glyph-identity.json` |

The symlink was load-bearing — `bench.mojo` and `split_bench.mojo` open
`"engine/bench/bench.bin"` at runtime, and ~40 suite headers say `-I engine`.
Renaming made all of those correct as written. Dated `out/STAGE_*_REPORT.md`
files keep the old name on purpose; they are records, not live docs.

**No `node` is needed to build any engine input any more.** `export-atlas.mjs`
and `decode-slug-core.mjs` are still JS but read `tools/vendor/ref/` instead of
an absolute path into `viz-web/glyph3d-js`, which is what they used to do.

---

## 3. New pixi tasks (`[tasks]` used to be empty)

```
pixi run suites        # engine/check.sh — 15 conformance suites, CPU + GPU
pixi run suites-gpu    # the 5 GPU suites alone
pixi run check-gen     # generators reproduce their committed outputs
pixi run build-engine  # the dylib, with --fp-mode contract=off
pixi run check-all     # the full gate suite
```

---

## 4. `check-all.sh` is EIGHT gates now (was six), and takes longer

Two new gates run FIRST, before build/clippy/test — inputs before consumers:

- **gate 1** — three generators plus the atlas exporter each rebuild their
  committed output and require **byte-identity**: `engine-trie.bin` (132140 B),
  `engine/glyph_schema.mojo` (5100 B), the 16-file vendor manifest, and the four
  `assets/atlas/*.bin`.
- **gate 2** — `engine/check.sh`: **15 Mojo conformance suites** (10 CPU + 5 GPU
  on Metal) plus a compile pass over all six benches.

Budget a few extra minutes. It is more coverage per run, not more work per gate.

### Four ways you can now fail a gate that did not exist before

1. **Hand-editing `engine/glyph_schema.mojo`.** It is GENERATED from
   `schema/glyph-identity.json` by `tools/gen_schema.py`, which also runs the
   kind/carrier validation (carrier declarations, cross-tier kind agreement, the
   wire pin, "a declaration may not outlive its justification"). Edit the schema
   and regenerate; the gate diffs the result.
2. **Editing anything under `tools/vendor/ref/`.** Those 16 files are vendored
   byte-verbatim from the web repo and hash-gated by `tools/vendor/SHA256SUMS`.
   An edit there is an undeclared fork and fails the build. `PROVENANCE.md`
   records where each came from and at which upstream commit.
3. **A non-finite layout param.** `ItemParams::validate` now refuses NaN and
   ±Inf at the FFI seam, naming the item index. `0.0` is explicitly LEGAL — a
   degenerate pitch is a choice, not an omission, so do not "fix" it to a
   default. NaN is the `.pipe.bin` encoding for *unset*, and every gate here
   compares by BITS where two NaNs are bit-equal, so a NaN layout previously
   passed everything silently.
4. **A dylib built without `--fp-mode contract=off`.** `Engine::new()` probes
   the linked dylib and panics if it fused a multiply-add. Nothing in this tree
   could detect that before. Always build via `pixi run build-engine`.

---

## 5. Faster — your perf numbers will move

The CPU parallel driver moved off `std.runtime._asyncrt.TaskGroup` (a PRIVATE
Mojo module with no stability guarantee and no deprecation path) onto the public
`max.algorithm.parallelize`. Best-of-3 on the pinned nightly:

| | before | after | |
|---|---:|---:|---|
| pipeline | 10.327 ms | **9.921 ms** | 1.041x |
| pipeline elided | 9.618 ms | **9.240 ms** | 1.041x |
| scan | 12.250 ms | **11.253 ms** | 1.089x |
| bake | 36.175 ms | 36.244 ms | unchanged |

Every checksum is bit-identical. `bench/lane_write_bench.mojo` and
`bench/split_bench.mojo` were deliberately LEFT on `TaskGroup` — their absolute
numbers are quoted in the README ledger and changing their driver would make
those historical entries incomparable.

---

## 6. What did NOT change

**All four render baselines are byte-equal.** `demo`, `text`, `repo-zoom` and
`repo-wide` are pixel-identical after the parallel-driver swap, the atlas moving
to a Python generator, and the merge with stage-K. If a render differs for you,
that is real signal — please say so rather than re-baselining.

---

## 7. Landmines found that are NOT fixed — worth knowing before you test

- **`real-kernels.pipe.bin` does not paginate.** Its generator declares
  `page: {rows, gapX}` where the oracle reads `pageRows`/`pageGapX`; `|| 0`
  swallowed both, and `pagesWide` is spelled correctly so it LOOKS configured.
  Its comment claims it covers "wrapped AND paged at once". It does not, and
  tracing every other fixture, **wrap+page co-occurrence is covered by zero
  fixtures.** The typo lives in the web repo and stays there by decision; this
  tree closes the hole when the corpus is regenerated.
- **Depth is zero at the caller.** `repo.rs:220-221` hardcodes
  `depth_per_band: 0.0` / `depth_per_col: 0.0`, and `z_step` falls through
  `..Default::default()`. The engine supports depth and `paged-rows.pipe.bin`
  exercises it; the runtime asks for none of it. So z-offsetting has never run
  at runtime, and turning it on will exercise a wrap+page+depth combination no
  fixture covers.
- **`conformance_matrix.mojo` has zero anti-vacuity guards** across its 48
  property combinations — nothing asserts the combination under test actually
  engaged. Same shape as the `real-kernels` defect. This is the next thing I am
  working on, so leave it to me rather than duplicating.
- **`std.gpu` -> `max.gpu` is BLOCKED, do not migrate it.** Measured on our
  pinned nightly: `from max.gpu import global_idx` fails; `std.gpu` still works.
  The privatization landed in a LATER nightly than our pin. `pixi.lock` is
  tracked so a fresh clone is safe; a `pixi update` past it would fail LOUDLY at
  parse. Details and the exit condition in `engine/TOOLCHAIN.md`.

---

## 8. Where to read more

- `engine/TOOLCHAIN.md` — the channel split (this tree is on Mojo 1.1 nightly
  via pixi; the web repo is on 1.0.0 stable via pip) and all three toolchain
  breakages measured against *this* channel rather than forecast.
- `tools/vendor/PROVENANCE.md` — the 16 vendored files, upstream paths, commit
  and hashes; and how to check local vs upstream drift, which are deliberately
  different questions.
