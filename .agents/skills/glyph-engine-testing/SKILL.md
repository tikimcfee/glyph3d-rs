---
name: glyph-engine-testing
description: Standard operating procedures for testing, verifying, benchmarking, and interacting with the glyph3d-native rendering engine and TUI.
---

# Glyph Engine Testing & Verification Guide

## 1. The 4-Tier Verification Pyramid

Always run tests appropriate to your scope before committing:

| Tier | Command | Purpose & Invariants |
| :--- | :--- | :--- |
| **1. Unit & WGSL** | `cargo test --workspace` | Tests unit logic, WGSL shader parsing/validation, encase layouts (211+ tests). |
| **2. Layout Parity** | `cargo run --release -p glyph3d-native -- --cubecl-repo-check native/fixtures/g-pick-repo` | Compares GPU layout slots against the CPU reference fold bit-for-bit across hundreds of thousands of slots. Must report 0 mismatches. |
| **3. Manifest & Golden** | `cargo run -p glyph -- test --frozen` | The canonical battery: verifies 0 compiler/clippy/doc warnings, currency stamp, pick-oracle, and byte-exact parity across all 9 golden views on Metal. |
| **4. Mutation Proof** | `cargo run -p glyph -- prove` | Applies declared mutations to verify that all checks fail when broken. |

### Currency & The Frozen Gate
- `cargo glyph build`: Updates content hashes and builds products.
- `cargo glyph test --frozen`: Asserts that artifacts are already current without rebuilding. If `--frozen` fails with "renderer is stale", run `cargo run -p glyph -- build`.

---

## 2. Parity & Edge-Case Checks

When modifying scan, monoid, or position kernels in `native/src/cubecl_chain/`:
1. **Sanity Check**:
   `cargo run --release -p glyph3d-native -- --cubecl-repo-check native/fixtures/g-pick-repo`
2. **Unicode & Cluster Verification**:
   `cargo run --release -p glyph3d-native -- --cubecl-repo-check tools/vendor/third-party/unicode-ucd`
   Checks emoji sequences, skin-tone modifiers, flags, and zero-width joiners.

---

## 3. Flagship Performance Benchmarking

To measure visual initialization, layout throughput, and memory footprint on the flagship corpus:

```sh
# Full benchmark with GPU hardware timestamps:
GLYPH_CHAIN_PROF=1 ./target/release/glyph3d-native \
  --load-repo /Users/lugo/localdev/viz-web/glyph3d-js \
  --repo-engine cubecl \
  --field-mode derived \
  --screenshot out/perf_test.png \
  --frames 1
```

### Metrics to Track:
- **Total Visual Initialization**: Wall-clock time from launch to first pixel presentation.
- **Hardware Compute Time**: Metal timestamp query sum across `decode_probe`, `tile_scan`, `spine_scan`, and `apply_and_emit`.
- **Submit + Render**: First-frame render pipeline execution time (typically < 1 ms).
- **Peak Device Memory**: VRAM allocated for buffers.

---

## 4. UI vs TUI Distinction

- **`glyph tui`**:
  - Standalone terminal application built with Ratatui (`native/src/tui/`).
  - Used for configuring presets, selecting layout engines (`cubecl`, `hyper`), toggling render modes (`instanced`, `derived`), and launching runs.
- **In-Engine UI (`egui`)**:
  - The HUD rendered directly inside the 3D graphics window.
  - Controls camera frustum, spatial zone dragging, desk rolodex, and live diagnostics.
