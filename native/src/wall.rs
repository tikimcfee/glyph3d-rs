//! The terminal wall cell renderer (`out/WALL-PLAN-2026-11-26.md`, package D1).
//!
//! Draws [`term_cells::Screen`] grids on an infinite orthographic canvas using
//! wgpu and the engine's Slug glyph atlas, without putting terminals in the
//! static `GlyphField` arena and without depending on `winit`.
//!
//! Pure CPU modules (`metrics`, `cell`, `camera`, `replay`, `test_pattern`) compile
//! under `--no-default-features`; the GPU renderer (`renderer`) is gated on
//! the `wgpu` feature.

pub mod agents;
#[cfg(feature = "windowed")]
pub mod app;
pub mod camera;
pub mod cell;
pub mod chrome;
pub mod describe;
pub mod glyphs;
#[cfg(feature = "windowed")]
pub mod keys;
pub mod labels;
pub mod metrics;
#[cfg(feature = "egui-ui")]
pub mod mount;
#[cfg(feature = "egui-ui")]
pub mod panels;
pub mod paper;
#[cfg(feature = "windowed")]
pub mod perf;
#[cfg(feature = "wgpu")]
pub mod renderer;
pub mod replay;
#[cfg(feature = "windowed")]
pub mod script;
#[cfg(feature = "windowed")]
pub mod session;
pub mod shapes;
pub mod test_pattern;
#[cfg(feature = "egui-ui")]
pub mod ui;

pub use camera::WallCamera;
pub use cell::{
    CellGpu, CellLinkHover, CellSelection, apply_hover_link_to_cell, apply_selection_to_cell,
    dirty_row_ranges, dirty_row_ranges_with_overlays, dirty_row_ranges_with_selection, pack_cell,
    pack_cell_resolved, pack_cell_style, resolve_cell_glyph, resolve_str_glyphs,
    snap_row_span_to_wide,
};
pub use glyphs::{EmojiSheetArchive, SparseEmojiSource};
#[cfg(feature = "wgpu")]
pub use glyphs::{GlyphSource, GlyphSyncStats, SparseEmojiLayout};
pub use labels::{Label, LabelGlyphGpu, LabelHeight, measure_label_width, pack_label_glyphs};
pub use metrics::{CellMetrics, WorldRect, snap_cell_dimension};
pub use paper::PaperStyle;
#[cfg(feature = "wgpu")]
pub use renderer::{
    TerminalDraw, UploadStats, WALL_DEPTH_FORMAT, WallFrame, WallRenderer, WallStyle, WallTarget,
    render_chrome_pattern_offscreen, render_test_pattern_offscreen, render_wall_to_png,
    render_wall_to_png_with_overlay,
};
#[cfg(feature = "wgpu")]
pub use replay::render_replay_offscreen;
pub use shapes::{DashStyle, DotShape, LeaderShape, LineShape, PolygonShape, RectShape, Shape};
#[cfg(feature = "egui-ui")]
pub use ui::{
    DebugHudInput, HudTerminalInfo, PaintedUi, UiEventOutcome, UiFrame, WallUi,
    configure_wall_context, show_debug_hud,
};

/// Convert a 24-bit `0xRRGGBB` sRGB hex colour and `alpha` into normalized sRGB RGBA (`0.0..=1.0`).
pub const fn srgb_hex(hex: u32, alpha: f32) -> [f32; 4] {
    [
        ((hex >> 16) & 0xFF) as f32 / 255.0,
        ((hex >> 8) & 0xFF) as f32 / 255.0,
        (hex & 0xFF) as f32 / 255.0,
        alpha,
    ]
}
