#!/bin/bash
# bench-mdi.sh — Stage K numbers: CPU cull vs GPU cull → indirect multi-draw
# on the glyph3d-js corpus (1,305 files / 95M glyph instances / 2 arena
# chunks), the three Stage F benchmark views, GLYPH_PROFILE=1 timings.
#
#   wide  full field, fit-all camera (1,305 segments → all backdrops)
#   mid   --focus-file liveTrie.js --zoom 0.2 (~13 files of glyphs)
#   zoom  --focus-file liveTrie.js            (one file)
#
# Per view and path: steady-state fps, GPU pass timings, CPU scopes ("cull
# (CPU)" for the CPU path; "pass end (CPU)" = the render-pass replay into the
# Metal encoder for both — the O(segments) indirect-draw loop on the GPU path
# vs the O(visible) direct draws on the CPU path). The GPU run also carries
# --cull-verify, and the two renders are byte-compared.
#
#   GLYPH_JS=<repo dir> FRAMES=<n> bash tools/bench-mdi.sh
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
BIN=$ROOT/native/target/release/glyph3d-native
REPO=${GLYPH_JS:-/Users/lugo/localdev/viz-web/glyph3d-js}
OUT=$ROOT/out/tooling-ab/mdi-bench
FRAMES=${FRAMES:-300}
mkdir -p "$OUT"
FAIL=0

bench() { # name mode args...
  local name=$1 mode=$2; shift 2
  local log="$OUT/$name-$mode.log"
  local t0=$SECONDS
  if [ "$mode" = gpu ]; then
    (cd native && GLYPH_PROFILE=1 GLYPH_CULL_DEBUG=1 "$BIN" --load-repo "$REPO" "$@" --frames "$FRAMES" \
        --cull gpu --cull-verify --screenshot "$OUT/$name-gpu.png" > "$log" 2>&1)
  else
    (cd native && GLYPH_PROFILE=1 GLYPH_CULL_DEBUG=1 "$BIN" --load-repo "$REPO" "$@" --frames "$FRAMES" \
        --cull "$mode" --screenshot "$OUT/$name-$mode.png" > "$log" 2>&1)
  fi
  local rc=$?
  echo "== $name [$mode] rc=$rc wall=$((SECONDS - t0))s"
  grep -E '^(CULLDBG|profile:|offscreen: steady|cull-verify|offscreen: [0-9]+ frame)' "$log"
  grep -E 'glyph field:|cull \(GPU\):' "$log" | sed 's/^.*INFO[^ ]* *//' | head -2
  [ "$rc" = 0 ] || { echo "FAIL  $name [$mode] exited $rc"; tail -5 "$log"; FAIL=1; }
  if [ "$mode" = gpu ] && ! grep -q '^cull-verify: EXACT' "$log"; then echo "FAIL  $name verify"; FAIL=1; fi
}

echo "corpus: $REPO | frames: $FRAMES"
bench wide cpu
bench wide gpu
cmp -s "$OUT/wide-cpu.png" "$OUT/wide-gpu.png" && echo "PASS  wide cpu vs gpu BYTE-EQUAL" || { echo "FAIL  wide cpu vs gpu DIFFER"; FAIL=1; }
bench mid cpu --focus-file liveTrie.js --zoom 0.2
bench mid gpu --focus-file liveTrie.js --zoom 0.2
cmp -s "$OUT/mid-cpu.png" "$OUT/mid-gpu.png" && echo "PASS  mid cpu vs gpu BYTE-EQUAL" || { echo "FAIL  mid cpu vs gpu DIFFER"; FAIL=1; }
bench zoom cpu --focus-file liveTrie.js
bench zoom gpu --focus-file liveTrie.js
cmp -s "$OUT/zoom-cpu.png" "$OUT/zoom-gpu.png" && echo "PASS  zoom cpu vs gpu BYTE-EQUAL" || { echo "FAIL  zoom cpu vs gpu DIFFER"; FAIL=1; }

echo
if [ "$FAIL" = 0 ]; then echo "BENCH-MDI: ALL PASS"; else echo "BENCH-MDI: FAILURES — see above"; exit 1; fi
