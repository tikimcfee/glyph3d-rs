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
#
# Saves `app.shot` PNGs under target/glyph-scratch/wall-smoke-agents/ and tears down only
# the window PID started here plus the scratch daemon.

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
            "attached": False,
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
            "cwd": "/home/lugo/glyph3d-rs",
            "created_at": "2026-11-26T17:50:00Z",
            "live": True,
            "attached": False,
            "status": "idle",
            "detail": "108 passed",
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
        exec tail -n +1 -f "$EXPORT_STREAM"
        ;;
    sessions)
        cat "$SESSIONS_JSON"
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

# ── 2. Open Agent View via `cmd+a` chord & normal-zoom card ──────────────────
ctl app.key cmd+a >/dev/null
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
    record_pass "cmd+a chord opens & pairs card" "paired sess-coder-1, hydrated $PAIRED_EXP expandable events, started --follow"
else
    record_fail "cmd+a chord opens & pairs card" "expanded=$PAIRED_EXP log=$(tr '\n' ';' <"$INVOCATIONS_LOG")"
fi

# ── 3. Close with `cmd+a`, reopen via hovered title-bar `≡` button ───────────
ctl app.key cmd+a >/dev/null
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

# ── 11. Bus snapshot in `wall.bus`, Sidebar (`cmd+b`), and Palette (`cmd+k`) ─
BUS_OUT="$(ctl wall.bus)"
ctl app.key cmd+b >/dev/null
wait_settled >/dev/null
sleep 0.10
maybe_shot "$SHOT_DIR/10-sidebar-live-agents.png"

ctl app.key cmd+k >/dev/null
sleep 0.12
maybe_shot "$SHOT_DIR/11-palette-live-agents.png"
ctl app.key Escape >/dev/null
ctl app.key cmd+b >/dev/null
wait_settled >/dev/null

if [[ "$WALL_SMOKE_SHOTS" == "1" ]]; then
    # Capture loose-canvas neighbour placement (`t2` spawned right of `t1` while `t1`'s Agent View is open).
    ctl wall.spawn >/dev/null
    sleep 0.25
    ctl wall.fit >/dev/null
    wait_settled >/dev/null
    maybe_shot "$SHOT_DIR/12-neighbour-overlap.png"
    ctl wall.close t2 >/dev/null 2>&1 || true
fi

if [[ "$BUS_OUT" == *"laptop-2"* && "$BUS_OUT" == *"coder"* && "$BUS_OUT" == *"reviewer"* && "$BUS_OUT" == *"workstation"* && "$BUS_OUT" == *"linux-gate"* ]]; then
    record_pass "bus snapshot in sidebar & palette" "hosts laptop-2 + workstation (4 live agents) populated"
else
    record_fail "bus snapshot in sidebar & palette" "wall.bus output: $(tr '\n' ' ' <<<"$BUS_OUT")"
fi

# ── 12. Extra namespace (`probe2` playing `agents`): `gx-coder` pair, detach & reattach ─
# Close `t1`'s Agent View first so `gx-coder` pairs cleanly on its own.
ctl wall.agent t1 close >/dev/null 2>&1 || true

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
    && ! grep -q "^terminal t2 " <<<"$W_AFTER_REATTACH"; then
    record_pass "extra-ns gx-coder detach & reattach" "mounted gx-coder, paired ($GX_PAIRED_EXP rows), detached cleanly, reattached via wall.attach without wrapper"
else
    record_fail "extra-ns gx-coder detach & reattach" "mounted=$GX_MOUNTED exp=$GX_PAIRED_EXP alive=$STILL_IN_PROBE2 reattached=$GX_REATTACHED attach_reply='$R_REATTACH'"
fi

ELAPSED="$(python3 -c "import time; print(f'{time.time() - $START_EPOCH:.1f}')")"
echo ""
echo "=== AGENT VIEW SMOKE SUMMARY: ${PASS_COUNT} passed, ${FAIL_COUNT} failed in ${ELAPSED}s ==="
echo "PNGs saved under: $SHOT_DIR"
[[ "$FAIL_COUNT" -eq 0 ]]
