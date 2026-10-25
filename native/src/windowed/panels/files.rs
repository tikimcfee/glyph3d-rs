//! The file browser, shown inside the Debug window: a filtered, virtualized
//! list of the scene's files. A row click is `pick group N` then `fly group
//! N`; the browser's one cell write is `browser_rows`, the rows it drew,
//! which the scene refreshes live poses for (view feedback, not an intent).
//! Moved out of state.rs (2026-10-24), a pure move.

use crate::glyph_scene::{UiProbe, UiProbeState};
use super::super::bus::lines;

pub(in crate::windowed) fn show(
    ui: &mut egui::Ui,
    probe_snap: Option<&UiProbeState>,
    ui_probe: Option<&UiProbe>,
    filter: &mut String,
    selected_group: &mut Option<u32>,
    panel_lines: &mut Vec<String>,
) {
    // K5: group browser — FLAT virtualized list
    // (ScrollArea::show_rows builds only the visible
    // rows; a CollapsingHeader-per-directory tree is NOT
    // virtualized and would be the classic egui
    // large-list trap at ~1.3k files). Vanilla egui — no
    // new deps (fence 2). Row click = `pick group N`
    // then `fly group N` (the Front framing of the
    // file's live box, bus::fly_pose); a click on the
    // selected row is `pick clear`.
    if let Some(snap) = probe_snap {
        if !snap.files.is_empty() {
            ui.separator();
            ui.label(format!(
                "files: {} (click = select + fly to file)",
                snap.files.len()
            ));
            ui.horizontal(|ui| {
                ui.label("filter:");
                ui.text_edit_singleline(filter);
            });
            let needle = filter.to_lowercase();
            let idxs: Vec<usize> = snap
                .files
                .iter()
                .enumerate()
                .filter(|(_, r)| {
                    needle.is_empty()
                        || r.rel_path.to_lowercase().contains(&needle)
                })
                .map(|(i, _)| i)
                .collect();
            let row_h = ui.text_style_height(&egui::TextStyle::Body);
            let mut shown: Vec<usize> = Vec::new();
            egui::ScrollArea::vertical()
                .max_height(280.0)
                .show_rows(ui, row_h, idxs.len(), |ui, range| {
                    shown.extend_from_slice(&idxs[range.clone()]);
                    for &i in &idxs[range] {
                        let row = &snap.files[i];
                        let dynst =
                            snap.file_dyn.get(i).copied().unwrap_or_default();
                        let depth = row.rel_path.matches('/').count();
                        ui.horizontal(|ui| {
                            // Tint swatch drawn, not
                            // typeset (no font-glyph
                            // dependency).
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(10.0, 10.0),
                                egui::Sense::hover(),
                            );
                            ui.painter().rect_filled(
                                rect,
                                2.0,
                                egui::Color32::from_rgb(
                                    dynst.tint[0],
                                    dynst.tint[1],
                                    dynst.tint[2],
                                ),
                            );
                            let label = format!(
                                "{}{}{}",
                                "  ".repeat(depth),
                                row.rel_path,
                                if dynst.hidden { "  (hidden)" } else { "" },
                            );
                            let resp = ui.selectable_label(
                                *selected_group == Some(row.group_id),
                                label,
                            );
                            if resp.clicked() {
                                if *selected_group == Some(row.group_id) {
                                    *selected_group = None;
                                    panel_lines.push(lines::PICK_CLEAR.to_string());
                                } else {
                                    *selected_group = Some(row.group_id);
                                    panel_lines.push(lines::pick_group(row.group_id));
                                    panel_lines.push(lines::fly_group(row.group_id));
                                }
                            }
                        });
                    }
                });
            // The scene refreshes these rows' poses
            // next frame (UiProbeState::browser_rows): a
            // sanctioned cell write, view feedback the
            // scene reads, not an intent.
            if let Some(cell) = &ui_probe {
                cell.borrow_mut().browser_rows = shown;
            }
        }
    }
}
