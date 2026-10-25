//! The command bus's executor (2026-10-24): the one place a windowed intent
//! touches the scene, the choice or the window. Lines are queued on
//! `WindowState::commands` by `push_line` (hotkeys, panels, self-tests) and
//! run FIFO by `drain`, which the RedrawRequested arm calls between frames,
//! where the relayout signal used to be consumed — so a panel click lands
//! one frame later than when panels called the scene inside the egui pass,
//! the same latency a relayout always had, and a shot after a verb still
//! captures the verb's result.
//!
//! Every scene-facing call is the CLI's: ops through `cli::ops::apply_op`,
//! rebuilds through `app::apply_relayout`. Each reply is printed, as the
//! panel's verb buttons printed theirs.

use crate::gpu::GpuContext;
use crate::{Op, SceneChoice};

use super::app::apply_relayout;
use super::bus::{self, AppCommand, Command, FlyTarget, PanelId};
use super::state::{cam_pose_arg, WindowState};
use super::{utc_stamp, LiveSource, LiveStep};
#[cfg(feature = "egui-ui")]
use super::panels::console::LogEntry;

/// The `App` fields a command may touch, borrowed disjointly from the
/// window state (the free-function pattern `apply_relayout` already uses:
/// the caller holds `&mut WindowState` out of `App::state`).
pub(super) struct Host<'a> {
    pub(super) ctx: &'a GpuContext,
    pub(super) choice: &'a mut SceneChoice,
    pub(super) cull_opts: &'a mut crate::SceneCullOptions,
    pub(super) live: &'a mut Option<LiveSource>,
    pub(super) live_step_ix: &'a mut usize,
    /// The egui overlay is on (false under `--no-ui` or without the feature).
    pub(super) ui: bool,
}

impl WindowState {
    /// Queue a line. A line that does not parse is reported and dropped,
    /// never queued: nothing the window writes should fail
    /// (`every_line_the_window_writes_parses`).
    pub(super) fn push_line(&mut self, line: &str) {
        match bus::parse_line(line) {
            Ok(parsed) => self.commands.push(parsed),
            Err(e) => {
                println!("bus: {line:?} refused: {e}");
                #[cfg(feature = "egui-ui")]
                self.log(LogEntry::Refused(format!("{line}: {e}")));
            }
        }
    }

    /// Append to the console's log (none without the overlay).
    #[cfg(feature = "egui-ui")]
    fn log(&mut self, entry: LogEntry) {
        if let Some(egui) = self.egui.as_mut() {
            egui.shell.log.push(entry);
        }
    }
}

/// Run every queued line, in order; returns whether anything ran. Lines a
/// command queues while running wait for the next drain.
pub(super) fn drain(host: &mut Host<'_>, state: &mut WindowState) -> bool {
    let mut ran = false;
    #[cfg(feature = "egui-ui")]
    if let Some(reply) = finish_fly(host, state) {
        println!("{reply}");
        state.log(LogEntry::Reply(reply));
        ran = true;
    }
    for parsed in std::mem::take(&mut state.commands) {
        ran = true;
        log::debug!("bus: {}", parsed.line);
        #[cfg(feature = "egui-ui")]
        state.log(LogEntry::Line(parsed.line));
        for command in parsed.commands {
            if let Some(reply) = execute(host, state, command) {
                println!("{reply}");
                #[cfg(feature = "egui-ui")]
                state.log(LogEntry::Reply(reply));
            }
        }
    }
    ran
}

fn execute(host: &mut Host<'_>, state: &mut WindowState, command: Command) -> Option<String> {
    match command {
        Command::Op(op) => apply(host.ctx, state, &op),
        Command::Pick(pick) => Some(
            state
                .scene
                .apply_pick(host.ctx, &pick)
                .unwrap_or_else(|| "pick: this scene does not support picking".to_string()),
        ),
        Command::Relayout(request) => {
            apply_relayout(host.ctx, host.choice, host.cull_opts, host.ui, state, request)
        }
        Command::Fly(target) => fly(state, target),
        Command::SceneKey(code) => {
            state.scene.on_key(host.ctx, code, true);
            None
        }
        Command::App(app) => app_command(host, state, app),
    }
}

/// One op through the CLI's applier. A `--cam-pose` replies with the pose
/// it set, in the flag's own form; offscreen says "does not support" for
/// it, which is its old stdout and no help in a log.
fn apply(ctx: &GpuContext, state: &mut WindowState, op: &Op) -> Option<String> {
    match (crate::cli::apply_op(state.scene.as_mut(), ctx, op), op) {
        (Some(reply), _) => Some(reply),
        (None, Op::CamPose(eye, yaw, pitch)) => Some(format!("camera: {}", cam_pose_arg(*eye, *yaw, *pitch))),
        (None, _) => Some("op: scene does not support picking/manipulation".to_string()),
    }
}

/// Step one: resolve the row and make sure the scene refreshes its live
/// pose. `UiFileDyn` is refreshed only for the rows the browser drew last
/// frame (`UiProbeState::browser_rows`, 2026-10-10), so a fly to a file
/// scrolled out of the browser — or typed in the console — would frame a
/// stale or zero box. The row joins `browser_rows` (a sanctioned write:
/// view feedback, like the browser's own), and `finish_fly` frames it once
/// a frame has rendered.
#[cfg(feature = "egui-ui")]
fn fly(state: &mut WindowState, target: FlyTarget) -> Option<String> {
    let Some(cell) = state.ui_probe.as_ref() else {
        return Some("fly: this scene has no file rows (demo scene, or --no-ui)".to_string());
    };
    let mut probe = cell.borrow_mut();
    let row = match bus::fly_row(&probe.files, &target) {
        Ok(row) => row,
        Err(e) => return Some(e),
    };
    if !probe.browser_rows.contains(&row) {
        probe.browser_rows.push(row);
    }
    state.pending_fly = Some((row, state.frames_total));
    None
}

#[cfg(not(feature = "egui-ui"))]
fn fly(_state: &mut WindowState, target: FlyTarget) -> Option<String> {
    let what = match target {
        FlyTarget::File(file) => file,
        FlyTarget::Group(group) => format!("group {group}"),
    };
    Some(format!("fly {what}: needs the egui overlay (the file rows live in its probe)"))
}

/// Step two: once a frame has rendered since step one, frame the row from
/// its refreshed pose, through the `--cam-pose` op.
#[cfg(feature = "egui-ui")]
fn finish_fly(host: &mut Host<'_>, state: &mut WindowState) -> Option<String> {
    let (row, queued_at) = state.pending_fly?;
    if state.frames_total <= queued_at {
        return None;
    }
    state.pending_fly = None;
    let aspect = state.config.width as f32 / state.config.height.max(1) as f32;
    // The probe borrow ends before the scene is touched.
    let (op, path) = {
        let probe = state.ui_probe.as_ref()?.borrow();
        let pose = *probe.file_dyn.get(row)?;
        (bus::fly_pose(&pose, aspect), probe.files.get(row)?.rel_path.clone())
    };
    apply(host.ctx, state, &op).map(|reply| format!("fly {path}: {reply}"))
}

fn app_command(host: &mut Host<'_>, state: &mut WindowState, command: AppCommand) -> Option<String> {
    match command {
        AppCommand::Shot => {
            state.capture_pending = Some(
                std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../out"))
                    .join(format!("windowed-shot-{}.png", utc_stamp(std::time::SystemTime::now()))),
            );
            // GLYPH_POSE_PRINT=1: the shot's camera as the `--cam-pose`
            // argument that reproduces it (degrees), so a view seen once
            // can be a golden view's command.
            if std::env::var_os("GLYPH_POSE_PRINT").is_some() {
                return Some(match state.scene.cam_pose() {
                    Some((eye, yaw, pitch)) => cam_pose_arg(eye, yaw, pitch),
                    None => "pose: this scene has no fly camera".to_string(),
                });
            }
            None
        }
        AppCommand::CopyPose => {
            let Some((eye, yaw, pitch)) = state.scene.cam_pose() else {
                return Some("pose: this scene has no fly camera".to_string());
            };
            let arg = cam_pose_arg(eye, yaw, pitch);
            #[cfg(feature = "egui-ui")]
            if let Some(egui) = state.egui.as_mut() {
                egui.shell.pending_clipboard = Some(arg.clone());
            }
            Some(arg)
        }
        AppCommand::Live(step) => {
            let title = live_step_title(host.live, host.live_step_ix, step)?;
            state.window.set_title(&title);
            Some(title)
        }
        AppCommand::Panel(panel) => toggle_panel(state, panel),
        AppCommand::SessionsRefresh => sessions_refresh(state),
        AppCommand::Help => Some(bus::help_text()),
        AppCommand::CliHelp => {
            use clap::CommandFactory;
            Some(crate::cli::Cli::command().render_long_help().to_string())
        }
        AppCommand::ClearLog => clear_log(state),
    }
}

#[cfg(feature = "egui-ui")]
fn clear_log(state: &mut WindowState) -> Option<String> {
    if let Some(egui) = state.egui.as_mut() {
        egui.shell.log.clear();
    }
    None
}

#[cfg(not(feature = "egui-ui"))]
fn clear_log(_state: &mut WindowState) -> Option<String> {
    Some("clear: there is no log without the egui overlay".to_string())
}

#[cfg(feature = "egui-ui")]
fn toggle_panel(state: &mut WindowState, panel: PanelId) -> Option<String> {
    let shell = &mut state.egui.as_mut()?.shell;
    let open = match panel {
        PanelId::Debug => &mut shell.debug_open,
        PanelId::Hud => &mut shell.hud_open,
        PanelId::Sessions => &mut shell.session_browser_open,
        PanelId::Console => &mut shell.console_open,
    };
    *open = !*open;
    // An opened console takes the keyboard, so F9 then typing just works.
    if panel == PanelId::Console && shell.console_open {
        shell.console.focus = true;
    }
    None
}

#[cfg(not(feature = "egui-ui"))]
fn toggle_panel(_state: &mut WindowState, panel: PanelId) -> Option<String> {
    let name = match panel {
        PanelId::Debug => "debug",
        PanelId::Hud => "hud",
        PanelId::Sessions => "sessions",
        PanelId::Console => "console",
    };
    Some(format!("panel {name}: built without the egui overlay"))
}

#[cfg(feature = "egui-ui")]
fn sessions_refresh(state: &mut WindowState) -> Option<String> {
    let shell = &mut state.egui.as_mut()?.shell;
    let dirs = crate::launch_config::LaunchConfig::load_or_default().session_dirs();
    shell.session_dirs_report = glyph_session_dirs::describe(&dirs);
    let sessions = crate::agent_transcript::discovery::scan_session_dirs(&dirs);
    let reply = format!("sessions: {} found ({})", sessions.len(), shell.session_dirs_report);
    shell.discovered_sessions = Some(sessions);
    Some(reply)
}

#[cfg(not(feature = "egui-ui"))]
fn sessions_refresh(_state: &mut WindowState) -> Option<String> {
    Some("sessions refresh: built without the egui overlay (the browser is one of its windows)".to_string())
}

/// Step a live source (F3/F4/F5) and name where it is, for the title bar.
/// `None` when the run has no live source.
fn live_step_title(live: &Option<LiveSource>, live_step_ix: &mut usize, step: LiveStep) -> Option<String> {
    let live = live.as_ref()?;
    if let Some(sender) = &live.step {
        let _ = sender.send(step);
    }
    let count = live.step_count;
    *live_step_ix = match step {
        LiveStep::Next => {
            if count > 0 { (*live_step_ix + 1).min(count - 1) } else { 0 }
        }
        LiveStep::Prev => live_step_ix.saturating_sub(1),
        LiveStep::Reset => 0,
    };
    Some(if count > 0 {
        format!("fieldzed live — step {}/{}", *live_step_ix + 1, count)
    } else {
        "fieldzed live".to_string()
    })
}
