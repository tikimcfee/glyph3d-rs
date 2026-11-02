//! The build environment: what cargo builds a product FROM besides its
//! declared inputs (2026-11-01).
//!
//! A product's stamp hashes its inputs' content, and the inputs are files in
//! this tree. But cargo also reads the user's cargo config (`$CARGO_HOME`,
//! every `.cargo/config.toml` above the checkout, and what each `include`s,
//! such as a host's build profile in `.cargo/host.toml`), the
//! `CARGO_PROFILE_*`, `CARGO_TARGET_*`, `RUSTFLAGS`... variables, and the
//! toolchain. A change to any of those rebuilds every crate, yet left the
//! input hash, and so the stamp, reading "current": `--frozen` vouched for a
//! binary built under other settings, and `build` skipped the build that
//! would have made one. So the stamp hashes this environment too, and a
//! change to it makes the product stale. Over-inclusion costs one cargo
//! build that finds everything fresh; omission is the bug, so the set errs
//! wide.
//!
//! Blind to a `--config` argument (no product build passes one) and to a
//! setting cargo reads from a place not listed here.

use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::paths::root;

/// The variables cargo or rustc read that change what a build produces or
/// where it lands: a name equal to one of these, or starting with one ending
/// in `_`. Not the `CARGO_PKG_*`/`CARGO_MANIFEST_*` cargo sets for the
/// runner itself (`cargo glyph` is `cargo run`): a direct
/// `target/release/glyph` run has none, and the stamp must read the same.
/// Not `RUSTUP_TOOLCHAIN` either, which rustup sets for its proxied
/// processes: the toolchain is hashed by what `rustc -vV` answers. Nor
/// `CARGO_HOME`, which `cargo run` sets for its child and a shell need not:
/// the config file it names is hashed by content wherever it is
/// (measured 2026-11-01: hashing the variable staled the renderer between
/// `cargo glyph` and `target/release/glyph`).
const BUILD_VARS: &[&str] = &[
    "CARGO_BUILD_",
    "CARGO_PROFILE_",
    "CARGO_TARGET_",
    "CARGO_UNSTABLE_",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_INCREMENTAL",
    "RUSTFLAGS",
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTC_BOOTSTRAP",
];

fn is_build_var(name: &str) -> bool {
    BUILD_VARS.iter().any(|v| if v.ends_with('_') { name.starts_with(v) } else { name == *v })
}

/// The cargo config files a cargo started in `start` reads, in its order:
/// `.cargo/config.toml` (or the legacy `.cargo/config`) in `start` and each
/// ancestor, then `cargo_home`'s, each followed by what it `include`s.
/// Every file is named once. An included file that does not exist is listed
/// too: an `optional` include appearing is a change.
pub(crate) fn config_files(start: &Path, cargo_home: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = start.ancestors().map(|d| d.join(".cargo")).collect();
    dirs.push(cargo_home.to_path_buf());
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        for name in ["config.toml", "config"] {
            let f = dir.join(name);
            if f.is_file() {
                push_with_includes(&f, &mut out);
                break;
            }
        }
    }
    out
}

fn push_with_includes(file: &Path, out: &mut Vec<PathBuf>) {
    if out.iter().any(|f| f == file) {
        return;
    }
    out.push(file.to_path_buf());
    let Ok(text) = std::fs::read_to_string(file) else { return };
    let Ok(table) = text.parse::<toml::Table>() else { return };
    let dir = file.parent().unwrap_or(Path::new("."));
    for path in included(table.get("include")) {
        push_with_includes(&dir.join(path), out);
    }
}

/// `include`'s paths, in any of its forms: a string, an array of strings, or
/// an array of `{ path = ..., optional = ... }` tables.
fn included(v: Option<&toml::Value>) -> Vec<String> {
    let one = |v: &toml::Value| match v {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Table(t) => t.get("path").and_then(|p| p.as_str()).map(str::to_string),
        _ => None,
    };
    match v {
        Some(toml::Value::Array(a)) => a.iter().filter_map(one).collect(),
        Some(v) => one(v).into_iter().collect(),
        None => Vec::new(),
    }
}

/// The environment as text, one fact per line, so a stamp can say what
/// moved: each build variable, each config file with a hash of its content,
/// and the compiler's version.
pub(crate) fn describe(vars: impl IntoIterator<Item = (String, String)>, configs: &[PathBuf], rustc: &str) -> String {
    let mut lines: Vec<String> = vars
        .into_iter()
        .filter(|(k, _)| is_build_var(k))
        .map(|(k, v)| format!("env {k}={v}"))
        .collect();
    lines.sort();
    for f in configs {
        match std::fs::read(f) {
            Ok(bytes) => lines.push(format!("config {} sha256:{:x}", f.display(), Sha256::digest(&bytes))),
            Err(_) => lines.push(format!("config {} (absent)", f.display())),
        }
    }
    lines.extend(rustc.lines().map(|l| format!("rustc {l}")));
    lines.join("\n") + "\n"
}

fn cargo_home() -> PathBuf {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
        .unwrap_or_default()
}

/// This process's build environment, as a product build from the checkout's
/// root would see it. Computed once per run.
pub(crate) fn current() -> &'static str {
    static TEXT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TEXT.get_or_init(|| {
        let rustc_bin = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let rustc = Command::new(rustc_bin)
            .arg("-vV")
            .current_dir(root())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_else(|e| format!("(rustc -vV failed: {e})"));
        describe(std::env::vars(), &config_files(&root(), &cargo_home()), &rustc)
    })
}

/// The host's own build profile (`.cargo/hosts/<host>.toml`), if this
/// checkout has one for this host and no config cargo reads selects one (a
/// worktree nested in a checkout has the checkout's `.cargo/host.toml`): the
/// build runs on the shared profile alone, slower but correct. Returns the
/// NOTE to print.
pub(crate) fn unselected_host_profile() -> Option<String> {
    let host = hostname()?;
    let profile = root().join(".cargo/hosts").join(format!("{host}.toml"));
    let selected = config_files(&root(), &cargo_home()).iter().any(|f| f.ends_with(".cargo/host.toml") && f.is_file());
    (profile.is_file() && !selected).then(|| {
        format!(
            "NOTE  .cargo/hosts/{host}.toml is this host's build profile and this checkout does not select it;\n      \
             `ln -s hosts/{host}.toml .cargo/host.toml` does (untracked; see .cargo/config.toml)"
        )
    })
}

/// The short host name (`hostname -s`), as `.cargo/hosts/` keys it.
pub(crate) fn hostname() -> Option<String> {
    let out = Command::new("hostname").arg("-s").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// A profile, target or flags variable is part of the environment; what
    /// cargo sets for the runner's own `cargo run`, and anything else, is not.
    #[test]
    fn the_build_variables_are_the_ones_cargo_builds_by() {
        let base = describe(
            vars(&[("HOME", "/h"), ("CARGO_PKG_NAME", "glyph"), ("CARGO_MANIFEST_DIR", "/r"), ("CARGO_HOME", "/h/.cargo")]),
            &[],
            "rustc 1",
        );
        assert_eq!(base, describe(vec![], &[], "rustc 1"), "unrelated variables moved the environment");
        for k in ["CARGO_PROFILE_RELEASE_DEBUG", "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS", "RUSTFLAGS", "RUSTC_BOOTSTRAP", "CARGO_BUILD_JOBS"] {
            assert_ne!(describe(vars(&[(k, "1")]), &[], "rustc 1"), base, "{k} did not move the environment");
        }
        assert_ne!(describe(vec![], &[], "rustc 2"), base, "the toolchain did not move the environment");
    }

    /// Cargo's config files above the start and in CARGO_HOME are found, and
    /// so is what they include, present or not; a file's content is hashed.
    #[test]
    fn config_files_follow_ancestors_home_and_includes() {
        let t = std::env::temp_dir().join(format!("glyph-build-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let (repo, home) = (t.join("a/repo"), t.join("home"));
        std::fs::create_dir_all(repo.join(".cargo")).unwrap();
        std::fs::create_dir_all(t.join("a/.cargo")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(repo.join(".cargo/config.toml"), "include = [{ path = \"host.toml\", optional = true }]\n").unwrap();
        std::fs::write(t.join("a/.cargo/config"), "[build]\njobs = 4\n").unwrap();
        std::fs::write(home.join("config.toml"), "include = \"more.toml\"\n").unwrap();
        let found = config_files(&repo, &home);
        // Only the files under the test's own tree: the temp dir's real
        // ancestors are the host's business.
        let ours: Vec<PathBuf> = found.iter().filter(|f| f.starts_with(&t)).cloned().collect();
        assert_eq!(
            ours,
            [
                repo.join(".cargo/config.toml"),
                repo.join(".cargo/host.toml"),
                t.join("a/.cargo/config"),
                home.join("config.toml"),
                home.join("more.toml"),
            ]
        );
        let before = describe(vec![], &found, "rustc 1");
        std::fs::write(repo.join(".cargo/host.toml"), "[profile.release]\nincremental = true\n").unwrap();
        let after = describe(vec![], &config_files(&repo, &home), "rustc 1");
        let _ = std::fs::remove_dir_all(&t);
        assert_ne!(before, after, "a host profile appearing did not move the environment");
    }
}
