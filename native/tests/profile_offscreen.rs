//! C36 witness (2026-10-24): `GLYPH_PROFILE=1` profiles every frame of a long
//! offscreen run, and the run survives.
//!
//! On an Apple M2 it did not. The profiler keeps a query set alive for every
//! frame whose timestamps are not read back yet, and on Metal each query set
//! is an `MTLCounterSampleBuffer`, of which the M2 allocates 32 at most. The
//! offscreen loop outpaces the GPU, and the profiler's cap was 64 pending
//! frames, so the 33rd frame's allocation failed ("Cannot allocate sample
//! buffer"). That failure took the device with it, and the screenshot
//! readback panicked with `BufferAsyncError`. Measured: 32 frames ran clean,
//! 33 panicked.
//!
//! The fix caps the pending frames at `gpu::PROFILER_PENDING_FRAMES` and has
//! the offscreen loop wait for the oldest profiled frame at the cap, so no
//! frame is dropped unmeasured either. Mutations
//! `profiler-pending-cap-restored` (the cap back at 64: the panic) and
//! `profiler-offscreen-wait-dropped` (no wait: frames dropped at the cap)
//! redden this test on the M2.
//!
//! On a host with more query sets to give (the RTX 5090) it witnesses that
//! the profiled run completes and measures every frame. Needs a GPU with
//! timestamp queries; it refuses to pass without the profile line.

use std::path::Path;
use std::process::Command;

/// More frames than the M2 has counter sample buffers.
const FRAMES: u32 = 60;

#[test]
fn a_profiled_offscreen_run_measures_every_frame() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("profile-offscreen");
    std::fs::create_dir_all(&dir).expect("create the scratch dir");
    let output = Command::new(env!("CARGO_BIN_EXE_glyph3d-native"))
        .env("GLYPH_PROFILE", "1")
        .args(["--load-repo", "fixtures/g-pick-repo", "--frames", &FRAMES.to_string()])
        .arg("--screenshot")
        .arg(dir.join("profiled.png"))
        .output()
        .expect("spawn glyph3d-native");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "a profiled {FRAMES}-frame offscreen run failed (C36):\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    // "profile: N frame(s) measured | GPU: ... | CPU: ..."
    let line = stdout.lines().find(|l| l.starts_with("profile:")).unwrap_or_else(|| {
        panic!("no profile line: was the profiler built (timestamp queries)?\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}")
    });
    let measured: u32 = line.split_whitespace().nth(1).and_then(|n| n.parse().ok()).expect("frame count");
    assert_eq!(measured, FRAMES, "every profiled frame was measured, none dropped at the cap: {line}");
    assert!(line.contains("glyph field pass"), "the glyph pass was timed: {line}");
}
