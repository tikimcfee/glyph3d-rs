#!/usr/bin/env bash
# tools/wall-smoke-termd.sh — the wall against its daemon leaving and its engine restarting.
#
# Starts an isolated scratch g3d-termd + glyph3d-native --wall in namespace `wsexit` under
# /tmp/g3d-ws-$$ (never touching `wall`, `agents`, `default`, or `rs-wall`, never the system
# clipboard, and signalling only the window PID and the scratch daemon it started here), with
# a stub `agent` on PATH, and checks, with the installed `g3d` and `g3d-termd` (or the ones
# `G3D_BIN` and `G3D_TERMD_BIN` name):
#   1. `g3d upgrade` (the brain alone): the wall rides it. Its terminal stays and takes typing
#      afterwards, and no outage notice is left behind.
#   2. `g3d stop` with a shell on the wall (protocol 1.6, "A namespace that leaves"): the red
#      termd notice is up straight away, saying termd stopped, measured from the moment the
#      daemon removes its socket (which it does as it tells its connections); the terminal is
#      gone.
#   3. a daemon started again on the namespace is picked up by the wall's passive watcher:
#      the notice clears and a new terminal lands on it.
#   4. `SIGTERM` to that daemon's holder: the notice says termd was ended by SIGTERM.
#
# The notice names the cause from `daemon.exit_reason` in the namespace's last status
# document. A daemon from before that field says only that it is leaving, and the wall then
# says "termd left on purpose": the script reads which daemon it has from the binary and
# expects accordingly, saying so.
#
# Not here: the idle exit (`exit_reason: idle`), which the wall keeps silent. termd counts a
# connection that has asked for anything but status as keeping the namespace alive, and the
# wall lists the terminals of every daemon it connects to, so a namespace never idles out
# under a running wall (measured 2026-11-27: a `--idle-exit 1` daemon the wall had picked up
# was still there 10 s later, where one with only `g3d status --follow` left after 1.6 s).
# `wall_core::status`'s tests hold the silence, with the field as the daemon sends it.
#
# Prints a PASS/FAIL table; failure PNGs land in target/glyph-scratch/wall-smoke-termd/.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

BIN="$REPO_ROOT/target/release/glyph3d-native"
WALLCTL="$REPO_ROOT/target/release/wallctl"
G3D="${G3D_BIN:-${HOME}/.local/bin/g3d}"
G3D_TERMD="${G3D_TERMD_BIN:-${HOME}/.local/bin/g3d-termd}"
NS="wsexit"

if [[ ! -x "$BIN" || ! -x "$WALLCTL" ]]; then
    echo "ERR: missing $BIN or $WALLCTL (build --release -p glyph3d-native -p wall-control first)" >&2
    exit 1
fi
if [[ ! -x "$G3D" || ! -x "$G3D_TERMD" ]]; then
    echo "ERR: missing $G3D or $G3D_TERMD (the g3d CLI that stops and upgrades the scratch daemon, and the daemon)" >&2
    exit 1
fi
# The wall starts the scratch daemon itself: have it start the one under test.
export G3D_TERMD_BIN="$G3D_TERMD"
# Does this daemon say which way it leaves (`daemon.exit_reason`, protocol 1.6)?
if grep -aq 'exit_reason' "$G3D_TERMD"; then
    SAYS_WHY=1
    echo "NOTE  $G3D_TERMD says why a namespace leaves (daemon.exit_reason): expecting the cause in the notice"
else
    SAYS_WHY=0
    echo "NOTE  $G3D_TERMD predates daemon.exit_reason: expecting \"termd left on purpose\", the wall's words for a cause not given"
fi

D="/tmp/g3d-ws-$$"
mkdir -m 700 -p "$D/bin"
export G3D_TERMD_DIR="$D"
export GLYPH_WALL_CLIPBOARD="memory"
SOCK="$D/termd-$NS.sock"

# Isolate the smoke window from the host's live gomuxai fleet.
cat >"$D/bin/agent" <<'EOF'
#!/bin/sh
while [ $# -gt 0 ]; do
    case "$1" in
        --backend-config) shift; shift ;;
        -q|--quiet) shift ;;
        --*) shift ;;
        *) break ;;
    esac
done
case "${1:-}" in
    export)
        printf '{"type":"snapshot","host":"smoke","stores":[],"agents":[]}\n'
        exec tail -f /dev/null
        ;;
    sessions)
        printf '[]\n'
        ;;
    *)
        exit 0
        ;;
esac
EOF
chmod +x "$D/bin/agent"
export PATH="$D/bin:${HOME}/.local/bin:${PATH}"

SHOT_DIR="$REPO_ROOT/target/glyph-scratch/wall-smoke-termd"
mkdir -p "$SHOT_DIR"
rm -f "$SHOT_DIR"/*.png "$SHOT_DIR"/wall.log

WALL_PID=""
cleanup() {
    "$WALLCTL" --ns "$NS" app.quit >/dev/null 2>&1 || true
    if [[ -n "$WALL_PID" ]]; then
        for _ in $(seq 1 20); do
            kill -0 "$WALL_PID" 2>/dev/null || break
            sleep 0.05
        done
        kill "$WALL_PID" 2>/dev/null || true
        wait "$WALL_PID" 2>/dev/null || true
    fi
    "$G3D" --ns "$NS" stop >/dev/null 2>&1 || true
    rm -rf "$D"
}
trap cleanup EXIT INT TERM

PASS_COUNT=0
FAIL_COUNT=0

ctl() {
    "$WALLCTL" --ns "$NS" "$@"
}

record_pass() {
    PASS_COUNT=$((PASS_COUNT + 1))
    printf 'PASS  %-30s %s\n' "$1" "$2"
}

record_fail() {
    FAIL_COUNT=$((FAIL_COUNT + 1))
    local slug="${1//[^a-zA-Z0-9_-]/_}"
    local shot="$SHOT_DIR/fail-${slug}.png"
    ctl app.shot "$shot" >/dev/null 2>&1 || true
    printf 'FAIL  %-30s %s  [shot: %s]\n' "$1" "$2" "$shot"
}

wait_for_text() {
    local tid="$1" needle="$2" tries="${3:-60}" txt=""
    for _ in $(seq 1 "$tries"); do
        txt="$(ctl app.text "$tid" 2>/dev/null || true)"
        if grep -Fq "$needle" <<<"$txt"; then
            return 0
        fi
        sleep 0.05
    done
    return 1
}

notice_line() {
    ctl app.where 2>/dev/null | grep '^termd_notice ' || echo "termd_notice ?"
}

termd_line() {
    ctl app.where 2>/dev/null | grep '^termd ' || echo "termd ?"
}

# The notice the wall shows when the namespace left the way $1 names (stop, sigterm) with
# shells in it.
lost_notice() {
    local phrase="termd left on purpose"
    if [[ "$SAYS_WHY" -eq 1 ]]; then
        case "$1" in
            stop) phrase="termd stopped" ;;
            sigterm) phrase="termd ended by SIGTERM" ;;
        esac
    fi
    printf 'termd_notice kind=lost dismiss=true text=%s: the shells in namespace "%s" ended with it · a new terminal starts a fresh termd' "$phrase" "$NS"
}

# The pid of the scratch namespace's holder, from its lock file, when that pid is a
# g3d-termd of this namespace (the only daemon this script ever signals).
scratch_holder_pid() {
    local pid cmd
    pid="$(tr -cd '0-9' <"$SOCK.lock" 2>/dev/null)"
    [[ -n "$pid" ]] || return 1
    cmd="$(ps -p "$pid" -o command= 2>/dev/null)"
    [[ "$cmd" == *g3d-termd* && "$cmd" == *"--ns $NS"* ]] || return 1
    printf '%s\n' "$pid"
}

# The wall, from `$D` so no repo-local launch_config overrides PATH discovery of the stub.
(cd "$D" && "$BIN" --wall --wall-ns "$NS") >"$SHOT_DIR/wall.log" 2>&1 &
WALL_PID=$!

READY=0
for _ in $(seq 1 240); do
    if [[ -S "$D/wall-$NS.sock" ]] && W="$(ctl app.where 2>/dev/null)" && grep -q "^terminal t1 " <<<"$W"; then
        READY=1
        break
    fi
    sleep 0.05
done
if [[ "$READY" -ne 1 ]]; then
    echo "FAIL  startup: control socket or t1 never appeared (see $SHOT_DIR/wall.log)" >&2
    exit 1
fi
ctl terminal.focus t1 >/dev/null
ctl app.type "echo BEFORE_\$((40+2))" >/dev/null
ctl app.key enter >/dev/null
wait_for_text t1 "BEFORE_42" 80 || true

# ── 1. `g3d upgrade` replaces the brain alone: the wall rides it ─────────────
UP_OUT="$("$G3D" --ns "$NS" upgrade 2>&1)"
UP_RC=$?
# The restart is ridden once typing reaches the shell again, on the new brain.
ctl app.type "echo AFTER_\$((70+7))" >/dev/null
ctl app.key enter >/dev/null
RODE=0
wait_for_text t1 "AFTER_77" 200 && RODE=1
W_UP="$(ctl app.where 2>/dev/null)"
UP_NOTICE="$(grep '^termd_notice ' <<<"$W_UP")"
if [[ "$UP_RC" -eq 0 && "$RODE" -eq 1 && "$UP_NOTICE" == "termd_notice none" ]] \
    && grep -q "^terminal t1 " <<<"$W_UP" && [[ -S "$SOCK" ]]; then
    record_pass "g3d upgrade (brain) is ridden" "t1 kept its screen and took typing after it; ${UP_NOTICE}"
else
    record_fail "g3d upgrade (brain) is ridden" "rc=$UP_RC rode=$RODE notice='${UP_NOTICE}' out='$(tr '\n' ' ' <<<"$UP_OUT")'"
fi

# ── 2. `g3d stop` with a shell on the wall: the outage notice, straight away ──
STOP_CHECK="$(python3 - "$G3D" "$NS" "$SOCK" "$WALLCTL" "$(lost_notice stop)" <<'PY'
import os, subprocess, sys, time

g3d, ns, sock, wallctl, WANT = sys.argv[1:6]

def where():
    out = subprocess.run([wallctl, "--ns", ns, "app.where"], capture_output=True, text=True).stdout
    return out.splitlines()

asked = time.monotonic()
stop = subprocess.run([g3d, "--ns", ns, "stop"], capture_output=True, text=True)
stopped = time.monotonic()

# The daemon removes its socket as it tells its connections: that is its notice.
gone = None
seen = None
line = "termd_notice ?"
lines = []
give_up = time.monotonic() + 8.0
while time.monotonic() < give_up and seen is None:
    if gone is None and not os.path.exists(sock):
        gone = time.monotonic()
    lines = where()
    line = next((l for l in lines if l.startswith("termd_notice ")), "termd_notice ?")
    if line.startswith("termd_notice kind=lost"):
        seen = time.monotonic()
    else:
        time.sleep(0.002)
if gone is None and not os.path.exists(sock):
    gone = seen

t1_gone = not any(l.startswith("terminal t1 ") for l in lines)
ok = (
    stop.returncode == 0
    and seen is not None
    and gone is not None
    and line == WANT
    and t1_gone
    # Straight away: no restart grace (5 s), no reconnect window; one poll of app.where is
    # a few ms of this.
    and (seen - gone) <= 0.100
)
if seen is None or gone is None:
    print(f"0 no lost notice within 8 s: '{line}' (socket gone: {gone is not None}, stop rc={stop.returncode})")
else:
    print(f"{int(ok)} notice {max(0.0, seen - gone) * 1e3:.0f} ms after the daemon's own notice"
          f" (the daemon took {(gone - stopped) * 1e3:.0f} ms after stop returned;"
          f" {(seen - asked) * 1e3:.0f} ms after stop was asked); t1 gone: {t1_gone}; '{line}'")
PY
)"
if [[ "${STOP_CHECK%% *}" == "1" ]]; then
    record_pass "g3d stop shows the outage" "${STOP_CHECK#* }"
else
    record_fail "g3d stop shows the outage" "${STOP_CHECK#* }"
fi

# ── 3. A daemon back on the namespace is picked up, and the notice clears ────
"$G3D_TERMD" --ns "$NS" --daemon >/dev/null 2>&1
BACK=0
for _ in $(seq 1 200); do
    if [[ -S "$SOCK" ]] && [[ "$(notice_line)" == "termd_notice none" ]]; then
        BACK=1
        break
    fi
    sleep 0.05
done
ctl wall.spawn >/dev/null
FRESH=0
for _ in $(seq 1 200); do
    W_NEW="$(ctl app.where 2>/dev/null)" || { sleep 0.05; continue; }
    if grep -q "^terminal " <<<"$W_NEW"; then
        FRESH=1
        break
    fi
    sleep 0.05
done
if [[ "$BACK" -eq 1 && "$FRESH" -eq 1 ]]; then
    record_pass "a daemon back is picked up" "notice cleared; $(grep -c '^terminal ' <<<"$W_NEW") new terminal on the new daemon; $(grep '^termd_notice ' <<<"$W_NEW")"
else
    record_fail "a daemon back is picked up" "back=$BACK fresh=$FRESH socket=$([[ -S "$SOCK" ]] && echo yes || echo no) $(notice_line)"
fi

# ── 4. SIGTERM to the holder: the notice says so ────────────────────────────
HOLDER_PID="$(scratch_holder_pid || true)"
TERM_NOTICE="termd_notice ?"
if [[ -n "$HOLDER_PID" ]]; then
    kill -TERM "$HOLDER_PID"
    for _ in $(seq 1 200); do
        TERM_NOTICE="$(notice_line)"
        [[ "$TERM_NOTICE" == "termd_notice kind=lost"* ]] && break
        sleep 0.05
    done
fi
if [[ -n "$HOLDER_PID" && "$TERM_NOTICE" == "$(lost_notice sigterm)" ]]; then
    record_pass "SIGTERM shows the outage" "holder $HOLDER_PID; '${TERM_NOTICE#termd_notice kind=lost dismiss=true text=}'"
else
    record_fail "SIGTERM shows the outage" "holder='${HOLDER_PID:-none found}' notice='${TERM_NOTICE}' want='$(lost_notice sigterm)'"
fi

echo ""
echo "=== TERMD SMOKE SUMMARY: ${PASS_COUNT} passed, ${FAIL_COUNT} failed ==="
[[ "$FAIL_COUNT" -eq 0 ]]
