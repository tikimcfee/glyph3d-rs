#!/usr/bin/env bash
# check-fixture-parity.sh — the reference-port gate: the Rust layout forms held
# to the JS oracle's recorded answers over the whole committed corpus.
#
# Five instruments, each over engine/fixtures, each diffing lanes computed by
# a Rust form against the oracle's own expected lanes in the same file:
#
#   --fixture-trie       every fixture's trie rebuilt from its own BYTES by the
#                        ported GlyphTrie and compared through the wire-order
#                        serializer against the trie the oracle stored. Not a
#                        round trip: block layout, insertion ORDER and the
#                        decoder choice are all under test.
#   --fixture-fold       the serial fold (fold.rs) over every fixture, EVERY
#                        lane of EVERY byte bit-exact, plus the miss list,
#                        leader count, per-item boxes and batch union.
#                        Non-leader bytes too — zero is their defined state.
#   --fixture-scan       the same fold as a segmented monoid scan (scan.rs) at
#                        a SWEEP of chunk/group/shard tunings, under the tiered
#                        contract. Fails if no leader reached the BIT tier.
#   --fixture-bake       the streaming record AND its seed protocol (bake.rs)
#                        replayed against the .bake.bin fixtures. Fails if no
#                        query ran.
#   --fixture-reference  text.rs's independent CPU fold against every fixture
#                        inside its domain. Fails if nothing was in domain.
#
# The parse itself is checked indirectly by all five (every lane they compare
# is computed FROM the parsed values) and directly by the loader's
# full-consumption check. Until 2026-09-30 a sixth half compared the parse
# against the retired Mojo loader; that second opinion is gone.
#
# NOT `set -e`: a nonzero exit inside a command substitution must not skip
# the rest of the report (see check-pick-oracle.sh). Every instrument runs and
# reports; the exit status is the union.
#
# The fixture COUNTS are declared here, not derived from the tree: a fixture
# deleted from engine/fixtures must turn this red, not shrink both sides of the
# comparison (build.toml's fixtures artifact declares the same 26 + 8 for the
# corpus rebuild; fixture.rs / bake.rs pin them a third time). Update
# deliberately.
set -uo pipefail
shopt -s nullglob
cd "$(dirname "$0")/.."
BIN=target/release/glyph3d-native
PIPE_EXPECTED=26
BAKE_EXPECTED=8
FAIL=0

[ -x "$BIN" ] || { echo "FAIL  $BIN not built (cargo build --release)"; exit 1; }

FIX=(engine/fixtures/*.pipe.bin)
BAKE=(engine/fixtures/*.bake.bin)
if [ "${#FIX[@]}" -ne "$PIPE_EXPECTED" ]; then
    echo "FAIL  ${#FIX[@]} .pipe.bin fixtures found, $PIPE_EXPECTED declared — refusing to compare a different corpus"
    exit 1
fi
if [ "${#BAKE[@]}" -ne "$BAKE_EXPECTED" ]; then
    echo "FAIL  ${#BAKE[@]} .bake.bin fixtures found, $BAKE_EXPECTED declared — refusing to compare a different corpus"
    exit 1
fi

# run <label> <flag> <files...>: the instrument's own summary line is the
# volume claim, so it is surfaced verbatim, PASS or FAIL.
run() {
    local label=$1 flag=$2
    shift 2
    local out
    if out=$(GLYPH_TRACE=warn "$BIN" "$flag" "$@" 2>&1); then
        echo "PASS  $label — $(echo "$out" | tail -1)"
    else
        echo "FAIL  $label:"
        echo "$out" | grep -v '^  PASS' | tail -20
        FAIL=1
    fi
}

run "trie rebuild" --fixture-trie "${FIX[@]}"
run "full fold" --fixture-fold "${FIX[@]}"
run "scan form" --fixture-scan "${FIX[@]}"
run "bake" --fixture-bake "${BAKE[@]}"
run "corpus diff" --fixture-reference "${FIX[@]}"

if [ "$FAIL" -eq 0 ]; then
    echo "ALL PASS"
fi
exit "$FAIL"
