#!/usr/bin/env bash
# Shared setup for the M2 (Apple Silicon, unified memory) measurement plan of
# out/GPU-DIRECTION-2026-10-09.md. Source this from the numbered scripts.
#
# Required environment:
#   GLYPH_FLAGSHIP_REPO   the flagship corpus checkout (the retired JS repo)
# Optional:
#   GLYPH_LINUX_REPO      a Linux kernel checkout, for 03_jit_layout.sh
#   GLYPH_M2_ROUNDS       interleaved rounds per configuration (default 6; the
#                         first is discarded as warm-up)
#
# Nothing here hardcodes a machine, a user or a path: set the variables above.
set -euo pipefail

: "${GLYPH_FLAGSHIP_REPO:?set GLYPH_FLAGSHIP_REPO to the flagship corpus directory}"
GLYPH_M2_ROUNDS="${GLYPH_M2_ROUNDS:-6}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"
RESULTS="$HERE/results"
mkdir -p "$RESULTS"
STAMP="$(date +%Y%m%d-%H%M%S)"

export RUST_LOG=glyph3d_native=info NO_COLOR=1 RUST_LOG_STYLE=never

loadavg() {
  # macOS has no /proc; `uptime` carries the same three figures.
  uptime | sed -E 's/.*load averages?: *//'
}

banner() {
  echo
  echo "== $* =="
  echo "   host: $(uname -m) $(uname -s) | load: $(loadavg) | $(date -Iseconds)"
}

ensure_renderer() {
  # The battery's own build scope (workspace, release); never package scope.
  if [ ! -x "$REPO_ROOT/target/release/glyph3d-native" ]; then
    (cd "$REPO_ROOT" && cargo build --release --workspace)
  fi
}
