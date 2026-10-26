#!/usr/bin/env bash
# Every golden view of build.toml rendered offscreen by the rustc release
# renderer and by the same source under relax, each PNG cmp'd against this
# host's baseline set (out/tooling-ab/baseline/<gpu-key>/). The GPU draws the
# pixels, so this is not a CPU-side check, but every input it is given -- the
# layout, the slots, the camera's trig, the colours -- is computed on the CPU by
# whichever compiler built the renderer: a byte-equal frame under relax says
# none of that moved. Needs target/release/glyph3d-native current. $OUT holds
# the frames; `goldens.sh text emoji` renders only the named views. Each view is
# rendered plain and under each settings.golden_equivalents, against the same PNG.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
RELAX=${RELAX:-relax}  # makepad's relax binary (libs/relax): on PATH, or name it in $RELAX
OUT=${OUT:-$here/target/out/goldens}
BIN=$root/target/release/glyph3d-native
mkdir -p "$OUT"
cd "$root/native"
key=$("$BIN" --gpu-key 2>/dev/null | tail -1)
base=$root/out/tooling-ab/baseline/$key
[ -d "$base" ] || { echo "no baseline set for this host's key '$key'"; exit 2; }
bad=0
while IFS='|' read -r name extra cmd; do
  if [ $# -gt 0 ] && [[ " $* " != *" $name "* ]]; then continue; fi
  f=$name${extra:+.${extra##* }}
  "$BIN" $cmd --screenshot "$OUT/$f.rustc.png" > "$OUT/$f.rustc.log" 2>&1
  "$RELAX" run --quiet --allow-c glyph3d-native -- $cmd --screenshot "$OUT/$f.relax.png" > "$OUT/$f.relax.log" 2>&1
  a=DIFFERS; cmp -s "$OUT/$f.rustc.png" "$base/$name.png" && a=BYTE-EQUAL
  b=DIFFERS; cmp -s "$OUT/$f.relax.png" "$base/$name.png" && b=BYTE-EQUAL
  [ $b = BYTE-EQUAL ] || bad=1
  printf '%-18s %-22s rustc %-10s relax %-10s (vs %s/%s.png)\n' "$name" "$extra" $a $b "$key" "$name"
done < <(python3 -c '
import tomllib, sys
m = tomllib.load(open(sys.argv[1], "rb"))
eq = m.get("settings", {}).get("golden_equivalents", [])
for v in m.get("golden_view", []):
    for extra in [""] + eq:
        print(v["name"] + "|" + extra + "|" + (v["cmd"] + " " + extra).strip())
' "$root/build.toml")
exit $bad
