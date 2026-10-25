//! The windowed egui panels (2026-10-24: moved out of `state.rs` once they
//! had no behaviour left). Each `show` reads the probe snapshot and the UI
//! state and pushes command LINES (`bus::lines`); none touches the scene.
//! Floating windows, as they were: docking is a later view change, which
//! "snapshot in, lines out" leaves free to make.

pub(super) mod carrel;
pub(super) mod console;
pub(super) mod debug;
pub(super) mod files;
pub(super) mod hud;
pub(super) mod sessions;

pub(super) use super::ui::UiState;

/// A `[ui]` colour setting (sRGB 0-255) as an egui colour.
fn ui_rgb([r, g, b]: [u8; 3]) -> egui::Color32 {
    egui::Color32::from_rgb(r, g, b)
}
