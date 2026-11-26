#!/usr/bin/env bash
# tools/vt-diff.sh — Differential fidelity & flood-throughput harness for
# `glyph-vt` against a scratch `g3d-termd` daemon (`libghostty-vt`).
#
# Safety rules (`out/WALL-PLAN-2026-11-26.md`):
# - NEVER touches namespaces `wall`, `agents`, `default`, or `rs-wall`.
# - Creates an isolated 0700 directory `D=/tmp/g3d-vt-$$` (< 100-byte socket path),
#   exports `G3D_TERMD_DIR=$D`, runs solely against namespace `probe`, and stops
#   the scratch daemon + removes `$D` on exit or signal.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

D="/tmp/g3d-vt-$$"
NS="probe"
G3D_BIN="${G3D_BIN:-/Users/lugo/.local/bin/g3d}"

cleanup() {
    if [[ -x "$G3D_BIN" && -d "$D" ]]; then
        G3D_TERMD_DIR="$D" "$G3D_BIN" --ns "$NS" stop >/dev/null 2>&1 || true
    fi
    if command -v /opt/homebrew/bin/tmux >/dev/null 2>&1; then
        /opt/homebrew/bin/tmux -L "vt-diff-$$" kill-server >/dev/null 2>&1 || true
    fi
    rm -rf "$D"
}
trap cleanup EXIT INT TERM

rm -rf "$D"
mkdir -m 700 "$D"
export G3D_TERMD_DIR="$D"
export G3D_TERMD_ENV_SOURCE=daemon

if [[ -d "/Library/Developer/CommandLineTools" && -z "${DEVELOPER_DIR:-}" ]]; then
    export DEVELOPER_DIR="/Library/Developer/CommandLineTools"
fi

if [[ "${VT_DIFF_SKIP_BUILD:-0}" != "1" ]]; then
    cargo build --release -p termd-client --example vt_diff
fi
"$ROOT/target/release/examples/vt_diff" --termd-dir "$D" --ns "$NS" --tmux-label "vt-diff-$$" "$@"
