#!/usr/bin/env bash
# tools/wall-smoke.sh — live scripted-input smoke check for the native wall window.
#
# Starts an isolated scratch g3d-termd + glyph3d-native --wall in namespace `probe`
# (never touching `wall`, `agents`, `default`, or `rs-wall`), drives every pointer
# and keyboard gesture through `wallctl` (`app.*` verbs), records PASS/FAIL per
# check, saves `app.shot` PNGs under target/glyph-scratch/wall-smoke/ on any failure,
# restores the system clipboard, and tears down both the window and scratch daemon.

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

D="/tmp/g3d-keys-$$"
mkdir -m 700 "$D"
export G3D_TERMD_DIR="$D"

SHOT_DIR="$REPO_ROOT/target/glyph-scratch/wall-smoke"
mkdir -p "$SHOT_DIR"
rm -f "$SHOT_DIR"/*.png "$SHOT_DIR"/wall.log

CLIP_BACKUP="$D/clipboard.bak"
HAS_CLIP=0
if command -v pbpaste >/dev/null 2>&1 && command -v pbcopy >/dev/null 2>&1; then
    pbpaste >"$CLIP_BACKUP" 2>/dev/null || true
    HAS_CLIP=1
fi

WALL_PID=""
cleanup() {
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
SUMMARY_LINES=()

ctl() {
    "$WALLCTL" --ns "$NS" "$@"
}

record_pass() {
    local name="$1"
    local detail="$2"
    PASS_COUNT=$((PASS_COUNT + 1))
    printf 'PASS  %-28s %s\n' "$name" "$detail"
    SUMMARY_LINES+=("PASS|$name|$detail")
}

record_fail() {
    local name="$1"
    local detail="$2"
    FAIL_COUNT=$((FAIL_COUNT + 1))
    local slug="${name//[^a-zA-Z0-9_-]/_}"
    local shot="$SHOT_DIR/fail-${slug}.png"
    ctl app.shot "$shot" >/dev/null 2>&1 || true
    printf 'FAIL  %-28s %s  [shot: %s]\n' "$name" "$detail" "$shot"
    SUMMARY_LINES+=("FAIL|$name|$detail")
}

wait_settled() {
    local w=""
    for _ in $(seq 1 50); do
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
    local tries="${3:-40}"
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

# Start scratch wall window in namespace `probe`.
"$BIN" --wall --wall-ns "$NS" >"$SHOT_DIR/wall.log" 2>&1 &
WALL_PID=$!

# Wait for control socket + initial terminal `t1` + shell readiness.
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
wait_settled >/dev/null

# Confirm t1's shell is ready to execute commands.
ctl app.type $'printf "BOOT_%s\\n" READY\n' >/dev/null
wait_for_text t1 "BOOT_READY" 60 || true

# ── 1. cmd+backslash spawn ───────────────────────────────────────────────────
ctl app.key cmd+backslash >/dev/null
SPAWN_OK=0
for _ in $(seq 1 40); do
    W="$(ctl app.where 2>/dev/null || true)"
    if grep -q "^terminal t2 " <<<"$W"; then
        SPAWN_OK=1
        break
    fi
    sleep 0.05
done
W="$(wait_settled)"
FOCUSED_NOW="$(awk '/^focused / {print $2}' <<<"$W")"
if [[ "$SPAWN_OK" -eq 1 && "$FOCUSED_NOW" == "t2" ]]; then
    record_pass "cmd+backslash spawn" "spawned t2 and focused it"
else
    record_fail "cmd+backslash spawn" "spawn_ok=$SPAWN_OK focused=$FOCUSED_NOW"
fi

# Fit both terminals cleanly on screen before pointer checks.
ctl wall.fit >/dev/null
W="$(wait_settled)"

# ── 2. Click focuses ─────────────────────────────────────────────────────────
read -r T1_CX T1_CY < <(python3 -c '
import sys
for line in sys.stdin:
    if line.startswith("terminal t1 "):
        for tok in line.split():
            if tok.startswith("rect="):
                l, t, r, b = map(float, tok[5:].split(","))
                print(f"{(l+r)/2:.2f} {(t+b)/2:.2f}")
' <<<"$W")
ctl app.click "$T1_CX" "$T1_CY" >/dev/null
W="$(wait_settled)"
CLICK_CHECK="$(python3 -c '
import sys
focused = ""
z = {}
for line in sys.stdin:
    if line.startswith("focused "):
        focused = line.split()[1]
    elif line.startswith("terminal "):
        parts = line.split()
        tid = parts[1]
        for tok in parts[2:]:
            if tok.startswith("z="):
                z[tid] = float(tok[2:])
ok = (focused == "t1") and (z.get("t1", -1) > z.get("t2", 999))
zt1, zt2 = z.get("t1"), z.get("t2")
print(f"{int(ok)} focused={focused} z_t1={zt1} z_t2={zt2}")
' <<<"$W")"
if [[ "${CLICK_CHECK%% *}" == "1" ]]; then
    record_pass "click focuses" "${CLICK_CHECK#* }"
else
    record_fail "click focuses" "${CLICK_CHECK#* }"
fi

# ── 3. Typing reaches the focused terminal ───────────────────────────────────
ctl app.type $'printf "TYPECHECK_%s\\n" 4242\n' >/dev/null
if wait_for_text t1 "TYPECHECK_4242" 40; then
    T2_TXT="$(ctl app.text t2 2>/dev/null || true)"
    if grep -Fq "TYPECHECK_4242" <<<"$T2_TXT"; then
        record_fail "typing reaches focused" "leaked into unfocused t2"
    else
        record_pass "typing reaches focused" "TYPECHECK_4242 in t1 only"
    fi
else
    record_fail "typing reaches focused" "TYPECHECK_4242 did not appear in t1"
fi

# ── 4. Title-bar drag moves and snaps ────────────────────────────────────────
W="$(wait_settled)"
read -r TB_X TB_Y WX0 WY0 < <(python3 -c '
import sys
bx = by = wx = wy = 0.0
for line in sys.stdin:
    if line.startswith("titlebar t1 "):
        for tok in line.split():
            if tok.startswith("rect="):
                l, t, r, b = map(float, tok[5:].split(","))
                bx, by = (l+r)/2, (t+b)/2
    elif line.startswith("terminal t1 "):
        for tok in line.split():
            if tok.startswith("world="):
                wx, wy, _, _ = map(float, tok[6:].split(","))
print(f"{bx:.2f} {by:.2f} {wx:.4f} {wy:.4f}")
' <<<"$W")
TB_X1="$(python3 -c "print(f'{$TB_X + 93.0:.2f}')")"
TB_Y1="$(python3 -c "print(f'{$TB_Y + 67.0:.2f}')")"
ctl app.drag "$TB_X" "$TB_Y" "$TB_X1" "$TB_Y1" 14 >/dev/null
W="$(wait_settled)"
MOVE_CHECK="$(python3 -c '
import sys, math
wx0, wy0 = float(sys.argv[1]), float(sys.argv[2])
wx1 = wy1 = 0.0
for line in sys.stdin:
    if line.startswith("terminal t1 "):
        for tok in line.split():
            if tok.startswith("world="):
                wx1, wy1, _, _ = map(float, tok[6:].split(","))
moved = (wx1 > wx0 + 0.5) and (wy1 < wy0 - 0.5)
rem_x = abs(wx1 - round(wx1 / 4.0) * 4.0)
snapped = rem_x < 0.02
ok = moved and snapped
print(f"{int(ok)} world=({wx0:.2f},{wy0:.2f})->({wx1:.2f},{wy1:.2f}) grid4_rem_x={rem_x:.4f}")
' "$WX0" "$WY0" <<<"$W")"
if [[ "${MOVE_CHECK%% *}" == "1" ]]; then
    record_pass "title-bar drag & snap" "${MOVE_CHECK#* }"
else
    record_fail "title-bar drag & snap" "${MOVE_CHECK#* }"
fi

# ── 5. Border & corner drag resizes in whole cells + termd re-wraps ──────────
ctl wall.fit >/dev/null
W="$(wait_settled)"
read -r C0 R0 EDGE_X EDGE_Y EDGE_X1 < <(python3 -c '
import sys
for line in sys.stdin:
    if line.startswith("terminal t1 "):
        parts = line.split()
        cols = int([p[5:] for p in parts if p.startswith("cols=")][0])
        rows = int([p[5:] for p in parts if p.startswith("rows=")][0])
        l, t, r, b = map(float, [p[5:] for p in parts if p.startswith("rect=")][0].split(","))
        print(f"{cols} {rows} {r:.2f} {(t+b)/2:.2f} {r - 110.0:.2f}")
' <<<"$W")
ctl app.drag "$EDGE_X" "$EDGE_Y" "$EDGE_X1" "$EDGE_Y" 14 >/dev/null
C1="$C0"
for _ in $(seq 1 40); do
    W="$(ctl app.where)"
    C1="$(python3 -c '
import sys
for line in sys.stdin:
    if line.startswith("terminal t1 "):
        print([p[5:] for p in line.split() if p.startswith("cols=")][0])
' <<<"$W")"
    [[ "$C1" != "$C0" ]] && break
    sleep 0.05
done

# Now drag bottom-right corner to change both cols and rows.
read -r R1 CRN_X CRN_Y CRN_X1 CRN_Y1 < <(python3 -c '
import sys
for line in sys.stdin:
    if line.startswith("terminal t1 "):
        parts = line.split()
        rows = int([p[5:] for p in parts if p.startswith("rows=")][0])
        l, t, r, b = map(float, [p[5:] for p in parts if p.startswith("rect=")][0].split(","))
        print(f"{rows} {r:.2f} {b:.2f} {r + 70.0:.2f} {b - 70.0:.2f}")
' <<<"$W")
ctl app.drag "$CRN_X" "$CRN_Y" "$CRN_X1" "$CRN_Y1" 14 >/dev/null
C2="$C1"
R2="$R1"
for _ in $(seq 1 40); do
    W="$(ctl app.where)"
    read -r C2 R2 < <(python3 -c '
import sys
for line in sys.stdin:
    if line.startswith("terminal t1 "):
        parts = line.split()
        c = [p[5:] for p in parts if p.startswith("cols=")][0]
        r = [p[5:] for p in parts if p.startswith("rows=")][0]
        print(f"{c} {r}")
' <<<"$W")
    [[ "$C2" != "$C1" && "$R2" != "$R1" ]] && break
    sleep 0.05
done

# Verify termd PTY size matches via `tput cols` and `tput lines`.
ctl app.click "$T1_CX" "$T1_CY" >/dev/null
ctl app.type $'printf "TPUT_%s_%s\\n" "$(tput cols)" "$(tput lines)"\n' >/dev/null
if [[ "$C1" -lt "$C0" && "$C2" -ne "$C1" && "$R2" -ne "$R1" ]] && wait_for_text t1 "TPUT_${C2}_${R2}" 40; then
    record_pass "border & corner resize" "${C0}x${R0} -> ${C1}x${R1} -> ${C2}x${R2} (tput matched)"
else
    record_fail "border & corner resize" "${C0}x${R0} -> ${C1}x${R1} -> ${C2}x${R2} (expected TPUT_${C2}_${R2})"
fi

# ── 6. Wheel over empty paper pans ───────────────────────────────────────────
ctl app.pinch 800 500 0.55 >/dev/null
sleep 0.22
W="$(wait_settled)"
read -r CAM_X0 CAM_Y0 CAM_Z0 < <(python3 -c '
import sys
for line in sys.stdin:
    if line.startswith("camera "):
        p = {k: v for k, v in (tok.split("=") for tok in line.split()[1:])}
        print(p["x"], p["y"], p["zoom"])
' <<<"$W")
ctl app.wheel 24 24 -120 -80 >/dev/null
W="$(wait_settled)"
PAN_CHECK="$(python3 -c '
import sys
x0, y0, z0 = map(float, sys.argv[1:4])
for line in sys.stdin:
    if line.startswith("camera "):
        p = {k: float(v) if k != "animating" else v for k, v in (tok.split("=") for tok in line.split()[1:])}
        px, py, pz = p["x"], p["y"], p["zoom"]
        dx, dy, dz = abs(px - x0), abs(py - y0), abs(pz - z0)
        ok = (dx > 0.5 and dy > 0.5 and dz < 1e-3)
        print(f"{int(ok)} cam=({x0:.2f},{y0:.2f})->({px:.2f},{py:.2f}) zoom={pz:.2f}")
' "$CAM_X0" "$CAM_Y0" "$CAM_Z0" <<<"$W")"
if [[ "${PAN_CHECK%% *}" == "1" ]]; then
    record_pass "wheel pans empty paper" "${PAN_CHECK#* }"
else
    record_fail "wheel pans empty paper" "${PAN_CHECK#* }"
fi

# ── 7. mod+wheel (and pinch) zooms about the pointer ─────────────────────────
sleep 0.22
W="$(wait_settled)"
ZX=380
ZY=270
ctl app.wheel "$ZX" "$ZY" 0 36 ctrl >/dev/null
W2="$(wait_settled)"
ZOOM_CHECK="$(python3 -c '
import sys, math
zx, zy = float(sys.argv[1]), float(sys.argv[2])
before, after = sys.argv[3], sys.argv[4]
def parse_cam(s):
    vw = vh = 0.0
    cx = cy = cz = 0.0
    for line in s.splitlines():
        if line.startswith("viewport "):
            p = dict(tok.split("=") for tok in line.split()[1:])
            vw, vh = float(p["w"]), float(p["h"])
        elif line.startswith("camera "):
            p = dict(tok.split("=") for tok in line.split()[1:])
            cx, cy, cz = float(p["x"]), float(p["y"]), float(p["zoom"])
    wx = cx + (zx - vw * 0.5) / cz
    wy = cy - (zy - vh * 0.5) / cz
    return cz, wx, wy
z0, wx0, wy0 = parse_cam(before)
z1, wx1, wy1 = parse_cam(after)
drift = math.hypot(wx1 - wx0, wy1 - wy0)
ok = (z1 > z0 * 1.02) and (drift < 0.05)
print(f"{int(ok)} zoom={z0:.3f}->{z1:.3f} anchor_drift={drift:.5f}")
' "$ZX" "$ZY" "$W" "$W2")"
if [[ "${ZOOM_CHECK%% *}" == "1" ]]; then
    record_pass "mod+wheel zooms at pointer" "${ZOOM_CHECK#* }"
else
    record_fail "mod+wheel zooms at pointer" "${ZOOM_CHECK#* }"
fi

# ── 8. Wheel over focused terminal scrolls history ───────────────────────────
ctl wall.fit >/dev/null
W="$(wait_settled)"
read -r T1_CX T1_CY < <(python3 -c '
import sys
for line in sys.stdin:
    if line.startswith("terminal t1 "):
        for tok in line.split():
            if tok.startswith("rect="):
                l, t, r, b = map(float, tok[5:].split(","))
                print(f"{(l+r)/2:.2f} {(t+b)/2:.2f}")
' <<<"$W")
ctl app.click "$T1_CX" "$T1_CY" >/dev/null
ctl app.type $'seq -f "HISTLINE_%02g" 1 70\n' >/dev/null
wait_for_text t1 "HISTLINE_70" 50 || true
sleep 0.25
W_BEFORE_SCROLL="$(wait_settled)"
ctl app.wheel "$T1_CX" "$T1_CY" 0 360 >/dev/null
sleep 0.25
W_AFTER_SCROLL="$(wait_settled)"
SCROLL_TXT="$(ctl app.text t1 2>/dev/null || true)"
SCROLL_CHECK="$(python3 -c '
import sys
b, a, txt = sys.argv[1], sys.argv[2], sys.argv[3]
def cam(s):
    for line in s.splitlines():
        if line.startswith("camera "):
            p = dict(tok.split("=") for tok in line.split()[1:])
            return float(p["x"]), float(p["y"]), float(p["zoom"])
    return 0.0, 0.0, 0.0
cx0, cy0, _ = cam(b)
cx1, cy1, _ = cam(a)
cam_still = abs(cx1 - cx0) < 1e-3 and abs(cy1 - cy0) < 1e-3
# After 70 lines on a ~25-30 row screen, live shows ~42..70; scrolling up brings lines <= 35 into view.
saw_history = any(f"HISTLINE_{n:02d}" in txt for n in range(1, 36))
ok = cam_still and saw_history
first_hist = next((line.strip() for line in txt.splitlines() if "HISTLINE_" in line), "none")
print(f"{int(ok)} cam_still={cam_still} top={first_hist}")
' "$W_BEFORE_SCROLL" "$W_AFTER_SCROLL" "$SCROLL_TXT")"
if [[ "${SCROLL_CHECK%% *}" == "1" ]]; then
    record_pass "wheel scrolls history" "${SCROLL_CHECK#* }"
else
    record_fail "wheel scrolls history" "${SCROLL_CHECK#* }"
fi
# Return terminal view to live bottom.
ctl terminal.scroll t1 -200 >/dev/null 2>&1 || true
sleep 0.15

# ── 9. Drag inside focused terminal selects text & copy chord copies ─────────
# Frame t1 first so row 0 and columns sit well clear of the 7 px border-grab band.
ctl wall.reveal t1 >/dev/null
W="$(wait_settled)"
ctl app.type $'clear; printf "  SELTOKEN_%s  \\n" 98765\n' >/dev/null
wait_for_text t1 "SELTOKEN_98765" 40 || true
sleep 0.10
W="$(wait_settled)"
read -r SX0 SY SX1 < <(python3 -c '
import sys
zoom = 20.0
for line in sys.stdin:
    if line.startswith("camera "):
        p = dict(tok.split("=") for tok in line.split()[1:])
        zoom = float(p["zoom"])
    elif line.startswith("terminal t1 "):
        parts = line.split()
        cols = int([p[5:] for p in parts if p.startswith("cols=")][0])
        rows = int([p[5:] for p in parts if p.startswith("rows=")][0])
        l, t, r, b = map(float, [p[5:] for p in parts if p.startswith("rect=")][0].split(","))
        pad = 0.3 * zoom
        cell_w = (r - l - 2.0 * pad) / max(1, cols)
        cell_h = (b - t - 2.0 * pad) / max(1, rows)
        x0 = l + max(pad + 1.2 * cell_w, 12.0)
        x1 = min(r - 12.0, l + pad + 22.0 * cell_w)
        y = t + max(pad + 0.5 * cell_h, 12.0)
        print(f"{x0:.2f} {y:.2f} {x1:.2f}")
' <<<"$W")
ctl app.drag "$SX0" "$SY" "$SX1" "$SY" 14 >/dev/null
W_SEL="$(ctl app.where)"
# Set sentinel AFTER drag so we prove specifically that the copy chord (`cmd+c`)
# writes the selected text to the OS clipboard (rather than copyOnSelect).
if [[ "$HAS_CLIP" -eq 1 ]]; then
    printf 'SENTINEL_BEFORE_COPY' | pbcopy
fi
ctl app.key cmd+c >/dev/null
sleep 0.10
COPIED=""
if [[ "$HAS_CLIP" -eq 1 ]]; then
    COPIED="$(pbpaste 2>/dev/null || true)"
    # Restore user clipboard right away (and again on EXIT).
    pbcopy <"$CLIP_BACKUP" 2>/dev/null || true
fi
SEL_SPAN="$(python3 -c '
import sys
for line in sys.stdin:
    if line.startswith("terminal t1 "):
        for tok in line.split():
            if tok.startswith("text_sel="):
                print(tok[9:])
' <<<"$W_SEL")"
if [[ "$SEL_SPAN" != "none" && "$COPIED" == *"SELTOKEN_98765"* ]]; then
    record_pass "drag select & copy chord" "text_sel=$SEL_SPAN clipboard=$(printf '%s' "$COPIED" | tr '\n' ' ')"
else
    record_fail "drag select & copy chord" "text_sel=$SEL_SPAN clipboard='$COPIED'"
fi

# ── 10. Shift+drag lasso selects several, then cmd+g groups ──────────────────
ctl wall.fit >/dev/null
W="$(wait_settled)"
read -r LX0 LY0 LX1 LY1 < <(python3 -c '
import sys
ls, ts, rs, bs = [], [], [], []
for line in sys.stdin:
    if line.startswith("terminal "):
        for tok in line.split():
            if tok.startswith("rect="):
                l, t, r, b = map(float, tok[5:].split(","))
                ls.append(l); ts.append(t); rs.append(r); bs.append(b)
print(f"{min(ls)-25.0:.2f} {min(ts)-35.0:.2f} {max(rs)+25.0:.2f} {max(bs)+25.0:.2f}")
' <<<"$W")
ctl app.drag "$LX0" "$LY0" "$LX1" "$LY1" 16 left shift >/dev/null
W_LASSO="$(wait_settled)"
SEL_LIST="$(awk '/^selection / {print $2}' <<<"$W_LASSO")"
ctl app.key cmd+g >/dev/null
W_GROUP="$(wait_settled)"
HAS_G1=0
grep -q "^group g-1 " <<<"$W_GROUP" && HAS_G1=1
if [[ "$SEL_LIST" == *"t1"* && "$SEL_LIST" == *"t2"* && "$HAS_G1" -eq 1 ]]; then
    record_pass "shift+drag lasso & group" "selection=$SEL_LIST -> created group g-1"
else
    record_fail "shift+drag lasso & group" "selection=$SEL_LIST has_g1=$HAS_G1"
fi

# Ungroup so t1 is standalone again for title-bar double-click & split checks.
ctl wall.ungroup g-1 >/dev/null 2>&1 || true
ctl wall.select none >/dev/null 2>&1 || true
ctl wall.fit >/dev/null
wait_settled >/dev/null

# ── 11. Double-click a title bar frames the terminal ─────────────────────────
ctl app.pinch 800 500 0.50 >/dev/null
W_PRE_DBL="$(wait_settled)"
read -r Z_PRE_DBL DB_X DB_Y < <(python3 -c '
import sys
z = bx = by = 0.0
for line in sys.stdin:
    if line.startswith("camera "):
        z = float(dict(tok.split("=") for tok in line.split()[1:])["zoom"])
    elif line.startswith("titlebar t1 "):
        for tok in line.split():
            if tok.startswith("rect="):
                l, t, r, b = map(float, tok[5:].split(","))
                bx, by = (l+r)/2, (t+b)/2
print(f"{z:.4f} {bx:.2f} {by:.2f}")
' <<<"$W_PRE_DBL")
ctl app.dblclick "$DB_X" "$DB_Y" >/dev/null
W_POST_DBL="$(wait_settled)"
DBL_CHECK="$(python3 -c '
import sys
z0 = float(sys.argv[1])
z1 = 0.0
for line in sys.stdin:
    if line.startswith("camera "):
        z1 = float(dict(tok.split("=") for tok in line.split()[1:])["zoom"])
ok = z1 > z0 * 1.15
print(f"{int(ok)} zoom={z0:.2f}->{z1:.2f}")
' "$Z_PRE_DBL" <<<"$W_POST_DBL")"
if [[ "${DBL_CHECK%% *}" == "1" ]]; then
    record_pass "double-click title bar" "${DBL_CHECK#* }"
else
    record_fail "double-click title bar" "${DBL_CHECK#* }"
fi

# ── 12. cmd+d and cmd+shift+d split ──────────────────────────────────────────
ctl terminal.focus t1 >/dev/null
W_S0="$(wait_settled)"
N_S0="$(grep -c '^terminal ' <<<"$W_S0")"
ctl app.key cmd+d >/dev/null
for _ in $(seq 1 40); do
    W_S1="$(ctl app.where)"
    [[ "$(grep -c '^terminal ' <<<"$W_S1")" -gt "$N_S0" ]] && break
    sleep 0.05
done
W_S1="$(wait_settled)"
N_S1="$(grep -c '^terminal ' <<<"$W_S1")"

ctl app.key cmd+shift+d >/dev/null
for _ in $(seq 1 40); do
    W_S2="$(ctl app.where)"
    [[ "$(grep -c '^terminal ' <<<"$W_S2")" -gt "$N_S1" ]] && break
    sleep 0.05
done
W_S2="$(wait_settled)"
N_S2="$(grep -c '^terminal ' <<<"$W_S2")"
SPLIT_GROUP="$(grep '^group ' <<<"$W_S2" | grep 'layout=split' || true)"
if [[ "$N_S1" -eq $((N_S0 + 1)) && "$N_S2" -eq $((N_S0 + 2)) && -n "$SPLIT_GROUP" ]]; then
    record_pass "cmd+d / cmd+shift+d split" "terminals ${N_S0}->${N_S1}->${N_S2} in split group"
else
    record_fail "cmd+d / cmd+shift+d split" "terminals ${N_S0}->${N_S1}->${N_S2} split_group='${SPLIT_GROUP}'"
fi

# ── 13. Right-click + 14. Escape ─────────────────────────────────────────────
ctl wall.fit >/dev/null
W_RC0="$(wait_settled)"
read -r RC_X RC_Y < <(python3 -c '
import sys
for line in sys.stdin:
    if line.startswith("terminal t1 "):
        for tok in line.split():
            if tok.startswith("rect="):
                l, t, r, b = map(float, tok[5:].split(","))
                print(f"{(l+r)/2:.2f} {(t+b)/2:.2f}")
' <<<"$W_RC0")
ctl app.click "$RC_X" "$RC_Y" right >/dev/null
W_RC1="$(ctl app.where)"
MENU_OPEN="$(awk '/^menu / {$1=""; sub(/^ /,""); print}' <<<"$W_RC1")"
ctl app.shot "$SHOT_DIR/right-click.png" >/dev/null 2>&1 || true

# Dismiss with Escape, then verify macOS Ctrl+left-click secondary press also opens menu.
ctl app.key Escape >/dev/null
W_ESC1="$(ctl app.where)"
MENU_AFTER_ESC="$(awk '/^menu / {print $2}' <<<"$W_ESC1")"

CTRL_CLICK_MENU="skipped"
if [[ "$(uname -s)" == "Darwin" ]]; then
    ctl app.click "$RC_X" "$RC_Y" left ctrl >/dev/null
    W_RC2="$(ctl app.where)"
    CTRL_CLICK_MENU="$(awk '/^menu / {$1=""; sub(/^ /,""); print}' <<<"$W_RC2")"
    ctl app.key Escape >/dev/null
fi

if [[ "$MENU_OPEN" == terminal:t1* && ( "$CTRL_CLICK_MENU" == "skipped" || "$CTRL_CLICK_MENU" == terminal:t1* ) ]]; then
    record_pass "right-click" "opened model menu ($MENU_OPEN, ctrl+click=${CTRL_CLICK_MENU%% *}); no egui popup mounted yet"
else
    record_fail "right-click" "menu='$MENU_OPEN' ctrl_click='$CTRL_CLICK_MENU'"
fi

# Escape clears wall selection when NO terminal holds keyboard focus (`PLAIN_KEYS`).
# Click empty paper at (18, 18) to blur focus, Shift-click t1 to select it without focusing,
# then press Escape and verify selection clears.
ctl app.click 18 18 >/dev/null
ctl app.click "$RC_X" "$RC_Y" left shift >/dev/null
W_SEL1="$(ctl app.where)"
FOCUS_BEFORE_ESC="$(awk '/^focused / {print $2}' <<<"$W_SEL1")"
SEL_BEFORE_ESC="$(awk '/^selection / {print $2}' <<<"$W_SEL1")"
ctl app.key Escape >/dev/null
W_ESC2="$(ctl app.where)"
SEL_AFTER_ESC="$(awk '/^selection / {print $2}' <<<"$W_ESC2")"

if [[ "$MENU_AFTER_ESC" == "none" && "$FOCUS_BEFORE_ESC" == "none" && "$SEL_BEFORE_ESC" == "t1" && "$SEL_AFTER_ESC" == "none" ]]; then
    record_pass "Escape" "closed context menu and cleared unfocused wall selection"
else
    record_fail "Escape" "menu_after=$MENU_AFTER_ESC focus=$FOCUS_BEFORE_ESC sel_before=$SEL_BEFORE_ESC sel_after=$SEL_AFTER_ESC"
fi

echo ""
echo "=== SMOKE SUMMARY: ${PASS_COUNT} passed, ${FAIL_COUNT} failed ==="
[[ "$FAIL_COUNT" -eq 0 ]]
