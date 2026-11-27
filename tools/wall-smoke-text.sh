#!/usr/bin/env bash
# tools/wall-smoke-text.sh — live selection-lifetime smoke check for the native wall window.
#
# Starts an isolated scratch g3d-termd + glyph3d-native --wall in namespace `probe`
# (G3D_TERMD_DIR=/tmp/g3d-root-$$; never touching `wall`, `agents`, `default`, or `rs-wall`),
# drives text selection via `wallctl app.drag`, feeds live PTY output through a FIFO so
# output arrives without keyboard input, and verifies:
#   1. drag selection highlights and Cmd+C copies the exact token
#   2. unrelated output on another row preserves the highlight
#   3. scrolling output shifts the highlight upward with the text (Cmd+C still copies it)
#   4. rewriting cells inside the highlighted span clears the highlight
#   5. resizing the terminal clears the highlight
#   6. typing into the focused terminal clears the highlight (`selectionClearOnTyping`)
#
# Saves `app.shot` PNGs under target/glyph-scratch/wall-smoke-text/ and tears down only
# the window PID started here plus the scratch daemon.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"
export PATH="${HOME}/.local/bin:${PATH}"

BIN="$REPO_ROOT/target/release/glyph3d-native"
WALLCTL="$REPO_ROOT/target/release/wallctl"
G3D="${HOME}/.local/bin/g3d"
NS="probe"

if [[ ! -x "$BIN" || ! -x "$WALLCTL" ]]; then
    echo "ERR: missing $BIN or $WALLCTL (build --release -p glyph3d-native -p wall-control first)" >&2
    exit 1
fi

D="/tmp/g3d-root-$$"
mkdir -m 700 "$D"
export G3D_TERMD_DIR="$D"

SHOT_DIR="$REPO_ROOT/target/glyph-scratch/wall-smoke-text"
mkdir -p "$SHOT_DIR"
rm -f "$SHOT_DIR"/*.png "$SHOT_DIR"/wall.log

FIFO="$D/pty.fifo"
mkfifo "$FIFO"
# Hold the write end open on fd 3 so `cat "$FIFO"` stays alive across writes.
exec 3<>"$FIFO"

CLIP_BACKUP="$D/clipboard.bak"
HAS_CLIP=0
if command -v pbpaste >/dev/null 2>&1 && command -v pbcopy >/dev/null 2>&1; then
    pbpaste >"$CLIP_BACKUP" 2>/dev/null || true
    HAS_CLIP=1
fi

WALL_PID=""
cleanup() {
    exec 3>&- 2>/dev/null || true
    if [[ "$HAS_CLIP" -eq 1 && -f "$CLIP_BACKUP" ]]; then
        pbcopy <"$CLIP_BACKUP" 2>/dev/null || true
    fi
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
    fi
    rm -rf "$D"
}
trap cleanup EXIT INT TERM

PASS_COUNT=0
FAIL_COUNT=0

ctl() {
    "$WALLCTL" --ns "$NS" "$@"
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

text_sel_of() {
    python3 -c '
import sys
tid = sys.argv[1]
for line in sys.stdin:
    if line.startswith(f"terminal {tid} "):
        for tok in line.split():
            if tok.startswith("text_sel="):
                print(tok.split("=", 1)[1])
                sys.exit(0)
print("missing")
' "$1"
}

# Compute logical screen coordinates `(x0, y, x1, y)` spanning cols `[c0, c1]` on 0-based `row` of `tid`.
cell_span_xy() {
    python3 -c '
import sys
tid, row, c0, c1 = sys.argv[1], int(sys.argv[2]), float(sys.argv[3]), float(sys.argv[4])
for line in sys.stdin:
    if line.startswith(f"terminal {tid} "):
        toks = dict(t.split("=", 1) for t in line.split()[2:] if "=" in t)
        cols = int(toks["cols"])
        rows = int(toks["rows"])
        l, t, r, b = map(float, toks["rect"].split(","))
        wl, wb, wr, wt = map(float, toks["world"].split(","))
        # Account for the 0.3 world-unit panel padding around the cell grid.
        pad_x = 0.3 * (r - l) / max(wr - wl, 1e-6)
        pad_y = 0.3 * (b - t) / max(wt - wb, 1e-6)
        cw = (r - l - 2.0 * pad_x) / cols
        ch = (b - t - 2.0 * pad_y) / rows
        x0 = l + pad_x + (c0 + 0.15) * cw
        x1 = l + pad_x + (c1 - 0.15) * cw
        y = t + pad_y + (row + 0.5) * ch
        print(f"{x0:.2f} {y:.2f} {x1:.2f} {y:.2f}")
        sys.exit(0)
' "$1" "$2" "$3" "$4"
}

wait_for_sel() {
    local tid="$1"
    local expected="$2"
    local tries="${3:-40}"
    local got=""
    for _ in $(seq 1 "$tries"); do
        got="$(ctl app.where 2>/dev/null | text_sel_of "$tid")"
        if [[ "$got" == "$expected" ]]; then
            printf '%s\n' "$got"
            return 0
        fi
        sleep 0.05
    done
    printf '%s\n' "$got"
    return 1
}

# Start scratch wall window in namespace `probe`.
"$BIN" --wall --wall-ns "$NS" >"$SHOT_DIR/wall.log" 2>&1 &
WALL_PID=$!

# Wait for control socket + initial terminal `t1`.
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

# Start a FIFO reader inside `t1` so PTY output arrives without sending keys to the wall.
ctl app.type "cat $FIFO" >/dev/null
ctl app.key enter >/dev/null
sleep 0.35
printf '\033[2J\033[H\033[6;1HPREFIX KEEP_TOKEN_42 SUFFIX\033[16;1HREADY_FIFO\r\n' >&3
if ! wait_for_text t1 "READY_FIFO" 80; then
    record_fail "fifo setup" "t1 never printed READY_FIFO"
    exit 1
fi
W="$(wait_settled)"

# ── 1. Drag select & copy ────────────────────────────────────────────────────
# `PREFIX KEEP_TOKEN_42 SUFFIX` sits on 0-based row 5; `KEEP_TOKEN_42` is cols 7..20.
read -r SX0 SY0 SX1 SY1 < <(cell_span_xy t1 5 7 20 <<<"$W")
ctl app.drag "$SX0" "$SY0" "$SX1" "$SY1" 8 >/dev/null
W="$(wait_settled)"
SEL1="$(text_sel_of t1 <<<"$W")"
ctl app.shot "$SHOT_DIR/01-selected.png" >/dev/null 2>&1 || true

CLIP1=""
if [[ "$HAS_CLIP" -eq 1 ]]; then
    printf '' | pbcopy
    ctl app.key cmd+c >/dev/null
    sleep 0.15
    CLIP1="$(pbpaste 2>/dev/null || true)"
fi

if [[ "$SEL1" == 5:*..5:* && ( "$HAS_CLIP" -eq 0 || "$CLIP1" == *"KEEP_TOKEN_42"* ) ]]; then
    record_pass "drag select & copy" "text_sel=$SEL1 clipboard=$CLIP1"
else
    record_fail "drag select & copy" "text_sel=$SEL1 clipboard=$CLIP1"
fi

# ── 2. Unrelated output preserves selection ──────────────────────────────────
printf '\033[20;1HUNRELATED_LINE_99\r\n' >&3
wait_for_text t1 "UNRELATED_LINE_99" 50 || true
sleep 0.15
SEL2="$(ctl app.where | text_sel_of t1)"
if [[ "$SEL2" == "$SEL1" && "$SEL2" != "none" ]]; then
    record_pass "unrelated output keeps selection" "text_sel=$SEL2"
else
    record_fail "unrelated output keeps selection" "before=$SEL1 after=$SEL2"
fi

# ── 3. Scrolling output shifts highlight upward with the text ────────────────
# Scroll the 30-row screen up by 3 lines: row 5 -> row 2.
printf '\033[30;1H\r\nSCROLL_1\r\nSCROLL_2\r\nSCROLL_3' >&3
wait_for_text t1 "SCROLL_3" 50 || true
EXPECTED_SHIFTED="${SEL1/#5:/2:}"
EXPECTED_SHIFTED="${EXPECTED_SHIFTED/..5:/..2:}"
SEL3="$(wait_for_sel t1 "$EXPECTED_SHIFTED" 40)" || true
ctl app.shot "$SHOT_DIR/02-scrolled.png" >/dev/null 2>&1 || true

CLIP3=""
if [[ "$HAS_CLIP" -eq 1 ]]; then
    printf '' | pbcopy
    ctl app.key cmd+c >/dev/null
    sleep 0.15
    CLIP3="$(pbpaste 2>/dev/null || true)"
fi

if [[ "$SEL3" == "$EXPECTED_SHIFTED" && ( "$HAS_CLIP" -eq 0 || "$CLIP3" == *"KEEP_TOKEN_42"* ) ]]; then
    record_pass "scroll shifts selection upward" "text_sel=$SEL1 -> $SEL3 clipboard=$CLIP3"
else
    record_fail "scroll shifts selection upward" "expected=$EXPECTED_SHIFTED got=$SEL3 clipboard=$CLIP3"
fi

# ── 4. Rewriting selected cells clears selection ─────────────────────────────
# 0-based row 2 is 1-based row 3; overwrite cols 9..14 right inside `KEEP_TOKEN_42`.
printf '\033[3;10HXXXXX' >&3
wait_for_text t1 "XXXXX" 50 || true
SEL4="$(wait_for_sel t1 "none" 40)" || true
ctl app.shot "$SHOT_DIR/03-cleared-rewrite.png" >/dev/null 2>&1 || true

if [[ "$SEL3" != "none" && "$SEL4" == "none" ]]; then
    record_pass "rewrite clears selection" "text_sel=$SEL3 -> none"
else
    record_fail "rewrite clears selection" "before=$SEL3 expected=none got=$SEL4"
fi

# ── 5. Resize clears selection ───────────────────────────────────────────────
printf '\033[2J\033[H\033[6;1HPREFIX RESIZE_TOKEN_77 SUFFIX\r\n' >&3
wait_for_text t1 "RESIZE_TOKEN_77" 50 || true
W="$(wait_settled)"
read -r RX0 RY0 RX1 RY1 < <(cell_span_xy t1 5 7 22 <<<"$W")
ctl app.drag "$RX0" "$RY0" "$RX1" "$RY1" 8 >/dev/null
SEL5_BEFORE="$(ctl app.where | text_sel_of t1)"
ctl terminal.resize t1 90 25 >/dev/null
RESIZED_OK=0
for _ in $(seq 1 40); do
    W="$(ctl app.where 2>/dev/null || true)"
    if grep -q "^terminal t1 cols=90 rows=25 " <<<"$W"; then
        RESIZED_OK=1
        break
    fi
    sleep 0.05
done
SEL5_AFTER="$(wait_for_sel t1 "none" 40)" || true
ctl app.shot "$SHOT_DIR/04-cleared-resize.png" >/dev/null 2>&1 || true

if [[ "$SEL5_BEFORE" != "none" && "$RESIZED_OK" -eq 1 && "$SEL5_AFTER" == "none" ]]; then
    record_pass "resize clears selection" "text_sel=$SEL5_BEFORE -> none (90x25)"
else
    record_fail "resize clears selection" "before=$SEL5_BEFORE resized=$RESIZED_OK after=$SEL5_AFTER"
fi

# ── 6. Typing clears selection (`selectionClearOnTyping`) ────────────────────
printf '\033[2J\033[H\033[6;1HPREFIX TYPE_TOKEN_88 SUFFIX\r\n' >&3
wait_for_text t1 "TYPE_TOKEN_88" 50 || true
ctl wall.fit >/dev/null
W="$(wait_settled)"
read -r TX0 TY0 TX1 TY1 < <(cell_span_xy t1 5 7 20 <<<"$W")
ctl app.drag "$TX0" "$TY0" "$TX1" "$TY1" 8 >/dev/null
SEL6_BEFORE="$(ctl app.where | text_sel_of t1)"
ctl app.type "z" >/dev/null
SEL6_AFTER="$(wait_for_sel t1 "none" 40)" || true
ctl app.shot "$SHOT_DIR/05-cleared-typing.png" >/dev/null 2>&1 || true

if [[ "$SEL6_BEFORE" != "none" && "$SEL6_AFTER" == "none" ]]; then
    record_pass "typing clears selection" "text_sel=$SEL6_BEFORE -> none"
else
    record_fail "typing clears selection" "before=$SEL6_BEFORE after=$SEL6_AFTER"
fi

echo ""
echo "=== TEXT SELECTION SMOKE SUMMARY: ${PASS_COUNT} passed, ${FAIL_COUNT} failed ==="
echo "PNGs saved under: $SHOT_DIR"
[[ "$FAIL_COUNT" -eq 0 ]]
