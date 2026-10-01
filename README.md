# glyph3d-native

A high-performance pure-Rust (Rust + wgpu) implementation of the glyph3d code-visualization
renderer: it lays out source code as fields of GPU glyphs — a single file, a stress demo, or an
entire repository rendered as a navigable grid of code pages — and draws them with a Slug-style
analytic-coverage renderer.

The engine features **sub-second repo load and layout** (~0.57s layout / ~0.93s total visual init
for 95.2 million glyph instances across 1,306 files on Apple Silicon) via parallel CPU scan/fold
and direct unified-memory arena streaming (`HyperLayout`), with zero external C-ABI dylib dependencies.

The defining property of this tree is **bit-exactness**: the pure-Rust layout engine reproduces
canonical layouts bit-for-bit, the Rust renderer's offscreen output is byte-deterministic,
and a comprehensive gate suite proves both on every commit. Refactors are output-neutral by contract,
verified by gates and golden SHA-256 screenshot hashes rather than by argument.

Platform: macOS on Apple Silicon (`osx-arm64`; Metal) and Linux x86_64 (`linux-64`; Vulkan).

## Architecture

```
            schema/glyph-identity.json          (vendored, hash-pinned)
                      │ tools/gen_schema.py
                      ▼
 assets/atlas/*.bin   engine-trie.bin
        │                    │
        ▼                    ▼
 ┌─────────────────────────────────────────────────────────────┐
 │ native/src/layout_hyper.rs  — High-performance Rust layout  │
 │ • CPU-parallel fold & survivors via Rayon cache-blocked scan│
 │ • Zero-copy direct write into mapped Metal shared memory    │
 │ • Optional CubeCL compute engine (--features cubecl)        │
 │ • ByteSpan semantic token painting for AST / LSP integration│
 └──────────────────────────────┬──────────────────────────────┘
                                │ writes RenderSlot [32B]
                                ▼
 ┌─────────────────────────────────────────────────────────────┐
 │ native/src/glyph_scene/     — Slug WGSL Renderer            │
 │ • buffers.rs: Mapped instance arena & zero-copy upload      │
 │ • pipelines.rs: Glyph analytic-coverage & composite pipeline│
 │ • render.rs: Frustum/LOD CPU culling, multi-pass rendering  │
 └─────────────────────────────────────────────────────────────┘
```

- **`native/src/layout_hyper.rs`** (Rust): Single-pass parallel layout engine. Computes line wrapping,
  indentation, blank/missing glyph filtering, and coordinates in CPU cache, writing 32-byte `RenderSlot`
  instances directly into mapped GPU shared memory.
- **`native/src/glyph_scene/`** (Rust, wgpu 30 / winit 0.30 / glam 0.33 / egui 0.36):
  Decomposed into modular submodules:
  - `buffers.rs`: Mapped instance arena allocation (`MTLStorageModeShared`) and unified memory transcoding.
  - `pipelines.rs`: Slug analytic-coverage render pipelines, selection mask/tint pipelines, and composite state.
  - `render.rs`: Frame render pass orchestration, two-level CPU frustum/LOD culling, backdrop quad pass,
    glyph field pass, and fullscreen composite pass.
- **`native/src/layout/span.rs`**: ByteSpan token painting for AST/LSP integration (`Paint::ByteSpans`),
  enabling high-performance byte-range syntax colorization without string copies.
- **`assets/atlas/`**: Prebaked glyph geometry (curves, glyph map, font table, codepoint trie)
  exported verbatim from the Slug atlas, plus `engine-trie.bin`. Byte format: `assets/atlas/FORMAT.md`.
- **`build.toml`**: Declarative gate, artifact, and mutation verification graph, executed by `glyph`.

## Quickstart

Requirements: macOS/Apple Silicon or Linux x86_64, a recent Rust toolchain (MSRV 1.95).

```sh
# Build and run the native binary
cargo build --release -p glyph3d-native
cargo run --release -p glyph3d-native

# Optional: enable experimental CubeCL compute kernels
cargo check --features cubecl
```

### Running the Renderer

Binary is `target/release/glyph3d-native` (or `cargo run --release -p glyph3d-native -- [ARGS]`).

| What | Command |
|---|---|
| Windowed text field (default: this crate's own `main.rs`) | `cargo run --release -p glyph3d-native` |
| 1M-quad stress demo | `cargo run --release -p glyph3d-native -- --demo` |
| Render a specific file | `cargo run --release -p glyph3d-native -- --render-file <path>` |
| A whole repo as a glyph field (default: `HyperLayout`) | `cargo run --release -p glyph3d-native -- --load-repo <dir> [--focus-file <substr>]` |
| Deterministic offscreen render → PNG | `cargo run --release -p glyph3d-native -- --load-repo <dir> --screenshot out/shot.png` |

Windowed controls: WASD/E/R/Q/F fly camera, right-drag look, left-click pick,
`h/g/t/x` edit verbs, F1 toggles the egui debug panel, F2 saves a screenshot
to `out/windowed-shot-<utc-stamp>.png`.

## Verification

The test and validation battery:

```sh
# 1. Run all unit and integration tests (106 tests + WGSL validation)
cargo test --workspace

# 2. Validate build.toml gates and mutation blocks
cargo run -p glyph -- validate

# 3. Verify pixel-exact golden rendering on flagship corpus
cargo run --release -p glyph3d-native -- --load-repo /path/to/glyph3d-js --screenshot /tmp/test.png
shasum -a 256 /tmp/test.png
# Expected: 7957dc62b473e64c5e35c9184811554b101d0bf3e40b3b7cdc7f95dab988c6d5
```

## Repo Map

| Path | What it is |
|---|---|
| `native/` | The pure-Rust renderer and layout engine binary. Contracts in `native/src/*.rs` |
| `native/src/layout_hyper.rs` | HyperLayout: sub-second parallel CPU layout into mapped shared memory |
| `native/src/glyph_scene/` | Modularized Slug WGSL renderer: `buffers.rs`, `pipelines.rs`, `render.rs` |
| `native/src/cubecl_*.rs` | Decoupled CubeCL GPU compute kernels (gated behind `[features] cubecl`) |
| `glyph/` | The verification and mutation runner (`cargo run -p glyph -- validate`) |
| `assets/atlas/` | Prebaked glyph-geometry binaries + `engine-trie.bin` + `FORMAT.md` |
| `schema/` | `glyph-identity.json` — single source of truth for buffer/lane layouts |
| `out/` | Golden baselines (`tooling-ab/baseline/`), proof PNGs, and historical stage reports |
