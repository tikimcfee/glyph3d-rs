//! Build script — links the Mojo glyph engine shared library (Stage D).
//!
//! The engine is built OUTSIDE cargo (Mojo has its own toolchain):
//!
//! ```sh
//! pixi run mojo build --fp-mode contract=off -I engine \
//!     engine/ffi.mojo -o native/libglyph_engine.dylib --emit shared-lib
//! install_name_tool -id @rpath/libglyph_engine.dylib native/libglyph_engine.dylib
//! ```
//!
//! (See engine/README-FFI.md. `--fp-mode contract=off` is load-bearing —
//! it pins the bit-exact float discipline; see engine/check.sh's header.
//! It is now ENFORCED, not just documented: engine.rs::assert_fp_contract_off
//! calls glyph_engine_fp_probe at every Engine::new() and panics if the dylib
//! fused the multiply-add. Verified against dylibs built both ways. Build via
//! `pixi run build-engine`.)
//!
//! This script only wires up linking: the search path, the dylib, and the two
//! rpaths the resulting binary needs at runtime:
//!   - native/                  for libglyph_engine.dylib itself
//!   - .pixi/envs/default/lib   for libKGENCompilerRTShared.dylib (Mojo runtime)

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let ws = manifest.parent().expect("native/ has a workspace parent");
    let engine_dir = manifest.to_path_buf(); // the dylib lives beside the crate
    let dylib = engine_dir.join("libglyph_engine.dylib");
    let mojo_runtime_lib = ws.join(".pixi/envs/default/lib");

    if !dylib.exists() {
        panic!(
            "libglyph_engine.dylib not found at {}.\n\
             Build it first:\n\
             \x20 pixi run mojo build --fp-mode contract=off -I engine \\\n\
             \x20     engine/ffi.mojo -o native/libglyph_engine.dylib --emit shared-lib\n\
             \x20 install_name_tool -id @rpath/libglyph_engine.dylib native/libglyph_engine.dylib",
            dylib.display()
        );
    }
    assert!(
        mojo_runtime_lib.join("libKGENCompilerRTShared.dylib").exists(),
        "Mojo runtime dylib missing under {} — is the pixi env installed?",
        mojo_runtime_lib.display()
    );

    println!("cargo:rustc-link-search=native={}", engine_dir.display());
    println!("cargo:rustc-link-lib=dylib=glyph_engine");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", engine_dir.display());
    println!(
        "cargo:rustc-link-arg=-Wl,-rpath,{}",
        mojo_runtime_lib.display()
    );

    println!("cargo:rerun-if-changed={}", dylib.display());
}
