//! Products: current, or not — content-hash stamps over declared inputs.

use std::collections::BTreeMap;
use std::path::Path;

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

/// Each input's content hash, by repo-relative path: the record kept beside
/// the stamp of what a product was last built from.
fn input_hashes(a: &Artifact) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for pat in &a.inputs {
        for f in expand(pat) {
            let rel = f.strip_prefix(root()).unwrap_or(&f).to_string_lossy().into_owned();
            out.insert(rel, format!("{:x}", Sha256::digest(std::fs::read(&f).unwrap_or_default())));
        }
    }
    out
}

fn record_path(name: &str) -> std::path::PathBuf {
    stamps().join(format!("{name}.inputs"))
}

/// The record as text, one `<hash>  <path>` line per input.
fn record_text(hashes: &BTreeMap<String, String>) -> String {
    hashes.iter().map(|(path, hash)| format!("{hash}  {path}\n")).collect()
}

fn parse_record(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once("  "))
        .map(|(hash, path)| (path.to_string(), hash.to_string()))
        .collect()
}

fn record_of(name: &str) -> Option<BTreeMap<String, String>> {
    std::fs::read_to_string(record_path(name)).ok().map(|t| parse_record(&t))
}

/// The inputs that are not what the product was last built from: their
/// content differs from the record, or the record does not name them, or
/// there is no record (every input, then: nothing says which moved).
fn changed_inputs(record: Option<&BTreeMap<String, String>>, now: &BTreeMap<String, String>) -> Vec<String> {
    now.iter()
        .filter(|(path, hash)| record.and_then(|r| r.get(*path)) != Some(*hash))
        .map(|(path, _)| path.clone())
        .collect()
}

/// Each file under `base` named in `files`, its modification time moved to
/// now; returns the ones that could not be moved, with why.
///
/// Cargo decides a unit is fresh when no source it read is NEWER than its
/// last build, by mtime alone. A file put back with an older mtime than the
/// build (`rsync -a`, `cp -p`, `tar x`, some checkouts and stashes) differs
/// from what was built and reads fresh: the product's stamp said "changed,
/// rebuild", cargo found nothing to do, and the stamp then vouched for a
/// binary built from the other content (2026-11-01, monolith1: a test
/// binary kept a branch's scratch tests that way). Moving the changed
/// inputs' mtimes to now before the build makes cargo see what the content
/// hash saw. Only the changed ones: cargo rebuilds what reads them and no
/// more, which is what an edit would have cost.
fn touch(base: &Path, files: &[String]) -> Vec<String> {
    let now = std::time::SystemTime::now();
    files
        .iter()
        .filter_map(|f| {
            let moved = std::fs::File::options().write(true).open(base.join(f)).and_then(|h| h.set_modified(now));
            moved.err().map(|e| format!("{f}: {e}"))
        })
        .collect()
}

/// Before a build: the inputs under `base` that are not what the record
/// says was last built, each touched (`touch`). Returns them, and the ones
/// that could not be touched.
fn bring_forward(base: &Path, record: Option<&BTreeMap<String, String>>, now: &BTreeMap<String, String>) -> (Vec<String>, Vec<String>) {
    let changed = changed_inputs(record, now);
    let unmoved = touch(base, &changed);
    (changed, unmoved)
}

/// Drop `name`'s stamp and forget `file` in its record, so the next build
/// moves the file's mtime whatever it is: prove, after restoring a file a
/// gate may have built a mutant from, outside the record's knowledge.
pub(crate) fn forget(name: &str, file: &str) {
    let _ = std::fs::remove_file(stamps().join(format!("{name}.sha256")));
    if let Some(mut record) = record_of(name) {
        record.remove(file);
        let _ = std::fs::write(record_path(name), record_text(&record));
    }
}

fn stamp_of(name: &str) -> Option<String> {
    std::fs::read_to_string(stamps().join(format!("{name}.sha256"))).ok()
}

/// The stamp, and beside it the environment it was made in as text, so a
/// rebuild the environment caused can say what moved, and the record of
/// each input's content, so the next rebuild knows which inputs moved.
fn write_stamp(name: &str, digest: &str, hashes: &BTreeMap<String, String>) {
    let _ = std::fs::create_dir_all(stamps());
    let _ = std::fs::write(stamps().join(format!("{name}.sha256")), digest);
    let _ = std::fs::write(stamps().join(format!("{name}.env")), build_env::current());
    let _ = std::fs::write(record_path(name), record_text(hashes));
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

/// Current: the stamp is this content in this environment, and the record
/// of what it was built from is there. A stamp with no record was written
/// by a runner that did not move mtimes, and may vouch for a binary cargo
/// never rebuilt; it is rebuilt once, every input touched.
pub(crate) fn is_current(name: &str, a: &Artifact) -> bool {
    stamp_of(name).as_deref() == Some(input_digest(a).as_str())
        && record_path(name).is_file()
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
        // What is about to be built, taken BEFORE the build: an input edited
        // while cargo runs then reads stale on the next run, where a digest
        // taken after would vouch for content cargo may never have read.
        let digest = input_digest(a);
        let hashes = input_hashes(a);
        let (changed, unmoved) = bring_forward(&root(), record_of(name).as_ref(), &hashes);
        if !changed.is_empty() {
            println!(
                "NOTE  {name}: {} input(s) differ from its last build; their mtimes move to now, so cargo rebuilds what reads them",
                changed.len()
            );
        }
        if !unmoved.is_empty() {
            println!("FAIL  {name}: an input that changed could not be touched, so cargo may call it fresh:");
            for l in unmoved.iter().take(6) {
                println!("      {l}");
            }
            ok = false;
            continue;
        }
        let (good, out) = sh(build, &root());
        if !good {
            println!("FAIL  {name} build errored — every check below would test the wrong artifact");
            for l in out.lines().rev().take(6).collect::<Vec<_>>().iter().rev() {
                println!("      {l}");
            }
            ok = false;
            continue;
        }
        write_stamp(name, &digest, &hashes);
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

    /// A changed input goes back to cargo NEWER than any build, whatever
    /// mtime it came back with: two sources an hour old, one put back with
    /// content other than the record's (as `rsync -a` or `cp -p` restores a
    /// file), one as recorded. The changed one's mtime moves to now; the
    /// unchanged one keeps its hour, so cargo rebuilds what reads the first
    /// and nothing more. A path the record does not name counts as changed,
    /// and with no record every input does. (2026-11-01: cargo, judging by
    /// mtime, called such a file fresh, and a test binary kept a branch's
    /// scratch tests behind a stamp reading current.)
    #[test]
    fn an_input_put_back_with_an_older_mtime_is_touched_before_the_build() {
        let t = std::env::temp_dir().join(format!("glyph-touch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(t.join("src")).unwrap();
        std::fs::write(t.join("src/changed.rs"), "fn restored() {}\n").unwrap();
        std::fs::write(t.join("src/same.rs"), "fn same() {}\n").unwrap();
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for f in ["src/changed.rs", "src/same.rs"] {
            std::fs::File::options().write(true).open(t.join(f)).unwrap().set_modified(hour_ago).unwrap();
        }
        let hash = |s: &str| format!("{:x}", Sha256::digest(s.as_bytes()));
        let now: BTreeMap<String, String> =
            [("src/changed.rs", "fn restored() {}\n"), ("src/same.rs", "fn same() {}\n")].iter().map(|(p, c)| (p.to_string(), hash(c))).collect();
        let record = parse_record(&record_text(
            &[("src/changed.rs", "fn edited() {}\n"), ("src/same.rs", "fn same() {}\n")].iter().map(|(p, c)| (p.to_string(), hash(c))).collect(),
        ));

        let before = std::time::SystemTime::now() - std::time::Duration::from_secs(1);
        let (changed, unmoved) = bring_forward(&t, Some(&record), &now);
        assert_eq!(changed, ["src/changed.rs"]);
        assert!(unmoved.is_empty(), "the changed input could not be touched: {unmoved:?}");
        let mtime = |f: &str| std::fs::metadata(t.join(f)).unwrap().modified().unwrap();
        let (moved, kept) = (mtime("src/changed.rs"), mtime("src/same.rs"));
        let _ = std::fs::remove_dir_all(&t);
        assert!(moved >= before, "the changed input kept its older mtime, so cargo would call it fresh");
        assert!(kept < before, "an unchanged input was touched: cargo would rebuild for nothing");

        let mut unnamed = record.clone();
        unnamed.remove("src/same.rs");
        assert_eq!(changed_inputs(Some(&unnamed), &now), ["src/changed.rs", "src/same.rs"], "a path the record does not name");
        assert_eq!(changed_inputs(None, &now).len(), 2, "with no record, every input");
    }
}
