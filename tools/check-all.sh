#!/bin/bash
# check-all.sh — kept as the historical entry point. The tool is `glyph`:
#
#   cargo glyph build          bring the binary and engine dylib up to date
#   cargo glyph test [scope]   run the checks (rust | render | corpus)
#   cargo glyph test --frozen  assert currency instead of building — use this to
#                              validate a commit, so a forgotten rebuild fails
#                              here rather than passing
#   cargo glyph run  [args]    launch the renderer
#   cargo glyph prove          break each check on purpose, require it to notice
#   cargo glyph gates          what each check compares, and cannot see
#
# This script exists so old muscle memory and any external caller keep working.
# It has no logic of its own; if you are reading it hoping to learn what runs,
# read build.toml and glyph/src/main.rs instead.
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo glyph test "$@"
