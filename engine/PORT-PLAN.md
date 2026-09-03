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
| **1** | `GlyphTrie` -> Rust | rebuilds a fixture's trie BYTES | the ordering landmine |
| **2** | serial fold -> Rust | fixture expected measures BIT-EXACT | the float discipline |
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

1. **`WorldTrie`** (`text.rs`) is the new seam. The fold used to take
   `&TrieTable` — font units, atlas only — so the fixture corpus could not
   reach it at all. Both tries now implement one trait and the fold is generic.
2. **The float discipline is now DETECTABLE.** Mutating `line_adv` to
   accumulate in f32 reddens the corpus diff with one-ulp X/BASE_X divergences,
   named by lane and byte. Landmine 2 below has a gate watching it BEFORE the
   code that can trip it gets written.

The domain is a PREDICATE (`out_of_domain`), not a file list, so widening the
fold in stage 2 automatically widens what it is held to. Six mutations were run
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
