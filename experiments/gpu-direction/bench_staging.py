#!/usr/bin/env python3
"""
bench_staging.py — interleaved timing of the HyperLayout discrete-GPU staging
experiment switches (GLYPH_EXPERIMENT_STAGING, GLYPH_EXPERIMENT_SKIP_TINT_REREAD;
see native/src/layout_hyper/device_alloc.rs). Reports the `hyper.*` spans and
the backend total, min/median over the kept rounds, round-robin so machine
drift hits every configuration alike.

Usage: bench_staging.py --repo <dir> [--bin target/release/glyph3d-native] [--rounds 6]
"""
import argparse, os, re, statistics, subprocess

CONFIGS = {
    "baseline": {},
    "skip-tint": {"GLYPH_EXPERIMENT_SKIP_TINT_REREAD": "1"},
    "mapwrite": {"GLYPH_EXPERIMENT_STAGING": "mapwrite"},
    "host-staging": {"GLYPH_EXPERIMENT_STAGING": "host"},
    "mapwrite+skip": {"GLYPH_EXPERIMENT_STAGING": "mapwrite", "GLYPH_EXPERIMENT_SKIP_TINT_REREAD": "1"},
    "host+skip": {"GLYPH_EXPERIMENT_STAGING": "host", "GLYPH_EXPERIMENT_SKIP_TINT_REREAD": "1"},
}
SPAN = re.compile(r"(hyper\.[a-z_.0-9]+)(?:\{[^}]*\})?: glyph3d_native::[^\n]*?close time\.busy=([0-9.]+)(µs|ms|s)")
PH = re.compile(r"backend ([0-9.]+)s")
KEYS = ["backend", "hyper.staging.create", "hyper.pass2", "hyper.emoji_tint_pairs",
        "hyper.staging.unmap", "hyper.staging.copy_submit", "hyper.staging.poll"]


def ms(v, u):
    return float(v) * {"s": 1000, "ms": 1, "µs": 0.001}[u]


def loadavg():
    return " ".join(open("/proc/loadavg").read().split()[:3])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True)
    ap.add_argument("--bin", default="target/release/glyph3d-native")
    ap.add_argument("--rounds", type=int, default=6)
    ap.add_argument("--field-mode", default="derived")
    a = ap.parse_args()
    res = {k: [] for k in CONFIGS}
    print("loadavg before", loadavg())
    for rnd in range(a.rounds):
        for name, env in CONFIGS.items():
            e = dict(os.environ, RUST_LOG="glyph3d_native=info", NO_COLOR="1", RUST_LOG_STYLE="never", **env)
            cmd = [a.bin, "--load-repo", a.repo, "--repo-engine", "hyper", "--field-mode", a.field_mode,
                   "--screenshot", "/tmp/bench_staging.png", "--frames", "1"]
            out = subprocess.run(cmd, env=e, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True).stdout
            r = {}
            for n, v, u in SPAN.findall(out):
                r[n] = ms(v, u)
            m = PH.search(out)
            r["backend"] = float(m.group(1)) * 1000 if m else float("nan")
            if rnd > 0:
                res[name].append(r)
            print(f"  r{rnd} {name:<14} backend {r['backend']:7.1f}", flush=True)
    print("loadavg after ", loadavg())
    print(f"{'config':<14} " + " ".join(f"{k.replace('hyper.', ''):>20}" for k in KEYS))
    for name in CONFIGS:
        row = []
        for k in KEYS:
            vals = [r[k] for r in res[name] if k in r]
            row.append(f"{min(vals):7.1f}/{statistics.median(vals):7.1f}" if vals else f"{'-':>15}")
        print(f"{name:<14} " + " ".join(f"{c:>20}" for c in row))
    print(f"(min/median ms over {a.rounds - 1} kept rounds; field mode {a.field_mode})")


if __name__ == "__main__":
    main()
