#!/usr/bin/env bash
# M2 item (b): is `apply_and_emit`'s quoted 171 ms (out/CUBECL-PERFORMANCE-
# HANDOFF-2026-10-05.md) real? 3 GB in 171 ms is 18 GB/s on a ~100 GB/s part;
# on the RTX 5090 the same kernel ran at 450 GB/s. Only relevant if anyone
# argues for keeping the chain, so this script is a no-op once the cubecl
# engine is gone from the binary: it checks `--help` first.
#
# `GLYPH_CHAIN_PROF=stages` adds one blocking resolve per stage, so the
# `backend` total it prints is NOT the production figure; read the per-kernel
# rows and the SUM, and sanity-check each implied bandwidth against the part.
source "$(dirname "${BASH_SOURCE[0]}")/00_env.sh"
ensure_renderer
BIN="$REPO_ROOT/target/release/glyph3d-native"

if ! "$BIN" --help 2>&1 | grep -q "cubecl"; then
  echo "02: this binary has no cubecl engine (retired) — nothing to measure."
  exit 0
fi

banner "02 CubeCL per-kernel timestamps on $GLYPH_FLAGSHIP_REPO"
OUT="$RESULTS/02-cubecl-stages-$STAMP.txt"
{
  for i in 1 2 3; do
    echo "--- run $i (load $(loadavg))"
    GLYPH_CHAIN_PROF=stages "$BIN" --load-repo "$GLYPH_FLAGSHIP_REPO" \
      --repo-engine cubecl --field-mode derived \
      --screenshot /tmp/m2-cubecl.png --frames 1 2>&1 \
      | grep -E "chain-prof|^  [a-z_]+ +[0-9.]+ms|SUM|phases:|dispatch breakdown|allocate_chain_buffers|prewarm" \
      | sed -E 's/^[0-9T:.Z-]+ +INFO +//'
  done
} | tee "$OUT"
echo "written $OUT"
