#!/usr/bin/env bash
# tools/wall-smoke-agents.sh — live end-to-end smoke check for WallAgents & Agent View cards.
#
# Starts an isolated scratch g3d-termd + glyph3d-native --wall in namespace `probe`
# (G3D_TERMD_DIR=/tmp/g3d-agv-$$; never touching `wall`, `agents`, `default`, or `rs-wall`)
# with a deterministic stub `agent` CLI on PATH (never spawning, messaging, or touching the
# host's live gomuxai fleet), and verifies:
#   1. 10-second idle guarantee: zero `agent` invocations while no terminal is an agent
#      and no Agent View, Sidebar, or Palette is open
#   2. opening an Agent View with the keyboard chord (`cmd+a`) pairs `t1` (`gx-coder`),
#      hydrates tail events, starts `--follow`, and renders the card beside `t1`
#   3. closing with `cmd+a` and reopening by clicking the hovered title-bar `≡` button
#   4. zooming out below `BADGE_FLOOR_PX` collapses the card to its compact badge, and
#      clicking the badge reveals `t1` back to normal zoom
#   5. placing the Agent View on all four sides (`left`, `top`, `bottom`, `right`)
#   6. filtering by category (`wall.agent.filter t1 only edit` + reset)
#   7. bulk expand and collapse (`wall.agent.expand` / `wall.agent.collapse`)
#   8. backward pagination (`wall.agent.earlier`) prepending older events & adopting orphans
#   9. write-time scrollback anchor + `wall.agent.jump` scrolling `t1` into primary history,
#      plus unpinned unread count (`wall.agent.follow off`)
#  10. `wall.agent.follow t1 tail` + `wall.agent.jump t1 live` returning `t1` to live tail
#  11. `wall.bus`, Sidebar (`cmd+b`), and Command Palette (`cmd+k`) listing live fleet agents
#  12. a loose neighbour shoved aside by an Agent View card, and restored
#  13. an extra namespace's `gx-coder` terminal: paired, detached, reattached
#  14. the sidebar's one list: an agent with no terminal attaches on a click and merges
#      into its terminal's row; two groups and two hosts listed by group, then by host
#      through the header switch, the mode kept in the store
#
# Saves `app.shot` PNGs under target/glyph-scratch/wall-smoke-agents/ (with `--shots`, one
# per step; always the sidebar by group and by host, `sidebar-by-group.png` and
# `sidebar-by-host.png`) and tears down only the window PID started here plus the scratch
# daemon.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

BIN="$REPO_ROOT/target/release/glyph3d-native"
WALLCTL="$REPO_ROOT/target/release/wallctl"
G3D="${HOME}/.local/bin/g3d"
G3D_TERMD="${HOME}/.local/bin/g3d-termd"
NS="probe"
EXTRA_NS="probe2"

if [[ ! -x "$BIN" || ! -x "$WALLCTL" ]]; then
    echo "ERR: missing $BIN or $WALLCTL (build --release -p glyph3d-native -p wall-control first)" >&2
    exit 1
fi

START_EPOCH="$(python3 -c 'import time; print(f"{time.time():.2f}")')"

D="/tmp/g3d-agv-$$"
mkdir -m 700 -p "$D/bin" "$D/data"
export G3D_TERMD_DIR="$D"
export GLYPH_WALL_CLIPBOARD="memory"

SHOT_DIR="$REPO_ROOT/target/glyph-scratch/wall-smoke-agents"
mkdir -p "$SHOT_DIR"
rm -f "$SHOT_DIR"/fail-*.png "$SHOT_DIR"/wall.log
WALL_SMOKE_SHOTS="${WALL_SMOKE_SHOTS:-0}"
if [[ "${1:-}" == "--shots" ]]; then
    WALL_SMOKE_SHOTS=1
fi
if [[ "$WALL_SMOKE_SHOTS" == "1" ]]; then
    rm -f "$SHOT_DIR"/*.png
fi

FIFO="$D/pty.fifo"
mkfifo "$FIFO"
exec 3<>"$FIFO"

WALL_PID=""
cleanup() {
    exec 3>&- 2>/dev/null || true
    "$WALLCTL" --ns "$NS" app.quit >/dev/null 2>&1 || true
    if [[ -n "$WALL_PID" ]]; then
        for _ in $(seq 1 20); do
            kill -0 "$WALL_PID" 2>/dev/null || break
            sleep 0.05
        done
        kill "$WALL_PID" 2>/dev/null || true
        wait "$WALL_PID" 2>/dev/null || true
    fi
    if [[ -x "$G3D" ]]; then
        "$G3D" --ns "$NS" stop >/dev/null 2>&1 || true
        "$G3D" --ns "$EXTRA_NS" stop >/dev/null 2>&1 || true
    fi
    rm -rf "$D"
}
trap cleanup EXIT INT TERM

# ── Build stub `agent` CLI and recorded EVENTS.md / export fixtures ──────────
INVOCATIONS_LOG="$D/data/invocations.log"
TRANSCRIPT_FILE="$D/data/coder-session.jsonl"
EXPORT_STREAM="$D/data/export.jsonl"
FOLLOW_STREAM="$D/data/follow.jsonl"
SESSIONS_JSON="$D/data/sessions.json"
PAGE_TAIL="$D/data/page-tail.jsonl"
PAGE_EARLIER="$D/data/page-earlier.jsonl"

: >"$INVOCATIONS_LOG"
: >"$FOLLOW_STREAM"

python3 - "$D" "$TRANSCRIPT_FILE" "$EXPORT_STREAM" "$SESSIONS_JSON" "$PAGE_TAIL" "$PAGE_EARLIER" <<'PY'
import json, sys

d_dir, transcript_file, export_stream, sessions_json, page_tail, page_earlier = sys.argv[1:7]

# Ensure transcript file size on disk exceeds `until = 700` so `read_transcript_window` never clamps.
with open(transcript_file, "w", encoding="utf-8") as fh:
    fh.write("\n" * 4096)

sessions = [
    {
        "session_id": "sess-coder-1",
        "harness": "claude",
        "path": transcript_file,
        "size": 4096,
        "mtime": "2026-11-26T18:30:00Z",
        "cwd": d_dir,
        "project": "glyph3d-rs",
        "title": "Hook WallAgents into native wall",
        "status": "processing",
        "model": "claude-opus-4-6",
        "workgroup": {"name": "coder", "host": "laptop-2"},
    },
    {
        "session_id": "sess-reviewer-2",
        "harness": "agy",
        "path": transcript_file + ".rev",
        "size": 1024,
        "mtime": "2026-11-26T18:20:00Z",
        "cwd": d_dir + "/review",
        "project": "glyph3d-rs",
        "title": "Audit WakeGate coalescing",
        "status": "idle",
        "workgroup": {"name": "reviewer", "host": "laptop-2"},
    },
]
with open(sessions_json, "w", encoding="utf-8") as fh:
    json.dump(sessions, fh)

export_snap = {
    "type": "snapshot",
    "host": "laptop-2",
    "stores": [{"dir": d_dir + "/store"}],
    "agents": [
        {
            "name": "coder",
            "host": "laptop-2",
            "store": d_dir + "/store",
            "backend": "claude",
            "session": "coder",
            "cwd": d_dir,
            "created_at": "2026-11-26T18:00:00Z",
            "live": True,
            "attached": True,
            "status": "processing",
            "detail": "editing native/src/wall/app.rs",
            "tool": "Edit",
            "session_id": "sess-coder-1",
            "transcript": transcript_file,
            "dialect": "claude",
        },
        # A record whose terminal session is gone (`attached: false`): export still prints
        # its last report, `idle`, and the wall reads it as `agent status` does, ended (gone).
        {
            "name": "reviewer",
            "host": "laptop-2",
            "store": d_dir + "/store",
            "backend": "agy",
            "session": "reviewer",
            "cwd": d_dir,
            "created_at": "2026-11-26T18:05:00Z",
            "live": True,
            "attached": False,
            "status": "idle",
            "detail": "ready for diff",
            "session_id": "sess-reviewer-2",
            "dialect": "agy",
        },
        {
            "name": "perf-probe",
            "host": "laptop-2",
            "store": d_dir + "/store",
            "backend": "claude",
            "session": "perf-probe",
            "cwd": d_dir,
            "created_at": "2026-11-26T18:10:00Z",
            "live": True,
            "attached": True,
            "status": "waiting",
            "tool": "Bash",
            "detail": "tools/wall-perf.sh",
            "dialect": "claude",
        },
        {
            "name": "linux-gate",
            "host": "workstation",
            "store": "",
            "backend": "claude",
            "session": "linux-gate",
            "cwd": "/home/user/glyph3d-rs",
            "created_at": "2026-11-26T17:50:00Z",
            "live": True,
            "attached": False,
            "status": "idle",
            "detail": "108 passed",
            "dialect": "claude",
        },
        # Another host's agent that CAN be attached (a remote host reports no
        # `attached`): listed bright where `linux-gate` is dimmed. Nothing here
        # attaches it: that would run the real `agent` CLI (see #14).
        {
            "name": "bench",
            "host": "workstation",
            "store": "",
            "backend": "claude",
            "session": "bench",
            "cwd": "/home/user/glyph3d-rs",
            "created_at": "2026-11-26T17:55:00Z",
            "live": True,
            "status": "processing",
            "detail": "cargo bench",
            "dialect": "claude",
        },
    ],
}
with open(export_stream, "w", encoding="utf-8") as fh:
    fh.write(json.dumps(export_snap) + "\n")

tail_frames = [
    {
        "type": "session",
        "session_id": "sess-coder-1",
        "harness": "claude",
        "cwd": d_dir,
        "title": "Hook WallAgents into native wall",
        "status": "processing",
    },
    # Orphan update for `ev-0` (whose `add` sits in `page_earlier` before `at = 700`).
    {
        "type": "event",
        "op": "update",
        "id": "ev-0",
        "seq": 2,
        "at": 500,
        "line": 3,
        "ts": "2026-11-26T18:25:01Z",
        "turn": {"id": "turn-1", "index": 1},
        "kind": "tool",
        "tool": {
            "id": "call-0",
            "name": "Bash",
            "action": "bash",
            "detail": "cargo test -p agent-feed",
            "result": {"status": "ok", "output": "test result: ok. 17 passed; 0 failed"},
        },
    },
    {
        "type": "event",
        "op": "add",
        "id": "ev-1",
        "seq": 3,
        "at": 700,
        "line": 6,
        "ts": "2026-11-26T18:25:10Z",
        "turn": {"id": "turn-1", "index": 1},
        "kind": "prompt",
        "prompt": {
            "origin": "human",
            "text": "Hook WallAgents into native/src/wall/app.rs with WakeGate coalescing and verify live.",
        },
    },
    {
        "type": "event",
        "op": "add",
        "id": "ev-2",
        "seq": 4,
        "at": 1100,
        "line": 10,
        "ts": "2026-11-26T18:25:14Z",
        "turn": {"id": "turn-1", "index": 1},
        "kind": "think",
        "think": {
            "text": "Check WakeGate.redraw_armed and pending atomics in App::open.\nEnsure egui pass runs when agents_open is true.",
        },
    },
    {
        "type": "event",
        "op": "add",
        "id": "ev-3",
        "seq": 5,
        "at": 1500,
        "line": 15,
        "ts": "2026-11-26T18:25:18Z",
        "turn": {"id": "turn-1", "index": 1},
        "kind": "tool",
        "tool": {
            "id": "call-1",
            "name": "Read",
            "action": "read",
            "target": "native/src/wall/app.rs",
            "meta": {"range": [508, 600]},
            "file": {
                "text": "let session_wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {\n    if !gate_sess.redraw_armed.load(Ordering::SeqCst)\n        && !gate_sess.pending.swap(true, Ordering::SeqCst)\n    {\n        let _ = proxy_sess.send_event(Wake);\n    }\n});",
                "start": 508,
                "total": 2236,
            },
            "result": {"status": "ok", "output": ""},
        },
    },
    {
        "type": "event",
        "op": "add",
        "id": "ev-4",
        "seq": 6,
        "at": 2100,
        "line": 22,
        "ts": "2026-11-26T18:25:25Z",
        "turn": {"id": "turn-1", "index": 1},
        "kind": "tool",
        "tool": {
            "id": "call-2",
            "name": "Edit",
            "action": "edit",
            "target": "native/src/wall/app.rs",
            "meta": {"added": 14, "removed": 2},
            "file": {
                "text": "+ agents.sync_sessions(sessions);\n+ agents_changed = agents.pump(wall);",
                "start": 1070,
                "total": 2250,
            },
            "result": {"status": "ok", "output": "Updated native/src/wall/app.rs"},
        },
    },
    {
        "type": "event",
        "op": "add",
        "id": "ev-5",
        "seq": 7,
        "at": 2600,
        "line": 28,
        "ts": "2026-11-26T18:25:30Z",
        "turn": {"id": "turn-1", "index": 1},
        "kind": "say",
        "say": {
            "text": "WallAgents is wired through WakeGate and drawing companion cards in the egui pass.",
            "final": True,
        },
    },
]
with open(page_tail, "w", encoding="utf-8") as fh:
    for obj in tail_frames:
        fh.write(json.dumps(obj) + "\n")

earlier_frames = [
    {
        "type": "event",
        "op": "add",
        "id": "ev-pre",
        "seq": 0,
        "at": 0,
        "line": 1,
        "ts": "2026-11-26T18:24:50Z",
        "turn": {"id": "turn-0", "index": 0},
        "kind": "prompt",
        "prompt": {"origin": "human", "text": "Run the agent-feed baseline tests first."},
    },
    {
        "type": "event",
        "op": "add",
        "id": "ev-0",
        "seq": 1,
        "at": 220,
        "line": 2,
        "ts": "2026-11-26T18:25:00Z",
        "turn": {"id": "turn-1", "index": 1},
        "kind": "tool",
        "tool": {
            "id": "call-0",
            "name": "Bash",
            "action": "bash",
            "detail": "cargo test -p agent-feed",
            "result": {"status": "pending", "output": ""},
        },
    },
]
with open(page_earlier, "w", encoding="utf-8") as fh:
    for obj in earlier_frames:
        fh.write(json.dumps(obj) + "\n")
PY

cat >"$D/bin/agent" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >>"$INVOCATIONS_LOG"
# Strip leading global flags (--backend-config <file>, -q, --quiet).
while [ \$# -gt 0 ]; do
    case "\$1" in
        --backend-config) shift; shift ;;
        -q|--quiet) shift ;;
        --*) shift ;;
        *) break ;;
    esac
done
sub="\${1:-}"
[ \$# -gt 0 ] && shift
case "\$sub" in
    export)
        # The feed down (#18): the CLI dies at once, as one whose bus is gone does.
        if [ -f "$D/data/feed-down" ]; then echo 'the bus is down (smoke)' >&2; exit 1; fi
        echo \$\$ >"$D/data/export.pid"
        exec tail -n +1 -f "$EXPORT_STREAM"
        ;;
    sessions)
        cat "$SESSIONS_JSON"
        ;;
    attach)
        case "\${1:-}" in
            ghost*)
                # An agent that was cleaned up, as the real CLI says it.
                printf 'Error: agent "%s" not found on host workstation (2 agents there: agent list --host workstation)\n' "\${1%%@*}" >&2
                exit 1
                ;;
            *)
                # A session that stays up until its terminal is closed.
                printf 'STUB_ATTACHED %s\n' "\${1:-}"
                exec sleep 600
                ;;
        esac
        ;;
    status)
        case "\${1:-}" in
            ghost*)
                printf 'Error: agent "%s" not found on host workstation (2 agents there: agent list --host workstation)\n' "\${1%%@*}" >&2
                exit 1
                ;;
            *) printf '%s: idle\n' "\${1:-}" ;;
        esac
        ;;
    transcript)
        mode="window"
        until_val=""
        while [ \$# -gt 0 ]; do
            case "\$1" in
                --follow) mode="follow" ;;
                --until) shift; until_val="\${1:-}" ;;
            esac
            shift
        done
        if [ "\$mode" = "follow" ]; then
            exec tail -n +1 -f "$FOLLOW_STREAM"
        elif [ "\$until_val" = "700" ]; then
            cat "$PAGE_EARLIER"
        else
            cat "$PAGE_TAIL"
        fi
        ;;
    *)
        exit 1
        ;;
esac
EOF
chmod +x "$D/bin/agent"

export PATH="$D/bin:${HOME}/.local/bin:${PATH}"

PASS_COUNT=0
FAIL_COUNT=0

ctl() {
    "$WALLCTL" --ns "$NS" "$@"
}

maybe_shot() {
    if [[ "$WALL_SMOKE_SHOTS" == "1" ]]; then
        ctl app.shot "$1" >/dev/null 2>&1 || true
    fi
}

record_pass() {
    local name="$1"
    local detail="$2"
    PASS_COUNT=$((PASS_COUNT + 1))
    printf 'PASS  %-34s %s\n' "$name" "$detail"
}

record_fail() {
    local name="$1"
    local detail="$2"
    FAIL_COUNT=$((FAIL_COUNT + 1))
    local slug="${name//[^a-zA-Z0-9_-]/_}"
    local shot="$SHOT_DIR/fail-${slug}.png"
    ctl app.shot "$shot" >/dev/null 2>&1 || true
    printf 'FAIL  %-34s %s  [shot: %s]\n' "$name" "$detail" "$shot"
}

wait_settled() {
    local w=""
    for _ in $(seq 1 60); do
        w="$(ctl app.where 2>/dev/null)" || { sleep 0.05; continue; }
        if grep -q "animating=false" <<<"$w"; then
            printf '%s\n' "$w"
            return 0
        fi
        sleep 0.04
    done
    ctl app.where 2>/dev/null || true
}

wait_for_text() {
    local tid="$1"
    local needle="$2"
    local tries="${3:-50}"
    local txt=""
    for _ in $(seq 1 "$tries"); do
        txt="$(ctl app.text "$tid" 2>/dev/null || true)"
        if grep -Fq "$needle" <<<"$txt"; then
            return 0
        fi
        sleep 0.05
    done
    return 1
}

expanded_count_of() {
    python3 -c '
import re, sys
s = sys.stdin.read()
m = re.search(r"(?:expanded|collapsed) (\d+) event", s)
print(m.group(1) if m else "-1")
'
}

# Start scratch wall window from `$D` so no repo-local launch_config overrides `PATH` discovery.
# `--wall-extra-ns agents=probe2` exercises passive extra-namespace watching (`probe2` starts later in #12).
(cd "$D" && "$BIN" --wall --wall-ns "$NS" --wall-extra-ns "agents=$EXTRA_NS") >"$SHOT_DIR/wall.log" 2>&1 &
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
ctl wall.fit >/dev/null
W="$(wait_settled)"
CHORD="$(awk '$1 == "platform" { for (i = 3; i <= NF; i++) if ($i ~ /^chord=/) print substr($i, 7) }' <<<"$W")"
CHORD="${CHORD:-cmd}"

# ── 1. 10-second idle guarantee: zero `agent` commands while idle ────────────
sleep 10.2
IDLE_CALLS="$(wc -l <"$INVOCATIONS_LOG" | tr -d ' ')"
if [[ "$IDLE_CALLS" -eq 0 ]]; then
    record_pass "idle 10s zero agent commands" "invocations=0 over 10.2s with plain terminal"
else
    record_fail "idle 10s zero agent commands" "expected 0 invocations, got $IDLE_CALLS: $(tr '\n' ';' <"$INVOCATIONS_LOG")"
fi

# Start FIFO reader in `t1`, set OSC-0 title `gx-coder` so `resolve_terminal_session` pairs `t1`
# to `sess-coder-1`, and print initial lines.
ctl app.type "cat $FIFO" >/dev/null
ctl app.key enter >/dev/null
sleep 0.25
printf '\033]0;gx-coder\007\033[2J\033[H' >&3
for i in $(seq -w 1 35); do
    printf 'BOOT_HISTORY_LINE_%s\r\n' "$i" >&3
done
printf 'AGENT_READY_PROMPT\r\n' >&3
if ! wait_for_text t1 "AGENT_READY_PROMPT" 60; then
    record_fail "agent pty setup" "t1 never printed AGENT_READY_PROMPT"
    exit 1
fi

# ── 2. Open Agent View via `${CHORD}+a` chord & normal-zoom card ─────────────
ctl app.key "${CHORD}+a" >/dev/null
PAIRED_EXP=0
for _ in $(seq 1 60); do
    PAIRED_EXP="$(ctl wall.agent.expand t1 2>/dev/null | expanded_count_of)"
    if [[ "$PAIRED_EXP" -ge 4 ]]; then
        break
    fi
    sleep 0.05
done
ctl wall.agent.collapse t1 >/dev/null 2>&1 || true
ctl wall.fit >/dev/null
W="$(wait_settled)"
maybe_shot "$SHOT_DIR/01-card-normal-zoom.png"

if [[ "$PAIRED_EXP" -ge 4 ]] && grep -q "^sessions" "$INVOCATIONS_LOG" && grep -q "^transcript .* --follow" "$INVOCATIONS_LOG"; then
    record_pass "${CHORD}+a chord opens & pairs card" "paired sess-coder-1, hydrated $PAIRED_EXP expandable events, started --follow"
else
    record_fail "${CHORD}+a chord opens & pairs card" "expanded=$PAIRED_EXP log=$(tr '\n' ';' <"$INVOCATIONS_LOG")"
fi

# ── 3. Close with `${CHORD}+a`, reopen via hovered title-bar `≡` button ──────
ctl app.key "${CHORD}+a" >/dev/null
ctl wall.fit >/dev/null
W_CLOSED="$(wait_settled)"
# Compute `t1` title-bar hover coordinates and the leftmost (`≡` Agent View) button center
# matching `bar_buttons::button_cluster_rects`:
#   size = clamp(bar_h - 4, 8, 18), cluster_w = 3*size + 4, x0 = r - 3 - cluster_w.
read -r BAR_MID_X BAR_MID_Y BTN_AG_X BTN_AG_Y < <(python3 -c '
import sys
for line in sys.stdin:
    if line.startswith("titlebar t1 "):
        for tok in line.split():
            if tok.startswith("rect="):
                l, t, r, b = map(float, tok[5:].split(","))
                bh = b - t
                size = max(8.0, min(18.0, bh - 4.0))
                cluster_w = 3.0 * size + 4.0
                x0 = r - 3.0 - cluster_w
                print(f"{(l+r)/2:.2f} {(t+b)/2:.2f} {x0 + size*0.5:.2f} {(t+b)/2:.2f}")
                sys.exit(0)
' <<<"$W_CLOSED")
# Move onto the bar first so `hovered` becomes true and `bar_buttons` lays out on screen, then click `≡`.
ctl app.move "$BAR_MID_X" "$BAR_MID_Y" >/dev/null
sleep 0.08
maybe_shot "$SHOT_DIR/02a-titlebar-buttons-hover.png"
ctl app.click "$BTN_AG_X" "$BTN_AG_Y" >/dev/null
sleep 0.10
ctl wall.fit >/dev/null
W_REOPEN="$(wait_settled)"
# In `wall.fit`, an open right-hand Agent View doubles the horizontal footprint vs closed, so zoom is smaller.
ZOOM_CLOSED="$(python3 -c 'import sys; print([ dict(t.split("=") for t in l.split()[1:])["zoom"] for l in sys.stdin if l.startswith("camera ") ][0])' <<<"$W_CLOSED")"
ZOOM_REOPEN="$(python3 -c 'import sys; print([ dict(t.split("=") for t in l.split()[1:])["zoom"] for l in sys.stdin if l.startswith("camera ") ][0])' <<<"$W_REOPEN")"
maybe_shot "$SHOT_DIR/02b-reopened-via-bar-button.png"

BAR_BTN_OK="$(python3 -c 'import sys; zc, zr = float(sys.argv[1]), float(sys.argv[2]); print(int(zr < zc * 0.85))' "$ZOOM_CLOSED" "$ZOOM_REOPEN")"
if [[ "$BAR_BTN_OK" == "1" ]]; then
    record_pass "title-bar button toggles card" "closed fit_zoom=$ZOOM_CLOSED -> clicked bar button -> reopened fit_zoom=$ZOOM_REOPEN"
else
    record_fail "title-bar button toggles card" "zoom_closed=$ZOOM_CLOSED zoom_reopen=$ZOOM_REOPEN btn=($BTN_AG_X,$BTN_AG_Y)"
fi

# ── 4. Far-zoom badge & badge click reveals terminal ─────────────────────────
ctl app.pinch 800 500 0.22 >/dev/null
W_BADGE="$(wait_settled)"
maybe_shot "$SHOT_DIR/03-badge-far-zoom.png"
read -r Z_FAR BADGE_X BADGE_Y < <(python3 -c '
import sys
zoom = 1.0
r = t = 0.0
for line in sys.stdin:
    if line.startswith("camera "):
        zoom = float(dict(tok.split("=") for tok in line.split()[1:])["zoom"])
    elif line.startswith("titlebar t1 "):
        tb_l, tb_t, tb_r, tb_b = map(float, [p[5:] for p in line.split() if p.startswith("rect=")][0].split(","))
        t = tb_t
        r = tb_r
# Badge sits at `(r + gap * zoom, tb_t)` with min size `28x18` pt (`gap = 1.5` world units).
bx = r + 1.5 * zoom + 14.0
by = t + 9.0
print(f"{zoom:.4f} {bx:.2f} {by:.2f}")
' <<<"$W_BADGE")
ctl app.click "$BADGE_X" "$BADGE_Y" >/dev/null
sleep 0.10
W_REVEALED="$(wait_settled)"
Z_REV="$(python3 -c 'import sys; print([ dict(t.split("=") for t in l.split()[1:])["zoom"] for l in sys.stdin if l.startswith("camera ") ][0])' <<<"$W_REVEALED")"
BADGE_OK="$(python3 -c 'import sys; zf, zr = float(sys.argv[1]), float(sys.argv[2]); print(int(zf < 2.5 and zr > zf * 1.8))' "$Z_FAR" "$Z_REV")"
if [[ "$BADGE_OK" == "1" ]]; then
    record_pass "far-zoom badge & click reveal" "far_zoom=$Z_FAR (<2.61 floor) -> badge click revealed zoom=$Z_REV"
else
    record_fail "far-zoom badge & click reveal" "far_zoom=$Z_FAR revealed_zoom=$Z_REV badge=($BADGE_X,$BADGE_Y)"
fi

# ── 5. All four sides (`left`, `top`, `bottom`, `right`) ─────────────────────
SIDES_OK=1
for side in left top bottom right; do
    R_SIDE="$(ctl wall.agent.side t1 "$side")"
    ctl wall.fit >/dev/null
    wait_settled >/dev/null
    maybe_shot "$SHOT_DIR/04-side-${side}.png"
    if [[ "$R_SIDE" != *"($side)"* && "$R_SIDE" != *"side: $side"* && "$R_SIDE" != *"$side"* ]]; then
        SIDES_OK=0
    fi
done
if [[ "$SIDES_OK" -eq 1 ]]; then
    record_pass "four sides placement" "placed on left, top, bottom, right"
else
    record_fail "four sides placement" "last reply: $R_SIDE"
fi

# ── 6. Category filter (`only edit` + `reset`) ───────────────────────────────
R_FILT="$(ctl wall.agent.filter t1 only edit)"
EXP_FILT="$(ctl wall.agent.expand t1 | expanded_count_of)"
maybe_shot "$SHOT_DIR/05-filter-only-edit.png"
ctl wall.agent.collapse t1 >/dev/null 2>&1 || true
ctl wall.agent.filter t1 reset >/dev/null
EXP_RESET="$(ctl wall.agent.expand t1 | expanded_count_of)"
ctl wall.agent.collapse t1 >/dev/null 2>&1 || true

if [[ "$R_FILT" == OK* && "$EXP_FILT" -eq 1 && "$EXP_RESET" -ge 4 ]]; then
    record_pass "category filter & reset" "only edit -> 1 matching expandable row; reset -> $EXP_RESET rows"
else
    record_fail "category filter & reset" "filt_reply='$R_FILT' exp_filt=$EXP_FILT exp_reset=$EXP_RESET"
fi

# ── 7. Expand & collapse rows ────────────────────────────────────────────────
R_EXP="$(ctl wall.agent.expand t1)"
N_EXP="$(expanded_count_of <<<"$R_EXP")"
maybe_shot "$SHOT_DIR/06a-expanded-rows.png"
R_COL="$(ctl wall.agent.collapse t1)"
N_COL="$(expanded_count_of <<<"$R_COL")"
maybe_shot "$SHOT_DIR/06b-collapsed-rows.png"

if [[ "$N_EXP" -ge 4 && "$N_COL" -eq "$N_EXP" ]]; then
    record_pass "expand & collapse rows" "expanded $N_EXP event(s), collapsed $N_COL event(s)"
else
    record_fail "expand & collapse rows" "expand='$R_EXP' collapse='$R_COL'"
fi

# ── 8. Load earlier (`wall.agent.earlier`) + orphan update adoption ──────────
R_EARLIER="$(ctl wall.agent.earlier t1)"
N_AFTER_EARLIER=0
for _ in $(seq 1 40); do
    N_AFTER_EARLIER="$(ctl wall.agent.expand t1 | expanded_count_of)"
    ctl wall.agent.collapse t1 >/dev/null 2>&1 || true
    if [[ "$N_AFTER_EARLIER" -gt "$N_EXP" ]]; then
        break
    fi
    sleep 0.05
done
# Expand `ev-0` specifically to verify its orphan `ok` result ("17 passed") rendered cleanly.
R_SHOW_EV0="$(ctl wall.agent.show t1 ev-0 full)"
maybe_shot "$SHOT_DIR/07-load-earlier-adopted.png"
ctl wall.agent.collapse t1 >/dev/null 2>&1 || true

if [[ "$R_EARLIER" == OK* && "$N_AFTER_EARLIER" -gt "$N_EXP" && "$R_SHOW_EV0" == OK* ]]; then
    record_pass "load earlier & orphan adoption" "expandable rows $N_EXP -> $N_AFTER_EARLIER; ev-0 expanded"
else
    record_fail "load earlier & orphan adoption" "earlier='$R_EARLIER' before=$N_EXP after=$N_AFTER_EARLIER show_ev0='$R_SHOW_EV0'"
fi

# ── 9. Live anchor stamp, scroll away, unread badge, and `wall.agent.jump` ───
printf 'MARKER_AT_ANCHOR_777\r\n' >&3
wait_for_text t1 "MARKER_AT_ANCHOR_777" 40 || true
sleep 0.12
python3 -c '
import json, sys
with open(sys.argv[1], "a", encoding="utf-8") as fh:
    fh.write(json.dumps({
        "type": "event",
        "op": "add",
        "id": "ev-jump-anchor",
        "seq": 8,
        "at": 3100,
        "line": 35,
        "ts": "2026-11-26T18:26:00Z",
        "turn": {"id": "turn-2", "index": 2},
        "kind": "tool",
        "tool": {
            "id": "call-anchor",
            "name": "Bash",
            "action": "bash",
            "detail": "echo MARKER_AT_ANCHOR_777",
            "result": {"status": "ok", "output": "MARKER_AT_ANCHOR_777"},
        },
    }) + "\n")
' "$FOLLOW_STREAM"
sleep 0.25

# Push `MARKER_AT_ANCHOR_777` ~45 lines up into primary scrollback off the 30-row screen.
for i in $(seq -w 1 45); do
    printf 'LATER_SCROLL_OUTPUT_%s\r\n' "$i" >&3
done
printf 'LIVE_BOTTOM_999\r\n' >&3
wait_for_text t1 "LIVE_BOTTOM_999" 50 || true
sleep 0.15
TXT_LIVE_BEFORE_JUMP="$(ctl app.text t1)"

# Unpin `follow` so arriving live events accumulate an unread badge (`+2 · Jump to latest`).
ctl wall.agent.follow t1 off >/dev/null
python3 -c '
import json, sys
with open(sys.argv[1], "a", encoding="utf-8") as fh:
    for idx, msg in [(9, "Verifying scrollback jump to MARKER_AT_ANCHOR_777."), (10, "Unread badge active while unpinned.")]:
        fh.write(json.dumps({
            "type": "event",
            "op": "add",
            "id": f"ev-unread-{idx}",
            "seq": idx,
            "at": 3200 + idx * 100,
            "line": 40 + idx,
            "ts": "2026-11-26T18:26:05Z",
            "turn": {"id": "turn-2", "index": 2},
            "kind": "say",
            "say": {"text": msg, "final": idx == 10},
        }) + "\n")
' "$FOLLOW_STREAM"
sleep 0.25

R_JUMP="$(ctl wall.agent.jump t1 ev-jump-anchor)"
JUMPED_SAW_ANCHOR=0
if wait_for_text t1 "MARKER_AT_ANCHOR_777" 40; then
    JUMPED_SAW_ANCHOR=1
fi
maybe_shot "$SHOT_DIR/08-jumped-to-anchor-and-unread.png"

if ! grep -Fq "MARKER_AT_ANCHOR_777" <<<"$TXT_LIVE_BEFORE_JUMP" && [[ "$R_JUMP" == *"jumped to ev-jump-anchor"* && "$JUMPED_SAW_ANCHOR" -eq 1 ]]; then
    record_pass "jump to event scrolls terminal" "reply='$R_JUMP'; MARKER_AT_ANCHOR_777 scrolled back into view"
else
    record_fail "jump to event scrolls terminal" "reply='$R_JUMP' saw_anchor=$JUMPED_SAW_ANCHOR"
fi

# ── 10. Follow live (`wall.agent.follow tail` + `wall.agent.jump live`) ──────
R_FOLLOW="$(ctl wall.agent.follow t1 tail)"
R_LIVE="$(ctl wall.agent.jump t1 live)"
BACK_AT_LIVE=0
if wait_for_text t1 "LIVE_BOTTOM_999" 40; then
    BACK_AT_LIVE=1
fi
sleep 0.10
maybe_shot "$SHOT_DIR/09-follow-live-restored.png"

if [[ "$R_FOLLOW" == OK* && "$R_LIVE" == *"scrolled to live"* && "$BACK_AT_LIVE" -eq 1 ]]; then
    record_pass "follow live restores tail" "follow='$R_FOLLOW' jump='$R_LIVE' LIVE_BOTTOM_999 visible"
else
    record_fail "follow live restores tail" "follow='$R_FOLLOW' jump='$R_LIVE' back_at_live=$BACK_AT_LIVE"
fi

# ── 11. Bus snapshot in `wall.bus`, Sidebar (`${CHORD}+b`), and Palette (`${CHORD}+k`) ─
BUS_OUT="$(ctl wall.bus)"
ctl app.key "${CHORD}+b" >/dev/null
wait_settled >/dev/null
sleep 0.10
maybe_shot "$SHOT_DIR/10-sidebar-live-agents.png"

ctl app.key "${CHORD}+k" >/dev/null
sleep 0.12
maybe_shot "$SHOT_DIR/11-palette-live-agents.png"
ctl app.key Escape >/dev/null
ctl app.key "${CHORD}+b" >/dev/null
wait_settled >/dev/null

if [[ "$BUS_OUT" == *"laptop-2"* && "$BUS_OUT" == *"coder"* && "$BUS_OUT" == *"reviewer"* && "$BUS_OUT" == *"workstation"* && "$BUS_OUT" == *"linux-gate"* && "$BUS_OUT" == *"bench"* ]]; then
    record_pass "bus snapshot in sidebar & palette" "hosts laptop-2 + workstation (5 live agents) populated"
else
    record_fail "bus snapshot in sidebar & palette" "wall.bus output: $(tr '\n' ' ' <<<"$BUS_OUT")"
fi

# ── 12. Loose-canvas neighbour shove-aside, restore & spawn past open card ────
world_bounds_of() {
    local tid="$1"
    local w="$2"
    python3 -c '
import sys
tid = sys.argv[1]
for line in sys.stdin:
    parts = line.split()
    if len(parts) >= 2 and parts[0] == "terminal" and parts[1] == tid:
        for tok in parts[2:]:
            if tok.startswith("world="):
                vals = tok[6:].split(",")
                print(vals[0], vals[1], vals[2], vals[3])
                sys.exit(0)
print("0 0 0 0")
' "$tid" <<<"$w"
}

# 12a. `t1` currently has its right-side Agent View open (`width=76`, `gap=1.5`):
#      spawning a new loose terminal (`t2`) must step past `t1 + card` (`t1_max_x + 77.5`).
ctl wall.spawn >/dev/null
W_SPAWN_OPEN=""
for _ in $(seq 1 60); do
    W_SPAWN_OPEN="$(ctl app.where 2>/dev/null || true)"
    if grep -q "^terminal t2 " <<<"$W_SPAWN_OPEN"; then
        break
    fi
    sleep 0.04
done
W_SPAWN_OPEN="$(wait_settled)"
read -r _ _ T1_MAX_X _ < <(world_bounds_of t1 "$W_SPAWN_OPEN")
read -r T2_OPEN_SPAWN_MIN_X _ _ _ < <(world_bounds_of t2 "$W_SPAWN_OPEN")
ctl wall.close t2 >/dev/null 2>&1 || true
for _ in $(seq 1 60); do
    if ! ctl app.where 2>/dev/null | grep -q "^terminal t2 "; then
        break
    fi
    sleep 0.04
done

# 12b. Close `t1`'s Agent View, spawn a right-hand neighbour (`t3`) directly beside `t1`,
#      then open `t1`'s Agent View (`t3` shoves clear of the card) and close it (`t3` returns).
ctl wall.agent t1 close >/dev/null
ctl wall.reveal t1 >/dev/null
wait_settled >/dev/null
ctl wall.spawn >/dev/null
NB_ID=""
W_NB_START=""
for _ in $(seq 1 60); do
    W_NB_START="$(ctl app.where 2>/dev/null || true)"
    NB_ID="$(awk '$1 == "terminal" && $2 != "t1" { print $2; exit }' <<<"$W_NB_START")"
    if [[ -n "$NB_ID" ]]; then
        break
    fi
    sleep 0.04
done
W_NB_START="$(wait_settled)"
read -r NB_START_MIN_X _ _ _ < <(world_bounds_of "$NB_ID" "$W_NB_START")

ctl wall.agent t1 open >/dev/null
ctl wall.fit >/dev/null
W_NB_SHOVED="$(wait_settled)"
maybe_shot "$SHOT_DIR/12-neighbour-shoved.png"
read -r NB_SHOVED_MIN_X _ _ _ < <(world_bounds_of "$NB_ID" "$W_NB_SHOVED")

ctl wall.agent t1 close >/dev/null
W_NB_RESTORED="$(wait_settled)"
read -r NB_RESTORED_MIN_X _ _ _ < <(world_bounds_of "$NB_ID" "$W_NB_RESTORED")
ctl wall.close "$NB_ID" >/dev/null 2>&1 || true
if [[ -n "$NB_ID" ]]; then
    for _ in $(seq 1 60); do
        if ! ctl app.where 2>/dev/null | grep -q "^terminal ${NB_ID} "; then
            break
        fi
        sleep 0.04
    done
fi

SHOVE_CHECK="$(python3 -c "
t1_r = float('$T1_MAX_X')
card_r = t1_r + 1.5 + 76.0
spawn_x = float('$T2_OPEN_SPAWN_MIN_X')
start_x = float('$NB_START_MIN_X')
shoved_x = float('$NB_SHOVED_MIN_X')
restored_x = float('$NB_RESTORED_MIN_X')
ok = (
    spawn_x > card_r
    and start_x > t1_r and start_x < card_r
    and shoved_x >= card_r + 2.0 - 1e-3
    and abs(restored_x - start_x) < 1e-3
)
print('1' if ok else '0', f'card_r={card_r:.1f} spawn_x={spawn_x:.1f} {start_x:.1f}->{shoved_x:.1f}->{restored_x:.1f}')
")"
SHOVE_OK="${SHOVE_CHECK%% *}"
SHOVE_DETAIL="${SHOVE_CHECK#* }"
if [[ "$SHOVE_OK" == "1" ]]; then
    record_pass "loose neighbour shove & spawn" "$SHOVE_DETAIL"
else
    record_fail "loose neighbour shove & spawn" "$SHOVE_DETAIL"
fi

# ── 13. Extra namespace (`probe2` playing `agents`): `gx-coder` pair, detach & reattach ─

# Start scratch `probe2` daemon in `$D` (`G3D_TERMD_DIR="$D"`) and create `agent-coder`
# via `g3d-termd` wire protocol 1.5 with NO OSC title and NO OSC 7 pwd.
"$G3D_TERMD" --ns "$EXTRA_NS" --daemon >/dev/null 2>&1
for _ in $(seq 1 60); do
    [[ -S "$D/termd-${EXTRA_NS}.sock" ]] && break
    sleep 0.05
done

python3 - "$D/termd-${EXTRA_NS}.sock" "create" <<'PY'
import json, socket, struct, sys

sock_path, action = sys.argv[1], sys.argv[2]

def send_frame(s, ty, payload_obj):
    raw = json.dumps(payload_obj).encode("utf-8")
    hdr = struct.pack("<IBBH I", 8 + len(raw), ty, 0, 0, 0)
    s.sendall(hdr + raw)

def recv_frame(s):
    buf = b""
    while len(buf) < 12:
        chunk = s.recv(4096)
        if not chunk:
            raise RuntimeError("eof")
        buf += chunk
    length, ty, flags, _, stream = struct.unpack("<IBBHI", buf[:12])
    need = length - 8
    payload = buf[12:]
    while len(payload) < need:
        chunk = s.recv(4096)
        if not chunk:
            raise RuntimeError("eof")
        payload += chunk
    return ty, json.loads(payload[:need].decode("utf-8"))

with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
    s.connect(sock_path)
    send_frame(s, 0x01, {"proto": [1, 5], "client": "wall-smoke-agents"})
    recv_frame(s)  # WELCOME
    send_frame(s, 0x02, {
        "req": 1,
        "op": "create",
        "id": "agent-coder",
        "cols": 80,
        "rows": 24,
        "argv": ["/bin/sh", "-c", "printf 'EXTRA_AGENT_LIVE_42\\r\\n'; exec sleep 60"],
    })
    while True:
        ty, msg = recv_frame(s)
        if ty == 0x82 and msg.get("req") == 1:
            assert msg.get("ok"), f"create failed: {msg}"
            break
PY

# Wait for passive extra-namespace watcher on `Sessions` to connect and mount `gx-coder`.
GX_MOUNTED=0
if wait_for_text "gx-coder" "EXTRA_AGENT_LIVE_42" 60; then
    GX_MOUNTED=1
fi

# Pair `gx-coder` with `sess-coder-1` purely via its `gx-<name>` canvas id.
ctl wall.agent gx-coder open >/dev/null 2>&1 || true
GX_PAIRED_EXP=0
for _ in $(seq 1 40); do
    GX_PAIRED_EXP="$(ctl wall.agent.expand gx-coder 2>/dev/null | expanded_count_of)"
    if [[ "$GX_PAIRED_EXP" -ge 5 ]]; then
        break
    fi
    sleep 0.05
done
ctl wall.agent.collapse gx-coder >/dev/null 2>&1 || true

# Close `gx-coder` on the wall: must detach locally without killing `agent-coder` in `probe2`,
# and must stay suppressed across `status_follow` (>650 ms).
ctl wall.close gx-coder >/dev/null
sleep 0.70
W_AFTER_CLOSE="$(ctl app.where)"
STILL_IN_PROBE2="$(python3 - "$D/termd-${EXTRA_NS}.sock" <<'PY'
import json, socket, struct, sys

def send_frame(s, ty, payload_obj):
    raw = json.dumps(payload_obj).encode("utf-8")
    hdr = struct.pack("<IBBHI", 8 + len(raw), ty, 0, 0, 0)
    s.sendall(hdr + raw)

def recv_frame(s):
    buf = b""
    while len(buf) < 12:
        buf += s.recv(4096)
    length, ty, _, _, _ = struct.unpack("<IBBHI", buf[:12])
    need = length - 8
    payload = buf[12:]
    while len(payload) < need:
        payload += s.recv(4096)
    return ty, json.loads(payload[:need].decode("utf-8"))

with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
    s.connect(sys.argv[1])
    send_frame(s, 0x01, {"proto": [1, 5], "client": "wall-smoke-check"})
    recv_frame(s)
    send_frame(s, 0x02, {"req": 2, "op": "list"})
    while True:
        ty, msg = recv_frame(s)
        if ty == 0x82 and msg.get("req") == 2:
            ids = [t["id"] for t in msg.get("terminals", [])]
            print("1" if "agent-coder" in ids else "0")
            break
PY
)"

# Now re-pick the closed local agent (`wall.attach coder@laptop-2`, as Sidebar & Palette emit):
# must reattach `gx-coder` directly via `Sessions::reattach` (no `t2` shell wrapper spawned!).
R_REATTACH="$(ctl wall.attach coder@laptop-2)"
GX_REATTACHED=0
if wait_for_text "gx-coder" "EXTRA_AGENT_LIVE_42" 40; then
    GX_REATTACHED=1
fi
W_AFTER_REATTACH="$(ctl app.where)"

if [[ "$GX_MOUNTED" -eq 1 && "$GX_PAIRED_EXP" -ge 5 ]] \
    && ! grep -q "^terminal gx-coder " <<<"$W_AFTER_CLOSE" \
    && [[ "$STILL_IN_PROBE2" == "1" && "$GX_REATTACHED" -eq 1 ]] \
    && ! grep -q "^terminal t[2-9] " <<<"$W_AFTER_REATTACH"; then
    record_pass "extra-ns gx-coder detach & reattach" "mounted gx-coder, paired ($GX_PAIRED_EXP rows), detached cleanly, reattached via wall.attach without wrapper"
else
    record_fail "extra-ns gx-coder detach & reattach" "mounted=$GX_MOUNTED exp=$GX_PAIRED_EXP alive=$STILL_IN_PROBE2 reattached=$GX_REATTACHED attach_reply='$R_REATTACH'"
fi

# ── 14. The sidebar's one list: click-to-attach, merged rows, group & host modes ──
# `rows=` on app.where's `panel sidebar` line is the list as drawn, top to bottom:
# `g:<group>[<host>=<n>,...]`, `s:none[...]`, `h:<host>[<n>]`, `t:<terminal>`,
# `t:<terminal>+<agent>=<status>` (an agent in its terminal's row), `a:<agent>=<status>`.
sb_tok() { # $1 = app.where text, $2 = key: the value of `key=` on the sidebar's line
    python3 -c '
import sys
key = sys.argv[1]
for line in sys.stdin:
    if line.startswith("panel sidebar "):
        for tok in line.split():
            if tok.startswith(key + "="):
                print(tok[len(key) + 1:])
                sys.exit(0)
print("")
' "$2" <<<"$1"
}

sb_center() { # $1 = app.where text, $2 = key, [$3 = an entry of a `;`-joined map]: "cx cy"
    python3 -c '
import sys
key, entry = sys.argv[1], sys.argv[2]
for line in sys.stdin:
    if line.startswith("panel sidebar "):
        for tok in line.split():
            if tok.startswith(key + "="):
                raw = tok[len(key) + 1:]
                if entry:
                    raw = next(
                        (i.rsplit(":", 1)[1] for i in raw.split(";") if i.rsplit(":", 1)[0] == entry),
                        "none",
                    )
                if raw != "none":
                    l, t, r, b = map(float, raw.split(","))
                    print(f"{(l + r) / 2:.2f} {(t + b) / 2:.2f}")
                    sys.exit(0)
print("0 0")
' "$2" "${3:-}" <<<"$1"
}

wait_sidebar_rows() { # poll until the sidebar's `rows=` holds $1; prints the last app.where
    local w=""
    for _ in $(seq 1 80); do
        w="$(ctl app.where 2>/dev/null)" || { sleep 0.05; continue; }
        if [[ "$(sb_tok "$w" rows)" == *"$1"* ]]; then
            printf '%s\n' "$w"
            return 0
        fi
        sleep 0.05
    done
    printf '%s\n' "$w"
}

term_ids() {
    ctl app.where 2>/dev/null | awk '/^terminal / {print $2}' | sort | tr '\n' ' '
}

# `coder` lives in the extra namespace, so the wall reattaches its termd session directly
# (#13). Any other agent's attach runs the `agent attach` program INSIDE a terminal, which
# the checks after this one do, now that the terminal is handed the wall's own `agent`
# (the stub above) instead of looking one up on the login shell's PATH.
ctl wall.rename t1 shell >/dev/null
ctl wall.close gx-coder >/dev/null
ctl wall.sidebar left >/dev/null
ctl wall.sidebar mode group >/dev/null
W_OL_OPEN="$(wait_sidebar_rows "a:coder@laptop-2=processing")"
ROWS_OPEN="$(sb_tok "$W_OL_OPEN" rows)"

# (a) `coder` has no terminal on the wall now: a click on its row attaches it, and from
#     then on it is listed in that terminal's row, with its status.
read -r CODER_X CODER_Y < <(sb_center "$W_OL_OPEN" agents "coder@laptop-2")
ctl app.click "$CODER_X" "$CODER_Y" >/dev/null
W_OL_ATTACHED="$(wait_sidebar_rows "t:gx-coder+coder@laptop-2=processing")"
ROWS_ATTACHED="$(sb_tok "$W_OL_ATTACHED" rows)"
IDS_ATTACHED="$(term_ids)"

# (b) Three more shells, and two groups.
ctl wall.spawn >/dev/null
ctl wall.spawn >/dev/null
ctl wall.spawn >/dev/null
NEW_IDS=""
for _ in $(seq 1 80); do
    NEW_IDS="$(python3 -c '
import sys
before, now = set(sys.argv[1].split()), sys.argv[2].split()
print(" ".join(i for i in now if i not in before))
' "$IDS_ATTACHED" "$(term_ids)")"
    [[ "$(wc -w <<<"$NEW_IDS" | tr -d ' ')" -ge 3 ]] && break
    sleep 0.05
done
read -r SH1 SH2 SH3 _ <<<"$NEW_IDS "
ctl wall.rename "${SH1:-none}" logs >/dev/null
ctl wall.rename "${SH2:-none}" watch >/dev/null
ctl wall.rename "${SH3:-none}" scratch >/dev/null
G_BUILD="$(ctl wall.group t1 gx-coder | sed -n 's/^OK: \(g-[0-9]*\) .*/\1/p')"
G_OPS="$(ctl wall.group "${SH1:-none}" "${SH2:-none}" | sed -n 's/^OK: \(g-[0-9]*\) .*/\1/p')"
ctl wall.group.rename "${G_BUILD:-none}" build >/dev/null
ctl wall.group.rename "${G_OPS:-none}" ops >/dev/null
ctl terminal.focus gx-coder >/dev/null
ctl wall.fit >/dev/null
wait_settled >/dev/null

# (c) By group: each group's members and a count by host, then `no group` with this
#     machine first, its agents with no terminal after its terminals.
W_OL_GROUP="$(wait_sidebar_rows "s:none[laptop-2=3,workstation=2]")"
ROWS_GROUP="$(sb_tok "$W_OL_GROUP" rows)"
MODE_GROUP="$(sb_tok "$W_OL_GROUP" mode)"
# Park the pointer in the sidebar's empty foot, so no row is drawn hovered in the shots.
read -r PARK_X PARK_Y < <(python3 -c '
import sys
l, t, r, b = map(float, sys.argv[1].split(","))
print(f"{(l + r) / 2:.2f} {b - 30.0:.2f}")
' "$(sb_tok "$W_OL_GROUP" rect)")
ctl app.move "$PARK_X" "$PARK_Y" >/dev/null
ctl app.shot "$SHOT_DIR/sidebar-by-group.png" >/dev/null 2>&1 || true

# (d) The header's `host` half: each host with its count, rows sorted by group.
read -r MODE_X MODE_Y < <(sb_center "$W_OL_GROUP" mode_host)
ctl app.click "$MODE_X" "$MODE_Y" >/dev/null
W_OL_HOST="$(wait_sidebar_rows "h:workstation[2]")"
ROWS_HOST="$(sb_tok "$W_OL_HOST" rows)"
MODE_HOST="$(sb_tok "$W_OL_HOST" mode)"
ctl app.move "$PARK_X" "$PARK_Y" >/dev/null
ctl app.shot "$SHOT_DIR/sidebar-by-host.png" >/dev/null 2>&1 || true
STORE_MODE="$(python3 -c '
import json, sys
try:
    print(json.load(open(sys.argv[1])).get("sidebar", {}).get("mode", "missing"))
except Exception as e:
    print(f"unreadable: {e}")
' "$D/wall-$NS.json")"

# (e) The verb, back to group; then the sidebar closes.
R_MODE_BACK="$(ctl wall.sidebar mode group)"
W_OL_BACK="$(wait_sidebar_rows "g:${G_BUILD:-none}[")"
MODE_BACK="$(sb_tok "$W_OL_BACK" mode)"
ctl wall.sidebar close >/dev/null

OL_CHECK="$(python3 -c '
import sys

(rows_open, rows_attached, rows_group, rows_host, ids_attached, sh1, sh2, sh3, g_build, g_ops,
 mode_group, mode_host, store_mode, mode_back, r_back) = sys.argv[1:16]

coder = "t:gx-coder+coder@laptop-2=processing"
away = ["a:bench@workstation=processing", "a:linux-gate@workstation=idle"]

# `coder` closed: an agent row of its own, between the others by urgency.
want_open = ["s:none[laptop-2=4,workstation=2]", "t:t1", "a:perf-probe@laptop-2=waiting",
             "a:coder@laptop-2=processing", "a:reviewer@laptop-2=ended", *away]
# Clicked: one row, the terminal, with the status of the agent it shows.
# The session of `reviewer` is gone: it reads ended, whatever it last reported.
loose = ["a:perf-probe@laptop-2=waiting", "a:reviewer@laptop-2=ended"]
want_attached = ["s:none[laptop-2=4,workstation=2]", "t:t1", coder, *loose, *away]

members = {g_build: ["t:t1", coder], g_ops: [f"t:{sh1}", f"t:{sh2}"]}
want_group = []
for gid in sorted(members):
    want_group += [f"g:{gid}[laptop-2=2]", *members[gid]]
want_group += ["s:none[laptop-2=3,workstation=2]", f"t:{sh3}", *loose, *away]

want_host = ["h:laptop-2[7]"] + [row for gid in sorted(members) for row in members[gid]]
want_host += [f"t:{sh3}", *loose, "h:workstation[2]", *away]

checks = {
    "open": rows_open.split(";") == want_open,
    # The click attached `coder` through its own termd session: no wrapper terminal.
    "click-attaches": rows_attached.split(";") == want_attached
    and ids_attached.split() == ["gx-coder", "t1"],
    "by-group": rows_group.split(";") == want_group and mode_group == "group",
    "by-host": rows_host.split(";") == want_host and mode_host == "host",
    "stored": store_mode == "host",
    "verb-back": mode_back == "group" and r_back == "OK: sidebar lists by group",
    "ids": all([sh1, sh2, sh3, g_build, g_ops]) and g_build != g_ops,
}
bad = [name for name, ok in checks.items() if not ok]
if bad:
    print("0 failed=" + ",".join(bad)
          + f" | open={rows_open} | attached={rows_attached} ids={ids_attached}"
          + f" | group={rows_group} | host={rows_host} | store={store_mode}"
          + f" | back={mode_back} {r_back!r}")
else:
    print(f"1 {len(want_group)} rows by group ({g_build}, {g_ops}), {len(want_host)} by host"
          f" (laptop-2 7, workstation 2); coder attached on a click and merged into gx-coder;"
          f" mode kept in the store")
' "$ROWS_OPEN" "$ROWS_ATTACHED" "$ROWS_GROUP" "$ROWS_HOST" "$IDS_ATTACHED" \
    "${SH1:-}" "${SH2:-}" "${SH3:-}" "${G_BUILD:-}" "${G_OPS:-}" \
    "$MODE_GROUP" "$MODE_HOST" "$STORE_MODE" "$MODE_BACK" "$R_MODE_BACK")"
if [[ "${OL_CHECK%% *}" == "1" ]]; then
    record_pass "sidebar one list (group & host)" "${OL_CHECK#* }"
else
    record_fail "sidebar one list (group & host)" "${OL_CHECK#* }"
fi

# ── 15. An attach terminal runs the wall's own `agent`: the stub ─────────────
# `bench` is on another host, so its attach is the `agent attach` program in a terminal.
# Until 2026-11-27 that program looked `agent` up through the login shell's PATH and ran
# the real CLI here; the stub's log never saw the attach.
new_term_id() { # $1 = ids before: the first terminal id that is new now
    python3 -c '
import sys
before = set(sys.argv[1].split())
print(next((i for i in sys.argv[2].split() if i not in before), ""))
' "$1" "$(term_ids)"
}
IDS_15="$(term_ids)"
R_ATT="$(ctl wall.attach bench@workstation 2>&1)"
ATT_ID=""
for _ in $(seq 1 100); do
    ATT_ID="$(new_term_id "$IDS_15")"
    [[ -n "$ATT_ID" ]] && break
    sleep 0.05
done
ATT_SEEN=0
[[ -n "$ATT_ID" ]] && wait_for_text "$ATT_ID" "STUB_ATTACHED bench@workstation" 120 && ATT_SEEN=1
ATT_RAN="$(grep -c '^attach bench@workstation$' "$INVOCATIONS_LOG")"
if [[ "$ATT_SEEN" -eq 1 && "$ATT_RAN" -eq 1 ]]; then
    record_pass "attach terminal runs the stub" "wall.attach bench@workstation opened $ATT_ID, whose program was the stub (its banner on screen, 1 attach in its log)"
else
    record_fail "attach terminal runs the stub" "reply='${R_ATT}' id='${ATT_ID}' banner=$ATT_SEEN stub attach calls=$ATT_RAN text='$(ctl app.text "${ATT_ID:-none}" 2>/dev/null | tr '\n' ' ' | cut -c1-200)'"
fi
[[ -n "$ATT_ID" ]] && ctl wall.close "$ATT_ID" >/dev/null

# ── 16. An agent that was cleaned up: said once, never retried ──────────────
# The fleet does not know `ghost`, so the wall tries it, and the CLI answers as the real
# one does for a cleaned-up agent. The terminal says so, offers no retry, and Enter closes.
IDS_16="$(term_ids)"
ctl wall.attach ghost@workstation >/dev/null 2>&1
GHOST_ID=""
for _ in $(seq 1 100); do
    GHOST_ID="$(new_term_id "$IDS_16")"
    [[ -n "$GHOST_ID" ]] && break
    sleep 0.05
done
GONE_NOTE="[ghost@workstation is not an agent any more (it was cleaned up), so there is nothing to attach to. Enter closes.]"
# The note is longer than the terminal is wide, so it wraps: compare with all whitespace out.
GHOST_SAID=0
GHOST_TEXT=""
for _ in $(seq 1 120); do
    GHOST_TEXT="$(ctl app.text "${GHOST_ID:-none}" 2>/dev/null || true)"
    if [[ "$(tr -d '[:space:]' <<<"$GHOST_TEXT")" == *"$(tr -d '[:space:]' <<<"$GONE_NOTE")"* ]]; then
        GHOST_SAID=1
        break
    fi
    sleep 0.05
done
GHOST_RETRY=0
[[ "$(tr -d '[:space:]' <<<"$GHOST_TEXT")" == *"Enterattachesagain"* ]] && GHOST_RETRY=1
GHOST_CLOSED=0
if [[ -n "$GHOST_ID" ]]; then
    ctl terminal.focus "$GHOST_ID" >/dev/null
    ctl app.key enter >/dev/null
    for _ in $(seq 1 100); do
        if ! grep -qw "$GHOST_ID" <<<"$(term_ids)"; then
            GHOST_CLOSED=1
            break
        fi
        sleep 0.05
    done
fi
GHOST_ATTACHES="$(grep -c '^attach ghost@workstation$' "$INVOCATIONS_LOG")"
GHOST_ASKED="$(grep -c '^status ghost@workstation$' "$INVOCATIONS_LOG")"
if [[ "$GHOST_SAID" -eq 1 && "$GHOST_RETRY" -eq 0 && "$GHOST_CLOSED" -eq 1 \
    && "$GHOST_ATTACHES" -eq 1 && "$GHOST_ASKED" -eq 1 ]]; then
    record_pass "a cleaned-up agent is not retried" "$GHOST_ID said it once, offered no retry, and closed on Enter (1 attach, 1 status in the stub's log)"
else
    record_fail "a cleaned-up agent is not retried" "id='${GHOST_ID}' said=$GHOST_SAID retry_offered=$GHOST_RETRY closed=$GHOST_CLOSED attaches=$GHOST_ATTACHES status=$GHOST_ASKED text='$(tr '\n' ' ' <<<"$GHOST_TEXT" | cut -c1-240)'"
fi

# ── 17. A cleaned-up agent leaves the list ──────────────────────────────────
# The follow stream's event lines as the CLI prints them: beside the agent, an `id`, the
# store's directory as a STRING, `at`, and for `changed` a `reason`. Until 2026-11-27 the
# wall could not read a line shaped so, and a helper stayed "processing" in the sidebar an
# hour after it was cleaned up.
ctl wall.sidebar left >/dev/null
ctl wall.sidebar mode group >/dev/null
W_17_BEFORE="$(wait_sidebar_rows "a:perf-probe@laptop-2=waiting")"
python3 - "$D" "$EXPORT_STREAM" changed <<'PY'
import json, sys
d_dir, stream, kind = sys.argv[1:4]
agent = {
    "name": "perf-probe", "host": "laptop-2", "kind": "termd", "run": 7, "backend": "claude",
    "session": "perf-probe", "namespace": "agents", "store": d_dir + "/store", "cwd": d_dir,
    "created_at": "2026-11-26T18:10:00Z", "live": True, "attached": True, "resumable": False,
    "status": "processing", "event": "PostToolUse", "detail": "Bash: tools/wall-perf.sh",
    "tool": "Bash", "status_at": "2026-11-26T18:31:00Z", "dialect": "claude",
}
line = {"type": "changed", "id": 101, "store": d_dir + "/store", "at": "2026-11-26T18:31:00Z",
        "reason": "status", "agent": agent}
with open(stream, "a", encoding="utf-8") as fh:
    fh.write(json.dumps(line) + "\n")
PY
W_17_CHANGED="$(wait_sidebar_rows "a:perf-probe@laptop-2=processing")"
ROWS_17_CHANGED="$(sb_tok "$W_17_CHANGED" rows)"
python3 - "$D" "$EXPORT_STREAM" <<'PY'
import json, sys
d_dir, stream = sys.argv[1:3]
agent = {
    "name": "perf-probe", "host": "laptop-2", "kind": "termd", "run": 7, "backend": "claude",
    "session": "perf-probe", "namespace": "agents", "store": d_dir + "/store", "cwd": d_dir,
    "created_at": "2026-11-26T18:10:00Z", "ended_at": "2026-11-26T18:32:00Z", "live": False,
    "attached": False, "resumable": False, "status": "ended", "end_reason": "quit",
    "dialect": "claude",
}
line = {"type": "removed", "id": 102, "store": d_dir + "/store", "at": "2026-11-26T18:32:00Z",
        "agent": agent}
with open(stream, "a", encoding="utf-8") as fh:
    fh.write(json.dumps(line) + "\n")
PY
ROWS_17_REMOVED=""
LEFT_17=0
for _ in $(seq 1 120); do
    ROWS_17_REMOVED="$(sb_tok "$(ctl app.where 2>/dev/null)" rows)"
    if [[ -n "$ROWS_17_REMOVED" && "$ROWS_17_REMOVED" != *"perf-probe"* ]]; then
        LEFT_17=1
        break
    fi
    sleep 0.05
done
# Clicking where it was does nothing now, and `wall.attach` says why.
R_17_ATTACH="$(ctl wall.attach perf-probe@laptop-2 2>&1)"
ctl wall.sidebar close >/dev/null
if [[ "$(sb_tok "$W_17_BEFORE" rows)" == *"a:perf-probe@laptop-2=waiting"* \
    && "$ROWS_17_CHANGED" == *"a:perf-probe@laptop-2=processing"* && "$LEFT_17" -eq 1 \
    && "$R_17_ATTACH" == *"perf-probe@laptop-2 is not an agent any more: it ended (quit) and was cleaned up"* ]]; then
    record_pass "a cleaned-up agent leaves the list" "perf-probe went waiting -> processing on a changed line, then off the list on its removed line; wall.attach: '${R_17_ATTACH}'"
else
    record_fail "a cleaned-up agent leaves the list" "before='$(sb_tok "$W_17_BEFORE" rows)' changed='${ROWS_17_CHANGED}' removed='${ROWS_17_REMOVED}' attach='${R_17_ATTACH}'"
fi

# ── 18. The fleet feed down: the list says when it was last seen ────────────
# The stub's `export` is killed and cannot come back while `feed-down` exists. The wall
# keeps the fleet it last read, heads the list with `agents: last seen HH:MM`, and dims
# every agent's row, so a frozen list never passes as live. With the feed back, the mark goes.
ctl wall.sidebar left >/dev/null
ctl wall.sidebar mode group >/dev/null
W_18_LIVE="$(wait_sidebar_rows "a:reviewer@laptop-2=ended")"
ROWS_18_LIVE="$(sb_tok "$W_18_LIVE" rows)"
STALE_18_LIVE="$(sb_tok "$W_18_LIVE" stale)"
touch "$D/data/feed-down"
EXPORT_PID="$(cat "$D/data/export.pid" 2>/dev/null || true)"
CLOCK_A="$(date +%H:%M)"
[[ -n "$EXPORT_PID" ]] && kill "$EXPORT_PID" 2>/dev/null
STALE_18_DOWN="none"
W_18_DOWN=""
for _ in $(seq 1 200); do
    W_18_DOWN="$(ctl app.where 2>/dev/null)" || { sleep 0.05; continue; }
    STALE_18_DOWN="$(sb_tok "$W_18_DOWN" stale)"
    [[ -n "$STALE_18_DOWN" && "$STALE_18_DOWN" != "none" ]] && break
    sleep 0.05
done
CLOCK_B="$(date +%H:%M)"
ROWS_18_DOWN="$(sb_tok "$W_18_DOWN" rows)"
BUS_18_DOWN="$(ctl wall.bus 2>&1 | head -1)"
ctl app.move "$PARK_X" "$PARK_Y" >/dev/null
ctl app.shot "$SHOT_DIR/sidebar-stale.png" >/dev/null 2>&1 || true
# The bus is back: the follower restarts the CLI after its backoff (5 s).
rm -f "$D/data/feed-down"
STALE_18_BACK="?"
for _ in $(seq 1 400); do
    STALE_18_BACK="$(sb_tok "$(ctl app.where 2>/dev/null)" stale)"
    [[ "$STALE_18_BACK" == "none" ]] && break
    sleep 0.05
done
BUS_18_BACK="$(ctl wall.bus 2>&1 | head -1)"
ctl wall.sidebar close >/dev/null
STALE_CHECK="$(python3 -c '
import sys
live_rows, live_stale, down_rows, down_stale, bus_down, back_stale, bus_back, clock_a, clock_b = sys.argv[1:10]
agents = [r for r in down_rows.split(";") if r.startswith("a:") or (r.startswith("t:") and "+" in r)]
clock, _, dimmed = down_stale.partition("/")
checks = {
    "live-unmarked": live_stale == "none",
    "marked": clock in (clock_a, clock_b),
    "dimmed-every-agent": dimmed == str(len(agents)) and len(agents) >= 3,
    "rows-kept": down_rows == live_rows and bool(live_rows),
    "wall.bus-says-so": bus_down.startswith(f"agents: last seen {clock} (the agents feed is down"),
    "back": back_stale == "none" and "last seen" not in bus_back,
}
bad = [name for name, ok in checks.items() if not ok]
if bad:
    print("0 failed=" + ",".join(bad) + f" | live stale={live_stale} | down stale={down_stale} (clock {clock_a}..{clock_b})"
          f" rows={down_rows} | bus={bus_down!r} | back stale={back_stale} bus={bus_back!r}")
else:
    print(f"1 feed killed: the list kept its {len(agents)} agents, dimmed, under \"agents: last seen {clock}\";"
          f" wall.bus: {bus_down!r}; feed back: mark gone")
' "$ROWS_18_LIVE" "$STALE_18_LIVE" "$ROWS_18_DOWN" "$STALE_18_DOWN" "$BUS_18_DOWN" "$STALE_18_BACK" "$BUS_18_BACK" "$CLOCK_A" "$CLOCK_B")"
if [[ "${STALE_CHECK%% *}" == "1" ]]; then
    record_pass "a dead feed marks the list stale" "${STALE_CHECK#* }"
else
    record_fail "a dead feed marks the list stale" "${STALE_CHECK#* }"
fi

# ── The wall can only run the stub `agent` ──────────────────────────────────
# Its fleet feed runs the `agent` it found first on PATH (the stub above), and since
# 2026-11-27 it hands that same binary to every attach terminal, whose own PATH (termd gives
# a terminal /usr/bin:/bin) would otherwise lead through the login shell to the real CLI.
AGENT_CLI="$(ctl app.where 2>/dev/null | sed -n 's/^agent_cli //p')"
if [[ "$AGENT_CLI" == "$D/bin/agent" ]]; then
    record_pass "only the stub agent CLI" "the wall's feed and its attach terminals run $AGENT_CLI"
else
    record_fail "only the stub agent CLI" "the wall would run '${AGENT_CLI:-nothing reported}', not the stub $D/bin/agent"
fi

ELAPSED="$(python3 -c "import time; print(f'{time.time() - $START_EPOCH:.1f}')")"
echo ""
echo "=== AGENT VIEW SMOKE SUMMARY: ${PASS_COUNT} passed, ${FAIL_COUNT} failed in ${ELAPSED}s ==="
echo "PNGs saved under: $SHOT_DIR"
[[ "$FAIL_COUNT" -eq 0 ]]
