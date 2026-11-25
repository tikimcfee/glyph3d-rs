//! `glyph test --relax` (2026-11-01): the tree under makepad's relax JIT,
//! with nothing built natively. cargo-test is every test target of the
//! workspace under `relax test --release`, judged by that gate's own rule
//! (`gates::judge_tests`: every target green, then the floor); the renderer
//! gates run `GLYPH_RENDERER` = the renderer under `relax run`.
//!
//! relax is upstream makepad's (`relax` in `libs/relax`), built in a makepad
//! checkout (`cargo build --release -p relax-runner --bin relax`) and named
//! by `GLYPH_RELAX` (default: `relax` on PATH). This file was prove's relax
//! route until mutation testing was retired (2026-11-24); what is left is the
//! part `test --relax` runs, which needs no patch to relax.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use crate::gates::judge_tests;
use crate::manifest::Manifest;
use crate::paths::root;

/// The relax binary: `GLYPH_RELAX`, or `relax` on PATH.
fn relax_bin() -> PathBuf {
    std::env::var_os("GLYPH_RELAX").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("relax"))
}

/// Whether the relax binary runs; Err says what is missing and how to get it.
pub(crate) fn relax_available() -> Result<(), String> {
    let bin = relax_bin();
    match Command::new(&bin).arg("--version").stdin(Stdio::null()).output() {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!("{} --version exited {}", bin.display(), o.status)),
        Err(e) => Err(format!("{} does not run: {e}", bin.display())),
    }
}

/// The workspace's packages, by name and directory: the root manifest's
/// `[workspace].members`, globs expanded. (Not every `Cargo.toml` in the
/// tree: `makepad/` is the makepad host, a crate the root `exclude`s, and
/// running it under relax was red as "not a member" on every tree.)
fn workspace_packages() -> Vec<(String, PathBuf)> {
    let root_manifest = std::fs::read_to_string(root().join("Cargo.toml")).expect("the root Cargo.toml reads");
    let members: Vec<String> = root_manifest
        .parse::<toml::Table>()
        .expect("the root Cargo.toml parses")
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .expect("the root Cargo.toml declares [workspace].members")
        .iter()
        .filter_map(|m| m.as_str().map(str::to_string))
        .collect();
    members
        .iter()
        .flat_map(|m| glob::glob(&root().join(m).join("Cargo.toml").to_string_lossy()).into_iter().flatten().flatten())
        .filter_map(|manifest| {
            let text = std::fs::read_to_string(&manifest).ok()?;
            let name = text.parse::<toml::Table>().ok()?.get("package")?.get("name")?.as_str()?.to_string();
            Some((name, manifest.parent()?.to_path_buf()))
        })
        .collect()
}

/// Every test target of the workspace, as `relax test` selects it: a lib,
/// the bins, each `tests/*.rs` (cargo's own discovery; no package here
/// declares a `[[test]]` or turns discovery off).
fn workspace_test_targets() -> Vec<(String, Vec<String>)> {
    let mut v = Vec::new();
    for (package, dir) in workspace_packages() {
        if dir.join("src/lib.rs").exists() {
            v.push((package.clone(), vec!["--lib".to_string()]));
        }
        let declares_bin = std::fs::read_to_string(dir.join("Cargo.toml")).is_ok_and(|t| t.contains("[[bin]]"));
        if dir.join("src/main.rs").exists() || dir.join("src/bin").is_dir() || declares_bin {
            v.push((package.clone(), vec!["--bins".to_string()]));
        }
        for f in glob::glob(&dir.join("tests/*.rs").to_string_lossy()).into_iter().flatten().flatten() {
            let stem = f.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            v.push((package.clone(), vec!["--test".to_string(), stem]));
        }
    }
    v
}

/// The renderer under relax, as one executable for `GLYPH_RENDERER`: the
/// gates spawn it from their own directories, and relax finds the workspace
/// above them.
pub(crate) fn relax_renderer() -> std::io::Result<PathBuf> {
    let path = root().join("target/glyph-relax/glyph3d-native-relax");
    std::fs::create_dir_all(path.parent().unwrap_or(&root()))?;
    let relax = relax_bin().to_string_lossy().replace('\'', "'\\''");
    std::fs::write(&path, format!("#!/bin/sh\nexec '{relax}' run --quiet --allow-c glyph3d-native -- \"$@\"\n"))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

/// The directory `relax test` runs a package's tests from: the package's
/// own, as cargo runs a test binary (and as the cargo-test gate does), so a
/// test's relative paths mean the same under both. From the workspace root,
/// `tests/oblique_repeat.rs` loaded no `fixtures/g-pick-repo`, rendered an
/// empty frame and failed under relax only (2026-11-01). relax finds the
/// workspace above the package.
fn package_dir(package: &str) -> PathBuf {
    workspace_packages().into_iter().find(|(name, _)| name == package).map_or_else(root, |(_, dir)| dir)
}

/// One `relax test` run of a package's target, from the package's directory
/// (`package_dir`), bounded by `[settings] process_timeout_secs` as the
/// cargo-test gate bounds a test binary: past it the run is killed and fails
/// with a `FAILED ... timed out` line.
///
/// `--release`, as the cargo-test gate runs: relax builds a test under
/// cargo's profile as cargo does (`test`, inheriting `dev`, without it), so
/// with no flag every `debug_assert!` is live under relax and compiled out
/// natively, and the two runs judge different programs. Seen 2026-11-24:
/// `tab_stops_agree_on_every_tier` panicked under relax only, on `pack_row`'s
/// column-page assertion (release saturates it).
fn relax_test(m: &Manifest, package: &str, target: &[String]) -> (bool, String) {
    let mut c = Command::new(relax_bin());
    c.arg("--allow-c").arg("test").arg(package).arg("--release").args(target);
    c.current_dir(package_dir(package));
    let limit = m.settings.process_timeout();
    let o = crate::bounded::run_within(&mut c, limit, true);
    match o.timed_out {
        None => (o.ok, o.out),
        Some(record) => {
            let line = format!("FAILED  relax test {package} {}: timed out after {} s", target.join(" "), limit.as_secs());
            let kept = crate::paths::keep(&format!("relax test {package} timed out"), &format!("{}\n\n{line}\n{record}", o.out));
            (false, format!("{}\n      {line}\n        its output and stacks kept: {kept}\n", o.out))
        }
    }
}

/// The whole suite under relax on the tree as it is, `jobs` targets at once,
/// judged by the cargo-test gate's rule: `glyph test --relax`'s cargo-test.
pub(crate) fn suite(m: &Manifest, jobs: usize) -> (bool, String) {
    let targets = workspace_test_targets();
    let outs: Mutex<Vec<(bool, String)>> = Mutex::new(Vec::new());
    let mut tasks: Vec<&(String, Vec<String>)> = targets.iter().collect();
    tasks.reverse();
    let tasks = Mutex::new(tasks);
    std::thread::scope(|s| {
        for _ in 0..jobs.clamp(1, targets.len().max(1)) {
            s.spawn(|| loop {
                let Some((package, target)) = tasks.lock().unwrap_or_else(|e| e.into_inner()).pop() else { break };
                let (ok, out) = relax_test(m, package, target);
                // A target that fails without a libtest FAILED line (relax
                // refused it, or it died before reporting) would leave the
                // judge's bare `FAIL  cargo test` unexplained: name it, with
                // its last line.
                let out = if ok || out.contains("FAILED") {
                    out
                } else {
                    let last = out.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("(no output)").trim();
                    format!("{out}\n      FAILED  relax test {package} {}: {last}", target.join(" "))
                };
                outs.lock().unwrap_or_else(|e| e.into_inner()).push((ok, out));
            });
        }
    });
    let outs = outs.into_inner().unwrap_or_else(|e| e.into_inner());
    let good = outs.iter().all(|(ok, _)| *ok);
    let out: String = outs.into_iter().map(|(_, o)| o).collect::<Vec<_>>().join("\n");
    // The judge repeats libtest's summary lines; only its verdict is
    // appended, so the run's tests are counted once.
    let (ok, judged) = judge_tests(m, good, &out);
    let verdict: String = judged.lines().filter(|l| !l.contains("test result")).map(|l| format!("{l}\n")).collect();
    (ok, format!("{out}\n{verdict}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The suite runs the workspace's members and nothing else: the makepad
    /// host beside them is `exclude`d, and running it under relax failed the
    /// tree every time (2026-10-31).
    #[test]
    fn workspace_packages_are_the_members_alone() {
        let mut names: Vec<String> = workspace_packages().into_iter().map(|(n, _)| n).collect();
        names.sort();
        assert_eq!(names, ["definition-engine", "glyph", "glyph-field", "glyph-field-derived", "glyph-scene-graph", "glyph3d-native"]);
    }

    /// The suite's targets are cargo's: a bin-only package's bins, and each
    /// integration test by its file.
    #[test]
    fn test_targets_follow_cargo_discovery() {
        let targets = workspace_test_targets();
        assert!(targets.contains(&("glyph".to_string(), vec!["--bins".to_string()])), "a bin-only package");
        assert!(targets.contains(&("glyph3d-native".to_string(), vec!["--test".to_string(), "tab_stops".to_string()])));
    }
}
