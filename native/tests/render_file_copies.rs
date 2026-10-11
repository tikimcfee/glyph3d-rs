//! `--render-file --copies N`: N views over the ONE file, tiled, in every
//! field (2026-10-11).
//!
//! Since `--render-file` became a one-item repo load (`repo::load_file`),
//! `--copies N` registers N views whose `buffer` is the same walked file —
//! the first producer of several views over one buffer (`crate::views`).
//! Each copy is laid out as its own item (a stored slot names its item, so
//! copies cannot share slots) and drawn with its own group row at its grid
//! offset (`repo::tile_views`); the Visible field reads every copy's bytes
//! from the one host buffer (`VisibleStaging::item_bytes`).
//!
//! Witnessed through the release binary, offscreen, at a camera parked on
//! the gap between the 2x2 grid's copies, so the frame's quadrants are the
//! copies: in Derived, Instanced and Visible alike, the four-copy frame's
//! top-left quadrant is the one-copy frame's byte for byte (copy 0 is where
//! a lone file is) and each other quadrant carries ink the one-copy frame
//! does not (the copies did not stack on copy 0). And the Derived frame of
//! four copies at the fitted camera is Instanced's, byte for byte. Before
//! 2026-10-11 this test was only that last comparison, against the text
//! stager's copies. Needs a GPU, like every render test in this crate.

use std::path::{Path, PathBuf};
use std::process::Command;

const FILE: &str = "fixtures/baseline-view.txt";
/// The gap between the 2x2 grid's copies of `FILE`: x is the block width
/// plus half the gap, y the block height plus half the gap, below row 0
/// (`repo::tile_views`; the load logs both as `layout [tile]`).
const POSE_ON_THE_GAP: [&str; 6] = ["--cam-pose", "34.4306", "-36.0612", "130", "0", "0"];

fn native_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn render(extra: &[&str], out: &Path) -> image::RgbaImage {
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
    image::open(out).expect("read screenshot").to_rgba8()
}

/// Pixels of quadrant `(right, bottom)` that differ between `a` and `b`.
fn quadrant_diff(a: &image::RgbaImage, b: &image::RgbaImage, right: bool, bottom: bool) -> usize {
    let (w, h) = a.dimensions();
    let xs = if right { w / 2..w } else { 0..w / 2 };
    let ys = if bottom { h / 2..h } else { 0..h / 2 };
    ys.flat_map(|y| xs.clone().map(move |x| (x, y))).filter(|&(x, y)| a.get_pixel(x, y) != b.get_pixel(x, y)).count()
}

#[test]
fn copies_tile_in_every_field() {
    let dir = std::env::temp_dir().join(format!("glyph-render-file-copies-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    for field in ["derived", "instanced", "visible"] {
        let mode = ["--field-mode", field];
        let one = render(&[&mode[..], &POSE_ON_THE_GAP[..]].concat(), &dir.join(format!("one-{field}.png")));
        let four =
            render(&[&mode[..], &POSE_ON_THE_GAP[..], &["--copies", "4"]].concat(), &dir.join(format!("four-{field}.png")));
        assert_eq!(quadrant_diff(&four, &one, false, false), 0, "{field}: copy 0 is not where the lone file is");
        for (right, bottom) in [(true, false), (false, true), (true, true)] {
            assert!(
                quadrant_diff(&four, &one, right, bottom) > 1000,
                "{field}: the quadrant (right {right}, bottom {bottom}) has no copy in it"
            );
        }
    }
    let four = render(&["--copies", "4"], &dir.join("four.png"));
    let four_instanced = render(&["--copies", "4", "--field-mode", "instanced"], &dir.join("four-instanced.png"));
    assert!(four == four_instanced, "--copies 4: the default field's frame differs from Instanced's");
    let _ = std::fs::remove_dir_all(&dir);
}
