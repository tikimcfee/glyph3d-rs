//! Products: current, or not — content-hash stamps over declared inputs.

use sha2::{Digest, Sha256};

use crate::build_env;
use crate::manifest::{Artifact, Class, Manifest};
use crate::paths::{expand, root, sh, stamps};

/// A product's currency is the hash of its inputs' CONTENT, not their mtimes.
/// mtime is the wrong signal: a checkout, a stash or a copy moves mtimes
/// without moving content, and the reverse. The build environment
/// (`build_env`: cargo config outside the tree, build variables, toolchain)
/// is hashed with them: cargo builds from it too (2026-11-01).
fn input_digest(a: &Artifact) -> String {
    digest_with(a, build_env::current())
}

fn digest_with(a: &Artifact, env: &str) -> String {
    let mut h = Sha256::new();
    h.update(env.as_bytes());
    h.update([0]);
    for pat in &a.inputs {
        for f in expand(pat) {
            h.update(f.strip_prefix(root()).unwrap_or(&f).to_string_lossy().as_bytes());
            h.update([0]);
            h.update(std::fs::read(&f).unwrap_or_default());
            h.update([0]);
        }
    }
    format!("{:x}", h.finalize())
}

fn stamp_of(name: &str) -> Option<String> {
    std::fs::read_to_string(stamps().join(format!("{name}.sha256"))).ok()
}

/// The stamp, and beside it the environment it was made in as text, so a
/// rebuild the environment caused can say what moved.
fn write_stamp(name: &str, digest: &str) {
    let _ = std::fs::create_dir_all(stamps());
    let _ = std::fs::write(stamps().join(format!("{name}.sha256")), digest);
    let _ = std::fs::write(stamps().join(format!("{name}.env")), build_env::current());
}

/// What moved in the build environment since `name`'s stamp: lines gone
/// (`-`) and lines new (`+`), or nothing when it did not move (or the stamp
/// predates the record).
fn env_moved(name: &str) -> Vec<String> {
    let Ok(was) = std::fs::read_to_string(stamps().join(format!("{name}.env"))) else { return Vec::new() };
    let now = build_env::current();
    let gone = was.lines().filter(|l| !now.lines().any(|n| n == *l)).map(|l| format!("- {l}"));
    let new = now.lines().filter(|l| !was.lines().any(|w| w == *l)).map(|l| format!("+ {l}"));
    gone.chain(new).collect()
}

/// The products that read `file` (repo-relative). A mutation of one of those
/// files, built by its gate under a name the stamp does not cover (`cargo test
/// --release` links the renderer for the integration tests), leaves a MUTATED
/// artifact behind an unchanged input hash once the file is restored.
pub(crate) fn products_reading<'m>(m: &'m Manifest, file: &str) -> Vec<&'m str> {
    let f = root().join(file);
    m.artifact
        .iter()
        .filter(|(_, a)| a.class == Class::Product)
        .filter(|(_, a)| a.inputs.iter().any(|pat| expand(pat).contains(&f)))
        .map(|(name, _)| name.as_str())
        .collect()
}

pub(crate) fn is_current(name: &str, a: &Artifact) -> bool {
    stamp_of(name).as_deref() == Some(input_digest(a).as_str())
        && a.outputs.iter().all(|o| root().join(o).exists())
}

/// `build` achieves currency; `--frozen` only asserts it. Keeping those apart
/// is the whole reason `test` no longer has a gate that quietly rebuilds.
pub(crate) fn ensure_products(m: &Manifest, frozen: bool) -> bool {
    if let Some(note) = build_env::unselected_host_profile() {
        println!("{note}");
    }
    let mut ok = true;
    for (name, a) in m.artifact.iter().filter(|(_, a)| a.class == Class::Product) {
        if is_current(name, a) {
            println!("PASS  {name} current (inputs and build environment unchanged)");
            continue;
        }
        let moved = env_moved(name);
        if !moved.is_empty() {
            println!("NOTE  {name}: the build environment moved since its stamp (cargo config, build variables, toolchain):");
            for l in moved.iter().take(12) {
                println!("        {l}");
            }
        }
        if frozen {
            println!("FAIL  {name} is stale or unbuilt — run `glyph build`.");
            println!("      --frozen asserts currency rather than achieving it, so that a");
            println!("      commit which forgot to rebuild fails here instead of passing.");
            ok = false;
            continue;
        }
        let Some(build) = &a.build else {
            println!("FAIL  {name} has no build command");
            ok = false;
            continue;
        };
        let (good, out) = sh(build, &root());
        if !good {
            println!("FAIL  {name} build errored — every check below would test the wrong artifact");
            for l in out.lines().rev().take(6).collect::<Vec<_>>().iter().rev() {
                println!("      {l}");
            }
            ok = false;
            continue;
        }
        write_stamp(name, &input_digest(a));
        println!("PASS  {name} rebuilt");
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load;

    /// prove re-stales exactly the products a restored file feeds: a renderer
    /// source and a crate source do, a check script does not.
    #[test]
    fn mutated_inputs_name_the_products_they_feed() {
        let m = load().expect("build.toml loads");
        assert_eq!(products_reading(&m, "native/src/main.rs"), ["renderer"]);
        assert_eq!(products_reading(&m, "crates/glyph-field/src/host_slots.rs"), ["renderer"]);
        assert!(products_reading(&m, "tools/check-pick-oracle.sh").is_empty());
    }

    /// A product built under other cargo settings is not current: the build
    /// environment is part of the stamp, so a profile, flag or toolchain
    /// change outside the tree stales it like a source edit does.
    #[test]
    fn the_build_environment_stales_a_product() {
        let m = load().expect("build.toml loads");
        let a = &m.artifact["renderer"];
        assert_eq!(digest_with(a, "env A"), digest_with(a, "env A"));
        assert_ne!(digest_with(a, "env A"), digest_with(a, "env B"), "the build environment does not reach the stamp");
    }
}
