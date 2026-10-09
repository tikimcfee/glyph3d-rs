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
            && echo "PASS  cubecl-fork (default chunk)" \
            || { echo "FAIL — PASS/strict lines missing"; echo "$out" | tail -5; exit 1; }
    else
        echo "$out" | tail -5
        exit 1
    fi
    # NOTE (2026-10-09): GLYPH_RECORD_CHUNK is read by NOTHING since the
    # records-mode retirement (d46a6c3), so this second pass repeats the first
    # and fences no window arithmetic. Kept, labelled, until whoever owns the
    # chain decides whether the chunked emitter comes back or the pass goes.
    #
    # (History) The chunked emitter, fenced: shrink the record window so the standing
    # fixture crosses several chunk boundaries (319,628 records since the
    # 2026-09-30 cluster extension = six windows) — the 16.7M
    # default never leaves window zero here, so the window arithmetic
    # (rec_first carry, rolling buffer reuse) was otherwise exercised only
    # by manual 97MB runs. The window base rides a runtime params buffer,
    # so every window shares ONE compiled kernel — this pass pays
    # dispatches and 4-byte uploads, not JIT compiles.
    if out=$(GLYPH_RECORD_CHUNK=60000 "$BIN" --cubecl-repo-check native/fixtures/cubecl-fork 2>&1); then
        echo "$out" | grep -q "cubecl-repo-check PASS" \
            && echo "$out" | grep -q "strict: bit-exact" \
            && echo "PASS  cubecl-fork-chunked (GLYPH_RECORD_CHUNK=60000 — read by nothing since d46a6c3; a repeat of the first pass)" \
            || { echo "FAIL  cubecl-fork-chunked — PASS/strict lines missing"; echo "$out" | tail -5; exit 1; }
    else
        echo "FAIL  cubecl-fork-chunked — exited nonzero"
        echo "$out" | tail -5
        exit 1
    fi
    # (The chunked-arena and readback-hop passes retired at E2b — the copy
    # hop, the mapped-arena hand-off and its write_bytes_at split all died
    # with the endpoint: the scatter writes the renderer-bound buffer
    # directly. Their mutations left the manifest the same day.)
    echo "ALL PASS"
    ;;
*)
    echo "usage: $0 chain|fork" >&2
    exit 2
    ;;
esac
