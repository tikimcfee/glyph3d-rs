#!/usr/bin/env bash
# The CPU-side bit-exact instruments of the gates, run unmodified by the rustc
# release renderer and by relax, and compared: exit status, and stdout with the
# per-run timings stripped. Run from anywhere; needs target/release/glyph3d-native
# current (`cargo glyph build`). Outputs land in $OUT. The first relax run compiles
# the renderer's whole graph (about three minutes, cold); later ones reuse its cache.
#
#   tools/relax-fidelity/gates.sh            every instrument
#   tools/relax-fidelity/gates.sh fold trie  the named ones (see `names` below)
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
RELAX=${RELAX:-relax}  # makepad's relax binary (libs/relax): on PATH, or name it in $RELAX
OUT=${OUT:-$here/target/out/gates}
BIN=$root/target/release/glyph3d-native
mkdir -p "$OUT"
cd "$root"
shopt -s nullglob
FIX=(engine/fixtures/*.pipe.bin)
HO=(native/fixtures/cubecl-fork native/fixtures/g-cluster-repo native/fixtures/overflow-leads.txt native/fixtures/g-pick-repo native/fixtures/chunk-cut.txt native/fixtures/chunk-cut-paint.txt)

# name | working dir | env | arguments (the gate commands of build.toml)
declare -A dir env args
add() { dir[$1]=$2; env[$1]=$3; args[$1]=$4; names+=("$1"); }
names=()
add trie      . "" "--fixture-trie ${FIX[*]}"
add fold      . "" "--fixture-fold ${FIX[*]}"
add reference . "" "--fixture-reference ${FIX[*]}"
add ho-leader . "" "--hyper-oracle-check ${HO[*]} --cluster-mode leader"
add ho-strict . "GLYPH_HYPER_ORACLE_STRICT=1" "--hyper-oracle-check ${FIX[*]} ${HO[*]}"
add ho-tab4   . "GLYPH_HYPER_ORACLE_STRICT=1" "--hyper-oracle-check native/fixtures/tab-stops native/fixtures/g-cluster-repo native/fixtures/g-pick-repo --tab-width 4"
add ho-tab8   . "" "--hyper-oracle-check native/fixtures/tab-stops --tab-width 8 --cluster-mode leader"
for e in hyper direct; do
  for m in down back; do
    add "verify-$e-$m" native "" "--load-repo fixtures/g-pick-repo --wrap-mode $m --repo-engine $e --repo-verify --repo-scan-only"
  done
done

pick=("$@")
[ ${#pick[@]} -eq 0 ] && pick=("${names[@]}")
# timings and relax's own banner lines are not results
scrub() { sed -E -e 's/[0-9]+(\.[0-9]+)? ?(MB\/s|ms|µs|us|s)\b/<t>/g' -e '/^makepad/d' -e 's/\x1b\[[0-9;]*m//g' "$1"; }
bad=0
for n in "${pick[@]}"; do
  [ -n "${args[$n]:-}" ] || { echo "unknown instrument $n (have: ${names[*]})"; exit 2; }
  (cd "${dir[$n]}" && env GLYPH_TRACE=warn ${env[$n]} "$BIN" ${args[$n]} > "$OUT/$n.rustc.out" 2> "$OUT/$n.rustc.err"); rc_a=$?
  (cd "${dir[$n]}" && env GLYPH_TRACE=warn ${env[$n]} "$RELAX" run --quiet --allow-c glyph3d-native -- ${args[$n]} > "$OUT/$n.relax.out" 2> "$OUT/$n.relax.err"); rc_b=$?
  if [ $rc_a -eq $rc_b ] && cmp -s <(scrub "$OUT/$n.rustc.out") <(scrub "$OUT/$n.relax.out"); then
    printf 'SAME    %-18s exit %d; %s\n' "$n" $rc_a "$(tail -1 "$OUT/$n.rustc.out" | cut -c1-150)"
  else
    bad=1
    printf 'DIFFERS %-18s exit rustc %d relax %d\n' "$n" $rc_a $rc_b
    diff <(scrub "$OUT/$n.rustc.out") <(scrub "$OUT/$n.relax.out") | head -10
    tail -3 "$OUT/$n.relax.err"
  fi
done
exit $bad
