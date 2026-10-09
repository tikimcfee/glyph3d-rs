# Apple Silicon vs. Desktop x86_64 & Discrete GPU Platform Audit

## Overview

This document serves as an architectural bridge for agents and engineers transitioning from the Apple Silicon development baseline (Apple M2, Metal, unified memory) to high-performance desktop architectures (x86_64, AMD Zen / Intel Core, Vulkan / DirectX 12, discrete GPUs with dedicated VRAM).

The codebase is **100% pure Rust** and builds without platform-specific assembly. However, specific memory allocation paths, cache size thresholds, GPU staging pipelines, and test gates were tuned or partitioned for Apple Silicon hardware characteristics.

---

## 1. Unified vs. Discrete Memory Architecture

### A. The Direct Mapping Path (`native/src/layout_hyper/device_alloc.rs`)
- **Apple Silicon Metal (`layout_device_unified`)**:
  - Gated under `#[cfg(target_os = "macos")]`.
  - Downcasts to HAL via `device.as_hal::<wgpu::hal::api::Metal>()` and creates a buffer with `MTLStorageModeShared`.
  - Maps device memory to a host pointer via `hal_dev.map_buffer`.
  *Pass 2 writes glyph slots directly into this pointer with zero intermediate memory copies or CPU-to-GPU bus transfers.*
  - Returns `DeviceEmission { mapped_base: Some(addr), .. }`.
- **Desktop / Non-macOS (`layout_device_discrete`)**:
  - Active under `#[cfg(not(target_os = "macos"))]`.
  - Allocates a staging buffer with `wgpu::BufferUsages::COPY_SRC` and `mapped_at_creation: true`.
  - Runs Pass 2 directly into the mapped staging slice pointer, unmaps it, and records an explicit `copy_buffer_to_buffer` command to blit from host staging into device VRAM.
  - Returns `DeviceEmission { mapped_base: None, .. }`.

### B. ⚠️ Critical Desktop Bottleneck: Dynamic Color Updates (`write_colors`)
- Locations:
  - [`crates/glyph-field-derived/src/storage.rs:58-81`](file:///Users/lugo/localdev/viz-native/glyph3d-native/crates/glyph-field-derived/src/storage.rs#L58-L81)
  - [`crates/glyph-field-instanced/src/storage.rs:58-81`](file:///Users/lugo/localdev/viz-native/glyph3d-native/crates/glyph-field-instanced/src/storage.rs#L58-L81)
- **Behavior**:
  - When `mapped_base` is `Some(addr)` (Metal unified), `write_colors` modifies colors in-place in DRAM via raw pointer arithmetic.
  - When `mapped_base` is `None` (Desktop discrete GPU), it falls back to:
    ```rust
    for (i, &color) in colors.iter().enumerate() {
        self.write_field(queue, slot, COLOR_OFFSET, bytemuck::bytes_of(&color));
    }
    ```
    This issues **one 4-byte `queue.write_buffer` driver call per slot**. For large files (e.g. 50,000 to 500,000 glyphs), runtime color updates (hover highlights, semantic selection, AST token repainting) will suffer driver call overhead on discrete GPUs.
- **Desktop Fix**: Coalesce contiguous slot ranges or stage writes into a mapped scratch buffer before submitting a single bulk copy.

### C. Direct Host Upload Flag (`setup.rs`)
- [`native/src/glyph_scene/setup.rs:125`](file:///Users/lugo/localdev/viz-native/glyph3d-native/native/src/glyph_scene/setup.rs#L125):
  ```rust
  direct_host_upload: ctx.profile.backend == wgpu::Backend::Metal
  ```
  On Metal, host records transcode directly into mapped shared buffers. On Vulkan/DX12, this evaluates to `false` and cleanly routes through `upload_staged_discrete`.

### D. Chunked Storage Buffers & Hardware Binding Limits
- Location: [`native/src/layout_hyper/device_alloc.rs`](file:///home/ivan/dev/glyph3d-rs/native/src/layout_hyper/device_alloc.rs)
- **Problem**: Modern GPUs enforce `max_storage_buffer_binding_size` (often 128 MiB–2 GiB; 2047 MiB on NVIDIA Vulkan). For huge codebases (e.g. 144M–1.3B glyph instances), single buffer allocations exceed hardware binding limits and cause driver validation failures.
- **Solution**: The layout planner partitions slot allocations across multiple `DeviceSlotChunk` storage buffers, binding each separately or within allowable device limits.

### E. Streaming 64 MiB Staging Buffer & PCIe DMA Saturation
- Location: [`native/src/layout_hyper/device_alloc.rs:stage_host_memory`](file:///home/ivan/dev/glyph3d-rs/native/src/layout_hyper/device_alloc.rs)
- **Problem**: In mega-scale repos like `torvalds/linux` (74,313 files, 1.29B glyphs, 24.69 GB `DerivedSlot` buffer size), staging buffers sized to match target storage chunks (1.9 GiB each) risk immediate VRAM exhaustion (`OutOfMemory`) on cards with 24–32 GB VRAM.
- **Solution**: Discrete uploads stream through a single reusable **64 MiB staging buffer** (`wgpu::BufferUsages::COPY_SRC | MAP_WRITE`) synchronized via `dev.device.poll(Wait)`.
- **Sizing Rationale (Why 64 MiB?)**:
  1. **PCIe DMA Line-Rate Plateau**: On PCIe 4.0/5.0 x16, small transfers (<8 MiB) are latency-bound by driver submissions and memory barriers. The DMA saturation curve reaches ~98% of peak hardware bus throughput (~24–28 GB/s) around 32–64 MiB; larger buffers yield diminishing returns (<1% gain).
  2. **Bounded VRAM Headroom**: Capping staging to 64 MiB bounds transient VRAM overhead to **0.2% of 32 GB**, guaranteeing that any repository whose final scene fits in VRAM will upload without OOM.
  3. **High Transfer-to-Sync Ratio**: A 64 MiB transfer takes ~2.5 ms on PCIe, while CPU-GPU synchronization (`map_async` + `poll(Wait)`) takes ~20–50 µs (<1.5% sync overhead).

### F. Dynamic Group Buffer Sizing & 32-bit Addressability
- Locations:
  - [`native/src/glyph_scene/setup.rs`](file:///home/ivan/dev/glyph3d-rs/native/src/glyph_scene/setup.rs)
  - [`crates/glyph-field-derived/shaders/glyph_field_derived.wgsl`](file:///home/ivan/dev/glyph3d-rs/crates/glyph-field-derived/shaders/glyph_field_derived.wgsl)
  - [`crates/glyph-field-derived/src/slot.rs`](file:///home/ivan/dev/glyph3d-rs/crates/glyph-field-derived/src/slot.rs)
- **Problem**: Repositories with $>65,536$ files (e.g. Linux kernel with 74,313 files) overflow 16-bit packed index fields. In particular, packing `item_idx` and `group_id` into a 32-bit word (`(item & 0xFFFF) | (group << 16)`) caused file indices $2 \le i < 65,536$ to unpack to values $>74,313$, falsely triggering vertex shader culling and dropping all glyphs while leaving backdrop cards visible.
- **Solution**:
  - `group_buf` is dynamically sized to `groups.len().max(65536)`.
  - `DerivedSlot.item_and_group` carries the raw 32-bit `group_id` directly, which the WGSL vertex shader indexes without 16-bit shifts.

---

## 2. CPU Cache Hierarchy & Chunk Constants

### A. Chunk Threshold (`CHUNK_THRESHOLD_BYTES`)
- Location: [`native/src/layout_hyper/chunk.rs:12`](file:///Users/lugo/localdev/viz-native/glyph3d-native/native/src/layout_hyper/chunk.rs#L12)
  ```rust
  pub const CHUNK_THRESHOLD_BYTES: usize = 64 * 1024; // 64 KiB
  ```
- **Apple Silicon Tuning**:
  - Apple M1/M2/M3 P-cores have **128 KiB L1 Data Cache** per core (and 64 KiB on E-cores).
  - A 64 KiB chunk plus working line buffers fits comfortably inside L1D cache throughout both Pass 1 and Pass 2.
- **Desktop x86_64 Architecture**:
  - AMD Zen 3 / Zen 4 / Zen 5: **32 KiB L1 Data Cache** per core (512 KiB to 1 MB L2).
  - Intel Golden Cove / Raptor Cove: **48 KiB L1 Data Cache** per core (1.25 MB to 2 MB L2).
  - A 64 KiB chunk exceeds the 32 KiB / 48 KiB L1D on desktop x86, spilling into L2.
- **Optimization for Desktop Agent**:
  Benchmark `CHUNK_THRESHOLD_BYTES = 32 * 1024` or `16 * 1024` using `tools/bench_hyper.py`. Sizing chunks to stay 100% within x86 L1D can yield measurable throughput gains during Pass 1 scan and Pass 2 emission.

### B. Cache Line Width & Burst Slot Stores
- Location: [`native/src/layout_hyper/pass2_device.rs:239`](file:///Users/lugo/localdev/viz-native/glyph3d-native/native/src/layout_hyper/pass2_device.rs#L239)
  - `emit_burst8`: emits 8 `DerivedSlot`s (160 bytes) or 4 `RenderSlot`s (128 bytes).
  - Standard x86_64 cache line width is **64 bytes** (Apple M2 SLC/L2 cache lines are 128 bytes).
  - `DerivedSlot` is 20 bytes (Word 0: X, Word 1: Row, Word 2: Glyph/Wrap, Word 3: Color, Word 4: Item/Group). Because 20 is not a power of two, slots straddle 64-byte boundaries.
  - Apple Silicon store buffers easily handle unaligned writes. On x86_64, test whether 64-byte aligned chunk stores or 16-slot groups (320 bytes = exactly 5 cache lines) reduce split-line store penalties.

---

## 3. GPU Profiles, Limits, and Golden Baselines

### A. Golden Pixel Keys (`out/tooling-ab/baseline/<key>/`)
- Generated by [`GpuProfile::key()`](file:///Users/lugo/localdev/viz-native/glyph3d-native/native/src/gpu.rs#L308) in `<backend>-<vendor>` format.
  - Apple Silicon Metal: `metal-apple`
  - Linux/Windows NVIDIA: `vulkan-nvidia`
  - Linux/Windows AMD: `vulkan-amd`
  - Linux/Windows Intel: `vulkan-intel`
- The golden gate (`cargo run -p glyph -- test render`) requires a baseline directory matching the host's GPU key.
- If running on a new desktop adapter with no baseline, the gate reports `no set for this host's key`.
- Adoption is straightforward: run `cargo glyph drift` to verify differences are purely edge-rasterization noise, and follow the adoption commands printed by the runner.

### B. Compute Workgroup Limits
- Apple M2 reports `max_compute_workgroups_per_dimension = 65,535`. CubeCL grids exceeding 65,535 tiles spill onto the Y dimension (`cubes_of`).
- Desktop Vulkan adapters (NVIDIA RTX, AMD Radeon) typically report $2^{31}-1$ (2,147,483,647). The spill logic remains functional and correct, but desktop hardware has virtually unlimited 1D grid headroom.

### C. CubeCL SIMD Widths
- Metal uses 32-wide SIMD groups.
- NVIDIA uses 32-wide warps.
- AMD RDNA uses 64-wide wavefronts (or Wave32 mode).
- CubeCL kernels configure thread units as 32 or 64. If optimizing GPU compute kernels on AMD desktop GPUs, check wavefront occupancy.

---

## 4. Concurrency Scaling Across Core Counts

- `HyperLayout` uses Rayon's global thread pool.
- Prototyping baseline: 8-core Apple M2 (4 Performance + 4 Efficiency cores).
- Desktop CPUs: 16-core / 32-thread AMD Ryzen or 24-core Intel Core i9.
- Because `slice_items_into_chunks` slices multi-megabyte files into independent 64 KiB segments with independent start columns and slot bases, Rayon achieves near-linear scaling across high-core-count desktop CPUs without thread starvation or single-core bottlenecks.

---

## 5. Portability & SIMD Summary

- The layout engine and renderer contain **no target-specific inline assembly or architecture intrinsics**.
- All fast paths (vectorized whitespace skips, comment fills, chunk loads `&[u32; 8]`, `colorize_pure_ascii_line`) rely on idiomatic slice operations and memory layouts that allow LLVM to auto-vectorize to AVX2/AVX-512 on x86_64 and NEON on ARM64.
- Deterministic pick checks (`bash tools/check-pick-oracle.sh`) and numerical layout checks (`cargo run -p glyph -- test rust`) are 100% platform-independent and bit-exact across all operating systems.
