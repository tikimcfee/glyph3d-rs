//! C30 witness (2026-10-10): a Visible frame is the same frame every time,
//! and it is the stored modes' frame.
//!
//! Until C30 the Visible field's cull reserved each line's segments and slots
//! (and appended each visible item) by `atomicAdd`, so the order of the
//! transient slots — the glyph draw order — was whatever order the GPU ran
//! the invocations in, different frame to frame. Where a line's back-stacked
//! wrap segments overlap another line's glyphs on screen, the glyph pass
//! (blended, writing depth) resolves the overlap by draw order, so those
//! glyphs flickered in and out on a still camera (Ivan, a large file in a big
//! JS repo). The cull now places everything by scans in (item, line) order:
//! arena order, the Derived and Instanced draw order.
//!
//! The corpus is generated here, deterministically: one long code-like file
//! whose lines a quarter of the time run past the 100-column wrap, so two- and
//! three-segment staircases sit everywhere. The camera looks steeply down on
//! it, where the deeper segments of one line project onto the next. Before
//! the fix, renders of this pose differed run to run (measured on
//! vulkan-nvidia with the pre-C30 cull: 88-183 px of 1.6 M between the
//! first of six runs and each other, max channel delta 70; 188 px against
//! derived). The slot placement going back to an atomic counter reddens it
//! (mutation `visible-slot-order-atomic`).
//!
//! The release binary renders it offscreen, as `pick-oracle`, the golden
//! runner and `visible_verbs` drive it. Needs a GPU.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Renders of the same pose that must agree byte for byte.
const RUNS: usize = 4;
/// Lines in the generated file.
const LINES: usize = 20_000;
/// Steeply down on the file's first pages (pitch -60): the deeper wrap
/// segments of one line project onto its neighbours.
const POSE: [&str; 6] = ["--cam-pose", "300", "-100", "120", "10", "-60"];

/// A small LCG: the corpus must be the same bytes on every host and run.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
    fn range(&mut self, lo: u32, hi: u32) -> u32 {
        lo + self.next() % (hi - lo + 1)
    }
}

/// Writes the corpus (one file, `long.rs`) under the test's own scratch dir.
fn corpus() -> PathBuf {
    let repo = Path::new(env!("CARGO_TARGET_TMPDIR")).join("visible-repeat").join("repo");
    std::fs::create_dir_all(&repo).expect("create the corpus dir");
    const WORDS: [&str; 22] = [
        "let", "fn", "match", "self", "return", "value", "buffer", "index", "offset", "glyph", "layout", "depth",
        "segment", "=>", "{", "}", "(", ")", ";", "&mut", "u32", "f32",
    ];
    let mut rng = Lcg(30);
    let mut text = String::new();
    for _ in 0..LINES {
        let n = if rng.range(0, 3) < 3 { rng.range(10, 95) } else { rng.range(101, 320) } as usize;
        let mut line = " ".repeat(4 * rng.range(0, 4) as usize);
        while line.len() < n {
            line.push_str(WORDS[rng.range(0, WORDS.len() as u32 - 1) as usize]);
            line.push(' ');
        }
        line.truncate(n);
        text.push_str(&line);
        text.push('\n');
    }
    std::fs::write(repo.join("long.rs"), text).expect("write the corpus");
    repo
}

fn render(repo: &Path, mode: &str, out: &Path) -> image::RgbaImage {
    let output = Command::new(env!("CARGO_BIN_EXE_glyph3d-native"))
        .arg("--load-repo")
        .arg(repo)
        .args(["--frames", "2", "--field-mode", mode, "--color-mode", "flat"])
        .args(POSE)
        .arg("--screenshot")
        .arg(out)
        .output()
        .expect("spawn glyph3d-native");
    assert!(
        output.status.success(),
        "glyph3d-native failed ({mode}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    image::open(out).expect("read the screenshot").to_rgba8()
}

/// Pixels that differ, and the largest channel delta among them.
fn diff(a: &image::RgbaImage, b: &image::RgbaImage) -> (usize, u8) {
    assert_eq!(a.dimensions(), b.dimensions());
    let mut n = 0;
    let mut max = 0u8;
    for (pa, pb) in a.pixels().zip(b.pixels()) {
        if pa != pb {
            n += 1;
            max = max.max(pa.0.iter().zip(pb.0.iter()).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0));
        }
    }
    (n, max)
}

#[test]
fn a_visible_frame_repeats_and_is_drawn_in_arena_order() {
    let repo = corpus();
    let dir = repo.parent().unwrap().to_path_buf();
    let derived = render(&repo, "derived", &dir.join("derived.png"));
    // The pose must show glyphs, or a blank frame would repeat trivially.
    let ink = derived.pixels().filter(|p| *p != derived.get_pixel(0, 0)).count();
    assert!(ink > 100_000, "the pose shows the corpus: {ink} non-background px");
    let first = render(&repo, "visible", &dir.join("visible-0.png"));
    for k in 1..RUNS {
        let again = render(&repo, "visible", &dir.join(format!("visible-{k}.png")));
        let (n, max) = diff(&first, &again);
        assert_eq!(n, 0, "visible run {k} differs from run 0 at the same pose: {n} px, max delta {max} (the draw order moved between runs)");
    }
    let (n, max) = diff(&first, &derived);
    assert_eq!(n, 0, "visible differs from derived at the same pose: {n} px, max delta {max} (visible no longer draws in arena order)");
}
