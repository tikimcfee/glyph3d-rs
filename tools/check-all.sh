#!/bin/bash
# check-all.sh — the full house gate suite in one command (Stage J).
#
#   1. generators                   — the two Python generators reproduce their
#                                     committed outputs BYTE-IDENTICALLY, and the
#                                     schema's own validation rules run (this is
#                                     the tier check; it had no home in this tree
#                                     until 2026-09-02)
#   2. engine-local/check.sh cpu    — the ten Mojo CPU conformance suites (the
#                                     five GPU suites are parked: max.gpu is not
#                                     in this pixi env, and check.sh says so
#                                     rather than printing a blanket green)
#   3. cargo build --release        — zero warnings (house rule)
#   4. cargo clippy --release       — zero warnings
#   5. cargo test                   — all tests green (19 at Stage J:
#                                     wgsl validation, CLI parity, encase layout)
#   6. --engine-check src/main.rs   — bit-exact engine vs CPU oracle
#   7. tools/check-stage-g.sh       — pick correctness vs python oracle
#   8. four-view byte-equal A/B     — demo/text/repo-zoom/repo-wide re-rendered
#                                     and cmp'd against out/tooling-ab/baseline/
#
# text.png renders native/fixtures/baseline-view.txt (immutable fixture —
# editing it is a conscious re-baseline act, see out/STAGE_I_REPORT.md).
# Exit 0 only when every gate is green.
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
BIN=native/target/release/glyph3d-native
BASE=out/tooling-ab/baseline
SWEEP=out/tooling-ab/sweep
FAIL=0

step() { echo; echo "── $*"; }
warn_count() { grep -cE "^warning" <<<"$1" || true; }

step "1/8 generators reproduce their committed outputs (byte-identical)"
G_OK=1
for g in "tools/gen_real_trie.py --verify-only" "tools/gen_schema.py --check"; do
  # No pipe: the exit code must be the GENERATOR's, not a tail's.
  if OUT=$(python3 $g 2>&1); then
    echo "PASS  ${g%% *} — $(echo "$OUT" | tail -1)"
  else
    echo "FAIL  ${g%% *}"; echo "$OUT" | tail -6; G_OK=0; FAIL=1
  fi
done
[ "$G_OK" = 1 ] || echo "      (a generator drifted from its committed output, or the schema is invalid)"

step "2/8 engine-local/check.sh cpu (ten Mojo conformance suites)"
if OUT=$(./engine-local/check.sh cpu 2>&1); then
  echo "$OUT" | tail -2
  echo "PASS  engine suites"
else
  echo "$OUT" | tail -8; echo "FAIL  engine suites"; FAIL=1
fi

step "3/8 cargo build --release (house rule: zero warnings)"
LOG=$(cd native && cargo build --release 2>&1) || { echo "$LOG"; echo "FAIL  build errored"; exit 1; }
W=$(warn_count "$LOG")
if [ "$W" = 0 ]; then echo "PASS  build — 0 warnings"; else echo "$LOG" | grep -E "^warning" -A4; echo "FAIL  build — $W warnings"; FAIL=1; fi

step "4/8 cargo clippy --release (zero warnings)"
LOG=$(cd native && cargo clippy --release 2>&1) || { echo "$LOG"; echo "FAIL  clippy errored"; exit 1; }
W=$(warn_count "$LOG")
if [ "$W" = 0 ]; then echo "PASS  clippy — 0 warnings"; else echo "$LOG" | grep -E "^warning" -A4; echo "FAIL  clippy — $W warnings"; FAIL=1; fi

step "5/8 cargo test"
LOG=$(cd native && cargo test --release 2>&1); RC=$?
echo "$LOG" | grep "test result"
PASSED=$(grep -c "test result: ok" <<<"$LOG" || true)
if [ $RC = 0 ] && [ "$PASSED" -ge 2 ]; then echo "PASS  tests green"; else echo "FAIL  cargo test (rc=$RC)"; FAIL=1; fi

step "6/8 engine-check (bit-exact vs CPU oracle)"
OUT=$(cd native && ./target/release/glyph3d-native --engine-check src/main.rs 2>&1 | tail -1)
echo "$OUT"
grep -q "engine-check PASS" <<<"$OUT" && echo "PASS  engine-check" || { echo "FAIL  engine-check"; FAIL=1; }

step "7/8 check-stage-g.sh (pick correctness vs python oracle)"
if bash tools/check-stage-g.sh > /tmp/check-stage-g.log 2>&1 && tail -1 /tmp/check-stage-g.log | grep -q "ALL PASS"; then
  echo "PASS  stage-g ALL PASS"
else
  tail -20 /tmp/check-stage-g.log; echo "FAIL  stage-g"; FAIL=1
fi
# stage-g rewrites its scratch proofs; keep tracked artifacts pristine.
git checkout -- out/g-check-*.png 2>/dev/null || true

step "8/8 four-view byte-equal A/B vs $BASE"
mkdir -p "$SWEEP"
(cd native && \
  ./target/release/glyph3d-native --demo --frames 2 --screenshot ../$SWEEP/demo.png >/dev/null 2>&1 && \
  ./target/release/glyph3d-native --render-file fixtures/baseline-view.txt --frames 2 --screenshot ../$SWEEP/text.png >/dev/null 2>&1 && \
  ./target/release/glyph3d-native --load-repo fixtures/g-pick-repo --frames 2 --screenshot ../$SWEEP/repo-wide.png >/dev/null 2>&1 && \
  ./target/release/glyph3d-native --load-repo fixtures/g-pick-repo --frames 2 --focus-file alpha.rs --zoom 3 --screenshot ../$SWEEP/repo-zoom.png >/dev/null 2>&1) \
  || { echo "FAIL  screenshot run errored"; FAIL=1; }
for v in demo text repo-zoom repo-wide; do
  if cmp -s "$BASE/$v.png" "$SWEEP/$v.png"; then
    echo "PASS  $v.png BYTE-EQUAL"
  else
    echo "FAIL  $v.png diverges from baseline — the renderer changed; the commit is wrong"
    FAIL=1
  fi
done

echo
if [ "$FAIL" = 0 ]; then echo "CHECK-ALL: ALL GATES GREEN"; else echo "CHECK-ALL: FAILURES — see above"; exit 1; fi
