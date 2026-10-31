//! Tab stops through the renderer (2026-10-11; the rule is
//! `fold::TabWidth`): every glyph field draws the same frame at every tab
//! width (one field since the Instanced field's retirement, 2026-10-31,
//! when the cross-field comparison below became a loop of one).
//!
//! `fixtures/tab-stops` (tabs at a line's start, after ASCII, after
//! non-ASCII and double-width glyphs, back to back, inside wrapped rows, and
//! one long line) is rendered through the release binary exactly as the
//! golden runner drives it, under each `--field-mode`, drawing the slots the
//! device Pass 2 placed. The fields' frames must be byte-equal at widths 1,
//! 4 and 8; the default (no flag, the `[layout]
//! tab_width_cells` setting) must equal `--tab-width 1`, which is the layout
//! before tab stops; and each wider width must move pixels, or the fixture
//! has stopped exercising tabs. The repo load is flat paint (every load is,
//! since the syntax heuristic's retirement, 2026-10-11), so the two fields
//! agree on colour. Needs a GPU, like every other render test in this crate.

use std::path::{Path, PathBuf};
use std::process::Command;

const REPO: &str = "fixtures/tab-stops";

fn native_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("glyph-tab-stops-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// One offscreen frame of the fixture with `args`.
fn render(out: &Path, args: &[&str]) -> image::RgbaImage {
    let bin = env!("CARGO_BIN_EXE_glyph3d-native");
    let output = Command::new(bin)
        .current_dir(native_dir())
        .args(["--load-repo", REPO, "--frames", "2"])
        .args(args)
        .arg("--screenshot")
        .arg(out)
        .output()
        .expect("spawn glyph3d-native");
    assert!(
        output.status.success(),
        "glyph3d-native failed ({args:?}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    image::open(out).expect("read the screenshot").to_rgba8()
}

fn differing(a: &image::RgbaImage, b: &image::RgbaImage) -> usize {
    assert_eq!(a.dimensions(), b.dimensions());
    a.pixels().zip(b.pixels()).filter(|(p, q)| p != q).count()
}

#[test]
fn every_field_draws_the_same_frame_at_every_tab_width() {
    let dir = scratch();
    let default = render(&dir.join("default.png"), &[]);
    let mut by_width: Vec<(u32, image::RgbaImage)> = Vec::new();
    for width in [1u32, 4, 8] {
        let w = width.to_string();
        let mut frames: Vec<(&str, image::RgbaImage)> = Vec::new();
        for field in glyph_field::GlyphFieldMode::ALL.map(|m| m.as_str()) {
            let out = dir.join(format!("{field}-{width}.png"));
            frames.push((field, render(&out, &["--field-mode", field, "--tab-width", &w])));
        }
        let (first_field, first) = &frames[0];
        for (field, frame) in &frames[1..] {
            let n = differing(first, frame);
            assert_eq!(n, 0, "--tab-width {width}: {field} differs from {first_field} in {n} px (frames in {})", dir.display());
        }
        by_width.push((width, frames.swap_remove(0).1));
    }
    let n = differing(&default, &by_width[0].1);
    assert_eq!(n, 0, "the default frame differs from --tab-width 1 in {n} px: the default must be one cell");
    for pair in by_width.windows(2) {
        let n = differing(&pair[0].1, &pair[1].1);
        assert!(n > 500, "--tab-width {} and {} differ in only {n} px: the fixture no longer exercises tab stops", pair[0].0, pair[1].0);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
