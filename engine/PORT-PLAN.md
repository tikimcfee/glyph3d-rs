# Porting the JS reference pipeline to Rust — plan, gates, and landmines

Written 2026-09-02, before any code, so the work survives a context boundary.
**Self-contained on purpose**: everything needed is in this repo or stated here.
No memory of the session that wrote it is required.

## What is being ported, and why it is worth it

Four JS modules in the web repo (`viz-web/glyph3d-js/packages/glyph3d-core/src/compute/`):

| module | lines | what |
|---|---:|---|
| `glyphPipelineReference.js` | 779 | the serial fold: UTF-8 decode, trie lookup, wrap, pagination, positions |
| `glyphPipelineScan.js` | 313 | the parallel scan form of the same fold |
| `glyphBake.js` | 250 | the bake / prefix layer |
| `GlyphTrie.js` | 201 | codepoint -> glyph trie construction |

Their import closure is CLEAN — they import only each other. No three.js, no
external deps, zero module-level mutable state, zero `Date`/`random`/
`performance`. Verified 2026-09-02.

Four things land when this does:

1. **The wasm target unblocks.** `native/src/text.rs::reference_layout` is
   already a bit-exact port of the fold for `wrap = 0, no pages`. Extending it to
   full `ItemParams` IS stage 2 of this plan. That was blocker #1 of
   `research/wasm-port-audit.md`.
2. **The fixture corpus un-freezes.** New `.pipe.bin` fixtures can be generated
   in-tree instead of never.
3. **The last engine-side JS retires.**
4. **The Rust layout becomes the single production path**, with Mojo as an
   oracle rather than a runtime dependency — one code path under test rather
   than two, only one of which is ever exercised.

## Stages, each with its own gate

Staged by ACCEPTANCE TEST, not by file size, so every stage lands with a gate
instead of a promise.

| stage | work | acceptance | risk |
|---|---|---|---|
| **0** ✅ | Rust `.pipe.bin` reader + bit-exact differ | reads all 14 fixtures; counts agree with `engine/fixture_io.mojo` | none, pure plumbing |
| **1** ✅ | `GlyphTrie` -> Rust | rebuilds a fixture's trie BYTES | the ordering landmine |
| **2** ✅ | serial fold -> Rust | fixture expected measures BIT-EXACT | the float discipline |
| **3** | scan form -> Rust | tiered agreement with stage 2 | monoid associativity |
| **4** | bake -> Rust | the 8 `.bake.bin` fixtures | lowest |

**Stage 0 is DONE** (2026-09-02). `native/src/fixture.rs` mirrors
`engine/fixture_io.mojo` section for section, including its carrier split, and
refuses any parse that does not consume the whole file. Gate 9,
`tools/check-fixture-parity.sh`, is its acceptance test and has two halves:

- **Parse parity.** Both loaders emit FNV-1a checksums over their PARSED, TYPED
  values (`--fixture-manifest` / `engine/fixture_manifest.mojo`) and the lines
  are diffed. Hashing the FILE would have proved nothing — that is the one thing
  both sides are guaranteed to agree on. All 14 fixtures, 11 sections each.
- **Corpus diff.** `text.rs::reference_layout` is laid against every fixture
  inside its domain and diffed BIT-EXACT against the oracle's own expected
  lanes: 4 fixtures, 5332 records, 47988 lanes, 7 of 8 measure lanes plus ROW
  and COL. LINE_ADV is the fold's witness lane and `RefGlyph` does not carry
  it — stated in the code rather than quietly omitted.

Two things fell out that stage 2 inherits:

1. **`ResolveGlyph`** (`text.rs`) is the new seam. The fold used to take
   `&TrieTable` — font units, atlas only — so the fixture corpus could not
   reach it at all. Both tries now implement one trait and the fold is generic.
2. **The float discipline is now DETECTABLE.** Mutating `line_adv` to
   accumulate in f32 reddens the corpus diff with one-ulp X/BASE_X divergences,
   named by lane and byte. Landmine 2 below has a gate watching it BEFORE the
   code that can trip it gets written.

The domain is a PREDICATE (`out_of_domain`), not a file list, so widening the
fold in stage 2 automatically widens what it is held to.

**Stage 1 is DONE** (2026-09-03). `native/src/glyph_trie.rs` is the port;
`--fixture-trie` rebuilds every fixture's trie from its own BYTES and compares
through `wire_value`, the single place wire order lives — so a transposed lane
in the serializer fails, which comparing the split arrays element-wise would
have missed. **14 fixtures, 11520 entries, 45 blocks** including `real-kernels`
at 7, where insertion order genuinely bites.

The input recipe is reconstructible in-tree because `gen.mjs` is vendored: the
codepoint set comes from the fixture's own bytes and the metrics are a pure
function of the codepoint. Two things that recipe pinned down:

- **The codepoint derivation uses the CONFORMANT decoder**, not the engine's
  lenient classifier — `gen.mjs` collects from
  `new TextDecoder('utf-8', {fatal:false})`, so `String::from_utf8_lossy` is the
  Rust counterpart. The two must not be confused: swapping in the lenient
  classifier reddens `malformed.pipe.bin` (it yields U+20A2 from a truncated
  3-byte sequence where WHATWG yields U+FFFD, which is then dropped). That
  fixture resolving to exactly 5 mapped codepoints is the WHATWG answer.
- **`Math.fround(expr)` narrows ONCE**, after an f64 evaluation. Writing
  `metricsFor` in stepwise f32 reddens ADVANCE at
  0.7384000420570374 vs 0.7383999824523926 — the same hazard as landmine 2, one
  layer earlier.

Eight mutations were run; seven reddened and the eighth found the dedup ceiling
above.

**Stage 2 is DONE** (2026-09-03). `native/src/fold.rs` is the port: decode ->
fold -> paginate -> per-item boxes -> batch union, serial. The Mojo shards all
four of those, but every decomposition is over disjoint ranges or an exact
min/max, so the parallelism is not part of the contract and nothing here
reproduces it.

`--fixture-fold` compares **EVERY lane of EVERY byte** — all 8 measure lanes and
all 4 count lanes — plus `ordToByte`, the miss list, the leader count, every
per-item box and the batch union. **14 fixtures, 149,767 leaders, 1,807,512
per-byte lanes, bit-exact.** Non-leader bytes are compared too rather than
skipped: zero is their defined state, so a port that leaves them dirty fails.

GLYPH_ID is compared as **u32**, not through an f32 view. The Mojo's `m_at`
refuses to return one for that lane — "a checker must carry it the way the
pipeline does" — and this comparison honors that.

Two corrections worth carrying:

- **`exp_ord` was a misleading name and it cost a wrong comparison.** The
  section is `u32[byteLen] ordToByte` (gen.mjs), the INVERSE map, not a second
  copy of the ORD lane. The first version of the check compared it to ORD on the
  strength of the field name; every multi-byte fixture reddened while the
  FIX_C_ORD lane beside it passed, which is what named the mistake. The field is
  `exp_ord_to_byte` now, in Rust and Mojo alike, and the manifest key is `h.otb`.
- **`fixture_census.mojo` was inventing a blind spot.** Its `Range` let NaN into
  `lo`/`hi`, where it poisoned them permanently — every comparison against a NaN
  bound is false — so any field whose FIRST value was NaN reported UNIFORM. It
  claimed `page_line_height always nan` for a corpus whose five paged fixtures
  carry 1.0, 1.1, 1.2 and 1.3. NaN is now segregated and counted as its own
  value; the census reports "every field varies."

### The corpus ceilings stage 2 found

16 mutations; **14 reddened**. The two that did not are properties of the CORPUS,
and both are now covered by unit tests in `fold.rs` that were each verified to
fail under the mutation the corpus lets through:

| mutation | why the corpus cannot see it | covered by |
|---|---|---|
| paginate stops skipping non-leader bytes | the only paged fixture with a non-leader (`real-kernels`) has origin_y = origin_z = 0, so remapping a zero row/col/base_x writes zeros back | `paginate_leaves_non_leader_bytes_alone` |
| paginate consults `page_line_height` | every paged fixture has `page_line_height == line_height` — so the fallback DELETED as unreachable in 4697e3b is unverified by the corpus | `paginate_ignores_page_line_height` |

Two further mutations were **semantically no-ops**, not ceilings, and saying so
matters because they print identically to a ceiling:

- `screen_row % rows` for `screen_row - y_page * rows` is IDENTICAL for every
  `screen_row > -rows`, which is the whole corpus (`paged-rows` scrolls 3 against
  6 rows). Covered anyway by
  `a_row_scrolled_past_a_whole_page_stays_in_flow`.
- `write_bounds` forced true for paged items is a dead store: the bounds pass
  resets lanes 0-5 to ±inf and recomputes them. The flag is an optimization,
  not a correctness condition.

And one mutation in the first battery **did not land at all** — the `seg_adv`
f64 attempt only introduced an unused variable, so its green meant "I failed to
break it," not "the corpus cannot see it." Redone properly it reddens 6
fixtures. That is the distinction this repo keeps paying for: assert the edit
changed the ARITHMETIC, not just the text. Six mutations were run
against gate 9 — swapped item fields, swapped carrier split, a reordered
section of identical size, a dropped trailing section, the f32 `line_adv`, and
every fixture forced out of domain — and all six reddened.

**Stage 2 alone unblocks wasm.** If that is the priority, 0+1+2 is the
deliverable and 3+4 can trail.

The acceptance corpus is `engine/fixtures/*.pipe.bin` — 14 files, 12.8 MB,
each carrying input bytes AND expected outputs. No oracle, no network, no JS
needed to run the gate.

## LANDMINE 1 — trie block layout depends on ITERATION ORDER

`buildGlyphTrie` groups codepoints with a JS `Map` and assigns block storage
indices by `built.length` **in insertion order**. MEASURED: the same codepoints
supplied in a different order produce a different `blockIndex` AND different
block arrays (block `0x4e` lands at index 2 in text order, 3 in sorted order).

A Rust `HashMap` scrambles that order; a `BTreeMap` sorts it. **Both produce a
valid-but-different trie, and then every fixture mismatches with a diff that
reads "everything is wrong" rather than "your map is unordered."** Use
`indexmap::IndexMap` for both the `byBlock` grouping and the `seen` content-dedup
map.

Second half of the same landmine: the content-dedup key in JS is a STRING,
`` `${e.join(',')}|${m.join(',')}` `` — number-to-string formatting Rust cannot
reproduce and should not try. Key on the raw bit patterns instead.

**RESOLVED in stage 1** (`native/src/glyph_trie.rs`, 2026-09-03). Three findings
worth carrying forward:

- The grouping is a `Vec` plus an index `HashMap` — insertion-ordered by
  construction, and NO new dependency in a repo whose gates are byte-exact.
  `indexmap` was not needed. The `seen` dedup map is a plain `HashMap` because it
  is never iterated: slots come from `built.len()` at insertion, so its order
  cannot reach the output. `block_storage_order_follows_codepoint_order` pins the
  distinction so a future "tidy-up" to a `HashMap`/`BTreeMap` grouping fails
  there, with a reason, instead of as fourteen unreadable fixture diffs.
- The bit-pattern key is **strictly finer** than the JS string key: JS renders
  +0.0 and -0.0 both as `"0"` and would merge two blocks differing only in a
  zero's sign. No fixture contains a signed-zero measure, so they agree here —
  but that is the direction of the difference if one ever does.
- **The corpus cannot discriminate content dedup AT ALL.** Disabling it leaves
  all 14 fixtures value-identical, because `metricsFor` derives glyph_id from
  `cp % 4093` and advance/height from `cp % 13` / `cp % 7` — functions of the
  whole codepoint — so two distinct blocks can never agree. The branch is
  structurally unreachable for this generator, not merely unexercised, and
  `identical_blocks_share_one_slot` is the only check covering it. Recorded at
  the dedup site too; do not read the corpus gate's green as evidence there.

## LANDMINE 2 — the float discipline is HYBRID, on purpose

Three regimes coexist inside `layoutItem`. A Rust port written naturally with
`f32` locals reproduces NONE of them.

| quantity | discipline | why |
|---|---|---|
| `segAdv` (the fold > 0 x) | **`Math.fround` per add** — genuine f32 arithmetic | matches the GPU's f32 summation order, which is what makes fold>0 lanes bit-exact across groupings |
| `lineAdv` (the foldless x) | accumulated in **f64**, narrowed **once** on store | the oracle is the truth layer; the f64 prefix sits between CPU serial-f32 drift and the GPU's log-bounded tree |
| `M_Y = -row * lineHeight + oy`, `M_X = x + ox` | computed in **f64**, narrowed **once** | single rounding, not two |

Put that table in the Rust source, not only here. It is three different regimes
inside one function and the compiler will not tell you which one you got.

Related: `conformance.mojo` bit-pins X only up to x ~ 3.5k (that is as far as
`long-line.pipe.bin` reaches). Above that the f64 accumulation is unpinned in
every layer, so a drift there fails no existing gate.

**RESOLVED in stage 2** (`native/src/fold.rs`, 2026-09-03). BOTH float regimes
are discriminated by the corpus, verified by mutation:

- `line_adv` in f32 instead of f64 reddens 13 fixtures at one ulp of X/BASE_X
  (`0x40c8ab37` vs `0x40c8ab36`).
- `seg_adv` in f64 instead of f32 reddens 6 fixtures, also one ulp
  (`0x4137e7d5` vs `0x4137e7d6`).
- Narrowing Y twice instead of once reddens 2 fixtures
  (`-0.1500001` vs `-0.15`).

The table is reproduced in the Rust source, as this plan asked.

## Other things known before starting

- **29 bitwise ops** in `glyphPipelineReference.js`. JS bitwise coerces to
  int32; match that deliberately rather than by accident.
- **`gen.mjs` needs `FIXTURE_MEASURE_STRIDE` / `FIXTURE_COUNT_STRIDE`** from
  `engine/glyph_schema.mjs` — a THIRD gen-schema output that
  `tools/gen_schema.py` deliberately does not emit and that does not exist in
  this tree. Stage 4 (or whoever regenerates fixtures) needs those constants;
  read them from `schema/glyph-identity.json` rather than reviving the .mjs.
- **The fixture corpus inputs are vendored** at `engine/fixtures/inputs/`
  (`foldGeometry.js`, `glyphPipelineKernels.js`, `minified-sample.js`), frozen
  at web commit `2ef79b7`. Nothing reads them yet; they are there so
  regeneration has a defined input.
- **The decode is a LENIENT classifier** — it never validates continuation
  bytes. Out-of-range codepoints (lead bytes `0xF5`-`0xF7`, or `0xF4` with a
  continuation above `0x8F`) resolve through the shared missing block, block 0.
  Both implementations agree on this as of `0ae7010`;
  `native/fixtures/overflow-leads.txt` is the only input that reaches it.
  A port MUST keep that contract — `str::from_utf8`, `chars()` and
  `from_utf8_lossy` all implement the CONFORMANT decoder and will diverge.

## What NOT to do

- **Do not regenerate over the existing fixtures.** They cannot be reproduced
  anyway (`glyphPipelineKernels.js` moved upstream in `3da6542` after they were
  made), and they are the last artifacts with direct JS-oracle provenance. New
  fixtures ADD; they do not replace. Superseded evidence lives in git history.
- **Do not reach back into `viz-web/glyph3d-js` to fix anything.** It is not
  trunk. A known defect there stays there: `real-kernels.pipe.bin`'s page bag is
  typo'd (`rows`/`gapX` where the oracle reads `pageRows`/`pageGapX`) so it does
  not paginate despite advertising "wrapped AND paged at once". This tree closes
  that hole when it regenerates, not by editing theirs.
- **Do not deduplicate the second realization.** `tools/gen_real_trie.py`'s
  `to_world` and `native/src/text.rs::fu_to_world` are the SAME formula written
  twice on purpose, and `--engine-check` diffs them. Merging them turns a check
  into a function compared with itself. The file says so; believe it.
- **Do not "simplify" the hybrid float discipline.** See landmine 2.

## Verification habits this repo runs on

Stated because they are why the gates here mean anything, and a port is exactly
where they get skipped.

- **A green must be earned.** Before trusting a passing check, break the thing it
  watches and confirm it reddens. Five checks written in one day this week could
  not fail; all five were caught by attacking them, none by reading them.
- **A red proves nothing until you know it failed for the RIGHT reason.**
- **Assert the edit landed.** A failed anchor match is silent.
- **A label is a claim.** If a test is parameterized (`page=1`, `wrap=5`),
  assert the parameter ENGAGED — self-consistent checks pass loudest when
  nothing happened. That found 16 vacuous cells in `conformance_matrix`.
- **`check-all.sh` reads the WORKING TREE, not HEAD.** Work in a worktree if
  anyone else is active in this repo. Setup in `engine/TOOLCHAIN.md`.
