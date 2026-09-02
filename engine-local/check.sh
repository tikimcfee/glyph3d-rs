#!/usr/bin/env bash
# check.sh — run every engine suite with the flags the contract requires.
#
# WHY THIS SCRIPT EXISTS: --fp-mode contract=off is not optional.
#
# Mojo 1.0 defaults to `contract=fast`, which is Clang's -ffp-contract=fast: it
# fuses `a + b*c` into an FMA ACROSS STATEMENTS. Splitting the expression into
# `var t = a*b` and then `t + c` still contracts. Verified in emitted assembly —
# a kernel doing o[i] = a[i]*b[i] + c[i] emits 2 fmla under the default and 2 fmul
# under contract=off.
#
# This pipeline is bit-exact against a JS oracle, and JS rounds at every step while
# an FMA rounds once. The port is full of `a*b + c` shapes:
#     Float32(-Float64(row) * lh + oy)
#     Float32(-Float64(wrap_row) * z_step + oz)
#     paginate's four-term Z chain
#
# Today the f64 intermediates truncate to f32 and the difference vanishes, so the
# suites pass either way. That is LUCK, not a guarantee — a future f64 lane, a
# reassociated expression, or a compiler version bump could turn it into silent
# oracle divergence. Pinning the flag costs nothing measurable (bench checksums and
# throughput identical) and converts luck into a property.
#
# Usage:  engine-local/check.sh          run every suite
#         engine-local/check.sh gpu      GPU suites only
set -euo pipefail
cd "$(dirname "$0")/.."
# Mojo comes from the pixi env (pixi.toml pins it); no .venv-mojo in this tree.
MOJO=(pixi run mojo)

FP="--fp-mode contract=off"
PIPE=(engine-local/fixtures/*.pipe.bin)
BAKE=(engine-local/fixtures/*.bake.bin)

CPU=(conformance conformance_scan ordinal_invariant conformance_record conformance_resume conformance_elide)
# gaps and matrix take ONE fixture (for its trie) and build their own topologies
GAPS=engine-local/fixtures/repo-file.pipe.bin
GPU=(gpu_decode gpu_scan gpu_paginate gpu_bounds gpu_pipeline)

run() { # name, fixtures...
    local name=$1; shift
    printf '%-22s ' "$name"
    if out=$("${MOJO[@]}" run $FP -I engine-local "engine-local/$name.mojo" "$@" 2>&1); then
        echo "${out##*$'\n'}"
    else
        echo "FAILED"; echo "$out" | tail -5; exit 1
    fi
}

# NATIVE-PORT: the default is CPU-ONLY here, and says so at the end rather than
# printing a blanket green. The five GPU suites import `max.gpu.host`, which this
# tree's pixi env does not provide (pixi.toml pins `mojo`, not `max`) — verified
# 2026-09-02: gpu_decode fails at parse with "'gpu' does not refer to a nested
# package". They are PARKED, not passing, and a runner that quietly omitted them
# under the word "all" would be exactly the kind of green this file exists to
# prevent. `check.sh gpu` still tries them, so the day `max` lands the gate is one
# word away.
case "${1:-cpu}" in
    gpu) list=("${GPU[@]}") ;;
    cpu) list=("${CPU[@]}") ;;
    all) list=("${CPU[@]}" "${GPU[@]}") ;;
    *)   echo "usage: engine-local/check.sh [cpu|gpu|all]" >&2; exit 2 ;;
esac

for s in "${list[@]}"; do run "$s" "${PIPE[@]}"; done
[[ "${1:-cpu}" == "gpu" ]] || run conformance_gaps "$GAPS"
[[ "${1:-cpu}" == "gpu" ]] || run conformance_matrix "$GAPS"
# The cross-form runner: OUR OWN SOURCE TREE as a live corpus — no fixtures, the
# serial and scan forms adjudicate each other. Files span the size-mod classes so
# the derived wrap/page/scroll params all occur; a directory outside the repo
# (the linux clone, say) is the same command with a different find.
[[ "${1:-cpu}" == "gpu" ]] || run conformance_real "$GAPS" \
    $(find native/src tools -name '*.rs' -o -name '*.py' 2>/dev/null | sort | head -60)
[[ "${1:-cpu}" == "gpu" ]] || run conformance_bake "${BAKE[@]}"
if [[ "${1:-cpu}" == "cpu" ]]; then
    echo "all CPU suites green (fp contraction disabled)"
    echo "GPU suites NOT RUN: max.gpu absent from this pixi env — parked, not passing."
else
    echo "all suites green (fp contraction disabled)"
fi
