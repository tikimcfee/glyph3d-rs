//! Validate the Instanced field's WGSL with naga — the exact compiler wgpu 30
//! bundles — so a broken shader fails `cargo test` instead of the next render.
//! (native/tests/wgsl.rs does the same for native/src/shaders; this shader
//! moved here byte-identically when the field split into modes.)
//!
//! This test READS the shaders; it must never modify them (fence 3).

use std::path::{Path, PathBuf};

fn shaders_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("shaders")
}

#[test]
fn instanced_wgsl_shaders_parse_and_validate() {
    let mut files: Vec<PathBuf> = std::fs::read_dir(shaders_dir())
        .expect("shaders/ must exist")
        .map(|e| e.expect("read_dir entry").path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "wgsl"))
        .collect();
    files.sort();
    let names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["glyph_field.wgsl"], "shader file set changed — update the test's expected list");

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
        // The slot struct the shader binds must be the Rust RenderSlot's size.
        for (_, ty) in module.types.iter() {
            if let (naga::TypeInner::Struct { span, .. }, Some(name)) = (&ty.inner, &ty.name) {
                if name == "InstanceSlot" {
                    assert_eq!(
                        *span as usize,
                        std::mem::size_of::<glyph_field_instanced::RenderSlot>(),
                        "InstanceSlot WGSL struct size must match RenderSlot"
                    );
                }
            }
        }
    }
    // The embedded source is the file on disk.
    assert_eq!(
        glyph_field_instanced::GLYPH_FIELD_WGSL,
        std::fs::read_to_string(shaders_dir().join("glyph_field.wgsl")).unwrap()
    );
}
