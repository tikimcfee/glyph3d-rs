# doc-review — the docs, checked against the tree

Scope: `BUILD-BRIEF.md`, `native/AGENTS.md`, `AGENTS.md`, `engine/BACKEND-PLAN.md`,
`engine/README.md`, `engine/delta/*.md`, `README.md`.
Tree: `/Users/lugo/localdev/viz-native/glyph3d-native`, branch `bounds`, HEAD `e9bdfb6`,
2026-09-04. **Nothing outside this file was modified.**

Method: run rather than read wherever a command existed. Every count was counted with a
command, every symbol grepped, every line-number citation opened, every "X does not do Y"
attacked with grep. Commands are quoted inline so each finding can be re-run.

Commands executed for this review (the load-bearing ones):

```
cargo test --release                     # native/ — 84 + 1 = 85 green
./engine/check.sh cpu                    # 11 CPU suites + 2 instruments, all green
bash tools/check-fixture-parity.sh       # gate 9, current counts
cargo metadata --no-deps                 # workspace_root
cargo fmt --check | grep -c '^Diff in'   # 234
ls engine/fixtures/*.pipe.bin | wc -l    # 17
ls engine/fixtures/*.bake.bin | wc -l    # 8
grep -c 'depends-on' pixi.toml           # 0
grep -n '@export' engine/ffi.mojo        # 8 exports, no bounds accessor
nm -gU native/libglyph_engine.dylib      # the same 8, at the binary
python3 tools/vendor-manifest.py --check # "20 vendored files match"
grep -c '^step ' tools/check-all.sh      # 12
```

---

## Summary

**~90 discrete claims checked across the seven top-level documents (the six
`engine/delta/` reports are counted separately, in §6). 34 do not hold as written.**

That ratio reads worse than it is. The failures concentrate:

* **~20 are staleness** — three numbers (fixture count, suite count, gate count) wrong in
  eighteen places between them, plus five dangling file paths. Nearly all of it dates
  from four commits landed on 2026-09-04.
* **5 are load-bearing reasons that do not hold** (§1) — the class the brief names. Four
  of the five are in `engine/README.md`; one is in `native/AGENTS.md`, and it is the same
  wrong reason `native/src/layout.rs` was corrected for in `b6827fb`.
* **The rest are cross-references that resolve to the wrong thing** — most notably eight
  "Stage N of `engine/BACKEND-PLAN.md`" pointers into a numbering that document no longer
  has (§3.4).
* **7 are in `BUILD-BRIEF.md`** (§4). Every one of its six `[measured]` markings holds
  when re-measured; the failures are all in *unmarked* claims.

In `engine/delta/` (§6), a further **~510 claims** were checked across all six reports,
**~77 fail** — but ~50 of those are pure `file:line` drift with the substance confirmed by
grep or by running the binary, and every recomputable number came back exact
(`data-path.md`'s memory arithmetic; `atlas-emoji.md`'s and `review.md`'s atlas censuses,
both reproduced by reparsing the `.bin` files from `FORMAT.md`; `layout-compute.md`'s
eight-FFI-symbol claim checked against source, header AND `nm` on the built dylib;
`wrap-mode.md`'s entire visual-check table reproduced cell-for-cell by running the binary
in both modes). **The analysis in those reports is sound; their pointers are not.**

Three things in there are not drift and matter on their own: the two JS oracles have
**forked** and one report positively denies it (§6.7a); `wrap-mode.md`'s twenty-two
mutation results rest on a harness that is **not in the tree** (§6.7); and the
cross-review's corrections **landed in the plan and the source but in none of the reports
it reviewed**, so the refuted claims are still standing where a reader meets them first
(§6.8).

Within that, the cross-review `review.md` audits two plan
documents by line number, and **neither is auditable any more** — `PLAN-DRAFT.md` was
deleted, and `BACKEND-PLAN.md` was rewritten, both in the very commit that landed
`review.md`. Four of its eleven plan citations now resolve to unrelated text rather than
failing, which is the worse outcome. §6.4 is the origin story of §1.1: the cross-review
found that defect, and the correction reached `layout.rs` and `BACKEND-PLAN.md` but not
`native/AGENTS.md`.

The three a reader would be most misled by are §1.1, §1.2 and §1.3.

---

## 1. The worst, ranked

1.1–1.3 are the three a reader would be most misled by. 1.4 and 1.5 are the same
class — a load-bearing sentence that forecloses looking — at smaller stakes.

### 1.1 `native/AGENTS.md` still carries the exact claim `layout.rs` retracted

`native/AGENTS.md:113-116` (the layout-seam section):

> A gate that needs the 32 B wire records asks `VerifyLayout`, a SEPARATE trait.
> **Holding a `LayoutGlyphs` makes the 36 B-per-source-byte readback unreachable**
> rather than merely discouraged.

`native/src/layout.rs:58-66` says the opposite, in as many words:

> `LayoutGlyphs` cannot produce records at all; a gate that needs them asks
> [`VerifyLayout`]… **IT IS NOT THE SAME AS DELETING THE READBACK, and this header
> used to claim it was**: `MojoLayout::run` calls `engine.records()` unconditionally in
> both strategies… 3.10 GB still crosses the FFI on every load. `VerifyLayout` gates
> the API, not the copy.

`engine/BACKEND-PLAN.md:53` (work item 3) agrees with the source, not with AGENTS.md.

So the source header was fixed and **the house-rules file was not**. This is the second
occurrence of the *same* wrong reason in the same repo — and agents are told to read
`native/AGENTS.md` FIRST (`BUILD-BRIEF.md:120`), which means the retracted version is the
one a fresh agent reads first. It is load-bearing in the defect sense described in the
brief: "unreachable" forecloses the investigation that item 3 of the plan of record
exists to open.

Verify:
```
sed -n '55,70p' native/src/layout.rs
grep -n 'unreachable' native/AGENTS.md
```

### 1.2 `engine/README.md`'s "read this first, next agent" section describes a different repo

`engine/README.md:509-553` is headed **"Setup from a fresh clone (read this first, next
agent)"**, and the schema line at :24 belongs with it. Seven statements are false
*of this tree*:

| line | claim | reality |
|---|---|---|
| 24 | "`bun tools/gen-schema.mjs` generates `engine/glyph_schema.mojo` and the JS twin" | **the file does not exist.** `ls tools/*.mjs` → `decode-slug-core.mjs`, `export-atlas.mjs` only. The generator is `python3 tools/gen_schema.py`, which is what `pixi.toml`, `check-all.sh` gate 1, root `AGENTS.md`'s fences table and `README.md`'s tools table all name. An agent told to regenerate the schema and following this line runs a command that does not exist. |
| 511 | "the repo is already a **Bun workspace**" | no `package.json`, no `bun.lock*` at any level. `ls package.json bun.lock bun.lockb` → all missing |
| 515, 551 | "`pip install modular` — provides `mojo` on PATH" | the toolchain is **pixi** (`pixi.toml`, `pixi install`, `pixi run mojo`); `engine/check.sh:29` hardcodes `MOJO=(pixi run mojo)`. Root `AGENTS.md:38` says pixi. Direct contradiction. |
| 516 | "expect: Mojo 1.0.0 or later" | `pixi.toml` pins `mojo >=1.1.0.dev2026083005,<2` |
| 518 | "ALL FIFTEEN suites" | 16 (see 2.1) |
| 540, 625 | "the repo's `.mcp.json` registers Modular's docs MCP server" | `ls .mcp.json` → no such file |
| 543, 630 | "`.claude/skills/` carries `mojo-syntax`, `closure_migration`, `mojo-gpu-fundamentals`, **which load automatically from the clone**"; "see MODULAR-SKILLS-LICENSE" | `.claude/` contains exactly one entry: `worktrees/`. No `skills/`, no `MODULAR-SKILLS-LICENSE`. |

Also dangling in the same file: `tools/scan-layout.test.mjs` (:487),
`examples/word-wall/data/…` (:219), `docs/plans/glyph3d-native-engine.md` (:3),
`vram-memory-architecture.md` (:297), `engine/delta/phantom-row.md` (:209) — none exist
in this tree. (`phantom-row.md` exists only under `.claude/worktrees/phantom-row/`, which
`AGENTS.md:20` tells readers to ignore; `docs/plans/…` and `vram-…` are web-repo paths.)

Verify:
```
ls package.json bun.lock bun.lockb .mcp.json; ls -a .claude
find . -name 'scan-layout.test.mjs' -o -name 'phantom-row.md' -not -path './.git/*'
```

### 1.3 `engine/README.md:206-209` says three suites are RED. They are green.

> Since the frozen fixture corpus was generated by the JS oracle under the old rule,
> **the corpus specifies the phantom** and `conformance`, `conformance_scan` and
> `conformance_bake` are red until it is regenerated.

The corpus *was* regenerated — commit `c9667ec` "rows: a newline stops claiming a row of
its own". Run:

```
./engine/check.sh cpu
→ conformance            conformance: all cases bit-exact
→ conformance_scan       scan conformance: all cases within the tiered contract
→ conformance_bake       bake conformance: all cases bit-exact
→ all 11 CPU suites + 2 instruments green
```

The rest of that paragraph is accurate — the fix landed exactly as described:
`native/src/fold.rs:337-348` is `if wrap <= 0 || length <= 0 { 1 } else { (length - 1) /
wrap + 1 }`, i.e. `ceil(length/wrap)` floored at one, and `fold.rs:972`'s table pins the
exact-multiple case `(4, 4, 1)` with the comment "EXACT MULTIPLE — the defect's whole
domain". Only the last sentence is stale, and it is the one that changes behaviour.

A doc that tells an agent three named suites are currently failing is worse than a stale
number: it invites the agent to "fix" a green suite, or to discount a real red as the
known one.

Related, same file: `engine/README.md:640` "**Not ported yet** — The GPU backend itself:
lift glyph_scan's loops onto device threads (needs GPU hardware…)" contradicts
`engine/README.md:172-232` in the same document, which describes five GPU suites doing
exactly that on Metal, with a benchmark table.

### 1.4 `engine/README.md:144` declares the debt ledger empty. It is not.

> the stale-declaration rule makes an exemption that outlives its deviation a build
> failure, so keeping it was not an option. **KNOWN_DEVIATIONS is empty: the system has
> no declared debts for the first time in its existence.**

`schema/glyph-identity.json:453-459` carries a live one:

```
grep -c '"misplaced"' schema/glyph-identity.json   → 1
sed -n '453,459p' schema/glyph-identity.json
   "name": "TOTAL_ROWS", "kind": "count",
   "misplaced": "A count on the host's f64 bounds array. … settle by splitting the
                 host bounds container by carrier…"
```

`tools/gen_schema.py:102-115,186-190` treats `misplaced` as *the* declaration mechanism —
an exact kind on a float carrier without one is a build failure. So the schema declares
exactly one outstanding debt, and gate 1 is green *because* it is declared. (The literal
token `KNOWN_DEVIATIONS` appears nowhere in the tree except this sentence: `grep -rn
KNOWN_DEVIATIONS .` → one hit, `engine/README.md:144`. So the claim is also
unfalsifiable as written — there is no such list to inspect.)

This is the shape the brief warns about: a triumphal, closing-the-book sentence
("for the first time in its existence") standing where a reader would otherwise go and
look. The surrounding paragraph about GLYPH_ID is accurate and checks out; it is the
generalisation at the end that overreaches.

### 1.5 `engine/README.md:84,95-99` describes the record's pre-settlement shape

Same file, thirty lines above the passage celebrating the GLYPH_ID settlement:

> `record   32 B per RENDERED GLYPH                 (measures 6 + counts 2)`
> …emitting a record is a concatenation of **three** contiguous runs (posMeasures'
> prefix, **staticMeasures' prefix**, posCounts whole)

The generated schema — the artifact the paragraph says is the source of truth — says
otherwise:

```
grep -n 'RECORD_.*_STRIDE' engine/glyph_schema.mojo
  53: comptime RECORD_MEASURE_STRIDE = 5
  54: comptime RECORD_COUNT_STRIDE = 3
sed -n '49,52p' engine/glyph_schema.mojo
  # A record is FOUR runs — posMeasures[0..3), staticMeasures WHOLE,
  # staticIdentities whole, posCounts whole
```

5 measures + 3 counts (`X Y Z ADVANCE HEIGHT | GLYPH_ID ROW COL`, which is exactly what
root `README.md` and `native/src/layout.rs` both state), in four runs, with
`staticMeasures` taken whole. "measures 6 + counts 2" and "three runs / staticMeasures'
prefix" are the arrangement from **before** GLYPH_ID left the f32 array — the very change
the same document describes as settled. The adjacent `slots 40 B (sm 8 + gi 4 + fl 4 +
lm 16 + lc 8)` line *was* updated for it. Half the paragraph moved and half did not, and
both halves read as current.

---

## 2. Staleness (counts and inventories)

Three numbers drifted and are wrong in several places each. Ground truth, measured today:

* **Fixtures: 25** — `ls engine/fixtures/*.pipe.bin | wc -l` → **17**;
  `*.bake.bin` → **8**. (Was 14+8=22 until `343c039` added the three `wrapback-*`.)
* **Mojo suites: 16** — `engine/check.sh` `CPU=(7 named)` plus `conformance_gaps`,
  `conformance_matrix`, `conformance_real`, `conformance_bake` = 11 CPU, `GPU=(5)`.
  The script's own closing line prints `all 16 suites green`.
* **check-all gates: 12** — `grep -c '^step ' tools/check-all.sh` → 12
  (0, 1, 1b, 2, 3, 4, 5, 6, 7, 8, 8b, 9).

### 2.1 Suite count — "fifteen" in four places

| file:line | says | actual |
|---|---|---|
| `AGENTS.md:11` | "15 conformance suites" | 16 |
| `AGENTS.md:49` | "15 Mojo suites (CPU+GPU)" | 16 |
| `README.md:32` | "15 conformance suites" (the ASCII diagram) | 16 |
| `README.md:109` | "all fifteen Mojo conformance suites" | 16 |
| `README.md:128` | "15 conformance suites" (repo map) | 16 |
| `engine/README.md:518` | "ALL FIFTEEN suites" | 16 |
| `engine/check.sh:66` | "the default is ALL FIFTEEN" (code comment) | 16 — **and the same script prints "all 16 suites green" 40 lines later** |

`native/AGENTS.md`'s "16 Mojo conformance suites (11 CPU + 5 GPU)" is **correct**.

### 2.2 Gate count — "Eight steps" in three places

| file:line | says | actual |
|---|---|---|
| `AGENTS.md:48` | "Eight steps: …" then lists 8 | 12 |
| `AGENTS.md:52-53` | "(`native/AGENTS.md` describes the original six cargo-side gates…)" | `native/AGENTS.md` now describes twelve; the cross-reference is stale |
| `README.md:107` | "Eight steps: (1)…(8)" | 12 |
| `README.md:142` | "The umbrella gate (8 steps, above)" | 12 |

Both files do not merely miscount: they **omit gates 0, 1b, 8b and 9 by name**. A reader
of `README.md` or root `AGENTS.md` comes away believing the reference port (gate 9 — five
stages, six halves, the largest single gate in the suite) has no gate at all, and that
nothing in `check-all` builds the dylib.

`native/AGENTS.md`'s "TWELVE gates as of 2026-09-04 (was nine, was six)" and
`BUILD-BRIEF.md:72`'s "twelve gates, though the headings still say N/9" are **correct**.
(Confirmed the headings: every `step` line reads `N/9`.)

### 2.3 Fixture count — "22" in the plan of record, "14+8" in a gate comment

* `engine/BACKEND-PLAN.md:24`: "**22 fixtures** rebuild byte-identical from vendored,
  per-file-pinned inputs with no web repo present (gate `1b`)." → **25**. The gate itself
  computes `F_COUNT` dynamically, so the gate is right and the doc is behind it.
* `tools/check-all.sh` (comment above gate 1b): "the **14** `.pipe.bin` + 8 `.bake.bin`
  the whole port is gated against" → 17 + 8.
* `BUILD-BRIEF.md:12,32`: "**25** binary fixtures" / "(25)" — **correct**.

### 2.4 `native/AGENTS.md`'s gate-9 numbers are all one commit behind

Ran `bash tools/check-fixture-parity.sh`. Every figure in the gate-9 paragraph moved when
the three `wrapback-*` fixtures landed:

| `native/AGENTS.md` says | gate prints today |
|---|---|
| "across all **14** fixtures" | 17 |
| "**14** fixtures, **11520** entries" (trie) | 17 fixtures, **13568** entries |
| "**14** fixtures, **149,767** leaders, **1,807,512** lanes" (fold) | 17, **155136** leaders, **1872012** lanes |
| "8 tunings — **112** cases, **1,144,944** leader lanes" (scan) | **136** cases, **1187896** leader-lanes |
| "**265** queries bit-exact" (bake) | **530** |
| "4 today, 5332 records" (text.rs domain) | 4, 5332 — **correct** |

### 2.5 Smaller drifts

* `native/AGENTS.md:153` (style section): "the tree is NOT fmt-clean (**≈77 hunks** across all
  src files)". `cd native && cargo fmt --check | grep -c '^Diff in'` → **234**. The
  instruction ("do not mass-reformat") is still right; the number is 3× off.
* `tools/check-all.sh` header, gate 5: "all tests green (**19** at Stage J…)". `cargo test
  --release` → **85** (84 lib + 1 `tests/wgsl.rs`). Scoped to "at Stage J", so historical
  rather than wrong, but it is the first number a reader of the script sees.
  `native/AGENTS.md`'s "**85 tests**" is **correct** — verified exactly.
* `native/AGENTS.md:17,24` worktree recipe: `cp ../../../engine/bench/bench.bin engine/bench/`.
  `ls engine/bench/bench.bin` → **no such file**. Marked "# optional; benches only", and
  `engine/check.sh` only *compiles* benches, so no gate needs it — but the command as
  written cannot succeed from this tree.
* `native/AGENTS.md:40`: gate 1 checks "the **16-file** vendor manifest".
  `python3 tools/vendor-manifest.py --check` prints **"20 vendored files match
  tools/vendor/SHA256SUMS"**; `grep -cE '^[0-9a-f]{64}' tools/vendor/SHA256SUMS` → 20.
  It grew when the six `engine/fixtures/inputs/*.js` and `schema/glyph-identity.json`
  were added to it (`f68b70f`) — the same commit the other stale counts date from.
* `engine/BACKEND-PLAN.md:6`: "Evidence … is in `engine/delta/` (**four** subsystem
  reports plus a cross-review)". `ls engine/delta/*.md` → five subsystem reports;
  `wrap-mode.md` landed later, in `cc814b3`. The plan of record undercounts its own
  evidence directory, and `wrap-mode.md` is the report covering the feature the
  in-flight `bounds` work depends on.
* `README.md:172` and `AGENTS.md:105` both point a newcomer at
  `out/ENGINE_TOOLCHAIN_REPORT.md` as "**what's true of the toolchain right now**". That
  report is titled "the JS dependency is gone" and states at :57 "**No `node` is needed
  to build any engine input any more**" — while `check-all` gate 1 runs `node
  tools/export-atlas.mjs` to build four committed engine inputs, and gate 1b runs `node
  gen.mjs`/`gen-bake.mjs` to rebuild the corpus. Its §3 also lists "15 conformance
  suites" and §4 is headed "check-all.sh is EIGHT gates now". `out/` reports are records
  by convention (the report says so itself at :55, "records, not live docs") — the defect
  is the two live docs promoting one to current truth.
* `pixi.toml:10` comment: "`all` also tries the five GPU suites, which need `max` in this
  env (**it is not there yet**)". `max` has been a dependency since `d0a947b`
  (`pixi.toml:24`), and `engine/check.sh:66` documents that fix. Stale comment.

---

## 3. Contradictions between documents

### 3.1 `AGENTS.md`'s "Gotchas" says the fixture generator cannot run here. It is a gate.

`AGENTS.md:94-95`:

> `engine/fixtures/gen.mjs` (fixture regeneration) and `engine/bench/gen-bench.mjs`
> import the JS oracle **from the web repo — they do not run in this tree**; fixtures are
> committed.

Half of that is now false, and it is the load-bearing half:

```
grep -n '^import' engine/fixtures/gen.mjs
  56: … from './inputs/glyphPipelineReference.js'
  60: … from '../glyph_schema.mjs'
  61: … from './inputs/GlyphTrie.js'
grep -n '^import' engine/bench/gen-bench.mjs
  14: … from '../../packages/glyph3d-core/src/compute/GlyphTrie.js'   ← still web-repo
```

`gen.mjs` and `gen-bake.mjs` read vendored inputs under `engine/fixtures/inputs/` (commit
`f68b70f`, "corpus: the fixtures could not be rebuilt where they live") and are executed
by **gate 1b on every `check-all`**. Only `gen-bench.mjs` still reaches into the web repo.
A reader of `AGENTS.md` is told the corpus is unrebuildable here; `native/AGENTS.md`,
`BUILD-BRIEF.md` and `check-all.sh` all say the opposite, correctly.

### 3.2 `BACKEND-PLAN.md`'s work item 1 is already done

`engine/BACKEND-PLAN.md:40` "**1. The phantom row.** … Three
fixtures encode it. Remove it, oracle first." That landed in `c9667ec` (oracle first,
corpus regenerated, `conformance`/`conformance_scan`/`conformance_bake` green — verified
by running them), and `343c039`/`cc814b3` built on top of it. The plan of record still
presents it as the next thing to do; the "work in flight" section below it is branched off
`cc814b3`, i.e. *after* the fix. A fresh agent handed "the plan of record" would start by
redoing item 1.

### 3.3 `README.md` Node requirement understates where node is used

`README.md:62`: "Node ≥ 18 (**the atlas gate** in `check-all` uses it)". Node is also
what gate 1b runs (`node gen.mjs`, `node gen-bake.mjs`). Minor, but the sentence reads as
an exhaustive account of the node dependency and is not one. Related, and correct:
`native/AGENTS.md`'s "No JS runs in gate 9" — verified, `grep -n 'node\|\.mjs'
tools/check-fixture-parity.sh` → no matches.

### 3.4 "Stage 0 of `engine/BACKEND-PLAN.md`" points into a numbering that was deleted

`native/AGENTS.md:105` — "Everything that lays glyphs out goes through
`native/src/layout.rs` (**Stage 0 of `engine/BACKEND-PLAN.md`**)" — and `:132`, "stage 1
points the same call at the Rust backend". `native/src/layout.rs` says the same at `:3`,
`:40`, `:47`, `:48`, `:395`, `:521` (stages 0, 1, 3).

```
grep -in 'stage' engine/BACKEND-PLAN.md
  47: …as a post-fold stage; bounds, cull, caret…
  66: …read in the vertex stage and never becomes a varying…
```

**`BACKEND-PLAN.md` has no stages.** They were removed on purpose — `b6827fb`'s message:
"BACKEND-PLAN.md rewritten as the plan rather than as a record of how it changed. Stages,
seams and A/B framings are gone."

The hazard is not the dead pointer, it is that the destination has a *different* numbered
list 1–6 ("The work, in order") which a reader will land on and match up: layout.rs's
"stage 3 replaces its interior with a device buffer" reads as BACKEND-PLAN item 3, "The
readback" — adjacent, and not the same commitment. Item 1 is the phantom row, not the
Rust backend. Eight cross-references, all resolvable to the wrong thing rather than to
nothing.

---

## 4. `BUILD-BRIEF.md` — the [measured] audit

Six claims carry **[measured]**. I re-ran all six. **All six hold.**

| line | claim | how I re-measured | verdict |
|---|---|---|---|
| 17 | "`native/Cargo.toml` — a leaf, there is no root workspace" | `cargo metadata --no-deps` → `workspace_root: …/native`, one member; `ls Cargo.toml` and `ls ../Cargo.toml` both missing | **holds** |
| 50 | "no `depends-on` anywhere" | `grep -c 'depends-on' pixi.toml` → 0 | **holds** |
| 62 | the `glyph_engine_load_item` twenty-positional-parameter story | `git show cc814b3^:engine/ffi.mojo` → 20 params after the handle; `git show 343c039 -- engine/ffi.mojo` → `+ wrap_mode: c_int` inserted **immediately before `has_page`**, exactly as the brief describes; the fix (`glyph_engine_load_item_desc`, a descriptor, renamed so a stale dylib fails to link) is at `engine/ffi.mojo:202` | **holds, precisely** |
| 73 | "twelve gates, though the headings still say N/9" | 12 `step` calls, all headings `N/9` | **holds** |
| 81 | "`engine/ffi_selftest.mojo` is neither run nor compiled by `engine/check.sh` — 0 occurrences" | `grep -rn ffi_selftest tools engine/check.sh pixi.toml` → hits only in `README-FFI.md`, the file itself, `README.md`, `.gitignore`, `delta/wrap-mode.md` | **holds** |
| 106 | the four verification traps, "[all measured]" | three of four are independently corroborated in-tree: the sha256-that-matched-the-wrong-file at `tools/vendor-manifest.py:71`; the `git checkout -- <dir>` that ate a live edit, in `check-all.sh`'s gate-1b comment; the mutation-target-never-run-clean, in `check-all.sh`'s gate-8b comment. The fourth (restore-without-rebuild) I could not corroborate from the tree, but nothing contradicts it | **holds** |

### 4.1 The convention is announced but not applied

`BUILD-BRIEF.md:4-5` says "Claims are marked **[measured]** … or **[inferred]** … — do not
promote the second kind." In practice there are **six [measured] markings and zero
[inferred] ones**, and the great majority of the file's claims carry no marking at all.
A reader following the stated convention will read an unmarked claim as at least as
settled as a marked one — and the two claims below, both unmarked, are the ones that fail.
That is the failure mode the convention exists to prevent, arriving through the gap in it
rather than through a mis-marking.

### 4.2 "Every generator already has a `--check` / `--verify-only` mode" — false for half of them

`BUILD-BRIEF.md:41-43` (unmarked):

> **COMMITTED artifacts** are verified by rebuilding to a scratch location and
> byte-comparing. **Every generator already has a `--check` / `--verify-only` mode**, so
> this half exists — what is missing is the graph.

Measured:

```
tools/gen_real_trie.py    --verify-only   ✓ (sys.argv, line 104)
tools/gen_schema.py       --check         ✓ (sys.argv, line 386)
tools/vendor-manifest.py  --check         ✓ (sys.argv, line 212)
tools/export-atlas.mjs    --out <dir>     ✗ — no check mode; gate 1 redirects and cmps
engine/fixtures/gen.mjs   (no argv at all) ✗ — gate 1b deletes and rebuilds
engine/fixtures/gen-bake.mjs (no argv)     ✗ — same
```

Three of six generators have no `--check`. This matters for the task the brief sets: an
agent designing "one declarative manifest with a verify mode" will plan around a uniform
`--check` contract and discover halfway that half the generators need the
scratch-dir-and-`cmp` shape instead. The brief's *first* sentence there already describes
that shape correctly, so the two sentences disagree with each other.

### 4.3 "Three languages" omits Python, which the brief's own table depends on

`BUILD-BRIEF.md:9-11`: "**Three languages**: Mojo…, Rust…, and JS (vendored, used only to
generate a conformance corpus)."

`ls tools/*.py` → **seven Python tools**, three of them in the artifact table on the very
next page (`gen_schema.py`, `gen_real_trie.py`, `vendor-manifest.py`) and one of them a
gate oracle (`g_pick_oracle.py`, gate 7). Python is on the critical path of the build
system the agent is being asked to build. Also: JS is not "used only to generate a
conformance corpus" — `tools/export-atlas.mjs` generates the four committed atlas bins and
is gate 1.

### 4.4 The dependency chain omits an edge the brief's own table implies

`BUILD-BRIEF.md:53-59` draws:

```
schema/glyph-identity.json -> gen_schema.py -> engine/glyph_schema.mojo
  -> pixi run build-engine -> cargo build
```

`tools/gen_schema.py:34-35` emits **two** files: `engine/glyph_schema.mojo` *and*
`engine/glyph_schema.mjs`; and `engine/fixtures/gen.mjs:60` imports
`../glyph_schema.mjs`. So there is a second edge out of the schema —
`glyph_schema.mjs → gen.mjs → the 25 fixtures → every conformance gate` — which the
brief's artifact table knows about (it lists "`.mojo` + `.mjs`") but the chain diagram
does not. For a document whose thesis is "nothing declares a dependency", the missing
edge is the more interesting half: editing the schema staleness-invalidates the *corpus*
as well as the dylib.

### 4.5 Smaller

* `BUILD-BRIEF.md:73-77`: "Recently added, each for a check that existed and was never
  consulted: gate 0 … **gate 1b** … gate 8b." Gate 1b is not that shape. Before `f68b70f`
  the fixtures **could not be regenerated in this tree at all** (`gen.mjs` imported from
  `../../packages/`); vendoring the inputs was the work, and the gate came with it. Gate 0
  and gate 8b do fit the description exactly. A small overclaim in a sentence whose
  pattern-naming is the point.
* `BUILD-BRIEF.md` ("What NOT to do"): "those four byte-equal screenshots are **the
  only gate that catches an unintended renderer change**." Defensible for pixel output —
  gate 8 is the only pixel gate — but `native/AGENTS.md`'s own `empty.rs` paragraph
  records a renderer-affecting defect (the page extent's origin seed) that **all four
  screenshots stayed byte-equal through**, caught instead by a unit test, and which gate 8
  can see today only because a zero-byte fixture was added for it. The instruction that
  follows ("a build must never re-baseline them") is right either way; the superlative is
  the part that would stop someone from asking what else gate 8 cannot see.
* `BUILD-BRIEF.md:21`: "`pixi run build-engine` compiles `engine/*.mojo`". It compiles
  `engine/ffi.mojo` with `-I engine` (`pixi.toml:19`). The substance — the schema is an
  input, the dylib depends on the whole directory — is right; the glob is loose.
* `BUILD-BRIEF.md:24-25`: "the dylib is written *into* `native/` so build.rs finds it by
  **relative path**". `native/build.rs` resolves it from `CARGO_MANIFEST_DIR`, i.e. an
  absolute path derived from the crate root. The placement rationale stands; "relative"
  does not.

---

## 5. What holds (checked, not assumed)

Grouped, so the ratio is visible. Each was verified by running the quoted command.

**`BUILD-BRIEF.md`** — all six `[measured]` claims (§4). `native/build.rs` "only links,
and says so in its header" ✓ (`native/build.rs:3` "The engine is built OUTSIDE cargo").
`.gitignore` has `*.dylib` ✓ (`.gitignore:9`). The artifact table's counts: 4 atlas bins ✓,
25 fixtures ✓, 4 baseline PNGs, all four tracked ✓ (`git ls-files out/tooling-ab/baseline/`).
Gate 0 builds the dylib and `cargo build` does not ✓. Gate 1b deletes all 25 and asserts
byte-identity ✓ (and computes its own count, so it will not go stale). Gate 8b runs
`--repo-verify` in both wrap modes ✓ (`--wrap-mode {down,back}` both exist in `--help`).
Every gate script and CLI entry point named in "What to build" item 5 exists and parses
(`tools/check-all.sh`, `tools/check-stage-g.sh`, `engine/check.sh`, `pixi run suites`,
`--repo-verify`, `--engine-check`) ✓.

**`native/AGENTS.md`** — 85 cargo tests, exact ✓. Twelve gates, headings still `N/9` ✓.
16 suites = 11 CPU + 5 GPU ✓. "a compile pass over all six benches" — `ls
engine/bench/*.mojo | wc -l` → 6 ✓. All six debug env vars are live in `native/src`
(`GLYPH_PROFILE` 9 hits, `GLYPH_PICK_DEBUG` 2, `GLYPH_CULL_DEBUG` 5, `GLYPH_G_DUMP` 5,
`GLYPH_K4_SELFTEST` 3, `GLYPH_L3_SHADER_COMPOSITE` 2) ✓. `fixtures/g-pick-repo/empty.rs`
is 0 bytes ✓. `layout::tests::paint_is_indexed_by_record_so_blanks_consume_an_entry`
exists and passes ✓. `layout::compact_records_into` ✓, `layout::diff_backends` ✓,
`ItemParams::validate` in a provided `layout_items` ✓. "No JS runs in gate 9" ✓. The naga
test pins the shader *set* ✓ (`native/tests/wgsl.rs:38`, expected-list assertion; 4 wgsl
files). Version pins wgpu 30 / winit 0.30 / glam 0.33 / egui 0.36 ✓.

**`AGENTS.md` (root)** — the fences table is accurate: every generated path maps to the
authority named (`gen_schema.py`, `gen_real_trie.py`, `export-atlas.mjs`,
`vendor-manifest.py`) ✓. `pixi run build-engine` carries `--fp-mode contract=off` and there
is a runtime `fp_probe` that panics without it ✓ (`pixi.toml:19`, `engine/ffi.mojo:376`,
`native/build.rs:12`). "`pixi.lock` is binary" ✓ (`.gitattributes`). The three narrower
entry points (`suites`, `suites-gpu`, `check-gen`) all exist as pixi tasks ✓.

**`README.md`** — every command in the "Running the renderer" table parses
(`--demo`, `--render-file`, `--load-repo`, `--focus-file`, `--screenshot`, `--frames`,
`--zoom`, `--engine-check`) ✓, checked against `--help`. `pixi run check-all` exists and is
`./tools/check-all.sh` ✓. `default = ["egui-ui"]`, so `cargo run --release` really does
give the F1 panel ✓. The tools table is accurate for all ten entries ✓. The `out/` layout
and gitignore claims ✓.

**`engine/BACKEND-PLAN.md`** — every symbol it names exists: `cull_segments`
(`glyph_scene.rs`), `compact_records_into` (`layout.rs:534`), `diff_backends`
(`layout.rs:642`), `bounds_range` (`fold.rs:653`), `B_MIN_Z`/`B_MAX_Z`
(`conformance_invariants.mojo:38`), `RepoParams::z_wrap_spacing` (`repo.rs:196`) ✓.
"`z_wrap_spacing` has no CLI flag" ✓ — absent from `--help`. "`B_MIN_Z`/`B_MAX_Z` … with
no FFI accessor" ✓ — checked at the **binary**, not just the source:
`nm -gU native/libglyph_engine.dylib | grep glyph_engine` lists exactly eight symbols
(`new`, `free`, `load_trie_file`, `load_item_desc`, `load_items`, `slot_count`,
`copy_slots`, `fp_probe`), matching `engine/ffi.mojo`'s eight `@export`s, with nothing for
bounds. The same command independently confirms `BUILD-BRIEF.md`'s rename fix: the old
`glyph_engine_load_item` is **absent** from the built dylib, so a stale dylib really does
fail to link rather than silently mis-marshal. "122 MB/s against the oracle's 46" ✓ — matches `engine/README.md`'s M2 table
(122.2 / 46.0). "3.10 GB per load" ✓ — matches `layout.rs`'s corrected header. "0.04 s for
407k records" ✓ — matches `check-all.sh`'s gate-8b comment, same measurement.

Work item 5's "**`emojiCell` is read in the vertex stage and never becomes a varying, so
nothing can reach the fragment shader**" holds: `glyph_field.wgsl:116` loads the whole
glyphmap texel, uses only `info.z` (mode), forwards `mode` as a flat varying
(`:92`, `:175`, consumed at `:262`), and `info.w` is referenced nowhere in the file
(`grep -n 'info\.w\|emojiCell' native/src/shaders/glyph_field.wgsl` → the header comment
at `:13` only). The shader's own header states the same at `:18`.

Its three "Open, and worth deciding" items are calibrated correctly, and one deserves
recording because it is easy to dismiss on a glance: "**nothing ties `InstanceSlot`'s
layout to `GlyphInstance`'s: `encase` and `naga` can disagree while the test stays
green**". `GlyphInstance` *does* derive `encase::ShaderType`
(`glyph_scene.rs:101`), and there *is* a `cargo test` layout suite — so the claim looks
refuted. It is not. `native/tests/wgsl.rs` only parses and validates the WGSL; it never
extracts `InstanceSlot`'s layout. The encase suite
(`glyph_scene.rs:3272-3380`) compares **encase(Rust) against bytemuck(Rust)** — both ends
Rust — and its lane map is hand-transcribed from a *comment* in the shader header
(`glyph_scene.rs:3289`, "Lane map from the glyph_field.wgsl header"). Nothing reads the
WGSL `struct InstanceSlot` declaration. The claim holds exactly as stated, and it is the
same "checker and checked share no relationship" failure the house rules describe.
`GlyphInstance` is 48 B with a live `_pad: u32` (`glyph_scene.rs:102-113`), so the payload
arithmetic in that item holds too ✓.

The bounds line-number table (`layout.rs:330`, `:345`, `glyph_scene.rs:255`, `:408,427`),
labelled "measured 2026-09-04", is close but drifted: `PageExtent` is at 328 (330 is its
first field), `InkExtent` at 344, `SegCull` at 254, the frustum `pz = ±1` at **413** (not
408), `eye.z.clamp(-1.0, 1.0)` at 427 ✓. Everything is findable; only `:408` sends a
reader five lines short.

Worth saying explicitly, because it is the opposite of the defect class: `BACKEND-PLAN`'s
"whether the engine's `bounds_range` matches that seeding is **UNVERIFIED**. Compare
first, in both directions" is exactly right in form — a stated non-claim. Nothing in the
plan overreaches its evidence.

**`engine/README.md`** — the parts that are about *this* tree's code hold. The phase-array
table's strides check out against the generated schema (`SM_STRIDE 2`, `LM_STRIDE 4`,
`LC_STRIDE 2`, `GI_STRIDE 1`, `FLAGS_STRIDE 1`, `WM/WC 1`) ✓, and `slots 40 B (sm 8 + gi 4
+ fl 4 + lm 16 + lc 8)` is arithmetically right and post-settlement ✓. The `--fp-mode
contract=off` rationale matches `engine/check.sh`'s header and the `fp_probe` enforcement
✓. The GLYPH_ID settlement narrative is accurate: `schema/glyph-identity.json:184` carries
exactly the `note` the README quotes ✓.

One claim that a grep appears to refute but does not, flagged so nobody "fixes" it:
"**There are no bitcasts and no 'which lanes are floats' table**". `grep -rn bitcast
engine/*.mojo` returns **39 hits across 10 files** — but zero of them are in the phase
containers the sentence is about. In `glyph_pipeline.mojo` the word appears only in
comments (`:47`, `:66-67`, stating the same house rule). The real hits are in
`fixture_io.mojo` (decoding the frozen on-disk wire format), `ffi.mojo` (C-ABI
marshalling) and the three conformance suites (comparing f64 bounds lanes as u64 bit
patterns) — boundary and verification code, exactly where a bitcast is the correct tool.
The claim holds; it is just narrower than a tree-wide grep tests.

---

## 6. `engine/delta/*.md`

Two things about the set as a whole, checked directly:

**6.1 `review.md` cites a document that does not exist.** Its header says "Reviewed: …
plus `engine/BACKEND-PLAN.md` and **`engine/PLAN-DRAFT.md`**", and it then cites
`PLAN-DRAFT.md` **eighteen times** (`grep -c PLAN-DRAFT engine/delta/review.md`) — `:110-121`, `:126`, `:148-160`, `:172-177`,
`:314-321`, `:447-452`, two rows of the table at `review.md:305-306`, and the whole of
section (d), "Dead claims in `BACKEND-PLAN.md` and `PLAN-DRAFT.md`".

```
ls engine/PLAN-DRAFT.md          → No such file
git log --all -- engine/PLAN-DRAFT.md   → empty (it was never tracked)
git show b6827fb                 → "…so is PLAN-DRAFT.md, which argued from the
                                    demonstration harness's byte format and from a
                                    'one instanced draw' reading of the web that
                                    turned out to be multi-draw-indirect."
```

The file was deliberately deleted in the same commit that landed `review.md`. Every
`PLAN-DRAFT.md` citation in the cross-review is therefore **unfalsifiable** — a reader
cannot check any of them, and section (d)'s "7 dead claims" cannot be audited at all.
Worse, the citations *look* checkable, which is the failure mode that matters here: an
agent will read "`PLAN-DRAFT.md:126` is correct on this" as settled rather than as
unverifiable. The `BACKEND-PLAN.md` half of section (d) is checkable and should be split
from the half that is not.

**6.2 The cross-review covers four of the five subsystem reports.** `review.md:3-4` names
`data-path.md`, `layout-compute.md`, `interaction.md`, `atlas-emoji.md`. `wrap-mode.md`
landed later (`cc814b3`) and has had **no adversarial pass** — and it is the report
covering `WrapDown`/`WrapBack`, the feature the in-flight `bounds` work exists because
of (`BACKEND-PLAN.md`: "WrapBack made this urgent rather than tidy"). `BACKEND-PLAN.md:6`
compounds this by describing the directory as "four subsystem reports plus a
cross-review", so nothing in the doc set signals that one report is unreviewed.

**6.3 Every `BACKEND-PLAN.md:NNN` citation in `review.md` points into the pre-rewrite
document — and half of them still resolve, to unrelated text.**

`review.md` and today's `BACKEND-PLAN.md` landed in the *same* commit (`b6827fb`), which
rewrote the plan. The cross-review was written against the old one. Current
`BACKEND-PLAN.md` is **141 lines**:

| `review.md` cites | claimed to be | is actually, today |
|---|---|---|
| `:150-152` (D-B1) | the `VerifyLayout` "unreachable" claim | **past EOF** |
| `:262-266` (D-B2) | the `row`/`col` rename experiment | **past EOF** |
| `:229-247` (D-B3) | the measured host-path block | **past EOF** |
| `:277-280` (D-B4) | "Suggested reorder…" | **past EOF** |
| `:309-319` (still standing) | the verification habits | **past EOF** |
| `:203-220` (still standing) | the M6 ceiling | **past EOF** |
| `:88-92` (D-B5) | the "needs are METADATA" table | the `B_MIN_Z`/`B_MAX_Z` paragraph + the cull step |
| `:19-24` (still standing) | zero production callers for the Rust modules | the Mojo CPU-fold throughput + "What holds it together" |
| `:40-42` (still standing) | GPU kernels unwired | work item 1, the phantom row |
| `:44-66` (still standing) | the readback-benchmark finding | work item 2, displacement input |
| `:98-102` (still standing) | `k_spine_scan grid_dim=1` | bounds step 2, widen `PageExtent` |

The four in-range ones are the dangerous half: they resolve cleanly to text that has
nothing to do with the claim, so a reader who checks a citation gets a confident wrong
answer rather than a missing-line error. Combined with §6.1 (`PLAN-DRAFT.md` gone
entirely), **section (d) of `review.md` — its whole audit of the two plan documents — is
currently uncheckable against either document it audits**, while `BACKEND-PLAN.md:6`
presents `engine/delta/` as the plan's evidence base.

**6.4 The cross-review already found §1.1, and the fix reached two of three places.**

`review.md` D-B1 states it plainly:

> "The `Vec<GlyphRecord>` readback survives only behind `VerifyLayout` … so it is
> unreachable from the render path rather than merely discouraged." **FALSE.** …
> `MojoLayout::run` calls `self.engine.records()` **unconditionally** in both strategies
> — `native/src/layout_mojo.rs:79` (batched) and `:110` (per-item) … The line in
> `layout.rs`'s header is the same claim and is equally wrong.

`b6827fb` fixed `native/src/layout.rs` and rewrote `BACKEND-PLAN.md` around the correction.
`native/AGENTS.md:113-116` was not touched. So this is not a claim nobody had checked —
it is a claim that was checked, found false, corrected in two of the three places it
lived, and left standing in the one an agent is told to read first.

I re-verified the substance independently rather than taking the review's word:
`grep -n 'records()' native/src/layout_mojo.rs` → `:78` (batched) and `:103` (per-item);
reading `:70-113`, both calls are unconditional and both precede the
`if let Some(sink) = records_out` that `VerifyLayout` gates. **The review is right and its
line numbers drifted a few lines** (it cites `:79` and `:110`) — the same benign drift
seen throughout, not a substantive error.

**6.5 `data-path.md`, `atlas-emoji.md`, `layout-compute.md` — ~241 claims checked,
~40 fail, ~29 of those pure line-number drift.**

This pass was run as a separate verification sweep with the same standard (recompute every
number, grep every "only/never", `nm` the dylib, reparse the atlas binaries independently
from `FORMAT.md`). Headline: **the analysis in all three is substantively sound.** Every
measurement in `data-path.md` was recomputed and is exact (96,860,762 × 32 B = 3.0995 GB;
1,679,517/96,860,762 = 1.734 %; 4,294,967,292/48 = 89,478,485). Every atlas count in
`atlas-emoji.md` was reproduced by reparsing the `.bin` files (4431 slots — I confirmed that count independently at
`glyphs.bin` offset 16 with `struct.unpack_from('<8I', d, 0)` → `(…, 4431, 2048, 1229,
2320)`; flags `{0:3511, 1:897, 2:23}`, which sums to 4431 and whose 897+23 matches the 920
zero-curve slots; all 20 bitmap ranges character-for-character). All the README
throughput numbers reproduce. The single strongest exhaustive claim in the set —
`layout-compute.md`'s "`ffi.mojo` exports exactly 8, `engine.rs` declares exactly those 8,
the built dylib has exactly those 8" — survives independent execution against all three
artifacts, `nm` included.

The failures are citation hygiene against a tree that moved under them. All three landed
at `b6827fb`; `c9667ec`, `343c039` and `cc814b3` then moved `fold.rs` +450 lines,
`text.rs` +78, `engine.rs` +46, `main.rs` +44, `glyph_pipeline.mojo` +91,
`BACKEND-PLAN.md` +50. Roughly 29 `file:line` citations are now off by 1–130 lines
(`git diff --numstat b6827fb HEAD -- native/src/ engine/`). Substance intact; the
pointers are not.

The **substantive** failures, ordered by how much they would mislead:

1. **`engine/PLAN-DRAFT.md` is cited three more times, outside `review.md`** —
   `atlas-emoji.md:25-27` ("`PLAN-DRAFT.md:110-121` already records the measurement and
   had already withdrawn the conclusion it was cited for"), `atlas-emoji.md:380` (files
   cited), and `layout-compute.md:124`. `find . -name 'PLAN-DRAFT*'` → nothing;
   `git log --all -- '*PLAN-DRAFT*'` → nothing (it was untracked scratch, removed in
   `b6827fb`). The `atlas-emoji.md:25-27` instance is the worst line in the three
   documents: it tells the reader the question is *settled elsewhere*, with a page
   reference, and there is no elsewhere. Combined with §6.1, **`PLAN-DRAFT.md` is cited
   from three of the six reports and exists in none of them.**
2. **`layout-compute.md:124`: "The only hit for 'displacement' in the whole native tree is
   `engine/PLAN-DRAFT.md`."** Both halves wrong. `grep -rn Displacement
   engine/BACKEND-PLAN.md` → `:45`, "**2. Displacement input.**" — a live work item for
   exactly the feature the sentence says nothing mentions, and the prior art for that
   section's own recommendation. An exhaustive-grep claim that points the reader away from
   the answer.
3. **`layout-compute.md:44`: "only `glyphPipelineKernels.js:1331` reads `depthPerColumn`."**
   `glyphPipelineReference.js:610` reads it too (the CPU oracle). The narrower claim — no
   call site *sets* it — does hold. Two other "only/never" claims in the same document
   (`ffi.mojo` never imports `glyph_bake`; no FFI accessor for `B_MIN_Z`) survived the
   same falsification attempt, so this is a specific miss, not a systemic one.
4. **`data-path.md:173-177`: "`instance_bufs` appears at [six sites] — and nowhere else.
   There is no append, no realloc, no restage."** `grep -n instance_bufs
   native/src/*.rs` → eight sites; `:1725` (the struct-literal init) is missing from the
   enumeration, and three of the six offsets are off by one. The conclusion is right; the
   enumeration offered as proof of it is not exhaustive. This is precisely the repo's own
   rule about source-scan guards: a grep-derived claim is worth what its match set is
   worth.
5. **`data-path.md:44,399-401` frames `instancePickingId` as feeding the ID render pass.**
   `GlyphField.js:1019-1020`'s own comment says the opposite: "No shader reads this
   attribute; it is the CPU-side mirror harnesses check the pick pass against." The 4 B
   per glyph is real; the mechanism is not.
6. **`atlas-emoji.md:268-269`: "the same function has already dropped every double-advance
   glyph three lines earlier."** The drop is `text.rs:143` (inside a closure); the
   `advance: cell_w` write is `text.rs:267` — 124 lines apart, different scopes. The
   conclusion (the defect is invisible today) is right for a different reason; the
   specificity is what makes the wrong reason read as verified.
7. **`atlas-emoji.md:258-261` truncates a quote in its own favour.**
   `GlyphLayoutKernel.js:24-26` reads "wrong for every glyph **after an emoji on the same
   row** — and wrong by a whole cell." Cutting at "wrong for every glyph" turns a
   conditional invariant into an absolute one.
8. Smaller: `atlas-emoji.md:285` attributes `ContentTreeLabels.js:450` to
   `TerminalGrid.js:450` (`ensureCodepoints` is not called there);
   `atlas-emoji.md:130-131` says `isEmojiCodepoint` has three ranges, `FontChain.js:51-56`
   has four (the fourth subsumed, so harmless, but presented as complete);
   `atlas-emoji.md:232-233` calls all six bind-group entries "non-filtering integer
   textures" when two are uniforms and two are storage buffers;
   `layout-compute.md:163`'s "gpu_* referenced only by each other and `check.sh:39`" misses
   three comment references (`conformance_matrix.mojo:372-373`,
   `conformance_invariants.mojo:22`, `native/src/layout.rs:528`) — the "not wired"
   conclusion holds, the exhaustive framing does not;
   `layout-compute.md:172` cites "`README.md:304`" for content at `engine/README.md:312-315`.

**And `layout-compute.md:102,181,319` cite `BACKEND-PLAN.md:40-42`, `:44-64`, `:104-107`
— all three now land on unrelated sections** (the phantom row, displacement input,
`SegCull`). So §6.3's citation-drift problem is not confined to `review.md`: **two of the
six reports cite the rewritten plan by pre-rewrite line number.**

**6.6 `wrap-mode.md` and `interaction.md` — my own spot checks.**

`wrap-mode.md` is the newest report (`cc814b3`) and **every citation I opened was exact**,
which is the useful contrast with §6.5: the drift is a function of a report's age, not of
the care taken writing it.

```
grep -n 'fn wrap_segment_of\|fn wrap_row_of' native/src/fold.rs   → 364, 386   (doc: 364, 386)
grep -n 'rows_for_line' native/src/scan.rs                       → 156        (doc: 156)
grep -n 'def wrap_.*_of' engine/glyph_pipeline.mojo              → 653, 677   (doc: 653, 677)
grep -n 'def wrap_.*_of' engine/gpu_pipeline.mojo                → 113, 128   (doc: 113, 128)
ls -l native/fixtures/visual-check/one-long-line.txt             → 44001 B    (doc: "44,000 characters")
```

Its structural claim also holds by reading: `fold.rs:386` `wrap_row_of` **delegates** to
`wrap_segment_of` under `Down` and returns 0 under `Back`, so the default's row really is
the segment index by construction rather than by a second copy of the formula — which is
what the report says it is, and the reason mode A is provably unchanged.

`interaction.md`'s headline defect holds: "**`SegCull` cannot represent z, but `MoveGroup`
writes z.**" `glyph_scene.rs:254-256` is `min: [f32; 2], max: [f32; 2]`;
`glyph_scene.rs:2439-2452` (`Verb::MoveGroup`) does `g.cols[0][2] += d[2]` and then calls
`sync_segment(gid)`. The segment record has nowhere to put the z it was just moved by.
This is the same finding `BACKEND-PLAN.md`'s bounds section is built on, reached
independently.

**6.7 `review.md`, `wrap-mode.md`, `interaction.md` — a second sweep, ~270 assertions,
29 fail.**

A dedicated pass over B's three reports landed after §6.6 was written and is folded in
here. Its verdict matches mine in shape: **two substantive failures, both in `review.md`,
both caused by commits that landed after it was published; everything else is citation
placement, with the underlying claim confirmed by grep or by running the binary.** It
reproduced `wrap-mode.md`'s entire visual-check table cell-for-cell by running
`glyph3d-native` in both modes (including `col 43999 → row 439, (221.78, −68.75, −65.85)`
vs `row 0, (52.44, 0.00, −65.85)`), recomputed the 530 seed queries by parsing all eight
`.bake.bin` (402 prefix + 128 wrap), re-parsed `glyphs.bin`/`codepoints.bin` from
`FORMAT.md` and reproduced `review.md`'s V3 atlas census exactly, and confirmed
`fixture_census` prints `NEVER TOGETHER: paged + wrapback` by execution.

The substantive failures — I re-verified both:

**(a) `review.md:338-372` (V8) is not merely stale, it is inverted, and it reports "No
live inconsistency" about the one place there now IS one.** V8 concluded that
`rows_for_line` is `⌊n/w⌋+1` "in every running implementation". Since `c9667ec` that is
false in this tree, and the two trees now genuinely disagree:

```
sed -n '410,413p' engine/fixtures/inputs/glyphPipelineReference.js   # vendored, this tree
  export function rowsForLine(len, wrap, mode = WRAP_DOWN) {
      if (mode === WRAP_BACK) return 1;
      if (!(wrap > 0) || len <= 0) return 1;
      return Math.floor((len - 1) / wrap) + 1;      // ceil — phantom row removed

sed -n '385,388p' ../../viz-web/glyph3d-js/packages/glyph3d-core/src/compute/glyphPipelineReference.js
  export function rowsForLine(len, wrap) {
      if (!(wrap > 0)) return 1;
      return Math.floor(len / wrap) + 1;            // the phantom row, and no mode
  }
```

The vendored oracle was corrected and given a mode parameter; the web oracle was not.
`BACKEND-PLAN.md:29-31` states the rule — "edit the oracle, regenerate the corpus, let it
red the port, then fix the port" — and that was followed *for the vendored copy*. Whether
the fork is intended is a real question for the owner: `BACKEND-PLAN.md:24-26` says "The
JS oracle is a spent correctness source; the web *target* is served by Rust→wasm", which
would make it deliberate — but **no document in either tree records that the two oracles
now differ**, and `review.md` positively asserts they do not. That is the sentence a
reader would stop at.

**(b) All of `review.md:508-661` — section (d) — is unanchored.** Independently reached
and identical to §6.1/§6.3.

The rest of that sweep, condensed:

* **`wrap-mode.md:218-220` claims a mutation harness that "ASSERTS the edit landed
  (pattern occurs exactly once, file hash changes) and rebuilds after reverting". No such
  harness is in the tree.** Verified: `git ls-files | grep -iE 'mutat|mut-|mutan'` →
  nothing across the tracked set; `ls tools/` has thirteen entries and none of them is
  one. Twenty-two mutation results (M1–M19, M21–M23) rest on it and none can be re-run or
  inspected. This is the repo's own named failure mode — "a mutation that produced no
  failure proves nothing until you know it landed" — cited here as a *guarantee* rather
  than as something a reader can check. It is the least verifiable part of an otherwise
  unusually verifiable document, and the highest-value thing to fix in it.
* **`wrap-mode.md:143-144` describes `glyph_engine_load_item`'s positional form gaining a
  `wrap_mode: c_int` in the present tense.** That symbol no longer exists (`nm -gU` shows
  only `_glyph_engine_load_item_desc`), and the same document's "The fix" section explains
  the replacement — the two sections disagree in tense.
* **`wrap-mode.md:127-129`: `text::fold_leaders` is "cross-checked against the engine's
  own ROW lane on every repo load".** Verified false: its only non-test call site is
  `glyph_scene.rs:1880`, inside `ensure_pick_cache` — so it runs per **pick-cache fill**,
  not per load; `--load-repo`/`--repo-scan-only` never reach it. `interaction.md` states
  this correctly ("on every fill"), so the two reports disagree.
* `wrap-mode.md:262` calls `one-long-line.txt` "one line, **no newline**"; it has one
  (44,000 chars + `\n` = 44,001 B). `:208-209` says "718,291 bytes"; `conformance_real`
  prints **724,030** — I saw the same figure running `engine/check.sh cpu`. `:180-186`'s
  sha256 pair is over "all 22 committed fixtures" and is not reproducible at 25.
* **`interaction.md` §5 says `encase_bytes_match_bytemuck` "guards Rust-vs-WGSL agreement
  … genuinely decisive".** Refuted — and this is the same gap I confirmed independently in
  §5 under `BACKEND-PLAN`: nothing reads the WGSL `struct InstanceSlot`. `review.md` D-P4
  got this right; `interaction.md` still carries the wrong version. A reader trusting
  `interaction.md` would not go looking.
* ~20 further `file:line` drifts across the three, substance confirmed in every case
  (e.g. `review.md` V7 lists eight `@export`s with all eight line numbers wrong and names
  the deleted `_load_item` — the *count* of eight still holds on all three surfaces;
  `interaction.md`'s `glyph_instance_size_and_offsets` cite lands on
  `frame_uniform_size_and_offsets`, a different test, 30 lines away).

**6.8 `review.md`'s corrections landed in the plan and the source, and in none of the
reports.** `git show --stat b6827fb` shows all five subsystem reports *and* the
cross-review introduced in one commit, alongside the `layout.rs` fix and the
`BACKEND-PLAN.md` rewrite; `git log -1` on each report shows none has been touched since
(only `wrap-mode.md`, a later document, has its own commit). So `interaction.md` still
carries, verbatim and unmarked, everything `review.md` refuted — C5/R6 (the encase claim
above), R8 (the "add coalescing for free, and should" recommendation, against the web's
measured opposite at `MegaGlyphField.js:590-596`), and C6's diverging bucket labels.
**The cross-review functioned as input to the plan rewrite, not as errata against the
reports it reviewed.** Anyone reading `engine/delta/` in order meets the refuted claims
first and the refutation only if they reach `review.md` — which now carries two failures
of its own that nothing corrects.

**6.9** Worth stating as the counterweight: both `data-path.md` and `layout-compute.md`
are on the **correct** side of the `layout.rs` readback correction — neither claims the FFI readback
is unreachable, and both independently locate `engine.records()` in both strategy
branches. The reports got this right before the house-rules file did.

---

## 7. Not checked

* The four A/B baseline PNGs — I did not run gate 8 or `check-all` end to end (the
  renderer opens a device; the review was read-and-verify, not a gate run). Gates 2 (CPU
  half), 5 and 9 I did run.
* The five GPU suites (`engine/check.sh gpu`) — not run.
* Historical benchmark tables in `engine/README.md` (the M2 / linux-corpus numbers).
  They are self-consistent and cross-consistent with `BACKEND-PLAN`, but they are
  measurements on a corpus not in this tree and cannot be re-run here.
* `README.md`'s "egui 0.36 sets MSRV 1.95" — no `rust-version` key in
  `native/Cargo.toml` to check it against.
* All six delta reports were swept (§6.5, §6.7). The ~50 `file:line` drifts are listed by
  document in those passes but are **not individually reproduced in this file** — they are
  mechanical and the underlying claims were confirmed. Anyone repairing citations should
  redo the sweep rather than work from the summaries here.
* Web-repo-side claims were checked where they were cheap or load-bearing
  (`depthPerColumn`, `glyphPipelineReference.js:610` and `:385-387`,
  `MegaGlyphField.js:590-596`, `CodeGrid.js:218-219`, `syncGpuLayout`'s call set). The
  reports make many more; the web half is not exhaustively audited here.
* **Whether the oracle fork (§6.7a) is intended.** I established that it exists and that
  nothing documents it. Whether the web oracle should follow `c9667ec` is a decision, not
  a fact, and it is the owner's.

---

## 8. If you fix only three things

1. **`native/AGENTS.md:113-116`** — delete the "unreachable" sentence, or replace it with
   `layout.rs:58-66`'s wording. It is the first file agents are told to read and it
   contradicts the source it describes (§1.1, §6.4).
2. **`engine/README.md:509-553`** — the "read this first, next agent" section is about a
   different repository. Either rewrite it against `pixi.toml` or delete it and point at
   root `AGENTS.md`'s "Environment & build". While there, `:206-209` (three suites
   declared red), `:24` (`bun tools/gen-schema.mjs`), `:84` and `:144` (§1.3–1.5).
3. **The three counts.** 25 fixtures, 16 suites, 12 gates — currently wrong in eighteen
   places (§2). The gates themselves already compute these dynamically; the documents are
   the only place they are hardcoded.

And one thing that is a decision rather than a fix: **the two JS oracles have forked**
(§6.7a). `c9667ec` corrected `engine/fixtures/inputs/glyphPipelineReference.js` and gave
it a wrap mode; the web repo's copy still returns `floor(len/wrap)+1` with no mode. That
may be exactly right — `BACKEND-PLAN.md:24-26` calls the JS oracle spent — but no document
says it happened, and `review.md:338-372` says the opposite.
