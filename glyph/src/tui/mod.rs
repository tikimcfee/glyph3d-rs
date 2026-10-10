//! Interactive launcher TUI for glyph3d-native.
//!
//! Provides a mission-control terminal interface for configuring launch
//! flags, inspecting hardware/GPU profile and build currency, and launching
//! the 3D windowed renderer with a clean terminal handoff.

mod draw;
mod state;
mod terminal;

pub use terminal::run;
