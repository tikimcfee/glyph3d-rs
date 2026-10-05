"""CLI: capture, analyze and compare per-stage GPU hardware counters.

    python3 tools/gpu_profile capture --out out/gpu-profile/base -- \\
        target/release/glyph3d-native --repo-engine cubecl --load-repo <dir> --screenshot /tmp/x.png --frames 1
    python3 tools/gpu_profile analyze out/gpu-profile/base        # re-run attribution on a saved capture
    python3 tools/gpu_profile report out/gpu-profile/base out/gpu-profile/nowalk --stage apply_and_emit

``capture`` = record + analyze; it writes <out>/profile.json. ``report``
compares any number of profiles — from any backend — on the canonical
metrics, one table per stage.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
    __package__ = "gpu_profile"

from gpu_profile.backends import BACKENDS, auto  # noqa: E402
from gpu_profile.schema import CANONICAL_METRICS, SCHEMA_TAG  # noqa: E402

DEFAULT_GAP_MS = 150


def _backend(name: str):
    return auto() if name == "auto" else BACKENDS[name]


def _write_profile(out: Path, profile: dict) -> None:
    (out / "profile.json").write_text(json.dumps(profile, indent=2) + "\n")
    print(f"gpu_profile: wrote {out / 'profile.json'} ({len(profile['stages'])} stages)")
    for note in profile["notes"]:
        print(f"  note: {note}")


def cmd_capture(args) -> None:
    backend = _backend(args.backend)
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        raise SystemExit("gpu_profile capture: give the target command after --")
    env = {"GLYPH_CHAIN_PROF": "stages", "GLYPH_GPU_ISOLATE_MS": str(args.gap_ms)}
    meta = {"backend": backend.NAME, "command": command, "gap_ms": args.gap_ms, "label": args.label or out.name}
    (out / "capture.json").write_text(json.dumps(meta, indent=2) + "\n")
    print(f"gpu_profile: capturing with {backend.NAME} -> {out}")
    backend.capture(command, out, env, args.time_limit)
    _write_profile(out, backend.analyze(out, meta["label"], command, args.gap_ms))


def cmd_analyze(args) -> None:
    out = Path(args.dir)
    meta = json.loads((out / "capture.json").read_text())
    backend = BACKENDS[meta["backend"]]
    _write_profile(out, backend.analyze(out, args.label or meta["label"], meta["command"], meta["gap_ms"]))


def _format(value) -> str:
    if value is None:
        return "n/a"
    if isinstance(value, float):
        return f"{value:.3f}" if abs(value) < 10 else f"{value:.1f}"
    return str(value)


def cmd_report(args) -> None:
    profiles = []
    for directory in args.dirs:
        path = Path(directory)
        path = path / "profile.json" if path.is_dir() else path
        profile = json.loads(path.read_text())
        if profile.get("schema") != SCHEMA_TAG:
            raise SystemExit(f"{path}: not a {SCHEMA_TAG} profile")
        profiles.append(profile)
    stage_order: list[str] = []
    for profile in profiles:
        for stage in profile["stages"]:
            if stage["name"] not in stage_order:
                stage_order.append(stage["name"])
    selected = args.stage or stage_order
    labels = [f"{p['label']} ({p['backend']})" for p in profiles]
    for name in selected:
        per_profile = [next((s for s in p["stages"] if s["name"] == name), None) for p in profiles]
        if all(s is None for s in per_profile):
            continue
        print(f"\n### {name}\n")
        print("| metric | " + " | ".join(labels) + " |")
        print("|---|" + "---|" * len(labels))
        for metric in CANONICAL_METRICS:
            values = [s["metrics"].get(metric.key) if s else None for s in per_profile]
            if all(v is None for v in values):
                continue
            print(f"| {metric.key} ({metric.unit}) | " + " | ".join(_format(v) for v in values) + " |")
        spans = [s["attribution"].get("active_span_ms") if s else None for s in per_profile]
        if any(v is not None for v in spans):
            print("| _attribution: active span (ms)_ | " + " | ".join(_format(v) for v in spans) + " |")
        if args.native:
            keys = sorted({k for s in per_profile if s for k in s["native"]})
            for key in keys:
                values = [s["native"].get(key) if s else None for s in per_profile]
                print(f"| _{key}_ | " + " | ".join(_format(v) for v in values) + " |")
    devices = {f"{p['label']}: {p['device']}" for p in profiles}
    print("\nDevices: " + "; ".join(sorted(devices)))


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(prog="gpu_profile", description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="verb", required=True)

    capture = sub.add_parser("capture", help="record the target and write profile.json")
    capture.add_argument("--backend", default="auto", choices=["auto", *BACKENDS])
    capture.add_argument("--out", required=True, help="capture directory (raw data + profile.json)")
    capture.add_argument("--label", help="name for this capture in reports (default: out dir name)")
    capture.add_argument("--gap-ms", type=int, default=DEFAULT_GAP_MS, help="idle gap around each stage")
    capture.add_argument("--time-limit", type=int, default=120, help="seconds before the recorder stops")
    capture.add_argument("command", nargs=argparse.REMAINDER)
    capture.set_defaults(func=cmd_capture)

    analyze = sub.add_parser("analyze", help="re-run attribution on a saved capture")
    analyze.add_argument("dir")
    analyze.add_argument("--label")
    analyze.set_defaults(func=cmd_analyze)

    report = sub.add_parser("report", help="compare profiles on canonical metrics")
    report.add_argument("dirs", nargs="+")
    report.add_argument("--stage", action="append", help="stage to show (repeatable; default all)")
    report.add_argument("--native", action="store_true", help="also print each backend's native counters")
    report.set_defaults(func=cmd_report)

    args = parser.parse_args(argv)
    args.func(args)


if __name__ == "__main__":
    main()
