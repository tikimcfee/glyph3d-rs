# Zed-integration notes — the session log

Companion to `seam.md` (the frozen contract): this file is the running
record of what landed, what was measured, what bit, and what's queued.
Newest rungs at the bottom; every claim cites the commit that landed it.

## The arc so far

- **S1–S2** (`fc6a197`): Zed's language stack runs headless — no window, no
  editor — and `BufferSnapshot::chunks()` is pure data. HighlightIds are
  THEME-space indexes (`Language::set_theme` + `build_highlight_map`), so a
  consumer gets (text, color, weight, italic) with no name resolution.
- **S3** (`fd1731f`, `8bcb1d8`): the `--highlight` op paints per-glyph
  instance colors from a byte-range sidecar; offscreen proof rendered.
- **P1a/b** (`1fd056b`, `beca711`): `native` grows a lib target (the
  experiment workspace links renderer + Zed in ONE binary, one lock seeded
  from Zed's); seam vocabulary types land (`SurfaceUpdate`, the version-equality
  join, `ContentDelta` — the corpus rule as an enum).
- **P1c** (`00b6520`, `f7008a3`): `apply_surface_updates` makes the join
  load-bearing in the renderer; `experiments/fieldzed` proves one-process
  linking. The join demonstrated BOTH ways (76,129 glyphs colored; a stale
  version and an unknown file dropped, counted, never translated).
- **P1-live** (`049da2d`): the edit→reflow pipe. Provider scripts edits on a
  real `language::Buffer` keeping its own byte mirror; renderer applies the
  SAME deltas through its own splice; content hash joins the two independent
  applications. 0 dropped across every frame.
- **Style plane optimized** (`b64a3c8`, after merging main's engine wave in
  `5d8688b`): thread-local cached rederiver (one Engine + trie per thread —
  ffi.mojo resets the arena per load_item, "reuse the arena across loads")
  AND — the real find — the walk did one `queue.write_buffer` PER GLYPH
  (~15 µs of validation each). Coalesced to RecolorLine's full-instance
  per-run writes: ~130 ms/file → ~1.2 ms; frame-0 1.33 s → 8.4 ms.
- **Live windowed** (`8aa36ed`, fix `2d9374a`): `windowed::run` takes an
  optional `LiveSource`; `fieldzed --live` loops the edit script in the
  interactive renderer at 60 FPS. First live run CAUGHT A REAL BUG via the
  law: the staging drain discarded an edit that arrived inside its window →
  every later delta applied to the wrong base → the join refused them all.
  The join was right; `LiveSource.backlog` now replays pre-loop arrivals.

## Measurements that decided things

- Tier-0 rebuild (2-file field, 76k instances): fold ~5 ms — whole-scene
  rebuild per edit is viable; no record-splicing needed at this scale.
- Style plane pre-optimization: ~120–160 ms FIXED/file → per-glyph
  write_buffer, not the engine. Measure before believing the obvious cause.
- Windowed rebuild hitch (2026-09-26, instrumented in `poll_live`):
  **atlas 185 ms | fold 5 | stage 2–5 | scene 2.5–3 | restyle 0.6** — the
  atlas reload was 96%. Hoisted (`LiveSource.atlas`, loaded once by the
  embedder): rebuilds 14–16 ms, under one 60 FPS frame.
- Debug-build note: wgpu pipeline creation in debug ≈ 3.9 s per rebuild —
  the poking build is release, always.

## The main-thread hitch — layered plan (READY, not yet built)

The remaining 14–16 ms rebuild runs ON the render thread between frames.
Layers, in the order measurement would justify them:

1. **DONE — hoist content-independent work** (the atlas; anything else that
   never changes with bytes).
2. **Persistent scene shell**: split GlyphScene into persistent resources
   (pipelines, layouts, atlas views) and volatile content (instance buffers,
   groups). Rebuild touches only the volatile half; scene phase 3 ms → ~1 ms,
   and it is the prerequisite for layer 3 (a worker can't cheaply rebuild
   what shares GPU objects with the live frame).
3. **Async rebuild + swap**: build the next scene on a worker thread, swap
   atomically between frames (the pose-snapshot swap already exists). The
   render loop NEVER blocks; a rebuild lands one-or-more frames late.
   Constraints: wgpu types are Send; `UiProbe` is `Rc` (!Send) — install the
   probe on the main thread after the swap; the engine FFI is thread-local
   per worker (its own cached rederiver).
4. **Incremental restage**: per-file slot-range rewrite with capacity slack
   (O(file), not O(field)) — only if a field large enough demands it. This
   is the seam doc's Tier 1/2 ladder; the scan monoid is the eventual shape.

Stop at the first layer whose cost stops being perceptible.

## Landmines (each pinned where the next binary trips over the comment)

- `[patch.crates-io]` does not propagate across workspaces — re-declare per
  workspace root; zed's `async-task` patch rev is upstream-dead (crates.io).
- Locks: SEED from Zed's (fresh resolves drift — rust-embed grew a required
  trait item their code doesn't implement).
- `cargo:rustc-link-arg` does not propagate to dependent packages (own
  build.rs mirrors the engine rpaths); cargo features do not unify across
  sibling binaries (each needs its own `util/debug-embed` edge).
- Nested workspaces: directory walk claims crates for ancestor workspaces —
  the repo root `exclude`s `experiments/`; the experiments root sits ABOVE
  its members so walk-up finds it first.
- Merge shape: main evolves the assembly layer too — port their changes INTO
  lib.rs (the `cluster_mode` precedent, `5d8688b`).

## Queued

- Layers 2–3 above (2 first; it unblocks 3).
- P2: structure plane — folds as the first `StructureDelta` variant; the
  micro-sidecar golden view (pins OUR half of the seam; their half is their
  tests).
- seam.md owes the write-coalescing credit (it currently names only the
  cached rederiver) — fold into the next seam.md touch.
- `experiments/zedspike` `git mv` under `zed_integration_experiments/` when
  the docs and code want to live together — one move, once.
