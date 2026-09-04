# Wrap mode — a per-item choice of what a wrap costs

A wrap has always cost a ROW. It can cost DEPTH instead, and which one it costs is
now an item-level parameter.

    WrapDown  (0, the default)  a wrap advances the visual row.
                                A line of n cells occupies ceil(n / wrap) rows.
    WrapBack  (1)               a wrap does NOT advance the row. Every segment of
                                a line shares ONE row and stacks in depth, each
                                z_step further back.

`rows_for_line(n, wrap, WrapBack) == 1` for every n, so under WrapBack a line's ROW
is exactly its line index.

Why: a 305,978-character line in a real bundle takes 3,060 rows under WrapDown — about
24 page columns — and shoves every later line that far into the distance. Under
WrapBack it takes one, and the derangement goes into the axis nothing else is using,
so the pages around it keep their structure. Measured on
`native/fixtures/visual-check/one-long-line.txt` (44,000 characters, one line, wrap
100): 440 rows in mode A, 1 in mode B; the repo field's footprint goes 369x382 -> 255x326.

## What does NOT change in mode B

`col` still counts within the LOGICAL line. `segment_advance` still resets at every
fold boundary, so each segment still starts at x = 0. The wrap SEGMENT index still
exists — it feeds z and no longer feeds row. Picking by `(row, col)` still resolves
uniquely, because col differs between segments (verified below).

That last point forced the one structural change in the fold: the single function that
answered "(col, wrap) -> row" had to become two.

- `fold::wrap_segment_of(col, wrap, terminator)` (`native/src/fold.rs:364`) — the
  DEPTH fan's segment index. Mode-free, and byte-for-byte what `wrap_row_of` returned
  before modes existed.
- `fold::wrap_row_of(col, wrap, terminator, mode)` (`native/src/fold.rs:386`) — the
  ROW contribution. `Down` **delegates** to `wrap_segment_of`; `Back` returns 0.

The delegation is the whole of the default's proof: mode A's row *is* the segment
index, by construction rather than by a second copy of the formula. Mojo twins at
`engine/glyph_pipeline.mojo:653` / `:677`, GPU mirrors at `engine/gpu_pipeline.mojo:113`
/ `:128`.

## The monoid, and what it now assumes

`scan::scan_combine`'s junction term was

    rows_for_line(a.tail_len + b.head_len, b.wrap)

and is now

    rows_for_line(a.tail_len + b.head_len, b.wrap, b.mode)   native/src/scan.rs:156

**This makes the non-associative surface WIDER, not narrower.** `combine` was already
non-associative across a change of `wrap`; it is now non-associative across a change of
`wrap` OR of `mode`, because a triple with `a.nl > 0, b.nl > 0, c.nl == 0` evaluates that
term with `b`'s parameters under the left grouping and `c`'s under the right, and either
parameter differing is enough to split them. Nothing about the mode simplifies the
domain. Mode inherits the wrap's precondition wholesale:

> mode is an ITEM-level parameter, never per line and never per range; every item
> boundary emits a resetting leaf, and a reset absorbs whatever preceded it, so no
> interval without a reset can span two modes.

If mode ever becomes per-line or per-range, the scan form's regrouping freedom goes with
it, and `scan::tests::mixed_mode_is_outside_the_monoid_s_domain` is where that shows up.

`ScanElem` gains `mode` (`native/src/scan.rs:83`), `scan_leaf_value` carries it
(`:91`), `scan_combine` propagates it beside `wrap` (`:127`), `lanes_from_prefix` takes
it (`:172`). Mojo twin: `engine/glyph_bake.mojo:67` / `:85` / `:125` / `:157`. Device
twin: the scan-partial buffer gains a `P_MODE` lane (`schema/glyph-identity.json`
scanPartial, stride 7 -> 8; `engine/glyph_schema.mojo:85`), because a partial that
travelled without its mode answers the junction with the wrong rule.

### The measured counterexample

`native/src/scan.rs:882` — `mixed_mode_is_outside_the_monoid_s_domain`. Same wrap (2)
throughout, so the ONLY difference between the two `c` operands is the mode. The junction
line is 5 cells: `rows_for_line(5, 2, Down) == 3`, `rows_for_line(5, 2, Back) == 1`.

    a = (nl 1, glyphs 3, rows 1, head 1, tail 2, wrap 2, Down)
    b = (nl 1, glyphs 4, rows 1, head 3, tail 1, wrap 2, Down)
    c = (nl 0, glyphs 2, rows 0, head 2, tail 2, wrap 2, BACK)

    ((a . b) . c).rows == 5      a . (b . c) .rows == 3

pinned as `assert_eq!((left, right), (5, 3))`. With `c` uniform (Down) both groupings
give 5, asserted in the same test. That is a measurement, not an argument.

Its constructive half is `mixed_modes_across_an_item_boundary_agree_with_the_serial_fold`
(`native/src/scan.rs:921`): two items in one arena, WrapDown then WrapBack, run through
the scan form at five chunk/group/shard tunings including ones that straddle the boundary
— every exact lane, Y, Z and `ordToByte` bit-equal to the serial fold, because the
boundary's resetting leaf absorbs the prefix before a mode can leak.

### The sweep runs wrap × mode

`the_monoid_is_associative_on_every_integer_field` (`native/src/scan.rs:688`) was six
wrap regimes; it is now **six wraps × two modes = 12 regimes, 6·2·15³ = 20,250 × 2 =
40,500 triples**, every integer field exact within each regime.

The regimes are checked to DISCRIMINATE rather than assumed to (`native/src/scan.rs:763`):
at every wrap that can fold (1, 2, 3, 4, 7) the Down and Back regimes must produce a
different `rows` on the same sample, and at wrap 0 they must coincide — a mode that
changed something at wrap 0 would be reaching past its own definition. This is the same
lesson the wrap widening learned, one parameter over: without it the cross could sweep
one rule twice under two names. Mutation-verified below.

## The rest of the plumbing

- `fold::Item.wrap_mode` (`native/src/fold.rs:129`), `layout::ItemParams.wrap_mode`
  (`native/src/layout.rs:174`), `Item.wrap_mode` in Mojo (`engine/glyph_pipeline.mojo:139`).
- The fold reads BOTH indices per leader (`native/src/fold.rs:511`,
  `engine/glyph_pipeline.mojo:759`): `wrap_segment` for Z, `wrap_row` for ROW.
- `paginate` reads the SEGMENT for Z (`native/src/fold.rs:639`,
  `engine/glyph_pipeline.mojo:896`, `engine/gpu_paginate.mojo:128`,
  `engine/gpu_pipeline.mojo:471`) and needs no mode of its own: the ROW lane it gates on
  already carries it. So pagination follows the mode for free — a WrapBack file's page
  breaks land on its line indices.
- `bake::rows_under_wrap(record, wrap, mode)` (`native/src/bake.rs:311`,
  `engine/glyph_bake.mojo:381`). Under WrapBack it counts LINES; it still walks the
  histogram rather than short-circuiting, so the two answers come out of one rule asked
  once per line.
- The bake RECORD is mode-free and says so (`native/src/bake.rs:45`): it folds at wrap 0,
  where `rows_for_line` is 1 under either mode. Mode is a QUERY parameter there, exactly
  like wrap. Mutation M18 confirms it — dropping mode propagation from the Mojo monoid
  reds the scan suite and leaves the bake suite green.
- `text::fold_leaders(bytes, wrap, mode)` (`native/src/text.rs:502`) — the pick path's
  independent row/col oracle, cross-checked against the engine's own ROW lane on every
  repo load. It is a SEPARATE lineage from the port, so it had to learn the mode too.
- `repo::RepoParams.wrap_mode` (`native/src/repo.rs:207`), defaulting to `Down`, plus
  `--wrap-mode down|back` (`native/src/main.rs:361`). An unknown value is refused by
  clap and `parse_wrap_mode` panics rather than falling back, because a silent fallback
  would render mode A while the operator believed they asked for B.

## The FFI: 8 spare bytes, verified before use

The 128 B item descriptor held 10×f64 + 6×i32 + 2×u64 = 120 B with 104..112 as pad.
`wrap_mode` becomes the seventh i32 at offset **104**, so the block is 124 B and
`ITEM_DESC_SIZE` does not move (`engine/ffi.mojo:199`, `native/src/engine.rs:108`).
The fit is asserted from the array rather than from a literal —
`assert!(80 + i32s.len() * 4 <= 112, ...)` at `native/src/engine.rs:120` — so adding an
i32 without moving the size fails loudly instead of overwriting `byte_start`.
`glyph_engine_load_item`'s positional form gained a `wrap_mode: c_int` beside
`wrap_width` (`engine/ffi.mojo:123`).

## The corpus

**Format v3 -> v4.** The `.pipe.bin` item record gains `f64 wrapMode` immediately after
`wrapWidth` (`engine/fixtures/gen.mjs`, `native/src/fixture.rs:289`,
`engine/fixture_io.mojo:148`). Both readers REFUSE an out-of-range code rather than
defaulting it. Field order is load-bearing for the parse-parity manifest, so
`wrap_mode` was added to both hashes in the same position (`native/src/fixture.rs:449`,
`engine/fixture_manifest.mojo:109`).

**Bake format v2 -> v3.** Both query kinds gain a mode: `prefixQuery` becomes
`{byteIndex, wrap, wrapMode, prefix[7], row, col, ord, lineAdv}` and `wrapQuery` becomes
`{wrap, wrapMode, rows}` (`engine/fixtures/gen-bake.mjs`, `native/src/bake.rs:492`,
`engine/conformance_bake.mojo:199`). Both wraps × both modes are now recorded on every
query byte, which is what puts `rows_under_wrap` under WrapBack on the oracle's side of
the fence instead of a unit test's: 530 seed-protocol queries, up from 265.

**Three fixtures ADDED, none flipped**, so every mode-A expected value in the corpus is
untouched:

| fixture | shape |
|---|---|
| `wrapback-mixed` | byte-for-byte `wrap-exact`'s input and params with mode 1 — a controlled A/B where the only difference is the mode. Lines of 4, 8, 2, 12, 0, 2 cells at wrap 4: three exact multiples, one partial, one empty. |
| `wrapback-long-line` | one 5,212-cell line at wrap 40. 131 rows under WrapDown, ONE under WrapBack. |
| `wrapback-items` | three items, modes down/back/down, wraps 5/5/7, emoji in the middle item — mixed modes in one arena, which is the shape the monoid's precondition is about. Swept over 8 chunk/group/shard tunings by gate 9. |

Corpus is now 17 `.pipe.bin` + 8 `.bake.bin`. The count is pinned as a COUNT
(`native/src/fixture.rs:1328`), not as "nonzero", so a fixture that stopped being
discovered lowers the assertion instead of quietly lowering coverage.

### The default is provably unchanged, and here is the measurement

Before bumping the format, the oracle's mode support was added with the mode defaulted
and the generators untouched, and the corpus regenerated:

    sha256 over all 22 committed fixtures, before: 07806ca132b8…
    sha256 over all 22 regenerated fixtures, after: 07806ca132b8…   IDENTICAL

(with `glyphPipelineScan.js` temporarily at its committed revision, because
`bake-repo-file` and `bake-repo-small-k` embed that file's own BYTES as their input —
editing it changes those two fixtures for a reason that is not semantic.) Every lane of
every byte of the mode-A corpus is bit-identical under the mode-aware oracle. Then the
format bumped and the three fixtures were added.

The renderer's four byte-equal screenshots are green under the shipped default, and
gate 8 is shown below to be capable of seeing the mode, so that green is a statement
rather than a structure.

## Where mode B is exercised beyond the corpus

The census reports honestly that the corpus has no `paged + wrapback`,
`trie-miss + wrapback` or `scrolled + wrapback` item (`engine/fixture_census.mojo`, now
17 fields and 6 properties). Those live in the constructed suites instead:

- `engine/conformance_matrix.mojo` — the matrix gained a MODE dimension: 48 -> **96**
  cells over wrap × mode × page × scroll × items × misses, 3,000-byte corpus each.
  Its wrap-engagement check had to INVERT for WrapBack (`:180`): a WrapBack item's
  maximum ROW is exactly its newline count however long its lines are, so WrapDown's
  evidence of wrapping must be ABSENT, and the evidence that the fold folded is a
  column past the wrap width instead. Two claims per WrapBack cell, not one.
- `engine/conformance_invariants.mojo` — containment and paginate-idempotence now sweep
  the wrap mode as a sixth dimension (156,992 quads, 68 paged items).
- `engine/conformance_real.mojo` — all 24 real files run under **both** modes
  (718,291 bytes × 2), serial vs scan.
- `engine/gpu_pipeline.mojo` — the synthetic multi-super case (20k/40k/70k bytes, past
  the GROUP=256 spine threshold) now runs a WRAP_BACK variant. Every WrapBack fixture is
  single-super, so without it the mode's junction term would never meet a chunk- or
  super-level combine on device.
- `engine/conformance_resume.mojo` picks the new fixtures up for free: they are unpaged,
  so the resume protocol replays them from bake checkpoints.

## Mutation results

Every mutation is applied by a harness that ASSERTS the edit landed (pattern occurs
exactly once, file hash changes) and rebuilds after reverting. A silent failed replace
and a real null result print differently.

| # | mutation | result |
|---|---|---|
| M1 | `fold::rows_for_line` loses its `Back` branch | RED — fold 2/17 (`wrapback-items`, `wrapback-mixed`), scan 2/17, 5 unit tests incl. both new ones |
| M2 | `fold::wrap_row_of` stops zeroing under `Back` | RED — fold **3/17**, scan 3/17, 1 unit test |
| M3 | the serial fold's Z reads the row instead of the segment | RED — fold 3/17. Scan GREEN, correctly: a different code path |
| M3b | the SCAN form's Z reads the row instead of the segment | RED — scan 3/17. Fold GREEN, same reason |
| M4 | `scan_combine` stops propagating `mode` | RED — scan 1/17 at K=7/G=3/S=3, 3 unit tests |
| M5 | `scan_leaf_value` stops carrying `mode` | RED — scan 1/17, 2 unit tests |
| M7 | the FFI descriptor writes 0 for the mode | RED — `--repo-verify --wrap-mode back`: item 4 placement differs |
| M10 | the parse-parity manifest stops hashing the mode | RED — gate 9 parse parity, Rust vs Mojo disagree |
| M11 | the CLI default flips to `back` | RED — `repo-wide.png` DIFFERS from the baseline |
| M12 | `rows_under_wrap` ignores the mode | RED — bake 4/8 fixtures, 1 unit test |
| M13 | Mojo `rows_for_line` loses its `WRAP_BACK` branch | RED — conformance, conformance_scan, conformance_matrix |
| M14 | Mojo GPU `rows_for` loses its `WRAP_BACK` branch | RED — gpu_pipeline |
| M15 | Mojo `fixture_io` reads the mode and drops it | RED — conformance, gate 9 parse parity |
| M16b | `elements()` stops varying with the mode, so the regime cross sweeps one rule twice | RED — *"wrap 1: the two modes agreed on all 3375 triples — this regime cannot tell the modes apart and the cross is decorative there"* |
| M17 | Mojo `wrap_row_of` stops zeroing under `WRAP_BACK` | RED — conformance, conformance_scan |
| M18 | Mojo `scan_combine` stops propagating `mode` | RED — conformance_scan. **conformance_bake GREEN** — the record really is mode-free |
| M19 | the GPU chunkReduce junction folds every line as `WRAP_DOWN` | RED — gpu_scan |

M13 also fires the matrix's inverted engagement check by name:

    items=1 wrap=5 mode=1 page=0 scroll=0 miss=0
      WRAP_BACK item 0 reached row 650 past its 130 newlines — a wrap spent a row

Two mutations that were NOT confounded matter as much as the reds. M1 leaves
`wrapback-long-line` green, because that fixture has no newline and `rows_for_line` is
only reached at one; M2 is what catches it. The two halves of the rule need two
fixtures and two mutations, the same pairing the phantom-row correction needed.

Additional unit-level guards, each mutation-checked by M1/M2 above:
`fold::tests::rows_for_line_counts_only_rows_a_cell_reaches` now asserts the whole
table again under `Back` (always 1) with an anti-vacuity check that the table contains
a line WrapDown actually folds; `a_newline_rides_the_row_it_closes` asserts
`wrap_segment_of == wrap_row_of(.., Down)` at every entry (the delegation, stated as a
test) and that `Back` is 0, with an anti-vacuity check that some column is past the
first segment.

## Visual check — mode B on a 44,000-character line

`native/fixtures/visual-check/one-long-line.txt`, one line, no newline, wrap 100,
`z_step` 0.15, `line_height` 1.25. `fold cross-check: PASS (0 mismatch)` in both modes —
`text.rs`'s independent oracle agrees with the engine's own ROW lane record for record.

| col | mode A `(row, x, y, z)` | mode B `(row, x, y, z)` |
|---:|---|---|
| 0 | 0, (0.00, 0.00, 0.00) | 0, (0.00, 0.00, 0.00) |
| 99 | 0, (52.44, 0.00, 0.00) | 0, (52.44, 0.00, 0.00) |
| 100 | **1**, (0.00, −1.25, −0.15) | **0**, (0.00, 0.00, −0.15) |
| 22000 | **220**, (56.44, −115.00, −33.00) | **0**, (0.00, 0.00, −33.00) |
| 43999 | **439**, (221.78, −68.75, −65.85) | **0**, (52.44, 0.00, −65.85) |

    pick --pick-row 1 --pick-col 0   ->  "no record on row 1 in one-long-line.txt"

440 rows become 1. Z is unchanged in every row of the table — the 440 segments still
recede 0.15 apart to −65.85 — which is the point: the derangement moved from y to z and
nothing else moved with it. X still restarts at 0.00 at every segment boundary (col 100,
col 22000 are segment starts). Col is unchanged, so `(row, col)` still resolves a unique
glyph even with 440 segments sharing row 0.

Mode A's x at col 43999 is 221.78 rather than 52.44 because pagination fans page columns
off the ROW lane: mode A's row 439 is page 3 of a 128-row page, mode B's row 0 is page 0.
That is pagination correctly following the mode, not a divergence.

The whole repo field shrinks accordingly: `369x382` under `--wrap-mode down`,
`255x326` under `--wrap-mode back` (`--repo-scan-only`, same three files).

## The FFI seam, after a failure this change caused

`--repo-verify` failed on this branch in the DEFAULT mode and nothing in
`check-all.sh` ran it. Both halves of that sentence needed fixing.

### Root cause: an arity change in a positional argument list, plus a stale dylib

`glyph_engine_load_item` took its twenty params **positionally**. Adding `wrap_mode`
between `wrap_width` and `has_page` shifts every argument after it by a register on
the caller's side. A binary built against the new signature, linked against a dylib
built from the old source, therefore hands `wrap_mode` to `has_page`, `has_page` to
`page_rows`, and so on — so `has_page` arrives as 0 and **pagination silently switches
off**, while the batched path, which marshals a 128 B descriptor block rather than
registers, stays correct. That asymmetry is exactly what `--repo-verify` reported:

    repo-verify FAIL: item 2 placement differs
      per-item: page { right: 19.07, bottom:  -373.75 }   <- unpaginated
      batched:  page { right: 64.15, bottom:  -158.75 }   <- correct

**`cargo build` does not build the dylib.** `native/build.rs` links whatever
`pixi run build-engine` last produced, which is why `check-all`'s gate 0 rebuilds it
first. So a stale dylib is a normal state of this tree, not an exotic one, and an
ad-hoc `cargo build && ./glyph3d-native ...` reaches it easily.

Reproduced deliberately (build `ffi.mojo` at its pre-`wrap_mode` arity, leave the Rust
side alone) and it matches the report exactly — same item index, same shape — plus two
symptoms that were reported separately and turn out to be the same cause:

| symptom | stale dylib | rebuilt |
|---|---|---|
| `--repo-verify` | FAIL, item 2 placement differs | PASS, 407,451 records bit-exact |
| `--pick-col 22000` on `one-long-line.txt`, mode back | clamps to `col 99` | resolves `col 22000` |
| pick oracle's fold cross-check | **FAIL (43,901 of 44,001)** | PASS (0 mismatch) |

### The fix: one marshalling format, so the failure class is gone

Detecting this turned out to be harder than removing it, and **two guards were written,
measured useless, and thrown away** before the third — that is the part worth keeping:

1. A separate `glyph_engine_abi_probe()` returning a packed constant. **Green under the
   stale dylib**: its numbers were hand-written declarations, not consequences of the
   signature, so deleting a parameter left it still reporting the old shape. A guard
   built on a declaration is worth what the declaration is worth.
2. The same shape word as the **last positional parameter**, on the theory that an arity
   change would misalign it. **Also green, measured** — instrumenting the stale callee
   printed `abi_shape got 268896010 want 268896010` while `has_page` got 0 and
   `page_rows` got 1. The shift was consumed among the register-passed ints in the
   *middle* of the list and never reached the tail. A positional list cannot
   self-describe: a sentinel is either before the insertion point, where it never moves,
   or after it, where it may not either.

So the per-item entry stopped being positional. `glyph_engine_load_item_desc`
(`engine/ffi.mojo:203`) takes the **same 128 B descriptor** the batched entry takes,
both deserialize through one `_item_from_desc` (`engine/ffi.mojo:160`), and Rust's
`Engine::load_item` (`native/src/engine.rs:252`) marshals it with the same
`write_item_desc` the batched path uses. Consequences:

- A new item field takes descriptor **pad**; it can never shift an argument again.
- Two readers of the item params became one, and two readers is precisely how the
  strategies get to disagree about a field.
- Both strategies now exercise `write_item_desc`, so `--repo-verify` cross-checks the
  descriptor round-trip as well as the fold.
- The block carries `ABI_SHAPE` at a **fixed offset** (108, the remaining pad), which
  cannot shift by construction. A dylib expecting a different descriptor shape refuses
  the call with `GE_ABI_MISMATCH` and the remedy printed:
  *"libglyph_engine.dylib expects a different descriptor shape than this binary writes
  (shape word 0x8007 at offset 108). … Rebuild: pixi run build-engine"* — measured by
  building the dylib with `ABI_DESC_I32S` at 6.
- The entry point was **renamed**, so a dylib predating this change fails to **link**:
  `Undefined symbols for architecture arm64`. A symbol that does not exist cannot be
  called wrong. That is the strongest available outcome for the case that actually bit.

`engine/ffi_selftest.mojo` builds its own descriptor now (`_desc_for`), deliberately as
a third writer of the block: it exists to call the FFI as a foreign caller would, and a
foreign caller marshals its own. It is **neither run nor compiled by `engine/check.sh`**
— a third check nothing consults, noted here rather than fixed.

### The gate: `--repo-verify` now runs, in both modes

New **gate 8b** in `tools/check-all.sh`, on `fixtures/g-pick-repo` (5 files, 0.4 MB,
407,451 records), `--wrap-mode down` and `--wrap-mode back`. **Measured cost: 0.04 s per
mode**, so the full gated fixture is affordable and there is no case for a smaller input.

Both modes, because the two paths could differ about `wrap_mode` specifically and mode A
would never show it.

Mutation-tested like the rest — `has_page` forced false in the per-item entry only,
after the shared read, modelling exactly the class the gate exists for:

| # | mutation | result |
|---|---|---|
| M21 | per-item entry zeroes `has_page`; batched path untouched | RED — gate 8b FAILS in **both** modes, "item 2 placement differs" |
| M22 | dylib expects `ABI_DESC_I32S` 6, binary writes 7 | RED — `GE_ABI_MISMATCH` at the seam, naming the remedy |
| M23 | dylib predates `glyph_engine_load_item_desc` | RED — **link failure**, `Undefined symbols for architecture arm64` |

Edit landed and reverted-then-rebuilt for each, as with M1–M19.

**Why nothing caught it originally**, which matters as much as the fix: `--repo-verify`
was used as a mutation TARGET (M7) and never run clean on this branch. A mutation
reddening a check proves the check works; it does not prove the check was consulted.
That is the same family as the two holes closed here earlier — `fixture_census` and
`fixture_manifest` were made instruments that *run* for exactly this reason, and
`engine/check.sh`'s header already said an instrument nothing runs is an absent one.

## Gates

`tools/check-all.sh`: **ALL GATES GREEN**, including gate 1b (25 fixtures deleted and
rebuilt byte-identically), the new gate 8b (`--repo-verify` bit-exact in both wrap
modes) and the four byte-equal screenshots. No baseline moved.
