//! The Agent Sessions browser (F7): the scanned transcripts, filtered by
//! harness and text; Load is an `--agent-session` line, Refresh `sessions
//! refresh`. Moved out of state.rs (2026-10-24), a pure move.

use super::super::bus::lines;
use super::{ui_rgb, UiState};

/// "just now", "5m ago", ... for a session's modification time.
fn format_relative_time(dur: std::time::Duration) -> String {
    let secs = dur.as_secs();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

pub(in crate::windowed) fn show(ctx: &egui::Context, shell: &mut UiState, panel_lines: &mut Vec<String>) {
    let session_browser_open = &mut shell.session_browser_open;
    let session_filter_text = &mut shell.session_filter_text;
    let session_filter_harness = &mut shell.session_filter_harness;
    let discovered_sessions = &shell.discovered_sessions;
    let session_dirs_report = &shell.session_dirs_report;
    if *session_browser_open {
        // The first open scans, through the bus like the button;
        // this frame shows "not scanned yet", the next the list.
        if discovered_sessions.is_none() {
            panel_lines.push(lines::SESSIONS_REFRESH.to_string());
        }

        egui::Window::new("Agent Sessions (F7)")
            .id(egui::Id::new("agent_session_browser"))
            .open(session_browser_open)
            .default_pos(egui::pos2(50.0, 50.0))
            .default_size(egui::vec2(580.0, 480.0))
            .min_size(egui::vec2(360.0, 240.0))
            .resizable(true)
            .collapsible(true)
            .show(ctx, |ui| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                // 1. Controls bar
                ui.horizontal(|ui| {
                    ui.label("Search:");
                    ui.text_edit_singleline(session_filter_text);
                    if !session_filter_text.is_empty() && ui.button("✖").clicked() {
                        session_filter_text.clear();
                    }
                    if ui.button("⟳ Refresh").clicked() {
                        panel_lines.push(lines::SESSIONS_REFRESH.to_string());
                    }
                });

                // 2. Filter tabs
                ui.horizontal(|ui| {
                    ui.selectable_value(
                        session_filter_harness,
                        crate::agent_transcript::discovery::SessionHarnessFilter::All,
                        "All",
                    );
                    ui.selectable_value(
                        session_filter_harness,
                        crate::agent_transcript::discovery::SessionHarnessFilter::ClaudeCode,
                        "Claude Code",
                    );
                    ui.selectable_value(
                        session_filter_harness,
                        crate::agent_transcript::discovery::SessionHarnessFilter::Antigravity,
                        "Antigravity",
                    );
                    ui.selectable_value(
                        session_filter_harness,
                        crate::agent_transcript::discovery::SessionHarnessFilter::KimiCode,
                        "Kimi",
                    );
                });
                ui.separator();
                ui.small(format!("Scanning: {session_dirs_report}"));

                let Some(all_sessions) = discovered_sessions.as_ref() else {
                    ui.label("No sessions scanned yet.");
                    return;
                };
                if all_sessions.is_empty() {
                    ui.label(
                        "No sessions found in those directories. Point claude_projects_dir, \
                         antigravity_brain_dir or kimi_sessions_dir in launch_config.toml \
                         elsewhere, or set one to \"\" to skip that app.",
                    );
                    return;
                }

                let filter_lower = session_filter_text.to_lowercase();
                let filtered: Vec<&crate::agent_transcript::discovery::DiscoveredSession> = all_sessions
                    .iter()
                    .filter(|s| {
                        match *session_filter_harness {
                            crate::agent_transcript::discovery::SessionHarnessFilter::All => true,
                            crate::agent_transcript::discovery::SessionHarnessFilter::ClaudeCode => {
                                s.harness == crate::agent_transcript::types::HarnessKind::ClaudeCode
                            }
                            crate::agent_transcript::discovery::SessionHarnessFilter::Antigravity => {
                                s.harness == crate::agent_transcript::types::HarnessKind::Antigravity
                            }
                            crate::agent_transcript::discovery::SessionHarnessFilter::KimiCode => {
                                s.harness == crate::agent_transcript::types::HarnessKind::KimiCode
                            }
                        }
                    })
                    .filter(|s| {
                        if filter_lower.is_empty() {
                            return true;
                        }
                        s.title.to_lowercase().contains(&filter_lower)
                            || s.id.to_lowercase().contains(&filter_lower)
                            || s.project_name.as_deref().unwrap_or("").to_lowercase().contains(&filter_lower)
                            || s.path.to_string_lossy().to_lowercase().contains(&filter_lower)
                    })
                    .collect();

                ui.label(format!("Showing {} of {} sessions", filtered.len(), all_sessions.len()));
                ui.separator();

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if filtered.is_empty() {
                            ui.label("No matching sessions found.");
                            return;
                        }
                        for s in filtered {
                            ui.group(|ui| {
                                ui.horizontal(|ui| {
                                    match s.harness {
                                        crate::agent_transcript::types::HarnessKind::ClaudeCode => {
                                            ui.colored_label(ui_rgb(crate::config::settings().ui.harness_claude), "[Claude]");
                                        }
                                        crate::agent_transcript::types::HarnessKind::Antigravity => {
                                            ui.colored_label(ui_rgb(crate::config::settings().ui.harness_antigravity), "[Antigravity]");
                                        }
                                        crate::agent_transcript::types::HarnessKind::KimiCode => {
                                            ui.colored_label(ui_rgb(crate::config::settings().ui.harness_kimi), "[Kimi]");
                                        }
                                        crate::agent_transcript::types::HarnessKind::Generic => {
                                            ui.colored_label(ui_rgb(crate::config::settings().ui.harness_generic), "[Agent]");
                                        }
                                    }
                                    if let Some(proj) = &s.project_name {
                                        ui.colored_label(ui_rgb(crate::config::settings().ui.project_name), format!("📂 {proj}"));
                                    }
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.button("▶ Load").clicked() {
                                            panel_lines.push(lines::agent_session(&s.path));
                                        }
                                        if let Some(m) = s.modified {
                                            if let Ok(dur) = std::time::SystemTime::now().duration_since(m) {
                                                ui.label(egui::RichText::new(format_relative_time(dur)).weak().small());
                                            }
                                        }
                                    });
                                });
                                ui.label(egui::RichText::new(&s.title).strong());
                                ui.horizontal(|ui| {
                                    let prefix = crate::agent_transcript::types::truncate_chars(&s.id, 16);
                                    ui.label(egui::RichText::new(format!("ID: {prefix}…")).weak().small());
                                    let kb = (s.file_size_bytes as f64) / 1024.0;
                                    ui.label(egui::RichText::new(format!("{kb:.1} KB")).weak().small());
                                });
                            });
                        }
                    });
            });
    }
}
