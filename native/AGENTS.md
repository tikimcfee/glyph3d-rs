# AGENTS.md — house rules for the `native/` crate

**Read the root `AGENTS.md` first.** It is canonical for everything repo-wide:
what each check compares and what it cannot see, what is fenced
and why, and what the stage/gate vocabulary means.

This file is the Rust crate: the layout seam, style discipline, debug env vars,
and the module contracts.

```bash
cargo glyph test        # runs the entire verification battery; exit 0 = all green
```

**Run in a WORKTREE if anyone else is working in this repo.** `cargo glyph test` reads
the WORKING TREE, not HEAD, so another thread's uncommitted edits fail your
checks and tell you nothing about your own change. That has already happened:

```bash
git worktree add .claude/worktrees/<name> -b worktree-<name>
cd .claude/worktrees/<name>
cargo glyph test
```


## The determinism chain (why the checks can be this strict)

Offscreen renders use a fixed virtual clock (1/60 s per frame), CPU-side
culling/picking (no GPU-dependent traversal order), and a fixed atlas. So a
given commit + given input ⇒ byte-identical PNG. That property is what makes the
pixel comparison meaningful, and it is a fact about this crate's
offscreen path.

The chain also needs every fragment's colour to be a function of that
fragment's own inputs, and a hardware derivative is not one on every GPU.
`fwidth`, and the implicit gradients of `textureSample`, are differences
across a 2x2 quad. On Apple's TBDR GPUs, in the glyph pass (blended, writes
depth, may discard), whether a quad-mate lane hidden behind a nearer glyph
was depth-rejected before or after shading varies with GPU timing, so the
winner's footprint did too. `repo-back-oblique` flickered at delta 1 on a
still camera, and frame 0 sat in a different state from the frames after it
(C35, 2026-10-24). On Apple TBDR the glyph shaders therefore take no
hardware derivative (`glyph_field::Footprint`, next section). A new
derivative in a blended, depth-writing pipeline needs the same thought:
today the glyph shaders are the only one. Environment writes `frag_depth`,
regions do not write depth, and meshes and backdrops take no derivative.

Everything that could break it is fenced, and **the fence table lives in root
`AGENTS.md`** — it used to be restated here with different reasons and a stale
claim (`engine/` as READ-ONLY, which it has not been since engine work moved
into this tree). One invariant worth carrying: any layout or rendering change
must leave every golden pixel baseline byte-equal. If a change moves a PNG,
the change is wrong.

## The layout seam

Everything that lays glyphs out goes through `native/src/layout.rs`
(the layout seam). Read that module header before adding a backend or a
caller; the short version:

- A backend takes `LayoutItem`s (bytes + `ItemParams` + `Paint` + group),
  appends instances to a caller-owned `GlyphArena`, and returns
  `ItemPlacement`s — a slot range, three counts, two extents. **No method on
  `LayoutGlyphs` returns a position.** That is the point: it is what lets a
  device-resident backend keep the glyphs on the device.
- A gate that needs the 32 B wire records asks `VerifyLayout`, a SEPARATE
  trait, so no method a caller holds RETURNS a position. That is what would let
  a device-resident backend keep glyphs on the device. **It does NOT mean the
  readback is gone** on the strategies that HAVE one — this file claimed
  otherwise until 2026-09-04 and was wrong, because `VerifyLayout` gates the
  API, not the copy. Do not widen `LayoutGlyphs` to return records.

  What DID delete the copy is the direct path: `HyperLayout::layout_items`
  writes instances straight into the caller's arena and materializes no 32 B
  wire record at all. The record path (`VerifyLayout`) remains as the
  verification form, and only `--hyper-oracle-check` takes it. Run
  `--repo-scan-only --repo-engine hyper|direct|batch|naive` for the per-stage
  split rather than trusting a figure here.
- `ItemParams::validate` runs in `LayoutGlyphs::layout_items`, a PROVIDED
  method. Implement `layout_validated_items`; a backend cannot forget the
  guard because it never calls it.
- Record compaction has one host reference, `layout::compact_records_into`
  (test-only today; the device Pass 2 emits compacted slots directly). It is
  the statement of what blanks, paint indexing and the extents mean, so a
  backend may differ about the FOLD — the thing the corpus checks — and is
  held to this for the rest.
- **Paint is indexed by RECORD, not by instance.** Compaction destroys the
  index that names a byte, so paint crosses the seam and is applied during
  compaction. Indexing it by instance is the tempting mistake and
  `layout::tests::paint_is_indexed_by_record_so_blanks_consume_an_entry`
  is what catches it.
- `layout::diff_backends` diffs two backends at the seam: placements and
  instances always, and records when BOTH have them. `--hyper-oracle-check`
  (gate `hyper-oracle`, `hyper_oracle.rs`) uses it to hold HyperLayout to the
  oracle-backed fold. (`--repo-verify`, which held HyperLayout to itself, was
  deleted on 2026-10-26: every `--repo-engine` is the same HyperLayout, and no
  mutation could make it fail.)
- **`HyperLayout` (`hyper`) is the DEFAULT layout engine** in pure Rust: a parallel,
  cache-blocked CPU layout engine using Rayon, intra-file line chunking, wrap-aware
  chunking for minified files, background pipelined prepasses, and aligned 8-burst /
  4-burst register slot emission. Emits compact 20-byte `DerivedSlot`s in
  `Derived` mode into HOST blocks that are the field's chunks (2026-10-26;
  `[glyph_scene] derived_block_slots`, `glyph_field::HostSlots`): the one host
  copy a makepad renderer publishes and the wgpu field uploads from, kept current
  by every slot write. (32-byte `RenderSlot`s for the Instanced mode went
  straight into mapped device memory or staged windows until that mode was
  retired, 2026-10-31.)
- **Paint is flat at load, colour arrives as spans** (2026-10-11). Every load
  paints `Paint::Flat`; per-glyph colour comes afterwards as byte spans (the
  `--highlight` sidecar and the seam's style runs: `style_file`). `Paint::PerRecord`
  and `Paint::ByteSpans` remain the layout's per-glyph forms. The lexical
  syntax heuristic — `Paint::SyntaxHeuristic`, `text::colorize_leaders`
  and the per-line device colouring with C17's whole-line colours at chunk
  cuts, `--color-mode` — was retired that day (Ivan: "it should be gone in
  general"); `--color-mode` is refused like any unknown flag, and a launch
  config's `color_mode = "flat"` still parses into nothing (`"syntax"` is
  refused, naming the retirement). The agent cards' comment-line guess in
  shown source went with it; their colours by event kind, result and diff
  line stay.
- `--repo-engine hyper|direct|batch|naive` selects the layout engine (the
  CubeCL compute engine, `cubecl`, was retired 2026-10-09):
  - `hyper` (default): parallel Rust CPU layout with sub-200ms visual init.
  - `direct` / `batch` / `naive`: the same `HyperLayout`, without `hyper`'s
    background prefetch; the stats line prints `batch`/`naive`'s
    readback/compaction phases.
- `--field-mode derived` selects the glyph field mode, of which one remains
  (`visible` was deleted on 2026-10-26, `out/history/VISIBLE-MODE.md`;
  `instanced` on 2026-10-31, and is refused):
  - `derived` (20 B per glyph; THE DEFAULT since 2026-10-11, Instanced
    retirement step 1 — no golden pixel moved): compact word layout, x stored,
    Y/Z derived in the vertex stage from the item table. Host-staged scenes
    (agent sessions; `--render-file` too until it became a one-item repo
    load, 2026-10-11) transcode their 48 B records against
    their own item table — the agent-session Carrel stages one item per text
    block (`agent_transcript::staging::block_item`) so derived y/z equal the
    absolute ones bit for bit. `nudge-glyph` y/z and `scale-glyph` have no
    lane here and reply "not representable" (x nudges apply).
  - (`instanced`, 32 B per glyph with precomputed 3D coordinates, held
    byte-equal to every golden by the `--field-mode instanced` equivalent
    from 2026-10-11, was retired on 2026-10-31: step 3 deleted its crate.)
  - **The Derived lanes are a load-time limit, per file** (`layout_hyper::derived_lane_overflow`,
    2026-10-10; per file since 2026-10-31): rows per item vs `ROW_MAX`, column pages
    vs `X_PAGE_MAX` (from Pass 1's `max_line_cols`), and items with a glyph past
    `ITEM_MAX`, checked once per load before any slot is emitted. Each item past a
    lane is laid out EMPTY with a warning naming it and its limit; every other item
    lands exactly where it would have (`an_item_past_a_lane_is_laid_out_empty_and_the_rest_is_untouched`).
    A limit costs its own file, never the load: torvalds/linux must always load
    (Ivan, 2026-10-31). Until that day the whole load fell back to Instanced. The
    emptied file keeps its bytes, so the pick cache refuses it (its bytes re-derive
    slots the layout never placed) rather than map a verb onto the next file's slots.
- **Tab stops are a recipe parameter** (`ItemParams::tab_width`, the
  type `fold::TabWidth`; 2026-10-11): a tab spans to the next multiple of N
  display cells of its logical line. A repo load takes `--tab-width`, else
  `[layout] tab_width_cells` (1, the layout before stops) through
  `RepoParams::tab_width`; `--hyper-oracle-check` applies the flag to its
  directory and text inputs (fixtures stay at one cell). Every HyperLayout
  walk asks `layout_hyper::tab_stop::TabStops` for a leader's measures: the
  line's cells come from the f64 line advance (exactly `cells x cell`), so a
  walk carries nothing new; a tab's ADVANCE lane is `f32(k) x cell` and the
  f64 line advance takes the exact `k x cell`. The one state a walk cannot
  rebuild is a chunk's start inside a long line: the per-chunk Pass 1 walk
  measures its tabs from the chunk's start, so `aggregate_chunk_prepasses`
  walks a newline-free chunk that holds a tab again from the true state
  (`walk_line_state`); `measure_continued_lines` takes the true seed
  already. `--render-file` is a one-item repo
  load (2026-10-11) with its own default, `[layout]
  render_file_tab_width_cells` (4, the stops its retired CPU stager had;
  `--tab-width` overrides it there too), resolved by
  `Cli::resolved_render_file_tab_width`. The agent-session cards stage
  through their own CPU code with fixed 4-cell stops and read no setting.
- A verify over ZERO items refuses. Before 2026-09-07 a missing corpus directory
  printed `PASS: 0 items, 0 instances` and exited 0 — the gate passing having
  compared nothing.

`fixtures/g-pick-repo/empty.rs` IS ZERO BYTES ON PURPOSE, and it is the only
input in the tree that reaches the page extent's origin seed. The seed binds
only for an item with ZERO records; before that file existed, seeding the
extent empty instead reddened its unit test and left all four screenshots
byte-equal, so pixel-ab could not see it at all. With the empty file in place
that same mutation moves `repo-wide.png` (verified 2026-09-04; `repo-zoom` is
framed on alpha.rs and still cannot see it, which is fine — one gate seeing it
is the point). Do not "tidy up" the empty file, and if the fixture is ever
rebuilt, put one back.

## Style discipline

- **Zero warnings** from `cargo build` and `cargo clippy`, always. Fix lints
  properly; suppress only when the lint is genuinely wrong for the code, as
  `#[expect(lint, reason = "...")]` (root `Cargo.toml` `[workspace.lints]`
  makes a bare `#[allow]` and a missing reason warnings, and an expectation
  that stops being met is one too).
- **Fail-loud panics**: this is a binary, not a library. `expect("...")` /
  `assert!` with a diagnostic message is the documented convention — do NOT
  convert to error-returning style. Bare `unwrap()` only in `#[cfg(test)]`.
- **Every wait on the device is bounded** (2026-11-01): block on the GPU
  only through `glyph_field::wait_for` / `glyph_field::map_read`, with
  `crate::config::settings().gpu.wait_limit()` (`[gpu] wait_timeout_secs`,
  60). Past it the wait panics naming what it waited for. Never
  `PollType::Wait { timeout: None }` (or `wait_indefinitely()`): on
  monolith1 a GPU test that never finished held the check battery 353 s
  until killed by hand.
- **`cargo fmt`**: the tree is NOT fmt-clean (mostly long-line wrapping). Do not mass-reformat — the diff/review cost
  exceeds the value. Match the local style of the file you're editing.
- Comments explain WHY (empirical findings, bug history, invariants), not
  what the code does. Existing `// Stage F: ...` tags are archaeology; do not
  add new stage letters (root `AGENTS.md` § Vocabulary) — name the substance
  and date it (`C28, 2026-10-10`).

## The hardware profile

`gpu::GpuProfile` is resolved once in `gpu::init` from the adapter wgpu picked
and carried in `GpuContext`. Anything that must branch on hardware — present
mode, indirect-draw support, the Metal `first_instance` workaround in
`glyph_scene.rs` — reads it; `cfg!(target_os)` is the wrong axis for all of
those and is not used for any of them. (The one `#[cfg(target_os = "macos")]`
axis in the tree is the unified-memory slot allocation below: it reaches the
Metal HAL, which exists only there.) `--gpu-key` prints the golden-set key
(`backend-vendor`), `--gpu-profile` the full record; root `AGENTS.md` § pixel-ab
says how the build tool uses both. `--present-mode fifo|mailbox|immediate` is
windowed-only and rides on the FPS line, because under Fifo that figure is the
display's refresh (75 on the first Linux box) and not a fact about the renderer.

`GpuProfile::glyph_footprint` (C35, 2026-10-24) picks where the glyph
shaders' AA footprint comes from, and so which shader text the field
compiles.
- **`Analytic` on Apple-on-Metal.** The same predicate as the unified-memory
  clause: an Intel or AMD Mac's Metal part is immediate-mode and stays on
  hardware. `crates/glyph-field/src/footprint.rs` derives this form from the
  mode's WGSL at pipeline creation by anchored substitution:
  - three flat varyings carry the quad's projective map in pixel units, its
    scale from `FrameUniform.viewport` (binding 0, the `Camera` block
    widened to read it);
  - the fragment inverts its 2x2 Jacobian for `fw` and for the emoji
    sheet's `textureSampleGrad`.
- **`Hardware` everywhere else.** The source is compiled exactly as written.

It is chosen at pipeline creation, not by a `Params` flag or an override
constant. A flag would make the 5090 run the analytic varyings and a branch
on every fragment, and an override constant would still change the module
it compiles. The 5090 must run today's shader, and its frames must stay
byte-equal to the vulkan-nvidia set. The measured cost there was 10-17% of
fps (+11-20% frame time), against a 14-26% fps gain on the M2 (oblique and repo-wide, 2026-10-24).
- **Instanced:** `quad_height = "inst.height"`.
- **Derived and Visible:** `"1.0"`. Visible draws through the Derived
  module.
- **The mask pipeline** is built over the same module, so selection and
  glyphs agree.

## Apple Silicon vs. Desktop x86_64 & Discrete GPU Specifics

This section is the one copy of these facts (the testing skill points here;
`research/desktop-platform-audit.md` is the 2026-10 survey behind it, and
its staging text predates C22). Key touchpoints:
- **Slot emission (`native/src/layout_hyper/emit.rs`)**: Pass 2 writes the Derived slots into HOST blocks that are the field's chunks, window by window through host scratch, on every machine; the wgpu field uploads one buffer per block (`crates/glyph-field/src/storage.rs`). Until 2026-10-31 the Instanced field's slots were emitted to the device directly (Metal shared memory on macOS; on discrete GPUs C22's windowed staging, with `GLYPH_STAGING` fallbacks); that path went with the Instanced field (`device_alloc.rs`, now `emit.rs`).
  - ⚠️ **Dynamic Color Writes**: `SlotStorage::write_colors` writes each slot's colour into its host block and, when the device holds a copy, one 4-byte `queue.write_buffer` per slot. On discrete GPUs with large repos, batch these writes to avoid driver call overhead.
- **Cache Sizing & Chunk Threshold (`native/src/layout_hyper/chunk.rs`)**:
  - `CHUNK_THRESHOLD_BYTES = 64 * 1024` (64 KiB) is tuned for Apple Silicon M-series L1 Data Cache (128 KiB per P-core).
  - Desktop x86_64 (AMD Zen 3/4/5, Intel Raptor Lake) has **32 KiB or 48 KiB L1D** per core. A 64 KiB chunk spills to L2. On desktop, testing 32 KiB or 16 KiB thresholds can keep chunks 100% L1D-resident.
  - A line with no newline near the threshold is cut INSIDE the line, and only before an ASCII byte (C15): no cluster sequence has an ASCII member after its first (asserted at atlas load), so every chunk resolves as the whole item does. The chunk after such a cut inherits the line's column, line advance and segment advance from Pass 1's aggregation, which computes the segment advance as the fold's running f32 sum. Changing the threshold moves `native/fixtures/chunk-cut.txt`'s cuts off the sequences it plants: re-prove its mutations.
- **Cache Lines & Burst Stores (`native/src/layout_hyper/pass2_device.rs`)**:
  - `emit_burst8` writes 8 `DerivedSlot`s (160 B). Standard x86_64 cache line width is 64 B (Apple M2 SLC/L2 is 128 B).
- **Copy sizes (C16, measured 2026-10-09 on an RTX 5090)**: a `copy_buffer_to_buffer` whose size is off 16 B runs whole at about half speed (wgpu only asks for 4), so the discrete staging copies were padded and split (`copy.rs`, `copy_split`). That code and its mutation went on 2026-10-31 with the host-record upload it served: the slots go up as host blocks through queued writes (`SlotStorage::from_host_slots`, then `sync` by block version), which make no buffer-to-buffer copy. The measurement stands for any copy path that returns.
- **Golden Pixel Keys**:
  - Golden baselines are keyed per rasterizer in `out/tooling-ab/baseline/<key>/`; sets exist for `metal-apple` and `vulkan-nvidia`. Any other key (e.g. `vulkan-amd`) is red until a human adopts its set.
- **Pure Rust Portability**:
  - No inline assembly, and one use of architecture intrinsics: `text::is_pure_printable_ascii` checks 16 bytes at a time with NEON on aarch64 (`std::arch::aarch64`); the 8-byte SWAR loop after it takes the tail there and the whole input on every other target (this line claimed zero until 2026-10-11). Otherwise LLVM auto-vectorizes clean slice loops to AVX2/AVX-512 on x86_64 and NEON on ARM64. Numerical pick checks (`tools/check-pick-oracle.sh`) are 100% bit-exact across platforms.

## Debug env vars

- `GLYPH_PROFILE=1` — builds a wgpu-profiler (and requests
  TIMESTAMP_QUERY_INSIDE_PASSES; TIMESTAMP_QUERY itself is requested
  wherever the adapter has it, with or without the variable);
  per-pass GPU timings print (windowed: 1 Hz; offscreen: once per run).
  Without it every profiling call site is a no-op `Option`. The profiler
  holds at most `gpu::PROFILER_PENDING_FRAMES` frames unread, because each
  is a Metal counter sample buffer and the M2 has 32 (C36); offscreen waits
  for the oldest frame at that cap, so it measures every frame. The wait
  costs no throughput there: `g-pick-repo`, 600 frames on the M2, 324-325
  fps unprofiled and 326-327 profiled (2026-10-24; the run is GPU-bound).
- `GLYPH_TRACE=<filter>` — the load path's span instrument: `repo.{walk,backend,verify,views,layout,staged,segments}` and
  `hyper.{pass1,pass2,emoji_tints,pass2.blocks}`, printed on span CLOSE with busy/idle times. The filter is a tracing EnvFilter string (fallback
  `RUST_LOG`; unset = off, one atomic per span). `glyph3d_native=info`
  is the useful setting — a bare `info` also admits wgpu's own
  tracing records, which is loud. The spans mirror the
  LoadStats Instant boundaries exactly so the two can be
  cross-checked; the prints stay the presentation contract.
- (`GLYPH_STAGING=host|single`, the discrete-GPU fallbacks of C22's windowed
  device emission, went with that path on 2026-10-31: the slots land in host
  blocks on every machine.)
- `GLYPH_HYPER_ORACLE_STRICT=1` — the hyper-oracle gate's form: besides
  refusing zero records, refuse a corpus in which the reference resolved no
  sequence or no ASCII-led one.
- `GLYPH_PERMUTE_GROUP_ROWS=1` — repo loads (and `--render-file`, a
  one-item repo load since 2026-10-11): files the views under their group
  rows in REVERSE, with row 0 a spare no view draws with
  (`repo::group_rows_of_views`), so no view's group row equals its index.
  The view registry's witness (`crate::views`, 2026-10-11):
  `native/tests/view_registry.rs` renders `g-pick-repo` with and without it
  and requires byte-equal frames in every field. Every pick line names both
  (`view=… group=…`).
- `GLYPH_PICK_DEBUG=1` — pick-path diagnostics: pixel ray, AABB hits, local
  point, candidate records (glyph_scene/pick.rs pick functions).
- `GLYPH_CULL_DEBUG=1` — at t=0.0 prints cull stats: visible draw ranges,
  instance count, backdrop count.
- `GLYPH_LIBRARY_TIMING=1` — `--layout-mode library` (2026-10-10,
  `native/src/library.rs`): one `LIBTIME` line per animated frame — nodes
  moved (dir and sheet local rows written), the CPU stages (`ease_ms`, the
  easing loop with its local-row writes; `note_ms`, noting the moved nodes
  for their deferred group rows and the tree cull), `local_bytes` (the local
  rows written, 32 B each), the PREVIOUS frame's flush
  (`last_flush_resolved`: nodes the CPU resolve reached;
  `last_flush_rows` / `last_flush_bytes`: group rows that changed and the
  bytes queued for them, whole runs; `last_flush_ms`: resolve, row rebuild
  and queueing), the last face update that did anything (`last_faces`,
  `last_faces_bytes`, `last_faces_ms`: the same for the page faces of the
  resolved sheets, `glyph_scene/faces.rs`) and the previous frame's
  tree cull (`last_cull_ms`, refit included; `last_cull_visited`: nodes
  walked; `last_cull_candidates`: segments handed to the exact test).
  Since 2026-10-26 the resolve, the faces and the regions are the host's
  (no compute pass): a move costs CPU and upload in proportion to the
  subtrees it moved, and nothing outside them. The move itself is still
  O(moved nodes): no bevy propagation, no per-file row rebuild (it waits
  for the flush, or for pick or a verb reading the group table first) and
  no `sync_segment` (the library culls top-down from the tree,
  `glyph_scene/tree_cull.rs`). Animations run on the
  offscreen 1/60 s clock (`SceneLike::animate`), so `--verb page-next
  --frames 120` is a deterministic page turn. With `[library] regions` on
  (E20, `glyph_scene/regions.rs`) a `REGIONTIME` line follows every frame
  whose regions did anything: `refits` (tree-cull slots refit, any library
  node), `composed` (instances recomposed: a refit box or a resolved
  directory), `uploaded` / `upload_bytes` (64 B instances whose bytes
  changed, and the bytes queued — whole runs, or the table past 15 %) and
  `prepare_ms`. A settled library prints nothing: it does no work. `out/LIBRARY-LAYOUT-FINDINGS-2026-10-10.md` has
  the measured figures.
- `GLYPH_G_DUMP=<slot>[,<len>]` — offscreen only: reads back instance bytes
  at `slot` from the glyph arena and prints hex (buffer write-path audits).
  It runs AFTER the frames and the readback wait. The PNG is untouched.
  `offscreen::parse_dump_spec` is the parser.
- `GLYPH_K4_SELFTEST=1` — windowed, dev-only (Stage K): at t≈3 s moves the
  Debug panel's "Show glyphs" slider programmatically (1.0 → 16.0 px per text
  row) and logs the cull counters before/after — exercises the panel → probe
  → CullState → cull path without a human at the mouse. (The panel's LOD
  block since C26, 2026-10-10: "Text detail" = `[lod] text_detail_px`, the
  greeking onset, with the fuzz toggle and smooth/pure beside it; "Show
  glyphs" = `[lod] show_glyphs_px`, the backdrop tier; the panel keeps
  them ordered, the moved handle winning — `UiProbeState::keep_lod_order`.
  CLI: `--text-detail-px`, `--show-glyphs-px`; the old `--greek-onset-px` /
  `--lod-min-px` and launch_config keys still parse.)
- `GLYPH_ZSPACE_SELFTEST=1` — windowed, dev-only: at t≈3 s drives the Debug
  panel's z_wrap_spacing dial to 2× and fires the SAME scene-rebuild arm the
  slider's drag-release uses, logging instance count (must not change —
  z_step moves no slot counts) and the field's z extent (must ~double)
  before/after. Exercises the panel → probe → pending_relayout → rebuild
  path without a human.
- `GLYPH_CLUSTER_SELFTEST=1` — windowed, dev-only: toggles the Debug panel's
  cluster-mode button through the SAME rebuild arm, logging the instance
  count before/after (on cluster-bearing content the count moves — trailers
  enter/leave the arena; the DIRECTION follows the starting mode: with
  cluster the default since 2026-09-22, the hook's toggle goes OFF and the
  count rises. Measured leader→cluster on fixtures/g-cluster-repo: 359 →
  345; on fixtures/emoji-corpus-small.txt, a TEXT scene: 893 → 801).
  The button shows on repo and text scenes; demo and agent-session scenes carry
  no mode and hide it.
- (`GLYPH_FIELDMODE_SELFTEST=1`, which cycled the field mode Instanced →
  Derived through the Debug panel's selector, went with the Instanced field
  and the selector on 2026-10-31.)
- `GLYPH_GRAB_SELFTEST=file[:dx:dy]` — windowed, dev-only (C29, 2026-10-10):
  the GRAB path through the window's own entry points — `apply_pick` on
  `file` (row 0 col 0), `on_cursor` to park the cursor at the centre,
  `on_key(G)` to grab, ten `on_cursor` steps along (dx, dy) px (default 0,
  400: straight down), `on_key(G)` to release — one step every fourth frame
  from t≈3 s, printing the HUD line after each (`GRAB-SELFTEST step N …`)
  and the group's offset at the end (`move-group 0 0 0`). Pair it with
  `--cam-pose` so the drag lands where you want.
- `GLYPH_DRAG_SCRIPT=g|c:DX:DY[:EVENTS[:FRAMES]]` — offscreen (2026-10-10): a grab
  drag scripted through the window's own entry points — cursor parked at
  the viewport centre, the grab key (`g` the picked file, `c` its zone; pick
  first with `--pick-file`; after a `--pick-px` that landed in a library
  region, both grab that region's directory), then EVENTS cursor moves per frame (default 1)
  totalling (DX, DY) px, for the first FRAMES frames (default every frame: a
  drag that stops, so a later ease can be seen keeping it). A drag is applied ONCE per frame (`apply_drag`,
  from `animate`), however many moves arrived; `GLYPH_DRAG_PER_EVENT=1`
  applies each move as it arrives, as before, for the A/B.
  `GLYPH_DRAG_TIMING=1` (offscreen or windowed) prints one `DRAGTIME` line
  per applied drag: moves folded in, groups synced, `move_ms` (the
  transform write; for a bevy-backed controller — the carrel — plus
  `update_transforms`, split into bevy propagation and scene-mesh
  re-extraction, both 0 for a library, whose files and zones are tree
  nodes), `sync_ms` (the controller's rows into the group table, adopted;
  for a tree move, noting it), `upload_ms` (0 for a tree move: the frame's
  flush resolves and uploads), `seg_ms`. Bevy propagation is
  O(every entity): 0.39 ms for one moved card on a 9,241-file library
  before the library left bevy (2026-10-10).
- `GLYPH_FRAME_SCRIPT="FRAME:STEP[;FRAME:STEP...]"` — offscreen (C32,
  2026-10-11): scene changes applied before frame FRAME (0-based) renders,
  so a frame can follow a DIFFERENT one, as every windowed frame does and
  no static offscreen run did. STEP is `pose X Y Z YAW PITCH` (the
  `--cam-pose` form, degrees) or `show-glyphs-px PX` (the panel's "Show
  glyphs" slider, set live). Built for C32 (a defect of the since-deleted
  Visible field that only a frame following a different one showed):
  `GLYPH_FRAME_SCRIPT="0:pose <elsewhere>;1:pose <target>" --frames 4`
  against a static render of the target. Use `--frames` past the last step.
- `GLYPH_REPEAT_CHECK=1` — offscreen (C35, 2026-10-24): every frame is
  read back and compared with frame 0. It prints `repeat: frame K: N px
  differ from frame 0 (max delta D)` per frame and a closing `repeat: F
  frames, M differ from frame 0`. A still scene must repeat inside one
  process. The per-frame GPU wait is part of the check: it is the stall under
  which the M2 showed both footprint states. `native/tests/oblique_repeat.rs`
  drives it at the `repo-back-oblique` pose.
- `GLYPH_POSE_PRINT=1` — windowed: F2 (screenshot) also prints the frame's
  camera as the `--cam-pose X Y Z YAW PITCH` argument (degrees) that
  reproduces it — the same line the Debug panel's "copy pose" button puts on
  the clipboard — so a view seen once can become a golden view's command.
- **F8** toggles the field HUD (windowed, egui): a top-left readout, separate
  from the F1 Debug window, of the field mode, engine, segments/hidden,
  backdrops, CPU cull ms, fps, and the instance and draw-range counts. It
  also shows the selection (the slot or slot range) and the pick's
  `file byte N`.
- `GLYPH_L3_SHADER_COMPOSITE=1` — offscreen, dev-only (Stage L): makes the
  offscreen target Bgra8UnormSrgb, forcing the WINDOWED shader-composite
  path (composite.wgsl) under the deterministic oracle driver; the readback
  swizzles BGRA→RGBA so the PNG compares directly against the Rgba
  baselines. The live-display-free proof of the composite shader.

The dev-only CubeCL instruments (`--cubecl-*`) and their env vars
(`GLYPH_CHAIN_*`, `GLYPH_RECORD_CHUNK`, `GLYPH_REPO_CHECK_*`) were retired with
the engine on 2026-10-09; `git show 659efdb:native/AGENTS.md` has their
documentation.

## Commit cadence

One logical change per commit; run `cargo glyph test` before each lands and
put what you ran in the message. Untracked scratch (`out/tooling-ab/sweep/`,
proof PNGs) is fine to regenerate; tracked artifacts change only on purpose.

The `out/history/STAGE_<X>_REPORT.md` convention is **retired** — see root `AGENTS.md`
§ "Where work lands". Do not open a new letter. Multi-part work still deserves a
written note in `out/`; it just does not need that template.

## Read next

Module headers in `src/*.rs` carry the real contracts (cull/LOD, pick, the
32 B record / slot formats, CLI op-stream ordering) — they are the most
reliable documentation in this crate, because they sit next to the code they
describe.

`out/` reports are design **history**, not current state; read one to learn why
a decision was made.
