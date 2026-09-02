#!/bin/bash
# check-mdi.sh — Stage K gate: GPU cull → indirect multi-draw parity.
#
#   1. mdi-probe        the indirect-draw first_instance device-configuration
#                       matrix (exit 0 = the spec expectations hold here)
#   2. per view         `--cull gpu --cull-verify`: the GPU cull's draw args and
#                       backdrop list, read back, are EXACTLY the CPU cull's
#                       decisions; then the `--cull cpu` and `--cull gpu`
#                       renders are byte-equal (cmp)
#   3. multi-chunk      GLYPH_CHUNK_CAP splits the fixture arena into several
#                       chunks so the (segment, chunk) draw-slot split is
#                       exercised: verify, cpu/gpu byte-equal, AND byte-equal
#                       to the single-chunk render (chunking is output-neutral)
#
# Views cover: a single cover segment (text), all-glyph (repo-zoom), partial
# frustum (repo-mid), the LOD backdrop tier (repo-far), the full field
# (repo-wide), and segment-table edits through verbs (hide-group, move-group)
# — the GPU cull reads the table it mirrors row-by-row on every edit.
# Renders land in out/tooling-ab/mdi/ (untracked scratch). Exit 0 = all green.
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
BIN=$ROOT/native/target/release/glyph3d-native
PROBE=$ROOT/native/target/release/mdi-probe
OUT=$ROOT/out/tooling-ab/mdi
mkdir -p "$OUT"
FAIL=0

step() { echo; echo "── $*"; }

step "1/3 mdi-probe — device-configuration × draw-form matrix"
if "$PROBE" > "$OUT/mdi-probe.log" 2>&1; then
  grep -E '^(adapter|RESULT)' "$OUT/mdi-probe.log"
  echo "PASS  mdi-probe"
else
  tail -25 "$OUT/mdi-probe.log"; echo "FAIL  mdi-probe"; FAIL=1
fi

# view <name> <args...>: gpu(+verify) and cpu renders, then cmp.
view() {
  local name=$1; shift
  local glog="$OUT/$name-gpu.log" clog="$OUT/$name-cpu.log"
  (cd native && GLYPH_CULL_DEBUG=1 "$BIN" "$@" --frames 2 --cull gpu --cull-verify \
      --screenshot "$OUT/$name-gpu.png" > "$glog" 2>&1)
  local grc=$?
  (cd native && GLYPH_CULL_DEBUG=1 "$BIN" "$@" --frames 2 --cull cpu \
      --screenshot "$OUT/$name-cpu.png" > "$clog" 2>&1)
  local crc=$?
  local verify dbg
  verify=$(grep -E '^cull-verify:' "$glog" | head -1)
  dbg=$(grep -E '^CULLDBG' "$clog" | head -1 | sed 's/^CULLDBG //')
  if [ "$grc" = 0 ] && grep -q '^cull-verify: EXACT' "$glog"; then
    echo "PASS  $name  verify ${verify#cull-verify: }"
  else
    echo "FAIL  $name  verify (rc=$grc): ${verify:-no cull-verify line}"; tail -6 "$glog"; FAIL=1
  fi
  if [ "$crc" = 0 ] && cmp -s "$OUT/$name-gpu.png" "$OUT/$name-cpu.png"; then
    echo "PASS  $name  cpu vs gpu BYTE-EQUAL  [cpu cull: $dbg]"
  else
    echo "FAIL  $name  cpu vs gpu DIFFER (cpu rc=$crc)"; FAIL=1
  fi
}

step "2/3 per view — GPU args == CPU cull (exact), renders byte-equal"
view text      --render-file fixtures/baseline-view.txt
view repo-wide --load-repo fixtures/g-pick-repo
view repo-zoom --load-repo fixtures/g-pick-repo --focus-file alpha.rs --zoom 3
view repo-mid  --load-repo fixtures/g-pick-repo --focus-file alpha.rs --zoom 1.2
view repo-far  --load-repo fixtures/g-pick-repo --zoom 0.01
view repo-hide --load-repo fixtures/g-pick-repo --pick-file alpha.rs --pick-row 0 --pick-col 0 --verb hide-group
view repo-move --load-repo fixtures/g-pick-repo --zoom 0.5 --pick-file wide.txt --pick-row 1 --pick-col 1 --verb "move-group 40 0 0"

step "3/3 multi-chunk arena (GLYPH_CHUNK_CAP) — draw-slot split across chunks"
for cap in 977 4096; do
  glog="$OUT/chunk$cap-gpu.log"; clog="$OUT/chunk$cap-cpu.log"
  (cd native && GLYPH_CHUNK_CAP=$cap GLYPH_CULL_DEBUG=1 "$BIN" --load-repo fixtures/g-pick-repo --frames 2 \
      --cull gpu --cull-verify --screenshot "$OUT/chunk$cap-gpu.png" > "$glog" 2>&1)
  grc=$?
  (cd native && GLYPH_CHUNK_CAP=$cap "$BIN" --load-repo fixtures/g-pick-repo --frames 2 \
      --cull cpu --screenshot "$OUT/chunk$cap-cpu.png" > "$clog" 2>&1)
  crc=$?
  chunks=$(grep -oE 'in [0-9]+ chunk\(s\)' "$glog" | head -1)
  verify=$(grep -E '^cull-verify:' "$glog" | head -1)
  if [ "$grc" = 0 ] && grep -q '^cull-verify: EXACT' "$glog"; then
    echo "PASS  cap=$cap ($chunks)  verify ${verify#cull-verify: }"
  else
    echo "FAIL  cap=$cap verify (rc=$grc): ${verify:-no cull-verify line}"; tail -6 "$glog"; FAIL=1
  fi
  if [ "$crc" = 0 ] && cmp -s "$OUT/chunk$cap-gpu.png" "$OUT/chunk$cap-cpu.png" \
       && cmp -s "$OUT/chunk$cap-gpu.png" "$OUT/repo-wide-gpu.png"; then
    echo "PASS  cap=$cap  cpu vs gpu vs single-chunk BYTE-EQUAL"
  else
    echo "FAIL  cap=$cap  renders diverge (cpu rc=$crc)"; FAIL=1
  fi
done

echo
if [ "$FAIL" = 0 ]; then echo "CHECK-MDI: ALL PASS"; else echo "CHECK-MDI: FAILURES — see above"; exit 1; fi
