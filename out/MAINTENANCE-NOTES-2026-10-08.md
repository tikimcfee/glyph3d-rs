# Maintenance notes — scratchpad (started 2026-10-08)

Working notes for the `maintenance-and-things` branch, on the Linux box
(RTX 5090, Vulkan). Started as a survey of main at `5809677`; now the running
status of the pass.

**How to edit this file.** The status board is the source of truth. Item IDs
are stable: never renumber, never reuse. When an item closes, set its row to
`closed`, name the commit, and delete its detail section (the commit message
carries the evidence). Record a hash in a FOLLOW-UP commit: amending a
commit to name itself changes the hash it names. New findings get the next free ID in their group.
Every claim is **[measured]** (a command ran) or **[inferred]** (read, not
run); never promote the second kind without running it.

Groups: **D** docs and tooling, **C** code shape, **P** pixels, **X** other
machines and clones, **M** machine-specific values, **R** runner.

---

## Status board

| ID | Item | Status | Commit / next step |
|---|---|---|---|
| D1 | Root and native AGENTS.md described 16 gates and the retired engine as live | closed | 3fc716c (root), 7a99b5a (native) |
| D2 | `cargo glyph` alias doubles inside `.claude/worktrees/` | closed | the runner strips the doubled alias (strip_doubled_alias, pinned to .cargo/config.toml by a test); nested-alias-unstripped. A string alias was tried and reverted (c64ac55/3029cc3): array-vs-string ancestors make cargo refuse its config |
| D3 | `cargo glyph test engine` matched zero gates, printed ALL GATES GREEN | closed | 4d9de92: refused as NOTHING RAN; every verdict counts gates; `engine` scope gone |
| D4 | `.agents/rules/rust-engineering.md` claims the tree does not back | closed | facts corrected, intent kept: no pedantic config (the gate is clippy --all-targets), stdout verdict/report lines are a contract not logging, new checks are gates (several are tools/*.sh), no tool-specific edit API; points to AGENTS.md as canonical |
| D5 | pixi carried the retired engine's tasks and mojo/max toolchain | closed | 16372a9 (tasks), 6954a9d (deps, lock re-solved; Mac env unsolved-installed) |
| D6 | Orphaned configs: `deny.toml` (nothing runs it) | closed | deny.toml and mise's cargo-deny deleted (no use for it yet; research/rust-tooling-survey-2026.md keeps the case for it) |
| D7 | Loose root docs: session handoffs, plus Mojo-era TOOLING-PLAN.md open items, BUILD-BRIEF.md, research/ surveys | closed | three session handoffs moved to out/ with their dates (links fixed); TOOLING-PLAN, BUILD-BRIEF and three Mojo-era research surveys bannered historical |
| D8 | Retired Rust checks went unwatched from 09-30; `overflow-leads.txt` read by nothing | closed | 0336e85, 48a75d9, 5257df9 (reference-port, repo-verify x2, cubecl-chain/-fork, new hyper-oracle; 15 gates), 50c7085 (chain green) |
| D9 | `engine-trie.bin` (+ `gen_real_trie.py`, `--engine-trie`) is committed and gated but read by no Rust code | closed | engine-trie.bin + its path plumbing (--engine-trie, default_engine_trie, LayoutGlyphs::load_trie_file, repo load params, LiveSource/PickContext fields) retired; the generator's checks became the atlas-tables gate (tools/check_atlas.py) and a Rust domain sweep on fu_to_world |
| C1 | Twin field crates ~60% shared; five copies of the mapped-buffer upload | closed (e9721e4, ee2c92f, 8368508) | storage + upload (`SlotStorage<S>`, `upload_host_slots<T: Transcode>`), pipelines/bind groups/draws (`FieldCore`) and the Metal mapped slot buffer (`create_mapped_slot_buffer`, shared with `layout_hyper/device_alloc.rs`) live once in `glyph-field`; byte-equal on both machines in BOTH field modes, macOS clippy clean. CubeCL's readback buffer differs in usage and waits on the GPU decision |
| C2 | Oversized files | open (two done) | the runner's main.rs (1,795) -> 8 modules (a1ebe0c) and tui.rs (2,036) -> tui/{state,terminal,draw} (7168fc0), both checked as pure moves (code-line multisets equal). Left: `layout_hyper/pass2_device.rs` (~1,800, of which `layout_pass2_chunk` is ONE ~1,250-line function — the hot loop; splitting it needs an M2 A/B per step), `fixture.rs`, and `text.rs` / CubeCL's `position.rs`, which wait on the colouring and GPU decisions |
| C3 | `#[allow]`s | open (8 of 23 gone) | RowFrame + layout_pass2_chunk reading EmitInputs (8e8abd5), ResolveCtx (c6ac877): neutral on both machines (M2 inside the A/A floor in all four configs). The walk STATE stayed `&mut` locals on purpose. 15 left outside CubeCL: scan.rs 4 (reference port), repo.rs 3, and one each in layout_hyper.rs, pass2_device.rs (`emit`, kept on purpose: a constructor), hyper_oracle.rs, offscreen.rs, windowed.rs, agent_transcript/staging.rs, spatial_scene/{spawner,turn_card}.rs. Take them as their files are next touched |
| C4 | 63 `unwrap()` in `cubecl_chain/repo/dispatch.rs`, one shape | closed (c78ee46) | moot: CubeCL retired 2026-10-09 |
| C5 | `spike_vertex_yz.rs`, 908 lines, own CLI flag, superseded | closed | deleted with --spike-vertex-yz; it ran its OWN inline WGSL and ItemParamsGpu, never the production Derived shader, so no coverage was lost (the Derived vertex-stage Y/Z stays a battery blind spot) |
| C6 | Rust comments describing the Mojo/FFI backend as live | closed | 7a99b5a (+ `--fixture-manifest` deleted, help text and runtime labels fixed) |
| C7 | 239 stage-letter comments | won't do | archaeology; reword only when touching |
| C8 | `clippy --all-targets`: 14 test-only lints + deny-level `reversed_empty_ranges` (seam.rs test) | closed | 14 test-only lints fixed (the deny-level one was a deliberately inverted range, now Range { start, end }); the clippy gate runs --all-targets |
| C9 | `discovery.rs` names a Claude project by the slug's last `-` segment (`…-glyph3d-js` → `js`) | closed | generalized: every session carries the workspace its PROVIDER recorded (Claude cwd, Antigravity workspaceUris/cwd, Kimi state.json cwd); one project_name_of for all; no storage-folder parsing |
| C10 | CubeCL fences red on both machines: two defects | closed | chain: c5ef78a (the instrument, not production); HyperLayout ASCII-led sequences: d427276, device page extent 341cad8, device-path oracle 2602c85, gates 9a100aa/ac75e9e. cubecl-fork green (708,529 -> 0). M2 A/B: no measurable cost (below) |
| C12 | CubeCL's standalone cluster pass (`--cubecl-cluster-check`, bench cluster mode; NOT the repo path's fused decode_probe) misses keycap/overlap/wrap sequences and ZWJ families; fails identically at d08af8b | closed (c78ee46) | moot: CubeCL retired 2026-10-09 |
| C13 | HyperLayout ignored the item's cluster mode | closed | d427276 (mode read once per item; leader-mode oracle run gated) |
| C14 | HyperLayout's host Pass 2 paginates `scroll_rows`/`page_cols` differently from the fold (instances only; recording path agrees). The device Pass 2 too (measured 2026-10-09, device tier). Gated alone as `hyper-oracle-paged` (red) so hyper-oracle could go green | closed | fixed 2026-10-09 (`row + scroll`, unscrolled `y_page`, `x_page` dropped without `page_rows`, device line path never turning a column page, Pass 1 stride after the last advance): one paginate in `layout_hyper/page.rs`; split folded back into hyper-oracle, 4 mutations; no golden pixel moved. Repo loads ARE paged (`page_rows`), only scroll/cols unset. c8bc75d (fix), 9573862 (gate). M2 A/B with C15/C16: inside the A/A floor |
| C15 | Device Pass 2 only: a line cut into chunks (> 64 KiB, no newline within the next 64 KiB) seeds the next chunk's segment advance as `rem * adv` (`aggregate_chunk_prepasses`), not the running f32 sum the fold and host path use: an ulp of x on `g-pick-repo/wide.txt`, 65 slots past column 65,280 [measured 2026-10-09, `--hyper-oracle-check native/fixtures/g-pick-repo`]. Also [inferred from source, no corpus has one]: a sequence split across such a cut is not clustered by the chunked passes (Pass 1, device Pass 2) while the host Pass 2 and the recording path, which walk the whole item, do cluster it, so Pass 1's survivor count can disagree with the host emission there | closed | re-derived and both claims CONFIRMED 2026-10-09 (seed: 20 + 45 = the 65 slots, two of wide.txt's three cuts; split: a constructed family/keycap/flag input, device emitted pieces and the host arena committed 3 unwritten slots). Fixed: cut only before an ASCII byte (atlas asserts the invariant), seed = running f32 sum, continued lines measured for the stride; `chunk-cut.txt` + g-pick-repo in hyper-oracle, 3 mutations. Pixels: repo-down and repo-back-oblique 11 px each, syntax PAINT at the moved cut (device colorizer restarts per chunk), not layout. 7458bec (fix), 4fd3970 (gate + fixture), e1d8690 (empty-tail guard). Re-confirmed after the merge: the same 22 px vs main@164af5c. Golden re-baseline of those two views is P1 (Ivan) |
| C17 | Device Pass 2 restarted its syntax colouring at every intra-line chunk cut (wide.txt: 578 glyphs disagreed with the whole-item colouriser) | closed | e20cb81 (colourisers: word-into-string rule, truncated-lead panic, unterminated last word; a test that compared the fast path with itself), af49b0d (cut lines coloured once over the whole line; comment-line indentation), 7a556fd (hyper-oracle PAINT tier, chunk-cut-paint.txt, 5 mutations). Pixels vs main@164af5c: repo-down 281, repo-back-oblique 747, repo-wide 20 px, all from af49b0d (measured); golden re-baseline is P1. M2 A/B vs 9baa679: syntax medians 178/178 (derived), 230/232 (instanced). Prove 40/40 provable |
| C18 | cubecl-fork went red ONCE inside a full battery at b42aed2 (output not captured), then passed 5 times standalone and in the next full battery [measured 2026-10-09]. Intermittent; cause unknown — a GPU-side race would matter for the CubeCL decision | closed (c78ee46) | moot: CubeCL retired 2026-10-09 |
| C19 | Derived `write_placements` writes whole slots with `row = 0, wrap = 0` (it has no row to write), so every whole-placement edit in Derived mode moves the glyphs to row 0 of their item: the highlight sidecar (`--highlight`, the AST/LSP colouring entry point) and the recolor-line verb. [measured 2026-10-09: `--highlight` over g-pick-repo/alpha.rs at `--focus-file alpha.rs --zoom 3`: Instanced recolours, Derived renders an empty frame, same at ff44d31; cull stats identical in all four runs (1 draw, 67 instances), and a colour-only write makes Derived byte-equal to Instanced — the cause is the row/wrap write, not culling.] Invisible to the battery: no golden runs Derived or `--highlight` | closed (c17e31b) | fixed: write only the fields a placement can change in Derived (colour, X, group; glyph id needs the wrap half preserved), or split a recolour-only edit from a placement edit in the trait. Witness: `repo-highlight` under the Derived equivalent, `derived-placement-row-zeroed`. Making the frame also found the sidecar parser never sorted runs that `style_file` walks as ascending (fixed, `sidecar-runs-unsorted`) |
| C20 | No golden renders the Derived field, so its upload, storage, vertex-stage Y/Z and edit paths are pixel-blind (C19 is the proof). But every golden view renders BYTE-EQUAL in both modes [measured 2026-10-09, all 9 views, on vulkan-nvidia and on metal-apple] | closed (c17e31b) | `[settings] golden_equivalents = ["--field-mode derived"]`: a Derived pass of pixel-ab against the SAME baseline sets (no new adoption, no re-baseline): each view again with `--field-mode derived`. Costs one more render per view |
| C21 | `glyph prove` takes ~45 min for 50 mutations (2026-10-09; Ivan: "we've gotta find a way to make those mutations run faster") | closed (ed4a570) | timed, then halved: 1375 s -> 724 s for 51 mutations (clean-tree check once per gate: 171 -> 28 s; restore rebuilds deferred to the next gate that reads the product and once at the end: 566 -> 8 + 16 s). What remains is mostly mutant builds (467 s) |
| C22 | Host staging (f47cb39) made Pass 2 itself ~25 ms slower on the discrete path (62 -> 88 ms derived, 95 -> 122 instanced, 102 MB tree): it now writes freshly allocated cached host memory (first-touch faults + read-for-ownership) instead of write-combined streaming. Backend still 482 -> 208 | closed (3fa7f6c) | windowed direct-to-staging emission is the discrete default: derived backend 194 -> 121 ms (482 this morning), instanced 307 -> 158; host and single kept behind GLYPH_STAGING |
| C23 | No mutation targets `layout_hyper/device_alloc.rs`: the staging paths are witnessed only by pixel-ab, and only on a discrete-GPU host (the M2's goldens load through unified memory) | partly closed (3fa7f6c) | the windowed emission core has a host-neutral witness (windowed_emission_matches_unwindowed + 2 mutations); the GPU sink itself (staging maps, VRAM copy offsets) is still seen only by pixel-ab on a discrete host |
| C24 | `glyph3d-native` does not build with `default-features = false`: `egui` is used ungated in `windowed/state.rs` (found by the GPU research agent, 2026-10-09) | closed | egui uses gated (ui_rgb; poll_live reads the pose only with egui); new gate cargo-check-no-ui (cargo check --no-default-features --all-targets, own target dir) + mutation egui-use-ungated |
| C25 | gpu.rs requests MAPPABLE_PRIMARY_BUFFERS on every adapter that offers it (wgpu warns "massive performance footgun" on discrete GPUs, every load on the Linux box); only Metal's direct upload uses it. Pre-existing | open | request it only where used (Metal + the direct upload), then A/B a discrete load; expected neutral (no mappable storage buffers are created on discrete) |
| C26 | The debug panel's LOD controls blur two ideas Ivan wants separate (2026-10-10): TEXT DETAIL (greek_onset_px, default 10: below it the shader fuzzes glyph windings for fewer ALU ops; hidden behind the greeking checkbox) and SHOW GLYPHS (lod_min_px, default 1: below it a file segment is its backdrop rectangle; in visible mode also the glyph-vs-wash boundary, with lod_backdrop_px 0.25 as the file-rectangle tier). All three are labelled "px/em" (= on-screen pixels per text row) | closed (5f196ae) | two always-visible panel sliders, "Text detail" (greek_onset_px) and "Show glyphs" (lod_min_px), visible mode's "File rectangle" handle under the latter; order text detail >= show glyphs >= file rectangle kept by clamping; [lod] show_glyphs_px / text_detail_px and --show-glyphs-px / --text-detail-px, old names as aliases; no default or pixel moved. The DRAG (clamp wiring) is unverified by hand: Ivan's by-eye check |
| C27 | The launcher TUI lives in `glyph` and starts the renderer through translated command-line flags: two parsers of launch_config.toml with different rules, the file applied twice, cargo shell-outs to run itself, option spellings in three places | decided, not started | move the launcher into the renderer over its own CLI/config model; launch by spawning itself with the typed config file. Plan + steps: out/PLAN-LAUNCHER-INTO-RENDERER-2026-10-10.md |
| C28 | Visible mode: at the greeking-level LOD (maybe another tier), extremely long lines that should wrap BACK in depth instead run straight off the right of the view, "as if that mode isn't applying the same wrap logic" (Ivan, testing 2026-10-10) | open, for the visible-mode delegate | EVIDENCE (Ivan, 2026-10-10): the overhang appears ONLY in the purple layer of the debug tint — purple is `TINT_WASH_FULL` (cull-state tint: a line under the glyph threshold drawn as a full wash; in the LOD tint washes are blue). So the runaway lines are washes. Reading of the code (unverified by a run): `cull_lines` passes the CULL bound as the wash's drawn extent — `wash_w = local.hi.x - local.lo.x`, where `line_local_box` sizes a wrapped line at `min(len, 2 * fold_unit)` cells (bytes, doubled to bound double-advance cells — conservative on purpose for culling) — and `visible_wash.wgsl` draws one flat quad at segment 0's row lane/z. Under `--wrap-mode back` (the default) the glyphs recede one fold unit wide per segment in z; the wash instead stays at the front depth, up to twice the column wide: the overhang. WrapDown stacks `rows` and is likely only too wide. Fix direction: draw the wash at the line's true extent (at most a fold unit per segment) and one quad per back-wrap segment (or a quad spanning segment 0..n-1 in z), keeping the 2x bound for the cull only. The glyph tier is held to Pass 2 by hyper-oracle, the wash tier by nothing |
| C29 | Visible mode: a file dragged to overlap another's receding column (long.md sliced into wide.txt's back-wrap stretch) renders fragments of one file at the other's positions — a floating `long.md` line mid-air, a truncated band — while the HUD reads `items 1/5 visible` with two files on screen. Screenshot: out/visible-overlap-2026-10-10.png (taken on a pre-C26 build) | open, for the visible-mode delegate | hypotheses, unverified: (a) after a move, the per-ITEM cull uses the new bbox (M3's set_item_bbox) but the per-LINE cull and/or the layout kernel still place lines from the item's ORIGINAL origin/params while the draw applies the group transform — cull and draw disagree about where the file is; (b) the cull compacts visible items and something indexes by compacted position instead of the item index. Repro: g-pick-repo, --field-mode visible, grab long.md (G) and drop it into wide.txt's column; compare --field-mode derived at the same pose ("copy pose") |
| C16 | Discrete-GPU upload: an ODD total survivor count costs ~10-20 ms of backend on this host (derived, 94 MB tree: 91,417,858 slots 415-425 ms, 91,417,859 slots 430-437 ms; same at bf9a757) [measured 2026-10-09]. Cause: a buffer copy whose size is off 16 B runs whole at ~half speed (RTX 5090/Vulkan), and the staging path copies the 20 B-slot stream twice (wgpu-core's staging at `unmap`, then ours); odd counts paid ~+10 ms per copy, counts = 2 mod 4 ~+3 ms. Instanced (32 B) never paid | closed | staging padded to 16 B, copies split into a 16-aligned body + tail (`glyph_field::copy`); odd = even after (derived medians 414 vs 415 ms, A/A spread 4 ms), VRAM bytes and 18 golden renders identical. e59e4a6 (fix), 3d2a50f (mutation rebuild), b90da50 (notes). M2 A/B vs a76aeaf: every config inside the A/A floor (derived syntax 174/174 ms median; the unified path makes no copy). Left: chunked Derived buffers start at 8 mod 16 (~+1 ms, measured with a forced split) |
| C11 | Mutation `find` strings also match their own entry in build.toml; correct only because the target comes first | closed | prover refuses a find that matches more than once; 3 mutations anchored (2 matched their own build.toml entry, 1 picked the first of 7 #[test]s); prove-ambiguous-find-accepted |
| P1 | pixel-ab red on both platforms since 10-07; Linux set a month stale, 2 views never adopted | closed | 21f9aef (metal-apple: walked on the M2 from the set's own commit to the tip, every moved pixel named — d1b0f7e camera, 8b28f1f the C17 colouring, 048d403 a culling regression repaired by 9c96ad1, 9c96ad1 palette + '#'/block comments, tip C15/C17), d4cb1a4 (vulkan-nvidia re-adopted after it; drift vs Metal 0 clustered in all nine; all 9 incl. emoji-cluster/repo-cluster). pixel-ab green here; the 4 pixel mutations prove (prove coverage 44/44 provable) |
| X1 | Experiments' Zed symlink scheme never built against real Zed | next up | needs a Zed checkout or the Mac; fieldzed's dylib build.rs deleted in 6a0b669 |
| X2 | Missing clones here: `viz-web/glyph3d-js` (flagship corpus, `GLYPH_WEB`), Zed | next up | discuss |
| X3 | `just` not installed here; justfile `profile` recipe unrun | open | install or accept |
| R1 | Renderer currency stamp ignored the root `Cargo.toml`/`Cargo.lock` | closed | dc6a00f |
| R2 | Dead `{dylib}` / `dylib_ext()` / engine-check machinery in the runner | closed | 3fc716c |
| R3 | `engine/glyph_schema.mjs` and the schema validation were in no gate; `gen_schema --check` red | closed | 16372a9 (`[artifact.glyph-schema]`, mutation `glyph-schema-byte`) |
| R4 | `glyph prove` left a MUTATED renderer behind a stamp reading current, for a cargo-test mutation without `rebuild` (`cargo test --release` links the bin) [measured 2026-10-09: tail-pads-zero, 62b55bfe -> 928dc5b4, "renderer current"]; the same for the runner, which later gates are spawned from | closed | runner fix + `prove-restale-blind` mutation; runner mutations declare `rebuild` f966e65 |
| R5 | The renderer product built at PACKAGE scope (`cd native && cargo build`), then cargo-build/cargo-test rebuilt it at workspace scope with unified features (crypto-common/std, indexmap/default): the stamp vouched for one binary and every later gate ran another [measured: 62b55bfe vs 6dc7340c] | closed | product builds at workspace scope; glyph/Cargo.toml is an input f966e65 |
| B0 | Workspace did not compile on Linux (ungated Metal HAL) | closed | 872621d on main (other agent); helper half is C1 |
| M1 | Mac paths in TUI presets, justfile, bench script, docs | closed | 1062e23, 88525ea |
| M2 | Session discovery hardcoded `$HOME`; tests not hermetic | closed | 1062e23, then f864183 (app defaults + Kimi parser) |
| M3 | Committed `launch_config.toml` was a personal preference file | closed | 1062e23 (now `.example`) |
| M4 | Experiments hardcoded a Mac Zed path | closed | 9351a8d (unbuilt: X1) |
| M5 | mise `.venv` not gitignored | closed | 88525ea |

## Landed on this branch

Pushed to origin/main through d3cf141 on 2026-10-09 (Ivan's call); later rows are on the branch until the next push.

| Commit | What |
|---|---|
| 6eb2d76 | these notes |
| 1062e23 | launch config drives repo presets and session discovery; `.example` config; hermetic TUI tests |
| 88525ea | `GLYPH_FLAGSHIP_REPO` / `GLYPH_WEB` env vars; repo-relative doc links |
| 9351a8d | experiments reach Zed through the `experiments/zed` symlink |
| f864183 | session discovery defaults to each app's locations (`crates/glyph-session-dirs`); Kimi Code parser |
| dc6a00f | renderer currency hashes root manifest and lockfile; `validate()` refuses dead literal inputs |
| 4d9de92 | `glyph test` refuses an empty gate selection (NOTHING RAN); verdicts count gates; `engine` scope removed |
| 3fc716c | root AGENTS.md rewritten against the nine gates; engine-check and `{dylib}` machinery out of the runner |
| 16372a9 | schema gated (`glyph-schema` artifact); generator's Mojo half, dead pixi tasks, ignores, mutants excludes removed |
| 6954a9d | pixi: mojo/max toolchain dropped, lock re-solved (fontTools env only) |
| 6a0b669 | experiments: fieldzed build.rs (asserted the engine dylib) deleted |
| 7a99b5a | Mojo/FFI removed from native comments, help text, native/AGENTS.md; `--fixture-manifest` deleted |
| 228ff6d | board after the retired-engine series |
| 6f6f004 | merge main (large-dataset fixes); floor 237; renders byte-identical to main@164af5c |
| c5ef78a | CubeCL chain check fixed (26595fb broke the instrument, not production) |
| 0336e85..5257df9 | five checks re-gated + hyper-oracle (helper agent); 15 gates, floor 240 |
| 50c7085 | cubecl-chain green, mutation proven |
| e59e4a6, f966e65, c8bc75d..e1d8690 | C16 odd-count upload; R4/R5 runner (prove re-stales, workspace-scope product); C14 pagination, C15 chunk cuts |
| e20cb81..7a556fd | C17: colourisers agree, cut lines coloured whole, hyper-oracle paint tier + chunk-cut-paint.txt |
| 21f9aef, d4cb1a4 | both golden sets re-adopted with every moved pixel attributed (P1) |
| d6e060d | merged main@3a6f65f (settings defaults + overrides, ground environment, transcript fixes): battery ALL GATES GREEN 15/15, pixel-ab byte-equal on vulkan-nvidia and metal-apple |
| 06e5245..48edfc9 | D2 runner strips the doubled alias (string alias tried + reverted), C11 unambiguous mutation finds, C9 session project from the provider-recorded workspace |
| 9cb0e47..87c3e66 | C5 spike deleted, C2 dead paginate kernel, aded165 public-repo path scrub, D9 engine-trie retired → atlas-tables gate (16 gates), D6 deny.toml gone |
| 865ee62..2a9e95b | D7 docs moved/bannered, C8 clippy --all-targets, D4 rules corrected, C18 logged |
| ff44d31..d3cf141 | handoff notes; C1 parts 1-2 (storage/upload, FieldCore); C19/C20 logged |
| 8368508 | C1 part 3: one Metal mapped slot-buffer helper |
| c17e31b | C19 fixed, sidecar run sort, golden_equivalents (Derived pass), repo-highlight frame + both baselines |
| 5d461b0 | notes: C1/C19/C20 closed (pushed to origin/main) |
| a1ebe0c, 7168fc0 | C2: runner main.rs -> modules; tui.rs -> tui/ |
| 8e8abd5, c6ac877 | C3: RowFrame + EmitInputs chunk args; ResolveCtx (perf-neutral, both machines) |
| 659efdb | notes: C2/C3 progress, C21, GPU research delegated (pushed) |
| c78ee46 | CubeCL retired (report: out/GPU-DIRECTION-2026-10-09.md); 14 gates, 48 mutations, floor 270 |
| f47cb39, a24f410, eb5241c | host staging default; staging stream trace; parallel host->staging copy |
| 3fa7f6c | C22 windowed direct-to-staging emission (discrete default), Pass 1 has_emoji, Pass 2 range/merge split |
| ac6c69f | C24 no-UI build fixed + cargo-check-no-ui gate |
| ed4a570 | C21 prove 2x faster |
| 32b3924 | merge of the visible-mode branch: M1 line table (+ Pass 1 kept separate per mode, A/B'd on both machines), Derived column-page and group-lane fixes |
| 0c56d03 | visible mode M2 merged: `--field-mode visible` end to end, hyper-oracle sixth tier (kernel == Pass 2, both GPUs), lane-limit fallback; pixel-ab 20/20 on both machines |
| 041d38d..15f7c79 | visible mode M3: overrides/spans/selection/verbs/picking by (item, byte), oracle span half, pixel witness; 314 tests |
| 5f196ae | C26 LOD panel split (Text detail / Show glyphs), config and CLI renames with aliases; 319 tests |

Main's build fix (872621d, the other agent's) was fast-forwarded in first.

**On the Mac after merging:**

```sh
cp launch_config.example.toml launch_config.toml
ln -s <path-to-zed-checkout> experiments/zed
export GLYPH_FLAGSHIP_REPO=<path-to-glyph3d-js-checkout>
```

## Next session: handoff (2026-10-10, tip of this branch = origin/main)

**State.** Battery ALL GATES GREEN, 15 of 15 (300 tests, floor 300); pixel-ab
20/20 (10 views, plain and under `--field-mode derived`) on BOTH vulkan-nvidia
and metal-apple; unscoped `glyph prove` reddens all mutations (~12 min since
C21). Everything is pushed: this branch and `origin/main` are the same commit.
(Ivan's own `main` checkout may lag `origin/main`: `git merge --ff-only
origin/main` there.)

**Landed 2026-10-09/10** (details in the landed table above): C1-C3 refactors;
C2 splits of the runner and the TUI; CubeCL retired (report:
`out/GPU-DIRECTION-2026-10-09.md`); discrete staging 482 -> 121 ms derived
backend on the Linux box (host staging, parallel copy, C22 windowed
direct-to-staging emission); C19 Derived edit bug, sidecar sort, golden
equivalents (Derived pass), repo-highlight frame; C21 prove 2x faster; C24
no-UI build + gate; and the VISIBLE field mode (M1 line table, M2
`--field-mode visible` end to end with a sixth hyper-oracle tier holding the
GPU kernel to Pass 2 on both GPUs; `out/VISIBLE-MODE.md` is its "how to see
it" log).

**In flight: the visible-mode delegate** (a Fable agent in its own worktree,
branch `worktree-agent-a164e953c6cdd7fc6`). It implements milestone by
milestone and reports after each; I review and merge (it never pushes). Next:
**M3** — re-key the slot-addressed consumers by (item, byte): GlyphField verbs
and `read_slot_words`, overrides, selection mask, picking, culling. Then M4
(`golden_equivalents` gains `--field-mode visible`). M5 (default flip) is
Ivan's call. Rules it works under: production code allowed on its branch;
full battery green per commit; every milestone ships something Ivan can SEE
in the open app (live mode switch, F8 HUD, `--debug-tint lod|cull`, poses via
"copy pose"), with the commands in the commit message. The M2 machine is
available to it (helpers in `target/scratch/helper-tools/rx/`, untracked).
If a session ends with it mid-milestone: check its worktree for uncommitted
files (by mtime; the worktree guard blocks `git -C`), then resume it with
SendMessage and a state summary.
**2026-10-10:** that delegate's context ran out mid-M3 (it handed back at
`041d38d` with 28 uncommitted files: crate and oracle work verified, the
renderer group unbuilt). A FRESH Fable agent was launched to finish M3 in the
same worktree from that handback; it reports commits, battery, mutations and
the M2 verdict. Ivan's LOD answer is relayed to it (px/em thresholds
confirmed); C26 waits until it is idle.
**M3 DONE and merged (2026-10-10):** the delegate's state was moved into THIS
worktree (merge of its scaffold 041d38d + its 28 uncommitted files copied
byte-exact), because the finishing agent inherited this session's isolation
and could not commit elsewhere. It committed f408b75 (crate), b103517
(oracle span half), e9a64f8 (renderer: selection, verbs, picking, dump by
(item, byte)), 15f7c79 (docs). Verified: battery 15/15, 314 tests, on both
machines; `--frozen` here on the committed tree; mutations
visible-span-first-byte-uncoloured, visible-verb-keyed-off-by-one-byte and
`--changed` (7/7) redden; the visible_verbs pixel witness matches Derived at
0 px on both GPUs. Declared crate gaps (for M4 or a fix-up): a selection past
mask_capacity is truncated silently; prepare_mask unguarded against a second
call per frame; verb group rows never freed. The delegate's old worktree
(.claude/worktrees/agent-a164e953c6cdd7fc6) is a STALE copy of committed work:
safe to discard (Ivan's call; the worktree guard keeps sessions out of it).
NEXT for visible mode: C26 (handed to the finishing agent), then M4 (visible as
a golden equivalent: its recolour frame is already byte-identical to Derived's
on both rasterizers).
**Ready to send after compaction — Ivan's testing report (2026-10-10):** visible
mode works "fantastically" on large and small repos, Linux "better than I'd
imagined"; two oddities found, C28 (long lines run off the right at the
greeking-level LOD instead of wrapping back) and C29 (an overlapped, moved file
renders fragments at the other file's positions; screenshot in out/). Hand
both to the agent that finished M3 and C26 (it knows this code; resume it with
SendMessage if it is still reachable, else a fresh Fable agent with this
section and the two rows as its brief). Ask for: a reproduction of each
(offscreen with --cam-pose where possible), the root cause stated before any
fix, a witness that would have caught it (C28: the wash tier is held to
nothing — a wash-vs-glyph extent check; C29: a moved-item case in the visible
oracle or the visible_verbs witness), the fix, the battery, and the M2 run.
Ivan may add more oddities before it is sent: append them here.
- C28 update (Ivan, 2026-10-10): the overhang is only in the purple (full
  wash) debug-tint layer — washes, as hypothesised; the row carries the code
  reading (the wash draws the cull's 2x-fold-unit bound, flat at segment 0's
  depth). Confirm with a run, then fix.

**Queued:**
- **C26** (LOD panel: separate "Text detail" from "Show glyphs") — hand to
  the delegate when it reports M3 and is idle; Ivan wants it on the hard
  parts first.
- **C27 — the launcher moves into the renderer.** Decided with Ivan;
  plan, facts to validate and steps in
  `out/PLAN-LAUNCHER-INTO-RENDERER-2026-10-10.md`. For a fresh session:
  validate the plan's facts against the code, then implement in its steps.
  Touches the TUI, `launch_config.rs` and `native/src/cli/`, none of the
  delegate's M3 files.

**On hold by decision:** retiring the load-time syntax heuristic and the
re-baseline that goes with it (after the new algorithms are agreed); the
`pass2_device.rs` split (until visible mode settles, to avoid merge pain).
**Small open items:** C23 (the GPU staging sink is still pixel-only), C25
(MAPPABLE_PRIMARY_BUFFERS requested on discrete; probably neutral, one A/B).

**Working conventions** (also in memory): test depth by change size; public
repo (no personal paths/hosts/session names in tracked files); keep pixel-ab
green and re-baseline only on Ivan's say-so, every moved pixel attributed;
interleaved A/Bs with load reported; the M2 is fanless (cool-downs).

---

## Open items: detail

### C1. Twin field crates — [measured]

`crates/glyph-field-instanced/src` 911 lines, `-derived` 880. Differing lines
per file: upload.rs 158, field.rs 142, pipeline.rs 92, storage.rs 39; mostly
the slot type name. Hoist into the `glyph-field` contract crate: the upload
paths generic over `T: Pod`, `SlotStorage<T>` with `write_colors`, the chunking
arithmetic. Includes one generic `create_mapped_hal_buffer<T>` gated once on
`target_os = "macos"` (the type exists only under Metal; the runtime choice
stays on `GpuProfile::mappable_primary_buffers`), retiring the five copies B0
had to gate one by one.

### C2. Oversized files — [measured]

| lines | file |
|---|---|
| 3643 | `native/src/cubecl_chain/position.rs` (dead `paginate` kernel removed 2026-10-09) |
| 1921 | `glyph/src/tui.rs` |
| 1659 | `native/src/layout_hyper/pass2_device.rs` |
| 1658 | `native/src/fixture.rs` |
| 1572 | `glyph/src/main.rs` (splits into `manifest.rs` + commands) |

Line counts are from 2026-10-08. As of 2026-10-09: tui.rs 2034, main.rs 1577.

### C3. `#[allow]` — [measured]

- `cubecl_chain/mod.rs`: 8 × `dead_code` on `ITEM_DESC_*_PAD*` constants.
- `cubecl_chain/repo/dispatch.rs:33, :162`: unused `sync_prof`, `record_sync`.
- `position.rs`: `unused_assignments`.
- `manual_range_contains` (8), `len_zero` (3) in `#[cube]` bodies: if the
  macro forces it, say so once at module top.
- `too_many_arguments` (20): `pass2_device.rs` ×5, `scan.rs` ×4, `repo.rs` ×3;
  a params struct per module.

### C4. `unwrap()` — [measured]

63 of 134 non-test unwraps are `buf.h_<name>.as_ref().unwrap()` in
`cubecl_chain/repo/dispatch.rs` (from :215). One accessor with one `expect`,
or non-optional fields. Others: `glyph/src/main.rs` 9
(goes with C5), `pass2_device.rs` 4, a handful of 1–2s.

### C10. CubeCL disagrees with the CPU path on this host — [measured 2026-10-09]

- `tools/check-cubecl.sh chain`: 4 of 5 fixtures PASS; `cluster-flags` FAILS
  with 9 record mismatches (4.14e-1 position deviation, 9.0e-2 x-extent).
- `tools/check-cubecl.sh fork` (STRICT, the IMMUTABLE fork corpus): counts
  match, 0 placement and 0 tint mismatches, but 314,405 of 314,686 slots
  differ (708,529 lane words). The mismatches read as a one-glyph shift: slot
  287 has `gi 5264 vs 17` and the chain's x equals the CPU's x one advance on.
- `--repo-engine cubecl` vs `hyper`, same camera: `g-pick-repo` 65 px differ;
  the fork corpus 295,357 px. Visible on screen.
- Same on the M2 at main@164af5c (Metal, run 2026-10-09 via `rx`): chain
  `cluster-flags` 9 record mismatches, fork 708,529 lane words / 314,405
  slots, hyper vs cubecl renders differ on both repos. Identical numbers to
  Linux, so NOT a Vulkan or discrete-GPU effect: a deterministic,
  cross-platform divergence in the code. Also unchanged by merging main's
  large-dataset fixes (6f6f004).
- **Attributed (M2, 2026-10-09)** — two separate defects:
  1. **Fork (CubeCL vs HyperLayout) — divergence entered WITH HyperLayout.**
     The fork fence compared CubeCL against the Mojo engine and was green
     until 2026-09-30. At 28bc7be (Mojo removed, HyperLayout becomes the
     reference) it fails with 1,113 record mismatches, and identically at
     c331464, 41dc330 and 5e94de8 (gate retired, red). e141724 (HyperLayout
     added, Mojo still present) does not build without the Mojo dylib. So
     CubeCL agreed with the oracle-validated engine and HyperLayout did not
     [inferred from the gate's history].
     Mechanism [inferred from source, matches the first mismatch]:
     `layout_hyper/char_resolve.rs` `resolve_byte_char_cluster` returns the
     `fast_byte_table` entry for every ASCII byte without asking
     `starts_a_sequence`, and `atlas.rs` fills that table for all ASCII except
     newline and static-zero codepoints. Keycap sequences (`0️⃣` = 0x30 VS16
     U+20E3) start with an ASCII byte, so HyperLayout lays out `0` as text
     and the keycap mark as a separate glyph (fork slot 287: chain gi 5264
     double-advance, hyper gi 17 = `0`; slot 288: hyper gi 4431). Unverified:
     why `#️⃣`/`*️⃣` agree between the two.
  2. **Chain (CubeCL vs scan.rs, `cluster-flags`) — regression in 26595fb,
     FIXED in c5ef78a, and it was the INSTRUMENT, not production.** 26595fb
     moved "this byte is a committed cluster head" into a device-only flag
     (`F_CLUSTER_HEAD`) set by `cluster_mark` on the repo path; the chain-check
     driver uploads CPU flags that never carried it, and a literal 136.0
     bitmap advance replaced the trie's. It also left three instruments
     reading a buffer it removed (cluster-check, decode-check advance, the
     bench's cluster verify) and the bench timing the flags buffer as text.
     Production launches `apply_and_emit`; the fixed `scan.rs::apply` runs
     only in the check and bench. Fork numbers and cubecl renders unchanged.
     Original finding:
     (2026-10-06, "eliminate 776MB intermediate VRAM buffers with inline trie
     evaluation in scan and emit"). Bisected on the M2, 7 steps: parent
     d08af8b PASS, 26595fb FAIL (9 record mismatches); PASS at every earlier
     step back to 5e94de8.
- The fork failure grew from 1,113 record mismatches (09-30) to 708,529 lane
  words (now); the check moved from records to slot lanes in between, and
  26595fb may add to it. Not separated.
- **hyper-oracle gate (2026-10-09, red)** [measured]: HyperLayout vs
  `fold.rs` (oracle-validated) over the 26 fixtures + cubecl-fork +
  g-cluster-repo + overflow-leads: 6 of 29 corpora differ, 377 / 475,234
  records. First divergence `cluster-keycap` byte 0 (fold gi 5264, hyper gi
  18) — defect 1 confirmed against the oracle corpus, whose own keycap
  fixture the fold clears. `#️⃣` diverges too (cubecl-fork byte 560), so
  the chain-vs-hyper agreement on `#`/`*` above is not agreement with the
  oracle. Two more HyperLayout divergences, independent of CubeCL:
  (a) it ignores the item's cluster MODE (leader-mode ZWJ zeroed in
  `cluster-zwj`; `--cluster-mode leader` on cubecl-fork + g-cluster-repo:
  29,826 records differ); (b) its host Pass 2 paginates `scroll_rows` and
  `page_cols` unlike the fold (`paged-rows`, `paged-cols`, `scroll-only`;
  instances only — its recording path `rederive.rs` agrees). Latent for
  repos, which set neither.
- Consequences (as first written; the gate above now exists): HyperLayout is the default engine and nothing compared it to
  the JS oracle (the `--fixture-*` instruments test fold.rs/scan.rs/text.rs).
  `repo-cluster`'s Metal golden dates from 2026-09-20 (05a5935), BEFORE
  HyperLayout, so it pins the Mojo-era cluster rendering: a Mac pixel-ab run
  of that view is an independent witness for defect 1.
- **Fixed 2026-10-09** [measured]: defect 1 and C13 in `char_resolve.rs`
  (the fast-table entry carries `seq_lead`, derived from the table; a
  `seq_lead` byte looks one byte ahead only in cluster mode, and only a
  non-ASCII successor takes the slow path; the mode is read once per item).
  hyper-oracle gained the device Pass 2 tier, which found the device page
  extent dropping the newline record (fixed) and C15 (open). After:
  hyper-oracle green (C14 split out to `hyper-oracle-paged`), cubecl-fork
  green, every golden frame byte-identical to bf9a757's renders on this host
  (repo-cluster included: g-cluster-repo has no keycap, so that view is a
  witness of HyperLayout's other cluster classes, not of defect 1).


## Reference

### Pixel attribution, 2026-10-08 — [measured]

pixel-ab here: 7 of 9 views diverge from `vulkan-nvidia`, 2 have no set.

- `text.png`: every differing pixel is the comment colour, `[106,153,85]` in
  the golden vs `[125,200,115]` fresh: `palette::COMMENT` in `9c96ad1`
  (2026-10-08), not re-baselined. Red on the Mac too.
- `repo-zoom.png`: field shifted down-right against both the Metal and old
  Linux goldens (which agree). Byte-equal under hyper/batch and both field
  modes, so the camera moved: `d1b0f7e` (2026-10-07) recentres the focus
  camera on ink extents. Red on the Mac too [inferred by reading].
- `repo-wide`, `repo-down`, `repo-back-oblique`, `emoji`: the Metal set was
  re-baselined 2026-10-02 and 10-05; the Linux set never was.
- Control: the tree at `99aea85` built here renders `text.png` byte-identical
  to the old Linux golden, ≤16 levels off Metal on 13,571 px (known edge
  noise). Its `--load-repo` panics on Linux (`layout.rs:588`), so repo views
  could not be controlled the same way.

### Measuring traps

- `cargo run -p glyph` re-serializes `Cargo.lock` before the runner hashes
  it, undoing a whitespace perturbation. Measure product currency with
  `target/release/glyph` directly.
- `cargo glyph` works in nested worktrees since D2; `target/release/glyph <verb>` is the fallback.
- Before R4 (2026-10-09), `glyph prove` on a `cargo-test` mutation WITHOUT
  `rebuild` left the mutated renderer in `target/release` under a current
  stamp. Fixed; on an older runner, `cargo build --release` after a prove.
- A renderer's sha256 depends on build SCOPE: `cd native && cargo build` and
  a root `cargo build` link different bytes (feature unification, R5). Hash
  binaries built the same way, or the comparison measures the scope.
- Byte-comparing renders against main: build main with `git archive` into the
  scratchpad, render the nine views from `native/` with `--screenshot`, `cmp`
  against `out/tooling-ab/sweep/`.

### Remote runs on a second machine (Apple Silicon Mac)

A small local helper (kept out of tree) uploads a job script over ssh and runs
it on the Mac, either synchronously or in a tmux window, then polls for its
exit. Everything remote lives under one scratch root with its own scratch
clone, fed by `git push <rev>:<branch>`; the owner's checkout there is never
written. Jobs must be bash 3.2-safe (macOS `/bin/bash`), and a non-login ssh
shell lacks the Homebrew PATH, so the runner sets it.

### Apple M2 performance: harness and noise floor (2026-10-09)

The A/B job: both refs built into separate target dirs, sides
alternate (order flips each round), 20 s cool-down between sides (the M2 is
fanless and throttles back-to-back), 6 rounds x 3 runs per side per config,
cold run discarded; flagship corpus (`viz-web/glyph3d-js`). A/A at bf9a757,
same binary both sides, warm runs, ms:

| config | backend min a/b | median a/b | visual min a/b | median a/b |
|---|---|---|---|---|
| derived flat | 133 / 140 | 146 / 145 | 140 / 147 | 154 / 152 |
| derived syntax | 170 / 172 | 173 / 175 | 180 / 183 | 183 / 185 |
| instanced flat | 216 / 207 | 226 / 226 | 223 / 213 | 234 / 232 |
| instanced syntax | 223 / 223 | 236 / 228 | 233 / 233 | 248 / 239 |

So a real difference has to exceed ~8-9 ms (about 4%); derived/syntax is
the tightest (~2 ms).

**A/B of the C10/C13 fix** (a = bf9a757, b = e21315a, the fix branch tip;
same harness, 2026-10-09):

| config | backend min a/b | median a/b | visual min a/b | median a/b |
|---|---|---|---|---|
| derived flat | 134 / 138 | 146 / 146 | 141 / 144 | 154 / 154 |
| derived syntax | 171 / 171 | 182 / 178 | 181 / 181 | 192 / 190 |
| instanced flat | 216 / 218 | 222 / 230 | 222 / 224 | 229 / 236 |
| instanced syntax | 222 / 222 | 229 / 229 | 233 / 234 | 239 / 240 |
| instanced flat, 10 rounds (n=20) | 217 / 216 | 223 / 222 | 223 / 222 | 229 / 228 |

Every difference is inside the A/A floor; the one at its edge (instanced flat
median +8) vanished at n=20. Verdict: no measurable cost on the M2.

**A/B of C14 + C15 + C16 + R4/R5** (a = a76aeaf, b = e1d8690; same harness,
2026-10-09):

| config | backend min a/b | median a/b | visual min a/b | median a/b |
|---|---|---|---|---|
| derived flat | 138 / 139 | 144 / 144 | 144 / 146 | 151 / 150 |
| derived syntax | 171 / 172 | 174 / 176 | 181 / 182 | 186 / 186 |
| instanced flat | 214 / 216 | 222 / 222 | 222 / 222 | 230 / 228 |
| instanced syntax | 225 / 224 | 229 / 230 | 235 / 234 | 239 / 240 |

Every difference is 2 ms or less. Verdict: no measurable cost on the M2.
After the merge, an unscoped `glyph prove` showed 40 of 40 provable mutations
reddening; the 4 pixel-ab mutations are unprovable while that gate is red
(P1). Uncovered: repo-verify, repo-verify-direct, cubecl-fork. The helpers'
`gen_chunk_cut.py` (chunk-cut.txt's generator, uncommitted on purpose) and
`ab3.py` (the Linux interleaved A/B) are kept in `target/scratch/helper-tools/`.

### Checked and fine

- No token, key or credential in the tree or in `git log --all -p`; no
  hostname; the only email is the pixi `authors` line.
- Every gate invocation passes `--screenshot`, so no launch config reaches a
  golden or a pick.
- `CHUNK_THRESHOLD_BYTES` (64 KiB, tuned to M-series L1) is documented as
  such; whether it suits the 5090 box is a benchmark question. The cubecl
  65535 grid cap is the WebGPU default limit, so portable.
