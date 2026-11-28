#!/usr/bin/env bash
# tools/wall-perf.sh — Baseline, scale, and regression harness for the native terminal wall.
#
# Portable across macOS (Metal) and Linux (Wayland / X11 Vulkan).
#
# Modes (`--modes "flood extra idle"` by default; `--modes scale` for N=50, 100, 200):
#   1. `flood` + `extra`: Flood throughput & window-thread frame breakdown (`App::pump` vs
#      `App::draw`) for N in 1, 4, 14 live terminals (`yes`, `seq`, `cat-src`), plus
#      `fullscreen-tui`, `wheel-pan-zoom` (N=14), and `mixed-8yes-1cat-1quiet`.
#   2. `idle`: Untouched idle behavior over 5 idle seconds (after camera settle).
#   3. `scale`: Spawns N in 50, 100, 200 idle 80x24 terminals (checking daemon/OS limits and
#      stopping cleanly where refused), fits all, and measures open time, adopt time, window +
#      daemon (`holder` + `brain`) RSS, 5 s idle CPU, pan/zoom frame time with all N in view,
#      pan/zoom with most off-screen (1 framed), and far-zoom when every terminal is a few
#      pixels tall on screen.
#
# Safety & process-hygiene invariants:
#   - Never touches live/shared namespaces `wall`, `agents`, `default`, `rs-wall`, or `probe`.
#   - Always sets `G3D_TERMD_DIR` to a private 0700 directory under `/tmp/g3d-wp-<pid>/...`.
#   - Every spawned terminal runs under a hard wall-clock + `RLIMIT_CPU` supervisor
#     (`--ttl-secs`) and `g3d-termd` starts with `--idle-exit 5`, so floods and daemons
#     self-terminate even if this script is `kill -9`'d.
#   - Never uses `pkill`; kills only explicit PIDs started by this run (`ACTIVE_WIN_PID`,
#     `ACTIVE_DRIVER_PID`, and `pids.txt` `holder`/`brain`/`term` PIDs).
#   - Never removes a scratch directory before confirming via `kill -0` that its daemon and
#     all recorded child PIDs are dead.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

NS="${GLYPH_WALL_PERF_NS:-wp-$$}"
case "$NS" in
  wall|agents|default|rs-wall|probe)
    echo "wall-perf.sh: refusing shared/live namespace '$NS'" >&2
    exit 1
    ;;
esac

FRAMES="${GLYPH_WALL_PERF_FRAMES:-120}"
MAX_WALL_MS="${GLYPH_WALL_PERF_MAX_MS:-4000}"
FLOOD_TTL_SECS="${GLYPH_WALL_PERF_FLOOD_TTL_SECS:-15}"
IDLE_TTL_SECS="${GLYPH_WALL_PERF_IDLE_TTL_SECS:-15}"
SCALE_TTL_SECS="${GLYPH_WALL_PERF_SCALE_TTL_SECS:-60}"
COLS="${GLYPH_WALL_PERF_COLS:-80}"
ROWS="${GLYPH_WALL_PERF_ROWS:-24}"
COUNTS="${GLYPH_WALL_PERF_COUNTS:-1 4 14}"
IDLE_COUNTS="${GLYPH_WALL_PERF_IDLE_COUNTS:-1 14}"
SCALE_COUNTS="${GLYPH_WALL_PERF_SCALE_COUNTS:-50 100 200}"
SCALE_NOFILE="${GLYPH_WALL_PERF_SCALE_NOFILE:-}"
IDLE_MS="${GLYPH_WALL_PERF_IDLE_MS:-5000}"
SETTLE_MS="${GLYPH_WALL_PERF_SETTLE_MS:-1200}"
CAT_FILE="${GLYPH_WALL_PERF_CAT_FILE:-$REPO_ROOT/native/src/wall/renderer.rs}"
CARGO_BUILD_EXTRA="-p wall-control"
MODES="${GLYPH_WALL_PERF_MODES:-flood extra idle}"
NO_BUILD="${GLYPH_WALL_PERF_NO_BUILD:-0}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-build) NO_BUILD=1; shift ;;
    --frames)   FRAMES="$2"; shift 2 ;;
    --max-ms)   MAX_WALL_MS="$2"; shift 2 ;;
    --modes)    MODES="$2"; shift 2 ;;
    --scale-counts) SCALE_COUNTS="$2"; shift 2 ;;
    --scale-nofile) SCALE_NOFILE="$2"; shift 2 ;;
    *)          echo "wall-perf.sh: unknown argument '$1'" >&2; exit 2 ;;
  esac
done

if [[ "$NO_BUILD" != "1" ]]; then
  echo "Building release glyph3d-native, wallctl, and termd_bench_ctl..." >&2
  cargo build --release -p glyph3d-native $CARGO_BUILD_EXTRA >&2
  cargo build --release -p termd-client --example termd_bench_ctl >&2
fi

host_loadavg() {
  if command -v sysctl >/dev/null 2>&1 && sysctl -n vm.loadavg >/dev/null 2>&1; then
    sysctl -n vm.loadavg | tr -d '{}' | awk '{printf "%s/%s/%s", $1, $2, $3}'
  elif [[ -r /proc/loadavg ]]; then
    awk '{printf "%s/%s/%s", $1, $2, $3}' /proc/loadavg
  else
    printf "na"
  fi
}

pid_rss_kb() {
  local pid="$1"
  [[ "$pid" =~ ^[0-9]+$ ]] && (( pid > 1 )) || { printf "0"; return 0; }
  if [[ -r "/proc/$pid/status" ]]; then
    awk '/^VmRSS:/ {print $2; found=1; exit} END {if (!found) print 0}' "/proc/$pid/status" 2>/dev/null || printf "0"
  else
    ps -o rss= -p "$pid" 2>/dev/null | awk '{print ($1 + 0)}' || printf "0"
  fi
}

RENDERER="${GLYPH_RENDERER:-$REPO_ROOT/target/release/glyph3d-native}"
CTL_BIN="$REPO_ROOT/target/release/examples/termd_bench_ctl"
G3D_BIN=""
if [[ -n "${HOME:-}" && -x "$HOME/.local/bin/g3d" ]]; then
  G3D_BIN="$HOME/.local/bin/g3d"
elif command -v g3d >/dev/null 2>&1; then
  G3D_BIN="$(command -v g3d)"
fi

SCRATCH_ROOT="/tmp/g3d-wp-$$"
mkdir -m 0700 -p "$SCRATCH_ROOT"
ACTIVE_DIR=""
ACTIVE_WIN_PID=""
ACTIVE_DRIVER_PID=""
CAFFEINATE_PID=""

if [[ -x /usr/bin/caffeinate ]]; then
  /usr/bin/caffeinate -disu -w "$$" >/dev/null 2>&1 &
  CAFFEINATE_PID="$!"
fi

kill_explicit_pid() {
  local pid="$1"
  [[ "$pid" =~ ^[0-9]+$ ]] && (( pid > 1 )) || return 0
  if kill -0 "$pid" 2>/dev/null; then
    kill -TERM "$pid" >/dev/null 2>&1 || true
    local i
    for (( i = 0; i < 15; i++ )); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.05
    done
    if kill -0 "$pid" 2>/dev/null; then
      kill -KILL "$pid" >/dev/null 2>&1 || true
    fi
    wait "$pid" >/dev/null 2>&1 || true
  fi
}

kill_recorded_pids() {
  local pids_file="$1"
  local sig="$2"
  [[ -f "$pids_file" ]] || return 0
  while read -r role pid _rest; do
    [[ "$pid" =~ ^[0-9]+$ ]] && (( pid > 1 )) || continue
    if kill -0 "$pid" 2>/dev/null; then
      if command -v pgrep >/dev/null 2>&1; then
        local cpid
        for cpid in $(pgrep -P "$pid" 2>/dev/null || true); do
          [[ "$cpid" =~ ^[0-9]+$ ]] && (( cpid > 1 )) && kill "-$sig" "$cpid" >/dev/null 2>&1 || true
        done
      fi
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
  if [[ -n "$ACTIVE_DRIVER_PID" ]]; then
    kill_explicit_pid "$ACTIVE_DRIVER_PID"
    ACTIVE_DRIVER_PID=""
  fi
  if [[ -n "$ACTIVE_WIN_PID" ]]; then
    kill_explicit_pid "$ACTIVE_WIN_PID"
    ACTIVE_WIN_PID=""
  fi
  if [[ -n "$CAFFEINATE_PID" ]]; then
    kill "$CAFFEINATE_PID" >/dev/null 2>&1 || true
    wait "$CAFFEINATE_PID" >/dev/null 2>&1 || true
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
SCALE_TSV="$SCRATCH_ROOT/scale.tsv"
: > "$RESULTS_TSV"
: > "$IDLE_TSV"
: > "$SCALE_TSV"

run_flood_case() {
  local label="$1"
  local n="$2"
  local shell_cmd="$3"

  ACTIVE_DIR="$SCRATCH_ROOT/${label}-n${n}"
  mkdir -m 0700 -p "$ACTIVE_DIR"

  local load_now
  load_now="$(host_loadavg)"
  echo "Running flood: workload=$label terminals=$n (${COLS}x${ROWS}) target_frames=$FRAMES max_ms=$MAX_WALL_MS ttl=${FLOOD_TTL_SECS}s load=$load_now ..." >&2
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
  GLYPH_WALL_CLIPBOARD=memory \
  G3D_TERMD_DIR="$ACTIVE_DIR" \
  "$RENDERER" --wall --wall-ns "$NS" --frames "$FRAMES" > "$out_file"

  local perf_line
  perf_line="$(grep '^wall-perf: ' "$out_file" | tail -n 1)"
  if [[ -z "$perf_line" ]]; then
    echo "wall-perf.sh: missing wall-perf summary line for $label N=$n" >&2
    cat "$out_file" >&2
    exit 1
  fi
  printf '%s\t%s\t%s\t%s\n' "$label" "$n" "$load_now" "$perf_line" >> "$RESULTS_TSV"

  stop_scratch_dir "$ACTIVE_DIR"
  ACTIVE_DIR=""
}

run_wheel_case() {
  local n="$1"

  ACTIVE_DIR="$SCRATCH_ROOT/wheel-n${n}"
  mkdir -m 0700 -p "$ACTIVE_DIR"

  local load_now
  load_now="$(host_loadavg)"
  echo "Running camera wheel pan/zoom: terminals=$n (${COLS}x${ROWS}) target_frames=$FRAMES max_ms=$MAX_WALL_MS ttl=${IDLE_TTL_SECS}s load=$load_now ..." >&2
  "$CTL_BIN" start \
    --dir "$ACTIVE_DIR" \
    --ns "$NS" \
    --count "$n" \
    --cols "$COLS" \
    --rows "$ROWS" \
    --warmup-ms 200 \
    --ttl-secs "$IDLE_TTL_SECS" \
    -- /bin/sh -c "head -n $ROWS '$CAT_FILE'; exec sleep 30"

  local out_file="$SCRATCH_ROOT/out-wheel-n${n}.log"
  local driver_log="$SCRATCH_ROOT/wheel-driver-n${n}.log"
  python3 - "$ACTIVE_DIR/wall-$NS.sock" "$FRAMES" > "$driver_log" 2>&1 <<'PY' &
import math, os, socket, sys, time

sock_path = sys.argv[1]
target_frames = int(sys.argv[2])
deadline = time.monotonic() + 8.0

while time.monotonic() < deadline and not os.path.exists(sock_path):
    time.sleep(0.01)

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(2.0)
s.connect(sock_path)
buf = b""

def rpc(cmd: str) -> str:
    global buf
    s.sendall((cmd + "\n").encode("utf-8"))
    while b"\n" not in buf:
        chunk = s.recv(4096)
        if not chunk:
            return ""
        buf += chunk
    hdr, buf = buf.split(b"\n", 1)
    need = int(hdr.decode("ascii")) + 1
    while len(buf) < need:
        chunk = s.recv(4096)
        if not chunk:
            break
        buf += chunk
    body = buf[: need - 1].decode("utf-8", errors="replace")
    buf = buf[need:]
    return body

try:
    w0 = rpc("app.where")
    step = 0
    while step < target_frames * 2 and time.monotonic() < deadline:
        angle = step * 0.22
        dx = round(95.0 * math.cos(angle), 1)
        dy = round(70.0 * math.sin(angle), 1)
        rpc(f"app.wheel 16 16 {dx} {dy}")
        if step % 3 == 0:
            z_dy = 28.0 if (step // 15) % 2 == 0 else -28.0
            rpc(f"app.wheel 800 500 0 {z_dy} ctrl")
        step += 1
        time.sleep(0.006)
except (BrokenPipeError, ConnectionResetError, OSError):
    pass
finally:
    try:
        s.close()
    except OSError:
        pass
PY
  ACTIVE_DRIVER_PID="$!"

  GLYPH_WALL_PERF=1 \
  GLYPH_WALL_PERF_MAX_MS="$MAX_WALL_MS" \
  GLYPH_WALL_CLIPBOARD=memory \
  G3D_TERMD_DIR="$ACTIVE_DIR" \
  "$RENDERER" --wall --wall-ns "$NS" --frames "$FRAMES" > "$out_file"

  kill_explicit_pid "$ACTIVE_DRIVER_PID"
  ACTIVE_DRIVER_PID=""

  local perf_line
  perf_line="$(grep '^wall-perf: ' "$out_file" | tail -n 1)"
  if [[ -z "$perf_line" ]]; then
    echo "wall-perf.sh: missing wall-perf summary line for wheel-pan-zoom N=$n" >&2
    cat "$out_file" >&2
    exit 1
  fi
  printf '%s\t%s\t%s\t%s\n' "wheel-pan-zoom" "$n" "$load_now" "$perf_line" >> "$RESULTS_TSV"

  stop_scratch_dir "$ACTIVE_DIR"
  ACTIVE_DIR=""
}

run_idle_case() {
  local n="$1"

  ACTIVE_DIR="$SCRATCH_ROOT/idle-n${n}"
  mkdir -m 0700 -p "$ACTIVE_DIR"

  local load_now
  load_now="$(host_loadavg)"
  echo "Running idle: terminals=$n (${COLS}x${ROWS}) settle=${SETTLE_MS}ms idle=${IDLE_MS}ms ttl=${IDLE_TTL_SECS}s load=$load_now ..." >&2
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
  GLYPH_WALL_CLIPBOARD=memory \
  G3D_TERMD_DIR="$ACTIVE_DIR" \
  "$RENDERER" --wall --wall-ns "$NS" > "$out_file"

  local perf_line
  perf_line="$(grep '^wall-perf: ' "$out_file" | tail -n 1)"
  if [[ -z "$perf_line" ]]; then
    echo "wall-perf.sh: missing wall-perf summary line for idle N=$n" >&2
    cat "$out_file" >&2
    exit 1
  fi
  printf 'idle\t%s\t%s\t%s\n' "$n" "$load_now" "$perf_line" >> "$IDLE_TSV"

  stop_scratch_dir "$ACTIVE_DIR"
  ACTIVE_DIR=""
}

FULLSCREEN_TUI_CMD="python3 -c '
import sys
cols, rows = $COLS, $ROWS
frames = []
for f in range(16):
    out = [\"\\x1b[H\"]
    hdr = f\" top - tick {f:04d} up 4d, 14 tasks, cpu {(37 + f * 3) % 100:2d}% mem 68% \"
    out.append(f\"\\x1b[1;37;44m{hdr:<{cols}.{cols}}\\x1b[0m\")
    for r in range(2, rows + 1):
        pid = 1000 + r * 17 + f
        cpu = ((r * 13 + f * 7) % 990) / 10.0
        bar_w = (r * 3 + f * 2) % 24
        bar = \"#\" * bar_w + \".\" * (24 - bar_w)
        fg = 31 + ((r + f) % 6)
        line = f\"{pid:5d} worker-{r:02d}  {cpu:5.1f}% [{bar}] iter={f:02d}\"
        out.append(f\"\\x1b[{r};1H\\x1b[{fg}m{line:<{cols}.{cols}}\\x1b[0m\")
    frames.append((\"\".join(out)).encode(\"ascii\"))
w = sys.stdout.buffer.write
fl = sys.stdout.buffer.flush
w(b\"\\x1b[?1049h\\x1b[?25l\")
fl()
i = 0
while True:
    w(frames[i & 15])
    fl()
    i += 1
'"

run_mixed_case() {
  ACTIVE_DIR="$SCRATCH_ROOT/mixed-10"
  mkdir -m 0700 -p "$ACTIVE_DIR"

  local mixed_frames=$(( FRAMES * 2 ))
  local mixed_max_ms=$(( MAX_WALL_MS * 2 ))
  local mixed_ttl=$(( FLOOD_TTL_SECS + 5 ))
  local load_now
  load_now="$(host_loadavg)"
  echo "Running mixed flood (8 yes + 1 cat-src + 1 quiet focused): terminals=10 (${COLS}x${ROWS}) target_frames=$mixed_frames max_ms=$mixed_max_ms ttl=${mixed_ttl}s load=$load_now ..." >&2
  "$CTL_BIN" start \
    --dir "$ACTIVE_DIR" \
    --ns "$NS" \
    --count 10 \
    --cols "$COLS" \
    --rows "$ROWS" \
    --warmup-ms 200 \
    --ttl-secs "$mixed_ttl" \
    -- /bin/sh -c "idx=1; while ! mkdir '$ACTIVE_DIR'/slot-\$idx 2>/dev/null; do idx=\$((idx + 1)); done; if [ \$idx -le 8 ]; then exec yes; elif [ \$idx -eq 9 ]; then while :; do cat '$CAT_FILE'; done; else printf 'quiet-focused-ready\n'; exec sleep 30; fi"

  local out_file="$SCRATCH_ROOT/out-mixed-10.log"
  GLYPH_WALL_PERF=1 \
  GLYPH_WALL_PERF_MAX_MS="$mixed_max_ms" \
  GLYPH_WALL_CLIPBOARD=memory \
  GLYPH_WALL_RUN="terminal.focus t10" \
  G3D_TERMD_DIR="$ACTIVE_DIR" \
  "$RENDERER" --wall --wall-ns "$NS" --frames "$mixed_frames" > "$out_file"

  local perf_line
  perf_line="$(grep '^wall-perf: ' "$out_file" | tail -n 1)"
  if [[ -z "$perf_line" ]]; then
    echo "wall-perf.sh: missing wall-perf summary line for mixed-10" >&2
    cat "$out_file" >&2
    exit 1
  fi
  printf '%s\t%s\t%s\t%s\n' "mixed-8yes-1cat-1quiet" "10" "$load_now" "$perf_line" >> "$RESULTS_TSV"

  stop_scratch_dir "$ACTIVE_DIR"
  ACTIVE_DIR=""
}

# Realistic 24-line ANSI-coloured terminal screen payload for idle scale terminals.
SCALE_INIT_FILE="$SCRATCH_ROOT/scale-screen.txt"
python3 - "$SCALE_INIT_FILE" "$COLS" "$ROWS" <<'PY'
import sys
path, cols, rows = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
lines = []
lines.append("\x1b[1;38;5;120;48;5;236m glyph3d-wall :: build-worker :: 80x24 ready ".ljust(cols + 18) + "\x1b[0m")
for r in range(2, rows):
    fg = 31 + (r % 6)
    bg = "\x1b[48;5;235m" if (r % 5 == 0) else ""
    txt = f" [{r:02d}] Compiling crate_{r:02d} v0.1.0 (src/module_{r:02d}.rs:{r*17}) -> ok"
    lines.append(f"{bg}\x1b[{fg}m{txt:<{cols}.{cols}}\x1b[0m")
lines.append("\x1b[32muser@host\x1b[0m:\x1b[34m~/dev/glyph3d-rs\x1b[0m$ ")
with open(path, "w", encoding="utf-8") as f:
    f.write("\r\n".join(lines))
PY

run_scale_case() {
  local n="$1"

  ACTIVE_DIR="$SCRATCH_ROOT/scale-n${n}"
  mkdir -m 0700 -p "$ACTIVE_DIR"

  local load_now
  load_now="$(host_loadavg)"
  local ulimit_n
  ulimit_n="$(ulimit -Sn 2>/dev/null || printf 'na')"
  local extra_ctl=()
  if [[ -n "$SCALE_NOFILE" ]]; then
    extra_ctl+=(--nofile "$SCALE_NOFILE")
    ulimit_n="raised:${SCALE_NOFILE}"
  fi

  echo "Running scale: terminals=$n (${COLS}x${ROWS}) nofile=$ulimit_n ttl=${SCALE_TTL_SECS}s load=$load_now ..." >&2
  local ctl_out
  ctl_out="$("$CTL_BIN" start \
    --dir "$ACTIVE_DIR" \
    --ns "$NS" \
    --count "$n" \
    --cols "$COLS" \
    --rows "$ROWS" \
    --warmup-ms 250 \
    --ttl-secs "$SCALE_TTL_SECS" \
    --shell-ttl \
    --allow-limit \
    "${extra_ctl[@]}" \
    -- /bin/sh -c "cat '$SCALE_INIT_FILE'; exec sleep $SCALE_TTL_SECS")"

  local created refused err_msg
  created="$(printf '%s\n' "$ctl_out" | sed -n 's/.*created=\([0-9]*\).*/\1/p' | tail -n 1)"
  refused="$(printf '%s\n' "$ctl_out" | sed -n 's/.*refused=\([01]\).*/\1/p' | tail -n 1)"
  err_msg="$(printf '%s\n' "$ctl_out" | sed -n 's/.*error=\(.*\)$/\1/p' | tail -n 1)"
  created="${created:-0}"
  refused="${refused:-0}"

  if [[ "$refused" == "1" || "$created" -lt "$n" ]]; then
    echo "  -> daemon/OS refused at terminal $((created + 1)) of $n (created=$created): $err_msg" >&2
    printf 'refused\t%s\t%s\t%s\t%s\t%s\n' "$n" "$created" "$ulimit_n" "$load_now" "$err_msg" >> "$SCALE_TSV"
    stop_scratch_dir "$ACTIVE_DIR"
    ACTIVE_DIR=""
    return 2
  fi

  local out_file="$SCRATCH_ROOT/out-scale-n${n}.log"
  GLYPH_WALL_PERF=1 \
  GLYPH_WALL_STARTUP=1 \
  GLYPH_WALL_CLIPBOARD=memory \
  G3D_TERMD_DIR="$ACTIVE_DIR" \
  "$RENDERER" --wall --wall-ns "$NS" > "$out_file" 2>&1 &
  ACTIVE_WIN_PID="$!"

  local holder_pid brain_pid
  holder_pid="$(awk '$1 == "holder" {print $2; exit}' "$ACTIVE_DIR/pids.txt" 2>/dev/null || printf '0')"
  brain_pid="$(awk '$1 == "brain" {print $2; exit}' "$ACTIVE_DIR/pids.txt" 2>/dev/null || printf '0')"

  local phase_json="$SCRATCH_ROOT/scale-phases-n${n}.json"
  python3 - "$ACTIVE_DIR/wall-$NS.sock" "$ACTIVE_WIN_PID" "$holder_pid" "$brain_pid" "$IDLE_MS" "$out_file" > "$phase_json" <<'PY'
import json, math, os, re, socket, subprocess, sys, time

sock_path, win_pid, holder_pid, brain_pid, idle_ms, log_path = (
    sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5]), sys.argv[6]
)

deadline = time.monotonic() + 20.0
while time.monotonic() < deadline and not os.path.exists(sock_path):
    time.sleep(0.02)

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(5.0)
s.connect(sock_path)
buf = b""

def rpc(cmd: str) -> str:
    global buf
    s.sendall((cmd + "\n").encode("utf-8"))
    while b"\n" not in buf:
        chunk = s.recv(4096)
        if not chunk:
            return ""
        buf += chunk
    hdr, buf = buf.split(b"\n", 1)
    need = int(hdr.decode("ascii")) + 1
    while len(buf) < need:
        chunk = s.recv(4096)
        if not chunk:
            break
        buf += chunk
    body = buf[: need - 1].decode("utf-8", errors="replace")
    buf = buf[need:]
    return body

def parse_where(txt: str):
    vw, vh, scale, zoom, anim = 1600.0, 1000.0, 1.0, 1.0, False
    m_vp = re.search(r"viewport w=([0-9.]+) h=([0-9.]+) scale=([0-9.]+)", txt)
    if m_vp:
        vw, vh, scale = float(m_vp.group(1)), float(m_vp.group(2)), float(m_vp.group(3))
    m_cam = re.search(r"camera x=([-0-9.]+) y=([-0-9.]+) zoom=([0-9.]+) animating=(true|false)", txt)
    if m_cam:
        zoom = float(m_cam.group(3))
        anim = (m_cam.group(4) == "true")
    return vw, vh, scale, zoom, anim

def wait_settled(max_s: float = 2.5):
    t_end = time.monotonic() + max_s
    time.sleep(0.08)
    while time.monotonic() < t_end:
        _, _, _, _, anim = parse_where(rpc("app.where"))
        if not anim:
            time.sleep(0.05)
            return
        time.sleep(0.04)

def rss_kb(pid: int) -> int:
    if pid <= 1:
        return 0
    st = f"/proc/{pid}/status"
    if os.path.exists(st):
        try:
            with open(st, "r", encoding="utf-8", errors="replace") as f:
                for line in f:
                    if line.startswith("VmRSS:"):
                        return int(line.split()[1])
        except OSError:
            pass
    try:
        out = subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)], text=True)
        return int(out.strip() or "0")
    except Exception:
        return 0

def frames_of(perf_line: str) -> int:
    m = re.search(r"\bframes=([0-9]+)\b", perf_line)
    return int(m.group(1)) if m else 0

def drive_camera_frames(vw: float, vh: float, target_frames: int = 60, pinch_only: bool = False):
    rpc("app.perf reset")
    step = 0
    t_end = time.monotonic() + 3.5
    cx, cy = vw * 0.5, vh * 0.5
    while time.monotonic() < t_end:
        angle = step * 0.35
        if pinch_only:
            # Off-centre pinch translates + zooms the camera cleanly without routing wheel to scrollback.
            ax = round(cx + 180.0 * math.cos(angle), 1)
            ay = round(cy + 120.0 * math.sin(angle), 1)
            fac = 1.022 if (step % 2 == 0) else (1.0 / 1.022)
            rpc(f"app.pinch {ax} {ay} {fac:.6f}")
        else:
            dx = round(26.0 * math.cos(angle), 1)
            dy = round(18.0 * math.sin(angle), 1)
            rpc(f"app.wheel 6 6 {dx} {dy}")
            if step % 2 == 0:
                fac = 1.018 if ((step // 2) % 2 == 0) else (1.0 / 1.018)
                rpc(f"app.pinch {cx:.1f} {cy:.1f} {fac:.6f}")
        step += 1
        time.sleep(0.015)
        if step >= target_frames and step % 10 == 0:
            cur = rpc("app.perf")
            if frames_of(cur) >= target_frames:
                break
    time.sleep(0.04)
    return rpc("app.perf")

# 0. Wait until non-blocking PendingTermd has connected and adopted all live terminals,
#    then fit all and sample startup timings + steady RSS.
t_conn = time.monotonic() + 15.0
while time.monotonic() < t_conn:
    st = rpc("app.state")
    if st.startswith("connected=true"):
        break
    time.sleep(0.04)
rpc("wall.fit")
wait_settled(3.0)
perf_open = rpc("app.perf")
vw, vh, sf, fit_zoom, _ = parse_where(rpc("app.where"))
win_rss = rss_kb(win_pid)
hold_rss = rss_kb(holder_pid)
brain_rss = rss_kb(brain_pid)

open_ms = "na"
adopt_ms = "na"
try:
    with open(log_path, "r", encoding="utf-8", errors="replace") as lf:
        for line in lf:
            m_ft = re.search(r"\[wall-startup\] t=\+([0-9.]+)ms phase=first_terminal_frame_presented", line)
            if m_ft:
                open_ms = m_ft.group(1)
            m_ad = re.search(r"\[wall-startup\] t=\+[0-9.]+ms dt=([0-9.]+)ms phase=termd_adopt", line)
            if m_ad:
                adopt_ms = m_ad.group(1)
except OSError:
    pass

# 1. Idle 5.0 s window (untouched).
rpc("app.perf reset")
time.sleep(idle_ms / 1000.0)
perf_idle = rpc("app.perf")

# 2. Pan/zoom with ALL N terminals in view (`fit-all`).
rpc("wall.fit")
wait_settled(1.5)
_, _, _, fit_zoom, _ = parse_where(rpc("app.where"))
perf_fit = drive_camera_frames(vw, vh, target_frames=60, pinch_only=True)

# 3. Pan/zoom with MOST terminals off-screen (`wall.reveal t01` frames 1 terminal).
rpc("wall.reveal t01")
wait_settled(1.5)
_, _, _, close_zoom, _ = parse_where(rpc("app.where"))
perf_offscreen = drive_camera_frames(vw, vh, target_frames=60, pinch_only=True)

# 4. Far-zoom: zoom out so every 80x24 terminal (57.6 wu tall) is ~4.6 CSS px tall (`zoom = 0.08`).
rpc("wall.fit")
wait_settled(1.5)
_, _, _, cur_zoom, _ = parse_where(rpc("app.where"))
target_far_zoom = 0.08
if cur_zoom > 0.0:
    rpc(f"app.pinch {vw * 0.5:.1f} {vh * 0.5:.1f} {target_far_zoom / cur_zoom:.6f}")
time.sleep(0.08)
_, _, _, far_zoom, _ = parse_where(rpc("app.where"))
perf_far = drive_camera_frames(vw, vh, target_frames=60, pinch_only=False)

rpc("app.quit")
s.close()

print(json.dumps({
    "vw": vw,
    "vh": vh,
    "scale": sf,
    "fit_zoom": fit_zoom,
    "close_zoom": close_zoom,
    "far_zoom": far_zoom,
    "win_rss_kb": win_rss,
    "holder_rss_kb": hold_rss,
    "brain_rss_kb": brain_rss,
    "open_ms": open_ms,
    "adopt_ms": adopt_ms,
    "perf_open": perf_open,
    "perf_idle": perf_idle,
    "perf_fit": perf_fit,
    "perf_offscreen": perf_offscreen,
    "perf_far": perf_far,
}))
PY

  wait "$ACTIVE_WIN_PID" >/dev/null 2>&1 || true
  ACTIVE_WIN_PID=""

  printf 'ok\t%s\t%s\t%s\t%s\t%s\n' "$n" "$created" "$ulimit_n" "$load_now" "$(cat "$phase_json")" >> "$SCALE_TSV"

  stop_scratch_dir "$ACTIVE_DIR"
  ACTIVE_DIR=""
}

if [[ " $MODES " == *" flood "* ]]; then
  for workload in yes seq cat-src; do
    case "$workload" in
      yes)     cmd="exec yes" ;;
      seq)     cmd="while :; do seq 1 2000000; done" ;;
      cat-src) cmd="while :; do cat '$CAT_FILE'; done" ;;
    esac
    for n in $COUNTS; do
      run_flood_case "$workload" "$n" "$cmd"
    done
  done
fi

if [[ " $MODES " == *" extra "* ]]; then
  run_flood_case "fullscreen-tui" 1 "$FULLSCREEN_TUI_CMD"
  run_wheel_case 14
  run_mixed_case
fi

if [[ " $MODES " == *" idle "* ]]; then
  for n in $IDLE_COUNTS; do
    run_idle_case "$n"
  done
fi

if [[ " $MODES " == *" scale "* ]]; then
  for n in $SCALE_COUNTS; do
    rc=0
    run_scale_case "$n" || rc=$?
    if (( rc == 2 )); then
      echo "Stopping scale sweep at N=$n where daemon/OS limit refused." >&2
      break
    elif (( rc != 0 )); then
      exit "$rc"
    fi
  done
fi

python3 - "$RESULTS_TSV" "$IDLE_TSV" "$SCALE_TSV" <<'PY'
import json, sys

results_path, idle_path, scale_path = sys.argv[1], sys.argv[2], sys.argv[3]

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

with open(results_path, "r", encoding="utf-8") as f:
    flood_rows = [r.strip() for r in f if r.strip()]

if flood_rows:
    print()
    print("### Wall Flood Performance")
    print()
    headers = [
        "Workload", "N", "Load", "Frames", "Wall (s)", "FPS",
        "Pump p50/p95/max (ms)", "Pump sum (ms) / Wakes",
        "Draw p50/p95/max (ms)", "Wait p50/max (ms)",
        "Worst Frame (ms)", "Proc CPU (ms)", "Uploads (drawn/up/rows/writes)", "Status"
    ]
    print("| " + " | ".join(headers) + " |")
    print("| " + " | ".join(["---"] * len(headers)) + " |")
    for raw in flood_rows:
        workload, n, load_str, line = raw.split("\t", 3)
        kv = parse_kv_line(line)
        pump = parse_sub(kv["pump_ms"])
        draw = parse_sub(kv["draw_ms"])
        wait = parse_sub(kv["wait_ms"])
        worst = parse_sub(kv.get("worst_ms", ""))
        up = parse_sub(kv["uploads"])
        wall_s = float(kv["wall_ms"]) / 1000.0
        wake_hz = pump.get("wake_hz", f"{float(pump['wakes']) / wall_s:.1f}" if wall_s > 0 else "0.0")
        wait_col = f"{wait['p50']} / {wait.get('max', wait['p50'])}"
        if worst:
            worst_col = f"{worst['total']} (#{worst['idx']}: acq {worst['acq']}, rnd {worst['render']}, sub {worst['submit']})"
        else:
            worst_col = "na"
        status = "stalled in pump" if kv.get("stalled") == "1" else "ok"
        if kv.get("occluded", "0") != "0":
            status += f" (occl:{kv['occluded']})"
        row = [
            workload,
            n,
            load_str,
            kv["frames"],
            f"{wall_s:.2f}",
            kv["fps"],
            f"{pump['p50']} / {pump['p95']} / {pump['max']}",
            f"{pump['sum']} ({pump['wakes']}w, {wake_hz}/s)",
            f"{draw['p50']} / {draw['p95']} / {draw['max']}",
            wait_col,
            worst_col,
            kv["proc_cpu_ms"],
            f"{up['drawn']}/{up['uploaded']}/{up['rows']}/{up['writes']}",
            status,
        ]
        print("| " + " | ".join(row) + " |")

with open(idle_path, "r", encoding="utf-8") as f:
    idle_rows = [r.strip() for r in f if r.strip()]

if idle_rows:
    print()
    print("### Wall Idle Check")
    print()
    idle_headers = [
        "Case", "N", "Load", "Startup Frames", "Settle+Idle Wall (s)",
        "Idle Window (s)", "Idle Frames", "Idle Proc CPU (ms)", "Total Proc CPU (ms)"
    ]
    print("| " + " | ".join(idle_headers) + " |")
    print("| " + " | ".join(["---"] * len(idle_headers)) + " |")
    for raw in idle_rows:
        case, n, load_str, line = raw.split("\t", 3)
        kv = parse_kv_line(line)
        wall_s = float(kv["wall_ms"]) / 1000.0
        idle_s = float(kv.get("idle_ms", "0")) / 1000.0
        row = [
            case,
            n,
            load_str,
            kv["frames"],
            f"{wall_s:.2f}",
            f"{idle_s:.2f}",
            kv.get("idle_frames", "na"),
            kv.get("idle_cpu_ms", "na"),
            kv["proc_cpu_ms"],
        ]
        print("| " + " | ".join(row) + " |")

with open(scale_path, "r", encoding="utf-8") as f:
    scale_rows = [r.strip() for r in f if r.strip()]

if scale_rows:
    print()
    print("### Wall Scale Performance (80x24 Idle Terminals)")
    print()
    s_headers = [
        "N", "Created", "NOFILE", "Load",
        "Open / Adopt (ms)",
        "Win RSS (MiB)", "Daemon RSS hold+brain (MiB)",
        "5s Idle Frames / CPU (ms)",
        "Fit-All Draw p50/p95/max (ms) [FPS, drawn/f]",
        "Off-Screen (1 vis) Draw p50/p95/max (ms) [FPS, drawn/f]",
        "Far-Zoom (~4.6px) Draw p50/p95/max (ms) [FPS, drawn/f]",
    ]
    print("| " + " | ".join(s_headers) + " |")
    print("| " + " | ".join(["---"] * len(s_headers)) + " |")
    for raw in scale_rows:
        kind, n, created, nofile, load_str, payload = raw.split("\t", 5)
        if kind == "refused":
            row = [
                n, created, nofile, load_str,
                "—", "—", "—", "—", "—", "—",
                f"REFUSED at t{int(created)+1}: `{payload}`",
            ]
            print("| " + " | ".join(row) + " |")
            continue
        d = json.loads(payload)
        open_kv = parse_kv_line(d["perf_open"])
        idle_kv = parse_kv_line(d["perf_idle"])

        def fmt_phase(line: str) -> str:
            kv = parse_kv_line(line)
            dr = parse_sub(kv["draw_ms"])
            up = parse_sub(kv["uploads"])
            fr = max(int(kv["frames"]), 1)
            drawn_per_f = float(up["drawn"]) / fr
            return f"{dr['p50']}/{dr['p95']}/{dr['max']} [{kv['fps']} fps, {drawn_per_f:.1f}/f]"

        win_mib = d["win_rss_kb"] / 1024.0
        hold_mib = d["holder_rss_kb"] / 1024.0
        brain_mib = d["brain_rss_kb"] / 1024.0
        daemon_col = f"{hold_mib + brain_mib:.1f} ({hold_mib:.1f}+{brain_mib:.1f})"
        row = [
            n,
            created,
            nofile,
            load_str,
            f"{d.get('open_ms', open_kv.get('open_ms', 'na'))} / {d.get('adopt_ms', open_kv.get('adopt_ms', 'na'))}",
            f"{win_mib:.1f}",
            daemon_col,
            f"{idle_kv['frames']}f / {idle_kv['proc_cpu_ms']}",
            fmt_phase(d["perf_fit"]),
            fmt_phase(d["perf_offscreen"]),
            fmt_phase(d["perf_far"]),
        ]
        print("| " + " | ".join(row) + " |")
PY
