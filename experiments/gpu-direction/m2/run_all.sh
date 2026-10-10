#!/usr/bin/env bash
# The whole M2 plan from out/GPU-DIRECTION-2026-10-09.md §5, in order. Each
# script is independent and re-runnable; results land in ./results/ (ignored
# by git — paste the tables into the report by hand, with the load average).
#
#   GLYPH_FLAGSHIP_REPO=/path/to/flagship GLYPH_LINUX_REPO=/path/to/linux \
#     experiments/gpu-direction/m2/run_all.sh
#
# Run it when the machine is otherwise idle; every table prints the load
# average next to itself so a busy run is visibly a busy run.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
"$HERE/01_hyper_baseline.sh"
"$HERE/02_cubecl_stages.sh"
"$HERE/03_jit_layout.sh"
echo
echo "M2 plan complete; results in $HERE/results/"
