#!/usr/bin/env bash
# tools/wall-smoke-panels.sh — live scripted-input smoke check for all mounted wall panels.
#
# Starts an isolated scratch g3d-termd + glyph3d-native --wall in namespace `probe`
# under /tmp/g3d-menu-$$ (never touching `wall`, `agents`, `default`, or `rs-wall`,
# never touching the system clipboard, and killing only the window PID started here),
# drives every panel through `wallctl` (`app.*` pointer & keyboard verbs), prints a
# PASS/FAIL table, and saves failure PNGs under target/glyph-scratch/wall-smoke-panels/.

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

D="/tmp/g3d-menu-$$"
mkdir -m 700 -p "$D/bin"
export G3D_TERMD_DIR="$D"

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

SHOT_DIR="$REPO_ROOT/target/glyph-scratch/wall-smoke-panels"
mkdir -p "$SHOT_DIR"
rm -f "$SHOT_DIR"/*.png "$SHOT_DIR"/wall.log

if [[ "$(uname -s)" == "Darwin" ]]; then
    CHORD="cmd"
else
    CHORD="ctrl+alt"
fi

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
SUMMARY_LINES=()

ctl() {
    "$WALLCTL" --ns "$NS" "$@"
}

record_pass() {
    local name="$1"
    local detail="$2"
    PASS_COUNT=$((PASS_COUNT + 1))
    printf 'PASS  %-32s %s\n' "$name" "$detail"
    SUMMARY_LINES+=("PASS|$name|$detail")
}

record_fail() {
    local name="$1"
    local detail="$2"
    FAIL_COUNT=$((FAIL_COUNT + 1))
    local slug="${name//[^a-zA-Z0-9_-]/_}"
    local shot="$SHOT_DIR/fail-${slug}.png"
    ctl app.shot "$shot" >/dev/null 2>&1 || true
    printf 'FAIL  %-32s %s  [shot: %s]\n' "$name" "$detail" "$shot"
    SUMMARY_LINES+=("FAIL|$name|$detail")
}

wait_settled() {
    local w=""
    for _ in $(seq 1 50); do
        w="$(ctl app.where 2>/dev/null)" || { sleep 0.04; continue; }
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

# Extract `(cx, cy)` center of a `key=l,t,r,b` token on the line starting with `$1`.
rect_center() {
    local prefix="$1"
    local key="$2"
    local w="$3"
    python3 -c '
import sys
prefix, key = sys.argv[1], sys.argv[2]
for line in sys.stdin:
    if line.startswith(prefix):
        for tok in line.split():
            if tok.startswith(key + "="):
                val = tok[len(key) + 1:]
                if val != "none":
                    l, t, r, b = map(float, val.split(","))
                    print(f"{(l+r)/2:.2f} {(t+b)/2:.2f}")
                    sys.exit(0)
print("0 0")
' "$prefix" "$key" <<<"$w"
}

# Extract `(cx, cy)` of a named entry inside a `;`-joined map token (`items=split-right:l,t,r,b;...`).
map_entry_center() {
    local prefix="$1"
    local map_key="$2"
    local entry_id="$3"
    local w="$4"
    python3 -c '
import sys
prefix, map_key, entry_id = sys.argv[1], sys.argv[2], sys.argv[3]
for line in sys.stdin:
    if line.startswith(prefix):
        for tok in line.split():
            if tok.startswith(map_key + "="):
                raw = tok[len(map_key) + 1:]
                if raw == "none":
                    break
                for item in raw.split(";"):
                    k, coords = item.split(":", 1)
                    if k == entry_id:
                        l, t, r, b = map(float, coords.split(","))
                        print(f"{(l+r)/2:.2f} {(t+b)/2:.2f}")
                        sys.exit(0)
print("0 0")
' "$prefix" "$map_key" "$entry_id" <<<"$w"
}

# Start scratch wall window in namespace `probe` from `$D` so no repo-local launch_config
# overrides PATH discovery of the stub `agent`.
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
W="$(wait_settled)"

# Confirm t1's shell is ready.
ctl app.type $'printf "BOOT_%s\\n" READY\n' >/dev/null
wait_for_text t1 "BOOT_READY" 60 || true

# ── 1. Topbar: + terminal, fit all, find ─────────────────────────────────────
W="$(wait_settled)"
read -r SPAWN_X SPAWN_Y < <(rect_center "topbar " "spawn" "$W")
ctl app.click "$SPAWN_X" "$SPAWN_Y" >/dev/null
SPAWNED_T2=0
for _ in $(seq 1 40); do
    W="$(ctl app.where)"
    if grep -q "^terminal t2 " <<<"$W"; then
        SPAWNED_T2=1
        break
    fi
    sleep 0.04
done
W="$(wait_settled)"

ctl app.pinch 800 500 0.25 >/dev/null
W_ZOOMED_OUT="$(wait_settled)"
Z_OUT="$(python3 -c 'import sys; print([dict(t.split("=") for t in l.split()[1:])["zoom"] for l in sys.stdin if l.startswith("camera ")][0])' <<<"$W_ZOOMED_OUT")"
read -r FIT_X FIT_Y < <(rect_center "topbar " "fit" "$W_ZOOMED_OUT")
ctl app.click "$FIT_X" "$FIT_Y" >/dev/null
W_FIT="$(wait_settled)"
Z_FIT="$(python3 -c 'import sys; print([dict(t.split("=") for t in l.split()[1:])["zoom"] for l in sys.stdin if l.startswith("camera ")][0])' <<<"$W_FIT")"

read -r FIND_X FIND_Y < <(rect_center "topbar " "find" "$W_FIT")
ctl app.click "$FIND_X" "$FIND_Y" >/dev/null
W_FIND="$(ctl app.where)"
PAL_FROM_TOPBAR="$(grep '^palette ' <<<"$W_FIND" | grep -c 'open=true' || true)"
ctl app.key Escape >/dev/null
W_FIND_ESC="$(ctl app.where)"
PAL_CLOSED="$(grep '^palette ' <<<"$W_FIND_ESC" | grep -c 'open=false' || true)"

TOPBAR_OK="$(python3 -c 'import sys; s, z0, z1, po, pc = int(sys.argv[1]), float(sys.argv[2]), float(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5]); print(int(s == 1 and z1 > z0 * 1.3 and po == 1 and pc == 1))' "$SPAWNED_T2" "$Z_OUT" "$Z_FIT" "$PAL_FROM_TOPBAR" "$PAL_CLOSED")"
if [[ "$TOPBAR_OK" == "1" ]]; then
    record_pass "topbar (+ terminal, fit, find)" "spawned t2, fit zoom ${Z_OUT}->${Z_FIT}, find opened & Esc closed"
else
    record_fail "topbar (+ terminal, fit, find)" "spawned=$SPAWNED_T2 zoom=${Z_OUT}->${Z_FIT} pal_open=$PAL_FROM_TOPBAR pal_closed=$PAL_CLOSED"
fi

# ── 2. Palette: chord open, type filter, ArrowDown, Enter frames, Escape ─────
ctl wall.rename t1 alpha-one >/dev/null
ctl wall.rename t2 alpha-two >/dev/null
ctl terminal.focus t1 >/dev/null
ctl app.pinch 800 500 0.50 >/dev/null
W_PAL0="$(wait_settled)"
Z_PAL0="$(python3 -c 'import sys; print([dict(t.split("=") for t in l.split()[1:])["zoom"] for l in sys.stdin if l.startswith("camera ")][0])' <<<"$W_PAL0")"

ctl app.key "${CHORD}+k" >/dev/null
ctl app.type "alpha" >/dev/null
W_PAL_TYPED="$(ctl app.where)"
ctl app.key ArrowDown >/dev/null
W_PAL_DOWN="$(ctl app.where)"
ctl app.key Enter >/dev/null
W_PAL_ENTER="$(wait_settled)"

ctl app.key "${CHORD}+k" >/dev/null
W_PAL_REOPEN="$(ctl app.where)"
ctl app.key Escape >/dev/null
W_PAL_ESC="$(ctl app.where)"

PAL_CHECK="$(python3 -c '
import sys
z0 = float(sys.argv[1])
typed, down, entered, reopen, esc = sys.argv[2:7]
def pal_line(s):
    for l in s.splitlines():
        if l.startswith("palette "):
            return l
    return ""
p_typed = pal_line(typed)
p_down = pal_line(down)
p_ent = pal_line(entered)
p_re = pal_line(reopen)
p_esc = pal_line(esc)
foc = next((l.split()[1] for l in entered.splitlines() if l.startswith("focused ")), "")
z1 = next((float(dict(t.split("=") for t in l.split()[1:])["zoom"]) for l in entered.splitlines() if l.startswith("camera ")), 0.0)
ok = (
    "open=true" in p_typed and "\"alpha\"" in p_typed and "active=0" in p_typed
    and "active=1" in p_down
    and "open=false" in p_ent and foc == "t2" and z1 > z0 * 1.15
    and "open=true" in p_re and "open=false" in p_esc
)
print(f"{int(ok)} focused={foc} zoom={z0:.2f}->{z1:.2f} down=({p_down})")
' "$Z_PAL0" "$W_PAL_TYPED" "$W_PAL_DOWN" "$W_PAL_ENTER" "$W_PAL_REOPEN" "$W_PAL_ESC")"
if [[ "${PAL_CHECK%% *}" == "1" ]]; then
    record_pass "palette (chord, filter, Enter, Esc)" "${PAL_CHECK#* }"
else
    record_fail "palette (chord, filter, Enter, Esc)" "${PAL_CHECK#* }"
fi

# ── 3. Palette: verb entry runs ──────────────────────────────────────────────
ctl app.key "${CHORD}+k" >/dev/null
ctl app.type "hide the grid" >/dev/null
ctl app.key Enter >/dev/null
W_VERB="$(ctl app.where)"
ctl wall.grid on >/dev/null
if grep -q "^paper grid_show=false " <<<"$W_VERB" && grep -q "^palette open=false " <<<"$W_VERB"; then
    record_pass "palette (verb entry)" "'hide the grid' toggled grid_show=false and closed"
else
    record_fail "palette (verb entry)" "where=$(grep -E '^(paper|palette) ' <<<"$W_VERB" | tr '\n' ' ')"
fi

# ── 4. Keyboard routing: panel fields absorb typing; closing restores ────────
ctl terminal.focus t1 >/dev/null
wait_settled >/dev/null
ctl app.key "${CHORD}+k" >/dev/null
ctl app.type "LEAK_PALETTE_99" >/dev/null
ctl app.key Escape >/dev/null
ctl app.key Escape >/dev/null

ctl wall.settings open >/dev/null
ctl app.type "LEAK_SETTINGS_99" >/dev/null
ctl app.key Escape >/dev/null
ctl app.key Escape >/dev/null

ctl app.type $'printf "TERM_LIVE_%s\\n" 7373\n' >/dev/null
GOT_LIVE=0
wait_for_text t1 "TERM_LIVE_7373" 40 && GOT_LIVE=1
T1_SNAPSHOT="$(ctl app.text t1 2>/dev/null || true)"
if [[ "$GOT_LIVE" -eq 1 ]] && ! grep -Fq "LEAK_PALETTE_99" <<<"$T1_SNAPSHOT" && ! grep -Fq "LEAK_SETTINGS_99" <<<"$T1_SNAPSHOT"; then
    record_pass "keyboard (panel vs terminal)" "palette & settings absorbed typing; TERM_LIVE_7373 reached t1"
else
    record_fail "keyboard (panel vs terminal)" "got_live=$GOT_LIVE leaked_pal=$(grep -Fc LEAK_PALETTE_99 <<<"$T1_SNAPSHOT") leaked_set=$(grep -Fc LEAK_SETTINGS_99 <<<"$T1_SNAPSHOT")"
fi

# ── 5. Sidebar: row click, dock left/float/right, group collapse, group|host switch ─
ctl wall.group t1 t2 >/dev/null
ctl terminal.focus t2 >/dev/null
ctl wall.sidebar right >/dev/null
W_SB0="$(wait_settled)"

read -r ROW_T1_X ROW_T1_Y < <(map_entry_center "panel sidebar " "terms" "t1" "$W_SB0")
ctl app.click "$ROW_T1_X" "$ROW_T1_Y" >/dev/null
W_SB_CLICK="$(wait_settled)"
SB_FOC="$(awk '/^focused / {print $2}' <<<"$W_SB_CLICK")"

read -r DL_X DL_Y < <(rect_center "panel sidebar " "dock_left" "$W_SB_CLICK")
ctl app.click "$DL_X" "$DL_Y" >/dev/null
W_SB_LEFT="$(ctl app.where)"

read -r DF_X DF_Y < <(rect_center "panel sidebar " "dock_float" "$W_SB_LEFT")
ctl app.click "$DF_X" "$DF_Y" >/dev/null
W_SB_FLOAT="$(ctl app.where)"

read -r DR_X DR_Y < <(rect_center "panel sidebar " "dock_right" "$W_SB_FLOAT")
ctl app.click "$DR_X" "$DR_Y" >/dev/null
W_SB_RIGHT="$(ctl app.where)"

read -r FOLD_X FOLD_Y < <(map_entry_center "panel sidebar " "folds" "g-1" "$W_SB_RIGHT")
ctl app.click "$FOLD_X" "$FOLD_Y" >/dev/null
W_SB_FOLDED="$(ctl app.where)"

read -r UNFOLD_X UNFOLD_Y < <(map_entry_center "panel sidebar " "folds" "g-1" "$W_SB_FOLDED")
ctl app.click "$UNFOLD_X" "$UNFOLD_Y" >/dev/null
W_SB_UNFOLDED="$(ctl app.where)"

# The header's `host` half lists the same rows under this machine (the stub bus calls it
# `smoke`); the `group` half lists them under their group again.
read -r MH_X MH_Y < <(rect_center "panel sidebar " "mode_host" "$W_SB_UNFOLDED")
ctl app.click "$MH_X" "$MH_Y" >/dev/null
W_SB_BYHOST="$(ctl app.where)"
read -r MG_X MG_Y < <(rect_center "panel sidebar " "mode_group" "$W_SB_BYHOST")
ctl app.click "$MG_X" "$MG_Y" >/dev/null
W_SB_BYGROUP="$(ctl app.where)"

read -r DC_X DC_Y < <(rect_center "panel sidebar " "dock_close" "$W_SB_UNFOLDED")
ctl app.click "$DC_X" "$DC_Y" >/dev/null
W_SB_CLOSED="$(ctl app.where)"
ctl wall.ungroup g-1 >/dev/null 2>&1 || true
ctl wall.fit >/dev/null
wait_settled >/dev/null

SB_CHECK="$(python3 -c '
import sys
foc, l, f, r, folded, unfolded, by_host, by_group, closed = sys.argv[1:10]
def sb(s):
    return next((x for x in s.splitlines() if x.startswith("panel sidebar ")), "")
def tok(s, key):
    return next((t[len(key) + 1:] for t in sb(s).split() if t.startswith(key + "=")), "")
folded_hidden = "terms=none" in sb(folded)
unfolded_shown = "t1:" in sb(unfolded) and "t2:" in sb(unfolded)
is_closed = "open=false" in sb(closed)
# One list, drawn top to bottom: the group and its members; folded, its header alone;
# by host, the same rows under this machine.
rows_ok = (
    tok(unfolded, "rows") == "g:g-1[smoke=2];t:t1;t:t2"
    and tok(folded, "rows") == "g:g-1[smoke=2]"
    and tok(by_host, "rows") == "h:smoke[2];t:t1;t:t2"
    and tok(by_group, "rows") == "g:g-1[smoke=2];t:t1;t:t2"
)
modes = "->".join(tok(s, "mode") for s in (unfolded, by_host, by_group))
ok = (
    foc == "t1"
    and "side=left" in sb(l)
    and "side=float" in sb(f)
    and "side=right" in sb(r)
    and "collapsed=g-1" in sb(folded) and folded_hidden
    and "collapsed=none" in sb(unfolded) and unfolded_shown
    and rows_ok and modes == "group->host->group"
    and is_closed
)
print(f"{int(ok)} foc={foc} dock=left->float->right folded_hidden={folded_hidden} mode={modes} rows_ok={rows_ok} closed={is_closed}")
' "$SB_FOC" "$W_SB_LEFT" "$W_SB_FLOAT" "$W_SB_RIGHT" "$W_SB_FOLDED" "$W_SB_UNFOLDED" "$W_SB_BYHOST" "$W_SB_BYGROUP" "$W_SB_CLOSED")"
if [[ "${SB_CHECK%% *}" == "1" ]]; then
    record_pass "sidebar (row, dock, fold, mode)" "${SB_CHECK#* }"
else
    record_fail "sidebar (row, dock, fold, mode)" "${SB_CHECK#* }"
fi

# ── 6. Settings: search filter, boolean toggle + frame change, number field ──
ctl wall.fit >/dev/null
W_SET_PRE="$(wait_settled)"
SHOT_GRID_ON="$SHOT_DIR/grid-on.png"
SHOT_GRID_OFF="$SHOT_DIR/grid-off.png"
ctl app.shot "$SHOT_GRID_ON" >/dev/null

ctl wall.settings open >/dev/null
W_SET0="$(ctl app.where)"
ctl app.type "show grid" >/dev/null
W_SET_BOOL_FILTER="$(ctl app.where)"
read -r BOOL_X BOOL_Y < <(map_entry_center "panel settings " "controls" "gridShow" "$W_SET_BOOL_FILTER")
ctl app.click "$BOOL_X" "$BOOL_Y" >/dev/null
W_SET_GRID_OFF="$(ctl app.where)"
ctl app.shot "$SHOT_GRID_OFF" >/dev/null

# Restore show grid via the same switch, then clear search and edit `gridMinor` by typing.
ctl app.click "$BOOL_X" "$BOOL_Y" >/dev/null
ctl app.key Escape >/dev/null
ctl app.type "grid spacing" >/dev/null
W_SET_NUM_FILTER="$(ctl app.where)"
read -r NUM_X NUM_Y < <(map_entry_center "panel settings " "controls" "gridMinor" "$W_SET_NUM_FILTER")
ctl app.click "$NUM_X" "$NUM_Y" >/dev/null
ctl app.key End >/dev/null
ctl app.key Backspace >/dev/null
ctl app.key Backspace >/dev/null
ctl app.type $'8\n' >/dev/null
W_SET_NUM_DONE="$(ctl app.where)"

ctl wall.config gridMinor default >/dev/null
ctl wall.settings close >/dev/null
rm -f "$SHOT_GRID_ON.tmp" "$SHOT_GRID_OFF.tmp"

SET_CHECK="$(python3 -c '
import sys, struct, zlib

s0, s_bf, s_off, s_nf, s_nd, p_on, p_off = sys.argv[1:8]

def items_of(s):
    for l in s.splitlines():
        if l.startswith("panel settings "):
            for tok in l.split():
                if tok.startswith("items="):
                    return int(tok[6:])
    return -1

def patch_colors(path):
    data = open(path, "rb").read()
    pos = 8
    w = h = bpp = 0
    idat = bytearray()
    while pos < len(data):
        ln, ctype = struct.unpack(">I4s", data[pos:pos+8])
        chunk = data[pos+8:pos+8+ln]
        pos += 12 + ln
        if ctype == b"IHDR":
            w, h, bit_depth, color_type = struct.unpack(">IIBB", chunk[:10])
            bpp = {2: 3, 6: 4}[color_type]
        elif ctype == b"IDAT":
            idat.extend(chunk)
        elif ctype == b"IEND":
            break
    raw = zlib.decompress(bytes(idat))
    stride = w * bpp
    prev = bytearray(stride)
    sx, sy = w / 1600.0, h / 1000.0
    x0, y0, x1, y1 = int(16 * sx), int(56 * sy), int(96 * sx), int(136 * sy)
    seen = set()
    off = 0
    for y in range(h):
        ftype = raw[off]
        row = bytearray(raw[off+1:off+1+stride])
        off += 1 + stride
        if ftype == 1:
            for i in range(bpp, stride):
                row[i] = (row[i] + row[i - bpp]) & 0xff
        elif ftype == 2:
            for i in range(stride):
                row[i] = (row[i] + prev[i]) & 0xff
        elif ftype == 3:
            for i in range(stride):
                a = row[i - bpp] if i >= bpp else 0
                row[i] = (row[i] + ((a + prev[i]) >> 1)) & 0xff
        elif ftype == 4:
            for i in range(stride):
                a = row[i - bpp] if i >= bpp else 0
                b = prev[i]
                c = prev[i - bpp] if i >= bpp else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                row[i] = (row[i] + pr) & 0xff
        if y0 <= y < y1:
            for x in range(x0, x1):
                p = x * bpp
                seen.add((row[p], row[p+1], row[p+2]))
        prev = row
    return len(seen)

n_all, n_bf, n_nf = items_of(s0), items_of(s_bf), items_of(s_nf)
paper_off = "paper grid_show=false " in s_off
paper_minor8 = "grid_minor=8" in s_nd
c_on = patch_colors(p_on)
c_off = patch_colors(p_off)
frame_changed = (c_on > 1 and c_off == 1)
ok = (n_all > 15 and 0 < n_bf < n_all and 0 < n_nf < n_all and paper_off and frame_changed and paper_minor8)
print(f"{int(ok)} items={n_all}->{n_bf}/{n_nf} paper_colors={c_on}->{c_off} grid_minor8={paper_minor8}")
' "$W_SET0" "$W_SET_BOOL_FILTER" "$W_SET_GRID_OFF" "$W_SET_NUM_FILTER" "$W_SET_NUM_DONE" "$SHOT_GRID_ON" "$SHOT_GRID_OFF")"
rm -f "$SHOT_GRID_ON" "$SHOT_GRID_OFF"
if [[ "${SET_CHECK%% *}" == "1" ]]; then
    record_pass "settings (search, bool, number)" "${SET_CHECK#* }"
else
    record_fail "settings (search, bool, number)" "${SET_CHECK#* }"
fi

# ── 7. Status: opens from topbar button and shows both terminals ─────────────
W_ST0="$(ctl app.where)"
read -r ST_BTN_X ST_BTN_Y < <(rect_center "topbar " "status" "$W_ST0")
ctl app.click "$ST_BTN_X" "$ST_BTN_Y" >/dev/null
W_ST1="$(ctl app.where)"
ctl wall.status close >/dev/null
ST_LINE="$(grep '^panel status ' <<<"$W_ST1" || true)"
if [[ "$ST_LINE" == *"open=true"* && "$ST_LINE" == *"t1"* && "$ST_LINE" == *"t2"* ]]; then
    record_pass "status (topbar open, 2 terms)" "$ST_LINE"
else
    record_fail "status (topbar open, 2 terms)" "status_line='$ST_LINE'"
fi

# ── 8. Help: search filters ──────────────────────────────────────────────────
W_HP0="$(ctl app.where)"
read -r HP_BTN_X HP_BTN_Y < <(rect_center "topbar " "help" "$W_HP0")
ctl app.click "$HP_BTN_X" "$HP_BTN_Y" >/dev/null
W_HP1="$(ctl app.where)"
ctl app.type "split" >/dev/null
W_HP2="$(ctl app.where)"
ctl app.key Escape >/dev/null
ctl app.key Escape >/dev/null

HP_CHECK="$(python3 -c '
import sys
def rows_of(s):
    for l in s.splitlines():
        if l.startswith("panel help "):
            for tok in l.split():
                if tok.startswith("rows="):
                    return int(tok[5:])
    return -1
r1, r2 = rows_of(sys.argv[1]), rows_of(sys.argv[2])
ok = (r1 > 20 and 0 < r2 < r1)
print(f"{int(ok)} rows={r1}->{r2}")
' "$W_HP1" "$W_HP2")"
if [[ "${HP_CHECK%% *}" == "1" ]]; then
    record_pass "help (search filters)" "${HP_CHECK#* }"
else
    record_fail "help (search filters)" "${HP_CHECK#* }"
fi

# ── 9. Floating panels: non-overlapping defaults & header drag ───────────────
ctl wall.status float >/dev/null
ctl wall.status open >/dev/null
ctl wall.help float >/dev/null
ctl wall.help open >/dev/null
# Let StatusWindow settle its content-measured floating width before recording origins.
ctl app.where >/dev/null
W_FLT0="$(ctl app.where)"

read -r HD_X HD_Y < <(rect_center "panel help " "header" "$W_FLT0")
HD_X1="$(python3 -c "print(f'{$HD_X - 110.0:.2f}')")"
HD_Y1="$(python3 -c "print(f'{$HD_Y + 85.0:.2f}')")"
ctl app.drag "$HD_X" "$HD_Y" "$HD_X1" "$HD_Y1" 10 >/dev/null
W_FLT1="$(ctl app.where)"
ctl wall.help close >/dev/null
ctl wall.status close >/dev/null

FLT_CHECK="$(python3 -c '
import sys
before, after = sys.argv[1], sys.argv[2]
def get_rect(s, prefix):
    for l in s.splitlines():
        if l.startswith(prefix):
            for tok in l.split():
                if tok.startswith("rect="):
                    return tuple(map(float, tok[5:].split(",")))
    return (0.0, 0.0, 0.0, 0.0)
st_r = get_rect(before, "panel status ")
hp0 = get_rect(before, "panel help ")
hp1 = get_rect(after, "panel help ")
ox = max(0.0, min(st_r[2], hp0[2]) - max(st_r[0], hp0[0]))
oy = max(0.0, min(st_r[3], hp0[3]) - max(st_r[1], hp0[1]))
no_overlap = (ox * oy == 0.0) and (abs(st_r[0] - hp0[0]) > 20.0 or abs(st_r[1] - hp0[1]) > 20.0)
dx = hp1[0] - hp0[0]
dy = hp1[1] - hp0[1]
dragged = (-135.0 <= dx <= -85.0) and (65.0 <= dy <= 105.0)
ok = no_overlap and dragged
print(f"{int(ok)} status_x={st_r[0]:.0f}..{st_r[2]:.0f} help_x={hp0[0]:.0f}..{hp0[2]:.0f} drag_delta=({dx:.1f},{dy:.1f})")
' "$W_FLT0" "$W_FLT1")"
if [[ "${FLT_CHECK%% *}" == "1" ]]; then
    record_pass "floating panels (no stack & drag)" "${FLT_CHECK#* }"
else
    record_fail "floating panels (no stack & drag)" "${FLT_CHECK#* }"
fi

# ── 10. Pointer routing: panel absorbs click/wheel/drag; wall drag crosses ───
ctl wall.sidebar right >/dev/null
ctl wall.fit >/dev/null
ctl terminal.focus t1 >/dev/null
W_PR0="$(wait_settled)"

read -r SB_PX SB_PY SB_PY2 SB_LEFT < <(python3 -c '
import sys
for l in sys.stdin:
    if l.startswith("panel sidebar "):
        for tok in l.split():
            if tok.startswith("rect="):
                l0, t0, r0, b0 = map(float, tok[5:].split(","))
                print(f"{(l0+r0)/2:.2f} {b0 - 60.0:.2f} {b0 - 140.0:.2f} {l0:.2f}")
' <<<"$W_PR0")

ctl app.click "$SB_PX" "$SB_PY" >/dev/null
ctl app.wheel "$SB_PX" "$SB_PY" -90 180 >/dev/null
ctl app.drag "$SB_PX" "$SB_PY" "$SB_PX" "$SB_PY2" 8 >/dev/null
W_PR_PANEL="$(wait_settled)"

# Now start a title-bar drag on t1 on the canvas and drag right across the docked sidebar.
read -r TB1_X TB1_Y T1_WX0 < <(python3 -c '
import sys
bx = by = wx = 0.0
for l in sys.stdin:
    if l.startswith("titlebar t1 "):
        for tok in l.split():
            if tok.startswith("rect="):
                l0, t0, r0, b0 = map(float, tok[5:].split(","))
                bx, by = (l0 + r0) * 0.5, (t0 + b0) * 0.5
    elif l.startswith("terminal t1 "):
        for tok in l.split():
            if tok.startswith("world="):
                wx = float(tok[6:].split(",")[0])
print(f"{bx:.2f} {by:.2f} {wx:.4f}")
' <<<"$W_PR_PANEL")
CROSS_X1="$(python3 -c "print(f'{$SB_LEFT + 70.0:.2f}')")"
ctl app.drag "$TB1_X" "$TB1_Y" "$CROSS_X1" "$TB1_Y" 14 >/dev/null
W_PR_CROSS="$(wait_settled)"
ctl wall.sidebar close >/dev/null
ctl wall.fit >/dev/null
wait_settled >/dev/null

PR_CHECK="$(python3 -c '
import sys
w0, wp, wc, wx0 = sys.argv[1], sys.argv[2], sys.argv[3], float(sys.argv[4])
def cam_foc(s):
    cam = foc = ""
    for l in s.splitlines():
        if l.startswith("camera "):
            p = dict(t.split("=") for t in l.split()[1:])
            cam = (float(p["x"]), float(p["y"]), float(p["zoom"]))
        elif l.startswith("focused "):
            foc = l.split()[1]
    return cam, foc
c0, f0 = cam_foc(w0)
cp, fp = cam_foc(wp)
panel_absorbed = (f0 == "t1" and fp == "t1" and all(abs(a - b) < 1e-3 for a, b in zip(c0, cp)))
wx1 = next((float([t[6:] for t in l.split() if t.startswith("world=")][0].split(",")[0]) for l in wc.splitlines() if l.startswith("terminal t1 ")), wx0)
crossed = (wx1 > wx0 + 2.0)
ok = panel_absorbed and crossed
print(f"{int(ok)} cam_untouched={panel_absorbed} t1_world_x={wx0:.2f}->{wx1:.2f}")
' "$W_PR0" "$W_PR_PANEL" "$W_PR_CROSS" "$T1_WX0")"
if [[ "${PR_CHECK%% *}" == "1" ]]; then
    record_pass "pointer routing (panel vs canvas)" "${PR_CHECK#* }"
else
    record_fail "pointer routing (panel vs canvas)" "${PR_CHECK#* }"
fi

# ── 11. Context menu: right-click split right, canvas menu, hold-to-close ────
W_CM0="$(wait_settled)"
read -r T1_CX T1_CY < <(rect_center "terminal t1 " "rect" "$W_CM0")
ctl app.click "$T1_CX" "$T1_CY" right >/dev/null
W_CM_OPEN="$(ctl app.where)"
read -r SPLIT_X SPLIT_Y < <(map_entry_center "context_menu " "items" "split-right" "$W_CM_OPEN")
ctl app.click "$SPLIT_X" "$SPLIT_Y" >/dev/null

SPLIT_APPEARED=0
NEW_TID=""
for _ in $(seq 1 40); do
    W_CM_SPLIT="$(ctl app.where)"
    if [[ "$(grep -c '^terminal ' <<<"$W_CM_SPLIT")" -ge 3 ]]; then
        NEW_TID="$(awk '/^terminal / && $2 != "t1" && $2 != "t2" {print $2; exit}' <<<"$W_CM_SPLIT")"
        SPLIT_APPEARED=1
        break
    fi
    sleep 0.04
done
W_CM_SPLIT="$(wait_settled)"

# Right-click empty canvas at bottom-left corner (clear of fitted panes).
ctl app.click 18 960 right >/dev/null
W_CM_CANVAS="$(ctl app.where)"
CANVAS_MENU_OK=0
if grep -q "^menu canvas " <<<"$W_CM_CANVAS" && grep -q "^context_menu open=true .*spawn:" <<<"$W_CM_CANVAS"; then
    CANVAS_MENU_OK=1
fi
ctl app.key Escape >/dev/null

# Hold-to-close on `$NEW_TID`: short press must NOT close; held press past close_hold_ms MUST close.
SHORT_SURVIVED=0
HOLD_CLOSED=0
if [[ -n "$NEW_TID" ]]; then
    W_PRE_HOLD="$(wait_settled)"
    read -r NT_CX NT_CY < <(rect_center "terminal ${NEW_TID} " "rect" "$W_PRE_HOLD")
    ctl app.click "$NT_CX" "$NT_CY" right >/dev/null
    W_NT_MENU="$(ctl app.where)"
    read -r CM_CLOSE_X CM_CLOSE_Y < <(map_entry_center "context_menu " "items" "close" "$W_NT_MENU")

    # Short press (< close_hold_ms): immediate down/up without extra sleep jitter under load.
    ctl app.move "$CM_CLOSE_X" "$CM_CLOSE_Y" >/dev/null
    ctl app.down left >/dev/null
    ctl app.up left >/dev/null
    W_AFTER_SHORT="$(ctl app.where)"
    grep -q "^terminal ${NEW_TID} " <<<"$W_AFTER_SHORT" && SHORT_SURVIVED=1

    # Full hold (> close_hold_ms = 650ms)
    ctl app.move "$CM_CLOSE_X" "$CM_CLOSE_Y" >/dev/null
    ctl app.down left >/dev/null
    sleep 0.78
    ctl app.up left >/dev/null
    for _ in $(seq 1 30); do
        W_AFTER_HOLD="$(ctl app.where)"
        if ! grep -q "^terminal ${NEW_TID} " <<<"$W_AFTER_HOLD"; then
            HOLD_CLOSED=1
            break
        fi
        sleep 0.04
    done
fi

if [[ "$SPLIT_APPEARED" -eq 1 && "$CANVAS_MENU_OK" -eq 1 && "$SHORT_SURVIVED" -eq 1 && "$HOLD_CLOSED" -eq 1 ]]; then
    record_pass "context menu (split, canvas, hold)" "split spawned $NEW_TID, canvas menu ok, short kept & hold closed"
else
    record_fail "context menu (split, canvas, hold)" "split=$SPLIT_APPEARED($NEW_TID) canvas=$CANVAS_MENU_OK short_kept=$SHORT_SURVIVED hold_closed=$HOLD_CLOSED"
fi

# ── 12. Title-bar buttons: hover shows, hide hides, hold-to-close kills ──────
ctl wall.ungroup g-1 >/dev/null 2>&1 || true
# Frame t2 so its title bar is well above `bar_buttons_min_px` (14 px).
ctl wall.reveal t2 >/dev/null
W_BB0="$(wait_settled)"
read -r TB2_X TB2_Y < <(rect_center "titlebar t2 " "rect" "$W_BB0")
ctl app.move "$TB2_X" "$TB2_Y" >/dev/null
W_BB_HOV="$(ctl app.where)"
HOVER_SHOWN=0
grep -q "^bar_buttons terminal=t2 " <<<"$W_BB_HOV" && HOVER_SHOWN=1

read -r BB_HIDE_X BB_HIDE_Y < <(rect_center "bar_buttons " "hide" "$W_BB_HOV")
ctl app.click "$BB_HIDE_X" "$BB_HIDE_Y" >/dev/null
W_BB_HIDDEN="$(wait_settled)"
WAS_HIDDEN=0
! grep -q "^terminal t2 " <<<"$W_BB_HIDDEN" && WAS_HIDDEN=1

ctl wall.reveal t2 >/dev/null
W_BB_REV="$(wait_settled)"
read -r TB2_X2 TB2_Y2 < <(rect_center "titlebar t2 " "rect" "$W_BB_REV")
ctl app.move "$TB2_X2" "$TB2_Y2" >/dev/null
W_BB_HOV2="$(ctl app.where)"
read -r BB_CLOSE_X BB_CLOSE_Y < <(rect_center "bar_buttons " "close" "$W_BB_HOV2")

# Short press on title-bar `×` must NOT kill t2.
ctl app.move "$BB_CLOSE_X" "$BB_CLOSE_Y" >/dev/null
ctl app.down left >/dev/null
ctl app.up left >/dev/null
W_BB_SHORT="$(ctl app.where)"
BB_SHORT_KEPT=0
grep -q "^terminal t2 " <<<"$W_BB_SHORT" && BB_SHORT_KEPT=1

# Full hold past close_hold_ms kills t2's shell.
ctl app.move "$BB_CLOSE_X" "$BB_CLOSE_Y" >/dev/null
ctl app.down left >/dev/null
sleep 0.78
ctl app.up left >/dev/null
BB_HOLD_KILLED=0
for _ in $(seq 1 30); do
    LIST_OUT="$(ctl wall.list 2>/dev/null || true)"
    if ! grep -qw "t2" <<<"$LIST_OUT"; then
        BB_HOLD_KILLED=1
        break
    fi
    sleep 0.04
done

if [[ "$HOVER_SHOWN" -eq 1 && "$WAS_HIDDEN" -eq 1 && "$BB_SHORT_KEPT" -eq 1 && "$BB_HOLD_KILLED" -eq 1 ]]; then
    record_pass "bar buttons (hover, hide, hold)" "hover showed buttons, hide hid t2, short kept & hold killed t2"
else
    record_fail "bar buttons (hover, hide, hold)" "hover=$HOVER_SHOWN hidden=$WAS_HIDDEN short_kept=$BB_SHORT_KEPT hold_killed=$BB_HOLD_KILLED"
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

echo ""
echo "=== PANEL SMOKE SUMMARY: ${PASS_COUNT} passed, ${FAIL_COUNT} failed ==="
[[ "$FAIL_COUNT" -eq 0 ]]
