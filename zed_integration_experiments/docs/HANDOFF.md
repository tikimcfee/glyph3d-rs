# HANDOFF — the Zed-integration experiments tree

Written 2026-09-27 for whoever picks this up next (agent or human). This
doc describes WHAT THINGS ARE; history and per-commit narrative live in
`notes.md`, the frozen contract in `seam.md`. Read order for a cold start:
this file, then `seam.md`, then `notes.md` if you want the why-it-got-this-
way layer.

## What this tree is

A worktree of glyph3d-native (branch `worktree-workspace-random-experiments`)
whose purpose is the Zed integration: the glyph field as a rendering backend
for Zed's editor machinery, proven rung by rung in `experiments/` while the
gated repo stays lean. Everything below is LANDED and working; the battery
is green at floor 127, goldens byte-equal.

**Command forms that matter here:**
- The gated battery: `cargo run --quiet --release -p glyph -- test` from the
  worktree root. (The `cargo glyph` ALIAS IS BROKEN IN THIS TREE and will
  stay broken: cargo CONCATENATES `[alias]` entries across the config
  walk-up, and this nested worktree sees the repo root's `.cargo/config.toml`
  twice — `cargo --list` shows the alias doubled. Use the hand-typed twin
  for everything; the error signature is the renderer's clap rejecting a
  doubled `--quiet`.)
- The one-binary experiment:
  `cd experiments && cargo build --release -p fieldzed` (debug builds are
  for compile checks only — debug wgpu costs ~3.9 s per scene rebuild).
- The live stepped demo: `./target/release/fieldzed --live s3-demo` —
  a window opens; **F3/F4/F5 step the 9-state table (title shows N/9)**,
  F1 Debug panel, F2 screenshot, WASD/right-drag fly, click picks. The
  provider is PAUSED between steps; states are frozen until keyed.
- Offscreen frames: `./target/release/fieldzed s3-demo <out_prefix>` —
  renders `edit{0..N}.png` (no folds, one-pass script).

## The systems, as they stand

**The seam** (`native/src/seam.rs` + `zed_integration_experiments/docs/
seam.md`): the provider-neutral contract. `SurfaceUpdate` = one file at one
`BufferVersion` carrying `ContentDelta` (Opened-once / Edited-in-place /
Tombstone — the corpus rule), `style: Vec<StyleRun>` (byte ranges, sRGB),
`structure: Vec<StructureDelta>`, `complete`. THE LAW: version equality or
drop — the renderer hashes the re-derived bytes (`content_hash_version`)
and refuses mismatches; there is no translation layer anywhere. File-driven
providers use content-hash versions (identity == version for static bytes).

**The renderer's live machinery** (gated repo, all battery-covered):
- `repo::load_items` — the loader proper; disk walks and caller-owned
  content (envelope bytes) are the same pipeline. `WalkResult::from_files`
  is the in-memory entry.
- `repo::rederive_cached` — ONE Engine + trie per THREAD (`thread_local`,
  keyed by trie path). The FFI resets its arena per `load_item`
  ("reuse the arena across loads", ffi.mojo); `Engine::new` + trie parse is
  ~120 ms, the record walk is microseconds.
- `GlyphScene::apply_surface_updates` — the seam's consumer; the style walk
  is COALESCED (full 48 B instances rebuilt per contiguous slot run,
  RecolorLine's pattern — one `write_buffer` per glyph is ~15 µs of wgpu
  validation and IS the trap) and consumes the COMPACTED record stream when
  folds exist.
- `windowed::LiveSource` + `poll_live` — the live loop: poll between frames,
  apply deltas to the owned content map, rebuild FROM THAT CONTENT (pose
  preserved — the relayout arm's rule), restyle every file's latest runs
  (`last_style` = the restyle memory; a rebuild resets colors, the memory
  repaints). The atlas rides IN the LiveSource (loaded once — its reload
  was 96% of the rebuild hitch). Phase instrument on every rebuild:
  `fold/atlas/stage/scene/restyle` µs. `backlog` replays pre-loop arrivals.
- `windowed::LiveStep` + `step`/`step_count` — the stepper: F3/F4/F5 from
  the App-level hotkey row (beside F2), window title names the state.

**fieldzed** (`experiments/fieldzed`): the one-binary proof — links
`glyph3d_native` AND Zed's crates (gpui/language/grammars/theme) in one
process, one lock. Provider thread runs a headless gpui App (TestAppContext
— see gaps) over real `language::Buffer`s with the Rust grammar and One
Dark; ships envelopes over mpsc. The stepped state table (9 states:
initial / fold ON / fold OFF / banner / delete / append / fold ON with
edits / fold OFF / reverted) waits on the control channel; transitions are
full-range Edited deltas (backwards/RESET included — no history needed),
fold flips are no-op edits at unchanged versions. `function_body_folds`
walks tree-sitter (`SyntaxLayer::node()`) for `function_item` bodies and
folds the block's INSIDE (`fn name(…) { }` outline survives; empty bodies
self-vanish in normalization).

**zedspike** (`experiments/zedspike`): the S1/S2 spike (headless chunks →
One Dark HTML + byte-range sidecar). Superseded as the demo path by
fieldzed; still the reference for "Zed's stack runs headless" and the
sidecar format (`--highlight`'s input). Its `Cargo.toml` comments carry the
patch-table and lock-seeding rationale.

**The fold pipeline** (P2a, complete as a PREVIEW tier):
`StructureDelta::Fold { range }` (byte-keyed, drop semantics) →
`seam::normalized_fold_lines` (expand outward to whole lines, sort, merge;
order-independent) → `repo::compact_folds` (drop folded records, renumber
rows, shift y by each fold's OWN measured span — `y(first folded) −
y(first kept after)`; extents recomputed exactly from the kept stream;
`kept_ix` joins compacted records back to parallel data) → arena re-splice
with fixed placements. `PickContext.folds` carries the set so the style
walk consumes the same compacted truth.

## Current boundaries (by design, not accident)

- **The pagination boundary**: long files fan into side-by-side pages
  (`page_rows` 128, bands stacked) — y is NOT a row ladder across pages, so
  renderer-side compaction is single-page ONLY. Paginated files skip folds,
  loudly, in ONE place (the folds-map build in `poll_live`) so the loader
  and the style walk agree. The durable fix — folds as ENGINE-level layout
  input so pagination/wrapping recompute — is the queue's headline item.
- **Records-less folds are `compact_folds`' pinned blind spot**: a fold
  covering only blank lines frees pitch the empirical span cannot see.
  Test-pinned so it can't silently "improve."
- **Picks on folded files** resolve against the unfolded record stream
  (`ensure_pick_cache` predates the fold set) — queued with P2b.
- **The provider is `TestAppContext`**, not the production headless shape
  (`remote_server`'s HeadlessProject pattern). Fine at current cadence;
  revisit when live editing UI arrives.
- **Live rebuilds ≈ 14 ms** (fold ~5 + stage ~5 + scene ~3 + restyle ~1).
  Under one 60 FPS frame. The layered hitch plan (persistent scene shell →
  async worker rebuild + swap → incremental restage) is written down and
  deliberately unbuilt; stop at the first layer whose cost is perceptible.

## Friction & errors — read before touching the build

**Workspace/dependency landmines (each pinned by a comment at the site):**
1. `[patch.crates-io]` does NOT propagate across workspaces — every
   workspace that path-deps into Zed's tree re-declares it. Zed's
   `async-task` patch rev is DEAD upstream (history rewrite) — crates.io.
2. Locks are SEEDED from Zed's `Cargo.lock` (copied into
   `experiments/Cargo.lock`). Fresh resolves drift (rust-embed grew a
   required `compressed` trait item zed's fs_embed arm doesn't implement).
   After merging main into this branch, refresh the seed.
3. `cargo:rustc-link-arg` does NOT propagate to dependent packages —
   fieldzed's build.rs mirrors the engine rpaths, or dyld fails with
   "no LC_RPATH's found."
4. Cargo features do NOT unify across sibling binaries — each binary
   touching zed's `grammars` needs its OWN `util/debug-embed` edge, or
   fs_embed walks the EXE's ancestors for a `.git`, finds this worktree,
   and panics "missing config for language rust."
5. Nested workspaces: the repo root `exclude`s `experiments/`; the
   experiments root sits ABOVE its members so walk-up finds it first.
6. Merges from main evolve the assembly layer — port their changes INTO
   `lib.rs` (the `cluster_mode` precedent, merge `5d8688b`), keep the CLI
   shell in main.rs.
7. `tree-sitter` must be ZED'S GIT FORK (same rev as the patch table) in
   any experiment crate that names Node types — a crates.io version is a
   second, incompatible crate.

**Process lessons (measured, each cost a live bug):**
- One `queue.write_buffer` per glyph ≈ 15 µs of validation — coalesce to
  per-run full-instance writes. (~110× on the style plane.)
- Content-independent work (the atlas: ~185 ms) must ride the source, not
  the rebuild. (96% of the hitch.)
- The restyle REBUILDS positions from records — any transform applied at
  load (compaction!) must also be applied to the stream the restyle walks,
  or the restyle undoes it (the empty-not-collapsed bug).
- A guard placed in one consumer (loader) but not the other (style walk)
  is half a guard — put shared geometry decisions at the single map build.
- Exclusive-end ranges in tests: `1..2` folds ONE line, not two. The
  batteries caught this twice; write the line arithmetic in the comment.
- Timer demos hide defects; STEPPED demos make them addressable by state
  name. Four live-found bugs came after the stepper existed in spirit
  (frozen screenshots + named frames).
- `cmp -s` on PNGs across different `image` crate builds is meaningless —
  the eyes (or pixel-ab gates) decide.

## The collaboration model

Ivan flies the window and is the visual oracle; the agent reads the
stdout stream (picks, `live:` phases, `seam:` audits, `fold:` lines) —
launch the window as a BACKGROUND task and read its log. Ask for eyeballs
at moments of truth (folds, reflow, anything geometric); screenshots
confound automated vision sometimes, human eyes don't. Defects are named
by state ("holes at step 4") — keep every demo stepped.

## Queued (coldest-start first)

1. **Engine-level fold input** — folds as ItemParams input so pagination,
   wrapping, everything recomputes; retires the pagination boundary; the
   renderer-side compaction stays as the preview tier. Requires engine
   work + a new oracle burden — a full rung, not a patch.
2. **P2b micro-sidecar golden view** — checked-in sidecar (+ folds) applied
   to a fixture, pixel-pinned, no Zed in the gate; pins OUR half of the
   seam. Include the PickContext fold set (picks-on-folded).
3. **Live-windowed cadence tiers** if ever needed (persistent scene shell;
   async rebuild + swap — `UiProbe` is Rc/!Send, install post-swap).
4. `experiments/zedspike` → `git mv` under `zed_integration_experiments/`
   when code and docs want to live together (one move, once).
5. `FileKey` graduation (rel_path → ProjectPath-shape) when the spatial
   workspace grammar work starts.

seam.md's ladder and notes.md's queue list carry the same items with more
context; keep all three in sync when something lands.
