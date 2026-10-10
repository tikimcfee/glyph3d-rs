//! Validate the scene graph's WGSL with naga — the compiler wgpu 30 bundles —
//! so a broken kernel fails `cargo test` instead of device creation, and pin
//! the shader set, the entry points and the struct sizes the host uploads.

use std::path::{Path, PathBuf};

#[test]
fn scene_graph_wgsl_parses_validates_and_matches_the_host_layouts() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("shaders");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("shaders/ must exist")
        .map(|e| e.expect("read_dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "wgsl"))
        .collect();
    files.sort();
    let names: Vec<String> = files.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
    assert_eq!(names, ["resolve.wgsl", "scatter.wgsl"], "shader file set changed");

    let pins: &[(&str, usize)] = &[
        ("Similarity", std::mem::size_of::<glyph_scene_graph::Similarity>()),
        ("Appearance", std::mem::size_of::<glyph_scene_graph::Appearance>()),
        ("ResolveParams", std::mem::size_of::<glyph_scene_graph::gpu::ResolveParamsGpu>()),
        ("ScatterParams", std::mem::size_of::<glyph_scene_graph::gpu::ScatterParamsGpu>()),
    ];
    let entry_points: &[(&str, &str)] = &[("resolve.wgsl", "resolve"), ("scatter.wgsl", "scatter")];

    let mut validator = naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all());
    let mut pinned = 0;
    for path in &files {
        let src = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let module = naga::front::wgsl::parse_str(&src).unwrap_or_else(|e| panic!("WGSL parse error in {}:\n{e}", path.display()));
        validator.validate(&module).unwrap_or_else(|e| panic!("WGSL validation error in {}:\n{e:?}", path.display()));
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        let want = entry_points.iter().find(|(f, _)| *f == file).map(|(_, e)| *e).expect("an entry-point pin per file");
        let got: Vec<&str> = module.entry_points.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(got, [want], "{file}: entry points");
        for (_, ty) in module.types.iter() {
            if let (naga::TypeInner::Struct { span, .. }, Some(name)) = (&ty.inner, &ty.name) {
                if let Some((_, size)) = pins.iter().find(|(n, _)| n == name) {
                    assert_eq!(*span as usize, *size, "{file}: WGSL struct {name} size vs the host's");
                    pinned += 1;
                }
            }
        }
    }
    // Similarity and Appearance in resolve.wgsl, the two params structs.
    assert_eq!(pinned, 4, "every pinned struct was found");
}
