#!/usr/bin/env python3
"""
bench_ab.py — interleaved A/B/C... timing of glyph3d-native repo loads.

Runs every named configuration once per round, round-robin, so a drifting
machine (another agent's GPU runs, thermal state) hits all configurations
alike. Discards the first `--warmup` rounds, reports min / median per
configuration, and prints the load average before and after.

Parsed lines (all from the renderer's own output with RUST_LOG=glyph3d_native=info):
  phases: walk Ws | backend Bs (...)         -> backend_ms
  visual: ... total visual init Ts           -> visual_ms
  hyper.pass1 ... time.busy=...              -> pass1_ms   (absent when prefetched)
  hyper.pass2 ... time.busy=...              -> pass2_ms
  hyper.staging.<name> ... time.busy=...     -> staging spans (added for this study)
  chain.upload / chain.dispatch ... busy     -> cubecl spans
  dispatch breakdown: b1 .., b2_totals .., b2_geo ..

Usage:
  bench_ab.py --repo <dir> --rounds 6 --warmup 1 \
      hyper-derived="--repo-engine hyper --field-mode derived" \
      cubecl-derived="--repo-engine cubecl --field-mode derived"
"""
import argparse, os, re, statistics, subprocess, sys, time

# The innermost span name is the one right before ": glyph3d_native::"; its
# fields (`{bytes=..}`) may sit between the name and the colon.
SPAN_RE = re.compile(r"([a-z0-9_.]+)(?:\{[^}]*\})?: glyph3d_native::[^\n]*?close time\.busy=([0-9.]+)(µs|ms|s|ns)")
PHASES_RE = re.compile(r"phases: walk ([0-9.]+)s \| backend ([0-9.]+)s \(([0-9.]+) MB/s")
VISUAL_RE = re.compile(r"total visual init ([0-9.]+)s")
B_RE = re.compile(r"dispatch breakdown: b1 ([0-9.]+)(µs|ms|s), b2_totals ([0-9.]+)(µs|ms|s), b2_geo ([0-9.]+)(µs|ms|s)")
INST_RE = re.compile(r"-> (\d+) glyph instances")


def to_ms(v, unit):
    v = float(v)
    return {"s": v * 1000, "ms": v, "µs": v / 1000, "ns": v / 1e6}[unit]


def run_once(bin_path, repo, args):
    env = dict(os.environ, RUST_LOG="glyph3d_native=info", NO_COLOR="1", RUST_LOG_STYLE="never")
    cmd = [bin_path, "--load-repo", repo, "--screenshot", "/tmp/bench_ab.png", "--frames", "1"] + args.split()
    t0 = time.monotonic()
    p = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, env=env)
    wall = (time.monotonic() - t0) * 1000
    out = p.stdout
    r = {"wall_ms": wall, "rc": p.returncode}
    m = PHASES_RE.search(out)
    if m:
        r["walk_ms"] = float(m.group(1)) * 1000
        r["backend_ms"] = float(m.group(2)) * 1000
    m = VISUAL_RE.search(out)
    if m:
        r["visual_ms"] = float(m.group(1)) * 1000
    m = INST_RE.search(out)
    if m:
        r["instances"] = int(m.group(1))
    for name, v, unit in SPAN_RE.findall(out):
        key = name.replace(".", "_") + "_ms"
        # the outermost "chain" span closes last; keep the LAST occurrence of each
        r[key] = to_ms(v, unit)
    m = B_RE.search(out)
    if m:
        r["b1_ms"] = to_ms(m.group(1), m.group(2))
        r["b2_totals_ms"] = to_ms(m.group(3), m.group(4))
        r["b2_geo_ms"] = to_ms(m.group(5), m.group(6))
    if p.returncode != 0:
        r["tail"] = out[-600:]
    return r


def loadavg():
    with open("/proc/loadavg") as f:
        return f.read().split()[:3]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default="target/release/glyph3d-native")
    ap.add_argument("--repo", required=True)
    ap.add_argument("--rounds", type=int, default=6)
    ap.add_argument("--warmup", type=int, default=1)
    ap.add_argument("configs", nargs="+", help="name=\"args\"")
    a = ap.parse_args()
    configs = []
    for c in a.configs:
        name, _, args = c.partition("=")
        configs.append((name, args))
    print(f"bin {a.bin}\nrepo {a.repo}\nrounds {a.rounds} (warmup {a.warmup} discarded)\nloadavg before {' '.join(loadavg())}")
    results = {n: [] for n, _ in configs}
    for rnd in range(a.rounds):
        for name, args in configs:
            r = run_once(a.bin, a.repo, args)
            tag = "warm" if rnd < a.warmup else "keep"
            print(f"  round {rnd} {name:<18} rc={r['rc']} backend={r.get('backend_ms', float('nan')):8.1f} ms visual={r.get('visual_ms', float('nan')):8.1f} ms wall={r['wall_ms']:8.0f} ms [{tag}]", flush=True)
            if r["rc"] != 0:
                print(r.get("tail", ""))
            if rnd >= a.warmup:
                results[name].append(r)
    print(f"loadavg after  {' '.join(loadavg())}\n")
    keys = sorted({k for rs in results.values() for r in rs for k in r if k.endswith("_ms")})
    hdr = f"{'config':<18} {'metric':<28} {'n':>2} {'min':>9} {'median':>9} {'max':>9}"
    print(hdr)
    print("-" * len(hdr))
    for name, _ in configs:
        rs = results[name]
        for k in keys:
            vals = [r[k] for r in rs if k in r]
            if not vals:
                continue
            print(f"{name:<18} {k:<28} {len(vals):>2} {min(vals):>9.1f} {statistics.median(vals):>9.1f} {max(vals):>9.1f}")
        inst = {r.get("instances") for r in rs}
        print(f"{name:<18} instances {inst}")
        print()


if __name__ == "__main__":
    main()
