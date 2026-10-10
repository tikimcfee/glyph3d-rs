#!/usr/bin/env bash
# M2 item (c): the visible-set prototype where it has the most to gain —
# unified memory, where the bytes are already resident and the competitor is
# CPU-on-demand layout, not PCIe. Runs `jit-layout` on the flagship and, if
# GLYPH_LINUX_REPO is set, on a Linux tree, over the four views.
#
# What to read: per view, the compute-pass GPU ms (expect well under 1 ms for
# page/overview), the `worst` view with segments off vs on (one 64 KiB line
# was 9 ms as a single thread on the 5090), the Pass-1 MB/s at 1/4/8/16/32
# threads (is it bandwidth-bound on the M2 as HyperLayout is?), the resident
# MB against the Metal `max_buffer_size`, and the upload time (on unified
# memory this should be a memcpy, not a transfer).
source "$(dirname "${BASH_SOURCE[0]}")/00_env.sh"

JIT_DIR="$REPO_ROOT/experiments/gpu-direction/jit-layout"
(cd "$JIT_DIR" && cargo build --release)
JIT="$JIT_DIR/target/release/jit-layout"

run_corpus() {
  local name="$1" dir="$2"
  banner "03 jit-layout on $name"
  local out="$RESULTS/03-jit-$name-$STAMP.txt"
  {
    for view in page overview worst random; do
      echo "--- view $view (load $(loadavg))"
      "$JIT" "$dir" --walk renderer --view "$view" --repeat 5 --no-draw
    done
    echo "--- worst, segments off / 2048 / 512 (load $(loadavg))"
    "$JIT" "$dir" --walk renderer --view worst --repeat 5 --no-draw --segment-bytes 65536
    "$JIT" "$dir" --walk renderer --view worst --repeat 5 --no-draw --segment-bytes 2048
    "$JIT" "$dir" --walk renderer --view worst --repeat 5 --no-draw --segment-bytes 512
    echo "--- random 1M lines, with draw (load $(loadavg))"
    "$JIT" "$dir" --walk renderer --view random --visible-lines 1000000 --repeat 5
    echo "--- page, with a screenshot"
    "$JIT" "$dir" --walk renderer --view page --repeat 3 --screenshot "$RESULTS/03-$name-page.png"
  } 2>&1 | tee "$out"
  echo "written $out"
}

run_corpus flagship "$GLYPH_FLAGSHIP_REPO"
if [ -n "${GLYPH_LINUX_REPO:-}" ]; then
  run_corpus linux "$GLYPH_LINUX_REPO"
else
  echo "03: GLYPH_LINUX_REPO unset — skipping the Linux-scale run."
fi
