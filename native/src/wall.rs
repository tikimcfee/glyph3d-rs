//! The terminal wall cell renderer (`out/WALL-PLAN-2026-11-26.md`, package D1).
//!
//! Draws [`term_cells::Screen`] grids on an infinite orthographic canvas using
//! wgpu and the engine's Slug glyph atlas, without putting terminals in the
//! static `GlyphField` arena and without depending on `winit`.
//!
//! Pure CPU modules (`metrics`, `cell`, `camera`, `replay`, `test_pattern`) compile
//! under `--no-default-features`; the GPU renderer (`renderer`) is gated on
//! the `wgpu` feature.

#[cfg(feature = "windowed")]
pub mod app;
pub mod camera;
pub mod cell;
#[cfg(feature = "windowed")]
pub mod keys;
pub mod metrics;
#[cfg(feature = "wgpu")]
pub mod renderer;
pub mod replay;
#[cfg(feature = "windowed")]
pub mod session;
pub mod test_pattern;

pub use camera::WallCamera;
pub use cell::{CellGpu, dirty_row_ranges, pack_cell, pack_cell_style, resolve_cell_glyph};
pub use metrics::{CellMetrics, WorldRect, snap_cell_dimension};
#[cfg(feature = "wgpu")]
pub use renderer::{
    TerminalDraw, UploadStats, WALL_DEPTH_FORMAT, WallRenderer, WallStyle, WallTarget,
    render_test_pattern_offscreen, render_wall_to_png,
};
#[cfg(feature = "wgpu")]
pub use replay::render_replay_offscreen;
