# Maintenance pass — 2026-10-08

Survey of the tree at `5809677` (main) from the `maintenance-and-things`
worktree on the Linux box (monolith1, RTX 5090, rustc stable). Every claim is
**[measured]** (a command ran and this is its output) or **[inferred]** (read
from source, not executed). Do not promote the second kind.

This is a record of what was found, in priority order, with the fix each item
wants. Nothing here has been applied yet.

---

## 0. The tree does not compile on Linux — [measured]

`cargo clippy --workspace --all-targets --release` and `cargo test --workspace
--release` both exit 101 on this host:

```
error[E0425]: cannot find type `Metal` in module `wgpu::hal::api`
  --> crates/glyph-field-derived/src/upload.rs:112
  --> crates/glyph-field-instanced/src/upload.rs:91, :108
note: found an item that was configured out  (wgpu-hal-30.0.1/src/lib.rs:272  #[cfg(metal)])
```

`wgpu::hal::api::Metal` is a type that exists only when wgpu-hal is compiled
with its Metal backend, i.e. only on macOS. Four sites in the tree name it:

| file | gated? |
|---|---|
| `native/src/layout_hyper/device_alloc.rs` (`create_mapped_slot_buffer`) | yes — `#[cfg(target_os = "macos")]`, with a `#[cfg(not(...))]` twin that routes to `layout_device_discrete` |
| `crates/glyph-field-instanced/src/upload.rs` (`upload_direct_metal`) | **no** |
| `crates/glyph-field-derived/src/upload.rs` (`upload_direct_metal`) | **no** |
| `native/src/cubecl_chain/repo/tail_readback.rs` (:153, :169) | **no** (behind the `cubecl` feature, which is default-on) |

History [measured]: `upload_direct_metal` entered ungated in `9985918`
(2026-10-04, "multi-architecture zero-copy staging") in
`native/src/glyph_scene/buffers.rs`, moved into the field crates by `683f772`
and `99aea85` (2026-10-05). Every commit since has been macOS work, so the
Linux break is four days old and nothing on this host has built since
`2771ed6`. The 2026-10-08 desktop audit doc (`research/desktop-platform-audit.md`)
describes the discrete path as working; it cannot have been run here.

**Fix (first commit on this branch):** mirror `device_alloc.rs`. Gate the
three `upload_direct_metal` / readback functions with `#[cfg(target_os =
"macos")]`, and give the `direct_host_upload == true` branch a
`#[cfg(not(target_os = "macos"))]` arm that falls through to
`upload_staged_discrete`. Then do the real fix in the same series: these are
**five copies of one ~35-line function** (create hal buffer, map, write,
unmap, wrap with `create_buffer_from_hal`), differing only in the slot type.
One generic `create_mapped_hal_buffer<T>(device, count, label) -> (*mut T,
wgpu::Buffer)` in the `glyph-field` contract crate, gated once, retires all
five. `native/AGENTS.md` says the OS is the wrong axis for hardware branches
and that stays true for the *runtime* decision (`profile.mappable_primary_buffers`
decides whether to take the path); the `cfg` is forced by the *type* existing
only under Metal, and the helper's header should say exactly that so the next
reader does not try to remove it.

Until this lands no other item in this file can be verified on Linux.

---

## 1. Documentation rot — the biggest cleanliness class

The Mojo engine was retired and the gate set shrank, and the canonical docs did
not follow. Root `AGENTS.md` says it is canonical for anything repo-wide and
that a stale sibling should be *called* stale rather than patched; by its own
rule it is now the stale one.

### 1a. Root `AGENTS.md` documents 16 gates; `build.toml` declares 9 — [measured]

Live (`grep '^\[\[gate\]\]' -A1 build.toml`): manifest, committed-artifacts,
vendor-hashes, cargo-build, cargo-clippy, cargo-doc, cargo-test, pick-oracle,
pixel-ab.

Documented in "What the checks actually do" but **absent from build.toml**:
engine-check, repo-verify, repo-verify-direct, reference-port, cubecl-chain,
cubecl-fork (engine-suites is already marked historical). That is roughly
half the section, including the longest paragraphs (cubecl-fork, reference-port
with its quoted volumes). Also stale in the same file:

- "Products" paragraph describes the dylib content hash; there is no dylib
  (`engine/` holds `fixtures/` and `glyph_schema.mjs`, zero `.mojo` files [measured]).
- "`cargo glyph test engine` after touching Mojo is ~25s" — see 1e.
- "Read next" names `engine/README.md`, `README-FFI.md`, `PORT-PLAN.md`,
  `BACKEND-PLAN.md`, `TOOLCHAIN.md`: **none exist** [measured]. `native/AGENTS.md`
  "Read next" names two of the same.
- Fence table lists `engine/glyph_schema.{mojo,mjs}` as generated; only the
  `.mjs` exists, and `tools/gen_schema.py` still writes `MOJO_OUT` (:34).
  [inferred] its `--check` mode may now be comparing against a file that is
  never committed — worth running once the build is back.
- "Decoupled CubeCL: ... optional Cargo feature `cubecl` (`cargo check
  --features cubecl`)" vs `native/Cargo.toml` `default = ["egui-ui", "cubecl"]`
  and `.agents/rules` §9 which *requires* it default. The doc and the manifest
  disagree; the manifest is right, fix the doc.
- "Build" section quotes 222 tests across 13 binaries. `SESSION-HANDOFF.md`
  quotes 158. `build.toml` says 222. The AGENTS.md text elsewhere says a count
  in prose is "one more thing to forget" — follow its own advice and drop the
  number from the Build section.

**Fix:** rewrite "What the checks actually do" against the nine live gates
(keep the retired paragraphs' *lessons* — the vacuity patterns — in a short
"retired, and what they taught" block, since the blind-spot reasoning is the
valuable part). Delete the Products/dylib paragraph. Fix the Read-next lists.
Date the edit.

### 1a½. `cargo glyph` is broken inside every documented worktree — [measured]

`native/AGENTS.md` tells you to work in `.claude/worktrees/<name>`. Cargo
reads `.cargo/config.toml` from **every ancestor directory** and merges them,
and `[alias]` arrays concatenate. From a worktree under the repo root the
alias is therefore read twice:

```
$ cargo --list | grep glyph          # from the worktree
    glyph   alias: run --quiet --release -p glyph -- run --quiet --release -p glyph --
$ cargo --list | grep glyph          # from the main checkout
    glyph   alias: run --quiet --release -p glyph --
```

The tool receives `run --quiet --release -p glyph --` as its verb, matches
`Cmd::Run`, and launches the renderer, which rejects `--quiet`. So `cargo
glyph test` in a worktree fails before any gate runs, with an error that
names the wrong binary. The expanded form
`cargo run --quiet --release -p glyph -- test` works. `just check` does not
(it calls the alias).

**Fix options:** (a) document worktrees *outside* the repo directory
(`git worktree add ../glyph3d-rs-wt/<name>`), which is the only fix that
needs no code; (b) make the justfile recipes call the expanded command and
point docs at `just`; (c) have the tool's `main` strip a repeated
`run --quiet --release -p glyph --` prefix from argv with a comment naming
this hazard. (a) plus (b) is the recommendation; (c) is a band-aid over a
cargo behaviour. Either way the worktree instructions in both AGENTS.md
files change.

### 1b. `native/AGENTS.md` — mostly current, three stale spots — [measured]

The layout-seam section still describes `Strategy::Direct` as "the engine
writes instances straight into the caller's arena ... across the FFI" and
names `repo-verify`/`repo-verify-direct` as live gates. `--repo-engine
direct|batch` still exist as CPU reference strategies
(`cli/args.rs:165`), so the *strategies* are real and the *FFI framing* is not.
Three Mojo/FFI mentions total; a short pass.

### 1c. `.agents/rules/rust-engineering.md` makes claims the tree does not back — [measured]

- §1 "We use `clippy::pedantic` as a baseline (configured in `Cargo.toml`)":
  no `[lints]` table and no `pedantic` anywhere in any Cargo.toml. Either add
  the lints table (expect a large red) or delete the sentence.
- §2 "Never use `println!` ... use `tracing`": 8 files import `tracing`, 0
  `#[tracing::instrument]`, and `glyph/src/main.rs` has 124 `println!`. For a
  CLI whose stdout *is* the contract that is correct, and the rule should say
  "diagnostics go through tracing; the gate runner's verdict lines and the
  offscreen instruments' PASS blocks are presentation, not logging".
- §8 names `replace_file_content` — an Antigravity tool name, meaningless to
  any other agent. Say "AST-aware edits" or drop it.
- §5 "Do not introduce new shell scripts" sits next to a gate that is a shell
  script (`tools/check-pick-oracle.sh`, run by `pixel-ab`'s sibling gate).
  Qualify it.
- The file says it "aligns with `native/AGENTS.md`"; the two should be
  reconciled so one points at the other rather than restating.

### 1d. `pixi.toml` is a toolchain for an engine that no longer exists — [measured]

`build-engine` (both platforms) compiles `engine/ffi.mojo`; `suites*` run
`engine/check.sh`. Neither file exists. The mojo/max pins (`==1.1.0`, `==26.6`)
and the lock are dead weight. The only live use is fontTools for the emoji
generators (`build.toml:62-63` runs `pixi run gen-emoji-sheet`), and
`mise.toml` already provisions a Python venv with `pip install fonttools`.

**Fix:** either (a) delete pixi entirely and point the two build.toml commands
at the mise venv, or (b) trim pixi to the fontTools env and the emoji tasks.
(a) is cleaner; (b) is less risk to the committed-artifacts gate. Either way
the `build.toml:34` comment about `build-engine` goes.

### 1e. `cargo glyph test engine` is a vacuous green — [inferred from source]

`glyph/src/main.rs:109` still has `Scope::Engine`, and `build.toml` has gates
scoped `corpus` (3), `render` (2), `rust` (4) — **zero `engine`**. `cmd_test`
(:1437) filters gates by scope and prints `CHECK-ALL: ALL GATES GREEN` over an
empty iterator. That is the exact "check that cannot fail" shape the root
AGENTS.md spends three paragraphs warning about. Remove the variant (and the
`engine` wording in the help text, justfile comment, and check-all.sh header),
or make `cmd_test` refuse an empty gate set — the second is the better fence
and should get a mutation.

### 1f. Orphaned scripts and configs — [measured]

- `tools/check-cubecl.sh` — the former cubecl-chain/cubecl-fork fence. Not in
  build.toml, justfile, or any gate. The CLI flags it drives
  (`--cubecl-chain-check`, `--cubecl-repo-check`, STRICT mode) are still live in
  `cli/args.rs`, so the *instrument* exists and *nothing runs it*. The
  SKILL.md tells humans to run `--cubecl-repo-check` by hand. Decide: re-gate
  it (it was the only bit-exact fence on the GPU chain) or delete the script
  and say why in the commit. Given cubecl is default-on and twelve of the last
  twenty-five commits are cubecl perf work, re-gating is the honest choice.
- `tools/check-fixture-parity.sh` — references `engine/fixture_io.mojo`. Dead.
- `native/.cargo/mutants.toml` — references `layout_mojo.rs`. Advisory only;
  update or delete.
- `deny.toml` — present, `cargo-deny` is in mise, nothing runs it. Wire into
  a gate or delete.
- `tools/verify_atlas.py`, `preview_glyphs.py`, `repro_pick_oblique.py` —
  already documented as run-by-nothing; unchanged.

### 1g. Loose root-level docs — [measured]

Four Markdown files sit at the repo root beside README.md and AGENTS.md:

| file | lines | state |
|---|---|---|
| `SESSION-HANDOFF.md` | — | 2026-10-02 session handoff; says test_floor is 158 (it is 222); Steps 1-4 are now partly landed (`81aaa85` did Step 1's KeyD fix; `agent_transcript/` exists) |
| `PLAN-AGENT-STACKS-FOCUS-LOCKING.md` | 205 | 2026-10-02 plan for the same work |
| `cubecl-performance-handoff.md` | 174 | links to a `file:///Users/lugo/.../worktrees/workspace-random-experiments/` path |
| `TOOLING-PLAN.md` | 334 | plan of record for the tooling; cross-referenced by AGENTS.md, keep |
| `BUILD-BRIEF.md` | — | already self-marks as historical, keep |

The first three are session artifacts. Root `AGENTS.md` § "Where work lands"
says notes go in `out/`. Move them there with their dates in the filename, and
fix the absolute link.

---

## 2. Code shape

### 2a. Twin crates share most of their lines — [measured]

`crates/glyph-field-instanced/src` and `crates/glyph-field-derived/src` are
911 and 880 lines respectively. `diff` per file:

| file | instanced | derived | differing lines |
|---|---|---|---|
| upload.rs | 259 | 257 | 158 |
| field.rs | 226 | 264 | 142 |
| pipeline.rs | 162 | 176 | 92 |
| storage.rs | 95 | 90 | 39 |

Most differing lines are the slot type name. The split was deliberate ("one
crate per render mode, the scene never touches slot bytes") and the contract
crate is the right home for the shared machinery: `upload_direct_metal` /
`upload_staged_discrete` generic over `T: Pod` (also closes item 0),
`SlotStorage<T>` with `write_colors`, and the chunking arithmetic in
`upload_host_slots` / `upload_derived_slots`. Each mode crate then keeps only
its slot layout, its transcode, and its WGSL. Output-neutral by construction;
the pixel gate and `cubecl-repo-check` are the proof once they run.

### 2b. Oversized files — [measured]

| lines | file | note |
|---|---|---|
| 3643 | `native/src/cubecl_chain/position.rs` | kernels + host; contains two `#[allow(dead_code)]` kernels (`derive_stride` :2332, `paginate` :2366) that are compiled and never launched |
| 1921 | `glyph/src/tui.rs` | the Ratatui launcher |
| 1659 | `native/src/layout_hyper/pass2_device.rs` | 5 `too_many_arguments` |
| 1658 | `native/src/fixture.rs` | 18 Mojo mentions in comments |
| 1587 | `native/src/text.rs` | |
| 1572 | `glyph/src/main.rs` | manifest types + every command |
| 1534 | `native/src/layout.rs` | the seam; leave it |
| 1502 | `native/src/fold.rs` | |
| 1465 | `native/src/agent_transcript/staging.rs` | |

`position.rs` is the one to look at: the retired kernels can go (the
`allow(dead_code)` is the tell), and the host-side dispatch wrappers vs the
`#[cube]` bodies are a natural seam. `glyph/src/main.rs` would split cleanly
into `manifest.rs` (the typed build.toml) and the command functions.

### 2c. Lint suppressions — 46 `#[allow]`, few justified — [measured]

House rule: `#[allow]` only when the lint is wrong for the code, with a
one-line justification. One of the 46 has one (`windowed.rs`).

- `cubecl_chain/mod.rs`: **8 × `allow(dead_code)`** on `ITEM_DESC_*_PAD*`
  constants. Prefix them `_` or fold them into one documented stride constant.
- `cubecl_chain/position.rs:2332, :2366`: dead kernels — delete.
- `cubecl_chain/repo/dispatch.rs:33, :162`: `sync_prof` field and
  `record_sync` — unused; delete or use.
- `position.rs`: `allow(unused_assignments)` — almost always a real
  simplification waiting.
- `manual_range_contains` (8) and `len_zero` (3) inside `#[cube]` bodies: if
  the cube macro cannot lower `(a..b).contains(&x)` / `is_empty()` that is a
  legitimate reason and it should be written down once at the module top, not
  repeated silently per function.
- `too_many_arguments` (20): `pass2_device.rs` ×5, `scan.rs` ×4, `repo.rs` ×3.
  The groups of five and four are the same signature repeated; a params
  struct per module pays for itself.

### 2d. `unwrap()` outside tests — house rule says `expect("why")` — [measured]

134 total; those before any `#[cfg(test)]` marker in their file:

| count | file |
|---|---|
| 63 | `native/src/cubecl_chain/repo/dispatch.rs` |
| 9 | `glyph/src/main.rs` |
| 7 | `native/src/spike_vertex_yz.rs` |
| 4 | `native/src/layout_hyper/pass2_device.rs` |
| 2 each | `repo.rs`, `glyph_scene/pick.rs`, `atlas.rs` |
| 1 each | `agent_carrel.rs`, `scan.rs`, `layout_hyper.rs`, `fixture.rs`, `repo_check.rs` |

The 63 are all one shape: `buf.h_<name>.as_ref().unwrap()` on `Option`-typed
host buffers that are always `Some` after setup (dispatch.rs:215-222 and on).
One accessor on the buffers struct (`fn h_bytes(&self) -> &Handle` with a
single `expect("host buffers are allocated in prep before dispatch")`) or
making the fields non-optional retires all 63 in one edit. The rest are a
short pass.

### 2e. `spike_vertex_yz.rs` — 908 lines, `pub mod`, its own CLI flag — [measured]

A spike for vertex-stage Y/Z derivation. Its outcome landed as the Derived
mode crate on 2026-10-05 (`99aea85`). Not referenced by any gate or mutation
in build.toml. Delete it with `--spike-vertex-yz` and its `cli/command.rs`
arm, naming the crate that superseded it in the commit.

### 2f. Comments describing a backend that no longer exists — [measured]

Mojo/FFI/dylib mentions in Rust source: `fixture.rs` 18, `fold.rs` 13,
`glyph/main.rs` 11, `repo.rs` 9, `layout.rs` 9, `text.rs` 8, `cluster.rs` 5.
Many are history ("ported from the Mojo fold, 2026-09") and should stay; the
ones that describe a *live* contract ("the FFI materializes a 32 B wire
record") should be reworded to the Rust path they now describe. Pass through
with the AGENTS.md rewrite, same commit series.

### 2g. Stage-letter comments — 239 — [measured]

`Stage L` 69, `K` 49, `F` 39, `G` 38, `E` 23, `H` 20, `A` 11, `C` 9, others ≤2.
Root AGENTS.md says the letters are archaeology and to prefer substance when
*touching* one. Not a sweep candidate; noted so nobody starts one.

---

## 3. Verification state on this host — [measured]

Updated later the same day, after a second agent's build fix (uncommitted in
the main checkout at the time; the three files from item 0 gated with
`#[cfg(target_os = "macos")]`, the minimal form) was copied here and the
battery run through the expanded tool command (item 1a½).

- `nvidia-smi`: 2.6 GiB of 32 GiB in use; no llama-server hog this time.
- With the fix: **cargo-build, cargo-clippy, cargo-doc 0 warnings; cargo-test
  222 over 13 binaries (floor 222); manifest, committed-artifacts (34
  fixtures, atlas, trie, emoji sheet, cluster table, corpus), vendor-hashes,
  pick-oracle all PASS.** The fix is correct and sufficient for the build.
  (`cargo clippy --all-targets` additionally reports 14 test-only lints and
  one deny-level `reversed_empty_ranges` in a `seam.rs` test; the gate runs
  without `--all-targets`, so these are not red. Worth a sweep.)
- **pixel-ab: red, 7 of 9 views diverge from `vulkan-nvidia`, 2 have no
  Linux baseline** (`emoji-cluster`, `repo-cluster`, added 2026-09-20 and
  never adopted here). `glyph drift` against `metal-apple` says "clustered —
  something has a shape" on every view but `demo`. **None of this is the
  build fix's doing**, and most of it is not Linux's:
  - `text.png`: every differing pixel is the comment colour. Old
    `[106,153,85]` in the golden, `[125,200,115]` in the fresh frame — the
    `palette::COMMENT` change in `9c96ad1` (2026-10-08, "enhance comment
    lexing"), which moved pixels and re-baselined nothing. The same commit
    ratcheted `test_floor`, so cargo-test was run; pixel-ab was not, or was
    red and unmentioned. **The Mac is red on this view too.**
  - `repo-zoom.png`: the whole field is shifted down-right against BOTH the
    Metal golden and the old Linux golden (which agree with each other).
    Identical under `--repo-engine hyper` and `batch` and under both
    `--field-mode`s (four renders, byte-equal in pairs), so layout and
    upload agree and the camera moved. `d1b0f7e` (2026-10-07, "wire
    InkExtent into FileView") recentres the focus camera on ink extents
    instead of page extents (`repo.rs`, the `cx`/`cy` lines). The Metal set
    was last touched 2026-10-05. **The Mac is red here too** [inferred: by
    reading, since the 10-05 tree's repo path panics on Linux — see below].
  - `repo-wide`, `repo-down`, `repo-back-oblique`, `emoji`: the Metal set
    was re-baselined 2026-10-02 (reversed-Z, Greeking) and 2026-10-05
    (Derived mode); the Linux set never was. Stale by a month of deliberate
    changes, plus whatever the two commits above add.
- **Control experiment:** the tree at `99aea85` (the Metal set's own commit)
  built on this host with the same gate patch renders `text.png`
  **byte-identical** to the old `vulkan-nvidia` golden and within ≤16 levels
  of the Metal golden on 13,571 px (the known cross-vendor edge noise, same
  figure as 2026-09-07). So this host reproduces the goldens exactly when
  the tree is the one that made them. That tree's `--load-repo` panics on
  Linux (`instance_chunks on a device arena`, `layout.rs:588`) under every
  strategy — the discrete repo path only started working somewhere in the
  2026-10-05..08 series — so the repo views could not be checked the same way.

**What this means for the queue:** main is pixel-red on both platforms
since 2026-10-07 and needs a deliberate re-baseline on the Mac (text,
repo-zoom at least; look at every frame) in a commit that says why. The
Linux set then needs re-adopting in full, including the two views it never
had. Neither is this branch's job; both block pixel-ab meaning anything
for the refactors in §2.

---

## 4. Proposed commit order for this branch

1. **fix(build): gate Metal HAL paths; one mapped-buffer helper in glyph-field** — item 0 + 2a's upload half. Run the full battery; record the Vulkan pixel-ab verdict in the message.
2. **chore(glyph): refuse an empty gate set; drop the engine scope** — item 1e, with a mutation.
3. **docs(agents): rewrite the gate section against build.toml; fix read-next; date it** — items 1a, 1b, 1c. One commit, because the files cross-reference.
4. **chore(tooling): retire pixi or trim it to fontTools; delete dead scripts; decide check-cubecl.sh** — items 1d, 1f. Separate commit per decision if any is contested.
5. **chore(docs): move session handoffs to out/** — item 1g.
6. **refactor(cubecl): remove dead kernels, pad-constant allows, dispatch.rs unwraps** — items 2b, 2c, 2d. Output-neutral; cubecl-repo-check is the proof, which is one more reason to re-gate it in step 4.
7. **chore: delete spike_vertex_yz** — item 2e.
8. **refactor(field): hoist shared storage/pipeline machinery into glyph-field** — rest of 2a. Last, because it is the largest and the one most worth a fresh battery.

## 5. Machine-specific values — [measured 2026-10-09]

Swept the tracked tree (excluding `integration/`, `tools/vendor/`, fixture inputs)
for usernames, absolute paths, home-dir reads, hostnames, emails and credential
shapes, and the full `git log --all -p` for credential shapes.

**Clean:** no token, key or credential anywhere in the tree or its history. No
hostname. The only email is the pixi `authors` line, which matches the commit
author. `ADAPTER.txt` naming the device and driver is deliberate. Every gate
invocation passes `--screenshot`, which makes `cli/args.rs` skip
`launch_config.toml`, so the committed launch config cannot reach a golden or a
pick (verified for pixel-ab's commands and every `$BIN` call in
`check-pick-oracle.sh`).

**Code that bakes in the Mac path `/Users/lugo/localdev/viz-web/glyph3d-js`:**

| Site | Effect elsewhere |
|---|---|
| `glyph/src/tui.rs:200` `REPO_PRESETS` | second preset is a dead path on every other box |
| `glyph/src/tui.rs:1721` | a unit test asserts that literal, so changing the preset breaks cargo-test |
| `justfile:31` `profile` default | `just profile` fails without an argument |
| `tools/bench_hyper.py:95` `--repo` default | same |
| `tools/vendor-manifest.py:54` | has a `GLYPH_WEB` env override; only used to refresh. Fine |
| `native/src/agent_transcript/tests.rs:458` | real-session test keyed to a Mac slug and one session id; silently skips elsewhere, so it tests nothing on any box but the M2 |

Fix shape: one env var (`GLYPH_FLAGSHIP_REPO`, say) read by the TUI preset,
bench_hyper and the justfile, falling back to a relative `../../viz-web/glyph3d-js`
(the path AGENTS.md already uses) or to omitting the preset when absent. The
TUI test should assert against the constant, not the literal.

**TUI reads the home directory directly.** `LauncherState::discover_agent_sessions`
(`tui.rs:244`) hardcodes `$HOME/.gemini/antigravity/brain` and `$HOME/.claude/projects`
and ignores the `claude_projects_dir` / `antigravity_brain_dir` overrides that
`native/src/launch_config.rs` honours. Two consequences: the launcher's default
session is whatever transcript on the machine was touched last (on an agent box,
usually the running agent's own session), and the 9 TUI tests that call
`LauncherState::new()` walk every `.jsonl` under the user's `~/.claude/projects`
plus read the repo's `launch_config.toml`. They pass, but they are not hermetic.
Fix: route discovery through `LaunchConfig` and give `new()` a test constructor
that skips discovery.

**Committed `launch_config.toml` is a personal preference file.** It sets
`field_mode = "derived"` against the CLI's `instanced` default, and
`color_mode = "flat"` against the TUI's built-in `syntax` (the CLI default is
also `flat`). Every windowed run from the repo root or `native/` picks it up, so
on any checkout the interactive field mode differs from the documented default
without a flag saying so, and the TUI and the bare binary disagree on colour
unless the file is present.
It also carries a commented example pointing at one specific Antigravity
session UUID. Options: rename to `launch_config.example.toml` and gitignore
`launch_config.toml`, or keep it and state in AGENTS.md that it overrides
interactive defaults.

**Docs and handoffs with Mac absolute paths**, all prose: `AGENTS.md:57`,
`BUILD-BRIEF.md:187`, `.agents/skills/glyph-engine-testing/SKILL.md:55,72,80`,
`tools/gpu_profile/README.md:12`, and `file:///Users/...` links in
`research/desktop-platform-audit.md` (6) and `cubecl-performance-handoff.md` (4,
two into a worktree that no longer exists). The links are dead on every other
machine and on GitHub; repo-relative links work everywhere. The loose root
handoffs (`SESSION-HANDOFF.md`, `cubecl-performance-handoff.md`) are already
item 1g.

**`experiments/` cannot build off the Mac.** `zedspike` and `fieldzed`
`Cargo.toml` and two `main.rs` files hardcode `/Users/lugo/localdev/externalcompute/zed`.
Excluded from the workspace and outside every gate, so harmless today; a
`[patch]` via a gitignored `experiments/.cargo/config.toml` or a `ZED_ROOT`
env var would make them portable if they are kept.

**Hardware-shaped constants**, not paths: `CHUNK_THRESHOLD_BYTES` (64 KiB, tuned
to M-series L1) and the burst-write sizing are documented as such in
`native/AGENTS.md`; the cubecl 65535 grid cap matches the WebGPU default
limit, so it is portable. Nothing to change, but the chunk threshold is a
benchmark question for the 5090 box, not a cleanliness one.

**Small:** mise's `.venv` (created at the repo root by `mise.toml`) is not in
`.gitignore`. `discovery.rs:96` derives a project hint by taking the last
`-`-separated segment of the slug, so `-Users-lugo-...-glyph3d-js` becomes
`js`; not machine-specific, but it is the comment that carries the path.

**Status, 2026-10-09: done on this branch, uncommitted.**
- TUI presets come from `repo_presets` in the launch config, falling back to
  `.` and `native/fixtures/g-pick-repo`. Session discovery in both the TUI and
  the renderer scans only `claude_projects_dir` / `antigravity_brain_dir`; the
  home default and the renderer's working-directory probe are gone. TUI tests
  start from `LauncherState::defaults()`, which reads no file and no directory.
- `launch_config.toml` is now the tracked `launch_config.example.toml`; the real
  file is gitignored. On the Mac, `cp launch_config.example.toml launch_config.toml`
  restores the previous behaviour exactly (same values).
- `GLYPH_FLAGSHIP_REPO` drives the justfile `profile` default and
  `bench_hyper.py --repo`, falling back to this repo. `vendor-manifest.py` reads
  `GLYPH_WEB` only, with no default.
- The real-session test runs only with `GLYPH_REAL_CLAUDE_SESSION` set.
- Docs carry no absolute paths; `file:///` links are repo-relative.
- `experiments/` reaches Zed through a gitignored `experiments/zed` symlink, with
  `exclude = ["zed"]` (proven necessary on a mock workspace). Not built here:
  there is no Zed checkout on this box.
- Proof: full battery green except pixel-ab, which is red on the same nine views
  as before; all nine renders are byte-identical to main@872621d built in
  scratch. cargo-test 225 over 13 binaries, floor raised 222 → 225.
- **Superseded the same day:** discovery now falls back to each app's default
  locations (Claude Code, Antigravity desktop + CLI, Kimi Code) when the config
  is silent; a configured path still wins per app, and `""` turns an app off.
  One table in `crates/glyph-session-dirs`, shared by the renderer and the
  launcher, and both report what they scanned. Kimi Code transcripts gained a
  parser (`agent_transcript/kimi.rs`). Renders still byte-identical to main.
