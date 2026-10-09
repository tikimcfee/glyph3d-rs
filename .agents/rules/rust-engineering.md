# Rust Engineering Guidelines

This rule enforces the core Rust development principles for the `glyph3d-native` repository, aligning with the late 2025/2026 Rust tooling survey and the project's house rules (`native/AGENTS.md`).
Agents MUST follow these rules when developing in this codebase. Facts about
the checks themselves (what each gate runs and cannot see) live in the root
`AGENTS.md`; where this file and that one disagree, `AGENTS.md` wins.

## 1. Static Analysis and Lints
- **Zero Warnings**: the battery's `cargo-clippy` gate runs `cargo clippy --release --all-targets` (default lint set, tests included) and is red on any warning; `cargo-build` and `cargo-doc` are zero-warning too. `clippy::pedantic` is NOT configured. An `#[allow(...)]` needs a comment saying why.
- **Fail-Loud Panics**: This is a binary, not a library. Do not bubble up errors when a hard failure is appropriate. However, do NOT use a bare `.unwrap()` outside of `#[cfg(test)]`. Instead, use `.expect("...")` or `assert!(..., "...")` with a clear diagnostic message explaining *why* it failed.

## 2. Observability (No `println!`)
- **Tracing over Print**: Never use `println!` or `dbg!` for diagnostics or persistent logging. The repository uses `tracing` and `tracing-subscriber`. Use `tracing::info!`, `debug!`, `warn!`, or `error!`. The exception is OUTPUT that is a contract: the `glyph` runner's verdict lines and the renderer's check instruments' reports (`PASS`/`FAIL` lines a gate matches on) go to stdout.
- **Instrumentation**: Annotate significant functions with `#[tracing::instrument]`. We use `tracing-tracy` for native profiling.

## 3. GPU & Shaders
- **WGSL**: Use `wgsl-analyzer` for your edits if modifying WGSL.
- **Validation**: Ensure WGSL compiles by running `naga-cli` validations if needed, though `cargo test` will validate WGSL if `naga` features are enabled.
- **Byte-Identical Pipeline**: The layout seam (`native/src/layout.rs`) and fold logic are tightly fenced. Any changes to the rendering layout must pass the strict determinism checks (`cargo glyph test --frozen`).

## 4. Testing
- Use `cargo nextest` instead of `cargo test` when interacting manually.
- The project has a rigorous `cargo glyph test` checking everything. Never assume a build is green just from `cargo build`; run `cargo glyph test` (or scoped variants like `cargo glyph test rust`) to verify against the Golden tests.

## 5. Tooling
- We use `mise` to manage tool versions (`rustc`, `just`, etc.). The entry point is `cargo glyph` (`build.toml` declares every gate); `just` recipes are thin doors onto it. Do not add new ad-hoc shell entry points or `wasm-pack` workflows: a new check is a gate in `build.toml` (several existing gates are `tools/*.sh` scripts the runner calls).

## 6. Style
- **Format**: Do not mass-reformat existing code with `cargo fmt`. Match the local style of the file you're editing.
- **Comments**: Comments must explain WHY (empirical findings, bug history, invariants), not what the code does. Add stage tags (e.g. `// Stage M: ...`) for historical context if introducing major architectural changes.

## 7. Descriptive Naming over Terse Density
- **No Cryptic Abbreviations**: Low-level systems and GPU programming is not an excuse for variable density. Use clear, self-documenting full names:
  - `glyph_advance_widths` instead of `sm` or `adv`
  - `glyph_indices` instead of `gi`
  - `glyph_flags` instead of `fl`
  - `threads_per_cube` instead of `units`
  - `bytes_per_thread` instead of `rake`
  - `survivor_ordinal` instead of `sv_`
  - `active_cell_advance_bits` instead of `cell_bits`
- **Readability Across Agents & Humans**: Code is read far more often than it is written. Eliminating acronym mapping eliminates bugs.

## 8. GPU Float Determinism & Kernel Safety
- **Strict Float Addition Order**: Floating-point addition is non-associative: `(a + b) + c != a + (b + c)`. All GPU compute kernels that sum layout coordinates or advances MUST accumulate in strict left-to-right order from the segment head to maintain bit-exact parity with CPU reference folds.
- **Bounded Kernel Loops & Hang Prevention**: Because GPU tiles have fixed threadgroup dimensions (e.g. 2048 bytes) while corpora end at arbitrary byte offsets, all backward and forward walks across shared or global memory MUST be strictly bounded (`global_byte_index < total_bytes`, `start_byte_index >= 0`). Never cast negative loop indices to `usize`.
- **Surgical Edits**: Never use blind regex or unanchored bulk search-and-replace across Rust source files. Every edit is targeted (an exact match, asserted to occur where intended), and `cargo check --workspace` runs after it.

## 9. Feature Flags & Binary Defaults
- Keep primary layout and render engines (e.g. `cubecl`) in the `default` features of `native/Cargo.toml` so that standard `cargo run`, `cargo test`, and `glyph tui` invocations work out-of-the-box without requiring manual feature flags.

## 10. Named Values Live in Configuration
- Every named, tunable value — colors, backgrounds, spacings, speeds, distances, fade ranges — lives in `config/defaults.toml` (compiled in), overridable per key from a `[section]` of `launch_config.toml` at runtime. Not as a Rust or WGSL literal. Read it through `crate::config::settings()`; shaders receive it through a uniform.
- **Contracts stay compiled.** Layout metrics (`CELL_HEIGHT_WORLD`, the line-height factor), slot and atlas formats, CPU/shader shared constants (`MAX_CURVES`, `TEX_W`, `GROUP_STRIDE`), and anything a reference check or fixture was computed against are not settings: a config edit must never be able to silently disagree with an oracle. If one ever needs to become configurable, that is its own change, with the fixtures it touches.
- **Defaults are complete.** Every settings field is required, so a key missing from `defaults.toml` is a parse failure, not a zero. Add the key there in the same commit that reads it.
- **Moving a literal is byte-neutral or it is a re-baseline.** TOML floats parse decimal → f64 → f32, which can land one ulp from a decimal → f32 literal. Pin each migrated value in `config::tests::defaults_match_migrated_literals`, and byte-compare the golden views.
- UI copy, log and error messages stay in code — localization is different machinery.

