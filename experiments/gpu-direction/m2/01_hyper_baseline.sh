#!/usr/bin/env bash
# M2 item (a): re-baseline HyperLayout on the flagship, flat and syntax, both
# field modes, with Pass 1 made visible. `bench_ab.py` parses the span-close
# lines, so on this path you get `hyper_pass2_ms`, the `hyper.staging.*` spans
# if the binary still carries them (they were on the research branch only; on
# main the unified Metal path never enters staging, so their absence is
# expected and not a failure), and the backend / visual totals.
#
# Pass 1 is hidden on the prefetch thread with `--repo-engine hyper`; the
# `batch` engine runs the same HyperLayout without the prefetch and prints
# `hyper.pass1`, which is why it is in the list.
#
# Expected on the M2 (quoted in AGENTS.md, 2026-10): ~168 ms backend flat,
# ~215 ms syntax. Anything far from that is a finding, not noise — read the
# load average first.
source "$(dirname "${BASH_SOURCE[0]}")/00_env.sh"
ensure_renderer
banner "01 HyperLayout baseline on $GLYPH_FLAGSHIP_REPO"

OUT="$RESULTS/01-hyper-baseline-$STAMP.txt"
(
  cd "$REPO_ROOT"
  python3 experiments/gpu-direction/bench_ab.py \
    --repo "$GLYPH_FLAGSHIP_REPO" --rounds "$GLYPH_M2_ROUNDS" --warmup 1 \
    hyper-derived-syntax="--repo-engine hyper --field-mode derived" \
    hyper-derived-flat="--repo-engine hyper --field-mode derived --color-mode flat" \
    hyper-instanced-syntax="--repo-engine hyper --field-mode instanced" \
    batch-derived-syntax="--repo-engine batch --field-mode derived"
) | tee "$OUT"
echo "written $OUT"
