# Rust Engineering Guidelines

This rule enforces the core Rust development principles for the `glyph3d-native` repository, aligning with the late 2025/2026 Rust tooling survey and the project's house rules (`native/AGENTS.md`).
Agents MUST follow these rules when developing in this codebase.

## 1. Static Analysis and Lints
- **Strict Clippy**: We use `clippy::pedantic` as a baseline (configured in `Cargo.toml`).
- **Zero Warnings**: Your code must compile with `cargo clippy --workspace --all-targets --all-features -- -D warnings` returning 0 warnings.
- **Fail-Loud Panics**: This is a binary, not a library. Do not bubble up errors when a hard failure is appropriate. However, do NOT use a bare `.unwrap()` outside of `#[cfg(test)]`. Instead, use `.expect("...")` or `assert!(..., "...")` with a clear diagnostic message explaining *why* it failed.

## 2. Observability (No `println!`)
- **Tracing over Print**: Never use `println!` or `dbg!` for persistent logging. The repository uses `tracing` and `tracing-subscriber`. Use `tracing::info!`, `debug!`, `warn!`, or `error!`.
- **Instrumentation**: Annotate significant functions with `#[tracing::instrument]`. We use `tracing-tracy` for native profiling.

## 3. GPU & Shaders
- **WGSL**: Use `wgsl-analyzer` for your edits if modifying WGSL.
- **Validation**: Ensure WGSL compiles by running `naga-cli` validations if needed, though `cargo test` will validate WGSL if `naga` features are enabled.
- **Byte-Identical Pipeline**: The layout seam (`native/src/layout.rs`) and fold logic are tightly fenced. Any changes to the rendering layout must pass the strict determinism checks (`cargo glyph test --frozen`).

## 4. Testing
- Use `cargo nextest` instead of `cargo test` when interacting manually.
- The project has a rigorous `cargo glyph test` checking everything. Never assume a build is green just from `cargo build`; run `cargo glyph test` (or scoped variants like `cargo glyph test rust`) to verify against the Golden tests.

## 5. Tooling
- We use `mise` to manage tool versions (`rustc`, `wasm-opt`, `just`, etc.) and `just` as our command runner. Do not introduce new shell scripts or `wasm-pack` workflows; use `just` recipes.

## 6. Style
- **Format**: Do not mass-reformat existing code with `cargo fmt`. Match the local style of the file you're editing.
- **Comments**: Comments must explain WHY (empirical findings, bug history, invariants), not what the code does. Add stage tags (e.g. `// Stage M: ...`) for historical context if introducing major architectural changes.
