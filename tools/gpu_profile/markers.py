"""Parse the renderer's backend-neutral stage markers.

With ``GLYPH_CHAIN_PROF=stages GLYPH_GPU_ISOLATE_MS=<gap>`` the chain
profiler (native/src/cubecl_chain/repo/dispatch.rs, ``ChainProfiler``) sleeps
``gap`` before and after every stage and prints, on stderr:

    gpu-mark: begin <stage> <unix_ns>
    gpu-mark: gpu_ns <stage> <ns>        # the stage's own GPU timestamp query
    gpu-mark: end <stage> <unix_ns>

A stage's ``end`` is printed only after its GPU work has resolved, so
[begin, end] brackets the stage's GPU execution and the gaps are real GPU
idle. Timeline samplers (Metal GPU Counters, Nsight Systems) attribute
samples by this window; per-kernel profilers (Nsight Compute) use the
``gpu_ns`` values to tie generic kernel names back to stages.
"""

from __future__ import annotations

import re
from dataclasses import dataclass

_MARK = re.compile(r"gpu-mark: (begin|end|gpu_ns) (\S+) (\d+)")


@dataclass
class StageMark:
    name: str
    begin_unix_ns: int
    end_unix_ns: int | None = None
    gpu_ns: int | None = None

    @property
    def duration_ms(self) -> float | None:
        return None if self.gpu_ns is None else self.gpu_ns / 1e6


def parse_marks(text: str) -> list[StageMark]:
    """Stage windows in launch order. A stage name may repeat (each is kept)."""
    marks: list[StageMark] = []
    open_by_name: dict[str, StageMark] = {}
    for kind, name, value in _MARK.findall(text):
        number = int(value)
        if kind == "begin":
            mark = StageMark(name, number)
            marks.append(mark)
            open_by_name[name] = mark
        elif name in open_by_name:
            mark = open_by_name[name]
            if kind == "gpu_ns":
                mark.gpu_ns = number
            else:
                mark.end_unix_ns = number
                del open_by_name[name]
    return [m for m in marks if m.end_unix_ns is not None]
