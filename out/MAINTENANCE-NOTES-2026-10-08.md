# Maintenance notes — scratchpad (started 2026-10-08)

Working notes for the `maintenance-and-things` branch, on the Linux box
(RTX 5090, Vulkan). Started as a survey of main at `5809677`; now the running
status of the pass.

**How to edit this file.** The status board is the source of truth. Item IDs
are stable: never renumber, never reuse. When an item closes, set its row to
`closed`, name the commit, and delete its detail section (the commit message
carries the evidence). New findings get the next free ID in their group.
Every claim is **[measured]** (a command ran) or **[inferred]** (read, not
run); never promote the second kind without running it.

Groups: **D** docs and tooling, **C** code shape, **P** pixels, **X** other
machines and clones, **M** machine-specific values, **R** runner.

---

## Status board

| ID | Item | Status | Commit / next step |
|---|---|---|---|
| D1 | Root AGENTS.md documents 16 gates, build.toml has 9; dead read-next links; Mojo/dylib prose | open | rewrite against build.toml; takes C6, R2 with it |
| D2 | `cargo glyph` alias doubles inside `.claude/worktrees/` | open | pick a fix option (below) |
| D3 | `cargo glyph test engine` matches zero gates, prints ALL GATES GREEN | open | refuse an empty gate set + mutation; drop the scope |
| D4 | `.agents/rules/rust-engineering.md` claims the tree does not back | open | short pass, with D1 |
| D5 | `pixi.toml` tasks for a Mojo engine that no longer exists | open | retire or trim to fontTools |
| D6 | Orphaned scripts and configs (check-cubecl.sh, fixture-parity, mutants.toml, deny.toml) | open | decide each; re-gating cubecl is the honest choice |
| D7 | Loose session handoffs at the repo root | open | move to `out/` with dates |
| C1 | Twin field crates ~60% shared; five copies of the mapped-buffer upload | open | hoist into `glyph-field`; last, largest |
| C2 | Oversized files; dead kernels in `cubecl_chain/position.rs` | open | delete dead kernels; split `glyph/src/main.rs` |
| C3 | 46 `#[allow]`, one justified | open | pass, with C2 |
| C4 | 63 `unwrap()` in `cubecl_chain/repo/dispatch.rs`, one shape | open | one accessor or non-optional fields |
| C5 | `spike_vertex_yz.rs`, 908 lines, own CLI flag, superseded | open | delete |
| C6 | Rust comments describing the Mojo/FFI backend as live | open | with D1 |
| C7 | 239 stage-letter comments | won't do | archaeology; reword only when touching |
| C8 | `clippy --all-targets`: 14 test-only lints + deny-level `reversed_empty_ranges` (seam.rs test) | open | sweep; gate runs without `--all-targets` |
| C9 | `discovery.rs` names a Claude project by the slug's last `-` segment (`…-glyph3d-js` → `js`) | open | small fix |
| P1 | pixel-ab red on both platforms since 10-07; Linux set a month stale, 2 views never adopted | Ivan's call | Mac re-baseline first, then Linux re-adoption |
| X1 | Experiments' Zed symlink scheme never built against real Zed | next up | needs a Zed checkout or the Mac |
| X2 | Missing clones here: `viz-web/glyph3d-js` (flagship corpus, `GLYPH_WEB`), Zed | next up | discuss |
| X3 | `just` not installed here; justfile `profile` recipe unrun | open | install or accept |
| R1 | Renderer currency stamp ignored the root `Cargo.toml`/`Cargo.lock` | closed | dc6a00f |
| R2 | Dead `{dylib}` / `dylib_ext()` machinery in the runner | open | with D1 |
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

Main's build fix (872621d, the other agent's) was fast-forwarded in first.

**On the Mac after merging:**

```sh
cp launch_config.example.toml launch_config.toml
ln -s ~/localdev/externalcompute/zed experiments/zed
export GLYPH_FLAGSHIP_REPO=~/localdev/viz-web/glyph3d-js
```

## Suggested order for what is open

1. D3: refuse an empty gate set (small, has a mutation, closes a vacuous green).
2. D1 + R2 + C6 + D4: the doc rewrite, one series; the files cross-reference.
3. D5, D6: tooling decisions, one commit per contested decision.
4. D7: move handoffs.
5. C2 + C3 + C4: cubecl cleanup. Output-neutral; re-gating cubecl (D6) gives it a proof.
6. C5: delete the spike.
7. C1: hoist the field machinery. Largest; wants a fresh battery.

X1, X2 and P1 are for discussion; C8, C9, X3 fit anywhere.

---

## Open items: detail

### D1. Root AGENTS.md is the stale one — [measured]

Live gates (`grep '^\[\[gate\]\]' -A1 build.toml`): manifest, committed-artifacts,
vendor-hashes, cargo-build, cargo-clippy, cargo-doc, cargo-test, pick-oracle,
pixel-ab. Documented but absent: engine-check, repo-verify, repo-verify-direct,
reference-port, cubecl-chain, cubecl-fork (engine-suites already marked
historical). Also stale in the same file:

- "Products" paragraph: currency is "a content hash of `engine/*.mojo` + the
  pixi pins". There is no Mojo (`engine/` holds `fixtures/` and
  `glyph_schema.mjs`). The inputs are now the Rust tree, root manifest and lock.
- "`cargo glyph test engine` after touching Mojo is ~25s": see D3.
- "Read next" names `engine/README.md`, `README-FFI.md`, `PORT-PLAN.md`,
  `BACKEND-PLAN.md`, `TOOLCHAIN.md`; none exist. `native/AGENTS.md` names two.
- Fence table lists `engine/glyph_schema.{mojo,mjs}`; only `.mjs` exists, yet
  `tools/gen_schema.py` still writes `MOJO_OUT` (:34). [inferred] its `--check`
  may compare against a never-committed file; run it.
- Says the `cubecl` feature is optional; `native/Cargo.toml` has
  `default = ["egui-ui", "cubecl"]`. The manifest is right.
- Build section quotes a test count; drop it (the file's own advice).

`native/AGENTS.md` (was 1b): the layout-seam section still frames
`Strategy::Direct` as the engine writing across the FFI and names
`repo-verify*` as live. The strategies are real (`--repo-engine direct|batch`);
the FFI framing is not. Three mentions.

**Fix:** rewrite "What the checks actually do" against the nine live gates;
keep the retired gates' lessons in a short "retired, and what they taught"
block. Delete the dylib paragraph, fix read-next, date it.

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

### D3. `test engine` is a vacuous green — [inferred from source]

`Scope::Engine` (`glyph/src/main.rs:109`) survives; build.toml scopes gates
`corpus`, `render`, `rust`, none `engine`. `cmd_test` filters by scope and
prints `CHECK-ALL: ALL GATES GREEN` over an empty set. Make `cmd_test` refuse
an empty gate set (with a mutation), and drop the scope and its mentions
(help text, justfile comment, `tools/check-all.sh` header). Run it first to
promote this to [measured].

### D4. rust-engineering.md — [measured]

- §1 claims `clippy::pedantic` "configured in `Cargo.toml`": no `[lints]`
  table anywhere. Add it (expect a large red) or delete the sentence.
- §2 "never `println!`": the runner's 124 `println!` are its contract. Say
  diagnostics go through tracing, verdict lines are presentation.
- §8 names `replace_file_content`, an Antigravity tool name.
- §5 "no new shell scripts" beside a gate that is a shell script.
- Reconcile with `native/AGENTS.md` by pointing, not restating.

### D5. pixi — [measured]

`build-engine` compiles `engine/ffi.mojo`; `suites*` run `engine/check.sh`;
neither exists. The mojo/max pins are dead. Live use: fontTools for the emoji
generators (`build.toml` runs `pixi run python tools/gen_emoji_sheet.py
--check`). `mise.toml` already provisions a venv with fonttools.
**Options:** (a) delete pixi, point build.toml at the mise venv; (b) trim
pixi to fontTools. (a) cleaner, (b) less risk to committed-artifacts.

### D6. Orphans — [measured]

- `tools/check-cubecl.sh`: the former cubecl-chain/fork fence, run by
  nothing. The flags it drives are live. Cubecl is default-on and heavily
  worked on, so re-gate it.
- `tools/check-fixture-parity.sh`: references `engine/fixture_io.mojo`. Dead.
- `native/.cargo/mutants.toml`: references `layout_mojo.rs`. Update or delete.
- `deny.toml`: `cargo-deny` is in mise; nothing runs it. Gate or delete.

### D7. Loose root docs — [measured]

`SESSION-HANDOFF.md` (2026-10-02, quotes floor 158),
`PLAN-AGENT-STACKS-FOCUS-LOCKING.md` (2026-10-02),
`cubecl-performance-handoff.md`: session artifacts; AGENTS.md says notes go
in `out/`. Move with dates in the filename (their links are repo-relative since
88525ea and will need `../`). Keep `TOOLING-PLAN.md` and `BUILD-BRIEF.md`.

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

### C6. Mojo comments — [measured]

Mojo/FFI/dylib mentions: `fixture.rs` 18, `fold.rs` 13, `glyph/main.rs` 11,
`repo.rs` 9, `layout.rs` 9, `text.rs` 8, `cluster.rs` 5. Keep history; reword
the ones describing a live contract.

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

### Checked and fine

- No token, key or credential in the tree or in `git log --all -p`; no
  hostname; the only email is the pixi `authors` line.
- Every gate invocation passes `--screenshot`, so no launch config reaches a
  golden or a pick.
- `CHUNK_THRESHOLD_BYTES` (64 KiB, tuned to M-series L1) is documented as
  such; whether it suits the 5090 box is a benchmark question. The cubecl
  65535 grid cap is the WebGPU default limit, so portable.
