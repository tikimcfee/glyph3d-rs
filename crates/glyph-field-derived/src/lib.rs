//! The Derived glyph field — [`glyph_field::GlyphFieldMode::Derived`].
//!
//! Compact 20 B [`DerivedSlot`] per glyph, Y and Z positions derived in the vertex stage
//! from line and item tables.

pub mod derive;
pub mod field;
#[cfg(feature = "wgpu")]
pub mod field_gpu;
#[cfg(feature = "wgpu")]
pub mod pipeline;
pub mod slot;
pub mod upload;

pub use derive::{
    derive_yz, item_lane, item_of, override_lane, override_of, pack_row, resolve_group, row_of, x_page_of, ITEM_MAX,
    OVERRIDE_MAX, ROW_MAX, X_PAGE_MAX,
};
pub use field::DerivedField;
#[cfg(feature = "wgpu")]
pub use field_gpu::DerivedFieldGpu;
pub use slot::DerivedSlot;

pub const GLYPH_FIELD_DERIVED_WGSL: &str = include_str!("../shaders/glyph_field_derived.wgsl");

/// The quad's local height in `vs_main`: a Derived cell is one world unit tall
/// (`(c.y - 0.5) * 1.0`). Read by the analytic footprint ([`glyph_field::analytic_source`]).
pub const QUAD_HEIGHT: &str = "1.0";
