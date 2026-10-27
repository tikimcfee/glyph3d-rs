# Test diet: which checks to delete or merge, and what each one costs (2026-10-26)

Branch `mp-test-diet` from `main` @ c56ae30. Written as a plan; Ivan then
decided, and part of it is executed on this branch (below). Steps 1 and 6
went to the provejit and gatetime siblings.

**Executed on this branch (2026-10-26):**
- `69861ce` scan.rs and bake.rs deleted (§ 4 step 3).
- `030d1cf` repo-verify and repo-verify-direct deleted, with the
  `--repo-verify` instrument (step 4).
- `d0b997f` the Visible field deleted **now**, not at wgpu's retirement
  (Ivan's decision; premise 1 below is superseded): the crate, the mode,
  hyper-oracle's visible tier (which makes that gate CPU-only), the
  visible golden equivalent and 14 mutations.
- `0fbdf5e` the line table deleted: Visible was its only consumer.
- Step 5, the tab-grid trim: done on mp-gatetime instead (`48ad388`, the 24
  cases in parallel, 2.7 s); this branch's trim (`7894114`) is reverted in
  `f0ebf90`. The Visible tier's removal alone took that test from 9.5 s to
  4.9 s here.
- Step 2, the warn_count unit test: proposed to gatetime (who owns
  `glyph/src/gates.rs`); not done here yet.

Ivan asked how much of the verification suite is repeated CPU work, and
whether it can be replaced with something more useful. Across the day's six
engine branches the gates caught bookkeeping (test-floor drift, a
mis-aimed mutation `expect`, one vacuous mutation) and no code bug. The
problems that mattered were found by reading, measuring and running the
code. This note prices every check, says what it alone can catch, and says
whether that failure is still possible. It plans which checks to DELETE or
MERGE.

**Not planned here.** The runner work belongs to the gatetime sibling
(`mp-gatetime`): the parallel gate DAG, parallel test binaries, declared
per-gate tiers, scratch dirs, and orphans re-derived from dep-info. Where
a verdict below says "merge tier", it means that gate should be
*declared* merge-tier once gatetime's mechanism lands.

## Premises (from the coordinator, 2026-10-26)

1. **Visible is wgpu-only.** makepad has no compute, and the port targets
   Derived alone (a makepad load past the Derived lanes is refused).
   Visible retires with wgpu. Nothing here keeps it past the port.
2. **Instanced is retiring, with no date.** It survives only as the wgpu
   fallback for loads past the Derived lanes. It retires with wgpu.
3. **The library layout is ported.** The makepad host already draws faces
   and regions, with region pick and drag verified. Its checks are the
   port's spec and stay.
4. **The winit window, egui overlay and launcher TUI are replaced** by
   makepad panels (`g3_ui.rs` on `mp-host`). They retire once that host is
   accepted, and their checks go with them.

## 0. Where the time goes

The per-gate figures are gatetime's, measured on main c56ae30 on the
32-core Linux host (`vulkan-nvidia` set). The runner is fully serial.
Each gate is given as cold / after a one-comment edit / no-op. I timed
the same gates once more in this worktree under shared load and got the
same picture within a few seconds. Per-mutation costs are provejit's
scoped cargo runs (`out/PROVE-ON-RELAX-2026-10-26.md` on `mp-prove-jit`).

| gate | cold | edit | no-op |
|---|---|---|---|
| products (release build) | 51 s | 19 s | 0.1 s |
| corpus gates (manifest, committed-artifacts, atlas ×2, vendor) | 1.7 s | 1.7 s | 1.7 s |
| cargo-build | 0.2 s | 0.2 s | 0.2 s |
| cargo-check-no-ui | 20 s | 1.7 s | 0.3 s |
| cargo-clippy | 13 s | 3.1 s | 0.2 s |
| cargo-doc | 3.2 s | 2.9 s | 0.2 s |
| orphans | 27 s | 24 s | 48 s |
| cargo-test: compile + run | 16 + ~30 s | 15.5 + ~30 s | 0.1 + ~30 s |
| pick-oracle | 4.2 s | 4.2 s | 4.2 s |
| hyper-oracle (4 runs) | 1.5 s | 1.5 s | 1.5 s |
| reference-port (5 halves) | 0.1 s | 0.1 s | 0.1 s |
| repo-verify ×2 (4 runs) | 0.1 s | 0.1 s | 0.1 s |
| pixel-ab (30 renders) | 7.6 s | 7.6 s | 7.6 s |
| **`cargo glyph test`** | **~172 s** | **~111 s** | **~95 s** |
| **`cargo glyph prove`, unscoped (116)** | | **41.4 min**: 454 s of mutant builds + 1,953 s of gates under mutation | |

The cargo-test run of ~30 s breaks down as 13.5 s of lib unit tests and
~16 s of GPU integration binaries. Within the lib tests,
`hyper_oracle::tests::tab_stops_agree_on_every_tier` alone takes 9.5 s.
That figure was measured here with `--exact`.

**The finding.** The layout oracles, corpus checks and pixel goldens cost
**~15 s** together. They are the cheapest part of the battery and the
part that guards what the makepad port must reproduce. The churn is in
two places:

- **`prove` re-proving all 116 mutations at every merge.** It takes
  ~22 s per mutation, and 81 % of that is the gate run under the mutation,
  mostly a cargo-test compile plus the whole suite.
- **Checks for wgpu-only code that is now on its way out.**

Delete and merge there, and leave the oracles alone.

## 1. The gates

Legend: **KEEP**, **MERGE** (fold into another check), **DELETE**, and
**RETIRES WITH X**, meaning the check is deleted together with the code it
checks. "Merge tier" is a tier to declare once gatetime's mechanism lands.

| gate | cost (edit) | what it alone catches (and what it was added for) | still possible? | verdict |
|---|---|---|---|---|
| manifest | 0 s | A product input that matches no file, so the currency stamp hashes nothing (`native/src/**`, 598205b → 2026-09-16; `native/Cargo.lock`, 2026-09-06 → 10-09) | yes, on every build.toml edit | KEEP |
| committed-artifacts | ~1.4 s | A generated file edited by hand, or a generator that changed behaviour (schema, emoji sheet, cluster table, emoji corpus, atlas, fixture corpus with declared counts) | yes, when those inputs change | KEEP (merge tier) |
| atlas-tables | 0.09 s | What `codepoints.bin`/`glyphs.bin` mean at the pins (D9) | yes, on an atlas re-export | KEEP (merge tier) |
| atlas-verify | 0.33 s | The curve and glyph-map textures against `glyphs.bin`, and the trie layout. The makepad float tables (`float_tables.rs`) read the same bins | yes | KEEP (merge tier) |
| vendor-hashes | 0.14 s | A vendored oracle input or font edited in place | yes | KEEP (merge tier) |
| products / cargo-build | 19 s | Zero warnings, and the binary every later gate reads | yes | KEEP |
| cargo-check-no-ui, **bare** half | < 1 s | The engine with no window: the library the makepad host links (2026-10-26) | **yes, and newly load-bearing** | KEEP |
| cargo-check-no-ui, `--features windowed` half | < 1 s | C24: egui used outside `egui-ui` (2026-10-09) | until the window retires (premise 4) | **RETIRES WITH THE WINDOW**, with `egui-use-ungated` |
| cargo-clippy | 3.1 s | Lints. C8 found 14 test-only lints, one deny-level, when `--all-targets` was turned on | yes | KEEP (merge tier) |
| cargo-doc | 2.9 s | Dead intra-doc links (`Engine::records`, 2026-09-07) | yes, on renames | KEEP (merge tier) |
| orphans | 24 s (48 s no-op) | A `.rs` file no `mod` reaches. Its realistic case, a dropped `mod tests;`, is also caught by the cargo-test floor, by count | rare | KEEP. Gatetime is re-deriving it from dep-info; if that stays slow, merge tier |
| cargo-test | 15.5 + ~30 s | See § 2 | yes | KEEP, trimmed (§ 2) |
| pick-oracle | 4.2 s | Picks and pixel-ray round trips against an independent Python fold. Catches a pick that resolves to the neighbouring glyph (`pick-off-by-one`), and the emoji double-advance rects (2026-09-10) | yes. Picking is CPU code the makepad host calls | KEEP |
| reference-port: trie, fold, reference halves | ~0.1 s | The Rust fold, trie and `text.rs` bit-exact against the JS oracle's recorded lanes (1,874,328 lanes). `phantom-row` is a shared fault that no fold-vs-fold comparison can see. hyper-oracle's truth reaches HyperLayout through `fold.rs` | **yes; this is the arithmetic the makepad port must reproduce** | KEEP |
| reference-port: **scan** half | (in 0.1 s) | `scan.rs`, the fold recast as a monoid scan: the dispatch structure of the retired GPU compute layout (CubeCL, retired 2026-10-09). **Nothing ships it.** Only `main.rs`'s instrument and `fixture.rs` call it | no. There is no GPU scan left to be faithful to | **DELETE** (§ 4 step 3) |
| reference-port: **bake** half | (in 0.1 s) | `bake.rs`, the streaming record and its seed protocol. **Nothing ships it.** Only `main.rs`'s instrument calls it, and Visible's line-table seeds are a separate implementation | no | **DELETE** (§ 4 step 3) |
| hyper-oracle, HyperLayout tiers (records, host Pass 2, device Pass 2 in the Derived format) | 1.5 s for all four runs | HyperLayout, the engine that ships, against the oracle-backed fold. **The most productive gate in the repo's history**: red on arrival, it found C10 (keycaps), C13 (cluster mode ignored), C14 (paging in both Pass 2s) and C15 (chunk-cut seeds, split sequences) | yes. The makepad host lays out through the same HyperLayout and emits Derived slots | KEEP |
| hyper-oracle, `RenderSlot` device tier | (inside) | The Instanced slot format, byte for byte | until Instanced goes | RETIRES WITH WGPU (premise 2) |
| hyper-oracle, Visible tier + span half | (inside, GPU) | The Visible kernel against the device Pass 2 | until Visible goes | RETIRES WITH WGPU (premise 1). Merge tier until then |
| repo-verify | 0.05 s | HyperLayout against HyperLayout (prefetched against inline Pass 1). Its own `blind_to`: no mutation can redden it, because since C15 both sides are one driver | no. It is a determinism check that cannot be shown to fail | **DELETE** |
| repo-verify-direct | 0.05 s | `direct` against recording. Same engine, same host path; "UNCOVERED in prove: no mutation separates the two sides" | no | **DELETE** |
| pixel-ab, plain (10 frames) | ~2.5 s | The only check that sees pixels. Found the `048d403` far-line cull regression and C17 during the 2026-10-09 re-adoption walk. `depth-write-off` and `emoji-uv-flip` are classes no numeric check can see | yes. **The end-to-end oracle until the makepad host reaches parity**, and then the reference the makepad set is compared against | KEEP |
| pixel-ab, `--field-mode instanced` | ~2.5 s, and the same again on every pixel-ab mutation | The Instanced fallback held to the same frames | until Instanced goes | RETIRES WITH WGPU. Merge tier until then |
| pixel-ab, `--field-mode visible` | ~2.5 s, and the same again on every pixel-ab mutation | The Visible field held to the same frames | until Visible goes | RETIRES WITH WGPU. Merge tier until then |

## 2. cargo-test, by binary and module

| class | cost | catches | still possible? | verdict |
|---|---|---|---|---|
| native lib: **`tab_stops_agree_on_every_tier`** | **9.5 s**, the lib binary's long pole. The binary takes 11.5–13.5 s; its other 306 tests sum to 14 s across threads | Tab stops in a TABBED line cut into chunks, on every tier: the one class no gate corpus reaches (`hyper-tab-chunk-rewalk-skipped`) | yes | KEEP, **trim**. It runs 2 widths × 2 cluster modes × 6 shapes × 4 texts, two of them 135 KB. Run the long texts only at the two pairings the hyper-oracle gate uses (width 4 cluster, width 8 leader). Est. 9.5 s → ~2.5 s, and the lib binary → ~4.5 s |
| native lib: layout core (`fold`, `layout`, `layout_hyper`, `hyper_oracle` grids, `text`, `glyph_trie`, `atlas`, `float_tables`, `fixture`, `seam`) | ~3 s | The unit grids for classes no corpus reaches: the pagination grid, windowed emission, line-table seeds, `fu_to_world` exact over u16, and the float tables' exact integers | yes, and these carry the makepad port's math | KEEP |
| native lib: `scan::tests` (8), `bake::tests` (4, one 1.1 s), the scan arm of `fixture` | ~1.3 s | The retired scan and bake forms | no | DELETE (step 3) |
| native lib: scene and group tables (`glyph_scene::{nodes,cull,pick,frame}`, `views`) | ~3 s; nodes 2.3 s is a GPU round trip | The CPU resolve, the flush, culling and LOD. The makepad host draws `FrameData` from these | yes | KEEP |
| native lib: library (`library::tests`, `glyph_scene::{regions,faces,tree_cull}`) | ~5 s; faces 3.7 s and regions ~4 s include wgpu round trips | The library's arithmetic, regions and tree cull. No golden renders the library | **yes: it is ported (premise 3)** | KEEP. When wgpu retires, re-point the GPU round trips (faces, the region draw) at the makepad host's tables |
| native lib: CLI, config, launch_config (51) | < 0.1 s | Flag parsing and config merging | yes | KEEP |
| native lib: launcher TUI (`launcher::tests`, `launcher::{save,draw}`, 17) | < 0.1 s | The TUI's controls and launch plan | until makepad panels replace it | RETIRES WITH THE TUI (premise 4) |
| native lib: `agent_transcript`, `revision` (25) | < 0.2 s | The gomuxai event reader against its vendored conformance corpus, and revision rebuilding. The makepad sessions modal reads the same | yes | KEEP |
| native lib: `spatial_scene`, `layout_stack` (24) | < 0.1 s | The bevy-ECS scene, zones and decks (still wired through `glyph_scene`, `relayout` and staging) | yes | KEEP |
| native lib: `gpu` (12) | < 0.1 s | The golden-set key per adapter | while goldens are wgpu frames | KEEP |
| crates: `glyph-field`, `-derived`, `-scene-graph` libs and their wgsl tests | < 0.1 s | Slot formats, the copy split (C16), stale handles, compose order, naga validation | yes | KEEP |
| crates: `glyph-field-instanced` (lib, wgsl) | < 0.1 s | The Instanced slot and shader | until Instanced goes | RETIRES WITH WGPU |
| crates: `glyph-field-visible` (`gpu` 0.8 s, `fp_probe` 0.2 s, lib, wgsl) | ~1 s | The Visible kernel against its CPU twin, C32's stale tail, and the device's float reassociation | until Visible goes | RETIRES WITH WGPU |
| integration: `visible_verbs`, `visible_repeat` | 2.4 / 1.25 s | M3 verb keys in pixels, and C30 frame-to-frame repeat | until Visible goes | RETIRES WITH WGPU. Merge tier until then |
| integration: `derived_verbs` | 3.4 s | Derived's verbs report what they cannot represent. It compares against Instanced | yes for Derived, which is the port's field | KEEP. Rewrite it to state the expected reply without the Instanced comparison when Instanced goes |
| integration: `view_registry`, `render_file_copies`, `tab_stops`, `empty_load` | 3.1 / 2.5 / 2.1 / 0.6 s | The group row is not the index (step A), `--copies` tiling, tab stops in pixels, the empty-load panic | yes | KEEP. Each renders in all three fields today; the Instanced and Visible runs (about 2/3 of their renders) can be merge tier now and go with wgpu |
| integration: `library_regions` | 1.1 s | Regions in pixels | yes (ported) | KEEP |
| integration: `curve_cap` | 0 s | The shader `MAX_CURVES` bound against the largest atlas outline. ☃ and ☠ were drawn cut off in Derived until 2026-10-11 | yes, and a makepad shader carries its own bound | KEEP. Point it at the makepad shader too when that exists |

## 3. Mutations, by group

Per mutation on the cargo route (provejit's scoped runs):

- **cargo-test:** 21–43 s. That is the gate's own compile plus the suite
  up to the first red binary; there is no separate mutant build.
- **renderer gates:** ~21 s of rebuild, plus the gate itself
  (hyper-oracle 1.4 s, reference-port 1.2 s, pixel-ab 8.5 s).
- **data mutations:** ~1 s.
- **once per run:** a clean-tree check for each gate (cargo-test 26–68 s)
  and a final rebuild (21–27 s).

Relax `--jit` cannot run the six compiler-diagnostic mutations,
`tests-silently-ignored`, the WGSL mutations on cargo-test, or any data
mutation.

**What a mutation is for.** It proves once that a check can fail. A
cargo-test mutation names the test it reddens. That test can only go
vacuous if the test, or the code it reaches, is edited. Re-proving all
116 at every merge pays 41 min to re-confirm 116 facts that the merge
mostly did not touch. `prove --since <ref>` already exists and selects
the right subset. Applied to today's merges (target file changed, or the
gate's script changed):

| merge | files | `--since` selects |
|---|---|---|
| mp-relax-fidelity | 8 | 0 |
| deps-ca-certificates | 10 | 0 |
| mp-tables | 4 | 6 |
| mp-cpu-twins | 38 | 28 |
| main into mp-tables | 48 | 31 |

That is 0–31 mutations instead of 116: 0 to ~11 min at the measured ~22 s
average, and less on relax.

| group | count | full-prove cost | what they prove | verdict |
|---|---|---|---|---|
| manifest, committed-artifacts, vendor-hashes, atlas-* | 12 | ~15 s | Corpus and tooling refusals | KEEP. Nearly free |
| Runner bookkeeping on cargo-test (`empty-test-selection-passes`, `prove-restale-blind`, `prove-ambiguous-find-accepted`, `since-matches-whole-gate-command`, `nested-alias-unstripped`) | 5 | ~1.8 min | The runner's own unit tests, for defects it once had | KEEP. Selected only when `glyph/` changes |
| **Gate-mechanism proofs anchored in engine files**: `unused-binding`, `clippy-lint` (both in `native/src/fold.rs`), `dead-doc-link` (`layout.rs`), `dead-doc-link-crate` (`glyph-field/src/lib.rs`) | 4 | ~1.3 min | That the runner COUNTS the warnings of rustc, clippy and rustdoc. That is a property of `warn_count` and the command line, not of `fold.rs`. Anchored in `fold.rs`, they are selected by `--since` on every branch that touches the fold. Relax cannot run them | **MERGE into one runner unit test**: `warn_count` over captured warning output of each tool, plus a pin on the doc command's `--workspace`. Delete the four mutations |
| `tests-silently-ignored` (floor), `tests-mod-dropped` (orphans) | 2 | ~1 min | The floor and the orphans walk | KEEP. Re-check `tests-mod-dropped` against gatetime's dep-info orphans |
| `windowed-module-ungated`, `winit-unconditional` | 2 | ~40 s | The bare engine stays windowless: the makepad host's link | KEEP |
| `egui-use-ungated` | 1 | ~20 s | C24 | RETIRES WITH THE WINDOW |
| reference-port (`advance-zeroed`, `phantom-row`, `fixture-deleted`) | 3 | ~50 s | The fold against the oracle, and a shrunken corpus refused | KEEP |
| pick-oracle (`pick-off-by-one`) | 1 | ~25 s | The independent pick oracle | KEEP |
| hyper-oracle, HyperLayout (`hyper-*`) | 12 | ~4.5 min | C10, C13, C14 and C15 put back, plus the device-only tiers | KEEP. They are what the oracle exists for |
| hyper-oracle, Visible kernel (`visible-span-first-byte-uncoloured`, `visible-keycap-lookahead-dropped`, `visible-tab-stop-by-column`) | 3 | ~1.1 min | The Visible kernel | RETIRES WITH WGPU |
| pixel-ab, plain-frame (`coverage-scale`, `depth-write-off`, `derived-placement-row-zeroed`, `sidecar-runs-unsorted`, `emoji-uv-flip`, `cluster-static-zero-off`, `resolve-group-tint-halved`) | 7 | ~3.4 min (~2 min once the two equivalents leave the per-mutation render) | The frames see their classes | KEEP. At wgpu's retirement the pixel gate moves to the makepad host's renders: a separate plan |
| pixel-ab, `instanced-coverage-scale` / `visible-row-off-by-one` | 2 | ~1 min | That each equivalent renders its field | RETIRE WITH WGPU |
| cargo-test, Visible (`wash-width-doubled`, `wash-depth-flat`, `visible-slot-order-atomic`, `visible-stale-segment-tail`, `visible-item-cull-ignores-group`, `visible-wash-drops-uncounted`, `visible-verb-keyed-off-by-one-byte`, `visible-item-tab-width-dropped`, `seed-cut-resets-segment-advance`) | 9 | ~4 min | C28, C30, C32, M3. Five of them are WGSL, which relax cannot run | RETIRE WITH WGPU. Check whether anything outside Visible reads the line-table seeds before cutting `seed-cut-resets-segment-advance` |
| cargo-test, layout core (tabs, chunks, Pass 1, Pass 2 windows, float tables, `fu_to_world`, copy split, Derived lanes, `--render-file` tiling, view registry, empty load, curve cap) | 19 | ~8 min | One named test each, each for a real defect (C15, C16, C22, step A, the 2026-10-11 curve cap) | KEEP. Prove by `--since` only |
| cargo-test, library and regions (`library-*`, `region-*`, `tree-cull-*`, `faces-update-drops-changed-face`, `regions-ignore-the-resolve`, `drag-delta-in-world-space`) | 18 | ~7 min | The library's E-findings | **KEEP**: the port's spec (premise 3) |
| cargo-test, scene and group (`lod-ignores-group-scale`, `group-adopt-takes-unchanged-appearance`, `group-rows-deferred-past-their-reader`, `group-flush-drops-changed-row`, `scene-graph-stale-handle-aliases`, `resolve-compose-order-swapped`) | 6 | ~2.5 min | The transform tree and flush the makepad host consumes | KEEP |
| cargo-test, agents and revisions | 9 | ~3.5 min | The gomuxai event reader and the revision engine | KEEP. Prove by `--since` only |
| cargo-test, `derived-column-page-term-dropped` | 1 | ~30 s | Derived z's column-page term. Its witness, `derived_vertex_stage_agrees_with_instanced`, compares against Instanced | KEEP. Re-anchor it to the fold's z before Instanced goes |

## 4. Removal plan, in order

Each step lands on its own with the battery green and no pixel moved.
Mutations removed with a step are re-checked with `prove --since` on
that branch.

1. **Retire the unscoped prove as the merge bar.** This is a process
   change plus one small runner change. The merge bar becomes
   `cargo glyph prove --since <merge-base>`. The unscoped run is kept for
   when `glyph/src` or the toolchain changes, before the makepad cutover,
   and when Ivan asks.
   - Add one selector to `--since`: a mutation is selected when the file
     holding the test named in its `expect` changed. Today a test edit
     that hollows a witness is not selected; that is the one real hole
     in `--since`.
   - *Takes with it:* AGENTS.md's "the unscoped run remains the landing
     bar" (§ The tool) and the matching line in the testing skill.
   - *Saves:* **41 min → 0–11 min per merge** on cargo (today's merges:
     0, 0, 6, 28 and 31 mutations), and less on relax.

2. **Merge the four gate-mechanism mutations into a runner unit test.**
   - Delete `unused-binding`, `clippy-lint`, `dead-doc-link` and
     `dead-doc-link-crate`.
   - Add a `gates.rs` test of `warn_count` over captured rustc, clippy
     and rustdoc output: one warning each, plus a `generated N warnings`
     summary that must not count. Pin the doc command's `--workspace`.
   - *Saves:* ~1.3 min of a full prove, and four mutations that `--since`
     would otherwise select on every fold or layout branch. None of the
     four can run on relax.

3. **Delete the scan and bake forms.**
   - **Rust:** `native/src/scan.rs` (1,256 lines) and `native/src/bake.rs`
     (950), the `--fixture-scan` and `--fixture-bake` instruments and
     their CLI and `main.rs` arms, and `fixture.rs`'s `diff_scan`.
   - **Fixtures and generators:** the 8 `*.bake.bin` fixtures and
     `gen-bake.mjs`.
   - **Vendored inputs:** `glyphBake.js` and `glyphPipelineScan.js`, with
     their `vendor-manifest.py` entries and `SHA256SUMS` lines. Keep
     `glyphPipelineKernels.js`: the `real-kernels` fixture embeds its
     bytes as input text (`gen.mjs` :195), and nothing executes its
     import of the scan file.
   - **Declared counts:** drop `counts.bake`. The manifest, the parity
     script, the `bake.rs` pin and PROVENANCE all declare 8.
   - **Docs:** take two of reference-port's five halves out of
     build.toml's `compare` and out of AGENTS.md. Keep the trie, fold and
     reference halves.
   - *Saves:* ~1.3 s per test run, two instruments, and ~2,300 lines that
     nothing ships.
   - Ask Ivan first whether he wants the bake's seed protocol kept as a
     design reference for edit relayout. It stays in git history and in
     the JS repo either way.

4. **Delete repo-verify and repo-verify-direct.** Their own `blind_to`
   says they cannot be shown to fail. Remove the two gates, the
   `RepoVerify` gate kind (`glyph/src/{gates,manifest}.rs`), and the
   `--repo-verify` instrument if nothing else calls it. The
   `--repo-engine` choices all construct the same HyperLayout; cleaning
   those up is separate. *Saves:* 0.1 s, plus two permanently uncovered
   lines in every prove report.

5. **Trim `tab_stops_agree_on_every_tier`** as in § 2: the same
   assertions on fewer configurations of the 135 KB texts.
   `hyper-tab-chunk-rewalk-skipped` must still redden it. *Saves:* ~7 s
   per change, since the lib binary is the suite's long pole.

6. **Declare the wgpu-only checks merge tier**, using gatetime's tier
   mechanism once it lands:
   - the `--field-mode instanced` and `--field-mode visible` golden
     equivalents;
   - hyper-oracle's Visible tier and span half (a separate hyper-oracle
     run, if the tier needs one);
   - `visible_verbs`, `visible_repeat` and the `glyph-field-visible` GPU
     tests;
   - the Instanced and Visible runs inside `view_registry`,
     `render_file_copies`, `tab_stops` and `empty_load`.

   They still run before every merge. They stop costing every change
   for fields the port does not have.
   *Saves:* ~5 s of pixel-ab, ~3.7 s of Visible integration tests, ~1 s
   of crate tests and ~5 s of the shared integration tests per change.
   On every pixel-ab mutation it saves ~5 s.

7. **When the makepad host is accepted** (premise 4), delete
   `windowed/`, the egui overlay and the launcher. Their checks go with
   them: cargo-check-no-ui's `--features windowed` half,
   `egui-use-ungated`, and the launcher's 17 tests. The bare no-window
   check stays.

8. **When wgpu retires** (premises 1 and 2), delete
   `crates/glyph-field-instanced`, `crates/glyph-field-visible`,
   `layout_hyper/visible.rs` and every check marked RETIRES WITH WGPU:
   the two golden equivalents, hyper-oracle's `RenderSlot` and Visible
   tiers, 14 mutations, and the integration tests above. Before that,
   rewrite `derived_verbs` and re-anchor `derived-column-page-term-dropped`.
   The plain golden frames and their 7 mutations then need the makepad
   host's own golden set. That is a separate plan, and it is the point
   where the wgpu frames stop being the oracle.

## 5. Totals

The wall-time figures are serial, as the runner is today. Gatetime's
parallel DAG and test binaries reduce every column further; this plan
reduces the work, not the scheduling.

| | today | after steps 1–6 | after steps 7–8 |
|---|---|---|---|
| per change (warm, one edit) | ~111 s | ~85 s of work: −7 s tab test, −1.3 s scan/bake, −~15 s wgpu-only checks to merge tier, −0.1 s repo-verify | about the same, with those checks gone outright |
| per merge | ~111 s + 41.4 min prove ≈ **43 min** | ~110 s + `prove --since` (0–11 min) ≈ **2–13 min** | the same, with fewer mutations to select |
| full prove (on demand) | 116 mutations, 41.4 min | 112, ~40 min | 97 (−1 window, −14 wgpu), ~33 min, and less on relax |

**Kept on purpose, all cheap and load-bearing:**
- the layout oracles: reference-port's fold, trie and reference halves,
  and hyper-oracle's HyperLayout tiers;
- the plain golden frames;
- pick-oracle;
- the bare no-window check;
- the corpus and atlas checks;
- the library's tests and mutations, now the port's spec.

The oracles and frames together cost ~15 s per change, and they are what
the makepad host's arithmetic and pixels will be measured against.
hyper-oracle has the best record in the repo. Its four defects were all
found on the day it was first pointed at the shipping engine, and nothing
since. That is evidence that an oracle pays when it is introduced, not
that it has stopped paying, and the makepad host is the next thing it
will be pointed at.

**Not measured here.** Gates were timed once each (by gatetime, and once
more in this worktree), under shared load, not as medians. The mutation
costs come from provejit's scoped runs and from gatetime's figure for
the last full prove. The "after" columns are measured parts minus what
each step removes. They are not a run of the trimmed battery.
