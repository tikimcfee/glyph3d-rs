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
# Usage:  engine/check.sh          run every suite
#         engine/check.sh gpu      GPU suites only
set -euo pipefail
cd "$(dirname "$0")/.."
# Mojo comes from the pixi env (pixi.toml pins it); no .venv-mojo in this tree.
MOJO=(pixi run mojo)

FP="--fp-mode contract=off"
TMPBIN=$(mktemp -t glyph3d-bench-probe)
PIPE=(engine/fixtures/*.pipe.bin)
BAKE=(engine/fixtures/*.bake.bin)

CPU=(conformance conformance_scan ordinal_invariant conformance_record conformance_resume conformance_elide conformance_invariants)
# gaps and matrix take ONE fixture (for its trie) and build their own topologies
GAPS=engine/fixtures/repo-file.pipe.bin
GPU=(gpu_decode gpu_scan gpu_paginate gpu_bounds gpu_pipeline)

# NATIVE-PORT 2026-09-02: the benches are COMPILED here, not run. They cannot run
# in a fresh tree (engine/bench/bench.bin is untracked and its generator needs the
# JS reference pipeline), but nothing compiled them either — so toolchain drift in
# a bench file was invisible. It had already happened: blob_bench.mojo still used
# `memcpy`, removed from std.memory in this nightly, and had not built for some
# time. A build is cheap and catches exactly that class.
compile_only() { # name, path
    printf '%-22s ' "$1"
    if out=$("${MOJO[@]}" build $FP -I engine "$2" -o "$TMPBIN" 2>&1); then
        rm -f "$TMPBIN"; echo "compiles"
    else
        echo "FAILED TO BUILD"; echo "$out" | tail -4; exit 1
    fi
}

run() { # name, fixtures...
    local name=$1; shift
    printf '%-22s ' "$name"
    if out=$("${MOJO[@]}" run $FP -I engine "engine/$name.mojo" "$@" 2>&1); then
        echo "${out##*$'\n'}"
    else
        echo "FAILED"; echo "$out" | tail -5; exit 1
    fi
}

# NATIVE-PORT: the default is ALL FIFTEEN. The five GPU suites were briefly
# unbuildable here — gpu_decode failed at parse with "'gpu' does not refer to a
# nested package" — for the sole reason that pixi.toml pinned `mojo` and not
# `max`. Adding the dependency was the whole fix: all five then passed on Apple
# silicon (Metal) first try, 2026-09-02. A suite you cannot build is not a parked
# suite, it is an absent one, and the honest move was to install the dep rather
# than teach the runner to skip gracefully.
case "${1:-all}" in
    gpu)   list=("${GPU[@]}") ;;
    cpu)   list=("${CPU[@]}") ;;
    bench) list=() ;;
    all)   list=("${CPU[@]}" "${GPU[@]}") ;;
    *)     echo "usage: engine/check.sh [cpu|gpu|bench|all]" >&2; exit 2 ;;
esac

if [[ "${1:-all}" == "bench" || "${1:-all}" == "all" ]]; then
    for b in engine/bench/*.mojo; do compile_only "$(basename "$b" .mojo)" "$b"; done
fi

# `set -u` + an empty array (bench mode) needs the guard.
for s in ${list[@]+"${list[@]}"}; do run "$s" "${PIPE[@]}"; done
[[ "${1:-all}" == "gpu" ]] || run conformance_gaps "$GAPS"
[[ "${1:-all}" == "gpu" ]] || run conformance_matrix "$GAPS"
# The cross-form runner: OUR OWN SOURCE TREE as a live corpus — no fixtures, the
# serial and scan forms adjudicate each other. Files span the size-mod classes so
# the derived wrap/page/scroll params all occur; a directory outside the repo
# (the linux clone, say) is the same command with a different find.
[[ "${1:-all}" == "gpu" ]] || run conformance_real "$GAPS" \
    $(find native/src tools -name '*.rs' -o -name '*.py' 2>/dev/null | sort | head -60)
[[ "${1:-all}" == "gpu" ]] || run conformance_bake "${BAKE[@]}"
case "${1:-all}" in
    cpu) echo "all 11 CPU suites green (fp contraction disabled); GPU suites NOT RUN" ;;
    bench) echo "all bench files compile (they are not RUN: bench.bin is untracked)" ;;
    gpu) echo "all 5 GPU suites green (fp contraction disabled)" ;;
    *)   echo "all 16 suites green + benches compile, CPU + GPU (fp contraction disabled)" ;;
esac
