> **History.** Moved to `out/history/` on 2026-10-10: a dated record, not current state. What is true now: `README.md`, root `AGENTS.md`, `out/MAINTENANCE-NOTES-2026-10-08.md`.

# Stage E2 — repo-scale loading: a whole repository as a field of code pages

**Goal**: load an entire repo through the Mojo engine and render it as a
navigable grid of code-file pages — one glyph arena, one group per file.

**Result**: done and verified on glyph3d-js itself. 1,305 files / 96.9 MB →
96.86M engine records → 95.18M glyph instances, load pipeline **3.7 s** wall
(web app: ~5.1 s for a *smaller* ~57 MB cut of the same repo), rendered as a
shelf-packed field of syntax-colored, paginated code pages. Batch FFI was
built, proven bit-exact against the per-file path on all 96.86M records, and
then **not** made the default — on this corpus the naive per-file loop is
measurably faster (evidence below).

## Architecture

```
walk (repo.rs)          engine (Mojo, FFI)            stage (repo.rs)         render (glyph_scene.rs)
─────────────           ──────────────────            ───────────────         ───────────────────────
recursive walk    →     per-file load_item  →         records + per-leader →  ONE arena, chunked into
whitelist + skips       (naive, default)              syntax colors           N storage buffers ≤
10 MiB cap              OR load_items blob            → instances appended    binding limit (2 chunks
valid-UTF-8 only        (batch, --repo-engine batch)  → FileView {record_     here), one draw/chunk,
                                                      base/count, slot_       group table shared
                                                      base/count}             1,305 GroupRows (5 vec4s)
                                                      → shelf layout          per file, dir tint
```

- **`src/repo.rs`** (new) — walker (skip `.git`/`node_modules`/`target`/
  `.pixi`/build dirs + dotfiles; ~60-extension source whitelist; 10 MiB cap
  that also keeps every item under the engine's per-item 2²⁴-byte ordinal
  wall; non-UTF-8 files skipped because the engine's decode assumes
  well-formed UTF-8 leads, per the E1 report), engine driver, staging,
  layout. All layout dials in `RepoParams`.
- **Engine pagination does the page fanning** (`has_page`, `page_rows=128`,
  per-file `pages_wide`, `page_gap_x`, `band_stride_y`): long files lay out
  as side-by-side pages of 100 cols × 128 rows, so NO file's footprint is
  taller than ~32 pages regardless of line count. `pages_wide` is sized from
  a folded-row estimate `max(newlines, bytes/wrap_cols)` — the bytes term is
  what catches minified single-line files (a newline count alone gives them
  `pages_wide=1` and a skyscraper). The estimate only guides proportions; the
  real footprint is measured from the records.
- **Shelf layout, classed by height**: files stably partitioned into
  small/medium/large height classes (path order kept within a class), each
  class shelf-packed left-to-right, classes stacked. Plain shelf packing pays
  every shelf its tallest member's height (2–3× waste on a skewed repo);
  classing keeps shelf-mates similar in height.
- **Syntax colors**: `text::colorize_leaders` — the Stage C tokenizer re-keyed
  to *engine record order* (one color per UTF-8 leader byte; `colors[i]`
  paints `records[i]`, bit-identical leader classification to
  `reference_layout`). Per-directory pastel tint via the group color
  (colorBlend 0 = multiply).
- **Chunked arena** (`glyph_scene.rs`): 95.18M instances × 48 B = 4,357 MiB
  total; each chunk stays under `max_storage_buffer_binding_size`. The
  adapter (Apple M2 / Metal) allows **4,095 MiB** per binding and per buffer
  (`gpu.rs` now requests the adapter's full limits), so the arena lands in
  **2 chunks**, one instanced draw each — instance_index is chunk-local,
  which is exactly right since each chunk buffer starts at 0. No subsampling
  anywhere; the 512 MiB wall is gone.
- **CLI**: `--load-repo <dir> [--screenshot out.png] [--frames N]
  [--focus-file SUBSTR] [--repo-engine naive|batch] [--repo-verify]
  [--repo-scan-only]`. Windowed mode orbits the field.

## Numbers (glyph3d-js, read-only, release build, M2)

| metric | value |
|---|---|
| files loaded | **1,305** (75 dirs walked; 4 files skipped >10 MiB, 0 non-UTF-8) |
| source bytes | **96.9 MB** (walker's whitelist is broader than the web app's ~680-file/57 MB cut — lock files, `_experiments`, docs included; real measured numbers, both corpora reported) |
| engine records | **96,860,762** (one per UTF-8 leader) |
| glyph instances | **95,181,245** (1.68M blank/missing dropped; 4,357 MiB on GPU in 2 chunks) |
| load wall time | **3.71 s** = walk 0.18 + engine 1.99 (48.6 MB/s) + stage 1.53 + layout ~0 (web: ~5.1 s for 57 MB → native wins absolutely AND per-byte, ~4.4×) |
| scan-only peak RSS | 2.15 GB (naive; records dropped per file) |
| full render peak | 8.53 GB RSS / 10.35 GB footprint (arena Vec + GPU buffers + upload churn) |
| steady-state fps, full field | **1.1 fps** (60 frames GPU-completed in 56.8 s) — see below |
| field extent | 15,098 × 22,683 world units |

**fps honesty**: 60 fps target MISSED at full-field view, by ~55×. The frame
is vertex-bound: 95.18M instances × 6 verts = 571M vertex invocations, each
doing a glyphmap `textureLoad`, every frame, with no frustum/LOD culling —
the zoomed view renders the same 95M instances and gets the same 1.1 fps.
This is precisely what the web renderer's far-LOD texture / ditherSpan path
exists for (the WGSL port already carries the minification ramp; the
far-texture tier is the unported part). Not a correctness issue — every
glyph is drawn — and not fixable by buffers or chunking. Listed as the top
remaining gap.

## Batch-FFI decision: built, verified, NOT default (evidence)

`glyph_engine_load_items` (NATIVE-PORT, `engine-local/ffi.mojo`): one call
for the whole corpus — concatenated blob + one **128 B descriptor block per
item** (explicit byte layout shared with `engine.rs::write_item_desc`: ten
f64 + six i32 params + u64 start/count), returning the full record stream
plus exact per-item record counts (counted from the pipeline's own flag
lanes). Per-item pagination params ride the descriptors, so batch and naive
run IDENTICAL layouts.

Measured on the real corpus (scan-only, warm cache):

| path | engine phase | throughput | peak RSS | sys time |
|---|---|---|---|---|
| naive per-file (1,305 calls) | **1.99–2.06 s** | 47–53 MB/s | 2.15 GB | 0.7 s |
| batch (one 96.9 MB call) | 5.63 s | 17.2 MB/s | 3.91 GB | **4.5 s** |

Batch loses: the whole-corpus run materializes ~3.5 GB of per-byte lane
arrays plus a 3.1 GB record arena in one shot, and the kernel time goes to
memory compression (`sys` 4.5 s, 6.5× the naive path's). The engine's 221
MB/s bench figure is for corpora that fit comfortably; at ~97 MB the single
call crosses the pressure cliff on this machine. **Default is naive**;
`--repo-engine batch` keeps the other path available.

**Verification**: `--repo-verify` runs BOTH paths and diffs every record
bit-exact (counts + measure bit patterns): `repo-verify PASS: 96,860,762
records bit-exact between naive and batch paths` — with per-item pagination
params in force. All Mojo conformance suites re-run green after the FFI
change (additive export only): ffi_selftest, conformance, conformance_scan,
conformance_bake, conformance_elide, conformance_record, conformance_resume,
conformance_gaps, conformance_matrix, ordinal_invariant, conformance_real
(34 files / 417,458 B).

## Screenshots (viewed, iterated)

- `out/e2-field-wide.png` — the whole field: ~1,300 distinct syntax-colored
  code pages in shelf rows; big files read as multi-page fans (the red-tinted
  fan top-left is a large test fixture). Iterations: (1) column layout →
  27:1 tall strip; (2) plain shelf → near-invisible (monster single-line
  files, `pages_wide=1` skyscrapers, 1.4% ink density); (3) +pagination
  +folded-row estimate +height classes → what shipped.
- `out/e2-file-zoom.png` — `--focus-file liveTrie.js`: the file fills the
  frame, crisp and readable (keywords/comments/strings/numbers colored),
  engine layout at 100-col wrap.

Known cosmetic: field aspect lands ~0.66 vs the 1.6 target (one tall
thin strip bottom-left = a file whose folded rows still exceed the estimate);
camera fits height, so the wide shot has side margins. Density/readability
goal met; polish item.

## Remaining gaps (for E3+)

1. **Far-LOD / culling** — the 60 fps story. Needs the web's far-texture
   tier (or at minimum per-group frustum culling by chunk/instance-range
   draw splits). Biggest lever by far.
2. **Emoji atlas** — bitmap slots still discard (no emoji pixels exported);
   🐀 leaves a double-width gap, as in E1.
3. **Live atlas growth** — miss reporting / trie regrowth channel absent
   (flags are not in the wire record; E1 noted this).
4. **Group rotation/clip plumbing** — still identity/unused; pagination made
   it unnecessary for page fanning. (Group offset, color tint, and
   colorBlend=0 ARE now exercised per file.)
5. **Picking absent** — FileView {record_base/count, slot_base/count} is the
   view contract picking will read; no hit-testing yet.
6. **Stage-phase cost** (1.53 s, ~40% of load): the per-leader tokenizer is
   serial Rust; vectorizable if load time ever matters more than it does at
   3.7 s.
7. **Memory headroom**: 10.35 GB peak footprint for the full render. Streaming
   upload (build chunks incrementally, drop the CPU arena) would roughly halve it.

## File diffs

- `native/src/repo.rs` — NEW: walker, per-file paginated params, naive/batch
  drivers, `--repo-verify` bit-exact gate, color staging, classed shelf
  layout, stats.
- `engine-local/ffi.mojo` — NEW `glyph_engine_load_items` (NATIVE-PORT):
  128 B/item descriptor blocks, full per-item params, per-item record counts
  from the flag lanes.
- `native/src/engine.rs` — batch FFI binding + `write_item_desc` +
  `Engine::load_items`.
- `native/src/text.rs` — `colorize_leaders` (per-leader syntax colors in
  engine record order); `StagedText.focus_bounds`.
- `native/src/glyph_scene.rs` — chunked instance arena (N buffers, N bind
  groups, N draws), `GroupRow::tinted`, focus-bounds camera override.
- `native/src/gpu.rs` — request the adapter's full storage/buffer limits
  (M2/Metal: 4,095 MiB each, logged).
- `native/src/main.rs` — `--load-repo`, `--repo-engine`, `--repo-verify`,
  `--focus-file`, `--repo-scan-only`; `SceneChoice::Repo`.
- `native/src/offscreen.rs` — steady-state GPU-completed fps line.
- `engine-local/README-FFI.md` — batch ABI documented (below).
- `native/libglyph_engine.dylib` — rebuilt; all conformance suites green.
