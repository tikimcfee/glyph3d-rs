# Agent code tooling: handoff note (2026-10-11)

Working note for the next session, not policy. Goal (Ivan): make finding,
reading and editing code in this tree less onerous for agents, with boring,
correct, local tools. Four research scouts surveyed LSP, structural search,
cargo tooling, and agent-layer MCPs; this is what was decided, installed and
measured, and what is still open.

## State when this was written

First session: nothing committed. The only change in the tree was the
untracked `rust-analyzer.toml` at the root, plus this note.

**Second session (2026-10-11): commit 1 (setup) landed, along with this note.**
`rust-analyzer.toml` was deliberately left OUT of it, still untracked, because
rust-analyzer ignores it (results below).

**Moved (2026-10-11).** Another session was working in the same
`maintenance-and-things` worktree (a derived-field empty-load fix), so this
work moved to its own sibling worktree, `../glyph3d-rs-tooling` (branch
`tooling`, from `e5ca4d3`). Only this work's hunks came along, and they were
removed there with `git apply -R`. Both branches merge back separately.

**Commit 2 (the orphans gate) landed:** `tools/check-orphans.sh`, gate
`orphans` (scope rust, run under `pixi run`), and mutation
`tests-mod-dropped`, which `prove --mutation` showed reddening for its stated
reason; the file was restored byte-exact. Coverage is now 16 gates, 14 with
mutations; the two uncovered are repo-verify and repo-verify-direct, as
before. Commit 3 then put cargo-doc on `--workspace` AND `--release` (the
release profile so the gate never shares `target/debug` and its lock with an
editor's rust-analyzer; measured 4.6 s, zero warnings, nothing gated on
`debug_assertions`). The project `rust-analyzer.toml` was deleted, because it
cannot reach the flycheck. Commit 3 also found that cargo-modules runs
`cargo check --compile-time-deps --all-targets` in the default target dir
(caught with a logging cargo shim on PATH), so the orphans script now sets
`CARGO_TARGET_DIR=target/orphans`: 564 MB, 29 s cold and 27 s warm, and
`target/debug` untouched. With that, no gate uses `target/debug`.

### The lint series as built (2026-10-11, branch `tooling`)

- `0fd05d5`: an empty `[workspace.lints]`, which every member opts into.
- `51fd106`: dropped six `#[allow(too_many_arguments)]` that suppressed
  nothing. Each function has exactly 7 parameters, and the lint fires above 7.
  Found by turning every allow into `#[expect]` for one clippy run.
- `8d50ea7` (`allow_attributes_without_reason`): reasons on the 17 left.
  Seven of them say "not yet bundled": parameter-struct candidates in Pass
  1, the line table, the reference scan and the transcript layout.
- `d67dd46` (`allow_attributes`): every suppression is `#[expect]`, so a
  stale one is a warning.
- `ac13974`: the windowed and emoji-scratch Pass 2 paths rebase `dest_addr`
  below their buffer, and the emitter did `ptr::add` from that base:
  formally UB. It now offsets in integers. hyper-oracle and pixel-ab green.
- `bfb7582` (`undocumented_unsafe_blocks`): 38 SAFETY comments. Six only
  compile on macOS, and were found and checked with
  `cargo clippy --target aarch64-apple-darwin --target-dir target/clippy-macos`
  (the rustup target is installed on the Linux box). Comments only.
- `de53e46`: native/AGENTS.md said zero arch intrinsics; text.rs has NEON.

**Open, for Ivan:**
- **Pass 1 / Pass 2 slot count is unchecked at run time.** Both emitters
  (and the host arena's `commit`) are sound only if Pass 2 writes exactly
  the survivors Pass 1 counted. hyper-oracle compares every slot, but only
  the device emoji-scratch path asserts the count. A per-chunk
  `assert_eq!(survivor_out, pre.survivor_count)` costs one compare per
  chunk and would turn an under-count (uninitialised slots shown) into a
  panic. An over-count would already have written past the range before it
  fired. That one wants a bench before it lands.
- **A macOS cross-clippy gate?** About 14 s warm, and it would have caught
  the six macOS-only sites on Linux. But the M2 has Homebrew Rust and no
  rustup, so the gate would have to be host-conditional.
- **The M2's battery after merge.** The macOS SAFETY comments were
  lint-checked cross-target, not built or run there.

### Commit 1 as built

- `pixi.toml`: ast-grep 0.45.3, difftastic 0.71.0, cargo-nextest 0.9.148 and
  samply 0.13.1 from conda-forge. The lock solve added them for both
  `linux-64` and `osx-arm64` and moved nothing already there; fontTools stayed
  at 4.66.1.
  - `setup` runs `cargo install --locked --root .pixi/cargo-tools
    naga-cli@30.0.1 cargo-modules@0.27.0`, and `[activation.env]` puts that
    `bin` first on PATH. It took 52 s cold; a re-run skips both crates.
  - Every tool resolves to its pixi copy under `pixi run`, not to
    `~/.cargo/bin`.
  - Tasks folded in from the justfile: `test`, `profile`, `profile-view`,
    `bake-atlas-cache` (scratch PNG in `target/`, now built with
    `cargo glyph run`), and the new `outline` and `setup`.
  - Dropped: `check-scoped` (`pixi run check rust` passes the scope
    through) and `build-native`, a package-scope build, which AGENTS.md
    warns links different bytes.
- Measured while building it:
  - pixi 0.80's task shell has no `${VAR:-default}`, and its templates have
    no `env`. So `profile`'s `$GLYPH_FLAGSHIP_REPO` fallback goes through
    `sh -c`.
  - `profile` reaches samply, but samply refuses on this host:
    `kernel.perf_event_paranoid` is 2 and needs to be ≤ 1, which takes sudo
    (`echo 1 | sudo tee /proc/sys/kernel/perf_event_paranoid`).
  - `bake-atlas-cache` rewrote the 379 MB cache, and `test` ran one test
    through nextest.
  - The ast-grep scan reports nested items (methods inside an `impl`). Its
    built-in report styles carry no end line, so `tools/ast-grep/outline.py`
    formats `--json=stream`.
- Not done yet: `cargo uninstall` of the `~/.cargo/bin` copies (ast-grep,
  difft, naga, cargo-modules). Hold it until this reaches `main`: until then
  only this worktree has the pixi copies.

Installed on the Linux host, user-level (no system packages; no passwordless
sudo there):

| Tool | Version | How | Verified |
|---|---|---|---|
| rust-analyzer | 1.98.1 | `rustup component add rust-analyzer` | `--version` runs (before: only the rustup proxy, "Unknown binary") |
| `rust-analyzer-lsp` Claude Code plugin | official marketplace | `claude plugin install rust-analyzer-lsp@claude-plugins-official --scope user` | enabled in settings; **needs a new session** to start |
| ast-grep | 0.50.0 | `cargo install --locked ast-grep` | struct-literal search + outline rule on this tree |
| difftastic | 0.71.0 | `cargo install --locked difftastic` | installed; not yet used |
| naga-cli | 30.0.1 (= `Cargo.lock`'s naga) | `cargo install --locked naga-cli@30.0.1` | all 14 shipping shaders validate; a broken copy exits 1 |
| cargo-modules | 0.27.0 | `cargo install --locked cargo-modules` | orphan check clean; reddens on a deleted `mod tests;` |

## First things to check in the new session

1. **The LSP tool works.** Try `documentSymbol` on `native/src/layout_hyper.rs`,
   then `findReferences` on something in `layout_hyper/page.rs`. Note cold-start
   time and rust-analyzer's RSS (scouts guessed 2–4 GB; measure it).
2. **`rust-analyzer.toml` is honoured.** It sets `cargo.targetDir = true` and
   `check.command = "check"`. Expect `target/rust-analyzer/` to appear, and
   rust-analyzer never to block `cargo glyph` on the build-dir lock. The RA
   manual still calls this file "work in progress"; if it is ignored, the
   fallback is a local plugin `.lsp.json` with
   `"initializationOptions": {"cargo": {"targetDir": true}}`.
3. **Whether diagnostics are pushed after an Edit**, and whether that triggers a
   `cargo check` each time. The scouts could not verify either.

### Results of those checks (second session, 2026-10-11)

1. **The LSP tool works.** `documentSymbol` on `layout_hyper/page.rs` is
   exact. `findReferences` on `Pager::frame` gives 11 sites in 3 files, the
   same set `rg '\.frame\('` finds. On a cold start it answers "No references
   found" until indexing ends, which took about 30 s, so a cold empty answer
   means nothing. **Gotcha:** `documentSymbol` reports a symbol's line as the
   start of its doc comment (`frame` at 98, its `fn` at 101). A position-based
   query aimed at that line finds nothing, so aim at the `fn` line. RSS: 1.0 GB
   at 3 s, 3.1 GB once indexed, plus `rust-analyzer-proc-macro-srv`.
2. **`rust-analyzer.toml` is NOT honoured.** No `target/rust-analyzer/`
   appears. Flycheck logs to `target/flycheck0/` and builds into
   **`target/debug/deps`** (451 artifacts; `target/debug` is 2.8 GB), both at
   startup and on every save afterwards. That is the directory and lock the
   **cargo-doc gate** uses (`cargo doc -p glyph3d-native`; its dependency
   metadata goes to `target/debug`), so the two do contend. In RA's
   `config.rs`, `cargo_targetDir` is in the `workspace` group, which a
   workspace ratoml should be able to set, so why the file is ignored is not
   yet known. Any fix needs an RA restart (a new session) to test.
3. **Diagnostics are pushed after an Edit.** Each save runs a flycheck
   (`cargo check`). Its errors arrive asynchronously, a few seconds later,
   attached to the NEXT tool result, not to the Edit's own. Tested with a
   deliberate `f32 + ()` in `page.rs`, then restored; `git diff` was empty.

**Follow-up, same session.** rust-analyzer can be restarted without a new
session. `pkill -x rust-analyzer` is enough: Claude Code starts a fresh one on
the next LSP call. Touching `rust-analyzer.toml` does NOT make the running
process re-read it. **The file is not read at all**, not just its
`targetDir`. A cold-started rust-analyzer still built into `target/debug`,
and a probe `check.extraArgs = ["--target-dir", "target/ra-probe"]`
never reached a flycheck either. The plugin launches a bare `rust-analyzer`
with no `initializationOptions`.

**Root cause (found the same day).** rust-analyzer DOES load the project
file: its log says `updating ra-toml workspace config: [cargo] targetDir =
true`. But every flycheck and cargo call reads config with no source root
(`reload.rs`: `self.config.flycheck(None)`, `self.config.cargo(None)`). For
`None`, the config getters consult client config, then the USER
`rust-analyzer.toml`, then defaults, and never a project file (`config.rs`,
the `workspace` arm of `_impl_for_config_data!`). So `cargo.*` and `check.*`
in a project `rust-analyzer.toml` cannot reach flycheck. Where the file sits
plays no part: a minimal virtual workspace, the same workspace under a
dot-directory, and a single package all built into `target/debug`, driven
by a hand-written LSP client with `RA_LOG=info`.

**What works:** the user-level file (`$XDG_CONFIG_HOME/rust-analyzer/rust-analyzer.toml`,
i.e. `~/.config/...` on Linux). With `[cargo] targetDir = true` there, every
flycheck artifact went to `target/rust-analyzer/`. rust-analyzer also
reports a spurious `cargo/targetDir: unexpected field`, which is upstream
rust-lang/rust-analyzer#23451 (workspace keys in the user file are applied
but flagged). One build-script pass runs before the user file loads, so
`target/debug` may still be touched once at startup. The project
`rust-analyzer.toml` should be deleted.

**Also found:** `.gitignore` ignores all of `.claude/`, with the comment
"Agent worktrees". So commit 1's `.claude/skills/glyph-engine-testing` link
cannot be tracked until the ignore is narrowed to `/.claude/*` plus
`!/.claude/skills/`. The worktrees under `.claude/worktrees/` stay ignored
either way.

## Measured facts worth keeping

- **ast-grep outline.** A rule matching `function_item | struct_item |
  enum_item | impl_item | trait_item`, run with `ast-grep scan --rule <yml>
  --json=stream <file>`, gives exact start-end lines (e.g. `101-120  pub fn
  frame(...)` in `layout_hyper/page.rs`). The rule was written to a scratch
  dir, so recreate it as a tracked file.
- **The ast-grep blind spot.** Macro and attribute arguments are token trees
  in the Rust grammar, so patterns and rewrites skip anything inside
  `assert!`, `format!`, `vec![]` and `#[cfg]`. After any rewrite, the
  compile and `cargo glyph test rust` are the witness, not the diff. Its
  matching is untyped (every `.records()`, whatever the receiver);
  `rust-analyzer ssr` resolves paths but writes in place with no dry run.
  Use it on a clean tree, then read `git diff`.
- **The orphan check** is `cargo modules orphans -p <pkg> --lib --cfg-test
  --deny`, on every lib target, plus `glyph`'s bin and `resolve-bench`. It
  takes about 30 s for all of them. Without `--cfg-test` it falsely flags
  every `#[cfg(test)] mod tests;` file (8 in native). Native's **bin** target
  reports 19 orphans, which is expected: `main.rs` reaches them through the
  lib, so don't gate that target. Proof: deleting `mod tests;` from
  `native/src/agent_transcript.rs` gives `Found 1 orphans`, exit 1; the file
  was restored byte-exact. rustc says nothing about that loss; only the test
  floor might.
- **naga.** `experiments/gpu-direction/jit-text/src/layout.wgsl` fails by
  design: it is a template with an `@VARIANT@` placeholder.
- **Lint baseline.** No `[workspace.lints]`, `clippy.toml` or `rustfmt.toml`.
  23 `#[allow]`s (22 `too_many_arguments`, 1 `single_range_in_vec_init`), none
  with a reason. 39 `unsafe {` blocks, 4 `SAFETY` comments.
- **The cargo-doc gate only documented `-p glyph3d-native`**
  (`glyph/src/gates.rs`), so doc links in `crates/*` and `glyph/` were
  unchecked. Fixed in commit 3: `--workspace --release`, zero warnings.
- **The test floor** (`gate_cargo_test`) parses `test result: ok` lines, so
  nextest must never replace `cargo test` inside the gate. nextest prints no
  such lines and skips doctests.
- **Setup files nobody runs.** `mise.toml` and the `justfile` exist, but
  neither mise nor just is installed on this host, and `mise.toml` says
  `naga = "latest"`, not 30.0.1.

## Decided with Ivan (2026-10-11, end of session)

- **Setup is consolidated into ONE place**: every tool the repo uses. Ivan is
  fine with either a `setup.sh` or a Rust-native route. The first idea,
  `cargo glyph setup` driven by a `[[tool]]` table in `build.toml`, is
  **superseded by what was found on the hosts**: the repo already has a
  cross-platform, lockfile-pinned tool declaration that the gates use.

  **What each host has (checked over ssh, read-only, 2026-10-11):**

  | | Linux | M2 (its checkout @ d303565) |
  |---|---|---|
  | Rust | rustup, 1.98.1 | **Homebrew rustc 1.98.0, no rustup** |
  | mise | not installed | not installed |
  | just | not installed | Homebrew |
  | pixi | `~/.pixi/bin` | Homebrew |
  | nextest, samply, node, python3, uv | (python3 only checked) | Homebrew |
  | rust-analyzer, ast-grep, difft, naga, cargo-modules | installed today (`~/.cargo/bin` / rustup) | **none** |

  **Three setup declarations are tracked, and only one carries weight:**
  - **`pixi.toml` + `pixi.lock`** (platforms `osx-arm64` and `linux-64`).
    `build.toml:85-86` runs the emoji-sheet check as `pixi run python ...`,
    because fontTools lives in the pixi env. Pixi is on both hosts. (Pixi is
    itself written in Rust, for the "rusty" question.)
  - **`mise.toml`**: dead. mise is on neither host, and it says
    `naga = "latest"`.
  - **The `justfile`**: just is on the M2 only. Its recipes are the same thin
    doors as pixi's `[tasks]`, plus `profile`, `profile-view` and
    `bake-atlas-cache`.

  **Recommendation: `pixi.toml` becomes the one declaration.**
  - **From conda-forge** (`pixi search`, 2026-10-11): ast-grep 0.45.3
    (upstream is 0.50.0; the pin is fine), difftastic 0.71.0,
    cargo-nextest 0.9.148, just 1.58.0, nodejs 26.10.0, samply 0.13.1,
    cargo-llvm-cov 0.9.1, cargo-mutants 27.1.0. Confirm each also resolves
    for `osx-arm64` when it is added; the lockfile solve does that.
  - **Not on conda-forge:** naga-cli (it must be 30.0.1, the version
    `Cargo.lock` pins), cargo-modules, cargo-hack. Install them with a pixi
    `setup` task doing pinned `cargo install --locked ...@<ver>`, ideally
    with `--root` inside `.pixi/` so they stay per-checkout and leave no
    trace in `~/.cargo/bin`.
  - **Gates reach tools the established way**: `pixi run <tool>`, as
    build.toml:85 does. The orphans gate becomes
    `pixi run cargo modules orphans ...`.
  - **Rust itself and rust-analyzer stay per host**, not in pixi. The Claude
    Code plugin launches `rust-analyzer` from PATH, and it should match the
    host's rustc: `rustup component add rust-analyzer` on Linux,
    `brew install rust-analyzer` on the M2. Document this in the setup
    task's output; don't install it.
  - **`mise.toml`**: delete it.
  - **`justfile`**: fold it into pixi `[tasks]` and delete it (DECIDED: Ivan
    doesn't use just; it was an agent's tool choice). The non-duplicate
    recipes move as tasks: `profile` (samply; keep the
    `GLYPH_FLAGSHIP_REPO` default), `profile-view`, `bake-atlas-cache`
    (move its scratch PNG out of `/tmp` into `target/`), and `test`
    (nextest). The others are already pixi tasks.
  - **Today's `~/.cargo/bin` installs on Linux** (ast-grep, difft, naga,
    cargo-modules) can be `cargo uninstall`ed once pixi provides them, so
    there is one copy of each.

- **The lint table is a go.** Ivan wants to "work toward cleaning up those
  notices". Because the clippy gate is ZERO-warning, a lint cannot land as a
  standing warning. Each lint lands in the same commit as all of its fixes,
  so the battery stays green at every step. That makes the cleanup a
  sequence of small commits, one lint each, and not one 60-site commit.

## Planned commits (one each, in order)

1. **Setup.**
   - `rust-analyzer.toml`.
   - `pixi.toml` extended into the one tool declaration, plus a `setup` task
     for the cargo-only tools (see "Decided" above); `mise.toml` deleted.
   - The ast-grep outline rule as a tracked file.
   - A `.claude/skills/glyph-engine-testing` link to
     `.agents/skills/glyph-engine-testing`, so Claude Code loads that skill.
     Today it does not; nothing under `.claude/` is tracked.
   - Short usage rules for the tools: LSP first; ast-grep for syntactic
     sweeps, with the macro caveat; `ssr` on a clean tree only; `rg` for
     prose and WGSL.
2. **The orphans gate** in `build.toml`, with its mutation (delete a
   `#[cfg(test)] mod tests;` pair). Ivan agreed: cargo-modules becomes a repo
   setup dependency.
3. **cargo-doc to `--workspace`.** Expect it to surface warnings that have
   been hiding.
4. **The `[workspace.lints]` table** (approved). Add it with an empty
   `[workspace.lints.clippy]` plus `[lints] workspace = true` in all 8
   members, which is output-neutral. Then add one lint per commit, each with
   all of its fixes:
   - **4a** `allow_attributes_without_reason`: give the 23 sites true
     reasons.
   - **4b** `allow_attributes`: convert them to `#[expect(..., reason)]`. An
     expect that never fires is itself a warning, so a stale one surfaces
     right here.
   - **4c** `undocumented_unsafe_blocks`: 35 missing `// SAFETY:` comments,
     mostly the raw-address writes in `layout_hyper/pass2_device.rs`. Each
     must state the actual invariant; read the site, use the LSP.

   Each commit runs `cargo glyph test rust`. Pixels cannot move (attributes
   and comments only), so `difft --exit-code` against the parent is a cheap
   extra witness that nothing else changed. Later candidates go the same
   way, one per commit: pedantic picks, but NOT the cast lints, which fight
   the f32/u32 index maths.

## Open questions for Ivan

- None. mise.toml is deleted, and the justfile folds into pixi (Ivan,
  2026-10-11).

## Doc and memory cleanup that rides with the setup commit

Ivan asked that every note mentioning the old setup be cleaned up with it.
The sweep, run 2026-10-11 and excluding `out/history/`, `research/` and
`experiments/` (history keeps its words), found:

- **`AGENTS.md:778`**: "the justfile has `gen-schema`" becomes `pixi run
  gen-schema`. Also say in AGENTS.md's Build/tool section that `pixi.toml` is
  the one tool declaration, and that Rust + rust-analyzer are per host.
- **`out/MAINTENANCE-NOTES-2026-10-08.md` row X3** ("`just` not installed
  here; justfile `profile` recipe unrun", open): close it with the commit.
  Rows D6, M1 and M5 are closed history; leave them.
- **`mise.toml`, `justfile`**: deleted. Check `.gitignore` for a now-dead
  `.venv` line (M5 added it for mise).
- **The emoji generators' docstrings** already say `pixi run ...`; no change.
- **Memories**: `agent-code-tooling` updated already; `m2-remote-access` now
  records the M2's Homebrew Rust. Re-run the sweep over the memory dir
  after the commit.

Re-run the sweep before committing; a grep is worth only its match set:
`rg -i '\bmise\b|justfile|\bjust (test|check|build|profile)|`just`'`.

## Considered and skipped (one line each)

- **Serena and the other LSP-MCP bridges:** a second rust-analyzer, a
  Python stack, system-prompt overrides.
- **ck / embedding search:** the module headers already do that job.
- **Repo-map tools:** the LSP outline covers them.
- **comby, GritQL, semgrep:** dormant, JS-focused and corporate
  respectively.
- **A rustfmt hook:** 1,952 hunks, and the repo forbids mass reformatting.
- **A `cargo check` hook on every edit:** the LSP already pushes
  diagnostics, and it would be a third process on the build lock.
- **Later, by hand, never gated:** nextest for fast loops, cargo-llvm-cov
  ("is this mutated line on a walked path?"), cargo-mutants `--in-diff` (to
  find mutations worth promoting into `build.toml`), cargo-hack (to compile
  `glyph-field` without clap). The one hook worth adding: `naga <file>` on
  `.wgsl` edits.

## Merge readiness, and what is left (2026-10-11, late)

**The merge back is textually clean and one attribute short of green.**
`git merge-tree` of `tooling` against the maintenance branch (seven commits
since the split: tab stops, C32, the empty-load regression, `GLYPH_FRAME_SCRIPT`)
reports no conflicts. The merged tree was checked out detached in a scratch
worktree and run through the three gates this branch tightened:

- `cargo clippy --release --all-targets`: 2 warnings, one site.
  `native/src/layout_hyper/pass2_host.rs:629` carries
  `#[allow(clippy::too_many_arguments)]` on `scan_item_max_row_extent`, added
  by the tab-stops commit (bd34c17). The function has seven parameters and
  the lint fires at eight, so the attribute suppresses nothing: delete it,
  as 51fd106 did for six of the same. Both `allow_attributes` and
  `allow_attributes_without_reason` flag it.
- `cargo doc --workspace --release --no-deps`: 0 warnings.
- `tools/check-orphans.sh`: 9 targets, ORPHANS OK.

The merge itself is Ivan's to run (it reads the other session's branch):
`git merge worktree-maintenance-and-things` in this worktree, delete the
attribute, `cargo glyph test rust`, and fold the fix into the merge commit
so every commit on the branch stays green.

**`atlas-verify` gate (this commit).** `tools/verify_atlas.py` had asserted
the curve and glyph-map textures, `glyphs.bin` against the glyph map, the
trie's section layout, the sequence and class sections and the emoji cells
since the export was written, in 0.14 s, and root AGENTS.md listed it under
"What the whole battery cannot see". It is a `cmd` gate now (scope corpus,
`pass_line = "VERIFY OK"`), proven by `atlas-glyphmap-disagreement`.
Seventeen gates; the two uncovered ones are still repo-verify(-direct), for
the reason AGENTS.md gives.

**The candidate list, corroborated or refuted.** Measured against the tree,
not the earlier counts (which double-counted lib and lib-test builds):

- *Pedantic "small-correctness pack"*: 15 unique sites across nine lints
  (`ptr_as_ptr` 4, `ref_as_ptr` 1, `naive_bytecount` 2, `checked_conversions`
  2, `range_plus_one` 2, and one each of `manual_assert`, `nonminimal_bool`,
  `redundant_else`, `unnested_or_patterns`). `large_stack_arrays`'s one site
  is a 128 KiB test buffer in `chunk.rs`'s tests, not a stack risk. **Refuted
  as a series**: the three lints this branch landed each catch a class this
  repo had suffered (unexplained suppressions, suppressions outliving their
  cause, an undocumented unsafe block hiding UB). None of these nine does;
  they are taste, and a workspace lint is a rule every later contributor
  must satisfy. If any, `ptr_as_ptr` + `ref_as_ptr` (5 sites, one commit) as
  a follow-on to the unsafe hygiene. `many_single_char_names` (76) matches
  Ivan's § 7 naming rule but is numeric code; his call, not a lint's.
- *Board rows*: C3 closed (lint-enforced), C2 re-measured, X3 was already
  closed with the setup commit. Done in this commit.
- *pixi `check-all`*: a duplicate of `check`; removed. `tools/check-all.sh`
  stays as the documented door.
- *A `naga` hook on `.wgsl` edits*: would live in `.claude/settings.json`,
  which `.gitignore` excludes (only `skills/` is tracked), and would run in
  every session's Edit hook on this machine. Cheap, but it is Ivan's editor
  config as much as the repo's; proposed, not done.
- *C25 (MAPPABLE_PRIMARY_BUFFERS on discrete)*: corroborated as real and
  small. The flag is consumed in two places (`glyph_scene/setup.rs:280`,
  `gpu.rs:408`), both already conditioned on the backend, so requesting it
  only where those fire is a renderer change with an expected-neutral
  discrete A/B. Renderer work for the maintenance branch, not this one.
- *C31 (visible-mode hide/show nondeterminism)*: a real bug, uninvestigated,
  the most valuable open item after the merge. Not tooling.
- *repo-verify(-direct) mutations*: refuted; AGENTS.md records why nothing
  separates `direct` from its reference without a device.
- *Golden coverage holes (far-LOD tint, library mode, visible flat twins)*:
  each needs Ivan to adopt frames; candidates already exist for two.
- *The runtime slot-count assert, parameter structs for the seven "not yet
  bundled" functions*: hot-path engine changes; each needs hyper-oracle and
  an M2 A/B. After the merge, one at a time.
- *macOS cross-clippy gate*: wants a host-conditional gate (the M2 has no
  rustup), which `build.toml` has no vocabulary for yet. Deferred.

**After the merge, on Ivan's hosts**: `pixi install && pixi run setup` on the
M2 (Homebrew Rust; `cargo install` of naga-cli and cargo-modules is the
untested step), then the full battery there; here, uninstall the
`~/.cargo/bin` copies of naga and cargo-modules that pixi now supersedes.

## Why prove takes an hour, and what would halve it (2026-10-11, measured)

Ivan: mutation runs take 20-30 minutes per iteration, too slow for anyone.
Measured against a full prove another session was running (93 mutations,
read from its log, not perturbed) and one clean `cargo test --release`:

- **Projected full prove: ~58 minutes.** On 2026-10-09 it was 724 s for 51
  mutations (C21). Per Rust mutation the mutant build is ~15 s, as it was;
  the cargo-test gate under a mutation went from ~10 s to ~60 s.
- **One test is 48 of those seconds.**
  `layout_hyper::line_table::tests::seeds_obey_the_cut_rule_and_the_fold_state`
  takes 47.9 s (nextest); every other test in the renderer's lib binary
  takes 1.1 s or less, and the whole suite's other 31 binaries take ~13 s.
  It replays each line from byte 0 to every seed, seeds every 32 bytes, on
  corpus lines past 64 KiB, under 3 wraps x 2 cluster modes: quadratic in
  line length. libtest runs every test in a binary even after one fails, so
  every cargo-test mutation that reaches that binary (~35 of 47) pays it,
  and so does every battery run.
  **Fix:** one forward walk per line, checking each seed as the walk passes
  its cut (the seeds are asserted sorted). Same assertions; the
  `sequences_near_cuts` counter, which the quadratic replay inflated, becomes
  "seeds with a sequence earlier on their line", its floor re-measured.
- **14 cargo-test mutations declare `rebuild = "cargo build --release"`
  that buys nothing.** The gate needs no product and every integration test
  reaches the renderer through `CARGO_BIN_EXE_glyph3d-native`, so `cargo
  test` builds it anyway; the separate build serialises lib and lib-test
  compiles that cargo would overlap (~8 s each, measured as the difference
  between mutations with and without it). The 4 that mutate `glyph/` keep
  theirs: every later gate is spawned from the runner binary.

Projected with both: **~25 minutes.** What remains is ~60 mutant builds at
~15 s each, the next lever, all unmeasured (they need a quiet machine and a
pixel-ab run to confirm output-neutral):

- `split-debuginfo = "unpacked"` in `[profile.release]` (Linux link time
  with `debug = true`; the binary's code is unchanged).
- Release incremental during prove only (`CARGO_INCREMENTAL=1`): re-optimises
  only the codegen units a one-line mutation touches, but changes rustc's
  arguments, so the first build under it is a full one.
- Sharding mutations over two or three warm worktrees (32 cores here):
  the largest possible win and the largest change; one warm target dir per
  shard.

And the iteration lever for agents, which is selection, not speed:
`--changed` reads `git diff HEAD`, so after a commit it selects nothing. A
`--since <ref>` (mutations whose file changed on the branch, plus every
mutation of a gate whose own implementation changed) would make "prove what
this branch touched" one flag; the unscoped run stays the landing bar.

## Prove, halved: what landed, measured (2026-10-11, night)

The section above projected ~58 minutes and named the levers. Ivan: "go
right ahead, this is quite important". What landed on `tooling`, in order:

- **ecb9596**: the seed test walks each line once. 47.9 s -> 0.14 s, the same
  269,082 seeds checked (both forms run side by side on one build); the
  counter the replay inflated now counts what it stood for. Nothing had
  proved the test could fail: `seed-cut-resets-segment-advance` does, and
  hyper-oracle's visible tier reddens on it too, so the test is the fast
  field-level witness and the gate the end-to-end one.
- **53ba53f, 7513de4**: no mutation declares a `rebuild` any more (47 in
  all). The cargo-test ones served a restore that C21's stamp drop does; the
  renderer-gate ones repeated `ensure_products`; dead-doc-link built the
  renderer for cargo-doc; the runner's five made the mutant runner run its
  own gate (`cargo test` never relinks target/release/glyph, measured).
- **1c113c7**: a fix found by the test scout. A mutant that failed to build
  its gate's products made prove return BEFORE restoring, leaving the
  mutant in the tree; unreached while every such mutation declared a
  rebuild. Witnessed with a temporary non-compiling mutation.
- **7513de4** also re-anchors four expects that matched more than their
  class (two doc links that a compile error matched, a bare `FAILED`, a
  `ROCKET` the rocket's own PASS line carries).
- **8393bd1, 55a2156**: `prove --since <ref>` and `--list`. Selects by
  target file, by the gate's script, and by new or edited declarations in
  build.toml (compared as TOML with the merge-base's), so a branch proves
  the mutations it added. Against the maintenance branch: 39 of 95.

| | before (another session's run, 2026-10-11) | after (this branch) |
|---|---|---|
| full prove | 3,502 s over 88 (9 pixel-ab skipped, red) | 1,720 s over all 95, every one reddened |
| cargo-test gate per mutation | 56.8 s | 20.6 s |
| clean cargo-test gate | ~62 s | 14.5 s |
| pixel-ab per mutation | (skipped) | 23.7 s |
| cargo-doc per mutation | 17.5 s | 8.0 s |

The after run overlapped another session's unscoped prove for part of its
length, so it reads slow if anything. What is left is ~60 mutant builds at
~15 s (457 s of the 1,720) and the gates themselves; the build-setting
levers in the section above are still unmeasured.

## Two scouts' findings, triaged (2026-10-11)

Ivan asked for more cleanup. Two read-only agents swept the tree, one over
the test suite and manifest, one over cruft; every claim acted on was
re-checked first. Landed: 0840eb5 (six stale doc claims), 30905e1
(.gitignore duplicates and a dead pixi exception, checked over 2,009 paths;
pixi `check-gen`), 1c113c7 and 7513de4 above, 7f93109 (visible_verbs renders its 14
distinct frames side by side, 4.8 s -> 2.5 s, both its mutations still
redden) and c6cee2b (glyph-field-visible's unused `encase`; glam's unused
`approx` and `mint` features).

**For Ivan to decide** (not done):
- `tools/preview_glyphs.py` cannot run: it imports `daimon_runtime`, a
  module foreign to this repo. FORMAT.md and AGENTS.md point at it, and its
  output `assets/atlas/preview.png` predates the 2026-09-20 atlas. Fix
  (plain matplotlib; not in pixi) or delete all three.
- 34 tracked files in `out/` referenced by nothing, history included
  (crop_*, g-*, stageC_*, f-*, test_*.txt, hand-check.txt, …), and
  `native/out/stageA.png`. And `.gitignore` ignores `out/*.png` while 52 are
  tracked, so every new proof PNG needs `git add -f`.
- `tools/decode-slug-core.mjs`: referenced only by out/history.
- `native/.cargo/mutants.toml`: cargo-mutants reads the workspace root's
  `.cargo/`, so since the 2026-09-06 workspace move this file is probably
  read by nothing (unverified; the tool is not installed), and its globs are
  native-relative. Move it to `.cargo/` with `native/src/...` globs if
  cargo-mutants is wanted later (the work log lists it as "by hand"), or
  delete it; its findings are worth keeping in either case.
- `glyph` alone pins `toml = "0.8"` (native uses 1.x): four duplicate crates.
  A dependency bump, its own commit.
- `node` runs in two gates and is declared nowhere (not in pixi.toml; the
  README lists only Rust).
- derived_verbs (7 serial launches, 1.5 s), visible_repeat (5, 1.2 s) and
  library_regions (4, 0.9 s) render serially as visible_verbs did.
  derived_verbs compares against the Instanced field, which retirement step
  3 deletes, so it was left for that rewrite.
- pixel-ab renders its 30 frames serially; ~10 runs per full prove.
- 13 of 15 hyper-oracle mutations expect only `hyper-oracle FAIL`, which a
  refused or empty corpus also prints; the per-tier first-difference lines
  are indented, so prove cannot see which tier reddened. Prefixing them
  `FAIL ` and anchoring each expect to its tier would make the "only the
  device tier can see" claims checked rather than stated.
- pick-oracle reports a pick that resolved to the neighbouring glyph as "no
  pick line": the script finds a probe's output by the RESOLVED row/col the
  renderer prints. The gate reddens for the right reason under
  pick-off-by-one; the message misnames it.
- Unproven and load-bearing: `text::reference_layout` (the independent
  lineage), the naga WGSL tests, and `tests/view_registry.rs` (its header
  says prove cannot reach it: the lib test fails first).
- Every renderer launch maps the 378 MB emoji-sheet.cache, emoji or not
  (~70 launches per cargo test). Measure with the load's own timing first.

Measured and not worth it: `doctest = false` (the doc-test phase is 0.5 s,
and runs only after every other binary passed); the quadratic
`max_line_cols` test (0.9 s, but other tests in its binary take as long,
so the binary's wall time would not move).

## Merging `tooling` into maintenance-and-things (2026-10-11)

Agreed with the maintenance session: it merges this branch itself, once,
and runs the battery once; `tooling` is frozen after this commit. A dry run
(`git merge-tree`) against its tip a0634b6 found one conflicted file,
build.toml, four hunks:

1. `test_floor`: 385 there, 379 here. Take 385; the first cargo test's NOTE
   names the merged count (one test is new here,
   since_selects_by_target_or_gate_script).
2. The syntax-heuristic mutations (hyper-cut-head/tail-own-share,
   hyper-comment-line-indent-painted, colorizer-*): take theirs, the
   deletion. This side only dropped their `rebuild` lines.
3. render-file-copies-stacked / tile-block-drops-pen-row /
   visible-item-bytes-by-item-index: take theirs (step 2's rework).
4. visible-slot-order-atomic and visible-stale-segment-tail: take theirs.

Then the `rebuild` lines their side still carries can go (eight in the
merged file; on cargo-test each costs ~8 s a prove and buys nothing), and
two attributes the new lints reject:

- `native/src/layout_hyper/pass2_host.rs:621`,
  `#[allow(clippy::too_many_arguments)]` on scan_item_max_row_extent (tab
  stops): seven parameters, so it suppresses nothing. Delete it.
- `native/src/repo.rs:576`, the same on load_views (step 2): nine
  parameters, a real suppression. `#[expect(..., reason = "...")]`.

The orphans gate needs `pixi run setup` once per checkout (cargo-modules).
Then `cargo glyph test`, with the fixes folded into the merge commit.
