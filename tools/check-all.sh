#!/bin/bash
# check-all.sh — the full house gate suite in one command (Stage J).
#
#   1. generators                   — the two Python generators AND the atlas
#                                     exporter reproduce their committed outputs
#                                     BYTE-IDENTICALLY, and the schema's own
#                                     validation rules run (the tier check; it had
#                                     no home in this tree until 2026-09-02). The
#                                     atlas gate also keeps tools/vendor/ref honest.
#   2. engine/check.sh            — all sixteen Mojo conformance suites, CPU
#                                     AND GPU, plus the two INSTRUMENTS
#                                     (fixture_census, fixture_manifest). The
#                                     five GPU suites run on Metal since `max`
#                                     became a real dependency (pixi.toml) on
#                                     2026-09-02.
#   3. cargo build --release        — zero warnings (house rule)
#   4. cargo clippy --release       — zero warnings
#   5. cargo test                   — all tests green (19 at Stage J:
#                                     wgsl validation, CLI parity, encase layout)
#   6. --engine-check src/main.rs   — bit-exact engine vs CPU oracle
#   7. tools/check-stage-g.sh       — pick correctness vs python oracle
#   8. four-view byte-equal A/B     — demo/text/repo-zoom/repo-wide re-rendered
#                                     and cmp'd against out/tooling-ab/baseline/
#   9. the reference port           — all five stages, six halves, no JS: parse
#                                     parity (Rust vs Mojo over PARSED values),
#                                     the trie rebuilt from bytes, the serial
#                                     fold over every lane of every byte, the
#                                     scan form across 8 tunings, the bake and
#                                     its seed protocol, and the independent
#                                     text.rs reference over its domain.
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

step "1/9 generators reproduce their committed outputs (byte-identical)"
G_OK=1
for g in "tools/gen_real_trie.py --verify-only" "tools/gen_schema.py --check" "tools/vendor-manifest.py --check"; do
  # No pipe: the exit code must be the GENERATOR's, not a tail's.
  if OUT=$(python3 $g 2>&1); then
    echo "PASS  ${g%% *} — $(echo "$OUT" | tail -1)"
  else
    echo "FAIL  ${g%% *}"; echo "$OUT" | tail -6; G_OK=0; FAIL=1
  fi
done
[ "$G_OK" = 1 ] || echo "      (a generator drifted from its committed output, or the schema is invalid)"
# The atlas exporter is a generator too, and since 2026-09-02 it reads only
# tools/vendor/ref — so this gate is also what keeps the vendored tree honest.
# It aborts on its own slot-set assertion before writing if the fonts or ranges
# drift; the cmp below catches anything that assertion would not.
A_TMP=$(mktemp -d)
if node tools/export-atlas.mjs --out "$A_TMP" >/dev/null 2>&1; then
  A_OK=1
  for b in curves.bin glyphmap.bin glyphs.bin codepoints.bin; do
    cmp -s "assets/atlas/$b" "$A_TMP/$b" || { echo "FAIL  export-atlas — $b differs from the committed asset"; A_OK=0; FAIL=1; }
  done
  [ "$A_OK" = 1 ] && echo "PASS  tools/export-atlas.mjs — 4 atlas bins BYTE-IDENTICAL (vendored inputs, no web repo)"
else
  echo "FAIL  tools/export-atlas.mjs errored"; FAIL=1
fi
rm -rf "$A_TMP"

# THE CORPUS REGENERATES ITSELF. Until 2026-09-04 `engine/fixtures/gen.mjs`
# imported its oracle from the WEB repo, so the 14 .pipe.bin + 8 .bake.bin the
# whole port is gated against could not be rebuilt in this tree at all. The
# inputs are vendored and revision-PINNED now (tools/vendor/PROVENANCE.md), and
# this asserts they still produce the committed bytes.
#
# It DELETES the fixtures first on purpose. Regenerating over them and hashing
# cannot distinguish "reproduced exactly" from "the generator failed and I
# compared the files to themselves" — which is precisely what happened while
# building this, and is why the exit codes are checked too.
step "1b/9 the fixture corpus regenerates byte-identically from vendored inputs"
FX=engine/fixtures
F_BEFORE=$( (cd $FX && shasum -a 256 *.pipe.bin *.bake.bin) | shasum -a 256 )
F_COUNT=$(ls $FX/*.pipe.bin $FX/*.bake.bin | wc -l | tr -d ' ')
rm -f $FX/*.pipe.bin $FX/*.bake.bin
( cd $FX && node gen.mjs >/dev/null 2>&1 ); G1=$?
( cd $FX && node gen-bake.mjs >/dev/null 2>&1 ); G2=$?
F_AFTER_COUNT=$(ls $FX/*.pipe.bin $FX/*.bake.bin 2>/dev/null | wc -l | tr -d ' ')
F_AFTER=$( (cd $FX && shasum -a 256 *.pipe.bin *.bake.bin 2>/dev/null) | shasum -a 256 )
if [ $G1 -ne 0 ] || [ $G2 -ne 0 ]; then
  echo "FAIL  fixture generators errored (gen=$G1 gen-bake=$G2) — the corpus is not rebuildable"; FAIL=1
elif [ "$F_AFTER_COUNT" != "$F_COUNT" ]; then
  echo "FAIL  regenerated $F_AFTER_COUNT fixtures, expected $F_COUNT"; FAIL=1
elif [ "$F_BEFORE" = "$F_AFTER" ]; then
  echo "PASS  $F_COUNT fixtures deleted and rebuilt BYTE-IDENTICAL from engine/fixtures/inputs/"
else
  echo "FAIL  a regenerated fixture differs from the committed one:"
  git diff --stat -- $FX | tail -8; FAIL=1
fi
# Restore the BINARIES only. `git checkout -- $FX` would also revert the
# generators and the vendored inputs, which silently undoes any uncommitted
# work in this directory — it wiped a live edit while this gate was being
# written, and turned three mutation reds into one artifact wearing three hats.
git checkout -- $FX/*.pipe.bin $FX/*.bake.bin 2>/dev/null || true

step "2/9 engine/check.sh (16 Mojo conformance suites + 2 instruments, CPU + GPU)"
if OUT=$(./engine/check.sh 2>&1); then
  echo "$OUT" | tail -2
  echo "PASS  engine suites"
else
  echo "$OUT" | tail -8; echo "FAIL  engine suites"; FAIL=1
fi

step "3/9 cargo build --release (house rule: zero warnings)"
LOG=$(cd native && cargo build --release 2>&1) || { echo "$LOG"; echo "FAIL  build errored"; exit 1; }
W=$(warn_count "$LOG")
if [ "$W" = 0 ]; then echo "PASS  build — 0 warnings"; else echo "$LOG" | grep -E "^warning" -A4; echo "FAIL  build — $W warnings"; FAIL=1; fi

step "4/9 cargo clippy --release (zero warnings)"
LOG=$(cd native && cargo clippy --release 2>&1) || { echo "$LOG"; echo "FAIL  clippy errored"; exit 1; }
W=$(warn_count "$LOG")
if [ "$W" = 0 ]; then echo "PASS  clippy — 0 warnings"; else echo "$LOG" | grep -E "^warning" -A4; echo "FAIL  clippy — $W warnings"; FAIL=1; fi

step "5/9 cargo test"
LOG=$(cd native && cargo test --release 2>&1); RC=$?
echo "$LOG" | grep "test result"
PASSED=$(grep -c "test result: ok" <<<"$LOG" || true)
if [ $RC = 0 ] && [ "$PASSED" -ge 2 ]; then echo "PASS  tests green"; else echo "FAIL  cargo test (rc=$RC)"; FAIL=1; fi

step "6/9 engine-check (bit-exact vs CPU oracle)"
OUT=$(cd native && ./target/release/glyph3d-native --engine-check src/main.rs 2>&1 | tail -1)
echo "$OUT"
grep -q "engine-check PASS" <<<"$OUT" && echo "PASS  engine-check" || { echo "FAIL  engine-check"; FAIL=1; }
# The SAME gate on malformed leads. src/main.rs is well-formed UTF-8 by
# construction (rustc enforces it), so the out-of-range decode path could never
# appear in it — and that path is where the two implementations DISAGREED until
# 2026-09-02: Mojo read off the end of its block index, Rust asserted. This
# fixture is the only input in the tree that reaches it.
OUT=$(cd native && ./target/release/glyph3d-native --engine-check fixtures/overflow-leads.txt 2>&1 | tail -1)
echo "$OUT"
grep -q "engine-check PASS" <<<"$OUT" && echo "PASS  engine-check (overflow leads)" || { echo "FAIL  engine-check (overflow leads)"; FAIL=1; }

step "7/9 check-stage-g.sh (pick correctness vs python oracle)"
if bash tools/check-stage-g.sh > /tmp/check-stage-g.log 2>&1 && tail -1 /tmp/check-stage-g.log | grep -q "ALL PASS"; then
  echo "PASS  stage-g ALL PASS"
else
  tail -20 /tmp/check-stage-g.log; echo "FAIL  stage-g"; FAIL=1
fi
# stage-g rewrites its scratch proofs; keep tracked artifacts pristine.
git checkout -- out/g-check-*.png 2>/dev/null || true

step "8/9 four-view byte-equal A/B vs $BASE"
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

step "9/9 the reference port: parse parity, trie, fold, scan, bake vs the JS oracle"
if OUT=$(tools/check-fixture-parity.sh 2>&1); then
  echo "$OUT"
else
  echo "$OUT"; FAIL=1
fi

echo
if [ "$FAIL" = 0 ]; then echo "CHECK-ALL: ALL GATES GREEN"; else echo "CHECK-ALL: FAILURES — see above"; exit 1; fi
