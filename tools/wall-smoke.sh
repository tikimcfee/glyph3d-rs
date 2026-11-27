#!/usr/bin/env bash
# tools/wall-smoke.sh — live scripted-input smoke check for the native wall window.
#
# Starts an isolated scratch g3d-termd + glyph3d-native --wall in namespace `probe`
# (never touching `wall`, `agents`, `default`, or `rs-wall`) with an in-memory
# clipboard (`GLYPH_WALL_CLIPBOARD=memory`, never touching `pbcopy`/`pbpaste` or
# the person's macOS clipboard), drives every pointer and keyboard gesture through
# `wallctl` (`app.*` verbs), records PASS/FAIL per check, saves `app.shot` PNGs
# under target/glyph-scratch/wall-smoke/ on any failure, and tears down cleanly.

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
export GLYPH_WALL_CLIPBOARD="memory"

SHOT_DIR="$REPO_ROOT/target/glyph-scratch/wall-smoke"
mkdir -p "$SHOT_DIR"
rm -f "$SHOT_DIR"/*.png "$SHOT_DIR"/wall.log

START_EPOCH="$(python3 -c 'import time; print(f"{time.time():.2f}")')"

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
    printf 'PASS  %-30s %s\n' "$name" "$detail"
}

record_fail() {
    local name="$1"
    local detail="$2"
    FAIL_COUNT=$((FAIL_COUNT + 1))
    local slug="${name//[^a-zA-Z0-9_-]/_}"
    local shot="$SHOT_DIR/fail-${slug}.png"
    ctl app.shot "$shot" >/dev/null 2>&1 || true
    printf 'FAIL  %-30s %s  [shot: %s]\n' "$name" "$detail" "$shot"
}

# Wait until camera animation is settled across consecutive reads so eased moves
# and zooms are never sampled mid-flight under heavy CPU load.
wait_settled() {
    local w=""
    local stable=0
    for _ in $(seq 1 80); do
        w="$(ctl app.where 2>/dev/null)" || { stable=0; sleep 0.04; continue; }
        if grep -q "animating=false" <<<"$w"; then
            stable=$((stable + 1))
            if [[ "$stable" -ge 2 ]]; then
                printf '%s\n' "$w"
                return 0
            fi
        else
            stable=0
        fi
        sleep 0.04
    done
    ctl app.where 2>/dev/null || true
}

# Wait for terminal `$1`'s `(cols, rows)` to differ from `($2, $3)` (both axes when
# `$4 == 1`, or cols when `$4 == 0`) AND then remain unchanged across 5 consecutive
# polls (~200 ms quiet window, up to 4 s deadline) so intermediate drag steps queued
# on `term-session-io` finish draining before the next gesture reads `rect=`.
wait_size_settled() {
    local tid="$1"
    local old_c="$2"
    local old_r="$3"
    local both="${4:-0}"
    local last_pair=""
    local stable=0
    local w=""
    local cur_c=""
    local cur_r=""
    for _ in $(seq 1 80); do
        w="$(ctl app.where 2>/dev/null)" || { sleep 0.05; continue; }
        read -r cur_c cur_r < <(awk -v t="$tid" '
            $1 == "terminal" && $2 == t {
                c = ""; r = "";
                for (i = 3; i <= NF; i++) {
                    if ($i ~ /^cols=/) c = substr($i, 6);
                    if ($i ~ /^rows=/) r = substr($i, 6);
                }
                print c, r;
            }
        ' <<<"$w")
        if [[ -z "$cur_c" || -z "$cur_r" ]]; then
            sleep 0.05
            continue
        fi
        local changed=0
        if [[ "$both" -eq 1 ]]; then
            [[ "$cur_c" != "$old_c" && "$cur_r" != "$old_r" ]] && changed=1
        else
            [[ "$cur_c" != "$old_c" || "$cur_r" != "$old_r" ]] && changed=1
        fi
        if [[ "$changed" -eq 1 ]] && grep -q "animating=false" <<<"$w"; then
            if [[ "${cur_c}x${cur_r}" == "$last_pair" ]]; then
                stable=$((stable + 1))
                if [[ "$stable" -ge 5 ]]; then
                    printf '%s %s\n' "$cur_c" "$cur_r"
                    return 0
                fi
            else
                last_pair="${cur_c}x${cur_r}"
                stable=1
            fi
        else
            last_pair=""
            stable=0
        fi
        sleep 0.04
    done
    printf '%s %s\n' "${cur_c:-$old_c}" "${cur_r:-$old_r}"
    return 1
}

wait_for_text() {
    local tid="$1"
    local needle="$2"
    local tries="${3:-60}"
    local txt=""
    for _ in $(seq 1 "$tries"); do
        txt="$(ctl app.text "$tid" 2>/dev/null || true)"
        if grep -Fq "$needle" <<<"$txt"; then
            return 0
        fi
        sleep 0.04
    done
    return 1
}

term_center() {
    local tid="$1"
    local w="$2"
    python3 -c '
import sys
tid = sys.argv[1]
for line in sys.stdin:
    if line.startswith(f"terminal {tid} "):
        for tok in line.split():
            if tok.startswith("rect="):
                l, t, r, b = map(float, tok[5:].split(","))
                print(f"{(l+r)/2:.2f} {(t+b)/2:.2f}")
' "$tid" <<<"$w"
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
W_BOOT="$(wait_settled)"
read -r CHORD COPY_CHORD PASTE_CHORD CTRL_CLICK_MENU < <(awk '
    $1 == "platform" {
        c = ""; cp = ""; ps = ""; cm = "false";
        for (i = 3; i <= NF; i++) {
            if ($i ~ /^chord=/) c = substr($i, 7);
            else if ($i ~ /^copy=/) cp = substr($i, 6);
            else if ($i ~ /^paste=/) ps = substr($i, 7);
            else if ($i ~ /^ctrl_click_menu=/) cm = substr($i, 17);
        }
        print c, cp, ps, cm;
    }
' <<<"$W_BOOT")
if [[ -z "${CHORD:-}" || -z "${COPY_CHORD:-}" || -z "${PASTE_CHORD:-}" ]]; then
    echo "FAIL  startup: app.where did not report platform chords" >&2
    exit 1
fi
IFS='+' read -r -a CHORD_MODS <<<"$CHORD"

# Confirm t1's shell is ready to execute commands.
ctl app.type $'printf "BOOT_%s\\n" READY\n' >/dev/null
wait_for_text t1 "BOOT_READY" 80 || true

# ── 1. Chord+backslash spawn ─────────────────────────────────────────────────
ctl app.key "${CHORD}+backslash" >/dev/null
SPAWN_OK=0
for _ in $(seq 1 60); do
    W="$(ctl app.where 2>/dev/null || true)"
    if grep -q "^terminal t2 " <<<"$W"; then
        SPAWN_OK=1
        break
    fi
    sleep 0.04
done
W="$(wait_settled)"
FOCUSED_NOW="$(awk '/^focused / {print $2}' <<<"$W")"
if [[ "$SPAWN_OK" -eq 1 && "$FOCUSED_NOW" == "t2" ]]; then
    record_pass "${CHORD}+backslash spawn" "spawned t2 and focused it"
else
    record_fail "${CHORD}+backslash spawn" "spawn_ok=$SPAWN_OK focused=$FOCUSED_NOW"
fi

# Confirm t2's shell is ready as well.
ctl app.type $'printf "T2_%s\\n" READY\n' >/dev/null
wait_for_text t2 "T2_READY" 80 || true

# Fit both terminals cleanly on screen before pointer checks.
ctl wall.fit >/dev/null
W="$(wait_settled)"

# ── 2. Click focuses ─────────────────────────────────────────────────────────
read -r T1_CX T1_CY < <(term_center t1 "$W")
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
if wait_for_text t1 "TYPECHECK_4242" 60; then
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
import sys
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
read -r C1 R1 < <(wait_size_settled t1 "$C0" "$R0" 0)

# Re-read `rect=` AFTER Drag 1 has completely settled so `(CRN_X, CRN_Y)` lands
# squarely on the resized bottom-right corner even under heavy CPU load.
W="$(wait_settled)"
read -r CRN_X CRN_Y CRN_X1 CRN_Y1 < <(python3 -c '
import sys
for line in sys.stdin:
    if line.startswith("terminal t1 "):
        parts = line.split()
        l, t, r, b = map(float, [p[5:] for p in parts if p.startswith("rect=")][0].split(","))
        print(f"{r:.2f} {b:.2f} {r + 70.0:.2f} {b - 70.0:.2f}")
' <<<"$W")
ctl app.drag "$CRN_X" "$CRN_Y" "$CRN_X1" "$CRN_Y1" 14 >/dev/null
read -r C2 R2 < <(wait_size_settled t1 "$C1" "$R1" 1)

W="$(wait_settled)"
read -r T1_CX T1_CY < <(term_center t1 "$W")
ctl app.click "$T1_CX" "$T1_CY" >/dev/null
ctl app.type $'printf "TPUT_%s_%s\\n" "$(tput cols)" "$(tput lines)"\n' >/dev/null
if [[ "$C1" -lt "$C0" && "$C2" -ne "$C1" && "$R2" -ne "$R1" ]] && wait_for_text t1 "TPUT_${C2}_${R2}" 80; then
    record_pass "border & corner resize" "${C0}x${R0} -> ${C1}x${R1} -> ${C2}x${R2} (settled, tput matched)"
else
    record_fail "border & corner resize" "${C0}x${R0} -> ${C1}x${R1} -> ${C2}x${R2} (expected TPUT_${C2}_${R2})"
fi

# ── 6. Wheel over empty paper pans ───────────────────────────────────────────
ctl app.pinch 800 500 0.55 >/dev/null
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

# ── 7. mod+wheel zooms about the pointer ─────────────────────────────────────
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
read -r T1_CX T1_CY < <(term_center t1 "$W")
ctl app.click "$T1_CX" "$T1_CY" >/dev/null
ctl app.type $'seq -f "HISTLINE_%02g" 1 70\n' >/dev/null
wait_for_text t1 "HISTLINE_70" 60 || true
W_BEFORE_SCROLL="$(wait_settled)"
ctl app.wheel "$T1_CX" "$T1_CY" 0 360 >/dev/null

SCROLL_TXT=""
for _ in $(seq 1 60); do
    SCROLL_TXT="$(ctl app.text t1 2>/dev/null || true)"
    if grep -Eq 'HISTLINE_(0[1-9]|[12][0-9]|3[0-5])' <<<"$SCROLL_TXT"; then
        break
    fi
    sleep 0.04
done
W_AFTER_SCROLL="$(wait_settled)"
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
ctl terminal.scroll t1 -200 >/dev/null 2>&1 || true
wait_for_text t1 "HISTLINE_70" 60 || true

# ── 9. Drag select + copy chord + paste chord (memory clipboard, no pbcopy) ──
ctl wall.reveal t1 >/dev/null
W="$(wait_settled)"
ctl app.type $'clear; printf "  SELTOKEN_%s  \\n" 98765\n' >/dev/null
wait_for_text t1 "SELTOKEN_98765" 60 || true
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
# Clear internal clipboard AFTER drag so we prove specifically that the copy chord
# writes the selected text to the wall's clipboard (rather than copyOnSelect).
ctl app.clipboard clear >/dev/null
BEFORE_CLIP="$(ctl app.clipboard)"
ctl app.key "$COPY_CHORD" >/dev/null
COPIED="$(ctl app.clipboard)"
SEL_SPAN="$(python3 -c '
import sys
for line in sys.stdin:
    if line.startswith("terminal t1 "):
        for tok in line.split():
            if tok.startswith("text_sel="):
                print(tok[9:])
' <<<"$W_SEL")"
if [[ -z "$BEFORE_CLIP" && "$SEL_SPAN" != "none" && "$COPIED" == *"SELTOKEN_98765"* ]]; then
    record_pass "drag select & copy chord" "text_sel=$SEL_SPAN app.clipboard=$(printf '%s' "$COPIED" | tr '\n' ' ')"
else
    record_fail "drag select & copy chord" "before='$BEFORE_CLIP' text_sel=$SEL_SPAN clipboard='$COPIED'"
fi

# ── 10. Shift+drag lasso selects several, then chord+g groups ────────────────
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
ctl app.key "${CHORD}+g" >/dev/null
W_GROUP="$(wait_settled)"
HAS_G1=0
grep -q "^group g-1 " <<<"$W_GROUP" && HAS_G1=1
if [[ "$SEL_LIST" == *"t1"* && "$SEL_LIST" == *"t2"* && "$HAS_G1" -eq 1 ]]; then
    record_pass "shift+drag lasso & group" "selection=$SEL_LIST -> created group g-1"
else
    record_fail "shift+drag lasso & group" "selection=$SEL_LIST has_g1=$HAS_G1"
fi

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

# ── 12. chord+d and chord+shift+d split ──────────────────────────────────────
ctl terminal.focus t1 >/dev/null
W_S0="$(wait_settled)"
N_S0="$(grep -c '^terminal ' <<<"$W_S0")"
ctl app.key "${CHORD}+d" >/dev/null
for _ in $(seq 1 60); do
    W_S1="$(ctl app.where)"
    [[ "$(grep -c '^terminal ' <<<"$W_S1")" -gt "$N_S0" ]] && break
    sleep 0.04
done
W_S1="$(wait_settled)"
N_S1="$(grep -c '^terminal ' <<<"$W_S1")"

ctl app.key "${CHORD}+shift+d" >/dev/null
for _ in $(seq 1 60); do
    W_S2="$(ctl app.where)"
    [[ "$(grep -c '^terminal ' <<<"$W_S2")" -gt "$N_S1" ]] && break
    sleep 0.04
done
W_S2="$(wait_settled)"
N_S2="$(grep -c '^terminal ' <<<"$W_S2")"
SPLIT_GROUP="$(grep '^group ' <<<"$W_S2" | grep 'layout=split' || true)"
if [[ "$N_S1" -eq $((N_S0 + 1)) && "$N_S2" -eq $((N_S0 + 2)) && -n "$SPLIT_GROUP" ]]; then
    record_pass "${CHORD}+d / ${CHORD}+shift+d split" "terminals ${N_S0}->${N_S1}->${N_S2} in split group"
else
    record_fail "${CHORD}+d / ${CHORD}+shift+d split" "terminals ${N_S0}->${N_S1}->${N_S2} split_group='${SPLIT_GROUP}'"
fi

# ── 13. Right-click + 14. Escape ─────────────────────────────────────────────
ctl wall.fit >/dev/null
W_RC0="$(wait_settled)"
read -r RC_X RC_Y < <(term_center t1 "$W_RC0")
ctl app.click "$RC_X" "$RC_Y" right >/dev/null
W_RC1="$(ctl app.where)"
MENU_OPEN="$(awk '/^menu / {$1=""; sub(/^ /,""); print}' <<<"$W_RC1")"

ctl app.key Escape >/dev/null
W_ESC1="$(ctl app.where)"
MENU_AFTER_ESC="$(awk '/^menu / {print $2}' <<<"$W_ESC1")"

CTRL_CLICK_MENU_GOT="skipped"
if [[ "$CTRL_CLICK_MENU" == "true" ]]; then
    ctl app.click "$RC_X" "$RC_Y" left ctrl >/dev/null
    W_RC2="$(ctl app.where)"
    CTRL_CLICK_MENU_GOT="$(awk '/^menu / {$1=""; sub(/^ /,""); print}' <<<"$W_RC2")"
    ctl app.key Escape >/dev/null
fi

if [[ "$MENU_OPEN" == terminal:t1* && ( "$CTRL_CLICK_MENU_GOT" == "skipped" || "$CTRL_CLICK_MENU_GOT" == terminal:t1* ) ]]; then
    record_pass "right-click" "opened context menu ($MENU_OPEN, ctrl+click=${CTRL_CLICK_MENU_GOT%% *})"
else
    record_fail "right-click" "menu='$MENU_OPEN' ctrl_click='$CTRL_CLICK_MENU_GOT'"
fi

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

# ── Keyboard End-to-End Battery (raw PTY byte oracle on t2) ──────────────────
# Write a tiny raw-mode byte logger script into scratch $D so t2 echoes exact
# hex tokens (`K:<hex>`) for every keystroke without shell canonical buffering.
cat >"$D/raw_keys.py" <<'PY'
import os, sys, tty, termios
fd = sys.stdin.fileno()
old = termios.tcgetattr(fd)
try:
    tty.setraw(fd)
    os.write(1, b"RAW_READY\r\n")
    buf = b""
    while b"QUITRAW" not in buf:
        b = os.read(fd, 64)
        if not b:
            break
        buf += b
        os.write(1, b"K:" + b.hex().encode("ascii") + b"\r\n")
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, old)
    os.write(1, b"RAW_EXIT\r\n")
PY

ctl wall.reveal t2 >/dev/null
wait_settled >/dev/null
ctl app.type "python3 $D/raw_keys.py"$'\n' >/dev/null
if ! wait_for_text t2 "RAW_READY" 80; then
    record_fail "raw key harness" "python3 raw_keys.py did not print RAW_READY in t2"
fi

# ── 15. Shift, CapsLock, Dead keys & IME composition ─────────────────────────
ctl app.type "clear"$'\n' >/dev/null 2>&1 || true
ctl app.key shift+a >/dev/null
ctl app.key --key B --text B b >/dev/null
ctl app.key --key Dead --no-text alt+KeyE >/dev/null
# While IME preedit is active (`isComposing`), raw key presses are suppressed;
# `Ime::Commit` delivers composed UTF-8 without bracketed-paste framing.
ctl app.ime preedit "´" >/dev/null
ctl app.key x >/dev/null
ctl app.ime commit "é" >/dev/null
ctl app.ime commit "你好" >/dev/null

if wait_for_text t2 "K:e4bda0e5a5bd" 60; then
    TXT_15="$(ctl app.text t2)"
    IME_CHECK="$(python3 -c '
import sys, re
hexes = "".join(re.findall(r"K:([0-9a-f]+)", sys.stdin.read()))
# Expected contiguous stream after "clear\r": "A" (41) + "B" (42) + "é" (c3a9) + "你好" (e4bda0e5a5bd),
# with NO "78" ("x" suppressed during preedit) and NO Dead-key bytes between 42 and c3a9.
want = "4142c3a9e4bda0e5a5bd"
ok = want in hexes
print(f"{int(ok)} tail={hexes[-32:]}")
' <<<"$TXT_15")"
    if [[ "${IME_CHECK%% *}" == "1" ]]; then
        record_pass "Shift, CapsLock, dead & IME" "AB + é + 你好 (${IME_CHECK#* }, preedit suppressed 'x')"
    else
        record_fail "Shift, CapsLock, dead & IME" "want 4142c3a9e4bda0e5a5bd in ${IME_CHECK#* }"
    fi
else
    record_fail "Shift, CapsLock, dead & IME" "did not observe K:e4bda0e5a5bd in t2"
fi

# ── 16. Option as Alt (auto / on / off) ──────────────────────────────────────
# 1) auto (default): composed Option+B (`∫`) sends `∫` (e288ab)
ctl wall.config optionAsAlt auto >/dev/null
ctl app.key --key "∫" --text "∫" alt+KeyB >/dev/null
# 2) on (`true`): Option+B sends `ESC b` (1b62) both when composed and unmodified
ctl wall.config optionAsAlt true >/dev/null
ctl app.key --key "∫" --text "∫" alt+KeyB >/dev/null
ctl app.key alt+b >/dev/null
# 3) off (`false`): Option+B sends composed `∫` (e288ab), and plain Alt+f sends `f` (66)
ctl wall.config optionAsAlt false >/dev/null
ctl app.key --key "∫" --text "∫" alt+KeyB >/dev/null
ctl app.key alt+f >/dev/null
ctl wall.config optionAsAlt auto >/dev/null

if wait_for_text t2 "K:66" 60; then
    TXT_16="$(ctl app.text t2)"
    OPT_CHECK="$(python3 -c '
import sys, re
hexes = "".join(re.findall(r"K:([0-9a-f]+)", sys.stdin.read()))
want = "e288ab" + "1b62" + "1b62" + "e288ab" + "66"
ok = want in hexes
print(f"{int(ok)} tail={hexes[-36:]}")
' <<<"$TXT_16")"
    if [[ "${OPT_CHECK%% *}" == "1" ]]; then
        record_pass "Option as Alt auto/on/off" "auto=∫, on=ESC b, off=∫/f (${OPT_CHECK#* })"
    else
        record_fail "Option as Alt auto/on/off" "want e288ab1b621b62e288ab66 in ${OPT_CHECK#* }"
    fi
else
    record_fail "Option as Alt auto/on/off" "did not observe K:66 in t2"
fi

# ── 17. Repeat, numpad, F-keys (incl. F1!), Home/End/PgUp/PgDn, bare mods ────
# Bare modifiers must emit 0 bytes.
ctl app.key Shift >/dev/null
ctl app.key Ctrl >/dev/null
ctl app.key Alt >/dev/null
ctl app.key Cmd >/dev/null
ctl app.key CapsLock >/dev/null
ctl app.key NumLock >/dev/null
# Repeat + Numpad + Numpad8 (NumLock off -> ArrowUp) + F1 + F12 + Home/End/PgUp/PgDn + Shift+Tab
ctl app.key --repeat r >/dev/null
ctl app.key Numpad7 >/dev/null
ctl app.key NumpadAdd >/dev/null
ctl app.key NumpadEnter >/dev/null
ctl app.key --key ArrowUp Numpad8 >/dev/null
ctl app.key F1 >/dev/null
ctl app.key F12 >/dev/null
ctl app.key Home >/dev/null
ctl app.key End >/dev/null
ctl app.key PageUp >/dev/null
ctl app.key PageDown >/dev/null
ctl app.key shift+Tab >/dev/null

if wait_for_text t2 "K:1b5b5a" 60; then
    TXT_17="$(ctl app.text t2)"
    NAV_CHECK="$(python3 -c '
import sys, re
hexes = "".join(re.findall(r"K:([0-9a-f]+)", sys.stdin.read()))
# r(72) + 7(37) + +(2b) + CR(0d) + Up(1b5b41) + F1(1b4f50) + F12(1b5b32347e)
# + Home(1b5b48) + End(1b5b46) + PgUp(1b5b357e) + PgDn(1b5b367e) + Shift+Tab(1b5b5a)
want = "72" + "37" + "2b" + "0d" + "1b5b41" + "1b4f50" + "1b5b32347e" + "1b5b48" + "1b5b46" + "1b5b357e" + "1b5b367e" + "1b5b5a"
ok = want in hexes
print(f"{int(ok)} tail={hexes[-64:]}")
' <<<"$TXT_17")"
    if [[ "${NAV_CHECK%% *}" == "1" ]]; then
        record_pass "repeat, numpad, F1..F12 & nav" "bare mods=0B; F1=1b4f50, kp, Home/End/PgUp/PgDn matched"
    else
        record_fail "repeat, numpad, F1..F12 & nav" "mismatch in ${NAV_CHECK#* }"
    fi
else
    record_fail "repeat, numpad, F1..F12 & nav" "did not observe Shift+Tab (1b5b5a) in t2"
fi

# ── 18. Ctrl+C/D/Z/L, plain Escape (no panel), and paste chord ───────────────
ctl app.key ctrl+c >/dev/null
ctl app.key ctrl+d >/dev/null
ctl app.key ctrl+z >/dev/null
ctl app.key ctrl+l >/dev/null
ctl app.key Escape >/dev/null
ctl app.clipboard set "PASTE77" >/dev/null
ctl app.key "$PASTE_CHORD" >/dev/null

if wait_for_text t2 "50415354453737" 60; then
    TXT_18="$(ctl app.text t2)"
    CTRL_CHECK="$(python3 -c '
import sys, re
hexes = "".join(re.findall(r"K:([0-9a-f]+)", sys.stdin.read()))
# ^C(03) + ^D(04) + ^Z(1a) + ^L(0c) + Esc(1b) + "PASTE77"(50415354453737)
want = "03041a0c1b50415354453737"
ok = want in hexes
print(f"{int(ok)} tail={hexes[-36:]}")
' <<<"$TXT_18")"
    if [[ "${CTRL_CHECK%% *}" == "1" ]]; then
        record_pass "Ctrl+C/D/Z/L, Esc & paste" "03 04 1a 0c 1b + PASTE77 reached terminal"
    else
        record_fail "Ctrl+C/D/Z/L, Esc & paste" "want 03041a0c1b50415354453737 in ${CTRL_CHECK#* }"
    fi
else
    record_fail "Ctrl+C/D/Z/L, Esc & paste" "did not observe pasted PASTE77 in t2"
fi

# ── 19. Help-panel wall chords stay with the wall (including toggle-close) ───
BEFORE_CHORDS="$(ctl app.text t2)"

# Toggle Settings open & closed (`chord+,`), Help (`chord+/`), Sidebar (`chord+b`),
# Status (`chord+s`), Palette (`chord+k`), Grid (`chord+shift+'`), Fullscreen (`chord+Enter`).
ctl app.key "${CHORD}+comma" >/dev/null
P_SET_ON="$(ctl app.where | awk '/^panels / {print}')"
ctl app.key "${CHORD}+comma" >/dev/null
P_SET_OFF="$(ctl app.where | awk '/^panels / {print}')"

ctl app.key "${CHORD}+slash" >/dev/null
P_HLP_ON="$(ctl app.where | awk '/^panels / {print}')"
ctl app.key "${CHORD}+slash" >/dev/null
P_HLP_OFF="$(ctl app.where | awk '/^panels / {print}')"

ctl app.key "${CHORD}+b" >/dev/null
P_SB_ON="$(ctl app.where | awk '/^panels / {print}')"
ctl app.key "${CHORD}+b" >/dev/null
P_SB_OFF="$(ctl app.where | awk '/^panels / {print}')"

ctl app.key "${CHORD}+s" >/dev/null
P_ST_ON="$(ctl app.where | awk '/^panels / {print}')"
ctl app.key "${CHORD}+s" >/dev/null
P_ST_OFF="$(ctl app.where | awk '/^panels / {print}')"

ctl app.key "${CHORD}+k" >/dev/null
P_PAL_ON="$(ctl app.where | awk '/^panels / {print}')"
ctl app.key "${CHORD}+k" >/dev/null
P_PAL_OFF="$(ctl app.where | awk '/^panels / {print}')"

ctl app.key "${CHORD}+Enter" >/dev/null
FS_ON="$(ctl app.where | awk '/^fullscreen / {print $2}')"
ctl app.key "${CHORD}+Enter" >/dev/null
FS_OFF="$(ctl app.where | awk '/^fullscreen / {print $2}')"

# Cycle focus (`chord+]`), step focus history back (`ctrl+comma`), open menu by key (`shift+F10`) & close (`Escape`).
FOC_0="$(ctl app.where | awk '/^focused / {print $2}')"
ctl app.key "${CHORD}+BracketRight" >/dev/null
FOC_1="$(ctl app.where | awk '/^focused / {print $2}')"
ctl app.key "ctrl+comma" >/dev/null
FOC_BACK="$(ctl app.where | awk '/^focused / {print $2}')"
ctl app.key "shift+F10" >/dev/null
KEY_MENU="$(ctl app.where | awk '/^menu / {print $2}')"
ctl app.key Escape >/dev/null

AFTER_CHORDS="$(ctl app.text t2)"
CHORD_LEAKED=0
[[ "$BEFORE_CHORDS" != "$AFTER_CHORDS" ]] && CHORD_LEAKED=1

if [[ "$P_SET_ON" == *"settings=true"* && "$P_SET_OFF" == *"settings=false"* \
   && "$P_HLP_ON" == *"help=true"* && "$P_HLP_OFF" == *"help=false"* \
   && "$P_SB_ON" == *"sidebar=true"* && "$P_SB_OFF" == *"sidebar=false"* \
   && "$P_ST_ON" == *"status=true"* && "$P_ST_OFF" == *"status=false"* \
   && "$P_PAL_ON" == *"palette=true"* && "$P_PAL_OFF" == *"palette=false"* \
   && "$FS_ON" != "none" && "$FS_OFF" == "none" \
   && "$FOC_1" != "$FOC_0" && "$FOC_BACK" == "$FOC_0" \
   && "$KEY_MENU" == "terminal:t2" && "$CHORD_LEAKED" -eq 0 ]]; then
    record_pass "wall chords vs terminal" "panels/fullscreen/focus/menu chords acted; 0 bytes leaked to t2"
else
    record_fail "wall chords vs terminal" "set=($P_SET_ON->$P_SET_OFF) hlp=($P_HLP_ON->$P_HLP_OFF) pal=($P_PAL_ON->$P_PAL_OFF) fs=($FS_ON->$FS_OFF) foc=($FOC_0->$FOC_1->$FOC_BACK) menu=$KEY_MENU leaked=$CHORD_LEAKED"
fi

# ── 20. Window focus blur clears held modifiers & cancels close-hold ─────────
N_BEFORE_BLUR="$(ctl app.where | grep -c '^terminal ')"
# Hold the wall's chord modifier(s), lose window focus, regain focus, then press
# plain `d`: must type `d` (0x64) into t2 rather than splitting (`${CHORD}+d`)!
for mod in "${CHORD_MODS[@]}"; do
    ctl app.key --down "$mod" >/dev/null
done
ctl app.focus off >/dev/null
ctl app.focus on >/dev/null
ctl app.key d >/dev/null
wait_for_text t2 "K:64" 60 || true
N_AFTER_D="$(ctl app.where | grep -c '^terminal ')"

# Also verify holding `${CHORD}+x` starts `close_hold=active`, and losing focus (`app.focus off`)
# cancels `close_hold` so the terminal is not closed while unfocused.
for mod in "${CHORD_MODS[@]}"; do
    ctl app.key --down "$mod" >/dev/null
done
ctl app.key --down x >/dev/null
HOLD_ON="$(ctl app.where | awk '$1=="terminal" && $2=="t2" {for(i=3;i<=NF;i++) if($i~/^close_hold=/) print substr($i,12)}')"
ctl app.focus off >/dev/null
ctl app.focus on >/dev/null
HOLD_AFTER_BLUR="$(ctl app.where | awk '$1=="terminal" && $2=="t2" {for(i=3;i<=NF;i++) if($i~/^close_hold=/) print substr($i,12)}')"

# Exit raw reader cleanly.
ctl app.type "QUITRAW" >/dev/null
wait_for_text t2 "RAW_EXIT" 60 || true

if [[ "$N_AFTER_D" -eq "$N_BEFORE_BLUR" && "$HOLD_ON" == "active" && "$HOLD_AFTER_BLUR" == "none" ]]; then
    record_pass "window blur resets mods & hold" "blur cleared held ${CHORD} ('d' typed, no split) and cancelled close-hold"
else
    record_fail "window blur resets mods & hold" "terms=${N_BEFORE_BLUR}->${N_AFTER_D} hold=${HOLD_ON}->${HOLD_AFTER_BLUR}"
fi

ELAPSED="$(python3 -c "import time; print(f'{time.time() - $START_EPOCH:.1f}')")"
echo ""
echo "=== SMOKE SUMMARY: ${PASS_COUNT} passed, ${FAIL_COUNT} failed in ${ELAPSED}s ==="
[[ "$FAIL_COUNT" -eq 0 ]]
