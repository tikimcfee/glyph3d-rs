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

## M3 — how to see it (2026-10-10)

**The problem M3 solves.** In visible mode the slot buffer is transient and
its order is set by atomics each frame, so everything the scene keyed by SLOT
— the selection mask, the glyph verbs, `GLYPH_G_DUMP` — named nothing there
(M2 left them "reported, not applied"). M3 re-keys them by **(item, byte)**:
the item is the file's index in the load (its group id at load,
`GlyphScene::visible_item_of`) and the byte is the glyph's LEADER offset
within the file — both already what the CPU pick yields (`PickFileInfo::group_id`,
`PickGlyph::byte_off`). The stored modes keep their slot paths untouched (the
goldens, and the goldens under `--field-mode derived`, are the proof).

**How each consumer is keyed now** (renderer side; the field's bodies are the
crate's, `crates/glyph-field-visible`):

| Consumer | Stored modes (unchanged) | Visible (M3) |
|---|---|---|
| Selection (`glyph_scene/target.rs`) | `Selection::Glyph{chunk,local}` / `Segment{slot_base,slot_count}` | `Selection::ByteRange{item,start,end}`: a glyph → `[byte, byte+utf8 len)`; a file-level or blank pick → `[0, byte_len)` (`pick::byte_range_selection`) |
| Mask pass (`render.rs`) | `record_draws` over the slot range | `prepare_mask(queue, encoder, item, start, end)` right after `prepare` (same encoder, before any pass; only when the mask pass will run — windowed), then `set_pipeline(mask)` + `record_mask_draw(pass)` |
| `recolor-glyph` | `write_color(slot)` | `set_glyph_override(GlyphOverride{item, byte, color, ..})` |
| `nudge-glyph` | `write_position(slot)` | override `x_nudge += dx` (y/z not representable — the reply says so, as in Derived) |
| `scale-glyph` | `write_extent(slot)` | not representable (advance/height come from the atlas table, as in Derived) — reply only |
| `set-glyph-background` / `set-glyph-transform` | group row allocated, `write_group_id(slot)` | group row allocated as before, then override `group = new row` |
| `reset-glyph-group` | `write_group_id(slot, own)` | override `group = NO_GROUP`; an override left with nothing in it is CLEARED (`clear_glyph_override`) |
| `recolor-line` | rebuilt placements over the row's slots | `set_item_span_range(item, start, end, color)` over the row's byte range (`pick::row_byte_range`, from the pick cache) |
| `hide-group` / `show-group` | group alpha + CPU cull flag | the same, plus `set_item_hidden(item, hide)` — the HUD's items/lines tiers move |
| group TRS edits (move/scale/tint, grab drag, carrel moves — everything that reaches `sync_segment`) | the SegCull box recomputed | the same, plus `set_item_bbox(item, min, max)` with the recomputed box (`--no-cull`: from the pick AABB) |
| `--highlight` / seam `apply_surface_updates` (`style.rs`) | per-slot placements | `set_item_spans(item, spans)` — whole-item replace (M2 already) |
| `GLYPH_G_DUMP` (`offscreen.rs`) | `<slot>[,<len>]` | `<item>:<byte>[,<len>]` → `locate` in the last prepared frame → the 20 B slot |
| HUD (F8) | slot / slot range | `selection item:start..end` and `pick file byte N` |

The scene's `glyph_overrides: HashMap<(item, byte), GlyphOverride>` is the
source of truth for the per-glyph edits; every verb merges one lane into it
(`pick::merged_override`: a recolour after a nudge keeps the nudge) and pushes
the whole record to the field. Every Visible verb reply names the `item` and
`byte` it applied to.

**Commands** (repo root, release build, GPU). The reproducible door is the
CLI op stream offscreen — the same `apply_pick`/`apply_verb` the click and the
panel call:

```sh
# Pick a glyph and recolour it, visible mode, screenshot. The reply names the key:
#   verb recolor-glyph: alpha.rs item 0 byte 35 (row 3 col 8 'a') -> #e01010 (override)
target/release/glyph3d-native --load-repo native/fixtures/g-pick-repo --field-mode visible \
    --focus-file alpha.rs --zoom 3 --pick-file alpha.rs --pick-row 3 --pick-col 8 \
    --verb "recolor-glyph e01010" --screenshot out/visible-recolor.png

# The same in derived mode, for the eye's A/B (the witness below does it by pixel):
target/release/glyph3d-native --load-repo native/fixtures/g-pick-repo --field-mode derived \
    --focus-file alpha.rs --zoom 3 --pick-file alpha.rs --pick-row 3 --pick-col 8 \
    --verb "recolor-glyph e01010" --screenshot out/derived-recolor.png

# The row, the per-glyph group (background quad / transform), and the reset:
#   --verb "recolor-line 10e010"
#   --verb "set-glyph-background 1010e0"      (rrggbb[aa]; new CLI form, 2026-10-10)
#   --verb "set-glyph-transform 0 2 0 1.5"    (tx ty tz [s]; new)
#   --verb reset-glyph-group                  (new)
#   --verb "nudge-glyph 0.5 0 0"              (x only in visible/derived; y/z reported)
#   --verb hide-group / show-group            (also set_item_hidden)

# Where did that glyph land this frame? Locate it in the transient buffer:
GLYPH_G_DUMP=0:35 target/release/glyph3d-native --load-repo native/fixtures/g-pick-repo \
    --field-mode visible --focus-file alpha.rs --zoom 3 --screenshot out/visible-dump.png
#   GLYPH_G_DUMP item 0 byte 35 -> transient slot N: [x, row, glyph_and_wrap, color, item_and_group]

# The windowed self-test: pick (first file, row 0 col 0) then recolor-glyph,
# nudge-glyph, set-glyph-background, hide-group, show-group, one every fourth
# frame from t≈3 s, printing each reply and the HUD line after it.
GLYPH_VISIBLE_VERB_SELFTEST=1 target/release/glyph3d-native --load-repo native/fixtures/g-pick-repo --field-mode visible
GLYPH_VISIBLE_VERB_SELFTEST=wide.txt:2:10 target/release/glyph3d-native --load-repo native/fixtures/g-pick-repo --field-mode visible

# The panel door: launch visible, LEFT-click a glyph — the selection mask
# highlights it (the field lays the byte range out again into its mask buffer);
# click a file's empty margin — the whole item highlights; h recolours the row,
# x hides/shows the file (the HUD's items/lines tiers move), t cycles the tint,
# g grabs the file (drag moves it; the item box follows). F1's verb buttons are
# the same `--verb` literals. The F8 HUD shows `selection item:start..end` and
# `pick file byte N`.
target/release/glyph3d-native --load-repo native/fixtures/g-pick-repo --field-mode visible
```

**The witness** (`cargo test --release -p glyph3d-native --test visible_verbs`;
`native/tests/visible_verbs.rs`, in the cargo-test gate; mutation
`visible-verb-keyed-off-by-one-byte` proves it). It runs the release binary
as `pick-oracle` and the golden runner do — `g-pick-repo` from the `repo-zoom`
camera (alpha.rs, zoom 3), `--pick-file alpha.rs --pick-row 3 --pick-col 8`
(the `a` of `alpha`, byte 35), in `derived` and in `visible` mode, plain and
with one verb — and reads the PNGs. Five cases: `recolor-glyph e01010`,
`recolor-line 10e010`, `set-glyph-background 1010e0`, `hide-group`, and
`selection` (the frame with the pick against the frame without one: the
selection mask — offscreen frames carry the highlight, since the mask
machinery is built for both drivers). For each it asserts:

- the verbed frame differs from the plain frame of the same mode in a SMALL
  region only (a cell at this zoom is 134×145 px: a bounding box at most
  200×220 px and 20,000 changed pixels for a glyph, 200×320 and 60,000 for
  its em-tall background quad, a band at most 300 px tall and 250,000 pixels
  for the row; `hide-group` empties the file and is held only to coincide),
  and some changed pixel carries the verb's DOMINANT channel at least 60
  above the other two (40 for the alpha-blended background) — the shader
  blends the slot colour with the group tint, so `#e01010` renders as
  ≈ (255,167,95) where the plain cream is (255,249,204); what survives the
  blend is which channel leads;
- the set of pixels changed in visible mode coincides with the set changed in
  derived mode: symmetric difference at most max(8, 5 % of the larger set),
  bounding boxes within 2 px — the (item, byte) key addressed the glyph the
  slot did;
- the visible reply names `item` and `byte`.

Measured on this box (vulkan-nvidia, 2026-10-10) — every case coincides
EXACTLY, 0 px in one mask only, identical boxes:

| case | changed px (both modes) | box | colour (verbed, best pixel) | visible reply |
|---|---|---|---|---|
| `recolor-glyph e01010` | 9,203 | 134×145 at (183,615) | red leads by 100 (255,155,93) | `item 0 byte 35 (row 3 col 8 'a') -> #e01010 (override)` |
| `recolor-line 10e010` | 57,717 | 1396×247 at (183,565) | green leads by 202 (8,210,6) | `item 0 bytes 27..46 (row 3) -> #10e010 (span)` |
| `set-glyph-background 1010e0` | 38,858 | 158×299 at (167,537) | blue leads by 60 (179,167,239) | `item 0 byte 35 (row 3 col 8) -> group 5 (override)` |
| `hide-group` | 94,232 | 1548×808 at (31,192) | — | `group 0 (...; item 0 hidden=true)` |
| `selection` (pick, no verb) | 9,203 | 134×145 at (183,615) | — (the `[glyph_scene] selection_tint`) | `pick: alpha.rs group=0 rec=35 row=3 col=8 line=3 byte=35 char='a'` |

Unit tests beside it: `pick::visible_key_tests` (the selection a pick becomes,
the row range, the override merge), `offscreen::tests::g_dump_spec_parses_slots_and_item_bytes`,
`cli::tests::glyph_group_verb_forms`. The self-test above, run on this box:
the pick resolves `u` at byte 0, each reply names `item 0 byte 0`, and the HUD
after `hide-group` reads `segments 5 (1 hidden) | items 4/5 visible | lines
312 candidate: 312 glyph` (318 before — alpha.rs's six lines left every
tier), back to `5/5` and `318` after `show-group`; every HUD line ends
`| selection 0:0..1 | pick alpha.rs byte 0`. `GLYPH_G_DUMP=0:35` printed
`transient slot 19: [40879d21, 00000003, 00000042, ffd4d4d4, 00000000]` — the
same five words the Derived field's slot 32 holds for that glyph. The two
modes' recolour frames (`visible-recolor.png` vs `derived-recolor.png`,
the first two commands above) are byte-identical on this box: 0 px differ.

On the M2 (metal-apple, 2026-10-10, the same rev through the remote job):
the full battery green there too (15 of 15, 314 tests, every golden
byte-equal plain and under `--field-mode derived`); hyper-oracle's visible
tier 1,497,095 slots flat and 1,497,095 again under spans (641,641 coloured)
in the strict run, the same figures as here; the witness coincides EXACTLY
in every case there as well (0 px in one mask only, identical boxes), with
Metal inking two pixels fewer than Vulkan on the row band (57,715), the
background quad (38,856) and the hidden file (94,230) — isolated edge flips,
the cross-vendor noise the pixel gate's `drift` instrument names.
`GLYPH_G_DUMP=0:35` printed the same transient slot 19 and the same five
words, and the two modes' recolour frames were byte-identical there too.

What this cannot see: `set_item_bbox` (a wrong box is a culling error,
pixel-visible only when it culls the item — the HUD's items tier is the
readout; the `move-group` reply is the trace), `set-glyph-transform`'s group
lane beyond what the background quad shares with it, and `nudge-glyph` (x only;
the reply is the trace — the override merge is unit-tested).

