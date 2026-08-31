# Stage G — picking & live manipulation

**Goal**: the last hard requirement from the original brief — resolve a click
(or a scripted pick) all the way down to file → row/col → byte offset → the
actual character, and manipulate instances (color/position/scale) and file
groups (move/scale/tint/hide) live, with every feature scriptable headlessly.

**Result**: done and verified. CPU-side picking resolves to the exact
character with an independent-oracle gate (17 scripted picks + 2 pixel-ray
round trips, all PASS). Nine manipulation verbs write through to the GPU with
**partial uploads only** (4 B single-glyph recolor … 1.2 KB line recolor,
80 B per group edit — never a buffer re-upload). No-edit renders remain
**pixel-identical** to Stage F/E2 on all three benchmark views, and the
windowed smoke run with edits applied at startup holds **97–116 fps** (Stage F
baseline: 94–120).

All numbers on glyph3d-js (1,305 files / 96.9 MB / 95,181,245 glyph
instances, 2 arena chunks), release build, Apple M2, 1600×1000.

## Pick design: CPU ray → glyph (option (a), chosen over the GPU ID pass)

The web app does a GPU ID pass; we deliberately do **not**. Reasons:

1. **Everything needed is already CPU-side and exact.** A picked file's glyph
   geometry is re-derivable bit-identically: `repo::rederive_records` re-runs
   the engine on that ONE file with the exact `ItemParams` it was staged with
   (carried in the new `FileView.item` / `PickFileInfo.item`), which is the
   same determinism discipline `--repo-verify` proves globally.
2. **The Stage F segment table already provides per-file world AABBs**, so the
   file-level hit test is ~1.3k ray-AABB slab tests ≈ microseconds.
3. A GPU ID pass would need another render target, a readback round-trip, and
   would re-enter the wgpu-30/Metal indirect-draw morass documented in the
   Stage F report — for zero accuracy gain at this scale.

Resolution pipeline (`glyph_scene.rs`):

```
screen px ──inverse view-proj──▶ world ray
  ──slab test vs live per-file AABBs (group TRS applied, hidden skipped)──▶ file
  ──ray ∩ file plane (z = group offset.z; group quats are identity)──▶ local point
  ──nearest record cell rect [x, x+adv] × [y−h/2, y+h/2], accept ≤ 0.8 world──▶ record
  ──record == UTF-8 leader index──▶ folded (row, col) from the engine lanes
  ──text::fold_leaders over the file bytes──▶ source line, byte offset, char
```

`fold_leaders` replicates the engine's fold exactly (COL = raw leader count
per source line, ROW = base_row + col//wrap, newline rides at col == line
length — from `glyph_pipeline.mojo` THE FOLD). Every cache fill **cross-checks
the CPU fold's (row, col) against the engine's ROW/COL lanes for every record
of the file** and logs PASS/FAIL — 0 mismatches on every file picked during
verification (fixture + glyph3d-js). Char resolution includes blanks and
newlines (a click on whitespace still resolves; such hits just have no arena
slot). Picking through a moved/scaled group stays correct because the ray test
reads the LIVE group TRS from the CPU mirror of the group table.

Cost: one engine re-run per newly picked file, cached one entry deep —
measured 0.3–1.0 ms for small files (fixture, liveTrie.js: 3,673 records in
999.6 µs); a 10 MiB monster would be ~200 ms worst case, once per file.

## Manipulation verbs (all partial uploads)

| verb | effect | GPU write |
|---|---|---|
| `recolor-glyph [rrggbb]` | picked glyph's packed color | 4 B at slot×48+24 |
| `recolor-line [rrggbb]` | every glyph on the picked folded row | ONE contiguous range write per run — the full 48 B records are **rebuilt** from the pick cache (26 glyphs = 1,248 B) |
| `nudge-glyph dx dy [dz]` | picked glyph's local position | 12 B at slot×48 |
| `scale-glyph f` | picked glyph's advance & height | 8 B at slot×48+32 |
| `move-group dx dy dz` | file group offset | 80 B group row |
| `scale-group s` | file group scale (clamp [0.001, 100]) | 80 B group row |
| `tint-group rrggbb` / `tint-cycle` | group color tint (palette cycle) | 80 B group row |
| `hide-group` / `show-group` / `toggle-hidden` | group alpha 0 ↔ 1 (shader culls ≤ 0.01) AND the CPU cull skips the segment (no draws, no backdrop) | 80 B group row |

Two design notes:

- **The strided-color bug (found by readback, fixed).** The first recolor-line
  wrote the run's colors as one contiguous byte range — but the color field is
  strided 48 B apart, so 24 × 4 B of colors stomped the neighboring
  pos/glyph_id/row/col fields. Nothing visible changed, which a
  `GLYPH_G_DUMP=<slot>` GPU readback then proved (colors had landed across
  word boundaries). The fix rebuilds the full instance records for the slot
  run from the cache — one write, all fields exact. nudge/scale-glyph edits
  are tracked in a `geom_overrides` map so a later recolor-line rebuild
  preserves them.
- **Segment sync on group edits** (`sync_segment`): the cull segment's world
  AABB is re-derived from the live TRS (local AABB × scale + offset), the
  far-LOD backdrop tint is scaled by pow(new)/pow(orig) of the group color so
  untouched segments keep their Stage F-fitted tint bit-exactly, and hidden
  segments are skipped by `cull_segments` outright. Without this, a moved or
  hidden file would cull/stale-backdrop at its OLD position.

## Interaction surface (windowed)

Left click and look-grab conflicted in Stage F, so the buttons are split
(documented in the help line and window title output):

- **LEFT click** = pick the glyph under the cursor → prints
  `pick: file group=… rec=… row=… col=… line=… byte=… char='…' slot=…` and
  flash-highlights it (bright yellow; the previous flash is restored on the
  next click — the original color is known because the pick cache carries the
  staged per-record colors).
- **RIGHT press-and-drag** = mouse-look (pointer confined+hidden while held,
  raw DeviceEvent deltas); Esc also releases. Fly keys unchanged: WASD move,
  E|R up, Q|F down, scroll = speed.
- **Verb keys on the last pick**: `h` highlight line, `g` grab/release the
  picked file (mouse drags it in the view plane through its AABB center —
  ray∩plane deltas; scroll scales it ×1.1/notch), `t` cycle the dir-tint
  palette, `x` toggle hidden.

## Offscreen scripting (how everything above was verified)

One interleaved op stream in CLI order (repeatable; `--pick-row/--pick-col`
upgrade the most recent `--pick-file` to a deterministic glyph pick):

```
--pick-file SUBSTR [--pick-row N] [--pick-col M]   deterministic file/glyph pick
--pick-px X Y                                      ray pick through a physical pixel
--verb "recolor-line ffd54f" | "move-group dx dy dz" | ...   applied to the last pick
```

composing with `--screenshot`, `--focus-file`, `--zoom`, `--frames` as usual.
The same ops also apply at windowed startup (used for the smoke run).

## Verification

- `cargo build --release` — clean, 0 warnings.
- `--repo-verify` — **PASS**: 96,860,762 records bit-exact (no engine changes).
- **No-edit pixel identity** (final binary, `cmp` byte-equal): zoom
  (`out/g-zoom-noedits2.png` == `out/e2-file-zoom.png`), mid
  (`out/g-mid-noedits2.png` == `out/f-mid-before.png`), full field with
  backdrops (`out/g-field-noedits.png` == `out/f-field-after.png`).
- **Pick correctness** — `tools/check-stage-g.sh` (ALL PASS):
  - 10 fixture-repo picks (`native/fixtures/g-pick-repo/`) asserted against
    `tools/g_pick_oracle.py`, an INDEPENDENT python fold replica reading the
    raw bytes: row 0 col 0 of `alpha.rs` → `'u'` at byte 0; wrap fold
    (`wide.txt` row 1 col 100 → byte 100, the fold boundary); 2-byte UTF-8
    (`sub/deep.py` row 0 col 5 → `'é'` at byte 5); pagination (`long.md`
    row 200 col 5 → byte 7205, page 1); blanks (space/newline resolve with
    `slot=-`).
  - 5 glyph3d-js picks across files/depths, same oracle gate (e.g.
    `GlyphField.js` row 12 col 7 → `'a'` at byte 668).
  - 2 **pixel-pick round trips**: row/col pick → analytic camera math →
    `--pick-px` at that pixel resolves the SAME record (alpha.rs rec 50,
    long.md rec 7205) — the ray path is exact.
  - Every picked file's fold cross-check logged PASS (0 mismatches).
- **Screenshot proofs** (viewed): `out/g-recolor-line.png` — row 40 of
  liveTrie.js recolored yellow (560 px band at (556,539)-(671,548)) plus one
  glyph recolored red, scaled ×2.5, nudged (visible oversized red `p`);
  `out/g-move-scale.png` — liveTrie.js moved (-30,+12), scaled 0.5, tinted
  red (the displaced red block); `out/g-hidden-gap.png` — same view with the
  file hidden: the block is gone, leaving a gap. Fixture close-up:
  `out/g-fixture-recolor.png` (a whole `println!` line in yellow).
- **Perf sanity**: windowed smoke, 15 s, edits applied at startup (recolor +
  move + scale + hide): FPS lines 97.7 / 116.3 / (one 2 s windowing dip to
  5.0) / 61.0 — within Stage F's 94–120 baseline band; edits are one-off
  partial uploads and cost nothing per frame. No panics, no wgpu errors.
  Offscreen full-field steady state after no-op edits path: unchanged
  (60-frame run ≈ 29 fps cold-start readback-bound as in Stage F's 120-frame
  figure; warm behavior unchanged since the draw path is untouched).

## Memory

Added CPU state: group-table mirror (1,305 × 80 B = 102 KB), pick table
(~1,305 × small), cull side-tables (4 × ~50 KB), one-entry pick cache (one
file's records + byte walks; ≤ ~84 MB for a 10 MiB monster, typically <1 MB).
No instance-sized buffers added; buffers gained COPY_DST/COPY_SRC usages only.

## Remaining gaps (after Stage G)

1. **Selection ranges** — picks resolve one record; recolor covers one glyph
   or one folded row. A drag-selection (anchor + extend) is a natural next
   step: the pick cache already exposes per-record slots, so a range is a
   contiguous-run recolor over two picks.
2. **Editing the text itself** — the arena is fixed-size; insert/delete needs
   engine re-run + arena relayout of the file's slot range (all downstream
   files' slot bases shift — or leave a gap/append strategy). Not attempted.
3. **Emoji atlas** — bitmap slots still discard (unchanged since E1).
4. **IME / text input** — no input path beyond the verb keys.
5. **Group rotation** — the quat lane stays identity; `sync_segment` and the
   pick ray assume it (documented at both sites). Clip lanes likewise unused.
6. **Flash is sticky** — the click flash restores on the NEXT click, not on a
   timer (tick() has no queue access; a timed restore needs a ctx-carrying
   tick or a dirty-flag applied in render).
7. **Monster-file pick cost** — the per-file record walk is O(file records)
   with no spatial index; fine to ~10 ms for MB-scale files, ~200 ms at the
   10 MiB cap. A per-file row index would make it O(row).
8. **Backdrop tint on initial dir tints** — Stage F fitted backdrops WITHOUT
   the group tint; tint edits scale relative to that baseline. Closing this
   means refitting BACKDROP_GAIN with group tints applied — cosmetic, far-LOD
   only.

## File diffs

- `native/src/glyph_scene.rs` — Stage G module-header contract; PickFileInfo /
  PickContext / PickCommand / Verb / PickHit / PickGlyph / PickCacheEntry;
  CPU pick (pixel_ray, ray_aabb, ray_file, pick_ray, pick_row_col,
  ensure_pick_cache with the fold cross-check); verb application with partial
  uploads (write_instance / write_group_row / sync_segment / geom_overrides);
  click flash; view-plane grab drag; scroll-scale; verb keys; CullState
  side-tables (local AABB, base tint, orig group rgb, hidden); cull_segments
  hidden skip; instance/group buffers + COPY_DST (+COPY_SRC for the debug
  readback); debug_dump_instances; viewport tracking.
- `native/src/repo.rs` — FileView carries the exact per-file ItemParams;
  RepoLoad carries root/trie; into_staged builds the PickContext (same AABB
  margins as the cull segments); `rederive_records` (deterministic per-file
  engine re-run); DIR_TINTS now pub; focus log prints page dims at {:.3}.
- `native/src/text.rs` — StagedText.pick field; `fold_leaders` (CPU replica
  of the engine fold for char resolution + cross-check).
- `native/src/scene.rs` — SceneLike hooks take &GpuContext; new on_cursor /
  on_click / set_viewport / apply_pick / apply_verb / debug_dump_instances
  (default no-ops).
- `native/src/windowed.rs` — right-drag look grab, left-click pick, cursor
  tracking, verb keys, startup op application, help line.
- `native/src/offscreen.rs` — interleaved op stream before frame 0;
  GLYPH_G_DUMP readback debug.
- `native/src/main.rs` — `--pick-file/--pick-row/--pick-col/--pick-px/--verb`
  parsing (op stream), help text, mode wiring.
- `tools/check-stage-g.sh`, `tools/g_pick_oracle.py` — NEW: the pick
  correctness gate (independent fold oracle, pixel round trips).
- `native/fixtures/g-pick-repo/` — NEW: tiny deterministic pick fixture repo
  (wrap fold, 2-byte UTF-8, 300-row pagination cases).
