#!/usr/bin/env bash
# check-fixture-parity.sh — stage 0's acceptance test for the reference port.
#
# THE CLAIM: native/src/fixture.rs and engine/fixture_io.mojo read the same
# .pipe.bin the same way. There is no second Rust parser to disagree with, so
# the second opinion is the Mojo one that has been reading this corpus all
# along.
#
# WHAT IT COMPARES: not the files — both sides trivially agree about those —
# but FNV-1a checksums over each side's PARSED, TYPED values, taken after
# strides, field order and the carrier split have been applied. A wrong stride,
# a swapped section, a field read in the wrong order or a measure narrowed at
# the wrong moment all change a checksum. Emitters:
#   Rust: glyph3d-native --fixture-manifest
#   Mojo: engine/fixture_manifest.mojo
#
# Plus the trie rebuild (--fixture-trie, stage 1): every fixture's trie rebuilt
# from its own BYTES by the ported GlyphTrie and compared through the wire-order
# serializer against the trie the oracle stored. Not a round trip — nothing of
# the stored trie's structure is handed back to the builder, so block layout,
# insertion ORDER and the decoder choice are all under test.
#
# Plus the corpus diff (--fixture-diff): text.rs's CPU fold laid against every
# fixture inside its domain and compared BIT-EXACT to the oracle's own expected
# lanes. That half fails if NOTHING was in domain, because a differ that
# compared nothing passes loudest.
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=native/target/release/glyph3d-native
FIX=(engine/fixtures/*.pipe.bin)
FAIL=0

[ -x "$BIN" ] || { echo "FAIL  $BIN not built (cargo build --release)"; exit 1; }
[ "${#FIX[@]}" -gt 0 ] || { echo "FAIL  no fixtures found — this gate would pass vacuously"; exit 1; }

RUST=$(mktemp -t fixparity-rust)
MOJO=$(mktemp -t fixparity-mojo)
trap 'rm -f "$RUST" "$MOJO"' EXIT

if ! "$BIN" --fixture-manifest "${FIX[@]}" >"$RUST" 2>/dev/null; then
    echo "FAIL  rust manifest emitter errored"; cat "$RUST"; exit 1
fi
# The Mojo runner prints compile warnings to stderr; only stdout is the manifest.
if ! pixi run mojo run --fp-mode contract=off -I engine \
        engine/fixture_manifest.mojo "${FIX[@]}" >"$MOJO" 2>/dev/null; then
    echo "FAIL  mojo manifest emitter errored"; cat "$MOJO"; exit 1
fi

R_LINES=$(wc -l <"$RUST" | tr -d ' ')
if [ "$R_LINES" -ne "${#FIX[@]}" ]; then
    echo "FAIL  manifest has $R_LINES lines for ${#FIX[@]} fixtures"; FAIL=1
fi

if diff -u "$MOJO" "$RUST" >/dev/null; then
    echo "PASS  parse parity — ${#FIX[@]} fixtures, 11 section checksums each, Rust == Mojo"
else
    echo "FAIL  parse parity — the two loaders disagree:"
    diff -u "$MOJO" "$RUST" | head -20
    FAIL=1
fi

if OUT=$("$BIN" --fixture-trie "${FIX[@]}" 2>&1); then
    echo "PASS  trie rebuild — $(echo "$OUT" | tail -1)"
else
    echo "FAIL  trie rebuild:"; echo "$OUT" | tail -20; FAIL=1
fi

if OUT=$("$BIN" --fixture-diff "${FIX[@]}" 2>&1); then
    echo "PASS  corpus diff — $(echo "$OUT" | tail -1)"
else
    echo "FAIL  corpus diff:"; echo "$OUT" | tail -20; FAIL=1
fi

exit "$FAIL"
