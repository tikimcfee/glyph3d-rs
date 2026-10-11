//! A repo load with no files renders, in every field (2026-10-11).
//!
//! A load with no items stages one zeroed padding record (the slot buffer
//! is never empty) whose group row no view draws with. The view registry's
//! Derived host transcode (step A) panicked on it ("a host record's group
//! row 0 names no single item"), so `--load-repo` of an empty directory, or
//! of a path that is a file, aborted where it used to draw an empty field.
//! Needs a GPU, like every render test in this crate.

use std::path::Path;
use std::process::Command;

#[test]
fn an_empty_repo_load_renders_in_every_field() {
    let dir = std::env::temp_dir().join(format!("glyph-empty-load-{}", std::process::id()));
    let repo = dir.join("empty-repo");
    std::fs::create_dir_all(&repo).expect("create the empty repo");
    for field in ["derived", "instanced", "visible"] {
        let out = dir.join(format!("{field}.png"));
        let output = Command::new(env!("CARGO_BIN_EXE_glyph3d-native"))
            .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")))
            .args(["--load-repo"])
            .arg(&repo)
            .args(["--frames", "2", "--field-mode", field, "--screenshot"])
            .arg(&out)
            .output()
            .expect("spawn glyph3d-native");
        assert!(
            output.status.success(),
            "an empty repo load failed under --field-mode {field}:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(out.exists(), "no frame written under --field-mode {field}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
