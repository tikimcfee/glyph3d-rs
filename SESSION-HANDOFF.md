# SESSION-HANDOFF — worktree `workspace-random-experiments` (2026-09-20)

Written by the ZCode session that set this tree up (Ivan + CF-z), for the
next session working here. Untracked on purpose — it is session scratch,
not repo content.

## What this tree is

- Worktree of the main repo, branch `worktree-workspace-random-experiments`,
  cut from `main` @ `6cc0d9e` on 2026-09-20 ~10:38.
- Purpose (Ivan's words): long-running random experiments, sometimes merging
  back into main.

## Setup already done — do NOT redo

- `pixi install` — done (~10:38). `.pixi/` is untracked and present.
- `pixi run build-engine` — done. `native/libglyph_engine.dylib` present
  (untracked, per-worktree; this is the known trap, see root AGENTS.md).
- Renderer built via `pixi run build-native` — `target/release/glyph3d-native` present.
- `engine/bench/bench.bin` intentionally absent; the checks only compile
  benches, so nothing needs it.

## The anomaly found here (UNRESOLVED — do not paper over it)

`cargo glyph test` in this tree errored INSTANTLY with the RENDERER's clap
error — `Usage: glyph3d-native`, rejecting `--quiet` — with zero battery
output. The hand-typed expansion of the same alias:

    cd <this worktree> && cargo run --quiet --release -p glyph -- test

RAN the battery correctly (products / manifest / committed-artifacts /
vendor-hashes all PASS before the output was truncated). NOTE: the battery
has NOT been run to completion in this tree yet — a green baseline is not
established.

Verified non-causes (already checked, don't redo):

- `.cargo/config.toml` is git-tracked and byte-identical to main's; the
  alias is `["run", "--quiet", "--release", "-p", "glyph", "--"]`.
- Same cargo binary as main: Homebrew 1.98.0 at `/opt/homebrew/bin/cargo`;
  no rustup, no `rust-toolchain*` files.
- No shadowing cargo configs above this tree: checked `~/.cargo/config.toml`,
  `viz-native/.cargo`, `localdev/.cargo`, `repo/.claude/.cargo`,
  `repo/.claude/worktrees/.cargo` — none exist.

## What NOT to trust (polluted probes from the first session)

- `cargo glyph --help` was an INVENTED probe, not a documented verb, and
  `--help` after an alias is edge-case territory in cargo. Only ONE
  documented verb (`test`) was ever tried in this tree.
- The "it works in main" comparison was UNCONTROLLED: another agent was
  actively building in the main checkout at the same time. That also
  explains main's `target/release/glyph3d-native` being rebuilt ~10:56 —
  not a mystery, not this worktree's doing.
- Do NOT run probes in the main checkout while that agent works there.
- Shell cwd persists between Bash calls here; late-session observations
  drifted back to main once and had to be discounted. Pin every command:
  `cd <worktree> && ...` in the same command line.

## First clean experiment (cheap, read-only, pinned to this tree)

One command, back-to-back, `pwd` echoed:

1. `ls -la target/release/glyph && file target/release/glyph` — confirm
   the tool binary on disk is really the tool (2.1 MB-ish, not the 23 MB
   renderer).
2. `cargo glyph gates` — a documented, read-only verb.
3. `cargo run --release -p glyph -- gates` — the hand-typed twin.

That separates which-binary-is-on-disk / what-the-alias-does /
what-the-verb-does, with no invented invocations.

## Practical fallback

The hand-typed door demonstrably works in this tree:

    cd <this worktree> && cargo run --release -p glyph -- <verb>

Experiments need not block on the alias mystery — but the mystery stays
open until someone runs the experiment above and lands a real diagnosis.

— CF-z (ZCode), 2026-09-20
