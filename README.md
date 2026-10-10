# glyph3d-native

A high-performance pure-Rust (Rust + wgpu) implementation of the glyph3d code-visualization
renderer: it lays out source code as fields of GPU glyphs — a single file, a stress demo, or an
entire repository rendered as a navigable grid of code pages — and draws them with a Slug-style
analytic-coverage renderer.

The engine features **sub-200ms repo load and visual initialization** (~168ms layout / ~177ms total visual init
in flat mode, ~215ms layout / ~226ms visual init in syntax mode for 95.2 million glyph instances across 1,306 files
on Apple Silicon Metal) via parallel cache-blocked CPU layout (`HyperLayout`), intra-file chunking, background
pipelined prepass, burst slot emission, and direct unified-memory arena mapping.

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
 assets/atlas/*.bin
        │
        ▼
 ┌─────────────────────────────────────────────────────────────┐
 │ native/src/layout_hyper.rs  — High-performance Rust layout  │
 │ • CPU-parallel fold & survivors via Rayon cache-blocked scan│
 │ • Intra-file chunking & wrap-aware segmentation for minified│
 │ • Zero-copy direct write into mapped Metal shared memory    │
 │ • ByteSpan semantic token painting for AST / LSP integration│
 └──────────────────────────────┬──────────────────────────────┘
                                │ writes RenderSlot [32B] or DerivedSlot [20B]
                                ▼
 ┌─────────────────────────────────────────────────────────────┐
 │ native/src/glyph_scene/     — Slug WGSL Renderer            │
 │ • crates/glyph-field*: GlyphField trait + one crate per mode│
 │   - Instanced: 32B RenderSlot upload, glyph pipeline, WGSL  │
 │   - Derived: 20B DerivedSlot upload, GPU vertex-stage Y/Z   │
 │   - Visible (exp): no slots; lines in view laid out per frame│
 │ • pipelines.rs: composite & selection tint pipelines        │
 │ • render.rs: Frustum/LOD CPU culling, multi-pass rendering  │
 └─────────────────────────────────────────────────────────────┘
```

- **`native/src/layout_hyper.rs`** (Rust): Cache-blocked parallel CPU layout engine. Computes line wrapping,
  indentation, blank/missing glyph filtering, and coordinates in CPU cache, writing 32-byte `RenderSlot`
  or 20-byte `DerivedSlot` instances directly into mapped GPU shared memory.
- **`native/src/glyph_scene/`** (Rust, wgpu 30 / winit 0.30 / glam 0.33 / egui 0.36):
  Decomposed into modular submodules:
  - `setup.rs`: scene construction; builds the glyph field for the chosen `--field-mode` (`instanced`,
    `derived`, or the experimental `visible`, which keeps no slot per glyph and lays out the lines in
    view per frame on the GPU — `out/VISIBLE-MODE.md`).
  - `pipelines.rs`: composite state and the selection tint pipeline.
  - `render.rs`: Frame render pass orchestration, two-level CPU frustum/LOD culling, backdrop quad pass,
    glyph field pass, and fullscreen composite pass.
- **`native/src/layout/span.rs`**: ByteSpan token painting for AST/LSP integration (`Paint::ByteSpans`),
  enabling high-performance byte-range syntax colorization without string copies.
- **`assets/atlas/`**: Prebaked glyph geometry (curves, glyph map, font table, codepoint trie)
  exported verbatim from the Slug atlas. Byte format: `assets/atlas/FORMAT.md`.
- **`build.toml`**: Declarative gate, artifact, and mutation verification graph, executed by `glyph`.

## Quickstart

Requirements: macOS/Apple Silicon or Linux x86_64, a recent Rust toolchain (MSRV 1.95).

```sh
# Build and run the native binary
cargo build --release -p glyph3d-native
cargo run --release -p glyph3d-native

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
# 1. Run all unit and integration tests (222 tests across 13 binaries)
cargo test --workspace

# 2. Validate build.toml gates and mutation blocks
cargo run -p glyph -- validate

# 3. Verify pixel-exact golden rendering on flagship corpus
cargo run --release -p glyph3d-native -- --load-repo /path/to/glyph3d-js --screenshot /tmp/test.png
shasum -a 256 /tmp/test.png
# Expected: 7957dc62b473e64c5e35c9184811554b101d0bf3e40b3b7cdc7f95dab988c6d5

# 4. Amortized multi-run performance benchmarking
python3 tools/bench_hyper.py --repo /path/to/glyph3d-js -n 5 --color-mode flat
```

## Repo Map

| Path | What it is |
|---|---|
| `native/` | The pure-Rust renderer and layout engine binary. Contracts in `native/src/*.rs` |
| `native/src/layout_hyper.rs` | HyperLayout: parallel CPU layout engine into mapped unified memory |
| `native/src/glyph_scene/` | Modularized Slug WGSL renderer: `setup.rs`, `pipelines.rs`, `render.rs` |
| `crates/glyph-field*` | The glyph field by render mode: mode-neutral contract (`glyph-field`), Instanced 32B mode (`glyph-field-instanced`), and Derived 20B mode (`glyph-field-derived`) |
| `glyph/` | The verification and mutation runner (`cargo run -p glyph -- validate`) |
| `tools/` | Verification scripts, generators, and `bench_hyper.py` performance harness |
| `.agents/` | Agent guidelines, house rules (`rules/rust-engineering.md`), and testing skill (`skills/glyph-engine-testing/SKILL.md`) |
| `assets/atlas/` | Prebaked glyph-geometry binaries + `FORMAT.md` |
| `schema/` | `glyph-identity.json` — single source of truth for buffer/lane layouts |
| `research/` | GPU architecture studies, web target notes, and `desktop-platform-audit.md` |
| `out/` | Golden baselines (`tooling-ab/baseline/`), proof PNGs, and historical stage reports |
