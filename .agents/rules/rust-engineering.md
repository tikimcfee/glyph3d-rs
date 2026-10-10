# Rust Engineering Guidelines

This rule states the Rust development principles for the `glyph3d-native` repository and the project's house rules (`native/AGENTS.md`). It describes how the code IS written; where a line here is a goal the tree does not meet yet, it says so.
Agents MUST follow these rules when developing in this codebase. Facts about
the checks themselves (what each gate runs and cannot see) live in the root
`AGENTS.md`; where this file and that one disagree, `AGENTS.md` wins.

## 1. Static Analysis and Lints
- **Zero Warnings**: the battery's `cargo-clippy` gate runs `cargo clippy --release --all-targets` (default lint set, tests included) and is red on any warning; `cargo-build` and `cargo-doc` are zero-warning too. `clippy::pedantic` is NOT configured. An `#[allow(...)]` needs a comment saying why.
- **Fail-Loud Panics**: This is a binary, not a library. Do not bubble up errors when a hard failure is appropriate. However, do NOT use a bare `.unwrap()` outside of `#[cfg(test)]`. Instead, use `.expect("...")` or `assert!(..., "...")` with a clear diagnostic message explaining *why* it failed.

## 2. Observability
- **Logging**: diagnostics go through `log`/`tracing` (`log::info!`, `tracing::info!`, ...; `tracing-log` routes `log` records into `tracing-subscriber`, filtered by `GLYPH_TRACE`, else `RUST_LOG`). Not `dbg!`.
- **`println!` is for OUTPUT that is a contract**: the `glyph` runner's verdict lines, the renderer's check instruments' reports (`PASS`/`FAIL` lines a gate matches on), stats lines a tool parses (`tools/bench_hyper.py` reads the `phases:` line), and self-test/debug prints opted into by an env var. The tree has many of these; a NEW diagnostic that is not a contract should be a log record.
- **Spans**: the load path is instrumented with `tracing` spans (`repo.*`, `hyper.*`; `native/AGENTS.md` § Debug env vars). GPU pass timings: `GLYPH_PROFILE=1` (wgpu-profiler). There is no tracy integration.

## 3. GPU & Shaders
- **WGSL**: Use `wgsl-analyzer` for your edits if modifying WGSL.
- **Validation**: `cargo test` validates every shader with the naga wgpu bundles (`native/tests/wgsl.rs` and each field-mode crate's `tests/wgsl.rs`); that pins the shader set, not what it draws. Only pixel-ab sees pixels.
- **Byte-Identical Pipeline**: The layout seam (`native/src/layout.rs`) and fold logic are tightly fenced. A change to layout or rendering must leave every golden byte-equal (`cargo glyph test`), or re-baseline on the owner's say-so with every moved pixel attributed.

## 4. Testing
- `cargo nextest run --workspace` (`just test`) is fine for a fast manual loop.
- Green means `cargo glyph test`, never `cargo build`. `cargo glyph test rust` covers the compiler, lints, docs and tests but no pixels; `cargo glyph test render` is the golden check. The testing skill (`.agents/skills/glyph-engine-testing/SKILL.md`) has the procedure.

## 5. Tooling
- We use `mise` to manage tool versions (`rustc`, `just`, etc.). The entry point is `cargo glyph` (`build.toml` declares every gate); `just` recipes are thin doors onto it. Do not add new ad-hoc shell entry points or `wasm-pack` workflows: a new check is a gate in `build.toml` (several existing gates are `tools/*.sh` scripts the runner calls).

## 6. Style
- **Format**: Do not mass-reformat existing code with `cargo fmt`. Match the local style of the file you're editing.
- **Comments**: Comments must explain WHY (empirical findings, bug history, invariants), not what the code does. Do NOT add stage letters (`// Stage M: ...`): that convention is retired (root `AGENTS.md` § Vocabulary). Name the substance and date it ("since the layout seam", "C28, 2026-10-10"); existing stage tags are archaeology, replaced by the substance when you touch them.

## 7. Descriptive Naming over Terse Density
- **No Cryptic Abbreviations**: Low-level systems and GPU programming is not an excuse for variable density. Use clear, self-documenting full names:
  - `glyph_advance_widths` instead of `sm` or `adv`
  - `glyph_indices` instead of `gi`
  - `glyph_flags` instead of `fl`
  - `threads_per_workgroup` instead of `units`
  - `bytes_per_thread` instead of `rake`
  - `survivor_ordinal` instead of `sv_`
  - `active_cell_advance_bits` instead of `cell_bits`
- **Readability Across Agents & Humans**: Code is read far more often than it is written. Eliminating acronym mapping eliminates bugs.

## 8. GPU Float Determinism & Kernel Safety
These bind the WGSL compute kernels (today the visible field's cull and layout, `crates/glyph-field-visible/shaders/`, held to Pass 2 by hyper-oracle's visible tier) and any CPU parallel pass.
- **Strict Float Addition Order**: Floating-point addition is non-associative: `(a + b) + c != a + (b + c)`. Any kernel that sums layout coordinates or advances MUST accumulate in strict left-to-right order from the segment head (or from a seed Pass 1 computed that way) to stay bit-exact with the CPU reference folds.
- **Deterministic order**: a GPU pass whose output order reaches the screen (draw order, a capped prefix) must not take that order from atomics: reserve with a prefix scan in index order (C30, 2026-10-10: atomic slot order made visible frames differ run to run).
- **Bounded Kernel Loops & Hang Prevention**: Because workgroups cover fixed spans while corpora end at arbitrary byte offsets, all backward and forward walks across shared or global memory MUST be strictly bounded (`global_byte_index < total_bytes`, `start_byte_index >= 0`). Never cast negative loop indices to `usize`.
- **Surgical Edits**: Never use blind regex or unanchored bulk search-and-replace across Rust source files. Every edit is targeted (an exact match, asserted to occur where intended), and `cargo check --workspace` runs after it.

## 9. Feature Flags & Binary Defaults
- Keep primary features (`egui-ui`, `launcher`) in the `default` features of `native/Cargo.toml` so `cargo glyph run`, `cargo test` and the launcher work out of the box. A feature-gated item used outside its gate breaks the `--no-default-features` build, which the `cargo-check-no-ui` gate compiles.

## 10. Named Values Live in Configuration
- Every named, tunable value — colors, backgrounds, spacings, speeds, distances, fade ranges — lives in `config/defaults.toml` (compiled in), overridable per key from a `[section]` of `launch_config.toml` at runtime. Not as a Rust or WGSL literal. Read it through `crate::config::settings()`; shaders receive it through a uniform.
- **Contracts stay compiled.** Layout metrics (`CELL_HEIGHT_WORLD`, the line-height factor), slot and atlas formats, CPU/shader shared constants (`MAX_CURVES`, `TEX_W`, `GROUP_STRIDE`), and anything a reference check or fixture was computed against are not settings: a config edit must never be able to silently disagree with an oracle. If one ever needs to become configurable, that is its own change, with the fixtures it touches.
- **Defaults are complete.** Every settings field is required, so a key missing from `defaults.toml` is a parse failure, not a zero. Add the key there in the same commit that reads it.
- **Moving a literal is byte-neutral or it is a re-baseline.** TOML floats parse decimal → f64 → f32, which can land one ulp from a decimal → f32 literal. Pin each migrated value in `config::tests::defaults_match_migrated_literals`, and byte-compare the golden views.
- UI copy, log and error messages stay in code — localization is different machinery.

