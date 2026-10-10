#!/usr/bin/env python3
"""
bench_pass1_ab.py — interleaved A/B of HyperLayout's Pass 1 between two
renderer binaries, on a normal load (no line table): `--repo-engine batch`
runs Pass 1 inline (the default `hyper` engine hides it on the prefetch
thread) and prints the `hyper.pass1` span; `hyper.pass2` and the backend
total come along. Round-robin, order alternating per round, optional
cool-down (fanless machines), min / median over the kept rounds.

Usage: bench_pass1_ab.py --repo <dir> --a <bin> --b <bin> [--rounds 8] [--warmup 1] [--cooldown 0]
"""
import argparse, os, re, statistics, subprocess, time

SPAN = re.compile(r"(hyper\.pass1|hyper\.pass2)(?:\{[^}]*\})?: glyph3d_native::[^\n]*?close time\.busy=([0-9.]+)(µs|ms|s)")
PH = re.compile(r"backend ([0-9.]+)s")


def ms(v, u):
    return float(v) * {"s": 1000, "ms": 1, "µs": 0.001}[u]


def run(bin_path, repo):
    env = dict(os.environ, RUST_LOG="glyph3d_native=info", NO_COLOR="1", RUST_LOG_STYLE="never")
    cmd = [bin_path, "--load-repo", repo, "--repo-engine", "batch", "--field-mode", "derived",
           "--screenshot", "/tmp/bench_pass1_ab.png", "--frames", "1"]
    out = subprocess.run(cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True).stdout
    r = {}
    for n, v, u in SPAN.findall(out):
        r[n] = ms(v, u)
    m = PH.search(out)
    r["backend"] = float(m.group(1)) * 1000 if m else float("nan")
    return r


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True)
    ap.add_argument("--a", required=True)
    ap.add_argument("--b", required=True)
    ap.add_argument("--rounds", type=int, default=8)
    ap.add_argument("--warmup", type=int, default=1)
    ap.add_argument("--cooldown", type=float, default=0.0)
    a = ap.parse_args()
    sides = [("A", a.a), ("B", a.b)]
    res = {"A": [], "B": []}
    print(f"A {a.a}\nB {a.b}\nrepo {a.repo}\nloadavg before {os.getloadavg()}")
    for rnd in range(a.rounds):
        order = sides if rnd % 2 == 0 else sides[::-1]
        for name, b in order:
            r = run(b, a.repo)
            if rnd >= a.warmup:
                res[name].append(r)
            print(f"  r{rnd} {name} pass1 {r.get('hyper.pass1', float('nan')):7.2f}  pass2 {r.get('hyper.pass2', float('nan')):7.1f}  backend {r['backend']:7.1f}", flush=True)
            if a.cooldown > 0:
                time.sleep(a.cooldown)
    print(f"loadavg after  {os.getloadavg()}")
    for k in ["hyper.pass1", "hyper.pass2", "backend"]:
        for name in "AB":
            v = [r[k] for r in res[name] if k in r]
            if v:
                print(f"{name} {k:<12} n={len(v)} min {min(v):8.2f} median {statistics.median(v):8.2f} max {max(v):8.2f} ms")


if __name__ == "__main__":
    main()
