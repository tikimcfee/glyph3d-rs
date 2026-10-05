"""NVIDIA backend: Nsight Compute (``ncu``) per-kernel counters.

UNTESTED ON HARDWARE as of 2026-10-05 — written ahead of the move to NVIDIA so
the schema and report are already shared. Expect to adjust metric names to
the installed ncu version (``ncu --query-metrics``).

Unlike Metal's timeline sampler, ncu replays each kernel and measures it in
isolation, so attribution is by kernel, not by time. Stage names come from:
  1. the kernel name, when it contains a stage name (CUDA backend names
     kernels after their Rust fn), else
  2. launch order matched to the ``gpu-mark:`` stage list when the counts
     agree (wgpu/naga SPIR-V often names every entry point ``main``), else
  3. one pseudo-stage per launch, ``launch<N>:<kernel>``, with a note.
"""

from __future__ import annotations

import csv
import os
import io
import shutil
import subprocess
from datetime import datetime
from pathlib import Path

from ..markers import parse_marks
from ..schema import new_profile, new_stage

NAME = "nvidia"

# ncu metric -> canonical key.
CANONICAL_MAP = {
    "sm__warps_active.avg.pct_of_peak_sustained_active": "occupancy_pct",
    "sm__maximum_warps_per_active_cycle_pct": "theoretical_occupancy_pct",
    "sm__throughput.avg.pct_of_peak_sustained_elapsed": "alu_limiter_pct",
    "sm__inst_executed_pipe_alu.avg.pct_of_peak_sustained_active": "alu_utilization_pct",
    "sm__inst_executed_pipe_fma.avg.pct_of_peak_sustained_active": "f32_utilization_pct",
    "l1tex__t_sectors_pipe_lsu_mem_global_op_ld.avg.pct_of_peak_sustained_elapsed": "device_load_utilization_pct",
    "l1tex__t_sectors_pipe_lsu_mem_global_op_st.avg.pct_of_peak_sustained_elapsed": "device_store_utilization_pct",
    "l1tex__data_pipe_lsu_wavefronts_mem_shared_op_ld.avg.pct_of_peak_sustained_elapsed": "shared_load_utilization_pct",
    "l1tex__data_pipe_lsu_wavefronts_mem_shared_op_st.avg.pct_of_peak_sustained_elapsed": "shared_store_utilization_pct",
    "lts__throughput.avg.pct_of_peak_sustained_elapsed": "l2_limiter_pct",
    "lts__t_sectors.avg.pct_of_peak_sustained_elapsed": "l2_utilization_pct",
    "dram__bytes_read.sum.per_second": "dram_read_gbps",
    "dram__bytes_write.sum.per_second": "dram_write_gbps",
    "launch__registers_per_thread": "registers_per_thread",
}
# Native-only extras worth having on NVIDIA: where warps stall. Metal has no
# equivalent, so these stay out of the canonical table and appear under
# "native" (the report can print them with --native).
STALL_METRICS = [
    f"smsp__average_warp_latency_issue_stalled_{reason}.ratio"
    for reason in (
        "long_scoreboard", "short_scoreboard", "barrier", "membar", "mio_throttle",
        "lg_throttle", "math_pipe_throttle", "wait", "not_selected", "selected",
        "no_instruction", "dispatch_stall", "drain", "imc_miss", "branch_resolving",
    )
]
EXTRA_METRICS = [
    "gpu__time_duration.sum",
    "device__attribute_display_name",
    "smsp__sass_inst_executed_op_local_ld.sum",
    "smsp__sass_inst_executed_op_local_st.sum",
]


def available() -> bool:
    return shutil.which("ncu") is not None


def capture(command: list[str], out_dir: Path, env: dict[str, str], time_limit_s: int) -> None:
    raw = out_dir / "raw"
    raw.mkdir(parents=True, exist_ok=True)
    metrics = ",".join([*CANONICAL_MAP, *STALL_METRICS, *EXTRA_METRICS])
    run = [
        "ncu", "--csv", "--page", "raw", "--metrics", metrics,
        "--target-processes", "all",
        "--log-file", str(raw / "ncu.csv"),
        *command,
    ]
    full_env = {**os.environ, **env}
    with open(out_dir / "target.stdout", "w") as log:
        subprocess.run(run, stdout=log, stderr=subprocess.STDOUT, env=full_env, check=True, timeout=time_limit_s * 20)


def _to_number(text: str, unit: str) -> float | str:
    try:
        value = float(text.replace(",", ""))
    except ValueError:
        return text
    scale = {"byte/second": 1e-9, "Kbyte/second": 1e-6, "Mbyte/second": 1e-3, "Gbyte/second": 1.0,
             "Tbyte/second": 1e3, "nsecond": 1e-6, "usecond": 1e-3, "msecond": 1.0}
    return value * scale.get(unit, 1.0)


def analyze(out_dir: Path, label: str, command: list[str], gap_ms: int) -> dict:
    del gap_ms  # ncu replays kernels in isolation; no time windows needed
    text = (out_dir / "raw" / "ncu.csv").read_text(errors="replace")
    lines = [line for line in text.splitlines() if line.startswith('"')]
    reader = list(csv.reader(io.StringIO("\n".join(lines))))
    header, units, rows = reader[0], reader[1], reader[2:]
    launches = [{h: _to_number(v, u) for h, u, v in zip(header, units, row)} for row in rows]
    device = str(launches[0].get("device__attribute_display_name", "NVIDIA GPU")) if launches else "NVIDIA GPU"
    profile = new_profile(NAME, device, label, command, datetime.now().astimezone().isoformat(timespec="seconds"))
    marks = parse_marks((out_dir / "target.stdout").read_text(errors="replace"))
    stage_names = [m.name for m in marks]

    def stage_name(index: int, launch: dict) -> str:
        kernel = str(launch.get("Kernel Name", ""))
        for name in stage_names:
            if name in kernel:
                return name
        if len(launches) == len(stage_names):
            return stage_names[index]
        return f"launch{index}:{kernel}"

    if launches and len(launches) != len(stage_names) and not any(
        n in str(launch.get("Kernel Name", "")) for launch in launches for n in stage_names
    ):
        profile["notes"].append(
            f"{len(launches)} launches vs {len(stage_names)} marked stages and no stage names in kernel names: "
            "label passes or name entry points to attribute"
        )
    for index, launch in enumerate(launches):
        duration = launch.get("gpu__time_duration.sum")
        stage = new_stage(stage_name(index, launch), duration if isinstance(duration, float) else None)
        stage["attribution"] = {"method": "ncu-kernel-replay", "kernel": launch.get("Kernel Name")}
        for metric, key in CANONICAL_MAP.items():
            value = launch.get(metric)
            if isinstance(value, float):
                stage["metrics"][key] = round(value, 4)
        local = [launch.get(m) for m in ("smsp__sass_inst_executed_op_local_ld.sum", "smsp__sass_inst_executed_op_local_st.sum")]
        if all(isinstance(v, float) for v in local):
            stage["metrics"]["local_memory_instructions"] = sum(local)
        stage["native"] = {k: v for k, v in launch.items() if k in STALL_METRICS or k in CANONICAL_MAP}
        profile["stages"].append(stage)
    return profile
