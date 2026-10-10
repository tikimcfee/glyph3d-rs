> **History.** Moved to `out/history/` on 2026-10-10: a dated record, not current state. What is true now: `README.md`, root `AGENTS.md`, `out/MAINTENANCE-NOTES-2026-10-08.md`.

# Stage J — engineering hygiene sweep (clippy zero, dead-code/unwrap triage, docs + house rules)

**Goal**: zero clippy warnings, zero stale `#[allow(dead_code)]`, diagnostic
`expect()`s on opaque runtime unwraps, accurate module docs, a written house
rulebook, and a one-command gate runner — all **provably renderer-neutral**.

**Result**: 3 commits, all gates green on HEAD (`5aa8fb2`). Clippy
**14 → 0**, build warnings **0 → 0** (held), `#[allow(dead_code)]`
**6 → 0** (3 sites deleted as genuinely unread, 3 were stale allows),
runtime bare `unwrap()` **5 → 0** (11 test-only ones left by design). No
rendering behavior changed: the four-view A/B suite is **BYTE-EQUAL** to
`out/tooling-ab/baseline/` after every commit.

Started from `d892ee6` (the brief said `2e3910f`; main had one further
commit of tracked pick-fix artifacts — harmless, swept around it).

## Commits

| hash | part |
|---|---|
| `e22f260` | clippy zero — 14/14 warnings fixed properly |
| `3dd59d2` | dead-code removal + unwrap triage |
| `5aa8fb2` | docs + house rules — module headers, AGENTS.md, check-all.sh |

### Commit 1 — clippy zero (`e22f260`)

| lint | count | fix |
|---|---|---|
| chunks_exact with constant | 3 | `as_chunks::<N>()` — byte-identical: atlas parsing asserts len % 4 == 0 first (no remainder possible); pick_px/cam_pose have the same drop-remainder semantics as chunks_exact |
| too_many_arguments | 2 | `repo.rs`: `StageCtx` bundles params/instances/blanks for `stage_one` (8→6 args). `scene.rs`: NEW `FrameTarget` bundles color/depth views + physical size — `SceneLike::render` drops its pre-existing `#[allow]`, both impls and both call sites updated |
| type_complexity | 2 | `glyph_scene.rs`: local `CulledDraws` alias. `text.rs`: `FoldTables` alias for `fold_leaders`' four parallel vectors |
| manual checked division | 2 | `text.rs`: `checked_div().unwrap_or(0)`; the newline branch now reuses `wrap_row` (col is unchanged there — identical value, one less division) |
| doc_lazy_continuation | 2 | blank `//!` line so the lossless-culling paragraph stops parsing as a list continuation |
| redundant import | 1 | `use wgpu;` removed |
| manual is_multiple_of | 1 | `is_multiple_of(4)` |
| field_reassign_with_default | 1 | `wgpu::Limits { .., ..Default::default() }` struct-update form (identical values) |

No `#[allow]` band-aids added; one pre-existing allow *removed*.

### Commit 2 — dead-code + unwrap triage (`3dd59d2`)

Removed all six `#[allow(dead_code)]`, rebuilt, and let the compiler rule:

- **Deleted as genuinely unread** (grep + `cargo build` verified):
  `FLAG_BLANK` (the "Stage E growth logic" reader never landed — format
  constant documented in a comment), `Atlas::{slot_count, curve_count}`
  (locals still feed the glyphmap agreement assert + info log),
  `FileView::{record_base, rows}` — the struct doc *claimed* "Stage G
  picking reads them"; it never did. Record base is re-derivable from the
  `item` params; rows is encoded in `height`.
- **Stale allows** (fields ARE read): `TrieEntry::height_fu` (text.rs:397),
  `TrieTable::mapped_count` (atlas info log), `FileView` itself (picking).
- **unwrap()**: 4 runtime unwraps in glyph_scene pick/verb paths +
  atlas magic `try_into()` → `expect("…invariant…")`. The 11 remaining bare
  unwraps are all in `#[cfg(test)]` (verb-parser + encase-layout tests)
  where the harness prints the failing input — left per fail-loud style.
  No error-returning conversions (documented binary convention).

### Commit 3 — docs + house rules (`5aa8fb2`)

- **Stale header claims fixed**: glyph_scene.rs said "two-level GPU culling"
  — CPU since the Stage F pivot (the same header documents *why*). repo.rs
  header referenced the deleted `record_base`. main.rs stage history gained
  Stage H/I lines. gpu.rs `init()` got a contract doc.
- **NEW `native/AGENTS.md`** — determinism chain, gates, zero-warning and
  fail-loud discipline, fences (engine-local/ + assets/atlas/ + shaders +
  baseline fixture read-only; no dep bump without a re-baselined
  mini-stage), the four debug env vars (each grep-verified before
  documenting), commit cadence.
- **NEW `tools/check-all.sh`** — all six gates in one command, including
  the screenshot regeneration + `cmp`. The exact baseline-view commands
  were **not recorded anywhere**; recovered by byte-matching:
  demo = `--demo --frames 2`; text = `--render-file fixtures/baseline-view.txt --frames 2`;
  repo-wide = `--load-repo fixtures/g-pick-repo --frames 2`;
  repo-zoom = same + `--focus-file alpha.rs --zoom 3`.

## `cargo fmt` status (deliberately untouched)

The tree is NOT fmt-clean: 77 hunks across 11 files (main.rs 21,
glyph_scene.rs 16, repo.rs 11, text.rs 8, windowed.rs 6, atlas.rs 5,
offscreen.rs 3, scene.rs/gpu.rs/engine.rs 2 each, build.rs 1) — almost all
long-line wrapping plus minor trailing-comment alignment. Mass-reformatting
would swamp this sweep's diff for zero behavioral value; documented in
AGENTS.md instead ("match local file style").

## Gates (final run on HEAD `5aa8fb2`)

| gate | result |
|---|---|
| `cargo build --release` | PASS — 0 warnings |
| `cargo clippy --release` | PASS — 0 warnings (was 14) |
| `cargo test` | PASS — 19 green (18 unit + 1 wgsl) |
| `--engine-check src/main.rs` | PASS — 33,713 records bit-exact |
| `tools/check-stage-g.sh` | ALL PASS |
| demo.png cmp baseline | BYTE-EQUAL |
| text.png cmp baseline | BYTE-EQUAL |
| repo-zoom.png cmp baseline | BYTE-EQUAL |
| repo-wide.png cmp baseline | BYTE-EQUAL |
| `tools/check-all.sh` | ALL GATES GREEN |

## File diffs (d892ee6..HEAD)

```
 native/AGENTS.md          |  78 ++++++  (new)
 tools/check-all.sh        |  75 ++++++  (new)
 native/src/repo.rs        |  71 +++---  (StageCtx, FileView trim, header fix)
 native/src/scene.rs       |  36 +++---  (FrameTarget, trait + impl + inherent)
 native/src/atlas.rs       |  28 +++---  (dead code, as_chunks, expect)
 native/src/glyph_scene.rs |  27 +++---  (render impl, alias, expects, headers)
 native/src/gpu.rs         |  14 +++---  (limits init, import, init doc)
 native/src/text.rs        |  14 +++---  (FoldTables, checked_div)
 native/src/offscreen.rs   |  12 +++---  (FrameTarget call site)
 native/src/windowed.rs    |  10 +++---  (FrameTarget call site)
 native/src/main.rs        |   9 +++---  (as_chunks, stage history header)
```

No changes to: `engine-local/`, `assets/atlas/`, `src/shaders/*.wgsl`,
fixtures, Cargo.toml/lock, staging math, buffer write paths, render order,
CLI semantics, or any version pin.

## Remaining debt deliberately NOT touched

- **`cargo fmt` divergence** — documented above and in AGENTS.md; not
  reformatted to keep the diff reviewable.
- **11 test-only bare `unwrap()`s** — idiomatic in `#[cfg(test)]`; the
  harness prints the failing input.
- **~37 `expect()`s elsewhere** — house fail-loud style, by design.
- **out/*_REPORT.md "remaining gaps" sections** (emoji, IME, whole-file
  LOD) — FEATURE gaps, not debt.
- **wgpu-30 Metal indirect-draw bug** (Stage F header) — documented
  upstream limitation; CPU culling is the standing design, revisit only
  after a wgpu upgrade (which itself requires a re-baselined mini-stage).
