//! The test build, as cargo describes it: `cargo test --release --no-run
//! --message-format=json`, read for the binaries it made (cargo-test runs
//! them) and the dep-info beside every unit it compiled (orphans reads it).
//! One compile, two gates.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::paths::root;

/// One unit the test build compiled (or found fresh).
pub(crate) struct Unit {
    /// The package's directory: cargo runs a test binary from here.
    pub(crate) package_dir: PathBuf,
    /// `lib`, `bin`, `test`, ... as cargo names the target kind.
    pub(crate) kind: String,
    pub(crate) name: String,
    /// A test harness (`profile.test`): a binary cargo test would run.
    pub(crate) harness: bool,
    pub(crate) executable: Option<PathBuf>,
    pub(crate) filenames: Vec<PathBuf>,
}

impl Unit {
    /// The file this unit names in reports: `glyph3d-native lib (test)`.
    pub(crate) fn label(&self) -> String {
        let pkg = self.package_dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        format!("{pkg} {} {}{}", self.kind, self.name, if self.harness { " (test)" } else { "" })
    }

    /// The dep-info rustc wrote for this unit: every source file it read.
    /// Beside the unit in `deps/` (`<crate>-<hash>.d`, the `lib` prefix and
    /// extension dropped), or cargo's own beside an uplifted binary
    /// (`target/release/<name>.d`).
    pub(crate) fn dep_info(&self) -> Option<PathBuf> {
        self.filenames.iter().chain(&self.executable).find_map(|f| {
            let dir = f.parent()?;
            let file = f.file_name()?.to_string_lossy();
            let d = if dir.ends_with("deps") {
                let stem = match file.rsplit_once('.') {
                    Some((stem, "rlib" | "rmeta" | "so" | "a")) => stem.strip_prefix("lib").unwrap_or(stem),
                    _ => &file,
                };
                dir.join(format!("{stem}.d"))
            } else {
                f.with_extension("d")
            };
            d.exists().then_some(d)
        })
    }
}

/// Build (or find fresh) everything `cargo test --release` would run. Err
/// carries the compiler's own rendered errors when it does not build.
pub(crate) fn test_build() -> Result<Vec<Unit>, String> {
    let out = Command::new("cargo")
        .args(["test", "--release", "--no-run", "--message-format=json"])
        .current_dir(root())
        .output()
        .map_err(|e| format!("could not spawn cargo: {e}"))?;
    let mut units = Vec::new();
    let mut rendered = String::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        match msg["reason"].as_str() {
            Some("compiler-artifact") => {
                let path = |v: &serde_json::Value| v.as_str().map(PathBuf::from);
                units.push(Unit {
                    package_dir: path(&msg["manifest_path"])
                        .and_then(|p| p.parent().map(Path::to_path_buf))
                        .unwrap_or_default(),
                    kind: msg["target"]["kind"][0].as_str().unwrap_or_default().to_string(),
                    name: msg["target"]["name"].as_str().unwrap_or_default().to_string(),
                    harness: msg["profile"]["test"].as_bool().unwrap_or(false),
                    executable: path(&msg["executable"]),
                    filenames: msg["filenames"].as_array().into_iter().flatten().filter_map(path).collect(),
                });
            }
            Some("compiler-message") => {
                if let Some(r) = msg["message"]["rendered"].as_str() {
                    rendered.push_str(r);
                }
            }
            _ => {}
        }
    }
    if out.status.success() {
        Ok(units)
    } else {
        rendered.push_str(&String::from_utf8_lossy(&out.stderr));
        Err(rendered)
    }
}
