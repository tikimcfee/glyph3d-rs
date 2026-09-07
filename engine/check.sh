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
# A template with X's: BSD mktemp accepts `-t name` bare, GNU mktemp refuses it
# ("too few X's") — found the first time this ran on Linux, 2026-09-07.
TMPBIN=$(mktemp -t glyph3d-bench-probe.XXXXXX)
# The shipped engine library: .dylib on macOS, .so on Linux (pixi.toml has a
# per-platform build-engine task; native/build.rs picks the same extension).
case "$(uname -s)" in Darwin) DYLIB=native/libglyph_engine.dylib ;; *) DYLIB=native/libglyph_engine.so ;; esac
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

# NATIVE-PORT: the default is ALL SIXTEEN. The five GPU suites were briefly
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
# THE INSTRUMENTS, run rather than merely present. fixture_census is a
# verification instrument and it HAD A FAULT OF ITS OWN — NaN poisoning its
# Range, so any field whose first value was NaN reported "uniform" and it
# invented a blind spot (2026-09-03). Nothing executed it, so nothing said. Same
# argument as compiling the benches: an instrument nothing runs is not a parked
# instrument, it is an absent one. fixture_manifest is gate 9's Mojo half.
[[ "${1:-all}" == "gpu" ]] || run fixture_census "${PIPE[@]}"
[[ "${1:-all}" == "gpu" ]] || run fixture_manifest "${PIPE[@]}"
# fold_profile is the THIRD instrument, added 2026-09-07. It prints where
# run_pipeline's time goes across five item shapes and compares the serial form
# against the scan form. It asserts nothing except that the two forms agree on
# the leader count, which is not a conformance claim (conformance_real owns
# that) but a guard that the two timed runs did the same work — without it a
# ratio could be comparing a full run against a broken one. ~0.2 s, so it runs
# with everything else rather than living in `bench` where nothing would.
[[ "${1:-all}" == "gpu" ]] || run fold_profile "$GAPS" \
    $(find native/src -name '*.rs' 2>/dev/null | sort | head -40)

# ffi_selftest is the ONE suite that does not `mojo run`: it asserts the C ABI,
# so it links the SHIPPED dylib and calls its exports through external_call —
# the boundary the product actually crosses. Importing ffi.mojo in-process
# instead was unsound under this pinned toolchain, not merely weak: executable
# codegen miscompiles offset-indexed loads through unsafe_bitcast'd pointers at
# some inlined call sites, per compilation unit (measured 2026-09-06: origin_x
# read back as a heap address; the same source built as the dylib is bit-exact
# on every single-item fixture). ffi_selftest.mojo's header has the full story.
# It covers the 14 single-item pipe fixtures and skips the 3 multi-item ones —
# the per-item entry takes one item per load by design; the batched entry's
# coverage lives in check-all's --repo-verify gate.
if [[ "${1:-all}" != "gpu" ]]; then
    # The artifact under test must exist and be current — same reason check-all
    # rebuilds the dylib as its step 0. One definition of the build command:
    # pixi.toml's build-engine task.
    printf '%-22s ' "$(basename "$DYLIB")"
    if out=$(pixi run build-engine 2>&1); then
        echo "built"
    else
        echo "FAILED TO BUILD"; echo "$out" | tail -4; exit 1
    fi
    printf '%-22s ' ffi_selftest
    if ! out=$("${MOJO[@]}" build $FP -I engine engine/ffi_selftest.mojo -o "$TMPBIN" \
        -Xlinker "$PWD/$DYLIB" \
        -Xlinker -rpath -Xlinker "$PWD/native" 2>&1); then
        echo "FAILED TO BUILD"; echo "$out" | tail -4; rm -f "$TMPBIN"; exit 1
    fi
    if out=$("$TMPBIN" "${PIPE[@]}" 2>&1); then
        echo "${out##*$'\n'}"
    else
        echo "FAILED"; echo "$out" | tail -5; rm -f "$TMPBIN"; exit 1
    fi
    rm -f "$TMPBIN"
fi
case "${1:-all}" in
    cpu) echo "all 11 CPU suites + ffi_selftest (dylib C ABI, 14 single-item fixtures) + 3 instruments green (fp contraction disabled); GPU suites NOT RUN" ;;
    bench) echo "all bench files compile (they are not RUN: bench.bin is untracked)" ;;
    gpu) echo "all 5 GPU suites green (fp contraction disabled)" ;;
    *)   echo "all 16 suites + ffi_selftest (dylib C ABI) green + 3 instruments + benches compile, CPU + GPU (fp contraction disabled)" ;;
esac
