"""Metal backend: Instruments' "Metal GPU Counters" timeline via xctrace.

Capture records the target under the Metal System Trace template plus the
Metal GPU Counters instrument (Apple's limiter/utilization/occupancy set,
sampled on a timeline every few tens of microseconds). The counters are
GPU-global and wgpu leaves compute encoders unlabeled, so samples are tied to
a stage by TIME: the renderer brackets each stage with idle gaps and
``gpu-mark:`` wall-clock lines (see markers.py), and each stage takes the
contiguous block of compute activity that overlaps its window.

Attribution is checked, not assumed: every stage records the active span it
was given next to its own GPU timestamp duration, and a large disagreement is
written into the profile's notes.
"""

from __future__ import annotations

import bisect
import os
import re
import subprocess
import xml.etree.ElementTree as ET
from array import array
from datetime import datetime
from pathlib import Path

from ..markers import StageMark, parse_marks
from ..schema import new_profile, new_stage

NAME = "metal"

# Apple counter name -> canonical key. Counters not listed are kept native-only.
CANONICAL_MAP = {
    "Compute Occupancy": "occupancy_pct",
    "ALU Limiter": "alu_limiter_pct",
    "ALU Utilization": "alu_utilization_pct",
    "F32 Utilization": "f32_utilization_pct",
    "Buffer Read Limiter": "device_load_limiter_pct",
    "Buffer Load Utilization": "device_load_utilization_pct",
    "Buffer Write Limiter": "device_store_limiter_pct",
    "Buffer Store Utilization": "device_store_utilization_pct",
    "Threadgroup/Imageblock Load Limiter": "shared_load_limiter_pct",
    "Threadgroup/Imageblock Load Utilization": "shared_load_utilization_pct",
    "Threadgroup/Imageblock Store Limiter": "shared_store_limiter_pct",
    "Threadgroup/Imageblock Store Utilization": "shared_store_utilization_pct",
    "GPU Last Level Cache Limiter": "l2_limiter_pct",
    "GPU Last Level Cache Utilization": "l2_utilization_pct",
    "MMU Limiter": "mmu_limiter_pct",
    "GPU Read Bandwidth": "dram_read_gbps",
    "GPU Write Bandwidth": "dram_write_gbps",
}
# Graphics-only counters: always ~0 for a compute stage, so they are dropped
# from the native dump to keep it readable.
_GRAPHICS_ONLY = ("Texture", "Fragment", "Vertex", "Partial Renders")
_ACTIVITY_COUNTER = "Compute Occupancy"


def available() -> bool:
    return os.uname().sysname == "Darwin" and subprocess.run(
        ["xcrun", "--find", "xctrace"], capture_output=True
    ).returncode == 0


def gpu_busy_pct() -> float | None:
    """Whole-GPU utilization right now (IOAccelerator 'Device Utilization %')."""
    text = subprocess.run(["ioreg", "-r", "-d", "1", "-c", "IOAccelerator"], capture_output=True, text=True).stdout
    match = re.search(r'"Device Utilization %"=(\d+)', text)
    return float(match.group(1)) if match else None


def capture(command: list[str], out_dir: Path, env: dict[str, str], time_limit_s: int) -> None:
    raw = out_dir / "raw"
    raw.mkdir(parents=True, exist_ok=True)
    trace = raw / "capture.trace"
    record = [
        "xcrun", "xctrace", "record",
        "--template", "Metal System Trace",
        "--instrument", "Metal GPU Counters",
        "--time-limit", f"{time_limit_s}s",
        "--output", str(trace),
        "--target-stdout", str(out_dir / "target.stdout"),
    ]
    for key, value in env.items():
        record += ["--env", f"{key}={value}"]
    record += ["--launch", "--", *command]
    with open(out_dir / "capture.log", "w") as log:
        subprocess.run(record, stdout=log, stderr=subprocess.STDOUT, check=True)


# ── xctrace export parsing ────────────────────────────────────────────────────


def _export(trace: Path, dest: Path, xpath: str | None = None) -> Path:
    if not dest.exists():
        cmd = ["xcrun", "xctrace", "export", "--input", str(trace)]
        cmd += ["--xpath", xpath] if xpath else ["--toc"]
        with open(dest, "w") as fh:
            subprocess.run(cmd, stdout=fh, check=True)
    return dest


def _table(trace: Path, raw: Path, schema: str) -> Path:
    return _export(trace, raw / f"{schema}.xml", f'/trace-toc/run[@number="1"]/data/table[@schema="{schema}"]')


def _rows(path: Path):
    """Yield each row as [(text, fmt), ...], resolving xctrace's id/ref dedup."""
    interned: dict[str, tuple[str | None, str | None]] = {}
    for _, element in ET.iterparse(path, events=("end",)):
        if element.tag != "row":
            continue
        for node in element.iter():
            node_id = node.attrib.get("id")
            if node_id is not None:
                interned[node_id] = (node.text, node.attrib.get("fmt"))
        columns = []
        for column in element:
            ref = column.attrib.get("ref")
            columns.append(interned[ref] if ref is not None else (column.text, column.attrib.get("fmt")))
        yield columns
        element.clear()


def _trace_start_unix_ns(toc: Path) -> int:
    text = toc.read_text()
    match = re.search(r"<start-date>([^<]+)</start-date>", text)
    if not match:
        raise RuntimeError("trace TOC has no <start-date>")
    return int(datetime.fromisoformat(match.group(1)).timestamp() * 1e9)


def _device_name(toc: Path) -> str:
    text = toc.read_text()
    match = re.search(r'<device [^>]*model="([^"]*)"', text)
    gpu = subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True).stdout.strip()
    return gpu + (f" ({match.group(1)})" if match else "")


def _counter_series(trace: Path, raw: Path):
    names: dict[int, str] = {}
    for columns in _rows(_table(trace, raw, "gpu-counter-info")):
        names[int(columns[1][0])] = columns[2][0]
    series: dict[str, tuple[array, array]] = {}
    for columns in _rows(_table(trace, raw, "gpu-counter-value")):
        name = names.get(int(columns[1][0]))
        if name is None:
            continue
        times, values = series.setdefault(name, (array("q"), array("d")))
        times.append(int(columns[0][0]))
        values.append(float(columns[2][0]))
    return series


def _spill_events(trace: Path, raw: Path) -> list[tuple[int, int]]:
    events = []
    for columns in _rows(_table(trace, raw, "graphics-compiler-spill-events")):
        events.append((int(columns[0][0]), int(columns[3][0] or 0)))
    return events


# ── attribution ───────────────────────────────────────────────────────────────


def _activity_threshold(values: array) -> float:
    """Above the idle floor. Apple's Compute Occupancy never reads 0 — it
    idles near 0.83% (measured on M2) — so "nonzero" would call the whole
    trace one active block. The floor is the 10th percentile (the isolation
    gaps guarantee plenty of idle samples)."""
    ordered = sorted(values)
    floor = ordered[len(ordered) // 10]
    peak = ordered[min(len(ordered) - 1, int(len(ordered) * 0.999))]
    return floor + max(2.0, 0.05 * (peak - floor))


def _activity_blocks(times: array, values: array, max_gap_ns: int) -> list[tuple[int, int, int, int]]:
    """Maximal runs of active samples: (start_ns, end_ns, first_index, last_index)."""
    threshold = _activity_threshold(values)
    blocks = []
    start = previous = None
    first = last = 0
    for index, (t, v) in enumerate(zip(times, values)):
        if v <= threshold:
            continue
        if start is None or t - previous > max_gap_ns:
            if start is not None:
                blocks.append((start, previous, first, last))
            start, first = t, index
        previous, last = t, index
    if start is not None:
        blocks.append((start, previous, first, last))
    return blocks


def _window_mean(times: array, values: array, lo: int, hi: int) -> tuple[float, int] | None:
    i = bisect.bisect_left(times, lo)
    j = bisect.bisect_right(times, hi)
    if j <= i:
        return None
    window = values[i:j]
    return sum(window) / len(window), j - i


def analyze(out_dir: Path, label: str, command: list[str], gap_ms: int) -> dict:
    raw = out_dir / "raw"
    trace = raw / "capture.trace"
    toc = _export(trace, raw / "toc.xml")
    marks_text = "".join(
        p.read_text(errors="replace") for p in (out_dir / "target.stdout", out_dir / "capture.log") if p.exists()
    )
    marks = parse_marks(marks_text)
    start_unix_ns = _trace_start_unix_ns(toc)
    captured_at = datetime.fromtimestamp(start_unix_ns / 1e9).astimezone().isoformat(timespec="seconds")
    profile = new_profile(NAME, _device_name(toc), label, command, captured_at)
    if not marks:
        profile["notes"].append("no gpu-mark lines found: run with GLYPH_CHAIN_PROF=stages GLYPH_GPU_ISOLATE_MS=<gap>")
        return profile

    series = _counter_series(trace, raw)
    if _ACTIVITY_COUNTER not in series:
        profile["notes"].append(f"no '{_ACTIVITY_COUNTER}' samples: was the Metal GPU Counters instrument recorded?")
        return profile
    activity_times, activity_values = series[_ACTIVITY_COUNTER]
    gap_ns = gap_ms * 1_000_000
    blocks = _activity_blocks(activity_times, activity_values, max_gap_ns=gap_ns // 4)

    # Calibrate the wall-clock -> trace-time offset on the longest stage: the
    # TOC start-date is only millisecond-precise and its zero is not
    # guaranteed to be the sampler's zero.
    offset = -start_unix_ns
    anchor = max(marks, key=lambda m: m.gpu_ns or 0)
    if anchor.gpu_ns:
        expected = anchor.begin_unix_ns + offset
        candidates = [b for b in blocks if abs((b[1] - b[0]) - anchor.gpu_ns) < 0.25 * anchor.gpu_ns + 2e6]
        if candidates:
            best = min(candidates, key=lambda b: abs(b[0] - expected))
            if abs(best[0] - expected) < 10 * gap_ns:
                offset += best[0] - expected
                profile["notes"].append(
                    f"offset calibrated on '{anchor.name}': {(best[0] - expected) / 1e6:+.2f} ms vs TOC start-date"
                )

    spills = _spill_events(trace, raw)
    if spills:
        profile["notes"].append(
            f"compiler spill events: {len(spills)}, max {max(s[1] for s in spills)} B (pipeline-level, see per-stage when attributable)"
        )

    for mark in marks:
        stage = new_stage(mark.name, mark.duration_ms)
        window_lo = mark.begin_unix_ns + offset
        window_hi = mark.end_unix_ns + offset
        overlapping = [b for b in blocks if b[1] >= window_lo and b[0] <= window_hi]
        if not overlapping:
            stage["attribution"] = {"method": "isolate-window", "samples": 0}
            profile["stages"].append(stage)
            continue
        span_lo = min(b[0] for b in overlapping)
        span_hi = max(b[1] for b in overlapping)
        attribution = {
            "method": "isolate-window",
            "active_span_ms": round((span_hi - span_lo) / 1e6, 3),
            "blocks": len(overlapping),
        }
        for name, (times, values) in series.items():
            if any(name.startswith(prefix) for prefix in _GRAPHICS_ONLY):
                continue
            result = _window_mean(times, values, span_lo, span_hi)
            if result is None:
                continue
            mean, samples = result
            stage["native"][name] = round(mean, 4)
            if name == _ACTIVITY_COUNTER:
                attribution["samples"] = samples
            key = CANONICAL_MAP.get(name)
            if key:
                stage["metrics"][key] = round(mean, 4)
        spilled = [bytes_ for t, bytes_ in spills if window_lo <= t <= window_hi]
        if spilled:
            stage["metrics"]["register_spill_bytes"] = max(spilled)
        stage["attribution"] = attribution
        if mark.gpu_ns and attribution["active_span_ms"] > 0:
            ratio = attribution["active_span_ms"] / (mark.gpu_ns / 1e6)
            attribution["span_vs_timestamp"] = round(ratio, 3)
            if mark.gpu_ns > 5e6 and not 0.8 <= ratio <= 1.25:
                profile["notes"].append(
                    f"'{mark.name}': active span {attribution['active_span_ms']} ms vs timestamp "
                    f"{mark.gpu_ns / 1e6:.2f} ms — attribution suspect"
                )
        profile["stages"].append(stage)
    return profile
