#!/usr/bin/env bash
# Start the native terminal wall under makepad's Relax JIT (`relax run`).
#
# Usage:
#   tools/wall-relax.sh [--ns NAME] [relax options...] [-- wall args...]
#
# By default (no --ns), starts an isolated scratch g3d-termd under /tmp/g3d-font-$$
# in namespace `probe`, exports G3D_TERMD_DIR, prints startup time when the control
# socket comes up, and stops + removes the scratch daemon on exit.
#
# With --ns NAME (or a positional namespace name), targets that namespace without
# creating or destroying a scratch daemon (respecting any caller-set G3D_TERMD_DIR).
#
# Environment:
#   GLYPH_RELAX           Path to relax binary (default: /Users/lugo/localdev/makepad/target/release/relax, else `relax`)
#   GLYPH_WALL_SHADER_DIR Directory for disk-loaded WGSL shaders (default: <repo>/native/src/shaders)
#   G3D_BIN               Path to g3d CLI for scratch daemon teardown (default: ~/.local/bin/g3d, else `g3d`)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

if [[ -n "${GLYPH_RELAX:-}" ]]; then
    RELAX="$GLYPH_RELAX"
elif [[ -x "/Users/lugo/localdev/makepad/target/release/relax" ]]; then
    RELAX="/Users/lugo/localdev/makepad/target/release/relax"
else
    RELAX="relax"
fi

if [[ -n "${G3D_BIN:-}" ]]; then
    G3D="$G3D_BIN"
elif [[ -x "${HOME}/.local/bin/g3d" ]]; then
    G3D="${HOME}/.local/bin/g3d"
else
    G3D="g3d"
fi

NS=""
SCRATCH=1
RELAX_ARGS=()
WALL_ARGS=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h|--help)
            sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        --ns)
            [[ $# -ge 2 ]] || { echo "error: --ns requires a namespace name" >&2; exit 2; }
            NS="$2"
            SCRATCH=0
            shift 2
            ;;
        --ns=*)
            NS="${1#--ns=}"
            SCRATCH=0
            shift
            ;;
        --)
            shift
            WALL_ARGS+=("$@")
            break
            ;;
        --ahead=*|--verbose|--quiet|--no-live|--unattended|--check-first|--agent|--quit-after-present)
            RELAX_ARGS+=("$1")
            shift
            ;;
        -*)
            WALL_ARGS+=("$1")
            shift
            ;;
        *)
            if [[ -z "$NS" ]]; then
                NS="$1"
                SCRATCH=0
            else
                WALL_ARGS+=("$1")
            fi
            shift
            ;;
    esac
done

SCRATCH_DIR=""
if [[ "$SCRATCH" -eq 1 ]]; then
    NS="probe"
    SCRATCH_DIR="/tmp/g3d-font-$$"
    mkdir -m 700 "$SCRATCH_DIR"
    export G3D_TERMD_DIR="$SCRATCH_DIR"
fi

export GLYPH_WALL_SHADER_DIR="${GLYPH_WALL_SHADER_DIR:-$REPO_ROOT/native/src/shaders}"
# Ensure g3d-termd is on PATH for Termd::connect_or_start.
if [[ -d "${HOME}/.local/bin" && ":${PATH}:" != *":${HOME}/.local/bin:"* ]]; then
    export PATH="${HOME}/.local/bin:${PATH}"
fi

if [[ -n "${G3D_TERMD_DIR:-}" ]]; then
    SOCK_PATH="${G3D_TERMD_DIR}/wall-${NS}.sock"
else
    SOCK_PATH="/tmp/g3d-$(id -u)/wall-${NS}.sock"
fi

WATCHER_PID=""
cleanup() {
    if [[ -n "$WATCHER_PID" ]]; then
        kill "$WATCHER_PID" 2>/dev/null || true
    fi
    if [[ -n "$SCRATCH_DIR" ]]; then
        "$G3D" --ns "$NS" stop >/dev/null 2>&1 || true
        rm -rf "$SCRATCH_DIR"
    fi
}
trap cleanup EXIT INT TERM

now_ms() {
    perl -MTime::HiRes=time -e 'printf "%.0f\n", time * 1000'
}

CACHE_STATE="cold"
if [[ -d "$REPO_ROOT/target/relax" ]]; then
    CACHE_STATE="warm"
fi

T0_MS="$(now_ms)"
echo "[wall-relax] starting (${CACHE_STATE} target/relax cache): ns=${NS} dir=${G3D_TERMD_DIR:-<default>}"
echo "[wall-relax] relax=${RELAX} shaders=${GLYPH_WALL_SHADER_DIR}"
if [[ "$SCRATCH" -eq 1 ]]; then
    echo "[wall-relax] control: G3D_TERMD_DIR=${G3D_TERMD_DIR} ./target/release/wallctl --ns ${NS} <verb>"
fi

# Background readiness timer: reports elapsed time when the wall's control socket opens.
(
    while [[ ! -S "$SOCK_PATH" ]]; do
        sleep 0.05
    done
    T1_MS="$(now_ms)"
    ELAPSED_MS=$((T1_MS - T0_MS))
    perl -e "printf \"[wall-relax] ready (control socket up, ${CACHE_STATE}) in %.2f s (%d ms)\\n\", ${ELAPSED_MS}/1000.0, ${ELAPSED_MS}"
) &
WATCHER_PID=$!

cd "$REPO_ROOT"
STATUS=0
"$RELAX" run --allow-c ${RELAX_ARGS[@]+"${RELAX_ARGS[@]}"} glyph3d-native -- --wall --wall-ns "$NS" ${WALL_ARGS[@]+"${WALL_ARGS[@]}"} || STATUS=$?

T_END_MS="$(now_ms)"
TOTAL_MS=$((T_END_MS - T0_MS))
perl -e "printf \"[wall-relax] exited (status %d) after %.2f s\\n\", ${STATUS}, ${TOTAL_MS}/1000.0"
exit "$STATUS"
