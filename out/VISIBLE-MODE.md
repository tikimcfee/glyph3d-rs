# Visible-set field mode — how to see it

The third glyph field mode (`--field-mode visible`, beside `instanced` and
`derived`): no slot per glyph is kept; the source bytes, a line table and the
atlas are resident on the GPU, and each frame lays out only the lines in view.
The design and its measurements are `out/GPU-DIRECTION-2026-10-09.md`; this
file is the running "how to see it" for each milestone, updated as they land.
Every command below runs from the repo root against a release build
(`cargo build --release --workspace`).

## M1 — Pass 1 emits the line table (2026-10-10)

What landed: `native/src/layout_hyper/line_table.rs`. Pass 1's per-chunk walk
collects one `LineEntry` per line (`byte_start, item, base_row, glyph_count`,
16 B) and a `SegmentSeed` at every cut of a long line (`line, byte_offset,
col, seg_adv, cells`, 24 B), cuts planned every `SEGMENT_BYTES` (2048) on the
chunk rule — before an ASCII byte only. The aggregation assembles the table
and replays the seeds of lines a chunk cut split. Nothing reads it yet; it is
built only when asked (`pass1_over_chunks_with_lines(.., Some(segment_bytes))`),
so every other mode's Pass 1 is unchanged.

What the tests assert (`cargo test --release -p glyph3d-native line_table`):

- `line_table_agrees_with_the_fold`: over the 26 pipe fixtures, `cubecl-fork`,
  `g-cluster-repo`, `g-pick-repo`, `overflow-leads.txt`, `chunk-cut.txt`,
  `chunk-cut-paint.txt` and `emoji-corpus-small.txt`, in both cluster modes
  (96 items, 12,620 lines): every entry's byte start, glyph count and base row
  equal the oracle-backed fold's (`hyper_oracle::reference_item`), the base
  rows are the running sum of `fold::rows_for_line` with the aggregation's
  one exception (an unterminated tail with no leader takes no row), and the
  sum equals the item's `row_count`.
- `seeds_obey_the_cut_rule_and_the_fold_state`: with 32-byte segments so every
  corpus line carries cuts, under wrap 0 / 7 / 100 (20,000+ seeds): every cut
  is before an ASCII byte, no resolved sequence spans a cut (100+ sequences
  precede one), and a serial replay of the line to the cut gives the seed's
  column, its `seg_adv` to the bit, and a `cells` whose product with the cell
  advance IS the fold's f64 line advance and narrows to the same f32.
- `every_trie_advance_is_whole_cells`: over all 0x110000 codepoints the trie's
  advance is 0, 1 or 2 cells and the bitmap advance is two — what makes
  `cells` exact.

To see the table itself:

```sh
target/release/glyph3d-native --line-table-stats native/fixtures/chunk-cut.txt native/fixtures/g-pick-repo
target/release/glyph3d-native --line-table-stats <any repo dir>      # e.g. a crates.io tree, a Linux tree
target/release/glyph3d-native --line-table-stats engine/fixtures/wrapback-long-line.pipe.bin --cluster-mode leader
```

It prints, per corpus, items / bytes / lines / glyphs / rows / seeds and the
longest line, the table's size, and the first entries and seeds of the first
seeded item. On the 93 MB crates tree (2026-10-10): 7,141 items, 2,660,879
lines (34.9 B/line), 90,234,603 glyphs — the renderer's own instance count —
559 seeds on 33 lines (longest 540,720 B), a 42.6 MB table built in 72 ms on
32 threads. The glyph and row totals are the cross-check against a normal
`--load-repo` of the same tree (`repo: … engine records -> N glyph instances`).

Not visible yet: nothing draws from the table until M2.
