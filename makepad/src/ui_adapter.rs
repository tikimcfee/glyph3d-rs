//! The glue between the panels' model (`ui_model`) and the engine: every
//! frame the scene's probe (`UiProbeState`, written by the scene's render),
//! the carrel and the session list become one `G3UiState`; every
//! `G3UiAction` a panel emits becomes the engine call the egui panel makes
//! for it (`native/src/windowed/state.rs` is the spec) — a probe write the
//! scene applies next frame, a verb through `parse_verb`, one of the
//! scene's keys, a pick, or a `RelayoutRequest` the host applies between
//! frames.

use std::rc::Rc;

use glyph3d_native::glyph_scene::{fit_distance, CameraMode, LodHandle, PickCommand, UiCarrelState, UiProbeState};
use glyph3d_native::config::EnvironmentMode;
use glyph3d_native::relayout::RelayoutRequest;
use glyph3d_native::repo::{RepoLayoutMode, Strategy};
use glyph3d_native::scene::SceneKey;
use glyph_field::GlyphFieldMode;

use crate::host::EngineHost;
use crate::ui_model::*;

/// The verb buttons: CLI `--verb` literals, parsed by the same
/// `parse_verb` the CLI uses, so a button and its flag agree by
/// construction. The zero-argument forms only, as in the egui panel.
const PANEL_VERBS: &[&str] = &["recolor-glyph", "recolor-line", "tint-cycle", "hide-group", "show-group", "toggle-hidden"];

/// The layout engines the panel offers (`naive` is the CLI's alone).
const STRATEGIES: &[(Strategy, &str)] = &[(Strategy::Hyper, "Hyper (CPU)"), (Strategy::Direct, "Direct"), (Strategy::Batched, "Batch")];

/// The glyph field modes the panel offers: one since the Instanced field's
/// retirement (2026-10-31).
const FIELD_MODES: &[(GlyphFieldMode, &str)] = &[(GlyphFieldMode::Derived, "Derived (20B)")];

/// The camera as the `--cam-pose` argument that reproduces it (degrees).
pub fn cam_pose_arg(eye: [f32; 3], yaw: f32, pitch: f32) -> String {
    format!(
        "--cam-pose {:.3} {:.3} {:.3} {:.3} {:.3}",
        eye[0],
        eye[1],
        eye[2],
        yaw.to_degrees(),
        pitch.to_degrees()
    )
}

fn choices<T: PartialEq + Copy>(options: &[(T, &str)], current: T) -> Vec<G3Choice> {
    options
        .iter()
        .map(|(value, label)| G3Choice { label: label.to_string(), selected: *value == current })
        .collect()
}

/// The field HUD (F8), line for line as the egui overlay draws it.
fn hud_lines(snap: &UiProbeState, fps: f32) -> Vec<String> {
    let mode = snap.field_mode.map(|m| m.to_string()).unwrap_or_else(|| "n/a".to_string());
    let strategy = snap.strategy.map(|s| s.to_string()).unwrap_or_else(|| "n/a".to_string());
    let mut lines = vec![
        format!("field {mode} | engine {strategy} | {fps:.1} fps"),
        format!(
            "segments {} ({} hidden) | backdrops {} | cull {:.2} ms CPU",
            snap.segments, snap.hidden_segments, snap.cull_backdrops, snap.cull_cpu_ms
        ),
    ];
    lines.push(format!("instances {} in {} draw ranges", snap.cull_instances, snap.cull_ranges));
    if snap.selection.is_some() || snap.pick_key.is_some() {
        lines.push(format!(
            "selection {} | pick {}",
            snap.selection.as_deref().unwrap_or("none"),
            snap.pick_key.as_deref().unwrap_or("none")
        ));
    }
    lines
}

/// The browser's rows, converted once per scene: the probe's `files` are
/// static for a scene's life (an `Rc` the probe keeps), so the rows are
/// rebuilt only when that `Rc` is a new one.
#[derive(Default)]
pub struct FileRows {
    /// The probe's `files` the rows were made from, held so its address
    /// cannot be reused by another.
    source: Option<Rc<dyn std::any::Any>>,
    rows: Rc<Vec<G3FileRow>>,
}

impl FileRows {
    fn of(&mut self, snap: &UiProbeState) -> Rc<Vec<G3FileRow>> {
        let files = &snap.files;
        let same = self.source.as_ref().is_some_and(|s| std::ptr::addr_eq(Rc::as_ptr(s), Rc::as_ptr(files)));
        if !same {
            self.rows = Rc::new(
                files
                    .iter()
                    .map(|f| G3FileRow { rel_path: f.rel_path.clone(), depth: f.rel_path.matches('/').count(), group_id: f.group_id })
                    .collect(),
            );
            let source: Rc<dyn std::any::Any> = files.clone();
            self.source = Some(source);
        }
        self.rows.clone()
    }
}

fn debug_state(snap: &UiProbeState, selected_group: Option<u32>, files: Rc<Vec<G3FileRow>>) -> G3DebugState {
    let camera = match snap.camera_mode {
        Some(CameraMode::Front { zoom }) => format!("Front(zoom {zoom:.2})"),
        Some(CameraMode::Orbit) => "Orbit".to_string(),
        Some(CameraMode::Fly) => "Fly".to_string(),
        None => "—".to_string(),
    };
    let ui_colors = &glyph3d_native::config::settings().ui;
    let grab_line = match (&snap.grabbed_zone, snap.grabbed_group) {
        (Some(z), _) => Some(format!("ACTIVE GRAB: Zone '{z}' (move mouse to drag, wheel to scale, C to release)")),
        (None, Some(g)) => Some(format!("ACTIVE GRAB: File group {g} (move mouse to drag, wheel to scale, G to release)")),
        (None, None) => None,
    };
    let carrel_grab_button = match &snap.grabbed_zone {
        Some(z) => Some(format!("Release Carrel '{z}' (C)")),
        None => snap.active_zone.as_ref().map(|_| "Grab Entire Carrel (C)".to_string()),
    };
    let file_grab_button = match snap.grabbed_group {
        Some(g) => Some(format!("Release File #{g} (G)")),
        None => snap.last_pick.as_ref().map(|_| "Grab File (G)".to_string()),
    };
    let lod = G3LodState {
        text_detail_px: snap.greek_onset_px,
        show_glyphs_px: snap.lod_min_px,
        greeking: snap.greeking,
        greek_pure: snap.greek_pure,
        file_backgrounds: snap.file_backgrounds,
        file_bg_color: snap.file_bg_color,
        cull_line: format!(
            "cull: {} draw ranges, {} instances | {} backdrops",
            snap.cull_ranges, snap.cull_instances, snap.cull_backdrops
        ),
    };
    G3DebugState {
        camera_line: format!(
            "camera: {camera} eye=({:.2},{:.2},{:.2}) yaw={:.3} pitch={:.3} rad ({:.1}°, {:.1}°)",
            snap.eye[0],
            snap.eye[1],
            snap.eye[2],
            snap.yaw,
            snap.pitch,
            snap.yaw.to_degrees(),
            snap.pitch.to_degrees(),
        ),
        engine_line: (snap.strategy.is_some() || snap.field_mode.is_some()).then(|| {
            format!(
                "engine: {} | field: {}",
                snap.strategy.map(|s| s.to_string()).unwrap_or_else(|| "n/a".to_string()),
                snap.field_mode.map(|m| format!("{m:?}")).unwrap_or_else(|| "n/a".to_string()),
            )
        }),
        pick_line: snap.last_pick.clone().unwrap_or_else(|| "pick: (none yet — left-click a glyph)".to_string()),
        zone_line: snap.active_zone.as_ref().map(|z| format!("carrel / zone: {z}")),
        grab_line,
        grab_color: ui_colors.grab_active,
        carrel_grab_button,
        file_grab_button,
        verbs: PANEL_VERBS.iter().map(|v| v.to_string()).collect(),
        can_clear_pick: snap.last_pick.is_some() || snap.selection.is_some(),
        environment_label: match snap.environment {
            EnvironmentMode::Off => "environment: Off (B for ground)",
            EnvironmentMode::Ground => "environment: Ground (B for off)",
        }
        .to_string(),
        lod,
        z_wrap_spacing: snap.z_wrap_spacing,
        z_extent_line: snap.z_extent.map(|[lo, hi]| format!("field depth: z ∈ [{lo:.1}, {hi:.1}] world units")),
        strategies: snap.strategy.map(|s| choices(STRATEGIES, s)).unwrap_or_default(),
        field_modes: snap.field_mode.map(|m| choices(FIELD_MODES, m)).unwrap_or_default(),
        cluster_label: snap.cluster_mode.map(|on| if on { "cluster mode: ON" } else { "cluster mode: off" }.to_string()),
        layout_label: snap.layout_mode.map(|mode| {
            match mode {
                RepoLayoutMode::Shelf => "layout mode: SHELF (click for carrel)",
                RepoLayoutMode::Carrel => "layout mode: CARREL (click for library)",
                RepoLayoutMode::Library => "layout mode: LIBRARY (click for shelf)",
            }
            .to_string()
        }),
        files,
        file_dyn: snap.file_dyn.iter().map(|d| G3FileDyn { hidden: d.hidden, tint: d.tint }).collect(),
        selected_group,
    }
}

fn carrel_state(c: &UiCarrelState) -> G3CarrelUiState {
    let ui_colors = &glyph3d_native::config::settings().ui;
    let total_beats = c.beat_count.max(c.turn_count);
    let (w_old, w_new) = c.window_item_range;
    let back = c.layout_options.deck_scroll_offset;
    G3CarrelUiState {
        session_id: c.session_id.clone(),
        position_label: if c.beat_count > 0 {
            format!("Beat {} / {} (T{})", c.active_beat + 1, c.beat_count, c.active_turn + 1)
        } else {
            format!("Turn {} / {}", c.active_turn + 1, c.turn_count)
        },
        window_label: format!("Window: [{w_old}..{w_new}] of {total_beats}"),
        history_label: if back > 0 { format!("(-{back} back in history)") } else { "(LIVE / LATEST)".to_string() },
        history_color: if back > 0 { ui_colors.history_back } else { ui_colors.history_live },
        options: G3CarrelOptions {
            deck_window_limit: c.layout_options.deck_window_limit,
            deck_scroll_offset: c.layout_options.deck_scroll_offset,
            desk_revision_limit: c.layout_options.desk_revision_limit,
            desk_scroll_offset: c.layout_options.desk_scroll_offset,
            max_file_stacks: c.layout_options.max_file_stacks,
        },
        max_deck_scroll: c.max_deck_scroll,
        max_desk_scroll: c.max_desk_scroll,
        beat_summary: c.beat_summary.clone(),
        beat_color: ui_colors.beat_summary,
        prompt_summary: c.prompt_summary.clone(),
        touched_files: c
            .touched_files
            .iter()
            .map(|(path, act, total)| format!("{path} (active: R{act}, total: {total})"))
            .collect(),
    }
}

/// The panels' state this frame.
pub fn ui_state(host: &EngineHost, file_rows: &mut FileRows) -> G3UiState {
    let snap = host.probe.as_ref().map(|p| p.borrow().clone());
    G3UiState {
        fps: host.fps,
        hud_open: host.hud_open,
        debug_open: host.debug_open,
        sessions_open: host.sessions_open,
        hud_lines: snap.as_ref().map(|s| hud_lines(s, host.fps)).unwrap_or_default(),
        debug: snap.as_ref().map(|s| debug_state(s, host.scene.selected_group_id(), file_rows.of(s))),
        carrel: snap.as_ref().and_then(|s| s.carrel.as_ref()).map(carrel_state),
        sessions: host.sessions.state(),
    }
}

/// Write the live controls into the probe; the scene's render applies
/// them before it culls the next frame.
fn with_probe(host: &EngineHost, write: impl FnOnce(&mut UiProbeState)) {
    if let Some(probe) = &host.probe {
        write(&mut probe.borrow_mut());
    }
}

/// Do what a panel asked.
pub fn apply(host: &mut EngineHost, action: G3UiAction) {
    let relayout = |host: &mut EngineHost, req: RelayoutRequest| host.pending_relayout = Some(req);
    match action {
        G3UiAction::TogglePanel(G3Panel::Hud) => host.hud_open = !host.hud_open,
        G3UiAction::TogglePanel(G3Panel::Debug) => host.debug_open = !host.debug_open,
        G3UiAction::TogglePanel(G3Panel::Sessions) => {
            host.sessions_open = !host.sessions_open;
            if host.sessions_open {
                host.sessions.ensure_listed();
            }
        }
        G3UiAction::CopyPose => match host.scene.cam_pose() {
            Some((eye, yaw, pitch)) => println!("{}", cam_pose_arg(eye, yaw, pitch)),
            None => println!("pose: this scene has no fly camera"),
        },
        G3UiAction::ToggleCarrelGrab => host.press(SceneKey::C),
        G3UiAction::ToggleFileGrab => host.press(SceneKey::G),
        G3UiAction::Verb(spec) => match glyph3d_native::parse_verb(&spec) {
            Ok(verb) => {
                if let Some(line) = host.scene.apply_verb(&verb) {
                    println!("{line}");
                }
            }
            Err(e) => println!("verb {spec:?}: {e}"),
        },
        G3UiAction::ClearPick => {
            host.scene.apply_pick(&PickCommand::Clear);
        }
        G3UiAction::ToggleEnvironment => host.press(SceneKey::B),
        // The handle that moved wins and the other thresholds yield
        // (`keep_lod_order`), as in the egui panel.
        G3UiAction::SetTextDetailPx(px) => with_probe(host, |p| {
            p.greek_onset_px = px;
            p.keep_lod_order(LodHandle::TextDetail);
        }),
        G3UiAction::SetShowGlyphsPx(px) => with_probe(host, |p| {
            p.lod_min_px = px;
            p.keep_lod_order(LodHandle::ShowGlyphs);
        }),
        G3UiAction::SetGreeking(on) => with_probe(host, |p| p.greeking = on),
        G3UiAction::SetGreekPure(on) => with_probe(host, |p| p.greek_pure = on),
        G3UiAction::SetFileBackgrounds(on) => with_probe(host, |p| p.file_backgrounds = on),
        G3UiAction::SetFileBgColor(rgba) => with_probe(host, |p| p.file_bg_color = rgba),
        G3UiAction::SetZWrapSpacing(z) => {
            with_probe(host, |p| p.z_wrap_spacing = Some(z));
            relayout(host, RelayoutRequest { z_wrap_spacing: Some(z), ..Default::default() });
        }
        G3UiAction::SetStrategy(i) => {
            if let Some((strategy, _)) = STRATEGIES.get(i) {
                relayout(host, RelayoutRequest { set_strategy: Some(*strategy), ..Default::default() });
            }
        }
        G3UiAction::SetFieldMode(i) => {
            if let Some((mode, _)) = FIELD_MODES.get(i) {
                relayout(host, RelayoutRequest { set_field_mode: Some(*mode), ..Default::default() });
            }
        }
        G3UiAction::ToggleCluster => relayout(host, RelayoutRequest { toggle_cluster: true, ..Default::default() }),
        G3UiAction::ToggleLayout => relayout(host, RelayoutRequest { toggle_layout: true, ..Default::default() }),
        G3UiAction::SetCarrelOptions(o) => {
            let Some(current) = host.probe.as_ref().and_then(|p| p.borrow().carrel.as_ref().map(|c| c.layout_options)) else {
                return;
            };
            let mut opts = current;
            opts.deck_window_limit = o.deck_window_limit;
            opts.deck_scroll_offset = o.deck_scroll_offset;
            opts.desk_revision_limit = o.desk_revision_limit;
            opts.desk_scroll_offset = o.desk_scroll_offset;
            opts.max_file_stacks = o.max_file_stacks;
            if opts != current {
                relayout(host, RelayoutRequest { carrel_options: Some(opts), ..Default::default() });
            }
        }
        G3UiAction::BrowserRows(rows) => with_probe(host, |p| p.browser_rows = rows),
        G3UiAction::PickFile(row) => pick_file(host, row),
        G3UiAction::CarrelOldest => host.press(SceneKey::End),
        G3UiAction::CarrelPrev => host.press(SceneKey::BracketLeft),
        G3UiAction::CarrelNext => host.press(SceneKey::BracketRight),
        G3UiAction::CarrelLatest => host.press(SceneKey::Home),
        G3UiAction::CarrelMode => host.press(SceneKey::V),
        G3UiAction::RefreshSessions => host.sessions.refresh(),
        G3UiAction::SetSessionFilter(text) => host.sessions.set_filter(text),
        G3UiAction::SetSessionHarness(tab) => host.sessions.set_harness(tab),
        G3UiAction::LoadSession(path) => {
            if let Some(req) = host.sessions.load(&path) {
                relayout(host, req);
            }
        }
    }
}

/// A browser row clicked: the selected row again clears the pick; any other
/// selects its file and flies the camera to face it, fitted to the
/// viewport (the Front camera's framing, `camera_eye_target`).
fn pick_file(host: &mut EngineHost, row: usize) {
    let Some((group_id, dynst)) = host.probe.as_ref().and_then(|p| {
        let p = p.borrow();
        Some((p.files.get(row)?.group_id, p.file_dyn.get(row).copied().unwrap_or_default()))
    }) else {
        return;
    };
    if host.scene.selected_group_id() == Some(group_id) {
        host.scene.apply_pick(&PickCommand::Clear);
        return;
    }
    host.scene.apply_pick(&PickCommand::Group(group_id));
    let (w, h) = host.viewport();
    let aspect = w as f32 / h.max(1) as f32;
    let dist = fit_distance(dynst.half[1].max(dynst.half[0] / aspect));
    host.scene.set_cam_pose([dynst.center[0], dynst.center[1], dist], 0.0, 0.0);
}
