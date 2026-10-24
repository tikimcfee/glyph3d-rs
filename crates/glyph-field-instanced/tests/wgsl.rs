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

/// The analytic-footprint form (C35, Apple TBDR only): it must compile, read
/// the viewport at FrameUniform's offset, and take NO hardware derivative:
/// no `dpdx`/`fwidth` and no implicit-gradient sample, because quad-mate
/// lanes are exactly what made the M2's frames depend on GPU timing.
#[test]
fn analytic_footprint_form_validates_and_takes_no_hardware_derivative() {
    let src = glyph_field::analytic_source(glyph_field_instanced::GLYPH_FIELD_WGSL, glyph_field_instanced::QUAD_HEIGHT);
    let module = naga::front::wgsl::parse_str(&src).unwrap_or_else(|e| panic!("analytic WGSL parse error:\n{e}"));
    naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all())
        .validate(&module)
        .unwrap_or_else(|e| panic!("analytic WGSL validation error:\n{e}"));
    assert_eq!(glyph_field_naga_checks::hardware_derivatives(&module), 0, "the analytic form takes a hardware derivative");
    assert_eq!(glyph_field_naga_checks::camera_viewport_offset(&module), Some(80), "Camera.viewport must sit at FrameUniform's offset (80 B)");
    // And the hardware form, the source as written, still takes its one fwidth and one implicit sample.
    let hw = naga::front::wgsl::parse_str(glyph_field_instanced::GLYPH_FIELD_WGSL).unwrap();
    assert_eq!(glyph_field_naga_checks::hardware_derivatives(&hw), 2, "the hardware form's fwidth + textureSample");
}

mod glyph_field_naga_checks {
    /// Derivative expressions plus implicit-gradient image samples, over every function and entry point.
    pub fn hardware_derivatives(module: &naga::Module) -> usize {
        let count = |f: &naga::Function| {
            f.expressions
                .iter()
                .filter(|(_, e)| match e {
                    naga::Expression::Derivative { .. } => true,
                    naga::Expression::ImageSample { level, .. } => matches!(level, naga::SampleLevel::Auto),
                    _ => false,
                })
                .count()
        };
        module.functions.iter().map(|(_, f)| count(f)).sum::<usize>() + module.entry_points.iter().map(|ep| count(&ep.function)).sum::<usize>()
    }

    pub fn camera_viewport_offset(module: &naga::Module) -> Option<u32> {
        module.types.iter().find_map(|(_, ty)| match (&ty.inner, ty.name.as_deref()) {
            (naga::TypeInner::Struct { members, .. }, Some("Camera")) => {
                members.iter().find(|m| m.name.as_deref() == Some("viewport")).map(|m| m.offset)
            }
            _ => None,
        })
    }
}
