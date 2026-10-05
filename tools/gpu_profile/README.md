# gpu_profile — per-stage GPU hardware counters, vendor-neutral

`tools/gpu_profile` answers "why is this kernel slow on this GPU" below the
level of timestamp queries: occupancy, which unit limits it, cache/DRAM
traffic, register spills. One CLI, pluggable backends, one shared vocabulary,
so an M2 capture and an RTX capture compare row by row.

```sh
# record + attribute (writes <out>/profile.json; raw data kept under <out>/raw)
python3 tools/gpu_profile capture --out out/gpu-profile/base -- \
    target/release/glyph3d-native --repo-engine cubecl \
    --load-repo /Users/lugo/localdev/viz-web/glyph3d-js --screenshot /tmp/x.png --frames 1

# compare captures (any backend mix) on the canonical metrics
python3 tools/gpu_profile report out/gpu-profile/base out/gpu-profile/variant --stage apply_and_emit [--native]

# re-run attribution on a saved capture after changing the analyzer
python3 tools/gpu_profile analyze out/gpu-profile/base
```

## Layout

| file | role |
|---|---|
| `schema.py` | the canonical metric vocabulary and the `glyph3d.gpu-profile/1` JSON shape |
| `markers.py` | parses the renderer's `gpu-mark:` stage windows (backend-neutral) |
| `backends/metal.py` | Instruments "Metal GPU Counters" timeline via `xctrace`, time-window attribution |
| `backends/nvidia.py` | Nsight Compute per-kernel replay (**untested on hardware**) |
| `__main__.py` | `capture` / `analyze` / `report` |

A backend is five names — `NAME`, `available()`, `gpu_busy_pct()`,
`capture(...)`, `analyze(...)` — plus a table mapping native counters to canonical keys.
Native values are always kept verbatim under `"native"`; a canonical metric
a backend cannot measure is absent and reports print `n/a`.

## The attribution contract (renderer side)

With `GLYPH_CHAIN_PROF=stages GLYPH_GPU_ISOLATE_MS=<gap>` (capture sets both)
`ChainProfiler` in `native/src/cubecl_chain/repo/dispatch.rs` sleeps `gap`
before and after every stage and prints `gpu-mark: begin|gpu_ns|end <stage>
<value>` on stderr. Stage `end` prints only after the stage's GPU work has
resolved, so the gaps are true GPU idle. Profiling-only: output is untouched.

## Metal notes (M2, Xcode 26.6 / xctrace 16.0, measured 2026-10-05)

- Counters are a GPU-global **timeline** (~35.75 µs per sample), not
  per-dispatch: Apple silicon exposes only GPU timestamps through
  `MTLCounterSampleBuffer` (no dispatch-boundary sampling), so Instruments is
  the only counter source. Stages shorter than ~1 ms get a handful of samples
  or none; read their rows accordingly (`attribution.samples`).
- cubecl-wgpu creates compute passes with `label: None`, so encoders show up
  as wgpu's `(wgpu internal) Signal:Compute Command 0` — hence time windows.
  Labeling passes upstream would let the analyzer use
  `metal-application-encoders-list` directly.
- `Compute Occupancy` idles at ~0.83%, never 0; activity = above the 10th
  percentile floor.
- The TOC `start-date` is NOT the sampler's time zero (measured +539 ms off);
  the analyzer calibrates the offset on the longest stage and records it in
  `notes`. Every stage carries `active_span_ms` next to its timestamp
  duration; a mismatch is flagged as "attribution suspect".
- **Check the GPU is otherwise idle before trusting any number.** On
  2026-10-05 a killed process left an orphaned compute workload running —
  `metal-gpu-intervals` showed `GPU Execution ( n/a )` holding the compute
  channel 100% of an idle 3 s trace, `ioreg -r -d 1 -c IOAccelerator` showed
  `Device Utilization %=100` at rest, and our kernels were time-sliced
  against it in ~8 ms quanta (every timing that day was inflated ~2x). Quick
  check: `ioreg -r -d 1 -c IOAccelerator | grep -o '"Device Utilization %"=[0-9]*'`
  should read near 0 when nothing is running. `capture` now runs this check
  itself (per backend: ioreg on Metal, nvidia-smi on NVIDIA) and refuses a
  GPU more than 15% busy at rest unless `--allow-busy-gpu`.
- Deeper than counters: a programmatic `.gputrace` (MTLCaptureManager,
  `MTL_CAPTURE_ENABLED=1`) opened in Xcode gives per-line shader cost and
  register pressure; not automated here.

## NVIDIA notes (for the move)

- `ncu` replays **CUDA** kernels only; it does not profile Vulkan compute. If
  we run CubeCL's CUDA backend, `backends/nvidia.py` applies as written (kernel
  names carry the Rust fn name). On wgpu/Vulkan, use Nsight Systems
  (`nsys profile --trace=vulkan,vulkan-annotations --gpu-metrics-device=all`)
  — a timeline sampler like Metal's, so it would reuse the same `gpu-mark:`
  window attribution — and `VK_EXT_pipeline_executable_properties` for
  register/spill counts. Neither is written yet.
- Stall reasons (`smsp__average_warp_latency_issue_stalled_*`) have no Metal
  equivalent; they are kept native-only and shown with `report --native`.
