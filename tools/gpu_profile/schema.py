"""Backend-neutral vocabulary for GPU kernel profiles.

Every backend (Metal today, NVIDIA next) maps its native counters onto the
CANONICAL metric names below and also keeps its native values verbatim, so a
report can compare an M2 capture against an RTX capture row by row while still
showing what each vendor actually measured. A canonical metric a backend
cannot measure is simply absent — never zero-filled — and the report prints
"n/a" for it. Adding a backend means writing one mapping table, not touching
the report.

The profile file is JSON, schema tag ``glyph3d.gpu-profile/1``:

    {
      "schema": "glyph3d.gpu-profile/1",
      "backend": "metal" | "nvidia",
      "device": "Apple M2",
      "label": "base",                 # free-form, defaults to the out dir name
      "command": [...],
      "captured_at": "2026-10-05T11:00:00-05:00",
      "stages": [
        {"name": "apply_and_emit",
         "duration_ms": 378.1,          # the stage's own GPU timestamp query
         "attribution": {...},          # how samples were tied to the stage
         "metrics": {canonical: value},
         "native": {native counter name: value}}
      ],
      "notes": [...]
    }
"""

from __future__ import annotations

from dataclasses import dataclass

SCHEMA_TAG = "glyph3d.gpu-profile/1"


@dataclass(frozen=True)
class Metric:
    key: str
    unit: str
    meaning: str


# Order is report order: time, then "is the machine full", then "what is it
# waiting on" (limiters), then traffic, then compiler facts.
CANONICAL_METRICS: tuple[Metric, ...] = (
    Metric("duration_ms", "ms", "stage GPU duration from its own timestamp query"),
    Metric("occupancy_pct", "%", "achieved occupancy: resident simdgroups/warps vs device maximum"),
    Metric("theoretical_occupancy_pct", "%", "occupancy ceiling set by registers/shared memory/block size"),
    Metric("alu_limiter_pct", "%", "time ALU work was attempted, vs peak (Metal limiter / NV SM throughput)"),
    Metric("alu_utilization_pct", "%", "ALU work actually issued, vs peak"),
    Metric("f32_utilization_pct", "%", "FP32 pipe utilization"),
    Metric("device_load_limiter_pct", "%", "time global/buffer loads were attempted, vs peak"),
    Metric("device_load_utilization_pct", "%", "global/buffer load throughput, vs peak"),
    Metric("device_store_limiter_pct", "%", "time global/buffer stores were attempted, vs peak"),
    Metric("device_store_utilization_pct", "%", "global/buffer store throughput, vs peak"),
    Metric("shared_load_limiter_pct", "%", "time threadgroup/shared loads were attempted, vs peak"),
    Metric("shared_load_utilization_pct", "%", "threadgroup/shared load throughput, vs peak"),
    Metric("shared_store_limiter_pct", "%", "time threadgroup/shared stores were attempted, vs peak"),
    Metric("shared_store_utilization_pct", "%", "threadgroup/shared store throughput, vs peak"),
    Metric("l2_limiter_pct", "%", "last-level cache busy, vs peak"),
    Metric("l2_utilization_pct", "%", "last-level cache throughput, vs peak"),
    Metric("mmu_limiter_pct", "%", "address translation pressure from LLC misses (Metal only)"),
    Metric("dram_read_gbps", "GB/s", "bytes read from memory outside the GPU, per second"),
    Metric("dram_write_gbps", "GB/s", "bytes written to memory outside the GPU, per second"),
    Metric("registers_per_thread", "regs", "registers allocated per thread (NV only; Metal does not expose)"),
    Metric("register_spill_bytes", "B", "bytes the shader compiler spilled for this pipeline"),
    Metric("local_memory_instructions", "inst", "local-memory (spill) load+store instructions executed (NV)"),
)

CANONICAL_KEYS = tuple(m.key for m in CANONICAL_METRICS)
METRIC_BY_KEY = {m.key: m for m in CANONICAL_METRICS}


def new_profile(backend: str, device: str, label: str, command: list[str], captured_at: str) -> dict:
    return {
        "schema": SCHEMA_TAG,
        "backend": backend,
        "device": device,
        "label": label,
        "command": command,
        "captured_at": captured_at,
        "stages": [],
        "notes": [],
    }


def new_stage(name: str, duration_ms: float | None) -> dict:
    stage = {"name": name, "attribution": {}, "metrics": {}, "native": {}}
    if duration_ms is not None:
        stage["metrics"]["duration_ms"] = duration_ms
    return stage
