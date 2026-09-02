//! Build script — links the Mojo glyph engine shared library (Stage D).
//!
//! The engine is built OUTSIDE cargo (Mojo has its own toolchain):
//!
//! ```sh
//! pixi run mojo build --fp-mode contract=off -I engine-local \
//!     engine-local/ffi.mojo -o native/libglyph_engine.dylib --emit shared-lib
//! install_name_tool -id @rpath/libglyph_engine.dylib native/libglyph_engine.dylib
//! ```
//!
//! (See engine-local/README-FFI.md. `--fp-mode contract=off` is load-bearing —
//! it pins the bit-exact float discipline; see engine-local/check.sh's header.
//! NOTE: no gate in this tree can currently DETECT a dylib built without it —
//! --engine-check runs with origin (0,0,0), which makes its only fusable
//! multiply-add FMA-invariant. Build via `pixi run build-engine`.)
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
             \x20 pixi run mojo build --fp-mode contract=off -I engine-local \\\n\
             \x20     engine-local/ffi.mojo -o native/libglyph_engine.dylib --emit shared-lib\n\
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
