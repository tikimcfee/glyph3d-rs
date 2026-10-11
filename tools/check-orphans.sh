#!/bin/bash
# Orphan-module gate: a .rs file under a crate's src/ that no `mod` declaration
# reaches is not compiled at all, and rustc says nothing about it. The realistic
# loss is a dropped `#[cfg(test)] mod tests;` — its tests stop existing without
# a warning, and only the cargo-test floor might notice, if the count dips far
# enough (root AGENTS.md § cargo-test). cargo-modules walks each target's module
# tree and lists the files it never reaches.
#
# Run under pixi (cargo-modules is built into .pixi/cargo-tools by
# `pixi run setup`). `--cfg-test` because without it every `#[cfg(test)] mod
# tests;` file reads as an orphan. Every LIB target is checked, plus the bins
# whose modules no lib reaches. glyph3d-native's own bin is NOT: main.rs reaches
# its modules through the lib, so cargo-modules reports all of them (19,
# measured 2026-10-11) as orphans of the bin.
#
# Accumulates and reports every target, not just the first failure (the lesson
# of check-pick-oracle.sh's `set -e`); ORPHANS OK on stdout is the contract the
# runner matches.
set -uo pipefail
cd "$(dirname "$0")/.."

command -v cargo-modules >/dev/null || {
    echo "FAIL  orphans — cargo-modules not on PATH: run \`pixi run setup\`, and run this under \`pixi run\`"
    exit 1
}

TARGETS=(
    "glyph3d-native --lib"
    "glyph-field --lib"
    "glyph-field-instanced --lib"
    "glyph-field-derived --lib"
    "glyph-field-visible --lib"
    "glyph-scene-graph --lib"
    "glyph-session-dirs --lib"
    "glyph-scene-graph --bin resolve-bench"
    "glyph --bin glyph"
)

# cargo-modules loads the workspace as rust-analyzer does, which runs
# `cargo check --compile-time-deps --all-targets` (build scripts, proc macros;
# measured with a logging cargo shim, 2026-10-11). Its own target dir, as
# cargo-check-no-ui has, so it never shares target/debug and its lock with an
# editor's rust-analyzer.
export CARGO_TARGET_DIR=target/orphans

FAILED=0
for t in "${TARGETS[@]}"; do
    read -r pkg sel <<<"$t"
    # NO_COLOR: the result lines are matched by the runner and read in logs.
    out=$(NO_COLOR=1 cargo modules orphans -p $pkg $sel --cfg-test --deny 2>&1)
    if [ $? -eq 0 ]; then
        echo "PASS  orphans $pkg $sel"
    else
        FAILED=1
        echo "FAIL  orphans $pkg $sel"
        echo "$out" | sed 's/^/      /'
    fi
done

if [ $FAILED -eq 0 ]; then
    echo "ORPHANS OK — ${#TARGETS[@]} targets, no unreachable module files"
else
    echo "ORPHANS: FAILURES"
    exit 1
fi
