//! The Agent Carrel window: beat navigation (scene keys, `key ...` lines
//! until those keys are verbs), the window limits (one flag line), and the
//! active beat's summary. Moved out of state.rs (2026-10-24), a pure move.

use crate::glyph_scene::UiProbeState;
use super::super::bus::lines;
use super::ui_rgb;

pub(in crate::windowed) fn show(ctx: &egui::Context, probe_snap: Option<&UiProbeState>, panel_lines: &mut Vec<String>) {
    if let Some(carrel) = probe_snap.and_then(|s| s.carrel.as_ref()) {
        egui::Window::new(format!("Agent Carrel: {}", carrel.session_id))
            .id(egui::Id::new("agent_carrel_hud"))
            .default_pos(egui::pos2(20.0, 20.0))
            .default_size(egui::vec2(380.0, 480.0))
            .min_size(egui::vec2(280.0, 160.0))
            .resizable(true)
            .collapsible(true)
            .vscroll(true)
            .show(ctx, |ui| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                ui.horizontal(|ui| {
                    if ui.button("⏮ Oldest (End)").clicked() {
                        panel_lines.push(lines::KEY_CARREL_OLDEST.to_string());
                    }

                    let total_beats = carrel.beat_count.max(carrel.turn_count);

                    if ui.button("◀ Prev (P/←/〔)").clicked() && total_beats > 0 {
                        panel_lines.push(lines::KEY_CARREL_PREV.to_string());
                    }

                    if carrel.beat_count > 0 {
                        ui.label(egui::RichText::new(format!(
                            "Beat {} / {} (T{})",
                            carrel.active_beat + 1,
                            carrel.beat_count,
                            carrel.active_turn + 1
                        )).strong());
                    } else {
                        ui.label(egui::RichText::new(format!("Turn {} / {}", carrel.active_turn + 1, carrel.turn_count)).strong());
                    }

                    if ui.button("Next (N/→/〕) ▶").clicked() && total_beats > 0 {
                        panel_lines.push(lines::KEY_CARREL_NEXT.to_string());
                    }

                    if ui.button("Latest ⏭ (Home)").clicked() {
                        panel_lines.push(lines::KEY_CARREL_LATEST.to_string());
                    }

                    ui.separator();
                    if ui.button("Mode (V)").clicked() {
                        panel_lines.push(lines::KEY_CARREL_MODE.to_string());
                    }
                    ui.separator();
                    if ui.button("Sessions (F7) 📂").clicked() {
                        panel_lines.push(lines::PANEL_SESSIONS.to_string());
                    }
                });

                // Timeline & Window Information
                ui.separator();
                let total_beats = carrel.beat_count.max(carrel.turn_count);
                let (w_old, w_new) = carrel.window_item_range;
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!(
                        "Window: [{}..{}] of {}",
                        w_old, w_new, total_beats
                    )).strong());
                    if carrel.layout_options.deck_scroll_offset > 0 {
                        ui.colored_label(
                            ui_rgb(crate::config::settings().ui.history_back),
                            format!("(-{} back in history)", carrel.layout_options.deck_scroll_offset),
                        );
                    } else {
                        ui.colored_label(
                            ui_rgb(crate::config::settings().ui.history_live),
                            "(LIVE / LATEST)",
                        );
                    }
                });

                // Collapsible Sliders for limits and offsets
                let mut current_opts = carrel.layout_options;
                let mut opts_changed = false;
                ui.collapsing("Window Limits & Time Travel", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Deck Window Limit:");
                        let resp = ui.add(egui::Slider::new(&mut current_opts.deck_window_limit, 5..=100).text("cards"));
                        if resp.drag_stopped() || resp.lost_focus() {
                            opts_changed = true;
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Time Scroll (K):");
                        let max_k = carrel.max_deck_scroll;
                        let resp = ui.add(egui::Slider::new(&mut current_opts.deck_scroll_offset, 0..=max_k).text("turns back"));
                        if resp.drag_stopped() || resp.lost_focus() {
                            opts_changed = true;
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Desk Revision Limit:");
                        let resp = ui.add(egui::Slider::new(&mut current_opts.desk_revision_limit, 3..=50).text("revs/file"));
                        if resp.drag_stopped() || resp.lost_focus() {
                            opts_changed = true;
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Max File Stacks:");
                        let resp = ui.add(egui::Slider::new(&mut current_opts.max_file_stacks, 5..=50).text("files"));
                        if resp.drag_stopped() || resp.lost_focus() {
                            opts_changed = true;
                        }
                    });
                });

                if opts_changed && current_opts != carrel.layout_options {
                    panel_lines.push(lines::carrel(&current_opts));
                }
                if !carrel.beat_summary.is_empty() {
                    ui.separator();
                    ui.label(egui::RichText::new("Active Beat:").heading());
                    ui.colored_label(ui_rgb(crate::config::settings().ui.beat_summary), &carrel.beat_summary);
                }
                if !carrel.prompt_summary.is_empty() {
                    ui.separator();
                    ui.collapsing("Turn Prompt", |ui| {
                        ui.label(&carrel.prompt_summary);
                    });
                }
                if !carrel.touched_files.is_empty() {
                    ui.separator();
                    ui.collapsing(format!("Workdesk Files ({})", carrel.touched_files.len()), |ui| {
                        for (path, act, total) in &carrel.touched_files {
                            ui.horizontal(|ui| {
                                ui.colored_label(ui_rgb(crate::config::settings().ui.workdesk_bullet), "•");
                                ui.label(format!("{path} (active: R{act}, total: {total})"));
                            });
                        }
                    });
                }
            });
    }
}
