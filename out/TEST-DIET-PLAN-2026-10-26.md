# Test diet: which checks to cut, and what each one costs (2026-10-26)

A plan only. Nothing is deleted on this branch. Branch `mp-test-diet` from
`main` @ c56ae30.

Ivan asked how much of the verification suite is repeated CPU work, and
whether it can be replaced with something more useful. Across the day's six
engine branches the gates caught bookkeeping (test-floor drift, a
mis-aimed mutation `expect`, one vacuous mutation) and no code bug. The
problems that mattered were found by reading, measuring and running the
code. This note prices every check, says what it alone can catch, and says
whether that failure is still possible.

## 0. Where the time actually goes

All figures were measured once, in this worktree's own target dir, on the
32-core Linux host (`vulkan-nvidia` golden set). Other agents' builds were
running at the same time (load average 9 to 28), so read them as ±30 %. The
mutation figures come from the provejit sibling's logs
(`out/PROVE-ON-RELAX-2026-10-26.md` on `mp-prove-jit`) and from
`out/AGENT-TOOLING-2026-10-11.md`. No full battery or full prove was run
for this note.

| what | today | share |
|---|---|---|
| **`cargo glyph prove`, unscoped** (116 mutations) | **~41 min** | **~96 %** of a merge |
| `cargo glyph test`, warm, after a one-line edit in `native/src` | **~103 s** | |
| `cargo glyph test` in a fresh worktree (cold target dirs) | ~3 min (release build 60 s + test build 15 s + no-ui 21 s + clippy/doc/orphans cold) | |

The per-change battery after a one-line comment edit to `native/src/fold.rs`
(each gate run once, in battery order):

| gate | warm, after edit | of which compile |
|---|---|---|
| products + cargo-build | 17.3 s | all |
| cargo-test | 38.8 s | ~10 s (lib test + 11 integration binaries relinked); 29 s of tests, run serially |
| orphans | 22.4 s | all of it is cargo-modules, whatever the edit |
| pixel-ab (30 renders) | 8.3 s | 0 |
| pick-oracle | 4.3 s | 0 |
| cargo-clippy | 3.0 s | all |
| cargo-doc | 2.7 s | all |
| hyper-oracle (4 runs) | 2.1 s | 0 |
| cargo-check-no-ui (2 checks) | 1.5 s warm (20.8 s cold: its own target dir) | all |
| committed-artifacts | ~1.6 s (fixtures regen 0.95 s, the rest ≤ 0.17 s each) | 0 |
| atlas-verify / atlas-tables / vendor-hashes | 0.33 / 0.09 / 0.14 s | 0 |
| reference-port (5 halves) | 0.10 s | 0 |
| repo-verify + repo-verify-direct (4 runs) | 0.10 s | 0 |
| manifest | 0.00 s | 0 |
| **total** | **~103 s** | ~57 s compile, ~46 s checking |

**The finding.** The layout oracles, the corpus checks and the pixel
goldens together cost about **15 s**. Most of the per-change time is
rustc (build and test relinks, ~30 s), the test suite's serial run (29 s,
9.5 s of it a single test) and orphans (22 s). The merge cost is almost
all `prove`: 116 mutations, each one a release rebuild plus a gate run.
That is 25 times the whole battery.

The cheap oracles are not where the churn is, and they guard the arithmetic
the makepad port has to reproduce. **Cut prove's repetition first. Leave
the oracles alone.**

## 1. The gates

Legend: **KEEP-change** = run on every change (the inner loop).
**KEEP-merge** = run once per merge. **MERGE** = fold into another check.
**DELETE**. Gates marked **IF** depend on a decision that is Ivan's (§ 5).

| gate | cost | what it alone catches (and what it was added for) | still possible? | verdict |
|---|---|---|---|---|
| manifest | 0 s | A product input that matches no file, so the currency stamp hashes nothing (`native/src/**`, 598205b → 2026-09-16; `native/Cargo.lock`, 2026-09-06 → 10-09) | yes, on every build.toml edit | KEEP-change (free) |
| committed-artifacts | ~1.6 s | A generated file edited by hand, or a generator that changed behaviour (schema, emoji sheet, cluster table, emoji corpus, atlas, fixture corpus, with declared counts) | yes, but only when those inputs change | KEEP-merge (corpus scope) |
| atlas-tables | 0.09 s | What `codepoints.bin`/`glyphs.bin` mean at the pins (D9: these checks lived in the retired engine-trie generator) | yes, on an atlas re-export | KEEP-merge |
| atlas-verify | 0.33 s | The curve and glyph-map textures against `glyphs.bin`, and the trie's section layout (the script ran by hand for a year, then was gated 2026-10-11) | yes; the makepad float tables (`float_tables.rs`) read the same bins | KEEP-merge |
| vendor-hashes | 0.14 s | A vendored oracle input or font edited in place | yes | KEEP-merge |
| products / cargo-build | 17.3 s | Zero warnings, and the binary every later gate reads | yes | KEEP-change (it is the build) |
| cargo-check-no-ui, **bare** half | ~0.6 s warm | The engine with no window: the build the makepad host links (2026-10-26). Proven by `windowed-module-ungated` and `winit-unconditional` | **yes, and newly load-bearing** | KEEP-change |
| cargo-check-no-ui, `--features windowed` half | ~0.9 s warm | C24: egui used outside `egui-ui` (2026-10-09). Window without overlay is a build no one ships | the build only matters while the winit window is a product | **DELETE** this half, with `egui-use-ungated` (**IF** the window stays a product, keep it at merge) |
| cargo-clippy | 3.0 s | Lints. C8 found 14 test-only lints, one deny-level, when `--all-targets` was turned on | yes | KEEP-merge |
| cargo-doc | 2.7 s | Dead intra-doc links (`Engine::records`, 2026-09-07, found by an audit) | yes, on renames | KEEP-merge |
| orphans | **22.4 s** | A `.rs` file no `mod` reaches. Its realistic case, a dropped `mod tests;`, is also caught by the cargo-test floor by count. Added 2026-10-11; nothing caught on record beyond its own mutation | rare | **KEEP-merge only.** It is 22 % of the per-change battery for a hygiene check |
| cargo-test | 38.8 s | See § 2 | yes | KEEP-change, trimmed (§ 2) |
| pick-oracle | 4.3 s | Picks and pixel-ray round trips against an independent Python fold. Catches a pick that resolves to the neighbouring glyph (`pick-off-by-one`) and emoji double-advance rects (2026-09-10) | yes. Picking is CPU code the makepad host will call | KEEP-change (render scope) |
| reference-port: trie, fold, reference halves | ~0.1 s | The Rust fold, trie and `text.rs` bit-exact against the JS oracle's recorded lanes (1,874,328 lanes). `phantom-row`: a fault that a fold-vs-scan comparison cannot see because both share `rows_for_line`. hyper-oracle's truth reaches HyperLayout through `fold.rs` | **yes; this is the arithmetic the makepad port must reproduce** | KEEP-change |
| reference-port: **scan** half | (in 0.1 s) | `scan.rs`, the fold recast as a monoid scan. That is the dispatch structure of the retired GPU compute layout (CubeCL, retired 2026-10-09). **Nothing ships it**: only `main.rs`'s instrument and `fixture.rs` call it | no. There is no GPU scan to be faithful to | **DELETE** (§ 4 step 3) |
| reference-port: **bake** half | (in 0.1 s) | `bake.rs`, the streaming record and its seed protocol. **Nothing ships it**: only `main.rs`'s instrument calls it. Visible mode's line-table seeds are a separate implementation | no | **DELETE** (§ 4 step 3) |
| hyper-oracle, layout tiers (4 runs) | 2.1 s | HyperLayout, the engine that ships, against the oracle-backed fold, including the device Pass 2. **The most productive gate in the repo's history**: red on arrival, it found C10 (keycaps), C13 (cluster mode ignored), C14 (paging, both Pass 2s) and C15 (chunk-cut seeds and split sequences) | yes. The makepad host lays out through the same HyperLayout | KEEP-change |
| hyper-oracle, `RenderSlot` device tier | (inside) | The Instanced slot format, byte for byte | ends with Instanced step 3 | DELETE with step 3 |
| hyper-oracle, Visible tier + span half | (inside, GPU) | The Visible kernel against the device Pass 2 | only while Visible exists | **IF** (§ 5) |
| repo-verify | 0.05 s | HyperLayout against HyperLayout (prefetched against inline Pass 1). Its own `blind_to` says no mutation can redden it: since C15 both sides are one driver | no. It is a determinism check that cannot be shown to fail | **DELETE** |
| repo-verify-direct | 0.05 s | `direct` against recording. Same engine, same host path, "UNCOVERED in prove: no mutation separates the two sides" | no | **DELETE** |
| pixel-ab, plain (10 frames) | ~2.8 s | The only check that sees pixels. Found the `048d403` far-line cull regression and C17 during the 2026-10-09 re-adoption walk; `depth-write-off` and `emoji-uv-flip` are classes nothing numeric can see | yes. **The end-to-end oracle until the makepad host reaches parity.** Then the wgpu frames become the reference the makepad set is compared to | KEEP-change |
| pixel-ab, `--field-mode instanced` | ~2.8 s (+2.8 s per pixel-ab mutation) | The Instanced field held to the same frames | ends with Instanced step 3 | DELETE with step 3 |
| pixel-ab, `--field-mode visible` | ~2.8 s | The Visible field held to the same frames. `visible-row-off-by-one` proves it | only while Visible exists | **IF** (§ 5) |

## 2. cargo-test, by binary and module

The whole run takes 29 s of serial test time. Times are per binary, or
per test where one test dominates its binary.

| class | cost | catches | still possible? | verdict |
|---|---|---|---|---|
| native lib: **`hyper_oracle::tests::tab_stops_agree_on_every_tier`** | **9.5 s**. It is the lib binary's long pole: the binary takes 11.5 s, and its other 306 tests sum to 14 s across threads | Tab stops in a TABBED line cut into chunks, on every tier. That is the one class no gate corpus reaches (`hyper-tab-chunk-rewalk-skipped`) | yes | KEEP, **trim**: it runs 2 widths × 2 cluster modes × 6 shapes × 4 texts, and two of the texts are 135 KB. Run the two long texts only at the two pairings the hyper-oracle gate uses (width 4 cluster, width 8 leader). Est. 9.5 s → ~2.5 s, and the lib binary → ~4.5 s |
| native lib: layout core (`fold`, `layout`, `layout_hyper`, `hyper_oracle` grids, `text`, `glyph_trie`, `atlas`, `float_tables`, `fixture`, `seam`) | ~3 s | The unit grids for classes no corpus reaches (pagination grid, windowed emission, line-table seeds, `fu_to_world` exact over u16, the float tables' exact integers) | yes, and these carry the makepad port's math | KEEP-change |
| native lib: `scan::tests` (8), `bake::tests` (4, one is 1.1 s), the scan arm of `fixture` | ~1.3 s | the retired scan and bake forms | no | DELETE with step 3 |
| native lib: scene and group tables (`glyph_scene::{nodes,cull,pick,frame,…}`, `views`) | ~3 s (faces 3.7 s and nodes 2.3 s are GPU round trips) | The CPU resolve, the flush, culling, LOD | yes. The makepad host draws `FrameData` from these | KEEP-change |
| native lib: library (`library::tests`, `glyph_scene::{regions,faces,tree_cull}`) | ~5 s (regions ~4 s, faces 3.7 s) | The library probe's arithmetic, regions and tree cull. No golden renders the library, so these are its only witness | only if the probe lives on | **IF** (§ 5) |
| native lib: CLI, config, launch_config (51 tests) | < 0.1 s | Flag parsing and config merging | yes | KEEP (free) |
| native lib: launcher TUI (17) | < 0.1 s | The launcher's controls and plan | only while the TUI is the launcher | **IF** (§ 5), and free either way |
| native lib: `agent_transcript`, `revision` (25) | < 0.2 s | The gomuxai event reader against its vendored conformance corpus, and revision rebuilding | yes. This is CPU logic any host would reuse | KEEP |
| native lib: `spatial_scene`, `layout_stack` (24) | < 0.1 s | The bevy-ECS scene, zones and decks | yes (still wired: `glyph_scene`, `relayout`, staging) | KEEP |
| native lib: `gpu` (12, key and profile) | < 0.1 s | The golden-set key per adapter | yes | KEEP |
| crates: `glyph-field`, `-derived`, `-scene-graph`, `-instanced` libs; wgsl tests ×4 | < 0.1 s | Slot formats, the copy split (C16), stale handles, compose order, naga validation | yes; `-instanced` ends with step 3 | KEEP; `-instanced` goes with step 3 |
| crates: `glyph-field-visible` (`gpu` 0.8 s, `fp_probe` 0.2 s, lib, wgsl) | ~1 s | The Visible kernel against its CPU twin, C32's stale tail, and the device's float reassociation | only while Visible exists | **IF** (§ 5) |
| integration: `derived_verbs` | 3.4 s | Derived's verbs report what they cannot represent. It compares against Instanced | rewrite at step 3 | KEEP, rewritten at step 3 to compare against a stated expectation, not the Instanced field |
| integration: `view_registry`, `render_file_copies`, `tab_stops`, `empty_load` | 3.1 / 2.5 / 2.1 / 0.6 s | The group row is not the index (step A), `--copies` tiling, tab stops in pixels across fields, and the empty load (a panic that once happened) | yes | KEEP. Each drops its Instanced run at step 3 (about 1/3 of its renders) |
| integration: `visible_verbs`, `visible_repeat` | 2.4 / 1.25 s | M3 verb keys in pixels; C30 frame-to-frame repeat | only while Visible exists | **IF** (§ 5) |
| integration: `library_regions` | 1.1 s | Regions in pixels | only while the library lives | **IF** (§ 5) |
| integration: `curve_cap` | 0 s | The shader `MAX_CURVES` bound against the largest atlas outline. This was a real defect: ☃ and ☠ were drawn cut off in Derived until 2026-10-11 | yes, and a makepad shader will carry its own bound | KEEP. Point it at the makepad shader too when that exists |

cargo runs test binaries one at a time. The eleven integration binaries
are 19 s of the 29 s, and each one launches the renderer offscreen.
Running them in parallel (nextest, or the runner) belongs to the gatetime
sibling. It is the largest single per-change lever left after the cuts
above.

## 3. Mutations, by group

Per-mutation cost on today's cargo route is measured (provejit,
`mp-prove-jit`). A native-lib cargo-test mutant takes ~30 s. A crate or
runner mutant takes ~21 s. An integration-test witness takes ~43 s. A
renderer-instrument gate (hyper-oracle, reference-port, pick-oracle)
takes ~23 s. A pixel-ab mutant takes ~29 s. A compiler-diagnostic gate
takes ~17–20 s. A data mutation takes ~1 s. On relax `--jit` a lib mutant
takes 6–8 s and a small crate 1.5–2 s, but an integration test is
slower there (55 s).

**What a mutation is for.** It proves once that a check can fail. A
cargo-test mutation names the test it reddens. That test can only go
vacuous if the test itself or the code it reaches is edited. Re-proving
all 116 at every merge pays ~41 min to re-confirm 116 facts. The merge
moved very few of them. `prove --since <ref>` already exists and selects
the right subset. Applied to today's merges (target file changed, or the
gate's script changed), it selects:

| merge | files | mutations `--since` selects |
|---|---|---|
| mp-relax-fidelity | 8 | 0 |
| deps-ca-certificates | 10 | 0 |
| mp-tables | 4 | 6 |
| mp-cpu-twins | 38 | 28 |
| main into mp-tables | 48 | 31 |

That is 0–31 mutations instead of 116: 0 to ~13 min on cargo, and less on
relax.

| group | count | full-prove cost | what they prove | verdict |
|---|---|---|---|---|
| manifest, committed-artifacts, vendor-hashes, atlas-* | 12 | ~15 s | Corpus and tooling refusals | KEEP. Nearly free |
| Runner bookkeeping on cargo-test (`empty-test-selection-passes`, `prove-restale-blind`, `prove-ambiguous-find-accepted`, `since-matches-whole-gate-command`, `nested-alias-unstripped`) | 5 | ~1.8 min | The runner's own unit tests, against defects it once had | KEEP. Selected only when `glyph/` changes |
| **Gate-mechanism proofs anchored in engine files**: `unused-binding`, `clippy-lint` (both `native/src/fold.rs`), `dead-doc-link` (`layout.rs`), `dead-doc-link-crate` (`glyph-field/src/lib.rs`) | 4 | ~1.3 min | That the runner COUNTS rustc's, clippy's and rustdoc's warnings. That is a property of `warn_count` and the command line, not of `fold.rs`. Because they are anchored in `fold.rs` and `layout.rs`, `--since` selects them on every branch that touches the fold, which is where real work happens | **MERGE into a runner unit test**: `warn_count` over captured warning output for each tool, plus a pin on the doc command's `--workspace`. Delete the four mutations. Instant, and never selected by engine edits |
| `tests-silently-ignored` (floor), `tests-mod-dropped` (orphans) | 2 | ~1 min | The floor and the orphans walk | KEEP. The floor is what the AGENTS.md history cares about |
| `egui-use-ungated` | 1 | ~20 s | C24 | DELETE with the windowed-no-egui half (§ 1) |
| `windowed-module-ungated`, `winit-unconditional` | 2 | ~40 s | The bare engine stays windowless: the makepad host's link | KEEP |
| reference-port (`advance-zeroed`, `phantom-row`, `fixture-deleted`) | 3 | ~50 s | The fold against the oracle; a shrunken corpus refused | KEEP |
| pick-oracle (`pick-off-by-one`) | 1 | ~25 s | The independent pick oracle | KEEP |
| hyper-oracle, HyperLayout (`hyper-*`, 12) | 12 | ~4.6 min | C10, C13, C14, C15 put back, and device-only tiers | KEEP. These are what the oracle exists for |
| hyper-oracle, Visible kernel (`visible-span-first-byte-uncoloured`, `visible-keycap-lookahead-dropped`, `visible-tab-stop-by-column`) | 3 | ~1.2 min | The Visible kernel | **IF** (§ 5) |
| pixel-ab, plain-frame (`coverage-scale`, `depth-write-off`, `derived-placement-row-zeroed`, `sidecar-runs-unsorted`, `emoji-uv-flip`, `cluster-static-zero-off`, `resolve-group-tint-halved`) | 7 | ~3.4 min | The frames see their classes | KEEP. Each also pays both equivalents' renders until those go |
| pixel-ab, `instanced-coverage-scale` | 1 | ~30 s | That the instanced equivalent renders Instanced | DELETE with step 3 |
| pixel-ab, `visible-row-off-by-one` | 1 | ~30 s | That the visible equivalent renders Visible | **IF** (§ 5) |
| cargo-test, layout core (tabs, chunks, pass 1, pass 2 windows, float tables, `fu_to_world`, copy split, Derived lanes, `--render-file` tiling, view registry, empty load, curve cap) | 19 | ~9.5 min | One named test each, for a real defect each (C15, C16, C22, step A, 2026-10-11's curve cap) | KEEP. Prove by `--since` only |
| cargo-test, Visible (`wash-*`, `visible-slot-order-atomic`, `visible-stale-segment-tail`, `visible-item-cull-ignores-group`, `visible-wash-drops-uncounted`, `visible-verb-keyed-off-by-one-byte`, `visible-item-tab-width-dropped`, `seed-cut-resets-segment-advance`) | 9 | ~4.5 min | C28, C30, C32, M3 | **IF** (§ 5) |
| cargo-test, library and regions (`library-*`, `region-*`, `tree-cull-*`, `faces-update-drops-changed-face`, `regions-ignore-the-resolve`, `drag-delta-in-world-space`) | 18 | ~9 min | The library probe's E-findings | **IF** (§ 5) |
| cargo-test, scene and group (`lod-ignores-group-scale`, `group-adopt-takes-unchanged-appearance`, `group-rows-deferred-past-their-reader`, `group-flush-drops-changed-row`, `scene-graph-*`, `resolve-compose-order-swapped`) | 6 | ~2.5 min | The transform tree and flush the makepad host consumes | KEEP |
| cargo-test, agents and revisions | 9 | ~4.5 min | The gomuxai event reader and revision engine | KEEP. Prove by `--since` only |
| cargo-test, `derived-column-page-term-dropped` | 1 | ~30 s | Derived z's column-page term. It is witnessed today by `derived_vertex_stage_agrees_with_instanced` | KEEP, re-anchored at step 3 to the fold's z (the test compares against Instanced today) |

## 4. Removal plan, in order

Each step lands on its own with the battery green. Pixels must not move;
a step that would move a pixel is out of scope. Mutations removed with a
step are re-checked with `prove --since` on that branch.

1. **Retire the unscoped prove as the merge bar.** This is a process
   change plus one small runner change.
   The merge bar becomes `cargo glyph prove --since <merge-base>`. The
   unscoped prove runs when `glyph/src` or the toolchain changes, before
   the makepad cutover, and when Ivan asks. Add one selector to `--since`:
   a mutation is selected when the file holding the test named in its
   `expect` changed. Today a test edit that hollows a witness is not
   selected; that is the one real hole in `--since`.
   *Takes with it:* AGENTS.md's "the unscoped run remains the landing
   bar" sentence (§ The tool) and the matching line in
   `glyph-engine-testing/SKILL.md`. *Saves:* **~41 min → 0–13 min per
   merge** on cargo (today's merges: 0, 0, 6, 28, 31 mutations); less on
   relax once `--jit` lands.

2. **Move the gate-mechanism proofs into runner unit tests.** Delete
   `unused-binding`, `clippy-lint`, `dead-doc-link` and
   `dead-doc-link-crate`. Add a `gates.rs` test of `warn_count` over
   captured output from rustc, clippy and rustdoc (one warning each, plus
   a `generated N warnings` summary that must not count), and a pin on
   the doc command. *Saves:* ~1.3 min of a full prove, and stops
   `--since` selecting four irrelevant mutations on every fold or layout
   branch. *Takes:* nothing else; the gates stay.

3. **Delete the scan and bake forms.** Remove `native/src/scan.rs` (1,256
   lines) and `native/src/bake.rs` (950), the `--fixture-scan` and
   `--fixture-bake` instruments and their CLI and `main.rs` arms, the
   scan arm of `fixture.rs` (`diff_scan`), the 8 `*.bake.bin` fixtures
   and `gen-bake.mjs`, and the vendored `glyphBake.js` and
   `glyphPipelineScan.js`. Keep `glyphPipelineKernels.js`: the
   `real-kernels` fixture embeds its bytes as input text (`gen.mjs`
   :195), and nothing executes its import of the scan file. Also remove their
   `vendor-manifest.py` entries and `SHA256SUMS` lines, set
   `counts.bake` to 0 or drop it (the manifest, the parity script, the
   `bake.rs` pin and PROVENANCE all declare 8), and remove two of the
   five reference-port halves from build.toml's `compare` and from
   AGENTS.md. Keep the trie, fold and reference halves, and keep
   `fold.rs`'s comment that cites the scan's junction term as history.
   *Saves:* ~1.3 s per test run and ~2,300 lines that nothing ships.
   *Ask Ivan first* if the bake's seed protocol is wanted as a design
   reference for edit relayout. It stays in git history and in the JS
   repo either way.

4. **Instanced retirement, step 3** (already planned). Delete
   `crates/glyph-field-instanced`, the `--field-mode instanced` golden
   equivalent, `instanced-coverage-scale`, hyper-oracle's `RenderSlot`
   device tier, and the instanced wgsl test. Drop the Instanced runs from
   `render_file_copies`, `tab_stops`, `empty_load` and `view_registry`.
   Rewrite `derived_verbs` and `derived_vertex_stage_agrees_with_instanced`
   (re-anchoring `derived-column-page-term-dropped`). *Saves:* 2.8 s per
   pixel-ab run, which is also 2.8 s on every pixel-ab mutation, ~3 s of
   integration renders, a crate's compile, and 1 mutation.

5. **Delete repo-verify and repo-verify-direct.** Both are HyperLayout
   against itself and cannot be shown to fail (their own `blind_to`).
   Remove the two gates, the `RepoVerify` gate kind in
   `glyph/src/{gates,manifest}.rs`, and the `--repo-verify` instrument if
   nothing else calls it. The `--repo-engine` choices that all construct
   the same HyperLayout are a separate cleanup. *Saves:* 0.1 s, two
   uncovered lines in every prove report, and an AGENTS.md section.

6. **Tier the battery** (declared per gate in build.toml; the gatetime
   sibling owns the mechanism). Run orphans, cargo-clippy, cargo-doc,
   committed-artifacts, atlas-* and vendor-hashes at merge. Drop the
   `--features windowed` half of cargo-check-no-ui with `egui-use-ungated`,
   unless Ivan wants the egui-less window kept. *Saves:* ~30 s per change
   (orphans 22.4 s, clippy 3.0, doc 2.7, corpus ~2.2).

7. **Trim `tab_stops_agree_on_every_tier`** as in § 2. The same
   assertions, run on fewer configurations of the 135 KB texts.
   `hyper-tab-chunk-rewalk-skipped` must still redden it. *Saves:* ~7 s
   per change.

8. **The decisions in § 5.** Each one is a large cut if the answer is
   "not ported".

## 5. Decisions that are Ivan's (marked IF above)

The coordinator was asked about these three. Until there is an answer,
the plan assumes nothing.

- **The Visible field** (GPU compute layout per frame). The makepad port
  draws with no compute dispatch, so Visible cannot follow it. If it
  stays as a wgpu-reference feature, keep its checks at merge tier. If it
  retires, the cut is `crates/glyph-field-visible`,
  `layout_hyper/visible.rs`, hyper-oracle's Visible tier and span half,
  the visible golden equivalent (2.8 s per pixel-ab run and per pixel-ab
  mutation), `visible_verbs` and `visible_repeat`, and **13 mutations
  (~6.5 min of a full prove)**. Line-table seeds exist for Visible;
  check whether anything else reads them before cutting.
- **The library layout probe** (`library/`, regions, faces, tree cull).
  No golden renders it. Its unit tests and 18 mutations are its only
  witnesses: **~9 min of a full prove** and ~5 s of tests per change.
  If it is ported, these tests become the port's spec and stay. If not,
  delete it with the probe.
- **The winit window, egui overlay and launcher TUI.** If the makepad
  host replaces them, `windowed/`, the launcher's 17 tests and the
  `windowed` half of cargo-check-no-ui go. The time saved is small and
  the code removed is large.

## 6. Totals

| | today | after steps 1–7 | after § 5, if all three retire |
|---|---|---|---|
| per change (warm, one edit) | ~103 s | **~60 s**; ~40 s with gatetime's parallel test binaries | ~50 s / ~35 s |
| per merge | ~103 s + ~41 min prove ≈ **43 min** | ~95 s + `prove --since` (0–13 min on cargo) ≈ **2–15 min** | about the same; fewer mutations to select |
| full prove (on demand) | 116 mutations, ~41 min | 110 mutations, ~38 min | 79 mutations, ~23 min (less on relax) |
| fresh worktree | ~3 min | ~2.5 min (orphans/no-ui cold dirs at merge only) | |

**Kept on purpose, all cheap and load-bearing.** These are the layout
oracles (reference-port's fold, trie and reference halves; hyper-oracle's
HyperLayout tiers), the plain golden frames, pick-oracle, the bare
no-window check, and the corpus and atlas checks. Together they cost
about 15 s per change. They are what the makepad host's arithmetic and
pixels will be measured against. hyper-oracle has the best record of any
check in the repo. Its five defects were all found on the day it was
first pointed at the shipping engine, and nothing since. That is
evidence that an oracle pays when it is introduced, not that it has
stopped paying. The makepad host is the next new thing it will be
pointed at.

**Not measured here.** The gates were timed once each under shared load,
not as medians. The mutation costs come from the provejit sibling's runs,
not from a full prove. The § 6 "after" figures are sums of measured parts
minus what each step removes. They are not a run of the trimmed battery.
