//! C35 witness (2026-10-24): a still frame repeats byte for byte inside one
//! process, frame 0 included, at the golden `repo-back-oblique` pose, in every
//! field mode.
//!
//! On an Apple M2 it did not. The glyph pass blends, writes depth and may
//! discard. Its AA footprint, `fwidth(glyph_uv)`, is a difference across a
//! 2x2 quad, and a quad-mate lane hidden behind a nearer glyph was sometimes
//! depth-rejected before shading and sometimes after, depending on GPU timing.
//! The winning fragment's footprint, and so its coverage, moved with it. The
//! set of fragments passing the depth test was identical every frame (a
//! stencil count), and so was the depth. Measured with the hardware
//! footprint, 12 frames in one process:
//! - 63-81 px of union flicker at delta 1;
//! - frame 0 a further ~880 px (max delta 138) from the frames after it. That
//!   "early state" was what a fresh process's frame 1, the golden, captured.
//!
//! A cross-process golden sees this only as an occasional 1-2 px flip.
//!
//! The fix takes Apple TBDR's footprint from the quad's projective map
//! (`glyph_field::Footprint::Analytic`, chosen by `GpuProfile::glyph_footprint`).
//! Mutation `footprint-selection-dropped` puts the M2 back on the
//! hardware derivative, and this test reddens.
//!
//! On a host whose selection is `Hardware` (the RTX 5090: 0 flicker and
//! frame 0 == frame 1 measured with the same per-frame readback, and this
//! test green there, 2026-10-24) it runs the shipping shader. There it
//! witnesses that host's determinism, not the analytic path.
//!
//! The release binary renders it offscreen with `GLYPH_REPEAT_CHECK=1`, as
//! `visible_repeat` drives it. Needs a GPU.

use std::path::Path;
use std::process::Command;

/// Frames per process; every one is compared with frame 0.
const FRAMES: &str = "12";
/// The `repo-back-oblique` golden's pose (build.toml).
const POSE: [&str; 6] = ["--cam-pose", "30", "-120", "60", "0", "-12"];

/// (frames compared, frames that differ from frame 0, the per-frame lines).
fn repeat(mode: &str, out: &Path) -> (u32, u32, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_glyph3d-native"))
        .env("GLYPH_REPEAT_CHECK", "1")
        .args(["--load-repo", "fixtures/g-pick-repo", "--frames", FRAMES, "--field-mode", mode])
        .args(POSE)
        .arg("--screenshot")
        .arg(out)
        .output()
        .expect("spawn glyph3d-native");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "glyph3d-native failed ({mode}):\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines: Vec<&str> = stdout.lines().filter(|l| l.starts_with("repeat:")).collect();
    let summary = lines.last().unwrap_or_else(|| panic!("no repeat summary ({mode}):\n{stdout}"));
    // "repeat: 12 frames, N differ from frame 0"
    let words: Vec<&str> = summary.split_whitespace().collect();
    let frames: u32 = words[1].parse().expect("frame count");
    let differ: u32 = words[3].parse().expect("differing count");
    (frames, differ, lines.join("\n"))
}

#[test]
fn a_still_oblique_frame_repeats_in_one_process_in_every_mode() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("oblique-repeat");
    std::fs::create_dir_all(&dir).expect("create the scratch dir");
    for mode in ["derived", "instanced", "visible"] {
        let out = dir.join(format!("{mode}.png"));
        let (frames, differ, lines) = repeat(mode, &out);
        assert_eq!(frames, 12, "{mode}: the check compared every frame");
        // A blank frame would repeat trivially.
        let img = image::open(&out).expect("read the screenshot").to_rgba8();
        let bg = *img.get_pixel(0, 0);
        let ink = img.pixels().filter(|p| **p != bg).count();
        assert!(ink > 300_000, "{mode}: the pose shows the corpus: {ink} non-background px");
        assert_eq!(
            differ, 0,
            "{mode}: {differ} of {frames} frames differ from frame 0 in one process at the oblique pose \
             (a scheduling-dependent glyph footprint, C35):\n{lines}"
        );
    }
}
