# CubeCL Layout Engine — Architecture & Performance Handoff

## 1. Executive Summary & Verification State

This document provides complete architectural orientation, data structure mappings, optimization history, and performance results for the CubeCL GPU layout pipeline in [`glyph3d-native`](native).

### Verification Status (All Gates Green)
- **Zero Terse Variables**: All cryptic, 1-character, and 2-character variable names (`b`, `ie`, `fl`, `sm`, `gi`, `hgt`, `ir`, `im`, `tc`, `tm`, `xc`, `xm`, `lc`, `lm`, `wc`, `otb`, `wm`, etc.) have been completely eliminated across the entire [`native/src/cubecl_chain/`](native/src/cubecl_chain) module and its integration points in [`native/src/cubecl_layout.rs`](native/src/cubecl_layout.rs) and [`native/src/repo.rs`](native/src/repo.rs).
- **Compilation & Static Analysis**: `cargo check --features cubecl -p glyph3d-native` and `cargo clippy --features cubecl -p glyph3d-native` pass with **0 errors and 0 warnings**.
- **Unit Test Suite**: `cargo test --workspace` passes **157 of 157 tests** + full Naga WGSL shader validation.
- **Pixel Golden Oracle**: `cargo run -p glyph -- test render` passes with **all 9 golden views byte-equal** on Apple Silicon Metal rasterizer (`demo.png`, `text.png`, `repo-wide.png`, `repo-down.png`, `repo-zoom.png`, `repo-back-oblique.png`, `emoji.png`, `emoji-cluster.png`, `repo-cluster.png`) and `pick-oracle: PASS`.
- **Hardware Verification Passes**:
  - `cargo run --features cubecl -p glyph3d-native -- --cubecl-chain-check ./engine/fixtures/ascii-basic.pipe.bin`: `PASS` (exact counts + rows, fold>0 X bit-exact, line_adv within 1e-4).
  - `cargo run --features cubecl -p glyph3d-native -- --cubecl-decode-check ./engine/fixtures/ascii-basic.pipe.bin`: `PASS` (flags + advance bit-exact vs CPU).
  - `cargo run --features cubecl -p glyph3d-native -- --cubecl-cluster-check ./engine/fixtures/cluster-zwj.pipe.bin`: `PASS` (bit-exact sequences and trailers).
  - `cargo run --features cubecl -p glyph3d-native -- --cubecl-repo-check native/fixtures/g-pick-repo`: `PASS` (**0 record mismatches**, **0 measure bit-deviations**, **407,133 slots field-equal** vs CPU engine arena, **5 placements bit-equal**, tint stream consistent).
- **Flagship Production Performance (`glyph3d-js`: 97.0 MB source, 1,306 files, 95.2M glyph instances)**:
  - **Total Visual Initialization**: **0.645s – 0.759s** (smashing the sub-second goal; down from 1.258s baseline, **48.7% latency reduction**).
  - **Backend Layout Time**: **0.605s – 0.719s** (throughput: **134.9 – 160.3 MB/s**, up from 91.2 MB/s).
  - **Offscreen Submit + Render**: **475 µs – 606 µs** per frame.
  - **Pure Hardware Compute Execution**: Combined GPU hardware execution across all 12 active compute kernels is **234.8 ms**.
  - **Intermediate VRAM Saved**: **2.69 GB** of intermediate allocations and global roundtrips eliminated (`line_columns`, `item_record_ordinals`, `ordinal_to_byte_map`, `layout_metrics`).

---

## 2. Canonical Variable Dictionary & Strides

All kernels and host buffers use consistent domain terminology:

| Domain Variable Name | Former Name | Stride / Unit | Semantic Role |
| :--- | :--- | :--- | :--- |
| `glyph_flags` | `fl` | 1 byte/glyph (packed 4 per `u32`) | Bitflags: `F_LEADER`, `F_NEWLINE`, `F_EMOJI_BITMAP`, etc. |
| `advance_widths` | `sm` | 1 `f32` per glyph | Glyph advance width in world units |
| `glyph_indices` | `gi` | 1 `u32` per glyph | Atlas glyph index |
| `glyph_heights` | `hgt` | 1 `f32` per glyph | Glyph quad height in world units |
| `item_descriptors` | `h_item_desc` | `ITEM_DESC_STRIDE = 32` `u32` | Flat std430 descriptor: bounds, configs, spatial metrics, page gap, paint params |
| `tile_counts` | `tc` | `PARTIAL_COUNT_STRIDE = 8` `u32` | Monoid partial tree reduction counts per tile |
| `tile_metrics` | `tm` | 1 `f32` per tile | Partial line advance accumulation per tile |
| `tile_spine_counts` | `xc` | `PARTIAL_COUNT_STRIDE = 8` `u32` | Global exclusive prefix scan counts across tiles |
| `tile_spine_metrics` | `xm` | 1 `f32` per tile | Global exclusive prefix scan advances across tiles |
| `line_columns` | `lc` | `LC_STRIDE = 2` `u32` | `[row, col]` visual text grid coordinates (bypassed in `Instances` mode) |
| `layout_metrics` | `lm` | `LM_STRIDE = 3` `f32` | `[x, y, z]` world positions (bypassed in `Instances` mode) |
| `item_record_ordinals`| `wc` | 1 `u32` per glyph | Leader index within file item (bypassed in `Instances` mode) |
| `ordinal_to_byte_map` | `otb` | 1 `u32` per glyph | Leader ordinal to source byte mapping (bypassed in `Instances` mode) |
| `instance_slots` | `slots` | `SLOT_STRIDE = 8` `u32` | 32-byte `GlyphInstance` quads bound directly to render pipeline |
| `instance_tints` | `tint` | `TINT_STRIDE = 2` `u32` | 8-byte `(glyph_id, packed_color)` semantic tint stream |
| `item_extents` | `ext` | `EXT_STRIDE = 10` `Atomic<u32>` | Bounding box atomic keys: `[page_r, page_b, page_zmin, page_zmax, ink_min_x, ink_min_y, ink_max_x, ink_max_y, ink_min_z, ink_max_z]` |
| `max_row_extents` | `extent_words` | 2 `u32` per item | Fixed-point encoded maximum row length for pagination |
| `survivor_tile_prefixes`| `sxc` | 1 `u32` per tile | Prefix sum of survivor instances up to tile |
| `survivor_unit_prefixes`| `sup` | 1 `u32` per unit | Micro prefix sum of survivor instances within tile |
| `item_index` / `item_idx` | `it` | `usize` | Current file item index |
| `lhs` / `rhs` / `temp_carried` | `a` / `b` / `t` | `ChainElem` | Tree scan left/right operands and carried prefix |

---

## 3. Completed Optimization Milestones

### Step 1: Intermediate Buffer Elimination & Direct Emission (`apply_and_emit`)
- **Status: COMPLETED**.
- **Implementation**:
  - Unified the Blelloch scan and chase loop from `apply` with the spatial positioning, extent reductions, and direct slot/tint emission from `resolve_x_fused` into a single kernel [`apply_and_emit`](native/src/cubecl_chain/position.rs#L292).
  - In `ChainMode::Instances`, completely bypassed:
    - `line_columns` (`h_lc`, 776 MB)
    - `item_record_ordinals` (`h_wc`, 388 MB)
    - `ordinal_to_byte_map` (`h_otb`, 388 MB)
    - `layout_metrics` (`h_lm`, 1.14 GB)
    - Replaced with 4-byte dummy buffers (`client.empty(4)`), saving **2.69 GB** of VRAM.
  - Eliminated the separate `resolve_x_fused` dispatch. Block 2 Part B now requires only 3 dispatches: `tile_scan`, `spine_scan`, and `apply_and_emit`.

### Step 2: Multi-Tile Pagination & Exact Float Progression
- **Status: COMPLETED**.
- **Implementation**:
  - Resolved multi-tile extent calculation drift on large paginated files (e.g., `index-B1pVy8sS.js` with 3.63M records) by adhering to the exact two-term `fma` sequence:
    ```rust
    let x_with_tail = fma(page_col, stride_reach_tail, base_x);
    let final_x = fma(page_col, stride_reach, x_with_tail);
    ```
  - For mid-segment starts, worker threads inspect `segment_column = col % fold_unit`. If mid-segment, threads scan backward at most `segment_column` times in `glyph_flags` to locate `F_LEADER`, then forward-accumulate `advance_widths` left-to-right.
  - Verification: `cubecl-repo-check` reports **0 measure bit-deviations** across all 2,037,255 measure words.

### Step 3: Consolidated `ItemDescriptor` (32 Words / 128 Bytes, std430)
- **Status: COMPLETED**.
- **Implementation**:
  - Consolidated 5 separate host metadata buffers (`item_record_bounds`, `item_layout_configs`, `item_spatial_metrics`, `page_gap_x`, and `paint_descriptors`) into a single flat buffer `item_descriptors` with stride 32.
  - Storage buffer bindings on `apply_and_emit` reduced to 17 arguments.
  - Binary search directly accesses `item_descriptors[item_idx * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START]`.

### Step 4: Stale `derive_stride` & `wm` Cleanup
- **Status: COMPLETED**.
- **Implementation**:
  - `stride_reach` and `stride_reach_tail` are derived inline inside `apply_and_emit` via `advance_fixed` and `fixed_pair`.
  - Stale comments and dead dispatch paths in `native/src/cubecl_chain/repo/dispatch.rs` and `position.rs` updated.
  - Retained `resolve_x` and `derive_stride` strictly for independent fixture regression checks (`--cubecl-chain-check`) and micro-benchmarks (`bench.rs`).

### Step 5: Host Preallocation in `marshal()`
- **Status: COMPLETED**.
- **Implementation**:
  - Preallocated `Vec::with_capacity(total_bytes)` in `marshal()`, eliminating 1,306 reallocation copies during multi-file repository packing.

### Step 6: Complete Retirement of `ChainMode::Records` / `Both` & Dummy Buffer Removal
- **Status: COMPLETED**.
- **Implementation**:
  - Completely retired `ChainMode::Records` and `ChainMode::Both`. The layout pipeline is 100% unified on direct `Instances` emission.
  - Eliminated all 6 dummy 4-byte buffer allocations (`h_lc`, `h_wc`, `h_otb`, `h_lm`, `h_rmax`, `h_xmax`) and removed their fields from `ChainBuffers`.
  - Streamlined `apply_and_emit` signature from 17 down to 14 active, load-bearing buffer arguments.
  - Deleted dead wire-record emission logic (`emit_records_chunked`) from `tail_emit.rs`.
  - Updated `repo_check.rs` and `cubecl_layout.rs` to verify instance slots, tints, and placements directly, with wire records rederived on CPU for `VerifyLayout`.
  - Fully verified: all 157 unit tests, all 9 golden views, `check-cubecl.sh fork`, `check-cubecl.sh chain`, and `cubecl-repo-check` pass with zero warnings and 0 deviations.

---

## 4. Hardware Kernel Profile Breakdown (`glyph3d-js`, 95.2M Instances)

Metal hardware timestamp queries (`GLYPH_CHAIN_PROF=stages`) across all 12 compute kernels:

| Kernel Stage | Purpose | Hardware Time (`ms`) | Status |
| :--- | :--- | :--- | :--- |
| `decode_probe` | UTF-8 decode, cluster candidate detection & advance lookup | 31.19 ms | Active |
| `cand_sort` | Cluster candidate radix sorting | 0.28 ms | Active |
| `jump_build` | Sequence state transition jump table | 0.01 ms | Active |
| `cluster_rank` | Grapheme cluster ranking | 0.05 ms | Active |
| `item_roots` | Multi-item boundary root setup | 0.01 ms | Active |
| `cluster_mark` | Tail-leader grapheme mark | 0.01 ms | Active |
| `sv_count_tile` | Survivor instance count per tile | 6.75 ms | Active |
| `sv_count_spine` | Survivor count spine scan | 0.25 ms | Active |
| `item_totals` | Per-item instance total reduction | 0.02 ms | Active |
| `tile_scan` | Decoupled prefix scan (rake + shared Blelloch) | 24.29 ms | Active |
| `spine_scan` | Global spine scan | 0.32 ms | Active |
| `apply_and_emit` | Unified scan chase, positioning, extents, and direct slot/tint emission | 171.65 ms | **Unified** |
| `scatter_slots` | Secondary VRAM scatter | **0.00 ms** | **ELIMINATED** |
| `resolve_x_fused`| Separate position pass | **0.00 ms** | **ELIMINATED** |
| **Sum** | **Total Pure Compute Kernel Execution** | **~234.8 ms** | **12 Kernels** |

---

## 5. Critical Invariants & Landmines

1. **Golden Views test CPU HyperLayout**: Running `cargo run -p glyph -- test render` validates that the CPU layout and Metal renderer are intact, but **does not run CubeCL**. All CubeCL layout changes must be validated with:
   `target/release/glyph3d-native --cubecl-repo-check native/fixtures/g-pick-repo`
2. **Double-Single Fixed-Point Precision**: `advance_fixed` and `fixed_pair` in [`monoid.rs`](native/src/cubecl_chain/monoid.rs) emulate 64-bit precision across 32-bit floats. Never simplify them to standard `f32` addition or `cubecl-repo-check`'s zero-deviation census will fail.
3. **`EXT_STRIDE` is 10, not 8**: Bounding box extents are 10 lanes:
   `[page_right, page_bottom, page_z_min, page_z_max, ink_min_x, ink_min_y, ink_max_x, ink_max_y, ink_min_z, ink_max_z]`. Flags live in separate `shared_item_flags`.
4. **`--features cubecl` Gating**: The entire CubeCL pipeline is behind the `cubecl` cargo feature. Note that `cargo run -p glyph -- test render` builds without `--features cubecl`, so always rebuild with `cargo build --release --features cubecl -p glyph3d-native` after running `glyph`.
5. **CubeCL `IfElseExprExpand` Types**: CubeCL's AST expansion requires strictly matching types across ternary branches. Use `0usize` instead of `0` in integer index expressions.

---

## 6. Verification Commands

```sh
# 1. Fast compile & type check
cargo check --features cubecl -p glyph3d-native

# 2. Clippy zero-warning assertion
cargo clippy --features cubecl -p glyph3d-native

# 3. Full unit tests & WGSL validation (157 tests)
cargo test --workspace

# 4. Pixel-ab golden render gate (9 golden views byte-equal)
cargo run -p glyph -- test render

# 5. Build release binary with CubeCL feature
cargo build --release --features cubecl -p glyph3d-native

# 6. CubeCL hardware verification passes
target/release/glyph3d-native --cubecl-chain-check ./engine/fixtures/ascii-basic.pipe.bin
target/release/glyph3d-native --cubecl-decode-check ./engine/fixtures/ascii-basic.pipe.bin
target/release/glyph3d-native --cubecl-cluster-check ./engine/fixtures/cluster-zwj.pipe.bin
target/release/glyph3d-native --cubecl-repo-check native/fixtures/g-pick-repo

# 7. Flagship benchmark run (CubeCL Layout Engine)
target/release/glyph3d-native --load-repo "$GLYPH_FLAGSHIP_REPO" --repo-engine cubecl --screenshot /tmp/test_flagship_cubecl.png --frames 1
```
