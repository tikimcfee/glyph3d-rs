//! Validate the Visible field's WGSL with naga — the exact compiler wgpu 30
//! bundles — so a broken kernel fails `cargo test` instead of runtime, and
//! pin the shader SET and the struct sizes the Rust side uploads.

use std::path::{Path, PathBuf};

fn shaders_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("shaders")
}

#[test]
fn visible_wgsl_shaders_parse_and_validate() {
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
    assert_eq!(
        names,
        ["visible_cull.wgsl", "visible_layout.wgsl", "visible_wash.wgsl"],
        "shader file set changed"
    );

    // The sizes the host uploads these as (lib.rs / gpu.rs), by struct name.
    let pins: &[(&str, usize)] = &[
        ("ItemGpu", std::mem::size_of::<glyph_field_visible::ItemGpu>()),
        ("ItemParamsGpu", std::mem::size_of::<glyph_field::ItemParamsGpu>()),
        ("LineEntry", std::mem::size_of::<glyph_field_visible::LineEntryGpu>()),
        ("SegmentSeed", std::mem::size_of::<glyph_field_visible::SegmentSeedGpu>()),
        ("ByteSpan", std::mem::size_of::<glyph_field_visible::ByteSpanGpu>()),
        ("Seg", std::mem::size_of::<glyph_field_visible::SegGpu>()),
        ("Wash", std::mem::size_of::<glyph_field_visible::WashGpu>()),
        ("DerivedSlot", std::mem::size_of::<glyph_field_derived::DerivedSlot>()),
        ("Frame", std::mem::size_of::<glyph_field_visible::FrameGpu>()),
        ("TrieMeta", std::mem::size_of::<glyph_field_visible::TrieMetaGpu>()),
        ("LayoutParams", std::mem::size_of::<glyph_field_visible::LayoutParamsGpu>()),
    ];

    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    );
    let mut pinned = 0usize;
    for path in &files {
        let src = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        let module = naga::front::wgsl::parse_str(&src)
            .unwrap_or_else(|e| panic!("WGSL parse error in {}:\n{e}", path.display()));
        validator
            .validate(&module)
            .unwrap_or_else(|e| panic!("WGSL validation error in {}:\n{e:?}", path.display()));
        for (_, ty) in module.types.iter() {
            if let (naga::TypeInner::Struct { span, .. }, Some(name)) = (&ty.inner, &ty.name) {
                if let Some((_, want)) = pins.iter().find(|(n, _)| n == name) {
                    assert_eq!(*span as usize, *want, "{}: WGSL struct {name} size", path.display());
                    pinned += 1;
                }
            }
        }
    }
    assert!(pinned >= pins.len(), "every pinned struct must appear in some shader ({pinned} of {})", pins.len());
}
