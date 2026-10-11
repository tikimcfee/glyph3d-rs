//! `--render-file --copies N` tiles N copies in the Derived field
//! (2026-10-11).
//!
//! The text stager gives each copy its own item and group row. The Derived
//! vertex stage draws a glyph with its ITEM's group, and until 2026-10-11
//! every copy's item named group 0, so the copies stacked on copy 0. It was
//! invisible while Instanced (which reads the slot's group) was the default,
//! and no golden view uses `--copies`.
//!
//! Witnessed through the release binary, offscreen: the default (Derived)
//! frame of four copies is byte-equal to Instanced's. (Instanced is the
//! witness while it exists; its retirement must give this test another.)
//! Needs a GPU, like every render test in this crate.

use std::path::{Path, PathBuf};
use std::process::Command;

const FILE: &str = "fixtures/baseline-view.txt";

fn native_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn render(extra: &[&str], out: &Path) -> Vec<u8> {
    let output = Command::new(env!("CARGO_BIN_EXE_glyph3d-native"))
        .current_dir(native_dir())
        .args(["--render-file", FILE, "--frames", "2"])
        .args(extra)
        .arg("--screenshot")
        .arg(out)
        .output()
        .expect("spawn glyph3d-native");
    assert!(
        output.status.success(),
        "glyph3d-native failed ({extra:?}):\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::read(out).expect("read screenshot")
}

#[test]
fn copies_tile_in_the_default_field() {
    let dir = std::env::temp_dir().join(format!("glyph-render-file-copies-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let four = render(&["--copies", "4"], &dir.join("four.png"));
    let four_instanced = render(&["--copies", "4", "--field-mode", "instanced"], &dir.join("four-instanced.png"));
    assert!(four == four_instanced, "--copies 4: the default field's frame differs from Instanced's");
    let _ = std::fs::remove_dir_all(&dir);
}
