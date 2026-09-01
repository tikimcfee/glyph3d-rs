#!/usr/bin/env python3
"""Oblique-pick repro driver (BEFORE/AFTER the pixel_ray fix).

Pass 1: --pick-file/--pick-row/--pick-col with GLYPH_PICK_DEBUG → the target
        record's id and world cell center on the z=0 page plane.
Pass 2: for each (yaw, pitch, D): --cam-pose aimed EXACTLY at that world
        point from that direction/distance, then --pick-px at the viewport
        center — the pixel the point projects to under the true projection.
        The pick must resolve the SAME record; pickdbg shows computed q and
        the world-space miss distance.

Usage: repro_pick_oblique.py <file_substr> <row> <col> [--focus] [--full]
"""
import math
import os
import re
import subprocess
import sys

BIN = "native/target/release/glyph3d-native"
REPO = os.environ.get("GLYPH_JS", "/Users/lugo/localdev/viz-web/glyph3d-js")
ENV = {**os.environ, "GLYPH_PICK_DEBUG": "1", "RUST_LOG": "warn"}

TARGET_RE = re.compile(
    r"pickdbg: target (\S+) rec=(\d+) local=\(([-\d.e]+),([-\d.e]+)\) adv=([-\d.e]+) h=([-\d.e]+) "
    r"off=\(([-\d.e]+),([-\d.e]+),([-\d.e]+)\) sc=\(([-\d.e]+),([-\d.e]+)\)"
    r" — world cell center \(([-\d.e]+),([-\d.e]+)\)"
)
DBG_PX_RE = re.compile(r"pickdbg: px \(([\d.]+),([\d.]+)\)(.*)")
DBG_Q_RE = re.compile(r"q=\(([-\d.e]+),([-\d.e]+)\) file=(\S+)")
DBG_NEAR_RE = re.compile(r"pickdbg: nearest rec=(\d+) .* dist_world=([-\d.e]+)")


def run(args):
    p = subprocess.run([BIN, *args], capture_output=True, text=True, env=ENV)
    return p.stdout + p.stderr


def regime(sub, row, col, focus, scenarios):
    tag = "focused (far=20000 arm)" if focus else "full-field (far≈1.68e6 arm)"
    print(f"── {tag} ──")
    base = ["--load-repo", REPO, "--screenshot", "out/pickdbg.png"]
    if focus:
        base += ["--focus-file", sub]
    out1 = run(base + ["--pick-file", sub, "--pick-row", str(row), "--pick-col", str(col)])
    m = TARGET_RE.search(out1)
    if not m:
        print(out1[-3000:])
        sys.exit(f"FATAL: no target line for {sub} row={row} col={col}")
    t = {
        "path": m.group(1), "rec": int(m.group(2)),
        "lx": float(m.group(3)), "ly": float(m.group(4)), "adv": float(m.group(5)),
        "wx": float(m.group(12)), "wy": float(m.group(13)),
    }
    print(
        f"target: {t['path']} rec={t['rec']} local=({t['lx']:.3f},{t['ly']:.3f}) "
        f"world=({t['wx']:.2f},{t['wy']:.2f})"
    )

    args = list(base)
    for yaw_d, pitch_d, dist in scenarios:
        yaw = math.radians(yaw_d)
        pitch = math.radians(pitch_d)
        fwd = (math.sin(yaw) * math.cos(pitch), math.sin(pitch), -math.cos(yaw) * math.cos(pitch))
        eye = (t["wx"] - fwd[0] * dist, t["wy"] - fwd[1] * dist, 0.0 - fwd[2] * dist)
        args += [
            "--cam-pose", f"{eye[0]:.4f}", f"{eye[1]:.4f}", f"{eye[2]:.4f}",
            f"{yaw_d:.4f}", f"{pitch_d:.4f}",
            "--pick-px", "800", "500",
        ]
    out2 = run(args)

    # Split into per-pick blocks: each ends at its "^pick: " line and carries
    # the pickdbg lines that preceded it.
    blocks = []
    cur = []
    for line in out2.splitlines():
        cur.append(line)
        if line.startswith("pick: "):
            blocks.append(cur)
            cur = []
    exp_qx = t["lx"] + t["adv"] * 0.5
    exp_qy = t["ly"]
    if len(blocks) != len(scenarios):
        print(f"WARNING: {len(blocks)} pick blocks for {len(scenarios)} scenarios")
    n_ok = 0
    for i, (yaw_d, pitch_d, dist) in enumerate(scenarios):
        blk = blocks[i] if i < len(blocks) else []
        text = "\n".join(blk)
        pick_line = next((l for l in blk if l.startswith("pick: ")), "???")
        q = DBG_Q_RE.search(text)
        nr = DBG_NEAR_RE.search(text)
        if "pick: MISS" in pick_line:
            why = "ray=None" if "pixel_ray returned None" in text else "no file AABB"
            print(f"  yaw={yaw_d:6.1f} pitch={pitch_d:6.1f} D={dist:8.1f} | MISS ({why})")
            continue
        m2 = re.search(r"rec=(\d+)", pick_line)
        if m2:
            got = int(m2.group(1))
            err = math.hypot(float(q.group(1)) - exp_qx, float(q.group(2)) - exp_qy) if q else float("nan")
            qs = f"q=({q.group(1)},{q.group(2)})" if q else "q=-"
            ok = got == t["rec"]
            n_ok += ok
            want = t["rec"]
            verdict = "OK" if ok else f"WRONG (want {want})"
            print(
                f"  yaw={yaw_d:6.1f} pitch={pitch_d:6.1f} D={dist:8.1f} | {qs} "
                f"aim=({exp_qx:.3f},{exp_qy:.3f}) err={err:9.4f} | rec={got} "
                f"| {verdict}"
            )
        else:
            dw = float(nr.group(2)) if nr else float("nan")
            print(
                f"  yaw={yaw_d:6.1f} pitch={pitch_d:6.1f} D={dist:8.1f} | FILE-ONLY "
                f"dist_world={dw:.4f} (want rec {t['rec']})"
            )
    print(f"  → {n_ok}/{len(scenarios)} resolved the exact record\n")


def main():
    sub, row, col = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    do_focus = "--focus" in sys.argv
    do_full = "--full" in sys.argv or not do_focus
    scenarios_focus = [
        (y, p, d)
        for (y, p) in [(0.0, 0.0), (35.0, -20.0), (60.0, -35.0)]
        for d in [20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0]
    ]
    scenarios_full = [
        (y, p, d)
        for (y, p) in [(0.0, 0.0), (35.0, -20.0)]
        for d in [50.0, 500.0, 2000.0]
    ]
    if do_focus:
        regime(sub, row, col, True, scenarios_focus)
    if do_full:
        regime(sub, row, col, False, scenarios_full)


if __name__ == "__main__":
    main()
