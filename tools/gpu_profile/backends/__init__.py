"""Profiler backends. Each module exposes the same four names:

    NAME: str
    available() -> bool
    capture(command, out_dir, env, time_limit_s) -> None   # writes out_dir/raw/...
    analyze(out_dir, label, command, gap_ms) -> dict       # a schema.SCHEMA_TAG profile
"""

from . import metal, nvidia

BACKENDS = {metal.NAME: metal, nvidia.NAME: nvidia}


def auto():
    for backend in (metal, nvidia):
        if backend.available():
            return backend
    raise SystemExit("gpu_profile: no backend available (need xcrun xctrace on macOS or ncu on PATH)")
