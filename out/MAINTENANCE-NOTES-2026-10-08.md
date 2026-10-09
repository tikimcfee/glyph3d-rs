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
| D2 | `cargo glyph` alias doubles inside `.claude/worktrees/` | open | pick a fix option (below) |
| D3 | `cargo glyph test engine` matched zero gates, printed ALL GATES GREEN | closed | 4d9de92: refused as NOTHING RAN; every verdict counts gates; `engine` scope gone |
| D4 | `.agents/rules/rust-engineering.md` claims the tree does not back | open | short pass |
| D5 | pixi carried the retired engine's tasks and mojo/max toolchain | closed | 16372a9 (tasks), 6954a9d (deps, lock re-solved; Mac env unsolved-installed) |
| D6 | Orphaned configs: `deny.toml` (nothing runs it) | open | gate or delete; mutants.toml fixed in 16372a9, fixture-parity script moved to D8 |
| D7 | Loose root docs: session handoffs, plus Mojo-era TOOLING-PLAN.md open items, BUILD-BRIEF.md, research/ surveys | open | move handoffs to `out/`; banner or trim the rest |
| D8 | Retired Rust checks went unwatched from 09-30; `overflow-leads.txt` read by nothing | closed | 0336e85, 48a75d9, 5257df9 (reference-port, repo-verify x2, cubecl-chain/-fork, new hyper-oracle; 15 gates), 50c7085 (chain green) |
| D9 | `engine-trie.bin` (+ `gen_real_trie.py`, `--engine-trie`) is committed and gated but read by no Rust code | open | decide: keep or retire (changes committed-artifacts) |
| C1 | Twin field crates ~60% shared; five copies of the mapped-buffer upload | open | hoist into `glyph-field`; last, largest |
| C2 | Oversized files; dead kernels in `cubecl_chain/position.rs` | open | delete dead kernels; split `glyph/src/main.rs` |
| C3 | 46 `#[allow]`, one justified | open | pass, with C2 |
| C4 | 63 `unwrap()` in `cubecl_chain/repo/dispatch.rs`, one shape | open | one accessor or non-optional fields |
| C5 | `spike_vertex_yz.rs`, 908 lines, own CLI flag, superseded | open | delete |
| C6 | Rust comments describing the Mojo/FFI backend as live | closed | 7a99b5a (+ `--fixture-manifest` deleted, help text and runtime labels fixed) |
| C7 | 239 stage-letter comments | won't do | archaeology; reword only when touching |
| C8 | `clippy --all-targets`: 14 test-only lints + deny-level `reversed_empty_ranges` (seam.rs test) | open | sweep; gate runs without `--all-targets` |
| C9 | `discovery.rs` names a Claude project by the slug's last `-` segment (`…-glyph3d-js` → `js`) | open | small fix |
| C10 | Two defects attributed (detail). Defect 2 (chain) FIXED in c5ef78a, it was the instrument. Defect 1: HyperLayout vs the oracle, now measured by the `hyper-oracle` gate (red), with two more HyperLayout defects (detail) | open | fix HyperLayout clusters with a perf A/B on the M2 |
| C12 | CubeCL's standalone cluster pass (`--cubecl-cluster-check`, bench cluster mode; NOT the repo path's fused decode_probe) misses keycap/overlap/wrap sequences and ZWJ families; fails identically at d08af8b | open | instrument-only; fix or retire the standalone pass |
| C13 | HyperLayout ignores the item's cluster mode (leader-mode ZWJ zeroed; 29,826 records differ on the cluster corpora in leader mode) | open | fix with C10 defect 1 (same resolver) |
| C14 | HyperLayout's host Pass 2 paginates `scroll_rows`/`page_cols` differently from the fold (instances only; recording path agrees) | open | latent for repos (they set neither); fix after C10 |
| C11 | Mutation `find` strings also match their own entry in build.toml; correct only because the target comes first | open | small: anchor them, or have the prover refuse an ambiguous find |
| P1 | pixel-ab red on both platforms since 10-07; Linux set a month stale, 2 views never adopted | Ivan's call | Mac re-baseline first, then Linux re-adoption |
| X1 | Experiments' Zed symlink scheme never built against real Zed | next up | needs a Zed checkout or the Mac; fieldzed's dylib build.rs deleted in 6a0b669 |
| X2 | Missing clones here: `viz-web/glyph3d-js` (flagship corpus, `GLYPH_WEB`), Zed | next up | discuss |
| X3 | `just` not installed here; justfile `profile` recipe unrun | open | install or accept |
| R1 | Renderer currency stamp ignored the root `Cargo.toml`/`Cargo.lock` | closed | dc6a00f |
| R2 | Dead `{dylib}` / `dylib_ext()` / engine-check machinery in the runner | closed | 3fc716c |
| R3 | `engine/glyph_schema.mjs` and the schema validation were in no gate; `gen_schema --check` red | closed | 16372a9 (`[artifact.glyph-schema]`, mutation `glyph-schema-byte`) |
| B0 | Workspace did not compile on Linux (ungated Metal HAL) | closed | 872621d on main (other agent); helper half is C1 |
| M1 | Mac paths in TUI presets, justfile, bench script, docs | closed | 1062e23, 88525ea |
| M2 | Session discovery hardcoded `$HOME`; tests not hermetic | closed | 1062e23, then f864183 (app defaults + Kimi parser) |
| M3 | Committed `launch_config.toml` was a personal preference file | closed | 1062e23 (now `.example`) |
| M4 | Experiments hardcoded a Mac Zed path | closed | 9351a8d (unbuilt: X1) |
| M5 | mise `.venv` not gitignored | closed | 88525ea |

## Landed on this branch

None pushed or merged to main; that is Ivan's call.

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

Main's build fix (872621d, the other agent's) was fast-forwarded in first.

**On the Mac after merging:**

```sh
cp launch_config.example.toml launch_config.toml
ln -s ~/localdev/externalcompute/zed experiments/zed
export GLYPH_FLAGSHIP_REPO=~/localdev/viz-web/glyph3d-js
```

## Suggested order for what is open

1. (D8 done 2026-10-09.) C10 next: the hyper-oracle gate is the fence a
   HyperLayout fix lands against.
2. D7, D4, D6: the remaining doc and config tidy.
3. C5: delete the spike.
4. C2 + C3 + C4: cubecl cleanup, once C10 says whether the CubeCL path is
   right; its fence is what proves the cleanup output-neutral.
5. C1: hoist the field machinery. Largest; wants a fresh battery.

For discussion: C10, D9, X1, X2, P1. Small, fit anywhere: C8, C9, C11, X3.

---

## Open items: detail

### D2. `cargo glyph` alias doubles in worktrees — [measured]

Cargo merges `.cargo/config.toml` from every ancestor directory and `[alias]`
arrays concatenate. From `.claude/worktrees/<name>`:

```
glyph   alias: run --quiet --release -p glyph -- run --quiet --release -p glyph --
```

The tool takes `run` as its verb and launches the renderer, which rejects
`--quiet`. Workaround: `cargo run --quiet --release -p glyph -- <verb>`, or
`target/release/glyph <verb>` once built. `just check` is broken too.

**Options:** (a) document worktrees outside the repo directory
(`git worktree add ../glyph3d-rs-wt/<name>`); (b) justfile recipes call the
expanded command; (c) the tool strips a repeated alias prefix from argv.
(a)+(b) recommended. Claude Code's own worktrees live under `.claude/worktrees/`,
so (a) only helps human-made ones; weigh (c) for that reason.

### D4. rust-engineering.md — [measured]

- §1 claims `clippy::pedantic` "configured in `Cargo.toml`": no `[lints]`
  table anywhere. Add it (expect a large red) or delete the sentence.
- §2 "never `println!`": the runner's 124 `println!` are its contract. Say
  diagnostics go through tracing, verdict lines are presentation.
- §8 names `replace_file_content`, an Antigravity tool name.
- §5 "no new shell scripts" beside a gate that is a shell script.
- Reconcile with `native/AGENTS.md` by pointing, not restating.

### D6. Orphans — [measured]

- `deny.toml`: `cargo-deny` is in mise; nothing runs it. Gate or delete.
### D7. Loose root docs — [measured]

`SESSION-HANDOFF.md` (2026-10-02, quotes floor 158),
`PLAN-AGENT-STACKS-FOCUS-LOCKING.md` (2026-10-02),
`cubecl-performance-handoff.md`: session artifacts; AGENTS.md says notes go
in `out/`. Move with dates in the filename (their links are repo-relative since
88525ea and will need `../`). Mojo-era, from the 2026-10-09 inventory:
`TOOLING-PLAN.md` is a "plan of record" whose open items (`engine/check.sh`,
`--engine-check` cases, `ffi_selftest`, a gate list with engine-check and
engine-suites) are void, keep its "verify the artifact that ships" rule;
`BUILD-BRIEF.md` (29 hits) self-marks as historical but its stale list omits
the retirement; `research/wasm-port-audit.md`, `native-rendering-stack-comparison.md`,
`rust-to-web-target-notes.md` are Mojo-era surveys (banner or move).

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
| 3643 | `native/src/cubecl_chain/position.rs` (dead kernels `derive_stride` :2332, `paginate` :2366) |
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
or non-optional fields. Others: `glyph/src/main.rs` 9, `spike_vertex_yz.rs` 7
(goes with C5), `pass2_device.rs` 4, a handful of 1–2s.

### C5. `spike_vertex_yz.rs` — [measured]

908 lines, `pub mod`, `--spike-vertex-yz`. Superseded by the Derived mode
crate (`99aea85`, 2026-10-05). No gate or mutation references it. Delete with
the flag and its `cli/command.rs` arm.

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

### P1. Pixel baselines — [measured]

See "Pixel attribution" below. Main needs a deliberate Mac re-baseline (text,
repo-zoom at least, looking at every frame) in a commit that says why, then a
full Linux re-adoption including `emoji-cluster` and `repo-cluster`. Until
then pixel-ab cannot prove any refactor on this host; the stand-in is
byte-comparing renders against main built in scratch (used for every commit
on this branch).

---

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
- `cargo glyph` is broken in worktrees (D2); use the expanded command.
- Byte-comparing renders against main: build main with `git archive` into the
  scratchpad, render the nine views from `native/` with `--screenshot`, `cmp`
  against `out/tooling-ab/sweep/`.

### Remote runs on the M2

`rx.sh` (session scratchpad, `rx/`): uploads a job script and runs it on
`airlugo` over ssh, either synchronously (`run`) or in tmux
`remote-rs-space:claude` (`job`, then `wait`). Everything remote lives under
`~/localdev/claude-remote/` on the Mac; work happens in a scratch clone there
(`glyph3d-rs/`, cloned from Ivan's checkout, which is never written), fed by
`rx.sh push <rev> <branch>`. Jobs must be bash 3.2-safe. The runner sets the
Homebrew PATH, which a non-login ssh shell lacks.

### M2 performance: harness and noise floor (2026-10-09)

`rx/jobs/perf-ab.sh`: both refs built into separate target dirs, sides
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

### Checked and fine

- No token, key or credential in the tree or in `git log --all -p`; no
  hostname; the only email is the pixi `authors` line.
- Every gate invocation passes `--screenshot`, so no launch config reaches a
  golden or a pick.
- `CHUNK_THRESHOLD_BYTES` (64 KiB, tuned to M-series L1) is documented as
  such; whether it suits the 5090 box is a benchmark question. The cubecl
  65535 grid cap is the WebGPU default limit, so portable.
