#!/bin/bash
# Stage G pick-correctness gate: scripted picks on the fixture repo and on
# glyph3d-js, asserted against an INDEPENDENT python fold oracle reading the
# actual file bytes. Also pixel-pick round trips (row/col pick → compute the
# screen pixel analytically → --pick-px must resolve the same record).
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=native/target/release/glyph3d-native
FIX=native/fixtures/g-pick-repo
ORACLE="python3 tools/g_pick_oracle.py"
REPO=${GLYPH_JS:-/Users/lugo/localdev/viz-web/glyph3d-js}
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
OUT=$($BIN --load-repo $FIX --screenshot out/g-check-fixture.png \
  --pick-file alpha.rs --pick-row 0 --pick-col 0 \
  --pick-file alpha.rs --pick-row 0 --pick-col 3 \
  --pick-file alpha.rs --pick-row 4 --pick-col 4 \
  --pick-file wide.txt --pick-row 1 --pick-col 100 \
  --pick-file wide.txt --pick-row 2 --pick-col 205 \
  --pick-file wide.txt --pick-row 3 --pick-col 0 \
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
check wide.txt 1 100
check wide.txt 2 205
check wide.txt 3 0
check sub/deep.py 0 5
check sub/deep.py 2 8
check long.md 200 5
check long.md 0 0

echo "── pixel-pick round trips (ray path) ──────────────────────────"
pixel_roundtrip() { # focus-file, row, col, expect_rec
    local ff="$1" row="$2" col="$3" erec="$4"
    local out w h lx ly px py
    out=$($BIN --load-repo $FIX --focus-file "$ff" --pick-file "$ff" --pick-row "$row" --pick-col "$col" --screenshot out/g-check-px.png 2>&1)
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
    line2=$($BIN --load-repo $FIX --focus-file "$ff" --pick-px "$px" "$py" --screenshot out/g-check-px.png 2>&1 | grep -E "^pick: " | head -1)
    if echo "$line2" | grep -qF "rec=$erec "; then
        echo "PASS  px ($px,$py) on $ff — $line2"
    else
        echo "FAIL  px ($px,$py) on $ff — expected rec=$erec, got: $line2"; FAIL=1
    fi
}
pixel_roundtrip alpha.rs 4 4 50
pixel_roundtrip long.md 200 5 7205

echo "── glyph3d-js: oracle-asserted picks across files/depths ──────"
OUT=$($BIN --load-repo "$REPO" --screenshot out/g-check-js.png \
  --pick-file packages/glyph3d-core/src/GlyphField.js --pick-row 0 --pick-col 0 \
  --pick-file packages/glyph3d-core/src/GlyphField.js --pick-row 12 --pick-col 7 \
  --pick-file core/glyphVertex.js --pick-row 5 --pick-col 2 \
  --pick-file liveTrie.js --pick-row 40 --pick-col 11 \
  --pick-file README.md --pick-row 1 --pick-col 0 \
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
js_check packages/glyph3d-core/src/GlyphField.js 0 0
js_check packages/glyph3d-core/src/GlyphField.js 12 7
js_check packages/glyph3d-core/src/core/glyphVertex.js 5 2
js_check packages/glyph3d-core/src/compute/liveTrie.js 40 11
js_check README.md 1 0

echo "────────────────────────────────────────────────────────────────"
[ $FAIL -eq 0 ] && echo "STAGE G PICK CHECK: ALL PASS" || { echo "STAGE G PICK CHECK: FAILURES"; exit 1; }
