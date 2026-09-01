//! Stage H (Phase 1): validate every hand-written WGSL shader under
//! `src/shaders/` with naga — the exact compiler wgpu 30 bundles — so a broken
//! shader fails `cargo test` instead of the next render.
//!
//! This test READS the shaders; it must never modify them (fence 3).

use std::path::{Path, PathBuf};

fn shaders_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/shaders")
}

/// Every `.wgsl` file under `src/shaders/`, sorted for a stable failure order.
fn shader_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(shaders_dir())
        .expect("src/shaders/ must exist")
        .map(|e| e.expect("read_dir entry").path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "wgsl"))
        .collect();
    files.sort();
    files
}

#[test]
fn all_wgsl_shaders_parse_and_validate() {
    let files = shader_files();
    // The known shader set — if a shader is added/removed, update this list so
    // the test can't silently pass over a file it never checked.
    let names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        ["cull.wgsl", "glyph_field.wgsl", "quad_field.wgsl"],
        "shader file set changed — update the test's expected list"
    );

    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    );
    for path in &files {
        let src = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        let module = naga::front::wgsl::parse_str(&src)
            .unwrap_or_else(|e| panic!("WGSL parse error in {}:\n{e}", path.display()));
        validator
            .validate(&module)
            .unwrap_or_else(|e| panic!("WGSL validation error in {}:\n{e}", path.display()));
        eprintln!("naga OK: {}", path.display());
    }
}
