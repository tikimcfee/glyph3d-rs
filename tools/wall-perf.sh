#!/usr/bin/env bash
# tools/wall-perf.sh — Baseline and regression harness for the native terminal wall.
#
# Measures:
#   1. Flood throughput & window-thread frame breakdown (`App::pump` vs `App::draw`)
#      for N in 1, 4, 14 live terminals under three workloads:
#        - `yes`          (2 B/line extreme scroll/newline flood)
#        - `seq`          (`seq 1 2000000` looped so all frames see live numeric output)
#        - `cat-src`      (`native/src/wall/renderer.rs` catted in a loop: dense code lines)
#      Each flood run asks for `--frames 120` with a safety wall-clock cap
#      (`GLYPH_WALL_PERF_MAX_MS`, default 4000 ms) so cases that starve or wedge
#      `Sessions::pump` terminate cleanly and report their exact achieved FPS,
#      pump stall time, draw time, process CPU, and upload stats.
#   2. Untouched idle behavior over 5 idle seconds (after a 1.2 s startup settle so the
#      opening `wall.fit` camera ease has snapped home), verifying 0 frames drawn and
#      minimal process CPU between input/output events.
#
# Safety & process-hygiene invariants:
#   - Never touches live namespaces `wall`, `agents`, or `default`.
#   - Always sets `G3D_TERMD_DIR` to a private 0700 directory under `/tmp/g3d-wp-<pid>/...`.
#   - Every spawned terminal runs under a hard wall-clock + `RLIMIT_CPU` supervisor
#     (`--ttl-secs`, default 12s for floods / 10s for idle) and `g3d-termd` starts with
#     `--idle-exit 5`, so floods and daemons self-terminate even if this script is `kill -9`'d.
#   - On EXIT/INT/TERM/HUP, stops each scratch daemon via `g3d`/`termd_bench_ctl stop` AND
#     kills recorded `holder`, `brain`, and `term` PIDs + child process groups from `pids.txt`.
#   - Never removes a scratch directory before confirming via `kill -0` that its daemon and
#     all recorded child PIDs are dead.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

NS="${GLYPH_WALL_PERF_NS:-wall-perf}"
case "$NS" in
  wall|agents|default)
    echo "wall-perf.sh: refusing live namespace '$NS'" >&2
    exit 1
    ;;
esac

FRAMES="${GLYPH_WALL_PERF_FRAMES:-120}"
MAX_WALL_MS="${GLYPH_WALL_PERF_MAX_MS:-4000}"
FLOOD_TTL_SECS="${GLYPH_WALL_PERF_FLOOD_TTL_SECS:-15}"
IDLE_TTL_SECS="${GLYPH_WALL_PERF_IDLE_TTL_SECS:-15}"
COLS="${GLYPH_WALL_PERF_COLS:-80}"
ROWS="${GLYPH_WALL_PERF_ROWS:-24}"
COUNTS="${GLYPH_WALL_PERF_COUNTS:-1 4 14}"
IDLE_COUNTS="${GLYPH_WALL_PERF_IDLE_COUNTS:-1 14}"
IDLE_MS="${GLYPH_WALL_PERF_IDLE_MS:-5000}"
SETTLE_MS="${GLYPH_WALL_PERF_SETTLE_MS:-1200}"
CAT_FILE="${GLYPH_WALL_PERF_CAT_FILE:-$REPO_ROOT/native/src/wall/renderer.rs}"
NO_BUILD="${GLYPH_WALL_PERF_NO_BUILD:-0}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-build) NO_BUILD=1; shift ;;
    --frames)   FRAMES="$2"; shift 2 ;;
    --max-ms)   MAX_WALL_MS="$2"; shift 2 ;;
    *)          echo "wall-perf.sh: unknown argument '$1'" >&2; exit 2 ;;
  esac
done

if [[ "$NO_BUILD" != "1" ]]; then
  echo "Building release glyph3d-native and termd_bench_ctl..." >&2
  cargo build --release -p glyph3d-native --bin glyph3d-native >&2
  cargo build --release -p termd-client --example termd_bench_ctl >&2
fi

RENDERER="${GLYPH_RENDERER:-$REPO_ROOT/target/release/glyph3d-native}"
CTL_BIN="$REPO_ROOT/target/release/examples/termd_bench_ctl"
G3D_BIN=""
if [[ -x /Users/lugo/.local/bin/g3d ]]; then
  G3D_BIN="/Users/lugo/.local/bin/g3d"
elif command -v g3d >/dev/null 2>&1; then
  G3D_BIN="$(command -v g3d)"
fi

SCRATCH_ROOT="/tmp/g3d-wp-$$"
mkdir -m 0700 -p "$SCRATCH_ROOT"
ACTIVE_DIR=""
CAFFEINATE_PID=""

if [[ -x /usr/bin/caffeinate ]]; then
  /usr/bin/caffeinate -disu -w "$$" >/dev/null 2>&1 &
  CAFFEINATE_PID="$!"
fi

kill_recorded_pids() {
  local pids_file="$1"
  local sig="$2"
  [[ -f "$pids_file" ]] || return 0
  while read -r role pid _rest; do
    [[ "$pid" =~ ^[0-9]+$ ]] && (( pid > 1 )) || continue
    if kill -0 "$pid" 2>/dev/null; then
      pkill "-$sig" -P "$pid" >/dev/null 2>&1 || true
      if [[ "$role" == "term" ]]; then
        kill "-$sig" -- "-$pid" >/dev/null 2>&1 || true
      fi
      kill "-$sig" "$pid" >/dev/null 2>&1 || true
    fi
  done < "$pids_file"
}

any_recorded_pid_alive() {
  local pids_file="$1"
  [[ -f "$pids_file" ]] || return 1
  while read -r _role pid _rest; do
    [[ "$pid" =~ ^[0-9]+$ ]] && (( pid > 1 )) || continue
    if kill -0 "$pid" 2>/dev/null; then
      return 0
    fi
  done < "$pids_file"
  return 1
}

stop_scratch_dir() {
  local d="$1"
  [[ -n "$d" && -d "$d" ]] || return 0
  local pids_file="$d/pids.txt"
  local sock="$d/termd-$NS.sock"

  # 1. Request clean shutdown via g3d stop (if available) and termd_bench_ctl stop.
  if [[ -n "$G3D_BIN" && -S "$sock" ]]; then
    G3D_TERMD_DIR="$d" "$G3D_BIN" --ns "$NS" stop >/dev/null 2>&1 || true
  fi
  if [[ -x "$CTL_BIN" ]]; then
    "$CTL_BIN" stop --dir "$d" --ns "$NS" >/dev/null 2>&1 || true
  fi
  [[ -d "$d" ]] || return 0

  # 2. Belt-and-suspenders: signal any surviving recorded holder/brain/term PIDs + children.
  if any_recorded_pid_alive "$pids_file"; then
    kill_recorded_pids "$pids_file" TERM
    local i
    for (( i = 0; i < 10; i++ )); do
      any_recorded_pid_alive "$pids_file" || break
      sleep 0.05
    done
  fi
  if any_recorded_pid_alive "$pids_file"; then
    kill_recorded_pids "$pids_file" KILL
    local i
    for (( i = 0; i < 40; i++ )); do
      any_recorded_pid_alive "$pids_file" || break
      sleep 0.05
    done
  fi

  # 3. Never remove the scratch directory if any recorded daemon/child is still alive.
  if any_recorded_pid_alive "$pids_file"; then
    echo "wall-perf.sh: ERROR: daemon/children still alive in $d; keeping directory for inspection:" >&2
    cat "$pids_file" >&2 || true
    return 1
  fi

  rm -rf "$d"
}

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  if [[ -n "$CAFFEINATE_PID" ]]; then
    kill "$CAFFEINATE_PID" >/dev/null 2>&1 || true
  fi
  local clean_ok=1
  if [[ -n "$ACTIVE_DIR" && -d "$ACTIVE_DIR" ]]; then
    stop_scratch_dir "$ACTIVE_DIR" || clean_ok=0
    ACTIVE_DIR=""
  fi
  if [[ -d "$SCRATCH_ROOT" ]]; then
    for sub in "$SCRATCH_ROOT"/*; do
      if [[ -d "$sub" ]]; then
        stop_scratch_dir "$sub" || clean_ok=0
      fi
    done
    if (( clean_ok == 1 )); then
      rm -rf "$SCRATCH_ROOT"
    fi
  fi
  exit "$status"
}
trap cleanup EXIT INT TERM HUP

RESULTS_TSV="$SCRATCH_ROOT/results.tsv"
IDLE_TSV="$SCRATCH_ROOT/idle.tsv"
: > "$RESULTS_TSV"
: > "$IDLE_TSV"

run_flood_case() {
  local label="$1"
  local n="$2"
  local shell_cmd="$3"

  ACTIVE_DIR="$SCRATCH_ROOT/${label}-n${n}"
  mkdir -m 0700 -p "$ACTIVE_DIR"

  echo "Running flood: workload=$label terminals=$n (${COLS}x${ROWS}) target_frames=$FRAMES max_ms=$MAX_WALL_MS ttl=${FLOOD_TTL_SECS}s ..." >&2
  "$CTL_BIN" start \
    --dir "$ACTIVE_DIR" \
    --ns "$NS" \
    --count "$n" \
    --cols "$COLS" \
    --rows "$ROWS" \
    --warmup-ms 150 \
    --ttl-secs "$FLOOD_TTL_SECS" \
    -- /bin/sh -c "$shell_cmd"

  local out_file="$SCRATCH_ROOT/out-${label}-n${n}.log"
  GLYPH_WALL_PERF=1 \
  GLYPH_WALL_PERF_MAX_MS="$MAX_WALL_MS" \
  G3D_TERMD_DIR="$ACTIVE_DIR" \
  "$RENDERER" --wall --wall-ns "$NS" --frames "$FRAMES" > "$out_file"

  local perf_line
  perf_line="$(grep '^wall-perf: ' "$out_file" | tail -n 1)"
  if [[ -z "$perf_line" ]]; then
    echo "wall-perf.sh: missing wall-perf summary line for $label N=$n" >&2
    cat "$out_file" >&2
    exit 1
  fi
  printf '%s\t%s\t%s\n' "$label" "$n" "$perf_line" >> "$RESULTS_TSV"

  stop_scratch_dir "$ACTIVE_DIR"
  ACTIVE_DIR=""
}

run_idle_case() {
  local n="$1"

  ACTIVE_DIR="$SCRATCH_ROOT/idle-n${n}"
  mkdir -m 0700 -p "$ACTIVE_DIR"

  echo "Running idle: terminals=$n (${COLS}x${ROWS}) settle=${SETTLE_MS}ms idle=${IDLE_MS}ms ttl=${IDLE_TTL_SECS}s ..." >&2
  "$CTL_BIN" start \
    --dir "$ACTIVE_DIR" \
    --ns "$NS" \
    --count "$n" \
    --cols "$COLS" \
    --rows "$ROWS" \
    --warmup-ms 200 \
    --ttl-secs "$IDLE_TTL_SECS" \
    -- /bin/sh -c 'printf "idle-ready\n"; exec sleep 30'

  local out_file="$SCRATCH_ROOT/out-idle-n${n}.log"
  GLYPH_WALL_PERF=1 \
  GLYPH_WALL_PERF_SETTLE_MS="$SETTLE_MS" \
  GLYPH_WALL_PERF_IDLE_MS="$IDLE_MS" \
  G3D_TERMD_DIR="$ACTIVE_DIR" \
  "$RENDERER" --wall --wall-ns "$NS" > "$out_file"

  local perf_line
  perf_line="$(grep '^wall-perf: ' "$out_file" | tail -n 1)"
  if [[ -z "$perf_line" ]]; then
    echo "wall-perf.sh: missing wall-perf summary line for idle N=$n" >&2
    cat "$out_file" >&2
    exit 1
  fi
  printf 'idle\t%s\t%s\n' "$n" "$perf_line" >> "$IDLE_TSV"

  stop_scratch_dir "$ACTIVE_DIR"
  ACTIVE_DIR=""
}

for workload in yes seq cat-src; do
  case "$workload" in
    yes)     cmd="exec yes" ;;
    seq)     cmd="while :; do /usr/bin/seq 1 2000000; done" ;;
    cat-src) cmd="while :; do /bin/cat '$CAT_FILE'; done" ;;
  esac
  for n in $COUNTS; do
    run_flood_case "$workload" "$n" "$cmd"
  done
done

for n in $IDLE_COUNTS; do
  run_idle_case "$n"
done

python3 - "$RESULTS_TSV" "$IDLE_TSV" <<'PY'
import sys

results_path, idle_path = sys.argv[1], sys.argv[2]

def parse_kv_line(line: str) -> dict:
    body = line.removeprefix("wall-perf:").strip()
    out = {}
    for tok in body.split():
        if "=" in tok:
            k, v = tok.split("=", 1)
            out[k] = v
    return out

def parse_sub(val: str) -> dict:
    out = {}
    for item in val.split(","):
        if ":" in item:
            k, v = item.split(":", 1)
            out[k] = v
    return out

print()
print("### Wall Flood Performance")
print()
headers = [
    "Workload", "N", "Frames", "Wall (s)", "FPS",
    "Pump p50/p95/max (ms)", "Pump sum (ms) / Wakes",
    "Draw p50/p95/max (ms)", "Wait p50 (ms)",
    "Proc CPU (ms)", "Uploads (drawn/up/rows/writes)", "Status"
]
print("| " + " | ".join(headers) + " |")
print("| " + " | ".join(["---"] * len(headers)) + " |")

with open(results_path, "r", encoding="utf-8") as f:
    for raw in f:
        raw = raw.strip()
        if not raw:
            continue
        workload, n, line = raw.split("\t", 2)
        kv = parse_kv_line(line)
        pump = parse_sub(kv["pump_ms"])
        draw = parse_sub(kv["draw_ms"])
        wait = parse_sub(kv["wait_ms"])
        up = parse_sub(kv["uploads"])
        wall_s = float(kv["wall_ms"]) / 1000.0
        status = "stalled in pump" if kv.get("stalled") == "1" else "ok"
        if kv.get("occluded", "0") != "0":
            status += f" (occl:{kv['occluded']})"
        row = [
            workload,
            n,
            kv["frames"],
            f"{wall_s:.2f}",
            kv["fps"],
            f"{pump['p50']} / {pump['p95']} / {pump['max']}",
            f"{pump['sum']} ({pump['wakes']}w)",
            f"{draw['p50']} / {draw['p95']} / {draw['max']}",
            wait["p50"],
            kv["proc_cpu_ms"],
            f"{up['drawn']}/{up['uploaded']}/{up['rows']}/{up['writes']}",
            status,
        ]
        print("| " + " | ".join(row) + " |")

print()
print("### Wall Idle Check")
print()
idle_headers = [
    "Case", "N", "Startup Frames", "Settle+Idle Wall (s)",
    "Idle Window (s)", "Idle Frames", "Idle Proc CPU (ms)", "Total Proc CPU (ms)"
]
print("| " + " | ".join(idle_headers) + " |")
print("| " + " | ".join(["---"] * len(idle_headers)) + " |")

with open(idle_path, "r", encoding="utf-8") as f:
    for raw in f:
        raw = raw.strip()
        if not raw:
            continue
        case, n, line = raw.split("\t", 2)
        kv = parse_kv_line(line)
        wall_s = float(kv["wall_ms"]) / 1000.0
        idle_s = float(kv.get("idle_ms", "0")) / 1000.0
        row = [
            case,
            n,
            kv["frames"],
            f"{wall_s:.2f}",
            f"{idle_s:.2f}",
            kv.get("idle_frames", "na"),
            kv.get("idle_cpu_ms", "na"),
            kv["proc_cpu_ms"],
        ]
        print("| " + " | ".join(row) + " |")
PY
