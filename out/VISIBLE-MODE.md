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

## Between M1 and M2 — two Derived-mode fixes the visible mode inherits (2026-10-10)

The visible mode draws through the Derived shader, so two latent defects in
its vertex stage were fixed first, each with the witness it lacked:

- **The column page** (`fix(derived): the vertex stage carries the column page`):
  z dropped `fold::paginate`'s `x_page × depth_per_col`. The slot's row lane
  is now `row:24 | x_page:8` (`crates/glyph-field-derived/src/derive.rs`) and
  the shader adds the term. Witness:
  `cargo test --release -p glyph3d-native derived_vertex_stage` — a Rust
  transcription of `derive_yz` held to the Instanced emitter's y/z over the
  pagination grid (4,048 slots, 1,064 on a later column page, max error
  2.4e-7); mutation `derived-column-page-term-dropped`.
- **The item/group lane** (`fix(derived): the group lane keeps the item`):
  one word was read as both item and group, so a group verb
  (`set-glyph-background`, `set-glyph-transform`) made the vertex stage derive
  Y/Z from another item. The lane is now `item:20 | override:12`; the group is
  the item's own (`ItemParamsGpu::group`) unless an override index names a
  row in a resident table (binding 10). `GlyphField::write_group_id` takes
  the item. Witness: `cargo test --release -p glyph-field-derived`
  (`group_lane_resolves_to_the_item_or_its_override`, the shader-text pins);
  mutation `derived-override-lane-unshifted`.

To see the second one in the app (windowed, any repo, `--field-mode derived`):
pick a glyph and run `set-glyph-background` or `set-glyph-transform` from the
verb panel — before the fix the glyph jumped to another file's rows (or
vanished, out of the item table); now it stays put and takes the new group.
Neither fix moves a golden pixel: no golden view is column-paged and every
load-time lane is the same word as before.

## M2 — how to see it (2026-10-10)

The mode runs end to end: the renderer's load path below hands
`crates/glyph-field-visible` its inputs, the field culls and lays out the
visible lines on the GPU each frame and draws them through the Derived
shader, and the sixth hyper-oracle tier (next section) holds the kernel to
HyperLayout slot for slot. Everything here stays inert for `instanced` and
`derived` (the full battery is green on both; every golden byte-equal). On
the 93 MB crates tree (2026-10-10, this box): backend 11-22 ms where the
stored modes wrote a 1.8 GB slot stream for ~460 ms; offscreen frames of the
default view at ~0.7 ms. Not yet in this mode: syntax colour (by decision —
colour arrives as byte-range spans), selection and the slot verbs (M3), the
golden equivalence (M4).

**What runs when `visible` is selected.** `repo::prefetch_repo` runs Pass 1
in the background WITH the line table (`prefetch_hyper(.., field_mode)`);
`HyperLayout::layout_items` takes that table (or runs Pass 1 again with
lines if the prefetch was for another mode), checks the Derived lane limits
once, runs NO Pass 2, measures each item's placement with
`compute_single_item_placement` in parallel, and returns the arena in its
third form — `GlyphArena::from_visible(VisibleStaging)`: the table, one seed
per item (params, group, `max_row_extent`, slot and byte counts) and no
bytes yet. `load_repo_from_prefetched` builds the file views, then MOVES the
walk's `RepoFile.bytes` Vecs into the staging (one allocation per file; a
1.4 GB tree is not copied). `into_staged` finishes each seed into a
`VisibleItem` with what only the shelf knows — the group row and the world
box the scene culls (the segment's, group offset applied) — with
`stride_x = max_row_extent + page_gap_x` for row-paged items, `byte_base`
the running sum in item order, `first_line/line_count` from the table, no
spans (`default_color = DEFAULT_COLOR_PACKED`). `glyph_scene::setup` casts
the table's records to the kernel's (`LineEntry`/`SegmentSeed` are the GPU
records lane for lane — a test pins it), builds the `TrieUpload` from the
atlas trie (`layout_hyper::visible::trie_upload`: the raw `codepoints.bin`
sections, the sequence table, a 0x110000-bit first-member bitmap, the
metrics) and calls `VisibleField::new`. Per frame, `render.rs` calls
`field.prepare(..)` with the camera, viewport, `px_scale`, the two LOD
thresholds (glyph tier = `[lod] min_px`, backdrop tier = `[lod]
visible_backdrop_px`, both live from the panel), `greek_mode`, the debug
tint and time; the CPU cull runs only for the BACKDROPS (under the backdrop
threshold) and the hidden flags; the glyph phase is the field's own draw
plus `record_wash_draw`. Selection and the slot verbs draw/apply nothing in
this mode (logged once / reported) until M3 re-keys them by byte range.

**Commands** (repo root, release build, GPU):

```sh
# The CLI door. Loads g-pick-repo in visible mode with the HUD open.
target/release/glyph3d-native --load-repo native/fixtures/g-pick-repo --field-mode visible

# The same with the debug tint (glyphs coloured by LOD tier, or by cull state).
target/release/glyph3d-native --load-repo native/fixtures/g-pick-repo --field-mode visible --debug-tint lod
target/release/glyph3d-native --load-repo native/fixtures/g-pick-repo --field-mode visible --debug-tint cull

# The panel door: launch in any mode, F1, "glyph field mode" → "Visible (no slots)".
# The scene rebuilds through the same arm the other selectors use; the
# highlighted label is the mode actually BUILT (a lane-limit fallback shows
# as Instanced). The "visible field" block under cull/LOD has the backdrop
# px/em slider and the debug tint selector.
target/release/glyph3d-native --load-repo native/fixtures/g-pick-repo --field-mode derived

# The TUI door: cargo glyph run → "3. Graphics & Shading" → Field Mode, ◄/►
# cycles instanced → derived → visible (exp); launch_config.toml's
# field_mode = "visible" seeds it.
cargo glyph run

# The HUD: F8 toggles it (open by default in visible mode). Top-left: mode,
# engine, segments/hidden, backdrops, CPU cull ms, fps; in visible mode also
# items and lines per tier, segments, slots (and dropped), GPU cull/layout/
# draw ms from VisibleField::stats(); stored modes show instances and ranges.

# A reproducible view: fly somewhere, F1 → "copy pose" puts
# `--cam-pose X Y Z YAW PITCH` (degrees) on the clipboard and prints it; or
# press F2 with GLYPH_POSE_PRINT=1 to print it with the screenshot. Feed it
# back to a golden-style command:
GLYPH_POSE_PRINT=1 target/release/glyph3d-native --load-repo native/fixtures/g-pick-repo --field-mode visible
target/release/glyph3d-native --load-repo native/fixtures/g-pick-repo --field-mode visible \
    --cam-pose 12.0 -3.0 40.0 0.0 -10.0 --screenshot out/visible-check.png

# The self-test: cycles Instanced → Derived → Visible at t≈3 s through the
# relayout arm and prints the HUD line after each.
GLYPH_FIELDMODE_SELFTEST=1 target/release/glyph3d-native --load-repo native/fixtures/g-pick-repo

# No GPU: the staging alone (Pass 1 with lines, the placements, the counts).
target/release/glyph3d-native --load-repo native/fixtures/g-pick-repo --field-mode visible --repo-scan-only
```

**What the tests assert** (`cargo test --release -p glyph3d-native --lib -- visible lane_limits max_line_cols`):

- `placements_agree_with_the_derived_device_pass2`: over `g-pick-repo`,
  `cubecl-fork`, `g-cluster-repo`, `chunk-cut.txt` and `chunk-cut-paint.txt`
  in both cluster modes (24 items), the single-item placement the visible
  load uses is bit-equal to the device Pass 2's — slot base and count,
  record count, page and ink extents. It found one defect on arrival: the
  ASCII shortcut of `compute_single_item_placement` left the newline's own
  record out of the page extent (one cell short on every ASCII line; no
  caller had ever read it). Fixed in the same change.
- `a_visible_load_stages_the_table_and_the_seeds`: a device-less visible
  load returns the third arena form, one seed per item, a table whose glyph
  counts sum to the arena's, and the device's placements; the same engine
  asked for `instanced` stages host records as before.
- `visible_records_are_the_kernels`, `staging_items_carry_bases_lines_and_stride`,
  `trie_upload_bitmap_matches_starts_a_sequence`: the casts, the item
  finishing, and the upload's bitmap against the table's own
  `starts_a_sequence` over every codepoint.
- `derived_lane_limits_refuse_past_each_lane_and_name_the_item`,
  `effective_field_mode_falls_back_to_instanced_past_the_lanes`,
  `max_line_cols_is_the_widest_line_in_leaders`: the release-time lane
  check (items / rows / column pages, boundaries included), the fallback
  decision, and the new Pass 1 figure it reads, held to the oracle's records
  over corpora with intra-line chunk cuts.

Not visible yet: the pixels. They arrive with the crate's `VisibleField`
bodies; the first thing to look at then is the HUD's tier counts against the
`--debug-tint lod` colours, and `repo-wide`'s camera (`cargo glyph graph`
lists the golden commands) under `--field-mode visible` beside the Derived
frame.

### The witness behind M2: the sixth hyper-oracle tier

`hyper-oracle` (gate, `--hyper-oracle-check`) now runs the visible field's
GPU layout kernel headless over EVERY line of every corpus item
(`glyph_field_visible::layout_all_lines`: Pass 1's line table and segment
seeds, the kernel's own trie lookup and cluster-mode sequence pass, no cull)
and compares its `DerivedSlot`s byte for byte, in slot order, with the device
Pass 2's Derived emission of the same bytes — the same record from the same
bytes, the GPU's fold against HyperLayout's, which the five CPU tiers hold to
the JS oracle. It needs a GPU (`GLYPH_HYPER_ORACLE_NO_GPU=1` skips it; the
gate's STRICT run refuses a skipped tier) and refuses zero slots. To see it:

```sh
cd native && GLYPH_TRACE=warn ../target/release/glyph3d-native --hyper-oracle-check fixtures/g-pick-repo fixtures/cubecl-fork ../engine/fixtures/cluster-keycap.pipe.bin ../engine/fixtures/paged-rows.pipe.bin
```

Every PASS line ends `visible kernel N slots`; the first divergence, if any,
names the item, the slot and both slots' lanes. Mutation
`visible-keycap-lookahead-dropped` (the kernel's keycap guard removed)
reddens it through this tier. The grid test
`hyper_oracle::tests::pagination_agrees_on_every_tier` runs the tier too, over
every paging shape.

