---
name: glyph-engine-testing
description: Standard operating procedures for testing, verifying, benchmarking, and interacting with the glyph3d-native rendering engine and TUI.
---

# Glyph Engine Testing & Verification Guide

## 1. The 4-Tier Verification Pyramid

Always run tests appropriate to your scope before committing:

| Tier | Command | Purpose & Invariants |
| :--- | :--- | :--- |
| **1. Unit & WGSL** | `cargo test --workspace` | Tests unit logic, WGSL shader parsing/validation, encase layouts (222 tests across 13 binaries). |
| **2. Layout Parity** | `bash tools/check-pick-oracle.sh` | Compares CPU layout picks and pixel-ray round trips against an independent Python fold oracle bit-for-bit across multiple depths and wrap modes. Must report ALL PASS. |
| **3. Manifest & Golden** | `cargo run -p glyph -- test rust` | The canonical battery: verifies 0 compiler/clippy/doc warnings, currency stamp, and all unit tests meeting the ratchet floor (`settings.test_floor = 222`). |
| **4. Mutation Proof** | `cargo run -p glyph -- prove` | Applies declared mutations to verify that all checks fail when broken. |

### Currency & The Frozen Gate
- `cargo glyph build`: Updates content hashes and builds products.
- `cargo glyph test --frozen`: Asserts that artifacts are already current without rebuilding. If `--frozen` fails with "renderer is stale", run `cargo run -p glyph -- build`.

---

## 2. Parity & Edge-Case Checks

1. **Pick Oracle (CPU HyperLayout & Reference Fold)**:
   ```sh
   bash tools/check-pick-oracle.sh
   ```
   Validates deterministic picks and ray-cast hits on `alpha.rs`, `wide.txt`, `sub/deep.py`, `long.md`, `text.rs`, and `emoji-view.txt`.

2. **HyperLayout against the oracle (records, host and device Pass 2, paint), incl. emoji sequences**:
   ```sh
   cargo glyph gate hyper-oracle
   ```
   Holds the production layout to the oracle-backed fold bit-for-bit over the fixture corpus, `g-pick-repo`,
   `g-cluster-repo` and the cluster/chunk-cut fixtures; emoji sequences, skin tones, flags and ZWJ included.
   (The CubeCL parity checks that used to sit here retired with the engine on 2026-10-09.)

---

## 3. Flagship Performance Benchmarking

### A. Amortized Benchmarking with `tools/bench_hyper.py`
Use the dedicated amortized benchmarking harness to measure cold vs warm performance:

```sh
# Benchmark 5 runs (1 cold + 4 warm) on flagship corpus. Point
# GLYPH_FLAGSHIP_REPO at your checkout of it (or any large repo);
# bench_hyper.py defaults --repo to that variable, else this repo.
python3 tools/bench_hyper.py \
  --repo "$GLYPH_FLAGSHIP_REPO" \
  -n 5 \
  --field-mode derived \
  --color-mode flat
```

Outputs mean, min, max, and stddev for:
- **Walk**: Filesystem stat and read.
- **Pass 1 (Scan)**: Parallel chunk newline and survivor scanning.
- **Pass 2 (Emit)**: Aligned burst slot emission directly into mapped unified memory.
- **Backend Total & Throughput**: Total CPU layout time and MB/s.
- **Total Visual Initialization**: Wall-clock time to first presented frame.

### B. Single-Run Detailed Timings & Profiles
```sh
# CPU HyperLayout with syntax coloring:
./target/release/glyph3d-native \
  --load-repo "$GLYPH_FLAGSHIP_REPO" \
  --repo-engine hyper \
  --field-mode derived \
  --color-mode syntax \
  --frames 1

```

---

## 4. UI vs TUI Distinction

- **Launcher TUI (`glyph tui`)**:
  - Interactive terminal application built with Ratatui (`glyph/src/tui/`). Run via `cargo run -p glyph -- tui`.
  - Features dynamic subtitles on focus/select for layout engines (`hyper`, `direct`, `batch`) and field modes (`instanced`, `derived`).
- **In-Engine UI (`egui`)**:
  - The HUD rendered directly inside the 3D graphics window.
  - Controls camera frustum, spatial zone dragging, desk rolodex, and live diagnostics (`F1` toggles debug panel).

---

## 5. Cross-Platform Desktop Considerations

When developing, testing, or benchmarking on non-macOS desktop architectures (x86_64, discrete NVIDIA/AMD GPUs), see the exhaustive guide in `research/desktop-platform-audit.md`:
- **Direct Unified Memory vs Discrete Staging**: On macOS Metal, slot buffers are directly mapped into DRAM (`mapped_base: Some(addr)`). On desktop discrete GPUs, `layout_device_discrete` routes through mapped staging buffers followed by GPU copy blits (`mapped_base: None`).
- **Dynamic Color Updates**: When `mapped_base` is `None`, per-slot `write_colors` falls back to single-slot driver writes. Batch dynamic color changes when targeting discrete GPUs.
- **Cache Sizing**: `CHUNK_THRESHOLD_BYTES = 64 KiB` was sized for Apple Silicon 128 KiB L1D caches. On desktop x86 with 32/48 KiB L1D, test 32 KiB chunks.
- **GPU Keys**: Baselines are keyed by `<backend>-<vendor>` (`metal-apple`, `vulkan-nvidia`, `vulkan-amd`). New host adapters can be calibrated with `cargo glyph drift`.
