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
before. Next is commit 3: cargo-doc on `--workspace` AND `--release` (the
release profile so the gate never shares `target/debug` and its lock with an
editor's rust-analyzer; measured 4.6 s, zero warnings, nothing gated on
`debug_assertions`). The project `rust-analyzer.toml` was deleted, because it
cannot reach the flycheck.

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
- **The cargo-doc gate only documents `-p glyph3d-native`**
  (`glyph/src/gates.rs`), so doc links in `crates/*` and `glyph/` are
  unchecked.
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
