//! The terminal wall cell renderer (`out/WALL-PLAN-2026-11-26.md`, package D1).
//!
//! Draws [`term_cells::Screen`] grids on an infinite orthographic canvas using
//! wgpu and the engine's Slug glyph atlas, without putting terminals in the
//! static `GlyphField` arena and without depending on `winit`.
//!
//! Pure CPU modules (`metrics`, `cell`, `camera`, `test_pattern`) compile
//! under `--no-default-features`; the GPU renderer (`renderer`) is gated on
//! the `wgpu` feature.

#[cfg(feature = "windowed")]
pub mod app;
pub mod camera;
pub mod cell;
#[cfg(feature = "windowed")]
pub mod keys;
pub mod labels;
pub mod metrics;
pub mod paper;
#[cfg(feature = "wgpu")]
pub mod renderer;
#[cfg(feature = "windowed")]
pub mod session;
pub mod shapes;
pub mod test_pattern;
#[cfg(feature = "egui-ui")]
pub mod ui;

pub use camera::WallCamera;
pub use cell::{CellGpu, dirty_row_ranges, pack_cell, pack_cell_style, resolve_cell_glyph, resolve_str_glyphs};
pub use labels::{Label, LabelGlyphGpu, LabelHeight, measure_label_width, pack_label_glyphs};
pub use metrics::{CellMetrics, WorldRect, snap_cell_dimension};
pub use paper::PaperStyle;
#[cfg(feature = "wgpu")]
pub use renderer::{
    TerminalDraw, UploadStats, WALL_DEPTH_FORMAT, WallFrame, WallRenderer, WallStyle, WallTarget,
    render_test_pattern_offscreen, render_wall_to_png, render_wall_to_png_with_overlay,
};
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
