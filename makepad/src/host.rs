//! The engine side of the makepad host: the scene, what it was built from,
//! and the one place a rebuild happens. The egui window
//! (`native/src/windowed/app.rs`) is the model, cut down to what a host
//! does once the engine draws through makepad: it owns the `SceneChoice`
//! and `SceneCullOptions`, builds the scene with the panel probe, applies a
//! `RelayoutRequest` between frames through `relayout::rebuild_for_relayout`
//! and swaps in what comes back. The panels and the viewport reach it
//! through `Scope` data (`ui_adapter`, `viewport`).
//!
//! The scene holds no device type (2026-10-31): the makepad passes draw
//! from its frames (`FrameData`: the field's host blocks, the tables, the
//! atlas), so the host builds it with `SceneLoad::host()` and never makes
//! a wgpu device. (Until that day it made one, with no surface, only so
//! the scene could build: 1.3 GB of that device's VRAM on g-pick-repo.)

use std::time::Instant;

use glyph3d_native::cli::{parse_cli_from, Cli, CliCommand, Op};
use glyph3d_native::glyph_scene::{CameraMode, UiProbe};
use glyph3d_native::relayout::{rebuild_for_relayout, RelayoutRequest};
use glyph3d_native::scene::{SceneKey, SceneLike};
use glyph3d_native::{SceneChoice, SceneCullOptions};

use crate::sessions::SessionBrowser;


/// The flags makepad's platform reads from the command line itself
/// (`--remote`, the virtual clock, `--window WxH@s` ...): dropped before the
/// engine's CLI parses the rest, which would refuse them. The second field
/// says whether the flag takes the next argument as its value.
const MAKEPAD_FLAGS: &[(&str, bool)] = &[
    ("--remote", false),
    ("--virtual-clock", false),
    ("--devtools", false),
    ("--stdin-loop", false),
    ("--hot", false),
    ("--window", true),
    ("--seed", true),
];

/// The engine's own arguments: a command line (program first) less
/// makepad's flags.
fn engine_args(mut args: impl Iterator<Item = String>) -> Vec<String> {
    let mut out = Vec::new();
    out.extend(args.next());
    while let Some(arg) = args.next() {
        if arg.starts_with("--remote-title-tag=") {
            continue;
        }
        match MAKEPAD_FLAGS.iter().find(|(flag, _)| *flag == arg) {
            Some((_, true)) => {
                args.next();
            }
            Some((_, false)) => {}
            None => out.push(arg),
        }
    }
    out
}

/// What the command line asked for: the scene, its options and the
/// startup ops (`--pick-*`, `--verb`, `--cam-pose`, `--highlight`).
pub type Plan = (SceneChoice, SceneCullOptions, Vec<Op>);

thread_local! {
    /// The plan, parsed at startup and taken once by the app.
    static PLAN: std::cell::RefCell<Option<Plan>> = const { std::cell::RefCell::new(None) };
}

/// Parse the command line at startup (`app_main!`'s `configure:`, which
/// runs before the app's script and so before any window exists): the
/// engine's CLI, the windowed render plan's choice, options and ops. Help
/// and version print and exit 0; a bad line, and any run mode with no
/// window (a fixture check, `--gpu-key`, an offscreen screenshot, which
/// belong to `glyph3d-native`), print why and exit 1. A window never opens
/// for a run that will not draw (2026-10-26: `--help` used to open one).
pub fn parse_plan_or_exit() {
    use clap::CommandFactory;
    let matches = Cli::command().try_get_matches_from(engine_args(std::env::args())).unwrap_or_else(|e| e.exit());
    let refuse = |why: String| -> ! {
        eprintln!("error: {why}");
        std::process::exit(1);
    };
    let cli = parse_cli_from(matches).unwrap_or_else(|e| refuse(e));
    let plan = match cli.action() {
        CliCommand::Render(plan) => match plan.target {
            glyph3d_native::cli::RenderTarget::Windowed { .. } => (plan.choice, plan.cull_opts, plan.ops),
            glyph3d_native::cli::RenderTarget::Offscreen { .. } => {
                refuse("--screenshot renders offscreen: run glyph3d-native for that".to_string())
            }
        },
        _ => refuse("this run mode has no window: run glyph3d-native for it".to_string()),
    };
    PLAN.with(|p| *p.borrow_mut() = Some(plan));
}

/// The plan [`parse_plan_or_exit`] stored, once.
pub fn take_plan() -> Plan {
    PLAN.with(|p| p.borrow_mut().take()).expect("the plan is parsed at startup, before the app's first event")
}

/// The engine as the host holds it.
pub struct EngineHost {
    /// What a (re)build takes: the host's memory, no device copy.
    load: glyph3d_native::SceneLoad,
    /// Owned: a relayout mutates the choice's params in place before it
    /// rebuilds.
    choice: SceneChoice,
    cull_opts: SceneCullOptions,
    pub scene: Box<dyn SceneLike>,
    /// The panels' read-back channel; None for the quad demo.
    pub probe: Option<UiProbe>,
    /// The viewport in physical px, as the scene was last told.
    viewport: (u32, u32),
    /// The rebuild the panels or the scene asked for, applied between
    /// frames (`apply_pending_relayout`), never mid-frame.
    pub pending_relayout: Option<RelayoutRequest>,
    /// Mouse-look: the pointer is locked and its motion turns the camera.
    pub looking: bool,
    /// Which panels are open (F8, F1, F7).
    pub hud_open: bool,
    pub debug_open: bool,
    pub sessions_open: bool,
    pub sessions: SessionBrowser,
    /// Which scene this is, bumped by every rebuild: the frame feed keys
    /// its per-scene tables on it.
    scene_generation: u64,
    start: Instant,
    last_tick: Instant,
    fps_window: (Instant, u32),
    pub fps: f32,
    /// The bytes makepad lets its publications hold on the device (its
    /// publication envelope, read once at startup, never 0): the glyph
    /// blocks are resident within it.
    pub glyph_envelope: usize,
}

impl EngineHost {
    /// Build the scene the command line asked for, with its probe, and run
    /// its startup ops (`--pick`, `--verb`, `--cam-pose`, `--highlight`)
    /// through the same entry points the panels use.
    pub fn new(choice: SceneChoice, mut cull_opts: SceneCullOptions, ops: &[Op], glyph_envelope: usize) -> Self {
        let load = glyph3d_native::SceneLoad::host();
        let (mut scene, probe) = glyph3d_native::build_scene_probed(&load, &choice, CameraMode::Fly, cull_opts);
        // The options follow the mode the scene BUILT (until 2026-10-31 a
        // load past the Derived lanes fell back to Instanced).
        if let Some(built) = probe.as_ref().and_then(|p| p.borrow().field_mode) {
            cull_opts.field_mode = built;
        }
        let viewport = (glyph3d_native::OFFSCREEN_WIDTH, glyph3d_native::OFFSCREEN_HEIGHT);
        scene.set_viewport(viewport.0, viewport.1);
        for op in ops {
            if let Some(line) = glyph3d_native::apply_op(scene.as_mut(), op) {
                println!("{line}");
            }
        }
        Self {
            load,
            choice,
            cull_opts,
            scene,
            probe,
            viewport,
            pending_relayout: None,
            looking: false,
            hud_open: false,
            debug_open: true,
            sessions_open: false,
            sessions: SessionBrowser::default(),
            scene_generation: 1,
            start: Instant::now(),
            last_tick: Instant::now(),
            fps_window: (Instant::now(), 0),
            fps: 0.0,
            glyph_envelope,
        }
    }

    /// The viewport's size in physical px; the scene re-targets on a change.
    pub fn set_viewport(&mut self, w: u32, h: u32) {
        let size = (w.max(1), h.max(1));
        if size != self.viewport {
            self.viewport = size;
            self.scene.set_viewport(size.0, size.1);
        }
    }

    pub fn viewport(&self) -> (u32, u32) {
        self.viewport
    }

    pub fn scene_generation(&self) -> u64 {
        self.scene_generation
    }

    /// Wall-clock seconds since the host started: the frame's time.
    pub fn time(&self) -> f32 {
        self.start.elapsed().as_secs_f32()
    }

    /// One frame's motion, before it is drawn: the fly camera's integration
    /// and the scene's own animation, on a wall-clock dt capped at 0.1 s so a
    /// stall does not launch the camera. Then the rebuild the last frame
    /// asked for, if any (a carrel's deck navigation signals one too).
    pub fn advance(&mut self) {
        let dt = self.last_tick.elapsed().as_secs_f32().min(0.1);
        self.last_tick = Instant::now();
        self.scene.tick(dt);
        self.scene.animate(dt);
        self.apply_pending_relayout();
    }

    /// Count a presented frame; the FPS figure covers the last second.
    pub fn count_frame(&mut self) {
        self.fps_window.1 += 1;
        let elapsed = self.fps_window.0.elapsed().as_secs_f32();
        if elapsed >= 1.0 {
            self.fps = self.fps_window.1 as f32 / elapsed;
            self.fps_window = (Instant::now(), 0);
        }
    }

    /// Apply the pending relayout: the engine rebuilds and the host swaps
    /// in the scene and probe it returns (pick, selection and grab reset
    /// with the scene).
    pub fn apply_pending_relayout(&mut self) {
        if let Some(opts) = self.scene.take_pending_carrel_options() {
            self.pending_relayout = Some(RelayoutRequest { carrel_options: Some(opts), ..Default::default() });
        }
        let Some(req) = self.pending_relayout.take() else { return };
        let rebuilt = rebuild_for_relayout(
            &self.load,
            self.viewport,
            &mut self.choice,
            &mut self.cull_opts,
            self.probe.as_ref(),
            true,
            req,
        );
        if let Some((scene, probe)) = rebuilt {
            self.scene = scene;
            self.probe = probe;
            self.scene_generation += 1;
        }
    }

    /// A key the scene acts on, pressed or released.
    pub fn key(&mut self, key: SceneKey, pressed: bool) {
        self.scene.on_key(key, pressed);
    }

    /// A press of one of the scene's keys, as a panel button makes one.
    pub fn press(&mut self, key: SceneKey) {
        self.key(key, true);
    }
}

#[cfg(test)]
mod tests {
    use super::engine_args;

    fn args(line: &[&str]) -> Vec<String> {
        engine_args(line.iter().map(|s| s.to_string()))
    }

    #[test]
    fn makepad_flags_and_their_values_are_dropped() {
        assert_eq!(
            args(&["app", "--remote", "--load-repo", "dir", "--window", "800x600@2", "--remote-title-tag=x", "--seed", "7", "--virtual-clock"]),
            ["app", "--load-repo", "dir"]
        );
    }

    #[test]
    fn the_engines_flags_pass_untouched() {
        let line = ["app", "--render-file", "f.txt", "--field-mode", "derived", "--cam-pose", "1", "2", "3", "4", "5"];
        assert_eq!(args(&line), line);
    }
}
