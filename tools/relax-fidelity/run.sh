#!/usr/bin/env bash
# Build and run the harness under rustc (release) and under relax in several
# configurations, and compare each relax output with rustc's. Outputs land in $OUT.
#
#   relax       a warm run (the cache's O2 code links at start)
#   relax-cold  a cold cache and 16 repetitions: baseline code first, O2/O3 builds
#               installed while it runs (the per-repetition self-check shows any change)
#   relax-sse   MAKEPAD_RUN_NO_AVX=1: the SSE encodings, and mul_add through the C fma
#               instead of vfmadd (relax's fma_native() is false without AVX)
#   relax-interp  MAKEPAD_RUN_INTERP=all: the interpreter tier (aarch64 only; on x86-64
#               relax has no interpreter and this configuration is skipped)
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
RELAX=${RELAX:-relax}  # makepad's relax binary (libs/relax): on PATH, or name it in $RELAX
OUT=${OUT:-$here/target/out}
mkdir -p "$OUT"
cd "$here"
cargo build --release --quiet
./target/release/relax-fidelity "$@" > "$OUT/rustc.txt"
prog=("$@")
run() { local name=$1; shift; env "$@" "$RELAX" run --quiet relax-fidelity -- "${prog[@]}" > "$OUT/$name.txt" 2> "$OUT/$name.err"; }
run relax-warmup RELAX_FIDELITY_REPS=1
run relax RELAX_FIDELITY_REPS=3
"$RELAX" clean -p relax-fidelity > /dev/null 2>&1
run relax-cold RELAX_FIDELITY_REPS=16
# its own clean cache: a cached build made with AVX must not stand in for it
"$RELAX" clean -p relax-fidelity > /dev/null 2>&1
run relax-sse RELAX_FIDELITY_REPS=3 MAKEPAD_RUN_NO_AVX=1
"$RELAX" clean -p relax-fidelity > /dev/null 2>&1
configs=(relax relax-cold relax-sse)
if [ "$(uname -m)" = aarch64 ] || [ "$(uname -m)" = arm64 ]; then
  run relax-interp RELAX_FIDELITY_REPS=3 MAKEPAD_RUN_INTERP=all
  configs+=(relax-interp)
fi
for f in rustc "${configs[@]}"; do
  printf '%-13s %s | %s\n' "$f" "$(tail -1 "$OUT/$f.txt")" "$(tail -2 "$OUT/$f.txt" | head -1)"
done
for f in "${configs[@]}"; do
  echo "== rustc vs $f"
  python3 compare.py "$OUT/rustc.txt" "$OUT/$f.txt" "${SHOW:-3}"
  if grep -q '^rep ' "$OUT/$f.err"; then
    echo "   $f changed between repetitions (a tier installed mid-run computes differently):"
    grep -A2 '^rep ' "$OUT/$f.err" | sed 's/^/   /' | head -12
  fi
done
