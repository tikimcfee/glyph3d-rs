//! The library's bounding regions through the release binary (2026-10-10,
//! `glyph_scene/regions.rs`): `g-pick-repo` in `--layout-mode library` has
//! one directory below the root, `sub/`, so one region. From the default
//! camera, rendered offscreen with scripted pixel picks:
//!
//! - content out-picks regions: a pixel on `sub/deep.py`'s text picks the
//!   file, a pixel inside `sub/`'s region but off any file (its page's
//!   margin, its padding) picks the region and names the directory, a pixel
//!   outside every region misses; with `[library] regions = false` the
//!   region pixels miss;
//! - the region is DRAWN only where it is: the regions-on frame differs
//!   from the regions-off frame inside `sub/`'s box, and not one pixel of
//!   the root's page (outside every region) moves;
//! - `g` grabs a picked region (GLYPH_DRAG_SCRIPT), and the drag moves it.
//!
//! Needs a GPU, like every other render test in this crate.

use std::path::{Path, PathBuf};
use std::process::Command;

fn native_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("glyph-library-regions-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// One offscreen frame of g-pick-repo's library with `extra` arguments and
/// environment; the PNG and the stdout lines.
fn render(out: &Path, extra: &[&str], env: &[(&str, &str)]) -> (image::RgbaImage, Vec<String>) {
    let bin = env!("CARGO_BIN_EXE_glyph3d-native");
    let output = Command::new(bin)
        .current_dir(native_dir())
        .args(["--load-repo", "fixtures/g-pick-repo", "--layout-mode", "library", "--frames", "2"])
        .args(extra)
        .arg("--screenshot")
        .arg(out)
        .envs(env.iter().copied())
        .output()
        .expect("run the renderer");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(output.status.success(), "renderer failed: {}\n{stdout}", String::from_utf8_lossy(&output.stderr));
    let img = image::open(out).expect("read the screenshot").to_rgba8();
    (img, stdout.lines().filter(|l| l.starts_with("pick:") || l.starts_with("grab")).map(str::to_string).collect())
}

/// On deep.py's text; inside sub/'s region on its page's margin; inside it
/// on the padding beside the page; outside everything.
const PICKS: [&str; 12] = ["--pick-px", "790", "710", "--pick-px", "700", "560", "--pick-px", "645", "700", "--pick-px", "300", "500"];

#[test]
fn regions_pick_after_content_and_draw_only_where_they_are() {
    let dir = scratch();
    let (on, lines) = render(&dir.join("on.png"), &PICKS, &[]);
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert!(lines[0].starts_with("pick: sub/deep.py "), "content out-picks the region: {}", lines[0]);
    assert!(lines[1].starts_with("pick: region sub/ (dir:sub, depth 1"), "the page margin is the region: {}", lines[1]);
    assert!(lines[2].starts_with("pick: region sub/ "), "the padding is the region: {}", lines[2]);
    assert!(lines[3].starts_with("pick: MISS"), "outside every region: {}", lines[3]);

    let off_cfg = dir.join("off.toml");
    std::fs::write(&off_cfg, "[library]\nregions = false\n").expect("write the launch config");
    let cfg = off_cfg.to_str().expect("utf-8 path");
    let mut args = vec!["--launch-config", cfg];
    args.extend_from_slice(&PICKS);
    let (off, lines) = render(&dir.join("off.png"), &args, &[]);
    assert!(lines[0].starts_with("pick: sub/deep.py "), "{}", lines[0]);
    assert!(lines[1].starts_with("pick: MISS") && lines[2].starts_with("pick: MISS"), "no regions, no region picks: {lines:?}");

    // The picks above leave no selection on the region frames (a region
    // pick selects nothing; the last pick is a miss): only regions differ.
    let differs = |x0: u32, y0: u32, x1: u32, y1: u32| {
        let mut n = 0;
        for y in y0..y1 {
            for x in x0..x1 {
                n += usize::from(on.get_pixel(x, y) != off.get_pixel(x, y));
            }
        }
        n
    };
    let inside = differs(640, 500, 960, 925);
    assert!(inside > 20_000, "sub/'s region draws over its box ({inside} pixels differ)");
    assert_eq!(differs(640, 40, 960, 480), 0, "the root's page is in no region");
    assert_eq!(differs(0, 0, 500, 1000), 0, "nothing left of the library");
    assert_eq!(differs(1100, 0, 1600, 1000), 0, "nothing right of the library");

    // g grabs the picked region and drags it: the box leaves where it was.
    let (dragged, lines) = render(&dir.join("drag.png"), &["--pick-px", "645", "700"], &[("GLYPH_DRAG_SCRIPT", "g:12:0:1:10")]);
    assert!(lines.iter().any(|l| l.starts_with("grab: region 'dir:sub'")), "g grabs the picked region: {lines:?}");
    let (still, _) = render(&dir.join("still.png"), &["--pick-px", "645", "700"], &[]);
    let mut moved = 0;
    for y in 500..925 {
        for x in 600..1100 {
            moved += usize::from(dragged.get_pixel(x, y) != still.get_pixel(x, y));
        }
    }
    assert!(moved > 20_000, "the region and its page move with the drag ({moved} pixels)");
    let _ = std::fs::remove_dir_all(&dir);
}
