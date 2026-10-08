#!/usr/bin/env python3
"""
bench_hyper.py — Rigorous amortized benchmarking tool for glyph3d-native HyperLayout.

Measures:
  - Cold run (Run 1)
  - Warm runs (Runs 2..N)
  - Mean, Min, Max, and StdDev across warm runs
  - Detailed phase timings: walk, pass1, pass2, backend, scene, visual init
"""

import argparse
import re
import statistics
import subprocess
import sys
from pathlib import Path

PASS1_RE = re.compile(r"hyper\.pass1:[^\n]*?time\.busy=([0-9.]+)([a-zA-Z\u0080-\uFFFF]+)")
PASS2_RE = re.compile(r"hyper\.pass2:[^\n]*?time\.busy=([0-9.]+)([a-zA-Z\u0080-\uFFFF]+)")
WALK_RE = re.compile(r"phases: walk ([0-9.]+)s \| backend ([0-9.]+)s \(([0-9.]+) MB/s")
VISUAL_RE = re.compile(r"visual:.*?total visual init ([0-9.]+)s")

def parse_time_to_ms(val_str, unit):
    v = float(val_str)
    if unit == "s":
        return v * 1000.0
    elif unit == "ms":
        return v
    elif unit == "µs":
        return v / 1000.0
    return v

def run_once(bin_path, repo_path, extra_args):
    cmd = [
        str(bin_path),
        "--load-repo", str(repo_path),
        "--repo-engine", "hyper",
        "--frames", "1",
    ] + extra_args

    import os
    env = os.environ.copy()
    env["RUST_LOG"] = "glyph3d_native=info"
    env["NO_COLOR"] = "1"
    env["RUST_LOG_STYLE"] = "never"

    proc = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, env=env)
    out = proc.stdout

    res = {
        "walk_ms": None,
        "pass1_ms": None,
        "pass2_ms": None,
        "backend_ms": None,
        "mb_s": None,
        "visual_init_ms": None,
    }

    m_p1 = PASS1_RE.search(out)
    if m_p1:
        res["pass1_ms"] = parse_time_to_ms(m_p1.group(1), m_p1.group(2))

    m_p2 = PASS2_RE.search(out)
    if m_p2:
        res["pass2_ms"] = parse_time_to_ms(m_p2.group(1), m_p2.group(2))

    m_walk = WALK_RE.search(out)
    if m_walk:
        res["walk_ms"] = float(m_walk.group(1)) * 1000.0
        res["backend_ms"] = float(m_walk.group(2)) * 1000.0
        res["mb_s"] = float(m_walk.group(3))

    m_vis = VISUAL_RE.search(out)
    if m_vis:
        res["visual_init_ms"] = float(m_vis.group(1)) * 1000.0

    return res

def print_row(name, cold_val, warm_vals, unit="ms"):
    if not warm_vals or warm_vals[0] is None:
        return
    valid_warm = [v for v in warm_vals if v is not None]
    if not valid_warm:
        return
    mean = statistics.mean(valid_warm)
    std = statistics.stdev(valid_warm) if len(valid_warm) > 1 else 0.0
    min_v = min(valid_warm)
    max_v = max(valid_warm)
    cold_str = f"{cold_val:.1f} {unit}" if cold_val is not None else "N/A"
    print(f"  {name:<18} | Cold: {cold_str:>9} | Mean: {mean:>6.1f} ± {std:>4.1f} {unit} | Min: {min_v:>6.1f} | Max: {max_v:>6.1f}")

def main():
    parser = argparse.ArgumentParser(description="Amortized benchmark for HyperLayout")
    parser.add_argument("--repo", default="/Users/lugo/localdev/viz-web/glyph3d-js", help="Path to repo")
    parser.add_argument("--bin", default="./target/release/glyph3d-native", help="Path to binary")
    parser.add_argument("-n", "--runs", type=int, default=5, help="Total number of runs (1 cold + N-1 warm)")
    parser.add_argument("--field-mode", default="derived", choices=["derived", "instanced"])
    parser.add_argument("--color-mode", default=None, choices=["syntax", "flat", "uniform"])
    args, extra = parser.parse_known_args()

    bin_path = Path(args.bin).resolve()
    if not bin_path.exists():
        print(f"Binary not found at {bin_path}. Run cargo build --release -p glyph3d-native first.")
        sys.exit(1)

    extra_args = ["--field-mode", args.field_mode]
    if args.color_mode:
        extra_args += ["--color-mode", args.color_mode]
    extra_args += extra

    print(f"=== Benchmarking HyperLayout ({args.field_mode} mode) ===")
    print(f"Repo: {args.repo}")
    print(f"Iterations: {args.runs} (1 cold, {args.runs - 1} warm)")
    print(f"Args: {' '.join(extra_args)}\n")

    runs = []
    for i in range(1, args.runs + 1):
        tag = "Cold (Run 1)" if i == 1 else f"Warm (Run {i})"
        sys.stdout.write(f"Executing {tag}... ")
        sys.stdout.flush()
        data = run_once(bin_path, args.repo, extra_args)
        runs.append(data)
        bk = data['backend_ms']
        vi = data['visual_init_ms']
        bk_str = f"{bk:.1f} ms" if bk is not None else "error"
        vi_str = f"{vi:.1f} ms" if vi is not None else "error"
        print(f"done (Backend: {bk_str}, Visual Init: {vi_str})")

    cold = runs[0]
    warm = runs[1:] if len(runs) > 1 else runs

    print("\n--- Summary Performance Breakdown ---")
    print_row("Walk", cold["walk_ms"], [r["walk_ms"] for r in warm])
    print_row("Pass 1 (Scan)", cold["pass1_ms"], [r["pass1_ms"] for r in warm])
    print_row("Pass 2 (Emit)", cold["pass2_ms"], [r["pass2_ms"] for r in warm])
    print_row("Backend Total", cold["backend_ms"], [r["backend_ms"] for r in warm])
    print_row("Visual Init", cold["visual_init_ms"], [r["visual_init_ms"] for r in warm])
    print_row("Throughput", cold["mb_s"], [r["mb_s"] for r in warm], unit="MB/s")
    print("--------------------------------------\n")

if __name__ == "__main__":
    main()
