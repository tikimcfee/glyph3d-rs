#!/bin/bash
# The CubeCL chain's battery fence. Two modes, two gates:
#   chain — chain-check over a fixture set: the device chain vs the CPU
#           scan reference (counts/rows exact, fold>0 X bit-exact, the
#           records witness).
#   fork  — repo-check on the STANDING fork fixture in STRICT mode: the
#           full chain's records vs the ENGINE's batched records,
#           bit-exact, with the census's m>=3 / seg>=3 buckets proven
#           exercised (so the gate cannot hollow with a cleaned /tmp).
# NOT `set -e` — see check-pick-oracle.sh for the measured reason: a
# nonzero exit inside a command substitution must not skip the report.
set -uo pipefail

cd "$(dirname "$0")/.."
BIN=target/release/glyph3d-native
[ -x "$BIN" ] || { echo "FAIL  $BIN missing — run: cargo glyph build"; exit 1; }

MODE="${1:-}"

case "$MODE" in
chain)
    FAILS=0
    for fx in wrapback-long-line paged-rows paged-cols multi-item cluster-flags; do
        if out=$("$BIN" --cubecl-chain-check "engine/fixtures/$fx.pipe.bin" 2>&1); then
            echo "$out" | grep -q "cubecl-chain-check PASS" \
                && echo "PASS  $fx" \
                || { echo "FAIL  $fx — no PASS line"; FAILS=$((FAILS+1)); }
        else
            echo "FAIL  $fx — exited nonzero"
            echo "$out" | tail -3
            FAILS=$((FAILS+1))
        fi
    done
    [ "$FAILS" -eq 0 ] && echo "ALL PASS" || { echo "FAILURES: $FAILS"; exit 1; }
    ;;
fork)
    export GLYPH_REPO_CHECK_STRICT=1
    if out=$("$BIN" --cubecl-repo-check native/fixtures/cubecl-fork 2>&1); then
        echo "$out" | grep -q "cubecl-repo-check PASS" \
            && echo "$out" | grep -q "strict: bit-exact" \
            && echo "ALL PASS" \
            || { echo "FAIL — PASS/strict lines missing"; echo "$out" | tail -5; exit 1; }
    else
        echo "$out" | tail -5
        exit 1
    fi
    ;;
*)
    echo "usage: $0 chain|fork" >&2
    exit 2
    ;;
esac
