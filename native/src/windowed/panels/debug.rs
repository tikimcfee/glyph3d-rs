//! The Debug window (F1): camera, pick and grab readouts, the verb
//! buttons, the live LOD tuning, the layout controls and, inside it, the
//! file browser (`files.rs`). Moved out of state.rs (2026-10-24), a pure
//! move: every button pushes a command line (`bus::lines`), and the only
//! writes left are the probe cell's live tuning (the LOD sliders, the z
//! dial's own value), which offscreen never installs.

use crate::glyph_scene::{UiProbe, UiProbeState};
use super::super::bus::lines;
use super::super::ui::PANEL_VERBS;
use super::{ui_rgb, UiState};

pub(in crate::windowed) fn show(
    ctx: &egui::Context,
    probe_snap: Option<&UiProbeState>,
    ui_probe: Option<&UiProbe>,
    fps: f32,
    shell: &mut UiState,
    panel_lines: &mut Vec<String>,
) {
    let debug_open = &mut shell.debug_open;
    let scratch = &mut shell.scratch;
    // K1 leftover REMOVED (stage-k fix): the empty
    // `CentralPanel::default()` paints an OPAQUE full-viewport
    // panel_fill rect that blanketed the 3D scene — "background
    // layer" only affects input order, not fill. With real
    // windows present it is vestigial; no CentralPanel is needed
    // (egui::Window shows fine without one).
    // K3: the Debug window. Every button pushes a command line
    // (verb buttons push the --verb literal itself), parsed by
    // the CLI's own definition and run through the CLI's own
    // applier; the determinism chain never learns egui exists.
    egui::Window::new("Debug")
        .id(egui::Id::new("stage_k_debug_panel"))
        .open(debug_open)
        .default_pos(egui::pos2(10.0, 10.0))
        .default_size(egui::vec2(360.0, 520.0))
        .min_size(egui::vec2(280.0, 180.0))
        .resizable(true)
        .collapsible(true)
        .vscroll(true)
        .show(ctx, |ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
            ui.label(format!("FPS: {fps:.1}"));
            match &probe_snap {
                Some(snap) => {
                    let mode = match snap.camera_mode {
                        Some(crate::glyph_scene::CameraMode::Front { zoom }) => {
                            format!("Front(zoom {zoom:.2})")
                        }
                        Some(crate::glyph_scene::CameraMode::Orbit) => {
                            "Orbit".to_string()
                        }
                        Some(crate::glyph_scene::CameraMode::Fly) => "Fly".to_string(),
                        None => "—".to_string(),
                    };
                    ui.label(format!(
                        "camera: {mode} eye=({:.2},{:.2},{:.2}) yaw={:.3} pitch={:.3} rad ({:.1}°, {:.1}°)",
                        snap.eye[0], snap.eye[1], snap.eye[2], snap.yaw, snap.pitch,
                        snap.yaw.to_degrees(), snap.pitch.to_degrees(),
                    ));
                    // The pose as the argument that reproduces
                    // it: a view seen once becomes a golden
                    // view's command line.
                    if ui.button("copy pose (--cam-pose, degrees)").clicked() {
                        panel_lines.push(lines::COPY_POSE.to_string());
                    }
                    if snap.strategy.is_some() || snap.field_mode.is_some() {
                        let engine_str = snap
                            .strategy
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| "n/a".to_string());
                        let field_str = snap
                            .field_mode
                            .map(|m| format!("{m:?}"))
                            .unwrap_or_else(|| "n/a".to_string());
                        ui.label(format!("engine: {engine_str} | field: {field_str}"));
                    }
                    ui.label(
                        snap.last_pick
                            .as_deref()
                            .unwrap_or("pick: (none yet — left-click a glyph)"),
                    );
                    if let Some(active_zone) = &snap.active_zone {
                        ui.label(format!("carrel / zone: {active_zone}"));
                    }
                    if snap.grabbed_zone.is_some() || snap.grabbed_group.is_some() {
                        ui.colored_label(
                            ui_rgb(crate::config::settings().ui.grab_active),
                            format!(
                                "ACTIVE GRAB: {}",
                                if let Some(z) = &snap.grabbed_zone {
                                    format!("Zone '{z}' (move mouse to drag, wheel to scale, C to release)")
                                } else if let Some(g) = snap.grabbed_group {
                                    format!("File group {g} (move mouse to drag, wheel to scale, G to release)")
                                } else {
                                    "".to_string()
                                }
                            ),
                        );
                    }
                    ui.horizontal(|ui| {
                        if let Some(z) = &snap.grabbed_zone {
                            if ui.button(format!("Release Carrel '{z}' (C)")).clicked() {
                                panel_lines.push(lines::KEY_GRAB_CARREL.to_string());
                            }
                        } else if snap.active_zone.is_some()
                            && ui.button("Grab Entire Carrel (C)").clicked()
                        {
                            panel_lines.push(lines::KEY_GRAB_CARREL.to_string());
                        }

                        if let Some(g) = snap.grabbed_group {
                            if ui.button(format!("Release File #{g} (G)")).clicked() {
                                panel_lines.push(lines::KEY_GRAB_FILE.to_string());
                            }
                        } else if snap.last_pick.is_some()
                            && ui.button("Grab File (G)").clicked()
                        {
                            panel_lines.push(lines::KEY_GRAB_FILE.to_string());
                        }
                    });
                }
                None => {
                    ui.label("camera/pick: n/a (demo scene has no probe)");
                }
            }
            ui.separator();
            ui.label("verbs (each button fires its own text as a line):");
            ui.horizontal_wrapped(|ui| {
                for spec in PANEL_VERBS {
                    if ui.button(*spec).clicked() {
                        panel_lines.push(spec.to_string());
                    }
                }
            });
            // K4: live cull/LOD tuning — the sliders write the
            // shared probe cell; GlyphScene::render applies it to
            // CullState's Cell and the Params uniform before
            // culling (same frame). The readouts are the sums
            // GLYPH_CULL_DEBUG prints.
            //
            // C26 (2026-10-10): two ideas, two always-visible
            // sliders in plain words. Every threshold is "px per
            // text row" — how many on-screen pixels tall a row
            // appears — fixed, never camera-adapted. The panel
            // keeps Text detail >= Show glyphs (>= File rectangle
            // in visible mode): the handle that moved wins and
            // the others yield (`UiProbeState::keep_lod_order`),
            // so glyphs appear fuzzed first and sharpen on
            // approach. CLI/config values are applied as given.
            if let (Some(snap), Some(cell)) = (&probe_snap, &ui_probe) {
                ui.separator();
                ui.label("LOD — live, windowed only (offscreen keeps the [lod] settings); px = on-screen pixels per text row:");
                let visible = snap.field_mode == Some(glyph_field::GlyphFieldMode::Visible);
                let mut p = cell.borrow_mut();
                let detail = ui.add(
                    egui::Slider::new(&mut p.greek_onset_px, 2.0..=32.0)
                        .suffix(" px")
                        .text("Text detail — rows this tall get full curve detail; below, glyphs fuzz progressively"),
                );
                let fuzz = p.greeking;
                ui.horizontal(|ui| {
                    ui.checkbox(&mut p.greeking, "fuzz below it (greeking, anti-moiré)");
                    ui.add_enabled_ui(fuzz, |ui| {
                        ui.selectable_value(&mut p.greek_pure, false, "smooth");
                        ui.selectable_value(&mut p.greek_pure, true, "pure (hard cut, max FPS)");
                    });
                });
                // Range brackets the default (1.0) with ~2 octaves
                // each way; logarithmic because the threshold is
                // a perceptual scale.
                let show = ui.add(
                    egui::Slider::new(&mut p.lod_min_px, 0.25..=16.0)
                        .logarithmic(true)
                        .suffix(" px")
                        .text(if visible {
                            "Show glyphs — rows this tall become glyphs; below, a line is a wash"
                        } else {
                            "Show glyphs — rows this tall become glyphs; below, a file is its rectangle"
                        }),
                );
                // The visible field's second handle, under Show
                // glyphs: the file-rectangle-vs-line-wash tier.
                let rect = visible.then(|| {
                    ui.indent("lod_file_rectangle", |ui| {
                        ui.add(
                            egui::Slider::new(&mut p.lod_backdrop_px, 0.05..=4.0)
                                .logarithmic(true)
                                .suffix(" px")
                                .text("File rectangle (visible mode) — rows this tall get line washes; below, the whole file is one rectangle"),
                        )
                    })
                    .inner
                });
                if detail.changed() {
                    p.keep_lod_order(crate::glyph_scene::LodHandle::TextDetail);
                } else if show.changed() {
                    p.keep_lod_order(crate::glyph_scene::LodHandle::ShowGlyphs);
                } else if rect.is_some_and(|r| r.changed()) {
                    p.keep_lod_order(crate::glyph_scene::LodHandle::FileRectangle);
                }
                ui.horizontal(|ui| {
                    ui.checkbox(&mut p.file_backgrounds, "File card backgrounds");
                    ui.color_edit_button_rgba_unmultiplied(&mut p.file_bg_color);
                });
                ui.label(format!(
                    "cull: {} draw ranges, {} instances | {} backdrops",
                    snap.cull_ranges, snap.cull_instances, snap.cull_backdrops
                ));
                // The visible field's debug tint: a diagnostic
                // the stored modes' shaders never read.
                if visible {
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label("debug tint:");
                        for (mode, label) in [(0u32, "off"), (1, "by LOD tier"), (2, "by cull state")] {
                            if ui.selectable_label(p.debug_tint == mode, label).clicked() {
                                p.debug_tint = mode;
                            }
                        }
                    });
                }
            }
            // The layout dial (repo scenes only): the wrap
            // staircase's pitch. Unlike K4's cull input this IS
            // layout — applying re-runs load_repo and rebuilds
            // the scene — so it fires on drag release (or a typed
            // commit), never per tick: the JS system's
            // `grid.layout` was likewise a discrete refold
            // command, not a live drag.
            if let (Some(snap), Some(cell)) = (&probe_snap, &ui_probe) {
                let mut p = cell.borrow_mut();
                if let Some(spacing) = &mut p.z_wrap_spacing {
                    ui.separator();
                    ui.label(
                        "layout (repo) — applies on release; the scene rebuilds, \
                         pick/selection state resets:",
                    );
                    // The bar spans 0-1; the number beside it
                    // (drag or type) takes any pitch >= 0.
                    let resp = ui.add(
                        egui::Slider::new(spacing, 0.0..=1.0)
                            .clamping(egui::SliderClamping::Never)
                            .text("z_wrap_spacing × em (0 = flat, default 0.15)"),
                    );
                    *spacing = spacing.max(0.0);
                    if let Some([lo, hi]) = snap.z_extent {
                        ui.label(format!(
                            "field depth: z ∈ [{lo:.1}, {hi:.1}] world units"
                        ));
                    }
                    if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                        panel_lines.push(lines::z_wrap_spacing(*spacing));
                    }
                }
            }
            // Layout engine strategy selection — repo scenes only.
            if let Some(snap) = &probe_snap {
                if let Some(current_strategy) = snap.strategy {
                    ui.separator();
                    ui.label(
                        "layout engine (repo) — click switches backend & rebuilds:",
                    );
                    ui.horizontal_wrapped(|ui| {
                        for (strategy, label) in [
                            (crate::repo::Strategy::Hyper, "Hyper (CPU)"),
                            (crate::repo::Strategy::Direct, "Direct"),
                            (crate::repo::Strategy::Batched, "Batch"),
                        ] {
                            if ui.selectable_label(current_strategy == strategy, label).clicked()
                                && current_strategy != strategy
                            {
                                panel_lines.push(lines::flag("repo-engine", strategy));
                            }
                        }
                    });
                }
            }

            // Glyph field mode selection: the two stored formats
            // and the Visible field (no slots; experimental, M2).
            // The highlighted label is the mode actually BUILT
            // (the probe reads `field.mode()`), so a lane-limit
            // fallback shows as Instanced here.
            if let Some(snap) = &probe_snap {
                if let Some(current_mode) = snap.field_mode {
                    ui.separator();
                    ui.label(
                        "glyph field mode — click switches the field & rebuilds (visible: experimental, lays out per frame):",
                    );
                    ui.horizontal(|ui| {
                        for (mode, label) in [
                            (glyph_field::GlyphFieldMode::Derived, "Derived (20B)"),
                            (glyph_field::GlyphFieldMode::Instanced, "Instanced (32B)"),
                            (glyph_field::GlyphFieldMode::Visible, "Visible (no slots)"),
                        ] {
                            if ui.selectable_label(current_mode == mode, label).clicked()
                                && current_mode != mode
                            {
                                panel_lines.push(lines::flag("field-mode", mode));
                            }
                        }
                    });
                }
            }

            // The sequence pass toggle — repo AND text scenes
            // (text scenes seed the probe from the staging
            // choice, no pick context needed). The same rebuild
            // arm as the dial: a discrete refold per click,
            // never a per-tick write.
            if let Some(snap) = &probe_snap {
                if let Some(on) = snap.cluster_mode {
                    ui.separator();
                    ui.label(
                        "sequence pass — click toggles; the scene rebuilds, \
                         pick/selection state resets:",
                    );
                    if ui
                        .button(if on { "cluster mode: ON" } else { "cluster mode: off" })
                        .clicked()
                    {
                        let current = if on {
                            crate::fold::ClusterMode::Cluster
                        } else {
                            crate::fold::ClusterMode::Leader
                        };
                        panel_lines.push(lines::flag("cluster-mode", super::super::app::other_cluster_mode(current)));
                    }
                }
            }
            // Canvas layout toggle (shelf vs carrel) — repo scenes only.
            if let Some(snap) = &probe_snap {
                if let Some(mode) = snap.layout_mode {
                    ui.separator();
                    ui.label(
                        "canvas layout (F6) — click toggles; the scene rebuilds, \
                         pick/selection state resets:",
                    );
                    let label = match mode {
                        crate::repo::RepoLayoutMode::Shelf => "layout mode: SHELF (click for carrel)",
                        crate::repo::RepoLayoutMode::Carrel => "layout mode: CARREL (click for library)",
                        crate::repo::RepoLayoutMode::Library => "layout mode: LIBRARY (click for shelf)",
                    };
                    if ui.button(label).clicked() {
                        panel_lines.push(lines::flag("layout-mode", super::super::app::next_layout_mode(mode)));
                    }
                }
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("agent sessions (F7):");
                if ui.button("Browse Sessions 📂").clicked() {
                    panel_lines.push(lines::PANEL_SESSIONS.to_string());
                }
            });
            ui.separator();
            ui.label("K2 typing test — WASD/h/g/t/x here must not move the scene:");
            ui.text_edit_singleline(scratch);
            super::files::show(ui, probe_snap, ui_probe, &mut shell.filter, &mut shell.selected_group, panel_lines);
        });
}
