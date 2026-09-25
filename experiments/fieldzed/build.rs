// fieldzed's link-setup: mirror of native/build.rs's rpath emission.
//
// `cargo:rustc-link-arg` does NOT propagate to dependent packages — the
// repo's glyph3d-native binary gets the engine rpaths, but THIS package's
// binary links libglyph_engine without them and dies at dyld ("no
// LC_RPATH's found"). The paths are derived the same way native/build.rs
// derives them; that file is the source of truth for the dylib contract
// (staleness probe, pixi env layout) — this one deliberately does only the
// minimum: assert the files exist, emit link-search + the two rpaths.
fn main() {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("fieldzed sits at <repo>/experiments/fieldzed");
    let native = repo.join("native");
    let ext = match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("macos") => "dylib",
        _ => "so",
    };
    let engine = native.join(format!("libglyph_engine.{ext}"));
    let mojo_rt = repo.join(".pixi/envs/default/lib");
    assert!(engine.exists(), "missing {} — run pixi run build-engine", engine.display());
    assert!(
        mojo_rt.join(format!("libKGENCompilerRTShared.{ext}")).exists(),
        "missing Mojo runtime under {} — is the pixi env installed?",
        mojo_rt.display()
    );
    println!("cargo:rustc-link-search=native={}", native.display());
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", native.display());
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", mojo_rt.display());
    println!("cargo:rerun-if-changed={}", engine.display());
}
