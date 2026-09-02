#!/bin/bash
# bench-mdi.sh — Stage K numbers: CPU cull vs GPU cull → indirect multi-draw
# on the glyph3d-js corpus (1,305 files / 95M glyph instances / 2 arena
# chunks), the three Stage F benchmark views, GLYPH_PROFILE=1 timings.
#
#   wide  full field, fit-all camera (1,305 segments → all backdrops)
#   mid   --focus-file liveTrie.js --zoom 0.2 (~13 files of glyphs)
#   zoom  --focus-file liveTrie.js            (one file)
#
# Per view and path: steady-state fps and, with PROFILE=1 (default), the GPU
# pass timings and CPU scopes ("cull (CPU)" on the CPU path; "encoder finish
# (CPU)" on both = the Metal encode of the draw list: the O(segments)
# indirect-draw loop on the GPU path vs the O(visible) direct draws on the
# CPU path). PROFILE=0 gives the clean fps (no query resolves, no
# back-pressure stalls). The GPU run also carries --cull-verify, and the two
# renders are byte-compared.
#
#   GLYPH_JS=<repo dir> FRAMES=<n> PROFILE=0|1 bash tools/bench-mdi.sh
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
BIN=$ROOT/native/target/release/glyph3d-native
REPO=${GLYPH_JS:-/Users/lugo/localdev/viz-web/glyph3d-js}
OUT=$ROOT/out/tooling-ab/mdi-bench
FRAMES=${FRAMES:-300}
PROFILE=${PROFILE:-1}
if [ "$PROFILE" = 1 ]; then export GLYPH_PROFILE=1; else unset GLYPH_PROFILE; fi
mkdir -p "$OUT"
FAIL=0

bench() { # name mode args...
  local name=$1 mode=$2; shift 2
  local log="$OUT/$name-$mode.log"
  local t0=$SECONDS
  rm -f "$OUT/$name-$mode.png"
  if [ "$mode" = cpu ]; then
    (cd native && GLYPH_CULL_DEBUG=1 "$BIN" --load-repo "$REPO" "$@" --frames "$FRAMES" \
        --cull cpu --screenshot "$OUT/$name-cpu.png" > "$log" 2>&1)
  else
    (cd native && GLYPH_CULL_DEBUG=1 "$BIN" --load-repo "$REPO" "$@" --frames "$FRAMES" \
        --cull "$mode" --cull-verify --screenshot "$OUT/$name-$mode.png" > "$log" 2>&1)
  fi
  local rc=$?
  echo "== $name [$mode] rc=$rc wall=$((SECONDS - t0))s"
  grep -E '^(CULLDBG|profile:|offscreen: steady|cull-verify|offscreen: [0-9]+ frame)' "$log"
  grep -E 'glyph field:|cull \((Gpu|GpuPacked)\):' "$log" | sed 's/^.*INFO[^ ]* *//' | head -2
  [ "$rc" = 0 ] || { echo "FAIL  $name [$mode] exited $rc"; tail -5 "$log"; FAIL=1; }
  if [ "$mode" != cpu ] && ! grep -q '^cull-verify: EXACT' "$log"; then echo "FAIL  $name [$mode] verify"; FAIL=1; fi
  if [ "$mode" != cpu ]; then
    cmp -s "$OUT/$name-cpu.png" "$OUT/$name-$mode.png" && echo "PASS  $name $mode vs cpu BYTE-EQUAL" \
      || { echo "FAIL  $name $mode vs cpu DIFFER"; FAIL=1; }
  fi
}

echo "corpus: $REPO | frames: $FRAMES | profile: $PROFILE"
for mode in cpu gpu gpu-packed; do bench wide "$mode"; done
for mode in cpu gpu gpu-packed; do bench mid "$mode" --focus-file liveTrie.js --zoom 0.2; done
for mode in cpu gpu gpu-packed; do bench zoom "$mode" --focus-file liveTrie.js; done

echo
if [ "$FAIL" = 0 ]; then echo "BENCH-MDI: ALL PASS"; else echo "BENCH-MDI: FAILURES — see above"; exit 1; fi
