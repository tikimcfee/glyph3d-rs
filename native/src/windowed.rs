//! Windowed mode: winit window + wgpu surface, uncapped continuous render loop,
//! FPS printed to stdout once per second. Renders whichever scene the CLI chose.
//!
//! Stage F: the glyph scenes run a FLY camera — WASD strafe/forward, E|R up,
//! Q|F down, scroll wheel = speed multiplier.
//! Stage G: interaction is split between the two mouse buttons so picking and
//! the fly camera coexist:
//!   - LEFT click (ungrabbed pointer) = PICK the glyph under the cursor
//!     (prints file:row:col:char; Stage L/L4: selection-tints it);
//!   - RIGHT press-and-drag = mouse-look (pointer confined + hidden while
//!     held; raw DeviceEvent deltas); Esc also releases;
//!   - verb keys act on the last pick: `h` highlight line, `g` grab/release
//!     the picked file (mouse drags it in the view plane, scroll scales it),
//!     `t` cycle the tint palette, `x` toggle hidden.
//!
//! Stage K: egui 0.36 overlay (feature `egui-ui`, default ON; `--no-ui` at
//! runtime gives exact pre-K behavior). egui sees every window event FIRST;
//! scene input routing only handles events egui did not consume. Per frame,
//! after the scene pass on the SAME encoder, an egui pass (LoadOp::Load, no
//! depth attachment) paints the tessellated UI onto the surface. K1 shipped
//! the plumbing with an EMPTY CentralPanel. K2 input-consumption rules:
//!   - keys route only when egui hasn't consumed them (a focused egui widget
//!     swallows keys — typing WASD/h/g/t/x in a text field must not fly the
//!     camera or fire verbs; Tab is always egui's);
//!   - right-press grab and left-click pick route only when egui doesn't
//!     want the pointer (hovering a panel counts); right-RELEASE always
//!     ungrabs (consumed releases must not latch the grab);
//!   - cursor bookkeeping always updates; scene cursor effects (look
//!     fallback, grabbed-group drags) skip while egui wants the pointer;
//!   - the moment egui wants ANY input, the look-grab is released (checked
//!     per frame in render()); it re-acquires once the pointer leaves egui.
//!
//! K3 adds the Debug window (FPS/camera/pick readouts, verb buttons calling
//! the exact CLI op API, K2 scratch text field). K4 makes LOD_MIN_PX live
//! via a slider (probe-cell → CullState Cell seam; offscreen keeps the
//! const) and adds live cull counters; F1 toggles the window.
//!
//! K6: in-window screenshot. The surface is configured with COPY_SRC (an
//! assert at configure checks the adapter supports it); on demand (F2 — an
//! app-level hotkey that fires regardless of egui focus — or the
//! `--screenshot-frame N --screenshot-out PATH` CLI pair) the just-encoded
//! surface texture is copied to a MAP_READ buffer AFTER the final
//! queue.submit and BEFORE present, blocking-mapped, swizzled BGRA→RGBA
//! (the windowed surface is Bgra8UnormSrgb — offscreen is Rgba and needs no
//! swizzle), and written as PNG. The readback happens after BOTH the scene
//! pass and the egui pass, so the PNG is the COMPOSED frame — 3D scene AND
//! the Debug window; that inclusion is the point (pixel-verification seam
//! for "invisible by construction" UI claims, per the stage erratum).
//!
//! Layout dial (repo scenes): the Debug panel's z_wrap_spacing slider
//! applies on drag RELEASE by rebuilding the scene — re-running
//! repo::load_repo with the new pitch and swapping the GlyphScene in place
//! (the pending_relayout arm in window_event). Layout is not a per-frame
//! input like K4's LOD threshold, and the JS system's `grid.layout` was
//! likewise a discrete refold command, not a live drag. The camera pose
//! survives the swap (restored from the old probe); pick/selection/grab
//! state resets with the scene.

use std::time::Instant;

use winit::event_loop::EventLoop;

use crate::gpu::GpuContext;
use crate::{Op, SceneChoice};

mod app;
use app::App;

mod state;
#[cfg(feature = "egui-ui")]
mod ui;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveStep {
    Next,
    Prev,
    Reset,
}

pub struct LiveSource {
    pub rx: std::sync::mpsc::Receiver<crate::seam::SurfaceUpdate>,
    pub content: std::collections::HashMap<String, std::sync::Arc<Vec<u8>>>,
    pub params: crate::repo::RepoParams,
    pub trie: std::path::PathBuf,
    pub emoji_sheet: std::path::PathBuf,
    pub atlas: crate::atlas::Atlas,
    pub last_style: std::collections::HashMap<String, crate::seam::SurfaceUpdate>,
    pub backlog: Vec<crate::seam::SurfaceUpdate>,
    pub step: Option<std::sync::mpsc::Sender<LiveStep>>,
    pub step_count: usize,
}

/// Stage K (K6): `yyyymmdd-hhmmss` UTC stamp for shot filenames (no chrono
/// dep; Howard Hinnant's civil-from-days algorithm).
fn utc_stamp(now: std::time::SystemTime) -> String {
    let secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before the unix epoch")
        .as_secs();
    let days = (secs / 86400) as i64;
    let tod = secs % 86400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    // Canonical Hinnant: (5*doy + 2)/153. (An earlier edit used the +456
    // variant's constant with the canonical d/m formulas — the variants are
    // not mixable; produced e.g. month=12 day=89 for 2026-09-02.)
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", tod / 3600, tod % 3600 / 60, tod % 60)
}

#[allow(clippy::too_many_arguments)] // the launch surface; one live param too many for the lint, not for the callers
pub fn run(
    ctx: GpuContext,
    // Owned: the relayout arm mutates the choice's params (Repo's
    // z_wrap_spacing, Repo/Text cluster_mode) before rebuilding the scene
    // (windowed.rs's pending_relayout arm). Offscreen keeps borrowing its own.
    choice: SceneChoice,
    cull: bool,
    ops: &[Op],
    ui: bool,
    shot: Option<(u64, std::path::PathBuf)>,
    present_mode: wgpu::PresentMode,
    live: Option<LiveSource>,
) {
    // Without the `egui-ui` feature the overlay is compiled out entirely;
    // the flag is accepted (and ignored) so the CLI is identical either way.
    #[cfg(not(feature = "egui-ui"))]
    let _ = ui;
    let event_loop = EventLoop::new().expect("event loop creation failed");
    let mut app = App {
        ctx,
        choice,
        cull,
        ops,
        live,
        live_step_ix: 0,
        start: Instant::now(),
        state: None,
        #[cfg(feature = "egui-ui")]
        ui,
        shot,
        present_mode,
    };
    event_loop.run_app(&mut app).expect("event loop error");
}

#[cfg(test)]
mod tests {
    use super::utc_stamp;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn utc_stamp_known_epochs() {
        // Values pinned against `date -u` (macOS): epoch 0, and the K6 fix
        // date 2026-09-02 18:21:12 UTC (the bad stamp that exposed the bug
        // read "202612-89-182112").
        let at = |s: u64| utc_stamp(UNIX_EPOCH + Duration::from_secs(s));
        assert_eq!(at(0), "19700101-000000");
        assert_eq!(at(1788373272), "20260902-182112");
        assert_eq!(at(951782400), "20000229-000000"); // leap day, era boundary math
        assert_eq!(at(4102444800), "21000101-000000"); // non-leap century year
    }
}
