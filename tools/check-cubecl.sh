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
    # The chunked emitter, fenced: shrink the record window so the standing
    # fixture (278,470 records) crosses five chunk boundaries — the 16.7M
    # default never leaves window zero here, so the window arithmetic
    # (rec_first carry, rolling buffer reuse) was otherwise exercised only
    # by manual 97MB runs. The window base rides a runtime params buffer,
    # so all five windows share ONE compiled kernel — this pass pays
    # dispatches and 4-byte uploads, not JIT compiles.
    if out=$(GLYPH_RECORD_CHUNK=60000 "$BIN" --cubecl-repo-check native/fixtures/cubecl-fork 2>&1); then
        echo "$out" | grep -q "cubecl-repo-check PASS" \
            && echo "$out" | grep -q "strict: bit-exact" \
            && echo "PASS  cubecl-fork-chunked (GLYPH_RECORD_CHUNK=60000, five windows)" \
            || { echo "FAIL  cubecl-fork-chunked — PASS/strict lines missing"; echo "$out" | tail -5; exit 1; }
    else
        echo "FAIL  cubecl-fork-chunked — exited nonzero"
        echo "$out" | tail -5
        exit 1
    fi
    # The chunked ARENA, fenced: force small chunk buffers AND small emit
    # windows so window↔buffer intersections multiply on purpose — 275,058
    # slots over 50,000-slot buffers is six buffers, 60,000-record windows
    # is five windows, and 50,000 % 60,000 != 0 so the boundaries MISALIGN
    # (the copy hop's per-intersection split is the only code that runs
    # there). The instance tier compares per chunk buffer.
    if out=$(GLYPH_ARENA_CHUNK_SLOTS=50000 GLYPH_RECORD_CHUNK=60000 "$BIN" --cubecl-repo-check native/fixtures/cubecl-fork 2>&1); then
        echo "$out" | grep -q "cubecl-repo-check PASS" \
            && echo "$out" | grep -q "strict: bit-exact" \
            && echo "PASS  cubecl-fork-chunked-arena (GLYPH_ARENA_CHUNK_SLOTS=50000, six buffers)" \
            || { echo "FAIL  cubecl-fork-chunked-arena — PASS/strict lines missing"; echo "$out" | tail -5; exit 1; }
    else
        echo "FAIL  cubecl-fork-chunked-arena — exited nonzero"
        echo "$out" | tail -5
        exit 1
    fi
    # The READBACK hop over the mapped arena, fenced: the footprint gate
    # (note 22 step 2) picks copy vs readback by estimated live bytes, and
    # GLYPH_FOOTPRINT_BUDGET=0 forces readback — the combination the gate
    # chooses at the flagship (over budget), where hand_off's
    # write_bytes_at splits the host instance mass across the arena's
    # chunk buffers. Same misaligned shapes as the chunked-arena pass so
    # the split math crosses buffer boundaries; the instance tier compares
    # per chunk buffer, so a wrong rebase lands exactly where it looks.
    if out=$(GLYPH_FOOTPRINT_BUDGET=0 GLYPH_ARENA_CHUNK_SLOTS=50000 GLYPH_RECORD_CHUNK=60000 "$BIN" --cubecl-repo-check native/fixtures/cubecl-fork 2>&1); then
        echo "$out" | grep -q "cubecl-repo-check PASS" \
            && echo "$out" | grep -q "strict: bit-exact" \
            && echo "PASS  cubecl-fork-readback-hop (GLYPH_FOOTPRINT_BUDGET=0, six buffers)" \
            || { echo "FAIL  cubecl-fork-readback-hop — PASS/strict lines missing"; echo "$out" | tail -5; exit 1; }
    else
        echo "FAIL  cubecl-fork-readback-hop — exited nonzero"
        echo "$out" | tail -5
        exit 1
    fi
    echo "ALL PASS"
    ;;
*)
    echo "usage: $0 chain|fork" >&2
    exit 2
    ;;
esac
