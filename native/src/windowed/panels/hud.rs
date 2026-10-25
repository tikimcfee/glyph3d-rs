//! The field HUD (F8): the always-on top-left readout of what the glyph
//! field is and what it did this frame, and `hud_line`, its one-line form
//! the self-tests print. Moved out of state.rs (2026-10-24), a pure move.

use crate::glyph_scene::UiProbeState;

/// The field HUD's one-line form: what the field is and what it did this
/// frame. Drawn (as several lines) by the F8 HUD and printed verbatim by the
/// field-mode self-test, so the two read the same numbers.
pub(in crate::windowed) fn hud_line(snap: &UiProbeState, fps: f32) -> String {
    let mode = snap.field_mode.map(|m| m.to_string()).unwrap_or_else(|| "n/a".to_string());
    let strategy = snap.strategy.map(|s| s.to_string()).unwrap_or_else(|| "n/a".to_string());
    let mut line = format!(
        "field {mode} | engine {strategy} | segments {} ({} hidden) | backdrops {} | cull {:.2} ms | fps {fps:.1}",
        snap.segments, snap.hidden_segments, snap.cull_backdrops, snap.cull_cpu_ms
    );
    match &snap.visible_stats {
        Some(s) => {
            line += &format!(
                " | items {}/{} visible, {} backdrop | lines {} candidate: {} glyph, {} wash ({} dropped) | segments {} | slots {} ({} dropped) | gpu cull {:.2} layout {:.2} draw {:.2} ms",
                s.items_visible,
                s.items_total,
                s.items_backdrop,
                s.lines_candidate,
                s.lines_glyph,
                s.lines_wash,
                s.wash_dropped,
                s.segments,
                s.slots,
                s.slots_dropped,
                s.cull_ms,
                s.layout_ms,
                s.draw_ms,
            );
        }
        None => {
            line += &format!(" | instances {} in {} draw ranges", snap.cull_instances, snap.cull_ranges);
        }
    }
    // What is selected and what the verbs address, in the field's own key
    // (M3: `item:start..end` and `file byte N` for the Visible field).
    if let Some(sel) = &snap.selection {
        line += &format!(" | selection {sel}");
    }
    if let Some(pick) = &snap.pick_key {
        line += &format!(" | pick {pick}");
    }
    line
}

pub(in crate::windowed) fn show(ctx: &egui::Context, probe_snap: Option<&UiProbeState>, fps: f32, hud_open: bool) {
    // The field HUD (F8): an always-on readout, separate from the
    // Debug window, anchored top-left — what the field is and
    // what it did this frame, in the same words the self-test
    // prints (`hud_line`).
    if hud_open {
        if let Some(snap) = probe_snap {
            egui::Area::new(egui::Id::new("field_hud"))
                .anchor(egui::Align2::LEFT_TOP, [8.0, 8.0])
                .order(egui::Order::Foreground)
                .interactable(false)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        let mode = snap.field_mode.map(|m| m.to_string()).unwrap_or_else(|| "n/a".to_string());
                        let strategy = snap.strategy.map(|s| s.to_string()).unwrap_or_else(|| "n/a".to_string());
                        ui.monospace(format!("field {mode} | engine {strategy} | {fps:.1} fps"));
                        ui.monospace(format!(
                            "segments {} ({} hidden) | backdrops {} | cull {:.2} ms CPU",
                            snap.segments, snap.hidden_segments, snap.cull_backdrops, snap.cull_cpu_ms
                        ));
                        match &snap.visible_stats {
                            Some(s) => {
                                ui.monospace(format!(
                                    "items {}/{} visible, {} backdrop",
                                    s.items_visible, s.items_total, s.items_backdrop
                                ));
                                ui.monospace(format!(
                                    "lines {} candidate: {} glyph | {} wash ({} dropped)",
                                    s.lines_candidate, s.lines_glyph, s.lines_wash, s.wash_dropped
                                ));
                                ui.monospace(format!(
                                    "segments {} | slots {} ({} dropped)",
                                    s.segments, s.slots, s.slots_dropped
                                ));
                                ui.monospace(format!(
                                    "gpu: cull {:.2} | layout {:.2} | draw {:.2} ms",
                                    s.cull_ms, s.layout_ms, s.draw_ms
                                ));
                            }
                            None => {
                                ui.monospace(format!(
                                    "instances {} in {} draw ranges",
                                    snap.cull_instances, snap.cull_ranges
                                ));
                            }
                        }
                        if snap.debug_tint != 0 {
                            ui.monospace(format!(
                                "debug tint: {}",
                                if snap.debug_tint == 1 { "by LOD tier" } else { "by cull state" }
                            ));
                        }
                        // The selection and the pick in the
                        // field's own key (M3): what a verb
                        // would address right now.
                        if snap.selection.is_some() || snap.pick_key.is_some() {
                            ui.monospace(format!(
                                "selection {} | pick {}",
                                snap.selection.as_deref().unwrap_or("none"),
                                snap.pick_key.as_deref().unwrap_or("none")
                            ));
                        }
                    });
                });
        }
    }
}
