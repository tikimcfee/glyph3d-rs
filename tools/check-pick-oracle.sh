#!/bin/bash
# Pick-correctness gate: scripted picks on the fixture repo and on a live
# source tree, asserted against an INDEPENDENT python fold oracle reading the
# actual file bytes. Also pixel-pick round trips (row/col pick → compute the
# screen pixel analytically → --pick-px must resolve the same record).
# (Was tools/check-stage-g.sh — the "g" was a fossil stage letter, renamed
# 2026-09-06 when the gates got names; the final ALL PASS line on stdout is
# the load-bearing contract the runner greps for.)
# NOT `set -e`, deliberately. This script already accumulates FAIL and reports
# every mismatch at the end (see the last line) — but -e killed it at the first
# nonzero exit inside a command substitution, so a genuine pick mismatch printed
# two lines, exited 1 with no diagnostic, and silently skipped every remaining
# check. Measured 2026-09-06 with an off-by-one in pick_row_col: 2 lines out of
# ~20, no cause named. Its sibling check-fixture-parity.sh has always been
# written this way; this one now matches.
set -uo pipefail

# Screenshots here are throwaway: they exist so the binary has somewhere to
# write while we read its stdout. They used to go to tracked files in out/ and
# be restored with `git checkout` afterwards, which left the tree dirty whenever
# a run died — and check-all reads the working tree.
SCRATCH="${TMPDIR:-/tmp}/glyph-pick-oracle"
mkdir -p "$SCRATCH"
cd "$(dirname "$0")/.."
BIN=target/release/glyph3d-native
# Unlike its siblings this script had no binary guard for most of its life:
# run out of order it died with a raw shell error instead of a diagnosis.
[ -x "$BIN" ] || { echo "FAIL  $BIN missing — run: cargo glyph build"; exit 1; }
FIX=native/fixtures/g-pick-repo
ORACLE="python3 tools/g_pick_oracle.py"
# The "big real repo" corpus. Was an absolute path into the web repo, which made
# gate 7 of check-all depend on a tree that is no longer trunk. This repo's own
# Rust source is the same KIND of corpus — many files, deep paths, real text — and
# it is always present. Both sides read the same live bytes (binary vs the python
# fold oracle), so an edited corpus stays self-consistent; only the file set and
# depth matter. GLYPH_JS still overrides, for anyone who wants the JS tree back.
REPO=${GLYPH_JS:-$PWD/native/src}
FAIL=0

assert_char() { # desc, line, expected_char, expected_byte
    local desc="$1" line="$2" echar="$3" ebyte="$4"
    if echo "$line" | grep -qF "char='$echar'" && echo "$line" | grep -qF "byte=$ebyte "; then
        echo "PASS  $desc — $line"
    else
        echo "FAIL  $desc — expected char='$echar' byte=$ebyte, got: $line"; FAIL=1
    fi
}

echo "── fixture repo: deterministic row/col picks ──────────────────"
OUT=$($BIN --load-repo $FIX --screenshot "$SCRATCH/fixture.png" \
  --pick-file alpha.rs --pick-row 0 --pick-col 0 \
  --pick-file alpha.rs --pick-row 0 --pick-col 3 \
  --pick-file alpha.rs --pick-row 4 --pick-col 4 \
  --pick-file wide.txt --pick-row 1 --pick-col 50 \
  --pick-file wide.txt --pick-row 2 --pick-col 0 \
  --pick-file wide.txt --pick-row 596 --pick-col 32900 \
  --pick-file wide.txt --pick-row 1995 --pick-col 52800 \
  --pick-file deep.py  --pick-row 0 --pick-col 5 \
  --pick-file deep.py  --pick-row 2 --pick-col 8 \
  --pick-file long.md  --pick-row 200 --pick-col 5 \
  --pick-file long.md  --pick-row 0  --pick-col 0 \
  2>&1)
echo "$OUT" | grep -E "fold cross-check: FAIL" && { echo "FAIL: fold cross-check mismatch"; FAIL=1; }
echo "$OUT" | grep -E "fold cross-check: PASS" | wc -l | xargs echo "fold cross-checks PASS:"

# oracle-derived expectations
exp() { $ORACLE "$1" "$2" "$3" | sed -E "s/.*char=(.*) byte=([0-9]+).*/\1 \2/"; }
check() { # file_substr row col
    local sub="$1" row="$2" col="$3"
    local line; line=$(echo "$OUT" | grep -E "^pick: \S*$sub" | grep -E " row=$row col=$col " | head -1)
    local e; e=$(exp "$FIX/$sub" "$row" "$col")
    # char may be quoted by python repr with double quotes for a quote char; normalize
    local echar ebyte
    ebyte=$(echo "$e" | awk '{print $NF}')
    echar=$(echo "$e" | sed -E "s/ [0-9]+$//")
    echar=${echar#\'}; echar=${echar%\'}
    if [ -z "$line" ]; then echo "FAIL  $sub row=$row col=$col — no pick line"; FAIL=1; return; fi
    if echo "$line" | grep -qF "byte=$ebyte "; then
        echo "PASS  $sub row=$row col=$col byte=$ebyte — $line"
    else
        echo "FAIL  $sub row=$row col=$col — expected byte=$ebyte char=$echar, got: $line"; FAIL=1
    fi
}
check alpha.rs 0 0
check alpha.rs 0 3
check alpha.rs 4 4
# wide.txt is the WRAP-DEPTH fixture: one file whose lines escalate from 80
# chars to 250 000, so a single load spans wrap segment 0 through 2500. `col`
# is the column within the LOGICAL line, not the screen column, so a deep row
# needs a large col — row 1995 is col 52800 of line 8, at z = -79.20.
# The shallow pair still matters (it is the case where z stays 0); the deep
# pairs are the ones the old 257-byte fixture could not express at all.
#
# THE ROW NUMBERS MOVED on 2026-09-04 (the phantom-row fix, engine/delta/):
# six of wide.txt's nine lines have glyph counts that are exact multiples of
# 100, and each used to claim one blank row. These four probes name the SAME
# BYTES as before — only the row they sit on changed (3 -> 2, 600 -> 596,
# 2000 -> 1995), which is the correction, four and five phantom rows deep.
# Row 3 col 0 is now a HOLE: line 2 is 101 cells, so it covers rows 2-3 and
# column 0 sits on row 2. The oracle answers NO RECORD there, which is what
# made this gate abort rather than disagree.
check wide.txt 1 50
check wide.txt 2 0
check wide.txt 596 32900
check wide.txt 1995 52800
check sub/deep.py 0 5
check sub/deep.py 2 8
check long.md 200 5
check long.md 0 0

echo "── pixel-pick round trips (ray path) ──────────────────────────"
pixel_roundtrip() { # focus-file, row, col, expect_rec
    local ff="$1" row="$2" col="$3" erec="$4"
    local out w h lx ly px py
    out=$($BIN --load-repo $FIX --focus-file "$ff" --pick-file "$ff" --pick-row "$row" --pick-col "$col" --screenshot "$SCRATCH/px.png" 2>&1)
    w=$(echo "$out" | grep -oE 'page [0-9.]+x[0-9.]+' | head -1 | sed -E 's/page ([0-9.]+)x([0-9.]+)/\1/')
    h=$(echo "$out" | grep -oE 'page [0-9.]+x[0-9.]+' | head -1 | sed -E 's/page ([0-9.]+)x([0-9.]+)/\2/')
    lx=$(echo "$out" | grep -oE 'pos=\([-0-9.]+,[-0-9.]+' | head -1 | sed -E 's/pos=\(([-0-9.]+),([-0-9.]+)/\1/')
    ly=$(echo "$out" | grep -oE 'pos=\([-0-9.]+,[-0-9.]+' | head -1 | sed -E 's/pos=\(([-0-9.]+),([-0-9.]+)/\2/')
    read px py < <(python3 -c "
import math
W,H,lx,ly = $w,$h,$lx,$ly
hw, hh = max(W,1)/2, max(H,1)/2
d = max(hh, hw/1.6)/math.tan(math.radians(20))*1.08 + 2.0
f = 1/math.tan(math.radians(20))
cx, cy = lx + 0.26, ly          # cell center-ish (cell left edge + half advance)
x_ndc = (cx - hw) * f / (1.6 * d); y_ndc = (cy + hh) * f / d
print(round((x_ndc+1)*800), round((1-y_ndc)*500))
")
    local line2
    line2=$($BIN --load-repo $FIX --focus-file "$ff" --pick-px "$px" "$py" --screenshot "$SCRATCH/px.png" 2>&1 | grep -E "^pick: " | head -1)
    if echo "$line2" | grep -qF "rec=$erec "; then
        echo "PASS  px ($px,$py) on $ff — $line2"
    else
        echo "FAIL  px ($px,$py) on $ff — expected rec=$erec, got: $line2"; FAIL=1
    fi
}
pixel_roundtrip alpha.rs 4 4 50
pixel_roundtrip long.md 200 5 7205

echo "── native/src: oracle-asserted picks across files/depths ──────"
OUT=$($BIN --load-repo "$REPO" --screenshot "$SCRATCH/js.png" \
  --pick-file text.rs --pick-row 0 --pick-col 0 \
  --pick-file text.rs --pick-row 12 --pick-col 7 \
  --pick-file engine.rs --pick-row 5 --pick-col 2 \
  --pick-file glyph_scene.rs --pick-row 40 --pick-col 11 \
  --pick-file gpu.rs --pick-row 1 --pick-col 0 \
  2>&1)
echo "$OUT" | grep -E "^pick: " | while read -r line; do echo "  $line"; done
echo "$OUT" | grep -E "fold cross-check: FAIL" && FAIL=1
js_check() { # relfile row col
    local rel="$1" row="$2" col="$3"
    local line e ebyte
    line=$(echo "$OUT" | grep -E "^pick: \S*$(basename "$rel")" | grep -E " row=$row col=$col " | head -1)
    e=$($ORACLE "$REPO/$rel" "$row" "$col" | sed -E "s/.*char=(.*) byte=([0-9]+).*/\2/")
    ebyte="$e"
    if [ -n "$line" ] && echo "$line" | grep -qF "byte=$ebyte "; then
        echo "PASS  $rel row=$row col=$col byte=$ebyte"
    else
        echo "FAIL  $rel row=$row col=$col — expected byte=$ebyte, got: $line"; FAIL=1
    fi
}
js_check text.rs 0 0
js_check text.rs 12 7
js_check engine.rs 5 2
js_check glyph_scene.rs 40 11
js_check gpu.rs 1 0

echo "────────────────────────────────────────────────────────────────"
[ $FAIL -eq 0 ] && echo "PICK ORACLE CHECK: ALL PASS" || { echo "PICK ORACLE CHECK: FAILURES"; exit 1; }
